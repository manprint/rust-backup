#![forbid(unsafe_code)]

//! rust-backup mongodb module (stub).
//!
//! Provides typed parameter handling and plan shape for MongoDB backups.
//! Real backup/restore logic lands in Phase 2.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupPlan, Preflight};

/// MongoDB connection parameters.
///
/// Supports either a URI string or explicit host/port/credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoDbParams {
    /// Connection URI (e.g. mongodb://host:27017/db). Takes precedence if set.
    #[serde(default)]
    pub uri: Option<String>,

    /// Host to connect to (used if uri is not set).
    #[serde(default)]
    pub host: Option<String>,

    /// Port (default 27017, used if uri is not set).
    #[serde(default = "default_mongo_port")]
    pub port: u16,

    /// Username for authentication.
    #[serde(default)]
    pub user: Option<String>,

    /// Password for authentication.
    #[serde(default)]
    pub password: Option<String>,

    /// Database to back up (optional; if omitted, back up all).
    #[serde(default)]
    pub database: Option<String>,

    /// Authentication database (usually 'admin').
    #[serde(default)]
    pub auth_db: Option<String>,
}

fn default_mongo_port() -> u16 {
    27017
}

/// MongoDB cluster backup plan payload.
///
/// Describes all databases, collections, indexes, and user definitions
/// needed to restore the cluster exactly as it was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoDbPlan {
    /// MongoDB server version (e.g. "6.0.8").
    pub server_version: String,

    /// All databases and their collections.
    pub databases: Vec<MongoDatabase>,
}

/// A MongoDB database definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoDatabase {
    /// Database name.
    pub name: String,

    /// Collections in this database.
    pub collections: Vec<MongoCollection>,

    /// Users defined in this database.
    pub users: Vec<MongoUser>,
}

/// A MongoDB collection definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoCollection {
    /// Collection name.
    pub name: String,

    /// Collection options (as a JSON object).
    #[serde(default)]
    pub options: Option<serde_json::Value>,

    /// Index names (full index specs stored separately in options).
    pub indexes: Vec<String>,

    /// Estimated document count.
    #[serde(default)]
    pub estimated_docs: u64,

    /// Estimated size in bytes.
    #[serde(default)]
    pub estimated_bytes: u64,
}

/// A MongoDB user definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MongoUser {
    /// Username.
    pub name: String,

    /// Role assignments.
    pub roles: Vec<String>,
}

/// The MongoDB backup module.
pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "mongodb"
    }

    fn version_support(&self) -> &'static str {
        "MongoDB 4..=8"
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
    #[allow(dead_code)]
    params: MongoDbParams,
}

#[async_trait]
impl Source for MongoDbSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-mongodb: analyze not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_out(&self, _plan: &BackupPlan, _sink: &mut dyn ChunkSink) -> Result<()> {
        Err(BackupError::phase(
            Phase::Transfer,
            "rb-mongodb: stream_out not yet implemented (plan Phase 2)",
        ))
    }

    async fn fingerprint(&self) -> Result<String> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-mongodb: fingerprint not yet implemented (plan Phase 2)",
        ))
    }
}

/// MongoDB destination (restore).
struct MongoDbDestination {
    #[allow(dead_code)]
    params: MongoDbParams,
}

#[async_trait]
impl Destination for MongoDbDestination {
    async fn validate(&self, _plan: &BackupPlan) -> Result<Preflight> {
        Ok(Preflight::pass().check(
            "not-implemented",
            false,
            "rb-mongodb restore stub — destination validation and restore not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_in(&self, _plan: &BackupPlan, _src: &mut dyn ChunkSource) -> Result<()> {
        Err(BackupError::phase(
            Phase::Apply,
            "rb-mongodb: stream_in not yet implemented (plan Phase 2)",
        ))
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

    #[tokio::test]
    async fn test_open_source_valid_params() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "uri": "mongodb://localhost:27017/mydb"
        }));

        let source = m.open_source(&params).await;
        assert!(source.is_ok());
    }

    #[tokio::test]
    async fn test_analyze_not_implemented() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "uri": "mongodb://localhost:27017/mydb"
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
