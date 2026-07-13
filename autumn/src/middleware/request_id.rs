//! Request ID middleware -- assigns a unique UUID v4 to every request.
//!
//! Each request gets a [`RequestId`] that is:
//!
//! 1. Inserted into request extensions (accessible to handlers via
//!    `Extension<RequestId>`).
//! 2. Added as an `X-Request-Id` response header for correlation in
//!    logs and downstream services.
//!
//! The [`RequestIdLayer`] is applied automatically by the framework.
//! You do not need to register it manually.
//!
//! # Examples
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::middleware::RequestId;
//! use axum::extract::Extension;
//!
//! #[get("/whoami")]
//! async fn whoami(Extension(req_id): Extension<RequestId>) -> String {
//!     format!("Your request ID is {req_id}")
//! }
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

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::{HeaderValue, Request, Response};
use http::header::HeaderName;
use pin_project_lite::pin_project;
use tower::{Layer, Service};
use uuid::Uuid;

/// Header name for the request ID, added to every response.
static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// A unique identifier assigned to each incoming HTTP request.
///
/// Wraps a [`Uuid`] v4 and is inserted into request extensions so handlers
/// can access it via `Extension<RequestId>`. It is also added to the
/// response as an `X-Request-Id` header for correlation in logs and
/// downstream services.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::middleware::RequestId;
/// use axum::extract::Extension;
///
/// #[get("/trace")]
/// async fn trace(Extension(req_id): Extension<RequestId>) -> String {
///     format!("request={}", req_id.as_uuid())
/// }
/// ```
#[derive(Clone, Debug)]
pub struct RequestId(Uuid);

impl RequestId {
    /// Returns the underlying [`Uuid`] value.
    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Tower [`Layer`] that wraps a service with `RequestIdService`.
///
/// Applied automatically by [`AppBuilder::run`](crate::app::AppBuilder::run).
/// If you are building a custom Axum router, you can add it manually:
///
/// ```rust,no_run
/// use autumn_web::middleware::RequestIdLayer;
///
/// let app = axum::Router::<()>::new()
///     .route("/", axum::routing::get(|| async { "ok" }))
///     .layer(RequestIdLayer);
/// ```
#[derive(Clone, Debug)]
pub struct RequestIdLayer;

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService { inner }
    }
}

/// Tower [`Service`] produced by [`RequestIdLayer`].
///
/// Generates a [`RequestId`] for each request, inserts it into request
/// extensions, and adds it as an `X-Request-Id` response header. You
/// do not construct this type directly -- it is created by
/// [`RequestIdLayer`].
#[derive(Clone, Debug)]
pub struct RequestIdService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RequestIdService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = RequestIdFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let id = RequestId(Uuid::new_v4());
        req.extensions_mut().insert(id.clone());

        RequestIdFuture {
            inner: self.inner.call(req),
            request_id: Some(id),
        }
    }
}

pin_project! {
    /// Future that adds the `X-Request-Id` header to the response.
    pub struct RequestIdFuture<F> {
        #[pin]
        inner: F,
        request_id: Option<RequestId>,
    }
}

impl<F, ResBody, E> Future for RequestIdFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(Ok(mut response)) => {
                if let Some(id) = this.request_id.take() {
                    // Format UUID directly into a stack buffer to avoid a String allocation.
                    let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
                    let s = id.0.as_hyphenated().encode_lower(&mut buf);
                    if let Ok(value) = HeaderValue::from_bytes(s.as_bytes()) {
                        response.headers_mut().insert(X_REQUEST_ID.clone(), value);
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
    use axum::extract::Extension;
    use axum::routing::get;
    use tower::ServiceExt; // for oneshot

    #[tokio::test]
    async fn response_has_request_id_header() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(RequestIdLayer);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(response.headers().contains_key("x-request-id"));
        // Verify it's a valid UUID
        let id_str = response.headers()["x-request-id"].to_str().unwrap();
        assert!(Uuid::parse_str(id_str).is_ok());
    }

    #[tokio::test]
    async fn each_request_gets_unique_id() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(RequestIdLayer);

        let r1 = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let r2 = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let id1 = r1.headers()["x-request-id"].to_str().unwrap();
        let id2 = r2.headers()["x-request-id"].to_str().unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn request_id_available_in_extensions() {
        async fn handler(Extension(id): Extension<RequestId>) -> String {
            id.to_string()
        }

        let app = Router::new().route("/", get(handler)).layer(RequestIdLayer);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // The response body should contain a valid UUID
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(Uuid::parse_str(&body_str).is_ok());
    }

    #[test]
    fn request_id_display() {
        let id = RequestId(Uuid::nil());
        assert_eq!(id.to_string(), "00000000-0000-0000-0000-000000000000");
    }
}
