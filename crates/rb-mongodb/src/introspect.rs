//! Read-only cluster introspection + plan construction (plan Phase 3.2).
//!
//! Walks databases → collections → indexes (and best-effort users) over a live
//! connection, filling a [`MongoPlanPayload`]. Strictly read-only: only
//! `list*`/`estimated_document_count`/`collStats`/`usersInfo` are issued
//! (I-IMMUT). System databases (`admin`/`local`/`config`) and non-`Collection`
//! types (views, time-series) are excluded.
//!
//! The live driver calls are exercised by the mongodb e2e; the pure
//! [`build_plan`] / [`now_rfc3339`] logic is unit-tested here.

use mongodb::bson::{doc, Document};
use mongodb::results::CollectionType;
use mongodb::IndexModel;

use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, PLAN_FORMAT_VERSION};

use crate::connect::{ReadOnlyConnection, ReadOnlyDatabase};
use crate::model::{MongoCollection, MongoDatabase, MongoPlanPayload, MongoUser};
use crate::MongoDbParams;

/// System databases never included in a backup, and never restored into.
pub(crate) const SYSTEM_DBS: &[&str] = &["admin", "local", "config"];

/// Connect and introspect the whole reachable cluster (read-only).
pub async fn introspect_cluster(params: &MongoDbParams) -> Result<MongoPlanPayload> {
    let conn = ReadOnlyConnection::connect(params).await?;
    introspect_with(&conn, params).await
}

/// Introspect the cluster over an existing source connection.
pub(crate) async fn introspect_with(
    conn: &ReadOnlyConnection,
    params: &MongoDbParams,
) -> Result<MongoPlanPayload> {
    let db_names = select_databases(conn, params).await?;

    let mut databases = Vec::new();
    let mut skipped = Vec::new();
    for name in db_names {
        let found = introspect_collections(conn, &name, |_| true).await?;
        skipped.extend(found.skipped);
        let users = introspect_users(&conn.client.database(&name)).await;
        databases.push(MongoDatabase {
            name,
            collections: found.collections,
            users,
        });
    }
    refuse_skipped(&skipped, params.allow_skipped_namespaces)?;

    Ok(MongoPlanPayload {
        server_version: conn.server_version.clone(),
        server_major: conn.server_major,
        databases,
    })
}

/// The databases to back up: the single `--database` when set, else every
/// non-system database.
async fn select_databases(
    conn: &ReadOnlyConnection,
    params: &MongoDbParams,
) -> Result<Vec<String>> {
    if let Some(db) = &params.database {
        return Ok(vec![db.clone()]);
    }
    let all = conn
        .client
        .list_database_names()
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, "list databases", e))?;
    Ok(all
        .into_iter()
        .filter(|n| !SYSTEM_DBS.contains(&n.as_str()))
        .collect())
}

/// The ordinary collections of one database, plus every namespace passed over
/// because it is not one.
pub(crate) struct Collections {
    pub(crate) collections: Vec<MongoCollection>,
    /// `db.name (Type)` of each view, time-series or other non-ordinary
    /// namespace.
    pub(crate) skipped: Vec<String>,
}

/// Introspect the collections of one database whose name `wanted` accepts:
/// options, indexes and estimates. `system.*` collections are never included.
/// The destination's read-back passes the planned names only, so an unrelated
/// collection — or view — in the same database is neither read nor reported.
pub(crate) async fn introspect_collections(
    conn: &ReadOnlyConnection,
    db_name: &str,
    wanted: impl Fn(&str) -> bool,
) -> Result<Collections> {
    let db = conn.client.database(db_name);

    let specs = db.list_collections().await.map_err(|e| {
        BackupError::phase_src(Phase::Analyze, format!("list collections {db_name}"), e)
    })?;

    let mut collections = Vec::new();
    let mut skipped = Vec::new();
    for spec in specs {
        if spec.name.starts_with("system.") || !wanted(&spec.name) {
            continue;
        }
        // Only ordinary collections carry restorable document data; views are
        // derived and time-series have a special on-disk shape (out of scope).
        if spec.collection_type != CollectionType::Collection {
            skipped.push(format!(
                "{}.{} ({:?})",
                db_name, spec.name, spec.collection_type
            ));
            continue;
        }
        collections.push(introspect_collection(&db, db_name, &spec.name, &spec.options).await?);
    }
    Ok(Collections {
        collections,
        skipped,
    })
}

/// Skipping a view or time-series collection SILENTLY meant a cluster holding
/// one restored without it and still reported a verified 1:1 copy, so they are
/// refused unless the operator opts in explicitly.
fn refuse_skipped(skipped: &[String], allowed: bool) -> Result<()> {
    if skipped.is_empty() {
        return Ok(());
    }
    if allowed {
        tracing::warn!(
            namespaces = %skipped.join(", "),
            "allow_skipped_namespaces is set: this backup is NOT a 1:1 copy"
        );
        return Ok(());
    }
    Err(BackupError::phase(
        Phase::Analyze,
        format!(
            "this build does not restore views or time-series collections, and will not \
             report a 1:1 copy without them: {}. Exclude those databases, or set \
             allow_skipped_namespaces=true to accept a partial copy.",
            skipped.join(", ")
        ),
    ))
}

/// Introspect one collection's options, indexes, and size estimates.
async fn introspect_collection(
    db: &ReadOnlyDatabase,
    db_name: &str,
    coll_name: &str,
    options: &mongodb::options::CreateCollectionOptions,
) -> Result<MongoCollection> {
    // Index specs: the JSON form of each IndexModel, minus the implicit _id_.
    let models: Vec<IndexModel> = db.list_indexes(coll_name).await.map_err(|e| {
        BackupError::phase_src(Phase::Analyze, format!("list indexes {coll_name}"), e)
    })?;
    let raw_indexes = raw_index_specs(db, coll_name).await?;
    let mut indexes = Vec::new();
    for m in models {
        let name = index_name(&m);
        if name.as_deref() == Some("_id_") {
            continue;
        }
        let what = format!("index of {db_name}.{coll_name}");
        reject_lossy_bson(&m, &what)?;
        let raw = raw_spec_of(&m, &raw_indexes).ok_or_else(|| {
            BackupError::phase(
                Phase::Analyze,
                format!(
                    "{what} {:?} is missing from the server's own index listing",
                    m.keys
                ),
            )
        })?;
        indexes.push(with_unmodelled(&m, Some(raw), INDEX_NOISE, &what)?);
    }

    let estimated_docs = db
        .estimated_document_count(coll_name)
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("count {coll_name}"), e))?;

    let what = format!("options of {db_name}.{coll_name}");
    reject_lossy_bson(options, &what)?;
    let raw_options = raw_collection_options(db, coll_name).await?;
    let options = match with_unmodelled(options, raw_options.as_ref(), &[], &what)? {
        serde_json::Value::Object(map) if !map.is_empty() => Some(serde_json::Value::Object(map)),
        _ => None,
    };
    Ok(MongoCollection {
        database: db_name.to_string(),
        name: coll_name.to_string(),
        options,
        indexes,
        estimated_docs,
        estimated_bytes: coll_size_bytes(db, coll_name).await,
    })
}

/// Refuse a spec that cannot survive the plan's JSON container.
///
/// Index specs and collection options are carried as `serde_json::Value`.
/// BSON's `Serialize` for a generic `Binary` calls `serialize_bytes`, which
/// serde_json renders as an array of integers — the destination then rebuilds a
/// BSON *array*, which MongoDB accepts as a different (and silently wrong)
/// value, and re-introspection produces the same array so verification agrees.
/// Rather than ship that silent corruption, such a spec is refused.
fn reject_lossy_bson<T: serde::Serialize>(value: &T, what: &str) -> Result<()> {
    let document = mongodb::bson::to_document(value)
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("encode {what} as BSON"), e))?;
    if let Some(path) = find_binary(&mongodb::bson::Bson::Document(document), String::new()) {
        return Err(BackupError::phase(
            Phase::Analyze,
            format!(
                "{what} contains BSON binary data at {path}, which this plan format cannot carry \
                 without changing its type; the {what} must be recreated manually"
            ),
        ));
    }
    Ok(())
}

/// Path of the first `Bson::Binary` inside `value`, if any.
fn find_binary(value: &mongodb::bson::Bson, path: String) -> Option<String> {
    match value {
        mongodb::bson::Bson::Binary(_) => Some(if path.is_empty() {
            "<root>".to_string()
        } else {
            path
        }),
        mongodb::bson::Bson::Document(document) => document.iter().find_map(|(key, nested)| {
            find_binary(
                nested,
                if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                },
            )
        }),
        mongodb::bson::Bson::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, nested)| find_binary(nested, format!("{path}[{index}]"))),
        _ => None,
    }
}

/// The index name from an `IndexModel`'s options, if set.
fn index_name(m: &IndexModel) -> Option<String> {
    m.options.as_ref().and_then(|o| o.name.clone())
}

/// `listIndexes` keys that describe where the index lives, not what it is:
/// pre-4.4 servers report the namespace inside every spec.
const INDEX_NOISE: &[&str] = &["ns"];

/// The JSON form of a driver-typed spec, plus every key of the server's own
/// (`raw`) spec the driver's type does not model.
///
/// The typed `IndexOptions`/`CreateCollectionOptions` silently drop a key the
/// pinned driver does not know (an index's `prepareUnique`, say). Both sides
/// read through the same type, so the loss was invisible to verification too.
/// Carrying the remainder verbatim makes the destination replay it and the
/// read-back compare it.
fn with_unmodelled<T: serde::Serialize>(
    typed: &T,
    raw: Option<&Document>,
    noise: &[&str],
    what: &str,
) -> Result<serde_json::Value> {
    let encode =
        |e: serde_json::Error| BackupError::phase_src(Phase::Analyze, format!("encode {what}"), e);
    let mut value = serde_json::to_value(typed).map_err(encode)?;
    let (Some(raw), serde_json::Value::Object(map)) = (raw, &mut value) else {
        return Ok(value);
    };
    let modelled = mongodb::bson::to_document(typed)
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("encode {what} as BSON"), e))?;
    for (key, extra) in raw {
        if modelled.contains_key(key) || noise.contains(&key.as_str()) {
            continue;
        }
        if let Some(path) = find_binary(extra, key.clone()) {
            return Err(BackupError::phase(
                Phase::Analyze,
                format!(
                    "{what} contains BSON binary data at {path}, which this plan format cannot \
                     carry without changing its type; the {what} must be recreated manually"
                ),
            ));
        }
        map.insert(key.clone(), serde_json::to_value(extra).map_err(encode)?);
    }
    Ok(value)
}

/// Spell every whole number as an integer, for comparison only.
///
/// A catalog number's BSON type is not part of what it means, and servers do
/// not preserve it: an `expireAfterSeconds: 60.0` written by a legacy shell is
/// stored as `60` by a 5.0+ destination, and the read-back then disagreed with
/// the plan over an identical index. The plan itself keeps the source's
/// spelling, so the destination is handed exactly what the source holds.
pub(crate) fn integral_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(number) => {
            if let Some(float) = number.as_f64().filter(|_| number.is_f64()) {
                if float.fract() == 0.0 && float.abs() < 9_007_199_254_740_992.0 {
                    *number = serde_json::Number::from(float as i64);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(integral_numbers),
        serde_json::Value::Object(map) => map.values_mut().for_each(integral_numbers),
        _ => {}
    }
}

/// The server's spec for the index the driver decoded as `model`.
///
/// By name — unless the driver could not decode the options at all, which it
/// reports as an index with a key and nothing else: a TTL written by a legacy
/// shell stores `expireAfterSeconds` as a double, the typed options refuse it,
/// and name, TTL and every other option used to vanish together. Such an index
/// is found by its key instead, and its options come from the raw spec.
fn raw_spec_of<'a>(model: &IndexModel, raw: &'a [Document]) -> Option<&'a Document> {
    match index_name(model) {
        Some(name) => raw
            .iter()
            .find(|spec| spec.get_str("name") == Ok(name.as_str())),
        None => raw
            .iter()
            .find(|spec| spec.get_document("key").ok() == Some(&model.keys)),
    }
}

/// The server's own index specs for one collection.
async fn raw_index_specs(db: &ReadOnlyDatabase, coll_name: &str) -> Result<Vec<Document>> {
    let reply = db
        .run_command(doc! { "listIndexes": coll_name, "cursor": { "batchSize": 100_000 } })
        .await
        .map_err(|e| {
            BackupError::phase_src(Phase::Analyze, format!("list raw indexes {coll_name}"), e)
        })?;
    first_batch(&reply, &format!("listIndexes {coll_name}"))
}

/// The server's own `create` options for one collection.
async fn raw_collection_options(
    db: &ReadOnlyDatabase,
    coll_name: &str,
) -> Result<Option<Document>> {
    let reply = db
        .run_command(doc! { "listCollections": 1, "filter": { "name": coll_name } })
        .await
        .map_err(|e| {
            BackupError::phase_src(Phase::Analyze, format!("list raw options {coll_name}"), e)
        })?;
    Ok(
        first_batch(&reply, &format!("listCollections {coll_name}"))?
            .into_iter()
            .find_map(|spec| spec.get_document("options").ok().cloned()),
    )
}

/// The documents of a cursor reply that fits its first batch. A reply that
/// does not is refused rather than read in part.
fn first_batch(reply: &Document, what: &str) -> Result<Vec<Document>> {
    let malformed = || BackupError::phase(Phase::Analyze, format!("{what}: malformed reply"));
    let cursor = reply.get_document("cursor").map_err(|_| malformed())?;
    if cursor.get("id").and_then(mongodb::bson::Bson::as_i64) != Some(0) {
        return Err(BackupError::phase(
            Phase::Analyze,
            format!("{what}: the reply does not fit one batch"),
        ));
    }
    Ok(cursor
        .get_array("firstBatch")
        .map_err(|_| malformed())?
        .iter()
        .filter_map(|entry| entry.as_document().cloned())
        .collect())
}

/// Best-effort on-disk size via `collStats`; `0` when unavailable (e.g. the
/// command is restricted). Size is informational only (estimate/progress).
async fn coll_size_bytes(db: &ReadOnlyDatabase, coll_name: &str) -> u64 {
    match db.run_command(doc! { "collStats": coll_name }).await {
        Ok(stats) => stats
            .get_i64("size")
            .ok()
            .or_else(|| stats.get_i32("size").ok().map(i64::from))
            .map(|v| v.max(0) as u64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// Best-effort user listing via `usersInfo`; an empty list on any error (the
/// connected user may lack the privilege). Users are captured for plan display
/// and the fingerprint but are NOT recreated on restore (see `dest.rs`).
async fn introspect_users(db: &ReadOnlyDatabase) -> Vec<MongoUser> {
    let Ok(info) = db.run_command(doc! { "usersInfo": 1 }).await else {
        return Vec::new();
    };
    let Ok(users) = info.get_array("users") else {
        return Vec::new();
    };
    users
        .iter()
        .filter_map(|b| b.as_document())
        .map(|d| MongoUser {
            db: d.get_str("db").unwrap_or_default().to_string(),
            name: d.get_str("user").unwrap_or_default().to_string(),
            roles: d
                .get_array("roles")
                .map(|a| a.iter().filter_map(bson_to_json).collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// Convert a BSON value to JSON, dropping it on failure.
fn bson_to_json(b: &mongodb::bson::Bson) -> Option<serde_json::Value> {
    serde_json::to_value(b).ok()
}

/// Build the self-contained [`BackupPlan`] from an introspected payload: one
/// data item per collection, in (database, collection) order.
pub fn build_plan(payload: &MongoPlanPayload, created_at: String) -> BackupPlan {
    let mut items = Vec::new();
    let mut estimated_bytes: u64 = 0;
    let mut coll_count = 0usize;

    let mut id: u32 = 0;
    for db in &payload.databases {
        for coll in &db.collections {
            coll_count += 1;
            estimated_bytes = estimated_bytes.saturating_add(coll.estimated_bytes);
            items.push(PlanItem {
                id,
                ordinal: id,
                kind: "collection".to_string(),
                name: format!("{}.{}", db.name, coll.name),
                estimated_bytes: coll.estimated_bytes,
                meta: serde_json::json!({
                    "database": db.name,
                    "collection": coll.name,
                }),
            });
            id += 1;
        }
    }

    let source_summary = format!(
        "MongoDB {} — {} database(s), {} collection(s)",
        payload.server_version,
        payload.databases.len(),
        coll_count,
    );

    BackupPlan {
        format_version: PLAN_FORMAT_VERSION,
        module: "mongodb".to_string(),
        mode: BackupMode::Copy1to1,
        created_at,
        source_summary,
        items,
        estimated_bytes,
        integrity: IntegritySpec::default(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Current time as an RFC3339 UTC string (the core does not read a clock).
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_rfc3339(secs)
}

fn format_unix_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as i64;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert days-since-Unix-epoch to (year, month, day). Howard Hinnant's
/// `civil_from_days`, valid for the full proleptic Gregorian range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{doc, spec::BinarySubtype, Binary, Bson};

    /// A key the driver's typed options do not model must reach the plan, or
    /// the destination drops it and the read-back cannot tell.
    #[test]
    fn keys_the_driver_does_not_model_are_carried_verbatim() {
        let model = IndexModel::builder()
            .keys(doc! { "b": 1, "a": -1 })
            .options(
                mongodb::options::IndexOptions::builder()
                    .name("b_1_a_-1".to_string())
                    .unique(true)
                    .build(),
            )
            .build();
        let raw = doc! {
            "key": { "b": 1, "a": -1 }, "name": "b_1_a_-1", "unique": true,
            "ns": "appdb.a", "prepareUnique": true, "futureOption": { "x": 1_i64 },
        };
        let spec = with_unmodelled(&model, Some(&raw), INDEX_NOISE, "index of appdb.a").unwrap();
        assert_eq!(spec["prepareUnique"], serde_json::json!(true));
        assert_eq!(spec["futureOption"], serde_json::json!({ "x": 1 }));
        assert_eq!(spec["unique"], serde_json::json!(true));
        assert!(spec.get("ns").is_none(), "the namespace is not an option");
        assert_eq!(
            spec["key"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["b", "a"],
            "compound key order is the index"
        );
        // Nothing to add: the spec is exactly the typed one.
        let plain = doc! { "key": { "b": 1, "a": -1 }, "name": "b_1_a_-1", "unique": true };
        assert_eq!(
            with_unmodelled(&model, Some(&plain), INDEX_NOISE, "index").unwrap(),
            with_unmodelled(&model, None, INDEX_NOISE, "index").unwrap()
        );
        // The carried remainder is held to the same no-binary rule.
        let binary = doc! { "blob": mongodb::bson::Binary {
        subtype: mongodb::bson::spec::BinarySubtype::Generic, bytes: vec![1] } };
        with_unmodelled(&model, Some(&binary), INDEX_NOISE, "index of appdb.a")
            .expect_err("binary must be refused");
    }

    /// The typed options fail to decode a double `expireAfterSeconds` and the
    /// driver then reports the index with no options at all, name included.
    #[test]
    fn an_index_whose_options_the_driver_could_not_decode_keeps_them() {
        let undecoded = IndexModel::builder().keys(doc! { "at": 1.0 }).build();
        let raw = vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "at": 1.0 }, "name": "at_1", "expireAfterSeconds": 60.0 },
        ];
        let found = raw_spec_of(&undecoded, &raw).expect("matched by key");
        let spec = with_unmodelled(&undecoded, Some(found), INDEX_NOISE, "index").unwrap();
        assert_eq!(spec["name"], serde_json::json!("at_1"));
        assert!(
            spec["expireAfterSeconds"].is_f64(),
            "the plan keeps the source's type"
        );
        assert!(raw_spec_of(&undecoded, &raw[..1]).is_none());
    }

    #[test]
    fn a_cursor_reply_that_does_not_fit_one_batch_is_refused() {
        let whole = doc! { "cursor": { "id": 0_i64, "firstBatch": [ { "name": "a" } ] } };
        assert_eq!(first_batch(&whole, "x").unwrap().len(), 1);
        let partial = doc! { "cursor": { "id": 7_i64, "firstBatch": [] } };
        first_batch(&partial, "x").expect_err("a partial listing must not be trusted");
        first_batch(&doc! {}, "x").expect_err("malformed");
    }

    /// BSON binary inside an index spec or collection options would be
    /// re-rendered by serde_json as an array of integers, so the destination
    /// would silently rebuild a *different* value — and re-introspection would
    /// produce the same array, so verification would agree with the
    /// corruption. It must fail analysis instead.
    #[test]
    fn binary_in_a_spec_is_refused_rather_than_silently_retyped() {
        let binary = Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: vec![1, 2, 3],
        });

        // Plain specs pass.
        reject_lossy_bson(
            &doc! { "key": { "email": 1 }, "unique": true },
            "index of appdb.a",
        )
        .expect("a plain index spec carries no binary");

        for (spec, want_path) in [
            (doc! { "blob": binary.clone() }, "blob"),
            (
                doc! { "validator": { "$expr": { "seed": binary.clone() } } },
                "validator.$expr.seed",
            ),
            (doc! { "keys": [1, binary.clone()] }, "keys[1]"),
        ] {
            let error =
                reject_lossy_bson(&spec, "options of appdb.a").expect_err("binary must be refused");
            let rendered = format!("{error}");
            assert!(rendered.contains("contains BSON binary data"), "{rendered}");
            assert!(rendered.contains(want_path), "{rendered}");
        }
    }

    #[test]
    fn build_plan_one_item_per_collection() {
        let payload = crate::model::test_fixture();
        let plan = build_plan(&payload, "2026-06-25T00:00:00Z".to_string());
        assert_eq!(plan.module, "mongodb");
        assert_eq!(plan.items.len(), 1);
        let item = &plan.items[0];
        assert_eq!(item.kind, "collection");
        assert_eq!(item.name, "appdb.accounts");
        assert_eq!(item.meta["database"], "appdb");
        assert_eq!(item.meta["collection"], "accounts");
        assert_eq!(plan.estimated_bytes, 65536);
        // payload round-trips through the plan Value.
        let back: MongoPlanPayload = serde_json::from_value(plan.payload).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn build_plan_ids_are_sequential_across_databases() {
        let mut payload = crate::model::test_fixture();
        payload.databases.push(MongoDatabase {
            name: "other".to_string(),
            collections: vec![MongoCollection {
                database: "other".to_string(),
                name: "logs".to_string(),
                options: None,
                indexes: vec![],
                estimated_docs: 1,
                estimated_bytes: 10,
            }],
            users: vec![],
        });
        let plan = build_plan(&payload, "t".to_string());
        let ids: Vec<u32> = plan.items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![0, 1]);
        assert_eq!(plan.items[1].name, "other.logs");
        assert_eq!(plan.estimated_bytes, 65546);
    }

    #[test]
    fn rfc3339_known_vectors() {
        assert_eq!(format_unix_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_unix_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
    }
}
