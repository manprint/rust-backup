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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{BackupError, Phase, Result};
use crate::progress::Progress;
use crate::wire::{self, ControlFrame, DataFrame};

/// Bounds for peer-supplied plan and stream bookkeeping. They prevent a valid
/// frame from still forcing unbounded allocation or millions of hashers.
pub const MAX_PLAN_ITEMS: usize = 100_000;
pub const MAX_PLAN_ITEM_NAME_BYTES: usize = 4 * 1024;
pub const MAX_PLAN_ITEM_META_BYTES: usize = 64 * 1024;
pub const MAX_IN_FLIGHT_ITEM_HASHERS: usize = 1024;

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
pub struct StreamChunkSink<S: AsyncWrite + Unpin + Send> {
    stream: S,
    progress: Option<Progress>,
}

impl<S: AsyncWrite + Unpin + Send> StreamChunkSink<S> {
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
impl<S: AsyncWrite + Unpin + Send> ChunkSink for StreamChunkSink<S> {
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
    item_hashers: HashMap<u32, blake3::Hasher>,
    /// Bytes received for each in-flight item, checked against ItemEnd.total.
    item_totals: HashMap<u32, u64>,
    /// Finished data-bearing item ids and their verified whole-item digests.
    completed_items: BTreeMap<u32, String>,
    /// Shared only by a multi-carrier session; bounds all carrier hashers, not
    /// N independent per-stream limits.
    session_hashers: Option<Arc<AtomicUsize>>,
}

impl<S: DuplexStream> StreamChunkSource<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            progress: None,
            item_hashers: HashMap::new(),
            item_totals: HashMap::new(),
            completed_items: BTreeMap::new(),
            session_hashers: None,
        }
    }
    /// As [`Self::new`] but increments `progress` as chunks/items arrive.
    pub fn new_counted(stream: S, progress: Progress) -> Self {
        Self {
            stream,
            progress: Some(progress),
            item_hashers: HashMap::new(),
            item_totals: HashMap::new(),
            completed_items: BTreeMap::new(),
            session_hashers: None,
        }
    }
    /// Recover the underlying stream (e.g. to read a trailing control frame).
    pub fn into_inner(self) -> S {
        self.stream
    }

    fn with_session_hasher_limit(mut self, session_hashers: Arc<AtomicUsize>) -> Self {
        self.session_hashers = Some(session_hashers);
        self
    }

    /// Verified item ids received before the stream ended.
    pub fn completed_item_ids(&self) -> BTreeSet<u32> {
        self.completed_items.keys().copied().collect()
    }

    /// Verified source digest for every completed item.
    pub fn completed_item_digests(&self) -> BTreeMap<u32, String> {
        self.completed_items.clone()
    }

    /// Order-independent whole-payload commitment, derived from verified item
    /// digests. Stable ordering by item id makes the trailing `Done` frame
    /// meaningful without buffering payload bytes.
    pub fn completion_digest(&self) -> String {
        completion_digest(&self.completed_items)
    }
}

/// Produce the session-level payload commitment from completed item digests.
pub fn completion_digest(completed: &BTreeMap<u32, String>) -> String {
    let mut hasher = blake3::Hasher::new();
    for (id, digest) in completed {
        hasher.update(&id.to_le_bytes());
        hasher.update(digest.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

#[async_trait]
impl<S: DuplexStream> ChunkSource for StreamChunkSource<S> {
    async fn next(&mut self) -> Result<ChunkEvent> {
        let frame: Option<DataFrame> = wire::recv_frame(&mut self.stream).await?;
        match frame {
            None => Err(BackupError::phase(
                Phase::Transfer,
                "data carrier closed before StreamEnd",
            )),
            Some(DataFrame::StreamEnd) => {
                if !self.item_hashers.is_empty() {
                    let mut incomplete: Vec<_> = self.item_hashers.keys().copied().collect();
                    incomplete.sort_unstable();
                    return Err(BackupError::phase(
                        Phase::Transfer,
                        format!("StreamEnd with incomplete items={incomplete:?}"),
                    ));
                }
                Ok(ChunkEvent::End)
            }
            Some(DataFrame::CarrierHello { carrier }) => Err(BackupError::phase(
                Phase::Transfer,
                format!("unexpected CarrierHello carrier={carrier} after data stream setup"),
            )),
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
                let expected_offset = self.item_totals.get(&item_id).copied().unwrap_or_default();
                if offset != expected_offset {
                    return Err(BackupError::phase(
                        Phase::Transfer,
                        format!(
                            "non-contiguous chunk for item={item_id}: offset={offset} expected={expected_offset}"
                        ),
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
                if !self.item_hashers.contains_key(&item_id) {
                    let too_many = if let Some(session_hashers) = &self.session_hashers {
                        session_hashers
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                                (count < MAX_IN_FLIGHT_ITEM_HASHERS).then_some(count + 1)
                            })
                            .is_err()
                    } else {
                        self.item_hashers.len() >= MAX_IN_FLIGHT_ITEM_HASHERS
                    };
                    if too_many {
                        return Err(BackupError::phase(
                            Phase::Transfer,
                            format!(
                                "too many in-flight items (limit {MAX_IN_FLIGHT_ITEM_HASHERS})"
                            ),
                        ));
                    }
                }
                self.item_hashers.entry(item_id).or_default().update(&data);
                *self.item_totals.entry(item_id).or_default() += data.len() as u64;
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
                let had_hasher = self.item_hashers.contains_key(&item_id);
                let hasher = self.item_hashers.remove(&item_id).unwrap_or_default();
                if had_hasher {
                    if let Some(session_hashers) = &self.session_hashers {
                        session_hashers.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let observed_total = self.item_totals.remove(&item_id).unwrap_or_default();
                if observed_total != total {
                    return Err(BackupError::phase(
                        Phase::Verify,
                        format!(
                            "item={item_id}: ItemEnd.total={total} but delivered={observed_total}"
                        ),
                    ));
                }
                let got = hasher.finalize().to_hex().to_string();
                if got != blake3 {
                    return Err(BackupError::Integrity(format!(
                        "item={item_id}: whole-item blake3 mismatch (declared {blake3}, computed {got})"
                    )));
                }
                if self.completed_items.insert(item_id, got).is_some() {
                    return Err(BackupError::phase(
                        Phase::Verify,
                        format!("item={item_id}: duplicate ItemEnd"),
                    ));
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

/// A set of item-pinned data carriers.  An item is always assigned to exactly
/// one carrier (`item_id % carriers`), so chunks within an item cannot reorder.
/// This deliberately does not stripe a single item across streams.
pub struct MultiStreamChunkSink {
    streams: Vec<StreamChunkSink<Box<dyn DuplexStream>>>,
}

impl MultiStreamChunkSink {
    pub fn new_counted(streams: Vec<Box<dyn DuplexStream>>, progress: Progress) -> Result<Self> {
        if streams.is_empty() {
            return Err(BackupError::phase(
                Phase::Connect,
                "no data carriers negotiated",
            ));
        }
        Ok(Self {
            streams: streams
                .into_iter()
                .map(|stream| StreamChunkSink::new_counted(stream, progress.clone()))
                .collect(),
        })
    }

    fn carrier(&self, item_id: u32) -> usize {
        (item_id as usize) % self.streams.len()
    }

    /// Best-effort abort delivered to every carrier after a source failure.
    pub async fn abort(&mut self, reason: &str) {
        for stream in &mut self.streams {
            let _ = wire::send_frame(
                &mut stream.stream,
                &DataFrame::Abort {
                    reason: reason.to_string(),
                },
            )
            .await;
        }
    }
}

#[async_trait]
impl ChunkSink for MultiStreamChunkSink {
    async fn send_chunk(&mut self, item_id: u32, offset: u64, data: &[u8]) -> Result<()> {
        let carrier = self.carrier(item_id);
        self.streams[carrier]
            .send_chunk(item_id, offset, data)
            .await
    }

    async fn finish_item(&mut self, item_id: u32, total: u64, blake3: &str) -> Result<()> {
        let carrier = self.carrier(item_id);
        self.streams[carrier]
            .finish_item(item_id, total, blake3)
            .await
    }

    async fn finish(&mut self) -> Result<()> {
        for stream in &mut self.streams {
            stream.finish().await?;
        }
        Ok(())
    }
}

/// Plan-ordered view over item-pinned carriers.
///
/// A module has always consumed one contiguous, plan-ordered event stream.  Do
/// not turn the carriers into an mpsc merge: that lets item N+1 reach a module
/// while it still owns the sink for item N.  Instead, pull only the stream that
/// owns the next planned item.  The other streams remain flow-controlled by the
/// transport until their turn, so this also preserves consumer-paced backpressure.
pub struct MultiStreamChunkSource {
    streams: Vec<StreamChunkSource<Box<dyn DuplexStream>>>,
    expected_item_ids: Vec<u32>,
    item_cursor: usize,
    end_cursor: usize,
    completed_items: BTreeMap<u32, String>,
}

impl MultiStreamChunkSource {
    pub fn new_ordered(
        streams: Vec<Box<dyn DuplexStream>>,
        expected_item_ids: Vec<u32>,
        progress: Progress,
    ) -> Result<Self> {
        if streams.is_empty() {
            return Err(BackupError::phase(
                Phase::Connect,
                "no data carriers negotiated",
            ));
        }
        let session_hashers = Arc::new(AtomicUsize::new(0));
        Ok(Self {
            streams: streams
                .into_iter()
                .map(|stream| {
                    StreamChunkSource::new_counted(stream, progress.clone())
                        .with_session_hasher_limit(session_hashers.clone())
                })
                .collect(),
            expected_item_ids,
            item_cursor: 0,
            end_cursor: 0,
            completed_items: BTreeMap::new(),
        })
    }

    pub fn completed_item_ids(&self) -> BTreeSet<u32> {
        self.completed_items.keys().copied().collect()
    }

    /// Verified source digest for every completed item.
    pub fn completed_item_digests(&self) -> BTreeMap<u32, String> {
        self.completed_items.clone()
    }

    pub fn completion_digest(&self) -> String {
        completion_digest(&self.completed_items)
    }
}

#[async_trait]
impl ChunkSource for MultiStreamChunkSource {
    async fn next(&mut self) -> Result<ChunkEvent> {
        if let Some(&expected_item_id) = self.expected_item_ids.get(self.item_cursor) {
            let carrier = expected_item_id as usize % self.streams.len();
            let event = self.streams[carrier].next().await?;
            match event {
                ChunkEvent::End => Err(BackupError::phase(
                    Phase::Transfer,
                    format!(
                        "carrier={carrier} ended before planned item={expected_item_id} completed"
                    ),
                )),
                ChunkEvent::Chunk {
                    item_id,
                    offset,
                    data,
                } => {
                    if item_id != expected_item_id {
                        return Err(BackupError::phase(
                            Phase::Transfer,
                            format!(
                                "item={item_id} arrived on carrier={carrier}; expected item={expected_item_id}"
                            ),
                        ));
                    }
                    Ok(ChunkEvent::Chunk {
                        item_id,
                        offset,
                        data,
                    })
                }
                ChunkEvent::ItemEnd {
                    item_id,
                    total,
                    blake3,
                } => {
                    if item_id != expected_item_id {
                        return Err(BackupError::phase(
                            Phase::Transfer,
                            format!(
                                "ItemEnd item={item_id} arrived on carrier={carrier}; expected item={expected_item_id}"
                            ),
                        ));
                    }
                    if self
                        .completed_items
                        .insert(item_id, blake3.clone())
                        .is_some()
                    {
                        return Err(BackupError::phase(
                            Phase::Verify,
                            format!("item={item_id}: duplicate ItemEnd across carriers"),
                        ));
                    }
                    self.item_cursor += 1;
                    Ok(ChunkEvent::ItemEnd {
                        item_id,
                        total,
                        blake3,
                    })
                }
            }
        } else {
            while self.end_cursor < self.streams.len() {
                let carrier = self.end_cursor;
                match self.streams[carrier].next().await? {
                    ChunkEvent::End => self.end_cursor += 1,
                    ChunkEvent::Chunk { item_id, .. } | ChunkEvent::ItemEnd { item_id, .. } => {
                        return Err(BackupError::phase(
                            Phase::Transfer,
                            format!("carrier={carrier} emitted unexpected item={item_id} after plan end"),
                        ));
                    }
                }
            }
            Ok(ChunkEvent::End)
        }
    }
}

// --- Plan-exchange handshake (carried on the control substream) -------------

/// Source → Destination: send the plan.
pub async fn send_plan<S: DuplexStream>(
    ctrl: &mut S,
    plan: &crate::plan::BackupPlan,
    carriers: usize,
) -> Result<()> {
    wire::send_frame(
        ctrl,
        &ControlFrame::Plan {
            plan: Box::new(plan.clone()),
            carriers: carriers.clamp(1, u32::MAX as usize) as u32,
            separate_data_streams: true,
        },
    )
    .await
}

/// Destination: receive the plan.
///
/// A plan whose `format_version` this build does not understand is rejected here
/// (the plan is self-contained, so a version we cannot interpret must never be
/// half-applied).
pub async fn recv_plan<S: DuplexStream>(
    ctrl: &mut S,
) -> Result<(crate::plan::BackupPlan, u32, bool)> {
    match wire::recv_frame::<_, ControlFrame>(ctrl).await? {
        Some(ControlFrame::Plan {
            plan: p,
            carriers,
            separate_data_streams,
        }) => {
            if p.format_version != crate::plan::PLAN_FORMAT_VERSION {
                return Err(BackupError::PlanRejected(format!(
                    "unsupported plan format_version {} (this build understands {})",
                    p.format_version,
                    crate::plan::PLAN_FORMAT_VERSION
                )));
            }
            validate_plan_bounds(&p)?;
            Ok((*p, carriers.max(1), separate_data_streams))
        }
        Some(ControlFrame::Abort { reason }) => Err(BackupError::PlanRejected(reason)),
        other => Err(BackupError::phase(
            Phase::Connect,
            format!("expected Plan frame, got {other:?}"),
        )),
    }
}

/// Validate allocations induced by an otherwise syntactically valid plan.
pub fn validate_plan_bounds(plan: &crate::plan::BackupPlan) -> Result<()> {
    if plan.items.len() > MAX_PLAN_ITEMS {
        return Err(BackupError::phase(
            Phase::Connect,
            format!(
                "plan has {} items; limit is {MAX_PLAN_ITEMS}",
                plan.items.len()
            ),
        ));
    }
    for item in &plan.items {
        if item.name.len() > MAX_PLAN_ITEM_NAME_BYTES || item.kind.len() > MAX_PLAN_ITEM_NAME_BYTES
        {
            return Err(BackupError::phase(
                Phase::Connect,
                format!(
                    "plan item {} name/kind exceeds {MAX_PLAN_ITEM_NAME_BYTES} bytes",
                    item.id
                ),
            ));
        }
        let meta_size = serde_json::to_vec(&item.meta)
            .map_err(|error| {
                BackupError::phase_src(Phase::Connect, "plan item meta serialize", error)
            })?
            .len();
        if meta_size > MAX_PLAN_ITEM_META_BYTES {
            return Err(BackupError::phase(
                Phase::Connect,
                format!(
                    "plan item {} meta exceeds {MAX_PLAN_ITEM_META_BYTES} bytes",
                    item.id
                ),
            ));
        }
    }
    Ok(())
}

/// Destination → Source: send the accept/reject decision.
pub async fn send_ack<S: DuplexStream>(
    ctrl: &mut S,
    accepted: bool,
    reason: &str,
    carriers: usize,
    separate_data_streams: bool,
) -> Result<()> {
    wire::send_frame(
        ctrl,
        &ControlFrame::PlanAck {
            accepted,
            reason: reason.to_string(),
            carriers: carriers.clamp(1, u32::MAX as usize) as u32,
            separate_data_streams,
        },
    )
    .await
}

/// Source: await the destination's decision.
pub async fn recv_ack<S: DuplexStream>(ctrl: &mut S) -> Result<(u32, bool)> {
    match wire::recv_frame::<_, ControlFrame>(ctrl).await? {
        Some(ControlFrame::PlanAck {
            accepted: true,
            carriers,
            separate_data_streams,
            ..
        }) => Ok((carriers.max(1), separate_data_streams)),
        Some(ControlFrame::PlanAck { reason, .. }) => Err(BackupError::PlanRejected(reason)),
        other => Err(BackupError::phase(
            Phase::Connect,
            format!("expected PlanAck, got {other:?}"),
        )),
    }
}
