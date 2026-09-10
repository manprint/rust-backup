#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
//! rust-backup — CLI entrypoint.
//!
//! Wires the module registry, transport, and session runner together. Module
//! connection params are assembled from typed common flags plus `-P key=value`
//! escapes into a JSON object that each module deserializes — so a new module
//! needs no new common flags (I-MODULAR at the CLI too).
//!
//! CONFIG PRECEDENCE (D10): CLI > env > YAML. clap resolves CLI vs env; the YAML
//! target (matched by module+role in `--config`) is the underlay merged field by
//! field in [`resolve_target`], so a flag left unset falls through to YAML and
//! then to the built-in default.

use std::sync::Arc;

use anyhow::{anyhow, Context};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rb_core::config::{Role, ServerConfig, SessionConfig, TargetSpec, TransportConfig};
use rb_core::module::{ModuleRegistry, TargetParams};
use rb_core::plan::BackupPlan;
use rb_core::progress::Progress;
use rb_core::session;

/// Value parser for every boolean flag that also reads an env var.
///
/// clap's default `bool` parser accepts only "true"/"false", so
/// `RUST_BACKUP_YES=1` — the idiom every unit file and CI job reaches for —
/// aborted with `invalid value '1' for '--yes'` instead of auto-accepting.
/// This accepts the usual truthy/falsey spellings (1/0, y/n, on/off, …).
fn boolish() -> clap::builder::BoolishValueParser {
    clap::builder::BoolishValueParser::new()
}

#[derive(Parser)]
#[command(
    name = "rust-backup",
    version,
    about = "Modular streaming source→destination backup over a bore-style tunnel"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Increase log verbosity (repeatable).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the coordination server.
    Server(ServerArgs),
    /// Run a multi-target session from a YAML config.
    Run {
        #[arg(long, env = "RUST_BACKUP_CONFIG")]
        config: String,
        #[arg(long, env = "RUST_BACKUP_PARALLEL_TARGETS")]
        parallel_targets: Option<usize>,
        #[arg(long, env = "RUST_BACKUP_FAIL_FAST", value_parser = boolish())]
        fail_fast: bool,
    },
    /// Dry-run: analyze a source and print its plan; no transport, no transfer.
    Plan(PlanArgs),
    /// PostgreSQL backup/restore (10+).
    Postgres(TargetArgs),
    /// MongoDB backup/restore (4..=8).
    Mongodb(TargetArgs),
    /// Filesystem backup/restore (POSIX; ownership/perms on Linux).
    Filesystem(TargetArgs),
    /// S3-compatible object storage backup/restore.
    S3(TargetArgs),
}

#[derive(Args)]
struct ServerArgs {
    #[arg(long, env = "RUST_BACKUP_BIND_ADDR", default_value = "0.0.0.0")]
    bind_addr: String,
    #[arg(long, env = "RUST_BACKUP_CONTROL_PORT", default_value_t = 7835)]
    control_port: u16,
    #[arg(long, env = "RUST_BACKUP_SECRET")]
    secret: Option<String>,
    /// Read the shared secret from a file (preferred to --secret).
    #[arg(long, env = "RUST_BACKUP_SECRET_FILE")]
    secret_file: Option<String>,
    /// PEM certificate enabling TLS on the control listener.
    #[arg(long, env = "RUST_BACKUP_TLS_CERT")]
    tls_cert: Option<String>,
    /// PEM private key enabling TLS on the control listener.
    #[arg(long, env = "RUST_BACKUP_TLS_KEY")]
    tls_key: Option<String>,
    #[arg(long, env = "RUST_BACKUP_MAX_CONNS", default_value_t = 256)]
    max_conns: usize,
    /// Enable the UDP/QUIC direct-path brokering.
    // Accept both `--udp` and `--udp=false`: e2e/automation must be able to
    // force relay-only server brokering without relying on an environment var.
    #[arg(
        long,
        env = "RUST_BACKUP_UDP",
        default_value_t = true,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = boolish()
    )]
    udp: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliRole {
    Source,
    Destination,
}

impl From<CliRole> for Role {
    fn from(r: CliRole) -> Role {
        match r {
            CliRole::Source => Role::Source,
            CliRole::Destination => Role::Destination,
        }
    }
}

/// Module name accepted by the `plan` dry-run subcommand.
#[derive(Clone, Copy, ValueEnum)]
enum CliModule {
    Postgres,
    Mongodb,
    Filesystem,
    S3,
}

impl CliModule {
    fn name(self) -> &'static str {
        match self {
            CliModule::Postgres => "postgres",
            CliModule::Mongodb => "mongodb",
            CliModule::Filesystem => "filesystem",
            CliModule::S3 => "s3",
        }
    }
}

/// Module connection params, shared by the transfer subcommands and `plan`.
/// Module connection params.
///
/// CREDENTIAL HANDLING: every one of these carries its documented
/// `RUST_BACKUP_<UPPER_SNAKE>` env var, because `/proc/<pid>/cmdline` is
/// world-readable on Linux while `/proc/<pid>/environ` is owner-only — a
/// password, an S3 secret key or a Mongo URI passed as a flag is visible to
/// every local account for as long as the transfer runs.
#[derive(Args, Default)]
struct ModuleParamArgs {
    #[arg(long, env = "RUST_BACKUP_HOST")]
    host: Option<String>,
    #[arg(long, env = "RUST_BACKUP_PORT")]
    port: Option<u16>,
    #[arg(long, env = "RUST_BACKUP_USER")]
    user: Option<String>,
    /// Backend password. Prefer the env var to a flag (see above).
    #[arg(long, env = "RUST_BACKUP_PASSWORD")]
    password: Option<String>,
    #[arg(long, env = "RUST_BACKUP_DATABASE")]
    database: Option<String>,
    #[arg(long, env = "RUST_BACKUP_SSLMODE")]
    sslmode: Option<String>,
    /// mongodb: full connection URI. Prefer the env var to a flag.
    #[arg(long, env = "RUST_BACKUP_URI")]
    uri: Option<String>,
    #[arg(long = "auth-db", env = "RUST_BACKUP_AUTH_DB")]
    auth_db: Option<String>,
    #[arg(long, env = "RUST_BACKUP_ROOT")]
    root: Option<String>,
    #[arg(long, env = "RUST_BACKUP_BUCKET")]
    bucket: Option<String>,
    #[arg(long, env = "RUST_BACKUP_ENDPOINT")]
    endpoint: Option<String>,
    #[arg(long, env = "RUST_BACKUP_REGION")]
    region: Option<String>,
    #[arg(long, env = "RUST_BACKUP_PREFIX")]
    prefix: Option<String>,
    #[arg(long = "access-key", env = "RUST_BACKUP_ACCESS_KEY")]
    access_key: Option<String>,
    /// s3: secret key. Prefer the env var to a flag.
    #[arg(long = "secret-key", env = "RUST_BACKUP_SECRET_KEY")]
    secret_key: Option<String>,

    // --- bool module params ---
    //
    // `Option<bool>`, not `bool`: a plain `bool` collapses "flag absent" and
    // "flag given as false" into the same value, so `overlay` could only ever
    // encode `true` and a YAML `overwrite: true` could not be turned off from
    // the CLI or the environment — which is exactly the opposite of the
    // documented CLI > env > YAML precedence. `num_args(0..=1)` +
    // `default_missing_value` keeps the bare `--overwrite` spelling working,
    // and `require_equals` keeps `--overwrite` from swallowing a positional.
    /// postgres: connect as admin (destination).
    #[arg(long, env = "RUST_BACKUP_ADMIN", value_parser = boolish(), num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    admin: Option<bool>,
    /// Destination: replace existing databases, collections or objects.
    #[arg(long, env = "RUST_BACKUP_OVERWRITE", value_parser = boolish(), num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    overwrite: Option<bool>,
    /// s3: use path-style addressing (MinIO).
    #[arg(long = "path-style", env = "RUST_BACKUP_PATH_STYLE", value_parser = boolish(), num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    path_style: Option<bool>,
    /// filesystem: follow symlinks.
    #[arg(long = "follow-symlinks", env = "RUST_BACKUP_FOLLOW_SYMLINKS", value_parser = boolish(), num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    follow_symlinks: Option<bool>,
    /// filesystem: do NOT preserve ownership (default is to preserve).
    #[arg(
        long = "no-preserve-ownership",
        env = "RUST_BACKUP_NO_PRESERVE_OWNERSHIP",
        value_parser = boolish(),
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    no_preserve_ownership: Option<bool>,
    /// filesystem: preserve xattrs.
    #[arg(long = "preserve-xattr", env = "RUST_BACKUP_PRESERVE_XATTR", value_parser = boolish(), num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    preserve_xattr: Option<bool>,

    /// Extra module params as key=value (repeatable); overrides typed flags.
    #[arg(short = 'P', long = "param")]
    param: Vec<String>,

    /// YAML config underlay (values not set on the CLI/env come from here).
    #[arg(long, env = "RUST_BACKUP_CONFIG")]
    config: Option<String>,
}

#[derive(Args)]
struct TargetArgs {
    /// Which side to run.
    role: CliRole,

    // --- transport (each optional so a YAML underlay can supply it) ---
    #[arg(long, env = "RUST_BACKUP_TO")]
    to: Option<String>,
    #[arg(long, env = "RUST_BACKUP_CHANNEL")]
    channel: Option<String>,
    #[arg(long, env = "RUST_BACKUP_SECRET")]
    secret: Option<String>,
    /// Read the transport shared secret from a file (preferred to --secret).
    #[arg(long, env = "RUST_BACKUP_SECRET_FILE")]
    secret_file: Option<String>,
    #[arg(long, env = "RUST_BACKUP_CARRIERS")]
    carriers: Option<u32>,
    /// Try the direct UDP/QUIC path (`--udp=false` forces relay-only).
    #[arg(
        long,
        env = "RUST_BACKUP_UDP",
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = boolish()
    )]
    udp: Option<bool>,
    /// Disable the direct UDP/QUIC path (relay only). Wins over `--udp`.
    #[arg(long = "no-udp")]
    no_udp: bool,
    #[arg(long, env = "RUST_BACKUP_INSECURE", value_parser = boolish())]
    insecure: bool,
    /// Aggregate payload limit in bytes/second (0/unset means unlimited).
    #[arg(long, env = "RUST_BACKUP_MAX_RATE")]
    max_rate: Option<u64>,
    /// Auto-accept the plan (destination only).
    #[arg(long = "yes", env = "RUST_BACKUP_YES", value_parser = boolish())]
    yes: bool,

    #[command(flatten)]
    module: ModuleParamArgs,
}

#[derive(Args)]
struct PlanArgs {
    /// Module to analyze.
    module: CliModule,
    #[command(flatten)]
    params: ModuleParamArgs,
}

impl ModuleParamArgs {
    /// The typed flags + `-P` escapes as a JSON object (CLI/env layer only).
    fn overlay(&self) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
        use serde_json::{Map, Value};
        let mut m = Map::new();
        let mut put_str = |k: &str, v: &Option<String>| {
            if let Some(s) = v {
                m.insert(k.into(), Value::String(s.clone()));
            }
        };
        put_str("host", &self.host);
        put_str("user", &self.user);
        put_str("password", &self.password);
        put_str("database", &self.database);
        put_str("sslmode", &self.sslmode);
        put_str("uri", &self.uri);
        put_str("auth_db", &self.auth_db);
        put_str("root", &self.root);
        put_str("bucket", &self.bucket);
        put_str("endpoint", &self.endpoint);
        put_str("region", &self.region);
        put_str("prefix", &self.prefix);
        put_str("access_key", &self.access_key);
        put_str("secret_key", &self.secret_key);
        if let Some(p) = self.port {
            m.insert("port".into(), Value::Number(p.into()));
        }
        // A bool folds whenever it was actually given, in either direction, so
        // an explicit `false` overrides a YAML `true` like every other field.
        let mut put_bool = |k: &str, v: Option<bool>| {
            if let Some(b) = v {
                m.insert(k.into(), Value::Bool(b));
            }
        };
        put_bool("admin", self.admin);
        put_bool("overwrite", self.overwrite);
        put_bool("path_style", self.path_style);
        put_bool("follow_symlinks", self.follow_symlinks);
        put_bool("preserve_xattr", self.preserve_xattr);
        // The switch is spelled in the negative; the parameter is not.
        put_bool(
            "preserve_ownership",
            self.no_preserve_ownership.map(|no| !no),
        );
        // -P key=value overrides (best-effort typed: bool/number/string).
        for kv in &self.param {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow!("-P expects key=value, got '{kv}'"))?;
            m.insert(k.to_string(), parse_scalar(v));
        }
        Ok(m)
    }

    /// The YAML target this invocation sits on top of, if any.
    fn underlay(&self, module: &str, role: Role) -> anyhow::Result<Option<TargetSpec>> {
        let Some(path) = &self.config else {
            return Ok(None);
        };
        let cfg = SessionConfig::from_path(path).map_err(anyhow::Error::new)?;
        Ok(cfg
            .targets
            .into_iter()
            .find(|t| t.module == module && t.role == role))
    }
}

/// Merge the YAML underlay params with the CLI/env overlay (overlay wins per key).
fn merge_params(
    underlay: Option<&serde_json::Value>,
    overlay: serde_json::Map<String, serde_json::Value>,
) -> TargetParams {
    use serde_json::Value;
    let mut base = match underlay {
        Some(Value::Object(o)) => o.clone(),
        _ => serde_json::Map::new(),
    };
    for (k, v) in overlay {
        base.insert(k, v);
    }
    TargetParams(Value::Object(base))
}

/// Resolve transport + params + accept policy for one CLI target.
fn resolve_target(
    a: &TargetArgs,
    module: &str,
    role: Role,
) -> anyhow::Result<(TransportConfig, TargetParams, bool)> {
    let underlay = a.module.underlay(module, role)?;
    let mut transport = underlay
        .as_ref()
        .map(|t| t.transport.clone())
        .unwrap_or_default();

    if let Some(to) = &a.to {
        transport.to = to.clone();
    }
    if let Some(channel) = &a.channel {
        transport.channel = channel.clone();
    }
    if a.secret.is_some() && a.secret_file.is_some() {
        return Err(anyhow!("--secret and --secret-file are mutually exclusive"));
    }
    if let Some(secret) = &a.secret {
        transport.secret = Some(secret.clone());
    }
    if let Some(path) = &a.secret_file {
        transport.secret = Some(read_secret_file(path)?);
    }
    if let Some(carriers) = a.carriers {
        transport.carriers = carriers;
    }
    // `--udp[=BOOL]`/`RUST_BACKUP_UDP` is the documented switch; `--no-udp` is
    // its explicit negation and wins, so a unit file can set the env var and a
    // one-off invocation can still force relay-only.
    if let Some(udp) = a.udp {
        transport.udp = udp;
    }
    if a.no_udp {
        transport.udp = false;
    }
    if a.insecure {
        transport.insecure = true;
    }
    if a.max_rate.is_some() {
        transport.max_rate = a.max_rate.filter(|rate| *rate > 0);
    }

    if transport.to.is_empty() {
        return Err(anyhow!("--to is required (or set it in --config)"));
    }
    if transport.channel.is_empty() {
        return Err(anyhow!("--channel is required (or set it in --config)"));
    }
    if !(1..=32).contains(&transport.carriers) {
        return Err(anyhow!(
            "carriers={} is outside the supported range 1..=32",
            transport.carriers
        ));
    }

    let params = merge_params(underlay.as_ref().map(|t| &t.params), a.module.overlay()?);
    let auto_accept = a.yes || underlay.as_ref().is_some_and(|t| t.auto_accept);
    Ok((transport, params, auto_accept))
}

/// Type a `-P key=value` escape.
///
/// The integer coercion exists for numeric module params (`port`), but it used
/// to swallow every digit-only string: `-P password=123456` reached the module
/// as a JSON number and failed with "invalid type: integer, expected a
/// string", and `-P database=007` was silently rewritten to `7`. So:
///
/// * a value in double quotes is always a string (the explicit escape),
/// * a bare integer is a number only when it round-trips exactly,
/// * everything else stays a string.
fn parse_scalar(v: &str) -> serde_json::Value {
    use serde_json::Value;
    if let Some(quoted) = v
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|_| v.len() >= 2)
    {
        return Value::String(quoted.to_string());
    }
    match v {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => match v.parse::<i64>() {
            // `"007".parse()` succeeds as 7 and `"+7"` as 7; neither renders
            // back to what the operator typed, so neither is a number here.
            Ok(n) if n.to_string() == v => Value::Number(n.into()),
            _ => Value::String(v.to_string()),
        },
    }
}

fn build_registry() -> ModuleRegistry {
    let mut reg = ModuleRegistry::new();
    reg.register(rb_postgres::module());
    reg.register(rb_mongodb::module());
    reg.register(rb_filesystem::module());
    reg.register(rb_s3::module());
    reg
}

#[tokio::main]
async fn main() {
    if let Err(error) = run_main().await {
        eprintln!("{error:#}");
        std::process::exit(exit_code(&error));
    }
}

async fn run_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let registry = build_registry();

    // An operator interrupt has to unwind the run, not kill the process where it
    // stands. SIGINT's default disposition terminates immediately, so no `Drop`
    // ran: the destination's active file — the one item that is genuinely
    // mid-write — was left at its final path, the length of however many bytes
    // had arrived, indistinguishable from a complete file. `select!` drops the
    // losing branch, so cancelling here runs `ActiveFile::drop` and removes it.
    //
    // A `SIGKILL` still denies every cleanup by definition; that case is covered
    // separately by refusing a dirty destination on the next run.
    tokio::select! {
        biased;
        signal = shutdown_signal() => Err(anyhow!(
            "interrupted by {signal}: the run was aborted before completion. \
             Nothing incomplete is ever certified — no VERIFIED line was printed — \
             and the destination's active item was removed. Re-run the transfer."
        )),
        result = dispatch(cli.cmd, &registry) => result,
    }
}

/// Resolve on the first `SIGINT`/`SIGTERM`, naming the one that arrived.
///
/// A failure to install either handler must not take the process down: the run
/// is still perfectly valid, it simply loses the graceful-interrupt path, so the
/// arm stays pending instead.
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(mut interrupt), Ok(mut terminate)) => tokio::select! {
            _ = interrupt.recv() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
        },
        (Ok(mut interrupt), Err(error)) => {
            tracing::warn!(%error, "cannot handle SIGTERM; only SIGINT will be graceful");
            interrupt.recv().await;
            "SIGINT"
        }
        (Err(error), Ok(mut terminate)) => {
            tracing::warn!(%error, "cannot handle SIGINT; only SIGTERM will be graceful");
            terminate.recv().await;
            "SIGTERM"
        }
        (Err(error), Err(_)) => {
            tracing::warn!(%error, "cannot install signal handlers; an interrupt will kill the \
                 process outright and the destination's active item will be left behind");
            std::future::pending().await
        }
    }
}

async fn dispatch(cmd: Cmd, registry: &ModuleRegistry) -> anyhow::Result<()> {
    match cmd {
        Cmd::Server(a) => {
            if a.secret.is_some() && a.secret_file.is_some() {
                return Err(anyhow!("--secret and --secret-file are mutually exclusive"));
            }
            let server_secret = if let Some(secret) = a.secret {
                Some(secret)
            } else if let Some(path) = a.secret_file {
                Some(read_secret_file(&path)?)
            } else {
                None
            };
            let cfg = ServerConfig {
                bind_addr: a.bind_addr,
                control_port: a.control_port,
                secret: server_secret,
                tls_cert: a.tls_cert,
                tls_key: a.tls_key,
                max_conns: a.max_conns,
                udp: a.udp,
            };
            rb_transport::run_server(&cfg)
                .await
                .context("coordination server")?;
            Ok(())
        }
        Cmd::Run {
            config,
            parallel_targets,
            fail_fast,
        } => run_session(registry, &config, parallel_targets, fail_fast).await,
        Cmd::Plan(a) => print_plan(registry, &a).await,
        Cmd::Postgres(a) => run_target(registry, "postgres", &a).await,
        Cmd::Mongodb(a) => run_target(registry, "mongodb", &a).await,
        Cmd::Filesystem(a) => run_target(registry, "filesystem", &a).await,
        Cmd::S3(a) => run_target(registry, "s3", &a).await,
    }
}

/// Stable shell-facing error codes. Keep this independent from stderr wording.
fn exit_code(error: &anyhow::Error) -> i32 {
    if let Some(typed) = error.downcast_ref::<rb_core::BackupError>() {
        return exit_code_backup(typed);
    }
    // Fallback only for errors that genuinely originate outside rb-core (clap,
    // filesystem config reads, or a transport library before it is phase-wrapped).
    // Only the phase tags `BackupError::Phase` really renders (`[{phase:?}]`)
    // plus the non-rb-core wordings are matched here. The old list also tested
    // for `[Config]`, `[Preflight]`, `[PlanRejected]` and `[SourceMutated]`,
    // none of which any `Display` impl can produce — those variants render as
    // "configuration error:", "preflight failed:", "plan rejected:" and
    // "SOURCE-IMMUTABILITY VIOLATION:", and all of them reach the typed path
    // above anyway.
    let text = format!("{error:#}");
    if text.contains("unknown module") || text.contains("--to is required") {
        2
    } else if text.contains("[Verify]") || text.contains("[Apply]") {
        5
    } else if text.contains("[Connect]") || text.contains("transport:") {
        7
    } else {
        1
    }
}

fn exit_code_backup(error: &rb_core::BackupError) -> i32 {
    use rb_core::BackupError::{Config, Integrity, Phase, PlanRejected, Preflight, SourceMutated};
    match error {
        Config(_) => 2,
        Preflight(_) => 3,
        PlanRejected(_) => 4,
        Integrity(_)
        | Phase {
            phase: rb_core::Phase::Apply | rb_core::Phase::Verify,
            ..
        } => 5,
        SourceMutated(_) => 6,
        Phase {
            phase: rb_core::Phase::Connect,
            ..
        } => 7,
        _ => 1,
    }
}

fn read_secret_file(path: &str) -> anyhow::Result<String> {
    let value = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read secret file {path}"))?;
    let value = value.trim_end_matches(['\r', '\n']).to_string();
    if value.is_empty() {
        return Err(anyhow!("secret file {path} is empty"));
    }
    Ok(value)
}

/// Run one target from CLI args.
async fn run_target(reg: &ModuleRegistry, module: &str, a: &TargetArgs) -> anyhow::Result<()> {
    let m = reg
        .get(module)
        .ok_or_else(|| anyhow!("unknown module '{module}'"))?;
    let role: Role = a.role.into();
    let (transport, params, auto_accept) = resolve_target(a, module, role)?;
    execute(&m, role, &transport, &params, auto_accept).await
}

/// `plan` dry-run: open the source read-only, analyze, print the plan. No
/// transport is established and nothing is transferred; the source is only read.
async fn print_plan(reg: &ModuleRegistry, a: &PlanArgs) -> anyhow::Result<()> {
    let module = a.module.name();
    let m = reg
        .get(module)
        .ok_or_else(|| anyhow!("unknown module '{module}'"))?;
    let underlay = a.params.underlay(module, Role::Source)?;
    let params = merge_params(underlay.as_ref().map(|t| &t.params), a.params.overlay()?);
    let src = m.open_source(&params).await.map_err(anyhow::Error::new)?;
    let plan = src.analyze().await.map_err(anyhow::Error::new)?;
    println!("{}", plan.render());
    Ok(())
}

/// Run a multi-target session from YAML.
async fn run_session(
    reg: &ModuleRegistry,
    path: &str,
    requested_parallel: Option<usize>,
    requested_fail_fast: bool,
) -> anyhow::Result<()> {
    let mut cfg = SessionConfig::from_path(path).map_err(anyhow::Error::new)?;
    if cfg.server.is_some() {
        return Err(anyhow!(
            "`run --config` does not start `server:`; start `rust-backup server` separately"
        ));
    }
    if let Some(parallel) = requested_parallel {
        cfg.parallel_targets = parallel.max(1);
    }
    if requested_fail_fast {
        cfg.fail_fast = true;
    }
    if cfg.parallel_targets <= 1 {
        return run_session_sequential(reg, &cfg).await;
    }
    run_session_parallel(reg, &cfg).await
}

async fn run_session_sequential(reg: &ModuleRegistry, cfg: &SessionConfig) -> anyhow::Result<()> {
    for (i, t) in cfg.targets.iter().enumerate() {
        tracing::info!(target = i, module = %t.module, role = ?t.role, "session target");
        let m = reg
            .get(&t.module)
            .ok_or_else(|| anyhow!("unknown module '{}'", t.module))?;
        let params = TargetParams(t.params.clone());
        execute(&m, t.role, &t.transport, &params, t.auto_accept)
            .await
            .with_context(|| format!("target {i} ({}/{:?})", t.module, t.role))?;
    }
    Ok(())
}

async fn run_session_parallel(reg: &ModuleRegistry, cfg: &SessionConfig) -> anyhow::Result<()> {
    use tokio::task::JoinSet;
    let mut pending = cfg.targets.iter().cloned().enumerate();
    let mut running = JoinSet::new();
    let mut errors = Vec::new();
    loop {
        while running.len() < cfg.parallel_targets {
            let Some((i, target)) = pending.next() else {
                break;
            };
            let module = reg
                .get(&target.module)
                .ok_or_else(|| anyhow!("unknown module '{}'", target.module))?;
            running.spawn(async move {
                let params = TargetParams(target.params.clone());
                tracing::info!(target = i, module = %target.module, role = ?target.role, "session target");
                execute(&module, target.role, &target.transport, &params, target.auto_accept).await
                    .with_context(|| format!("target {i} ({}/{:?})", target.module, target.role))
            });
        }
        let Some(result) = running.join_next().await else {
            break;
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                errors.push(error);
                if cfg.fail_fast {
                    running.abort_all();
                    break;
                }
            }
            Err(error) => {
                errors.push(anyhow!("target task failed: {error}"));
                if cfg.fail_fast {
                    running.abort_all();
                    break;
                }
            }
        }
    }
    while let Some(result) = running.join_next().await {
        if let Ok(Err(error)) = result {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        return Ok(());
    }
    // Preserve a TYPED error. Collapsing the failures into a formatted string
    // lost both the exit-code classification (`downcast_ref::<BackupError>`
    // survives `context`, a fresh `anyhow!` does not) and every cause below the
    // outermost layer, so an immutability violation in a parallel session
    // reported exit 1 and no reason at all.
    let total = errors.len();
    let worst = most_severe(errors);
    if total == 1 {
        return Err(worst);
    }
    let detail = format!("{worst:#}");
    Err(worst.context(format!("{total} target(s) failed; most severe: {detail}")))
}

/// The failure a session should be judged by: the one whose exit code is the
/// most severe, ties broken by arrival order.
fn most_severe(errors: Vec<anyhow::Error>) -> anyhow::Error {
    let mut ranked: Vec<(usize, anyhow::Error)> = errors
        .into_iter()
        .map(|error| (severity_rank(exit_code(&error)), error))
        .collect();
    ranked.sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
    ranked
        .into_iter()
        .next()
        .map(|(_, error)| error)
        .unwrap_or_else(|| anyhow!("session target failed"))
}

/// Ordering over the shell-facing exit codes, worst first. Source mutation
/// outranks everything: it is the invariant the tool exists to protect.
fn severity_rank(code: i32) -> usize {
    match code {
        6 => 7, // source mutated
        5 => 6, // apply/verify/integrity
        3 => 5, // preflight
        4 => 4, // plan rejected
        7 => 3, // connect
        2 => 2, // config
        _ => 1,
    }
}

/// How often a running transfer logs its progress line (I-OBSERV).
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Periodic progress reporter (phase-agnostic: it renders whatever counters the
/// session has published). Aborted with its guard when the run ends, so a
/// finished run never keeps logging.
struct ProgressReporter {
    task: tokio::task::JoinHandle<()>,
    progress: Progress,
    label: String,
    started: std::time::Instant,
}

impl ProgressReporter {
    fn spawn(progress: Progress, label: String) -> Self {
        let started = std::time::Instant::now();
        // Emit the initial snapshot synchronously. A very small transfer can
        // otherwise finish and drop the reporter before its spawned task is
        // first scheduled, leaving that target with no progress record.
        tracing::info!(
            target_label = %label,
            "{}",
            progress.line(started.elapsed().as_secs_f64())
        );
        let task_progress = progress.clone();
        let task_label = label.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + PROGRESS_INTERVAL,
                PROGRESS_INTERVAL,
            );
            loop {
                tick.tick().await;
                tracing::info!(
                    target_label = %task_label,
                    "{}",
                    task_progress.line(started.elapsed().as_secs_f64())
                );
            }
        });
        ProgressReporter {
            task,
            progress,
            label,
            started,
        }
    }

    fn finish(&mut self, succeeded: bool) {
        self.task.abort();
        if succeeded {
            self.progress.complete();
        }
        tracing::info!(
            target_label = %self.label,
            status = if succeeded { "verified" } else { "failed" },
            "{}",
            self.progress.line(self.started.elapsed().as_secs_f64())
        );
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Connect the transport for `role` and drive the session.
/// Reduce `transport.carriers` to the module's declared maximum, logging once
/// when that changes the requested value.
fn clamp_carriers(
    module: &Arc<dyn rb_core::BackupModule>,
    transport: &TransportConfig,
) -> TransportConfig {
    let capped = transport.carriers.min(module.max_carriers()).max(1);
    if capped != transport.carriers {
        tracing::info!(
            module = module.name(),
            requested = transport.carriers,
            carriers = capped,
            "module supports fewer data carriers than requested"
        );
    }
    let mut transport = transport.clone();
    transport.carriers = capped;
    transport
}

async fn execute(
    module: &Arc<dyn rb_core::BackupModule>,
    role: Role,
    transport: &TransportConfig,
    params: &TargetParams,
    auto_accept: bool,
) -> anyhow::Result<()> {
    let progress = Progress::default();
    // Clamp the requested carrier count by what THIS module can restore before
    // the transport opens anything. The destination also enforces its own cap
    // during negotiation, but a module's declared capability must be honoured
    // on the side that owns the module handle instead of being dead metadata.
    let transport = &clamp_carriers(module, transport);
    let mut reporter =
        ProgressReporter::spawn(progress.clone(), format!("{}/{:?}", module.name(), role));
    let result: anyhow::Result<()> = async {
        match role {
            Role::Source => {
                let src = module
                    .open_source(params)
                    .await
                    .map_err(anyhow::Error::new)?;
                let ch = rb_transport::connect_source(transport).await.map_err(|e| {
                    anyhow::Error::new(rb_core::BackupError::phase_src(
                        rb_core::Phase::Connect,
                        "connect source transport",
                        e,
                    ))
                })?;
                session::run_source_limited(&*src, &ch, &progress, transport.max_rate)
                    .await
                    .map_err(anyhow::Error::new)?;
            }
            Role::Destination => {
                let dst = module
                    .open_destination(params)
                    .await
                    .map_err(anyhow::Error::new)?;
                let ch = rb_transport::connect_destination(transport)
                    .await
                    .map_err(|e| {
                        anyhow::Error::new(rb_core::BackupError::phase_src(
                            rb_core::Phase::Connect,
                            "connect destination transport",
                            e,
                        ))
                    })?;
                let mut accept = move |plan: &BackupPlan| -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = bool> + Send>,
                > {
                    if auto_accept {
                        return Box::pin(async { true });
                    }
                    let rendered = plan.render();
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || prompt_yes_rendered(&rendered))
                            .await
                            .unwrap_or(false)
                    })
                };
                session::run_destination_with_accept(&*dst, &ch, &progress, &mut accept)
                    .await
                    .map_err(anyhow::Error::new)?;
            }
        }
        Ok(())
    }
    .await;
    reporter.finish(result.is_ok());
    result
}

/// Interactive async-accept: show the plan and read `yes`/`no` from stdin.
fn prompt_yes_rendered(rendered: &str) -> bool {
    use std::io::Write;
    print!("\n{rendered}\nProceed with restore? [yes/no] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| format!("rust_backup={level},rb_core={level},rb_transport={level}"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// `plan` and the config loader used to rebuild a module error as a plain
    /// string (`anyhow!("{e}")`), which threw the typed `BackupError` away: a
    /// missing credential exited 1 instead of the documented 2, because the
    /// text fallback has no case for "configuration error:". The wrapping must
    /// survive a `context` layer too, which is how the real call sites use it.
    #[test]
    fn wrapping_a_module_error_keeps_its_documented_exit_code() {
        use rb_core::BackupError;
        let cases: Vec<(BackupError, i32)> = vec![
            (BackupError::Config("secret key missing".into()), 2),
            (BackupError::Preflight("destination not writable".into()), 3),
            (BackupError::PlanRejected("operator declined".into()), 4),
            (BackupError::SourceMutated("fingerprint changed".into()), 6),
        ];
        for (error, want) in cases {
            let rendered = error.to_string();
            let wrapped = anyhow::Error::new(error).context("analyze the source");
            assert_eq!(
                exit_code(&wrapped),
                want,
                "wrapped {rendered:?} must keep exit code {want}"
            );
        }
    }

    /// The shell contract is classified from the TYPED error, over the errors
    /// the program can really produce — not from literals no `Display` impl
    /// emits. Each case is built the way the corresponding failure builds it.
    #[test]
    fn exit_code_maps_real_error_classes() {
        use rb_core::BackupError;
        let cases: Vec<(anyhow::Error, i32)> = vec![
            (
                anyhow::Error::new(BackupError::Config("bad params".into())),
                2,
            ),
            (
                anyhow::Error::new(BackupError::Preflight("no space".into())),
                3,
            ),
            (
                anyhow::Error::new(BackupError::PlanRejected("rejected by operator".into())),
                4,
            ),
            (
                anyhow::Error::new(BackupError::phase(rb_core::Phase::Apply, "restore failed")),
                5,
            ),
            (
                anyhow::Error::new(BackupError::Integrity("digest mismatch".into())),
                5,
            ),
            (
                anyhow::Error::new(BackupError::SourceMutated("fingerprint changed".into())),
                6,
            ),
            (
                anyhow::Error::new(BackupError::phase(rb_core::Phase::Connect, "no route")),
                7,
            ),
            (anyhow!("unknown module 'nope'"), 2),
            (anyhow!("--to is required (or set it in --config)"), 2),
        ];
        for (error, code) in cases {
            assert_eq!(exit_code(&error), code, "for {error:#}");
            // Wrapping in session/target context must not change the class.
            let wrapped = error.context("target 3 (s3/Source)");
            assert_eq!(exit_code(&wrapped), code, "wrapped: {wrapped:#}");
        }
    }

    /// A parallel session must be judged by its most severe failure and must
    /// keep that failure's type, so the shell still sees the right code and the
    /// operator still sees the reason.
    #[test]
    fn parallel_session_reports_the_most_severe_typed_failure() {
        use rb_core::BackupError;
        let errors = vec![
            anyhow::Error::new(BackupError::phase(rb_core::Phase::Connect, "no route"))
                .context("target 1 (filesystem/Destination)"),
            anyhow::Error::new(BackupError::SourceMutated(
                "source fingerprint changed during backup".into(),
            ))
            .context("target 0 (s3/Source)"),
        ];
        let worst = most_severe(errors);
        assert_eq!(exit_code(&worst), 6);
        let rendered = format!("{worst:#}");
        assert!(
            rendered.contains("SOURCE-IMMUTABILITY VIOLATION"),
            "the cause must survive: {rendered}"
        );
        assert!(rendered.contains("target 0"), "{rendered}");
    }

    /// A module that can only restore one carrier must not be handed four.
    #[test]
    fn requested_carriers_are_clamped_to_the_module_capability() {
        let mut transport = rb_core::config::TransportConfig {
            to: "coord:7835".into(),
            channel: "ch".into(),
            secret: None,
            carriers: 4,
            udp: false,
            insecure: false,
            max_rate: None,
        };
        let postgres = rb_postgres::module();
        assert_eq!(postgres.max_carriers(), 1);
        assert_eq!(clamp_carriers(&postgres, &transport).carriers, 1);

        let filesystem = rb_filesystem::module();
        assert!(filesystem.max_carriers() >= 4);
        assert_eq!(clamp_carriers(&filesystem, &transport).carriers, 4);

        transport.carriers = 0;
        assert_eq!(clamp_carriers(&filesystem, &transport).carriers, 1);
    }

    #[test]
    fn typed_exit_code_ignores_error_message_wording() {
        for message in ["first wording", "a completely different wording"] {
            let error =
                anyhow::Error::new(rb_core::BackupError::phase(rb_core::Phase::Verify, message));
            assert_eq!(exit_code(&error), 5);
        }
    }

    fn write_yaml(body: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rust-backup-cli-test-{}-{:?}.yml",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, body).expect("write test yaml");
        path
    }

    const YAML: &str = r#"
targets:
  - module: postgres
    role: source
    transport: { to: yaml-coord:7835, channel: yaml-ch, secret: yaml-secret, carriers: 1, udp: true }
    params: { host: yaml-db, user: yaml-ro, port: 5433 }
  - module: postgres
    role: destination
    transport: { to: yaml-coord:7835, channel: yaml-ch }
    params: { host: yaml-db-b, admin: true }
    auto_accept: true
"#;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("cli parses")
    }

    #[test]
    fn server_udp_accepts_bare_and_explicit_boolean_values() {
        let Cmd::Server(defaults) = parse(&["rust-backup", "server"]).cmd else {
            panic!("expected server subcommand")
        };
        assert!(defaults.udp);

        let Cmd::Server(enabled) = parse(&["rust-backup", "server", "--udp"]).cmd else {
            panic!("expected server subcommand")
        };
        assert!(enabled.udp);

        let Cmd::Server(disabled) = parse(&["rust-backup", "server", "--udp=false"]).cmd else {
            panic!("expected server subcommand")
        };
        assert!(!disabled.udp);
    }

    #[tokio::test]
    async fn run_config_rejects_ignored_server_section() {
        let yaml =
            write_yaml("server: { bind_addr: 127.0.0.1, control_port: 7835 }\ntargets: []\n");
        let error = run_session(
            &build_registry(),
            yaml.to_str().expect("utf8 path"),
            None,
            false,
        )
        .await
        .expect_err("server section must not be silently ignored");
        assert!(error.to_string().contains("does not start `server:`"));
        std::fs::remove_file(yaml).ok();
    }

    /// The YAML destination target sets `admin: true`. A `bool` field could only
    /// ever encode `true` in the overlay, so an explicit `--admin=false` (or
    /// `RUST_BACKUP_ADMIN=0`) silently lost to the YAML — the precedence
    /// documented in USAGE.md held for every field except the booleans, which
    /// are the ones that destroy data (`overwrite`) or escalate privilege
    /// (`admin`).
    #[test]
    fn an_explicit_false_overrides_a_yaml_true() {
        let yaml = write_yaml(YAML);
        let path = yaml.to_str().expect("utf8 path").to_string();

        let cli = parse(&[
            "rust-backup",
            "postgres",
            "destination",
            "--admin=false",
            "--config",
            &path,
        ]);
        let Cmd::Postgres(explicit) = cli.cmd else {
            panic!("expected postgres subcommand")
        };
        let (_, params, _) =
            resolve_target(&explicit, "postgres", Role::Destination).expect("resolve");
        assert_eq!(
            params.0["admin"], false,
            "an explicit --admin=false must beat the YAML target's admin: true"
        );

        // The other two directions still behave: unset falls through to YAML,
        // and the bare flag still means true.
        let cli = parse(&["rust-backup", "postgres", "destination", "--config", &path]);
        let Cmd::Postgres(unset) = cli.cmd else {
            panic!("expected postgres subcommand")
        };
        let (_, params, _) =
            resolve_target(&unset, "postgres", Role::Destination).expect("resolve");
        assert_eq!(params.0["admin"], true);

        let cli = parse(&["rust-backup", "postgres", "destination", "--overwrite"]);
        let Cmd::Postgres(bare) = cli.cmd else {
            panic!("expected postgres subcommand")
        };
        assert_eq!(bare.module.overwrite, Some(true), "a bare flag means true");

        std::fs::remove_file(yaml).ok();
    }

    /// D10: a flag left unset falls through to the YAML target; a flag that IS
    /// set overrides it, per field.
    #[test]
    fn cli_overrides_yaml_per_field() {
        let yaml = write_yaml(YAML);
        let cli = parse(&[
            "rust-backup",
            "postgres",
            "source",
            "--channel",
            "cli-ch",
            "--host",
            "cli-db",
            "--config",
            yaml.to_str().expect("utf8 path"),
        ]);
        let Cmd::Postgres(a) = cli.cmd else {
            panic!("expected postgres subcommand")
        };
        let (transport, params, auto_accept) =
            resolve_target(&a, "postgres", Role::Source).expect("resolve");
        // From YAML (not set on the CLI):
        assert_eq!(transport.to, "yaml-coord:7835");
        assert_eq!(transport.secret.as_deref(), Some("yaml-secret"));
        assert_eq!(params.0["user"], "yaml-ro");
        assert_eq!(params.0["port"], 5433);
        // Overridden on the CLI:
        assert_eq!(transport.channel, "cli-ch");
        assert_eq!(params.0["host"], "cli-db");
        assert!(!auto_accept);
        std::fs::remove_file(yaml).ok();
    }

    /// The underlay is selected by module AND role: the destination row supplies
    /// its own params and `auto_accept`.
    #[test]
    fn underlay_is_matched_by_role() {
        let yaml = write_yaml(YAML);
        let cli = parse(&[
            "rust-backup",
            "postgres",
            "destination",
            "--config",
            yaml.to_str().expect("utf8 path"),
        ]);
        let Cmd::Postgres(a) = cli.cmd else {
            panic!("expected postgres subcommand")
        };
        let (transport, params, auto_accept) =
            resolve_target(&a, "postgres", Role::Destination).expect("resolve");
        assert_eq!(transport.channel, "yaml-ch");
        assert_eq!(params.0["host"], "yaml-db-b");
        assert_eq!(params.0["admin"], true);
        assert!(auto_accept, "auto_accept comes from the YAML target");
        std::fs::remove_file(yaml).ok();
    }

    /// Without a YAML underlay the transport essentials are mandatory.
    #[test]
    fn missing_transport_essentials_are_rejected() {
        let cli = parse(&["rust-backup", "filesystem", "source", "--root", "/data"]);
        let Cmd::Filesystem(a) = cli.cmd else {
            panic!("expected filesystem subcommand")
        };
        let err = resolve_target(&a, "filesystem", Role::Source).expect_err("must fail");
        assert!(err.to_string().contains("--to is required"), "{err}");
    }

    /// Multi-carrier is accepted through the negotiated safety cap, and invalid
    /// counts fail before a session starts.
    #[test]
    fn multi_carrier_range_is_enforced() {
        let cli = parse(&[
            "rust-backup",
            "filesystem",
            "source",
            "--to",
            "coord:7835",
            "--channel",
            "c1",
            "--carriers",
            "4",
        ]);
        let Cmd::Filesystem(a) = cli.cmd else {
            panic!("expected filesystem subcommand")
        };
        let (transport, _, _) = resolve_target(&a, "filesystem", Role::Source).expect("4 works");
        assert_eq!(transport.carriers, 4);

        let cli = parse(&[
            "rust-backup",
            "filesystem",
            "source",
            "--to",
            "coord:7835",
            "--channel",
            "c1",
            "--carriers",
            "33",
        ]);
        let Cmd::Filesystem(a) = cli.cmd else {
            panic!("expected filesystem subcommand")
        };
        let err = resolve_target(&a, "filesystem", Role::Source).expect_err("must fail");
        assert!(err.to_string().contains("1..=32"), "{err}");
    }

    /// `-P key=value` beats the typed flag and keeps scalar typing.
    #[test]
    fn param_escape_overrides_typed_flag() {
        let cli = parse(&[
            "rust-backup",
            "postgres",
            "source",
            "--to",
            "coord:7835",
            "--channel",
            "c1",
            "--host",
            "flag-host",
            "-P",
            "host=escape-host",
            "-P",
            "port=6000",
            "-P",
            "overwrite=true",
        ]);
        let Cmd::Postgres(a) = cli.cmd else {
            panic!("expected postgres subcommand")
        };
        let (_, params, _) = resolve_target(&a, "postgres", Role::Source).expect("resolve");
        assert_eq!(params.0["host"], "escape-host");
        assert_eq!(params.0["port"], 6000);
        assert_eq!(params.0["overwrite"], true);
    }

    #[test]
    fn overwrite_flag_is_accepted_after_destination_options() {
        let cli = parse(&[
            "rust-backup",
            "postgres",
            "destination",
            "--to",
            "coord:7835",
            "--channel",
            "c1",
            "--host",
            "db",
            "--admin",
            "--yes",
            "--overwrite",
        ]);
        let Cmd::Postgres(a) = cli.cmd else {
            panic!("expected postgres subcommand")
        };
        let (_, params, auto_accept) =
            resolve_target(&a, "postgres", Role::Destination).expect("resolve");
        assert_eq!(params.0["overwrite"], true);
        assert_eq!(params.0["admin"], true);
        assert!(auto_accept);
    }

    /// The `plan` dry-run subcommand parses and carries module params only.
    #[test]
    fn plan_subcommand_parses() {
        let cli = parse(&["rust-backup", "plan", "filesystem", "--root", "/srv/data"]);
        let Cmd::Plan(a) = cli.cmd else {
            panic!("expected plan subcommand")
        };
        assert_eq!(a.module.name(), "filesystem");
        let overlay = a.params.overlay().expect("overlay");
        assert_eq!(overlay["root"], "/srv/data");
    }

    /// USAGE.md promises `RUST_BACKUP_<UPPER_SNAKE>` for every flag, and a unit
    /// file that sets one must actually get it. `RUST_BACKUP_YES` silently did
    /// nothing, so an unattended destination sat on the interactive prompt
    /// forever, and every credential flag had to travel through a
    /// world-readable `/proc/<pid>/cmdline`.
    ///
    /// Documented exceptions: `-P/--param` is repeatable, `--no-udp` is the
    /// negation of `--udp`, and `-v/--verbose` is listed with no env var.
    #[test]
    fn every_target_flag_carries_its_documented_env_var() {
        use clap::CommandFactory;
        const EXCEPTIONS: &[&str] = &["param", "no-udp", "verbose", "help", "version"];
        let command = Cli::command();
        let mut checked = 0;
        for name in ["postgres", "mongodb", "filesystem", "s3", "server", "run"] {
            let sub = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("{name} subcommand"));
            for arg in sub.get_arguments() {
                let Some(long) = arg.get_long() else { continue };
                if EXCEPTIONS.contains(&long) {
                    continue;
                }
                let expected = format!("RUST_BACKUP_{}", long.replace('-', "_").to_uppercase());
                let actual = arg.get_env().and_then(|e| e.to_str());
                assert_eq!(
                    actual,
                    Some(expected.as_str()),
                    "{name} --{long} must read {expected}"
                );
                checked += 1;
            }
        }
        assert!(checked >= 30, "only {checked} flags inspected");
    }

    /// Boolean flags are configured from unit files and CI jobs as `=1`, `=yes`
    /// or `=off`; clap's default `bool` parser accepted only "true"/"false" and
    /// aborted the process on anything else.
    #[test]
    fn boolean_flags_accept_the_usual_env_spellings() {
        use clap::CommandFactory;
        const BOOL_FLAGS: &[(&str, &str)] = &[
            ("postgres", "yes"),
            ("postgres", "insecure"),
            ("postgres", "admin"),
            ("postgres", "overwrite"),
            ("postgres", "udp"),
            ("mongodb", "yes"),
            ("s3", "path-style"),
            ("filesystem", "follow-symlinks"),
            ("filesystem", "no-preserve-ownership"),
            ("filesystem", "preserve-xattr"),
            ("server", "udp"),
            ("run", "fail-fast"),
        ];
        use clap::builder::TypedValueParser;
        let command = Cli::command();
        // The shared parser really does accept every spelling…
        for value in [
            "1", "0", "y", "n", "yes", "no", "on", "off", "true", "false",
        ] {
            boolish()
                .parse_ref(&command, None, std::ffi::OsStr::new(value))
                .unwrap_or_else(|error| panic!("boolish must accept {value}: {error}"));
        }
        // …and every boolean flag that reads an env var uses it instead of
        // clap's strict bool parser (compared against a probe argument so the
        // check does not depend on how clap renders a parser).
        let strict = format!(
            "{:?}",
            clap::Arg::new("probe")
                .action(clap::ArgAction::SetTrue)
                .get_value_parser()
        );
        for (name, long) in BOOL_FLAGS {
            let sub = command
                .find_subcommand(name)
                .unwrap_or_else(|| panic!("{name} subcommand"));
            let arg = sub
                .get_arguments()
                .find(|arg| arg.get_long() == Some(long))
                .unwrap_or_else(|| panic!("{name} --{long}"));
            let parser = format!("{:?}", arg.get_value_parser());
            assert_ne!(
                parser, strict,
                "{name} --{long} keeps clap's strict bool parser, which rejects `=1`"
            );
        }
    }

    /// `--udp=false` and `--no-udp` both force relay-only, and the explicit
    /// negation wins over the env-supplied value.
    #[test]
    fn udp_is_a_tri_state_and_no_udp_wins() {
        let base = [
            "rust-backup",
            "filesystem",
            "source",
            "--to",
            "coord:7835",
            "--channel",
            "c1",
        ];
        let resolve = |extra: &[&str]| {
            let args: Vec<&str> = base.iter().copied().chain(extra.iter().copied()).collect();
            let Cmd::Filesystem(a) = parse(&args).cmd else {
                panic!("expected filesystem subcommand")
            };
            resolve_target(&a, "filesystem", Role::Source)
                .expect("resolve")
                .0
                .udp
        };
        assert!(resolve(&[]), "the direct path is attempted by default");
        assert!(resolve(&["--udp"]));
        assert!(!resolve(&["--udp=false"]));
        assert!(!resolve(&["--no-udp"]));
        assert!(!resolve(&["--udp=true", "--no-udp"]));
    }

    /// A digit-only password is a string, not an integer: coercing it made the
    /// module reject its own params with "invalid type: integer".
    #[test]
    fn param_escape_types_values_without_eating_numeric_strings() {
        use serde_json::Value;
        assert_eq!(parse_scalar("6000"), Value::Number(6000.into()));
        assert_eq!(parse_scalar("-1"), Value::Number((-1).into()));
        assert_eq!(parse_scalar("true"), Value::Bool(true));
        assert_eq!(parse_scalar("false"), Value::Bool(false));
        // Non-canonical integers keep the operator's exact text.
        for text in ["007", "+7", "1_000", "12345678901234567890123", " 7"] {
            assert_eq!(
                parse_scalar(text),
                Value::String(text.to_string()),
                "{text}"
            );
        }
        // Explicit quoting is the escape for a value that IS canonical.
        assert_eq!(
            parse_scalar("\"123456\""),
            Value::String("123456".to_string())
        );
        assert_eq!(parse_scalar("\"\""), Value::String(String::new()));
        assert_eq!(parse_scalar("\""), Value::String("\"".to_string()));

        let cli = parse(&[
            "rust-backup",
            "plan",
            "postgres",
            "-P",
            "password=\"123456\"",
            "-P",
            "database=007",
            "-P",
            "port=6000",
        ]);
        let Cmd::Plan(a) = cli.cmd else {
            panic!("expected plan subcommand")
        };
        let overlay = a.params.overlay().expect("overlay");
        assert_eq!(overlay["password"], "123456");
        assert_eq!(overlay["database"], "007");
        assert_eq!(overlay["port"], 6000);
    }

    /// Every registered module is reachable by name (I-MODULAR at the CLI).
    #[test]
    fn registry_exposes_all_modules() {
        let reg = build_registry();
        assert_eq!(
            reg.names(),
            vec!["filesystem", "mongodb", "postgres", "s3"],
            "module registry names"
        );
    }
}
