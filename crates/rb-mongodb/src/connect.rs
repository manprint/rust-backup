//! MongoDB connection establishment (plan Phase 3.1).
//!
//! Builds a client from either a full `--uri` or discrete host/port/credential
//! params, probes the server version (`buildInfo`), and rejects unsupported
//! majors (< [`MIN_MONGO_MAJOR`]). All source operations are reads; there is no
//! server-side read-only switch as in PostgreSQL, so I-IMMUT on the source rests
//! on (a) a read-only backup user (operator-provided, documented) and (b) the
//! source code paths issuing only `find`/`count`/`list*` (never a write).

use mongodb::bson::doc;
use mongodb::options::{ClientOptions, Credential, ServerAddress};
use mongodb::Client;

use rb_core::error::{BackupError, Phase, Result};

use crate::MongoDbParams;

/// Minimum supported MongoDB major version. The introspection/restore logic
/// relies on the 4.x+ command surface (`listCollections` options, `buildInfo`).
pub const MIN_MONGO_MAJOR: u32 = 4;

/// A live MongoDB client plus its probed server version.
pub struct MongoConnection {
    /// The connected client (connection pool managed internally by the driver).
    pub client: Client,
    /// Raw `version` string from `buildInfo` (e.g. `"6.0.8"`).
    pub server_version: String,
    /// Parsed major version (e.g. `6`).
    pub server_major: u32,
}

impl MongoConnection {
    /// Connect using `params`, probe the version, reject majors < [`MIN_MONGO_MAJOR`].
    pub async fn connect(params: &MongoDbParams) -> Result<Self> {
        let mut opts = client_options(params).await?;
        opts.app_name = Some("rust-backup".to_string());

        let client = Client::with_options(opts)
            .map_err(|e| BackupError::phase_src(Phase::Connect, "build mongodb client", e))?;

        let (server_version, server_major) = probe_version(&client, params).await?;
        if server_major < MIN_MONGO_MAJOR {
            return Err(BackupError::phase(
                Phase::Connect,
                format!(
                    "MongoDB major {server_major} (\"{server_version}\") is unsupported; \
                     minimum is {MIN_MONGO_MAJOR}"
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

/// Build [`ClientOptions`] from a full URI, or from discrete host/credential params.
async fn client_options(params: &MongoDbParams) -> Result<ClientOptions> {
    if let Some(uri) = &params.uri {
        return ClientOptions::parse(uri)
            .await
            .map_err(|e| BackupError::phase_src(Phase::Connect, "parse mongodb uri", e));
    }

    let mut opts = ClientOptions::default();
    opts.hosts = vec![ServerAddress::Tcp {
        host: params.host.clone(),
        port: Some(params.port),
    }];
    if let Some(user) = &params.user {
        let mut cred = Credential::default();
        cred.username = Some(user.clone());
        cred.password = params.password.clone();
        // Auth source defaults to "admin" on the server when unset; mirror that.
        cred.source = params.auth_db.clone();
        opts.credential = Some(cred);
    }
    Ok(opts)
}

/// Run `buildInfo` and parse the major version. Tries the params' command
/// database first (auth_db / database / "admin"); `buildInfo` is unprivileged.
async fn probe_version(client: &Client, params: &MongoDbParams) -> Result<(String, u32)> {
    let db = client.database(&params.command_db());
    let info = db
        .run_command(doc! { "buildInfo": 1 })
        .await
        .map_err(|e| BackupError::phase_src(Phase::Connect, "run buildInfo", e))?;
    let full = info
        .get_str("version")
        .map_err(|e| BackupError::phase_src(Phase::Connect, "buildInfo missing version", e))?;
    let major = parse_major(full).ok_or_else(|| {
        BackupError::phase(
            Phase::Connect,
            format!("cannot parse MongoDB version: {full:?}"),
        )
    })?;
    Ok((full.to_string(), major))
}

/// Parse the major version from a MongoDB `version` string. `"6.0.8"` → 6,
/// `"4.4.18"` → 4, `"8.0.0"` → 8. Tolerates pre-release suffixes: `"7.0.0-rc0"`
/// → 7.
pub fn parse_major(s: &str) -> Option<u32> {
    let head = s.trim().split(['.', ' ', '-']).next()?;
    let digits: String = head.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_major_modern_versions() {
        for (input, want) in [
            ("4.4.18", 4),
            ("5.0.14", 5),
            ("6.0.8", 6),
            ("7.0.0", 7),
            ("8.0.0", 8),
        ] {
            assert_eq!(parse_major(input), Some(want), "input={input}");
        }
    }

    #[test]
    fn parse_major_tolerates_suffixes() {
        assert_eq!(parse_major("7.0.0-rc0"), Some(7));
        assert_eq!(parse_major("  6.0.8  "), Some(6));
        assert_eq!(parse_major("8.0.0-alpha"), Some(8));
    }

    #[test]
    fn parse_major_rejects_garbage() {
        assert_eq!(parse_major(""), None);
        assert_eq!(parse_major("not-a-version"), None);
        assert_eq!(parse_major("vX"), None);
    }
}
