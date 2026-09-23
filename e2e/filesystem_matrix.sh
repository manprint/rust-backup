#!/usr/bin/env bash
# T-FS-MATRIX: the per-row runner for docs/testing/FILESYSTEM_MATRIX.md.
# Invoke directly: sudo -n /absolute/path/to/script.
#
# Three kinds of row, each with its own tree because the refusals abort a whole
# run:
#   (A) root pass    — every round-trip row, restored with ownership preserved,
#                      compared field by field against the source manifest.
#   (B) user pass    — the same rows that do not need privilege to create,
#                      seeded, sent and restored by an unprivileged account with
#                      --no-preserve-ownership. Printed with a `/user` suffix.
#   (C) refusals     — socket, non-UTF-8 name, tree deeper than 1024.
#   (D) the two rows that are about privilege rather than content: the access
#       time guard (M-FS-32) and extended attributes (M-FS-31).
#   (E) --overwrite (M-FS-OVERWRITE): the root pass restored again onto its own
#       non-empty destination — refused at preflight without the flag, then
#       emptied and restored with it, without following a planted symlink.
#
# Prints one PASS/FAIL/SKIP line per row and a final `MATRIX FS:` summary.
# Exits non-zero on any failure.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
if (( EUID != 0 )); then
  echo 'ERROR: invoke directly with sudo -n /absolute/path/e2e/filesystem_matrix.sh' >&2
  exit 2
fi
rb_build_release

work=$(mktemp -d)
chmod 755 "$work" # `runuser` needs to traverse the per-run fixture directory.
# The checkout may be private to its owner; the unprivileged account must not
# need traversal permission on it, so it gets its own copies.
install -m 0755 "$RB_E2E_BIN" "$work/rust-backup"
install -m 0644 "$(dirname "$0")/lib.sh" "$work/lib.sh"
RB_E2E_BIN="$work/rust-backup"
PIDS=()
DEST_EXTRA=()
PASS=0
FAIL=0
SKIP=0
cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do kill "$pid" >/dev/null 2>&1 || true; done
  if (( status != 0 || FAIL != 0 )); then
    find "$work" -maxdepth 2 -name '*.log' -print -exec sed -n '1,120p' {} \; 2>/dev/null || true
  fi
  # The fixture deliberately contains a 0500 directory and, in the root pass,
  # entries with mode 0000: `rm -rf` cannot descend into them as they stand.
  chmod -R u+rwX "$work" 2>/dev/null || true
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

pass_row() { printf 'PASS %s\n' "$1"; PASS=$((PASS + 1)); }
fail_row() { printf 'FAIL %s: %s\n' "$1" "$2" >&2; FAIL=$((FAIL + 1)); }
skip_row() { printf 'SKIP %s: %s\n' "$1" "$2"; SKIP=$((SKIP + 1)); }

nonroot=nobody
id "$nonroot" >/dev/null 2>&1 || nonroot=$(getent passwd 65534 | cut -d: -f1)

start_server() { # port log
  RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$1" --udp=false >"$2" 2>&1 &
  PIDS+=("$!")
  rb_wait_tcp 127.0.0.1 "$1"
}

# A full transfer of one tree. `run_as` empty means root on both sides.
transfer() { # source dest port channel [run-as-user] [no-preserve]
  local src=$1 dst=$2 port=$3 channel=$4 run_as=${5:-} no_ownership=${6:-}
  local -a source_cmd=("$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port"
    --channel "$channel" --no-udp --insecure --root "$src")
  local -a destination_cmd=("$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port"
    --channel "$channel" --no-udp --insecure --yes --root "$dst")
  if [[ $no_ownership == no-preserve ]]; then
    destination_cmd+=(--no-preserve-ownership)
  fi
  destination_cmd+=(${DEST_EXTRA[@]+"${DEST_EXTRA[@]}"})
  if [[ -n $run_as ]]; then
    runuser -u "$run_as" -- env RUST_LOG=info "${source_cmd[@]}" >"$work/$channel-source.log" 2>&1 &
  else
    RUST_LOG=info "${source_cmd[@]}" >"$work/$channel-source.log" 2>&1 &
  fi
  local spid=$!; PIDS+=("$spid")
  sleep .3
  if [[ -n $run_as ]]; then
    runuser -u "$run_as" -- env RUST_LOG=info "${destination_cmd[@]}" >"$work/$channel-destination.log" 2>&1 &
  else
    RUST_LOG=info "${destination_cmd[@]}" >"$work/$channel-destination.log" 2>&1 &
  fi
  local dpid=$!; PIDS+=("$dpid")
  local source_rc=0 destination_rc=0
  wait "$spid" || source_rc=$?
  wait "$dpid" || destination_rc=$?
  [[ $source_rc -eq 0 && $destination_rc -eq 0 ]] && \
    rb_assert_formal_verification "$work/$channel-source.log" "$work/$channel-destination.log"
}

# A source-only run that must be refused before anything is transferred: the
# message names the offending path, and no destination root is created.
assert_refusal() { # id root dst port channel expected-text [extra source args...]
  local id=$1 src=$2 dst=$3 port=$4 channel=$5 expected=$6; shift 6
  local rc=0
  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" \
    --channel "$channel" --no-udp --insecure --root "$src" "$@" \
    >"$work/$channel-source.log" 2>&1 || rc=$?
  if (( rc == 0 )); then
    fail_row "$id" 'the run was accepted'
  elif ! grep -qF "$expected" "$work/$channel-source.log"; then
    fail_row "$id" "the refusal does not say \"$expected\""
  elif [[ -e $dst ]]; then
    fail_row "$id" 'the refused run created the destination root'
  else
    pass_row "$id"
  fi
}

# One row is the set of manifest lines whose path starts with its prefix. The
# comparison is field by field (kind, mode, uid, gid, mtime_ns, size, hardlink
# group, device number, content digest or link target), so a failure names the
# row rather than the whole tree.
compare_row() { # id prefix source-manifest destination-manifest [ignore-ownership]
  local id=$1 prefix=$2 src=$3 dst=$4 ignore=${5:-}
  local filter='$2 ~ p'
  local fields='$0'
  if [[ $ignore == ignore-ownership ]]; then
    fields='$1"\t"$2"\t"$3"\t"$6"\t"$7"\t"$8"\t"$9"\t"$10'
  fi
  local left right
  left=$(awk -F'\t' -v p="^${prefix}" "$filter {print $fields}" "$src")
  right=$(awk -F'\t' -v p="^${prefix}" "$filter {print $fields}" "$dst")
  if [[ -z $left ]]; then
    fail_row "$id" "no source entry matches $prefix"
  elif [[ $left == "$right" ]]; then
    pass_row "$id"
  else
    fail_row "$id" 'source and destination differ'
    diff <(printf '%s\n' "$left") <(printf '%s\n' "$right") >&2 || true
  fi
}

# Every round-trip row, with the prefix its entries carry. The `root` list is
# the subset that needs privilege to create.
ROWS_COMMON=(01 02 03 04 05 06 07 08 11 14 15 16 17 18 19 20 21 22 23 24 30)
ROWS_ROOT=(09 10 12 13 25 26)

# --- (A) root pass ----------------------------------------------------------
root_src="$work/root-source"; root_dst="$work/root-destination"
mkdir -p "$root_src"
rb_seed_filesystem_matrix_fixture "$root_src" root
port=$(rb_free_port)
if start_server "$port" "$work/root-server.log" && transfer "$root_src" "$root_dst" "$port" fs-matrix-root; then
  rb_tree_manifest "$root_src" >"$work/root-source.manifest"
  rb_tree_manifest "$root_dst" >"$work/root-destination.manifest"
  for row in "${ROWS_COMMON[@]}" "${ROWS_ROOT[@]}"; do
    compare_row "M-FS-$row" "m${row}_" "$work/root-source.manifest" "$work/root-destination.manifest"
  done
  # M-FS-30 also has to be identical byte for byte, not only by digest: the
  # sparse file is restored dense, which changes disk usage but nothing else.
  if cmp -s "$root_src/m30_sparse.bin" "$root_dst/m30_sparse.bin"; then
    pass_row 'M-FS-30/content'
  else
    fail_row 'M-FS-30/content' 'the restored sparse file differs byte for byte'
  fi
else
  fail_row 'M-FS-root-pass' 'the privileged transfer failed'
  for row in "${ROWS_COMMON[@]}" "${ROWS_ROOT[@]}"; do
    skip_row "M-FS-$row" 'the privileged transfer failed'
  done
fi

# --- (E) --overwrite onto the non-empty root destination ---------------------
if [[ -d $root_dst ]]; then
  outside="$work/overwrite-outside"
  mkdir -p "$outside"
  echo keep >"$outside/keep"
  echo stale >"$root_dst/stale-marker"
  ln -s "$outside" "$root_dst/stale-link"
  port=$(rb_free_port)
  start_server "$port" "$work/overwrite-server.log"
  if transfer "$root_src" "$root_dst" "$port" fs-matrix-no-overwrite; then
    fail_row 'M-FS-OVERWRITE/refused' 'a non-empty root was restored without --overwrite'
  elif grep -qF 'pass --overwrite' "$work/fs-matrix-no-overwrite-destination.log" &&
      [[ -e $root_dst/stale-marker ]]; then
    pass_row 'M-FS-OVERWRITE/refused'
  else
    fail_row 'M-FS-OVERWRITE/refused' 'the refusal does not name --overwrite or touched the root'
  fi
  DEST_EXTRA=(--overwrite)
  if transfer "$root_src" "$root_dst" "$port" fs-matrix-overwrite; then
    rb_tree_manifest "$root_dst" >"$work/overwrite-destination.manifest"
    if diff -q "$work/root-source.manifest" "$work/overwrite-destination.manifest" >/dev/null &&
        [[ ! -e $root_dst/stale-marker && ! -L $root_dst/stale-link ]]; then
      pass_row 'M-FS-OVERWRITE/restored'
    else
      fail_row 'M-FS-OVERWRITE/restored' 'the overwritten root differs from the source tree'
    fi
    if [[ $(cat "$outside/keep" 2>/dev/null) == keep ]]; then
      pass_row 'M-FS-OVERWRITE/symlink-not-followed'
    else
      fail_row 'M-FS-OVERWRITE/symlink-not-followed' 'the target of a planted symlink was deleted'
    fi
  else
    fail_row 'M-FS-OVERWRITE/restored' 'the --overwrite transfer failed'
  fi
  DEST_EXTRA=()
else
  skip_row 'M-FS-OVERWRITE' 'the privileged transfer failed'
fi

# --- (B) unprivileged pass --------------------------------------------------
# Seeded by the account that will read it, so the source owns every file and
# `O_NOATIME` is permitted; ownership is explicitly out of the contract.
user_src="$work/user-source"; user_dst="$work/user-destination"
mkdir -p "$user_src" "$user_dst"
chown "$nonroot" "$user_src" "$user_dst"
runuser -u "$nonroot" -- bash -c "source '$work/lib.sh'; rb_seed_filesystem_matrix_fixture '$user_src' user"
port=$(rb_free_port)
if start_server "$port" "$work/user-server.log" && \
   transfer "$user_src" "$user_dst" "$port" fs-matrix-user "$nonroot" no-preserve; then
  runuser -u "$nonroot" -- bash -c "source '$work/lib.sh'; rb_tree_manifest '$user_src'" \
    >"$work/user-source.manifest"
  runuser -u "$nonroot" -- bash -c "source '$work/lib.sh'; rb_tree_manifest '$user_dst'" \
    >"$work/user-destination.manifest"
  for row in "${ROWS_COMMON[@]}"; do
    compare_row "M-FS-$row/user" "m${row}_" \
      "$work/user-source.manifest" "$work/user-destination.manifest" ignore-ownership
  done
  for row in "${ROWS_ROOT[@]}"; do
    skip_row "M-FS-$row/user" 'the entry needs privilege to create'
  done
else
  fail_row 'M-FS-user-pass' 'the unprivileged transfer failed'
fi

# --- (C) refusals -----------------------------------------------------------
socket_src="$work/socket-source"
rb_seed_fs_refusal_socket "$socket_src"
port=$(rb_free_port)
start_server "$port" "$work/socket-server.log"
assert_refusal 'M-FS-27' "$socket_src" "$work/socket-destination" "$port" fs-matrix-socket \
  'a unix socket cannot be reproduced'

nonutf8_src="$work/nonutf8-source"
rb_seed_fs_refusal_nonutf8 "$nonutf8_src"
port=$(rb_free_port)
start_server "$port" "$work/nonutf8-server.log"
assert_refusal 'M-FS-28' "$nonutf8_src" "$work/nonutf8-destination" "$port" fs-matrix-nonutf8 \
  'is not valid UTF-8'

depth_src="$work/depth-source"
rb_seed_fs_refusal_depth "$depth_src"
port=$(rb_free_port)
start_server "$port" "$work/depth-server.log"
assert_refusal 'M-FS-29' "$depth_src" "$work/depth-destination" "$port" fs-matrix-depth \
  'deeper than 1024 levels'

# --- (D) the two privilege rows ---------------------------------------------
# M-FS-31: extended attributes are not restored by this build, so asking for
# them is refused while connecting rather than silently dropped.
port=$(rb_free_port)
start_server "$port" "$work/xattr-server.log"
assert_refusal 'M-FS-31' "$root_src" "$work/xattr-destination" "$port" fs-matrix-xattr \
  'preserve_xattr' --preserve-xattr

# M-FS-32 (T-FS-ATIME): a source that does not own the files cannot use
# O_NOATIME, so reading them would move every access time. Without the opt-in
# the run must fail before transferring anything and leave the atimes alone;
# with it the run completes and says so once.
atime_src="$work/atime-source"; atime_dst="$work/atime-destination"
mkdir -p "$atime_src"
rb_seed_filesystem_fixture "$atime_src" $((64 * 1024))
chown -R 0:0 "$atime_src"
chmod -R a+rX "$atime_src"
find "$atime_src" -type f -exec chmod 0644 {} +
# Order matters: `rb_tree_digest` opens and reads every regular file, so it
# moves their access times. Snapshot the atimes AFTER it, or the comparison
# below reports the digest's own reads as drift caused by the run.
before_tree=$(rb_tree_digest "$atime_src")
before_atime=$(rb_atime_manifest "$atime_src")
port=$(rb_free_port)
start_server "$port" "$work/atime-server.log"
atime_rc=0
runuser -u "$nonroot" -- env RUST_LOG=info "$RB_E2E_BIN" filesystem source \
  --to "127.0.0.1:$port" --channel fs-matrix-atime --no-udp --insecure \
  --root "$atime_src" >"$work/fs-matrix-atime-source.log" 2>&1 || atime_rc=$?
if (( atime_rc == 0 )); then
  fail_row 'M-FS-32' 'a non-owner source ran without accepting atime updates'
elif ! grep -q 'without updating its access time' "$work/fs-matrix-atime-source.log"; then
  fail_row 'M-FS-32' 'the atime refusal lacks the documented diagnostic'
elif [[ "$before_atime" != "$(rb_atime_manifest "$atime_src")" ]]; then
  fail_row 'M-FS-32' 'the refused run moved the source atimes anyway'
elif [[ -e $atime_dst ]]; then
  fail_row 'M-FS-32' 'the refused run created the destination root'
else
  mkdir -p "$atime_dst"
  chown "$nonroot" "$atime_dst"
  port=$(rb_free_port)
  start_server "$port" "$work/atime-accepted-server.log"
  runuser -u "$nonroot" -- env RUST_LOG=info "$RB_E2E_BIN" filesystem source \
    --to "127.0.0.1:$port" --channel fs-matrix-atime-ok --no-udp --insecure \
    --allow-atime-updates --root "$atime_src" >"$work/fs-matrix-atime-ok-source.log" 2>&1 &
  spid=$!; PIDS+=("$spid")
  sleep .3
  runuser -u "$nonroot" -- env RUST_LOG=info "$RB_E2E_BIN" filesystem destination \
    --to "127.0.0.1:$port" --channel fs-matrix-atime-ok --no-udp --insecure --yes \
    --no-preserve-ownership --root "$atime_dst" >"$work/fs-matrix-atime-ok-destination.log" 2>&1 &
  dpid=$!; PIDS+=("$dpid")
  accepted_source_rc=0; accepted_destination_rc=0
  wait "$spid" || accepted_source_rc=$?
  wait "$dpid" || accepted_destination_rc=$?
  if (( accepted_source_rc != 0 || accepted_destination_rc != 0 )); then
    fail_row 'M-FS-32' 'the accepted-atime run failed'
  elif ! grep -q 'atime updates on the source accepted by --allow-atime-updates' \
      "$work/fs-matrix-atime-ok-source.log"; then
    fail_row 'M-FS-32' 'the accepted-atime run did not warn'
  elif ! rb_assert_formal_verification "$work/fs-matrix-atime-ok-source.log" \
      "$work/fs-matrix-atime-ok-destination.log"; then
    fail_row 'M-FS-32' 'the accepted-atime run printed no formal verification'
  elif [[ "$before_tree" != "$(rb_tree_digest "$atime_src")" ]]; then
    fail_row 'M-FS-32' 'the accepted-atime run changed the source tree'
  else
    pass_row 'M-FS-32'
  fi
fi

# M-FS-25/26 again, from the other side: a destination without CAP_MKNOD must
# say so at preflight instead of failing halfway through the restore.
mknod_dst="$work/mknod-destination"
mkdir -p "$mknod_dst"
chown "$nonroot" "$mknod_dst"
port=$(rb_free_port)
if start_server "$port" "$work/mknod-server.log"; then
  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" \
    --channel fs-matrix-mknod --no-udp --insecure --root "$root_src" \
    >"$work/fs-matrix-mknod-source.log" 2>&1 &
  spid=$!; PIDS+=("$spid")
  sleep .3
  runuser -u "$nonroot" -- env RUST_LOG=info "$RB_E2E_BIN" filesystem destination \
    --to "127.0.0.1:$port" --channel fs-matrix-mknod --no-udp --insecure --yes \
    --no-preserve-ownership --root "$mknod_dst" \
    >"$work/fs-matrix-mknod-destination.log" 2>&1 &
  dpid=$!; PIDS+=("$dpid")
  mknod_destination_rc=0
  # The source's own exit code does not decide this row: what is under test is
  # the destination's preflight, and the source fails too once it is refused.
  wait "$spid" >/dev/null 2>&1 || true
  wait "$dpid" || mknod_destination_rc=$?
  if (( mknod_destination_rc == 0 )); then
    fail_row 'M-FS-25/26-privilege' 'device nodes were accepted without CAP_MKNOD'
  elif ! grep -q 'needs root or CAP_MKNOD' "$work/fs-matrix-mknod-destination.log"; then
    fail_row 'M-FS-25/26-privilege' 'the rejection lacks a precise diagnostic'
  elif [[ -n "$(ls -A "$mknod_dst")" ]]; then
    fail_row 'M-FS-25/26-privilege' 'the refused restore wrote into the destination root'
  else
    pass_row 'M-FS-25/26-privilege'
  fi
else
  fail_row 'M-FS-25/26-privilege' 'the server did not start'
fi

printf 'MATRIX FS: %d pass, %d fail, %d skip\n' "$PASS" "$FAIL" "$SKIP"
(( FAIL == 0 ))
