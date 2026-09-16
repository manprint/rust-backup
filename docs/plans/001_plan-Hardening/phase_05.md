# Phase 4 — Source immutability guardrails (all modules)

> **Intent:** make "the source is never written" a property of the types and of the
> deployment, not of discipline: typed read-only clients per module, a statement/command
> allowlist, a static lint in the gates, a warning when the source role can write, a
> filesystem atime guard, least-privilege end-to-end roles with server-log evidence, and a
> panic-safe after-audit in the session runner. A formal threat model documents every vector.
> **Shippable alone?** yes — every guard is either compile-time or fails closed with a named message.
> **Preconditions:** phase_04 DONE (connection pool exists; the wrapper replaces the pooled client type).

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
- Postgres: `connect_read_only` (`crates/rb-postgres/src/connect.rs:41-85`) sets `default_transaction_read_only=on` as a startup option; source code in `source.rs`, `introspect.rs`, `immutability.rs` uses `client.query*`/`client.copy_out` on `tokio_postgres::Client`. No `BEGIN`/`SET TRANSACTION` anywhere. After § 3.2 the client comes from `SourcePool`.
- MongoDB: source reads only `find(doc!{})` (`crates/rb-mongodb/src/source.rs:59-66`), `count_documents` + `_id`-sorted iteration (`immutability.rs:107-138`), `run_command` for `collStats`/`usersInfo` (read-only admin commands); every write call lives in `dest.rs` (223, 250, 331, 412, 421, 629). No explicit read concern / read preference.
- S3: `impl Source for S3Source` at `crates/rb-s3/src/lib.rs:487-599` calls only `list_objects_v2`, `get_object_tagging`, `get_object_acl`, `head_object`, `get_object`, `get_bucket_policy`; mutating calls (`create_bucket:203`, `delete_object:864`, `put_object:1162`) are in `impl Destination` (692+). Test hook `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART` at 1357.
- Filesystem: `NoAtimeReader::open` (`crates/rb-filesystem/src/source.rs:141-155`) tries `O_RDONLY|O_NOATIME`, falls back to plain `O_RDONLY` on `Errno::EPERM` (150). `fingerprint` (`immutability.rs:8-33`) reads every file via `read_noatime` (`source.rs:119-128`). Params struct and `--no-preserve-ownership` plumbing: `crates/rb-filesystem/src/lib.rs`, `crates/rust-backup/src/main.rs:212-220`.
- Session: `run_source_limited` (`crates/rb-core/src/session.rs:317`), fingerprint before at 324, `source_run` at 326, after-check on `Err` at 343-354, `SourceMutated` at 344 and 257; no `catch_unwind` in rb-core. `futures-util 0.3.21` is a workspace dependency (used by rb-postgres); check `crates/rb-core/Cargo.toml`.
- Lints: all crate roots deny `unwrap_used`/`expect_used`/`panic` outside tests; `scripts/crate_invariants.sh` enforces the attribute presence. Gate list: `scripts/gates.sh`.
- e2e: `e2e/postgres_matrix.sh`, `e2e/mongodb_matrix.sh` (images `mongo:4.4`, `mongo:5.0`..`mongo:8`), `e2e/s3_minio_test.sh` (MinIO `RELEASE.2025-09-07T16-13-09Z`, `mc RELEASE.2025-08-13T08-35-41Z` at line 71 for seeding), `e2e/lib.sh` helpers from § 0.2, `scripts/source_readonly_lint.sh` from § 0.4.
- Docs: `docs/modules/{POSTGRES,MONGODB,S3,FILESYSTEM}.md`, `docs/usage/05-filesystem.md:40` flag table, `docs/usage/10-variabili-ambiente.md:64`, `README.md`.

---

## Sub-phases

### 4.1 Formal threat model: `docs/IMMUTABILITY.md`
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — analysis and documentation; self-review.
- **Files:** new `docs/IMMUTABILITY.md`, `docs/modules/{POSTGRES,MONGODB,S3,FILESYSTEM}.md` (one "Immutability" pointer paragraph each), `README.md` link line (full README pass is § 4.9), `docs/QA_GUIDE.md` link.
- **Change:** write the document in English with these sections: (1) the invariant I-IMMUT in one paragraph and what "altered" means (data, schema, metadata, timestamps; excluded: server statistics counters and server log volume, stated explicitly); (2) one table per module `Vector | Reachable by our code? | Guard | Evidence (test ID)` covering at least — PostgreSQL: DML, DDL, TEMP objects, `nextval`/`setval`, advisory locks, `LISTEN`/`NOTIFY`, `pg_export_snapshot`, `pgsql_tmp` spill (closed by § 3.3), prepared transactions, replication slots, `BEGIN READ WRITE` override of `default_transaction_read_only`, statistics counters (accepted), ACCESS SHARE locks held during COPY (documented effect on concurrent DDL); MongoDB: writes, index builds, `$out`/`$merge`, profiler collection writes, `currentOp`/`killOp`, `renameCollection`; S3: Put/Delete/Multipart, tagging, ACL, bucket policy, versioning; filesystem: atime (O_NOATIME, § 4.5 guard), ctime, xattr, advisory locks, mmap, symlink following, `pgsql_tmp`-like temp files (none); (3) the fingerprint contract per module (what is hashed, cost, when it runs, what happens on drift, including the panic path of § 4.8); (4) least-privilege deployment recipes: PostgreSQL role for 10..=13 (per-schema `GRANT USAGE` + `SELECT ON ALL TABLES/SEQUENCES`) and 14+ (`GRANT pg_read_all_data`), MongoDB role (final set from § 4.7), S3 IAM/MinIO policy JSON (final set from § 4.7), filesystem (run as owner or root; the atime flag); (5) the guard architecture (typed clients, allowlist, lint, server-log assertions) with the test IDs T-IMMUT-*. Rows whose evidence lands later in this phase are written with the planned test ID and marked "evidence lands in § 4.x" — the marker is removed when that sub-phase closes.
- **Unit tests:** none (documentation).
- **e2e tests:** none.
- **Done:** document present with the five sections and every vector above; module docs and QA guide link to it; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 4.2).

### 4.2 PostgreSQL `ReadOnlyClient` with statement allowlist and role probe
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — type design + implementation; self-review gate (type design).
- **Files:** `crates/rb-postgres/src/connect.rs` (41-85 and the `SourcePool` from § 3.2), `crates/rb-postgres/src/source.rs`, `crates/rb-postgres/src/introspect.rs`, `crates/rb-postgres/src/immutability.rs`, `crates/rb-postgres/src/lib.rs`, `docs/modules/POSTGRES.md`, `docs/IMMUTABILITY.md`.
- **Change:**
  1. In `connect.rs` add `pub(crate) struct ReadOnlyClient { inner: tokio_postgres::Client }` exposing only `query`, `query_one`, `query_opt`, `copy_out` (same signatures as the `Client` methods, returning `rb_core::Result`), each calling `guard_read_only(sql)?` first. No `execute`, `batch_execute`, `copy_in`, `transaction`, `simple_query`, and no accessor returning `&Client`. `connect_read_only` returns a connection type whose `client` field is `ReadOnlyClient`; `connect_admin` keeps `tokio_postgres::Client`. Update every source-side call site (the compiler lists them). The destination's `verify()` (`lib.rs:172-188`) reads the destination through `stream_out`; give it a `ReadOnlyClient` wrapped around the admin connection's client so the same code path type-checks.
  2. `pub(crate) fn guard_read_only(sql: &str) -> rb_core::Result<()>`: (a) strip single-quoted literals (`'...'`, doubled quotes inside), dollar-quoted literals (`$tag$...$tag$`) and double-quoted identifiers, replacing them with a space; (b) strip `--` line comments and `/* */` block comments; (c) reject when a `;` remains (multi-statement); (d) trim, uppercase; the head must match `^(SELECT|WITH|SHOW|TABLE|VALUES)\b` or `^COPY\s*\(.*\)\s*TO\s+STDOUT` or `^COPY\s+\S+(\s*\([^)]*\))?\s+TO\s+STDOUT`; (e) reject when the stripped text contains any word from the deny list `INSERT UPDATE DELETE MERGE TRUNCATE CREATE ALTER DROP GRANT REVOKE COMMENT REFRESH CLUSTER VACUUM ANALYZE REINDEX LOCK SET RESET BEGIN START COMMIT ROLLBACK PREPARE CALL DO LISTEN NOTIFY UNLISTEN DISCARD SECURITY IMPORT` as a whole word, or any of the function names `nextval setval lastval pg_advisory_lock pg_advisory_xact_lock pg_try_advisory_lock lo_create lo_import lo_unlink lo_open lowrite pg_terminate_backend pg_cancel_backend pg_reload_conf pg_export_snapshot pg_create_restore_point pg_switch_wal txid_current pg_current_xact_id pg_logical_emit_message` followed by `(`. On rejection return `BackupError::Other(anyhow::anyhow!("I-IMMUT guard refused a statement on the source: {head}"))` where `head` is the first 120 characters of the original SQL. (`ANALYZE` and `SET` are denied even though they are not data writes: the source connection pins its GUCs at startup and never needs them.)
  3. Role probe at source connect (once per run, boot connection): `SELECT rolsuper OR rolcreatedb OR rolcreaterole FROM pg_roles WHERE rolname = current_user` and `SELECT EXISTS (SELECT 1 FROM information_schema.role_table_grants WHERE grantee = current_user AND privilege_type IN ('INSERT','UPDATE','DELETE','TRUNCATE'))`; when either is true emit `tracing::warn!("source role {user} can write to the source; a read-only role is recommended, see docs/IMMUTABILITY.md")`. Never refuse.
  4. Docs: `docs/modules/POSTGRES.md` "Immutability" paragraph names the typed client and the allowlist; `docs/IMMUTABILITY.md` rows for PostgreSQL get evidence IDs.
- **Unit tests:** (in `connect.rs`) `guard_accepts_select_with_show_and_values`; `guard_accepts_copy_to_stdout_table_form_and_query_form` (both shapes used by `copy_out_sql` and `table_stat`, including a `WHERE` condition and a quoted identifier named `"delete"`); `guard_rejects_each_denied_statement` (one assertion per deny word: `INSERT ... `, `UPDATE`, `DELETE`, `TRUNCATE`, `CREATE TEMP TABLE`, `ALTER`, `DROP`, `SET`, `BEGIN READ WRITE`, `CALL`, `DO $$ $$`, `COPY t FROM STDIN`, `SELECT nextval('s')`, `SELECT setval('s', 1)`, `SELECT pg_advisory_lock(1)`, `SELECT pg_export_snapshot()`, `WITH d AS (DELETE FROM t RETURNING 1) SELECT 1`); `guard_rejects_multi_statement`; `guard_ignores_literals_and_comments` (`SELECT 'DROP TABLE x' -- INSERT` accepted); `read_only_client_has_no_write_methods` (a `compile_fail` doctest or a `trybuild`-free static assertion: a private fn body calling `client.execute` must not exist — implement as a unit test that asserts `guard_read_only` is invoked by counting through a `#[cfg(test)]` counter, and rely on the type for the rest); `role_probe_query_text_is_read_only` (the probe SQL passes the guard).
- **e2e tests:** T-IMMUT-PG-LP lands in § 4.7 (least-privilege role + `rb_pg_assert_readonly_log`); until then `bash e2e/postgres_matrix.sh 16` must pass unchanged and `rb_pg_assert_readonly_log <src> postgres` must pass for the run window (already asserted since § 1.2).
- **Done:** tests green; the compiler proves no source-side file references a write method (also `bash scripts/source_readonly_lint.sh` exits 0); T-PG-ORACLE `0 fail` on 10, 16, 18; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 4.3).

### 4.3 MongoDB `ReadOnlyDatabase` with command allowlist and explicit read settings
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — type design + implementation; self-review gate.
- **Files:** `crates/rb-mongodb/src/source.rs`, `crates/rb-mongodb/src/immutability.rs`, the introspection file(s) of rb-mongodb (`ls crates/rb-mongodb/src/`), the connect/params file, `docs/modules/MONGODB.md`, `docs/IMMUTABILITY.md`.
- **Change:** add `pub(crate) struct ReadOnlyDatabase { inner: mongodb::Database }` exposing `list_collection_names`, `list_collections` (specifications), `find(collection, filter, options)` returning the cursor, `count_documents(collection, filter)`, `list_indexes(collection)`, and `run_command(doc)` that reads the first key of the document and rejects any name not in `collStats listCollections listIndexes usersInfo rolesInfo dbStats buildInfo connectionStatus hello isMaster ping serverStatus` with `BackupError::Other(anyhow!("I-IMMUT guard refused command {name} on the source"))`. `ReadOnlyClient` wrapper around `mongodb::Client` exposing `database(name) -> ReadOnlyDatabase`, `list_database_names`, and `run_admin_command` with the same allowlist. The source connect path builds `ClientOptions` with `selection_criteria = ReadPreference::Primary` and `read_concern = ReadConcern::local()` set explicitly (UNVERIFIED field names for the locked `mongodb` crate version: check `Cargo.lock` and docs.rs before writing; record in `STATE.md` §8). Replace every source-side use (source.rs, immutability.rs, introspection) so no raw `Database`/`Collection` is reachable there; `dest.rs` unchanged. Docs: `docs/modules/MONGODB.md` "Immutability" paragraph; `docs/IMMUTABILITY.md` MongoDB rows get evidence IDs.
- **Unit tests:** `run_command_rejects_write_commands` (one assertion each: `drop`, `insert`, `update`, `delete`, `create`, `createIndexes`, `dropIndexes`, `renameCollection`, `findAndModify`, `collMod`, `aggregate` with `$out`, `applyOps`, `setProfilingLevel`); `run_command_accepts_each_allowlisted_command`; `read_only_database_exposes_no_write_methods` (type-level: a test module that would call `insert_one` is not written — assert via the lint script instead); existing rb-mongodb unit tests pass.
- **e2e tests:** T-IMMUT-MONGO-LP lands in § 4.7; until then `bash e2e/mongodb_matrix.sh 7` passes unchanged (T-MONGO-MATRIX, T-MONGO-IMMUT).
- **Done:** tests green; `bash scripts/source_readonly_lint.sh` exits 0 with the rb-mongodb file list updated to every source-side file; T-MONGO-MATRIX on 4 and 8; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 4.4).

### 4.4 S3 `ReadOnlyS3` newtype and source/destination file split
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — mechanical move + type design; self-review.
- **Files:** `crates/rb-s3/src/lib.rs` (487-599 source impl, 692+ destination impl), new `crates/rb-s3/src/source.rs`, new `crates/rb-s3/src/dest.rs` (optional, only if the move makes lib.rs clearer; not required), `scripts/source_readonly_lint.sh` (remove the `SKIP rb-s3` branch), `docs/modules/S3.md`, `docs/IMMUTABILITY.md`.
- **Change:** move `S3Source` and `impl Source for S3Source` verbatim into `crates/rb-s3/src/source.rs` (`pub(crate)`), keeping `lib.rs` as the crate root with `mod source;` and the same public surface (`rb_s3::module()`); no behaviour change. Add `pub(crate) struct ReadOnlyS3 { inner: aws_sdk_s3::Client }` exposing only `list_objects_v2`, `head_object`, `get_object`, `get_object_tagging`, `get_object_acl`, `get_bucket_policy`, `get_bucket_location` (builders returned as-is). `S3Source` holds `ReadOnlyS3`; the destination keeps the raw client. Update the lint file table for rb-s3 to `crates/rb-s3/src/source.rs`. Docs: `docs/modules/S3.md` "Immutability" paragraph; `docs/IMMUTABILITY.md` S3 rows get evidence IDs.
- **Unit tests:** existing rb-s3 tests pass unchanged (the move is mechanical); `read_only_s3_builder_names_are_read_only` is not meaningful — rely on the type and the lint.
- **e2e tests:** T-IMMUT-S3-LP lands in § 4.7; until then `bash e2e/s3_minio_test.sh` passes unchanged (T-S3-MINIO, T-S3-IMMUT).
- **Done:** `cargo test -p rb-s3` green; `bash scripts/source_readonly_lint.sh` checks rb-s3 and exits 0; T-S3-MINIO passes; docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 4.5).

### 4.5 Filesystem atime guard and `--allow-atime-updates`
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — implementation + CLI plumbing; self-review.
- **Files:** `crates/rb-filesystem/src/source.rs` (`NoAtimeReader::open:141-155`, `read_noatime:119-128`), `crates/rb-filesystem/src/immutability.rs` (8-33), `crates/rb-filesystem/src/lib.rs` (params struct, `Source::fingerprint:135-137`), `crates/rust-backup/src/main.rs` (`ModuleParamArgs:154-232`, next to `--no-preserve-ownership:212-220`; `merge_params:352-366`), `docs/usage/05-filesystem.md:40` table, `docs/usage/10-variabili-ambiente.md:64` table, `docs/modules/FILESYSTEM.md`, `docs/IMMUTABILITY.md`, `docs/usage/07-sessioni-yaml.md` (params key).
- **Change:** `FilesystemParams.allow_atime_updates: bool` (default `false`). `NoAtimeReader::open(path, allow_atime_updates)`: on `EPERM` from the `O_NOATIME` attempt, when the flag is `false` return a phase-tagged error (use the phase of the caller; the first caller is the fingerprint-before run, so the error surfaces before any transfer) with message `cannot open {path} without updating its access time (O_NOATIME needs file ownership or CAP_FOWNER); run as the file owner or root, or pass --allow-atime-updates to accept atime changes on the source`; when `true`, fall back as today and emit `tracing::warn!` once per run (`atime updates on the source accepted by --allow-atime-updates`). Thread the flag through `read_noatime` and `fingerprint`. CLI: `--allow-atime-updates` (`ArgAction::SetTrue`, `env = "RUST_BACKUP_ALLOW_ATIME_UPDATES"`, help "Filesystem source: accept access-time updates when O_NOATIME is not permitted (not the file owner and no CAP_FOWNER)"), merge key `allow_atime_updates`, rejected on non-filesystem modules and on the destination role following the `--no-preserve-ownership` pattern. Docs: flag row in `docs/usage/05-filesystem.md:40` (Lato = sorgente), env row in `docs/usage/10-variabili-ambiente.md:64`, session.yml key in `07-sessioni-yaml.md`, behaviour paragraph in `docs/modules/FILESYSTEM.md` (guarantees table gains the atime row), `docs/IMMUTABILITY.md` filesystem atime row evidence T-FS-ATIME. Blockquote in this sub-phase: **behaviour change** — a non-owner run without the flag that succeeded before now fails at fingerprint-before.
- **Unit tests:** (in `crates/rb-filesystem/src/lib.rs` tests module) `noatime_open_succeeds_for_owner` (tempdir file, flag false, open ok); `noatime_eperm_is_refused_without_flag` (simulate by injecting the `Errno::EPERM` branch through a small `open_with(flags, fallback: bool)` seam, since a real EPERM needs a foreign-owned file); `noatime_eperm_falls_back_with_flag`; `allow_atime_updates_flag_is_filesystem_source_only` (clap test in `crates/rust-backup`); regression: the existing 13 rb-filesystem tests and `bash e2e/relay_smoke.sh` (owner run, unaffected).
- **e2e tests:** T-FS-ATIME — in `e2e/filesystem_matrix.sh` (§ 5.3; add the case to `e2e/filesystem_netns_test.sh` now so it runs before § 5.3 lands): as root, create a tree owned by `root` with mode `0644`, run the source as the non-root account (`runuser -u <nonroot>` pattern at `filesystem_netns_test.sh:40-54`) without the flag: exit non-zero, log contains `cannot open`, `rb_atime_manifest` of the tree unchanged, destination root absent; with `--allow-atime-updates`: success and the warning line present.
- **Done:** tests green; T-FS-ATIME passes under `sudo`; docs tables and IMMUTABILITY updated; `bash scripts/help_parity.sh` green; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 4.6).

### 4.6 Wire the read-only lint into the gates
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — mechanical; self-review.
- **Files:** `scripts/source_readonly_lint.sh` (final file table for all four modules, from § 4.2-4.5: rb-postgres `source.rs introspect.rs immutability.rs`; rb-mongodb every source-side file; rb-s3 `source.rs`; rb-filesystem `source.rs walk.rs immutability.rs`), `scripts/gates.sh` (append `bash scripts/source_readonly_lint.sh` after `scripts/crate_invariants.sh`), `docs/QA_GUIDE.md` (gate list), `CLAUDE.md` Workflow gate line (append the script name), `docs/IMMUTABILITY.md` (guard architecture: lint is a gate).
- **Change:** as listed; the lint must exit 0 on the tree after § 4.2-4.5 without weakening any regex (a legitimate hit means the code, not the lint, moves).
- **Unit tests:** none.
- **e2e tests:** T-IMMUT-LINT — `bash scripts/gates.sh` runs the lint and is green; `bash scripts/source_readonly_lint.sh --selftest` exits 0.
- **Done:** gate wired; CI job `rust-quality` (runs `gates.sh`) green on the next run; docs updated; closed in `STATE.md` (§1 -> 4.7).

### 4.7 Least-privilege end-to-end roles with server-log evidence
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — harness; self-review gate (acceptance assertions).
- **Files:** `e2e/postgres_matrix.sh`, `e2e/mongodb_matrix.sh`, `e2e/s3_minio_test.sh`, `e2e/lib.sh` (new helpers `rb_mongo_assert_readonly_log`, `rb_minio_readonly_policy`), `e2e/README.md`, `docs/IMMUTABILITY.md` (recipes section: the final privilege sets).
- **Change:**
  - PostgreSQL: after loading fixtures, create role `rb_ro LOGIN PASSWORD 'rb_ro'`; on major >= 14 `GRANT pg_read_all_data TO rb_ro`; on 10..=13 loop over every non-system schema (`SELECT nspname FROM pg_namespace WHERE nspname NOT LIKE 'pg\_%' AND nspname <> 'information_schema'`) and run `GRANT USAGE ON SCHEMA`, `GRANT SELECT ON ALL TABLES IN SCHEMA`, `GRANT SELECT ON ALL SEQUENCES IN SCHEMA`; also `GRANT CONNECT ON DATABASE`. Run the source as `rb_ro`; destination stays superuser. Assert `rb_pg_assert_readonly_log <src> rb_ro` and `rb_pg_assert_connections <src> rb_ro 3`. If the run fails for a missing read privilege, add the minimal grant, record it, and update the recipe in `docs/IMMUTABILITY.md`.
  - MongoDB: start containers with `mongod --profile 0 --slowms 0` (R9 UNVERIFIED for JSON log shape per major: verify on 4.4 and 8, record); create user `rb_ro` with roles `[{role: 'read', db: <db>}]` plus a custom role `rbView` on `admin` with actions `viewUser`, `viewRole`, `listDatabases`, `serverStatus` (extend minimally until the run passes; record the final set). Run the source as `rb_ro`. `rb_mongo_assert_readonly_log <container> <since_ts>`: parse `docker logs --since` JSON lines with `"c":"COMMAND"`; fail when `attr.command` has a first key in `insert update delete create drop createIndexes dropIndexes renameCollection findAndModify collMod applyOps setProfilingLevel`.
  - S3 (MinIO): `rb_minio_readonly_policy <alias> <bucket>`: `mc admin policy create <alias> rb-ro <json>` with `Action: ["s3:GetObject","s3:GetObjectTagging","s3:GetObjectAcl","s3:GetBucketPolicy","s3:GetBucketLocation","s3:ListBucket"]` on `arn:aws:s3:::<bucket>` and `arn:aws:s3:::<bucket>/*` (R8 UNVERIFIED action names; fallback `s3:Get*` + `s3:ListBucket`, record which); `mc admin user add <alias> rb_ro rb_ro_secret_12345`; `mc admin policy attach <alias> rb-ro --user rb_ro`; run the source with these credentials; success is the proof (any write attempt is `AccessDenied`).
  - Labels printed: `PASS T-IMMUT-PG-LP`, `PASS T-IMMUT-MONGO-LP`, `PASS T-IMMUT-S3-LP`.
- **Unit tests:** none (shell).
- **e2e tests:** T-IMMUT-PG-LP on 10, 13, 14, 18 (both grant recipes); T-IMMUT-MONGO-LP on 4 and 8; T-IMMUT-S3-LP on MinIO.
- **Done:** all three pass; final privilege sets written in `docs/IMMUTABILITY.md` and the "evidence lands in" markers removed; CI jobs `postgres`, `mongodb`, `core` (MinIO) green on the next run; ShellCheck clean; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 4.8).

### 4.8 Panic-safe after-audit in the session runner
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — lifecycle change in core; self-review gate.
- **Files:** `crates/rb-core/src/session.rs` (`run_source_limited:317-355`), `crates/rb-core/Cargo.toml` (add `futures-util = { workspace = true }` if absent), `docs/IMMUTABILITY.md` (fingerprint contract: panic path), `docs/modules/README.md` (if it documents the session contract).
- **Change:** wrap the `source_run` future at line 326 in `futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut))`. On `Err(payload)` (panic): run `source.fingerprint().await` exactly as the existing error path does (343-354); if the fingerprint differs return `BackupError::SourceMutated`, otherwise return `BackupError::Other(anyhow!("source pipeline panicked: {msg}"))` where `msg` is extracted from the payload (`&str` or `String`, else `"<non-string panic>"`). Emit `tracing::error!` with the same message. Behaviour on the success and normal-error paths is unchanged. Comment above the block: why a panic must not skip the audit (I-IMMUT on every exit path, `CLAUDE.md`).
- **Unit tests:** (in `session.rs` tests, using the in-memory channel like the existing 4 tokio tests) `panic_in_stream_out_still_runs_the_after_audit` — a mock `Source` whose `stream_out` indexes out of bounds and whose `fingerprint` increments an `AtomicUsize`; assert the runner returns `Err(BackupError::Other(..))` whose message contains `panicked`, and the counter equals 2; `panic_with_mutated_fingerprint_reports_source_mutated` — same mock, second fingerprint differs, assert `SourceMutated`.
- **e2e tests:** T-IMMUT-PANIC is the unit pair above (no e2e: a panic cannot be injected without a test hook, and the lints forbid `panic!` in production).
- **Done:** tests green; `cargo test -p rb-core` green; T-E2E0 and T-SESSION-2 pass (runner unchanged on normal paths); docs updated; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 4.9).

### 4.9 Update README.md
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `README.md`.
- **Change:** add or update a "Source safety" section: the source is opened read-only by construction, the recommended least-privilege roles (short recipes or a link to `docs/IMMUTABILITY.md`), the warning users see when the source role can write, the filesystem access-time rule and `--allow-atime-updates` (flag table at `README.md:136` gets `RUST_BACKUP_ALLOW_ATIME_UPDATES`, default `false`), and the new refusal message with its remedy in "Troubleshooting". Only shipped behaviour; no type or file names; preserve structure, tone, language; edit, do not rewrite.
- **Unit tests:** none (documentation).
- **e2e tests:** none — recipes were executed by § 4.7.
- **Done:** a user can create a read-only role for each backend and run a non-owner filesystem backup from the README alone; `bash scripts/gates.sh` green; closed in `STATE.md` with the §11 docs row for phase 4 set; §1 -> 5.1.

---

## Phase gates

- **Fmt:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings` and `bash scripts/source_readonly_lint.sh`
- **Test subset:** `cargo test --locked --all-features` then `bash scripts/gates.sh`
- **Regression guard:** T-PG-ORACLE (10, 16, 18), T-MONGO-MATRIX (4, 8), T-S3-MINIO, T-FS-OWN, T-FS-IMMUT, T-E2E0, T-SESSION-2
- **README:** source safety section, new flag and env var, troubleshooting

## Phase done criterion
`docs/IMMUTABILITY.md` lists every vector with a guard and a passing evidence test;
T-IMMUT-PG-LP, T-IMMUT-MONGO-LP, T-IMMUT-S3-LP, T-IMMUT-LINT, T-IMMUT-PANIC and T-FS-ATIME
pass; `bash scripts/gates.sh` (now including the read-only lint) is green; every module's
source-side code holds only the read-only client type. README.md reflects this phase's
shipped behavior, and `STATE.md` §11 shows this phase `DONE` with every sub-phase closed.
