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

## A failed restore removes what it created

A restore creates collections and then fills them, so an interrupted run leaves
collections holding part of the source's documents. Nothing certifies them —
there is no `RESTORE VERIFIED` — but a half-filled collection is
indistinguishable from a small one on inspection.

On any failure the destination therefore drops the collections **this run
created**, named in a `WARN` line, and re-checks the namespace guard first so a
cleanup path can never be the one that drops a `system.*` namespace. A
collection the run found already present is never touched: without
`--overwrite` the run never wrote to it, and with `--overwrite` it was dropped
before the restore began. A cleanup that itself fails is reported as an `ERROR`
naming the namespace; the original failure is what the process exits with.

The live proof is `e2e/fault_matrix.sh mongodb`.

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

## Immutability

The source only reads, and the type says so: source-side code holds a
`ReadOnlyClient` and its `ReadOnlyDatabase`, which expose `list_collections`,
`list_collection_names`, `find`, `count_documents`,
`estimated_document_count`, `list_indexes` and `run_command` — and nothing
else. There is no accessor returning a `Collection` or a raw `Database`, so a
write on the source path does not compile.

`run_command` is the one call that takes the operation as data, so it is also
guarded: the command's first key must be one of `collStats`,
`listCollections`, `listIndexes`, `usersInfo`, `rolesInfo`, `dbStats`,
`buildInfo`, `connectionStatus`, `hello`, `isMaster` or `ping`. Anything else —
`insert`, `drop`, `createIndexes`, `aggregate` (whatever its pipeline),
`applyOps`, `setProfilingLevel` — is refused with
`I-IMMUT guard refused command <name> on the source`.

The source client also pins `readPreference=primary` and `readConcern=local`,
overriding whatever an operator's URI asked for: the two fingerprint audits that
bracket a run must read the same node under the same visibility rules, or
replication lag on a secondary would be reported as source mutation.

MongoDB has no server-side read-only session switch, so the remaining layer is
the role: use a `read`-only backup user. The run is bracketed by two
fingerprints, and drift ends it as a source mutation. The vector list, the
fingerprint contract and the read-only role recipe are in
[docs/IMMUTABILITY.md](../IMMUTABILITY.md).

## Privileges

The source needs read access to listed databases/collections and catalog commands
such as `listCollections`; user visibility may require `usersInfo` privileges.
The destination needs permission to create/drop collections and indexes. Use
`--overwrite` (or `overwrite: true` in YAML) only when replacing existing
collections is intended. The drop happens **before** the first document arrives,
so a failed run does not leave the previous contents behind — restore into a
fresh collection name and switch over if they must survive a failed attempt.

## Limits

- User credentials are not exposed by `usersInfo`; users and roles are therefore
  not recreated.
- **More than 200 000 collections in one run is refused.** One plan item per
  collection, bounded by the plan's item ceiling (`MAX_PLAN_ITEMS`), which is
  what keeps a peer-supplied plan from dictating the destination's allocation.
  The source raises it itself, naming the ceiling, before the plan is sent.
- **Every document is read three times per run**: the fingerprint before, the
  transfer, and the fingerprint after. The fingerprint hashes the full
  `_id`-ordered content of every collection — a sample could not catch a
  mutation outside it — and I-IMMUT requires it on both sides of the run,
  including a failed one. The destination read-back adds a fourth full read on
  its own side.
- Source cursors are opened with `noCursorTimeout`, because backpressure is
  supposed to stall them: the cursor advances only when the destination has
  taken the previous chunk, and a `--max-rate` cap stalls it further. Without
  that flag a destination slower than the server's `cursorTimeoutMillis` (10
  minutes by default) killed the run with `CursorNotFound` on an untouched
  source. A deployment that forbids `noCursorTimeout` (some hosted shared tiers
  do) will refuse the `find` outright rather than fail halfway.
- Views and time-series collections are not copied; the run refuses to proceed
  unless `allow_skipped_namespaces` is set (see above).
- Discrete host/port parameters are plaintext unless the server policy provides
  transport protection. A URI with `?tls=true` is the current TLS path.
- Live matrix verification requires Docker: `e2e/mongodb_matrix.sh 4 5 6 7 8`.
  Cross-major syntax is `source:destination`, for example
  `e2e/mongodb_matrix.sh 4:8 5:7 6:8`.
