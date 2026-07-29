use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rb_core::error::{BackupError, Phase, Result};

use crate::{FilesystemEntry, FilesystemParams, FilesystemPlan};

pub(crate) fn checked_root(params: &FilesystemParams, phase: Phase) -> Result<PathBuf> {
    if params.follow_symlinks {
        return Err(BackupError::phase(
            phase,
            "filesystem follow_symlinks is unsupported: it can escape the declared root",
        ));
    }
    if params.preserve_xattr {
        return Err(BackupError::phase(
            phase,
            "filesystem preserve_xattr is not implemented by the safe std+nix backend",
        ));
    }
    let root = PathBuf::from(&params.root);
    let meta = fs::metadata(&root).map_err(|e| io_error(phase, &root, e))?;
    if !meta.is_dir() {
        return Err(BackupError::phase(
            phase,
            format!("filesystem root is not a directory: {}", root.display()),
        ));
    }
    Ok(root)
}

pub(crate) fn relative_path(path: &str, phase: Phase) -> Result<PathBuf> {
    let rel = Path::new(path);
    if rel.as_os_str().is_empty()
        || rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(BackupError::phase(
            phase,
            format!("unsafe filesystem plan path: {path:?}"),
        ));
    }
    Ok(rel.to_path_buf())
}

pub(crate) fn at_root(root: &Path, path: &str, phase: Phase) -> Result<PathBuf> {
    Ok(root.join(relative_path(path, phase)?))
}

pub(crate) fn collect(root: &Path) -> Result<FilesystemPlan> {
    let mut entries = Vec::new();
    let mut hardlinks = HashMap::new();
    visit(root, Path::new(""), &mut entries, &mut hardlinks)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let total_bytes = entries
        .iter()
        .filter(|e| e.kind == "file")
        .map(|e| e.size)
        .sum();
    Ok(FilesystemPlan {
        root: root.display().to_string(),
        entries,
        total_bytes,
        ownership_note: "uid/gid are restored only when destination has root or CAP_CHOWN".into(),
    })
}

fn visit(
    root: &Path,
    rel: &Path,
    entries: &mut Vec<FilesystemEntry>,
    hardlinks: &mut HashMap<(u64, u64), String>,
) -> Result<()> {
    let dir = root.join(rel);
    let mut children: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| io_error(Phase::Analyze, &dir, e))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| io_error(Phase::Analyze, &dir, e))?;
    children.sort_by_key(|child| child.file_name());

    for child in children {
        let path = child.path();
        let child_rel = rel.join(child.file_name());
        let meta = fs::symlink_metadata(&path).map_err(|e| io_error(Phase::Analyze, &path, e))?;
        let file_type = meta.file_type();
        let mut entry = entry_from_meta(&child_rel, &meta);
        if file_type.is_dir() {
            entry.kind = "dir".into();
            entries.push(entry);
            visit(root, &child_rel, entries, hardlinks)?;
        } else if file_type.is_symlink() {
            entry.kind = "symlink".into();
            entry.target = Some(
                fs::read_link(&path)
                    .map_err(|e| io_error(Phase::Analyze, &path, e))?
                    .to_string_lossy()
                    .into_owned(),
            );
            entries.push(entry);
        } else if file_type.is_file() {
            let key = (meta.dev(), meta.ino());
            if meta.nlink() > 1 {
                if let Some(first) = hardlinks.get(&key) {
                    entry.kind = "hardlink".into();
                    entry.size = 0;
                    entry.hardlink_to = Some(first.clone());
                } else {
                    hardlinks.insert(key, entry.path.clone());
                }
            }
            entries.push(entry);
        } else {
            return Err(BackupError::phase(
                Phase::Analyze,
                format!("unsupported filesystem entry: {}", path.display()),
            ));
        }
    }
    Ok(())
}

fn entry_from_meta(rel: &Path, meta: &fs::Metadata) -> FilesystemEntry {
    FilesystemEntry {
        path: rel.to_string_lossy().into_owned(),
        kind: "file".into(),
        size: meta.len(),
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        mtime: meta.mtime().max(0) as u64,
        mtime_nsec: meta.mtime_nsec().max(0) as u32,
        target: None,
        hardlink_to: None,
        xattrs: BTreeMap::new(),
    }
}

pub(crate) fn io_error(phase: Phase, path: &Path, error: std::io::Error) -> BackupError {
    BackupError::phase(phase, format!("{}: {error}", path.display()))
}
