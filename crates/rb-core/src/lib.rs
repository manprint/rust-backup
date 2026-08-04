#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! # rb-core
//!
//! Core abstractions for **rust-backup**: a modular, streaming, source→destination
//! backup/restore tool whose data path is the `bore` tunnel transport (relay +
//! direct QUIC).
//!
//! ## Invariants enforced by this crate's shape
//! - **Source immutability** — [`module::Source`] is read-only; the session
//!   audits [`module::Source::fingerprint`] before/after every run.
//! - **No temp files** — data flows backend→channel→backend in chunks
//!   ([`channel::ChunkSink`] / [`channel::ChunkSource`]); nothing stages to disk.
//! - **Explicit errors** — every fallible API returns [`error::Result`] with a
//!   phase-tagged [`error::BackupError`].
//! - **Modularity** — new backends implement [`module::BackupModule`] in their
//!   own crate and register with [`module::ModuleRegistry`].
//! - **Backpressure** — chunk writes ride consumer-paced substream flow control
//!   (see [`channel`]), balancing the producer/consumer bandwidth gap.

pub mod channel;
pub mod config;
pub mod error;
pub mod module;
pub mod plan;
pub mod progress;
pub mod session;
pub mod verification;
pub mod wire;

pub use error::{BackupError, Phase, Result};
pub use module::{BackupModule, Destination, ModuleRegistry, Source, TargetParams};
pub use plan::{BackupMode, BackupPlan, PlanItem, Preflight};
