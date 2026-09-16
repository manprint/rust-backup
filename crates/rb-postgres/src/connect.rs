//! PostgreSQL connection establishment (plan Phase 2.1).
//!
//! Read-only source / admin destination connections plus a server-version probe
//! that rejects unsupported majors (< [`MIN_PG_MAJOR`]). The protocol driver
//! returned by `tokio-postgres` is spawned so the [`Client`] is usable; it ends
//! when the client drops.
//!
//! TLS: `disable`/`allow`/`prefer` connect without SSL. The verifying modes
//! (`require`/`verify-ca`/`verify-full`) are implemented over rustls — see
//! [`postgres_tls`] for the root store, the optional private CA (`sslrootcert`)
//! and what each mode does and does not check.

use std::collections::HashMap;
use std::time::Duration;

use rb_core::error::{BackupError, Phase, Result};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::PostgresParams;

/// Minimum supported PostgreSQL major version (logical backup relies on v10+
/// catalog shape: `pg_sequence`, single-number majors, etc.).
pub const MIN_PG_MAJOR: u32 = 10;

/// A live PostgreSQL connection plus its probed server version.
pub struct PgConnection {
    /// The query client (the protocol driver task runs in the background).
    pub client: Client,
    /// Raw `server_version` string as reported by the server (e.g. `"16.3"`).
    pub server_version: String,
    /// Parsed major version (e.g. `16`).
    pub server_major: u32,
}

impl PgConnection {
    /// Connect read-write (the destination/restore side).
    pub async fn connect(params: &PostgresParams, database: &str) -> Result<Self> {
        Self::connect_inner(params, database, false).await
    }

    /// Connect with the session forced to `default_transaction_read_only = on`, so
    /// any accidental write fails at the server. Used for every SOURCE connection
    /// (introspection, streaming, fingerprinting) to enforce I-IMMUT defensively —
    /// even a bug cannot mutate the source.
    pub async fn connect_read_only(params: &PostgresParams, database: &str) -> Result<Self> {
        Self::connect_inner(params, database, true).await
    }

    /// Connect to `database` using `params`, spawn the protocol driver, probe the
    /// server version, and reject majors below [`MIN_PG_MAJOR`].
    async fn connect_inner(
        params: &PostgresParams,
        database: &str,
        read_only: bool,
    ) -> Result<Self> {
        require_supported_sslmode(&params.sslmode)?;

        let mut cfg = Config::new();
        cfg.host(&params.host)
            .port(params.port)
            .user(&params.user)
            .dbname(database)
            // Identifies us in pg_stat_activity for DBA visibility / audit.
            .application_name("rust-backup");
        if let Some(pw) = &params.password {
            cfg.password(pw);
        }
        // Pin every GUC that changes how a value is *rendered* as text. The
        // source streams rows ordered by `ROW(cols)::text` and the destination
        // read-back re-runs that same query on the restored data, so the two
        // agree only if both sessions render identically. They did not: PG 10
        // defaults `extra_float_digits` to 0 (which prints
        // 1.0000000000000002 and 1.0000000000000004 both as `1`, tying the sort
        // key) while PG 12+ defaults to 1, and a differing `TimeZone` reorders
        // timestamps across a DST fold — reporting a byte-perfect restore as
        // corrupt. The same pins make the source fingerprint's `md5(t::text)`
        // stable.
        let mut options = String::from(
            "-c extra_float_digits=3 -c DateStyle=ISO,MDY -c IntervalStyle=postgres \
             -c TimeZone=UTC -c bytea_output=hex -c lc_monetary=C",
        );
        options.push_str(&session_timeout_options(read_only));
        cfg.options(&options);
        // A half-open peer (a NAT that forgot the flow, a host that vanished)
        // is invisible to a connection that is waiting for a reply: without
        // keepalives a source blocked in `COPY` would wait forever instead of
        // failing. The retries multiply the interval, so the socket gives up
        // about 30 s after the last probe went unanswered.
        cfg.keepalives(true)
            .keepalives_idle(Duration::from_secs(30))
            .keepalives_interval(Duration::from_secs(10))
            .keepalives_retries(3);

        let client = connect_client(&mut cfg, params, database).await?;

        let (server_version, server_major) = probe_version(&client).await?;
        if server_major < MIN_PG_MAJOR {
            return Err(BackupError::phase(
                Phase::Connect,
                format!(
                    "PostgreSQL major {server_major} (\"{server_version}\") is unsupported; \
                     minimum is {MIN_PG_MAJOR}"
                ),
            ));
        }

        Ok(Self {
            client,
            server_version,
            server_major,
        })
    }
}

/// Session guardrails appended to the rendered startup options.
///
/// Source sessions read: they must never wait on someone else's lock, so
/// `lock_timeout` bounds every read at 30 s, and an idle transaction of ours
/// must not pin a snapshot — both are failures we want reported, not hangs.
/// Statements themselves are not bounded: a `COPY` of a large table is
/// legitimately long. A destination session writes DDL and refreshes
/// materialized views, both of which legitimately take locks and time, so it
/// sets neither timeout.
fn session_timeout_options(read_only: bool) -> String {
    if read_only {
        // `default_transaction_read_only=on`: every transaction in this session
        // is read-only, so an accidental write fails at the server (defensive
        // I-IMMUT).
        [
            " -c default_transaction_read_only=on",
            " -c lock_timeout=30s",
            " -c statement_timeout=0",
            " -c idle_in_transaction_session_timeout=0",
        ]
        .concat()
    } else {
        String::from(" -c statement_timeout=0")
    }
}

/// One read-only connection per database, reused by every phase of a run.
///
/// Before this, a single-database run opened about nine connections — two for
/// each introspection, three for each of the two fingerprint audits, one to
/// stream — which a `max_connections`-constrained server counts against every
/// other client. The pool keeps the boot connection and one per database.
///
/// **Never call [`SourcePool::get`] while holding a guard from the same pool**:
/// the map is behind one mutex, so a nested call would deadlock. Every caller
/// is sequential and drops its guard before asking for the next database.
pub(crate) struct SourcePool {
    params: PostgresParams,
    conns: Mutex<HashMap<String, PgConnection>>,
}

/// A borrowed pooled connection. Held for one statement or one `COPY`.
pub(crate) type PooledConn<'a> = MappedMutexGuard<'a, PgConnection>;

impl SourcePool {
    pub(crate) fn new(params: PostgresParams) -> Self {
        Self {
            params,
            conns: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn params(&self) -> &PostgresParams {
        &self.params
    }

    /// The connection for `database`, or for the bootstrap database when
    /// `None`. A connection the driver has already closed (server restart,
    /// killed backend) is replaced once rather than handed out dead.
    pub(crate) async fn get(&self, database: Option<&str>) -> Result<PooledConn<'_>> {
        let name = database
            .map(str::to_string)
            .unwrap_or_else(|| self.params.bootstrap_database());
        let mut guard = self.conns.lock().await;
        let stale = guard
            .get(&name)
            .map(|conn| conn.client.is_closed())
            .unwrap_or(true);
        if stale {
            let conn = PgConnection::connect_read_only(&self.params, &name).await?;
            guard.insert(name.clone(), conn);
        }
        MutexGuard::try_map(guard, |map| map.get_mut(&name)).map_err(|_| {
            BackupError::phase(
                Phase::Connect,
                format!("pooled connection for database '{name}' disappeared"),
            )
        })
    }
}

async fn connect_client(
    cfg: &mut Config,
    params: &PostgresParams,
    database: &str,
) -> Result<Client> {
    let label = || {
        format!(
            "connect postgres {}:{}/{database}",
            params.host, params.port
        )
    };
    if matches!(
        params.sslmode.as_str(),
        "require" | "verify-ca" | "verify-full"
    ) {
        cfg.ssl_mode(SslMode::Require);
        let (client, connection) = cfg
            .connect(postgres_tls(params)?)
            .await
            .map_err(|e| BackupError::phase_src(Phase::Connect, label(), e))?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!(%e, "postgres connection closed");
            }
        });
        Ok(client)
    } else {
        let (client, connection) = cfg
            .connect(NoTls)
            .await
            .map_err(|e| BackupError::phase_src(Phase::Connect, label(), e))?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!(%e, "postgres connection closed");
            }
        });
        Ok(client)
    }
}

fn postgres_tls(params: &PostgresParams) -> Result<MakeRustlsConnect> {
    use tokio_rustls::rustls::pki_types::pem::PemObject;

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = &params.sslrootcert {
        let pem = std::fs::read(path).map_err(|e| {
            BackupError::phase_src(Phase::Connect, format!("read sslrootcert {path}"), e)
        })?;
        let certs = tokio_rustls::rustls::pki_types::CertificateDer::pem_slice_iter(&pem)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| BackupError::phase_src(Phase::Connect, "parse sslrootcert", e))?;
        if certs.is_empty() {
            return Err(BackupError::phase(
                Phase::Connect,
                "sslrootcert contains no certificate",
            ));
        }
        roots.add_parsable_certificates(certs);
    }
    let cfg = ClientConfig::builder_with_provider(std::sync::Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| BackupError::phase_src(Phase::Connect, "configure PostgreSQL TLS", e))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(cfg))
}

/// Validate supported modes. TLS modes use rustls and are never downgraded.
fn require_supported_sslmode(sslmode: &str) -> Result<()> {
    match sslmode {
        "disable" | "allow" | "prefer" | "require" | "verify-ca" | "verify-full" => Ok(()),
        other => Err(BackupError::phase(
            Phase::Connect,
            format!("unsupported PostgreSQL sslmode {other:?}"),
        )),
    }
}

/// Probe `server_version` (a read-only `SHOW`) and parse its major.
async fn probe_version(client: &Client) -> Result<(String, u32)> {
    let row = client
        .query_one("SHOW server_version", &[])
        .await
        .map_err(|e| BackupError::phase_src(Phase::Connect, "probe server_version", e))?;
    let full: String = row.get(0);
    let major = parse_major(&full).ok_or_else(|| {
        BackupError::phase(
            Phase::Connect,
            format!("cannot parse PostgreSQL server_version: {full:?}"),
        )
    })?;
    Ok((full, major))
}

/// Parse the major version from a `server_version` string. PostgreSQL 10+ uses a
/// single-number major: `"15.2"` → 15, `"10.23"` → 10. Tolerates pre-release and
/// distro suffixes: `"16beta1"` → 16, `"14.2 (Debian 14.2-1.pgdg110+1)"` → 14.
pub fn parse_major(s: &str) -> Option<u32> {
    let head = s.trim().split(['.', ' ']).next()?;
    let digits: String = head.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_major_modern_versions() {
        for (input, want) in [
            ("10.23", 10),
            ("12.18", 12),
            ("14.2", 14),
            ("15.2", 15),
            ("16.3", 16),
            ("17.0", 17),
            ("18", 18),
        ] {
            assert_eq!(parse_major(input), Some(want), "input={input}");
        }
    }

    #[test]
    fn parse_major_tolerates_suffixes() {
        assert_eq!(parse_major("16beta1"), Some(16));
        assert_eq!(parse_major("17rc1"), Some(17));
        assert_eq!(parse_major("14.2 (Debian 14.2-1.pgdg110+1)"), Some(14));
        assert_eq!(parse_major("  15.4  "), Some(15));
    }

    #[test]
    fn parse_major_rejects_garbage() {
        assert_eq!(parse_major(""), None);
        assert_eq!(parse_major("not-a-version"), None);
        assert_eq!(parse_major("beta"), None);
    }

    #[test]
    fn read_only_options_include_lock_and_statement_timeouts() {
        let options = session_timeout_options(true);
        assert!(
            options.contains("-c default_transaction_read_only=on"),
            "{options}"
        );
        assert!(options.contains("-c lock_timeout=30s"), "{options}");
        assert!(options.contains("-c statement_timeout=0"), "{options}");
        assert!(
            options.contains("-c idle_in_transaction_session_timeout=0"),
            "{options}"
        );
    }

    #[test]
    fn destination_options_do_not_set_lock_timeout() {
        // The destination takes locks on purpose (DDL, REFRESH): a lock timeout
        // there would abort legitimate work, and a read-only default would
        // abort all of it.
        let options = session_timeout_options(false);
        assert!(options.contains("-c statement_timeout=0"), "{options}");
        assert!(!options.contains("lock_timeout"), "{options}");
        assert!(
            !options.contains("default_transaction_read_only"),
            "{options}"
        );
    }

    /// Live check that the pool hands out one connection per database instead
    /// of opening a new one per call. Runs only against a real server.
    #[tokio::test]
    async fn pool_reuses_connection_per_database() {
        let Some(params) = live_params() else {
            eprintln!("SKIP pool_reuses_connection_per_database: set RUST_BACKUP_PG_HOST to run");
            return;
        };
        let database = params.bootstrap_database();
        let pool = SourcePool::new(params);
        let first = {
            let conn = pool.get(Some(&database)).await.expect("first connection");
            backend_pid(&conn.client).await
        };
        let second = {
            let conn = pool.get(Some(&database)).await.expect("second connection");
            backend_pid(&conn.client).await
        };
        assert_eq!(first, second, "the pool opened a second backend");
    }

    async fn backend_pid(client: &Client) -> i32 {
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("backend pid")
            .get(0)
    }

    fn live_params() -> Option<PostgresParams> {
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
            extension_version: crate::ExtensionVersionPolicy::Source,
        })
    }

    #[test]
    fn sslmode_gate_accepts_plaintext_and_tls_modes() {
        for m in [
            "disable",
            "allow",
            "prefer",
            "require",
            "verify-ca",
            "verify-full",
        ] {
            assert!(require_supported_sslmode(m).is_ok(), "mode={m}");
        }
    }

    #[test]
    fn sslmode_gate_rejects_unknown_modes() {
        for m in ["bogus", "verify-none"] {
            assert!(require_supported_sslmode(m).is_err(), "mode={m}");
        }
    }
}
