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
//! `build_load_shed_layer` (in `router.rs`, private to the crate), which
//! returns `None` in that case so this layer is never applied and there is
//! no overhead.
//!
//! The admission gauge is a dedicated counter, independent of
//! [`crate::middleware::MetricsCollector`]'s `requests_active` and the
//! graceful-shutdown drain accounting, so shedding cannot double-count,
//! deadlock, or extend the drain budget.

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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, header::RETRY_AFTER};
use axum::response::IntoResponse;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::middleware::MetricsCollector;
use crate::middleware::maintenance::{health_prefix_matches, prefix_with_trailing_slash};

/// `Retry-After` value (seconds) sent on every shed `503`. Kept short so a
/// client or load balancer retries fast (or fails over to another replica)
/// rather than piling onto the already-loaded process.
const RETRY_AFTER_SECS: &str = "1";

/// Request-extension marker that exempts a request from load-shed admission
/// accounting.
///
/// Set on requests that have already been counted by an upstream
/// `LoadShedLayer` so a shared-counter replay doesn't double-count them. The
/// MCP endpoint uses this: `/mcp`'s outer envelope and its `tools/call`
/// dispatch replay share the same `LoadShedLayer` instance (same `Arc`
/// counter, see `crate::router::build_load_shed_layer`); without this marker
/// a single `tools/call` would acquire one slot at the envelope and a second
/// at the replay, silently halving the effective ceiling for MCP traffic (at
/// `max_concurrent_requests = 1` a solo `tools/call` would even shed itself).
/// The marker keeps the replay from consuming a second slot for the same
/// logical request. It is only ever set internally — external requests
/// cannot carry it, since extensions are not derived from headers.
#[derive(Clone, Copy, Debug)]
pub struct LoadShedExempt;

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
    health_prefix_slash: String,
    probe_paths: Vec<String>,
    cors: Option<Arc<crate::config::CorsConfig>>,
}

impl LoadShedLayer {
    /// Create a layer that admits at most `limit` concurrent requests.
    ///
    /// A `limit` of `0` disables shedding entirely (every request is
    /// forwarded, uncounted) — callers should prefer not constructing this
    /// layer at all when the ceiling is unset (see `build_load_shed_layer`
    /// in `router.rs`), but `0` is handled safely here too so a
    /// misconfigured value never wedges every request shut.
    #[must_use]
    pub fn new(limit: usize, metrics: MetricsCollector) -> Self {
        Self {
            limit,
            in_flight: Arc::new(AtomicUsize::new(0)),
            metrics,
            health_prefix: String::new(),
            health_prefix_slash: String::new(),
            probe_paths: Vec::new(),
            cors: None,
        }
    }

    /// Requests whose path starts with this prefix always pass through,
    /// uncounted (e.g. the actuator prefix).
    #[must_use]
    pub fn with_health_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.health_prefix = prefix.into();
        self.health_prefix_slash = prefix_with_trailing_slash(&self.health_prefix);
        self
    }

    /// Exact-match probe paths that always pass through, uncounted (e.g.
    /// `/live`, `/ready`, `/startup`, `/health`).
    #[must_use]
    pub fn with_probe_paths(mut self, paths: Vec<String>) -> Self {
        self.probe_paths = paths;
        self
    }

    /// CORS config to mirror onto a shed `503`'s headers.
    ///
    /// This layer sits outside `CorsLayer` in the main ingress stack (see
    /// `apply_middleware`), so a shed response never flows back through it;
    /// without mirroring, a cross-origin browser client would see an opaque
    /// CORS failure instead of a readable `503`. `None` (the default) skips
    /// mirroring — harmless when this layer's other application site (the
    /// `/mcp` envelope) sits *inside* its own `CorsLayer`, which then
    /// overwrites these headers with its own regardless.
    #[must_use]
    pub fn with_cors(mut self, cors: Option<Arc<crate::config::CorsConfig>>) -> Self {
        self.cors = cors;
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
        let prefix_matched = health_prefix_matches(
            path,
            &self.layer.health_prefix,
            &self.layer.health_prefix_slash,
        );
        prefix_matched
            || self.layer.probe_paths.iter().any(|probe| probe == path)
            || req.extensions().get::<LoadShedExempt>().is_some()
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
                guard: None,
            };
        }

        let in_flight = &self.layer.in_flight;
        let mut current = in_flight.load(Ordering::Acquire);
        loop {
            if current >= self.layer.limit {
                self.layer.metrics.record_request_shed();
                // Capture the request Origin before it's dropped, so a
                // mirrored CORS response can echo it back (see with_cors).
                let cors_origin = self
                    .layer
                    .cors
                    .as_ref()
                    .and_then(|_| req.headers().get(http::header::ORIGIN).cloned());
                return LoadShedFuture::ShortCircuit {
                    response: Some(build_shed_response(
                        self.layer.cors.as_deref(),
                        cors_origin.as_ref(),
                    )),
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
            guard: Some(InFlightGuard {
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
/// `Retry-After` and any mirrored CORS headers set here).
///
/// When `cors` is configured (see [`LoadShedLayer::with_cors`]), the
/// `Access-Control-*` headers a real `CorsLayer` would have added are
/// mirrored directly onto this response — this layer sits outside
/// `CorsLayer` in the main ingress stack, so without this a cross-origin
/// browser client would see an opaque CORS failure instead of a readable
/// `503`.
fn build_shed_response(
    cors: Option<&crate::config::CorsConfig>,
    origin: Option<&HeaderValue>,
) -> Response<Body> {
    let mut response = crate::error::AutumnError::service_unavailable_msg(
        "Too many concurrent requests; try again shortly.",
    )
    .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECS));
    if let Some(cors) = cors {
        crate::router::mirror_cors_headers(cors, origin, &mut response);
    }
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
            guard: Option<InFlightGuard>,
        },
    }
}

impl<F, E> Future for LoadShedFuture<F>
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
            LoadShedFutureProj::ShortCircuit { response } => Poll::Ready(Ok(response
                .take()
                .expect("LoadShedFuture polled after completion"))),
            LoadShedFutureProj::Forward { inner, guard } => {
                let output = std::task::ready!(inner.poll(cx));
                // Release the slot as soon as the inner future resolves,
                // rather than waiting for this whole future to be dropped —
                // if a caller (middleware combinator, logging, post-
                // processing) holds onto the resolved future, the slot would
                // otherwise stay occupied longer than the request is
                // actually in flight.
                guard.take();
                Poll::Ready(output)
            }
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
            .route("/", get(|| async { "root" }))
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

    #[tokio::test]
    async fn guard_is_released_as_soon_as_inner_future_resolves() {
        // A completed `LoadShedFuture` releases its slot immediately when
        // polled to `Poll::Ready`, rather than waiting for the whole future to
        // be dropped — a caller that holds onto the resolved future (a
        // combinator, logging, post-processing) must not keep the slot
        // occupied any longer than the request was actually in flight.
        let metrics = MetricsCollector::new();
        let layer = LoadShedLayer::new(1, metrics);
        let app = make_app(layer.clone());

        let mut fut =
            Box::pin(app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()));
        let resp = std::future::poll_fn(|cx| Pin::new(&mut fut).poll(cx)).await;
        assert_eq!(resp.unwrap().status(), axum::http::StatusCode::OK);

        // The slot must already be free here, before `fut` is dropped.
        assert_eq!(
            layer.in_flight.load(Ordering::Acquire),
            0,
            "slot must be released on Poll::Ready, not deferred until drop"
        );
    }

    // ── MCP replay exemption (avoids double-counting a tools/call) ────────

    #[tokio::test]
    async fn load_shed_exempt_marker_bypasses_the_ceiling() {
        // A request carrying the `LoadShedExempt` marker must pass through
        // uncounted, even with the single slot already occupied — this is
        // how a `tools/call` replay avoids consuming a second slot for the
        // same logical request already counted at the `/mcp` envelope.
        let metrics = MetricsCollector::new();
        let layer = LoadShedLayer::new(1, metrics);
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

        // The single shared slot is occupied: an ordinary (non-exempt)
        // request to the same route is shed...
        assert_eq!(
            status(app.clone(), "/block").await,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "sanity: the ceiling is actually saturated"
        );

        // ...but a request marked exempt is admitted regardless (proves the
        // marker bypasses accounting rather than merely reserving another
        // slot — this would otherwise deadlock, since the layer's one slot
        // is already held by `held`).
        let mut exempt_req = Request::builder()
            .uri("/block")
            .body(Body::empty())
            .unwrap();
        exempt_req.extensions_mut().insert(LoadShedExempt);
        let exempt_fut = {
            let app = app.clone();
            tokio::spawn(async move { app.oneshot(exempt_req).await.unwrap().status() })
        };
        wait_for_entered(&entered, 2).await;

        gate.notify_waiters();
        assert_eq!(exempt_fut.await.unwrap(), axum::http::StatusCode::OK);
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);
    }

    // ── Probe / actuator exemption ────────────────────────────────────────

    /// An empty `health_prefix` (reachable via `actuator.prefix = ""`) must be
    /// treated the same as `"/"` — matching `MaintenanceService::gate_request`'s
    /// behavior exactly — so the two admission-style gates never disagree on
    /// whether the root path is exempt.
    #[tokio::test]
    async fn empty_health_prefix_exempts_root_path_like_maintenance_does() {
        let metrics = MetricsCollector::new();
        let layer = LoadShedLayer::new(1, metrics)
            .with_health_prefix("")
            .with_probe_paths(vec![]);
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

        // The single slot is occupied, but "/" is exempt via the empty prefix.
        assert_eq!(status(app.clone(), "/").await, axum::http::StatusCode::OK);

        gate.notify_waiters();
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);
    }

    // ── CORS headers mirrored onto a shed 503 ─────────────────────────────

    /// This layer sits outside `CorsLayer` on the main stack, so without
    /// mirroring, a cross-origin browser client would see an opaque CORS
    /// failure instead of a readable 503 (see `with_cors`'s doc comment).
    #[tokio::test]
    async fn shed_503_carries_cors_headers_when_configured() {
        let metrics = MetricsCollector::new();
        let cors = crate::config::CorsConfig {
            allowed_origins: vec!["http://other.example".to_owned()],
            ..Default::default()
        };
        let layer = LoadShedLayer::new(1, metrics).with_cors(Some(Arc::new(cors)));
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

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/block")
                    .header(http::header::ORIGIN, "http://other.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers()
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("http://other.example"),
            "shed 503 must mirror the matching CORS origin"
        );

        gate.notify_waiters();
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);
    }

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
