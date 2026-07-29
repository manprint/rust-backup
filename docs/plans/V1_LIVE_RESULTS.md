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
loop devices after the privileged runs. Still unobserved: credential-gated real
AWS and the F3.4 one-vs-four-carrier speed comparison.
