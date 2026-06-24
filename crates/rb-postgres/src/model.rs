//! PostgreSQL cluster model — the module-specific `BackupPlan.payload` (plan
//! Phase 2.2). Produced by [`crate::introspect`] (read-only) and consumed by
//! [`crate::ddl`] (DDL reconstruction) and the streaming/restore paths.
//!
//! Design: lean on PostgreSQL's own DDL-emitting helpers at introspection time
//! (`pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_viewdef`,
//! `pg_get_functiondef`, `format_type`, `pg_get_expr`) so the stored definitions
//! are authoritative and version-stable, instead of hand-reconstructing them.
//! Category fields are kept as short strings (matching the catalog's single-char
//! codes where applicable) for serde stability and readable DDL.

use serde::{Deserialize, Serialize};

/// Top-level payload describing a PostgreSQL cluster slice for 1:1 restore.
///
/// Roles/memberships/tablespaces are cluster-global; `databases` carries one
/// fully-introspected tree per target database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PgPlanPayload {
    /// Raw `server_version` (e.g. `"16.3"`).
    pub server_version: String,
    /// Parsed major (e.g. `16`).
    pub server_major: u32,
    /// Cluster roles (login + group), ordered for safe creation.
    pub roles: Vec<PgRole>,
    /// Role memberships (`member` is a member of `role`).
    pub memberships: Vec<PgMembership>,
    /// Non-default tablespaces (excludes `pg_default`/`pg_global`).
    pub tablespaces: Vec<PgTablespace>,
    /// Target databases with their full schema trees.
    pub databases: Vec<PgDatabase>,
}

/// A cluster role. Passwords are NOT captured: reading `pg_authid.rolpassword`
/// needs superuser, which conflicts with the read-only-source invariant — so
/// restored roles are created without a password (documented limitation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PgRole {
    pub name: String,
    pub superuser: bool,
    pub createdb: bool,
    pub createrole: bool,
    pub inherit: bool,
    pub login: bool,
    pub replication: bool,
    pub bypassrls: bool,
    /// `-1` means no connection limit.
    pub connlimit: i32,
    /// `VALID UNTIL` (timestamptz rendered as text), if set.
    pub valid_until: Option<String>,
    /// Role-level config (`ALTER ROLE … SET k=v`), e.g. `"search_path=app"`.
    pub config: Vec<String>,
    pub comment: Option<String>,
}

/// `member` is a member of group role `role`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgMembership {
    pub role: String,
    pub member: String,
    pub admin_option: bool,
}

/// A non-default tablespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgTablespace {
    pub name: String,
    pub owner: String,
    pub location: String,
}

/// A database plus its full object tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PgDatabase {
    pub name: String,
    pub owner: String,
    pub encoding: String,
    pub collate: String,
    pub ctype: String,
    /// `None` when the database is on `pg_default`.
    pub tablespace: Option<String>,
    pub connlimit: i32,
    pub allow_connections: bool,
    pub is_template: bool,
    pub comment: Option<String>,
    /// Database-level config (`ALTER DATABASE … SET …`).
    pub config: Vec<String>,
    pub extensions: Vec<PgExtension>,
    pub schemas: Vec<PgSchema>,
}

/// An installed extension (restored via `CREATE EXTENSION`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgExtension {
    pub name: String,
    pub version: String,
    pub schema: String,
}

/// A schema (namespace) and its contained objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PgSchema {
    pub name: String,
    pub owner: String,
    pub comment: Option<String>,
    /// Raw `aclitem` strings (`grantee=privs/grantor`); rendered to GRANTs in DDL.
    pub acl: Vec<String>,
    pub tables: Vec<PgTable>,
    pub sequences: Vec<PgSequence>,
    pub views: Vec<PgView>,
    pub functions: Vec<PgFunction>,
}

/// A table (ordinary or partitioned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PgTable {
    pub schema: String,
    pub name: String,
    pub owner: String,
    /// `relpersistence`: `"p"` permanent, `"u"` unlogged, `"t"` temp.
    pub persistence: String,
    /// `relkind`: `"r"` ordinary, `"p"` partitioned.
    pub kind: String,
    pub columns: Vec<PgColumn>,
    pub constraints: Vec<PgConstraint>,
    pub indexes: Vec<PgIndex>,
    /// Raw `aclitem` strings; rendered to GRANTs in DDL.
    pub acl: Vec<String>,
    pub comment: Option<String>,
    /// `reloptions` (e.g. `fillfactor=70`).
    pub reloptions: Vec<String>,
    /// Non-default tablespace, if any.
    pub tablespace: Option<String>,
    /// `pg_get_partkeydef` for a partitioned parent.
    pub partition_key: Option<String>,
    /// Set when this table is itself a partition.
    pub partition_of: Option<PgPartitionOf>,
    /// `reltuples` estimate (may be `-1`/stale; refined for progress).
    pub estimated_rows: i64,
    /// `pg_total_relation_size` estimate in bytes.
    pub estimated_bytes: i64,
}

/// Partition attachment info (parent + bound expression).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgPartitionOf {
    /// Schema-qualified parent table.
    pub parent: String,
    /// `pg_get_expr(relpartbound)` — the `FOR VALUES …`/`DEFAULT` clause.
    pub bound: String,
}

/// A table column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PgColumn {
    pub name: String,
    pub ordinal: i32,
    /// `format_type(atttypid, atttypmod)` — fully-qualified type with modifiers.
    pub type_name: String,
    pub not_null: bool,
    /// `pg_get_expr(adbin)` default expression, if any.
    pub default: Option<String>,
    /// Identity: `"a"` (ALWAYS) / `"d"` (BY DEFAULT), else `None`.
    pub identity: Option<String>,
    /// Generation expression for a STORED generated column, if any.
    pub generated: Option<String>,
    /// Non-default collation name, if any.
    pub collation: Option<String>,
    pub comment: Option<String>,
}

/// A table constraint (`pg_get_constraintdef`-rendered).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgConstraint {
    pub name: String,
    /// `contype`: `"p"` PK, `"u"` unique, `"f"` FK, `"c"` check, `"x"` exclusion.
    pub kind: String,
    /// `pg_get_constraintdef(oid)` — the full constraint clause.
    pub definition: String,
    /// FK target table (`schema.name`) for restore ordering, when `kind == "f"`.
    pub references: Option<String>,
}

/// An index (`pg_get_indexdef`-rendered).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgIndex {
    pub name: String,
    /// `pg_get_indexdef(indexrelid)`.
    pub definition: String,
    pub is_primary: bool,
    pub is_unique: bool,
    /// Backs a constraint — created by the constraint, so DDL skips a separate
    /// `CREATE INDEX`.
    pub is_constraint: bool,
    pub comment: Option<String>,
}

/// A sequence. `last_value`/`is_called` are best-effort (NULL without privilege).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PgSequence {
    pub schema: String,
    pub name: String,
    pub owner: String,
    /// `int2`/`int4`/`int8`.
    pub data_type: String,
    pub start: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cache: i64,
    pub cycle: bool,
    /// Current value, if readable.
    pub last_value: Option<i64>,
    pub is_called: bool,
    /// `OWNED BY schema.table.column`, if owned.
    pub owned_by: Option<String>,
    pub acl: Vec<String>,
    pub comment: Option<String>,
}

/// A view or materialized view (`pg_get_viewdef`-rendered).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgView {
    pub schema: String,
    pub name: String,
    pub owner: String,
    /// `pg_get_viewdef(oid, true)` — the SELECT body.
    pub definition: String,
    pub materialized: bool,
    pub acl: Vec<String>,
    pub comment: Option<String>,
}

/// A function/procedure (`pg_get_functiondef`-rendered).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgFunction {
    pub schema: String,
    pub name: String,
    /// `pg_get_function_identity_arguments` — the argument signature.
    pub signature: String,
    /// `pg_get_functiondef(oid)` — the full `CREATE [OR REPLACE] FUNCTION …`.
    pub definition: String,
    pub owner: String,
    pub acl: Vec<String>,
    pub comment: Option<String>,
}

/// A fully-populated fixture exercising every field/variant. Shared by the model
/// serde tests and the `introspect`/`ddl` unit tests so they all build on the same
/// reference cluster.
#[cfg(test)]
pub(crate) fn test_fixture() -> PgPlanPayload {
    PgPlanPayload {
            server_version: "16.3".to_string(),
            server_major: 16,
            roles: vec![
                PgRole {
                    name: "app_owner".to_string(),
                    superuser: false,
                    createdb: false,
                    createrole: false,
                    inherit: true,
                    login: true,
                    replication: false,
                    bypassrls: false,
                    connlimit: -1,
                    valid_until: Some("2030-01-01 00:00:00+00".to_string()),
                    config: vec!["search_path=app, public".to_string()],
                    comment: Some("application owner".to_string()),
                },
                PgRole {
                    name: "readers".to_string(),
                    inherit: true,
                    connlimit: -1,
                    ..Default::default()
                },
            ],
            memberships: vec![PgMembership {
                role: "readers".to_string(),
                member: "app_owner".to_string(),
                admin_option: false,
            }],
            tablespaces: vec![PgTablespace {
                name: "fast".to_string(),
                owner: "app_owner".to_string(),
                location: "/mnt/fast".to_string(),
            }],
            databases: vec![PgDatabase {
                name: "appdb".to_string(),
                owner: "app_owner".to_string(),
                encoding: "UTF8".to_string(),
                collate: "en_US.utf8".to_string(),
                ctype: "en_US.utf8".to_string(),
                tablespace: None,
                connlimit: -1,
                allow_connections: true,
                is_template: false,
                comment: Some("the app db".to_string()),
                config: vec!["statement_timeout=0".to_string()],
                extensions: vec![PgExtension {
                    name: "pgcrypto".to_string(),
                    version: "1.3".to_string(),
                    schema: "public".to_string(),
                }],
                schemas: vec![PgSchema {
                    name: "app".to_string(),
                    owner: "app_owner".to_string(),
                    comment: None,
                    acl: vec!["app_owner=UC/app_owner".to_string()],
                    tables: vec![PgTable {
                        schema: "app".to_string(),
                        name: "accounts".to_string(),
                        owner: "app_owner".to_string(),
                        persistence: "p".to_string(),
                        kind: "r".to_string(),
                        columns: vec![
                            PgColumn {
                                name: "id".to_string(),
                                ordinal: 1,
                                type_name: "bigint".to_string(),
                                not_null: true,
                                default: None,
                                identity: Some("a".to_string()),
                                generated: None,
                                collation: None,
                                comment: Some("pk".to_string()),
                            },
                            PgColumn {
                                name: "email".to_string(),
                                ordinal: 2,
                                type_name: "text".to_string(),
                                not_null: true,
                                default: None,
                                identity: None,
                                generated: None,
                                collation: Some("C".to_string()),
                                comment: None,
                            },
                        ],
                        constraints: vec![PgConstraint {
                            name: "accounts_pkey".to_string(),
                            kind: "p".to_string(),
                            definition: "PRIMARY KEY (id)".to_string(),
                            references: None,
                        }],
                        indexes: vec![PgIndex {
                            name: "accounts_email_idx".to_string(),
                            definition:
                                "CREATE UNIQUE INDEX accounts_email_idx ON app.accounts USING btree (email)"
                                    .to_string(),
                            is_primary: false,
                            is_unique: true,
                            is_constraint: false,
                            comment: None,
                        }],
                        acl: vec!["app_owner=arwdDxt/app_owner".to_string()],
                        comment: Some("accounts".to_string()),
                        reloptions: vec!["fillfactor=80".to_string()],
                        tablespace: None,
                        partition_key: None,
                        partition_of: None,
                        estimated_rows: 1000,
                        estimated_bytes: 81920,
                    }],
                    sequences: vec![PgSequence {
                        schema: "app".to_string(),
                        name: "accounts_id_seq".to_string(),
                        owner: "app_owner".to_string(),
                        data_type: "int8".to_string(),
                        start: 1,
                        increment: 1,
                        min_value: 1,
                        max_value: i64::MAX,
                        cache: 1,
                        cycle: false,
                        last_value: Some(1001),
                        is_called: true,
                        owned_by: Some("app.accounts.id".to_string()),
                        acl: vec![],
                        comment: None,
                    }],
                    views: vec![PgView {
                        schema: "app".to_string(),
                        name: "active_accounts".to_string(),
                        owner: "app_owner".to_string(),
                        definition: "SELECT id, email FROM app.accounts;".to_string(),
                        materialized: false,
                        acl: vec![],
                        comment: None,
                    }],
                    functions: vec![PgFunction {
                        schema: "app".to_string(),
                        name: "touch".to_string(),
                        signature: "".to_string(),
                        definition:
                            "CREATE OR REPLACE FUNCTION app.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;"
                                .to_string(),
                        owner: "app_owner".to_string(),
                        acl: vec![],
                        comment: None,
                    }],
                }],
            }],
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_serde_roundtrip() {
        let payload = super::test_fixture();
        let json = serde_json::to_value(&payload).expect("serialize");
        let back: PgPlanPayload = serde_json::from_value(json).expect("deserialize");
        assert_eq!(payload, back, "payload must survive a serde roundtrip");
    }

    #[test]
    fn payload_roundtrips_through_string() {
        let payload = super::test_fixture();
        let s = serde_json::to_string(&payload).expect("to_string");
        let back: PgPlanPayload = serde_json::from_str(&s).expect("from_str");
        assert_eq!(payload, back);
    }
}
