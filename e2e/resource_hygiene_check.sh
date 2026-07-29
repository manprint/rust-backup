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
  rg -q '^NAMESPACES=\(\)' "$script"
  rg -q 'for ns in "\$\{NAMESPACES\[@\]:-\}"' "$script"
  rg -q 'NAMESPACES\+=\("\$ns"\)' "$script"
  rg -q 'leaked namespace' "$script"
done

for script in e2e/postgres_matrix.sh e2e/mongodb_matrix.sh; do
  rg -q '^CONTAINERS=\(\)' "$script"
  rg -q 'CONTAINERS\+=\("\$name"\)' "$script"
  rg -q 'leaked container' "$script"
done

rg -q '^containers=\(\)' e2e/s3_minio_test.sh
rg -q 'containers\+=\("\$1"\)' e2e/s3_minio_test.sh
rg -q 'leaked container' e2e/s3_minio_test.sh
rg -q '^CONTAINER_CREATED=0$' e2e/postgres_introspect.sh
rg -q '^CONTAINER_CREATED=1$' e2e/postgres_introspect.sh

echo 'PASS: e2e resource hygiene contract'
