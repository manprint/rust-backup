//! Destination-side preflight (plan Phase 2.5).
//!
//! Validates that the destination can perform the restore BEFORE any bytes flow:
//! server major ≥ source major, admin can create roles + databases, and target
//! databases are absent (or `--overwrite`). The decision logic is a pure function
//! ([`assess`]) of a [`DestProbe`] gathered over an ADMIN connection, so it is
//! unit-tested without a live DB; only [`probe_dest`] touches the server.
//!
//! Free-disk space is intentionally NOT a hard check: a SQL connection cannot see
//! the destination host's filesystem, so the size is reported informationally.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;

use bytes::Bytes;
use futures_util::SinkExt;
use tokio_postgres::{Client, CopyInSink};

use rb_core::channel::{ChunkEvent, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{human_bytes, BackupPlan, Preflight};

use crate::ddl::{build_cluster_ddl, create_database, quote_ident, quote_qualified};
use crate::model::PgPlanPayload;
use crate::source::ItemMeta;
use crate::{PgConnection, PostgresParams};

/// Facts gathered from the destination needed to assess the plan.
#[derive(Debug, Clone, Default)]
pub struct DestProbe {
    /// Destination server major version.
    pub dest_major: u32,
    /// Connected role is a superuser (implies all creation privileges).
    pub is_super: bool,
    pub createdb: bool,
    pub createrole: bool,
    /// Target database names that already exist on the destination.
    pub existing_databases: HashSet<String>,
}

/// Preflight the plan against the destination (ADMIN connection).
pub async fn validate(params: &PostgresParams, plan: &BackupPlan) -> Result<Preflight> {
    let payload: PgPlanPayload = serde_json::from_value(plan.payload.clone())
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
    payload: &PgPlanPayload,
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

    let privileges_ok = probe.is_super || (probe.createdb && probe.createrole);
    pf = pf.check(
        "privileges",
        privileges_ok,
        if privileges_ok {
            "admin can create roles and databases".to_string()
        } else {
            "admin needs SUPERUSER, or both CREATEDB and CREATEROLE".to_string()
        },
    );

    for db in &payload.databases {
        let exists = probe.existing_databases.contains(&db.name);
        let ok = !exists || overwrite;
        let detail = if !exists {
            format!("'{}' absent — will be created", db.name)
        } else if overwrite {
            format!("'{}' exists — will be overwritten (--overwrite)", db.name)
        } else {
            format!("'{}' already exists (use --overwrite to replace)", db.name)
        };
        pf = pf.check(format!("database:{}", db.name), ok, detail);

        match create_database(db, probe.dest_major) {
            Ok(_) => {
                pf = pf.check(
                    format!("database_locale:{}", db.name),
                    true,
                    format!(
                        "encoding and locale can be recreated on PostgreSQL {}",
                        probe.dest_major
                    ),
                );
            }
            Err(error) => {
                pf = pf.check(
                    format!("database_locale:{}", db.name),
                    false,
                    error.to_string(),
                );
            }
        }
    }

    // Informational only: free disk is not visible over a SQL connection.
    pf = pf.check(
        "estimated_size",
        true,
        format!(
            "≈ {} to restore; destination free space is not verifiable over SQL",
            human_bytes(estimated_bytes)
        ),
    );

    pf
}

async fn probe_dest(params: &PostgresParams, payload: &PgPlanPayload) -> Result<DestProbe> {
    let conn = PgConnection::connect(params, &params.bootstrap_database()).await?;

    let row = conn
        .client
        .query_one(
            "SELECT rolsuper, rolcreatedb, rolcreaterole \
             FROM pg_catalog.pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .map_err(|e| BackupError::phase_src(Phase::Validate, "probe current_user privileges", e))?;

    let names: Vec<String> = payload.databases.iter().map(|d| d.name.clone()).collect();
    let existing = conn
        .client
        .query(
            "SELECT datname::text FROM pg_catalog.pg_database WHERE datname = ANY($1)",
            &[&names],
        )
        .await
        .map_err(|e| BackupError::phase_src(Phase::Validate, "probe existing databases", e))?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

    Ok(DestProbe {
        dest_major: conn.server_major,
        is_super: row.get(0),
        createdb: row.get(1),
        createrole: row.get(2),
        existing_databases: existing,
    })
}

// --- restore (apply) ---------------------------------------------------------

/// Apply the streamed payload to reach 1:1 with the source (plan Phase 2.6):
/// roles → databases (cluster scope) → per-db structure (`pre_data`) → bulk data
/// via `COPY … FROM STDIN (FORMAT binary)` → per-db `post_data` (constraints,
/// indexes, sequence values, grants, owners). No temp files.
///
/// `CREATE DATABASE` cannot run inside a transaction, so cluster-scope DDL is
/// autocommit. Per-database apply is autocommit too in this phase; wrapping each
/// database in a transaction is a documented refinement. Verified end-to-end by
/// the postgres e2e (live restore is not runnable in a DB-less CI).
pub async fn stream_in(
    params: &PostgresParams,
    plan: &BackupPlan,
    src: &mut dyn ChunkSource,
) -> Result<()> {
    let payload: PgPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Apply, format!("bad plan payload: {e}")))?;

    // 1. Cluster scope: roles, then databases.
    // When overwriting `postgres` (or another configured bootstrap database),
    // run cluster DDL from a database that is not itself being replaced.
    let bootstrap = restore_bootstrap_database(params, &payload);
    let boot = PgConnection::connect(params, &bootstrap).await?;
    let ddl = build_cluster_ddl(&payload, boot.server_major)?;
    run_statements(&boot.client, &ddl.roles).await?;
    if params.overwrite {
        for database in &payload.databases {
            drop_database_for_overwrite(&boot.client, &database.name).await?;
        }
    }
    run_statements(&boot.client, &ddl.databases).await?;
    drop(boot);

    // 2. Per-database structure; keep each connection for the data load.
    let mut conns: HashMap<String, PgConnection> = HashMap::new();
    for dbddl in &ddl.per_database {
        let conn = PgConnection::connect(params, &dbddl.name).await?;
        run_statements(&conn.client, &dbddl.pre_data).await?;
        conns.insert(dbddl.name.clone(), conn);
    }

    // 3. Bulk data: one linear pass over the chunk stream, each item routed to a
    //    COPY … FROM STDIN on its database connection.
    let metas = item_metas(plan);
    apply_data(&conns, &metas, src).await?;

    // 4. Per-database post_data.
    for dbddl in &ddl.per_database {
        if let Some(conn) = conns.get(&dbddl.name) {
            run_statements(&conn.client, &dbddl.post_data).await?;
        }
    }
    Ok(())
}

/// Re-introspect every restored database plus the selected cluster-global
/// objects and compare them with the source plan. Planner estimates and server
/// version strings are normalized because they are not restored state.
pub async fn verify_catalog(params: &PostgresParams, plan: &BackupPlan) -> Result<()> {
    let mut expected: PgPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Verify, format!("bad plan payload: {e}")))?;
    let role_names: HashSet<_> = expected
        .roles
        .iter()
        .map(|role| role.name.clone())
        .collect();
    let tablespace_names: HashSet<_> = expected
        .tablespaces
        .iter()
        .map(|tablespace| tablespace.name.clone())
        .collect();

    let mut actual = PgPlanPayload::default();
    for (index, expected_db) in expected.databases.iter().enumerate() {
        let mut scoped = params.clone();
        scoped.database = Some(expected_db.name.clone());
        let mut observed = crate::introspect::introspect_cluster(&scoped)
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Verify,
                    format!("re-introspect PostgreSQL database {}", expected_db.name),
                    error,
                )
            })?;
        if index == 0 {
            actual.server_version = expected.server_version.clone();
            actual.server_major = expected.server_major;
            actual.roles = observed
                .roles
                .into_iter()
                .filter(|role| role_names.contains(&role.name))
                .collect();
            actual.memberships = observed
                .memberships
                .into_iter()
                .filter(|membership| {
                    role_names.contains(&membership.role) && role_names.contains(&membership.member)
                })
                .collect();
            actual.tablespaces = observed
                .tablespaces
                .into_iter()
                .filter(|tablespace| tablespace_names.contains(&tablespace.name))
                .collect();
        }
        let database = observed.databases.pop().ok_or_else(|| {
            BackupError::phase(
                Phase::Verify,
                format!(
                    "restored PostgreSQL database {:?} is missing",
                    expected_db.name
                ),
            )
        })?;
        actual.databases.push(database);
    }

    normalize_catalog(&mut expected);
    normalize_catalog(&mut actual);
    if actual != expected {
        let difference = first_catalog_difference(&expected, &actual);
        return Err(BackupError::phase(
            Phase::Verify,
            format!("PostgreSQL catalog read-back differs from the source plan: {difference}"),
        ));
    }
    Ok(())
}

fn first_catalog_difference(expected: &PgPlanPayload, actual: &PgPlanPayload) -> String {
    let expected = serde_json::to_value(expected).unwrap_or(serde_json::Value::Null);
    let actual = serde_json::to_value(actual).unwrap_or(serde_json::Value::Null);
    first_json_difference("$", &expected, &actual)
        .unwrap_or_else(|| "different serialized catalog values".to_string())
}

fn first_json_difference(
    path: &str,
    expected: &serde_json::Value,
    actual: &serde_json::Value,
) -> Option<String> {
    use serde_json::Value;
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for key in expected.keys().chain(actual.keys()) {
                match (expected.get(key), actual.get(key)) {
                    (Some(expected), Some(actual)) if expected != actual => {
                        return first_json_difference(&format!("{path}.{key}"), expected, actual);
                    }
                    (Some(_), None) => return Some(format!("{path}.{key} missing at destination")),
                    (None, Some(_)) => {
                        return Some(format!("{path}.{key} unexpectedly present at destination"));
                    }
                    _ => {}
                }
            }
            None
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                return Some(format!(
                    "{path} length source={} destination={}",
                    expected.len(),
                    actual.len()
                ));
            }
            for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                if expected != actual {
                    return first_json_difference(&format!("{path}[{index}]"), expected, actual);
                }
            }
            None
        }
        _ => Some(format!(
            "{path} source={} destination={}",
            compact_json(expected),
            compact_json(actual)
        )),
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    const MAX: usize = 240;
    let rendered = value.to_string();
    if rendered.chars().count() <= MAX {
        rendered
    } else {
        format!("{}…", rendered.chars().take(MAX).collect::<String>())
    }
}

fn normalize_catalog(payload: &mut PgPlanPayload) {
    for database in &mut payload.databases {
        for schema in &mut database.schemas {
            for table in &mut schema.tables {
                table.estimated_rows = 0;
                table.estimated_bytes = 0;
            }
        }
    }
}

/// Select a cluster database that is not one of the overwrite targets.
fn restore_bootstrap_database(params: &PostgresParams, payload: &PgPlanPayload) -> String {
    let preferred = params.bootstrap_database();
    if payload.databases.iter().all(|db| db.name != preferred) {
        return preferred;
    }
    if payload.databases.iter().all(|db| db.name != "postgres") {
        return "postgres".to_string();
    }
    // Source discovery excludes template databases, making template1 the safe
    // administrative fallback when the standard postgres database is a target.
    "template1".to_string()
}

fn alter_database_connections(name: &str, allowed: bool) -> String {
    format!(
        "ALTER DATABASE {} WITH ALLOW_CONNECTIONS = {};",
        quote_ident(name),
        allowed
    )
}

fn drop_database_sql(name: &str) -> String {
    format!("DROP DATABASE {};", quote_ident(name))
}

/// Replace one existing database on every supported PostgreSQL major. Disabling
/// new connections closes the race that a plain terminate-then-drop sequence
/// would leave on PostgreSQL 10–12 (which lack `DROP DATABASE ... FORCE`).
async fn drop_database_for_overwrite(client: &Client, name: &str) -> Result<()> {
    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .map_err(|error| {
            BackupError::phase_src(
                Phase::Apply,
                format!("check existing database {name:?}"),
                error,
            )
        })?
        .get(0);
    if !exists {
        return Ok(());
    }

    let disable = alter_database_connections(name, false);
    client.batch_execute(&disable).await.map_err(|error| {
        BackupError::phase_src(
            Phase::Apply,
            format!("disable connections before overwriting database {name:?}"),
            error,
        )
    })?;

    let terminated = client
        .query(
            "SELECT pg_catalog.pg_terminate_backend(pid) \
             FROM pg_catalog.pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_catalog.pg_backend_pid()",
            &[&name],
        )
        .await;
    let rows = match terminated {
        Ok(rows) => rows,
        Err(error) => {
            let _ = client
                .batch_execute(&alter_database_connections(name, true))
                .await;
            return Err(BackupError::phase_src(
                Phase::Apply,
                format!("terminate sessions before overwriting database {name:?}"),
                error,
            ));
        }
    };
    if rows.iter().any(|row| !row.get::<_, bool>(0)) {
        let _ = client
            .batch_execute(&alter_database_connections(name, true))
            .await;
        return Err(BackupError::phase(
            Phase::Apply,
            format!("not permitted to terminate every session on database {name:?}"),
        ));
    }

    let drop_sql = drop_database_sql(name);
    if let Err(error) = client.batch_execute(&drop_sql).await {
        let _ = client
            .batch_execute(&alter_database_connections(name, true))
            .await;
        return Err(BackupError::phase_src(
            Phase::Apply,
            format!("drop database {name:?} for overwrite"),
            error,
        ));
    }
    Ok(())
}

/// Map `item.id` → its COPY descriptor (table-kind items only).
fn item_metas(plan: &BackupPlan) -> HashMap<u32, ItemMeta> {
    plan.items
        .iter()
        .filter(|i| i.kind == "table")
        .filter_map(|i| {
            serde_json::from_value::<ItemMeta>(i.meta.clone())
                .ok()
                .map(|m| (i.id, m))
        })
        .collect()
}

async fn run_statements(client: &Client, stmts: &[String]) -> Result<()> {
    for stmt in stmts {
        if let Err(error) = client.batch_execute(stmt).await {
            // Bootstrap roles such as `postgres` exist on every fresh cluster.
            // Keep the restore idempotent there; subsequent ALTER/GRANT DDL still
            // applies source settings where the destination account permits it.
            if stmt.starts_with("CREATE ROLE ")
                && error.code() == Some(&tokio_postgres::error::SqlState::DUPLICATE_OBJECT)
            {
                tracing::debug!(%stmt, "role already exists; retaining destination bootstrap role");
                continue;
            }
            return Err(BackupError::phase_src(
                Phase::Apply,
                format!("apply DDL: {stmt}"),
                error,
            ));
        }
    }
    Ok(())
}

/// `COPY … FROM STDIN (FORMAT binary)` mirroring the source's `COPY … TO STDOUT`,
/// so the binary stream loads back verbatim.
fn copy_in_sql(meta: &ItemMeta) -> String {
    let qual = quote_qualified(&meta.schema, &meta.table);
    if meta.columns.is_empty() {
        format!("COPY {qual} FROM STDIN (FORMAT binary)")
    } else {
        let cols = meta
            .columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        format!("COPY {qual} ({cols}) FROM STDIN (FORMAT binary)")
    }
}

async fn apply_data(
    conns: &HashMap<String, PgConnection>,
    metas: &HashMap<u32, ItemMeta>,
    src: &mut dyn ChunkSource,
) -> Result<()> {
    // The open COPY sink for the item currently being loaded.
    let mut current: Option<ActiveCopy> = None;

    loop {
        match src.next().await? {
            ChunkEvent::Chunk { item_id, data, .. } => {
                let reopen = current
                    .as_ref()
                    .map(|(id, _, _)| *id != item_id)
                    .unwrap_or(true);
                if reopen {
                    finish_current(&mut current).await?;
                    let meta = metas.get(&item_id).ok_or_else(|| {
                        BackupError::phase(Phase::Apply, format!("data for unknown item {item_id}"))
                    })?;
                    let conn = conns.get(&meta.database).ok_or_else(|| {
                        BackupError::phase(
                            Phase::Apply,
                            format!("no connection for database '{}'", meta.database),
                        )
                    })?;
                    let sql = copy_in_sql(meta);
                    let sink = conn.client.copy_in::<_, Bytes>(&sql).await.map_err(|e| {
                        BackupError::phase_src(
                            Phase::Apply,
                            format!("copy_in {}.{}", meta.schema, meta.table),
                            e,
                        )
                    })?;
                    current = Some((item_id, 0, Box::pin(sink)));
                }
                if let Some((_, received, sink)) = &mut current {
                    *received += data.len() as u64;
                    sink.send(Bytes::from(data))
                        .await
                        .map_err(|e| BackupError::phase_src(Phase::Apply, "copy_in send", e))?;
                }
            }
            ChunkEvent::ItemEnd { item_id, total, .. } => {
                let (open_id, received, _) = current.as_ref().ok_or_else(|| {
                    BackupError::phase(
                        Phase::Apply,
                        format!("ItemEnd item={item_id} without open COPY"),
                    )
                })?;
                if *open_id != item_id || *received != total {
                    return Err(BackupError::phase(
                        Phase::Verify,
                        format!("ItemEnd item={item_id} does not match open item={open_id} bytes={received}"),
                    ));
                }
                finish_current(&mut current).await?
            }
            ChunkEvent::End => {
                finish_current(&mut current).await?;
                break;
            }
        }
    }
    Ok(())
}

type ActiveCopy = (u32, u64, Pin<Box<CopyInSink<Bytes>>>);

async fn finish_current(current: &mut Option<ActiveCopy>) -> Result<()> {
    if let Some((_, _, mut sink)) = current.take() {
        sink.as_mut()
            .finish()
            .await
            .map_err(|e| BackupError::phase_src(Phase::Apply, "copy_in finish", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(major: u32, is_super: bool, createdb: bool, createrole: bool) -> DestProbe {
        DestProbe {
            dest_major: major,
            is_super,
            createdb,
            createrole,
            existing_databases: HashSet::new(),
        }
    }

    fn params(database: Option<&str>) -> PostgresParams {
        PostgresParams {
            host: "localhost".into(),
            port: 5432,
            user: "postgres".into(),
            password: None,
            database: database.map(str::to_string),
            sslmode: "disable".into(),
            sslrootcert: None,
            admin: true,
            overwrite: true,
        }
    }

    #[test]
    fn overwrite_uses_a_non_target_bootstrap_database() {
        let mut payload = crate::model::test_fixture();
        assert_eq!(
            restore_bootstrap_database(&params(None), &payload),
            "postgres"
        );
        assert_eq!(
            restore_bootstrap_database(&params(Some("appdb")), &payload),
            "postgres"
        );

        payload.databases[0].name = "postgres".into();
        assert_eq!(
            restore_bootstrap_database(&params(None), &payload),
            "template1"
        );
    }

    #[test]
    fn overwrite_database_ddl_quotes_names_and_controls_connections() {
        assert_eq!(
            alter_database_connections("odd\"db", false),
            "ALTER DATABASE \"odd\"\"db\" WITH ALLOW_CONNECTIONS = false;"
        );
        assert_eq!(
            alter_database_connections("odd\"db", true),
            "ALTER DATABASE \"odd\"\"db\" WITH ALLOW_CONNECTIONS = true;"
        );
        assert_eq!(drop_database_sql("odd\"db"), "DROP DATABASE \"odd\"\"db\";");
    }

    #[test]
    fn passes_on_clean_destination() {
        let payload = crate::model::test_fixture(); // source major 16, db "appdb"
        let pf = assess(&payload, &probe(16, false, true, true), false, 1024);
        assert!(
            pf.ok,
            "clean newer destination should pass: {:?}",
            pf.checks
        );
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "database:appdb" && c.passed));
    }

    #[test]
    fn fails_when_destination_older() {
        let payload = crate::model::test_fixture();
        let pf = assess(&payload, &probe(15, true, true, true), false, 0);
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "server_version" && !c.passed));
    }

    #[test]
    fn preflight_rejects_incomplete_locale_metadata() {
        let mut payload = crate::model::test_fixture();
        payload.databases[0].locale_provider = Some(crate::model::PgLocaleProvider::Icu);
        payload.databases[0].provider_locale = None;

        let pf = assess(&payload, &probe(16, true, true, true), false, 0);
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "database_locale:appdb" && !c.passed));
    }

    #[test]
    fn fails_without_creation_privileges() {
        let payload = crate::model::test_fixture();
        // not super, missing CREATEROLE
        let pf = assess(&payload, &probe(16, false, true, false), false, 0);
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "privileges" && !c.passed));
        // superuser alone suffices.
        let pf2 = assess(&payload, &probe(16, true, false, false), false, 0);
        assert!(pf2
            .checks
            .iter()
            .any(|c| c.name == "privileges" && c.passed));
    }

    #[test]
    fn copy_in_sql_mirrors_copy_out() {
        let m = ItemMeta {
            database: "appdb".into(),
            schema: "app".into(),
            table: "accounts".into(),
            columns: vec!["id".into(), "email".into()],
        };
        assert_eq!(
            copy_in_sql(&m),
            "COPY \"app\".\"accounts\" (\"id\", \"email\") FROM STDIN (FORMAT binary)"
        );
        let m2 = ItemMeta {
            columns: vec![],
            ..m
        };
        assert_eq!(
            copy_in_sql(&m2),
            "COPY \"app\".\"accounts\" FROM STDIN (FORMAT binary)"
        );
    }

    #[test]
    fn item_metas_indexes_table_items_by_id() {
        let payload = crate::model::test_fixture();
        let bp = crate::introspect::build_plan(&payload, "t".to_string());
        let metas = item_metas(&bp);
        assert_eq!(metas.len(), 1);
        let m = metas.get(&0).expect("item 0");
        assert_eq!(m.database, "appdb");
        assert_eq!(m.schema, "app");
        assert_eq!(m.table, "accounts");
        assert_eq!(m.columns, vec!["id".to_string(), "email".to_string()]);
    }

    #[test]
    fn existing_database_blocks_unless_overwrite() {
        let payload = crate::model::test_fixture();
        let mut p = probe(16, true, true, true);
        p.existing_databases.insert("appdb".to_string());

        let blocked = assess(&payload, &p, false, 0);
        assert!(!blocked.ok, "existing db must block without --overwrite");
        assert!(blocked
            .checks
            .iter()
            .any(|c| c.name == "database:appdb" && !c.passed));

        let allowed = assess(&payload, &p, true, 0);
        assert!(
            allowed.ok,
            "--overwrite must allow restoring over existing db"
        );
    }
}
