//! Source-side streaming (plan Phase 3.3).
//!
//! Per collection plan item, open a `find({})` cursor and push each document's
//! raw BSON (length-prefixed, self-delimiting) into the channel's [`ChunkSink`]
//! in ≤ `CHUNK_SIZE` chunks — no temp files (I-NOTEMP). The byte stream is
//! consumer-paced: `send_chunk` rides the substream's flow control, so a slow
//! destination stalls these cursor reads (I-BANDWIDTH). Strictly read-only
//! (I-IMMUT): only `find` is issued.
//!
//! The re-chunking is decoupled from the driver (generic over any stream of
//! BSON documents) so it is unit-tested in-process; the live `find` integration
//! is exercised by the mongodb e2e.

use futures_util::Stream;
use mongodb::bson::{doc, Document};
use serde::Deserialize;

use rb_core::channel::ChunkSink;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::BackupPlan;
use rb_core::wire::CHUNK_SIZE;

use crate::{MongoConnection, MongoDbParams};

/// The per-item descriptor carried in `PlanItem::meta` (set by `build_plan`,
/// consumed by both source `stream_out` and destination `stream_in`).
#[derive(Debug, Deserialize)]
pub(crate) struct ItemMeta {
    pub(crate) database: String,
    pub(crate) collection: String,
}

/// Stream every collection item of `plan` into `sink`. Does NOT close the sink
/// (the session calls `finish` once all items are done).
pub async fn stream_out(
    params: &MongoDbParams,
    plan: &BackupPlan,
    sink: &mut dyn ChunkSink,
) -> Result<()> {
    let conn = MongoConnection::connect(params).await?;

    for item in &plan.items {
        if item.kind != "collection" {
            continue;
        }
        let meta: ItemMeta = serde_json::from_value(item.meta.clone()).map_err(|e| {
            BackupError::phase(
                Phase::Transfer,
                format!("bad item meta '{}': {e}", item.name),
            )
        })?;

        let coll = conn
            .client
            .database(&meta.database)
            .collection::<Document>(&meta.collection);
        // A stable order makes the source commitment reproducible when the
        // destination is read back after restore. `_id` is unique and indexed.
        let cursor = coll
            .find(doc! {})
            .sort(doc! { "_id": 1 })
            .await
            .map_err(|e| {
                BackupError::phase_src(Phase::Transfer, format!("find {}", item.name), e)
            })?;
        docs_to_sink(item.id, cursor, sink).await?;
    }
    Ok(())
}

/// Serialize each document to raw BSON and re-chunk the concatenation into ≤
/// `CHUNK_SIZE` pieces, pushing each to `sink` with a running offset, hashing the
/// whole item, then emitting `finish_item`. Returns the total byte count.
///
/// Each document is length-prefixed by BSON itself, so the stream is
/// self-delimiting and the destination can split it back into documents even
/// though chunk boundaries fall anywhere.
async fn docs_to_sink<E, St>(item_id: u32, stream: St, sink: &mut dyn ChunkSink) -> Result<u64>
where
    St: Stream<Item = std::result::Result<Document, E>>,
    E: std::fmt::Display,
{
    use futures_util::StreamExt;

    tokio::pin!(stream);
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
    let mut offset: u64 = 0;
    let mut hasher = blake3::Hasher::new();

    while let Some(item) = stream.next().await {
        let doc = item.map_err(|e| BackupError::phase(Phase::Transfer, format!("cursor: {e}")))?;
        let bytes = mongodb::bson::to_vec(&doc)
            .map_err(|e| BackupError::phase_src(Phase::Transfer, "encode bson", e))?;
        buf.extend_from_slice(&bytes);
        while buf.len() >= CHUNK_SIZE {
            let chunk: Vec<u8> = buf.drain(..CHUNK_SIZE).collect();
            hasher.update(&chunk);
            sink.send_chunk(item_id, offset, &chunk).await?;
            offset += CHUNK_SIZE as u64;
        }
    }
    if !buf.is_empty() {
        hasher.update(&buf);
        sink.send_chunk(item_id, offset, &buf).await?;
        offset += buf.len() as u64;
    }
    let digest = hasher.finalize().to_hex().to_string();
    sink.finish_item(item_id, offset, &digest).await?;
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mongodb::bson::doc;

    #[derive(Default)]
    struct RecordingSink {
        chunks: Vec<(u32, u64, Vec<u8>)>,
        finished: Vec<(u32, u64, String)>,
        closed: bool,
    }

    #[async_trait]
    impl ChunkSink for RecordingSink {
        async fn send_chunk(&mut self, item_id: u32, offset: u64, data: &[u8]) -> Result<()> {
            self.chunks.push((item_id, offset, data.to_vec()));
            Ok(())
        }
        async fn finish_item(&mut self, item_id: u32, total: u64, blake3: &str) -> Result<()> {
            self.finished.push((item_id, total, blake3.to_string()));
            Ok(())
        }
        async fn finish(&mut self) -> Result<()> {
            self.closed = true;
            Ok(())
        }
    }

    fn doc_stream(
        docs: Vec<Document>,
    ) -> impl Stream<Item = std::result::Result<Document, std::io::Error>> {
        futures_util::stream::iter(docs.into_iter().map(Ok))
    }

    #[tokio::test]
    async fn concatenates_and_hashes_documents() {
        let docs = vec![
            doc! { "_id": 1, "name": "a" },
            doc! { "_id": 2, "name": "b" },
        ];
        // Expected raw bytes: each document's BSON, concatenated.
        let mut expected = Vec::new();
        for d in &docs {
            expected.extend_from_slice(&mongodb::bson::to_vec(d).unwrap());
        }

        let mut sink = RecordingSink::default();
        let total = docs_to_sink(3, doc_stream(docs), &mut sink).await.unwrap();

        assert_eq!(total as usize, expected.len());
        let mut got = Vec::new();
        for c in &sink.chunks {
            got.extend_from_slice(&c.2);
        }
        assert_eq!(
            got, expected,
            "reassembled bytes must equal concatenated BSON"
        );
        assert_eq!(sink.finished.len(), 1);
        assert_eq!(sink.finished[0].0, 3);
        assert_eq!(sink.finished[0].1 as usize, expected.len());
        assert_eq!(sink.finished[0].2, rb_core::wire::blake3_hex(&expected));
        assert!(!sink.closed, "stream_out must not close the sink");
    }

    #[tokio::test]
    async fn empty_collection_finishes_item_with_zero() {
        let mut sink = RecordingSink::default();
        let total = docs_to_sink(7, doc_stream(vec![]), &mut sink)
            .await
            .unwrap();
        assert_eq!(total, 0);
        assert!(sink.chunks.is_empty());
        assert_eq!(sink.finished[0], (7, 0, rb_core::wire::blake3_hex(&[])));
    }

    #[tokio::test]
    async fn splits_large_payload_on_chunk_size() {
        // Many docs with a sizeable field so the concatenation exceeds CHUNK_SIZE.
        let blob = "x".repeat(1024);
        let docs: Vec<Document> = (0..2000).map(|i| doc! { "_id": i, "b": &blob }).collect();
        let mut expected = Vec::new();
        for d in &docs {
            expected.extend_from_slice(&mongodb::bson::to_vec(d).unwrap());
        }
        assert!(expected.len() > CHUNK_SIZE, "test must exceed one chunk");

        let mut sink = RecordingSink::default();
        let total = docs_to_sink(0, doc_stream(docs), &mut sink).await.unwrap();
        assert_eq!(total as usize, expected.len());
        // Every chunk but the last is exactly CHUNK_SIZE; offsets are contiguous.
        let mut off = 0u64;
        for (i, c) in sink.chunks.iter().enumerate() {
            assert_eq!(c.1, off, "offset of chunk {i}");
            if i + 1 < sink.chunks.len() {
                assert_eq!(c.2.len(), CHUNK_SIZE);
            } else {
                assert!(c.2.len() <= CHUNK_SIZE && !c.2.is_empty());
            }
            off += c.2.len() as u64;
        }
        assert_eq!(off as usize, expected.len());
    }
}
