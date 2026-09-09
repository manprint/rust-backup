# PostgreSQL module

`postgres` performs a logical PostgreSQL 10..=18 copy with pure-Rust catalog
introspection and binary `COPY`; it never calls `pg_dump`.

Database encoding and locale are restored independently of the destination
cluster defaults. The module uses `template0` automatically and preserves the
database locale provider using the catalog and DDL available in each supported
major (`libc` on 10–14, ICU metadata on 15+, ICU rules on 16+, and `builtin` on
17+).

## Captured and restore order

The source captures cluster roles, memberships, databases, schemas, extensions,
tables, sequence state, table data, constraints, indexes, grants and ownership
metadata. The destination restores in dependency order: roles and databases,
pre-data DDL, streamed table data, then post-data constraints/indexes, sequences
and grants. Source reads are read-only; data streams straight into the tunnel.

## Completion proof

The destination reconnects after apply, re-introspects the complete restorable
catalog, and compares it with the normalized source plan. It then reads every
table through deterministic binary `COPY` and must reproduce every item and the
whole-payload BLAKE3. Only after this proof and the source's full post-run
catalog/data fingerprint audit do both commands exit successfully. A database
that merely accepted DDL or rows cannot produce `RESTORE VERIFIED`.

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
tables, view options such as `WITH CHECK OPTION` or `security_barrier`,
column-level privileges, default privileges (`ALTER DEFAULT PRIVILEGES`), large
objects, user-defined collations, rules other than a view's own `_RETURN`, an
inheritance child whose inherited column is locally `NOT NULL`, or a `reg*`
column — whose binary `COPY` representation is a raw OID that names a different
object on the destination.

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

## Privileges

Use a read-only source account able to inspect the required catalogs and `SELECT`
the copied tables. The destination needs an administrative account: it creates
roles/databases/schemas and applies ownership/grants. `--admin` marks this intent;
`--overwrite` (or `overwrite: true` in YAML) is required when restoring over
existing target databases. The destination disables new connections, terminates
existing sessions and recreates each target database from scratch.

## Limits

- PostgreSQL passwords are never captured or restored.
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
  physical row placement. `ORDER BY` over an expression is a blocking sort: no
  `COPY` byte leaves the source until the table is sorted, and a table larger
  than `work_mem` spills to the source's `pgsql_tmp`. That is the one place
  where a run touches source disk, and it is server-side temporary space rather
  than a staged copy of the payload — but it does mean the first byte of a very
  large table is not immediate. Removing it needs an order-independent row
  commitment (a commutative fold), which is deliberately left to a later
  version rather than half-built here.
- Live version-matrix verification requires Docker: `e2e/postgres_matrix.sh
  10 11 12 13 14 15 16 17 18`. Cross-major syntax is `source:destination`, for
  example `e2e/postgres_matrix.sh 10:18 12:16 14:17 16:18`; destinations older
  than their source are rejected.
