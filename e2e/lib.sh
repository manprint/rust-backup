#!/usr/bin/env bash
# Shared Linux-only helpers for rust-backup end-to-end scripts.

set -u

RB_E2E_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RB_E2E_BIN=${RUST_BACKUP_BIN:-"$RB_E2E_ROOT/target/release/rust-backup"}

rb_build_release() {
  if [[ ! -x "$RB_E2E_BIN" ]] || find "$RB_E2E_ROOT/Cargo.toml" "$RB_E2E_ROOT/crates" \
      -type f -newer "$RB_E2E_BIN" -print -quit | grep -q .; then
    (cd "$RB_E2E_ROOT" && cargo build --release --all-features)
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
