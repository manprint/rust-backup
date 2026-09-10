use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nix::fcntl::AtFlags;
use nix::sys::stat::{utimensat, UtimensatFlags};
use nix::sys::time::TimeSpec;
use nix::unistd::{fchownat, Gid, Uid};
use rb_core::channel::{ChunkEvent, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{BackupPlan, Preflight};

use crate::source::payload;
use crate::walk::{at_root, io_error};
use crate::{FilesystemEntry, FilesystemParams, FilesystemPlan};

pub(crate) fn validate_destination_params(params: &FilesystemParams) -> Result<()> {
    if params.follow_symlinks || params.preserve_xattr {
        return Err(BackupError::phase(
            Phase::Connect,
            "filesystem follow_symlinks and preserve_xattr are not supported by this safe backend",
        ));
    }
    Ok(())
}

pub(crate) async fn validate(params: &FilesystemParams, plan: &BackupPlan) -> Result<Preflight> {
    let payload = payload(plan, Phase::Validate)?;
    validate_entries(&payload)?;
    let root = Path::new(&params.root);
    let parent = root.parent().unwrap_or_else(|| Path::new("."));
    let parent_ok = parent.is_dir();
    let target_empty = destination_is_empty(root)?;
    let ownership_possible = has_cap_chown();
    let ownership_needed = params.preserve_ownership
        && payload
            .entries
            .iter()
            .any(|entry| entry.uid != current_uid() || entry.gid != current_gid());
    Ok(Preflight::pass()
        .check(
            "destination-parent",
            parent_ok,
            format!("{}", parent.display()),
        )
        .check(
            "destination-empty",
            target_empty,
            "destination root must be absent or empty; existing entries are never deleted",
        )
        .check(
            "ownership",
            !ownership_needed || ownership_possible,
            if ownership_needed && !ownership_possible {
                String::from(
                    "exact uid/gid restore requires root or CAP_CHOWN; use --no-preserve-ownership to explicitly exclude ownership from the restore contract",
                )
            } else if params.preserve_ownership {
                String::from("uid/gid restoration is available")
            } else {
                String::from("uid/gid restoration intentionally disabled")
            },
        )
        .check(
            "estimated-bytes",
            true,
            format!("{} bytes to restore", payload.total_bytes),
        ))
}

pub(crate) async fn stream_in(
    params: &FilesystemParams,
    plan: &BackupPlan,
    src: &mut dyn ChunkSource,
) -> Result<()> {
    let payload = payload(plan, Phase::Apply)?;
    validate_entries(&payload)?;
    let root = Path::new(&params.root);
    if !destination_is_empty(root)? {
        return Err(BackupError::phase(
            Phase::Apply,
            format!(
                "filesystem destination must be empty before restore: {}",
                root.display()
            ),
        ));
    }
    fs::create_dir_all(root).map_err(|e| io_error(Phase::Apply, root, e))?;
    create_directories(root, &payload)?;

    let by_id: HashMap<_, _> = plan.items.iter().map(|item| (item.id, item)).collect();
    let mut active: Option<ActiveFile> = None;
    loop {
        match src.next().await? {
            ChunkEvent::Chunk {
                item_id,
                offset,
                data,
            } => {
                let item = by_id.get(&item_id).ok_or_else(|| {
                    BackupError::phase(Phase::Apply, format!("unknown filesystem item {item_id}"))
                })?;
                if active.as_ref().map(|file| file.item_id) != Some(item_id) {
                    if active.is_some() {
                        return Err(BackupError::phase(
                            Phase::Apply,
                            "filesystem items arrived out of order",
                        ));
                    }
                    active = Some(ActiveFile::open(root, item_id, &item.name)?);
                }
                let file = active.as_mut().ok_or_else(|| {
                    BackupError::phase(Phase::Apply, "missing active filesystem item")
                })?;
                if offset != file.offset {
                    return Err(BackupError::phase(
                        Phase::Apply,
                        format!("non-contiguous file chunk for {}", item.name),
                    ));
                }
                file.file
                    .write_all(&data)
                    .map_err(|e| io_error(Phase::Apply, &file.path, e))?;
                file.hasher.update(&data);
                file.offset += data.len() as u64;
            }
            ChunkEvent::ItemEnd {
                item_id,
                total,
                blake3,
            } => {
                let item = by_id.get(&item_id).ok_or_else(|| {
                    BackupError::phase(Phase::Apply, format!("unknown filesystem item {item_id}"))
                })?;
                if active.is_none() {
                    active = Some(ActiveFile::open(root, item_id, &item.name)?);
                }
                let complete = active.take().ok_or_else(|| {
                    BackupError::phase(Phase::Apply, "missing active filesystem item")
                })?;
                if complete.item_id != item_id
                    || complete.offset != total
                    || complete.digest() != blake3
                {
                    return Err(BackupError::Integrity(format!(
                        "filesystem item digest/length mismatch: {}",
                        item.name
                    )));
                }
                complete.commit();
            }
            ChunkEvent::End => break,
        }
    }
    if active.is_some() {
        return Err(BackupError::phase(
            Phase::Apply,
            "filesystem stream ended before item completion",
        ));
    }
    create_links(root, &payload)?;
    apply_metadata(root, &payload, params.preserve_ownership && has_cap_chown())?;
    Ok(())
}

/// Re-scan the destination after apply and compare every selected path and
/// restorable metadata field with the source plan. File bytes are verified by
/// the shared read-back sink in `lib.rs`.
pub(crate) fn verify_metadata(params: &FilesystemParams, plan: &BackupPlan) -> Result<()> {
    let expected = payload(plan, Phase::Verify)?;
    let root = crate::walk::checked_root(params, Phase::Verify)?;
    let actual = crate::walk::collect(&root).map_err(|error| {
        BackupError::phase_src(Phase::Verify, "scan restored filesystem", error)
    })?;
    if actual.entries.len() != expected.entries.len() {
        return Err(BackupError::phase(
            Phase::Verify,
            format!(
                "filesystem entry count mismatch: source={} destination={}",
                expected.entries.len(),
                actual.entries.len()
            ),
        ));
    }
    // `actual` is sorted by the walk; `expected` arrives in whatever order the
    // peer chose. Compare like against like.
    let mut expected_entries: Vec<_> = expected.entries.iter().collect();
    expected_entries.sort_by(|a, b| a.path.cmp(&b.path));
    for (source, destination) in expected_entries.into_iter().zip(&actual.entries) {
        let ownership_matches = !params.preserve_ownership
            || (source.uid == destination.uid && source.gid == destination.gid);
        if source.path != destination.path
            || source.kind != destination.kind
            || source.size != destination.size
            || source.mode != destination.mode
            || source.mtime != destination.mtime
            || source.mtime_nsec != destination.mtime_nsec
            || source.target != destination.target
            || source.hardlink_to != destination.hardlink_to
            || source.xattrs != destination.xattrs
            || !ownership_matches
        {
            return Err(BackupError::phase(
                Phase::Verify,
                format!(
                    "filesystem metadata mismatch at {:?}: source={source:?} destination={destination:?}",
                    source.path
                ),
            ));
        }
    }
    if actual.total_bytes != expected.total_bytes {
        return Err(BackupError::phase(
            Phase::Verify,
            format!(
                "filesystem byte total mismatch: source={} destination={}",
                expected.total_bytes, actual.total_bytes
            ),
        ));
    }
    Ok(())
}

pub(crate) struct ActiveFile {
    item_id: u32,
    path: std::path::PathBuf,
    file: File,
    offset: u64,
    hasher: blake3::Hasher,
    committed: bool,
}

impl ActiveFile {
    pub(crate) fn open(root: &Path, item_id: u32, name: &str) -> Result<Self> {
        let path = at_root(root, name, Phase::Apply)?;
        if let Some(parent) = path.parent() {
            create_private_dir_all(parent)?;
        }
        // O_NOFOLLOW: a plan that planted a symlink at this path must not have
        // the restore write the item's bytes into the link's target.
        //
        // `.mode(OWNER_ONLY_FILE)`: the source's real mode is applied later, in
        // one pass after every item has streamed. Creating at the OS default
        // (0o666 minus the destination's umask, so typically 0o644) published a
        // file that is meant to be 0o600 — a private key, a credentials file —
        // world-readable for the whole rest of the restore. Start closed and let
        // `apply_metadata` open it up; a restore that fails in between leaves
        // the restrictive mode, never the permissive one.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(OWNER_ONLY_FILE)
            .custom_flags(libc_o_nofollow())
            .open(&path)
            .map_err(|e| io_error(Phase::Apply, &path, e))?;
        Ok(Self {
            item_id,
            path,
            file,
            offset: 0,
            hasher: blake3::Hasher::new(),
            committed: false,
        })
    }
    fn digest(&self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ActiveFile {
    fn drop(&mut self) {
        if !self.committed {
            // The destination starts empty, so this path was created by this
            // restore.  Never leave a truncated file looking like committed
            // state after EOF, Abort, an integrity error, or ENOSPC.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A file is created owner-only and widened later; the same rule applies to the
/// directories that hold it.
pub(crate) const OWNER_ONLY_FILE: u32 = 0o600;
pub(crate) const OWNER_ONLY_DIR: u32 = 0o700;

/// `fs::create_dir_all` with an owner-only mode on every component it creates.
/// The plan's own mode is applied by `apply_metadata` once the tree is complete,
/// so a directory that is meant to be 0o700 must not be readable in the
/// meantime — and an intermediate component the plan never names keeps the
/// restrictive mode rather than inheriting 0o777 minus the umask.
fn create_private_dir_all(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(OWNER_ONLY_DIR)
        .create(path)
        .map_err(|e| io_error(Phase::Apply, path, e))
}

fn create_directories(root: &Path, plan: &FilesystemPlan) -> Result<()> {
    for entry in &plan.entries {
        if entry.kind == "dir" {
            let path = at_root(root, &entry.path, Phase::Apply)?;
            create_private_dir_all(&path)?;
        }
    }
    Ok(())
}

fn create_links(root: &Path, plan: &FilesystemPlan) -> Result<()> {
    for entry in &plan.entries {
        let path = at_root(root, &entry.path, Phase::Apply)?;
        match entry.kind.as_str() {
            "symlink" => {
                let target = entry.target.as_deref().ok_or_else(|| {
                    BackupError::phase(
                        Phase::Apply,
                        format!("missing symlink target: {}", entry.path),
                    )
                })?;
                if fs::symlink_metadata(&path).is_ok() {
                    fs::remove_file(&path).map_err(|e| io_error(Phase::Apply, &path, e))?;
                }
                std::os::unix::fs::symlink(target, &path)
                    .map_err(|e| io_error(Phase::Apply, &path, e))?;
            }
            "hardlink" => {
                let first = entry.hardlink_to.as_deref().ok_or_else(|| {
                    BackupError::phase(
                        Phase::Apply,
                        format!("missing hardlink target: {}", entry.path),
                    )
                })?;
                let target = at_root(root, first, Phase::Apply)?;
                if fs::symlink_metadata(&path).is_ok() {
                    fs::remove_file(&path).map_err(|e| io_error(Phase::Apply, &path, e))?;
                }
                fs::hard_link(&target, &path).map_err(|e| io_error(Phase::Apply, &path, e))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn apply_metadata(root: &Path, plan: &FilesystemPlan, chown: bool) -> Result<()> {
    // Apply directories last so child creation cannot change their final mtime.
    let mut entries: Vec<_> = plan.entries.iter().collect();
    entries.sort_by_key(|entry| (entry.kind == "dir", std::cmp::Reverse(entry.path.len())));
    for entry in entries {
        if entry.kind == "hardlink" {
            continue;
        }
        let path = at_root(root, &entry.path, Phase::Apply)?;
        if chown {
            fchownat(
                None,
                &path,
                Some(Uid::from_raw(entry.uid)),
                Some(Gid::from_raw(entry.gid)),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            )
            .map_err(|e| BackupError::phase(Phase::Apply, format!("{}: {e}", path.display())))?;
        }
        if entry.kind != "symlink" {
            // chown(2) clears setuid/setgid on regular files. Apply ownership
            // first and the exact permission bits last, otherwise a successful
            // privileged restore silently turns 04755 into 0755.
            //
            // NEVER `fs::set_permissions` here: that is chmod(2), which FOLLOWS
            // symlinks. A plan could then plant a symlink at a path and have the
            // following entry chmod the link's target — arbitrary mode change,
            // setuid included, anywhere on a host whose restore runs as root
            // (which is exactly how ownership-preserving restores run). Open the
            // path with O_NOFOLLOW and fchmod the descriptor, so the kernel, not
            // a preceding stat, enforces it.
            chmod_no_follow(&path, entry)?;
        }
        let mtime = TimeSpec::new(entry.mtime, entry.mtime_nsec as i64);
        utimensat(
            None,
            &path,
            &TimeSpec::UTIME_OMIT,
            &mtime,
            UtimensatFlags::NoFollowSymlink,
        )
        .map_err(|e| BackupError::phase(Phase::Apply, format!("{}: {e}", path.display())))?;
    }
    Ok(())
}

fn validate_entries(plan: &FilesystemPlan) -> Result<()> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in &plan.entries {
        let _ = crate::walk::relative_path(&entry.path, Phase::Validate)?;
        if !matches!(entry.kind.as_str(), "file" | "dir" | "symlink" | "hardlink") {
            return Err(BackupError::phase(
                Phase::Validate,
                format!("unknown filesystem entry kind: {}", entry.kind),
            ));
        }
        // Two entries for one path let a later entry act on whatever an earlier
        // entry created there — the symlink-then-chmod pattern above.
        if !seen.insert(entry.path.as_str()) {
            return Err(BackupError::phase(
                Phase::Validate,
                format!("duplicate filesystem plan entry: {:?}", entry.path),
            ));
        }
        if (entry.kind == "symlink") != entry.target.is_some() {
            return Err(BackupError::phase(
                Phase::Validate,
                format!(
                    "filesystem entry {:?} of kind {} must {}carry a symlink target",
                    entry.path,
                    entry.kind,
                    if entry.kind == "symlink" { "" } else { "not " }
                ),
            ));
        }
        if (entry.kind == "hardlink") != entry.hardlink_to.is_some() {
            return Err(BackupError::phase(
                Phase::Validate,
                format!(
                    "filesystem entry {:?} of kind {} must {}carry a hardlink target",
                    entry.path,
                    entry.kind,
                    if entry.kind == "hardlink" { "" } else { "not " }
                ),
            ));
        }
        if let Some(target) = &entry.hardlink_to {
            let _ = crate::walk::relative_path(target, Phase::Validate)?;
        }
        // Xattrs are never collected by this build, so a plan carrying them
        // would only fail much later, in verification.
        if !entry.xattrs.is_empty() {
            return Err(BackupError::phase(
                Phase::Validate,
                format!(
                    "filesystem entry {:?} carries extended attributes, which this build does not restore",
                    entry.path
                ),
            ));
        }
    }

    // Two cross-entry rules. Both are about what one entry can make another
    // entry's path mean, which the per-entry checks above cannot see.
    let kinds: HashMap<&str, &str> = plan
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.kind.as_str()))
        .collect();
    for entry in &plan.entries {
        // 1. A non-directory entry must not be an ancestor of another entry.
        //    `create_directories` and the per-item `create_dir_all` materialise
        //    ancestors as real directories, so a plan holding both `x` (symlink)
        //    and `x/sub` made `x` a directory before `create_links` ever saw it,
        //    and the run then died on an unlink of a non-empty directory instead
        //    of refusing the plan.
        let mut ancestor = entry.path.as_str();
        while let Some(cut) = ancestor.rfind('/') {
            ancestor = &ancestor[..cut];
            if let Some(kind) = kinds.get(ancestor) {
                if *kind != "dir" {
                    return Err(BackupError::phase(
                        Phase::Validate,
                        format!(
                            "filesystem plan entry {:?} of kind {kind} is an ancestor of {:?}",
                            ancestor, entry.path
                        ),
                    ));
                }
            }
        }
        // 2. A hardlink must name a regular file in the same plan. `link(2)` on
        //    Linux does not dereference a symlink source, so pointing one at a
        //    symlink entry is not an escape today — but that is POSIX-
        //    unspecified, and nothing else in the restore re-checks it.
        if let Some(target) = &entry.hardlink_to {
            match kinds.get(target.as_str()) {
                Some(&"file") => {}
                Some(kind) => {
                    return Err(BackupError::phase(
                        Phase::Validate,
                        format!(
                            "filesystem entry {:?} hardlinks to {:?}, which the plan declares as {kind}, not a file",
                            entry.path, target
                        ),
                    ));
                }
                None => {
                    return Err(BackupError::phase(
                        Phase::Validate,
                        format!(
                            "filesystem entry {:?} hardlinks to {:?}, which the plan does not contain",
                            entry.path, target
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `chmod` a restored path without ever following a symlink.
///
/// Linux's `fchmodat` rejects `AT_SYMLINK_NOFOLLOW`, so the no-follow guarantee
/// comes from opening the path with `O_NOFOLLOW` and calling `fchmod` on the
/// resulting descriptor: if the path is a symlink the open itself fails.
pub(crate) fn chmod_no_follow(path: &Path, entry: &FilesystemEntry) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    let flags = if entry.kind == "dir" {
        libc_o_directory() | libc_o_nofollow()
    } else {
        libc_o_nofollow()
    };
    options.custom_flags(flags);
    let file = options
        .open(path)
        .map_err(|e| io_error(Phase::Apply, path, e))?;
    // `File::set_permissions` is `fchmod(2)` on the already-open descriptor, so
    // it cannot be redirected by anything at `path` after the open.
    file.set_permissions(fs::Permissions::from_mode(entry.mode & 0o7777))
        .map_err(|e| io_error(Phase::Apply, path, e))
}

fn libc_o_nofollow() -> i32 {
    0o400_000 // O_NOFOLLOW on Linux
}

fn libc_o_directory() -> i32 {
    0o200_000 // O_DIRECTORY on Linux
}

fn has_cap_chown() -> bool {
    if Uid::effective().is_root() {
        return true;
    }
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("CapEff:\t").map(str::to_owned))
        })
        .and_then(|raw| u64::from_str_radix(raw.trim(), 16).ok())
        .is_some_and(|bits| bits & 1 != 0)
}

fn current_uid() -> u32 {
    Uid::current().as_raw()
}
fn current_gid() -> u32 {
    Gid::current().as_raw()
}

fn destination_is_empty(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    if !path.is_dir() {
        return Ok(false);
    }
    fs::read_dir(path)
        .map_err(|e| io_error(Phase::Validate, path, e))?
        .next()
        .transpose()
        .map_err(|e| io_error(Phase::Validate, path, e))
        .map(|entry| entry.is_none())
}
