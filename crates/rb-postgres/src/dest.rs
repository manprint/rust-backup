//! Destination-side preflight (plan Phase 2.5).
//!
//! Validates that the destination can perform the restore BEFORE any bytes flow:
//! server major ≥ source major, admin can create roles + databases, and target
//! databases are absent (or `--overwrite`). The decision logic is a pure function
//! ([`assess`]) of a [`DestProbe`] gathered over an ADMIN connection, so it is
//! unit-tested without a live DB; only [`probe_dest`] touches the server.
//!
//! Free-disk space is intentionally NOT a hard check: a SQL connection cannot see
//! the destination host's filesystem, so the size is reported informationally.

use std::collections::HashSet;

use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{human_bytes, BackupPlan, Preflight};

use crate::model::PgPlanPayload;
use crate::{PgConnection, PostgresParams};

/// Facts gathered from the destination needed to assess the plan.
#[derive(Debug, Clone, Default)]
pub struct DestProbe {
    /// Destination server major version.
    pub dest_major: u32,
    /// Connected role is a superuser (implies all creation privileges).
    pub is_super: bool,
    pub createdb: bool,
    pub createrole: bool,
    /// Target database names that already exist on the destination.
    pub existing_databases: HashSet<String>,
}

/// Preflight the plan against the destination (ADMIN connection).
pub async fn validate(params: &PostgresParams, plan: &BackupPlan) -> Result<Preflight> {
    let payload: PgPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Validate, format!("bad plan payload: {e}")))?;
    let probe = probe_dest(params, &payload).await?;
    Ok(assess(
        &payload,
        &probe,
        params.overwrite,
        plan.estimated_bytes,
    ))
}

/// Pure assessment: fold the probe + plan into a [`Preflight`].
pub fn assess(
    payload: &PgPlanPayload,
    probe: &DestProbe,
    overwrite: bool,
    estimated_bytes: u64,
) -> Preflight {
    let mut pf = Preflight::pass();

    let version_ok = probe.dest_major >= payload.server_major;
    pf = pf.check(
        "server_version",
        version_ok,
        format!(
            "destination major {} vs source major {} (need destination ≥ source)",
            probe.dest_major, payload.server_major
        ),
    );

    let privileges_ok = probe.is_super || (probe.createdb && probe.createrole);
    pf = pf.check(
        "privileges",
        privileges_ok,
        if privileges_ok {
            "admin can create roles and databases".to_string()
        } else {
            "admin needs SUPERUSER, or both CREATEDB and CREATEROLE".to_string()
        },
    );

    for db in &payload.databases {
        let exists = probe.existing_databases.contains(&db.name);
        let ok = !exists || overwrite;
        let detail = if !exists {
            format!("'{}' absent — will be created", db.name)
        } else if overwrite {
            format!("'{}' exists — will be overwritten (--overwrite)", db.name)
        } else {
            format!("'{}' already exists (use --overwrite to replace)", db.name)
        };
        pf = pf.check(format!("database:{}", db.name), ok, detail);
    }

    // Informational only: free disk is not visible over a SQL connection.
    pf = pf.check(
        "estimated_size",
        true,
        format!(
            "≈ {} to restore; destination free space is not verifiable over SQL",
            human_bytes(estimated_bytes)
        ),
    );

    pf
}

async fn probe_dest(params: &PostgresParams, payload: &PgPlanPayload) -> Result<DestProbe> {
    let conn = PgConnection::connect(params, &params.bootstrap_database()).await?;

    let row = conn
        .client
        .query_one(
            "SELECT rolsuper, rolcreatedb, rolcreaterole \
             FROM pg_catalog.pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .map_err(|e| BackupError::phase_src(Phase::Validate, "probe current_user privileges", e))?;

    let names: Vec<String> = payload.databases.iter().map(|d| d.name.clone()).collect();
    let existing = conn
        .client
        .query(
            "SELECT datname::text FROM pg_catalog.pg_database WHERE datname = ANY($1)",
            &[&names],
        )
        .await
        .map_err(|e| BackupError::phase_src(Phase::Validate, "probe existing databases", e))?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();

    Ok(DestProbe {
        dest_major: conn.server_major,
        is_super: row.get(0),
        createdb: row.get(1),
        createrole: row.get(2),
        existing_databases: existing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(major: u32, is_super: bool, createdb: bool, createrole: bool) -> DestProbe {
        DestProbe {
            dest_major: major,
            is_super,
            createdb,
            createrole,
            existing_databases: HashSet::new(),
        }
    }

    #[test]
    fn passes_on_clean_destination() {
        let payload = crate::model::test_fixture(); // source major 16, db "appdb"
        let pf = assess(&payload, &probe(16, false, true, true), false, 1024);
        assert!(
            pf.ok,
            "clean newer destination should pass: {:?}",
            pf.checks
        );
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "database:appdb" && c.passed));
    }

    #[test]
    fn fails_when_destination_older() {
        let payload = crate::model::test_fixture();
        let pf = assess(&payload, &probe(15, true, true, true), false, 0);
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "server_version" && !c.passed));
    }

    #[test]
    fn fails_without_creation_privileges() {
        let payload = crate::model::test_fixture();
        // not super, missing CREATEROLE
        let pf = assess(&payload, &probe(16, false, true, false), false, 0);
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "privileges" && !c.passed));
        // superuser alone suffices.
        let pf2 = assess(&payload, &probe(16, true, false, false), false, 0);
        assert!(pf2
            .checks
            .iter()
            .any(|c| c.name == "privileges" && c.passed));
    }

    #[test]
    fn existing_database_blocks_unless_overwrite() {
        let payload = crate::model::test_fixture();
        let mut p = probe(16, true, true, true);
        p.existing_databases.insert("appdb".to_string());

        let blocked = assess(&payload, &p, false, 0);
        assert!(!blocked.ok, "existing db must block without --overwrite");
        assert!(blocked
            .checks
            .iter()
            .any(|c| c.name == "database:appdb" && !c.passed));

        let allowed = assess(&payload, &p, true, 0);
        assert!(
            allowed.ok,
            "--overwrite must allow restoring over existing db"
        );
    }
}
