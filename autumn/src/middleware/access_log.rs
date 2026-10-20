//! Structured per-request access log (#999).
//!
//! Emits exactly one `tracing` event (target [`ACCESS_LOG_TARGET`], level
//! `INFO`) at the point each response is returned, carrying the HTTP method,
//! the matched low-cardinality route template (never the raw path), the
//! response status code, the request duration in milliseconds, and the
//! request's `request_id` (read from the `x-request-id` response header
//! stamped by [`RequestIdLayer`](crate::middleware::RequestIdLayer), so it
//! matches error pages and the header clients see).
//!
//! The event flows through the already-installed subscriber, so it honors
//! `LogConfig.format` (`pretty` / `json`) and works with **no** telemetry
//! feature and no OTLP collector. It never includes query strings, headers, or
//! bodies, preserving the log scrubbing posture established for logs (#697) by
//! construction.
//!
//! Access logging is on by default (`log.access_log = true`; overridable via
//! `AUTUMN_LOG__ACCESS_LOG`). Steady-state probe and asset noise is excluded
//! via `log.access_log_exclude` (default: `/health`, `/live`, `/ready`,
//! `/startup`, `/actuator`, `/static`), matched on whole path segments so
//! `/healthz` is still logged while `/actuator/health` is not.
//!
//! Applied automatically by the framework router in **two** positions:
//!
//! - The **primary** layer ([`AccessLogLayer::new`]) sits inner to
//!   [`RequestIdLayer`](crate::middleware::RequestIdLayer) and the log-context
//!   layer, so the event is emitted inside the request span with the request
//!   id taken from the request extension. Once it emits, it flips the
//!   [`AccessLogEmitted`] sentinel the fallback planted in the request
//!   extensions, keeping the fallback silent.
//! - A **fallback** layer ([`AccessLogLayer::fallback`]) sits at the outermost
//!   router assembly point — outside the startup barrier, the static-first
//!   (SSG/ISR) middleware, the session layer, and the late MCP endpoint
//!   merge — and emits only for responses the primary never saw: startup
//!   503s, pre-built static page hits, session-store outage 503s, and MCP
//!   endpoint requests. Those short-circuits never ran `RequestIdLayer`, so
//!   the fallback reads `request_id` from the `x-request-id` response header
//!   when present and omits it otherwise.
//!
//! Known accepted tradeoff of the in-context primary placement: a custom
//! exception filter (or a session-store save failure) that *rewrites* an
//! already-produced response does so after the line was emitted, so the
//! logged `status` reflects the handler's response in that narrow case. The
//! built-in filters preserve status.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
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
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::extract::MatchedPath;
use axum::http::{HeaderName, Method, Request, Response};
use pin_project_lite::pin_project;
use tower::{Layer, Service};

static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// `tracing` target carried by every access-log event, e.g. for filtering
/// (`autumn::access=off`) or routing in a custom layer.
pub const ACCESS_LOG_TARGET: &str = "autumn::access";

/// Route label used when no route matched the request (e.g. 404s), keeping the
/// `route` field low-cardinality. Mirrors the metrics layer.
pub const UNMATCHED_ROUTE: &str = "_unmatched";

/// The route pattern a response carries when the request had no `MatchedPath`.
///
/// A plugin mounted at runtime is routed from the 404 fallback. That happens
/// after the access log and metrics layers read the request. The plugin sets
/// this on its response, and those layers use it when the request has no route.
#[derive(Debug, Clone)]
pub struct ResponseRoute(pub String);

/// Shared sentinel coordinating the primary and fallback access-log layers.
///
/// The fallback inserts it into the **request** extensions on the way in, and
/// the primary flips it once it emits, so the fallback only logs requests
/// that short-circuited before reaching the primary. It rides the request
/// (not the response) because error-page/exception filters may rebuild the
/// response entirely, which would drop a response-side marker.
#[derive(Clone, Debug, Default)]
pub struct AccessLogEmitted(Arc<std::sync::atomic::AtomicBool>);

impl AccessLogEmitted {
    fn mark(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    fn is_marked(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Tower [`Layer`] that emits one structured access-log event per served
/// request. Applied automatically by the framework router when
/// `log.access_log` is enabled (the default).
#[derive(Clone, Debug)]
pub struct AccessLogLayer {
    exclude: Arc<[String]>,
    fallback: bool,
}

impl AccessLogLayer {
    /// Create the primary layer. Requests whose path falls under one of the
    /// `exclude` prefixes (whole-segment match) are not logged. Emitted
    /// responses are stamped with [`AccessLogEmitted`].
    #[must_use]
    pub fn new(exclude: Vec<String>) -> Self {
        Self {
            exclude: normalize_exclusions(exclude),
            fallback: false,
        }
    }

    /// Create the outermost fallback layer: it emits only for responses that
    /// do **not** carry the [`AccessLogEmitted`] marker — i.e. requests that
    /// short-circuited before the primary layer ran.
    #[must_use]
    pub fn fallback(exclude: Vec<String>) -> Self {
        Self {
            exclude: normalize_exclusions(exclude),
            fallback: true,
        }
    }
}

/// Normalize configured exclusion prefixes once at construction so the
/// per-request check is a plain prefix match: trailing slashes are stripped
/// and empty / slash-only entries are dropped.
fn normalize_exclusions(exclude: Vec<String>) -> Arc<[String]> {
    exclude
        .into_iter()
        .map(|prefix| {
            let trimmed = prefix.trim_end_matches('/');
            if trimmed.len() == prefix.len() {
                prefix
            } else {
                trimmed.to_owned()
            }
        })
        .filter(|prefix| !prefix.is_empty())
        .collect()
}

impl<S> Layer<S> for AccessLogLayer {
    type Service = AccessLogService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AccessLogService {
            inner,
            exclude: Arc::clone(&self.exclude),
            fallback: self.fallback,
        }
    }
}

/// Tower [`Service`] produced by [`AccessLogLayer`].
#[derive(Clone, Debug)]
pub struct AccessLogService<S> {
    inner: S,
    exclude: Arc<[String]>,
    fallback: bool,
}

/// Request facts captured before the inner service consumes the request.
/// `None` when the request path is excluded from access logging.
///
/// The route and id are kept as the shared values the request already
/// carries, not copied to text: `MatchedPath` is an `Arc<str>` (a clone is a
/// refcount bump) and `RequestId` is rendered on the stack at emit time. The
/// fallback layer captures these on every request and almost never emits.
struct RequestMeta {
    method: Method,
    route: Option<MatchedPath>,
    request_id: Option<crate::middleware::RequestId>,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for AccessLogService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = AccessLogFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let mut req = req;
        let meta = if is_excluded(req.uri().path(), &self.exclude) {
            None
        } else {
            Some(RequestMeta {
                method: req.method().clone(),
                route: req.extensions().get::<MatchedPath>().cloned(),
                // Set by RequestIdLayer, which sits outer to the primary
                // layer. Absent at the outermost fallback position.
                request_id: req
                    .extensions()
                    .get::<crate::middleware::RequestId>()
                    .cloned(),
            })
        };

        // The fallback plants the sentinel for the primary to flip; the
        // primary picks it up when present (it is absent in bare setups
        // that apply only the primary layer).
        let sentinel = if self.fallback {
            let sentinel = AccessLogEmitted::default();
            req.extensions_mut().insert(sentinel.clone());
            Some(sentinel)
        } else {
            req.extensions().get::<AccessLogEmitted>().cloned()
        };

        AccessLogFuture {
            inner: self.inner.call(req),
            meta,
            start: crate::time::ambient_instant(),
            fallback: self.fallback,
            sentinel,
        }
    }
}

/// Returns `true` when `path` equals an exclusion prefix or lives under it as
/// a whole path segment (`/actuator` excludes `/actuator/health`, not
/// `/actuators`). Prefixes are pre-normalized by [`normalize_exclusions`].
fn is_excluded(path: &str, exclude: &[String]) -> bool {
    exclude.iter().any(|prefix| {
        path.strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

pin_project! {
    /// Future that emits the access-log event once the inner service produces
    /// its response.
    pub struct AccessLogFuture<F> {
        #[pin]
        inner: F,
        meta: Option<RequestMeta>,
        start: Instant,
        fallback: bool,
        sentinel: Option<AccessLogEmitted>,
    }
}

impl<F, ResBody, E> Future for AccessLogFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(Ok(response)) => {
                let already_emitted = *this.fallback
                    && this
                        .sentinel
                        .as_ref()
                        .is_some_and(AccessLogEmitted::is_marked);
                if let Some(meta) = this.meta.take()
                    && !already_emitted
                {
                    let duration_ms = crate::time::ambient_instant()
                        .saturating_duration_since(*this.start)
                        .as_secs_f64()
                        * 1000.0;
                    // The primary layer reads the id from the request
                    // extension; the fallback covers short-circuits that may
                    // never have run RequestIdLayer, so it falls back to the
                    // `x-request-id` response header and omits the field when
                    // neither is present.
                    let header_id = response
                        .headers()
                        .get(&X_REQUEST_ID)
                        .and_then(|value| value.to_str().ok());
                    // Rendered on the stack, as `RequestIdLayer` renders the
                    // header. The field stays a `&str`, so it is recorded with
                    // `record_str` (quoted in text output) exactly as before.
                    let mut id_buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
                    let request_id = meta
                        .request_id
                        .as_ref()
                        .map(|id| id.encode(&mut id_buf))
                        .or(header_id);
                    tracing::info!(
                        target: ACCESS_LOG_TARGET,
                        method = %meta.method,
                        route = meta
                            .route
                            .as_ref()
                            .map(MatchedPath::as_str)
                            .or_else(|| {
                                response
                                    .extensions()
                                    .get::<ResponseRoute>()
                                    .map(|route| route.0.as_str())
                            })
                            .unwrap_or(UNMATCHED_ROUTE),
                        status = response.status().as_u16(),
                        duration_ms,
                        request_id,
                        "request served"
                    );
                    if !*this.fallback
                        && let Some(sentinel) = this.sentinel.as_ref()
                    {
                        sentinel.mark();
                    }
                }
                Poll::Ready(Ok(response))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Collects what a scoped `fmt` subscriber writes, so a test can assert on
    /// the rendered access-log line.
    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut held) = self.0.lock() {
                held.extend_from_slice(bytes);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn exclude(prefixes: &[&str]) -> Arc<[String]> {
        normalize_exclusions(prefixes.iter().map(|p| (*p).to_owned()).collect())
    }

    #[test]
    fn excludes_exact_path_and_sub_segments() {
        let prefixes = exclude(&["/health", "/actuator", "/static"]);
        assert!(is_excluded("/health", &prefixes));
        assert!(is_excluded("/actuator/health", &prefixes));
        assert!(is_excluded("/static/css/app.css", &prefixes));
    }

    #[test]
    fn does_not_exclude_lookalike_prefixes() {
        let prefixes = exclude(&["/health", "/static"]);
        assert!(!is_excluded("/healthz", &prefixes));
        assert!(!is_excluded("/staticfiles", &prefixes));
        assert!(!is_excluded("/users/1", &prefixes));
    }

    #[test]
    fn trailing_slashes_in_config_are_tolerated() {
        let prefixes = exclude(&["/actuator/"]);
        assert!(is_excluded("/actuator", &prefixes));
        assert!(is_excluded("/actuator/metrics", &prefixes));
    }

    #[test]
    fn empty_or_slash_only_prefixes_exclude_nothing() {
        assert!(!is_excluded("/users/1", &exclude(&[""])));
        assert!(!is_excluded("/users/1", &exclude(&["/"])));
        assert!(!is_excluded("/users/1", &[]));
    }

    #[tokio::test]
    async fn a_response_route_names_the_log_line_when_the_request_had_none() {
        use tower::ServiceExt as _;

        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let service = AccessLogLayer::new(Vec::new()).layer(tower::service_fn(
            |_request: Request<axum::body::Body>| async {
                let mut response = Response::new(axum::body::Body::empty());
                response
                    .extensions_mut()
                    .insert(ResponseRoute("/p/x".to_owned()));
                Ok::<_, std::convert::Infallible>(response)
            },
        ));
        let request = Request::builder()
            .uri("/p/x")
            .body(axum::body::Body::empty())
            .expect("request");
        service.oneshot(request).await.expect("infallible");

        let out = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert!(out.contains("/p/x"), "{out}");
        assert!(!out.contains(UNMATCHED_ROUTE), "{out}");
    }

    /// The route and request id are captured as the request's own shared
    /// values but still recorded as `&str` fields, so the text format quotes
    /// them exactly as it did when they were copied to `String`s (a `display`
    /// field would not be quoted). The route is the matched template, never
    /// the raw path, and the id is the one the response header carries.
    #[tokio::test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "route templates use {placeholders}"
    )]
    async fn route_and_request_id_are_recorded_as_strings() {
        use tower::ServiceExt as _;

        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        // RequestIdLayer outer, access log inner: the framework's order.
        let router = axum::Router::new()
            .route("/users/{id}", axum::routing::get(|| async { "ok" }))
            .layer(AccessLogLayer::new(Vec::new()))
            .layer(crate::middleware::RequestIdLayer::default());
        let request = Request::builder()
            .uri("/users/7")
            .body(axum::body::Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.expect("infallible");
        let id = response
            .headers()
            .get(&X_REQUEST_ID)
            .and_then(|value| value.to_str().ok())
            .expect("x-request-id")
            .to_owned();

        let out = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert!(out.contains(r#"route="/users/{id}""#), "{out}");
        assert!(out.contains(&format!(r#"request_id="{id}""#)), "{out}");
        assert!(!out.contains("/users/7"), "{out}");
    }

    /// A proxy-sent id is logged as sent, case and hyphens included: the
    /// stack render writes the same text the `x-request-id` header carries.
    /// A request no route matched still records the placeholder as a string.
    #[tokio::test]
    async fn an_inbound_request_id_is_logged_as_sent() {
        use tower::ServiceExt as _;

        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let sent = "0E4F1C2A-9b3d-4E5F-8a6b-7C8D9E0F1A2B";
        let service = AccessLogLayer::new(Vec::new()).layer(tower::service_fn(
            |_request: Request<axum::body::Body>| async {
                Ok::<_, std::convert::Infallible>(Response::new(axum::body::Body::empty()))
            },
        ));
        let mut request = Request::builder()
            .uri("/nowhere")
            .body(axum::body::Body::empty())
            .expect("request");
        request.extensions_mut().insert(
            crate::middleware::RequestId::parse_inbound(sent).expect("well-formed inbound id"),
        );
        service.oneshot(request).await.expect("infallible");

        let out = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert!(out.contains(&format!(r#"request_id="{sent}""#)), "{out}");
        assert!(
            out.contains(&format!(r#"route="{UNMATCHED_ROUTE}""#)),
            "{out}"
        );
    }

    /// The fallback never sees a `RequestId` extension (`RequestIdLayer` is
    /// inner to it), so it still takes the id from the response header.
    #[tokio::test]
    async fn the_fallback_takes_the_request_id_from_the_response_header() {
        use tower::ServiceExt as _;

        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let header_id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let service = AccessLogLayer::fallback(Vec::new()).layer(tower::service_fn(
            move |_request: Request<axum::body::Body>| async move {
                let mut response = Response::new(axum::body::Body::empty());
                response.headers_mut().insert(
                    X_REQUEST_ID.clone(),
                    axum::http::HeaderValue::from_static(header_id),
                );
                Ok::<_, std::convert::Infallible>(response)
            },
        ));
        let request = Request::builder()
            .uri("/starting")
            .body(axum::body::Body::empty())
            .expect("request");
        service.oneshot(request).await.expect("infallible");

        let out = String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8");
        assert!(
            out.contains(&format!(r#"request_id="{header_id}""#)),
            "{out}"
        );
    }
}
