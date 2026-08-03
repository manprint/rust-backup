//! Destination-side preflight + restore (plan Phase 3.4 / 3.5).
//!
//! [`validate`] gathers a [`DestProbe`] over a live connection then folds it into
//! a [`rb_core::plan::Preflight`] with the pure [`assess`] (unit-tested without a
//! server). [`stream_in`] applies the plan to reach 1:1: create collections
//! (+options) → bulk `insert_many` (unordered, batched) the streamed BSON →
//! build indexes. No temp files (I-NOTEMP).
//!
//! Users/roles are intentionally NOT recreated: `usersInfo` does not expose
//! password credentials, so faithful user restore is impossible from a logical
//! dump (documented limitation, mirrors PostgreSQL passwords). They are captured
//! in the plan for visibility only.

use std::collections::{HashMap, HashSet};

use mongodb::bson::{doc, Document};
use mongodb::IndexModel;

use rb_core::channel::{ChunkEvent, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{human_bytes, BackupPlan, Preflight};

use crate::model::MongoPlanPayload;
use crate::source::ItemMeta;
use crate::{MongoConnection, MongoDbParams};

/// Documents per `insert_many` call during restore.
const INSERT_BATCH: usize = 1000;

/// Facts gathered from the destination needed to assess the plan.
#[derive(Debug, Clone, Default)]
pub struct DestProbe {
    /// Destination server major version.
    pub dest_major: u32,
    /// Existing target collections as `"database.collection"`.
    pub existing_collections: HashSet<String>,
}

/// Preflight the plan against the destination.
pub async fn validate(params: &MongoDbParams, plan: &BackupPlan) -> Result<Preflight> {
    let payload: MongoPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Validate, format!("bad plan payload: {e}")))?;
    let probe = probe_dest(params, &payload).await?;
    Ok(assess(
        &payload,
        &probe,
        params.overwrite,
        plan.estimated_bytes,
    ))
}

/// Pure assessment: fold the probe + plan into a [`Preflight`].
pub fn assess(
    payload: &MongoPlanPayload,
    probe: &DestProbe,
    overwrite: bool,
    estimated_bytes: u64,
) -> Preflight {
    let mut pf = Preflight::pass();

    let version_ok = probe.dest_major >= payload.server_major;
    pf = pf.check(
        "server_version",
        version_ok,
        format!(
            "destination major {} vs source major {} (need destination ≥ source)",
            probe.dest_major, payload.server_major
        ),
    );

    for db in &payload.databases {
        for coll in &db.collections {
            let key = format!("{}.{}", db.name, coll.name);
            let exists = probe.existing_collections.contains(&key);
            let ok = !exists || overwrite;
            let detail = if !exists {
                format!("'{key}' absent — will be created")
            } else if overwrite {
                format!("'{key}' exists — will be replaced (--overwrite)")
            } else {
                format!("'{key}' already exists (use --overwrite to replace)")
            };
            pf = pf.check(format!("collection:{key}"), ok, detail);
        }
    }

    // Informational only: free disk is not visible over the wire protocol.
    pf = pf.check(
        "estimated_size",
        true,
        format!(
            "≈ {} to restore; destination free space is not verifiable remotely",
            human_bytes(estimated_bytes)
        ),
    );

    pf
}

async fn probe_dest(params: &MongoDbParams, payload: &MongoPlanPayload) -> Result<DestProbe> {
    let conn = MongoConnection::connect(params).await?;
    let mut existing = HashSet::new();
    for db in &payload.databases {
        let names = conn
            .client
            .database(&db.name)
            .list_collection_names()
            .await
            .map_err(|e| {
                BackupError::phase_src(Phase::Validate, format!("list collections {}", db.name), e)
            })?;
        for n in names {
            existing.insert(format!("{}.{}", db.name, n));
        }
    }
    Ok(DestProbe {
        dest_major: conn.server_major,
        existing_collections: existing,
    })
}

// --- restore (apply) ---------------------------------------------------------

/// Apply the streamed payload to reach 1:1 with the source (plan Phase 3.5).
pub async fn stream_in(
    params: &MongoDbParams,
    plan: &BackupPlan,
    src: &mut dyn ChunkSource,
) -> Result<()> {
    let payload: MongoPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Apply, format!("bad plan payload: {e}")))?;
    let conn = MongoConnection::connect(params).await?;

    // 1. Create every collection (with its options), dropping first on overwrite.
    for db in &payload.databases {
        let database = conn.client.database(&db.name);
        for coll in &db.collections {
            create_collection(&database, coll, params.overwrite).await?;
        }
    }

    // 2. Bulk data: one linear pass over the chunk stream, routed by item id.
    let metas = item_metas(plan);
    apply_data(&conn, &metas, src).await?;

    // 3. Indexes (after data load, so building is cheaper).
    for db in &payload.databases {
        let database = conn.client.database(&db.name);
        for coll in &db.collections {
            build_indexes(&database, coll).await?;
        }
    }
    Ok(())
}

/// Create one collection via the `create` command, merging its captured options.
/// On `overwrite`, drop any existing collection first (best-effort).
async fn create_collection(
    db: &mongodb::Database,
    coll: &crate::model::MongoCollection,
    overwrite: bool,
) -> Result<()> {
    if overwrite {
        // Best-effort: a NamespaceNotFound on a fresh destination is fine.
        let _ = db.collection::<Document>(&coll.name).drop().await;
    }

    let mut cmd = doc! { "create": &coll.name };
    if let Some(opts) = &coll.options {
        let opts_doc = mongodb::bson::to_document(opts).map_err(|e| {
            BackupError::phase_src(Phase::Apply, format!("decode options {}", coll.name), e)
        })?;
        cmd.extend(opts_doc);
    }
    db.run_command(cmd).await.map_err(|e| {
        BackupError::phase_src(Phase::Apply, format!("create collection {}", coll.name), e)
    })?;
    Ok(())
}

/// Build the captured (non-`_id_`) indexes for one collection.
async fn build_indexes(db: &mongodb::Database, coll: &crate::model::MongoCollection) -> Result<()> {
    if coll.indexes.is_empty() {
        return Ok(());
    }
    let collection = db.collection::<Document>(&coll.name);
    for spec in &coll.indexes {
        let model: IndexModel = serde_json::from_value(spec.clone()).map_err(|e| {
            BackupError::phase(Phase::Apply, format!("decode index of {}: {e}", coll.name))
        })?;
        collection.create_index(model).await.map_err(|e| {
            BackupError::phase_src(Phase::Apply, format!("create index on {}", coll.name), e)
        })?;
    }
    Ok(())
}

/// Map `item.id` → its descriptor (collection-kind items only).
fn item_metas(plan: &BackupPlan) -> HashMap<u32, ItemMeta> {
    plan.items
        .iter()
        .filter(|i| i.kind == "collection")
        .filter_map(|i| {
            serde_json::from_value::<ItemMeta>(i.meta.clone())
                .ok()
                .map(|m| (i.id, m))
        })
        .collect()
}

/// Drain the chunk stream, reassemble documents, and `insert_many` them in
/// batches into the routed collection.
async fn apply_data(
    conn: &MongoConnection,
    metas: &HashMap<u32, ItemMeta>,
    src: &mut dyn ChunkSource,
) -> Result<()> {
    let mut current: Option<CurrentItem> = None;

    loop {
        match src.next().await? {
            ChunkEvent::Chunk { item_id, data, .. } => {
                let reopen = current
                    .as_ref()
                    .map(|c| c.item_id != item_id)
                    .unwrap_or(true);
                if reopen {
                    if let Some(c) = current.take() {
                        c.finish(conn).await?;
                    }
                    let meta = metas.get(&item_id).ok_or_else(|| {
                        BackupError::phase(Phase::Apply, format!("data for unknown item {item_id}"))
                    })?;
                    current = Some(CurrentItem::new(item_id, meta));
                }
                if let Some(c) = &mut current {
                    c.feed(conn, &data).await?;
                }
            }
            ChunkEvent::ItemEnd { item_id, total, .. } => {
                let open = current.as_ref().ok_or_else(|| {
                    BackupError::phase(
                        Phase::Apply,
                        format!("ItemEnd item={item_id} without open collection"),
                    )
                })?;
                if open.item_id != item_id || open.bytes != total {
                    return Err(BackupError::phase(
                        Phase::Verify,
                        format!(
                            "ItemEnd item={item_id} does not match open item={} bytes={}",
                            open.item_id, open.bytes
                        ),
                    ));
                }
                if let Some(c) = current.take() {
                    c.finish(conn).await?;
                }
            }
            ChunkEvent::End => {
                if let Some(c) = current.take() {
                    c.finish(conn).await?;
                }
                break;
            }
        }
    }
    Ok(())
}

/// Per-item restore state: a byte accumulator for partial BSON across chunk
/// boundaries plus a pending insert batch destined for one collection.
struct CurrentItem {
    item_id: u32,
    database: String,
    collection: String,
    buf: Vec<u8>,
    batch: Vec<Document>,
    bytes: u64,
}

impl CurrentItem {
    fn new(item_id: u32, meta: &ItemMeta) -> Self {
        Self {
            item_id,
            database: meta.database.clone(),
            collection: meta.collection.clone(),
            buf: Vec::new(),
            batch: Vec::new(),
            bytes: 0,
        }
    }

    /// Append bytes, parse any whole documents, and flush full batches.
    async fn feed(&mut self, conn: &MongoConnection, data: &[u8]) -> Result<()> {
        self.bytes += data.len() as u64;
        self.buf.extend_from_slice(data);
        take_documents(&mut self.buf, &mut self.batch)?;
        while self.batch.len() >= INSERT_BATCH {
            let chunk: Vec<Document> = self.batch.drain(..INSERT_BATCH).collect();
            self.insert(conn, chunk).await?;
        }
        Ok(())
    }

    /// Flush the trailing batch and assert the byte accumulator is fully consumed.
    async fn finish(mut self, conn: &MongoConnection) -> Result<()> {
        take_documents(&mut self.buf, &mut self.batch)?;
        if !self.buf.is_empty() {
            return Err(BackupError::phase(
                Phase::Apply,
                format!(
                    "trailing {} bytes after last document in {}.{}",
                    self.buf.len(),
                    self.database,
                    self.collection
                ),
            ));
        }
        if !self.batch.is_empty() {
            let docs = std::mem::take(&mut self.batch);
            self.insert(conn, docs).await?;
        }
        Ok(())
    }

    async fn insert(&self, conn: &MongoConnection, docs: Vec<Document>) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        let coll = conn
            .client
            .database(&self.database)
            .collection::<Document>(&self.collection);
        coll.insert_many(docs).ordered(false).await.map_err(|e| {
            BackupError::phase_src(
                Phase::Apply,
                format!("insert_many {}.{}", self.database, self.collection),
                e,
            )
        })?;
        Ok(())
    }
}

/// Drain every complete BSON document from the front of `buf` into `out`,
/// leaving any partial trailing document in `buf`. BSON documents are
/// length-prefixed (first 4 bytes, little-endian), so they are self-delimiting.
fn take_documents(buf: &mut Vec<u8>, out: &mut Vec<Document>) -> Result<()> {
    let mut pos = 0usize;
    while buf.len() - pos >= 4 {
        let len = i32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        if len < 5 {
            return Err(BackupError::phase(
                Phase::Apply,
                format!("invalid BSON document length {len}"),
            ));
        }
        if buf.len() - pos < len {
            break; // partial document; wait for more bytes.
        }
        let doc: Document = mongodb::bson::from_slice(&buf[pos..pos + len])
            .map_err(|e| BackupError::phase_src(Phase::Apply, "decode bson document", e))?;
        out.push(doc);
        pos += len;
    }
    if pos > 0 {
        buf.drain(..pos);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    fn probe(major: u32, existing: &[&str]) -> DestProbe {
        DestProbe {
            dest_major: major,
            existing_collections: existing.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn passes_on_clean_destination() {
        let payload = crate::model::test_fixture(); // source major 6, appdb.accounts
        let pf = assess(&payload, &probe(6, &[]), false, 1024);
        assert!(
            pf.ok,
            "clean equal-version destination should pass: {:?}",
            pf.checks
        );
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "collection:appdb.accounts" && c.passed));
    }

    #[test]
    fn fails_when_destination_older() {
        let payload = crate::model::test_fixture();
        let pf = assess(&payload, &probe(5, &[]), false, 0);
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "server_version" && !c.passed));
    }

    #[test]
    fn existing_collection_blocks_unless_overwrite() {
        let payload = crate::model::test_fixture();
        let p = probe(6, &["appdb.accounts"]);

        let blocked = assess(&payload, &p, false, 0);
        assert!(
            !blocked.ok,
            "existing collection must block without overwrite"
        );
        assert!(blocked
            .checks
            .iter()
            .any(|c| c.name == "collection:appdb.accounts" && !c.passed));

        let allowed = assess(&payload, &p, true, 0);
        assert!(
            allowed.ok,
            "overwrite must allow restoring over existing collection"
        );
    }

    #[test]
    fn item_metas_indexes_collection_items_by_id() {
        let payload = crate::model::test_fixture();
        let bp = crate::introspect::build_plan(&payload, "t".to_string());
        let metas = item_metas(&bp);
        assert_eq!(metas.len(), 1);
        let m = metas.get(&0).expect("item 0");
        assert_eq!(m.database, "appdb");
        assert_eq!(m.collection, "accounts");
    }

    #[test]
    fn take_documents_splits_a_concatenated_stream() {
        let docs = vec![
            doc! { "_id": 1, "v": "alpha" },
            doc! { "_id": 2, "v": "beta" },
            doc! { "_id": 3, "v": "gamma" },
        ];
        let mut bytes = Vec::new();
        for d in &docs {
            bytes.extend_from_slice(&mongodb::bson::to_vec(d).unwrap());
        }

        // Feed the bytes one at a time: documents emerge only when complete, and
        // the leftover buffer never grows past a single partial document.
        let mut buf = Vec::new();
        let mut out = Vec::new();
        for b in &bytes {
            buf.push(*b);
            take_documents(&mut buf, &mut out).unwrap();
        }
        assert_eq!(out, docs, "all documents reassembled in order");
        assert!(buf.is_empty(), "no trailing bytes once the stream is whole");
    }

    #[test]
    fn take_documents_leaves_partial_tail() {
        let d = doc! { "_id": 1, "v": "x" };
        let bytes = mongodb::bson::to_vec(&d).unwrap();
        let mut buf = bytes[..bytes.len() - 1].to_vec(); // one byte short
        let mut out = Vec::new();
        take_documents(&mut buf, &mut out).unwrap();
        assert!(out.is_empty(), "incomplete document must not be parsed");
        assert_eq!(buf.len(), bytes.len() - 1, "partial bytes retained");
    }

    #[test]
    fn take_documents_rejects_bogus_length() {
        let mut buf = vec![1, 0, 0, 0]; // length=1, impossible (< 5)
        let mut out = Vec::new();
        assert!(take_documents(&mut buf, &mut out).is_err());
    }
}
