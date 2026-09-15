#!/usr/bin/env bash
# Ensure the CLI and the operator guide in docs/usage/ describe the same set of
# switches, in BOTH directions. We intentionally inspect each clap help surface
# rather than maintaining a second flag inventory.
#
#   * present-but-undocumented — a switch the binary accepts and docs/usage/
#     omits;
#   * documented-but-absent    — a switch the guide tells the reader to type and
#     the binary rejects. This half is not cosmetic: `--preserve-ownership` was
#     documented for a flag that never existed (the real one is
#     `--no-preserve-ownership`), and a one-directional check cannot see that.
#
# The second half is then applied to every other operator-facing document too.
# `docs/usage/` is the only surface required to be *complete* — it is the single
# source of truth for using the program — but a ghost flag is just as misleading
# in a module guide or in the README.
set -euo pipefail
cd "$(dirname "$0")/.."

bin=target/debug/rust-backup
[[ -x $bin ]] || cargo build --all-features >/dev/null
usage_docs=(docs/usage/*.md)

# Every long switch the binary accepts, across every help surface.
cli_flags() {
  {
    "$bin" --help
    "$bin" server --help
    "$bin" run --help
    "$bin" plan --help
    for module in postgres mongodb filesystem s3; do
      "$bin" "$module" source --help
      "$bin" "$module" destination --help
    done
  } | grep -Eo -- '--[a-z0-9][a-z0-9-]*' | sort -u
}

# Every long switch the operator guide mentions. `--` runs in markdown table
# rules are not switches, hence the leading-alphanumeric requirement; the
# exclusion list holds switches that belong to other programs the documents
# legitimately show.
not_ours='^--(all-features|no-default-features|release|locked|rm|network|entrypoint|name|env-file|detach|check|ff-only|now|lib)$'
doc_flags() {
  grep -hEo -- '--[a-z0-9][a-z0-9-]*' "$@" | grep -Ev "$not_ours" | sort -u
}

missing=0
while IFS= read -r flag; do
  [[ -z $flag ]] && continue
  printf 'docs/usage/ is missing a documented flag: %s\n' "$flag" >&2
  missing=1
done < <(comm -23 <(cli_flags) <(doc_flags "${usage_docs[@]}"))

while IFS= read -r flag; do
  [[ -z $flag ]] && continue
  printf 'docs/usage/ documents a flag the CLI does not accept: %s\n' "$flag" >&2
  missing=1
done < <(comm -13 <(cli_flags) <(doc_flags "${usage_docs[@]}"))

# Ghost flags in the rest of the operator documentation. Globs, not a hand-kept
# list: a document could otherwise be added and never scanned. `docs/plans/` and
# `CHANGELOG.md` are excluded on purpose: those are history, and both
# legitimately name flags that were proposed, renamed, or (as with
# `--preserve-ownership`) never existed at all.
other_docs=(README.md USAGE.md docs/*.md docs/usage/*.md docs/modules/*.md e2e/README.md)
for doc in "${other_docs[@]}"; do
  [[ -f $doc ]] || continue
  while IFS= read -r flag; do
    [[ -z $flag ]] && continue
    printf '%s names a flag the CLI does not accept: %s\n' "$doc" "$flag" >&2
    missing=1
  done < <(comm -13 <(cli_flags) <(doc_flags "$doc"))
done

(( missing == 0 ))
printf 'help/docs parity: PASS (both directions over docs/usage/, plus ghost-flag scan of every operator document)\n'
