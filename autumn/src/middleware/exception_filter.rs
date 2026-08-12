//! Global exception filter middleware.
//!
//! Intercepts error responses produced by [`AutumnError`](crate::AutumnError)
//! and passes them through a chain of user-registered filters before the
//! response is sent to the client.
//!
//! # Reporting vs. filtering
//!
//! An [`ExceptionFilter`] transforms the *response*. To catch handler panics
//! and ship panic + 5xx *events* to an external sink (Sentry, Slack, a custom
//! reporter), use the panic-aware [`reporting`](crate::reporting) module and
//! [`AppBuilder::with_error_reporter`](crate::app::AppBuilder::with_error_reporter).
//! The two compose: filters shape what the client sees, reporters decide where
//! failures go.
//!
//! # How it works
//!
//! When `AutumnError::into_response()` runs, it stashes an
//! [`AutumnErrorInfo`] clone into the response extensions. The
//! [`ExceptionFilterLayer`] middleware checks for this extension after the
//! inner service returns. If present, it runs the filter chain, giving each
//! filter a chance to transform, log, or replace the response.
//!
//! # Examples
//!
//! ```rust,no_run
//! use autumn_web::middleware::ExceptionFilter;
//! use autumn_web::middleware::AutumnErrorInfo;
//! use axum::response::Response;
//!
//! struct LoggingFilter;
//!
//! impl ExceptionFilter for LoggingFilter {
//!     fn filter(&self, error: &AutumnErrorInfo, response: Response) -> Response {
//!         eprintln!("Error {}: {}", error.status, error.message);
//!         response
//!     }
//! }
//!
//! # #[autumn_web::main]
//! # async fn main() {
//! autumn_web::app()
//!     .exception_filter(LoggingFilter)
//!     // .routes(...)
//! #   .routes(vec![])
//! #   ;
//! # }
//! ```

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
    )
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use http::HeaderValue;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

/// Metadata extracted from an [`AutumnError`](crate::AutumnError) and stashed
/// in the response extensions.
///
/// Exception filters receive this to inspect the original error without
/// needing to parse the response body.
#[derive(Clone, Debug)]
pub struct AutumnErrorInfo {
    /// The HTTP status code of the error.
    pub status: StatusCode,
    /// The human-readable error message.
    pub message: String,
    /// Optional field-level validation details (for 422 responses).
    pub details: Option<std::collections::HashMap<String, Vec<String>>>,
    /// Optional explicit Problem Details type URI.
    pub problem_type: Option<&'static str>,
    /// Formatted backtrace string captured at error creation time (debug builds only).
    /// Used by the dev error overlay to render the stack trace section.
    pub backtrace_string: Option<String>,
}

impl AutumnErrorInfo {
    /// Build the default Problem Details JSON error response from this info.
    ///
    /// Useful when a filter wants to log or enrich but keep the standard
    /// response format. Server-error details are sanitized because this helper
    /// does not know whether the current profile is development or production.
    #[must_use]
    pub fn into_default_response(self) -> Response {
        problem_response_from_info(&self, None, None, false)
    }
}

/// Clone an error response's headers for re-attachment onto a freshly rebuilt
/// body, dropping the ones that describe the *old* representation.
///
/// Filters like [`ProblemDetailsFilter`] and the HTML error-page filter render a
/// brand-new, uncompressed body, so headers that describe the previous body must
/// not ride along — keeping them yields a mismatched response (e.g. a stale
/// `Content-Encoding: gzip` from an inner `CompressionLayer` left on plain JSON
/// or HTML, which browsers fail to decode). What *should* survive are
/// connection-level annotations the rebuilt response still wants, such as
/// `X-Request-Id` and CORS headers mirrored onto a timeout 503.
///
/// Stripped: `Content-Type` and `Content-Length` (the rebuilt response sets its
/// own), plus the body-representation headers `Content-Encoding`,
/// `Transfer-Encoding`, and `Content-Range`.
pub fn preserved_error_headers(response: &Response) -> http::HeaderMap {
    let mut headers = response.headers().clone();
    for name in [
        http::header::CONTENT_TYPE,
        http::header::CONTENT_LENGTH,
        http::header::CONTENT_ENCODING,
        http::header::TRANSFER_ENCODING,
        http::header::CONTENT_RANGE,
    ] {
        headers.remove(name);
    }
    headers
}

/// Exception filter that rebuilds framework errors as request-aware Problem
/// Details responses before HTML error-page negotiation runs.
pub struct ProblemDetailsFilter {
    pub is_dev: bool,
}

impl ExceptionFilter for ProblemDetailsFilter {
    fn filter(&self, error: &AutumnErrorInfo, response: Response) -> Response {
        let context = response
            .extensions()
            .get::<crate::middleware::error_page_filter::ErrorPageRequestContext>();
        let request_id = context.and_then(|ctx| ctx.request_id.clone()).or_else(|| {
            response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });
        let instance = context.map(|ctx| ctx.uri.path().to_owned());
        let preserved_headers = preserved_error_headers(&response);

        let mut out = problem_response_from_info(error, request_id, instance, self.is_dev);
        out.headers_mut().extend(preserved_headers);
        out.headers_mut().insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );

        if let Some(wants_html) = response
            .extensions()
            .get::<crate::middleware::error_page_filter::WantsHtml>()
            .cloned()
        {
            out.extensions_mut().insert(wants_html);
        }
        if let Some(ctx) = context.cloned() {
            out.extensions_mut().insert(ctx);
        }
        // A caught panic's identity must survive the rebuild: the capsule
        // replay driver reads it to compare panic against panic rather than
        // accepting any response with the same status.
        #[cfg(feature = "reporting")]
        if let Some(caught) = response
            .extensions()
            .get::<crate::reporting::CaughtPanic>()
            .cloned()
        {
            out.extensions_mut().insert(caught);
        }
        out.extensions_mut().insert(error.clone());
        out
    }
}

fn problem_response_from_info(
    error: &AutumnErrorInfo,
    request_id: Option<String>,
    instance: Option<String>,
    is_dev: bool,
) -> Response {
    let body = crate::error::problem_details(
        error.status,
        error.message.clone(),
        error.details.as_ref(),
        error.problem_type,
        request_id,
        instance,
        is_dev,
    );
    let mut response = (error.status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

/// Trait for global exception filters.
///
/// Implement this trait to intercept error responses before they are sent
/// to the client. Filters can log, transform, or completely replace the
/// response.
///
/// # Examples
///
/// ```rust
/// use autumn_web::middleware::{ExceptionFilter, AutumnErrorInfo};
/// use axum::response::Response;
///
/// struct SentryFilter;
///
/// impl ExceptionFilter for SentryFilter {
///     fn filter(&self, error: &AutumnErrorInfo, response: Response) -> Response {
///         // Log to Sentry, metrics, etc.
///         eprintln!("[sentry] {} {}", error.status, error.message);
///         response // pass through unchanged
///     }
/// }
/// ```
pub trait ExceptionFilter: Send + Sync + 'static {
    /// Inspect and optionally transform an error response.
    ///
    /// `error` contains the original error metadata. `response` is the
    /// current HTTP response (which may have been modified by a previous
    /// filter in the chain). Return the response to send to the client.
    fn filter(&self, error: &AutumnErrorInfo, response: Response) -> Response;
}

/// Tower [`Layer`] that applies the exception filter chain.
///
/// Applied automatically by [`AppBuilder::run`](crate::app::AppBuilder::run)
/// when at least one exception filter is registered.
#[derive(Clone)]
pub struct ExceptionFilterLayer {
    filters: Arc<Vec<Arc<dyn ExceptionFilter>>>,
}

impl ExceptionFilterLayer {
    /// Create a new layer with the given filter chain.
    #[must_use]
    pub fn new(filters: Vec<Arc<dyn ExceptionFilter>>) -> Self {
        Self {
            filters: Arc::new(filters),
        }
    }
}

impl<S> Layer<S> for ExceptionFilterLayer {
    type Service = ExceptionFilterService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ExceptionFilterService {
            inner,
            filters: Arc::clone(&self.filters),
        }
    }
}

/// Tower [`Service`] produced by [`ExceptionFilterLayer`].
#[derive(Clone)]
pub struct ExceptionFilterService<S> {
    inner: S,
    filters: Arc<Vec<Arc<dyn ExceptionFilter>>>,
}

impl<S, ReqBody> Service<Request<ReqBody>> for ExceptionFilterService<S>
where
    S: Service<Request<ReqBody>, Response = Response>,
{
    type Response = Response;
    type Error = S::Error;
    type Future = ExceptionFilterFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        ExceptionFilterFuture {
            inner: self.inner.call(req),
            filters: Arc::clone(&self.filters),
        }
    }
}

pin_project! {
    /// Future that applies exception filters to error responses.
    pub struct ExceptionFilterFuture<F> {
        #[pin]
        inner: F,
        filters: Arc<Vec<Arc<dyn ExceptionFilter>>>,
    }
}

impl<F, E> Future for ExceptionFilterFuture<F>
where
    F: Future<Output = Result<Response, E>>,
{
    type Output = Result<Response, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(Ok(response)) => {
                // Check if this response came from AutumnError and clone the info out
                if let Some(error_info) = response.extensions().get::<AutumnErrorInfo>().cloned() {
                    let mut response = response;
                    let filters = this.filters;
                    for filter in filters.iter() {
                        response = filter.filter(&error_info, response);
                    }
                    Poll::Ready(Ok(response))
                } else {
                    Poll::Ready(Ok(response))
                }
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
    use http::Request;
    use tower::ServiceExt;

    use crate::error::AutumnError;

    #[tokio::test]
    async fn filter_receives_error_info() {
        use std::sync::atomic::{AtomicBool, Ordering};

        static CALLED: AtomicBool = AtomicBool::new(false);

        struct TestFilter;
        impl ExceptionFilter for TestFilter {
            fn filter(&self, error: &AutumnErrorInfo, response: Response) -> Response {
                assert_eq!(error.status, StatusCode::NOT_FOUND);
                assert_eq!(error.message, "not here");
                CALLED.store(true, Ordering::SeqCst);
                response
            }
        }

        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    Err::<String, AutumnError>(AutumnError::not_found_msg("not here"))
                }),
            )
            .layer(ExceptionFilterLayer::new(vec![Arc::new(TestFilter)]));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(CALLED.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn filter_can_replace_response() {
        struct ReplaceFilter;
        impl ExceptionFilter for ReplaceFilter {
            fn filter(&self, _error: &AutumnErrorInfo, _response: Response) -> Response {
                (StatusCode::SERVICE_UNAVAILABLE, "custom error page").into_response()
            }
        }

        let app = Router::new()
            .route(
                "/",
                get(|| async { Err::<String, AutumnError>(AutumnError::not_found_msg("gone")) }),
            )
            .layer(ExceptionFilterLayer::new(vec![Arc::new(ReplaceFilter)]));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"custom error page");
    }

    #[tokio::test]
    async fn problem_details_filter_preserves_existing_response_headers() {
        let error = AutumnErrorInfo {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "database unavailable".into(),
            details: None,
            problem_type: None,
            backtrace_string: None,
        };
        let mut original = (StatusCode::INTERNAL_SERVER_ERROR, "old error body").into_response();
        original.headers_mut().insert(
            "access-control-allow-origin",
            http::HeaderValue::from_static("https://client.example"),
        );
        original
            .headers_mut()
            .insert("x-frame-options", http::HeaderValue::from_static("DENY"));
        original.headers_mut().insert(
            "content-security-policy",
            http::HeaderValue::from_static("default-src 'self'"),
        );
        original.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain; charset=utf-8"),
        );

        let response = ProblemDetailsFilter { is_dev: false }.filter(&error, original);

        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://client.example"
        );
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(
            response.headers()["content-security-policy"],
            "default-src 'self'"
        );
        assert_eq!(
            response.headers()[http::header::CONTENT_TYPE],
            "application/problem+json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["detail"], "Internal server error");
    }

    #[tokio::test]
    async fn problem_details_filter_drops_stale_body_representation_headers() {
        // If an inner layer transformed the body (e.g. compressed it) before the
        // filter runs, the rebuilt Problem Details JSON must not inherit headers
        // that described the old body.
        let error = AutumnErrorInfo {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "boom".into(),
            details: None,
            problem_type: None,
            backtrace_string: None,
        };
        let mut original = (StatusCode::INTERNAL_SERVER_ERROR, "compressed").into_response();
        original.headers_mut().insert(
            http::header::CONTENT_ENCODING,
            HeaderValue::from_static("gzip"),
        );
        original.headers_mut().insert(
            http::header::CONTENT_RANGE,
            HeaderValue::from_static("bytes 0-9/100"),
        );
        original
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_static("req-7"));

        let response = ProblemDetailsFilter { is_dev: false }.filter(&error, original);

        assert!(
            !response
                .headers()
                .contains_key(http::header::CONTENT_ENCODING),
            "stale Content-Encoding must be dropped"
        );
        assert!(
            !response.headers().contains_key(http::header::CONTENT_RANGE),
            "stale Content-Range must be dropped"
        );
        assert_eq!(response.headers()["x-request-id"], "req-7");
        assert_eq!(
            response.headers()[http::header::CONTENT_TYPE],
            "application/problem+json"
        );
    }

    #[tokio::test]
    async fn success_responses_bypass_filters() {
        use std::sync::atomic::{AtomicBool, Ordering};

        static CALLED: AtomicBool = AtomicBool::new(false);

        struct NeverFilter;
        impl ExceptionFilter for NeverFilter {
            fn filter(&self, _error: &AutumnErrorInfo, response: Response) -> Response {
                CALLED.store(true, Ordering::SeqCst);
                response
            }
        }

        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(ExceptionFilterLayer::new(vec![Arc::new(NeverFilter)]));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!CALLED.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn multiple_filters_run_in_order() {
        use std::sync::atomic::{AtomicU32, Ordering};

        static COUNTER: AtomicU32 = AtomicU32::new(0);

        struct OrderFilter(u32);
        impl ExceptionFilter for OrderFilter {
            fn filter(&self, _error: &AutumnErrorInfo, response: Response) -> Response {
                let current = COUNTER.fetch_add(1, Ordering::SeqCst);
                assert_eq!(current, self.0, "filters should run in registration order");
                response
            }
        }

        COUNTER.store(0, Ordering::SeqCst);

        let app = Router::new()
            .route(
                "/",
                get(|| async { Err::<String, AutumnError>(AutumnError::bad_request_msg("oops")) }),
            )
            .layer(ExceptionFilterLayer::new(vec![
                Arc::new(OrderFilter(0)),
                Arc::new(OrderFilter(1)),
                Arc::new(OrderFilter(2)),
            ]));

        app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(COUNTER.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn error_info_into_default_response() {
        let info = AutumnErrorInfo {
            status: StatusCode::NOT_FOUND,
            message: "not found".into(),
            details: None,
            problem_type: None,
            backtrace_string: None,
        };
        let response = info.into_default_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn default_response_hides_internal_error_detail() {
        let info = AutumnErrorInfo {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "database password leaked".into(),
            details: None,
            problem_type: None,
            backtrace_string: None,
        };
        let response = info.into_default_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["detail"], "Internal server error");
    }
}
