//! Cooperative operator interrupt (`SIGINT`/`SIGTERM`).
//!
//! Dropping a restore mid-flight skips the module's own failure path, which is
//! the only code that knows what the run created. So the binary *requests* an
//! interrupt here instead: a destination that is applying a payload sees its
//! chunk stream fail, unwinds through its normal error path and removes what it
//! created, while the binary waits (bounded) for [`restores_settled`].

use std::sync::OnceLock;

use tokio::sync::watch;

use crate::error::{BackupError, Phase, Result};

struct State {
    signal: watch::Sender<Option<&'static str>>,
    in_flight: watch::Sender<usize>,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| State {
        signal: watch::channel(None).0,
        in_flight: watch::channel(0).0,
    })
}

/// Record that the operator interrupted the process. Idempotent; the first
/// signal name wins.
pub fn request(signal: &'static str) {
    state().signal.send_if_modified(|current| {
        if current.is_some() {
            return false;
        }
        *current = Some(signal);
        true
    });
}

/// The signal that interrupted the process, if any.
pub fn requested() -> Option<&'static str> {
    *state().signal.borrow()
}

/// Resolve once an interrupt has been requested.
pub async fn wait() -> &'static str {
    let mut rx = state().signal.subscribe();
    loop {
        if let Some(signal) = *rx.borrow_and_update() {
            return signal;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The error an interrupted run fails with.
pub fn error(signal: &str) -> BackupError {
    BackupError::phase(
        Phase::Apply,
        format!("interrupted by {signal}: the run was aborted before completion"),
    )
}

/// `Err` once an interrupt has been requested.
pub fn check() -> Result<()> {
    match requested() {
        Some(signal) => Err(error(signal)),
        None => Ok(()),
    }
}

/// Held by a destination from the moment it starts applying a payload until it
/// has finished undoing or certifying it.
#[derive(Debug)]
pub struct RestoreInFlight(());

impl RestoreInFlight {
    pub fn begin() -> Self {
        state().in_flight.send_modify(|count| *count += 1);
        Self(())
    }
}

impl Drop for RestoreInFlight {
    fn drop(&mut self) {
        state()
            .in_flight
            .send_modify(|count| *count = count.saturating_sub(1));
    }
}

/// Number of destinations currently holding a [`RestoreInFlight`].
pub fn restores_in_flight() -> usize {
    *state().in_flight.borrow()
}

/// Resolve once no destination is applying or undoing a payload.
pub async fn restores_settled() {
    let mut rx = state().in_flight.subscribe();
    while *rx.borrow_and_update() != 0 {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Forget a requested interrupt. Test support only: a process that was
/// interrupted exits.
#[doc(hidden)]
pub fn clear() {
    state().signal.send_replace(None);
}
