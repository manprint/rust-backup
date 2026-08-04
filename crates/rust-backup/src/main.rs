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
        #[arg(long, env = "RUST_BACKUP_FAIL_FAST")]
        fail_fast: bool,
    },
    /// Dry-run: analyze a source and print its plan; no transport, no transfer.
    Plan(PlanArgs),
    /// PostgreSQL backup/restore (10..=latest).
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
        default_missing_value = "true"
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
#[derive(Args, Default)]
struct ModuleParamArgs {
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    password: Option<String>,
    #[arg(long)]
    database: Option<String>,
    #[arg(long)]
    sslmode: Option<String>,
    #[arg(long)]
    uri: Option<String>,
    #[arg(long = "auth-db")]
    auth_db: Option<String>,
    #[arg(long)]
    root: Option<String>,
    #[arg(long)]
    bucket: Option<String>,
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    region: Option<String>,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long = "access-key")]
    access_key: Option<String>,
    #[arg(long = "secret-key")]
    secret_key: Option<String>,

    // --- bool module params: only folded when present (flip a default) ---
    /// postgres: connect as admin (destination).
    #[arg(long)]
    admin: bool,
    /// Destination: replace existing databases, collections or objects.
    #[arg(long, env = "RUST_BACKUP_OVERWRITE")]
    overwrite: bool,
    /// s3: use path-style addressing (MinIO).
    #[arg(long = "path-style")]
    path_style: bool,
    /// filesystem: follow symlinks.
    #[arg(long = "follow-symlinks")]
    follow_symlinks: bool,
    /// filesystem: do NOT preserve ownership (default is to preserve).
    #[arg(long = "no-preserve-ownership")]
    no_preserve_ownership: bool,
    /// filesystem: preserve xattrs.
    #[arg(long = "preserve-xattr")]
    preserve_xattr: bool,

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
    /// Disable the direct UDP/QUIC path (relay only).
    #[arg(long = "no-udp")]
    no_udp: bool,
    #[arg(long)]
    insecure: bool,
    /// Aggregate payload limit in bytes/second (0/unset means unlimited).
    #[arg(long, env = "RUST_BACKUP_MAX_RATE")]
    max_rate: Option<u64>,
    /// Auto-accept the plan (destination only).
    #[arg(long = "yes")]
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
        // bool flags fold only when they flip a default.
        if self.admin {
            m.insert("admin".into(), Value::Bool(true));
        }
        if self.overwrite {
            m.insert("overwrite".into(), Value::Bool(true));
        }
        if self.path_style {
            m.insert("path_style".into(), Value::Bool(true));
        }
        if self.follow_symlinks {
            m.insert("follow_symlinks".into(), Value::Bool(true));
        }
        if self.no_preserve_ownership {
            m.insert("preserve_ownership".into(), Value::Bool(false));
        }
        if self.preserve_xattr {
            m.insert("preserve_xattr".into(), Value::Bool(true));
        }
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
        let cfg = SessionConfig::from_path(path).map_err(|e| anyhow!("{e}"))?;
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

fn parse_scalar(v: &str) -> serde_json::Value {
    use serde_json::Value;
    match v {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => {
            if let Ok(n) = v.parse::<i64>() {
                Value::Number(n.into())
            } else {
                Value::String(v.to_string())
            }
        }
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

    match cli.cmd {
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
        } => run_session(&registry, &config, parallel_targets, fail_fast).await,
        Cmd::Plan(a) => print_plan(&registry, &a).await,
        Cmd::Postgres(a) => run_target(&registry, "postgres", &a).await,
        Cmd::Mongodb(a) => run_target(&registry, "mongodb", &a).await,
        Cmd::Filesystem(a) => run_target(&registry, "filesystem", &a).await,
        Cmd::S3(a) => run_target(&registry, "s3", &a).await,
    }
}

/// Stable shell-facing error codes. Keep this independent from stderr wording.
fn exit_code(error: &anyhow::Error) -> i32 {
    if let Some(typed) = error.downcast_ref::<rb_core::BackupError>() {
        return exit_code_backup(typed);
    }
    // Fallback only for errors that genuinely originate outside rb-core (clap,
    // filesystem config reads, or a transport library before it is phase-wrapped).
    let text = error.to_string();
    if text.contains("[Config]")
        || text.contains("unknown module")
        || text.contains("--to is required")
    {
        2
    } else if text.contains("[Preflight]") {
        3
    } else if text.contains("[PlanRejected]") || text.contains("rejected by operator") {
        4
    } else if text.contains("[Verify]") || text.contains("[Apply]") {
        5
    } else if text.contains("[SourceMutated]") || text.contains("source fingerprint changed") {
        6
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
    let src = m.open_source(&params).await.map_err(|e| anyhow!("{e}"))?;
    let plan = src.analyze().await.map_err(|e| anyhow!("{e}"))?;
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
    let mut cfg = SessionConfig::from_path(path).map_err(|e| anyhow!("{e}"))?;
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
        Ok(())
    } else {
        Err(anyhow!(
            "{} target(s) failed: {}",
            errors.len(),
            errors
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ))
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
async fn execute(
    module: &Arc<dyn rb_core::BackupModule>,
    role: Role,
    transport: &TransportConfig,
    params: &TargetParams,
    auto_accept: bool,
) -> anyhow::Result<()> {
    let progress = Progress::default();
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

    #[test]
    fn exit_code_maps_phase_classes() {
        for (message, code) in [
            ("[Config] bad", 2),
            ("[Preflight] bad", 3),
            ("[PlanRejected] bad", 4),
            ("[Verify] bad", 5),
            ("[SourceMutated] bad", 6),
            ("[Connect] bad", 7),
        ] {
            assert_eq!(exit_code(&anyhow!(message)), code);
        }
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
