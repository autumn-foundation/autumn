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
//! Disabled entirely when no ceiling resolves (`server.max_concurrent_requests
//! = 0`, or unset outside the `prod` profile with no capacity contract) — see
//! `build_load_shed_layer` (in `router.rs`, private to the crate), which
//! returns `None` in that case so this layer is never applied and there is
//! no overhead. The `prod` profile sets a default ceiling (#3057).
//!
//! The ceiling itself no longer has to be a hand-tuned guess: with
//! `[server] capacity_contract` pointing at a committed `capacity.lock`, it is
//! sourced from the envelope `autumn calibrate` proved for this build on this
//! host class, so the layer sheds at a measured edge rather than an assumed
//! one (issue #1733, `docs/guide/capacity-contracts.md`). An explicit
//! `max_concurrent_requests` still wins, and every contract problem degrades
//! to the profile default (unlimited outside `prod`) — see
//! [`crate::capacity::resolve_admission_limit_with_default`].
//!
//! Adaptive mode (issue #3068, ADR 0016): [`LoadShedLayer::adaptive`] reads
//! the ceiling from an [`AdaptiveLimiter`], and gives it one sample per
//! admitted request. Criticality partitions apply in both modes: a request
//! of class `c` is admitted only while the in-flight count is below
//! [`PartitionShares::threshold`]. The class comes from the [`Criticality`]
//! request extension, which `CriticalityLayer` (in `router.rs`) sets.
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
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, header::RETRY_AFTER};
use axum::response::IntoResponse;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::admission::{AdaptiveLimiter, Criticality, InboundDeadline, PartitionShares, Sample};
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

/// Marks the `/mcp` envelope. The layer gives it an [`EnvelopeAdmission`].
#[derive(Clone, Copy, Debug)]
pub struct LoadShedEnvelope;

/// The envelope's admission, as seen by its `tools/call` replay.
#[derive(Clone, Debug, Default)]
pub struct EnvelopeAdmission(Arc<AtomicBool>);

impl EnvelopeAdmission {
    /// The replay was shed: the envelope must give no limiter sample.
    pub fn skip_sample(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_skipped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Tower [`Layer`] that caps concurrent in-flight requests and sheds the
/// excess with an immediate `503 Service Unavailable`.
///
/// Clone this layer freely — the in-flight counter is shared via [`Arc`].
#[derive(Clone)]
pub struct LoadShedLayer {
    limit: LimitSource,
    shares: PartitionShares,
    in_flight: Arc<AtomicUsize>,
    /// Requests that hold a slot of their own class: direct requests and
    /// `tools/call` replays. An envelope that is not yet classified is not
    /// counted, so a replay burst does not shed itself.
    classified: Arc<AtomicUsize>,
    metrics: MetricsCollector,
    paths: Arc<ExemptPaths>,
    cors: Option<Arc<crate::config::CorsConfig>>,
}

/// Where the ceiling comes from.
#[derive(Clone)]
enum LimitSource {
    /// A fixed ceiling. `0` disables shedding.
    Static(usize),
    /// A ceiling that an [`AdaptiveLimiter`] moves.
    Adaptive(Arc<AdaptiveLimiter>),
}

/// The exempt-path sets, resolved once at router-assembly time.
///
/// Behind an `Arc` because [`LoadShedService`] clones this layer wholesale and
/// is itself cloned on the request path; by-value `String`/`Vec<String>` fields
/// made every such clone deep-copy them (issue #2193).
#[derive(Clone)]
struct ExemptPaths {
    health_prefix: String,
    health_prefix_slash: String,
    probe_paths: Vec<String>,
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
        Self::with_source(LimitSource::Static(limit), metrics)
    }

    /// Create a layer whose ceiling `limiter` sets (issue #3068).
    ///
    /// Each admitted request gives the limiter one [`Sample`] when its
    /// response head is ready. These give no sample, because their latency
    /// does not show capacity:
    ///
    /// - a `4xx` or `503` response (rate limit, not found, maintenance);
    /// - a route with `timeout = "off"` (for example, a long poll);
    /// - a request that the client cancels.
    ///
    /// If the request timeout cancels the request, the layer records the
    /// elapsed time as a drop. Shed and exempt requests give no sample.
    #[must_use]
    pub fn adaptive(limiter: Arc<AdaptiveLimiter>, metrics: MetricsCollector) -> Self {
        metrics.set_admission_limit(limiter.limit());
        Self::with_source(LimitSource::Adaptive(limiter), metrics)
    }

    fn with_source(limit: LimitSource, metrics: MetricsCollector) -> Self {
        if let LimitSource::Static(limit) = limit {
            metrics.set_admission_limit(limit);
        }
        Self {
            limit,
            shares: PartitionShares::default(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            classified: Arc::new(AtomicUsize::new(0)),
            metrics,
            paths: Arc::new(ExemptPaths {
                health_prefix: String::new(),
                health_prefix_slash: String::new(),
                probe_paths: Vec::new(),
            }),
            cors: None,
        }
    }

    /// The share of the ceiling that each [`Criticality`] can fill.
    /// Default: [`PartitionShares::default`].
    #[must_use]
    pub const fn with_partitions(mut self, shares: PartitionShares) -> Self {
        self.shares = shares;
        self
    }

    /// The current ceiling. `0` means no ceiling.
    fn current_limit(&self) -> usize {
        match &self.limit {
            LimitSource::Static(limit) => *limit,
            LimitSource::Adaptive(limiter) => limiter.limit(),
        }
    }

    /// Requests whose path starts with this prefix always pass through,
    /// uncounted (e.g. the actuator prefix).
    #[must_use]
    pub fn with_health_prefix(mut self, prefix: impl Into<String>) -> Self {
        let paths = Arc::make_mut(&mut self.paths);
        paths.health_prefix = prefix.into();
        paths.health_prefix_slash = prefix_with_trailing_slash(&paths.health_prefix);
        self
    }

    /// Exact-match probe paths that always pass through, uncounted (e.g.
    /// `/live`, `/ready`, `/startup`, `/health`).
    #[must_use]
    pub fn with_probe_paths(mut self, paths: Vec<String>) -> Self {
        Arc::make_mut(&mut self.paths).probe_paths = paths;
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
    /// Whether `req` is on a probe or actuator path.
    fn is_exempt<B>(&self, req: &Request<B>) -> bool {
        let path = req.uri().path();
        let prefix_matched = health_prefix_matches(
            path,
            &self.layer.paths.health_prefix,
            &self.layer.paths.health_prefix_slash,
        );
        prefix_matched
            || self
                .layer
                .paths
                .probe_paths
                .iter()
                .any(|probe| probe == path)
    }

    /// Shed `req` as class `criticality`: count it and build the `503`.
    fn shed<B, F>(&self, req: &Request<B>, criticality: Criticality) -> LoadShedFuture<F> {
        self.layer.metrics.record_request_shed_for(criticality);
        // Capture the request Origin before it's dropped, so a
        // mirrored CORS response can echo it back (see with_cors).
        let cors_origin = self
            .layer
            .cors
            .as_ref()
            .and_then(|_| req.headers().get(http::header::ORIGIN).cloned());
        LoadShedFuture::ShortCircuit {
            response: Some(build_shed_response(
                self.layer.cors.as_deref(),
                cors_origin.as_ref(),
            )),
        }
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

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let limit = self.layer.current_limit();
        if limit == 0 || self.is_exempt(&req) {
            return LoadShedFuture::Forward {
                inner: self.inner.call(req),
                guard: None,
            };
        }
        if req.extensions().get::<LoadShedExempt>().is_some() {
            // An outer admission (the `/mcp` envelope) already holds this
            // request's slot, as `critical`, because it did not know the
            // route. Claim a slot at the route's own class now. The claim
            // counts only classified requests, so other unclassified
            // envelopes do not shed this one.
            let Some(&criticality) = req.extensions().get::<Criticality>() else {
                return LoadShedFuture::Forward {
                    inner: self.inner.call(req),
                    guard: None,
                };
            };
            let threshold = self.layer.shares.threshold(criticality, limit);
            if claim(&self.layer.classified, threshold).is_none() {
                // The envelope answers 200, so it must give no sample.
                if let Some(envelope) = req.extensions().get::<EnvelopeAdmission>() {
                    envelope.skip_sample();
                }
                return self.shed(&req, criticality);
            }
            return LoadShedFuture::Forward {
                inner: self.inner.call(req),
                guard: Some(InFlightGuard {
                    in_flight: None,
                    classified: Some(Arc::clone(&self.layer.classified)),
                    sampler: None,
                    envelope: None,
                }),
            };
        }

        let criticality = req
            .extensions()
            .get::<Criticality>()
            .copied()
            .unwrap_or_default();
        let threshold = self.layer.shares.threshold(criticality, limit);
        let in_flight = &self.layer.in_flight;
        let Some(current) = claim(in_flight, threshold) else {
            return self.shed(&req, criticality);
        };
        // An envelope is not classified until its replay claims a slot.
        let envelope = req
            .extensions()
            .get::<LoadShedEnvelope>()
            .is_some()
            .then(EnvelopeAdmission::default);
        if let Some(envelope) = &envelope {
            req.extensions_mut().insert(envelope.clone());
        }
        let classified = envelope.is_none().then(|| {
            self.layer.classified.fetch_add(1, Ordering::AcqRel);
            Arc::clone(&self.layer.classified)
        });

        let deadline = req.extensions().get::<InboundDeadline>().copied();
        let sampler = match (&self.layer.limit, deadline) {
            (LimitSource::Static(_), _) | (_, Some(InboundDeadline::Off)) => None,
            (LimitSource::Adaptive(limiter), deadline) => Some(Sampler {
                limiter: Arc::clone(limiter),
                metrics: self.layer.metrics.clone(),
                start: tokio::time::Instant::now(),
                // The CAS above succeeded, so `current < threshold <= usize::MAX`.
                in_flight: current.saturating_add(1),
                deadline: match deadline {
                    Some(InboundDeadline::At(at)) => Some(at),
                    _ => None,
                },
            }),
        };
        LoadShedFuture::Forward {
            inner: self.inner.call(req),
            guard: Some(InFlightGuard {
                in_flight: Some(Arc::clone(in_flight)),
                classified,
                sampler,
                envelope,
            }),
        }
    }
}

/// Take one slot of `counter` while it is below `threshold`.
///
/// Returns the count before the claim. Lock-free: a CAS loop.
fn claim(counter: &AtomicUsize, threshold: usize) -> Option<usize> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current >= threshold {
            return None;
        }
        // `current < threshold` is checked above, so the bump is exact;
        // `saturating_add` only guards the theoretical `usize::MAX` limit.
        match counter.compare_exchange_weak(
            current,
            current.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(current),
            Err(observed) => current = observed,
        }
    }
}

/// The start of one admitted request, for an adaptive limit.
struct Sampler {
    limiter: Arc<AdaptiveLimiter>,
    metrics: MetricsCollector,
    start: tokio::time::Instant,
    in_flight: usize,
    /// The request deadline, when the request-timeout layer set one.
    deadline: Option<tokio::time::Instant>,
}

impl Sampler {
    /// Give the limiter the sample for this request.
    fn record(self, dropped: bool) {
        let now = tokio::time::Instant::now();
        let sample = Sample {
            rtt: now.saturating_duration_since(self.start),
            in_flight: self.in_flight,
            dropped,
            at: self.limiter.elapsed(now),
        };
        let metrics = &self.metrics;
        let _ = self
            .limiter
            .record_with(sample, |limit| metrics.set_admission_limit(limit));
    }
}

/// Held for the lifetime of an admitted request's inner future; decrements
/// the shared in-flight counter on drop, whether the future resolves
/// normally or is cancelled (dropped) mid-flight — the same guarantee
/// [`crate::middleware::metrics::MetricsFuture`]'s `PinnedDrop` gives
/// `requests_active`.
struct InFlightGuard {
    /// The total count. `None` for a replay: the envelope holds that slot.
    in_flight: Option<Arc<AtomicUsize>>,
    /// The count of requests that hold a slot of their own class. `None`
    /// for an envelope.
    classified: Option<Arc<AtomicUsize>>,
    /// `Some` in adaptive mode until the sample is recorded.
    sampler: Option<Sampler>,
    /// `Some` for an `/mcp` envelope.
    envelope: Option<EnvelopeAdmission>,
}

impl InFlightGuard {
    /// Record the sample for a response. `None` is an inner service error.
    ///
    /// A `504` or an error is a drop. A `4xx` or `503` gives no sample: a
    /// rate limit, a not-found or maintenance mode answers fast and does not
    /// show capacity.
    fn complete(&mut self, status: Option<axum::http::StatusCode>) {
        let Some(sampler) = self.sampler.take() else {
            return;
        };
        if self
            .envelope
            .as_ref()
            .is_some_and(EnvelopeAdmission::is_skipped)
        {
            return;
        }
        match status {
            Some(s) if s.is_client_error() || s == axum::http::StatusCode::SERVICE_UNAVAILABLE => {}
            Some(s) => sampler.record(s == axum::http::StatusCode::GATEWAY_TIMEOUT),
            None => sampler.record(true),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // Cancelled before a response. At the request deadline, this is
        // overload: record the elapsed time as a drop. Before it, the client
        // went away, which does not show capacity.
        if let Some(sampler) = self.sampler.take()
            && !self
                .envelope
                .as_ref()
                .is_some_and(EnvelopeAdmission::is_skipped)
            && sampler
                .deadline
                .is_some_and(|d| tokio::time::Instant::now() >= d)
        {
            sampler.record(true);
        }
        if let Some(classified) = &self.classified {
            classified.fetch_sub(1, Ordering::AcqRel);
        }
        if let Some(in_flight) = &self.in_flight {
            in_flight.fetch_sub(1, Ordering::AcqRel);
        }
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
                if let Some(guard) = guard.as_mut() {
                    guard.complete(output.as_ref().ok().map(Response::status));
                }
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
            .route("/work", get(|| async { "ok" }))
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
            .route("/work", get(|| async { "work" }))
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

    // ── Criticality partitions (#3068) ────────────────────────────────────

    fn request(uri: &str, criticality: Option<crate::admission::Criticality>) -> Request<Body> {
        let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        if let Some(c) = criticality {
            req.extensions_mut().insert(c);
        }
        req
    }

    #[tokio::test]
    async fn sheddable_is_shed_before_critical() {
        use crate::admission::Criticality;
        let metrics = MetricsCollector::new();
        // Limit 2, default shares: sheddable may fill 1 slot.
        let layer = LoadShedLayer::new(2, metrics.clone());
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer, gate.clone(), entered.clone());

        let held = {
            let app = app.clone();
            tokio::spawn(
                async move { app.oneshot(request("/block", None)).await.unwrap().status() },
            )
        };
        wait_for_entered(&entered, 1).await;

        let shed = app
            .clone()
            .oneshot(request("/work", Some(Criticality::Sheddable)))
            .await
            .unwrap();
        assert_eq!(shed.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        let crit = app
            .clone()
            .oneshot(request("/work", Some(Criticality::Critical)))
            .await
            .unwrap();
        assert_eq!(crit.status(), axum::http::StatusCode::OK);

        let snap = metrics.snapshot().http;
        assert_eq!(snap.requests_shed_total, 1);
        assert_eq!(snap.admission.shed_sheddable, 1);
        assert_eq!(snap.admission.shed_critical, 0);

        gate.notify_waiters();
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn custom_shares_reserve_headroom_for_critical() {
        use crate::admission::{Criticality, PartitionShares};
        let layer = LoadShedLayer::new(2, MetricsCollector::new())
            .with_partitions(PartitionShares::new(0.5, 0.0).unwrap());
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer, gate.clone(), entered.clone());

        let held = {
            let app = app.clone();
            tokio::spawn(
                async move { app.oneshot(request("/block", None)).await.unwrap().status() },
            )
        };
        wait_for_entered(&entered, 1).await;

        assert_eq!(
            app.clone()
                .oneshot(request("/work", None))
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "default may fill only half the limit"
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/work", Some(Criticality::Critical)))
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::OK
        );
        gate.notify_waiters();
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);
    }

    // ── Adaptive limit (#3068) ────────────────────────────────────────────

    fn aimd_limiter(initial: usize) -> Arc<crate::admission::AdaptiveLimiter> {
        use crate::admission::{AdaptiveLimiter, Aimd, LimitAlgorithm, LimitBounds};
        AdaptiveLimiter::new(LimitAlgorithm::Aimd(Aimd::new(
            LimitBounds::new(1, 100, initial).unwrap(),
            Duration::from_secs(1),
        )))
    }

    #[tokio::test]
    async fn adaptive_limit_grows_on_fast_full_use() {
        let metrics = MetricsCollector::new();
        let limiter = aimd_limiter(2);
        let app = make_app(LoadShedLayer::adaptive(
            Arc::clone(&limiter),
            metrics.clone(),
        ));
        assert_eq!(metrics.snapshot().http.admission.limit, 2);
        // One in flight at limit 2 is full use for AIMD: +1.
        assert_eq!(
            status(app.clone(), "/work").await,
            axum::http::StatusCode::OK
        );
        assert_eq!(limiter.limit(), 3);
        assert_eq!(metrics.snapshot().http.admission.limit, 3);
    }

    #[tokio::test]
    async fn adaptive_limit_backs_off_on_gateway_timeout() {
        let limiter = aimd_limiter(50);
        let app = Router::new()
            .route(
                "/slow-upstream",
                get(|| async { axum::http::StatusCode::GATEWAY_TIMEOUT }),
            )
            .layer(LoadShedLayer::adaptive(
                Arc::clone(&limiter),
                MetricsCollector::new(),
            ));
        assert_eq!(
            status(app, "/slow-upstream").await,
            axum::http::StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(limiter.limit(), 45, "a 504 is a drop: x0.9");
    }

    #[tokio::test]
    async fn adaptive_limit_sheds_at_the_current_limit() {
        let limiter = aimd_limiter(1);
        let layer = LoadShedLayer::adaptive(Arc::clone(&limiter), MetricsCollector::new());
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer, gate.clone(), entered.clone());
        let held = {
            let app = app.clone();
            tokio::spawn(
                async move { app.oneshot(request("/block", None)).await.unwrap().status() },
            )
        };
        wait_for_entered(&entered, 1).await;
        assert_eq!(
            status(app.clone(), "/work").await,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(limiter.limit(), 1, "a shed request is not a sample");
        gate.notify_waiters();
        assert_eq!(held.await.unwrap(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn adaptive_exempt_paths_are_not_sampled() {
        let limiter = aimd_limiter(2);
        let app = make_app(
            LoadShedLayer::adaptive(Arc::clone(&limiter), MetricsCollector::new())
                .with_health_prefix("/actuator"),
        );
        assert_eq!(
            status(app, "/actuator/health").await,
            axum::http::StatusCode::OK
        );
        assert_eq!(limiter.limit(), 2);
    }

    fn hang_app(layer: LoadShedLayer) -> Router {
        Router::new()
            .route(
                "/hang",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    "never"
                }),
            )
            .route(
                "/missing",
                get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .route(
                "/maintenance",
                get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            )
            .layer(layer)
    }

    #[tokio::test(start_paused = true)]
    async fn adaptive_client_cancel_records_nothing() {
        let limiter = aimd_limiter(50);
        let layer = LoadShedLayer::adaptive(Arc::clone(&limiter), MetricsCollector::new());
        let app = hang_app(layer.clone());
        // No deadline: the client went away.
        let res =
            tokio::time::timeout(Duration::from_secs(5), app.oneshot(request("/hang", None))).await;
        assert!(res.is_err());
        assert_eq!(limiter.limit(), 50);
        assert_eq!(layer.in_flight.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn adaptive_client_errors_and_503_are_not_sampled() {
        let limiter = aimd_limiter(1);
        let app = hang_app(LoadShedLayer::adaptive(
            Arc::clone(&limiter),
            MetricsCollector::new(),
        ));
        for uri in ["/missing", "/maintenance"] {
            app.clone().oneshot(request(uri, None)).await.unwrap();
        }
        assert_eq!(
            limiter.limit(),
            1,
            "a fast 404 or 503 must not grow the limit"
        );
    }

    #[tokio::test]
    async fn adaptive_timeout_off_routes_are_not_sampled() {
        let limiter = aimd_limiter(1);
        let app = make_app(LoadShedLayer::adaptive(
            Arc::clone(&limiter),
            MetricsCollector::new(),
        ));
        let mut req = request("/work", None);
        req.extensions_mut().insert(InboundDeadline::Off);
        assert_eq!(
            app.oneshot(req).await.unwrap().status(),
            axum::http::StatusCode::OK
        );
        assert_eq!(limiter.limit(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn adaptive_deadline_cancel_is_a_drop() {
        // The request timeout cancels the request at its 5 s deadline. AIMD
        // must back off.
        let limiter = aimd_limiter(50);
        let layer = LoadShedLayer::adaptive(Arc::clone(&limiter), MetricsCollector::new());
        let app = hang_app(layer.clone());
        let mut req = request("/hang", None);
        req.extensions_mut().insert(InboundDeadline::At(
            tokio::time::Instant::now() + Duration::from_secs(5),
        ));
        let res = tokio::time::timeout(Duration::from_secs(5), app.oneshot(req)).await;
        assert!(res.is_err(), "the timeout cancels the request");
        assert_eq!(limiter.limit(), 45);
        assert_eq!(layer.in_flight.load(Ordering::Acquire), 0);
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

    /// Regression (#3183 review): an MCP `tools/call` replay is already
    /// counted at the envelope as `default`. The replay must still be shed
    /// when its route's class is over its share.
    #[tokio::test]
    async fn exempt_replay_is_rechecked_against_its_class() {
        use crate::admission::Criticality;
        // Limit 2, sheddable share 0.5: one slot for sheddable.
        let layer = LoadShedLayer::new(2, MetricsCollector::new());
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer, gate.clone(), entered.clone());

        // One ordinary request in flight, then the envelope's own slot: the
        // replay below sees in_flight = 2 (simulated by a second holder).
        let mut held = Vec::new();
        for _ in 0..2 {
            let app = app.clone();
            held.push(tokio::spawn(async move {
                let mut req = request("/block", Some(Criticality::Critical));
                req.extensions_mut().insert(LoadShedEnvelope);
                app.oneshot(req).await.unwrap().status()
            }));
        }
        wait_for_entered(&entered, 2).await;

        let replay = |c, uri| {
            let mut req = request(uri, Some(c));
            req.extensions_mut().insert(LoadShedExempt);
            req
        };
        // The first sheddable replay takes the one sheddable slot.
        let first = {
            let app = app.clone();
            tokio::spawn(async move {
                app.oneshot(replay(Criticality::Sheddable, "/block"))
                    .await
                    .unwrap()
                    .status()
            })
        };
        wait_for_entered(&entered, 3).await;
        assert_eq!(
            app.clone()
                .oneshot(replay(Criticality::Sheddable, "/work"))
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "a sheddable tool over its share is shed"
        );
        assert_eq!(
            app.clone()
                .oneshot(replay(Criticality::Critical, "/work"))
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::OK,
            "a critical tool below the limit is not shed"
        );
        gate.notify_waiters();
        assert_eq!(first.await.unwrap(), axum::http::StatusCode::OK);
        for h in held {
            assert_eq!(h.await.unwrap(), axum::http::StatusCode::OK);
        }
    }

    /// Defect 1 (#3186): N+1 concurrent `sheddable` replays shed exactly 1.
    /// Each envelope holds a slot as `critical`; replays must not count the
    /// other replays' envelopes.
    #[tokio::test]
    async fn replay_burst_sheds_only_the_excess() {
        use crate::admission::{Criticality, PartitionShares};
        // Limit 10, sheddable share 0.5: threshold 5.
        let layer = LoadShedLayer::new(10, MetricsCollector::new())
            .with_partitions(PartitionShares::new(1.0, 0.5).unwrap());
        let gate = Arc::new(Notify::new());
        let entered = Arc::new(StdAtomicUsize::new(0));
        let app = make_blocking_app(layer, gate.clone(), entered.clone());

        let mut envelopes = Vec::new();
        for _ in 0..6 {
            let app = app.clone();
            envelopes.push(tokio::spawn(async move {
                let mut req = request("/block", Some(Criticality::Critical));
                req.extensions_mut().insert(LoadShedEnvelope);
                app.oneshot(req).await.unwrap().status()
            }));
        }
        wait_for_entered(&entered, 6).await;

        let mut replays = Vec::new();
        for _ in 0..6 {
            let app = app.clone();
            replays.push(tokio::spawn(async move {
                let mut req = request("/block", Some(Criticality::Sheddable));
                req.extensions_mut().insert(LoadShedExempt);
                app.oneshot(req).await.unwrap().status()
            }));
        }
        wait_for_entered(&entered, 11).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let shed = replays.iter().filter(|h| h.is_finished()).count();
        assert_eq!(shed, 1, "exactly one replay is over the share");
        gate.notify_waiters();
        let mut statuses = Vec::new();
        for h in replays {
            statuses.push(h.await.unwrap());
        }
        assert_eq!(
            statuses
                .iter()
                .filter(|s| **s == axum::http::StatusCode::SERVICE_UNAVAILABLE)
                .count(),
            1
        );
        for h in envelopes {
            assert_eq!(h.await.unwrap(), axum::http::StatusCode::OK);
        }
    }

    /// Defect 2 (#3186): a replay shed gives the envelope no limiter sample.
    #[tokio::test]
    async fn replay_shed_gives_no_limiter_sample() {
        use crate::admission::{Criticality, PartitionShares};
        use std::convert::Infallible;
        let limiter = aimd_limiter(10);
        // Sheddable share 0: every sheddable replay is shed.
        let layer = LoadShedLayer::adaptive(Arc::clone(&limiter), MetricsCollector::new())
            .with_partitions(PartitionShares::new(1.0, 0.0).unwrap());
        let replay_svc = layer
            .clone()
            .layer(tower::service_fn(|_req: Request<Body>| async move {
                Ok::<_, Infallible>(Response::new(Body::empty()))
            }));
        let envelope_svc = layer.layer(tower::service_fn(move |req: Request<Body>| {
            let mut replay_svc = replay_svc.clone();
            async move {
                let handle = req
                    .extensions()
                    .get::<EnvelopeAdmission>()
                    .cloned()
                    .expect("the layer gives the envelope a handle");
                let mut replay = request("/work", Some(Criticality::Sheddable));
                replay.extensions_mut().insert(LoadShedExempt);
                replay.extensions_mut().insert(handle);
                let shed = replay_svc.call(replay).await.unwrap();
                assert_eq!(shed.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
                // MCP turns the shed into an HTTP 200 tool error.
                Ok::<_, Infallible>(Response::new(Body::empty()))
            }
        }));
        let mut req = request("/mcp", Some(Criticality::Critical));
        req.extensions_mut().insert(LoadShedEnvelope);
        let resp = envelope_svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(limiter.limit(), 10, "a shed replay is not a sample");
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
