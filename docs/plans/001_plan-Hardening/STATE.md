# Hardening — Implementation State

> **READ THIS FILE FIRST at the start of every session, before any other plan
> file. OPEN a unit in §1 before touching code; CLOSE it after the gates pass.**
> **Last updated:** 2026-09-16 | **By:** `agent:opus` | **Session:** None-09-16

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
- **ID:** 1.1
- **Status:** `none`
- **Intent:** write the PostgreSQL fixture files (one group per matrix row) under e2e/fixtures/postgres/ and fill the matrix `Fixture file` column
- **Phase:** 1 — PostgreSQL fidelity matrix (`phase_02.md`)
- **Next action:** open sub-phase 1.1 in `phase_02.md`: create 00_roles.sql, 10_tables*.sql, 20_sequences.sql, 30_views.sql, 40_matviews.sql, 50_indexes*.sql, 60_constraints*.sql, 70_extensions.sql, 71_rbtest_extension.sh, 80_postgis.sql and refusals/*.sql; load them on 10, 12, 16, 18 (T-PG-FIX)
- **Assigned:** `agent:opus`
- **Repo state:** branch `dev` | working tree: 0.1-0.4 written, uncommitted | last commit `1549128`

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
| 1 | sub-phase | 0.1 | `agent:opus` | matrix catalogues created: 106 `M-PG-*` rows and 32 `M-FS-*` rows, with fixture file, expected outcome and oracle per row; link lines added | docs/testing/POSTGRES_MATRIX.md, docs/testing/FILESYSTEM_MATRIX.md, docs/QA_GUIDE.md, e2e/README.md | gates.sh PASS | uncommitted |
| 2 | sub-phase | 0.2 | `agent:opus` | `RB_PG_LOG_ARGS` + 8 `rb_pg_*` helpers appended to `e2e/lib.sh`; fixture directory with naming rule and oracle ignore list; helper table in `e2e/README.md`; `pg_dump` flags excluded from the ghost-flag scan | e2e/lib.sh, e2e/fixtures/postgres/{README.md,oracle_ignore.txt}, e2e/README.md, scripts/help_parity.sh | gates.sh PASS; bash -n PASS; ShellCheck v0.11.0 PASS; relay_smoke.sh PASS (5/0); helpers smoke-tested against postgres:16-alpine | uncommitted |
| 3 | sub-phase | 0.3 | `agent:opus` | `scripts/env_inventory.sh`: TSV of every (command, flag, env, default, help) pair from 14 clap help surfaces plus direct `env::var` reads, split `code` vs `test`; `--check-count` guard | scripts/env_inventory.sh | gates.sh PASS; ShellCheck PASS; bash -n PASS; T-HARN-INV PASS (48 distinct RUST_BACKUP_* >= 40) | uncommitted |
| 4 | sub-phase | 0.4 | `agent:opus` | `scripts/source_readonly_lint.sh`: per-module forbidden call-shape table over source-side files, `--selftest`; not yet wired into gates.sh (§ 4.6) | scripts/source_readonly_lint.sh | gates.sh PASS; ShellCheck PASS; T-HARN-LINT PASS (9 files clean, selftest detects the injected write) | uncommitted |
| 5 | sub-phase | 0.5 | `agent:opus` | README verified, no change — phase 0 shipped no user-visible behaviour; the new harness files are reachable through the QA guide and e2e/README links README already carries | README.md (unchanged) | gates.sh PASS | uncommitted |

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

## 9. Blockers and open questions

- No user-deferred question — the user adopted every recommended default at the clarification gate (D11-D27).
- `UNVERIFIED` external facts (resolve in the owning sub-phase, record the outcome in §8): R5 PostGIS image tags per major (default table in `overview.md`; missing => SKIP) — § 1.2/1.6/1.7; R6 PostGIS `spatial_ref_sys` extcondition (read from catalog; fixture srid 990001) — § 1.4; R7 `tokio-postgres 0.7.x` keepalive setters (fallback `keepalives` + `keepalives_idle`) — § 3.2; R8 MinIO policy action names (fallback `s3:Get*` + `s3:ListBucket`) — § 4.7; R9 `mongod --profile 0 --slowms 0` JSON logging on 4.4..8 — § 4.7.
- Phase 1 § 1.2 will append here the list of FAIL row IDs that § 1.3 must fix; phase 6 § 6.1 will append the `MISMATCH` list that § 6.2 must fix.

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
| 1 — PostgreSQL fidelity matrix | phase_02.md | `TODO` | 1.1-1.8 |
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
| T-HARN-INV | script | `PASS` | `scripts/env_inventory.sh --check-count 40` exits 0 — 48 distinct names (§ 0.3); re-run after § 1.5 for RUST_BACKUP_EXTENSION_VERSION |
| T-HARN-LINT | script | `PASS` | exits 0 on the tree; `--selftest` detects the injected hit (§ 0.4); wired into gates.sh in § 4.6 |
| T-PG-FIX | e2e | `TODO` | all fixtures load on 10, 12, 16, 18 with ON_ERROR_STOP (§ 1.1) |
| T-PG-ORACLE | e2e | `TODO` | `postgres_matrix.sh M` prints `0 fail`; pg_dump + counts diffs empty (§ 1.2, 1.3) |
| T-PG-REFUSE | e2e | `TODO` | every `M-PG-REF-*` refused before transfer, no destination database (§ 1.2) |
| T-PG-EXTCFG | e2e | `TODO` | `rbtest_cfg` k=1000 and `spatial_ref_sys` 990001 round-trip (§ 1.4) |
| T-PG-EXTVER | e2e | `TODO` | missing extension version refused; `--extension-version default` restores with deviation line (§ 1.5) |
| T-PG-GIS | e2e | `TODO` | PostGIS matrix `0 fail` on every resolvable major and `12:16` (§ 1.6) |
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
| README.md | 1 | `DONE (no user-visible change)` | PostgreSQL objects, `--extension-version`, env table row, limitations (§ 1.8) |
| README.md | 2 | `DONE (no user-visible change)` | verification block, troubleshooting row-count message (§ 2.4) |
| README.md | 3 | `DONE (no user-visible change)` | lock timeout, connections, fingerprint cost (§ 3.6) |
| README.md | 4 | `DONE (no user-visible change)` | source safety section, least-privilege recipes, `--allow-atime-updates` (§ 4.9) |
| README.md | 5 | `DONE (no user-visible change)` | filesystem entries preserved/refused, capabilities, troubleshooting (§ 5.6) |
| README.md | 6 | `DONE (no user-visible change)` | final full read (§ 6.5) |
| docs/testing/POSTGRES_MATRIX.md, FILESYSTEM_MATRIX.md | 0, 1, 5, 6 | `IN_PROGRESS` | created § 0.1 (106 + 32 rows); `Fixture file` column filled § 1.1/1.6/5.2; reconciled § 6.3 |
| docs/modules/POSTGRES.md | 1, 2, 3, 6 | `TODO` | supported objects, extensions, verification, runtime model, Limits |
| docs/modules/FILESYSTEM.md | 4, 5, 6 | `TODO` | atime rule, special files, TOCTOU guarantee, fingerprint cost |
| docs/modules/MONGODB.md, S3.md | 4, 6 | `TODO` | Immutability paragraphs |
| docs/IMMUTABILITY.md | 4, 6 | `TODO` | created § 4.1; evidence markers removed § 4.7; reconciled § 6.3 |
| docs/usage/03-postgres.md, 05-filesystem.md, 07-sessioni-yaml.md, 10-variabili-ambiente.md, 11-codici-uscita.md | 1, 2, 4, 5, 6 | `TODO` | flag/env rows, verification, test hook, exit codes; full audit § 6.2 |
| docs/QA_GUIDE.md, e2e/README.md, e2e/fixtures/postgres/README.md | 0, 1, 3, 5, 6 | `IN_PROGRESS` | § 0.1 matrix links; § 0.2 helper table + fixture naming rule; scripts/jobs/gates still to reconcile in § 6 |
| CLAUDE.md | 4, 6 | `TODO` | gate line gains the two new scripts |

### Audits

One row per verify report, so the audit history is visible from the entry point.
Findings themselves live in `verify/index.md`.

| Report | Date | Verdict | Open findings |
|--------|------|---------|---------------|
| <none yet> | — | — | — |
