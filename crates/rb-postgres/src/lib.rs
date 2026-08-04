#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! rust-backup PostgreSQL module.
//!
//! Logical, pure-Rust backup/restore for PostgreSQL 10..=latest via catalog
//! introspection + binary `COPY` streaming (no `pg_dump`). Built phase by phase
//! per `docs/plans/RUST_BACKUP_PLAN.md` §"Phase 2".

mod connect;
pub mod ddl;
mod dest;
mod immutability;
mod introspect;
mod model;
mod source;

pub use connect::{parse_major, PgConnection, MIN_PG_MAJOR};
pub use model::PgPlanPayload;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupPlan, Preflight};
use rb_core::verification::{RestoreEvidence, VerificationReport, VerificationSink};

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
        let payload = introspect::introspect_cluster(&self.params).await?;
        Ok(introspect::build_plan(&payload, introspect::now_rfc3339()))
    }

    async fn stream_out(&self, plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        source::stream_out(&self.params, plan, sink).await
    }

    async fn fingerprint(&self) -> Result<String> {
        immutability::fingerprint(&self.params).await
    }
}

/// PostgreSQL destination (restore).
struct PostgresDestination {
    #[allow(dead_code)]
    params: PostgresParams,
}

#[async_trait]
impl Destination for PostgresDestination {
    async fn validate(&self, plan: &BackupPlan) -> Result<Preflight> {
        dest::validate(&self.params, plan).await
    }

    async fn stream_in(&self, plan: &BackupPlan, src: &mut dyn ChunkSource) -> Result<()> {
        dest::stream_in(&self.params, plan, src).await
    }

    async fn verify(
        &self,
        plan: &BackupPlan,
        evidence: &RestoreEvidence,
    ) -> Result<VerificationReport> {
        dest::verify_catalog(&self.params, plan).await?;
        let mut verifier = VerificationSink::new(evidence);
        source::stream_out(&self.params, plan, &mut verifier)
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Verify,
                    "read back restored PostgreSQL table data",
                    error,
                )
            })?;
        verifier.finish().await?;
        verifier.report("PostgreSQL catalog and deterministic binary COPY read-back match")
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
