#!/usr/bin/env bash
# Documentation parity for the operator guide.
#
# `scripts/help_parity.sh` answers "does this switch exist?" over docs/usage/ as
# a whole. This script answers the three questions that survive it:
#
#   * is the switch documented in the chapter that owns it, or only somewhere?
#   * does the row name the environment variable clap actually reads, and the
#     default clap actually prints?
#   * does docs/usage/10-variabili-ambiente.md list every variable the program
#     reads, and only variables it reads?
#
# The inventory comes from `scripts/env_inventory.sh` (the clap help of every
# command surface plus the direct `env::var` reads), so the binary is the source
# of truth and nothing is maintained twice.
#
# Usage: bash scripts/docs_parity.sh [--selftest] [inventory.tsv]
#   --selftest   remove a documented variable from a throwaway copy of the guide
#                and assert that the check reports it. Exits 0 when it does.
#   inventory    reuse an inventory TSV instead of regenerating it.
set -euo pipefail
cd "$(dirname "$0")/.."

SELFTEST=0
INVENTORY=""
while (( $# > 0 )); do
  case "$1" in
    --selftest) SELFTEST=1; shift ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    -*) echo "ERROR: unknown argument $1" >&2; exit 2 ;;
    *) INVENTORY=$1; shift ;;
  esac
done

ALLOWLIST=scripts/docs_parity_allow.txt
[[ -f $ALLOWLIST ]] || { echo "ERROR: $ALLOWLIST not found" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if [[ -z $INVENTORY ]]; then
  INVENTORY=$WORK/inventory.tsv
  # env_inventory.sh honours RUST_BACKUP_BIN, so gates.sh does not pay for a
  # second build of the same binary.
  bash scripts/env_inventory.sh >"$INVENTORY"
fi

# The checker. Reads the inventory and a docs/usage/ root and prints one
# `MISMATCH <file>:<line> <what>` per finding.
check() {
  python3 - "$INVENTORY" "$1" "$ALLOWLIST" "${2:-}" <<'PYCHECK'
import re
import sys

inventory_path, docs_root, allow_path, extra_docs = sys.argv[1:5]

# The module subcommands share ONE clap struct: `postgres source --help` and
# `filesystem source --help` list the same switches, so the inventory's command
# column cannot say which chapter owns a flag. This map does, and the guard
# below fails when a new module flag is not assigned to a chapter — the map
# cannot silently fall behind the binary.
OWNERSHIP = {
    "02-trasporto.md": [
        "--to", "--channel", "--secret", "--secret-file", "--carriers",
        "--udp", "--no-udp", "--insecure", "--max-rate", "--yes",
        "--config", "--param",
    ],
    "03-postgres.md": [
        "--host", "--port", "--user", "--password", "--database", "--sslmode",
        "--admin", "--overwrite", "--extension-version",
    ],
    "04-mongodb.md": [
        "--host", "--port", "--user", "--password", "--database",
        "--uri", "--auth-db", "--overwrite",
    ],
    "05-filesystem.md": [
        "--root", "--follow-symlinks", "--no-preserve-ownership",
        "--preserve-xattr", "--allow-atime-updates",
    ],
    "06-s3.md": [
        "--bucket", "--endpoint", "--region", "--prefix",
        "--access-key", "--secret-key", "--path-style", "--overwrite",
    ],
}
# `server` and `run` have their own clap structs, so their flag set comes from
# the inventory itself and needs no map.
COMMAND_DOC = {"server": "01-server.md", "run": "07-sessioni-yaml.md"}
# Printed by every surface, documented once in the transport chapter.
UNIVERSAL = {"--help", "--version", "--verbose"}
VARIABLE = re.compile(r"\b(?:(?:RUST_BACKUP|BORE)_[A-Z0-9_]+|RUST_LOG)\b")
ENVIRONMENT_DOC = "10-variabili-ambiente.md"

findings = []


def report(where, line, what):
    findings.append(f"MISMATCH {where}:{line} {what}")


def read(path):
    with open(path, encoding="utf-8") as stream:
        return stream.read().split("\n")


allowed_exact = set()
allowed_prefix = []
for entry in read(allow_path):
    entry = entry.split("#", 1)[0].strip()
    if not entry:
        continue
    if entry.endswith("*"):
        allowed_prefix.append(entry[:-1])
    else:
        allowed_exact.add(entry)


def allowed(name):
    return name in allowed_exact or any(name.startswith(p) for p in allowed_prefix)


# --- the inventory ---------------------------------------------------------
rows = []
for line in read(inventory_path)[1:]:
    if not line.strip():
        continue
    parts = line.split("\t")
    if len(parts) == 5:
        rows.append(parts)

flag_environment = {}
flag_default = {}
command_flags = {}
variables = {}
for command, flag, environment, default, help_text in rows:
    if flag == "-":
        variables.setdefault(environment, help_text)
        continue
    command_flags.setdefault(command, set()).add(flag)
    if environment != "-":
        variables.setdefault(environment, f"{command} {flag}")
        previous = flag_environment.setdefault(flag, environment)
        if previous != environment:
            report("scripts/env_inventory.sh", 0,
                   f"{flag} reads {previous} on one command and {environment} on another")
    if default != "-":
        flag_default.setdefault(flag, default)

module_flags = set()
for command, flags in command_flags.items():
    if command.split()[0] in ("postgres", "mongodb", "filesystem", "s3", "plan"):
        module_flags |= flags

# --- tables ----------------------------------------------------------------
def table_rows(path):
    """Yield (line number, [cell, ...], new table) for every markdown table row.

    A cell may carry an escaped pipe (`--udp[=true\\|false]`), so the split is on
    an unescaped `|` only — splitting on every pipe cut those rows in half and
    read the next cell as the variable name."""
    fresh = True
    for number, line in enumerate(read(path), start=1):
        stripped = line.strip()
        if not stripped.startswith("|"):
            fresh = True
            continue
        cells = [cell.replace("\\|", "|").strip()
                 for cell in re.split(r"(?<!\\)\|", stripped.strip("|"))]
        if all(re.fullmatch(r":?-{2,}:?", cell) for cell in cells):
            continue
        yield number, cells, fresh
        fresh = False


def documented_flags(path):
    found = {}
    for number, cells, _ in table_rows(path):
        match = re.search(r"--[a-z0-9][a-z0-9-]*", cells[0])
        if match:
            found.setdefault(match.group(0), (number, cells))
    return found


def cell(value):
    return value.replace("`", "").strip()


def is_none(value):
    return cell(value) in ("", "-", "—", "–")


# 1. every variable the program reads is listed in the environment chapter
environment_names = set()
for _, cells, _ in table_rows(f"{docs_root}/{ENVIRONMENT_DOC}"):
    for match in VARIABLE.finditer(" ".join(cells)):
        environment_names.add(match.group(0))
for name, origin in sorted(variables.items()):
    if name in environment_names or allowed(name):
        continue
    report(f"{docs_root}/{ENVIRONMENT_DOC}", 0,
           f"undocumented variable {name} (read by {origin})")

# 2. every flag is documented in the chapter that owns it
for command, doc in COMMAND_DOC.items():
    documented = documented_flags(f"{docs_root}/{doc}")
    for flag in sorted(command_flags.get(command, set()) - UNIVERSAL):
        if flag not in documented:
            report(f"{docs_root}/{doc}", 0, f"{command} accepts {flag} and no table row describes it")

assigned = {flag for flags in OWNERSHIP.values() for flag in flags}
for flag in sorted(module_flags - UNIVERSAL - assigned):
    report("scripts/docs_parity.sh", 0,
           f"{flag} is accepted by the module commands and no chapter owns it (add it to OWNERSHIP)")

for doc, flags in OWNERSHIP.items():
    documented = documented_flags(f"{docs_root}/{doc}")
    for flag in flags:
        if flag not in documented:
            report(f"{docs_root}/{doc}", 0, f"{flag} belongs to this chapter and no table row describes it")

# 3. the variable and default cells of every flag table match clap
for doc in sorted(set(OWNERSHIP) | set(COMMAND_DOC.values())):
    path = f"{docs_root}/{doc}"
    header = None
    for number, cells, fresh in table_rows(path):
        # A header belongs to its own table only: the troubleshooting tables of
        # the same chapter quote flags in their first column and must not be
        # read as flag rows.
        if fresh:
            header = None
        lowered = [c.lower() for c in cells]
        if "flag" in lowered:
            header = lowered
            continue
        if header is None or "variabile" not in header:
            continue
        match = re.search(r"--[a-z0-9][a-z0-9-]*", cells[0])
        if not match:
            continue
        flag = match.group(0)
        position = header.index("variabile")
        if position < len(cells):
            documented_environment = cells[position]
            expected = flag_environment.get(flag)
            if expected is None and not is_none(documented_environment):
                report(path, number, f"{flag} reads no environment variable, the row names {cell(documented_environment)}")
            elif expected is not None and cell(documented_environment) != expected:
                shown = "none" if is_none(documented_environment) else cell(documented_environment)
                report(path, number, f"{flag} reads {expected}, the row names {shown}")
        if "default" in header and flag in flag_default:
            position = header.index("default")
            if position < len(cells) and not is_none(cells[position]):
                if cell(cells[position]) != flag_default[flag]:
                    report(path, number,
                           f"{flag} defaults to {flag_default[flag]}, the row says {cell(cells[position])}")

# 3b. the environment chapter's own rows: the flag each variable feeds, and the
# default, are the ones clap prints
path = f"{docs_root}/{ENVIRONMENT_DOC}"
header = None
for number, cells, fresh in table_rows(path):
    if fresh:
        header = None
    lowered = [c.lower() for c in cells]
    if lowered and lowered[0] == "variabile":
        header = lowered
        continue
    if header is None or "flag equivalente" not in header:
        continue
    name = cell(cells[0])
    position = header.index("flag equivalente")
    match = re.search(r"--[a-z0-9][a-z0-9-]*", cells[position]) if position < len(cells) else None
    if not match:
        continue
    flag = match.group(0)
    expected = flag_environment.get(flag)
    if expected is not None and expected != name:
        report(path, number, f"{flag} reads {expected}, the row pairs it with {name}")
    if "default" in header and flag in flag_default:
        position = header.index("default")
        if position < len(cells) and not is_none(cells[position]):
            if cell(cells[position]) != flag_default[flag]:
                report(path, number,
                       f"{flag} defaults to {flag_default[flag]}, the row says {cell(cells[position])}")

# 4. no document names a variable the program does not read
documents = [f"{docs_root}/{name}" for name in sorted(
    __import__("os").listdir(docs_root)) if name.endswith(".md")]
documents += [name for name in extra_docs.split(",") if name]
for path in documents:
    for number, line in enumerate(read(path), start=1):
        for match in VARIABLE.finditer(line):
            name = match.group(0)
            tail = line[match.end():]
            # `RUST_BACKUP_PLAN_V3.md` is a file name, not a variable.
            if tail.startswith(".md") or line[:match.start()].endswith("/"):
                continue
            if name in variables or allowed(name):
                continue
            report(path, number, f"names {name}, which the program never reads")

for finding in findings:
    print(finding)
sys.exit(1 if findings else 0)
PYCHECK
}

if (( SELFTEST == 1 )); then
  cp -r docs/usage "$WORK/usage"
  grep -v 'RUST_BACKUP_CARRIERS' "$WORK/usage/10-variabili-ambiente.md" >"$WORK/trimmed"
  mv "$WORK/trimmed" "$WORK/usage/10-variabili-ambiente.md"
  if output=$(check "$WORK/usage"); then
    echo 'FAIL: the selftest removed RUST_BACKUP_CARRIERS and the check still passed' >&2
    exit 1
  fi
  grep -q 'undocumented variable RUST_BACKUP_CARRIERS' <<<"$output" || {
    echo 'FAIL: the selftest expected the removed variable to be reported, got:' >&2
    echo "$output" >&2
    exit 1
  }
  # Second case: a row that names the wrong variable. The first case only
  # proves the environment chapter is read; this one proves the cells are
  # compared against clap rather than merely found.
  sed -i 's/`RUST_BACKUP_SSLMODE`/`RUST_BACKUP_SSL_MODE`/' "$WORK/usage/03-postgres.md"
  if output=$(check "$WORK/usage"); then
    echo 'FAIL: the selftest renamed a variable cell and the check still passed' >&2
    exit 1
  fi
  grep -q -- '--sslmode reads RUST_BACKUP_SSLMODE, the row names RUST_BACKUP_SSL_MODE' <<<"$output" || {
    echo 'FAIL: the selftest expected the renamed cell to be reported, got:' >&2
    echo "$output" >&2
    exit 1
  }
  echo 'docs parity: selftest PASS (removed variable and renamed cell both detected)'
  exit 0
fi

if ! check docs/usage README.md; then
  echo 'docs parity: FAIL' >&2
  exit 1
fi
printf 'docs/usage parity: PASS (every flag in its chapter, every variable listed, variables and defaults match clap)\n'
