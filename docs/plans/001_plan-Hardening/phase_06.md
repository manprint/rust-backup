# Phase 5 — Filesystem deep verification

> **Intent:** the filesystem module reproduces FIFOs and (with CAP_MKNOD) device nodes,
> refuses unix sockets before any transfer, is proven on a permissions/ownership/link matrix
> run as root, and cannot be redirected through a symlink planted in the destination during
> restore. The fingerprint stays content-based and its cost is documented.
> **Shippable alone?** yes — new entry kinds fail closed on older destinations; every other change is a test or a hardening with a test.
> **Preconditions:** phase_05 DONE (atime guard exists; `e2e/filesystem_netns_test.sh` already carries T-FS-ATIME).

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`,
   finish or revert that unit first (§6 says how far it got). Run the gate
   commands in STATE.md **§3** and check the result against what §1, §7, and §11
   claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type:
   sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to
   `claimed — nothing written yet`.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to
   `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1
   at the next unit with `Status: none`, bump the timestamp. When STATE.md §3 has
   WIP commits on, commit the closed sub-phase and put its sha in the §4 row. A
   sub-phase is not done until this is written.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is
   half-finished into §6 before stopping — plus a `wip(<N.Y>)` commit when WIP
   commits are on.

Shared facts for this phase (recon 2026-09-16):
- `crates/rb-filesystem/src/walk.rs`: `MAX_WALK_DEPTH = 1024` (37), `visit:89` (sorted children 110, non-UTF-8 refusal 116-120/161-173, symlink 121-136 via `symlink_metadata` + `read_link`, hardlink dedupe by `(dev, ino)` 138-147, else-branch hard error `unsupported filesystem entry: {path}` 149-154), `entry_from_meta:175-192` (mode 180, uid/gid 181-182, mtime 186-187, `xattrs: BTreeMap::new()`).
- `crates/rb-filesystem/src/lib.rs`: `FilesystemEntry` (kind string, `size: u64` at 69, `xattrs` 85-89), params struct, `Source::fingerprint:135-137`, `Destination::verify:158-180`, single `mod tests` at 188-785 with 13 tests (names: `analyze_and_stream_are_complete_and_chunked:285`, `a_plan_cannot_chmod_through_a_planted_symlink:362`, `a_non_directory_entry_may_not_be_an_ancestor_of_another:404`, `a_hardlink_must_name_a_file_in_the_plan:436`, `a_restored_file_and_its_parents_start_owner_only:479`, `chmod_never_follows_a_symlink:516`, `a_non_empty_destination_is_refused:551`, `only_canonical_plain_relative_paths_are_accepted:572`, `a_plan_carrying_xattrs_is_refused_at_validate:589`, `pre_epoch_mtimes_are_preserved:617`, `roundtrip_restores_contents_links_and_mode:640`, `interrupted_restore_removes_the_active_partial_file:735`, `fingerprint_is_stable_then_detects_content_change:773`).
- `crates/rb-filesystem/src/dest.rs`: `validate:29-71` (ownership check 53-65), `destination_is_empty:567-580`, root `create_dir_all` at 90, `create_directories:306-314` -> `create_private_dir_all:298-304` (mode `0o700`, constant `OWNER_ONLY_DIR:291`), `ActiveFile::open:236-267` (`O_NOFOLLOW`, `0o600`, `OWNER_ONLY_FILE:290`), symlink creation 320-332, hardlink 333-345, `apply_metadata:352-395` (dirs last; chown 361-370 with `fchownat ... AT_SYMLINK_NOFOLLOW`; `chmod_no_follow` 371-384 masks `0o7777`, skipped for symlinks; `utimensat` 385-393 `NoFollowSymlink`, atime `UTIME_OMIT`), `validate_entries:398-452` (kinds `file dir symlink hardlink`, else `unknown filesystem entry kind`), `has_cap_chown:545-557` (`Uid::effective().is_root()` or `/proc/self/status` `CapEff` bit 0), `verify_metadata:171-224`, raw flag literals `0o400_000`/`0o200_000` at 538/542.
- `crates/rb-filesystem/src/immutability.rs:8-33`: hashes path, kind, size, mode, uid, gid, mtime(+nsec), symlink target, hardlink target, content (BLAKE3) for files.
- Deps: `nix = 0.29.0` with features `fs`, `user` (`crates/rb-filesystem/Cargo.toml`); `mkfifo` is in `nix::unistd`, `mknod`/`major`/`minor`/`SFlag` in `nix::sys::stat` (may need the `fs` feature only — verify by compiling).
- e2e: `e2e/filesystem_netns_test.sh` (root; runuser pattern 40-54; T-FS-OWN/T-FS-IMMUT), `e2e/filesystem_disk_full.sh` (T-FS-ENOSPC), `e2e/lib.sh` `rb_seed_filesystem_fixture:276-306`, `rb_tree_digest:178-221` (Python `os.walk` digest over kind/path/mode/uid/gid/mtime_ns/size/hardlink group/content/symlink target), `rb_tree_manifest:223`, `rb_atime_manifest:260`. CI job `filesystem-privileged` (`.github/workflows/e2e.yml`, `sudo -n`, bare `ubuntu-24.04`).
- Docs: `docs/modules/FILESYSTEM.md` (attribute table at 9), `docs/usage/05-filesystem.md` (flag table 40, attribute table 60, checks 109, troubleshooting 189), `docs/testing/FILESYSTEM_MATRIX.md` (§ 0.1), `docs/IMMUTABILITY.md`.

---

## Sub-phases

### 5.1 Special files: FIFO, char/block devices, sockets
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — implementation; self-review gate (data model, privilege check).
- **Files:** `crates/rb-filesystem/src/walk.rs` (149-154, 175-192), `crates/rb-filesystem/src/lib.rs` (`FilesystemEntry`, tests module), `crates/rb-filesystem/src/dest.rs` (`validate:29-71`, `validate_entries:398-452`, creation 306-345, `apply_metadata:352-395`, `verify_metadata:171-224`, `has_cap_chown:545-557`), `crates/rb-filesystem/src/immutability.rs`, `e2e/lib.sh` (`rb_tree_digest` Python), `docs/modules/FILESYSTEM.md`, `docs/usage/05-filesystem.md`.
- **Change:**
  1. Model: `FilesystemEntry.rdev: Option<u64>` (`#[serde(default)]`); kinds `"fifo"`, `"chardev"`, `"blockdev"`. In `walk.rs` replace the else-branch: `file_type().is_fifo()` -> kind `fifo`, `size = 0`; `is_char_device()` -> `chardev`, `rdev = Some(meta.rdev())`; `is_block_device()` -> `blockdev`, same; `is_socket()` -> error `unsupported filesystem entry (a unix socket cannot be reproduced): {path}` (phase Analyze); anything else keeps the existing message. Special entries carry no data item.
  2. Destination `validate_entries`: accept the three kinds; `chardev`/`blockdev` require `rdev.is_some()`. `validate`: new check `special_files` — when any `chardev`/`blockdev` exists and `!has_cap_mknod()`, fail with `restoring device nodes needs root or CAP_MKNOD (N device entries in the plan)`. Generalize `has_cap_chown` into `has_cap(bit: u32)` (CAP_CHOWN = 0, CAP_MKNOD = 27) and keep `has_cap_chown()` as a thin wrapper.
  3. Creation, inside the existing entry loop after directories exist: `fifo` -> `nix::unistd::mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR)`; `chardev`/`blockdev` -> `nix::sys::stat::mknod(path, SFlag::S_IFCHR | S_IFBLK, Mode::S_IRUSR | Mode::S_IWUSR, rdev as dev_t)`. `apply_metadata` handles these kinds like files (chown, `chmod_no_follow`, `utimensat`); confirm the dirs-last ordering still holds.
  4. `verify_metadata`: compare `rdev` for the two device kinds. Fingerprint: include `rdev` in the per-entry hash for the new kinds (content hash only for `file`).
  5. `rb_tree_digest` Python: include `stat.S_ISFIFO`, `S_ISCHR`, `S_ISBLK` in `kind` and `st_rdev` for devices. `rb_tree_manifest` likewise if it prints kinds.
  6. Docs: `docs/modules/FILESYSTEM.md` attribute table (9) and `docs/usage/05-filesystem.md` attribute table (60) gain rows FIFO / device / socket with the privilege column (`—` / root or CAP_MKNOD / refused); checks table (109) gains `special_files`; troubleshooting (189) gains the two new messages.
  > **Behaviour change:** a tree containing a FIFO or a device node was refused before and now round-trips (devices only with the capability); a unix socket is still refused, with a new message.
- **Unit tests:** (in `lib.rs` tests module) `a_fifo_roundtrips_with_mode_and_mtime` (tempdir, `mkfifo`, `chmod 0640`, backup/restore through the in-memory channel, assert kind/mode/mtime and that `verify_metadata` passes); `a_unix_socket_is_refused_at_analyze` (`std::os::unix::net::UnixListener::bind` inside the tree; `analyze()` returns an Analyze-tagged error containing `unix socket`); `device_entries_need_cap_mknod_at_validate` (hand-built plan with a `chardev` entry and `has_cap` stubbed false through a small seam -> `validate` reports `special_files` failed); `device_entry_without_rdev_is_rejected`; `unknown_entry_kind_is_still_rejected` (kind `"whiteout"`); `older_plan_without_rdev_deserializes`.
- **e2e tests:** T-FS-SPECIAL — in `e2e/filesystem_netns_test.sh` (root): fixture gains `mkfifo`, `mknod null c 1 3`, `mknod loop0 b 7 0`; round-trip digest equal including rdev; a second run with a unix socket in the tree exits non-zero before transfer (destination root absent, message present); a third run as the non-root account with device nodes fails at preflight with the `CAP_MKNOD` message (moved into `e2e/filesystem_matrix.sh` in § 5.3, keep the assertions).
- **Done:** tests green (13 existing + 6 new); T-FS-SPECIAL under `sudo`; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 5.2).

### 5.2 Permissions, ownership and link fixture matrix
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — harness fixtures; self-review.
- **Files:** `e2e/lib.sh` (`rb_seed_filesystem_fixture:276-306` -> add `rb_seed_filesystem_matrix_fixture <root> <mode>` where mode is `root` or `user`), `docs/testing/FILESYSTEM_MATRIX.md` (`Fixture` column).
- **Change:** new function building every `M-FS-01..26, 30` case from `docs/testing/FILESYSTEM_MATRIX.md` under `<root>` with deterministic names `m01_...`; entries needing root (09, 10, 12, 13, 25, 26) are created only in `root` mode; refusal cases (27 socket, 28 non-UTF-8 name `$'\xff\xfe'`, 29 depth 1025 via a loop of `mkdir`) are built by three separate functions `rb_seed_fs_refusal_socket|nonutf8|depth <root>` because each aborts a run. Timestamps via `touch -d '1960-01-01 00:00:00'` (22) and `touch -d '2100-01-01 00:00:00.123456789'` (23). Sparse file (30): `truncate -s 64M` then write 4 KiB at offset 32 MiB. Owners: `chown nobody:nogroup` (12) and `chown 12345:12345` (13). Keep `rb_seed_filesystem_fixture` unchanged for existing callers.
- **Unit tests:** none (shell).
- **e2e tests:** none yet (consumed by § 5.3). Self-check: `bash -n`, ShellCheck, and a manual root run of the seeder followed by `rb_tree_digest` printing without error.
- **Done:** seeder functions present; matrix doc `Fixture` column filled; ShellCheck clean; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 5.3).

### 5.3 `e2e/filesystem_matrix.sh` and CI wiring
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — harness; self-review gate (acceptance assertions).
- **Files:** new `e2e/filesystem_matrix.sh`, `.github/workflows/e2e.yml` (job `filesystem-privileged`), `e2e/full_matrix.sh` (register the script with the root-required skip rule), `e2e/README.md`, `docs/QA_GUIDE.md`.
- **Change:** structure copied from `e2e/filesystem_netns_test.sh` (root guard at line 8, server + source + destination over the relay, `rb_assert_formal_verification`). Cases: (A) root mode: seed with `rb_seed_filesystem_matrix_fixture <src> root`, backup as root, restore as root with ownership preserved; assert `rb_tree_digest` equal (kinds, modes incl. setuid/setgid/sticky, uid/gid incl. 12345, mtimes incl. pre-epoch and nanoseconds, hardlink groups, symlink targets incl. dangling and loop, rdev), `rb_atime_manifest` of the source unchanged, and the sparse file (30) equal in size and content (`cmp`). (B) user mode: seed `user` as the non-root account (runuser pattern), backup and restore as that account with `--no-preserve-ownership`; digest equal ignoring uid/gid. (C) refusals: socket, non-UTF-8, depth 1025 — each run exits non-zero, the destination root does not exist, the source log names the path (for non-UTF-8: the lossy display) and the phase is Analyze. (D) T-FS-ATIME and the T-FS-SPECIAL privilege case moved here from `filesystem_netns_test.sh` (leave the originals in place if moving breaks their labels; do not duplicate assertions in both scripts). Print `PASS M-FS-nn`/`FAIL M-FS-nn` per row by checking the row's entries in the digest comparison (a manifest diff per path), then `MATRIX FS: p pass, f fail`. CI: append `sudo -n "$GITHUB_WORKSPACE/e2e/filesystem_matrix.sh"` to `filesystem-privileged`; raise its timeout to 45 minutes.
- **Unit tests:** none (shell).
- **e2e tests:** T-FS-MATRIX — `sudo bash e2e/filesystem_matrix.sh` prints `MATRIX FS: 30 pass, 0 fail` (rows 01-26, 30 in A/B plus 27-29 refusals; 31 and 32 are asserted by the existing xattr unit test and T-FS-ATIME and print PASS from those checks).
- **Done:** T-FS-MATRIX passes locally under `sudo`; `bash e2e/full_matrix.sh` lists the script; actionlint clean; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 5.4).

### 5.4 Destination TOCTOU hardening for directories
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — hardening; self-review gate.
- **Files:** `crates/rb-filesystem/src/dest.rs` (root `create_dir_all` at 90, `create_private_dir_all:298-304`, `create_directories:306-314`), `crates/rb-filesystem/src/lib.rs` tests module, `docs/modules/FILESYSTEM.md` (guarantees).
- **Change:** `std::fs::create_dir_all` and `DirBuilder::recursive(true)` treat an existing symlink-to-directory as an existing directory (they follow it), so a symlink planted between `destination_is_empty` and directory creation redirects writes outside the destination. Fix: (1) the destination root: after `create_dir_all(root)` (only when it did not exist), `symlink_metadata(root)` must be a directory and not a symlink, else error `destination root is a symlink: {root}` (phase Validate); (2) planned directories are created one level at a time in plan order (parents precede children because paths are sorted and validated canonical, see `only_canonical_plain_relative_paths_are_accepted:572`): `DirBuilder::new().mode(0o700).create(path)`; on `AlreadyExists`, `symlink_metadata(path)` must be a real directory, else error `destination path was replaced by a symlink during restore: {path}` (phase Apply); remove `create_private_dir_all` recursion. Files already use `O_NOFOLLOW` (236-267); `chmod_no_follow` and `utimensat` use no-follow flags (371-393) — confirm and leave. Docs: `docs/modules/FILESYSTEM.md` guarantees paragraph: "directory creation never follows a symlink planted in the destination".
- **Unit tests:** `a_planted_symlink_directory_is_refused_during_restore` (empty dest root; plan with `a/` and `a/f.txt`; before calling the directory-creation step create `root/a -> <tempdir outside>`; assert the Apply-tagged error and that the outside directory received no file); `a_symlink_destination_root_is_refused`; existing `a_plan_cannot_chmod_through_a_planted_symlink:362` and `chmod_never_follows_a_symlink:516` still pass.
- **e2e tests:** T-FS-TOCTOU is the unit pair above (the race window cannot be hit deterministically from a script); T-FS-MATRIX and T-FS-OWN remain the regression.
- **Done:** tests green (existing + 2); T-FS-MATRIX still passes; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 5.5).

### 5.5 Fingerprint cost and scope-edge documentation
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `docs/modules/FILESYSTEM.md` (Limits), `docs/usage/05-filesystem.md`, `docs/testing/FILESYSTEM_MATRIX.md`, `docs/IMMUTABILITY.md` (filesystem fingerprint contract).
- **Change:** D24/D25 — no code. Write: the fingerprint hashes every file's full content before and after the transfer, so a run reads the source tree three times (state the consequence for large trees and that this is the immutability proof); sparse files are restored dense (holes become zeros; size and content identical; disk usage may grow); xattrs and POSIX ACLs are not captured and `--preserve-xattr` is refused at connect; unix sockets are refused. Matrix rows 30, 31 carry `Expected` = `round-trip (dense)` / `refused at connect`.
- **Unit tests:** none.
- **e2e tests:** none.
- **Done:** four docs updated consistently; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 5.6).

### 5.6 Update README.md
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `README.md`.
- **Change:** update "Modules -> Filesystem": what is preserved (regular files, directories, symlinks, hardlinks, FIFOs, device nodes with root or CAP_MKNOD, modes incl. setuid/setgid/sticky, ownership with root or CAP_CHOWN, mtime), what is refused (unix sockets, non-UTF-8 names, depth > 1024, xattrs when requested), what is restored differently (sparse files dense); "Requirements" line for device nodes; "Troubleshooting" entries for `unix socket cannot be reproduced`, `needs root or CAP_MKNOD`, `destination path was replaced by a symlink`. Only shipped behaviour; no internal names; preserve structure, tone, language; edit, do not rewrite.
- **Unit tests:** none (documentation).
- **e2e tests:** none — messages copied from real runs.
- **Done:** a user knows from the README alone which filesystem entries round-trip and which privileges are needed; `bash scripts/gates.sh` green; closed in `STATE.md` with the §11 docs row for phase 5 set; §1 -> 6.1.

---

## Phase gates

- **Fmt:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings` and `bash scripts/source_readonly_lint.sh`
- **Test subset:** `cargo test --locked --all-features -p rb-filesystem` then `bash scripts/gates.sh`
- **Regression guard:** T-FS-OWN, T-FS-IMMUT, T-FS-ENOSPC, T-FS-ATIME, T-E2E0, T-SESSION-2
- **README:** filesystem module section, requirements, troubleshooting

## Phase done criterion
`sudo bash e2e/filesystem_matrix.sh` prints `MATRIX FS: 30 pass, 0 fail`; T-FS-SPECIAL,
T-FS-TOCTOU (unit) and T-FS-ATIME pass; CI job `filesystem-privileged` green on the next run;
`docs/modules/FILESYSTEM.md` and `docs/usage/05-filesystem.md` describe special files, the
capability requirement, the fingerprint cost and the scope edges. README.md reflects this
phase's shipped behavior, and `STATE.md` §11 shows this phase `DONE` with every sub-phase closed.
