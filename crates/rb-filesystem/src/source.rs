use std::ffi::OsStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use nix::errno::Errno;
use nix::fcntl::{fcntl, open, openat, FcntlArg, OFlag};
use nix::sys::stat::{fstat, Mode, SFlag};
use nix::unistd::read;
use rb_core::channel::ChunkSink;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, PLAN_FORMAT_VERSION};
use rb_core::wire::CHUNK_SIZE;
use serde::Deserialize;

use crate::walk::{checked_root, relative_path};
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
    let params = params.clone();
    crate::blocking(Phase::Analyze, move || analyze_tree(&params)).await
}

fn analyze_tree(params: &FilesystemParams) -> Result<BackupPlan> {
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

/// Chunks the reader thread may run ahead of the wire: enough to overlap disk
/// reads with sends — small files especially — while keeping the run's file
/// data in memory to a few MiB.
const READ_AHEAD: usize = 4;

/// A planned file, as the reader thread needs it.
struct Planned {
    item_id: u32,
    path: String,
    size: u64,
}

/// What the reader thread hands the async side, in plan order.
enum Read {
    /// `buf[..len]` is the item's data at `offset`. `buf` is always
    /// `CHUNK_SIZE` long and goes back to the reader for reuse: a fresh,
    /// zeroed 1 MiB buffer per small file costs more than the file.
    Chunk {
        item_id: u32,
        offset: u64,
        buf: Vec<u8>,
        len: usize,
    },
    End {
        item_id: u32,
        total: u64,
        blake3: String,
    },
}

/// Stream every planned file, in plan order, in full `CHUNK_SIZE` chunks.
///
/// One blocking thread opens, reads and hashes the files and runs at most
/// `READ_AHEAD` chunks ahead; this task only forwards to the sink. Disk I/O on a
/// runtime worker would stall the transport tasks behind it — heartbeats
/// included — whenever the source filesystem is slow. A sink that blocks on a
/// full window fills the channel and stops the reader: backpressure still
/// reaches the disk (I-BANDWIDTH).
pub(crate) async fn stream_out(
    params: &FilesystemParams,
    plan: &BackupPlan,
    sink: &mut dyn ChunkSink,
) -> Result<()> {
    let root = checked_root(params, Phase::Transfer)?;
    let payload = payload(plan, Phase::Transfer)?;
    // Index once: a linear scan per item is O(items x entries), which at the
    // core's 200 000-item cap is tens of billions of string comparisons.
    let sizes: std::collections::HashMap<&str, u64> = payload
        .entries
        .iter()
        .filter(|entry| entry.kind == "file")
        .map(|entry| (entry.path.as_str(), entry.size))
        .collect();
    let mut files = Vec::with_capacity(plan.items.len());
    for item in plan.items.iter().filter(|item| item.kind == "file") {
        let size = *sizes.get(item.name.as_str()).ok_or_else(|| {
            BackupError::phase(
                Phase::Transfer,
                format!("missing file metadata: {}", item.name),
            )
        })?;
        files.push(Planned {
            item_id: item.id,
            path: item.name.clone(),
            size,
        });
    }
    let allow_atime_updates = params.allow_atime_updates;
    let (tx, mut rx) = tokio::sync::mpsc::channel(READ_AHEAD);
    let (spare_tx, spare_rx) = std::sync::mpsc::channel();
    let reader = tokio::task::spawn_blocking(move || {
        read_files(&root, allow_atime_updates, &files, &tx, &spare_rx)
    });
    let sent = send_files(&mut rx, sink, &spare_tx).await;
    // A reader still ahead of a failed sink is blocked on the full channel;
    // closing it is what stops the reader, so the join below is bounded by one
    // read.
    drop(rx);
    let read = reader.await.map_err(|error| {
        BackupError::phase(
            Phase::Transfer,
            format!("filesystem reader task failed: {error}"),
        )
    })?;
    // The sink's failure first: a reader that lost its consumer just stops.
    sent?;
    read
}

async fn send_files(
    rx: &mut tokio::sync::mpsc::Receiver<Read>,
    sink: &mut dyn ChunkSink,
    spare: &std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    while let Some(read) = rx.recv().await {
        match read {
            Read::Chunk {
                item_id,
                offset,
                buf,
                len,
            } => {
                sink.send_chunk(item_id, offset, &buf[..len]).await?;
                // A reader that already finished frees the buffer instead.
                let _ = spare.send(buf);
            }
            Read::End {
                item_id,
                total,
                blake3,
            } => sink.finish_item(item_id, total, &blake3).await?,
        }
    }
    // The reader closed the channel: finished, or failed and reports why.
    Ok(())
}

/// The reader thread. Returns `Ok` when the async side hung up: that side
/// carries the reason.
fn read_files(
    root: &Path,
    allow_atime_updates: bool,
    files: &[Planned],
    tx: &tokio::sync::mpsc::Sender<Read>,
    spare: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let mut tree = Tree::open(root, allow_atime_updates, Phase::Transfer)?;
    let mut unused = None;
    for file in files {
        let mut reader = tree.open_file(&file.path)?;
        let mut digest = blake3::Hasher::new();
        let mut offset = 0_u64;
        loop {
            // `try_recv` only: a blocking `recv` would wait on a consumer that
            // may already be gone.
            let mut buf = unused
                .take()
                .or_else(|| spare.try_recv().ok())
                .unwrap_or_else(|| vec![0_u8; CHUNK_SIZE]);
            let len = reader.fill(&mut buf)?;
            if len == 0 {
                unused = Some(buf);
                break;
            }
            // Refuse a growing file at the first byte past its planned size,
            // not after streaming however much it grew.
            if offset + len as u64 > file.size {
                return Err(BackupError::phase(
                    Phase::Transfer,
                    format!("source file grew while streaming: {}", file.path),
                ));
            }
            digest.update(&buf[..len]);
            let read = Read::Chunk {
                item_id: file.item_id,
                offset,
                buf,
                len,
            };
            if tx.blocking_send(read).is_err() {
                return Ok(());
            }
            offset += len as u64;
            // `fill` stops short only at end of file.
            if len < CHUNK_SIZE {
                break;
            }
        }
        if offset != file.size {
            return Err(BackupError::phase(
                Phase::Transfer,
                format!("source file changed size while streaming: {}", file.path),
            ));
        }
        let end = Read::End {
            item_id: file.item_id,
            total: offset,
            blake3: digest.finalize().to_hex().to_string(),
        };
        if tx.blocking_send(end).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

pub(crate) fn payload(plan: &BackupPlan, phase: Phase) -> Result<FilesystemPlan> {
    if plan.module != "filesystem" {
        return Err(BackupError::phase(
            phase,
            "filesystem module received another module's plan",
        ));
    }
    // Deserialize from the borrowed value: the payload lists every entry of
    // the tree, and cloning it first doubled that memory for nothing.
    FilesystemPlan::deserialize(&plan.payload)
        .map_err(|e| BackupError::phase(phase, format!("bad filesystem plan payload: {e}")))
}

/// Opens the files of one tree without following a symlink anywhere below its
/// root.
///
/// The plan is walked by path, and the tree can change before a file is read.
/// Opening `root.join(path)` follows every symlink on the path, so a directory
/// swapped for a link (`a -> /root`) between the walk and the read would stream
/// a file from outside the declared root — and the closing immutability audit
/// only notices after the bytes have left. Each component is opened from the
/// one before it with `O_NOFOLLOW` instead, so a swap fails the open. The root
/// itself is resolved normally: an operator may name a symlinked root.
pub(crate) struct Tree {
    root: PathBuf,
    root_fd: OwnedFd,
    allow_atime_updates: bool,
    phase: Phase,
    /// The directory of the last file opened, `None` for the root itself:
    /// siblings, which the walk keeps adjacent, skip the component walk.
    parent: Option<(PathBuf, Option<OwnedFd>)>,
}

impl Tree {
    pub(crate) fn open(root: &Path, allow_atime_updates: bool, phase: Phase) -> Result<Self> {
        let root_fd = open(
            root,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| BackupError::phase(phase, format!("{}: {e}", root.display())))?;
        Ok(Self {
            root: root.to_path_buf(),
            root_fd,
            allow_atime_updates,
            phase,
            parent: None,
        })
    }

    /// Open the planned regular file `path` (a plan path, relative to the root).
    pub(crate) fn open_file(&mut self, path: &str) -> Result<NoAtimeReader> {
        let rel = relative_path(path, self.phase)?;
        let full = self.root.join(&rel);
        let name = rel.file_name().ok_or_else(|| {
            BackupError::phase(self.phase, format!("unsafe filesystem plan path: {path:?}"))
        })?;
        let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
        if self.parent.as_ref().map(|(cached, _)| cached.as_path()) != Some(parent_rel) {
            // Drop the old parent first: a failed walk must not leave a stale
            // directory paired with the new path.
            self.parent = None;
            let mut dir: Option<OwnedFd> = None;
            for component in parent_rel.components() {
                let at = dir.as_ref().unwrap_or(&self.root_fd);
                let next = openat(
                    at,
                    component.as_os_str(),
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| open_error(self.phase, &full, error))?;
                dir = Some(next);
            }
            self.parent = Some((parent_rel.to_path_buf(), dir));
        }
        let dir = match &self.parent {
            Some((_, Some(dir))) => dir.as_fd(),
            _ => self.root_fd.as_fd(),
        };
        NoAtimeReader::open_at(dir, name, &full, self.allow_atime_updates, self.phase)
    }
}

/// `ELOOP` (a symlink under `O_NOFOLLOW`) and `ENOTDIR` (a non-directory on the
/// path) both mean the entry is no longer what the walk recorded.
fn open_error(phase: Phase, path: &Path, error: Errno) -> BackupError {
    match error {
        Errno::ELOOP | Errno::ENOTDIR => changed_type(phase, path),
        error => BackupError::phase(phase, format!("{}: {error}", path.display())),
    }
}

fn changed_type(phase: Phase, path: &Path) -> BackupError {
    BackupError::phase(
        phase,
        format!(
            "{} is no longer a regular file below the root (it, or a directory on its path, \
             changed type since the tree was walked)",
            path.display()
        ),
    )
}

/// Warned once per run, not once per file: a tree of a million foreign-owned
/// files would otherwise print a million identical lines.
static ATIME_FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub(crate) struct NoAtimeReader {
    /// Owned: dropping the reader closes the descriptor.
    fd: OwnedFd,
    /// For error messages.
    path: PathBuf,
    phase: Phase,
}

impl NoAtimeReader {
    fn open_at(
        dir: BorrowedFd<'_>,
        name: &OsStr,
        path: &Path,
        allow_atime_updates: bool,
        phase: Phase,
    ) -> Result<Self> {
        Self::open_at_with(
            dir,
            name,
            path,
            allow_atime_updates,
            phase,
            |dir, name, flags| openat(dir, name, flags, Mode::empty()),
        )
    }

    /// `opener` is a seam: `O_NOATIME` fails with `EPERM` only for a file this
    /// process neither owns nor has `CAP_FOWNER` over, which a unit test cannot
    /// create without root. The tests inject that answer instead.
    ///
    /// `O_NOFOLLOW` refuses a symlink swapped in for the file; `O_NONBLOCK`
    /// keeps a FIFO swapped in from blocking the open forever, waiting for a
    /// writer. Either is then refused by the regular-file check.
    pub(crate) fn open_at_with(
        dir: BorrowedFd<'_>,
        name: &OsStr,
        path: &Path,
        allow_atime_updates: bool,
        phase: Phase,
        mut opener: impl FnMut(BorrowedFd<'_>, &OsStr, OFlag) -> nix::Result<OwnedFd>,
    ) -> Result<Self> {
        let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        let fd = match opener(dir, name, flags | OFlag::O_NOATIME) {
            Ok(fd) => fd,
            // The kernel refuses `O_NOATIME` on a file we do not own: reading it
            // WILL move its access time, which is a change to the source. That
            // is a failure unless the operator accepted it explicitly — and the
            // first reader is the fingerprint-before pass, so the refusal lands
            // before a single byte has been transferred.
            Err(Errno::EPERM) if !allow_atime_updates => {
                return Err(BackupError::phase(
                    phase,
                    format!(
                        "cannot open {} without updating its access time (O_NOATIME needs file \
                         ownership or CAP_FOWNER); run as the file owner or root, or pass \
                         --allow-atime-updates to accept atime changes on the source",
                        path.display()
                    ),
                ));
            }
            Err(Errno::EPERM) => {
                if !ATIME_FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!("atime updates on the source accepted by --allow-atime-updates");
                }
                opener(dir, name, flags).map_err(|error| open_error(phase, path, error))?
            }
            Err(error) => return Err(open_error(phase, path, error)),
        };
        let stat = fstat(&fd)
            .map_err(|e| BackupError::phase(phase, format!("{}: {e}", path.display())))?;
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
            return Err(changed_type(phase, path));
        }
        // Reads block again from here on: some FUSE filesystems honour
        // `O_NONBLOCK` on a regular file and answer `EAGAIN`.
        let status = OFlag::from_bits_truncate(
            fcntl(&fd, FcntlArg::F_GETFL)
                .map_err(|e| BackupError::phase(phase, format!("{}: {e}", path.display())))?,
        );
        fcntl(&fd, FcntlArg::F_SETFL(status - OFlag::O_NONBLOCK))
            .map_err(|e| BackupError::phase(phase, format!("{}: {e}", path.display())))?;
        Ok(Self {
            fd,
            path: path.to_path_buf(),
            phase,
        })
    }

    /// Fill `buf` from the file; fewer bytes than `buf.len()` only at end of
    /// file, so a stream of full buffers carries full wire chunks.
    pub(crate) fn fill(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut used = 0;
        while used < buf.len() {
            match read(&self.fd, &mut buf[used..]) {
                Ok(0) => break,
                Ok(n) => used += n,
                // `nix::read` surfaces `EINTR`, which `std`'s reader retries on
                // our behalf: a signal delivered mid-read is an interruption of
                // the syscall, not a failure of the file, and abandoning a tree
                // halfway over one would be a self-inflicted backup failure.
                Err(Errno::EINTR) => continue,
                Err(error) => {
                    return Err(BackupError::phase(
                        self.phase,
                        format!("read {}: {error}", self.path.display()),
                    ))
                }
            }
        }
        Ok(used)
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
