# Phase 3 — PostgreSQL latent defects: consistency, connections, timeouts, memory, abort paths

> **Intent:** close the latent-defect class in the postgres module: uncontrolled connection
> count, missing lock/statement timeouts and keepalives, the sorted fingerprint that spills to
> the source's `pgsql_tmp`, unproven memory bounds on large tables, and unproven abort/race
> behaviour (destination killed mid-COPY, concurrent writer, source killed).
> **Shippable alone?** yes — each sub-phase lands with its own regression proof.
> **Preconditions:** phase_03 DONE.

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`,
   finish or revert that unit first (§6 says how far it got). Run the gate
   commands in STATE.md **§3** and check the result against what §1, §7, and §11
   claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type:
   sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to
   `claimed — nothing written yet`.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to
   `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1
   at the next unit with `Status: none`, bump the timestamp. When STATE.md §3 has
   WIP commits on, commit the closed sub-phase and put its sha in the §4 row. A
   sub-phase is not done until this is written.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is
   half-finished into §6 before stopping — plus a `wip(<N.Y>)` commit when WIP
   commits are on.

Shared facts for this phase (recon 2026-09-16):
- Connections: `connect_read_only` / `connect_admin` in `crates/rb-postgres/src/connect.rs:41-85`; startup options at 78-85 (`extra_float_digits=3`, `DateStyle=ISO,MDY`, `IntervalStyle=postgres`, `TimeZone=UTC`, `bytea_output=hex`, `lc_monetary=C`, and `default_transaction_read_only=on` for read-only). No `statement_timeout`/`lock_timeout`/keepalives anywhere. Per single-database run today: `analyze` opens 2 (boot + db, `introspect.rs:40,53`), each `fingerprint` opens 3 (`immutability.rs:44,52`), `stream_out` opens 1 per database (`source.rs:71`) — about 9.
- Fingerprint: `crates/rb-postgres/src/immutability.rs` — `fingerprint:42`, `compose:84` (prefix `rust-backup/pg-fingerprint/v1\n` at 89), `normalize:100`, `hash_catalog:113`, `table_stat:119` (`count(*)` at 134; `COPY (SELECT md5(t::text) FROM ... ORDER BY t::text COLLATE "C") TO STDOUT` at 141-155). Unit tests at 229+ (`catalog_hash_ignores_estimate_drift_but_not_structure`).
- Streaming is sequential: `source::stream_out:47` loops items, one connection reused per database; `dest::apply_data:904` one `ActiveCopy`; chunk buffer `Vec::with_capacity(CHUNK_SIZE)` at `source.rs:138`; `CHUNK_SIZE = 1 MiB` (`crates/rb-core/src/wire.rs:25`); `max_carriers() == 1` (`lib.rs:117`).
- Session audit: `crates/rb-core/src/session.rs:317-355` (`run_source_limited`, fingerprint before at 324, after-check on the error path at 343-354, `SourceMutated` at 344 and 257).
- Existing e2e patterns: `e2e/fault_matrix.sh` (T-FAULT, all modules), `e2e/bandwidth_netem.sh` (RSS cap mechanism — read how it measures and bounds memory before writing § 3.4), `e2e/postgres_matrix.sh` mid-transfer abort (T-PG-IMMUT).
- Open audit item PG9b (`docs/plans/RUST_BACKUP_AUDIT_2026-09-09.md:91`) and the matching Limits bullet in `docs/modules/POSTGRES.md:176+` ("blocking server-side sort, can spill to source pgsql_tmp").
- Dependency: `tokio-postgres 0.7` (workspace). R7 UNVERIFIED: `Config::keepalives(bool)` and `keepalives_idle(Duration)` exist since 0.5; `keepalives_interval`/`keepalives_retries` may need a newer 0.7.x — check `cargo doc -p tokio-postgres --open` or docs.rs for the locked version in `Cargo.lock` before use.

---

## Sub-phases

### 3.1 Formal review checklist and runtime-model documentation
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — review; self-review gate (hot path, concurrency, lifecycle).
- **Files:** `docs/modules/POSTGRES.md` (new section "Runtime model and guardrails"), `crates/rb-postgres/src/*.rs` (read; fixes only where the checklist finds a defect, each with a test), `STATE.md` §8/§10.
- **Change:** walk the checklist and record one line per item in the new doc section (fact, anchor, status):
  (a) every source-side connection is created by `connect_read_only` (grep `connect_admin` usages: must be destination only);
  (b) dropping an unfinished `CopyInSink` (destination error mid-item) aborts the COPY server-side and the failed-restore cleanup (`dc259a6`) removes the database — confirm by reading `apply_data` error path and `ActiveCopy` drop;
  (c) source `copy_out` stream dropped mid-item (destination gone): the source returns a phase-tagged `Transfer` error and does not hang — confirm the read loop in `stream_out` propagates channel errors;
  (d) `relax_depths` pass bound (`ddl.rs:998`) and `MAX_REPORTED_DIFFERENCES` protect against wire-received hostile plans;
  (e) no client-side collection of a whole table (`Vec`/`String`/`collect()` over rows) in `source.rs`, `dest.rs`, `immutability.rs`;
  (f) memory bound per item = chunk buffer (1 MiB) + COPY frame in flight; write the number in the doc;
  (g) every `Err` site in `source.rs`/`dest.rs` carries the right `Phase` (`Connect`/`Analyze`/`Validate`/`Transfer`/`Apply`/`Verify`/`Teardown`) — list mismatches and fix them;
  (h) backpressure: a slow destination stalls `copy_out` reads (I-BANDWIDTH) — reference `e2e/bandwidth_netem.sh`; no change unless a buffer decouples them;
  (i) `verify()` (`lib.rs:172-188`) re-streams the destination with the source code path: confirm it uses the destination's admin connection and cannot touch the source;
  (j) `default_transaction_read_only=on` can be overridden by an explicit `BEGIN READ WRITE`: confirm no source-side code issues `BEGIN`/`START TRANSACTION`/`SET TRANSACTION` (this becomes a guard test in § 4.2).
  Every defect found is fixed in this sub-phase with a named unit test (`review_<letter>_<what>`), or, when larger than a local fix, recorded in `STATE.md` §9 and assigned to the sub-phase below that owns the area.
- **Unit tests:** one per fixed defect (`review_g_phase_tags_on_copy_errors` etc.); none when the checklist finds nothing (state that in the doc).
- **e2e tests:** none (review).
- **Done:** the doc section lists all ten items with anchors and status; defects fixed or assigned; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 3.2).

### 3.2 Connection pool per database, timeouts and keepalives
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — concurrency/lifecycle work; self-review gate.
- **Files:** `crates/rb-postgres/src/connect.rs` (41-85), `crates/rb-postgres/src/lib.rs` (`PostgresSource` struct and `Source` impl), `crates/rb-postgres/src/introspect.rs` (`introspect_cluster:40-53`), `crates/rb-postgres/src/immutability.rs` (`fingerprint:42-52`), `crates/rb-postgres/src/source.rs` (`stream_out:47-71`), `crates/rb-postgres/src/dest.rs` (destination connection cache `HashMap<String, PgConnection>` at 222/903), `docs/modules/POSTGRES.md`, `docs/usage/03-postgres.md`.
- **Change:**
  1. Add `pub(crate) struct SourcePool { params, conns: tokio::sync::Mutex<HashMap<String, PgConnection>> }` in `connect.rs` with `async fn get(&self, database: Option<&str>) -> Result<PooledConn>` returning a guard that hands out `&PgConnection` for the boot connection (`None`) or a per-database connection; on `client.is_closed()` the entry is dropped and reconnected once. `PostgresSource` owns one `SourcePool`; `introspect_cluster`, `fingerprint` and `stream_out` take `&SourcePool` instead of opening connections. Sequential callers only (the session runner calls `fingerprint`, `analyze`, `stream_out`, `fingerprint` in order) — the mutex is held for the duration of one statement/COPY, never across two pool calls (document this in a comment; a nested `get` would deadlock).
  2. Read-only startup options (78-85): append `-c lock_timeout=30s -c statement_timeout=0 -c idle_in_transaction_session_timeout=0`. Destination options: append `-c statement_timeout=0` only (DDL and REFRESH may legitimately run long).
  3. Both connect paths: `config.keepalives(true).keepalives_idle(Duration::from_secs(30))`; add `keepalives_interval(Duration::from_secs(10))` and `keepalives_retries(3)` only if the locked `tokio-postgres` version exposes them (R7); record the outcome in `STATE.md` §8.
  4. Error mapping: a `lock_timeout` failure surfaces as a phase-tagged error whose message includes the server text (`canceling statement due to lock timeout`) and the relation being read.
  5. Docs: `docs/modules/POSTGRES.md` "Runtime model" (connections per run: 1 boot + 1 per database; timeouts); `docs/usage/03-postgres.md` note on `lock_timeout` behaviour (a table locked exclusively on the source fails the run within 30 s instead of hanging).
- **Unit tests:** `read_only_options_include_lock_and_statement_timeouts` (options string assertion in `connect.rs` tests, next to the existing 5); `destination_options_do_not_set_lock_timeout`; `pool_reuses_connection_per_database` (pool test with a fake connection factory, or a `#[tokio::test]` gated on `RB_PG_TEST_URL` that skips with a printed reason when the variable is unset — do not use `#[ignore]`); existing `source.rs`/`lib.rs` tokio tests still pass; regression: all 66 existing rb-postgres tests.
- **e2e tests:** T-PG-CONN — in `e2e/postgres_matrix.sh` after the run: `rb_pg_assert_connections <src> <source_user> 3` (1 boot + 1 database + 1 tolerance); T-PG-TIMEOUT — new postgres case in `e2e/fault_matrix.sh`: before the run, `docker exec -d <src> psql -U postgres -d mx -c "BEGIN; LOCK TABLE mx.t_tab_01 IN ACCESS EXCLUSIVE MODE; SELECT pg_sleep(120);"`; the source exits non-zero within 90 s, its log contains `lock timeout` and a `Phase` tag, the destination cleans up (no `mx` database), and after the lock releases a plain re-run succeeds.
- **Done:** tests green; T-PG-CONN on 10, 16, 18; T-PG-TIMEOUT on 16; T-PG-MATRIX/T-PG-IMMUT/T-PG-ORACLE still `0 fail` on 10..=18 (this refactor touches every source path — run the full range before closing); docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 3.3).

### 3.3 Fingerprint v2: commutative row commitment (PG9b)
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — hot-path change; self-review gate.
- **Files:** `crates/rb-postgres/src/immutability.rs` (`table_stat:119-155`, `compose:84-89`, tests 229+), `docs/modules/POSTGRES.md` (Limits bullet on `pgsql_tmp`), `docs/IMMUTABILITY.md` (created in § 4.1; if this sub-phase runs first, add the note to `POSTGRES.md` and move it in 4.1).
- **Change:** replace the sorted COPY with `COPY (SELECT md5(t::text) FROM ONLY <qual> t [WHERE <condition>]) TO STDOUT` (condition only for extension config tables, § 1.4). Fold the stream client-side: for each 32-hex line parse into `u128` (`u128::from_str_radix(line, 16)`; a malformed line is an `Integrity` error naming the table), `sum = sum.wrapping_add(h)`, `count += 1`. `table_stat` returns `(count: u64, commitment: u128)`; `compose` writes `<name>\t<count>\t<commitment:032x>\n` per table under the new prefix `rust-backup/pg-fingerprint/v2\n`. Keep the separate exact `count(*)` (line 134) only if the streamed `count` cannot replace it; prefer removing the extra scan and assert `count` equals the streamed row total. Remove the `pgsql_tmp` Limits bullet from `docs/modules/POSTGRES.md` and add one line to the fingerprint description ("order-independent 128-bit row commitment; two full reads per run"). Do not edit the historical audit file; note "PG9b closed by 3.3" in `STATE.md` §4.
- **Unit tests:** `row_commitment_is_order_independent` (fold `[a,b,c]` and `[c,a,b]` equal); `row_commitment_detects_single_row_change`; `row_commitment_detects_duplicate_swap` (`{A,A,C}` vs `{B,B,C}` differ — the reason XOR was rejected); `row_commitment_rejects_malformed_line`; `fingerprint_prefix_is_v2`; existing `catalog_hash_ignores_estimate_drift_but_not_structure` and the other 3 tests still pass.
- **e2e tests:** T-PG-FP2 — after `bash e2e/postgres_matrix.sh 16`, `rb_pg_assert_no_temp_files <src>` passes (containers start with `log_temp_files=0`, § 0.2) and T-PG-IMMUT (mid-transfer abort + fingerprint unchanged) still passes; T-PG-WRITER (§ 3.5) proves a single-row change is still detected end to end.
- **Done:** tests green; T-PG-FP2 on 16 and 10 (TAB-13 100k rows and TAB-16 TOAST rows included); docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 3.4).

### 3.4 Memory bound under a large table
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — harness; self-review.
- **Files:** new `e2e/postgres_large_table.sh`, `.github/workflows/e2e.yml` (new job `postgres-large`), `e2e/README.md`, `docs/QA_GUIDE.md`.
- **Change:** script (structure of `e2e/postgres_matrix.sh`, helpers from `e2e/lib.sh`): start `postgres:<M>-alpine` source and destination, create table `big(id bigint, payload text)` with `INSERT ... SELECT g, repeat(md5(g::text), 32) FROM generate_series(1, 2000000) g` (about 2 GiB), run server + source + destination, sample `VmHWM` from `/proc/<pid>/status` of both rust-backup processes every second (reuse the sampling/bounding mechanism of `e2e/bandwidth_netem.sh`; if that script uses a hard cap such as `prlimit`/`ulimit`, use the same), assert peak RSS of each process `< 256 MiB` and that the run ends with `RESTORE VERIFIED` and `rows verified: 1 tables, 0 materialized views, 2000000 rows`. Print the two peaks. CI: job `postgres-large` on version 16 only, `timeout-minutes: 40`, same triggers as `postgres`.
- **Unit tests:** none.
- **e2e tests:** T-PG-RSS — `bash e2e/postgres_large_table.sh 16` passes with both peaks below 256 MiB.
- **Done:** T-PG-RSS passes locally; actionlint clean; docs list the script; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 3.5).

### 3.5 Abort and race proofs
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — harness + any fix; self-review gate (lifecycle).
- **Files:** `e2e/fault_matrix.sh` (postgres section), `crates/rb-postgres/src/*.rs` and `crates/rb-core/src/session.rs` only if a case fails, `docs/modules/POSTGRES.md`.
- **Change:** read the postgres cases already in `e2e/fault_matrix.sh`; add the missing ones among:
  (a) destination `kill -9` while TAB-13 is streaming: source exits non-zero with a `Transfer`-tagged error, its log contains the after-audit line (find the exact text emitted by `session.rs:343-355` / `audit_source_before_ack` and grep for it) and no `SourceMutated`;
  (b) concurrent writer: while TAB-13 streams, `docker exec <src> psql -U postgres -d mx -c "INSERT INTO mx.t_tab_01 SELECT ..."` (superuser, distinct from the tool's role): source exits with the `SourceMutated` exit code from `docs/usage/11-codici-uscita.md`, destination reports failure and no `mx` database remains;
  (c) source `kill -9` mid-COPY: destination exits non-zero, cleanup removed the database (regression of `dc259a6`);
  (d) destination `--yes` missing / plan rejected: source fingerprint-after still runs (log line) and exit is non-zero.
  Each case prints `PASS`/`FAIL` with a `T-PG-ABORT-<letter>` label; (b) is `T-PG-WRITER`. Any product fix gets a unit test where the logic is unit-testable and otherwise the e2e case is the regression.
- **Unit tests:** as needed per fix.
- **e2e tests:** T-PG-ABORT (a, c, d) and T-PG-WRITER (b) pass on 16 inside `bash e2e/fault_matrix.sh`.
- **Done:** all four cases PASS; `bash e2e/fault_matrix.sh` overall passes (other modules unaffected); docs "Failure behaviour" paragraph in `docs/modules/POSTGRES.md` lists the four outcomes; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 3.6).

### 3.6 Update README.md
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `README.md`.
- **Change:** update "PostgreSQL" notes: the source connection now fails within about 30 seconds when a table is exclusively locked (message users will see), connections used per run, keepalives; "Limitations": remove the statement that the fingerprint can spill temporary files on the source (if present) and state the two full read passes; "Troubleshooting": `canceling statement due to lock timeout` with remedy. Only shipped behaviour, no internal names; preserve structure, tone, language; edit, do not rewrite.
- **Unit tests:** none (documentation).
- **e2e tests:** none — examples taken from real runs.
- **Done:** README accurate for lock-timeout behaviour and fingerprint cost; `bash scripts/gates.sh` green; closed in `STATE.md` with the §11 docs row for phase 3 set; §1 -> 4.1.

---

## Phase gates

- **Fmt:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --locked --all-features -p rb-postgres -p rb-core` then `bash scripts/gates.sh`
- **Regression guard:** T-PG-ORACLE on 10..=18 after § 3.2 and § 3.3; T-PG-IMMUT; T-FAULT; T-E2E0
- **README:** lock timeout, connections, fingerprint cost, troubleshooting

## Phase done criterion
T-PG-CONN, T-PG-TIMEOUT, T-PG-FP2, T-PG-RSS, T-PG-ABORT, T-PG-WRITER pass; `bash
e2e/postgres_matrix.sh M` prints `0 fail` for M in 10..=18 with `<= 3` source connections and
zero temporary-file log lines; `docs/modules/POSTGRES.md` carries the "Runtime model and
guardrails" section. README.md reflects this phase's shipped behavior, and `STATE.md` §11
shows this phase `DONE` with every sub-phase closed.
