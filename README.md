# rust-backup

Modular, **streaming** source→destination backup/restore over an efficient tunnel
transport (TCP relay + direct UDP/QUIC, hole-punching, automatic fallback), vendored
from [`bore`](https://github.com/manprint/bore).

- **Streaming, zero temp files** — the source reads its backend straight into the
  channel; the destination applies the stream straight into its backend. Neither
  host writes intermediate data to disk.
- **Source is always unaltered** — the source side is strictly read-only and its
  state is fingerprinted before/after every run.
- **Modular** — `postgres`, `mongodb`, `filesystem`, `s3` today; new backends are
  drop-in crates.
- **Pure Rust** — no `pg_dump`/`mongodump`/external tools; everything is a crate
  dependency.
- **Bandwidth-aware** — the producer is consumer-paced, so a slow destination never
  forces the source to buffer.

> **Status: skeleton.** The transport relay path and the full architecture (traits,
> plan model, wire framing, session runner) are implemented and tested. The module
> backends and the direct QUIC path are typed stubs — see
> [`docs/plans/RUST_BACKUP_PLAN.md`](docs/plans/RUST_BACKUP_PLAN.md) for the phased
> roadmap.

## How it works

```
 source ──register(channel)──▶  coordination server  ◀──connect(channel)── destination
   │  analyze() → plan ─────────────── relayed/direct channel ──────────────▶ validate()
   │  stream_out()  (read-only) ═══════ chunks (BLAKE3, backpressured) ══════▶ stream_in()
```

1. The **source** connects to the coordination server, registers a channel id,
   connects to its backend with a **read-only** user, introspects it, and builds a
   self-contained **backup plan**.
2. The **destination** connects to the same channel with an **admin** user, receives
   the plan, runs **preflight** (disk space, accessibility, version compat,
   privileges), and — in async mode — prints the plan and waits for `yes`.
3. On accept, the source **streams** the payload and the destination **restores** it
   1:1. At the end both sides are aligned and the source is provably unchanged.

## Quick start

```sh
# 1. Run the coordination server (publicly reachable host)
rust-backup server --control-port 7835 --secret s3cr3t

# 2. Destination first (waits for the plan)
rust-backup postgres destination --to coord.example:7835 --channel pgjob --secret s3cr3t \
    --host db-b --user admin --password ... --yes

# 3. Source (analyzes, shows the plan, streams)
rust-backup postgres source --to coord.example:7835 --channel pgjob --secret s3cr3t \
    --host db-a --user readonly --password ...
```

See [`USAGE.md`](USAGE.md) for every subcommand, flag, environment variable, and the
YAML multi-target session format.

## Build

```sh
cargo build --release --all-features      # full (direct UDP/QUIC path on)
cargo build --release --no-default-features  # relay-only (no quinn)
bash scripts/gates.sh                      # fmt + clippy + build×2 + test
```

## Layout

| Path | What |
|------|------|
| `crates/rb-core` | traits, plan model, wire framing, channel, session runner, config, progress, errors |
| `crates/rb-transport` | coordination server + paired byte-channel (relay; direct QUIC = Phase 1) |
| `crates/rb-{postgres,mongodb,filesystem,s3}` | backup modules |
| `crates/rust-backup` | the CLI binary |
| `docs/plans/RUST_BACKUP_PLAN.md` | full design + phased implementation plan |
| `vendored-from-bore/` | bore source copied for the transport port |
| `e2e/`, `scripts/` | end-to-end harness + gates |

## License

GNU Affero General Public License v3.0 or later.
