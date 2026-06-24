# rust-backup — Implementation Plan

> Self-contained, phased plan. Every sub-phase tags the model that should execute
> it (Opus architect / Sonnet developer / Haiku explorer) and lists Files · Change
> · Unit tests · e2e tests · Done-criteria. Downstream agents execute this without
> re-exploring the codebase. The skeleton (Phase 0) is already built and green; the
> later phases are the roadmap for the new repository.

---

## ⏯ RESUME HERE — current implementation state

> Read this first to continue work. Updated **2026-06-24**.

**Done & committed:** Phase 0 (skeleton) + **Phase 1 (transport direct path), all
non-e2e sub-phases**. Working tree green on `bash scripts/gates.sh` (clippy
`-D warnings`, `--all-features` and `--no-default-features`).

**Workflow contract:** drive sub-phase by sub-phase — Sonnet implements, Opus
reviews ⟦OPUS GATE⟧ rows + runs `scripts/gates.sh`, **zero regressions**. A
sub-phase is NOT done until its acceptance test actually *ran* (`0 ignored`, not
faked) — past delegated agents hid/faked tests, so verify each one.

**Phase 1 — what landed (see §"Phase 1" below for the spec):**
- `crates/rb-transport/src/direct.rs` — QUIC (quinn 0.11/rustls 0.23) direct path:
  `DirectConn`/`DirectListener`/`connect_direct`/`bind_socket` (wildcard bind, **no
  `SO_REUSEADDR`**, `EADDRINUSE`→ephemeral), `configure_udp_socket_buffers` (Linux
  nix force+getsockopt-verify+clamp warn), BBR + 16 MiB window configs, `SkipVerify`,
  `derive_token`. `UdpDirectTuning` in `shared.rs`.
- `crates/rb-transport/src/proto.rs` — `ClientMsg` (Register/Connect/Authenticate/
  **Heartbeat**/`UdpCandidateOffer`), `ServerMsg` (Challenge/Ok/**Heartbeat**/Error/
  `UdpPunch`/`UdpUnavailable`); `Delimited<T>` null-JSON codec.
- `crates/rb-transport/src/server.rs` — `UdpMatchmaker` (per-channel oneshot
  rendezvous, order-independent `try_match`); generic `serve_control<S>` =
  control-plane loop (server heartbeat + UDP broker + **recv-deadline reaper**,
  `SECRET_CTRL_TIMEOUT`=60s passed as a PARAM so tests don't race); `serve_provider`
  calls it; `serve_consumer` selects it vs `accept_relays` (relay splice loop).
- `crates/rb-transport/src/client.rs` — `connect_source`/`connect_destination` run
  `setup_direct` on `&mut control` BEFORE control moves into the channel:
  `gather_candidates` (loopback + primary-IP, never `0.0.0.0`; best-effort RFC 5389
  STUN via `RUST_BACKUP_STUN_SERVER`) → `UdpCandidateOffer` → `recv_punch` →
  listen/dial → `set_direct`.
- `crates/rb-transport/src/channel.rs` — `PairedChannel` direct routing + per-conn
  relay fallback (`DIRECT_SETUP_TIMEOUT`=10s); `set_direct`/`is_direct`; control
  substream owned by a spawned `drive_control` task (`AbortOnDrop`) that sends
  `ClientMsg::Heartbeat` every `CTRL_CLIENT_HEARTBEAT`=20s and drains server frames.
- Tests (all ran, `0 ignored`): `tests/udp_broker.rs` (8), `tests/direct_test.rs`
  (4), `tests/direct_e2e.rs` (1, real server+2 clients, `is_direct()` both sides),
  STUN parse (2), `serve_control` reaper/keepalive (2), socket-buffer (1).

**Not done in Phase 1:** **1.5 netns e2e** (`e2e/transport_netns_test.sh`) — blocked
in this environment (no sudo/netns/Docker); must run on a host with privileges.

**Known edge (logged, non-blocking):** if a peer stops *reading*, `send_server` can
block `serve_control`'s select so the reaper can't fire. Recv-deadline reaper is the
spec'd mechanism; the send-block case is deferred (revisit in Phase 8 hardening).

**Phase 2 (PostgreSQL) — in progress.** See the per-sub-phase status block under
§"Phase 2" below. Done so far: **2.1** (connect+version), **2.2** (read-only
introspection → `PgPlanPayload`), **2.3** (DDL reconstruction, golden-tested).

**⚠ Live introspection (2.2) is UNVERIFIED in this env** (no Docker/live pg). The
SQL is written to be version-robust but must be checked by running
`bash e2e/postgres_introspect.sh [PG_MAJOR]` on a Docker host before trusting it.
(2.3 DDL is pure → fully unit-verified here via goldens.)

**▶ NEXT: Phase 2.4 — `source.rs` `stream_out`**: per data-bearing item (from
`build_plan`) run `COPY (SELECT cols) TO STDOUT (FORMAT binary)` via
tokio-postgres `copy_out`, chunk into the `ChunkSink` (≤1 MiB, no temp). The
item `meta` already carries `{database, schema, table, columns}` and the COPY
column list (generated columns omitted). Make the chunking unit-testable with an
in-memory sink + a synthetic byte stream (decouple from a live DB).
Module crates depend only on `rb-core`; never edit core to add a backend
(I-MODULAR).

**Build/test:** `cargo build --all-features` · `cargo build --no-default-features`
(relay-only, no quinn) · `cargo test --all-features` · `bash scripts/gates.sh` (full).

---

## 0. Scope & reference scenario

**Goal.** A modular Rust application, `rust-backup`, that performs
**source→destination** backup/restore by **streaming** data over the `bore`
tunnel transport (TCP relay + direct UDP/QUIC, hole-punching, carriers, with
automatic direct↔relay fallback). A coordination server pairs the two sides on a
named channel. Neither side writes temp data to disk — everything is streamed.
First modules: **postgres**, **mongodb**, **filesystem**, **s3**. First mode:
**1:1 copy** (`Copy1to1`), designed to extend to further modes.

**Reference acceptance scenario (postgres, the canonical case).**

1. `rust-backup postgres source --to coord:7835 --channel pgjob --host db-a --user ro …`
   connects to source DB with a **read-only** user, introspects the cluster
   (databases, roles, grants, schemas, tables, sequences, extensions, ownership),
   builds a complete **backup plan**, and shows it to the operator.
2. `rust-backup postgres destination --to coord:7835 --channel pgjob --host db-b --user admin …`
   connects with an **admin** user, receives the plan, runs preflight (disk space,
   accessibility, version ≥ source major, privileges), and (async mode) prints the
   plan and waits for `yes`.
3. On accept, the source **streams** the cluster (DDL then `COPY … TO STDOUT
   BINARY` per table, ordered) and the destination **applies** it 1:1 (roles → db
   → schema → tables → data → constraints/indexes → sequences → grants).
4. **End state:** destination is a 1:1 copy of source; `db-a` is **byte/logically
   unchanged** (fingerprint identical before/after).

**Observable invariants under test:**
- **I-IMMUT**: source fingerprint identical before and after every run, including
  runs aborted mid-stream. Hardest-tested invariant.
- **I-NOTEMP**: no temp file on either host (source reads → channel → dest applies).
- **I-ERRORS**: every phase returns an explicit phase-tagged error; no `unwrap` in
  production paths.
- **I-MODULAR**: a new backend is a new crate implementing `BackupModule`; zero core
  change.
- **I-BANDWIDTH**: producer is consumer-paced (backpressure); a slow destination
  never forces the source to buffer or stall its backend unsafely.
- **I-OBSERV**: both sides emit clear progress (phase, bytes, rate, items).

**Repo / gates.** Workspace at `rust-backup/`. Gate command:
`bash scripts/gates.sh` ⇒ `cargo fmt --all --check` · `cargo clippy --all-targets
--all-features -- -D warnings` · `cargo build --all-features` · `cargo build
--no-default-features` · `cargo test --all-features`. Plus netns e2e in `e2e/`.

---

## 1. Approved decisions (D1–D14)

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | **Cargo workspace, multi-crate** (`rb-core`, `rb-transport`, `rb-{postgres,mongodb,filesystem,s3}`, `rust-backup` bin). | New backend = new crate; dependency isolation; the heavy driver trees don't bloat the core. |
| **D2** | **Source = provider (registers channel-id, serves bytes); Destination = consumer (connects, drives restore).** Mirrors bore secret-tunnel exactly. | Reuse bore `serve_provider`/`serve_consumer`/`relay` unchanged in spirit; data flows source→dest over a consumer-opened substream. |
| **D3** | **Backpressure = consumer-paced substream flow control** (yamux window / QUIC stream window) + idle-timeout I/O. | Solves I-BANDWIDTH + I-NOTEMP + I-IMMUT together: a slow dest stalls the source's writes → stalls its backend reads; source never buffers locally. |
| **D4** | **Plan is a self-contained artifact** carried `BackupMode`-tagged with an opaque module `payload`. | Destination validates + restores from the plan alone; no extra round-trip to source. |
| **D5** | **Pure-Rust drivers, no external binaries** (no `pg_dump`/`mongodump`). postgres `tokio-postgres`; mongodb `mongodb`; s3 `aws-sdk-s3`; filesystem `std`+`nix`. | Logical introspection + streaming COPY/cursor/GET. Heavier code but no runtime deps. |
| **D6** | **Postgres = cluster fidelity** (roles/grants/extensions/schema/data/sequences/ownership/tablespaces). | Largest module; DDL reconstruction in Rust from catalogs. |
| **D7** | **Full bore transport vendored.** Relay path implemented + tested now; **direct UDP/QUIC path is a Phase-1 port** of `holepunch.rs`. | Skeleton ships a working relay channel; direct is documented + scaffolded, falls back to relay. |
| **D8** | **Skeleton = typed stubs that compile.** Modules return `not-implemented` phase errors; full type scaffolding (params, plan payloads, trait impls) present. | This iteration delivers a compiling, tested skeleton; logic lands per later phase in the new repo. |
| **D9** | **Single substream per channel in the skeleton session** (carriers==1, byte-correct). Multi-carrier parallel item distribution = Phase 6. | Avoids yamux ordering ambiguity now; preserves a clean upgrade path. |
| **D10** | **Config precedence CLI > env > YAML.** Every flag has a `RUST_BACKUP_*` env; YAML supports a multi-target `targets:` list. | One session can run multiple targets. |
| **D11** | **Async-accept** is dest-side: preflight must pass AND operator (or `--yes`) approves; rejection aborts cleanly with source untouched. | I-IMMUT holds on rejection. |
| **D12** | **Integrity = per-chunk BLAKE3** (+ optional per-item digest), streamed (no whole-file buffering). | Ported from bore `transfer.rs`. |
| **D13** | **Filesystem ownership/permissions preserved on Linux**; arbitrary uid/gid restore needs root/`CAP_CHOWN` — documented, and preflight warns when lacking. | Sudo case is explicit, not silent. |
| **D14** | **`#![forbid(unsafe_code)]`** in every crate. | Matches bore's safety posture. |

---

## 2. Target architecture

### 2.1 Crate graph

```
rust-backup (bin: CLI, config merge, module registry, session runner glue)
│
├── rb-core        (no deps on siblings) — traits, plan, wire, channel, session, config, progress, error
├── rb-transport   → rb-core            — coord server, PairedChannel (relay now, direct Phase 1)
├── rb-postgres    → rb-core
├── rb-mongodb     → rb-core
├── rb-filesystem  → rb-core
└── rb-s3          → rb-core
```

`rb-core` is the only crate modules see. The transport is wired into `rb-core`'s
`DataChannel` trait by the binary, so modules are transport-agnostic and unit-test
against an in-memory channel.

### 2.2 Data path

```
 SOURCE host                         COORD server                     DEST host
 ┌─────────────┐   register(ch)   ┌──────────────┐   connect(ch)   ┌──────────────┐
 │ Source impl │ ───────────────▶ │  registry     │ ◀────────────── │ Destination  │
 │ analyze()   │                  │  ch → pool    │                 │ validate()   │
 │ stream_out()│ ◀═ accept_stream │  relay  ═════▶│  open_stream ═▶ │ stream_in()  │
 └─────────────┘   (provider)     └──────────────┘   (consumer)     └──────────────┘
        ▲  control+data on ONE substream (skeleton): Plan→PlanAck→[chunks]→StreamEnd→Done
        │  Phase 1: per-connection direct QUIC path (hole-punched), relay stays warm fallback
```

### 2.3 Reuse map (bore → rust-backup)

| What | bore source (anchor) | rust-backup target | Notes |
|------|----------------------|--------------------|-------|
| yamux mux | `mux.rs` `client/server`, `Opener::open`, `Acceptor::accept`, `STREAM_READY=0`, `write/read_stream_ready` | `rb-transport/src/mux.rs` | copy ~verbatim; one stream = one task |
| prefix replay | `prefixed.rs` `Prefixed::new` | `rb-transport/src/prefixed.rs` | verbatim |
| carrier pool | `pool.rs` `CarrierPool::{new,push,pick}` | `rb-transport/src/pool.rs` | verbatim |
| TCP/TLS control | `transport.rs` `Endpoint::parse`, `connect`, `client_config`, `server_tls_from_pem` | `rb-transport/src/transport.rs` | trim to control channel |
| HMAC auth | `auth.rs` `Authenticator::{new,server_handshake,client_handshake}` | `rb-transport/src/auth.rs` | retarget to new proto |
| framed codec | `shared.rs` `Delimited` (null-JSON, `MAX_FRAME_LENGTH`), `tune_tcp`, `NETWORK_TIMEOUT` | `rb-transport/src/proto.rs` + `rb-core/src/wire.rs` | new minimal `Msg` enums |
| provider/consumer relay | `secret.rs` `serve_provider`/`serve_consumer`/`relay` (registry `DashMap<String,Arc<CarrierPool>>`, `broker_udp`), control heartbeat + recv-deadline reaper (`CTRL_CLIENT_HEARTBEAT` 20 s / `SECRET_CTRL_TIMEOUT` 60 s) | `rb-transport/src/server.rs` | simplify (no vhost/vpn/admin); keep heartbeat/reaper (Phase 1.6) |
| control accept loop | `server.rs` accept loop + TLS + dispatch + `--max-conns` semaphore | `rb-transport/src/server.rs` | simplify |
| reconnect/backoff | `reconnect.rs` `Backoff`, `run(auto_reconnect, connect, serve)` | `rb-transport/src/reconnect.rs` | verbatim |
| direct QUIC | `holepunch.rs` `DirectConn`/`DirectListener`/`connect_direct`/`punch`/`server_endpoint`/`transport_config`/`configure_udp_socket_buffers`/`make_socket`/`bind_socket`, `DatagramSend` | `rb-transport/src/direct.rs` (**Phase 1**) | the big port; relay fallback per-connection; **punch socket never `SO_REUSEADDR`** (`EADDRINUSE`→ephemeral, BUG-S3) |
| chunk streaming + backpressure + BLAKE3 | `transfer.rs` `send_frame`/`recv_frame`, `write_all_idle`/`read_exact_idle`, `ChunkStart`, `ProgressShared`, `CHUNK_SIZE`/`COPY_BUFFER` | `rb-core/src/{wire,channel,progress}.rs` | **already ported** in Phase 0 |
| netns e2e harness | `scripts/vhost_netns_test.sh` (ns create, spawn, pass/fail counters, `trap cleanup`) | `e2e/*_netns_test.sh` | pattern reuse |
| gate script | `test_gates.sh` | `scripts/gates.sh` | adapted |

---

## 3. Interface

### 3.1 CLI

```
rust-backup <module> <role> [PARAMS]      module ∈ postgres|mongodb|filesystem|s3 ; role ∈ source|destination
rust-backup server [SERVER OPTS]          run the coordination server
rust-backup run --config session.yml      multi-target session from YAML
rust-backup plan <module> source [PARAMS]  analyze + print plan only (dry-run, no transfer)
```

**Transport params (all roles):** `--to <host:port>` `--channel <id>` `--secret <s>`
`--carriers <n>` `--udp/--no-udp` `--insecure`. **Dest extra:** `--yes` (auto-accept).
**Global:** `--config <file.yml>` `-v/--verbose`.

**Module params (examples):**
- postgres: `--host --port --user --password --database --sslmode` (+ dest `--admin`).
- mongodb: `--uri | (--host --port --user --password)` `--database --auth-db`.
- filesystem: `--root --follow-symlinks --preserve-ownership --preserve-xattr`.
- s3: `--endpoint --region --bucket --prefix --access-key --secret-key --path-style`.

### 3.2 Environment

Every flag mirrors to `RUST_BACKUP_<UPPER_SNAKE>` via clap `env`. Precedence
CLI > env > YAML (D10).

### 3.3 YAML (multi-target)

```yaml
server: { bind_addr: 0.0.0.0, control_port: 7835, secret: s3cr3t, udp: true }
targets:
  - { module: postgres,   role: source,      transport: {to: coord:7835, channel: pg1, secret: s3cr3t}, params: {host: db-a, user: ro} }
  - { module: filesystem, role: destination, transport: {to: coord:7835, channel: fs1}, params: {root: /restore}, auto_accept: true }
```

---

## 4. Wire protocol & plan model (Phase 0, implemented)

- **Transport control proto** (`rb-transport/src/proto.rs`, null-JSON `Delimited`):
  client→server `Register{channel}` | `Connect{channel}` | `Authenticate(tag)` |
  `Heartbeat`; server→client `Challenge(uuid)` | `Ok` | `Error(msg)` | `Heartbeat`.
  The provider sends `Heartbeat` every `CTRL_CLIENT_HEARTBEAT` (20 s); the server
  reaps a registry entry idle past `SECRET_CTRL_TIMEOUT` (60 s) — a yamux substream
  hides a half-open peer, so liveness is an app-level recv-deadline (Phase 1.6).
  Phase 1 adds UDP punch variants (`UdpCandidateOffer`/`UdpPunch`/`UdpUnavailable`).
- **App channel framing** (`rb-core/src/wire.rs`, length-prefixed JSON, raw payload
  after `ChunkStart`):
  `ControlFrame::{Plan, PlanAck{accepted,reason}, Done{total_bytes,blake3}, Abort{reason}}`
  and `DataFrame::{ChunkStart{item_id,offset,len,blake3}, ItemEnd{item_id,total,blake3}, StreamEnd}`.
- **Plan** (`rb-core/src/plan.rs`): `BackupPlan{format_version, module, mode, created_at,
  source_summary, items[PlanItem], estimated_bytes, integrity, payload(Value)}`.
- **Handshake** (`rb-core/src/session.rs`): provider accepts a substream → `Plan` →
  consumer `PlanAck` → chunks → `StreamEnd` → `Done`. Source audits fingerprint
  before/after (I-IMMUT).

---

## 5. Phases

> Each sub-phase: **Model · Files · Change · Unit · e2e · Done**. Opus-review gates
> are marked **⟦OPUS GATE⟧**.

### Phase 0 — Skeleton  *(this iteration — built & green)*

| Sub | Model | Status |
|-----|-------|--------|
| 0.1 workspace + `rb-core` (error/plan/wire/channel/module/config/progress/session) | **Opus** | ✅ done, 7 tests |
| 0.2 `rb-transport` relay (coord server + `PairedChannel` + vendored mux/pool/transport/auth/reconnect) | **Sonnet** | relay path + in-process test |
| 0.3 module stubs ×4 (params + plan payload + trait impls + registration) | **Sonnet** | compile + stub tests |
| 0.4 `rust-backup` bin (CLI, config merge, registry, session glue) | **Opus/Sonnet** | wires it all |
| 0.5 docs (CLAUDE.md, README, USAGE, this plan, module docs) + gates + e2e scaffold | **Haiku** | deliverable |

**Done-criteria (Phase 0):** `bash scripts/gates.sh` green; `--all-features` and
`--no-default-features` both build; `cargo test` green; CLI prints help for every
subcommand; relay channel moves bytes in an in-process e2e.

---

### Phase 1 — Transport direct path (QUIC/hole-punch port)

> **Status** *(2026-06-24)* — non-e2e items closed; gates green.
> 1.1 ✅ done (3 tests) · 1.2 ✅ done (8 tests, `UdpMatchmaker`) ·
> 1.3 ✅ done (`direct_test` 4 + `direct_e2e` 1 + STUN parse 2) ·
> 1.4 ✅ done (`test_socket_buffers_enlarged`, Linux getsockopt) ·
> 1.5 ⏸ script-only here (no sudo/netns in this env) ·
> 1.6 ✅ done (`serve_control` reaper + client `drive_control` heartbeat;
> `serve_control_reaps_silent_peer` / `serve_control_keeps_heartbeating_peer`).

- **1.1 ⟦OPUS GATE⟧** *(Sonnet impl)* — `rb-transport/src/direct.rs`. Port
  `holepunch.rs` `DirectConn`/`DirectListener`/`connect_direct`/`punch`/`client_endpoint`/
  `server_endpoint`/`server_config`/`transport_config`/`DatagramSend`,
  `configure_udp_socket_buffers`, and `make_socket`/`bind_socket` — the punch socket
  **never sets `SO_REUSEADDR`**; bind the fixed/preferred port, fall back to an
  ephemeral port on `EADDRINUSE` (co-bound REUSEADDR sockets steal each other's
  datagrams — the concurrent-tunnel ~30 s flap, BUG-S3). **Unit:** QUIC endpoint
  pair handshake on loopback; `transport_config` BBR + 16 MiB windows asserted;
  second co-bind of the same fixed port is refused → ephemeral fallback. **e2e:**
  none yet. **Done:** `--features udp` compiles, two endpoints exchange a bidi
  stream on loopback.
- **1.2 ⟦OPUS GATE⟧** *(Sonnet)* — `proto.rs` + `server.rs`: add `UdpCandidateOffer`/
  `UdpPunch`/`UdpUnavailable`; server brokers candidates to both paired peers (10 s
  timeout → `UdpUnavailable`). **Unit:** broker pairs two offers, times out alone.
  **Done:** broker logic table-tested.
- **1.3 ⟦OPUS GATE⟧** *(Sonnet)* — `channel.rs`: direct `PairedChannel` variant —
  consumer dials direct, provider listens; **per-connection fallback** to the warm
  relay in place (DEC-LU4-style); STUN reflexive discovery; never gate liveness on UDP.
  **Unit:** fallback returns relay stream when direct fails. **Done:** direct used when
  available, relay otherwise, transparently.
- **1.4** *(Haiku)* — socket buffer force + tuning consts; `warn!` on clamp survival
  (port `configure_udp_socket_buffers`); `warn!`-log the `EADDRINUSE`→ephemeral
  fixed-port fallback (BUG-S3). **Done:** getsockopt-verified, warns.
- **1.5** *(Sonnet)* — `e2e/transport_netns_test.sh` (T-NET1): netns with a NAT/relay
  topology; assert direct path is taken when reachable and relay when blocked; byte
  transfer succeeds both ways. **Done:** netns N pass / 0 fail.
- **1.6 ⟦OPUS GATE⟧** *(Sonnet)* — control heartbeat + recv-deadline reaper
  (`server.rs`, channel control loop). Provider sends `Heartbeat` every
  `CTRL_CLIENT_HEARTBEAT` (20 s); server reaps a registry entry whose control
  substream is silent past `SECRET_CTRL_TIMEOUT` (60 s) — a half-open peer is
  invisible to a yamux `recv`, so liveness is an app-level deadline. Timeout is a
  test hook (override the const). **Unit:** silent provider reaped after the
  deadline; heartbeating provider survives. **Done:** no zombie channel ids.

---

### Phase 2 — PostgreSQL module (cluster fidelity, D6)

> `tokio-postgres = "0.7"`. Supports pg 10..=latest via protocol v3. Logical
> approach: catalog introspection + `COPY … (FORMAT binary)` streaming.

> **Status** *(2026-06-24)* — gates green; DB-backed e2e is script-only here
> (no Docker/live pg in this env), so each sub-phase ships real unit tests now +
> a live test gated on `RUST_BACKUP_PG_HOST` (skips cleanly when unset).
> 2.1 ✅ done — `connect.rs`: `PgConnection::connect` (NoTls; sslmode
> disable/allow/prefer OK, verifying modes rejected w/ clear error → TLS
> follow-up), `parse_major`, `MIN_PG_MAJOR`=10, reject < 10. Tests: 5 unit
> (`parse_major` ×3, sslmode gate ×2) + `tests/connect_live.rs` (gated).
> 2.2 ✅ done (live-UNVERIFIED here) — `model.rs` (`PgPlanPayload` cluster
> model: roles/memberships/tablespaces/databases→schemas→tables(cols/cons/idx)/
> sequences/views/functions; passwords NOT captured — read-only) +
> `introspect.rs` (read-only `pg_catalog` SQL, casts to text/int8/bool, leans on
> `pg_get_*def`; version-branches `attgenerated` 12+/`prokind` 11+; excludes
> extension-owned objects; `build_plan` = one stream item per data-bearing table,
> skips child partitions, omits generated cols from COPY list; `now_rfc3339`).
> Tests: serde roundtrip ×2, `build_plan` ×2, rfc3339 vectors, all run;
> `tests/introspect_live.rs` + `e2e/postgres_introspect.sh` (gated/Docker).
> 2.3 ✅ done — `ddl.rs` (pure, golden-tested): `build_cluster_ddl(payload)` →
> `ClusterDdl{roles, databases, per_database:[DatabaseDdl{pre_data, post_data}]}`.
> Emitters for roles/memberships/tablespaces/databases (+GUCs), extensions,
> schemas, tables (columns only — UNLOGGED, identity/generated/defaults/collation,
> PARTITION BY/OF, reloptions, tablespace), constraints (non-FK before FK),
> indexes (skips constraint-backed), sequences (+OWNED BY/setval), views,
> functions (verbatim from `pg_get_functiondef`), comments, owners, ACL→GRANT
> (priv-letter map + WITH GRANT OPTION). Tables created bare; constraints/indexes
> deferred to post_data so COPY loads fast. 9 goldens (hand-written expected SQL,
> not echoes) all run. ACL/GUC rendering best-effort (verified by e2e apply, 2.6).
> · 2.4–2.8 ⏳ pending.
>
> **TLS follow-up (deferred):** wire a rustls connector (`tokio-postgres-rustls`)
> so `require`/`verify-ca`/`verify-full` work; today they error out.

- **2.1** *(Sonnet)* — `rb-postgres/Cargo.toml` add `tokio-postgres`, `postgres-protocol`;
  `connect.rs` read-only connect (source) / admin connect (dest); server version probe
  → reject < 10. **Unit:** version parse 10..18. **Done:** connects, reports version.
- **2.2 ⟦OPUS GATE⟧** *(Sonnet)* — `introspect.rs`: query `pg_roles`, `pg_auth_members`,
  `pg_database`, `pg_namespace`, `pg_class`, `pg_attribute`, `pg_constraint`, `pg_index`,
  `pg_sequence`, `pg_proc`, `pg_extension`, `pg_tablespace`, ACL (`aclitem`) → fill
  `PgPlanPayload`. Read-only. **Unit:** payload serde roundtrip on a fixture catalog
  dump. **e2e:** introspect a live pg in docker, snapshot the plan. **Done:** plan is
  complete + human-renderable; **source untouched (I-IMMUT fingerprint check)**.
- **2.3 ⟦OPUS GATE⟧** *(Sonnet)* — `ddl.rs`: reconstruct DDL from the payload — roles
  (+ membership), databases (owner/encoding/locale), schemas, tables (columns/types/
  defaults/not-null), constraints (PK/FK/unique/check), indexes, sequences, grants,
  ownership, extensions. Ordering map for restore. **Unit:** golden DDL for a fixture
  schema. **Done:** emitted SQL applies cleanly to an empty cluster.
- **2.4** *(Sonnet)* — `source.rs` `stream_out`: per table `COPY (SELECT *) TO STDOUT
  (FORMAT binary)` via `copy_out`, chunked into `ChunkSink`; ordered (data after DDL);
  sequence values captured. Read-only; no temp. **Unit:** copy_out stream chunks a
  fixture table. **Done:** all tables streamed, sizes match plan estimates ±.
- **2.5** *(Sonnet)* — `dest validate`: disk-space estimate vs free; version ≥ source
  major; admin privileges (`pg_has_role`, `CREATEROLE`/`CREATEDB`); target databases
  absent or `--overwrite`. **Unit:** preflight table. **Done:** clear pass/fail checks.
- **2.6 ⟦OPUS GATE⟧** *(Sonnet)* — `dest stream_in`: apply roles → create db → schema →
  tables → `COPY … FROM STDIN (FORMAT binary)` via `copy_in` → constraints/indexes →
  set sequences → grants → ownership. Transactional per database where possible. **Unit:**
  apply order test. **e2e:** restore the 2.4 stream into an empty cluster; row counts +
  checksums match. **Done:** dest is 1:1 with source.
- **2.7 ⟦OPUS GATE⟧** *(Sonnet)* — `immutability.rs`: source fingerprint = hash of
  (catalog snapshot + per-table `COUNT`+sample checksum). Assert identical before/after,
  including a run aborted mid-`COPY`. Enforce read-only user (reject if user can write).
  **Unit:** fingerprint stable. **e2e:** T-PG-IMMUT — kill transfer at 50%, source
  fingerprint unchanged. **Done:** I-IMMUT proven for pg.
- **2.8** *(Sonnet)* — `e2e/postgres_matrix.sh` (T-PG-MATRIX): docker pg 10/12/14/16/18,
  seed → backup → restore → diff. **Done:** matrix green.

---

### Phase 3 — MongoDB module (4..=8)

> `mongodb = "3"` (pure Rust). Mirrors Phase 2 shape.

- **3.1** *(Sonnet)* — connect (uri/host); server version probe → reject < 4. **Done:** connects.
- **3.2 ⟦OPUS GATE⟧** *(Sonnet)* — `introspect.rs`: list databases, collections (+options),
  indexes, users/roles (admin db) → `MongoPlanPayload`. **Done:** complete plan; I-IMMUT.
- **3.3** *(Sonnet)* — `source stream_out`: per collection cursor `find({})` → BSON docs
  batched into `ChunkSink` (raw BSON). **Done:** all collections streamed.
- **3.4** *(Sonnet)* — `dest validate`: version ≥ source major series, disk, privileges,
  target empty/allowed. **Done:** checks.
- **3.5 ⟦OPUS GATE⟧** *(Sonnet)* — `dest stream_in`: create collections + options →
  `insert_many` batched (ordered=false, capped batch) → build indexes → users/roles.
  **e2e:** restore matches source counts. **Done:** 1:1.
- **3.6 ⟦OPUS GATE⟧** *(Sonnet)* — immutability harness (listing + per-collection count
  + sample _id checksum) before/after + mid-abort. **Done:** I-IMMUT for mongo.
- **3.7** *(Sonnet)* — `e2e/mongodb_matrix.sh`: docker mongo 4/5/6/7/8. **Done:** green.

---

### Phase 4 — Filesystem module (Linux ownership/permissions, D13)

> `std` + `nix` (`fs`, `user`). No external deps.

- **4.1** *(Sonnet)* — `walk.rs`: recursive walk; per entry capture kind (file/dir/
  symlink/hardlink), size, mode, uid, gid, mtime, symlink target, xattr (optional) →
  `FsPlanPayload`. Read-only. **Unit:** walk a tempdir fixture. **Done:** plan complete.
- **4.2** *(Sonnet)* — `source stream_out`: per file stream bytes into `ChunkSink`
  (64 KiB reads, no whole-file buffer); dirs/symlinks are metadata-only items. **Done:** streamed.
- **4.3 ⟦OPUS GATE⟧** *(Sonnet)* — `dest stream_in`: recreate tree; write file bytes;
  restore mode/mtime always; uid/gid when privileged (`chown`); **preflight warns +
  records when not root/`CAP_CHOWN`** (D13). Symlinks/hardlinks recreated. **Unit:**
  metadata roundtrip (non-root: mode preserved, ownership best-effort). **e2e:** root
  netns case restores uid/gid exactly; non-root case preserves mode + warns. **Done:**
  1:1 with documented sudo behavior.
- **4.4 ⟦OPUS GATE⟧** *(Sonnet)* — immutability: source tree hash (path+meta+content)
  before/after; assert no atime/mtime write to source (open read-only, `O_NOATIME`
  where available). **e2e:** T-FS-IMMUT mid-abort. **Done:** I-IMMUT for fs.
- **4.5** *(Haiku)* — `docs/modules/FILESYSTEM.md`: sudo/`CAP_CHOWN` matrix, xattr,
  symlink/hardlink semantics. **Done:** documented.

---

### Phase 5 — S3 module (AWS S3 + MinIO)

> `aws-sdk-s3` + `aws-config`. Streaming GET→PUT (multipart), no temp.

- **5.1** *(Sonnet)* — connect/config (endpoint override for MinIO, path-style, creds);
  list buckets/objects + metadata + (optional) policy/ACL → `S3PlanPayload`. **Done:** plan.
- **5.2** *(Sonnet)* — `source stream_out`: per object `GetObject` body byte-stream →
  `ChunkSink`. **Done:** streamed.
- **5.3** *(Sonnet)* — `dest validate`: bucket exists/creatable, region, creds, target
  empty/allowed. **Done:** checks.
- **5.4 ⟦OPUS GATE⟧** *(Sonnet)* — `dest stream_in`: create bucket; `PutObject` /
  multipart upload streamed from `ChunkSource`; preserve content-type/metadata/storage
  class; ACL/policy. **e2e:** MinIO container 1:1. **Done:** 1:1.
- **5.5** *(Sonnet)* — immutability: source object listing + per-object ETag before/after
  + mid-abort. **Done:** I-IMMUT for s3.
- **5.6** *(Sonnet)* — `e2e/s3_minio_test.sh`. **Done:** green.

---

### Phase 6 — Multi-carrier parallelism + bandwidth adaptation ⟦OPUS GATE⟧

- **6.1** *(Sonnet)* — `session.rs`: negotiate N data substreams (`carriers`); distribute
  items across carriers (one item = one carrier, never intra-item striping — reorder
  trap, mirror bore's flow-pinning note). Control stays on stream 0. **Unit:** N-carrier
  distribution; carriers==1 byte-identical to Phase 0. **Done:** parallel + safe.
- **6.2** *(Sonnet)* — explicit pacing option `--max-rate` (token bucket) on top of
  natural backpressure, for shared links. **Unit:** rate limiter. **Done:** capped.
- **6.3** *(Sonnet)* — `e2e/bandwidth_netem.sh` (T-BW): netns + `tc netem` asymmetric
  bandwidth/RTT; assert source never buffers > window, transfer completes, no loss-induced
  corruption (BLAKE3). **Done:** I-BANDWIDTH proven under a real gap.

---

### Phase 7 — Async-accept UX + multi-target session

- **7.1** *(Sonnet)* — `rust-backup/src/cli.rs`: full clap tree (all subcommands, env
  on every flag), `--config` merge (CLI>env>YAML). **Unit:** precedence table. **Done:** parses all.
- **7.2** *(Sonnet)* — interactive accept: dest prints plan + preflight, reads `yes/no`
  from stdin (or `--yes` / `auto_accept`). **Unit:** accept-policy closures. **Done:** prompt works.
- **7.3** *(Sonnet)* — `session_runner.rs`: run a `targets[]` list — sequential by default,
  bounded-concurrent with `--parallel-targets`; per-target tracing span + progress; one
  failed target doesn't abort siblings unless `--fail-fast`. **e2e:** 2-target YAML session.
  **Done:** multi-target session runs.

---

### Phase 8 — Hardening + e2e + docs ⟦OPUS GATE⟧

- **8.1 ⟦OPUS GATE⟧** *(Sonnet)* — **source-immutability fuzz/error-injection suite**
  across all modules: inject failures (connection drop, dest disk-full, kill at random %,
  channel reset) and assert (a) source fingerprint unchanged, (b) dest left with no
  partial/committed state or a clearly-aborted state, (c) explicit phase-tagged error.
  The flagship test bank. **Done:** I-IMMUT + I-ERRORS proven under faults.
- **8.2** *(Sonnet)* — explicit-error audit: `#![deny(clippy::unwrap_used,
  clippy::expect_used)]` in production code; every `?` site has a phase-tagged map.
  **Done:** lint clean.
- **8.3** *(Sonnet)* — `e2e/full_matrix.sh`: orchestrate all module netns/docker e2e +
  direct/relay variants; pass/fail counters (bore pattern). **Done:** full matrix green.
- **8.4** *(Haiku)* — finalize `README.md`, `USAGE.md`, per-module docs, `CHANGELOG.md`.
  **Done:** docs complete.
- **8.5** *(Haiku)* — `.github/workflows/ci.yml` (fmt/clippy/build×2/test + docker module
  jobs + audit). **Done:** CI green.

---

## 6. Test strategy

- **Unit** (`#[cfg(test)]` + `tests/*.rs` per crate): wire/plan/session (T-CORE*, T-SESS*),
  transport relay (T-TRANS*), per-module params/payload/introspect/ddl golden tests.
- **Integration** (in-process, bore `e2e_test.rs` pattern): server+source+dest in one
  process over the loopback relay; serialize with a `lazy_static` mutex if a fixed port
  is used.
- **e2e netns + docker** (`e2e/`, bore `vhost_netns_test.sh` pattern: ns create, spawn,
  curl/psql/mongosh assert, `trap cleanup`, `PASS/FAIL` counters, exit 1 on fail):
  T-NET1, T-PG-MATRIX, T-PG-IMMUT, T-FS-IMMUT, T-BW, T-S3, full_matrix.
- **Invariant tests are first-class**: I-IMMUT has a dedicated mid-abort test per module
  (Phase 2.7/3.6/4.4/5.5/8.1).
- **Gate:** `bash scripts/gates.sh` (fmt/clippy-Dwarnings/build×2/test). Zero regressions.

---

## 7. Model-assignment summary

| Phase / area | Primary model | Opus gates |
|--------------|---------------|------------|
| 0.1 rb-core architecture | **Opus** | self |
| 0.2 transport relay, 0.3 module stubs | **Sonnet** | — |
| 0.4 bin glue | **Opus**→Sonnet | session wiring |
| 0.5 docs | **Haiku** | Opus final read |
| 1.x direct QUIC path | **Sonnet** | 1.1, 1.2, 1.3, 1.6 (concurrency/hot-path, heartbeat/reaper) |
| 2.x postgres | **Sonnet** | 2.2, 2.3, 2.6, 2.7 (plan/DDL/restore/immutability) |
| 3.x mongodb | **Sonnet** | 3.2, 3.5, 3.6 |
| 4.x filesystem | **Sonnet** | 4.3, 4.4 (ownership/immutability) |
| 5.x s3 | **Sonnet** | 5.4 |
| 6.x carriers/bandwidth | **Sonnet** | phase (concurrency) |
| 7.x CLI/session | **Sonnet** | — |
| 8.x hardening/e2e/docs | **Sonnet**+**Haiku** | 8.1 (immutability fuzz) |

**Rule:** start Sonnet; drop mechanical/bulk + docs to Haiku; escalate to Opus only
for the gated rows (architecture, plan/DDL correctness, concurrency/hot-path, the
immutability proofs).

## 8. bore reuse anchors (quick reference for implementers)

```
mux:        bore src/mux.rs       client/server, Opener::open, Acceptor::accept, STREAM_READY, write/read_stream_ready
pool:       bore src/pool.rs      CarrierPool::{new,push,pick}, Carrier::new
transport:  bore src/transport.rs Endpoint::parse, connect, client_config, server_tls_from_pem
auth:       bore src/auth.rs      Authenticator::{new,server_handshake,client_handshake}; Hello-before-auth ordering
framed:     bore src/shared.rs    Delimited (null-JSON), tune_tcp, CONTROL_PORT=7835, NETWORK_TIMEOUT=3s, MAX_FRAME_LENGTH
relay:      bore src/secret.rs    serve_provider/serve_consumer/relay, registry DashMap<String,Arc<CarrierPool>>, broker_udp, heartbeat/reaper CTRL_CLIENT_HEARTBEAT=20s/SECRET_CTRL_TIMEOUT=60s
server:     bore src/server.rs    accept loop + TLS + dispatch + max-conns semaphore
direct:     bore src/holepunch.rs DirectConn/DirectListener/connect_direct/punch/server_endpoint/transport_config/configure_udp_socket_buffers/make_socket/bind_socket (NO SO_REUSEADDR; EADDRINUSE->ephemeral), DatagramSend{Sent,TooLarge}
stream:     bore src/transfer.rs  send/recv_frame, write_all_idle/read_exact_idle, ChunkStart, ProgressShared, CHUNK_SIZE=1MiB, COPY_BUFFER=64KiB
reconnect:  bore src/reconnect.rs Backoff, run(auto_reconnect, connect, serve)
e2e:        bore scripts/vhost_netns_test.sh  ns create/spawn/assert, trap cleanup, PASS/FAIL counters
```
All of the above are copied verbatim into `rust-backup/vendored-from-bore/` for the port.
```
