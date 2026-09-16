#!/usr/bin/env bash
# Installs the files of the custom extension `rbtest` into a running PostgreSQL
# container, so 70_extensions.sql can `CREATE EXTENSION rbtest`.
#
# Usage: bash e2e/fixtures/postgres/71_rbtest_extension.sh <container> [version]
#   version 1.0 (default) installs 1.0 and makes it the default version.
#   version 1.1 installs ONLY 1.1: a destination prepared this way cannot
#   satisfy a source that runs 1.0, which is matrix row M-PG-EXT-08.
set -euo pipefail

container=${1:?usage: 71_rbtest_extension.sh <container> [version]}
version=${2:-1.0}
case "$version" in
  1.0|1.1) ;;
  *) echo "ERROR: unsupported rbtest version $version" >&2; exit 2 ;;
esac

sharedir=$(docker exec "$container" pg_config --sharedir)
extension_dir="$sharedir/extension"

docker exec -i "$container" tee "$extension_dir/rbtest.control" >/dev/null <<CONTROL
# rbtest extension for the rust-backup fidelity matrix
comment = 'configuration-table fixture for M-PG-EXT-07 and M-PG-EXT-08'
default_version = '$version'
relocatable = true
CONTROL

docker exec -i "$container" tee "$extension_dir/rbtest--$version.sql" >/dev/null <<SCRIPT
-- rbtest $version: one configuration table whose rows above k = 1000 are user
-- data registered with pg_extension_config_dump.
CREATE TABLE rbtest_cfg (k integer PRIMARY KEY, v text);
INSERT INTO rbtest_cfg VALUES (1, 'shipped-a'), (2, 'shipped-b'), (3, 'shipped-c');
SELECT pg_catalog.pg_extension_config_dump('rbtest_cfg', 'WHERE k >= 1000');
SCRIPT

echo "rbtest $version installed into $container ($extension_dir)"
