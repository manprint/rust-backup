//! Live introspection smoke test (plan Phase 2.2).
//!
//! Runs only when `RUST_BACKUP_PG_HOST` is set; otherwise skips cleanly (no
//! Docker/live pg in CI). Exercises the real `Source::analyze` path
//! (introspect_cluster + build_plan). Deep fidelity assertions live in the e2e
//! script `e2e/postgres_introspect.sh`.

use rb_core::module::TargetParams;
use rb_postgres::PgPlanPayload;

fn target_params_from_env() -> Option<TargetParams> {
    let host = std::env::var("RUST_BACKUP_PG_HOST").ok()?;
    let mut obj = serde_json::Map::new();
    obj.insert("host".into(), host.into());
    obj.insert(
        "port".into(),
        std::env::var("RUST_BACKUP_PG_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(5432)
            .into(),
    );
    obj.insert(
        "user".into(),
        std::env::var("RUST_BACKUP_PG_USER")
            .unwrap_or_else(|_| "postgres".into())
            .into(),
    );
    if let Ok(pw) = std::env::var("RUST_BACKUP_PG_PASSWORD") {
        obj.insert("password".into(), pw.into());
    }
    if let Ok(db) = std::env::var("RUST_BACKUP_PG_DATABASE") {
        obj.insert("database".into(), db.into());
    }
    obj.insert("sslmode".into(), "prefer".into());
    Some(TargetParams::from_value(serde_json::Value::Object(obj)))
}

#[tokio::test]
async fn analyze_produces_a_plan() {
    let Some(params) = target_params_from_env() else {
        eprintln!("SKIP analyze_produces_a_plan: set RUST_BACKUP_PG_HOST to run");
        return;
    };
    let module = rb_postgres::module();
    let source = module.open_source(&params).await.expect("open_source");
    let plan = source.analyze().await.expect("analyze");

    eprintln!("{}", plan.render());
    assert_eq!(plan.module, "postgres");
    assert!(!plan.databases_is_empty(), "plan must list ≥1 database");
}

// Small helper extension so the test reads clearly without importing the payload
// internals — decode the payload and check it carries at least one database.
trait PlanProbe {
    fn databases_is_empty(&self) -> bool;
}
impl PlanProbe for rb_core::plan::BackupPlan {
    fn databases_is_empty(&self) -> bool {
        serde_json::from_value::<PgPlanPayload>(self.payload.clone())
            .map(|p| p.databases.is_empty())
            .unwrap_or(true)
    }
}
