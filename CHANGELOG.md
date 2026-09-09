# Changelog

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
