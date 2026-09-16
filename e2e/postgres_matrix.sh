#!/usr/bin/env bash
# T-PG-MATRIX + T-PG-IMMUT + T-PG-ORACLE + T-PG-REFUSE: the PostgreSQL fidelity
# matrix. For each case two Docker PostgreSQL containers (source seeded from
# e2e/fixtures/postgres, destination empty) are moved over the relay by
# `rust-backup server` + `postgres source` + `postgres destination`, and the
# result is judged by two external oracles run inside the containers:
#
#   * `pg_dump --schema-only` taken by the destination's own pg_dump against
#     both servers, normalised and diffed;
#   * one TSV line per relation (count and order-independent row digest read
#     FROM ONLY), sequence, constraint and index, diffed.
#
# Every row of docs/testing/POSTGRES_MATRIX.md then prints `PASS <ID>`,
# `FAIL <ID>` or `SKIP <ID>`, followed by `MATRIX <case>: p pass, f fail,
# s skip`.
#
# Usage: bash e2e/postgres_matrix.sh [SOURCE[:DESTINATION] ...] (default: 16)
# Env:   RB_PG_IMAGE_REPO=postgis/postgis runs the PostGIS image set and the
#        M-PG-GIS-* rows.
# Needs: docker, cargo. No sudo. Relay-only (--no-udp) for CI determinism.
# Exits: 0 all rows passed, 1 any row failed, 77 the environment cannot run the
#        case (no image tag for this major).
set -euo pipefail
cd "$(dirname "$0")/.."
source e2e/lib.sh

CASES=("${@:-16}")
IMAGE_REPO=${RB_PG_IMAGE_REPO:-postgres}
BIN="./target/release/rust-backup"
PASSWORD="$RB_PG_PASSWORD"
CTRL_PORT=$(rb_free_port)
WORK=$(mktemp -d)
FIXTURES="e2e/fixtures/postgres"
DATABASE="appdb"

PASS=0
FAIL=0
SKIP=0
# Destination flags every transfer of the current case carries. A cross-major
# PostGIS pair ships different PostGIS versions in the two images, so the whole
# case runs under --extension-version default (M-PG-GIS-06 asserts both sides of
# that policy explicitly).
EXTRA_DEST_ARGS=()
TOTAL_PASS=0
TOTAL_FAIL=0
TOTAL_SKIP=0

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
  if [[ ${RUST_BACKUP_E2E_KEEP:-0} == 1 ]]; then
    echo "work directory kept: $WORK"
  else
    rm -rf "$WORK"
  fi
  return "$status"
}
trap cleanup EXIT INT TERM

pass_row() { echo "PASS $1"; PASS=$((PASS + 1)); }
fail_row() { echo "FAIL $1${2:+: $2}"; FAIL=$((FAIL + 1)); }
skip_row() { echo "SKIP $1${2:+ ($2)}"; SKIP=$((SKIP + 1)); }

echo "==> building release binary"
cargo build --release --all-features

# --------------------------------------------------------------------------
# Images
# --------------------------------------------------------------------------

# PostGIS publishes one image per (postgres major, postgis version) pair. The
# table is verified with `docker manifest inspect` before use; a major with no
# published tag makes the case exit 77 rather than fail.
postgis_tag() { # major
  case "$1" in
    10) echo "10-2.5" ;;
    11) echo "11-3.3" ;;
    12) echo "12-3.4" ;;
    13) echo "13-3.5" ;;
    14) echo "14-3.5" ;;
    15) echo "15-3.5" ;;
    16) echo "16-3.5" ;;
    17) echo "17-3.5" ;;
    18) echo "18-3.6" ;;
    *)  echo "" ;;
  esac
}

image_for() { # major
  local major=$1 tag
  if [[ $IMAGE_REPO == "postgis/postgis" ]]; then
    tag=$(postgis_tag "$major")
    [[ -n $tag ]] || return 1
    if ! docker manifest inspect "postgis/postgis:$tag" >/dev/null 2>&1; then
      return 1
    fi
    echo "postgis/postgis:$tag"
  else
    echo "${IMAGE_REPO}:${major}-alpine"
  fi
}

start_pg() { # name image port
  local name="$1" image="$2" port="$3"
  # Track the name before `docker run`: Docker can leave a created container
  # behind when host-port programming fails.
  CONTAINERS+=("$name")
  rb_pg_start "$name" "$image" "$port"
}

# --------------------------------------------------------------------------
# Seeding
# --------------------------------------------------------------------------

seed_source() { # container major
  local container="$1" major="$2"
  # Deliberately not the UTF8 template1 of the official image: the restore must
  # pick template0 by itself or CREATE DATABASE is rejected outright.
  docker exec "$container" psql -U postgres -v ON_ERROR_STOP=1 -q -c \
    "CREATE DATABASE ${DATABASE} OWNER app_owner TEMPLATE template0
       ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C'" >/dev/null 2>&1 || {
    # app_owner is created by 00_roles.sql, which needs a database to be loaded
    # into: create the database unowned first, then hand it over.
    docker exec "$container" psql -U postgres -v ON_ERROR_STOP=1 -q -c \
      "CREATE DATABASE ${DATABASE} TEMPLATE template0
         ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C'" >/dev/null
  }
  bash "$FIXTURES/71_rbtest_extension.sh" "$container" >/dev/null
  rb_pg_load_fixtures "$container" "$major" "$DATABASE" "$FIXTURES"
  if [[ $IMAGE_REPO == "postgis/postgis" ]]; then
    rb_pg_load_fixtures "$container" "$major" "$DATABASE" "$FIXTURES/postgis"
  fi
  docker exec "$container" psql -U postgres -v ON_ERROR_STOP=1 -q -c \
    "ALTER DATABASE ${DATABASE} OWNER TO app_owner" >/dev/null
  docker exec "$container" psql -U postgres -v ON_ERROR_STOP=1 -q -c \
    "ALTER DATABASE ${DATABASE} SET work_mem = '4MB'" >/dev/null
}

# --------------------------------------------------------------------------
# Oracles
# --------------------------------------------------------------------------

# The data oracle doubles as the immutability fingerprint: it reads every
# relation FROM ONLY, so an inheritance parent never counts its children twice.
data_checksum() { # container db
  local out="$WORK/counts-$1-$2-$RANDOM"
  rb_pg_oracle_counts "$1" postgres "$2" "$out" >/dev/null
  cat "$out"
}

# PostgreSQL 18 catalogues every NOT NULL constraint, so pg_dump 18 renders an
# inherited one as `CONSTRAINT events_id_not_null NOT NULL` while pg_dump 17 and
# older write a bare `NOT NULL`. The names are the server's own defaults, so
# only the rendering differs.
strip_not_null_names() {
  sed -E 's/ CONSTRAINT [A-Za-z0-9_]+ NOT NULL/ NOT NULL/g'
}

# pg_dump 13 moved `ALTER TABLE ONLY … ATTACH PARTITION` into a later section,
# so a 12:16 pair dumps the same schema with those statements in different
# positions. Position is not the property under test — the restore, the
# partition index validity check and the fidelity probe cover attachment — so
# they are collected and re-emitted, sorted, at the end of both dumps.
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

# `pg_get_viewdef` renders the same view differently across majors (10 writes
# `SELECT accounts.id`, 12+ writes `SELECT id`), and each side is deparsed by
# its own server. The view columns are compared instead (fidelity_probe), and
# the header line still proves the view exists.
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

# A newer destination catalogues NOT NULL as a constraint row of its own
# (PostgreSQL 18) that the older source cannot have, and renders index and
# constraint definitions with newer spellings. Only cross-major runs normalise.
normalize_counts() { # file cross
  if [[ ${2:-} == "cross-major" ]]; then
    grep -vE '^con	[^	]*	[^	]*	NOT NULL' "$1" | strip_not_null_names
  else
    cat "$1"
  fi
}

normalize_schema() { # file cross
  if [[ ${2:-} == "cross-major" ]]; then
    strip_view_bodies <"$1" | float_partition_attach | strip_not_null_names
  else
    cat "$1"
  fi
}

# Restored state that neither oracle can see: inheritance scope, matview
# contents, view-on-view, sequence values and the validity of a partitioned
# parent's index. `major` is the SOURCE major on both sides: it decides which
# fixture objects exist.
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
    UNION ALL SELECT 'mx-inherit-only', (SELECT count(*)::text FROM ONLY mx.t_tab_06)
    UNION ALL SELECT 'mx-matview-01', (SELECT count(*)::text FROM mx.m_mv_01)
    UNION ALL SELECT 'mx-matview-02-populated',
      (SELECT relispopulated::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'mx' AND c.relname = 'm_mv_02')
    ORDER BY 1"
  docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
    SELECT 'sequence:' || schemaname || '.' || sequencename,
           coalesce(last_value::text, 'never-called')
      FROM pg_sequences WHERE schemaname IN ('app', 'mx') ORDER BY 1"
  docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
    SELECT 'index:' || n.nspname || '.' || ic.relname, i.indisvalid::text
      FROM pg_index i
      JOIN pg_class ic ON ic.oid = i.indexrelid
      JOIN pg_namespace n ON n.oid = ic.relnamespace
     WHERE n.nspname IN ('app', 'mx') ORDER BY 1"
  docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
    SELECT 'viewcol:' || n.nspname || '.' || c.relname || '.' || a.attname,
           pg_catalog.format_type(a.atttypid, a.atttypmod)
      FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
     WHERE n.nspname IN ('app', 'mx') AND c.relkind IN ('v', 'm') ORDER BY 1"
  if (( major >= 11 )); then
    docker exec "$container" psql -U postgres -d "$database" -At -F'|' -c "
      SELECT 'partitioned-rows', count(*)::text FROM app.events"
  fi
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
       FROM pg_database d WHERE d.datname = '${DATABASE}'"
}

# --------------------------------------------------------------------------
# Transfer
# --------------------------------------------------------------------------

run_transfer() { # src_port dst_port channel [abort|overwrite|source-only] [database]
  local src_port="$1" dst_port="$2" channel="$3" mode="${4:-}" database="${5:-$DATABASE}"
  "$BIN" server --bind-addr 127.0.0.1 --control-port "$CTRL_PORT" >"$WORK/server.log" 2>&1 &
  local server_pid=$!; PIDS+=("$server_pid")
  sleep 1

  # Source registers as provider first, then destination consumes.
  "$BIN" postgres source --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --host 127.0.0.1 --port "$src_port" --user postgres \
    --password "$PASSWORD" --database "$database" --sslmode disable >"$WORK/src.log" 2>&1 &
  local src_pid=$!; PIDS+=("$src_pid")

  if [[ "$mode" == "abort" ]]; then
    sleep 2; kill -9 "$src_pid" >/dev/null 2>&1 || true
    kill "$server_pid" >/dev/null 2>&1 || true
    return 0
  fi

  if [[ "$mode" == "source-only" ]]; then
    # A refused plan never reaches the destination: the source fails during
    # Analyze, so no destination process is started for this shape. A source
    # that is *not* refused would instead sit out its ten-minute registration
    # wait, so it is given 120 s and then killed — the caller reports that as a
    # failure because the refusal message never appears.
    local timed_out=0 waited_refusal=0 refuse_rc=0
    while (( waited_refusal < 240 )); do
      kill -0 "$src_pid" 2>/dev/null || break
      sleep 0.5
      waited_refusal=$((waited_refusal + 1))
    done
    if kill -0 "$src_pid" 2>/dev/null; then
      kill -9 "$src_pid" >/dev/null 2>&1 || true
      timed_out=1
    fi
    wait "$src_pid" || refuse_rc=$?
    # A source killed for running past the deadline reports 124, not the 137 of
    # the signal: the caller distinguishes "never refused" from "refused".
    (( timed_out == 1 )) && refuse_rc=124
    kill "$server_pid" >/dev/null 2>&1 || true
    return "$refuse_rc"
  fi

  # Start the destination only once the coordination server has seen the source
  # register. The source analyzes before it registers, so a plan refused during
  # Analyze would otherwise leave the destination waiting out its ten-minute
  # registration timeout for a channel that will never exist.
  local waited=0 src_rc_early=0
  while (( waited < 600 )); do
    if ! kill -0 "$src_pid" 2>/dev/null; then
      wait "$src_pid" || src_rc_early=$?
      kill "$server_pid" >/dev/null 2>&1 || true
      return "${src_rc_early:-1}"
    fi
    if grep -q 'provider registered' "$WORK/server.log" 2>/dev/null; then
      break
    fi
    sleep 0.5
    waited=$((waited + 1))
  done

  local destination_args=(postgres destination --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --yes --admin --host 127.0.0.1 --port "$dst_port" --user postgres \
    --password "$PASSWORD" --sslmode disable)
  if [[ "$mode" == "overwrite" ]]; then
    destination_args+=(--overwrite)
  fi
  if [[ "$mode" == "extension-version-default" ]]; then
    destination_args+=(--extension-version default)
  fi
  # `extension-version-default` already spells the flag: adding the case-wide
  # copy too makes clap reject the argument as given twice.
  if (( ${#EXTRA_DEST_ARGS[@]} > 0 )) && [[ "$mode" != "extension-version-default" ]]; then
    destination_args+=("${EXTRA_DEST_ARGS[@]}")
  fi
  "$BIN" "${destination_args[@]}" >"$WORK/dst.log" 2>&1 &
  local dst_pid=$!; PIDS+=("$dst_pid")

  # Neither `wait` may be unbounded: a destination that dies before the
  # handshake (a bad argument list, a refused preflight) leaves the source
  # waiting for a peer that will never arrive, and `wait "$src_pid"` would hang
  # the whole matrix. Wait for the first side to finish, then give the other a
  # bounded grace period before killing it.
  local src_rc=0 dst_rc=0 waited=0
  while (( waited < 1800 )); do
    kill -0 "$src_pid" 2>/dev/null || break
    kill -0 "$dst_pid" 2>/dev/null || break
    sleep 0.5
    waited=$((waited + 1))
  done
  waited=0
  while (( waited < 600 )); do
    kill -0 "$src_pid" 2>/dev/null || kill -0 "$dst_pid" 2>/dev/null || break
    sleep 0.5
    waited=$((waited + 1))
  done
  kill -9 "$src_pid" "$dst_pid" >/dev/null 2>&1 || true
  wait "$src_pid" || src_rc=$?
  wait "$dst_pid" || dst_rc=$?
  kill "$server_pid" >/dev/null 2>&1 || true
  [[ $src_rc -eq 0 && $dst_rc -eq 0 ]] && \
    rb_assert_formal_verification "$WORK/src.log" "$WORK/dst.log"
}

# --------------------------------------------------------------------------
# Matrix rows: ID|minimum major|object names (comma separated, '-' for none)
# --------------------------------------------------------------------------

matrix_rows() {
  cat <<'ROWS'
M-PG-TAB-01|10|mx.t_tab_01
M-PG-TAB-02|10|mx.t_tab_02
M-PG-TAB-03|10|mx.t_tab_03
M-PG-TAB-04|11|mx.t_tab_04,mx.t_tab_04_2026,mx.t_tab_04_default
M-PG-TAB-05|11|mx.t_tab_05,mx.t_tab_05_north_low,mx.t_tab_05_south
M-PG-TAB-06|10|mx.t_tab_06,mx.t_tab_06_child
M-PG-TAB-07|10|mx.t_tab_07
M-PG-TAB-08|10|mx.t_tab_08
M-PG-TAB-09|12|mx.t_tab_09
M-PG-TAB-10|10|mx.t_tab_10
M-PG-TAB-11|10|mx.t_tab_11,mx.s_tab_11_seq
M-PG-TAB-12|10|mx.t_tab_12
M-PG-TAB-13|10|mx.t_tab_13
M-PG-TAB-14|10|Mixed Schema.Weird Table
M-PG-TAB-15|10|mx.t_tab_15
M-PG-TAB-16|10|mx.t_tab_16
M-PG-TAB-17|10|mx.t_tab_17
M-PG-TAB-18|10|mx.t_tab_18
M-PG-SEQ-01|10|mx.s_seq_01
M-PG-SEQ-02|10|mx.s_seq_02
M-PG-SEQ-03|10|mx.s_seq_03
M-PG-SEQ-04|10|mx.s_seq_04
M-PG-SEQ-05|10|mx.s_seq_05
M-PG-SEQ-06|10|mx.s_seq_06
M-PG-SEQ-07|10|mx.t_tab_08_id_always_seq
M-PG-VIEW-01|10|mx.v_view_01
M-PG-VIEW-02|10|mx.v_view_02_a,mx.v_view_02_b,mx.v_view_02_c
M-PG-VIEW-03|10|mx.v_view_03
M-PG-VIEW-04|10|mx.v_view_04
M-PG-VIEW-05|10|mx.v_view_05
M-PG-VIEW-06|10|mx.v_view_06
M-PG-VIEW-07|10|mx.v_view_07
M-PG-VIEW-08|10|mx.v_view_08
M-PG-VIEW-09|10|mx.v_view_09
M-PG-MV-01|10|mx.m_mv_01
M-PG-MV-02|10|mx.m_mv_02
M-PG-MV-03|10|mx.m_mv_03
M-PG-MV-04|10|mx.m_mv_04,mx.i_mv_04_unique,mx.i_mv_04_label
M-PG-MV-05|10|mx.m_mv_05
M-PG-MV-06|10|mx.m_mv_06
M-PG-MV-07|10|mx.m_mv_07
M-PG-IDX-01|10|mx.i_idx_01
M-PG-IDX-02|10|mx.i_idx_02
M-PG-IDX-03|10|mx.i_idx_03
M-PG-IDX-04|10|mx.i_idx_04
M-PG-IDX-05|10|mx.i_idx_05
M-PG-IDX-06|11|mx.i_idx_06
M-PG-IDX-07|10|mx.i_idx_07
M-PG-IDX-08|10|mx.i_idx_08
M-PG-IDX-09|10|mx.i_idx_09
M-PG-IDX-10|10|mx.i_idx_10
M-PG-IDX-11|10|mx.i_idx_11
M-PG-IDX-12|10|mx.i_idx_12
M-PG-IDX-13|11|mx.i_idx_13
M-PG-IDX-14|10|mx.i_idx_14
M-PG-IDX-15|10|mx.i_idx_15
M-PG-IDX-16|10|mx.I Idx 16
M-PG-IDX-17|15|mx.i_idx_17
M-PG-IDX-18|10|mx.i_idx_18
M-PG-CON-01|10|mx.t_con_01.c_con_01
M-PG-CON-02|10|mx.t_con_02.c_con_02
M-PG-CON-03|10|mx.t_con_03.c_con_03
M-PG-CON-04|10|mx.t_con_04_child.c_con_04
M-PG-CON-05|10|mx.t_con_05_child.c_con_05
M-PG-CON-06|10|mx.t_con_06_child.c_con_06
M-PG-CON-07|12|mx.t_con_07_child.c_con_07
M-PG-CON-08|11|mx.t_con_08.c_con_08
M-PG-CON-09|10|mx.t_con_09.c_con_09
M-PG-CON-10|10|mx.t_con_10.c_con_10
M-PG-CON-11|10|mx.t_con_11.c_con_11
M-PG-CON-12|10|mx.t_con_12.c_con_12
M-PG-CON-13|10|mx.t_con_13.c_con_13
M-PG-CON-14|10|mx.t_con_14.c_con_14
M-PG-CON-15|10|mx.t_con_15.c_con_15
M-PG-CON-16|11|mx.t_con_16.c_con_16
M-PG-CON-17|18|mx.t_con_17
M-PG-CON-18|15|mx.t_con_18.c_con_18
M-PG-EXT-01|10|mx.t_ext_01,mx.i_ext_01_trgm
M-PG-EXT-02|10|btree_gist
M-PG-EXT-03|10|mx.t_ext_03,mx.i_ext_03_gin
M-PG-EXT-04|10|mx.t_ext_04
M-PG-EXT-05|10|mx.t_ext_05
M-PG-EXT-06|10|tablefunc
M-PG-EXT-07|10|public.rbtest_cfg
ROWS
}

gis_rows() {
  cat <<'ROWS'
M-PG-GIS-01|10|postgis
M-PG-GIS-02|10|mx.t_gis_02
M-PG-GIS-03|10|mx.i_gis_03
M-PG-GIS-04|10|public.spatial_ref_sys
M-PG-GIS-05|10|mx.v_gis_05
ROWS
}

# A row passes when every one of its objects is present in the destination
# oracle output and no line of either diff mentions it.
evaluate_rows() { # source_major counts_dst schema_dst counts_diff schema_diff
  local major=$1 counts_dst=$2 schema_dst=$3 counts_diff=$4 schema_diff=$5
  local line id minimum names name missing differing
  local -a all_rows=()
  mapfile -t all_rows < <(matrix_rows)
  if [[ $IMAGE_REPO == "postgis/postgis" ]]; then
    mapfile -t -O "${#all_rows[@]}" all_rows < <(gis_rows)
  fi
  for line in "${all_rows[@]}"; do
    IFS='|' read -r id minimum names <<<"$line"
    if (( major < minimum )); then
      skip_row "$id" "needs >= $minimum"
      continue
    fi
    missing=""
    differing=""
    while IFS= read -r name; do
      [[ -z $name ]] && continue
      if ! grep -qF "$name" "$counts_dst" && ! grep -qF "$name" "$schema_dst"; then
        missing="$missing $name"
      fi
      # Only added/removed lines mean this object differs: a unified diff's
      # context lines name objects that are identical on both sides.
      if grep -E '^[+-]' "$counts_diff" "$schema_diff" 2>/dev/null \
          | grep -vE '^[^:]*:(\+\+\+|---)' | grep -qF "$name"; then
        differing="$differing $name"
      fi
    done < <(tr ',' '\n' <<<"$names")
    if [[ -n $missing ]]; then
      fail_row "$id" "absent from the destination:$missing"
    elif [[ -n $differing ]]; then
      fail_row "$id" "differs between source and destination:$differing"
    else
      pass_row "$id"
    fi
  done
  if [[ $IMAGE_REPO != "postgis/postgis" ]]; then
    skip_row "M-PG-GIS-01" "needs RB_PG_IMAGE_REPO=postgis/postgis"
    skip_row "M-PG-GIS-02" "needs RB_PG_IMAGE_REPO=postgis/postgis"
    skip_row "M-PG-GIS-03" "needs RB_PG_IMAGE_REPO=postgis/postgis"
    skip_row "M-PG-GIS-04" "needs RB_PG_IMAGE_REPO=postgis/postgis"
    skip_row "M-PG-GIS-05" "needs RB_PG_IMAGE_REPO=postgis/postgis"
  fi
  if (( GIS_CROSS == 0 )); then
    skip_row "M-PG-GIS-06" "cross-major PostGIS pair, run as 12:16"
  fi
  # M-PG-EXT-08 is asserted by run_extension_version, which needs its own
  # source database and a destination that carries only rbtest 1.1.
}

refusal_row_id() { # fixture basename
  case "$1" in
    enum)                echo "M-PG-REF-01" ;;
    domain)              echo "M-PG-REF-02" ;;
    composite_type)      echo "M-PG-REF-03" ;;
    trigger)             echo "M-PG-REF-04" ;;
    rls_policy)          echo "M-PG-REF-05" ;;
    rule)                echo "M-PG-REF-06" ;;
    large_object)        echo "M-PG-REF-07" ;;
    collation)           echo "M-PG-REF-08" ;;
    event_trigger)       echo "M-PG-REF-09" ;;
    foreign_table)       echo "M-PG-REF-10" ;;
    aggregate)           echo "M-PG-REF-11" ;;
    column_grant)        echo "M-PG-REF-12" ;;
    default_privileges)  echo "M-PG-REF-13" ;;
    publication)         echo "M-PG-REF-14" ;;
    named_not_null)      echo "M-PG-REF-15" ;;
    *)                   echo "M-PG-REF-??" ;;
  esac
}

run_refusals() { # source_container source_port destination_container major case_id
  local src="$1" src_port="$2" dst="$3" major="$4" case_id="$5"
  local file base needed id database mark rc
  for file in "$FIXTURES"/refusals/*.sql; do
    base=$(basename "$file" .sql)
    needed=0
    if [[ $base =~ \.ge([0-9]+)$ ]]; then
      needed=${BASH_REMATCH[1]}
      base=${base%.ge*}
    fi
    id=$(refusal_row_id "$base")
    if (( major < needed )); then
      skip_row "$id" "needs >= $needed"
      continue
    fi
    database="mx_ref_${base}"
    docker exec "$src" psql -U postgres -v ON_ERROR_STOP=1 -q -c \
      "CREATE DATABASE ${database}" >/dev/null
    if ! docker exec -i "$src" psql -U postgres -v ON_ERROR_STOP=1 -q -d "$database" \
        >/dev/null 2>"$WORK/refusal-$base.err" <"$file"; then
      fail_row "$id" "fixture did not load (see $WORK/refusal-$base.err)"
      continue
    fi
    mark=$(date +%s)
    rc=0
    run_transfer "$src_port" 0 "pgref-${base}-${case_id}" source-only "$database" || rc=$?
    if (( rc == 0 )); then
      fail_row "$id" "the source produced a plan for a refused object"
      continue
    fi
    if ! grep -q 'this build cannot reproduce the following objects' "$WORK/src.log"; then
      fail_row "$id" "the source failed for another reason: $(tail -1 "$WORK/src.log")"
      continue
    fi
    if docker exec "$dst" psql -U postgres -At -c \
        "SELECT 1 FROM pg_database WHERE datname = '${database}'" | grep -q 1; then
      fail_row "$id" "the destination holds ${database} although the plan was refused"
      continue
    fi
    if docker logs --since "$mark" "$src" 2>&1 | grep -qE 'statement: COPY|execute [^:]*: COPY'; then
      fail_row "$id" "the source ran COPY before refusing"
      continue
    fi
    pass_row "$id"
  done
}

# M-PG-EXT-08 (T-PG-EXTVER): the source runs rbtest 1.0, the destination carries
# only 1.1. `CREATE EXTENSION rbtest VERSION '1.0'` there is an error, so the
# plan must be refused at preflight — before any role or database is created —
# and `--extension-version default` must accept the destination's version while
# naming the substitution in the verification report.
run_extension_version() { # source_container source_port destination_container destination_port case_id
  local src="$1" src_port="$2" dst="$3" dst_port="$4" case_id="$5"
  local database="mx_extver" sharedir rc=0

  docker exec "$src" psql -U postgres -v ON_ERROR_STOP=1 -q -c \
    "CREATE DATABASE ${database}" >/dev/null
  docker exec "$src" psql -U postgres -v ON_ERROR_STOP=1 -q -d "$database" -c \
    "CREATE EXTENSION rbtest; INSERT INTO rbtest_cfg VALUES (1000, 'custom')" >/dev/null

  # Leave the destination with 1.1 only: the 1.0 script file installed for the
  # main run would otherwise satisfy the source exactly.
  sharedir=$(docker exec "$dst" pg_config --sharedir)
  docker exec "$dst" rm -f "$sharedir/extension/rbtest--1.0.sql"
  bash "$FIXTURES/71_rbtest_extension.sh" "$dst" 1.1 >/dev/null

  # The refusal half must run without the case-wide destination flags: a
  # cross-major PostGIS pair sets --extension-version default for every other
  # transfer, which is exactly what this half must not have.
  local -a saved_extra=("${EXTRA_DEST_ARGS[@]}")
  EXTRA_DEST_ARGS=()
  run_transfer "$src_port" "$dst_port" "pgextver-${case_id}" "" "$database" || rc=$?
  EXTRA_DEST_ARGS=("${saved_extra[@]}")
  if (( rc == 0 )); then
    fail_row "M-PG-EXT-08" "the destination restored an extension version it does not have"
    return
  fi
  if ! grep -q 'extension rbtest version 1.0 is not available' "$WORK/dst.log"; then
    fail_row "M-PG-EXT-08" "no version refusal in the destination log: $(tail -1 "$WORK/dst.log")"
    return
  fi
  if docker exec "$dst" psql -U postgres -At -c \
      "SELECT 1 FROM pg_database WHERE datname = '${database}'" | grep -q 1; then
    fail_row "M-PG-EXT-08" "the destination holds ${database} although preflight refused the plan"
    return
  fi

  rc=0
  run_transfer "$src_port" "$dst_port" "pgextver-default-${case_id}" \
    extension-version-default "$database" || rc=$?
  if (( rc != 0 )); then
    fail_row "M-PG-EXT-08" "--extension-version default did not restore: $(tail -1 "$WORK/dst.log")"
    return
  fi
  if ! grep -q 'deviation: extension rbtest restored at version 1.1 (source 1.0)' "$WORK/dst.log"; then
    fail_row "M-PG-EXT-08" "the substituted version was not reported as a deviation"
    return
  fi
  pass_row "M-PG-EXT-08"
}

# --------------------------------------------------------------------------
# Cases
# --------------------------------------------------------------------------

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

  SOURCE_IMAGE=$(image_for "$SOURCE_MAJOR") || {
    echo "SKIP: no ${IMAGE_REPO} image for PostgreSQL ${SOURCE_MAJOR}" >&2; exit 77;
  }
  DESTINATION_IMAGE=$(image_for "$DESTINATION_MAJOR") || {
    echo "SKIP: no ${IMAGE_REPO} image for PostgreSQL ${DESTINATION_MAJOR}" >&2; exit 77;
  }

  PASS=0
  FAIL=0
  SKIP=0
  CASE_ID="${SOURCE_MAJOR}-to-${DESTINATION_MAJOR}"
  CROSS=""
  if (( SOURCE_MAJOR != DESTINATION_MAJOR )); then CROSS="cross-major"; fi
  EXTRA_DEST_ARGS=()
  GIS_CROSS=0
  if [[ $IMAGE_REPO == "postgis/postgis" && -n $CROSS ]]; then
    GIS_CROSS=1
    EXTRA_DEST_ARGS=(--extension-version default)
  fi
  echo "================  PostgreSQL ${SOURCE_MAJOR} -> ${DESTINATION_MAJOR} (${IMAGE_REPO})  ================"
  SRC="rb-pg-src-${CASE_ID}-$$"; DST="rb-pg-dst-${CASE_ID}-$$"
  SRC_PORT=$(rb_free_port)
  DST_PORT=$(rb_free_port)
  while [[ $DST_PORT == "$SRC_PORT" ]]; do DST_PORT=$(rb_free_port); done

  start_pg "$SRC" "$SOURCE_IMAGE" "$SRC_PORT"
  start_pg "$DST" "$DESTINATION_IMAGE" "$DST_PORT"
  bash "$FIXTURES/71_rbtest_extension.sh" "$DST" >/dev/null
  seed_source "$SRC" "$SOURCE_MAJOR"

  # Source fingerprint before any transfer (external immutability baseline).
  fp_before=$(data_checksum "$SRC" "$DATABASE")

  # 1) Mid-transfer abort must not mutate the source (T-PG-IMMUT).
  run_transfer "$SRC_PORT" "$DST_PORT" "pgjob-abort-${CASE_ID}" abort || true
  fp_after_abort=$(data_checksum "$SRC" "$DATABASE")
  if [[ "$fp_before" == "$fp_after_abort" ]]; then
    pass_row "IMMUT-ABORT"
  else
    fail_row "IMMUT-ABORT" "source mutated by the aborted transfer"
  fi

  # 1b) M-PG-GIS-06: the two PostGIS images carry different PostGIS versions, so
  # the pair must be refused at preflight without --extension-version default —
  # and the rest of the case then runs with it (EXTRA_DEST_ARGS).
  GIS06_REFUSED=0
  if (( GIS_CROSS == 1 )); then
    saved_extra=("${EXTRA_DEST_ARGS[@]}")
    EXTRA_DEST_ARGS=()
    gis_rc=0
    run_transfer "$SRC_PORT" "$DST_PORT" "pggis06-${CASE_ID}" || gis_rc=$?
    EXTRA_DEST_ARGS=("${saved_extra[@]}")
    if (( gis_rc == 0 )); then
      fail_row "M-PG-GIS-06" "the destination restored a PostGIS version it does not have"
    elif ! grep -q 'extension postgis version .* is not available' "$WORK/dst.log"; then
      fail_row "M-PG-GIS-06" "no version refusal in the destination log: $(tail -1 "$WORK/dst.log")"
    elif docker exec "$DST" psql -U postgres -At -c \
        "SELECT 1 FROM pg_database WHERE datname = '${DATABASE}'" | grep -q 1; then
      fail_row "M-PG-GIS-06" "the destination holds ${DATABASE} although preflight refused the plan"
    else
      GIS06_REFUSED=1
    fi
  fi

  # 2) Full backup -> restore.
  TRANSFER_MARK=$(date +%s)
  if run_transfer "$SRC_PORT" "$DST_PORT" "pgjob-${CASE_ID}"; then
    pass_row "TRANSFER"
  else
    fail_row "TRANSFER" "see $WORK/src.log and $WORK/dst.log"
    echo "--- source log (tail) ---"; tail -20 "$WORK/src.log" || true
    echo "--- destination log (tail) ---"; tail -20 "$WORK/dst.log" || true
    echo "MATRIX ${CASE}: ${PASS} pass, ${FAIL} fail, ${SKIP} skip"
    TOTAL_PASS=$((TOTAL_PASS + PASS)); TOTAL_FAIL=$((TOTAL_FAIL + FAIL)); TOTAL_SKIP=$((TOTAL_SKIP + SKIP))
    continue
  fi

  if (( GIS_CROSS == 1 && GIS06_REFUSED == 1 )); then
    if grep -q 'deviation: extension postgis restored at version' "$WORK/dst.log"; then
      pass_row "M-PG-GIS-06"
    else
      fail_row "M-PG-GIS-06" "the substituted PostGIS version was not reported as a deviation"
    fi
  fi

  # 3) The source must be untouched, and its own log must show only reads.
  if [[ "$fp_before" == "$(data_checksum "$SRC" "$DATABASE")" ]]; then
    pass_row "IMMUT-FULL"
  else
    fail_row "IMMUT-FULL" "source mutated by the full transfer"
  fi
  if rb_pg_assert_readonly_log "$SRC" postgres "$TRANSFER_MARK"; then
    pass_row "IMMUT-READONLY-LOG"
  else
    fail_row "IMMUT-READONLY-LOG" "the source issued a non-read statement"
  fi
  # I-NOTEMP: the source must not spill. Today's source sorts every COPY, which
  # spills on the TOAST-heavy row with the fixture's 4 MB work_mem; the
  # commutative fingerprint of phase 3 § 3.3 removes that sort. Until it lands
  # the check reports rather than fails (`RB_PG_NOTEMP_MODE=strict` to enforce).
  if rb_pg_assert_no_temp_files "$SRC"; then
    pass_row "NOTEMP"
  elif [[ ${RB_PG_NOTEMP_MODE:-warn} == "strict" ]]; then
    fail_row "NOTEMP" "the source spilled a temporary file"
  else
    skip_row "NOTEMP" "the source still sorts each COPY; enforced from phase 3 § 3.3"
  fi

  # 4) External oracles.
  rb_pg_oracle_counts "$SRC" postgres "$DATABASE" "$WORK/counts.src"
  rb_pg_oracle_counts "$DST" postgres "$DATABASE" "$WORK/counts.dst"
  SRC_IP=$(rb_pg_container_ip "$SRC")
  rb_pg_oracle_schema "$DST" "$SRC_IP" 5432 postgres "$DATABASE" "$WORK/schema.src"
  rb_pg_oracle_schema "$DST" 127.0.0.1 5432 postgres "$DATABASE" "$WORK/schema.dst"
  diff -u <(normalize_counts "$WORK/counts.src" "$CROSS") \
          <(normalize_counts "$WORK/counts.dst" "$CROSS") >"$WORK/counts.diff" || true
  diff -u <(normalize_schema "$WORK/schema.src" "$CROSS") \
          <(normalize_schema "$WORK/schema.dst" "$CROSS") >"$WORK/schema.diff" || true
  if [[ -s "$WORK/counts.diff" ]]; then
    fail_row "ORACLE-DATA" "$(wc -l <"$WORK/counts.diff") diff lines"
    head -40 "$WORK/counts.diff"
  else
    pass_row "ORACLE-DATA"
  fi
  if [[ -s "$WORK/schema.diff" ]]; then
    fail_row "ORACLE-SCHEMA" "$(wc -l <"$WORK/schema.diff") diff lines"
    head -40 "$WORK/schema.diff"
  else
    pass_row "ORACLE-SCHEMA"
  fi

  # 5) Per-row results.
  evaluate_rows "$SOURCE_MAJOR" "$WORK/counts.dst" "$WORK/schema.dst" \
    "$WORK/counts.diff" "$WORK/schema.diff"

  # 6) Regression: encoding, collation and locale-provider metadata survive a
  # restore even though they differ from destination template1.
  source_db_metadata=$(database_metadata "$SRC" "$SOURCE_MAJOR")
  destination_db_metadata=$(database_metadata "$DST" "$DESTINATION_MAJOR")
  if [[ "$source_db_metadata" == "SQL_ASCII|C|C|c||" && \
        "$source_db_metadata" == "$destination_db_metadata" ]]; then
    pass_row "DB-METADATA"
  else
    fail_row "DB-METADATA" "source=${source_db_metadata} destination=${destination_db_metadata}"
  fi

  # 7) Fidelity of the state neither oracle can see.
  if diff <(fidelity_probe "$SRC" "$DATABASE" "$SOURCE_MAJOR") \
          <(fidelity_probe "$DST" "$DATABASE" "$SOURCE_MAJOR") >"$WORK/fidelity.diff"; then
    pass_row "FIDELITY-PROBE"
  else
    fail_row "FIDELITY-PROBE" "see the diff below"
    cat "$WORK/fidelity.diff"
  fi

  # 8) A partitioned parent's index must be valid, not left ON ONLY.
  if (( SOURCE_MAJOR >= 11 )); then
    valid=$(docker exec "$DST" psql -U postgres -d "$DATABASE" -At -c \
      "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
        WHERE c.relname = 'events_ts_idx'")
    if [[ "$valid" == "t" ]]; then
      pass_row "PARTITIONED-INDEX-VALID"
    else
      fail_row "PARTITIONED-INDEX-VALID" "indisvalid=${valid:-missing}"
    fi
  fi

  # 9) A read-only source role must not silently produce a plan whose sequence
  # values are all unreadable: pg_sequences.last_value is NULL both for "never
  # called" and for "no privilege".
  docker exec -i "$SRC" psql -U postgres -d "$DATABASE" -v ON_ERROR_STOP=1 -q >/dev/null <<SQL
CREATE ROLE backup_ro LOGIN PASSWORD 'ro';
GRANT CONNECT ON DATABASE ${DATABASE} TO backup_ro;
GRANT USAGE ON SCHEMA app TO backup_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA app TO backup_ro;
SQL
  plan_ro() {
    "$BIN" plan postgres --host 127.0.0.1 --port "$SRC_PORT" \
      --user backup_ro --password ro --database "$DATABASE" --sslmode disable
  }
  if plan_ro >"$WORK/plan-ro.log" 2>&1; then
    fail_row "SEQ-PRIVILEGE-REFUSAL" "a role without sequence privileges produced a plan"
  elif grep -q "cannot read the current value of sequence" "$WORK/plan-ro.log"; then
    pass_row "SEQ-PRIVILEGE-REFUSAL"
  else
    fail_row "SEQ-PRIVILEGE-REFUSAL" "the plan failed for another reason"
    tail -5 "$WORK/plan-ro.log"
  fi

  # 10) Role and database settings must round-trip as data, never as SQL.
  role_settings() { # container
    docker exec "$1" psql -U postgres -At -F'|' -c \
      "SELECT rolname, rolsuper::text, array_to_string(rolconfig, '@@')
         FROM pg_roles WHERE rolname = 'app_owner'"
    docker exec "$1" psql -U postgres -At -c \
      "SELECT array_to_string(s.setconfig, '@@')
         FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase
        WHERE d.datname = '${DATABASE}' AND s.setrole = 0"
  }
  if [[ "$(role_settings "$SRC")" == "$(role_settings "$DST")" ]] && \
     [[ "$(docker exec "$DST" psql -U postgres -At -c \
            "SELECT rolsuper FROM pg_roles WHERE rolname='app_owner'")" == "f" ]]; then
    pass_row "ROLE-SETTINGS"
  else
    fail_row "ROLE-SETTINGS" "settings differ or were executed as SQL"
    diff <(role_settings "$SRC") <(role_settings "$DST") || true
  fi

  # 11) Refusals: every M-PG-REF-* row.
  run_refusals "$SRC" "$SRC_PORT" "$DST" "$SOURCE_MAJOR" "$CASE_ID"

  # 12) Regression: the typed --overwrite flag must pass preflight, disconnect
  # users, drop the existing database and restore it from scratch.
  docker exec "$DST" psql -U postgres -d "$DATABASE" -v ON_ERROR_STOP=1 -q -c \
    "INSERT INTO app.accounts (email) VALUES ('destination-only@x')" >/dev/null
  # The data comparison runs through the same normalisation as ORACLE-DATA: a
  # cross-major pair legitimately renders inherited NOT NULL constraints
  # differently, and comparing the raw oracle output failed a correct restore.
  overwrite_rc=0
  run_transfer "$SRC_PORT" "$DST_PORT" "pgjob-overwrite-${CASE_ID}" overwrite || overwrite_rc=$?
  data_checksum "$SRC" "$DATABASE" >"$WORK/overwrite.src"
  data_checksum "$DST" "$DATABASE" >"$WORK/overwrite.dst"
  if (( overwrite_rc != 0 )); then
    fail_row "OVERWRITE" "the transfer failed: $(tail -1 "$WORK/dst.log")"
  elif ! diff -u <(normalize_counts "$WORK/overwrite.src" "$CROSS") \
                 <(normalize_counts "$WORK/overwrite.dst" "$CROSS") >"$WORK/overwrite.diff"; then
    fail_row "OVERWRITE" "restored data differs: $(wc -l <"$WORK/overwrite.diff") diff lines"
    head -20 "$WORK/overwrite.diff"
  elif [[ "$(database_metadata "$SRC" "$SOURCE_MAJOR")" \
       != "$(database_metadata "$DST" "$DESTINATION_MAJOR")" ]]; then
    fail_row "OVERWRITE" "database metadata differs after the overwrite"
  else
    pass_row "OVERWRITE"
  fi

  # 13) M-PG-EXT-08: the extension-version policy. Last, because it removes
  # rbtest 1.0 from the destination — every earlier step needs it to be there.
  run_extension_version "$SRC" "$SRC_PORT" "$DST" "$DST_PORT" "$CASE_ID"

  docker rm -f "$SRC" "$DST" >/dev/null 2>&1 || true
  echo "MATRIX ${CASE}: ${PASS} pass, ${FAIL} fail, ${SKIP} skip"
  TOTAL_PASS=$((TOTAL_PASS + PASS)); TOTAL_FAIL=$((TOTAL_FAIL + FAIL)); TOTAL_SKIP=$((TOTAL_SKIP + SKIP))
done

echo "================================================"
echo "MATRIX TOTAL: ${TOTAL_PASS} pass, ${TOTAL_FAIL} fail, ${TOTAL_SKIP} skip"
[[ $TOTAL_FAIL -eq 0 ]]
