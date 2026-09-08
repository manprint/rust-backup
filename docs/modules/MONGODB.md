# MongoDB module

`mongodb` performs a logical MongoDB 4..=8 copy using the Rust driver and BSON
streaming; it never calls `mongodump`.

## Captured and restore order

The plan captures databases, ordinary collections, collection options, index
definitions, document/size estimates, and user/role metadata for visibility and
immutability checks. Restore creates collections, streams BSON documents in
bounded batches, then creates indexes.

## Completion proof

After apply, the destination re-lists collections, options and indexes and
compares the restorable catalog with the source plan. It then reads every BSON
document in deterministic `_id` order and reproduces every item and payload
BLAKE3 commitment. The source also fingerprints all documents before and after
the run. Empty collections, nested documents, arrays, nulls, compound/partial
indexes, abort cleanup and overwrite retries are covered by the live matrices.

## The plan is treated as hostile input by the destination

Every namespace is re-derived from the received plan, which is peer-controlled.
The destination refuses a system database (`admin`, `local`, `config`), a
`system.*` collection and any name carrying `/ \ . space NUL $ "` — both in
preflight and again immediately before each `createCollection`, so the check
cannot be bypassed through a validate/apply race. Without this a plan naming
`admin.system.users` would, under `--overwrite`, drop every account on the
destination cluster.

Streamed BSON is self-delimiting by a 4-byte length prefix. A prefix below the
minimum document size, a negative one, or one past MongoDB's own 16 MiB document
limit is refused outright rather than treated as "wait for more bytes", which
would buffer a whole item in memory and break I-NOTEMP.

Documents are inserted `ordered(true)`: a capped collection's natural order is
its insertion order, so unordered batching would not reproduce it.

## Views, time-series and other non-ordinary namespaces

Only ordinary collections copy. A source carrying a view, a time-series
collection or any other non-`collection` namespace **fails analysis**, naming
each one, instead of silently producing a partial copy that verification would
then certify as a faithful backup. Set `allow_skipped_namespaces: true`
(`-P allow_skipped_namespaces=true`) to accept that partial copy deliberately;
the run then logs a warning listing exactly what it left behind.

Index specs and collection options travel as JSON. BSON binary inside one of
them cannot survive that container with its type intact — `serde_json` renders it
as an array of integers, which MongoDB accepts as a *different* value, and
re-introspection reproduces the same array so verification would agree with the
corruption. Such a spec fails analysis, naming the offending field path.

## Privileges

The source needs read access to listed databases/collections and catalog commands
such as `listCollections`; user visibility may require `usersInfo` privileges.
The destination needs permission to create/drop collections and indexes. Use
`--overwrite` (or `overwrite: true` in YAML) only when replacing existing
collections is intended.

## Limits

- User credentials are not exposed by `usersInfo`; users and roles are therefore
  not recreated.
- Views and time-series collections are not copied; the run refuses to proceed
  unless `allow_skipped_namespaces` is set (see above).
- Discrete host/port parameters are plaintext unless the server policy provides
  transport protection. A URI with `?tls=true` is the current TLS path.
- Live matrix verification requires Docker: `e2e/mongodb_matrix.sh 4 5 6 7 8`.
  Cross-major syntax is `source:destination`, for example
  `e2e/mongodb_matrix.sh 4:8 5:7 6:8`.
