# Rust-backup severe implementation audit — 2026-07-29

## Outcome

The audit found correctness failures that could produce false success, hangs,
lost peer errors, and incomplete destination output. The identified issues in
the audited paths were fixed and covered by regressions. The final
non-privileged gate and e2e matrix are green. After a path-restricted NOPASSWD rule
was installed, all privileged netns/netem/disk-full suites were also run and
fixed to green.

## Correctness and concurrency findings fixed

| Severity | Finding | Resolution and regression |
|---|---|---|
| S1 | The source could return success after `Done` while destination apply or final verification was still failing. A one-way acknowledgement could also be discarded during TLS/yamux close. | Required `Done -> CompleteAck -> CompleteAckAck`; one- and four-carrier failure regressions preserve the destination reason, and plain/TLS relay e2e passes. |
| S1 | In parallel sessions, a consumer relay stream arriving just before provider registration was dropped, leaving the source waiting indefinitely. | Relay now waits at most 10 seconds for registration. A deterministic consumer-first test and the watched two-target e2e pass. |
| S2 | Destination abort did not interrupt a source sleeping in rate limiting or blocked in a carrier write. | `PacedSink` now selects every wait/write against the abort watch; regressions cover both paths and a 50-item workload starts at most three items after an item-1 abort while preserving the peer reason. |
| S2 | The multi-carrier source control watcher could remain detached on setup or terminal-control errors. | Watcher creation is delayed until carrier setup succeeds and every exit cancels and joins it with a bound. |
| S2 | EOF was accepted as `StreamEnd`; an incomplete item could be hidden by `StreamEnd`; chunk offsets were not checked. | Parser now requires explicit, complete termination and exact contiguous offsets. The fault bank exercises these cases over one and four carrier paths. |
| S2 | Interrupted filesystem restore could leave a truncated file at its final path. | Uncommitted active files are removed on drop; a regression verifies interruption cleanup. |
| S2 | S3 fidelity validation did not reject non-default object ACLs. | Preflight checks every object ACL and accepts only the owner-only `FULL_CONTROL` form, including MinIO's compatible representation. Unit, MinIO, and credential-gated AWS coverage were added. |
| S3 | `e2e/fault_matrix.sh` claimed to run the protocol fault bank but selected only `session_test`. | It now executes both suites; the data-driven bank asserts exactly 26 cases. |
| S1 | Direct QUIC reported connection establishment but plan exchange deadlocked because `open_bi` is invisible to the acceptor until the opener writes. | Direct carriers now exchange `STREAM_READY`; loopback regressions and the real namespace transfer pass. |
| S1 | Single-carrier ENOSPC sent an abort but left the source blocked in write until an external watchdog. | The source now reads peer control concurrently, writes are abort-aware, teardown is graceful, and a bounded race preserves the destination reason. In-process and real ext4 regressions pass. |
| S1 | Concurrently polling the read half of the same yamux stream used for single-carrier payload writes intermittently stalled the first large chunk until the 30 s idle timeout. | Current peers negotiate a dedicated data stream even for one carrier; serde defaults preserve the legacy multiplexed wire layout. A warm 20-run plain relay stress and five TLS runs pass. |
| S2 | Privileged restore applied mode before chown, which silently cleared setuid/setgid. The old root test also cleared the source bit before comparing it. | Ownership now precedes final mode application and the fixture sets 04755 after chown; unit and privileged fidelity tests pass. |
| S2 | The bandwidth test shaped destination egress (ACKs), not coordinator-to-destination payload, producing a false 5 Mbit/s claim. | qdisc moved to the coordinator's destination-facing egress; the final 200 MiB run took 354 s and source RSS remained 9,908 KiB. |
| S3 | Exact-path sudo hid rustup/cargo and made every privileged script fail with exit 127. | Shared bootstrap builds as the invoking sudo user with explicit toolchain paths and avoids root-owned workspace artifacts. |

## Security and dependency audit

The first `cargo audit` found vulnerable paths through `anyhow`,
`quinn-proto`, `crossbeam-epoch`, and legacy `rustls-webpki`, plus the
unmaintained `rustls-pemfile`. The vulnerable crates were updated, the AWS SDK
was moved off its legacy TLS feature path, and PEM parsing now uses rustls
`pki_types`. Final result: `cargo audit` passes while scanning 420 dependencies.

## Tests observed on the final tree

| Command | Result |
|---|---|
| `bash scripts/gates.sh` | `ALL GATES PASSED`; fmt, Clippy with `-D warnings`, all-feature build/tests, no-default-feature build, help parity; zero ignored tests |
| `bash e2e/full_matrix.sh` | 8 passed, 0 failed; plain and TLS relay at 1/4 carriers, two-target session, protocol fault bank, MinIO; privileged suite explicitly skipped |
| `RUST_BACKUP_PRIVILEGED=1 bash e2e/full_matrix.sh` | 12 passed, 0 failed; the preceding 8 plus filesystem metadata, real ENOSPC, transport netns and bandwidth/netem |
| `bash e2e/postgres_matrix.sh 10 12 14 16 18` | 25 passed, 0 failed |
| `bash e2e/mongodb_matrix.sh 4 5 6 7 8` | 20 passed, 0 failed |
| `bash e2e/s3_minio_test.sh` | base transfer, injected multipart abort cleanup, and source immutability passed |
| `cargo audit` | passed; 420 dependencies scanned |
| `sudo -n e2e/filesystem_netns_test.sh` | 4 passed, 0 failed |
| `sudo -n e2e/filesystem_disk_full.sh` | 5 passed, 0 failed |
| `sudo -n e2e/transport_netns_test.sh` | 9 passed, 0 failed |
| `sudo -n e2e/bandwidth_netem.sh` | 6 passed, 0 failed; final aggregate measured 354 s / 9,908 KiB RSS under netem and 66 s / 10,228 KiB under the application cap |

The aggregate matrix now includes TLS with four carriers. Long-running session
scripts have explicit watchdogs, so a regression becomes a bounded failure
with retained diagnostics instead of hanging CI.

The installed sudoers wildcard matches executables in a user-writable repository
directory. It was sufficient to run the privileged matrix, but it is effectively
an arbitrary-root grant, not a security boundary. It should be limited to a
disposable test host or replaced by a root-owned runner that validates its inputs.

## Remaining plan gaps and external blockers

- F2.3's Docker-backed, backend-specific fault matrix of at least 20 injected
  cases is still not implemented. The 26-case in-process wire/protocol bank is
  complete, but it is not a substitute for backend process/network faults.
- The remaining F6 NAT classifier/port-mapping/lab work is still open and is
  explicitly a v0.2, non-QA blocker in the plan.
- Real AWS coverage is implemented in `e2e/s3_aws_test.sh`, but remains
  credential-gated and was not run on this host.
- F3.4's measured one-vs-four-carrier bandwidth comparison remains open.
- Manual QA walkthrough and final independent sign-off remain human gates.

These gaps are intentionally not represented as passing or silently skipped in
the plan/status documents.
