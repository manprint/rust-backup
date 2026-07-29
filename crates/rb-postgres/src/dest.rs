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

use crate::ddl::{build_cluster_ddl, quote_ident, quote_qualified};
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
    let ddl = build_cluster_ddl(&payload);

    // 1. Cluster scope: roles, then databases.
    let boot = PgConnection::connect(params, &params.bootstrap_database()).await?;
    run_statements(&boot.client, &ddl.roles).await?;
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
    let mut current: Option<(u32, Pin<Box<CopyInSink<Bytes>>>)> = None;

    loop {
        match src.next().await? {
            ChunkEvent::Chunk { item_id, data, .. } => {
                let reopen = current
                    .as_ref()
                    .map(|(id, _)| *id != item_id)
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
                    current = Some((item_id, Box::pin(sink)));
                }
                if let Some((_, sink)) = &mut current {
                    sink.send(Bytes::from(data))
                        .await
                        .map_err(|e| BackupError::phase_src(Phase::Apply, "copy_in send", e))?;
                }
            }
            ChunkEvent::ItemEnd { .. } => finish_current(&mut current).await?,
            ChunkEvent::End => {
                finish_current(&mut current).await?;
                break;
            }
        }
    }
    Ok(())
}

async fn finish_current(current: &mut Option<(u32, Pin<Box<CopyInSink<Bytes>>>)>) -> Result<()> {
    if let Some((_, mut sink)) = current.take() {
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
