# Changelog

## Unreleased

- Added relay end-to-end coverage, TLS control-listener support, secret-file
  input, bounded session accounting, and S3 fidelity preflight checks.
- Multi-carrier transfers now negotiate the safe count on the wire and bind
  relay data streams by explicit carrier identity; filesystem restores are
  plan-ordered and item-pinned without intra-item striping.

## Versioning

This project follows semantic versioning. Breaking plan/wire or configuration
changes require a major or minor release and an explicit `PLAN_FORMAT_VERSION`
review; fixes and additive backward-compatible options are patch releases.
