#!/usr/bin/env bash
# T-FS-OWN and T-FS-IMMUT. Invoke directly: sudo -n /absolute/path/to/script.
# Tests privileged restore, non-root ownership fallback, and abort immutability.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
if (( EUID != 0 )); then echo 'ERROR: invoke directly with sudo -n /absolute/path/e2e/filesystem_netns_test.sh' >&2; exit 2; fi
rb_build_release

work=$(mktemp -d)
chmod 755 "$work" # `runuser` needs to traverse the per-run fixture directory.
PIDS=()
PASS=0
FAIL=0
cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do kill "$pid" >/dev/null 2>&1 || true; done
  if (( status != 0 || FAIL != 0 )); then find "$work" -name '*.log' -maxdepth 2 -print -exec sed -n '1,160p' {} \; 2>/dev/null || true; fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }

start_server() { # port log
  RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$1" --udp=false >"$2" 2>&1 &
  PIDS+=("$!")
  rb_wait_tcp 127.0.0.1 "$1"
}
transfer() { # source dest port channel [run-as-user]
  local src=$1 dst=$2 port=$3 channel=$4 run_as=${5:-}
  if [[ -n "$run_as" ]]; then
    runuser -u "$run_as" -- env RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" --channel "$channel" --no-udp --insecure --root "$src" >"$work/$channel-source.log" 2>&1 &
  else
    RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" --channel "$channel" --no-udp --insecure --root "$src" >"$work/$channel-source.log" 2>&1 &
  fi
  local spid=$!; PIDS+=("$spid")
  sleep .3
  if [[ -n "$run_as" ]]; then
    runuser -u "$run_as" -- env RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port" --channel "$channel" --no-udp --insecure --yes --root "$dst" >"$work/$channel-destination.log" 2>&1 &
  else
    RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port" --channel "$channel" --no-udp --insecure --yes --root "$dst" >"$work/$channel-destination.log" 2>&1 &
  fi
  local dpid=$!; PIDS+=("$dpid")
  wait "$spid" && wait "$dpid"
}

# (a) root/CAP_CHOWN: exact metadata and hard-link topology.
root_src="$work/root-source"; root_dst="$work/root-destination"
mkdir -p "$root_src" "$root_dst"
rb_seed_filesystem_fixture "$root_src" $((2 * 1024 * 1024 + 13))
chown 0:0 "$root_src/nested/hello.txt"
chown 1:1 "$root_src/hard-a" "$root_src/hard-b" || true
chown 65534:65534 "$root_src/empty" || true
port=$(rb_free_port)
if start_server "$port" "$work/root-server.log" && transfer "$root_src" "$root_dst" "$port" fs-root; then
  if [[ "$(rb_tree_digest "$root_src")" == "$(rb_tree_digest "$root_dst")" ]] && [[ "$(stat -c '%d:%i' "$root_dst/hard-a")" == "$(stat -c '%d:%i' "$root_dst/hard-b")" ]]; then
    pass 'root restore preserves ownership, modes, mtimes, symlink, hard link'
  else fail 'root restore metadata/link topology differs'; fi
else fail 'root filesystem transfer failed'; fi

# (b) non-root: foreign metadata is readable, destination keeps process owner and warns.
nonroot=nobody
id "$nonroot" >/dev/null 2>&1 || nonroot=$(getent passwd 65534 | cut -d: -f1)
nr_src="$work/nonroot-source"; nr_dst="$work/nonroot-destination"
mkdir -p "$nr_src" "$nr_dst"
rb_seed_filesystem_fixture "$nr_src" 0
chmod -R a+rX "$nr_src"
chown -R 0:0 "$nr_src"
chown "$nonroot":"$nonroot" "$nr_dst"
port=$(rb_free_port)
if start_server "$port" "$work/nonroot-server.log" && transfer "$nr_src" "$nr_dst" "$port" fs-nonroot "$nonroot"; then
  owner=$(stat -c '%u:%g' "$nr_dst/nested/hello.txt")
  run_uid_gid=$(id -u "$nonroot"):$(id -g "$nonroot")
  if [[ "$owner" == "$run_uid_gid" ]] && grep -q 'uid/gid will remain the destination process owner' "$work/fs-nonroot-destination.log"; then
    pass 'non-root restore falls back to process ownership with warning'
  else fail 'non-root ownership fallback/warning missing'; fi
  if [[ "$(stat -c '%a:%Y' "$nr_src/nested/hello.txt")" == "$(stat -c '%a:%Y' "$nr_dst/nested/hello.txt")" ]]; then pass 'non-root preserves mode and mtime'; else fail 'non-root mode or mtime differs'; fi
else fail 'non-root filesystem transfer failed'; fi

# (c) abort destination after transfer starts; source contents and every atime stay fixed.
abort_src="$work/abort-source"; abort_dst="$work/abort-destination"
mkdir -p "$abort_src" "$abort_dst"
rb_seed_filesystem_fixture "$abort_src" $((128 * 1024 * 1024))
before_tree=$(rb_tree_digest "$abort_src")
before_atime=$(rb_atime_manifest "$abort_src")
port=$(rb_free_port)
if start_server "$port" "$work/abort-server.log"; then
  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" --channel fs-abort --no-udp --insecure --max-rate 4194304 --root "$abort_src" >"$work/fs-abort-source.log" 2>&1 &
  spid=$!; PIDS+=("$spid")
  sleep .3
  RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port" --channel fs-abort --no-udp --insecure --yes --root "$abort_dst" >"$work/fs-abort-destination.log" 2>&1 &
  dpid=$!; PIDS+=("$dpid")
  sleep 16 # ~50% of a 128 MiB payload at 4 MiB/s, allowing relay startup.
  kill -9 "$dpid" >/dev/null 2>&1 || true
  wait "$spid" >/dev/null 2>&1 || true
  if [[ "$before_tree" == "$(rb_tree_digest "$abort_src")" ]] && [[ "$before_atime" == "$(rb_atime_manifest "$abort_src")" ]]; then
    pass 'mid-transfer destination abort leaves source tree and atimes unchanged'
  else fail 'mid-transfer abort changed source tree or atime'; fi
else fail 'abort test server did not start'; fi

printf 'T-FS-OWN/T-FS-IMMUT summary: PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
(( FAIL == 0 ))
