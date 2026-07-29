//! Session orchestration — the run loops that tie a [`Source`]/[`Destination`]
//! to a [`DataChannel`], enforcing the project invariants end to end.
//!
//! With one carrier the control and data frames remain byte-identical to the
//! original `Plan → PlanAck → [chunks…] → StreamEnd → Done` stream. With more
//! carriers, that stream is control-only after PlanAck and item-pinned data
//! frames use independent substreams.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::channel::{
    self, ChunkSink, DataChannel, MultiStreamChunkSink, MultiStreamChunkSource, StreamChunkSink,
    StreamChunkSource,
};
use crate::error::{BackupError, Phase, Result};
use crate::module::{Destination, Source};
use crate::plan::BackupPlan;
use crate::progress::Progress;
use crate::wire::{self, ControlFrame};

/// Budget for the plan/ack exchange. Data traffic has its independent idle
/// timeout in `wire::IO_IDLE_TIMEOUT`; an operator may inspect a plan, but a
/// half-open peer must not wedge a session forever.
const DEFAULT_PLAN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn plan_exchange_timeout() -> Duration {
    std::env::var("RUST_BACKUP_PLAN_TIMEOUT")
        .ok()
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PLAN_EXCHANGE_TIMEOUT)
}

async fn exchange_timeout<T>(
    side: &str,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    exchange_timeout_with(plan_exchange_timeout(), side, future).await
}

async fn exchange_timeout_with<T>(
    timeout: Duration,
    side: &str,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(timeout, future).await.map_err(|_| {
        BackupError::phase(
            Phase::Connect,
            format!("plan exchange timed out waiting for {side}"),
        )
    })?
}

fn validate_completion_ack(frame: Option<ControlFrame>) -> Result<()> {
    match frame {
        Some(ControlFrame::CompleteAck) => Ok(()),
        Some(ControlFrame::Abort { reason }) => Err(BackupError::phase(
            Phase::Apply,
            format!("destination aborted: {reason}"),
        )),
        other => Err(BackupError::phase(
            Phase::Verify,
            format!("expected CompleteAck, got {other:?}"),
        )),
    }
}

async fn send_completion_ack<S>(stream: &mut S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    wire::send_frame(stream, &ControlFrame::CompleteAck).await?;
    let ack = tokio::time::timeout(
        wire::IO_IDLE_TIMEOUT,
        wire::recv_frame::<_, ControlFrame>(stream),
    )
    .await
    .map_err(|_| {
        BackupError::phase(
            Phase::Verify,
            "timed out waiting for source completion acknowledgement",
        )
    })??;
    if !matches!(ack, Some(ControlFrame::CompleteAckAck)) {
        return Err(BackupError::phase(
            Phase::Verify,
            format!("expected CompleteAckAck, got {ack:?}"),
        ));
    }
    stream.shutdown().await.map_err(|error| {
        BackupError::phase_src(
            Phase::Transfer,
            "gracefully close destination completion stream",
            error,
        )
    })
}

async fn send_completion_ack_ack<S>(stream: &mut S) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    wire::send_frame(stream, &ControlFrame::CompleteAckAck).await?;
    stream.shutdown().await.map_err(|error| {
        BackupError::phase_src(
            Phase::Transfer,
            "gracefully close source completion stream",
            error,
        )
    })
}

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
    channel::send_plan(&mut stream, &plan, channel.carriers()).await?;
    // recv_ack errors out (PlanRejected) without ever touching the source.
    let (peer_carriers, separate_data_streams) =
        exchange_timeout("destination PlanAck", channel::recv_ack(&mut stream)).await?;
    let agreed_carriers = channel.carriers().min(peer_carriers as usize).max(1);
    if agreed_carriers != channel.carriers() {
        info!(
            requested = channel.carriers(),
            agreed = agreed_carriers,
            "destination downgraded carrier count"
        );
    }
    info!("destination accepted plan; streaming payload");

    if separate_data_streams || agreed_carriers > 1 {
        return source_stream_multi(
            source,
            channel,
            progress,
            max_rate,
            plan,
            stream,
            agreed_carriers,
        )
        .await;
    }

    // Legacy peer layout: plan/control and payload share one substream. New
    // peers negotiate a separate data stream above so their control reader can
    // observe aborts concurrently without splitting a yamux payload stream.
    let mut stream_sink = StreamChunkSink::new_counted(stream, progress.clone());
    let (streamed, payload_digest) = {
        let mut sink = PacedSink::new(&mut stream_sink, max_rate);
        let result = match source.stream_out(&plan, &mut sink).await {
            Ok(()) => sink.finish().await,
            Err(e) => Err(e),
        };
        (result, channel::completion_digest(&sink.completed_items))
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
        let _ = stream.shutdown().await;
        return Err(e);
    }

    // 5. Completion frame.
    let bytes_sent = progress.bytes();
    wire::send_frame(
        &mut stream,
        &ControlFrame::Done {
            total_bytes: bytes_sent,
            blake3: payload_digest,
        },
    )
    .await?;
    let completion = tokio::time::timeout(
        wire::IO_IDLE_TIMEOUT,
        wire::recv_frame::<_, ControlFrame>(&mut stream),
    )
    .await
    .map_err(|_| {
        BackupError::phase(
            Phase::Verify,
            "timed out waiting for destination completion acknowledgement",
        )
    })??;
    validate_completion_ack(completion)?;
    send_completion_ack_ack(&mut stream).await?;

    Ok(SourceOutcome { plan, bytes_sent })
}

/// Source payload path for the negotiated separate-data layout. The plan/control
/// stream is kept out of the data plane even with one carrier; an item always
/// maps to exactly one data substream.
async fn source_stream_multi(
    source: &dyn Source,
    channel: &dyn DataChannel,
    progress: &Progress,
    max_rate: Option<u64>,
    plan: BackupPlan,
    control: Box<dyn crate::channel::DuplexStream>,
    carriers: usize,
) -> Result<SourceOutcome> {
    let mut streams = Vec::with_capacity(carriers);
    for index in 0..carriers {
        let mut stream = exchange_timeout(
            &format!("destination opening data carrier {index}"),
            channel.accept_stream(),
        )
        .await?;
        wire::send_frame(
            &mut stream,
            &wire::DataFrame::CarrierHello {
                carrier: index as u16,
            },
        )
        .await?;
        streams.push(stream);
    }
    // Start the sole control reader only after carrier setup.  Starting it
    // before a fallible setup step detached the task whenever `?` returned.
    // An Abort sent during setup remains buffered on the control stream.
    let (control_read, mut control_write) = tokio::io::split(control);
    let (abort_tx, mut abort_rx) = watch::channel(None::<String>);
    let abort_task = tokio::spawn(async move {
        let mut control_read = control_read;
        let received = wire::recv_frame::<_, ControlFrame>(&mut control_read).await;
        let observed = match &received {
            Ok(Some(ControlFrame::CompleteAck)) => None,
            Ok(Some(ControlFrame::Abort { reason })) => {
                Some(format!("destination aborted: {reason}"))
            }
            Ok(Some(other)) => Some(format!(
                "unexpected control frame while streaming: {other:?}"
            )),
            Ok(None) => Some("control stream closed while streaming".to_string()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(observed) = observed {
            let _ = abort_tx.send(Some(observed));
        }
        received
    });
    let mut data_sink = MultiStreamChunkSink::new_counted(streams, progress.clone())?;
    let (streamed, payload_digest) = {
        let mut sink = PacedSink::new(&mut data_sink, max_rate).with_abort_watch(&mut abort_rx);
        let result = match source.stream_out(&plan, &mut sink).await {
            Ok(()) => sink.finish().await,
            Err(e) => Err(e),
        };
        (result, channel::completion_digest(&sink.completed_items))
    };
    if let Err(error) = streamed {
        data_sink.abort(&error.to_string()).await;
        let _ = wire::send_frame(
            &mut control_write,
            &ControlFrame::Abort {
                reason: error.to_string(),
            },
        )
        .await;
        abort_task.abort();
        let _ = abort_task.await;
        return Err(error);
    }
    let bytes_sent = progress.bytes();
    let done_result = wire::send_frame(
        &mut control_write,
        &ControlFrame::Done {
            total_bytes: bytes_sent,
            blake3: payload_digest,
        },
    )
    .await;
    if let Err(error) = done_result {
        abort_task.abort();
        let _ = abort_task.await;
        return Err(error);
    }
    let completion = tokio::time::timeout(wire::IO_IDLE_TIMEOUT, abort_task)
        .await
        .map_err(|_| {
            BackupError::phase(
                Phase::Verify,
                "timed out waiting for destination completion acknowledgement",
            )
        })?
        .map_err(|error| {
            BackupError::phase(
                Phase::Transfer,
                format!("completion watcher task failed: {error}"),
            )
        })??;
    validate_completion_ack(completion)?;
    send_completion_ack_ack(&mut control_write).await?;
    Ok(SourceOutcome { plan, bytes_sent })
}

struct PacedSink<'a> {
    inner: &'a mut dyn ChunkSink,
    rate: Option<u64>,
    started: Instant,
    sent: u64,
    completed_items: BTreeMap<u32, String>,
    abort_watch: Option<&'a mut watch::Receiver<Option<String>>>,
}

impl<'a> PacedSink<'a> {
    fn new(inner: &'a mut dyn ChunkSink, rate: Option<u64>) -> Self {
        Self {
            inner,
            rate: rate.filter(|rate| *rate > 0),
            started: Instant::now(),
            sent: 0,
            completed_items: BTreeMap::new(),
            abort_watch: None,
        }
    }
    fn with_abort_watch(mut self, abort_watch: &'a mut watch::Receiver<Option<String>>) -> Self {
        self.abort_watch = Some(abort_watch);
        self
    }
    fn delay_for(rate: u64, sent: u64, elapsed: Duration) -> Option<Duration> {
        Duration::from_secs_f64(sent as f64 / rate as f64).checked_sub(elapsed)
    }
    fn abort_error(abort_watch: &watch::Receiver<Option<String>>) -> BackupError {
        BackupError::phase(
            Phase::Transfer,
            abort_watch
                .borrow()
                .clone()
                .unwrap_or_else(|| "destination control stream closed".to_string()),
        )
    }
    async fn pace(&mut self, bytes: usize) -> Result<()> {
        let Some(rate) = self.rate else { return Ok(()) };
        self.sent = self.sent.saturating_add(bytes as u64);
        if let Some(delay) = Self::delay_for(rate, self.sent, self.started.elapsed()) {
            if let Some(abort_watch) = self.abort_watch.as_deref_mut() {
                if abort_watch.borrow().is_some() {
                    return Err(Self::abort_error(abort_watch));
                }
                tokio::select! {
                    biased;
                    _ = abort_watch.changed() => return Err(Self::abort_error(abort_watch)),
                    _ = tokio::time::sleep(delay) => {}
                }
            } else {
                tokio::time::sleep(delay).await;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ChunkSink for PacedSink<'_> {
    async fn send_chunk(&mut self, item_id: u32, offset: u64, data: &[u8]) -> Result<()> {
        self.pace(data.len()).await?;
        let inner = &mut self.inner;
        let Some(abort_watch) = self.abort_watch.as_deref_mut() else {
            return inner.send_chunk(item_id, offset, data).await;
        };
        if abort_watch.borrow().is_some() {
            return Err(Self::abort_error(abort_watch));
        }
        tokio::select! {
            biased;
            _ = abort_watch.changed() => Err(Self::abort_error(abort_watch)),
            result = inner.send_chunk(item_id, offset, data) => result,
        }
    }
    async fn finish_item(&mut self, item_id: u32, total: u64, blake3: &str) -> Result<()> {
        if self.completed_items.contains_key(&item_id) {
            return Err(BackupError::phase(
                Phase::Verify,
                format!("source emitted duplicate ItemEnd for item={item_id}"),
            ));
        }
        let inner = &mut self.inner;
        if let Some(abort_watch) = self.abort_watch.as_deref_mut() {
            if abort_watch.borrow().is_some() {
                return Err(Self::abort_error(abort_watch));
            }
            tokio::select! {
                biased;
                _ = abort_watch.changed() => return Err(Self::abort_error(abort_watch)),
                result = inner.finish_item(item_id, total, blake3) => result?,
            }
        } else {
            inner.finish_item(item_id, total, blake3).await?;
        }
        self.completed_items.insert(item_id, blake3.to_string());
        Ok(())
    }
    async fn finish(&mut self) -> Result<()> {
        let inner = &mut self.inner;
        let Some(abort_watch) = self.abort_watch.as_deref_mut() else {
            return inner.finish().await;
        };
        if abort_watch.borrow().is_some() {
            return Err(Self::abort_error(abort_watch));
        }
        tokio::select! {
            biased;
            _ = abort_watch.changed() => Err(Self::abort_error(abort_watch)),
            result = inner.finish() => result,
        }
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
    run_destination_with_accept(dest, channel, progress, &mut |plan| {
        std::future::ready(accept(plan))
    })
    .await
}

/// Async form of [`run_destination`]. Interactive callers must use this so
/// stdin reads can run in Tokio's blocking pool rather than stall I/O workers.
pub async fn run_destination_with_accept<F, Fut>(
    dest: &dyn Destination,
    channel: &dyn DataChannel,
    progress: &Progress,
    accept: &mut F,
) -> Result<BackupPlan>
where
    F: FnMut(&BackupPlan) -> Fut + Send,
    Fut: std::future::Future<Output = bool> + Send,
{
    // 1. Consumer opens the substream and receives the plan.
    let stream = channel.open_stream().await?;
    let mut stream = stream;
    // An unreadable/unsupported plan is refused on the wire too, so the source
    // fails fast with the reason instead of blocking on a PlanAck that never comes.
    let (plan, source_requested_carriers, separate_data_streams) =
        match exchange_timeout("source plan", channel::recv_plan(&mut stream)).await {
            Ok(plan) => plan,
            Err(e) => {
                let _ = channel::send_ack(&mut stream, false, &e.to_string(), 1, false).await;
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
    let approved = preflight.ok && accept(&plan).await;
    let reason = if !preflight.ok {
        "preflight failed".to_string()
    } else if !approved {
        "rejected by operator".to_string()
    } else {
        "accepted".to_string()
    };
    let agreed_carriers = (source_requested_carriers as usize)
        .min(channel.carriers())
        .min(dest.max_carriers())
        .max(1);
    channel::send_ack(
        &mut stream,
        approved,
        &reason,
        agreed_carriers,
        separate_data_streams,
    )
    .await?;
    if !approved {
        return Err(if !preflight.ok {
            BackupError::Preflight(reason)
        } else {
            BackupError::PlanRejected(reason)
        });
    }

    if separate_data_streams || agreed_carriers > 1 {
        return destination_stream_multi(dest, channel, progress, plan, stream, agreed_carriers)
            .await;
    }

    // 4. Apply the streamed payload (no temp files).
    let mut src = StreamChunkSource::new_counted(stream, progress.clone());
    let apply_result = dest.stream_in(&plan, &mut src).await;
    let received_items = src.completed_item_ids();
    let received_digest = src.completion_digest();
    let mut stream = src.into_inner();
    if let Err(error) = apply_result {
        // Same substream is parsing data frames at this point; a data-plane
        // abort preserves the reason for a source that is between item writes.
        let _ = wire::send_frame(
            &mut stream,
            &wire::DataFrame::Abort {
                reason: error.to_string(),
            },
        )
        .await;
        let _ = stream.shutdown().await;
        return Err(error);
    }

    let expected_items: BTreeSet<u32> = plan
        .items
        .iter()
        .filter(|item| item.expects_data())
        .map(|item| item.id)
        .collect();
    if received_items != expected_items {
        let missing: Vec<_> = expected_items
            .difference(&received_items)
            .copied()
            .collect();
        let unexpected: Vec<_> = received_items
            .difference(&expected_items)
            .copied()
            .collect();
        return Err(BackupError::phase(
            Phase::Verify,
            format!("item completion mismatch; missing={missing:?} unexpected={unexpected:?}"),
        ));
    }

    // 5. Completion frame.
    match wire::recv_frame::<_, ControlFrame>(&mut stream).await? {
        Some(ControlFrame::Done {
            total_bytes,
            blake3,
        }) => {
            if total_bytes != progress.bytes() {
                return Err(BackupError::phase(
                    Phase::Verify,
                    format!(
                        "Done.total_bytes={total_bytes} but received={}",
                        progress.bytes()
                    ),
                ));
            }
            if blake3 != received_digest {
                return Err(BackupError::phase(
                    Phase::Verify,
                    "Done.blake3 does not match verified item digests",
                ));
            }
            send_completion_ack(&mut stream).await?;
            info!(bytes = total_bytes, "destination done; restore complete");
        }
        Some(ControlFrame::Abort { reason }) => {
            return Err(BackupError::phase(
                Phase::Apply,
                format!("source aborted: {reason}"),
            ));
        }
        other => {
            return Err(BackupError::phase(
                Phase::Verify,
                format!("expected Done, got {other:?}"),
            ));
        }
    }
    Ok(plan)
}

/// Destination payload path for the negotiated separate-data layout. Reader
/// tasks merge one or more independent item-pinned streams back into the
/// module's single `ChunkSource` interface.
async fn destination_stream_multi(
    dest: &dyn Destination,
    channel: &dyn DataChannel,
    progress: &Progress,
    plan: BackupPlan,
    mut control: Box<dyn crate::channel::DuplexStream>,
    carriers: usize,
) -> Result<BackupPlan> {
    let mut streams = Vec::with_capacity(carriers);
    for index in 0..carriers {
        streams.push(
            exchange_timeout(
                &format!("source accepting data carrier {index}"),
                channel.open_stream(),
            )
            .await?,
        );
    }
    // `open_stream` and `accept_stream` are independently scheduled through
    // the relay.  Bind their logical indexes from the explicit hello, never
    // from arrival order.
    let mut by_carrier: Vec<Option<Box<dyn crate::channel::DuplexStream>>> =
        (0..carriers).map(|_| None).collect();
    for mut stream in streams {
        let hello = wire::recv_frame::<_, wire::DataFrame>(&mut stream).await?;
        let Some(wire::DataFrame::CarrierHello { carrier }) = hello else {
            return Err(BackupError::phase(
                Phase::Connect,
                "data carrier missing CarrierHello",
            ));
        };
        let carrier = carrier as usize;
        if carrier >= carriers || by_carrier[carrier].is_some() {
            return Err(BackupError::phase(
                Phase::Connect,
                format!("invalid or duplicate data carrier index={carrier}"),
            ));
        }
        by_carrier[carrier] = Some(stream);
    }
    let streams: Vec<_> = by_carrier
        .into_iter()
        .enumerate()
        .map(|(carrier, stream)| {
            stream.ok_or_else(|| {
                BackupError::phase(
                    Phase::Connect,
                    format!("missing data carrier index={carrier}"),
                )
            })
        })
        .collect::<Result<_>>()?;
    let expected_item_ids = plan
        .items
        .iter()
        .filter(|item| item.expects_data())
        .map(|item| item.id)
        .collect();
    let mut src =
        MultiStreamChunkSource::new_ordered(streams, expected_item_ids, progress.clone())?;
    let apply_result = dest.stream_in(&plan, &mut src).await;
    let received_items = src.completed_item_ids();
    let received_digest = src.completion_digest();
    if let Err(error) = apply_result {
        let _ = wire::send_frame(
            &mut control,
            &ControlFrame::Abort {
                reason: error.to_string(),
            },
        )
        .await;
        let _ = control.shutdown().await;
        return Err(error);
    }

    let expected_items: BTreeSet<u32> = plan
        .items
        .iter()
        .filter(|item| item.expects_data())
        .map(|item| item.id)
        .collect();
    if received_items != expected_items {
        let missing: Vec<_> = expected_items
            .difference(&received_items)
            .copied()
            .collect();
        let unexpected: Vec<_> = received_items
            .difference(&expected_items)
            .copied()
            .collect();
        return Err(BackupError::phase(
            Phase::Verify,
            format!("item completion mismatch; missing={missing:?} unexpected={unexpected:?}"),
        ));
    }

    match wire::recv_frame::<_, ControlFrame>(&mut control).await? {
        Some(ControlFrame::Done {
            total_bytes,
            blake3,
        }) => {
            if total_bytes != progress.bytes() {
                return Err(BackupError::phase(
                    Phase::Verify,
                    format!(
                        "Done.total_bytes={total_bytes} but received={}",
                        progress.bytes()
                    ),
                ));
            }
            if blake3 != received_digest {
                return Err(BackupError::phase(
                    Phase::Verify,
                    "Done.blake3 does not match verified item digests",
                ));
            }
            send_completion_ack(&mut control).await?;
            info!(
                bytes = total_bytes,
                carriers = channel.carriers(),
                "destination done; restore complete"
            );
        }
        Some(ControlFrame::Abort { reason }) => {
            return Err(BackupError::phase(
                Phase::Apply,
                format!("source aborted: {reason}"),
            ));
        }
        other => {
            return Err(BackupError::phase(
                Phase::Verify,
                format!("expected Done, got {other:?}"),
            ));
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod pacing_tests {
    use super::{exchange_timeout_with, PacedSink};
    use crate::channel::ChunkSink;
    use crate::error::Result;
    use async_trait::async_trait;
    use std::time::Duration;
    use tokio::sync::watch;

    struct BlockingSink;

    #[async_trait]
    impl ChunkSink for BlockingSink {
        async fn send_chunk(&mut self, _: u32, _: u64, _: &[u8]) -> Result<()> {
            std::future::pending().await
        }

        async fn finish_item(&mut self, _: u32, _: u64, _: &str) -> Result<()> {
            std::future::pending().await
        }

        async fn finish(&mut self) -> Result<()> {
            std::future::pending().await
        }
    }

    #[test]
    fn limiter_waits_only_when_ahead_of_schedule() {
        assert_eq!(
            PacedSink::delay_for(100, 100, Duration::ZERO),
            Some(Duration::from_secs(1))
        );
        assert_eq!(PacedSink::delay_for(100, 100, Duration::from_secs(2)), None);
    }

    #[tokio::test]
    async fn plan_exchange_has_an_explicit_timeout() {
        let err = exchange_timeout_with(
            Duration::from_millis(1),
            "test peer",
            std::future::pending::<Result<()>>(),
        )
        .await
        .expect_err("pending plan exchange must time out");
        assert!(format!("{err}").contains("plan exchange timed out"));
    }

    #[tokio::test]
    async fn destination_abort_interrupts_rate_wait_and_blocked_write() {
        for rate in [Some(1), None] {
            let mut inner = BlockingSink;
            let (abort_tx, mut abort_rx) = watch::channel(None::<String>);
            let mut sink = PacedSink::new(&mut inner, rate).with_abort_watch(&mut abort_rx);
            let notify = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                abort_tx.send(Some("injected apply failure".into())).ok();
            });
            let error =
                tokio::time::timeout(Duration::from_secs(1), sink.send_chunk(7, 0, &[0; 1024]))
                    .await
                    .expect("destination abort must bound the source")
                    .expect_err("destination abort must fail the source");
            assert!(error.to_string().contains("injected apply failure"));
            notify.await.expect("abort notifier");
        }
    }
}
