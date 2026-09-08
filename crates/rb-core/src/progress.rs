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

    /// Replace the *byte* estimate with the observed total. Backends such as
    /// PostgreSQL plan from on-disk relation sizes while transferring a smaller
    /// logical stream, so an acknowledged success must use actuals to render a
    /// truthful 100% terminal snapshot.
    ///
    /// The item total is deliberately NOT rewritten: it comes from the plan and
    /// is exact. Rewriting it made the terminal line read `items 4/4` for a run
    /// that had covered 4 of 10 planned items, i.e. the final snapshot could
    /// never disagree with the plan.
    pub fn complete(&self) {
        let bytes = self.inner.bytes_done.load(Ordering::Relaxed);
        self.inner.bytes_total.store(bytes, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte estimate is replaced by the actual; the ITEM total stays at the
    /// plan value, so a run that covered part of the plan cannot render as a
    /// complete one.
    #[test]
    fn completion_replaces_the_byte_estimate_but_not_the_plan_item_count() {
        let short = Progress::new(4, 50_000);
        short.add_bytes(1000);
        short.item_done();
        short.complete();
        let line = short.line(1.0);
        assert!(line.contains("items 1/4"), "{line}");
        assert!(line.contains("1000 B/1000 B"), "{line}");
        assert!(line.contains("(100.0%)"), "{line}");

        let full = Progress::new(2, 50_000);
        full.add_bytes(4096);
        full.item_done();
        full.item_done();
        full.complete();
        let line = full.line(1.0);
        assert!(line.contains("items 2/2"), "{line}");
    }
}
