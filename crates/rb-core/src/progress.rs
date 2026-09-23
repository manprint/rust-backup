//! Lock-free progress accounting, surfaced on both source and destination.
//!
//! INVARIANT (observability): every run emits clear progress on both sides. The
//! counters are atomic so the hot data path increments them without contention;
//! a caller-owned ticker renders [`Progress::line`] (or [`Snapshot::render`])
//! periodically.
//!
//! A run is a sequence of [`Stage`]s. The transfer is only one of them: a
//! PostgreSQL destination spends minutes building indexes after the last byte
//! arrived, and every destination then reads the whole payload back. Each stage
//! that has countable work publishes it through [`Progress::begin_work`] and
//! the `work_*` counters, so the operator sees *what* is running and how far
//! along it is, instead of a transfer line frozen at its last value.
//!
//! Modules do not receive the handle through their trait: the session runs them
//! inside [`scope`], and a module reaches the run's handle with [`current`].

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::plan::human_bytes;

/// What a run is doing right now. Ordered as a run goes through them; a
/// destination skips [`Stage::Auditing`] and [`Stage::Analyzing`], a source
/// skips [`Stage::Preflight`], [`Stage::Finalizing`] and [`Stage::Verifying`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    /// Waiting for the peer or the plan.
    Connecting = 0,
    /// Fingerprinting the source for the immutability audit.
    Auditing = 1,
    /// Reading the source catalog and building the plan.
    Analyzing = 2,
    /// Destination preflight checks and the operator's decision.
    Preflight = 3,
    /// Payload bytes are moving.
    Transferring = 4,
    /// The destination applies what only makes sense after the data (indexes,
    /// constraints, derived data). Module-specific; reported by the module.
    Finalizing = 5,
    /// The destination reads the restored state back and checks it.
    Verifying = 6,
    /// The source has sent everything and waits for the destination's verdict.
    AwaitingPeer = 7,
}

impl Stage {
    fn from_u8(value: u8) -> Stage {
        match value {
            1 => Stage::Auditing,
            2 => Stage::Analyzing,
            3 => Stage::Preflight,
            4 => Stage::Transferring,
            5 => Stage::Finalizing,
            6 => Stage::Verifying,
            7 => Stage::AwaitingPeer,
            _ => Stage::Connecting,
        }
    }

    /// The word a progress line starts with.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Connecting => "connecting",
            Stage::Auditing => "audit",
            Stage::Analyzing => "analyze",
            Stage::Preflight => "preflight",
            Stage::Transferring => "transfer",
            Stage::Finalizing => "finalize",
            Stage::Verifying => "verify",
            Stage::AwaitingPeer => "waiting",
        }
    }
}

/// Shared, cloneable progress handle. Clone freely across tasks.
#[derive(Clone, Default)]
pub struct Progress {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    bytes_done: AtomicU64,
    items_done: AtomicUsize,
    items_total: AtomicUsize,
    bytes_total: AtomicU64,
    stage: AtomicU8,
    work_done: AtomicU64,
    work_total: AtomicU64,
    work_bytes: AtomicU64,
    work_bytes_total: AtomicU64,
    detail: Mutex<String>,
    /// The peer's own latest progress line, when it sends one (the source
    /// learns what the destination is doing while it waits for the verdict).
    peer: Mutex<Option<String>>,
}

/// A consistent-enough copy of every counter, for rendering. The counters are
/// read one by one, so a snapshot taken mid-update can be one step apart
/// between fields; it is only ever displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub stage: Stage,
    pub items_done: usize,
    pub items_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub work_done: u64,
    pub work_total: u64,
    pub work_bytes: u64,
    pub work_bytes_total: u64,
    pub detail: String,
    pub peer: Option<String>,
}

impl Snapshot {
    /// The byte counter the current stage advances: the read-back bytes while
    /// verifying, the payload bytes otherwise. A caller computing a recent rate
    /// differences this value between two snapshots.
    pub fn moving_bytes(&self) -> u64 {
        if self.stage == Stage::Verifying {
            self.work_bytes
        } else {
            self.bytes_done
        }
    }

    /// One progress line. `rate` is bytes per second over whatever window the
    /// caller measured; `stage_secs` is how long the current stage has run.
    pub fn render(&self, rate: u64, stage_secs: f64) -> String {
        let since = format!("{} in this stage", human_duration(stage_secs));
        match self.stage {
            Stage::Connecting => format!("connecting: waiting for the peer and the plan, {since}"),
            Stage::Auditing => {
                format!("audit: fingerprinting the source for the immutability check, {since}")
            }
            Stage::Analyzing => {
                format!("analyze: reading the source and building the plan, {since}")
            }
            Stage::Preflight => {
                format!("preflight: checking the plan against the destination, {since}")
            }
            Stage::Transferring => {
                // The plan's byte total is an estimate (PostgreSQL plans from
                // on-disk sizes), so it is marked as one until the transfer ends.
                let transfer = transfer_counts(self, rate, "~");
                if self.items_total > 0 && self.items_done >= self.items_total {
                    format!("transfer: {transfer}, every item transferred, {since}")
                } else {
                    format!("transfer: {transfer}, {since}")
                }
            }
            Stage::Finalizing => {
                let what = if self.detail.is_empty() {
                    "applying the post-data steps"
                } else {
                    self.detail.as_str()
                };
                if self.work_total > 0 {
                    format!(
                        "finalize: {what}, step {}/{} ({:.1}%), {since}",
                        self.work_done,
                        self.work_total,
                        percent(self.work_done, self.work_total)
                    )
                } else {
                    format!("finalize: {what}, {since}")
                }
            }
            Stage::Verifying => {
                let what = if self.detail.is_empty() {
                    "reading the restored data back"
                } else {
                    self.detail.as_str()
                };
                format!(
                    "verify: {what}, items {}/{}  {}/{}  ({:.1}%)  {}/s, {since}",
                    self.work_done,
                    self.work_total,
                    human_bytes(self.work_bytes),
                    human_bytes(self.work_bytes_total),
                    percent(self.work_bytes, self.work_bytes_total),
                    human_bytes(rate),
                )
            }
            Stage::AwaitingPeer => match &self.peer {
                Some(peer) => format!(
                    "waiting: payload sent ({} items, {}), {since}; destination: {peer}",
                    self.items_done,
                    human_bytes(self.bytes_done),
                ),
                None => format!(
                    "waiting: payload sent ({} items, {}); the destination is applying and \
                     verifying it, {since}",
                    self.items_done,
                    human_bytes(self.bytes_done),
                ),
            },
        }
    }
}

fn transfer_counts(snapshot: &Snapshot, rate: u64, estimate_mark: &str) -> String {
    format!(
        "items {}/{}  {}/{estimate_mark}{}  ({:.1}%)  {}/s",
        snapshot.items_done,
        snapshot.items_total,
        human_bytes(snapshot.bytes_done),
        human_bytes(snapshot.bytes_total),
        percent(snapshot.bytes_done, snapshot.bytes_total),
        human_bytes(rate),
    )
}

fn percent(done: u64, total: u64) -> f64 {
    if total > 0 {
        (done as f64 / total as f64 * 100.0).min(100.0)
    } else {
        0.0
    }
}

/// `42s`, `3m05s`, `1h02m`.
pub fn human_duration(secs: f64) -> String {
    let secs = if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    };
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

impl Progress {
    pub fn new(items_total: usize, bytes_total: u64) -> Self {
        let p = Progress::default();
        p.inner.items_total.store(items_total, Ordering::Relaxed);
        p.inner.bytes_total.store(bytes_total, Ordering::Relaxed);
        p
    }

    /// Set the expected totals once the plan is known (the plan is only
    /// available after `analyze` on the source / after `recv_plan` on the
    /// destination, so totals arrive later than the handle itself).
    pub fn set_totals(&self, items_total: usize, bytes_total: u64) {
        self.inner.items_total.store(items_total, Ordering::Relaxed);
        self.inner.bytes_total.store(bytes_total, Ordering::Relaxed);
    }

    /// Record `n` transferred payload bytes.
    pub fn add_bytes(&self, n: u64) {
        self.inner.bytes_done.fetch_add(n, Ordering::Relaxed);
    }

    /// Record one completed item.
    pub fn item_done(&self) {
        self.inner.items_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Current transferred byte count.
    pub fn bytes(&self) -> u64 {
        self.inner.bytes_done.load(Ordering::Relaxed)
    }

    /// The stage the run is in.
    pub fn stage(&self) -> Stage {
        Stage::from_u8(self.inner.stage.load(Ordering::Relaxed))
    }

    /// Enter `stage` with no countable work. Leaving the transfer settles its
    /// byte total to what actually moved: the plan's estimate (PostgreSQL plans
    /// from on-disk sizes) is only a guess, and every later line would
    /// otherwise show the transfer stuck at the guess's percentage.
    pub fn set_stage(&self, stage: Stage) {
        self.begin_work(stage, "", 0, 0);
    }

    /// Enter `stage`, describing it with `detail` and announcing `steps` units
    /// of work and `bytes` bytes to go through (either may be zero).
    pub fn begin_work(&self, stage: Stage, detail: &str, steps: u64, bytes: u64) {
        if self.stage() == Stage::Transferring && stage != Stage::Transferring {
            self.complete();
        }
        self.inner.work_done.store(0, Ordering::Relaxed);
        self.inner.work_total.store(steps, Ordering::Relaxed);
        self.inner.work_bytes.store(0, Ordering::Relaxed);
        self.inner.work_bytes_total.store(bytes, Ordering::Relaxed);
        self.set_detail(detail);
        self.inner.stage.store(stage as u8, Ordering::Relaxed);
    }

    /// Replace the description of the current stage's work.
    pub fn set_detail(&self, detail: &str) {
        if let Ok(mut current) = self.inner.detail.lock() {
            detail.clone_into(&mut current);
        }
    }

    /// Record one completed unit of the current stage's work.
    pub fn work_step(&self) {
        self.inner.work_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Record `n` bytes the current stage's work went through.
    pub fn work_bytes(&self, n: u64) {
        self.inner.work_bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Replace the *byte* estimate with the observed total. Backends such as
    /// PostgreSQL plan from on-disk relation sizes while transferring a smaller
    /// logical stream, so an acknowledged success must use actuals to render a
    /// truthful 100% terminal snapshot.
    ///
    /// The item total is deliberately NOT rewritten: it comes from the plan and
    /// is exact. Rewriting it made the terminal line read `items 4/4` for a run
    /// that had covered 4 of 10 planned items, i.e. the final snapshot could
    /// never disagree with the plan.
    pub fn complete(&self) {
        let bytes = self.inner.bytes_done.load(Ordering::Relaxed);
        self.inner.bytes_total.store(bytes, Ordering::Relaxed);
    }

    /// Copy every counter for rendering.
    pub fn snapshot(&self) -> Snapshot {
        let inner = &self.inner;
        Snapshot {
            stage: self.stage(),
            items_done: inner.items_done.load(Ordering::Relaxed),
            items_total: inner.items_total.load(Ordering::Relaxed),
            bytes_done: inner.bytes_done.load(Ordering::Relaxed),
            bytes_total: inner.bytes_total.load(Ordering::Relaxed),
            work_done: inner.work_done.load(Ordering::Relaxed),
            work_total: inner.work_total.load(Ordering::Relaxed),
            work_bytes: inner.work_bytes.load(Ordering::Relaxed),
            work_bytes_total: inner.work_bytes_total.load(Ordering::Relaxed),
            detail: inner
                .detail
                .lock()
                .map(|detail| detail.clone())
                .unwrap_or_default(),
            peer: inner.peer.lock().ok().and_then(|peer| peer.clone()),
        }
    }

    /// Record the peer's latest progress line.
    pub fn set_peer_line(&self, line: &str) {
        if let Ok(mut peer) = self.inner.peer.lock() {
            *peer = Some(line.to_string());
        }
    }

    /// The transfer summary — items, bytes, percent and the average rate over
    /// `elapsed_secs` (the caller owns the clock — core never reads time
    /// directly). Used for the terminal line of a run.
    pub fn line(&self, elapsed_secs: f64) -> String {
        let snapshot = self.snapshot();
        let rate = if elapsed_secs > 0.0 {
            (snapshot.bytes_done as f64 / elapsed_secs) as u64
        } else {
            0
        };
        transfer_counts(&snapshot, rate, "")
    }
}

/// Turns successive snapshots into lines: the rate over the interval since the
/// previous line (an average since the start keeps falling through minutes of
/// index builds and read-back, which reads as a stalled transfer) and the time
/// spent in the current stage. The caller supplies the clock, and should call
/// [`Ticker::observe`] more often than it renders so a stage change is dated
/// when it happens rather than at the next line.
pub struct Ticker {
    stage: Stage,
    stage_started: std::time::Instant,
    bytes: u64,
    at: std::time::Instant,
}

impl Ticker {
    pub fn new(snapshot: &Snapshot, now: std::time::Instant) -> Self {
        Ticker {
            stage: snapshot.stage,
            stage_started: now,
            bytes: snapshot.moving_bytes(),
            at: now,
        }
    }

    /// Notice a stage change (between two lines).
    pub fn observe(&mut self, snapshot: &Snapshot, now: std::time::Instant) {
        if snapshot.stage != self.stage {
            // The rate restarts with the stage: its window and byte baseline
            // are the stage's own (the verify stage counts different bytes).
            self.stage = snapshot.stage;
            self.stage_started = now;
            self.bytes = snapshot.moving_bytes();
            self.at = now;
        }
    }

    /// Render `snapshot` against the previous line and remember it.
    pub fn line(&mut self, snapshot: &Snapshot, now: std::time::Instant) -> String {
        self.observe(snapshot, now);
        let window = now.duration_since(self.at).as_secs_f64();
        let moved = snapshot.moving_bytes().saturating_sub(self.bytes);
        let rate = if window > 0.0 {
            (moved as f64 / window) as u64
        } else {
            0
        };
        self.bytes = snapshot.moving_bytes();
        self.at = now;
        snapshot.render(rate, now.duration_since(self.stage_started).as_secs_f64())
    }
}

tokio::task_local! {
    static CURRENT: Progress;
}

/// Run `future` with `progress` as the run's handle, reachable through
/// [`current`] from any code the future awaits on the same task.
pub async fn scope<F: std::future::Future>(progress: Progress, future: F) -> F::Output {
    CURRENT.scope(progress, future).await
}

/// The handle of the run this task is executing, if it runs inside [`scope`].
/// Module code reports stage work through it; outside a session (a unit test
/// calling a module directly) there is none and reporting is skipped.
pub fn current() -> Option<Progress> {
    CURRENT.try_with(Progress::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte estimate is replaced by the actual; the ITEM total stays at the
    /// plan value, so a run that covered part of the plan cannot render as a
    /// complete one.
    #[test]
    fn completion_replaces_the_byte_estimate_but_not_the_plan_item_count() {
        let short = Progress::new(4, 50_000);
        short.add_bytes(1000);
        short.item_done();
        short.complete();
        let line = short.line(1.0);
        assert!(line.contains("items 1/4"), "{line}");
        assert!(line.contains("1000 B/1000 B"), "{line}");
        assert!(line.contains("(100.0%)"), "{line}");

        let full = Progress::new(2, 50_000);
        full.add_bytes(4096);
        full.item_done();
        full.item_done();
        full.complete();
        let line = full.line(1.0);
        assert!(line.contains("items 2/2"), "{line}");
    }

    /// A PostgreSQL run streamed 1.03 GiB against a 1.41 GiB estimate and then
    /// spent minutes on indexes and the read-back with its line frozen at
    /// `(73.2%)`. Leaving the transfer settles the total; the later stages say
    /// what they are doing and how far they got.
    #[test]
    fn stages_after_the_transfer_report_their_own_work() {
        let progress = Progress::new(2, 10_000);
        progress.set_stage(Stage::Transferring);
        progress.add_bytes(7000);
        progress.item_done();
        progress.item_done();
        let transfer = progress.snapshot().render(700, 12.0);
        assert!(transfer.starts_with("transfer: items 2/2"), "{transfer}");
        assert!(transfer.contains("(70.0%)"), "{transfer}");
        assert!(transfer.contains("6.84 KiB/~9.77 KiB"), "{transfer}");
        assert!(transfer.contains("every item transferred"), "{transfer}");

        progress.begin_work(Stage::Finalizing, "indexes and constraints", 4, 0);
        progress.work_step();
        let finalize = progress.snapshot().render(0, 95.0);
        assert!(
            finalize.starts_with("finalize: indexes and constraints, step 1/4 (25.0%)"),
            "{finalize}"
        );
        assert!(finalize.ends_with("1m35s in this stage"), "{finalize}");
        assert!(
            progress.line(1.0).contains("(100.0%)"),
            "the transfer total settled"
        );

        progress.begin_work(Stage::Verifying, "", 2, 7000);
        progress.work_bytes(3500);
        progress.work_step();
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.moving_bytes(), 3500);
        let verify = snapshot.render(3500, 1.0);
        assert!(
            verify.starts_with("verify: reading the restored data back, items 1/2"),
            "{verify}"
        );
        assert!(verify.contains("(50.0%)"), "{verify}");
    }

    /// The waiting source shows the destination's own line once it has one.
    #[test]
    fn a_waiting_source_shows_the_destination_line() {
        let progress = Progress::new(1, 100);
        progress.set_stage(Stage::Transferring);
        progress.add_bytes(100);
        progress.item_done();
        progress.set_stage(Stage::AwaitingPeer);
        let before = progress.snapshot().render(0, 3.0);
        assert!(before.contains("the destination is applying"), "{before}");
        progress.set_peer_line("finalize: building indexes, step 3/9 (33.3%), 2s in this stage");
        let after = progress.snapshot().render(0, 4.0);
        assert!(
            after.ends_with("4s in this stage; destination: finalize: building indexes, step 3/9 (33.3%), 2s in this stage"),
            "{after}"
        );
    }

    #[test]
    fn the_ticker_dates_a_stage_and_measures_the_recent_rate() {
        let start = std::time::Instant::now();
        let progress = Progress::new(1, 1000);
        progress.set_stage(Stage::Transferring);
        let mut ticker = Ticker::new(&progress.snapshot(), start);
        progress.add_bytes(500);
        let first = ticker.line(
            &progress.snapshot(),
            start + std::time::Duration::from_secs(5),
        );
        assert!(first.contains("100 B/s"), "{first}");
        progress.begin_work(Stage::Verifying, "", 1, 500);
        ticker.observe(
            &progress.snapshot(),
            start + std::time::Duration::from_secs(6),
        );
        progress.work_bytes(200);
        let verify = ticker.line(
            &progress.snapshot(),
            start + std::time::Duration::from_secs(10),
        );
        assert!(verify.contains("50 B/s"), "{verify}");
        assert!(verify.ends_with("4s in this stage"), "{verify}");
    }

    #[test]
    fn durations_render_compactly() {
        assert_eq!(human_duration(0.4), "0s");
        assert_eq!(human_duration(59.9), "59s");
        assert_eq!(human_duration(185.0), "3m05s");
        assert_eq!(human_duration(3720.0), "1h02m");
    }

    #[tokio::test]
    async fn a_scoped_handle_is_reachable_and_absent_outside() {
        assert!(current().is_none());
        let progress = Progress::default();
        scope(progress.clone(), async {
            if let Some(handle) = current() {
                handle.set_stage(Stage::Verifying);
            }
        })
        .await;
        assert_eq!(progress.stage(), Stage::Verifying);
    }
}
