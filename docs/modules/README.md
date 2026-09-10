# Modules

Each backup module is its own crate implementing `rb_core::BackupModule`. A module
supplies: a connection-params struct, a plan-payload struct (the module-specific part
of `BackupPlan.payload`), a `Source` (read-only analyze + stream out), and a
`Destination` (validate + stream in).

| Module | Crate | Versions | Driver | Status |
|--------|-------|----------|--------|--------|
| postgres | `rb-postgres` | 10+ | `tokio-postgres` (pure Rust) | complete; live matrix 10–18 + cross-major pairs green — limits in [POSTGRES.md](POSTGRES.md) |
| mongodb | `rb-mongodb` | 4..=8 | `mongodb` (pure Rust) | complete; live matrix 4–8 + cross-major pairs green — limits in [MONGODB.md](MONGODB.md) |
| filesystem | `rb-filesystem` | POSIX | `std` + `nix` | complete; privileged metadata, netns and ENOSPC e2e green — limits in [FILESYSTEM.md](FILESYSTEM.md) |
| s3 | `rb-s3` | AWS S3 / MinIO | `aws-sdk-s3` | complete; MinIO matrix green, real AWS credential-gated — limits in [S3.md](S3.md) |

## Adding a new module (I-MODULAR)

1. `cargo new --lib crates/rb-<name>`, depend on `rb-core`.
2. Define `Params` (serde) + `<Name>Plan` payload (serde).
3. `impl Source` (`analyze`, `stream_out`, `fingerprint`) — **read-only**.
4. `impl Destination` (`validate`, `stream_in`).
5. `impl BackupModule` + `pub fn module() -> Arc<dyn BackupModule>`.
6. Register it in the binary's module registry.
7. Add unit tests + an `e2e/` script. No change to `rb-core` is needed.

See `docs/plans/RUST_BACKUP_PLAN.md` for the per-module phase breakdown and the exact
catalogs/APIs each module introspects.
