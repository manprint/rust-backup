//! Source-immutability fingerprint (plan Phase 2.7).
//!
//! [`fingerprint`] produces a stable digest of the source: a structural catalog
//! hash (planner estimates normalized out, since autovacuum/ANALYZE moves them
//! without user mutation) plus, per data-bearing table, an order-independent
//! 128-bit commitment over its rows and the number of rows folded into it. The
//! session captures
//! it before and after every run and raises `BackupError::SourceMutated` on any
//! drift (I-IMMUT). All queries are read-only, on a `connect_read_only` session.
//!
//! The digest COMPOSITION is pure and unit-tested; the catalog/COUNT/checksum
//! queries are exercised by the postgres e2e (incl. the mid-`COPY`-abort case).

use futures_util::StreamExt;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::wire::blake3_hex;
use tokio_postgres::Client;

use crate::connect::SourcePool;
use crate::ddl::quote_qualified;
use crate::introspect;
use crate::model::PgPlanPayload;

/// Snapshot the source fingerprint composes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    /// Hash of the structural catalog (estimates normalized out).
    pub catalog_hash: String,
    /// Per-table exact row count + complete content checksum.
    pub tables: Vec<TableStat>,
}

/// One table's immutability evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStat {
    /// `database.schema.table`.
    pub name: String,
    /// Rows folded into the commitment — the exact count, taken from the same
    /// scan rather than from a second `COUNT(*)`.
    pub rows: u64,
    /// Order-independent commitment: the wrapping 128-bit sum of `md5(row)`.
    /// Addition and not XOR, because XOR cancels a duplicated row and would
    /// read `{A, A, C}` and `{B, B, C}` as the same table.
    pub commitment: u128,
}

/// Folds a stream of `md5(row)` lines into `(rows, commitment)`.
///
/// The server streams the hashes unordered and unaggregated, so nothing sorts
/// and nothing spills to `pgsql_tmp`; the commutative fold is what makes the
/// result independent of the order the rows arrive in. A line can be split
/// across two `COPY` frames, so the tail of a frame is carried over.
#[derive(Default)]
struct RowCommitment {
    sum: u128,
    rows: u64,
    residual: Vec<u8>,
}

impl RowCommitment {
    fn update(&mut self, chunk: &[u8], table: &str) -> Result<()> {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            let (line, tail) = rest.split_at(end);
            if self.residual.is_empty() {
                self.fold(line, table)?;
            } else {
                let mut whole = std::mem::take(&mut self.residual);
                whole.extend_from_slice(line);
                self.fold(&whole, table)?;
            }
            rest = &tail[1..];
        }
        self.residual.extend_from_slice(rest);
        Ok(())
    }

    fn fold(&mut self, line: &[u8], table: &str) -> Result<()> {
        let text = std::str::from_utf8(line)
            .map_err(|_| malformed_row_hash(table))?
            .trim_end_matches('\r');
        if text.is_empty() {
            return Ok(());
        }
        let hash = u128::from_str_radix(text, 16).map_err(|_| malformed_row_hash(table))?;
        self.sum = self.sum.wrapping_add(hash);
        self.rows += 1;
        Ok(())
    }

    fn finish(mut self, table: &str) -> Result<(u64, u128)> {
        if !self.residual.is_empty() {
            let residual = std::mem::take(&mut self.residual);
            self.fold(&residual, table)?;
        }
        Ok((self.rows, self.sum))
    }
}

/// A row hash that is not 32 hex digits means the stream is not what we asked
/// for: report it against the table it came from rather than fold garbage.
fn malformed_row_hash(table: &str) -> BackupError {
    BackupError::Integrity(format!(
        "source fingerprint: {table} produced a row hash that is not hexadecimal"
    ))
}

/// Compute the source fingerprint (read-only).
pub async fn fingerprint(pool: &SourcePool) -> Result<String> {
    let payload = introspect::introspect_cluster(pool).await?;

    let mut normalized = payload.clone();
    normalize(&mut normalized);
    let catalog_hash = hash_catalog(&normalized);

    let mut tables = Vec::new();
    for db in &payload.databases {
        let conn = pool.get(Some(&db.name)).await?;
        // Classic inheritance children hold their own rows and are counted as
        // their own tables, so the parent is measured with `ONLY` — exactly the
        // scope its plan item streams.
        let inheritance_parents: std::collections::HashSet<&str> = db
            .schemas
            .iter()
            .flat_map(|s| s.tables.iter())
            .flat_map(|t| t.inherits.iter().map(|p| p.as_str()))
            .collect();
        for schema in &db.schemas {
            for t in &schema.tables {
                // Child partitions are covered by the parent's COUNT/checksum.
                if t.partition_of.is_some() {
                    continue;
                }
                let qual = format!("{}.{}", t.schema, t.name);
                let only = t.kind != "p" && inheritance_parents.contains(qual.as_str());
                let stat =
                    table_stat(&conn.client, &db.name, &t.schema, &t.name, only, None).await?;
                tables.push(stat);
            }
        }
        // Extension configuration tables hold user rows the plan streams, so
        // they are part of the source's evidence too — restricted to the same
        // scope the item streams.
        for config in &db.extension_configs {
            let stat = table_stat(
                &conn.client,
                &db.name,
                &config.schema,
                &config.table,
                false,
                config.condition.as_deref(),
            )
            .await?;
            tables.push(stat);
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
    hasher.update(b"rust-backup/pg-fingerprint/v2\n");
    hasher.update(b"catalog:");
    hasher.update(snap.catalog_hash.as_bytes());
    for t in &tables {
        hasher.update(format!("\n{}\t{}\t{:032x}", t.name, t.rows, t.commitment).as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Zero out planner estimates that drift without user mutation. Sequence state
/// is retained because it is restored state and any change is real source drift.
fn normalize(p: &mut PgPlanPayload) {
    for db in &mut p.databases {
        for config in &mut db.extension_configs {
            config.estimated_bytes = 0;
            config.expected_rows = None;
        }
        for s in &mut db.schemas {
            for v in &mut s.views {
                v.expected_rows = None;
            }
            for t in &mut s.tables {
                t.estimated_rows = 0;
                t.estimated_bytes = 0;
                t.expected_rows = None;
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

/// Row count and order-independent row commitment for one table, from a
/// single streaming scan.
async fn table_stat(
    client: &Client,
    db: &str,
    schema: &str,
    table: &str,
    only: bool,
    condition: Option<&str>,
) -> Result<TableStat> {
    let qual = quote_qualified(schema, table);
    let scope = if only {
        format!("ONLY {qual}")
    } else {
        qual.clone()
    };
    // An extension configuration table's condition (`WHERE …`, verbatim from
    // `pg_extension.extcondition`); empty for an ordinary table.
    let cond = condition.map(|c| format!(" {c}")).unwrap_or_default();

    // No `ORDER BY` and no aggregate: the server streams one hash per row, the
    // fold happens here, and neither side ever holds the table — so the source
    // writes no temporary file on our behalf (I-NOTEMP). The row count comes
    // out of the same scan instead of a second one.
    let copy_sql = format!("COPY (SELECT md5(t::text) FROM {scope} t{cond}) TO STDOUT");
    let stream = client
        .copy_out(&copy_sql)
        .await
        .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("checksum {qual}"), e))?;
    futures_util::pin_mut!(stream);
    let name = format!("{db}.{schema}.{table}");
    let mut fold = RowCommitment::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| BackupError::phase_src(Phase::Analyze, format!("checksum {qual}"), e))?;
        fold.update(&chunk, &name)?;
    }
    let (rows, commitment) = fold.finish(&name)?;

    Ok(TableStat {
        name,
        rows,
        commitment,
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
                    commitment: 0x2222,
                },
                TableStat {
                    name: "d.app.a".to_string(),
                    rows: 1,
                    commitment: 0x1111,
                },
            ],
        }
    }

    fn fold_rows(table: &str, rows: &[&str]) -> Result<(u64, u128)> {
        let mut fold = RowCommitment::default();
        for row in rows {
            fold.update(format!("{row}\n").as_bytes(), table)?;
        }
        fold.finish(table)
    }

    fn md5_like(seed: u8) -> String {
        format!("{:032x}", u128::from(seed) * 0x0123_4567_89ab_cdef)
    }

    #[test]
    fn row_commitment_is_order_independent() {
        let (a, b, c) = (md5_like(1), md5_like(2), md5_like(3));
        let forward = fold_rows("t", &[&a, &b, &c]).expect("fold");
        let shuffled = fold_rows("t", &[&c, &a, &b]).expect("fold");
        assert_eq!(forward, shuffled);
    }

    #[test]
    fn row_commitment_detects_single_row_change() {
        let (a, b, c) = (md5_like(1), md5_like(2), md5_like(3));
        let base = fold_rows("t", &[&a, &b, &c]).expect("fold");
        let changed = fold_rows("t", &[&a, &b, &md5_like(4)]).expect("fold");
        assert_ne!(base, changed);
    }

    #[test]
    fn row_commitment_detects_duplicate_swap() {
        // XOR would fold {A, A, C} and {B, B, C} to the same value: both pairs
        // cancel. The wrapping sum does not, which is why it is the operation.
        let (a, b, c) = (md5_like(1), md5_like(2), md5_like(3));
        let left = fold_rows("t", &[&a, &a, &c]).expect("fold");
        let right = fold_rows("t", &[&b, &b, &c]).expect("fold");
        assert_ne!(left, right);
    }

    #[test]
    fn row_commitment_counts_every_row() {
        let a = md5_like(7);
        let (rows, _) = fold_rows("t", &[&a, &a, &a]).expect("fold");
        assert_eq!(rows, 3, "duplicate rows are still rows");
    }

    #[test]
    fn row_commitment_survives_a_line_split_across_frames() {
        let a = md5_like(9);
        let whole = fold_rows("t", &[&a]).expect("fold");
        let mut fold = RowCommitment::default();
        let line = format!("{a}\n");
        let (head, tail) = line.as_bytes().split_at(7);
        fold.update(head, "t").expect("head");
        fold.update(tail, "t").expect("tail");
        assert_eq!(fold.finish("t").expect("fold"), whole);
    }

    #[test]
    fn row_commitment_rejects_malformed_line() {
        let error = fold_rows("d.app.t", &["not-a-hash"]).expect_err("must refuse");
        assert!(
            matches!(error, BackupError::Integrity(ref m) if m.contains("d.app.t")),
            "{error:?}"
        );
    }

    #[test]
    fn fingerprint_prefix_is_v2() {
        // The prefix is what keeps a v1 digest from ever comparing equal to a
        // v2 one; assert it through a known composition rather than by reading
        // the constant back.
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rust-backup/pg-fingerprint/v2\n");
        hasher.update(b"catalog:");
        hasher.update(b"cat");
        hasher.update(b"\nd.app.a\t1\t00000000000000000000000000001111");
        hasher.update(b"\nd.app.b\t2\t00000000000000000000000000002222");
        assert_eq!(compose(&snap()), hasher.finalize().to_hex().to_string());
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

        let mut new_commitment = snap();
        new_commitment.tables[0].commitment = 0x3333;
        assert_ne!(base, compose(&new_commitment), "content change must show");

        let mut new_catalog = snap();
        new_catalog.catalog_hash = "cat2".to_string();
        assert_ne!(base, compose(&new_catalog), "schema change must show");
    }

    #[test]
    fn normalize_zeroes_volatile_estimates() {
        let mut p = crate::model::test_fixture();
        // Planner estimates are volatile; sequence state is not.
        assert_ne!(p.databases[0].schemas[0].tables[0].estimated_bytes, 0);
        let sequence_value = p.databases[0].schemas[0].sequences[0].last_value;

        normalize(&mut p);
        assert_eq!(p.databases[0].schemas[0].tables[0].estimated_bytes, 0);
        assert_eq!(p.databases[0].schemas[0].tables[0].estimated_rows, 0);
        assert_eq!(
            p.databases[0].schemas[0].sequences[0].last_value,
            sequence_value
        );
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
