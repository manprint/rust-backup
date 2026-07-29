//! Carrier pool for round-robin substream opening.
//! Vendored from bore with minimal adaptation.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::mux;

pub struct Carrier {
    pub opener: mux::Opener,
    pub alive: Arc<AtomicBool>,
}

impl Carrier {
    pub fn new(opener: mux::Opener) -> Self {
        Self {
            opener,
            alive: Arc::new(AtomicBool::new(true)),
        }
    }
}

pub type PendingCarriers = Arc<DashMap<String, mpsc::UnboundedSender<Carrier>>>;

pub struct TokenGuard {
    registry: PendingCarriers,
    token: String,
}

impl TokenGuard {
    pub fn new(registry: PendingCarriers, token: String) -> Self {
        Self { registry, token }
    }
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.token);
    }
}

pub struct CarrierPool {
    carriers: Mutex<Vec<Carrier>>,
    next: AtomicUsize,
}

impl CarrierPool {
    pub fn new(first: mux::Opener) -> Self {
        Self {
            carriers: Mutex::new(vec![Carrier::new(first)]),
            next: AtomicUsize::new(0),
        }
    }

    pub fn pick(&self) -> Option<mux::Opener> {
        // A panic while holding this short-lived bookkeeping lock must not take
        // down a backup session; its vector remains valid after unwinding.
        let mut carriers = self
            .carriers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        carriers.retain(|c| c.alive.load(Ordering::Relaxed));
        if carriers.is_empty() {
            return None;
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % carriers.len();
        Some(carriers[idx].opener.clone())
    }
}
