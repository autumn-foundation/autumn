//! Integration tests for the per-route `#[throttle]` attribute (issue #1350).
//!
//! Verifies that annotating a handler with `#[throttle(limit = N, per = "…")]`
//! or `#[throttle("name")]` binds a per-handler rate limiter that composes
//! with (and, for that route, is stricter than) the global limiter without
//! affecting sibling routes.

use autumn_web::config::AutumnConfig;
use autumn_web::security::{
    KeyStrategy, RateLimitEnvelopeCounted, RateLimitExempt, RateLimitNamedConfig,
    RateLimitPrincipal,
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

// Each test gets its own uniquely-named handler so it maps to a distinct
// `route_id` bucket in the process-global throttle registry. This keeps the
// tests isolated under parallel `--test-threads` without any registry reset.
#[get("/retry-headers")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn retry_headers() -> &'static str {
    "retry-headers-ok"
}

#[get("/independent-ips")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn independent_ips() -> &'static str {
    "independent-ips-ok"
}

#[get("/exempt")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn exempt_route() -> &'static str {
    "exempt-ok"
}

#[get("/envelope-counted")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn envelope_counted_route() -> &'static str {
    "envelope-counted-ok"
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

#[get("/principal-session")]
#[throttle(limit = 1, per = "1s", key = "principal")]
async fn principal_session_throttled() -> &'static str {
    "principal-session-ok"
}

// Login handlers that establish an authenticated session (auth key `user_id`)
// the same way `#[secured]` reads it, so a later `#[throttle(key = "principal")]`
// request under that session can derive its principal without the global
// principal middleware being installed.
#[get("/throttle-login-alice")]
async fn throttle_login_alice(session: autumn_web::session::Session) -> &'static str {
    session.insert("user_id", "alice").await;
    "logged-in"
}

#[get("/throttle-login-bob")]
async fn throttle_login_bob(session: autumn_web::session::Session) -> &'static str {
    session.insert("user_id", "bob").await;
    "logged-in"
}

// A plain user handler (no `#[throttle]`) that returns its OWN 429 carrying no
// rate-limit headers — models a lockout/backpressure response from application
// code. It consumed a global token, so the global limiter's informational
// `x-ratelimit-*` headers must still be attached.
#[get("/user-429")]
async fn user_429() -> axum::http::StatusCode {
    axum::http::StatusCode::TOO_MANY_REQUESTS
}

#[get("/login")]
#[throttle("login")]
async fn named_login() -> &'static str {
    "named-ok"
}

#[get("/trusted-proxy-ip")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn trusted_proxy_ip() -> &'static str {
    "trusted-proxy-ip-ok"
}

#[get("/global-plus-throttle")]
#[throttle(limit = 1, per = "1s", key = "ip")]
async fn global_plus_throttle() -> &'static str {
    "global-plus-throttle-ok"
}

#[cfg(all(feature = "redis", feature = "test-support"))]
#[get("/redis-route-a")]
#[throttle(limit = 1, per = "60s", key = "ip")]
async fn redis_route_a() -> &'static str {
    "redis-route-a-ok"
}

#[cfg(all(feature = "redis", feature = "test-support"))]
#[get("/redis-route-b")]
#[throttle(limit = 1, per = "60s", key = "ip")]
async fn redis_route_b() -> &'static str {
    "redis-route-b-ok"
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

/// Config with the global limiter disabled, isolating a `#[throttle]` denial's
/// own `x-ratelimit-*` headers for a precise assertion. The per-route throttle
/// applies regardless of the global limiter's `enabled` flag; the companion
/// `throttle_429_headers_survive_enabled_global_limiter` test covers the case
/// where the global limiter is *also* enabled and must not overwrite the
/// throttle's `remaining: 0` / route limit.
fn throttle_only_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.security.rate_limit.enabled = false;
    config.security.rate_limit.trust_forwarded_headers = true;
    config
}

/// Config that uses the recommended top-level `[security.trusted_proxies]`
/// resolver and leaves the deprecated `security.rate_limit.*` proxy fields
/// unset — the layout that exposed bug #1350 (the throttle path keying on the
/// proxy peer instead of the real forwarded client).
fn top_level_trusted_proxy_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.security.rate_limit.enabled = true;
    // Generous global limiter so only the per-route throttle drives 429s.
    config.security.rate_limit.requests_per_second = 1000.0;
    config.security.rate_limit.burst = 1000;
    // Legacy rate-limit proxy fields intentionally left at their defaults
    // (trust_forwarded_headers = false) so the throttle path must consult the
    // shared top-level resolver.
    config.security.trusted_proxies.trust_forwarded_headers = true;
    config.security.trusted_proxies.trusted_hops = Some(1);
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
        .routes(routes![retry_headers])
        .config(throttle_only_config())
        .build();

    client
        .get("/retry-headers")
        .header("X-Forwarded-For", "198.51.100.7")
        .send()
        .await
        .assert_status(200);

    let denied = client
        .get("/retry-headers")
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

/// Regression (Codex P2, PR #1662): when the GLOBAL limiter is enabled and a
/// per-route `#[throttle]` denies a request, the throttle's own 429 (built
/// inside the handler) bubbles UP through the global `RateLimitService`'s
/// *allowed* path — the global limiter let the request through, so from its
/// perspective the inner service simply returned a response. That path used to
/// unconditionally overwrite `x-ratelimit-*` with the GLOBAL bucket's values,
/// so a throttled 429 advertised the global (non-zero) remaining quota and the
/// global limit instead of the route's `remaining: 0` / route limit.
///
/// This drives the route past its per-route limit while the global bucket still
/// has ample quota, and asserts the 429 carries the ROUTE's headers.
#[tokio::test]
async fn throttle_429_headers_survive_enabled_global_limiter() {
    // Generous global limiter (rps/burst = 1000) + a strict per-route throttle
    // (limit = 1). The global bucket is nowhere near exhausted, so any 429 here
    // is purely the per-route throttle's doing.
    let client = TestApp::new()
        .routes(routes![global_plus_throttle])
        .config(base_config())
        .build();

    client
        .get("/global-plus-throttle")
        .header("X-Forwarded-For", "198.51.100.42")
        .send()
        .await
        .assert_status(200);

    let denied = client
        .get("/global-plus-throttle")
        .header("X-Forwarded-For", "198.51.100.42")
        .send()
        .await;
    denied.assert_status(429);

    // The 429 must advertise the ROUTE's exhausted bucket, not the global one.
    // Route limit is 1 (global burst is 1000); route remaining is 0.
    denied.assert_header("x-ratelimit-remaining", "0");
    denied.assert_header("x-ratelimit-limit", "1");

    // Retry-After from the per-route throttle must survive too.
    let retry_after = denied
        .header("retry-after")
        .expect("Retry-After header must be present on the throttled 429");
    assert!(
        retry_after.parse::<u64>().is_ok(),
        "Retry-After should be an integer number of seconds, got {retry_after:?}",
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
        .routes(routes![independent_ips])
        .config(base_config())
        .build();

    client
        .get("/independent-ips")
        .header("X-Forwarded-For", "192.0.2.10")
        .send()
        .await
        .assert_status(200);
    client
        .get("/independent-ips")
        .header("X-Forwarded-For", "192.0.2.10")
        .send()
        .await
        .assert_status(429);
    // Different IP: fresh bucket.
    client
        .get("/independent-ips")
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

    // Use the raw axum router built by the framework (via TestApp::router) so
    // the throttle attribute wiring is exercised end-to-end.
    let mut config = base_config();
    config.security.rate_limit.enabled = true;
    let app: Router = TestApp::new()
        .routes(routes![exempt_route])
        .config(config)
        .build()
        .into_router();

    let peer: SocketAddr = "127.0.0.1:65000".parse().expect("addr");

    // First request without exempt marker: succeeds (limit = 1).
    let req = {
        let mut r = Request::builder()
            .method("GET")
            .uri("/exempt")
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
            .uri("/exempt")
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
            .uri("/exempt")
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
async fn envelope_counted_marker_still_charges_per_route_throttle() {
    // Regression for the split-marker fix (#1350): an MCP `tools/call` replay
    // now carries `RateLimitEnvelopeCounted` (not `RateLimitExempt`). That
    // marker only dedups the framework-default limiter's envelope bucket — it
    // must NOT bypass the per-route `#[throttle]` bucket, so the route still
    // 429s after its limit, exactly as a direct call would.
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let mut config = base_config();
    config.security.rate_limit.enabled = true;
    let app: Router = TestApp::new()
        .routes(routes![envelope_counted_route])
        .config(config)
        .build()
        .into_router();

    let peer: SocketAddr = "127.0.0.1:65010".parse().expect("addr");

    let build_req = || {
        let mut r = Request::builder()
            .method("GET")
            .uri("/envelope-counted")
            .body(Body::empty())
            .expect("request builds");
        r.extensions_mut().insert(ConnectInfo(peer));
        // Simulate the MCP replay marker on EVERY request.
        r.extensions_mut().insert(RateLimitEnvelopeCounted);
        r
    };

    // limit = 1: the first envelope-counted request is allowed…
    let resp1 = app.clone().oneshot(build_req()).await.expect("oneshot");
    assert_eq!(resp1.status(), StatusCode::OK);

    // …and the second is throttled — the marker did NOT bypass the route bucket.
    let resp2 = app.clone().oneshot(build_req()).await.expect("oneshot");
    assert_eq!(resp2.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn principal_key_isolates_by_principal_extension() {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use tower::ServiceExt;

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

/// Regression (Codex P2, PR #1662): a `#[throttle(key = "principal")]` route must
/// key on the authenticated principal even when the GLOBAL limiter is left at its
/// default IP strategy — the case where the router does NOT install
/// `populate_rate_limit_principal`, so no `RateLimitPrincipal` extension is ever
/// set. `__check_throttle` must derive the principal from the same verified
/// session (`user_id`) `populate_rate_limit_principal` reads; otherwise every user
/// behind one source IP would silently share the route bucket.
///
/// Two principals share one source IP: principal A is throttled after its own
/// limit while principal B — with no `RateLimitPrincipal` extension either — still
/// passes, proving keying is per-principal, not per-IP.
#[tokio::test]
async fn principal_key_derives_from_session_without_global_principal_middleware() {
    // Global limiter enabled but at its DEFAULT (IP) strategy — so the router
    // does NOT install populate_rate_limit_principal and no RateLimitPrincipal
    // extension is ever set. The global IP bucket is generous (burst = 1000), so
    // it never 429s here; only the per-route principal throttle can.
    let client = TestApp::new()
        .routes(routes![
            principal_session_throttled,
            throttle_login_alice,
            throttle_login_bob
        ])
        .config(base_config())
        .build();

    // Establish two authenticated sessions (user_id = alice / bob) and capture
    // each session cookie. TestClient has no cookie jar, so sessions stay
    // isolated and we replay the exact cookie we want on each request.
    let session_cookie = |set_cookie: &str| -> String {
        set_cookie
            .split(';')
            .next()
            .expect("set-cookie should start with a cookie pair")
            .to_owned()
    };

    let alice_login = client.get("/throttle-login-alice").send().await;
    alice_login.assert_status(200);
    let alice_cookie = session_cookie(
        alice_login
            .header("set-cookie")
            .expect("login must set a session cookie"),
    );

    let bob_login = client.get("/throttle-login-bob").send().await;
    bob_login.assert_status(200);
    let bob_cookie = session_cookie(
        bob_login
            .header("set-cookie")
            .expect("login must set a session cookie"),
    );

    // Both principals share ONE source IP; only the derived principal should
    // differentiate their per-route buckets.
    let ip = "198.51.100.88";

    // Principal "alice": first allowed…
    client
        .get("/principal-session")
        .header("Cookie", &alice_cookie)
        .header("X-Forwarded-For", ip)
        .send()
        .await
        .assert_status(200);
    // …second throttled (limit = 1), proving the principal was derived from the
    // session even without the global principal middleware.
    client
        .get("/principal-session")
        .header("Cookie", &alice_cookie)
        .header("X-Forwarded-For", ip)
        .send()
        .await
        .assert_status(429);

    // Principal "bob" shares alice's source IP but is a distinct principal, so it
    // gets its OWN bucket and still passes — impossible under silent IP fallback.
    client
        .get("/principal-session")
        .header("Cookie", &bob_cookie)
        .header("X-Forwarded-For", ip)
        .send()
        .await
        .assert_status(200);
}

/// Regression (Codex P2, PR #1662): a plain user handler that returns its OWN 429
/// (no rate-limit headers) DID consume a global token, so the global limiter's
/// allowed path must still attach its informational `x-ratelimit-*` quota
/// headers. The earlier fix skipped header insertion for ANY inner 429, which was
/// too broad and stripped quota metadata from legitimate user 429s. The corrected
/// logic only avoids OVERWRITING headers the inner response already set (a
/// per-route throttle's 429), so a header-less user 429 still gets the globals.
#[tokio::test]
async fn user_handler_429_receives_global_ratelimit_headers() {
    let client = TestApp::new()
        .routes(routes![user_429])
        .config(base_config())
        .build();

    let resp = client
        .get("/user-429")
        .header("X-Forwarded-For", "198.51.100.77")
        .send()
        .await;
    resp.assert_status(429);

    // The global limiter allowed this request through (consuming a token) before
    // the handler returned its own 429, so the global quota headers are present
    // even though the handler set none.
    assert!(
        resp.header("x-ratelimit-limit").is_some(),
        "global x-ratelimit-limit must be present on a header-less user 429"
    );
    assert!(
        resp.header("x-ratelimit-remaining").is_some(),
        "global x-ratelimit-remaining must be present on a header-less user 429"
    );
}

/// Bug #1350: `#[throttle(key = "ip")]` must resolve the client IP via the same
/// top-level `[security.trusted_proxies]` resolver the global limiter uses, so a
/// client behind a configured proxy is keyed on its real forwarded address.
///
/// Before the fix the throttle path rebuilt its limiter from only the deprecated
/// `security.rate_limit.*` proxy fields. With the recommended top-level config
/// (and no legacy fields), IP extraction distrusted `X-Forwarded-For`; since the
/// test transport sets no TCP peer, the limiter saw no key and *bypassed* every
/// request — so neither request below would ever have been throttled.
#[tokio::test]
async fn throttle_ip_key_uses_top_level_trusted_proxies_resolver() {
    let client = TestApp::new()
        .routes(routes![trusted_proxy_ip])
        .config(top_level_trusted_proxy_config())
        .build();

    // XFF chain: real client, then one proxy hop. `trusted_hops = 1` peels the
    // proxy and keys on the real client 4.4.4.4. First request is allowed.
    client
        .get("/trusted-proxy-ip")
        .header("X-Forwarded-For", "4.4.4.4, 5.5.5.5")
        .send()
        .await
        .assert_status(200);
    // Same real client again → 429 (bucket of limit = 1 is spent). Under the bug
    // this returned 200 because the throttle bypassed IP-less requests.
    client
        .get("/trusted-proxy-ip")
        .header("X-Forwarded-For", "4.4.4.4, 5.5.5.5")
        .send()
        .await
        .assert_status(429);
    // A different real client behind the same proxy gets an independent bucket,
    // confirming keying is on the forwarded client, not the shared proxy hop.
    client
        .get("/trusted-proxy-ip")
        .header("X-Forwarded-For", "6.6.6.6, 5.5.5.5")
        .send()
        .await
        .assert_status(200);
}

/// Bug #1350: on the shared Redis backend, two distinct throttled routes for the
/// same client must NOT collide on one bucket. Before the fix every per-route
/// limiter shared the single `redis.key_prefix` and keyed only on the client, so
/// spending route A's bucket also spent route B's.
///
/// Requires a real Redis (via testcontainers/Docker), so it is `#[ignore]`d by
/// default and gated on `test-support`. Run with:
/// `cargo test -p autumn-web --features "redis test-support" --test integration_tests \
///     throttle_redis_routes_do_not_share_bucket -- --ignored`.
#[cfg(all(feature = "redis", feature = "test-support"))]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn throttle_redis_routes_do_not_share_bucket() {
    use autumn_web::security::{RateLimitBackend, RateLimitRedisConfig};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis as RedisImage;

    // Isolate from any per-route limiters cached by earlier tests in this process.
    autumn_web::security::__throttle_registry_reset();

    let container = RedisImage::default()
        .start()
        .await
        .expect("failed to start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");

    let mut config = base_config();
    config.security.rate_limit.backend = RateLimitBackend::Redis;
    config.security.rate_limit.redis = RateLimitRedisConfig {
        url: Some(format!("redis://127.0.0.1:{port}")),
        key_prefix: "test:throttle".to_owned(),
    };

    let client = TestApp::new()
        .routes(routes![redis_route_a, redis_route_b])
        .config(config)
        .build();

    let ip = "198.51.100.200";

    // Route A, first hit for this client → allowed (limit = 1).
    client
        .get("/redis-route-a")
        .header("X-Forwarded-For", ip)
        .send()
        .await
        .assert_status(200);
    // Route B, first hit for the SAME client → must also be allowed: its Redis
    // bucket is namespaced by route. Under the bug this was 429 (shared bucket).
    client
        .get("/redis-route-b")
        .header("X-Forwarded-For", ip)
        .send()
        .await
        .assert_status(200);
    // Each route independently enforces its own limit on the second hit.
    client
        .get("/redis-route-a")
        .header("X-Forwarded-For", ip)
        .send()
        .await
        .assert_status(429);
    client
        .get("/redis-route-b")
        .header("X-Forwarded-For", ip)
        .send()
        .await
        .assert_status(429);
}
