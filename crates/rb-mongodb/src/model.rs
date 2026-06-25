//! MongoDB cluster plan model (plan Phase 3.2).
//!
//! [`MongoPlanPayload`] is the module-specific, self-contained descriptor carried
//! in [`rb_core::plan::BackupPlan::payload`]. It captures everything the
//! destination needs to validate and restore the cluster 1:1 — databases,
//! collections (+ options + index specs), document/size estimates — without any
//! round-trip back to the source.
//!
//! Index specs are stored as the JSON form of the driver's `IndexModel` so the
//! destination can rebuild them verbatim. User definitions are captured for plan
//! visibility and the immutability fingerprint, but — like PostgreSQL passwords —
//! credentials are NOT captured and users are NOT recreated on restore (a
//! documented limitation; see `dest.rs`).

use serde::{Deserialize, Serialize};

/// Top-level MongoDB backup descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MongoPlanPayload {
    /// MongoDB server `version` string (e.g. `"6.0.8"`).
    pub server_version: String,
    /// Parsed major version (e.g. `6`). Used by destination preflight.
    pub server_major: u32,
    /// All backed-up databases (system databases excluded).
    pub databases: Vec<MongoDatabase>,
}

/// A MongoDB database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MongoDatabase {
    /// Database name.
    pub name: String,
    /// Collections in this database (views excluded).
    pub collections: Vec<MongoCollection>,
    /// Users defined in this database (captured, not restored).
    #[serde(default)]
    pub users: Vec<MongoUser>,
}

/// A MongoDB collection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MongoCollection {
    /// Owning database name (denormalized so a plan item is self-describing).
    pub database: String,
    /// Collection name.
    pub name: String,
    /// `create`-command options (capped/size/validator/…) as JSON, if any.
    #[serde(default)]
    pub options: Option<serde_json::Value>,
    /// Index specifications, each the JSON form of an `IndexModel`. The implicit
    /// `_id_` index is excluded (it is created automatically).
    #[serde(default)]
    pub indexes: Vec<serde_json::Value>,
    /// Estimated document count (volatile; normalized out of the fingerprint).
    #[serde(default)]
    pub estimated_docs: u64,
    /// Estimated size in bytes (volatile; normalized out of the fingerprint).
    #[serde(default)]
    pub estimated_bytes: u64,
}

/// A MongoDB user definition (captured for visibility/fingerprint only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MongoUser {
    /// Auth database the user lives in.
    pub db: String,
    /// User name.
    pub name: String,
    /// Role assignments (raw role documents/strings as reported by `usersInfo`).
    #[serde(default)]
    pub roles: Vec<serde_json::Value>,
}

#[cfg(test)]
/// A deterministic fixture used by `build_plan`, `assess`, and fingerprint tests.
/// One database `appdb` with a single collection `accounts` carrying one index
/// and non-zero (volatile) estimates.
pub fn test_fixture() -> MongoPlanPayload {
    MongoPlanPayload {
        server_version: "6.0.8".to_string(),
        server_major: 6,
        databases: vec![MongoDatabase {
            name: "appdb".to_string(),
            collections: vec![MongoCollection {
                database: "appdb".to_string(),
                name: "accounts".to_string(),
                options: None,
                indexes: vec![serde_json::json!({
                    "key": { "email": 1 },
                    "name": "email_1",
                    "unique": true
                })],
                estimated_docs: 500,
                estimated_bytes: 65536,
            }],
            users: vec![],
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_serde_roundtrip() {
        let p = test_fixture();
        let json = serde_json::to_value(&p).expect("serialize");
        let back: MongoPlanPayload = serde_json::from_value(json).expect("deserialize");
        assert_eq!(p, back);
    }

    #[test]
    fn payload_roundtrips_through_plan_value() {
        // The payload survives the Value round-trip it takes through BackupPlan.
        let p = test_fixture();
        let v: serde_json::Value = serde_json::to_value(&p).unwrap();
        let back: MongoPlanPayload = serde_json::from_value(v).unwrap();
        assert_eq!(back.databases[0].collections[0].name, "accounts");
        assert_eq!(back.databases[0].collections[0].indexes.len(), 1);
    }
}
