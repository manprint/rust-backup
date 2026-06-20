#![forbid(unsafe_code)]

//! rust-backup s3 module (stub).
//!
//! Provides typed parameter handling and plan shape for S3-compatible backups.
//! Real backup/restore logic lands in Phase 2.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupPlan, Preflight};

/// S3-compatible storage backup parameters.
///
/// Supports AWS S3, MinIO, and other S3-compatible APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Params {
    /// S3 endpoint URL (optional; if omitted, use AWS S3).
    /// For MinIO: http://localhost:9000 or https://minio.example.com
    #[serde(default)]
    pub endpoint: Option<String>,

    /// AWS region (e.g. us-east-1). Required for AWS S3; optional for MinIO.
    #[serde(default)]
    pub region: Option<String>,

    /// S3 bucket name (required).
    pub bucket: String,

    /// Prefix within the bucket (optional; if set, only back up keys with this prefix).
    #[serde(default)]
    pub prefix: Option<String>,

    /// AWS access key / MinIO access key ID (optional; use AWS credentials chain if omitted).
    #[serde(default)]
    pub access_key: Option<String>,

    /// AWS secret access key / MinIO secret access key (optional; use AWS credentials chain if omitted).
    #[serde(default)]
    pub secret_key: Option<String>,

    /// Use path-style URLs (for MinIO and some S3-compatible services).
    /// AWS S3 prefers virtual-hosted style (false).
    #[serde(default)]
    pub path_style: bool,
}

/// S3 backup plan payload.
///
/// Describes all buckets, objects, and metadata needed to restore the S3 state exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Plan {
    /// Source endpoint (or "aws-s3" for AWS S3).
    #[serde(default)]
    pub source_endpoint: Option<String>,

    /// All buckets and their objects.
    pub buckets: Vec<S3Bucket>,
}

/// An S3 bucket definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Bucket {
    /// Bucket name.
    pub name: String,

    /// All objects in the bucket.
    pub objects: Vec<S3Object>,

    /// Bucket policy (optional, as a JSON string).
    #[serde(default)]
    pub policy: Option<String>,
}

/// An S3 object definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Object {
    /// Object key (path).
    pub key: String,

    /// Object size in bytes.
    pub size: u64,

    /// ETag (usually an MD5 hash; useful for integrity verification).
    #[serde(default)]
    pub etag: Option<String>,

    /// Content-Type metadata.
    #[serde(default)]
    pub content_type: Option<String>,

    /// Storage class (STANDARD, GLACIER, etc.).
    #[serde(default)]
    pub storage_class: Option<String>,
}

/// The S3 backup module.
pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn version_support(&self) -> &'static str {
        "S3-compatible (AWS S3, MinIO)"
    }

    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>> {
        let s3_params: S3Params = params.deserialize()?;
        Ok(Box::new(S3Source { params: s3_params }))
    }

    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>> {
        let s3_params: S3Params = params.deserialize()?;
        Ok(Box::new(S3Destination { params: s3_params }))
    }
}

/// S3 source (read-only).
struct S3Source {
    #[allow(dead_code)]
    params: S3Params,
}

#[async_trait]
impl Source for S3Source {
    async fn analyze(&self) -> Result<BackupPlan> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-s3: analyze not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_out(&self, _plan: &BackupPlan, _sink: &mut dyn ChunkSink) -> Result<()> {
        Err(BackupError::phase(
            Phase::Transfer,
            "rb-s3: stream_out not yet implemented (plan Phase 2)",
        ))
    }

    async fn fingerprint(&self) -> Result<String> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-s3: fingerprint not yet implemented (plan Phase 2)",
        ))
    }
}

/// S3 destination (restore).
struct S3Destination {
    #[allow(dead_code)]
    params: S3Params,
}

#[async_trait]
impl Destination for S3Destination {
    async fn validate(&self, _plan: &BackupPlan) -> Result<Preflight> {
        Ok(Preflight::pass().check(
            "not-implemented",
            false,
            "rb-s3 restore stub — destination validation and restore not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_in(&self, _plan: &BackupPlan, _src: &mut dyn ChunkSource) -> Result<()> {
        Err(BackupError::phase(
            Phase::Apply,
            "rb-s3: stream_in not yet implemented (plan Phase 2)",
        ))
    }
}

/// Register the S3 module.
pub fn module() -> Arc<dyn rb_core::module::BackupModule> {
    Arc::new(Module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_name() {
        let m = Module;
        assert_eq!(m.name(), "s3");
    }

    #[tokio::test]
    async fn test_open_source_valid_params() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "bucket": "my-bucket"
        }));

        let source = m.open_source(&params).await;
        assert!(source.is_ok());
    }

    #[tokio::test]
    async fn test_analyze_not_implemented() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "bucket": "my-bucket"
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
