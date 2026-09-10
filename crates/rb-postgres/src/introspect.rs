//! Read-only catalog introspection (plan Phase 2.2).
//!
//! Builds a [`PgPlanPayload`] from `pg_catalog`, then a self-contained
//! [`BackupPlan`] from it. Strictly read-only: only `SELECT`/catalog-function
//! calls — never anything that mutates the source (I-IMMUT).
//!
//! Robustness choices:
//! - Cast everything to `text`/`int8`/`int4`/`bool`/`text[]` in SQL, so Rust-side
//!   type mapping is trivial and version-independent.
//! - Lean on the server's own DDL emitters (`pg_get_constraintdef`,
//!   `pg_get_indexdef`, `pg_get_viewdef`, `pg_get_functiondef`, `format_type`,
//!   `pg_get_expr`, `pg_get_partkeydef`) instead of hand-reconstructing DDL.
//! - `left(nspname,3) <> 'pg_'` to skip system schemas (no `LIKE`-escape pitfalls).
//! - Version-branch the few catalog columns that differ across 10 and newer
//!   (`attgenerated` is 12+, `prokind` is 11+, database locale metadata is
//!   provider-aware in 15+ and renamed in 17).
//! - Exclude extension-owned objects from DDL (recreated by `CREATE EXTENSION`).
//!
//! NOTE: the live introspection is validated against a real server via the e2e
//! script `e2e/postgres_introspect.sh` (Docker) — it cannot run in a DB-less CI.
//! The pure pieces (`build_plan`, `now_rfc3339`) have unit tests that always run.

use std::collections::{HashMap, HashSet};

use tokio_postgres::Client;

use rb_core::error::{BackupError, Phase, Result};
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, PLAN_FORMAT_VERSION};

use crate::model::*;
use crate::{PgConnection, PostgresParams};

fn analyze_err(ctx: &str, e: tokio_postgres::Error) -> BackupError {
    BackupError::phase_src(Phase::Analyze, format!("introspect: {ctx}"), e)
}

/// Connect read-only and build the full cluster payload: cluster-global roles /
/// memberships / tablespaces, then each target database's object tree.
pub async fn introspect_cluster(params: &PostgresParams) -> Result<PgPlanPayload> {
    let boot = PgConnection::connect_read_only(params, &params.bootstrap_database()).await?;
    let server_version = boot.server_version.clone();
    let server_major = boot.server_major;

    let roles = gather_roles(&boot.client).await?;
    let memberships = gather_memberships(&boot.client).await?;
    let tablespaces = gather_tablespaces(&boot.client).await?;
    let names = target_database_names(params, &boot.client).await?;
    let mut databases = gather_databases_meta(&boot.client, &names, server_major).await?;
    drop(boot);

    // Per-database trees require a connection to each database.
    for db in &mut databases {
        let conn = PgConnection::connect_read_only(params, &db.name).await?;
        db.extensions = gather_extensions(&conn.client).await?;
        db.schemas = gather_schemas(&conn.client, server_major).await?;
        let unsupported = find_unsupported_objects(&conn.client, &db.name, server_major).await?;
        report_unsupported(unsupported, params.allow_unsupported_objects)?;
    }

    Ok(PgPlanPayload {
        server_version,
        server_major,
        roles,
        memberships,
        tablespaces,
        databases,
    })
}

// --- cluster-global ----------------------------------------------------------

async fn gather_roles(client: &Client) -> Result<Vec<PgRole>> {
    let rows = client
        .query(
            "SELECT r.rolname::text, r.rolsuper, r.rolcreatedb, r.rolcreaterole, \
                    r.rolinherit, r.rolcanlogin, r.rolreplication, r.rolbypassrls, \
                    r.rolconnlimit, r.rolvaliduntil::text, r.rolconfig, \
                    pg_catalog.shobj_description(r.oid, 'pg_authid')::text \
             FROM pg_catalog.pg_roles r \
             WHERE left(r.rolname, 3) <> 'pg_' \
             ORDER BY r.rolcanlogin, r.rolname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("roles", e))?;
    Ok(rows
        .iter()
        .map(|r| PgRole {
            name: r.get(0),
            superuser: r.get(1),
            createdb: r.get(2),
            createrole: r.get(3),
            inherit: r.get(4),
            login: r.get(5),
            replication: r.get(6),
            bypassrls: r.get(7),
            connlimit: r.get(8),
            valid_until: r.get(9),
            config: r.get::<_, Option<Vec<String>>>(10).unwrap_or_default(),
            comment: r.get(11),
        })
        .collect())
}

async fn gather_memberships(client: &Client) -> Result<Vec<PgMembership>> {
    let rows = client
        .query(
            "SELECT g.rolname::text, m.rolname::text, am.admin_option \
             FROM pg_catalog.pg_auth_members am \
             JOIN pg_catalog.pg_roles g ON g.oid = am.roleid \
             JOIN pg_catalog.pg_roles m ON m.oid = am.member \
             WHERE left(g.rolname, 3) <> 'pg_' AND left(m.rolname, 3) <> 'pg_' \
             ORDER BY 1, 2",
            &[],
        )
        .await
        .map_err(|e| analyze_err("memberships", e))?;
    Ok(rows
        .iter()
        .map(|r| PgMembership {
            role: r.get(0),
            member: r.get(1),
            admin_option: r.get(2),
        })
        .collect())
}

async fn gather_tablespaces(client: &Client) -> Result<Vec<PgTablespace>> {
    let rows = client
        .query(
            "SELECT t.spcname::text, pg_catalog.pg_get_userbyid(t.spcowner)::text, \
                    pg_catalog.pg_tablespace_location(t.oid)::text \
             FROM pg_catalog.pg_tablespace t \
             WHERE t.spcname NOT IN ('pg_default', 'pg_global') \
             ORDER BY t.spcname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("tablespaces", e))?;
    Ok(rows
        .iter()
        .map(|r| PgTablespace {
            name: r.get(0),
            owner: r.get(1),
            location: r.get::<_, Option<String>>(2).unwrap_or_default(),
        })
        .collect())
}

async fn target_database_names(params: &PostgresParams, client: &Client) -> Result<Vec<String>> {
    if let Some(db) = &params.database {
        return Ok(vec![db.clone()]);
    }
    let rows = client
        .query(
            "SELECT datname::text FROM pg_catalog.pg_database \
             WHERE datallowconn AND NOT datistemplate ORDER BY datname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("list databases", e))?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

/// Catalog projection normalized to provider / provider-locale / ICU-rules.
/// Keep this aligned with PostgreSQL's own `pg_dump` version branches.
fn database_locale_projection(server_major: u32) -> &'static str {
    if server_major >= 17 {
        "d.datlocprovider::text, d.datlocale::text, d.daticurules::text"
    } else if server_major >= 16 {
        "d.datlocprovider::text, d.daticulocale::text, d.daticurules::text"
    } else if server_major >= 15 {
        "d.datlocprovider::text, d.daticulocale::text, NULL::text"
    } else {
        "'c'::text, NULL::text, NULL::text"
    }
}

fn parse_locale_provider(value: &str) -> Result<PgLocaleProvider> {
    match value {
        "c" => Ok(PgLocaleProvider::Libc),
        "i" => Ok(PgLocaleProvider::Icu),
        "b" => Ok(PgLocaleProvider::Builtin),
        other => Err(BackupError::phase(
            Phase::Analyze,
            format!("introspect: unsupported database locale provider {other:?}"),
        )),
    }
}

async fn gather_databases_meta(
    client: &Client,
    names: &[String],
    server_major: u32,
) -> Result<Vec<PgDatabase>> {
    let locale_projection = database_locale_projection(server_major);
    let sql = format!(
        "SELECT d.datname::text, pg_catalog.pg_get_userbyid(d.datdba)::text, \
                pg_catalog.pg_encoding_to_char(d.encoding)::text, \
                d.datcollate::text, d.datctype::text, {locale_projection}, \
                ts.spcname::text, d.datconnlimit, d.datallowconn, d.datistemplate, \
                pg_catalog.shobj_description(d.oid, 'pg_database')::text, \
                (SELECT s.setconfig FROM pg_catalog.pg_db_role_setting s \
                    WHERE s.setdatabase = d.oid AND s.setrole = 0), \
                d.datacl::text[] \
         FROM pg_catalog.pg_database d \
         LEFT JOIN pg_catalog.pg_tablespace ts \
            ON ts.oid = d.dattablespace AND ts.spcname <> 'pg_default' \
         WHERE d.datname = ANY($1) \
         ORDER BY d.datname"
    );
    let rows = client
        .query(&sql, &[&names])
        .await
        .map_err(|e| analyze_err("databases", e))?;
    rows.iter()
        .map(|r| {
            let locale_provider = parse_locale_provider(&r.get::<_, String>(5))?;
            Ok(PgDatabase {
                name: r.get(0),
                owner: r.get(1),
                encoding: r.get(2),
                collate: r.get(3),
                ctype: r.get(4),
                locale_provider: Some(locale_provider),
                provider_locale: r.get(6),
                icu_rules: r.get(7),
                tablespace: r.get(8),
                connlimit: r.get(9),
                allow_connections: r.get(10),
                is_template: r.get(11),
                comment: r.get(12),
                config: r.get::<_, Option<Vec<String>>>(13).unwrap_or_default(),
                acl: r.get::<_, Option<Vec<String>>>(14).unwrap_or_default(),
                extensions: Vec::new(),
                schemas: Vec::new(),
            })
        })
        .collect()
}

// --- per-database ------------------------------------------------------------

async fn gather_extensions(client: &Client) -> Result<Vec<PgExtension>> {
    let rows = client
        .query(
            "SELECT e.extname::text, e.extversion::text, n.nspname::text \
             FROM pg_catalog.pg_extension e \
             JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
             ORDER BY e.extname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("extensions", e))?;
    Ok(rows
        .iter()
        .map(|r| PgExtension {
            name: r.get(0),
            version: r.get(1),
            schema: r.get(2),
        })
        .collect())
}

/// Build the full schema tree for the connected database.
async fn gather_schemas(client: &Client, major: u32) -> Result<Vec<PgSchema>> {
    // Schemas (ordered) + name→index map.
    let schema_rows = client
        .query(
            "SELECT n.nspname::text, pg_catalog.pg_get_userbyid(n.nspowner)::text, \
                    pg_catalog.obj_description(n.oid, 'pg_namespace')::text, n.nspacl::text[] \
             FROM pg_catalog.pg_namespace n \
             WHERE n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
             ORDER BY n.nspname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("schemas", e))?;
    let mut schemas: Vec<PgSchema> = Vec::with_capacity(schema_rows.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    for r in &schema_rows {
        let name: String = r.get(0);
        index.insert(name.clone(), schemas.len());
        schemas.push(PgSchema {
            name,
            owner: r.get(1),
            comment: r.get(2),
            acl: r.get::<_, Option<Vec<String>>>(3).unwrap_or_default(),
            tables: Vec::new(),
            sequences: Vec::new(),
            views: Vec::new(),
            functions: Vec::new(),
        });
    }

    // Batched per-relation detail, bucketed by table oid.
    let mut columns = gather_columns(client, major).await?;
    let mut constraints = gather_constraints(client).await?;
    let mut indexes = gather_indexes(client).await?;

    for (oid, schema_name, mut table) in gather_tables(client).await? {
        table.columns = columns.remove(&oid).unwrap_or_default();
        table.constraints = constraints.remove(&oid).unwrap_or_default();
        table.indexes = indexes.remove(&oid).unwrap_or_default();
        if let Some(&i) = index.get(&schema_name) {
            schemas[i].tables.push(table);
        }
    }
    for (schema_name, seq) in gather_sequences(client).await? {
        if let Some(&i) = index.get(&schema_name) {
            schemas[i].sequences.push(seq);
        }
    }
    for (oid, schema_name, mut view) in gather_views(client).await? {
        view.indexes = indexes.remove(&oid).unwrap_or_default();
        if let Some(&i) = index.get(&schema_name) {
            schemas[i].views.push(view);
        }
    }
    for (schema_name, func) in gather_functions(client, major).await? {
        if let Some(&i) = index.get(&schema_name) {
            schemas[i].functions.push(func);
        }
    }

    Ok(schemas)
}

/// Returns `(table_oid, schema_name, table)` — oid/schema used only for assembly.
async fn gather_tables(client: &Client) -> Result<Vec<(i64, String, PgTable)>> {
    let rows = client
        .query(
            "SELECT c.oid::int8, n.nspname::text, c.relname::text, \
                    pg_catalog.pg_get_userbyid(c.relowner)::text, \
                    c.relpersistence::text, c.relkind::text, c.relacl::text[], \
                    pg_catalog.obj_description(c.oid, 'pg_class')::text, c.reloptions, \
                    (SELECT spc.spcname::text FROM pg_catalog.pg_tablespace spc \
                        WHERE spc.oid = c.reltablespace AND spc.spcname <> 'pg_default'), \
                    CASE WHEN c.relkind = 'p' THEN pg_catalog.pg_get_partkeydef(c.oid) END, \
                    c.relispartition, \
                    CASE WHEN c.relispartition \
                         THEN pg_catalog.pg_get_expr(c.relpartbound, c.oid, true) END, \
                    CASE WHEN c.relispartition THEN ( \
                         SELECT (pn.nspname || '.' || pc.relname)::text \
                         FROM pg_catalog.pg_inherits i \
                         JOIN pg_catalog.pg_class pc ON pc.oid = i.inhparent \
                         JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace \
                         WHERE i.inhrelid = c.oid) END, \
                    CASE WHEN NOT c.relispartition THEN ( \
                         SELECT array_agg((pn.nspname || '.' || pc.relname)::text \
                                          ORDER BY pn.nspname, pc.relname) \
                         FROM pg_catalog.pg_inherits i \
                         JOIN pg_catalog.pg_class pc ON pc.oid = i.inhparent \
                         JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace \
                         WHERE i.inhrelid = c.oid) END, \
                    c.reltuples::int8, pg_catalog.pg_total_relation_size(c.oid)::int8 \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p') \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                                  WHERE d.objid = c.oid AND d.deptype = 'e') \
             ORDER BY n.nspname, c.relname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("tables", e))?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: i64 = r.get(0);
            let schema: String = r.get(1);
            let partition_parent: Option<String> = r.get(13);
            let partition_bound: Option<String> = r.get(12);
            let partition_of = partition_parent.map(|parent| PgPartitionOf {
                parent,
                bound: partition_bound.unwrap_or_default(),
            });
            let table = PgTable {
                schema: schema.clone(),
                name: r.get(2),
                owner: r.get(3),
                persistence: r.get(4),
                kind: r.get(5),
                columns: Vec::new(),
                constraints: Vec::new(),
                indexes: Vec::new(),
                acl: r.get::<_, Option<Vec<String>>>(6).unwrap_or_default(),
                comment: r.get(7),
                reloptions: r.get::<_, Option<Vec<String>>>(8).unwrap_or_default(),
                tablespace: r.get(9),
                partition_key: r.get(10),
                partition_of,
                inherits: r.get::<_, Option<Vec<String>>>(14).unwrap_or_default(),
                estimated_rows: r.get(15),
                estimated_bytes: r.get(16),
            };
            (oid, schema, table)
        })
        .collect())
}

async fn gather_columns(client: &Client, major: u32) -> Result<HashMap<i64, Vec<PgColumn>>> {
    // `attgenerated` exists only on pg 12+. On 10/11 there are no generated
    // columns, so `default` is always the attrdef expr and `generated` is NULL.
    let (default_expr, generated_expr) = if major >= 12 {
        (
            "CASE WHEN a.attgenerated = '' THEN pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) END",
            "CASE WHEN a.attgenerated = 's' THEN pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) END",
        )
    } else {
        ("pg_catalog.pg_get_expr(ad.adbin, ad.adrelid)", "NULL::text")
    };
    let sql = format!(
        "SELECT a.attrelid::int8, a.attname::text, a.attnum::int4, \
                pg_catalog.format_type(a.atttypid, a.atttypmod)::text, a.attnotnull, \
                {default_expr}, \
                CASE WHEN a.attidentity = '' THEN NULL ELSE a.attidentity::text END, \
                {generated_expr}, \
                CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation \
                     THEN (SELECT co.collname::text FROM pg_catalog.pg_collation co \
                              WHERE co.oid = a.attcollation) END, \
                a.attislocal, \
                pg_catalog.col_description(a.attrelid, a.attnum)::text \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
         WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped \
           AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
         ORDER BY a.attrelid, a.attnum"
    );
    let rows = client
        .query(&sql, &[])
        .await
        .map_err(|e| analyze_err("columns", e))?;
    let mut map: HashMap<i64, Vec<PgColumn>> = HashMap::new();
    for r in &rows {
        let oid: i64 = r.get(0);
        map.entry(oid).or_default().push(PgColumn {
            name: r.get(1),
            ordinal: r.get(2),
            type_name: r.get(3),
            not_null: r.get(4),
            default: r.get(5),
            identity: r.get(6),
            generated: r.get(7),
            collation: r.get(8),
            local: r.get(9),
            comment: r.get(10),
        });
    }
    Ok(map)
}

const CONSTRAINTS_QUERY: &str = "SELECT con.conrelid::int8, con.conname::text, con.contype::text, \
            pg_catalog.pg_get_constraintdef(con.oid, true)::text, \
            CASE WHEN con.contype = 'f' THEN \
                 (SELECT (fn.nspname || '.' || fc.relname)::text \
                  FROM pg_catalog.pg_class fc \
                  JOIN pg_catalog.pg_namespace fn ON fn.oid = fc.relnamespace \
                  WHERE fc.oid = con.confrelid) END \
     FROM pg_catalog.pg_constraint con \
     JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE c.relkind IN ('r', 'p') \
       AND con.contype IN ('p', 'u', 'f', 'c', 'x') \
       AND con.conislocal \
       AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
     ORDER BY con.conrelid, con.conname";

async fn gather_constraints(client: &Client) -> Result<HashMap<i64, Vec<PgConstraint>>> {
    let rows = client
        .query(CONSTRAINTS_QUERY, &[])
        .await
        .map_err(|e| analyze_err("constraints", e))?;
    let mut map: HashMap<i64, Vec<PgConstraint>> = HashMap::new();
    for r in &rows {
        let oid: i64 = r.get(0);
        map.entry(oid).or_default().push(PgConstraint {
            name: r.get(1),
            kind: r.get(2),
            definition: r.get(3),
            references: r.get(4),
        });
    }
    Ok(map)
}

async fn gather_indexes(client: &Client) -> Result<HashMap<i64, Vec<PgIndex>>> {
    let rows = client
        .query(
            "SELECT i.indrelid::int8, ic.relname::text, \
                    pg_catalog.pg_get_indexdef(i.indexrelid, 0, true)::text, \
                    i.indisprimary, i.indisunique, (con.oid IS NOT NULL), \
                    pg_catalog.obj_description(i.indexrelid, 'pg_class')::text, \
                    (SELECT (pin.nspname || '.' || pic.relname)::text \
                     FROM pg_catalog.pg_inherits ii \
                     JOIN pg_catalog.pg_class pic ON pic.oid = ii.inhparent \
                     JOIN pg_catalog.pg_namespace pin ON pin.oid = pic.relnamespace \
                     WHERE ii.inhrelid = i.indexrelid) \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
             JOIN pg_catalog.pg_class c ON c.oid = i.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_catalog.pg_constraint con ON con.conindid = i.indexrelid \
             WHERE c.relkind IN ('r', 'p', 'm') \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
             ORDER BY i.indrelid, ic.relname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("indexes", e))?;
    let mut map: HashMap<i64, Vec<PgIndex>> = HashMap::new();
    for r in &rows {
        let oid: i64 = r.get(0);
        map.entry(oid).or_default().push(PgIndex {
            name: r.get(1),
            definition: r.get(2),
            is_primary: r.get(3),
            is_unique: r.get(4),
            is_constraint: r.get(5),
            comment: r.get(6),
            attach_to: r.get(7),
        });
    }
    Ok(map)
}

async fn gather_sequences(client: &Client) -> Result<Vec<(String, PgSequence)>> {
    let rows = client
        .query(
            "SELECT n.nspname::text, c.relname::text, \
                    pg_catalog.pg_get_userbyid(c.relowner)::text, \
                    pg_catalog.format_type(s.seqtypid, NULL)::text, \
                    s.seqstart::int8, s.seqincrement::int8, s.seqmin::int8, s.seqmax::int8, \
                    s.seqcache::int8, s.seqcycle, pgs.last_value::int8, c.relacl::text[], \
                    pg_catalog.obj_description(c.oid, 'pg_class')::text, \
                    (SELECT (dn.nspname || '.' || dc.relname || '.' || da.attname)::text \
                        FROM pg_catalog.pg_depend d \
                        JOIN pg_catalog.pg_class dc ON dc.oid = d.refobjid \
                        JOIN pg_catalog.pg_namespace dn ON dn.oid = dc.relnamespace \
                        JOIN pg_catalog.pg_attribute da \
                          ON da.attrelid = d.refobjid AND da.attnum = d.refobjsubid \
                        WHERE d.objid = c.oid AND d.classid = 'pg_class'::regclass \
                          AND d.refclassid = 'pg_class'::regclass \
                          AND d.deptype IN ('a', 'i') AND d.refobjsubid > 0 LIMIT 1), \
                    (pg_catalog.pg_has_role(c.relowner, 'USAGE') \
                     OR pg_catalog.has_sequence_privilege(c.oid, 'SELECT,USAGE')), \
                    pg_catalog.has_sequence_privilege(c.oid, 'SELECT') \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_sequence s ON s.seqrelid = c.oid \
             LEFT JOIN pg_catalog.pg_sequences pgs \
                ON pgs.schemaname = n.nspname AND pgs.sequencename = c.relname \
             WHERE c.relkind = 'S' \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend de \
                                  WHERE de.objid = c.oid AND de.deptype = 'e') \
             ORDER BY n.nspname, c.relname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("sequences", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let schema: String = r.get(0);
        let name: String = r.get(1);
        let mut seq = PgSequence {
            schema: schema.clone(),
            name: name.clone(),
            owner: r.get(2),
            data_type: r.get(3),
            start: r.get(4),
            increment: r.get(5),
            min_value: r.get(6),
            max_value: r.get(7),
            cache: r.get(8),
            cycle: r.get(9),
            last_value: None,
            is_called: false,
            acl: r.get::<_, Option<Vec<String>>>(11).unwrap_or_default(),
            comment: r.get(12),
            owned_by: r.get(13),
        };
        let readable: bool = r.get(14);
        let selectable: bool = r.get(15);
        // `pg_sequences.last_value` is NULL both for a sequence that was never
        // called AND for one this role may not read, and the recommended
        // read-only source role holds no sequence privilege at all. Silently
        // taking the second case for the first restored every sequence to its
        // START value — and the destination read-back, equally unable to tell,
        // agreed. Refuse instead of guessing.
        if !readable {
            return Err(BackupError::phase(
                Phase::Analyze,
                format!(
                    "cannot read the current value of sequence {schema}.{name}: the source role                      holds neither SELECT nor USAGE on it. Without it every restored sequence                      would silently reset to its start value. Grant it, e.g.                      `GRANT SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO <source role>;`"
                ),
            ));
        }
        if selectable {
            // The sequence relation itself carries `is_called` separately from
            // `last_value`, which `pg_sequences` folds into one nullable column:
            // `setval(seq, 50, false)` is otherwise indistinguishable from a
            // fresh sequence.
            let qual = crate::ddl::quote_qualified(&schema, &name);
            let row = client
                .query_one(&format!("SELECT last_value, is_called FROM {qual}"), &[])
                .await
                .map_err(|e| analyze_err(&format!("sequence state {schema}.{name}"), e))?;
            seq.last_value = Some(row.get(0));
            seq.is_called = row.get(1);
        } else {
            let last_value: Option<i64> = r.get(10);
            seq.is_called = last_value.is_some();
            seq.last_value = last_value;
        }
        out.push((schema, seq));
    }
    Ok(out)
}

/// Returns `(view_oid, schema_name, view)`; the oid attaches matview indexes.
async fn gather_views(client: &Client) -> Result<Vec<(i64, String, PgView)>> {
    let rows = client
        .query(
            "SELECT n.nspname::text, c.relname::text, \
                    pg_catalog.pg_get_userbyid(c.relowner)::text, \
                    pg_catalog.pg_get_viewdef(c.oid, true)::text, \
                    (c.relkind = 'm'), c.relacl::text[], \
                    pg_catalog.obj_description(c.oid, 'pg_class')::text, \
                    (SELECT array_agg(DISTINCT (sn.nspname || '.' || sc.relname)::text \
                                      ORDER BY (sn.nspname || '.' || sc.relname)::text) \
                     FROM pg_catalog.pg_depend dep \
                     JOIN pg_catalog.pg_rewrite rw ON rw.oid = dep.objid \
                     JOIN pg_catalog.pg_class sc ON sc.oid = dep.refobjid \
                     JOIN pg_catalog.pg_namespace sn ON sn.oid = sc.relnamespace \
                     WHERE dep.classid = 'pg_rewrite'::regclass \
                       AND dep.refclassid = 'pg_class'::regclass \
                       AND rw.ev_class = c.oid AND sc.oid <> c.oid \
                       AND sc.relkind IN ('v', 'm')), \
                    c.oid::int8 \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('v', 'm') \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                                  WHERE d.objid = c.oid AND d.deptype = 'e') \
             ORDER BY n.nspname, c.relname",
            &[],
        )
        .await
        .map_err(|e| analyze_err("views", e))?;
    Ok(rows
        .iter()
        .map(|r| {
            let schema: String = r.get(0);
            let view = PgView {
                schema: schema.clone(),
                name: r.get(1),
                owner: r.get(2),
                definition: r.get(3),
                materialized: r.get(4),
                indexes: Vec::new(),
                depends_on: r.get::<_, Option<Vec<String>>>(7).unwrap_or_default(),
                acl: r.get::<_, Option<Vec<String>>>(5).unwrap_or_default(),
                comment: r.get(6),
            };
            (r.get(8), schema, view)
        })
        .collect())
}

async fn gather_functions(client: &Client, major: u32) -> Result<Vec<(String, PgFunction)>> {
    // `prokind` exists on pg 11+; on 10 use proisagg/proiswindow. Exclude
    // aggregates/window funcs (pg_get_functiondef errors on them).
    let kind_filter = if major >= 11 {
        "AND p.prokind IN ('f', 'p')"
    } else {
        "AND NOT p.proisagg AND NOT p.proiswindow"
    };
    let sql = format!(
        "SELECT n.nspname::text, p.proname::text, \
                pg_catalog.pg_get_function_identity_arguments(p.oid)::text, \
                pg_catalog.pg_get_functiondef(p.oid)::text, \
                pg_catalog.pg_get_userbyid(p.proowner)::text, p.proacl::text[], \
                pg_catalog.obj_description(p.oid, 'pg_proc')::text \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
           {kind_filter} \
           AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                              WHERE d.objid = p.oid AND d.deptype = 'e') \
         ORDER BY n.nspname, p.proname"
    );
    let rows = client
        .query(&sql, &[])
        .await
        .map_err(|e| analyze_err("functions", e))?;
    Ok(rows
        .iter()
        .map(|r| {
            let schema: String = r.get(0);
            let func = PgFunction {
                schema: schema.clone(),
                name: r.get(1),
                signature: r.get(2),
                definition: r.get(3),
                owner: r.get(4),
                acl: r.get::<_, Option<Vec<String>>>(5).unwrap_or_default(),
                comment: r.get(6),
            };
            (schema, func)
        })
        .collect())
}

/// One object class this build cannot reproduce, plus the offenders found.
struct UnsupportedClass {
    /// What the class is, phrased for an operator.
    what: &'static str,
    /// Up to [`MAX_REPORTED`] example names.
    names: Vec<String>,
    /// How many were found in total.
    total: usize,
}

const MAX_REPORTED: usize = 5;

/// Probe for object classes that are absent from [`PgPlanPayload`].
///
/// Anything not in the model is invisible to `verify_catalog`, which compares
/// the source model against a re-introspection *through the same model* — so an
/// unmodelled class is lost silently and then certified as a faithful copy.
/// Each probe below is one `SELECT` over `pg_catalog`.
async fn find_unsupported_objects(
    client: &Client,
    database: &str,
    major: u32,
) -> Result<Vec<UnsupportedClass>> {
    // `(what, name expression, from/where clause)`. The name expression is
    // `text`; the query is wrapped so every probe shares one code path.
    let mut probes: Vec<(&'static str, &'static str)> = vec![
        (
            "triggers",
            "SELECT (n.nspname || '.' || c.relname || '.' || t.tgname)::text \
             FROM pg_catalog.pg_trigger t \
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE NOT t.tgisinternal \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'",
        ),
        (
            "row-level security policies",
            "SELECT (n.nspname || '.' || c.relname || '.' || p.polname)::text \
             FROM pg_catalog.pg_policy p \
             JOIN pg_catalog.pg_class c ON c.oid = p.polrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'",
        ),
        (
            "tables with row-level security enabled",
            "SELECT (n.nspname || '.' || c.relname)::text \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE (c.relrowsecurity OR c.relforcerowsecurity) \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'",
        ),
        (
            "user-defined types (enum, domain, composite or range)",
            "SELECT (n.nspname || '.' || t.typname)::text \
             FROM pg_catalog.pg_type t \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND (t.typtype IN ('e', 'd', 'r') \
                    OR (t.typtype = 'c' AND EXISTS (SELECT 1 FROM pg_catalog.pg_class rc \
                                                    WHERE rc.oid = t.typrelid \
                                                      AND rc.relkind = 'c'))) \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                                  WHERE d.objid = t.oid AND d.deptype = 'e')",
        ),
        (
            "aggregate or window functions",
            "SELECT (n.nspname || '.' || p.proname)::text \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND p.prokind IN ('a', 'w') \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                                  WHERE d.objid = p.oid AND d.deptype = 'e')",
        ),
        (
            "foreign tables",
            "SELECT (n.nspname || '.' || c.relname)::text \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'f' \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'",
        ),
        (
            "view options such as WITH CHECK OPTION or security_barrier",
            "SELECT (n.nspname || '.' || c.relname)::text \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('v', 'm') AND c.reloptions IS NOT NULL \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'",
        ),
        (
            "column-level privileges",
            "SELECT (n.nspname || '.' || c.relname || '.' || a.attname)::text \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE a.attacl IS NOT NULL AND NOT a.attisdropped \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'",
        ),
        (
            "default privileges (ALTER DEFAULT PRIVILEGES)",
            "SELECT (COALESCE(n.nspname::text || '.', '') \
                     || pg_catalog.pg_get_userbyid(da.defaclrole)::text \
                     || ':' || da.defaclobjtype::text)::text \
             FROM pg_catalog.pg_default_acl da \
             LEFT JOIN pg_catalog.pg_namespace n ON n.oid = da.defaclnamespace",
        ),
        (
            "inheritance children whose inherited column is locally NOT NULL",
            "SELECT (n.nspname || '.' || c.relname || '.' || a.attname)::text \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE a.attnotnull AND NOT a.attislocal AND a.attnum > 0 \
               AND NOT a.attisdropped AND NOT c.relispartition \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhrelid = c.oid) \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i \
                               JOIN pg_catalog.pg_attribute pa \
                                 ON pa.attrelid = i.inhparent AND pa.attname = a.attname \
                               WHERE i.inhrelid = c.oid AND pa.attnotnull)",
        ),
        (
            "large objects",
            "SELECT lo.oid::text FROM pg_catalog.pg_largeobject_metadata lo",
        ),
        (
            "user-defined collations",
            "SELECT (n.nspname || '.' || co.collname)::text \
             FROM pg_catalog.pg_collation co \
             JOIN pg_catalog.pg_namespace n ON n.oid = co.collnamespace \
             WHERE n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                                  WHERE d.objid = co.oid AND d.deptype = 'e')",
        ),
        (
            "rules other than a view's own _RETURN rule",
            "SELECT (n.nspname || '.' || c.relname || '.' || r.rulename)::text \
             FROM pg_catalog.pg_rewrite r \
             JOIN pg_catalog.pg_class c ON c.oid = r.ev_class \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE r.rulename <> '_RETURN' \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'",
        ),
        (
            "reg* columns, whose binary COPY representation is a raw OID that \
             names a different object on the destination",
            "SELECT (n.nspname || '.' || c.relname || '.' || a.attname || ' ' || \
                     pg_catalog.format_type(a.atttypid, a.atttypmod))::text \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped \
               AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               AND pg_catalog.format_type(a.atttypid, NULL) ~ \
                   '^reg(class|proc|procedure|type|namespace|role|oper|operator|config|dictionary|collation)(\\[\\])?$'",
        ),
        (
            "logical-replication publications",
            "SELECT p.pubname::text FROM pg_catalog.pg_publication p",
        ),
        (
            "logical-replication subscriptions",
            "SELECT s.subname::text FROM pg_catalog.pg_subscription s \
             WHERE s.subdbid = (SELECT d.oid FROM pg_catalog.pg_database d \
                                WHERE d.datname = pg_catalog.current_database())",
        ),
        (
            "event triggers",
            "SELECT e.evtname::text FROM pg_catalog.pg_event_trigger e",
        ),
        (
            "identifiers containing a dot, which this build's qualified \
             references cannot name unambiguously",
            "SELECT name FROM ( \
               SELECT ('schema ' || n.nspname)::text AS name \
                 FROM pg_catalog.pg_namespace n \
                WHERE n.nspname LIKE '%.%' \
                  AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               UNION ALL \
               SELECT ('relation ' || n.nspname || '.' || c.relname)::text \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                WHERE c.relname LIKE '%.%' \
                  AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
               UNION ALL \
               SELECT ('column ' || n.nspname || '.' || c.relname || '.' || a.attname)::text \
                 FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                WHERE a.attname LIKE '%.%' AND a.attnum > 0 AND NOT a.attisdropped \
                  AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_' \
             ) AS dotted",
        ),
    ];
    // PostgreSQL 18 catalogues every NOT NULL constraint in `pg_constraint`
    // (contype 'n'), which makes two new things expressible that this build
    // reads as a plain `attnotnull` and would therefore rebuild as a *default
    // named, validated* constraint without the read-back noticing: an
    // operator-chosen constraint name, and a NOT VALID not-null constraint
    // (which does not actually forbid existing NULLs).
    if major >= 18 {
        probes.push((
            "NOT NULL constraints that are named or NOT VALID (PostgreSQL 18+)",
            "SELECT (n.nspname || '.' || c.relname || '.' || co.conname                      || CASE WHEN co.convalidated THEN '' ELSE ' NOT VALID' END)::text              FROM pg_catalog.pg_constraint co              JOIN pg_catalog.pg_class c ON c.oid = co.conrelid              JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace              WHERE co.contype = 'n' AND co.conislocal                AND n.nspname <> 'information_schema' AND left(n.nspname, 3) <> 'pg_'                AND (NOT co.convalidated OR co.conname !~ '_not_null$')",
        ));
    }
    // `prokind` is PostgreSQL 11+; on 10 the same split lives in two booleans.
    if major < 11 {
        for probe in &mut probes {
            if probe.0 == "aggregate or window functions" {
                probe.1 = "SELECT (n.nspname || '.' || p.proname)::text \
                           FROM pg_catalog.pg_proc p \
                           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                           WHERE n.nspname <> 'information_schema' \
                             AND left(n.nspname, 3) <> 'pg_' \
                             AND (p.proisagg OR p.proiswindow) \
                             AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
                                                WHERE d.objid = p.oid AND d.deptype = 'e')";
            }
        }
    }

    let mut found = Vec::new();
    for (what, sql) in probes {
        let rows = client
            .query(&format!("{sql} ORDER BY 1"), &[])
            .await
            .map_err(|e| analyze_err(&format!("probe {what} in {database}"), e))?;
        if rows.is_empty() {
            continue;
        }
        found.push(UnsupportedClass {
            what,
            names: rows
                .iter()
                .take(MAX_REPORTED)
                .map(|r| r.get::<_, String>(0))
                .collect(),
            total: rows.len(),
        });
    }
    Ok(found)
}

/// Turn the probe result into a refusal, or a loud warning when the operator has
/// explicitly accepted a partial copy.
fn report_unsupported(found: Vec<UnsupportedClass>, allowed: bool) -> Result<()> {
    if found.is_empty() {
        return Ok(());
    }
    let detail = found
        .iter()
        .map(|class| {
            let mut names = class.names.join(", ");
            if class.total > class.names.len() {
                names.push_str(&format!(" and {} more", class.total - class.names.len()));
            }
            format!("{} ({}): {names}", class.what, class.total)
        })
        .collect::<Vec<_>>()
        .join("; ");
    if allowed {
        tracing::warn!(
            unsupported = %detail,
            "allow_unsupported_objects is set: this backup is NOT a 1:1 copy"
        );
        return Ok(());
    }
    Err(BackupError::phase(
        Phase::Analyze,
        format!(
            "this build cannot reproduce the following objects, and the catalog read-back \
             cannot detect their absence either, so the run would report a verified copy of a \
             cluster that had lost them: {detail}. Set allow_unsupported_objects=true to accept \
             a knowingly partial copy."
        ),
    ))
}

// --- plan assembly (pure) ----------------------------------------------------

/// Build a self-contained [`BackupPlan`] from the payload. One data-stream item
/// per table that holds its own rows: ordinary tables and partitioned *parents*
/// (a parent `COPY` reads every partition), but NOT child partitions (their data
/// rides the parent's `COPY`, so streaming them too would duplicate rows).
pub fn build_plan(payload: &PgPlanPayload, created_at: String) -> BackupPlan {
    let mut items = Vec::new();
    let mut total: u64 = 0;
    let mut table_count = 0usize;
    for db in &payload.databases {
        let all: Vec<&PgTable> = db.schemas.iter().flat_map(|s| s.tables.iter()).collect();
        // Every table that at least one other table classically inherits from.
        let inheritance_parents: HashSet<&str> = all
            .iter()
            .flat_map(|t| t.inherits.iter().map(|p| p.as_str()))
            .collect();
        // Partition children by parent, to attribute their size to the parent.
        let mut partitions: HashMap<&str, Vec<&PgTable>> = HashMap::new();
        for t in &all {
            if let Some(p) = &t.partition_of {
                partitions.entry(p.parent.as_str()).or_default().push(t);
            }
        }
        for schema in &db.schemas {
            for table in &schema.tables {
                table_count += 1;
                if table.partition_of.is_some() {
                    continue;
                }
                // Generated columns cannot be COPY'd back in, so the COPY column
                // list (both directions) omits them.
                let cols: Vec<String> = table
                    .columns
                    .iter()
                    .filter(|c| c.generated.is_none())
                    .map(|c| c.name.clone())
                    .collect();
                let qual = format!("{}.{}", table.schema, table.name);
                // `pg_total_relation_size` of a partitioned parent excludes its
                // partitions, and the partitions are not items of their own, so
                // a multi-terabyte partitioned table reported ~0 planned bytes
                // and every progress percentage derived from it was fiction.
                let bytes = table.estimated_bytes.max(0) as u64;
                let bytes = bytes.saturating_add(partition_subtree_bytes(&qual, &partitions));
                total = total.saturating_add(bytes);
                let id = items.len() as u32;
                items.push(PlanItem {
                    id,
                    ordinal: id,
                    kind: "table".to_string(),
                    name: format!("{}.{}.{}", db.name, table.schema, table.name),
                    estimated_bytes: bytes,
                    meta: serde_json::json!({
                        "database": db.name,
                        "schema": table.schema,
                        "table": table.name,
                        "columns": cols,
                        "only": table.kind != "p" && inheritance_parents.contains(qual.as_str()),
                    }),
                });
            }
        }
    }
    let source_summary = format!(
        "PostgreSQL {} cluster: {} role(s), {} database(s), {} table(s), {} data item(s)",
        payload.server_version,
        payload.roles.len(),
        payload.databases.len(),
        table_count,
        items.len(),
    );
    BackupPlan {
        format_version: PLAN_FORMAT_VERSION,
        module: "postgres".to_string(),
        mode: BackupMode::Copy1to1,
        created_at,
        source_summary,
        items,
        estimated_bytes: total,
        integrity: IntegritySpec::default(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Total on-disk size of a partitioned table's whole partition tree.
///
/// Iterative (a worklist, not recursion) and visit-once, so a deep hierarchy
/// cannot exhaust the stack and a malformed cycle cannot loop forever.
fn partition_subtree_bytes(root: &str, partitions: &HashMap<&str, Vec<&PgTable>>) -> u64 {
    let mut total = 0u64;
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue = vec![root.to_string()];
    while let Some(parent) = queue.pop() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        for child in partitions.get(parent.as_str()).into_iter().flatten() {
            total = total.saturating_add(child.estimated_bytes.max(0) as u64);
            queue.push(format!("{}.{}", child.schema, child.name));
        }
    }
    total
}

/// Current UTC time as an RFC3339 string (`YYYY-MM-DDThh:mm:ssZ`). Implemented
/// without a date crate (Howard Hinnant's civil-from-days), so the module — not
/// the clock-free core — stamps the plan's `created_at`.
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_rfc3339(secs)
}

fn format_unix_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as i64;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert days-since-Unix-epoch to (year, month, day). Howard Hinnant's
/// `civil_from_days`, valid for the full proleptic Gregorian range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_locale_catalog_projection_covers_supported_majors() {
        for major in 10..=14 {
            assert_eq!(
                database_locale_projection(major),
                "'c'::text, NULL::text, NULL::text"
            );
        }
        assert_eq!(
            database_locale_projection(15),
            "d.datlocprovider::text, d.daticulocale::text, NULL::text"
        );
        assert_eq!(
            database_locale_projection(16),
            "d.datlocprovider::text, d.daticulocale::text, d.daticurules::text"
        );
        for major in 17..=18 {
            assert_eq!(
                database_locale_projection(major),
                "d.datlocprovider::text, d.datlocale::text, d.daticurules::text"
            );
        }
    }

    #[test]
    fn database_locale_provider_catalog_codes_are_strict() {
        assert_eq!(
            parse_locale_provider("c").expect("libc"),
            PgLocaleProvider::Libc
        );
        assert_eq!(
            parse_locale_provider("i").expect("icu"),
            PgLocaleProvider::Icu
        );
        assert_eq!(
            parse_locale_provider("b").expect("builtin"),
            PgLocaleProvider::Builtin
        );
        assert!(parse_locale_provider("future-provider").is_err());
    }

    #[test]
    fn constraints_query_excludes_pg18_not_null_catalog_constraints() {
        assert!(CONSTRAINTS_QUERY.contains("con.contype IN ('p', 'u', 'f', 'c', 'x')"));
        assert!(!CONSTRAINTS_QUERY.contains("'n'"));
    }

    #[test]
    fn rfc3339_known_vectors() {
        assert_eq!(format_unix_rfc3339(0), "1970-01-01T00:00:00Z");
        // 1_700_000_000 = 2023-11-14T22:13:20Z
        assert_eq!(format_unix_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // 1_000_000_000 = 2001-09-09T01:46:40Z
        assert_eq!(format_unix_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
    }

    #[test]
    fn build_plan_streams_only_data_bearing_tables() {
        let payload = crate::model::test_fixture();
        let plan = build_plan(&payload, "2026-01-01T00:00:00Z".to_string());

        assert_eq!(plan.module, "postgres");
        assert_eq!(plan.format_version, PLAN_FORMAT_VERSION);
        assert_eq!(plan.mode, BackupMode::Copy1to1);
        assert_eq!(plan.created_at, "2026-01-01T00:00:00Z");

        // The fixture has one ordinary table (no partitions) → one stream item.
        assert_eq!(plan.items.len(), 1);
        let item = &plan.items[0];
        assert_eq!(item.kind, "table");
        assert_eq!(item.name, "appdb.app.accounts");
        assert_eq!(item.estimated_bytes, 81_920);
        assert_eq!(plan.estimated_bytes, 81_920);
        assert_eq!(item.meta["database"], "appdb");
        assert_eq!(item.meta["schema"], "app");
        assert_eq!(item.meta["table"], "accounts");
        assert_eq!(
            item.meta["columns"],
            serde_json::json!(["id", "email"]),
            "COPY column list omits generated columns"
        );

        // The payload roundtrips back out of the plan unchanged.
        let back: PgPlanPayload = serde_json::from_value(plan.payload).expect("payload");
        assert_eq!(back, payload);
    }

    #[test]
    fn build_plan_skips_child_partitions() {
        let mut payload = crate::model::test_fixture();
        // Add a partitioned parent + one child to the existing schema.
        let schema = &mut payload.databases[0].schemas[0];
        schema.tables.push(PgTable {
            schema: "app".to_string(),
            name: "events".to_string(),
            kind: "p".to_string(),
            partition_key: Some("RANGE (created_at)".to_string()),
            estimated_bytes: 0,
            ..Default::default()
        });
        schema.tables.push(PgTable {
            schema: "app".to_string(),
            name: "events_2026".to_string(),
            kind: "r".to_string(),
            partition_of: Some(PgPartitionOf {
                parent: "app.events".to_string(),
                bound: "FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')".to_string(),
            }),
            estimated_bytes: 4096,
            ..Default::default()
        });
        let plan = build_plan(&payload, "x".to_string());
        // accounts + events parent = 2 items; the child partition is skipped.
        let names: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["appdb.app.accounts", "appdb.app.events"]);
        // The parent's own `pg_total_relation_size` is ~0 — its bytes live in
        // its partitions, which are not items of their own.
        let events = &plan.items[1];
        assert_eq!(events.estimated_bytes, 4096);
        assert_eq!(plan.estimated_bytes, 81_920 + 4096);
        assert_eq!(
            events.meta["only"], false,
            "a partitioned parent must NOT be read with ONLY: all of its rows live in partitions"
        );
    }

    /// A classic inheritance child is its own data item, so the parent must be
    /// streamed with `ONLY` — otherwise the parent's item carries the child's
    /// rows too and the destination ends up with them twice.
    #[test]
    fn an_inheritance_parent_is_planned_with_only() {
        let mut payload = crate::model::test_fixture();
        let schema = &mut payload.databases[0].schemas[0];
        schema.tables.push(PgTable {
            schema: "app".to_string(),
            name: "log".to_string(),
            kind: "r".to_string(),
            estimated_bytes: 1024,
            ..Default::default()
        });
        schema.tables.push(PgTable {
            schema: "app".to_string(),
            name: "log_2025".to_string(),
            kind: "r".to_string(),
            inherits: vec!["app.log".to_string()],
            estimated_bytes: 2048,
            ..Default::default()
        });
        let plan = build_plan(&payload, "x".to_string());
        let names: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["appdb.app.accounts", "appdb.app.log", "appdb.app.log_2025"],
            "an inheritance child holds its own rows and is its own item"
        );
        assert_eq!(plan.items[1].meta["only"], true, "the parent reads ONLY");
        assert_eq!(
            plan.items[2].meta["only"], false,
            "the child has no children of its own"
        );
        assert_eq!(plan.items[0].meta["only"], false);
    }

    /// Deep partition trees must all be attributed to the root item.
    #[test]
    fn nested_partition_sizes_roll_up_to_the_root_item() {
        let table = |name: &str, parent: Option<&str>, kind: &str, bytes: i64| PgTable {
            schema: "app".to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            partition_of: parent.map(|p| PgPartitionOf {
                parent: p.to_string(),
                bound: "DEFAULT".to_string(),
            }),
            estimated_bytes: bytes,
            ..Default::default()
        };
        let payload = PgPlanPayload {
            databases: vec![PgDatabase {
                name: "appdb".to_string(),
                schemas: vec![PgSchema {
                    name: "app".to_string(),
                    tables: vec![
                        table("sales", None, "p", 0),
                        table("north", Some("app.sales"), "p", 0),
                        table("apple", Some("app.north"), "r", 700),
                        table("south", Some("app.sales"), "r", 300),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let plan = build_plan(&payload, "x".to_string());
        assert_eq!(plan.items.len(), 1, "only the root is a data item");
        assert_eq!(plan.items[0].estimated_bytes, 1000);
        assert_eq!(plan.estimated_bytes, 1000);
    }
}
