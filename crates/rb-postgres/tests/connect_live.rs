//! Live connection smoke test (plan Phase 2.1).
//!
//! Needs a real PostgreSQL server, so it runs ONLY when `RUST_BACKUP_PG_HOST` is
//! set (e.g. a docker pg); otherwise it skips cleanly instead of failing. This is
//! the honest pattern for an optional-DB test — the pure-logic coverage lives in
//! the `connect` unit tests (`parse_major`, sslmode gate), which always run.

use rb_postgres::{PgConnection, PostgresParams, MIN_PG_MAJOR};

fn params_from_env() -> Option<PostgresParams> {
    let host = std::env::var("RUST_BACKUP_PG_HOST").ok()?;
    Some(PostgresParams {
        allow_unsupported_objects: false,
        host,
        port: std::env::var("RUST_BACKUP_PG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5432),
        user: std::env::var("RUST_BACKUP_PG_USER").unwrap_or_else(|_| "postgres".into()),
        password: std::env::var("RUST_BACKUP_PG_PASSWORD").ok(),
        database: std::env::var("RUST_BACKUP_PG_DATABASE").ok(),
        sslmode: "prefer".into(),
        sslrootcert: None,
        admin: false,
        overwrite: false,
    })
}

#[tokio::test]
async fn connects_and_reports_version() {
    let Some(params) = params_from_env() else {
        eprintln!("SKIP connects_and_reports_version: set RUST_BACKUP_PG_HOST to run");
        return;
    };
    let db = params
        .database
        .clone()
        .unwrap_or_else(|| "postgres".to_string());
    let conn = PgConnection::connect(&params, &db)
        .await
        .expect("connect to live postgres");
    assert!(
        conn.server_major >= MIN_PG_MAJOR,
        "server major {} below minimum {MIN_PG_MAJOR}",
        conn.server_major
    );
    eprintln!(
        "connected: server_version={} major={}",
        conn.server_version, conn.server_major
    );
}
