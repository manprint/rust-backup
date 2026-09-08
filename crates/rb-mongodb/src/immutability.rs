//! Source-immutability fingerprint (plan Phase 3.6).
//!
//! [`fingerprint`] produces a stable digest of the source: a structural hash of
//! the introspected catalog (volatile size/count estimates normalized out, since
//! they drift without any user mutation) plus, per collection, an exact
//! `count_documents` and a complete deterministic `_id`-ordered content checksum. The
//! session captures it before and after every run and raises
//! `BackupError::SourceMutated` on any drift (I-IMMUT). All queries are reads.
//!
//! The digest COMPOSITION is pure and unit-tested; the live count/checksum
//! queries are exercised by the mongodb e2e (incl. the mid-transfer-abort case).

use futures_util::StreamExt;
use mongodb::bson::{doc, Document};

use rb_core::error::{BackupError, Phase, Result};
use rb_core::wire::blake3_hex;

use crate::introspect;
use crate::model::MongoPlanPayload;
use crate::{MongoConnection, MongoDbParams};

/// Snapshot the source fingerprint composes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    /// Hash of the structural catalog (estimates normalized out).
    pub catalog_hash: String,
    /// Per-collection exact doc count + complete content checksum.
    pub collections: Vec<CollStat>,
}

/// One collection's immutability evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollStat {
    /// `database.collection`.
    pub name: String,
    pub docs: i64,
    /// Hex digest over every document in deterministic `_id` order.
    pub checksum: String,
}

/// Compute the source fingerprint (read-only).
pub async fn fingerprint(params: &MongoDbParams) -> Result<String> {
    let payload = introspect::introspect_cluster(params).await?;

    let mut normalized = payload.clone();
    normalize(&mut normalized);
    let catalog_hash = hash_catalog(&normalized);

    let conn = MongoConnection::connect(params).await?;
    let mut collections = Vec::new();
    for db in &payload.databases {
        for coll in &db.collections {
            collections.push(coll_stat(&conn, &db.name, &coll.name).await?);
        }
    }

    Ok(compose(&SourceFingerprint {
        catalog_hash,
        collections,
    }))
}

/// Fold a snapshot into a single deterministic hex digest. Order-independent in
/// the collection list (sorted by name) so introspection ordering cannot
/// perturb it.
pub fn compose(snap: &SourceFingerprint) -> String {
    let mut colls = snap.collections.clone();
    colls.sort_by(|a, b| a.name.cmp(&b.name));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rust-backup/mongo-fingerprint/v1\n");
    hasher.update(b"catalog:");
    hasher.update(snap.catalog_hash.as_bytes());
    for c in &colls {
        hasher.update(format!("\n{}|{}|{}", c.name, c.docs, c.checksum).as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Zero out fields that drift without user mutation (size/count estimates) so
/// the structural hash reflects collections/options/indexes only.
fn normalize(p: &mut MongoPlanPayload) {
    for db in &mut p.databases {
        // The user probe is best-effort: `usersInfo` can fail transiently (an
        // election, a step-down, a momentary auth error) and then yields an
        // empty list. Hashing it made such a blip look like a source mutation —
        // the one error class documented as "must never happen" — on a source
        // nothing had touched. Users are never restored either, so they have no
        // business in the immutability digest.
        db.users.clear();
        for c in &mut db.collections {
            c.estimated_docs = 0;
            c.estimated_bytes = 0;
        }
    }
}

/// Hash the normalized catalog. `serde` serializes struct fields in a fixed
/// order, so this is deterministic.
fn hash_catalog(p: &MongoPlanPayload) -> String {
    let bytes = serde_json::to_vec(p).unwrap_or_default();
    blake3_hex(&bytes)
}

/// Exact `count_documents` plus a complete deterministic content checksum.
async fn coll_stat(conn: &MongoConnection, db: &str, coll: &str) -> Result<CollStat> {
    let collection = conn.client.database(db).collection::<Document>(coll);

    let docs = collection
        .count_documents(doc! {})
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("count {db}.{coll}"), e))?
        as i64;

    // `_id` is unique and indexed, so hashing every document in this order is
    // stable and cannot miss a mutation in a collection larger than a sample.
    let mut cursor = collection
        .find(doc! {})
        .sort(doc! { "_id": 1 })
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("sample {db}.{coll}"), e))?;

    let mut hasher = blake3::Hasher::new();
    while let Some(item) = cursor.next().await {
        let d = item
            .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("read {db}.{coll}"), e))?;
        let bytes = mongodb::bson::to_vec(&d)
            .map_err(|e| BackupError::phase_src(Phase::Analyze, "encode bson", e))?;
        hasher.update(&bytes);
    }

    Ok(CollStat {
        name: format!("{db}.{coll}"),
        docs,
        checksum: hasher.finalize().to_hex().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> SourceFingerprint {
        SourceFingerprint {
            catalog_hash: "cat".to_string(),
            collections: vec![
                CollStat {
                    name: "d.b".to_string(),
                    docs: 2,
                    checksum: "y".to_string(),
                },
                CollStat {
                    name: "d.a".to_string(),
                    docs: 1,
                    checksum: "x".to_string(),
                },
            ],
        }
    }

    #[test]
    fn compose_is_deterministic_and_order_independent() {
        let a = snap();
        let mut b = snap();
        b.collections.reverse();
        assert_eq!(compose(&a), compose(&b), "collection order must not matter");
        assert_eq!(compose(&a), compose(&a));
    }

    #[test]
    fn compose_changes_on_any_drift() {
        let base = compose(&snap());

        let mut more = snap();
        more.collections[0].docs += 1;
        assert_ne!(base, compose(&more), "doc count change must show");

        let mut content = snap();
        content.collections[0].checksum = "z".to_string();
        assert_ne!(base, compose(&content), "content change must show");

        let mut cat = snap();
        cat.catalog_hash = "cat2".to_string();
        assert_ne!(base, compose(&cat), "schema change must show");
    }

    #[test]
    fn normalize_zeroes_volatile_estimates() {
        let mut p = crate::model::test_fixture();
        assert_ne!(p.databases[0].collections[0].estimated_bytes, 0);
        assert_ne!(p.databases[0].collections[0].estimated_docs, 0);

        normalize(&mut p);
        assert_eq!(p.databases[0].collections[0].estimated_bytes, 0);
        assert_eq!(p.databases[0].collections[0].estimated_docs, 0);
    }

    #[test]
    fn catalog_hash_ignores_estimate_drift_but_not_structure() {
        let base = crate::model::test_fixture();
        let mut base_n = base.clone();
        normalize(&mut base_n);
        let h0 = hash_catalog(&base_n);

        // Estimate drift must NOT change the normalized hash.
        let mut drifted = base.clone();
        drifted.databases[0].collections[0].estimated_bytes += 4096;
        drifted.databases[0].collections[0].estimated_docs += 50;
        normalize(&mut drifted);
        assert_eq!(
            h0,
            hash_catalog(&drifted),
            "estimate drift must not change hash"
        );

        // A real structural change MUST change the hash.
        let mut renamed = base.clone();
        renamed.databases[0].collections[0].name = "renamed".to_string();
        normalize(&mut renamed);
        assert_ne!(h0, hash_catalog(&renamed), "schema change must change hash");
    }
}
