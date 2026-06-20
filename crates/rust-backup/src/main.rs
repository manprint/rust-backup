#![forbid(unsafe_code)]
//! rust-backup — CLI entrypoint.
//!
//! Wires the module registry, transport, and session runner together. Module
//! connection params are assembled from typed common flags plus `-P key=value`
//! escapes into a JSON object that each module deserializes — so a new module
//! needs no new common flags (I-MODULAR at the CLI too).

use std::sync::Arc;

use anyhow::{anyhow, Context};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rb_core::config::{Role, ServerConfig, SessionConfig, TransportConfig};
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
    },
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
    #[arg(long, env = "RUST_BACKUP_MAX_CONNS", default_value_t = 256)]
    max_conns: usize,
    /// Enable the UDP/QUIC direct-path brokering.
    #[arg(long, env = "RUST_BACKUP_UDP", default_value_t = true)]
    udp: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliRole {
    Source,
    Destination,
}

#[derive(Args)]
struct TargetArgs {
    /// Which side to run.
    role: CliRole,

    // --- transport ---
    #[arg(long, env = "RUST_BACKUP_TO")]
    to: String,
    #[arg(long, env = "RUST_BACKUP_CHANNEL")]
    channel: String,
    #[arg(long, env = "RUST_BACKUP_SECRET")]
    secret: Option<String>,
    #[arg(long, env = "RUST_BACKUP_CARRIERS", default_value_t = 1)]
    carriers: u32,
    /// Disable the direct UDP/QUIC path (relay only).
    #[arg(long = "no-udp")]
    no_udp: bool,
    #[arg(long)]
    insecure: bool,
    /// Auto-accept the plan (destination only).
    #[arg(long = "yes")]
    yes: bool,

    // --- common module params (folded into the params JSON when set) ---
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

    /// YAML config underlay.
    #[arg(long, env = "RUST_BACKUP_CONFIG")]
    config: Option<String>,
}

impl TargetArgs {
    fn transport(&self) -> TransportConfig {
        TransportConfig {
            to: self.to.clone(),
            channel: self.channel.clone(),
            secret: self.secret.clone(),
            carriers: self.carriers,
            udp: !self.no_udp,
            insecure: self.insecure,
        }
    }

    /// Assemble the module params JSON from the set typed flags + `-P` escapes.
    fn params(&self) -> anyhow::Result<TargetParams> {
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
        Ok(TargetParams(Value::Object(m)))
    }
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
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let registry = build_registry();

    match cli.cmd {
        Cmd::Server(a) => {
            let cfg = ServerConfig {
                bind_addr: a.bind_addr,
                control_port: a.control_port,
                secret: a.secret,
                max_conns: a.max_conns,
                udp: a.udp,
            };
            rb_transport::run_server(&cfg)
                .await
                .context("coordination server")?;
            Ok(())
        }
        Cmd::Run { config } => run_session(&registry, &config).await,
        Cmd::Postgres(a) => run_target(&registry, "postgres", &a).await,
        Cmd::Mongodb(a) => run_target(&registry, "mongodb", &a).await,
        Cmd::Filesystem(a) => run_target(&registry, "filesystem", &a).await,
        Cmd::S3(a) => run_target(&registry, "s3", &a).await,
    }
}

/// Run one target from CLI args.
async fn run_target(reg: &ModuleRegistry, module: &str, a: &TargetArgs) -> anyhow::Result<()> {
    let m = reg
        .get(module)
        .ok_or_else(|| anyhow!("unknown module '{module}'"))?;
    let params = a.params()?;
    let transport = a.transport();
    let role = match a.role {
        CliRole::Source => Role::Source,
        CliRole::Destination => Role::Destination,
    };
    execute(&m, role, &transport, &params, a.yes).await
}

/// Run a multi-target session from YAML.
async fn run_session(reg: &ModuleRegistry, path: &str) -> anyhow::Result<()> {
    let cfg = SessionConfig::from_path(path).map_err(|e| anyhow!("{e}"))?;
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

/// Connect the transport for `role` and drive the session.
async fn execute(
    module: &Arc<dyn rb_core::BackupModule>,
    role: Role,
    transport: &TransportConfig,
    params: &TargetParams,
    auto_accept: bool,
) -> anyhow::Result<()> {
    let progress = Progress::default();
    match role {
        Role::Source => {
            let src = module
                .open_source(params)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let ch = rb_transport::connect_source(transport)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            session::run_source(&*src, &ch, &progress)
                .await
                .map_err(|e| anyhow!("{e}"))?;
        }
        Role::Destination => {
            let dst = module
                .open_destination(params)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let ch = rb_transport::connect_destination(transport)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let mut accept = move |plan: &BackupPlan| {
                if auto_accept {
                    return true;
                }
                prompt_yes(plan)
            };
            session::run_destination(&*dst, &ch, &progress, &mut accept)
                .await
                .map_err(|e| anyhow!("{e}"))?;
        }
    }
    Ok(())
}

/// Interactive async-accept: show the plan and read `yes`/`no` from stdin.
fn prompt_yes(plan: &BackupPlan) -> bool {
    use std::io::Write;
    print!("\n{}\nProceed with restore? [yes/no] ", plan.render());
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
