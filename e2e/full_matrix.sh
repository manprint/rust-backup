#!/usr/bin/env bash
# Full local matrix. Network-namespace scripts require a sudoers rule for each
# exact absolute script path; database matrices are opt-in because they pull images.
set -u
root=$(cd "$(dirname "$0")/.." && pwd)
passed=0; failed=0
run() { if "$@"; then printf 'PASS: %s\n' "$*"; passed=$((passed + 1)); else printf 'FAIL: %s\n' "$*" >&2; failed=$((failed + 1)); fi; }
cd "$root"
run bash e2e/resource_hygiene_check.sh
run bash e2e/relay_smoke.sh
run env RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh
run env RUST_BACKUP_E2E_TLS=1 bash e2e/relay_smoke.sh
run env RUST_BACKUP_E2E_TLS=1 RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh
run bash e2e/session_two_targets.sh
run bash e2e/fault_matrix.sh
run bash e2e/s3_minio_test.sh
if [[ ${RUST_BACKUP_PRIVILEGED:-0} == 1 ]]; then
  run sudo -n "$root/e2e/filesystem_netns_test.sh"
  run sudo -n "$root/e2e/filesystem_disk_full.sh"
  run sudo -n "$root/e2e/transport_netns_test.sh"
  run sudo -n "$root/e2e/bandwidth_netem.sh"
else
  printf 'SKIP: privileged netns/bandwidth tests (set RUST_BACKUP_PRIVILEGED=1)\n'
fi
if [[ ${RUST_BACKUP_FULL_DB_MATRIX:-0} == 1 ]]; then
  run bash e2e/postgres_introspect.sh 16
  run bash e2e/postgres_matrix.sh 10 12 14 16 18
  run bash e2e/mongodb_matrix.sh 4 5 6 7 8
fi
printf 'e2e summary: %d passed, %d failed\n' "$passed" "$failed"
(( failed == 0 ))
