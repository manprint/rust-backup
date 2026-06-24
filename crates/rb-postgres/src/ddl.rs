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

use crate::model::*;

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

/// `ALTER ROLE … SET k TO v;` for each role GUC (best-effort: splits on first `=`).
pub fn alter_role_settings(r: &PgRole) -> Vec<String> {
    r.config
        .iter()
        .map(|kv| {
            let (k, v) = split_setting(kv);
            format!("ALTER ROLE {} SET {} TO {};", quote_ident(&r.name), k, v)
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

/// `CREATE DATABASE … WITH …;`.
pub fn create_database(d: &PgDatabase) -> String {
    let mut s = format!(
        "CREATE DATABASE {} WITH OWNER = {} ENCODING = {} LC_COLLATE = {} LC_CTYPE = {}",
        quote_ident(&d.name),
        quote_ident(&d.owner),
        quote_literal(&d.encoding),
        quote_literal(&d.collate),
        quote_literal(&d.ctype),
    );
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
    s
}

/// `ALTER DATABASE … SET k TO v;` for each database GUC (best-effort).
pub fn alter_database_settings(d: &PgDatabase) -> Vec<String> {
    d.config
        .iter()
        .map(|kv| {
            let (k, v) = split_setting(kv);
            format!(
                "ALTER DATABASE {} SET {} TO {};",
                quote_ident(&d.name),
                k,
                v
            )
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
        let cols: Vec<String> = t.columns.iter().map(column_clause).collect();
        format!(
            "CREATE {unlogged}TABLE {qual} (\n  {}\n)",
            cols.join(",\n  ")
        )
    };

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
fn column_clause(c: &PgColumn) -> String {
    let mut s = format!("{} {}", quote_ident(&c.name), c.type_name);
    if let Some(coll) = &c.collation {
        s.push_str(&format!(" COLLATE {}", quote_ident(coll)));
    }
    if let Some(expr) = &c.generated {
        s.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"));
    } else if let Some(kind) = &c.identity {
        let when = if kind == "a" { "ALWAYS" } else { "BY DEFAULT" };
        s.push_str(&format!(" GENERATED {when} AS IDENTITY"));
    } else if let Some(def) = &c.default {
        s.push_str(&format!(" DEFAULT {def}"));
    }
    if c.not_null {
        s.push_str(" NOT NULL");
    }
    s
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
pub fn create_view(v: &PgView) -> String {
    let kind = if v.materialized {
        "MATERIALIZED VIEW"
    } else {
        "VIEW"
    };
    format!(
        "CREATE {kind} {} AS {}",
        quote_qualified(&v.schema, &v.name),
        v.definition.trim_end()
    )
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

/// Assemble the full, ordered cluster DDL from the payload.
pub fn build_cluster_ddl(payload: &PgPlanPayload) -> ClusterDdl {
    let mut roles = Vec::new();
    for r in &payload.roles {
        roles.push(create_role(r));
    }
    for r in &payload.roles {
        roles.extend(alter_role_settings(r));
    }
    for m in &payload.memberships {
        roles.push(grant_membership(m));
    }
    for t in &payload.tablespaces {
        roles.push(create_tablespace(t));
    }

    let mut databases = Vec::new();
    for d in &payload.databases {
        databases.push(create_database(d));
        databases.extend(alter_database_settings(d));
    }

    let per_database = payload.databases.iter().map(build_database_ddl).collect();

    ClusterDdl {
        roles,
        databases,
        per_database,
    }
}

fn build_database_ddl(d: &PgDatabase) -> DatabaseDdl {
    let mut pre = Vec::new();
    let mut post = Vec::new();

    for e in &d.extensions {
        pre.push(create_extension(e));
    }
    for sc in &d.schemas {
        pre.push(create_schema(sc));
        if let Some(c) = &sc.comment {
            pre.push(comment_on("SCHEMA", &quote_ident(&sc.name), c));
        }
        post.extend(grants_from_acl("SCHEMA", &quote_ident(&sc.name), &sc.acl));
    }

    // Sequences (definitions before tables, since column defaults may reference
    // them); values/ownership after data.
    for sc in &d.schemas {
        for seq in &sc.sequences {
            pre.push(create_sequence(seq));
            let qual = quote_qualified(&seq.schema, &seq.name);
            post.push(alter_table_owner(&seq.schema, &seq.name, &seq.owner));
            if let Some(o) = sequence_owned_by(seq) {
                post.push(o);
            }
            if let Some(v) = sequence_setval(seq) {
                post.push(v);
            }
            if let Some(c) = &seq.comment {
                post.push(comment_on("SEQUENCE", &qual, c));
            }
            post.extend(grants_from_acl("SEQUENCE", &qual, &seq.acl));
        }
    }

    // Tables: partition parents before children so PARTITION OF resolves.
    let mut tables: Vec<&PgTable> = d.schemas.iter().flat_map(|s| s.tables.iter()).collect();
    tables.sort_by_key(|t| t.partition_of.is_some());
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
    // Indexes that don't back a constraint.
    for t in &tables {
        for i in &t.indexes {
            if let Some(stmt) = create_index(i) {
                post.push(stmt);
            }
        }
    }

    // Views + functions after tables (they may reference them).
    for sc in &d.schemas {
        for f in &sc.functions {
            pre.push(create_function(f));
            // ownership/grants for functions use the FUNCTION form.
            let fq = format!("{}({})", quote_qualified(&f.schema, &f.name), f.signature);
            post.push(format!(
                "ALTER FUNCTION {fq} OWNER TO {};",
                quote_ident(&f.owner)
            ));
            post.extend(grants_from_acl("FUNCTION", &fq, &f.acl));
        }
    }
    for sc in &d.schemas {
        for v in &sc.views {
            pre.push(create_view(v));
            let vq = quote_qualified(&v.schema, &v.name);
            post.push(alter_table_owner(&v.schema, &v.name, &v.owner));
            if let Some(c) = &v.comment {
                post.push(comment_on("VIEW", &vq, c));
            }
            post.extend(grants_from_acl("TABLE", &vq, &v.acl));
        }
    }

    DatabaseDdl {
        name: d.name.clone(),
        pre_data: pre,
        post_data: post,
    }
}

// --- helpers -----------------------------------------------------------------

/// Split a `name=value` GUC entry into `(name, value)`, best-effort. The value is
/// returned as-is (already a valid SET value for most settings).
fn split_setting(kv: &str) -> (String, String) {
    match kv.split_once('=') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (kv.to_string(), "DEFAULT".to_string()),
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
            alter_role_settings(&r),
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
            create_database(&d),
            "CREATE DATABASE \"appdb\" WITH OWNER = \"app_owner\" ENCODING = 'UTF8' \
             LC_COLLATE = 'en_US.utf8' LC_CTYPE = 'en_US.utf8';"
        );
        assert_eq!(
            alter_database_settings(&d),
            vec!["ALTER DATABASE \"appdb\" SET statement_timeout TO 0;"]
        );
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
        assert_eq!(
            sql,
            "CREATE TABLE \"app\".\"accounts\" (\n  \
             \"id\" bigint GENERATED ALWAYS AS IDENTITY NOT NULL,\n  \
             \"email\" text COLLATE \"C\" NOT NULL,\n  \
             \"status\" text DEFAULT 'active'::text NOT NULL\n) WITH (fillfactor=80);"
        );
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
        let ddl = build_cluster_ddl(&payload);

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
    }
}
