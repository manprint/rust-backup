# Source immutability (I-IMMUT)

The first invariant of this tool: **a backup never alters its source.** Every
run measures the source before it starts and again when it ends — including a
run that failed, was aborted, or died mid-stream — and a difference between the
two measurements ends the run as `SOURCE-IMMUTABILITY VIOLATION` (exit `6`)
instead of certifying the copy. The source side of every module is opened
read-only, and what it may ask the backend for is an allowlist, not a
convention.

"Altered" means anything a user could observe as state: rows and documents and
objects, schema and catalog, ownership and privileges, and the metadata that
belongs to the data (an object's tags, a file's mode or modification time).

Two things are deliberately **not** counted as alteration, because no reader can
avoid them:

- **Server statistics counters.** Reading a table moves
  `pg_stat_user_tables.seq_scan`, a MongoDB read moves `serverStatus` counters,
  an S3 `GetObject` appears in access logs. The fingerprint normalizes planner
  estimates away for exactly this reason.
- **Server log volume.** A run leaves lines in the source's log. The e2e suite
  reads those lines as evidence that nothing else happened.

Everything else in this document is either guarded by construction or listed as
a vector with the test that proves it closed.

## Vectors per module

`Reachable by our code?` answers one question only: whether the source side of
this repository can reach the vector at all. `no (by type)` means the source
holds a wrapper that does not expose the call; `no (by guard)` means the call
exists but a run-time allowlist refuses it; `accepted` means it happens and is
documented as harmless.

### PostgreSQL

| Vector | Reachable by our code? | Guard | Evidence |
|--------|------------------------|-------|----------|
| Any write method of the driver (`execute`, `batch_execute`, `copy_in`, `transaction`, `simple_query`) | no (by type) | the source side holds `ReadOnlyClient`, which exposes only `query`, `query_one`, `query_opt` and `copy_out` and hands the inner `tokio_postgres::Client` to nobody | `read_only_client_has_no_write_methods`, T-IMMUT-LINT |
| `INSERT` / `UPDATE` / `DELETE` / `MERGE` / `TRUNCATE` | no (by guard) | statements are built from catalog names, so each one passes `guard_read_only` first: the head must be `SELECT` / `WITH` / `SHOW` / `TABLE` / `VALUES` / `COPY … TO STDOUT` and no writing keyword may appear outside a literal. The session also carries `default_transaction_read_only=on` | `guard_rejects_each_denied_statement`, T-PG-IMMUT, IMMUT-READONLY-LOG (matrix) |
| DDL (`CREATE` / `ALTER` / `DROP` / `COMMENT`) | no (by guard) | same allowlist; all DDL lives in the destination module | `guard_rejects_each_denied_statement`, T-PG-IMMUT, IMMUT-READONLY-LOG |
| A second statement smuggled into one string (`SELECT 1; DROP …`) | no (by guard) | the guard refuses any `;` left after literals, dollar-quoted bodies, quoted identifiers and comments are removed | `guard_rejects_multi_statement`, `guard_ignores_literals_and_comments` |
| `CREATE TEMP TABLE` and other temp objects | no (by guard) | never issued, refused by the allowlist, and a read-only transaction refuses it at the server | `guard_rejects_each_denied_statement`, IMMUT-READONLY-LOG |
| `nextval` / `setval` / `lastval` | no (by guard) | sequence values are read from `pg_sequence` and `pg_sequences`, never advanced; the three functions are on the guard's deny list | `guard_rejects_each_denied_statement`, T-PG-ORACLE (sequence rows compared) |
| Advisory locks (`pg_advisory_lock`, …) | no (by guard) | never issued; nothing in the source path needs cross-session coordination | `guard_rejects_each_denied_statement`, IMMUT-READONLY-LOG |
| Large-object writes (`lo_import`, `lo_create`, `lowrite`, …) | no (by guard) | never issued; on the guard's function deny list | `guard_rejects_each_denied_statement` |
| Backend control (`pg_terminate_backend`, `pg_cancel_backend`, `pg_reload_conf`) | no (by guard) | never issued; on the guard's function deny list | `guard_rejects_each_denied_statement` |
| `LISTEN` / `NOTIFY` / `UNLISTEN` | no (by guard) | never issued; on the deny list | `guard_rejects_each_denied_statement`, IMMUT-READONLY-LOG |
| `SET` / `RESET` of a session GUC | no (by guard) | the session pins every GUC it needs in the startup options and never changes one afterwards | `guard_rejects_each_denied_statement`, `read_only_options_include_lock_and_statement_timeouts` |
| `ANALYZE` / `VACUUM` / `REINDEX` / `CLUSTER` | no (by guard) | never issued; on the deny list, because they write catalog statistics or rewrite storage | `guard_rejects_each_denied_statement` |
| `pg_export_snapshot` | no (by guard) | the source does not open a snapshot-exporting transaction; every statement is its own implicit transaction | `guard_rejects_each_denied_statement`, IMMUT-READONLY-LOG |
| A source role that *could* write (superuser, `INSERT` grants) | accepted, warned | the role is probed once per run and a warning names it; the tool never refuses a role the operator chose | `role_probe_query_text_is_read_only`, T-IMMUT-PG-LP |
| Spill to the source's `pgsql_tmp` | accepted, reduced | the fingerprint no longer sorts (order-independent commitment, § "Fingerprint contract"); the data stream still sorts by the C-collated row text, which the destination read-back depends on | NOTEMP (matrix, reporting), T-PG-FP2 |
| Prepared transactions (`PREPARE TRANSACTION`) | no (by guard) | never issued; `PREPARE` is on the deny list | `guard_rejects_each_denied_statement`, IMMUT-READONLY-LOG |
| Replication slots / `pg_switch_wal` / restore points | no (by guard) | the module is logical and never touches WAL; the functions are on the deny list | `guard_rejects_each_denied_statement`, IMMUT-READONLY-LOG |
| `BEGIN READ WRITE` overriding the read-only default | no (by guard) | the source path issues no transaction-control statement at all, and the allowlist refuses every one of them | `guard_rejects_each_denied_statement`, runtime-model review item (j), `docs/modules/POSTGRES.md` |
| Statistics counters (`pg_stat_*`) | accepted | reading moves them; the fingerprint normalizes planner estimates so autovacuum and `ANALYZE` cannot look like user mutation | `catalog_hash_ignores_estimate_drift_but_not_structure` |
| `ACCESS SHARE` locks held while `COPY` runs | accepted | a plain read lock. It does not change data, but it does block a concurrent `ALTER TABLE` / `DROP` for the duration of the copy, and a `lock_timeout` of 30 s bounds the opposite direction | T-PG-TIMEOUT |

### MongoDB

| Vector | Reachable by our code? | Guard | Evidence |
|--------|------------------------|-------|----------|
| Any write method of the driver (`insert_*`, `update_*`, `delete_*`, `replace_one`, `find_one_and_*`, `bulk_write`, `create_collection`, `create_index`, `drop`) | no (by type) | the source side holds `ReadOnlyClient`/`ReadOnlyDatabase`, which name the collection per call and never hand out a `Collection` or a raw `Database` | `read_only_database_exposes_no_write_methods`, T-IMMUT-LINT |
| `insert` / `update` / `delete` / `findAndModify` as a raw command | no (by guard) | `run_command` reads the command's first key and refuses anything outside the read allowlist (`collStats`, `listCollections`, `listIndexes`, `usersInfo`, `rolesInfo`, `dbStats`, `buildInfo`, `connectionStatus`, `hello`, `isMaster`, `ping`) | `run_command_rejects_write_commands`, T-MONGO-IMMUT |
| `create` / `drop` / `renameCollection` / `collMod` | no (by type and guard) | destination-only, and refused by the command allowlist | `run_command_rejects_write_commands`, T-MONGO-IMMUT |
| Index builds (`createIndexes` / `dropIndexes`) | no (by type and guard) | the source reads index specifications through `list_indexes` | `run_command_rejects_write_commands`, T-MONGO-IMMUT |
| Aggregation with `$out` / `$merge` | no (by guard) | the source runs no aggregation pipeline, and `aggregate` is not on the allowlist whatever its pipeline says | `run_command_rejects_write_commands`, T-MONGO-IMMUT |
| `applyOps`, `dropDatabase`, `killOp` and other admin mutations | no (by guard) | outside the allowlist | `run_command_rejects_write_commands` |
| Profiler collection writes (`setProfilingLevel`, `system.profile`) | no (by guard) | never issued and outside the allowlist; the e2e runs the server with profiling off so its log is evidence | `run_command_rejects_write_commands`, T-IMMUT-MONGO-LP |
| Reading a lagging secondary, which looks like source drift | no | the source client pins `readPreference=primary` and `readConcern=local`, overriding whatever the operator's URI asked for | `read_only_options_pin_primary_and_local` |
| Read commands used (`collStats`, `listCollections`, `listIndexes`, `usersInfo`) | yes, read-only | the allowlist accepts exactly these and refuses everything else | `run_command_accepts_each_allowlisted_command` |

### S3

| Vector | Reachable by our code? | Guard | Evidence |
|--------|------------------------|-------|----------|
| `PutObject` / multipart upload | no (by type) | the source holds `ReadOnlyS3`, a newtype over the SDK client with a method per read operation, none for a write, and no accessor for the inner client; uploads live in the destination | T-IMMUT-LINT, T-S3-IMMUT |
| `DeleteObject` / `DeleteObjects` | no (by type) | destination-only, and absent from `ReadOnlyS3` | T-IMMUT-LINT, T-S3-IMMUT |
| `CopyObject` | no (by type) | absent from `ReadOnlyS3` | T-IMMUT-LINT |
| `PutObjectTagging` / `PutObjectAcl` | no (by type) | the source only reads tags and ACLs (`get_object_tagging`, `get_object_acl`) | T-IMMUT-LINT, T-S3-IMMUT |
| `PutBucketPolicy` / `PutBucketVersioning` | no (by type) | the source only reads the bucket policy and its versioning status | T-IMMUT-LINT, T-S3-IMMUT |
| `CreateBucket` | no (by type) | destination-only | T-IMMUT-LINT, T-S3-IMMUT |
| A write reached from a read-side file through a raw SDK client | no | the whole read side is `crates/rb-s3/src/source.rs`, which the lint greps for every writing operation name; the lint now fails if that file disappears | T-IMMUT-LINT |
| Read calls used (`ListObjectsV2`, `HeadObject`, `GetObject`, `GetObjectTagging`, `GetObjectAcl`, `GetBucketPolicy`, `GetBucketVersioning`) | yes, read-only | the methods `ReadOnlyS3` exposes, which are these seven plus `GetBucketLocation` — carried on the wrapper so that a read nobody issues today still cannot arrive from a raw client, and consequently absent from the least-privilege policy below; a read-only credential is the deployment guard on top: any write is `AccessDenied` at the service | T-IMMUT-S3-LP |

### Filesystem

| Vector | Reachable by our code? | Guard | Evidence |
|--------|------------------------|-------|----------|
| Access time (atime) | no, unless allowed | files are opened `O_RDONLY | O_NOATIME`; when the kernel refuses that (not the owner, no `CAP_FOWNER`) the run fails during the first fingerprint — before any transfer — unless `--allow-atime-updates` (`RUST_BACKUP_ALLOW_ATIME_UPDATES`, `allow_atime_updates` in YAML) accepts the change, which also logs `atime updates on the source accepted by --allow-atime-updates` once | `noatime_eperm_is_refused_without_flag`, `noatime_eperm_falls_back_with_flag`, `noatime_open_succeeds_for_owner`, T-FS-ATIME |
| Content, mode, ownership, mtime | no | the source opens read-only and never writes; the destination writes only under its own root | T-FS-IMMUT |
| Change time (ctime) | no | nothing in the source path changes an inode | T-FS-IMMUT |
| Extended attributes | no | read-only; xattrs are copied, never written on the source | T-FS-IMMUT |
| Advisory locks (`flock`, `fcntl`) | no | never taken | T-FS-IMMUT |
| `mmap` writes | no | files are read with ordinary `read` calls | T-FS-IMMUT |
| Symlink following outside the root | no | the walk resolves entries under a checked root and copies symlinks as symlinks | T-FS-OWN |
| Temporary files on the source | no | nothing is staged anywhere (I-NOTEMP) | T-HYGIENE |

## Fingerprint contract

One measurement before the run, one after — on every exit path. What is hashed
differs per module, because "the same state" means something different for each
backend:

| Module | What is hashed | Cost per measurement | On drift |
|--------|----------------|----------------------|----------|
| PostgreSQL | the structural catalog (planner estimates normalized out) plus, per data-bearing table, the row count and an order-independent 128-bit commitment (the wrapping sum of `md5(row)`); prefix `rust-backup/pg-fingerprint/v2` | one full read of every table, streamed and folded client-side — no server sort, no temporary file | `SourceMutated`, exit `6`; a destination that already applied the payload drops the databases it created |
| MongoDB | per collection: `count_documents` and an `_id`-ordered content checksum, plus the catalog; prefix `rust-backup/mongo-fingerprint/v1` | one full read of every collection | `SourceMutated`, exit `6` |
| S3 | the object listing (key, size, etag, metadata) and the bucket policy; prefix `rust-backup/s3-fingerprint/v2` | one listing; object bodies are not re-read | `SourceMutated`, exit `6` |
| Filesystem | per entry: path, kind, size, mode, uid, gid, mtime (with nanoseconds), link target, hardlink target, device number, and the file's contents | one full read of the tree — so a run reads the source three times in total (baseline, stream, audit), which is the dominant cost on large trees | `SourceMutated`, exit `6` |

Two properties matter as much as the content:

- **It runs on failure too.** An aborted transfer is exactly where a
  half-written source would hide, so the audit runs on the error path as well as
  on success.
- **It runs after a panic.** A panic in the source pipeline must not be a way to
  skip the audit. `rb_core::session::run_source_limited` runs the whole source
  pipeline inside `catch_unwind`, so an unwind becomes an ordinary error and
  takes the same failure path: the source is fingerprinted again and the run
  reports `SourceMutated` if it drifted, otherwise
  `source pipeline panicked: <message>` (exit `1`). The destination is aborted
  either way, so a panic cannot leave a certified copy behind
  (T-IMMUT-PANIC: `panic_in_stream_out_still_runs_the_after_audit`,
  `panic_with_mutated_fingerprint_reports_source_mutated`).

## Least-privilege deployment

The guards above are properties of this code. These recipes make the *server*
refuse a write even if the code were wrong.

### PostgreSQL 14 and later

```sql
CREATE ROLE rb_ro LOGIN PASSWORD 'change-me';
GRANT pg_read_all_data TO rb_ro;
GRANT CONNECT ON DATABASE app TO rb_ro;
```

### PostgreSQL 10 to 13

`pg_read_all_data` does not exist yet, so grant per schema — and remember that a
schema created after the grant needs it again:

```sql
CREATE ROLE rb_ro LOGIN PASSWORD 'change-me';
GRANT CONNECT ON DATABASE app TO rb_ro;
-- for every non-system schema:
GRANT USAGE ON SCHEMA public TO rb_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO rb_ro;
GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO rb_ro;
```

The role needs no superuser attribute, no `CREATEDB` and no `CREATEROLE`. The
run warns when the source role has any of them, or any write grant, because a
guard you can verify beats a guard you trust: `warn_if_role_can_write`
(`crates/rb-postgres/src/connect.rs`) probes the role once per run and logs
`source role <user> can write to the source; a read-only role is recommended`.
The recipe above is the one `T-IMMUT-PG-LP` runs, on 10, 13, 14, 16 and 18.

### MongoDB

A `read` role on each copied database, plus `viewUser`/`viewRole` on it so the
plan can record the database's user inventory. Nothing cluster-wide: the source
never runs `listDatabases` or `serverStatus`, and `buildInfo`, `hello` and `ping`
need no privilege at all.

```javascript
use admin
db.createRole({
  role: "rbView",
  privileges: [
    { resource: { db: "app", collection: "" }, actions: ["viewUser", "viewRole"] }
  ],
  roles: []
})
db.createUser({
  user: "rb_ro",
  pwd: passwordPrompt(),
  roles: [{ role: "read", db: "app" }, { role: "rbView", db: "admin" }]
})
```

Without `rbView` the backup still runs: the user probe is best-effort and the
plan then records no users, which is a silent gap rather than an error. Point the
tool at this user with `--user rb_ro --auth-db admin` and the password in
`RUST_BACKUP_PASSWORD` (T-IMMUT-MONGO-LP runs exactly this recipe and reads the
server's own command log back).

### S3 / MinIO

A credential whose policy allows only the read actions the source issues:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:ListBucket", "s3:GetBucketPolicy", "s3:GetBucketVersioning"],
      "Resource": ["arn:aws:s3:::app-bucket"]
    },
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:GetObjectTagging", "s3:GetObjectAcl"],
      "Resource": ["arn:aws:s3:::app-bucket/*"]
    }
  ]
}
```

`s3:GetBucketVersioning` is not optional: the refusal of versioned buckets is
decided on it, so a backup cannot start without it. `s3:GetObject` also covers
`HeadObject`. On **MinIO**, drop `s3:GetObjectAcl` — MinIO rejects it as an
unsupported action and authorizes `GetObjectAcl` under `s3:GetObject`; that
5-action policy is what `rb_minio_readonly_policy` in `e2e/lib.sh` installs for
T-IMMUT-S3-LP, which then reads MinIO's own API trace back and checks that the
same credential is refused a write. Losing `s3:GetBucketPolicy` is tolerated by
the tool — the plan simply records no policy — which makes it a silent fidelity
loss, so it belongs in the recipe. Pass the keys through
`RUST_BACKUP_ACCESS_KEY` / `RUST_BACKUP_SECRET_KEY`, never as flags.

### Filesystem

Run as the owner of the tree, or as root. Either lets the source open files with
`O_NOATIME`, so a backup leaves no trace at all; otherwise the run fails and
has to be told explicitly that moving access times is acceptable, with
`--allow-atime-updates` (`RUST_BACKUP_ALLOW_ATIME_UPDATES`, or
`allow_atime_updates: true` in a session file).

## Guard architecture

Four layers, each with its own failure mode, so no single mistake removes the
invariant:

1. **Types.** The source side of each module holds a read-only wrapper that does
   not expose a write call, so a write is a compile error rather than a run-time
   accident: `ReadOnlyClient` (PostgreSQL), `ReadOnlyClient`/`ReadOnlyDatabase`
   (MongoDB) and `ReadOnlyS3` (S3).
2. **Allowlists.** Where the backend takes a statement or a command name as
   data, the text is checked before it is sent: a statement head that is not
   `SELECT` / `WITH` / `SHOW` / `TABLE` / `VALUES` / `COPY … TO STDOUT`, or a
   command name outside the read list, is refused with a named error
   (`I-IMMUT guard refused …`). PostgreSQL's `guard_read_only` and MongoDB's
   `guard_read_command` are both in place.
3. **A lint in the gates.** `scripts/source_readonly_lint.sh` greps every
   source-side file — the wrappers included — for write calls, so a new call
   site is caught even if it type-checks. It runs inside `scripts/gates.sh`,
   together with its own `--selftest`: a lint that can no longer detect an
   injected write fails the gate instead of passing quietly (T-IMMUT-LINT).
4. **Server-side evidence.** The e2e suite runs each source under the
   least-privilege identity documented above — so the server would refuse a
   write anyway — and then reads the *server's* own record of the session back
   to prove none was attempted: `rb_pg_assert_readonly_log` over the PostgreSQL
   log (`log_statement=all`), `rb_mongo_assert_readonly_log` over a mongod
   started with `--profile 0 --slowms 0`, and
   `rb_minio_assert_readonly_trace` over `mc admin trace --json`. Each
   assertion also fails when its window is empty, because silence is not
   evidence; the MongoDB one classifies `aggregate` by its pipeline, since the
   driver implements `count_documents` as a read-only aggregate.

Test IDs: `T-PG-IMMUT`, `T-MONGO-IMMUT`, `T-S3-IMMUT`, `T-FS-IMMUT`,
`T-FS-OWN`, `T-PG-FP2`, `T-PG-TIMEOUT`, `T-FS-ATIME`, `T-IMMUT-PG-LP`,
`T-IMMUT-MONGO-LP`, `T-IMMUT-S3-LP`, `T-IMMUT-LINT`, `T-IMMUT-PANIC`. The
matrix documents `docs/testing/POSTGRES_MATRIX.md` and
`docs/testing/FILESYSTEM_MATRIX.md` carry the per-row detail.
