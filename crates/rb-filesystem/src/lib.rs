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
    /// Signed: pre-1970 modification times are legal and must round-trip.
    #[serde(default)]
    pub mtime: i64,
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

    /// Build a plan whose payload is exactly `entries`, with no data items.
    fn hostile_plan(root: &Path, entries: Vec<FilesystemEntry>) -> BackupPlan {
        let payload = FilesystemPlan {
            root: root.display().to_string(),
            entries,
            total_bytes: 0,
            ownership_note: String::new(),
        };
        BackupPlan {
            format_version: rb_core::plan::PLAN_FORMAT_VERSION,
            module: "filesystem".into(),
            mode: rb_core::plan::BackupMode::Copy1to1,
            created_at: "1970-01-01T00:00:00Z".into(),
            source_summary: "hostile".into(),
            items: Vec::new(),
            estimated_bytes: 0,
            integrity: rb_core::plan::IntegritySpec::default(),
            payload: serde_json::to_value(payload).unwrap(),
        }
    }

    fn entry(path: &str, kind: &str, mode: u32) -> FilesystemEntry {
        FilesystemEntry {
            path: path.into(),
            kind: kind.into(),
            size: 0,
            mode,
            uid: 0,
            gid: 0,
            mtime: 0,
            mtime_nsec: 0,
            target: None,
            hardlink_to: None,
            xattrs: BTreeMap::new(),
        }
    }

    /// A plan that plants a symlink at a path and then declares a FILE at the
    /// same path must never reach `chmod`: the mode would be applied to the
    /// link's target, i.e. to an arbitrary file outside the destination root,
    /// with setuid bits included, on a restore that runs as root.
    #[tokio::test]
    async fn a_plan_cannot_chmod_through_a_planted_symlink() {
        let destination_root = tempdir("symlink-chmod-destination");
        let victim_dir = tempdir("symlink-chmod-victim");
        let victim = victim_dir.join("victim");
        fs::write(&victim, b"untouched").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();

        let mut link = entry("x", "symlink", 0o120_777);
        link.target = Some(victim.display().to_string());
        let plan = hostile_plan(&destination_root, vec![link, entry("x", "file", 0o104_777)]);

        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        let mut source = EventSource(VecDeque::from([ChunkEvent::End]));
        let error = destination
            .stream_in(&plan, &mut source)
            .await
            .expect_err("a duplicate plan path must be refused");
        assert!(
            error
                .to_string()
                .contains("duplicate filesystem plan entry"),
            "unexpected error: {error}"
        );

        assert_eq!(
            fs::symlink_metadata(&victim).unwrap().permissions().mode() & 0o7777,
            0o600,
            "the victim's mode must be untouched"
        );
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
        let _ = fs::remove_dir_all(&destination_root);
        let _ = fs::remove_dir_all(&victim_dir);
    }

    /// The mode application itself must be no-follow, independently of the
    /// plan-level guards above: a symlink at the target path fails the open
    /// rather than re-permissioning whatever it points at.
    #[test]
    fn chmod_never_follows_a_symlink() {
        let root = tempdir("nofollow");
        let victim = root.join("victim");
        fs::write(&victim, b"untouched").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        let planted = root.join("planted");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();

        let error = crate::dest::chmod_no_follow(&planted, &entry("planted", "file", 0o104_777))
            .expect_err("chmod through a symlink must fail");
        assert!(
            error.to_string().to_lowercase().contains("symbolic link"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::symlink_metadata(&victim).unwrap().permissions().mode() & 0o7777,
            0o600,
            "the victim's mode must be untouched"
        );
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");

        // A real file at the path is still chmod'd exactly.
        let real = root.join("real");
        fs::write(&real, b"x").unwrap();
        crate::dest::chmod_no_follow(&real, &entry("real", "file", 0o104_755)).unwrap();
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o7777,
            0o4755
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The destination refuses to restore into a non-empty root, so a plan can
    /// never act on state it did not create.
    #[tokio::test]
    async fn a_non_empty_destination_is_refused() {
        let destination_root = tempdir("non-empty-destination");
        fs::write(destination_root.join("pre-existing"), b"x").unwrap();
        let plan = hostile_plan(&destination_root, vec![entry("f", "file", 0o100_644)]);
        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        let mut source = EventSource(VecDeque::from([ChunkEvent::End]));
        let error = destination
            .stream_in(&plan, &mut source)
            .await
            .expect_err("a non-empty destination must be refused");
        assert!(
            error.to_string().contains("must be empty"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_dir_all(&destination_root);
    }

    /// `"."`, `"a/./b"` and `".."` are all outside the set of restorable paths.
    #[test]
    fn only_canonical_plain_relative_paths_are_accepted() {
        for path in [".", "a/./b", "..", "a/../b", "/abs", "", "a//b", "./a"] {
            assert!(
                crate::walk::relative_path(path, Phase::Validate).is_err(),
                "{path:?} must be refused"
            );
        }
        for path in ["a", "a/b", "a/b/c.txt"] {
            assert!(
                crate::walk::relative_path(path, Phase::Validate).is_ok(),
                "{path:?} must be accepted"
            );
        }
    }

    /// The plan cannot claim extended attributes this build never restores.
    #[tokio::test]
    async fn a_plan_carrying_xattrs_is_refused_at_validate() {
        let destination_root = tempdir("xattr-destination");
        let mut with_xattr = entry("f", "file", 0o100_644);
        with_xattr
            .xattrs
            .insert("user.thing".into(), b"value".to_vec());
        let plan = hostile_plan(&destination_root, vec![with_xattr]);
        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        let preflight = destination.validate(&plan).await;
        let error = match preflight {
            Ok(report) => {
                assert!(!report.ok, "xattrs must fail preflight");
                let _ = fs::remove_dir_all(&destination_root);
                return;
            }
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("extended attributes"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_dir_all(&destination_root);
    }

    /// A pre-1970 modification time must round-trip instead of being clamped.
    #[test]
    fn pre_epoch_mtimes_are_preserved() {
        let root = tempdir("pre-epoch");
        let file = root.join("old");
        fs::write(&file, b"x").unwrap();
        nix::sys::stat::utimensat(
            None,
            &file,
            &nix::sys::time::TimeSpec::new(-86_400, 0),
            &nix::sys::time::TimeSpec::new(-86_400, 0),
            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
        let plan = crate::walk::collect(&root).unwrap();
        let old = plan
            .entries
            .iter()
            .find(|entry| entry.path == "old")
            .unwrap();
        assert_eq!(old.mtime, -86_400);
        let _ = fs::remove_dir_all(&root);
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
