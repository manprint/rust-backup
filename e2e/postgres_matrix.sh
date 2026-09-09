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

seed_source() { # container major
  local container="$1" major="$2"
  docker exec -i "$container" psql -U postgres -v ON_ERROR_STOP=1 <<'SQL'
CREATE ROLE app_owner LOGIN PASSWORD 'x';
CREATE ROLE readers;
GRANT readers TO app_owner;
-- Role/database GUC values travel inside the plan and are applied by the
-- destination administrator through the simple query protocol, which executes
-- every ';'-separated statement in the string. Any unprivileged user can put
-- arbitrary text in their own custom (placeholder) GUC, so the value below must
-- survive as data and never as a second statement. `5min` additionally used to
-- be emitted unquoted, which is a syntax error that aborted the restore.
ALTER ROLE app_owner SET "myapp.k" = '1; ALTER ROLE app_owner SUPERUSER';
ALTER ROLE app_owner SET statement_timeout = '5min';
-- Deliberately differs from the UTF8 template1 in the official image. Restore
-- must select template0 automatically or CREATE DATABASE is rejected before
-- any schema/data is applied.
CREATE DATABASE appdb OWNER app_owner TEMPLATE template0
  ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C';
ALTER DATABASE appdb SET work_mem = '4MB';
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

-- Classic inheritance. The parent and the child both hold rows, and a plain
-- SELECT on the parent expands into the child: a parent streamed without ONLY
-- carries the child's rows too, and the child is its own item, so the
-- destination ends up holding them twice.
CREATE TABLE app.log (id int, msg text);
CREATE TABLE app.log_2025 () INHERITS (app.log);
INSERT INTO app.log SELECT g, 'parent-'||g FROM generate_series(1,10) g;
INSERT INTO app.log_2025 SELECT g, 'child-'||g FROM generate_series(1,90) g;

-- A sequence not owned by any column, a function reading it, and a column
-- default calling that function: the default cannot be inlined in CREATE TABLE
-- unless functions are created first.
CREATE SEQUENCE app.code_seq;
SELECT nextval('app.code_seq');
CREATE FUNCTION app.next_code() RETURNS text LANGUAGE sql
  AS $fn$ SELECT 'c'||nextval('app.code_seq') $fn$;
CREATE TABLE app.doc (id int, code text DEFAULT app.next_code());
INSERT INTO app.doc (id) SELECT g FROM generate_series(1,5) g;

-- A function returning a table's row type: it must be created AFTER that table,
-- which is the opposite dependency direction from the default above.
CREATE FUNCTION app.all_accounts() RETURNS SETOF app.accounts LANGUAGE sql
  AS $fn$ SELECT * FROM app.accounts $fn$;

-- A view reading another view, named so that catalog (name) order is exactly
-- the wrong creation order.
CREATE VIEW app.zombies AS SELECT id, email FROM app.accounts WHERE status <> 'active';
CREATE VIEW app.active AS SELECT * FROM app.zombies;

-- A populated materialized view: its query must run only after the data load.
CREATE MATERIALIZED VIEW app.account_totals AS
  SELECT a.id, count(o.id) AS orders
  FROM app.accounts a LEFT JOIN app.orders o ON o.acct = a.id
  GROUP BY a.id;
CREATE UNIQUE INDEX account_totals_id_idx ON app.account_totals (id);
SQL
  # Partitioned tables exist on 10, but a primary key or an index on the
  # partitioned parent needs 11+.
  if (( major >= 11 )); then
    docker exec -i "$container" psql -U postgres -v ON_ERROR_STOP=1 -d appdb <<'SQL'
-- A partitioned parent whose PK and index cascade to every partition: the
-- partitions' own catalog rows must not be re-emitted (duplicate primary key /
-- relation already exists), and the parent's index stays invalid until each
-- child index is attached to it.
CREATE TABLE app.events (id bigint, ts date NOT NULL, PRIMARY KEY (id, ts))
  PARTITION BY RANGE (ts);
CREATE TABLE app.events_2026 PARTITION OF app.events
  FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE app.events_2027 PARTITION OF app.events
  FOR VALUES FROM ('2027-01-01') TO ('2028-01-01');
CREATE INDEX events_ts_idx ON app.events (ts);
INSERT INTO app.events SELECT g, '2026-03-04'::date FROM generate_series(1,50) g;
INSERT INTO app.events SELECT g, '2027-03-04'::date FROM generate_series(51,120) g;
SQL
  fi
}

# Restored state that the per-table checksums and the schema dump cannot see:
# inheritance scope, materialized view contents, view-on-view, sequence values
# and the validity of a partitioned parent's index.
# `major` is the SOURCE major for both containers: it decides which fixture
# objects exist, so a cross-major case must probe the same set on both sides.
fidelity_probe() { # container db major
  local container="$1" database="$2" major="$3"
  docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
    SELECT 'inherit-only', (SELECT count(*)::text FROM ONLY app.log)
    UNION ALL SELECT 'inherit-all', (SELECT count(*)::text FROM app.log)
    UNION ALL SELECT 'inherit-child', (SELECT count(*)::text FROM app.log_2025)
    UNION ALL SELECT 'matview-rows', (SELECT count(*)::text FROM app.account_totals)
    UNION ALL SELECT 'matview-sum', (SELECT coalesce(sum(orders),0)::text FROM app.account_totals)
    UNION ALL SELECT 'view-on-view', (SELECT count(*)::text FROM app.active)
    UNION ALL SELECT 'doc-codes', (SELECT count(DISTINCT code)::text FROM app.doc)
    ORDER BY 1"
  docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
    SELECT 'sequence:'||sequencename, coalesce(last_value::text, 'never-called')
    FROM pg_sequences WHERE schemaname = 'app' ORDER BY 1"
  docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
    SELECT 'index:'||ic.relname, i.indisvalid::text
    FROM pg_index i
    JOIN pg_class ic ON ic.oid = i.indexrelid
    JOIN pg_namespace n ON n.oid = ic.relnamespace
    WHERE n.nspname = 'app' ORDER BY 1"
  # A view's body is deparsed differently by different majors, so its shape is
  # what a cross-major comparison can assert: the columns it exposes.
  docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
    SELECT 'viewcol:'||c.relname||'.'||a.attname,
           pg_catalog.format_type(a.atttypid, a.atttypmod)
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
    WHERE n.nspname = 'app' AND c.relkind IN ('v', 'm') ORDER BY 1"
  if (( major >= 11 )); then
    docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
      SELECT 'partitioned-rows', count(*)::text FROM app.events"
  fi
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

schema_dump() { # container db [cross-major]
  docker exec "$1" pg_dump -U postgres -d "$2" --schema-only --no-owner --no-privileges \
    | grep -vE '^--|^$|^SET |^SELECT pg_catalog|^\\(un)?restrict |^CREATE EXTENSION IF NOT EXISTS plpgsql |^COMMENT ON EXTENSION plpgsql ' \
    | if [[ "${3:-}" == "cross-major" ]]; then
        strip_view_bodies | float_partition_attach | strip_not_null_names
      else cat; fi
}

# PostgreSQL 18 catalogues every NOT NULL constraint, so pg_dump 18 renders a
# partition's inherited one as `CONSTRAINT events_id_not_null NOT NULL` while
# pg_dump 17 and older write a bare `NOT NULL` for the identical column. The
# names are the server's own defaults (rb-postgres refuses a source that has
# named or NOT VALID not-null constraints), so only the rendering differs.
strip_not_null_names() {
  sed -E 's/ CONSTRAINT [A-Za-z0-9_]+ NOT NULL/ NOT NULL/g'
}

# pg_dump 13 moved `ALTER TABLE ONLY … ATTACH PARTITION` out of the partition's
# own section into a later one, so a 12:16 pair dumps the very same schema with
# those statements in different positions. Their position is not the property
# under test — the restore itself, the partition index validity check and the
# fidelity probe cover attachment — so they are collected and re-emitted,
# sorted, at the end of both dumps.
float_partition_attach() {
  awk '
    /^ALTER TABLE ONLY .* ATTACH PARTITION / { attach[n++] = $0; next }
    { print }
    END {
      for (i = 0; i < n; i++) { print attach[i] | "sort" }
      close("sort")
    }
  '
}

# `pg_get_viewdef` — and hence pg_dump — renders the same view differently
# across majors: PostgreSQL 10 writes `SELECT accounts.id`, 12+ writes
# `SELECT id`. Each side of a cross-major case is dumped by its own pg_dump, so
# the bodies cannot be compared textually; the view columns are compared instead
# (see fidelity_probe), and the header line still proves the view exists.
strip_view_bodies() {
  awk '
    /^CREATE (OR REPLACE )?(MATERIALIZED )?VIEW / {
      print
      if ($0 !~ /;[[:space:]]*$/) { inview = 1 }
      next
    }
    inview { if ($0 ~ /;[[:space:]]*$/) { inview = 0 } ; next }
    { print }
  '
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
  seed_source "$SRC" "$SOURCE_MAJOR"

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
  SCHEMA_MODE=""
  if (( SOURCE_MAJOR != DESTINATION_MAJOR )); then SCHEMA_MODE="cross-major"; fi
  if diff <(schema_dump "$SRC" appdb "$SCHEMA_MODE") \
          <(schema_dump "$DST" appdb "$SCHEMA_MODE") >/tmp/rb-schema.diff; then
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

  # 7) Fidelity of the state that checksums and the schema dump cannot see.
  if diff <(fidelity_probe "$SRC" appdb "$SOURCE_MAJOR") \
          <(fidelity_probe "$DST" appdb "$SOURCE_MAJOR") >/tmp/rb-fidelity.diff; then
    echo "PASS: inheritance scope, matview contents, sequence values and index validity match"
    PASS=$((PASS+1))
  else
    echo "FAIL: restored state differs (see /tmp/rb-fidelity.diff)"; FAIL=$((FAIL+1))
    cat /tmp/rb-fidelity.diff
  fi

  # 8) A partitioned parent's index must be valid, not left ON ONLY.
  if (( SOURCE_MAJOR >= 11 )); then
    valid=$(docker exec "$DST" psql -U postgres -d appdb -At -c \
      "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
        WHERE c.relname = 'events_ts_idx'")
    if [[ "$valid" == "t" ]]; then
      echo "PASS: the partitioned parent's index is valid"; PASS=$((PASS+1))
    else
      echo "FAIL: events_ts_idx on the restored parent is invalid (indisvalid=${valid:-missing})"
      FAIL=$((FAIL+1))
    fi
  fi

  # 9) A read-only source role must not be able to silently produce a plan whose
  # sequence values are all unreadable: `pg_sequences.last_value` is NULL both
  # for "never called" and for "no privilege", and taking the second for the
  # first reset every restored sequence to its start value.
  docker exec -i "$SRC" psql -U postgres -d appdb -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
CREATE ROLE backup_ro LOGIN PASSWORD 'ro';
GRANT CONNECT ON DATABASE appdb TO backup_ro;
GRANT USAGE ON SCHEMA app TO backup_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA app TO backup_ro;
SQL
  plan_ro() {
    "$BIN" plan postgres --host 127.0.0.1 --port "$SRC_PORT"       --user backup_ro --password ro --database appdb --sslmode disable
  }
  if plan_ro >/tmp/rb-plan-ro.log 2>&1; then
    echo "FAIL: a source role without sequence privileges produced a plan anyway"
    FAIL=$((FAIL+1))
  elif grep -q "cannot read the current value of sequence" /tmp/rb-plan-ro.log; then
    echo "PASS: an unreadable sequence value is refused, naming the missing grant"
    PASS=$((PASS+1))
  else
    echo "FAIL: plan failed for the wrong reason (see /tmp/rb-plan-ro.log)"; FAIL=$((FAIL+1))
    tail -5 /tmp/rb-plan-ro.log
  fi
  docker exec "$SRC" psql -U postgres -d appdb -v ON_ERROR_STOP=1 >/dev/null -c \
    "GRANT SELECT ON ALL SEQUENCES IN SCHEMA app TO backup_ro"
  if plan_ro >/tmp/rb-plan-ro2.log 2>&1; then
    echo "PASS: the same role plans successfully once sequences are readable"
    PASS=$((PASS+1))
  else
    echo "FAIL: a read-only role with sequence SELECT still cannot plan"; FAIL=$((FAIL+1))
    tail -5 /tmp/rb-plan-ro2.log
  fi

  # 10) Role and database settings must round-trip as data, never as SQL.
  role_settings() { # container
    docker exec "$1" psql -U postgres -At -F'|' -c \
      "SELECT rolname, rolsuper::text, array_to_string(rolconfig, '@@')
         FROM pg_roles WHERE rolname = 'app_owner'"
    docker exec "$1" psql -U postgres -At -c \
      "SELECT array_to_string(s.setconfig, '@@')
         FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase
        WHERE d.datname = 'appdb' AND s.setrole = 0"
  }
  if [[ "$(role_settings "$SRC")" == "$(role_settings "$DST")" ]] && \
     [[ "$(docker exec "$DST" psql -U postgres -At -c \
            "SELECT rolsuper FROM pg_roles WHERE rolname='app_owner'")" == "f" ]]; then
    echo "PASS: role/database settings round-trip without executing as SQL"; PASS=$((PASS+1))
  else
    echo "FAIL: role/database settings differ or were executed"; FAIL=$((FAIL+1))
    diff <(role_settings "$SRC") <(role_settings "$DST") || true
  fi

  # 11) Regression: the typed --overwrite flag must pass preflight, disconnect
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
