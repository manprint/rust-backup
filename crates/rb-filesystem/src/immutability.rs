use rb_core::error::{Phase, Result};

use rb_core::wire::CHUNK_SIZE;

use crate::source::Tree;
use crate::walk::checked_root;
use crate::FilesystemParams;

/// Deterministic tree hash over paths, metadata, link targets and file contents.
pub(crate) async fn fingerprint(params: &FilesystemParams) -> Result<String> {
    let params = params.clone();
    crate::blocking(Phase::Analyze, move || fingerprint_tree(&params)).await
}

fn fingerprint_tree(params: &FilesystemParams) -> Result<String> {
    let root = checked_root(params, Phase::Analyze)?;
    let plan = crate::walk::collect(&root)?;
    // The same opener the transfer reads with, so the audit before and after
    // a run reads exactly the objects the stream reads.
    let mut tree = Tree::open(&root, params.allow_atime_updates, Phase::Analyze)?;
    let mut buf = vec![0_u8; CHUNK_SIZE];
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
        // A device node's identity is its device number: same path, same mode,
        // different major/minor is a different device.
        if let Some(rdev) = entry.rdev {
            hasher.update(&rdev.to_le_bytes());
        }
        if entry.kind == "file" {
            let mut reader = tree.open_file(&entry.path)?;
            loop {
                let used = reader.fill(&mut buf)?;
                hasher.update(&buf[..used]);
                if used < buf.len() {
                    break;
                }
            }
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}
