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

use std::sync::atomic::{AtomicBool, Ordering};

use rb_core::error::{BackupError, Phase, Result};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};
use tokio_postgres::config::SslMode;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Config, CopyOutStream, NoTls, Row};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::PostgresParams;

/// Minimum supported PostgreSQL major version (logical backup relies on v10+
/// catalog shape: `pg_sequence`, single-number majors, etc.).
pub const MIN_PG_MAJOR: u32 = 10;

/// A live PostgreSQL connection plus its probed server version.
///
/// Read-write: this is the destination/restore side. The source side uses
/// `ReadOnlyConnection` (crate-private), whose client cannot express a write
/// at all.
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
        let (client, server_version, server_major) = connect_inner(params, database, false).await?;
        Ok(Self {
            client,
            server_version,
            server_major,
        })
    }
}

/// A live SOURCE connection: the session is forced to
/// `default_transaction_read_only = on`, so any accidental write fails at the
/// server, and the handle is a [`ReadOnlyClient`], so the write never compiles
/// in the first place (I-IMMUT).
pub(crate) struct ReadOnlyConnection {
    /// The read-only query client.
    pub(crate) client: ReadOnlyClient,
    /// Raw `server_version` string as reported by the server.
    pub(crate) server_version: String,
    /// Parsed major version.
    pub(crate) server_major: u32,
}

impl ReadOnlyConnection {
    /// Connect read-only. Used for every SOURCE connection (introspection,
    /// streaming, fingerprinting) and for the destination's own read-back.
    pub(crate) async fn connect(params: &PostgresParams, database: &str) -> Result<Self> {
        let (client, server_version, server_major) = connect_inner(params, database, true).await?;
        Ok(Self {
            client: ReadOnlyClient::new(client),
            server_version,
            server_major,
        })
    }
}

/// Connect to `database` using `params`, spawn the protocol driver, probe the
/// server version, and reject majors below [`MIN_PG_MAJOR`].
async fn connect_inner(
    params: &PostgresParams,
    database: &str,
    read_only: bool,
) -> Result<(Client, String, u32)> {
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

    Ok((client, server_version, server_major))
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

// --- the read-only client ----------------------------------------------------

/// The only handle source-side code has on a PostgreSQL session.
///
/// Two layers of I-IMMUT, one static and one dynamic:
/// * the type exposes no write method at all — no `execute`, `batch_execute`,
///   `copy_in`, `transaction`, `simple_query`, and no accessor that hands out
///   the inner [`Client`] — so a source-side write does not compile;
/// * every statement still passes [`guard_read_only`] before it reaches the
///   server, because `query` carries arbitrary SQL text and much of ours is
///   built at runtime from catalog names.
///
/// The methods mirror the `tokio_postgres::Client` ones but take the SQL as
/// `&str` (never a prepared `Statement`, which would carry text the guard never
/// saw) and return [`Result`]; the driver error is wrapped transparently so the
/// caller adds the phase and the context it knows.
pub(crate) struct ReadOnlyClient {
    inner: Client,
}

impl ReadOnlyClient {
    pub(crate) fn new(inner: Client) -> Self {
        Self { inner }
    }

    /// Whether the driver task has ended (server restart, killed backend).
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub(crate) async fn query(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>> {
        guard_read_only(sql)?;
        self.inner.query(sql, params).await.map_err(driver_error)
    }

    pub(crate) async fn query_one(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Row> {
        guard_read_only(sql)?;
        self.inner
            .query_one(sql, params)
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn query_opt(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>> {
        guard_read_only(sql)?;
        self.inner
            .query_opt(sql, params)
            .await
            .map_err(driver_error)
    }

    pub(crate) async fn copy_out(&self, sql: &str) -> Result<CopyOutStream> {
        guard_read_only(sql)?;
        self.inner.copy_out(sql).await.map_err(driver_error)
    }
}

/// Wrap a driver error without inventing a phase: every call site knows the
/// phase and the operation, and adds both with `BackupError::phase_src`.
fn driver_error(e: tokio_postgres::Error) -> BackupError {
    BackupError::Other(anyhow::Error::new(e))
}

/// Statements a source session must never send. Whole words, matched on the
/// text with literals, identifiers and comments removed.
///
/// `ANALYZE` and `SET` are denied even though neither writes user data: the
/// source session pins its GUCs at startup (see [`connect_inner`]) and never
/// needs to change one, and `ANALYZE` writes catalog statistics.
const DENY_WORDS: [&str; 32] = [
    "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "CREATE", "ALTER", "DROP", "GRANT",
    "REVOKE", "COMMENT", "REFRESH", "CLUSTER", "VACUUM", "ANALYZE", "REINDEX", "LOCK", "SET",
    "RESET", "BEGIN", "START", "COMMIT", "ROLLBACK", "PREPARE", "CALL", "DO", "LISTEN", "NOTIFY",
    "UNLISTEN", "DISCARD", "SECURITY", "IMPORT",
];

/// Functions that mutate (sequence state, large objects, advisory locks) or
/// change server/session state, even inside a plain `SELECT`. Matched as a word
/// immediately followed by `(`.
const DENY_FUNCTIONS: [&str; 20] = [
    "NEXTVAL",
    "SETVAL",
    "LASTVAL",
    "PG_ADVISORY_LOCK",
    "PG_ADVISORY_XACT_LOCK",
    "PG_TRY_ADVISORY_LOCK",
    "LO_CREATE",
    "LO_IMPORT",
    "LO_UNLINK",
    "LO_OPEN",
    "LOWRITE",
    "PG_TERMINATE_BACKEND",
    "PG_CANCEL_BACKEND",
    "PG_RELOAD_CONF",
    "PG_EXPORT_SNAPSHOT",
    "PG_CREATE_RESTORE_POINT",
    "PG_SWITCH_WAL",
    "TXID_CURRENT",
    "PG_CURRENT_XACT_ID",
    "PG_LOGICAL_EMIT_MESSAGE",
];

/// Refuse any statement that is not a read.
///
/// The check runs on the statement text with every single-quoted literal,
/// dollar-quoted literal, double-quoted identifier and comment replaced by a
/// space, so a table named `"delete"` or a literal `'DROP TABLE x'` is data,
/// not syntax. What remains must be a single statement whose head is
/// `SELECT`/`WITH`/`SHOW`/`TABLE`/`VALUES` or a `COPY … TO STDOUT` (either
/// shape this crate emits), and must contain no denied word or function.
pub(crate) fn guard_read_only(sql: &str) -> Result<()> {
    let stripped = strip_literals_and_comments(sql);
    if stripped.contains(';') {
        return Err(guard_refusal(sql));
    }
    let upper = stripped.to_uppercase();
    let upper = upper.trim();
    if !head_is_allowed(upper) {
        return Err(guard_refusal(sql));
    }
    for (word, followed_by_paren) in words(upper) {
        if DENY_WORDS.contains(&word) {
            return Err(guard_refusal(sql));
        }
        if followed_by_paren && DENY_FUNCTIONS.contains(&word) {
            return Err(guard_refusal(sql));
        }
    }
    Ok(())
}

/// The refusal carries the head of the *original* statement (not the stripped
/// one) so the operator sees what was actually sent.
fn guard_refusal(sql: &str) -> BackupError {
    let head: String = sql.trim().chars().take(120).collect();
    BackupError::Other(anyhow::anyhow!(
        "I-IMMUT guard refused a statement on the source: {head}"
    ))
}

/// Replace every literal, quoted identifier and comment with a single space.
///
/// Backslashes are not escapes: PostgreSQL has had
/// `standard_conforming_strings = on` by default since 9.1, so `'a\'` is a
/// complete literal. Treating `\'` as an escape here would swallow the rest of
/// the statement instead of ending the literal — the opposite of safe.
fn strip_literals_and_comments(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' | '"' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == c {
                        // A doubled quote is an escaped quote, not the end.
                        if chars.get(i + 1) == Some(&c) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push(' ');
            }
            '$' => match dollar_tag(&chars, i) {
                Some(tag) => {
                    i += tag.len();
                    // An unterminated dollar quote swallows the rest: there is
                    // no statement after it either way.
                    match find_from(&chars, i, &tag) {
                        Some(end) => i = end + tag.len(),
                        None => i = chars.len(),
                    }
                    out.push(' ');
                }
                // `$1` and friends are parameter placeholders.
                None => {
                    out.push(c);
                    i += 1;
                }
            },
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            '/' if next == Some('*') => {
                // PostgreSQL block comments nest.
                let mut depth = 0usize;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                out.push(' ');
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The dollar-quote tag starting at `start` (`$$`, `$body$`), or `None` when
/// the `$` is not one (a placeholder, an operator).
fn dollar_tag(chars: &[char], start: usize) -> Option<String> {
    let mut tag = String::from("$");
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' {
            tag.push('$');
            return Some(tag);
        }
        let valid = if tag.len() == 1 {
            c.is_alphabetic() || c == '_'
        } else {
            c.is_alphanumeric() || c == '_'
        };
        if !valid {
            return None;
        }
        tag.push(c);
        i += 1;
    }
    None
}

/// Index of the next occurrence of `needle` in `chars` at or after `from`.
fn find_from(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || chars.len() < needle.len() {
        return None;
    }
    (from..=chars.len() - needle.len()).find(|&i| chars[i..i + needle.len()] == needle[..])
}

/// Whether the statement head is one of the allowed shapes. `s` is the
/// stripped, uppercased, trimmed text.
fn head_is_allowed(s: &str) -> bool {
    for keyword in ["SELECT", "WITH", "SHOW", "TABLE", "VALUES"] {
        if starts_with_word(s, keyword) {
            return true;
        }
    }
    if !starts_with_word(s, "COPY") {
        return false;
    }
    let rest = s["COPY".len()..].trim_start();
    // `COPY (query) TO STDOUT` or `COPY relation [(columns)] TO STDOUT`. A
    // quoted relation name became a space, so the relation may be as little as
    // the `.` between a stripped schema and a stripped table.
    let rest = if rest.starts_with('(') {
        match skip_parenthesized(rest) {
            Some(rest) => rest,
            None => return false,
        }
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(rest.len());
        if end == 0 {
            return false;
        }
        let after = rest[end..].trim_start();
        if after.starts_with('(') {
            match skip_parenthesized(after) {
                Some(after) => after,
                None => return false,
            }
        } else {
            after
        }
    };
    let rest = rest.trim_start();
    if !starts_with_word(rest, "TO") {
        return false;
    }
    let rest = rest["TO".len()..].trim_start();
    starts_with_word(rest, "STDOUT")
}

/// `s` begins with `word` and the word ends there (SQL identifier characters).
fn starts_with_word(s: &str, word: &str) -> bool {
    s.len() >= word.len()
        && s.is_char_boundary(word.len())
        && &s[..word.len()] == word
        && !s[word.len()..].starts_with(is_word_char)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The text after the parenthesized group `s` starts with, or `None` when the
/// parentheses do not balance. Literals are already stripped, so no `(` inside
/// a string can be counted here.
fn skip_parenthesized(s: &str) -> Option<&str> {
    let mut depth = 0usize;
    for (index, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[index + 1..]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every identifier-shaped word of `s`, with a flag saying whether a `(`
/// follows it (a function call).
fn words(s: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, c) in s.char_indices() {
        if is_word_char(c) {
            start.get_or_insert(index);
            continue;
        }
        if let Some(from) = start.take() {
            out.push((&s[from..index], c == '('));
        }
    }
    if let Some(from) = start {
        out.push((&s[from..], false));
    }
    out
}

/// Does the role we connect as have any way to write the source? Answered on
/// the source's own connection, and only ever logged (§ I-IMMUT is enforced by
/// the guard and by `default_transaction_read_only`; refusing a role the
/// operator chose is not this tool's call).
const ROLE_PROBE_ATTRIBUTES: &str =
    "SELECT rolsuper OR rolcreatedb OR rolcreaterole FROM pg_roles WHERE rolname = current_user";

const ROLE_PROBE_GRANTS: &str =
    "SELECT EXISTS (SELECT 1 FROM information_schema.role_table_grants \
     WHERE grantee = current_user AND privilege_type IN ('INSERT','UPDATE','DELETE','TRUNCATE'))";

/// Warn once per run when the source role can write to the source. A probe that
/// fails (an old server, a role that cannot read `information_schema`) is a
/// missing warning, never a failed backup.
async fn warn_if_role_can_write(client: &ReadOnlyClient, user: &str) {
    let elevated = matches!(
        client.query_opt(ROLE_PROBE_ATTRIBUTES, &[]).await,
        Ok(Some(ref row)) if row.get::<_, Option<bool>>(0).unwrap_or(false)
    );
    let granted = matches!(
        client.query_opt(ROLE_PROBE_GRANTS, &[]).await,
        Ok(Some(ref row)) if row.get::<_, Option<bool>>(0).unwrap_or(false)
    );
    if elevated || granted {
        tracing::warn!(
            "source role {user} can write to the source; a read-only role is recommended, \
             see docs/IMMUTABILITY.md"
        );
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
    conns: Mutex<HashMap<String, ReadOnlyConnection>>,
    /// The role probe has run (it is about the role, not the database).
    probed: AtomicBool,
}

/// A borrowed pooled connection. Held for one statement or one `COPY`.
pub(crate) type PooledConn<'a> = MappedMutexGuard<'a, ReadOnlyConnection>;

impl SourcePool {
    pub(crate) fn new(params: PostgresParams) -> Self {
        Self {
            params,
            conns: Mutex::new(HashMap::new()),
            probed: AtomicBool::new(false),
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
            let conn = ReadOnlyConnection::connect(&self.params, &name).await?;
            // On the freshly opened connection, before it is handed out: the
            // probe is two `SELECT`s and runs once per run.
            if !self.probed.swap(true, Ordering::Relaxed) {
                warn_if_role_can_write(&conn.client, &self.params.user).await;
            }
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

    async fn backend_pid(client: &ReadOnlyClient) -> i32 {
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

    // --- the I-IMMUT statement guard -----------------------------------------

    #[test]
    fn guard_accepts_select_with_show_and_values() {
        for sql in [
            "SELECT 1",
            "SELECT count(*)::int8 FROM \"public\".\"t\"",
            "  select rolname from pg_roles order by 1  ",
            "WITH r AS (SELECT 1) SELECT * FROM r",
            "SHOW server_version",
            "TABLE pg_class",
            "VALUES (1), (2)",
            "SELECT last_value, is_called FROM \"public\".\"s\"",
        ] {
            assert!(guard_read_only(sql).is_ok(), "rejected: {sql}");
        }
    }

    #[test]
    fn guard_accepts_copy_to_stdout_table_form_and_query_form() {
        for sql in [
            // `copy_out_sql`, both shapes, with a column named `delete`.
            "COPY \"public\".\"t\" (\"id\", \"delete\") TO STDOUT (FORMAT binary)",
            "COPY \"public\".\"t\" TO STDOUT (FORMAT binary)",
            "COPY (SELECT * FROM \"ext\".\"cfg\" WHERE tenant = 'a') TO STDOUT (FORMAT binary)",
            // `table_stat`, with and without a condition and `ONLY`.
            "COPY (SELECT md5(t::text) FROM \"public\".\"t\" t) TO STDOUT",
            "COPY (SELECT md5(t::text) FROM ONLY \"public\".\"t\" t) TO STDOUT",
            "COPY (SELECT md5(t::text) FROM \"ext\".\"cfg\" t WHERE keep) TO STDOUT",
            "COPY t TO STDOUT",
        ] {
            assert!(guard_read_only(sql).is_ok(), "rejected: {sql}");
        }
    }

    #[test]
    fn guard_rejects_each_denied_statement() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET x = 1",
            "DELETE FROM t",
            "TRUNCATE t",
            "CREATE TEMP TABLE t (x int)",
            "ALTER TABLE t ADD COLUMN x int",
            "DROP TABLE t",
            "SET lock_timeout = 0",
            "BEGIN READ WRITE",
            "CALL p()",
            "DO $$ BEGIN PERFORM 1; END $$",
            "COPY t FROM STDIN",
            "SELECT nextval('s')",
            "SELECT setval('s', 1)",
            "SELECT pg_advisory_lock(1)",
            "SELECT pg_export_snapshot()",
            "WITH d AS (DELETE FROM t RETURNING 1) SELECT 1",
            "MERGE INTO t USING s ON true",
            "REFRESH MATERIALIZED VIEW v",
            "GRANT SELECT ON t TO r",
            "REVOKE SELECT ON t FROM r",
            "COMMENT ON TABLE t IS 'x'",
            "VACUUM t",
            "ANALYZE t",
            "REINDEX TABLE t",
            "CLUSTER t",
            "LOCK TABLE t",
            "RESET ALL",
            "COMMIT",
            "ROLLBACK",
            "PREPARE p AS SELECT 1",
            "START TRANSACTION",
            "LISTEN c",
            "NOTIFY c",
            "UNLISTEN c",
            "DISCARD ALL",
            "IMPORT FOREIGN SCHEMA s FROM SERVER x INTO l",
            "SELECT pg_terminate_backend(1)",
            "SELECT lo_import('/etc/passwd')",
            "SELECT txid_current()",
        ] {
            assert!(guard_read_only(sql).is_err(), "accepted: {sql}");
        }
    }

    #[test]
    fn guard_rejects_multi_statement() {
        for sql in [
            "SELECT 1; DROP TABLE t",
            "SELECT 1;",
            "COPY (SELECT 1) TO STDOUT; INSERT INTO t VALUES (1)",
        ] {
            assert!(guard_read_only(sql).is_err(), "accepted: {sql}");
        }
    }

    #[test]
    fn guard_ignores_literals_and_comments() {
        // Denied words inside a literal, an identifier or a comment are data,
        // not syntax — refusing them would break legitimate reads.
        for sql in [
            "SELECT 'DROP TABLE x' -- INSERT",
            "SELECT 'it''s a DELETE' AS s",
            "SELECT $q$ TRUNCATE t $q$",
            "SELECT 1 /* UPDATE t SET x = 1 */",
            "SELECT 1 /* nested /* ALTER */ still a comment */",
            "SELECT \"delete\" FROM \"public\".\"insert\"",
            "SELECT * FROM t WHERE name = 'GRANT' AND id = $1",
        ] {
            assert!(guard_read_only(sql).is_ok(), "rejected: {sql}");
        }
        // …and the guard still sees the syntax around them.
        assert!(guard_read_only("SELECT 'ok'; DROP TABLE x").is_err());
        assert!(guard_read_only("/* SELECT */ DROP TABLE x").is_err());
    }

    /// The type is the static half of I-IMMUT, so it is worth a test that fails
    /// when someone adds a write method or an unguarded read to it. A runtime
    /// counter cannot do this: every `ReadOnlyClient` method needs a live
    /// server, so an unguarded one would simply never be exercised here.
    #[test]
    fn read_only_client_has_no_write_methods() {
        let source = include_str!("connect.rs");
        let (_, rest) = source
            .split_once("impl ReadOnlyClient {")
            .expect("the impl block");
        let block = rest.split("\n}\n").next().expect("the end of the block");
        for forbidden in [
            "execute",
            "copy_in",
            "transaction",
            "simple_query",
            "prepare",
            "&Client",
            "&self.inner",
        ] {
            assert!(
                !block.contains(forbidden),
                "ReadOnlyClient exposes {forbidden}"
            );
        }
        // Every statement-taking method guards first.
        let taking_sql = block.matches("sql: &str").count();
        assert_eq!(taking_sql, 4, "methods taking SQL changed");
        assert_eq!(
            block.matches("guard_read_only(sql)?").count(),
            taking_sql,
            "a method takes SQL without calling the guard"
        );
    }

    #[test]
    fn role_probe_query_text_is_read_only() {
        for sql in [ROLE_PROBE_ATTRIBUTES, ROLE_PROBE_GRANTS] {
            assert!(guard_read_only(sql).is_ok(), "rejected the probe: {sql}");
        }
    }

    #[test]
    fn guard_refusal_names_the_statement_head() {
        let error = guard_read_only("DROP TABLE very_important").expect_err("must refuse");
        let text = error.to_string();
        assert!(text.contains("I-IMMUT guard refused"), "{text}");
        assert!(text.contains("DROP TABLE very_important"), "{text}");
    }
}
