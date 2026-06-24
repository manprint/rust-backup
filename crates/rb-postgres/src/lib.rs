#![forbid(unsafe_code)]

//! rust-backup PostgreSQL module.
//!
//! Logical, pure-Rust backup/restore for PostgreSQL 10..=latest via catalog
//! introspection + binary `COPY` streaming (no `pg_dump`). Built phase by phase
//! per `docs/plans/RUST_BACKUP_PLAN.md` §"Phase 2".

mod connect;

pub use connect::{parse_major, PgConnection, MIN_PG_MAJOR};

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupPlan, Preflight};

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

    /// Destination-only: whether to connect as an admin user (for restore).
    /// Read-only source connections do not set this.
    #[serde(default)]
    pub admin: bool,
}

fn default_pg_port() -> u16 {
    5432
}

fn default_sslmode() -> String {
    "prefer".to_string()
}

/// PostgreSQL cluster-level backup plan payload.
///
/// Describes the complete schema, roles, and object hierarchy needed
/// to restore the cluster exactly as it was at analysis time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresPlan {
    /// PostgreSQL server version (e.g. "15.2").
    pub server_version: String,

    /// All roles in the cluster.
    pub roles: Vec<RoleDef>,

    /// All databases and their schemas.
    pub databases: Vec<DatabaseDef>,
}

/// A PostgreSQL role definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleDef {
    /// Role name.
    pub name: String,

    /// Whether this is a user (login privilege).
    pub is_user: bool,

    /// Comment/description.
    #[serde(default)]
    pub comment: Option<String>,
}

/// A PostgreSQL database definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseDef {
    /// Database name.
    pub name: String,

    /// Owner role name.
    pub owner: String,

    /// Server encoding (e.g. UTF8).
    pub encoding: String,

    /// Schemas in this database.
    pub schemas: Vec<SchemaDef>,

    /// Extension names installed in this database.
    pub extensions: Vec<String>,
}

/// A PostgreSQL schema definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDef {
    /// Schema name.
    pub name: String,

    /// Tables in this schema.
    pub tables: Vec<TableDef>,

    /// Sequences in this schema.
    pub sequences: Vec<SequenceDef>,

    /// Functions in this schema.
    pub functions: Vec<FunctionDef>,
}

/// A PostgreSQL table definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDef {
    /// Schema name (for full qualification).
    pub schema: String,

    /// Table name.
    pub name: String,

    /// Columns (name and type).
    pub columns: Vec<(String, String)>,

    /// Constraints (PK, FK, unique, check).
    pub constraints: Vec<String>,

    /// Index definitions (as DDL strings).
    pub indexes: Vec<String>,

    /// Owner role.
    pub owner: String,

    /// Grant statements.
    pub grants: Vec<String>,

    /// Estimated row count (for progress).
    #[serde(default)]
    pub estimated_rows: u64,

    /// Estimated size in bytes.
    #[serde(default)]
    pub estimated_bytes: u64,
}

/// A PostgreSQL sequence definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceDef {
    /// Sequence name.
    pub name: String,

    /// Current value.
    pub current_value: i64,
}

/// A PostgreSQL function definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    /// Function name with signature.
    pub signature: String,

    /// Function body (or OID reference).
    pub definition: String,
}

/// The PostgreSQL backup module.
pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn version_support(&self) -> &'static str {
        "PostgreSQL 10..=18 (logical, pure Rust)"
    }

    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>> {
        let pg_params: PostgresParams = params.deserialize()?;
        Ok(Box::new(PostgresSource { params: pg_params }))
    }

    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>> {
        let pg_params: PostgresParams = params.deserialize()?;
        Ok(Box::new(PostgresDestination { params: pg_params }))
    }
}

/// PostgreSQL source (read-only).
struct PostgresSource {
    #[allow(dead_code)]
    params: PostgresParams,
}

#[async_trait]
impl Source for PostgresSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-postgres: analyze not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_out(&self, _plan: &BackupPlan, _sink: &mut dyn ChunkSink) -> Result<()> {
        Err(BackupError::phase(
            Phase::Transfer,
            "rb-postgres: stream_out not yet implemented (plan Phase 2)",
        ))
    }

    async fn fingerprint(&self) -> Result<String> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-postgres: fingerprint not yet implemented (plan Phase 2)",
        ))
    }
}

/// PostgreSQL destination (restore).
struct PostgresDestination {
    #[allow(dead_code)]
    params: PostgresParams,
}

#[async_trait]
impl Destination for PostgresDestination {
    async fn validate(&self, _plan: &BackupPlan) -> Result<Preflight> {
        Ok(Preflight::pass().check(
            "not-implemented",
            false,
            "rb-postgres restore stub — destination validation and restore not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_in(&self, _plan: &BackupPlan, _src: &mut dyn ChunkSource) -> Result<()> {
        Err(BackupError::phase(
            Phase::Apply,
            "rb-postgres: stream_in not yet implemented (plan Phase 2)",
        ))
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

    #[tokio::test]
    async fn test_analyze_not_implemented() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "host": "localhost",
            "user": "postgres",
        }));

        let source = m.open_source(&params).await.unwrap();
        let result = source.analyze().await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not yet implemented"));
    }
}
