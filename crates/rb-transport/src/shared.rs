//! Shared utilities: TCP tuning, proxy buffer sizing, UDP direct-path tuning,
//! UDP hole-punch candidate sanitation.

use serde::{Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::{trace, warn};

pub const DEFAULT_PROXY_BUFFER_SIZE: usize = 256 * 1024;
const MIN_PROXY_BUFFER_SIZE: usize = 4 * 1024;
const MAX_PROXY_BUFFER_SIZE: usize = 16 * 1024 * 1024;

const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(15);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

pub fn proxy_buffer_size() -> usize {
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| match std::env::var("BORE_PROXY_BUFFER_SIZE") {
        Ok(raw) => match parse_size_bytes(&raw) {
            Some(bytes) => {
                let resolved = (bytes as usize).clamp(MIN_PROXY_BUFFER_SIZE, MAX_PROXY_BUFFER_SIZE);
                trace!(
                    requested = bytes,
                    resolved,
                    "proxy buffer size set via BORE_PROXY_BUFFER_SIZE"
                );
                resolved
            }
            None => {
                trace!(
                    value = %raw,
                    "ignoring unparseable BORE_PROXY_BUFFER_SIZE; using default"
                );
                DEFAULT_PROXY_BUFFER_SIZE
            }
        },
        Err(_) => DEFAULT_PROXY_BUFFER_SIZE,
    })
}

pub fn parse_size_bytes(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let split_at = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    let bytes: u64 = number.parse().ok()?;
    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "ki" | "kib" => 1024,
        "mi" | "mib" => 1024 * 1024,
        "gi" | "gib" => 1024 * 1024 * 1024,
        _ => return None,
    };
    bytes.checked_mul(multiplier)
}

pub fn tune_tcp(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_TIME)
        .with_interval(TCP_KEEPALIVE_INTERVAL);
    let _ = SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

// === UDP hole-punch candidate sanitation (ported from bore holepunch.rs) ===

/// Upper bound on hole-punch candidates accepted from a peer, offered on the
/// wire, or punched/dialed in one traversal round. Every peer-controlled list is
/// clamped BEFORE any per-candidate task fan-out, so a hostile peer cannot turn
/// the puncher into a port scanner.
pub const MAX_UDP_CANDIDATES: usize = 16;

/// Aggregate drop counters from [`sanitize_candidates`]. Logged as ONE line per
/// round (never one warning per stray candidate).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CandidateSanitation {
    /// Structurally unusable (port 0, unspecified, multicast, broadcast).
    pub dropped_invalid: usize,
    /// Exact duplicate of an earlier entry (order-preserving dedup).
    pub dropped_duplicate: usize,
    /// Entries past the [`MAX_UDP_CANDIDATES`] cap.
    pub dropped_overflow: usize,
}

impl CandidateSanitation {
    /// Total dropped entries.
    pub fn dropped(&self) -> usize {
        self.dropped_invalid + self.dropped_duplicate + self.dropped_overflow
    }
}

/// Whether `addr` may be offered or punched as a hole-punch candidate.
///
/// Private/CGNAT addresses are VALID — same-LAN peers need them, and the
/// accepted QUIC source is authenticated by token, never by candidate list.
/// Only addresses unusable by construction are rejected: port 0, unspecified,
/// multicast, IPv4 broadcast.
pub fn valid_candidate(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast(),
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// Validate, dedup (order-preserving) and cap a candidate list in place.
/// Shared by the offering side, the coordination-server broker and the
/// punch/dial entry points (defense in depth) — one implementation so no path
/// drifts.
pub fn sanitize_candidates(candidates: &mut Vec<SocketAddr>) -> CandidateSanitation {
    let mut san = CandidateSanitation::default();
    let mut seen: Vec<SocketAddr> = Vec::with_capacity(MAX_UDP_CANDIDATES);
    candidates.retain(|addr| {
        if !valid_candidate(addr) {
            san.dropped_invalid += 1;
            return false;
        }
        if seen.contains(addr) {
            san.dropped_duplicate += 1;
            return false;
        }
        if seen.len() >= MAX_UDP_CANDIDATES {
            san.dropped_overflow += 1;
            return false;
        }
        seen.push(*addr);
        true
    });
    san
}

/// Sanitize a candidate list and emit one aggregate `warn!` when anything was
/// dropped (a non-zero count means a peer sent something out of contract, or a
/// local gather bug).
pub fn sanitize_and_log(context: &'static str, candidates: &mut Vec<SocketAddr>) {
    let san = sanitize_candidates(candidates);
    if san.dropped() == 0 {
        return;
    }
    warn!(
        context,
        kept = candidates.len(),
        invalid = san.dropped_invalid,
        duplicate = san.dropped_duplicate,
        overflow = san.dropped_overflow,
        "dropped udp candidates"
    );
}

// === UDP Direct Path (Phase 1) ===

/// Per-stream QUIC receive window: 16 MiB.
pub const DIRECT_QUIC_STREAM_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;

/// Total QUIC receive window for one connection: 16 MiB.
pub const DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;

/// Bytes sent but not yet acknowledged before the sender blocks: 16 MiB.
pub const DIRECT_QUIC_SEND_WINDOW: u64 = 16 * 1024 * 1024_u64;

/// Requested UDP receive buffer: 16 MiB (will be clamped by kernel, SO_RCVBUF/_FORCE).
pub const DIRECT_UDP_SOCKET_RECV_BUFFER: usize = 16 * 1024 * 1024;

/// Requested UDP send buffer: 16 MiB (will be clamped by kernel, SO_SNDBUF/_FORCE).
pub const DIRECT_UDP_SOCKET_SEND_BUFFER: usize = 16 * 1024 * 1024;

/// Maximum concurrent QUIC bidi streams on a direct connection.
pub const MAX_DIRECT_STREAMS: u32 = 100;

/// Bandwidth-oriented tuning for the direct UDP path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpDirectTuning {
    /// Per-stream QUIC receive window.
    pub stream_receive_window: u32,
    /// Total QUIC receive window for one direct connection.
    pub connection_receive_window: u32,
    /// Bytes sent but not yet acknowledged before the sender blocks.
    pub send_window: u64,
    /// Requested UDP receive buffer.
    pub udp_socket_recv_buffer: usize,
    /// Requested UDP send buffer.
    pub udp_socket_send_buffer: usize,
    /// Max concurrent QUIC bidi streams on the direct connection.
    pub max_direct_streams: u32,
}

impl Default for UdpDirectTuning {
    fn default() -> Self {
        Self {
            stream_receive_window: DIRECT_QUIC_STREAM_RECEIVE_WINDOW,
            connection_receive_window: DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW,
            send_window: DIRECT_QUIC_SEND_WINDOW,
            udp_socket_recv_buffer: DIRECT_UDP_SOCKET_RECV_BUFFER,
            udp_socket_send_buffer: DIRECT_UDP_SOCKET_SEND_BUFFER,
            max_direct_streams: MAX_DIRECT_STREAMS,
        }
    }
}

#[cfg(test)]
mod candidate_tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("test address literal")
    }

    /// Only structurally unusable candidates are rejected; private/LAN stay.
    #[test]
    fn valid_candidate_keeps_private_rejects_unusable() {
        assert!(valid_candidate(&addr("192.168.1.10:5000")));
        assert!(valid_candidate(&addr("100.64.0.1:5000"))); // CGNAT
        assert!(valid_candidate(&addr("[fd00::1]:5000")));
        assert!(!valid_candidate(&addr("10.0.0.1:0"))); // port 0
        assert!(!valid_candidate(&addr("0.0.0.0:5000"))); // unspecified
        assert!(!valid_candidate(&addr("224.0.0.1:5000"))); // multicast
        assert!(!valid_candidate(&addr("255.255.255.255:5000"))); // broadcast
        assert!(!valid_candidate(&addr("[::]:5000")));
    }

    /// Invalid entries drop, duplicates dedup order-preserving, order kept.
    #[test]
    fn sanitize_drops_invalid_and_duplicates() {
        let mut c = vec![
            addr("192.168.1.10:5000"),
            addr("0.0.0.0:5000"),
            addr("192.168.1.10:5000"),
            addr("10.0.0.5:6000"),
            addr("10.0.0.5:0"),
        ];
        let san = sanitize_candidates(&mut c);
        assert_eq!(c, vec![addr("192.168.1.10:5000"), addr("10.0.0.5:6000")]);
        assert_eq!(san.dropped_invalid, 2);
        assert_eq!(san.dropped_duplicate, 1);
        assert_eq!(san.dropped_overflow, 0);
        assert_eq!(san.dropped(), 3);
    }

    /// A hostile peer cannot fan out more than MAX_UDP_CANDIDATES dials.
    #[test]
    fn sanitize_caps_hostile_list() {
        let mut c: Vec<SocketAddr> = (1..=100u16)
            .map(|i| addr(&format!("10.0.0.1:{}", 1000 + i)))
            .collect();
        let san = sanitize_candidates(&mut c);
        assert_eq!(c.len(), MAX_UDP_CANDIDATES);
        assert_eq!(san.dropped_overflow, 100 - MAX_UDP_CANDIDATES);
    }
}
