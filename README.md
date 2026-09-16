# rust-backup

[![CI](https://github.com/manprint/rust-backup/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/ci.yml)
[![End-to-end](https://github.com/manprint/rust-backup/actions/workflows/e2e.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/e2e.yml)
[![Security](https://github.com/manprint/rust-backup/actions/workflows/security.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/security.yml)
[![Container](https://github.com/manprint/rust-backup/actions/workflows/docker.yml/badge.svg?branch=dev)](https://github.com/manprint/rust-backup/actions/workflows/docker.yml)

`rust-backup` streams a backup directly from a read-only source to its
destination through a coordination server. It does not create an intermediate
archive. The destination validates a self-contained plan before applying data,
each item and the complete payload are verified with BLAKE3, the persisted
destination is read back through the backend, and the source is fingerprinted
again on every exit path to prove that it was not modified.

Supported modules:

- PostgreSQL 10+: logical cluster metadata and binary `COPY` data. Major 10 is
  the enforced minimum; the CI matrix covers 10–18.
- MongoDB 4–8: databases, collection options, indexes and BSON documents.
- POSIX filesystem: files, directories, links, modes, ownership and mtimes.
- AWS S3 and compatible storage such as MinIO: streaming multipart restore.

The transport uses a TCP/yamux relay and can upgrade carriers to direct
UDP/QUIC. If direct setup is unavailable it falls back to the relay; an active
stream is never transparently resumed after a connection loss.

## Documentation of record

**[`docs/usage/`](docs/usage/README.md) is the single source of truth for using
this program** — one page per feature (server, transport, postgres, mongodb,
filesystem, s3, YAML sessions, `plan`, Docker, environment variables, exit
codes), each opening with its minimal working example and covering every flag.
The guide is written in Italian; the sections below stay as a quick English
overview, and `scripts/gates.sh` enforces that the guide and `--help` agree.

[`docs/IMMUTABILITY.md`](docs/IMMUTABILITY.md) is the reference for the one
promise that outranks all the others — a backup never alters its source: what is
guarded, how it is proved, and the read-only role to create for each backend.

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
   │                                          persisted backend read-back
   └ source fingerprint unchanged        VerificationAck ◀───────────┘
```

Source and destination must use the same coordination address, channel and
secret. Start the source first; it waits for the destination. A destination
prints the plan and asks for confirmation unless `--yes` or
`auto_accept: true` is set.

## Verified completion

Exit code `0` is issued only after both sides complete the formal proof. The
destination reopens the restored backend, compares its restorable catalog or
metadata, streams every persisted data item again, and reproduces the source
item and payload BLAKE3 commitments. The source then proves its own complete
fingerprint is unchanged. A legacy completion acknowledgement without read-back
evidence is rejected.

Successful runs end with `status="verified"` at `100.0%` and these messages on
the respective peers:

```text
BACKUP VERIFIED: source unchanged; destination read-back matches
RESTORE VERIFIED: persisted destination matches source
```

For PostgreSQL the destination also prints, under that record, what it proved:

```text
rows verified: 103590 from tables, 1137 from materialized views, 104727 rows
constraints: 32 (validated 30, not valid 2)
```

The first line counts the rows `COPY` wrote and the rows `REFRESH MATERIALIZED
VIEW` produced; they are the numbers the source counted while analyzing, so a
restore that loses rows fails instead of certifying itself. The second counts
the constraints re-read from the destination catalog and compared with the
source, `not valid` being those that legitimately stay `NOT VALID`. A
`deviation: ...` line is added for anything deliberately restored differently,
such as an extension installed at the destination's own version.

Both messages include the same payload BLAKE3. Apply, catalog, data read-back,
source audit, timeout, or acknowledgement failures produce a non-zero exit and
never print verified completion. See each module document for the exact
restorable contract and its explicit exclusions.

## Source safety

A backup never alters its source, and that is enforced rather than promised. The
source side of every module holds a read-only handle — it has no write call to
offer — statements and commands are checked against a read allowlist before they
are sent, and the complete source fingerprint is measured before the run and
again afterwards, on every exit path: success, failure, an abort mid-stream, and
a pipeline that panicked. A source that changed during the run ends the transfer
with `SOURCE-IMMUTABILITY VIOLATION` and exit `6`, and a destination that had
already written the payload drops it instead of certifying it.

Give the source a least-privilege account anyway, so the *server* refuses a
write even if this program were wrong:

```sql
-- PostgreSQL 14 and later
CREATE ROLE rb_ro LOGIN PASSWORD 'change-me';
GRANT pg_read_all_data TO rb_ro;
GRANT CONNECT ON DATABASE app TO rb_ro;
```

On PostgreSQL 10 to 13 `pg_read_all_data` does not exist yet, so grant `USAGE`
plus `SELECT ON ALL TABLES` and `ON ALL SEQUENCES` per non-system schema. On
MongoDB create a user with `read` on each copied database, plus `viewUser` and
`viewRole` on it if the plan should also record the database's users. On S3 or
MinIO use a credential whose policy allows only `s3:ListBucket`,
`s3:GetBucketPolicy`, `s3:GetBucketVersioning`, `s3:GetObject` and
`s3:GetObjectTagging` (`s3:GetObjectAcl` too on AWS, which has that action).
Every recipe, with the reason for each grant, is in
[`docs/IMMUTABILITY.md`](docs/IMMUTABILITY.md).

A PostgreSQL role that can write the source is not refused — which role to use
is the operator's decision — but the run warns once: `source role <user> can
write to the source; a read-only role is recommended, see docs/IMMUTABILITY.md`.

For a filesystem source the same rule covers access times. Reading a file
normally moves its `atime`, so the source opens every file with `O_NOATIME`,
which the kernel allows only to the file's owner or to a process with
`CAP_FOWNER`. Running as anyone else fails at the first read:

```text
cannot open /srv/data/report.csv without updating its access time (O_NOATIME
needs file ownership or CAP_FOWNER); run as the file owner or root, or pass
--allow-atime-updates to accept atime changes on the source
```

The remedy is in the message: run the source as the owner of the tree (or as
root), or, when moving access times is acceptable, pass
`--allow-atime-updates` (`RUST_BACKUP_ALLOW_ATIME_UPDATES=true`, or
`allow_atime_updates: true` in a session file — default `false`). The run then
warns once, `atime updates on the source accepted by --allow-atime-updates`, and
nothing else about the source is touched: contents, ownership, permissions and
modification times are read only.

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

Every variable `compose.yml` reads, and what it actually changes:

| Variable | Default | Effect |
|----------|---------|--------|
| `RUST_BACKUP_IMAGE` | `ghcr.io/manprint/rust-backup:latest` | image to run |
| `RUST_BACKUP_CONTROL_PORT` | `7835` | **published host TCP port only.** The server inside the container always listens on 7835 — the compose `command` passes `--control-port 7835` literally. Clients still connect to the host port you publish here |
| `RUST_BACKUP_UDP_PORT` | `7835` | published host UDP port, same mapping-only meaning |
| `RUST_BACKUP_MAX_CONNS` | `256` | passed through as `--max-conns`; this one does change the server |
| `RUST_BACKUP_UDP` | `true` | passed through as `--udp` |
| `RUST_BACKUP_SECRET_FILE` | `./deploy/coordination/coordination.secret` | host path mounted as the `coordination_secret` docker secret |
| `RUST_BACKUP_VERSION` | `dev` | build arg stamped into the image (`docker compose build` only) |
| `RUST_BACKUP_VCS_REF` | `local` | build arg stamped into the image (`docker compose build` only) |

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
# Source is read-only; run it as the owner of the tree (see "Source safety").
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

### What is copied, what is refused

Regular files, directories, symlinks, hardlinks, FIFOs and character or block
device nodes round-trip with their permission bits — setuid, setgid and the
sticky bit included — and their modification time. Ownership is restored
exactly when the destination runs as root or holds `CAP_CHOWN`; device nodes
additionally need root or `CAP_MKNOD` there, and the restore refuses in
preflight, before writing anything, when that privilege is missing. Hardlinks
are recreated as links to the same file, and symlinks stay symlinks —
`--follow-symlinks` is rejected.

Three entries are refused rather than copied approximately: a **unix socket**
(it cannot be recreated as the socket it was), a **path that is not valid
UTF-8**, and a **tree deeper than 1024 levels**. Extended attributes and POSIX
ACLs are never read, so `--preserve-xattr` is rejected too. One thing is
restored differently on purpose: a **sparse file** arrives byte-identical but
dense, so the copy can occupy more disk than the original.

Reading files the source account does not own needs `--allow-atime-updates`,
described under [Source safety](#source-safety). See
[filesystem details](docs/modules/FILESYSTEM.md).

### Troubleshooting

```text
[Analyze] unsupported filesystem entry (a unix socket cannot be reproduced): /srv/data/run/app.sock
```

The source stops before streaming anything. Point `--root` at a subtree that
holds no socket — a live socket belongs to the running service, not to its
data.

```text
FAILED: restoring device nodes needs root or CAP_MKNOD (2 device entries in the plan) check=special_files
```

The plan carries device nodes and the destination cannot create them, so the
run stops in preflight with `preflight failed` and nothing is written. Run the
destination as root, or grant the binary the capability
(`sudo setcap cap_mknod,cap_chown+ep /usr/local/bin/rust-backup`).

```text
[Apply] destination path was replaced by a symlink during restore: /srv/restore/data
```

Something under `--root` turned a directory into a symlink while the restore
was running. The restore stops instead of following it out of the destination
tree; restore into a directory no other process writes to.

## PostgreSQL backup

The source account must be read-only. A role that can write the source is not
refused, but the run warns once — `source role <user> can write to the source; a
read-only role is recommended, see docs/IMMUTABILITY.md` — and the source path
is protected anyway: its client type exposes no write method, every statement
passes a read-only allowlist, and the session is opened with
`default_transaction_read_only=on`. The destination account needs the rights
reported by preflight (normally cluster administration, role and database
creation). Existing target databases are rejected unless `--overwrite` (or the
equivalent `overwrite: true` YAML parameter) is explicitly configured.

Pass credentials in the environment, not on the command line:
`/proc/<pid>/cmdline` is world-readable on Linux, so a `--password` flag is
visible to every local account for as long as the transfer runs, while
`/proc/<pid>/environ` is readable only by the owning user. Every flag has a
`RUST_BACKUP_<UPPER_SNAKE>` equivalent
([docs/usage/10-variabili-ambiente.md](docs/usage/10-variabili-ambiente.md)).

```bash
# Source
RUST_BACKUP_PASSWORD="$SOURCE_PG_PASSWORD" rust-backup postgres source \
  --to coordinator.example:7835 --channel pg-prod --secret-file /run/secrets/rb \
  --host pg-source.internal --port 5432 --user backup_readonly \
  --database app --sslmode require

# Destination
RUST_BACKUP_PASSWORD="$DEST_PG_PASSWORD" rust-backup postgres destination \
  --to coordinator.example:7835 --channel pg-prod --secret-file /run/secrets/rb \
  --host pg-destination.internal --port 5432 --user postgres \
  --database postgres --sslmode require --admin --yes
```

Containerized clients use the same arguments:

```bash
docker run --rm --network host \
  -e RUST_BACKUP_PASSWORD="$SOURCE_PG_PASSWORD" \
  ghcr.io/manprint/rust-backup:latest postgres source \
  --to 127.0.0.1:7835 --channel pg-prod --secret "$RB_SECRET" \
  --host 127.0.0.1 --port 5432 --user backup_readonly --database app

docker run --rm --network host \
  -e RUST_BACKUP_PASSWORD="$DEST_PG_PASSWORD" \
  ghcr.io/manprint/rust-backup:latest postgres destination \
  --to 127.0.0.1:7835 --channel pg-prod --secret "$RB_SECRET" \
  --host 127.0.0.1 --port 55432 --user postgres --admin --yes
```

Extensions are restored at the version the source runs, and the rows an
extension registered as configuration data travel with them — a custom PostGIS
`spatial_ref_sys` entry or a tuned configuration table arrives on the
destination instead of being replaced by whatever `CREATE EXTENSION` inserts.

A destination that does not carry that exact version is refused before anything
is written:

```text
extension postgis version 3.4.4 is not available on the destination
(available: 3.5.3); install it or pass --extension-version default
```

Install the missing version, or accept the destination's own:

```bash
RUST_BACKUP_PASSWORD="$DEST_PG_PASSWORD" rust-backup postgres destination \
  --to coordinator.example:7835 --channel pg-prod --secret-file /run/secrets/rb \
  --host pg-destination.internal --port 5432 --user postgres \
  --database postgres --sslmode require --admin --yes \
  --extension-version default
```

The restore then reports what it substituted under its verified record —
`deviation: extension postgis restored at version 3.5.3 (source 3.4.4)` — so the
difference is recorded rather than silent. The same choice is available as
`RUST_BACKUP_EXTENSION_VERSION`, and its default, `source`, is the refusal
above. An extension the destination does not have at all is always refused.

The source is read with a 30-second lock timeout, so a table somebody else holds
under `ACCESS EXCLUSIVE` ends the run instead of stalling it:

```text
[Analyze] introspect: count app.accounts: db error: ERROR: canceling statement
due to lock timeout
```

Wait for the lock to be released (a maintenance job, a long `ALTER TABLE`) and
run it again — nothing was written on the destination. Statements themselves are
not time-bounded, because copying a large table is legitimately slow, and the
run opens one connection to the bootstrap database plus one per database it
copies, with TCP keepalives on both sides.

Before and after every run the source is measured — the catalog, plus the row
count and an order-independent commitment over the rows of each table — and any
difference between the two measurements fails the run with exit `6` rather than
certifying a copy of a moving database. That costs one full read of the source
per measurement, so a backup reads the data three times in total: two audits and
the copy itself.

A restore that ends with

```text
app.accounts: source counted 1000 rows, destination COPY wrote 999
```

wrote fewer rows than the source counted and refused to certify the result: it
exits `5` and drops the databases it created, so nothing half-restored is left
to be mistaken for a copy. Re-run it; if the message comes back, report it with
both logs, which carry the expected and the written count.

Use `-P sslrootcert=/run/secrets/postgres-ca.pem` for a private CA. See
[PostgreSQL fidelity and privileges](docs/modules/POSTGRES.md).

## MongoDB backup

Omit `--database` to copy all non-system databases. The source user needs read
access and metadata visibility; the destination user needs database/user/index
administration appropriate to the plan.

A URI carries its own credentials, so pass it as `RUST_BACKUP_URI` rather than
as a flag (see the note under PostgreSQL backup).

```bash
RUST_BACKUP_URI="mongodb://backup_readonly:${MONGO_SOURCE_PASSWORD}@mongo-source.internal:27017/?authSource=admin" \
rust-backup mongodb source \
  --to coordinator.example:7835 --channel mongo-prod --secret "$RB_SECRET" \
  --database app --auth-db admin

RUST_BACKUP_URI="mongodb://root:${MONGO_DEST_PASSWORD}@mongo-destination.internal:27017/?authSource=admin" \
rust-backup mongodb destination \
  --to coordinator.example:7835 --channel mongo-prod --secret "$RB_SECRET" \
  --database app --auth-db admin --yes
```

Docker example:

```bash
docker run --rm --network host \
  -e RUST_BACKUP_URI="mongodb://backup_readonly:${MONGO_SOURCE_PASSWORD}@127.0.0.1:27017/?authSource=admin" \
  ghcr.io/manprint/rust-backup:latest \
  mongodb source --to 127.0.0.1:7835 --channel mongo-prod --secret "$RB_SECRET" --database app

docker run --rm --network host \
  -e RUST_BACKUP_URI="mongodb://root:${MONGO_DEST_PASSWORD}@127.0.0.1:37017/?authSource=admin" \
  ghcr.io/manprint/rust-backup:latest \
  mongodb destination --to 127.0.0.1:7835 --channel mongo-prod --secret "$RB_SECRET" --database app --yes
```

See [MongoDB fidelity details](docs/modules/MONGODB.md).

## S3 or MinIO backup

Versioned buckets, object-lock retention and non-portable server-side encryption
are rejected rather than silently producing a weaker copy. The source key needs
list/get and metadata/policy visibility; the destination key needs bucket/object
write and multipart permissions.

```bash
RUST_BACKUP_ACCESS_KEY="$SRC_ACCESS_KEY" RUST_BACKUP_SECRET_KEY="$SRC_SECRET_KEY" \
rust-backup s3 source \
  --to coordinator.example:7835 --channel s3-prod --secret "$RB_SECRET" \
  --region eu-south-1 --bucket source-bucket --prefix production/

RUST_BACKUP_ACCESS_KEY="$DST_ACCESS_KEY" RUST_BACKUP_SECRET_KEY="$DST_SECRET_KEY" \
rust-backup s3 destination \
  --to coordinator.example:7835 --channel s3-prod --secret "$RB_SECRET" \
  --region eu-south-1 --bucket restored-bucket --prefix recovered/ \
  -P create_bucket=true --yes
```

For MinIO or another custom endpoint, add `--endpoint` and `--path-style`:

```bash
docker run --rm --network host \
  -e RUST_BACKUP_ACCESS_KEY=minioadmin -e RUST_BACKUP_SECRET_KEY="$MINIO_PASSWORD" \
  ghcr.io/manprint/rust-backup:latest \
  s3 source --to 127.0.0.1:7835 --channel minio --secret "$RB_SECRET" \
  --endpoint http://127.0.0.1:9000 --region us-east-1 --path-style --bucket source

docker run --rm --network host \
  -e RUST_BACKUP_ACCESS_KEY=minioadmin -e RUST_BACKUP_SECRET_KEY="$MINIO_PASSWORD" \
  ghcr.io/manprint/rust-backup:latest \
  s3 destination --to 127.0.0.1:7835 --channel minio --secret "$RB_SECRET" \
  --endpoint http://127.0.0.1:9000 --region us-east-1 --path-style --bucket destination \
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

# PostgreSQL same-major and cross-major restores.
bash e2e/postgres_introspect.sh 16
bash e2e/postgres_matrix.sh 10 11 12 13 14 15 16 17 18
bash e2e/postgres_matrix.sh 10:18 12:16 14:17 16:18
# The same matrix on PostGIS images, which adds the spatial rows.
RB_PG_IMAGE_REPO=postgis/postgis bash e2e/postgres_matrix.sh 16 12:16

# MongoDB same-major and cross-major restores.
bash e2e/mongodb_matrix.sh 4 5 6 7 8
bash e2e/mongodb_matrix.sh 4:8 5:7 6:8

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
  network/disk/bandwidth tests. The real-AWS job runs only on a push (never on a
  pull request) and only when the repository variable `RUN_AWS_E2E` is `true`;
  it then needs the protected `aws-e2e` environment, the `AWS_*` secrets and the
  `RUST_BACKUP_AWS_{SOURCE,DEST}_BUCKET` variables. When it runs, it really
  runs — the job sets the script's own `RUST_BACKUP_AWS_E2E=1` opt-in.
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

Operator-facing — the usage guide in [docs/usage/](docs/usage/README.md) is the
single source of truth for running the program (Italian):

- [Usage guide index](docs/usage/README.md) —
  [server](docs/usage/01-server.md) ·
  [transport](docs/usage/02-trasporto.md) ·
  [postgres](docs/usage/03-postgres.md) ·
  [mongodb](docs/usage/04-mongodb.md) ·
  [filesystem](docs/usage/05-filesystem.md) ·
  [s3](docs/usage/06-s3.md) ·
  [YAML sessions](docs/usage/07-sessioni-yaml.md) ·
  [plan dry-run](docs/usage/08-plan.md) ·
  [Docker](docs/usage/09-docker.md) ·
  [environment variables](docs/usage/10-variabili-ambiente.md) ·
  [exit codes](docs/usage/11-codici-uscita.md)
- [USAGE.md](USAGE.md) — pointer to the pages above
- [Transport and carrier contract](docs/TRANSPORT.md)
- [Module-specific behavior](docs/modules/README.md) —
  [postgres](docs/modules/POSTGRES.md) ·
  [mongodb](docs/modules/MONGODB.md) ·
  [filesystem](docs/modules/FILESYSTEM.md) ·
  [s3](docs/modules/S3.md)
- [QA guide](docs/QA_GUIDE.md)
- [Release notes](CHANGELOG.md)
- [End-to-end harness](e2e/README.md)

Design and audit history (records, not instructions — the operator docs above
are authoritative):

- [Current status and sign-off](docs/plans/RESUME.md)
- [Plan V3 — the final roadmap](docs/plans/RUST_BACKUP_PLAN_V3.md)
- [Plan V2](docs/plans/RUST_BACKUP_PLAN_V2.md) ·
  [original plan](docs/plans/RUST_BACKUP_PLAN.md)
- [Live V1 results](docs/plans/V1_LIVE_RESULTS.md)
- Implementation audits:
  [2026-07-29](docs/plans/RUST_BACKUP_AUDIT_2026-07-29.md) ·
  [2026-09-09](docs/plans/RUST_BACKUP_AUDIT_2026-09-09.md)

Licensed under [AGPL-3.0-or-later](LICENSE).
