//! Process-local learned direct-peer addresses. Advisory only: a cache entry
//! changes probe order, never candidate membership or relay fallback.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(120);

fn cache() -> &'static Mutex<HashMap<String, (SocketAddr, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (SocketAddr, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn remember(key: &str, address: SocketAddr) {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.to_owned(), (address, Instant::now()));
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

    #[test]
    fn remember_recall_and_invalidate() {
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
