#!/usr/bin/env bash
# Flag and environment-variable inventory for rust-backup.
#
# Prints one TSV line per (command, flag) pair taken from the clap help of every
# command surface, plus one line per environment variable read directly from the
# code (`code` for production reads, `test` for reads under crates/*/tests/). It is the input of the documentation parity audit (docs/usage/
# 10-variabili-ambiente.md): a flag or variable that exists but is undocumented
# shows up here first.
#
# Usage: bash scripts/env_inventory.sh [--check-count <n>]
#   --check-count <n>  exit 1 unless at least <n> distinct RUST_BACKUP_* names
#                      were found (guard against a silently shrinking surface).
set -euo pipefail
cd "$(dirname "$0")/.."

CHECK_COUNT=0
while (( $# > 0 )); do
  case "$1" in
    --check-count)
      [[ $# -ge 2 ]] || { echo "ERROR: --check-count needs a number" >&2; exit 2; }
      CHECK_COUNT=$2
      shift 2
      ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "ERROR: unknown argument $1" >&2
      exit 2
      ;;
  esac
done

BIN=${RUST_BACKUP_BIN:-target/debug/rust-backup}
if [[ ! -x $BIN ]]; then
  cargo build -q --all-features
fi
[[ -x $BIN ]] || { echo "ERROR: binary $BIN not found" >&2; exit 2; }

COMMANDS=(
  "server"
  "run"
  "plan postgres"
  "plan mongodb"
  "plan filesystem"
  "plan s3"
)
for module in postgres mongodb filesystem s3; do
  for role in source destination; do
    COMMANDS+=("$module $role")
  done
done

HELP_DUMP=$(mktemp)
trap 'rm -f "$HELP_DUMP"' EXIT
for command in "${COMMANDS[@]}"; do
  printf '#COMMAND\t%s\n' "$command" >>"$HELP_DUMP"
  # shellcheck disable=SC2086 # the command is a deliberate argument list.
  "$BIN" $command --help >>"$HELP_DUMP" 2>&1
done

ROWS=$(python3 - "$HELP_DUMP" <<'PY'
import re
import sys

rows = []
command = None
paragraph = []


def flush() -> None:
    global paragraph
    if not paragraph or command is None:
        paragraph = []
        return
    # clap prints the switch on its own line and the help text, `[env: …]` and
    # `[default: …]` on the following indented lines; a short option can also
    # carry its help inline. Splitting the switch off the first line handles
    # both shapes without mangling a value placeholder like `--admin[=<ADMIN>]`.
    specification = paragraph[0]
    switch = re.match(
        r"^\s*(?:-{1,2}[A-Za-z0-9][A-Za-z0-9-]*(?:\s?\[?=?[<\[][^\s]*)?(?:\.\.\.)?(?:,\s*)?)+",
        specification,
    )
    inline = specification[switch.end():] if switch else specification
    text = " ".join([inline.strip()] + [part.strip() for part in paragraph[1:]]).strip()
    flag = re.search(r"--[a-z0-9][a-z0-9-]*", specification)
    if flag:
        environment = re.search(r"\[env: ([A-Z0-9_]+)=", text)
        default = re.search(r"\[default: ([^\]]*)\]", text)
        help_text = re.sub(r"\[(env|default|possible values|aliases): [^\]]*\]", "", text)
        help_text = help_text.strip()
        sentence = re.split(r"(?<=[.;]) ", help_text)[0].strip() if help_text else ""
        rows.append((
            command,
            flag.group(0),
            environment.group(1) if environment else "-",
            default.group(1) if default else "-",
            sentence if sentence else "-",
        ))
    paragraph = []


with open(sys.argv[1], encoding="utf-8") as stream:
    for line in stream:
        line = line.rstrip("\n")
        if line.startswith("#COMMAND\t"):
            flush()
            command = line.split("\t", 1)[1]
            in_options = False
            continue
        if re.match(r"^[A-Za-z].*:$", line):
            flush()
            in_options = line.startswith("Options")
            continue
        if not in_options:
            continue
        # A blank line does NOT end an option: clap separates the paragraphs of a
        # long help with one, and `[env: …]` is printed after the last paragraph.
        # Flushing here dropped the env var of every option whose help wrapped
        # into paragraphs — silently, as a shrinking inventory.
        if not line.strip():
            continue
        if re.match(r"^\s{2,8}-", line):
            flush()
            paragraph.append(line)
        elif paragraph:
            paragraph.append(line)
flush()

for row in rows:
    print("\t".join(row))
PY
)

CODE_ROWS=$(
  grep -rnoE 'env::var(_os)?\("(RUST_BACKUP|BORE)_[A-Z0-9_]+"' crates/ \
    | sed -E 's#^([^:]+):([0-9]+):.*"((RUST_BACKUP|BORE)_[A-Z0-9_]+)".*#\1:\2\t\3#' \
    | sort -u \
    | while IFS=$'\t' read -r location name; do
        kind='code'
        case "$location" in */tests/*|*/benches/*) kind='test' ;; esac
        printf '%s\t-\t%s\t-\t%s\n' "$kind" "$name" "$location"
      done
  printf 'code\t-\tRUST_LOG\t-\tread by tracing_subscriber::EnvFilter (crates/rust-backup/src/main.rs)\n'
)

printf 'command\tflag\tenv\tdefault\thelp\n'
printf '%s\n%s\n' "$ROWS" "$CODE_ROWS" | grep -v '^$' | LC_ALL=C sort

if (( CHECK_COUNT > 0 )); then
  found=$(printf '%s\n%s\n' "$ROWS" "$CODE_ROWS" | cut -f3 \
    | grep -E '^RUST_BACKUP_[A-Z0-9_]+$' | sort -u | wc -l)
  if (( found < CHECK_COUNT )); then
    echo "FAIL: found $found distinct RUST_BACKUP_* names, expected at least $CHECK_COUNT" >&2
    exit 1
  fi
  echo "env inventory: $found distinct RUST_BACKUP_* names (>= $CHECK_COUNT)" >&2
fi
