use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{fchmodat, mknod, utimensat, FchmodatFlags, Mode, SFlag, UtimensatFlags};
use nix::sys::time::TimeSpec;
use nix::unistd::{fchownat, mkfifo, Gid, Uid};
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

/// What this process is allowed to do, resolved once so preflight and the
/// tests can answer the same question the same way — the tests build the
/// capability set instead of the kernel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Capabilities {
    pub(crate) chown: bool,
    pub(crate) mknod: bool,
}

impl Capabilities {
    fn current() -> Self {
        Self {
            chown: has_cap_chown(),
            mknod: has_cap_mknod(),
        }
    }
}

pub(crate) async fn validate(params: &FilesystemParams, plan: &BackupPlan) -> Result<Preflight> {
    validate_with(params, plan, Capabilities::current())
}

pub(crate) fn validate_with(
    params: &FilesystemParams,
    plan: &BackupPlan,
    caps: Capabilities,
) -> Result<Preflight> {
    let payload = payload(plan, Phase::Validate)?;
    validate_entries(&payload)?;
    let root = Path::new(&params.root);
    let parent = root.parent().unwrap_or_else(|| Path::new("."));
    let parent_ok = parent.is_dir();
    let target_empty = destination_is_empty(root)?;
    let (empty_ok, empty_detail) = if target_empty {
        (true, String::from("destination root is absent or empty"))
    } else if !params.overwrite {
        (
            false,
            String::from(
                "destination root must be absent or empty; pass --overwrite to delete its \
                 existing entries first",
            ),
        )
    } else {
        match overwrite_refusal(root) {
            Some(reason) => (false, reason),
            None => (
                true,
                format!(
                    "--overwrite: the {} existing entries in {} will be deleted before the \
                     first payload byte",
                    top_level_entries(root)?,
                    root.display()
                ),
            ),
        }
    };
    let ownership_possible = caps.chown;
    // Device nodes are the one kind whose creation needs a capability of its
    // own. Saying so at preflight is the difference between "this restore
    // cannot work here" and a run that fails halfway through apply.
    let device_entries = payload
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "chardev" | "blockdev"))
        .count();
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
        .check("destination-empty", empty_ok, empty_detail)
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
            "special_files",
            device_entries == 0 || caps.mknod,
            if device_entries > 0 && !caps.mknod {
                format!(
                    "restoring device nodes needs root or CAP_MKNOD ({device_entries} device entries in the plan)"
                )
            } else if device_entries > 0 {
                format!("{device_entries} device nodes can be recreated")
            } else {
                String::from("no device nodes in the plan")
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
        if !params.overwrite {
            return Err(BackupError::phase(
                Phase::Apply,
                format!(
                    "filesystem destination must be empty before restore: {}",
                    root.display()
                ),
            ));
        }
        clear_destination_root(root)?;
    }
    create_destination_root(root)?;
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
    create_nodes(root, &payload)?;
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
            || source.rdev != destination.rdev
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
            create_private_dir_all(root, parent)?;
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

/// Create one directory, owner-only, and refuse anything else already sitting
/// at that path.
///
/// `create_dir_all` and `DirBuilder::recursive(true)` *follow* a symlink that
/// points at a directory and call it an existing directory. Between
/// `destination_is_empty` and this call, anything that can write the
/// destination's parent can therefore plant `root/a -> /elsewhere` and have the
/// restore write the tree outside the declared root. `create` (non-recursive)
/// fails with `AlreadyExists` instead, and what is actually there is then
/// checked with `symlink_metadata`, which does not follow.
///
/// The plan's own mode is applied by `apply_metadata` once the tree is complete,
/// so a directory that is meant to be 0o700 must not be readable in the
/// meantime — and an intermediate component the plan never names keeps the
/// restrictive mode rather than inheriting 0o777 minus the umask.
fn create_private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(OWNER_ONLY_DIR).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing =
                fs::symlink_metadata(path).map_err(|e| io_error(Phase::Apply, path, e))?;
            if existing.is_dir() {
                Ok(())
            } else {
                Err(BackupError::phase(
                    Phase::Apply,
                    format!(
                        "destination path was replaced by a symlink during restore: {}",
                        path.display()
                    ),
                ))
            }
        }
        Err(error) => Err(io_error(Phase::Apply, path, error)),
    }
}

/// `create_private_dir` for a path and every component of it below `root`,
/// parent first. `root` itself is created by `create_destination_root`.
fn create_private_dir_all(root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        create_private_dir(&current)?;
    }
    Ok(())
}

/// The destination root may legitimately not exist yet. Create it — and then
/// insist that what is there is a real directory: a symlink at the root would
/// redirect the whole restore, and `create_dir_all` would have accepted it.
pub(crate) fn create_destination_root(root: &Path) -> Result<()> {
    if let Err(error) = fs::create_dir_all(root) {
        return Err(io_error(Phase::Apply, root, error));
    }
    let meta = fs::symlink_metadata(root).map_err(|e| io_error(Phase::Apply, root, e))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(BackupError::phase(
            Phase::Validate,
            format!("destination root is a symlink: {}", root.display()),
        ));
    }
    Ok(())
}

/// Planned directories are created parent first: the plan's paths are sorted and
/// every one of them is a canonical relative path, so a parent always precedes
/// its children.
pub(crate) fn create_directories(root: &Path, plan: &FilesystemPlan) -> Result<()> {
    for entry in &plan.entries {
        if entry.kind == "dir" {
            let path = at_root(root, &entry.path, Phase::Apply)?;
            create_private_dir_all(root, &path)?;
        }
    }
    Ok(())
}

/// Create everything that is not a directory and carries no data: symlinks,
/// hardlinks, FIFOs and device nodes. Runs after the payload has landed, so a
/// hardlink always finds the file it names.
fn create_nodes(root: &Path, plan: &FilesystemPlan) -> Result<()> {
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
            // Created owner-only and widened by `apply_metadata`, exactly like a
            // regular file: a FIFO or a device must not be readable by anyone
            // else for the window between creation and the final chmod.
            "fifo" => {
                mkfifo(&path, Mode::from_bits_truncate(OWNER_ONLY_FILE)).map_err(|e| {
                    BackupError::phase(Phase::Apply, format!("{}: {e}", path.display()))
                })?;
            }
            "chardev" | "blockdev" => {
                let rdev = entry.rdev.ok_or_else(|| {
                    BackupError::phase(
                        Phase::Apply,
                        format!("missing device number: {}", entry.path),
                    )
                })?;
                let kind = if entry.kind == "chardev" {
                    SFlag::S_IFCHR
                } else {
                    SFlag::S_IFBLK
                };
                mknod(&path, kind, Mode::from_bits_truncate(OWNER_ONLY_FILE), rdev).map_err(
                    |e| BackupError::phase(Phase::Apply, format!("{}: {e}", path.display())),
                )?;
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
            // Files, directories, FIFOs and device nodes all get their exact
            // mode here; a symlink has none of its own.
            //
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
        if !matches!(
            entry.kind.as_str(),
            "file" | "dir" | "symlink" | "hardlink" | "fifo" | "chardev" | "blockdev"
        ) {
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
        // A device node is its device number: without one there is nothing to
        // create, and carrying one on any other kind would mean the plan
        // describes something this build does not understand.
        let is_device = matches!(entry.kind.as_str(), "chardev" | "blockdev");
        if is_device != entry.rdev.is_some() {
            return Err(BackupError::phase(
                Phase::Validate,
                format!(
                    "filesystem entry {:?} of kind {} must {}carry a device number",
                    entry.path,
                    entry.kind,
                    if is_device { "" } else { "not " }
                ),
            ));
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
    if matches!(entry.kind.as_str(), "fifo" | "chardev" | "blockdev") {
        return chmod_node_no_follow(path, entry.mode);
    }
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

/// `chmod` a node that must not be opened.
///
/// Opening a FIFO blocks until a peer appears at the other end, and opening a
/// device node talks to the device — neither is acceptable during a restore. An
/// `O_PATH` descriptor names the inode without opening it, and `/proc/self/fd/N`
/// resolves back to exactly that inode, which is how a no-follow `chmod` is done
/// on Linux (`fchmodat` rejects `AT_SYMLINK_NOFOLLOW`). `O_NOFOLLOW` on the
/// `O_PATH` open is what makes it symlink-safe.
fn chmod_node_no_follow(path: &Path, mode: u32) -> Result<()> {
    let fd = nix::fcntl::open(
        path,
        OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| BackupError::phase(Phase::Apply, format!("{}: {e}", path.display())))?;
    let by_fd = format!("/proc/self/fd/{fd}");
    let result = fchmodat(
        None,
        by_fd.as_str(),
        Mode::from_bits_truncate(mode & 0o7777),
        FchmodatFlags::FollowSymlink,
    )
    .map_err(|e| BackupError::phase(Phase::Apply, format!("{}: {e}", path.display())));
    let _ = nix::unistd::close(fd);
    result
}

fn libc_o_nofollow() -> i32 {
    0o400_000 // O_NOFOLLOW on Linux
}

fn libc_o_directory() -> i32 {
    0o200_000 // O_DIRECTORY on Linux
}

/// Is `bit` set in this process's effective capability set?
///
/// Read from `/proc/self/status` for EVERY process, root included. "Effective
/// uid 0, therefore yes" is false on any hardened runtime: `docker run
/// --cap-drop=CHOWN`, a systemd unit with `CapabilityBoundingSet=`, a CI
/// sandbox — all run as root without the capability. Answering yes there made
/// the `ownership` and `special_files` preflight checks approve a restore whose
/// `fchownat`/`mknod` then failed with `EPERM` *after* the whole payload had
/// landed, which is exactly the outcome those checks exist to prevent. The uid
/// survives only as the fallback for a host with no readable `/proc`.
fn has_cap(bit: u32) -> bool {
    match effective_capabilities() {
        Some(bits) => bits & (1u64 << bit) != 0,
        None => Uid::effective().is_root(),
    }
}

/// This process's `CapEff` mask, or `None` when `/proc` cannot answer.
fn effective_capabilities() -> Option<u64> {
    capability_bits(&fs::read_to_string("/proc/self/status").ok()?)
}

/// Pure half of [`effective_capabilities`], so the parse is testable without a
/// `/proc` to point it at.
pub(crate) fn capability_bits(status: &str) -> Option<u64> {
    let raw = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))?;
    u64::from_str_radix(raw.trim(), 16).ok()
}

/// CAP_CHOWN — restoring uid/gid.
fn has_cap_chown() -> bool {
    has_cap(0)
}

/// CAP_MKNOD — creating device nodes. FIFOs need no capability.
fn has_cap_mknod() -> bool {
    has_cap(27)
}

fn current_uid() -> u32 {
    Uid::current().as_raw()
}
fn current_gid() -> u32 {
    Gid::current().as_raw()
}

/// Why `root` cannot be emptied by `--overwrite`, if it cannot: a symlink
/// would redirect the deletion, a non-directory has no entries to delete, and
/// `/` is never a restore target worth a recursive delete.
fn overwrite_refusal(root: &Path) -> Option<String> {
    let meta = match fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(error) => return Some(format!("cannot inspect {}: {error}", root.display())),
    };
    if meta.file_type().is_symlink() {
        return Some(format!(
            "--overwrite refused: destination root {} is a symlink",
            root.display()
        ));
    }
    if !meta.is_dir() {
        return Some(format!(
            "--overwrite refused: destination root {} is not a directory",
            root.display()
        ));
    }
    match fs::canonicalize(root) {
        Ok(real) if real == Path::new("/") => Some(String::from(
            "--overwrite refused: the destination root is /",
        )),
        Ok(_) => None,
        Err(error) => Some(format!("cannot resolve {}: {error}", root.display())),
    }
}

fn top_level_entries(root: &Path) -> Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(root).map_err(|e| io_error(Phase::Validate, root, e))? {
        entry.map_err(|e| io_error(Phase::Validate, root, e))?;
        count += 1;
    }
    Ok(count)
}

/// `--overwrite`: delete every entry under `root`, keeping `root` itself (it
/// may be a mount point). Entries are classified without following links: a
/// symlink is unlinked, never traversed, and `remove_dir_all` does not follow
/// symlinks inside the trees it removes, so nothing outside `root` is touched.
fn clear_destination_root(root: &Path) -> Result<()> {
    if let Some(reason) = overwrite_refusal(root) {
        return Err(BackupError::phase(Phase::Validate, reason));
    }
    let mut deleted = 0usize;
    for entry in fs::read_dir(root).map_err(|e| io_error(Phase::Apply, root, e))? {
        let entry = entry.map_err(|e| io_error(Phase::Apply, root, e))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| io_error(Phase::Apply, &path, e))?;
        let removed = if kind.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(|e| io_error(Phase::Apply, &path, e))?;
        deleted += 1;
    }
    tracing::info!(
        root = %root.display(),
        deleted,
        "--overwrite: deleted the existing entries of the destination root"
    );
    Ok(())
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
