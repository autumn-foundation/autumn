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
use autumn_web::{ClientAddr, get, post, routes};

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

/// Consumes a body, so the global `DefaultBodyLimit` is actually enforced —
/// the limit layer only stamps an extension; body *extractors* apply it.
#[post("/echo")]
async fn echo(body: axum::body::Bytes) -> String {
    body.len().to_string()
}

/// Reports the `request_id` the log context was seeded with, which is only
/// populated if `RequestIdLayer` ran *before* `LogContextLayer`.
#[get("/log-context-request-id")]
async fn log_context_request_id() -> String {
    autumn_web::log::context::snapshot()
        .and_then(|fields| fields.request_id)
        .unwrap_or_else(|| "unseeded".to_owned())
}

/// Reports the `UploadConfig` the ingress stack put into request extensions,
/// so the test can tell "the extension is installed" from "it silently
/// vanished".
#[get("/upload-config")]
async fn upload_config_probe(
    config: Option<axum::Extension<autumn_web::security::UploadConfig>>,
) -> String {
    config.map_or_else(
        || "missing".to_owned(),
        |axum::Extension(cfg)| cfg.max_request_size_bytes.to_string(),
    )
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

/// INVARIANT: both upload guards are installed, inner to the user layers — the
/// global `DefaultBodyLimit` and the `UploadConfig` request extension.
///
/// The extension used to be inserted by an `axum::middleware::from_fn`; it is
/// now an `axum::Extension` layer (issue #2193), which must have exactly the
/// same effect — the `Multipart` extractor reads per-file limits and the
/// allowed MIME-type list from it.
///
/// Note that `DefaultBodyLimit` only stamps an extension; the limit is applied
/// by body *extractors*, so the route under test must actually consume a body
/// or the check is vacuous.
#[tokio::test]
async fn upload_guards_are_installed_in_the_ingress_stack() {
    let mut config = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    config.security.upload.max_request_size_bytes = 128;

    let client = TestApp::new()
        .config(config)
        .routes(routes![echo, upload_config_probe])
        .build();

    // Under the cap: served normally.
    client
        .post("/echo")
        .header("content-type", "application/octet-stream")
        .body("x".repeat(64))
        .send()
        .await
        .assert_status(200)
        .assert_body_contains("64");

    // Over the cap: rejected by `DefaultBodyLimit`, and by nothing else — a 405
    // or 404 here would mean the request never reached the body extractor.
    client
        .post("/echo")
        .header("content-type", "application/octet-stream")
        .body("x".repeat(4096))
        .send()
        .await
        .assert_status(413);

    // The `UploadConfig` extension reached the handler with the configured value.
    client
        .get("/upload-config")
        .send()
        .await
        .assert_status(200)
        .assert_body_contains("128");
}

/// INVARIANT: `RequestIdLayer` is OUTER to `LogContextLayer`, so the request id
/// is available to seed the request-scoped log context — and the context wraps
/// the handler, so everything it logs correlates with the `x-request-id` the
/// client sees.
///
/// This is the adjacency most at risk from the tuple collapse: the two layers
/// are the first two elements of the middle group, and swapping them still
/// compiles (both are `Route -> Route`, `Error = Infallible`) while silently
/// dropping `request_id` from every log line emitted during the request.
///
/// `router.rs`: "RequestId stays here (inner to session) so the request id seeds
/// the session, logs, and trace context", and `LogContextLayer` is "inner to
/// `RequestIdLayer` (so the request id is available to seed it)".
#[tokio::test]
async fn log_context_is_seeded_with_the_request_id_the_client_sees() {
    let client = TestApp::new()
        .routes(routes![log_context_request_id])
        .build();

    let resp = client.get("/log-context-request-id").send().await;
    resp.assert_status(200);

    let header_id = resp
        .header("x-request-id")
        .expect("RequestIdLayer must stamp x-request-id")
        .to_owned();
    let context_id = String::from_utf8(resp.body.clone()).expect("body is utf-8");

    assert_eq!(
        context_id, header_id,
        "the log context must be seeded with the same request id the client \
         receives, which only holds while RequestIdLayer is OUTER to \
         LogContextLayer and LogContextLayer wraps the handler"
    );
}
