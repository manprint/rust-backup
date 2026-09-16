# Phase 0 — Harness foundation (additive)

> **Intent:** land the test-matrix documents, the fixture layout, the external-oracle shell
> helpers, the flag/env inventory script and the read-only lint script. No product behaviour
> changes; nothing is wired into `scripts/gates.sh` yet.
> **Shippable alone?** yes — every deliverable is additive (new files, new shell functions).
> **Preconditions:** none

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

Conventions for every sub-phase in this file: plain technical prose, no emojis; shell
scripts start with `#!/usr/bin/env bash` and `set -euo pipefail` like `e2e/lib.sh`; they
must pass `bash -n` and ShellCheck (CI job `repository-lint`, `.github/workflows/ci.yml`).

---

## Sub-phases

### 0.1 Matrix documents
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation scaffolding; self-review.
- **Files:** new `docs/testing/POSTGRES_MATRIX.md`, new `docs/testing/FILESYSTEM_MATRIX.md`, `docs/QA_GUIDE.md` (add a link line), `e2e/README.md` (add a link line). New directory `docs/testing/` is justified: no existing directory holds test matrices (`docs/QA_GUIDE.md` is a walkthrough, `e2e/README.md` a harness overview).
- **Change:** create both documents with this structure: title, one paragraph stating that the runner (`e2e/postgres_matrix.sh`, `e2e/filesystem_matrix.sh`) prints one `PASS`/`FAIL`/`SKIP` line per row ID and that the document never records execution status, then one table per kind with columns `ID | Case | Min major | Fixture file | Expected | Oracle`. `Expected` is `round-trip` or `refused before transfer`. Fill the rows exactly as listed below (the fixture author in § 1.1 and § 5.2 implements one object group per row; object names embed the row number, e.g. table `mx.t_tab_03`).

  `POSTGRES_MATRIX.md` rows (`Min major` = 10 unless stated):
  - TAB: 01 plain heap table with all common column types (int, bigint, numeric(12,4), text, varchar(40), bool, date, timestamptz, interval, jsonb, bytea, uuid, int[], text[]); 02 UNLOGGED table; 03 table with `reloptions` (`fillfactor=70`); 04 RANGE-partitioned parent with two partitions and a DEFAULT partition (min 11); 05 LIST-partitioned parent with a sub-partitioned child (min 11); 06 classic INHERITS parent + child with rows in both; 07 table with a dropped column (attnum gap) and rows; 08 identity columns `GENERATED ALWAYS` and `BY DEFAULT` with rows beyond the seed; 09 stored generated column (min 12); 10 column with explicit `COLLATE "C"` and a column with the database default collation; 11 column default calling `nextval` of a standalone sequence; 12 zero-row table; 13 100 000-row table (bulk); 14 quoted mixed-case schema and table (`"Mixed Schema"."Weird Table"`) with a reserved-word column `"select"`; 15 fifty-column table; 16 TOAST-heavy table with 1 MiB text values (200 rows); 17 table with a table-level and column-level `COMMENT`; 18 table owned by a non-superuser role with GRANTs to two roles.
  - SEQ: 01 standalone sequence with `INCREMENT 5 MINVALUE 10 MAXVALUE 1000 CACHE 20 CYCLE`; 02 `AS smallint` sequence; 03 sequence `OWNED BY` a column; 04 sequence never called (`is_called = false`); 05 sequence advanced to its `MAXVALUE`; 06 negative-increment sequence; 07 identity-backed sequence (from TAB-08) whose current value must match after restore.
  - VIEW: 01 simple view; 02 three-level view chain; 03 view with `GROUP BY` on the primary key; 04 updatable view `WITH CHECK OPTION`; 05 view with `security_barrier`; 06 view calling a user function; 07 view over a materialized view; 08 recursive CTE view; 09 view with a `COMMENT`.
  - MV: 01 populated matview; 02 matview `WITH NO DATA` (unpopulated; must stay unpopulated); 03 matview over a matview (refresh order); 04 matview with a UNIQUE index and a non-unique index; 05 matview with `reloptions`; 06 matview over a view; 07 matview with a `COMMENT` (regression of `3723036`).
  - IDX: 01 btree; 02 unique; 03 partial (`WHERE`); 04 expression (`lower(col)`); 05 multi-column `DESC NULLS FIRST`; 06 `INCLUDE` (min 11); 07 GIN on jsonb; 08 GiST on a range column; 09 hash; 10 BRIN; 11 opclass `text_pattern_ops`; 12 storage parameter `fillfactor=50`; 13 partitioned index attached to partitions (min 11); 14 index on a matview; 15 index with `COLLATE "C"`; 16 quoted index name; 17 `NULLS NOT DISTINCT` unique index (min 15); 18 index with a `COMMENT`.
  - CON: 01 single-column PK; 02 composite PK; 03 UNIQUE; 04 FK `ON DELETE CASCADE ON UPDATE SET NULL`; 05 FK `DEFERRABLE INITIALLY DEFERRED`; 06 FK `NOT VALID` with a violating row present before `ADD CONSTRAINT ... NOT VALID` (must stay NOT VALID, violating row must round-trip); 07 FK referencing a partitioned table (min 12); 08 FK from a partitioned table (min 11); 09 self-referencing FK; 10 composite FK; 11 CHECK; 12 CHECK `NOT VALID`; 13 CHECK `NO INHERIT`; 14 EXCLUDE USING gist (needs `btree_gist`); 15 constraint `COMMENT`; 16 constraint inherited on a partition (`conislocal = false`, must not be re-added); 17 plain NOT NULL on PostgreSQL 18 (catalog row `contype = 'n'`, must round-trip) (min 18); 18 UNIQUE `NULLS NOT DISTINCT` constraint (min 15).
  - EXT: 01 `pg_trgm` with a GIN trigram index; 02 `btree_gist` (used by CON-14); 03 `hstore` with an hstore column and GIN index; 04 `citext` column; 05 `uuid-ossp` with a `uuid_generate_v4()` default; 06 extension installed in a non-public schema (`CREATE SCHEMA ext; CREATE EXTENSION hstore SCHEMA ext`); 07 custom extension `rbtest` with a config table registered via `pg_extension_config_dump` and a custom row matching the condition (round-trip of the row); 08 custom extension whose exact version is missing on the destination: refused at preflight without `--extension-version default`, restored with it.
  - GIS (only under `RB_PG_IMAGE_REPO=postgis/postgis`): 01 `CREATE EXTENSION postgis`; 02 table with `geometry(Point,4326)` and `geography` columns and rows; 03 GiST index on the geometry column; 04 custom `spatial_ref_sys` row (srid 990001) round-trips; 05 view using `ST_AsText`; 06 cross-major pair `12:16` (extension version differs: refused without the flag, restored with `--extension-version default`).
  - REF (refusals, one scratch database each, expected `refused before transfer`): 01 enum type; 02 domain; 03 composite type; 04 trigger; 05 RLS policy; 06 rule (non-view); 07 large object; 08 user collation; 09 event trigger; 10 foreign table (`postgres_fdw`); 11 aggregate; 12 column-level GRANT; 13 `ALTER DEFAULT PRIVILEGES`; 14 publication; 15 named NOT NULL constraint (min 18).

  `FILESYSTEM_MATRIX.md` rows: 01 regular files with mode 0644/0600/0400; 02 empty file; 03 empty nested directories; 04 setuid file (4755); 05 setgid file (2755); 06 setuid+setgid file (6755); 07 setgid directory (2775); 08 sticky directory (1777); 09 directory mode 0000 (root run); 10 file mode 0000 (root run); 11 directory 0500 containing a 0400 file; 12 files owned by `nobody:nogroup`; 13 file owned by a uid/gid that does not exist on the host (12345:12345); 14 relative symlink; 15 absolute symlink; 16 dangling symlink; 17 symlink to a directory; 18 symlink loop (a -> b -> a); 19 hardlink pair in the same directory; 20 hardlink triple across directories; 21 hardlink to a setuid file; 22 pre-epoch mtime (1960-01-01); 23 far-future mtime (2100-01-01) with nanoseconds; 24 FIFO; 25 char device `c 1 3` (root run); 26 block device `b 7 0` (root run); 27 unix socket: refused before transfer; 28 non-UTF-8 file name: refused before transfer; 29 tree deeper than 1024: refused before transfer; 30 sparse 64 MiB file (size and content equal; holes not preserved, documented limit); 31 xattr present with `--preserve-xattr`: refused at connect (existing behaviour); 32 non-owner source without `--allow-atime-updates`: refused before transfer; with the flag: round-trip.
- **Unit tests:** none (documentation).
- **e2e tests:** none (no behaviour change).
- **Done:** both files exist with every row above; `docs/QA_GUIDE.md` and `e2e/README.md` link to them; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 0.2, §4 ledger row, §6 `none`, §11 board).

### 0.2 Fixture layout and oracle helpers in `e2e/lib.sh`
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — shell tooling; self-review.
- **Files:** `e2e/lib.sh` (append after `rb_seed_filesystem_fixture`, line 276-306), new `e2e/fixtures/postgres/README.md`, new `e2e/fixtures/postgres/oracle_ignore.txt` (empty, header comment only), `e2e/resource_hygiene_check.sh` (only if it enumerates helper names).
- **Change:** New directory `e2e/fixtures/` is justified: fixtures are currently inline in scripts and no directory holds them. `README.md` states the naming rule `NN_<kind>[.ge<major>].sql` (loaded in lexical order; a file with suffix `.ge12` is loaded only when the server major is >= 12) and `refusals/<name>[.ge<major>].sql` (each loaded alone into a scratch database). Add to `e2e/lib.sh`:
  - `RB_PG_LOG_ARGS` (exported string): `-c log_statement=all -c log_connections=on -c log_temp_files=0 -c log_line_prefix='%u@%d %m ' -c log_min_duration_statement=-1`.
  - `rb_pg_container_ip <container>`: prints `docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}'`.
  - `rb_pg_load_fixtures <container> <major> <db> <dir>`: for each `*.sql` in `<dir>` sorted, parse an optional `.ge<N>` suffix; skip when `<major> < N` and print `SKIP <file> (needs >= N)`; otherwise `docker exec -i <container> psql -v ON_ERROR_STOP=1 -U postgres -d <db> < file`.
  - `rb_pg_oracle_schema <dump_container> <host> <port> <user> <db> <out_file>`: `docker exec <dump_container> pg_dump --schema-only --no-sync -h <host> -p <port> -U <user> <db>` piped through `sed` that deletes lines matching `^--`, `^SET `, `^SELECT pg_catalog.set_config`, `^\\restrict`, `^\\unrestrict`, blank lines, then deletes every line matching a pattern listed in `e2e/fixtures/postgres/oracle_ignore.txt` (one extended regex per line, `#` comments allowed). Password via `PGPASSWORD` env passed with `-e`.
  - `rb_pg_oracle_counts <container> <user> <db> <out_file>`: `psql -Atc` of one query producing sorted TSV lines `rel<TAB><schema.name><TAB><count><TAB><md5>` for every `pg_class` row with `relkind IN ('r','m')` that is not extension-owned (`pg_depend deptype 'e'`) or that appears in `pg_extension.extconfig`, where `<count>` = `count(*)` and `<md5>` = `md5(string_agg(md5(t::text), '' ORDER BY md5(t::text)))` computed with `FROM ONLY <qual> t` (unpopulated matviews print `unpopulated`); plus lines `seq<TAB><schema.name><TAB><last_value><TAB><is_called>` for every sequence; `con<TAB><schema.table.conname><TAB><convalidated><TAB><pg_get_constraintdef>`; `idx<TAB><schema.indexname><TAB><indexdef>`. Implement with a `DO`-free approach: one psql script that builds the per-relation query list with `format()` and executes it via `\gexec`.
  - `rb_pg_assert_readonly_log <container> <user>`: `docker logs <container> 2>&1`, keep lines whose prefix user is `<user>` and that contain `statement: ` or `execute <unnamed>: `; strip the prefix; fail (print offending line, return 1) unless the statement matches the extended regex `^(SELECT|WITH|SHOW|TABLE|VALUES)\b|^COPY[[:space:]]*\(?.*\)?[[:space:]]+TO[[:space:]]+STDOUT`; pass prints the number of statements checked.
  - `rb_pg_assert_connections <container> <user> <max>`: count lines matching `connection authorized: user=<user>`; fail when above `<max>`.
  - `rb_pg_assert_no_temp_files <container>`: fail when `docker logs` contains `temporary file:`.
  - `rb_pg_start <name> <image> <port> [extra docker args...]`: `docker run -d --name <name> -e POSTGRES_PASSWORD=postgres -p <port>:5432 <extra> <image> postgres $RB_PG_LOG_ARGS`; waits for `pg_isready` (60 s). Reuse the wait loop from `e2e/postgres_matrix.sh:50-58` if one exists there; otherwise implement it.
- **Unit tests:** none (shell). Self-check: `bash -n e2e/lib.sh`; `docker run --rm koalaman/shellcheck:v0.11.0 e2e/lib.sh` clean (same pinned image as CI).
- **e2e tests:** none yet (helpers are exercised by T-PG-ORACLE in § 1.2).
- **Done:** helpers defined and documented in `e2e/README.md` (one line each); `bash e2e/relay_smoke.sh` still passes (lib.sh unchanged for existing callers); `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 0.3).

### 0.3 Flag and environment inventory script
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — mechanical tooling; self-review.
- **Files:** new `scripts/env_inventory.sh`.
- **Change:** script builds `cargo build -q --all-features` (binary `target/debug/rust-backup`, or honours `RUST_BACKUP_BIN` when set), then for each command in the list `server`, `run`, `plan postgres`, `plan mongodb`, `plan filesystem`, `plan s3`, and `<module> <role>` for module in `postgres mongodb filesystem s3` and role in `source destination`, runs `<bin> <command> --help`, and parses each option paragraph into one TSV line `command<TAB>--flag<TAB>ENV_OR_-<TAB>default_or_-<TAB>first help sentence` (clap prints `[env: NAME=]` and `[default: X]` on the flag line or the next lines; join a paragraph before parsing). Then appends lines `code<TAB>-<TAB>VAR<TAB>-<TAB>path:line` for every direct read found by `grep -rnoE 'env::var(_os)?\("(RUST_BACKUP|BORE)_[A-Z0-9_]+"' crates/` (expected today: `RUST_BACKUP_PLAN_TIMEOUT`, `RUST_BACKUP_VERIFY_TIMEOUT`, `RUST_BACKUP_STUN_SERVERS`, `RUST_BACKUP_STUN_SERVER`, `BORE_PROXY_BUFFER_SIZE`, `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART`; `RUST_LOG` is read through `tracing_subscriber`, add it as a fixed line). Output goes to stdout, sorted, header line first. Option `--check-count <n>` exits 1 when fewer than `<n>` distinct `RUST_BACKUP_*` names were found.
- **Unit tests:** none (shell). Self-check: `bash scripts/env_inventory.sh --check-count 40` exits 0 (recon counted 40 clap `env=` names in `crates/rust-backup/src/main.rs` plus 6 direct reads).
- **e2e tests:** T-HARN-INV — `bash scripts/env_inventory.sh --check-count 40` exits 0 and the output contains `RUST_BACKUP_EXTENSION_VERSION` once § 1.5 lands (re-run then).
- **Done:** script executable, ShellCheck clean, T-HARN-INV passes; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 0.4).

### 0.4 Read-only static lint script (not yet wired)
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — mechanical tooling; self-review.
- **Files:** new `scripts/source_readonly_lint.sh`.
- **Change:** script holds a table `crate -> source-side files -> forbidden extended regexes` and greps each listed file (excluding lines inside `#[cfg(test)]` blocks is not attempted: test modules live in the same files, so the regexes must be specific to production call shapes). Initial table:
  - `crates/rb-postgres/src/{source.rs,introspect.rs,immutability.rs}`: `\.execute\(`, `\.batch_execute\(`, `\.copy_in`, `\.transaction\(`, `\.simple_query\(`, `\bconnect_admin\b`.
  - `crates/rb-mongodb/src/{source.rs,immutability.rs}` plus any other file whose name starts with `introspect` or `source`: `\.insert_(one|many)\(`, `\.update_(one|many)\(`, `\.delete_(one|many)\(`, `\.replace_one\(`, `\.find_one_and_`, `\.drop\(`, `\.create_collection\(`, `\.create_index`, `\.bulk_write\(`, `\.rename\(`, `"\$out"`, `"\$merge"`.
  - `crates/rb-filesystem/src/{source.rs,walk.rs,immutability.rs}`: `set_permissions\(`, `fchownat\(`, `chown\(`, `utimensat\(`, `remove_(file|dir)`, `create_dir`, `File::create\(`, `\.write\(true\)`, `\.create\(true\)`, `\.truncate\(true\)`, `\brename\(`, `\bsymlink\(`, `hard_link\(`, `mkfifo\(`, `mknod\(`.
  - `crates/rb-s3/src/source.rs` (file created in § 4.4; until then the script prints `SKIP rb-s3 (source.rs not split yet)`): `put_object`, `delete_object`, `create_bucket`, `put_bucket`, `put_object_tagging`, `put_object_acl`, `copy_object`, `create_multipart_upload`.
  Exit 1 and print `path:line: <match>` for every hit. Option `--selftest`: copies `crates/rb-postgres/src/source.rs` to a temp file, appends `client.execute(`, runs the check on the temp copy and asserts a hit.
- **Unit tests:** none (shell).
- **e2e tests:** T-HARN-LINT — `bash scripts/source_readonly_lint.sh` exits 0 on the current tree and `bash scripts/source_readonly_lint.sh --selftest` exits 0 (self-test detected the injected hit).
- **Done:** T-HARN-LINT passes; ShellCheck clean; script not yet in `gates.sh` (wired in § 4.6); `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 0.5).

### 0.5 Update README.md
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `README.md` (repo root).
- **Change:** No user-visible change in this phase — verify the README is still accurate and leave it unchanged; record that verification in `STATE.md` §4 (`What changed` = "README verified, no change"). Preserve the existing README structure, tone, and language.
- **Unit tests:** none (documentation).
- **e2e tests:** none.
- **Done:** verification recorded; `bash scripts/gates.sh` green; closed in `STATE.md` with the §11 docs row for phase 0 set to `DONE (no user-visible change)`; §1 -> 1.1.

---

## Phase gates

- **Fmt:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --locked --all-features` (all via `bash scripts/gates.sh`), `bash -n` + ShellCheck on every new script
- **Regression guard:** T-E2E0 (`bash e2e/relay_smoke.sh`) still passes after the `e2e/lib.sh` edit
- **README:** verified as still accurate (no user-visible change in this phase)

## Phase done criterion
`docs/testing/POSTGRES_MATRIX.md` and `docs/testing/FILESYSTEM_MATRIX.md` list every row ID
above; `e2e/lib.sh` exposes the eight helpers and `RB_PG_LOG_ARGS`; `bash
scripts/env_inventory.sh --check-count 40` and `bash scripts/source_readonly_lint.sh
--selftest` exit 0; `bash scripts/gates.sh` green; `bash e2e/relay_smoke.sh` passes.
README.md reflects this phase's shipped behavior (none), and `STATE.md` §11 shows this phase
`DONE` with every sub-phase closed.
