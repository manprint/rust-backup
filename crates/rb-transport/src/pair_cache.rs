//! Process-local learned direct-peer addresses. Advisory only: a cache entry
//! changes probe order, never candidate membership or relay fallback.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(120);
/// A cap makes the documented bound real. Entries only ever expire when the key
/// they belong to is recalled, so a process that opens many channels and never
/// revisits them would otherwise keep one entry per channel for its whole life.
const MAX_ENTRIES: usize = 128;

fn cache() -> &'static Mutex<HashMap<String, (SocketAddr, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (SocketAddr, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn remember(key: &str, address: SocketAddr) {
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|_, (_, created)| created.elapsed() < TTL);
    if guard.len() >= MAX_ENTRIES && !guard.contains_key(key) {
        // Evict the least recently learned entry. The cache is advisory — it
        // only reorders probes — so dropping one costs a probe, never a
        // connection.
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, (_, created))| *created)
            .map(|(key, _)| key.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(key.to_owned(), (address, Instant::now()));
}

pub fn recall(key: &str) -> Option<SocketAddr> {
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match guard.get(key) {
        Some((address, created)) if created.elapsed() < TTL => Some(*address),
        Some(_) => {
            guard.remove(key);
            None
        }
        None => None,
    }
}

pub fn invalidate(key: &str) {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(key);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is process-global, and the cap test deliberately evicts
    /// entries, so these two tests cannot run at the same time: without this
    /// lock the filling test could evict the other test's key between its
    /// `remember` and its `recall`.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn the_cache_never_grows_past_its_cap() {
        let _serialized = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let address: SocketAddr = "127.0.0.1:45679".parse().unwrap();
        for index in 0..(MAX_ENTRIES * 2) {
            remember(&format!("cap-probe-{index}"), address);
        }
        let len = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert!(len <= MAX_ENTRIES, "cache grew to {len} entries");
        for index in 0..(MAX_ENTRIES * 2) {
            invalidate(&format!("cap-probe-{index}"));
        }
    }

    #[test]
    fn remember_recall_and_invalidate() {
        let _serialized = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = "fault-bank-pair-cache";
        let address: SocketAddr = "127.0.0.1:45678".parse().unwrap();
        invalidate(key);
        assert_eq!(recall(key), None);
        remember(key, address);
        assert_eq!(recall(key), Some(address));
        invalidate(key);
        assert_eq!(recall(key), None);
    }
}
