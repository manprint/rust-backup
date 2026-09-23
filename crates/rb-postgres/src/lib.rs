#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! rust-backup PostgreSQL module.
//!
//! Logical, pure-Rust backup/restore for PostgreSQL 10+ via catalog
//! introspection + binary `COPY` streaming (no `pg_dump`). Built phase by phase
//! per `docs/plans/RUST_BACKUP_PLAN.md` §"Phase 2".

mod connect;
pub mod ddl;
mod dest;
mod immutability;
mod introspect;
mod model;
mod source;

use connect::SourcePool;
pub use connect::{parse_major, PgConnection, MIN_PG_MAJOR};
pub use model::PgPlanPayload;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupMode, BackupPlan, Preflight};
use rb_core::verification::{RestoreEvidence, VerificationReport, VerificationSink};

/// What to install when the destination cannot provide the exact version of an
/// extension the source has.
///
/// An extension's version is part of what it *does* — a newer one can add a
/// function the restored data depends on, an older one can lack it — so
/// installing a different version silently is a change of behaviour the catalog
/// read-back would also have to accept. The choice is therefore explicit: refuse
/// (the default), or accept the destination's default version and say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionVersionPolicy {
    /// Install the source's exact version; refuse the plan when the destination
    /// does not have it.
    #[default]
    Source,
    /// Install whatever version the destination defaults to, and record the
    /// difference as a deviation in the verification report.
    Default,
}

/// PostgreSQL connection parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresParams {
    /// Host to connect to (required).
    pub host: String,

    /// Port (default 5432).
    #[serde(default = "default_pg_port")]
    pub port: u16,

    /// User for authentication (required for source; read-only recommended).
    pub user: String,

    /// Password (optional; set via PGPASSWORD if omitted).
    #[serde(default)]
    pub password: Option<String>,

    /// Database to back up (optional; if omitted, use postgres database).
    #[serde(default)]
    pub database: Option<String>,

    /// SSL mode: disable, allow, prefer, require (default: prefer).
    #[serde(default = "default_sslmode")]
    pub sslmode: String,
    /// PEM root CA used by verify-ca and verify-full.
    #[serde(default)]
    pub sslrootcert: Option<String>,

    /// Destination-only: whether to connect as an admin user (for restore).
    /// Read-only source connections do not set this.
    #[serde(default)]
    pub admin: bool,

    /// Destination-only: allow restoring over databases that already exist.
    /// Without it, preflight fails when a target database is present.
    #[serde(default)]
    pub overwrite: bool,

    /// Destination-only: which version of each source extension to install.
    /// Defaults to the source's exact version, which is refused at preflight
    /// when the destination does not carry it.
    #[serde(default)]
    pub extension_version: ExtensionVersionPolicy,

    /// Source-only: proceed even though the cluster holds object classes this
    /// build cannot reproduce (triggers, row-level security policies,
    /// user-defined types, aggregates, foreign tables, large objects, column
    /// or default ACLs, view options, `reg*` column values).
    ///
    /// Analysis refuses such a cluster by default: the objects are absent from
    /// the plan model, so the destination cannot restore them AND the catalog
    /// read-back — which compares the same model on both sides — cannot see
    /// their absence. The run would report a verified 1:1 copy of a cluster
    /// that had silently lost, for instance, every RLS policy. Setting this
    /// accepts a knowingly partial copy; the run then logs exactly what it
    /// leaves behind.
    #[serde(default)]
    pub allow_unsupported_objects: bool,

    /// Both sides: hot backup of a source that stays online.
    ///
    /// Source: each database is read through one `REPEATABLE READ, READ ONLY`
    /// transaction, so the catalog, the sequence values, the row counts and
    /// every `COPY` see a single consistent state even while the application
    /// writes; the session skips the immutability audit, which a live source
    /// fails by construction. The plan is marked
    /// [`BackupMode::HotSnapshot`].
    ///
    /// Destination: accepts such a plan. The restored copy is verified as
    /// strictly as a cold one — against the snapshot, not against the source's
    /// later state.
    #[serde(default)]
    pub hot_backup: bool,
}

fn default_pg_port() -> u16 {
    5432
}

fn default_sslmode() -> String {
    "prefer".to_string()
}

impl PostgresParams {
    /// The database to connect to first for cluster-global introspection
    /// (roles, tablespaces, the database list). Falls back to `postgres`.
    pub fn bootstrap_database(&self) -> String {
        self.database
            .clone()
            .unwrap_or_else(|| "postgres".to_string())
    }
}

/// The PostgreSQL backup module.
pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn max_carriers(&self) -> u32 {
        // One COPY sink is tied to one connection for v0.1.
        1
    }

    fn version_support(&self) -> &'static str {
        "PostgreSQL 10+ (logical, pure Rust)"
    }

    fn supports_hot_backup(&self) -> bool {
        true
    }

    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>> {
        let pg_params: PostgresParams = params.deserialize()?;
        let hot = pg_params.hot_backup;
        let pool = if hot {
            SourcePool::snapshot(pg_params)
        } else {
            SourcePool::new(pg_params)
        };
        Ok(Box::new(PostgresSource { pool, hot }))
    }

    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>> {
        let pg_params: PostgresParams = params.deserialize()?;
        Ok(Box::new(PostgresDestination {
            params: pg_params,
            created_databases: std::sync::Mutex::new(Vec::new()),
            table_rows: AtomicU64::new(0),
            derived_rows: AtomicU64::new(0),
        }))
    }
}

/// PostgreSQL source (read-only). Owns the connection pool every phase of the
/// run borrows from, so a whole backup costs one boot connection plus one per
/// database instead of one per phase.
struct PostgresSource {
    pool: SourcePool,
    /// Opened with `hot_backup`: the pool reads through one snapshot per
    /// database and the plan is a [`BackupMode::HotSnapshot`].
    hot: bool,
}

#[async_trait]
impl Source for PostgresSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        let mut payload = introspect::introspect_cluster(&self.pool).await?;
        // Counted here and not inside `introspect_cluster`: the same
        // introspection runs on both fingerprint audits and on the
        // destination's read-back, and none of those needs a second full scan
        // of every table.
        introspect::gather_row_counts(&self.pool, &mut payload).await?;
        let mut plan = introspect::build_plan(&payload, introspect::now_rfc3339());
        if self.hot {
            plan.mode = BackupMode::HotSnapshot;
        }
        Ok(plan)
    }

    async fn stream_out(&self, plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        source::stream_out(&self.pool, plan, sink).await
    }

    async fn fingerprint(&self) -> Result<String> {
        immutability::fingerprint(&self.pool).await
    }

    fn hot_backup(&self) -> bool {
        self.hot
    }
}

/// PostgreSQL destination (restore).
struct PostgresDestination {
    params: PostgresParams,
    /// Databases this run created, recorded while applying so that a failure
    /// after the load can still remove exactly them (`abandon`).
    created_databases: std::sync::Mutex<Vec<String>>,
    /// Rows written by the restore, handed from `stream_in` to `verify` — the
    /// trait hands the report back in a later call, and these are counted while
    /// applying. Atomics rather than a mutex: two counters, no poisoning.
    table_rows: AtomicU64,
    derived_rows: AtomicU64,
}

#[async_trait]
impl Destination for PostgresDestination {
    async fn validate(&self, plan: &BackupPlan) -> Result<Preflight> {
        dest::validate(&self.params, plan).await
    }

    fn accepts_hot_backup(&self) -> bool {
        self.params.hot_backup
    }

    async fn stream_in(&self, plan: &BackupPlan, src: &mut dyn ChunkSource) -> Result<()> {
        let rows = dest::stream_in(&self.params, plan, src).await?;
        self.table_rows.store(rows.table_rows, Ordering::Relaxed);
        self.derived_rows
            .store(rows.derived_rows, Ordering::Relaxed);
        // A poisoned lock here would mean another task panicked while holding
        // it; the restore itself is single-threaded, and losing the list would
        // only cost the cleanup, so the run is not failed for it.
        if let Ok(mut created) = self.created_databases.lock() {
            *created = rows.created_databases;
        }
        Ok(())
    }

    /// The load succeeded and the run failed anyway (the read-back said no, or
    /// the source changed under us): what is on disk looks complete and is not
    /// certified, so it goes.
    async fn abandon(&self) {
        let created = match self.created_databases.lock() {
            Ok(created) => created.clone(),
            Err(_) => return,
        };
        dest::abandon_created(&self.params, &created).await;
    }

    async fn verify(
        &self,
        plan: &BackupPlan,
        evidence: &RestoreEvidence,
    ) -> Result<VerificationReport> {
        let progress = rb_core::progress::current();
        if let Some(progress) = &progress {
            progress.set_detail("comparing the restored catalog with the plan");
        }
        let catalog = dest::verify_catalog(&self.params, plan).await?;
        if let Some(progress) = &progress {
            progress.set_detail("reading the restored tables back");
        }
        let mut verifier = VerificationSink::new(evidence);
        // The read-back runs the source path against the destination, on its
        // own read-only pool — it can never reach the source host.
        let read_back = SourcePool::new(self.params.clone());
        source::stream_out(&read_back, plan, &mut verifier)
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Verify,
                    "read back restored PostgreSQL table data",
                    error,
                )
            })?;
        verifier.finish().await?;
        let mut detail =
            "PostgreSQL catalog and deterministic binary COPY read-back match".to_string();
        // A deviation is not a failure, but it must reach the operator's
        // `RESTORE VERIFIED` line: the destination is 1:1 with the source
        // *except* for what is named here.
        // Also on the headline record, because the source peer reads the report
        // and never sees the destination's own lines.
        for deviation in &catalog.deviations {
            detail.push_str("; deviation: ");
            detail.push_str(deviation);
        }
        let table_rows = self.table_rows.load(Ordering::Relaxed);
        let derived_rows = self.derived_rows.load(Ordering::Relaxed);
        // The same facts as structured fields: the `RESTORE VERIFIED` lines are
        // for the operator reading a terminal, this is for whatever collects
        // the module's own events (I-OBSERV).
        tracing::info!(
            table_rows,
            derived_rows,
            constraints = catalog.constraints,
            constraints_not_valid = catalog.constraints_not_valid,
            deviations = catalog.deviations.len(),
            "PostgreSQL destination read-back verified"
        );
        verifier.report(detail).map(|report| {
            report
                .with_rows(table_rows, derived_rows)
                .with_constraints(catalog.constraints, catalog.constraints_not_valid)
                .with_deviations(catalog.deviations)
        })
    }
}

/// Register the PostgreSQL module.
pub fn module() -> Arc<dyn rb_core::module::BackupModule> {
    Arc::new(Module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_name() {
        let m = Module;
        assert_eq!(m.name(), "postgres");
    }

    #[tokio::test]
    async fn test_open_source_valid_params() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "host": "localhost",
            "user": "postgres",
        }));

        let source = m.open_source(&params).await;
        assert!(source.is_ok());
    }

    /// `hot_backup` is off unless asked for, and when asked for it reaches both
    /// sides: the source reports it to the session (which then skips the
    /// immutability audit) and the destination accepts a hot plan.
    #[tokio::test]
    async fn hot_backup_is_opt_in_on_both_sides() {
        let m = Module;
        assert!(m.supports_hot_backup());
        let cold = TargetParams::from_value(serde_json::json!({
            "host": "localhost",
            "user": "postgres",
        }));
        let hot = TargetParams::from_value(serde_json::json!({
            "host": "localhost",
            "user": "postgres",
            "hot_backup": true,
        }));
        assert!(!m.open_source(&cold).await.unwrap().hot_backup());
        assert!(m.open_source(&hot).await.unwrap().hot_backup());
        assert!(!m
            .open_destination(&cold)
            .await
            .unwrap()
            .accepts_hot_backup());
        assert!(m.open_destination(&hot).await.unwrap().accepts_hot_backup());
    }

    /// Live: a snapshot pool keeps reading the state it started from while
    /// another session commits, inside a read-only `REPEATABLE READ`
    /// transaction; an ordinary pool sees the commit. Runs only against a real
    /// server, as a role that may create a scratch table.
    #[tokio::test]
    async fn snapshot_pool_reads_one_state_while_another_session_writes() {
        let Some(params) = live_pg_params() else {
            eprintln!(
                "SKIP snapshot_pool_reads_one_state_while_another_session_writes: \
                 set RUST_BACKUP_PG_HOST to run"
            );
            return;
        };
        let database = params.bootstrap_database();
        let writer = PgConnection::connect(&params, &database)
            .await
            .expect("writer connection");
        let table = format!("rb_hot_snapshot_{}", std::process::id());
        writer
            .client
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {table}; CREATE TABLE {table} (id int); \
                 INSERT INTO {table} VALUES (1);"
            ))
            .await
            .expect("create the scratch table");
        let count = format!("SELECT count(*)::int8 FROM {table}");

        let snapshot = connect::SourcePool::snapshot(params.clone());
        let before: i64 = {
            let conn = snapshot.get(Some(&database)).await.expect("snapshot conn");
            let settings = conn
                .client
                .query_one(
                    "SELECT current_setting('transaction_isolation'), \
                     current_setting('transaction_read_only')",
                    &[],
                )
                .await
                .expect("transaction settings");
            assert_eq!(settings.get::<_, String>(0), "repeatable read");
            assert_eq!(settings.get::<_, String>(1), "on");
            conn.client
                .query_one(&count, &[])
                .await
                .expect("count")
                .get(0)
        };
        writer
            .client
            .batch_execute(&format!("INSERT INTO {table} VALUES (2)"))
            .await
            .expect("concurrent write");
        let after: i64 = {
            let conn = snapshot.get(Some(&database)).await.expect("snapshot conn");
            conn.client
                .query_one(&count, &[])
                .await
                .expect("count")
                .get(0)
        };
        let fresh: i64 = {
            let pool = connect::SourcePool::new(params.clone());
            let conn = pool.get(Some(&database)).await.expect("plain conn");
            conn.client
                .query_one(&count, &[])
                .await
                .expect("count")
                .get(0)
        };
        drop(snapshot);
        writer
            .client
            .batch_execute(&format!("DROP TABLE {table}"))
            .await
            .expect("drop the scratch table");

        assert_eq!(before, 1);
        assert_eq!(after, 1, "the snapshot must not see a later commit");
        assert_eq!(fresh, 2, "an ordinary pool sees the commit");
    }

    fn live_pg_params() -> Option<PostgresParams> {
        let host = std::env::var("RUST_BACKUP_PG_HOST").ok()?;
        Some(PostgresParams {
            host,
            port: std::env::var("RUST_BACKUP_PG_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            user: std::env::var("RUST_BACKUP_PG_USER").unwrap_or_else(|_| "postgres".into()),
            password: std::env::var("RUST_BACKUP_PG_PASSWORD").ok(),
            database: std::env::var("RUST_BACKUP_PG_DATABASE").ok(),
            sslmode: "prefer".into(),
            sslrootcert: None,
            admin: false,
            overwrite: false,
            extension_version: ExtensionVersionPolicy::Source,
            allow_unsupported_objects: false,
            hot_backup: false,
        })
    }

    /// analyze now performs a real read-only connection; with no server reachable
    /// it must fail in the Connect phase (not silently succeed). Points at a
    /// closed port for a deterministic, fast refusal.
    #[tokio::test]
    async fn analyze_fails_without_server() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "host": "127.0.0.1",
            "port": 1,
            "user": "postgres",
            "sslmode": "disable",
        }));

        let source = m.open_source(&params).await.unwrap();
        let err = source
            .analyze()
            .await
            .expect_err("must fail with no server");
        assert!(
            err.to_string().contains("[Connect]"),
            "expected a Connect-phase error, got: {err}"
        );
    }
}
