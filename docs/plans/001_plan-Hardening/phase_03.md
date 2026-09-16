# Phase 2 — Restore validation: row counts and constraint state

> **Intent:** the destination proves, from the plan alone, that every table and populated
> materialized view holds exactly the source's row count and that every constraint exists in
> the source's validation state; the proof appears in the `RESTORE VERIFIED` block.
> **Shippable alone?** yes — additive plan metadata; a mismatch that was silent before now fails the restore.
> **Preconditions:** phase_02 DONE (matrix runner and fixtures exist; `extension_config` items exist).

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
- Items: `rb_core::plan::PlanItem { id, ordinal, kind, name, estimated_bytes, meta: serde_json::Value }` (`crates/rb-core/src/plan.rs:28`); `BackupPlan.payload: serde_json::Value` (`plan.rs:65`). Item construction for postgres: locate with `grep -n "PlanItem" crates/rb-postgres/src/*.rs`.
- Destination COPY: `crates/rb-postgres/src/dest.rs` — `apply_data:904` consumes `ChunkEvent::{Chunk, ItemEnd, End}`, one `ActiveCopy` at a time, `copy_in:931` (`tokio_postgres::CopyInSink`; `finish()` returns the number of rows copied as `u64`). Matview REFRESH statements are emitted by `ddl.rs:925-928` and executed in the post-data step of `stream_in`.
- Constraints: `CONSTRAINTS_QUERY` (`crates/rb-postgres/src/introspect.rs:457`) uses `pg_get_constraintdef(oid, true)`, `contype IN ('p','u','f','c','x')`, `conislocal` filter; `add_constraint` (`ddl.rs:451`) emits the definition verbatim after data load (non-FK 833-837, FK 838-841). `catalog_differences` (`dest.rs:396-401`) reports up to `MAX_REPORTED_DIFFERENCES`.
- Verification output: `rb_core::verification::{RestoreEvidence, VerificationSink, VerificationReport}` (`crates/rb-core/src/verification.rs:41-212`); the destination prints the `RESTORE VERIFIED` block that `e2e/lib.sh:71 rb_assert_formal_verification` greps — locate the printing site with `grep -rn "RESTORE VERIFIED" crates/`.
- Errors: `BackupError::Integrity(String)` exists (`crates/rb-core/src/error.rs:32`); its exit code is documented in `docs/usage/11-codici-uscita.md:12`.
- Test-only hook pattern: `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART` (`crates/rb-s3/src/lib.rs:1357`), documented as unsupported at `docs/usage/10-variabili-ambiente.md:117-118`.
- Fingerprint semantics: `FROM ONLY` counts a table's own rows (inheritance parents exclude children; partitioned parents yield 0).

---

## Sub-phases

### 2.1 `expected_rows` in the plan and the destination row-count check
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — design of the acceptance assertion + implementation; self-review gate (acceptance assertions).
- **Files:** `crates/rb-postgres/src/lib.rs` (or wherever `PlanItem`s are built), `crates/rb-postgres/src/introspect.rs` (`gather_views:626`), `crates/rb-postgres/src/model.rs` (`PgView`), `crates/rb-postgres/src/dest.rs` (`apply_data:904`, `stream_in` post-data step, `verify_catalog:323`), `crates/rb-postgres/src/ddl.rs` (`refresh_view:544`, loop 925-928), `docs/usage/03-postgres.md`, `docs/modules/POSTGRES.md`, `docs/usage/10-variabili-ambiente.md:117-118`, `docs/usage/11-codici-uscita.md`.
- **Change:**
  1. Source, during `analyze()`: for every table data item run `SELECT count(*) FROM ONLY <qual>` on the read-only connection and store the value as `meta["expected_rows"]` (JSON number). For `extension_config` items use `SELECT count(*) FROM ONLY <qual> WHERE <condition>` (no `WHERE` when the condition is `None`). For every materialized view with `populated == true` run `SELECT count(*) FROM <qual>` and store it in a new field `PgView.expected_rows: Option<u64>` (`#[serde(default)]`); unpopulated matviews keep `None`. Test hook: when `RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA` parses as `i64`, add it (saturating at 0) to every value written; read it once at `analyze()` start; never read on the destination.
  2. Destination `apply_data`: on `ItemEnd`, after `CopyInSink::finish()` returns `rows`, read `item.meta["expected_rows"]`; when present and `rows != expected` return `BackupError::Integrity(format!("{name}: source counted {expected} rows, destination COPY wrote {rows}"))`. When absent (older plan) skip the check and emit `tracing::warn!` once per run: `plan carries no expected_rows; row-count verification skipped`.
  3. Destination post-data: after the REFRESH statements executed, for every `PgView` with `expected_rows = Some(n)` run `SELECT count(*) FROM <qual>` on the destination and compare; mismatch is `Integrity` with the same message shape (`materialized view <qual>: ...`). `refresh_view` must not be emitted when `populated == false` (if § 1.3 already did this, keep the test).
  4. `verify_catalog` / `VerificationReport`: add totals `tables_rows_verified`, `matviews_rows_verified`, `rows_total` to the report (locate the report struct; add fields with `#[serde(default)]` if it is serialized) so § 2.3 can print them.
  5. Docs: `docs/usage/03-postgres.md` restore section (one paragraph: the destination compares row counts; mismatch is a verification failure); `docs/modules/POSTGRES.md` "Verification" section; `docs/usage/10-variabili-ambiente.md:117-118` add `RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA` as a test-only unsupported hook; `docs/usage/11-codici-uscita.md` extend the `Integrity` row meaning with "row-count mismatch".
- **Unit tests:** `expected_rows_mismatch_is_an_integrity_error` (pure helper `check_expected_rows(name: &str, expected: Option<u64>, actual: u64) -> rb_core::Result<()>`); `missing_expected_rows_skips_the_check`; `count_sql_uses_only_and_condition` (SQL text for a table, an inheritance parent, an `extension_config` item with and without condition); `matview_expected_rows_roundtrips_in_payload` (serde with and without the field); `unpopulated_matview_has_no_expected_rows_and_no_refresh`.
- **e2e tests:** T-PG-ROWS — `bash e2e/postgres_matrix.sh 16` passes and the destination log contains `rows verified:` (line added in § 2.3; until then assert the absence of `row-count verification skipped`). T-PG-ROWS-NEG — same fixture, source started with `RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA=1`: destination exits with the `Integrity` exit code from `docs/usage/11-codici-uscita.md`, its log contains `source counted`, and the destination database was removed (cleanup path `dc259a6`). Add this case to `e2e/fault_matrix.sh` (postgres section) so it runs in CI.
- **Done:** tests above green; T-PG-ROWS and T-PG-ROWS-NEG pass on 16; oracle diffs still empty (the count queries are reads only; `rb_pg_assert_readonly_log` still passes); docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 2.2).

### 2.2 Constraint state verification
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — implementation; self-review.
- **Files:** `crates/rb-postgres/src/introspect.rs` (`CONSTRAINTS_QUERY:457`), `crates/rb-postgres/src/model.rs` (constraint struct), `crates/rb-postgres/src/dest.rs` (`catalog_differences:396-401`, `verify_catalog:323`), `crates/rb-postgres/src/ddl.rs` (`add_constraint:451`), `docs/modules/POSTGRES.md`.
- **Change:** extend `CONSTRAINTS_QUERY` with `convalidated, condeferrable, condeferred` and (if § 1.3 did not) `obj_description(oid, 'pg_constraint') AS comment`. Model fields `validated: bool` (`#[serde(default = "default_true")]`), `deferrable: bool`, `initially_deferred: bool` (`#[serde(default)]`), `comment: Option<String>`. The DDL stays verbatim from `pg_get_constraintdef` (it already carries `NOT VALID`, `DEFERRABLE`, `INITIALLY DEFERRED`); add a debug assertion-free consistency check in `add_constraint`: when `validated == false` the definition text must contain `NOT VALID`, otherwise return `BackupError::PlanRejected("constraint <name>: state and definition disagree")`. `catalog_differences`: when two constraints with the same name differ, print which of `validated` / `deferrable` / `initially_deferred` / `definition` / `comment` differs, e.g. `constraint mx.t_con_06.c_con_06: validated expected false, actual true`. Post-restore: `verify_catalog` re-introspection already reads the new columns, so the comparison covers them; add report counters `constraints_total`, `constraints_validated`, `constraints_not_valid` for § 2.3. Docs: `docs/modules/POSTGRES.md` "Verification" lists constraint state among the compared facts.
- **Unit tests:** `not_valid_constraint_keeps_not_valid_in_model_and_ddl` (row CON-06 shape); `constraint_state_mismatch_is_reported_by_name`; `constraint_state_and_definition_disagreement_is_rejected`; `constraint_model_deserializes_without_new_fields` (older plan JSON: `validated` defaults to `true`).
- **e2e tests:** T-PG-CONSTR — matrix rows CON-04/05/06/12 on 16: after restore `SELECT conname, convalidated, condeferrable, condeferred FROM pg_constraint WHERE conname LIKE 'c_con_%'` is identical on both sides (already part of `rb_pg_oracle_counts` `con` lines) and the violating row of CON-06 is present on the destination; a positive-control run where the fixture validates the FK on the source (`ALTER TABLE ... VALIDATE CONSTRAINT` in a scratch database) shows `convalidated = true` on the destination.
- **Done:** tests above green; T-PG-CONSTR passes on 16 and 12; matrix `0 fail` on 10, 16, 18; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 2.3).

### 2.3 Verification report lines (I-OBSERV)
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — implementation; self-review.
- **Files:** the `RESTORE VERIFIED` printing site (`grep -rn "RESTORE VERIFIED" crates/`), `crates/rb-core/src/verification.rs` (report struct), `crates/rb-postgres/src/lib.rs` (`verify:172-188`), `e2e/lib.sh:71` (`rb_assert_formal_verification` — read-only reference, must keep matching), `docs/usage/03-postgres.md`, `docs/usage/11-codici-uscita.md`.
- **Change:** append, without altering any existing line, the lines `rows verified: <t> tables, <m> materialized views, <n> rows` and `constraints: <c> (validated <v>, not valid <nv>)` and, when present, every `deviation: ...` note (from § 1.5) to the destination's `RESTORE VERIFIED` block for the postgres module. Non-postgres modules print nothing new. Also emit the same facts through `tracing::info!` at the end of `verify`. Docs: show the updated block in `docs/usage/03-postgres.md` (realistic example) and mention the lines in `docs/usage/11-codici-uscita.md` diagnostics section.
- **Unit tests:** `verification_block_contains_row_and_constraint_lines` (format function test with fixed counters); `verification_block_existing_lines_unchanged` (golden string comparison of the pre-existing lines).
- **e2e tests:** T-PG-ROWS (from § 2.1) now asserts `rows verified:` and `constraints:` lines exist in the destination log; `rb_assert_formal_verification` still passes on every module (`bash e2e/full_matrix.sh`).
- **Done:** tests green; T-PG-ROWS passes; `bash e2e/relay_smoke.sh` and `bash e2e/session_two_targets.sh` pass (block format compatibility); docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 2.4).

### 2.4 Update README.md
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `README.md`.
- **Change:** update "Verification" (or the section describing `RESTORE VERIFIED`) with the new lines and their meaning; "Troubleshooting" gains the message `source counted N rows, destination COPY wrote M` with the remedy (the run fails closed; re-run; if it persists report with both logs). Only shipped behaviour, no internal names; preserve structure, tone, language; edit, do not rewrite.
- **Unit tests:** none (documentation).
- **e2e tests:** none — the README example output was taken from a real run.
- **Done:** a user can read and interpret the verification block from the README alone; `bash scripts/gates.sh` green; closed in `STATE.md` with the §11 docs row for phase 2 set; §1 -> 3.1.

---

## Phase gates

- **Fmt:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --locked --all-features -p rb-postgres -p rb-core` then `bash scripts/gates.sh`
- **Regression guard:** T-PG-ORACLE (16), T-PG-MATRIX, T-PG-IMMUT, T-E2E0, T-SESSION-2, T-FAULT
- **README:** verification block and troubleshooting updated

## Phase done criterion
T-PG-ROWS, T-PG-ROWS-NEG and T-PG-CONSTR pass; `bash e2e/postgres_matrix.sh 16` prints
`0 fail` and its destination log carries the `rows verified:` and `constraints:` lines;
`bash e2e/full_matrix.sh` reports no new failure. README.md reflects this phase's shipped
behavior, and `STATE.md` §11 shows this phase `DONE` with every sub-phase closed.
