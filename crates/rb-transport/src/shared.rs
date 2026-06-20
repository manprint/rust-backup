//! Shared utilities: TCP tuning, proxy buffer sizing.

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
