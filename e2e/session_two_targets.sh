#!/usr/bin/env bash
# T-SESSION-2: two independent filesystem pairs through `run --config`.
set -euo pipefail
source "$(dirname "$0")/lib.sh"
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
RUST_LOG=info "$RB_E2E_BIN" run --config "$work/session.yml" --parallel-targets 4 >"$work/run.log" 2>&1
[[ "$(rb_tree_digest "$work/one-src")" == "$(rb_tree_digest "$work/one-dst")" ]]
[[ "$(rb_tree_digest "$work/two-src")" == "$(rb_tree_digest "$work/two-dst")" ]]
[[ $(rg -c 'target_label=' "$work/run.log") -ge 4 ]]

cat >"$work/fail-fast.yml" <<EOF
targets:
  - { module: does-not-exist, role: source, transport: { to: "127.0.0.1:$port", channel: bad, udp: false }, params: { root: "$work/one-src" } }
  - { module: filesystem, role: destination, transport: { to: "127.0.0.1:$port", channel: never, udp: false }, params: { root: "$work/never-dst" }, auto_accept: true }
EOF
if "$RB_E2E_BIN" run --config "$work/fail-fast.yml" --fail-fast >"$work/fail-fast.log" 2>&1; then
  echo 'FAIL: fail-fast accepted invalid first target' >&2; exit 1
fi
[[ ! -e "$work/never-dst" ]]
echo 'T-SESSION-2 passed: parallel pairs, progress, fail-fast'
