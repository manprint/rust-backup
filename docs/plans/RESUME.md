# Implementation status — v0.1.0, QA handoff

The current executable work is tracked by
[RUST_BACKUP_PLAN_V3.md](RUST_BACKUP_PLAN_V3.md) — the final plan: bug register
(B1–B12), phases F1–F7, and the QA exit criteria. Live Docker and privileged
network-namespace results belong in `V1_LIVE_RESULTS.md`; do not mark a matrix
complete until its recorded command and result exist.

[RUST_BACKUP_PLAN_V2.md](RUST_BACKUP_PLAN_V2.md) is retained for the status of
what V2 already closed; rows it lists as still open are restated in V3 (F2–F6).
The historical V1 plan is retained for rationale only. Its embedded resume block
is superseded by this file.

## What proved each phase (F7.5)

One row per plan phase, with the command that actually ran. A row without a
command that ran is not done — that rule is the whole point of the table.

| Phase | Proving command | Result |
|---|---|---|
| F1.1 ordered demux, item-pinned carriers | `cargo test --all-features` + `RUST_BACKUP_E2E_CARRIERS=4 bash e2e/relay_smoke.sh` | PASS |
| F1.2 carrier negotiation, plain + TLS | `bash e2e/relay_smoke.sh`, then the same with `RUST_BACKUP_E2E_TLS=1`, each at 1 and 4 carriers | PASS=5 FAIL=0 per run; each asserts the *negotiated* count, not the requested one |
| F1.3–F1.4 abort handshake, verification handshake | `cargo test --all-features` (rb-core + rb-transport banks) | 233 passed, 0 ignored |
| F1.5 per-module carrier caps | `cargo test --all-features -- clamp_carriers` and the CLI bank, plus the live cap proof in `e2e/s3_minio_test.sh` | PASS |
| F1.6 direct/relay fallback | `sudo -n "$PWD/e2e/transport_netns_test.sh"` | PASS=9 FAIL=0 |
| F2.1–F2.2 in-process fault banks | `bash e2e/fault_matrix.sh protocol` (26-case core bank) | PASS |
| F2.3 live fault matrix, all four modules | `bash e2e/fault_matrix.sh` | **CASES=21 PASS=53 FAIL=0**, no `SKIP` |
| F2.4 real ext4 ENOSPC | `sudo -n "$PWD/e2e/filesystem_disk_full.sh"` | PASS=5 FAIL=0 |
| F2.5 immutability under concurrent load | `bash e2e/fault_matrix.sh immutability` | PASS=2 FAIL=0 (both directions) |
| F3.1 filesystem ownership in a netns | `sudo -n "$PWD/e2e/filesystem_netns_test.sh"` | PASS=4 FAIL=0 |
| F3.2 direct, fallback, active loss | `sudo -n "$PWD/e2e/transport_netns_test.sh"` | PASS=9 FAIL=0 |
| F3.3 netem, backpressure, rate cap, RSS ceiling | `sudo -n "$PWD/e2e/bandwidth_netem.sh"` | **PASS=16 FAIL=0** — 200 MiB at 5 Mbit/s + 80 ms in 355 s, 16 MiB under a 256 KiB/s cap in 66 s, peak source RSS 10.9 MiB against a 192 MiB ceiling |
| F3.4 one-vs-four-carrier speed proof | `sudo -n "$PWD/e2e/bandwidth_netem.sh"` (final section) | 4 carriers 44 s vs 1 carrier 43 s (ratio 102 %), byte-identical restored trees, and 4 carriers observed negotiated |
| F4.1–F4.2 S3 fidelity and MinIO matrix | `bash e2e/s3_minio_test.sh`; real AWS via `RUST_BACKUP_AWS_E2E=1 bash e2e/s3_aws_test.sh` | PASS, including the per-module carrier cap (asks for 4, negotiates 1); the AWS run is credential-gated and prints SKIP without it |
| F5.1–F5.2 transport hygiene | `cargo test --all-features` (rb-transport bank) | PASS |
| F5.3 typed exit codes | `bash e2e/fault_matrix.sh exitcodes` | PASS=5 FAIL=0 |
| F6.1, F6.3, F6.4, F6.6, F6.8 | `cargo test --all-features` (holepunch/candidate banks) | PASS |
| F6.2, F6.5, F6.7 | **not implemented — v0.2.** `docs/TRANSPORT.md` names them so QA does not file them as regressions | open by design |
| F7.1 QA guide | `docs/QA_GUIDE.md` rewritten: build, run shape, one worked example per module, dry run, reading progress, failure triage, test matrix | written; criterion 16 still needs an independent reader |
| F7.2 help ↔ docs parity | `bash scripts/help_parity.sh` (in `scripts/gates.sh`, both directions) | PASS |
| F7.3 CI completes the matrix | `.github/workflows/{ci,e2e,security}.yml` | fault matrix, relay-only test run, audit/deny, parity and the privileged netns jobs all run in CI |
| F7.4 single entry point | `bash e2e/full_matrix.sh` (privileged behind `RUST_BACKUP_PRIVILEGED=1`, DB matrices behind `RUST_BACKUP_FULL_DB_MATRIX=1`, both printing `SKIP` when off) | PASS |
| F7.5 release metadata | this file + `CHANGELOG.md` `0.1.0` | done |
| F7.6 final read | the sign-off below | done |

## F7.6 — final invariant read, sign-off

Re-read against the code on 2026-09-09, after the F2/F3 work of this session.

- **I-IMMUT.** `run_source_limited` (`crates/rb-core/src/session.rs`) captures
  `Source::fingerprint()` before analyze and audits it on *every* exit path,
  including a failed or aborted run — the failure path is exactly where a
  half-applied source write would hide. The plan-rejection path added this
  session returns through the same `Err` arm, so it is audited too. Assertion
  **A** of all 17 live fault cases re-checks the source tree/checksum
  independently of the process's own claim.
- **I-NOTEMP.** No production path touches `std::env::temp_dir`, `tempfile` or a
  staging file — the only two `temp_dir` uses in the workspace are inside
  `#[cfg(test)]` modules. `ChunkSink::send_chunk` refuses anything above
  `wire::CHUNK_SIZE` and the receiver refuses an oversized length prefix before
  allocating. The rollback added to PostgreSQL and MongoDB introduces no
  buffering: it re-uses the bootstrap admin connection that was already open.
- **I-ERRORS.** Zero `unwrap`/`expect`/`panic!`/`unreachable!`/`todo!` outside
  `#[cfg(test)]` in any crate. Every crate root now declares
  `#![forbid(unsafe_code)]` plus the unwrap/expect/panic denial, and
  `scripts/crate_invariants.sh` fails the gate if one goes missing — `rb-core`
  had lost `forbid(unsafe_code)` entirely. Exactly two `#[allow]` attributes
  remain in non-test code, each with a one-line justification above it
  (`clippy::too_many_arguments` on the accept-boundary handler, `dead_code` on a
  test-only deserialization field); the two stale ones on
  `Postgres{Source,Destination}::params` were removed.
- **I-MODULAR.** `max_carriers` is a defaulted trait method on both
  `BackupModule` and `Destination`, so a new module opts in without a core
  change. This session's product fixes are confined to `rb-postgres` and
  `rb-mongodb`, except the plan-rejection frame ordering, which is core protocol
  behaviour and belongs there.
- **I-BANDWIDTH.** An item rides exactly one carrier (`item_id % carriers` in
  `channel.rs`), and the destination pulls items in plan order from their pinned
  carrier — no intra-item striping and no mpsc merge that would let item N+1
  overtake item N. The rate cap is applied on the sink, so it slows the source
  backend rather than filling a read-ahead buffer.
- **I-OBSERV.** Both roles emit phase, bytes, rate and item counts through
  `tracing` + `rb_core::progress::Progress`, ending at `status="verified"`
  `100.0%` with `BACKUP VERIFIED` / `RESTORE VERIFIED`. The fault matrix relies
  on this: a case that never shows transferred bytes in the destination's
  progress output fails outright rather than passing on an empty target. This
  read found one thing missing and fixed it: the *negotiated* carrier count was
  never logged, only a downgrade was, so nothing could distinguish a
  four-carrier run from a silent fallback to one — including the tests that
  claimed to exercise four. Both peers now log
  `negotiated data plane carriers=N`, and three e2e scripts assert it.

**QA exit criteria.** 1–15 and 17 are backed by a recorded command and result
here and in `V1_LIVE_RESULTS.md`. **Criterion 16 is deliberately still open**:
`docs/QA_GUIDE.md` has to be executed end to end by someone who did not write
it, and that cannot be self-certified. Criterion 7 asks for ≥20 cases with
`FAIL=0`; the matrix now prints its own case count (`CASES=`) next to the
assertion counts so the criterion can be read off the summary line.

**Verdict: ready for staging QA**, with two stated limits — criterion 16 needs an
independent human pass, and phase F6's remaining NAT rows (F6.2, F6.5, F6.7) are
v0.2 and documented as such in `docs/TRANSPORT.md`.
