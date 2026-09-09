#!/usr/bin/env bash
# Regression guard: e2e fixtures may only remove resources they created.
# Static by design: safe on developer laptops and CI, no Docker or sudo needed.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

for script in e2e/*.sh scripts/*.sh; do
  bash -n "$script"
done

for script in e2e/transport_netns_test.sh e2e/bandwidth_netem.sh; do
  grep -Eq '^NAMESPACES=\(\)' "$script"
  grep -Eq 'for ns in "\$\{NAMESPACES\[@\]:-\}"' "$script"
  grep -Eq 'NAMESPACES\+=\("\$ns"\)' "$script"
  grep -Eq 'leaked namespace' "$script"
done

for script in e2e/postgres_matrix.sh e2e/mongodb_matrix.sh; do
  grep -Eq '^CONTAINERS=\(\)' "$script"
  grep -Eq 'CONTAINERS\+=\("\$name"\)' "$script"
  grep -Eq 'leaked container' "$script"
done

# The fault matrix starts its own postgres/mongodb/MinIO pairs.
grep -Eq '^CONTAINERS=\(\)' e2e/fault_matrix.sh
grep -Eq 'CONTAINERS\+=\("\$1"\)' e2e/fault_matrix.sh
grep -Eq 'leaked container' e2e/fault_matrix.sh

grep -Eq '^containers=\(\)' e2e/s3_minio_test.sh
grep -Eq 'containers\+=\("\$1"\)' e2e/s3_minio_test.sh
grep -Eq 'leaked container' e2e/s3_minio_test.sh
grep -Eq '^CONTAINER_CREATED=0$' e2e/postgres_introspect.sh
grep -Eq '^CONTAINER_CREATED=1$' e2e/postgres_introspect.sh

echo 'PASS: e2e resource hygiene contract'
