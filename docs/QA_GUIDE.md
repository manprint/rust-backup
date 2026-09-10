# QA guide

Everything below is runnable from a clean checkout by someone who has never seen
this repository. Prerequisites are stated per scenario: nothing, Docker, or an
approved `sudo -n` rule.

## Build

```bash
cargo build --release --all-features
```

`--all-features` includes the direct UDP/QUIC path. `cargo build
--no-default-features` produces the relay-only binary, which is also gated in CI.

## The shape of every run

Three processes: one coordination server, and a source and a destination that
pair on the same `--channel`. The source is always read-only; the destination is
the only side that writes.

```bash
target/release/rust-backup server --bind-addr 127.0.0.1 --control-port 7839
target/release/rust-backup filesystem source      --to 127.0.0.1:7839 --channel qa-fs --root ./source
target/release/rust-backup filesystem destination --to 127.0.0.1:7839 --channel qa-fs --yes --root ./destination
```

TLS is the same server command plus `--tls-cert CERT --tls-key KEY`; clients then
use the TLS options and never combine TLS with `--insecure`. `--insecure` exists
for loopback testing only.

Start the source first: it registers as the channel's provider, and the
destination consumes it.

## One worked example per module

Credentials belong in the environment, not in argv: `/proc/<pid>/cmdline` is
world-readable for the whole run, `/proc/<pid>/environ` is not. Every module
parameter has an env var (`RUST_BACKUP_PASSWORD`, `RUST_BACKUP_URI`,
`RUST_BACKUP_SECRET_KEY`, …) — see `USAGE.md` for the full list.

**postgres** — source needs a read-only account that can read the catalogs, the
copied tables *and every sequence*; destination needs cluster administration
(`--admin`), plus `--overwrite` to replace an existing target database.

```bash
RUST_BACKUP_PASSWORD=… target/release/rust-backup postgres source \
  --to coordinator:7839 --channel qa-pg \
  --host pg-source --port 5432 --user backup_readonly --database appdb --sslmode require

RUST_BACKUP_PASSWORD=… target/release/rust-backup postgres destination \
  --to coordinator:7839 --channel qa-pg --yes --admin --overwrite \
  --host pg-destination --port 5432 --user postgres --sslmode require
```

**mongodb** — omit `--database` to copy every non-system database.

```bash
RUST_BACKUP_URI='mongodb://backup:…@mongo-source:27017/?authSource=admin' \
  target/release/rust-backup mongodb source --to coordinator:7839 --channel qa-mg --database app

RUST_BACKUP_URI='mongodb://root:…@mongo-destination:27017/?authSource=admin' \
  target/release/rust-backup mongodb destination --to coordinator:7839 --channel qa-mg --yes
```

**filesystem** — ownership and modes are reproduced only when the destination
runs as root; a non-root destination refuses the plan unless
`--no-preserve-ownership` opts out explicitly.

```bash
target/release/rust-backup filesystem source      --to coordinator:7839 --channel qa-fs --root /srv/data
sudo target/release/rust-backup filesystem destination --to coordinator:7839 --channel qa-fs --yes --root /srv/restored
```

**s3** — works against AWS S3 and MinIO; `--path-style` is the default when an
`--endpoint` is set.

```bash
RUST_BACKUP_ACCESS_KEY=… RUST_BACKUP_SECRET_KEY=… target/release/rust-backup s3 source \
  --to coordinator:7839 --channel qa-s3 --bucket source-bucket --prefix in/ --region eu-south-1

RUST_BACKUP_ACCESS_KEY=… RUST_BACKUP_SECRET_KEY=… target/release/rust-backup s3 destination \
  --to coordinator:7839 --channel qa-s3 --yes --bucket destination-bucket --prefix out/ --region eu-south-1
```

## Dry run, and many targets at once

`rust-backup plan MODULE source …` prints the plan and touches nothing else —
use it to inspect what a run would copy, and to see what a module *refuses* to
copy before committing to a window.

`rust-backup run --config targets.yml --parallel-targets 4` drives several
source/destination pairs from one YAML file; `--fail-fast` cancels the targets
that have not started yet. Ready-made templates, one per module, are in
`examples/`: `filesystem-session.yml`, `postgres-session.yml`,
`mongodb-session.yml`, `s3-session.yml`, and `session.yml` for a mixed run.
Replace `coordinator.example` and every `CHANGE_ME` before use.

## Reading the output

`--max-rate BYTES_PER_SECOND` limits aggregate source output. `--carriers N` is
valid for filesystem (up to 32); postgres, mongodb and S3 negotiate down to one.
`--carriers` is a *request*: read the count the run actually used off the line
both peers log once negotiation settles, and report the logged number rather
than the requested one.

```
negotiated data plane carriers=4 separate_data_streams=true
```

A progress line reports completed items, transferred bytes, percent and rate per
source/destination label:

```
items 3/4  14.94 MiB/16.00 MiB  (93.4%)  1019.60 KiB/s  target_label=filesystem/Destination
```

Exit `0` is not by itself the success criterion. It additionally requires both
peer logs to end with `status="verified"` at `100.0%`, matching 64-character
payload BLAKE3 values, and the respective `BACKUP VERIFIED` / `RESTORE VERIFIED`
messages. The destination proof comes from re-introspection plus a complete
read-back of the persisted payload, never from writes having returned success.

Exit codes: 0 success, 1 any other failure, 2 configuration, 3 preflight, 4 plan
rejection, 5 integrity *or* apply *or* verify, 6 source mutation, 7
transport/connect. The same table is in [USAGE.md](../USAGE.md#exit-codes).

## When a run fails

A failed run leaves nothing that can pass for a copy: the filesystem module
deletes its active partial file, postgres drops the databases the run created,
mongodb drops the collections it created, and S3 aborts every multipart upload it
started. Each of those is logged. When the destination is killed outright, or its
backend dies, cleanup is impossible by construction — the leftover is then
uncertified and a later restore refuses the dirty destination rather than merging
into it.

Attach both peer logs and the exact command lines when reporting a failure. The
first line to look at is the process's own last line: it is the error it exited
with, and it starts with its phase — `[Connect]`, `[Analyze]`, `[Validate]`,
`[Transfer]`, `[Apply]`, `[Verify]` or `[Teardown]`.

## The test matrix

No privileges, no Docker beyond MinIO:

```bash
bash e2e/full_matrix.sh
```

It runs plain and TLS relay smoke with one and four carriers, two-target
sessions, the non-backend part of the fault matrix, and MinIO. Add
`RUST_BACKUP_FULL_DB_MATRIX=1` for the Docker PostgreSQL/MongoDB matrices
(PostgreSQL 10–18 including cross-major pairs, MongoDB 4–8).

MongoDB 8 needs a host kernel older than 6.19: every published MongoDB 8 image
refuses to start above that (SERVER-121912). On a newer kernel
`e2e/mongodb_matrix.sh 8` prints a `SKIP` naming the kernel and exits 77, which
`e2e/full_matrix.sh` renders as a `SKIP` row rather than a pass or a failure —
that major has to be exercised on an older kernel or in CI. Nothing else in the
suite depends on it.

The fault matrix is also runnable on its own, and takes a group name so a failed
group can be re-run alone (Docker is needed only for the backend groups):

```bash
bash e2e/fault_matrix.sh                       # every group
bash e2e/fault_matrix.sh postgres              # just one
# groups: filesystem immutability postgres mongodb s3 protocol exitcodes
```

Run `e2e/s3_aws_test.sh` separately for the explicitly credential-gated AWS
procedure described in `docs/modules/S3.md`; without the gate it prints `SKIP`.

Privileged filesystem, real-ENOSPC, netns and bandwidth tests are opt-in:

```bash
RUST_BACKUP_PRIVILEGED=1 bash e2e/full_matrix.sh
```

They require the approved `sudo -n` setup and are never silently counted as
passes when skipped. The aggregate invokes the exact paths
`filesystem_netns_test.sh`, `filesystem_disk_full.sh`,
`transport_netns_test.sh`, and `bandwidth_netem.sh` (which also carries the
one-vs-four-carrier proof).

The current wildcard rule over `e2e/*` is suitable only for a disposable test
host: because the repository is user-writable, it is effectively an
arbitrary-root grant. A persistent or shared runner should instead use a
root-owned wrapper that validates every operation and input.
