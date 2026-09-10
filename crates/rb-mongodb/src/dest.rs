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

/// MongoDB's own BSON document limit. A length prefix beyond it is a protocol
/// violation, never something to wait for more bytes on.
const MAX_BSON_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Refuse a namespace the plan must never name.
///
/// The source excludes system databases and `system.*` collections, but the
/// destination re-derives every name from the received plan — peer-controlled
/// input. Without this guard a plan naming `admin.system.users` would, under
/// `--overwrite`, have the restore DROP every account on the destination
/// cluster.
pub fn check_namespace(database: &str, collection: &str) -> Result<()> {
    let bad_db = database.is_empty()
        || crate::introspect::SYSTEM_DBS.contains(&database)
        || database.contains(['/', '\\', '.', ' ', '\0', '$', '"']);
    let bad_collection = collection.is_empty()
        || collection.starts_with("system.")
        || collection.contains('\0')
        || collection.contains('$');
    if bad_db || bad_collection {
        return Err(BackupError::phase(
            Phase::Validate,
            format!("refusing system or invalid namespace {database:?}.{collection:?}"),
        ));
    }
    Ok(())
}

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

    let namespace_faults: Vec<String> = payload
        .databases
        .iter()
        .flat_map(|db| {
            db.collections.iter().filter_map(|coll| {
                check_namespace(&db.name, &coll.name)
                    .err()
                    .map(|error| error.to_string())
            })
        })
        .collect();
    pf = pf.check(
        "namespaces",
        namespace_faults.is_empty(),
        if namespace_faults.is_empty() {
            "every planned namespace is a restorable user namespace".to_string()
        } else {
            namespace_faults.join("; ")
        },
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

    // `--overwrite` destroys every target namespace first, as one pass, before
    // the snapshot below. Doing it per collection inside the create loop — where
    // it used to live — recorded an overwritten collection as "already present",
    // so a run that failed afterwards kept a half-loaded replacement of the very
    // data the operator asked to replace and never reported it. Dropping first
    // also means a namespace the drop pass never reached still holds its
    // original contents, so the cleanup cannot remove something this run did not
    // touch.
    if params.overwrite {
        drop_target_namespaces(&conn, &payload).await?;
    }

    // Snapshot which target namespaces exist *before* this run creates any, so a
    // later failure removes exactly what this run brought into existence and
    // never a collection it found already there.
    let preexisting = existing_target_namespaces(&conn, &payload).await?;
    let outcome = restore_namespaces(params, &conn, &payload, plan, src).await;
    if let Err(error) = outcome {
        remove_partially_restored(&conn, &payload, &preexisting).await;
        return Err(error);
    }
    Ok(())
}

/// The restore proper. Split out of [`stream_in`] so every `?` funnels through
/// one error path that can undo what the run created.
async fn restore_namespaces(
    params: &MongoDbParams,
    conn: &MongoConnection,
    payload: &MongoPlanPayload,
    plan: &BackupPlan,
    src: &mut dyn ChunkSource,
) -> Result<()> {
    // 1. Create every collection (with its options), dropping first on overwrite.
    //    The namespace guard is re-applied here: preflight and apply are two
    //    separate exchanges, and only this one destroys anything.
    for db in &payload.databases {
        let database = conn.client.database(&db.name);
        for coll in &db.collections {
            check_namespace(&db.name, &coll.name)?;
            create_collection(&database, coll, params.overwrite).await?;
        }
    }

    // 2. Bulk data: one linear pass over the chunk stream, routed by item id.
    let metas = item_metas(plan);
    apply_data(conn, &metas, src).await?;

    // 3. Indexes (after data load, so building is cheaper).
    for db in &payload.databases {
        let database = conn.client.database(&db.name);
        for coll in &db.collections {
            build_indexes(&database, coll).await?;
        }
    }
    Ok(())
}

/// Drop every namespace the plan targets, before anything is created and before
/// the pre-existing snapshot is taken. Only reached with `--overwrite`; the
/// namespace guard is re-applied per collection so a hostile plan cannot make
/// this pass destroy a system namespace.
async fn drop_target_namespaces(conn: &MongoConnection, payload: &MongoPlanPayload) -> Result<()> {
    for db in &payload.databases {
        let database = conn.client.database(&db.name);
        for coll in &db.collections {
            check_namespace(&db.name, &coll.name)?;
            if let Err(error) = database.collection::<Document>(&coll.name).drop().await {
                if !is_namespace_not_found(&error) {
                    return Err(BackupError::phase_src(
                        Phase::Apply,
                        format!("drop existing collection {}.{}", db.name, coll.name),
                        error,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `db.collection` namespaces from the plan that already exist on the
/// destination, as `("db", "collection")` pairs.
async fn existing_target_namespaces(
    conn: &MongoConnection,
    payload: &MongoPlanPayload,
) -> Result<HashSet<(String, String)>> {
    let mut existing = HashSet::new();
    for db in &payload.databases {
        let database = conn.client.database(&db.name);
        let names = database.list_collection_names().await.map_err(|error| {
            BackupError::phase_src(
                Phase::Apply,
                format!("list existing collections in {}", db.name),
                error,
            )
        })?;
        let present: HashSet<&String> = names.iter().collect();
        for coll in &db.collections {
            if present.contains(&coll.name) {
                existing.insert((db.name.clone(), coll.name.clone()));
            }
        }
    }
    Ok(existing)
}

/// Namespaces a failed restore must remove: the plan's targets that this run
/// created. A namespace the run found already present is never touched — without
/// `--overwrite` the run never wrote to it, and with `--overwrite` it was dropped
/// before this snapshot was taken, so it cannot appear here.
fn namespaces_to_remove<'a>(
    targets: impl IntoIterator<Item = (&'a str, &'a str)>,
    preexisting: &HashSet<(String, String)>,
) -> Vec<(&'a str, &'a str)> {
    targets
        .into_iter()
        .filter(|(db, coll)| !preexisting.contains(&((*db).to_string(), (*coll).to_string())))
        .collect()
}

/// Drop the half-filled collections this run created. A restore that fails
/// mid-load would otherwise leave collections holding part of the source's
/// documents: nothing certifies them (there is no `RESTORE VERIFIED`), but they
/// are indistinguishable from small collections on inspection. Best-effort by
/// construction — the caller's error is the real outcome and must not be
/// replaced by a cleanup failure — so every problem here is logged loudly.
async fn remove_partially_restored(
    conn: &MongoConnection,
    payload: &MongoPlanPayload,
    preexisting: &HashSet<(String, String)>,
) {
    let targets = payload.databases.iter().flat_map(|db| {
        db.collections
            .iter()
            .map(move |coll| (db.name.as_str(), coll.name.as_str()))
    });
    for (db_name, coll_name) in namespaces_to_remove(targets, preexisting) {
        // The namespace guard already ran before this collection was created;
        // re-check it so a cleanup path can never be the one that drops a system
        // namespace.
        if check_namespace(db_name, coll_name).is_err() {
            continue;
        }
        let collection = conn
            .client
            .database(db_name)
            .collection::<Document>(coll_name);
        match collection.drop().await {
            Ok(()) => tracing::warn!(
                namespace = %format!("{db_name}.{coll_name}"),
                "restore failed; dropped the partially restored collection this run created"
            ),
            Err(error) if is_namespace_not_found(&error) => {}
            Err(error) => tracing::error!(
                namespace = %format!("{db_name}.{coll_name}"),
                %error,
                "restore failed and the partially restored collection could not be dropped; \
                 it holds an incomplete copy and no verification evidence — drop it before \
                 retrying"
            ),
        }
    }
}

/// Re-introspect each restored database and compare collection options and
/// indexes with the source plan. Users and volatile size/count estimates are
/// intentionally outside the restorable contract.
pub async fn verify_catalog(params: &MongoDbParams, plan: &BackupPlan) -> Result<()> {
    let mut expected: MongoPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Verify, format!("bad plan payload: {e}")))?;
    normalize_catalog(&mut expected);

    for expected_db in &expected.databases {
        let mut scoped = params.clone();
        scoped.database = Some(expected_db.name.clone());
        let mut actual = crate::introspect::introspect_cluster(&scoped)
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Verify,
                    format!("re-introspect MongoDB database {}", expected_db.name),
                    error,
                )
            })?;
        normalize_catalog(&mut actual);
        let actual_db = actual.databases.into_iter().next().ok_or_else(|| {
            BackupError::phase(
                Phase::Verify,
                format!(
                    "restored MongoDB database {:?} is missing",
                    expected_db.name
                ),
            )
        })?;
        if &actual_db != expected_db {
            return Err(BackupError::phase(
                Phase::Verify,
                format!(
                    "MongoDB catalog mismatch for database {:?}",
                    expected_db.name
                ),
            ));
        }
    }
    Ok(())
}

fn normalize_catalog(payload: &mut MongoPlanPayload) {
    payload.server_version.clear();
    payload.server_major = 0;
    payload.databases.sort_by(|a, b| a.name.cmp(&b.name));
    for database in &mut payload.databases {
        // Credentials are not available from a logical backup and are not
        // restored; this limitation is already enforced/documented separately.
        database.users.clear();
        database.collections.sort_by(|a, b| a.name.cmp(&b.name));
        for collection in &mut database.collections {
            collection.estimated_docs = 0;
            collection.estimated_bytes = 0;
            collection
                .indexes
                .sort_by_key(|value| serde_json::to_string(value).unwrap_or_default());
        }
    }
}

/// Create one collection via the `create` command, merging its captured options.
/// On `overwrite`, drop any existing collection first (best-effort).
async fn create_collection(
    db: &mongodb::Database,
    coll: &crate::model::MongoCollection,
    overwrite: bool,
) -> Result<()> {
    if overwrite {
        // A NamespaceNotFound on a fresh destination is fine; anything else is
        // a real failure that would otherwise resurface as an opaque
        // NamespaceExists from the `create` below.
        if let Err(error) = db.collection::<Document>(&coll.name).drop().await {
            if !is_namespace_not_found(&error) {
                return Err(BackupError::phase_src(
                    Phase::Apply,
                    format!("drop existing collection {}", coll.name),
                    error,
                ));
            }
        }
    }

    let mut cmd = doc! { "create": &coll.name };
    if let Some(opts) = &coll.options {
        // `bson::to_document` SERIALIZES the JSON value, and bson's serializer
        // has no `$`-key handling: an option containing `{"$date": …}` or
        // `{"$binary": …}` would be replayed to the server as a literal
        // sub-document. Deserializing into `Document` goes through bson's
        // extended-JSON-aware visitor instead, which is also what the index
        // path already does.
        let opts_doc: Document = serde_json::from_value(opts.clone()).map_err(|e| {
            BackupError::phase(Phase::Apply, format!("decode options {}: {e}", coll.name))
        })?;
        cmd.extend(opts_doc);
    }
    db.run_command(cmd).await.map_err(|e| {
        BackupError::phase_src(Phase::Apply, format!("create collection {}", coll.name), e)
    })?;
    Ok(())
}

/// Whether a driver error is MongoDB's `NamespaceNotFound` (code 26).
fn is_namespace_not_found(error: &mongodb::error::Error) -> bool {
    matches!(
        &*error.kind,
        mongodb::error::ErrorKind::Command(command) if command.code == 26
    )
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
                let open = current_for_item_end(current.take(), metas, item_id)?;
                if open.item_id != item_id || open.bytes != total {
                    return Err(BackupError::phase(
                        Phase::Verify,
                        format!(
                            "ItemEnd item={item_id} does not match open item={} bytes={}",
                            open.item_id, open.bytes
                        ),
                    ));
                }
                open.finish(conn).await?;
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

fn current_for_item_end(
    current: Option<CurrentItem>,
    metas: &HashMap<u32, ItemMeta>,
    item_id: u32,
) -> Result<CurrentItem> {
    if let Some(current) = current {
        return Ok(current);
    }
    let meta = metas.get(&item_id).ok_or_else(|| {
        BackupError::phase(
            Phase::Apply,
            format!("ItemEnd for unknown collection item={item_id}"),
        )
    })?;
    // An empty collection legitimately has ItemEnd without a preceding Chunk.
    Ok(CurrentItem::new(item_id, meta))
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
        // Ordered inserts: for a capped collection the insertion (natural)
        // order IS state — `find()` with no sort, `$natural` and tailable
        // cursors all read it, and it decides eviction order. An unordered bulk
        // write may reorder freely. Ordering also gives precise duplicate-key
        // attribution, which is worth more than the throughput it costs.
        coll.insert_many(docs).ordered(true).await.map_err(|e| {
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
        let declared = i32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        // A negative prefix sign-extends through `as usize` into ~2^64, and any
        // oversized prefix takes the "wait for more bytes" branch forever: the
        // accumulator then grows to the whole item, which OOMs the destination
        // and breaks I-NOTEMP. Bound it by MongoDB's own document limit.
        if declared < 5 || declared as usize > MAX_BSON_DOCUMENT_BYTES {
            return Err(BackupError::phase(
                Phase::Apply,
                format!("invalid BSON document length {declared}"),
            ));
        }
        let len = declared as usize;
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

    #[test]
    fn empty_collection_item_end_opens_zero_byte_restore_state() {
        let metas = HashMap::from([(
            7,
            ItemMeta {
                database: "appdb".into(),
                collection: "empty".into(),
            },
        )]);
        let current = current_for_item_end(None, &metas, 7).expect("empty collection state");
        assert_eq!(current.item_id, 7);
        assert_eq!(current.bytes, 0);
        assert!(current.buf.is_empty());
        assert!(current.batch.is_empty());
    }
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
        // A too-short prefix, a negative one (which sign-extends through
        // `as usize` into ~2^64) and one past MongoDB's own 16 MiB document
        // limit must all fail immediately. Taking the "wait for more bytes"
        // branch instead would buffer the whole item into the accumulator,
        // OOM the destination and break I-NOTEMP.
        let bogus: [Vec<u8>; 4] = [
            vec![1, 0, 0, 0],                      // 1, impossible (< 5)
            vec![0, 0, 0, 0],                      // 0
            vec![0xFF; 4],                         // -1
            0x7FFF_FFFFi32.to_le_bytes().to_vec(), // i32::MAX
        ];
        for prefix in bogus {
            let mut buf = prefix.clone();
            // Trailing bytes make sure the failure is not merely "too short".
            buf.extend_from_slice(&[0u8; 8]);
            let mut out = Vec::new();
            let error = take_documents(&mut buf, &mut out)
                .expect_err(&format!("prefix {prefix:?} must be refused"));
            assert!(
                format!("{error}").contains("invalid BSON document length"),
                "{error}"
            );
            assert!(out.is_empty());
        }
    }

    /// The destination re-derives every namespace from the received plan, which
    /// is peer-controlled. A plan naming `admin.system.users` under
    /// `--overwrite` would otherwise DROP every account on the cluster.
    #[test]
    fn system_and_malformed_namespaces_are_refused() {
        for (db, coll) in [
            ("admin", "system.users"),
            ("admin", "accounts"),
            ("local", "oplog.rs"),
            ("config", "shards"),
            ("appdb", "system.views"),
            ("", "accounts"),
            ("appdb", ""),
            ("app.db", "accounts"),
            ("app db", "accounts"),
            ("app$db", "accounts"),
            ("app\0db", "accounts"),
            ("app/db", "accounts"),
            ("app\\db", "accounts"),
            ("app\"db", "accounts"),
            ("appdb", "acc$ounts"),
            ("appdb", "acc\0unts"),
        ] {
            let error =
                check_namespace(db, coll).expect_err(&format!("{db:?}.{coll:?} must be refused"));
            assert!(
                format!("{error}").contains("refusing system or invalid namespace"),
                "{error}"
            );
        }
        // Dots are legal *inside* a collection name (`a.b.c` sub-collection
        // naming is idiomatic), so those must still pass.
        check_namespace("appdb", "accounts").expect("plain namespace");
        check_namespace("appdb", "events.2026.raw").expect("dotted collection");
    }

    #[test]
    fn a_failed_restore_removes_only_the_namespaces_it_created() {
        let preexisting: HashSet<(String, String)> = [("appdb".to_string(), "kept".to_string())]
            .into_iter()
            .collect();

        let removed = namespaces_to_remove(
            [
                ("appdb", "kept"),
                ("appdb", "created"),
                ("other", "created"),
            ],
            &preexisting,
        );

        assert_eq!(
            removed,
            vec![("appdb", "created"), ("other", "created")],
            "a collection the run found already present must survive its failure"
        );
    }

    #[test]
    fn a_restore_that_created_nothing_removes_nothing() {
        let preexisting: HashSet<(String, String)> = [
            ("appdb".to_string(), "accounts".to_string()),
            ("appdb".to_string(), "orders".to_string()),
        ]
        .into_iter()
        .collect();

        assert!(
            namespaces_to_remove([("appdb", "accounts"), ("appdb", "orders")], &preexisting)
                .is_empty(),
            "nothing was created, so nothing may be dropped"
        );
    }
}
