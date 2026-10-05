//! Retry budget for outbound HTTP (issue #3058).
//!
//! The upstream fails every attempt. After the budget bucket is empty, the
//! client must retry only a small share of requests (about 10 %, at most 20 %).
//! It must never block a first attempt.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_web::http_client::Client;
use autumn_web::prelude::*;
use autumn_web::sim::{Sim, SimNet};
use autumn_web::sim_test;
use autumn_web::test::TestApp;

/// Requests the test sends.
const REQUESTS: u64 = 1_000;
/// Requests to skip before the measurement. The default bucket (500 tokens,
/// 14 per transient retry) is empty well before this.
const WARM_UP: u64 = 100;
/// The retry share after the bucket is empty: about 10 %, at most 20 %.
const RETRY_SHARE: std::ops::RangeInclusive<f64> = 0.08..=0.12;

/// An upstream that counts attempts and fails each one with a `503`.
fn always_failing(attempts: Arc<AtomicU64>) -> axum::Router {
    axum::Router::new().route(
        "/work",
        axum::routing::get(move || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { axum::http::StatusCode::SERVICE_UNAVAILABLE }
        }),
    )
}

#[get("/call")]
async fn call(client: Client) -> String {
    match client.get("http://upstream/work").send().await {
        Ok(response) => response.status().as_u16().to_string(),
        Err(error) => format!("error: {error}"),
    }
}

/// Send `count` requests and return the upstream attempts they made.
async fn send(sim: &Sim, attempts: &AtomicU64, count: u64) -> u64 {
    let before = attempts.load(Ordering::SeqCst);
    for _ in 0..count {
        let body = sim.client().get("/call").send().await.text();
        assert_eq!(body, "503", "the last 503 comes back to the caller");
    }
    attempts.load(Ordering::SeqCst) - before
}

#[sim_test]
async fn sim_retry_budget_caps_retries_when_the_upstream_always_fails(mut sim: Sim) {
    let attempts = Arc::new(AtomicU64::new(0));
    sim.net(SimNet::new().host("upstream", always_failing(attempts.clone())));
    sim.build(TestApp::new().routes(routes![call]));

    let warm_up = send(&sim, &attempts, WARM_UP).await;
    assert!(
        warm_up > WARM_UP,
        "the full bucket allows retries: {warm_up}"
    );

    let measured = REQUESTS - WARM_UP;
    let made = send(&sim, &attempts, measured).await;
    assert!(made >= measured, "a first attempt was blocked: {made}");
    let retries = made - measured;
    #[allow(clippy::cast_precision_loss)]
    let share = retries as f64 / measured as f64;
    assert!(
        RETRY_SHARE.contains(&share),
        "retry share {share:.3} is not in {RETRY_SHARE:?}"
    );
}

#[sim_test]
async fn sim_retry_budget_is_per_app(mut sim: Sim) {
    // A new app starts with a full bucket, so one app cannot drain another.
    let attempts = Arc::new(AtomicU64::new(0));
    sim.net(SimNet::new().host("upstream", always_failing(attempts.clone())));
    sim.build(TestApp::new().routes(routes![call]));
    send(&sim, &attempts, WARM_UP).await;

    sim.build(TestApp::new().routes(routes![call]));
    let made = send(&sim, &attempts, 1).await;
    assert_eq!(made, 4, "a full bucket allows all 3 retries");
}

/// An upstream that fails each odd attempt and succeeds each even one.
fn flapping(attempts: Arc<AtomicU64>) -> axum::Router {
    axum::Router::new().route(
        "/work",
        axum::routing::get(move || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt % 2 == 0 {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                } else {
                    axum::http::StatusCode::OK
                }
            }
        }),
    )
}

#[sim_test]
async fn sim_retry_budget_gives_back_the_tokens_of_a_retry_that_succeeds(mut sim: Sim) {
    // Each request fails once, then its retry succeeds. With no refund the
    // bucket (500 / 14) would be empty after about 40 requests.
    let attempts = Arc::new(AtomicU64::new(0));
    sim.net(SimNet::new().host("upstream", flapping(attempts.clone())));
    sim.build(TestApp::new().routes(routes![call]));
    for index in 0..200 {
        let body = sim.client().get("/call").send().await.text();
        assert_eq!(body, "200", "request {index}");
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 400);
}

#[sim_test]
async fn sim_retry_budget_can_be_turned_off(mut sim: Sim) {
    let mut config = autumn_web::config::AutumnConfig::default();
    config.http.client.retry_budget.enabled = false;
    let attempts = Arc::new(AtomicU64::new(0));
    sim.net(SimNet::new().host("upstream", always_failing(attempts.clone())));
    sim.build(TestApp::new().routes(routes![call]).config(config));
    let made = send(&sim, &attempts, WARM_UP).await;
    assert_eq!(made, WARM_UP * 4, "every request makes all 4 attempts");
}

#[sim_test]
async fn sim_retry_budget_limits_retries_after_network_errors(mut sim: Sim) {
    let net = SimNet::new().host("upstream", always_failing(Arc::default()));
    net.partition("upstream");
    sim.net(net.clone());
    sim.build(TestApp::new().routes(routes![call]));
    for _ in 0..REQUESTS {
        let body = sim.client().get("/call").send().await.text();
        assert!(body.starts_with("error:"), "{body}");
    }
    let attempts = net.events().len() as u64;
    let retries = attempts - REQUESTS;
    // A full bucket gives about 35 retries, then about 10 % of requests.
    assert!(retries < 35 + REQUESTS / 5, "too many retries: {retries}");
}
