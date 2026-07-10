//! Request-scoped log context middleware.
//!
//! Establishes an always-on [`LogContext`](crate::log::context::LogContext) for
//! every HTTP request, seeded with the request's `request_id` (read from the
//! [`RequestId`](crate::middleware::RequestId) extension installed by
//! [`RequestIdLayer`](crate::middleware::RequestIdLayer)). The request is then
//! driven inside both that task-local context and a `tracing` span carrying
//! `request_id`/`user_id`/`tenant_id`, so every event emitted during the
//! request automatically correlates back to it.
//!
//! This layer is **not** gated behind any telemetry feature — it is applied on
//! the default ingress path. Place it inner to `RequestIdLayer` so the request
//! id is available.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::http::{Request, Response};
use http_body::{Body as HttpBody, Frame, SizeHint};
use pin_project_lite::pin_project;
use tower::{Layer, Service};
use tracing::Instrument as _;

use crate::log::context::{self, LogContext};
use crate::log::filter::ParameterFilter;
use crate::middleware::RequestId;

/// Tower [`Layer`] that installs a [`LogContext`] for the duration of each
/// request. Applied automatically by the framework router.
#[derive(Clone)]
pub struct LogContextLayer {
    filter: Arc<ParameterFilter>,
}

impl LogContextLayer {
    /// Create a new layer using `filter` to scrub sensitive custom fields
    /// recorded via [`with_log_field`](crate::log::context::with_log_field).
    #[must_use]
    pub const fn new(filter: Arc<ParameterFilter>) -> Self {
        Self { filter }
    }
}

impl std::fmt::Debug for LogContextLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogContextLayer").finish_non_exhaustive()
    }
}

impl<S> Layer<S> for LogContextLayer {
    type Service = LogContextService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LogContextService {
            inner,
            filter: Arc::clone(&self.filter),
        }
    }
}

/// Tower [`Service`] produced by [`LogContextLayer`].
#[derive(Clone)]
pub struct LogContextService<S> {
    inner: S,
    filter: Arc<ParameterFilter>,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for LogContextService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    S::Future: Send + 'static,
    S::Error: 'static,
    ResBody: HttpBody + Send + 'static,
{
    type Response = Response<LogContextBody<crate::current::CurrentActorBody<ResBody>>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let request_id = req.extensions().get::<RequestId>().map(ToString::to_string);

        // Open an always-on request span carrying the well-known correlation
        // fields. They are declared up front (Empty) so `set_user_id` /
        // `set_tenant_id` can record them later and they surface in standard
        // log output for every event emitted within the request.
        let span = tracing::info_span!(
            "request",
            request_id = tracing::field::Empty,
            user_id = tracing::field::Empty,
            tenant_id = tracing::field::Empty,
        );
        if let Some(ref rid) = request_id {
            span.record("request_id", tracing::field::display(rid));
        }

        // Attach the request span so `set_user_id` / `set_tenant_id` record onto
        // it directly, rather than whatever child span happens to be current.
        let ctx =
            LogContext::with_filter(request_id, Arc::clone(&self.filter)).with_span(span.clone());
        let body_ctx = ctx.clone();

        // Construct the inner future with the request span entered *and* the log
        // context installed, so any synchronous work a downstream layer performs
        // in its own `Service::call` (logging, `with_log_field`) is correlated
        // too — not just the async polling of the returned future.
        //
        // Note the deliberate asymmetry with the actor scope below: log context
        // wraps this synchronous `call` (via `sync_scope`) *and* the async future,
        // whereas `scope_request` wraps only the async future. No downstream
        // synchronous `call` touches `Current`, so a future author must not add a
        // synchronous `Current::set_actor` here expecting it to land.
        let inner = span
            .in_scope(|| context::sync_scope(ctx.clone(), || self.inner.call(req)))
            .instrument(span);

        // Wrap the response body so the context is re-established on every frame
        // poll: a lazy/streaming body (SSE, `Body::from_stream`) is produced
        // after this future resolves, otherwise dropping the context before any
        // stream code runs. Mirrors `TenantPropagatingBody`.
        //
        // Also establish an empty current-actor scope (#1383) for the whole
        // request so the auth layer can publish the resolved principal via
        // `Current::set_actor` after the request future has started, and the
        // generated repository/audit writes read it ambiently.
        Box::pin(crate::current::scope_request(context::scoped(
            ctx,
            async move {
                let response = inner.await?;
                let (parts, body) = response.into_parts();
                // Capture the request's *resolved* actor scope (the auth layer has
                // published the principal by now) and re-establish it on every
                // streamed frame, so a lazy/SSE body's writes stay attributed to
                // the request actor rather than falling back to SYSTEM_ACTOR.
                // Wrapped inside the log-context body so both scopes are active
                // during frame production, consistent with the tenant body wrap.
                let actor_body =
                    crate::current::CurrentActorBody::new(body, crate::current::current_scope());
                Ok(Response::from_parts(
                    parts,
                    LogContextBody::new(actor_body, body_ctx),
                ))
            },
        )))
    }
}

pin_project! {
    /// Response body wrapper that re-establishes the request [`LogContext`] (and
    /// re-enters its span) on every frame poll, so logs emitted while producing
    /// lazy/streaming response bodies stay correlated to the originating request.
    pub struct LogContextBody<B> {
        #[pin]
        inner: B,
        ctx: LogContext,
    }
}

impl<B> LogContextBody<B> {
    const fn new(inner: B, ctx: LogContext) -> Self {
        Self { inner, ctx }
    }
}

impl<B: HttpBody> HttpBody for LogContextBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let ctx = this.ctx.clone();
        let span = ctx.span();
        let _entered = span.enter();
        context::sync_scope(ctx, || this.inner.poll_frame(cx))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::context;
    use crate::middleware::RequestIdLayer;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use std::sync::Mutex;
    use tower::ServiceExt as _;
    use tracing::subscriber::with_default;
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// Test tracing layer that records the request context snapshot observed at
    /// the moment each event is emitted.
    #[derive(Clone, Default)]
    struct CaptureLayer {
        seen: Arc<Mutex<Vec<context::LogFields>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            _event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if let Some(fields) = context::snapshot() {
                self.seen.lock().unwrap().push(fields);
            }
        }
    }

    fn filter() -> Arc<ParameterFilter> {
        Arc::new(ParameterFilter::default())
    }

    #[tokio::test]
    async fn streaming_body_frames_re_establish_the_context() {
        // A response body produced lazily is polled after the request future has
        // resolved (and the task-local scope has ended). LogContextBody must
        // re-install the context for each frame poll.
        struct ProbeBody {
            // The request_id observed during each frame poll (one entry per poll).
            seen: Arc<Mutex<Vec<Option<String>>>>,
            done: bool,
        }
        impl HttpBody for ProbeBody {
            type Data = axum::body::Bytes;
            type Error = std::convert::Infallible;
            fn poll_frame(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
                // Record what the context looks like *during* frame production.
                self.seen
                    .lock()
                    .unwrap()
                    .push(context::snapshot().and_then(|s| s.request_id));
                if self.done {
                    Poll::Ready(None)
                } else {
                    self.done = true;
                    Poll::Ready(Some(Ok(Frame::data(axum::body::Bytes::from_static(b"x")))))
                }
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let ctx = LogContext::new(Some("req-stream".to_owned()));
        let body = LogContextBody::new(
            ProbeBody {
                seen: Arc::clone(&seen),
                done: false,
            },
            ctx,
        );

        // Poll the wrapped body with NO ambient context installed.
        let mut body = Box::pin(body);
        let _ = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;

        let captured = seen.lock().unwrap().clone();
        assert_eq!(
            captured,
            vec![Some("req-stream".to_owned())],
            "streaming body frame production lost the request context"
        );
    }

    #[test]
    fn event_in_handler_carries_request_id_and_user_id() {
        let capture = CaptureLayer::default();
        let seen = Arc::clone(&capture.seen);
        let subscriber = tracing_subscriber::registry().with(capture.boxed());

        with_default(subscriber, || {
            // Build the same layering the framework uses: RequestId outer,
            // LogContext inner.
            let app = Router::new()
                .route(
                    "/",
                    get(|| async {
                        // Simulate the auth layer resolving the user.
                        context::set_user_id("42");
                        tracing::info!("handled");
                        "ok"
                    }),
                )
                .layer(LogContextLayer::new(filter()))
                .layer(RequestIdLayer);

            // `with_default` is sync; drive the request to completion on a local
            // runtime so the captured events are observed under this subscriber.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let response = rt.block_on(async {
                app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            });

            let header_id = response
                .headers()
                .get("x-request-id")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();

            let handled = {
                let events = seen.lock().unwrap();
                events
                    .iter()
                    .find(|f| f.request_id.is_some())
                    .cloned()
                    .expect("expected at least one event carrying request context")
            };
            assert_eq!(handled.request_id.as_deref(), Some(header_id.as_str()));
            assert_eq!(handled.user_id.as_deref(), Some("42"));
        });
    }

    #[tokio::test]
    async fn downstream_synchronous_call_runs_inside_the_context() {
        // A downstream service that inspects the context in its *synchronous*
        // `Service::call` (before returning a future) must already see it.
        #[derive(Clone)]
        struct ProbeService {
            seen: Arc<Mutex<Option<bool>>>,
        }
        impl Service<Request<Body>> for ProbeService {
            type Response = axum::response::Response;
            type Error = std::convert::Infallible;
            type Future =
                std::future::Ready<Result<axum::response::Response, std::convert::Infallible>>;
            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, _req: Request<Body>) -> Self::Future {
                *self.seen.lock().unwrap() = Some(context::current().is_some());
                std::future::ready(Ok(axum::response::Response::new(Body::empty())))
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let svc = LogContextLayer::new(filter()).layer(ProbeService {
            seen: Arc::clone(&seen),
        });
        let _ = svc
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "downstream Service::call should run inside the log context"
        );
    }

    #[tokio::test]
    async fn context_is_isolated_between_requests() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    let before = context::snapshot().unwrap();
                    assert!(before.fields.is_empty());
                    context::with_log_field("k", "v");
                    "ok"
                }),
            )
            .layer(LogContextLayer::new(filter()))
            .layer(RequestIdLayer);

        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
        }
    }

    #[test]
    fn custom_fields_are_captured_and_sensitive_ones_scrubbed() {
        let capture = CaptureLayer::default();
        let seen = Arc::clone(&capture.seen);
        let subscriber = tracing_subscriber::registry().with(capture.boxed());

        with_default(subscriber, || {
            let app = Router::new()
                .route(
                    "/",
                    get(|| async {
                        context::set_tenant_id("acme");
                        context::with_log_field("order_id", "A-1001");
                        context::with_log_field("password", "hunter2");
                        tracing::info!("processed order");
                        "ok"
                    }),
                )
                .layer(LogContextLayer::new(filter()))
                .layer(RequestIdLayer);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            });

            let fields = {
                let events = seen.lock().unwrap();
                events
                    .iter()
                    .find(|f| f.fields.contains_key("order_id"))
                    .cloned()
                    .expect("expected an event carrying the custom field")
            };
            assert_eq!(fields.tenant_id.as_deref(), Some("acme"));
            assert_eq!(
                fields.fields.get("order_id").map(String::as_str),
                Some("A-1001")
            );
            assert_eq!(
                fields.fields.get("password").map(String::as_str),
                Some(crate::log::filter::FILTERED_PLACEHOLDER),
                "sensitive custom field should be scrubbed by the layer filter"
            );
        });
    }

    #[test]
    fn user_id_renders_even_when_set_from_a_nested_child_span() {
        // Regression guard: `set_user_id` must record onto the request span, not
        // whatever child span is current — otherwise the field is silently
        // dropped from log output when set from inside e.g. a DB-query span.
        #[derive(Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        struct BufGuard(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for BufGuard {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
            type Writer = BufGuard;
            fn make_writer(&'a self) -> Self::Writer {
                BufGuard(Arc::clone(&self.0))
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(BufWriter(Arc::clone(&buf))),
        );

        with_default(subscriber, || {
            let app = Router::new()
                .route(
                    "/",
                    get(|| async {
                        // Emit from inside a nested child span.
                        let child = tracing::info_span!("child");
                        let _guard = child.enter();
                        context::set_user_id("42");
                        tracing::info!("from child");
                        "ok"
                    }),
                )
                .layer(LogContextLayer::new(filter()))
                .layer(RequestIdLayer);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            });
        });

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("user_id=42"),
            "user_id should be recorded on the request span and rendered; got: {out}"
        );
    }

    #[tokio::test]
    async fn request_id_matches_x_request_id_header() {
        async fn handler() -> String {
            context::snapshot()
                .and_then(|s| s.request_id)
                .unwrap_or_default()
        }

        let app = Router::new()
            .route("/", get(handler))
            .layer(LogContextLayer::new(filter()))
            .layer(RequestIdLayer);

        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let header_id = resp
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_id = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(body_id, header_id);
        assert!(!body_id.is_empty());
    }
}
