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
| `postgres_matrix.sh` | T-PG-MATRIX, T-PG-IMMUT | 2.8 | pg 10/12/14/16/18 backup→restore→diff + mid-abort immutability |
| `mongodb_matrix.sh` | — | 3 | mongo 4..8 |
| `filesystem_netns_test.sh` | T-FS-OWN, T-FS-IMMUT | 4 | ownership (root vs non-root) + immutability |
| `filesystem_disk_full.sh` | T-FS-ENOSPC | F2.4 | real ext4 ENOSPC, abort propagation, cleanup + immutability |
| `bandwidth_netem.sh` | T-BW | 6 | asymmetric bandwidth/RTT, backpressure proof |
| `s3_minio_test.sh` | — | 5 | MinIO 1:1 |
| `s3_aws_test.sh` | — | 5 | credential-gated real AWS S3 smoke |
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
Set `RUST_BACKUP_FULL_DB_MATRIX=1` to additionally run PostgreSQL 10/12/14/16/18
and MongoDB 4..8 matrices.
