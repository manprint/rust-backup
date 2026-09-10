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

The **destination root itself** is not restored: its own mode, ownership and
mtime stay whatever the operator created them as. Only the entries *inside* the
tree are reproduced. Give the root the permissions you want before the run.

Files and directories are created owner-only (`0600` / `0700`) and widened to
their recorded mode once the content and ownership are in place, so a restore
never leaves a world-readable window on a file whose final mode is private.

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

## Current limits

- `--follow-symlinks` is rejected. Following a link could escape the declared
  source root and would no longer preserve that link faithfully.
- `--preserve-xattr` is rejected for now. The module uses only safe `std` +
  `nix` APIs under the workspace `forbid(unsafe_code)` rule; xattr support will
  be added when a safe implementation fits that constraint.
- Regular file reads try Linux `O_NOATIME` first. If the caller does not own the
  file and has no capability to use it, the kernel rejects that flag and the
  read falls back to ordinary read-only open.
- The destination root's own metadata is never restored (see above).
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
