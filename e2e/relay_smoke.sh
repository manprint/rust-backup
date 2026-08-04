#!/usr/bin/env bash
# T-E2E0: filesystem source -> destination through the TCP relay only.
# Needs cargo + Python 3. No Docker, sudo, or network namespace.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
rb_build_release

work=$(mktemp -d)
server_pid=
source_pid=
destination_pid=
PASS=0
FAIL=0
cleanup() {
  status=$?
  for pid in "${destination_pid:-}" "${source_pid:-}" "${server_pid:-}"; do
    [[ -n "$pid" ]] && kill "$pid" >/dev/null 2>&1 || true
  done
  if (( status != 0 || FAIL != 0 )); then
    for log in server source destination; do
      printf -- '--- %s log ---\n' "$log" >&2
      sed -n '1,200p' "$work/$log.log" >&2 2>/dev/null || true
    done
  fi
  if [[ ${RUST_BACKUP_E2E_KEEP:-0} == 1 ]]; then
    printf 'kept e2e workdir: %s\n' "$work" >&2
  else
    rm -rf "$work"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }

src="$work/source"
dst="$work/destination"
mkdir -p "$src" "$dst"
rb_seed_filesystem_fixture "$src" $((3 * 1024 * 1024 + 71))
before=$(rb_tree_digest "$src")
port=$(rb_free_port)
endpoint="127.0.0.1:$port"
carriers=${RUST_BACKUP_E2E_CARRIERS:-1}
server_tls=()
if [[ ${RUST_BACKUP_E2E_TLS:-0} == 1 ]]; then
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -keyout "$work/server.key" -out "$work/server.crt" \
    -subj '/CN=localhost' >/dev/null 2>&1
  server_tls=(--tls-cert "$work/server.crt" --tls-key "$work/server.key")
  endpoint="https://127.0.0.1:$port"
fi

RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$port" --udp=false "${server_tls[@]}" >"$work/server.log" 2>&1 &
server_pid=$!
if rb_wait_tcp 127.0.0.1 "$port"; then pass 'relay server accepts TCP'; else fail 'relay server did not start'; fi

if (( FAIL == 0 )); then
  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "$endpoint" --channel relay-smoke \
    --no-udp --insecure --carriers "$carriers" --root "$src" >"$work/source.log" 2>&1 &
  source_pid=$!
  sleep .2
  RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "$endpoint" --channel relay-smoke \
    --no-udp --insecure --carriers "$carriers" --yes --root "$dst" >"$work/destination.log" 2>&1 &
  destination_pid=$!
  if wait "$source_pid" && wait "$destination_pid" && \
     rb_assert_formal_verification "$work/source.log" "$work/destination.log"; then
    pass 'relay transfer completed with persisted read-back proof'
  else fail 'relay transfer or formal verification failed'; fi
fi

if (( FAIL == 0 )) && [[ "$(rb_tree_digest "$src")" == "$(rb_tree_digest "$dst")" ]]; then
  pass 'destination tree preserves bytes, modes, mtimes, links, ownership'
else
  fail 'destination tree differs from source'
fi
if [[ "$before" == "$(rb_tree_digest "$src")" ]]; then pass 'source tree unchanged'; else fail 'source tree changed'; fi

printf 'T-E2E0%s carriers=%s summary: PASS=%d FAIL=%d\n' "${RUST_BACKUP_E2E_TLS:+ TLS}" "$carriers" "$PASS" "$FAIL"
(( FAIL == 0 ))
