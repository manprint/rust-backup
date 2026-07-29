#!/usr/bin/env bash
# T-SESSION-2: two independent filesystem pairs through `run --config`.
set -euo pipefail
source "$(dirname "$0")/lib.sh"
command -v timeout >/dev/null || { echo 'ERROR: missing timeout' >&2; exit 2; }
rb_build_release

work=$(mktemp -d)
server_pid=
cleanup() { [[ -z ${server_pid:-} ]] || kill "$server_pid" >/dev/null 2>&1 || true; rm -rf "$work"; }
trap cleanup EXIT INT TERM

for pair in one two; do
  mkdir -p "$work/$pair-src" "$work/$pair-dst"
  rb_seed_filesystem_fixture "$work/$pair-src" $((1024 * 1024 + 17))
done
port=$(rb_free_port)
"$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$port" --udp=false >"$work/server.log" 2>&1 &
server_pid=$!
rb_wait_tcp 127.0.0.1 "$port"

cat >"$work/session.yml" <<EOF
parallel_targets: 4
targets:
  - { module: filesystem, role: source, transport: { to: "127.0.0.1:$port", channel: one, udp: false }, params: { root: "$work/one-src" } }
  - { module: filesystem, role: destination, transport: { to: "127.0.0.1:$port", channel: one, udp: false }, params: { root: "$work/one-dst" }, auto_accept: true }
  - { module: filesystem, role: source, transport: { to: "127.0.0.1:$port", channel: two, udp: false }, params: { root: "$work/two-src" } }
  - { module: filesystem, role: destination, transport: { to: "127.0.0.1:$port", channel: two, udp: false }, params: { root: "$work/two-dst" }, auto_accept: true }
EOF
if ! timeout --foreground "${RUST_BACKUP_SESSION_TIMEOUT:-45s}" env RUST_LOG=info \
  "$RB_E2E_BIN" run --config "$work/session.yml" --parallel-targets 4 >"$work/run.log" 2>&1; then
  echo 'FAIL: parallel two-target session failed or timed out' >&2
  sed -n '1,240p' "$work/run.log" >&2
  sed -n '1,160p' "$work/server.log" >&2
  exit 1
fi
for pair in one two; do
  if [[ "$(rb_tree_digest "$work/$pair-src")" != "$(rb_tree_digest "$work/$pair-dst")" ]]; then
    echo "FAIL: $pair destination differs from source" >&2
    exit 1
  fi
done
progress_lines=$(grep -c 'target_label' "$work/run.log" || true)
if (( progress_lines < 4 )); then
  echo "FAIL: expected progress for four targets, got $progress_lines line(s)" >&2
  sed -n '1,240p' "$work/run.log" >&2
  exit 1
fi

cat >"$work/fail-fast.yml" <<EOF
targets:
  - { module: does-not-exist, role: source, transport: { to: "127.0.0.1:$port", channel: bad, udp: false }, params: { root: "$work/one-src" } }
  - { module: filesystem, role: destination, transport: { to: "127.0.0.1:$port", channel: never, udp: false }, params: { root: "$work/never-dst" }, auto_accept: true }
EOF
if timeout --foreground 15s "$RB_E2E_BIN" run --config "$work/fail-fast.yml" --fail-fast >"$work/fail-fast.log" 2>&1; then
  echo 'FAIL: fail-fast accepted invalid first target' >&2; exit 1
fi
[[ ! -e "$work/never-dst" ]]
echo 'T-SESSION-2 passed: parallel pairs, progress, fail-fast'
