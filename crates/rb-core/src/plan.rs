//! The backup plan: a complete, self-describing artifact produced by the source
//! and consumed by the destination.
//!
//! INVARIANT (self-contained): the plan MUST carry everything the destination
//! needs to (a) validate it can perform the restore and (b) interpret the
//! subsequent byte stream — without any further round-trip to the source. The
//! module-specific details live in [`BackupPlan::payload`] (opaque to the core).

use serde::{Deserialize, Serialize};

/// Wire/format version of the plan structure. Bump on breaking changes; the
/// destination rejects a plan whose `format_version` it does not understand.
pub const PLAN_FORMAT_VERSION: u16 = 1;

/// Backup operation mode. Extensible: new modes (incremental, snapshot, PITR…)
/// are added here without breaking existing ones.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackupMode {
    /// Full 1:1 copy — destination becomes byte/logical-equivalent to source.
    Copy1to1,
}

/// One ordered unit of work in the plan (a table, a collection, a file, an
/// object…). The `meta` field carries module-specific detail.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlanItem {
    /// Stable id used to tag chunks on the wire.
    pub id: u32,
    /// Restore ordering (lower applies first; e.g. roles before grants).
    pub ordinal: u32,
    /// Module-defined kind ("table", "collection", "file", "object", "role"…).
    pub kind: String,
    /// Human-readable name shown in the plan display.
    pub name: String,
    /// Best-effort size estimate in bytes (for progress + preflight).
    pub estimated_bytes: u64,
    /// Module-specific item detail (column types, file mode/uid/gid, etc.).
    #[serde(default)]
    pub meta: serde_json::Value,
}

/// Integrity expectations for the streamed payload.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IntegritySpec {
    /// Hash algorithm. Currently always `"blake3"`.
    pub algorithm: String,
    /// Whether each item is independently hashed/verified.
    pub per_item: bool,
}

impl Default for IntegritySpec {
    fn default() -> Self {
        IntegritySpec {
            algorithm: "blake3".to_string(),
            per_item: true,
        }
    }
}

/// The complete backup plan. Produced by [`crate::module::Source::analyze`],
/// shown to the user, validated by [`crate::module::Destination::validate`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BackupPlan {
    /// Plan structure version. See [`PLAN_FORMAT_VERSION`].
    pub format_version: u16,
    /// Module that produced this plan ("postgres", "mongodb", …).
    pub module: String,
    /// Operation mode.
    pub mode: BackupMode,
    /// RFC3339 creation timestamp (set by the caller; core does not read a clock).
    pub created_at: String,
    /// Human-readable one-paragraph summary of what will be copied.
    pub source_summary: String,
    /// Ordered restore units.
    pub items: Vec<PlanItem>,
    /// Total estimated payload size across all items.
    pub estimated_bytes: u64,
    /// Integrity expectations.
    pub integrity: IntegritySpec,
    /// Module-specific descriptor — opaque to the core; everything the
    /// destination module needs to perform the restore (DDL, role defs, ACLs,
    /// index specs, fs metadata, bucket policy…).
    pub payload: serde_json::Value,
}

impl BackupPlan {
    /// Render a human-readable summary for display (CLI + async-accept prompt).
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Backup plan  module={}  mode={:?}  created={}\n",
            self.module, self.mode, self.created_at
        ));
        out.push_str(&format!("  {}\n", self.source_summary));
        out.push_str(&format!(
            "  items: {}   estimated: {}   integrity: {}\n",
            self.items.len(),
            human_bytes(self.estimated_bytes),
            self.integrity.algorithm,
        ));
        for it in &self.items {
            out.push_str(&format!(
                "    [{:>3}] {:<10} {:<40} {}\n",
                it.id,
                it.kind,
                it.name,
                human_bytes(it.estimated_bytes)
            ));
        }
        out
    }
}

/// One preflight check result on the destination side.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PreflightCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// Aggregate destination preflight result. The transfer only proceeds when
/// `ok == true`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Preflight {
    pub ok: bool,
    pub checks: Vec<PreflightCheck>,
    /// Destination free space if known (bytes).
    pub available_bytes: Option<u64>,
}

impl Preflight {
    /// A passing preflight with no recorded checks.
    pub fn pass() -> Self {
        Preflight {
            ok: true,
            checks: Vec::new(),
            available_bytes: None,
        }
    }

    /// Add a check and fold its result into `ok`.
    pub fn check(
        mut self,
        name: impl Into<String>,
        passed: bool,
        detail: impl Into<String>,
    ) -> Self {
        self.ok &= passed;
        self.checks.push(PreflightCheck {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
        self
    }
}

/// Format a byte count compactly (KiB/MiB/GiB).
pub fn human_bytes(n: u64) -> String {
    const U: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{f:.2} {}", U[i])
    }
}
