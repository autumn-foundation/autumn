//! W3C Trace Context propagation middleware.
//!
//! Wraps each HTTP request in a server-side tracing span whose parent is
//! taken from the incoming `traceparent` / `tracestate` headers (per the
//! [W3C Trace Context spec][w3c]). On response, the current span context
//! is injected back into the response headers so callers can continue the
//! trace on the return path.
//!
//! [w3c]: https://www.w3.org/TR/trace-context/
//!
//! The layer is feature-gated on `telemetry-otlp`; when the feature is
//! disabled there is no OpenTelemetry crate available to read/write the
//! context so this module compiles away to nothing.
//!
//! Installed automatically by the framework when the `telemetry-otlp`
//! feature is enabled — you do not need to register it manually.

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
use std::task::{Context, Poll};

use axum::http::{HeaderMap, Request, Response};
use opentelemetry::propagation::{Extractor, Injector};
use pin_project_lite::pin_project;
use tower::{Layer, Service};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// Tower [`Layer`] that extracts W3C trace context from incoming requests
/// and injects the current context into outgoing responses.
///
/// Relies on the global [`TextMapPropagator`](opentelemetry::propagation::TextMapPropagator)
/// set by [`telemetry::init`](crate::telemetry::init) — typically the
/// [`TraceContextPropagator`](opentelemetry_sdk::propagation::TraceContextPropagator)
/// for W3C `traceparent` / `tracestate`.
#[derive(Clone, Debug, Default)]
pub struct TraceContextLayer;

impl<S> Layer<S> for TraceContextLayer {
    type Service = TraceContextService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TraceContextService { inner }
    }
}

/// Tower [`Service`] produced by [`TraceContextLayer`].
#[derive(Clone, Debug)]
pub struct TraceContextService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for TraceContextService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = TraceContextFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let parent_cx = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&HeaderMapExtractor(req.headers()))
        });

        let method = req.method().clone();
        let uri = req.uri().clone();
        // OTel HTTP server semantic conventions want `{method} {http.route}`
        // for span names, falling back to the method alone when the matched
        // route template is unknown. Axum resolves the template inside the
        // inner service so we can't see it here — using the raw URI path
        // would explode cardinality (every `/users/123` hits a distinct
        // span name). Stick to the method and keep the path as a searchable
        // attribute.
        let span = tracing::info_span!(
            "http.server.request",
            otel.name = %method,
            otel.kind = "server",
            http.request.method = %method,
            url.path = %uri.path(),
            http.response.status_code = tracing::field::Empty,
        );
        // Failure only occurs when no `tracing-opentelemetry` subscriber is
        // installed — which simply means traces aren't being collected.
        // Nothing to recover from at ingress time.
        let _ = span.set_parent(parent_cx);

        // Enter the span before building the inner future so any tracing
        // performed synchronously inside downstream `Service::call`
        // implementations is parented on the extracted context, not just
        // the work that runs during `poll`.
        let future = span.in_scope(|| self.inner.call(req));
        TraceContextFuture {
            inner: future.instrument(span.clone()),
            span,
        }
    }
}

pin_project! {
    /// Future that records response status and injects the W3C trace
    /// context into the outgoing response headers.
    pub struct TraceContextFuture<F> {
        #[pin]
        inner: tracing::instrument::Instrumented<F>,
        span: tracing::Span,
    }
}

impl<F, ResBody, E> Future for TraceContextFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(Ok(mut response)) => {
                this.span
                    .record("http.response.status_code", response.status().as_u16());

                let cx = this.span.context();
                opentelemetry::global::get_text_map_propagator(|propagator| {
                    propagator.inject_context(&cx, &mut HeaderMapInjector(response.headers_mut()));
                });
                Poll::Ready(Ok(response))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct HeaderMapExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderMapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

struct HeaderMapInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderMapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(key.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use tower::ServiceExt;

    const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
    const SPAN_ID: &str = "b7ad6b7169203331";
    const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    #[test]
    fn header_map_extractor_reads_values() {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", TRACEPARENT.parse().unwrap());
        headers.insert("tracestate", "vendor=opaque".parse().unwrap());
        let extractor = HeaderMapExtractor(&headers);
        assert_eq!(extractor.get("traceparent"), Some(TRACEPARENT));
        assert_eq!(extractor.get("tracestate"), Some("vendor=opaque"));
        assert_eq!(extractor.get("absent"), None);
        let keys: std::collections::HashSet<_> = extractor.keys().into_iter().collect();
        assert!(keys.contains("traceparent"));
        assert!(keys.contains("tracestate"));
    }

    #[test]
    fn header_map_injector_writes_values() {
        let mut headers = HeaderMap::new();
        {
            let mut injector = HeaderMapInjector(&mut headers);
            injector.set("traceparent", TRACEPARENT.to_owned());
            injector.set("tracestate", "vendor=opaque".to_owned());
        }
        assert_eq!(
            headers.get("traceparent").unwrap().to_str().unwrap(),
            TRACEPARENT
        );
        assert_eq!(
            headers.get("tracestate").unwrap().to_str().unwrap(),
            "vendor=opaque"
        );
    }

    #[test]
    fn traceparent_extracts_expected_span_context() {
        // Verifies the extractor plumbing end-to-end against the W3C
        // propagator without needing a global subscriber.
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", TRACEPARENT.parse().unwrap());
        let propagator = TraceContextPropagator::new();
        let cx = propagator.extract(&HeaderMapExtractor(&headers));
        let span_cx = cx.span().span_context().clone();
        assert!(span_cx.is_valid());
        assert_eq!(span_cx.trace_id().to_string(), TRACE_ID);
        assert_eq!(span_cx.span_id().to_string(), SPAN_ID);
    }

    #[tokio::test]
    async fn service_runs_request_without_propagator_installed() {
        // When no global propagator is installed (the default in test
        // processes), the layer should still pass the request through
        // without panicking or erroring.
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(TraceContextLayer);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("traceparent", TRACEPARENT)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn service_propagates_inner_error_through_poll() {
        // Exercises the `Poll::Ready(Err(_))` branch in `TraceContextFuture::poll`.
        use std::convert::Infallible;
        use tower::Service;

        #[derive(Clone)]
        struct ErrService;
        impl Service<Request<Body>> for ErrService {
            type Response = Response<Body>;
            type Error = &'static str;
            type Future = std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<Response<Body>, &'static str>> + Send>,
            >;
            fn poll_ready(
                &mut self,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn call(&mut self, _req: Request<Body>) -> Self::Future {
                Box::pin(async { Err::<Response<Body>, _>("downstream boom") })
            }
        }

        // Infallible placates the compiler that `ErrService::Error` is usable
        // directly without axum's Router error remapping.
        let _ = std::marker::PhantomData::<Infallible>;

        let mut layered = TraceContextLayer.layer(ErrService);
        let result = layered
            .call(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await;
        assert_eq!(result.unwrap_err(), "downstream boom");
    }

    #[tokio::test]
    async fn call_records_status_code_on_response_span_field() {
        // Covers the `span.record("http.response.status_code", ...)` line in
        // `TraceContextFuture::poll` by driving a status through the layer.
        let app = Router::new()
            .route("/", get(|| async { (StatusCode::IM_A_TEAPOT, "teapot") }))
            .layer(TraceContextLayer);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    }
}
