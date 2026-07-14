//! Security response headers middleware.
//!
//! Applies OWASP-recommended security headers to every HTTP response.
//! The headers are configured via [`HeadersConfig`]
//! in `autumn.toml` under `[security.headers]`.
//!
//! # Headers applied
//!
//! | Header | When | Default |
//! |--------|------|---------|
//! | `X-Frame-Options` | `x_frame_options` is non-empty | `DENY` |
//! | `X-Content-Type-Options` | `x_content_type_options` is `true` | `nosniff` |
//! | `X-XSS-Protection` | `xss_protection` is `true` | `1; mode=block` |
//! | `Strict-Transport-Security` | `strict_transport_security` is `true` | `max-age=31536000; includeSubDomains` |
//! | `Content-Security-Policy` | `content_security_policy` is non-empty | htmx-compatible same-origin policy |
//! | `Referrer-Policy` | `referrer_policy` is non-empty | `strict-origin-when-cross-origin` |
//! | `Permissions-Policy` | `permissions_policy` is non-empty | not sent |
//!
//! # Examples
//!
//! The layer is applied automatically by [`AppBuilder::run`](crate::app::AppBuilder::run).
//! For custom Axum routers:
//!
//! ```rust,no_run
//! use autumn_web::security::SecurityHeadersLayer;
//! use autumn_web::security::HeadersConfig;
//!
//! let config = HeadersConfig::default();
//! let app = axum::Router::<()>::new()
//!     .route("/", axum::routing::get(|| async { "ok" }))
//!     .layer(SecurityHeadersLayer::from_config(&config));
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::{HeaderValue, Request, Response, StatusCode};
use http::header::HeaderName;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use super::config::{HeadersConfig, default_content_security_policy};

/// Placeholder token embedded in the nonce-aware CSP template.
///
/// At request time every occurrence is replaced with the generated nonce value.
const NONCE_PLACEHOLDER: &str = "AUTUMN_CSP_NONCE";

/// Per-request Content Security Policy nonce.
///
/// When `security.headers.csp_nonce.enabled = true` in `autumn.toml`, the
/// [`SecurityHeadersLayer`] generates a fresh 128-bit URL-safe-base64 nonce
/// for every request, stores it in request extensions, and injects it into
/// the `Content-Security-Policy` response header.
///
/// Extract the nonce in a handler to embed it in inline `<script>` and
/// `<style>` tags:
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::security::CspNonce;
///
/// #[get("/page")]
/// async fn page(nonce: CspNonce) -> Markup {
///     html! {
///         script nonce=(nonce.value()) { "console.log('hello')" }
///         style  nonce=(nonce.value()) { "body { margin: 0 }" }
///     }
/// }
/// ```
///
/// Use `Option<CspNonce>` when the layer may or may not be active:
///
/// ```rust,ignore
/// async fn page(nonce: Option<CspNonce>) -> Markup {
///     let nonce_attr = nonce.as_ref().map(|n| n.value()).unwrap_or("");
///     html! { script nonce=(nonce_attr) { "// ..." } }
/// }
/// ```
#[derive(Clone, Debug)]
pub struct CspNonce(String);

impl CspNonce {
    /// Returns the raw nonce string for embedding in HTML attributes.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.0
    }

    /// Construct a nonce with a known value for unit tests elsewhere in the
    /// crate (e.g. the story gallery's nonce-threading tests); production
    /// nonces are only ever minted by [`SecurityHeadersLayer`].
    #[cfg(test)]
    pub(crate) fn new_for_tests(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl<S> FromRequestParts<S> for CspNonce
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "CSP nonce not found in request extensions. Is CspNonce enabled in security.headers.csp_nonce?",
        ))
    }
}

impl<S> OptionalFromRequestParts<S> for CspNonce
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

/// Generate a cryptographically-random 128-bit URL-safe base64 nonce.
fn generate_nonce() -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("getrandom must not fail on supported platforms");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the nonce-aware default CSP template.
///
/// Replaces `'unsafe-inline'` in `style-src` with `'nonce-AUTUMN_CSP_NONCE'`
/// and appends `'nonce-AUTUMN_CSP_NONCE'` to `script-src`.
fn nonce_aware_default_csp() -> String {
    "default-src 'self'; \
     img-src 'self' data:; \
     style-src 'self' 'nonce-AUTUMN_CSP_NONCE'; \
     script-src 'self' 'nonce-AUTUMN_CSP_NONCE'; \
     connect-src 'self'; \
     form-action 'self'; \
     frame-ancestors 'none'; \
     base-uri 'self'"
        .to_owned()
}

/// Resolve the effective `Content-Security-Policy` string that responses carry,
/// mirroring the resolution in [`ComputedHeaders::from_config`].
///
/// When per-request nonce injection is active (nonce enabled AND the CSP is the
/// framework default), this returns the nonce-aware template containing
/// [`NONCE_PLACEHOLDER`] tokens — i.e. what responses send *before* the
/// per-request nonce is substituted in. Otherwise it returns the configured CSP
/// verbatim.
///
/// The security-posture manifest reports this value (placeholder intact, not a
/// live nonce) so the declared CSP matches what nonce-enabled apps emit while
/// staying byte-deterministic across runs.
pub fn resolved_content_security_policy(config: &HeadersConfig) -> String {
    if config.csp_nonce.enabled
        && config.content_security_policy == default_content_security_policy()
    {
        nonce_aware_default_csp()
    } else {
        config.content_security_policy.clone()
    }
}

/// Pre-computed header pairs to inject into every response.
///
/// Created once from [`HeadersConfig`] and shared via `Arc` across
/// all clones of [`SecurityHeadersService`].
///
/// When `csp_nonce` is enabled and the default CSP is in use, `nonce_csp_template`
/// holds the CSP string with [`NONCE_PLACEHOLDER`] tokens. The CSP header is
/// NOT in `static_pairs`; it is built per-request by replacing the placeholder
/// with the generated nonce value.
#[derive(Debug, Clone)]
struct ComputedHeaders {
    /// Headers applied to every response (everything except the dynamic CSP
    /// when nonce injection is active).
    static_pairs: Vec<(HeaderName, HeaderValue)>,
    /// CSP template containing `AUTUMN_CSP_NONCE` placeholder tokens.
    /// `None` when nonce injection is disabled or the user has set a custom CSP.
    nonce_csp_template: Option<Arc<str>>,
    csp_nonce_enabled: bool,
}

impl ComputedHeaders {
    fn from_config(config: &HeadersConfig) -> Self {
        let mut static_pairs = Vec::with_capacity(8);

        if !config.x_frame_options.is_empty()
            && let Ok(val) = HeaderValue::from_str(&config.x_frame_options)
        {
            static_pairs.push((HeaderName::from_static("x-frame-options"), val));
        }

        if config.x_content_type_options {
            static_pairs.push((
                HeaderName::from_static("x-content-type-options"),
                HeaderValue::from_static("nosniff"),
            ));
        }

        if config.xss_protection {
            static_pairs.push((
                HeaderName::from_static("x-xss-protection"),
                HeaderValue::from_static("1; mode=block"),
            ));
        }

        if config.strict_transport_security {
            let mut hsts = format!("max-age={}", config.hsts_max_age_secs);
            if config.hsts_include_subdomains {
                hsts.push_str("; includeSubDomains");
            }
            if let Ok(val) = HeaderValue::from_str(&hsts) {
                static_pairs.push((HeaderName::from_static("strict-transport-security"), val));
            }
        }

        // Determine whether to use per-request nonce injection for CSP.
        //
        // Nonce injection is active when:
        //   1. `csp_nonce.enabled = true`, AND
        //   2. The CSP string is the framework default (not user-overridden).
        //
        // Apps that set an explicit `content_security_policy` opt out automatically:
        // their custom value is used verbatim and the nonce is still generated
        // (for the extractor) but not written into the header.
        let using_default_csp = config.content_security_policy == default_content_security_policy();
        // Single source of truth for the resolved CSP string (nonce-aware
        // template when injection is active, configured value otherwise); the
        // manifest dump reuses this same resolution.
        let resolved_csp = resolved_content_security_policy(config);
        let nonce_csp_template = if config.csp_nonce.enabled && using_default_csp {
            Some(Arc::from(resolved_csp.as_str()))
        } else {
            // Static CSP (either custom, or nonce disabled).
            if !resolved_csp.is_empty()
                && let Ok(val) = HeaderValue::from_str(&resolved_csp)
            {
                static_pairs.push((HeaderName::from_static("content-security-policy"), val));
            }
            None
        };

        if !config.referrer_policy.is_empty()
            && let Ok(val) = HeaderValue::from_str(&config.referrer_policy)
        {
            static_pairs.push((HeaderName::from_static("referrer-policy"), val));
        }

        if !config.permissions_policy.is_empty()
            && let Ok(val) = HeaderValue::from_str(&config.permissions_policy)
        {
            static_pairs.push((HeaderName::from_static("permissions-policy"), val));
        }

        Self {
            static_pairs,
            nonce_csp_template,
            csp_nonce_enabled: config.csp_nonce.enabled,
        }
    }
}

/// Tower [`Layer`] that adds security headers to every response.
///
/// Created from a [`HeadersConfig`] and applied to the Axum router.
/// The headers are pre-computed once and shared across all clones.
#[derive(Clone, Debug)]
pub struct SecurityHeadersLayer {
    headers: Arc<ComputedHeaders>,
}

impl SecurityHeadersLayer {
    /// Create a new layer from the given headers configuration.
    #[must_use]
    pub fn from_config(config: &HeadersConfig) -> Self {
        Self {
            headers: Arc::new(ComputedHeaders::from_config(config)),
        }
    }
}

impl<S> Layer<S> for SecurityHeadersLayer {
    type Service = SecurityHeadersService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SecurityHeadersService {
            inner,
            headers: Arc::clone(&self.headers),
        }
    }
}

/// Tower [`Service`] produced by [`SecurityHeadersLayer`].
///
/// Adds pre-computed security headers to every HTTP response.
#[derive(Clone, Debug)]
pub struct SecurityHeadersService<S> {
    inner: S,
    headers: Arc<ComputedHeaders>,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for SecurityHeadersService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = SecurityHeadersFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        // When nonce injection is active: generate a nonce, insert it into
        // request extensions (so handlers can extract it), and pass it along
        // to the response future where it will be substituted into the CSP.
        let nonce = if self.headers.csp_nonce_enabled {
            let n = generate_nonce();
            req.extensions_mut().insert(CspNonce(n.clone()));
            Some(n)
        } else {
            None
        };
        SecurityHeadersFuture {
            inner: self.inner.call(req),
            headers: Some(Arc::clone(&self.headers)),
            nonce,
        }
    }
}

pin_project! {
    /// Future that injects security headers into the response.
    pub struct SecurityHeadersFuture<F> {
        #[pin]
        inner: F,
        headers: Option<Arc<ComputedHeaders>>,
        nonce: Option<String>,
    }
}

impl<F, ResBody, E> Future for SecurityHeadersFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(Ok(mut response)) => {
                if let Some(computed) = this.headers.take() {
                    let resp_headers = response.headers_mut();
                    for (name, value) in &computed.static_pairs {
                        resp_headers.insert(name.clone(), value.clone());
                    }
                    // Inject the per-request nonce into the CSP template.
                    if let (Some(nonce), Some(template)) =
                        (this.nonce.take(), &computed.nonce_csp_template)
                    {
                        let csp_value = template.replace(NONCE_PLACEHOLDER, &nonce);
                        if let Ok(val) = HeaderValue::from_str(&csp_value) {
                            resp_headers
                                .insert(HeaderName::from_static("content-security-policy"), val);
                        }
                    }
                }
                Poll::Ready(Ok(response))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use std::task::{Context, Poll};
    use tower::{Layer, Service, ServiceExt};

    #[tokio::test]
    async fn default_headers_applied() {
        let config = HeadersConfig::default();
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert_eq!(
            response.headers().get("x-xss-protection").unwrap(),
            "1; mode=block"
        );
        assert_eq!(
            response.headers().get("referrer-policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
        // HSTS not present by default
        assert!(
            response
                .headers()
                .get("strict-transport-security")
                .is_none()
        );
        // CSP present by default with htmx-compatible policy
        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("default CSP should be emitted");
        let csp = csp.to_str().unwrap();
        assert!(csp.contains("default-src 'self'"), "csp = {csp}");
        assert!(csp.contains("script-src 'self'"), "csp = {csp}");
        assert!(csp.contains("img-src 'self' data:"), "csp = {csp}");
        assert!(!csp.contains("'unsafe-eval'"), "csp = {csp}");
    }

    #[tokio::test]
    async fn default_csp_allows_htmx_to_function() {
        // htmx and Autumn's htmx CSRF helper are served from /static/js/
        // (same origin), operate via addEventListener, and issue hx-* requests
        // to the same origin. The default CSP must allow all of that without
        // requiring inline scripts.
        let config = HeadersConfig::default();
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("default CSP missing")
            .to_str()
            .unwrap()
            .to_owned();

        // htmx script loads from same origin
        assert!(
            csp.contains("script-src 'self'"),
            "csp must allow same-origin scripts for htmx: {csp}"
        );
        // htmx XHR/fetch requests go to same origin
        assert!(
            csp.contains("connect-src 'self'"),
            "csp must allow same-origin connects for htmx swaps: {csp}"
        );
        // No eval required for htmx standard operation
        assert!(
            !csp.contains("'unsafe-eval'"),
            "default csp should not weaken script-src with unsafe-eval: {csp}"
        );
    }

    #[tokio::test]
    async fn hsts_header_when_enabled() {
        let config = HeadersConfig {
            strict_transport_security: true,
            hsts_max_age_secs: 86400,
            hsts_include_subdomains: true,
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.headers().get("strict-transport-security").unwrap(),
            "max-age=86400; includeSubDomains"
        );
    }

    #[tokio::test]
    async fn csp_header_when_configured() {
        let config = HeadersConfig {
            content_security_policy: "default-src 'self'".to_owned(),
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.headers().get("content-security-policy").unwrap(),
            "default-src 'self'"
        );
    }

    #[tokio::test]
    async fn empty_x_frame_options_not_sent() {
        let config = HeadersConfig {
            x_frame_options: String::new(),
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(response.headers().get("x-frame-options").is_none());
    }

    #[tokio::test]
    async fn empty_csp_not_sent() {
        let config = HeadersConfig {
            content_security_policy: String::new(),
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(response.headers().get("content-security-policy").is_none());
    }

    #[tokio::test]
    async fn empty_referrer_policy_not_sent() {
        let config = HeadersConfig {
            referrer_policy: String::new(),
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(response.headers().get("referrer-policy").is_none());
    }

    #[tokio::test]
    async fn empty_permissions_policy_not_sent() {
        let config = HeadersConfig {
            permissions_policy: String::new(),
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(response.headers().get("permissions-policy").is_none());
    }

    #[tokio::test]
    async fn referrer_policy_when_configured() {
        let config = HeadersConfig {
            referrer_policy: "strict-origin-when-cross-origin".to_owned(),
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.headers().get("referrer-policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
    }

    #[tokio::test]
    async fn permissions_policy_when_configured() {
        let config = HeadersConfig {
            permissions_policy: "camera=(), microphone=()".to_owned(),
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.headers().get("permissions-policy").unwrap(),
            "camera=(), microphone=()"
        );
    }

    #[tokio::test]
    async fn hsts_without_subdomains() {
        let config = HeadersConfig {
            strict_transport_security: true,
            hsts_max_age_secs: 3600,
            hsts_include_subdomains: false,
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response.headers().get("strict-transport-security").unwrap(),
            "max-age=3600"
        );
    }

    #[derive(Clone)]
    struct PendingService;

    impl<ReqBody> Service<Request<ReqBody>> for PendingService {
        type Response = axum::response::Response;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn call(&mut self, _req: Request<ReqBody>) -> Self::Future {
            unreachable!("poll_ready should block this")
        }
    }

    #[test]
    fn layer_poll_ready_passes_through() {
        let layer = SecurityHeadersLayer::from_config(&HeadersConfig::default());
        let mut service = layer.layer(PendingService);

        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        let poll = Service::<Request<axum::body::Body>>::poll_ready(&mut service, &mut cx);

        assert!(
            poll.is_pending(),
            "poll_ready should pass through Pending from inner service"
        );
    }

    // ── CSP nonce tests (RED phase) ───────────────────────────────────────────

    use super::super::config::CspNonceConfig;

    #[tokio::test]
    async fn nonce_injected_into_csp_script_src_when_enabled() {
        let config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("CSP header must be present")
            .to_str()
            .unwrap();

        assert!(
            csp.contains("script-src 'self' 'nonce-"),
            "script-src must contain nonce directive: {csp}"
        );
    }

    #[tokio::test]
    async fn nonce_injected_into_csp_style_src_when_enabled() {
        let config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("CSP header must be present")
            .to_str()
            .unwrap();

        assert!(
            csp.contains("style-src 'self' 'nonce-"),
            "style-src must contain nonce directive: {csp}"
        );
    }

    #[tokio::test]
    async fn nonce_csp_removes_unsafe_inline_from_style_src() {
        let config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap();

        assert!(
            !csp.contains("'unsafe-inline'"),
            "CSP with nonce must not contain 'unsafe-inline': {csp}"
        );
    }

    #[tokio::test]
    async fn nonce_value_differs_between_requests() {
        let config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let layer = SecurityHeadersLayer::from_config(&config);

        let make_app = || {
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(layer.clone())
        };

        let r1 = make_app()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let r2 = make_app()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp1 = r1
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let csp2 = r2
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        assert_ne!(csp1, csp2, "Each request must receive a unique nonce");
    }

    #[tokio::test]
    async fn explicit_csp_not_modified_when_nonce_enabled() {
        let config = HeadersConfig {
            content_security_policy: "default-src 'none'".to_owned(),
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            response
                .headers()
                .get("content-security-policy")
                .unwrap()
                .to_str()
                .unwrap(),
            "default-src 'none'",
            "Explicit CSP string must be used verbatim (no nonce injection)"
        );
    }

    #[tokio::test]
    async fn csp_nonce_extractor_returns_nonempty_value() {
        async fn handler(nonce: CspNonce) -> String {
            nonce.value().to_owned()
        }

        let config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(handler))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let (_, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let nonce_value = std::str::from_utf8(&body_bytes).unwrap();

        assert!(!nonce_value.is_empty(), "Extracted nonce must be non-empty");
    }

    #[tokio::test]
    async fn csp_nonce_extractor_value_matches_csp_header_nonce() {
        async fn handler(nonce: CspNonce) -> String {
            nonce.value().to_owned()
        }

        let config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(handler))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let (parts, body) = response.into_parts();
        let csp = parts
            .headers
            .get("content-security-policy")
            .expect("CSP header missing")
            .to_str()
            .unwrap()
            .to_owned();

        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let nonce_value = std::str::from_utf8(&body_bytes).unwrap();

        assert!(
            csp.contains(nonce_value),
            "CSP header must contain the same nonce as the extractor: nonce={nonce_value}, csp={csp}"
        );
    }

    #[tokio::test]
    async fn csp_nonce_is_128_bit_url_safe_base64() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        async fn handler(nonce: CspNonce) -> String {
            nonce.value().to_owned()
        }

        let config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };
        let app = Router::new()
            .route("/", get(handler))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let (_, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let nonce_value = std::str::from_utf8(&body_bytes).unwrap();
        let decoded = URL_SAFE_NO_PAD
            .decode(nonce_value)
            .unwrap_or_else(|_| panic!("nonce must be URL-safe base64, got: {nonce_value}"));

        assert!(
            decoded.len() >= 16,
            "nonce must be ≥128 bits (16 bytes), got {} bytes",
            decoded.len()
        );
    }

    #[tokio::test]
    async fn optional_nonce_extractor_none_when_disabled() {
        async fn handler(nonce: Option<CspNonce>) -> String {
            nonce.map(|n| n.value().to_owned()).unwrap_or_default()
        }

        let config = HeadersConfig::default(); // csp_nonce.enabled = false
        let app = Router::new()
            .route("/", get(handler))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let (_, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let value = std::str::from_utf8(&body_bytes).unwrap();

        assert!(
            value.is_empty(),
            "Optional nonce extractor must return None when nonce is disabled"
        );
    }

    #[tokio::test]
    async fn nonce_disabled_uses_static_csp_with_unsafe_inline() {
        let config = HeadersConfig::default(); // csp_nonce disabled

        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("CSP header must be present when disabled")
            .to_str()
            .unwrap();

        assert!(
            csp.contains("'unsafe-inline'"),
            "Default CSP without nonce should still have unsafe-inline in style-src: {csp}"
        );
        assert!(
            !csp.contains("'nonce-"),
            "Default CSP without nonce must not contain nonce directive: {csp}"
        );
    }

    #[tokio::test]
    async fn custom_csp_with_nonce_enabled_generates_nonce_for_extractor() {
        use super::super::config::CspNonceConfig;

        async fn handler(nonce: CspNonce) -> String {
            nonce.value().to_owned()
        }

        let config = HeadersConfig {
            content_security_policy: "default-src 'self'; script-src 'self'".to_string(),
            csp_nonce: CspNonceConfig { enabled: true },
            ..Default::default()
        };

        let app = Router::new()
            .route("/", get(handler))
            .layer(SecurityHeadersLayer::from_config(&config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        // 1. Verify custom CSP header is sent unmodified
        let csp = response
            .headers()
            .get("content-security-policy")
            .expect("CSP header must be present")
            .to_str()
            .unwrap();
        assert_eq!(csp, "default-src 'self'; script-src 'self'");

        // 2. Verify CspNonce extractor gets a valid nonce
        let (_, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let nonce_val = std::str::from_utf8(&body_bytes).unwrap();

        assert!(!nonce_val.is_empty(), "Nonce must not be empty");
        assert_eq!(
            nonce_val.len(),
            22,
            "Nonce must be a 22-character base64 string"
        );
    }
}
