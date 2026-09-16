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
        elif stat.S_ISFIFO(st.st_mode):
            kind = b'p'
        elif stat.S_ISCHR(st.st_mode):
            kind = b'c'
        elif stat.S_ISBLK(st.st_mode):
            kind = b'b'
        else:
            kind = b'?'
        # A device node is its device number: same path and mode with a
        # different major/minor is a different device.
        rdev = str(st.st_rdev).encode() if kind in (b'c', b'b') else b'-'
        hardlink = b'-'
        if kind == b'f' and st.st_nlink > 1:
            key = (st.st_dev, st.st_ino)
            if key not in hardlinks:
                hardlinks[key] = next_hardlink
                next_hardlink += 1
            hardlink = str(hardlinks[key]).encode()
        header = b'\0'.join((kind, rel, str(stat.S_IMODE(st.st_mode)).encode(),
            str(st.st_uid).encode(), str(st.st_gid).encode(), str(st.st_mtime_ns).encode(),
            str(st.st_size).encode(), hardlink, rdev)) + b'\n'
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
        elif stat.S_ISFIFO(st.st_mode): kind = 'p'
        elif stat.S_ISCHR(st.st_mode): kind = 'c'
        elif stat.S_ISBLK(st.st_mode): kind = 'b'
        else: kind = '?'
        rdev = str(st.st_rdev) if kind in ('c', 'b') else '-'
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
            str(st.st_gid), str(st.st_mtime_ns), str(st.st_size), link_group, rdev, payload)))
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

# The `docs/testing/FILESYSTEM_MATRIX.md` fixture: one deterministic group of
# entries per row, named after the row (`m04_setuid`, `m19_hard_a`, ...), so a
# failing digest can be read back to the case it belongs to. `mode` is `root`
# or `user`: the rows that need privilege to create (modes 0000, foreign
# owners, device nodes — 09, 10, 12, 13, 25, 26) are seeded only in `root`
# mode, and every other row is built in both so the unprivileged pass covers
# the same ground it can cover.
#
# Refusal rows (27, 28, 29) each abort a whole run, so they are separate
# seeders below rather than part of this tree.
rb_seed_filesystem_matrix_fixture() { # root [root|user]
  local root=$1 mode=${2:-user}
  mkdir -p "$root"

  # 01 plain files, three modes. 02 an empty file.
  printf 'm01 0644\n' >"$root/m01_a.txt"
  printf 'm01 0600\n' >"$root/m01_b.txt"
  printf 'm01 0400\n' >"$root/m01_c.txt"
  chmod 0644 "$root/m01_a.txt"
  chmod 0600 "$root/m01_b.txt"
  chmod 0400 "$root/m01_c.txt"
  : >"$root/m02_empty"

  # 03 empty nested directories.
  mkdir -p "$root/m03_dir/deep/deeper"

  # 04-06 the special mode bits on files, 07-08 on directories.
  printf 'm04\n' >"$root/m04_setuid"
  printf 'm05\n' >"$root/m05_setgid"
  printf 'm06\n' >"$root/m06_setuid_setgid"
  chmod 4755 "$root/m04_setuid"
  chmod 2755 "$root/m05_setgid"
  chmod 6755 "$root/m06_setuid_setgid"
  mkdir -p "$root/m07_setgid_dir" "$root/m08_sticky_dir"
  printf 'm07\n' >"$root/m07_setgid_dir/file.txt"
  printf 'm08\n' >"$root/m08_sticky_dir/file.txt"
  chmod 2775 "$root/m07_setgid_dir"
  chmod 1777 "$root/m08_sticky_dir"

  # 11 a directory that only its owner may read, holding a read-only file. The
  # directory mode goes on last: 0500 still allows the walk (r-x), but the file
  # has to exist first.
  mkdir -p "$root/m11_dir"
  printf 'm11\n' >"$root/m11_dir/ro.txt"
  chmod 0400 "$root/m11_dir/ro.txt"
  chmod 0500 "$root/m11_dir"

  # 14-18 symlinks: relative, absolute, dangling, to a directory, and a loop.
  ln -s m01_a.txt "$root/m14_rel_link"
  ln -s "$root/m01_a.txt" "$root/m15_abs_link"
  ln -s m16_nowhere "$root/m16_dangling"
  ln -s m03_dir "$root/m17_dir_link"
  ln -s m18_loop_b "$root/m18_loop_a"
  ln -s m18_loop_a "$root/m18_loop_b"

  # 19-21 hardlinks: a pair, a triple across directories, and one to a setuid
  # file (the mode must survive on every name).
  printf 'm19\n' >"$root/m19_hard_a"
  ln "$root/m19_hard_a" "$root/m19_hard_b"
  mkdir -p "$root/m20_dir1" "$root/m20_dir2" "$root/m20_dir3"
  printf 'm20\n' >"$root/m20_dir1/m20_hard_a"
  ln "$root/m20_dir1/m20_hard_a" "$root/m20_dir2/m20_hard_b"
  ln "$root/m20_dir1/m20_hard_a" "$root/m20_dir3/m20_hard_c"
  printf 'm21\n' >"$root/m21_setuid_src"
  chmod 4755 "$root/m21_setuid_src"
  ln "$root/m21_setuid_src" "$root/m21_hard_link"

  # 24 a FIFO. Its mode is set explicitly because mkfifo applies the umask.
  mkfifo "$root/m24_fifo"
  chmod 0640 "$root/m24_fifo"

  # 30 a sparse file: 64 MiB of hole with 4 KiB written at 32 MiB. Restored
  # dense — size and content are identical, disk usage is not (documented).
  truncate -s 64M "$root/m30_sparse.bin"
  dd if=/dev/urandom of="$root/m30_sparse.bin" bs=4096 count=1 seek=8192 \
    conv=notrunc status=none

  if [[ $mode == root ]]; then
    # 09-10 modes that deny even their owner; only root can walk back into them.
    mkdir -p "$root/m09_mode000_dir"
    printf 'm09\n' >"$root/m09_mode000_dir/file.txt"
    printf 'm10\n' >"$root/m10_mode000.txt"
    chmod 0000 "$root/m10_mode000.txt"
    chmod 0000 "$root/m09_mode000_dir"
    # 12-13 foreign owners, one that exists on the host and one that does not.
    printf 'm12\n' >"$root/m12_nobody.txt"
    printf 'm13\n' >"$root/m13_ghost.txt"
    local nobody_uid nobody_gid
    nobody_uid=$(id -u nobody 2>/dev/null || echo 65534)
    nobody_gid=$(id -g nobody 2>/dev/null || echo 65534)
    chown "$nobody_uid:$nobody_gid" "$root/m12_nobody.txt"
    chown 12345:12345 "$root/m13_ghost.txt"
    # 25-26 device nodes.
    mknod "$root/m25_chardev" c 1 3
    mknod "$root/m26_blockdev" b 7 0
    chmod 0660 "$root/m25_chardev" "$root/m26_blockdev"
  fi

  # 22-23 timestamps at both extremes, set last so nothing above moves them
  # again. Pre-epoch and far-future both have to round-trip, nanoseconds
  # included.
  printf 'm22\n' >"$root/m22_pre_epoch.txt"
  printf 'm23\n' >"$root/m23_future.txt"
  touch -d '1960-01-01 00:00:00' "$root/m22_pre_epoch.txt"
  touch -d '2100-01-01 00:00:00.123456789' "$root/m23_future.txt"
}

# M-FS-27. A unix socket cannot be reproduced, so the whole run is refused
# while analyzing; it therefore cannot share a tree with the round-trip rows.
rb_seed_fs_refusal_socket() { # root
  mkdir -p "$1"
  printf 'm27\n' >"$1/m27_file.txt"
  python3 -c 'import socket, sys; s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' \
    "$1/m27_socket"
}

# M-FS-28. A POSIX name is an arbitrary byte string; this one is not UTF-8, so
# the plan cannot carry it and the run is refused instead of silently renaming
# the entry to contain U+FFFD.
rb_seed_fs_refusal_nonutf8() { # root
  mkdir -p "$1"
  python3 -c 'import os, sys
root = os.fsencode(sys.argv[1])
with open(os.path.join(root, b"m28_\xff\xfe"), "wb") as handle:
    handle.write(b"m28\n")' "$1"
}

# M-FS-29. Deeper than `MAX_WALK_DEPTH` (1024): the walk recurses, so an
# adversarially deep tree has to be refused rather than overflow the stack.
rb_seed_fs_refusal_depth() { # root [levels]
  local root=$1 levels=${2:-1025}
  mkdir -p "$root"
  (
    cd "$root" || exit 1
    local level
    for ((level = 0; level < levels; level++)); do
      mkdir d && cd d || exit 1
    done
    printf 'm29\n' >bottom.txt
  )
}

rb_seed_filesystem_fixture() { # root [large bytes] [bulk files]
  local root=$1 large_bytes=${2:-0} bulk_files=${3:-0}
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
  # Many small-to-medium items of mixed size. A handful of items cannot expose an
  # out-of-order demux (B1): the carrier map is `item_id % carriers`, so a plan
  # has to hold enough items for several to be in flight at once before item N+1
  # can be seen overtaking item N. Sizes are spread deterministically from 0 to
  # 256 KiB so the set also covers zero-byte items and items that span several
  # chunks on every carrier.
  if [[ "$bulk_files" -gt 0 ]]; then
    mkdir -p "$root/bulk"
    local index size
    for ((index = 0; index < bulk_files; index++)); do
      size=$(((index * 7919) % 262144))
      head -c "$size" /dev/urandom >"$root/bulk/item-$(printf '%04d' "$index").bin"
    done
  fi
}

# ---------------------------------------------------------------------------
# PostgreSQL fidelity-matrix helpers (docs/testing/POSTGRES_MATRIX.md).
#
# Every helper drives an existing Docker container through `docker exec`; none of
# them creates or removes a container except `rb_pg_start`, which records the name
# it created in RB_PG_CONTAINERS so a caller's cleanup trap can reap it.
# ---------------------------------------------------------------------------

# Server-side logging the immutability and hygiene assertions read back.
# `log_line_prefix` uses `|` as its field separator and contains no space, so the
# whole string stays safe under the word splitting that passes it to `docker run
# ... postgres $RB_PG_LOG_ARGS`. The prefix is therefore `user@database|time|`.
RB_PG_LOG_ARGS="-c log_statement=all -c log_connections=on -c log_temp_files=0 -c log_line_prefix=%u@%d|%m| -c log_min_duration_statement=-1"
export RB_PG_LOG_ARGS

# Superuser password of every container started by rb_pg_start; also used by the
# pg_dump oracle when it connects across containers.
RB_PG_PASSWORD=${RB_PG_PASSWORD:-postgres}
export RB_PG_PASSWORD

RB_PG_CONTAINERS=()

rb_pg_container_ip() { # container
  docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1"
}

rb_pg_start() { # name image host_port [extra docker run args...]
  local name=$1 image=$2 port=$3
  shift 3
  # Record the name before `docker run`: Docker can leave a created container
  # behind when host-port programming fails.
  RB_PG_CONTAINERS+=("$name")
  # shellcheck disable=SC2086 # RB_PG_LOG_ARGS is a deliberate argument list.
  docker run -d --name "$name" -e POSTGRES_PASSWORD="$RB_PG_PASSWORD" \
    -p "${port}:5432" "$@" "$image" postgres $RB_PG_LOG_ARGS >/dev/null
  local _attempt
  # Probe over TCP, never over the unix socket. While the entrypoint runs
  # `initdb` and `/docker-entrypoint-initdb.d`, it keeps a *temporary* server up
  # with `listen_addresses=''` — reachable on the socket, invisible over TCP —
  # and then shuts it down and starts the real one. A socket probe answers YES
  # to that temporary server, so the caller connects into the restart and reads
  # "the database system is shutting down" or loses its fixture load. The
  # trailing `SELECT 1` closes the gap between listening and accepting. A
  # PostGIS image spends minutes in that window building its template
  # databases, which is why the plain images only flaked and the PostGIS ones
  # failed outright.
  for _attempt in $(seq 1 240); do
    if docker exec "$name" pg_isready -h 127.0.0.1 -p 5432 -U postgres >/dev/null 2>&1 &&
      docker exec "$name" psql -h 127.0.0.1 -U postgres -tAc 'SELECT 1' >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "FAIL: $name ($image) not ready after 240s" >&2
  return 1
}

# Loads `<dir>/*.sql` in lexical order. A `.ge<major>` suffix gates the file on
# the server major; the naming rule is e2e/fixtures/postgres/README.md.
rb_pg_load_fixtures() { # container major db dir
  local container=$1 major=$2 database=$3 dir=$4 file base needed
  [[ -d $dir ]] || { echo "FAIL: fixture directory $dir does not exist" >&2; return 1; }
  while IFS= read -r file; do
    base=${file##*/}
    needed=0
    if [[ $base =~ \.ge([0-9]+)\.sql$ ]]; then needed=${BASH_REMATCH[1]}; fi
    if (( major < needed )); then
      echo "SKIP $base (needs >= $needed)"
      continue
    fi
    if ! docker exec -i "$container" psql -v ON_ERROR_STOP=1 -U postgres -d "$database" \
        -q >/dev/null <"$file"; then
      echo "FAIL: fixture $base did not load into $database" >&2
      return 1
    fi
    echo "LOAD $base"
  done < <(find "$dir" -maxdepth 1 -name '*.sql' | LC_ALL=C sort)
}

# External schema oracle: pg_dump run inside <dump_container> against a server
# reachable at <host>:<port>, normalised so two dumps of the same schema compare
# equal. Patterns in e2e/fixtures/postgres/oracle_ignore.txt drop further lines.
rb_pg_oracle_schema() { # dump_container host port user db out_file
  local dump_container=$1 host=$2 port=$3 user=$4 database=$5 out=$6
  local ignore="$RB_E2E_ROOT/e2e/fixtures/postgres/oracle_ignore.txt"
  if ! docker exec -e PGPASSWORD="$RB_PG_PASSWORD" "$dump_container" \
      pg_dump --schema-only --no-sync -h "$host" -p "$port" -U "$user" "$database" \
      >"$out.raw" 2>"$out.err"; then
    echo "FAIL: pg_dump oracle failed for $database at $host:$port" >&2
    cat "$out.err" >&2
    return 1
  fi
  sed -E -e '/^--/d' -e '/^SET /d' -e '/^SELECT pg_catalog\.set_config/d' \
    -e '/^\\restrict/d' -e '/^\\unrestrict/d' -e '/^[[:space:]]*$/d' "$out.raw" >"$out.tmp"
  if [[ -s $ignore ]]; then
    local pattern
    while IFS= read -r pattern; do
      [[ -z $pattern || $pattern == \#* ]] && continue
      sed -E "/$pattern/d" "$out.tmp" >"$out.tmp2" && mv "$out.tmp2" "$out.tmp"
    done <"$ignore"
  fi
  mv "$out.tmp" "$out"
  rm -f "$out.raw" "$out.err"
}

# External data oracle: one sorted TSV line per relation, sequence, constraint
# and index. Relations are read with FROM ONLY so an inherited or partitioned
# parent never counts its children's rows twice, and the row digest is
# order-independent (md5 of the sorted per-row md5s).
rb_pg_oracle_counts() { # container user db out_file
  local container=$1 user=$2 database=$3 out=$4
  if ! docker exec -i "$container" psql -X -q -v ON_ERROR_STOP=1 \
      -U "$user" -d "$database" >"$out.raw" 2>"$out.err" <<'SQL'
\pset tuples_only on
\pset format unaligned
\pset fieldsep '\t'
\pset footer off
SELECT format(
    'SELECT %L, %L, count(*)::text, coalesce(md5(string_agg(md5(t::text), %L ORDER BY md5(t::text))), %L) FROM ONLY %s t',
    'rel', n.nspname || '.' || c.relname, '', '',
    quote_ident(n.nspname) || '.' || quote_ident(c.relname))
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind IN ('r', 'm')
   AND n.nspname NOT IN ('pg_catalog', 'information_schema')
   AND n.nspname NOT LIKE 'pg_toast%'
   AND n.nspname NOT LIKE 'pg_temp%'
   AND (c.relkind <> 'm' OR c.relispopulated)
   AND (NOT EXISTS (SELECT 1 FROM pg_depend d
                     WHERE d.classid = 'pg_class'::regclass
                       AND d.objid = c.oid AND d.deptype = 'e')
        OR EXISTS (SELECT 1 FROM pg_extension e WHERE c.oid = ANY (e.extconfig)))
 ORDER BY 1
\gexec
SELECT 'rel', n.nspname || '.' || c.relname, 'unpopulated', 'unpopulated'
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind = 'm' AND NOT c.relispopulated
   AND n.nspname NOT IN ('pg_catalog', 'information_schema');
SELECT format(
    'SELECT %L, %L, last_value::text, is_called::text FROM %s',
    'seq', n.nspname || '.' || c.relname,
    quote_ident(n.nspname) || '.' || quote_ident(c.relname))
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind = 'S'
   AND n.nspname NOT IN ('pg_catalog', 'information_schema')
   AND n.nspname NOT LIKE 'pg_temp%'
 ORDER BY 1
\gexec
SELECT 'con', n.nspname || '.' || rel.relname || '.' || con.conname,
       con.convalidated::text, pg_get_constraintdef(con.oid)
  FROM pg_constraint con
  JOIN pg_class rel ON rel.oid = con.conrelid
  JOIN pg_namespace n ON n.oid = rel.relnamespace
 WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
   -- A foreign key on a partitioned table is cloned into every partition by the
   -- server, which also chooses the clone's name: PostgreSQL 16 derives it from
   -- the child table, 18 from the parent constraint. The clone is implied by the
   -- constraint that is compared, so comparing its server-chosen name would fail
   -- a faithful cross-major restore. `conparentid` is PostgreSQL 11+, hence the
   -- `to_jsonb` probe instead of a direct column reference.
   AND coalesce(to_jsonb(con) ->> 'conparentid', '0') = '0';
SELECT 'idx', schemaname || '.' || indexname, indexdef
  FROM pg_indexes
 WHERE schemaname NOT IN ('pg_catalog', 'information_schema');
SQL
  then
    echo "FAIL: data oracle query failed on $database" >&2
    cat "$out.err" >&2
    return 1
  fi
  LC_ALL=C sort "$out.raw" >"$out"
  rm -f "$out.raw" "$out.err"
}

# I-IMMUT evidence from the server's own log: every statement the source role
# issued must be a read. `since` (any timestamp `docker logs --since` accepts)
# narrows the window to one run, which matters while the source still connects
# as the same superuser the harness itself uses to seed the fixtures. Extended-protocol statements are logged as
# `execute <name>: <sql>`; continuation lines of a multi-line statement carry no
# prefix and are therefore not re-checked.
rb_pg_assert_readonly_log() { # container user [since]
  local container=$1 user=$2 since=${3:-} statement checked=0 offenders=0
  local -a log_args=()
  [[ -n $since ]] && log_args+=(--since "$since")
  while IFS= read -r statement; do
    checked=$((checked + 1))
    if ! printf '%s\n' "$statement" | grep -Eq \
        '^(SELECT|WITH|SHOW|TABLE|VALUES)\b|^COPY[[:space:]]*\(?.*\)?[[:space:]]+TO[[:space:]]+STDOUT'; then
      echo "FAIL: non-read statement from $user: $statement" >&2
      offenders=$((offenders + 1))
    fi
    # A multi-line statement is logged as one prefixed line plus continuation
    # lines carrying no prefix (PostGIS registers `spatial_ref_sys` with a
    # multi-line condition, so the source's own COPY spans four log lines).
    # Rejoining them first is what keeps `… TO STDOUT` on the same line as the
    # `COPY` that opens it — otherwise every wrapped read looks like a write.
  done < <(docker logs "${log_args[@]}" "$container" 2>&1 \
    | awk '
        $0 ~ /^[^|]*@[^|]*\|[0-9]/ { if (line != "") print line; line = $0; next }
        { if (line != "") line = line " " $0 }
        END { if (line != "") print line }' \
    | grep -E "$(printf '^%s@[^|]*\\|' "$user")" \
    | grep -E 'statement: |execute [^:]*: ' \
    | sed -E 's/^.*(statement: |execute [^:]*: )//' \
    | sed -E 's/[[:space:]]+/ /g')
  if (( offenders > 0 )); then
    echo "FAIL: $offenders non-read statements from $user" >&2
    return 1
  fi
  echo "read-only log check: $checked statements from $user, all reads"
}

# T-PG-CONN. Connection count is a property of a *running* transfer, so it is
# sampled while the transfer runs and judged afterwards. Only sessions whose
# `application_name` is the tool are counted: the harness's own psql calls, and
# anything else on the server, are not ours to bound.
rb_pg_watch_connections() { # container user outfile  (run in the background)
  local container=$1 user=$2 out=$3
  : >"$out"
  while :; do
    docker exec "$container" psql -U postgres -At -c \
      "SELECT count(*) FROM pg_stat_activity
        WHERE usename = '$user' AND application_name = 'rust-backup'" 2>/dev/null >>"$out"
    sleep 0.5
  done
}

# The peak the watcher saw, against the documented budget (1 boot + 1 per
# database + 1 of tolerance for the moment a connection is being replaced).
rb_pg_assert_connections() { # samples-file max
  local out=$1 max=$2 peak
  peak=$(grep -E '^[0-9]+$' "$out" 2>/dev/null | sort -n | tail -n1)
  if [[ -z ${peak:-} ]]; then
    echo "FAIL: no connection samples were taken" >&2
    return 1
  fi
  if (( peak > max )); then
    echo "FAIL: the source held $peak concurrent connections, budget is $max" >&2
    return 1
  fi
  echo "peak source connections: $peak (budget $max)"
}

# T-IMMUT-S3-LP. The least-privilege policy for an S3 source bucket: the exact
# set of read actions this tool issues, and nothing else. `s3:ListBucket` covers
# `ListObjectsV2`, `s3:GetObject` covers both `GetObject` and `HeadObject`,
# `s3:GetBucketVersioning` is what the refusal of versioned buckets is decided
# on (so a backup cannot start without it), and the rest cover the metadata the
# plan records: object tags and the bucket policy. Object ACLs are read too,
# but MinIO rejects `s3:GetObjectAcl` as an unsupported action and authorizes
# `GetObjectAcl` under `s3:GetObject`; on AWS the same recipe needs
# `s3:GetObjectAcl` added to the object statement. `GetBucketPolicy`
# is the one grant the tool tolerates losing - without it the plan simply
# records no policy, which is a silent fidelity loss, so it belongs in the
# recipe. `e2e/s3_minio_test.sh` drops the bucket's anonymous download grant
# before it runs with this policy, so a backup that completes there is
# authorized by this policy alone.
rb_minio_readonly_policy() { # bucket
  local bucket=$1
  cat <<JSON
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:ListBucket", "s3:GetBucketPolicy", "s3:GetBucketVersioning"],
      "Resource": ["arn:aws:s3:::${bucket}"]
    },
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:GetObjectTagging"],
      "Resource": ["arn:aws:s3:::${bucket}/*"]
    }
  ]
}
JSON
}

# T-IMMUT-S3-LP. `mc admin trace --json` is MinIO's server-side record of every
# S3 API call it served, so it plays the part the PostgreSQL log and the mongod
# log play for the other two modules: the proof that what left this tool was
# read-only does not come from this tool. Any non-read API in the window fails
# the assertion, and an empty window fails too - silence is not evidence. Only
# the backup runs against this server during the traced window, so every record
# in it belongs to the source.
rb_minio_assert_readonly_trace() { # trace-file
  local trace=$1
  python3 -c '
import json, sys

READS = {
    "s3.ListObjectsV2", "s3.ListObjects", "s3.GetObject", "s3.HeadObject",
    "s3.HeadBucket", "s3.GetObjectTagging", "s3.GetObjectACL",
    "s3.GetBucketPolicy", "s3.GetBucketLocation", "s3.GetBucketVersioning",
    "s3.GetBucketTagging", "s3.ListBuckets", "s3.ListMultipartUploads",
}
checked = 0
offenders = []
with open(sys.argv[1], encoding="utf-8", errors="replace") as handle:
    for line in handle:
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except ValueError:
            continue
        api = record.get("api") or (record.get("trace") or {}).get("funcName")
        if not api or not api.startswith("s3."):
            continue
        checked += 1
        if api not in READS:
            offenders.append("%s %s" % (api, record.get("path", "")))
if checked == 0:
    print("FAIL: no S3 API records in the MinIO trace; "
          "the read-only assertion proved nothing", file=sys.stderr)
    raise SystemExit(1)
if offenders:
    for offender in offenders[:5]:
        print("FAIL: write API from the source: %s" % offender, file=sys.stderr)
    raise SystemExit(1)
print("MinIO S3 APIs checked: %d, all reads" % checked)
' "$trace"
}

# T-IMMUT-MONGO-LP. A mongod started with `--profile 0 --slowms 0` writes a
# "Slow query" record for every command it runs (level 0 keeps `system.profile`
# untouched, so the evidence collection cannot itself mutate the source), which
# makes the server's own log the record of what this tool did. The driver tags
# its connections with `appName=rust-backup`, so only those records are
# examined: MongoDB's own housekeeping (the logical session cache refresh writes
# to `config`) is not our traffic and must not be read as our traffic. Any
# command whose first key writes fails the assertion, and a window with no
# records of ours fails too, because silence would otherwise look like proof.
# Needs MongoDB >= 4.4 (JSON log format) — which is why `image_for 4` is
# `mongo:4.4`.
rb_mongo_assert_readonly_log() { # container [since] [app_name]
  local container=$1 since=${2:-} app=${3:-rust-backup}
  local -a log_args=()
  [[ -n $since ]] && log_args+=(--since "$since")
  docker logs ${log_args[@]+"${log_args[@]}"} "$container" 2>&1 | python3 -c '
import json, sys

# `aggregate` is classified by its pipeline, not by its name: the Rust driver
# implements `count_documents` as an `aggregate` with `$match`/`$group`, so
# read-only aggregates are expected traffic from this tool. Only `$out` and
# `$merge` write, and a record whose pipeline the server truncated is treated as
# a write so that unreadable evidence fails loudly instead of passing quietly.
WRITES = {
    "insert", "update", "delete", "create", "drop", "dropDatabase",
    "createIndexes", "dropIndexes", "renameCollection", "findAndModify",
    "collMod", "applyOps", "setProfilingLevel", "createUser", "updateUser",
    "dropUser", "createRole", "updateRole", "dropRole", "grantRolesToUser",
    "revokeRolesFromUser", "grantPrivilegesToRole", "killOp",
    "mapReduce", "compact", "fsync", "shutdown",
}


def aggregate_writes(command):
    pipeline = command.get("pipeline")
    if not isinstance(pipeline, list):
        return True
    return any(
        key in ("$out", "$merge")
        for stage in pipeline
        if isinstance(stage, dict)
        for key in stage
    )


app = sys.argv[1]
checked = 0
offenders = []
for line in sys.stdin:
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        record = json.loads(line)
    except ValueError:
        continue
    if record.get("c") != "COMMAND":
        continue
    attr = record.get("attr") or {}
    if app and attr.get("appName") != app:
        continue
    command = attr.get("command")
    if not isinstance(command, dict) or not command:
        continue
    name = next(iter(command))
    checked += 1
    if name in WRITES or (name == "aggregate" and aggregate_writes(command)):
        offenders.append("%s %s" % (name, command[name]))
if checked == 0:
    print("FAIL: no %s command records in the mongod log; "
          "the read-only assertion proved nothing" % (app or "COMMAND"), file=sys.stderr)
    raise SystemExit(1)
if offenders:
    for offender in offenders[:5]:
        print("FAIL: write command from the source: %s" % offender, file=sys.stderr)
    raise SystemExit(1)
print("mongod commands checked: %d, all reads" % checked)
' "$app"
}

rb_pg_assert_no_temp_files() { # container
  local container=$1 hits
  hits=$(docker logs "$container" 2>&1 | grep -c 'temporary file:' || true)
  if (( hits > 0 )); then
    echo "FAIL: the source spilled $hits temporary files (I-NOTEMP)" >&2
    docker logs "$container" 2>&1 | grep 'temporary file:' | head -5 >&2
    return 1
  fi
  echo "temporary files on the source: none"
}
