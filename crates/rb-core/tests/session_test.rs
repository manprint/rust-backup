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
    consumer: Mutex<Option<Box<dyn DuplexStream>>>,
    provider: Mutex<Option<Box<dyn DuplexStream>>>,
}

impl TestChannel {
    fn pair() -> Arc<Self> {
        let (a, b) = tokio::io::duplex(1 << 20);
        Arc::new(TestChannel {
            consumer: Mutex::new(Some(Box::new(a))),
            provider: Mutex::new(Some(Box::new(b))),
        })
    }
}

#[async_trait]
impl DataChannel for TestChannel {
    async fn open_stream(&self) -> Result<Box<dyn DuplexStream>> {
        Ok(self
            .consumer
            .lock()
            .unwrap()
            .take()
            .expect("one open_stream"))
    }
    async fn accept_stream(&self) -> Result<Box<dyn DuplexStream>> {
        Ok(self
            .provider
            .lock()
            .unwrap()
            .take()
            .expect("one accept_stream"))
    }
    fn carriers(&self) -> usize {
        1
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

struct MockSource {
    fp: String,
    payload: Vec<u8>,
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
