# Hardening — Implementation State

> **READ THIS FILE FIRST at the start of every session, before any other plan
> file. OPEN a unit in §1 before touching code; CLOSE it after the gates pass.**
> **Last updated:** 2026-09-16 | **By:** `agent:opus` | **Session:** 2026-09-16

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
- **ID:** 2.1
- **Status:** `none`
- **Intent:** the destination proves it restored every row the source streamed
- **Phase:** phase_03.md (phase 2)
- **Next action:** open § 2.1 in phase_03.md: expected_rows on table and extension_config items, check_expected_rows, RESTORE VERIFIED rows line
- **Assigned:** `agent:opus`
- **Repo state:** branch `dev` | working tree: phase 1 committed | last commit: see §4

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
| 6 | sub-phase | 1.1 | `agent:opus` | 16 fixture files + 15 refusal files + the rbtest installer: one object group per matrix row, in schema `mx` (plus `"Mixed Schema"` and `ext`); matrix `Fixture file` column reconciled | e2e/fixtures/postgres/*.sql, refusals/*.sql, postgis/80_postgis.sql, 71_rbtest_extension.sh, README.md, docs/testing/POSTGRES_MATRIX.md | gates.sh PASS; ShellCheck PASS; T-PG-FIX PASS on 10, 12, 16, 18 (every file loads with ON_ERROR_STOP, gated files skipped, all 15 refusal files load alone) | `c7c4e61` |
| 7 | sub-phase | 1.2 | `agent:opus` | runner v2: fixtures instead of the inline seed, pg_dump + counts oracles, per-row PASS/FAIL/SKIP, refusal loop, registration-aware fail-fast, MATRIX summary; first run on 16 refused at Analyze (3 FAIL rows recorded in §9) | e2e/postgres_matrix.sh, e2e/lib.sh, e2e/fixtures/postgres/*.sql | ShellCheck --severity=warning PASS; resource_hygiene_check PASS; run on 16 reaches per-row evaluation only after § 1.3 (T-PG-ORACLE deferred there) | `c7c4e61` |
| 8 | sub-phase | 1.3 | `agent:opus` | view/matview reloptions + `relispopulated` captured and restored; constraint and index comments emitted; PostgreSQL 18 named-NOT-NULL probe compares the exact default name; 6 new unit tests (77 total in rb-postgres) | crates/rb-postgres/src/{model,introspect,ddl}.rs, docs/modules/POSTGRES.md, e2e/postgres_matrix.sh | gates.sh PASS (77 rb-postgres tests); matrix 16/10/12/18: only M-PG-EXT-07 and its two derived rows fail (§ 1.4), NOTEMP skipped (§ 3.3) | `c7c4e61` |
| 9 | sub-phase | 1.4 | `agent:opus` | extension configuration tables captured, streamed with their condition, restored over a cleared scope; unknown item kinds refused | model.rs, introspect.rs, source.rs, dest.rs, immutability.rs, POSTGRES.md, 03-postgres.md, postgres_matrix.sh | gates.sh PASS; postgres_matrix.sh 16 => 109 pass, 0 fail, 10 skip | `c7c4e61` |
| 10 | sub-phase | 1.5 | `agent:opus` | extension version preflight, --extension-version source|default, deviation reported on RESTORE VERIFIED | lib.rs, ddl.rs, dest.rs, main.rs, 03-postgres.md, 10-variabili-ambiente.md, 07-sessioni-yaml.md, POSTGRES.md, postgres_matrix.sh, env_inventory.sh | gates.sh PASS; postgres_matrix.sh 16 => 110 pass, 0 fail, 9 skip (M-PG-EXT-08 green) | uncommitted |
| 11 | sub-phase | 1.6 | `agent:opus` | PostGIS matrix green on every major and on the cross pair 12:16; extension-owned objects no longer refuse a cluster | introspect.rs, postgres_matrix.sh, lib.sh, POSTGRES_MATRIX.md, e2e/README.md | gates.sh PASS; postgis 10-18 => 901 pass, 0 fail; postgis 16 => 115 pass, 0 fail; postgis 12:16 => 114 pass, 0 fail | `c7c4e61` |
| 12 | sub-phase | 1.7 | `agent:opus` | CI: PostGIS jobs added, PostgreSQL e2e timeouts raised to 80 minutes | .github/workflows/e2e.yml, docs/QA_GUIDE.md | actionlint 1.7.12 clean; gates.sh PASS | `c7c4e61` |
| 13 | sub-phase | 1.8 | `agent:opus` | README documents extension configuration tables, the extension-version policy and the PostGIS e2e invocation | README.md | gates.sh PASS; help_parity PASS | `c7c4e61` |

## 5. Files touched

| Path | What was done | Unit |
|------|---------------|------|
| `docs/testing/POSTGRES_MATRIX.md` | new: 106 rows (TAB 18, SEQ 7, VIEW 9, MV 7, IDX 18, CON 18, EXT 8, GIS 6, REF 15) with the oracle legend | 0.1 |
| `docs/testing/FILESYSTEM_MATRIX.md` | new: 32 rows with the oracle legend | 0.1 |
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

**Failing output (verbatim, trimmed to the error):**
```
<none>
```

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

## 9. Blockers and open questions

- No user-deferred question — the user adopted every recommended default at the clarification gate (D11-D27).
- `UNVERIFIED` external facts (resolve in the owning sub-phase, record the outcome in §8): ~~R5 PostGIS image tags per major~~ **RESOLVED § 1.6**: all nine tags exist (`10-2.5 11-3.3 12-3.4 13-3.5 14-3.5 15-3.5 16-3.5 17-3.5 18-3.6`); ~~R6 PostGIS `spatial_ref_sys` extcondition~~ **RESOLVED § 1.6**: read from `pg_extension.extcondition` at run time, PostGIS registers a multi-line `WHERE NOT (…)` and M-PG-GIS-04 round-trips srid 990001; R7 `tokio-postgres 0.7.x` keepalive setters (fallback `keepalives` + `keepalives_idle`) — § 3.2; R8 MinIO policy action names (fallback `s3:Get*` + `s3:ListBucket`) — § 4.7; R9 `mongod --profile 0 --slowms 0` JSON logging on 4.4..8 — § 4.7.
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
  - `NOTEMP` — the source still sorts every COPY and spills with the fixture's 4 MB `work_mem`.
    The runner reports it as a SKIP until phase 3 § 3.3 lands the commutative fingerprint
    (`RB_PG_NOTEMP_MODE=strict` enforces it today).
- Phase 1 § 1.2 FAIL rows, input for § 1.3 (first run of the new runner, `bash e2e/postgres_matrix.sh 16`):
  - **M-PG-VIEW-04, M-PG-VIEW-05, M-PG-MV-05** — `find_unsupported_objects` refuses any view or
    materialized view carrying `reloptions`, so the whole run is refused at Analyze:
    `view options such as WITH CHECK OPTION or security_barrier (3): mx.m_mv_05, mx.v_view_04,
    mx.v_view_05`. Until this is fixed no other row can be evaluated — the transfer never starts.
    The matrix expects these three rows to round-trip, so § 1.3 implements view/matview relation
    options rather than moving the rows to `refused before transfer`.
  - Every other row is still unevaluated: the run aborts before the oracles.
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
| 2 — Restore validation | phase_03.md | `TODO` | 2.1-2.4 |
| 3 — PostgreSQL latent defects | phase_04.md | `TODO` | 3.1-3.6 |
| 4 — Source immutability guardrails | phase_05.md | `TODO` | 4.1-4.9 |
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
| T-PG-ROWS | e2e | `TODO` | destination prints `rows verified:` and `constraints:`; no `skipped` warning (§ 2.1, 2.3) |
| T-PG-ROWS-NEG | e2e | `TODO` | `RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA=1` => Integrity exit code, database removed (§ 2.1) |
| T-PG-CONSTR | e2e | `TODO` | `convalidated`/deferrable state identical; NOT VALID FK stays NOT VALID with violating row (§ 2.2) |
| T-PG-CONN | e2e | `TODO` | `<= 3` source connections per single-database run (§ 3.2) |
| T-PG-TIMEOUT | e2e | `TODO` | exclusively locked table => phase-tagged `lock timeout` failure within 90 s (§ 3.2) |
| T-PG-FP2 | e2e | `TODO` | zero `temporary file` lines on the source during the matrix run (§ 3.3) |
| T-PG-RSS | e2e | `TODO` | 2 GiB table, peak RSS < 256 MiB on both sides (§ 3.4) |
| T-PG-ABORT | e2e | `TODO` | destination kill, source kill, plan rejected: correct exits, after-audit ran, cleanup done (§ 3.5) |
| T-PG-WRITER | e2e | `TODO` | concurrent writer => `SourceMutated` exit code (§ 3.5) |
| T-IMMUT-PG-LP | e2e | `TODO` | source runs as `rb_ro`; log allowlist passes on 10, 13, 14, 18 (§ 4.7) |
| T-IMMUT-MONGO-LP | e2e | `TODO` | source runs as `read` role; no write command in mongod log on 4 and 8 (§ 4.7) |
| T-IMMUT-S3-LP | e2e | `TODO` | source runs with Get/List-only MinIO policy (§ 4.7) |
| T-IMMUT-LINT | gate | `TODO` | readonly lint inside `gates.sh` (§ 4.6) |
| T-IMMUT-PANIC | unit | `TODO` | `panic_in_stream_out_still_runs_the_after_audit`, `panic_with_mutated_fingerprint_reports_source_mutated` (§ 4.8) |
| T-FS-ATIME | e2e | `TODO` | non-owner run refused without flag, succeeds with `--allow-atime-updates` (§ 4.5) |
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
| README.md | 2 | `TODO` | verification block, troubleshooting row-count message (§ 2.4) |
| README.md | 3 | `TODO` | lock timeout, connections, fingerprint cost (§ 3.6) |
| README.md | 4 | `TODO` | source safety section, least-privilege recipes, `--allow-atime-updates` (§ 4.9) |
| README.md | 5 | `TODO` | filesystem entries preserved/refused, capabilities, troubleshooting (§ 5.6) |
| README.md | 6 | `TODO` | final full read (§ 6.5) |
| docs/testing/POSTGRES_MATRIX.md, FILESYSTEM_MATRIX.md | 0, 1, 5, 6 | `IN_PROGRESS` | created § 0.1 (106 + 32 rows); `Fixture file` column filled § 1.1/1.6/5.2; reconciled § 6.3 |
| docs/modules/POSTGRES.md | 1, 2, 3, 6 | `IN_PROGRESS` | supported objects and "Extension versions" written (§ 1.3-1.5); verification, runtime model and Limits still owed by phases 2, 3 and 6 |
| docs/modules/FILESYSTEM.md | 4, 5, 6 | `TODO` | atime rule, special files, TOCTOU guarantee, fingerprint cost |
| docs/modules/MONGODB.md, S3.md | 4, 6 | `TODO` | Immutability paragraphs |
| docs/IMMUTABILITY.md | 4, 6 | `TODO` | created § 4.1; evidence markers removed § 4.7; reconciled § 6.3 |
| docs/usage/03-postgres.md, 05-filesystem.md, 07-sessioni-yaml.md, 10-variabili-ambiente.md, 11-codici-uscita.md | 1, 2, 4, 5, 6 | `IN_PROGRESS` | `--extension-version` flag/env/YAML rows and the configuration-table paragraph landed (§ 1.4-1.5); verification, test hook and exit codes still owed; full audit § 6.2 |
| docs/QA_GUIDE.md, e2e/README.md, e2e/fixtures/postgres/README.md | 0, 1, 3, 5, 6 | `IN_PROGRESS` | § 0.1 matrix links; § 0.2 helper table + fixture naming rule; scripts/jobs/gates still to reconcile in § 6 |
| CLAUDE.md | 4, 6 | `TODO` | gate line gains the two new scripts |

### Audits

One row per verify report, so the audit history is visible from the entry point.
Findings themselves live in `verify/index.md`.

| Report | Date | Verdict | Open findings |
|--------|------|---------|---------------|
| <none yet> | — | — | — |
