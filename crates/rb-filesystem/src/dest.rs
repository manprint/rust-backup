use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
use crate::{FilesystemParams, FilesystemPlan};

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
            true,
            if ownership_needed && !ownership_possible {
                String::from("warning: uid/gid will remain the destination process owner (not root/CAP_CHOWN)")
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

struct ActiveFile {
    item_id: u32,
    path: std::path::PathBuf,
    file: File,
    offset: u64,
    hasher: blake3::Hasher,
    committed: bool,
}

impl ActiveFile {
    fn open(root: &Path, item_id: u32, name: &str) -> Result<Self> {
        let path = at_root(root, name, Phase::Apply)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(Phase::Apply, parent, e))?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
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

fn create_directories(root: &Path, plan: &FilesystemPlan) -> Result<()> {
    for entry in &plan.entries {
        if entry.kind == "dir" {
            let path = at_root(root, &entry.path, Phase::Apply)?;
            fs::create_dir_all(&path).map_err(|e| io_error(Phase::Apply, &path, e))?;
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
            fs::set_permissions(&path, fs::Permissions::from_mode(entry.mode & 0o7777))
                .map_err(|e| io_error(Phase::Apply, &path, e))?;
        }
        let mtime = TimeSpec::new(entry.mtime as i64, entry.mtime_nsec as i64);
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
    for entry in &plan.entries {
        let _ = crate::walk::relative_path(&entry.path, Phase::Validate)?;
        if !matches!(entry.kind.as_str(), "file" | "dir" | "symlink" | "hardlink") {
            return Err(BackupError::phase(
                Phase::Validate,
                format!("unknown filesystem entry kind: {}", entry.kind),
            ));
        }
    }
    Ok(())
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
