//! Explicit, phase-tagged error model.
//!
//! INVARIANT (error handling): every fallible step returns [`Result`]; no
//! `unwrap`/`expect` in production code paths. Errors carry the [`Phase`] they
//! occurred in so logs and the source-immutability audit can attribute faults.

use thiserror::Error;

/// Execution phase a failure occurred in. Used for log attribution and to prove
/// the source-immutability invariant (a fault in `Analyze`/`Transfer` must never
/// have mutated the source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Establishing transport channel or connecting to a backend.
    Connect,
    /// Source inspecting the backend to build the plan (read-only).
    Analyze,
    /// Destination preflight checks against the received plan.
    Validate,
    /// Bytes streaming over the channel.
    Transfer,
    /// Destination applying the streamed payload (restore).
    Apply,
    /// Post-transfer integrity verification.
    Verify,
    /// Cleanup / channel teardown.
    Teardown,
}

/// The crate-wide error type. All public fallible APIs return [`Result`].
#[derive(Debug, Error)]
pub enum BackupError {
    /// A failure attributed to a specific execution [`Phase`].
    #[error("[{phase:?}] {message}")]
    Phase {
        phase: Phase,
        message: String,
        #[source]
        source: Option<anyhow::Error>,
    },

    /// The source-immutability invariant was violated (fingerprint changed).
    /// This is the most severe error class — it must never happen.
    #[error("SOURCE-IMMUTABILITY VIOLATION: {0}")]
    SourceMutated(String),

    /// Destination refused the plan (async-accept said no, or version mismatch).
    #[error("plan rejected: {0}")]
    PlanRejected(String),

    /// A preflight check failed (disk space, accessibility, version compat).
    #[error("preflight failed: {0}")]
    Preflight(String),

    /// Integrity verification failed (BLAKE3 mismatch).
    #[error("integrity check failed: {0}")]
    Integrity(String),

    /// Configuration error (CLI/env/yaml).
    #[error("configuration error: {0}")]
    Config(String),

    /// Catch-all for wrapped lower-level errors.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl BackupError {
    /// Build a phase-tagged error from a message.
    pub fn phase(phase: Phase, message: impl Into<String>) -> Self {
        BackupError::Phase {
            phase,
            message: message.into(),
            source: None,
        }
    }

    /// Build a phase-tagged error wrapping a lower-level cause.
    pub fn phase_src(
        phase: Phase,
        message: impl Into<String>,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        BackupError::Phase {
            phase,
            message: message.into(),
            source: Some(source.into()),
        }
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, BackupError>;
