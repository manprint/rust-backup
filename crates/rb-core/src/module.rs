//! Backup-module trait surface and registry.
//!
//! MODULARITY INVARIANT: a new backend (the next heterogeneous target) is added
//! by implementing [`BackupModule`] in its own crate and registering it — no
//! core change. Everything a module needs flows through these traits plus the
//! transport-agnostic [`crate::channel`] types.
//!
//! SOURCE-IMMUTABILITY INVARIANT: [`Source`] exposes only read/analyze/stream
//! operations and a [`Source::fingerprint`] used by the session to assert the
//! source is byte/logically unchanged before and after every run.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::channel::{ChunkSink, ChunkSource};
use crate::error::{BackupError, Result};
use crate::plan::{BackupPlan, Preflight};
use crate::verification::{RestoreEvidence, VerificationReport};

/// Opaque per-target parameter bag (merged from CLI/env/yaml). Each module
/// deserializes it into its own typed params struct.
#[derive(Clone, Debug, Default)]
pub struct TargetParams(pub serde_json::Value);

impl TargetParams {
    /// Deserialize into a module-defined params type.
    pub fn deserialize<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.0.clone()).map_err(|e| {
            let mut message = format!("invalid module params: {e}");
            // `-P key=123456` is typed as a number by the CLI escape, so a
            // digit-only password or bucket name lands here. Name the escape
            // instead of leaving the operator with a type error they did not
            // write.
            if message.contains("invalid type: integer")
                || message.contains("invalid type: boolean")
            {
                message.push_str(
                    "; a `-P key=value` escape is typed by shape — quote a value that \
                     must stay a string: -P key='\"value\"'",
                );
            }
            BackupError::Config(message)
        })
    }

    /// Build from any serializable struct (used by the CLI layer).
    pub fn from_value(v: serde_json::Value) -> Self {
        TargetParams(v)
    }
}

/// The source side of a target: analyze (read-only) and stream payload out.
#[async_trait]
pub trait Source: Send + Sync {
    /// Inspect the backend and build a complete, self-describing [`BackupPlan`].
    /// MUST NOT mutate the source.
    async fn analyze(&self) -> Result<BackupPlan>;

    /// Stream the payload described by `plan` into `sink`, chunk by chunk, with
    /// no local temp files. MUST NOT mutate the source.
    async fn stream_out(&self, plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()>;

    /// A stable fingerprint of source state (catalog hash, fs tree hash, object
    /// listing hash…). Used by the session to prove immutability. Read-only.
    async fn fingerprint(&self) -> Result<String>;

    /// Whether this source was opened for a hot backup: it reads one consistent
    /// snapshot of a backend that keeps being written, and its plan is
    /// [`crate::plan::BackupMode::HotSnapshot`]. The session then skips the
    /// fingerprint audit, which a live backend fails by construction. Only a
    /// module whose [`BackupModule::supports_hot_backup`] is true may return
    /// true here.
    fn hot_backup(&self) -> bool {
        false
    }
}

/// The destination side of a target: validate and apply the streamed payload.
#[async_trait]
pub trait Destination: Send + Sync {
    /// Maximum independent data carriers this destination can apply safely.
    /// The conservative default protects new modules until they opt in.
    fn max_carriers(&self) -> usize {
        1
    }
    /// Preflight the plan: disk space, accessibility, version compatibility,
    /// privilege checks. The transfer proceeds only if `Preflight::ok`.
    async fn validate(&self, plan: &BackupPlan) -> Result<Preflight>;

    /// Whether this destination's operator accepted a hot-backup plan
    /// ([`crate::plan::BackupMode::HotSnapshot`]). The session fails the
    /// preflight of such a plan otherwise: the restored copy matches a snapshot
    /// of a source that kept changing, and consent to that belongs to both
    /// sides.
    fn accepts_hot_backup(&self) -> bool {
        false
    }

    /// Apply the streamed payload to reach 1:1 with the source. No temp files.
    async fn stream_in(&self, plan: &BackupPlan, src: &mut dyn ChunkSource) -> Result<()>;

    /// Read the persisted destination back and prove that every selected item
    /// matches the source commitment. Success is not acknowledged to the
    /// source until this verification completes.
    async fn verify(
        &self,
        plan: &BackupPlan,
        evidence: &RestoreEvidence,
    ) -> Result<VerificationReport>;

    /// The run failed *after* the payload was applied — read-back verification
    /// said no, or the source rejected the completion (it mutated under us).
    /// What the destination holds is then a complete-looking but uncertified
    /// copy, which is exactly the thing this tool refuses to leave behind, so a
    /// module that can tell what this run created removes it here. Best effort
    /// by construction: the run already failed and its error is the real
    /// outcome, so this cannot return one. Default: nothing to undo.
    async fn abandon(&self) {}
}

/// A backup-capable backend type. One instance per module, registered once.
#[async_trait]
pub trait BackupModule: Send + Sync {
    /// CLI/identifier name ("postgres", "mongodb", "filesystem", "s3").
    fn name(&self) -> &'static str;

    /// Maximum independent data carriers this module can restore safely.
    /// Defaulting to one keeps third-party modules conservative by design.
    fn max_carriers(&self) -> u32 {
        1
    }

    /// Human-readable supported-version range (e.g. "PostgreSQL 10+").
    fn version_support(&self) -> &'static str;

    /// Whether this module can take a hot backup (`hot_backup` param): read a
    /// consistent snapshot of a source that stays online. The binary refuses
    /// the param for a module that cannot, rather than let it run a cold copy
    /// of a live backend and fail the immutability audit — or worse, restore
    /// tables read at different moments.
    fn supports_hot_backup(&self) -> bool {
        false
    }

    /// Open a read-only source from connection params.
    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>>;

    /// Open a read-write destination from connection params.
    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>>;
}

#[cfg(test)]
mod param_tests {
    use super::TargetParams;

    #[derive(Debug, serde::Deserialize)]
    struct Params {
        // Only the deserialization outcome is under test; the value is never read.
        #[allow(dead_code)]
        password: String,
    }

    /// A shape-typed `-P` value that the module wants as a string must say how
    /// to force the string, not just report a type the operator never typed.
    #[test]
    fn a_numeric_typed_param_names_the_quoting_escape() {
        let params = TargetParams(serde_json::json!({ "password": 123456 }));
        let error = params
            .deserialize::<Params>()
            .expect_err("an integer is not a password");
        let text = error.to_string();
        assert!(text.contains("invalid type: integer"), "{text}");
        assert!(text.contains("quote a value"), "{text}");
    }
}

/// Registry of available modules, populated at startup by the binary.
#[derive(Default, Clone)]
pub struct ModuleRegistry {
    modules: HashMap<&'static str, Arc<dyn BackupModule>>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a module under its `name()`.
    pub fn register(&mut self, module: Arc<dyn BackupModule>) {
        self.modules.insert(module.name(), module);
    }

    /// Look up a module by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn BackupModule>> {
        self.modules.get(name).cloned()
    }

    /// All registered module names (sorted).
    pub fn names(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.modules.keys().copied().collect();
        v.sort_unstable();
        v
    }
}
