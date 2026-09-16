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

use std::collections::{HashMap, HashSet};
use std::pin::Pin;

use bytes::Bytes;
use futures_util::SinkExt;
use tokio_postgres::{Client, CopyInSink};

use rb_core::channel::{ChunkEvent, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{human_bytes, BackupPlan, Preflight};

use crate::ddl::{build_cluster_ddl, create_database, quote_ident, quote_qualified};
use crate::model::PgPlanPayload;
use crate::source::{ItemMeta, DATA_ITEM_KINDS};
use crate::{ExtensionVersionPolicy, PgConnection, PostgresParams};

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
    /// `(extension, version)` pairs the destination can install, from
    /// `pg_available_extension_versions`.
    pub available_extensions: Vec<(String, String)>,
}

/// Preflight the plan against the destination (ADMIN connection).
pub async fn validate(params: &PostgresParams, plan: &BackupPlan) -> Result<Preflight> {
    let payload: PgPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Validate, format!("bad plan payload: {e}")))?;
    check_item_kinds(plan)?;
    let probe = probe_dest(params, &payload).await?;
    Ok(assess(
        &payload,
        &probe,
        params.overwrite,
        plan.estimated_bytes,
        params.extension_version,
    ))
}

/// Pure assessment: fold the probe + plan into a [`Preflight`].
pub fn assess(
    payload: &PgPlanPayload,
    probe: &DestProbe,
    overwrite: bool,
    estimated_bytes: u64,
    extension_version: ExtensionVersionPolicy,
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

        match create_database(db, probe.dest_major) {
            Ok(_) => {
                pf = pf.check(
                    format!("database_locale:{}", db.name),
                    true,
                    format!(
                        "encoding and locale can be recreated on PostgreSQL {}",
                        probe.dest_major
                    ),
                );
            }
            Err(error) => {
                pf = pf.check(
                    format!("database_locale:{}", db.name),
                    false,
                    error.to_string(),
                );
            }
        }
    }

    // An extension the destination cannot install fails the restore halfway
    // through `pre_data`, after roles and databases have been created. The same
    // is true of a version it does not carry: `CREATE EXTENSION … VERSION '1.0'`
    // is an error, not a downgrade. Both are decided here, before anything is
    // written.
    for db in &payload.databases {
        for extension in &db.extensions {
            let exact = probe
                .available_extensions
                .iter()
                .any(|(name, version)| name == &extension.name && version == &extension.version);
            let any_version: Vec<&str> = probe
                .available_extensions
                .iter()
                .filter(|(name, _)| name == &extension.name)
                .map(|(_, version)| version.as_str())
                .collect();
            let relaxed = extension_version == ExtensionVersionPolicy::Default;
            let ok = exact || (relaxed && !any_version.is_empty());
            let detail = if exact {
                format!(
                    "extension {} version {} is available",
                    extension.name, extension.version
                )
            } else if ok {
                format!(
                    "extension {} version {} is not available; will install the destination's \
                     default version (--extension-version default)",
                    extension.name, extension.version
                )
            } else {
                let available = if any_version.is_empty() {
                    "none".to_string()
                } else {
                    any_version.join(", ")
                };
                format!(
                    "extension {} version {} is not available on the destination (available: \
                     {available}); install it or pass --extension-version default",
                    extension.name, extension.version
                )
            };
            pf = pf.check(format!("extension:{}", extension.name), ok, detail);
        }
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

    let available_extensions = conn
        .client
        .query(
            "SELECT name::text, version::text FROM pg_catalog.pg_available_extension_versions \
             ORDER BY 1, 2",
            &[],
        )
        .await
        .map_err(|e| BackupError::phase_src(Phase::Validate, "probe available extensions", e))?
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect();

    Ok(DestProbe {
        dest_major: conn.server_major,
        is_super: row.get(0),
        createdb: row.get(1),
        createrole: row.get(2),
        existing_databases: existing,
        available_extensions,
    })
}

// --- restore (apply) ---------------------------------------------------------

/// Apply the streamed payload to reach 1:1 with the source (plan Phase 2.6):
/// roles → databases (cluster scope) → per-db structure (`pre_data`) → bulk data
/// via `COPY … FROM STDIN (FORMAT binary)` → per-db `post_data` (constraints,
/// indexes, sequence values, grants, owners). No temp files.
///
/// `CREATE DATABASE` cannot run inside a transaction, so cluster-scope DDL is
/// autocommit. Per-database apply is autocommit too in this phase; wrapping each
/// database in a transaction is a documented refinement. Verified end-to-end by
/// the postgres e2e (live restore is not runnable in a DB-less CI).
pub async fn stream_in(
    params: &PostgresParams,
    plan: &BackupPlan,
    src: &mut dyn ChunkSource,
) -> Result<RestoredRows> {
    let payload: PgPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Apply, format!("bad plan payload: {e}")))?;

    // 1. Cluster scope: roles, then databases.
    // When overwriting `postgres` (or another configured bootstrap database),
    // run cluster DDL from a database that is not itself being replaced.
    let bootstrap = restore_bootstrap_database(params, &payload);
    let boot = PgConnection::connect(params, &bootstrap).await?;
    let ddl = build_cluster_ddl(&payload, boot.server_major, params.extension_version)?;
    run_statements(&boot.client, &ddl.roles).await?;
    if params.overwrite {
        for database in &payload.databases {
            drop_database_for_overwrite(&boot.client, &database.name).await?;
        }
    }

    // Snapshot which target databases exist *before* this run creates any, so a
    // later failure removes exactly what this run brought into existence and
    // never a database it found already there. The bootstrap connection is held
    // open across the load for this reason alone: the rollback needs a working
    // admin session on a database that is not itself being restored, and
    // reconnecting is precisely what a dying destination cannot do.
    let preexisting = existing_target_databases(&boot.client, &payload).await?;
    let outcome = async {
        run_statements(&boot.client, &ddl.databases).await?;
        restore_databases(params, &ddl, &payload, plan, src).await
    }
    .await;
    match outcome {
        Err(error) => {
            remove_partially_restored(&boot.client, &payload, &preexisting).await;
            Err(error)
        }
        Ok(rows) => Ok(rows),
    }
}

/// What the restore actually wrote, per origin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoredRows {
    /// Rows written by `COPY … FROM STDIN`.
    pub table_rows: u64,
    /// Rows a `REFRESH MATERIALIZED VIEW` produced on this side.
    pub derived_rows: u64,
}

/// Steps 2–4 of the restore. Split out of [`stream_in`] so every `?` funnels
/// through one error path — and so `conns` is dropped before the caller tries to
/// `DROP DATABASE` anything, which PostgreSQL refuses while a session is open.
async fn restore_databases(
    params: &PostgresParams,
    ddl: &crate::ddl::ClusterDdl,
    payload: &PgPlanPayload,
    plan: &BackupPlan,
    src: &mut dyn ChunkSource,
) -> Result<RestoredRows> {
    // 2. Per-database structure; keep each connection for the data load.
    let mut conns: HashMap<String, PgConnection> = HashMap::new();
    for dbddl in &ddl.per_database {
        let conn = PgConnection::connect(params, &dbddl.name).await?;
        run_statements(&conn.client, &dbddl.pre_data).await?;
        conns.insert(dbddl.name.clone(), conn);
    }

    // 3. Bulk data: one linear pass over the chunk stream, each item routed to a
    //    COPY … FROM STDIN on its database connection.
    let metas = item_metas(plan)?;
    let table_rows = apply_data(&conns, &metas, src).await?;

    // 4. Per-database post_data.
    for dbddl in &ddl.per_database {
        if let Some(conn) = conns.get(&dbddl.name) {
            run_statements(&conn.client, &dbddl.post_data).await?;
        }
    }

    // 5. A populated materialized view's rows are produced here, by the
    //    `REFRESH` in post_data, not streamed — so the source's count is the
    //    only evidence that the refresh reproduced them.
    let derived_rows = check_matview_rows(&conns, payload).await?;
    Ok(RestoredRows {
        table_rows,
        derived_rows,
    })
}

/// Count every populated materialized view on the destination and compare it
/// with the count the source took.
async fn check_matview_rows(
    conns: &HashMap<String, PgConnection>,
    payload: &PgPlanPayload,
) -> Result<u64> {
    let mut total: u64 = 0;
    for db in &payload.databases {
        let Some(conn) = conns.get(&db.name) else {
            continue;
        };
        for schema in &db.schemas {
            for view in &schema.views {
                let Some(expected) = view.expected_rows else {
                    continue;
                };
                let sql = crate::introspect::count_sql(&view.schema, &view.name, false, None);
                let row = conn.client.query_one(&sql, &[]).await.map_err(|e| {
                    BackupError::phase_src(
                        Phase::Verify,
                        format!(
                            "count rows of materialized view {}.{}",
                            view.schema, view.name
                        ),
                        e,
                    )
                })?;
                let actual = row.get::<_, i64>(0).max(0) as u64;
                check_expected_rows(
                    &format!("materialized view {}.{}", view.schema, view.name),
                    Some(expected.max(0) as u64),
                    actual,
                )?;
                total = total.saturating_add(actual);
            }
        }
    }
    Ok(total)
}

/// Target database names that already exist on the destination.
async fn existing_target_databases(
    client: &Client,
    payload: &PgPlanPayload,
) -> Result<HashSet<String>> {
    let mut existing = HashSet::new();
    for database in &payload.databases {
        let present: bool = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
                &[&database.name],
            )
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Apply,
                    format!("check existing database {:?}", database.name),
                    error,
                )
            })?
            .get(0);
        if present {
            existing.insert(database.name.clone());
        }
    }
    Ok(existing)
}

/// Databases a failed restore must remove: the plan's targets that this run
/// created. A target the run found already present is never touched — without
/// `--overwrite` the run never wrote to it, and with `--overwrite` it was dropped
/// before this snapshot was taken, so it cannot appear here.
fn databases_to_remove<'a>(
    targets: impl IntoIterator<Item = &'a str>,
    preexisting: &HashSet<String>,
) -> Vec<&'a str> {
    targets
        .into_iter()
        .filter(|name| !preexisting.contains(*name))
        .collect()
}

/// Remove the half-restored databases this run created. A restore that fails
/// mid-load would otherwise leave a database holding part of the source's rows:
/// nothing certifies it (there is no `RESTORE VERIFIED`), but it is
/// indistinguishable from a small database on inspection, and the filesystem
/// module already deletes its partial file rather than leave one looking
/// complete. Best-effort by construction — the caller's error is the real
/// outcome and must not be replaced by a cleanup failure — so every problem here
/// is logged loudly and names the manual `DROP DATABASE` that finishes the job.
async fn remove_partially_restored(
    client: &Client,
    payload: &PgPlanPayload,
    preexisting: &HashSet<String>,
) {
    let targets = payload.databases.iter().map(|db| db.name.as_str());
    for name in databases_to_remove(targets, preexisting) {
        match drop_database_for_overwrite(client, name).await {
            Ok(()) => tracing::warn!(
                database = %name,
                "restore failed; removed the partially restored database this run created"
            ),
            Err(error) => tracing::error!(
                database = %name,
                %error,
                "restore failed and the partially restored database could not be removed; \
                 it holds an incomplete copy and no verification evidence — drop it with \
                 DROP DATABASE before retrying"
            ),
        }
    }
}

/// Re-introspect every restored database plus the selected cluster-global
/// objects and compare them with the source plan. Planner estimates and server
/// version strings are normalized because they are not restored state.
pub async fn verify_catalog(params: &PostgresParams, plan: &BackupPlan) -> Result<CatalogProof> {
    let mut expected: PgPlanPayload = serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(Phase::Verify, format!("bad plan payload: {e}")))?;
    let role_names: HashSet<_> = expected
        .roles
        .iter()
        .map(|role| role.name.clone())
        .collect();
    let tablespace_names: HashSet<_> = expected
        .tablespaces
        .iter()
        .map(|tablespace| tablespace.name.clone())
        .collect();

    let mut actual = PgPlanPayload::default();
    let mut destination_major = expected.server_major;
    for (index, expected_db) in expected.databases.iter().enumerate() {
        let mut scoped = params.clone();
        scoped.database = Some(expected_db.name.clone());
        let mut observed = crate::introspect::introspect_cluster(&scoped)
            .await
            .map_err(|error| {
                BackupError::phase_src(
                    Phase::Verify,
                    format!("re-introspect PostgreSQL database {}", expected_db.name),
                    error,
                )
            })?;
        if index == 0 {
            destination_major = observed.server_major;
            actual.server_version = expected.server_version.clone();
            actual.server_major = expected.server_major;
            actual.roles = observed
                .roles
                .into_iter()
                .filter(|role| role_names.contains(&role.name))
                .collect();
            actual.memberships = observed
                .memberships
                .into_iter()
                .filter(|membership| {
                    role_names.contains(&membership.role) && role_names.contains(&membership.member)
                })
                .collect();
            actual.tablespaces = observed
                .tablespaces
                .into_iter()
                .filter(|tablespace| tablespace_names.contains(&tablespace.name))
                .collect();
        }
        let database = observed.databases.pop().ok_or_else(|| {
            BackupError::phase(
                Phase::Verify,
                format!(
                    "restored PostgreSQL database {:?} is missing",
                    expected_db.name
                ),
            )
        })?;
        actual.databases.push(database);
    }

    let source_major = expected.server_major;
    if destination_major != source_major {
        restate_view_definitions(params, &mut expected, destination_major).await?;
        strip_privileges_newer_than(&mut expected, source_major);
        strip_privileges_newer_than(&mut actual, source_major);
    }
    normalize_catalog(&mut expected);
    normalize_catalog(&mut actual);
    // Under `--extension-version default` the restored version is allowed to
    // differ from the source's — and only that. Every other extension field
    // still has to match, so the comparison keeps the name and schema and
    // reports each substitution as a named deviation rather than hiding it.
    let deviations = reconcile_extension_versions(&expected, &mut actual, params.extension_version);
    if actual != expected {
        let differences = catalog_differences(&expected, &actual);
        let counted = if differences.len() >= MAX_REPORTED_DIFFERENCES {
            format!("first {MAX_REPORTED_DIFFERENCES} differences")
        } else if differences.len() == 1 {
            "1 difference".to_string()
        } else {
            format!("{} differences", differences.len())
        };
        return Err(BackupError::phase(
            Phase::Verify,
            format!(
                "PostgreSQL catalog read-back differs from the source plan ({counted}):\n  - {}",
                differences.join("\n  - ")
            ),
        ));
    }
    Ok(CatalogProof {
        constraints: count_constraints(&expected),
        constraints_not_valid: count_constraints_not_valid(&expected),
        deviations,
    })
}

/// What the catalog read-back established, beyond "the two payloads are equal".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogProof {
    /// Integrity constraints compared on both sides.
    pub constraints: u64,
    /// How many of them are `NOT VALID` on both sides.
    pub constraints_not_valid: u64,
    /// One line per accepted, reported difference (extension versions).
    /// Notes, without the `deviation: ` prefix — the prefix belongs to the
    /// place that prints them, so a note is never labelled twice.
    pub deviations: Vec<String>,
}

fn count_constraints(payload: &PgPlanPayload) -> u64 {
    constraints(payload).count() as u64
}

fn count_constraints_not_valid(payload: &PgPlanPayload) -> u64 {
    constraints(payload).filter(|c| !c.validated).count() as u64
}

fn constraints(payload: &PgPlanPayload) -> impl Iterator<Item = &crate::model::PgConstraint> {
    payload
        .databases
        .iter()
        .flat_map(|db| db.schemas.iter())
        .flat_map(|schema| schema.tables.iter())
        .flat_map(|table| table.constraints.iter())
}

/// Accept a different installed extension version under
/// [`ExtensionVersionPolicy::Default`], and name every substitution.
///
/// The version is copied from the expectation into the observation so the
/// payload comparison that follows still checks everything else about the
/// extension; each copy produces one `deviation:` line for the operator's
/// `RESTORE VERIFIED` output.
fn reconcile_extension_versions(
    expected: &PgPlanPayload,
    actual: &mut PgPlanPayload,
    policy: ExtensionVersionPolicy,
) -> Vec<String> {
    let mut deviations = Vec::new();
    if policy != ExtensionVersionPolicy::Default {
        return deviations;
    }
    for (expected_db, actual_db) in expected.databases.iter().zip(actual.databases.iter_mut()) {
        for expected_extension in &expected_db.extensions {
            for actual_extension in actual_db.extensions.iter_mut() {
                if actual_extension.name != expected_extension.name
                    || actual_extension.version == expected_extension.version
                {
                    continue;
                }
                deviations.push(format!(
                    "extension {} restored at version {} (source {})",
                    actual_extension.name, actual_extension.version, expected_extension.version
                ));
                actual_extension.version = expected_extension.version.clone();
            }
        }
    }
    deviations
}

/// Privilege letters that exist only from the named PostgreSQL major onwards.
/// `m` is `MAINTAIN`, added in PostgreSQL 17.
const PRIVILEGE_SINCE: &[(char, u32)] = &[('m', 17)];

/// Drop privilege letters the source major cannot express from every `aclitem`.
///
/// Granting on a newer destination materialises that server's *own* owner
/// defaults, which legitimately include privileges the source cluster never had:
/// a PostgreSQL 10 table owner holds `arwdDxt`, the same owner on 18 holds
/// `arwdDxtm`. Restoring the source's grants is correct, and revoking the
/// destination's native default from its owner would not be — so the comparison
/// ignores letters that did not exist on the source.
fn strip_privileges_newer_than(payload: &mut PgPlanPayload, source_major: u32) {
    let dropped: Vec<char> = PRIVILEGE_SINCE
        .iter()
        .filter(|(_, since)| *since > source_major)
        .map(|(letter, _)| *letter)
        .collect();
    if dropped.is_empty() {
        return;
    }
    let strip = |acl: &mut Vec<String>| {
        for item in acl.iter_mut() {
            *item = strip_privilege_letters(item, &dropped);
        }
    };
    for database in &mut payload.databases {
        strip(&mut database.acl);
        for schema in &mut database.schemas {
            strip(&mut schema.acl);
            for table in &mut schema.tables {
                strip(&mut table.acl);
            }
            for sequence in &mut schema.sequences {
                strip(&mut sequence.acl);
            }
            for view in &mut schema.views {
                strip(&mut view.acl);
            }
            for function in &mut schema.functions {
                strip(&mut function.acl);
            }
        }
    }
}

/// Remove `dropped` privilege letters (and any `*` grant-option marker that
/// follows them) from one `grantee=privs/grantor` aclitem.
fn strip_privilege_letters(item: &str, dropped: &[char]) -> String {
    let Some((grantee, rest)) = item.split_once('=') else {
        return item.to_string();
    };
    let (privs, grantor) = match rest.split_once('/') {
        Some((privs, grantor)) => (privs, Some(grantor)),
        None => (rest, None),
    };
    let letters: Vec<char> = privs.chars().collect();
    let mut kept = String::with_capacity(privs.len());
    let mut index = 0;
    while index < letters.len() {
        let letter = letters[index];
        let grantable = letters.get(index + 1) == Some(&'*');
        if !dropped.contains(&letter) {
            kept.push(letter);
            if grantable {
                kept.push('*');
            }
        }
        index += if grantable { 2 } else { 1 };
    }
    match grantor {
        Some(grantor) => format!("{grantee}={kept}/{grantor}"),
        None => format!("{grantee}={kept}"),
    }
}

/// Re-render every expected view definition through the *destination's* own
/// deparser, so a cross-major comparison is between two databases rather than
/// between two versions of `pg_get_viewdef`.
///
/// `pg_get_viewdef` output is version-dependent — PostgreSQL 10 renders
/// `SELECT * FROM app.zombies` as `SELECT zombies.id, zombies.email`, 12+ as
/// `SELECT id, email` — so comparing the source's text with the destination's
/// re-read text failed for *any* view on *any* cross-major restore, however
/// faithful the restore was. The probe creates a temporary view (session-local,
/// invisible to other sessions, gone at disconnect) from the source text and
/// asks the destination to render that; the result is what the destination would
/// report for a correctly restored view, and comparing it stays exact.
///
/// A definition the probe cannot handle keeps its original text, so the
/// comparison still fails closed.
async fn restate_view_definitions(
    params: &PostgresParams,
    expected: &mut PgPlanPayload,
    destination_major: u32,
) -> Result<()> {
    for database in &mut expected.databases {
        let has_views = database.schemas.iter().any(|s| !s.views.is_empty());
        if !has_views {
            continue;
        }
        let conn = PgConnection::connect(params, &database.name).await?;
        for schema in &mut database.schemas {
            for view in &mut schema.views {
                match render_view_definition(&conn.client, &view.definition).await {
                    Ok(rendered) => view.definition = rendered,
                    Err(error) => tracing::warn!(
                        view = %format!("{}.{}", view.schema, view.name),
                        %error,
                        destination_major,
                        "could not re-render the view definition on the destination; \
                         comparing the source text verbatim"
                    ),
                }
            }
        }
    }
    Ok(())
}

/// The destination's own rendering of `definition`, via a temporary view.
async fn render_view_definition(client: &Client, definition: &str) -> Result<String> {
    const PROBE: &str = "__rb_viewdef_probe";
    let body = definition.trim_end();
    let body = body.strip_suffix(';').unwrap_or(body);
    client
        .batch_execute(&format!("DROP VIEW IF EXISTS pg_temp.{PROBE}"))
        .await
        .map_err(|e| BackupError::phase_src(Phase::Verify, "drop view definition probe", e))?;
    client
        .batch_execute(&format!("CREATE TEMP VIEW {PROBE} AS {body}"))
        .await
        .map_err(|e| BackupError::phase_src(Phase::Verify, "create view definition probe", e))?;
    let row = client
        .query_one(
            &format!("SELECT pg_catalog.pg_get_viewdef('pg_temp.{PROBE}'::regclass, true)::text"),
            &[],
        )
        .await
        .map_err(|e| BackupError::phase_src(Phase::Verify, "render view definition probe", e))?;
    let rendered: String = row.get(0);
    client
        .batch_execute(&format!("DROP VIEW pg_temp.{PROBE}"))
        .await
        .map_err(|e| BackupError::phase_src(Phase::Verify, "drop view definition probe", e))?;
    Ok(rendered)
}

/// How many catalog differences a failed read-back names before it stops
/// walking. One structural mistake (a schema that never got created, an owner
/// applied cluster-wide) differs in thousands of places; the first handful
/// identify it and the rest only bury it.
const MAX_REPORTED_DIFFERENCES: usize = 20;

/// Every way the restored catalog differs from the plan, each difference named
/// by the objects it belongs to — `tables["account_move"].columns["state"]`,
/// not `tables[92].columns[57]` — so the message says what to go and look at.
fn catalog_differences(expected: &PgPlanPayload, actual: &PgPlanPayload) -> Vec<String> {
    let expected = serde_json::to_value(expected).unwrap_or(serde_json::Value::Null);
    let actual = serde_json::to_value(actual).unwrap_or(serde_json::Value::Null);
    let mut found = Vec::new();
    collect_json_differences("catalog", &expected, &actual, &mut found);
    if found.is_empty() {
        found.push("different serialized catalog values".to_string());
    }
    found
}

/// The name a catalog object carries, used as its path segment.
fn element_name(value: &serde_json::Value) -> Option<&str> {
    value.get("name").and_then(serde_json::Value::as_str)
}

fn collect_json_differences(
    path: &str,
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    found: &mut Vec<String>,
) {
    use serde_json::Value;
    if expected == actual || found.len() >= MAX_REPORTED_DIFFERENCES {
        return;
    }
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            let keys = expected
                .keys()
                .chain(actual.keys().filter(|key| !expected.contains_key(*key)));
            for key in keys {
                let child = format!("{path}.{key}");
                match (expected.get(key), actual.get(key)) {
                    (Some(expected), Some(actual)) => {
                        collect_json_differences(&child, expected, actual, found);
                    }
                    (Some(_), None) => found.push(format!("{child} missing at destination")),
                    (None, Some(_)) => {
                        found.push(format!("{child} unexpectedly present at destination"));
                    }
                    (None, None) => {}
                }
                if found.len() >= MAX_REPORTED_DIFFERENCES {
                    return;
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            let named = expected
                .iter()
                .chain(actual)
                .all(|element| element_name(element).is_some());
            if named {
                collect_named_differences(path, expected, actual, found);
            } else {
                if expected.len() != actual.len() {
                    found.push(format!(
                        "{path} length source={} destination={}",
                        expected.len(),
                        actual.len()
                    ));
                }
                for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                    collect_json_differences(&format!("{path}[{index}]"), expected, actual, found);
                    if found.len() >= MAX_REPORTED_DIFFERENCES {
                        return;
                    }
                }
            }
        }
        _ => found.push(format!(
            "{path} source={} destination={}",
            compact_json(expected),
            compact_json(actual)
        )),
    }
}

/// Compare two lists of named objects by name rather than by position: one
/// missing table then reports itself instead of shifting every later index and
/// drowning the real difference in noise.
fn collect_named_differences(
    path: &str,
    expected: &[serde_json::Value],
    actual: &[serde_json::Value],
    found: &mut Vec<String>,
) {
    let names = |elements: &[serde_json::Value]| -> Vec<String> {
        elements
            .iter()
            .filter_map(element_name)
            .map(str::to_string)
            .collect()
    };
    let expected_names = names(expected);
    let actual_names = names(actual);
    for (element, name) in expected.iter().zip(&expected_names) {
        let child = format!("{path}[{name:?}]");
        match actual
            .iter()
            .find(|candidate| element_name(candidate) == Some(name.as_str()))
        {
            Some(counterpart) => collect_json_differences(&child, element, counterpart, found),
            None => found.push(format!("{child} missing at destination")),
        }
        if found.len() >= MAX_REPORTED_DIFFERENCES {
            return;
        }
    }
    for name in &actual_names {
        if !expected_names.contains(name) {
            found.push(format!(
                "{path}[{name:?}] unexpectedly present at destination"
            ));
            if found.len() >= MAX_REPORTED_DIFFERENCES {
                return;
            }
        }
    }
    // Same objects, different declaration order — invisible above, and a real
    // difference: column order is part of a table's shape.
    if expected_names.len() == actual_names.len() && expected_names != actual_names {
        found.push(format!(
            "{path} order source={expected_names:?} destination={actual_names:?}"
        ));
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    const MAX: usize = 240;
    let rendered = value.to_string();
    if rendered.chars().count() <= MAX {
        rendered
    } else {
        format!("{}…", rendered.chars().take(MAX).collect::<String>())
    }
}

fn normalize_catalog(payload: &mut PgPlanPayload) {
    for database in &mut payload.databases {
        // A planner estimate, not user state: `relpages` differs between two
        // faithful copies of the same rows.
        for config in &mut database.extension_configs {
            config.estimated_bytes = 0;
            // Counted on the source during `analyze` and checked while applying
            // and refreshing; the destination's re-introspection never counts,
            // so comparing the field here would report every restore as a
            // difference.
            config.expected_rows = None;
        }
        for schema in &mut database.schemas {
            for view in &mut schema.views {
                view.expected_rows = None;
            }
            for table in &mut schema.tables {
                table.estimated_rows = 0;
                table.estimated_bytes = 0;
                table.expected_rows = None;
                // `ordinal` is `pg_attribute.attnum`, which keeps counting the
                // columns a table has dropped: a source table that lost four
                // columns numbers its 58th live column 62, while a logical
                // restore — which recreates only the live columns — numbers the
                // same column 58. That gap records the source table's *history*,
                // not its shape, and no logical restore can reproduce it (only
                // pg_upgrade can, because it keeps the physical files). Nor
                // could comparing attnum ever be a complete check: a column
                // dropped from the end leaves no gap at all. So both sides are
                // renumbered and what the comparison enforces is the column
                // *order*, which is checked in full.
                for (position, column) in table.columns.iter_mut().enumerate() {
                    column.ordinal = i32::try_from(position)
                        .unwrap_or(i32::MAX)
                        .saturating_add(1);
                }
            }
        }
    }
}

/// Select a cluster database that is not one of the overwrite targets.
fn restore_bootstrap_database(params: &PostgresParams, payload: &PgPlanPayload) -> String {
    let preferred = params.bootstrap_database();
    if payload.databases.iter().all(|db| db.name != preferred) {
        return preferred;
    }
    if payload.databases.iter().all(|db| db.name != "postgres") {
        return "postgres".to_string();
    }
    // Source discovery excludes template databases, making template1 the safe
    // administrative fallback when the standard postgres database is a target.
    "template1".to_string()
}

fn alter_database_connections(name: &str, allowed: bool) -> String {
    format!(
        "ALTER DATABASE {} WITH ALLOW_CONNECTIONS = {};",
        quote_ident(name),
        allowed
    )
}

fn drop_database_sql(name: &str) -> String {
    format!("DROP DATABASE {};", quote_ident(name))
}

/// Replace one existing database on every supported PostgreSQL major. Disabling
/// new connections closes the race that a plain terminate-then-drop sequence
/// would leave on PostgreSQL 10–12 (which lack `DROP DATABASE ... FORCE`).
async fn drop_database_for_overwrite(client: &Client, name: &str) -> Result<()> {
    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_database WHERE datname = $1)",
            &[&name],
        )
        .await
        .map_err(|error| {
            BackupError::phase_src(
                Phase::Apply,
                format!("check existing database {name:?}"),
                error,
            )
        })?
        .get(0);
    if !exists {
        return Ok(());
    }

    let disable = alter_database_connections(name, false);
    client.batch_execute(&disable).await.map_err(|error| {
        BackupError::phase_src(
            Phase::Apply,
            format!("disable connections before overwriting database {name:?}"),
            error,
        )
    })?;

    let terminated = client
        .query(
            "SELECT pg_catalog.pg_terminate_backend(pid) \
             FROM pg_catalog.pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_catalog.pg_backend_pid()",
            &[&name],
        )
        .await;
    let rows = match terminated {
        Ok(rows) => rows,
        Err(error) => {
            reallow_connections(client, name).await;
            return Err(BackupError::phase_src(
                Phase::Apply,
                format!("terminate sessions before overwriting database {name:?}"),
                error,
            ));
        }
    };
    if rows.iter().any(|row| !row.get::<_, bool>(0)) {
        reallow_connections(client, name).await;
        return Err(BackupError::phase(
            Phase::Apply,
            format!("not permitted to terminate every session on database {name:?}"),
        ));
    }

    let drop_sql = drop_database_sql(name);
    if let Err(error) = client.batch_execute(&drop_sql).await {
        reallow_connections(client, name).await;
        return Err(BackupError::phase_src(
            Phase::Apply,
            format!("drop database {name:?} for overwrite"),
            error,
        ));
    }
    Ok(())
}

/// Undo the connection block taken before an overwrite attempt that then
/// failed. A failure *here* leaves a pre-existing production database that
/// nobody can connect to, so it must be reported rather than dropped: the
/// caller's error is about the overwrite, and said nothing about connections
/// still being disabled.
async fn reallow_connections(client: &Client, name: &str) {
    if let Err(error) = client
        .batch_execute(&alter_database_connections(name, true))
        .await
    {
        tracing::error!(
            database = %name,
            %error,
            "could not re-enable connections after a failed overwrite; the database is \
             unreachable until an administrator runs \
             ALTER DATABASE ... WITH ALLOW_CONNECTIONS = true"
        );
    }
}

/// Reject a plan carrying an item kind this build cannot restore. A newer
/// source may emit kinds an older destination has never heard of; silently
/// skipping them would restore a database missing exactly the data the new kind
/// was added to carry, and certify it as verified (I-FAILCLOSED).
fn check_item_kinds(plan: &BackupPlan) -> Result<()> {
    for item in &plan.items {
        if !DATA_ITEM_KINDS.contains(&item.kind.as_str()) {
            return Err(BackupError::PlanRejected(format!(
                "unknown postgres item kind {}",
                item.kind
            )));
        }
    }
    Ok(())
}

/// Map `item.id` → its COPY descriptor (data-bearing items only).
fn item_metas(plan: &BackupPlan) -> Result<HashMap<u32, ItemMeta>> {
    check_item_kinds(plan)?;
    Ok(plan
        .items
        .iter()
        .filter_map(|i| {
            serde_json::from_value::<ItemMeta>(i.meta.clone())
                .ok()
                .map(|m| (i.id, m))
        })
        .collect())
}

/// Statement to run on the destination right before an item's `COPY … FROM
/// STDIN`. `CREATE EXTENSION` has already inserted the extension's own rows
/// into a configuration table, so the source's rows are loaded over a cleared
/// scope — the same scope the source streamed — instead of on top of them.
fn pre_copy_sql(meta: &ItemMeta) -> Option<String> {
    if meta.kind != "extension_config" {
        return None;
    }
    let qual = quote_qualified(&meta.schema, &meta.table);
    Some(match meta.condition.as_deref() {
        Some(condition) => format!("DELETE FROM {qual} {condition}"),
        None => format!("TRUNCATE {qual}"),
    })
}

async fn run_statements(client: &Client, stmts: &[String]) -> Result<()> {
    for stmt in stmts {
        if let Err(error) = client.batch_execute(stmt).await {
            // Bootstrap roles such as `postgres` exist on every fresh cluster.
            // Keep the restore idempotent there; subsequent ALTER/GRANT DDL still
            // applies source settings where the destination account permits it.
            if stmt.starts_with("CREATE ROLE ")
                && error.code() == Some(&tokio_postgres::error::SqlState::DUPLICATE_OBJECT)
            {
                tracing::debug!(%stmt, "role already exists; retaining destination bootstrap role");
                continue;
            }
            return Err(BackupError::phase_src(
                Phase::Apply,
                format!("apply DDL: {stmt}"),
                error,
            ));
        }
    }
    Ok(())
}

/// `COPY … FROM STDIN (FORMAT binary)` mirroring the source's `COPY … TO STDOUT`,
/// so the binary stream loads back verbatim.
fn copy_in_sql(meta: &ItemMeta) -> String {
    let qual = quote_qualified(&meta.schema, &meta.table);
    if meta.columns.is_empty() {
        format!("COPY {qual} FROM STDIN (FORMAT binary)")
    } else {
        let cols = meta
            .columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        format!("COPY {qual} ({cols}) FROM STDIN (FORMAT binary)")
    }
}

/// Compare the rows the source counted with the rows this side actually wrote.
///
/// The BLAKE3 commitments prove that every byte that arrived is the byte that
/// was sent; they say nothing about rows that never left, or that a `COPY`
/// silently dropped. `expected` is absent in a plan written before this check
/// existed, and an absent count is not a failure — it is an unverified restore,
/// and the caller says so once per run.
fn check_expected_rows(name: &str, expected: Option<u64>, actual: u64) -> Result<()> {
    match expected {
        Some(expected) if expected != actual => Err(BackupError::Integrity(format!(
            "{name}: source counted {expected} rows, destination COPY wrote {actual}"
        ))),
        _ => Ok(()),
    }
}

async fn apply_data(
    conns: &HashMap<String, PgConnection>,
    metas: &HashMap<u32, ItemMeta>,
    src: &mut dyn ChunkSource,
) -> Result<u64> {
    // The open COPY sink for the item currently being loaded.
    let mut current: Option<ActiveCopy> = None;
    let mut rows_total: u64 = 0;
    // An older plan carries no counts. That is not a failure, but the operator
    // must know the restore was not row-verified.
    let mut unverified = false;

    // Close the open COPY and hold its row count against the source's.
    macro_rules! close_and_check {
        () => {
            if let Some((closed_id, rows)) = finish_current(&mut current).await? {
                rows_total = rows_total.saturating_add(rows);
                if let Some(meta) = metas.get(&closed_id) {
                    if meta.expected_rows.is_none() {
                        unverified = true;
                    }
                    check_expected_rows(
                        &format!("{}.{}", meta.schema, meta.table),
                        meta.expected_rows,
                        rows,
                    )?;
                }
            }
        };
    }

    loop {
        match src.next().await? {
            ChunkEvent::Chunk { item_id, data, .. } => {
                let reopen = current
                    .as_ref()
                    .map(|(id, _, _)| *id != item_id)
                    .unwrap_or(true);
                if reopen {
                    close_and_check!();
                    let meta = metas.get(&item_id).ok_or_else(|| {
                        BackupError::phase(Phase::Apply, format!("data for unknown item {item_id}"))
                    })?;
                    let conn = conns.get(&meta.database).ok_or_else(|| {
                        BackupError::phase(
                            Phase::Apply,
                            format!("no connection for database '{}'", meta.database),
                        )
                    })?;
                    if let Some(sql) = pre_copy_sql(meta) {
                        conn.client.batch_execute(&sql).await.map_err(|e| {
                            BackupError::phase_src(Phase::Apply, format!("pre-copy: {sql}"), e)
                        })?;
                    }
                    let sql = copy_in_sql(meta);
                    let sink = conn.client.copy_in::<_, Bytes>(&sql).await.map_err(|e| {
                        BackupError::phase_src(
                            Phase::Apply,
                            format!("copy_in {}.{}", meta.schema, meta.table),
                            e,
                        )
                    })?;
                    current = Some((item_id, 0, Box::pin(sink)));
                }
                if let Some((_, received, sink)) = &mut current {
                    *received += data.len() as u64;
                    sink.send(Bytes::from(data))
                        .await
                        .map_err(|e| BackupError::phase_src(Phase::Apply, "copy_in send", e))?;
                }
            }
            ChunkEvent::ItemEnd { item_id, total, .. } => {
                let (open_id, received, _) = current.as_ref().ok_or_else(|| {
                    BackupError::phase(
                        Phase::Apply,
                        format!("ItemEnd item={item_id} without open COPY"),
                    )
                })?;
                if *open_id != item_id || *received != total {
                    return Err(BackupError::phase(
                        Phase::Verify,
                        format!("ItemEnd item={item_id} does not match open item={open_id} bytes={received}"),
                    ));
                }
                close_and_check!();
            }
            ChunkEvent::End => {
                close_and_check!();
                break;
            }
        }
    }
    if unverified {
        tracing::warn!("plan carries no expected_rows; row-count verification skipped");
    }
    Ok(rows_total)
}

type ActiveCopy = (u32, u64, Pin<Box<CopyInSink<Bytes>>>);

/// Close the open `COPY` and return `(item_id, rows written)`, so the caller can
/// hold the count against the source's own.
async fn finish_current(current: &mut Option<ActiveCopy>) -> Result<Option<(u32, u64)>> {
    if let Some((item_id, _, mut sink)) = current.take() {
        let rows = sink
            .as_mut()
            .finish()
            .await
            .map_err(|e| BackupError::phase_src(Phase::Apply, "copy_in finish", e))?;
        return Ok(Some((item_id, rows)));
    }
    Ok(None)
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
            // The shared fixture installs pgcrypto 1.3; a probe that did not
            // offer it would fail the extension check of every assess test.
            available_extensions: vec![("pgcrypto".to_string(), "1.3".to_string())],
        }
    }

    fn params(database: Option<&str>) -> PostgresParams {
        PostgresParams {
            allow_unsupported_objects: false,
            host: "localhost".into(),
            port: 5432,
            user: "postgres".into(),
            password: None,
            database: database.map(str::to_string),
            sslmode: "disable".into(),
            sslrootcert: None,
            admin: true,
            overwrite: true,
            extension_version: ExtensionVersionPolicy::Source,
        }
    }

    #[test]
    fn overwrite_uses_a_non_target_bootstrap_database() {
        let mut payload = crate::model::test_fixture();
        assert_eq!(
            restore_bootstrap_database(&params(None), &payload),
            "postgres"
        );
        assert_eq!(
            restore_bootstrap_database(&params(Some("appdb")), &payload),
            "postgres"
        );

        payload.databases[0].name = "postgres".into();
        assert_eq!(
            restore_bootstrap_database(&params(None), &payload),
            "template1"
        );
    }

    #[test]
    fn overwrite_database_ddl_quotes_names_and_controls_connections() {
        assert_eq!(
            alter_database_connections("odd\"db", false),
            "ALTER DATABASE \"odd\"\"db\" WITH ALLOW_CONNECTIONS = false;"
        );
        assert_eq!(
            alter_database_connections("odd\"db", true),
            "ALTER DATABASE \"odd\"\"db\" WITH ALLOW_CONNECTIONS = true;"
        );
        assert_eq!(drop_database_sql("odd\"db"), "DROP DATABASE \"odd\"\"db\";");
    }

    #[test]
    fn passes_on_clean_destination() {
        let payload = crate::model::test_fixture(); // source major 16, db "appdb"
        let pf = assess(
            &payload,
            &probe(16, false, true, true),
            false,
            1024,
            ExtensionVersionPolicy::Source,
        );
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
        let pf = assess(
            &payload,
            &probe(15, true, true, true),
            false,
            0,
            ExtensionVersionPolicy::Source,
        );
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "server_version" && !c.passed));
    }

    #[test]
    fn preflight_rejects_incomplete_locale_metadata() {
        let mut payload = crate::model::test_fixture();
        payload.databases[0].locale_provider = Some(crate::model::PgLocaleProvider::Icu);
        payload.databases[0].provider_locale = None;

        let pf = assess(
            &payload,
            &probe(16, true, true, true),
            false,
            0,
            ExtensionVersionPolicy::Source,
        );
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "database_locale:appdb" && !c.passed));
    }

    #[test]
    fn fails_without_creation_privileges() {
        let payload = crate::model::test_fixture();
        // not super, missing CREATEROLE
        let pf = assess(
            &payload,
            &probe(16, false, true, false),
            false,
            0,
            ExtensionVersionPolicy::Source,
        );
        assert!(!pf.ok);
        assert!(pf
            .checks
            .iter()
            .any(|c| c.name == "privileges" && !c.passed));
        // superuser alone suffices.
        let pf2 = assess(
            &payload,
            &probe(16, true, false, false),
            false,
            0,
            ExtensionVersionPolicy::Source,
        );
        assert!(pf2
            .checks
            .iter()
            .any(|c| c.name == "privileges" && c.passed));
    }

    /// PostgreSQL 17 added MAINTAIN, so a PostgreSQL 10 source's owner ACL
    /// (`arwdDxt`) legitimately reads `arwdDxtm` once restored onto 18.
    #[test]
    fn privileges_newer_than_the_source_are_ignored_when_comparing() {
        assert_eq!(
            strip_privilege_letters("postgres=arwdDxtm/postgres", &['m']),
            "postgres=arwdDxt/postgres"
        );
        // The grant-option marker travels with its letter.
        assert_eq!(strip_privilege_letters("u=rm*w/o", &['m']), "u=rw/o");
        assert_eq!(strip_privilege_letters("u=rm*w/o", &[]), "u=rm*w/o");
        // PUBLIC has an empty grantee, and a missing grantor stays missing.
        assert_eq!(strip_privilege_letters("=rm/o", &['m']), "=r/o");
        assert_eq!(strip_privilege_letters("u=rm", &['m']), "u=r");
        // Not an aclitem at all: left alone rather than mangled.
        assert_eq!(strip_privilege_letters("nonsense", &['m']), "nonsense");

        let mut payload = PgPlanPayload {
            server_major: 10,
            databases: vec![crate::model::PgDatabase {
                acl: vec!["postgres=CTcm/postgres".to_string()],
                schemas: vec![crate::model::PgSchema {
                    tables: vec![crate::model::PgTable {
                        acl: vec!["postgres=arwdDxtm/postgres".to_string()],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        strip_privileges_newer_than(&mut payload, 10);
        assert_eq!(payload.databases[0].acl, vec!["postgres=CTc/postgres"]);
        assert_eq!(
            payload.databases[0].schemas[0].tables[0].acl,
            vec!["postgres=arwdDxt/postgres"]
        );
        // A source that already knows MAINTAIN keeps it.
        let mut modern = payload.clone();
        strip_privileges_newer_than(&mut modern, 17);
        assert_eq!(modern.databases[0].acl, vec!["postgres=CTc/postgres"]);
    }

    /// Build a one-table catalog whose columns carry the given `(name, attnum)`.
    fn catalog_with_columns(columns: &[(&str, i32)]) -> PgPlanPayload {
        PgPlanPayload {
            databases: vec![crate::model::PgDatabase {
                name: "appdb".into(),
                schemas: vec![crate::model::PgSchema {
                    name: "app".into(),
                    tables: vec![crate::model::PgTable {
                        name: "accounts".into(),
                        columns: columns
                            .iter()
                            .map(|(name, ordinal)| crate::model::PgColumn {
                                name: (*name).to_string(),
                                ordinal: *ordinal,
                                type_name: "integer".into(),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn columns_dropped_on_the_source_do_not_fail_the_read_back() {
        // The source dropped four columns before `state`, so it numbers it 62.
        // A logical restore recreates only the live columns and numbers it 58 —
        // the same table, in the same order.
        let mut source = catalog_with_columns(&[("id", 1), ("email", 2), ("state", 62)]);
        let mut destination = catalog_with_columns(&[("id", 1), ("email", 2), ("state", 3)]);
        normalize_catalog(&mut source);
        normalize_catalog(&mut destination);
        assert_eq!(source, destination);
    }

    #[test]
    fn a_reordered_column_still_fails_the_read_back() {
        let mut source = catalog_with_columns(&[("id", 1), ("email", 2)]);
        let mut destination = catalog_with_columns(&[("email", 1), ("id", 2)]);
        normalize_catalog(&mut source);
        normalize_catalog(&mut destination);
        assert_ne!(source, destination);
        let differences = catalog_differences(&source, &destination);
        assert!(
            differences.iter().any(|d| d.contains("order source=")),
            "{differences:?}"
        );
    }

    #[test]
    fn a_difference_is_named_by_the_objects_it_belongs_to() {
        let source = catalog_with_columns(&[("id", 1), ("email", 2)]);
        let mut destination = source.clone();
        destination.databases[0].schemas[0].tables[0].columns[1].type_name = "text".into();
        let differences = catalog_differences(&source, &destination);
        assert_eq!(
            differences,
            vec![concat!(
                r#"catalog.databases["appdb"].schemas["app"].tables["accounts"]"#,
                r#".columns["email"].type_name source="integer" destination="text""#
            )]
        );
    }

    #[test]
    fn a_missing_object_is_reported_by_name_and_every_difference_is_listed() {
        let source = catalog_with_columns(&[("id", 1), ("email", 2)]);
        let mut destination = source.clone();
        destination.databases[0].schemas[0].tables[0].columns.pop();
        destination.databases[0].schemas[0].tables[0].columns[0].not_null = true;
        let differences = catalog_differences(&source, &destination);
        assert!(
            differences
                .iter()
                .any(|d| d
                    .ends_with("tables[\"accounts\"].columns[\"email\"] missing at destination")),
            "{differences:?}"
        );
        assert!(
            differences
                .iter()
                .any(|d| d.contains("columns[\"id\"].not_null source=false destination=true")),
            "{differences:?}"
        );
    }

    #[test]
    fn the_difference_list_is_capped() {
        let columns: Vec<(String, i32)> = (0..MAX_REPORTED_DIFFERENCES * 2)
            .map(|index| (format!("c{index}"), index as i32 + 1))
            .collect();
        let borrowed: Vec<(&str, i32)> = columns
            .iter()
            .map(|(name, ordinal)| (name.as_str(), *ordinal))
            .collect();
        let source = catalog_with_columns(&borrowed);
        let mut destination = source.clone();
        for column in &mut destination.databases[0].schemas[0].tables[0].columns {
            column.type_name = "text".into();
        }
        assert_eq!(
            catalog_differences(&source, &destination).len(),
            MAX_REPORTED_DIFFERENCES
        );
    }

    /// The read-back must name the constraint and the field that differs: a
    /// restore that dropped `NOT VALID` differs in one boolean, and "the
    /// catalogs differ" would not be actionable.
    #[test]
    fn constraint_state_mismatch_is_reported_by_name() {
        let expected = crate::model::test_fixture();
        let mut actual = expected.clone();
        let constraint = actual.databases[0].schemas[0].tables[0]
            .constraints
            .first_mut()
            .expect("the fixture table has a constraint");
        let name = constraint.name.clone();
        constraint.validated = false;

        let differences = catalog_differences(&expected, &actual);
        assert!(
            differences.iter().any(|d| d.contains(&name)
                && d.contains("validated")
                && d.contains("source=true")
                && d.contains("destination=false")),
            "{differences:?}"
        );

        // The same for deferrability.
        let mut actual = expected.clone();
        actual.databases[0].schemas[0].tables[0].constraints[0].initially_deferred = true;
        let differences = catalog_differences(&expected, &actual);
        assert!(
            differences
                .iter()
                .any(|d| d.contains(&name) && d.contains("initially_deferred")),
            "{differences:?}"
        );
    }

    /// A byte-for-byte commitment is computed over the rows that arrived: rows
    /// that never left the source, or that the destination `COPY` dropped, are
    /// invisible to it. The count is the other half of the proof.
    #[test]
    fn expected_rows_mismatch_is_an_integrity_error() {
        let err = check_expected_rows("app.accounts", Some(1_000), 999)
            .expect_err("a short restore is an integrity failure");
        assert!(
            matches!(&err, BackupError::Integrity(m)
                if m == "app.accounts: source counted 1000 rows, destination COPY wrote 999"),
            "unexpected error: {err}"
        );
        // Too many rows is just as wrong: it means the item was applied twice.
        assert!(check_expected_rows("app.accounts", Some(1_000), 1_001).is_err());
        check_expected_rows("app.accounts", Some(1_000), 1_000).expect("an exact match passes");
        check_expected_rows("app.empty", Some(0), 0).expect("an empty table passes");
    }

    /// A plan written before the count existed carries none. That is an
    /// unverified restore, not a failed one — the caller warns once per run.
    #[test]
    fn missing_expected_rows_skips_the_check() {
        check_expected_rows("app.accounts", None, 0).expect("no count, no check");
        check_expected_rows("app.accounts", None, 12_345).expect("no count, no check");
    }

    /// `CREATE EXTENSION … VERSION '1.0'` on a destination that only ships 1.1
    /// is an error, and it happens after roles and databases already exist. The
    /// preflight decides it instead, and names what the destination does have.
    #[test]
    fn assess_refuses_missing_extension_version() {
        let payload = crate::model::test_fixture();
        let mut dest = probe(16, true, true, true);
        dest.available_extensions = vec![("pgcrypto".to_string(), "1.4".to_string())];

        let pf = assess(&payload, &dest, false, 0, ExtensionVersionPolicy::Source);
        let check = pf
            .checks
            .iter()
            .find(|c| c.name == "extension:pgcrypto")
            .expect("the extension is checked");
        assert!(!check.passed, "1.3 is not available");
        assert!(
            check.detail.contains("available: 1.4")
                && check.detail.contains("--extension-version default"),
            "{}",
            check.detail
        );
        assert!(!pf.ok, "the plan is refused");

        // An extension the destination does not carry at all reports `none`.
        dest.available_extensions.clear();
        let pf = assess(&payload, &dest, false, 0, ExtensionVersionPolicy::Source);
        let check = pf
            .checks
            .iter()
            .find(|c| c.name == "extension:pgcrypto")
            .expect("the extension is checked");
        assert!(check.detail.contains("available: none"), "{}", check.detail);
    }

    /// Under `--extension-version default` a different available version passes,
    /// and the check says which version will be installed instead.
    #[test]
    fn assess_accepts_default_policy_with_note() {
        let payload = crate::model::test_fixture();
        let mut dest = probe(16, true, true, true);
        dest.available_extensions = vec![("pgcrypto".to_string(), "1.4".to_string())];

        let pf = assess(&payload, &dest, false, 0, ExtensionVersionPolicy::Default);
        let check = pf
            .checks
            .iter()
            .find(|c| c.name == "extension:pgcrypto")
            .expect("the extension is checked");
        assert!(check.passed, "{}", check.detail);
        assert!(
            check
                .detail
                .contains("will install the destination's default version"),
            "{}",
            check.detail
        );
        assert!(pf.ok, "the plan passes preflight");

        // The policy relaxes the version, never the extension itself.
        dest.available_extensions.clear();
        let pf = assess(&payload, &dest, false, 0, ExtensionVersionPolicy::Default);
        assert!(!pf.ok, "a missing extension is still refused");
    }

    /// The version substitution is reported, not hidden: the read-back accepts
    /// the installed version and the operator sees one `deviation:` line.
    #[test]
    fn a_substituted_extension_version_is_reported_as_a_deviation() {
        let expected = crate::model::test_fixture();
        let mut actual = expected.clone();
        actual.databases[0].extensions[0].version = "1.4".to_string();

        // Under the default policy the versions must match exactly: nothing is
        // reconciled, so the payload comparison still sees the difference.
        let mut untouched = actual.clone();
        assert!(reconcile_extension_versions(
            &expected,
            &mut untouched,
            ExtensionVersionPolicy::Source
        )
        .is_empty());
        assert_ne!(untouched, expected);

        let deviations =
            reconcile_extension_versions(&expected, &mut actual, ExtensionVersionPolicy::Default);
        assert_eq!(
            deviations,
            vec!["extension pgcrypto restored at version 1.4 (source 1.3)".to_string()]
        );
        assert_eq!(actual, expected, "only the version is reconciled");
    }

    #[test]
    fn copy_in_sql_mirrors_copy_out() {
        let m = ItemMeta {
            kind: "table".into(),
            database: "appdb".into(),
            schema: "app".into(),
            table: "accounts".into(),
            columns: vec!["id".into(), "email".into()],
            only: false,
            condition: None,
            expected_rows: None,
        };
        assert_eq!(
            copy_in_sql(&m),
            "COPY \"app\".\"accounts\" (\"id\", \"email\") FROM STDIN (FORMAT binary)"
        );
        let m2 = ItemMeta {
            columns: vec![],
            ..m
        };
        assert_eq!(
            copy_in_sql(&m2),
            "COPY \"app\".\"accounts\" FROM STDIN (FORMAT binary)"
        );
    }

    /// The destination clears the scope the source streamed before loading it:
    /// `CREATE EXTENSION` has already inserted the extension's own rows into the
    /// configuration table, and they are not the source's rows.
    #[test]
    fn extension_config_destination_deletes_matching_rows_before_copy() {
        let m = ItemMeta {
            kind: "extension_config".into(),
            database: "appdb".into(),
            schema: "public".into(),
            table: "rbtest_cfg".into(),
            columns: vec![],
            only: false,
            condition: Some("WHERE k >= 1000".into()),
            expected_rows: None,
        };
        assert_eq!(
            pre_copy_sql(&m).as_deref(),
            Some("DELETE FROM \"public\".\"rbtest_cfg\" WHERE k >= 1000")
        );
        assert_eq!(
            copy_in_sql(&m),
            "COPY \"public\".\"rbtest_cfg\" FROM STDIN (FORMAT binary)"
        );
        // An ordinary table is loaded into a database that was just created, so
        // there is nothing to clear.
        let table = ItemMeta {
            kind: "table".into(),
            ..m
        };
        assert_eq!(pre_copy_sql(&table), None);
    }

    /// With no registered condition the whole relation is configuration data,
    /// so the whole relation is cleared.
    #[test]
    fn extension_config_without_condition_truncates() {
        let m = ItemMeta {
            kind: "extension_config".into(),
            database: "appdb".into(),
            schema: "public".into(),
            table: "spatial_ref_sys".into(),
            columns: vec![],
            only: false,
            condition: None,
            expected_rows: None,
        };
        assert_eq!(
            pre_copy_sql(&m).as_deref(),
            Some("TRUNCATE \"public\".\"spatial_ref_sys\"")
        );
    }

    /// A newer source may plan an item kind this build cannot restore. Skipping
    /// it would produce a database missing exactly that data and still report a
    /// verified restore, so both preflight and the data pass refuse the plan.
    #[test]
    fn unknown_item_kind_is_rejected_at_validate_and_apply() {
        let payload = crate::model::test_fixture();
        let mut plan = crate::introspect::build_plan(&payload, "t".to_string());
        plan.items[0].kind = "large_object".to_string();

        // The check `validate` runs before probing the destination.
        let err = check_item_kinds(&plan).expect_err("unknown kind");
        assert!(
            matches!(&err, BackupError::PlanRejected(m) if m == "unknown postgres item kind large_object"),
            "unexpected error: {err}"
        );
        // The same check on the apply path, so an older binary handed the plan
        // by a peer that skipped preflight still refuses it.
        let err = item_metas(&plan).expect_err("unknown kind");
        assert!(matches!(err, BackupError::PlanRejected(_)), "{err}");

        // Both kinds this build does restore are accepted.
        plan.items[0].kind = "extension_config".to_string();
        check_item_kinds(&plan).expect("extension_config is restorable");
    }

    #[test]
    fn item_metas_indexes_table_items_by_id() {
        let payload = crate::model::test_fixture();
        let bp = crate::introspect::build_plan(&payload, "t".to_string());
        let metas = item_metas(&bp).expect("known item kinds");
        assert_eq!(metas.len(), 1);
        let m = metas.get(&0).expect("item 0");
        assert_eq!(m.database, "appdb");
        assert_eq!(m.schema, "app");
        assert_eq!(m.table, "accounts");
        assert_eq!(m.columns, vec!["id".to_string(), "email".to_string()]);
    }

    #[test]
    fn existing_database_blocks_unless_overwrite() {
        let payload = crate::model::test_fixture();
        let mut p = probe(16, true, true, true);
        p.existing_databases.insert("appdb".to_string());

        let blocked = assess(&payload, &p, false, 0, ExtensionVersionPolicy::Source);
        assert!(!blocked.ok, "existing db must block without --overwrite");
        assert!(blocked
            .checks
            .iter()
            .any(|c| c.name == "database:appdb" && !c.passed));

        let allowed = assess(&payload, &p, true, 0, ExtensionVersionPolicy::Source);
        assert!(
            allowed.ok,
            "--overwrite must allow restoring over existing db"
        );
    }

    #[test]
    fn a_failed_restore_removes_only_the_databases_it_created() {
        let mut preexisting = HashSet::new();
        preexisting.insert("kept".to_string());

        let removed = databases_to_remove(["kept", "created", "also_created"], &preexisting);

        assert_eq!(
            removed,
            vec!["created", "also_created"],
            "a database the run found already present must survive its failure"
        );
    }

    #[test]
    fn a_restore_that_created_nothing_removes_nothing() {
        let preexisting: HashSet<String> = ["appdb".to_string()].into_iter().collect();

        assert!(
            databases_to_remove(["appdb"], &preexisting).is_empty(),
            "nothing was created, so nothing may be dropped"
        );
    }
}
