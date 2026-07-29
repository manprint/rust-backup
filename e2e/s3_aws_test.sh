#!/usr/bin/env bash
# Credential-gated real-AWS smoke. Writes only a unique per-run key/prefix and
# deletes those exact objects on exit.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
if [[ ${RUST_BACKUP_AWS_E2E:-0} != 1 ]]; then
  echo 'SKIP: real AWS S3 smoke (set RUST_BACKUP_AWS_E2E=1)'
  exit 0
fi
for command in aws cmp python3; do
  command -v "$command" >/dev/null || { echo "ERROR: missing $command" >&2; exit 2; }
done
: "${RUST_BACKUP_AWS_SOURCE_BUCKET:?set a disposable source bucket}"
: "${RUST_BACKUP_AWS_DEST_BUCKET:?set a disposable destination bucket}"

rb_build_release
work=$(mktemp -d)
server_pid= source_pid= destination_pid=
token="rust-backup-e2e-$(date +%s)-$$"
source_key="$token/source.txt"
destination_prefix="$token/destination/"
destination_key="${destination_prefix}source.txt"
region=${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  for pid in "${destination_pid:-}" "${source_pid:-}" "${server_pid:-}"; do
    [[ -n "$pid" ]] && kill "$pid" >/dev/null 2>&1 || true
  done
  aws s3api delete-object --bucket "$RUST_BACKUP_AWS_SOURCE_BUCKET" --key "$source_key" >/dev/null 2>&1 || true
  aws s3api delete-object --bucket "$RUST_BACKUP_AWS_DEST_BUCKET" --key "$destination_key" >/dev/null 2>&1 || true
  if (( status != 0 )); then
    find "$work" -name '*.log' -print -exec sed -n '1,200p' {} \; >&2 2>/dev/null || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

printf 'rust-backup real AWS smoke %s\n' "$token" >"$work/source.txt"
aws s3api put-object --bucket "$RUST_BACKUP_AWS_SOURCE_BUCKET" --key "$source_key" \
  --body "$work/source.txt" >/dev/null
before_etag=$(aws s3api head-object --bucket "$RUST_BACKUP_AWS_SOURCE_BUCKET" \
  --key "$source_key" --query ETag --output text)

port=$(rb_free_port)
RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$port" --udp=false \
  >"$work/server.log" 2>&1 &
server_pid=$!
rb_wait_tcp 127.0.0.1 "$port"

RUST_LOG=info "$RB_E2E_BIN" s3 source --to "127.0.0.1:$port" --channel "$token" \
  --no-udp --insecure --bucket "$RUST_BACKUP_AWS_SOURCE_BUCKET" --prefix "$token/" \
  --region "$region" >"$work/source.log" 2>&1 &
source_pid=$!
sleep .5
RUST_LOG=info "$RB_E2E_BIN" s3 destination --to "127.0.0.1:$port" --channel "$token" \
  --no-udp --insecure --yes --bucket "$RUST_BACKUP_AWS_DEST_BUCKET" \
  --prefix "$destination_prefix" --region "$region" -P overwrite=true \
  >"$work/destination.log" 2>&1 &
destination_pid=$!
wait "$source_pid"
wait "$destination_pid"

aws s3api get-object --bucket "$RUST_BACKUP_AWS_DEST_BUCKET" --key "$destination_key" \
  "$work/restored.txt" >/dev/null
cmp "$work/source.txt" "$work/restored.txt"
after_etag=$(aws s3api head-object --bucket "$RUST_BACKUP_AWS_SOURCE_BUCKET" \
  --key "$source_key" --query ETag --output text)
[[ "$before_etag" == "$after_etag" ]]
echo 'PASS: real AWS S3 transfer, byte identity, and source immutability'
