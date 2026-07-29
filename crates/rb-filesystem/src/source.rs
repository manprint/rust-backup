use std::path::Path;

use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{close, read};
use rb_core::channel::ChunkSink;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, PLAN_FORMAT_VERSION};

use crate::walk::{at_root, checked_root};
use crate::{FilesystemParams, FilesystemPlan};

pub(crate) fn validate_source_params(params: &FilesystemParams) -> Result<()> {
    if params.follow_symlinks {
        return Err(BackupError::phase(
            Phase::Connect,
            "filesystem follow_symlinks is unsupported: it can escape the declared root",
        ));
    }
    if params.preserve_xattr {
        return Err(BackupError::phase(
            Phase::Connect,
            "filesystem preserve_xattr is not implemented by the safe std+nix backend",
        ));
    }
    Ok(())
}

pub(crate) async fn analyze(params: &FilesystemParams) -> Result<BackupPlan> {
    let root = checked_root(params, Phase::Analyze)?;
    let payload = crate::walk::collect(&root)?;
    let mut next_id = 1_u32;
    let items = payload
        .entries
        .iter()
        .filter(|entry| entry.kind == "file")
        .map(|entry| {
            let item = PlanItem {
                id: next_id,
                ordinal: next_id,
                kind: "file".into(),
                name: entry.path.clone(),
                estimated_bytes: entry.size,
                meta: serde_json::json!({"path": entry.path}),
            };
            next_id = next_id.saturating_add(1);
            item
        })
        .collect();
    Ok(BackupPlan {
        format_version: PLAN_FORMAT_VERSION,
        module: "filesystem".into(),
        mode: BackupMode::Copy1to1,
        created_at: now_rfc3339(),
        source_summary: format!(
            "filesystem tree {} ({} entries)",
            root.display(),
            payload.entries.len()
        ),
        items,
        estimated_bytes: payload.total_bytes,
        integrity: IntegritySpec::default(),
        payload: serde_json::to_value(payload)
            .map_err(|e| BackupError::phase(Phase::Analyze, format!("filesystem payload: {e}")))?,
    })
}

pub(crate) async fn stream_out(
    params: &FilesystemParams,
    plan: &BackupPlan,
    sink: &mut dyn ChunkSink,
) -> Result<()> {
    let root = checked_root(params, Phase::Transfer)?;
    let payload = payload(plan, Phase::Transfer)?;
    for item in &plan.items {
        if item.kind != "file" {
            continue;
        }
        let entry = payload
            .entries
            .iter()
            .find(|entry| entry.path == item.name && entry.kind == "file")
            .ok_or_else(|| {
                BackupError::phase(
                    Phase::Transfer,
                    format!("missing file metadata: {}", item.name),
                )
            })?;
        let path = at_root(&root, &entry.path, Phase::Transfer)?;
        let mut reader = NoAtimeReader::open(&path)?;
        let mut offset = 0_u64;
        let mut digest = blake3::Hasher::new();
        loop {
            let data = reader.read_chunk()?;
            if data.is_empty() {
                break;
            }
            digest.update(&data);
            sink.send_chunk(item.id, offset, &data).await?;
            offset += data.len() as u64;
        }
        if offset != entry.size {
            return Err(BackupError::phase(
                Phase::Transfer,
                format!("source file changed size while streaming: {}", entry.path),
            ));
        }
        sink.finish_item(item.id, offset, &digest.finalize().to_hex())
            .await?;
    }
    Ok(())
}

pub(crate) fn read_noatime(path: &Path, mut visit: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut reader = NoAtimeReader::open(path)?;
    loop {
        let bytes = reader.read_chunk()?;
        if bytes.is_empty() {
            return Ok(());
        }
        visit(&bytes)?;
    }
}

pub(crate) fn payload(plan: &BackupPlan, phase: Phase) -> Result<FilesystemPlan> {
    if plan.module != "filesystem" {
        return Err(BackupError::phase(
            phase,
            "filesystem module received another module's plan",
        ));
    }
    serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(phase, format!("bad filesystem plan payload: {e}")))
}

struct NoAtimeReader {
    fd: std::os::fd::RawFd,
}

impl NoAtimeReader {
    fn open(path: &Path) -> Result<Self> {
        let flags = OFlag::O_RDONLY | OFlag::O_NOATIME;
        let fd = match open(path, flags, Mode::empty()) {
            Ok(fd) => Ok(fd),
            Err(Errno::EPERM) => open(path, OFlag::O_RDONLY, Mode::empty()),
            Err(error) => Err(error),
        }
        .map_err(|e| BackupError::phase(Phase::Transfer, format!("{}: {e}", path.display())))?;
        Ok(Self { fd })
    }

    fn read_chunk(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0_u8; 64 * 1024];
        let used = read(self.fd, &mut buf)
            .map_err(|e| BackupError::phase(Phase::Transfer, format!("filesystem read: {e}")))?;
        buf.truncate(used);
        Ok(buf)
    }
}

impl Drop for NoAtimeReader {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as i64;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}
