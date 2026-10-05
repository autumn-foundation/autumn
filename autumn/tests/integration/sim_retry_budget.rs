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
/// The upper limit for the retry share after the bucket is empty.
const MAX_RETRY_SHARE: f64 = 0.20;

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
        share <= MAX_RETRY_SHARE,
        "retry share {share:.3} is above {MAX_RETRY_SHARE}"
    );
    assert!(retries > 0, "an empty bucket still refills: {retries}");
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
