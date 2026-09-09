# Full-project review — 2026-09-09

Scope: every crate in the workspace, the CLI surface, the e2e suite and the CI
workflows, read for defects rather than for plan progress. A finding is listed
here only if it was **fixed and observed**: unit-tested, and — where the defect
could only appear against a real backend — reproduced and then re-run live.

The recurring shape of the serious findings is the same one the design warns
about: `verify_catalog`/`fingerprint` compares a source model against a
re-introspection *through the same model*, so anything the model cannot express
is lost invisibly **and then certified as a faithful copy**. Every such gap is
now either reproduced or refused up front.

## rb-transport — `dad88c3`

| id | Defect |
|----|--------|
| T1 | `Delimited` read was unbounded; `MAX_FRAME_LENGTH` was declared and never enforced |
| T2 | No deadline on the handshake reads (first `Register`/`Connect`, auth challenge and answer) |
| T3 | Provider registration was a `contains_key` + `insert` TOCTOU, and the drop guard could evict another provider's entry |
| T4 | `relay()` read `STREAM_READY` with no deadline while holding a `max_conns` permit |
| T5 | No consumer exclusivity per channel: a second destination interleaved its substreams into the first one's session |
| T6 | `tune_tcp` was missing on the client control socket (a lost bore invariant) |
| T7 | The TLS handshake had no deadline (a lost bore invariant) |

## rb-filesystem — `7046cca`

| id | Defect |
|----|--------|
| FS1 | **Local privilege escalation.** `apply_metadata` used `chmod(2)`, which follows symlinks, so a planted symlink in the destination tree could set setuid on any host path during a root restore. Now `open(O_NOFOLLOW)` + `fchmod`; payload writes are `O_NOFOLLOW` too |
| FS2 | `.` was accepted as an entry path, so a plan could re-own/re-permission the destination root |
| FS3 | Duplicate entry paths and kind/target mismatches were accepted |
| FS4 | A plan carrying xattrs was accepted and silently dropped them |
| FS5 | Metadata verification depended on the peer's entry ordering |
| FS6 | Non-UTF-8 paths and symlink targets were lossily renamed through `to_string_lossy`, then verified mangled-against-mangled |
| FS7 | Pre-1970 mtimes were clamped to the epoch |
| FS8 | The source walk had unbounded recursion |

## rb-s3 and the session/CLI boundary — `a579aa3`

| id | Defect |
|----|--------|
| S3-1 | Pagination trusted `is_truncated`: a truncated listing became a self-certifying incomplete backup, and the inverse spun forever. Token-only now |
| S3-2 | An error from `source.next()` left an orphaned (billable) multipart upload, and `abort_upload` swallowed its own failure |
| S3-3 | The part buffer was sized from the peer's `estimated_bytes` — a multi-GiB allocation from a hostile plan |
| S3-4 | The destination trusted the received plan; it now re-runs the unsupported-feature gate, requires `size == estimate` and rejects duplicate target keys |
| S3-5 | `Content-Encoding`/`Cache-Control`/`Content-Disposition`/`Content-Language` were dropped silently; `Expires` cannot be re-encoded and is refused at preflight |
| S3-6 | `create_bucket` without `CreateBucketConfiguration` failed outside `us-east-1` |
| S3-7 | `path_style: false` was silently ignored when an endpoint was set |
| S3-8 | `BackupModule::max_carriers` was dead code next to `Destination::max_carriers` |
| S3-9 | One preflight check per object (100k log lines) instead of one bounded summary |
| CLI-1 | A parallel session collapsed typed errors, so the exit code was always 1 |
| CORE-1 | `Progress::complete()` overwrote `items_total`, so a short run could not render as short |

## rb-mongodb — `972792b`

| id | Defect |
|----|--------|
| MG1 | No namespace validation on the received plan: `admin.system.users` plus `--overwrite` would drop every account. `check_namespace` now runs at preflight **and** before each `createCollection` |
| MG2 | The BSON length prefix was used unchecked, so a negative or oversized prefix buffered a whole item (OOM, and a breach of I-NOTEMP). Bounded by the 16 MiB document limit |
| MG3 | Collection options were re-encoded with `bson::to_document`, which cannot read their extended-JSON shape |
| MG4 | `insert_many(...).ordered(false)` did not reproduce a capped collection's natural order |
| MG5 | The `--overwrite` drop ignored every failure; it now tolerates only `NamespaceNotFound` |
| MG6 | Views and time-series namespaces were skipped silently and then certified. Analysis fails; `allow_skipped_namespaces` opts in and warns |
| MG7 | BSON binary in an index spec or collection options became an integer array through JSON, and the re-introspection agreed |
| MG8 | A transient `usersInfo` failure was reported as `SourceMutated` |

## rb-postgres — `f746b87`, `98144a7`, this commit

| id | Defect |
|----|--------|
| PG1 | Classic `INHERITS`: the parent was read without `ONLY` and the child was also its own item, so **every child row was restored twice** — and the read-back, reading the parent the same expanding way, agreed |
| PG2 | Materialized views were created in pre-data and restored **empty**, silently. `WITH NO DATA` + `REFRESH` in post-data; matview indexes are captured |
| PG3 | `pg_sequences.last_value` is NULL both for "never called" and for "no privilege", so with the documented read-only role **every sequence reset to its start value** and the read-back agreed. Refused now, naming the missing grant |
| PG4 | **SQL injection.** Role and database GUC values were interpolated raw into a multi-statement `batch_execute`, so any unprivileged source user could escalate on the destination. The same defect broke every value containing a space |
| PG5 | Constraints and indexes that PostgreSQL materialises on partitions were re-emitted, aborting the restore after the data load. `conislocal` filter plus `ALTER INDEX … ATTACH PARTITION`, so the parent index ends up valid rather than `ON ONLY` |
| PG6 | Restore order ignored dependencies: `SET check_function_bodies = false`, defaults and identity clauses after functions, views topologically ordered |
| PG7 | ACLs were GRANT-only (no `REVOKE ALL … FROM PUBLIC`), the database ACL was not modelled, and identity-sequence ACLs were skipped — found live: their loss made any `GRANT ON ALL SEQUENCES` cluster unrestorable |
| PG8 | Triggers, RLS, user types, aggregates, foreign tables, view options, column/default ACLs, large objects, user collations, rules and child-only `NOT NULL` were copied away silently; all are refused in analyze now, with `allow_unsupported_objects` to accept a partial copy |
| PG9a | Row-text ordering depended on session GUCs; `extra_float_digits`, `DateStyle`, `IntervalStyle`, `TimeZone`, `bytea_output` and `lc_monetary` are pinned on every connection |
| PG10 | Identity columns lost their sequence parameters, and `setval` targeted a name the destination never created |
| PG11 | `reg*` columns ship a raw OID through binary `COPY`, and the read-back compares the same OID |
| PG12 | The overwrite rollback swallowed its own failure, leaving a database nobody can connect to and no hint |
| PG13 | Multi-level partition hierarchies were created out of order |
| PG14 | A partitioned parent reported ≈0 planned bytes |
| PG15 | **Found live.** `pg_get_viewdef` renders differently per major, so cross-major verification compared two *deparsers* and **any view failed any cross-major restore**. The expected definition is re-rendered through the destination's own deparser |
| PG16 | **Found live.** PostgreSQL 17 added `MAINTAIN`, so a 10→18 restore's owner ACL legitimately gains `m`; privilege letters newer than the source major are ignored when comparing |
| PG17 | **Found live.** PostgreSQL 17 allows an identity column on a partitioned table and propagates `attidentity` to every partition, but refuses `ALTER TABLE <partition> … ADD GENERATED`. The generator is emitted for the parent only |
| PG18 | **Found live (16:18).** PostgreSQL 18 catalogues every `NOT NULL` in `pg_constraint`, which makes a *named* or `NOT VALID` not-null constraint expressible; this build reads only `attnotnull` and would rebuild it default-named and validated. Refused on an 18+ source |
| PG9b | *Deferred, documented:* the sorted `COPY` is a blocking server-side sort that can spill to the source's `pgsql_tmp`. An order-independent commitment is left to a later version |

## rust-backup CLI

| id | Defect |
|----|--------|
| CLI-2 | `RUST_BACKUP_YES`, `RUST_BACKUP_INSECURE` and the target-side `RUST_BACKUP_UDP` were documented in `USAGE.md` and never wired, so a destination configured through the environment waited on the interactive prompt forever |
| CLI-3 | No module param had an env var, so a database password, an S3 secret key or a Mongo URI could only travel through argv — world-readable in `/proc/<pid>/cmdline` for the whole run |
| CLI-4 | Boolean env vars accepted only "true"/"false": `RUST_BACKUP_YES=1` aborted with `invalid value '1' for '--yes'`. This was already true of the shipped `RUST_BACKUP_OVERWRITE` |
| CLI-5 | `-P key=value` coerced every digit-only value to a JSON number, so `-P database=007` silently became `7` and a numeric password failed as "invalid type: integer" |

## rb-core

Read line by line (`channel.rs`, `wire.rs`, `session.rs`, `config.rs`,
`plan.rs`, `module.rs`, `verification.rs`, `error.rs`, `progress.rs`) with **no
defect found**. The properties that matter were re-checked explicitly: the frame
limit is enforced before allocation, plan bounds and `format_version` are
refused on the wire, the negotiated carrier count is clamped by each side's own
channel (so a hostile peer cannot amplify substream setup), the fingerprint
audit runs on every exit path including an abort, and both `Done` totals and the
per-item digests are compared against independently accumulated values.

## Test and CI coverage added

* 4 integration tests spawning the real binary for the environment contract
  (env-only run, boolean spellings, a misspelling still refused, flag beats env).
* Unit tests: 16 in rb-postgres, 2 in rb-mongodb, 5 in rb-s3, 6 in
  rb-filesystem, 8 in rb-transport, 4 in the CLI, 1 in rb-core.
* `e2e/fault_matrix.sh` asserts the process exit contract end to end (2, 3, 4
  and 7); nothing asserted a process status before.
* `e2e/postgres_matrix.sh`: 12 assertions per case (was 7) — cross-major
  fidelity probe, partition index validity, read-only-role sequence refusal,
  role/database settings round-trip — plus fixtures for classic inheritance, a
  populated matview, a view on a view, a function-calling column default, a
  table-returning function, a partitioned PK with an index, a GUC injection
  payload and a read-only source role.
* Cross-major dump comparison now normalizes two *renderer* differences that are
  not fidelity differences: pg_dump 13 moved `ATTACH PARTITION` to a later
  section, and pg_dump 18 prints a partition's inherited not-null constraint
  name.

## Dependency health

`h2` (RUSTSEC-2026-0258), `chacha20` (yanked) and `lru` (RUSTSEC-2026-0253,
reached through `aws-sdk-s3`) were updated; `mongodb` moved to 3.9. Three
workflow action pins whose SHA no longer matched their version comment were
repinned to the commit their tag names today.
