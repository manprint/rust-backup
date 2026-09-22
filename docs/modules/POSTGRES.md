# PostgreSQL module

`postgres` performs a logical PostgreSQL copy with pure-Rust catalog
introspection and binary `COPY`; it never calls `pg_dump`.

**Supported majors.** Major 10 is the enforced minimum (`MIN_PG_MAJOR`): an
older server is refused at connect. There is no enforced upper bound. The CI
matrix runs 10–18 same-major plus the cross-major pairs `10:18`, `12:16`,
`14:17` and `16:18`; a newer major is accepted and, because analysis fails on
any catalog object this build cannot reproduce (see below), a version that
introduces something new refuses rather than certifying a partial copy.

Database encoding and locale are restored independently of the destination
cluster defaults. The module uses `template0` automatically and preserves the
database locale provider using the catalog and DDL available in each supported
major (`libc` on 10–14, ICU metadata on 15+, ICU rules on 16+, and `builtin` on
17+).

## Captured and restore order

The source captures cluster roles, memberships, databases, schemas, extensions,
tables, sequence state, table data, constraints, indexes, grants and ownership
metadata, together with the comments carried by each of them — including
comments on constraints and on indexes — and the relation options of views and
materialized views (`check_option`, `security_barrier`, storage parameters).
Relations an extension registered with `pg_extension_config_dump` are captured
as extension configuration tables: the relation itself is recreated by `CREATE
EXTENSION`, but the rows matching the extension's registered condition are user
data and are streamed and restored (PostGIS `spatial_ref_sys` is the usual
case). The destination clears exactly that scope — `DELETE` with the registered
condition, `TRUNCATE` when the extension registered none — before loading the
source's rows, so the rows `CREATE EXTENSION` inserted are replaced rather than
merged with. A
materialized view the source left unpopulated (`WITH NO DATA`, never refreshed)
is restored unpopulated: refreshing it would hand the destination rows the
source does not have. The destination restores in dependency order: roles and databases,
pre-data DDL, streamed table data, then post-data constraints/indexes, sequences
and grants. Source reads are read-only; data streams straight into the tunnel.

## Extension versions

An extension is restored at the source's exact version. The destination's
preflight checks every extension against `pg_available_extension_versions` and
refuses the plan when that version is absent — `CREATE EXTENSION … VERSION '1.0'`
against a destination that only ships 1.1 is an error, and it would otherwise
surface halfway through `pre_data`, with roles and databases already created.
The refusal names what the destination does have.

`--extension-version default` (`RUST_BACKUP_EXTENSION_VERSION=default`, the
destination side only) accepts the destination's default version instead: the
`CREATE EXTENSION` statement is emitted without a `VERSION` clause, the catalog
read-back accepts the installed version for that extension and nothing else, and
each substitution is reported as `deviation: extension <name> restored at version
<actual> (source <version>)` on the `RESTORE VERIFIED` line. The policy relaxes
the version, never the extension: one the destination cannot install at all is
still refused.

## Completion proof

The source counts the rows of every plan item — in exactly the scope the item
streams, `FROM ONLY` for an inheritance parent and restricted to its condition
for an extension configuration table — and of every populated materialized view.
While applying, the destination compares each count with the rows its `COPY`
actually wrote, and after the refresh statements it counts every populated
materialized view on its own side. A difference is an integrity failure. The
BLAKE3 commitments prove that what arrived is what was sent; only the counts
prove that nothing was left behind, and a plan that carries no counts is
restored with one warning saying the row check was skipped.

A constraint is compared in full: its definition, its comment, and the state
`pg_constraint` records — `convalidated`, `condeferrable`, `condeferred`. A
`NOT VALID` constraint stays `NOT VALID`, because the rows that violate it are
legal state on the source and a validated copy would have had to reject them;
the same plan is refused outright when a constraint's recorded state and its
definition text disagree.

The destination then reconnects after apply, re-introspects the complete
restorable catalog, and compares it with the normalized source plan. It reads
every table through deterministic binary `COPY` and must reproduce every item and
the whole-payload BLAKE3. Only after this proof and the source's full post-run
catalog/data fingerprint audit do both commands exit successfully. A database
that merely accepted DDL or rows cannot produce `RESTORE VERIFIED`.

What it proved is printed under that record: `rows verified: <t> from tables,
<m> from materialized views, <n> rows` and `constraints: <c> (validated <v>,
not valid <nv>)`, plus one `deviation: ...` line per declared departure. The
same numbers leave the module as fields of the `PostgreSQL destination
read-back verified` event.

## A failed restore removes what it created

`CREATE DATABASE` cannot run inside a transaction, so a restore is not one
atomic unit: a run interrupted mid-load has already created the database, its
schemas and part of its rows. Nothing certifies such a database — there is no
`RESTORE VERIFIED` — but on inspection it is indistinguishable from a small
one, which is exactly the mistake this build refuses to leave available.

So on any failure after creation the destination drops the databases **this run
created**, named in a `WARN` line. A database the run found already present is
never touched: without `--overwrite` the run never wrote to it, and with
`--overwrite` it was dropped before the restore began, so it cannot be mistaken
for pre-existing state. If the drop itself fails — the usual reason being that
the server the restore was writing to has gone away — an `ERROR` line names the
database and the `DROP DATABASE` that finishes the job, and the original
failure, not the cleanup, is what the process reports and exits with.

The live proof is `e2e/fault_matrix.sh postgres`: with the source or the
coordination server killed mid-load, the target database is gone afterwards;
with the destination's own backend stopped mid-apply, nothing can clean up and
the assertion is instead that the leftover is uncertified and visibly not the
source.

## Failure behaviour

Four ways a run ends badly, and what each leaves behind — every one of them is a
case in `e2e/fault_matrix.sh`:

| Failure | Source | Destination |
|---------|--------|-------------|
| The destination process dies mid-stream | fails with a `Transfer` error, runs its failure-path immutability audit and reports the source unchanged — never a mutation it did not cause | whatever it had written is uncertified; nothing can clean up after a killed process |
| Somebody writes to the source while it is read (cold run; a [hot backup](#hot-backup-hot_backup) expects it and is not audited) | the fingerprint audit catches it: `SOURCE-IMMUTABILITY VIOLATION`, exit `6` | the load may have completed, so the databases this run created are dropped — a copy of a moving source is never left behind looking complete |
| The source process dies mid-`COPY` | — | the apply fails and the databases this run created are dropped |
| The destination refuses the plan (preflight, or an existing database without `--overwrite`) | learns it as a plan rejection (exit `4`) and still audits the source | nothing was written, nothing is removed; an existing database is never touched |

The rule behind the table: a database this run **created** never survives a run
that did not print `RESTORE VERIFIED`, whether the failure happened while
loading or after it. A database that was already there is never removed by a
failure — without `--overwrite` the run never wrote to it, and with
`--overwrite` it was dropped before the restore began.

## Restore order, and why it is not a fixed list

Object dependencies point in both directions at once: a column default may call
a user function, and a function may return a table's row type. No single fixed
order of `CREATE TABLE` and `CREATE FUNCTION` satisfies both, so the cycle is
broken the way `pg_dump` breaks it — every dump opens with
`SET check_function_bodies = false`, tables come before functions, and column
defaults and identity clauses are attached afterwards with
`ALTER TABLE … ALTER COLUMN …`.

Views are emitted in dependency order derived from `pg_depend`/`pg_rewrite`, not
in catalog name order: `app.active` selecting from `app.zombies` sorts first and
would fail to resolve. Tables are emitted parents-first over both partitioning
and classic inheritance, computed as a longest-path depth, so a partition that is
itself partitioned cannot precede its own parent.

Materialized views are created `WITH NO DATA` and populated with
`REFRESH MATERIALIZED VIEW` in post-data, after the table rows have arrived.
Creating them with data ran their queries against empty tables, so every
materialized view restored empty — invisibly, since the model carries no content
for them and the per-item digests cover table items only.

## Partitioning and classic inheritance are different things

A partitioned parent holds no rows of its own; its plan item streams the whole
tree and its partitions are not items. A classic `INHERITS` child holds its own
rows *and* is expanded into by a plain `SELECT` on its parent — so an inheritance
parent is read `FROM ONLY`, and the child is a data item in its own right.
Without that distinction the parent's item carried the child's rows too and the
destination ended up holding each of them twice, while the read-back — reading
the destination parent the same expanding way — agreed with the result.

A constraint that PostgreSQL materialised on a partition or an inheritance child
(`conislocal = false`) is not re-emitted: the parent's own
`ALTER TABLE … ADD CONSTRAINT` cascades and creating it again fails the restore
outright. A partitioned parent's index is rendered by `pg_get_indexdef` as
`ON ONLY parent` and stays **invalid** until each child index is attached, so
`ALTER INDEX … ATTACH PARTITION` is emitted for every child index.

An inheritance child's `CREATE TABLE` lists only its *local* columns; re-listing
an inherited one merges it but marks it local, a different catalog state from the
source's.

## The source fingerprint

Before and after every run — including a run that failed — the source is
measured: the structural catalog hash, plus, for each data-bearing table, the
number of rows and an **order-independent 128-bit commitment** over them (the
wrapping sum of `md5(row)`, folded client-side while the server streams one hash
per row). Addition and not XOR: XOR cancels duplicated rows, so `{A, A, C}` and
`{B, B, C}` would look identical. Nothing is sorted and nothing is aggregated
server-side, so the audit itself writes no temporary file, and it costs one full
read of each table per audit. The audit runs twice, so a run reads the cluster
three times in total — before, during the transfer, and after — and the
destination read-back adds a fourth full read on its own side. Any drift between
the two measurements is `SourceMutated` (I-IMMUT). The digest is prefixed
`rust-backup/pg-fingerprint/v2` and is not comparable with a digest taken by an
older build.

A cluster of more than 200 000 data items is refused against the plan's item
ceiling (`MAX_PLAN_ITEMS`), which is what keeps a peer-supplied plan from
dictating the destination's allocation. The source raises the refusal itself,
naming the ceiling, before the plan is sent.

## Hot backup (`hot_backup`)

`--hot-backup` on both peers copies a source that stays online. The fingerprint
above cannot work there — the application writes throughout, so the two
measurements always differ — and a copy made of independent per-table `COPY`
statements would be worse than a refusal: children read after their new parents
were inserted fail their foreign key on the destination, and a sequence read
before its table restarts below ids the copy already holds.

The source therefore reads through **one snapshot per database**
(`SourcePool::snapshot`): every pooled connection sends
`BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY` right after connecting (after
the role probe, whose failure must stay a missing warning) and keeps it until
the run ends. Introspection, the sequence values, the row counts and every
`COPY` then see the state of the first statement of that connection; sequences
are read non-transactionally *after* the snapshot, so a restored sequence is
never behind a restored id. A connection that drops mid-run is not replaced —
a new one would read a later state — and the run fails with `hot backup: the
connection holding the snapshot of database '…' was lost; restart the backup`.

The session skips both fingerprint audits and prints `BACKUP VERIFIED (hot
backup)` / `RESTORE VERIFIED (hot backup)` instead of the cold headlines. The
plan is `BackupMode::HotSnapshot`, which a destination restores only with its
own `hot_backup`; the destination's checks — catalog read-back, row counts,
per-item BLAKE3 — are unchanged and are exact, because the plan and the stream
describe the same snapshot.

Costs, all bounded by the run: the transaction holds back vacuum's cleanup
horizon, and it keeps the `ACCESS SHARE` lock of every relation it read, so DDL
on those tables (and the queries queued behind that DDL) waits for the backup.
`idle_in_transaction_session_timeout=0` is already pinned on source sessions, so
a server-wide value cannot kill the snapshot while the destination reviews the
plan. A whole-cluster run is consistent per database, not across databases —
PostgreSQL cannot share a snapshot between databases.

## Determinism of the read-back

Table items are streamed ordered by the C-collated text of the whole row, and the
destination read-back re-runs that same query on the restored data. That is only
sound if both servers render values identically, so every source and read-back
connection pins `extra_float_digits`, `DateStyle`, `IntervalStyle`, `TimeZone`,
`bytea_output` and `lc_monetary`. PostgreSQL 10 defaults `extra_float_digits` to
0 (printing `1.0000000000000002` and `1.0000000000000004` both as `1`, tying the
sort key) while 12+ defaults to 1; unpinned, a byte-perfect 10 → 16 restore could
be reported as corrupt.

## Settings are data, not SQL

Role and database GUCs (`ALTER ROLE … SET`, `ALTER DATABASE … SET`) arrive inside
the plan and are applied by the destination administrator. The GUC name is
validated as a plain identifier and the value is emitted as a string literal, so
a value cannot become a second statement. Any unprivileged user on the source can
put arbitrary text in their own custom (placeholder) GUC, which — interpolated
raw into a multi-statement string — was a privilege escalation against the
destination cluster. `search_path` and `DateStyle` are list-valued and appended
verbatim as `pg_dump` does; a semicolon inside one is refused rather than
emitted.

## Objects this build refuses to copy silently

`verify_catalog` compares the source model against a re-introspection *through
the same model*, so anything absent from the model is lost invisibly and then
certified as a faithful copy. Analysis therefore **fails** when the source holds
any of: triggers, row-level security (policies or `relrowsecurity`), user-defined
types (enum, domain, composite, range), aggregate or window functions, foreign
tables, column-level privileges, default privileges (`ALTER DEFAULT PRIVILEGES`), large
objects, user-defined collations, rules other than a view's own `_RETURN`, an
inheritance child whose inherited column is locally `NOT NULL`, or a `reg*`
column — whose binary `COPY` representation is a raw OID that names a different
object on the destination.

Analysis also fails on **logical-replication publications and subscriptions**
(a publication is not copied and a subscription would restart replication
against the source's upstream from the destination), on **event triggers** (a
cluster-wide DDL hook the model does not carry), and on any **identifier
containing a dot** — schema, relation or column — because this build's qualified
references cannot name `a.b`.`c` unambiguously.

On a **PostgreSQL 18 or newer source** the same applies to a `NOT NULL`
constraint that is named or `NOT VALID`. PostgreSQL 18 catalogues every not-null
constraint in `pg_constraint` (`contype = 'n'`), which makes two things
expressible that this build reads as a plain `attnotnull` and would silently
rebuild as a *default-named, validated* constraint: an operator-chosen
constraint name, and a `NOT VALID` not-null constraint, which does not actually
forbid the rows already in the table. Constraints carrying the server's own
default name are copied as before, so an ordinary cluster is unaffected — and
because a partition's not-null constraint is inherited from its parent, its name
on the destination is the same one PostgreSQL would pick natively.

The error names every offender. `allow_unsupported_objects=true`
(`-P allow_unsupported_objects=true`) accepts a knowingly partial copy; the run
then logs exactly what it leaves behind.

Default privileges are the most common refusal on application databases, and
the narrowest loss: existing grants (`relacl`) are copied like any other ACL,
and what is left behind is only the rule applied to objects a role creates
*after* the restore. Each offender reads `schema.role:objtype` (no `schema.`
for a role-wide rule), `objtype` being `pg_default_acl.defaclobjtype`. How to
read the rules on the source and recreate them on the destination is in
[docs/usage/03-postgres.md](../usage/03-postgres.md#caso-frequente-alter-default-privileges).

## Runtime model and guardrails

A formal review of the module's runtime behaviour, one line per checked
property, with the anchor that proves it. Reviewed 2026-09-16 against the code
in this repository.

| # | Property | Anchor | Status |
|---|----------|--------|--------|
| a | Every source-side connection is read-only. `PgConnection::connect` (read-write) appears only in `dest.rs`; introspection, fingerprint and `stream_out` all use `connect_read_only`, which sets `default_transaction_read_only=on`. | `connect.rs:37,45`; `introspect.rs:41,54`; `immutability.rs:52`; `source.rs:95`; `dest.rs:183,254,306,714` | PASS |
| b | A destination error mid-item aborts the `COPY` server-side: the unfinished sink is dropped, and a dropped sender makes the driver send `CopyFail` instead of `CopyDone`. The run then drops the databases it created. | `dest.rs` `apply_data`/`finish_current`; `remove_partially_restored`; `tokio-postgres-0.7.18/src/copy_in.rs:56-60` | PASS |
| c | A destination that disappears mid-item fails the source rather than hanging: every chunk is handed to the channel with `await`, and the channel error propagates out of `copy_stream_to_sink` with its phase. | `source.rs` `copy_stream_to_sink` | PASS |
| d | A plan received over the wire cannot drive the destination into unbounded work: dependency depths are relaxed iteratively with a pass count bounded by the number of nodes (never recursion), and difference reporting stops at `MAX_REPORTED_DIFFERENCES`. | `ddl.rs` `relax_depths`; `dest.rs` `MAX_REPORTED_DIFFERENCES = 20` | PASS |
| e | No table is ever collected client-side. The source streams `COPY` frames, the destination streams them into `copy_in`, and the fingerprint hashes the `COPY` stream as it arrives. What is collected is catalog metadata, bounded by the number of objects, not by rows. | `source.rs` `copy_stream_to_sink`; `dest.rs` `apply_data`; `immutability.rs` `table_stat` | PASS |
| f | Memory per item is the 1 MiB chunk buffer plus the `COPY` frame in flight (the buffer is drained in 1 MiB units, so its peak is `CHUNK_SIZE` + one frame), on both sides — independent of the table's size. Phase 3 § 3.4 proves it on a 2 GiB table. | `rb-core/src/wire.rs:25` (`CHUNK_SIZE`); `source.rs` `copy_stream_to_sink` | PASS |
| g | Every fallible path carries the phase it happened in. Two sites were wrong and were fixed in this review: counting a materialized view's rows and the item-framing check both run inside apply and were tagged `Verify`. | `dest.rs` `matview_count_error`, `item_end_mismatch`; tests `review_g_matview_count_error_is_tagged_apply`, `review_g_item_end_mismatch_is_tagged_apply` | FIXED |
| h | Backpressure is the consumer's (I-BANDWIDTH): a chunk write waits on the substream window, so a slow destination stalls the source's `COPY` reads. No buffer decouples them, and the module uses one carrier. | `source.rs` `copy_stream_to_sink`; `lib.rs` `max_carriers() == 1`; `e2e/bandwidth_netem.sh` | PASS |
| i | The read-back runs the source code path against the **destination**: `verify` passes the destination's own parameters, so it cannot reach the source host. | `lib.rs` `PostgresDestination::verify` | PASS |
| j | Nothing on the source side issues `BEGIN`, `START TRANSACTION` or `SET TRANSACTION` of its own making, so the read-only default of the session cannot be relaxed from inside. Phase 4 § 4.2 turns this into an enforced allowlist. The one exception is the hot backup's constant `BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY`, sent by a method that takes no SQL and opens a transaction that cannot write. | grep over `source.rs`, `introspect.rs`, `immutability.rs`, `connect.rs`; `read_only_client_has_no_write_methods` | PASS |

### Connections, timeouts and keepalives

A run opens **one connection to the bootstrap database plus one per database it
copies**, and every phase — the fingerprint audits, introspection, the row
counts and the `COPY` stream — borrows them from the same pool. A connection the
driver reports as closed is replaced once, so a restarted server does not fail
the run at the first statement after it.

Source sessions carry `lock_timeout=30s`: a table somebody else holds under
`ACCESS EXCLUSIVE` fails the run inside half a minute, with the server's own
`canceling statement due to lock timeout` and the relation being read, instead
of waiting for a lock that may never be released. They do **not** bound
`statement_timeout` (a `COPY` of a large table is legitimately long) and set
`idle_in_transaction_session_timeout=0`, since the module opens no long
transactions of its own. Destination sessions set neither timeout: DDL and
`REFRESH MATERIALIZED VIEW` take locks and time on purpose.

Both sides enable TCP keepalives (idle 30 s, interval 10 s, 3 retries), so a
peer that disappears without closing its socket surfaces as a failed run rather
than a process waiting forever.

Two phase tags are deliberately kept as they are: `immutability.rs` and
`introspect.rs` tag their reads `Analyze` even when the session runs them as a
post-run audit or as the destination's re-introspection, because the callers
that use them in another phase re-wrap the error with that phase
(`dest.rs` `verify_catalog`, `lib.rs` `verify`).

## Immutability

The source is opened read-only and never written, and three independent layers
say so:

- **The type.** Source-side code holds a `ReadOnlyClient`, not a
  `tokio_postgres::Client`. It exposes `query`, `query_one`, `query_opt` and
  `copy_out` and nothing else — no `execute`, `batch_execute`, `copy_in`,
  `transaction` or `simple_query`, and no accessor that hands the inner client
  out. A write on the source path does not compile.
- **The statement allowlist.** Much of the SQL is built at runtime from catalog
  names, so every statement still passes a guard before it reaches the server.
  The guard removes literals, dollar-quoted bodies, quoted identifiers and
  comments, then requires a single statement whose head is `SELECT`, `WITH`,
  `SHOW`, `TABLE`, `VALUES` or `COPY … TO STDOUT`, with no writing keyword
  (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `ALTER`, `DROP`, `TRUNCATE`,
  `GRANT`, `SET`, `ANALYZE`, `VACUUM`, …) and no mutating function
  (`nextval`, `setval`, `lo_import`, `pg_advisory_lock`,
  `pg_terminate_backend`, …). A refusal reads
  `I-IMMUT guard refused a statement on the source: <first 120 characters>`.
  A table named `"delete"` or a literal `'DROP TABLE x'` is data, not syntax,
  and passes.
- **The server.** The session carries `default_transaction_read_only=on`, so a
  write that somehow got past both layers still fails at the backend.

On top of that the run is bracketed by two fingerprints, so any drift ends it as
a source mutation — except in a hot backup (below), which is not audited.

At connect time the source role is probed once
(`rolsuper`/`rolcreatedb`/`rolcreaterole`, plus
`information_schema.role_table_grants` for `INSERT`/`UPDATE`/`DELETE`/
`TRUNCATE`). A role that can write the source is only **warned** about —
`source role <user> can write to the source; a read-only role is recommended,
see docs/IMMUTABILITY.md` — never refused: which role to use stays the
operator's call.

The full vector list, the fingerprint contract and the read-only role recipes
per major are in [docs/IMMUTABILITY.md](../IMMUTABILITY.md).

## Privileges

Use a read-only source account able to inspect the required catalogs and `SELECT`
the copied tables. The destination needs an administrative account: it creates
roles/databases/schemas and applies ownership/grants. `--admin` marks this intent;
`--overwrite` (or `overwrite: true` in YAML) is required when restoring over
existing target databases. The destination disables new connections, terminates
existing sessions and recreates each target database from scratch.

That drop happens **before** the first payload byte arrives, so `--overwrite` is
not a safe retry of a failed window: the previous contents are already gone, and
the section above then removes what the failed run created. Take your own
snapshot first, or restore into a fresh database name and switch over, if the
previous contents must survive a failed attempt.

## Limits

- **Column numbering is not reproduced; column order is.** `pg_attribute.attnum`
  keeps counting the columns a table has dropped, so a source table that lost
  four columns numbers its 58th live column 62. A logical restore recreates only
  the live columns and numbers that same column 58. The gap is the source
  table's history, not its shape: no logical restore can rebuild it (only
  `pg_upgrade` can, because it keeps the physical files), and comparing attnum
  could never be a complete check anyway — a column dropped from the *end*
  leaves no gap at all. The read-back therefore normalizes both sides and
  enforces the full column order, names, types, defaults and comments.
- PostgreSQL passwords are never captured or restored.
- Roles are cluster-wide and are **not** rolled back. The destination removes the
  databases a failed restore created, but roles it created stay behind; drop them
  by hand if the cluster must be returned to its exact prior state.
- The source account needs `SELECT` or `USAGE` on every sequence.
  `pg_sequences.last_value` is NULL both for a sequence that was never called
  and for one the role may not read, and `GRANT SELECT ON ALL TABLES` does not
  cover sequences — so a plan built without that grant used to reset every
  restored sequence to its start value, with the read-back (equally unable to
  read it) agreeing. Analysis now refuses, naming the sequence and the grant.
- Restoring a database-level ACL needs a superuser destination: the grants are
  applied last, but a source ACL that does not grant `CONNECT` to the restoring
  account would otherwise lock the read-back out of the database.
- This is logical, not physical/PITR replication.
- `sslmode=require`, `verify-ca`, and `verify-full` use rustls. Pass a private
  root CA as `-P sslrootcert=/path/to/ca.pem`; TLS modes are never downgraded.
  All three verify the chain *and* the hostname, i.e. they behave as
  `verify-full`. That is stricter than libpq, where `require` encrypts without
  verifying and `verify-ca` skips the hostname check: a self-signed or
  wrong-hostname server certificate is refused here in every mode. It fails
  closed, never open.
- Table items are streamed ordered by the C-collated text of the whole row so
  that the source stream and the destination read-back agree independently of
  physical row placement — a partitioned parent, in particular, is read in a
  different physical order on the two sides. `ORDER BY` over an expression is a
  blocking sort: no `COPY` byte leaves the source until the table is sorted, and
  a table larger than `work_mem` spills to the source's `pgsql_tmp`. That is the
  one place where a run touches source disk, and it is server-side temporary
  space rather than a staged copy of the payload — but it does mean the first
  byte of a very large table is not immediate. The **fingerprint** no longer
  sorts (it folds an order-independent commitment, below), so this is now the
  data stream alone; removing it too would require the destination read-back to
  compare a commitment instead of the streamed bytes, which the transport's
  per-item digest does not do today.
- Live version-matrix verification requires Docker: `e2e/postgres_matrix.sh
  10 11 12 13 14 15 16 17 18`. Cross-major syntax is `source:destination`, for
  example `e2e/postgres_matrix.sh 10:18 12:16 14:17 16:18`; destinations older
  than their source are rejected.
