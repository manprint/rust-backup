//! DDL reconstruction from a [`PgPlanPayload`] (plan Phase 2.3).
//!
//! Pure functions: payload → ordered SQL. No I/O, fully golden-tested. The output
//! is grouped to match the restore order (Phase 2.6):
//!
//! cluster: roles → memberships → tablespaces → databases
//! per-db:  pre_data  = extensions, schemas, sequences (no value yet), tables
//!                      (columns only — constraints/indexes deferred), functions,
//!                      views, schema/object comments
//!          post_data = constraints (PK/UNIQUE then FK), indexes, sequence
//!                      ownership + values, grants, owners, table/column comments
//!
//! Tables are created WITHOUT constraints/indexes so the bulk `COPY` loads fast;
//! they are added in `post_data` after the data lands.
//!
//! Best-effort areas (documented, exercised by the e2e apply test, not goldens):
//! role/database GUC settings and ACL→GRANT rendering for uncommon privilege
//! letters. Roles are created without passwords (not captured — see `model`).

use std::collections::{HashMap, HashSet};

use crate::model::*;
use rb_core::error::{BackupError, Phase, Result};

/// Cluster-wide DDL split by the connection it must run on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterDdl {
    /// Run on the bootstrap connection BEFORE creating databases.
    pub roles: Vec<String>,
    /// `CREATE DATABASE` (+ per-db GUCs); run on the bootstrap connection.
    pub databases: Vec<String>,
    /// Per-database object DDL; each runs on a connection to that database.
    pub per_database: Vec<DatabaseDdl>,
}

/// DDL for one database, split around the data-load step.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatabaseDdl {
    pub name: String,
    /// Applied before `COPY` (structure only).
    pub pre_data: Vec<String>,
    /// Applied after `COPY` (constraints, indexes, sequence values, grants…).
    pub post_data: Vec<String>,
}

// --- identifier / literal quoting --------------------------------------------

/// Double-quote an identifier, escaping embedded quotes. Always quoting is safe
/// for reserved words and mixed case alike.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `"schema"."name"`.
pub fn quote_qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Single-quote a string literal, escaping embedded quotes.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// --- cluster-global emitters -------------------------------------------------

/// `CREATE ROLE … WITH …;` (no password — not captured). Excludes role GUCs,
/// which [`alter_role_settings`] emits separately.
pub fn create_role(r: &PgRole) -> String {
    let mut opts = vec![
        if r.login { "LOGIN" } else { "NOLOGIN" }.to_string(),
        if r.superuser {
            "SUPERUSER"
        } else {
            "NOSUPERUSER"
        }
        .to_string(),
        if r.createdb { "CREATEDB" } else { "NOCREATEDB" }.to_string(),
        if r.createrole {
            "CREATEROLE"
        } else {
            "NOCREATEROLE"
        }
        .to_string(),
        if r.inherit { "INHERIT" } else { "NOINHERIT" }.to_string(),
        if r.replication {
            "REPLICATION"
        } else {
            "NOREPLICATION"
        }
        .to_string(),
        if r.bypassrls {
            "BYPASSRLS"
        } else {
            "NOBYPASSRLS"
        }
        .to_string(),
    ];
    if r.connlimit >= 0 {
        opts.push(format!("CONNECTION LIMIT {}", r.connlimit));
    }
    if let Some(vu) = &r.valid_until {
        opts.push(format!("VALID UNTIL {}", quote_literal(vu)));
    }
    format!(
        "CREATE ROLE {} WITH {};",
        quote_ident(&r.name),
        opts.join(" ")
    )
}

/// `ALTER ROLE … SET k TO v;` for each role GUC.
pub fn alter_role_settings(r: &PgRole) -> Result<Vec<String>> {
    r.config
        .iter()
        .map(|kv| {
            Ok(format!(
                "ALTER ROLE {} SET {};",
                quote_ident(&r.name),
                set_clause(kv)?
            ))
        })
        .collect()
}

/// `GRANT group TO member [WITH ADMIN OPTION];`.
pub fn grant_membership(m: &PgMembership) -> String {
    let admin = if m.admin_option {
        " WITH ADMIN OPTION"
    } else {
        ""
    };
    format!(
        "GRANT {} TO {}{};",
        quote_ident(&m.role),
        quote_ident(&m.member),
        admin
    )
}

/// `CREATE TABLESPACE … OWNER … LOCATION …;`.
pub fn create_tablespace(t: &PgTablespace) -> String {
    format!(
        "CREATE TABLESPACE {} OWNER {} LOCATION {};",
        quote_ident(&t.name),
        quote_ident(&t.owner),
        quote_literal(&t.location)
    )
}

/// `CREATE DATABASE … WITH TEMPLATE = template0 …;`, rendered for the
/// destination major. `template0` is required when encoding or locale differ
/// from the destination cluster's `template1`.
pub fn create_database(d: &PgDatabase, destination_major: u32) -> Result<String> {
    if destination_major < 10 {
        return Err(BackupError::phase(
            Phase::Apply,
            format!("cannot create database on unsupported PostgreSQL {destination_major}"),
        ));
    }

    // Historical plans did not capture enough data to distinguish providers;
    // preserve their former libc-style interpretation.
    let provider = d.locale_provider.unwrap_or(PgLocaleProvider::Libc);
    match provider {
        PgLocaleProvider::Icu if destination_major < 15 => {
            return Err(BackupError::phase(
                Phase::Apply,
                format!(
                    "database {:?} uses ICU, unsupported by PostgreSQL {destination_major}",
                    d.name
                ),
            ));
        }
        PgLocaleProvider::Builtin if destination_major < 17 => {
            return Err(BackupError::phase(
                Phase::Apply,
                format!(
                    "database {:?} uses the builtin locale provider, unsupported by PostgreSQL {destination_major}",
                    d.name
                ),
            ));
        }
        _ => {}
    }
    if d.icu_rules.is_some() && provider != PgLocaleProvider::Icu {
        return Err(BackupError::phase(
            Phase::Apply,
            format!("database {:?} has ICU rules but is not using ICU", d.name),
        ));
    }
    if d.icu_rules.is_some() && destination_major < 16 {
        return Err(BackupError::phase(
            Phase::Apply,
            format!(
                "database {:?} has ICU rules, unsupported by PostgreSQL {destination_major}",
                d.name
            ),
        ));
    }
    if matches!(provider, PgLocaleProvider::Icu | PgLocaleProvider::Builtin)
        && d.provider_locale.is_none()
    {
        return Err(BackupError::phase(
            Phase::Apply,
            format!(
                "database {:?} is missing its {} locale",
                d.name,
                match provider {
                    PgLocaleProvider::Icu => "ICU",
                    PgLocaleProvider::Builtin => "builtin",
                    PgLocaleProvider::Libc => "provider",
                }
            ),
        ));
    }

    let mut s = format!(
        "CREATE DATABASE {} WITH TEMPLATE = template0 OWNER = {} ENCODING = {}",
        quote_ident(&d.name),
        quote_ident(&d.owner),
        quote_literal(&d.encoding),
    );
    if destination_major >= 15 {
        let provider_name = match provider {
            PgLocaleProvider::Libc => "libc",
            PgLocaleProvider::Icu => "icu",
            PgLocaleProvider::Builtin => "builtin",
        };
        s.push_str(&format!(" LOCALE_PROVIDER = {provider_name}"));
    }
    // LC_COLLATE/LC_CTYPE are understood by every supported major. The LOCALE
    // shortcut was added after PostgreSQL 10, so do not use it here.
    s.push_str(&format!(" LC_COLLATE = {}", quote_literal(&d.collate)));
    s.push_str(&format!(" LC_CTYPE = {}", quote_literal(&d.ctype)));
    if let Some(locale) = &d.provider_locale {
        let option = match provider {
            PgLocaleProvider::Icu => "ICU_LOCALE",
            PgLocaleProvider::Builtin => "BUILTIN_LOCALE",
            PgLocaleProvider::Libc => {
                return Err(BackupError::phase(
                    Phase::Apply,
                    format!("database {:?} has a provider locale but uses libc", d.name),
                ));
            }
        };
        s.push_str(&format!(" {option} = {}", quote_literal(locale)));
    }
    if let Some(rules) = &d.icu_rules {
        s.push_str(&format!(" ICU_RULES = {}", quote_literal(rules)));
    }
    if let Some(ts) = &d.tablespace {
        s.push_str(&format!(" TABLESPACE = {}", quote_ident(ts)));
    }
    if d.connlimit >= 0 {
        s.push_str(&format!(" CONNECTION LIMIT = {}", d.connlimit));
    }
    if !d.allow_connections {
        s.push_str(" ALLOW_CONNECTIONS = false");
    }
    s.push(';');
    Ok(s)
}

/// `ALTER DATABASE … SET k TO v;` for each database GUC.
pub fn alter_database_settings(d: &PgDatabase) -> Result<Vec<String>> {
    d.config
        .iter()
        .map(|kv| {
            Ok(format!(
                "ALTER DATABASE {} SET {};",
                quote_ident(&d.name),
                set_clause(kv)?
            ))
        })
        .collect()
}

// --- per-database emitters ---------------------------------------------------

/// `CREATE EXTENSION IF NOT EXISTS … WITH SCHEMA … VERSION …;`.
pub fn create_extension(e: &PgExtension) -> String {
    format!(
        "CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {} VERSION {};",
        quote_ident(&e.name),
        quote_ident(&e.schema),
        quote_literal(&e.version)
    )
}

/// `CREATE SCHEMA … AUTHORIZATION …;` (public is created via IF NOT EXISTS).
pub fn create_schema(s: &PgSchema) -> String {
    if s.name == "public" {
        format!(
            "CREATE SCHEMA IF NOT EXISTS {} AUTHORIZATION {};",
            quote_ident(&s.name),
            quote_ident(&s.owner)
        )
    } else {
        format!(
            "CREATE SCHEMA {} AUTHORIZATION {};",
            quote_ident(&s.name),
            quote_ident(&s.owner)
        )
    }
}

/// `CREATE [UNLOGGED] TABLE …` — columns only for an ordinary/partitioned-parent
/// table, or `PARTITION OF` for a child. Constraints and indexes are emitted
/// separately so the data load is unencumbered.
pub fn create_table(t: &PgTable) -> String {
    let unlogged = if t.persistence == "u" {
        "UNLOGGED "
    } else {
        ""
    };
    let qual = quote_qualified(&t.schema, &t.name);

    let mut s = if let Some(p) = &t.partition_of {
        // Child partition: inherits columns from the parent.
        format!(
            "CREATE {unlogged}TABLE {qual} PARTITION OF {} {}",
            quote_qualified_dotted(&p.parent),
            p.bound
        )
    } else {
        // An inheritance child lists only its own columns: re-listing an
        // inherited one merges it but marks it local, which is a different
        // catalog state from the source's.
        let cols: Vec<String> = t
            .columns
            .iter()
            .filter(|c| c.local || t.inherits.is_empty())
            .map(column_clause)
            .collect();
        if cols.is_empty() {
            // Every column comes from a parent.
            format!("CREATE {unlogged}TABLE {qual} ()")
        } else {
            format!(
                "CREATE {unlogged}TABLE {qual} (\n  {}\n)",
                cols.join(",\n  ")
            )
        }
    };

    // Classic `INHERITS` is not partitioning: the child keeps its own rows and
    // a plain `SELECT` on the parent expands into it. Without this clause the
    // two tables restore unrelated, and because the parent's own `COPY` had
    // already streamed the child's rows (see `copy_out_sql`), the destination
    // ended up holding every child row twice.
    if !t.inherits.is_empty() {
        let parents: Vec<String> = t
            .inherits
            .iter()
            .map(|p| quote_qualified_dotted(p))
            .collect();
        s.push_str(&format!(" INHERITS ({})", parents.join(", ")));
    }
    if let Some(pk) = &t.partition_key {
        s.push_str(&format!(" PARTITION BY {pk}"));
    }
    if !t.reloptions.is_empty() {
        s.push_str(&format!(" WITH ({})", t.reloptions.join(", ")));
    }
    if let Some(ts) = &t.tablespace {
        s.push_str(&format!(" TABLESPACE {}", quote_ident(ts)));
    }
    s.push(';');
    s
}

/// One column definition for a `CREATE TABLE` list.
///
/// Deliberately carries neither `DEFAULT` nor the identity clause: a default may
/// call a user function, and a function may return a table's row type, so
/// inlining either creates a dependency cycle that no fixed table/function
/// order can satisfy. Both are attached afterwards by [`set_column_default`] and
/// [`add_identity`], which is also how `pg_dump` breaks that cycle.
fn column_clause(c: &PgColumn) -> String {
    let mut s = format!("{} {}", quote_ident(&c.name), c.type_name);
    if let Some(coll) = &c.collation {
        s.push_str(&format!(" COLLATE {}", quote_ident(coll)));
    }
    if let Some(expr) = &c.generated {
        s.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"));
    }
    if c.not_null {
        s.push_str(" NOT NULL");
    }
    s
}

/// `ALTER TABLE … ALTER COLUMN … SET DEFAULT …;` — emitted after functions
/// exist, since a default expression may call one.
pub fn set_column_default(t: &PgTable, c: &PgColumn) -> Option<String> {
    let def = c.default.as_ref()?;
    if c.generated.is_some() || c.identity.is_some() {
        return None;
    }
    Some(format!(
        "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {def};",
        quote_qualified(&t.schema, &t.name),
        quote_ident(&c.name)
    ))
}

/// `ALTER TABLE … ALTER COLUMN … ADD GENERATED … AS IDENTITY (…);`
///
/// `seq` is the plan's sequence for this column, when present. Naming it
/// explicitly matters: PostgreSQL would otherwise invent `<table>_<column>_seq`,
/// which is *not* the source name once the table has been renamed — and the
/// `setval` in post_data, which targets the source name, then failed the restore
/// outright. The sequence parameters are part of the identity generator's
/// semantics, so they travel with it rather than being lost.
pub fn add_identity(t: &PgTable, c: &PgColumn, seq: Option<&PgSequence>) -> Option<String> {
    let kind = c.identity.as_ref()?;
    if t.partition_of.is_some() {
        // An identity generator belongs to the partitioned parent; PostgreSQL
        // refuses to add one to a partition.
        return None;
    }
    let when = if kind == "a" { "ALWAYS" } else { "BY DEFAULT" };
    let mut options = Vec::new();
    if let Some(seq) = seq {
        options.push(format!(
            "SEQUENCE NAME {}",
            quote_qualified(&seq.schema, &seq.name)
        ));
        options.push(format!("START WITH {}", seq.start));
        options.push(format!("INCREMENT BY {}", seq.increment));
        options.push(format!("MINVALUE {}", seq.min_value));
        options.push(format!("MAXVALUE {}", seq.max_value));
        options.push(format!("CACHE {}", seq.cache));
        if seq.cycle {
            options.push("CYCLE".to_string());
        }
    }
    let clause = if options.is_empty() {
        String::new()
    } else {
        format!(" ({})", options.join(" "))
    };
    Some(format!(
        "ALTER TABLE {} ALTER COLUMN {} ADD GENERATED {when} AS IDENTITY{clause};",
        quote_qualified(&t.schema, &t.name),
        quote_ident(&c.name)
    ))
}

/// `ALTER TABLE … ADD CONSTRAINT … <def>;` (def from `pg_get_constraintdef`).
pub fn add_constraint(t: &PgTable, c: &PgConstraint) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} {};",
        quote_qualified(&t.schema, &t.name),
        quote_ident(&c.name),
        c.definition
    )
}

/// `CREATE INDEX …;` — the full statement from `pg_get_indexdef`. Returns `None`
/// for an index that backs a constraint (the constraint creates it).
pub fn create_index(i: &PgIndex) -> Option<String> {
    if i.is_constraint {
        return None;
    }
    Some(format!("{};", i.definition))
}

/// `CREATE SEQUENCE …;` (without value/ownership — those go in post_data).
pub fn create_sequence(s: &PgSequence) -> String {
    let mut out = format!(
        "CREATE SEQUENCE {} AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {}",
        quote_qualified(&s.schema, &s.name),
        s.data_type,
        s.increment,
        s.min_value,
        s.max_value,
        s.start,
        s.cache,
    );
    if s.cycle {
        out.push_str(" CYCLE");
    }
    out.push(';');
    out
}

/// `ALTER SEQUENCE … OWNED BY …;` when the sequence is owned by a column.
pub fn sequence_owned_by(s: &PgSequence) -> Option<String> {
    s.owned_by.as_ref().map(|col| {
        format!(
            "ALTER SEQUENCE {} OWNED BY {};",
            quote_qualified(&s.schema, &s.name),
            quote_qualified_dotted(col)
        )
    })
}

/// `SELECT setval(…);` to restore the current sequence value, when known.
pub fn sequence_setval(s: &PgSequence) -> Option<String> {
    s.last_value.map(|v| {
        format!(
            "SELECT pg_catalog.setval({}, {}, {});",
            quote_literal(&format!(
                "{}.{}",
                quote_ident(&s.schema),
                quote_ident(&s.name)
            )),
            v,
            s.is_called
        )
    })
}

/// `CREATE [MATERIALIZED] VIEW … AS …;` (definition from `pg_get_viewdef`).
///
/// A materialized view is created `WITH NO DATA` and populated by
/// [`refresh_view`] in post_data. Creating it with data ran its query in
/// pre_data — against tables that had not been loaded yet — so every
/// materialized view restored empty, and nothing noticed: the model carries no
/// content for it and the per-item digests cover table items only.
pub fn create_view(v: &PgView) -> String {
    let kind = if v.materialized {
        "MATERIALIZED VIEW"
    } else {
        "VIEW"
    };
    let body = v.definition.trim_end();
    if v.materialized {
        let body = body.strip_suffix(';').unwrap_or(body);
        format!(
            "CREATE {kind} {} AS {body} WITH NO DATA;",
            quote_qualified(&v.schema, &v.name)
        )
    } else {
        format!(
            "CREATE {kind} {} AS {body}",
            quote_qualified(&v.schema, &v.name)
        )
    }
}

/// `REFRESH MATERIALIZED VIEW …;` for a materialized view, `None` otherwise.
pub fn refresh_view(v: &PgView) -> Option<String> {
    v.materialized.then(|| {
        format!(
            "REFRESH MATERIALIZED VIEW {};",
            quote_qualified(&v.schema, &v.name)
        )
    })
}

/// `ALTER INDEX <parent> ATTACH PARTITION <child>;`
///
/// `pg_get_indexdef` renders a partitioned table's index as `ON ONLY parent`,
/// which leaves it **invalid** until every child index is attached — so the
/// index silently does not serve queries.
pub fn attach_index(t: &PgTable, i: &PgIndex) -> Option<String> {
    if i.is_constraint {
        // A constraint on the partitioned parent cascades to its partitions and
        // attaches their indexes itself.
        return None;
    }
    let parent = i.attach_to.as_ref()?;
    Some(format!(
        "ALTER INDEX {} ATTACH PARTITION {};",
        quote_qualified_dotted(parent),
        quote_qualified(&t.schema, &i.name)
    ))
}

/// `REVOKE ALL … FROM PUBLIC` (and from the owner) before the object's `GRANT`s.
///
/// Object classes whose *default* ACL is non-empty — `FUNCTION` grants `EXECUTE`
/// to `PUBLIC`, `DATABASE` grants `CONNECT`/`TEMPORARY` — cannot be restored by
/// granting alone: the destination starts from the permissive default, so a
/// source that had revoked it stayed open, and re-granting the owner's own
/// entry materialised the defaults into the ACL, which then failed the catalog
/// read-back. An empty `acl` means "never customised", where the destination
/// default is already right.
pub fn revoke_before_grant(objtype: &str, qual: &str, owner: &str, acl: &[String]) -> Vec<String> {
    if acl.is_empty() {
        return Vec::new();
    }
    vec![
        format!("REVOKE ALL ON {objtype} {qual} FROM PUBLIC;"),
        format!(
            "REVOKE ALL ON {objtype} {qual} FROM {};",
            quote_ident(owner)
        ),
    ]
}

/// The function's `CREATE OR REPLACE FUNCTION …` from `pg_get_functiondef`,
/// emitted verbatim (terminated).
pub fn create_function(f: &PgFunction) -> String {
    let def = f.definition.trim_end();
    if def.ends_with(';') {
        def.to_string()
    } else {
        format!("{def};")
    }
}

// --- comments / ownership / grants -------------------------------------------

/// `ALTER … OWNER TO …;` for a table-like object (TABLE/VIEW/SEQUENCE share the
/// `ALTER TABLE` form for ownership in PostgreSQL).
pub fn alter_table_owner(schema: &str, name: &str, owner: &str) -> String {
    format!(
        "ALTER TABLE {} OWNER TO {};",
        quote_qualified(schema, name),
        quote_ident(owner)
    )
}

/// `COMMENT ON <kind> <name> IS '…';`. `name` must already be quoted/qualified.
pub fn comment_on(kind: &str, name: &str, comment: &str) -> String {
    format!("COMMENT ON {kind} {name} IS {};", quote_literal(comment))
}

/// Render `aclitem` strings (`grantee=privs/grantor`) to `GRANT …` statements for
/// an object. `objtype` is the GRANT target keyword (`TABLE`, `SEQUENCE`,
/// `SCHEMA`, `FUNCTION`, `DATABASE`); `qual` is the already-quoted object name.
pub fn grants_from_acl(objtype: &str, qual: &str, acl: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for item in acl {
        let Some((grantee_raw, rest)) = item.split_once('=') else {
            continue;
        };
        let privs = rest.split('/').next().unwrap_or("");
        let grantee = if grantee_raw.is_empty() {
            "PUBLIC".to_string()
        } else {
            quote_ident(grantee_raw)
        };
        // Separate plain privileges from WITH GRANT OPTION ones (priv letter
        // followed by '*').
        let (plain, grantable) = parse_priv_letters(privs);
        if !plain.is_empty() {
            out.push(format!(
                "GRANT {} ON {objtype} {qual} TO {grantee};",
                plain.join(", ")
            ));
        }
        if !grantable.is_empty() {
            out.push(format!(
                "GRANT {} ON {objtype} {qual} TO {grantee} WITH GRANT OPTION;",
                grantable.join(", ")
            ));
        }
    }
    out
}

// --- assembly ----------------------------------------------------------------

/// Assemble the full, ordered cluster DDL from the payload, adapting database
/// locale syntax to the actual destination PostgreSQL major.
pub fn build_cluster_ddl(payload: &PgPlanPayload, destination_major: u32) -> Result<ClusterDdl> {
    if destination_major < payload.server_major {
        return Err(BackupError::phase(
            Phase::Apply,
            format!(
                "destination PostgreSQL {destination_major} is older than source PostgreSQL {}",
                payload.server_major
            ),
        ));
    }
    let mut roles = Vec::new();
    for r in &payload.roles {
        roles.push(create_role(r));
    }
    for r in &payload.roles {
        roles.extend(alter_role_settings(r)?);
    }
    for m in &payload.memberships {
        roles.push(grant_membership(m));
    }
    for t in &payload.tablespaces {
        roles.push(create_tablespace(t));
    }

    let mut databases = Vec::new();
    for d in &payload.databases {
        databases.push(create_database(d, destination_major)?);
        databases.extend(alter_database_settings(d)?);
    }

    let per_database = payload
        .databases
        .iter()
        .map(|database| build_database_ddl(database, destination_major))
        .collect::<Result<Vec<_>>>()?;

    Ok(ClusterDdl {
        roles,
        databases,
        per_database,
    })
}

fn build_database_ddl(d: &PgDatabase, destination_major: u32) -> Result<DatabaseDdl> {
    let mut pre = Vec::new();
    let mut post = Vec::new();

    // pg_dump opens every dump with this. A `LANGUAGE sql`/`plpgsql` body is
    // name-resolved at creation time by default, so without it a function
    // reading a view or a table that does not exist yet fails, and no fixed
    // emission order can satisfy every dependency direction at once.
    pre.push("SET check_function_bodies = false;".to_string());

    // Schemas before extensions: `CREATE EXTENSION … WITH SCHEMA gis` fails with
    // `schema "gis" does not exist` unless the schema is already there, and the
    // reverse dependency does not exist — a schema never needs an extension to
    // be created. This only ever worked because every fixture installed its
    // extensions into the pre-existing `public`.
    for sc in &d.schemas {
        pre.push(create_schema(sc));
        // `CREATE SCHEMA IF NOT EXISTS public AUTHORIZATION ...` does not alter
        // the pre-created schema. PostgreSQL 15+ also creates it under the
        // virtual pg_database_owner role, so cross-major restores must make
        // ownership and default grants explicit.
        pre.push(format!(
            "ALTER SCHEMA {} OWNER TO {};",
            quote_ident(&sc.name),
            quote_ident(&sc.owner)
        ));
        if sc.name == "public" {
            pre.push("REVOKE ALL ON SCHEMA \"public\" FROM PUBLIC;".to_string());
            if destination_major >= 15 {
                pre.push("REVOKE ALL ON SCHEMA \"public\" FROM \"pg_database_owner\";".to_string());
            }
        }
        if let Some(c) = &sc.comment {
            pre.push(comment_on("SCHEMA", &quote_ident(&sc.name), c));
        }
        post.extend(grants_from_acl("SCHEMA", &quote_ident(&sc.name), &sc.acl));
    }
    for e in &d.extensions {
        pre.push(create_extension(e));
    }

    // PostgreSQL creates an owned sequence itself for every IDENTITY column. Do
    // not create a second sequence before the CREATE TABLE; retain post-data
    // setval/ownership work so the generated sequence receives source state.
    let identity_sequences: HashMap<&str, &PgSequence> = d
        .schemas
        .iter()
        .flat_map(|schema| schema.sequences.iter())
        .filter_map(|seq| seq.owned_by.as_deref().map(|owned| (owned, seq)))
        .collect();
    let identity_owned: HashSet<String> = d
        .schemas
        .iter()
        .flat_map(|schema| {
            schema.tables.iter().flat_map(move |table| {
                table.columns.iter().filter_map(move |column| {
                    column
                        .identity
                        .as_ref()
                        .map(|_| format!("{}.{}.{}", schema.name, table.name, column.name))
                })
            })
        })
        .collect();

    // Sequences (definitions before tables, since column defaults may reference
    // them); values/ownership after data.
    for sc in &d.schemas {
        for seq in &sc.sequences {
            let is_identity_sequence = seq
                .owned_by
                .as_ref()
                .is_some_and(|owned| identity_owned.contains(owned));
            if !is_identity_sequence {
                pre.push(create_sequence(seq));
            }
            let qual = quote_qualified(&seq.schema, &seq.name);
            if !is_identity_sequence {
                post.push(alter_table_owner(&seq.schema, &seq.name, &seq.owner));
                if let Some(o) = sequence_owned_by(seq) {
                    post.push(o);
                }
            }
            if let Some(v) = sequence_setval(seq) {
                post.push(v);
            }
            // Comments and privileges apply to an identity sequence too — only
            // its creation, ownership and column link are PostgreSQL's to
            // manage. Skipping the grants made any cluster that had run
            // `GRANT ... ON ALL SEQUENCES` unrestorable: the read-back saw the
            // destination's empty ACL and failed the run.
            if let Some(c) = &seq.comment {
                post.push(comment_on("SEQUENCE", &qual, c));
            }
            post.extend(grants_from_acl("SEQUENCE", &qual, &seq.acl));
        }
    }

    // Tables: a parent — whether it is partitioned or classically inherited
    // from — must exist before its children. A two-bucket sort left a partition
    // that is itself partitioned ordered by name against its own child.
    let tables: Vec<&PgTable> = topological_tables(d);
    for t in &tables {
        pre.push(create_table(t));
        let qual = quote_qualified(&t.schema, &t.name);
        post.push(alter_table_owner(&t.schema, &t.name, &t.owner));
        if let Some(c) = &t.comment {
            post.push(comment_on("TABLE", &qual, c));
        }
        for col in &t.columns {
            if let Some(c) = &col.comment {
                let cq = format!("{}.{}", qual, quote_ident(&col.name));
                post.push(comment_on("COLUMN", &cq, c));
            }
        }
        post.extend(grants_from_acl("TABLE", &qual, &t.acl));
    }

    // Identity generators, named and parameterised exactly as on the source.
    // Before the data load, because `COPY` supplies the column's values.
    for t in &tables {
        for col in &t.columns {
            let key = format!("{}.{}.{}", t.schema, t.name, col.name);
            if let Some(stmt) = add_identity(t, col, identity_sequences.get(key.as_str()).copied())
            {
                pre.push(stmt);
            }
        }
    }

    // Functions after tables (a function may return a table's row type) and
    // column defaults after functions (a default may call one).
    for sc in &d.schemas {
        for f in &sc.functions {
            pre.push(create_function(f));
            // ownership/grants for functions use the FUNCTION form.
            let fq = format!("{}({})", quote_qualified(&f.schema, &f.name), f.signature);
            post.push(format!(
                "ALTER FUNCTION {fq} OWNER TO {};",
                quote_ident(&f.owner)
            ));
            post.extend(revoke_before_grant("FUNCTION", &fq, &f.owner, &f.acl));
            post.extend(grants_from_acl("FUNCTION", &fq, &f.acl));
        }
    }
    for t in &tables {
        for col in &t.columns {
            if let Some(stmt) = set_column_default(t, col) {
                pre.push(stmt);
            }
        }
    }

    // Views in dependency order: a view selecting from another view must be
    // created after it, which catalog name order does not guarantee.
    let views: Vec<&PgView> = topological_views(d);
    for v in &views {
        pre.push(create_view(v));
        let vq = quote_qualified(&v.schema, &v.name);
        post.push(alter_table_owner(&v.schema, &v.name, &v.owner));
        if let Some(c) = &v.comment {
            post.push(comment_on("VIEW", &vq, c));
        }
        post.extend(grants_from_acl("TABLE", &vq, &v.acl));
    }

    // Constraints: non-FK (PK/UNIQUE/CHECK/exclusion) before FK, so FK targets
    // already have the keys they reference.
    for t in &tables {
        for c in t.constraints.iter().filter(|c| c.kind != "f") {
            post.push(add_constraint(t, c));
        }
    }
    for t in &tables {
        for c in t.constraints.iter().filter(|c| c.kind == "f") {
            post.push(add_constraint(t, c));
        }
    }
    // Indexes that don't back a constraint, then the attachments that make a
    // partitioned parent's index valid.
    for t in &tables {
        for i in &t.indexes {
            if let Some(stmt) = create_index(i) {
                post.push(stmt);
            }
        }
    }
    for t in &tables {
        for i in &t.indexes {
            if let Some(stmt) = attach_index(t, i) {
                post.push(stmt);
            }
        }
    }

    // Materialized views are populated only now that the tables hold their
    // rows, in the same dependency order, and their indexes follow.
    for v in &views {
        if let Some(stmt) = refresh_view(v) {
            post.push(stmt);
        }
    }
    for v in &views {
        for i in &v.indexes {
            if let Some(stmt) = create_index(i) {
                post.push(stmt);
            }
        }
    }

    // The database's own ACL last: tightening `CONNECT` earlier could lock the
    // restore (and the read-back that follows it) out of the database it is
    // still working on.
    let dq = quote_ident(&d.name);
    post.extend(revoke_before_grant("DATABASE", &dq, &d.owner, &d.acl));
    post.extend(grants_from_acl("DATABASE", &dq, &d.acl));

    Ok(DatabaseDdl {
        name: d.name.clone(),
        pre_data: pre,
        post_data: post,
    })
}

/// Order tables so every parent precedes its children, for both partitioning
/// (`PARTITION OF`) and classic inheritance (`INHERITS`). Ties keep catalog
/// order (schema, name), so the emitted DDL stays stable.
fn topological_tables(d: &PgDatabase) -> Vec<&PgTable> {
    let tables: Vec<&PgTable> = d.schemas.iter().flat_map(|s| s.tables.iter()).collect();
    let parents_of = |t: &PgTable| -> Vec<String> {
        let mut parents: Vec<String> = t.inherits.clone();
        if let Some(p) = &t.partition_of {
            parents.push(p.parent.clone());
        }
        parents
    };
    let keys: Vec<String> = tables
        .iter()
        .map(|t| format!("{}.{}", t.schema, t.name))
        .collect();
    let depths = relax_depths(
        &keys,
        &tables.iter().map(|t| parents_of(t)).collect::<Vec<_>>(),
    );
    let mut ordered: Vec<(usize, usize)> = depths.into_iter().zip(0..).collect();
    ordered.sort_by_key(|(depth, index)| (*depth, *index));
    ordered
        .into_iter()
        .map(|(_, index)| tables[index])
        .collect()
}

/// Order views so every view precedes the ones that read it.
fn topological_views(d: &PgDatabase) -> Vec<&PgView> {
    let views: Vec<&PgView> = d.schemas.iter().flat_map(|s| s.views.iter()).collect();
    let keys: Vec<String> = views
        .iter()
        .map(|v| format!("{}.{}", v.schema, v.name))
        .collect();
    let parents: Vec<Vec<String>> = views.iter().map(|v| v.depends_on.clone()).collect();
    let depths = relax_depths(&keys, &parents);
    let mut ordered: Vec<(usize, usize)> = depths.into_iter().zip(0..).collect();
    ordered.sort_by_key(|(depth, index)| (*depth, *index));
    ordered.into_iter().map(|(_, index)| views[index]).collect()
}

/// Longest-path depth of every node, given each node's parents by key.
///
/// Iterative relaxation rather than recursion: the input is a plan received over
/// the wire, and recursing on it would let a deep (or, with a cycle, endless)
/// hierarchy abort the process instead of returning. The pass count bounds it;
/// a cycle — which the catalog cannot produce — simply stops improving.
fn relax_depths(keys: &[String], parents: &[Vec<String>]) -> Vec<usize> {
    let position: HashMap<&str, usize> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.as_str(), index))
        .collect();
    let mut depths = vec![0usize; keys.len()];
    for _ in 0..keys.len() {
        let mut changed = false;
        for (index, node_parents) in parents.iter().enumerate() {
            let want = node_parents
                .iter()
                .filter_map(|parent| position.get(parent.as_str()))
                .map(|&parent| depths[parent] + 1)
                .max()
                .unwrap_or(0);
            if want > depths[index] {
                depths[index] = want;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    depths
}

// --- helpers -----------------------------------------------------------------

/// GUC names whose value is a *list* and therefore must be appended verbatim
/// rather than as a string literal — pg_dump's `makeAlterConfigCommand` makes
/// exactly this exception, because the stored form is already a comma-separated
/// list of individually-quoted identifiers.
const LIST_VALUED_GUCS: &[&str] = &["search_path", "datestyle"];

/// Render one `name=value` GUC entry (from `pg_roles.rolconfig` /
/// `pg_db_role_setting.setconfig`) as a `SET` clause: `name TO value`.
///
/// Both halves come from a plan the destination received over the wire, and the
/// statement is executed through the simple query protocol, which runs every
/// `;`-separated statement in the string. Interpolating them raw was a SQL
/// injection: any unprivileged user on the source can put arbitrary text in
/// their own role's custom (placeholder) GUC, e.g.
/// `ALTER ROLE attacker SET "myapp.k" = '1; ALTER ROLE attacker SUPERUSER'`,
/// and the destination — restoring as an administrator — would have executed
/// the second statement. The same defect broke every ordinary value containing
/// a space (`statement_timeout=5min` → `SET statement_timeout TO 5min` → syntax
/// error, aborting the restore).
fn set_clause(kv: &str) -> Result<String> {
    let (name, value) = match kv.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (kv, None),
    };
    if !is_guc_name(name) {
        return Err(BackupError::phase(
            Phase::Apply,
            format!("refusing to apply a setting with a non-identifier name: {name:?}"),
        ));
    }
    let Some(value) = value else {
        return Ok(format!("{name} TO DEFAULT"));
    };
    if LIST_VALUED_GUCS
        .iter()
        .any(|guc| guc.eq_ignore_ascii_case(name))
    {
        // The catalog quotes list elements that need it, so a `;` cannot occur
        // in a well-formed value. Refuse rather than append one verbatim.
        if value.contains(';') {
            return Err(BackupError::phase(
                Phase::Apply,
                format!("refusing a list-valued setting {name} containing a semicolon"),
            ));
        }
        Ok(format!("{name} TO {value}"))
    } else {
        Ok(format!("{name} TO {}", quote_literal(value)))
    }
}

/// Whether `name` is a plain GUC name — `word` or `word.word`, ASCII letters,
/// digits and underscores, not starting with a digit.
fn is_guc_name(name: &str) -> bool {
    fn part(p: &str) -> bool {
        let mut chars = p.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        (first.is_ascii_alphabetic() || first == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
    let mut parts = name.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(one), None, _) => part(one),
        (Some(one), Some(two), None) => part(one) && part(two),
        _ => false,
    }
}

/// Re-quote a server-rendered dotted name (`schema.table[.column]`) — the catalog
/// gives these unquoted, so quote each part.
fn quote_qualified_dotted(dotted: &str) -> String {
    dotted
        .split('.')
        .map(quote_ident)
        .collect::<Vec<_>>()
        .join(".")
}

/// Split privilege letters into (plain, with-grant-option) keyword lists.
fn parse_priv_letters(privs: &str) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut grantable = Vec::new();
    let chars: Vec<char> = privs.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let kw = priv_keyword(chars[i]);
        let has_star = chars.get(i + 1) == Some(&'*');
        if let Some(kw) = kw {
            if has_star {
                grantable.push(kw.to_string());
            } else {
                plain.push(kw.to_string());
            }
        }
        i += if has_star { 2 } else { 1 };
    }
    (plain, grantable)
}

/// Map an `aclitem` privilege letter to its SQL keyword (common set).
fn priv_keyword(c: char) -> Option<&'static str> {
    Some(match c {
        'r' => "SELECT",
        'a' => "INSERT",
        'w' => "UPDATE",
        'd' => "DELETE",
        'D' => "TRUNCATE",
        'x' => "REFERENCES",
        't' => "TRIGGER",
        'X' => "EXECUTE",
        'U' => "USAGE",
        'C' => "CREATE",
        'c' => "CONNECT",
        'T' => "TEMPORARY",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents_and_literals_quote() {
        assert_eq!(quote_ident("foo"), "\"foo\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_qualified("app", "t"), "\"app\".\"t\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    /// `CREATE EXTENSION … WITH SCHEMA gis` needs `gis` to exist already. The
    /// emitted order used to be extensions first, which only ever worked because
    /// every fixture put its extensions in the pre-existing `public`.
    #[test]
    fn a_schema_is_created_before_an_extension_that_lives_in_it() {
        let db = crate::model::PgDatabase {
            name: "appdb".to_string(),
            owner: "app_owner".to_string(),
            schemas: vec![crate::model::PgSchema {
                name: "gis".to_string(),
                owner: "app_owner".to_string(),
                ..Default::default()
            }],
            extensions: vec![crate::model::PgExtension {
                name: "postgis".to_string(),
                version: "3.4.2".to_string(),
                schema: "gis".to_string(),
            }],
            ..Default::default()
        };
        let ddl = build_database_ddl(&db, 16).expect("database ddl");
        let schema_at = ddl
            .pre_data
            .iter()
            .position(|statement| statement.starts_with("CREATE SCHEMA \"gis\""))
            .expect("the schema must be created");
        let extension_at = ddl
            .pre_data
            .iter()
            .position(|statement| statement.contains("CREATE EXTENSION IF NOT EXISTS \"postgis\""))
            .expect("the extension must be created");
        assert!(
            schema_at < extension_at,
            "schema at {schema_at} must precede its extension at {extension_at}: {:#?}",
            ddl.pre_data
        );
    }

    #[test]
    fn role_ddl_golden() {
        let r = PgRole {
            name: "app_owner".into(),
            login: true,
            inherit: true,
            connlimit: -1,
            valid_until: Some("2030-01-01 00:00:00+00".into()),
            config: vec!["search_path=app, public".into()],
            ..Default::default()
        };
        assert_eq!(
            create_role(&r),
            "CREATE ROLE \"app_owner\" WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
             INHERIT NOREPLICATION NOBYPASSRLS VALID UNTIL '2030-01-01 00:00:00+00';"
        );
        assert_eq!(
            alter_role_settings(&r).expect("role settings"),
            vec!["ALTER ROLE \"app_owner\" SET search_path TO app, public;"]
        );
    }

    #[test]
    fn role_connection_limit_emitted_when_set() {
        let r = PgRole {
            name: "lim".into(),
            connlimit: 5,
            ..Default::default()
        };
        assert!(create_role(&r).contains("CONNECTION LIMIT 5"));
    }

    #[test]
    fn membership_and_database_golden() {
        let m = PgMembership {
            role: "readers".into(),
            member: "app_owner".into(),
            admin_option: false,
        };
        assert_eq!(grant_membership(&m), "GRANT \"readers\" TO \"app_owner\";");

        let d = PgDatabase {
            name: "appdb".into(),
            owner: "app_owner".into(),
            encoding: "UTF8".into(),
            collate: "en_US.utf8".into(),
            ctype: "en_US.utf8".into(),
            connlimit: -1,
            allow_connections: true,
            config: vec!["statement_timeout=0".into()],
            ..Default::default()
        };
        assert_eq!(
            create_database(&d, 16).expect("database DDL"),
            "CREATE DATABASE \"appdb\" WITH TEMPLATE = template0 OWNER = \"app_owner\" \
             ENCODING = 'UTF8' LOCALE_PROVIDER = libc LC_COLLATE = 'en_US.utf8' \
             LC_CTYPE = 'en_US.utf8';"
        );
        assert_eq!(
            alter_database_settings(&d).expect("database settings"),
            vec!["ALTER DATABASE \"appdb\" SET statement_timeout TO '0';"]
        );
    }

    #[test]
    fn database_locale_ddl_is_version_aware_from_10_through_18() {
        let base = PgDatabase {
            name: "appdb".into(),
            owner: "app_owner".into(),
            encoding: "UTF8".into(),
            collate: "C".into(),
            ctype: "en_US.utf8".into(),
            locale_provider: Some(PgLocaleProvider::Libc),
            connlimit: -1,
            allow_connections: true,
            ..Default::default()
        };

        for major in 10..=14 {
            assert_eq!(
                create_database(&base, major).expect("libc DDL"),
                "CREATE DATABASE \"appdb\" WITH TEMPLATE = template0 OWNER = \"app_owner\" \
                 ENCODING = 'UTF8' LC_COLLATE = 'C' LC_CTYPE = 'en_US.utf8';"
            );
        }
        for major in 15..=18 {
            assert_eq!(
                create_database(&base, major).expect("libc provider DDL"),
                "CREATE DATABASE \"appdb\" WITH TEMPLATE = template0 OWNER = \"app_owner\" \
                 ENCODING = 'UTF8' LOCALE_PROVIDER = libc LC_COLLATE = 'C' \
                 LC_CTYPE = 'en_US.utf8';"
            );
        }
    }

    #[test]
    fn database_icu_and_builtin_options_follow_server_capabilities() {
        let mut database = PgDatabase {
            name: "localized".into(),
            owner: "owner".into(),
            encoding: "UTF8".into(),
            collate: "C".into(),
            ctype: "C".into(),
            locale_provider: Some(PgLocaleProvider::Icu),
            provider_locale: Some("und-u-ks-level2".into()),
            connlimit: -1,
            allow_connections: true,
            ..Default::default()
        };

        assert!(create_database(&database, 14).is_err());
        let pg15 = create_database(&database, 15).expect("PostgreSQL 15 ICU");
        assert!(pg15.contains("LOCALE_PROVIDER = icu"));
        assert!(pg15.contains("ICU_LOCALE = 'und-u-ks-level2'"));
        assert!(!pg15.contains("ICU_RULES"));

        database.icu_rules = Some("&V < w <<< W".into());
        assert!(create_database(&database, 15).is_err());
        for major in 16..=18 {
            let ddl = create_database(&database, major).expect("ICU rules DDL");
            assert!(ddl.contains("ICU_RULES = '&V < w <<< W'"));
        }

        database.locale_provider = Some(PgLocaleProvider::Builtin);
        database.provider_locale = Some("C.UTF-8".into());
        database.icu_rules = None;
        assert!(create_database(&database, 16).is_err());
        for major in 17..=18 {
            let ddl = create_database(&database, major).expect("builtin DDL");
            assert!(ddl.contains("LOCALE_PROVIDER = builtin"));
            assert!(ddl.contains("BUILTIN_LOCALE = 'C.UTF-8'"));
        }
    }

    #[test]
    fn legacy_database_plan_is_treated_as_libc() {
        let database = PgDatabase {
            name: "legacy".into(),
            owner: "owner".into(),
            encoding: "UTF8".into(),
            collate: "C".into(),
            ctype: "C".into(),
            connlimit: -1,
            allow_connections: true,
            ..Default::default()
        };
        let pg14 = create_database(&database, 14).expect("legacy PostgreSQL 14 DDL");
        assert!(pg14.contains("TEMPLATE = template0"));
        assert!(pg14.contains("LC_COLLATE = 'C' LC_CTYPE = 'C'"));
        assert!(!pg14.contains(" LOCALE = "));
        assert!(create_database(&database, 18)
            .expect("legacy PostgreSQL 18 DDL")
            .contains("LOCALE_PROVIDER = libc"));
    }

    #[test]
    fn table_ddl_golden_columns_only() {
        let t = PgTable {
            schema: "app".into(),
            name: "accounts".into(),
            owner: "app_owner".into(),
            persistence: "p".into(),
            kind: "r".into(),
            columns: vec![
                PgColumn {
                    name: "id".into(),
                    ordinal: 1,
                    type_name: "bigint".into(),
                    not_null: true,
                    identity: Some("a".into()),
                    ..Default::default()
                },
                PgColumn {
                    name: "email".into(),
                    ordinal: 2,
                    type_name: "text".into(),
                    not_null: true,
                    collation: Some("C".into()),
                    ..Default::default()
                },
                PgColumn {
                    name: "status".into(),
                    ordinal: 3,
                    type_name: "text".into(),
                    not_null: true,
                    default: Some("'active'::text".into()),
                    ..Default::default()
                },
            ],
            reloptions: vec!["fillfactor=80".into()],
            ..Default::default()
        };
        let sql = create_table(&t);
        // Neither the default nor the identity clause is inline: both would
        // pin the table's creation to an order that user functions and
        // table-returning functions cannot both satisfy.
        assert_eq!(
            sql,
            "CREATE TABLE \"app\".\"accounts\" (\n  \
             \"id\" bigint NOT NULL,\n  \
             \"email\" text COLLATE \"C\" NOT NULL,\n  \
             \"status\" text NOT NULL\n) WITH (fillfactor=80);"
        );
        assert_eq!(
            set_column_default(&t, &t.columns[2]).expect("default statement"),
            "ALTER TABLE \"app\".\"accounts\" ALTER COLUMN \"status\" SET DEFAULT 'active'::text;"
        );
        assert_eq!(set_column_default(&t, &t.columns[1]), None);

        // The identity generator names its sequence explicitly and carries the
        // source parameters, so a renamed table's sequence keeps its name and
        // the post_data setval finds it.
        let seq = PgSequence {
            schema: "app".into(),
            name: "accounts_id_seq".into(),
            data_type: "bigint".into(),
            start: 1,
            increment: 2,
            min_value: 1,
            max_value: 1_000_000,
            cache: 20,
            cycle: true,
            owned_by: Some("app.accounts.id".into()),
            ..Default::default()
        };
        assert_eq!(
            add_identity(&t, &t.columns[0], Some(&seq)).expect("identity statement"),
            "ALTER TABLE \"app\".\"accounts\" ALTER COLUMN \"id\" ADD GENERATED ALWAYS AS \
             IDENTITY (SEQUENCE NAME \"app\".\"accounts_id_seq\" START WITH 1 INCREMENT BY 2 \
             MINVALUE 1 MAXVALUE 1000000 CACHE 20 CYCLE);"
        );
        assert_eq!(add_identity(&t, &t.columns[1], None), None);

        // A partition never carries the generator itself.
        let partition = PgTable {
            partition_of: Some(PgPartitionOf {
                parent: "app.parent".into(),
                bound: "DEFAULT".into(),
            }),
            ..t.clone()
        };
        assert_eq!(
            add_identity(&partition, &partition.columns[0], Some(&seq)),
            None
        );
    }

    /// GUC values reach the destination inside a peer-supplied plan and are
    /// executed through the simple query protocol, which runs every
    /// `;`-separated statement in the string.
    #[test]
    fn role_and_database_settings_cannot_smuggle_a_second_statement() {
        let attacker = PgRole {
            name: "attacker".into(),
            config: vec!["myapp.k=1; ALTER ROLE attacker SUPERUSER".into()],
            ..Default::default()
        };
        assert_eq!(
            alter_role_settings(&attacker).expect("quoted setting"),
            vec![
                "ALTER ROLE \"attacker\" SET myapp.k TO '1; ALTER ROLE attacker SUPERUSER';"
                    .to_string()
            ]
        );

        // An ordinary value containing a space used to be a syntax error that
        // aborted the whole restore.
        let app = PgRole {
            name: "app".into(),
            config: vec!["statement_timeout=5min".into()],
            ..Default::default()
        };
        assert_eq!(
            alter_role_settings(&app).expect("quoted setting"),
            vec!["ALTER ROLE \"app\" SET statement_timeout TO '5min';".to_string()]
        );

        // A non-identifier GUC name is refused outright.
        let bogus = PgRole {
            name: "bogus".into(),
            config: vec!["a b; DROP TABLE t=1".into()],
            ..Default::default()
        };
        let error = alter_role_settings(&bogus).expect_err("bad name must be refused");
        assert!(
            format!("{error}").contains("non-identifier name"),
            "{error}"
        );

        // A list-valued GUC is appended verbatim (pg_dump parity), so a
        // semicolon inside one is refused instead.
        let listy = PgDatabase {
            name: "appdb".into(),
            config: vec!["search_path=app; DROP TABLE t".into()],
            ..Default::default()
        };
        let error = alter_database_settings(&listy).expect_err("semicolon must be refused");
        assert!(format!("{error}").contains("semicolon"), "{error}");
    }

    #[test]
    fn inheritance_is_emitted_and_lists_only_local_columns() {
        let child = PgTable {
            schema: "app".into(),
            name: "log_2025".into(),
            kind: "r".into(),
            columns: vec![
                PgColumn {
                    name: "id".into(),
                    type_name: "integer".into(),
                    local: false,
                    ..Default::default()
                },
                PgColumn {
                    name: "note".into(),
                    type_name: "text".into(),
                    local: true,
                    ..Default::default()
                },
            ],
            inherits: vec!["app.log".into()],
            ..Default::default()
        };
        assert_eq!(
            create_table(&child),
            "CREATE TABLE \"app\".\"log_2025\" (\n  \"note\" text\n) \
             INHERITS (\"app\".\"log\");"
        );

        // A child with no columns of its own.
        let plain = PgTable {
            columns: vec![PgColumn {
                name: "id".into(),
                type_name: "integer".into(),
                local: false,
                ..Default::default()
            }],
            ..child.clone()
        };
        assert_eq!(
            create_table(&plain),
            "CREATE TABLE \"app\".\"log_2025\" () INHERITS (\"app\".\"log\");"
        );

        // Without inheritance, `local` is irrelevant: every column is emitted.
        let ordinary = PgTable {
            inherits: vec![],
            ..child
        };
        assert_eq!(
            create_table(&ordinary),
            "CREATE TABLE \"app\".\"log_2025\" (\n  \"id\" integer,\n  \"note\" text\n);"
        );
    }

    #[test]
    fn a_materialized_view_is_created_empty_and_refreshed_after_the_data() {
        let m = PgView {
            schema: "app".into(),
            name: "daily".into(),
            owner: "app_owner".into(),
            definition: " SELECT day, count(*) FROM app.events GROUP BY day;".into(),
            materialized: true,
            indexes: vec![],
            depends_on: vec![],
            acl: vec![],
            comment: None,
        };
        assert_eq!(
            create_view(&m),
            "CREATE MATERIALIZED VIEW \"app\".\"daily\" AS  SELECT day, count(*) \
             FROM app.events GROUP BY day WITH NO DATA;"
        );
        assert_eq!(
            refresh_view(&m).expect("refresh"),
            "REFRESH MATERIALIZED VIEW \"app\".\"daily\";"
        );
        let v = PgView {
            materialized: false,
            ..m
        };
        assert_eq!(
            create_view(&v),
            "CREATE VIEW \"app\".\"daily\" AS  SELECT day, count(*) FROM app.events \
             GROUP BY day;"
        );
        assert_eq!(refresh_view(&v), None);
    }

    #[test]
    fn a_partitioned_parents_index_is_attached_by_its_children() {
        let child = PgTable {
            schema: "app".into(),
            name: "events_2026".into(),
            ..Default::default()
        };
        let index = PgIndex {
            name: "events_2026_ts_idx".into(),
            definition: "CREATE INDEX events_2026_ts_idx ON app.events_2026 USING btree (ts)"
                .into(),
            is_primary: false,
            is_unique: false,
            is_constraint: false,
            attach_to: Some("app.events_ts_idx".into()),
            comment: None,
        };
        assert_eq!(
            attach_index(&child, &index).expect("attach"),
            "ALTER INDEX \"app\".\"events_ts_idx\" ATTACH PARTITION \
             \"app\".\"events_2026_ts_idx\";"
        );
        // A constraint's index is attached by the constraint itself.
        let backed = PgIndex {
            is_constraint: true,
            ..index.clone()
        };
        assert_eq!(attach_index(&child, &backed), None);
        // An ordinary index has nothing to attach to.
        let plain = PgIndex {
            attach_to: None,
            ..index
        };
        assert_eq!(attach_index(&child, &plain), None);
    }

    #[test]
    fn a_customised_acl_revokes_the_permissive_default_first() {
        // An empty ACL means "never customised": the destination default is
        // already correct and a REVOKE would diverge from the source.
        assert!(revoke_before_grant("FUNCTION", "\"app\".\"f\"()", "app_owner", &[]).is_empty());
        assert_eq!(
            revoke_before_grant(
                "FUNCTION",
                "\"app\".\"f\"()",
                "app_owner",
                &["app_owner=X/app_owner".to_string()]
            ),
            vec![
                "REVOKE ALL ON FUNCTION \"app\".\"f\"() FROM PUBLIC;".to_string(),
                "REVOKE ALL ON FUNCTION \"app\".\"f\"() FROM \"app_owner\";".to_string(),
            ]
        );
    }

    #[test]
    fn multi_level_hierarchies_are_created_parents_first() {
        let table = |name: &str, parent: Option<&str>, inherits: Vec<&str>| PgTable {
            schema: "app".into(),
            name: name.into(),
            kind: "r".into(),
            partition_of: parent.map(|p| PgPartitionOf {
                parent: p.into(),
                bound: "DEFAULT".into(),
            }),
            inherits: inherits.into_iter().map(String::from).collect(),
            ..Default::default()
        };
        // Alphabetically `apple` < `north` < `sales`, i.e. exactly reversed.
        let database = PgDatabase {
            name: "appdb".into(),
            schemas: vec![PgSchema {
                name: "app".into(),
                tables: vec![
                    table("apple", Some("app.north"), vec![]),
                    table("north", Some("app.sales"), vec![]),
                    table("sales", None, vec![]),
                    table("log_2025", None, vec!["app.log"]),
                    table("log", None, vec![]),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let order: Vec<&str> = topological_tables(&database)
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(
            order,
            vec!["sales", "log", "north", "log_2025", "apple"],
            "every parent must precede its children"
        );
    }

    #[test]
    fn views_are_created_in_dependency_order() {
        let view = |name: &str, depends_on: Vec<&str>| PgView {
            schema: "app".into(),
            name: name.into(),
            owner: "app_owner".into(),
            definition: "SELECT 1;".into(),
            materialized: false,
            indexes: vec![],
            depends_on: depends_on.into_iter().map(String::from).collect(),
            acl: vec![],
            comment: None,
        };
        let database = PgDatabase {
            name: "appdb".into(),
            schemas: vec![PgSchema {
                name: "app".into(),
                // `active` sorts first but reads `zombies`.
                views: vec![view("active", vec!["app.zombies"]), view("zombies", vec![])],
                ..Default::default()
            }],
            ..Default::default()
        };
        let order: Vec<&str> = topological_views(&database)
            .iter()
            .map(|v| v.name.as_str())
            .collect();
        assert_eq!(order, vec!["zombies", "active"]);
    }

    #[test]
    fn unlogged_and_generated_and_partitions() {
        let parent = PgTable {
            schema: "app".into(),
            name: "events".into(),
            kind: "p".into(),
            persistence: "u".into(),
            columns: vec![PgColumn {
                name: "ts".into(),
                type_name: "timestamptz".into(),
                not_null: true,
                ..Default::default()
            }],
            partition_key: Some("RANGE (ts)".into()),
            ..Default::default()
        };
        assert_eq!(
            create_table(&parent),
            "CREATE UNLOGGED TABLE \"app\".\"events\" (\n  \"ts\" timestamptz NOT NULL\n) \
             PARTITION BY RANGE (ts);"
        );

        let child = PgTable {
            schema: "app".into(),
            name: "events_2026".into(),
            kind: "r".into(),
            partition_of: Some(PgPartitionOf {
                parent: "app.events".into(),
                bound: "FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            create_table(&child),
            "CREATE TABLE \"app\".\"events_2026\" PARTITION OF \"app\".\"events\" \
             FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');"
        );

        let gen = PgColumn {
            name: "area".into(),
            type_name: "numeric".into(),
            generated: Some("w * h".into()),
            ..Default::default()
        };
        assert_eq!(
            column_clause(&gen),
            "\"area\" numeric GENERATED ALWAYS AS (w * h) STORED"
        );
    }

    #[test]
    fn constraint_index_sequence_golden() {
        let t = PgTable {
            schema: "app".into(),
            name: "accounts".into(),
            ..Default::default()
        };
        let pk = PgConstraint {
            name: "accounts_pkey".into(),
            kind: "p".into(),
            definition: "PRIMARY KEY (id)".into(),
            references: None,
        };
        assert_eq!(
            add_constraint(&t, &pk),
            "ALTER TABLE \"app\".\"accounts\" ADD CONSTRAINT \"accounts_pkey\" PRIMARY KEY (id);"
        );

        let idx = PgIndex {
            name: "accounts_email_idx".into(),
            definition:
                "CREATE UNIQUE INDEX accounts_email_idx ON app.accounts USING btree (email)".into(),
            is_primary: false,
            is_unique: true,
            is_constraint: false,
            attach_to: None,
            comment: None,
        };
        assert_eq!(
            create_index(&idx).unwrap(),
            "CREATE UNIQUE INDEX accounts_email_idx ON app.accounts USING btree (email);"
        );
        // Constraint-backing index is skipped.
        let backed = PgIndex {
            is_constraint: true,
            ..idx.clone()
        };
        assert_eq!(create_index(&backed), None);

        let seq = PgSequence {
            schema: "app".into(),
            name: "accounts_id_seq".into(),
            owner: "app_owner".into(),
            data_type: "bigint".into(),
            start: 1,
            increment: 1,
            min_value: 1,
            max_value: 9223372036854775807,
            cache: 1,
            cycle: false,
            last_value: Some(1001),
            is_called: true,
            owned_by: Some("app.accounts.id".into()),
            ..Default::default()
        };
        assert_eq!(
            create_sequence(&seq),
            "CREATE SEQUENCE \"app\".\"accounts_id_seq\" AS bigint INCREMENT BY 1 MINVALUE 1 \
             MAXVALUE 9223372036854775807 START WITH 1 CACHE 1;"
        );
        assert_eq!(
            sequence_owned_by(&seq).unwrap(),
            "ALTER SEQUENCE \"app\".\"accounts_id_seq\" OWNED BY \"app\".\"accounts\".\"id\";"
        );
        assert_eq!(
            sequence_setval(&seq).unwrap(),
            "SELECT pg_catalog.setval('\"app\".\"accounts_id_seq\"', 1001, true);"
        );
    }

    #[test]
    fn acl_grants_render() {
        // owner full privileges, no grant option.
        let g = grants_from_acl(
            "TABLE",
            "\"app\".\"t\"",
            &["app_owner=arwd/app_owner".into()],
        );
        assert_eq!(
            g,
            vec!["GRANT INSERT, SELECT, UPDATE, DELETE ON TABLE \"app\".\"t\" TO \"app_owner\";"]
        );

        // PUBLIC (empty grantee) + a grant-option privilege.
        let g2 = grants_from_acl("TABLE", "\"app\".\"t\"", &["=r/owner".into()]);
        assert_eq!(g2, vec!["GRANT SELECT ON TABLE \"app\".\"t\" TO PUBLIC;"]);
        let g3 = grants_from_acl("SCHEMA", "\"app\"", &["u=UC*/owner".into()]);
        assert_eq!(
            g3,
            vec![
                "GRANT USAGE ON SCHEMA \"app\" TO \"u\";",
                "GRANT CREATE ON SCHEMA \"app\" TO \"u\" WITH GRANT OPTION;",
            ]
        );
    }

    #[test]
    fn cluster_ddl_assembly_ordering() {
        let payload = crate::model::test_fixture();
        let ddl = build_cluster_ddl(&payload, 16).expect("cluster DDL");

        // Roles created, then memberships, then tablespaces.
        assert!(ddl.roles[0].starts_with("CREATE ROLE \"app_owner\""));
        assert!(ddl
            .roles
            .iter()
            .any(|s| s.starts_with("GRANT \"readers\" TO")));
        assert!(ddl
            .roles
            .iter()
            .any(|s| s.starts_with("CREATE TABLESPACE \"fast\"")));

        assert!(ddl.databases[0].starts_with("CREATE DATABASE \"appdb\""));

        let db = &ddl.per_database[0];
        assert_eq!(db.name, "appdb");
        // pre_data: extension + schema + sequence + table (columns only) present.
        assert!(db.pre_data.iter().any(|s| s.contains("CREATE EXTENSION")));
        assert!(db
            .pre_data
            .iter()
            .any(|s| s.starts_with("CREATE TABLE \"app\".\"accounts\"")));
        // CREATE TABLE must come before the PK constraint is added.
        let create_pos = db
            .pre_data
            .iter()
            .position(|s| s.contains("CREATE TABLE \"app\".\"accounts\""));
        assert!(create_pos.is_some());
        // Constraints/indexes live in post_data, not pre_data.
        assert!(db
            .post_data
            .iter()
            .any(|s| s.contains("ADD CONSTRAINT \"accounts_pkey\"")));
        assert!(db.pre_data.iter().all(|s| !s.contains("ADD CONSTRAINT")));
        // setval is post-data.
        assert!(db.post_data.iter().any(|s| s.contains("pg_catalog.setval")));

        // Function bodies are not name-resolved at creation.
        assert_eq!(db.pre_data[0], "SET check_function_bodies = false;");

        let position = |haystack: &[String], needle: &str| -> usize {
            haystack
                .iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("missing statement containing {needle:?}"))
        };
        // A default may call a user function, so defaults follow functions; the
        // identity generator precedes the data load that fills its column.
        let create_function = position(&db.pre_data, "CREATE OR REPLACE FUNCTION app.touch");
        let add_identity = position(&db.pre_data, "ADD GENERATED ALWAYS AS IDENTITY");
        let create_table = position(&db.pre_data, "CREATE TABLE \"app\".\"accounts\"");
        assert!(create_table < add_identity, "identity needs its table");
        assert!(
            add_identity < create_function,
            "identity is applied before the data load, functions come later"
        );
        assert!(db
            .pre_data
            .iter()
            .any(|s| s.contains("SEQUENCE NAME \"app\".\"accounts_id_seq\"")));
    }

    /// PostgreSQL creates and owns an IDENTITY column's sequence, but its
    /// privileges are still restorable state.
    #[test]
    fn an_identity_sequences_privileges_are_restored_but_not_its_creation() {
        let mut payload = crate::model::test_fixture();
        let sequence = &mut payload.databases[0].schemas[0].sequences[0];
        assert_eq!(sequence.owned_by.as_deref(), Some("app.accounts.id"));
        sequence.acl = vec![
            "app_owner=rwU/app_owner".to_string(),
            "readers=r/app_owner".to_string(),
        ];
        let ddl = build_cluster_ddl(&payload, 16).expect("cluster DDL");
        let db = &ddl.per_database[0];
        assert!(
            db.pre_data
                .iter()
                .all(|s| !s.starts_with("CREATE SEQUENCE \"app\".\"accounts_id_seq\"")),
            "the identity column creates its own sequence"
        );
        assert!(
            db.post_data
                .iter()
                .all(|s| !s.contains("OWNED BY \"app\".\"accounts\".\"id\"")),
            "the identity link is PostgreSQL's to manage"
        );
        let owner_grant = concat!(
            "GRANT SELECT, UPDATE, USAGE ON SEQUENCE ",
            "\"app\".\"accounts_id_seq\" TO \"app_owner\";"
        );
        assert!(db.post_data.iter().any(|s| s == owner_grant));
        assert!(db
            .post_data
            .iter()
            .any(|s| s.contains("TO \"readers\";") && s.contains("accounts_id_seq")));
    }

    #[test]
    fn a_materialized_view_is_refreshed_in_post_data_after_its_tables() {
        let mut payload = crate::model::test_fixture();
        payload.databases[0].schemas[0].views.push(PgView {
            schema: "app".into(),
            name: "daily".into(),
            owner: "app_owner".into(),
            definition: "SELECT count(*) FROM app.accounts;".into(),
            materialized: true,
            indexes: vec![PgIndex {
                name: "daily_idx".into(),
                definition: "CREATE UNIQUE INDEX daily_idx ON app.daily USING btree (count)".into(),
                is_primary: false,
                is_unique: true,
                is_constraint: false,
                attach_to: None,
                comment: None,
            }],
            depends_on: vec![],
            acl: vec![],
            comment: None,
        });
        let ddl = build_cluster_ddl(&payload, 16).expect("cluster DDL");
        let db = &ddl.per_database[0];
        assert!(
            db.pre_data.iter().any(
                |s| s.starts_with("CREATE MATERIALIZED VIEW \"app\".\"daily\"")
                    && s.ends_with("WITH NO DATA;")
            ),
            "a materialized view must be created empty: pre_data runs before the data load"
        );
        let refresh = db
            .post_data
            .iter()
            .position(|s| s == "REFRESH MATERIALIZED VIEW \"app\".\"daily\";")
            .expect("refresh in post_data");
        let index = db
            .post_data
            .iter()
            .position(|s| s.contains("daily_idx"))
            .expect("matview index in post_data");
        assert!(refresh < index, "populate the matview before indexing it");
    }

    #[test]
    fn public_schema_defaults_are_reset_before_source_acl_is_restored() {
        let mut payload = crate::model::test_fixture();
        payload.server_major = 14;
        let schema = &mut payload.databases[0].schemas[0];
        schema.name = "public".into();
        schema.owner = "app_owner".into();
        schema.acl = vec!["=U/app_owner".into()];

        let pg14 = build_cluster_ddl(&payload, 14).expect("PostgreSQL 14 DDL");
        let pre14 = &pg14.per_database[0].pre_data;
        assert!(pre14.contains(&"ALTER SCHEMA \"public\" OWNER TO \"app_owner\";".to_string()));
        assert!(pre14.contains(&"REVOKE ALL ON SCHEMA \"public\" FROM PUBLIC;".to_string()));
        assert!(pre14.iter().all(|sql| !sql.contains("pg_database_owner")));

        let pg18 = build_cluster_ddl(&payload, 18).expect("PostgreSQL 18 DDL");
        let db18 = &pg18.per_database[0];
        assert!(db18
            .pre_data
            .contains(&"REVOKE ALL ON SCHEMA \"public\" FROM \"pg_database_owner\";".to_string()));
        assert!(db18
            .post_data
            .contains(&"GRANT USAGE ON SCHEMA \"public\" TO PUBLIC;".to_string()));
    }
}
