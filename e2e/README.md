# e2e harness

End-to-end tests using Linux network namespaces (transport/relay/direct) and Docker
containers (postgres/mongodb/minio), following the bore `scripts/vhost_netns_test.sh`
pattern: create namespaces, spawn server + source + destination, assert, count
`PASS`/`FAIL`, `trap cleanup EXIT INT TERM`, exit non-zero on any failure.

| Script | Test IDs | Phase | What |
|--------|----------|-------|------|
| `relay_smoke.sh` | T-E2E0 | 0 | server + source + dest over the relay move bytes |
| `transport_netns_test.sh` | T-NET1 | 1 | direct path when reachable, relay when blocked |
| `postgres_introspect.sh` | T-PG-INTROSPECT | 2.2 | seed a pg, run the gated live introspection test |
| `postgres_matrix.sh` | T-PG-MATRIX, T-PG-IMMUT | 2.8 | pg 10/12/14/16/18 backup→restore→diff + mid-abort immutability |
| `mongodb_matrix.sh` | — | 3 | mongo 4..8 |
| `filesystem_netns_test.sh` | T-FS-IMMUT | 4 | ownership (root vs non-root) + immutability |
| `bandwidth_netem.sh` | T-BW | 6 | asymmetric bandwidth/RTT, backpressure proof |
| `s3_minio_test.sh` | — | 5 | MinIO 1:1 |
| `full_matrix.sh` | — | 8 | everything, direct + relay |

NOPASSWD sudo for netns scripts is per-exact-path: invoke `sudo -n /abs/path/e2e/<script>.sh`.
Rebuild the release binary (`cargo build --release --all-features`) before sudo-running.

Scripts beyond `relay_smoke.sh` are created in their respective phases.
