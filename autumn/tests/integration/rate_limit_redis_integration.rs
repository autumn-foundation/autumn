#![cfg(feature = "redis")]
//! Integration tests: Redis-backed rate limiter across two "replicas".
//!
//! Verifies that with `backend = "redis"`, a single client IP hitting
//! two separate `RateLimitLayer` instances (simulating two pods sharing
//! one Redis) is bounded by the *global* configured rate, not 2× it.
//!
//! Also verifies the `on_backend_failure` postures and confirms that the
//! memory backend still leaks ~2× the configured limit across two instances
//! (the contrast described in the acceptance criteria).
//!
//! The Docker-dependent test (`two_replicas_share_global_budget`) is gated
//! on the `test-support` feature (which pulls in `testcontainers`).

use autumn_web::security::{
    KeyStrategy, RateLimitBackend, RateLimitBackendFailure, RateLimitConfig, RateLimitLayer,
    RateLimitRedisConfig,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use tower::ServiceExt;

fn app_from_config(config: &RateLimitConfig) -> Router {
    Router::new()
        .route("/ping", get(|| async { "pong" }))
        .layer(RateLimitLayer::from_config(config))
}

fn req_for_ip(ip: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/ping")
        .header("X-Forwarded-For", ip)
        .body(Body::empty())
        .expect("infallible request builder")
}

/// Two router instances sharing one Redis → global budget must hold.
///
/// burst=20, rps=10. We fire 60 requests very quickly (negligible refill)
/// alternating between two routers. With a *global* budget of burst=20 tokens,
/// at most ~20 should be allowed regardless of which replica handles them.
#[cfg(feature = "test-support")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn two_replicas_share_global_budget() {
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis as RedisImage;

    let container = RedisImage::default()
        .start()
        .await
        .expect("failed to start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let redis_url = format!("redis://127.0.0.1:{port}");

    let config = RateLimitConfig {
        enabled: true,
        requests_per_second: 10.0,
        burst: 20,
        trust_forwarded_headers: true,
        trusted_proxies: Vec::new(),
        backend: RateLimitBackend::Redis,
        redis: RateLimitRedisConfig {
            url: Some(redis_url),
            key_prefix: "test:rl".to_owned(),
        },
        on_backend_failure: RateLimitBackendFailure::FailOpen,
        key_strategy: KeyStrategy::default(),
        tiers: std::collections::HashMap::new(),
    };
    let app_a = app_from_config(&config);
    let app_b = app_from_config(&config);

    let client_ip = "198.51.100.7";
    let total = 60_usize;
    let mut allowed = 0_usize;

    for i in 0..total {
        let req = req_for_ip(client_ip);
        let resp = if i % 2 == 0 {
            app_a.clone().oneshot(req).await.expect("request failed")
        } else {
            app_b.clone().oneshot(req).await.expect("request failed")
        };
        if resp.status() == StatusCode::OK {
            allowed += 1;
        }
    }

    // With burst=20 the global limiter allows at most 20 (+ negligible refill).
    // Allow a tiny margin of 2 extra for timing jitter.
    assert!(
        allowed <= 22,
        "global budget not enforced: {allowed}/{total} passed (expected ≤ 22)"
    );
    // The initial burst must have been served.
    assert!(
        allowed >= 1,
        "at least some requests should have been allowed"
    );
}

/// Memory backend: two instances have *independent* buckets → each allows up to `burst`.
///
/// This is the contrast: with `backend = "memory"` two replicas allow ~2× burst.
#[tokio::test]
async fn memory_replicas_have_independent_budgets() {
    let config = RateLimitConfig {
        enabled: true,
        requests_per_second: 10.0,
        burst: 5,
        trust_forwarded_headers: true,
        trusted_proxies: Vec::new(),
        backend: RateLimitBackend::Memory,
        redis: RateLimitRedisConfig::default(),
        on_backend_failure: RateLimitBackendFailure::FailOpen,
        key_strategy: KeyStrategy::default(),
        tiers: std::collections::HashMap::new(),
    };
    let app_a = app_from_config(&config);
    let app_b = app_from_config(&config);

    let client_ip = "10.0.0.1";
    let mut allowed = 0_usize;

    // 20 requests alternating → each replica gets 10 requests, sees burst=5.
    // With independent buckets both replicas serve their own burst: ~10 pass.
    for i in 0..20_usize {
        let req = req_for_ip(client_ip);
        let resp = if i % 2 == 0 {
            app_a.clone().oneshot(req).await.expect("request failed")
        } else {
            app_b.clone().oneshot(req).await.expect("request failed")
        };
        if resp.status() == StatusCode::OK {
            allowed += 1;
        }
    }

    // Each replica independently allows burst=5 → ~10 total.
    assert!(
        allowed >= 8,
        "memory replicas should allow ~2× burst: only {allowed} passed"
    );
}

/// `fail_open`: when Redis is unreachable every request passes through.
#[tokio::test]
async fn fail_open_allows_requests_when_redis_unavailable() {
    let config = RateLimitConfig {
        enabled: true,
        requests_per_second: 1.0,
        burst: 1,
        trust_forwarded_headers: true,
        trusted_proxies: Vec::new(),
        backend: RateLimitBackend::Redis,
        redis: RateLimitRedisConfig {
            // Port nobody listens on.
            url: Some("redis://127.0.0.1:19999".to_owned()),
            key_prefix: "test:rl".to_owned(),
        },
        on_backend_failure: RateLimitBackendFailure::FailOpen,
        key_strategy: KeyStrategy::default(),
        tiers: std::collections::HashMap::new(),
    };
    let app = app_from_config(&config);

    for _ in 0..5_usize {
        let resp = app
            .clone()
            .oneshot(req_for_ip("1.2.3.4"))
            .await
            .expect("request failed");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "fail_open: request must pass through when Redis is down"
        );
    }
}

/// `fail_closed`: when Redis is unreachable every request gets `429`.
#[tokio::test]
async fn fail_closed_blocks_requests_when_redis_unavailable() {
    let config = RateLimitConfig {
        enabled: true,
        requests_per_second: 1.0,
        burst: 1,
        trust_forwarded_headers: true,
        trusted_proxies: Vec::new(),
        backend: RateLimitBackend::Redis,
        redis: RateLimitRedisConfig {
            url: Some("redis://127.0.0.1:19999".to_owned()),
            key_prefix: "test:rl".to_owned(),
        },
        on_backend_failure: RateLimitBackendFailure::FailClosed,
        key_strategy: KeyStrategy::default(),
        tiers: std::collections::HashMap::new(),
    };
    let app = app_from_config(&config);

    let resp = app
        .clone()
        .oneshot(req_for_ip("5.6.7.8"))
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "fail_closed: request must be blocked when Redis is down"
    );
}
