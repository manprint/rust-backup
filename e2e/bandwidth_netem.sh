#!/usr/bin/env bash
# T-BW: relay backpressure under 5 Mbit/s + 80 ms netem, RSS cap, --max-rate.
# Invoke directly with sudo -n /absolute/path/e2e/bandwidth_netem.sh.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
if (( EUID != 0 )); then echo 'ERROR: run with exact-path sudo -n' >&2; exit 2; fi
for command in ip tc python3 awk; do command -v "$command" >/dev/null || { echo "ERROR: missing $command" >&2; exit 2; }; done
rb_build_release

tag="rbbw$$"; coord="${tag}c"; src_ns="${tag}s"; dst_ns="${tag}d"
work=$(mktemp -d)
PIDS=()
PASS=0; FAIL=0
PAYLOAD_BYTES=${RUST_BACKUP_BANDWIDTH_BYTES:-$((200 * 1024 * 1024))}
RATE_TEST_BYTES=${RUST_BACKUP_RATE_TEST_BYTES:-$((16 * 1024 * 1024))}
RSS_LIMIT_KIB=${RUST_BACKUP_RSS_LIMIT_KIB:-196608}
carriers=${RUST_BACKUP_E2E_CARRIERS:-1}
cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do kill "$pid" >/dev/null 2>&1 || true; done
  for ns in "$coord" "$src_ns" "$dst_ns"; do ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; ip netns del "$ns" 2>/dev/null || true; done
  if (( status != 0 || FAIL != 0 )); then find "$work" -name '*.log' -print -exec sed -n '1,160p' {} \; 2>/dev/null || true; fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }

for ns in "$coord" "$src_ns" "$dst_ns"; do ip netns add "$ns"; ip -n "$ns" link set lo up; done
link() { # coord-if peer-if peer-ns coord-ip peer-ip
  ip link add "$1" type veth peer name "$2"; ip link set "$1" netns "$coord"; ip link set "$2" netns "$3"
  ip -n "$coord" addr add "$4" dev "$1"; ip -n "$3" addr add "$5" dev "$2"; ip -n "$coord" link set "$1" up; ip -n "$3" link set "$2" up
}
link "${tag}cs" src0 "$src_ns" 10.81.0.1/24 10.81.0.2/24
link "${tag}cd" dst0 "$dst_ns" 10.82.0.1/24 10.82.0.2/24
ip -n "$src_ns" route add default via 10.81.0.1; ip -n "$dst_ns" route add default via 10.82.0.1
ip netns exec "$coord" sysctl -qw net.ipv4.ip_forward=1
# All relay -> destination traffic passes through this constrained receive link.
ip netns exec "$dst_ns" tc qdisc replace dev dst0 root handle 1: tbf rate 5mbit burst 64k latency 400ms
ip netns exec "$dst_ns" tc qdisc add dev dst0 parent 1:1 handle 10: netem delay 80ms

port=7835
ip netns exec "$coord" env RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 0.0.0.0 --control-port "$port" --udp=false >"$work/server.log" 2>&1 &
PIDS+=("$!")
sleep .5

run_case() { # label bytes max-rate-or-empty
  local label=$1 bytes=$2 max_rate=$3 src="$work/$1-source" dst="$work/$1-destination"
  mkdir -p "$src" "$dst"; rb_seed_filesystem_fixture "$src" "$bytes"
  local before elapsed started spid dpid monitor max_rss=0 rc=0
  before=$(rb_tree_digest "$src"); started=$(date +%s)
  local rate_args=()
  [[ -n "$max_rate" ]] && rate_args=(--max-rate "$max_rate")
  ip netns exec "$src_ns" env RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "10.81.0.1:$port" --channel "$label" --no-udp --insecure --carriers "$carriers" "${rate_args[@]}" --root "$src" >"$work/$label-source.log" 2>&1 &
  spid=$!; PIDS+=("$spid")
  sleep .3
  ip netns exec "$dst_ns" env RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "10.82.0.1:$port" --channel "$label" --no-udp --insecure --carriers "$carriers" --yes --root "$dst" >"$work/$label-destination.log" 2>&1 &
  dpid=$!; PIDS+=("$dpid")
  ( while kill -0 "$spid" >/dev/null 2>&1; do awk '/VmRSS:/ {print $2}' "/proc/$spid/status" 2>/dev/null || true; sleep 1; done ) >"$work/$label-rss.log" &
  monitor=$!
  wait "$spid" || rc=1; wait "$dpid" || rc=1; wait "$monitor" || true
  elapsed=$(( $(date +%s) - started )); max_rss=$(sort -n "$work/$label-rss.log" | tail -1); max_rss=${max_rss:-0}
  if (( rc == 0 )) && [[ "$before" == "$(rb_tree_digest "$src")" ]] && [[ "$(rb_tree_digest "$src")" == "$(rb_tree_digest "$dst")" ]]; then pass "$label: transfer and digest verification"; else fail "$label: transfer or digest verification"; fi
  if (( max_rss <= RSS_LIMIT_KIB )); then pass "$label: source RSS ${max_rss} KiB <= ${RSS_LIMIT_KIB} KiB"; else fail "$label: source RSS ${max_rss} KiB exceeds cap"; fi
  CASE_ELAPSED=$elapsed
  CASE_BYTES=$bytes
  CASE_MAX_RSS=$max_rss
}

run_case T-BW-NETEM "$PAYLOAD_BYTES" ''
elapsed=$CASE_ELAPSED; bytes=$CASE_BYTES; max_rss=$CASE_MAX_RSS
# 5 Mbit/s allows at most ~0.625 MB/s. Leave 30% slack for scheduler/qdisc burst.
minimum=$(( bytes * 8 * 70 / 5000000 / 100 ))
if (( elapsed >= minimum )); then pass "T-BW-NETEM: ${elapsed}s proves constrained receiver pacing"; else fail "T-BW-NETEM: ${elapsed}s is too fast for 5 Mbit/s"; fi

run_case T-BW-MAX-RATE "$RATE_TEST_BYTES" 262144
capped_elapsed=$CASE_ELAPSED; capped_bytes=$CASE_BYTES; capped_rss=$CASE_MAX_RSS
rate_minimum=$(( capped_bytes * 70 / 262144 / 100 ))
if (( capped_elapsed >= rate_minimum )); then pass "T-BW-MAX-RATE: ${capped_elapsed}s visibly honors 256 KiB/s cap"; else fail "T-BW-MAX-RATE: ${capped_elapsed}s bypassed cap"; fi

printf 'T-BW summary: PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
(( FAIL == 0 ))
