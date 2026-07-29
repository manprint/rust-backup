#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! rust-backup MongoDB module.
//!
//! Logical, pure-Rust backup/restore for MongoDB 4..=8 via catalog introspection
//! and BSON document streaming (no `mongodump`). Built phase by phase per the
//! plan's Phase 3 (`docs/plans/RUST_BACKUP_PLAN.md`).

mod connect;
mod dest;
mod immutability;
mod introspect;
mod model;
mod source;

pub use connect::{parse_major, MongoConnection, MIN_MONGO_MAJOR};
pub use model::{MongoCollection, MongoDatabase, MongoPlanPayload, MongoUser};

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::Result;
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupPlan, Preflight};

/// MongoDB connection parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoDbParams {
    /// Full connection URI (e.g. `mongodb://host:27017/db`). When set, the
    /// discrete host/port/credential fields are ignored.
    #[serde(default)]
    pub uri: Option<String>,

    /// Host to connect to (used if `uri` is unset).
    #[serde(default = "default_mongo_host")]
    pub host: String,

    /// Port (default 27017, used if `uri` is unset).
    #[serde(default = "default_mongo_port")]
    pub port: u16,

    /// Username for authentication.
    #[serde(default)]
    pub user: Option<String>,

    /// Password for authentication.
    #[serde(default)]
    pub password: Option<String>,

    /// Database to back up (optional; if omitted, back up all non-system dbs).
    #[serde(default)]
    pub database: Option<String>,

    /// Authentication database (usually `admin`).
    #[serde(default)]
    pub auth_db: Option<String>,

    /// Destination-only: allow restoring over collections that already exist
    /// (drops them first). Without it, preflight fails when a target exists.
    #[serde(default)]
    pub overwrite: bool,
}

fn default_mongo_host() -> String {
    "localhost".to_string()
}

fn default_mongo_port() -> u16 {
    27017
}

impl MongoDbParams {
    /// The database to run cluster-level/unprivileged commands against
    /// (`buildInfo`, `usersInfo`). Prefers the auth database, then the target
    /// database, then `admin`.
    pub fn command_db(&self) -> String {
        self.auth_db
            .clone()
            .or_else(|| self.database.clone())
            .unwrap_or_else(|| "admin".to_string())
    }
}

/// The MongoDB backup module.
pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "mongodb"
    }

    fn version_support(&self) -> &'static str {
        "MongoDB 4..=8 (logical, pure Rust)"
    }

    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>> {
        let mongo_params: MongoDbParams = params.deserialize()?;
        Ok(Box::new(MongoDbSource {
            params: mongo_params,
        }))
    }

    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>> {
        let mongo_params: MongoDbParams = params.deserialize()?;
        Ok(Box::new(MongoDbDestination {
            params: mongo_params,
        }))
    }
}

/// MongoDB source (read-only).
struct MongoDbSource {
    params: MongoDbParams,
}

#[async_trait]
impl Source for MongoDbSource {
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

/// MongoDB destination (restore).
struct MongoDbDestination {
    params: MongoDbParams,
}

#[async_trait]
impl Destination for MongoDbDestination {
    async fn validate(&self, plan: &BackupPlan) -> Result<Preflight> {
        dest::validate(&self.params, plan).await
    }

    async fn stream_in(&self, plan: &BackupPlan, src: &mut dyn ChunkSource) -> Result<()> {
        dest::stream_in(&self.params, plan, src).await
    }
}

/// Register the MongoDB module.
pub fn module() -> Arc<dyn rb_core::module::BackupModule> {
    Arc::new(Module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_name() {
        let m = Module;
        assert_eq!(m.name(), "mongodb");
    }

    #[test]
    fn command_db_precedence() {
        let mut p = MongoDbParams {
            uri: None,
            host: "h".into(),
            port: 27017,
            user: None,
            password: None,
            database: Some("appdb".into()),
            auth_db: Some("admin".into()),
            overwrite: false,
        };
        assert_eq!(p.command_db(), "admin");
        p.auth_db = None;
        assert_eq!(p.command_db(), "appdb");
        p.database = None;
        assert_eq!(p.command_db(), "admin");
    }

    #[tokio::test]
    async fn test_open_source_valid_params() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "uri": "mongodb://localhost:27017/mydb"
        }));
        let source = m.open_source(&params).await;
        assert!(source.is_ok());
    }

    /// analyze performs a real connection; with no server reachable it must fail
    /// in the Connect phase (not silently succeed). Points at a closed port for a
    /// deterministic, fast refusal.
    #[tokio::test]
    async fn analyze_fails_without_server() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "host": "127.0.0.1",
            "port": 1,
            // short server-selection timeout so the test fails fast.
            "uri": "mongodb://127.0.0.1:1/?serverSelectionTimeoutMS=800"
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
