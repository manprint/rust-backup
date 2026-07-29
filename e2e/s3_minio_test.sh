#!/usr/bin/env bash
# T-S3-MINIO + T-S3-IMMUT: real streaming S3 source -> destination.
# Requires docker and cargo. The script leaves no containers or host files behind.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
rb_build_release
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
src_name=rb-s3-src-$$
dst_name=rb-s3-dst-$$
server_pid=
source_pid=
dst_pid=
cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    printf -- '--- relay log ---\n' >&2
    sed -n '1,240p' "$work/server.log" >&2 2>/dev/null || true
    printf -- '--- source log ---\n' >&2
    sed -n '1,240p' "$work/source.log" >&2 2>/dev/null || true
    printf -- '--- destination log ---\n' >&2
    sed -n '1,240p' "$work/destination.log" >&2 2>/dev/null || true
  fi
  [[ -n "${dst_pid:-}" ]] && kill "$dst_pid" 2>/dev/null || true
  [[ -n "${source_pid:-}" ]] && kill "$source_pid" 2>/dev/null || true
  [[ -n "${server_pid:-}" ]] && kill "$server_pid" 2>/dev/null || true
  docker rm -f "$src_name" "$dst_name" >/dev/null 2>&1 || true
  if [[ ${RUST_BACKUP_E2E_KEEP:-0} == 1 ]]; then
    printf 'kept e2e workdir: %s\n' "$work" >&2
  else
    rm -rf "$work"
  fi
  return "$status"
}
trap cleanup EXIT

mkdir -p "$work/seed/nested"
printf 'rust-backup S3 smoke\n' > "$work/seed/nested/hello.txt"
dd if=/dev/urandom of="$work/seed/large.bin" bs=1M count=7 status=none
touch "$work/seed/empty"

docker run -d --name "$src_name" -p 19000:9000 -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin minio/minio:latest server /data >/dev/null
docker run -d --name "$dst_name" -p 19001:9000 -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin minio/minio:latest server /data >/dev/null
for _ in {1..30}; do
  if curl -fsS http://127.0.0.1:19000/minio/health/live >/dev/null && curl -fsS http://127.0.0.1:19001/minio/health/live >/dev/null; then break; fi
  sleep 1
done
mc() {
  docker run --rm --network host -v "$work/seed:/seed:ro" --entrypoint /bin/sh minio/mc:latest -c \
    'mc alias set src http://127.0.0.1:19000 minioadmin minioadmin >/dev/null && mc alias set dst http://127.0.0.1:19001 minioadmin minioadmin >/dev/null && mc "$@"' sh "$@"
}
mc mb src/source >/dev/null
mc cp --recursive /seed/ src/source/in/ >/dev/null
before=$(mc ls --recursive --json src/source | sort)
printf 'MinIO seeded\n' >&2

cd "$root"
"$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port 7840 >"$work/server.log" 2>&1 &
server_pid=$!
sleep 1
"$RB_E2E_BIN" s3 source --to 127.0.0.1:7840 --channel minio-e2e --no-udp --bucket source --prefix in/ --endpoint http://127.0.0.1:19000 --access-key minioadmin --secret-key minioadmin --path-style >"$work/source.log" 2>&1 &
source_pid=$!
sleep 1
"$RB_E2E_BIN" s3 destination --to 127.0.0.1:7840 --channel minio-e2e --no-udp --yes --bucket destination --prefix out/ --endpoint http://127.0.0.1:19001 --access-key minioadmin --secret-key minioadmin --path-style -P create_bucket=true >"$work/destination.log" 2>&1 &
dst_pid=$!
printf 'Relay, source and destination started\n' >&2
wait "$source_pid"
printf 'Source finished\n' >&2
wait "$dst_pid"
printf 'Destination finished\n' >&2
after=$(mc ls --recursive --json src/source | sort)
[[ "$before" == "$after" ]]
mc diff --quiet src/source/in/ dst/destination/out/
printf 'S3 MinIO e2e passed (streaming multipart + source immutability)\n'
