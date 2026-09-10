# V1 live Docker results — 2026-07-29

The initial Docker commands below were run in this workspace without sudo.

| Matrix | Command | Result |
|---|---|---|
| PostgreSQL 10/12/14/16/18 | `bash e2e/postgres_matrix.sh 10 12 14 16 18` | **PASS=25 FAIL=0** — abort immutability, transfer, source immutability, data equality and schema equality for every major |
| MongoDB 4/5/6/7/8 | `bash e2e/mongodb_matrix.sh 4 5 6 7 8` | **PASS=20 FAIL=0** — abort immutability, transfer, source immutability and destination equality for every major |
| MinIO S3 | `bash e2e/s3_minio_test.sh` | **PASS** — streaming multipart restore, injected abort cleanup and source immutability |

MongoDB 8 uses the maintained `mongo:8` tag rather than `mongo:8.0`: the latter
intentionally refuses Linux kernels >=6.19 (SERVER-121912). This still tests the
MongoDB 8 release line.

## Severe audit rerun — 2026-07-29

| Matrix | Command | Result |
|---|---|---|
| Rust gates | `bash scripts/gates.sh` | **PASS** |
| Non-privileged aggregate | `bash e2e/full_matrix.sh` | initial **FAIL** in the S3 abort source-status race; fixed with `CompleteAck` |
| MinIO S3 stability | `bash e2e/s3_minio_test.sh` (twice) | **PASS**, **PASS** |
| PostgreSQL 10/12/14/16/18 | `bash e2e/postgres_matrix.sh 10 12 14 16 18` | **PASS=25 FAIL=0** |
| MongoDB 4/5/6/7/8 | `bash e2e/mongodb_matrix.sh 4 5 6 7 8` | **PASS=20 FAIL=0** |

The initial `sudo -n true` probe failed because the later sudoers rule permits
only exact repository script paths. After that rule was installed:

| Privileged matrix | Exact command | Result |
|---|---|---|
| Filesystem ownership/immutability | `sudo -n "$PWD/e2e/filesystem_netns_test.sh"` | **PASS=4 FAIL=0** |
| Real ext4 ENOSPC | `sudo -n "$PWD/e2e/filesystem_disk_full.sh"` | **PASS=5 FAIL=0** |
| Direct/fallback/active-loss | `sudo -n "$PWD/e2e/transport_netns_test.sh"` | **PASS=9 FAIL=0** |
| Netem/backpressure/rate cap | `sudo -n "$PWD/e2e/bandwidth_netem.sh"` | **PASS=6 FAIL=0** — final aggregate: 200 MiB/5 Mbit/s: 354 s, 9,908 KiB RSS; 256 KiB/s: 66 s, 10,228 KiB RSS |

Cleanup observation: zero matching processes, network namespaces, mounts and
loop devices after the privileged runs. Still unobserved at that point:
credential-gated real AWS and the F3.4 one-vs-four-carrier comparison — F3.4 is
closed by the run recorded in the next section; the AWS smoke stays gated on
credentials this workspace does not hold.

## F2/F3 live runs — 2026-09-09

Same workspace; Docker for the backend groups, and the privileged scripts under
the exact-path sudoers rule.

| Matrix | Exact command | Result |
|---|---|---|
| Rust gates | `bash scripts/gates.sh` | **PASS** — 250 tests passed, 0 ignored |
| Fault matrix, all groups | `bash e2e/fault_matrix.sh` | **CASES=23 PASS=57 FAIL=0**, no `SKIP` — every group ran |
| Fault matrix, filesystem | `bash e2e/fault_matrix.sh filesystem` | **CASES=8 PASS=22 FAIL=0** — 7 fault cases × A/B/C (including the `SIGINT` destination interrupt), plus the refusal of a destination a killed run left partial |
| Fault matrix, immutability (F2.5) | `bash e2e/fault_matrix.sh immutability` | **PASS=2 FAIL=0** — a sibling load raises no false `SourceMutated` and still verifies; a real source write exits 6 with `SOURCE-IMMUTABILITY VIOLATION` and no `RESTORE VERIFIED` |
| Fault matrix, postgres | `bash e2e/fault_matrix.sh postgres` | **PASS=9 FAIL=0** (source killed, relay reset, destination backend stopped) |
| Fault matrix, mongodb | `bash e2e/fault_matrix.sh mongodb` | **PASS=9 FAIL=0** |
| Fault matrix, s3 | `bash e2e/fault_matrix.sh s3` | **PASS=9 FAIL=0** |
| Fault matrix, exit codes | `bash e2e/fault_matrix.sh exitcodes` | **PASS=5 FAIL=0**, the rejection case repeated 3× |
| Privileged netem, rate cap and carriers (F3.3 + F3.4) | `sudo -n "$PWD/e2e/bandwidth_netem.sh"` | **PASS=16 FAIL=0** — 200 MiB over a 5 Mbit/s + 80 ms link in 355 s; 16 MiB under a 256 KiB/s cap in 66 s; peak source RSS 10.9 MiB against a 192 MiB ceiling; **1 carrier 43 s vs 4 carriers 44 s (ratio 102 %) on byte-identical restored trees**, with both peers observed negotiating 1 and 4 respectively |
| Relay smoke, plain, 1 and 4 carriers | `bash e2e/relay_smoke.sh`, `RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh` | **PASS=5 FAIL=0** each, including the negotiated-count assertion |
| PostgreSQL 16 regression spot-check after the rollback change | `bash e2e/postgres_matrix.sh 16` | **PASS=12 FAIL=0** — a successful restore, `--overwrite` included, is unaffected by the new failure path |
| MongoDB 7 regression spot-check after the rollback change | `bash e2e/mongodb_matrix.sh 7` | **PASS=5 FAIL=0 SKIPPED=0** |
| MongoDB 8 on this host | `bash e2e/mongodb_matrix.sh 8` | **SKIP (exit 77)** — `mongo:8` refuses Linux 7.0.0 (SERVER-121912); the major is covered by CI, whose runners are on 6.x |
| Two-target YAML session incl. `--fail-fast` | `bash e2e/session_two_targets.sh` | **PASS** |
| Direct, relay fallback and active loss, after the transport cleanups | `sudo -n "$PWD/e2e/transport_netns_test.sh"` | **PASS=9 FAIL=0** |
| Filesystem ownership and abort immutability | `sudo -n "$PWD/e2e/filesystem_netns_test.sh"` | **PASS=5 FAIL=0** |
| Real ext4 ENOSPC | `sudo -n "$PWD/e2e/filesystem_disk_full.sh"` | **PASS=5 FAIL=0** — phase-tagged ENOSPC naming the active item, the abort reason reaching the source, the source immutable, and the partial destination file removed |
| MinIO S3 incl. the per-module carrier cap | `bash e2e/s3_minio_test.sh` | **PASS** — a source asking for 4 carriers negotiates down to the 1 the S3 destination permits |

F3.4 is deliberately a no-regression proof, not a speedup: the link is the
bottleneck on a shaped path, so four carriers can only be shown not to cost
anything and to restore the same bytes. The 102 % ratio is one second of
scheduler noise on a 43-second transfer.

The postgres and mongodb `[B]` assertions were red before the rollback landed:
reverting the PostgreSQL change makes `pg-relay-reset [B]` fail with *"a
half-restored database survived (rows=0)"*, which is what the assertion exists
to catch. The exit-code rejection case was red intermittently until the
destination learned to wait for its rejection frame to be consumed; it is
repeated three times per run for that reason.
