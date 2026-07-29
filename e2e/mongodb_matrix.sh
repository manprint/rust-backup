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
# Usage:  bash e2e/mongodb_matrix.sh [MAJOR ...]   (default: 6)
# Needs:  docker, cargo. No sudo. Relay-only (--no-udp) for CI determinism.
# Exits non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")/.."

MAJORS=("${@:-6}")
PASS=0
FAIL=0
BIN="./target/release/rust-backup"
CTRL_PORT=7836   # distinct from the postgres matrix default (7835)

CONTAINERS=()
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" >/dev/null 2>&1 || true; done
  for c in "${CONTAINERS[@]:-}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
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
  docker run -d --name "$name" -p "${port}:27017" "$image" >/dev/null
  CONTAINERS+=("$name")
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
    db.accounts.createIndex({ email: 1 }, { unique: true });
    db.orders.createIndex({ acct: 1 });
    var a = [];
    for (var i = 0; i < 500; i++) a.push({ email: "u" + i + "@x", status: "active" });
    db.accounts.insertMany(a);
    var o = [];
    for (var j = 0; j < 2000; j++) o.push({ acct: j % 500, total: j * 1.5 });
    db.orders.insertMany(o);
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
          return { key: i.key, name: i.name, unique: i.unique === true };
        }).sort(function (a, b) { return a.name < b.name ? -1 : 1; });
        out += c + "|idx|" + JSON.stringify(idx) + "\n";
      });
    print(out);
  '
}

run_transfer() { # src_port dst_port channel [abort]
  local src_port="$1" dst_port="$2" channel="$3" abort="${4:-}"
  "$BIN" server --bind-addr 127.0.0.1 --control-port "$CTRL_PORT" >/tmp/rb-mongo-server.log 2>&1 &
  local server_pid=$!; PIDS+=("$server_pid")
  sleep 1

  # Source registers as provider first, then destination consumes.
  "$BIN" mongodb source --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --host 127.0.0.1 --port "$src_port" --database appdb \
    >/tmp/rb-mongo-src.log 2>&1 &
  local src_pid=$!; PIDS+=("$src_pid")

  if [[ "$abort" == "abort" ]]; then
    sleep 2; kill -9 "$src_pid" >/dev/null 2>&1 || true
    kill "$server_pid" >/dev/null 2>&1 || true
    return 0
  fi

  sleep 2
  "$BIN" mongodb destination --to "127.0.0.1:${CTRL_PORT}" --channel "$channel" \
    --no-udp --insecure --yes --host 127.0.0.1 --port "$dst_port" \
    >/tmp/rb-mongo-dst.log 2>&1 &
  local dst_pid=$!; PIDS+=("$dst_pid")

  wait "$src_pid"; local src_rc=$?
  wait "$dst_pid"; local dst_rc=$?
  kill "$server_pid" >/dev/null 2>&1 || true
  [[ $src_rc -eq 0 && $dst_rc -eq 0 ]]
}

for MAJOR in "${MAJORS[@]}"; do
  echo "================  MongoDB ${MAJOR}  ================"
  IMAGE="$(image_for "$MAJOR")"
  SRC="rb-mongo-src-${MAJOR}-$$"; DST="rb-mongo-dst-${MAJOR}-$$"
  SRC_PORT=$((27100 + MAJOR)); DST_PORT=$((27200 + MAJOR))

  start_mongo "$SRC" "$SRC_PORT" "$IMAGE"
  start_mongo "$DST" "$DST_PORT" "$IMAGE"
  seed_source "$SRC"

  # Source content baseline (external immutability reference).
  fp_before="$(cluster_digest "$SRC" appdb)"

  # 1) Mid-transfer abort must not mutate the source (T-MONGO-IMMUT).
  run_transfer "$SRC_PORT" "$DST_PORT" "mongojob-abort-${MAJOR}" abort || true
  fp_after_abort="$(cluster_digest "$SRC" appdb)"
  if [[ "$fp_before" == "$fp_after_abort" ]]; then
    echo "PASS: source unchanged after aborted transfer"; PASS=$((PASS+1))
  else
    echo "FAIL: source mutated by aborted transfer"; FAIL=$((FAIL+1))
  fi

  # 2) Full backup -> restore.
  if run_transfer "$SRC_PORT" "$DST_PORT" "mongojob-${MAJOR}"; then
    echo "PASS: transfer completed"; PASS=$((PASS+1))
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

  docker rm -f "$SRC" "$DST" >/dev/null 2>&1 || true
done

echo "================================================"
echo "PASS=${PASS}  FAIL=${FAIL}"
[[ $FAIL -eq 0 ]]
