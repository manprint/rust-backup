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
    /// Source only: accept that reading a file moves its access time.
    ///
    /// `O_NOATIME` needs file ownership or `CAP_FOWNER`; without either the
    /// kernel answers `EPERM` and reading the file updates its atime, which is
    /// a change to the source (I-IMMUT). Off by default, so such a run fails
    /// during the first fingerprint instead of silently touching the tree.
    #[serde(default)]
    pub allow_atime_updates: bool,
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
    /// `file`, `dir`, `symlink`, `hardlink`, `fifo`, `chardev` or `blockdev`.
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
    /// Device number of a `chardev`/`blockdev` entry, `None` for every other
    /// kind. Absent from plans written before device nodes were supported,
    /// which is why it defaults.
    #[serde(default)]
    pub rdev: Option<u64>,
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
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
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
            allow_atime_updates: false,
        }
    }

    // --- the atime guard (I-IMMUT) ------------------------------------------

    #[test]
    fn noatime_open_succeeds_for_owner() {
        // The test process owns the file it just created, so `O_NOATIME` is
        // permitted and nothing falls back.
        let root = tempdir("noatime-owner");
        let file = root.join("f");
        fs::write(&file, b"x").unwrap();
        let reader = crate::source::NoAtimeReader::open(&file, false, Phase::Analyze);
        assert!(reader.is_ok(), "owner open must succeed");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn noatime_eperm_is_refused_without_flag() {
        let root = tempdir("noatime-eperm");
        let file = root.join("f");
        fs::write(&file, b"x").unwrap();
        let mut attempts = 0;
        let error =
            crate::source::NoAtimeReader::open_with(&file, false, Phase::Analyze, |_, _| {
                attempts += 1;
                Err(nix::errno::Errno::EPERM)
            })
            .expect_err("EPERM without the flag must fail the run");
        assert_eq!(attempts, 1, "the refusal must not retry without O_NOATIME");
        let text = error.to_string();
        assert!(text.contains("without updating its access time"), "{text}");
        assert!(text.contains("--allow-atime-updates"), "{text}");
        assert!(text.contains("[Analyze]"), "the caller's phase: {text}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn noatime_eperm_falls_back_with_flag() {
        let root = tempdir("noatime-accepted");
        let file = root.join("f");
        fs::write(&file, b"payload").unwrap();
        let mut flags_seen = Vec::new();
        let reader =
            crate::source::NoAtimeReader::open_with(&file, true, Phase::Transfer, |path, flags| {
                flags_seen.push(flags);
                if flags.contains(nix::fcntl::OFlag::O_NOATIME) {
                    return Err(nix::errno::Errno::EPERM);
                }
                nix::fcntl::open(path, flags, nix::sys::stat::Mode::empty())
            });
        assert!(reader.is_ok(), "the flag must allow the plain open");
        assert_eq!(flags_seen.len(), 2, "one O_NOATIME attempt, one fallback");
        assert!(!flags_seen[1].contains(nix::fcntl::OFlag::O_NOATIME));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn allow_atime_updates_defaults_to_refusing() {
        // A plan or session file written before the flag existed must not
        // silently opt in.
        let params: FilesystemParams =
            serde_json::from_value(serde_json::json!({ "root": "/tmp" })).unwrap();
        assert!(!params.allow_atime_updates);
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

    // --- destination TOCTOU (§ 5.4) -----------------------------------------

    /// `create_dir_all` follows a symlink that points at a directory and calls
    /// it an existing directory. One planted between the emptiness check and
    /// directory creation would therefore redirect the whole restore outside
    /// the declared root.
    #[tokio::test]
    async fn a_planted_symlink_directory_is_refused_during_restore() {
        let destination_root = tempdir("toctou-destination");
        let outside = tempdir("toctou-outside");
        std::os::unix::fs::symlink(&outside, destination_root.join("a")).unwrap();
        let payload = FilesystemPlan {
            root: destination_root.display().to_string(),
            entries: vec![
                entry("a", "dir", 0o040_755),
                entry("a/f.txt", "file", 0o100_644),
            ],
            total_bytes: 0,
            ownership_note: String::new(),
        };
        let error = crate::dest::create_directories(&destination_root, &payload)
            .expect_err("a symlink in place of a planned directory must be refused");
        assert!(
            matches!(
                &error,
                rb_core::error::BackupError::Phase {
                    phase: Phase::Apply,
                    ..
                }
            ),
            "the refusal must be Apply-phase: {error:?}"
        );
        assert!(
            error.to_string().contains("replaced by a symlink"),
            "unexpected error: {error}"
        );
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing may have been created through the link"
        );
        fs::remove_dir_all(&destination_root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    /// And the same for the root itself: a symlinked destination root would
    /// redirect every path under it.
    #[tokio::test]
    async fn a_symlink_destination_root_is_refused() {
        let parent = tempdir("toctou-root");
        let real = parent.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = parent.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = crate::dest::create_destination_root(&link)
            .expect_err("a symlinked destination root must be refused");
        assert!(
            matches!(
                &error,
                rb_core::error::BackupError::Phase {
                    phase: Phase::Validate,
                    ..
                }
            ),
            "the refusal must be Validate-phase: {error:?}"
        );
        assert!(
            error.to_string().contains("destination root is a symlink"),
            "unexpected error: {error}"
        );
        // A real, absent root is still created.
        let fresh = parent.join("fresh");
        crate::dest::create_destination_root(&fresh).unwrap();
        assert!(fs::symlink_metadata(&fresh).unwrap().is_dir());
        fs::remove_dir_all(&parent).ok();
    }

    // --- special files (§ 5.1) ----------------------------------------------

    /// A FIFO is a node, not a stream: nothing is read from it, and what has to
    /// survive is the node plus its mode and mtime.
    #[tokio::test]
    async fn a_fifo_roundtrips_with_mode_and_mtime() {
        let source_root = tempdir("fifo-source");
        let destination_root = tempdir("fifo-destination");
        let pipe = source_root.join("pipe");
        nix::unistd::mkfifo(&pipe, nix::sys::stat::Mode::from_bits_truncate(0o640)).unwrap();
        // mkfifo(2) applies the umask, so state the mode instead of assuming it.
        fs::set_permissions(&pipe, fs::Permissions::from_mode(0o640)).unwrap();
        let source_meta = fs::symlink_metadata(&pipe).unwrap();

        let source = FilesystemSource {
            params: params(&source_root),
        };
        let plan = source.analyze().await.unwrap();
        let payload = crate::source::payload(&plan, Phase::Analyze).unwrap();
        let entry = payload
            .entries
            .iter()
            .find(|entry| entry.path == "pipe")
            .expect("the FIFO must be in the plan");
        assert_eq!(entry.kind, "fifo");
        assert_eq!(entry.size, 0);
        assert!(entry.rdev.is_none(), "a FIFO has no device number");
        assert!(plan.items.is_empty(), "a FIFO carries no data item");

        let mut sink = RecordingSink::default();
        source.stream_out(&plan, &mut sink).await.unwrap();
        assert!(sink.chunks.is_empty());
        let evidence = sink.evidence();

        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        destination
            .stream_in(&plan, &mut EventSource(VecDeque::from([ChunkEvent::End])))
            .await
            .unwrap();
        destination.verify(&plan, &evidence).await.unwrap();

        let restored = fs::symlink_metadata(destination_root.join("pipe")).unwrap();
        assert!(
            restored.file_type().is_fifo(),
            "the node must still be a FIFO"
        );
        assert_eq!(restored.mode() & 0o7777, 0o640);
        assert_eq!(restored.mtime(), source_meta.mtime());
        assert_eq!(restored.mtime_nsec(), source_meta.mtime_nsec());
        fs::remove_dir_all(&source_root).ok();
        fs::remove_dir_all(&destination_root).ok();
    }

    /// A unix socket only exists while a process holds it bound. Recreating the
    /// inode would certify something the source does not have, so the run is
    /// refused while analyzing rather than restored as a dead node.
    #[tokio::test]
    async fn a_unix_socket_is_refused_at_analyze() {
        let root = tempdir("socket-source");
        let listener = std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();
        let source = FilesystemSource {
            params: params(&root),
        };
        let error = source
            .analyze()
            .await
            .expect_err("a unix socket must not be planned");
        assert!(
            matches!(
                &error,
                rb_core::error::BackupError::Phase {
                    phase: Phase::Analyze,
                    ..
                }
            ),
            "the refusal must be Analyze-phase: {error:?}"
        );
        assert!(
            error.to_string().contains("unix socket"),
            "the message must say what was refused: {error}"
        );
        drop(listener);
        fs::remove_dir_all(&root).ok();
    }

    /// Device nodes are the one kind whose creation needs a capability, and a
    /// destination without it has to say so at preflight.
    #[tokio::test]
    async fn device_entries_need_cap_mknod_at_validate() {
        let destination_root = tempdir("mknod-destination");
        let mut device = entry("null", "chardev", 0o020_666);
        device.rdev = Some(259);
        let plan = hostile_plan(&destination_root, vec![device]);
        let without_mknod = crate::dest::Capabilities {
            chown: true,
            mknod: false,
        };
        let report =
            crate::dest::validate_with(&params(&destination_root), &plan, without_mknod).unwrap();
        assert!(!report.ok, "a device node without CAP_MKNOD must not pass");
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "special_files")
            .expect("the special_files check must be reported");
        assert!(!check.passed);
        assert!(
            check.detail.contains("CAP_MKNOD"),
            "the operator has to learn what is missing: {}",
            check.detail
        );

        let with_mknod = crate::dest::Capabilities {
            chown: true,
            mknod: true,
        };
        let report =
            crate::dest::validate_with(&params(&destination_root), &plan, with_mknod).unwrap();
        assert!(report.ok, "with the capability the same plan is restorable");
        fs::remove_dir_all(&destination_root).ok();
    }

    /// Device number and kind belong together: a device entry without one, or a
    /// non-device entry carrying one, describes something this build cannot
    /// reproduce.
    #[tokio::test]
    async fn device_entry_without_rdev_is_rejected() {
        let destination_root = tempdir("rdev-destination");
        let plan = hostile_plan(
            &destination_root,
            vec![entry("null", "blockdev", 0o060_600)],
        );
        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        let error = destination
            .validate(&plan)
            .await
            .expect_err("a device entry without a device number must be refused");
        assert!(
            error.to_string().contains("device number"),
            "unexpected error: {error}"
        );

        let mut file = entry("f", "file", 0o100_644);
        file.rdev = Some(259);
        let plan = hostile_plan(&destination_root, vec![file]);
        let error = destination
            .validate(&plan)
            .await
            .expect_err("a plain file must not carry a device number");
        assert!(
            error.to_string().contains("must not carry a device number"),
            "unexpected error: {error}"
        );
        fs::remove_dir_all(&destination_root).ok();
    }

    /// The new kinds widen the allowlist; they do not remove it.
    #[tokio::test]
    async fn unknown_entry_kind_is_still_rejected() {
        let destination_root = tempdir("kind-destination");
        let plan = hostile_plan(&destination_root, vec![entry("w", "whiteout", 0o100_644)]);
        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        let error = destination
            .validate(&plan)
            .await
            .expect_err("an unknown kind must be refused");
        assert!(
            error.to_string().contains("unknown filesystem entry kind"),
            "unexpected error: {error}"
        );
        fs::remove_dir_all(&destination_root).ok();
    }

    /// A plan written before device nodes existed has no `rdev` field at all,
    /// and must still deserialize — the destination is allowed to be newer than
    /// the source.
    #[test]
    fn older_plan_without_rdev_deserializes() {
        let older = r#"{"path":"f","kind":"file","size":3,"mode":33188,"uid":0,"gid":0,
                        "mtime":1,"mtime_nsec":0,"target":null,"hardlink_to":null,"xattrs":{}}"#;
        let entry: FilesystemEntry = serde_json::from_str(older).unwrap();
        assert_eq!(entry.path, "f");
        assert!(entry.rdev.is_none());
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
            rdev: None,
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

    /// A plan whose non-directory entry is an ancestor of another entry must be
    /// refused up front: the restore materialises ancestors as real directories
    /// before it ever looks at the entry that claims the same path, so the run
    /// used to die mid-apply on an unlink of a non-empty directory and leave
    /// stray directories behind instead of rejecting the plan.
    #[tokio::test]
    async fn a_non_directory_entry_may_not_be_an_ancestor_of_another() {
        let destination_root = tempdir("ancestor-kind-destination");
        let mut link = entry("x", "symlink", 0o120_777);
        link.target = Some("/etc/shadow".to_string());
        let plan = hostile_plan(
            &destination_root,
            vec![link, entry("x/sub", "dir", 0o040_755)],
        );

        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        let mut source = EventSource(VecDeque::from([ChunkEvent::End]));
        let error = destination
            .stream_in(&plan, &mut source)
            .await
            .expect_err("an entry under a symlink must be refused");
        assert!(
            error.to_string().contains("is an ancestor of"),
            "unexpected error: {error}"
        );
        assert!(
            !destination_root.join("x").exists(),
            "the refused plan must not have created anything"
        );
        let _ = fs::remove_dir_all(&destination_root);
    }

    /// A hardlink must name a regular file the same plan restores. `link(2)` not
    /// dereferencing a symlink source is Linux behaviour, not a guarantee, and
    /// nothing later in the restore re-checks what the target actually is.
    #[tokio::test]
    async fn a_hardlink_must_name_a_file_in_the_plan() {
        let destination_root = tempdir("hardlink-kind-destination");
        let mut link = entry("victim-link", "symlink", 0o120_777);
        link.target = Some("/etc/shadow".to_string());
        let mut hard = entry("copy", "hardlink", 0o100_644);
        hard.hardlink_to = Some("victim-link".to_string());
        let plan = hostile_plan(&destination_root, vec![link, hard]);

        let destination = FilesystemDestination {
            params: params(&destination_root),
        };
        let mut source = EventSource(VecDeque::from([ChunkEvent::End]));
        let error = destination
            .stream_in(&plan, &mut source)
            .await
            .expect_err("a hardlink onto a symlink entry must be refused");
        assert!(
            error.to_string().contains("not a file"),
            "unexpected error: {error}"
        );

        let mut dangling = entry("orphan", "hardlink", 0o100_644);
        dangling.hardlink_to = Some("absent".to_string());
        let plan = hostile_plan(&destination_root, vec![dangling]);
        let mut source = EventSource(VecDeque::from([ChunkEvent::End]));
        let error = destination
            .stream_in(&plan, &mut source)
            .await
            .expect_err("a hardlink to an absent entry must be refused");
        assert!(
            error.to_string().contains("does not contain"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_dir_all(&destination_root);
    }

    /// The source's real mode is applied in a single pass after every item has
    /// streamed, so whatever a restored file is created with is its mode for the
    /// rest of the run. Creating at the OS default published a file destined to
    /// be 0o600 as 0o644 for that whole window; it must start owner-only and be
    /// widened later, never the other way round. The same holds for the
    /// directories the restore creates on the way.
    #[test]
    fn a_restored_file_and_its_parents_start_owner_only() {
        let root = tempdir("private-until-applied");
        let active = crate::dest::ActiveFile::open(&root, 1, "nested/deep/secret.key")
            .expect("open the restored file");

        let file_mode = fs::symlink_metadata(root.join("nested/deep/secret.key"))
            .expect("the restored file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode,
            crate::dest::OWNER_ONLY_FILE,
            "a restored file must not be readable by anyone else before its mode is applied"
        );

        for parent in ["nested", "nested/deep"] {
            let mode = fs::symlink_metadata(root.join(parent))
                .expect("the parent directory exists")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode,
                crate::dest::OWNER_ONLY_DIR,
                "parent {parent} must not be traversable by anyone else yet"
            );
        }

        drop(active);
        let _ = fs::remove_dir_all(&root);
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

    // A `DirEntry` keeps an `Arc` on the directory stream it came from, so
    // holding the children of one level while the walk recurses into them
    // pinned one descriptor per level: a tree deeper than `RLIMIT_NOFILE` died
    // with "Too many open files" and `MAX_WALK_DEPTH` could never report its
    // own refusal. Walk a tree deeper than a deliberately small descriptor
    // budget; the limit is restored before the test returns, and 512 leaves
    // ample room for whatever else the test binary has open.
    #[test]
    fn walk_holds_no_descriptor_per_directory_level() {
        use nix::sys::resource::{getrlimit, setrlimit, Resource};

        const DEPTH: usize = 600;
        const BUDGET: u64 = 512;

        let root = tempdir("walk-fd-budget");
        let mut deep = root.clone();
        for _ in 0..DEPTH {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("bottom.txt"), b"m29").unwrap();

        let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        if soft <= BUDGET {
            // The ambient budget is already at or under what the tree needs to
            // prove anything; lowering it further would test nothing.
            let _ = fs::remove_dir_all(&root);
            return;
        }
        setrlimit(Resource::RLIMIT_NOFILE, BUDGET, hard).unwrap();
        // `visit` recurses, and a test thread's default stack is far smaller
        // than the main thread's: run the walk with room to reach the bottom,
        // so the test measures descriptors rather than stack.
        let walk_root = root.clone();
        let walked = std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(move || crate::walk::collect(&walk_root))
            .unwrap()
            .join()
            .unwrap();
        setrlimit(Resource::RLIMIT_NOFILE, soft, hard).unwrap();

        let plan = walked.expect("a tree deeper than the descriptor budget must still walk");
        assert!(
            plan.entries.iter().any(|entry| entry.kind == "file"),
            "the walk reached the bottom of the tree"
        );
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
