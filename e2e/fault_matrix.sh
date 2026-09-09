#!/usr/bin/env bash
# T-FAULT: non-privileged fault smoke bank.  Sudo-only disk/netns cases remain
# deliberately out of this script; this covers protocol abort and relay loss.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
rb_build_release

work=$(mktemp -d)
server_pid='' source_pid='' destination_pid=''
PASS=0; FAIL=0
cleanup() {
  local status=$?
  for pid in "${destination_pid:-}" "${source_pid:-}" "${server_pid:-}"; do
    [[ -n "$pid" ]] && kill "$pid" >/dev/null 2>&1 || true
  done
  if (( status != 0 || FAIL != 0 )); then
    find "$work" -name '*.log' -exec sh -c 'echo --- "$1"; sed -n "1,120p" "$1"' _ {} \; >&2 2>/dev/null || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }

# Deterministic in-process faults: the 26-case wire bank plus session-level
# rejection, source-error, completion and immutability paths.
if cargo test -p rb-core --test fault_injection --test session_test >/dev/null; then
  pass '26-case core protocol bank is phase-tagged and source immutability is audited'
else
  fail 'core protocol fault bank failed'
fi

src="$work/source"; dst="$work/destination"
mkdir -p "$src" "$dst"
rb_seed_filesystem_fixture "$src" $((32 * 1024 * 1024))
before=$(rb_tree_digest "$src")
port=$(rb_free_port)
RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$port" --udp=false >"$work/server.log" 2>&1 &
server_pid=$!
if ! rb_wait_tcp 127.0.0.1 "$port"; then fail 'fault relay did not start'; fi

if (( FAIL == 0 )); then
  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" --channel fault-kill-source \
    --no-udp --insecure --max-rate 262144 --root "$src" >"$work/source.log" 2>&1 &
  source_pid=$!
  sleep .2
  RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port" --channel fault-kill-source \
    --no-udp --insecure --yes --root "$dst" >"$work/destination.log" 2>&1 &
  destination_pid=$!
  sleep 1
  kill -9 "$source_pid" >/dev/null 2>&1 || true
  set +e
  wait "$source_pid"; source_rc=$?
  wait "$destination_pid"; destination_rc=$?
  set -e
  if (( source_rc != 0 && destination_rc != 0 )); then
    pass 'live source kill aborts both sides without false success'
  else
    fail "live source kill returned source=$source_rc destination=$destination_rc"
  fi
fi

if [[ "$before" == "$(rb_tree_digest "$src")" ]]; then
  pass 'live fault leaves source tree unchanged'
else
  fail 'live fault mutated source tree'
fi

# Documented exit codes (docs/QA_GUIDE.md): a caller scripting rust-backup
# branches on them, so each class must actually reach the process status. They
# used to be collapsed to 1 for every target of a parallel session.
expect_exit() { # code description -- command...
  local want="$1" description="$2"; shift 3
  set +e
  "$@" >"$work/exit.log" 2>&1
  local got=$?
  set -e
  if (( got == want )); then
    pass "exit code $want: $description"
  else
    fail "exit code for $description was $got, expected $want"
    sed -n '1,10p' "$work/exit.log" >&2
  fi
}

# 2 = configuration: an unknown module never reaches a transport.
expect_exit 2 'unknown module' -- "$RB_E2E_BIN" nosuchmodule source --to 127.0.0.1:1 --channel x
# 2 = configuration: a target without --to.
expect_exit 2 'missing --to' -- "$RB_E2E_BIN" filesystem source --channel x --root "$src"
# 3 = preflight: the filesystem destination refuses a root that is not empty.
mkdir -p "$work/occupied" && : >"$work/occupied/pre-existing"
expect_exit 3 'non-empty filesystem destination' -- \
  "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port" --channel exit-preflight \
  --no-udp --insecure --yes --root "$work/occupied"
# 7 = transport: nothing is listening on a closed port.
closed=$(rb_free_port)
expect_exit 7 'unreachable coordination server' -- \
  "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$closed" --channel exit-transport \
  --no-udp --insecure --root "$src"

printf 'T-FAULT summary: PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
(( FAIL == 0 ))
