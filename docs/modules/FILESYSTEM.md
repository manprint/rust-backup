# filesystem module

1:1 copy of a directory tree from source to an empty destination root over the
streaming channel. The destination must be absent or empty: rust-backup never
deletes pre-existing entries as part of a restore.

## What is preserved

| Attribute | Source (read) | Destination (restore) | Privilege |
|-----------|---------------|------------------------|-----------|
| file contents | always | always | none |
| mode (rwx, setuid/gid, sticky) | always | always | none |
| mtime | always | always | none |
| symlinks / hardlinks | always | recreated | none |
| **uid / gid (ownership)** | always (recorded in plan) | **only when privileged** | **root or `CAP_CHOWN`** |
| xattrs | not yet supported | not yet supported | — |

On privileged restores ownership is applied before the final permission bits:
POSIX `chown` clears setuid/setgid on regular files, so the reverse order would
silently turn modes such as `04755` into `0755`.

## The sudo / ownership case (important)

`chown`-ing a restored file to an arbitrary `uid`/`gid` is a privileged operation on
Linux. It requires the **destination** process to run as `root` or hold `CAP_CHOWN`.

- **With privilege** (recommended for system backups): run the destination under
  `sudo`. Ownership is restored exactly:
  ```sh
  sudo rust-backup filesystem destination --to coord:7835 --channel fs --root /restore --yes
  ```
- **Without privilege**: contents, mode, mtime, and links are restored, but every
  file is owned by the running user. The destination **preflight** records a warning
  for each ownership it cannot honor; the run still completes (degraded fidelity).

The **source** never needs privilege — it only reads (opened read-only, with
`O_NOATIME` where the platform allows, so the source's access times are not even
touched, upholding I-IMMUT).

`--preserve-ownership` (default `true`) toggles the attempt; set it `false` to skip
ownership entirely and silence the warnings when you intentionally want
running-user ownership.

## Current limits

- `--follow-symlinks` is rejected. Following a link could escape the declared
  source root and would no longer preserve that link faithfully.
- `--preserve-xattr` is rejected for now. The module uses only safe `std` +
  `nix` APIs under the workspace `forbid(unsafe_code)` rule; xattr support will
  be added when a safe implementation fits that constraint.
- Regular file reads try Linux `O_NOATIME` first. If the caller does not own the
  file and has no capability to use it, the kernel rejects that flag and the
  read falls back to ordinary read-only open.

## Invariant notes

- **I-IMMUT**: the source tree is opened read-only; the module computes a tree
  fingerprint (path + metadata + content hash) before and after and asserts equality.
- **I-NOTEMP**: files stream in 64 KiB reads straight into the channel; nothing is
  copied to a staging directory.
- An item is published directly at its final path, but its active file is removed
  on EOF, Abort, integrity failure, write error, or ENOSPC; a truncated active
  file is never left looking committed.
- Implementation status: the core path and interrupted-file cleanup are
  unit-tested; privileged root/CAP_CHOWN live coverage still requires the
  documented sudo matrix.
