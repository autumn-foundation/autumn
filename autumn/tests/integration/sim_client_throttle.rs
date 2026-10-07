//! Issue #3068: client-side adaptive throttling (Google SRE) in the HTTP
//! client, and criticality propagation on outbound calls.
//!
//! The upstream is a `SimNet` host. The test counts requests and accepts as
//! the throttle sees them, and checks that the client rejects locally only
//! after the accept ratio falls below `1/K`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use autumn_web::config::AutumnConfig;
use autumn_web::http_client::{Client, ClientError, Response};
use autumn_web::prelude::*;
use autumn_web::sim::{Sim, SimNet};
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use axum::http::{HeaderMap, StatusCode};

const K: f64 = 2.0;

/// Calls the upstream once, with no retries.
#[get("/call")]
async fn call(client: Client) -> String {
    match client.get("http://upstream/x").no_retry().send().await {
        Ok(response) => response.status().as_u16().to_string(),
        Err(ClientError::ThrottledLocally { host }) => format!("throttled:{host}"),
        Err(error) => format!("error: {error}"),
    }
}

/// Calls the upstream and returns the criticality header it saw.
#[get("/report", criticality = "sheddable")]
async fn report(client: Client) -> String {
    client
        .get("http://upstream/echo")
        .send()
        .await
        .map_or_else(|e| format!("error: {e}"), Response::text)
}

/// Same, with an explicit criticality on the builder.
#[get("/report-critical", criticality = "sheddable")]
async fn report_critical(client: Client) -> String {
    client
        .get("http://upstream/echo")
        .criticality(autumn_web::Criticality::Critical)
        .send()
        .await
        .map_or_else(|e| format!("error: {e}"), Response::text)
}

/// A default-criticality route that calls the upstream.
#[get("/report-default")]
async fn report_default(client: Client) -> String {
    client
        .get("http://upstream/echo")
        .send()
        .await
        .map_or_else(|e| format!("error: {e}"), Response::text)
}

/// An upstream that answers `200` while `healthy`, else `503`. It counts
/// the requests that reach it.
fn upstream(healthy: Arc<AtomicBool>, hits: Arc<AtomicUsize>) -> axum::Router {
    axum::Router::new()
        .route(
            "/x",
            axum::routing::get(move || {
                let healthy = healthy.clone();
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if healthy.load(Ordering::SeqCst) {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        )
        .route(
            "/echo",
            axum::routing::get(|headers: HeaderMap| async move {
                headers
                    .get("x-autumn-criticality")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_owned()
            }),
        )
}

fn config(throttle: bool) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.http.client.adaptive_throttle.enabled = throttle;
    config.http.client.adaptive_throttle.k = K;
    config
}

fn app(throttle: bool) -> TestApp {
    TestApp::new()
        .routes(routes![call, report, report_critical, report_default])
        .config(config(throttle))
}

#[sim_test]
async fn sim_client_throttle_rejects_locally_only_below_one_over_k(mut sim: Sim) {
    let healthy = Arc::new(AtomicBool::new(true));
    let hits = Arc::new(AtomicUsize::new(0));
    sim.net(SimNet::new().host("upstream", upstream(healthy.clone(), hits.clone())));
    sim.build(app(true));

    // The throttle's own view: every attempt is a request; a 200 is an accept.
    let (mut requests, mut accepts, mut throttled) = (0_u32, 0_u32, 0_u32);
    for i in 0..400 {
        if i == 50 {
            healthy.store(false, Ordering::SeqCst);
        }
        let ratio_ok = f64::from(accepts) * K >= f64::from(requests);
        let body = sim.client().get("/call").send().await.text();
        requests += 1;
        match body.as_str() {
            "200" => accepts += 1,
            "503" => {}
            "throttled:upstream" => {
                throttled += 1;
                assert!(
                    !ratio_ok,
                    "call {i} was throttled with {accepts} accepts of {requests} requests, \
                     at or above 1/K (seed={:#x})",
                    sim.seed
                );
            }
            other => panic!("unexpected body {other:?} (seed={:#x})", sim.seed),
        }
    }
    assert!(
        throttled > 100,
        "the client must reject most calls locally once the upstream is down: \
         {throttled} of 350 (seed={:#x})",
        sim.seed
    );
    let reached = hits.load(Ordering::SeqCst);
    assert_eq!(
        u32::try_from(reached).expect("small count") + throttled,
        400,
        "a throttled call never reaches the upstream"
    );
}

#[sim_test]
async fn sim_client_throttle_off_by_default_sends_every_call(mut sim: Sim) {
    let healthy = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));
    sim.net(SimNet::new().host("upstream", upstream(healthy, hits.clone())));
    sim.build(app(false));
    for _ in 0..200 {
        assert_eq!(sim.client().get("/call").send().await.text(), "503");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 200);
}

#[sim_test]
async fn sim_client_sends_the_inbound_criticality(mut sim: Sim) {
    let hits = Arc::new(AtomicUsize::new(0));
    sim.net(SimNet::new().host("upstream", upstream(Arc::new(AtomicBool::new(true)), hits)));
    sim.build(app(false));
    assert_eq!(sim.client().get("/report").send().await.text(), "sheddable");
    assert_eq!(
        sim.client().get("/report-critical").send().await.text(),
        "critical",
        "the builder value wins over the inbound one"
    );
    assert_eq!(
        sim.client().get("/report-default").send().await.text(),
        "none",
        "a default request sends no header"
    );
}
