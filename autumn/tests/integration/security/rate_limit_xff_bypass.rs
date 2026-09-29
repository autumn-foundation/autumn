use autumn_web::security::{RateLimitConfig, RateLimitLayer};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::{Router, body::Body, routing::get};
use std::net::SocketAddr;
use tower::ServiceExt;

#[tokio::test]
async fn rate_limit_xff_bypass() {
    let config = RateLimitConfig {
        enabled: true,
        requests_per_second: 0.1,      // very low limit
        burst: 1,                      // only 1 request allowed
        trust_forwarded_headers: true, // typical for setups behind load balancers
        trusted_proxies: vec!["203.0.113.10".to_string()],
        ..Default::default()
    };

    let app = Router::new()
        .route("/", get(|| async { "Hello" }))
        .layer(RateLimitLayer::from_config(&config));

    let peer: SocketAddr = "203.0.113.10:4000".parse().unwrap();

    let make_req = |attacker_ip: &str, real_ip: &str| {
        let mut req = Request::builder()
            .method("GET")
            .uri("/")
            // The attacker sends an XFF header
            .header("X-Forwarded-For", attacker_ip)
            // The proxy appends another XFF header (or appends to the same, but let's test separate headers)
            .header("X-Forwarded-For", real_ip)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        req
    };

    // First request from attacker
    let first = app
        .clone()
        .oneshot(make_req("1.1.1.1", "198.51.100.1"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    // Second request from attacker, but they spoofed their XFF header. The proxy still appended the real IP.
    let second = app
        .clone()
        .oneshot(make_req("2.2.2.2", "198.51.100.1"))
        .await
        .unwrap();

    // If it's vulnerable, it will return OK because it keyed on the first XFF header (1.1.1.1 then 2.2.2.2)
    // instead of the real one.
    // If it's secure, it should return TOO_MANY_REQUESTS because it should see "198.51.100.1" (the last IP).
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "Vulnerable to XFF spoofing: attacker bypassed rate limit!"
    );
}
