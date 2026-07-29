//! rb-core wire/channel roundtrip + integrity tests (T-CORE*).

use rb_core::channel::{
    validate_plan_bounds, ChunkEvent, ChunkSink, ChunkSource, StreamChunkSink, StreamChunkSource,
    MAX_PLAN_ITEM_META_BYTES, MAX_PLAN_ITEM_NAME_BYTES,
};
use rb_core::plan::{human_bytes, BackupMode, BackupPlan, IntegritySpec, PlanItem};
use rb_core::wire;

/// T-CORE1: a chunk written by the sink is read back identically by the source.
#[tokio::test]
async fn chunk_roundtrip() {
    let (a, b) = tokio::io::duplex(1 << 20);
    let payload = vec![7u8; 4096];
    let p2 = payload.clone();

    let send = tokio::spawn(async move {
        let mut sink = StreamChunkSink::new(a);
        sink.send_chunk(1, 0, &p2).await.unwrap();
        sink.finish_item(1, p2.len() as u64, &wire::blake3_hex(&p2))
            .await
            .unwrap();
        sink.finish().await.unwrap();
    });

    let mut src = StreamChunkSource::new(b);
    match src.next().await.unwrap() {
        ChunkEvent::Chunk { item_id, data, .. } => {
            assert_eq!(item_id, 1);
            assert_eq!(data, payload);
        }
        e => panic!("expected Chunk, got {e:?}"),
    }
    assert!(matches!(
        src.next().await.unwrap(),
        ChunkEvent::ItemEnd { .. }
    ));
    assert!(matches!(src.next().await.unwrap(), ChunkEvent::End));
    send.await.unwrap();
}

/// T-CORE2: a corrupted chunk is detected via BLAKE3 mismatch.
#[tokio::test]
async fn integrity_mismatch_detected() {
    // Hand-craft a ChunkStart whose declared hash does not match the bytes.
    let (mut a, b) = tokio::io::duplex(1 << 16);
    let bad = wire::DataFrame::ChunkStart {
        item_id: 1,
        offset: 0,
        len: 3,
        blake3: "deadbeef".to_string(),
    };
    tokio::spawn(async move {
        wire::send_frame(&mut a, &bad).await.unwrap();
        wire::write_all_idle(&mut a, &[1, 2, 3]).await.unwrap();
    });
    let mut src = StreamChunkSource::new(b);
    let err = src.next().await.unwrap_err();
    assert!(format!("{err}").contains("integrity"), "got: {err}");
}

/// T-CORE3: plans serialize/deserialize losslessly through JSON.
#[tokio::test]
async fn plan_serde_roundtrip() {
    let plan = BackupPlan {
        format_version: rb_core::plan::PLAN_FORMAT_VERSION,
        module: "filesystem".into(),
        mode: BackupMode::Copy1to1,
        created_at: "2026-06-20T00:00:00Z".into(),
        source_summary: "1 dir".into(),
        items: vec![],
        estimated_bytes: 1024,
        integrity: IntegritySpec::default(),
        payload: serde_json::json!({"root": "/data"}),
    };
    let js = serde_json::to_string(&plan).unwrap();
    let back: BackupPlan = serde_json::from_str(&js).unwrap();
    assert_eq!(back.module, "filesystem");
    assert!(back.render().contains("filesystem"));
}

fn bounded_plan() -> BackupPlan {
    BackupPlan {
        format_version: rb_core::plan::PLAN_FORMAT_VERSION,
        module: "filesystem".into(),
        mode: BackupMode::Copy1to1,
        created_at: "2026-06-20T00:00:00Z".into(),
        source_summary: "bounded test".into(),
        items: vec![PlanItem {
            id: 1,
            ordinal: 0,
            kind: "file".into(),
            name: "one".into(),
            estimated_bytes: 0,
            meta: serde_json::Value::Null,
        }],
        estimated_bytes: 0,
        integrity: IntegritySpec::default(),
        payload: serde_json::Value::Null,
    }
}

/// V7.3: all peer-controlled plan strings and JSON metadata have explicit caps.
#[test]
fn plan_bounds_reject_oversized_item_fields() {
    let mut plan = bounded_plan();
    plan.items[0].name = "x".repeat(MAX_PLAN_ITEM_NAME_BYTES + 1);
    assert!(
        format!("{}", validate_plan_bounds(&plan).expect_err("name cap"))
            .contains("name/kind exceeds")
    );

    let mut plan = bounded_plan();
    plan.items[0].meta = serde_json::Value::String("x".repeat(MAX_PLAN_ITEM_META_BYTES));
    assert!(
        format!("{}", validate_plan_bounds(&plan).expect_err("meta cap")).contains("meta exceeds")
    );
}

/// T-CORE5: a peer-declared chunk length above CHUNK_SIZE is refused BEFORE the
/// receiver allocates it (a hostile/corrupt header must not become a 4 GiB
/// allocation).
#[tokio::test]
async fn oversized_declared_chunk_is_refused() {
    let (mut a, b) = tokio::io::duplex(1 << 16);
    let huge = wire::DataFrame::ChunkStart {
        item_id: 1,
        offset: 0,
        len: u32::MAX,
        blake3: wire::blake3_hex(&[]),
    };
    tokio::spawn(async move {
        // Header only: if the receiver were to allocate/read it would hang here.
        wire::send_frame(&mut a, &huge).await.unwrap();
        futures_util_pending().await;
    });
    let mut src = StreamChunkSource::new(b);
    let err = src.next().await.unwrap_err();
    assert!(
        format!("{err}").contains("exceeds CHUNK_SIZE"),
        "got: {err}"
    );
}

/// Keeps the writer half alive without sending payload bytes.
async fn futures_util_pending() {
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
}

/// T-CORE6: chunks that individually verify but do not add up to the declared
/// whole-item digest are caught at ItemEnd (per-item integrity, D12).
#[tokio::test]
async fn item_digest_mismatch_detected() {
    let (a, b) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let mut sink = StreamChunkSink::new(a);
        sink.send_chunk(1, 0, b"hello").await.unwrap();
        // Digest of DIFFERENT content: each chunk hash is right, the fold is not.
        sink.finish_item(1, 5, &wire::blake3_hex(b"HELLO"))
            .await
            .unwrap();
    });
    let mut src = StreamChunkSource::new(b);
    assert!(matches!(
        src.next().await.unwrap(),
        ChunkEvent::Chunk { .. }
    ));
    let err = src.next().await.unwrap_err();
    assert!(
        format!("{err}").contains("whole-item blake3 mismatch"),
        "got: {err}"
    );
}

/// T-CORE7: the whole-item digest is folded across MANY chunks (it is a running
/// hash, not a per-chunk one), and a correct multi-chunk item verifies.
#[tokio::test]
async fn multi_chunk_item_digest_verifies() {
    let (a, b) = tokio::io::duplex(1 << 20);
    let part = vec![9u8; 1000];
    let whole: Vec<u8> = part.iter().chain(part.iter()).copied().collect();
    let expect = wire::blake3_hex(&whole);
    let p2 = part.clone();
    tokio::spawn(async move {
        let mut sink = StreamChunkSink::new(a);
        sink.send_chunk(1, 0, &p2).await.unwrap();
        sink.send_chunk(1, 1000, &p2).await.unwrap();
        sink.finish_item(1, 2000, &expect).await.unwrap();
        sink.finish().await.unwrap();
    });
    let mut src = StreamChunkSource::new(b);
    for _ in 0..2 {
        assert!(matches!(
            src.next().await.unwrap(),
            ChunkEvent::Chunk { .. }
        ));
    }
    assert!(matches!(
        src.next().await.unwrap(),
        ChunkEvent::ItemEnd { .. }
    ));
    assert!(matches!(src.next().await.unwrap(), ChunkEvent::End));
}

/// T-CORE8: a zero-chunk item still verifies against the digest of no bytes, so
/// empty items (empty file, empty table) are not a special case.
#[tokio::test]
async fn empty_item_verifies() {
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let mut sink = StreamChunkSink::new(a);
        sink.finish_item(7, 0, &wire::blake3_hex(&[]))
            .await
            .unwrap();
        sink.finish().await.unwrap();
    });
    let mut src = StreamChunkSource::new(b);
    assert!(matches!(
        src.next().await.unwrap(),
        ChunkEvent::ItemEnd { item_id: 7, .. }
    ));
}

/// T-CORE9: a mid-stream producer abort reaches the consumer as an explicit
/// phase-tagged error carrying the reason (not an opaque EOF).
#[tokio::test]
async fn mid_stream_abort_carries_reason() {
    let (mut a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        wire::send_frame(
            &mut a,
            &wire::DataFrame::Abort {
                reason: "backend read failed".into(),
            },
        )
        .await
        .unwrap();
    });
    let mut src = StreamChunkSource::new(b);
    let err = src.next().await.unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("source aborted mid-stream"), "got: {text}");
    assert!(text.contains("backend read failed"), "got: {text}");
}

/// T-CORE4: human_bytes formats scale correctly.
#[test]
fn human_bytes_scales() {
    assert_eq!(human_bytes(512), "512 B");
    assert!(human_bytes(1536).starts_with("1.50 KiB"));
}
