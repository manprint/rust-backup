# Changelog

## Unreleased

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
- Added a final `CompleteAck`/`CompleteAckAck` handshake: a source cannot report
  success before destination apply and digest verification finish, and the
  destination does not close before the acknowledgement is known to be received.
  Destination aborts now interrupt source pacing and blocked chunk writes, and
  truncated carriers/offset gaps are rejected.
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
