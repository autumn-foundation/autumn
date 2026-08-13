//! Maintenance mode Tower middleware.
//!
//! Intercepts HTTP requests when maintenance mode is active and returns
//! `503 Service Unavailable` with a `Retry-After` header. Routes under the
//! configured actuator/health prefix always pass through so platform load
//! balancers keep every replica in rotation.
//!
//! # Response format negotiation
//!
//! - Requests with `Accept: text/html` (or no `Accept`) receive an HTML page.
//! - Requests with `Accept: application/json` or
//!   `Accept: application/problem+json` receive an RFC 7807 Problem Details
//!   JSON object.
//!
//! # Bypass mechanisms
//!
//! In order of evaluation:
//!
//! 1. **Actuator prefix** – paths starting with `health_prefix` (default
//!    `/actuator`) always pass through.
//! 2. **Bypass header** – a configured `(header_name, expected_value)` pair;
//!    requests carrying the matching header are not gated.
//! 3. **IP allow-list** – CIDR-expanded list; clients whose IP matches any
//!    entry bypass the 503.
//! 4. **Read-only mode** – when `readonly = true`, `GET`, `HEAD`, and
//!    `OPTIONS` requests pass through; write methods are gated.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{
    Method, Request, Response, StatusCode,
    header::{ACCEPT, CONTENT_TYPE},
};
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::maintenance::{MaintenanceConfig, MaintenanceState, ip_in_allow_list};

/// Default health/actuator prefix whose routes bypass maintenance.
pub const DEFAULT_HEALTH_PREFIX: &str = "/actuator";

/// Retry-After value (seconds) sent in every 503 response.
const RETRY_AFTER_SECS: &str = "120";

/// Whether `path` matches a health/actuator prefix used by admission-style
/// gates (maintenance mode, load shedding — see
/// [`crate::middleware::load_shed`]): an empty prefix or `"/"` matches only
/// the root path; otherwise `path` must equal the prefix exactly or start
/// with the prefix followed by a `/` segment boundary.
///
/// `prefix_with_slash` must be `prefix` with a trailing `/` appended (or
/// `prefix` itself if it already ends in `/`) — see
/// [`prefix_with_trailing_slash`]. Callers precompute this once at
/// construction time instead of reallocating it on every request, so this
/// function itself never allocates.
///
/// Shared by [`MaintenanceService::gate_request`] and
/// [`crate::middleware::load_shed::LoadShedService`] so the two gates cannot
/// silently diverge on edge cases (e.g. an empty configured prefix).
pub(crate) fn health_prefix_matches(path: &str, prefix: &str, prefix_with_slash: &str) -> bool {
    if prefix.is_empty() || prefix == "/" {
        path == "/"
    } else {
        path == prefix || path.starts_with(prefix_with_slash)
    }
}

/// Precompute the trailing-slash form of `prefix` for
/// [`health_prefix_matches`], once at construction time.
pub(crate) fn prefix_with_trailing_slash(prefix: &str) -> String {
    if prefix.is_empty() || prefix.ends_with('/') {
        prefix.to_owned()
    } else {
        let mut s = String::with_capacity(prefix.len().saturating_add(1));
        s.push_str(prefix);
        s.push('/');
        s
    }
}

/// Tower [`Layer`] that adds maintenance-mode gating to a service.
///
/// Clone this layer and call [`with_health_prefix`](Self::with_health_prefix)
/// to override the default `/actuator` bypass prefix.
#[derive(Clone)]
pub struct MaintenanceLayer {
    state: MaintenanceState,
    paths: Arc<GatePaths>,
}

/// The path sets the gate consults, resolved once at router-assembly time.
///
/// Held behind an `Arc` because the produced [`MaintenanceService`] is cloned
/// on the request path — once per traversal of the ingress stack above it — and
/// this layer is applied unconditionally (there is no router-level "maintenance
/// disabled" gate). Storing the strings by value made every one of those clones
/// deep-copy two `String`s and two `Vec<String>`s (issue #2193).
#[derive(Clone)]
struct GatePaths {
    health_prefix: String,
    health_prefix_slash: String,
    bypass_paths: Vec<String>,
    probe_paths: Vec<String>,
}

impl MaintenanceLayer {
    /// Create a [`MaintenanceLayer`] backed by `state`.
    ///
    /// Uses `/actuator` as the health-check prefix by default.
    #[must_use]
    pub fn new(state: MaintenanceState) -> Self {
        let health_prefix = DEFAULT_HEALTH_PREFIX.to_owned();
        let health_prefix_slash = prefix_with_trailing_slash(&health_prefix);
        Self {
            state,
            paths: Arc::new(GatePaths {
                health_prefix,
                health_prefix_slash,
                bypass_paths: Vec::new(),
                probe_paths: Vec::new(),
            }),
        }
    }

    /// Mutate the (build-time) path set in place, cloning it only if the `Arc`
    /// has already been shared — the builder methods below are chained on a
    /// freshly-constructed layer, so in practice this never copies.
    fn paths_mut(&mut self) -> &mut GatePaths {
        Arc::make_mut(&mut self.paths)
    }

    /// Override the health-check prefix (e.g. `/health`).
    ///
    /// Requests to paths that start with this prefix always pass through,
    /// regardless of maintenance state, so load balancers keep the replica
    /// in rotation.
    #[must_use]
    pub fn with_health_prefix(mut self, prefix: impl Into<String>) -> Self {
        let paths = self.paths_mut();
        paths.health_prefix = prefix.into();
        paths.health_prefix_slash = prefix_with_trailing_slash(&paths.health_prefix);
        self
    }

    /// Configure bypass paths that escape maintenance mode.
    ///
    /// Requests to these paths (or starting with them as slash-delimited segments) always pass through.
    #[must_use]
    pub fn with_bypass_paths(mut self, paths: Vec<String>) -> Self {
        self.paths_mut().bypass_paths = paths;
        self
    }

    /// Configure exact-match health probe paths that escape maintenance mode.
    #[must_use]
    pub fn with_probe_paths(mut self, paths: Vec<String>) -> Self {
        self.paths_mut().probe_paths = paths;
        self
    }
}

impl<S> Layer<S> for MaintenanceLayer {
    type Service = MaintenanceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MaintenanceService {
            inner,
            state: self.state.clone(),
            paths: Arc::clone(&self.paths),
        }
    }
}

/// Tower [`Service`] produced by [`MaintenanceLayer`].
#[derive(Clone)]
pub struct MaintenanceService<S> {
    inner: S,
    state: MaintenanceState,
    paths: Arc<GatePaths>,
}

impl<S> MaintenanceService<S> {
    /// Determine whether this request should be gated by maintenance mode.
    ///
    /// Returns `Some(503 response)` when the request should be blocked, or
    /// `None` when it should pass through to the inner service.
    fn gate_request<B>(
        &self,
        req: &Request<B>,
        config: &MaintenanceConfig,
    ) -> Option<Response<Body>> {
        // 1. Actuator/health routes and configured bypass paths always pass through.
        let path = req.uri().path();
        if health_prefix_matches(
            path,
            &self.paths.health_prefix,
            &self.paths.health_prefix_slash,
        ) {
            return None;
        }
        for probe in &self.paths.probe_paths {
            if path == probe {
                return None;
            }
        }
        for bypass in &self.paths.bypass_paths {
            if path == bypass {
                return None;
            }
            if bypass != "/"
                && !bypass.is_empty()
                && let Some(stripped) = path.strip_prefix(bypass)
                && (bypass.ends_with('/') || stripped.starts_with('/'))
            {
                return None;
            }
        }

        // 2. Bypass header.
        if let Some((header_name, expected_value)) = &config.bypass_header
            && let Some(val) = req.headers().get(header_name.as_str())
            && val.as_bytes() == expected_value.as_bytes()
        {
            return None;
        }

        // 3. IP allow-list.
        if !config.allow_ips.is_empty()
            && let Some(client_ip) = extract_client_ip(req)
            && ip_in_allow_list(&client_ip, &config.allow_ips)
        {
            return None;
        }

        // 4. Read-only mode: safe methods pass through.
        if config.readonly {
            let method = req.method();
            if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
                return None;
            }
        }

        Some(build_503_response(req, config))
    }
}

impl<S, ReqBody> Service<Request<ReqBody>> for MaintenanceService<S>
where
    S: Service<Request<ReqBody>, Response = Response<Body>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = MaintenanceFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        if let Some(config) = self.state.get()
            && let Some(response) = self.gate_request(&req, &config)
        {
            return MaintenanceFuture::ShortCircuit {
                response: Some(response),
            };
        }
        MaintenanceFuture::Forward {
            inner: self.inner.call(req),
        }
    }
}

pin_project! {
    /// Future returned by [`MaintenanceService`].
    ///
    /// Either resolves immediately with a 503 response (short-circuit path)
    /// or delegates to the wrapped inner service.
    #[project = MaintenanceFutureProj]
    pub enum MaintenanceFuture<F> {
        ShortCircuit { response: Option<Response<Body>> },
        Forward { #[pin] inner: F },
    }
}

impl<F, E> Future for MaintenanceFuture<F>
where
    F: Future<Output = Result<Response<Body>, E>>,
{
    type Output = Result<Response<Body>, E>;

    #[allow(
        clippy::expect_used,
        reason = "unreachable: future not polled after Ready"
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            MaintenanceFutureProj::ShortCircuit { response } => Poll::Ready(Ok(response
                .take()
                .expect("MaintenanceFuture polled after completion"))),
            MaintenanceFutureProj::Forward { inner } => inner.poll(cx),
        }
    }
}

/// Extract the client IP from a request.
///
/// When [`crate::security::ResolvedClientIdentity`] is present (stamped by
/// `TrustedProxiesLayer`), its resolved address is used so that the configured
/// trusted-proxy policy governs which forwarded IP is accepted. Raw
/// `X-Forwarded-For` / `X-Real-IP` headers are only consulted as a fallback
/// when the layer is not installed (e.g. in tests).
fn extract_client_ip<B>(req: &Request<B>) -> Option<IpAddr> {
    // Resolved by TrustedProxiesLayer — respects the proxy trust policy.
    if let Some(identity) = req
        .extensions()
        .get::<crate::security::ResolvedClientIdentity>()
    {
        return identity.addr;
    }

    // Fallback: raw headers (TrustedProxiesLayer not installed).

    // X-Forwarded-For: <client>, <proxy1>, <proxy2>
    if let Some(xff) = req.headers().get("x-forwarded-for")
        && let Ok(s) = xff.to_str()
        && let Some(first) = s.split(',').next()
        && let Ok(ip) = first.trim().parse::<IpAddr>()
    {
        return Some(ip);
    }

    // X-Real-Ip (set by nginx)
    if let Some(xri) = req.headers().get("x-real-ip")
        && let Ok(s) = xri.to_str()
        && let Ok(ip) = s.trim().parse::<IpAddr>()
    {
        return Some(ip);
    }

    // SocketAddr extension set by Axum's ConnectInfo
    req.extensions()
        .get::<std::net::SocketAddr>()
        .map(std::net::SocketAddr::ip)
}

/// Build a 503 response with the appropriate content type.
#[allow(clippy::expect_used, reason = "infallible: static 503 response")]
fn build_503_response<B>(req: &Request<B>, config: &MaintenanceConfig) -> Response<Body> {
    let message = config
        .message
        .as_deref()
        .unwrap_or("The service is temporarily unavailable. Please try again later.");

    let wants_json = req
        .headers()
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            accept.contains("application/json") || accept.contains("application/problem+json")
        });

    if wants_json {
        let body = serde_json::json!({
            "type": "about:blank",
            "title": "Service Unavailable",
            "status": 503,
            "detail": message,
        });
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("Retry-After", RETRY_AFTER_SECS)
            .header(CONTENT_TYPE, "application/problem+json")
            .body(Body::from(body.to_string()))
            .expect("valid 503 JSON response")
    } else {
        let html = format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\">\
             <title>503 Service Unavailable</title>\
             <style>body{{font-family:sans-serif;max-width:600px;margin:4rem auto;padding:0 1rem}}\
             h1{{color:#c0392b}}</style></head>\
             <body><h1>Service Temporarily Unavailable</h1>\
             <p>{message}</p>\
             <p>Please try again later.</p></body></html>"
        );
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("Retry-After", RETRY_AFTER_SECS)
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(html))
            .expect("valid 503 HTML response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintenance::MaintenanceConfig;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt; // for oneshot

    fn make_app(state: MaintenanceState) -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .route("/api/data", get(|| async { "data" }))
            .route("/actuator/health", get(|| async { "healthy" }))
            .layer(MaintenanceLayer::new(state))
    }

    async fn response_status(app: Router, uri: &str) -> StatusCode {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    // ── Maintenance off ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_off_passes_through() {
        let state = MaintenanceState::new();
        let app = make_app(state);
        assert_eq!(response_status(app, "/").await, StatusCode::OK);
    }

    // ── Maintenance on — basic 503 ────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_on_returns_503() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        let app = make_app(state);
        assert_eq!(
            response_status(app, "/").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn maintenance_on_includes_retry_after_header() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        let app = make_app(state);

        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(resp.headers().contains_key("retry-after"));
    }

    // ── Content negotiation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_on_html_response_for_browser() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(ct.contains("text/html"), "expected text/html, got {ct}");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("Service Temporarily Unavailable"),
            "body: {html}"
        );
    }

    #[tokio::test]
    async fn maintenance_on_json_response_for_api_client() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/data")
                    .header(ACCEPT, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(
            ct.contains("application/problem+json"),
            "expected problem+json, got {ct}"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 503);
        assert_eq!(json["title"], "Service Unavailable");
        assert!(json["detail"].is_string(), "detail should be a string");
    }

    #[tokio::test]
    async fn maintenance_on_problem_json_for_problem_json_accept() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/data")
                    .header(ACCEPT, "application/problem+json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(ct.contains("application/problem+json"), "got {ct}");
    }

    #[tokio::test]
    async fn maintenance_on_custom_message_in_body() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            message: Some("Deploying v2.0".into()),
            ..Default::default()
        });
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("Deploying v2.0"),
            "custom message absent: {html}"
        );
    }

    // ── Actuator / health bypass ──────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_on_actuator_path_passes_through() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        let app = make_app(state);
        assert_eq!(
            response_status(app, "/actuator/health").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn maintenance_on_custom_health_prefix_passes_through() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());

        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/", get(|| async { "root" }))
            .layer(MaintenanceLayer::new(state).with_health_prefix("/health"));

        assert_eq!(
            response_status(app.clone(), "/health").await,
            StatusCode::OK
        );
        assert_eq!(
            response_status(app, "/").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ── Bypass header ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_on_bypass_header_passes_through() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            bypass_header: Some(("X-Autumn-Maintenance-Bypass".into(), "my-secret".into())),
            ..Default::default()
        });
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("X-Autumn-Maintenance-Bypass", "my-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn maintenance_on_wrong_bypass_header_value_blocked() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            bypass_header: Some(("X-Autumn-Maintenance-Bypass".into(), "my-secret".into())),
            ..Default::default()
        });
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("X-Autumn-Maintenance-Bypass", "wrong-value")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn maintenance_on_missing_bypass_header_blocked() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            bypass_header: Some(("X-Autumn-Maintenance-Bypass".into(), "my-secret".into())),
            ..Default::default()
        });
        let app = make_app(state);
        assert_eq!(
            response_status(app, "/").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ── IP allow-list bypass ──────────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_on_allowed_ip_passes_through() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            allow_ips: vec!["127.0.0.1".into()],
            ..Default::default()
        });
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("X-Forwarded-For", "127.0.0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn maintenance_on_disallowed_ip_blocked() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            allow_ips: vec!["10.0.0.0/8".into()],
            ..Default::default()
        });
        let app = make_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("X-Forwarded-For", "192.168.1.5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// When `ResolvedClientIdentity` is stamped by `TrustedProxiesLayer`, its
    /// resolved addr is used for allow-list checks — a spoofed X-Forwarded-For
    /// header cannot bypass maintenance mode by claiming an allowed IP.
    #[tokio::test]
    async fn maintenance_resolved_identity_takes_precedence_over_raw_xff() {
        use crate::security::ResolvedClientIdentity;
        use std::net::IpAddr;

        let state = MaintenanceState::new();
        // Only 10.x.x.x is allowed.
        state.enable(MaintenanceConfig {
            allow_ips: vec!["10.0.0.0/8".into()],
            ..Default::default()
        });

        // The resolved identity (from TrustedProxiesLayer) says the real client
        // is 192.168.1.5 — not in the allow list.
        let real_ip: IpAddr = "192.168.1.5".parse().unwrap();
        let mut req = Request::builder()
            .uri("/")
            // Attacker forges an allowed IP in XFF.
            .header("X-Forwarded-For", "10.0.0.1")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ResolvedClientIdentity {
            addr: Some(real_ip),
            host: None,
            scheme: None,
        });

        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(MaintenanceLayer::new(state));

        let resp = app.oneshot(req).await.unwrap();
        // Must be blocked: resolved IP (192.168.1.5) is not in the allow list.
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── Read-only mode ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_readonly_get_passes_through() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            readonly: true,
            ..Default::default()
        });
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(MaintenanceLayer::new(state));

        assert_eq!(response_status(app, "/").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn maintenance_readonly_post_returns_503() {
        use axum::routing::post;
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            readonly: true,
            ..Default::default()
        });
        let app = Router::new()
            .route("/submit", post(|| async { "ok" }))
            .layer(MaintenanceLayer::new(state));

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/submit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn maintenance_readonly_head_passes_through() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig {
            readonly: true,
            ..Default::default()
        });
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(MaintenanceLayer::new(state));

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::HEAD)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // HEAD returns 200 (no body)
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── Layer behaviour ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn maintenance_layer_clone_shares_state() {
        let state = MaintenanceState::new();
        let layer = MaintenanceLayer::new(state.clone());

        // The cloned layer wraps the same underlying Arc, so enabling
        // maintenance through `state` is visible through the clone.
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(layer.clone());

        state.enable(MaintenanceConfig::default());
        assert_eq!(
            response_status(app, "/").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn maintenance_on_root_health_prefix_passes_only_root() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());

        let app = Router::new()
            .route("/", get(|| async { "root" }))
            .route("/api/data", get(|| async { "data" }))
            .layer(MaintenanceLayer::new(state).with_health_prefix("/"));

        assert_eq!(response_status(app.clone(), "/").await, StatusCode::OK);
        assert_eq!(
            response_status(app, "/api/data").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn maintenance_on_custom_health_prefix_segment_aware() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());

        let app = Router::new()
            .route("/actuator/health", get(|| async { "healthy" }))
            .route("/actuator-dashboard", get(|| async { "dashboard" }))
            .layer(MaintenanceLayer::new(state).with_health_prefix("/actuator"));

        assert_eq!(
            response_status(app.clone(), "/actuator/health").await,
            StatusCode::OK
        );
        assert_eq!(
            response_status(app, "/actuator-dashboard").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
