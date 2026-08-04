#!/usr/bin/env bash
# T-NET1: real source -> destination filesystem transfers across Linux namespaces.
# Covers direct QUIC, forced relay fallback, and loss of UDP after direct setup.
# Invoke directly with sudo -n /absolute/path/e2e/transport_netns_test.sh.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
if (( EUID != 0 )); then echo 'ERROR: run with exact-path sudo -n' >&2; exit 2; fi
for command in ip iptables python3 timeout; do command -v "$command" >/dev/null || { echo "ERROR: missing $command" >&2; exit 2; }; done
rb_build_release

tag="rbnet$$"
coord="${tag}c"; nat="${tag}n"; src_ns="${tag}s"; dst_ns="${tag}d"
work=$(mktemp -d)
PIDS=()
NAMESPACES=()
PASS=0
FAIL=0
# Payload is 96 MiB at 4 MiB/s (~24 s); 60 s leaves handshake headroom while
# keeping a failed direct/fallback path bounded in the privileged matrix.
TRANSFER_TIMEOUT=${RUST_BACKUP_E2E_TRANSFER_TIMEOUT:-60s}
# NAT lab validates candidate nomination and failover, not sustained bandwidth;
# keep it short enough that the deterministic watchdog remains meaningful.
NAT_PAYLOAD_BYTES=${RUST_BACKUP_NAT_PAYLOAD_BYTES:-$((16 * 1024 * 1024))}
cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do kill "$pid" >/dev/null 2>&1 || true; done
  # Delete only namespaces this invocation successfully created.  A stale or
  # foreign name must make setup fail, never be removed by this test.
  for ns in "${NAMESPACES[@]:-}"; do
    ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
    ip netns del "$ns" 2>/dev/null || true
  done
  for ns in "${NAMESPACES[@]:-}"; do
    if ip netns list | awk '{print $1}' | grep -Fxq "$ns"; then
      echo "FAIL: leaked namespace $ns" >&2
      status=1
    fi
  done
  if (( status != 0 || FAIL != 0 )); then find "$work" -name '*.log' -print -exec sed -n '1,180p' {} \; 2>/dev/null || true; fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }

ns_wait_tcp() { # namespace host port
  local ns=$1 host=$2 port=$3
  for _ in $(seq 1 80); do
    if ip netns exec "$ns" python3 - "$host" "$port" <<'PY' >/dev/null 2>&1
import socket, sys
s = socket.socket(); s.settimeout(.2)
try: s.connect((sys.argv[1], int(sys.argv[2])))
except OSError: raise SystemExit(1)
finally: s.close()
PY
    then return 0; fi
    sleep .1
  done
  return 1
}

veth() { # host-end ns-end ns host-ip ns-ip
  local host_if=$1 ns_if=$2 ns=$3 host_ip=$4 ns_ip=$5
  ip link add "$host_if" type veth peer name "$ns_if"
  ip link set "$host_if" netns "$coord"
  ip link set "$ns_if" netns "$ns"
  ip -n "$coord" addr add "$host_ip" dev "$host_if"
  ip -n "$ns" addr add "$ns_ip" dev "$ns_if"
  ip -n "$coord" link set "$host_if" up
  ip -n "$ns" link set "$ns_if" up
}

for ns in "$coord" "$nat" "$src_ns" "$dst_ns"; do
  ip netns add "$ns"
  NAMESPACES+=("$ns")
  ip -n "$ns" link set lo up
done
# Source -- NAT -- coordinator -- destination. NAT makes the direct-path test
# exercise observed mapped candidates instead of two loopback peers.
veth "${tag}cn" cn0 "$nat" 10.70.0.1/24 10.70.0.2/24
ip link add "${tag}ns" type veth peer name sn0
ip link set "${tag}ns" netns "$nat"; ip link set sn0 netns "$src_ns"
ip -n "$nat" addr add 10.71.0.1/24 dev "${tag}ns"; ip -n "$src_ns" addr add 10.71.0.2/24 dev sn0
ip -n "$nat" link set "${tag}ns" up; ip -n "$src_ns" link set sn0 up
veth "${tag}cd" dn0 "$dst_ns" 10.72.0.1/24 10.72.0.2/24
ip -n "$src_ns" route add default via 10.71.0.1
ip -n "$nat" route add default via 10.70.0.1
ip -n "$dst_ns" route add default via 10.72.0.1
ip -n "$coord" route add 10.71.0.0/24 via 10.70.0.2
ip netns exec "$coord" sysctl -qw net.ipv4.ip_forward=1
ip netns exec "$nat" sysctl -qw net.ipv4.ip_forward=1
ip netns exec "$nat" iptables -t nat -A POSTROUTING -o cn0 -j MASQUERADE

port=7835
ip netns exec "$coord" env RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 0.0.0.0 --control-port "$port" --udp >"$work/server.log" 2>&1 &
PIDS+=("$!")
ns_wait_tcp "$src_ns" 10.70.0.1 "$port" || { echo 'ERROR: coordinator not ready' >&2; exit 1; }

run_transfer() { # label initial-udp-block drop-after-direct
  local label=$1 blocked=$2 drop_after=$3
  local src="$work/$label-source" dst="$work/$label-destination"
  mkdir -p "$src" "$dst"
  rb_seed_filesystem_fixture "$src" "$NAT_PAYLOAD_BYTES"
  local before_tree before_atime
  before_tree=$(rb_tree_digest "$src")
  before_atime=$(rb_atime_manifest "$src")
  if [[ "$blocked" == yes ]]; then ip netns exec "$nat" iptables -I FORWARD -p udp -j DROP; fi
  timeout --foreground "$TRANSFER_TIMEOUT" ip netns exec "$src_ns" env RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "10.70.0.1:$port" --channel "$label" --insecure --max-rate 4194304 --root "$src" >"$work/$label-source.log" 2>&1 &
  local spid=$!; PIDS+=("$spid")
  sleep .4
  timeout --foreground "$TRANSFER_TIMEOUT" ip netns exec "$dst_ns" env RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "10.72.0.1:$port" --channel "$label" --insecure --yes --root "$dst" >"$work/$label-destination.log" 2>&1 &
  local dpid=$!; PIDS+=("$dpid")
  if [[ "$drop_after" == yes ]]; then
    for _ in $(seq 1 100); do
      if grep -q 'direct udp connection established' "$work/$label-source.log" && grep -q 'direct udp connection established' "$work/$label-destination.log"; then break; fi
      sleep .1
    done
    if grep -q 'direct udp connection established' "$work/$label-source.log" && grep -q 'direct udp connection established' "$work/$label-destination.log"; then
      pass "$label: direct path established before UDP loss"
      sleep 2
      ip netns exec "$nat" iptables -I FORWARD -p udp -j DROP
    else
      fail "$label: direct path did not establish before UDP loss"
    fi
  fi
  local src_rc=0 dst_rc=0
  wait "$spid" || src_rc=$?
  wait "$dpid" || dst_rc=$?
  ip netns exec "$nat" iptables -F
  if [[ "$drop_after" == yes ]]; then
    # A byte stream that has already delivered a prefix cannot migrate to a new
    # relay stream without sequence acknowledgements and replay. v0.1 explicitly
    # rejects transparent resume of half-applied restores, so active-path loss
    # must fail closed, promptly, and without mutating the source or publishing
    # the active partial file.
    if (( src_rc != 0 && dst_rc != 0 && src_rc != 124 && dst_rc != 124 )); then
      pass "$label: active direct-stream loss fails both peers before watchdog"
    else
      fail "$label: active direct-stream loss was not a prompt bilateral failure"
    fi
    if [[ "$before_tree" == "$(rb_tree_digest "$src")" && "$before_atime" == "$(rb_atime_manifest "$src")" ]]; then
      pass "$label: source tree and atimes remain immutable"
    else
      fail "$label: source tree or atimes changed"
    fi
    if [[ ! -e "$dst/nested/payload.bin" ]]; then
      pass "$label: active partial destination file was removed"
    else
      fail "$label: active partial destination file was published"
    fi
    if grep -q '\[Transfer\].*connection lost' "$work/$label-source.log" && \
       grep -q '\[Transfer\].*connection lost' "$work/$label-destination.log"; then
      pass "$label: both peers report phase-tagged connection loss"
    else
      fail "$label: phase-tagged connection-loss diagnostics missing"
    fi
    return
  fi
  local src_digest dst_digest
  src_digest=$(rb_tree_digest "$src")
  dst_digest=$(rb_tree_digest "$dst")
  if (( src_rc == 0 && dst_rc == 0 )) && [[ "$src_digest" == "$dst_digest" ]] && \
     rb_assert_formal_verification "$work/$label-source.log" "$work/$label-destination.log"; then
    pass "$label: transfer completed with identical tree and persisted read-back proof"
  else
    fail "$label: transfer/tree verification failed"
    printf 'DIAG: %s source_rc=%d destination_rc=%d source_digest=%s destination_digest=%s\n' \
      "$label" "$src_rc" "$dst_rc" "$src_digest" "$dst_digest" >&2
    diff -u <(rb_tree_manifest "$src") <(rb_tree_manifest "$dst") >&2 || true
  fi
}

run_transfer T-NET-DIRECT no no
if grep -q 'direct udp connection established' "$work/T-NET-DIRECT-source.log" && grep -q 'direct udp connection established' "$work/T-NET-DIRECT-destination.log"; then pass 'T-NET-DIRECT used direct UDP on both peers'; else fail 'T-NET-DIRECT did not use direct UDP'; fi

run_transfer T-NET-BLOCKED-RELAY yes no
if ! grep -q 'direct udp connection established' "$work/T-NET-BLOCKED-RELAY-source.log" "$work/T-NET-BLOCKED-RELAY-destination.log" && grep -qE 'using relay|falling back to relay|direct path.*timed out|direct path unavailable' "$work/T-NET-BLOCKED-RELAY-source.log" "$work/T-NET-BLOCKED-RELAY-destination.log"; then pass 'T-NET-BLOCKED-RELAY logged clean relay fallback'; else fail 'T-NET-BLOCKED-RELAY direct/fallback assertion failed'; fi

run_transfer T-NET-MID-LOSS no yes

printf 'T-NET1 summary: PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
(( FAIL == 0 ))
