#!/usr/bin/env bash
# Every crate root must declare the workspace's two standing lint invariants.
#
# Clippy enforces them only where they are declared, so a crate that silently
# omits one is not covered by the gate that is supposed to cover it — and that
# is exactly what happened: `rb-core`, the crate every module depends on, went
# without `forbid(unsafe_code)` while the project documented it as universal.
set -euo pipefail
cd "$(dirname "$0")/.."

missing=0
for root in crates/*/src/lib.rs crates/rust-backup/src/main.rs; do
  [[ -f $root ]] || continue
  if ! grep -q '#!\[forbid(unsafe_code)\]' "$root"; then
    printf '%s does not declare #![forbid(unsafe_code)]\n' "$root" >&2
    missing=1
  fi
  if ! grep -q 'deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)' "$root"; then
    printf '%s does not deny unwrap/expect/panic outside tests\n' "$root" >&2
    missing=1
  fi
done

(( missing == 0 ))
printf 'crate lint invariants: PASS\n'
