# rust-backup — Usage

Three ways to configure, highest precedence first: **CLI flags > environment
variables > YAML config**. Every CLI flag has a `RUST_BACKUP_<UPPER_SNAKE>` env
var — `--auth-db` reads `RUST_BACKUP_AUTH_DB`, and so on. The only exceptions
are `-v/--verbose`, the repeatable `-P/--param`, and `--no-udp` (the negation of
`--udp`, which reads `RUST_BACKUP_UDP`).

> **Credentials belong in the environment, not in argv.** On Linux
> `/proc/<pid>/cmdline` is world-readable while `/proc/<pid>/environ` is
> readable only by the owning account, so `--password`, `--secret-key` and a
> `--uri` carrying credentials are visible to every local user for as long as
> the transfer runs. Pass them as `RUST_BACKUP_PASSWORD`,
> `RUST_BACKUP_SECRET_KEY` and `RUST_BACKUP_URI`; the transport secret also has
> `--secret-file`.

## Subcommands

```
rust-backup <module> <role> [PARAMS]   module = postgres|mongodb|filesystem|s3 ; role = source|destination
rust-backup server [SERVER OPTS]        run the coordination server
rust-backup run --config session.yml    run a multi-target session from YAML
rust-backup plan <module> source [PARAMS]  dry-run: analyze + print the plan, no transfer
```

Every surface accepts `--help`; the top-level also accepts `--version` and
`--verbose` (repeatable). Module roles accept `--param` / `-P key=value` for a
module-specific setting that has no dedicated flag. A `-P` value is typed by its
shape: `true`/`false` become booleans, a canonical integer becomes a number
(`-P port=6000`), and everything else stays a string. Wrap a value in double
quotes to force a string that would otherwise read as a number or a boolean —
`-P 'password="123456"'`, `-P 'prefix="2026"'`. `run` accepts
`--parallel-targets` and `--fail-fast`. Filesystem destinations accept
`--no-preserve-ownership` when restoring without ownership changes.

## Transport flags (all module roles)

| Flag | Env | Default | Meaning |
|------|-----|---------|---------|
| `--to <host:port>` | `RUST_BACKUP_TO` | — | coordination server address; an `https://` prefix is what turns client TLS on (see below) |
| `--channel <id>` | `RUST_BACKUP_CHANNEL` | — | rendezvous channel id (source & dest must match) |
| `--secret <s>` / `--secret-file <path>` | `RUST_BACKUP_SECRET` / `RUST_BACKUP_SECRET_FILE` | none | shared HMAC secret; prefer file or environment over argv |
| `--carriers <n>` | `RUST_BACKUP_CARRIERS` | 1 | *requested* data carriers (1..32); filesystem may use all, PostgreSQL/MongoDB/S3 safely negotiate to 1. Both peers log the agreed count as `negotiated data plane carriers=N` |
| `--udp` / `--no-udp` | `RUST_BACKUP_UDP` | on | try the direct UDP/QUIC path (falls back to relay) |
| `--insecure` | `RUST_BACKUP_INSECURE` | off | skip TLS verification (testing only) |
| `--max-rate <bytes/s>` | `RUST_BACKUP_MAX_RATE` | unlimited | aggregate source payload rate cap |
| `--yes` *(dest)* | `RUST_BACKUP_YES` | off | auto-accept the plan (skip interactive prompt) |
| `--overwrite` *(dest)* | `RUST_BACKUP_OVERWRITE` | off | replace existing destination databases, collections, or objects |
| `--config <file>` | `RUST_BACKUP_CONFIG` | — | YAML config underlay |
| `-v` | — | — | increase log verbosity (repeatable) |

**Module boolean switches are three-state.** `--overwrite`, `--admin`,
`--path-style`, `--follow-symlinks`, `--no-preserve-ownership` and
`--preserve-xattr` mean "true" when given bare, take an explicit value with
`=` (`--overwrite=false`), and fold into the config only when they were actually
given — so an explicit `false` overrides a YAML `true`, like every other field.
The value must be attached with `=`; `--overwrite false` is rejected, because a
space-separated value would swallow the next argument.

**Client TLS is selected by the `--to` value, not by a flag.** `--to
coordinator:7835` speaks plain TCP; `--to https://coordinator:7835` performs a
TLS handshake against a coordination server started with `--tls-cert` and
`--tls-key`, verifying the certificate against the system roots — so the host in
`--to` must match the certificate's name. `http://` is accepted and means plain.
A scheme with no port defaults to 443 for `https://` and 80 for `http://`, so
name the port explicitly. `--insecure` skips certificate verification and is for
isolated tests only.

> **`--overwrite` destroys before it restores, and a restore is not atomic.**
> Plan a maintenance window on that basis. PostgreSQL drops each target
> database, MongoDB drops each target collection, and S3 deletes the stale keys
> in the target prefix — all *before* the first payload byte arrives. So a run
> that fails midway does not leave the previous contents behind: PostgreSQL and
> MongoDB additionally remove what the failed run created (see the module docs),
> which leaves the target absent rather than half-filled, and S3 leaves the
> objects it had already replaced. Nothing is ever reported as verified in these
> cases — but if you need the previous contents to survive a failed attempt,
> take your own snapshot first, or restore into a fresh database, collection or
> prefix and switch over afterwards.

## Verified completion

A successful source prints `BACKUP VERIFIED`, a successful destination prints
`RESTORE VERIFIED`, and their final progress line is `status="verified"` at
`100.0%`. Both lines carry the same full-payload BLAKE3. The destination must
first re-introspect backend metadata/catalogs and reread all persisted payload;
the source must then pass its full post-run immutability audit. Missing legacy
evidence, mismatches, unreadable restored data, and verification timeouts fail
the command with a non-zero exit.

## Server

```sh
rust-backup server --bind-addr 0.0.0.0 --control-port 7835 \
  --secret-file /run/secrets/rust-backup --tls-cert server.crt --tls-key server.key
```

| Flag | Env | Default | Meaning |
|------|-----|---------|---------|
| `--bind-addr` | `RUST_BACKUP_BIND_ADDR` | `0.0.0.0` | listen address |
| `--control-port` | `RUST_BACKUP_CONTROL_PORT` | `7835` | TCP control port (and the UDP port when `--udp` is on) |
| `--secret` | `RUST_BACKUP_SECRET` | none | shared HMAC secret |
| `--secret-file` | `RUST_BACKUP_SECRET_FILE` | none | same, read from a file |
| `--tls-cert` | `RUST_BACKUP_TLS_CERT` | none; TLS disabled | server certificate chain |
| `--tls-key` | `RUST_BACKUP_TLS_KEY` | none; required with `--tls-cert` | server private key |
| `--max-conns` | `RUST_BACKUP_MAX_CONNS` | `256` | the real bound, applied twice: at most this many client connections are accepted at once, and at most this many relayed substreams are spliced at once. A pairing uses two connections (source + destination), so this permits `--max-conns / 2` concurrent transfers. Excess connections wait in the kernel backlog rather than being dropped |
| `--udp` | `RUST_BACKUP_UDP` | on | broker the direct UDP/QUIC path |

## Modules

### postgres (PostgreSQL 10+, cluster fidelity)

```sh
# source: a READ-ONLY user is required
rust-backup postgres source --to coord:7835 --channel pg --secret s --host db-a \
  --port 5432 --user readonly --password pw --database app
# destination: an ADMIN user (CREATEROLE/CREATEDB + ownership) is required
rust-backup postgres destination --to coord:7835 --channel pg --secret s --host db-b \
  --user postgres --password pw --admin --yes
```
Params: `--host --port(5432) --user --password --database --sslmode` ; use
`-P sslrootcert=/path/to/ca.pem` for a private PostgreSQL CA; dest `--admin`.
Each reads `RUST_BACKUP_HOST`, `RUST_BACKUP_PORT`, `RUST_BACKUP_USER`,
`RUST_BACKUP_PASSWORD`, `RUST_BACKUP_DATABASE`, `RUST_BACKUP_SSLMODE`,
`RUST_BACKUP_ADMIN`.
The plan covers roles, grants, databases, schemas, tables, constraints, indexes,
sequences, extensions, and ownership.

### mongodb (MongoDB 4..=8)

```sh
rust-backup mongodb source --to coord:7835 --channel mg --uri "mongodb://ro@db-a:27017" --database app
rust-backup mongodb destination --to coord:7835 --channel mg --uri "mongodb://admin@db-b:27017" --yes
```
Params: `--uri` OR `--host --port(27017) --user --password` ; `--database --auth-db`
(`RUST_BACKUP_URI`, `RUST_BACKUP_HOST`, `RUST_BACKUP_PORT`, `RUST_BACKUP_USER`,
`RUST_BACKUP_PASSWORD`, `RUST_BACKUP_DATABASE`, `RUST_BACKUP_AUTH_DB`).

### filesystem (POSIX; ownership/permissions preserved on Linux)

```sh
rust-backup filesystem source --to coord:7835 --channel fs --root /data
sudo rust-backup filesystem destination --to coord:7835 --channel fs --root /restore --yes
```
Params: `--root --follow-symlinks --no-preserve-ownership --preserve-xattr`
(`RUST_BACKUP_ROOT`, `RUST_BACKUP_FOLLOW_SYMLINKS`,
`RUST_BACKUP_NO_PRESERVE_OWNERSHIP`, `RUST_BACKUP_PRESERVE_XATTR`).
Ownership preservation is on by default and has no positive flag of its own;
`--no-preserve-ownership` turns it off. In YAML and `-P` the parameter keeps its
underlying name, `preserve_ownership: true|false`.

> **sudo / ownership.** Restoring arbitrary `uid`/`gid` requires `root` or
> `CAP_CHOWN`. With ownership preservation enabled (the default), preflight
> fails if the destination cannot reproduce the planned ownership. Run the
> **destination** under `sudo`, or explicitly choose the reduced contract with
> `--no-preserve-ownership`. The **source** never needs privileges (read-only).
> See `docs/modules/FILESYSTEM.md`.

### s3 (AWS S3 + MinIO)

```sh
rust-backup s3 source --to coord:7835 --channel s3 --bucket src --region eu-west-1 \
  --access-key AK --secret-key SK
rust-backup s3 destination --to coord:7835 --channel s3 --bucket dst \
  --endpoint http://minio:9000 --path-style --access-key AK --secret-key SK --yes
```
Params: `--endpoint(MinIO) --region --bucket --prefix --access-key --secret-key --path-style`
(`RUST_BACKUP_ENDPOINT`, `RUST_BACKUP_REGION`, `RUST_BACKUP_BUCKET`,
`RUST_BACKUP_PREFIX`, `RUST_BACKUP_ACCESS_KEY`, `RUST_BACKUP_SECRET_KEY`,
`RUST_BACKUP_PATH_STYLE`).

## Multi-target YAML session

Run several backups in one session (`rust-backup run --config session.yml`):

```yaml
targets:
  - module: postgres
    role: source
    transport: { to: coord:7835, channel: pg1, secret: s3cr3t }
    params: { host: db-a, user: readonly, password: pw, database: app }
  - module: filesystem
    role: destination
    transport: { to: coord:7835, channel: fs1 }
    params: { root: /restore }
    auto_accept: true
```

`run --config` rejects a `server:` section: start the coordination service
explicitly with `rust-backup server`. CLI flags and env vars override matching
YAML fields; typed CLI options and `-P key=value` override module params.

## Async (non-automatic) mode

By default the destination prints the received plan + preflight results and waits for
`yes` on stdin before the transfer starts. Pass `--yes` (or `auto_accept: true` in
YAML) to proceed automatically.

## Environment variables with no flag

These are read directly from the environment and therefore appear in no
`--help` output. Every one of them is optional.

| Variable | Default | Meaning |
|----------|---------|---------|
| `RUST_LOG` | `info` | `tracing` filter; `-v` raises the level without it |
| `RUST_BACKUP_PLAN_TIMEOUT` | `600` | seconds to wait for the peer's plan exchange (`Plan`/`PlanAck`). A value of `0` or unparsable text falls back to the default |
| `RUST_BACKUP_VERIFY_TIMEOUT` | `86400` | seconds the source waits for the destination's read-back verification. Deliberately separate from transfer time: a large database or bucket needs a complete second read |
| `RUST_BACKUP_STUN_SERVERS` | `stun.l.google.com:19302,stun.cloudflare.com:3478` | comma-separated STUN chain for the direct UDP path; the first four entries are used. `RUST_BACKUP_STUN_SERVER` (singular) is still accepted and used only when the plural one is unset |
| `BORE_PROXY_BUFFER_SIZE` | `256 KiB` | relay splice buffer; accepts suffixes (`512k`, `1MiB`) and is clamped to 4 KiB..16 MiB. Inherited from the vendored `bore` transport |

`RUST_BACKUP_S3_TEST_FAIL_AFTER_PART` is a test-only fault hook used by the
MinIO e2e script; it is not part of the supported configuration surface.

## Exit codes

`0` success · `1` any other failure · `2` config · `3` preflight · `4` plan
rejected by the operator · `5` integrity, apply or verify · `6` source mutation ·
`7` transport/connect.

Code `5` covers all three: an integrity failure (a digest or a read-back
mismatch) and any error tagged `[Apply]` or `[Verify]`.
