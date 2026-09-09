//! Carrier pool for round-robin substream opening.
//! Vendored from bore with minimal adaptation.
//!
//! A provider registers once and every data carrier is a substream on that one
//! mux, so a pool here holds exactly one live carrier today. bore's
//! registration channel for *additional* provider connections
//! (`PendingCarriers`/`TokenGuard`) was carried over unused — allocated per
//! server, threaded through three handlers and then ignored — and is gone: an
//! unused mechanism reads as a supported one.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

pub struct CarrierPool {
    carriers: Mutex<Vec<Carrier>>,
    next: AtomicUsize,
    consumer_claimed: AtomicBool,
}

/// Holds the single consumer slot of a channel for as long as that destination's
/// control connection lives. Released on drop so a retried destination can take
/// the channel over once the first one is gone.
pub struct ConsumerClaim {
    pool: Arc<CarrierPool>,
}

impl Drop for ConsumerClaim {
    fn drop(&mut self) {
        self.pool.consumer_claimed.store(false, Ordering::Release);
    }
}

impl CarrierPool {
    pub fn new(first: mux::Opener) -> Self {
        Self {
            carriers: Mutex::new(vec![Carrier::new(first)]),
            next: AtomicUsize::new(0),
            consumer_claimed: AtomicBool::new(false),
        }
    }

    /// Claim this channel's one consumer slot, or `None` if a destination
    /// already holds it.
    ///
    /// A channel pairs exactly one source with one destination. A second
    /// destination on the same channel id would have its relayed substreams
    /// interleaved with the first one's on the *same* provider, so the source
    /// would mix two plan exchanges and two sets of data carriers into one
    /// session. Refusing here turns that into an explicit error on the second
    /// destination instead of a corrupted or wedged run on the first.
    pub fn claim_consumer(self: &Arc<Self>) -> Option<ConsumerClaim> {
        self.consumer_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ConsumerClaim {
                pool: Arc::clone(self),
            })
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
