#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
//! rb-transport — coordination server + paired byte-channel for rust-backup.
//!
//! Provides a TCP relay with yamux multiplexing vendored from bore, plus a
//! minimal control protocol. The direct UDP/QUIC path is documented in `direct.rs`
//! and deferred to Phase 1.

pub mod adaptive_nat;
pub mod auth;
pub mod channel;
pub mod client;
pub mod connectivity;
pub mod mux;
pub mod pair_cache;
pub mod pool;
pub mod prefixed;
pub mod proto;
pub mod server;
pub mod shared;
pub mod transport;

#[cfg(feature = "udp")]
pub mod direct;

pub use channel::PairedChannel;
pub use client::{connect_destination, connect_source};
pub use server::run_server;
pub use transport::CONTROL_PORT;
