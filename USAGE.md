# rust-backup — Usage

Three ways to configure, highest precedence first: **CLI flags > environment
variables > YAML config**. Every CLI flag has a `RUST_BACKUP_<UPPER_SNAKE>` env var.

## Subcommands

```
rust-backup <module> <role> [PARAMS]   module = postgres|mongodb|filesystem|s3 ; role = source|destination
rust-backup server [SERVER OPTS]        run the coordination server
rust-backup run --config session.yml    run a multi-target session from YAML
rust-backup plan <module> source [PARAMS]  dry-run: analyze + print the plan, no transfer
```

## Transport flags (all module roles)

| Flag | Env | Default | Meaning |
|------|-----|---------|---------|
| `--to <host:port>` | `RUST_BACKUP_TO` | — | coordination server address |
| `--channel <id>` | `RUST_BACKUP_CHANNEL` | — | rendezvous channel id (source & dest must match) |
| `--secret <s>` / `--secret-file <path>` | `RUST_BACKUP_SECRET` / `RUST_BACKUP_SECRET_FILE` | none | shared HMAC secret; prefer file or environment over argv |
| `--carriers <n>` | `RUST_BACKUP_CARRIERS` | 1 | only `1` is currently accepted; multi-carrier transfer is not implemented |
| `--udp` / `--no-udp` | `RUST_BACKUP_UDP` | on | try the direct UDP/QUIC path (falls back to relay) |
| `--insecure` | `RUST_BACKUP_INSECURE` | off | skip TLS verification (testing only) |
| `--max-rate <bytes/s>` | `RUST_BACKUP_MAX_RATE` | unlimited | aggregate source payload rate cap |
| `--yes` *(dest)* | `RUST_BACKUP_YES` | off | auto-accept the plan (skip interactive prompt) |
| `--config <file>` | `RUST_BACKUP_CONFIG` | — | YAML config underlay |
| `-v` | — | — | increase log verbosity (repeatable) |

## Server

```sh
rust-backup server --bind-addr 0.0.0.0 --control-port 7835 \
  --secret-file /run/secrets/rust-backup --tls-cert server.crt --tls-key server.key
```

| Flag | Env | Default |
|------|-----|---------|
| `--bind-addr` | `RUST_BACKUP_BIND_ADDR` | `0.0.0.0` |
| `--control-port` | `RUST_BACKUP_CONTROL_PORT` | `7835` |
| `--secret` | `RUST_BACKUP_SECRET` | none |
| `--secret-file` | `RUST_BACKUP_SECRET_FILE` | none |
| `--tls-cert` | `RUST_BACKUP_TLS_CERT` | none; TLS disabled |
| `--tls-key` | `RUST_BACKUP_TLS_KEY` | none; required with `--tls-cert` |
| `--max-conns` | `RUST_BACKUP_MAX_CONNS` | `256` |
| `--udp` | `RUST_BACKUP_UDP` | on |

## Modules

### postgres (PostgreSQL 10..=latest, cluster fidelity)

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
The plan covers roles, grants, databases, schemas, tables, constraints, indexes,
sequences, extensions, and ownership.

### mongodb (MongoDB 4..=8)

```sh
rust-backup mongodb source --to coord:7835 --channel mg --uri "mongodb://ro@db-a:27017" --database app
rust-backup mongodb destination --to coord:7835 --channel mg --uri "mongodb://admin@db-b:27017" --yes
```
Params: `--uri` OR `--host --port(27017) --user --password` ; `--database --auth-db`.

### filesystem (POSIX; ownership/permissions preserved on Linux)

```sh
rust-backup filesystem source --to coord:7835 --channel fs --root /data
sudo rust-backup filesystem destination --to coord:7835 --channel fs --root /restore --yes
```
Params: `--root --follow-symlinks --preserve-ownership(true) --preserve-xattr`.

> **sudo / ownership.** Restoring arbitrary `uid`/`gid` requires `root` or
> `CAP_CHOWN`. Without it, file **contents and mode** are restored but ownership
> falls back to the running user, and preflight emits a warning. Run the
> **destination** under `sudo` to preserve ownership exactly. The **source** never
> needs privileges (read-only). See `docs/modules/FILESYSTEM.md`.

### s3 (AWS S3 + MinIO)

```sh
rust-backup s3 source --to coord:7835 --channel s3 --bucket src --region eu-west-1 \
  --access-key AK --secret-key SK
rust-backup s3 destination --to coord:7835 --channel s3 --bucket dst \
  --endpoint http://minio:9000 --path-style --access-key AK --secret-key SK --yes
```
Params: `--endpoint(MinIO) --region --bucket --prefix --access-key --secret-key --path-style`.

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

## Exit codes

`0` success · `2` config · `3` preflight · `4` operator rejection · `5`
integrity/apply · `6` source mutation · `7` transport · `1` other failure.
