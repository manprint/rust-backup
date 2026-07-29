# V1 live Docker results — 2026-07-29

All commands below were run in this workspace without sudo.

| Matrix | Command | Result |
|---|---|---|
| PostgreSQL 10/12/14/16/18 | `bash e2e/postgres_matrix.sh 10 12 14 16 18` | **PASS=25 FAIL=0** — abort immutability, transfer, source immutability, data equality and schema equality for every major |
| MongoDB 4/5/6/7/8 | `bash e2e/mongodb_matrix.sh 4 5 6 7 8` | **PASS=20 FAIL=0** — abort immutability, transfer, source immutability and destination equality for every major |
| MinIO S3 | `bash e2e/s3_minio_test.sh` | **PASS** — streaming multipart restore and source immutability |

MongoDB 8 uses the maintained `mongo:8` tag rather than `mongo:8.0`: the latter
intentionally refuses Linux kernels >=6.19 (SERVER-121912). This still tests the
MongoDB 8 release line.
