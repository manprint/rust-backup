//! Session orchestration — the run loops that tie a [`Source`]/[`Destination`]
//! to a [`DataChannel`], enforcing the project invariants end to end.
//!
//! Skeleton uses a single substream (carriers == 1), which is byte-correct;
//! multi-carrier parallel item distribution is a documented enhancement (see the
//! plan, Phase 5). The control + data + completion frames ride that one stream
//! sequentially: `Plan → PlanAck → [chunks…] → StreamEnd → Done`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{info, warn};

use crate::channel::{self, ChunkSink, DataChannel, StreamChunkSink, StreamChunkSource};
use crate::error::{BackupError, Phase, Result};
use crate::module::{Destination, Source};
use crate::plan::BackupPlan;
use crate::progress::Progress;
use crate::wire::{self, ControlFrame};

/// Outcome of a completed source run (returned for logging/tests).
#[derive(Debug)]
pub struct SourceOutcome {
    pub plan: BackupPlan,
    pub bytes_sent: u64,
}

/// Run the SOURCE side: analyze (read-only) → send plan → await accept → stream
/// payload → audit immutability.
///
/// SOURCE-IMMUTABILITY: the source fingerprint is captured before analyze and
/// re-checked after streaming; any change is a hard [`BackupError::SourceMutated`].
pub async fn run_source(
    source: &dyn Source,
    channel: &dyn DataChannel,
    progress: &Progress,
) -> Result<SourceOutcome> {
    run_source_limited(source, channel, progress, None).await
}

/// As [`run_source`], with an optional aggregate payload rate cap. It is
/// applied on the sink, preserving normal transport backpressure and avoiding
/// any read-ahead buffer.
pub async fn run_source_limited(
    source: &dyn Source,
    channel: &dyn DataChannel,
    progress: &Progress,
    max_rate: Option<u64>,
) -> Result<SourceOutcome> {
    // 1. Immutability baseline (read-only).
    let fp_before = source.fingerprint().await?;

    let outcome = source_run(source, channel, progress, max_rate).await;

    // 6. Immutability audit — the central invariant. It runs on EVERY exit path,
    // including a run that failed or was aborted mid-stream: an aborted transfer
    // is exactly the case where a half-applied source write would hide.
    match source.fingerprint().await {
        Ok(fp_after) if fp_after != fp_before => Err(BackupError::SourceMutated(format!(
            "source fingerprint changed during backup ({fp_before} -> {fp_after})"
        ))),
        Ok(_) => {
            if let Ok(o) = &outcome {
                info!(bytes = o.bytes_sent, "source done; immutability verified");
            } else {
                info!("source run failed; source verified unchanged");
            }
            outcome
        }
        // A failed audit cannot prove immutability. If the run itself already
        // failed, that error is the more informative one; otherwise surface the
        // audit failure — a "successful" run we cannot audit is not a success.
        Err(audit_err) => match outcome {
            Err(run_err) => Err(run_err),
            Ok(_) => Err(BackupError::phase_src(
                Phase::Verify,
                "post-run source fingerprint audit failed",
                audit_err,
            )),
        },
    }
}

/// The source run proper (analyze → plan exchange → stream → Done), without the
/// immutability audit, which [`run_source_limited`] applies to every exit path.
async fn source_run(
    source: &dyn Source,
    channel: &dyn DataChannel,
    progress: &Progress,
    max_rate: Option<u64>,
) -> Result<SourceOutcome> {
    // 2. Analyze and build the self-contained plan (read-only).
    let plan = source.analyze().await?;
    progress.set_totals(plan.items.len(), plan.estimated_bytes);
    info!(module = %plan.module, items = plan.items.len(), "source plan ready");
    info!("\n{}", plan.render());

    // 3. Provider accepts the consumer's substream; exchange plan + decision.
    let stream = channel.accept_stream().await?;
    let mut stream = stream;
    channel::send_plan(&mut stream, &plan).await?;
    // recv_ack errors out (PlanRejected) without ever touching the source.
    channel::recv_ack(&mut stream).await?;
    info!("destination accepted plan; streaming payload");

    // 4. Stream the payload (no temp files; backpressure consumer-paced).
    let mut stream_sink = StreamChunkSink::new_counted(stream, progress.clone());
    let streamed = {
        let mut sink = PacedSink::new(&mut stream_sink, max_rate);
        match source.stream_out(&plan, &mut sink).await {
            Ok(()) => sink.finish().await,
            Err(e) => Err(e),
        }
    };
    let mut stream = stream_sink.into_inner();

    // A source-side failure is told to the destination explicitly, so it aborts
    // its restore with a real reason instead of interpreting a mid-stream EOF.
    // Best-effort: the channel may already be gone, which is not a new fault.
    if let Err(e) = streamed {
        let _ = wire::send_frame(
            &mut stream,
            &crate::wire::DataFrame::Abort {
                reason: e.to_string(),
            },
        )
        .await;
        return Err(e);
    }

    // 5. Completion frame.
    let bytes_sent = progress.bytes();
    wire::send_frame(
        &mut stream,
        &ControlFrame::Done {
            total_bytes: bytes_sent,
            blake3: String::new(), // per-item integrity is the guarantee
        },
    )
    .await?;

    Ok(SourceOutcome { plan, bytes_sent })
}

struct PacedSink<'a> {
    inner: &'a mut dyn ChunkSink,
    rate: Option<u64>,
    started: Instant,
    sent: u64,
}

impl<'a> PacedSink<'a> {
    fn new(inner: &'a mut dyn ChunkSink, rate: Option<u64>) -> Self {
        Self {
            inner,
            rate: rate.filter(|rate| *rate > 0),
            started: Instant::now(),
            sent: 0,
        }
    }
    fn delay_for(rate: u64, sent: u64, elapsed: Duration) -> Option<Duration> {
        Duration::from_secs_f64(sent as f64 / rate as f64).checked_sub(elapsed)
    }
    async fn pace(&mut self, bytes: usize) {
        let Some(rate) = self.rate else { return };
        self.sent = self.sent.saturating_add(bytes as u64);
        if let Some(delay) = Self::delay_for(rate, self.sent, self.started.elapsed()) {
            tokio::time::sleep(delay).await;
        }
    }
}

#[async_trait]
impl ChunkSink for PacedSink<'_> {
    async fn send_chunk(&mut self, item_id: u32, offset: u64, data: &[u8]) -> Result<()> {
        self.pace(data.len()).await;
        self.inner.send_chunk(item_id, offset, data).await
    }
    async fn finish_item(&mut self, item_id: u32, total: u64, blake3: &str) -> Result<()> {
        self.inner.finish_item(item_id, total, blake3).await
    }
    async fn finish(&mut self) -> Result<()> {
        self.inner.finish().await
    }
}

/// Run the DESTINATION side: receive plan → validate → (async-accept) confirm →
/// apply streamed payload.
///
/// `accept` is the async-accept policy: given the plan it returns whether to
/// proceed (interactive `yes`, or auto-accept). It is only consulted when
/// preflight passes.
pub async fn run_destination(
    dest: &dyn Destination,
    channel: &dyn DataChannel,
    progress: &Progress,
    accept: &mut (dyn FnMut(&BackupPlan) -> bool + Send),
) -> Result<BackupPlan> {
    // 1. Consumer opens the substream and receives the plan.
    let stream = channel.open_stream().await?;
    let mut stream = stream;
    // An unreadable/unsupported plan is refused on the wire too, so the source
    // fails fast with the reason instead of blocking on a PlanAck that never comes.
    let plan = match channel::recv_plan(&mut stream).await {
        Ok(plan) => plan,
        Err(e) => {
            let _ = channel::send_ack(&mut stream, false, &e.to_string()).await;
            return Err(e);
        }
    };
    progress.set_totals(plan.items.len(), plan.estimated_bytes);
    info!(module = %plan.module, "destination received plan");
    info!("\n{}", plan.render());

    // 2. Preflight (disk space, accessibility, version compat, privileges).
    let preflight = dest.validate(&plan).await?;
    for c in &preflight.checks {
        if c.passed {
            info!(check = %c.name, "{}", c.detail);
        } else {
            warn!(check = %c.name, "FAILED: {}", c.detail);
        }
    }

    // 3. Decide: preflight must pass AND the accept policy must approve.
    let approved = preflight.ok && accept(&plan);
    let reason = if !preflight.ok {
        "preflight failed".to_string()
    } else if !approved {
        "rejected by operator".to_string()
    } else {
        "accepted".to_string()
    };
    channel::send_ack(&mut stream, approved, &reason).await?;
    if !approved {
        return Err(if !preflight.ok {
            BackupError::Preflight(reason)
        } else {
            BackupError::PlanRejected(reason)
        });
    }

    // 4. Apply the streamed payload (no temp files).
    let mut src = StreamChunkSource::new_counted(stream, progress.clone());
    dest.stream_in(&plan, &mut src).await?;
    let mut stream = src.into_inner();

    // 5. Completion frame.
    match wire::recv_frame::<_, ControlFrame>(&mut stream).await? {
        Some(ControlFrame::Done { total_bytes, .. }) => {
            info!(bytes = total_bytes, "destination done; restore complete");
        }
        Some(ControlFrame::Abort { reason }) => {
            return Err(BackupError::phase(
                Phase::Apply,
                format!("source aborted: {reason}"),
            ));
        }
        other => warn!("expected Done, got {other:?}"),
    }
    Ok(plan)
}

#[cfg(test)]
mod pacing_tests {
    use super::PacedSink;
    use std::time::Duration;

    #[test]
    fn limiter_waits_only_when_ahead_of_schedule() {
        assert_eq!(
            PacedSink::delay_for(100, 100, Duration::ZERO),
            Some(Duration::from_secs(1))
        );
        assert_eq!(PacedSink::delay_for(100, 100, Duration::from_secs(2)), None);
    }
}
