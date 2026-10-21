//! Source-side streaming (plan Phase 3.3).
//!
//! Per collection plan item, open a `find({})` cursor and push each document's
//! raw BSON (length-prefixed, self-delimiting) into the channel's [`ChunkSink`]
//! in full `CHUNK_SIZE` chunks — no temp files (I-NOTEMP). The documents are
//! forwarded exactly as the server sent them, never decoded. The byte stream is
//! consumer-paced: `send_chunk` rides the substream's flow control, so a slow
//! destination stalls these cursor reads (I-BANDWIDTH). Strictly read-only
//! (I-IMMUT): only `find` is issued.
//!
//! The re-chunking ([`ItemWriter`]) is decoupled from the driver so it is
//! unit-tested in-process; the live `find` integration is exercised by the
//! mongodb e2e.

use mongodb::bson::doc;
use serde::Deserialize;

use rb_core::channel::ChunkSink;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::BackupPlan;
use rb_core::wire::CHUNK_SIZE;

use crate::connect::{sort_by_id, ReadOnlyConnection};
use crate::MongoDbParams;

/// The per-item descriptor carried in `PlanItem::meta` (set by `build_plan`,
/// consumed by both source `stream_out` and destination `stream_in`). On the
/// destination it is peer-controlled: `dest::resolve_items` admits it only when
/// it names a namespace the plan's catalog declares.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
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
    let conn = ReadOnlyConnection::connect(params).await?;

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

        // A stable order makes the source commitment reproducible when the
        // destination is read back after restore. `_id` is unique and indexed.
        let mut cursor = conn
            .client
            .database(&meta.database)
            .find(&meta.collection, doc! {}, Some(sort_by_id()))
            .await
            .map_err(|e| {
                BackupError::phase_src(Phase::Transfer, format!("find {}", item.name), e)
            })?;
        let mut out = ItemWriter::new(item.id, sink);
        // `advance` + `current` lend the document in the driver's own batch
        // buffer: nothing is allocated or decoded per document.
        while cursor.advance().await.map_err(|e| {
            BackupError::phase_src(Phase::Transfer, format!("read {}", item.name), e)
        })? {
            out.write(cursor.current().as_bytes()).await?;
        }
        out.finish().await?;
    }
    Ok(())
}

/// Re-chunks one item's concatenated documents into full `CHUNK_SIZE` wire
/// chunks with running offsets, hashing the whole item, then emits
/// `finish_item`.
///
/// Each document is length-prefixed by BSON itself, so the stream is
/// self-delimiting and the destination can split it back into documents even
/// though chunk boundaries fall anywhere. One buffer is reused for the whole
/// item.
struct ItemWriter<'a> {
    item_id: u32,
    sink: &'a mut dyn ChunkSink,
    buf: Vec<u8>,
    offset: u64,
    hasher: blake3::Hasher,
}

impl<'a> ItemWriter<'a> {
    fn new(item_id: u32, sink: &'a mut dyn ChunkSink) -> Self {
        Self {
            item_id,
            sink,
            buf: Vec::with_capacity(CHUNK_SIZE),
            offset: 0,
            hasher: blake3::Hasher::new(),
        }
    }

    /// Append `bytes`, sending every chunk they complete.
    async fn write(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let take = (CHUNK_SIZE - self.buf.len()).min(bytes.len());
            let (head, rest) = bytes.split_at(take);
            self.buf.extend_from_slice(head);
            bytes = rest;
            if self.buf.len() == CHUNK_SIZE {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.hasher.update(&self.buf);
        self.sink
            .send_chunk(self.item_id, self.offset, &self.buf)
            .await?;
        self.offset += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Send the short trailing chunk, if any, and close the item. Returns the
    /// item's total byte count.
    async fn finish(mut self) -> Result<u64> {
        if !self.buf.is_empty() {
            self.flush().await?;
        }
        let digest = self.hasher.finalize().to_hex().to_string();
        self.sink
            .finish_item(self.item_id, self.offset, &digest)
            .await?;
        Ok(self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mongodb::bson::{doc, Document};

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

    /// What the cursor would lend `stream_out`, one document at a time.
    async fn write_all(item_id: u32, docs: &[Document], sink: &mut dyn ChunkSink) -> Result<u64> {
        let mut out = ItemWriter::new(item_id, sink);
        for doc in docs {
            out.write(&mongodb::bson::to_vec(doc).unwrap()).await?;
        }
        out.finish().await
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
        let total = write_all(3, &docs, &mut sink).await.unwrap();

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
        let total = write_all(7, &[], &mut sink).await.unwrap();
        assert_eq!(total, 0);
        assert!(sink.chunks.is_empty());
        assert_eq!(sink.finished[0], (7, 0, rb_core::wire::blake3_hex(&[])));
    }

    #[tokio::test]
    async fn a_document_larger_than_a_chunk_spans_full_chunks() {
        let docs = vec![
            doc! { "_id": 1, "b": "z".repeat(CHUNK_SIZE * 2 + 17) },
            doc! { "_id": 2 },
        ];
        let expected: Vec<u8> = docs
            .iter()
            .flat_map(|d| mongodb::bson::to_vec(d).unwrap())
            .collect();
        let mut sink = RecordingSink::default();
        let total = write_all(5, &docs, &mut sink).await.unwrap();
        assert_eq!(total as usize, expected.len());
        let sizes: Vec<usize> = sink.chunks.iter().map(|c| c.2.len()).collect();
        assert_eq!(
            sizes,
            vec![CHUNK_SIZE, CHUNK_SIZE, expected.len() - 2 * CHUNK_SIZE],
            "every chunk but the last is full"
        );
        let got: Vec<u8> = sink.chunks.iter().flat_map(|c| c.2.clone()).collect();
        assert_eq!(got, expected);
        assert_eq!(sink.finished[0].2, rb_core::wire::blake3_hex(&expected));
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
        let total = write_all(0, &docs, &mut sink).await.unwrap();
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

/// The raw path forwards documents exactly as the server stores them, where
/// older peers decoded each into a `Document` and re-encoded it. Item digests
/// are computed over those bytes on both sides of a run, so the two must agree
/// for every BSON type or a mixed-version pair could never verify.
#[cfg(test)]
mod raw_bson {
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::{
        doc, oid::ObjectId, Binary, Bson, DateTime, Decimal128, Document, JavaScriptCodeWithScope,
        RawDocumentBuf, Regex, Timestamp,
    };

    fn every_type() -> Document {
        doc! {
            "_id": ObjectId::parse_str("65f1c0ffee0000000000abcd").unwrap(),
            "double": 1.5,
            "negative_zero": -0.0_f64,
            "nan": f64::NAN,
            "string": "żółw 🐢",
            "document": { "nested": { "deeper": [1, 2, 3] } },
            "array": [1, "two", { "three": 3 }, [4]],
            "binary": Binary { subtype: BinarySubtype::Generic, bytes: vec![0, 1, 2, 255] },
            "uuid": Binary { subtype: BinarySubtype::Uuid, bytes: vec![7; 16] },
            "old_binary": Binary { subtype: BinarySubtype::BinaryOld, bytes: vec![5, 6] },
            "user_binary": Binary { subtype: BinarySubtype::UserDefined(0x80), bytes: vec![9] },
            "undefined": Bson::Undefined,
            "true": true,
            "false": false,
            "datetime": DateTime::from_millis(-62_135_596_800_000),
            "null": Bson::Null,
            "regex": Regex { pattern: "^a.*b$".to_string(), options: "imsx".to_string() },
            "code": Bson::JavaScriptCode("function () { return 1; }".to_string()),
            "symbol": Bson::Symbol("sym".to_string()),
            "code_with_scope": JavaScriptCodeWithScope {
                code: "x".to_string(),
                scope: doc! { "x": 1 },
            },
            "int32": i32::MIN,
            "timestamp": Timestamp { time: 1_700_000_000, increment: 7 },
            "int64": i64::MAX,
            "decimal": Decimal128::from_bytes([3; 16]),
            "min_key": Bson::MinKey,
            "max_key": Bson::MaxKey,
        }
    }

    #[test]
    fn older_peers_reencode_every_bson_type_byte_for_byte() {
        let raw = mongodb::bson::to_vec(&every_type()).unwrap();
        let decoded: Document = mongodb::bson::from_slice(&raw).unwrap();
        assert_eq!(
            mongodb::bson::to_vec(&decoded).unwrap(),
            raw,
            "decode + re-encode must reproduce the raw bytes"
        );
    }

    /// `insert_many::<RawDocumentBuf>` serializes each document through the
    /// raw serializer; it must copy the bytes, not rebuild them.
    #[test]
    fn the_raw_insert_path_copies_documents_verbatim() {
        let raw =
            RawDocumentBuf::from_bytes(mongodb::bson::to_vec(&every_type()).unwrap()).unwrap();
        let serialized = mongodb::bson::to_raw_document_buf(&raw).unwrap();
        assert_eq!(serialized.as_bytes(), raw.as_bytes());

        let duplicate = duplicate_key_document();
        let raw = RawDocumentBuf::from_bytes(duplicate.clone()).unwrap();
        assert_eq!(
            mongodb::bson::to_raw_document_buf(&raw).unwrap().as_bytes(),
            duplicate
        );
    }

    /// `{_id: 1, a: 1, a: 2}` encoded by hand: no encoder of ours can write it.
    fn duplicate_key_document() -> Vec<u8> {
        let mut body = Vec::new();
        for (key, value) in [("_id", 1i32), ("a", 1), ("a", 2)] {
            body.push(0x10);
            body.extend_from_slice(key.as_bytes());
            body.push(0);
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.push(0);
        let mut bytes = ((body.len() + 4) as i32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&body);
        bytes
    }

    /// The one divergence, and why the raw path exists: a `Document` is a map,
    /// so decoding merges duplicate field names and the old copy silently lost
    /// a value — then verified, since both sides lost it the same way.
    #[test]
    fn decoding_merged_duplicate_field_names() {
        let raw = duplicate_key_document();
        let decoded: Document = mongodb::bson::from_slice(&raw).unwrap();
        assert_eq!(decoded.len(), 2, "{decoded:?}");
        assert_ne!(mongodb::bson::to_vec(&decoded).unwrap(), raw);
    }
}
