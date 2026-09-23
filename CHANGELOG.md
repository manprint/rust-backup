# Changelog

## Unreleased

### The waiting source shows what the destination is doing

- **While it waits for the verdict, the source prints the destination's own
  progress line**, for every module: `waiting: payload sent (2 items, 952.72
  MiB), 7s in this stage; destination: verify: reading the restored tables
  back, items 0/2  619.00 MiB/952.72 MiB  (65.0%)  309.59 MiB/s, 3s in this
  stage`. Before, the source only said it was waiting, through minutes of index
  builds and read-back on the other side.
- New control frame `PeerProgress { stage, line }`, sent by the destination
  every 2 seconds on the payload-free control stream, negotiated through a new
  `peer_progress` field in `Plan` and `PlanAck`. A peer from an earlier release
  neither sends nor receives it; the run works as before.

## 0.0.14 — prerelease (2026-09-23)

### Progress lines name the stage and keep moving after the transfer

- **Every progress line starts with the stage the run is in** — `connecting`,
  `audit`, `analyze`, `preflight`, `transfer`, `finalize`, `verify`, `waiting` —
  and ends with the time spent in it. A PostgreSQL restore of 1.03 GiB spent
  1m45s building indexes and constraints and 20 s reading the data back while
  its line stayed at `items 1462/1462  1.03 GiB/1.41 GiB  (73.2%)`, with a rate
  that kept falling; the source side printed the same frozen line.
- **The read-back is now progress:** `verify: … items 384/1462  170.45
  MiB/1.03 GiB  (16.4%)` for every module, after a `comparing the restored
  catalog with the plan` step on PostgreSQL. PostgreSQL's post-data work is
  `finalize: building indexes and constraints, refreshing materialized views,
  step 120/7800`. The source says `waiting: payload sent …` until the
  destination's verdict, and `audit:` while fingerprinting.
- **The rate is the last 5 seconds**, not the average since start; the byte
  total is marked `~` while it is the plan's estimate and replaced by the
  actual total when the transfer ends. The final line reads `done: …  average,
  <duration> in total` or `failed during <stage>: …`.

## 0.0.13 — prerelease (2026-09-23)

### PostgreSQL: views that do not survive their own round-trip restore verified

- **Fix: a same-major restore of Odoo failed `[Verify]` (exit `5`) on its
  report views.** PostgreSQL renders `state IN ('a','b')` on a `varchar` column
  as `ARRAY['a'::character varying, 'b'::character varying]::text[]`, and the
  restored copy — re-created from that very text — as `ARRAY['a'::character
  varying::text, 'b'::character varying::text]`. The read-back compared the
  source text verbatim whenever both servers ran the same major, so the three
  views `purchase_bill_union`, `report_all_channels_sales` and
  `report_stock_quantity` always differed. Every view is now re-rendered on the
  destination through a temporary view before comparing, as cross-major runs
  already did; the comparison stays exact against the destination's rendering.
- New matrix row M-PG-VIEW-10 restores such a view on every major 10..18 and in
  the cross-version pairs.

## 0.0.12 — prerelease (2026-09-23)

### PostgreSQL read-back failures show where the texts differ

- **A long text that differs is reported at its first differing character.**
  The read-back used to print the first 240 characters of each side, so the
  three Odoo report views that failed `[Verify]` (exit `5`) showed two
  identical prefixes. It now prints `differs at character N (lengths source=…
  destination=…)` with about 80 characters of each side around that point.
- **The report says how view definitions were compared.** A `note:` line names
  the mode — verbatim when both servers run the same major, re-rendered through
  a temporary view on the destination across majors — and lists every view
  whose re-render failed with the server's reason, which used to be only a
  `WARN` line in the destination log.

## 0.0.11 — prerelease (2026-09-23)

### `--hot-backup`: copy a PostgreSQL source that stays online

- **New flag `--hot-backup` (`RUST_BACKUP_HOT_BACKUP`, YAML `hot_backup: true`),
  PostgreSQL only, on both peers.** A source whose application keeps writing —
  Odoo's cron jobs, queues and sessions never stop — failed every run with
  `SOURCE-IMMUTABILITY VIOLATION` (exit `6`), because the audit compares the
  whole cluster before and after the copy.
- **The copy is one snapshot, not a relaxed check.** Each source connection
  opens `BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY` before its first read
  and keeps it for the run, as `pg_dump` does, so the catalog, the sequence
  values, the row counts and every `COPY` describe one instant. Without it a
  written source restores children whose parent was never read, sequences
  behind the ids already copied, and row counts the destination rejects — the
  cold run of the new e2e fails its integrity check exactly that way.
- **The immutability audit is skipped, and says so.** The source prints `BACKUP
  VERIFIED (hot backup): destination read-back matches the source snapshot; the
  online source was not audited`, never `source unchanged`. The destination's
  verification is unchanged and exact against the snapshot, and prints `RESTORE
  VERIFIED (hot backup)`.
- **Both operators consent.** The plan is `mode=HotSnapshot`; a destination
  without its own `--hot-backup` fails it at preflight (exit `3`) before writing
  anything, and an older build cannot decode it. On any other module the flag is
  a configuration error (exit `2`).
- Cost, documented in `docs/usage/03-postgres.md`: the snapshot holds back
  vacuum and keeps `ACCESS SHARE` on every table read until the run ends, so DDL
  on them waits for the backup. A whole-cluster run is consistent per database.
- Proved by `e2e/postgres_hot_backup.sh` (CI job `postgres-hot-backup`).

## 0.0.10 — prerelease (2026-09-22)

### Static musl archives: the binary runs on Alpine

- **The release now ships `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` archives, statically linked.** The `-linux-gnu`
  archives are linked dynamically against glibc 2.34+, and on Alpine (musl) their
  program interpreter `/lib64/ld-linux-x86-64.so.2` does not exist: bash reports
  `cannot execute: required file not found`, busybox `sh` reports `not found`.
  The musl binary has no interpreter and no shared-library dependency, so it also
  runs on distributions older than glibc 2.34.
- `fast-release.yml` builds the musl targets natively inside `rust:alpine`,
  fails the job if the binary carries an `INTERP` header, and runs `--version`
  in a bare `alpine` container before the archive is published.

## 0.0.9 — prerelease (2026-09-22)

### The plan item ceiling is 200 000, and it is now reachable

- **`MAX_PLAN_ITEMS` raised from 100 000 to 200 000.** One plan item per file,
  collection, object or table, so this is the number of restore units a single
  run can carry.
- **The plan frame got a bound of its own, and that is what made the ceiling
  real.** The plan crosses the wire as ONE frame, and every frame was bounded by
  `FRAME_LIMIT` (16 MiB). A filesystem tree encodes to roughly 210 bytes per
  entry and an S3 bucket to roughly 340 — so the old 100 000-item ceiling was
  already unreachable: a run died somewhere north of 60 000 entries with a
  generic `frame exceeds FRAME_LIMIT`, never naming the item count the operator
  could act on. `wire::PLAN_FRAME_LIMIT` (128 MiB) now bounds the plan frame
  alone, sized at about twice the fattest realistic item shape. Ordinary frames
  keep the 16 MiB bound. The larger read happens exactly once, at a known point
  in the protocol, after both sides are paired.
- **A test ties the two constants together.** `plan_frame_budget_covers_the_item_ceiling`
  builds a plan of `MAX_PLAN_ITEMS` S3-shaped items and fails if it does not fit
  the frame bound — raising one constant without the other is now a red test,
  not a field failure.
- **`rust-backup plan` enforces the ceiling too.** The dry-run's documented
  promise is that it fails exactly as the transfer would; it was printing plans
  a real run would refuse. It now validates the plan bounds before rendering.

### Costs of the higher ceiling

A destination decoding a plan at the new ceiling holds more: the frame buffer
plus the decoded items. For the fattest module (S3, whose item meta carries
etag, storage class and content type) that is roughly 65 MiB of frame at
200 000 items. The bound against a hostile plan moves with it — deliberately,
and confined to the single plan read.

## 0.0.8 — prerelease (2026-09-22)

A review pass over the four modules and the session core. Everything here is a
defect found by reading the code against its own invariants, not a new feature:
three of them abort a healthy run, and the rest are limits the program enforced
without ever telling the operator they existed.

### Runs that failed while working exactly as designed

- **The 30-second idle timeout no longer fights the backpressure invariant.**
  Every payload write rides the substream's flow-control window, and
  I-BANDWIDTH says a slow destination MUST stall it — the destination stops
  reading while it applies what it already holds. One S3 `upload_part` of a
  whole multipart part, one MongoDB `insert_many` batch, one PostgreSQL `COPY`
  flush waiting on a lock, and the window stayed full past 30 s: the run died
  with `write idle timeout` for doing what the design requires. A 500 MiB S3
  part over a 100 Mbit/s uplink needs ~42 s of it. Payload I/O now uses a
  separate 600 s bound (`RUST_BACKUP_IO_IDLE_TIMEOUT`), while the handshake
  waits keep the short one; channel liveness was never resting on this bound
  anyway — the control heartbeat (20 s) and the coordination server's reaper
  (60 s) prove it far sooner.
- **MongoDB source cursors are opened with `noCursorTimeout`.** The cursor
  advances only once the destination has taken the previous chunk, and
  `--max-rate` stalls it further, so a destination slower than the server's
  `cursorTimeoutMillis` (10 minutes by default) used to kill the backup with
  `CursorNotFound` on a source nothing had touched.
- **A root process without `CAP_CHOWN` is no longer told it has it.** The
  filesystem capability probe answered "effective uid 0, therefore yes" without
  reading `/proc/self/status`, so a restore in a `--cap-drop=CHOWN` container
  passed the `ownership` preflight, streamed the whole payload, and only then
  failed with `EPERM` on the first `fchownat` — exactly the outcome that check
  exists to prevent. The uid now survives only as the fallback for a host with
  no readable `/proc`.
- **A file read interrupted by a signal is retried.** `nix`'s `read` surfaces
  `EINTR`, which `std`'s reader handles for you; the filesystem source turned it
  into a failed backup.

### Limits that are now stated instead of discovered

- **The 100 000-item plan ceiling is enforced by the source**, which names it,
  instead of arriving from the peer as a `Connect` failure after the whole
  backend had been fingerprinted, analysed and sent. It is documented in all
  four module guides: an ordinary system root already exceeds it, so the
  filesystem module is for a data tree, not for `/`.
- **S3 preflight prints the destination's real memory peak.** A multipart part
  is assembled whole in memory and its size grows with the object to stay under
  S3's 10 000-part limit — about 524 MiB for an object near the 5 TiB ceiling.
  The `multipart-memory` check states it before the plan is accepted.
- **The source is read three times per run** — fingerprint, transfer,
  fingerprint — and the destination read-back adds a fourth on its own side.
  I-IMMUT requires both audits, including on a failed run; the module guides now
  say so, where only the PostgreSQL one mentioned the cost at all.

### Speed

- **S3 analysis issues its per-object probes 16 at a time.** `HeadObject`,
  `GetObjectTagging` and `GetObjectAcl` per object, plus a re-listing for each of
  the two fingerprints, came to roughly five million *sequential* round-trips for
  a million-object bucket. Every probe is still a read and the plan is
  byte-identical to what the serial loop produced.

### Diagnostics

- **A dying control plane says so.** A failed client heartbeat, a closed control
  substream and a coordination-server send that times out each emit a line
  naming the side that stopped; the only previous evidence was the reaper's
  message 60 s later, which names the symptom (I-OBSERV).

## 0.0.7 — prerelease (2026-09-16)

The hardening plan (`docs/plans/001_plan-Hardening/`), phases 0 to 6: what each
module refuses to approximate, what the destination proves before it reports a
verified copy, and the gates that keep the documentation honest about both.

### PostgreSQL fidelity

- **Extension configuration tables are user data again.** Relations registered
  with `pg_extension_config_dump` are captured and restored as their own plan
  items, loaded over a scope cleared by the extension's own condition. A PostGIS
  cluster's custom `spatial_ref_sys` rows now survive a restore instead of being
  silently replaced by the extension's defaults.
- **An object owned by an extension no longer refuses the cluster.** PostGIS
  defines three rules on `public.geometry_columns`, which made every PostGIS
  cluster unbackupable; `CREATE EXTENSION` recreates them verbatim on the
  destination.
- **A plan item this build cannot restore is refused, at preflight and on the
  apply path**, rather than skipped and then reported as a verified copy.
- **Extension versions are preflighted** against
  `pg_available_extension_versions`, and a version the destination cannot
  install is named along with what it does have. `--extension-version
  <source|default>` (`RUST_BACKUP_EXTENSION_VERSION`, `extension_version:`)
  chooses; under `default` the report carries `deviation: extension <name>
  restored at version <actual> (source <version>)`.
- View and materialized-view relation options are captured and emitted,
  unpopulated materialized views stay unpopulated, comments on constraints and
  indexes round-trip, and the PostgreSQL 18 named-`NOT NULL` probe compares
  against the server's own default name.

### What the destination proves

- **Row counts.** The source counts the rows of every data item and of every
  populated materialized view during Analyze; the destination compares them with
  what `COPY` wrote and `REFRESH` produced, and fails closed on a difference.
- **Constraint state.** `convalidated`, `condeferrable` and `condeferred` are
  captured, restored verbatim and compared; a plan whose constraint state and
  definition text disagree is refused.
- **The proof is printed.** `RESTORE VERIFIED` gained `rows verified: …`,
  `constraints: …` and one `deviation: …` line per declared departure, and the
  same facts leave the module as structured fields.

### Runtime and failure behaviour

- **One pooled read-only connection per database** for every phase of a run: a
  single-database backup now holds one connection where it used to open about
  nine.
- **`lock_timeout=30s` on source sessions and TCP keepalives on both sides.** A
  table held under `ACCESS EXCLUSIVE` now fails the run in 30 s with the
  server's own message instead of stalling it indefinitely.
- **Fingerprint v2** — an order-independent 128-bit commitment per table, folded
  client-side from a streamed `md5(row)`. It replaces the sorted checksum and its
  second `COUNT(*)`: the immutability audit no longer sorts and no longer spills
  to disk.
- **A run that fails after the payload landed cleans up after itself.**
  `Destination::abandon()` (defaulted, so other modules are unchanged) removes
  the databases the run created when read-back fails or the source rejects
  completion.
- Two phase tags were wrong: counting a materialized view's rows and the item
  framing check are Apply, not Verify.

### Source immutability

I-IMMUT stops being a promise the code makes to itself and becomes four
independent layers, each with its own failure mode. `docs/IMMUTABILITY.md` is
the reference — every vector with its guard and the test that proves it, the
fingerprint contract per module, and the least-privilege recipe for each
backend.

- **Types.** The source side of every module holds a read-only wrapper with no
  write call to offer: `ReadOnlyClient` over `tokio_postgres::Client`,
  `ReadOnlyClient`/`ReadOnlyDatabase` over the MongoDB driver (collections are
  named per call, so no `Collection` or raw `Database` escapes), and
  `ReadOnlyS3` over the AWS SDK client.
- **Allowlists.** Where a backend takes a statement or a command as data, the
  text is checked before it is sent — `guard_read_only` for PostgreSQL (literal
  and comment stripping, single statement, head allowlist, deny words and deny
  functions) and an 11-command read list for MongoDB, which also pins
  `readPreference=primary` and `readConcern=local`. The PostgreSQL source role is
  probed once per run, and a role that could write is warned about, never
  refused.
- **A lint in the gates.** `scripts/source_readonly_lint.sh` and its
  `--selftest` run inside `scripts/gates.sh`: a write-shaped call added to a
  source-side file fails the build, and a lint that can no longer detect one
  fails it too.
- **Server-side evidence.** Each module's e2e runs a whole transfer under a
  least-privilege identity and reads the server's own record back —
  `rb_pg_assert_readonly_log` on PostgreSQL 10..18 (both grant recipes),
  `rb_mongo_assert_readonly_log` over a mongod started with `--profile 0
  --slowms 0`, and `rb_minio_assert_readonly_trace` over `mc admin trace`, each
  with a refused write as the counter-proof.
- **The audit survives a panic.** The source pipeline runs inside
  `catch_unwind`, so an unwind takes the same failure path as any other error
  and the source is fingerprinted again before the run reports `SourceMutated`
  or the panic itself.
- **Reading a filesystem source the process does not own moves the files'
  access times**, so `O_NOATIME` + `EPERM` is now a refusal at the first
  fingerprint. `--allow-atime-updates` (`RUST_BACKUP_ALLOW_ATIME_UPDATES`,
  `allow_atime_updates:`) is the explicit opt-in, and it warns once.

### Filesystem

- **Special files are carried, not skipped silently.** FIFOs and character/block
  device nodes become plan entries (`fifo`, `chardev`, `blockdev`, with the
  device number in a new `rdev` field) and are recreated with `mkfifo`/`mknod`;
  mode is applied through `O_PATH|O_NOFOLLOW`, because opening a FIFO blocks and
  opening a device has side effects. A unix socket is refused at Analyze — "a
  unix socket cannot be reproduced". A `special_files` preflight check refuses
  device nodes without root or `CAP_MKNOD` before anything is written, and the
  fingerprint hashes `rdev`, so a changed device number is drift.
- **A deep tree exhausted the process's file descriptors instead of walking.**
  Linux's `DirEntry` holds an `Arc` on the directory stream it came from, so the
  children of one level, kept alive while the walk recursed into them, pinned
  one descriptor per level. A tree deeper than `RLIMIT_NOFILE` failed with
  "Too many open files" — and `MAX_WALK_DEPTH` could never be reached to report
  the refusal it exists for. The walk keeps names now, not entries; the entry's
  type has always come from `symlink_metadata`, so nothing else changes.
- **A symlink can no longer redirect a restore.** Directories are created one
  level at a time behind a `symlink_metadata` check instead of `create_dir_all`,
  which follows a symlink to a directory: a planted symlink is refused in Apply
  and a symlinked destination root in Validate.
- The scope edges are written down where an operator meets them — sparse files
  restored dense, xattrs and POSIX ACLs never captured, sockets refused, devices
  needing a capability — along with the cost of the guarantee: a run reads the
  source tree three times, and that is the evidence behind "source unchanged".

### Test harness

- **A catalogue before the code.** `docs/testing/POSTGRES_MATRIX.md` lists 106
  rows and `docs/testing/FILESYSTEM_MATRIX.md` 32, each with its fixture, the
  outcome it must produce and the oracle that decides it. Neither document
  records execution status: the runners print `PASS`/`FAIL`/`SKIP` per row and
  stay the only source of truth for that.
- **Runners for every row.** The PostgreSQL fidelity matrix runs from fixture
  files with `pg_dump` and catalog oracles, container-log immutability evidence
  and per-case counters; `e2e/filesystem_matrix.sh` runs the filesystem matrix
  twice, as root and as an unprivileged user, plus the refusal, xattr, atime and
  `CAP_MKNOD` rows.
- **New coverage.** `e2e/postgres_large_table.sh` (a 2 GiB table, peak RSS of
  both peers under 256 MiB) with CI job `postgres-large`; CI jobs
  `postgres-postgis` and `postgres-postgis-cross-version`; matrix rows for
  extension configuration tables, missing extension versions, PostGIS,
  `CONN-BUDGET`, row counts and constraint state; fault cases
  `pg-lock-timeout`, `pg-destination-kill`, `pg-concurrent-writer`,
  `pg-plan-rejected` and `pg-row-count-mismatch`.

- **The gate is the CI job again.** `scripts/gates.sh` only *built* the
  relay-only feature set and never built the documentation, while CI's
  `rust-quality` job ran `cargo test --no-default-features` and `cargo doc
  --all-features` under `RUSTDOCFLAGS=-D warnings` as two steps of its own — so
  a green gate could still meet a red CI, which it did, on a public doc comment
  linking to a `pub(crate)` type. Both steps moved into the gate and out of the
  workflow, which now runs `bash scripts/gates.sh` and nothing else.

- **A container was called ready while its entrypoint was still initialising.**
  `rb_pg_start` probed `pg_isready` over the unix socket, and the postgres
  entrypoint keeps a temporary server there — `listen_addresses=''` — while it
  runs `initdb` and `/docker-entrypoint-initdb.d`, then shuts it down and starts
  the real one. The probe answered YES to that server, so the caller connected
  into the restart: "the database system is shutting down", or a fixture that
  never loaded. A PostGIS image spends minutes building its template databases,
  which is why the plain matrices only flaked where the PostGIS ones failed
  outright. Readiness is now a TCP probe plus a `SELECT 1`, which the init-time
  server cannot answer, and `e2e/fault_matrix.sh` probes the same way.
- **`M-FS-32` snapshotted the source access times before reading every file.**
  `rb_tree_digest` opens and hashes each regular file, so taking the atime
  manifest first recorded the digest's own reads as drift the run had caused.
  The order matches every other immutability case now: content digest first,
  atimes last.
- **The `T-FS-OWN` non-root cases ran the source as the unprivileged account
  too.** Their fixture tree is deliberately root-owned — that is what makes the
  exact restore impossible for a non-root destination — so the access-time guard
  added in this release correctly refused the run at Analyze, before the
  destination was reached. Only the destination runs unprivileged now; a
  non-root *source* is proven over a tree that account owns, by
  `e2e/filesystem_matrix.sh`.

### Documentation parity

- **`scripts/docs_parity.sh` fails when the operator guide stops describing the
  program.** It reads `scripts/env_inventory.sh` — the clap help of every command
  surface plus the direct `env::var` reads — and asks what help parity cannot: is
  a flag documented in the chapter that owns it, does the row name the variable
  and the default clap prints, does `10-variabili-ambiente.md` list every
  variable the program reads, and does any document name one it never reads. Its
  `--selftest` removes a variable row and renames a variable cell and asserts
  both are reported.
- **The audit it forced.** One cell was wrong — `--udp` defaults to `true`, not
  "attivo" — and the module, matrix and harness documents were reconciled with
  the code: the S3 read surface corrected to the eight methods `ReadOnlyS3`
  carries, a leftover plan marker removed from `IMMUTABILITY.md`, and the two
  test IDs that resolved to nothing anchored in the scripts that run them.
- The README now answers from itself what each restore needs in privilege, what
  every exit code means, and what PostgreSQL and MongoDB refuse to copy rather
  than approximate.


## 0.0.6 — prerelease (2026-09-16)

### Correctness

- **PostgreSQL: a table that ever dropped a column failed the read-back.** The
  catalog comparison required `pg_attribute.attnum` to match, and PostgreSQL
  never reuses an attnum: a source table that lost four columns numbers its 58th
  live column 62, while a logical restore — which recreates only the live
  columns — numbers the same column 58. A byte-perfect restore of a long-lived
  schema (an upgraded Odoo cluster, for instance) was reported as corrupt:

  ```text
  [Verify] PostgreSQL catalog read-back differs from the source plan:
  $.databases[0].schemas[1].tables[92].columns[57].ordinal source=62 destination=58
  ```

  The gap records the source table's *history*, not its shape. No logical
  restore can rebuild it — only `pg_upgrade`, which keeps the physical files —
  and the check could never have been complete anyway, because a column dropped
  from the end leaves no gap at all. Both sides are now renumbered and the
  comparison enforces the full column *order*, along with names, types,
  nullability, defaults, identity, generation, collation and comments. The limit
  is recorded in `docs/modules/POSTGRES.md`, and `e2e/postgres_matrix.sh` seeds a
  table with dropped columns so a live restore exercises the case.

### Diagnostics

- **A failed catalog read-back now says what to go and look at.** It reported a
  single difference, addressed by position (`tables[92].columns[57]`). It now
  reports every difference it finds, up to 20, each named by the objects it
  belongs to:

  ```text
  [Verify] PostgreSQL catalog read-back differs from the source plan (2 differences):
    - catalog.databases["odoo"].schemas["public"].tables["account_move"].columns["state"].type_name source="varchar" destination="text"
    - catalog.databases["odoo"].schemas["public"].tables["res_partner"].columns["vat"] missing at destination
  ```

  Lists of named objects are matched by name rather than by position, so one
  missing table reports itself instead of shifting every later index and burying
  the real difference. A pure reordering is still a difference, and is reported
  as one.

## 0.0.5 — prerelease (2026-09-16)

Same code as 0.0.4; the first tag published by the fast path.

### CI/CD

- **`fast-release.yml` owns the tag path: binaries only.** It compiles once per
  architecture on a native runner with a cargo cache keyed by `Cargo.lock`,
  smoke-tests the binary, and publishes the archives, their checksums and the
  GitHub release. `release.yml` and `docker.yml` no longer trigger on `v*`, so a
  tag no longer pays for a second binary build, a container validation build, a
  Trivy scan, SBOM or provenance — all of which still run on every branch push.
  The tagged commit has already passed the full gate on `dev`.

## 0.0.4 — prerelease (2026-09-16)

### Correctness

- **PostgreSQL: a commented materialized view aborted the restore.** The
  comment of both relkinds is captured, but it was always replayed as
  `COMMENT ON VIEW`, which PostgreSQL refuses for a materialized view —
  verified against a live PostgreSQL 13:

  ```text
  ERROR:  "mv" is not a view
  ```

  Materialized views now get `COMMENT ON MATERIALIZED VIEW`; plain views are
  unchanged. (`ALTER TABLE … OWNER TO`, used for both, is accepted by
  PostgreSQL for either relkind — checked on the same server.) Covered by a
  unit test on the emitted keyword and by comments on both the plain and the
  materialized seed view in `e2e/postgres_matrix.sh`;
  `e2e/postgres_matrix.sh 13` passes 12/12.

## 0.0.3 — prerelease (2026-09-15)

### Correctness

- **PostgreSQL: a view that depends on a primary key broke the whole restore.**
  Views were created in `pre_data`, before the keys. SQL lets a query select a
  column that is not in its `GROUP BY` when it is functionally dependent on it,
  and PostgreSQL proves that dependency from the primary key alone — so a legal
  source view such as `SELECT t.id, t.state FROM task t GROUP BY t.id` was
  rejected at CREATE time while its table still had no primary key:

  ```text
  [Apply] apply DDL: CREATE VIEW ...: db error: ERROR: column "t.kanban_state"
  must appear in the GROUP BY clause or be used in an aggregate function
  ```

  Reported on a real Odoo cluster (`report_project_task_user`, PostgreSQL 13 →
  13). Views and materialized views are now emitted in `post_data`, after every
  constraint; a materialized view is still created `WITH NO DATA` and refreshed
  once the tables hold their rows. Nothing in the data load needs a view: `COPY`
  targets tables.
  Covered by a unit test on the emitted order and by two new seed views in
  `e2e/postgres_matrix.sh` (plain and materialized) that reproduce the failure
  against a live cluster. `e2e/postgres_matrix.sh 13` passes 12/12 with the fix.

## 0.0.2 — prerelease (2026-09-15)

What `v0.0.1` shipped, plus the three failures that release exposed. Every
workflow on `dev` is green on this tree: CI, end-to-end, container, release and
the security audit.

### Security

- **rustls 0.23.40 → 0.23.45** — RUSTSEC-2026-0285 (published 2026-09-14,
  medium): TLS 1.3 handshake messages accepted across encryption-level
  boundaries. The `v0.0.1` binaries and images link the affected version; use
  `0.0.2` instead.

### CI/CD

- **MinIO images moved.** Docker Hub's `minio/minio` and `minio/mc` now answer
  `pull access denied … repository does not exist`, which broke
  `e2e/s3_minio_test.sh` and the `s3` group of `e2e/fault_matrix.sh`. Both pull
  MinIO's own registry at a pinned release
  (`quay.io/minio/minio:RELEASE.2025-09-07T16-13-09Z`,
  `quay.io/minio/mc:RELEASE.2025-08-13T08-35-41Z`).
- **Action pins carry their exact release.** zizmor (pedantic) went red with no
  change on our side because upstream moved the floating `v4`/`v7` tags:
  `docker/setup-buildx-action` (v4.3.0) and `docker/build-push-action` (v7.3.0)
  no longer matched their comments. Both are bumped to the current release, and
  every pin in every workflow now names the exact version — an exact tag never
  moves, so the finding cannot return on the next upstream re-tag.
- **`ci.yml` no longer runs on `v*` tags.** Making a cache-using workflow
  reachable from a release-triggering event is exactly what zizmor's
  cache-poisoning audit flags, and the commit a tag points at has already passed
  CI on `dev`; `release.yml` and `docker.yml` rebuild from scratch anyway.
- `release.yml` names the repository for `gh` (`GH_REPO`): that job downloads
  artifacts and never checks the repository out, so `gh release create` died
  with `failed to run git: fatal: not a git repository` the first time a tag was
  ever pushed.

## 0.0.1 — first prerelease (2026-09-15)

The tree reviewed on 2026-09-11, cut as a prerelease so the binaries and the
container image can be exercised outside this repository. Pre-1.0: the CLI
surface and the plan wire format may still change.

### Documentation

- **`docs/usage/` is now the single source of truth for operating the program.**
  One page per feature — coordination server, transport, postgres, mongodb,
  filesystem, s3, YAML sessions, the `plan` dry-run, Docker/Compose, environment
  variables, exit codes — each opening with the minimal working invocation and
  then covering every flag, its environment variable, its default and what it
  actually changes. The guide is written in Italian; `README.md` and `USAGE.md`
  point at it.
- `USAGE.md` is now a pointer to those pages instead of a second, divergent flag
  inventory.
- Corrected a command that never existed: the dry-run is `rust-backup plan
  <module> [PARAMS]`, with no `source` positional (`USAGE.md` and `CLAUDE.md`
  both showed `plan <module> source`, which clap rejects with
  `unexpected argument 'source' found`).

### CI/CD

- `scripts/help_parity.sh` now checks the binary's `--help` against
  `docs/usage/*.md` in both directions, and the ghost-flag scan covers the new
  guide as well as `README.md`, `USAGE.md`, `docs/`, `docs/modules/` and
  `e2e/README.md`.
- `release.yml` refuses a tag whose name does not match the crate version, and
  publishes a tag below `1.0.0` (or one carrying a `-suffix`) as a **prerelease**
  instead of as the repository's default download.
- `docker.yml` pins `latest=false` on the metadata action: without it the auto
  flavour would move the `latest` tag onto a prerelease cut from `dev`. `latest`
  keeps meaning `main`, and a `v*` tag publishes `v0.0.1`, `0.0.1`, `0.0` and
  `sha-…`.
- Workspace version set to `0.0.1` to match the tag.

## Unreleased — pre-staging review (2026-09-11)

A full line-level re-review of the tree before staging. Every item below fails
against the previous code and carries a test that proves it.

### Correctness

- **A completed QUIC transfer could be reported as failed.** quinn's
  `CONNECTION_CLOSE(0)` overtakes the stream FIN, so the destination's
  `await_peer_close` saw `connection lost: closed by peer: 0` instead of EOF on
  a transfer that had already verified. A graceful application close with code
  `0` — the code both `DirectConn::close` and a dropped connection handle send —
  now reads as EOF. Nothing else does, so a truncation still fails
  (`rb-transport`).
- **MongoDB `--overwrite` could leave a half-loaded replacement uncleaned.** The
  "already present" snapshot was taken *after* `restore_namespaces` had already
  dropped each target, so an overwritten collection was recorded as pre-existing
  and excluded from rollback. The drops now happen before the snapshot.
- **PostgreSQL: four classes of object were copied away silently.** Analysis now
  refuses logical-replication publications, logical-replication subscriptions,
  event triggers, and any identifier containing a dot (which this build's
  qualified references cannot name unambiguously). Each probe was validated
  against live PostgreSQL 10 and 18, as superuser and as a plain LOGIN role.
- **PostgreSQL: a zero-column inheritance parent streamed its children's rows.**
  `COPY t TO` has no `ONLY` form, and a table with no COPY-able columns took the
  plain-`COPY` path. It now uses `COPY (SELECT FROM ONLY t)`.
- **PostgreSQL: an extension was created before the schema it lives in.**
  `CREATE EXTENSION … WITH SCHEMA gis` now follows the `CREATE SCHEMA`.
- **`Ctrl-C` left the destination's partial item on disk.** `SIGINT`/`SIGTERM`
  are handled: the run aborts through the normal path, the active item is
  removed, nothing prints `VERIFIED`, and the message says the run was
  interrupted. Proven by a new `e2e/fault_matrix.sh` case whose negative control
  (an unhandled `SIGQUIT`) still fails assertion B.
- **A module error wrapped by `plan` or the config loader lost its exit code.**
  Four `map_err(|e| anyhow!("{e}"))` sites threw the typed `BackupError` away,
  so a missing credential exited 1 instead of the documented 2.
- **`--admin=false` could not override `admin: true` in YAML.** The six boolean
  module flags are three-state now (`--flag`, `--flag=false`, absent), and only
  an explicitly given value overlays the config. The value must be attached with
  `=`; the space-separated `--flag false` spelling is rejected so the switch
  cannot swallow the next argument.
- **A watcher task leaked on the destination-verify timeout.** That one exit
  path returned without aborting the spawned completion watcher.
- **A carrier index above `u16::MAX` was truncated on the wire** instead of
  refused at `Connect`.

### Hardening

- `--max-conns` now bounds **the accepted control connections as well as** the
  relayed substreams. The accept loop was unbounded: any peer that could reach
  the port got a task, a yamux session and — after `Register` — a registry entry
  that outlived the handshake. The permit is taken *before* `accept()`, so the
  excess waits in the kernel backlog.
- Restored files and directories are created owner-only (`0600`/`0700`) and
  widened to their recorded mode only once content and ownership are in place;
  there is no longer a window in which a private file is world-readable.
- The filesystem destination refuses two more hostile-plan shapes: a
  non-directory entry that is an ancestor of another entry, and a hardlink that
  names an entry the plan does not declare as a file.
- The control-frame buffer can no longer overshoot `MAX_FRAME_LENGTH` by up to
  1023 bytes before the bound trips; each read is capped by the remaining
  allowance.
- S3 re-checks the catalog size against the plan item's size inside
  `restore_object`, not only at preflight.

### Tests

- The multi-carrier fault bank was fake coverage: `protocol_faults_are_…` bound
  `carriers` and never passed it anywhere, so both passes drove the identical
  single-stream parser and every defensive branch of `MultiStreamChunkSource`
  was untested. There is now a real item-pinned multi-carrier harness, two tests
  for the demux's own defences, one that proves an idle sibling carrier is not
  read while the cursor sits on an item, and a per-case timeout so a stalled
  parser fails instead of wedging CI.
- `e2e/relay_smoke.sh` seeds the large mixed-size fixture (220 bulk entries plus
  the metadata tree) that plan F1.1 and QA criterion 2 ask for; the old
  eight-entry tree never kept four carriers busy at once.
- Suite: 250 tests, 0 ignored (rb-postgres 65, rb-transport 58, rb-core 46,
  rb-mongodb 31, rust-backup 24, rb-filesystem 13, rb-s3 13).

### CI and tooling

- The `aws-s3` e2e job never set `RUST_BACKUP_AWS_E2E=1`, so it printed SKIP and
  reported green — a real-AWS run that never happened.
- `scripts/gates.sh` passes `--locked` to clippy, both builds and the test run.
- `security.yml` derives its tool cache keys from the pinned versions instead of
  repeating them, so bumping a `--version` can no longer restore the old binary
  from cache and skip the install.
- The `postgres-introspection` job gained the cargo cache the other jobs have.
- `scripts/help_parity.sh` scans documents by glob. Its hand-kept list named
  `docs/DEPLOYMENT.md`, which does not exist, and the `[[ -f ]]` guard made that
  silently a no-op.

### Documentation

- The PostgreSQL supported range is stated one way everywhere: major 10 is the
  enforced minimum, there is no upper bound, and the CI matrix covers 10–18.
- Exit code 5 is documented as integrity **or** apply **or** verify (it was
  "integrity/apply" in one place and "integrity/verify" in another), and both
  lists now include 0 and 1.
- New: every environment variable that has no flag (`RUST_LOG`,
  `RUST_BACKUP_PLAN_TIMEOUT`, `RUST_BACKUP_VERIFY_TIMEOUT`,
  `RUST_BACKUP_STUN_SERVERS`, `BORE_PROXY_BUFFER_SIZE`), every variable
  `compose.yml` reads — including that `RUST_BACKUP_CONTROL_PORT` remaps only
  the published host port — and every switch the e2e scripts read.
- `docs/modules/README.md` no longer says "live matrix pending" for modules
  whose matrices are green; the historical V1/V2 plans carry a superseded
  banner; the README documentation index lists every document.
- `CLAUDE.md`: `tokio::io::split` on a `mux::Stream` is safe for read/write
  (yamux 0.13 keeps separate reader/writer waker slots) — what is unsafe is two
  tasks on the same direction.

## 0.1.0 — 2026-09-09

First release. The workspace version, the plan/wire `PLAN_FORMAT_VERSION` and
the QA exit criteria in `docs/plans/RUST_BACKUP_PLAN_V3.md` are all frozen at
this point; `docs/plans/RESUME.md` records the command that proved each phase.

- A failed restore no longer leaves state that could pass for a copy:
  PostgreSQL drops the databases the run created, MongoDB drops the collections
  it created, and both name what they removed. A target the run found already
  present is never touched. The filesystem module already deleted its active
  partial file and S3 already aborted its multipart uploads.
- `e2e/fault_matrix.sh` became the live fault matrix across all four modules —
  source killed mid-transfer, coordination server killed during the plan
  exchange and mid-payload, destination SIGKILLed at 10/50/90 % of the bytes,
  and the destination backend stopped mid-apply — each asserting source
  immutability, the absence of usable partial state, and a truthful error phase.
  It also covers immutability under concurrent load in both directions: writes
  beside the source root are not a false `SourceMutated`, and writes into it
  exit 6. A further case proves the other half of that promise for the faults
  nothing can clean up: a restore into the tree a killed destination left behind
  is refused at preflight rather than merged into.
- `e2e/bandwidth_netem.sh` adds the one-vs-four-carrier proof: identical
  restored trees and no regression on a shaped link, with the measured ratio
  recorded.
- A rejected plan now reliably reaches the source as a plan rejection (exit 4)
  rather than intermittently as a transport failure (exit 7): the destination
  waits, bounded, for its rejection frame to be consumed before exiting instead
  of leaving it buffered in the relay.
- Both peers now log the carrier count they negotiated
  (`negotiated data plane carriers=N separate_data_streams=…`), not only a
  downgrade. `--carriers` is a request, and without the agreed number neither an
  operator nor a test could tell a four-carrier run from a silent fallback to
  one; `e2e/relay_smoke.sh`, `e2e/bandwidth_netem.sh` and `e2e/s3_minio_test.sh`
  now assert it — the last of them proves the per-module cap live by asking for
  four carriers against an S3 destination that permits one.
- The learned direct-peer cache is now bounded in fact as well as in its
  description: 128 entries, expiry swept on insert, least recently learned
  evicted. It stays advisory — an eviction costs a probe, never a connection.
- Dropped `CheckConfig::role`, which the caller set from the direct-path role
  and the connectivity probe never read: both peers run the same symmetric
  exchange. Removed bore's unused registration channel for additional provider
  connections (`PendingCarriers`/`TokenGuard`) along with the parameter that
  threaded it through three handlers — a provider registers once here and every
  carrier is a substream on that one mux.
- `scripts/crate_invariants.sh` asserts `#![forbid(unsafe_code)]` and the
  unwrap/expect/panic denial in every crate root — `rb-core` was missing both,
  so the crate every module depends on sat outside its own lint gate.
- `e2e/mongodb_matrix.sh` reports a host that cannot run a MongoDB major at all
  as a `SKIP` naming the image and the kernel, and exits 77 when nothing ran;
  `e2e/full_matrix.sh` renders 77 as a `SKIP` row with its own counter. Every
  published MongoDB 8 image refuses Linux 6.19 and newer (SERVER-121912), which
  used to surface as an opaque "not ready" failure.
- `scripts/help_parity.sh` now checks both directions, so a flag documented in
  `USAGE.md` that the CLI does not accept fails the gate. That caught
  `--preserve-ownership`, which never existed as a flag.
- Documented that `--overwrite` destroys before it restores and that a restore
  is not atomic, for all three modules that support it.

- Successful completion now requires backend read-back evidence: filesystem,
  PostgreSQL, MongoDB and S3 re-introspect their restorable metadata/catalog and
  reread all persisted payload to reproduce source BLAKE3 commitments. Both
  peers finish at a truthful verified 100%; evidence-free legacy completion is
  rejected. Source immutability audits now cover complete PostgreSQL and MongoDB
  datasets instead of samples.
- Hardened real-service regressions cover PostgreSQL 10–18 and MongoDB 4–8 both
  same-version and cross-version, PostgreSQL 18 NOT NULL catalogs and `public`
  schema defaults, database locale/template0 recreation, empty MongoDB
  collections and indexes, filesystem ownership contracts, and S3 metadata,
  policy, exact key-set, overwrite and multipart cleanup.
- Added pinned GitHub Actions for Rust CI, the complete end-to-end matrix,
  dependency/security analysis, multi-architecture container publication, and
  reproducible Linux release artifacts with checksums and provenance.
- Added a hardened non-root coordination-server image, Compose/TLS deployment
  examples, per-backend session configurations, and complete binary/container
  deployment and end-to-end guidance in the README.
- Removed unused direct dependencies and updated the MongoDB driver to 3.8.
- Added relay end-to-end coverage, TLS control-listener support, secret-file
  input, bounded session accounting, and S3 fidelity preflight checks.
- Multi-carrier transfers now negotiate the safe count on the wire and bind
  relay data streams by explicit carrier identity; filesystem restores are
  plan-ordered and item-pinned without intra-item striping.
- Added an evidence-bearing `VerificationAck` → `CompleteAckAck` →
  `VerificationComplete` handshake: neither peer reports success before
  destination read-back and source immutability proofs are mutually observed,
  on both relay and direct QUIC streams. Destination aborts now interrupt source
  pacing and blocked chunk writes, and truncated carriers/offset gaps are rejected.
- Destination failures now use an `Abort`/`AbortAck` control-plane handshake, so
  relay teardown cannot replace an apply error such as ENOSPC with a generic EOF.
  Progress also emits an initial per-target snapshot synchronously, including for
  transfers that finish before the periodic reporter is first scheduled.
- Interrupted filesystem items remove their partial final-path file. S3 fidelity
  preflight now rejects non-default ACLs and has a credential-gated real-AWS smoke.
- Direct QUIC streams now use an explicit readiness byte, privileged restores
  apply ownership before setuid/setgid mode bits, and current peers negotiate a
  dedicated data stream even at one carrier so destination aborts interrupt
  blocked source writes without concurrently splitting a yamux stream.
- Privileged QA now covers real ext4 ENOSPC and correctly shapes relay egress for
  the 200 MiB netem/backpressure measurement.
- Updated vulnerable dependencies and removed the obsolete `rustls-pemfile` and
  legacy rustls 0.21 dependency path; `cargo audit` is clean.

## Versioning

This project follows semantic versioning. Breaking plan/wire or configuration
changes require a major or minor release and an explicit `PLAN_FORMAT_VERSION`
review; fixes and additive backward-compatible options are patch releases.
