#!/usr/bin/env bash
# Ensure the CLI and USAGE.md describe the same set of switches, in BOTH
# directions.  We intentionally inspect each clap help surface rather than
# maintaining a second flag inventory.
#
#   * present-but-undocumented — a switch the binary accepts and USAGE.md omits;
#   * documented-but-absent    — a switch USAGE.md tells the reader to type and
#     the binary rejects. This half is not cosmetic: `--preserve-ownership` was
#     documented for a flag that never existed (the real one is
#     `--no-preserve-ownership`), and a one-directional check cannot see that.
#
# The second half is then applied to every other operator-facing document too.
# `USAGE.md` is the only one required to be *complete*, but a ghost flag is just
# as misleading in a module guide — and the same `--preserve-ownership` had also
# been copied into `docs/modules/FILESYSTEM.md`.
set -euo pipefail
cd "$(dirname "$0")/.."

bin=target/debug/rust-backup
[[ -x $bin ]] || cargo build --all-features >/dev/null
usage=USAGE.md

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

# Every long switch USAGE.md mentions. `--` runs in markdown table rules are not
# switches, hence the leading-alphanumeric requirement; the exclusion list holds
# switches that belong to other programs the document legitimately shows.
not_ours='^--(all-features|no-default-features|release|locked|rm|network|entrypoint|name|env-file|detach|check|ff-only|now|lib)$'
documented_flags() {
  grep -Eo -- '--[a-z0-9][a-z0-9-]*' "$usage" | grep -Ev "$not_ours" | sort -u
}

missing=0
while IFS= read -r flag; do
  [[ -z $flag ]] && continue
  printf 'USAGE.md missing documented flag: %s\n' "$flag" >&2
  missing=1
done < <(comm -23 <(cli_flags) <(documented_flags))

while IFS= read -r flag; do
  [[ -z $flag ]] && continue
  printf 'USAGE.md documents a flag the CLI does not accept: %s\n' "$flag" >&2
  missing=1
done < <(comm -13 <(cli_flags) <(documented_flags))

# Ghost flags in the rest of the operator documentation. Globs, not a hand-kept
# list: the list named `docs/DEPLOYMENT.md`, a file that does not exist, and the
# `[[ -f ]]` guard below made that silently a no-op — a document could be added
# and never scanned. `docs/plans/` and `CHANGELOG.md` are excluded on purpose:
# those are history, and both legitimately name flags that were proposed,
# renamed, or (as with `--preserve-ownership`) never existed at all.
other_docs=(README.md USAGE.md docs/*.md docs/modules/*.md e2e/README.md)
for doc in "${other_docs[@]}"; do
  [[ -f $doc ]] || continue
  while IFS= read -r flag; do
    [[ -z $flag ]] && continue
    printf '%s names a flag the CLI does not accept: %s\n' "$doc" "$flag" >&2
    missing=1
  done < <(comm -13 <(cli_flags) \
    <(grep -Eo -- '--[a-z0-9][a-z0-9-]*' "$doc" | grep -Ev "$not_ours" | sort -u))
done

(( missing == 0 ))
printf 'help/USAGE parity: PASS (both directions, plus ghost-flag scan of the module docs)\n'
