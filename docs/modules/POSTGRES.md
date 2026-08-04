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

## Privileges

Use a read-only source account able to inspect the required catalogs and `SELECT`
the copied tables. The destination needs an administrative account: it creates
roles/databases/schemas and applies ownership/grants. `--admin` marks this intent;
`--overwrite` (or `overwrite: true` in YAML) is required when restoring over
existing target databases. The destination disables new connections, terminates
existing sessions and recreates each target database from scratch.

## Limits

- PostgreSQL passwords are never captured or restored.
- This is logical, not physical/PITR replication.
- `sslmode=require`, `verify-ca`, and `verify-full` use rustls. Pass a private
  root CA as `-P sslrootcert=/path/to/ca.pem`; TLS modes are never downgraded.
- Live version-matrix verification requires Docker: `e2e/postgres_matrix.sh
  10 11 12 13 14 15 16 17 18`. Cross-major syntax is `source:destination`, for
  example `e2e/postgres_matrix.sh 10:18 12:16 14:17 16:18`; destinations older
  than their source are rejected.
