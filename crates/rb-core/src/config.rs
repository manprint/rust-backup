//! Configuration model and layering.
//!
//! Three input sources, highest precedence first: CLI flags > environment
//! variables > YAML config file. The binary parses CLI+env via `clap` (which
//! reads env directly) and merges the YAML underneath. A single session may run
//! multiple targets (the `targets:` list).

use serde::{Deserialize, Serialize};

use crate::error::{BackupError, Result};

/// Which side of a target this process runs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read-only producer.
    Source,
    /// Read-write consumer (restore).
    Destination,
}

/// Transport / coordination-channel settings shared by all modules.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TransportConfig {
    /// Coordination server `host:port` (mirrors bore's `--to`).
    pub to: String,
    /// Channel id the source and destination rendezvous on.
    pub channel: String,
    /// Optional shared secret (HMAC auth, mirrors bore `--secret`).
    #[serde(default)]
    pub secret: Option<String>,
    /// Parallel relay/direct carriers.
    #[serde(default = "default_carriers")]
    pub carriers: u32,
    /// Attempt the direct UDP/QUIC path (falls back to relay).
    #[serde(default = "default_udp")]
    pub udp: bool,
    /// Skip TLS cert verification (testing only).
    #[serde(default)]
    pub insecure: bool,
    /// Optional aggregate payload cap in bytes/second. Zero means unlimited.
    #[serde(default)]
    pub max_rate: Option<u64>,
}

fn default_carriers() -> u32 {
    1
}
fn default_udp() -> bool {
    true
}

impl Default for TransportConfig {
    fn default() -> Self {
        TransportConfig {
            to: String::new(),
            channel: String::new(),
            secret: None,
            carriers: default_carriers(),
            udp: default_udp(),
            insecure: false,
            max_rate: None,
        }
    }
}

/// One target to back up or restore in a session.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TargetSpec {
    /// Module name ("postgres", …).
    pub module: String,
    /// Source or destination side.
    pub role: Role,
    /// Transport settings.
    #[serde(default)]
    pub transport: TransportConfig,
    /// Module-specific connection/params (opaque here; see each module).
    #[serde(default)]
    pub params: serde_json::Value,
    /// Auto-accept the plan (skip the interactive `yes` prompt) on the
    /// destination side. Source-side ignored.
    #[serde(default)]
    pub auto_accept: bool,
}

/// Coordination-server settings (for `rust-backup server`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerConfig {
    /// Address to bind the control port on.
    #[serde(default = "default_bind")]
    pub bind_addr: String,
    /// Control port.
    #[serde(default = "default_control_port")]
    pub control_port: u16,
    /// Optional shared secret required of clients.
    #[serde(default)]
    pub secret: Option<String>,
    /// Max concurrent relayed connections.
    #[serde(default = "default_max_conns")]
    pub max_conns: usize,
    /// Enable the UDP/QUIC direct path brokering.
    #[serde(default = "default_udp")]
    pub udp: bool,
}

fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_control_port() -> u16 {
    7835
}
fn default_max_conns() -> usize {
    256
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind_addr: default_bind(),
            control_port: default_control_port(),
            secret: None,
            max_conns: default_max_conns(),
            udp: default_udp(),
        }
    }
}

/// Top-level session config (from a YAML file).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SessionConfig {
    /// Server settings (only used by `rust-backup server`).
    #[serde(default)]
    pub server: Option<ServerConfig>,
    /// Targets to run.
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    /// Maximum simultaneously running targets; one preserves deterministic order.
    #[serde(default = "default_parallel_targets")]
    pub parallel_targets: usize,
    /// Stop scheduling new targets as soon as one target fails.
    #[serde(default)]
    pub fail_fast: bool,
}

fn default_parallel_targets() -> usize {
    1
}

impl SessionConfig {
    /// Parse a session config from YAML text.
    pub fn from_yaml(text: &str) -> Result<Self> {
        serde_yaml::from_str(text).map_err(|e| BackupError::Config(format!("YAML parse: {e}")))
    }

    /// Load a session config from a YAML file path.
    pub fn from_path(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| BackupError::Config(format!("read {path}: {e}")))?;
        Self::from_yaml(&text)
    }
}
