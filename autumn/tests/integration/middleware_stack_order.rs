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
//! states it, and each is written so it would actually FAIL if that invariant
//! were inverted — a test that passes either way is worse than no test, because
//! it reads like coverage.
//!
//! NOT covered here: `RequestIdLayer` being outer to `LogContextLayer`. The only
//! router-level observable is the request id the log context is seeded with, and
//! reading it back through a handler proved intermittent when other tests run
//! concurrently, so a gate built on it would be flaky. That pair is pinned at the
//! layer level instead, by the tests in `src/middleware/log_context.rs`.

use std::borrow::Cow;
use std::task::{Context, Poll};

use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::error::AutumnError;
use autumn_web::middleware::{AutumnErrorInfo, ExceptionFilter};
use autumn_web::plugin::Plugin;
use autumn_web::test::TestApp;
use autumn_web::{ClientAddr, get, post, routes};
use axum::extract::Request;
use axum::response::{IntoResponse, Response};

#[get("/ok")]
async fn ok_handler() -> &'static str {
    "ok"
}

/// Returns a 404 `AutumnError`. Paired with `RewriteStatusTo500` below, the
/// status the client sees differs from the one the handler produced.
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

/// Rewrites any error response's status to `500`. The shipped
/// `ProblemDetailsFilter` only rebuilds the *body*, so a status-changing filter
/// is the only way to tell "metrics observed the response before the filter
/// chain" from "after".
struct RewriteStatusTo500;

impl ExceptionFilter for RewriteStatusTo500 {
    fn filter(&self, _error: &AutumnErrorInfo, response: Response) -> Response {
        let (mut parts, body) = response.into_parts();
        parts.status = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
        Response::from_parts(parts, body)
    }
}

/// `TestApp` has no direct exception-filter setter, but it merges an
/// `AppBuilder` built by a plugin — which does.
struct RewriteStatusPlugin;

impl Plugin for RewriteStatusPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("rewrite-status-filter")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.exception_filter(RewriteStatusTo500)
    }
}

/// INVARIANT: `MetricsLayer` is outer to `ExceptionFilterLayer`, so it records
/// the status the **client** receives, not the raw handler status.
///
/// The discriminating part is the status-*changing* filter: the handler yields
/// 404 and the filter chain turns it into 500, so the metrics bucket says
/// unambiguously which side of the filter `MetricsLayer` sits on. The shipped
/// `ProblemDetailsFilter` rebuilds only the body, so a test using it alone
/// would pass either way.
///
/// `router.rs`: the exception-filter/error-page/metrics group,
/// "`Metrics` -> `ExceptionFilter` -> `ErrorPageContext`".
#[tokio::test]
async fn metrics_record_the_client_visible_status_not_the_pre_filter_one() {
    let client = TestApp::new()
        .plugin(RewriteStatusPlugin)
        .routes(routes![boom_handler])
        .build();

    // The handler returns 404; the filter chain rewrites it to 500.
    client.get("/boom").send().await.assert_status(500);

    let snapshot = client.state().metrics().snapshot();
    assert_eq!(
        snapshot.http.by_status.s5xx,
        1,
        "MetricsLayer must observe the 500 the client received, not the \
         handler's pre-filter 404 — seeing 4xx here means the layer moved \
         inside the exception filter. by_status = 2xx:{} 3xx:{} 4xx:{} 5xx:{}",
        snapshot.http.by_status.s2xx,
        snapshot.http.by_status.s3xx,
        snapshot.http.by_status.s4xx,
        snapshot.http.by_status.s5xx,
    );
    assert_eq!(
        snapshot.http.by_status.s4xx, 0,
        "the pre-filter 404 must not appear in the metrics"
    );
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

/// A `static_gate` layer that short-circuits `/gated` with a `302` **without
/// ever calling the inner service** — the shape a real page-cache gate uses.
#[derive(Clone)]
struct RedirectGateLayer;

impl<S> tower::Layer<S> for RedirectGateLayer {
    type Service = RedirectGateService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RedirectGateService { inner }
    }
}

#[derive(Clone)]
struct RedirectGateService<S> {
    inner: S,
}

impl<S> tower::Service<Request> for RedirectGateService<S>
where
    S: tower::Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        if req.uri().path() == "/gated" {
            return Box::pin(async {
                Ok((
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, "/login")],
                )
                    .into_response())
            });
        }
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);
        Box::pin(async move { inner.call(req).await })
    }
}

/// INVARIANT: the framework's outermost `SecurityHeadersLayer` wraps the
/// `static_gate` layers, so a gate short-circuit — a response the inner stack
/// never produced — still carries HSTS/CSP/nosniff.
///
/// This needs a layer that returns *without calling the inner service*. The 404
/// fallback does not qualify: it is registered before every layer, so its
/// response travels the whole stack outward exactly like a handler's would, and
/// would still carry the headers even if `SecurityHeadersLayer` were innermost.
///
/// `router.rs`: `SecurityHeaders` is applied by `build_router_pre_state` after
/// `apply_middleware` returns and after the gate, "so that a gate short-circuit
/// (redirect/401) still carries HSTS/CSP/nosniff".
#[tokio::test]
async fn gate_short_circuit_still_carries_security_headers() {
    let client = TestApp::new()
        .routes(routes![ok_handler])
        .static_gate(RedirectGateLayer)
        .build();

    let resp = client.get("/gated").send().await;
    resp.assert_status(302);
    assert_eq!(resp.header("location"), Some("/login"));
    assert!(
        resp.header("x-content-type-options").is_some(),
        "a gate short-circuit never reaches the inner stack, so it can only \
         carry the security headers while SecurityHeadersLayer is applied \
         OUTSIDE the static_gate layers; headers = {:?}",
        resp.headers
    );

    // Control: a normal request through the same app is unaffected by the gate.
    client
        .get("/ok")
        .send()
        .await
        .assert_status(200)
        .assert_body_contains("ok");
}

/// INVARIANT: the 404 fallback is registered BEFORE the global middleware, so
/// unmatched routes are still wrapped by the whole ingress stack rather than
/// bypassing it.
///
/// `router.rs`: "404 fallback handler for unmatched routes must be registered
/// BEFORE global middleware so that unmatched routes are still protected by
/// rate limiting, CSRF, CORS, etc."
#[tokio::test]
async fn unmatched_routes_are_wrapped_by_the_framework_stack() {
    let client = TestApp::new().routes(routes![ok_handler]).build();

    let resp = client.get("/no-such-route").send().await;
    resp.assert_status(404);
    assert!(
        resp.header("x-content-type-options").is_some(),
        "the 404 fallback must be wrapped by SecurityHeadersLayer; headers = {:?}",
        resp.headers
    );
    assert!(
        resp.header("x-request-id").is_some(),
        "the 404 fallback must be wrapped by RequestIdLayer; headers = {:?}",
        resp.headers
    );
}

/// INVARIANT: a config-disabled optional layer is genuinely absent, not
/// present-and-denying.
///
/// Conditional members of the composed stack go through
/// `tower::util::option_layer`, which maps `None` to `Identity` — whose
/// `Service` is the inner service itself. Each case below is paired with a
/// **positive control** built from the same code path with the layer enabled,
/// so the assertion distinguishes "layer absent" from "layer present but
/// happening to say nothing" (issue #2193).
#[tokio::test]
async fn option_layer_none_really_means_the_layer_is_absent() {
    // ── CORS ────────────────────────────────────────────────────────────────
    let mut enabled = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    enabled.cors.allowed_origins = vec!["https://allowed.example".to_owned()];
    let with_cors = TestApp::new()
        .config(enabled)
        .routes(routes![ok_handler])
        .build();
    let resp = with_cors
        .get("/ok")
        .header("origin", "https://allowed.example")
        .send()
        .await;
    resp.assert_status(200);
    assert!(
        resp.header("access-control-allow-origin").is_some(),
        "positive control failed: an enabled CorsLayer must emit the header for \
         an allowed origin, otherwise the negative case below proves nothing; \
         headers = {:?}",
        resp.headers
    );

    let mut disabled = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    disabled.cors.allowed_origins.clear();
    let without_cors = TestApp::new()
        .config(disabled)
        .routes(routes![ok_handler])
        .build();
    let resp = without_cors
        .get("/ok")
        .header("origin", "https://allowed.example")
        .send()
        .await;
    resp.assert_status(200);
    assert!(
        resp.header("access-control-allow-origin").is_none(),
        "with no configured origins the CORS layer must be absent; headers = {:?}",
        resp.headers
    );
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

/// Counts handler invocations for [`csrf_is_validated_before_submit_token`].
/// Process-global, but this route is mounted by that test alone.
static SUBMIT_HANDLER_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[post("/create")]
async fn counted_create() -> &'static str {
    SUBMIT_HANDLER_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    "created"
}

/// INVARIANT: `SubmitTokenLayer` is INNER to `CsrfLayer`, so CSRF is validated
/// first on the request path — and a replayed `_submit_token` is still
/// short-circuited by the replay guard when the request carries a valid `_csrf`
/// (issue #1360, AC #4).
///
/// This runs through the REAL assembled ingress stack (both layers enabled via
/// `AutumnConfig`), which is what distinguishes it from the layer-level unit
/// test `distinct_from_csrf_replayed_submit_token_short_circuits` in
/// `src/security/submit_token.rs`: that router mounts the submit-token layer
/// alone, so it cannot observe the relative order of the two.
///
/// The discriminating assertion is the last one. A replay of an
/// already-consumed token that arrives WITHOUT a valid `_csrf` must be refused
/// `403` — if the guard were outer to CSRF it would serve the stored `200`
/// replay and CSRF would never run. The first two steps pin the direction that
/// AC #4 names: a valid `_csrf` does not stop the replay guard from firing.
///
/// `router.rs`: "Inner to the CSRF layer so CSRF is validated first on the
/// request path; a replayed `_submit_token` is still short-circuited even when
/// the request carries a valid `_csrf`".
#[tokio::test]
async fn csrf_is_validated_before_submit_token() {
    const CSRF_TOKEN: &str = "csrf-order-guard-token";
    const SUBMIT_TOKEN: &str = "submit-order-guard-token";
    // Private to `security::submit_token`; the header is the only observable
    // that separates a replayed response from a freshly-handled one.
    const REPLAYED: &str = "x-submit-token-replayed";

    let mut config = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    // CSRF is off by default; the submit-token guard is on by default and
    // resolves to the per-process memory store outside production.
    config.security.csrf.enabled = true;
    assert!(
        config.security.submit_token.enabled,
        "this test needs the submit-token guard installed by the real router"
    );

    let client = TestApp::new()
        .config(config)
        .routes(routes![counted_create])
        .build();

    // Double-submit: matching cookie and form field, unsigned because no
    // `security.signing_secret` is configured in this profile.
    let submit = |body: String| {
        client
            .post("/create")
            .header("cookie", &format!("autumn-csrf={CSRF_TOKEN}"))
            .form(&body)
    };
    let valid_body = format!("_csrf={CSRF_TOKEN}&_submit_token={SUBMIT_TOKEN}&title=hello");

    let first = submit(valid_body.clone()).send().await;
    first.assert_status(200);
    assert!(
        first.header(REPLAYED).is_none(),
        "the first submission must be handled, not replayed; headers = {:?}",
        first.headers
    );
    assert_eq!(
        SUBMIT_HANDLER_RUNS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the first submission must reach the handler exactly once"
    );

    // AC #4: the same token again, still carrying a VALID `_csrf`, is
    // intercepted by the replay guard rather than refused by CSRF.
    let second = submit(valid_body).send().await;
    assert_ne!(
        second.status.as_u16(),
        403,
        "a replay carrying a valid `_csrf` must not be refused by CSRF; \
         body = {}",
        second.text()
    );
    assert_eq!(
        second.header(REPLAYED),
        Some("true"),
        "the replay guard must short-circuit the second submission; status = {}, \
         headers = {:?}",
        second.status,
        second.headers
    );
    assert_eq!(
        SUBMIT_HANDLER_RUNS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the handler must have run exactly once across both submissions"
    );

    // The ordering discriminator: same consumed token, no CSRF token at all.
    // CSRF runs first, so this is a 403 — not the stored 200 replay.
    let unauthenticated_replay = client
        .post("/create")
        .header("cookie", "unrelated=1")
        .form(&format!("_submit_token={SUBMIT_TOKEN}&title=hello"))
        .send()
        .await;
    assert_eq!(
        unauthenticated_replay.status.as_u16(),
        403,
        "a replay with no valid `_csrf` must be refused by CSRF before the \
         replay guard can serve the stored response; a 200 here means \
         `SubmitTokenLayer` moved OUTSIDE `CsrfLayer`. headers = {:?}, body = {}",
        unauthenticated_replay.headers,
        unauthenticated_replay.text()
    );
    assert_eq!(
        SUBMIT_HANDLER_RUNS.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the CSRF-refused replay must not reach the handler either"
    );
}
