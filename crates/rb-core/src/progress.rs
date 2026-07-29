//! Lock-free progress accounting, surfaced on both source and destination.
//!
//! INVARIANT (observability): every run emits clear progress on both sides. The
//! counters are atomic so the hot data path increments them without contention;
//! a caller-owned ticker renders [`Progress::line`] periodically.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::plan::human_bytes;

/// Shared, cloneable progress handle. Clone freely across tasks.
#[derive(Clone, Default)]
pub struct Progress {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    bytes_done: AtomicU64,
    items_done: AtomicUsize,
    items_total: AtomicUsize,
    bytes_total: AtomicU64,
}

impl Progress {
    pub fn new(items_total: usize, bytes_total: u64) -> Self {
        let p = Progress::default();
        p.inner.items_total.store(items_total, Ordering::Relaxed);
        p.inner.bytes_total.store(bytes_total, Ordering::Relaxed);
        p
    }

    /// Set the expected totals once the plan is known (the plan is only
    /// available after `analyze` on the source / after `recv_plan` on the
    /// destination, so totals arrive later than the handle itself).
    pub fn set_totals(&self, items_total: usize, bytes_total: u64) {
        self.inner.items_total.store(items_total, Ordering::Relaxed);
        self.inner.bytes_total.store(bytes_total, Ordering::Relaxed);
    }

    /// Record `n` transferred payload bytes.
    pub fn add_bytes(&self, n: u64) {
        self.inner.bytes_done.fetch_add(n, Ordering::Relaxed);
    }

    /// Record one completed item.
    pub fn item_done(&self) {
        self.inner.items_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Current transferred byte count.
    pub fn bytes(&self) -> u64 {
        self.inner.bytes_done.load(Ordering::Relaxed)
    }

    /// Render a one-line progress string given the elapsed seconds (the caller
    /// owns the clock — core never reads time directly).
    pub fn line(&self, elapsed_secs: f64) -> String {
        let done = self.inner.bytes_done.load(Ordering::Relaxed);
        let total = self.inner.bytes_total.load(Ordering::Relaxed);
        let items_done = self.inner.items_done.load(Ordering::Relaxed);
        let items_total = self.inner.items_total.load(Ordering::Relaxed);
        let rate = if elapsed_secs > 0.0 {
            (done as f64 / elapsed_secs) as u64
        } else {
            0
        };
        let pct = if total > 0 {
            (done as f64 / total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        format!(
            "items {items_done}/{items_total}  {}/{}  ({pct:.1}%)  {}/s",
            human_bytes(done),
            human_bytes(total),
            human_bytes(rate),
        )
    }
}
