#!/usr/bin/env bash
# I-IMMUT static guard: the source side of every module must not be able to
# write to the backend it reads.
#
# Each module lists the files that run on the source side and the call shapes
# that would mutate that backend. A hit is printed as `path:line: <match>` and
# fails the run. The regexes are deliberately call-shaped rather than
# name-shaped: unit tests live in the same files, so a broad pattern such as
# `execute` would match test scaffolding and make the gate useless.
#
# Usage: bash scripts/source_readonly_lint.sh [--selftest]
#   --selftest  prove the check can still fail: a copy of a source file with an
#               injected write call must be reported.
set -euo pipefail
cd "$(dirname "$0")/.."

POSTGRES_PATTERNS='\.execute\(|\.batch_execute\(|\.copy_in|\.transaction\(|\.simple_query\(|\bconnect_admin\b'
# shellcheck disable=SC2016 # `$out`/`$merge` are MongoDB operators, not shell.
MONGODB_PATTERNS='\.insert_(one|many)\(|\.update_(one|many)\(|\.delete_(one|many)\(|\.replace_one\(|\.find_one_and_|\.drop\(|\.create_collection\(|\.create_index|\.bulk_write\(|\.rename\(|"\$out"|"\$merge"'
FILESYSTEM_PATTERNS='set_permissions\(|fchownat\(|chown\(|utimensat\(|remove_(file|dir)|create_dir|File::create\(|\.write\(true\)|\.create\(true\)|\.truncate\(true\)|\brename\(|\bsymlink\(|hard_link\(|mkfifo\(|mknod\('
S3_PATTERNS='put_object|delete_object|create_bucket|put_bucket|put_object_tagging|put_object_acl|copy_object|create_multipart_upload'

HITS=0
CHECKED=0

check_file() { # file patterns
  local file=$1 patterns=$2
  [[ -f $file ]] || return 0
  CHECKED=$((CHECKED + 1))
  local line
  while IFS= read -r line; do
    [[ -z $line ]] && continue
    echo "${file}:${line}"
    HITS=$((HITS + 1))
  done < <(grep -nEo "$patterns" "$file" || true)
}

check_tree() {
  local file
  for file in crates/rb-postgres/src/source.rs crates/rb-postgres/src/introspect.rs \
      crates/rb-postgres/src/immutability.rs; do
    check_file "$file" "$POSTGRES_PATTERNS"
  done

  # The mongodb module may split its read side into further `source*`/
  # `introspect*` files; every one of them is source-side code.
  for file in crates/rb-mongodb/src/source*.rs crates/rb-mongodb/src/introspect*.rs \
      crates/rb-mongodb/src/immutability.rs; do
    check_file "$file" "$MONGODB_PATTERNS"
  done

  for file in crates/rb-filesystem/src/source.rs crates/rb-filesystem/src/walk.rs \
      crates/rb-filesystem/src/immutability.rs; do
    check_file "$file" "$FILESYSTEM_PATTERNS"
  done

  if [[ -f crates/rb-s3/src/source.rs ]]; then
    check_file crates/rb-s3/src/source.rs "$S3_PATTERNS"
  else
    echo "SKIP rb-s3 (source.rs not split yet)"
  fi
}

if [[ ${1:-} == "--selftest" ]]; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  cp crates/rb-postgres/src/source.rs "$tmp/source.rs"
  printf '\n// injected by --selftest\nfn injected(client: &Client) { client.execute("DELETE FROM t", &[]); }\n' \
    >>"$tmp/source.rs"
  HITS=0
  check_file "$tmp/source.rs" "$POSTGRES_PATTERNS"
  if (( HITS == 0 )); then
    echo "FAIL: --selftest did not detect the injected write call" >&2
    exit 1
  fi
  echo "source read-only lint: selftest PASS (injected write detected)"
  exit 0
elif [[ $# -gt 0 ]]; then
  echo "ERROR: unknown argument $1" >&2
  exit 2
fi

check_tree

if (( HITS > 0 )); then
  echo "FAIL: $HITS write-shaped call(s) in source-side code (I-IMMUT)" >&2
  exit 1
fi
echo "source read-only lint: PASS ($CHECKED files, no write-shaped call)"
