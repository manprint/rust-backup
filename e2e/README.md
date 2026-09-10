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
| `postgres_matrix.sh` | T-PG-MATRIX, T-PG-IMMUT | 2.8 | pg 10..18 same/cross-major restore + catalog/data proof + abort immutability |
| `mongodb_matrix.sh` | — | 3 | mongo 4..8 same/cross-major restore + catalog/BSON proof + abort/overwrite |
| `filesystem_netns_test.sh` | T-FS-OWN, T-FS-IMMUT | 4 | ownership (root vs non-root) + immutability |
| `filesystem_disk_full.sh` | T-FS-ENOSPC | F2.4 | real ext4 ENOSPC, abort propagation, cleanup + immutability |
| `bandwidth_netem.sh` | T-BW | 6 | asymmetric bandwidth/RTT, backpressure proof |
| `s3_minio_test.sh` | — | 5 | MinIO 1:1 |
| `s3_aws_test.sh` | — | 5 | credential-gated real AWS S3 smoke |
| `resource_hygiene_check.sh` | T-HYGIENE | 8 | static guard: every e2e script parses, and each one only removes namespaces/containers it created. Run by `full_matrix.sh`; no Docker or sudo |
| `full_matrix.sh` | — | 8 | non-privileged matrix; Docker DBs and sudo tests opt in |

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
`RUST_BACKUP_PRIVILEGED=1` for filesystem/ENOSPC/transport/bandwidth sudo tests.
Set `RUST_BACKUP_FULL_DB_MATRIX=1` to additionally run PostgreSQL 10..18 and
MongoDB 4..8 same-version matrices plus the configured cross-version pairs.
Each successful E2E path calls `rb_assert_formal_verification`: both peers must
print their formal verified message, finish at `status="verified"` and `100.0%`,
and report the same 64-character payload BLAKE3.

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

Database arguments accept `source:destination`, for example:

```bash
bash e2e/postgres_matrix.sh 10:18 14:17
bash e2e/mongodb_matrix.sh 4:8 6:8
```
