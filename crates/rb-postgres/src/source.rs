//! Source-side streaming (plan Phase 2.4).
//!
//! Per data-bearing plan item, run `COPY <table> (<cols>) TO STDOUT (FORMAT
//! binary)` and push the raw COPY-binary bytes into the channel's [`ChunkSink`]
//! in ≤ `CHUNK_SIZE` chunks — no temp files (I-NOTEMP). The byte stream is
//! consumer-paced: `send_chunk` rides the substream's flow control, so a slow
//! destination stalls these `COPY` reads (I-BANDWIDTH). Strictly read-only on the
//! source (I-IMMUT).
//!
//! The re-chunking is decoupled from `tokio-postgres` (generic over any byte
//! stream) so it is unit-tested in-process; the live `copy_out` integration is
//! exercised by the postgres e2e.

use bytes::Bytes;
use futures_util::Stream;
use serde::Deserialize;

use rb_core::channel::ChunkSink;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::BackupPlan;
use rb_core::wire::CHUNK_SIZE;

use crate::ddl::{quote_ident, quote_qualified};
use crate::{PgConnection, PostgresParams};

/// The per-item COPY descriptor carried in `PlanItem::meta` (set by `build_plan`,
/// consumed by both source `stream_out` and destination `stream_in`).
#[derive(Debug, Deserialize)]
pub(crate) struct ItemMeta {
    pub(crate) database: String,
    pub(crate) schema: String,
    pub(crate) table: String,
    #[serde(default)]
    pub(crate) columns: Vec<String>,
    /// Read with `FROM ONLY`, set for a table that has classic inheritance
    /// children. A plain `SELECT` on such a parent expands into its children,
    /// so the parent's item would carry their rows as well — and, since an
    /// inheritance child is its own item too, the destination received every
    /// child row twice.
    #[serde(default)]
    pub(crate) only: bool,
}

/// Stream every data-bearing item of `plan` into `sink`. Does NOT close the sink
/// (the session calls `finish` once all items are done).
pub async fn stream_out(
    params: &PostgresParams,
    plan: &BackupPlan,
    sink: &mut dyn ChunkSink,
) -> Result<()> {
    // Reuse one connection across consecutive items in the same database (the
    // plan lists items grouped by database).
    let mut conn: Option<(String, PgConnection)> = None;

    for item in &plan.items {
        if item.kind != "table" {
            continue;
        }
        let meta: ItemMeta = serde_json::from_value(item.meta.clone()).map_err(|e| {
            BackupError::phase(
                Phase::Transfer,
                format!("bad item meta '{}': {e}", item.name),
            )
        })?;

        let stale = conn
            .as_ref()
            .map(|(db, _)| db != &meta.database)
            .unwrap_or(true);
        if stale {
            let c = PgConnection::connect_read_only(params, &meta.database).await?;
            conn = Some((meta.database.clone(), c));
        }

        if let Some((_, c)) = &conn {
            let copy_sql = copy_out_sql(&meta);
            let stream = c.client.copy_out(&copy_sql).await.map_err(|e| {
                BackupError::phase_src(Phase::Transfer, format!("copy_out {}", item.name), e)
            })?;
            copy_stream_to_sink(item.id, stream, sink).await?;
        }
    }
    Ok(())
}

/// Build the binary `COPY … TO STDOUT` statement for an item. Uses an explicit
/// column list (so it matches the destination's `COPY … FROM STDIN` column list)
/// unless none were captured.
fn copy_out_sql(meta: &ItemMeta) -> String {
    let qual = quote_qualified(&meta.schema, &meta.table);
    if meta.columns.is_empty() {
        // A table with no COPY-able columns (`CREATE TABLE t ();`) still has
        // rows and can still be a classic-inheritance parent. `COPY t TO` has no
        // `ONLY` form, so the parent's item silently streamed its children's
        // rows too and the restore duplicated them into the parent — the
        // read-back then failed with an integrity error on a perfectly legal
        // source. `SELECT` with an empty select list is the zero-column
        // counterpart of the column list below and matches the destination's
        // column-less `COPY … FROM STDIN`.
        let only = if meta.only { "ONLY " } else { "" };
        format!("COPY (SELECT FROM {only}{qual}) TO STDOUT (FORMAT binary)")
    } else {
        let cols = meta
            .columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        // `ONLY` for an inheritance parent; never for a partitioned one, whose
        // rows live entirely in its partitions.
        let only = if meta.only { "ONLY " } else { "" };
        // Heap order is not a database contract. Sort by the C-collated record
        // representation so source streaming and destination read-back produce
        // the same commitment independently of physical row placement. Equal
        // rows have equal COPY bytes, so duplicate ordering is irrelevant.
        format!(
            "COPY (SELECT {cols} FROM {only}{qual} ORDER BY (ROW({cols})::text) COLLATE \"C\") TO STDOUT (FORMAT binary)"
        )
    }
}

/// Re-chunk a byte stream into ≤ `CHUNK_SIZE` pieces, push each to `sink` with a
/// running offset, hash the whole item, and emit `finish_item`. Returns the total
/// byte count. Generic over the stream so it is unit-testable without a live DB.
async fn copy_stream_to_sink<E, St>(
    item_id: u32,
    stream: St,
    sink: &mut dyn ChunkSink,
) -> Result<u64>
where
    St: Stream<Item = std::result::Result<Bytes, E>>,
    E: std::fmt::Display,
{
    use futures_util::StreamExt;

    tokio::pin!(stream);
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
    let mut offset: u64 = 0;
    let mut hasher = blake3::Hasher::new();

    while let Some(frame) = stream.next().await {
        let bytes =
            frame.map_err(|e| BackupError::phase(Phase::Transfer, format!("copy_out: {e}")))?;
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

    #[test]
    fn copy_sql_with_and_without_columns() {
        let m = ItemMeta {
            database: "d".into(),
            schema: "app".into(),
            table: "accounts".into(),
            columns: vec!["id".into(), "email".into()],
            only: false,
        };
        assert_eq!(
            copy_out_sql(&m),
            "COPY (SELECT \"id\", \"email\" FROM \"app\".\"accounts\" ORDER BY (ROW(\"id\", \"email\")::text) COLLATE \"C\") TO STDOUT (FORMAT binary)"
        );
        let m2 = ItemMeta {
            columns: vec![],
            ..m
        };
        assert_eq!(
            copy_out_sql(&m2),
            "COPY (SELECT FROM \"app\".\"accounts\") TO STDOUT (FORMAT binary)"
        );
    }

    /// The `ONLY` rule holds for a table with no COPY-able columns too. A
    /// zero-column inheritance parent read without it streams its children's
    /// rows as its own, and the restore duplicates them into the parent.
    #[test]
    fn a_zero_column_inheritance_parent_is_still_read_with_only() {
        let m = ItemMeta {
            database: "d".into(),
            schema: "app".into(),
            table: "marker".into(),
            columns: vec![],
            only: true,
        };
        assert_eq!(
            copy_out_sql(&m),
            "COPY (SELECT FROM ONLY \"app\".\"marker\") TO STDOUT (FORMAT binary)"
        );
    }

    /// An inheritance parent must be read with `ONLY`: a plain `SELECT` on it
    /// expands into its children, whose rows are already their own items.
    #[test]
    fn an_inheritance_parent_is_read_with_only() {
        let m = ItemMeta {
            database: "d".into(),
            schema: "app".into(),
            table: "log".into(),
            columns: vec!["id".into()],
            only: true,
        };
        assert_eq!(
            copy_out_sql(&m),
            "COPY (SELECT \"id\" FROM ONLY \"app\".\"log\" ORDER BY (ROW(\"id\")::text) COLLATE \"C\") TO STDOUT (FORMAT binary)"
        );
    }

    #[tokio::test]
    async fn chunks_split_offsets_and_hash() {
        let big = vec![7u8; 2_500_000];
        let small = vec![9u8; 10];
        let mut expected = big.clone();
        expected.extend_from_slice(&small);

        let frames: Vec<std::result::Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from(big)), Ok(Bytes::from(small))];
        let mut sink = RecordingSink::default();
        let total = copy_stream_to_sink(5, futures_util::stream::iter(frames), &mut sink)
            .await
            .expect("stream");

        assert_eq!(total, 2_500_010);
        let sizes: Vec<usize> = sink.chunks.iter().map(|c| c.2.len()).collect();
        assert_eq!(
            sizes,
            vec![CHUNK_SIZE, CHUNK_SIZE, 2_500_010 - 2 * CHUNK_SIZE]
        );
        let offsets: Vec<u64> = sink.chunks.iter().map(|c| c.1).collect();
        assert_eq!(offsets, vec![0, CHUNK_SIZE as u64, 2 * CHUNK_SIZE as u64]);
        assert!(sink.chunks.iter().all(|c| c.0 == 5));

        let mut got = Vec::new();
        for c in &sink.chunks {
            got.extend_from_slice(&c.2);
        }
        assert_eq!(got, expected, "reassembled bytes must equal the input");

        assert_eq!(sink.finished.len(), 1);
        assert_eq!(sink.finished[0].0, 5);
        assert_eq!(sink.finished[0].1, 2_500_010);
        assert_eq!(sink.finished[0].2, rb_core::wire::blake3_hex(&expected));
        assert!(!sink.closed, "stream_out must not close the sink");
    }

    #[tokio::test]
    async fn empty_stream_finishes_item_with_zero() {
        let frames: Vec<std::result::Result<Bytes, std::io::Error>> = vec![];
        let mut sink = RecordingSink::default();
        let total = copy_stream_to_sink(1, futures_util::stream::iter(frames), &mut sink)
            .await
            .expect("stream");
        assert_eq!(total, 0);
        assert!(sink.chunks.is_empty());
        assert_eq!(sink.finished[0].0, 1);
        assert_eq!(sink.finished[0].1, 0);
        assert_eq!(sink.finished[0].2, rb_core::wire::blake3_hex(&[]));
    }
}
