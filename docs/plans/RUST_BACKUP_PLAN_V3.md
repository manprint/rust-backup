# rust-backup — Plan V3: final iteration to a QA-testable application

> Companion to `RUST_BACKUP_PLAN.md` (V1 = design) and `RUST_BACKUP_PLAN_V2.md`
> (V2 = remaining-work plan, now largely executed). **This is the last plan.**
> It was written on **2026-07-29** after a line-level re-review of the whole tree
> (14 976 LOC of Rust, 1 153 LOC of e2e shell) and it contains three things:
>
> 1. the **bug register** — real defects found in the code as it stands, each with
>    file, line, symptom and prescribed fix;
> 2. the **phases** that close them plus the remaining V2 debt;
> 3. the **QA exit criteria** — when every phase here is green, the binary is
>    handed to QA.
>
> When all phases of V3 are done, the deliverable is: a release binary, a
> documented QA procedure (`docs/QA_GUIDE.md`), and every e2e script in `e2e/`
> passing on a host with Docker + sudo.

---

## How to work from this document (read this first)

You are implementing, not designing. Everything you need is in the row you are
executing; do not re-explore the codebase to decide *what* to do.

**Row format:** `Model · Files · Change · Unit · e2e · Done`.

**Rules — non-negotiable**

1. **One sub-phase = one commit.** Never start the next sub-phase before the gate
   of the current one is green.
2. **The gate is `bash scripts/gates.sh`** ⇒ `cargo fmt --all --check` ·
   `cargo clippy --all-targets --all-features -- -D warnings` · `cargo build
   --all-features` · `cargo build --no-default-features` · `cargo test
   --all-features`. Plus the e2e script named in the row.
3. **"Gates green" is not "done".** A sub-phase is done when the *named* test
   exists, **ran**, and passed, with `0 ignored`. Never mark a row done because
   it compiles. Never add `#[ignore]` to make a gate pass.
4. **Zero regressions.** If a change breaks an existing test, the change is
   wrong until proven otherwise in the commit message.
5. **Never break the invariants** in `CLAUDE.md`: I-IMMUT (source read-only,
   fingerprint audited on every exit path), I-NOTEMP (no staging to disk, ≤1 MiB
   chunks), I-ERRORS (phase-tagged `rb_core::Result`, no `unwrap`/`expect`/
   `panic!` in production paths — clippy denies them per crate root), I-MODULAR
   (new backend = new crate, zero core change), I-BANDWIDTH (consumer-paced
   backpressure, never stripe one item across carriers), I-OBSERV.
6. **Every behaviour change updates its markdown** in the same commit
   (`docs/modules/*.md`, `docs/TRANSPORT.md`, `USAGE.md`, `CHANGELOG.md`).
7. **Report failures verbatim.** Live-run output goes in
   `docs/plans/V1_LIVE_RESULTS.md`; never paraphrase an error.
8. If a row turns out to be impossible as written, stop and write the reason in
   `docs/plans/RESUME.md`. Do not silently narrow the scope.

**Model assignment** (per `CLAUDE.md`): Haiku 4.5 = docs, mechanical/bulk edits,
script reconciliation. Sonnet 4.6 = default implementer for all code and tests.
Opus 4.8 = the ⟦OPUS GATE⟧ rows only (protocol/concurrency/invariant review).
State the model used at the top of every sub-phase report.

---

## 0. Measured state of the tree — 2026-07-29

* `bash scripts/gates.sh` → green. **125 test attributes, 0 ignored.**
* Crates: `rb-core` 1 741 · `rb-transport` 3 361 · `rb-postgres` 3 602 ·
  `rb-mongodb` 1 723 · `rb-filesystem` 1 025 · `rb-s3` 789 · `rust-backup` 959.
  No `todo!`/`unimplemented!`. Per-crate clippy denies unwrap/expect/panic.
* e2e present (13 files): `relay_smoke.sh`, `fault_matrix.sh`,
  `session_two_targets.sh`, `s3_minio_test.sh`, `postgres_matrix.sh`,
  `postgres_introspect.sh`, `mongodb_matrix.sh`, `filesystem_netns_test.sh`,
  `transport_netns_test.sh`, `bandwidth_netem.sh`, `full_matrix.sh`, `lib.sh`.
* CI jobs: `gates`, `s3-minio`, `relay-smoke`, `session-two-targets`,
  `fault-matrix`, `postgres-matrix`, `mongodb-matrix`.
* Docs present: `USAGE.md`, `CHANGELOG.md`, `docs/TRANSPORT.md`,
  `docs/modules/{README,POSTGRES,MONGODB,FILESYSTEM,S3}.md`,
  `docs/plans/{RESUME,V1_LIVE_RESULTS}.md`.

**Proven (ran, passed):** postgres matrix 10/12/14/16/18 (25/25) · mongodb matrix
4/5/6/7/8 (20/20) · MinIO base case · relay smoke (plain + TLS) ·
session two-targets · fault matrix (3/3, small).

**Never run:** every sudo/netns script (`filesystem_netns_test.sh`,
`transport_netns_test.sh`, `bandwidth_netem.sh`), S3 abort injection, real AWS.

**Not implemented:** V2 rows V4.4–V4.7 (connectivity checks, portmap, address
cache, NAT lab), the full ~30-case fault bank (V7.1), V7.4 reconnect decision,
V4.8 control-send bound, the measured multi-carrier speed proof.

### Progress observed after this plan was started — 2026-07-29

This status supersedes only the preceding initial snapshot; a row is marked
complete below only when its named command has been observed.

| Area | Observed status |
|---|---|
| F1.1 ordered item-pinned restore | implemented; `cargo test -p rb-core --test session_test multi_carrier_run_ok --all-features` and real relay 4-carrier smoke pass |
| F1.2 negotiated count + bounded stream setup | implemented and covered by real 1/4-carrier relay smoke; current peers negotiate a separate control/data layout, while missing legacy fields default to one multiplexed carrier |
| F1.3 explicit multi-carrier abort | implemented in both directions; dedicated fault-bank expansion remains open |
| F1.4 bounded source abort observation | complete: control abort interrupts pacing and blocked writes; the 50-item regression bounds backend work to at most three started items, preserves the reason, and completes all 50 on the happy path |
| F1.5 carrier caps/trailer validation | filesystem cap and conservative module caps implemented; core/PostgreSQL/MongoDB trailer validation implemented; full per-module adversarial matrix remains open |
| F1.6 carrier docs | implemented in `docs/TRANSPORT.md` and `USAGE.md` |
| F5.3 typed exit codes | typed `BackupError` paths preserved at the CLI boundary; wording-independence unit test passes |
| F6.1 reflexive profile | STUN observations now populate reflexive candidates and mapping classification; F6.2, F6.5 and F6.7 remain open |
| F7.1/F7.4 | QA guide and non-privileged default matrix implemented; privileged scripts are opt-in only |
| F4.1 multipart abort cleanup | implemented and observed on MinIO: injected post-part failure leaves no incomplete upload and source listing unchanged |
| F5.1 reconnect decision | complete: unimplemented reconnect scaffold removed; a dropped coordination path fails explicitly and cannot resume a half-applied restore |
| F7.2 parity | complete: `scripts/help_parity.sh` is part of `scripts/gates.sh` and checks every clap help flag against `USAGE.md` |
| F2.1 protocol fault injections | reusable 26-case bank covers EOF/no `StreamEnd`, incomplete items, offset gaps, oversize/corrupt chunks, item hashes/totals, duplicate trailer, carrier hello, abort and plan bounds over 1/4 carrier parser paths |
| Completion race found by severe audit | fixed with required `CompleteAck`/`CompleteAckAck`; source waits for destination apply + final verification and destination waits until receipt is confirmed; one/four-carrier regressions and MinIO pass |
| Consumer-first relay race found by severe audit | relay waits a bounded 10 s for provider registration instead of dropping an early consumer stream; deterministic regression and parallel two-target e2e pass |
| Dependency audit | vulnerable dependency paths updated or removed; `cargo audit` passes over 420 resolved dependencies |
| F2.4 real disk-full | ext4 loop-device e2e passes 5/5: phase-tagged ENOSPC, bilateral bounded failure, peer reason, source immutability and partial-file cleanup |
| F3.1 privileged filesystem | passes 4/4; root metadata including a real post-chown 04755 mode, non-root fallback, and abort immutability observed |
| F3.2 privileged transport | passes 9/9 after adding the missing QUIC stream-ready marker; direct and setup-fallback transfers complete, active-path loss fails closed with immutable source and partial cleanup |
| F3.3 bandwidth/backpressure | passes 6/6; final aggregate: 200 MiB over real 5 Mbit/s egress took 354 s at 9,908 KiB source RSS; 256 KiB/s cap took 66 s at 10,228 KiB RSS |
| F6.3 v2 broker riders | candidates, priority/kind and NAT profile are sanitized then forwarded end-to-end; old frames retain serde defaults and frame cap coverage |
| F6.6 learned-address cache | bounded process-local cache is integrated into direct setup: cached peer first, remember successful path, invalidate on direct failure |
| F6.4 authenticated checks | implemented before QUIC on the same UDP socket: HMAC key derivation, bounded candidate groups, invalid-packet rejection and loopback nomination test |
| F6.8 sibling QUIC carriers | implemented: control uses the primary authenticated QUIC connection; each later carrier opens/accepts an authenticated sibling and independently falls back to relay |

Commands observed after these changes:

```text
cargo test -p rb-core --all-features                         PASS, 0 ignored
cargo test -p rb-transport --all-features                    PASS, 0 ignored
cargo test -p rust-backup --all-features                     PASS, 0 ignored
RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh           PASS=4 FAIL=0
RUST_BACKUP_E2E_TLS=1 RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh
                                                               PASS=4 FAIL=0
RUST_BACKUP_E2E_CARRIERS=1 bash e2e/relay_smoke.sh × 20        PASS=20 FAIL=0
bash scripts/gates.sh                                        ALL GATES PASSED
bash e2e/s3_minio_test.sh                                    PASS (base + injected multipart abort)
bash e2e/s3_minio_test.sh                                    PASS again (flakiness check)
bash e2e/postgres_matrix.sh 10 12 14 16 18                   PASS=25 FAIL=0
bash e2e/mongodb_matrix.sh 4 5 6 7 8                         PASS=20 FAIL=0
bash e2e/full_matrix.sh                                     PASS=8 FAIL=0
RUST_BACKUP_PRIVILEGED=1 bash e2e/full_matrix.sh            PASS=12 FAIL=0
cargo audit                                                 PASS (420 dependencies)
sudo -n e2e/filesystem_netns_test.sh                        PASS=4 FAIL=0
sudo -n e2e/filesystem_disk_full.sh                         PASS=5 FAIL=0
sudo -n e2e/transport_netns_test.sh                         PASS=9 FAIL=0
sudo -n e2e/bandwidth_netem.sh                              PASS=6 FAIL=0
```

The installed path-wildcard NOPASSWD rule permits and has observed all privileged
scripts. Because the matched repository directory is user-writable, that rule is
equivalent to allowing arbitrary root code; retain it only on a disposable test
host, or replace it with a root-owned, narrowly validated runner.
Not yet complete: the Docker-backed ≥20-case backend fault matrix, remaining NAT
rows, real AWS, and the multi-carrier speed comparison.

---

## 1. Original bug register (found by the initial review)

The register below is the input to this plan, not the current state. The
observed-status table above and the audit report linked from this directory are
authoritative for fixes that have since landed.

Severity: **S1** = wrong data / silent corruption or hang · **S2** = wrong
error/behaviour under fault · **S3** = hardening / hygiene.

| id | S | Where | Symptom | Fix lands in |
|----|---|-------|---------|--------------|
| **B1** | S1 | `rb-core/src/channel.rs:371-457` (`MultiStreamChunkSource`) vs `rb-filesystem/src/dest.rs:103-110`, `rb-postgres/src/dest.rs:245-295`, `rb-mongodb/src/dest.rs:213-250`, `rb-s3/src/lib.rs:497-513` | With `carriers > 1` the destination merges carriers through one `mpsc` in **arrival** order, so item *N+1* can start before item *N* finishes. Every module destination assumes items arrive **in plan order and contiguous**: filesystem errors `"filesystem items arrived out of order"`; postgres closes the open `COPY` and reopens the *wrong* one; mongodb flushes the wrong batch; s3 iterates `for item in &plan.items` and cannot tolerate any interleaving. On loopback with small items the race usually does not fire — which is why the 4-carrier smoke passed. Under real RTT it corrupts or fails. | **F1.1** |
| **B2** | S1 | `rb-transport/src/channel.rs:60-97` + `client.rs:92,162` | `carriers` is taken from each side's own CLI/YAML value and is **never negotiated on the wire**. Source `--carriers 4` + destination default `1` ⇒ source `accept_stream()` blocks forever on carrier 2 (relay accept has no timeout) and the destination waits for `Done` ⇒ deadlock until the 60 s coord reaper, then an unclear error. | **F1.2** |
| **B3** | S2 | `rb-core/src/session.rs:450-453` | Multi-carrier destination: on `stream_in` failure it does `apply_result?` and returns **without sending any `Abort`**. The single-carrier path (`session.rs:363-374`) does send one. The source keeps streaming into a dead peer. | **F1.3** |
| **B4** | S2 | `rb-core/src/session.rs:190-225` | Multi-carrier source failure calls `data_sink.abort()` on the **data** streams only; the control stream gets nothing, so a destination already past `stream_in` sees EOF on control instead of a reason. | **F1.3** |
| **B5** | S2 | `rb-core/src/session.rs:148-186` | V2.3/V2.4 promised the source "notices a peer abort promptly (check the read half between items)". It does **not**: the source never reads while streaming, so a destination `Abort` is discovered only when a write finally fails. A destination that dies on item 1 of 10 000 keeps the source reading its backend for the whole run. | **F1.4** |
| **B6** | S2 | `rb-postgres/src/dest.rs:287`, `rb-mongodb/src/dest.rs:240` | `ChunkEvent::ItemEnd { .. }` ignores `item_id` and `total`: it closes whatever sink is open. Combined with B1 this closes the wrong `COPY`; on its own it means a hostile/buggy source can end item *A* with item *B*'s trailer. `rb-filesystem/src/dest.rs:141` does verify `item_id`/`total`/`blake3` — copy that shape. | **F1.5** |
| **B7** | S3 | `rb-core/src/channel.rs:269-298` | `ItemEnd.total` is never compared against the bytes actually received for that item (only the digest is). A truncated item with a matching digest is impossible, but the mismatch should still be a distinct, phase-tagged error naming the item. | **F1.5** |
| **B8** | S3 | `rb-core/src/channel.rs:30,251-258` | `MAX_IN_FLIGHT_ITEM_HASHERS` is enforced **per stream**, so with N carriers the real bound is `N × 1024`. Bound it per session. | **F1.5** |
| **B9** | S2 | `rb-core/src/channel.rs:377-407` | When one carrier reader errors, the sibling reader tasks are left running and are only stopped when the `Receiver` drops; there is no explicit cancellation and no test that they are not leaked across a long session. | **F1.1** |
| **B10** | S3 | `rust-backup/src/main.rs:453-473` | `exit_code` classifies by **substring matching on the error text** (`"[Verify]"`, `"transport:"`). Any rewording of a message silently changes a shell contract. Classify from the typed `BackupError`/`Phase` before it is flattened into `anyhow`. | **F5.3** |
| **B11** | S2 | `rb-transport/src/server.rs` control loop | A peer that stops *reading* can block a control send inside the `select!`, so the recv-deadline reaper never fires and the channel id zombies (this is bore's known edge; V2 row V4.8 was never done). | **F5.2** |
| **B12** | S3 | `rb-transport/src/reconnect.rs` (89 LOC) | `Backoff`/`run` are compiled but called from nowhere. Either wire auto-reconnect for the provider registration or delete the module. Dead scaffolding in a shipped release is a defect. | **F5.1** |

---

## Phase F1 — 🔴 Data-plane correctness (carriers). Blocks everything.

> Rationale: `--carriers > 1` is currently a **corruption/hang risk** (B1, B2).
> Either it is made correct here, or it is disabled for v0.1. This phase makes it
> correct **and** adds the capability gate, because postgres cannot honestly
> support concurrent items on one connection.
> No Docker and no sudo needed for any row in F1.

- **F1.1 🔴 ⟦OPUS GATE⟧** *(Opus reviews the design note, Sonnet implements)* —
  **Ordered, item-pinned demux on the destination.**
  **Files:** `rb-core/src/channel.rs` (`MultiStreamChunkSource`), `rb-core/src/session.rs`.
  **Change:** replace the "N reader tasks → one mpsc" merge with a **plan-ordered
  pull**. The item→carrier map is deterministic and already defined by the sink:
  `carrier = item_id as usize % carriers` (`channel.rs:326-328`). So:
  * `MultiStreamChunkSource::new_ordered(streams, expected_item_ids_in_plan_order, progress)`
    keeps the N substreams **unspawned** (no tasks, no mpsc) and holds a cursor
    over the expected item ids.
  * `next()` reads **only** from the carrier owning the current expected item,
    until that item's `ItemEnd`; then it advances the cursor to the next expected
    item and switches carrier. Metadata-only items (`expects_data() == false`,
    `plan.rs:124`) are skipped by the cursor, never awaited.
  * Result: the module-facing event order is *identical* to single-carrier
    (contiguous, plan-ordered), while the other N-1 carriers keep filling their
    transport windows in parallel — which is where the speedup comes from. No
    buffering is introduced, so I-NOTEMP and I-BANDWIDTH hold: a stalled consumer
    stalls its carrier's window and therefore the source's writes.
  * A frame arriving on the wrong carrier for the current item is a phase-tagged
    `Transfer` error naming both ids (this is what catches a mismatched sink).
  * Delete the spawned-task path so **B9** cannot exist; if any task remains,
    give it explicit cancellation on drop and a test.
  **Unit** (`rb-core/tests/session_test.rs` + `channel.rs` unit tests):
  (a) 4 carriers, 9 data items, a destination that records the event order ⇒ the
  order equals the single-carrier order exactly; (b) same plan with two
  metadata-only items interleaved ⇒ still passes; (c) a source that writes item 3
  on the wrong carrier ⇒ phase-tagged error naming item 3; (d) a byte-identity
  test: the same source/plan with `carriers = 1` and `carriers = 4` yields the
  same `completion_digest` and the same per-item digests; (e) no task leak: after
  a failed run all substreams are dropped (assert via a stream whose `Drop`
  flips an `Arc<AtomicBool>`).
  **e2e:** `RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh` with a fixture of
  **≥ 200 files of mixed sizes** (extend `rb_seed_filesystem_fixture` in
  `e2e/lib.sh` if needed) — the current fixture is too small to expose B1.
  **Done:** filesystem restore is byte-identical with 1 and 4 carriers over the
  relay, on a fixture large enough that items overlap in flight.

- **F1.2 🔴** *(Sonnet)* — **Negotiate the carrier count on the wire.**
  **Files:** `rb-core/src/{wire,channel,session}.rs`, `rb-transport/src/channel.rs`,
  `rust-backup/src/main.rs`, `docs/TRANSPORT.md`, `USAGE.md`.
  **Change:** the **source's** requested count is authoritative and travels in the
  plan exchange; the destination replies with the count it will actually open.
  Concretely: add `carriers: u32` to the plan-exchange frames (extend
  `ControlFrame::PlanAck` with `carriers: u32`, defaulting via `#[serde(default)]`
  so an old peer still deserializes), and have both sides use
  `agreed = min(source_requested, destination_capable)` before opening/accepting
  any data substream. A destination that cannot honour `> 1` (see F1.5) replies
  `1` and the source silently downgrades, logging it once at `info`.
  Additionally: bound the substream setup — wrap each `accept_stream()` /
  `open_stream()` for data carriers in a timeout (reuse the plan-exchange budget
  helper `session.rs:39-57`) so a mismatch can never hang; the error must name
  which side and which carrier index stalled.
  **Unit:** (a) serde matrix old↔new for `PlanAck`; (b) source 4 / destination 1
  ⇒ run completes on 1 carrier, no hang, log contains the downgrade;
  (c) a destination that never opens carrier 2 ⇒ `Connect`-phase error within the
  injected budget, not a hang.
  **Done:** no carrier-count configuration can deadlock a run.

- **F1.3 🔴** *(Sonnet)* — **Abort parity on the multi-carrier paths (B3, B4).**
  **Files:** `rb-core/src/session.rs`.
  **Change:** in `destination_stream_multi`, on `stream_in` failure send
  `ControlFrame::Abort { reason }` on the **control** stream (best-effort, ignore
  send errors) before returning — mirror `session.rs:363-374`. In
  `source_stream_multi`, after `data_sink.abort(...)`, also send
  `ControlFrame::Abort { reason }` on the control stream. Factor the three
  copy-pasted "check items, read Done, verify bytes+digest" blocks
  (`session.rs:376-432` and `455-514`) into one helper used by both paths so they
  cannot drift again.
  **Unit:** (a) 4 carriers, destination fails on item 1 ⇒ the source's run ends
  with the destination's reason, not a broken pipe; (b) 4 carriers, source fails
  mid-item ⇒ destination error text contains `"source aborted"`; (c) the shared
  helper is exercised by both the 1-carrier and 4-carrier tests.
  **Done:** both directions abort explicitly with a reason on every carrier count.

- **F1.4 🔴** *(Sonnet)* — **The source notices a peer abort promptly (B5).**
  **Files:** `rb-core/src/session.rs`, `rb-core/src/channel.rs`.
  **Change:** between items (in `PacedSink::finish_item`, after the inner call)
  poll the control read half without blocking: a pending `ControlFrame::Abort`
  ⇒ return a phase-tagged `Transfer` error carrying the destination's reason, so
  `stream_out` unwinds and the source stops reading its backend. Use a
  non-blocking read (`tokio::time::timeout(Duration::ZERO, …)` or a `try_read`
  wrapper) — **never** block waiting for a frame that will normally not exist,
  and never consume bytes belonging to another frame type (buffer partial reads).
  Document the check in `docs/TRANSPORT.md`.
  **Unit:** a destination that aborts after item 1 of 50 ⇒ the source's
  `stream_out` is asked for at most a small bounded number of further items
  (assert an item counter ≤ 3), and the returned error names the destination's
  reason. Also assert the happy path emits **zero** spurious errors over 50 items.
  **Done:** a dead destination stops the source in bounded work, not at end of run.

- **F1.5 🔴** *(Sonnet)* — **Per-module carrier capability + trailer validation
  (B6, B7, B8).**
  **Files:** `rb-core/src/module.rs` (trait), `rb-core/src/channel.rs`,
  `rb-postgres/src/dest.rs`, `rb-mongodb/src/dest.rs`, `rb-s3/src/lib.rs`,
  `rb-filesystem/src/dest.rs`, `docs/modules/*.md`, `USAGE.md`.
  **Change:**
  1. Add `fn max_carriers(&self) -> u32 { 1 }` to `BackupModule` (default 1, so
     I-MODULAR holds and no module is forced to change). `rb-filesystem` returns
     32; `rb-postgres`, `rb-mongodb`, `rb-s3` return 1 for v0.1 with a one-line
     `//` justification each (postgres: one `COPY` per connection; mongodb: one
     batch accumulator; s3: strict plan-order multipart loop). The destination
     uses this value in the F1.2 negotiation.
  2. In `rb-postgres`/`rb-mongodb` `apply_data`, validate the `ItemEnd` trailer
     like `rb-filesystem/src/dest.rs:141` does: the `item_id` must equal the open
     item, and `total` must equal the bytes fed to that item; otherwise a
     phase-tagged `Apply`/`Integrity` error naming the item.
  3. In `rb-core/src/channel.rs`, compare `ItemEnd.total` against the bytes the
     source actually delivered for that id and make the in-flight-hasher bound
     session-wide, not per stream.
  **Unit:** (a) each module's `max_carriers` asserted; (b) a `carriers=4` request
  against postgres/mongodb/s3 downgrades to 1 and logs it (no error);
  (c) mismatched-`item_id` trailer rejected per module; (d) mismatched `total`
  rejected in core with the item id in the message; (e) the hasher cap rejects at
  the session level regardless of carrier count.
  **Done:** `--carriers > 1` is only ever used by a module that can actually
  honour it, and a wrong trailer is never applied.

- **F1.6** *(Haiku)* — **Document the carrier contract.**
  **Files:** `docs/TRANSPORT.md`, `USAGE.md`, `CHANGELOG.md`.
  **Change:** one section: item→carrier mapping, the ordered-pull rule, the
  negotiation and downgrade, per-module `max_carriers`, and the explicit statement
  that a single item is never striped (I-BANDWIDTH). Note the `PlanAck` field
  addition in `CHANGELOG.md` as a wire-compatibility event.
  **Done:** an implementer can reason about carriers without reading the code.

---

## Phase F2 — 🔴 The fault-injection bank (V2 row V7.1, still ~90 % missing)

> Today: `e2e/fault_matrix.sh` (71 LOC, one live case) plus a few in-process
> abort tests. The plan promised ~30 cases across four modules. This phase is the
> flagship proof of I-IMMUT and I-ERRORS. **No Docker for F2.1/F2.2; Docker for
> F2.3; sudo only for the disk-full case (F2.4).**

For **every** case the assertions are the same three, and they must be written as
helpers so no case can forget one:
* **A** — the source fingerprint is unchanged (I-IMMUT), audited on the failing
  path too;
* **B** — the destination holds no committed partial state, or a state explicitly
  marked aborted (per module: no half-created schema, no partial collection, no
  orphan multipart upload, no half-written file left as if complete);
* **C** — the error is phase-tagged and the phase is **truthful** (a failure while
  applying is `Apply`, not `Transfer`).

- **F2.1 🔴** *(Sonnet)* — **In-process harness.**
  **Files:** new `crates/rb-core/tests/fault_injection.rs`, plus test-only helpers
  in `rb-core` (behind `#[cfg(test)]` or a `test-support` feature — must not leak
  into the release build).
  **Change:** build a reusable in-memory channel that can inject, at a chosen byte
  offset or item index: channel reset, truncation (no `StreamEnd`), a corrupted
  chunk byte, a corrupted `ChunkStart.blake3`, a corrupted `ItemEnd.blake3`, a
  wrong `ItemEnd.item_id`, a duplicate `ItemEnd`, a wrong `Done.total_bytes`, a
  wrong `Done.blake3`, an unexpected frame in place of `Done`, a peer that never
  answers the plan, a plan rejected after preflight, a source that skips an item,
  a source that emits an item absent from the plan. Fault points must be
  expressible as data (a `Fault` enum + offset), not by copy-pasting a test.
  **Unit:** the 14 faults above × `carriers ∈ {1, 4}` ⇒ each asserts A/B/C.
  **Done:** `cargo test --all-features` runs the bank with `0 ignored`.

- **F2.2 🔴** *(Sonnet)* — **Backend-side faults, per module, in process.**
  **Files:** `crates/rb-core/tests/fault_injection.rs` plus per-module test files
  where the module owns the state (`rb-filesystem/tests/…`, and mock-backed cases
  for postgres/mongodb/s3 that need no server).
  **Change:** cases: source backend disappears mid-read; destination backend
  refuses mid-apply; destination backend fails on the *last* item; preflight
  passes then apply fails; an item whose declared size disagrees with the bytes
  produced; zero-byte item; an item id at `u32::MAX`; a plan at
  `MAX_PLAN_ITEMS` and one item over it; an item name at
  `MAX_PLAN_ITEM_NAME_BYTES` and one byte over; meta at the cap and over it.
  **Done:** ≥ 12 further cases, each asserting A/B/C.

- **F2.3 🔴** *(Sonnet)* — **Live fault matrix, all four modules.**
  **Files:** `e2e/fault_matrix.sh` (extend from 1 to the full bank), `e2e/lib.sh`.
  **Change:** parameterise over module × fault. Faults: coordination server killed
  mid-transfer; destination process `kill -9` at ~10 %, ~50 %, ~90 % of bytes
  (drive from the progress log or from bytes written); source process killed
  mid-transfer (exists today — keep); destination backend container stopped
  mid-apply (postgres/mongodb/MinIO); channel reset by killing the relay
  connection. After every case assert A/B/C using module-specific probes:
  postgres ⇒ the target database is absent or empty and no half-created schema
  exists; mongodb ⇒ the target collections are absent or empty; s3 ⇒
  `ListMultipartUploads` is empty **and** no partial object is visible;
  filesystem ⇒ no file exists that is shorter than its plan size while being
  reported complete. Source-side probe in all cases: `rb_tree_digest` /
  fingerprint unchanged. Keep the bore script shape: `set -u`,
  `trap cleanup EXIT INT TERM`, PASS/FAIL counters, non-zero exit on any failure,
  a final summary line.
  **Done:** the script prints `PASS=N FAIL=0` with **N ≥ 20** on a Docker host,
  and the run is recorded in `docs/plans/V1_LIVE_RESULTS.md`.

- **F2.4 ✅** *(Sonnet)* — **Destination disk-full (needs sudo for the loop device).**
  **Files:** `e2e/filesystem_disk_full.sh`, `docs/QA_GUIDE.md`.
  **Change:** create a small fixed-size loopback filesystem, restore a payload
  larger than it, assert `ENOSPC` surfaces as a phase-tagged `Apply` error naming
  the item, assert A and B. The exact-path script rejects non-root invocation.
  **Done:** 5/5 passes under `sudo -n`, including bounded bilateral abort and
  loop/mount cleanup.

- **F2.5** *(Sonnet)* — **Immutability under concurrent load** (the I-IMMUT
  stress case). **Files:** `e2e/fault_matrix.sh`.
  **Change:** while a filesystem source streams, a second process writes into a
  **sibling** directory (not the source root), and separately, a case where the
  source root *is* modified ⇒ the run must fail with `SourceMutated` and exit
  code 6. Also assert source `atime`s are unchanged (`rb_atime_manifest` in
  `e2e/lib.sh`).
  **Done:** both directions proven — no false positive, no false negative.

---

## Phase F3 — ✅ Execute the privileged e2e

> The three scripts exist but have never been invoked. Until they run, transport
> fallback, ownership fidelity and I-BANDWIDTH are unproven. Each row is
> "run it, then fix what it surfaces" — fixes land with an in-process test.

- **F3.1 ✅** *(Sonnet)* — **Run `sudo -n e2e/filesystem_netns_test.sh`.**
  Expect breakage in: uid/gid restore without `CAP_CHOWN`, hardlink topology,
  symlink targets, `4755`/`0400` modes, mtime rounding, empty dirs/files.
  **Done:** N pass / 0 fail for the root case **and** the non-root case, with the
  documented warning present in the non-root run; every fix has a unit test in
  `rb-filesystem`; output recorded in `V1_LIVE_RESULTS.md`.
- **F3.2 ✅** *(Sonnet)* — **Run `sudo -n e2e/transport_netns_test.sh`.**
  Direct path is used when UDP is reachable and relay fallback completes when
  punch ports are dropped before setup. The original demand that a
  **mid-transfer** UDP drop resume over relay was corrected by the severe audit:
  replaying an unacknowledged byte-stream prefix is unsafe and contradicts F5.1.
  Active loss instead fails both peers before the watchdog, preserves source
  tree/atimes, removes the active partial item and reports `[Transfer]`. Watch the
  `SO_REUSEADDR` rule on the punch socket (never set it — `EADDRINUSE` ⇒
  ephemeral port), `STREAM_READY` written before splice, and that UDP never gates
  channel liveness (see `CLAUDE.md`).
  **Done:** 9/9, recorded.
- **F3.3 ✅** *(Sonnet)* — **Run `sudo -n e2e/bandwidth_netem.sh`** (I-BANDWIDTH).
  Asymmetric netem (e.g. source 100 Mbit, destination 5 Mbit, 80 ms RTT), ≥ 200 MiB.
  Assert: transfer completes; every item digest verifies; source `VmRSS` stays
  under a fixed ceiling sampled every second (no read-ahead buffering);
  `--max-rate` visibly caps throughput.
  **Done:** the RSS ceiling and the rate cap are asserted numerically, recorded.
- **F3.4** *(Sonnet)* — **Multi-carrier speed proof** (only after F1).
  **Files:** `e2e/bandwidth_netem.sh`.
  **Change:** run the same netem transfer with `--carriers 1` and `--carriers 4`
  on the **filesystem** module; assert identical digests and that 4 carriers are
  not slower than 1 (report the measured ratio; do not assert a specific speedup —
  assert no regression and log the number).
  **Done:** the number is recorded in `V1_LIVE_RESULTS.md`.

---

## Phase F4 — S3 module completion (V2 rows V9.1–V9.3)

- **F4.1 🔴** *(Sonnet)* — **Abort injection on MinIO (T-S3-IMMUT).**
  **Files:** `e2e/s3_minio_test.sh`, `rb-s3/src/lib.rs` if a leak is found.
  **Change:** kill the destination mid-multipart (a multi-part object ⇒ use an
  object > 2 × part size). Assert: source objects and ETags unchanged;
  `ListMultipartUploads` empty (every error path must reach `abort_upload`,
  `lib.rs:719-735`); the error is phase-tagged. Also inject a failure **between**
  `upload_part` calls and after `complete_multipart_upload` starts.
  **Done:** no orphan upload in any injection point.
- **F4.2** *(Sonnet)* — **Fidelity preflight completeness.**
  **Files:** `rb-s3/src/lib.rs` (`unsupported_object_features`,
  `validate_source_fidelity`), `docs/modules/S3.md`.
  **Change:** verify by test that each of these **fails** preflight rather than
  silently degrading: ACLs, object tags, non-standard storage class, versioned
  bucket, SSE headers, `x-amz-meta` beyond what is restored, objects > 5 GiB
  (part sizing), zero-byte objects (must **work**, not fail), keys containing
  unicode / `+` / `%` / a trailing slash.
  **Unit:** one detection test per feature; a positive test for zero-byte and for
  the awkward key set.
  **Done:** no silent fidelity loss; each decision documented in `S3.md`.
- **F4.3** *(Haiku)* — **Real-AWS procedure**, credential-gated and documented in
  `docs/modules/S3.md`; the test skips loudly without credentials.
  **Done:** procedure documented, gate present.

---

## Phase F5 — Close the hygiene defects (B10–B12) + control-plane edge

- **F5.1** *(Sonnet)* — **Decide `reconnect.rs` (B12).**
  **Files:** `rb-transport/src/{reconnect,client}.rs`, `docs/TRANSPORT.md`.
  **Change:** preferred: wire `Backoff` into the provider's control registration so
  a dropped control connection retries with backoff and the run survives a coord
  restart; the data plane must **not** silently resume a half-applied transfer —
  a reconnect after the plan exchange aborts the run with a clear error. If
  wiring is out of scope, **delete** the module and say so in `docs/TRANSPORT.md`.
  **Unit:** with wiring — registration resumes after an injected server bounce;
  without — no dead module remains (`cargo build` has no unused-module warning and
  the doc states the decision).
  **Done:** explicit decision, no dead scaffolding.
- **F5.2** *(Sonnet)* — **Bound every control-plane send (B11).**
  **Files:** `rb-transport/src/server.rs`.
  **Change:** wrap each control send in a bounded `timeout`, or move sends to a
  task with a bounded queue, so a peer that stops reading cannot block the
  `select!` and starve the recv-deadline reaper (`SECRET_CTRL_TIMEOUT` = 60 s).
  **Unit:** a peer that registers and then never reads is reaped within the
  deadline; a healthy peer is not affected.
  **Done:** no zombie channel id under a stuck reader.
- **F5.3** *(Sonnet)* — **Typed exit codes (B10).**
  **Files:** `rust-backup/src/main.rs`, `rb-core/src/error.rs`, `USAGE.md`.
  **Change:** carry the typed `BackupError` (or a small `ExitClass` derived from
  it at the boundary) to `main` instead of classifying `anyhow` text; keep the
  existing numeric contract (2 config, 3 preflight, 4 plan rejected, 5 integrity,
  6 source-mutated, 7 transport). Keep the text-based mapping only as a
  documented fallback for errors that genuinely originate as `anyhow`.
  **Unit:** table test mapping each `BackupError` variant/`Phase` to its code;
  a test that rewording a message does **not** change the code.
  **Done:** exit codes survive a message rewrite; e2e scripts assert codes.

---

## Phase F6 — NAT traversal parity (V2 rows V4.1/V4.3–V4.7) — **v0.2, not a QA blocker**

> Keep rust-backup's own wire types (`rb-transport/src/proto.rs`); do **not**
> import bore's `ClientMessage`/`ServerMessage`. Read the named bore anchor in
> `vendored-from-bore/` before writing code. Relay must always stay warm; UDP
> never gates channel liveness; direct setup must never delay the relay fallback
> beyond `DIRECT_SETUP_TIMEOUT` (`rb-transport/src/channel.rs:19`).

- **F6.1 ⟦OPUS GATE⟧** *(Sonnet)* — **Reflexive profile.** Anchor: `holepunch.rs`
  `StunTarget`/`SelectedStun`/`discover_reflexive_chain`/
  `discover_reflexive_profile`/`resolve_live_stun_targets`. Today
  `rb-transport/src/client.rs:324-440` has a bounded target chain but produces no
  populated profile (address-dependent vs port-dependent mapping is not derived).
  **Files:** new `rb-transport/src/stun.rs` + `client.rs`. **Unit:** target-list
  table; profile derived from synthetic observations; a dead target does not fail
  the gather. **Done:** a reflexive candidate + mapping class exist without
  operator configuration.
- **F6.2 ⟦OPUS GATE⟧** *(Sonnet)* — **Classifier and plan.** Anchor:
  `adaptive_nat.rs`, `holepunch.rs::classify_nat`. `rb-transport/src/adaptive_nat.rs`
  exists (131 LOC) but only as types with `NatProfile::default()` sent on the wire
  (`client.rs:245`, `server.rs:188`). Port the pure classifier, plan builder,
  punch pacing/window and port prediction (`PREDICT_RANGE`) for symmetric NAT.
  Keep the module pure (no I/O). **Unit:** the bore classification matrix + plan
  mode per class pair. **Done:** the plan is derived, not defaulted.
- **F6.3** *(Sonnet)* — **Candidate offer v2 riders.** Anchor: `shared.rs`
  `UdpCandidateOffer`/`UdpNatProfile`/`UdpPunchV2`, `secret.rs` broker rider.
  Propagate kinds/priority/generation + the real profile through the coordination
  server. Backward compatible both ways; keep the frame inside
  `MAX_FRAME_LENGTH` (port bore's worst-case-frame-size test); the F6-era
  sanitation (`shared.rs` `valid_candidate`/`sanitize_candidates`,
  `MAX_UDP_CANDIDATES = 16`) must apply to the new shape.
  **Unit:** old→new and new→old serde matrices; worst-case frame size; sanitation.
- **F6.4 ⟦OPUS GATE⟧** *(Sonnet)* — **Keyed connectivity checks before QUIC.**
  Anchor: `holepunch.rs` `UdpTraversalSocket`, `CheckRole`/`CheckConfig`/`CheckPlan`,
  `plan_check_groups`, `plan_check_window`, `derive_check_key`,
  `run_connectivity_checks`, `listener_checks_then_quic`, `dialer_checks_then_quic`.
  One socket for checks and QUIC; never `SO_REUSEADDR` on the punch socket;
  `EADDRINUSE` ⇒ ephemeral. **Unit:** check-group planning table; wrong key
  rejected with a counter; loopback checks → QUIC handshake.
- **F6.5** *(Sonnet)* — **Port mapping (PCP + UPnP)** behind a default-off
  `portmap` feature that must not affect `--no-default-features`. Anchor:
  `portmap.rs`. All failures are silent downgrades. **Unit:** PCP codec vectors;
  lease renew + drop-release against a mock backend.
- **F6.6** *(Sonnet)* — **Learned-address cache.** Anchor: `holepunch.rs`
  `remember`/`recall`/`invalidate`. `DirectConn::open_sibling` already exists
  (`rb-transport/src/direct.rs:503`) — use it for per-carrier direct QUIC.
  **Unit:** hit/miss/invalidate; sibling connection carries bytes on loopback.
- **F6.7** *(Sonnet)* — **NAT lab.** Anchor: `tests/support/natlab.rs`,
  `tests/nat_traversal_test.rs`, `scripts/udp_nat_netns_test.sh` (all vendored).
  **Files:** `rb-transport/tests/support/natlab.rs`,
  `rb-transport/tests/nat_traversal.rs`, `e2e/udp_nat_netns_test.sh`.
  **Done:** full-cone / restricted-cone / port-restricted / symmetric outcomes are
  a repeatable test.
- **F6.8** *(Sonnet)* — **Per-carrier direct QUIC** on top of F6.6 + F1.
  **Done:** carriers ride the direct path with per-carrier relay fallback.

---

## Phase F7 — 🔴 QA handoff

- **F7.1 🔴** *(Haiku)* — **`docs/QA_GUIDE.md`** (new). Content: how to build the
  release binary; how to start the coordination server (plain and TLS); one
  worked example per module (postgres, mongodb, filesystem, s3) with the exact
  commands for source and destination; the `plan` dry-run; `run --config` with a
  sample YAML; how to read the progress output; the exit-code table; the full
  list of e2e scripts with their prerequisites (none / Docker / sudo) and expected
  output; what to do with a failure (which log, which file to attach).
  **Done:** a QA engineer who has never seen the repo can run every scenario from
  this file alone.
- **F7.2 🔴** *(Haiku)* — **`--help` ↔ docs parity, enforced.**
  **Files:** `USAGE.md`, `scripts/gates.sh` or a CI step.
  **Change:** a check that diffs every flag in `rust-backup --help` (all
  subcommands) against `USAGE.md` and fails on any flag documented-but-absent or
  present-but-undocumented. Cover: `plan`, the `--config` merge semantics,
  `-P key=value`, `--max-rate`, `--parallel-targets`, `--fail-fast`, `--carriers`
  (with the F1.5 per-module limits), `--secret-file`, `--tls-cert`/`--tls-key`,
  `--no-udp`, `--insecure`, `--yes`.
  **Done:** the check runs in CI and passes.
- **F7.3 🔴** *(Haiku)* — **CI completes the matrix.**
  **Files:** `.github/workflows/ci.yml`.
  **Change:** add to the existing 7 jobs: the extended `fault-matrix` (F2.3), a
  `--no-default-features` **test** run (not just build), `cargo audit`,
  `cargo deny` (licences), the F7.2 help-parity check, a 4-carrier relay smoke on
  the large fixture (F1.1), and a privileged job (netns) for
  `filesystem_netns_test.sh` + `transport_netns_test.sh` if a privileged runner is
  available — otherwise a documented manual gate in `QA_GUIDE.md`.
  **Done:** every non-privileged e2e runs in CI.
- **F7.4** *(Haiku)* — **`e2e/full_matrix.sh` is the single entry point.**
  **Change:** by default run everything that needs no privileges (relay smoke ×
  {1,4} carriers, session two-targets, fault bank non-privileged part, MinIO);
  Docker DB matrices behind `RUST_BACKUP_FULL_DB_MATRIX=1`; privileged scripts
  behind `RUST_BACKUP_PRIVILEGED=1`. Final bore-style summary with a non-zero
  exit on any failure, and a `SKIP` line (never a silent pass) for anything not
  run. **Done:** one command tells QA the whole state of the tree.
- **F7.5** *(Haiku)* — **Release metadata.** `CHANGELOG.md` gets a `0.1.0`
  section listing the F1/F2 fixes and the `PlanAck` wire addition; workspace
  version set to `0.1.0`; `docs/plans/RESUME.md` points at this file as the
  current plan and records, per phase, the command that proved it.
  **Done:** the tree states its own version and status accurately.
- **F7.6 🔴 ⟦OPUS GATE⟧** *(Opus)* — **Final read before QA.** Re-read the
  invariants against the code: I-IMMUT (audit on every exit path, incl. the new
  fault paths), I-NOTEMP (no buffering introduced by F1.1), I-ERRORS (no
  `unwrap`/`expect`/`panic!` outside tests; every `#[allow]` carries a one-line
  justification), I-MODULAR (no core change was needed to add `max_carriers`
  beyond the trait default), I-BANDWIDTH (no intra-item striping; backpressure
  intact), I-OBSERV (progress on both sides). Confirm the QA exit criteria below
  are each backed by a recorded command + result.
  **Done:** sign-off recorded in `docs/plans/RESUME.md`.

---

## 2. QA exit criteria — all must be *observed*, with the command recorded

| # | Criterion | Row |
|---|-----------|-----|
| 1 | `bash scripts/gates.sh` green, `0 ignored` | standing |
| 2 | `e2e/relay_smoke.sh` green, plain **and** TLS, carriers 1 **and** 4, large fixture | F1.1, F1.2 |
| 3 | `e2e/postgres_matrix.sh 10 12 14 16 18` green | already ✅ — re-run after F1/F5 |
| 4 | `e2e/mongodb_matrix.sh 4 5 6 7 8` green | already ✅ — re-run after F1/F5 |
| 5 | `e2e/session_two_targets.sh` green incl. `--fail-fast` | already ✅ — re-run |
| 6 | `e2e/s3_minio_test.sh` green **incl. abort injection** | F4.1 |
| 7 | `e2e/fault_matrix.sh` ≥ 20 cases, `FAIL=0` | F2.3 |
| 8 | in-process fault bank ≥ 26 cases, `0 ignored` | F2.1, F2.2 |
| 9 | `sudo -n e2e/filesystem_disk_full.sh` 5/5 | F2.4 |
| 10 | `sudo -n e2e/filesystem_netns_test.sh` green, root + non-root + mid-abort | F3.1 |
| 11 | `sudo -n e2e/transport_netns_test.sh` 9/9, including active-loss fail-safe | F3.2 |
| 12 | `sudo -n e2e/bandwidth_netem.sh` green with RSS ceiling + rate cap asserted | F3.3 |
| 13 | carriers: negotiated, ordered, per-module capped; no configuration deadlocks | F1.2, F1.5 |
| 14 | exit codes stable under message rewording | F5.3 |
| 15 | `--help` ↔ `USAGE.md` parity check green in CI | F7.2 |
| 16 | `docs/QA_GUIDE.md` walkthrough executed end to end by someone who did not write it | F7.1 |
| 17 | Opus sign-off recorded | F7.6 |

Phase **F6 is explicitly NOT a QA criterion** — it is v0.2. `docs/TRANSPORT.md`
must state plainly which bore NAT capabilities are not yet ported, so QA does not
file NAT-traversal gaps as regressions.

---

## 3. Execution order (dependency-aware)

```
F1.1 → F1.2 → F1.3 → F1.4 → F1.5 → F1.6      (data plane first: it can corrupt)
F5.3 → F2.1 → F2.2 → F2.3 → F2.4 → F2.5      (typed exit codes make the bank assertable)
F4.1 → F4.2                                   (S3, MinIO only)
F5.1 → F5.2                                   (transport hygiene)
F3.1 → F3.2 → F3.3 → F3.4                     (privileged host; F3.4 needs F1)
F7.1 → F7.2 → F7.3 → F7.4 → F7.5 → F7.6      (handoff, once behaviour is frozen)
F6.1 → … → F6.8                               (v0.2, after the QA handoff)
```

Parallelisable without conflict: **F4** (only `rb-s3` + its e2e) and **F5.1/F5.2**
(only `rb-transport`) can run alongside **F2**. **F1** touches `rb-core` and every
module destination — do not run anything else against those files while it is open.

---

## 4. Per-sub-phase report template (use verbatim)

```
Sub-phase: F1.1
Model: Sonnet 4.6
Files touched: crates/rb-core/src/channel.rs, crates/rb-core/src/session.rs
Change: <2 lines>
Tests added: <names>
Command run: cargo test --all-features -- ordered_demux    → 5 passed, 0 ignored
Gate: bash scripts/gates.sh                                 → green
e2e: RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh     → PASS=4 FAIL=0
Invariants re-checked: I-NOTEMP (no buffering added), I-BANDWIDTH (no striping)
Done: yes
```

A row without a `Command run:` line that actually ran is **not done**.
