//! Issue #3068: under overload, `sheddable` requests are rejected before
//! `critical` ones.
//!
//! The test fills the limit one slot at a time with held requests. At each
//! step it sends one probe of each class. It records the first step at which
//! each class is rejected. A lower class must be rejected first (or at the
//! same step), never later.

use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use axum::http::StatusCode;

/// The static limit for every test in this file.
const LIMIT: usize = 4;

/// Holds a slot for 60 virtual seconds. `critical`, so that holders can
/// fill the whole limit whatever the shares.
#[get("/hold", criticality = "critical")]
async fn hold() -> &'static str {
    tokio::time::sleep(Duration::from_secs(60)).await;
    "held"
}

#[get("/batch", criticality = "sheddable")]
async fn batch() -> &'static str {
    "batch"
}

#[get("/page")]
async fn page() -> &'static str {
    "page"
}

/// Returns the criticality that the handler's task sees.
#[get("/pay", criticality = "critical")]
async fn pay() -> String {
    format!("{:?}", autumn_web::admission::current_criticality())
}

fn config(default_share: f64) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.server.max_concurrent_requests = Some(LIMIT);
    config.server.admission.partitions.default = default_share;
    config
}

/// For each class (sheddable, default, critical), the number of held
/// requests at which a probe of that class is first rejected.
async fn first_rejection(sim: &Sim) -> [usize; 3] {
    let client = sim.client();
    let mut first = [usize::MAX; 3];
    for held in 0..=LIMIT {
        let holders = futures::future::join_all((0..held).map(|_| client.get("/hold").send()));
        let probes = async {
            // Let every holder reach its handler first.
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut statuses = [StatusCode::OK; 3];
            for (i, path) in ["/batch", "/page", "/pay"].into_iter().enumerate() {
                statuses[i] = client.get(path).send().await.status;
            }
            statuses
        };
        let (held_responses, statuses) = futures::future::join(holders, probes).await;
        for r in held_responses {
            assert_eq!(
                r.status,
                StatusCode::OK,
                "a holder within the limit is admitted"
            );
        }
        for (i, status) in statuses.into_iter().enumerate() {
            if status == StatusCode::SERVICE_UNAVAILABLE && first[i] == usize::MAX {
                first[i] = held;
            } else if status != StatusCode::SERVICE_UNAVAILABLE {
                assert_eq!(status, StatusCode::OK);
            }
        }
    }
    first
}

#[sim_test]
async fn sim_admission_sheddable_is_rejected_before_critical(mut sim: Sim) {
    sim.build(
        TestApp::new()
            .routes(routes![hold, batch, page, pay])
            .config(config(1.0)),
    );
    let [sheddable, default, critical] = first_rejection(&sim).await;
    // Default shares: sheddable fills half the limit; default and critical
    // fill all of it.
    assert_eq!(sheddable, LIMIT / 2, "seed={:#x}", sim.seed);
    assert_eq!(default, LIMIT, "seed={:#x}", sim.seed);
    assert_eq!(critical, LIMIT, "seed={:#x}", sim.seed);
}

#[sim_test]
async fn sim_admission_reserved_headroom_keeps_critical_last(mut sim: Sim) {
    sim.build(
        TestApp::new()
            .routes(routes![hold, batch, page, pay])
            .config(config(0.75)),
    );
    let [sheddable, default, critical] = first_rejection(&sim).await;
    assert!(
        sheddable < default && default < critical,
        "classes must be rejected in order: sheddable {sheddable}, default {default}, \
         critical {critical} (seed={:#x})",
        sim.seed
    );
    assert_eq!((sheddable, default, critical), (2, 3, 4));
}

#[sim_test]
async fn sim_admission_handler_sees_its_route_criticality(mut sim: Sim) {
    sim.build(
        TestApp::new()
            .routes(routes![hold, batch, page, pay])
            .config(config(1.0)),
    );
    assert_eq!(
        sim.client().get("/pay").send().await.text(),
        "Some(Critical)"
    );
}

#[sim_test]
async fn sim_admission_trusted_header_overrides_the_route(mut sim: Sim) {
    let mut cfg = config(1.0);
    cfg.server.admission.trust_criticality_header = true;
    sim.build(
        TestApp::new()
            .routes(routes![hold, batch, page, pay])
            .config(cfg),
    );
    let text = sim
        .client()
        .get("/pay")
        .header("X-Autumn-Criticality", "sheddable")
        .send()
        .await
        .text();
    assert_eq!(text, "Some(Sheddable)");
}
