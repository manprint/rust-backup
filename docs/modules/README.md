# Modules

Each backup module is its own crate implementing `rb_core::BackupModule`. A module
supplies: a connection-params struct, a plan-payload struct (the module-specific part
of `BackupPlan.payload`), a `Source` (read-only analyze + stream out), and a
`Destination` (validate + stream in).

| Module | Crate | Versions | Driver (Phase 2+) | Status |
|--------|-------|----------|-------------------|--------|
| postgres | `rb-postgres` | 10..=latest | `tokio-postgres` (pure Rust) | stub |
| mongodb | `rb-mongodb` | 4..=8 | `mongodb` (pure Rust) | stub |
| filesystem | `rb-filesystem` | POSIX | `std` + `nix` | stub — see [FILESYSTEM.md](FILESYSTEM.md) |
| s3 | `rb-s3` | AWS S3 / MinIO | `aws-sdk-s3` | stub |

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
