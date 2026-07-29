//! End-to-end session orchestration tests over an in-memory channel (T-SESS*).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rb_core::channel::{ChunkEvent, ChunkSink, ChunkSource, DataChannel, DuplexStream};
use rb_core::error::Result;
use rb_core::module::{Destination, Source};
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, Preflight};
use rb_core::progress::Progress;
use rb_core::session;

/// In-memory single-substream channel: consumer half via `open_stream`,
/// provider half via `accept_stream`.
struct TestChannel {
    consumer: Mutex<Vec<Box<dyn DuplexStream>>>,
    provider: Mutex<Vec<Box<dyn DuplexStream>>>,
    carriers: usize,
}

impl TestChannel {
    fn pair() -> Arc<Self> {
        Self::pair_carriers(1)
    }

    fn pair_carriers(carriers: usize) -> Arc<Self> {
        let streams = if carriers == 1 { 1 } else { carriers + 1 };
        let mut consumer = Vec::with_capacity(streams);
        let mut provider = Vec::with_capacity(streams);
        for _ in 0..streams {
            let (a, b) = tokio::io::duplex(1 << 20);
            consumer.push(Box::new(a) as Box<dyn DuplexStream>);
            provider.push(Box::new(b) as Box<dyn DuplexStream>);
        }
        Arc::new(TestChannel {
            consumer: Mutex::new(consumer),
            provider: Mutex::new(provider),
            carriers,
        })
    }
}

#[async_trait]
impl DataChannel for TestChannel {
    async fn open_stream(&self) -> Result<Box<dyn DuplexStream>> {
        Ok(self.consumer.lock().unwrap().remove(0))
    }
    async fn accept_stream(&self) -> Result<Box<dyn DuplexStream>> {
        Ok(self.provider.lock().unwrap().remove(0))
    }
    fn carriers(&self) -> usize {
        self.carriers
    }
}

fn demo_plan() -> BackupPlan {
    BackupPlan {
        format_version: rb_core::plan::PLAN_FORMAT_VERSION,
        module: "mock".into(),
        mode: BackupMode::Copy1to1,
        created_at: "2026-06-20T00:00:00Z".into(),
        source_summary: "1 item".into(),
        items: vec![PlanItem {
            id: 1,
            ordinal: 0,
            kind: "blob".into(),
            name: "alpha".into(),
            estimated_bytes: 2048,
            meta: serde_json::Value::Null,
        }],
        estimated_bytes: 2048,
        integrity: IntegritySpec::default(),
        payload: serde_json::json!({}),
    }
}

fn metadata_only_plan() -> BackupPlan {
    let mut plan = demo_plan();
    plan.items[0].meta = serde_json::json!({"expects_data": false});
    plan.items[0].estimated_bytes = 0;
    plan.estimated_bytes = 0;
    plan
}

fn two_item_plan() -> BackupPlan {
    let mut plan = demo_plan();
    plan.items.push(PlanItem {
        id: 2,
        ordinal: 1,
        kind: "blob".into(),
        name: "beta".into(),
        estimated_bytes: 0,
        meta: serde_json::Value::Null,
    });
    plan
}

struct MockSource {
    fp: String,
    payload: Vec<u8>,
}

struct MetadataOnlySource;
#[async_trait]
impl Source for MetadataOnlySource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Ok(metadata_only_plan())
    }
    async fn stream_out(&self, _plan: &BackupPlan, _sink: &mut dyn ChunkSink) -> Result<()> {
        Ok(())
    }
    async fn fingerprint(&self) -> Result<String> {
        Ok("stable".into())
    }
}

struct SkippingSource;
#[async_trait]
impl Source for SkippingSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Ok(two_item_plan())
    }
    async fn stream_out(&self, _plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        sink.send_chunk(1, 0, b"present").await?;
        sink.finish_item(1, 7, &rb_core::wire::blake3_hex(b"present"))
            .await
    }
    async fn fingerprint(&self) -> Result<String> {
        Ok("stable".into())
    }
}

#[async_trait]
impl Source for MockSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Ok(demo_plan())
    }
    async fn stream_out(&self, _plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        sink.send_chunk(1, 0, &self.payload).await?;
        sink.finish_item(
            1,
            self.payload.len() as u64,
            &rb_core::wire::blake3_hex(&self.payload),
        )
        .await
    }
    async fn fingerprint(&self) -> Result<String> {
        Ok(self.fp.clone())
    }
}

/// A source whose fingerprint changes after analyze — must trip the guard.
struct MutatingSource {
    calls: AtomicU64,
}
#[async_trait]
impl Source for MutatingSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Ok(demo_plan())
    }
    async fn stream_out(&self, _p: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        sink.send_chunk(1, 0, b"x").await?;
        sink.finish_item(1, 1, &rb_core::wire::blake3_hex(b"x"))
            .await
    }
    async fn fingerprint(&self) -> Result<String> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("fp-{n}")) // differs each call
    }
}

struct MockDest {
    received: Arc<Mutex<Vec<u8>>>,
}
#[async_trait]
impl Destination for MockDest {
    async fn validate(&self, _plan: &BackupPlan) -> Result<Preflight> {
        Ok(Preflight::pass().check("space", true, "ok"))
    }
    async fn stream_in(&self, _plan: &BackupPlan, src: &mut dyn ChunkSource) -> Result<()> {
        loop {
            match src.next().await? {
                ChunkEvent::Chunk { data, .. } => {
                    self.received.lock().unwrap().extend_from_slice(&data)
                }
                ChunkEvent::ItemEnd { .. } => {}
                ChunkEvent::End => break,
            }
        }
        Ok(())
    }
}

/// A source that fails partway through streaming (backend read error).
struct FailingSource {
    fp: String,
}
#[async_trait]
impl Source for FailingSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Ok(demo_plan())
    }
    async fn stream_out(&self, _p: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        sink.send_chunk(1, 0, b"partial").await?;
        Err(rb_core::error::BackupError::phase(
            rb_core::error::Phase::Transfer,
            "simulated backend read failure",
        ))
    }
    async fn fingerprint(&self) -> Result<String> {
        Ok(self.fp.clone())
    }
}

/// A source announcing a plan version this build does not understand.
struct FuturePlanSource;
#[async_trait]
impl Source for FuturePlanSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        let mut plan = demo_plan();
        plan.format_version = rb_core::plan::PLAN_FORMAT_VERSION + 7;
        Ok(plan)
    }
    async fn stream_out(&self, _p: &BackupPlan, _sink: &mut dyn ChunkSink) -> Result<()> {
        panic!("must never stream: the plan version was refused")
    }
    async fn fingerprint(&self) -> Result<String> {
        Ok("stable".into())
    }
}

/// A destination whose preflight fails.
struct FailingPreflightDest;
#[async_trait]
impl Destination for FailingPreflightDest {
    async fn validate(&self, _plan: &BackupPlan) -> Result<Preflight> {
        Ok(Preflight::pass().check("space", false, "not enough room"))
    }
    async fn stream_in(&self, _plan: &BackupPlan, _src: &mut dyn ChunkSource) -> Result<()> {
        panic!("must never apply: preflight failed")
    }
}

/// T-SESS4: a source that fails mid-stream propagates its own error, tells the
/// destination why, and is still audited for immutability.
#[tokio::test]
async fn source_failure_aborts_destination_with_reason() {
    let ch = TestChannel::pair();
    let src = FailingSource {
        fp: "stable".into(),
    };
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let s = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    let serr = s.await.unwrap().expect_err("source must fail");
    assert!(
        format!("{serr}").contains("simulated backend read failure"),
        "source error should be its own cause, not the immutability audit: {serr}"
    );
    let derr = d.await.unwrap().expect_err("destination must fail");
    assert!(
        format!("{derr}").contains("source aborted mid-stream"),
        "destination should learn the reason: {derr}"
    );
}

/// T-SESS5: the immutability audit also runs when the run FAILED — a source that
/// drifted during an aborted transfer is still caught (this is the case where a
/// half-applied write would otherwise hide).
#[tokio::test]
async fn aborted_run_still_audits_immutability() {
    struct MutatingFailingSource {
        calls: AtomicU64,
    }
    #[async_trait]
    impl Source for MutatingFailingSource {
        async fn analyze(&self) -> Result<BackupPlan> {
            Ok(demo_plan())
        }
        async fn stream_out(&self, _p: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
            sink.send_chunk(1, 0, b"partial").await?;
            Err(rb_core::error::BackupError::phase(
                rb_core::error::Phase::Transfer,
                "simulated failure",
            ))
        }
        async fn fingerprint(&self) -> Result<String> {
            Ok(format!("fp-{}", self.calls.fetch_add(1, Ordering::SeqCst)))
        }
    }

    let ch = TestChannel::pair();
    let src = MutatingFailingSource {
        calls: AtomicU64::new(0),
    };
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let s = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    let serr = s.await.unwrap().expect_err("must fail");
    let _ = d.await.unwrap();
    assert!(
        matches!(serr, rb_core::error::BackupError::SourceMutated(_)),
        "drift during an aborted run must surface as SourceMutated, got: {serr}"
    );
}

/// T-SESS6: a plan whose format_version this build cannot interpret is refused
/// by the destination before any restore, and the source is told.
#[tokio::test]
async fn unsupported_plan_version_refused() {
    let ch = TestChannel::pair();
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let s = tokio::spawn(async move { session::run_source(&FuturePlanSource, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    let derr = d.await.unwrap().expect_err("destination must refuse");
    assert!(
        matches!(derr, rb_core::error::BackupError::PlanRejected(_)),
        "got: {derr}"
    );
    let serr = s.await.unwrap().expect_err("source must see the refusal");
    assert!(
        matches!(serr, rb_core::error::BackupError::PlanRejected(_)),
        "got: {serr}"
    );
}

/// T-SESS7: a failed preflight aborts before any apply and is reported as
/// Preflight (not as an operator rejection).
#[tokio::test]
async fn failed_preflight_aborts_before_apply() {
    let ch = TestChannel::pair();
    let src = MockSource {
        fp: "stable".into(),
        payload: vec![1u8; 32],
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let s = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&FailingPreflightDest, &*ch2, &p2, &mut yes).await
    });
    let derr = d.await.unwrap().expect_err("preflight must fail");
    assert!(
        matches!(derr, rb_core::error::BackupError::Preflight(_)),
        "got: {derr}"
    );
    assert!(s.await.unwrap().is_err(), "source must see the refusal");
}

/// T-SESS8: `--max-rate` paces the payload without buffering ahead — a 4 KiB
/// payload at 4 KiB/s takes at least ~1 s of wall clock.
#[tokio::test]
async fn max_rate_paces_the_transfer() {
    let ch = TestChannel::pair();
    let payload = vec![3u8; 4096];
    let src = MockSource {
        fp: "stable".into(),
        payload,
    };
    let received = Arc::new(Mutex::new(Vec::new()));
    let dst = MockDest {
        received: received.clone(),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let started = std::time::Instant::now();
    let s =
        tokio::spawn(async move { session::run_source_limited(&src, &*ch, &p1, Some(4096)).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    s.await.unwrap().expect("source ok");
    d.await.unwrap().expect("dest ok");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(900),
        "4 KiB at 4 KiB/s should take ~1s, took {:?}",
        started.elapsed()
    );
    assert_eq!(received.lock().unwrap().len(), 4096);
}

/// T-SESS9: both sides publish plan totals into `Progress`, so a reporter can
/// render items/bytes/percent/rate for the whole run (I-OBSERV).
#[tokio::test]
async fn both_sides_publish_progress_totals() {
    let ch = TestChannel::pair();
    let payload = vec![5u8; 2048];
    let src = MockSource {
        fp: "stable".into(),
        payload,
    };
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let (r1, r2) = (p1.clone(), p2.clone());
    let ch2 = ch.clone();
    let s = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    s.await.unwrap().expect("source ok");
    d.await.unwrap().expect("dest ok");
    for (side, p) in [("source", r1), ("destination", r2)] {
        let line = p.line(1.0);
        assert!(line.contains("items 1/1"), "{side}: {line}");
        assert!(line.contains("2.00 KiB/2.00 KiB"), "{side}: {line}");
        assert!(line.contains("(100.0%)"), "{side}: {line}");
    }
}

/// T-SESS1: full source→dest run; payload arrives intact, immutability holds.
#[tokio::test]
async fn full_run_ok() {
    let ch = TestChannel::pair();
    let payload = vec![42u8; 5000];
    let src = MockSource {
        fp: "stable".into(),
        payload: payload.clone(),
    };
    let received = Arc::new(Mutex::new(Vec::new()));
    let dst = MockDest {
        received: received.clone(),
    };
    let p1 = Progress::default();
    let p2 = Progress::default();
    let ch2 = ch.clone();

    let s = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    s.await.unwrap().expect("source ok");
    d.await.unwrap().expect("dest ok");
    assert_eq!(*received.lock().unwrap(), payload);
}

/// T-SESS12: four data carriers preserve the same result as one carrier. The
/// first in-memory substream is control; all payload rides item-pinned streams.
#[tokio::test]
async fn multi_carrier_run_ok() {
    let ch = TestChannel::pair_carriers(4);
    let payload = vec![99u8; 5000];
    let src = MockSource {
        fp: "stable".into(),
        payload: payload.clone(),
    };
    let received = Arc::new(Mutex::new(Vec::new()));
    let dst = MockDest {
        received: received.clone(),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let source = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let destination = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    source.await.unwrap().unwrap();
    destination.await.unwrap().unwrap();
    assert_eq!(*received.lock().unwrap(), payload);
}

/// V3.1: a source cannot report success after silently skipping a data-bearing
/// plan item; metadata-only entries remain valid with no `ItemEnd`.
#[tokio::test]
async fn completion_requires_every_data_bearing_plan_item() {
    let ch = TestChannel::pair();
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let source = tokio::spawn(async move { session::run_source(&SkippingSource, &*ch, &p1).await });
    let destination = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    let err = destination
        .await
        .expect("destination task")
        .expect_err("missing item must fail verification");
    assert!(format!("{err}").contains("missing=[2]"), "got: {err}");
    let _ = source.await.expect("source task");

    let ch = TestChannel::pair();
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let source =
        tokio::spawn(async move { session::run_source(&MetadataOnlySource, &*ch, &p1).await });
    let destination = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    source
        .await
        .expect("source task")
        .expect("metadata source ok");
    destination
        .await
        .expect("destination task")
        .expect("metadata-only item needs no ItemEnd");
}

/// T-SESS2: a destination that rejects the plan aborts cleanly; source sees it.
#[tokio::test]
async fn rejected_plan_aborts() {
    let ch = TestChannel::pair();
    let src = MockSource {
        fp: "stable".into(),
        payload: vec![1u8; 16],
    };
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let s = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut no = |_: &BackupPlan| false;
        session::run_destination(&dst, &*ch2, &p2, &mut no).await
    });
    assert!(s.await.unwrap().is_err(), "source should see rejection");
    assert!(d.await.unwrap().is_err(), "dest should report rejection");
}

/// T-SESS3: a source that mutates during the run is caught by the audit.
#[tokio::test]
async fn mutating_source_caught() {
    let ch = TestChannel::pair();
    let src = MutatingSource {
        calls: AtomicU64::new(0),
    };
    let dst = MockDest {
        received: Arc::new(Mutex::new(Vec::new())),
    };
    let (p1, p2) = (Progress::default(), Progress::default());
    let ch2 = ch.clone();
    let s = tokio::spawn(async move { session::run_source(&src, &*ch, &p1).await });
    let d = tokio::spawn(async move {
        let mut yes = |_: &BackupPlan| true;
        session::run_destination(&dst, &*ch2, &p2, &mut yes).await
    });
    let serr = s.await.unwrap();
    let _ = d.await.unwrap();
    let err = serr.expect_err("must trip immutability guard");
    assert!(
        matches!(err, rb_core::error::BackupError::SourceMutated(_)),
        "got: {err}"
    );
}
