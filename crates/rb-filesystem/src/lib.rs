#![forbid(unsafe_code)]

//! rust-backup filesystem module (stub).
//!
//! Provides typed parameter handling and plan shape for filesystem backups.
//! Real backup/restore logic lands in Phase 2.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupPlan, Preflight};

/// Filesystem backup parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemParams {
    /// Root directory to back up (required).
    pub root: String,

    /// Whether to follow symlinks (default false).
    #[serde(default)]
    pub follow_symlinks: bool,

    /// Whether to preserve ownership (uid/gid) — requires elevated privileges on restore.
    #[serde(default = "default_preserve_ownership")]
    pub preserve_ownership: bool,

    /// Whether to preserve extended attributes (default false).
    #[serde(default)]
    pub preserve_xattr: bool,
}

fn default_preserve_ownership() -> bool {
    true
}

/// Filesystem backup plan payload.
///
/// Describes the complete directory tree, file metadata, symlinks, and
/// extended attributes needed to restore the filesystem exactly as it was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemPlan {
    /// Root directory path.
    pub root: String,

    /// All filesystem entries (files, directories, symlinks).
    pub entries: Vec<FilesystemEntry>,

    /// Total size in bytes across all entries.
    pub total_bytes: u64,

    /// Note: restoring with ownership preservation may require root or CAP_CHOWN.
    #[serde(default)]
    pub ownership_note: String,
}

/// A single filesystem entry (file, directory, or symlink).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemEntry {
    /// Relative path from root.
    pub path: String,

    /// Entry kind: "file", "dir", or "symlink".
    pub kind: String,

    /// File size in bytes (0 for directories).
    #[serde(default)]
    pub size: u64,

    /// Unix file mode (permissions, file type bits).
    #[serde(default)]
    pub mode: u32,

    /// User ID (only preserved if params.preserve_ownership).
    #[serde(default)]
    pub uid: u32,

    /// Group ID (only preserved if params.preserve_ownership).
    #[serde(default)]
    pub gid: u32,

    /// Modification time (Unix timestamp in seconds).
    #[serde(default)]
    pub mtime: u64,

    /// Symlink target (only set if kind == "symlink").
    #[serde(default)]
    pub target: Option<String>,
}

/// The filesystem backup module.
pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "filesystem"
    }

    fn version_support(&self) -> &'static str {
        "Filesystem (POSIX; ownership+perms preserved on Linux)"
    }

    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>> {
        let fs_params: FilesystemParams = params.deserialize()?;
        Ok(Box::new(FilesystemSource { params: fs_params }))
    }

    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>> {
        let fs_params: FilesystemParams = params.deserialize()?;
        Ok(Box::new(FilesystemDestination { params: fs_params }))
    }
}

/// Filesystem source (read-only).
struct FilesystemSource {
    #[allow(dead_code)]
    params: FilesystemParams,
}

#[async_trait]
impl Source for FilesystemSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-filesystem: analyze not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_out(&self, _plan: &BackupPlan, _sink: &mut dyn ChunkSink) -> Result<()> {
        Err(BackupError::phase(
            Phase::Transfer,
            "rb-filesystem: stream_out not yet implemented (plan Phase 2)",
        ))
    }

    async fn fingerprint(&self) -> Result<String> {
        Err(BackupError::phase(
            Phase::Analyze,
            "rb-filesystem: fingerprint not yet implemented (plan Phase 2)",
        ))
    }
}

/// Filesystem destination (restore).
struct FilesystemDestination {
    #[allow(dead_code)]
    params: FilesystemParams,
}

#[async_trait]
impl Destination for FilesystemDestination {
    async fn validate(&self, _plan: &BackupPlan) -> Result<Preflight> {
        Ok(Preflight::pass().check(
            "not-implemented",
            false,
            "rb-filesystem restore stub — destination validation and restore not yet implemented (plan Phase 2)",
        ))
    }

    async fn stream_in(&self, _plan: &BackupPlan, _src: &mut dyn ChunkSource) -> Result<()> {
        Err(BackupError::phase(
            Phase::Apply,
            "rb-filesystem: stream_in not yet implemented (plan Phase 2)",
        ))
    }
}

/// Register the filesystem module.
pub fn module() -> Arc<dyn rb_core::module::BackupModule> {
    Arc::new(Module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_name() {
        let m = Module;
        assert_eq!(m.name(), "filesystem");
    }

    #[tokio::test]
    async fn test_open_source_valid_params() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "root": "/home/user"
        }));

        let source = m.open_source(&params).await;
        assert!(source.is_ok());
    }

    #[tokio::test]
    async fn test_analyze_not_implemented() {
        let m = Module;
        let params = TargetParams::from_value(serde_json::json!({
            "root": "/home/user"
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
