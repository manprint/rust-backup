#!/usr/bin/env bash
# T-PG-HOT: `--hot-backup` copies a PostgreSQL source that keeps being written.
#
# Two Docker PostgreSQL containers; the source holds a parent/child pair linked
# by a foreign key and fed by a sequence, and a writer inserts parent+child
# pairs (and rewrites parents) for the whole transfer. The transfer is slowed
# with --max-rate so the writes land between the COPY of one table and the
# next — exactly where a copy that is not one snapshot restores children whose
# parent it never read.
#
#   HOT-01  hot backup under writes: both peers print their hot-backup
#           verification with one commitment, the restored children all have
#           their parent, and no sequence restarts at or below an id it already
#           handed out.
#   HOT-02  a destination without --hot-backup refuses the hot plan at
#           preflight (exit 3) and creates nothing.
#   HOT-03  the same writes under a cold backup still fail the immutability
#           audit (exit 6): the audit is off only when asked for.
#   HOT-04  --hot-backup on a module that cannot take a snapshot is a
#           configuration error (exit 2).
#
# Usage: bash e2e/postgres_hot_backup.sh [MAJOR]     (default: 16)
# Needs: docker, cargo. No sudo. Relay-only (--no-udp) for determinism.
# Exits: 0 every case passed, 1 otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."
source e2e/lib.sh

MAJOR=${1:-16}
IMAGE="${RB_PG_IMAGE_REPO:-postgres}:${MAJOR}-alpine"
DATABASE=hotdb
PARENTS=${RUST_BACKUP_PG_HOT_PARENTS:-50000}
# bytes/second: the ~30 MiB payload then takes about ten seconds to move.
MAX_RATE=${RUST_BACKUP_PG_HOT_RATE:-3000000}
BIN="./target/release/rust-backup"
WORK=$(mktemp -d)
SRC=rb-pg-hot-src-$$
DST=rb-pg-hot-dst-$$
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

src_sql() { docker exec "$SRC" psql -U postgres -d "$DATABASE" -v ON_ERROR_STOP=1 -At -c "$1"; }
dst_sql() { docker exec "$DST" psql -U postgres -d "$DATABASE" -v ON_ERROR_STOP=1 -At -c "$1"; }

# Inserts a parent and two children per statement and rewrites an existing
# parent, one autocommit statement at a time, until killed.
start_writer() {
  {
    for _ in $(seq 1 100000); do
      echo "WITH p AS (INSERT INTO parent (payload) VALUES (repeat('w', 200)) RETURNING id)
            INSERT INTO child (parent_id, payload) SELECT id, repeat('c', 200) FROM p, generate_series(1, 2);"
      echo "UPDATE parent SET payload = md5(random()::text) WHERE id = (random() * ${PARENTS})::int + 1;"
      echo "SELECT pg_sleep(0.01);"
    done
  } | docker exec -i "$SRC" psql -U postgres -d "$DATABASE" -q -o /dev/null >/dev/null 2>&1 &
  WRITER_PID=$!
  PIDS+=("$WRITER_PID")
}

stop_writer() {
  kill "$WRITER_PID" >/dev/null 2>&1 || true
  wait "$WRITER_PID" 2>/dev/null || true
}

# transfer <case> <source extra args> -- <destination extra args>
# Sets SRC_RC and DST_RC; logs land in $WORK/<case>.{src,dst,server}.log.
transfer() {
  local name=$1
  shift
  local src_args=() dst_args=()
  while (( $# > 0 )) && [[ $1 != -- ]]; do src_args+=("$1"); shift; done
  [[ ${1:-} == -- ]] && shift
  dst_args=("$@")
  local ctrl_port
  ctrl_port=$(rb_free_port)
  "$BIN" server --bind-addr 127.0.0.1 --control-port "$ctrl_port" >"$WORK/$name.server.log" 2>&1 &
  local server_pid=$!; PIDS+=("$server_pid")
  sleep 1
  RUST_BACKUP_PASSWORD="$RB_PG_PASSWORD" "$BIN" postgres source \
    --to "127.0.0.1:${ctrl_port}" --channel "$name" --no-udp --insecure \
    --host 127.0.0.1 --port "$SRC_PORT" --user postgres --database "$DATABASE" \
    --sslmode disable --max-rate "$MAX_RATE" "${src_args[@]}" >"$WORK/$name.src.log" 2>&1 &
  local src_pid=$!; PIDS+=("$src_pid")
  local waited=0
  while (( waited < 600 )); do
    kill -0 "$src_pid" 2>/dev/null || break
    grep -q 'provider registered' "$WORK/$name.server.log" 2>/dev/null && break
    sleep 0.5
    waited=$((waited + 1))
  done
  RUST_BACKUP_PASSWORD="$RB_PG_PASSWORD" "$BIN" postgres destination \
    --to "127.0.0.1:${ctrl_port}" --channel "$name" --no-udp --insecure --yes --admin \
    --host 127.0.0.1 --port "$DST_PORT" --user postgres --sslmode disable \
    "${dst_args[@]}" >"$WORK/$name.dst.log" 2>&1 &
  local dst_pid=$!; PIDS+=("$dst_pid")
  SRC_RC=0; DST_RC=0
  wait "$src_pid" || SRC_RC=$?
  wait "$dst_pid" || DST_RC=$?
  kill "$server_pid" >/dev/null 2>&1 || true
}

show_logs() { # case
  tail -15 "$WORK/$1.src.log" || true
  tail -15 "$WORK/$1.dst.log" || true
}

# --- HOT-04 needs no database ----------------------------------------------
rb_build_release
mkdir -p "$WORK/fs-root"
fs_rc=0
"$BIN" filesystem source --to 127.0.0.1:9 --channel hot-fs --no-udp --insecure \
  --root "$WORK/fs-root" --hot-backup >"$WORK/hot04.log" 2>&1 || fs_rc=$?
if (( fs_rc == 2 )) && grep -q 'not supported by the filesystem module' "$WORK/hot04.log"; then
  pass "HOT-04 --hot-backup on filesystem is a configuration error (exit 2)"
else
  fail "HOT-04 filesystem --hot-backup: exit $fs_rc"
  cat "$WORK/hot04.log" >&2 || true
fi

SRC_PORT=$(rb_free_port)
DST_PORT=$(rb_free_port)
rb_pg_start "$SRC" "$IMAGE" "$SRC_PORT" || exit 1
rb_pg_start "$DST" "$IMAGE" "$DST_PORT" || exit 1

echo "seeding ${PARENTS} parents and $((PARENTS * 2)) children into $DATABASE"
docker exec "$SRC" psql -U postgres -v ON_ERROR_STOP=1 -q -c "CREATE DATABASE ${DATABASE}"
src_sql "CREATE TABLE parent (id serial PRIMARY KEY, payload text NOT NULL);
         CREATE TABLE child (id bigserial PRIMARY KEY,
                             parent_id int NOT NULL REFERENCES parent (id),
                             payload text NOT NULL);
         INSERT INTO parent (payload) SELECT repeat('p', 200) FROM generate_series(1, ${PARENTS});
         INSERT INTO child (parent_id, payload)
           SELECT (g % ${PARENTS}) + 1, repeat('c', 200) FROM generate_series(1, $((PARENTS * 2))) g;" >/dev/null

# --- HOT-02: no destination consent ------------------------------------------
transfer hot02 --hot-backup --
if (( DST_RC == 3 )) && (( SRC_RC != 0 )) &&
    rb_strip_ansi <"$WORK/hot02.dst.log" | grep -q 'pass --hot-backup to the destination'; then
  pass "HOT-02 a destination without --hot-backup refuses the hot plan (exit 3)"
else
  fail "HOT-02 no consent: source=$SRC_RC destination=$DST_RC"
  show_logs hot02
fi
if [[ -z $(docker exec "$DST" psql -U postgres -At -c \
    "SELECT 1 FROM pg_database WHERE datname = '${DATABASE}'") ]]; then
  pass "HOT-02 the refused plan created nothing on the destination"
else
  fail "HOT-02 the refused plan left ${DATABASE} on the destination"
fi

# --- HOT-01: hot backup under writes ------------------------------------------
parents_before=$(src_sql "SELECT count(*) FROM parent")
start_writer
transfer hot01 --hot-backup -- --hot-backup
stop_writer
parents_after=$(src_sql "SELECT count(*) FROM parent")

if (( SRC_RC == 0 && DST_RC == 0 )); then
  pass "HOT-01 transfer: both peers exited 0 while the source was written"
else
  fail "HOT-01 transfer: source=$SRC_RC destination=$DST_RC"
  show_logs hot01
fi
if (( parents_after > parents_before )); then
  pass "HOT-01 writer: the source grew from ${parents_before} to ${parents_after} parents during the run"
else
  fail "HOT-01 writer: the source did not change (${parents_before} -> ${parents_after}); the case proves nothing"
fi
src_digest=$(rb_strip_ansi <"$WORK/hot01.src.log" | grep 'BACKUP VERIFIED (hot backup)' | tail -n1 |
  grep -oE 'blake3=[0-9a-f]{64}' | cut -d= -f2 || true)
dst_digest=$(rb_strip_ansi <"$WORK/hot01.dst.log" | grep 'RESTORE VERIFIED (hot backup)' | tail -n1 |
  grep -oE 'blake3=[0-9a-f]{64}' | cut -d= -f2 || true)
if [[ -n $src_digest && $src_digest == "$dst_digest" ]]; then
  pass "HOT-01 verification: both peers report the hot-backup read-back with one commitment"
else
  fail "HOT-01 verification: source='${src_digest}' destination='${dst_digest}'"
  show_logs hot01
fi
if rb_strip_ansi <"$WORK/hot01.src.log" | grep -q 'BACKUP VERIFIED: source unchanged'; then
  fail "HOT-01 the hot source claims an unchanged source"
else
  pass "HOT-01 the hot source does not claim an unchanged source"
fi
orphans=$(dst_sql "SELECT count(*) FROM child c LEFT JOIN parent p ON p.id = c.parent_id WHERE p.id IS NULL" || echo "?")
if [[ $orphans == 0 ]]; then
  pass "HOT-01 consistency: every restored child has its parent"
else
  fail "HOT-01 consistency: ${orphans} restored children have no parent"
fi
seq_ok=$(dst_sql "SELECT (SELECT last_value FROM parent_id_seq) >= (SELECT coalesce(max(id), 0) FROM parent)
                  AND (SELECT last_value FROM child_id_seq) >= (SELECT coalesce(max(id), 0) FROM child)" || echo "?")
if [[ $seq_ok == t ]]; then
  pass "HOT-01 sequences: no restored sequence is behind the ids it handed out"
else
  fail "HOT-01 sequences: a restored sequence is behind its table (${seq_ok})"
fi

# --- HOT-03: the same writes under a cold backup ------------------------------
start_writer
transfer hot03 -- --overwrite
stop_writer
if (( SRC_RC == 6 )); then
  pass "HOT-03 a cold backup of the written source fails the immutability audit (exit 6)"
else
  fail "HOT-03 cold backup under writes: source=$SRC_RC destination=$DST_RC (want source 6)"
  show_logs hot03
fi

echo "T-PG-HOT summary: ${PASS} pass, ${FAIL} fail"
(( FAIL == 0 ))
