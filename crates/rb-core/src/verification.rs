//! Destination read-back verification shared by every backup module.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::channel::{completion_digest, ChunkSink};
use crate::error::{BackupError, Phase, Result};

/// Source commitments already checked by the transport receiver. A destination
/// module must independently reproduce these digests from its persisted state.
#[derive(Clone, Debug)]
pub struct RestoreEvidence {
    pub total_bytes: u64,
    pub payload_blake3: String,
    pub item_blake3: BTreeMap<u32, String>,
}

/// Proof returned to both CLIs after destination read-back succeeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub items_verified: usize,
    pub bytes_verified: u64,
    pub payload_blake3: String,
    pub detail: String,
    /// Rows the destination wrote through `COPY`, where the module counts them.
    /// Bytes prove that what arrived is what was sent; rows prove that nothing
    /// was left behind. Defaulted so a module that does not count them, and a
    /// report from an older peer, both deserialize.
    #[serde(default)]
    pub table_rows_verified: u64,
    /// Rows a module re-derived on the destination rather than streaming them —
    /// the contents of a materialized view, which `REFRESH` produces.
    #[serde(default)]
    pub derived_rows_verified: u64,
    /// Integrity constraints the destination re-read and matched.
    #[serde(default)]
    pub constraints_verified: u64,
    /// How many of those are `NOT VALID` — enforced for new rows only. A
    /// restore that quietly validated them would have had to reject rows the
    /// source legally holds, so the number is worth printing.
    #[serde(default)]
    pub constraints_not_valid: u64,
    /// Facts the module reports that are not failures: the destination is 1:1
    /// with the source *except* for what is named here (an extension installed
    /// at the destination's default version, say). A module also folds them
    /// into `detail`, so the single headline record keeps carrying them for the
    /// source peer, which only ever sees the report itself.
    #[serde(default)]
    pub deviations: Vec<String>,
}

impl VerificationReport {
    /// Attach the row totals a module counted while restoring.
    pub fn with_rows(mut self, table_rows: u64, derived_rows: u64) -> Self {
        self.table_rows_verified = table_rows;
        self.derived_rows_verified = derived_rows;
        self
    }

    /// Attach the constraint totals a module compared.
    pub fn with_constraints(mut self, verified: u64, not_valid: u64) -> Self {
        self.constraints_verified = verified;
        self.constraints_not_valid = not_valid;
        self
    }

    /// Attach the non-failure notes a module wants the operator to read.
    pub fn with_deviations(mut self, deviations: Vec<String>) -> Self {
        self.deviations = deviations;
        self
    }

    /// The lines printed under the destination's `RESTORE VERIFIED` record,
    /// in order. Empty for a module that counts none of these, so every other
    /// module's block stays exactly as it was.
    pub fn extra_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.rows_total() > 0 {
            lines.push(format!(
                "rows verified: {} from tables, {} from materialized views, {} rows",
                self.table_rows_verified,
                self.derived_rows_verified,
                self.rows_total()
            ));
        }
        if self.constraints_verified > 0 {
            lines.push(format!(
                "constraints: {} (validated {}, not valid {})",
                self.constraints_verified,
                self.constraints_verified
                    .saturating_sub(self.constraints_not_valid),
                self.constraints_not_valid
            ));
        }
        for deviation in &self.deviations {
            lines.push(format!("deviation: {deviation}"));
        }
        lines
    }

    /// Total rows accounted for, streamed plus re-derived.
    pub fn rows_total(&self) -> u64 {
        self.table_rows_verified
            .saturating_add(self.derived_rows_verified)
    }
}

impl RestoreEvidence {
    /// Used by in-memory/test destinations whose persistence is their captured
    /// byte buffer. Production modules perform backend-specific read-back.
    pub fn report(&self, detail: impl Into<String>) -> VerificationReport {
        VerificationReport {
            items_verified: self.item_blake3.len(),
            bytes_verified: self.total_bytes,
            payload_blake3: self.payload_blake3.clone(),
            detail: detail.into(),
            table_rows_verified: 0,
            derived_rows_verified: 0,
            constraints_verified: 0,
            constraints_not_valid: 0,
            deviations: Vec::new(),
        }
    }
}

/// A zero-buffer sink for reading a restored backend through its normal source
/// path. It hashes each item again and compares it with the transport-verified
/// source commitment.
pub struct VerificationSink {
    evidence: RestoreEvidence,
    active: HashMap<u32, (blake3::Hasher, u64)>,
    verified: BTreeMap<u32, String>,
    bytes: u64,
    finished: bool,
}

impl VerificationSink {
    pub fn new(evidence: &RestoreEvidence) -> Self {
        Self {
            evidence: evidence.clone(),
            active: HashMap::new(),
            verified: BTreeMap::new(),
            bytes: 0,
            finished: false,
        }
    }

    pub fn report(&self, detail: impl Into<String>) -> Result<VerificationReport> {
        if !self.finished {
            return Err(BackupError::phase(
                Phase::Verify,
                "destination read-back verifier did not finish",
            ));
        }
        Ok(VerificationReport {
            items_verified: self.verified.len(),
            bytes_verified: self.bytes,
            payload_blake3: completion_digest(&self.verified),
            detail: detail.into(),
            table_rows_verified: 0,
            derived_rows_verified: 0,
            constraints_verified: 0,
            constraints_not_valid: 0,
            deviations: Vec::new(),
        })
    }

    fn ensure_open(&self) -> Result<()> {
        if self.finished {
            Err(BackupError::phase(
                Phase::Verify,
                "destination emitted data after read-back verification finished",
            ))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl ChunkSink for VerificationSink {
    async fn send_chunk(&mut self, item_id: u32, offset: u64, data: &[u8]) -> Result<()> {
        self.ensure_open()?;
        if !self.evidence.item_blake3.contains_key(&item_id) {
            return Err(BackupError::phase(
                Phase::Verify,
                format!("destination read-back returned unexpected item={item_id}"),
            ));
        }
        let (hasher, bytes) = self.active.entry(item_id).or_default();
        if offset != *bytes {
            return Err(BackupError::phase(
                Phase::Verify,
                format!("destination read-back item={item_id} offset={offset}, expected={bytes}"),
            ));
        }
        hasher.update(data);
        *bytes += data.len() as u64;
        self.bytes += data.len() as u64;
        Ok(())
    }

    async fn finish_item(&mut self, item_id: u32, total: u64, declared: &str) -> Result<()> {
        self.ensure_open()?;
        if self.verified.contains_key(&item_id) {
            return Err(BackupError::phase(
                Phase::Verify,
                format!("destination read-back duplicated item={item_id}"),
            ));
        }
        let (hasher, bytes) = self.active.remove(&item_id).unwrap_or_default();
        let actual = hasher.finalize().to_hex().to_string();
        let expected = self.evidence.item_blake3.get(&item_id).ok_or_else(|| {
            BackupError::phase(
                Phase::Verify,
                format!("destination read-back returned unexpected item={item_id}"),
            )
        })?;
        if bytes != total || declared != actual || expected != &actual {
            return Err(BackupError::Integrity(format!(
                "destination read-back mismatch item={item_id}: bytes={bytes}/{total} source={expected} destination={actual}"
            )));
        }
        self.verified.insert(item_id, actual);
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        self.ensure_open()?;
        if !self.active.is_empty() {
            let mut ids: Vec<_> = self.active.keys().copied().collect();
            ids.sort_unstable();
            return Err(BackupError::phase(
                Phase::Verify,
                format!("destination read-back left incomplete items={ids:?}"),
            ));
        }
        let expected_ids: Vec<_> = self.evidence.item_blake3.keys().copied().collect();
        let actual_ids: Vec<_> = self.verified.keys().copied().collect();
        if actual_ids != expected_ids {
            return Err(BackupError::phase(
                Phase::Verify,
                format!(
                    "destination read-back item mismatch: expected={expected_ids:?} actual={actual_ids:?}"
                ),
            ));
        }
        let payload = completion_digest(&self.verified);
        if self.bytes != self.evidence.total_bytes || payload != self.evidence.payload_blake3 {
            return Err(BackupError::Integrity(format!(
                "destination read-back payload mismatch: bytes={}/{} digest={}/{}",
                self.bytes, self.evidence.total_bytes, payload, self.evidence.payload_blake3
            )));
        }
        self.finished = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChunkSink;

    fn evidence(bytes: &[u8]) -> RestoreEvidence {
        let digest = crate::wire::blake3_hex(bytes);
        let item_blake3 = BTreeMap::from([(7, digest)]);
        RestoreEvidence {
            total_bytes: bytes.len() as u64,
            payload_blake3: completion_digest(&item_blake3),
            item_blake3,
        }
    }

    #[tokio::test]
    async fn readback_sink_proves_exact_payload() {
        let bytes = b"persisted bytes";
        let mut sink = VerificationSink::new(&evidence(bytes));
        sink.send_chunk(7, 0, bytes).await.unwrap();
        sink.finish_item(7, bytes.len() as u64, &crate::wire::blake3_hex(bytes))
            .await
            .unwrap();
        sink.finish().await.unwrap();
        let report = sink.report("read back").unwrap();
        assert_eq!(report.items_verified, 1);
        assert_eq!(report.bytes_verified, bytes.len() as u64);
    }

    #[test]
    fn verification_block_contains_row_and_constraint_lines() {
        let report = evidence(b"x")
            .report("PostgreSQL catalog and deterministic binary COPY read-back match")
            .with_rows(4200, 120)
            .with_constraints(42, 2)
            .with_deviations(vec![
                "extension rbtest restored at version 1.1 (source 1.0)".to_string(),
            ]);
        assert_eq!(
            report.extra_lines(),
            vec![
                "rows verified: 4200 from tables, 120 from materialized views, 4320 rows"
                    .to_string(),
                "constraints: 42 (validated 40, not valid 2)".to_string(),
                "deviation: extension rbtest restored at version 1.1 (source 1.0)".to_string(),
            ]
        );
    }

    #[test]
    fn verification_block_existing_lines_unchanged() {
        // The pre-existing record is `items`, `bytes`, `blake3`, `detail`. A
        // module that counts none of the new facts must add no line at all, and
        // the four fields must read exactly as they did before § 2.3.
        let report = evidence(b"persisted bytes").report("destination read-back matches");
        assert_eq!(report.extra_lines(), Vec::<String>::new());
        assert_eq!(report.items_verified, 1);
        assert_eq!(report.bytes_verified, 15);
        assert_eq!(report.detail, "destination read-back matches");
        assert_eq!(report.payload_blake3.len(), 64);
    }

    #[test]
    fn a_report_without_the_new_fields_deserializes() {
        let json = r#"{"items_verified":2,"bytes_verified":10,"payload_blake3":"ab","detail":"d"}"#;
        let report: VerificationReport = serde_json::from_str(json).expect("older peer report");
        assert_eq!(report.rows_total(), 0);
        assert_eq!(report.constraints_verified, 0);
        assert!(report.deviations.is_empty());
        assert_eq!(report.extra_lines(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn readback_sink_rejects_persisted_corruption() {
        let expected = evidence(b"source");
        let actual = b"broken";
        let mut sink = VerificationSink::new(&expected);
        sink.send_chunk(7, 0, actual).await.unwrap();
        let error = sink
            .finish_item(7, actual.len() as u64, &crate::wire::blake3_hex(actual))
            .await
            .expect_err("corruption must fail");
        assert!(error.to_string().contains("read-back mismatch"));
    }
}
