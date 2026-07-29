use rb_core::error::{Phase, Result};

use crate::source::read_noatime;
use crate::walk::checked_root;
use crate::FilesystemParams;

/// Deterministic tree hash over paths, metadata, link targets and file contents.
pub(crate) async fn fingerprint(params: &FilesystemParams) -> Result<String> {
    let root = checked_root(params, Phase::Analyze)?;
    let plan = crate::walk::collect(&root)?;
    let mut hasher = blake3::Hasher::new();
    for entry in &plan.entries {
        hasher.update(entry.path.as_bytes());
        hasher.update(entry.kind.as_bytes());
        hasher.update(&entry.size.to_le_bytes());
        hasher.update(&entry.mode.to_le_bytes());
        hasher.update(&entry.uid.to_le_bytes());
        hasher.update(&entry.gid.to_le_bytes());
        hasher.update(&entry.mtime.to_le_bytes());
        hasher.update(&entry.mtime_nsec.to_le_bytes());
        if let Some(target) = &entry.target {
            hasher.update(target.as_bytes());
        }
        if let Some(target) = &entry.hardlink_to {
            hasher.update(target.as_bytes());
        }
        if entry.kind == "file" {
            let path = crate::walk::at_root(&root, &entry.path, Phase::Analyze)?;
            read_noatime(&path, |bytes| {
                hasher.update(bytes);
                Ok(())
            })?;
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}
