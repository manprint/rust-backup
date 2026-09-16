//! MongoDB connection establishment (plan Phase 3.1).
//!
//! Builds a client from either a full `--uri` or discrete host/port/credential
//! params, probes the server version (`buildInfo`), and rejects unsupported
//! majors (< [`MIN_MONGO_MAJOR`]). All source operations are reads; there is no
//! server-side read-only switch as in PostgreSQL, so I-IMMUT on the source rests
//! on (a) a read-only backup user (operator-provided, documented) and (b) the
//! source code paths issuing only `find`/`count`/`list*` (never a write).

use futures_util::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::options::{
    ClientOptions, Credential, FindOptions, ReadConcern, ReadPreference, SelectionCriteria,
    ServerAddress,
};
use mongodb::results::CollectionSpecification;
use mongodb::{Client, Cursor, Database, IndexModel};

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
        Self::connect_with(params, false).await
    }

    /// `read_only` pins the read path: every operation goes to the primary with
    /// read concern `local`, so the two fingerprint audits that bracket a run
    /// read the same node under the same visibility rules. Without the pin a
    /// URI carrying `readPreference=secondary` would let replication lag look
    /// like source drift.
    pub(crate) async fn connect_with(params: &MongoDbParams, read_only: bool) -> Result<Self> {
        let mut opts = client_options(params).await?;
        opts.app_name = Some("rust-backup".to_string());
        if read_only {
            pin_read_settings(&mut opts);
        }

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

// --- the read-only client ----------------------------------------------------

/// Commands the source is allowed to run. Everything else — every write, every
/// DDL, every profiling or admin mutation — is refused before it leaves the
/// process (I-IMMUT).
///
/// Matched case-insensitively: MongoDB itself is case-sensitive about command
/// names, so a different spelling would be refused by the server anyway, and no
/// write command differs from one of these only by case.
const READ_COMMANDS: [&str; 11] = [
    "collStats",
    "listCollections",
    "listIndexes",
    "usersInfo",
    "rolesInfo",
    "dbStats",
    "buildInfo",
    "connectionStatus",
    "hello",
    "isMaster",
    "ping",
];

/// The name of the command `command` runs (its first key), if it is on the read
/// allowlist. A command with no key at all is refused too: `run_command` sends
/// the document verbatim and an empty one is never something we meant to send.
pub(crate) fn guard_read_command(command: &Document) -> Result<&str> {
    let name = command.keys().next().map(String::as_str).unwrap_or("");
    if READ_COMMANDS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
    {
        return Ok(name);
    }
    Err(BackupError::Other(anyhow::anyhow!(
        "I-IMMUT guard refused command {name} on the source"
    )))
}

/// Wrap a driver error without inventing a phase: every call site knows the
/// phase and the operation, and adds both with `BackupError::phase_src`.
fn driver_error(e: mongodb::error::Error) -> BackupError {
    BackupError::Other(anyhow::Error::new(e))
}

/// The only handle source-side code has on a MongoDB cluster.
///
/// Like PostgreSQL's `ReadOnlyClient`, two layers of I-IMMUT: the type exposes
/// no write (no `collection()` handing out a writable `Collection`, no
/// `create_collection`, no `drop`), and `run_command` — the one call that takes
/// the operation as data — checks the command name against [`READ_COMMANDS`].
#[derive(Clone)]
pub(crate) struct ReadOnlyClient {
    inner: Client,
}

impl ReadOnlyClient {
    pub(crate) fn new(inner: Client) -> Self {
        Self { inner }
    }

    pub(crate) fn database(&self, name: &str) -> ReadOnlyDatabase {
        ReadOnlyDatabase {
            inner: self.inner.database(name),
        }
    }

    pub(crate) async fn list_database_names(&self) -> Result<Vec<String>> {
        self.inner.list_database_names().await.map_err(driver_error)
    }

    /// An allowlisted command against the `admin` database. Part of the
    /// read-only surface rather than of a call site: the wrapper is the only
    /// handle source code has, so the read it does not expose is a read
    /// somebody would otherwise take from the raw driver.
    #[allow(dead_code)]
    pub(crate) async fn run_admin_command(&self, command: Document) -> Result<Document> {
        guard_read_command(&command)?;
        self.inner
            .database("admin")
            .run_command(command)
            .await
            .map_err(driver_error)
    }
}

/// One database, read-only. Collections are never handed out: every operation
/// names the collection and goes through this type.
pub(crate) struct ReadOnlyDatabase {
    inner: Database,
}

impl ReadOnlyDatabase {
    /// See the note on `run_admin_command`: part of the read-only surface.
    #[allow(dead_code)]
    pub(crate) fn name(&self) -> &str {
        self.inner.name()
    }

    /// See the note on `run_admin_command`: part of the read-only surface.
    #[allow(dead_code)]
    pub(crate) async fn list_collection_names(&self) -> Result<Vec<String>> {
        self.inner
            .list_collection_names()
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn list_collections(&self) -> Result<Vec<CollectionSpecification>> {
        self.inner
            .list_collections()
            .await
            .map_err(driver_error)?
            .try_collect()
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn find(
        &self,
        collection: &str,
        filter: Document,
        options: Option<FindOptions>,
    ) -> Result<Cursor<Document>> {
        self.inner
            .collection::<Document>(collection)
            .find(filter)
            .with_options(options)
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn count_documents(&self, collection: &str, filter: Document) -> Result<u64> {
        self.inner
            .collection::<Document>(collection)
            .count_documents(filter)
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn estimated_document_count(&self, collection: &str) -> Result<u64> {
        self.inner
            .collection::<Document>(collection)
            .estimated_document_count()
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn list_indexes(&self, collection: &str) -> Result<Vec<IndexModel>> {
        self.inner
            .collection::<Document>(collection)
            .list_indexes()
            .await
            .map_err(driver_error)?
            .try_collect()
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn run_command(&self, command: Document) -> Result<Document> {
        guard_read_command(&command)?;
        self.inner.run_command(command).await.map_err(driver_error)
    }
}

/// The `_id`-ordered read every source path uses: the source streams documents
/// in this order and the destination read-back re-runs the same query, so the
/// per-item BLAKE3 matches only if both sides agree on the order.
pub(crate) fn sort_by_id() -> FindOptions {
    FindOptions::builder().sort(doc! { "_id": 1 }).build()
}

/// A live SOURCE connection: the client cannot express a write, the read
/// preference is pinned to the primary and the read concern to `local`, so two
/// fingerprints taken around a run read the same node with the same visibility
/// rules (a secondary could lag and look like source drift).
pub(crate) struct ReadOnlyConnection {
    pub(crate) client: ReadOnlyClient,
    pub(crate) server_version: String,
    pub(crate) server_major: u32,
}

impl ReadOnlyConnection {
    pub(crate) async fn connect(params: &MongoDbParams) -> Result<Self> {
        let conn = MongoConnection::connect_with(params, true).await?;
        Ok(Self {
            client: ReadOnlyClient::new(conn.client),
            server_version: conn.server_version,
            server_major: conn.server_major,
        })
    }
}

/// Pin the read path of a source client. Overrides whatever the URI asked for:
/// a `readPreference=secondary` in an operator's URI would let replication lag
/// look like source drift between the two fingerprint audits, and a
/// `readConcern` other than `local` changes which writes are visible without
/// changing anything about immutability.
fn pin_read_settings(opts: &mut ClientOptions) {
    opts.selection_criteria = Some(SelectionCriteria::ReadPreference(ReadPreference::Primary));
    opts.read_concern = Some(ReadConcern::local());
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

    // --- the I-IMMUT command guard -------------------------------------------

    #[test]
    fn run_command_rejects_write_commands() {
        for command in [
            doc! { "drop": "c" },
            doc! { "insert": "c", "documents": [] },
            doc! { "update": "c", "updates": [] },
            doc! { "delete": "c", "deletes": [] },
            doc! { "create": "c" },
            doc! { "createIndexes": "c", "indexes": [] },
            doc! { "dropIndexes": "c", "index": "i_1" },
            doc! { "renameCollection": "d.c", "to": "d.c2" },
            doc! { "findAndModify": "c", "remove": true },
            doc! { "collMod": "c" },
            // `aggregate` is refused whatever the pipeline is, so a writing
            // stage (an out or merge stage) never gets to be a question.
            doc! { "aggregate": "c", "pipeline": [], "cursor": {} },
            doc! { "applyOps": [] },
            doc! { "setProfilingLevel": 2 },
            doc! { "dropDatabase": 1 },
            doc! { "killOp": 1 },
            doc! {},
        ] {
            let name = command.keys().next().cloned().unwrap_or_default();
            assert!(
                guard_read_command(&command).is_err(),
                "accepted command {name}"
            );
        }
    }

    #[test]
    fn run_command_accepts_each_allowlisted_command() {
        for command in [
            doc! { "collStats": "c" },
            doc! { "listCollections": 1 },
            doc! { "listIndexes": "c" },
            doc! { "usersInfo": 1 },
            doc! { "rolesInfo": 1 },
            doc! { "dbStats": 1 },
            doc! { "buildInfo": 1 },
            doc! { "connectionStatus": 1 },
            doc! { "hello": 1 },
            doc! { "isMaster": 1 },
            doc! { "ping": 1 },
            // The server spells this one lowercase on older majors.
            doc! { "ismaster": 1 },
        ] {
            let name = command.keys().next().cloned().unwrap_or_default();
            assert!(
                guard_read_command(&command).is_ok(),
                "rejected read command {name}"
            );
        }
    }

    #[test]
    fn guard_refusal_names_the_command() {
        let error = guard_read_command(&doc! { "drop": "c" }).expect_err("must refuse");
        let text = error.to_string();
        assert!(
            text.contains("I-IMMUT guard refused command drop"),
            "{text}"
        );
    }

    /// The type is the static half of I-IMMUT for MongoDB: no write method, no
    /// accessor handing out a writable `Collection`/`Database`, and the one
    /// call that takes the operation as data goes through the allowlist.
    #[test]
    fn read_only_database_exposes_no_write_methods() {
        let source = include_str!("connect.rs");
        for (marker, label) in [
            ("impl ReadOnlyDatabase {", "ReadOnlyDatabase"),
            ("impl ReadOnlyClient {", "ReadOnlyClient"),
        ] {
            let (_, rest) = source.split_once(marker).expect("the impl block");
            let block = rest.split("\n}\n").next().expect("the end of the block");
            for forbidden in [
                "insert_one",
                "insert_many",
                "update_one",
                "update_many",
                "delete_one",
                "delete_many",
                "replace_one",
                "find_one_and",
                "create_collection",
                "create_index",
                "bulk_write",
                "drop(",
                "rename(",
                "-> Database",
                "-> Collection",
                "&self.inner.inner",
            ] {
                assert!(!block.contains(forbidden), "{label} exposes {forbidden}");
            }
            // Every `run_command`-shaped method guards first.
            let commands = block.matches("command: Document").count();
            assert_eq!(
                block.matches("guard_read_command(&command)?").count(),
                commands,
                "{label} sends a command without the allowlist"
            );
        }
    }

    #[tokio::test]
    async fn read_only_options_pin_primary_and_local() {
        let params = MongoDbParams {
            uri: Some("mongodb://h:27017/?readPreference=secondary".to_string()),
            host: "h".to_string(),
            port: 27017,
            user: None,
            password: None,
            database: None,
            allow_skipped_namespaces: false,
            auth_db: None,
            overwrite: false,
        };
        let mut opts = client_options(&params).await.expect("parse uri");
        assert!(
            !matches!(
                opts.selection_criteria,
                Some(SelectionCriteria::ReadPreference(ReadPreference::Primary))
            ),
            "the fixture must start from a non-primary preference"
        );
        pin_read_settings(&mut opts);
        assert!(matches!(
            opts.selection_criteria,
            Some(SelectionCriteria::ReadPreference(ReadPreference::Primary))
        ));
        assert_eq!(opts.read_concern, Some(ReadConcern::local()));
    }
}
