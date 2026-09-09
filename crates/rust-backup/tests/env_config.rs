//! USAGE.md's configuration contract, exercised against the real binary.
//!
//! Env vars cannot be tested in-process: `Cli::parse` reads the process
//! environment, so one test's variable would leak into every other test in the
//! same binary. Each case therefore spawns the CLI with its own environment.

use std::net::TcpListener;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rust-backup")
}

/// A port nothing listens on, so a transport attempt fails immediately.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = listener.local_addr().expect("probe addr").port();
    drop(listener);
    port
}

/// Env vars this test must not inherit from the surrounding shell.
const CLEARED: &[&str] = &[
    "RUST_BACKUP_TO",
    "RUST_BACKUP_CHANNEL",
    "RUST_BACKUP_SECRET",
    "RUST_BACKUP_SECRET_FILE",
    "RUST_BACKUP_CONFIG",
    "RUST_BACKUP_CARRIERS",
    "RUST_BACKUP_MAX_RATE",
    "RUST_BACKUP_UDP",
    "RUST_BACKUP_YES",
    "RUST_BACKUP_INSECURE",
    "RUST_BACKUP_OVERWRITE",
    "RUST_BACKUP_ADMIN",
    "RUST_BACKUP_HOST",
    "RUST_BACKUP_PORT",
    "RUST_BACKUP_USER",
    "RUST_BACKUP_PASSWORD",
    "RUST_BACKUP_DATABASE",
    "RUST_BACKUP_SSLMODE",
    "RUST_BACKUP_URI",
    "RUST_BACKUP_AUTH_DB",
    "RUST_BACKUP_ROOT",
    "RUST_BACKUP_BUCKET",
    "RUST_BACKUP_ENDPOINT",
    "RUST_BACKUP_REGION",
    "RUST_BACKUP_PREFIX",
    "RUST_BACKUP_ACCESS_KEY",
    "RUST_BACKUP_SECRET_KEY",
    "RUST_BACKUP_PATH_STYLE",
    "RUST_BACKUP_FOLLOW_SYMLINKS",
    "RUST_BACKUP_NO_PRESERVE_OWNERSHIP",
    "RUST_BACKUP_PRESERVE_XATTR",
];

fn command() -> Command {
    let mut cmd = Command::new(bin());
    for name in CLEARED {
        cmd.env_remove(name);
    }
    cmd
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rust-backup-env-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A module param supplied only through its documented env var must configure
/// the run: `RUST_BACKUP_ROOT` alone is enough to analyze a tree.
#[test]
fn a_module_param_can_come_from_its_env_var_alone() {
    let root = scratch("plan-root");
    std::fs::write(root.join("a.txt"), b"env-configured").expect("seed file");
    let out = command()
        .args(["plan", "filesystem"])
        .env("RUST_BACKUP_ROOT", &root)
        .output()
        .expect("spawn plan");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "plan must run from the env var alone: {stderr}"
    );
    assert!(stdout.contains("a.txt"), "{stdout}");
    std::fs::remove_dir_all(&root).ok();
}

/// `RUST_BACKUP_YES=1` used to abort with `invalid value '1' for '--yes'`,
/// which is exactly how a unit file or CI job spells a boolean. Every usual
/// spelling must reach the run instead of clap's usage error (exit 2).
#[test]
fn boolean_env_vars_accept_the_usual_spellings() {
    let root = scratch("bool-root");
    let port = closed_port();
    let to = format!("127.0.0.1:{port}");
    let cases: &[(&str, &str)] = &[
        ("RUST_BACKUP_YES", "1"),
        ("RUST_BACKUP_YES", "yes"),
        ("RUST_BACKUP_YES", "on"),
        ("RUST_BACKUP_YES", "true"),
        ("RUST_BACKUP_YES", "0"),
        ("RUST_BACKUP_UDP", "0"),
        ("RUST_BACKUP_UDP", "off"),
        ("RUST_BACKUP_UDP", "no"),
        ("RUST_BACKUP_INSECURE", "1"),
        ("RUST_BACKUP_OVERWRITE", "y"),
        ("RUST_BACKUP_ADMIN", "1"),
        ("RUST_BACKUP_FOLLOW_SYMLINKS", "0"),
        ("RUST_BACKUP_PRESERVE_XATTR", "0"),
        ("RUST_BACKUP_NO_PRESERVE_OWNERSHIP", "no"),
        ("RUST_BACKUP_PATH_STYLE", "1"),
    ];
    for (name, value) in cases {
        let out = command()
            .args([
                "filesystem",
                "source",
                "--to",
                &to,
                "--channel",
                "env-probe",
                "--no-udp",
                "--insecure",
            ])
            .env("RUST_BACKUP_ROOT", &root)
            .env(name, value)
            .output()
            .unwrap_or_else(|error| panic!("spawn with {name}={value}: {error}"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("invalid value"),
            "{name}={value} was refused by the parser: {stderr}"
        );
        assert_eq!(
            out.status.code(),
            Some(7),
            "{name}={value} must reach the transport, not a usage error: {stderr}"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Accepting more spellings must not accept nonsense: a misspelled boolean is
/// still a configuration error rather than a silent default.
#[test]
fn a_misspelled_boolean_env_var_is_still_refused() {
    let root = scratch("bad-bool-root");
    let out = command()
        .args(["plan", "filesystem"])
        .env("RUST_BACKUP_ROOT", &root)
        .env("RUST_BACKUP_FOLLOW_SYMLINKS", "banana")
        .output()
        .expect("spawn plan");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("invalid value"), "{stderr}");
    std::fs::remove_dir_all(&root).ok();
}

/// CLI beats env (USAGE.md's precedence): the flag value wins even when the
/// env var names a different tree.
#[test]
fn a_flag_overrides_its_env_var() {
    let flag_root = scratch("flag-root");
    let env_root = scratch("env-root");
    std::fs::write(flag_root.join("from-flag.txt"), b"flag").expect("seed flag file");
    std::fs::write(env_root.join("from-env.txt"), b"env").expect("seed env file");
    let out = command()
        .args(["plan", "filesystem", "--root"])
        .arg(&flag_root)
        .env("RUST_BACKUP_ROOT", &env_root)
        .output()
        .expect("spawn plan");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("from-flag.txt"), "{stdout}");
    assert!(!stdout.contains("from-env.txt"), "{stdout}");
    std::fs::remove_dir_all(&flag_root).ok();
    std::fs::remove_dir_all(&env_root).ok();
}
