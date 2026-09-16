#!/usr/bin/env bash
# T-S3-MINIO + T-S3-IMMUT + T-IMMUT-S3-LP: real streaming S3 source ->
# destination, including a full run driven by a least-privilege identity whose
# read-onlyness is proven from MinIO's own API trace.
# Requires docker and cargo. The script leaves no containers or host files behind.
set -euo pipefail

source "$(dirname "$0")/lib.sh"
rb_build_release
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
src_name=rb-s3-src-$$
dst_name=rb-s3-dst-$$
containers=()
server_pid=
source_pid=
dst_pid=
cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    for log in server source destination overwrite-source overwrite-destination abort-source abort-destination lp-source lp-destination; do
      printf -- '--- %s log ---\n' "$log" >&2
      sed -n '1,240p' "$work/$log.log" >&2 2>/dev/null || true
    done
  fi
  [[ -n "${dst_pid:-}" ]] && kill "$dst_pid" 2>/dev/null || true
  [[ -n "${source_pid:-}" ]] && kill "$source_pid" 2>/dev/null || true
  [[ -n "${server_pid:-}" ]] && kill "$server_pid" 2>/dev/null || true
  for container in "${containers[@]:-}"; do
    docker rm -f "$container" >/dev/null 2>&1 || true
  done
  for container in "${containers[@]:-}"; do
    if docker container inspect "$container" >/dev/null 2>&1; then
      printf 'FAIL: leaked container %s\n' "$container" >&2
      status=1
    fi
  done
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
# More than two 5 MiB parts: required to exercise a failure between uploads.
dd if=/dev/urandom of="$work/seed/large.bin" bs=1M count=12 status=none
touch "$work/seed/empty"

start_minio() { # name host-port
  # Docker Hub's `minio/minio` was withdrawn (`pull access denied ...
  # repository does not exist`); quay.io is MinIO's own registry, and the
  # release is pinned so the suite cannot decay under us again.
  docker run -d --name "$1" -p "$2:9000" -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin quay.io/minio/minio:RELEASE.2025-09-07T16-13-09Z server /data >/dev/null
  containers+=("$1")
}
start_minio "$src_name" 19000
start_minio "$dst_name" 19001
minio_ready=0
for _ in {1..30}; do
  if curl -fsS http://127.0.0.1:19000/minio/health/live >/dev/null 2>&1 \
    && curl -fsS http://127.0.0.1:19001/minio/health/live >/dev/null 2>&1; then
    minio_ready=1
    break
  fi
  sleep 1
done
(( minio_ready == 1 )) || { echo 'MinIO health checks did not become ready' >&2; exit 1; }
MC_IMAGE=quay.io/minio/mc:RELEASE.2025-08-13T08-35-41Z
mc() {
  docker run --rm --network host -v "$work/seed:/seed:ro" --entrypoint /bin/sh "$MC_IMAGE" -c \
    'mc alias set src http://127.0.0.1:19000 minioadmin minioadmin >/dev/null && mc alias set dst http://127.0.0.1:19001 minioadmin minioadmin >/dev/null && mc "$@"' sh "$@"
}
# Run a shell snippet inside the mc image with the source alias already set. The
# snippet is expanded by *this* shell but reaches the container as text, so a
# `"$RB_RO_SECRET"` inside it is resolved by the container's shell from the
# environment (`-e RB_RO_SECRET`) and never appears in any argument vector.
mc_sh() { # shell-snippet
  docker run --rm --network host -e RB_RO_SECRET -v "$work/seed:/seed:ro" \
    --entrypoint /bin/sh "$MC_IMAGE" -c \
    "mc alias set src http://127.0.0.1:19000 minioadmin minioadmin >/dev/null && $1"
}
mc mb src/source >/dev/null
mc cp --recursive /seed/ src/source/in/ >/dev/null
mc cp --attr 'Content-Type=text/x-rust-backup-e2e;origin=rust-backup-e2e' \
  /seed/nested/hello.txt src/source/in/nested/hello.txt >/dev/null
mc anonymous set download src/source >/dev/null
before=$(mc ls --recursive --json src/source | sort)
printf 'MinIO seeded\n' >&2

cd "$root"
"$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port 7840 >"$work/server.log" 2>&1 &
server_pid=$!
sleep 1
"$RB_E2E_BIN" s3 source --to 127.0.0.1:7840 --channel minio-e2e --no-udp --carriers 4 --bucket source --prefix in/ --endpoint http://127.0.0.1:19000 --access-key minioadmin --secret-key minioadmin --path-style >"$work/source.log" 2>&1 &
source_pid=$!
sleep 1
"$RB_E2E_BIN" s3 destination --to 127.0.0.1:7840 --channel minio-e2e --no-udp --yes --bucket destination --prefix out/ --endpoint http://127.0.0.1:19001 --access-key minioadmin --secret-key minioadmin --path-style -P create_bucket=true >"$work/destination.log" 2>&1 &
dst_pid=$!
printf 'Relay, source and destination started\n' >&2
wait "$source_pid"
printf 'Source finished\n' >&2
wait "$dst_pid"
printf 'Destination finished\n' >&2
rb_assert_formal_verification "$work/source.log" "$work/destination.log"
# The source above deliberately asks for four carriers. S3 restores from one, and
# the destination is what enforces that, so the negotiated count must come back as
# 1: any other value means the per-module cap stopped being applied — and a
# mismatch that deadlocked instead of downgrading would never reach this line.
rb_assert_carriers 1 "$work/source.log" "$work/destination.log"
after=$(mc ls --recursive --json src/source | sort)
[[ "$before" == "$after" ]]
mc diff --quiet src/source/in/ dst/destination/out/
mc anonymous get dst/destination | grep -q 'download'
mc stat --json dst/destination/out/nested/hello.txt | grep -q 'rust-backup-e2e'

# A retry with --overwrite must replace existing keys and remove stale keys
# inside the declared destination prefix before it can be formally verified.
printf 'stale destination object\n' >"$work/seed/stale.txt"
mc cp /seed/stale.txt dst/destination/out/stale.txt >/dev/null
source_pid='' dst_pid=''
"$RB_E2E_BIN" s3 source --to 127.0.0.1:7840 --channel minio-overwrite --no-udp --bucket source --prefix in/ --endpoint http://127.0.0.1:19000 --access-key minioadmin --secret-key minioadmin --path-style >"$work/overwrite-source.log" 2>&1 &
source_pid=$!
sleep 1
"$RB_E2E_BIN" s3 destination --to 127.0.0.1:7840 --channel minio-overwrite --no-udp --yes --overwrite --bucket destination --prefix out/ --endpoint http://127.0.0.1:19001 --access-key minioadmin --secret-key minioadmin --path-style >"$work/overwrite-destination.log" 2>&1 &
dst_pid=$!
wait "$source_pid"
wait "$dst_pid"
rb_assert_formal_verification "$work/overwrite-source.log" "$work/overwrite-destination.log"
if mc stat dst/destination/out/stale.txt >/dev/null 2>&1; then
  echo 'stale S3 object survived --overwrite' >&2
  exit 1
fi
mc diff --quiet src/source/in/ dst/destination/out/

# Inject after the first completed multipart part. The destination returns a
# phase-tagged Apply error only after `abort_upload` has removed the upload.
source_pid='' dst_pid=''
RUST_BACKUP_S3_TEST_FAIL_AFTER_PART=1 "$RB_E2E_BIN" s3 source --to 127.0.0.1:7840 --channel minio-abort --no-udp --bucket source --prefix in/ --endpoint http://127.0.0.1:19000 --access-key minioadmin --secret-key minioadmin --path-style >"$work/abort-source.log" 2>&1 &
source_pid=$!
sleep 1
RUST_BACKUP_S3_TEST_FAIL_AFTER_PART=1 "$RB_E2E_BIN" s3 destination --to 127.0.0.1:7840 --channel minio-abort --no-udp --yes --bucket destination --prefix abort/ --endpoint http://127.0.0.1:19001 --access-key minioadmin --secret-key minioadmin --path-style >"$work/abort-destination.log" 2>&1 &
dst_pid=$!
set +e
wait "$source_pid"; abort_source_rc=$?
wait "$dst_pid"; abort_destination_rc=$?
set -e
[[ $abort_source_rc -ne 0 && $abort_destination_rc -ne 0 ]]
after_abort=$(mc ls --recursive --json src/source | sort)
[[ "$before" == "$after_abort" ]]
if mc ls --incomplete --recursive --json dst/destination 2>/dev/null | grep -q .; then
  echo 'orphan multipart upload after injected abort' >&2
  exit 1
fi
# T-IMMUT-S3-LP: the same backup, run by an identity that holds only the
# documented read policy, with MinIO's own trace as the evidence. The harness
# first drops the bucket's anonymous download grant, so the reads that follow
# can only be authorized by the user policy under test.
mc anonymous set none src/source >/dev/null
export RB_RO_SECRET=rb-e2e-readonly-secret
rb_minio_readonly_policy source >"$work/seed/rb-ro-policy.json"
mc_sh 'mc admin user add src rb-ro "$RB_RO_SECRET"' >/dev/null
mc_sh 'mc admin policy create src rb-readonly /seed/rb-ro-policy.json' >/dev/null
mc_sh 'mc admin policy attach src rb-readonly --user rb-ro' >/dev/null

trace_name=rb-s3-trace-$$
containers+=("$trace_name")
docker run -d --name "$trace_name" --network host --entrypoint /bin/sh "$MC_IMAGE" -c \
  'mc alias set src http://127.0.0.1:19000 minioadmin minioadmin >/dev/null && mc admin trace --json src' >/dev/null
sleep 2

source_pid='' dst_pid=''
# The source's credentials go through the environment of this one child process:
# a flag would publish the secret key in the host process list.
(
  export RUST_BACKUP_ACCESS_KEY=rb-ro RUST_BACKUP_SECRET_KEY="$RB_RO_SECRET"
  exec "$RB_E2E_BIN" s3 source --to 127.0.0.1:7840 --channel minio-lp --no-udp --bucket source --prefix in/ --endpoint http://127.0.0.1:19000 --path-style >"$work/lp-source.log" 2>&1
) &
source_pid=$!
sleep 1
"$RB_E2E_BIN" s3 destination --to 127.0.0.1:7840 --channel minio-lp --no-udp --yes --overwrite --bucket destination --prefix lp/ --endpoint http://127.0.0.1:19001 --access-key minioadmin --secret-key minioadmin --path-style >"$work/lp-destination.log" 2>&1 &
dst_pid=$!
wait "$source_pid"
wait "$dst_pid"
rb_assert_formal_verification "$work/lp-source.log" "$work/lp-destination.log"
docker stop "$trace_name" >/dev/null
docker logs "$trace_name" >"$work/trace.json" 2>/dev/null || true
rb_minio_assert_readonly_trace "$work/trace.json"
mc diff --quiet src/source/in/ dst/destination/lp/

# And the server refuses a write with those credentials — the guarantee does not
# depend on this tool being well-behaved.
if mc_sh 'mc alias set ro http://127.0.0.1:19000 rb-ro "$RB_RO_SECRET" >/dev/null && mc cp /seed/stale.txt ro/source/in/denied.txt' >/dev/null 2>&1; then
  echo 'the least-privilege identity was allowed to write to the source bucket' >&2
  exit 1
fi
after_lp=$(mc ls --recursive --json src/source | sort)
[[ "$before" == "$after_lp" ]]
printf 'PASS T-IMMUT-S3-LP\n'

printf 'S3 MinIO e2e passed (metadata/policy/key-set read-back, overwrite cleanup, multipart abort cleanup, source immutability, least-privilege run with server-side trace)\n'
