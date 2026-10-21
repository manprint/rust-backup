//! Destination-side preflight + restore (plan Phase 3.4 / 3.5).
//!
//! [`validate`] gathers a [`DestProbe`] over a live connection then folds it into
//! a [`rb_core::plan::Preflight`] with the pure [`assess`] (unit-tested without a
//! server). [`stream_in`] applies the plan to reach 1:1: create collections
//! (+options) → bulk `insert_many` (ordered, batched, pipelined) the streamed
//! raw BSON → build indexes. No temp files (I-NOTEMP).
//!
//! Users/roles are intentionally NOT recreated: `usersInfo` does not expose
//! password credentials, so faithful user restore is impossible from a logical
//! dump (documented limitation, mirrors PostgreSQL passwords). They are captured
//! in the plan for visibility only.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use mongodb::bson::{doc, Document, RawDocumentBuf};
use mongodb::IndexModel;
use tokio::sync::mpsc;

use rb_core::channel::{ChunkEvent, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{human_bytes, summarize, BackupPlan, Preflight};

use crate::connect::ReadOnlyConnection;
use crate::model::{MongoCollection, MongoPlanPayload};
use crate::source::ItemMeta;
use crate::{MongoConnection, MongoDbParams};

/// Upper bound on the documents of one `insert_many` call.
const INSERT_BATCH_DOCS: usize = 10_000;

/// Upper bound on the BSON bytes of one `insert_many` call; a batch always takes
/// at least one document, which may reach [`MAX_BSON_DOCUMENT_BYTES`] alone. It
/// is a memory bound, not a protocol one — the driver already splits a call at
/// the server's message limit. A count alone held up to 1000 documents of up to
/// 16 MiB each, about 16 GiB, in destination memory.
const INSERT_BATCH_BYTES: usize = 8 * 1024 * 1024;

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

/// Map every collection item of `plan` to the namespace its documents restore
/// into, or list every reason the items cannot be trusted.
///
/// `PlanItem::meta` is peer-controlled, exactly like the catalog, and it is what
/// routes the data: a plan whose catalog is clean could otherwise point an
/// item's documents at `admin.system.users`, or at a collection the destination
/// already holds — past the namespace guard, past the existence check (both
/// read the catalog only) and outside the set a failed run cleans up. So an
/// item is accepted only when it names a namespace the catalog declares, each
/// catalog collection is fed by exactly one item, and nothing else is in the
/// plan.
pub(crate) fn resolve_items(
    plan: &BackupPlan,
    payload: &MongoPlanPayload,
) -> std::result::Result<HashMap<u32, ItemMeta>, Vec<String>> {
    let mut faults = Vec::new();
    let mut catalog: HashMap<ItemMeta, Option<u32>> = HashMap::new();
    for db in &payload.databases {
        for coll in &db.collections {
            let namespace = ItemMeta {
                database: db.name.clone(),
                collection: coll.name.clone(),
            };
            if catalog.insert(namespace, None).is_some() {
                faults.push(format!("the catalog lists {}.{} twice", db.name, coll.name));
            }
        }
    }

    let mut targets = HashMap::new();
    for item in &plan.items {
        if item.kind != "collection" {
            faults.push(format!("item {} has unknown kind {:?}", item.id, item.kind));
            continue;
        }
        let meta: ItemMeta = match serde_json::from_value(item.meta.clone()) {
            Ok(meta) => meta,
            Err(error) => {
                faults.push(format!("item {} has unreadable meta: {error}", item.id));
                continue;
            }
        };
        if let Err(error) = check_namespace(&meta.database, &meta.collection) {
            faults.push(format!("item {}: {error}", item.id));
            continue;
        }
        let namespace = format!("{}.{}", meta.database, meta.collection);
        match catalog.get_mut(&meta) {
            None => faults.push(format!(
                "item {} targets {namespace}, which the plan's catalog does not declare",
                item.id
            )),
            Some(Some(first)) => faults.push(format!(
                "items {first} and {} both target {namespace}",
                item.id
            )),
            Some(owner) => {
                *owner = Some(item.id);
                if targets.insert(item.id, meta).is_some() {
                    faults.push(format!("item id {} appears twice", item.id));
                }
            }
        }
    }
    let mut unfed: Vec<String> = catalog
        .iter()
        .filter(|(_, owner)| owner.is_none())
        .map(|(namespace, _)| {
            format!(
                "{}.{} has no plan item",
                namespace.database, namespace.collection
            )
        })
        .collect();
    unfed.sort();
    faults.extend(unfed);

    if faults.is_empty() {
        Ok(targets)
    } else {
        Err(faults)
    }
}

/// [`resolve_items`] as a phase-tagged refusal, for the exchanges that act on
/// the plan rather than merely report on it.
pub(crate) fn require_items(
    plan: &BackupPlan,
    payload: &MongoPlanPayload,
    phase: Phase,
) -> Result<HashMap<u32, ItemMeta>> {
    resolve_items(plan, payload).map_err(|faults| {
        BackupError::phase(
            phase,
            format!("refusing the plan's items: {}", summarize(&faults)),
        )
    })
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
    Ok(assess(plan, &payload, &probe, params.overwrite))
}

/// Pure assessment: fold the probe + plan into a [`Preflight`].
pub fn assess(
    plan: &BackupPlan,
    payload: &MongoPlanPayload,
    probe: &DestProbe,
    overwrite: bool,
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

    let item_faults = resolve_items(plan, payload).err().unwrap_or_default();
    pf = pf.check(
        "plan_items",
        item_faults.is_empty(),
        if item_faults.is_empty() {
            "every plan item restores into exactly one namespace the catalog declares".to_string()
        } else {
            summarize(&item_faults)
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
            human_bytes(plan.estimated_bytes)
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
    // Re-checked here, before the first write: preflight and apply are two
    // separate exchanges, and only this one acts on the items.
    let targets = require_items(plan, &payload, Phase::Validate)?;
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
    let outcome = restore_namespaces(params, &conn, &payload, &targets, src).await;
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
    targets: &HashMap<u32, ItemMeta>,
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
    apply_data(&InsertMany(&conn.client), targets, src).await?;

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

/// Re-read the catalog of every restored collection and compare its options
/// and indexes with the source plan. Only the planned collections are read: the
/// destination database may legitimately hold others (preflight only refuses a
/// collision), and a view among them must not fail the run either. Users and
/// volatile size/count estimates are outside the restorable contract.
pub async fn verify_catalog(params: &MongoDbParams, plan: &BackupPlan) -> Result<()> {
    let expected: MongoPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Verify, format!("bad plan payload: {e}")))?;
    // The read-back that follows routes by the same item meta.
    require_items(plan, &expected, Phase::Verify)?;

    let conn = ReadOnlyConnection::connect(params).await.map_err(|error| {
        BackupError::phase_src(
            Phase::Verify,
            "reconnect to read the restored catalog",
            error,
        )
    })?;
    let mut faults = Vec::new();
    for expected_db in &expected.databases {
        let planned: HashSet<&str> = expected_db
            .collections
            .iter()
            .map(|coll| coll.name.as_str())
            .collect();
        let restored =
            crate::introspect::introspect_collections(&conn, &expected_db.name, |name| {
                planned.contains(name)
            })
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Verify,
                    format!("re-introspect MongoDB database {}", expected_db.name),
                    error,
                )
            })?;
        faults.extend(catalog_faults(
            &expected_db.name,
            &expected_db.collections,
            restored,
        ));
    }
    if !faults.is_empty() {
        return Err(BackupError::phase(
            Phase::Verify,
            format!("MongoDB catalog mismatch: {}", summarize(&faults)),
        ));
    }
    Ok(())
}

/// What differs between the planned collections of one database and what the
/// destination now holds under those names.
fn catalog_faults(
    database: &str,
    expected: &[MongoCollection],
    restored: crate::introspect::Collections,
) -> Vec<String> {
    let mut faults: Vec<String> = restored
        .skipped
        .iter()
        .map(|namespace| format!("{namespace} is not an ordinary collection"))
        .collect();
    let mut restored: HashMap<String, MongoCollection> = restored
        .collections
        .into_iter()
        .map(|coll| (coll.name.clone(), coll))
        .collect();
    for want in expected {
        let namespace = format!("{database}.{}", want.name);
        let Some(got) = restored.remove(&want.name) else {
            let not_ordinary = format!("{namespace} (");
            if !faults.iter().any(|fault| fault.starts_with(&not_ordinary)) {
                faults.push(format!("{namespace} is missing"));
            }
            continue;
        };
        if got.options != want.options {
            faults.push(format!("{namespace} has different collection options"));
        }
        let (want_indexes, got_indexes) = (index_specs(&want.indexes), index_specs(&got.indexes));
        if want_indexes != got_indexes {
            faults.push(format!(
                "{namespace} has different indexes (planned: {}; restored: {})",
                index_names(&want.indexes),
                index_names(&got.indexes)
            ));
        }
    }
    faults
}

/// Index specs in a canonical order, so the comparison does not depend on the
/// order the server lists them in.
fn index_specs(indexes: &[serde_json::Value]) -> Vec<String> {
    let mut specs: Vec<String> = indexes.iter().map(|spec| spec.to_string()).collect();
    specs.sort();
    specs
}

fn index_names(indexes: &[serde_json::Value]) -> String {
    let mut names: Vec<&str> = indexes
        .iter()
        .map(|spec| {
            spec.get("name")
                .and_then(|name| name.as_str())
                .unwrap_or("?")
        })
        .collect();
    names.sort_unstable();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
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

/// One `insert_many` call: documents bound for one collection, in stream order.
struct Batch {
    target: Arc<ItemMeta>,
    docs: Vec<RawDocumentBuf>,
}

/// Where restored batches go. The live writer is [`InsertMany`]; the tests
/// substitute a recorder to pin the pipeline's ordering and failure rules
/// without a server.
#[async_trait]
trait BatchWriter: Sync {
    async fn write(&self, batch: Batch) -> Result<()>;
}

/// The live [`BatchWriter`].
struct InsertMany<'a>(&'a mongodb::Client);

#[async_trait]
impl BatchWriter for InsertMany<'_> {
    async fn write(&self, batch: Batch) -> Result<()> {
        let Batch { target, docs } = batch;
        let collection = self
            .0
            .database(&target.database)
            .collection::<RawDocumentBuf>(&target.collection);
        // Ordered inserts: for a capped collection the insertion (natural)
        // order IS state — `find()` with no sort, `$natural` and tailable
        // cursors all read it, and it decides eviction order. An unordered bulk
        // write may reorder freely. Ordering also gives precise duplicate-key
        // attribution, which is worth more than the throughput it costs.
        collection
            .insert_many(docs)
            .ordered(true)
            .await
            .map(drop)
            .map_err(|e| {
                BackupError::phase_src(
                    Phase::Apply,
                    format!("insert_many {}.{}", target.database, target.collection),
                    e,
                )
            })
    }
}

/// Restore the chunk stream. A reader reassembles documents and cuts them into
/// batches while the writer inserts the previous batch, so receiving batch N+1
/// overlaps the server's work on batch N; one batch waits in between. Batches
/// stay in stream order — one writer, one insert in flight — which is what
/// keeps a capped collection's natural order.
///
/// Neither side is ever dropped mid-operation: `join!`, not `try_join!`. A
/// dropped driver future does not stop the server, and an insert that landed
/// after the cleanup's `drop` would silently re-create the collection with part
/// of its documents. So a failing reader raises `stop` and waits for the insert
/// in flight; a failing writer closes its end, which stops the reader at once
/// even while it waits on the network.
async fn apply_data(
    writer: &dyn BatchWriter,
    targets: &HashMap<u32, ItemMeta>,
    src: &mut dyn ChunkSource,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<Batch>(1);
    let stop = AtomicBool::new(false);
    let stop = &stop;
    let read = async move {
        let outcome = read_batches(targets, src, &tx).await;
        if outcome.is_err() {
            stop.store(true, Ordering::Relaxed);
        }
        outcome
    };
    let write = async move {
        while let Some(batch) = rx.recv().await {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            writer.write(batch).await?;
        }
        Ok(())
    };
    match tokio::join!(read, write) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(read_error), Err(write_error)) => {
            tracing::warn!(
                error = %read_error,
                "the restore stream failed as well as the insert reported below"
            );
            Err(write_error)
        }
    }
}

/// The reader half of [`apply_data`]: reassemble documents per item and hand
/// every completed batch to the writer, in stream order. Returns `Ok` early once
/// the writer has stopped: the writer's error is the outcome.
async fn read_batches(
    targets: &HashMap<u32, ItemMeta>,
    src: &mut dyn ChunkSource,
    tx: &mpsc::Sender<Batch>,
) -> Result<()> {
    let mut current: Option<CurrentItem> = None;
    loop {
        let event = tokio::select! {
            biased;
            () = tx.closed() => return Ok(()),
            event = src.next() => event?,
        };
        let ready = match event {
            ChunkEvent::Chunk { item_id, data, .. } => {
                let mut ready = Vec::new();
                let open = match current.take() {
                    Some(open) if open.item_id == item_id => open,
                    previous => {
                        if let Some(previous) = previous {
                            ready.extend(previous.finish()?);
                        }
                        CurrentItem::new(item_id, target_for(targets, item_id)?)
                    }
                };
                let open = current.insert(open);
                ready.extend(open.feed(&data)?);
                ready
            }
            ChunkEvent::ItemEnd { item_id, total, .. } => {
                // An empty collection legitimately has ItemEnd without a
                // preceding Chunk.
                let open = match current.take() {
                    Some(open) => open,
                    None => CurrentItem::new(item_id, target_for(targets, item_id)?),
                };
                if open.item_id != item_id || open.bytes != total {
                    return Err(BackupError::phase(
                        Phase::Verify,
                        format!(
                            "ItemEnd item={item_id} does not match open item={} bytes={}",
                            open.item_id, open.bytes
                        ),
                    ));
                }
                open.finish()?.into_iter().collect()
            }
            ChunkEvent::End => {
                if let Some(open) = current.take() {
                    forward(tx, open.finish()?).await;
                }
                return Ok(());
            }
        };
        if !forward(tx, ready).await {
            return Ok(());
        }
    }
}

/// Hand `batches` to the writer in order; `false` once the writer has stopped.
async fn forward(tx: &mpsc::Sender<Batch>, batches: impl IntoIterator<Item = Batch>) -> bool {
    for batch in batches {
        if tx.send(batch).await.is_err() {
            return false;
        }
    }
    true
}

fn target_for(targets: &HashMap<u32, ItemMeta>, item_id: u32) -> Result<&ItemMeta> {
    targets
        .get(&item_id)
        .ok_or_else(|| BackupError::phase(Phase::Apply, format!("data for unknown item {item_id}")))
}

/// Per-item restore state: a byte accumulator for a document split across
/// chunk boundaries plus the batch being filled for one collection.
struct CurrentItem {
    item_id: u32,
    target: Arc<ItemMeta>,
    buf: Vec<u8>,
    batch: Vec<RawDocumentBuf>,
    batch_bytes: usize,
    bytes: u64,
}

impl CurrentItem {
    fn new(item_id: u32, target: &ItemMeta) -> Self {
        Self {
            item_id,
            target: Arc::new(target.clone()),
            buf: Vec::new(),
            batch: Vec::new(),
            batch_bytes: 0,
            bytes: 0,
        }
    }

    /// Append stream bytes and return the batches the documents they completed
    /// filled.
    fn feed(&mut self, data: &[u8]) -> Result<Vec<Batch>> {
        self.bytes += data.len() as u64;
        self.buf.extend_from_slice(data);
        let mut ready = Vec::new();
        for doc in take_documents(&mut self.buf)? {
            ready.extend(self.push(doc));
        }
        Ok(ready)
    }

    /// Add one document, first closing the batch when the document would take
    /// it past either bound. A batch always holds at least one document, so a
    /// single document of up to `MAX_BSON_DOCUMENT_BYTES` still restores.
    fn push(&mut self, doc: RawDocumentBuf) -> Option<Batch> {
        let len = doc.as_bytes().len();
        let full = !self.batch.is_empty()
            && (self.batch.len() >= INSERT_BATCH_DOCS
                || self.batch_bytes + len > INSERT_BATCH_BYTES);
        let closed = full.then(|| self.take_batch());
        self.batch_bytes += len;
        self.batch.push(doc);
        closed
    }

    fn take_batch(&mut self) -> Batch {
        self.batch_bytes = 0;
        Batch {
            target: Arc::clone(&self.target),
            docs: std::mem::take(&mut self.batch),
        }
    }

    /// Close the item: its trailing batch, if any, once every byte it received
    /// formed a whole document. `feed` already took every complete one, so any
    /// byte left is a truncated document.
    fn finish(mut self) -> Result<Option<Batch>> {
        if !self.buf.is_empty() {
            return Err(BackupError::phase(
                Phase::Apply,
                format!(
                    "trailing {} bytes after last document in {}.{}",
                    self.buf.len(),
                    self.target.database,
                    self.target.collection
                ),
            ));
        }
        Ok((!self.batch.is_empty()).then(|| self.take_batch()))
    }
}

/// Take every complete BSON document from the front of `buf`, leaving any
/// partial trailing document in it. BSON documents are length-prefixed (first 4
/// bytes, little-endian), so they are self-delimiting. Documents stay raw: they
/// are inserted byte for byte as the source server stored them.
fn take_documents(buf: &mut Vec<u8>) -> Result<Vec<RawDocumentBuf>> {
    let mut docs = Vec::new();
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
        let doc = RawDocumentBuf::from_bytes(buf[pos..pos + len].to_vec())
            .map_err(|e| BackupError::phase_src(Phase::Apply, "decode bson document", e))?;
        docs.push(doc);
        pos += len;
    }
    if pos > 0 {
        buf.drain(..pos);
    }
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    use rb_core::wire::CHUNK_SIZE;

    // --- the restore pipeline ------------------------------------------------

    /// One scripted step of a test chunk stream.
    enum Step {
        Event(ChunkEvent),
        /// Fail after a delay, as a transfer that breaks mid-run.
        Fail(Duration),
        /// Never answer, as a peer that went quiet.
        Hang,
    }

    struct Script(VecDeque<Step>);

    #[async_trait]
    impl ChunkSource for Script {
        async fn next(&mut self) -> Result<ChunkEvent> {
            match self.0.pop_front() {
                Some(Step::Event(event)) => Ok(event),
                Some(Step::Fail(delay)) => {
                    tokio::time::sleep(delay).await;
                    Err(BackupError::phase(
                        Phase::Transfer,
                        "scripted transfer failure",
                    ))
                }
                Some(Step::Hang) => std::future::pending().await,
                None => Ok(ChunkEvent::End),
            }
        }
    }

    /// Records every batch it is handed, optionally slowly or failing.
    #[derive(Default)]
    struct Recorder {
        batches: std::sync::Mutex<Vec<(String, Vec<Vec<u8>>)>>,
        delay: Duration,
        fail: bool,
    }

    #[async_trait]
    impl BatchWriter for Recorder {
        async fn write(&self, batch: Batch) -> Result<()> {
            tokio::time::sleep(self.delay).await;
            if self.fail {
                return Err(BackupError::phase(Phase::Apply, "scripted insert failure"));
            }
            let namespace = format!("{}.{}", batch.target.database, batch.target.collection);
            let docs = batch.docs.into_iter().map(|doc| doc.into_bytes()).collect();
            self.batches.lock().unwrap().push((namespace, docs));
            Ok(())
        }
    }

    impl Recorder {
        fn batches(&self) -> Vec<(String, Vec<Vec<u8>>)> {
            self.batches.lock().unwrap().clone()
        }
    }

    fn targets() -> HashMap<u32, ItemMeta> {
        HashMap::from([
            (
                0,
                ItemMeta {
                    database: "appdb".into(),
                    collection: "blobs".into(),
                },
            ),
            (
                1,
                ItemMeta {
                    database: "appdb".into(),
                    collection: "tiny".into(),
                },
            ),
            (
                7,
                ItemMeta {
                    database: "appdb".into(),
                    collection: "empty".into(),
                },
            ),
        ])
    }

    fn encoded(docs: &[Document]) -> Vec<u8> {
        docs.iter()
            .flat_map(|doc| mongodb::bson::to_vec(doc).unwrap())
            .collect()
    }

    /// The stream of one item cut into full wire chunks, as the source sends it.
    fn item_steps(item_id: u32, bytes: &[u8]) -> Vec<Step> {
        let mut steps: Vec<Step> = bytes
            .chunks(CHUNK_SIZE)
            .enumerate()
            .map(|(index, chunk)| {
                Step::Event(ChunkEvent::Chunk {
                    item_id,
                    offset: (index * CHUNK_SIZE) as u64,
                    data: chunk.to_vec(),
                })
            })
            .collect();
        steps.push(Step::Event(ChunkEvent::ItemEnd {
            item_id,
            total: bytes.len() as u64,
            blake3: blake3::hash(bytes).to_hex().to_string(),
        }));
        steps
    }

    fn tiny_docs(count: i32) -> Vec<Document> {
        (0..count).map(|i| doc! { "_id": i }).collect()
    }

    #[tokio::test]
    async fn documents_reach_the_writer_in_order_in_bounded_batches() {
        let blob = "x".repeat(1024 * 1024);
        let blobs: Vec<Document> = (0..20).map(|i| doc! { "_id": i, "b": &blob }).collect();
        let tiny = tiny_docs(25_000);
        let (blob_bytes, tiny_bytes) = (encoded(&blobs), encoded(&tiny));
        let mut steps = item_steps(0, &blob_bytes);
        steps.extend(item_steps(7, b""));
        steps.extend(item_steps(1, &tiny_bytes));
        let recorder = Recorder::default();

        apply_data(&recorder, &targets(), &mut Script(steps.into()))
            .await
            .expect("a clean stream restores");

        let batches = recorder.batches();
        let shape: Vec<(&str, usize)> = batches
            .iter()
            .map(|(namespace, docs)| (namespace.as_str(), docs.len()))
            .collect();
        // Seven 1 MiB documents fit 8 MiB and an eighth does not; the tiny
        // documents are bounded by count instead. The empty collection sends
        // nothing.
        assert_eq!(
            shape,
            vec![
                ("appdb.blobs", 7),
                ("appdb.blobs", 7),
                ("appdb.blobs", 6),
                ("appdb.tiny", INSERT_BATCH_DOCS),
                ("appdb.tiny", INSERT_BATCH_DOCS),
                ("appdb.tiny", 5_000),
            ]
        );
        for (_, docs) in &batches {
            let bytes: usize = docs.iter().map(Vec::len).sum();
            assert!(bytes <= INSERT_BATCH_BYTES, "a batch of {bytes} bytes");
        }
        let restored = |namespace: &str| -> Vec<u8> {
            batches
                .iter()
                .filter(|(n, _)| n == namespace)
                .flat_map(|(_, docs)| docs.concat())
                .collect()
        };
        assert_eq!(
            restored("appdb.blobs"),
            blob_bytes,
            "byte for byte, in order"
        );
        assert_eq!(
            restored("appdb.tiny"),
            tiny_bytes,
            "byte for byte, in order"
        );
    }

    #[tokio::test]
    async fn a_single_document_larger_than_the_batch_bound_still_restores() {
        let big = doc! { "_id": 1, "b": "y".repeat(INSERT_BATCH_BYTES + 1) };
        let bytes = encoded(&[big]);
        let recorder = Recorder::default();
        apply_data(
            &recorder,
            &targets(),
            &mut Script(item_steps(0, &bytes).into()),
        )
        .await
        .expect("one oversized document is a batch of its own");
        let batches = recorder.batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].1, vec![bytes]);
    }

    /// A failed insert must end the run at once, even while the reader waits
    /// on a peer that has nothing more to send.
    #[tokio::test]
    async fn a_failed_insert_stops_the_reader_even_while_it_waits() {
        let tiny = encoded(&tiny_docs(INSERT_BATCH_DOCS as i32 + 1));
        let mut steps: Vec<Step> = item_steps(1, &tiny);
        steps.pop(); // no ItemEnd: the peer goes quiet instead
        steps.push(Step::Hang);
        let recorder = Recorder {
            fail: true,
            ..Recorder::default()
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            apply_data(&recorder, &targets(), &mut Script(steps.into())),
        )
        .await
        .expect("the reader must not outlive the writer");
        let error = outcome.expect_err("the insert failure is the outcome");
        assert!(
            format!("{error}").contains("scripted insert failure"),
            "{error}"
        );
    }

    /// A transfer that fails mid-run must wait for the insert in flight: the
    /// cleanup that follows drops the collection, and an insert landing after
    /// it would re-create the collection with part of its documents.
    #[tokio::test]
    async fn a_failed_stream_waits_for_the_insert_in_flight() {
        let tiny = encoded(&tiny_docs(INSERT_BATCH_DOCS as i32 + 1));
        let mut steps: Vec<Step> = item_steps(1, &tiny);
        steps.pop();
        steps.push(Step::Fail(Duration::from_millis(50)));
        let recorder = Recorder {
            delay: Duration::from_millis(300),
            ..Recorder::default()
        };
        let error = apply_data(&recorder, &targets(), &mut Script(steps.into()))
            .await
            .expect_err("the transfer failure is the outcome");
        assert!(
            format!("{error}").contains("scripted transfer failure"),
            "{error}"
        );
        let batches = recorder.batches();
        assert_eq!(
            batches.len(),
            1,
            "the insert in flight completed before the return"
        );
        assert_eq!(batches[0].1.len(), INSERT_BATCH_DOCS);
    }

    #[tokio::test]
    async fn an_item_end_that_disagrees_with_the_stream_is_refused() {
        let bytes = encoded(&tiny_docs(3));
        let mut steps = item_steps(1, &bytes);
        if let Some(Step::Event(ChunkEvent::ItemEnd { total, .. })) = steps.last_mut() {
            *total += 1;
        }
        let error = apply_data(&Recorder::default(), &targets(), &mut Script(steps.into()))
            .await
            .expect_err("a short item must be refused");
        assert!(
            format!("{error}").contains("does not match open item"),
            "{error}"
        );

        let truncated = &bytes[..bytes.len() - 1];
        let error = apply_data(
            &Recorder::default(),
            &targets(),
            &mut Script(item_steps(1, truncated).into()),
        )
        .await
        .expect_err("a truncated document must be refused");
        assert!(format!("{error}").contains("trailing"), "{error}");

        let error = apply_data(
            &Recorder::default(),
            &targets(),
            &mut Script(item_steps(9, &bytes).into()),
        )
        .await
        .expect_err("data for an unresolved item must be refused");
        assert!(format!("{error}").contains("unknown item 9"), "{error}");
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
        let pf = assess(&plan_for(&payload), &payload, &probe(6, &[]), false);
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
        let pf = assess(&plan_for(&payload), &payload, &probe(5, &[]), false);
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

        let plan = plan_for(&payload);
        let blocked = assess(&plan, &payload, &p, false);
        assert!(
            !blocked.ok,
            "existing collection must block without overwrite"
        );
        assert!(blocked
            .checks
            .iter()
            .any(|c| c.name == "collection:appdb.accounts" && !c.passed));

        let allowed = assess(&plan, &payload, &p, true);
        assert!(
            allowed.ok,
            "overwrite must allow restoring over existing collection"
        );
    }

    fn plan_for(payload: &MongoPlanPayload) -> BackupPlan {
        crate::introspect::build_plan(payload, "t".to_string())
    }

    fn two_collection_payload() -> MongoPlanPayload {
        let mut payload = crate::model::test_fixture();
        let mut orders = payload.databases[0].collections[0].clone();
        orders.name = "orders".to_string();
        payload.databases[0].collections.push(orders);
        payload
    }

    fn faults_of(plan: &BackupPlan, payload: &MongoPlanPayload) -> Vec<String> {
        resolve_items(plan, payload).expect_err("the plan's items must be refused")
    }

    #[test]
    fn resolve_items_maps_each_item_to_its_catalog_namespace() {
        let payload = two_collection_payload();
        let targets = resolve_items(&plan_for(&payload), &payload).expect("a source-built plan");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[&0].database, "appdb");
        assert_eq!(targets[&0].collection, "accounts");
        assert_eq!(targets[&1].collection, "orders");
    }

    /// The item meta routes the documents, and it is as peer-controlled as the
    /// catalog. A clean catalog must not let an item write into a system
    /// namespace, into a collection the catalog never declared (one the
    /// existence check and the cleanup set never saw), or into a namespace
    /// another item already feeds.
    #[test]
    fn hostile_item_meta_is_refused_before_any_write() {
        let payload = two_collection_payload();

        let mut system = plan_for(&payload);
        system.items[0].meta =
            serde_json::json!({"database": "admin", "collection": "system.users"});
        let faults = faults_of(&system, &payload);
        assert!(
            faults
                .iter()
                .any(|f| f.contains("refusing system or invalid namespace")),
            "{faults:?}"
        );

        let mut undeclared = plan_for(&payload);
        undeclared.items[0].meta =
            serde_json::json!({"database": "appdb", "collection": "payroll"});
        let faults = faults_of(&undeclared, &payload);
        assert!(
            faults
                .iter()
                .any(|f| f.contains("appdb.payroll, which the plan's catalog does not declare")),
            "{faults:?}"
        );
        assert!(
            faults
                .iter()
                .any(|f| f == "appdb.accounts has no plan item"),
            "{faults:?}"
        );

        let mut duplicate = plan_for(&payload);
        duplicate.items[1].meta = duplicate.items[0].meta.clone();
        let faults = faults_of(&duplicate, &payload);
        assert!(
            faults
                .iter()
                .any(|f| f == "items 0 and 1 both target appdb.accounts"),
            "{faults:?}"
        );

        let mut unreadable = plan_for(&payload);
        unreadable.items[0].meta = serde_json::Value::Null;
        let faults = faults_of(&unreadable, &payload);
        assert!(
            faults.iter().any(|f| f.contains("unreadable meta")),
            "{faults:?}"
        );

        let mut foreign = plan_for(&payload);
        foreign.items[0].kind = "object".to_string();
        let faults = faults_of(&foreign, &payload);
        assert!(
            faults.iter().any(|f| f.contains("unknown kind")),
            "{faults:?}"
        );

        let mut twice = payload.clone();
        let again = twice.databases[0].collections[0].clone();
        twice.databases[0].collections.push(again);
        let faults = faults_of(&plan_for(&twice), &twice);
        assert!(
            faults
                .iter()
                .any(|f| f == "the catalog lists appdb.accounts twice"),
            "{faults:?}"
        );

        // Preflight reports it, and the apply-time re-check refuses it.
        let pf = assess(&system, &payload, &probe(6, &[]), true);
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "plan_items" && !c.passed));
        let error = require_items(&system, &payload, Phase::Validate).expect_err("refused");
        assert!(
            format!("{error}").contains("refusing the plan's items"),
            "{error}"
        );
    }

    fn collections(list: Vec<MongoCollection>, skipped: &[&str]) -> crate::introspect::Collections {
        crate::introspect::Collections {
            collections: list,
            skipped: skipped.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_restored_catalog_that_matches_the_plan_has_no_faults() {
        let payload = two_collection_payload();
        let planned = &payload.databases[0].collections;
        let mut restored = planned.clone();
        restored.reverse();
        for coll in &mut restored {
            // Volatile estimates never matter.
            coll.estimated_docs = 0;
            coll.estimated_bytes = 0;
        }
        assert!(catalog_faults("appdb", planned, collections(restored, &[])).is_empty());
    }

    #[test]
    fn catalog_faults_name_the_collection_and_what_differs() {
        let payload = two_collection_payload();
        let planned = &payload.databases[0].collections;

        let missing = catalog_faults("appdb", planned, collections(vec![planned[0].clone()], &[]));
        assert_eq!(missing, vec!["appdb.orders is missing".to_string()]);

        let mut changed = planned.clone();
        changed[0].options = Some(serde_json::json!({"capped": true, "size": 4096}));
        changed[1].indexes.clear();
        let faults = catalog_faults("appdb", planned, collections(changed, &[]));
        assert_eq!(faults.len(), 2, "{faults:?}");
        assert_eq!(faults[0], "appdb.accounts has different collection options");
        assert_eq!(
            faults[1],
            "appdb.orders has different indexes (planned: email_1; restored: none)"
        );

        let view = catalog_faults(
            "appdb",
            planned,
            collections(vec![planned[0].clone()], &["appdb.orders (View)"]),
        );
        assert_eq!(
            view,
            vec!["appdb.orders (View) is not an ordinary collection".to_string()],
            "a planned name that came back as a view is reported once, as what it is"
        );
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
            out.extend(take_documents(&mut buf).unwrap());
        }
        let out: Vec<u8> = out
            .into_iter()
            .flat_map(RawDocumentBuf::into_bytes)
            .collect();
        assert_eq!(
            out, bytes,
            "all documents reassembled in order, byte for byte"
        );
        assert!(buf.is_empty(), "no trailing bytes once the stream is whole");
    }

    #[test]
    fn take_documents_leaves_partial_tail() {
        let d = doc! { "_id": 1, "v": "x" };
        let bytes = mongodb::bson::to_vec(&d).unwrap();
        let mut buf = bytes[..bytes.len() - 1].to_vec(); // one byte short
        let out = take_documents(&mut buf).unwrap();
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
            let error =
                take_documents(&mut buf).expect_err(&format!("prefix {prefix:?} must be refused"));
            assert!(
                format!("{error}").contains("invalid BSON document length"),
                "{error}"
            );
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
