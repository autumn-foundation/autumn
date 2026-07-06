//! Bounded-concurrency admission control ("load shedding") Tower middleware.
//!
//! Caps the number of concurrently in-flight requests. Once the ceiling is
//! reached, additional requests are rejected immediately with `503 Service
//! Unavailable` + `Retry-After`, before the handler runs or the request body
//! is read — a brownout (fail fast, try another replica) instead of an
//! unbounded pile-up of admitted work that risks an OOM kill (a full
//! blackout). Routes under the configured actuator/health prefix and exact
//! probe paths always pass through uncounted, so platform load balancers
//! keep every replica in rotation regardless of load (see #1006).
//!
//! Disabled entirely when no ceiling is configured
//! (`server.max_concurrent_requests` unset or `0`) — see
//! [`crate::router::build_load_shed_layer`], which returns `None` in that
//! case so this layer is never applied and there is no overhead.
//!
//! The admission gauge is a dedicated counter, independent of
//! [`crate::middleware::MetricsCollector`]'s `requests_active` and the
//! graceful-shutdown drain accounting, so shedding cannot double-count,
//! deadlock, or extend the drain budget.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, header::RETRY_AFTER};
use axum::response::IntoResponse;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::middleware::MetricsCollector;

/// `Retry-After` value (seconds) sent on every shed `503`. Kept short so a
/// client or load balancer retries fast (or fails over to another replica)
/// rather than piling onto the already-loaded process.
const RETRY_AFTER_SECS: &str = "1";

/// Tower [`Layer`] that caps concurrent in-flight requests and sheds the
/// excess with an immediate `503 Service Unavailable`.
///
/// Clone this layer freely — the in-flight counter is shared via [`Arc`].
#[derive(Clone)]
pub struct LoadShedLayer {
    limit: usize,
    in_flight: Arc<AtomicUsize>,
    metrics: MetricsCollector,
    health_prefix: String,
    probe_paths: Vec<String>,
}

impl LoadShedLayer {
    /// Create a layer that admits at most `limit` concurrent requests.
    ///
    /// A `limit` of `0` disables shedding entirely (every request is
    /// forwarded, uncounted) — callers should prefer not constructing this
    /// layer at all when the ceiling is unset (see
    /// [`crate::router::build_load_shed_layer`]), but `0` is handled safely
    /// here too so a misconfigured value never wedges every request shut.
    #[must_use]
    pub fn new(limit: usize, metrics: MetricsCollector) -> Self {
        Self {
            limit,
            in_flight: Arc::new(AtomicUsize::new(0)),
            metrics,
            health_prefix: String::new(),
            probe_paths: Vec::new(),
        }
    }

    /// Requests whose path starts with this prefix always pass through,
    /// uncounted (e.g. the actuator prefix).
    #[must_use]
    pub fn with_health_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.health_prefix = prefix.into();
        self
    }

    /// Exact-match probe paths that always pass through, uncounted (e.g.
    /// `/live`, `/ready`, `/startup`, `/health`).
    #[must_use]
    pub fn with_probe_paths(mut self, paths: Vec<String>) -> Self {
        self.probe_paths = paths;
        self
    }
}

impl<S> Layer<S> for LoadShedLayer {
    type Service = LoadShedService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoadShedService {
            inner,
            layer: self.clone(),
        }
    }
}

/// Tower [`Service`] produced by [`LoadShedLayer`].
#[derive(Clone)]
pub struct LoadShedService<S> {
    inner: S,
    layer: LoadShedLayer,
}

impl<S> LoadShedService<S> {
    /// Whether `req` bypasses admission control entirely (probes/actuator).
    fn is_exempt<B>(&self, req: &Request<B>) -> bool {
        let path = req.uri().path();

        let prefix = &self.layer.health_prefix;
        let prefix_matched = if prefix.is_empty() {
            false
        } else if prefix == "/" {
            path == "/"
        } else {
            path == prefix || {
                let mut prefix_slash = prefix.clone();
                if !prefix_slash.ends_with('/') {
                    prefix_slash.push('/');
                }
                path.starts_with(&prefix_slash)
            }
        };
        prefix_matched || self.layer.probe_paths.iter().any(|probe| probe == path)
    }
}

impl<S, ReqBody> Service<Request<ReqBody>> for LoadShedService<S>
where
    S: Service<Request<ReqBody>, Response = Response<Body>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = LoadShedFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        if self.layer.limit == 0 || self.is_exempt(&req) {
            return LoadShedFuture::Forward {
                inner: self.inner.call(req),
                _guard: None,
            };
        }

        let in_flight = &self.layer.in_flight;
        let mut current = in_flight.load(Ordering::Acquire);
        loop {
            if current >= self.layer.limit {
                self.layer.metrics.record_request_shed();
                return LoadShedFuture::ShortCircuit {
                    response: Some(build_shed_response()),
                };
            }
            match in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }

        LoadShedFuture::Forward {
            inner: self.inner.call(req),
            _guard: Some(InFlightGuard {
                counter: Arc::clone(in_flight),
            }),
        }
    }
}

/// Held for the lifetime of an admitted request's inner future; decrements
/// the shared in-flight counter on drop, whether the future resolves
/// normally or is cancelled (dropped) mid-flight — the same guarantee
/// [`crate::middleware::metrics::MetricsFuture`]'s `PinnedDrop` gives
/// `requests_active`.
struct InFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Build the `503 Service Unavailable` response for a shed request.
///
/// Delegates to [`crate::error::AutumnError`] (the same mechanism the
/// built-in per-request timeout uses) so the response flows through the
/// standard Problem Details / error-page stack: JSON for API clients, the
/// framework's styled HTML error page for browsers with an `Accept: text/html`
/// preference (negotiated by the outer `ErrorPageContext`/`ExceptionFilter`
/// layers, which preserve headers already on the response — including the
/// `Retry-After` set here).
fn build_shed_response() -> Response<Body> {
    let mut response = crate::error::AutumnError::service_unavailable_msg(
        "Too many concurrent requests; try again shortly.",
    )
    .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECS));
    response
}

pin_project! {
    /// Future returned by [`LoadShedService`].
    ///
    /// Either resolves immediately with a `503` (short-circuit path, ceiling
    /// reached) or delegates to the wrapped inner service while holding an
    /// [`InFlightGuard`] that releases the slot when this future is dropped.
    #[project = LoadShedFutureProj]
    pub enum LoadShedFuture<F> {
        ShortCircuit { response: Option<Response<Body>> },
        Forward {
            #[pin]
            inner: F,
            _guard: Option<InFlightGuard>,
        },
    }
}

impl<F, E> Future for LoadShedFuture<F>
where
    F: Future<Output = Result<Response<Body>, E>>,
{
    type Output = Result<Response<Body>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            LoadShedFutureProj::ShortCircuit { response } => Poll::Ready(Ok(response
                .take()
                .expect("LoadShedFuture polled after completion"))),
            LoadShedFutureProj::Forward { inner, .. } => inner.poll(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use std::sync::atomic::AtomicUsize as StdAtomicUsize;
    use std::time::Duration;
    use tokio::sync::Notify;
    use tower::ServiceExt; // for oneshot

    fn make_app(layer: LoadShedLayer) -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .route("/actuator/health", get(|| async { "healthy" }))
            .route("/live", get(|| async { "live" }))
            .layer(layer)
    }

    async fn status(app: Router, uri: &str) -> axum::http::StatusCode {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    // ── Below ceiling / disabled ──────────────────────────────────────────

    #[tokio::test]
    async fn below_ceiling_passes_through() {
        let layer = LoadShedLayer::new(10, MetricsCollector::new());
        let app = make_app(layer);
        assert_eq!(status(app, "/").await, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn disabled_zero_limit_never_sheds() {
        let layer = LoadShedLayer::new(0, MetricsCollector::new());
        let app = make_app(layer);
        // Fire several requests sequentially; a zero limit must never 503.
        for _ in 0..5 {
            assert_eq!(status(app.clone(), "/").await, axum::http::StatusCode::OK);
        }
    }

    // ── At ceiling: shed with 503 + Retry-After ───────────────────────────

    /// Holds a handler open until told to release, incrementing `entered`
    /// as soon as the handler body starts (which only happens once the
    /// layer has admitted the request) so the test can deterministically
    /// wait for N requests to be in-flight before firing the deciding one.
    async fn blocking_handler(gate: Arc<Notify>, entered: Arc<StdAtomicUsize>) -> &'static str {
        entered.fetch_add(1, Ordering::SeqCst);
        gate.notified().await;
        "released"
    }

    fn make_blocking_app(
        layer: LoadShedLayer,
        gate: Arc<Notify>,
        entered: Arc<StdAtomicUsize>,
    ) -> Router {
        Router::new()
            .route(
                "/block",
                get(move || blocking_handler(gate.clone(), entered.clone())),
            )
            .route("/actuator/health", get(|| async { "healthy" }))
            .route("/live", get(|| async { "live" }))
            .layer(layer)
    }

    async fn wait_for_entered(entered: &Arc<StdAtomicUsize>, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while entered.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("handlers did not reach the expected in-flight count in time");
    }

    #[tokio::test]
    async fn at_ceiling_sheds_with_503_and_retry_after() {
        let metrics = MetricsCollector::new();
        let layer = LoadShedLayer::new(2, metrics.clone());
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer, gate.clone(), entered.clone());

        // Occupy both slots concurrently.
        let mut handles = Vec::new();
        for _ in 0..2 {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .uri("/block")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }));
        }
        wait_for_entered(&entered, 2).await;

        // The third concurrent request must be shed immediately.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/block")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            resp.headers().contains_key(RETRY_AFTER),
            "shed response must carry Retry-After"
        );
        assert_eq!(metrics.snapshot().http.requests_shed_total, 1);

        // Release the held requests; both must have completed successfully.
        gate.notify_waiters();
        for handle in handles {
            assert_eq!(handle.await.unwrap(), axum::http::StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn released_slots_are_reusable() {
        let metrics = MetricsCollector::new();
        let layer = LoadShedLayer::new(1, metrics.clone());
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer.clone(), gate.clone(), entered.clone());

        let held = {
            let app = app.clone();
            tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .uri("/block")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            })
        };
        wait_for_entered(&entered, 1).await;
        assert_eq!(layer.in_flight.load(Ordering::Acquire), 1);

        // Release: the slot must return to the pool.
        gate.notify_waiters();
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);

        tokio::time::timeout(Duration::from_secs(5), async {
            while layer.in_flight.load(Ordering::Acquire) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("in-flight counter should return to 0 after completion");

        // A fresh request must be admitted again (slot was released, not leaked).
        assert_eq!(
            status(app, "/actuator/health").await,
            axum::http::StatusCode::OK
        );
    }

    #[tokio::test]
    async fn cancelled_request_still_releases_its_slot() {
        // A dropped in-flight future (client disconnect / cancellation) must
        // still free its slot via InFlightGuard's Drop — not only the
        // successful-completion path.
        let metrics = MetricsCollector::new();
        let layer = LoadShedLayer::new(1, metrics);
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer.clone(), gate.clone(), entered.clone());

        let fut = app.clone().oneshot(
            Request::builder()
                .uri("/block")
                .body(Body::empty())
                .unwrap(),
        );
        let mut fut = Box::pin(fut);
        // Poll once to admit the request (increments in_flight), then drop
        // the future before it resolves.
        let () = futures::future::poll_fn(|cx| {
            let _ = Pin::new(&mut fut).poll(cx);
            Poll::Ready(())
        })
        .await;
        wait_for_entered(&entered, 1).await;
        assert_eq!(layer.in_flight.load(Ordering::Acquire), 1);
        drop(fut);

        assert_eq!(layer.in_flight.load(Ordering::Acquire), 0);
    }

    // ── Probe / actuator exemption ────────────────────────────────────────

    #[tokio::test]
    async fn actuator_health_bypasses_ceiling() {
        let metrics = MetricsCollector::new();
        let layer = LoadShedLayer::new(1, metrics)
            .with_health_prefix("/actuator")
            .with_probe_paths(vec!["/live".to_owned()]);
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer, gate.clone(), entered.clone());

        let held = {
            let app = app.clone();
            tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .uri("/block")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            })
        };
        wait_for_entered(&entered, 1).await;

        // The single slot is occupied, but probe/actuator paths still 200.
        assert_eq!(
            status(app.clone(), "/actuator/health").await,
            axum::http::StatusCode::OK
        );
        assert_eq!(
            status(app.clone(), "/live").await,
            axum::http::StatusCode::OK
        );

        gate.notify_waiters();
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);
    }
}
