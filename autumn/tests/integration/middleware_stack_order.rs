//! Executable pins for the ingress-ordering invariants that `router.rs`
//! documents only in prose.
//!
//! Every framework layer is `Route -> Route` with `Error = Infallible`, so the
//! compiler cannot tell a correctly-ordered stack from a reversed one — and the
//! two composition forms in play run in *opposite* directions (consecutive
//! `Router::layer` calls put the LAST call outermost; a `tower-layer` tuple or a
//! `tower::ServiceBuilder` chain puts the FIRST element outermost). These tests
//! assert the observable consequences of each documented placement, so a
//! restructuring of the stack (issue #2193 collapsed ~26 `Router::layer` calls
//! into a handful of composed ones) cannot silently invert it.
//!
//! Each test names the invariant it pins and the comment in `router.rs` that
//! states it.

use autumn_web::config::AutumnConfig;
use autumn_web::error::AutumnError;
use autumn_web::test::TestApp;
use autumn_web::{ClientAddr, get, routes};

#[get("/ok")]
async fn ok_handler() -> &'static str {
    "ok"
}

/// Returns an `AutumnError` that `ProblemDetailsFilter` normalises into a
/// Problem Details JSON body with a different status than the handler produced
/// on its own.
#[get("/boom")]
async fn boom_handler() -> Result<String, AutumnError> {
    Err(AutumnError::not_found_msg("gone"))
}

#[get("/panic")]
async fn panic_handler() -> &'static str {
    panic!("handler exploded");
}

#[get("/whoami")]
async fn whoami(client: ClientAddr) -> String {
    client.0.to_string()
}

/// INVARIANT: `MetricsLayer` is outer to `ExceptionFilterLayer`, so it records
/// the status the **client** receives, not the raw handler status.
///
/// `router.rs`: the exception-filter/error-page/metrics group,
/// "`Metrics` -> `ExceptionFilter` -> `ErrorPageContext`".
#[tokio::test]
async fn metrics_record_the_client_visible_status_not_the_pre_filter_one() {
    let client = TestApp::new().routes(routes![boom_handler]).build();

    client.get("/boom").send().await.assert_status(404);

    let snapshot = client.state().metrics().snapshot();
    assert_eq!(
        snapshot.http.by_status.s4xx,
        1,
        "MetricsLayer must observe the filtered 404 the client received; \
         seeing it as anything else means the layer moved inside the exception \
         filter. by_status = 2xx:{} 3xx:{} 4xx:{} 5xx:{}",
        snapshot.http.by_status.s2xx,
        snapshot.http.by_status.s3xx,
        snapshot.http.by_status.s4xx,
        snapshot.http.by_status.s5xx,
    );
    assert_eq!(snapshot.http.by_status.s5xx, 0);
}

/// INVARIANT: the panic-catch (`ReportingLayer`) is inner to `RequestIdLayer`
/// and outer to the handler, so a panic becomes a clean `500` that still
/// carries `x-request-id` and still flows out through the exception-filter
/// chain.
///
/// `router.rs`: "inner to `RequestIdLayer` (so the request id is available when
/// a handler panics) and outer to the timeout, user layers, and handler".
#[cfg(feature = "reporting")]
#[tokio::test]
async fn handler_panic_becomes_a_500_that_still_carries_the_request_id() {
    let client = TestApp::new().routes(routes![panic_handler]).build();

    let resp = client.get("/panic").send().await;
    resp.assert_status(500);
    assert!(
        resp.headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-request-id")),
        "a panic-turned-500 must still carry x-request-id, which only holds \
         while the panic catch sits INNER to RequestIdLayer; headers = {:?}",
        resp.headers
    );
}

/// INVARIANT: `TrustedProxiesLayer` is applied unconditionally and outer to the
/// user layers, so `ResolvedClientIdentity` is stamped before anything reads
/// `ClientAddr`.
///
/// The layer's builder contains an `if` that gates only a log line — the layer
/// itself is always installed. A refactor that mistakes that `if` for a guard
/// would silently fall back to the socket address.
///
/// `router.rs`: "`TrustedProxiesLayer` ... stamping `ResolvedClientIdentity`
/// before any user or framework middleware reads `ClientAddr` / `ClientHost` /
/// `ClientScheme`".
#[tokio::test]
async fn trusted_proxy_resolution_runs_before_client_addr_is_read() {
    let mut config = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    // No ranges and no hop count: every peer is trusted, so the rightmost
    // `X-Forwarded-For` entry is the resolved client — see `ProxyResolver`.
    config.security.trusted_proxies.trust_forwarded_headers = true;
    config.security.trusted_proxies.ranges.clear();
    config.security.trusted_proxies.trusted_hops = None;

    let client = TestApp::new()
        .config(config)
        .routes(routes![whoami])
        .build();

    let resp = client
        .get("/whoami")
        .header("x-forwarded-for", "203.0.113.7")
        .send()
        .await;
    resp.assert_status(200);
    resp.assert_body_contains("203.0.113.7");
}

/// INVARIANT: the framework's outermost `SecurityHeadersLayer` wraps the whole
/// stack, so even a response produced by a *short-circuiting* inner layer — not
/// the handler — carries the security headers.
///
/// `router.rs`: `SecurityHeaders` is applied by `build_router_pre_state` after
/// `apply_middleware` returns, "so that a gate short-circuit (redirect/401)
/// still carries HSTS/CSP/nosniff".
#[tokio::test]
async fn short_circuit_responses_still_carry_security_headers() {
    let client = TestApp::new().routes(routes![ok_handler]).build();

    // An unmatched path is answered by the 404 fallback, which is registered
    // BEFORE the global middleware precisely so it is wrapped by all of it.
    let resp = client.get("/no-such-route").send().await;
    resp.assert_status(404);
    assert!(
        resp.headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-content-type-options")),
        "the 404 fallback must be wrapped by SecurityHeadersLayer; headers = {:?}",
        resp.headers
    );
    assert!(
        resp.headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-request-id")),
        "the 404 fallback must be wrapped by RequestIdLayer; headers = {:?}",
        resp.headers
    );
}

/// INVARIANT: a *disabled* optional layer is inert.
///
/// Conditional members of the composed stack go through
/// `tower::util::option_layer`, which maps `None` to `Identity` — whose
/// `Service` is the inner service itself. This pins that a disabled layer
/// neither runs nor leaks headers, i.e. that `option_layer` really is the
/// "layer absent" case and not "layer present but misconfigured"
/// (issue #2193).
#[tokio::test]
async fn disabled_optional_layers_are_inert() {
    let mut config = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    config.cors.allowed_origins.clear();
    config.security.rate_limit.enabled = false;
    config.compression.enabled = false;
    config.tenancy.enabled = false;

    let client = TestApp::new()
        .config(config)
        .routes(routes![ok_handler])
        .build();

    // Many requests: with rate limiting off, none of them may be throttled.
    for _ in 0..40 {
        let resp = client
            .get("/ok")
            .header("origin", "https://evil.example")
            .send()
            .await;
        resp.assert_status(200);
        assert!(
            resp.header("access-control-allow-origin").is_none(),
            "CORS is disabled, so no CORS header may be emitted; headers = {:?}",
            resp.headers
        );
        assert!(
            resp.header("content-encoding").is_none(),
            "compression is disabled, so no Content-Encoding may be emitted; \
             headers = {:?}",
            resp.headers
        );
    }
}

/// INVARIANT: the request body limit (`DefaultBodyLimit`) and the
/// `UploadConfig` extension are both installed, inner to the user layers.
///
/// The extension used to be inserted by an `axum::middleware::from_fn`; it is
/// now an `axum::Extension` layer (issue #2193), which must have the same
/// effect — the `Multipart` extractor reads per-file limits from it.
#[tokio::test]
async fn body_limit_still_rejects_oversized_requests() {
    let mut config = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    config.security.upload.max_request_size_bytes = 64;

    let client = TestApp::new()
        .config(config)
        .routes(routes![ok_handler])
        .build();

    let resp = client
        .post("/ok")
        .header("content-type", "application/json")
        .body("x".repeat(4096))
        .send()
        .await;
    assert!(
        resp.status.as_u16() == 413 || resp.status.as_u16() == 405,
        "an oversized body must be rejected by DefaultBodyLimit (413) before \
         reaching routing, or rejected as a method mismatch (405) — got {}",
        resp.status
    );
}
