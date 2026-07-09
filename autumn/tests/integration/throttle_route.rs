//! Integration tests for the per-route `#[throttle]` attribute (issue #1350).
//!
//! Verifies that annotating a handler with `#[throttle(limit = N, per = "…")]`
//! or `#[throttle("name")]` binds a per-handler rate limiter that composes
//! with (and, for that route, is stricter than) the global limiter without
//! affecting sibling routes.

use autumn_web::config::AutumnConfig;
use autumn_web::security::{
    KeyStrategy, RateLimitExempt, RateLimitNamedConfig, RateLimitPrincipal,
};
use autumn_web::test::TestApp;
use autumn_web::{get, routes, throttle};

// ── Handlers ───────────────────────────────────────────────────────────────

#[get("/throttled")]
#[throttle(limit = 2, per = "1s", key = "ip")]
async fn throttled() -> &'static str {
    "throttled-ok"
}

#[get("/plain")]
async fn plain() -> &'static str {
    "plain-ok"
}

#[get("/quick")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn quick() -> &'static str {
    "quick-ok"
}

#[get("/window")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn window() -> &'static str {
    "window-ok"
}

#[get("/principal")]
#[throttle(limit = 1, per = "1s", key = "principal")]
async fn principal_throttled() -> &'static str {
    "principal-ok"
}

#[get("/login")]
#[throttle("login")]
async fn named_login() -> &'static str {
    "named-ok"
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn base_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    // Enable the global limiter but leave it generous so per-route throttles
    // are what shows up in the test outcomes.
    config.security.rate_limit.enabled = true;
    config.security.rate_limit.requests_per_second = 1000.0;
    config.security.rate_limit.burst = 1000;
    config.security.rate_limit.trust_forwarded_headers = true;
    config
}

/// Config with the global limiter disabled so a `#[throttle]` denial's own
/// `x-ratelimit-*` headers are asserted precisely. (When the global limiter is
/// enabled and generous, it sees the request as allowed and stamps its own
/// `x-ratelimit-limit`/`remaining`/`reset` onto the outgoing response — the
/// throttle's `Retry-After` still survives, but its `remaining: 0` is
/// overwritten by the global bucket's remaining count.) The per-route throttle
/// applies regardless of the global limiter's `enabled` flag.
fn throttle_only_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.security.rate_limit.enabled = false;
    config.security.rate_limit.trust_forwarded_headers = true;
    config
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn throttled_route_429s_after_burst_while_sibling_route_unaffected() {
    let client = TestApp::new()
        .routes(routes![throttled, plain])
        .config(base_config())
        .build();

    // Burst of 2 — the first two throttled requests succeed, the third fails.
    client
        .get("/throttled")
        .header("X-Forwarded-For", "198.51.100.1")
        .send()
        .await
        .assert_status(200);
    client
        .get("/throttled")
        .header("X-Forwarded-For", "198.51.100.1")
        .send()
        .await
        .assert_status(200);

    let throttled = client
        .get("/throttled")
        .header("X-Forwarded-For", "198.51.100.1")
        .send()
        .await;
    throttled.assert_status(429);

    // The sibling /plain route sharing the same client IP is unaffected.
    for _ in 0..5 {
        client
            .get("/plain")
            .header("X-Forwarded-For", "198.51.100.1")
            .send()
            .await
            .assert_status(200);
    }
}

#[tokio::test]
async fn throttled_429_carries_retry_after_and_ratelimit_headers() {
    let client = TestApp::new()
        .routes(routes![quick])
        .config(throttle_only_config())
        .build();

    client
        .get("/quick")
        .header("X-Forwarded-For", "198.51.100.7")
        .send()
        .await
        .assert_status(200);

    let denied = client
        .get("/quick")
        .header("X-Forwarded-For", "198.51.100.7")
        .send()
        .await;
    denied.assert_status(429);

    let retry_after = denied
        .header("retry-after")
        .expect("Retry-After header must be present on 429");
    assert!(
        retry_after.parse::<u64>().is_ok(),
        "Retry-After should be an integer number of seconds, got {retry_after:?}",
    );
    denied.assert_header("x-ratelimit-remaining", "0");
    assert!(
        denied.header("x-ratelimit-limit").is_some(),
        "x-ratelimit-limit header must be present"
    );
    assert!(
        denied.header("x-ratelimit-reset").is_some(),
        "x-ratelimit-reset header must be present"
    );
    denied.assert_header_contains("content-type", "application/problem+json");
}

#[tokio::test]
async fn throttled_window_resets_after_sleep() {
    let client = TestApp::new()
        .routes(routes![window])
        .config(base_config())
        .build();

    client
        .get("/window")
        .header("X-Forwarded-For", "198.51.100.9")
        .send()
        .await
        .assert_status(200);
    client
        .get("/window")
        .header("X-Forwarded-For", "198.51.100.9")
        .send()
        .await
        .assert_status(429);

    // Wait for the 1s window to refill enough for one more request.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    client
        .get("/window")
        .header("X-Forwarded-For", "198.51.100.9")
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn named_limiter_reads_from_config() {
    let mut config = base_config();
    config.security.rate_limit.named.insert(
        "login".to_owned(),
        RateLimitNamedConfig {
            limit: 1,
            per: "1s".to_owned(),
            key: Some(KeyStrategy::Ip),
        },
    );

    let client = TestApp::new()
        .routes(routes![named_login])
        .config(config)
        .build();

    client
        .get("/login")
        .header("X-Forwarded-For", "203.0.113.42")
        .send()
        .await
        .assert_status(200);
    client
        .get("/login")
        .header("X-Forwarded-For", "203.0.113.42")
        .send()
        .await
        .assert_status(429);
}

#[tokio::test]
async fn independent_ips_have_independent_throttle_buckets() {
    let client = TestApp::new()
        .routes(routes![quick])
        .config(base_config())
        .build();

    client
        .get("/quick")
        .header("X-Forwarded-For", "192.0.2.10")
        .send()
        .await
        .assert_status(200);
    client
        .get("/quick")
        .header("X-Forwarded-For", "192.0.2.10")
        .send()
        .await
        .assert_status(429);
    // Different IP: fresh bucket.
    client
        .get("/quick")
        .header("X-Forwarded-For", "192.0.2.11")
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn rate_limit_exempt_bypasses_per_route_throttle() {
    // The tower path can't set request extensions from the outside, so we
    // build a router directly and inject `RateLimitExempt` on every request.
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    // Reset the process-wide throttle registry to avoid cross-test bleed.
    autumn_web::security::__throttle_registry_reset();

    // Use the raw axum router built by the framework (via TestApp::router) so
    // the throttle attribute wiring is exercised end-to-end.
    let mut config = base_config();
    config.security.rate_limit.enabled = true;
    let app: Router = TestApp::new()
        .routes(routes![quick])
        .config(config)
        .build()
        .into_router();

    let peer: SocketAddr = "127.0.0.1:65000".parse().expect("addr");

    // First request without exempt marker: succeeds (limit = 1).
    let req = {
        let mut r = Request::builder()
            .method("GET")
            .uri("/quick")
            .body(Body::empty())
            .expect("request builds");
        r.extensions_mut().insert(ConnectInfo(peer));
        r
    };
    let resp1 = app.clone().oneshot(req).await.expect("oneshot");
    assert_eq!(resp1.status(), StatusCode::OK);

    // Second request WITHOUT exempt marker: 429.
    let req = {
        let mut r = Request::builder()
            .method("GET")
            .uri("/quick")
            .body(Body::empty())
            .expect("request builds");
        r.extensions_mut().insert(ConnectInfo(peer));
        r
    };
    let resp2 = app.clone().oneshot(req).await.expect("oneshot");
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);

    // Third request WITH exempt marker: bypasses per-route throttle → 200.
    let req = {
        let mut r = Request::builder()
            .method("GET")
            .uri("/quick")
            .body(Body::empty())
            .expect("request builds");
        r.extensions_mut().insert(ConnectInfo(peer));
        r.extensions_mut().insert(RateLimitExempt);
        r
    };
    let resp3 = app.clone().oneshot(req).await.expect("oneshot");
    assert_eq!(resp3.status(), StatusCode::OK);
}

#[tokio::test]
async fn principal_key_isolates_by_principal_extension() {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    autumn_web::security::__throttle_registry_reset();

    let mut config = base_config();
    // Use principal key strategy globally so populate_rate_limit_principal
    // doesn't get in the way; the per-route spec explicitly picks principal.
    config.security.rate_limit.key_strategy = KeyStrategy::AuthenticatedPrincipal;
    let app: Router = TestApp::new()
        .routes(routes![principal_throttled])
        .config(config)
        .build()
        .into_router();

    let peer: SocketAddr = "127.0.0.1:65001".parse().expect("addr");

    let build_req = |principal: &str| {
        let mut r = Request::builder()
            .method("GET")
            .uri("/principal")
            .body(Body::empty())
            .expect("request builds");
        r.extensions_mut().insert(ConnectInfo(peer));
        r.extensions_mut()
            .insert(RateLimitPrincipal(principal.to_owned()));
        r
    };

    let r1 = app.clone().oneshot(build_req("alice")).await.expect("ok");
    assert_eq!(r1.status(), StatusCode::OK);
    let r2 = app.clone().oneshot(build_req("alice")).await.expect("ok");
    assert_eq!(r2.status(), StatusCode::TOO_MANY_REQUESTS);
    // Different principal on the same peer: fresh bucket.
    let r3 = app.clone().oneshot(build_req("bob")).await.expect("ok");
    assert_eq!(r3.status(), StatusCode::OK);
}
