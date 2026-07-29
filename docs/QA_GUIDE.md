# QA guide

Build the release binary:

```bash
cargo build --release --all-features
```

Start a plain coordination server, then run source and destination in separate
terminals. TLS uses the same server command plus `--tls-cert CERT --tls-key KEY`;
clients use the configured TLS options and never combine TLS with `--insecure`.

```bash
target/release/rust-backup server --bind-addr 127.0.0.1 --control-port 7839
target/release/rust-backup filesystem source --to 127.0.0.1:7839 --channel qa-fs --root ./source
target/release/rust-backup filesystem destination --to 127.0.0.1:7839 --channel qa-fs --yes --root ./destination
```

The module forms are identical: replace `filesystem` with `postgres`, `mongodb`,
or `s3`, and provide that module's connection parameters via `-P key=value` or
YAML. Inspect without mutation using `rust-backup plan MODULE ...`. Run several
targets from YAML with `rust-backup run --config targets.yml --parallel-targets 4`;
`--fail-fast` cancels targets not yet started.

`--max-rate BYTES_PER_SECOND` limits aggregate source output. `--carriers N` is
valid for filesystem (up to 32); postgres, mongodb and S3 negotiate down to one.
A progress line reports completed items, transferred bytes, percent and rate for
each source/destination label.

Exit codes: 2 configuration, 3 preflight, 4 plan rejection, 5 integrity/verify,
6 source mutation, 7 transport. Attach both peer logs and the command line when
reporting a failure.

Run all non-privileged coverage with:

```bash
bash e2e/full_matrix.sh
```

It runs relay smoke (one and four carriers), two-target sessions, fault bank and
MinIO. Add `RUST_BACKUP_FULL_DB_MATRIX=1` for Docker PostgreSQL/Mongo matrices.
Privileged netns/bandwidth tests deliberately remain opt-in:

```bash
RUST_BACKUP_PRIVILEGED=1 bash e2e/full_matrix.sh
```

Those tests require the approved `sudo -n` setup and are never silently counted
as passes when skipped.
