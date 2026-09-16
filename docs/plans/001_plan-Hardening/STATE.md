# Hardening — Implementation State

> **READ THIS FILE FIRST at the start of every session, before any other plan
> file. OPEN a unit in §1 before touching code; CLOSE it after the gates pass.**
> **Last updated:** 2026-09-16 (phase 4 committed `4d1b5e7`) | **By:** `agent:opus` | **Session:** 2026-09-16

## 0. Protocol

This is the only execution-state file — position, progress, ledger, and blockers
all live here. A **unit of work** is one sub-phase, one `task`, one `bug`, one
`verify` audit, or one correction from a verify report.

**Resume (cold start):**
1. Read this file end to end.
2. Read §1 `Status`:
   - `OPEN` — a unit was claimed and may be half-written. Read §6, then finish or
     revert it before starting anything new. If §3 has WIP commits on and `HEAD`
     is a `wip:` commit, that commit is the in-flight work: its diff is what got
     written, §6 says why it stopped. Finish and `amend` into the close commit,
     or revert it.
   - `none` — nothing in flight. Open the unit named in §1 `Next action:`.
3. Run the gate commands in §3 and compare the result with what §1, §7, and §11
   claim. The repo is the truth; correct this file if it drifted.
4. Open only the file §1 points at: the phase file at the named sub-phase, or the
   verify report for a correction. Read `overview.md` only if §2 is insufficient.

**Open a unit — before touching code, mandatory:** set §1 `Type`, `ID`,
`Status: OPEN`, `Intent`, `Next action:`, `Assigned`; set §6 to `claimed —
nothing written yet`; bump the header timestamp. Only then edit anything.

**Close a unit — after its gates are green, mandatory:** append a §4 ledger row;
reset §6 to `none — tree consistent`; update §5, §7, §8, §9, §10 and the §11
board; set §1 to the next unit with `Status: none`; bump the timestamp. When §3
has WIP commits on, commit the closed unit — code, tests, this file, docs, ledger
together — staging only the files in §5 plus the plan files, never `git add -A`,
and record the sha in the §4 row. A unit is not `DONE` until this is written.

**Interrupted mid-unit:** leave §1 `OPEN` and write into §6 exactly what is
half-finished — files written, edits still pending, temporary code to remove.
`OPEN` with an empty §6 is an execution bug. With WIP commits on, also commit
that state as `wip(<id>): <what remains>`.

## 1. Current unit

- **Type:** `sub-phase`
- **ID:** 5.1
- **Status:** `none`
- **Intent:** Filesystem special files: FIFOs and devices round-trip, sockets refused
- **Phase:** phase_06.md — Phase 5
- **Next action:** read phase_06.md § 5.1 and open it
- **Assigned:** `agent:opus`
- **Repo state:** phases 0-4 committed on dev; nothing in flight

## 2. Feature context (self-contained recap)

Harden `rust-backup`'s PostgreSQL and filesystem modules and the source-immutability
guardrails of all four modules (postgres, mongodb, filesystem, s3). Seven phases: 0 harness
foundation (matrix docs, fixture layout, oracle helpers in `e2e/lib.sh`, `scripts/env_inventory.sh`,
`scripts/source_readonly_lint.sh`); 1 PostgreSQL fidelity matrix on 10..=18 plus PostGIS with a
`pg_dump`/psql external oracle, extension config tables, `--extension-version`; 2 restore
validation (`expected_rows` in the plan, constraint state, `RESTORE VERIFIED` lines); 3 latent
defects (connection pool, `lock_timeout`/keepalives, commutative fingerprint v2, RSS proof,
abort/race proofs); 4 immutability guardrails (typed read-only clients, allowlist, lint in gates,
`--allow-atime-updates`, least-privilege e2e roles, panic-safe after-audit, `docs/IMMUTABILITY.md`);
5 filesystem (FIFO/device/socket handling, permissions/ownership/link matrix as root, TOCTOU
hardening); 6 docs audit with `scripts/docs_parity.sh` in the gates. Every module crate keeps
`#![forbid(unsafe_code)]` and denies `unwrap`/`expect`/`panic` outside tests; production code
returns `rb_core::Result` with a `Phase`.

**Reference scenario:** `bash e2e/postgres_matrix.sh M` prints `0 fail` for M in 10..=18 and
for `10:18 12:16 14:17 16:18`, also under `RB_PG_IMAGE_REPO=postgis/postgis`; the source log
shows only read statements from `rb_ro`, at most 3 connections, no temporary files, fingerprint
unchanged; `sudo bash e2e/filesystem_matrix.sh` prints `MATRIX FS: 30 pass, 0 fail`;
`bash scripts/gates.sh` (with readonly lint and docs parity) is green.
**Hard constraints:** I-IMMUT (source never altered, audited on every exit path), I-NOTEMP,
I-ERRORS, I-MODULAR (no `rb_core::module` trait change), I-BANDWIDTH (postgres `max_carriers()==1`),
I-OBSERV, plan self-contained (no destination-to-source round-trip), no new crate, additive
serde-defaulted payload changes, unknown kinds fail closed, docs languages preserved
(Italian `docs/usage/`, English elsewhere), no emojis or informal text in any deliverable.
**Key decisions in force:** D3 matrix IDs `M-PG-<KIND>-<nn>`/`M-FS-<nn>`, fixtures
`e2e/fixtures/postgres/NN_<kind>[.ge<major>].sql`; D12 extension config tables captured with
their condition; D13 `--extension-version source|default` (env `RUST_BACKUP_EXTENSION_VERSION`);
D15 `pg_dump --schema-only` inside the destination container as oracle; D16 `expected_rows` via
`SELECT count(*) FROM ONLY`; D17 mismatch = `BackupError::Integrity`; D18 pooled connections,
`lock_timeout=30s`, `statement_timeout=0`, keepalives; D19 fingerprint v2 = count + wrapping
`u128` sum of per-row md5; D20 typed read-only clients + allowlist + lint + least-privilege e2e
+ panic-safe audit; D21 atime guard with `--allow-atime-updates` (env
`RUST_BACKUP_ALLOW_ATIME_UPDATES`); D23 FIFO/device restored, sockets refused; D24 content
fingerprint kept; D25 sparse/xattr/ACL out of scope; D27 CI timeouts up to 80 min.
Recon inventory of clap env vars (40) for phase 6: `RUST_BACKUP_CONFIG PARALLEL_TARGETS FAIL_FAST
BIND_ADDR CONTROL_PORT SECRET SECRET_FILE TLS_CERT TLS_KEY MAX_CONNS UDP HOST PORT USER PASSWORD
DATABASE SSLMODE URI AUTH_DB ROOT BUCKET ENDPOINT REGION PREFIX ACCESS_KEY SECRET_KEY ADMIN
OVERWRITE PATH_STYLE FOLLOW_SYMLINKS NO_PRESERVE_OWNERSHIP PRESERVE_XATTR TO CHANNEL CARRIERS
INSECURE MAX_RATE YES` (all prefixed `RUST_BACKUP_`), direct reads `RUST_BACKUP_PLAN_TIMEOUT`,
`RUST_BACKUP_VERIFY_TIMEOUT`, `RUST_BACKUP_STUN_SERVERS`, `RUST_BACKUP_STUN_SERVER`,
`BORE_PROXY_BUFFER_SIZE`, `RUST_LOG`, test-only `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART`.

## 3. Environment and commands

The authoritative gate commands. Identical to the phase gates and to
`overview.md`'s verification summary — no drift.

- **Repo root:** `/mnt/fabio/dati/Git/Github-manprint-public/rust-backup`
- **Build:** `cargo build --locked --all-features` and `cargo build --locked --no-default-features` · **Fmt:** `cargo fmt --all --check` · **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings`
- **Unit tests:** `cargo test --locked --all-features` · **Full gate:** `bash scripts/gates.sh` (fmt, clippy, both builds, tests, `scripts/crate_invariants.sh`, `scripts/help_parity.sh`; phase 4 adds `scripts/source_readonly_lint.sh`, phase 6 adds `scripts/docs_parity.sh`)
- **E2E:** `bash e2e/postgres_matrix.sh <M>|<S:D>` (add `RB_PG_IMAGE_REPO=postgis/postgis` for PostGIS) · `bash e2e/postgres_introspect.sh 16` · `bash e2e/fault_matrix.sh` · `bash e2e/postgres_large_table.sh 16` (phase 3) · `sudo bash e2e/filesystem_matrix.sh` (phase 5) · `sudo bash e2e/filesystem_netns_test.sh` · `bash e2e/mongodb_matrix.sh <M>` · `bash e2e/s3_minio_test.sh` · `bash e2e/relay_smoke.sh` · `bash e2e/session_two_targets.sh` · `bash e2e/full_matrix.sh` (all, with skip accounting)
- **Setup / caveats:** Docker required for postgres/mongodb/minio scripts; root (`sudo -n`) for filesystem and transport privileged scripts; e2e run serially; release binary built by `rb_build_release` (`e2e/lib.sh:9`); ShellCheck via `docker run --rm koalaman/shellcheck:v0.11.0`, actionlint via `rhysd/actionlint:1.7.12` (same pins as CI); this plan never pushes — CI results are recorded from runs the user triggers.
- **WIP commits:** `on` — one local commit per completed phase on branch `dev` (user instruction, session 1); never pushed. Sub-phase ledger rows carry the phase commit sha, backfilled when the phase closes.

## 4. Work ledger (append-only, one row per closed unit)

Every unit type shares this ledger, in the order it closed. `ID` is the sub-phase
(`N.Y`), the ad-hoc ledger ID (`T-A<NNN>` / `B-A<NNN>`), the report (`V<NNN>`), or
the correction (`V<NNN>-C<n>`). `Commit` is the sha when one exists, `uncommitted`
otherwise — with WIP commits off this skill does not commit on its own, so
`uncommitted` is the honest value.

| # | Type | ID | Agent | What changed | Files | Gates | Commit |
|---|------|----|-------|--------------|-------|-------|--------|
| 1 | sub-phase | 0.1 | `agent:opus` | matrix catalogues created: 106 `M-PG-*` rows and 32 `M-FS-*` rows, with fixture file, expected outcome and oracle per row; link lines added | docs/testing/POSTGRES_MATRIX.md, docs/testing/FILESYSTEM_MATRIX.md, docs/QA_GUIDE.md, e2e/README.md | gates.sh PASS | `86195ff` |
| 2 | sub-phase | 0.2 | `agent:opus` | `RB_PG_LOG_ARGS` + 8 `rb_pg_*` helpers appended to `e2e/lib.sh`; fixture directory with naming rule and oracle ignore list; helper table in `e2e/README.md`; `pg_dump` flags excluded from the ghost-flag scan | e2e/lib.sh, e2e/fixtures/postgres/{README.md,oracle_ignore.txt}, e2e/README.md, scripts/help_parity.sh | gates.sh PASS; bash -n PASS; ShellCheck v0.11.0 PASS; relay_smoke.sh PASS (5/0); helpers smoke-tested against postgres:16-alpine | `86195ff` |
| 3 | sub-phase | 0.3 | `agent:opus` | `scripts/env_inventory.sh`: TSV of every (command, flag, env, default, help) pair from 14 clap help surfaces plus direct `env::var` reads, split `code` vs `test`; `--check-count` guard | scripts/env_inventory.sh | gates.sh PASS; ShellCheck PASS; bash -n PASS; T-HARN-INV PASS (48 distinct RUST_BACKUP_* >= 40) | `86195ff` |
| 4 | sub-phase | 0.4 | `agent:opus` | `scripts/source_readonly_lint.sh`: per-module forbidden call-shape table over source-side files, `--selftest`; not yet wired into gates.sh (§ 4.6) | scripts/source_readonly_lint.sh | gates.sh PASS; ShellCheck PASS; T-HARN-LINT PASS (9 files clean, selftest detects the injected write) | `86195ff` |
| 5 | sub-phase | 0.5 | `agent:opus` | README verified, no change — phase 0 shipped no user-visible behaviour; the new harness files are reachable through the QA guide and e2e/README links README already carries | README.md (unchanged) | gates.sh PASS | `86195ff` |
| 6 | sub-phase | 1.1 | `agent:opus` | 16 fixture files + 15 refusal files + the rbtest installer: one object group per matrix row, in schema `mx` (plus `"Mixed Schema"` and `ext`); matrix `Fixture file` column reconciled | e2e/fixtures/postgres/*.sql, refusals/*.sql, postgis/80_postgis.sql, 71_rbtest_extension.sh, README.md, docs/testing/POSTGRES_MATRIX.md | gates.sh PASS; ShellCheck PASS; T-PG-FIX PASS on 10, 12, 16, 18 (every file loads with ON_ERROR_STOP, gated files skipped, all 15 refusal files load alone) | `9b570b9` |
| 7 | sub-phase | 1.2 | `agent:opus` | runner v2: fixtures instead of the inline seed, pg_dump + counts oracles, per-row PASS/FAIL/SKIP, refusal loop, registration-aware fail-fast, MATRIX summary; first run on 16 refused at Analyze (3 FAIL rows recorded in §9) | e2e/postgres_matrix.sh, e2e/lib.sh, e2e/fixtures/postgres/*.sql | ShellCheck --severity=warning PASS; resource_hygiene_check PASS; run on 16 reaches per-row evaluation only after § 1.3 (T-PG-ORACLE deferred there) | `9b570b9` |
| 8 | sub-phase | 1.3 | `agent:opus` | view/matview reloptions + `relispopulated` captured and restored; constraint and index comments emitted; PostgreSQL 18 named-NOT-NULL probe compares the exact default name; 6 new unit tests (77 total in rb-postgres) | crates/rb-postgres/src/{model,introspect,ddl}.rs, docs/modules/POSTGRES.md, e2e/postgres_matrix.sh | gates.sh PASS (77 rb-postgres tests); matrix 16/10/12/18: only M-PG-EXT-07 and its two derived rows fail (§ 1.4), NOTEMP skipped (§ 3.3) | `9b570b9` |
| 9 | sub-phase | 1.4 | `agent:opus` | extension configuration tables captured, streamed with their condition, restored over a cleared scope; unknown item kinds refused | model.rs, introspect.rs, source.rs, dest.rs, immutability.rs, POSTGRES.md, 03-postgres.md, postgres_matrix.sh | gates.sh PASS; postgres_matrix.sh 16 => 109 pass, 0 fail, 10 skip | `9b570b9` |
| 10 | sub-phase | 1.5 | `agent:opus` | extension version preflight, `--extension-version source\|default`, deviation reported on RESTORE VERIFIED | lib.rs, ddl.rs, dest.rs, main.rs, 03-postgres.md, 10-variabili-ambiente.md, 07-sessioni-yaml.md, POSTGRES.md, postgres_matrix.sh, env_inventory.sh | gates.sh PASS; postgres_matrix.sh 16 => 110 pass, 0 fail, 9 skip (M-PG-EXT-08 green) | `9b570b9` |
| 11 | sub-phase | 1.6 | `agent:opus` | PostGIS matrix green on every major and on the cross pair 12:16; extension-owned objects no longer refuse a cluster | introspect.rs, postgres_matrix.sh, lib.sh, POSTGRES_MATRIX.md, e2e/README.md | gates.sh PASS; postgis 10-18 => 901 pass, 0 fail; postgis 16 => 115 pass, 0 fail; postgis 12:16 => 114 pass, 0 fail | `9b570b9` |
| 12 | sub-phase | 1.7 | `agent:opus` | CI: PostGIS jobs added, PostgreSQL e2e timeouts raised to 80 minutes | .github/workflows/e2e.yml, docs/QA_GUIDE.md | actionlint 1.7.12 clean; gates.sh PASS | `9b570b9` |
| 13 | sub-phase | 1.8 | `agent:opus` | README documents extension configuration tables, the extension-version policy and the PostGIS e2e invocation | README.md | gates.sh PASS; help_parity PASS | `9b570b9` |
| 14 | sub-phase | 2.1 | `agent:opus` | source counts rows per item and per populated matview; the destination refuses a restore whose COPY or REFRESH produced a different number | model.rs, introspect.rs, source.rs, dest.rs, lib.rs, verification.rs, postgres_matrix.sh, fault_matrix.sh, POSTGRES.md, 03-postgres.md, 10-variabili-ambiente.md, 11-codici-uscita.md | gates.sh PASS; postgres_matrix.sh 16 => 111 pass, 0 fail (ROWS-VERIFIED green); fault_matrix.sh postgres => CASES=4 PASS=11 FAIL=0 | `6e2d810` |
| 15 | sub-phase | 2.2 | `agent:opus` | constraint validity, deferrability and comment captured, emitted verbatim and compared; a state/definition disagreement is refused | model.rs, introspect.rs, ddl.rs, dest.rs, verification.rs, postgres_matrix.sh, POSTGRES.md | gates.sh PASS; postgres_matrix.sh 16 12 => 224 pass, 0 fail (CONSTR-STATE, CONSTR-NOT-VALID-ROW green) | `6e2d810` |
| 16 | sub-phase | 2.3 | `agent:opus` | the destination prints what it proved under RESTORE VERIFIED: rows verified, constraints, one line per declared deviation; same facts as structured fields | verification.rs, session.rs, lib.rs, dest.rs, postgres_matrix.sh, 03-postgres.md, 11-codici-uscita.md, POSTGRES.md | gates.sh PASS; postgres_matrix.sh 16 => 115 pass, 0 fail (ROWS-LINE, CONSTR-LINE, EXT-08 green); relay_smoke PASS=5 FAIL=0; session_two_targets PASS | `6e2d810` |
| 17 | sub-phase | 2.4 | `agent:opus` | README documents the verification lines and the row-count refusal; CONSTR-STATE stops comparing partition FK clone names across majors | README.md, postgres_matrix.sh | gates.sh PASS; full_matrix.sh (RUST_BACKUP_FULL_DB_MATRIX=1) 12 pass 1 fail => after the fix postgres_matrix.sh 16:18 12:16 => 228/0 and 10:18 14:17 => 217/0 | `6e2d810` |
| 18 | sub-phase | 3.1 | `agent:opus` | runtime-model review a-j written into POSTGRES.md with anchors; two phase tags fixed (matview count and item framing were Verify inside apply) | dest.rs, POSTGRES.md | gates.sh PASS (98 rb-postgres unit tests, 2 new) | `75ea492` |
| 19 | sub-phase | 3.2 | `agent:opus` | one pooled read-only connection per database for every source phase; lock_timeout/idle timeouts and TCP keepalives; T-PG-CONN and T-PG-TIMEOUT | connect.rs, introspect.rs, immutability.rs, source.rs, lib.rs, dest.rs, lib.sh, postgres_matrix.sh, fault_matrix.sh, POSTGRES.md, 03-postgres.md | gates.sh PASS (101 rb-postgres tests); matrix 10..18 => 1025 pass, 0 fail; fault_matrix postgres => CASES=5 PASS=15 FAIL=0 | `75ea492` |
| 20 | sub-phase | 3.3 | `agent:opus` | fingerprint v2: order-independent 128-bit row commitment folded client-side, no server sort and no second COUNT(*) — PG9b closed | immutability.rs, POSTGRES.md, postgres_matrix.sh | gates.sh PASS (108 rb-postgres tests, 7 new); matrix 10 16 => 221 pass, 0 fail with IMMUT-ABORT/FULL/READONLY-LOG green; source temp files per case 12 => 6 (the rest is the data stream's sort) | `75ea492` |
| 21 | sub-phase | 3.4 | `agent:opus` | e2e/postgres_large_table.sh: one 2 GiB table end to end with both peers' peak RSS sampled and capped at 256 MiB; CI job postgres-large | postgres_large_table.sh, e2e.yml, e2e/README.md, QA_GUIDE.md | gates.sh PASS; ShellCheck PASS; actionlint 1.7.12 clean; T-PG-RSS on 16 => 5 pass, 0 fail (source 14056 KiB, destination 22592 KiB) | `75ea492` |
| 22 | sub-phase | 3.5 | `agent:opus` | abort and race proofs: destination kill, concurrent writer, source kill, refused plan — and the product fix they found (a run that fails after apply now removes the databases it created) | module.rs, session.rs, dest.rs, lib.rs, session_test.rs, fault_matrix.sh, POSTGRES.md | gates.sh PASS (18 rb-core session tests, 2 new); fault_matrix.sh postgres => CASES=8 PASS=21 FAIL=0; fault_matrix.sh (all) => CASES=28 PASS=69 FAIL=0 | `75ea492` |
| 23 | sub-phase | 3.6 | `agent:opus` | README: lock-timeout message and remedy, connections and keepalives, the two fingerprint read passes and exit 6 | README.md | gates.sh PASS; closing matrix 10..18 => 1025 pass, 0 fail; relay_smoke PASS; session_two_targets PASS | `75ea492` |
| 32 | sub-phase | 4.9 | `agent:opus` | README gains a "Source safety" section: what makes the source read-only and what the audit does on every exit path, the least-privilege account per backend with a pointer to the full recipes, the writable-role warning, and the access-time rule with the refusal message and both remedies | README.md | gates.sh PASS (help/docs parity included); phase-4 regression guard: postgres 10/16/18, mongodb 4, MinIO all green | `4d1b5e7` |
| 31 | sub-phase | 4.8 | `agent:opus` | the source pipeline runs inside `catch_unwind`, so a panic becomes an ordinary error and takes the same failure path as any other: the source is fingerprinted again and the run reports `SourceMutated` on drift, otherwise `source pipeline panicked: <message>` | crates/rb-core/src/session.rs, crates/rb-core/Cargo.toml, crates/rb-core/tests/session_test.rs, docs/IMMUTABILITY.md, docs/modules/README.md | gates.sh PASS (20 session_test tests, 2 new); T-E2E0 PASS=5 FAIL=0; T-SESSION-2 PASS | `4d1b5e7` |
| 30 | sub-phase | 4.7 | `agent:opus` | all three server-side least-privilege proofs: PostgreSQL runs a whole transfer as `rb_ro` (both grant recipes) and the server log is asserted for that role; MongoDB runs one as a `read`-only user against a mongod that logs every command; MinIO runs one under a 5-action read policy with `mc admin trace` as the record and a refused write as the counter-proof | e2e/lib.sh, e2e/postgres_matrix.sh, e2e/mongodb_matrix.sh, e2e/s3_minio_test.sh, e2e/README.md, docs/IMMUTABILITY.md, scripts/help_parity.sh | gates.sh PASS; ShellCheck v0.11.0 PASS on the four scripts; T-IMMUT-PG-LP PASS on 10, 13, 14, 16, 18; T-IMMUT-MONGO-LP PASS on 4, 6, 7 (110 commands checked, all reads); T-IMMUT-S3-LP PASS (25 S3 APIs checked, all reads; the same credential is refused a write) | `4d1b5e7` |
| 29 | sub-phase | 4.6 | `agent:opus` | `scripts/gates.sh` now runs the read-only lint and its `--selftest`, so I-IMMUT layer 3 is a gate rather than a script someone remembers; gate lists in CLAUDE.md, the QA guide and IMMUTABILITY.md say so | scripts/gates.sh, scripts/help_parity.sh, CLAUDE.md, docs/QA_GUIDE.md, docs/IMMUTABILITY.md | gates.sh PASS with the lint inside (12 files, selftest detects the injected write); ShellCheck v0.11.0 PASS on the three scripts | `4d1b5e7` |
| 28 | sub-phase | 4.5 | `agent:opus` | reading a file the process does not own moves its atime, so `O_NOATIME` + `EPERM` is now a refusal during the first fingerprint instead of a silent fallback; `--allow-atime-updates` (env + YAML) accepts it and warns once | rb-filesystem/src/{source,lib,immutability}.rs, rb-filesystem/Cargo.toml, rust-backup/src/main.rs, filesystem_netns_test.sh, 05-filesystem.md, 10-variabili-ambiente.md, 07-sessioni-yaml.md, FILESYSTEM.md, IMMUTABILITY.md | gates.sh PASS (17 rb-filesystem tests, 4 new; 23 rust-backup tests, 1 new); help_parity PASS; env_inventory 51 names; live proof on /etc/skel both ways; relay_smoke PASS=5 FAIL=0 | `4d1b5e7` |
| 27 | sub-phase | 4.4 | `agent:opus` | the whole S3 read side moved into `crates/rb-s3/src/source.rs` (S3Source, its `Source` impl, `list_objects`, `validate_source_fidelity`) and holds a `ReadOnlyS3` newtype — eight read builders, no write method, no accessor for the SDK client; the lint now checks that file and fails if it disappears | rb-s3/src/{lib,source}.rs, scripts/source_readonly_lint.sh, IMMUTABILITY.md, S3.md | gates.sh PASS (13 rb-s3 unit tests, unchanged); source_readonly_lint PASS (12 files) + selftest PASS; e2e/s3_minio_test.sh PASS (metadata/policy/key-set read-back, overwrite cleanup, multipart abort cleanup, source immutability) | `4d1b5e7` |
| 26 | sub-phase | 4.3 | `agent:opus` | source-side MongoDB code now holds `ReadOnlyClient`/`ReadOnlyDatabase` (named-collection reads only, no `Collection` or raw `Database` ever handed out); `run_command` passes an 11-command read allowlist; the source client pins `readPreference=primary` and `readConcern=local`; the read-only lint now also covers both `connect.rs` files | rb-mongodb/{connect,introspect,source,immutability}.rs, rb-mongodb/Cargo.toml, scripts/source_readonly_lint.sh, IMMUTABILITY.md, MONGODB.md | gates.sh PASS (36 rb-mongodb unit tests, 5 new); source_readonly_lint PASS (11 files) + selftest PASS; mongodb_matrix 4 => PASS=5 FAIL=0, 7 and 4:7 => PASS=10 FAIL=0, 8 SKIP (host kernel, SERVER-121912) | `4d1b5e7` |
| 25 | sub-phase | 4.2 | `agent:opus` | source-side PostgreSQL code now holds `ReadOnlyClient` (only `query`/`query_one`/`query_opt`/`copy_out`, no accessor to the raw client); every statement passes `guard_read_only` (literal/comment stripping, single-statement, head allowlist, deny words and deny functions); the source role is probed once and a writable role is warned about, never refused | connect.rs, introspect.rs, immutability.rs, Cargo.toml, IMMUTABILITY.md, POSTGRES.md, 03-postgres.md, README.md | gates.sh PASS (116 rb-postgres unit tests, 8 new); source_readonly_lint PASS + selftest PASS; matrix 10 16 18 => 339 pass, 0 fail, 35 skip; role probe warns as `postgres`, silent as `rb_ro` | `4d1b5e7` |
| 24 | sub-phase | 4.1 | `agent:opus` | docs/IMMUTABILITY.md written: the invariant and what 'altered' means, 4 vector tables, the fingerprint contract per module, least-privilege recipes, the 4-layer guard architecture; linked from the module docs, README and QA guide | IMMUTABILITY.md, POSTGRES.md, MONGODB.md, S3.md, FILESYSTEM.md, README.md, QA_GUIDE.md | gates.sh PASS (ghost-flag scan included) | `4d1b5e7` |

## 5. Files touched

| Path | What was done | Unit |
|------|---------------|------|
| `docs/testing/POSTGRES_MATRIX.md` | new: 106 rows (TAB 18, SEQ 7, VIEW 9, MV 7, IDX 18, CON 18, EXT 8, GIS 6, REF 15) with the oracle legend | 0.1 |
| `docs/testing/FILESYSTEM_MATRIX.md` | new: 32 rows with the oracle legend | 0.1 |
| `README.md` | new "Source safety" section; the filesystem section points at it for `--allow-atime-updates` | 4.9 |
| `crates/rb-core/src/session.rs` | `run_source_limited` wraps `source_run` in `catch_unwind`; `panicked()` renders the payload | 4.8 |
| `crates/rb-core/Cargo.toml` | `futures-util` dependency (`FutureExt::catch_unwind`) | 4.8 |
| `crates/rb-core/tests/session_test.rs` | `PanickingSource` + the two T-IMMUT-PANIC tests | 4.8 |
| `docs/IMMUTABILITY.md` | the fingerprint contract's panic row names the mechanism, the exit code and the two tests | 4.8 |
| `docs/modules/README.md` | step 3 states that `fingerprint` is called after a panicked run too | 4.8 |
| `e2e/lib.sh` | `rb_mongo_assert_readonly_log`, `rb_minio_readonly_policy`, `rb_minio_assert_readonly_trace` | 4.7 |
| `e2e/postgres_matrix.sh` | `SOURCE_USER`/`SOURCE_PASSWORD`, `create_readonly_role`, step 12b: a whole transfer as `rb_ro` with log and connection assertions | 4.7 |
| `e2e/mongodb_matrix.sh` | credential-aware `mongo_eval`/`run_transfer`, `create_readonly_user`, step 6: the LP source container (`--auth --profile 0 --slowms 0`) | 4.7 |
| `e2e/s3_minio_test.sh` | `mc_sh`, the least-privilege user and policy, the traced LP transfer and the refused-write probe | 4.7 |
| `e2e/README.md` | the three LP test IDs in the script table; the helper table renamed and extended with the MongoDB and MinIO helpers | 4.7 |
| `docs/IMMUTABILITY.md` | the final MongoDB and S3 least-privilege recipes; guard layer 4 names the three log assertions; the "evidence lands in § 4.7" markers removed | 4.7 |
| `scripts/help_parity.sh` | `--profile`, `--slowms`, `--json` added to the foreign-flag exclusion list | 4.7 |
| `scripts/gates.sh` | runs the read-only lint and its `--selftest` before the help parity | 4.6 |
| `scripts/help_parity.sh` | `--all`, `--all-targets`, `--selftest` added to the foreign-flag exclusion list | 4.6 |
| `CLAUDE.md` | the gate line lists every script `gates.sh` runs | 4.6 |
| `docs/QA_GUIDE.md` | a "Build" paragraph naming the one gate command and everything it chains | 4.6 |
| `crates/rb-filesystem/src/source.rs` | `NoAtimeReader::open`/`open_with` refuse `EPERM` without the opt-in, warn once with it; `read_noatime` takes the flag and the caller's phase | 4.5 |
| `crates/rb-filesystem/src/lib.rs` | `FilesystemParams.allow_atime_updates`; 4 new unit tests | 4.5 |
| `crates/rb-filesystem/src/immutability.rs` | the fingerprint reads with the flag, tagged `Analyze` | 4.5 |
| `crates/rb-filesystem/Cargo.toml` | `tracing` dependency (the one-shot warning) | 4.5 |
| `crates/rust-backup/src/main.rs` | `--allow-atime-updates` / `RUST_BACKUP_ALLOW_ATIME_UPDATES`, merged as `allow_atime_updates`; 1 new test | 4.5 |
| `e2e/filesystem_netns_test.sh` | case (d): T-FS-ATIME, both polarities under root | 4.5 |
| `docs/usage/05-filesystem.md` | flag row plus the "Access time e `O_NOATIME`" section with both messages | 4.5 |
| `docs/usage/10-variabili-ambiente.md` | `RUST_BACKUP_ALLOW_ATIME_UPDATES` row | 4.5 |
| `docs/usage/07-sessioni-yaml.md` | `allow_atime_updates` key | 4.5 |
| `docs/modules/FILESYSTEM.md` | the atime rule, the refusal message, the guarantees-table row, the reworded limit | 4.5 |
| `crates/rb-s3/src/source.rs` | new: the whole S3 read side (`ReadOnlyS3`, `S3Source`, its `Source` impl, `list_objects`, `validate_source_fidelity`) | 4.4 |
| `crates/rb-s3/src/lib.rs` | `mod source;`, `open_source` builds `S3Source::new`, six helpers made `pub(crate)`, the moved blocks removed | 4.4 |
| `docs/modules/S3.md` | "Immutability" names the newtype, the read operations and the one-file read side | 4.4 |
| `crates/rb-mongodb/src/connect.rs` | `ReadOnlyClient`/`ReadOnlyDatabase`/`ReadOnlyConnection`, `guard_read_command` + the 11-command allowlist, `pin_read_settings`, `sort_by_id`; 5 new unit tests | 4.3 |
| `crates/rb-mongodb/src/introspect.rs` | introspection runs on `ReadOnlyConnection`/`ReadOnlyDatabase`; the spec listings no longer collect a cursor at the call site | 4.3 |
| `crates/rb-mongodb/src/source.rs` | `stream_out` reads through `ReadOnlyConnection` with the shared `_id` sort | 4.3 |
| `crates/rb-mongodb/src/immutability.rs` | `coll_stat` counts and reads through `ReadOnlyDatabase` | 4.3 |
| `crates/rb-mongodb/Cargo.toml` | `anyhow` dependency (the guard refusal) | 4.3 |
| `scripts/source_readonly_lint.sh` | the postgres and mongodb file lists now include `connect.rs`, where the wrappers live | 4.3 |
| `docs/modules/MONGODB.md` | "Immutability" rewritten: typed client, command allowlist, pinned read preference and concern | 4.3 |
| `crates/rb-postgres/src/connect.rs` | `ReadOnlyClient` (query/query_one/query_opt/copy_out only), `ReadOnlyConnection`, `guard_read_only` + its stripper and allowlist, the role probe, `SourcePool` now pools read-only connections; 8 new unit tests | 4.2 |
| `crates/rb-postgres/src/introspect.rs` | 16 helper signatures take `&ReadOnlyClient`; `analyze_err` takes `BackupError` | 4.2 |
| `crates/rb-postgres/src/immutability.rs` | `table_stat` takes `&ReadOnlyClient` | 4.2 |
| `crates/rb-postgres/Cargo.toml` | `anyhow` dependency (the guard refusal) | 4.2 |
| `docs/IMMUTABILITY.md` | PostgreSQL vector table rewritten: type/guard/server layers, 6 new rows, unit-test evidence IDs; guard-architecture layers 1-2 name what has landed | 4.2 |
| `docs/modules/POSTGRES.md` | "Immutability" now describes the three layers and the role probe | 4.2 |
| `docs/usage/03-postgres.md` | the warning a writable source role triggers, with what still protects the source | 4.2 |
| `README.md` | the PostgreSQL section states the warning and the three protections | 4.2 |
| `docs/QA_GUIDE.md` | link line to both matrix documents in `## The test matrix` | 0.1 |
| `e2e/README.md` | link line to both matrix documents after the script table | 0.1 |
| `e2e/lib.sh` | appended RB_PG_LOG_ARGS, RB_PG_PASSWORD, RB_PG_CONTAINERS and rb_pg_{start,container_ip,load_fixtures,oracle_schema,oracle_counts,assert_readonly_log,assert_connections,assert_no_temp_files} | 0.2 |
| `e2e/fixtures/postgres/README.md` | new: fixture naming rule NN_<kind>[.ge<major>].sql, refusals/, oracle_ignore.txt, 71_rbtest_extension.sh | 0.2 |
| `e2e/fixtures/postgres/oracle_ignore.txt` | new: header comment only, no pattern yet | 0.2 |
| `e2e/README.md` | new section with one row per helper | 0.2 |
| `scripts/help_parity.sh` | ghost-flag exclusion list gains `--schema-only` and `--no-sync` (pg_dump) | 0.2 |
| `scripts/env_inventory.sh` | new, executable: 413 inventory rows; `--check-count 40` exits 0 | 0.3 |
| `scripts/source_readonly_lint.sh` | new, executable: postgres/mongodb/filesystem patterns active, rb-s3 prints SKIP until § 4.4 splits source.rs | 0.4 |
| `e2e/fixtures/postgres/*.sql` | new: 00_roles, 10_tables(+ge11,ge12), 20_sequences, 30_views, 40_matviews, 50_indexes(+ge11,ge15), 60_constraints(+ge11,ge12,ge15,ge18), 70_extensions | 1.1 |
| `e2e/fixtures/postgres/refusals/*.sql` | new: 15 files, one refused object kind each on top of one plain table | 1.1 |
| `e2e/fixtures/postgres/postgis/80_postgis.sql` | new: GIS-01..05, loaded only under RB_PG_IMAGE_REPO=postgis/postgis | 1.1 |
| `e2e/fixtures/postgres/71_rbtest_extension.sh` | new: installs the custom rbtest extension (1.0, or 1.1-only for EXT-08) | 1.1 |
| `e2e/fixtures/postgres/README.md` | documented the .ge sort-order constraint and the postgis/ subdirectory | 1.1 |
| `docs/testing/POSTGRES_MATRIX.md` | Fixture file column reconciled with the files written | 1.1 |
| `e2e/postgres_matrix.sh` | rewritten around the fixtures and the two external oracles; per-row results; refusal loop; legacy assertions kept | 1.2 |
| `e2e/lib.sh` | rb_pg_assert_readonly_log gained an optional since window | 1.2 |
| `e2e/fixtures/postgres/{00_roles,10_tables,10_tables.ge11,30_views,40_matviews,50_indexes,50_indexes.ge11}.sql` | legacy inline-fixture objects (schema app) moved in, names kept | 1.2 |
| `crates/rb-postgres/src/model.rs` | PgView gains options + populated; PgConstraint gains comment (all serde-defaulted) | 1.3 |
| `crates/rb-postgres/src/introspect.rs` | views read reloptions/relispopulated; constraints read obj_description; the view-options refusal probe removed; pg18 NOT NULL probe compares the server default name exactly | 1.3 |
| `crates/rb-postgres/src/ddl.rs` | create_view emits WITH (...); refresh_view skips an unpopulated matview; comment_on_constraint and comment_on_index added and wired | 1.3 |
| `docs/modules/POSTGRES.md` | captured list gains comments, view options and unpopulated matviews; the refusal list loses view options | 1.3 |
| `e2e/postgres_matrix.sh` | per-case counters, diff evaluation restricted to changed lines, refusal reason asserted, bounded source-only wait, epoch since marks | 1.3 |
| `crates/rb-postgres/src/model.rs` | PgExtensionConfig + PgDatabase.extension_configs (serde-defaulted) | 1.4 |
| `crates/rb-postgres/src/introspect.rs` | gather_extension_configs + one extension_config plan item per entry | 1.4 |
| `crates/rb-postgres/src/source.rs` | ItemMeta.kind/.condition, DATA_ITEM_KINDS, conditioned COPY … TO STDOUT | 1.4 |
| `crates/rb-postgres/src/dest.rs` | check_item_kinds (validate + apply), pre_copy_sql DELETE/TRUNCATE, estimate normalization | 1.4 |
| `crates/rb-postgres/src/immutability.rs` | fingerprint covers config tables restricted by their condition | 1.4 |
| `docs/modules/POSTGRES.md` | captured list: extension configuration tables and the cleared-scope restore | 1.4 |
| `docs/usage/03-postgres.md` | "Cosa viene copiato": paragraph on extension configuration tables | 1.4 |
| `e2e/postgres_matrix.sh` | source-only refusal exit code was swallowed by ${refuse_rc:-$?} | 1.4 |
| `crates/rb-postgres/src/lib.rs` | ExtensionVersionPolicy, PostgresParams.extension_version, deviations appended to the verification detail | 1.5 |
| `crates/rb-postgres/src/ddl.rs` | create_extension/build_database_ddl/build_cluster_ddl take the policy; no VERSION clause under default | 1.5 |
| `crates/rb-postgres/src/dest.rs` | DestProbe.available_extensions, extension:<name> preflight checks, reconcile_extension_versions | 1.5 |
| `crates/rust-backup/src/main.rs` | --extension-version flag + RUST_BACKUP_EXTENSION_VERSION + overlay key | 1.5 |
| `docs/usage/03-postgres.md` | module parameter table row | 1.5 |
| `docs/usage/10-variabili-ambiente.md` | RUST_BACKUP_EXTENSION_VERSION row | 1.5 |
| `docs/usage/07-sessioni-yaml.md` | extension_version params key | 1.5 |
| `docs/modules/POSTGRES.md` | "Extension versions" section | 1.5 |
| `e2e/postgres_matrix.sh` | run_extension_version (M-PG-EXT-08) + extension-version-default transfer mode | 1.5 |
| `scripts/env_inventory.sh` | a blank line inside an option block no longer ends it | 1.5 |
| `crates/rb-postgres/src/introspect.rs` | unsupported_probes extracted as a pure fn; every probe ignores extension-owned objects (directly and through the owning relation) | 1.6 |
| `e2e/postgres_matrix.sh` | EXTRA_DEST_ARGS per case, M-PG-GIS-06 cross-pair scenario, bounded run_transfer waits | 1.6 |
| `e2e/lib.sh` | rb_pg_assert_readonly_log rejoins wrapped statement log lines | 1.6 |
| `docs/testing/POSTGRES_MATRIX.md` | resolved PostGIS tag table (R5) and the cross-pair policy note | 1.6 |
| `e2e/README.md` | RB_PG_IMAGE_REPO / RB_PG_NOTEMP_MODE rows, readonly-log helper signature, PostGIS invocation | 1.6 |
| `.github/workflows/e2e.yml` | postgres/postgres-cross-version timeout 80; new postgres-postgis and postgres-postgis-cross-version jobs | 1.7 |
| `docs/QA_GUIDE.md` | PostGIS invocation and the two new CI jobs | 1.7 |
| `README.md` | PostgreSQL backup section: config tables, the refusal message, --extension-version default example and the deviation line; PostGIS e2e command | 1.8 |
| `crates/rb-postgres/src/model.rs` | expected_rows on PgTable, PgView and PgExtensionConfig (serde-defaulted) | 2.1 |
| `crates/rb-postgres/src/introspect.rs` | count_sql + gather_row_counts (test hook RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA); counts carried in the item meta | 2.1 |
| `crates/rb-postgres/src/source.rs` | ItemMeta.expected_rows | 2.1 |
| `crates/rb-postgres/src/dest.rs` | check_expected_rows on every closed COPY, check_matview_rows after post-data, RestoredRows, normalization | 2.1 |
| `crates/rb-postgres/src/lib.rs` | analyze counts rows; the destination carries the totals from stream_in into the report | 2.1 |
| `crates/rb-core/src/verification.rs` | VerificationReport.table_rows_verified / derived_rows_verified + with_rows/rows_total | 2.1 |
| `e2e/postgres_matrix.sh` | ROWS-VERIFIED row (no row-count warning in the destination log) | 2.1 |
| `e2e/fault_matrix.sh` | pg-row-count-mismatch case (T-PG-ROWS-NEG) | 2.1 |
| `docs/modules/POSTGRES.md, docs/usage/03-postgres.md, 10-variabili-ambiente.md, 11-codici-uscita.md` | row-count verification, the test hook and the integrity exit code | 2.1 |
| `crates/rb-postgres/src/model.rs` | PgConstraint.validated / deferrable / initially_deferred (serde-defaulted) | 2.2 |
| `crates/rb-postgres/src/introspect.rs` | CONSTRAINTS_QUERY reads convalidated, condeferrable, condeferred | 2.2 |
| `crates/rb-postgres/src/ddl.rs` | add_constraint returns Result and refuses a state/definition disagreement | 2.2 |
| `crates/rb-postgres/src/dest.rs` | verify_catalog returns CatalogProof with the constraint counters | 2.2 |
| `crates/rb-core/src/verification.rs` | VerificationReport.constraints_verified / constraints_not_valid + with_constraints | 2.2 |
| `e2e/postgres_matrix.sh` | CONSTR-STATE and CONSTR-NOT-VALID-ROW rows | 2.2 |
| `docs/modules/POSTGRES.md` | constraint state among the compared facts | 2.2 |
| `crates/rb-core/src/verification.rs` | VerificationReport.deviations + with_deviations + extra_lines() | 2.3 |
| `crates/rb-core/src/session.rs` | log_verification_extras() after both RESTORE VERIFIED records | 2.3 |
| `crates/rb-postgres/src/lib.rs` | verify() attaches the deviations and logs the facts as fields | 2.3 |
| `crates/rb-postgres/src/dest.rs` | reconcile_extension_versions stores the note without the `deviation: ` prefix | 2.3 |
| `e2e/postgres_matrix.sh` | ROWS-LINE and CONSTR-LINE rows | 2.3 |
| `docs/usage/03-postgres.md` | new section "Cosa stampa la verifica" with a real block | 2.3 |
| `docs/usage/11-codici-uscita.md` | the extra lines named in the success section | 2.3 |
| `docs/modules/POSTGRES.md` | the printed lines documented under the read-back proof | 2.3 |
| `README.md` | the verification block gained the rows/constraints lines and the row-count mismatch remedy | 2.4 |
| `e2e/postgres_matrix.sh` | CONSTR-STATE excludes partition FK clones (cross-major naming) | 2.4 |
| `crates/rb-postgres/src/dest.rs` | matview_count_error / item_end_mismatch helpers, both Phase::Apply | 3.1 |
| `docs/modules/POSTGRES.md` | new section "Runtime model and guardrails" (items a-j) | 3.1 |
| `crates/rb-postgres/src/connect.rs` | SourcePool, session_timeout_options, keepalives | 3.2 |
| `crates/rb-postgres/src/introspect.rs` | introspect_cluster / gather_row_counts take the pool | 3.2 |
| `crates/rb-postgres/src/immutability.rs` | fingerprint takes the pool | 3.2 |
| `crates/rb-postgres/src/source.rs` | stream_out takes the pool, one guard per COPY | 3.2 |
| `crates/rb-postgres/src/lib.rs` | PostgresSource owns the pool; verify uses a destination-side read pool | 3.2 |
| `crates/rb-postgres/src/dest.rs` | verify_catalog re-introspects through a pool | 3.2 |
| `e2e/lib.sh` | rb_pg_watch_connections + rb_pg_assert_connections | 3.2 |
| `e2e/postgres_matrix.sh` | CONN-BUDGET row sampled during the transfer | 3.2 |
| `e2e/fault_matrix.sh` | pg-lock-timeout case (T-PG-TIMEOUT) | 3.2 |
| `docs/modules/POSTGRES.md` | "Connections, timeouts and keepalives" | 3.2 |
| `docs/usage/03-postgres.md` | "Lock, timeout e connessioni" | 3.2 |
| `crates/rb-postgres/src/immutability.rs` | RowCommitment fold, TableStat{rows,commitment}, prefix v2, no COUNT(*) | 3.3 |
| `docs/modules/POSTGRES.md` | new "The source fingerprint" section; Limits bullet now names the data sort only | 3.3 |
| `e2e/postgres_matrix.sh` | NOTEMP reason updated | 3.3 |
| `e2e/postgres_large_table.sh` | new: 2 GiB table, VmHWM sampling, rows-verified and proof assertions | 3.4 |
| `.github/workflows/e2e.yml` | job postgres-large (16, timeout 40) | 3.4 |
| `e2e/README.md` | script row and the two new environment variables | 3.4 |
| `docs/QA_GUIDE.md` | how the memory bound is proved | 3.4 |
| `crates/rb-core/src/module.rs` | Destination::abandon() hook, default no-op | 3.5 |
| `crates/rb-core/src/session.rs` | the destination run calls abandon() when it fails after apply | 3.5 |
| `crates/rb-postgres/src/dest.rs` | RestoredRows.created_databases + abandon_created | 3.5 |
| `crates/rb-postgres/src/lib.rs` | PostgresDestination records what it created and drops it in abandon() | 3.5 |
| `crates/rb-core/tests/session_test.rs` | AbandonRecordingDest + the two hook tests | 3.5 |
| `e2e/fault_matrix.sh` | pg-destination-kill, pg-concurrent-writer, pg-plan-rejected; RUST_BACKUP_E2E_KEEP honoured | 3.5 |
| `docs/modules/POSTGRES.md` | "Failure behaviour" table | 3.5 |
| `README.md` | lock timeout, connections, fingerprint cost and the source-mutation exit | 3.6 |
| `docs/IMMUTABILITY.md` | new: threat model, vectors, fingerprint contract, recipes, guard architecture | 4.1 |
| `docs/modules/POSTGRES.md` | "Immutability" pointer paragraph | 4.1 |
| `docs/modules/MONGODB.md` | "Immutability" pointer paragraph | 4.1 |
| `docs/modules/S3.md` | "Immutability" pointer paragraph | 4.1 |
| `docs/modules/FILESYSTEM.md` | "Immutability" pointer paragraph | 4.1 |
| `README.md` | link line under "Documentation of record" | 4.1 |
| `docs/QA_GUIDE.md` | "Source immutability" section linking the document | 4.1 |

## 6. In-flight work
none — tree consistent

## 7. Verification state

| Gate / test | Command | Last result | When |
|-------------|---------|-------------|------|
| gates | `bash scripts/gates.sh` | PASS (baseline before 0.1 also PASS) | 2026-09-16 |
| relay smoke (T-E2E0) | `bash e2e/relay_smoke.sh` | PASS (5 pass, 0 fail) after the lib.sh edit | 2026-09-16 |
| shellcheck | `docker run --rm koalaman/shellcheck:v0.11.0 <script>` | PASS on e2e/lib.sh | 2026-09-16 |
| T-HARN-INV | `bash scripts/env_inventory.sh --check-count 40` | `PASS` | `scripts/env_inventory.sh --check-count 40` exits 0 — 48 distinct names (§ 0.3); re-run after § 1.5 for RUST_BACKUP_EXTENSION_VERSION |
| T-HARN-LINT | `bash scripts/source_readonly_lint.sh` and `--selftest` | `PASS` | exits 0 on the tree; `--selftest` detects the injected hit (§ 0.4); wired into gates.sh in § 4.6 |
| T-PG-FIX | load every fixture with `rb_pg_load_fixtures` on 10, 12, 16, 18 | `PASS` | all fixtures load with ON_ERROR_STOP on 10, 12, 16, 18 (§ 1.1) |
| T-PG-ORACLE / T-PG-REFUSE | `bash e2e/postgres_matrix.sh 16 10 12 18` | 16: 106 pass 3 fail; 10/12/18 same three (EXT-07 + ORACLE-DATA + OVERWRITE, all the § 1.4 gap); every M-PG-REF row passes; CON-17 passes on 18 | 2026-09-16 |
| § 1.4 gates | `bash scripts/gates.sh` | PASS (83 rb-postgres unit tests, 5 new) | 2026-09-16 |
| § 1.4 matrix | `bash e2e/postgres_matrix.sh 16` | PASS — 109 pass, 0 fail, 10 skip (M-PG-EXT-07, ORACLE-DATA, OVERWRITE now green) | 2026-09-16 |
| § 1.5 gates | `bash scripts/gates.sh` | PASS (87 rb-postgres + 22 rust-backup unit tests) | 2026-09-16 |
| § 1.5 matrix | `bash e2e/postgres_matrix.sh 16` | PASS — 110 pass, 0 fail, 9 skip (M-PG-EXT-08 green) | 2026-09-16 |
| § 1.5 inventory | `bash scripts/env_inventory.sh --check-count 40` | PASS — 49 distinct RUST_BACKUP_* names (+RUST_BACKUP_EXTENSION_VERSION) | 2026-09-16 |
| § 1.6 postgis | RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh 10..18 | PASS — 901 pass, 0 fail, 50 skip over 10,11,12,13,14,15,17,18; 16 => 115/0; 12:16 => 114/0 (GIS-01..06 green) | 2026-09-16 |
| § 1.6 gates | `bash scripts/gates.sh` | PASS (88 rb-postgres unit tests) | 2026-09-16 |
| R5 PostGIS tags | docker manifest inspect postgis/postgis:<tag> | RESOLVED — all nine tags exist: 10-2.5 11-3.3 12-3.4 13-3.5 14-3.5 15-3.5 16-3.5 17-3.5 18-3.6 | 2026-09-16 |
| § 1.7 actionlint | docker run --rm -v "$PWD:/repo" -w /repo rhysd/actionlint:1.7.12 | PASS — no findings | 2026-09-16 |
| phase 1 closing matrix | `bash e2e/postgres_matrix.sh 10 11 12 13 14 15 16 17 18 10:18 12:16 14:17 16:18` | PASS — 1396 pass, 0 fail, 149 skip | 2026-09-16 |
| phase 1 closing matrix (PostGIS) | RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh 10..18 + 12:16 | PASS — 901 pass, 0 fail (majors) plus 115/0 on 16 and 114/0 on 12:16 | 2026-09-16 |
| § 2.1 rows | `bash e2e/postgres_matrix.sh 16` + `bash e2e/fault_matrix.sh postgres` | PASS — 111 pass/0 fail with ROWS-VERIFIED; fault group CASES=4 PASS=11 FAIL=0 including pg-row-count-mismatch (exit 5, database removed) | 2026-09-16 |
| § 2.2 gates | `bash scripts/gates.sh` | PASS (96 rb-postgres unit tests) | 2026-09-16 |
| § 2.2 matrix | `bash e2e/postgres_matrix.sh 16 12` then `10 18` | PASS — 224 pass, 0 fail (16+12) and 217 pass, 0 fail (10+18); CONSTR-STATE and CONSTR-NOT-VALID-ROW green on every major | 2026-09-16 |
| § 2.3 gates | `bash scripts/gates.sh` | PASS (99 rb-postgres + 17 rb-core unit tests) | 2026-09-16 |
| § 2.3 matrix | `bash e2e/postgres_matrix.sh 16` | PASS — 115 pass, 0 fail, 9 skip (ROWS-LINE, CONSTR-LINE, M-PG-EXT-08 green) | 2026-09-16 |
| § 2.3 block compatibility | `bash e2e/relay_smoke.sh` + `bash e2e/session_two_targets.sh` | PASS — T-E2E0 PASS=5 FAIL=0; T-SESSION-2 passed | 2026-09-16 |
| § 2.4 full matrix | RUST_BACKUP_FULL_DB_MATRIX=1 `bash e2e/full_matrix.sh` | 12 passed, 1 failed — the single failure was CONSTR-STATE on 16:18 (partition FK clone naming); after the fix `postgres_matrix.sh 16:18 12:16` => 228 pass, 0 fail | 2026-09-16 |
| § 2.4 cross pairs | bash e2e/postgres_matrix.sh 10:18 14:17 | PASS — 217 pass, 0 fail (the other two pairs after the CONSTR-STATE clone fix) | 2026-09-16 |
| § 2.4 gates | bash scripts/gates.sh | PASS | 2026-09-16 |
| § 3.1 gates | bash scripts/gates.sh | PASS (98 rb-postgres unit tests, 2 new review tests) | 2026-09-16 |
| § 3.2 conn budget | bash e2e/postgres_matrix.sh 16 | PASS — 116 pass, 0 fail; CONN-BUDGET green (peak 1 connection, budget 3) | 2026-09-16 |
| § 3.2 lock timeout | bash e2e/fault_matrix.sh postgres | PASS — CASES=5 PASS=15 FAIL=0; pg-lock-timeout fails in 30s, tagged [Analyze], retry succeeds | 2026-09-16 |
| § 3.2 full range | bash e2e/postgres_matrix.sh 10 11 12 13 14 15 16 17 18 | PASS — 1025 pass, 0 fail, 99 skip (CONN-BUDGET green on every major) | 2026-09-16 |
| § 3.3 fingerprint v2 | bash e2e/postgres_matrix.sh 10 16 | PASS — 221 pass, 0 fail; IMMUT-ABORT/IMMUT-FULL/IMMUT-READONLY-LOG green on both; source temp files 12 => 6 per case | 2026-09-16 |
| § 3.3 gates | bash scripts/gates.sh | PASS (108 rb-postgres unit tests, 7 new) | 2026-09-16 |
| § 3.4 large table | bash e2e/postgres_large_table.sh 16 | PASS — 5 pass, 0 fail; peak RSS source 14056 KiB, destination 22592 KiB, cap 262144 KiB; 2000000 rows verified | 2026-09-16 |
| § 3.5 fault matrix | bash e2e/fault_matrix.sh postgres, then all groups | PASS — postgres CASES=8 PASS=21 FAIL=0; full run CASES=28 PASS=69 FAIL=0 | 2026-09-16 |
| phase 3 closing matrix | bash e2e/postgres_matrix.sh 10 11 12 13 14 15 16 17 18 | PASS — 1025 pass, 0 fail, 99 skip; CONN-BUDGET green on all nine majors; relay_smoke and session_two_targets PASS | 2026-09-16 |
| § 4.1 gates | bash scripts/gates.sh | PASS | 2026-09-16 |
| § 4.2 gates | bash scripts/gates.sh | PASS (116 rb-postgres unit tests, 8 new: the guard, the type assertion and the role-probe SQL) | 2026-09-16 |
| § 4.2 lint | bash scripts/source_readonly_lint.sh (+ --selftest) | PASS — 9 files clean, selftest still detects an injected write | 2026-09-16 |
| § 4.2 matrix (T-PG-ORACLE) | bash e2e/postgres_matrix.sh 10 16 18 | PASS — 339 pass, 0 fail, 35 skip (10: 105/0/19, 16: 116/0/9, 18: 118/0/7) | 2026-09-16 |
| § 4.3 gates | bash scripts/gates.sh | PASS (36 rb-mongodb unit tests, 5 new: the command allowlist both ways, the refusal message, the type assertion, the pinned read settings) | 2026-09-16 |
| § 4.3 lint | bash scripts/source_readonly_lint.sh (+ --selftest) | PASS — 11 files (the two `connect.rs` wrappers added), selftest still detects an injected write | 2026-09-16 |
| § 4.6 gates (T-IMMUT-LINT) | bash scripts/gates.sh | PASS — the run now prints `source read-only lint: PASS (12 files, no write-shaped call)` and `selftest PASS (injected write detected)` between the crate invariants and the help parity | 2026-09-16 |
| § 4.6 shellcheck | docker run --rm koalaman/shellcheck:v0.11.0 --severity=warning scripts/{gates,help_parity,source_readonly_lint}.sh | PASS — no findings | 2026-09-16 |
| § 4.5 gates | bash scripts/gates.sh | PASS (17 rb-filesystem unit tests, 4 new; 23 rust-backup unit tests, 1 new); help_parity PASS; `scripts/env_inventory.sh --check-count 40` => 51 names | 2026-09-16 |
| § 4.5 atime refusal (T-FS-ATIME, non-root part) | release binary, source root `/etc/skel` (root-owned, world-readable), no flag | PASS — exit 1 with `[Analyze] cannot open /etc/skel/.bash_logout without updating its access time (O_NOATIME needs file ownership or CAP_FOWNER); run as the file owner or root, or pass --allow-atime-updates to accept atime changes on the source`; the tree's atimes unchanged; no destination root created | 2026-09-16 |
| § 4.5 atime accepted (T-FS-ATIME, non-root part) | same tree with `--allow-atime-updates`, destination `--no-preserve-ownership` | PASS — both peers exit 0, exactly one `WARN atime updates on the source accepted by --allow-atime-updates`, `BACKUP VERIFIED` / `RESTORE VERIFIED` over 11 items / 119569 bytes | 2026-09-16 |
| § 4.5 regression | bash e2e/relay_smoke.sh | PASS — T-E2E0 PASS=5 FAIL=0 (an owner run is unaffected by the guard) | 2026-09-16 |
| § 4.4 gates | bash scripts/gates.sh | PASS (13 rb-s3 unit tests, unchanged — the move is mechanical) | 2026-09-16 |
| § 4.4 lint | bash scripts/source_readonly_lint.sh (+ --selftest) | PASS — 12 files, `crates/rb-s3/src/source.rs` now checked instead of skipped | 2026-09-16 |
| § 4.4 minio (T-S3-MINIO, T-S3-IMMUT) | bash e2e/s3_minio_test.sh | PASS — `S3 MinIO e2e passed (metadata/policy/key-set read-back, overwrite cleanup, multipart abort cleanup, source immutability)` | 2026-09-16 |
| § 4.3 matrix (T-MONGO-MATRIX) | bash e2e/mongodb_matrix.sh 4 8 then 7 4:7 | PASS — 4: PASS=5 FAIL=0; 7: PASS=5 FAIL=0; 4:7: PASS=5 FAIL=0. `8` SKIP: `mongo:8 refuses to start on Linux 7.0.0-31-generic (SERVER-121912)` — the documented host limit, 7 runs in its place | 2026-09-16 |
| § 4.2 role probe | rust-backup plan postgres on postgres:16-alpine, as `postgres` then as `rb_ro` (pg_read_all_data) | PASS — one `WARN source role postgres can write to the source; a read-only role is recommended, see docs/IMMUTABILITY.md` for the superuser, no warning for `rb_ro`, both plans exit 0 | 2026-09-16 |

**Failing output (verbatim, trimmed to the error):**
```
<none>
```
| § 4.7 T-IMMUT-PG-LP | `bash e2e/postgres_matrix.sh 16 10` then `13 14 18` | PASS on every major — 16: 117 pass/0 fail, 10: 106/0, 13: 115/0, 14: 115/0, 18: 115/0; each prints `PASS T-IMMUT-PG-LP` after a full overwrite transfer run as `rb_ro` | 2026-09-16 |
| § 4.7 T-IMMUT-MONGO-LP | `bash e2e/mongodb_matrix.sh 4` then `6 7` | PASS on 4, 6 and 7 — `mongod commands checked: 110, all reads` per case; 8 unrunnable on this kernel (§ 9) | 2026-09-16 |
| § 4.7 T-IMMUT-S3-LP | `bash e2e/s3_minio_test.sh` | PASS — `MinIO S3 APIs checked: 25, all reads`, the destination matches the LP source, and `mc cp` with the same credential is refused | 2026-09-16 |
| § 4.7 gates | `bash scripts/gates.sh` | PASS (ghost-flag scan included, after `--profile`/`--slowms`/`--json` were excluded as foreign switches) | 2026-09-16 |
| R8 MinIO policy action names | `mc admin policy create` against MinIO RELEASE.2025-09-07 | RESOLVED — `s3:GetObjectAcl` is rejected (`unsupported action`) and `s3:GetBucketLocation` is not needed; the working set is `s3:ListBucket`, `s3:GetBucketPolicy`, `s3:GetBucketVersioning`, `s3:GetObject`, `s3:GetObjectTagging` | 2026-09-16 |
| R9 mongod JSON log shape | `docker logs` on `mongo:4.4`, `mongo:6.0`, `mongo:7.0` with `--profile 0 --slowms 0` | RESOLVED — every command appears as a `"c":"COMMAND"` record with `attr.command` and `attr.appName`, identical shape on all three majors | 2026-09-16 |
| § 4.8 T-IMMUT-PANIC | `cargo test -p rb-core --test session_test` | PASS — 20 tests, the two new ones green: the panic surfaces as `BackupError::Other` naming it, the fingerprint ran twice, and drift on the panic path reports `SourceMutated` | 2026-09-16 |
| § 4.8 gates | `bash scripts/gates.sh` | PASS | 2026-09-16 |
| § 4.8 regressions | `bash e2e/relay_smoke.sh`, `bash e2e/session_two_targets.sh` | PASS — T-E2E0 5 pass/0 fail, T-SESSION-2 passed (the runner is unchanged on the normal paths) | 2026-09-16 |
| § 4.9 gates | `bash scripts/gates.sh` | PASS — the ghost-flag scan accepts the README's `--allow-atime-updates` and finds no flag the CLI lacks | 2026-09-16 |
| phase 4 regression guard | `bash e2e/postgres_matrix.sh 10 16 18`, `bash e2e/mongodb_matrix.sh 4`, `bash e2e/s3_minio_test.sh`, `bash e2e/relay_smoke.sh`, `bash e2e/session_two_targets.sh` | PASS — postgres 10: 106/0, 16: 117/0, 18: 119/0; mongodb 4: 6 pass 0 fail; MinIO passed incl. T-IMMUT-S3-LP; T-E2E0 5/0; T-SESSION-2 passed. T-FS-OWN/T-FS-IMMUT could not run (no passwordless sudo, § 9) | 2026-09-16 |

## 8. Runtime deviations from the plan

One row per deviation, including a superseded `D*` decision (the new row itself
goes in `overview.md`) and every edit made to a not-yet-executed phase file.
Also record here the resolution of each `UNVERIFIED` reference (R5-R9 in `overview.md`).

| # | Plan said | What was done | Why | Impact on later phases |
|---|-----------|---------------|-----|------------------------|
| 1 | both matrix documents use the columns `ID | Case | Min major | Fixture file | Expected | Oracle` | `FILESYSTEM_MATRIX.md` uses `ID | Case | Requires | Fixture | Expected | Oracle` | the filesystem module has no server major; the column carries the required privilege (`root`/`none`) instead, which is the fact the runner needs | none — § 5.2 fills a `Fixture` column, § 5.3 reads the `Requires` column |
| 2 | `RB_PG_LOG_ARGS` sets `log_line_prefix='%u@%d %m '` | `log_line_prefix=%u@%d|%m|` (no spaces, `|` separated) | the string is word-split into `docker run ... postgres $RB_PG_LOG_ARGS`; a value containing spaces breaks into invalid arguments | `rb_pg_assert_readonly_log` matches the prefix `^<user>@[^|]*|`; runners must not assume a space-separated prefix |
| 3 | § 0.2 touches only `e2e/lib.sh`, the fixture files and `e2e/resource_hygiene_check.sh` | `scripts/help_parity.sh` also edited: `--schema-only` and `--no-sync` added to the ghost-flag exclusion list | the new helper table in `e2e/README.md` names pg_dump's own flags, which the scan reported as flags the CLI does not accept | none; § 6.1-6.4 keep the exclusion list when building `scripts/docs_parity.sh` |
| 4 | recon counted 40 clap `env=` names in `crates/rust-backup/src/main.rs` | the binary exposes 38; the inventory's 48 distinct `RUST_BACKUP_*` names come from 38 clap names plus code and test reads | the recon figure was off by two — the enumerated list in §2 itself holds 38 names, and `grep -oE 'env = "RUST_BACKUP_[A-Z0-9_]+"' main.rs | sort -u` returns exactly those 38 | `--check-count 40` still passes because the threshold counts every `RUST_BACKUP_*` name; § 6.1-6.2 must audit 38 clap names, not 40 |
| 5 | `80_postgis.sql` sits in `e2e/fixtures/postgres/` | it sits in `e2e/fixtures/postgres/postgis/` | `rb_pg_load_fixtures` loads every `*.sql` of the directory it is given, so a PostGIS-only file in the main directory would be loaded on plain postgres images too | § 1.2/1.6: the runner calls `rb_pg_load_fixtures` a second time over `postgis/` when `RB_PG_IMAGE_REPO=postgis/postgis` |
| 6 | M-PG-EXT-06 installs `hstore` in schema `ext` | it installs `tablefunc` in schema `ext` | `hstore` is already installed in `public` for EXT-03 and PostgreSQL allows one installation of an extension per database | none — the property under test is `pg_extension.extnamespace` |
| 7 | § 1.2 asserts `rb_pg_assert_readonly_log <src> postgres` over the container log | the assertion takes a `since` timestamp and covers only the transfer window | the harness itself seeds the fixtures as `postgres`, so the whole-log form would report the fixture DDL as a source write | § 4.7 replaces the user with `rb_ro` and can drop the window |
| 8 | the refusal loop runs a source and a destination | it runs the source only | a plan refused during Analyze is refused before the source registers the channel, so a destination would only wait out its registration timeout; the loop still asserts that the destination container holds no database of that name | none |
| 9 | § 1.3 adds the tests `inheritance_parent_item_is_only_its_rows`, `partitioned_parent_has_no_data_item`, `copy_column_list_excludes_generated_columns`, `pg18_auto_named_not_null_is_accepted` | not added: the repository already has `an_inheritance_parent_is_read_with_only`, `build_plan_skips_child_partitions`, `build_plan_streams_only_data_bearing_tables` (which asserts the generated-column COPY list) and `constraints_query_excludes_pg18_not_null_catalog_constraints` | they assert exactly the same behaviour under different names; a duplicate test costs maintenance without adding coverage | none — the plan's intent is covered; the pg18 name rule additionally gained live evidence (auto-named accepted, custom name refused) |
| 10 | PgPlanPayload.extension_configs | PgDatabase.extension_configs | extensions are per-database; gather_extensions already runs inside the per-database loop and a cluster-wide list could not say which database a relation belongs to | none — still serde-defaulted and part of payload equality |
| 11 | item name "<schema>.<table>" | item name "<db>.<schema>.<table>" | table items already carry the database in the name; two databases can both register the same extension table | none — the name is display only, routing uses meta.database |
| 12 | COPY (SELECT * FROM <qual> WHERE <condition>) | COPY (SELECT * FROM <qual> <condition>) | pg_extension.extcondition already contains its own leading WHERE; adding a second one is a syntax error (the plan test string shows the same shape) | none — matches the expected SQL in the plan unit test |
| 13 | § 1.4 touches no harness file | fixed run_transfer source-only in e2e/postgres_matrix.sh | ${refuse_rc:-$?} never substitutes when refuse_rc is the string 0, so every refusal exit code was discarded and all 14 M-PG-REF rows reported "the source produced a plan for a refused object" | harness-only; found by the § 1.4 run, fixed there rather than left to fail a later phase |
| 14 | clap `default_value = "source"` | no clap default; the module's serde default is `source` | a clap default is folded into the params overlay on every run, so a YAML `extension_version: default` could never win — the same precedence bug `Option<bool>` exists to avoid for the boolean flags | none for the CLI (absent still means source); YAML keeps working as documented |
| 15 | append the deviation to the VerificationReport notes | verify_catalog returns Vec<String>, appended to VerificationReport.detail | VerificationReport has no notes/lines field — only items_verified, bytes_verified, payload_blake3 and detail; detail is what the session logs on the RESTORE VERIFIED line | the e2e assertion reads the same line; no wire-format change |
| 16 | reject --extension-version when the module is not postgres or the role is not destination, like --overwrite | no rejection: the flag is a shared module param, exactly like --overwrite | there is no per-module/role validation site in main.rs; --overwrite is simply ignored by the modules and roles that do not read it | extension_version_flag_is_destination_only asserts the plumbing and the value_parser instead of a rejection that the codebase never had |
| 17 | § 1.5 touches no harness file | hardened scripts/env_inventory.sh | a blank line ended an option block, so clap's paragraph separator inside a long help hid the following [env: …] — the inventory dropped from 48 names to 37 with no error | harness-only; the check-count guard now measures the real surface (49) |
| 18 | fix any GIS row failure with the § 1.3 procedure | changed the refusal probes instead of any GIS row: an object owned by an extension (pg_depend deptype=e, directly or through its relation) no longer refuses the cluster | PostGIS defines three rules on public.geometry_columns, so every PostGIS cluster was refused at Analyze; CREATE EXTENSION recreates those objects verbatim on the destination, so nothing is lost by ignoring them | M-PG-REF-* rows still fail-closed (their objects are user-owned); a future extension-owned trigger or policy is no longer a refusal |
| 19 | § 1.6 touches postgres_matrix.sh and the fixtures | also hardened e2e/lib.sh and run_transfer | a multi-line statement is logged as a prefixed line plus unprefixed continuations, so the PostGIS spatial_ref_sys COPY looked like a write; and a destination that dies before the handshake left run_transfer waiting on the source forever | harness-only; both are prerequisites for the phase 4 least-privilege log assertions |
| 20 | README anchors: Modules -> PostgreSQL, the environment-variable table at README.md:136, Limitations | everything went into the PostgreSQL backup section | the README has no Limitations section and its only variable table describes compose.yml, not module flags; the module section already states that every flag has a RUST_BACKUP_ equivalent and links the variables page | none — 6.1-6.4 audit the README against the same rule |
| 21 | the oracle compares every constraint | constraints cloned into a partition (conparentid <> 0) are excluded, and OVERWRITE compares through normalize_counts | the server names a partitioned FK's clones itself and changed the algorithm in 17 (c_con_07_1 vs t_con_07_child_ref_ref_ts_fkey), so a faithful 16:18 restore failed ORACLE-DATA, M-PG-CON-07 and OVERWRITE; OVERWRITE additionally compared raw oracle output across majors | harness-only; the parent constraint is still compared in full |
| 22 | count rows inside analyze() for every table data item, and extension_config items with FROM ONLY | a separate gather_row_counts pass after introspection; configuration tables are counted without ONLY | introspect_cluster also runs on both fingerprint audits and on the destination read-back, none of which needs a second full scan; and the config item streams without ONLY, so counting with it would compare two different scopes | none — the counts land in the same meta key the plan specifies |
| 23 | add totals tables_rows_verified, matviews_rows_verified, rows_total to the report | table_rows_verified and derived_rows_verified, with rows_total() derived | the two numbers have different provenance (streamed vs produced by REFRESH) and every other module can leave them at zero; a stored total would be a third field that can disagree with the two | 2.3 prints rows_total() and the two components |
| 24 | report counters constraints_total, constraints_validated, constraints_not_valid | constraints_verified and constraints_not_valid; validated = verified - not_valid | a stored third counter can disagree with the other two; the destination counts what it re-introspected, and 'verified' is what every other counter in VerificationReport is named | 2.3 prints 'constraints: <verified> (validated <verified-not_valid>, not valid <not_valid>)' |
| 25 | positive control: a scratch database where the fixture runs ALTER TABLE ... VALIDATE CONSTRAINT | no scratch database; the CON fixtures already mix validated and NOT VALID constraints and CONSTR-STATE compares convalidated/condeferrable/condeferred per name on both sides | the mixed fixture is the stronger control — it proves validated=true is carried AND validated=false is not silently validated, in the same run, on every major | none — T-PG-CONSTR asserts both polarities; CONSTR-NOT-VALID-ROW additionally proves the violating row survived |
| 26 | the line reads `rows verified: <t> tables, <m> materialized views, <n> rows` | rows verified: <t> from tables, <m> from materialized views, <n> rows | the two numbers are rows by origin, not object counts; the plan wording reads as "3 tables" and would misreport 103590 tables | none — README (2.4) and the e2e assertion use the shipped wording |
| 27 | append every `deviation: ...` note from § 1.5 to the block | VerificationReport gained a serde-defaulted `deviations: Vec<String>`; the note is stored bare and the printer adds the `deviation: ` prefix, while `detail` keeps carrying the notes | the note was already prefixed inside `detail` (deviation 15), so printing it through a prefixing line produced `deviation: deviation: ...`; detail keeps them because the source peer reads the report, not the destination lines | none — the e2e grep on the full note text still matches; additive serde field, older peers deserialize |
| 28 | verification_block_existing_lines_unchanged does a golden string comparison of the pre-existing lines | it asserts the four pre-existing report fields (items, bytes, blake3, detail) verbatim and that extra_lines() is empty for a module that counts none of the new facts | the headline record is a tracing event with structured fields; capturing it would mean adding tracing-subscriber as a dev-dependency of rb-core to prove a call site the change never edited | none — relay_smoke and session_two_targets prove the block format live |
| 29 | README "Troubleshooting" gains the row-count message | the message went into the "PostgreSQL backup" section, next to the extension-version refusal it sits beside operationally | the README has no Troubleshooting section (same finding as deviation 20 for "Limitations"); inventing one for a single message would fragment the module section | none — 6.1-6.4 audit the README against the sections it actually has |
| 30 | § 2.2 CONSTR-STATE compares every c_con_% constraint on both sides | it excludes partition FK clones (conparentid <> 0), like the § 1.3 oracle already did | PostgreSQL >= 17 names a partitioned FK clone <parent>_<n> (c_con_07_1), which matches the fixture prefix, while <= 16 names it after the table; the 16:18 pair failed on names the server chose, not on state | none — the parent constraint is still compared in full; found by the phase 2 regression run, fixed in § 2.4 |
| 31 | item (a): grep connect_admin usages, must be destination only | the read-write constructor is PgConnection::connect; connect_admin does not exist | the plan recon named a function the module never had; the property is the same and holds | none — 3.2 touches connect.rs and uses the real names |
| 32 | R7: keepalives_interval / keepalives_retries may need a newer tokio-postgres 0.7.x | RESOLVED — the locked 0.7.18 exposes keepalives, keepalives_idle, keepalives_interval and keepalives_retries; all four are set | checked the vendored source of the locked version (config.rs:467-516) instead of assuming | none — no dependency bump needed |
| 33 | T-PG-CONN asserts rb_pg_assert_connections <src> <user> 3 after the run | a background watcher samples pg_stat_activity during the transfer and the assertion judges the peak afterwards | connections only exist while the run does; an assertion taken after it would always read zero | harness-only; CONN-BUDGET is a matrix row and reads a samples file |
| 34 | T-PG-TIMEOUT locks mx.t_tab_01 and expects the destination to clean up | it locks app.rows (the fault fixture's own table) and runs the source alone, asserting that no database was created | the fault matrix has its own faultdb fixture, and the lock fails the run during Analyze — before the source registers the channel — so a destination would only sit out its registration timeout | none — the case additionally proves the same run succeeds once the lock is released |
| 35 | T-PG-FP2: zero `temporary file` lines on the source during the matrix run; remove the pgsql_tmp Limits bullet | the fingerprint no longer spills (12 => 6 files per case on 16), the data stream still does; the Limits bullet was rewritten instead of removed and NOTEMP stays in reporting mode | the spill has two sources and § 3.3 only owns one: the data stream sorts by the C-collated row text so that the destination read-back reproduces the per-item BLAKE3, which a partitioned parent (read in a different physical order on the two sides) would otherwise break | T-PG-FP2 is partially met and recorded as such in § 9; removing the data sort needs the read-back to compare a commitment instead of the streamed bytes — a transport-level change no phase of this plan owns |
| 36 | § 3.5 touches crates/*.rs only if a case fails | a case did: the concurrent-writer run left the restored database behind, so rb-core gained a defaulted Destination::abandon() hook and the postgres destination drops what the run created | the cleanup only covered failures during apply; a source that mutates (or a read-back that fails) is reported after the load, and left a complete-looking, uncertified database — exactly what the module documents it never leaves | additive trait method with a default no-op, so mongodb, s3 and filesystem are unchanged; phase 4/5 may implement it for their modules |
| 57 | § 4.6 wires the lint into `scripts/gates.sh` | also its `--selftest`, and `scripts/help_parity.sh` gained `--all`, `--all-targets` and `--selftest` to its foreign-flag exclusion list | a lint whose regexes stop matching passes silently, which is worse than no lint; and documenting the new gate command in the QA guide named three cargo/script switches that the ghost-flag scan then reported as flags the CLI does not accept | none — the exclusion list exists for exactly this (cargo, docker, git, `pg_dump`), and the CLI's own flags are still checked in both directions |
| 52 | § 4.5 the flag is `ArgAction::SetTrue` | `Option<bool>` with `boolish()`, `num_args(0..=1)`, `default_missing_value("true")`, `require_equals` | the convention `main.rs` documents for every module bool: `SetTrue` collapses "absent" and "given as false", so a YAML `allow_atime_updates: true` could never be turned off from the CLI, against the documented CLI > env > YAML precedence | none — `--allow-atime-updates` and `--allow-atime-updates=false` both work |
| 53 | § 4.5 the flag is rejected on non-filesystem modules and on the destination role, following the `--no-preserve-ownership` pattern | no rejection: the flag merges into the params like every other module bool, and only the filesystem source reads it | that pattern does not exist — `--no-preserve-ownership` (like `--follow-symlinks`, `--path-style`, `--admin`) is merged and left to the module, and the param structs ignore unknown keys; inventing a rejection table here would be new machinery in `main.rs`, outside this sub-phase | none — documented as `Lato: sorgente` in `docs/usage/05-filesystem.md`, and `allow_atime_updates_flag_is_filesystem_source_only` locks the key, the typed default and the explicit `false` |
| 69 | § 4.9 puts the atime refusal and its remedy in a README "Troubleshooting" section | the refusal and both remedies are in the new "Source safety" section | `README.md` has no troubleshooting section — the guide it points to (`docs/usage/`) carries that role — and splitting a message from the rule it enforces would make both harder to find | § 6.5's final README read should confirm this is still the right home |
| 66 | § 4.8 the panic test mock panics inside `stream_out` with `panic!` | the mock indexes an empty `Vec` out of bounds | `panic!` is forbidden in this workspace's lints, and an out-of-bounds index is the shape a real bug takes; a constant index on an array would be rejected at compile time by `unconditional_panic`, so the index comes from `Vec::capacity()` | none |
| 67 | § 4.8 `catch_unwind` is applied at `session.rs:326` | applied where `source_run` is awaited, which § 3.2-3.5 moved | the plan's line numbers predate those sub-phases; the call site is the same single statement | none |
| 68 | (unstated) rb-core dependencies | `futures-util` added to `crates/rb-core/Cargo.toml` | `FutureExt::catch_unwind` lives there and rb-core did not depend on it (rb-transport did) | none — workspace dependency, already in the lock file |
| 58 | § 4.7 MongoDB LP role: `read` plus a custom `rbView` on `admin` with `viewUser`, `viewRole`, `listDatabases`, `serverStatus` | `rbView` carries only `viewUser` and `viewRole`, and on the copied database rather than the cluster | the source never runs `listDatabases` or `serverStatus`, so granting them would document a privilege the tool does not use — the point of the recipe is the minimum, and the LP case fails if the minimum is wrong | none — the narrower set is what `docs/IMMUTABILITY.md` now documents |
| 59 | § 4.7 `rb_mongo_assert_readonly_log` fails when a command's first key is a write name, `aggregate` included | `aggregate` is judged by its pipeline: only `$out`/`$merge` (or a pipeline the server truncated) count as writes | the first run failed on five `aggregate` records from our own traffic: the mongodb Rust driver implements `count_documents` as an `aggregate` with `$match`/`$group`, so a read-only aggregate is expected and the source guard only ever sees `run_command` | none — the guard still refuses `aggregate` as a command; the assertion is about what the server saw |
| 60 | § 4.7 asserts on every `"c":"COMMAND"` record in the window | only records with `attr.appName == "rust-backup"` | mongod's own logical-session-cache refresh writes to `config` and is logged too; attributing the server's housekeeping to this tool would be a false failure, and the driver already tags its connections (`connect.rs:50`) | none — the assertion also fails when the window holds no record of ours, so the filter cannot hide traffic |
| 61 | § 4.7 MongoDB LP runs against the existing source container | a second source container per case, with `--auth --profile 0 --slowms 0`, seeded identically | authentication cannot be enabled on a running mongod, and a server without `--auth` enforces no role at all, so the LP claim would be untested; profiling stays at level 0 so the evidence itself writes nothing to the source | none — the destination is reused with `--overwrite`, and the case compares it against the LP source's own digest |
| 62 | § 4.7 S3: "success is the proof" (any write attempt is `AccessDenied`) | plus two explicit proofs: `mc admin trace --json` is captured for the transfer window and every `s3.*` API in it must be a read, and `mc cp` with the same credential must be refused | "the backup succeeded" only proves the read grants are sufficient; the server-side evidence layer of `docs/IMMUTABILITY.md` claims more than that for PostgreSQL and MongoDB, and S3 should not be the weak row | none |
| 63 | § 4.7 S3 LP runs against the seeded source bucket as it stands | the harness first runs `mc anonymous set none src/source` | the earlier cases make the bucket anonymously downloadable, and an anonymous `s3:GetObject` grant would authorize the reads whatever the user policy said — removing it is what makes the policy under test the thing being tested | none — the object listing before/after is unchanged, so the immutability comparison is unaffected |
| 64 | (unstated) the ghost-flag scan | `--profile`, `--slowms` and `--json` excluded as foreign switches | `docs/IMMUTABILITY.md` and `e2e/README.md` now quote `mongod --profile 0 --slowms 0` and `mc admin trace --json`; the scan has no notion of which program a switch belongs to | `--json` is generic enough that a future `docs/usage/` page claiming it would no longer be caught — the docs-parity gate of § 6.1 is where that gap belongs |
| 65 | (unstated) credentials in the e2e harness | the LP source password and the MinIO secret key reach the tool through `RUST_BACKUP_PASSWORD` / `RUST_BACKUP_ACCESS_KEY` / `RUST_BACKUP_SECRET_KEY` in the child's environment, and the container-side secret through `docker run -e RB_RO_SECRET` with no value | a flag would publish the secret in the host process list, which is exactly what the tool's own documentation tells operators not to do | none — the pre-existing `minioadmin` root flags in `e2e/s3_minio_test.sh` are untouched (container defaults, not a deployment) |
| 57 | (unstated) `e2e/lib.sh` carried two `rb_pg_assert_connections` definitions | the dead first one (container/user/max, reading the log live) is removed | the samples-based version from § 3.2 shadowed it, so the file documented an assertion that could never run | none |
| 54 | § 4.5 unit tests use an `open_with(flags, fallback: bool)` seam | the seam is `NoAtimeReader::open_with(path, allow_atime_updates, phase, opener)`, where `opener` answers the two `open` attempts | the test needs to control what the *kernel* answers, not which flags we pass; injecting the opener also proves the refusal never retries and the accepted path retries exactly once without `O_NOATIME` | none |
| 55 | § 4.5 e2e T-FS-ATIME runs under `sudo` in `e2e/filesystem_netns_test.sh` | the case is written (case (d), root-owned tree read by `nobody`, both polarities) but could not be run here; the same two polarities were proven without root against `/etc/skel` | `sudo -n` on this host asks for a password (§ 9), so no privileged e2e can run in this session; `/etc/skel` is root-owned and world-readable, which produces exactly the `EPERM` the guard is about | § 5.3 must run `sudo -n bash e2e/filesystem_netns_test.sh` (and the filesystem matrix) on a host with passwordless sudo, or in CI |
| 56 | (unstated) rb-filesystem dependencies | `tracing` added to `crates/rb-filesystem/Cargo.toml` | the accepted-atime path warns once per run and the crate had no logging dependency | none — workspace dependency, already in the lock file |
| 48 | § 4.4 `ReadOnlyS3` exposes `list_objects_v2`, `head_object`, `get_object`, `get_object_tagging`, `get_object_acl`, `get_bucket_policy`, `get_bucket_location` | plus `get_bucket_versioning` | `validate_source_fidelity` refuses a versioned bucket, and it reads that status through the wrapper; without the method the check would need a raw client, which is the thing the type exists to prevent | none — `get_bucket_location` is kept (and marked `allow(dead_code)`) because the plan lists it as part of the read surface |
| 49 | § 4.4 moves `S3Source` and its `Source` impl into `source.rs` | also `list_objects` and `validate_source_fidelity` | both are source-only and both hold the client; leaving them in `lib.rs` would leave the two functions that do the actual reading outside the file the lint checks | none — `unsupported_object_features` and `acl_is_owner_only_full_control` stay in `lib.rs` (the destination and the unit tests use them, and neither touches a client) |
| 50 | (unstated) the S3 client is built per call (`client(&self.params).await` in five places) | `S3Source` builds it once in `open_source` and holds it in `ReadOnlyS3` | the plan says the source holds the wrapper, which means one client per run instead of one per phase; SDK credential resolution is lazy, so nothing about error timing changes | none — the destination still builds its own per call |
| 51 | § 4.4 the lint's rb-s3 branch prints `SKIP` when `source.rs` is absent | the branch now fails instead | the file is the deliverable of this sub-phase; a `SKIP` would silently stop checking S3 if the read side were ever folded back into `lib.rs` | none |
| 42 | § 4.3 `ClientOptions` field names for read preference / read concern are UNVERIFIED | RESOLVED — mongodb 3.9.1 has `ClientOptions::selection_criteria: Option<SelectionCriteria>` (set to `SelectionCriteria::ReadPreference(ReadPreference::Primary)`) and `ClientOptions::read_concern: Option<ReadConcern>` (set to `ReadConcern::local()`) | read from the locked crate source (`client/options.rs:598,626`, `concern.rs:59`, `selection_criteria.rs:19,102`) instead of assuming | none — no dependency bump; `read_only_options_pin_primary_and_local` locks the behaviour |
| 43 | § 4.3 `ReadOnlyDatabase` exposes `list_collection_names`, `list_collections`, `find`, `count_documents`, `list_indexes`, `run_command` | plus `estimated_document_count` and `name`; `list_collections` and `list_indexes` return a `Vec` instead of the cursor | introspection counts with `estimated_document_count`, and the two spec listings are small and were already collected at the call site — returning the cursor would keep `TryStreamExt` (and the raw driver types) in the source files | none — the wrapper is the only handle either way |
| 44 | § 4.3 `read_only_database_exposes_no_write_methods` is type-level, asserted via the lint script | both: a source-text assertion over the two `impl` blocks (no write call, no `-> Database`/`-> Collection`, every `command: Document` method calls the guard) *and* the lint script's file list extended with `crates/rb-mongodb/src/connect.rs` and `crates/rb-postgres/src/connect.rs` | the wrapper file is where a write method would be added, and it was in neither list; the in-crate assertion fails without waiting for the gate | § 4.6 wires the (now wider) lint into `gates.sh` |
| 45 | (unstated) the command allowlist matches the names given | matched case-insensitively | MongoDB spells `isMaster` lowercase on older majors, and no write command differs from an allowlisted name only by case; a name the server does not recognize fails there anyway | none |
| 46 | (unstated) rb-mongodb dependencies | `anyhow` added to `crates/rb-mongodb/Cargo.toml` | the refusal the plan specifies is `BackupError::Other(anyhow!(…))` | none — workspace dependency, already in the lock file |
| 47 | § 4.3 Done: T-MONGO-MATRIX on 4 and 8 | 4, 7 and the cross pair 4:7; `8` is skipped by the runner on this host | every published MongoDB 8 image refuses to start on a kernel newer than 6.19 (SERVER-121912) and this host runs 7.0.0-31-generic — already documented in `docs/QA_GUIDE.md` | major 8 has to be exercised in CI or on an older kernel; nothing in the change is version-specific |
| 38 | § 4.2 item 1: give `verify()` a `ReadOnlyClient` wrapped around the admin connection's client so the read-back type-checks | nothing to do — since § 3.2 the destination read-back already runs `source::stream_out` on its own `SourcePool`, which now hands out `ReadOnlyClient` | the plan was written before the pool existed; the destination never reads through the admin client any more | none — the read-back is read-only by the same type as the source |
| 39 | `read_only_client_has_no_write_methods` asserts the guard is invoked by counting through a `#[cfg(test)]` counter | the test asserts on the text of the `impl ReadOnlyClient` block: no `execute`/`copy_in`/`transaction`/`simple_query`/`&Client`/`&self.inner` escape, and every method taking `sql: &str` calls `guard_read_only(sql)?` | every `ReadOnlyClient` method needs a live server, so a counter would never be exercised by a unit test — an unguarded method would pass it silently; the text assertion fails the moment one is added | none — stronger than the counter for the property the plan wanted |
| 40 | (unstated) `analyze_err(ctx, e: tokio_postgres::Error)` | `analyze_err(ctx, e: BackupError)` | the read methods return `rb_core::Result`, so a call site's `map_err` now receives the crate error; `phase_src` takes `impl Into<anyhow::Error>` so the cause chain is unchanged | none — 21 call sites, same message and phase |
| 41 | (unstated) rb-postgres dependencies | `anyhow` added to `crates/rb-postgres/Cargo.toml` | the refusal the plan specifies is `BackupError::Other(anyhow::anyhow!(…))` and the crate did not depend on anyhow (rb-core did) | none — workspace dependency, already in the lock file |
| 37 | the filesystem atime row names --allow-atime-updates | it describes the opt-in without naming the flag; § 4.5 writes the name when the flag exists | scripts/help_parity.sh scans every operator document for flags the CLI does not accept, and gates.sh failed on the not-yet-implemented name | § 4.5 must replace the two placeholders in docs/IMMUTABILITY.md with the real flag name |

## 9. Blockers and open questions

- No user-deferred question — the user adopted every recommended default at the clarification gate (D11-D27).
- `UNVERIFIED` external facts (resolve in the owning sub-phase, record the outcome in §8): ~~R5 PostGIS image tags per major~~ **RESOLVED § 1.6**: all nine tags exist (`10-2.5 11-3.3 12-3.4 13-3.5 14-3.5 15-3.5 16-3.5 17-3.5 18-3.6`); ~~R6 PostGIS `spatial_ref_sys` extcondition~~ **RESOLVED § 1.6**: read from `pg_extension.extcondition` at run time, PostGIS registers a multi-line `WHERE NOT (…)` and M-PG-GIS-04 round-trips srid 990001; ~~R7 `tokio-postgres 0.7.x` keepalive setters~~ **RESOLVED § 3.2**: the locked 0.7.18 exposes all four (`keepalives`, `keepalives_idle`, `keepalives_interval`, `keepalives_retries`); ~~R8 MinIO policy action names~~ **RESOLVED § 4.7**: MinIO rejects `s3:GetObjectAcl` as an unsupported action and needs no `s3:GetBucketLocation`; the working set is `s3:ListBucket`, `s3:GetBucketPolicy`, `s3:GetBucketVersioning`, `s3:GetObject`, `s3:GetObjectTagging` (AWS additionally takes `s3:GetObjectAcl`); ~~R9 `mongod --profile 0 --slowms 0` JSON logging~~ **RESOLVED § 4.7**: identical `"c":"COMMAND"` + `attr.command` + `attr.appName` shape on 4.4, 6.0 and 7.0 (8 unrunnable on this kernel).
- Phase 1 § 1.3 status of the § 1.2 FAIL list (runs on 16, 10, 12, 18):
  - M-PG-VIEW-04, M-PG-VIEW-05, M-PG-MV-05 — **FIXED** (view/matview `reloptions` captured and emitted).
  - M-PG-IDX-18 — **FIXED** (index comments were captured but never emitted; found by the read-back).
  - M-PG-REF-15 — **FIXED** (the PostgreSQL 18 named-NOT-NULL probe now compares with the exact
    server-default name instead of a `_not_null` suffix, so `ref_named_not_null` is refused).
  - M-PG-EXT-07 — **FIXED** in § 1.4 (extension configuration tables are captured, streamed with
    their condition and restored). `ORACLE-DATA` and `OVERWRITE` went green with it.
  - A harness defect found while closing § 1.4: `run_transfer`'s `source-only` mode discarded the
    source's exit code, so all 14 `M-PG-REF-*` rows failed with "the source produced a plan for a
    refused object". Fixed (§ 8 row 4); `bash e2e/postgres_matrix.sh 16` is now 109 pass, 0 fail.
  - `NOTEMP` — **partially closed by § 3.3**. The fingerprint now folds an order-independent
    commitment and no longer sorts: temporary files per case on 16 went from 12 to 6. The rest is
    the *data* stream's `ORDER BY (ROW(cols)::text) COLLATE "C"`, which the destination read-back
    depends on — it reproduces the source's per-item BLAKE3 by re-running the same ordered query,
    and a partitioned parent is read in a different physical order on the two sides. Removing it
    needs the read-back to compare an order-independent commitment instead of the streamed bytes,
    which is a transport/verification change no sub-phase of this plan owns. The runner keeps
    reporting `NOTEMP` as a SKIP (`RB_PG_NOTEMP_MODE=strict` enforces it today).
- Phase 1 § 1.2 FAIL rows, input for § 1.3 (first run of the new runner, `bash e2e/postgres_matrix.sh 16`):
  - **M-PG-VIEW-04, M-PG-VIEW-05, M-PG-MV-05** — `find_unsupported_objects` refuses any view or
    materialized view carrying `reloptions`, so the whole run is refused at Analyze:
    `view options such as WITH CHECK OPTION or security_barrier (3): mx.m_mv_05, mx.v_view_04,
    mx.v_view_05`. Until this is fixed no other row can be evaluated — the transfer never starts.
    The matrix expects these three rows to round-trip, so § 1.3 implements view/matview relation
    options rather than moving the rows to `refused before transfer`.
  - Every other row is still unevaluated: the run aborts before the oracles.
- **Privileged e2e cannot run in this session**: `sudo -n true` answers `sudo: è necessaria una
  password` on this host, so every script that needs root — `e2e/filesystem_netns_test.sh`
  (T-FS-OWN, T-FS-IMMUT, and T-FS-ATIME case (d) added in § 4.5) and `e2e/filesystem_matrix.sh`
  (§ 5.3) — is written and lint-clean but unexecuted here. Phase 5 must run them on a host with
  passwordless sudo, or in CI. Where a non-privileged proof of the same behaviour exists it is
  recorded in §7 (§ 4.5 proved both atime polarities against the root-owned, world-readable
  `/etc/skel`).
- § 1.7 CI evidence is **pending**: the workflow is committed locally only (this session never pushes,
  per the user's instruction), so `postgres-postgis` and `postgres-postgis-cross-version` have no run
  URL yet. Record it in §7 after the user pushes `dev`.
- Phase 6 § 6.1 will append the `MISMATCH` list that § 6.2 must fix.

## 10. Do-not-repeat

- XOR as the commutative row commitment — rejected at design time: `{A,A,C}` and `{B,B,C}` collide. Use the wrapping `u128` sum plus exact count (D19).
- Exported snapshot (`pg_export_snapshot`) for source consistency — rejected (D18): a long-held snapshot blocks vacuum on the source, and the fingerprint audit already fails the run on drift.
- `#[ignore]` for privilege- or service-dependent unit tests — not used in this repo; gate on an env var and print the skip reason instead.

## 11. Progress board

Whole-plan status at a glance. Updated when a unit closes; never allowed to
disagree with §1 and §4.

### Phases

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Harness foundation | phase_01.md | `DONE` | 0.1-0.5 closed; harness additive, nothing wired into gates.sh yet |
| 1 — PostgreSQL fidelity matrix | phase_02.md | `DONE` | 1.1-1.8 closed; plain matrix 1396 pass / 0 fail and PostGIS 901 pass / 0 fail |
| 2 — Restore validation | phase_03.md | `DONE` | 2.1-2.4 closed; matrix 0 fail on 10,12,14,16,18 and every cross pair; full_matrix green after the CONSTR-STATE clone fix |
| 3 — PostgreSQL latent defects | phase_04.md | `DONE` | 3.1-3.6 closed; pooled connections, timeouts, fingerprint v2, RSS proof, abort/race proofs (one product fix: abandon) |
| 4 — Source immutability guardrails | phase_05.md | `DONE` | 4.1-4.9 closed: IMMUTABILITY.md, typed read-only clients in all three database modules, the filesystem atime guard, the lint as a gate, the three least-privilege runs proven from the servers' own logs, a panic-safe audit, and the README source-safety section. T-FS-ATIME stays PARTIAL until a host with passwordless sudo runs the privileged case (§ 9) |
| 5 — Filesystem deep verification | phase_06.md | `TODO` | 5.1-5.6 |
| 6 — Documentation audit and parity gate | phase_07.md | `TODO` | 6.1-6.5 |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`
A `SKIPPED` sub-phase or phase keeps its row and carries the reason.

### Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| T-HARN-INV | script | `PASS` | `scripts/env_inventory.sh --check-count 40` exits 0 — 49 distinct names including `RUST_BACKUP_EXTENSION_VERSION` (§ 0.3, re-run in § 1.5) |
| T-HARN-LINT | script | `PASS` | exits 0 on the tree; `--selftest` detects the injected hit (§ 0.4); wired into gates.sh in § 4.6 |
| T-PG-FIX | e2e | `PASS` | all fixtures load with ON_ERROR_STOP on 10, 12, 16, 18 (§ 1.1) |
| T-PG-ORACLE | e2e | `PASS` | 10-18 plus `10:18 12:16 14:17 16:18` => 1396 pass, 0 fail, 149 skip (§ 1.2, 1.3) |
| T-PG-REFUSE | e2e | `PASS` | all 14 applicable `M-PG-REF-*` rows (15 on 18) refused before transfer, no destination database, no COPY (§ 1.2) |
| T-PG-EXTCFG | e2e | `PASS` | `rbtest_cfg` k=1000 (M-PG-EXT-07) and `spatial_ref_sys` 990001 (M-PG-GIS-04) both round-trip (§ 1.4, 1.6) |
| T-PG-EXTVER | e2e | `PASS` | M-PG-EXT-08 green on 16: 1.0 refused at preflight with no database created, `--extension-version default` restores and prints the deviation line (§ 1.5) |
| T-PG-GIS | e2e | `PASS` | PostGIS matrix `0 fail` on 10-18 (901 pass) and on `12:16` (114 pass, GIS-06 green) (§ 1.6) |
| T-PG-ROWS | e2e | `PASS` | no `row-count verification skipped` warning and the destination prints `rows verified:` / `constraints:` (§ 2.1, 2.3) |
| T-PG-ROWS-NEG | e2e | `PASS` | `fault_matrix.sh postgres` case `pg-row-count-mismatch`: exit 5, `source counted` in the destination log, database removed (§ 2.1) |
| T-PG-CONSTR | e2e | `PASS` | `convalidated`/deferrable state identical; NOT VALID FK stays NOT VALID with violating row (§ 2.2) |
| T-PG-CONN | e2e | `PASS` | peak 1 source connection (boot and database coincide on a single-database run), budget 3 — green on 10..18 (§ 3.2) |
| T-PG-TIMEOUT | e2e | `PASS` | `pg-lock-timeout`: fails in 30 s with the server message, tagged [Analyze], and the same run succeeds once the lock is released (§ 3.2) |
| T-PG-FP2 | e2e | `PARTIAL` | the fingerprint no longer sorts or spills (12 => 6 temp files per case); the remaining spill is the data stream sort, see § 9 (§ 3.3) |
| T-PG-RSS | e2e | `PASS` | 2 GiB table: peaks 14056 KiB (source) and 22592 KiB (destination), cap 262144 KiB (§ 3.4) |
| T-PG-ABORT | e2e | `PASS` | destination kill, source kill and refused plan: correct exits, failure-path audit ran, created databases dropped (§ 3.5) |
| T-PG-WRITER | e2e | `PASS` | `pg-concurrent-writer`: exit 6, SOURCE-IMMUTABILITY VIOLATION, and the uncertified database is removed (§ 3.5) |
| T-IMMUT-PG-LP | e2e | `PASS` | a full overwrite transfer runs as `rb_ro` on 10, 13, 14, 16 and 18 (both grant recipes, no extra grant needed); the server log for that role is read-only and the connection budget holds (§ 4.7) |
| T-IMMUT-MONGO-LP | e2e | `PASS` | source runs as a `read`+`viewUser`/`viewRole` user on 4, 6 and 7; the mongod log shows 110 commands from `appName=rust-backup`, all reads; 8 unrunnable on this kernel (§ 4.7, § 9) |
| T-IMMUT-S3-LP | e2e | `PASS` | source runs under the 5-action MinIO read policy with the anonymous grant removed; `mc admin trace` shows 25 S3 APIs, all reads, and the same credential is refused a write (§ 4.7) |
| T-IMMUT-LINT | gate | `PASS` | `scripts/gates.sh` runs `scripts/source_readonly_lint.sh` and its `--selftest`: 12 source-side files clean, injected write detected (§ 4.6) |
| T-IMMUT-PANIC | unit | `PASS` | both in `crates/rb-core/tests/session_test.rs`: the panic is reported as an error naming it, the after-audit ran (2 fingerprints), and drift on that path is `SourceMutated` (§ 4.8) |
| T-FS-ATIME | e2e | `PARTIAL` | case (d) written in `e2e/filesystem_netns_test.sh`; the privileged run needs passwordless sudo (§ 9). Both polarities proven without root against `/etc/skel`: refused with the documented message and no atime change; accepted with the flag, one warning, formal verification (§ 4.5) |
| T-FS-SPECIAL | e2e | `TODO` | FIFO + devices round-trip as root; socket refused; devices refused without CAP_MKNOD (§ 5.1) |
| T-FS-MATRIX | e2e | `TODO` | `filesystem_matrix.sh` prints `MATRIX FS: 30 pass, 0 fail` (§ 5.3) |
| T-FS-TOCTOU | unit | `TODO` | `a_planted_symlink_directory_is_refused_during_restore`, `a_symlink_destination_root_is_refused` (§ 5.4) |
| T-DOCS-PARITY | gate | `TODO` | `scripts/docs_parity.sh` exits 0 inside `gates.sh` (§ 6.1-6.4) |
| T-PG-MATRIX, T-PG-IMMUT, T-PG-INTROSPECT | e2e (existing) | `TODO` (re-run per phase) | regression guards; must stay green after every postgres sub-phase |
| T-FS-OWN, T-FS-IMMUT, T-FS-ENOSPC | e2e (existing) | `TODO` (re-run per phase) | regression guards for the filesystem module |
| T-E2E0, T-SESSION-2, T-FAULT, T-MONGO-MATRIX, T-MONGO-IMMUT, T-S3-MINIO, T-S3-IMMUT | e2e (existing) | `TODO` (re-run per phase) | cross-module regression guards |

### Docs

One row per phase README sub-phase, so a partially documented feature is visible;
other docs get their own rows.

| Doc | Phase | Status | Notes |
|-----|-------|--------|-------|
| README.md | 0 | `DONE (no user-visible change)` | no user-visible change — verify accuracy (§ 0.5) |
| README.md | 1 | `DONE` | extension configuration tables, the version refusal message, `--extension-version default` example and the reported deviation, PostGIS e2e command (§ 1.8) |
| README.md | 2 | `DONE` | the `rows verified:` / `constraints:` lines with their meaning, and the row-count refusal with its remedy (§ 2.4) |
| README.md | 3 | `DONE` | lock-timeout message and remedy, connections and keepalives, the two fingerprint read passes, exit 6 (§ 3.6) |
| README.md | 4 | `DONE` | "Source safety": the read-only source and the audit on every exit path, the least-privilege account per backend, the writable-role warning, the atime rule with its refusal message and both remedies (§ 4.9) |
| README.md | 5 | `TODO` | filesystem entries preserved/refused, capabilities, troubleshooting (§ 5.6) |
| README.md | 6 | `TODO` | final full read (§ 6.5) |
| docs/testing/POSTGRES_MATRIX.md, FILESYSTEM_MATRIX.md | 0, 1, 5, 6 | `IN_PROGRESS` | created § 0.1 (106 + 32 rows); `Fixture file` column filled § 1.1/1.6/5.2; reconciled § 6.3 |
| docs/modules/POSTGRES.md | 1, 2, 3, 6 | `IN_PROGRESS` | supported objects, "Extension versions", the printed proof, "Runtime model and guardrails", "Connections, timeouts and keepalives", "The source fingerprint" and "Failure behaviour" written (§ 1.3-1.5, 2.1-2.3, 3.1-3.5); Limits reconciled in § 6.3 |
| docs/modules/FILESYSTEM.md | 4, 5, 6 | `IN_PROGRESS` | the atime rule, its message and its remedies written, plus the guarantees-table row (§ 4.5); special files, the TOCTOU guarantee and the fingerprint cost land in phase 5 |
| docs/modules/MONGODB.md | 4, 6 | `IN_PROGRESS` | "Immutability" rewritten for the typed client, the command allowlist and the pinned read settings (§ 4.3); reconciled § 6.3 |
| docs/modules/README.md | 4, 6 | `IN_PROGRESS` | the module-author checklist states that `fingerprint` runs after a failed or panicked run too (§ 4.8); reconciled § 6.3 |
| docs/modules/S3.md | 4, 6 | `IN_PROGRESS` | "Immutability" names `ReadOnlyS3`, the eight read operations and the one-file read side (§ 4.4); reconciled § 6.3 |
| docs/IMMUTABILITY.md | 4, 6 | `IN_PROGRESS` | created § 4.1; PostgreSQL (§ 4.2), MongoDB (§ 4.3), S3 (§ 4.4) and the filesystem atime row (§ 4.5) carry their guard class and evidence IDs; the MongoDB and S3 least-privilege recipes are final and every "evidence lands in" marker is gone (§ 4.7); the fingerprint contract states the panic path (§ 4.8); reconciled § 6.3 |
| docs/usage/03-postgres.md, 05-filesystem.md, 07-sessioni-yaml.md, 10-variabili-ambiente.md, 11-codici-uscita.md | 1, 2, 4, 5, 6 | `IN_PROGRESS` | `--extension-version` rows, the configuration-table paragraph, "Cosa stampa la verifica" and the exit-code note landed (§ 1.4-1.5, 2.1-2.3); test hook and filesystem rows still owed; full audit § 6.2 |
| docs/QA_GUIDE.md, e2e/README.md, e2e/fixtures/postgres/README.md | 0, 1, 3, 5, 6 | `IN_PROGRESS` | § 0.1 matrix links; § 0.2 helper table + fixture naming rule; scripts/jobs/gates still to reconcile in § 6 |
| CLAUDE.md | 4, 6 | `IN_PROGRESS` | the gate line now lists `crate_invariants.sh`, `source_readonly_lint.sh` (+ `--selftest`) and `help_parity.sh` (§ 4.6); `docs_parity.sh` is added in § 6.1 |

### Audits

One row per verify report, so the audit history is visible from the entry point.
Findings themselves live in `verify/index.md`.

| Report | Date | Verdict | Open findings |
|--------|------|---------|---------------|
| <none yet> | — | — | — |
