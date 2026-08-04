#!/usr/bin/env bash
# T-MONGO-MATRIX + T-MONGO-IMMUT (plan Phase 3.7): full backup -> restore -> diff
# over the relay, across MongoDB majors, plus the source-immutability assertion
# (incl. a run aborted mid-transfer).
#
# For each major: two Docker MongoDB containers (source seeded, destination
# empty), then `rust-backup server` + `mongodb source` + `mongodb destination`
# move the cluster over the relay; finally per-collection counts, document
# content (ordered by _id) and index specs are compared, and the source is
# proven unchanged.
#
# Usage: bash e2e/mongodb_matrix.sh [SOURCE[:DESTINATION] ...] (default: 6)
# Examples: `6` tests 6 -> 6, `4:8` tests a cross-major restore.
# Needs:  docker, cargo. No sudo. Relay-only (--no-udp) for CI determinism.
# Exits non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")/.."
source e2e/lib.sh

CASES=("${@:-6}")
PASS=0
FAIL=0
BIN="./target/release/rust-backup"
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

image_for() { # major -> docker image tag
  case "$1" in
    4) echo "mongo:4.4" ;;
    # 8.0.x refuses Linux >= 6.19 (SERVER-121912).  `mongo:8` tracks the
    # maintained MongoDB 8 release line while still exercising the 8.x wire
    # and dump/restore format.
    8) echo "mongo:8" ;;
    *) echo "mongo:$1.0" ;;
  esac
}

# Run a JS snippet inside a container, preferring mongosh, falling back to the
# legacy `mongo` shell (MongoDB 4.x images ship `mongo`, 5+ ship `mongosh`).
mongo_eval() { # container db js
  local c="$1" db="$2" js="$3"
  if docker exec "$c" sh -c 'command -v mongosh' >/dev/null 2>&1; then
    docker exec "$c" mongosh --quiet "$db" --eval "$js"
  else
    docker exec "$c" mongo --quiet "$db" --eval "$js"
  fi
}

start_mongo() { # name port image -> start container, wait ready
  local name="$1" port="$2" image="$3"
  CONTAINERS+=("$name")
  docker run -d --name "$name" -p "${port}:27017" "$image" >/dev/null
  # First startup of a newly pulled Mongo image can exceed 40 seconds on CI.
  for _ in $(seq 1 90); do
    if mongo_eval "$name" admin 'db.adminCommand({ ping: 1 })' >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "FAIL: $name not ready"; return 1
}

seed_source() { # container
  mongo_eval "$1" appdb '
    db.createCollection("empty");
    db.accounts.createIndex({ email: 1 }, { unique: true });
    db.orders.createIndex({ acct: 1 });
    db.events.createIndex(
      { kind: 1, seq: -1 },
      { name: "active_kind_seq", partialFilterExpression: { active: true } }
    );
    var a = [];
    for (var i = 0; i < 500; i++) a.push({ email: "u" + i + "@x", status: "active" });
    db.accounts.insertMany(a);
    var o = [];
    for (var j = 0; j < 2000; j++) o.push({ acct: j % 500, total: j * 1.5 });
    db.orders.insertMany(o);
    db.events.insertMany([
      { _id: "event-a", kind: "created", seq: 1, active: true,
        nested: { source: "api", retry: false }, tags: ["one", "two"], optional: null },
      { _id: "event-b", kind: "updated", seq: 2, active: false,
        nested: { source: "worker", retry: true }, tags: [], optional: "present" }
    ]);
  ' >/dev/null
}

# A deterministic textual digest of a database: per collection, its document
# count + documents ordered by _id + index key/name/unique specs. Identical for
# two logically-equal databases regardless of insertion order.
cluster_digest() { # container db
  mongo_eval "$1" "$2" '
    var out = "";
    db.getCollectionNames()
      .filter(function (n) { return n.indexOf("system.") !== 0; })
      .sort()
      .forEach(function (c) {
        var coll = db.getCollection(c);
        var docs = coll.find().sort({ _id: 1 }).toArray();
        out += c + "|count|" + docs.length + "\n";
        out += c + "|data|" + JSON.stringify(docs) + "\n";
        var idx = coll.getIndexes().map(function (i) {
          var key = {};
          Object.keys(i.key).forEach(function (field) {
            // mongosh may expose the same BSON int64 direction either as a
            // JavaScript number or as a Long object, depending on server
            // version.  Normalise both without hiding semantic differences.
            key[field] = Number(i.key[field].toString());
          });
          return { key: key, name: i.name, unique: i.unique === true };
        }).sort(function (a, b) { return a.name < b.name ? -1 : 1; });
        out += c + "|idx|" + JSON.stringify(idx) + "\n";
      });
    print(out);
  ' | sed -E 's/"_id":\{"\$oid":"([0-9a-f]{24})"\}/"_id":"\1"/g'
}

run_transfer() { # src_port dst_port channel [abort|overwrite]
  local src_port="$1" dst_port="$2" channel="$3" mode="${4:-}"
  "$BIN" server --bind-addr 127.0.0.1 --control-port "$CTRL_PORT" >/tmp/rb-mongo-server.log 2>&1 &
  local server_pid=$!; PIDS+=("$server_pid")
  sleep 1

  # Source registers as provider first, then destination consumes.
  "$BIN" mongodb source --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --host 127.0.0.1 --port "$src_port" --database appdb \
    >/tmp/rb-mongo-src.log 2>&1 &
  local src_pid=$!; PIDS+=("$src_pid")

  if [[ "$mode" == "abort" ]]; then
    sleep 2; kill -9 "$src_pid" >/dev/null 2>&1 || true
    kill "$server_pid" >/dev/null 2>&1 || true
    return 0
  fi

  sleep 2
  local destination_args=(mongodb destination --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --yes --host 127.0.0.1 --port "$dst_port")
  if [[ $mode == overwrite ]]; then
    destination_args+=(--overwrite)
  fi
  "$BIN" "${destination_args[@]}" >/tmp/rb-mongo-dst.log 2>&1 &
  local dst_pid=$!; PIDS+=("$dst_pid")

  wait "$src_pid"; local src_rc=$?
  wait "$dst_pid"; local dst_rc=$?
  kill "$server_pid" >/dev/null 2>&1 || true
  [[ $src_rc -eq 0 && $dst_rc -eq 0 ]] && \
    rb_assert_formal_verification /tmp/rb-mongo-src.log /tmp/rb-mongo-dst.log
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
    echo "FAIL: invalid MongoDB case $CASE" >&2; exit 2;
  }
  (( DESTINATION_MAJOR >= SOURCE_MAJOR )) || {
    echo "FAIL: destination MongoDB must be >= source ($CASE)" >&2; exit 2;
  }
  CASE_ID="${SOURCE_MAJOR}-to-${DESTINATION_MAJOR}"
  echo "================  MongoDB ${SOURCE_MAJOR} -> ${DESTINATION_MAJOR}  ================"
  SOURCE_IMAGE="$(image_for "$SOURCE_MAJOR")"
  DESTINATION_IMAGE="$(image_for "$DESTINATION_MAJOR")"
  SRC="rb-mongo-src-${CASE_ID}-$$"; DST="rb-mongo-dst-${CASE_ID}-$$"
  SRC_PORT=$(rb_free_port)
  DST_PORT=$(rb_free_port)
  while [[ $DST_PORT == "$SRC_PORT" ]]; do DST_PORT=$(rb_free_port); done

  start_mongo "$SRC" "$SRC_PORT" "$SOURCE_IMAGE"
  start_mongo "$DST" "$DST_PORT" "$DESTINATION_IMAGE"
  seed_source "$SRC"

  # Source content baseline (external immutability reference).
  fp_before="$(cluster_digest "$SRC" appdb)"

  # 1) Mid-transfer abort must not mutate the source (T-MONGO-IMMUT).
  run_transfer "$SRC_PORT" "$DST_PORT" "mongojob-abort-${CASE_ID}" abort || true
  fp_after_abort="$(cluster_digest "$SRC" appdb)"
  if [[ "$fp_before" == "$fp_after_abort" ]]; then
    echo "PASS: source unchanged after aborted transfer"; PASS=$((PASS+1))
  else
    echo "FAIL: source mutated by aborted transfer"; FAIL=$((FAIL+1))
  fi

  # 2) Full backup -> restore.
  if run_transfer "$SRC_PORT" "$DST_PORT" "mongojob-${CASE_ID}"; then
    echo "PASS: transfer completed with persisted read-back proof"; PASS=$((PASS+1))
  else
    echo "FAIL: transfer failed (see /tmp/rb-mongo-*.log)"; FAIL=$((FAIL+1)); continue
  fi

  # 3) Source still unchanged after the full run.
  fp_after="$(cluster_digest "$SRC" appdb)"
  if [[ "$fp_before" == "$fp_after" ]]; then
    echo "PASS: source unchanged after full transfer"; PASS=$((PASS+1))
  else
    echo "FAIL: source mutated by full transfer"; FAIL=$((FAIL+1))
  fi

  # 4) Destination matches source 1:1 (counts + content + indexes).
  if diff <(printf '%s\n' "$fp_before" | sed '/^$/d') <(cluster_digest "$DST" appdb | sed '/^$/d') >/tmp/rb-mongo.diff; then
    echo "PASS: destination matches source"; PASS=$((PASS+1))
  else
    echo "FAIL: destination differs (see /tmp/rb-mongo.diff)"; FAIL=$((FAIL+1))
  fi

  # 5) A realistic retry replaces existing collections, including a
  # destination-only document, and must produce a fresh read-back proof.
  mongo_eval "$DST" appdb 'db.accounts.insertOne({email:"destination-only@x", status:"stale"})' >/dev/null
  if run_transfer "$SRC_PORT" "$DST_PORT" "mongojob-overwrite-${CASE_ID}" overwrite && \
     diff <(printf '%s\n' "$fp_before" | sed '/^$/d') <(cluster_digest "$DST" appdb | sed '/^$/d') >/tmp/rb-mongo-overwrite.diff; then
    echo "PASS: --overwrite replaces existing collections with verified state"; PASS=$((PASS+1))
  else
    echo "FAIL: --overwrite did not recreate exact state"; FAIL=$((FAIL+1))
  fi

  docker rm -f "$SRC" "$DST" >/dev/null 2>&1 || true
done

echo "================================================"
echo "PASS=${PASS}  FAIL=${FAIL}"
[[ $FAIL -eq 0 ]]
