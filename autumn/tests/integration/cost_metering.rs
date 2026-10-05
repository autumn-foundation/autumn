//! Per-request cost metering (issue #1720).
//!
//! The `CostLayer` records CPU time, allocated bytes and DB queries for each
//! request, adds them to the tenant total, and exposes them on
//! `Server-Timing`, the metrics registry and `GET /actuator/cost`.

use autumn_web::config::AutumnConfig;
use autumn_web::cost::{CostAccountant, CostSignal, UNATTRIBUTED_TENANT};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

/// Spends a known amount of CPU so the meter has something to read.
#[get("/busy")]
async fn busy() -> &'static str {
    let mut acc = 0_u64;
    for i in 0..200_000_u64 {
        acc = std::hint::black_box(acc.wrapping_mul(31).wrapping_add(i));
    }
    std::hint::black_box(acc);
    "ok"
}

fn metered_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.cost.enabled = true;
    config.observability.server_timing = Some(true);
    config.actuator.sensitive = true;
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config
}

fn accountant(client: &autumn_web::test::TestClient) -> CostAccountant {
    client
        .state()
        .extension::<CostAccountant>()
        .map(|a| (*a).clone())
        .expect("cost.enabled installs a CostAccountant")
}

#[tokio::test]
async fn server_timing_carries_cost_metrics() {
    let client = TestApp::new()
        .config(metered_config())
        .routes(routes![busy])
        .build();

    let resp = client
        .get("/busy")
        .header("x-tenant-id", "acme")
        .send()
        .await;
    resp.assert_ok();

    let header = resp
        .header("server-timing")
        .expect("server timing is on")
        .to_owned();
    assert!(header.contains("cost-cpu;dur="), "{header}");
    assert!(header.contains("cost-db;desc=\"0 queries\""), "{header}");
    // No allocation probe: no alloc metric.
    assert!(!header.contains("cost-alloc"), "{header}");
}

#[tokio::test]
async fn costs_are_attributed_to_the_resolved_tenant() {
    let client = TestApp::new()
        .config(metered_config())
        .routes(routes![busy])
        .build();

    for tenant in ["acme", "acme", "globex"] {
        client
            .get("/busy")
            .header("x-tenant-id", tenant)
            .send()
            .await
            .assert_ok();
    }

    let accountant = accountant(&client);
    let acme = accountant.tenant("acme").expect("acme is recorded");
    let globex = accountant.tenant("globex").expect("globex is recorded");
    assert_eq!(acme.requests, 2);
    assert_eq!(globex.requests, 1);
    assert!(acme.cpu_micros > 0, "the busy loop uses CPU: {acme:?}");
    assert_eq!(accountant.snapshot().total.requests, 3);
}

#[tokio::test]
async fn request_without_tenancy_is_unattributed() {
    let mut config = metered_config();
    config.tenancy.enabled = false;
    let client = TestApp::new().config(config).routes(routes![busy]).build();

    client.get("/busy").send().await.assert_ok();

    let unattributed = accountant(&client)
        .tenant(UNATTRIBUTED_TENANT)
        .expect("request without tenant is recorded");
    assert_eq!(unattributed.requests, 1);
}

#[tokio::test]
async fn actuator_cost_endpoint_reports_tenant_rollup() {
    let client = TestApp::new()
        .config(metered_config())
        .routes(routes![busy])
        .build();
    client
        .get("/busy")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .assert_ok();

    let resp = client.get("/actuator/cost").send().await;
    resp.assert_ok();
    let body: serde_json::Value = resp.json();
    assert_eq!(body["enabled"], true, "{body}");
    assert_eq!(body["tenants"]["acme"]["requests"], 1, "{body}");
    assert_eq!(body["total"]["requests"], 1, "{body}");
    assert_eq!(body["signal"]["high"], false, "{body}");
}

#[tokio::test]
async fn prometheus_exposes_per_tenant_cost_counters() {
    let client = TestApp::new()
        .config(metered_config())
        .routes(routes![busy])
        .build();
    client
        .get("/busy")
        .header("x-tenant-id", "acme")
        .send()
        .await
        .assert_ok();

    let scrape = client.get("/actuator/prometheus").send().await.text();
    assert!(
        scrape.contains("autumn_cost_requests_total{tenant=\"acme\"} 1"),
        "{scrape}"
    );
    assert!(
        scrape.contains("autumn_cost_cpu_seconds_total{tenant=\"acme\"}"),
        "{scrape}"
    );
}

#[tokio::test]
async fn cost_header_follows_the_server_timing_setting() {
    let mut config = metered_config();
    config.observability.server_timing = Some(false);
    let client = TestApp::new().config(config).routes(routes![busy]).build();

    let resp = client
        .get("/busy")
        .header("x-tenant-id", "acme")
        .send()
        .await;
    resp.assert_ok();

    assert!(
        resp.header("server-timing").is_none(),
        "no cost data leaks when server timing is off"
    );
    assert_eq!(
        accountant(&client).tenant("acme").map(|t| t.requests),
        Some(1),
        "metering still records"
    );
}

#[tokio::test]
async fn metering_is_off_by_default() {
    let mut config = AutumnConfig::default();
    config.observability.server_timing = Some(true);
    config.actuator.sensitive = true;
    let client = TestApp::new().config(config).routes(routes![busy]).build();

    let resp = client.get("/busy").send().await;
    resp.assert_ok();
    let header = resp.header("server-timing").unwrap_or_default().to_owned();
    assert!(!header.contains("cost-cpu"), "{header}");
    assert!(client.state().extension::<CostAccountant>().is_none());
    // The signal is always present, so deferral works without metering.
    assert!(client.state().extension::<CostSignal>().is_some());

    let body: serde_json::Value = client.get("/actuator/cost").send().await.json();
    assert_eq!(body["enabled"], false, "{body}");
}

/// AC: the signal is live. An operator changes the runtime-config key and the
/// running app follows it, with no redeploy.
#[tokio::test]
async fn signal_follows_runtime_config_without_redeploy() {
    use autumn_web::runtime_config::{
        COST_SIGNAL_KEY, ConfigRegistry, InMemoryConfigStore, RuntimeConfigService,
    };
    use std::sync::Arc;

    let mut registry = ConfigRegistry::new();
    registry.define_cost_signal().expect("define the cost key");
    let service = Arc::new(RuntimeConfigService::new(
        Arc::new(registry),
        Arc::new(InMemoryConfigStore::new()),
    ));

    let mut config = AutumnConfig::default();
    config.cost.defer_threshold = Some(400.0);
    config.cost.signal_refresh_secs = 1;
    let installed = Arc::clone(&service);
    let client = TestApp::new()
        .config(config)
        .state_initializer(move |state| state.insert_extension(installed))
        .build();
    let signal = client
        .state()
        .extension::<CostSignal>()
        .expect("signal installed");
    assert!(!signal.is_high());

    service
        .set(COST_SIGNAL_KEY, "650", Some("ops"))
        .expect("set the signal");
    // Real time: the refresher reads the store on a blocking thread every
    // second. Give a loaded CI runner a wide margin; the loop ends early.
    for _ in 0..300 {
        if signal.is_high() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        (signal.value() - 650.0).abs() < f64::EPSILON,
        "the signal follows runtime config: {:?}",
        signal.snapshot()
    );
    assert!(signal.is_high());
}

#[cfg(all(feature = "db", feature = "test-support"))]
mod db_queries {
    use super::*;
    use autumn_web::test::TestDb;
    use diesel_async::RunQueryDsl;

    #[get("/two-queries")]
    async fn two_queries(mut db: Db) -> AutumnResult<&'static str> {
        diesel::sql_query("SELECT 1").execute(&mut *db).await?;
        diesel::sql_query("SELECT 2").execute(&mut *db).await?;
        Ok("ok")
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn db_queries_are_counted_without_server_timing() {
        let db = TestDb::shared().await;
        let mut config = metered_config();
        config.observability.server_timing = Some(false);
        let client = TestApp::new()
            .config(config)
            .routes(routes![two_queries])
            .with_db(db.pool())
            .build();

        client
            .get("/two-queries")
            .header("x-tenant-id", "acme")
            .send()
            .await
            .assert_ok();

        let acme = accountant(&client).tenant("acme").expect("acme recorded");
        assert_eq!(acme.db_queries, 2, "{acme:?}");
    }
}
