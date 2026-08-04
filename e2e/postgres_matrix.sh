#!/usr/bin/env bash
# T-PG-MATRIX + T-PG-IMMUT (plan Phase 2.8): full backup -> restore -> diff over
# the relay, across PostgreSQL majors, plus the source-immutability assertion
# (incl. a run aborted mid-transfer).
#
# For each major: two Docker PostgreSQL containers (source seeded, destination
# empty), then `rust-backup server` + `postgres source` + `postgres destination`
# move the cluster over the relay; finally schema + per-table data checksums are
# compared and the source is proven unchanged.
#
# Usage: bash e2e/postgres_matrix.sh [SOURCE[:DESTINATION] ...] (default: 16)
# Examples: `16` tests 16 -> 16, `12:18` tests a cross-major restore.
# Needs:  docker, cargo. No sudo. Relay-only (--no-udp) for CI determinism.
# Exits non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")/.."
source e2e/lib.sh

CASES=("${@:-16}")
PASS=0
FAIL=0
BIN="./target/release/rust-backup"
PASSWORD="rbpg"
CTRL_PORT=$(rb_free_port)

CONTAINERS=()
PIDS=()
cleanup() {
  local status=$?
  for p in "${PIDS[@]:-}"; do kill "$p" >/dev/null 2>&1 || true; done
  for c in "${CONTAINERS[@]:-}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
  for c in "${CONTAINERS[@]:-}"; do
    if docker container inspect "$c" >/dev/null 2>&1; then
      echo "FAIL: leaked container $c" >&2
      status=1
    fi
  done
  return "$status"
}
trap cleanup EXIT INT TERM

echo "==> building release binary"
cargo build --release --all-features

start_pg() { # name port major -> starts container, waits ready
  local name="$1" port="$2" major="$3"
  # Track the name before `docker run`: Docker can leave a created container
  # behind when host-port programming fails.
  CONTAINERS+=("$name")
  docker run -d --name "$name" -e POSTGRES_PASSWORD="$PASSWORD" \
    -p "${port}:5432" "postgres:${major}-alpine" >/dev/null
  for _ in $(seq 1 30); do
    docker exec "$name" pg_isready -U postgres >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "FAIL: $name not ready"; return 1
}

seed_source() { # container
  docker exec -i "$1" psql -U postgres -v ON_ERROR_STOP=1 <<'SQL'
CREATE ROLE app_owner LOGIN PASSWORD 'x';
CREATE ROLE readers;
GRANT readers TO app_owner;
-- Deliberately differs from the UTF8 template1 in the official image. Restore
-- must select template0 automatically or CREATE DATABASE is rejected before
-- any schema/data is applied.
CREATE DATABASE appdb OWNER app_owner TEMPLATE template0
  ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C';
\connect appdb
CREATE SCHEMA app AUTHORIZATION app_owner;
CREATE TABLE app.accounts (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email text NOT NULL UNIQUE,
  status text NOT NULL DEFAULT 'active'
);
CREATE TABLE app.orders (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  acct bigint NOT NULL REFERENCES app.accounts (id),
  total numeric(12,2) NOT NULL
);
CREATE INDEX orders_acct_idx ON app.orders (acct);
INSERT INTO app.accounts (email) SELECT 'u'||g||'@x' FROM generate_series(1,500) g;
INSERT INTO app.orders (acct, total)
  SELECT (random()*499)::int + 1, (random()*1000)::numeric(12,2) FROM generate_series(1,2000);
SQL
}

# Per-table data checksum (order-independent within a table).
data_checksum() { # container db
  docker exec "$1" psql -U postgres -d "$2" -At -F'|' -c "
    SELECT n.nspname||'.'||c.relname
    FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE c.relkind='r' AND n.nspname='app' ORDER BY 1" | while read -r tbl; do
      sum=$(docker exec "$1" psql -U postgres -d "$2" -At -c \
        "SELECT coalesce(md5(string_agg(md5(t::text), '' ORDER BY t::text)),'') FROM ${tbl} t")
      cnt=$(docker exec "$1" psql -U postgres -d "$2" -At -c "SELECT count(*) FROM ${tbl}")
      echo "${tbl}|${cnt}|${sum}"
    done
}

schema_dump() { # container db
  docker exec "$1" pg_dump -U postgres -d "$2" --schema-only --no-owner --no-privileges \
    | grep -vE '^--|^$|^SET |^SELECT pg_catalog|^\\(un)?restrict |^CREATE EXTENSION IF NOT EXISTS plpgsql |^COMMENT ON EXTENSION plpgsql '
}

# Immutable database properties, with catalog columns normalized across 10..18.
database_metadata() { # container major
  local container="$1" major="$2" locale_columns
  if (( major >= 17 )); then
    locale_columns="d.datlocprovider::text, coalesce(d.datlocale, ''), coalesce(d.daticurules, '')"
  elif (( major >= 16 )); then
    locale_columns="d.datlocprovider::text, coalesce(d.daticulocale, ''), coalesce(d.daticurules, '')"
  elif (( major >= 15 )); then
    locale_columns="d.datlocprovider::text, coalesce(d.daticulocale, ''), ''"
  else
    locale_columns="'c', '', ''"
  fi
  docker exec "$container" psql -U postgres -d postgres -At -F'|' -c \
    "SELECT pg_encoding_to_char(d.encoding), d.datcollate, d.datctype, ${locale_columns}
       FROM pg_database d WHERE d.datname = 'appdb'"
}

run_transfer() { # src_port dst_port channel [abort|overwrite]
  local src_port="$1" dst_port="$2" channel="$3" mode="${4:-}"
  "$BIN" server --bind-addr 127.0.0.1 --control-port "$CTRL_PORT" >/tmp/rb-server.log 2>&1 &
  local server_pid=$!; PIDS+=("$server_pid")
  sleep 1

  # Source registers as provider first, then destination consumes.
  "$BIN" postgres source --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --host 127.0.0.1 --port "$src_port" --user postgres \
    --password "$PASSWORD" --database appdb --sslmode disable >/tmp/rb-src.log 2>&1 &
  local src_pid=$!; PIDS+=("$src_pid")

  if [[ "$mode" == "abort" ]]; then
    sleep 2; kill -9 "$src_pid" >/dev/null 2>&1 || true
    kill "$server_pid" >/dev/null 2>&1 || true
    return 0
  fi

  sleep 2
  local destination_args=(postgres destination --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --yes --admin --host 127.0.0.1 --port "$dst_port" --user postgres \
    --password "$PASSWORD" --sslmode disable)
  if [[ "$mode" == "overwrite" ]]; then
    destination_args+=(--overwrite)
  fi
  "$BIN" "${destination_args[@]}" >/tmp/rb-dst.log 2>&1 &
  local dst_pid=$!; PIDS+=("$dst_pid")

  wait "$src_pid"; local src_rc=$?
  wait "$dst_pid"; local dst_rc=$?
  kill "$server_pid" >/dev/null 2>&1 || true
  [[ $src_rc -eq 0 && $dst_rc -eq 0 ]] && \
    rb_assert_formal_verification /tmp/rb-src.log /tmp/rb-dst.log
}

for CASE in "${CASES[@]}"; do
  if [[ $CASE == *:* ]]; then
    SOURCE_MAJOR=${CASE%%:*}
    DESTINATION_MAJOR=${CASE##*:}
  else
    SOURCE_MAJOR=$CASE
    DESTINATION_MAJOR=$CASE
  fi
  [[ $SOURCE_MAJOR =~ ^[0-9]+$ && $DESTINATION_MAJOR =~ ^[0-9]+$ ]] || {
    echo "FAIL: invalid PostgreSQL case $CASE" >&2; exit 2;
  }
  (( DESTINATION_MAJOR >= SOURCE_MAJOR )) || {
    echo "FAIL: destination PostgreSQL must be >= source ($CASE)" >&2; exit 2;
  }
  CASE_ID="${SOURCE_MAJOR}-to-${DESTINATION_MAJOR}"
  echo "================  PostgreSQL ${SOURCE_MAJOR} -> ${DESTINATION_MAJOR}  ================"
  SRC="rb-pg-src-${CASE_ID}-$$"; DST="rb-pg-dst-${CASE_ID}-$$"
  SRC_PORT=$(rb_free_port)
  DST_PORT=$(rb_free_port)
  while [[ $DST_PORT == "$SRC_PORT" ]]; do DST_PORT=$(rb_free_port); done

  start_pg "$SRC" "$SRC_PORT" "$SOURCE_MAJOR"
  start_pg "$DST" "$DST_PORT" "$DESTINATION_MAJOR"
  seed_source "$SRC"

  # Source fingerprint before any transfer (external immutability baseline).
  fp_before=$(data_checksum "$SRC" appdb)

  # 1) Mid-transfer abort must not mutate the source (T-PG-IMMUT).
  run_transfer "$SRC_PORT" "$DST_PORT" "pgjob-abort-${CASE_ID}" abort || true
  fp_after_abort=$(data_checksum "$SRC" appdb)
  if [[ "$fp_before" == "$fp_after_abort" ]]; then
    echo "PASS: source unchanged after aborted transfer"; PASS=$((PASS+1))
  else
    echo "FAIL: source mutated by aborted transfer"; FAIL=$((FAIL+1))
  fi

  # 2) Full backup -> restore.
  if run_transfer "$SRC_PORT" "$DST_PORT" "pgjob-${CASE_ID}"; then
    echo "PASS: transfer completed with persisted read-back proof"; PASS=$((PASS+1))
  else
    echo "FAIL: transfer failed (see /tmp/rb-*.log)"; FAIL=$((FAIL+1)); continue
  fi

  # 3) Source still unchanged after the full run.
  fp_after=$(data_checksum "$SRC" appdb)
  if [[ "$fp_before" == "$fp_after" ]]; then
    echo "PASS: source unchanged after full transfer"; PASS=$((PASS+1))
  else
    echo "FAIL: source mutated by full transfer"; FAIL=$((FAIL+1))
  fi

  # 4) Destination data matches source 1:1.
  if [[ "$(data_checksum "$SRC" appdb)" == "$(data_checksum "$DST" appdb)" ]]; then
    echo "PASS: destination data matches source"; PASS=$((PASS+1))
  else
    echo "FAIL: destination data differs"; FAIL=$((FAIL+1))
  fi

  # 5) Destination schema matches source.
  if diff <(schema_dump "$SRC" appdb) <(schema_dump "$DST" appdb) >/tmp/rb-schema.diff; then
    echo "PASS: destination schema matches source"; PASS=$((PASS+1))
  else
    echo "FAIL: schema differs (see /tmp/rb-schema.diff)"; FAIL=$((FAIL+1))
  fi

  # 6) Regression: encoding, collation and locale-provider metadata survive a
  # restore even though they differ from destination template1.
  source_db_metadata=$(database_metadata "$SRC" "$SOURCE_MAJOR")
  destination_db_metadata=$(database_metadata "$DST" "$DESTINATION_MAJOR")
  if [[ "$source_db_metadata" == "SQL_ASCII|C|C|c||" && \
        "$source_db_metadata" == "$destination_db_metadata" ]]; then
    echo "PASS: database locale metadata matches source"; PASS=$((PASS+1))
  else
    echo "FAIL: database locale metadata differs: source=${source_db_metadata} destination=${destination_db_metadata}"
    FAIL=$((FAIL+1))
  fi

  # 7) Regression: the typed --overwrite flag must pass preflight, disconnect
  # users, drop the existing database and restore it from scratch.
  docker exec "$DST" psql -U postgres -d appdb -v ON_ERROR_STOP=1 -c \
    "INSERT INTO app.accounts (email) VALUES ('destination-only@x')" >/dev/null
  if run_transfer "$SRC_PORT" "$DST_PORT" "pgjob-overwrite-${CASE_ID}" overwrite && \
     [[ "$(data_checksum "$SRC" appdb)" == "$(data_checksum "$DST" appdb)" ]] && \
     [[ "$(database_metadata "$SRC" "$SOURCE_MAJOR")" == "$(database_metadata "$DST" "$DESTINATION_MAJOR")" ]]; then
    echo "PASS: --overwrite replaces the existing database"; PASS=$((PASS+1))
  else
    echo "FAIL: --overwrite did not replace the existing database"; FAIL=$((FAIL+1))
  fi

  docker rm -f "$SRC" "$DST" >/dev/null 2>&1 || true
done

echo "================================================"
echo "PASS=${PASS}  FAIL=${FAIL}"
[[ $FAIL -eq 0 ]]
