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
/// Max body size for the one frame that legitimately scales with the plan:
/// [`ControlFrame::Plan`].
///
/// Every other frame is a fixed-shape control message or a `CHUNK_SIZE` payload
/// chunk, so [`FRAME_LIMIT`] bounds them with room to spare. The plan is
/// different — it carries one entry per restore unit, up to
/// [`crate::channel::MAX_PLAN_ITEMS`] of them, and it is sent whole because the
/// destination must validate and restore from the plan alone. At 16 MiB the item
/// ceiling was unreachable: a filesystem tree encodes to roughly 210 bytes per
/// entry and an S3 bucket to roughly 340, so the frame bound bit first and did it
/// with a generic "too large", not with the item count the operator could act on.
///
/// The budget here is ~670 bytes per item at the current ceiling — about twice
/// the fattest realistic shape (S3, whose item meta carries etag, storage class
/// and content type). `plan_frame_budget_covers_the_item_ceiling` in
/// `rb-core/tests/wire_test.rs` is what keeps the two constants honest: raise
/// [`crate::channel::MAX_PLAN_ITEMS`] and that test tells you to raise this too.
///
/// The larger allocation is read exactly once, at a known point in the protocol
/// (`channel::recv_plan`), after the two sides are already paired on a secret —
/// it is not a bound an arbitrary frame can reach.
pub const PLAN_FRAME_LIMIT: usize = 128 * 1024 * 1024; // 128 MiB
/// Idle timeout for a handshake read the caller is *waiting* on (re-armed on
/// progress). Short on purpose: a peer that owes a completion frame and stops
/// writing is wedged, not merely slow. Payload I/O uses
/// [`payload_idle_timeout`] instead — see why there.
pub const IO_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default idle timeout for a single payload read/write syscall.
///
/// A payload write rides the substream's flow-control window, and I-BANDWIDTH
/// says a slow destination MUST stall it: the destination stops reading while
/// it applies what it already holds — an S3 `upload_part` of a whole multipart
/// part (up to ~524 MiB), a MongoDB `insert_many` batch, a PostgreSQL `COPY`
/// flush waiting on a lock — and the window stays full for exactly that long.
/// The previous 30 s bound turned every such pause into `write idle timeout`,
/// aborting runs that the backpressure design says must simply wait: a 500 MiB
/// S3 part over a 100 Mbit/s uplink needs ~42 s of it.
///
/// The bound survives only to fail a genuinely wedged peer. Channel liveness is
/// proved independently and much sooner by the transport's control heartbeat
/// (20 s) and the coordination server's 60 s recv-deadline reaper, so making it
/// generous here costs no detection latency that matters.
pub const DEFAULT_PAYLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// [`DEFAULT_PAYLOAD_IDLE_TIMEOUT`], overridable with
/// `RUST_BACKUP_IO_IDLE_TIMEOUT` (whole seconds).
///
/// Resolved once and cached: this is read on every chunk of every item.
pub fn payload_idle_timeout() -> Duration {
    static RESOLVED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        parse_idle_timeout(std::env::var("RUST_BACKUP_IO_IDLE_TIMEOUT").ok().as_deref())
    })
}

/// Pure half of [`payload_idle_timeout`]. An absent, unparsable or zero value
/// all keep the default: `0` must not be a way to disable the bound entirely,
/// which would let a wedged peer hold a run open forever.
fn parse_idle_timeout(raw: Option<&str>) -> Duration {
    raw.and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PAYLOAD_IDLE_TIMEOUT)
}

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
    send_frame_bounded(stream, frame, FRAME_LIMIT, Phase::Transfer).await
}

/// [`send_frame`] with an explicit size bound and phase, for the plan frame.
pub async fn send_frame_bounded<S, T>(
    stream: &mut S,
    frame: &T,
    limit: usize,
    phase: Phase,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(frame)
        .map_err(|e| BackupError::phase_src(phase, "frame serialize", e))?;
    if body.len() > limit {
        return Err(BackupError::phase(
            phase,
            format!("frame is {} bytes; limit is {limit}", body.len()),
        ));
    }
    let len = (body.len() as u32).to_le_bytes();
    write_all_idle(stream, &len).await?;
    write_all_idle(stream, &body).await?;
    stream
        .flush()
        .await
        .map_err(|e| BackupError::phase_src(phase, "frame flush", e))?;
    Ok(())
}

/// Read a length-prefixed JSON frame. Returns `None` on clean EOF.
pub async fn recv_frame<S, T>(stream: &mut S) -> Result<Option<T>>
where
    S: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    recv_frame_bounded(stream, FRAME_LIMIT, Phase::Transfer).await
}

/// [`recv_frame`] with an explicit size bound and phase, for the plan frame.
///
/// The bound is checked against the declared length BEFORE the body buffer is
/// allocated, so an inflated length prefix costs nothing.
pub async fn recv_frame_bounded<S, T>(
    stream: &mut S,
    limit: usize,
    phase: Phase,
) -> Result<Option<T>>
where
    S: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(BackupError::phase_src(phase, "frame len read", e)),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > limit {
        return Err(BackupError::phase(
            phase,
            format!("incoming frame is {len} bytes; limit is {limit}"),
        ));
    }
    let mut body = vec![0u8; len];
    read_exact_idle(stream, &mut body).await?;
    let frame = serde_json::from_slice(&body)
        .map_err(|e| BackupError::phase_src(phase, "frame deserialize", e))?;
    Ok(Some(frame))
}

/// `write_all` with an idle timeout re-armed on every successful write — a stuck
/// peer fails eventually instead of hanging forever. (Ported from
/// `bore::transfer`.) The bound is [`payload_idle_timeout`], not
/// [`IO_IDLE_TIMEOUT`]: a full flow-control window is backpressure, not a fault.
pub async fn write_all_idle<S>(stream: &mut S, mut buf: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let idle = payload_idle_timeout();
    while !buf.is_empty() {
        let n = tokio::time::timeout(idle, stream.write(buf))
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

/// `read_exact` with an idle timeout re-armed on every successful read. Bounded
/// by [`payload_idle_timeout`] for the same reason as [`write_all_idle`]: the
/// producer's own backend can legitimately go quiet mid-frame (PostgreSQL's
/// `ORDER BY` blocking sort emits no `COPY` byte until the table is sorted).
pub async fn read_exact_idle<S>(stream: &mut S, buf: &mut [u8]) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let idle = payload_idle_timeout();
    let mut off = 0;
    while off < buf.len() {
        let n = tokio::time::timeout(idle, stream.read(&mut buf[off..]))
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
    use super::{parse_idle_timeout, ControlFrame, DEFAULT_PAYLOAD_IDLE_TIMEOUT, IO_IDLE_TIMEOUT};
    use std::time::Duration;

    /// The payload bound must stay well clear of the handshake bound: a
    /// destination applying one S3 multipart part, one MongoDB insert batch or
    /// one blocked PostgreSQL `COPY` flush stops reading for longer than the
    /// handshake bound allows, and that is backpressure working, not a fault.
    #[test]
    fn the_payload_idle_bound_is_far_above_the_handshake_bound() {
        assert!(
            DEFAULT_PAYLOAD_IDLE_TIMEOUT >= IO_IDLE_TIMEOUT * 10,
            "payload {DEFAULT_PAYLOAD_IDLE_TIMEOUT:?} vs handshake {IO_IDLE_TIMEOUT:?}"
        );
    }

    /// `0` and garbage keep the default rather than disabling the bound, which
    /// would let a wedged peer hold a run open forever.
    #[test]
    fn the_idle_override_never_removes_the_bound() {
        assert_eq!(parse_idle_timeout(Some("90")), Duration::from_secs(90));
        assert_eq!(parse_idle_timeout(Some(" 90 ")), Duration::from_secs(90));
        for refused in [None, Some(""), Some("0"), Some("-1"), Some("later")] {
            assert_eq!(
                parse_idle_timeout(refused),
                DEFAULT_PAYLOAD_IDLE_TIMEOUT,
                "{refused:?} must fall back to the default"
            );
        }
    }

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
