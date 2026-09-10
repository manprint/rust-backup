#!/usr/bin/env bash
# T-FAULT: the live fault-injection matrix (plan V3 rows F2.3 and F2.5).
#
# Every live case asserts the same three properties, and they are written as
# helpers so no case can forget one:
#
#   A  the source is unchanged (I-IMMUT), audited on the failing path too;
#   B  the destination holds no state that can pass for a complete copy;
#   C  the failure is phase-tagged and the phase is truthful.
#
# B has two shapes, because they are two different promises:
#   * when the destination process survives the fault it must *undo* what the
#     run created — the filesystem module deletes its active partial file, and
#     postgres/mongodb drop the database/collections the run itself created;
#   * when the destination is SIGKILLed, or its backend dies, nothing can clean
#     up. The promise is then that the leftover cannot be mistaken for a
#     complete copy: no peer reported success, and a later restore refuses the
#     dirty destination instead of silently merging into it.
#
# Faults: the source process killed mid-transfer, the coordination server killed
# during the plan exchange, the coordination server killed mid-payload (a relay
# reset), the destination process SIGKILLed at ~10/50/90 % of the bytes, and —
# for the backend-backed modules — the destination backend stopped mid-apply.
#
# Docker is needed only for the postgres/mongodb/s3 groups. Without it they
# print SKIP and are never counted as passes. Set RUST_BACKUP_FAULT_BACKENDS=0
# to skip them deliberately on a host that has Docker.
#
# Usage: bash e2e/fault_matrix.sh [GROUP ...]
#   groups: filesystem immutability postgres mongodb s3 protocol exitcodes
#   default: all of them.
set -uo pipefail

source "$(dirname "$0")/lib.sh"
rb_build_release

# Not named GROUPS: that is a bash built-in array of the caller's group ids, and
# assigning to it is silently discarded.
FAULT_GROUPS=("${@:-all}")
group_enabled() { # name
  local name=$1 requested
  for requested in "${FAULT_GROUPS[@]}"; do
    [[ "$requested" == all || "$requested" == "$name" ]] && return 0
  done
  return 1
}

work=$(mktemp -d)
PASS=0; FAIL=0
# Cases, not assertions: the QA exit criterion is phrased in cases, and one case
# raises several assertions.
CASES=0
PIDS=()
CONTAINERS=()

cleanup() {
  local status=$?
  for pid in "${PIDS[@]:-}"; do [[ -n "$pid" ]] && kill -9 "$pid" >/dev/null 2>&1; done
  # Only remove containers this invocation created, then prove they are gone:
  # a fixture that leaks a container silently poisons the next run.
  for container in "${CONTAINERS[@]:-}"; do docker rm -f "$container" >/dev/null 2>&1; done
  for container in "${CONTAINERS[@]:-}"; do
    if docker container inspect "$container" >/dev/null 2>&1; then
      echo "FAIL: leaked container $container" >&2
      status=1
    fi
  done
  if (( status != 0 || FAIL != 0 )); then
    find "$work" -name '*.log' -exec sh -c 'echo "--- $1"; sed -n "1,40p" "$1"' _ {} \; >&2 2>/dev/null
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

pass() { printf 'PASS: %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }
skip() { printf 'SKIP: %s\n' "$1"; }
case_begin() { CASES=$((CASES + 1)); printf -- '-- case %s\n' "$1"; }

# Reap a job that may have died from SIGKILL, without letting the shell print
# its own "Killed" job notice into the middle of the report. Sets REAP_RC rather
# than echoing: a command substitution would run `wait` in a subshell, which
# cannot wait for its parent's children and always reports failure.
reap() { # pid -> sets REAP_RC
  REAP_RC=0
  { wait "$1"; REAP_RC=$?; } 2>/dev/null
}

# --- the three shared assertions ---------------------------------------------

# A: the source is bit-for-bit what it was before the failing run.
assert_source_unchanged() { # label before after
  if [[ "$2" == "$3" ]]; then
    pass "$1 [A] source unchanged on the failing path"
  else
    fail "$1 [A] source mutated by a failing run"
  fi
}

# C: the failure carries a phase and the phase is the truthful one.
#
# Anchored at the start of the line on purpose: the process prints the error it
# exits with through `eprintln!("{error:#}")`, so that one line begins with the
# phase tag, while every `tracing` line begins with a timestamp. Matching a tag
# anywhere would instead pick up whichever diagnostic happened to be logged last.
assert_phase() { # label log expected-alternation
  local label=$1 log=$2 want=$3 seen
  seen=$(rb_strip_ansi <"$log" | grep -oE '^\[(Connect|Analyze|Validate|Transfer|Apply|Verify|Teardown)\]' | tail -n1)
  if [[ -z "$seen" ]]; then
    fail "$label [C] no phase-tagged error in $(basename "$log")"
  elif [[ "$seen" =~ ^\[($want)\]$ ]]; then
    pass "$label [C] failure is tagged $seen"
  else
    fail "$label [C] phase $seen is not one of [$want] in $(basename "$log")"
  fi
}

# Part of B in every shape: neither peer may claim success.
no_false_success() { # source-log destination-log
  ! rb_strip_ansi <"$1" | grep -q 'BACKUP VERIFIED' \
    && ! rb_strip_ansi <"$2" | grep -q 'RESTORE VERIFIED'
}

# --- transport plumbing shared by every module -------------------------------

# Sets RB_SERVER_PORT and RB_SERVER_PID in the caller's shell. Deliberately not
# a command substitution: `port=$(start_server …)` would run the whole function
# in a subshell and lose both the pid and its `PIDS` registration.
start_server() { # log
  RB_SERVER_PORT=$(rb_free_port)
  RUST_LOG=info "$RB_E2E_BIN" server --bind-addr 127.0.0.1 --control-port "$RB_SERVER_PORT" \
    --udp=false >"$1" 2>&1 &
  RB_SERVER_PID=$!
  PIDS+=("$RB_SERVER_PID")
  rb_wait_tcp 127.0.0.1 "$RB_SERVER_PORT"
}

# Block until the destination has persisted `fraction` percent of `total` bytes,
# measured on the destination itself rather than from the 1 s progress tick, so
# the 10/50/90 % kill points are actually distinguishable.
wait_for_bytes() { # probe-command total fraction
  local probe=$1 total=$2 fraction=$3
  local want=$((total * fraction / 100))
  local got=0 waited=0
  while (( got < want && waited < 1200 )); do
    got=$($probe 2>/dev/null || echo 0)
    got=${got:-0}
    sleep .05
    waited=$((waited + 1))
  done
  (( got >= want ))
}

du_bytes() { du -sb "$1" 2>/dev/null | cut -f1; }

# Precondition for every backend case: the run must actually be transferring
# before the fault is injected. Without this a case whose peers never paired
# would satisfy "the destination holds nothing" trivially and report PASS.
wait_for_progress() { # destination-log
  local log=$1 waited=0
  while (( waited < 900 )); do
    if rb_strip_ansi <"$log" 2>/dev/null | grep -oE '\([0-9]+\.[0-9]%\)' \
        | grep -qvE '^\(0\.0%\)$'; then
      return 0
    fi
    sleep .1
    waited=$((waited + 1))
  done
  return 1
}

# --- filesystem group --------------------------------------------------------

FS_PAYLOAD_BYTES=$((24 * 1024 * 1024))

fs_case() { # label victim when
  local label=$1 victim=$2 when=$3
  case_begin "$label"
  local src="$work/$label-src" dst="$work/$label-dst"
  local slog="$work/$label-source.log" dlog="$work/$label-destination.log"
  mkdir -p "$src" "$dst"
  rb_seed_filesystem_fixture "$src" "$FS_PAYLOAD_BYTES"
  local before before_atime
  before=$(rb_tree_digest "$src")
  before_atime=$(rb_atime_manifest "$src")
  start_server "$work/$label-server.log" || { fail "$label server did not start"; return; }
  local port=$RB_SERVER_PORT server_pid=$RB_SERVER_PID

  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" --channel "$label" \
    --no-udp --insecure --max-rate $((4 * 1024 * 1024)) --root "$src" >"$slog" 2>&1 &
  local source_pid=$!; PIDS+=("$source_pid")
  sleep .3
  RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port" --channel "$label" \
    --no-udp --insecure --yes --root "$dst" >"$dlog" 2>&1 &
  local destination_pid=$!; PIDS+=("$destination_pid")

  if [[ "$when" == "plan" ]]; then
    # Before any payload byte: the peers are pairing and exchanging the plan.
    sleep .5
  else
    wait_for_bytes "du_bytes $dst" "$FS_PAYLOAD_BYTES" "$when" \
      || { fail "$label never reached ${when}% of the payload"; return; }
  fi

  case "$victim" in
    source) kill -9 "$source_pid" >/dev/null 2>&1 ;;
    destination) kill -9 "$destination_pid" >/dev/null 2>&1 ;;
    # An operator interrupt, not a hard kill: the destination handles SIGINT, so
    # here the strong half of assertion B applies — the active partial file must
    # be gone, exactly as the module documentation promises.
    destination-interrupt) kill -INT "$destination_pid" >/dev/null 2>&1 ;;
    server) kill -9 "$server_pid" >/dev/null 2>&1 ;;
  esac

  local source_rc destination_rc
  reap "$source_pid"; source_rc=$REAP_RC
  reap "$destination_pid"; destination_rc=$REAP_RC
  kill -9 "$server_pid" >/dev/null 2>&1
  reap "$server_pid"

  assert_source_unchanged "$label" "$before" "$(rb_tree_digest "$src")"
  if [[ "$before_atime" != "$(rb_atime_manifest "$src")" ]]; then
    fail "$label [A] source atimes changed"
  fi

  # B. The surviving peer must have removed its active partial file; a SIGKILLed
  #    destination cannot, so there the promise is that the leftover is refused
  #    by the next restore instead of being merged into it.
  if [[ "$victim" == "destination" ]]; then
    # SIGKILL denies the destination any cleanup, so the promise is narrower and
    # is about what the leftover can be mistaken for: no peer claimed success,
    # and the payload on disk is visibly not the source's. That a non-empty root
    # is then refused by the next restore is asserted once, in the exit-code
    # section below, rather than re-proved per case.
    local left=0 source_size
    [[ -f "$dst/nested/payload.bin" ]] && left=$(stat -c %s "$dst/nested/payload.bin")
    source_size=$(stat -c %s "$src/nested/payload.bin")
    if no_false_success "$slog" "$dlog" && (( left < source_size )); then
      pass "$label [B] killed destination claims nothing and left $left/$source_size bytes, not a copy"
    else
      fail "$label [B] a SIGKILLed destination left something that could pass for complete"
    fi
  else
    local truncated
    truncated=$(find "$dst" -type f -printf '%s %P\n' 2>/dev/null | while read -r size rel; do
      [[ -f "$src/$rel" ]] || { echo "extra:$rel"; continue; }
      (( size == $(stat -c %s "$src/$rel") )) || echo "short:$rel"
    done)
    if no_false_success "$slog" "$dlog" && [[ -z "$truncated" ]]; then
      pass "$label [B] destination left no file that could pass for complete"
    else
      fail "$label [B] destination kept partial state: ${truncated:-a false success}"
    fi
  fi

  # C. The peer that observed the fault as an error is the one that survived it.
  case "$victim" in
    source) assert_phase "$label" "$dlog" 'Transfer|Apply' ;;
    destination | destination-interrupt) assert_phase "$label" "$slog" 'Transfer|Verify' ;;
    server) assert_phase "$label" "$slog" 'Connect|Transfer|Verify' ;;
  esac
}

if group_enabled filesystem; then
  echo "== filesystem faults"
  fs_case fs-source-kill        source      30
  fs_case fs-destination-kill10 destination 10
  fs_case fs-destination-kill50 destination 50
  fs_case fs-destination-kill90 destination 90
  fs_case fs-destination-int50  destination-interrupt 50
  fs_case fs-server-kill-plan   server      plan
  fs_case fs-relay-reset        server      40

  # The other half of assertion B, for the faults where nothing can clean up:
  # the leftover must be *refused* by the next restore rather than silently
  # merged into. The destination killed at 50 % above left a partial tree; a
  # fresh run into that same root must stop at preflight (exit 3) and must not
  # report a verified restore.
  case_begin fs-dirty-destination-refused
  # Precondition, or the case is vacuous: an empty destination root is accepted,
  # so this only proves anything while the earlier kill really left files behind.
  if [[ -z "$(ls -A "$work/fs-destination-kill50-dst" 2>/dev/null)" ]]; then
    fail 'fs-dirty-destination-refused: the killed run left an empty root, so there is nothing to refuse'
  fi
  dirty_src="$work/fs-dirty-src"
  mkdir -p "$dirty_src"
  rb_seed_filesystem_fixture "$dirty_src" $((2 * 1024 * 1024))
  if start_server "$work/fs-dirty-server.log"; then
    dirty_port=$RB_SERVER_PORT dirty_server_pid=$RB_SERVER_PID
    RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$dirty_port" \
      --channel fs-dirty --no-udp --insecure --root "$dirty_src" \
      >"$work/fs-dirty-source.log" 2>&1 &
    dirty_source_pid=$!; PIDS+=("$dirty_source_pid")
    sleep .3
    RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$dirty_port" \
      --channel fs-dirty --no-udp --insecure --yes \
      --root "$work/fs-destination-kill50-dst" >"$work/fs-dirty-destination.log" 2>&1
    dirty_destination_rc=$?
    reap "$dirty_source_pid"
    kill -9 "$dirty_server_pid" >/dev/null 2>&1
    reap "$dirty_server_pid"
    if (( dirty_destination_rc == 3 )) && \
       no_false_success "$work/fs-dirty-source.log" "$work/fs-dirty-destination.log"; then
      pass 'fs-dirty-destination-refused [B] a partial destination is refused at preflight, not merged into'
    else
      fail "fs-dirty-destination-refused [B] exited $dirty_destination_rc, expected 3 with no success claim"
      sed -n '1,10p' "$work/fs-dirty-destination.log" >&2
    fi
  else
    fail 'fs-dirty-destination-refused server did not start'
  fi
fi

# --- F2.5: immutability under concurrent load --------------------------------
#
# Two directions, because only proving one of them is worthless: writing next to
# the source must NOT be reported as mutation, and writing *into* it must be.

immutability_case() { # label mode
  local label=$1 mode=$2
  case_begin "$label"
  local src="$work/$label-src" dst="$work/$label-dst" sibling="$work/$label-sibling"
  local slog="$work/$label-source.log" dlog="$work/$label-destination.log"
  mkdir -p "$src" "$dst" "$sibling"
  rb_seed_filesystem_fixture "$src" $((8 * 1024 * 1024))
  local before
  before=$(rb_tree_digest "$src")
  start_server "$work/$label-server.log" || { fail "$label server did not start"; return; }
  local port=$RB_SERVER_PORT server_pid=$RB_SERVER_PID

  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$port" --channel "$label" \
    --no-udp --insecure --max-rate $((2 * 1024 * 1024)) --root "$src" >"$slog" 2>&1 &
  local source_pid=$!; PIDS+=("$source_pid")
  sleep .3
  RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$port" --channel "$label" \
    --no-udp --insecure --yes --root "$dst" >"$dlog" 2>&1 &
  local destination_pid=$!; PIDS+=("$destination_pid")

  # Wait until the run is genuinely streaming before touching anything.
  wait_for_bytes "du_bytes $dst" $((8 * 1024 * 1024)) 20 >/dev/null

  if [[ "$mode" == "sibling" ]]; then
    for i in $(seq 1 40); do
      printf 'unrelated write %s\n' "$i" >>"$sibling/noise-$((i % 4)).txt"
      sleep .02
    done
  else
    printf 'the source changed under the run\n' >"$src/mutation.txt"
  fi

  local source_rc destination_rc
  reap "$source_pid"; source_rc=$REAP_RC
  reap "$destination_pid"; destination_rc=$REAP_RC
  kill -9 "$server_pid" >/dev/null 2>&1
  reap "$server_pid"

  if [[ "$mode" == "sibling" ]]; then
    if (( source_rc == 0 && destination_rc == 0 )) \
      && rb_assert_formal_verification "$slog" "$dlog" \
      && [[ "$before" == "$(rb_tree_digest "$src")" ]]; then
      pass "$label writes beside the source root are not a false SourceMutated"
    else
      fail "$label a sibling write broke a run it never touched (source=$source_rc destination=$destination_rc)"
    fi
  else
    if (( source_rc == 6 )) \
      && rb_strip_ansi <"$slog" | grep -q 'SOURCE-IMMUTABILITY VIOLATION' \
      && ! rb_strip_ansi <"$dlog" | grep -q 'RESTORE VERIFIED'; then
      pass "$label a real source mutation exits 6 and blocks the destination proof"
    else
      fail "$label source mutation was not reported as exit 6 (got $source_rc)"
    fi
  fi
}

if group_enabled immutability; then
  echo "== immutability under concurrent load (F2.5)"
  immutability_case immut-sibling-load sibling
  immutability_case immut-source-drift mutate
fi

# --- backend-backed groups ---------------------------------------------------

backends_enabled() {
  group_enabled postgres || group_enabled mongodb || group_enabled s3 || return 1
  [[ ${RUST_BACKUP_FAULT_BACKENDS:-1} == 1 ]] || { skip 'backend fault groups (RUST_BACKUP_FAULT_BACKENDS=0)'; return 1; }
  docker info >/dev/null 2>&1 || { skip 'postgres/mongodb/s3 fault groups (no usable Docker)'; return 1; }
}

if backends_enabled; then
  # -- postgres --------------------------------------------------------------
  if group_enabled postgres; then
  echo "== postgres faults"
  PG_PASSWORD=rbpg
  pg_src=rb-fault-pg-src; pg_dst=rb-fault-pg-dst
  pg_src_port=$(rb_free_port); pg_dst_port=$(rb_free_port)
  start_pg() { # name port
    CONTAINERS+=("$1")
    docker run -d --name "$1" -e POSTGRES_PASSWORD="$PG_PASSWORD" -p "$2:5432" \
      postgres:16-alpine >/dev/null || return 1
    for _ in $(seq 1 40); do
      docker exec "$1" pg_isready -U postgres >/dev/null 2>&1 && return 0
      sleep 1
    done
    return 1
  }
  if start_pg "$pg_src" "$pg_src_port" && start_pg "$pg_dst" "$pg_dst_port"; then
    docker exec -i "$pg_src" psql -U postgres -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
CREATE DATABASE faultdb;
\connect faultdb
CREATE SCHEMA app;
CREATE TABLE app.rows (id bigint PRIMARY KEY, payload text NOT NULL);
INSERT INTO app.rows
  SELECT i, repeat(md5(i::text), 4) FROM generate_series(1, 400000) AS s(i);
SQL
    pg_source_checksum() {
      docker exec "$pg_src" psql -U postgres -d faultdb -At -c \
        "SELECT count(*)||':'||coalesce(md5(string_agg(md5(t::text), '' ORDER BY id)),'') FROM app.rows t"
    }
    pg_dest_state() { # -> "absent" | "rows=<n>"
      local exists
      exists=$(docker exec "$pg_dst" psql -U postgres -At -c \
        "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname='faultdb')" 2>/dev/null)
      if [[ "$exists" != "t" ]]; then echo absent; return; fi
      local rows
      rows=$(docker exec "$pg_dst" psql -U postgres -d faultdb -At -c \
        "SELECT count(*) FROM app.rows" 2>/dev/null)
      echo "rows=${rows:-unreadable}"
    }
    pg_case() { # label victim
      local label=$1 victim=$2
      case_begin "$label"
      local slog="$work/$label-source.log" dlog="$work/$label-destination.log"
      local before
      before=$(pg_source_checksum)
      start_server "$work/$label-server.log" || { fail "$label server did not start"; return; }
      local port=$RB_SERVER_PORT server_pid=$RB_SERVER_PID
      # The credential travels in the environment, never in argv.
      RUST_LOG=info RUST_BACKUP_PASSWORD="$PG_PASSWORD" \
        "$RB_E2E_BIN" postgres source --to "127.0.0.1:$port" --channel "$label" \
        --no-udp --insecure --max-rate $((3 * 1024 * 1024)) --host 127.0.0.1 --port "$pg_src_port" \
        --user postgres --database faultdb --sslmode disable >"$slog" 2>&1 &
      local source_pid=$!; PIDS+=("$source_pid")
      sleep .5
      RUST_LOG=info RUST_BACKUP_PASSWORD="$PG_PASSWORD" \
        "$RB_E2E_BIN" postgres destination --to "127.0.0.1:$port" --channel "$label" \
        --no-udp --insecure --yes --admin --overwrite --host 127.0.0.1 --port "$pg_dst_port" \
        --user postgres --sslmode disable >"$dlog" 2>&1 &
      local destination_pid=$!; PIDS+=("$destination_pid")

      if ! wait_for_progress "$dlog"; then
        fail "$label never began transferring, so the fault would prove nothing"
        kill -9 "$source_pid" "$destination_pid" "$server_pid" >/dev/null 2>&1
        reap "$source_pid"; reap "$destination_pid"; reap "$server_pid"
        return
      fi
      # Let the COPY get past its first rows so the fault really is mid-apply.
      sleep 1

      case "$victim" in
        source) kill -9 "$source_pid" >/dev/null 2>&1 ;;
        server) kill -9 "$server_pid" >/dev/null 2>&1 ;;
        backend) docker stop -t 0 "$pg_dst" >/dev/null 2>&1 ;;
      esac

      local source_rc destination_rc
      reap "$source_pid"; source_rc=$REAP_RC
      reap "$destination_pid"; destination_rc=$REAP_RC
      kill -9 "$server_pid" >/dev/null 2>&1
      reap "$server_pid"
      [[ "$victim" == backend ]] && { docker start "$pg_dst" >/dev/null 2>&1; \
        for _ in $(seq 1 40); do docker exec "$pg_dst" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done; }

      assert_source_unchanged "$label" "$before" "$(pg_source_checksum)"

      local state; state=$(pg_dest_state)
      if [[ "$victim" == backend ]]; then
        # Nothing can clean up after the server it was writing to disappeared;
        # the promise is that the leftover is visibly not the source.
        if no_false_success "$slog" "$dlog" && [[ "$state" == absent || "$state" != "rows=${before%%:*}" ]]; then
          pass "$label [B] dead backend leaves an uncertified, visibly incomplete database ($state)"
        else
          fail "$label [B] destination looks like a complete copy after a dead backend ($state)"
        fi
      elif no_false_success "$slog" "$dlog" && [[ "$state" == absent ]]; then
        pass "$label [B] the half-restored database this run created was dropped"
      else
        fail "$label [B] a half-restored database survived ($state)"
      fi

      case "$victim" in
        source) assert_phase "$label" "$dlog" 'Transfer|Apply' ;;
        server) assert_phase "$label" "$slog" 'Connect|Transfer|Verify' ;;
        backend) assert_phase "$label" "$dlog" 'Apply|Transfer|Verify' ;;
      esac
    }
    pg_case pg-source-kill  source
    pg_case pg-relay-reset  server
    pg_case pg-backend-stop backend
  else
    fail 'postgres fault group could not start its containers'
  fi
  docker rm -f "$pg_src" "$pg_dst" >/dev/null 2>&1
  fi

  # -- mongodb ---------------------------------------------------------------
  if group_enabled mongodb; then
  echo "== mongodb faults"
  mg_src=rb-fault-mg-src; mg_dst=rb-fault-mg-dst
  mg_src_port=$(rb_free_port); mg_dst_port=$(rb_free_port)
  start_mongo() { # name port
    CONTAINERS+=("$1")
    docker run -d --name "$1" -p "$2:27017" mongo:7.0 >/dev/null || return 1
    for _ in $(seq 1 40); do
      docker exec "$1" mongosh --quiet --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && return 0
      sleep 1
    done
    return 1
  }
  if start_mongo "$mg_src" "$mg_src_port" && start_mongo "$mg_dst" "$mg_dst_port"; then
    docker exec "$mg_src" mongosh --quiet faultdb --eval '
      const docs = [];
      for (let i = 0; i < 120000; i++) { docs.push({ _id: i, payload: "x".repeat(180) }); }
      for (let i = 0; i < docs.length; i += 5000) { db.rows.insertMany(docs.slice(i, i + 5000)); }
      db.rows.createIndex({ payload: 1 });
    ' >/dev/null
    mg_source_state() {
      docker exec "$mg_src" mongosh --quiet faultdb --eval 'db.rows.countDocuments({})' 2>/dev/null | tr -d '\r'
    }
    mg_dest_count() {
      docker exec "$mg_dst" mongosh --quiet faultdb --eval \
        'db.getCollectionNames().includes("rows") ? db.rows.countDocuments({}) : "absent"' 2>/dev/null | tr -d '\r'
    }
    mg_case() { # label victim
      local label=$1 victim=$2
      case_begin "$label"
      local slog="$work/$label-source.log" dlog="$work/$label-destination.log"
      local before
      before=$(mg_source_state)
      start_server "$work/$label-server.log" || { fail "$label server did not start"; return; }
      local port=$RB_SERVER_PORT server_pid=$RB_SERVER_PID
      RUST_LOG=info "$RB_E2E_BIN" mongodb source --to "127.0.0.1:$port" --channel "$label" \
        --no-udp --insecure --max-rate $((3 * 1024 * 1024)) \
        --uri "mongodb://127.0.0.1:$mg_src_port" --database faultdb >"$slog" 2>&1 &
      local source_pid=$!; PIDS+=("$source_pid")
      sleep .5
      RUST_LOG=info "$RB_E2E_BIN" mongodb destination --to "127.0.0.1:$port" --channel "$label" \
        --no-udp --insecure --yes --overwrite \
        --uri "mongodb://127.0.0.1:$mg_dst_port" >"$dlog" 2>&1 &
      local destination_pid=$!; PIDS+=("$destination_pid")

      if ! wait_for_progress "$dlog"; then
        fail "$label never began transferring, so the fault would prove nothing"
        kill -9 "$source_pid" "$destination_pid" "$server_pid" >/dev/null 2>&1
        reap "$source_pid"; reap "$destination_pid"; reap "$server_pid"
        return
      fi
      sleep 1

      case "$victim" in
        source) kill -9 "$source_pid" >/dev/null 2>&1 ;;
        server) kill -9 "$server_pid" >/dev/null 2>&1 ;;
        backend) docker stop -t 0 "$mg_dst" >/dev/null 2>&1 ;;
      esac

      local source_rc destination_rc
      reap "$source_pid"; source_rc=$REAP_RC
      reap "$destination_pid"; destination_rc=$REAP_RC
      kill -9 "$server_pid" >/dev/null 2>&1
      reap "$server_pid"
      [[ "$victim" == backend ]] && { docker start "$mg_dst" >/dev/null 2>&1; \
        for _ in $(seq 1 40); do docker exec "$mg_dst" mongosh --quiet --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1 && break; sleep 1; done; }

      assert_source_unchanged "$label" "$before" "$(mg_source_state)"

      local count; count=$(mg_dest_count)
      if [[ "$victim" == backend ]]; then
        if no_false_success "$slog" "$dlog" && [[ "$count" == absent || "$count" != "$before" ]]; then
          pass "$label [B] dead backend leaves an uncertified, visibly incomplete collection ($count)"
        else
          fail "$label [B] destination looks like a complete copy after a dead backend ($count)"
        fi
      elif no_false_success "$slog" "$dlog" && [[ "$count" == absent ]]; then
        pass "$label [B] the half-filled collection this run created was dropped"
      else
        fail "$label [B] a half-filled collection survived (count=$count)"
      fi

      case "$victim" in
        source) assert_phase "$label" "$dlog" 'Transfer|Apply' ;;
        server) assert_phase "$label" "$slog" 'Connect|Transfer|Verify' ;;
        backend) assert_phase "$label" "$dlog" 'Apply|Transfer|Verify' ;;
      esac
    }
    mg_case mg-source-kill  source
    mg_case mg-relay-reset  server
    mg_case mg-backend-stop backend
  else
    fail 'mongodb fault group could not start its containers'
  fi
  docker rm -f "$mg_src" "$mg_dst" >/dev/null 2>&1
  fi

  # -- s3 (MinIO) ------------------------------------------------------------
  if group_enabled s3; then
  echo "== s3 faults"
  s3_src=rb-fault-s3-src; s3_dst=rb-fault-s3-dst
  s3_src_port=$(rb_free_port); s3_dst_port=$(rb_free_port)
  # MinIO accepts TCP before it serves S3, so readiness is its own health
  # endpoint, not a connect() — an `mc` command issued too early fails, and
  # every later assertion would then be about an empty bucket.
  minio_ready() { # port
    local _
    for _ in $(seq 1 60); do
      curl -fsS "http://127.0.0.1:$1/minio/health/live" >/dev/null 2>&1 && return 0
      sleep 1
    done
    return 1
  }
  start_minio() { # name port
    CONTAINERS+=("$1")
    docker run -d --name "$1" -p "$2:9000" -e MINIO_ROOT_USER=minioadmin \
      -e MINIO_ROOT_PASSWORD=minioadmin minio/minio:latest server /data >/dev/null || return 1
    minio_ready "$2"
  }
  # The two aliases are configured independently: chaining them with `&&` made a
  # source-only listing fail whenever the destination endpoint was the one
  # deliberately stopped, which reads as a source mutation that never happened.
  mc() { # run the MinIO client against both endpoints
    docker run --rm --network host --entrypoint /bin/sh minio/mc:latest -c \
      "mc alias set src http://127.0.0.1:$s3_src_port minioadmin minioadmin >/dev/null 2>&1
       mc alias set dst http://127.0.0.1:$s3_dst_port minioadmin minioadmin >/dev/null 2>&1
       mc \"\$@\"" sh "$@"
  }
  if start_minio "$s3_src" "$s3_src_port" && start_minio "$s3_dst" "$s3_dst_port"; then
    s3_seeded=1
    mc mb src/source >/dev/null || s3_seeded=0
    mc mb dst/destination >/dev/null || s3_seeded=0
    # Objects well over 2 × the 5 MiB part size, so the restore is genuinely
    # multipart and an interrupted upload can orphan parts.
    docker run --rm --network host --entrypoint /bin/sh minio/mc:latest -c \
      "set -e
       mc alias set src http://127.0.0.1:$s3_src_port minioadmin minioadmin >/dev/null
       head -c 26214400 /dev/urandom >/tmp/big.bin
       for i in 0 1 2 3; do mc cp /tmp/big.bin src/source/in/obj-\$i.bin >/dev/null; done" sh \
      || s3_seeded=0
    s3_source_listing() { mc ls --recursive --json src/source 2>/dev/null | sort; }
    if (( s3_seeded == 0 )) || [[ -z "$(s3_source_listing)" ]]; then
      fail 's3 fault group could not seed its source bucket'
      s3_seeded=0
    fi
    s3_case() { # label victim
      local label=$1 victim=$2
      case_begin "$label"
      local slog="$work/$label-source.log" dlog="$work/$label-destination.log"
      local before
      before=$(s3_source_listing)
      start_server "$work/$label-server.log" || { fail "$label server did not start"; return; }
      local port=$RB_SERVER_PORT server_pid=$RB_SERVER_PID
      RUST_LOG=info RUST_BACKUP_ACCESS_KEY=minioadmin RUST_BACKUP_SECRET_KEY=minioadmin \
        "$RB_E2E_BIN" s3 source --to "127.0.0.1:$port" --channel "$label" \
        --no-udp --insecure --max-rate $((4 * 1024 * 1024)) --bucket source --prefix in/ \
        --endpoint "http://127.0.0.1:$s3_src_port" --path-style >"$slog" 2>&1 &
      local source_pid=$!; PIDS+=("$source_pid")
      sleep .5
      RUST_LOG=info RUST_BACKUP_ACCESS_KEY=minioadmin RUST_BACKUP_SECRET_KEY=minioadmin \
        "$RB_E2E_BIN" s3 destination --to "127.0.0.1:$port" --channel "$label" \
        --no-udp --insecure --yes --overwrite --bucket destination --prefix "$label/" \
        --endpoint "http://127.0.0.1:$s3_dst_port" --path-style >"$dlog" 2>&1 &
      local destination_pid=$!; PIDS+=("$destination_pid")

      # Let a multipart upload get under way before breaking it.
      if ! wait_for_progress "$dlog"; then
        fail "$label never began transferring, so the fault would prove nothing"
        kill -9 "$source_pid" "$destination_pid" "$server_pid" >/dev/null 2>&1
        reap "$source_pid"; reap "$destination_pid"; reap "$server_pid"
        return
      fi
      sleep 2

      case "$victim" in
        source) kill -9 "$source_pid" >/dev/null 2>&1 ;;
        server) kill -9 "$server_pid" >/dev/null 2>&1 ;;
        backend) docker stop -t 0 "$s3_dst" >/dev/null 2>&1 ;;
      esac

      local source_rc destination_rc
      reap "$source_pid"; source_rc=$REAP_RC
      reap "$destination_pid"; destination_rc=$REAP_RC
      kill -9 "$server_pid" >/dev/null 2>&1
      reap "$server_pid"
      [[ "$victim" == backend ]] && { docker start "$s3_dst" >/dev/null 2>&1; minio_ready "$s3_dst_port"; }

      assert_source_unchanged "$label" "$before" "$(s3_source_listing)"

      # B for S3: a fully uploaded object is not partial state, but an
      # unfinished multipart upload is — it is invisible, billable and would
      # otherwise accumulate on every failure.
      local incomplete
      incomplete=$(mc ls --incomplete --recursive --json dst/destination 2>/dev/null | grep -c .)
      if [[ "$victim" == backend ]]; then
        # An upload cannot be aborted on an endpoint that is gone. The only
        # remedy for what a dead endpoint orphans is a bucket lifecycle rule,
        # so the assertion here is limited to what the tool controls.
        if no_false_success "$slog" "$dlog"; then
          pass "$label [B] dead endpoint leaves no false success (${incomplete} upload(s) for a lifecycle rule)"
        else
          fail "$label [B] a peer claimed success after its endpoint died"
        fi
      elif no_false_success "$slog" "$dlog" && [[ "${incomplete:-0}" == 0 ]]; then
        pass "$label [B] every multipart upload it started was aborted"
      else
        fail "$label [B] ${incomplete} incomplete multipart upload(s) left behind"
      fi

      case "$victim" in
        source) assert_phase "$label" "$dlog" 'Transfer|Apply' ;;
        server) assert_phase "$label" "$slog" 'Connect|Transfer|Verify' ;;
        backend) assert_phase "$label" "$dlog" 'Apply|Transfer|Verify' ;;
      esac
    }
    if (( s3_seeded == 1 )); then
      s3_case s3-source-kill  source
      s3_case s3-relay-reset  server
      s3_case s3-backend-stop backend
    fi
  else
    fail 's3 fault group could not start its MinIO containers'
  fi
  docker rm -f "$s3_src" "$s3_dst" >/dev/null 2>&1
  fi
fi

# --- deterministic in-process banks ------------------------------------------

if group_enabled protocol; then
echo "== in-process protocol banks"
if (cd "$RB_E2E_ROOT" && cargo test -p rb-core --test fault_injection --test session_test >/dev/null 2>&1); then
  pass '26-case core protocol bank is phase-tagged and source immutability is audited'
else
  fail 'core protocol fault bank failed'
fi
fi

# --- the documented process exit contract ------------------------------------
#
# A caller scripting rust-backup branches on these, so each class must actually
# reach the process status. They used to be collapsed to 1 for every target of a
# parallel session.

if group_enabled exitcodes; then
echo "== exit code contract"
exit_src="$work/exit-src"; mkdir -p "$exit_src"
printf 'x' >"$exit_src/only-file"
start_server "$work/exit-server.log" || fail 'exit-code server did not start'
exit_port=$RB_SERVER_PORT
exit_server_pid=$RB_SERVER_PID

expect_exit() { # code description -- command...
  local want="$1" description="$2"; shift 3
  case_begin "exit-$want: $description"
  "$@" >"$work/exit.log" 2>&1
  local got=$?
  if (( got == want )); then
    pass "exit code $want: $description"
  else
    fail "exit code for $description was $got, expected $want"
    sed -n '1,10p' "$work/exit.log" >&2
  fi
}

# 2 = configuration: an unknown module never reaches a transport.
expect_exit 2 'unknown module' -- "$RB_E2E_BIN" nosuchmodule source --to 127.0.0.1:1 --channel x
# 2 = configuration: a target without --to.
expect_exit 2 'missing --to' -- "$RB_E2E_BIN" filesystem source --channel x --root "$exit_src"
# 7 = transport: nothing is listening on a closed port.
expect_exit 7 'unreachable coordination server' -- \
  "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$(rb_free_port)" --channel exit-transport \
  --no-udp --insecure --root "$exit_src"

# 3 = preflight (destination) and 4 = plan rejected (source): the filesystem
# destination refuses a root that is not empty, and the source must learn that
# as a rejection rather than as a generic failure. Needs a real pair, because
# preflight runs on the received plan.
# Repeated deliberately. The rejecting destination has to get its `PlanAck`
# delivered before its own process exits, or the source sees EOF and reports a
# transport failure (7) instead of a plan rejection (4). That is a race, and a
# single attempt reported it green most of the time.
case_begin 'exit-3/4: preflight rejection, both sides'
mkdir -p "$work/occupied"
: >"$work/occupied/pre-existing"
reject_attempts=3
reject_destination_ok=0
reject_source_ok=0
for attempt in $(seq 1 "$reject_attempts"); do
  RUST_LOG=info "$RB_E2E_BIN" filesystem source --to "127.0.0.1:$exit_port" \
    --channel "exit-preflight-$attempt" --no-udp --insecure --root "$exit_src" \
    >"$work/reject-source-$attempt.log" 2>&1 &
  reject_source_pid=$!; PIDS+=("$reject_source_pid")
  sleep .3
  RUST_LOG=info "$RB_E2E_BIN" filesystem destination --to "127.0.0.1:$exit_port" \
    --channel "exit-preflight-$attempt" --no-udp --insecure --yes --root "$work/occupied" \
    >"$work/reject-destination-$attempt.log" 2>&1
  reject_destination_rc=$?
  reap "$reject_source_pid"; reject_source_rc=$REAP_RC
  (( reject_destination_rc == 3 )) && reject_destination_ok=$((reject_destination_ok + 1))
  (( reject_source_rc == 4 )) && reject_source_ok=$((reject_source_ok + 1))
  (( reject_destination_rc == 3 )) || sed -n '1,10p' "$work/reject-destination-$attempt.log" >&2
  (( reject_source_rc == 4 )) || sed -n '1,10p' "$work/reject-source-$attempt.log" >&2
done
kill -9 "$exit_server_pid" >/dev/null 2>&1
reap "$exit_server_pid"

if (( reject_destination_ok == reject_attempts )); then
  pass 'exit code 3: destination preflight rejection'
else
  fail "destination preflight rejection gave 3 in only $reject_destination_ok/$reject_attempts attempts"
fi
if (( reject_source_ok == reject_attempts )); then
  pass 'exit code 4: source sees the rejection as a plan rejection, every time'
else
  fail "rejected source gave 4 in only $reject_source_ok/$reject_attempts attempts"
fi
fi

printf 'T-FAULT summary: CASES=%d PASS=%d FAIL=%d\n' "$CASES" "$PASS" "$FAIL"
(( FAIL == 0 ))
