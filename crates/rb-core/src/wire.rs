//! On-the-wire framing for the data channel.
//!
//! Two frame families, both length-prefixed JSON (`u32` LE length + body),
//! mirroring `bore`'s `transfer.rs` codec so the vendored transport interops:
//!
//! * [`ControlFrame`] — plan exchange + async-accept handshake + completion.
//! * [`DataFrame`] — per-chunk headers; the raw chunk payload follows the
//!   `ChunkStart` frame immediately (NOT wrapped in JSON), so large payloads are
//!   never base64/JSON-bloated.
//!
//! INVARIANT (streaming, no temp files): chunks flow straight from the source
//! backend into the channel and from the channel into the destination backend.
//! Nothing here buffers a whole item to disk.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{BackupError, Phase, Result};
use crate::plan::BackupPlan;
use crate::verification::VerificationReport;

/// Max bytes per data chunk on the wire.
pub const CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB
/// Read buffer for streaming sources without a natural chunk boundary.
pub const COPY_BUFFER: usize = 64 * 1024; // 64 KiB
/// Max JSON control/data frame body size.
pub const FRAME_LIMIT: usize = 16 * 1024 * 1024; // 16 MiB
/// Idle timeout for a single read/write syscall (re-armed on progress).
pub const IO_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Control-plane frames carried on the first ("control") substream.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ControlFrame {
    /// Source → Destination: the full backup plan.
    Plan {
        plan: Box<BackupPlan>,
        /// Number of data carriers requested by the provider.  Zero is never
        /// emitted; it is accepted defensively as one for malformed old peers.
        #[serde(default = "default_carriers")]
        carriers: u32,
        /// New peers keep payload off this control stream even for one carrier,
        /// allowing abort/completion reads to run concurrently without splitting
        /// a yamux data stream. False preserves the legacy wire layout.
        #[serde(default)]
        separate_data_streams: bool,
    },
    /// Destination → Source: accept/reject decision (async-accept mode).
    PlanAck {
        accepted: bool,
        reason: String,
        /// Number of data carriers the consumer will actually open.  Defaults
        /// to one so a newer provider safely interoperates with an old peer.
        #[serde(default = "default_carriers")]
        carriers: u32,
        /// Echoes the source capability above. Missing on legacy peers.
        #[serde(default)]
        separate_data_streams: bool,
    },
    /// Source → Destination: all items streamed; final whole-payload digest.
    Done { total_bytes: u64, blake3: String },
    /// Legacy destination acknowledgement retained only so current peers can
    /// decode it and fail closed with a precise missing-evidence error.
    CompleteAck,
    /// Destination → Source: persisted backend state was read back and matched
    /// every source item. This evidence-bearing acknowledgement is mandatory
    /// for a successful current-protocol run.
    VerificationAck { report: VerificationReport },
    /// Source → Destination: confirms `VerificationAck` and the source audit;
    /// the destination answers with `VerificationComplete`.
    CompleteAckAck,
    /// Destination → Source: both persisted read-back and source immutability
    /// proofs are known to the destination. The source may now close cleanly.
    VerificationComplete,
    /// Either side: abort with reason (clean teardown, no partial apply).
    Abort { reason: String },
    /// Receiver → aborting peer: confirms the structured abort reason was
    /// received, allowing a multiplexed transport to close without dropping it.
    AbortAck,
}

const fn default_carriers() -> u32 {
    1
}

/// Data-plane frames carried on each data substream. A `ChunkStart` is followed
/// immediately by exactly `len` raw payload bytes.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum DataFrame {
    /// First frame on every negotiated separate data substream. Relay stream
    /// accept order is not a carrier identity, so the receiver must bind it
    /// explicitly before applying item-pinning.
    CarrierHello { carrier: u16 },
    /// Header for one chunk of `item_id` at byte `offset`; `len` raw bytes follow.
    ChunkStart {
        item_id: u32,
        offset: u64,
        len: u32,
        blake3: String,
    },
    /// All chunks for `item_id` sent; `total` bytes, whole-item digest.
    ItemEnd {
        item_id: u32,
        total: u64,
        blake3: String,
    },
    /// No more items on this substream.
    StreamEnd,
    /// The producer failed mid-stream and will send nothing further. Carried in
    /// the DATA family (not [`ControlFrame::Abort`]) because in the single-substream
    /// session the peer is parsing `DataFrame`s at this point — a control-family
    /// frame would surface as an opaque deserialize error instead of a reason.
    Abort { reason: String },
}

/// Write a length-prefixed JSON frame.
pub async fn send_frame<S, T>(stream: &mut S, frame: &T) -> Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(frame)
        .map_err(|e| BackupError::phase_src(Phase::Transfer, "frame serialize", e))?;
    if body.len() > FRAME_LIMIT {
        return Err(BackupError::phase(
            Phase::Transfer,
            "frame exceeds FRAME_LIMIT",
        ));
    }
    let len = (body.len() as u32).to_le_bytes();
    write_all_idle(stream, &len).await?;
    write_all_idle(stream, &body).await?;
    stream
        .flush()
        .await
        .map_err(|e| BackupError::phase_src(Phase::Transfer, "frame flush", e))?;
    Ok(())
}

/// Read a length-prefixed JSON frame. Returns `None` on clean EOF.
pub async fn recv_frame<S, T>(stream: &mut S) -> Result<Option<T>>
where
    S: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(BackupError::phase_src(Phase::Transfer, "frame len read", e)),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > FRAME_LIMIT {
        return Err(BackupError::phase(
            Phase::Transfer,
            "incoming frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    read_exact_idle(stream, &mut body).await?;
    let frame = serde_json::from_slice(&body)
        .map_err(|e| BackupError::phase_src(Phase::Transfer, "frame deserialize", e))?;
    Ok(Some(frame))
}

/// `write_all` with an idle timeout re-armed on every successful write — a stuck
/// peer fails fast instead of hanging forever. (Ported from `bore::transfer`.)
pub async fn write_all_idle<S>(stream: &mut S, mut buf: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    while !buf.is_empty() {
        let n = tokio::time::timeout(IO_IDLE_TIMEOUT, stream.write(buf))
            .await
            .map_err(|_| BackupError::phase(Phase::Transfer, "write idle timeout"))?
            .map_err(|e| BackupError::phase_src(Phase::Transfer, "write", e))?;
        if n == 0 {
            return Err(BackupError::phase(
                Phase::Transfer,
                "write returned 0 (peer closed)",
            ));
        }
        buf = &buf[n..];
    }
    Ok(())
}

/// `read_exact` with an idle timeout re-armed on every successful read.
pub async fn read_exact_idle<S>(stream: &mut S, buf: &mut [u8]) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let mut off = 0;
    while off < buf.len() {
        let n = tokio::time::timeout(IO_IDLE_TIMEOUT, stream.read(&mut buf[off..]))
            .await
            .map_err(|_| BackupError::phase(Phase::Transfer, "read idle timeout"))?
            .map_err(|e| BackupError::phase_src(Phase::Transfer, "read", e))?;
        if n == 0 {
            return Err(BackupError::phase(
                Phase::Transfer,
                "unexpected EOF mid-frame",
            ));
        }
        off += n;
    }
    Ok(())
}

/// Hex BLAKE3 digest of a byte slice.
pub fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::ControlFrame;

    #[test]
    fn legacy_plan_defaults_to_one_multiplexed_carrier() {
        let frame: ControlFrame = serde_json::from_value(serde_json::json!({
            "Plan": {
                "plan": {
                    "format_version": 1,
                    "module": "test",
                    "mode": "copy1to1",
                    "created_at": "1970-01-01T00:00:00Z",
                    "source_summary": "legacy",
                    "items": [],
                    "estimated_bytes": 0,
                    "integrity": { "algorithm": "blake3", "per_item": true },
                    "payload": null
                }
            }
        }))
        .expect("legacy Plan must remain decodable");

        match frame {
            ControlFrame::Plan {
                carriers,
                separate_data_streams,
                ..
            } => {
                assert_eq!(carriers, 1);
                assert!(!separate_data_streams);
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn legacy_plan_ack_defaults_to_one_multiplexed_carrier() {
        let frame: ControlFrame = serde_json::from_value(serde_json::json!({
            "PlanAck": { "accepted": true, "reason": "" }
        }))
        .expect("legacy PlanAck must remain decodable");

        match frame {
            ControlFrame::PlanAck {
                carriers,
                separate_data_streams,
                ..
            } => {
                assert_eq!(carriers, 1);
                assert!(!separate_data_streams);
            }
            other => panic!("expected PlanAck, got {other:?}"),
        }
    }
}
