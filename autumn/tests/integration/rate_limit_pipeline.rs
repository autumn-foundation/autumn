//! Integration tests for the built-in rate limiter (Story S-047).
//!
//! Verifies that configuring `[security.rate_limit]` in `AutumnConfig`
//! wires the [`RateLimitLayer`] into the router pipeline and produces
//! `429 Too Many Requests` with a `Retry-After` header when a client
//! exceeds their per-IP budget.

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

fn configured(rps: f64, burst: u32) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.security.rate_limit.enabled = true;
    config.security.rate_limit.requests_per_second = rps;
    config.security.rate_limit.burst = burst;
    // Tests exercise the XFF-key path without a real TCP listener.
    config.security.rate_limit.trust_forwarded_headers = true;
    config
}

#[tokio::test]
async fn exceeding_burst_yields_429_with_retry_after() {
    // Very slow refill + burst of 2 so the third request is reliably blocked.
    let client = TestApp::new()
        .routes(routes![ping])
        .config(configured(0.1, 2))
        .build();

    client
        .get("/ping")
        .header("X-Forwarded-For", "198.51.100.7")
        .send()
        .await
        .assert_status(200);
    client
        .get("/ping")
        .header("X-Forwarded-For", "198.51.100.7")
        .send()
        .await
        .assert_status(200);

    let throttled = client
        .get("/ping")
        .header("X-Forwarded-For", "198.51.100.7")
        .send()
        .await;
    throttled.assert_status(429);
    let retry_after = throttled
        .header("retry-after")
        .expect("Retry-After header must be present on 429");
    assert!(
        retry_after.parse::<u64>().is_ok(),
        "Retry-After should be an integer number of seconds, got {retry_after:?}",
    );
    throttled.assert_header("x-ratelimit-remaining", "0");
}

#[tokio::test]
async fn independent_ips_have_independent_buckets() {
    let client = TestApp::new()
        .routes(routes![ping])
        .config(configured(0.1, 1))
        .build();

    // Exhaust IP A.
    client
        .get("/ping")
        .header("X-Forwarded-For", "203.0.113.10")
        .send()
        .await
        .assert_status(200);
    client
        .get("/ping")
        .header("X-Forwarded-For", "203.0.113.10")
        .send()
        .await
        .assert_status(429);

    // IP B should still have a full bucket.
    client
        .get("/ping")
        .header("X-Forwarded-For", "203.0.113.11")
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn disabled_rate_limiter_is_passthrough() {
    let client = TestApp::new().routes(routes![ping]).build();

    for _ in 0..10 {
        client
            .get("/ping")
            .header("X-Forwarded-For", "192.0.2.1")
            .send()
            .await
            .assert_status(200);
    }
}
