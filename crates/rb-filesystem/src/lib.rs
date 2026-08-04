#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! POSIX filesystem backup module.
//!
//! The source side only reads directory metadata and regular-file bytes.  The
//! plan is self-contained: it contains every path and its restore metadata;
//! only regular-file contents travel on the chunk channel.

mod dest;
mod immutability;
mod source;
mod walk;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use rb_core::channel::{ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{BackupPlan, Preflight};
use rb_core::verification::{RestoreEvidence, VerificationReport, VerificationSink};

/// Filesystem backup parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemParams {
    /// Root directory to back up or restore into.
    pub root: String,
    /// Following links is deliberately rejected: it can escape the declared root
    /// and cannot preserve a link as a link.
    #[serde(default)]
    pub follow_symlinks: bool,
    /// Attempt to restore uid/gid.  Requires root or CAP_CHOWN at destination.
    #[serde(default = "default_preserve_ownership")]
    pub preserve_ownership: bool,
    /// Reserved for a later safe, cross-platform xattr implementation.
    #[serde(default)]
    pub preserve_xattr: bool,
}

fn default_preserve_ownership() -> bool {
    true
}

/// Complete filesystem descriptor carried in [`BackupPlan::payload`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemPlan {
    /// Source root, for display only; restore always uses destination `root`.
    pub root: String,
    /// Entries sorted in deterministic relative-path order.
    pub entries: Vec<FilesystemEntry>,
    pub total_bytes: u64,
    pub ownership_note: String,
}

/// One entry in a filesystem plan.  `path` is always a validated relative path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemEntry {
    pub path: String,
    /// `file`, `dir`, `symlink`, or `hardlink`.
    pub kind: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
    #[serde(default)]
    pub mtime: u64,
    #[serde(default)]
    pub mtime_nsec: u32,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub hardlink_to: Option<String>,
    /// Kept in the plan format for forward compatibility.  Xattrs are not
    /// collected until a safe std+nix-only API is available.
    #[serde(default)]
    pub xattrs: BTreeMap<String, Vec<u8>>,
}

/// The filesystem module registration object.
pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "filesystem"
    }

    fn max_carriers(&self) -> u32 {
        32
    }

    fn version_support(&self) -> &'static str {
        "Filesystem (POSIX; ownership+perms preserved on Linux)"
    }

    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>> {
        let params: FilesystemParams = params.deserialize()?;
        source::validate_source_params(&params)?;
        Ok(Box::new(FilesystemSource { params }))
    }

    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>> {
        let params: FilesystemParams = params.deserialize()?;
        dest::validate_destination_params(&params)?;
        Ok(Box::new(FilesystemDestination { params }))
    }
}

struct FilesystemSource {
    params: FilesystemParams,
}

#[async_trait]
impl Source for FilesystemSource {
    async fn analyze(&self) -> Result<BackupPlan> {
        source::analyze(&self.params).await
    }

    async fn stream_out(&self, plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        source::stream_out(&self.params, plan, sink).await
    }

    async fn fingerprint(&self) -> Result<String> {
        immutability::fingerprint(&self.params).await
    }
}

struct FilesystemDestination {
    params: FilesystemParams,
}

#[async_trait]
impl Destination for FilesystemDestination {
    fn max_carriers(&self) -> usize {
        32
    }

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
        dest::verify_metadata(&self.params, plan)?;
        let mut verifier = VerificationSink::new(evidence);
        source::stream_out(&self.params, plan, &mut verifier)
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Verify,
                    "read back restored filesystem contents",
                    error,
                )
            })?;
        verifier.finish().await?;
        verifier.report(if self.params.preserve_ownership {
            "filesystem requested metadata contract and file contents read back exactly"
        } else {
            "filesystem metadata except opted-out ownership and file contents read back exactly"
        })
    }
}

/// Register the filesystem module.
pub fn module() -> Arc<dyn BackupModule> {
    Arc::new(Module)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use rb_core::channel::{ChunkEvent, ChunkSink, ChunkSource};
    use rb_core::verification::RestoreEvidence;

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        chunks: Vec<(u32, u64, Vec<u8>)>,
        completed: Vec<(u32, u64, String)>,
    }

    #[async_trait]
    impl ChunkSink for RecordingSink {
        async fn send_chunk(&mut self, id: u32, offset: u64, data: &[u8]) -> Result<()> {
            self.chunks.push((id, offset, data.to_vec()));
            Ok(())
        }
        async fn finish_item(&mut self, id: u32, total: u64, digest: &str) -> Result<()> {
            self.completed.push((id, total, digest.into()));
            Ok(())
        }
        async fn finish(&mut self) -> Result<()> {
            Ok(())
        }
    }

    impl RecordingSink {
        fn evidence(&self) -> RestoreEvidence {
            let item_blake3: BTreeMap<_, _> = self
                .completed
                .iter()
                .map(|(id, _, digest)| (*id, digest.clone()))
                .collect();
            RestoreEvidence {
                total_bytes: self.completed.iter().map(|(_, total, _)| total).sum(),
                payload_blake3: rb_core::channel::completion_digest(&item_blake3),
                item_blake3,
            }
        }
    }

    struct EventSource(VecDeque<ChunkEvent>);

    #[async_trait]
    impl ChunkSource for EventSource {
        async fn next(&mut self) -> Result<ChunkEvent> {
            Ok(self.0.pop_front().unwrap_or(ChunkEvent::End))
        }
    }

    fn tempdir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rb-filesystem-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn params(root: &Path) -> FilesystemParams {
        FilesystemParams {
            root: root.display().to_string(),
            follow_symlinks: false,
            preserve_ownership: false,
            preserve_xattr: false,
        }
    }

    fn fixture(root: &Path) {
        fs::create_dir(root.join("nested")).unwrap();
        let data: Vec<u8> = (0..(64 * 1024 + 19)).map(|n| (n % 251) as u8).collect();
        fs::write(root.join("nested/data.bin"), data).unwrap();
        fs::set_permissions(
            root.join("nested/data.bin"),
            fs::Permissions::from_mode(0o4640),
        )
        .unwrap();
        std::os::unix::fs::symlink("nested/data.bin", root.join("data-link")).unwrap();
        fs::hard_link(root.join("nested/data.bin"), root.join("data-hardlink")).unwrap();
        fs::write(root.join("empty"), []).unwrap();
    }

    #[tokio::test]
    async fn analyze_and_stream_are_complete_and_chunked() {
        let root = tempdir("source");
        fixture(&root);
        let source = FilesystemSource {
            params: params(&root),
        };
        let plan = source.analyze().await.unwrap();
        let payload: FilesystemPlan = serde_json::from_value(plan.payload.clone()).unwrap();
        assert!(payload
            .entries
            .iter()
            .any(|entry| entry.kind == "dir" && entry.path == "nested"));
        assert!(payload
            .entries
            .iter()
            .any(|entry| entry.kind == "symlink" && entry.path == "data-link"));
        assert!(payload.entries.iter().any(|entry| entry.kind == "hardlink"));
        assert_eq!(plan.items.len(), 2, "only unique regular files carry bytes");
        let mut sink = RecordingSink::default();
        source.stream_out(&plan, &mut sink).await.unwrap();
        assert!(sink
            .chunks
            .iter()
            .all(|(_, _, bytes)| bytes.len() <= 64 * 1024));
        assert_eq!(sink.completed.len(), 2);
        assert_eq!(
            sink.completed
                .iter()
                .map(|(_, total, _)| total)
                .sum::<u64>(),
            64 * 1024 + 19
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn roundtrip_restores_contents_links_and_mode() {
        let source_root = tempdir("roundtrip-source");
        let destination_root = tempdir("roundtrip-destination");
        fixture(&source_root);
        let source = FilesystemSource {
            params: params(&source_root),
        };
        let plan = source.analyze().await.unwrap();
        let mut sink = RecordingSink::default();
        source.stream_out(&plan, &mut sink).await.unwrap();
        let evidence = sink.evidence();
        let mut events = VecDeque::new();
        for (id, offset, data) in sink.chunks {
            events.push_back(ChunkEvent::Chunk {
                item_id: id,
                offset,
                data,
            });
            if let Some((_, total, digest)) = sink.completed.iter().find(|(done, _, _)| *done == id)
            {
                let last = events.back().is_some_and(|event| matches!(event, ChunkEvent::Chunk { data, .. } if offset + data.len() as u64 == *total));
                if last {
                    events.push_back(ChunkEvent::ItemEnd {
                        item_id: id,
                        total: *total,
                        blake3: digest.clone(),
                    });
                }
            }
        }
        for (id, total, digest) in &sink.completed {
            if !events
                .iter()
                .any(|event| matches!(event, ChunkEvent::ItemEnd { item_id, .. } if item_id == id))
            {
                events.push_back(ChunkEvent::ItemEnd {
                    item_id: *id,
                    total: *total,
                    blake3: digest.clone(),
                });
            }
        }
        events.push_back(ChunkEvent::End);
        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        destination
            .stream_in(&plan, &mut EventSource(events))
            .await
            .unwrap();
        let report = destination.verify(&plan, &evidence).await.unwrap();
        assert_eq!(report.items_verified, plan.items.len());
        assert_eq!(report.bytes_verified, evidence.total_bytes);
        assert_eq!(
            fs::read(destination_root.join("nested/data.bin")).unwrap(),
            fs::read(source_root.join("nested/data.bin")).unwrap()
        );
        assert_eq!(
            fs::read_link(destination_root.join("data-link")).unwrap(),
            PathBuf::from("nested/data.bin")
        );
        assert_eq!(
            fs::metadata(destination_root.join("nested/data.bin"))
                .unwrap()
                .ino(),
            fs::metadata(destination_root.join("data-hardlink"))
                .unwrap()
                .ino()
        );
        assert_eq!(
            fs::metadata(destination_root.join("nested/data.bin"))
                .unwrap()
                .mode()
                & 0o7777,
            0o4640
        );

        fs::write(
            destination_root.join("nested/data.bin"),
            b"post-restore corruption",
        )
        .unwrap();
        let error = destination
            .verify(&plan, &evidence)
            .await
            .expect_err("persisted corruption must invalidate a completed restore");
        assert!(
            error.to_string().contains("mismatch"),
            "unexpected verification error: {error}"
        );
        fs::remove_dir_all(source_root).unwrap();
        fs::remove_dir_all(destination_root).unwrap();
    }

    #[tokio::test]
    async fn interrupted_restore_removes_the_active_partial_file() {
        let source_root = tempdir("partial-source");
        let destination_root = tempdir("partial-destination");
        fixture(&source_root);
        let source = FilesystemSource {
            params: params(&source_root),
        };
        let plan = source.analyze().await.unwrap();
        let item = plan
            .items
            .iter()
            .find(|item| item.estimated_bytes > 16)
            .expect("fixture has a data item")
            .clone();
        let mut events = VecDeque::from([
            ChunkEvent::Chunk {
                item_id: item.id,
                offset: 0,
                data: vec![0x5a; 16],
            },
            ChunkEvent::End,
        ]);
        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        destination
            .stream_in(&plan, &mut EventSource(std::mem::take(&mut events)))
            .await
            .expect_err("truncated item must fail");
        assert!(
            !destination_root.join(&item.name).exists(),
            "an interrupted item must not remain at its final path"
        );
        fs::remove_dir_all(source_root).unwrap();
        fs::remove_dir_all(destination_root).unwrap();
    }

    #[tokio::test]
    async fn fingerprint_is_stable_then_detects_content_change() {
        let root = tempdir("fingerprint");
        fixture(&root);
        let source = FilesystemSource {
            params: params(&root),
        };
        let before = source.fingerprint().await.unwrap();
        assert_eq!(before, source.fingerprint().await.unwrap());
        fs::write(root.join("nested/data.bin"), b"changed").unwrap();
        assert_ne!(before, source.fingerprint().await.unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
