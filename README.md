# rust-backup

[![CI](https://github.com/manprint/rust-backup/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/ci.yml)
[![End-to-end](https://github.com/manprint/rust-backup/actions/workflows/e2e.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/e2e.yml)
[![Security](https://github.com/manprint/rust-backup/actions/workflows/security.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/security.yml)
[![Container](https://github.com/manprint/rust-backup/actions/workflows/docker.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/docker.yml)

`rust-backup` streams a backup directly from a read-only source to its
destination through a coordination server. It does not create an intermediate
archive. The destination validates a self-contained plan before applying data,
each item and the complete payload are verified with BLAKE3, and the source is
fingerprinted again on every exit path to prove that it was not modified.

Supported modules:

- PostgreSQL 10–18: logical cluster metadata and binary `COPY` data.
- MongoDB 4–8: databases, collections, indexes, users and BSON documents.
- POSIX filesystem: files, directories, links, modes, ownership and mtimes.
- AWS S3 and compatible storage such as MinIO: streaming multipart restore.

The transport uses a TCP/yamux relay and can upgrade carriers to direct
UDP/QUIC. If direct setup is unavailable it falls back to the relay; an active
stream is never transparently resumed after a connection loss.

## Install

Download a Linux `x86_64` or `aarch64` archive from
[GitHub Releases](https://github.com/manprint/rust-backup/releases), verify it,
and install the binary:

```bash
sha256sum --check rust-backup-*.tar.gz.sha256
tar -xzf rust-backup-*.tar.gz
sudo install -m 0755 rust-backup-*/rust-backup /usr/local/bin/rust-backup
rust-backup --version
```

Or build the locked workspace:

```bash
git clone https://github.com/manprint/rust-backup.git
cd rust-backup
cargo build --locked --release --all-features
install -m 0755 target/release/rust-backup ~/.local/bin/rust-backup
```

The container is published for `linux/amd64` and `linux/arm64`:

```bash
docker pull ghcr.io/manprint/rust-backup:latest  # main
docker pull ghcr.io/manprint/rust-backup:dev     # dev
docker run --rm ghcr.io/manprint/rust-backup:latest --version
```

## Architecture

```text
 source ──register(channel)──▶ coordination server ◀──connect(channel)── destination
   │ analyze → plan ───────────── relay/direct stream ────────────────▶ validate
   │ read-only stream ═════ BLAKE3 chunks + backpressure ════════════▶ restore
   └ source fingerprint unchanged                 CompleteAck ◀───────┘
```

Source and destination must use the same coordination address, channel and
secret. Start the source first; it waits for the destination. A destination
prints the plan and asks for confirmation unless `--yes` or
`auto_accept: true` is set.

## Deploy the coordination server

The server listens on TCP `7835` for coordination/relay traffic and UDP `7835`
for direct-path negotiation. Expose both protocols when `--udp=true`.

### Docker Compose

Create a secret, then start the hardened non-root container:

```bash
cp deploy/coordination/coordination.secret.example \
  deploy/coordination/coordination.secret
openssl rand -base64 48 > deploy/coordination/coordination.secret
chmod 600 deploy/coordination/coordination.secret

docker compose pull
docker compose up -d coordinator
docker compose ps
docker compose logs -f coordinator
```

Configuration is in [compose.yml](compose.yml). Useful overrides:

```bash
RUST_BACKUP_IMAGE=ghcr.io/manprint/rust-backup:dev \
RUST_BACKUP_CONTROL_PORT=17835 \
RUST_BACKUP_UDP_PORT=17835 \
RUST_BACKUP_MAX_CONNS=1024 \
docker compose up -d
```

For native TLS, place `tls.crt` and `tls.key` in
`deploy/coordination/tls/` and apply the override:

```bash
docker compose -f compose.yml \
  -f deploy/coordination/compose.tls.yml up -d
```

Use `https://coordinator.example:7835` as the client `--to` value when TLS is
enabled. Do not use `--insecure` outside isolated tests. A reverse proxy can
terminate TCP TLS, but UDP `7835` must still reach the coordinator directly if
the QUIC path is enabled.

### Binary or systemd

Run the binary directly:

```bash
sudo install -d -m 0750 -o rust-backup -g rust-backup /etc/rust-backup
openssl rand -base64 48 | sudo tee /etc/rust-backup/coordination.secret >/dev/null
sudo chmod 600 /etc/rust-backup/coordination.secret

rust-backup server \
  --bind-addr 0.0.0.0 \
  --control-port 7835 \
  --secret-file /etc/rust-backup/coordination.secret \
  --max-conns 256 \
  --udp=true
```

Example `/etc/systemd/system/rust-backup-coordinator.service`:

```ini
[Unit]
Description=rust-backup coordination server
After=network-online.target
Wants=network-online.target

[Service]
User=rust-backup
Group=rust-backup
ExecStart=/usr/local/bin/rust-backup server --bind-addr 0.0.0.0 --control-port 7835 --secret-file /etc/rust-backup/coordination.secret --max-conns 256 --udp=true
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true

[Install]
WantedBy=multi-user.target
```

Then run `sudo systemctl daemon-reload && sudo systemctl enable --now
rust-backup-coordinator`. Allow `7835/tcp` and, when enabled, `7835/udp` in the
host and cloud firewalls.

## Configuration files

CLI flags override environment variables, which override YAML. Transport and
backend credentials are redacted from debug output, but YAML files still contain
plain secrets: create them with mode `0600`, keep them outside Git, or render the
templates from a secret manager immediately before use.

Ready-to-edit paired source/destination templates:

- [examples/filesystem-session.yml](examples/filesystem-session.yml)
- [examples/postgres-session.yml](examples/postgres-session.yml)
- [examples/mongodb-session.yml](examples/mongodb-session.yml)
- [examples/s3-session.yml](examples/s3-session.yml)
- [examples/session.yml](examples/session.yml) for a mixed session

A paired file must set `parallel_targets: 2` or greater so source and
destination can rendezvous concurrently:

```bash
cp examples/postgres-session.yml /secure/path/postgres-session.yml
chmod 600 /secure/path/postgres-session.yml
$EDITOR /secure/path/postgres-session.yml
rust-backup run --config /secure/path/postgres-session.yml
```

For separate hosts, run one target per file or use the direct CLI commands in
the following sections.

## Filesystem backup

Binary, in two terminals or hosts:

```bash
# Source is read-only and normally does not need root.
rust-backup filesystem source \
  --to coordinator.example:7835 --channel fs-prod \
  --secret-file /etc/rust-backup/coordination.secret \
  --carriers 4 --root /srv/data

# Root/CAP_CHOWN is required only for exact uid/gid restoration.
sudo rust-backup filesystem destination \
  --to coordinator.example:7835 --channel fs-prod \
  --secret-file /etc/rust-backup/coordination.secret \
  --carriers 4 --root /srv/restore --yes
```

Docker on Linux (`--network host` lets the client reach host services):

```bash
IMAGE=ghcr.io/manprint/rust-backup:latest
SECRET="$PWD/deploy/coordination/coordination.secret"

docker run --rm --network host \
  -v "$SECRET:/run/secrets/coordinator:ro" \
  -v /srv/data:/source:ro \
  "$IMAGE" filesystem source --to 127.0.0.1:7835 --channel fs-prod \
  --secret-file /run/secrets/coordinator --carriers 4 --root /source

docker run --rm --network host --user 0:0 \
  -v "$SECRET:/run/secrets/coordinator:ro" \
  -v /srv/restore:/restore \
  "$IMAGE" filesystem destination --to 127.0.0.1:7835 --channel fs-prod \
  --secret-file /run/secrets/coordinator --carriers 4 --root /restore --yes
```

`--follow-symlinks` and `--preserve-xattr` are deliberately rejected in the
current safe backend; symlinks themselves are preserved. See
[filesystem details](docs/modules/FILESYSTEM.md).

## PostgreSQL backup

The source account must be read-only. The destination account needs the rights
reported by preflight (normally cluster administration, role and database
creation). Existing target databases are rejected unless `overwrite=true` is
explicitly configured.

```bash
# Source
rust-backup postgres source \
  --to coordinator.example:7835 --channel pg-prod --secret "$RB_SECRET" \
  --host pg-source.internal --port 5432 --user backup_readonly \
  --password "$SOURCE_PG_PASSWORD" --database app --sslmode require

# Destination
rust-backup postgres destination \
  --to coordinator.example:7835 --channel pg-prod --secret "$RB_SECRET" \
  --host pg-destination.internal --port 5432 --user postgres \
  --password "$DEST_PG_PASSWORD" --database postgres --sslmode require --admin --yes
```

Containerized clients use the same arguments:

```bash
docker run --rm --network host \
  ghcr.io/manprint/rust-backup:latest postgres source \
  --to 127.0.0.1:7835 --channel pg-prod --secret "$RB_SECRET" \
  --host 127.0.0.1 --port 5432 --user backup_readonly \
  --password "$SOURCE_PG_PASSWORD" --database app

docker run --rm --network host \
  ghcr.io/manprint/rust-backup:latest postgres destination \
  --to 127.0.0.1:7835 --channel pg-prod --secret "$RB_SECRET" \
  --host 127.0.0.1 --port 55432 --user postgres \
  --password "$DEST_PG_PASSWORD" --admin --yes
```

Use `-P sslrootcert=/run/secrets/postgres-ca.pem` for a private CA. See
[PostgreSQL fidelity and privileges](docs/modules/POSTGRES.md).

## MongoDB backup

Omit `--database` to copy all non-system databases. The source user needs read
access and metadata visibility; the destination user needs database/user/index
administration appropriate to the plan.

```bash
rust-backup mongodb source \
  --to coordinator.example:7835 --channel mongo-prod --secret "$RB_SECRET" \
  --uri "mongodb://backup_readonly:${MONGO_SOURCE_PASSWORD}@mongo-source.internal:27017/?authSource=admin" \
  --database app --auth-db admin

rust-backup mongodb destination \
  --to coordinator.example:7835 --channel mongo-prod --secret "$RB_SECRET" \
  --uri "mongodb://root:${MONGO_DEST_PASSWORD}@mongo-destination.internal:27017/?authSource=admin" \
  --database app --auth-db admin --yes
```

Docker example:

```bash
docker run --rm --network host ghcr.io/manprint/rust-backup:latest \
  mongodb source --to 127.0.0.1:7835 --channel mongo-prod --secret "$RB_SECRET" \
  --uri "mongodb://backup_readonly:${MONGO_SOURCE_PASSWORD}@127.0.0.1:27017/?authSource=admin" --database app

docker run --rm --network host ghcr.io/manprint/rust-backup:latest \
  mongodb destination --to 127.0.0.1:7835 --channel mongo-prod --secret "$RB_SECRET" \
  --uri "mongodb://root:${MONGO_DEST_PASSWORD}@127.0.0.1:37017/?authSource=admin" --database app --yes
```

See [MongoDB fidelity details](docs/modules/MONGODB.md).

## S3 or MinIO backup

Versioned buckets, object-lock retention and non-portable server-side encryption
are rejected rather than silently producing a weaker copy. The source key needs
list/get and metadata/policy visibility; the destination key needs bucket/object
write and multipart permissions.

```bash
rust-backup s3 source \
  --to coordinator.example:7835 --channel s3-prod --secret "$RB_SECRET" \
  --region eu-south-1 --bucket source-bucket --prefix production/ \
  --access-key "$SRC_ACCESS_KEY" --secret-key "$SRC_SECRET_KEY"

rust-backup s3 destination \
  --to coordinator.example:7835 --channel s3-prod --secret "$RB_SECRET" \
  --region eu-south-1 --bucket restored-bucket --prefix recovered/ \
  --access-key "$DST_ACCESS_KEY" --secret-key "$DST_SECRET_KEY" \
  -P create_bucket=true --yes
```

For MinIO or another custom endpoint, add `--endpoint` and `--path-style`:

```bash
docker run --rm --network host ghcr.io/manprint/rust-backup:latest \
  s3 source --to 127.0.0.1:7835 --channel minio --secret "$RB_SECRET" \
  --endpoint http://127.0.0.1:9000 --region us-east-1 --path-style \
  --bucket source --access-key minioadmin --secret-key "$MINIO_PASSWORD"

docker run --rm --network host ghcr.io/manprint/rust-backup:latest \
  s3 destination --to 127.0.0.1:7835 --channel minio --secret "$RB_SECRET" \
  --endpoint http://127.0.0.1:9000 --region us-east-1 --path-style \
  --bucket destination --access-key minioadmin --secret-key "$MINIO_PASSWORD" \
  -P create_bucket=true --yes
```

See [S3 fidelity and AWS IAM guidance](docs/modules/S3.md).

## End-to-end verification

Every backend has an executable e2e path. These commands build real binaries,
seed real services/files, perform source → destination restore, compare results,
check source immutability and clean their resources:

```bash
# Filesystem relay, TLS and 1/4 carriers; sessions; faults; MinIO.
bash e2e/full_matrix.sh

# Filesystem ownership, interruption cleanup, real ext4 ENOSPC and transport.
sudo -n "$PWD/e2e/filesystem_netns_test.sh"
sudo -n "$PWD/e2e/filesystem_disk_full.sh"
sudo -n "$PWD/e2e/transport_netns_test.sh"
sudo -n "$PWD/e2e/bandwidth_netem.sh"

# PostgreSQL 10, 12, 14, 16 and 18.
bash e2e/postgres_introspect.sh 16
bash e2e/postgres_matrix.sh 10 12 14 16 18

# MongoDB 4, 5, 6, 7 and 8.
bash e2e/mongodb_matrix.sh 4 5 6 7 8

# S3-compatible and optional credential-gated real AWS.
bash e2e/s3_minio_test.sh
RUST_BACKUP_AWS_E2E=1 bash e2e/s3_aws_test.sh
```

The one-command aggregate supports opt-ins:

```bash
RUST_BACKUP_PRIVILEGED=1 \
RUST_BACKUP_FULL_DB_MATRIX=1 \
bash e2e/full_matrix.sh
```

Privileged tests intentionally require root. Do not install a sudoers wildcard
over a user-writable checkout on a shared host; use an ephemeral CI runner or a
root-owned test wrapper. Full prerequisites and test IDs are in
[e2e/README.md](e2e/README.md) and [docs/QA_GUIDE.md](docs/QA_GUIDE.md).

## CI/CD and release policy

Every push and pull request targeting `dev` or `main` runs:

- `ci.yml`: fmt, Clippy with warnings denied, all/default feature builds and
  tests, rustdoc, shell lint, actionlint and Compose validation.
- `e2e.yml`: every local e2e suite, database version matrices and privileged
  network/disk/bandwidth tests. Real AWS runs only when the protected `aws-e2e`
  environment, repository variables and secrets are configured.
- `security.yml`: RustSec, cargo-deny advisories/licenses/bans/sources, unused,
  duplicate and outdated dependency checks, dependency review, workflow audit
  and CodeQL.
- `docker.yml`: non-root image smoke test, critical Trivy scan, multi-arch GHCR
  publishing, SBOM/provenance and GitHub attestation.
- `release.yml`: reproducible locked Linux amd64/arm64 archives on both branches;
  a signed `v*` tag additionally creates a GitHub Release and checksums.

Dependabot targets `dev` weekly for Cargo, GitHub Actions and Docker updates.
Third-party actions are pinned to immutable commit SHAs.

Create a release after `dev` has been merged into `main`:

```bash
git switch main
git pull --ff-only
git tag -s v0.1.0 -m 'rust-backup v0.1.0'
git push origin v0.1.0
```

## More documentation

- [Complete CLI reference](USAGE.md)
- [Transport and carrier contract](docs/TRANSPORT.md)
- [Module-specific behavior](docs/modules/README.md)
- [QA guide](docs/QA_GUIDE.md)
- [Severe implementation audit](docs/plans/RUST_BACKUP_AUDIT_2026-07-29.md)

Licensed under [AGPL-3.0-or-later](LICENSE).
