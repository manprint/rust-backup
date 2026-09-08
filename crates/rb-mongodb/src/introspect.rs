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

use futures_util::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::results::CollectionType;
use mongodb::IndexModel;

use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, PLAN_FORMAT_VERSION};

use crate::model::{MongoCollection, MongoDatabase, MongoPlanPayload, MongoUser};
use crate::{MongoConnection, MongoDbParams};

/// System databases never included in a backup, and never restored into.
pub(crate) const SYSTEM_DBS: &[&str] = &["admin", "local", "config"];

/// Connect and introspect the whole reachable cluster (read-only).
pub async fn introspect_cluster(params: &MongoDbParams) -> Result<MongoPlanPayload> {
    let conn = MongoConnection::connect(params).await?;
    let db_names = select_databases(&conn, params).await?;

    let mut databases = Vec::new();
    for name in db_names {
        databases.push(introspect_database(&conn, &name, params).await?);
    }

    Ok(MongoPlanPayload {
        server_version: conn.server_version,
        server_major: conn.server_major,
        databases,
    })
}

/// The databases to back up: the single `--database` when set, else every
/// non-system database.
async fn select_databases(conn: &MongoConnection, params: &MongoDbParams) -> Result<Vec<String>> {
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

/// Introspect one database: its collections (+ options + indexes + estimates)
/// and (best-effort) its users.
async fn introspect_database(
    conn: &MongoConnection,
    db_name: &str,
    params: &MongoDbParams,
) -> Result<MongoDatabase> {
    let db = conn.client.database(db_name);

    let specs = db
        .list_collections()
        .await
        .map_err(|e| {
            BackupError::phase_src(Phase::Analyze, format!("list collections {db_name}"), e)
        })?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| {
            BackupError::phase_src(
                Phase::Analyze,
                format!("read collection specs {db_name}"),
                e,
            )
        })?;

    let mut collections = Vec::new();
    let mut skipped = Vec::new();
    for spec in specs {
        // Only ordinary collections carry restorable document data; views are
        // derived and time-series have a special on-disk shape (out of scope).
        // Skipping them SILENTLY meant a cluster with views restored without
        // them and still reported a verified 1:1 copy, so they are refused
        // unless the operator opts in explicitly.
        if spec.collection_type != CollectionType::Collection {
            skipped.push(format!(
                "{}.{} ({:?})",
                db_name, spec.name, spec.collection_type
            ));
            continue;
        }
        if spec.name.starts_with("system.") {
            continue;
        }
        collections.push(introspect_collection(&db, db_name, &spec.name, &spec.options).await?);
    }
    if !skipped.is_empty() && params.allow_skipped_namespaces {
        tracing::warn!(
            namespaces = %skipped.join(", "),
            "allow_skipped_namespaces is set: this backup is NOT a 1:1 copy"
        );
    }
    if !skipped.is_empty() && !params.allow_skipped_namespaces {
        return Err(BackupError::phase(
            Phase::Analyze,
            format!(
                "this build does not restore views or time-series collections, and will not \
                 report a 1:1 copy without them: {}. Exclude those databases, or set \
                 allow_skipped_namespaces=true to accept a partial copy.",
                skipped.join(", ")
            ),
        ));
    }

    let users = introspect_users(&db).await;

    Ok(MongoDatabase {
        name: db_name.to_string(),
        collections,
        users,
    })
}

/// Introspect one collection's options, indexes, and size estimates.
async fn introspect_collection(
    db: &mongodb::Database,
    db_name: &str,
    coll_name: &str,
    options: &mongodb::options::CreateCollectionOptions,
) -> Result<MongoCollection> {
    let coll = db.collection::<Document>(coll_name);

    // Index specs: the JSON form of each IndexModel, minus the implicit _id_.
    let models: Vec<IndexModel> = coll
        .list_indexes()
        .await
        .map_err(|e| {
            BackupError::phase_src(Phase::Analyze, format!("list indexes {coll_name}"), e)
        })?
        .try_collect()
        .await
        .map_err(|e| {
            BackupError::phase_src(Phase::Analyze, format!("read indexes {coll_name}"), e)
        })?;
    let mut indexes = Vec::new();
    for m in models {
        if index_name(&m).as_deref() == Some("_id_") {
            continue;
        }
        reject_lossy_bson(&m, &format!("index of {db_name}.{coll_name}"))?;
        let v = serde_json::to_value(&m).map_err(|e| {
            BackupError::phase_src(Phase::Analyze, format!("encode index of {coll_name}"), e)
        })?;
        indexes.push(v);
    }

    let estimated_docs = coll
        .estimated_document_count()
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("count {coll_name}"), e))?;

    reject_lossy_bson(options, &format!("options of {db_name}.{coll_name}"))?;
    Ok(MongoCollection {
        database: db_name.to_string(),
        name: coll_name.to_string(),
        options: create_options_value(options),
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

/// Serialize `CreateCollectionOptions` to a JSON object, returning `None` when it
/// is empty (the common case — no capped/validator/etc.).
fn create_options_value(
    options: &mongodb::options::CreateCollectionOptions,
) -> Option<serde_json::Value> {
    match serde_json::to_value(options) {
        Ok(serde_json::Value::Object(m)) if !m.is_empty() => Some(serde_json::Value::Object(m)),
        _ => None,
    }
}

/// Best-effort on-disk size via `collStats`; `0` when unavailable (e.g. the
/// command is restricted). Size is informational only (estimate/progress).
async fn coll_size_bytes(db: &mongodb::Database, coll_name: &str) -> u64 {
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
async fn introspect_users(db: &mongodb::Database) -> Vec<MongoUser> {
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
