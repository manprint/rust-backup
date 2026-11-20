//! Operator-interrupt handling of a destination run. One test, one process:
//! the interrupt flag is process-global.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rb_core::channel::{ChunkEvent, ChunkSink, ChunkSource, DataChannel, DuplexStream};
use rb_core::error::Result;
use rb_core::module::{Destination, Source};
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, Preflight};
use rb_core::progress::Progress;
use rb_core::verification::{RestoreEvidence, VerificationReport};
use rb_core::{interrupt, session};

struct TestChannel {
    consumer: Mutex<Vec<Box<dyn DuplexStream>>>,
    provider: Mutex<Vec<Box<dyn DuplexStream>>>,
}

impl TestChannel {
    fn pair() -> Arc<Self> {
        let mut consumer = Vec::new();
        let mut provider = Vec::new();
        for _ in 0..2 {
            let (a, b) = tokio::io::duplex(1 << 20);
            consumer.push(Box::new(a) as Box<dyn DuplexStream>);
            provider.push(Box::new(b) as Box<dyn DuplexStream>);
        }
        Arc::new(Self {
            consumer: Mutex::new(consumer),
            provider: Mutex::new(provider),
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
        1
    }
}

struct OneBlob;

#[async_trait]
impl Source for OneBlob {
    async fn analyze(&self) -> Result<BackupPlan> {
        Ok(BackupPlan {
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
                estimated_bytes: 64,
                meta: serde_json::Value::Null,
            }],
            estimated_bytes: 64,
            integrity: IntegritySpec::default(),
            payload: serde_json::json!({}),
        })
    }
    async fn stream_out(&self, _plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        let payload = [7u8; 64];
        sink.send_chunk(1, 0, &payload).await?;
        sink.finish_item(1, 64, &rb_core::wire::blake3_hex(&payload))
            .await
    }
    async fn fingerprint(&self) -> Result<String> {
        Ok("stable".into())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum InterruptAt {
    /// After the first chunk event, with more of the stream still to read.
    MidApply,
    /// After the whole stream was read, just before `stream_in` returns `Ok`.
    EndOfApply,
    /// During the read-back, after the payload was applied.
    Verify,
    /// Never from inside the destination.
    Never,
}

struct Dest {
    at: InterruptAt,
    applies: AtomicUsize,
    stream_failed: AtomicBool,
    abandoned: AtomicUsize,
}

impl Dest {
    fn new(at: InterruptAt) -> Arc<Self> {
        Arc::new(Self {
            at,
            applies: AtomicUsize::new(0),
            stream_failed: AtomicBool::new(false),
            abandoned: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Destination for Dest {
    async fn validate(&self, _plan: &BackupPlan) -> Result<Preflight> {
        Ok(Preflight::pass())
    }

    async fn stream_in(&self, _plan: &BackupPlan, src: &mut dyn ChunkSource) -> Result<()> {
        self.applies.fetch_add(1, Ordering::Relaxed);
        assert_eq!(interrupt::restores_in_flight(), 1);
        let mut first = true;
        loop {
            let event = src.next().await.inspect_err(|_| {
                self.stream_failed.store(true, Ordering::Relaxed);
            })?;
            if matches!(event, ChunkEvent::End) {
                break;
            }
            if first && self.at == InterruptAt::MidApply {
                interrupt::request("SIGINT");
            }
            first = false;
        }
        if self.at == InterruptAt::EndOfApply {
            interrupt::request("SIGTERM");
        }
        Ok(())
    }

    async fn verify(
        &self,
        _plan: &BackupPlan,
        evidence: &RestoreEvidence,
    ) -> Result<VerificationReport> {
        if self.at == InterruptAt::Verify {
            interrupt::request("SIGINT");
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
        Ok(evidence.report("read-back"))
    }

    async fn abandon(&self) {
        self.abandoned.fetch_add(1, Ordering::Relaxed);
    }
}

async fn run(dest: Arc<Dest>) -> String {
    let channel = TestChannel::pair();
    let peer = channel.clone();
    let source_task = tokio::spawn(async move {
        session::run_source(&OneBlob, &*channel, &Progress::default()).await
    });
    let mut yes = |_: &BackupPlan| true;
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        session::run_destination(&*dest, &*peer, &Progress::default(), &mut yes),
    )
    .await
    .expect("an interrupted destination must not hang");
    drop(peer);
    source_task.abort();
    assert_eq!(interrupt::restores_in_flight(), 0);
    result
        .expect_err("an interrupted run must fail")
        .to_string()
}

#[tokio::test]
async fn an_interrupt_unwinds_or_undoes_the_restore_at_every_stage() {
    // Mid-apply: the chunk stream fails, so the module's own error path runs;
    // nothing was certified as applied, so `abandon` is not the cleanup.
    let dest = Dest::new(InterruptAt::MidApply);
    let error = run(dest.clone()).await;
    assert!(error.contains("interrupted by SIGINT"), "{error}");
    assert!(dest.stream_failed.load(Ordering::Relaxed));
    assert_eq!(dest.abandoned.load(Ordering::Relaxed), 0);
    interrupt::clear();

    // The signal lands after the last chunk: the apply succeeded, so the copy
    // is whole but uncertified and must be undone.
    let dest = Dest::new(InterruptAt::EndOfApply);
    let error = run(dest.clone()).await;
    assert!(error.contains("interrupted by SIGTERM"), "{error}");
    assert_eq!(dest.abandoned.load(Ordering::Relaxed), 1);
    interrupt::clear();

    // During the read-back: the run is dropped and the applied copy undone.
    let dest = Dest::new(InterruptAt::Verify);
    let error = run(dest.clone()).await;
    assert!(error.contains("interrupted by SIGINT"), "{error}");
    assert_eq!(dest.abandoned.load(Ordering::Relaxed), 1);

    // Still interrupted, before anything was applied: nothing to undo.
    let dest = Dest::new(InterruptAt::Never);
    let error = run(dest.clone()).await;
    assert!(error.contains("interrupted by SIGINT"), "{error}");
    assert_eq!(dest.applies.load(Ordering::Relaxed), 0);
    assert_eq!(dest.abandoned.load(Ordering::Relaxed), 0);
    interrupt::clear();
}
