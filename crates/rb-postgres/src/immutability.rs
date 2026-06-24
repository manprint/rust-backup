//! Source-immutability fingerprint (plan Phase 2.7).
//!
//! [`fingerprint`] produces a stable digest of the source: a structural catalog
//! hash (estimates/sequence values normalized out, since autovacuum/ANALYZE move
//! them without any user mutation) plus, per data-bearing table, an exact
//! `COUNT(*)` and a deterministic sampled content checksum. The session captures
//! it before and after every run and raises `BackupError::SourceMutated` on any
//! drift (I-IMMUT). All queries are read-only, on a `connect_read_only` session.
//!
//! The digest COMPOSITION is pure and unit-tested; the catalog/COUNT/checksum
//! queries are exercised by the postgres e2e (incl. the mid-`COPY`-abort case).

use rb_core::error::{BackupError, Phase, Result};
use rb_core::wire::blake3_hex;
use tokio_postgres::Client;

use crate::ddl::quote_qualified;
use crate::introspect;
use crate::model::PgPlanPayload;
use crate::{PgConnection, PostgresParams};

/// Max rows hashed per table for the content checksum (deterministic sample).
const CHECKSUM_SAMPLE_ROWS: usize = 10_000;

/// Snapshot the source fingerprint composes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    /// Hash of the structural catalog (estimates normalized out).
    pub catalog_hash: String,
    /// Per-table exact row count + sampled content checksum.
    pub tables: Vec<TableStat>,
}

/// One table's immutability evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStat {
    /// `database.schema.table`.
    pub name: String,
    pub rows: i64,
    /// Hex digest over a deterministic row sample (`""` when empty/unreadable).
    pub checksum: String,
}

/// Compute the source fingerprint (read-only).
pub async fn fingerprint(params: &PostgresParams) -> Result<String> {
    let payload = introspect::introspect_cluster(params).await?;

    let mut normalized = payload.clone();
    normalize(&mut normalized);
    let catalog_hash = hash_catalog(&normalized);

    let mut tables = Vec::new();
    for db in &payload.databases {
        let conn = PgConnection::connect_read_only(params, &db.name).await?;
        for schema in &db.schemas {
            for t in &schema.tables {
                // Child partitions are covered by the parent's COUNT/checksum.
                if t.partition_of.is_some() {
                    continue;
                }
                let stat = table_stat(&conn.client, &db.name, &t.schema, &t.name).await?;
                tables.push(stat);
            }
        }
    }

    Ok(compose(&SourceFingerprint {
        catalog_hash,
        tables,
    }))
}

/// Fold a snapshot into a single deterministic hex digest. Order-independent in
/// the table list (sorted by name) so introspection ordering cannot perturb it.
pub fn compose(snap: &SourceFingerprint) -> String {
    let mut tables = snap.tables.clone();
    tables.sort_by(|a, b| a.name.cmp(&b.name));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rust-backup/pg-fingerprint/v1\n");
    hasher.update(b"catalog:");
    hasher.update(snap.catalog_hash.as_bytes());
    for t in &tables {
        hasher.update(format!("\n{}|{}|{}", t.name, t.rows, t.checksum).as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Zero out fields that drift without user mutation (planner estimates, sequence
/// values) so the structural hash reflects schema/DDL only.
fn normalize(p: &mut PgPlanPayload) {
    for db in &mut p.databases {
        for s in &mut db.schemas {
            for t in &mut s.tables {
                t.estimated_rows = 0;
                t.estimated_bytes = 0;
            }
            for seq in &mut s.sequences {
                seq.last_value = None;
                seq.is_called = false;
            }
        }
    }
}

/// Hash the normalized catalog. `serde_json` serializes struct fields in a fixed
/// order, so this is deterministic.
fn hash_catalog(p: &PgPlanPayload) -> String {
    let bytes = serde_json::to_vec(p).unwrap_or_default();
    blake3_hex(&bytes)
}

/// Exact `COUNT(*)` plus a deterministic sampled content checksum for one table.
async fn table_stat(client: &Client, db: &str, schema: &str, table: &str) -> Result<TableStat> {
    let qual = quote_qualified(schema, table);

    let count_row = client
        .query_one(&format!("SELECT count(*)::int8 FROM {qual}"), &[])
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("count {qual}"), e))?;
    let rows: i64 = count_row.get(0);

    // Deterministic sample: hash each row's text, ordered by that text, capped.
    // Ordering by the row text makes the sample stable across runs when data is
    // unchanged (a bare LIMIT would pick arbitrary rows and falsely "drift").
    let sum_sql = format!(
        "SELECT coalesce(md5(string_agg(h, '')), '') FROM \
         (SELECT md5(t::text) AS h FROM {qual} t ORDER BY t::text LIMIT {CHECKSUM_SAMPLE_ROWS}) s"
    );
    let checksum_row = client
        .query_one(&sum_sql, &[])
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("checksum {qual}"), e))?;
    let checksum: String = checksum_row.get(0);

    Ok(TableStat {
        name: format!("{db}.{schema}.{table}"),
        rows,
        checksum,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> SourceFingerprint {
        SourceFingerprint {
            catalog_hash: "cat".to_string(),
            tables: vec![
                TableStat {
                    name: "d.app.b".to_string(),
                    rows: 2,
                    checksum: "y".to_string(),
                },
                TableStat {
                    name: "d.app.a".to_string(),
                    rows: 1,
                    checksum: "x".to_string(),
                },
            ],
        }
    }

    #[test]
    fn compose_is_deterministic_and_order_independent() {
        let a = snap();
        let mut b = snap();
        b.tables.reverse(); // different input order, same content
        assert_eq!(compose(&a), compose(&b), "table order must not matter");
        assert_eq!(compose(&a), compose(&a));
    }

    #[test]
    fn compose_changes_on_any_drift() {
        let base = compose(&snap());

        let mut more_rows = snap();
        more_rows.tables[0].rows += 1;
        assert_ne!(base, compose(&more_rows), "row count change must show");

        let mut new_checksum = snap();
        new_checksum.tables[0].checksum = "z".to_string();
        assert_ne!(base, compose(&new_checksum), "content change must show");

        let mut new_catalog = snap();
        new_catalog.catalog_hash = "cat2".to_string();
        assert_ne!(base, compose(&new_catalog), "schema change must show");
    }

    #[test]
    fn normalize_zeroes_volatile_estimates() {
        let mut p = crate::model::test_fixture();
        // fixture has non-zero estimates + a sequence last_value.
        assert_ne!(p.databases[0].schemas[0].tables[0].estimated_bytes, 0);
        assert!(p.databases[0].schemas[0].sequences[0].last_value.is_some());

        normalize(&mut p);
        assert_eq!(p.databases[0].schemas[0].tables[0].estimated_bytes, 0);
        assert_eq!(p.databases[0].schemas[0].tables[0].estimated_rows, 0);
        assert!(p.databases[0].schemas[0].sequences[0].last_value.is_none());
    }

    #[test]
    fn catalog_hash_ignores_estimate_drift_but_not_structure() {
        let base = crate::model::test_fixture();
        let mut base_n = base.clone();
        normalize(&mut base_n);
        let h0 = hash_catalog(&base_n);

        // Estimate drift (autovacuum) must NOT change the normalized hash.
        let mut drifted = base.clone();
        drifted.databases[0].schemas[0].tables[0].estimated_bytes += 4096;
        drifted.databases[0].schemas[0].tables[0].estimated_rows += 50;
        normalize(&mut drifted);
        assert_eq!(
            h0,
            hash_catalog(&drifted),
            "estimate drift must not change hash"
        );

        // A real structural change MUST change the hash.
        let mut renamed = base.clone();
        renamed.databases[0].schemas[0].tables[0].name = "renamed".to_string();
        normalize(&mut renamed);
        assert_ne!(h0, hash_catalog(&renamed), "schema change must change hash");
    }
}
