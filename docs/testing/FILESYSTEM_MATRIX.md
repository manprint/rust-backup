# Filesystem fidelity matrix

Catalogue of the POSIX filesystem cases the module must reproduce 1:1, or refuse
before any byte is transferred. The runner is `e2e/filesystem_matrix.sh`: it
prints one `PASS <ID>`, `FAIL <ID>` or `SKIP <ID>` line per row and a final
`MATRIX FS: p pass, f fail` summary. This document is the catalogue only — it
never records execution status; the runner output is the single source of truth
for that.

Row IDs are `M-FS-<nn>` and the seeded entries embed the row number
(`m01_plain.txt`, `m14_rel_link`, ...). `Requires` is `root` when the row is
seeded only in the root pass of the runner (`sudo bash e2e/filesystem_matrix.sh`)
and `none` when it runs in both the root and the unprivileged pass. `Expected` is
`round-trip` (the entry must exist on the destination with identical metadata and
content) or `refused before transfer` (the run must fail during Analyze, with no
destination tree left behind). `Oracle` names the check that decides the row:

| Oracle | Meaning |
|--------|---------|
| `tree digest` | `rb_tree_digest` of source and destination equal, with the per-path `rb_tree_manifest` row of this entry compared field by field (kind, mode, uid, gid, mtime_ns, size, hardlink group, content or link target) |
| `atime manifest` | `rb_atime_manifest` of the source identical before and after the run |
| `content compare` | `cmp` of the source and destination file |
| `refusal` | the source exits non-zero before transfer, the message names the offending path, the phase is Analyze, and the destination root does not exist |

| ID | Case | Requires | Fixture | Expected | Oracle |
|----|------|----------|---------|----------|--------|
| M-FS-01 | regular files with mode 0644, 0600, 0400 | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-02 | empty file | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-03 | empty nested directories | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-04 | setuid file (4755) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-05 | setgid file (2755) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-06 | setuid and setgid file (6755) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-07 | setgid directory (2775) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-08 | sticky directory (1777) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-09 | directory mode 0000 | root | `rb_seed_filesystem_matrix_fixture` (root mode) | round-trip | tree digest |
| M-FS-10 | file mode 0000 | root | `rb_seed_filesystem_matrix_fixture` (root mode) | round-trip | tree digest |
| M-FS-11 | directory 0500 containing a 0400 file | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-12 | files owned by `nobody:nogroup` | root | `rb_seed_filesystem_matrix_fixture` (root mode) | round-trip | tree digest |
| M-FS-13 | file owned by a uid/gid absent from the host (12345:12345) | root | `rb_seed_filesystem_matrix_fixture` (root mode) | round-trip | tree digest |
| M-FS-14 | relative symlink | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-15 | absolute symlink | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-16 | dangling symlink | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-17 | symlink to a directory | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-18 | symlink loop (a -> b -> a) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest |
| M-FS-19 | hardlink pair in the same directory | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest (same link group) |
| M-FS-20 | hardlink triple across directories | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest (same link group) |
| M-FS-21 | hardlink to a setuid file | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest (same link group, mode preserved) |
| M-FS-22 | pre-epoch mtime (1960-01-01) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest (mtime_ns) |
| M-FS-23 | far-future mtime (2100-01-01) with nanoseconds | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest (mtime_ns) |
| M-FS-24 | FIFO | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest (kind) |
| M-FS-25 | character device `c 1 3` | root | `rb_seed_filesystem_matrix_fixture` (root mode) | round-trip | tree digest (kind and rdev) |
| M-FS-26 | block device `b 7 0` | root | `rb_seed_filesystem_matrix_fixture` (root mode) | round-trip | tree digest (kind and rdev) |
| M-FS-27 | unix socket | none | `rb_seed_fs_refusal_socket` | refused before transfer | refusal |
| M-FS-28 | non-UTF-8 file name | none | `rb_seed_fs_refusal_nonutf8` | refused before transfer | refusal (lossy path display) |
| M-FS-29 | tree deeper than 1024 | none | `rb_seed_fs_refusal_depth` | refused before transfer | refusal |
| M-FS-30 | sparse 64 MiB file: size and content equal, holes not preserved (documented limit) | none | `rb_seed_filesystem_matrix_fixture` | round-trip | tree digest + content compare |
| M-FS-31 | xattr present with `--preserve-xattr` | none | unit test `rb-filesystem` | refused at connect (existing behaviour) | refusal |
| M-FS-32 | non-owner source without `--allow-atime-updates`; round-trip with the flag | root | `rb_seed_filesystem_matrix_fixture` (root mode) plus a non-owner run | refused before transfer, then round-trip | refusal, then tree digest + atime manifest |
