//! PostgreSQL connection establishment (plan Phase 2.1).
//!
//! Read-only source / admin destination connections plus a server-version probe
//! that rejects unsupported majors (< [`MIN_PG_MAJOR`]). The protocol driver
//! returned by `tokio-postgres` is spawned so the [`Client`] is usable; it ends
//! when the client drops.
//!
//! TLS note: only the non-TLS sslmodes (`disable`/`allow`/`prefer`) are wired in
//! this phase — `prefer`/`allow` simply connect without SSL here. The verifying
//! modes (`require`/`verify-ca`/`verify-full`) are rejected with a clear error
//! until the rustls connector lands (tracked in the plan's Phase 2 TLS follow-up).

use rb_core::error::{BackupError, Phase, Result};
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
        if read_only {
            // Startup option: every transaction in this session is read-only, so
            // an accidental write fails at the server (defensive I-IMMUT).
            options.push_str(" -c default_transaction_read_only=on");
        }
        cfg.options(&options);

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
