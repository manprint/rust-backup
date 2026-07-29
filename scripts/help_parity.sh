#!/usr/bin/env bash
# Ensure every public CLI switch is represented in USAGE.md.  We intentionally
# inspect each clap help surface rather than maintaining a second flag inventory.
set -euo pipefail
cd "$(dirname "$0")/.."

bin=target/debug/rust-backup
[[ -x $bin ]] || cargo build --all-features >/dev/null
usage=USAGE.md
missing=0
while IFS= read -r flag; do
  [[ -z $flag ]] && continue
  if ! rg -Fq -- "$flag" "$usage"; then
    printf 'USAGE.md missing documented flag: %s\n' "$flag" >&2
    missing=1
  fi
done < <(
  {
    "$bin" --help
    "$bin" server --help
    "$bin" run --help
    "$bin" plan --help
    for module in postgres mongodb filesystem s3; do
      "$bin" "$module" source --help
      "$bin" "$module" destination --help
    done
  } | rg -o -- '--[a-z0-9-]+' | sort -u
)
(( missing == 0 ))
printf 'help/USAGE parity: PASS\n'
