//! Session status board for `run --config` with parallel targets.
//!
//! Each target already logs its own progress line every few seconds; with three
//! targets interleaved that does not answer the one question the operator of a
//! source host has before starting the destination: *is every plan ready?* The
//! board answers it twice — once, the moment every source target has its plan
//! ready (or has failed before getting there), and as a per-target status block
//! every [`STATUS_INTERVAL`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rb_core::config::{Role, TargetSpec};
use rb_core::progress::{Progress, Stage};

/// How often the per-target status block is logged.
pub const STATUS_INTERVAL: Duration = Duration::from_secs(60);
/// How often the board looks for the moment every source plan is ready.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Where a scheduled target is in the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetState {
    /// Waiting for a free `parallel_targets` slot.
    Queued,
    Running,
    Verified,
    Failed,
}

struct Entry {
    label: String,
    role: Role,
    progress: Progress,
    state: Mutex<TargetState>,
}

/// Shared view of every target of one session.
#[derive(Clone)]
pub struct SessionBoard {
    entries: Arc<Vec<Entry>>,
    parallel_targets: usize,
}

impl SessionBoard {
    pub fn new(targets: &[TargetSpec], parallel_targets: usize) -> Self {
        let entries = targets
            .iter()
            .enumerate()
            .map(|(i, target)| Entry {
                label: format!(
                    "target {i} {}/{:?} channel={}",
                    target.module, target.role, target.transport.channel
                ),
                role: target.role,
                progress: Progress::default(),
                state: Mutex::new(TargetState::Queued),
            })
            .collect();
        Self {
            entries: Arc::new(entries),
            parallel_targets,
        }
    }

    /// The progress handle target `index` must report into.
    pub fn progress(&self, index: usize) -> Progress {
        self.entries
            .get(index)
            .map(|entry| entry.progress.clone())
            .unwrap_or_default()
    }

    pub fn set_state(&self, index: usize, state: TargetState) {
        if let Some(entry) = self.entries.get(index) {
            if let Ok(mut current) = entry.state.lock() {
                *current = state;
            }
        }
    }

    fn state(entry: &Entry) -> TargetState {
        entry
            .state
            .lock()
            .map(|state| *state)
            .unwrap_or(TargetState::Failed)
    }

    fn sources(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.role == Role::Source)
    }

    /// Source targets that run a plan, and how many of them can be ready at the
    /// same moment. Fewer slots than sources means the plans come one batch at
    /// a time, and the operator has to know that up front.
    pub fn slot_warning(&self) -> Option<String> {
        let sources = self.sources().count();
        (sources > self.parallel_targets).then(|| {
            format!(
                "parallel_targets={} runs fewer targets at once than the {sources} source \
                 targets: their plans are never all ready at the same time — a queued target \
                 starts only after a running one finishes",
                self.parallel_targets
            )
        })
    }

    /// Once every source target has its plan ready or has already finished,
    /// the line announcing it. `None` while a source is still preparing (or
    /// when the session has no source target).
    pub fn plans_settled(&self) -> Option<String> {
        let mut total = 0;
        let mut ready = 0;
        let mut failed = 0;
        for entry in self.sources() {
            total += 1;
            let state = Self::state(entry);
            if entry.progress.plan_ready() {
                ready += 1;
            } else if state == TargetState::Failed {
                failed += 1;
            } else {
                return None;
            }
        }
        if total == 0 {
            return None;
        }
        Some(if failed == 0 {
            format!("ALL SOURCE PLANS READY ({ready}/{total}): start the destination now")
        } else {
            format!(
                "SOURCE PLANS SETTLED: {ready}/{total} ready, {failed} failed before its plan \
                 (see its error); the destination can start for the ready ones"
            )
        })
    }

    /// One status block: a headline with the ready-plan count, then one line
    /// per target.
    pub fn status(&self) -> String {
        let total = self.sources().count();
        let ready = self
            .sources()
            .filter(|entry| entry.progress.plan_ready())
            .count();
        let mut out = if total > 0 {
            format!("session status: source plans ready {ready}/{total}")
        } else {
            "session status".to_string()
        };
        for entry in self.entries.iter() {
            out.push_str("\n  ");
            out.push_str(&entry.label);
            out.push_str(": ");
            out.push_str(&self.describe(entry));
        }
        out
    }

    fn describe(&self, entry: &Entry) -> String {
        let stage = entry.progress.stage();
        match Self::state(entry) {
            TargetState::Queued => format!(
                "queued, starts when one of the {} parallel slots frees",
                self.parallel_targets
            ),
            TargetState::Verified => "done (verified)".to_string(),
            TargetState::Failed => "FAILED (see its error)".to_string(),
            TargetState::Running => match entry.role {
                Role::Source if !entry.progress.plan_ready() => {
                    format!("preparing the plan ({})", stage.label())
                }
                Role::Source if stage == Stage::Handshake => {
                    "PLAN READY, waiting for the destination".to_string()
                }
                _ => format!("running ({})", stage.label()),
            },
        }
    }

    /// Log the settled-plans line once and the status block every
    /// [`STATUS_INTERVAL`] until the returned task is aborted.
    pub fn spawn_monitor(&self) -> tokio::task::JoinHandle<()> {
        let board = self.clone();
        tokio::spawn(async move {
            let mut announced = false;
            let mut poll = tokio::time::interval(POLL_INTERVAL);
            let mut status = tokio::time::interval(STATUS_INTERVAL);
            // The first tick of an interval is immediate; the block is for
            // later, when there is something to report.
            status.tick().await;
            loop {
                tokio::select! {
                    _ = poll.tick() => {
                        if !announced {
                            if let Some(line) = board.plans_settled() {
                                announced = true;
                                tracing::info!("{line}");
                                tracing::info!("{}", board.status());
                            }
                        }
                    }
                    _ = status.tick() => tracing::info!("{}", board.status()),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(module: &str, role: Role, channel: &str) -> TargetSpec {
        serde_json::from_value(serde_json::json!({
            "module": module,
            "role": role,
            "transport": { "to": "127.0.0.1:7835", "channel": channel },
        }))
        .expect("target spec")
    }

    fn three_sources(parallel: usize) -> SessionBoard {
        SessionBoard::new(
            &[
                spec("postgres", Role::Source, "fiera"),
                spec("mongodb", Role::Source, "mongo-fiera"),
                spec("filesystem", Role::Source, "fiera-fs"),
            ],
            parallel,
        )
    }

    #[test]
    fn plans_are_announced_only_when_every_source_has_one() {
        let board = three_sources(3);
        for index in 0..3 {
            board.set_state(index, TargetState::Running);
        }
        board.progress(0).set_stage(Stage::Handshake);
        board.progress(1).set_stage(Stage::Handshake);
        board.progress(2).set_stage(Stage::Analyzing);
        assert_eq!(board.plans_settled(), None);
        let status = board.status();
        assert!(status.contains("source plans ready 2/3"), "{status}");
        assert!(
            status.contains(
                "target 0 postgres/Source channel=fiera: PLAN READY, waiting for the destination"
            ),
            "{status}"
        );
        assert!(
            status.contains(
                "target 2 filesystem/Source channel=fiera-fs: preparing the plan (analyze)"
            ),
            "{status}"
        );

        board.progress(2).set_stage(Stage::Handshake);
        assert_eq!(
            board.plans_settled().as_deref(),
            Some("ALL SOURCE PLANS READY (3/3): start the destination now")
        );
    }

    #[test]
    fn a_source_that_failed_before_its_plan_settles_the_count() {
        let board = three_sources(3);
        board.progress(0).set_stage(Stage::Handshake);
        board.progress(1).set_stage(Stage::Handshake);
        board.set_state(2, TargetState::Failed);
        let line = board.plans_settled().expect("settled");
        assert!(line.contains("2/3 ready, 1 failed"), "{line}");
    }

    #[test]
    fn a_plan_that_moved_on_still_counts_as_ready() {
        let board = three_sources(3);
        for index in 0..3 {
            board.set_state(index, TargetState::Running);
            board.progress(index).set_stage(Stage::Handshake);
        }
        board.progress(0).set_stage(Stage::Transferring);
        board.set_state(1, TargetState::Verified);
        assert!(board.plans_settled().is_some());
        let status = board.status();
        assert!(status.contains("running (transfer)"), "{status}");
        assert!(status.contains("done (verified)"), "{status}");
    }

    #[test]
    fn fewer_slots_than_sources_is_called_out() {
        let board = three_sources(2);
        let warning = board.slot_warning().expect("warning");
        assert!(warning.contains("parallel_targets=2"), "{warning}");
        assert!(three_sources(3).slot_warning().is_none());
        assert!(board
            .status()
            .contains("queued, starts when one of the 2 parallel slots frees"));
    }
}
