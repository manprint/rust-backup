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
use tokio_postgres::{Client, Config, NoTls};

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
    /// Connect to `database` using `params`, spawn the protocol driver, probe the
    /// server version, and reject majors below [`MIN_PG_MAJOR`]. The connection is
    /// read-only-safe: it issues only `SHOW server_version` here.
    pub async fn connect(params: &PostgresParams, database: &str) -> Result<Self> {
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

        let (client, connection) = cfg.connect(NoTls).await.map_err(|e| {
            BackupError::phase_src(
                Phase::Connect,
                format!(
                    "connect postgres {}:{}/{database}",
                    params.host, params.port
                ),
                e,
            )
        })?;

        // The connection future drives the wire protocol and must be polled for
        // the client to function; it resolves when the client is dropped.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!(%e, "postgres connection closed");
            }
        });

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

/// Reject the TLS-verifying sslmodes until a rustls connector is wired.
fn require_supported_sslmode(sslmode: &str) -> Result<()> {
    match sslmode {
        "disable" | "allow" | "prefer" => Ok(()),
        other => Err(BackupError::phase(
            Phase::Connect,
            format!(
                "sslmode=\"{other}\": TLS is not yet supported \
                 (use disable/allow/prefer); see the plan's Phase 2 TLS follow-up"
            ),
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
    fn sslmode_gate_allows_plaintext_modes() {
        for m in ["disable", "allow", "prefer"] {
            assert!(require_supported_sslmode(m).is_ok(), "mode={m}");
        }
    }

    #[test]
    fn sslmode_gate_rejects_tls_modes() {
        for m in ["require", "verify-ca", "verify-full"] {
            assert!(require_supported_sslmode(m).is_err(), "mode={m}");
        }
    }
}
