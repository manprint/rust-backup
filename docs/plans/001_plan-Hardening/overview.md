# Hardening — Plan Overview

> **Status:** planning | **Authored:** 2026-09-16 by `agent:opus`
> **Folder:** `docs/plans/001_plan-Hardening/`
> **Executing this plan? Read [STATE.md](STATE.md) FIRST** — it is the only
> execution-state file: live position, progress board, environment, in-flight
> work, next action. Open a unit in it before touching code, close it after.

## Goal

Harden the PostgreSQL and filesystem backup/restore paths and the source-immutability
guardrails of every module. Deliverables: a formal PostgreSQL version x object-kind test
matrix (10..=18, same-major and cross-major, PostGIS included) with an independent external
oracle; restore validation that compares row counts and constraint state between source and
destination from the plan alone; a latent-defect pass on consistency, connections, timeouts,
memory and abort paths; typed read-only clients plus least-privilege end-to-end proof that the
source is never written; filesystem coverage for special files, permissions, ownership and
links; a row-by-row audit of every documentation table with a parity gate. End state: every
matrix row passes or is refused before any transfer, and `bash scripts/gates.sh` plus the
e2e scripts named in the verification summary are green.

```
Reference scenario
  for M in 10..=18 (postgres:M-alpine) and every postgis/postgis:M-* tag available:
    bash e2e/postgres_matrix.sh M            # same-major
  bash e2e/postgres_matrix.sh 10:18 12:16 14:17 16:18   # cross-major (+ 12:16 on PostGIS)
  -> every M-PG-* row prints PASS (or REFUSED-BEFORE-TRANSFER for refusal rows)
  -> psql oracle: per-relation count(*) and row md5 equal on both sides; pg_dump --schema-only
     (destination binary) diff empty after normalization
  -> source log (log_statement=all): only SELECT/WITH/SHOW/COPY ... TO STDOUT from rb_ro;
     <= 3 connections; zero "temporary file" lines; fingerprint before == after
  sudo bash e2e/filesystem_matrix.sh
  -> FIFO/setuid/setgid/sticky/0000/mixed-owner/symlink/hardlink tree round-trips byte- and
     metadata-identical; socket / non-UTF-8 / depth>1024 refused before any transfer
  bash scripts/gates.sh  (includes source_readonly_lint.sh and docs_parity.sh)  -> green
```

## Design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | Single agent `agent:opus` for every stage; final review is self-review | Every sub-phase `Model` tag reads `agent:opus`; review gates labelled self-review |
| **D2** | Plan folder `docs/plans/001_plan-Hardening/`; plan mode writes no production code | `execute` mode implements sub-phase by sub-phase |
| **D3** | Matrix rows carry stable IDs `M-PG-<KIND>-<nn>` / `M-FS-<nn>` in `docs/testing/*.md`; fixtures are SQL files in `e2e/fixtures/postgres/` gated by major through the filename suffix `.ge<major>` | Runner prints one PASS/FAIL/SKIP line per row ID; fixture objects are named after their row ID |
| **D4** | Harness stays real-binary + `docker run` (existing pattern); no docker compose, no mocks in e2e | New scripts copy `e2e/postgres_matrix.sh` / `e2e/filesystem_netns_test.sh` structure |
| **D5** | Restore validation stays self-contained: the plan carries expectations (`expected_rows`, constraint state); no source round-trip from the destination | Counts are computed by the source during `analyze()` |
| **D6** | Views compared by definition; tables and populated materialized views by row count; sequences by `last_value`/`is_called`; indexes and constraints by catalog definition (existing `verify_catalog`) | Unpopulated matviews are not refreshed and carry no count |
| **D7** | No new crate; no change to `rb_core::module` traits; module payload changes are additive with serde defaults | I-MODULAR untouched; older plans still deserialize |
| **D8** | Documentation languages preserved: Italian under `docs/usage/`, English in `docs/modules/`, `docs/*.md`, `README.md` | Docs sub-phases edit, never translate |
| **D9** | WIP commits off (no flag given) | `STATE.md` §3 `WIP commits: off`; ledger `Commit` column reads `uncommitted` |
| **D10** | `BackupPlan.format_version` is not bumped; unknown item kinds and entry kinds are rejected by the destination at validate (fail closed) | Backward compatibility = explicit refusal, never silent skip |
| **D11 (user, Q1)** | Scope = tables, views, matviews, indexes, sequences, extensions, constraints. Kinds refused today (enum/domain/composite types, triggers, RLS, rules, publications, large objects, collations, event triggers, foreign tables, column/default ACLs) stay refused | Matrix has one refusal row per kind asserting refusal fires before any transfer |
| **D12 (user, Q2)** | Extension config tables (`pg_extension.extconfig`/`extcondition`) are captured as data items and restored after `CREATE EXTENSION`; included in the fingerprint | PostGIS custom `spatial_ref_sys` rows round-trip; destination deletes rows matching the condition (or truncates when no condition) before COPY |
| **D13 (user, Q3)** | Destination preflight checks `pg_available_extension_versions`; pinned version missing => refuse; new opt-in `--extension-version default` installs the destination default and records the deviation | New CLI flag + env var + session.yml key; `create_extension` omits `VERSION` under `default` |
| **D14 (user, Q4)** | PostGIS CI job for every major with an available `postgis/postgis` tag plus one cross-major pair (`12:16`) | New `postgres-postgis` job; tags resolved at run time, missing tag => skip (exit 77) |
| **D15 (user, Q5)** | e2e may use `pg_dump --schema-only` executed inside the destination container (same binary for both sides) and psql `count(*)`/md5 as external oracle; product stays pg_dump-free | Normalization allowlist `e2e/fixtures/postgres/oracle_ignore.txt` |
| **D16 (user, Q6)** | `analyze()` runs `SELECT count(*) FROM ONLY <table>` per table item and per populated matview; stored in `PlanItem.meta.expected_rows` (tables) and payload (matviews); no wire change | Destination compares the `COPY FROM` row count and the post-`REFRESH` count |
| **D17 (user, Q7)** | Row-count mismatch is `BackupError::Integrity`; restore FAILED | Exit-code doc extended |
| **D18 (user, Q8)** | Per-statement reads stay (no exported snapshot; a long-held snapshot would block vacuum on the source); connections pooled per database; read-only connections set `lock_timeout=30s`, `statement_timeout=0`, `idle_in_transaction_session_timeout=0`, TCP keepalives | `introspect_cluster`/`fingerprint`/`stream_out` share one pool; <= 3 source connections per single-database run |
| **D19 (user, Q9)** | Fingerprint v2: commutative row commitment (`count`, wrapping `u128` sum of per-row md5) instead of the C-collated server sort | No `pgsql_tmp` spill on the source; audit item PG9b closed; two passes remain |
| **D20 (user, Q10)** | Typed read-only clients per module (`ReadOnlyClient`, `ReadOnlyDatabase`, `ReadOnlyS3`) + SQL/command allowlist + static lint in gates + warning when the source role can write + least-privilege e2e roles with server-log assertions + panic-safe after-audit in `rb-core` | Source code paths cannot reach a write method; a bug that tries is a compile error or a guard error |
| **D21 (user, Q11)** | Filesystem: `O_NOATIME` EPERM fallback is refused at fingerprint-before with a clear message; opt-in `--allow-atime-updates` | Behaviour change for non-owner runs; owner/root runs unaffected |
| **D22 (user, Q12)** | New `docs/IMMUTABILITY.md` (threat model, guards, evidence tests, least-privilege recipes) and new `docs/testing/` for the matrices; module docs get Limits pointers | Two new doc locations, linked from README and `docs/QA_GUIDE.md` |
| **D23 (user, Q13)** | FIFO restored with `mkfifo`; char/block devices restored with `mknod` when CAP_MKNOD else preflight refusal (same pattern as ownership); unix sockets refused at analyze | New entry kinds `fifo`, `chardev`, `blockdev` with `rdev`; older destinations reject unknown kinds |
| **D24 (user, Q14)** | Filesystem fingerprint keeps full content hashing (three full reads per run); cost documented | No code change; Limits bullet |
| **D25 (user, Q15)** | Sparse holes, xattrs, POSIX ACLs out of scope; xattr refusal still asserted | Matrix rows document them as limits |
| **D26 (user, Q16)** | Misleading doc fields are found by a systematic row-by-row audit against a generated inventory | `scripts/env_inventory.sh` + `scripts/docs_parity.sh` |
| **D27 (user, Q17)** | CI runtime may grow up to about 2x; job timeouts raised accordingly | `e2e.yml` timeouts 40 -> 80 min on postgres jobs |

## Open questions

All clarification-gate questions were resolved by the user (D11-D27). The rows below are the
`UNVERIFIED` external facts; each is resolved by the implementer inside the sub-phase that
depends on it and the outcome is recorded in `STATE.md` §8. None needs a user decision.

| # | Question | Assumed default in this plan | Affects |
|---|----------|------------------------------|---------|
| R5 | Which `postgis/postgis` tags exist per major? | Table `10-2.5 11-3.3 12-3.4 13..17-3.5 18-3.6`; missing tag => SKIP (exit 77) | phase 1 § 1.2, 1.6, 1.7 |
| R6 | Exact `extcondition` PostGIS registers for `spatial_ref_sys` | Read from the catalog at run time; fixture srid 990001 satisfies any plausible condition | phase 1 § 1.4 |
| R7 | Does the locked `tokio-postgres 0.7.x` expose `keepalives_interval`/`keepalives_retries`? | Use `keepalives` + `keepalives_idle` only if not | phase 3 § 3.2 |
| R8 | MinIO policy action names for `GetObjectAcl`/`GetObjectTagging` | Fallback `s3:Get*` + `s3:ListBucket` | phase 4 § 4.7 |
| R9 | `mongod --profile 0 --slowms 0` logs every op as JSON on 4.4..8 without `system.profile` writes | Verify on 4.4 and 8; otherwise assert only the least-privilege run succeeds | phase 4 § 4.7 |

## Architecture summary

Source side: `PostgresSource` owns a per-database connection pool of `ReadOnlyClient`
(guarded `query`/`copy_out` only). `analyze()` emits table items with `expected_rows`
and new `extension_config` items; `fingerprint()` hashes the normalized catalog plus a
commutative per-table row commitment. Destination side: `stream_in` compares the COPY row
count with `expected_rows`, refreshes only populated matviews and compares their counts,
verifies constraint state, and refuses unknown kinds. `rb-core` session keeps the
before/after fingerprint audit on every exit path including panics. Filesystem adds
`fifo`/`chardev`/`blockdev` entries and an atime guard. Harness: fixtures + external psql /
pg_dump oracle + server-log allowlist assertions; docs guarded by an inventory parity gate.

## Interface

| Surface | Name | Type / values | Default | Notes |
|---------|------|---------------|---------|-------|
| CLI flag (postgres destination) | `--extension-version` | `source` \| `default` | `source` | env `RUST_BACKUP_EXTENSION_VERSION`; session.yml `params.extension_version`; follows `--overwrite` pattern (`crates/rust-backup/src/main.rs:203`); rejected with a config error on other modules/roles |
| CLI flag (filesystem source) | `--allow-atime-updates` | bool | `false` | env `RUST_BACKUP_ALLOW_ATIME_UPDATES`; session.yml `params.allow_atime_updates`; follows `--no-preserve-ownership` pattern (`main.rs:212-220`) |
| Env (test-only, unsupported) | `RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA` | i64 | unset | Source-side fault hook adding a delta to every `expected_rows`; documented next to `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART` (`docs/usage/10-variabili-ambiente.md:117-118`) |
| Destination output | `RESTORE VERIFIED` block | new lines `rows verified: ...`, `constraints: ...` | — | Existing lines unchanged (grep in `e2e/lib.sh:71` keeps working) |
| Behaviour change | filesystem source not owner and no CAP_FOWNER | refuse | — | Was: silent atime updates. New message names the path and the flag |
| Behaviour change | filesystem FIFO / char / block device | restored | — | Was: hard error "unsupported filesystem entry". Sockets still refused |

## Protocol and data-structure changes

| Change | Shape | Backward-compat strategy |
|--------|-------|--------------------------|
| `PlanItem.meta.expected_rows` (postgres table items, extension_config items) | `u64`, JSON key in `meta` | Additive; an older destination ignores unknown meta keys |
| Postgres payload item kind `extension_config` | `meta {extension, schema, table, condition, expected_rows}` | Older destination must reject unknown item kinds at validate (add explicit check if missing) |
| `PgPlanPayload`: `extension_configs: Vec<ExtensionConfigTable>`, `PgView.populated: bool`, `PgView.expected_rows: Option<u64>`, `PgConstraint.{validated, deferrable, initially_deferred, comment}` | serde `#[serde(default)]`; `validated`/`populated` default `true` | Older plans deserialize with defaults; bump `PgPlanPayload` version field if one exists |
| Filesystem entry kinds `fifo`, `chardev`, `blockdev`; `FilesystemEntry.rdev: Option<u64>` | JSON | Older destination rejects unknown kind at validate ("unknown filesystem entry kind") — fail closed |
| Postgres fingerprint domain prefix | `rust-backup/pg-fingerprint/v2\n` | Fingerprints are compared only within one run; no persistence |
| Filesystem fingerprint | adds `rdev` for the new kinds | Same as above |

## Phases

| Phase | File | Primary assignment | Shippable alone? |
|-------|------|--------------------|------------------|
| 0 — Harness foundation (additive) | [phase_01.md](phase_01.md) | `agent:opus` | yes |
| 1 — PostgreSQL fidelity matrix | [phase_02.md](phase_02.md) | `agent:opus` | yes |
| 2 — Restore validation (rows, constraints) | [phase_03.md](phase_03.md) | `agent:opus` | yes |
| 3 — PostgreSQL latent defects | [phase_04.md](phase_04.md) | `agent:opus` | yes |
| 4 — Source immutability guardrails (all modules) | [phase_05.md](phase_05.md) | `agent:opus` | yes |
| 5 — Filesystem deep verification | [phase_06.md](phase_06.md) | `agent:opus` | yes |
| 6 — Documentation audit and parity gate | [phase_07.md](phase_07.md) | `agent:opus` | yes |

Live status of every phase is in `STATE.md` §11, never duplicated here.

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| Read-only connect, startup GUC pins | `connect_read_only`, options string | `crates/rb-postgres/src/connect.rs:41-85` |
| Catalog queries per kind | `gather_extensions/tables/columns/indexes/sequences/views/functions`, `CONSTRAINTS_QUERY` | `crates/rb-postgres/src/introspect.rs:244,330,403,491,531,626,676,457` |
| Refusal probes | `find_unsupported_objects` | `crates/rb-postgres/src/introspect.rs:748-926` |
| DDL ordering | `build_database_ddl`, `topological_tables`, `topological_views`, `relax_depths` | `crates/rb-postgres/src/ddl.rs:701,951,975,998` |
| Extension / matview / constraint DDL | `create_extension`, `create_view`, `refresh_view`, `add_constraint` | `crates/rb-postgres/src/ddl.rs:281,517,544,451` |
| COPY out / in SQL | `copy_out_sql`, `copy_in_sql` | `crates/rb-postgres/src/source.rs:97`, `crates/rb-postgres/src/dest.rs:889` |
| Destination apply loop | `apply_data`, `ActiveCopy` | `crates/rb-postgres/src/dest.rs:904-931` |
| Preflight | `validate`, `assess`, `probe_dest`, `DestProbe` | `crates/rb-postgres/src/dest.rs:42,55,130` |
| Catalog diff | `verify_catalog`, `catalog_differences` | `crates/rb-postgres/src/dest.rs:323-401` |
| Fingerprint | `fingerprint`, `hash_catalog`, `table_stat`, `compose` | `crates/rb-postgres/src/immutability.rs:42,113,119,84` |
| Session audit | `run_source_limited`, `audit_source_before_ack` | `crates/rb-core/src/session.rs:317-355,229-257` |
| Capability probe | `has_cap_chown` | `crates/rb-filesystem/src/dest.rs:545-557` |
| Walk kinds | `visit`, `entry_from_meta` | `crates/rb-filesystem/src/walk.rs:89-154,175-192` |
| No-atime open | `NoAtimeReader::open` | `crates/rb-filesystem/src/source.rs:141-155` |
| Metadata apply order | `apply_metadata`, `verify_metadata` | `crates/rb-filesystem/src/dest.rs:352-395,171-224` |
| e2e helpers | `rb_assert_formal_verification`, `rb_tree_digest`, `rb_atime_manifest`, `rb_seed_filesystem_fixture` | `e2e/lib.sh:71,178,260,276` |
| Test-only fault hook pattern | `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART` | `crates/rb-s3/src/lib.rs:1357` |
| Gate list | `scripts/gates.sh`, `scripts/crate_invariants.sh`, `scripts/help_parity.sh` | `scripts/` |
| CLI params / merge | `ModuleParamArgs`, `merge_params`, `underlay` | `crates/rust-backup/src/main.rs:154-232,352-366,340-349` |

## References (external documentation consulted)

| # | What it settled | Source | Version / date |
|---|-----------------|--------|----------------|
| R1 | PostGIS extension tables (`spatial_ref_sys`, `layer`, `topology`) are backed up only with the extension, via config-table registration | https://postgis.net/docs/postgis_installation.html | fetched 2026-09-16 |
| R2 | `pg_dump` includes every relation registered by `pg_extension_config_dump`, filtered by its condition | https://www.postgresql.org/docs/current/app-pgdump.html | current, 2026-09-16 |
| R3 | Generated columns (`GENERATED ALWAYS AS ... STORED`) exist from PostgreSQL 12 | https://www.postgresql.org/docs/release/12.0/ | 12.0 |
| R4 | Procedures (`CREATE PROCEDURE`), indexes/PK/FK on partitioned tables exist from PostgreSQL 11 | https://www.postgresql.org/docs/release/11.0/ | 11.0 |
| R5 | UNVERIFIED: `postgis/postgis` Docker tags per major (expected `10-2.5`, `11-3.3`, `12-3.4`, `13..17-3.5`, `18-3.6`) | https://hub.docker.com/r/postgis/postgis | resolve at run time with `docker manifest inspect` |
| R6 | UNVERIFIED: exact `pg_extension_config_dump` condition PostGIS registers for `spatial_ref_sys` (read from `pg_extension.extcondition` at run time; the fixture uses srid 990001) | https://postgis.net/docs/ | — |
| R7 | UNVERIFIED: `tokio-postgres 0.7` exposes `Config::keepalives` and `keepalives_idle` (present since 0.5); `keepalives_interval`/`keepalives_retries` may need a newer 0.7.x | https://docs.rs/tokio-postgres/0.7 | 0.7 |
| R8 | UNVERIFIED: MinIO policy action names for `GetObjectAcl`/`GetObjectTagging`; fallback `s3:Get*` + `s3:ListBucket` | https://min.io/docs/minio/linux/administration/identity-access-management/policy-based-access-control.html | — |
| R9 | UNVERIFIED: `mongod --profile 0 --slowms 0` logs every operation to the server log as JSON on 4.4+ without writing `system.profile` | https://www.mongodb.com/docs/manual/reference/program/mongod/ | 4.4..8 |

Unverified points are marked `UNVERIFIED`; each is resolved by the implementer inside the
sub-phase that depends on it (no user decision needed) and recorded in `STATE.md` §8.

## Invariants

- **I-IMMUT:** source unaltered; fingerprint before == after on every exit path (now including panics).
- **I-NOTEMP:** no staging to disk; new items stream in <= 1 MiB chunks like every other item.
- **I-ERRORS:** every new failure is phase-tagged or a named `BackupError` variant; no `unwrap`/`expect`/`panic!` outside tests.
- **I-MODULAR:** `rb_core::module` traits unchanged.
- **I-BANDWIDTH:** one item per carrier; postgres stays `max_carriers() == 1`.
- **I-OBSERV:** new verification facts appear in the `RESTORE VERIFIED` block and in tracing.
- **I-SELFCONTAINED:** destination validates and verifies from the plan alone.
- **I-RO-TYPE (new):** source-side code holds only read-only client types; `scripts/source_readonly_lint.sh` guards the files.
- **I-FAILCLOSED (new):** an unknown plan item kind or filesystem entry kind is refused at validate, never skipped.

## Risk register

| Risk | Mitigation |
|------|-----------|
| Connection-pool refactor breaks introspection or streaming | T-PG-MATRIX + T-PG-CONN must pass on 10..=18 before phase 3 closes (§ 3.2) |
| `pg_dump` oracle noise across majors produces false diffs | Normalization + `oracle_ignore.txt` allowlist reviewed per row (§ 1.2); in-product `verify_catalog` remains authoritative |
| PostGIS image tag missing for a major | Runner resolves tags, missing => SKIP (exit 77 semantics from `e2e/full_matrix.sh`) (§ 1.6) |
| Commutative row commitment weaker than sorted digest | 128-bit wrapping sum + exact count; accidental collision negligible; unit tests for order-independence, single-row change, duplicate swap (§ 3.3) |
| atime guard breaks non-owner workflows | Opt-in flag documented; owner/root unaffected; e2e proves both paths (§ 4.5) |
| CI minutes grow | Timeouts raised to 80 min (D27); PostGIS job only where tags exist |
| Least-privilege role needs privileges not known upfront | § 4.7 iterates until green and records the final role in `docs/IMMUTABILITY.md` |

## Verification summary

| Gate | Command | Where it runs |
|------|---------|---------------|
| fmt / lint / build / unit / crate invariants / help parity | `bash scripts/gates.sh` | every sub-phase |
| readonly lint (added in § 4.6) | `bash scripts/source_readonly_lint.sh` | phase 4 onward, inside `gates.sh` |
| docs parity (added in § 6.4) | `bash scripts/docs_parity.sh` | phase 6 onward, inside `gates.sh` |
| postgres matrix | `bash e2e/postgres_matrix.sh <M>` and `<S:D>` | phases 1-4 |
| postgres PostGIS | `RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh <M>` | phases 1-4 |
| postgres faults / timeouts / abort | `bash e2e/fault_matrix.sh` | phases 3-4 |
| postgres memory | `bash e2e/postgres_large_table.sh 16` | phase 3 |
| filesystem matrix (root) | `sudo bash e2e/filesystem_matrix.sh` | phases 4-5 |
| existing regression | `bash e2e/full_matrix.sh` | every phase that touches e2e |

**Acceptance:** the reference scenario is proven by T-PG-ORACLE (per-row PASS with external
oracle), T-PG-REFUSE (refusal rows fire before transfer), T-PG-GIS + T-PG-EXTCFG (PostGIS
incl. config rows), T-PG-ROWS + T-PG-CONSTR (in-product row/constraint verification),
T-IMMUT-PG-LP + T-PG-CONN + T-PG-FP2 (server-log evidence), T-FS-MATRIX + T-FS-SPECIAL
(filesystem matrix), T-DOCS-PARITY (docs gate).
**Run caveats:** Docker required for postgres/mongo/minio scripts; `sudo -n` root for
filesystem scripts; e2e runs serially; release build via `rb_build_release` (`e2e/lib.sh:9`).
These commands are identical to `STATE.md` §3; they must not drift.

## Model-assignment summary

| Phase | Sub-phases by assignment | Primary | `agent-1` review gates |
|-------|--------------------------|---------|------------------------|
| 0 | 0.1-0.5 -> `agent:opus` | `agent:opus` | self-review |
| 1 | 1.1-1.8 -> `agent:opus` | `agent:opus` | self-review (1.3 fixes, 1.4 data model) |
| 2 | 2.1-2.4 -> `agent:opus` | `agent:opus` | self-review (2.1 acceptance assertions) |
| 3 | 3.1-3.6 -> `agent:opus` | `agent:opus` | self-review (3.2 concurrency/lifecycle, 3.3 hot path) |
| 4 | 4.1-4.9 -> `agent:opus` | `agent:opus` | self-review (4.2-4.4 type design, 4.8 lifecycle) |
| 5 | 5.1-5.6 -> `agent:opus` | `agent:opus` | self-review (5.4 TOCTOU) |
| 6 | 6.1-6.5 -> `agent:opus` | `agent:opus` | self-review (6.5 final docs read) |
