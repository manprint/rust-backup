# e2e harness

End-to-end tests use real binaries. They create namespaces or Docker containers,
spawn server + source + destination, assert `PASS`/`FAIL`, clean up on exit, and
return non-zero on any failure.

| Script | Test IDs | Phase | What |
|--------|----------|-------|------|
| `relay_smoke.sh` | T-E2E0 | 0 | server + source + dest over the relay move bytes |
| `fault_matrix.sh` | T-FAULT | 7 | in-process protocol faults + live source-kill immutability |
| `session_two_targets.sh` | T-SESSION-2 | 7 | parallel YAML pairs, progress, fail-fast |
| `transport_netns_test.sh` | T-NET1 | 1 | direct path, setup fallback, active-loss fail-safe |
| `postgres_introspect.sh` | T-PG-INTROSPECT | 2.2 | seed a pg, run the gated live introspection test |
| `postgres_matrix.sh` | T-PG-MATRIX, T-PG-IMMUT, T-IMMUT-PG-LP | 2.8, 4.7 | pg 10..18 same/cross-major restore + catalog/data proof + abort immutability + a full run as a read-only role, proven from the server log |
| `postgres_large_table.sh` | T-PG-RSS | 3.4 | one 2 GiB table end to end, peak RSS of both peers sampled and capped |
| `postgres_hot_backup.sh` | T-PG-HOT | — | `--hot-backup` under concurrent writes: consistent snapshot restored and verified, no orphan children, no sequence behind its table; cold run still exit 6, no destination consent exit 3, unsupported module exit 2 |
| `mongodb_matrix.sh` | T-MONGO-IMMUT, T-IMMUT-MONGO-LP | 3, 4.7 | mongo 4..8 same/cross-major restore + catalog/BSON proof + abort/overwrite + a full run as a `read`-only user, proven from the mongod command log |
| `filesystem_netns_test.sh` | T-FS-OWN, T-FS-IMMUT | 4 | ownership (root vs non-root) + immutability |
| `filesystem_matrix.sh` | T-FS-MATRIX, T-FS-SPECIAL, T-FS-ATIME | 5.3 | every `M-FS-*` row: a privileged pass and an unprivileged one over the whole fixture, the three refusals, the access-time guard and the xattr refusal. Root only |
| `filesystem_disk_full.sh` | T-FS-ENOSPC | F2.4 | real ext4 ENOSPC, abort propagation, cleanup + immutability |
| `bandwidth_netem.sh` | T-BW | 6 | asymmetric bandwidth/RTT, backpressure proof |
| `s3_minio_test.sh` | T-S3-IMMUT, T-IMMUT-S3-LP | 5, 4.7 | MinIO 1:1 + a full run under a read-only policy, proven from `mc admin trace` and by a refused write |
| `s3_aws_test.sh` | — | 5 | credential-gated real AWS S3 smoke |
| `resource_hygiene_check.sh` | T-HYGIENE | 8 | static guard: every e2e script parses, and each one only removes namespaces/containers it created. Run by `full_matrix.sh`; no Docker or sudo |
| `full_matrix.sh` | — | 8 | non-privileged matrix; Docker DBs and sudo tests opt in |

The per-row catalogue behind the fidelity runners is
`docs/testing/POSTGRES_MATRIX.md` (`M-PG-*`) and
`docs/testing/FILESYSTEM_MATRIX.md` (`M-FS-*`): case, minimum version or required
privilege, fixture, expected outcome and oracle for every row the runners print.

`relay_smoke.sh` needs only cargo and Python 3. `s3_minio_test.sh` needs Docker.
The filesystem, transport, and bandwidth scripts need Linux `ip`, `iptables`, `tc`,
and non-interactive sudo; invoke them as `sudo -n /abs/path/e2e/<script>.sh`.
They require a fresh release binary, built by the invoking user with
`cargo build --release --all-features` (or pass `RUST_BACKUP_BIN=/absolute/binary`).
Do not treat a sudoers wildcard over this user-writable directory as a security
boundary: it permits arbitrary root code. Use it only on a disposable test host,
or expose a root-owned runner with narrowly validated inputs.

`full_matrix.sh` runs plain and TLS relay (one and four carriers), the non-sudo
fault bank, session orchestration, resource hygiene, and MinIO. Set
`RUST_BACKUP_PRIVILEGED=1` for the filesystem metadata and matrix runs, ENOSPC,
transport and bandwidth sudo tests.
Set `RUST_BACKUP_FULL_DB_MATRIX=1` to additionally run PostgreSQL 10..18 and
MongoDB 4..8 same-version matrices plus the configured cross-version pairs.
Each successful E2E path calls `rb_assert_formal_verification`: both peers must
print their formal verified message, finish at `status="verified"` and `100.0%`,
and report the same 64-character payload BLAKE3.

## Backend helpers in `e2e/lib.sh`

Shared by `postgres_matrix.sh` and any script that needs an external oracle or
I-IMMUT evidence from the server's own log. Fixtures live in
`e2e/fixtures/postgres/` (naming rule in that directory's `README.md`).

| Helper | What it does |
|--------|--------------|
| `RB_PG_LOG_ARGS` | server flags every matrix container starts with: `log_statement=all`, `log_connections=on`, `log_temp_files=0`, `log_line_prefix=%u@%d\|%m\|` |
| `rb_pg_start <name> <image> <host_port> [docker args...]` | starts a PostgreSQL container with `RB_PG_LOG_ARGS`, records it in `RB_PG_CONTAINERS`, waits up to 240 s for `pg_isready` **over TCP** plus a `SELECT 1` — the entrypoint's init-time server listens on the unix socket only, so a socket probe reports a server that is about to be shut down and restarted |
| `rb_pg_container_ip <container>` | the container's IP, for a peer container connecting to it directly |
| `rb_pg_load_fixtures <container> <major> <db> <dir>` | loads `<dir>/*.sql` in lexical order with `ON_ERROR_STOP=1`, skipping `.ge<major>` files above the server major and printing `LOAD`/`SKIP` per file |
| `rb_pg_oracle_schema <dump_container> <host> <port> <user> <db> <out>` | `pg_dump --schema-only` run inside `<dump_container>`, normalised and filtered through `e2e/fixtures/postgres/oracle_ignore.txt` |
| `rb_pg_oracle_counts <container> <user> <db> <out>` | sorted TSV of `rel` (count and order-independent row digest, read `FROM ONLY`), `seq` (`last_value`, `is_called`), `con` (`convalidated`, definition) and `idx` (definition) |
| `rb_pg_assert_readonly_log <container> <user> [since]` | fails when any statement logged for `<user>` is not a read (`SELECT`/`WITH`/`SHOW`/`TABLE`/`VALUES`/`COPY ... TO STDOUT`); `since` is an epoch mark limiting the window to one transfer. Continuation lines of a wrapped statement are rejoined first, so a multi-line `COPY … TO STDOUT` is judged whole |
| `rb_pg_assert_connections <container> <user> <max>` | fails when `<user>` opened more than `<max>` connections |
| `rb_pg_assert_no_temp_files <container>` | fails when the server logged a `temporary file:` line (I-NOTEMP) |
| `rb_mongo_assert_readonly_log <container> [since] [app]` | fails when any command the mongod logged for `appName=<app>` (default `rust-backup`) writes; needs the container started with `--profile 0 --slowms 0`, which logs every command without writing a profile collection. `aggregate` is judged by its pipeline — the driver runs `count_documents` as one — and an empty window is a failure, since silence is not evidence |
| `rb_minio_readonly_policy <bucket>` | prints the least-privilege MinIO policy for a source bucket: `s3:ListBucket`, `s3:GetBucketPolicy`, `s3:GetBucketVersioning` on the bucket and `s3:GetObject`, `s3:GetObjectTagging` on its objects (on AWS, add `s3:GetObjectAcl`) |
| `rb_minio_assert_readonly_trace <trace-file>` | fails when any `s3.*` API in a `mc admin trace --json` capture is not a read; an empty capture is a failure too |
| `rb_seed_filesystem_matrix_fixture <root> [root\|user]` | seeds one deterministic group of entries per `docs/testing/FILESYSTEM_MATRIX.md` row, named after it (`m04_setuid`, `m19_hard_a`, ...). `root` mode adds the rows that need privilege to create: modes 0000, foreign owners, device nodes |
| `rb_seed_fs_refusal_socket\|nonutf8\|depth <root>` | one seeder per refusal row (M-FS-27/28/29). Each aborts a whole run, so each needs its own tree |

Environment switches the scripts read:

| Variable | Default | Used by | Effect |
|----------|---------|---------|--------|
| `RUST_BACKUP_BIN` | built release binary | all | absolute path to an already-built binary |
| `RUST_BACKUP_PRIVILEGED` | `0` | `full_matrix.sh` | also run the sudo filesystem/ENOSPC/transport/bandwidth groups |
| `RUST_BACKUP_FULL_DB_MATRIX` | `0` | `full_matrix.sh` | also run the PostgreSQL and MongoDB version matrices |
| `RUST_BACKUP_FAULT_BACKENDS` | `1` | `fault_matrix.sh` | set `0` to skip the Docker-backed fault groups |
| `RUST_BACKUP_E2E_CARRIERS` | `1` | `relay_smoke.sh` | carriers both peers request |
| `RUST_BACKUP_E2E_BULK_FILES` | `220` | `relay_smoke.sh` | bulk fixture entries, so several carriers stay busy at once |
| `RUST_BACKUP_E2E_TLS` | `0` | `relay_smoke.sh` | generate a self-signed cert and run the relay over TLS |
| `RUST_BACKUP_E2E_KEEP` | `0` | most | keep the temporary work directory and print its path |
| `RUST_BACKUP_AWS_E2E` | `0` | `s3_aws_test.sh` | opt in to the real-AWS smoke; without it the script prints SKIP |
| `RB_PG_IMAGE_REPO` | `postgres` | `postgres_matrix.sh` | set to `postgis/postgis` to run the `M-PG-GIS-*` rows; the tag per major is resolved by `postgis_tag` and a major with no such image exits 77 (SKIP) |
| `RB_PG_NOTEMP_MODE` | `warn` | `postgres_matrix.sh` | `strict` turns the I-NOTEMP row from SKIP into a failure. Since § 3.3 the fingerprint no longer sorts; what still spills is the data stream's `ORDER BY`, which the read-back depends on |
| `RUST_BACKUP_PG_LARGE_ROWS` | `2000000` | `postgres_large_table.sh` | rows in the single large table (about 1 KiB each) |
| `RUST_BACKUP_RSS_LIMIT_KIB` | `262144` | `postgres_large_table.sh`, `bandwidth_netem.sh` | per-process peak-RSS cap |
| `RUST_BACKUP_PG_HOT_PARENTS` | `50000` | `postgres_hot_backup.sh` | parent rows seeded (twice as many children) |
| `RUST_BACKUP_PG_HOT_RATE` | `3000000` | `postgres_hot_backup.sh` | `--max-rate` in bytes/s, so writes land mid-transfer |

Database arguments accept `source:destination`, for example:

```bash
bash e2e/postgres_matrix.sh 10:18 14:17
bash e2e/mongodb_matrix.sh 4:8 6:8
RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh 16 12:16
```

A cross-major PostGIS pair ships two PostGIS versions, so the case runs with
`--extension-version default`; `M-PG-GIS-06` first asserts that the same pair is
refused at preflight without it.
