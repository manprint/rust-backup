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
    /// Full copy of a source that stays online (`--hot-backup`). The source
    /// reads one consistent snapshot, the destination is verified against that
    /// snapshot, and the source is NOT audited for immutability: its live state
    /// moves on while it is read. A destination restores such a plan only when
    /// its own operator accepted it too
    /// ([`crate::module::Destination::accepts_hot_backup`]). A build that
    /// predates this variant cannot decode the plan and refuses it.
    HotSnapshot,
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
    /// Render a human-readable summary listing every item (`plan` dry-run).
    pub fn render(&self) -> String {
        self.render_items(usize::MAX)
    }

    /// As [`Self::render`], listing at most [`RENDER_SUMMARY_ITEMS`] items.
    ///
    /// For the run log and the accept prompt: at the item ceiling the full
    /// listing is 200 000 lines on each peer, which buries the preflight
    /// checks and the prompt it is printed above.
    pub fn render_summary(&self) -> String {
        self.render_items(RENDER_SUMMARY_ITEMS)
    }

    fn render_items(&self, limit: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Backup plan  module={}  mode={:?}  created={}\n",
            printable(&self.module),
            self.mode,
            printable(&self.created_at)
        ));
        out.push_str(&format!("  {}\n", printable(&self.source_summary)));
        out.push_str(&format!(
            "  items: {}   estimated: {}   integrity: {}\n",
            self.items.len(),
            human_bytes(self.estimated_bytes),
            printable(&self.integrity.algorithm),
        ));
        for it in self.items.iter().take(limit) {
            out.push_str(&format!(
                "    [{:>3}] {:<10} {:<40} {}\n",
                it.id,
                printable(&it.kind),
                printable(&it.name),
                human_bytes(it.estimated_bytes)
            ));
        }
        if self.items.len() > limit {
            out.push_str(&format!(
                "    … and {} more item(s); `rust-backup plan` lists them all\n",
                self.items.len() - limit
            ));
        }
        out
    }
}

/// Items listed by [`BackupPlan::render_summary`].
pub const RENDER_SUMMARY_ITEMS: usize = 50;

/// `text` with control characters escaped.
///
/// Plan text is the peer's, and item names are whatever the source backend
/// holds: a file called `x\nRESTORE VERIFIED` must not print a line of its own
/// in the log an operator reads, nor drive the terminal with escape sequences.
fn printable(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().any(char::is_control) {
        text.chars()
            .flat_map(|c| {
                if c.is_control() {
                    c.escape_default().collect::<Vec<_>>()
                } else {
                    vec![c]
                }
            })
            .collect::<String>()
            .into()
    } else {
        text.into()
    }
}

impl PlanItem {
    /// Whether this item must emit an [`crate::channel::ChunkEvent::ItemEnd`].
    ///
    /// Most plan items carry data. Modules with metadata-only entries (for
    /// example filesystem directories and symlinks) opt out with
    /// `meta: {"expects_data": false}`. Keeping this convention in the opaque
    /// metadata avoids a plan-format compatibility break while making the core
    /// completion check unambiguous.
    pub fn expects_data(&self) -> bool {
        self.meta
            .get("expects_data")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
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

/// Render at most a few entries of a fault list plus the total, so a preflight
/// or verification line stays readable for a plan at the item ceiling.
pub fn summarize(entries: &[String]) -> String {
    const SHOWN: usize = 5;
    if entries.len() <= SHOWN {
        return entries.join(", ");
    }
    format!(
        "{}, and {} more",
        entries[..SHOWN].join(", "),
        entries.len() - SHOWN
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(names: &[String]) -> BackupPlan {
        BackupPlan {
            format_version: PLAN_FORMAT_VERSION,
            module: "filesystem".into(),
            mode: BackupMode::Copy1to1,
            created_at: "now".into(),
            source_summary: "tree".into(),
            items: names
                .iter()
                .enumerate()
                .map(|(id, name)| PlanItem {
                    id: id as u32,
                    ordinal: id as u32,
                    kind: "file".into(),
                    name: name.clone(),
                    estimated_bytes: 1,
                    meta: serde_json::Value::Null,
                })
                .collect(),
            estimated_bytes: names.len() as u64,
            integrity: IntegritySpec::default(),
            payload: serde_json::Value::Null,
        }
    }

    #[test]
    fn the_logged_plan_is_bounded_and_the_dry_run_is_not() {
        let names: Vec<String> = (0..RENDER_SUMMARY_ITEMS + 7)
            .map(|i| format!("f{i}"))
            .collect();
        let plan = plan_of(&names);
        let summary = plan.render_summary();
        assert_eq!(summary.lines().count(), 3 + RENDER_SUMMARY_ITEMS + 1);
        assert!(summary.contains("and 7 more item(s)"), "{summary}");
        assert_eq!(plan.render().lines().count(), 3 + names.len());
    }

    #[test]
    fn a_plan_name_cannot_forge_a_log_line() {
        let plan = plan_of(&["x\nRESTORE VERIFIED\u{1b}[2J".to_string()]);
        let rendered = plan.render();
        assert_eq!(rendered.lines().count(), 4, "{rendered}");
        assert!(
            rendered.contains("x\\nRESTORE VERIFIED\\u{1b}[2J"),
            "{rendered}"
        );
    }

    /// Fault lists stay readable for a plan at the item ceiling.
    #[test]
    fn fault_summaries_are_bounded() {
        let many: Vec<String> = (0..100).map(|i| format!("k{i}")).collect();
        let rendered = summarize(&many);
        assert!(rendered.contains("and 95 more"), "{rendered}");
        assert_eq!(summarize(&many[..2]), "k0, k1");
        assert_eq!(summarize(&[]), "");
    }
}
