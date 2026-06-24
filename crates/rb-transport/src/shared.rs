//! Shared utilities: TCP tuning, proxy buffer sizing, UDP direct-path tuning.

use serde::{Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::trace;

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
