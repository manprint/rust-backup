# Changelog

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
