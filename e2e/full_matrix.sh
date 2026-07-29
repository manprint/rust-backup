#!/usr/bin/env bash
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
passed=0; failed=0
run() { if "$@"; then passed=$((passed + 1)); else failed=$((failed + 1)); fi; }
cd "$root"
run bash e2e/s3_minio_test.sh
if [[ ${RUST_BACKUP_FULL_DB_MATRIX:-0} == 1 ]]; then
  run bash e2e/postgres_matrix.sh
  run bash e2e/mongodb_matrix.sh
fi
printf 'e2e summary: %d passed, %d failed\n' "$passed" "$failed"
(( failed == 0 ))
