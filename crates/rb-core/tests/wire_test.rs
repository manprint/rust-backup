//! rb-core wire/channel roundtrip + integrity tests (T-CORE*).

use rb_core::channel::{ChunkEvent, ChunkSink, ChunkSource, StreamChunkSink, StreamChunkSource};
use rb_core::plan::{human_bytes, BackupMode, BackupPlan, IntegritySpec};
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

/// T-CORE4: human_bytes formats scale correctly.
#[test]
fn human_bytes_scales() {
    assert_eq!(human_bytes(512), "512 B");
    assert!(human_bytes(1536).starts_with("1.50 KiB"));
}
