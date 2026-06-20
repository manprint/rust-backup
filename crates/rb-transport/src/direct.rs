//! Direct UDP/QUIC path — Phase 1 stub.
//!
//! This module is gated behind the `udp` feature and is currently a documented
//! stub. The relay path (TCP + yamux) is the working transport; the direct QUIC
//! path will be ported from bore's holepunch.rs in a future phase.
//!
//! When implemented, this will expose:
//! - `DirectConn`: QUIC connection handle for opening native QUIC streams
//! - Hole-punching candidate gathering and negotiation
//! - Fallback to relay on UDP failure
//!
//! For now, the crate compiles without this module active, and clients always
//! use the relay path, which is fully functional and tested.

#![allow(dead_code)]

/// Phase 1: DirectConn and associated types will be ported here from bore/src/holepunch.rs
///
/// Expected signature:
/// ```ignore
/// pub struct DirectConn { /* QUIC connection state */ }
/// impl DirectConn {
///     pub async fn open_stream(&self) -> io::Result<impl AsyncRead + AsyncWrite>;
/// }
/// ```
pub fn todo_direct_quic_setup() {
    todo!("Phase 1: port holepunch.rs DirectConn/DirectListener")
}
