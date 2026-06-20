# CLAUDE.md — rust-backup

## What this is

`rust-backup` — a **modular, streaming, source→destination backup/restore** tool
(`#![forbid(unsafe_code)]`). It moves a backend's state from a *source* host to a
*destination* host over an efficient tunnel transport vendored from `bore` (TCP
relay + direct UDP/QUIC, hole-punching, carriers, automatic direct↔relay
fallback). A coordination server pairs the two sides on a named channel. **Nothing
is staged to disk on either host** — all data streams.

**Subcommands**
- `rust-backup <module> source <PARAMS>` — analyze (read-only) + stream out
- `rust-backup <module> destination <PARAMS>` — validate + restore 1:1
- `rust-backup server <OPTS>` — coordination server
- `rust-backup run --config session.yml` — multi-target session
- `rust-backup plan <module> source <PARAMS>` — dry-run: print plan only

**Modules** (`module ∈ postgres | mongodb | filesystem | s3`): postgres (cluster
fidelity, pg 10..=latest), mongodb (4..=8), filesystem (POSIX, ownership/perms on
Linux), s3 (AWS S3 + MinIO). All use **pure-Rust drivers — no external binaries**
(`pg_dump`/`mongodump` are NOT used).

The full design + phased roadmap is `docs/plans/RUST_BACKUP_PLAN.md`. Read it before
implementing anything. Vendored bore source for the transport port is in
`vendored-from-bore/`.

## Crate layout (workspace)

```
crates/rb-core        traits, plan, wire framing, channel abstraction, session runner, config, progress, errors
crates/rb-transport   coordination server + PairedChannel (relay now; direct QUIC = Phase 1)
crates/rb-postgres    } module crates — each implements rb_core::BackupModule.
crates/rb-mongodb     } New backends are NEW crates; never edit the core to add one.
crates/rb-filesystem  }
crates/rb-s3          }
crates/rust-backup    binary: CLI, config merge, module registry, session glue
```
`rb-core` is the only crate modules depend on. Transport is injected into
`rb_core::channel::DataChannel`, so modules are transport-agnostic and unit-test
against an in-memory channel.

## Non-negotiable invariants (NEVER break)

- **I-IMMUT — source is ALWAYS unaltered.** `Source` is read-only; the session
  audits `Source::fingerprint()` before and after every run (incl. aborted runs)
  and returns `BackupError::SourceMutated` on any drift. Source DB users must be
  read-only; filesystem opens read-only (`O_NOATIME` where possible). This is the
  most heavily tested invariant — see Phase 8.1.
- **I-NOTEMP — no temp files.** Data flows backend→`ChunkSink`→channel→`ChunkSource`
  →backend in ≤1 MiB chunks. Never buffer a whole item to disk (unlike bore
  `transfer.rs`, which stages for resume — we do NOT).
- **I-ERRORS — explicit, phase-tagged errors.** Every fallible API returns
  `rb_core::Result`; errors carry a `Phase`. No `unwrap`/`expect` in production
  paths (lint-enforced from Phase 8.2).
- **I-MODULAR.** A new target = a new crate implementing `BackupModule` +
  registration; zero core change.
- **I-BANDWIDTH — consumer-paced backpressure.** Chunk writes ride substream flow
  control (yamux/QUIC window). A slow destination stalls source writes → stalls
  source backend reads. Never add intra-item striping across carriers (reorder
  trap — one item rides one carrier; mirror bore's flow-pinning rule).
- **I-OBSERV.** Both source and destination emit clear progress (phase, bytes,
  rate, items) via `tracing` + `rb_core::progress::Progress`.
- **Plan is self-contained.** The destination validates and restores from the
  plan alone — no extra round-trip to the source.
- **Transport rules inherited from bore** (see `vendored-from-bore/` + the bore
  CLAUDE.md): client sends its `Register`/`Connect` BEFORE auth (yamux lazy
  substream — else deadlock); write `STREAM_READY` before splice; never
  `tokio::io::split` a `mux::Stream` across two tasks (yamux waker bug — one stream
  = one task); apply `tune_tcp` to every socket; `--max-conns` semaphore is the
  real bound. Direct path falls back to the warm relay per-connection; UDP never
  gates channel liveness.

## Model selection per task (target: minimize tokens)

Show the model used for every task/subtask.

- **Haiku 4.5** — codebase exploration, grep/lint, doc writing, mechanical/bulk
  edits, structured extraction. Sub-agent recon.
- **Sonnet 4.6** — default. Feature implementation, module logic, tests, debugging,
  refactors. The implementer for all module + transport work.
- **Opus 4.8** — architect/supervisor. Architecture across crates, the plan/DDL
  correctness gates, concurrency/hot-path review, the immutability proofs, final
  reads. Breaks work down for Sonnet/Haiku and validates it.

Start at Sonnet; drop to Haiku for bulk/mechanical + docs; escalate to Opus only at
the ⟦OPUS GATE⟧ rows in the plan.

## Workflow

1. **Analysis** → structured, self-contained output usable by downstream agents.
2. **Implementation** → phase by phase, sub-phase by sub-phase. Tests first or
   alongside. **Zero regressions** — a sub-phase that breaks an existing test is
   not done.
3. **Gates** (every sub-phase): `bash scripts/gates.sh` ⇒ `cargo fmt --all --check`
   · `cargo clippy --all-targets --all-features -- -D warnings` · `cargo build
   --all-features` · `cargo build --no-default-features` · `cargo test
   --all-features`. Plus the relevant `e2e/` script.
4. **Docs** — every behavior/API/invariant change updates the matching markdown.

## Build / test

```
cargo build --all-features          # full (udp on)
cargo build --no-default-features   # relay-only (no quinn)
cargo test --all-features
bash scripts/gates.sh               # the full gate
```

Skeleton status: `rb-core` complete + tested; `rb-transport` relay path; module
stubs return `not-implemented` phase errors with full type scaffolding. See the
plan for what each phase fills in.
