//! The traffic-mirroring tower layer (issue #1653).
//!
//! # The one invariant
//!
//! **The live request must not be able to tell that mirroring is on.** Every
//! design decision here follows from that:
//!
//! - The shadow request is dispatched on a **detached task** before the primary
//!   handler has even finished, so the two run concurrently and the client
//!   never waits on the candidate.
//! - The primary response body is **teed, not buffered**: frames flow to the
//!   client the instant they are produced and a copy accumulates on the side.
//!   Buffering-then-forwarding would have added the handler's whole body
//!   latency to every mirrored request.
//! - The shadow response is read into [`ResponseFacts`] inside the detached
//!   task and dropped there. It is never a `Response`, so no code path exists
//!   on which it could be returned.
//! - Every failure mode of the mirror (transport error, deadline, oversize
//!   body, a full in-flight ceiling) resolves to *a counter and nothing else*.
//!
//! # What bounds it
//!
//! `max_in_flight` reserves a slot before dispatch and releases it when the
//! detached task ends, so a candidate that stops answering costs at most that
//! many outstanding requests rather than one per inbound request. `timeout`
//! bounds each attempt. `max_body_bytes` bounds what either side may buffer —
//! an oversize body is not partially captured, it is abandoned and counted, so
//! a streaming endpoint cannot grow the process.

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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, Response};
use bytes::{Bytes, BytesMut};
use pin_project_lite::pin_project;
use tokio::sync::oneshot;
use tower::{Layer, Service};

use crate::entropy::Entropy;
use crate::log::filter::ParameterFilter;
use crate::shadow::diff::{
    Comparison, DivergenceKind, ResponseFacts, compare, redact_path_and_query,
};
use crate::shadow::registry::{RequestContext, ShadowRegistry};
use crate::shadow::sample::{MirrorDecision, MirrorSelector, roll_from};
use crate::shadow::transport::{
    ShadowError, ShadowRequest, ShadowTransport, forwarded_headers, shadow_url,
};
use crate::time::ClockSource;

/// Metric recording every mirrored request's outcome, labelled by the bounded
/// route label and one of `match` / `diverged` / `error` / `timeout` /
/// `skipped`.
pub const COMPARISONS_METRIC: &str = "autumn_shadow_comparisons_total";

/// Metric recording divergences only, labelled by route and divergence kind.
///
/// Redundant with the `diverged` outcome above by design: this is the series an
/// operator alerts on, and it stays at zero on a clean run.
pub const DIVERGENCES_METRIC: &str = "autumn_shadow_divergences_total";

/// Bounds and destination for a mirror run.
#[derive(Clone, Debug)]
pub struct MirrorSettings {
    /// Base URL of the candidate build, without a trailing slash.
    pub target_base: String,
    /// Deadline for one shadow request.
    pub timeout: Duration,
    /// Ceiling on concurrently in-flight mirrored requests.
    pub max_in_flight: usize,
    /// Largest response body either side may buffer for comparison.
    pub max_body_bytes: usize,
    /// Character budget for each recorded JSON sample.
    pub max_sample_bytes: usize,
}

/// Everything the mirror needs, resolved once at router-assembly time.
struct MirrorContext {
    settings: MirrorSettings,
    selector: MirrorSelector,
    registry: ShadowRegistry,
    transport: Arc<dyn ShadowTransport>,
    filter: Arc<ParameterFilter>,
    entropy: Arc<dyn Entropy>,
    clock: Arc<dyn ClockSource>,
    in_flight: Arc<AtomicUsize>,
}

impl std::fmt::Debug for MirrorContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorContext")
            .field("settings", &self.settings)
            .field("in_flight", &self.in_flight.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Tower [`Layer`] that mirrors eligible traffic to a shadow build and diffs
/// the responses.
///
/// Clone it freely — all state is shared through [`Arc`], including the
/// in-flight counter, so every clone admits against the same ceiling.
#[derive(Clone, Debug)]
pub struct ShadowMirrorLayer {
    ctx: Arc<MirrorContext>,
}

impl ShadowMirrorLayer {
    /// Assemble the layer.
    ///
    /// `entropy` and `clock` are injected rather than read from the ambient
    /// process so a [`#[sim_test]`](crate::sim_test) can make the sampling
    /// decision and the recorded timestamps reproducible.
    #[must_use]
    pub fn new(
        settings: MirrorSettings,
        selector: MirrorSelector,
        registry: ShadowRegistry,
        transport: Arc<dyn ShadowTransport>,
        filter: Arc<ParameterFilter>,
        entropy: Arc<dyn Entropy>,
        clock: Arc<dyn ClockSource>,
    ) -> Self {
        crate::metrics::describe_counter(
            COMPARISONS_METRIC,
            "Mirrored requests by route and comparison outcome",
        );
        crate::metrics::describe_counter(
            DIVERGENCES_METRIC,
            "Primary/shadow response divergences by route and kind",
        );
        Self {
            ctx: Arc::new(MirrorContext {
                settings,
                selector,
                registry,
                transport,
                filter,
                entropy,
                clock,
                in_flight: Arc::new(AtomicUsize::new(0)),
            }),
        }
    }
}

impl<S> Layer<S> for ShadowMirrorLayer {
    type Service = ShadowMirrorService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ShadowMirrorService {
            inner,
            ctx: Arc::clone(&self.ctx),
        }
    }
}

/// The [`ShadowMirrorLayer`]'s service.
#[derive(Clone, Debug)]
pub struct ShadowMirrorService<S> {
    inner: S,
    ctx: Arc<MirrorContext>,
}

impl<S, ReqBody> Service<Request<ReqBody>> for ShadowMirrorService<S>
where
    S: Service<Request<ReqBody>, Response = Response<Body>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = ShadowMirrorFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let target = request_target(&req);
        let ctx = Arc::clone(&self.ctx);
        let decision = self
            .ctx
            .selector
            .decide(req.method(), &target, req.headers(), || {
                roll_from(ctx.entropy.as_ref())
            });

        let primary_tx = match decision {
            MirrorDecision::Mirror => {
                self.ctx
                    .begin_mirror(req.method().clone(), &target, req.headers())
            }
            // Not mirrored: the request is forwarded with no wrapper at all, so
            // the overwhelmingly common path costs one decision and nothing
            // else — no body wrapper, no allocation, no metric.
            MirrorDecision::Skip(_) => None,
        };

        ShadowMirrorFuture {
            inner: self.inner.call(req),
            primary_tx,
            max_body_bytes: self.ctx.settings.max_body_bytes,
        }
    }
}

impl MirrorContext {
    /// Reserve a slot, dispatch the shadow request on a detached task, and
    /// return the sender the response tee will deliver the primary facts on.
    ///
    /// `None` means "do not tee": either the ceiling was reached (counted) or
    /// there is nothing to compare against.
    fn begin_mirror(
        self: &Arc<Self>,
        method: Method,
        target: &str,
        headers: &HeaderMap,
    ) -> Option<oneshot::Sender<Option<ResponseFacts>>> {
        let Some(permit) =
            InFlightPermit::try_acquire(&self.in_flight, self.settings.max_in_flight)
        else {
            // The candidate is not keeping up. Drop the mirror rather than
            // queueing it: a queue would turn a slow shadow into unbounded
            // memory growth on the *primary*, which is the one thing this
            // feature must never do.
            self.registry.record_dropped_at_capacity();
            record_outcome(self.selector.route_label(target), "dropped");
            return None;
        };

        let request = ShadowRequest {
            method,
            url: shadow_url(&self.settings.target_base, target),
            headers: forwarded_headers(headers),
        };
        let context = RequestContext {
            method: request.method.to_string(),
            target: redact_path_and_query(target, &self.filter),
            route: self.selector.route_label(target).to_owned(),
        };

        // Mirroring needs somewhere to run the detached task. There always is
        // one on a served request, but a caller driving the router from a
        // non-tokio executor would otherwise make `tokio::spawn` panic on the
        // request path — which is precisely what this module's panic gate
        // exists to prevent. No runtime means no mirror, not a broken request.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                target: "autumn::shadow",
                "no tokio runtime on this request; skipping the mirror"
            );
            return None;
        };

        self.registry.record_mirrored();

        let (tx, rx) = oneshot::channel();
        let ctx = Arc::clone(self);
        runtime.spawn(async move {
            run_mirror(ctx, permit, context, request, rx).await;
        });
        Some(tx)
    }
}

/// Dispatch one shadow request, await the primary's teed body, compare, record.
async fn run_mirror(
    ctx: Arc<MirrorContext>,
    permit: InFlightPermit,
    context: RequestContext,
    request: ShadowRequest,
    primary_rx: oneshot::Receiver<Option<ResponseFacts>>,
) {
    // The permit is released when this task ends, whatever the outcome.
    let _permit = permit;

    let shadow = match tokio::time::timeout(ctx.settings.timeout, ctx.transport.send(request)).await
    {
        Err(_) | Ok(Err(ShadowError::Timeout)) => {
            ctx.registry.record_shadow_timeout();
            record_outcome(&context.route, "timeout");
            return;
        }
        Ok(Err(ShadowError::Oversize)) => {
            ctx.registry.record_skipped_oversize();
            record_outcome(&context.route, "skipped");
            return;
        }
        Ok(Err(error)) => {
            ctx.registry.record_shadow_error();
            record_outcome(&context.route, "error");
            tracing::debug!(target: "autumn::shadow", route = %context.route, %error, "shadow request failed");
            return;
        }
        Ok(Ok(facts)) => facts,
    };

    // `Err` means the primary body never completed — the client disconnected or
    // the response was aborted mid-stream. There is no primary response to
    // compare against, so nothing is recorded.
    let Ok(primary) = primary_rx.await else {
        return;
    };

    // `None` means the primary body blew the capture budget; a shadow body may
    // do the same.
    let (Some(primary), true) = (primary, shadow.body.len() <= ctx.settings.max_body_bytes) else {
        ctx.registry.record_skipped_oversize();
        record_outcome(&context.route, "skipped");
        return;
    };

    let comparison = compare(
        &primary,
        &shadow,
        &ctx.filter,
        ctx.settings.max_sample_bytes,
    );
    record_outcome(&context.route, comparison.outcome_label());
    if let Comparison::Diverged(divergence) = &comparison {
        record_divergence(&context.route, divergence.kind);
        tracing::warn!(
            target: "autumn::shadow",
            route = %context.route,
            method = %context.method,
            request_target = %context.target,
            kind = divergence.kind.as_str(),
            fingerprint = %divergence.fingerprint,
            primary_status = divergence.primary_status,
            shadow_status = divergence.shadow_status,
            "shadow build diverged from the live build"
        );
    }
    let observed_at_ms = ctx.clock.now().timestamp_millis().unsigned_abs();
    ctx.registry
        .record_comparison(&context, comparison, observed_at_ms);
}

fn record_outcome(route: &str, outcome: &'static str) {
    crate::metrics::counter(COMPARISONS_METRIC)
        .with_label("route", route.to_owned())
        .with_label("outcome", outcome)
        .increment(1);
}

fn record_divergence(route: &str, kind: DivergenceKind) {
    crate::metrics::counter(DIVERGENCES_METRIC)
        .with_label("route", route.to_owned())
        .with_label("kind", kind.as_str())
        .increment(1);
}

/// The request target (`/path?query`) mirroring replays.
fn request_target<B>(req: &Request<B>) -> String {
    req.uri()
        .path_and_query()
        .map_or_else(|| req.uri().path().to_owned(), |pq| pq.as_str().to_owned())
}

/// A reserved in-flight slot, released on drop.
#[derive(Debug)]
struct InFlightPermit {
    counter: Arc<AtomicUsize>,
}

impl InFlightPermit {
    /// Reserve a slot, or `None` when the ceiling is already reached.
    fn try_acquire(counter: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        let mut current = counter.load(Ordering::Acquire);
        loop {
            if current >= limit {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                current.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Self {
                        counter: Arc::clone(counter),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for InFlightPermit {
    fn drop(&mut self) {
        let _ = self
            .counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(1))
            });
    }
}

pin_project! {
    /// Future returned by [`ShadowMirrorService`].
    ///
    /// When the request was selected for mirroring it wraps the response body
    /// in the tee; otherwise it is a transparent pass-through.
    pub struct ShadowMirrorFuture<F> {
        #[pin]
        inner: F,
        primary_tx: Option<oneshot::Sender<Option<ResponseFacts>>>,
        max_body_bytes: usize,
    }
}

impl<F, E> Future for ShadowMirrorFuture<F>
where
    F: Future<Output = Result<Response<Body>, E>>,
{
    type Output = Result<Response<Body>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let response = std::task::ready!(this.inner.poll(cx))?;
        let Some(tx) = this.primary_tx.take() else {
            return Poll::Ready(Ok(response));
        };
        Poll::Ready(Ok(tee_response(response, tx, *this.max_body_bytes)))
    }
}

/// Wrap a response body so a copy of its bytes reaches `tx` once the client has
/// been served the last frame.
fn tee_response(
    response: Response<Body>,
    tx: oneshot::Sender<Option<ResponseFacts>>,
    max_body_bytes: usize,
) -> Response<Body> {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    let (parts, body) = response.into_parts();

    // A body that is already at end-of-stream (a `HEAD`, a `204`) may never be
    // polled at all, so there would be no poll on which to deliver the facts.
    // Deliver them now and leave the body untouched.
    if http_body::Body::is_end_stream(&body) {
        let _ = tx.send(Some(ResponseFacts::new(status, content_type, Bytes::new())));
        return Response::from_parts(parts, body);
    }

    let teed = Body::new(MirrorTeeBody {
        inner: body,
        status,
        content_type,
        captured: BytesMut::new(),
        max_body_bytes,
        overflowed: false,
        tx: Some(tx),
    });
    Response::from_parts(parts, teed)
}

/// Response body that copies frames to the differ on their way to the client.
///
/// Every method delegates, so the client sees exactly the body it would have
/// seen without mirroring — same frames, same order, same size hint.
struct MirrorTeeBody {
    inner: Body,
    status: u16,
    content_type: Option<String>,
    captured: BytesMut,
    max_body_bytes: usize,
    overflowed: bool,
    tx: Option<oneshot::Sender<Option<ResponseFacts>>>,
}

impl MirrorTeeBody {
    fn capture(&mut self, data: &Bytes) {
        if self.overflowed {
            return;
        }
        if self.captured.len().saturating_add(data.len()) > self.max_body_bytes {
            // Abandon rather than truncate: half a body would diff against
            // half another body and manufacture divergences that are not real.
            self.overflowed = true;
            self.captured = BytesMut::new();
            return;
        }
        self.captured.extend_from_slice(data);
    }

    fn finish(&mut self) {
        let Some(tx) = self.tx.take() else {
            return;
        };
        let facts = if self.overflowed {
            None
        } else {
            Some(ResponseFacts::new(
                self.status,
                self.content_type.clone(),
                std::mem::take(&mut self.captured).freeze(),
            ))
        };
        let _ = tx.send(facts);
    }
}

impl http_body::Body for MirrorTeeBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let data = data.clone();
                    this.capture(&data);
                }
                // A body that announces its end alongside its last frame is
                // finished here; the consumer is entitled not to poll again.
                if http_body::Body::is_end_stream(&this.inner) {
                    this.finish();
                }
            }
            Poll::Ready(None) => this.finish(),
            // A body that errored mid-stream never reached the client whole,
            // so there is nothing comparable. Dropping the sender cancels the
            // waiting mirror task.
            Poll::Ready(Some(Err(_))) => {
                this.tx = None;
            }
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::Body::size_hint(&self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entropy::SeededEntropy;
    use crate::log::filter::ParameterFilter;
    use crate::shadow::registry::ShadowRegistry;
    use crate::shadow::sample::{MirrorSelector, SHADOW_HEADER};
    use crate::shadow::transport::{ShadowError, ShadowFuture, ShadowRequest, ShadowTransport};
    use crate::time::FixedClock;
    use axum::body::Body;
    use axum::http::{Request, Response, StatusCode};
    use std::sync::Mutex;
    use std::time::Duration;
    use tower::{ServiceExt, service_fn};

    /// What a [`FakeTransport`] should do with each mirrored request.
    #[derive(Clone)]
    enum Behaviour {
        Reply { status: u16, body: &'static str },
        Fail,
        Oversize,
        Stall(Duration),
    }

    #[derive(Debug)]
    struct FakeTransport {
        seen: Arc<Mutex<Vec<ShadowRequest>>>,
        behaviour: Mutex<Behaviour>,
    }

    impl FakeTransport {
        fn new(behaviour: Behaviour) -> Arc<Self> {
            Arc::new(Self {
                seen: Arc::new(Mutex::new(Vec::new())),
                behaviour: Mutex::new(behaviour),
            })
        }

        fn seen(&self) -> Vec<ShadowRequest> {
            self.seen.lock().expect("lock").clone()
        }
    }

    impl std::fmt::Debug for Behaviour {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Behaviour")
        }
    }

    impl ShadowTransport for FakeTransport {
        fn send(&self, request: ShadowRequest) -> ShadowFuture {
            self.seen.lock().expect("lock").push(request);
            let behaviour = self.behaviour.lock().expect("lock").clone();
            Box::pin(async move {
                match behaviour {
                    Behaviour::Reply { status, body } => Ok(ResponseFacts::new(
                        status,
                        Some("application/json".to_owned()),
                        bytes::Bytes::from_static(body.as_bytes()),
                    )),
                    Behaviour::Fail => Err(ShadowError::Transport("refused".to_owned())),
                    Behaviour::Oversize => Err(ShadowError::Oversize),
                    Behaviour::Stall(duration) => {
                        tokio::time::sleep(duration).await;
                        Ok(ResponseFacts::new(200, None, bytes::Bytes::new()))
                    }
                }
            })
        }
    }

    fn settings() -> MirrorSettings {
        MirrorSettings {
            target_base: "http://shadow.invalid".to_owned(),
            timeout: Duration::from_millis(100),
            max_in_flight: 4,
            max_body_bytes: 1024,
            max_sample_bytes: 512,
        }
    }

    fn layer(
        transport: Arc<dyn ShadowTransport>,
        registry: &ShadowRegistry,
        settings: MirrorSettings,
    ) -> ShadowMirrorLayer {
        ShadowMirrorLayer::new(
            settings,
            MirrorSelector::new(1.0, &[], "/actuator", &[]),
            registry.clone(),
            transport,
            Arc::new(ParameterFilter::default()),
            Arc::new(SeededEntropy::new(7)),
            Arc::new(FixedClock::at(chrono::Utc::now())),
        )
    }

    /// A primary handler that always answers with `body`.
    fn primary(
        body: &'static str,
    ) -> impl tower::Service<
        Request<Body>,
        Response = Response<Body>,
        Error = std::convert::Infallible,
    > + Clone {
        service_fn(move |_req: Request<Body>| async move {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("valid response"),
            )
        })
    }

    async fn read_body(response: Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Wait until `condition` holds or ~2 s elapse. The mirror runs on a
    /// detached task, so tests observe its effects rather than awaiting it.
    async fn settle(mut condition: impl FnMut() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_mutating_request_is_never_mirrored() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "{}",
        });
        let registry = ShadowRegistry::new(10);
        let service =
            layer(transport.clone(), &registry, settings()).layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .method("POST")
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_body(response).await, r#"{"ok":true}"#);

        assert!(transport.seen().is_empty());
        assert_eq!(registry.stats().mirrored, 0);
    }

    #[tokio::test]
    async fn an_eligible_request_is_replayed_against_the_target() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let service =
            layer(transport.clone(), &registry, settings()).layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .uri("/api/orders?page=2")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(read_body(response).await, r#"{"ok":true}"#);

        settle(|| !transport.seen().is_empty()).await;
        let seen = transport.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url, "http://shadow.invalid/api/orders?page=2");
        assert_eq!(seen[0].method, axum::http::Method::GET);
        assert!(seen[0].headers.contains_key(SHADOW_HEADER));

        settle(|| registry.stats().compared == 1).await;
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 1);
        assert_eq!(stats.matched, 1);
        assert_eq!(stats.diverged, 0);
    }

    #[tokio::test]
    async fn a_divergent_shadow_never_reaches_the_client_but_is_recorded() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings())
            .layer(primary(r#"{"ok":true,"total":42}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let body = read_body(response).await;
        assert_eq!(
            body, r#"{"ok":true,"total":42}"#,
            "the client must receive the primary body, untouched"
        );

        settle(|| registry.stats().diverged == 1).await;
        let recent = registry.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].divergence.kind, DivergenceKind::Body);
        assert_eq!(recent[0].target, "/api/orders");
    }

    #[tokio::test]
    async fn a_failing_shadow_is_counted_and_leaves_the_client_alone() {
        let transport = FakeTransport::new(Behaviour::Fail);
        let registry = ShadowRegistry::new(10);
        let service =
            layer(transport.clone(), &registry, settings()).layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_body(response).await, r#"{"ok":true}"#);

        settle(|| registry.stats().shadow_errors == 1).await;
        assert_eq!(registry.stats().shadow_errors, 1);
        assert_eq!(registry.stats().compared, 0);
    }

    #[tokio::test]
    async fn a_stalled_shadow_is_abandoned_without_delaying_the_client() {
        let transport = FakeTransport::new(Behaviour::Stall(Duration::from_secs(30)));
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.timeout = Duration::from_millis(50);
        let service =
            layer(transport.clone(), &registry, settings).layer(primary(r#"{"ok":true}"#));

        let started = std::time::Instant::now();
        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let body = read_body(response).await;
        let elapsed = started.elapsed();

        assert_eq!(body, r#"{"ok":true}"#);
        assert!(
            elapsed < Duration::from_secs(5),
            "the client waited {elapsed:?} on a stalled shadow"
        );

        settle(|| registry.stats().shadow_timeouts == 1).await;
        assert_eq!(registry.stats().shadow_timeouts, 1);
    }

    #[tokio::test]
    async fn the_in_flight_ceiling_drops_excess_mirrors() {
        let transport = FakeTransport::new(Behaviour::Stall(Duration::from_secs(30)));
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.max_in_flight = 1;
        settings.timeout = Duration::from_secs(10);
        let mirror = layer(transport.clone(), &registry, settings);

        for _ in 0..4 {
            let service = mirror.clone().layer(primary(r#"{"ok":true}"#));
            let request = Request::builder()
                .uri("/api/orders")
                .body(Body::empty())
                .expect("request");
            let response = service.oneshot(request).await.expect("response");
            let _ = read_body(response).await;
        }

        settle(|| registry.stats().dropped_at_capacity >= 3).await;
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 1, "only one mirror may be in flight");
        assert_eq!(stats.dropped_at_capacity, 3);
    }

    #[tokio::test]
    async fn an_oversized_primary_body_is_skipped_rather_than_buffered() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "{}",
        });
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.max_body_bytes = 16;
        let service = layer(transport.clone(), &registry, settings)
            .layer(primary(r#"{"padding":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(
            read_body(response).await,
            r#"{"padding":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
            "an oversized body must still stream to the client in full"
        );

        settle(|| registry.stats().skipped_oversize == 1).await;
        assert_eq!(registry.stats().skipped_oversize, 1);
        assert_eq!(registry.stats().compared, 0);
    }

    #[tokio::test]
    async fn an_oversized_shadow_body_is_skipped() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"padding":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
        });
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.max_body_bytes = 16;
        let service = layer(transport.clone(), &registry, settings).layer(primary("{}"));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle(|| registry.stats().skipped_oversize == 1).await;
        assert_eq!(registry.stats().skipped_oversize, 1);
        assert_eq!(registry.stats().compared, 0);
    }

    #[tokio::test]
    async fn a_transport_reported_oversize_body_is_skipped_not_errored() {
        // The real transport stops reading a candidate body once it passes the
        // budget, so the caller never sees its length — it sees this error.
        let transport = FakeTransport::new(Behaviour::Oversize);
        let registry = ShadowRegistry::new(10);
        let service =
            layer(transport.clone(), &registry, settings()).layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(read_body(response).await, r#"{"ok":true}"#);

        settle(|| registry.stats().skipped_oversize == 1).await;
        let stats = registry.stats();
        assert_eq!(stats.skipped_oversize, 1);
        assert_eq!(
            stats.shadow_errors, 0,
            "oversize is not a transport failure"
        );
        assert_eq!(stats.compared, 0);
    }

    #[tokio::test]
    async fn the_recorded_target_redacts_sensitive_query_parameters() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "{}",
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(primary(r#"{"a":1}"#));

        let request = Request::builder()
            .uri("/api/orders?token=supersecret&page=2")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle(|| registry.stats().diverged == 1).await;
        let recent = registry.recent();
        assert_eq!(recent.len(), 1);
        assert!(
            !recent[0].target.contains("supersecret"),
            "recorded target leaked a secret: {}",
            recent[0].target
        );
        assert!(recent[0].target.contains("page=2"));
    }

    #[tokio::test]
    async fn an_empty_primary_body_still_compares() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 204,
            body: "",
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(service_fn(
            |_req: Request<Body>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::NO_CONTENT)
                        .body(Body::empty())
                        .expect("valid response"),
                )
            },
        ));

        let request = Request::builder()
            .method("HEAD")
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle(|| registry.stats().compared == 1).await;
        assert_eq!(registry.stats().matched, 1);
    }
}
