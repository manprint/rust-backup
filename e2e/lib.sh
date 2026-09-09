#!/usr/bin/env bash
# Shared Linux-only helpers for rust-backup end-to-end scripts.

set -u

RB_E2E_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RB_E2E_BIN=${RUST_BACKUP_BIN:-"$RB_E2E_ROOT/target/release/rust-backup"}

rb_build_release() {
  if [[ ! -x "$RB_E2E_BIN" ]] || find "$RB_E2E_ROOT/Cargo.toml" "$RB_E2E_ROOT/crates" \
      -type f -newer "$RB_E2E_BIN" -print -quit | grep -q .; then
    if (( EUID == 0 )) && [[ -n ${SUDO_USER:-} && $SUDO_USER != root ]]; then
      # Exact-path sudo runs with secure_path and normally hides rustup/cargo.
      # Build as the invoking user: this both finds their toolchain and avoids
      # leaving root-owned artifacts in a developer checkout.
      local invoking_home cargo_bin
      invoking_home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
      cargo_bin="$invoking_home/.cargo/bin/cargo"
      if [[ -z "$invoking_home" || ! -x "$cargo_bin" ]]; then
        echo "ERROR: cargo toolchain not found for sudo user $SUDO_USER" >&2
        return 2
      fi
      runuser -u "$SUDO_USER" -- env \
        HOME="$invoking_home" CARGO_HOME="$invoking_home/.cargo" \
        RUSTUP_HOME="$invoking_home/.rustup" \
        "$cargo_bin" build --manifest-path "$RB_E2E_ROOT/Cargo.toml" \
        --release --all-features
    else
      (cd "$RB_E2E_ROOT" && cargo build --release --all-features)
    fi
  fi
}

rb_free_port() {
  python3 - <<'PY'
import socket
sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

rb_wait_tcp() { # host port [attempts]
  local host=$1 port=$2 attempts=${3:-50}
  for _ in $(seq 1 "$attempts"); do
    if python3 - "$host" "$port" <<'PY' >/dev/null 2>&1
import socket, sys
sock = socket.socket()
sock.settimeout(.2)
try:
    sock.connect((sys.argv[1], int(sys.argv[2])))
except OSError:
    raise SystemExit(1)
finally:
    sock.close()
PY
    then return 0; fi
    sleep .1
  done
  return 1
}

# A successful CLI exit is not sufficient: require the protocol's persisted
# read-back proof, matching source/destination commitment, and a terminal 100%
# progress record on both peers.
rb_strip_ansi() {
  sed $'s/\033\[[0-9;]*[mK]//g'
}

rb_assert_formal_verification() { # source-log destination-log
  local source_log=$1 destination_log=$2 source_digest destination_digest
  rb_strip_ansi <"$source_log" | grep 'BACKUP VERIFIED: source unchanged; destination read-back matches' >/dev/null || {
    echo "FAIL: source log has no formal BACKUP VERIFIED evidence: $source_log" >&2
    return 1
  }
  rb_strip_ansi <"$destination_log" | grep 'RESTORE VERIFIED: persisted destination matches source' >/dev/null || {
    echo "FAIL: destination log has no formal RESTORE VERIFIED evidence: $destination_log" >&2
    return 1
  }
  for log in "$source_log" "$destination_log"; do
    rb_strip_ansi <"$log" | grep -E 'status="?verified"?.*\(100\.0%\)|\(100\.0%\).*status="?verified"?' >/dev/null || {
      echo "FAIL: no terminal verified 100% progress record: $log" >&2
      return 1
    }
  done
  source_digest=$(rb_strip_ansi <"$source_log" | grep 'BACKUP VERIFIED:' | tail -n1 | grep -oE 'blake3=[0-9a-f]{64}' | cut -d= -f2 || true)
  destination_digest=$(rb_strip_ansi <"$destination_log" | grep 'RESTORE VERIFIED:' | tail -n1 | grep -oE 'blake3=[0-9a-f]{64}' | cut -d= -f2 || true)
  if [[ -z $source_digest || $source_digest != "$destination_digest" ]]; then
    echo "FAIL: source/destination verification commitments differ" >&2
    return 1
  fi
}

# Both peers must agree on the carrier count they actually used. Requesting
# `--carriers 4` is not proof: negotiation legitimately downgrades (a module cap,
# the peer's own channel), and a silent fallback to one carrier would let a
# multi-carrier case pass while exercising a single stream. The session logs the
# agreed count on both sides for exactly this reason.
rb_assert_carriers() { # want source-log destination-log
  local want=$1 log agreed
  shift
  for log in "$@"; do
    # Anchored on the negotiation message, not on any `carriers=` in the log:
    # a plan render or a future field of the same name must not be able to
    # satisfy this assertion.
    agreed=$(rb_strip_ansi <"$log" | grep 'negotiated data plane' | tail -n1 \
      | grep -oE 'carriers=[0-9]+' | cut -d= -f2)
    if [[ -z ${agreed:-} ]]; then
      echo "FAIL: no negotiated carrier count logged: $log" >&2
      return 1
    fi
    if [[ $agreed != "$want" ]]; then
      echo "FAIL: negotiated carriers=$agreed, expected $want: $log" >&2
      return 1
    fi
  done
}

# Validate a combined `run --config` log without relying on grep's locale,
# binary-file heuristics, or one-event-per-line behaviour.  Parallel targets
# may write their records in any order, so compare the multisets of formal
# commitments and require one terminal progress record per peer.
rb_assert_session_formal_verification() { # combined-log expected-pairs
  local combined_log=$1 expected_pairs=$2
  python3 - "$combined_log" "$expected_pairs" <<'PY'
from collections import Counter
import re
import sys

path = sys.argv[1]
expected = int(sys.argv[2])
with open(path, "rb") as stream:
    text = stream.read().decode("utf-8", "replace")
text = re.sub(r"\x1b\[[0-9;]*[mK]", "", text)

digest = r"([0-9a-f]{64})"
source = re.findall(
    r"BACKUP VERIFIED: source unchanged; destination read-back matches"
    r"[^\r\n]*?blake3=" + digest,
    text,
)
destination = re.findall(
    r"RESTORE VERIFIED: persisted destination matches source"
    r"[^\r\n]*?blake3=" + digest,
    text,
)
progress = re.findall(
    r"\(100\.0%\)[^\r\n]*?target_label=\S+/(Source|Destination)"
    r"[^\r\n]*?status=\"?verified\"?",
    text,
)
source_progress = progress.count("Source")
destination_progress = progress.count("Destination")

if (
    len(source) != expected
    or len(destination) != expected
    or Counter(source) != Counter(destination)
    or source_progress != expected
    or destination_progress != expected
):
    print(
        "FAIL: incomplete session proof: "
        f"source_evidence={len(source)}/{expected} "
        f"destination_evidence={len(destination)}/{expected} "
        f"source_progress={source_progress}/{expected} "
        f"destination_progress={destination_progress}/{expected} "
        f"commitments_match={Counter(source) == Counter(destination)}",
        file=sys.stderr,
    )
    raise SystemExit(1)
PY
}

# Stable tree digest: content, type, mode, ownership, mtime, and link topology.
# Atime is deliberately excluded: callers needing I-IMMUT use rb_atime_manifest.
rb_tree_digest() {
  python3 - "$1" <<'PY'
import hashlib, os, stat, sys
root = os.path.abspath(sys.argv[1])
h = hashlib.sha256()
hardlinks = {}
next_hardlink = 1
for base, dirs, names in os.walk(root, topdown=True, followlinks=False):
    dirs.sort(); names.sort()
    for name in dirs + names:
        path = os.path.join(base, name)
        rel = os.path.relpath(path, root).encode()
        st = os.lstat(path)
        if stat.S_ISREG(st.st_mode):
            kind = b'f'
        elif stat.S_ISDIR(st.st_mode):
            kind = b'd'
        elif stat.S_ISLNK(st.st_mode):
            kind = b'l'
        else:
            kind = b'?'
        hardlink = b'-'
        if kind == b'f' and st.st_nlink > 1:
            key = (st.st_dev, st.st_ino)
            if key not in hardlinks:
                hardlinks[key] = next_hardlink
                next_hardlink += 1
            hardlink = str(hardlinks[key]).encode()
        header = b'\0'.join((kind, rel, str(stat.S_IMODE(st.st_mode)).encode(),
            str(st.st_uid).encode(), str(st.st_gid).encode(), str(st.st_mtime_ns).encode(),
            str(st.st_size).encode(), hardlink)) + b'\n'
        h.update(header)
        if kind == b'f':
            with open(path, 'rb', buffering=0) as f:
                for chunk in iter(lambda: f.read(1024 * 1024), b''):
                    h.update(chunk)
        elif kind == b'l':
            h.update(os.readlink(path).encode() + b'\n')
print(h.hexdigest())
PY
}

# Human-readable counterpart to rb_tree_digest, emitted only on failures so an
# e2e mismatch identifies the exact metadata/content field instead of a pair of
# opaque hashes.
rb_tree_manifest() {
  python3 - "$1" <<'PY'
import hashlib, os, stat, sys
root = os.path.abspath(sys.argv[1])
hardlinks = {}
next_hardlink = 1
for base, dirs, names in os.walk(root, topdown=True, followlinks=False):
    dirs.sort(); names.sort()
    for name in dirs + names:
        path = os.path.join(base, name)
        rel = os.path.relpath(path, root)
        st = os.lstat(path)
        if stat.S_ISREG(st.st_mode): kind = 'f'
        elif stat.S_ISDIR(st.st_mode): kind = 'd'
        elif stat.S_ISLNK(st.st_mode): kind = 'l'
        else: kind = '?'
        link_group = '-'
        if kind == 'f' and st.st_nlink > 1:
            key = (st.st_dev, st.st_ino)
            if key not in hardlinks:
                hardlinks[key] = next_hardlink
                next_hardlink += 1
            link_group = str(hardlinks[key])
        payload = '-'
        if kind == 'f':
            h = hashlib.sha256()
            with open(path, 'rb', buffering=0) as f:
                for chunk in iter(lambda: f.read(1024 * 1024), b''):
                    h.update(chunk)
            payload = h.hexdigest()
        elif kind == 'l':
            payload = os.readlink(path)
        print('\t'.join((kind, rel, oct(stat.S_IMODE(st.st_mode)), str(st.st_uid),
            str(st.st_gid), str(st.st_mtime_ns), str(st.st_size), link_group, payload)))
PY
}

rb_atime_manifest() {
  python3 - "$1" <<'PY'
import os, sys
root = os.path.abspath(sys.argv[1])
rows = []
for base, dirs, names in os.walk(root, topdown=True, followlinks=False):
    dirs.sort(); names.sort()
    for name in dirs + names:
        path = os.path.join(base, name)
        st = os.lstat(path)
        rows.append((os.path.relpath(path, root), st.st_atime_ns))
for path, atime in rows:
    print(f"{path}\t{atime}")
PY
}

rb_seed_filesystem_fixture() { # root [large bytes]
  local root=$1 large_bytes=${2:-0}
  mkdir -p "$root/nested/empty"
  printf 'rust-backup relay e2e\n' >"$root/nested/hello.txt"
  : >"$root/empty"
  printf 'hard-link payload\n' >"$root/hard-a"
  ln "$root/hard-a" "$root/hard-b"
  ln -s nested/hello.txt "$root/hello-link"
  chmod 4755 "$root/nested/hello.txt"
  chmod 0400 "$root/empty"
  touch -d '2020-02-03 04:05:06 UTC' "$root/nested/hello.txt" "$root/empty"
  if [[ "$large_bytes" -gt 0 ]]; then
    dd if=/dev/urandom of="$root/nested/payload.bin" bs=1M \
      count=$(((large_bytes + 1048575) / 1048576)) status=none
    truncate -s "$large_bytes" "$root/nested/payload.bin"
  fi
}
