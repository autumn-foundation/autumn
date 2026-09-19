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
struct RequestMeta {
    method: Method,
    route: Option<String>,
    request_id: Option<String>,
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
                route: req
                    .extensions()
                    .get::<MatchedPath>()
                    .map(|matched| matched.as_str().to_owned()),
                // Set by RequestIdLayer, which sits outer to the primary
                // layer. Absent at the outermost fallback position.
                request_id: req
                    .extensions()
                    .get::<crate::middleware::RequestId>()
                    .map(ToString::to_string),
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
            start: Instant::now(),
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
                    let duration_ms = this.start.elapsed().as_secs_f64() * 1000.0;
                    // The primary layer reads the id from the request
                    // extension; the fallback covers short-circuits that may
                    // never have run RequestIdLayer, so it falls back to the
                    // `x-request-id` response header and omits the field when
                    // neither is present.
                    let header_id = response
                        .headers()
                        .get(&X_REQUEST_ID)
                        .and_then(|value| value.to_str().ok());
                    let request_id = meta.request_id.as_deref().or(header_id);
                    tracing::info!(
                        target: ACCESS_LOG_TARGET,
                        method = %meta.method,
                        route = meta.route.as_deref().unwrap_or(UNMATCHED_ROUTE),
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
}
