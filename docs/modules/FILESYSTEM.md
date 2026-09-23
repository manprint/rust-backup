# filesystem module

1:1 copy of a directory tree from source to an empty destination root over the
streaming channel. The destination must be absent or empty, unless
`overwrite=true` (`--overwrite`): then every existing entry under the root is
deleted after preflight and before the first payload byte. The root directory
itself stays (it may be a mount point); entries are classified without following
links, so a symlink is unlinked and never traversed, and nothing outside the
root is touched. A symlinked root, a non-directory root and `/` are refused.

## What is preserved

| Attribute | Source (read) | Destination (restore) | Privilege |
|-----------|---------------|------------------------|-----------|
| file contents | always | always | none |
| mode (rwx, setuid/gid, sticky) | always | always | none |
| mtime | always | always | none |
| symlinks / hardlinks | always | recreated | none |
| FIFOs (named pipes) | the node, never its contents | recreated | none |
| character / block device nodes | the node and its device number | recreated | **root or `CAP_MKNOD`** |
| unix sockets | **refused while analyzing** | — | — |
| **uid / gid (ownership)** | always (recorded in plan) | **only when privileged** | **root or `CAP_CHOWN`** |
| xattrs | not yet supported | not yet supported | — |
| **access time (atime) of the source** | left untouched (`O_NOATIME`) | — | **file ownership or `CAP_FOWNER`**; without it the run fails unless `--allow-atime-updates` is passed |

On privileged restores ownership is applied before the final permission bits:
POSIX `chown` clears setuid/setgid on regular files, so the reverse order would
silently turn modes such as `04755` into `0755`.

The **destination root itself** is not restored: its own mode, ownership and
mtime stay whatever the operator created them as. Only the entries *inside* the
tree are reproduced. Give the root the permissions you want before the run.

Files and directories are created owner-only (`0600` / `0700`) and widened to
their recorded mode once the content and ownership are in place, so a restore
never leaves a world-readable window on a file whose final mode is private. The
same applies to FIFOs and device nodes; their mode is set without ever opening
them, because opening a FIFO waits for a peer and opening a device talks to the
device.

A FIFO carries no data item: what is in the pipe belongs to the processes at
either end, not to the filesystem. A unix socket is refused instead of
recreated — the inode can be made, but it would be a dead node that no listener
owns, and copying it would certify something the source does not have:

```text
[Analyze] unsupported filesystem entry (a unix socket cannot be reproduced): /srv/data/app.sock
```

Device nodes are the one kind whose restore needs a capability of its own, and
the destination says so at preflight rather than failing halfway through:

```text
restoring device nodes needs root or CAP_MKNOD (2 device entries in the plan)
```

## The sudo / ownership case (important)

`chown`-ing a restored file to an arbitrary `uid`/`gid` is a privileged operation on
Linux. It requires the **destination** process to run as `root` or hold `CAP_CHOWN`.

- **With privilege** (recommended for system backups): run the destination under
  `sudo`. Ownership is restored exactly:
  ```sh
  sudo rust-backup filesystem destination --to coord:7835 --channel fs --root /restore --yes
  ```
- **Without privilege**: the default ownership-preserving contract fails during
  **preflight** if any planned `uid`/`gid` cannot be honored. Use
  `--no-preserve-ownership` only when running-user ownership is intentional;
  this explicit reduced contract still verifies contents, modes, mtimes and links.

The **source** never needs privilege — it only reads (opened read-only, with
`O_NOATIME` where the platform allows, so the source's access times are not even
touched, upholding I-IMMUT).

Ownership preservation is on by default and has no positive flag of its own:
`--no-preserve-ownership` is the explicit opt-out. In YAML and `-P` the setting
keeps its underlying name, `preserve_ownership: true|false`.

## Completion proof

Before reporting verified completion, the destination rescans the root and
compares the exact planned path/type set, sizes, modes, mtimes, symlink and
hardlink topology, requested ownership and xattrs. It then reopens every regular
file and reproduces all item and payload BLAKE3 commitments. Extra, missing,
unreadable, changed or truncated entries fail the run.

## Plan hardening (the destination treats the plan as hostile input)

The plan arrives over the wire, so every path and mode in it is attacker-
controlled from the destination's point of view. Concretely:

- Every entry path must be a **canonical relative path made only of plain
  components**. `..`, absolute paths, `.`, `a/./b` and `a//b` are all refused.
  `.` used to be accepted, which let a plan re-permission and re-own the
  operator's destination root itself.
- Entry paths must be **unique**, and `kind` must agree with the presence of a
  symlink/hardlink target. Two entries for one path let a later entry act on
  what an earlier one created there.
- The mode is applied with `open(O_NOFOLLOW)` + `fchmod`, never with
  `chmod(2)`, which follows symlinks. Payload bytes are written through
  `O_NOFOLLOW` too. Without this, a plan could plant a symlink and have the
  restore chmod (setuid included) or overwrite the link's target — anywhere on
  a host whose restore runs as root, which is exactly how ownership-preserving
  restores run.
- A plan that carries extended attributes is refused at preflight, since this
  build does not restore them.
- Directory creation never follows a symlink planted in the destination.
  `create_dir_all` treats a symlink that points at a directory as an existing
  directory, so anything able to write the destination's parent between the
  emptiness check and the restore could redirect the whole tree elsewhere.
  Planned directories are therefore created one level at a time, and a path that
  already exists must be a real directory (`symlink_metadata`, which does not
  follow) or the restore stops:

  ```text
  [Apply] destination path was replaced by a symlink during restore: /restore/a
  ```

  The destination root gets the same check, reported as
  `destination root is a symlink: /restore`.
- Metadata verification sorts the received entries before comparing, so it does
  not depend on the peer's ordering.

Names are carried in the plan as UTF-8. A source path or symlink target that is
not valid UTF-8 fails analysis with the offending on-disk path, rather than
being silently renamed to contain U+FFFD — which the destination would then
create, and which verification (comparing mangled against mangled) would
accept. Modification times are signed, so pre-1970 timestamps round-trip
instead of being clamped to the epoch.

The source walk is depth-bounded (1024 levels): it recurses, and an
adversarially deep tree would abort the process rather than return an error.

## Immutability

Files are read with `O_RDONLY` and `O_NOATIME`, so a backup leaves even the
access times untouched. The kernel allows `O_NOATIME` only to the file owner or
to a process with `CAP_FOWNER`; for anything else it answers `EPERM`, and
reading the file would move its atime — a change to the source. That is a
failure, not a fallback:

```text
[Analyze] cannot open /srv/data/file.bin without updating its access time
(O_NOATIME needs file ownership or CAP_FOWNER); run as the file owner or root,
or pass --allow-atime-updates to accept atime changes on the source
```

The refusal comes from the first fingerprint, so it lands before any byte is
transferred and nothing on either host has been touched. Run as the owner or as
root, or pass `--allow-atime-updates` to accept the atime change explicitly —
with the flag the run continues and warns once,
`atime updates on the source accepted by --allow-atime-updates`.

Nothing on the source is written, and the run is bracketed by two fingerprints
over paths, metadata, link targets and contents. The vector list, the
fingerprint contract and the access-time rule are in
[docs/IMMUTABILITY.md](../IMMUTABILITY.md).

**What that costs.** The fingerprint hashes every file's full contents, and it
runs twice — before the transfer and after it. A backup therefore reads the
source tree **three times**: once for the baseline, once to stream it, once to
prove it did not change. On a tree of a few gigabytes this is the dominant cost
of the run, and on a slow disk it roughly triples the time a plain copy would
take. It is not an optimisation that was forgotten: it is the evidence behind
`BACKUP VERIFIED: source unchanged`, and a fingerprint over metadata alone would
certify a tree whose contents had been rewritten in place.

## Current limits

- `--follow-symlinks` is rejected. Following a link could escape the declared
  source root and would no longer preserve that link faithfully.
- `--preserve-xattr` is rejected for now. The module uses only safe `std` +
  `nix` APIs under the workspace `forbid(unsafe_code)` rule; xattr support will
  be added when a safe implementation fits that constraint.
- Regular file reads use Linux `O_NOATIME`. If the caller neither owns the file
  nor holds `CAP_FOWNER`, the kernel rejects that flag and the run fails unless
  `--allow-atime-updates` accepts the access-time change (see "Immutability").
- The destination root's own metadata is never restored (see above).
- **Sparse files are restored dense.** A file with holes comes back with the
  same size and byte-for-byte the same content — the holes read as zeros, and
  that is what is written — so the restore is correct but can occupy more disk
  space than the source did. Nothing in the plan records where the holes were.
- Extended attributes and POSIX ACLs are **not captured at all**, so a restore
  cannot reproduce them; asking for them with `--preserve-xattr` is refused
  while connecting rather than silently dropped.
- **A tree of more than 200 000 entries is refused.** One plan item per regular
  file, and the plan's item ceiling (`MAX_PLAN_ITEMS`) is what bounds the
  destination's allocation from a peer-supplied plan. An ordinary system root
  still exceeds it, so this module is for a data tree, not for `/`. The source
  raises the refusal itself, naming the ceiling, as soon as the plan is built —
  it is no longer a rejection that arrives from the peer after the whole tree
  has been read and sent.
- **The source tree is read three times per run**: the fingerprint before, the
  transfer, and the fingerprint after. The fingerprint hashes every file's
  contents, which is what lets it catch a change that preserved size and mtime,
  and I-IMMUT requires it on both sides of the run — including a run that
  failed. Budget roughly three times the tree's size in source reads, and note
  that the destination read-back adds a fourth full read on its own side.
- Unix sockets are refused while analyzing (see "What is preserved"). FIFOs and
  device nodes round-trip, devices only where the destination has root or
  `CAP_MKNOD`.
- `SIGINT`/`SIGTERM` abort the run: the active item is removed, no `VERIFIED`
  line is printed, and the process exits non-zero with an explicit "interrupted
  by SIGINT/SIGTERM" message. Items already completed before the signal stay on
  disk — an interrupted restore leaves a partial *tree*, never a partial *file*,
  and nothing about it is certified.

## Invariant notes

- **I-IMMUT**: the source tree is opened read-only; the module computes a tree
  fingerprint (path + metadata + content hash) before and after and asserts equality.
- **I-NOTEMP**: files stream in 64 KiB reads straight into the channel; nothing is
  copied to a staging directory.
- An item is published directly at its final path, but its active file is removed
  on EOF, Abort, integrity failure, write error, or ENOSPC; a truncated active
  file is never left looking committed.
- The privileged matrix verifies exact ownership/modes, default non-root
  preflight refusal, the explicit ownership opt-out, interrupted-file cleanup,
  source immutability, and formal persisted read-back evidence.
