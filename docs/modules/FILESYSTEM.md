# filesystem module

1:1 copy of a directory tree from source to destination over the streaming channel.

## What is preserved

| Attribute | Source (read) | Destination (restore) | Privilege |
|-----------|---------------|------------------------|-----------|
| file contents | always | always | none |
| mode (rwx, setuid/gid, sticky) | always | always | none |
| mtime | always | always | none |
| symlinks / hardlinks | always | recreated | none |
| **uid / gid (ownership)** | always (recorded in plan) | **only when privileged** | **root or `CAP_CHOWN`** |
| xattrs | with `--preserve-xattr` | with `--preserve-xattr` | varies |

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

## Invariant notes

- **I-IMMUT**: the source tree is opened read-only; the module computes a tree
  fingerprint (path + metadata + content hash) before and after and asserts equality.
- **I-NOTEMP**: files stream in 64 KiB reads straight into the channel; nothing is
  copied to a staging directory.
- Implementation status: **stub** (see `docs/plans/RUST_BACKUP_PLAN.md` Phase 4).
