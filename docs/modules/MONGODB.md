# MongoDB module

`mongodb` performs a logical MongoDB 4..=8 copy using the Rust driver and BSON
streaming; it never calls `mongodump`.

## Captured and restore order

The plan captures databases, ordinary collections, collection options, index
definitions, document/size estimates, and user/role metadata for visibility and
immutability checks. Restore creates collections, streams BSON documents in
bounded batches, then creates indexes.

## Privileges

The source needs read access to listed databases/collections and catalog commands
such as `listCollections`; user visibility may require `usersInfo` privileges.
The destination needs permission to create/drop collections and indexes. Set
`overwrite=true` only when replacing existing collections is intended.

## Limits

- User credentials are not exposed by `usersInfo`; users and roles are therefore
  not recreated.
- Views and time-series collections are skipped; only ordinary collections copy.
- Discrete host/port parameters are plaintext unless the server policy provides
  transport protection. A URI with `?tls=true` is the current TLS path.
- Live matrix verification requires Docker: `e2e/mongodb_matrix.sh 4 5 6 7 8`.

