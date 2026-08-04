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
