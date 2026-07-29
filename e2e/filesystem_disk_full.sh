#!/usr/bin/env bash
# T-FS-ENOSPC: a real bounded filesystem must fail in Apply without leaving the
# active partial file or mutating the source. Invoke with exact-path sudo -n.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
if (( EUID != 0 )); then echo 'ERROR: run with exact-path sudo -n' >&2; exit 2; fi
for command in findmnt losetup mkfs.ext4 mount mountpoint python3 timeout truncate umount; do
  command -v "$command" >/dev/null || { echo "ERROR: missing $command" >&2; exit 2; }
done
rb_build_release

work=$(mktemp -d)
image="$work/destination.ext4"
mount_dir="$work/mount"
source_root="$work/source"
destination_root="$mount_dir/restore"
server_pid='' source_pid='' destination_pid=''
PASS=0 FAIL=0

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  for pid in "${destination_pid:-}" "${source_pid:-}" "${server_pid:-}"; do
    [[ -n "$pid" ]] && kill "$pid" >/dev/null 2>&1 || true
  done
  if mountpoint -q "$mount_dir"; then umount "$mount_dir" || status=1; fi
  if losetup -j "$image" 2>/dev/null | grep -q .; then
    echo "FAIL: loop device leaked for $image" >&2
    status=1
  fi
  if (( status != 0 || FAIL != 0 )); then
    find "$work" -name '*.log' -print -exec sed -n '1,180p' {} \; >&2 2>/dev/null || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM
pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }

mkdir -p "$mount_dir" "$source_root"
truncate -s 8M "$image"
mkfs.ext4 -q -F "$image"
mount -o loop "$image" "$mount_dir"
mkdir -p "$destination_root"
rb_seed_filesystem_fixture "$source_root" $((24 * 1024 * 1024))
before_tree=$(rb_tree_digest "$source_root")
before_atime=$(rb_atime_manifest "$source_root")

port=$(rb_free_port)
RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$port" --udp=false \
  >"$work/server.log" 2>&1 &
server_pid=$!
rb_wait_tcp 127.0.0.1 "$port"

timeout --foreground 45s "$RB_E2E_BIN" filesystem source \
  --to "127.0.0.1:$port" --channel fs-enospc --no-udp --insecure \
  --root "$source_root" >"$work/source.log" 2>&1 &
source_pid=$!
sleep .3
timeout --foreground 45s "$RB_E2E_BIN" filesystem destination \
  --to "127.0.0.1:$port" --channel fs-enospc --no-udp --insecure --yes \
  --root "$destination_root" >"$work/destination.log" 2>&1 &
destination_pid=$!

source_rc=0 destination_rc=0
wait "$source_pid" || source_rc=$?
wait "$destination_pid" || destination_rc=$?

if (( source_rc != 0 && destination_rc != 0 && source_rc != 124 && destination_rc != 124 )); then
  pass 'ENOSPC fails both peers before watchdog'
else
  fail "unexpected exits source=$source_rc destination=$destination_rc"
fi
if grep -Eqi '\[Apply\].*(payload\.bin|item=[0-9]+).*(No space left on device|ENOSPC)' "$work/destination.log"; then
  pass 'destination reports phase-tagged ENOSPC naming the active item'
else
  fail 'destination ENOSPC diagnostic is missing phase or item'
fi
if grep -q 'destination aborted:' "$work/source.log"; then
  pass 'source receives the destination abort reason'
else
  fail 'source did not receive destination abort reason'
fi
if [[ "$before_tree" == "$(rb_tree_digest "$source_root")" && "$before_atime" == "$(rb_atime_manifest "$source_root")" ]]; then
  pass 'source tree and atimes remain immutable after ENOSPC'
else
  fail 'source tree or atimes changed after ENOSPC'
fi
if [[ ! -e "$destination_root/nested/payload.bin" ]]; then
  pass 'active partial destination file is removed after ENOSPC'
else
  fail 'active partial destination file remains after ENOSPC'
fi

printf 'T-FS-ENOSPC summary: PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
(( FAIL == 0 ))
