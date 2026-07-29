# rust-backup — Plan V2: everything still missing for a first working version

> Companion to `RUST_BACKUP_PLAN.md` (V1, phases 0–8). V1 describes the design and
> what has been *implemented*. **This document is the remaining-work plan**: it was
> written after a full re-review of the tree on **2026-07-29** and lists only what
> is still missing, wrong, unproven, or out of date — phase by phase, sub-phase by
> sub-phase, each one small enough for a Sonnet/Haiku agent to execute without
> re-exploring the codebase.
>
> Format of every sub-phase: **Model · Files · Change · Unit · e2e · Done**.
> Rows marked **⟦OPUS GATE⟧** need an Opus review before they count as done.
> Rows marked **🔴 BLOCKER** are required for v0.1 ("first working version").

---

## Status observed — 2026-07-29 (post-implementation)

This section supersedes older “never run” wording below. “Complete” means code
and the named proof were both run in this workspace; it does not imply a sudo
test was run.

| Area | Status | Observed proof |
|------|--------|----------------|
| V1 PostgreSQL | **complete** | `e2e/postgres_matrix.sh 10 12 14 16 18` → 25/25 |
| V1 MongoDB | **complete** | `e2e/mongodb_matrix.sh 4 5 6 7 8` → 20/20 |
| V3 session contract | **complete** | workspace gates green; plan bounds, completion digest, plan timeout and destination abort tests |
| V5 TLS/secrets | **complete for coordination TLS and Postgres TLS** | plain + self-signed TLS relay e2e; secret redaction tests; Postgres TLS unit suite |
| V6.1 relay multi-carrier | **implemented and proven** | deterministic 4-carrier core test and `RUST_BACKUP_E2E_CARRIERS=4 e2e/relay_smoke.sh` → 4/4; privileged netem speed proof remains sudo-pending |
| V7.2/V7.3 | **complete** | production no-unwrap/expect/panic Clippy denies; bounds tests; `scripts/gates.sh` green |
| V7.1 | **partial** | core fault suite + `e2e/fault_matrix.sh` → 3/3, including a live source-kill immutability case; the planned ~30 backend/disk/channel cases remain |
| V10 CI matrix | **implemented** | CI now runs gates, TLS + 4-carrier relay, MinIO, session, fault bank, PostgreSQL 10/12/14/16/18 and MongoDB 4/5/6/7/8 |
| sudo-only e2e | **deliberately pending** | `filesystem_netns_test.sh`, `transport_netns_test.sh`, `bandwidth_netem.sh` were not invoked |

### V4 exact status

V4 is **not complete**. The multi-STUN chain, pure NAT classifier/plan tests,
backwards-compatible v2 candidate schema with frame bound, and bounded
control-plane sends are implemented. Still required: gathering a populated
reflexive profile, v2 broker rider propagation, keyed connectivity checks,
PCP/UPnP mapping, learned-address cache, NAT lab and sudo netns execution.

### Remaining non-sudo work

V4.4–V4.7, full V7.1 fault bank, V7.4 reconnect, and the measured V6 netem
speed proof remain. They are intentionally not marked complete. Sudo-only work
remains suspended by request.

---

## 0. State of the tree (measured, not assumed)

* `bash scripts/gates.sh` → **green**: `cargo fmt --check`, `clippy --all-targets
  --all-features -D warnings`, `build --all-features`, `build
  --no-default-features`, `test --all-features`.
* **119 tests, 0 ignored, 0 failed** (was 98 before the 2026-07-29 session).
* Crates: `rb-core` (~950 LOC), `rb-transport` (~1.7k), `rb-postgres` (~1.9k),
  `rb-mongodb` (~1.4k), `rb-filesystem` (~1.1k), `rb-s3` (~680), `rust-backup`
  bin (~640). No `todo!`/`unimplemented!` anywhere.
* e2e present: `postgres_matrix.sh`, `postgres_introspect.sh`, `mongodb_matrix.sh`,
  `s3_minio_test.sh`, `full_matrix.sh`. Only the MinIO one has ever run green.

### 0.1 Fixed in the 2026-07-29 review session (do NOT re-plan these)

| # | Fix | Where | Test |
|---|-----|-------|------|
| F1 | Receiver no longer trusts a peer-declared chunk length (was `vec![0; len]` with `len: u32` → up to 4 GiB alloc from one header); anything `> CHUNK_SIZE` is a phase-tagged protocol error. | `rb-core/src/channel.rs` | `wire_test::oversized_declared_chunk_is_refused` |
| F2 | Whole-item integrity is now actually verified: `StreamChunkSource` folds a running per-item BLAKE3 and compares it at `ItemEnd`. Before, `ItemEnd.blake3` was ignored by every module (`ChunkEvent::ItemEnd{..} => {}`), so D12's per-item digest was decorative. | `rb-core/src/channel.rs` | `item_digest_mismatch_detected`, `multi_chunk_item_digest_verifies`, `empty_item_verifies` |
| F3 | The source-immutability audit now runs on **every** exit path, including a failed/aborted run (it previously ran only after a fully successful stream, i.e. never in the case the invariant exists for). | `rb-core/src/session.rs` | `session_test::aborted_run_still_audits_immutability` |
| F4 | A source failing mid-stream sends `DataFrame::Abort{reason}`; the destination reports "source aborted mid-stream: …" instead of interpreting an EOF. | `rb-core/src/{wire,channel,session}.rs` | `mid_stream_abort_carries_reason`, `source_failure_aborts_destination_with_reason` |
| F5 | `PLAN_FORMAT_VERSION` is enforced by the destination (documented since Phase 0, never implemented) and the refusal is sent back as a `PlanAck{accepted:false}` so the source fails fast instead of blocking. | `rb-core/src/{channel,session}.rs` | `unsupported_plan_version_refused` |
| F6 | UDP candidate sanitation ported from bore: `valid_candidate`/`sanitize_candidates`/`sanitize_and_log`, `MAX_UDP_CANDIDATES = 16`, applied at the offering side, the coordination-server broker and the punch/dial entry point (defense in depth). A hostile peer can no longer make the puncher fan out an unbounded dial list. | `rb-transport/src/{shared,server,client}.rs` | 3 unit tests in `shared.rs::candidate_tests` |
| F7 | `expect("valid idle timeout")` removed from the QUIC config hot path (built from a `VarInt`, infallible). | `rb-transport/src/direct.rs` | covered by existing direct tests |
| F8 | CLI: `--config` YAML underlay is now actually merged for single-target runs (CLI > env > YAML, per field, target matched by module+role). It was parsed and silently ignored — D10 was unimplemented outside `run --config`. | `rust-backup/src/main.rs` | 5 unit tests (`cli_overrides_yaml_per_field`, `underlay_is_matched_by_role`, …) |
| F9 | CLI: `plan <module>` dry-run subcommand added (specified in V1 §3.1, absent from the binary). | `rust-backup/src/main.rs` | `plan_subcommand_parses` |
| F10 | CLI: `--carriers > 1` now fails loudly instead of silently streaming on one carrier. | `rust-backup/src/main.rs` | `multi_carrier_is_rejected` |
| F11 | I-OBSERV: `Progress::line` was never called by anything — a run printed no progress at all. Both sides now publish plan totals (`Progress::set_totals`) and the binary logs a progress line every 5 s. | `rb-core/src/{progress,session}.rs`, `rust-backup/src/main.rs` | `both_sides_publish_progress_totals` |
| F12 | `vendored-from-bore/` refreshed from `bore-forked@nat-adv` (`5f7fe00`) + `VENDORED_FROM.txt`, and the three new NAT files (`adaptive_nat.rs`, `portmap.rs`, `udp_diagnostic.rs`), the netns UDP script and `docs/nat/*` are now vendored as port references. | `vendored-from-bore/` | n/a (reference material) |

### 0.2 Verification debt — the honest status of each module

| Module | Code | Proven by | NEVER run |
|--------|------|-----------|-----------|
| transport relay | complete | in-process relay test | netns/NAT topology |
| transport direct (QUIC) | complete | loopback direct e2e | real NAT, netns, relay↔direct flap |
| postgres | complete | pure unit + DDL goldens | **all live SQL** (introspect, COPY, restore, fingerprint) |
| mongodb | complete | pure unit + parsers | **all live driver calls** |
| filesystem | complete | local unit round-trip | privileged (root/CAP_CHOWN) and mid-abort e2e |
| s3 | complete | MinIO e2e (green) | AWS proper, abort injection |

> **Rule for every agent working from this plan:** "gates green" is not "done". A
> sub-phase is done when its named test *ran* and passed (`0 ignored`), and for
> live phases when the named script ran on a host with Docker/sudo.

---

## Phase V1 — 🔴 Live database verification (highest priority)

> Nothing in postgres/mongodb has ever touched a real server. This phase converts
> ~3.3k LOC of "code-complete" into "works". It needs a host with Docker; run it
> before any new feature work.

- **V1.1 🔴** *(Sonnet)* — **Run the postgres live gates.** Files: none (execution).
  Change: on a Docker host run `bash e2e/postgres_introspect.sh 16`, then
  `bash e2e/postgres_matrix.sh 16`, then the full majors
  `bash e2e/postgres_matrix.sh 10 12 14 16 18`. Record every failure verbatim in
  `docs/plans/V1_LIVE_RESULTS.md` with the failing SQL. **Done:** both scripts exit
  0 for every major, or every failure is filed as a V1.2 fix item.
- **V1.2 🔴 ⟦OPUS GATE⟧** *(Sonnet)* — **Fix what the live runs surface.** Files:
  `rb-postgres/src/{introspect,ddl,dest,immutability}.rs`. Expect breakage in:
  catalog column names across majors (`attgenerated` 12+, `prokind` 11+,
  `pg_sequence` shapes), ACL letter → `GRANT` rendering, GUC (`ALTER DATABASE …
  SET`) quoting, identifier quoting of mixed-case/keyword names, `COPY … (FORMAT
  binary)` column-list mismatch on generated/dropped columns, partitioned-table
  ordering, extension-owned object exclusion. **Unit:** every fix lands with a
  golden or unit test reproducing the live failure in-process. **Done:** matrix
  green on 10/12/14/16/18 and the new unit tests pass without Docker.
- **V1.3 🔴** *(Sonnet)* — **Postgres restore transactionality.** Files:
  `rb-postgres/src/dest.rs`. Change: wrap the per-database DDL (`pre_data`) and
  `post_data` phases each in an explicit transaction so a mid-DDL failure leaves no
  half-created schema; data `COPY` stays outside (autocommit) but each failed item
  must abort the whole run with a phase-tagged error naming the item. Document why
  data is not in one transaction (WAL/lock cost) in `docs/modules/POSTGRES.md`.
  **Unit:** apply-order test extended to assert the BEGIN/COMMIT wrapping strings.
  **e2e:** a matrix variant that kills the destination mid-`pre_data` and asserts
  the target database is either absent or empty. **Done:** no half-applied schema.
- **V1.4 🔴** *(Sonnet)* — **Run the mongodb live gates + fix.** Change: on a Docker
  host run `bash e2e/mongodb_matrix.sh 6`, then `4 5 6 7 8`. Expect breakage in:
  `create` command option pass-through per server version, index option names
  (`v`, `background`, collation), `_id_` filtering, `insert_many` batch size vs
  16 MiB command limit (must chunk by *bytes*, not only by 1000 docs — verify),
  BSON framer on documents at the 16 MiB boundary, `count_documents` on 4.x.
  **Unit:** each fix gets an in-process test. **Done:** matrix green for 4..=8.
- **V1.5** *(Haiku)* — **Write the two missing module docs.** Files:
  `docs/modules/POSTGRES.md`, `docs/modules/MONGODB.md` (only FILESYSTEM/S3 exist).
  Content: what is captured, what is deliberately NOT (pg passwords, mongo users,
  views/time-series), version matrix, required source privileges (read-only) and
  destination privileges, restore ordering, known limits. **Done:** both files exist
  and are linked from `docs/modules/README.md`.

---

## Phase V2 — 🔴 e2e harness that actually exists

> `e2e/README.md` documents five scripts that are **not in the repo**:
> `relay_smoke.sh`, `transport_netns_test.sh`, `filesystem_netns_test.sh`,
> `bandwidth_netem.sh` (and `full_matrix.sh` only runs MinIO). The doc currently
> describes a harness that cannot run.

- **V2.1 🔴** *(Sonnet)* — `e2e/relay_smoke.sh` (T-E2E0). Change: no privileges
  needed — start `rust-backup server --udp=false` on a free port, run a
  `filesystem source` → `filesystem destination` pair over the relay on two temp
  dirs (`--no-udp --yes`), assert byte-identical trees (`diff -r`), modes and
  mtimes, and that the source tree hash is unchanged. Follow the bore script
  pattern: `set -u`, `trap cleanup EXIT INT TERM`, `PASS/FAIL` counters, non-zero
  exit on any failure. **Done:** runs in CI with no sudo and no Docker.
- **V2.2 🔴** *(Sonnet)* — `e2e/filesystem_netns_test.sh` (T-FS-OWN, T-FS-IMMUT).
  Change: two cases. (a) **root/CAP_CHOWN case**: seed a source tree with 3
  distinct uid/gid pairs, sparse-ish files, a symlink, a hardlink pair, an empty
  file, a nested empty dir, unusual modes (`4755`, `0400`); restore and assert
  uid/gid/mode/mtime/link topology exactly. (b) **non-root case**: assert modes and
  mtimes are preserved, ownership falls back to the running user, and the preflight
  emitted the documented warning. Plus (c) **mid-abort**: kill the destination at
  ~50 % of bytes, then assert the source tree hash and every source `atime` are
  unchanged. **Done:** N pass / 0 fail under `sudo -n /abs/path/e2e/…`.
- **V2.3 🔴** *(Sonnet)* — `e2e/transport_netns_test.sh` (T-NET1, the V1 1.5 debt).
  Change: netns topology `src-ns ── nat-ns ── coord-ns ── dst-ns`; three runs:
  (a) UDP reachable → assert the direct path is used (log assertion on
  `is_direct`/"direct udp connection established") and bytes transfer;
  (b) UDP blocked by a netns `iptables -j DROP` on the punch ports → assert clean
  relay fallback, same bytes, no error; (c) direct established then UDP dropped
  mid-transfer → assert the transfer still completes over the relay (per-connection
  fallback) and the control channel never died. **Done:** 3/3 pass.
- **V2.4** *(Sonnet)* — `e2e/bandwidth_netem.sh` (T-BW). Change: netns + `tc netem`
  asymmetric bandwidth/RTT (e.g. source 100 Mbit, destination 5 Mbit, 80 ms RTT);
  transfer ≥ 200 MiB and assert: the transfer completes, every item digest verifies,
  the source process RSS stays bounded (no unbounded read-ahead — sample
  `/proc/<pid>/status` `VmRSS` every second, assert below a fixed ceiling), and
  `--max-rate` visibly caps throughput when set. **Done:** I-BANDWIDTH proven
  against a real gap, not just by construction.
- **V2.5** *(Haiku)* — reconcile `e2e/README.md` and `e2e/full_matrix.sh` with what
  exists; `full_matrix.sh` must run relay_smoke + filesystem + transport + s3 by
  default and the Docker DB matrices behind `RUST_BACKUP_FULL_DB_MATRIX=1`, with the
  bore-style PASS/FAIL summary. **Done:** no documented-but-absent script remains.

---

## Phase V3 — 🔴 Correctness gaps in the session contract

> Found by review; each is a small, well-scoped core change with an in-process test.
> None needs Docker.

- **V3.1 🔴 ⟦OPUS GATE⟧** *(Sonnet)* — **Item-completeness check.** Files:
  `rb-core/src/session.rs` (+ `channel.rs` if a counter is needed). Problem: the
  destination applies whatever items arrive; if the source streams fewer items than
  the plan (a skipped table, a dropped collection) the run reports success. Change:
  the destination tracks the set of `ItemEnd`s received and, at `Done`, requires it
  to equal the set of plan items expected to carry data; a mismatch is a phase-tagged
  `Verify` error naming the missing ids. Modules that legitimately emit no data for
  an item (fs directories/symlinks, metadata-only items) must be expressible — add
  a `PlanItem.meta` convention or an explicit `expects_data: bool` field on
  `PlanItem` (bump `PLAN_FORMAT_VERSION` to 2 if the field is added; F5 now enforces
  it, so a bump is a real compatibility event and must be noted in `CHANGELOG.md`).
  **Unit:** a source that skips item 2 must fail the run; a fs-style plan with
  metadata-only items must still pass. **Done:** silent partial restores impossible.
- **V3.2 🔴** *(Sonnet)* — **`Done.total_bytes` is checked.** Files:
  `rb-core/src/session.rs`. Change: compare the declared `total_bytes` against the
  destination's own received byte count; mismatch → `Verify` error. Also stop sending
  an empty `Done.blake3`: either carry a rolling whole-payload digest (fold of item
  digests, order-independent) or remove the field. Pick the fold and document it.
  **Unit:** tampered `Done` is rejected; normal run passes. **Done:** the completion
  frame is meaningful.
- **V3.3 🔴** *(Sonnet)* — **Bounded wait for the plan / for `PlanAck`.** Files:
  `rb-core/src/wire.rs`, `session.rs`. Problem: the 4-byte frame-length read has no
  timeout (deliberately — the source may wait minutes for an operator to type
  `yes`), so a half-open peer can wedge either side forever. Change: introduce two
  explicit budgets — `PLAN_EXCHANGE_TIMEOUT` (default 10 min, env
  `RUST_BACKUP_PLAN_TIMEOUT`) around the plan/ack exchange, and keep
  `IO_IDLE_TIMEOUT` for the data path; a timeout is a `Connect`-phase error naming
  which side stalled. **Unit:** a peer that never answers fails within a short
  test-injected budget. **Done:** no unbounded wait anywhere.
- **V3.4** *(Sonnet)* — **Abort on the destination side too.** Files:
  `rb-core/src/session.rs`. Change: when `stream_in` fails, the destination sends
  `ControlFrame::Abort{reason}` (best-effort) before returning, so the source stops
  streaming immediately instead of discovering a broken pipe; the source's stream
  loop must notice a peer abort promptly (check the read half between items).
  **Unit:** a destination failing on item 1 makes the source stop with the
  destination's reason. **Done:** both directions abort explicitly.
- **V3.5** *(Sonnet)* — **`prompt_yes` must not block the runtime.** Files:
  `rust-backup/src/main.rs`. Change: read stdin via `tokio::task::spawn_blocking`
  (today a blocking `read_line` runs on a runtime worker while the control heartbeat
  task needs to tick; on a single-worker runtime this stalls the keepalive and the
  coordination server reaps the channel after 60 s). The accept closure must become
  async or be resolved before the session starts. **Unit:** an accept policy that
  sleeps 90 s in blocking context does not kill the channel (drive with
  `tokio::time::pause` or a short test-injected heartbeat). **Done:** an operator
  can think for two minutes without losing the channel.

---

## Phase V4 — NAT traversal: align rb-transport with bore@nat-adv

> `vendored-from-bore/` is now at `5f7fe00` (branch `nat-adv`). The delta since the
> original port is large and concentrated exactly where rust-backup is weakest:
> `holepunch.rs` **+2462 lines**, `adaptive_nat.rs` **+361**, `portmap.rs` **+847
> (new)**, `udp_diagnostic.rs` **+472**, `secret.rs` **+395**, `shared.rs` **+462**,
> plus `tests/nat_traversal_test.rs` (558) and `tests/support/natlab.rs` (370) and
> `scripts/udp_nat_netns_test.sh` (277). rb-transport currently implements the
> *pre-nat-adv* design: one socket, loopback+primary-IP+one optional STUN
> candidate, blind punch, no NAT classification, no port mapping, no connectivity
> checks. F6 ported the sanitation layer only.
>
> Port order below is by value/risk. Each sub-phase names its bore anchor; read the
> anchor before writing code, and keep rust-backup's own wire types (`proto.rs`) —
> do **not** import bore's `ClientMessage`/`ServerMessage`.

- **V4.1 ⟦OPUS GATE⟧** *(Sonnet)* — **Multi-STUN chain + reflexive profile.**
  Anchor: `holepunch.rs` `StunTarget`/`SelectedStun`/`discover_reflexive_chain`/
  `discover_reflexive_profile`/`resolve_live_stun_targets`. Files:
  `rb-transport/src/direct.rs` (or a new `stun.rs`). Change: replace the single
  `RUST_BACKUP_STUN_SERVER` probe with an ordered target list (coordination-server
  STUN responder first, then public fallbacks), each tried with retry/timeout,
  yielding a *profile*: reflexive address per target + whether the mapping is
  address/port-dependent. Keep it best-effort and non-blocking for the relay path.
  **Unit:** target-list construction table; profile derivation from synthetic
  observations; a dead target does not fail the gather. **Done:** a reflexive
  candidate is discovered without operator configuration.
- **V4.2 ⟦OPUS GATE⟧** *(Sonnet)* — **NAT classification + adaptive plan.**
  Anchor: `adaptive_nat.rs` (`NatMappingClass`, `NatCandidateKind`, `NatPlanMode`,
  `NatProfile`, `NatPlan`) and `holepunch.rs` `classify_nat`/`StunObservation`.
  Files: new `rb-transport/src/adaptive_nat.rs`. Change: port the pure classifier
  and plan builder (endpoint-independent / address-dependent / port-dependent /
  symmetric → plan mode, candidate kinds, punch pacing and window). Keep it pure and
  table-testable; no I/O in this module. **Unit:** the bore classification matrix
  (each NAT class from its observation set) + plan-mode selection per class pair.
  **Done:** classification and plan are decided from observations, not guessed.
- **V4.3** *(Sonnet)* — **Candidate offer v2 on the wire.** Anchor: `shared.rs`
  `UdpCandidateOffer`/`UdpNatProfile`/`UdpPunchV2`, `secret.rs` broker rider logic.
  Files: `rb-transport/src/proto.rs`, `server.rs`, `client.rs`. Change: extend
  `ClientMsg::UdpCandidateOffer` with candidate *kinds*, priorities, a `generation`
  counter and the NAT profile; extend `ServerMsg::UdpPunch` with an optional v2
  rider (peer profile + agreed plan). **Both must stay backward compatible**: an
  old peer's offer deserializes (missing fields default), and the frame must stay
  inside `MAX_FRAME_LENGTH` — port bore's worst-case-frame-size test. **Unit:**
  old→new and new→old serde matrices; worst-case frame size assertion; sanitation
  (F6) applied to the new shape. **Done:** richer offers, no wire break.
- **V4.4 ⟦OPUS GATE⟧** *(Sonnet)* — **Connectivity checks before QUIC.** Anchor:
  `holepunch.rs` `UdpTraversalSocket`, `CheckRole`/`CheckConfig`/`CheckPlan`,
  `plan_check_groups`, `plan_check_window`, `derive_check_key`,
  `run_connectivity_checks`, `listener_checks_then_quic`, `dialer_checks_then_quic`.
  Files: `rb-transport/src/direct.rs`. Change: before starting QUIC, run keyed
  (HMAC over the shared token) check datagrams over grouped candidate pairs, pick
  the winning pair, and start QUIC *on the validated pair* instead of racing blind
  dials. Preserve the existing rules: one socket for checks and QUIC, never
  `SO_REUSEADDR` on the punch socket, `EADDRINUSE`→ephemeral, UDP never gates
  channel liveness, relay stays warm. **Unit:** check-group planning table; keyed
  check accept/reject (wrong key rejected, counters incremented); loopback
  checks→QUIC handshake. **Done:** direct setup succeeds on a class of NATs where
  blind punching fails, and never *delays* the relay fallback beyond
  `DIRECT_SETUP_TIMEOUT`.
- **V4.5** *(Sonnet)* — **Port mapping (PCP + UPnP), opt-in.** Anchor:
  `portmap.rs` (`MappingBackend`, `acquire_lease`, `spawn_manager`, `PcpBackend`,
  `UpnpBackend`, `LeaseHandle`). Files: new `rb-transport/src/portmap.rs`, feature
  `portmap` (default off; must not affect `--no-default-features`). Change: try to
  acquire an external port mapping for the punch socket, add the mapped address as a
  candidate kind, renew the lease in a background task, release on drop. All failures
  are silent downgrades. **Unit:** PCP request/response codec vectors; lease renewal
  and drop-release with a mock backend. **Done:** a mapped candidate appears when the
  gateway supports it; nothing regresses when it does not.
- **V4.6** *(Sonnet)* — **Learned-address cache + sibling connections.** Anchor:
  `holepunch.rs` address cache (`remember`/`recall`/`invalidate`) and
  `DirectConn::open_sibling`. Files: `rb-transport/src/{direct,channel}.rs`. Change:
  remember the winning remote address per channel id and try it first next run;
  invalidate on failure. `open_sibling` is the prerequisite for V6 (multi-carrier
  over direct). **Unit:** cache hit/miss/invalidate; sibling connection carries
  bytes on loopback. **Done:** reconnects skip discovery when the path is stable.
- **V4.7** *(Sonnet)* — **NAT lab test harness.** Anchor: bore
  `tests/support/natlab.rs`, `tests/nat_traversal_test.rs`,
  `scripts/udp_nat_netns_test.sh` (all three now vendored). Files:
  `rb-transport/tests/support/natlab.rs`, `rb-transport/tests/nat_traversal.rs`,
  `e2e/udp_nat_netns_test.sh`. Change: port the in-process NAT simulator (full-cone,
  restricted-cone, port-restricted, symmetric) and assert the traversal outcome per
  class pair; the netns script is the real-kernel counterpart. **Done:** the NAT
  matrix is a repeatable test, not a field report.
- **V4.8** *(Sonnet)* — **Control-plane send-block edge** (V1's "known edge").
  Files: `rb-transport/src/server.rs`. Change: a peer that stops *reading* can block
  `send_server` inside the `select!`, so the recv-deadline reaper never fires. Wrap
  every control send in a bounded `timeout` (or move sends to a separate task with a
  bounded queue) so a wedged peer is reaped. **Unit:** a peer that never reads is
  reaped within the deadline. **Done:** no zombie channel id under a stuck reader.

---

## Phase V5 — Transport TLS + secrets, end to end

- **V5.1 🔴** *(Sonnet)* — **Server-side TLS is missing.** Files:
  `rb-transport/src/{transport,server}.rs`, `rust-backup/src/main.rs`,
  `rb-core/src/config.rs`. Problem: the client can speak TLS
  (`transport::connect(..., insecure)`), but `run_server` only accepts plain TCP —
  so `--insecure` is the *only* working mode and the control channel (which carries
  the secret-authenticated handshake and the whole plan) is cleartext. Change: port
  bore's `server_tls_from_pem`; add `--tls-cert`/`--tls-key` (+ `RUST_BACKUP_TLS_*`)
  to `rust-backup server` and a TLS acceptor branch in the accept loop; keep plain
  TCP available for netns/CI. **Unit:** PEM loading errors are explicit; a TLS
  client↔TLS server handshake over loopback carries a frame. **e2e:** relay_smoke
  variant with a self-signed cert and `--insecure` on the clients. **Done:** an
  operator can run the coordination server with TLS on.
- **V5.2** *(Sonnet)* — **Secret handling audit.** Files: `rb-transport/src/auth.rs`,
  `rust-backup/src/main.rs`. Change: `--secret` on the command line leaks via
  `/proc/*/cmdline`; document and prefer `RUST_BACKUP_SECRET`/a secret file
  (`--secret-file`). Ensure no secret is ever logged (audit the `tracing` fields of
  every module too — postgres/mongo params include passwords). **Unit:** a params
  Debug/render never contains the password. **Done:** no secret in logs or plan
  output.
- **V5.3** *(Sonnet)* — **Postgres/MongoDB TLS** (V1's deferred TLS follow-ups).
  Files: `rb-postgres/src/connect.rs` (+`tokio-postgres-rustls`),
  `docs/modules/{POSTGRES,MONGODB}.md`. Change: implement `require`/`verify-ca`/
  `verify-full` (today they error out); for mongo, document that discrete
  host/port params connect without TLS and `--uri` with `?tls=true` is the TLS path,
  or wire an explicit `--tls` param. **Unit:** sslmode→connector mapping table.
  **e2e:** a postgres matrix run against a TLS-enabled container. **Done:** verifying
  modes work.

---

## Phase V6 — Multi-carrier parallelism (V1 6.1 debt)

- **V6.1 ⟦OPUS GATE⟧** *(Opus designs, Sonnet implements)* — **Protocol change for
  N carriers.** Files: `rb-core/src/{channel,session,wire}.rs`,
  `rb-transport/src/channel.rs`. Change: the current module interface is one ordered
  sink; item-pinned parallel carriers need (a) control on substream 0 only,
  (b) N data substreams negotiated at setup, (c) one item = one carrier, never
  intra-item striping (reorder trap — mirror bore's flow-pinning rule),
  (d) a destination-side demux that keeps per-item order while items interleave,
  (e) `carriers == 1` must remain byte-identical to today. Also remove the F10 CLI
  guard when this lands. **Unit:** N-carrier distribution is deterministic;
  single-carrier byte-identity; out-of-order item interleaving applies correctly.
  **e2e:** relay_smoke and bandwidth_netem with `--carriers 4`. **Done:** measured
  speedup on the netem gap with identical digests.
- **V6.2** *(Sonnet)* — **Per-carrier direct QUIC** on top of V4.6
  (`open_sibling`). **Done:** carriers ride the direct path with relay fallback per
  carrier.

---

## Phase V7 — 🔴 Hardening: the invariant test bank (V1 8.1/8.2 debt)

- **V7.1 🔴 ⟦OPUS GATE⟧** *(Sonnet)* — **Error-injection suite across all modules.**
  Files: new `crates/rb-core/tests/fault_injection.rs` (in-process, mock channel +
  fault-injecting sink/source) and `e2e/fault_matrix.sh` (live, per module). Faults
  to inject, per module: coordination-server killed mid-transfer; destination
  process killed at 10/50/90 % of bytes; channel reset; destination disk full
  (loopback fs of fixed size); destination backend killed mid-apply; corrupted chunk
  bytes; corrupted item digest; truncated stream (no `StreamEnd`); plan rejected
  after preflight; source backend dropped mid-read. For every case assert: (a) the
  source fingerprint is unchanged, (b) the destination is left with no committed
  partial state (or a clearly-aborted state), (c) the error is phase-tagged and names
  the phase truthfully. **Done:** I-IMMUT + I-ERRORS proven under faults, not by
  construction. This is the flagship bank; expect ~30 cases.
- **V7.2 🔴** *(Sonnet)* — **Lint-enforce the no-panic rule.** Files: every crate
  root, `scripts/gates.sh`. Change: add
  `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing))]`
  (start with unwrap/expect/panic) to all `src` crates and fix the remaining
  production sites: `rb-transport/src/pool.rs` (`lock().expect("carrier pool
  mutex")` — use a poison-tolerant accessor), `auth.rs` + `direct.rs` HMAC
  `expect` (rewrite as an infallible construction or a returned error),
  `rb-filesystem/src/dest.rs` `parent().unwrap_or_else` paths (audit),
  `rb-s3` client construction. **Done:** gates enforce it and there is no
  `#[allow]` without a one-line justification comment.
- **V7.3** *(Sonnet)* — **Resource bounds review.** Files: `rb-core/src/wire.rs`,
  `rb-transport/*`. Change: `FRAME_LIMIT` is 16 MiB for *control* frames — a plan
  with a million items is a legitimate 16 MiB frame, but the destination must not
  allocate unboundedly elsewhere: cap plan item count (`MAX_PLAN_ITEMS`, explicit
  error above it), cap `PlanItem.name`/`meta` size, and cap the number of in-flight
  item hashers (F2) so a hostile stream cannot open millions of hasher entries.
  **Unit:** each cap has a rejection test. **Done:** every peer-controlled quantity
  is bounded.
- **V7.4** *(Sonnet)* — **Reconnect/backoff is unused.** Files:
  `rb-transport/src/reconnect.rs`, `client.rs`. Change: `Backoff`/`run` are vendored
  and compiled but nothing calls them — either wire auto-reconnect for the source
  provider registration (a dropped control connection today ends the run) or delete
  the module and say so in the docs. Decide explicitly; do not leave dead scaffolding.
  **Unit:** reconnect after a server restart resumes the registration (with a
  fault-injected server bounce). **Done:** no unused vendored module.

---

## Phase V8 — Session/UX completion

- **V8.1** *(Sonnet)* — **`run --config` ignores the `server:` section.** Files:
  `rust-backup/src/main.rs`. Change: either start the coordination server from the
  session config when `server:` is present (documented) or reject the key with a
  clear message. **Unit:** config with `server:` behaves as documented. **Done:** no
  silently-ignored config key (audit *all* keys with a round-trip test).
- **V8.2** *(Sonnet)* — **Two-target session e2e** (V1 7.3 debt). Files:
  `e2e/session_two_targets.sh`. Change: a YAML with two filesystem targets (source
  and destination on two channels) run with `--parallel-targets 2`; assert both
  complete, per-target progress lines appear, and `--fail-fast` stops scheduling
  after an induced failure. **Done:** the multi-target runner is proven.
- **V8.3** *(Haiku)* — **`--help` and docs match the binary.** Files: `USAGE.md`,
  `README.md`, `docs/modules/README.md`. Change: document `plan`, the `--config`
  merge semantics (F8), `-P key=value`, `--max-rate`, `--parallel-targets`,
  `--fail-fast`, the carriers restriction (F10) and the new progress output (F11).
  **Done:** every flag in the binary appears in `USAGE.md` and vice versa (check by
  diffing `--help` output against the doc in CI).
- **V8.4** *(Sonnet)* — **Exit codes.** Files: `rust-backup/src/main.rs`. Change:
  map error classes to distinct exit codes (2 config, 3 preflight, 4 plan rejected,
  5 integrity, 6 source-mutated, 7 transport) so scripts can branch; document them.
  **Unit:** classification function table-tested. **Done:** e2e scripts assert exit
  codes instead of grepping stderr.

---

## Phase V9 — S3 module completion

- **V9.1** *(Sonnet)* — abort injection for S3 (V1 5.5 debt): kill mid-multipart and
  assert (a) source objects/ETags unchanged, (b) no orphan multipart upload is left
  (`ListMultipartUploads` empty — `abort_upload` must run on every error path),
  (c) phase-tagged error. **Done:** T-S3-IMMUT green on MinIO.
- **V9.2** *(Sonnet)* — document/decide the prototype limits currently silent: ACLs
  (unsupported), object tags, storage class, versioned buckets, SSE headers,
  `x-amz-meta` fidelity, objects > 5 GiB (multipart part sizing), zero-byte objects,
  keys with unicode/`+`/`%`. Add a preflight check that *fails* when the source
  bucket uses a feature the restore cannot reproduce, instead of silently dropping
  it. **Unit:** each unsupported-feature detection. **Done:** no silent fidelity loss.
- **V9.3** *(Sonnet)* — real-AWS smoke (optional, credential-gated) documented in
  `docs/modules/S3.md`. **Done:** documented procedure + gated test.

---

## Phase V10 — CI + release readiness

- **V10.1 🔴** *(Haiku)* — CI matrix. Files: `.github/workflows/ci.yml`. Change: the
  workflow today runs `gates.sh` + the MinIO e2e only. Add jobs: `relay_smoke`,
  `session_two_targets`, postgres matrix (services: pg 12/16/18), mongo matrix
  (6/8), a `--no-default-features` test run (not just build), `cargo audit`,
  `cargo deny` (licences), and a netns job (privileged runner) for
  transport/filesystem. **Done:** CI reproduces every non-privileged e2e.
- **V10.2** *(Haiku)* — `CHANGELOG.md` (absent), version bump policy, and a
  `docs/plans/RESUME.md` pointer replacing the ⏯ block inside the V1 plan so state
  lives in one place. **Done:** files exist and are accurate.
- **V10.3** *(Haiku)* — `docs/TRANSPORT.md`: the transport contract as implemented
  (relay path, direct path, heartbeat/reaper, candidate sanitation, fallback rules,
  what is *not* yet ported from bore@nat-adv per V4). **Done:** an implementer can
  work on the transport without reading bore first.

---

## 1. Exit criteria for "first working version" (v0.1)

All of the following must be *observed*, not inferred:

1. `bash scripts/gates.sh` green, `0 ignored`. ✅ today
2. `e2e/relay_smoke.sh` green with no sudo/Docker. **(V2.1)**
3. `e2e/postgres_matrix.sh 10 12 14 16 18` green on a Docker host. **(V1.1–V1.3)**
4. `e2e/mongodb_matrix.sh 4 5 6 7 8` green. **(V1.4)**
5. `e2e/filesystem_netns_test.sh` green, root and non-root, incl. mid-abort. **(V2.2)**
6. `e2e/s3_minio_test.sh` green incl. abort injection. **(V9.1)** — base case ✅ today
7. `e2e/transport_netns_test.sh` green: direct used when reachable, relay when
   blocked, mid-transfer fallback. **(V2.3)**
8. Fault-injection bank green for all four modules. **(V7.1)**
9. No `unwrap`/`expect`/`panic!` in production paths, lint-enforced. **(V7.2)**
10. Item completeness + `Done` accounting enforced. **(V3.1, V3.2)**
11. TLS available on the coordination server. **(V5.1)**
12. `USAGE.md` matches `--help`; every module has a doc page. **(V1.5, V8.3)**

Nice-to-have for v0.1, required for v0.2: V4 (NAT traversal parity with
bore@nat-adv), V6 (multi-carrier), V5.3 (backend TLS), V9.2/V9.3.

---

## 2. Suggested execution order (dependency-aware)

```
V1.1 → V1.2 → V1.3 → V1.4        (live DB truth first; everything else builds on it)
V2.1 → V3.1 → V3.2 → V3.3        (session contract, provable without Docker)
V2.2 → V2.3                      (privileged e2e; needs a sudo host)
V7.2 → V7.1                      (lint first so new code lands clean, then the bank)
V5.1 → V10.1                     (TLS then CI that exercises it)
V1.5 → V8.3 → V10.2/V10.3        (docs, once behaviour is settled)
V4.1 → V4.2 → V4.3 → V4.4 → V4.5 → V4.6 → V4.7 → V4.8   (NAT parity, self-contained)
V6.1 → V6.2                      (last: it changes the data-plane protocol)
```

---

## 3. Appendix — bore alignment inventory (as of 2026-07-29)

`vendored-from-bore/` snapshot: **`bore-forked@nat-adv` = `5f7fe00`** (see
`vendored-from-bore/VENDORED_FROM.txt`). Files vendored: `holepunch.rs`,
`secret.rs`, `shared.rs`, `server.rs`, `client.rs`, `mux.rs`, `pool.rs`,
`transport.rs`, `prefixed.rs`, `reconnect.rs`, `auth.rs`, `transfer.rs`,
**`adaptive_nat.rs`**, **`portmap.rs`**, **`udp_diagnostic.rs`**, plus
`REF_udp_nat_netns_test.sh`, `REF_netns_test.sh`, `REF_test_gates.sh` and
`REF_docs/{NAT_TRAVERSAL,ADAPTIVE_NAT,PLAN_MANUAL_UDP_CANDIDATES,UDP_CONNECTION_IMPROVE}.md`.

| bore anchor | rust-backup status | Plan row |
|-------------|--------------------|----------|
| `valid_candidate`/`sanitize_candidates`/`MAX_UDP_CANDIDATES` | **ported** 2026-07-29 | F6 |
| `DirectListener::accept` stray-peer tolerance (bore H3) | already equivalent (auth errors `continue`) | — |
| `pool.rs` token-guard RAII regression test | n/a (no carrier tokens in rb-transport) | — |
| `StunTarget`/`discover_reflexive_chain`/`discover_reflexive_profile` | **missing** (single env-var STUN probe) | V4.1 |
| `adaptive_nat.rs` `NatProfile`/`NatPlan`/`classify_nat` | **missing** | V4.2 |
| `UdpCandidateOffer` v2 (kinds, priority, generation, profile), `UdpPunchV2` | **missing** (plain `addrs: Vec<SocketAddr>`) | V4.3 |
| `UdpTraversalSocket` + keyed connectivity checks + `listener/dialer_checks_then_quic` | **missing** (blind punch then QUIC race) | V4.4 |
| `portmap.rs` PCP/UPnP leases | **missing** | V4.5 |
| address cache (`remember`/`recall`/`invalidate`), `open_sibling` | **missing** | V4.6 |
| `port prediction` (`PREDICT_RANGE`) for symmetric NAT | **missing** | V4.2/V4.4 |
| `udp_diagnostic.rs` (`diagnose`, STUN responder, `run_stun_responder`) | **missing** — a `rust-backup diagnose udp` subcommand would pay for itself | V4.1 (responder), new V4.9 if wanted |
| `tests/support/natlab.rs`, `tests/nat_traversal_test.rs`, `scripts/udp_nat_netns_test.sh` | **missing** | V4.7 |
| bore control-send blocking edge | present in rb-transport too | V4.8 |
| `server_tls_from_pem` (server-side TLS) | **missing on the server** | V5.1 |

**Deliberately NOT ported** (out of scope for rust-backup): vhost/HTTP routing,
SSH gateway (`sshgw*.rs`), VPN (`vpn*.rs`), admin API/UI, weblog, basicauth,
`edge.rs`, `certinfo.rs`, `transfer.rs` disk staging/resume (rust-backup is
I-NOTEMP by contract). Keep this list in mind before "porting the rest of bore".
