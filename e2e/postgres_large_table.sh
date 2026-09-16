#!/usr/bin/env bash
# T-PG-RSS: memory is bounded by the chunk, not by the table.
#
# A single 2 GiB table is moved between two Docker PostgreSQL containers while
# the resident set of both `rust-backup` processes is sampled once a second.
# The promise under test (I-NOTEMP + I-BANDWIDTH) is that neither side ever
# holds the table: the source streams `COPY` frames into 1 MiB chunks and the
# destination writes them straight into `COPY … FROM STDIN`, so peak RSS must
# stay far below the payload — the cap here is 256 MiB per process.
#
# Usage: bash e2e/postgres_large_table.sh [MAJOR]     (default: 16)
# Env:   RUST_BACKUP_PG_LARGE_ROWS    rows in the table (default 2000000)
#        RUST_BACKUP_RSS_LIMIT_KIB    per-process cap in KiB (default 262144)
# Needs: docker, cargo. No sudo. Relay-only (--no-udp) for determinism.
# Exits: 0 both peaks under the cap and the restore verified, 1 otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."
source e2e/lib.sh

MAJOR=${1:-16}
ROWS=${RUST_BACKUP_PG_LARGE_ROWS:-2000000}
RSS_LIMIT_KIB=${RUST_BACKUP_RSS_LIMIT_KIB:-262144}
IMAGE="${RB_PG_IMAGE_REPO:-postgres}:${MAJOR}-alpine"
DATABASE=bigdb
BIN="./target/release/rust-backup"
WORK=$(mktemp -d)
SRC=rb-pg-large-src-$$
DST=rb-pg-large-dst-$$
PIDS=()
PASS=0
FAIL=0

pass() { printf 'PASS %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }

cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do kill -9 "$pid" >/dev/null 2>&1 || true; done
  docker rm -f "$SRC" "$DST" >/dev/null 2>&1 || true
  if [[ ${RUST_BACKUP_E2E_KEEP:-0} == 1 ]]; then
    echo "work directory kept: $WORK"
  else
    rm -rf "$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

# Peak resident set of one process, sampled while it lives. `VmHWM` is the
# kernel's own high-water mark, so a spike between two samples still shows.
watch_rss() { # pid outfile
  local pid=$1 out=$2
  : >"$out"
  while kill -0 "$pid" >/dev/null 2>&1; do
    awk '/VmHWM:/ {print $2}' "/proc/$pid/status" 2>/dev/null >>"$out" || true
    sleep 1
  done
}

peak_of() { # samples-file
  grep -E '^[0-9]+$' "$1" 2>/dev/null | sort -n | tail -n1
}

rb_build_release

SRC_PORT=$(rb_free_port)
DST_PORT=$(rb_free_port)
CTRL_PORT=$(rb_free_port)
rb_pg_start "$SRC" "$IMAGE" "$SRC_PORT" || exit 1
rb_pg_start "$DST" "$IMAGE" "$DST_PORT" || exit 1

echo "seeding ${ROWS} rows (~$((ROWS / 1024)) MiB of payload) into $DATABASE"
docker exec "$SRC" psql -U postgres -v ON_ERROR_STOP=1 -q -c \
  "CREATE DATABASE ${DATABASE} TEMPLATE template0 ENCODING 'SQL_ASCII'
     LC_COLLATE 'C' LC_CTYPE 'C'"
docker exec "$SRC" psql -U postgres -v ON_ERROR_STOP=1 -q -d "$DATABASE" -c \
  "CREATE TABLE big (id bigint PRIMARY KEY, payload text NOT NULL)"
docker exec "$SRC" psql -U postgres -v ON_ERROR_STOP=1 -q -d "$DATABASE" -c \
  "INSERT INTO big SELECT g, repeat(md5(g::text), 32) FROM generate_series(1, ${ROWS}) g"

"$BIN" server --bind-addr 127.0.0.1 --control-port "$CTRL_PORT" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!; PIDS+=("$SERVER_PID")
sleep 1

RUST_BACKUP_PASSWORD="$RB_PG_PASSWORD" "$BIN" postgres source \
  --to "127.0.0.1:${CTRL_PORT}" --channel pg-large --no-udp --insecure \
  --host 127.0.0.1 --port "$SRC_PORT" --user postgres --database "$DATABASE" \
  --sslmode disable >"$WORK/src.log" 2>&1 &
SRC_PID=$!; PIDS+=("$SRC_PID")
watch_rss "$SRC_PID" "$WORK/src.rss" &
PIDS+=("$!")
disown %% 2>/dev/null || true

waited=0
while (( waited < 600 )); do
  kill -0 "$SRC_PID" 2>/dev/null || break
  grep -q 'provider registered' "$WORK/server.log" 2>/dev/null && break
  sleep 0.5
  waited=$((waited + 1))
done

RUST_BACKUP_PASSWORD="$RB_PG_PASSWORD" "$BIN" postgres destination \
  --to "127.0.0.1:${CTRL_PORT}" --channel pg-large --no-udp --insecure --yes --admin \
  --host 127.0.0.1 --port "$DST_PORT" --user postgres --sslmode disable \
  >"$WORK/dst.log" 2>&1 &
DST_PID=$!; PIDS+=("$DST_PID")
watch_rss "$DST_PID" "$WORK/dst.rss" &
PIDS+=("$!")
disown %% 2>/dev/null || true

src_rc=0; dst_rc=0
wait "$SRC_PID" || src_rc=$?
wait "$DST_PID" || dst_rc=$?
kill "$SERVER_PID" >/dev/null 2>&1 || true

src_peak=$(peak_of "$WORK/src.rss")
dst_peak=$(peak_of "$WORK/dst.rss")
echo "peak RSS: source ${src_peak:-?} KiB, destination ${dst_peak:-?} KiB (cap ${RSS_LIMIT_KIB} KiB)"

if (( src_rc == 0 && dst_rc == 0 )); then
  pass "T-PG-RSS transfer: both peers exited 0"
else
  fail "T-PG-RSS transfer: source=$src_rc destination=$dst_rc"
  tail -20 "$WORK/src.log" || true
  tail -20 "$WORK/dst.log" || true
fi

if rb_assert_formal_verification "$WORK/src.log" "$WORK/dst.log"; then
  pass "T-PG-RSS verification: the restore carries its read-back proof"
else
  fail "T-PG-RSS verification: no formal proof in the logs"
fi

if rb_strip_ansi <"$WORK/dst.log" | grep -q "rows verified: ${ROWS} from tables, 0 from materialized views, ${ROWS} rows"; then
  pass "T-PG-RSS rows: the destination counted all ${ROWS} rows"
else
  fail "T-PG-RSS rows: the destination did not report ${ROWS} streamed rows"
  rb_strip_ansi <"$WORK/dst.log" | grep 'rows verified' || true
fi

for side in src dst; do
  peak=$(peak_of "$WORK/$side.rss")
  if [[ -z ${peak:-} ]]; then
    fail "T-PG-RSS $side: no RSS sample was taken"
  elif (( peak <= RSS_LIMIT_KIB )); then
    pass "T-PG-RSS $side: peak ${peak} KiB <= ${RSS_LIMIT_KIB} KiB"
  else
    fail "T-PG-RSS $side: peak ${peak} KiB exceeds ${RSS_LIMIT_KIB} KiB"
  fi
done

echo "T-PG-RSS summary: ${PASS} pass, ${FAIL} fail"
(( FAIL == 0 ))
