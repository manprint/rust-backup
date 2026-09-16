# Phase 1 — PostgreSQL fidelity matrix

> **Intent:** every object kind in scope (tables, sequences, views, matviews, indexes,
> constraints, extensions incl. PostGIS) round-trips on PostgreSQL 10..=18 same-major and on
> the cross-major pairs, proven by an external oracle; every out-of-scope kind is refused
> before any transfer; extension config tables and extension version policy are handled.
> **Shippable alone?** yes — fixes land with regression tests; new flag defaults to today's behaviour.
> **Preconditions:** phase_01 DONE (matrix doc, fixture layout, oracle helpers).

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

Shared facts for this phase (from recon, verified 2026-09-16):
- Catalog read: `crates/rb-postgres/src/introspect.rs` — `gather_extensions:244`, `gather_tables:330` (extension-owned excluded via `pg_depend deptype='e'` at 361-362), `gather_columns:403` (gate `major >= 12` for `attgenerated` at 406; `NOT attisdropped` at 430), `CONSTRAINTS_QUERY:457`, `gather_indexes:491`, `gather_sequences:531` (live `last_value/is_called` at 610), `gather_views:626`, `gather_functions:676`, `find_unsupported_objects:748-926` (pg18 not-null probe 917-926).
- DDL emit: `crates/rb-postgres/src/ddl.rs` — `create_extension:281` (emits `WITH SCHEMA ... VERSION '...'`, no CASCADE), `add_constraint:451`, `create_sequence:470`, `sequence_setval:500`, `create_view:517` (matview `WITH NO DATA` at 532), `refresh_view:544`, `build_database_ddl:701-947` (extensions 734-736, sequences 754-778, tables 781, identity 797-806, functions 809-820, views 830 + 895-906, non-FK constraints 833-837, FK 838-841, indexes 908-914, partitioned index attach 916-921, matview REFRESH 925-928, matview indexes 930-934), `topological_tables:951`, `topological_views:975`, `relax_depths:998`.
- Streaming: `crates/rb-postgres/src/source.rs` — `stream_out:47`, `copy_out_sql:97` (binary), chunk buffer 138. `crates/rb-postgres/src/dest.rs` — `validate:42`, `assess:55`, `probe_dest:130`, `verify_catalog:323`, `copy_in_sql:889`, `apply_data:904`, `copy_in:931`.
- Model: `crates/rb-postgres/src/model.rs` (`PgPlanPayload`, 531 lines). Params/traits: `crates/rb-postgres/src/lib.rs` (`max_carriers:117`, `verify:172-188`). Fingerprint: `crates/rb-postgres/src/immutability.rs` (`fingerprint:42`, `table_stat:119`).
- CLI params: `crates/rust-backup/src/main.rs` — `ModuleParamArgs:154-232` (`--admin:200`, `--overwrite:203`), `merge_params:352-366`, `underlay:340-349`.
- e2e: `e2e/postgres_matrix.sh` (docker run 50-51, `data_checksum` 228, case parsing 345-354, `schema_dump` diff 405-409, `fidelity_probe` 425), `e2e/postgres_introspect.sh`, `.github/workflows/e2e.yml` (`postgres:80` matrix 10..18, `postgres-cross-version:104` pairs `10:18 12:16 14:17 16:18`).
- Lints: every crate root has `#![forbid(unsafe_code)]` and `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]`; production code must return `rb_core::Result`.
- Docs to keep in sync: `docs/modules/POSTGRES.md` (Limits at 176), `docs/usage/03-postgres.md` (flag table at 49), `docs/usage/10-variabili-ambiente.md` (tables at 27/42/55/64/105), `docs/usage/11-codici-uscita.md`.

---

## Sub-phases

### 1.1 Fixture files per kind
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — SQL fixtures; self-review against the matrix rows.
- **Files:** new `e2e/fixtures/postgres/00_roles.sql`, `10_tables.sql`, `10_tables.ge11.sql`, `10_tables.ge12.sql`, `20_sequences.sql`, `30_views.sql`, `40_matviews.sql`, `50_indexes.sql`, `50_indexes.ge11.sql`, `50_indexes.ge15.sql`, `60_constraints.sql`, `60_constraints.ge11.sql`, `60_constraints.ge12.sql`, `60_constraints.ge15.sql`, `60_constraints.ge18.sql`, `70_extensions.sql`, `71_rbtest_extension.sh` (installs the custom extension files into the container, see below), `80_postgis.sql` (loaded only when `RB_PG_IMAGE_REPO` is `postgis/postgis`), `refusals/*.sql` (15 files, `named_not_null.ge18.sql` gated).
- **Change:** one statement group per row of `docs/testing/POSTGRES_MATRIX.md` (created in § 0.1), in schema `mx` (plus `"Mixed Schema"` for TAB-14 and `ext` for EXT-06). Object names embed kind and row: `mx.t_tab_01`, `mx.s_seq_01`, `mx.v_view_01`, `mx.m_mv_01`, index `i_idx_01`, constraint `c_con_01`. Every table gets at least 3 rows unless the row says otherwise; TAB-13 uses `generate_series(1,100000)`; TAB-16 uses `repeat('x', 1048576)`. `00_roles.sql` creates roles `mx_owner` (NOLOGIN), `mx_reader`, `mx_writer` and grants used by TAB-18. Move every object of the existing inline fixture in `e2e/postgres_matrix.sh` (accounts/orders with add+drop column, `log`/`log_2025` inheritance, `doc` with sequence-backed default calling a function, partitioned `events` with two partitions and an index, views `zombies/active/account_status`, two matviews with a unique index, roles with GUCs, database owner/template/GUC) into these files so no existing coverage is lost; keep their names. Refusal fixtures each create exactly one refused object kind on top of one plain table. `71_rbtest_extension.sh <container>`: writes `rbtest.control` (`default_version = '1.0'`, `relocatable = true`) and `rbtest--1.0.sql` (creates `rbtest_cfg(k int primary key, v text)`, inserts k 1..3, runs `SELECT pg_catalog.pg_extension_config_dump('rbtest_cfg', 'WHERE k >= 1000')`) into `$(docker exec <c> pg_config --sharedir)/extension/` via `docker exec -i ... tee`; when called with a second argument `1.1` it writes only version `1.1` files with `default_version = '1.1'` (used by EXT-08 on the destination container). `70_extensions.sql` then runs `CREATE EXTENSION rbtest` and `INSERT INTO rbtest_cfg VALUES (1000, 'custom')`. `80_postgis.sql`: `CREATE EXTENSION postgis`, table `mx.t_gis_02` with `geom geometry(Point,4326)` and `geog geography(Point,4326)` and 10 rows, GiST index, `INSERT INTO spatial_ref_sys (srid, auth_name, auth_srid, srtext, proj4text) VALUES (990001, 'RB', 990001, 'GEOGCS["RB",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]', '+proj=longlat +datum=WGS84 +no_defs')`, view `mx.v_gis_05` using `ST_AsText(geom)`.
- **Unit tests:** none (SQL).
- **e2e tests:** T-PG-FIX — for M in 10 12 16 18: start `postgres:M-alpine` with `rb_pg_start`, run `rb_pg_load_fixtures` for `e2e/fixtures/postgres`, every file loads with `ON_ERROR_STOP` (gated files skipped with a `SKIP` line); each refusal file loads alone into a fresh database. Run this by hand until § 1.2 automates it.
- **Done:** T-PG-FIX passes on 10, 12, 16, 18; `docs/testing/POSTGRES_MATRIX.md` `Fixture file` column filled; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 1.2).

### 1.2 Runner v2: `e2e/postgres_matrix.sh` with external oracle and per-row results
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — harness; self-review (acceptance assertions).
- **Files:** `e2e/postgres_matrix.sh` (whole script), `e2e/fixtures/postgres/oracle_ignore.txt`, `e2e/README.md`.
- **Change:** keep the script contract (`postgres_matrix.sh <M>` or `<S:D>`, exit 77 for environment limits) and the existing assertions (`rb_assert_formal_verification`, `data_checksum` 228, fidelity probe 425, T-PG-IMMUT mid-transfer abort). Replace the inline fixture with `rb_pg_load_fixtures` over `e2e/fixtures/postgres` (run `71_rbtest_extension.sh` on the source container before loading, and on the destination container with the same version; EXT-08 uses `1.1` on the destination and is asserted in § 1.5). Start containers with `rb_pg_start` so `RB_PG_LOG_ARGS` apply. Image repo from `RB_PG_IMAGE_REPO` (default `postgres`, tag `<M>-alpine`); when it is `postgis/postgis` use the tag mapping function `postgis_tag <M>` (table `10-2.5 11-3.3 12-3.4 13-3.5 14-3.5 15-3.5 16-3.5 17-3.5 18-3.6`, R5 UNVERIFIED: verify each with `docker manifest inspect`, correct the table, and on a missing tag print `SKIP` and exit 77) and load `80_postgis.sql`. After the restore: (a) `rb_pg_oracle_schema` using the destination container's `pg_dump` against both servers (source reached through `rb_pg_container_ip`), `diff -u`, empty diff required; (b) `rb_pg_oracle_counts` on both sides, `diff`, empty required; (c) `rb_pg_assert_readonly_log <src> <source_user>` (the source user is `postgres` until § 4.7 introduces `rb_ro`); (d) per-row result: for each row ID in the matrix doc, derive the object name(s) from the naming rule, check they appear in the destination oracle output and in the equal diff, print `PASS <ID>`; a row whose object is absent or differing prints `FAIL <ID>`; a row gated above the current major prints `SKIP <ID> (needs >= N)`; (e) refusal loop: for each `refusals/*.sql` create database `mx_ref_<name>` on the source, load the file, run the source with `--database mx_ref_<name>` against a destination, expect a non-zero exit, expect the destination to hold no database of that name, expect the source log to contain no `COPY` statement for that run window, print `PASS M-PG-REF-nn` or `FAIL`. Cross-major diffs that are legitimate (for example privilege syntax newer than the source major) go into `oracle_ignore.txt` with a comment naming the row and the reason; every entry is reviewed in self-review. Final line `MATRIX <M>: <pass> pass, <fail> fail, <skip> skip`; exit 1 when `fail > 0`.
- **Unit tests:** none (shell).
- **e2e tests:** T-PG-ORACLE — `bash e2e/postgres_matrix.sh 16` prints `MATRIX 16: N pass, 0 fail, K skip` with `K` equal to the number of rows gated above 16 (only `.ge18` rows) and both oracle diffs empty; T-PG-REFUSE — every `M-PG-REF-*` row prints PASS on 16 and 18. Failures found here are fixed in § 1.3, not by relaxing the oracle.
- **Done:** runner prints per-row results and the summary; T-PG-MATRIX and T-PG-IMMUT still pass (existing assertions kept); ShellCheck clean; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 1.3; record the FAIL row IDs found on 10/12/16/18 in §9 as the input for 1.3).

### 1.3 Triage and fix every failing row (tables, sequences, views, matviews, indexes, constraints, extensions)
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — implementation and regression tests; self-review gate on every DDL-ordering change.
- **Files:** `crates/rb-postgres/src/introspect.rs`, `crates/rb-postgres/src/ddl.rs`, `crates/rb-postgres/src/dest.rs`, `crates/rb-postgres/src/source.rs`, `crates/rb-postgres/src/model.rs`, `docs/modules/POSTGRES.md`, `docs/testing/POSTGRES_MATRIX.md`.
- **Change:** procedure per FAIL row recorded in `STATE.md` §9: (1) reproduce with the smallest fixture; (2) write the failing unit test in the module that owns the defect (`ddl.rs` tests for emitted DDL text, `introspect.rs` tests for query/gate logic, `dest.rs` tests for `assess`/`catalog_differences`, `model.rs` for serde); (3) fix; (4) re-run the matrix on the majors where the row applies; (5) if the object cannot be reproduced faithfully, make it a refusal in `find_unsupported_objects` (fires at Analyze, before any transfer), move the row to `Expected = refused before transfer`, and add a Limits bullet in `docs/modules/POSTGRES.md` — never certify a partial copy. Suspects to check explicitly even if the runner passes (each becomes a unit test):
  - MV-02: `gather_views:626` must read `c.relispopulated` into a new `PgView.populated: bool` (`#[serde(default = "default_true")]`); `build_database_ddl:925-928` must skip `refresh_view` when `populated == false`. Test `unpopulated_matview_is_not_refreshed`.
  - MV-03: the REFRESH loop at 925-928 must iterate in `topological_views` order. Test `matview_refresh_follows_dependency_order`.
  - SEQ-07 / TAB-08: identity-backed sequences must receive `setval` (dedup at 744-778 skips creation, not `setval`). Test `identity_sequence_current_value_is_restored`.
  - TAB-09: `copy_out_sql:97` and `copy_in_sql:889` must use an explicit column list that excludes generated columns (COPY FROM refuses them). Test `copy_column_list_excludes_generated_columns`.
  - TAB-04/05: the source must not `COPY` a partitioned parent (`relkind = 'p'`) as a data item; only leaf partitions carry data. Test `partitioned_parent_has_no_data_item`.
  - TAB-06: inheritance parent data item copies only its own rows (plain `COPY parent` already does; count semantics in § 2.1 use `FROM ONLY`). Test `inheritance_parent_item_is_only_its_rows` (SQL text assertion).
  - CON-15: `CONSTRAINTS_QUERY:457` lacks `obj_description(oid, 'pg_constraint')`; add `comment: Option<String>` to the constraint model and emit `COMMENT ON CONSTRAINT <name> ON <table> IS <literal>` right after `add_constraint:451`. Test `constraint_comment_is_emitted`.
  - CON-17 (pg18): plain NOT NULL rows (`contype = 'n'`, auto-named) must round-trip; only named or `NOT VALID` not-null rows are refused (probe 917-926). Test `pg18_auto_named_not_null_is_accepted`.
  - IDX-13: partitioned index attach (916-921) must run after every partition index exists; IDX-17/CON-18 `NULLS NOT DISTINCT` are verbatim from `pg_get_indexdef`/`pg_get_constraintdef` (assert in a test that the strings survive the model round-trip).
  - EXT-06: `create_extension:281` emits `WITH SCHEMA ext`; schema `ext` must be created before (713-736 already orders schemas first). Test `extension_in_custom_schema_follows_schema_creation`.
  - TAB-18: ownership + GRANTs for a NOLOGIN role; roles are cluster-wide (existing Limits bullet) — assert the oracle diff is empty on same-major.
  Update `docs/modules/POSTGRES.md` "Supported objects" with constraint comments and any newly refused kind; update the matrix doc `Expected` column where a row became a refusal.
- **Unit tests:** the tests named above plus one regression test per FAIL row fixed (`<kind>_<row>_roundtrip` naming, e.g. `idx_10_brin_index_is_emitted`); all existing 66 rb-postgres tests still pass.
- **e2e tests:** T-PG-ORACLE on 10, 11, 12, 13, 14, 15, 16, 17, 18 and cross pairs `10:18 12:16 14:17 16:18` all print `0 fail`; T-PG-REFUSE passes on 16 and 18.
- **Done:** zero FAIL rows on every major and pair listed; every fix has a named unit test; `STATE.md` §9 list of FAIL rows emptied (each row noted in §4 `What changed`); `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 1.4).

### 1.4 Extension config tables (`pg_extension_config_dump`)
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — data-model design + implementation; self-review gate (data model).
- **Files:** `crates/rb-postgres/src/introspect.rs` (`gather_extensions:244`), `crates/rb-postgres/src/model.rs`, `crates/rb-postgres/src/lib.rs` (item construction in `analyze`; locate with `grep -n "PlanItem" crates/rb-postgres/src/*.rs`), `crates/rb-postgres/src/source.rs` (`stream_out:47`, `copy_out_sql:97`), `crates/rb-postgres/src/dest.rs` (`apply_data:904`, `copy_in_sql:889`, `verify_catalog:323`), `crates/rb-postgres/src/immutability.rs` (`fingerprint:42`, table loop at 52-155), `docs/modules/POSTGRES.md`, `docs/usage/03-postgres.md`.
- **Change:** R1/R2 — pg_dump includes rows of relations registered through `pg_extension_config_dump`, filtered by the registered condition; rust-backup drops them today because extension-owned relations are excluded wholesale. Add to `gather_extensions` a second query: `SELECT e.extname, n.nspname, c.relname, u.cond FROM pg_extension e CROSS JOIN LATERAL unnest(e.extconfig, e.extcondition) AS u(reloid, cond) JOIN pg_class c ON c.oid = u.reloid JOIN pg_namespace n ON n.oid = c.relnamespace ORDER BY 1,2,3` into `PgPlanPayload.extension_configs: Vec<ExtensionConfigTable { extension: String, schema: String, table: String, condition: Option<String> }>` (`#[serde(default)]`). `analyze` emits one `PlanItem` per entry with `kind = "extension_config"`, `name = "<schema>.<table>"`, `estimated_bytes` from `pg_class.relpages * 8192`, `meta = {extension, schema, table, condition}` (`expected_rows` is added in § 2.1). Source `stream_out`: for this kind emit `COPY (SELECT * FROM <qual> WHERE <condition>) TO STDOUT (FORMAT binary)` (omit `WHERE` when `condition` is `None`); the read-only guard of § 4.2 must accept this shape (it is `COPY (...) TO STDOUT`). Destination `apply_data`: before opening the COPY for this kind run, on the destination only, `DELETE FROM <qual> WHERE <condition>` (or `TRUNCATE <qual>` when `None`) so the rows `CREATE EXTENSION` inserted are replaced by the source's rows; then `COPY <qual> FROM STDIN (FORMAT binary)`. Unknown item kinds: make `apply_data` return `BackupError::PlanRejected(format!("unknown postgres item kind {kind}"))` at the first such item and add the same check to `validate:42` so an older binary fails at preflight (I-FAILCLOSED). `verify_catalog`: the `extension_configs` list is part of the payload equality; add a readable difference message. Fingerprint: the table loop must include the config tables with their condition (`SELECT count(*) ... WHERE cond` and the row commitment restricted by the condition; use the same helper the regular tables use). Docs: `docs/modules/POSTGRES.md` "Supported objects" gains "extension configuration tables (rows matching the extension's registered condition)"; `docs/usage/03-postgres.md` gains one paragraph in the restore section.
- **Unit tests:** `extension_config_item_streams_conditioned_rows` (source SQL text equals the expected `COPY (SELECT * FROM "s"."t" WHERE k >= 1000) TO STDOUT (FORMAT binary)`); `extension_config_destination_deletes_matching_rows_before_copy`; `extension_config_without_condition_truncates`; `unknown_item_kind_is_rejected_at_validate_and_apply`; `payload_without_extension_configs_deserializes` (older plan JSON).
- **e2e tests:** T-PG-EXTCFG — matrix rows EXT-07 (all majors via `rbtest`) and GIS-04 (PostGIS image): after restore the destination holds `rbtest_cfg` row `k = 1000` and `spatial_ref_sys` srid 990001, and the oracle counts for those tables are equal; T-PG-IMMUT still passes (fingerprint includes config rows).
- **Done:** tests above green; T-PG-EXTCFG passes on 16 and on one PostGIS major; older-plan deserialization test green; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 1.5).

### 1.5 Extension version preflight and `--extension-version`
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — preflight logic + CLI plumbing; self-review.
- **Files:** `crates/rb-postgres/src/dest.rs` (`probe_dest:130`, `DestProbe`, `assess:55`, `verify_catalog:323-401`), `crates/rb-postgres/src/ddl.rs` (`create_extension:281`, `build_database_ddl:734-736`), `crates/rb-postgres/src/lib.rs` (params struct; `PostgresParams`), `crates/rust-backup/src/main.rs` (`ModuleParamArgs:154-232`, `merge_params:352-366`), `scripts/help_parity.sh` (if it enumerates flags), `docs/usage/03-postgres.md:49` table, `docs/usage/10-variabili-ambiente.md:64` table, `docs/modules/POSTGRES.md`, `docs/usage/07-sessioni-yaml.md` (params key list if present).
- **Change:** `PostgresParams.extension_version: ExtensionVersionPolicy` with variants `Source` (default) and `Default`, parsed from the string values `source` / `default`. CLI: add `--extension-version <source|default>` to `ModuleParamArgs` next to `--overwrite:203`, `env = "RUST_BACKUP_EXTENSION_VERSION"`, `default_value = "source"`, `value_parser = ["source", "default"]`, help "Extension version policy on the destination: source = install the exact source version (refuse when unavailable); default = install the destination's default version and record the deviation". Merge key `extension_version` in `merge_params`. Reject the flag when the module is not postgres or the role is not destination following exactly the `--overwrite` handling (read `main.rs:203` and its validation site). `probe_dest`: add `available_extensions: Vec<(String, String)>` from `SELECT name, version FROM pg_available_extension_versions ORDER BY 1,2`. `assess`: for every payload extension add check `extension:<name>`: pass when `(name, version)` is available; under `Default`, pass with the note `will install default version <v>` when `name` is available in any version; otherwise fail with message `extension <name> version <v> is not available on the destination (available: <list or none>); install it or pass --extension-version default`. DDL: `create_extension` takes the policy; under `Default` omit the `VERSION '<v>'` clause. `verify_catalog`: under `Default`, compare extensions ignoring `version` and append `deviation: extension <name> restored at version <actual> (source <v>)` to the `VerificationReport` notes (locate the notes/lines field in `rb_core::verification::VerificationReport`). Docs: table rows for the flag and the env var (`Lato` = destination), `docs/modules/POSTGRES.md` "Extensions" paragraph, `docs/usage/07-sessioni-yaml.md` params key.
- **Unit tests:** `assess_refuses_missing_extension_version` (fake `DestProbe` without the version); `assess_accepts_default_policy_with_note`; `create_extension_omits_version_under_default_policy`; `extension_version_flag_is_destination_only` (clap parse test in `crates/rust-backup`, next to existing CLI tests); `merge_params_carries_extension_version`.
- **e2e tests:** T-PG-EXTVER — matrix row EXT-08: destination container prepared with `71_rbtest_extension.sh <dst> 1.1`; run without the flag exits non-zero, the destination log contains `extension rbtest version 1.0 is not available`, no `mx` database exists on the destination; run with `--extension-version default` succeeds and the `RESTORE VERIFIED` block contains `deviation: extension rbtest restored at version 1.1 (source 1.0)`. T-HARN-INV re-run shows the new env var.
- **Done:** tests above green; T-PG-EXTVER passes on 16; `bash scripts/help_parity.sh` green; docs tables updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 1.6).

### 1.6 PostGIS fixture run and image resolution
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — harness; self-review.
- **Files:** `e2e/postgres_matrix.sh` (`postgis_tag`, image handling from § 1.2), `e2e/fixtures/postgres/80_postgis.sql`, `e2e/fixtures/postgres/oracle_ignore.txt`, `docs/testing/POSTGRES_MATRIX.md` (GIS rows `Fixture file`), `e2e/README.md`.
- **Change:** run `RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh <M>` for every major whose tag resolves (R5). PostGIS images are Debian-based: `pg_dump`, `psql` and `pg_config` exist; the superuser is `postgres`; `postgres $RB_PG_LOG_ARGS` command form is the same. Fix any GIS row failure with the § 1.3 procedure. Record the resolved tag table in the script and in the matrix doc. The cross pair `12:16` on PostGIS is run with `--extension-version default` (GIS-06) and without (expected refusal), reusing the T-PG-EXTVER assertions.
- **Unit tests:** regression tests for any GIS fix (naming `gis_<row>_...`).
- **e2e tests:** T-PG-GIS — `RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh 16` prints `0 fail`; GIS-04 (config row) and GIS-06 (cross pair with and without the flag) print PASS.
- **Done:** T-PG-GIS passes on every resolvable major and on `12:16`; unresolvable tags print SKIP with exit 77 and are listed in `STATE.md` §8; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 1.7).

### 1.7 CI matrix update
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — CI configuration; self-review.
- **Files:** `.github/workflows/e2e.yml` (`postgres:80`, `postgres-cross-version:104`), `docs/QA_GUIDE.md`.
- **Change:** raise `timeout-minutes` of `postgres` and `postgres-cross-version` from 40/45 to 80 (D27). Add job `postgres-postgis` copying the `postgres` job with `env: RB_PG_IMAGE_REPO: postgis/postgis`, matrix `version: [10, 11, 12, 13, 14, 15, 16, 17, 18]` minus the majors § 1.6 recorded as unresolvable, `timeout-minutes: 80`, `continue-on-error: false`. Add pair `12:16` to a new job `postgres-postgis-cross-version` (same env). Keep triggers identical (`push`/`pull_request` on `dev`, `main`, `workflow_dispatch`). Run `docker run --rm -v "$PWD:/repo" rhysd/actionlint:1.7.12` locally (same pin as CI). Document the new jobs in `docs/QA_GUIDE.md` CI section.
- **Unit tests:** none.
- **e2e tests:** none locally; CI run on the `dev` branch shows the new jobs green (record the run URL in `STATE.md` §7).
- **Done:** actionlint clean; workflow pushed by the user (this plan never pushes) and green, or the run URL recorded as pending in §9; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 1.8).

### 1.8 Update README.md
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `README.md`.
- **Change:** update these sections for what this phase made usable: "Modules -> PostgreSQL" (supported objects now include constraint comments and extension configuration tables such as PostGIS `spatial_ref_sys` custom rows; list any kind that became a refusal); "Usage -> postgres destination" (new flag `--extension-version <source|default>` with a realistic example and the refusal message users will see); "Configuration / environment variables" table at `README.md:136` (row `RUST_BACKUP_EXTENSION_VERSION`, default `source`); "Limitations" (extension versions must exist on the destination unless the flag is used). Include only shipped behaviour; no module, function or file names; preserve structure, tone and language; edit, do not rewrite.
- **Unit tests:** none (documentation).
- **e2e tests:** none — the README example was executed against a local pair and produced the documented output.
- **Done:** a new user can restore a PostGIS database with a differing extension version from the README alone; no implementation detail present; `bash scripts/gates.sh` green; closed in `STATE.md` with the §11 docs row for phase 1 set; §1 -> 2.1.

---

## Phase gates

- **Fmt:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --locked --all-features -p rb-postgres -p rust-backup` then the full `bash scripts/gates.sh`
- **Regression guard:** T-PG-MATRIX, T-PG-IMMUT, T-PG-INTROSPECT (`bash e2e/postgres_introspect.sh 16`), T-E2E0
- **README:** updated for `--extension-version`, extension config tables, new refusals

## Phase done criterion
`bash e2e/postgres_matrix.sh M` prints `0 fail` for M in 10..=18 and for `10:18 12:16 14:17
16:18`; `RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh 16` and `12:16` print
`0 fail`; every `M-PG-REF-*` row prints PASS; T-PG-EXTCFG and T-PG-EXTVER pass; CI jobs
`postgres`, `postgres-cross-version`, `postgres-postgis` green. README.md reflects this
phase's shipped behavior, and `STATE.md` §11 shows this phase `DONE` with every sub-phase closed.
