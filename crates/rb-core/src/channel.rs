//! The abstraction modules program against: a paired bidirectional byte channel
//! between source and destination, plus chunk-level sink/source framing on top.
//!
//! LAYERING: `rb-transport` provides the concrete [`DataChannel`] (coordination
//! server, relay + direct QUIC, carriers). Modules never see transport details —
//! they get [`ChunkSink`] / [`ChunkSource`]. This keeps the byte plane swappable
//! and lets modules be unit-tested against in-memory channels.
//!
//! BACKPRESSURE INVARIANT: chunk writes go straight onto a substream whose flow
//! control is consumer-paced (yamux window / QUIC stream window). A slow
//! destination stalls the source's writes, which stalls its backend reads — so
//! the producer never outruns the consumer and the source is never forced to
//! buffer locally. This is how the producer/consumer bandwidth gap is balanced.

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{BackupError, Phase, Result};
use crate::progress::Progress;
use crate::wire::{self, ControlFrame, DataFrame};

/// Any bidirectional byte stream usable as a substream.
pub trait DuplexStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> DuplexStream for T {}

/// A paired byte channel between source and destination, provided by the
/// transport layer. The destination ("consumer") drives substream opening; the
/// source ("provider") accepts. The first substream is the control channel.
#[async_trait]
pub trait DataChannel: Send + Sync {
    /// Open the next outbound substream (destination/consumer side).
    async fn open_stream(&self) -> Result<Box<dyn DuplexStream>>;
    /// Accept the next inbound substream (source/provider side).
    async fn accept_stream(&self) -> Result<Box<dyn DuplexStream>>;
    /// Number of parallel data carriers negotiated for this channel.
    fn carriers(&self) -> usize;
}

/// One event drawn from a [`ChunkSource`].
#[derive(Debug)]
pub enum ChunkEvent {
    /// Payload bytes for `item_id` at `offset`.
    Chunk {
        item_id: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// `item_id` fully delivered; verify against `blake3`.
    ItemEnd {
        item_id: u32,
        total: u64,
        blake3: String,
    },
    /// No further data on this stream.
    End,
}

/// Producer side: modules push backend bytes into the channel chunk by chunk.
#[async_trait]
pub trait ChunkSink: Send {
    /// Send one chunk of `item_id`. `data.len()` must be ≤ [`wire::CHUNK_SIZE`].
    async fn send_chunk(&mut self, item_id: u32, offset: u64, data: &[u8]) -> Result<()>;
    /// Mark `item_id` complete with its whole-item digest.
    async fn finish_item(&mut self, item_id: u32, total: u64, blake3: &str) -> Result<()>;
    /// Close the stream (no more items).
    async fn finish(&mut self) -> Result<()>;
}

/// Consumer side: modules pull backend bytes out of the channel.
#[async_trait]
pub trait ChunkSource: Send {
    /// Next event; [`ChunkEvent::End`] terminates.
    async fn next(&mut self) -> Result<ChunkEvent>;
}

/// A [`ChunkSink`] backed by a single transport substream.
pub struct StreamChunkSink<S: DuplexStream> {
    stream: S,
    progress: Option<Progress>,
}

impl<S: DuplexStream> StreamChunkSink<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            progress: None,
        }
    }
    /// As [`Self::new`] but increments `progress` as chunks/items flow.
    pub fn new_counted(stream: S, progress: Progress) -> Self {
        Self {
            stream,
            progress: Some(progress),
        }
    }
    /// Recover the underlying stream (e.g. to send a trailing control frame).
    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[async_trait]
impl<S: DuplexStream> ChunkSink for StreamChunkSink<S> {
    async fn send_chunk(&mut self, item_id: u32, offset: u64, data: &[u8]) -> Result<()> {
        if data.len() > wire::CHUNK_SIZE {
            return Err(BackupError::phase(
                Phase::Transfer,
                "chunk exceeds CHUNK_SIZE",
            ));
        }
        let header = DataFrame::ChunkStart {
            item_id,
            offset,
            len: data.len() as u32,
            blake3: wire::blake3_hex(data),
        };
        wire::send_frame(&mut self.stream, &header).await?;
        // Raw payload follows the header (not JSON-wrapped) — backpressure here.
        wire::write_all_idle(&mut self.stream, data).await?;
        if let Some(p) = &self.progress {
            p.add_bytes(data.len() as u64);
        }
        Ok(())
    }

    async fn finish_item(&mut self, item_id: u32, total: u64, blake3: &str) -> Result<()> {
        let f = DataFrame::ItemEnd {
            item_id,
            total,
            blake3: blake3.to_string(),
        };
        wire::send_frame(&mut self.stream, &f).await?;
        if let Some(p) = &self.progress {
            p.item_done();
        }
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        wire::send_frame(&mut self.stream, &DataFrame::StreamEnd).await
    }
}

/// A [`ChunkSource`] backed by a single transport substream.
///
/// Verifies integrity at two levels without ever buffering an item: every chunk
/// is checked against its own `ChunkStart.blake3`, and a running per-item hasher
/// is folded chunk by chunk and compared against `ItemEnd.blake3`. A module
/// therefore never applies bytes that failed either check.
pub struct StreamChunkSource<S: DuplexStream> {
    stream: S,
    progress: Option<Progress>,
    /// Running whole-item hashers, keyed by item id. An entry lives only while
    /// the item is in flight (removed at `ItemEnd`), so memory is O(items in
    /// flight), never O(bytes).
    item_hashers: std::collections::HashMap<u32, blake3::Hasher>,
}

impl<S: DuplexStream> StreamChunkSource<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            progress: None,
            item_hashers: std::collections::HashMap::new(),
        }
    }
    /// As [`Self::new`] but increments `progress` as chunks/items arrive.
    pub fn new_counted(stream: S, progress: Progress) -> Self {
        Self {
            stream,
            progress: Some(progress),
            item_hashers: std::collections::HashMap::new(),
        }
    }
    /// Recover the underlying stream (e.g. to read a trailing control frame).
    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[async_trait]
impl<S: DuplexStream> ChunkSource for StreamChunkSource<S> {
    async fn next(&mut self) -> Result<ChunkEvent> {
        let frame: Option<DataFrame> = wire::recv_frame(&mut self.stream).await?;
        match frame {
            None | Some(DataFrame::StreamEnd) => Ok(ChunkEvent::End),
            Some(DataFrame::Abort { reason }) => Err(BackupError::phase(
                Phase::Transfer,
                format!("source aborted mid-stream: {reason}"),
            )),
            Some(DataFrame::ChunkStart {
                item_id,
                offset,
                len,
                blake3,
            }) => {
                // A peer-declared length is never trusted for allocation: the
                // sender is bound by CHUNK_SIZE, so anything larger is a protocol
                // violation, not a 4 GiB buffer to allocate.
                if len as usize > wire::CHUNK_SIZE {
                    return Err(BackupError::phase(
                        Phase::Transfer,
                        format!("incoming chunk len {len} exceeds CHUNK_SIZE"),
                    ));
                }
                let mut data = vec![0u8; len as usize];
                wire::read_exact_idle(&mut self.stream, &mut data).await?;
                let got = wire::blake3_hex(&data);
                if got != blake3 {
                    return Err(BackupError::Integrity(format!(
                        "chunk item={item_id} offset={offset}: blake3 mismatch"
                    )));
                }
                self.item_hashers.entry(item_id).or_default().update(&data);
                if let Some(p) = &self.progress {
                    p.add_bytes(data.len() as u64);
                }
                Ok(ChunkEvent::Chunk {
                    item_id,
                    offset,
                    data,
                })
            }
            Some(DataFrame::ItemEnd {
                item_id,
                total,
                blake3,
            }) => {
                // Whole-item integrity: fold of every chunk received for this id.
                // An item with no chunks hashes the empty input, which is exactly
                // what the sender's whole-item digest is for a zero-byte item.
                let hasher = self.item_hashers.remove(&item_id).unwrap_or_default();
                let got = hasher.finalize().to_hex().to_string();
                if got != blake3 {
                    return Err(BackupError::Integrity(format!(
                        "item={item_id}: whole-item blake3 mismatch (declared {blake3}, computed {got})"
                    )));
                }
                if let Some(p) = &self.progress {
                    p.item_done();
                }
                Ok(ChunkEvent::ItemEnd {
                    item_id,
                    total,
                    blake3,
                })
            }
        }
    }
}

// --- Plan-exchange handshake (carried on the control substream) -------------

/// Source → Destination: send the plan.
pub async fn send_plan<S: DuplexStream>(
    ctrl: &mut S,
    plan: &crate::plan::BackupPlan,
) -> Result<()> {
    wire::send_frame(ctrl, &ControlFrame::Plan(Box::new(plan.clone()))).await
}

/// Destination: receive the plan.
///
/// A plan whose `format_version` this build does not understand is rejected here
/// (the plan is self-contained, so a version we cannot interpret must never be
/// half-applied).
pub async fn recv_plan<S: DuplexStream>(ctrl: &mut S) -> Result<crate::plan::BackupPlan> {
    match wire::recv_frame::<_, ControlFrame>(ctrl).await? {
        Some(ControlFrame::Plan(p)) => {
            if p.format_version != crate::plan::PLAN_FORMAT_VERSION {
                return Err(BackupError::PlanRejected(format!(
                    "unsupported plan format_version {} (this build understands {})",
                    p.format_version,
                    crate::plan::PLAN_FORMAT_VERSION
                )));
            }
            Ok(*p)
        }
        Some(ControlFrame::Abort { reason }) => Err(BackupError::PlanRejected(reason)),
        other => Err(BackupError::phase(
            Phase::Connect,
            format!("expected Plan frame, got {other:?}"),
        )),
    }
}

/// Destination → Source: send the accept/reject decision.
pub async fn send_ack<S: DuplexStream>(ctrl: &mut S, accepted: bool, reason: &str) -> Result<()> {
    wire::send_frame(
        ctrl,
        &ControlFrame::PlanAck {
            accepted,
            reason: reason.to_string(),
        },
    )
    .await
}

/// Source: await the destination's decision.
pub async fn recv_ack<S: DuplexStream>(ctrl: &mut S) -> Result<()> {
    match wire::recv_frame::<_, ControlFrame>(ctrl).await? {
        Some(ControlFrame::PlanAck { accepted: true, .. }) => Ok(()),
        Some(ControlFrame::PlanAck { reason, .. }) => Err(BackupError::PlanRejected(reason)),
        other => Err(BackupError::phase(
            Phase::Connect,
            format!("expected PlanAck, got {other:?}"),
        )),
    }
}
