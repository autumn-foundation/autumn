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
//! is one deadline per mirror. It covers the shadow request, the primary body
//! wait and the comparison. So `max_in_flight` bounds outstanding *mirrors*,
//! end to end (issue #2333). The comparison runs on the blocking pool. At the
//! deadline it is counted as `abandoned` and records nothing. It keeps its
//! thread until its current step ends. A second cap, twice `max_in_flight`,
//! limits those threads. `max_body_bytes` bounds what either side may buffer —
//! an oversize body is not partially captured, it is abandoned and counted, so
//! a streaming endpoint cannot grow the process.

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
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
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
use crate::shadow::registry::{Recorded, RequestContext, ShadowRegistry};
use crate::shadow::sample::{MirrorDecision, MirrorSelector, SkipReason, roll_from};
use crate::shadow::transport::{
    ShadowError, ShadowRequest, ShadowTransport, forwarded_headers, shadow_url,
};
use crate::time::ClockSource;

/// Built-in metric family recording every mirrored request's outcome.
///
/// Rendered by the actuator's Prometheus endpoint from
/// [`ShadowRegistry::comparisons_by_route`], not through
/// [`crate::metrics::counter`]: the `autumn_` namespace belongs to the
/// framework's built-in families and the public facade correctly refuses it.
///
/// Labelled by route and one
/// of `match`, `diverged`, `error`, `timeout`, `skipped` (a body over the
/// capture budget), `dropped` (the in-flight ceiling was full), `refused` (the
/// live build answered `429`/`503`, so the request never reached a handler), or
/// `incomplete` (the client never finished reading the primary response),
/// `abandoned` (the comparison did not finish by the deadline), or
/// `primary_error` (the LIVE build's own response could not be decoded — not a
/// candidate failure, and counted apart from one).
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
    /// Comparisons still on the blocking pool, abandoned ones included.
    blocking: Arc<AtomicUsize>,
    /// Blocking time added to each comparison, so tests can model a large body.
    #[cfg(test)]
    compare_delay: Duration,
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
                blocking: Arc::new(AtomicUsize::new(0)),
                #[cfg(test)]
                compare_delay: Duration::ZERO,
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
    // `decide` reads the body's end-of-stream signal to catch undeclared bodies (#2332).
    ReqBody: http_body::Body,
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
        let decision = self.ctx.selector.decide(
            req.method(),
            &target,
            req.headers(),
            // Only a body already at end-of-stream (a normal bodiless GET) may mirror (#2332).
            http_body::Body::is_end_stream(req.body()),
            || roll_from(ctx.entropy.as_ref()),
        );

        let pending = match decision {
            MirrorDecision::Mirror => Some(PendingMirror {
                method: req.method().clone(),
                // The trusted-proxy layer runs outside this one, so its verdict is already here.
                headers: forwarded_headers(
                    req.headers(),
                    req.extensions()
                        .get::<crate::security::ResolvedClientIdentity>(),
                ),
                // `Router::layer` wraps each route, so `MatchedPath` is already set. It is a
                // bounded label; the raw path is not, so it is never the fallback.
                route: req
                    .extensions()
                    .get::<axum::extract::MatchedPath>()
                    .map_or_else(
                        || self.ctx.selector.route_label(&target).to_owned(),
                        |matched| matched.as_str().to_owned(),
                    ),
                target,
            }),
            // Not mirrored: no wrapper, no allocation. Conditional skips are counted because they
            // are coverage the mirror silently gives up on cache-heavy routes (#2335); the other
            // skip reasons are configuration facts.
            MirrorDecision::Skip(reason) => {
                if reason == SkipReason::Conditional {
                    self.ctx.registry.record_skipped_conditional();
                }
                tracing::trace!(
                    target: "autumn::shadow",
                    reason = reason.as_str(),
                    "request not mirrored"
                );
                None
            }
        };

        ShadowMirrorFuture {
            inner: self.inner.call(req),
            ctx: Arc::clone(&self.ctx),
            pending,
        }
    }
}

/// A request that cleared the mirroring gates, held until its primary response
/// exists.
///
/// Dispatch is deliberately deferred to the response path. Mirroring is
/// fire-and-forget, so nothing is gained by racing the handler — and this layer
/// sits *outside* every admission-control layer (load shed, maintenance mode,
/// rate limiting, trusted-host, CORS). Dispatching on the request path would
/// therefore replay traffic the live build is in the middle of refusing: the
/// candidate, under none of those pressures, answers `200`, and every mirrored
/// request during a maintenance window or a shed becomes a `status_class`
/// divergence. Waiting for the status lets those be skipped, and spares the
/// candidate the amplification at exactly the moment the replica is shedding
/// because it is over capacity.
struct PendingMirror {
    method: Method,
    target: String,
    route: String,
    headers: HeaderMap,
}

impl MirrorContext {
    /// Reserve a slot, dispatch the shadow request on a detached task, and
    /// return the sender the response tee will deliver the primary facts on.
    ///
    /// `None` means "do not tee": either the ceiling was reached (counted) or
    /// there is nothing to compare against.
    fn begin_mirror(
        self: &Arc<Self>,
        pending: PendingMirror,
    ) -> Option<oneshot::Sender<Option<ResponseFacts>>> {
        // A mirror is not a handler effect or a recorded outbound call: a replay starts none
        // (#2351).
        #[cfg(feature = "reporting")]
        // `current_tape`, not `tape_active`: only the reporting layer marks the replay scope.
        if crate::capsule::effects::current_tape().is_some() {
            return None;
        }
        #[cfg(feature = "http-client")]
        if crate::http_client::outbound_blocked_for_replay() {
            return None;
        }
        let Some(permit) =
            InFlightPermit::try_acquire(&self.in_flight, self.settings.max_in_flight)
        else {
            // Drop rather than queue: a queue would turn a slow shadow into unbounded memory
            // growth on the primary.
            self.registry.record_dropped_at_capacity();
            record_outcome(&self.registry, &pending.route, "dropped");
            return None;
        };

        let request = ShadowRequest {
            method: pending.method,
            url: shadow_url(&self.settings.target_base, &pending.target),
            headers: pending.headers,
        };
        let context = RequestContext {
            method: request.method.to_string(),
            target: redact_path_and_query(&pending.target, &self.filter),
            route: pending.route,
        };

        // A caller driving the router from a non-tokio executor would make `tokio::spawn` panic
        // on the request path. No runtime means no mirror.
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
    let _permit = permit;

    // One deadline covers every stage (shadow request, primary body, comparison). Independent
    // timeouts would let a mirror hold a slot for a multiple of `timeout_ms`.
    let now = tokio::time::Instant::now();
    // `Instant + Duration` panics on overflow; an absurd `timeout_ms` becomes an immediate
    // deadline instead.
    let deadline = now.checked_add(ctx.settings.timeout).unwrap_or(now);

    let shadow = match tokio::time::timeout_at(deadline, ctx.transport.send(request)).await {
        Err(_) | Ok(Err(ShadowError::Timeout)) => {
            ctx.registry.record_shadow_timeout();
            record_outcome(&ctx.registry, &context.route, ShadowError::Timeout.as_str());
            return;
        }
        Ok(Err(ShadowError::Oversize)) => {
            ctx.registry.record_skipped_oversize();
            record_outcome(&ctx.registry, &context.route, "skipped");
            return;
        }
        Ok(Err(error)) => {
            ctx.registry.record_shadow_error();
            record_outcome(&ctx.registry, &context.route, error.as_str());
            tracing::debug!(target: "autumn::shadow", route = %context.route, %error, "shadow request failed");
            return;
        }
        Ok(Ok(facts)) => facts,
    };

    // The primary body resolves when the client finishes reading it, so a slow reader or a
    // long-lived `text/event-stream` would otherwise pin this permit. `Err` on the channel means
    // the client disconnected or the response was aborted mid-stream.
    let Ok(Ok(primary)) = tokio::time::timeout_at(deadline, primary_rx).await else {
        ctx.registry.record_primary_incomplete();
        record_outcome(&ctx.registry, &context.route, "incomplete");
        return;
    };

    // `None` means the primary body exceeded the capture budget; check the shadow body too.
    let (Some(primary), true) = (primary, shadow.body.len() <= ctx.settings.max_body_bytes) else {
        ctx.registry.record_skipped_oversize();
        record_outcome(&ctx.registry, &context.route, "skipped");
        return;
    };

    // Decode, compare and record on the blocking pool under the same deadline: CPU work with no
    // await point can only be bounded by a join under `timeout_at` (#2333). At the deadline the
    // mirror counts as `abandoned` and `state` makes the work stop at its next step.
    //
    // An abandoned job keeps its blocking thread until its step ends, so a second counter, held
    // until the job returns, caps that pile-up.
    let Some(blocking) =
        InFlightPermit::try_acquire(&ctx.blocking, ctx.settings.max_in_flight.saturating_mul(2))
    else {
        ctx.registry.record_comparison_abandoned();
        record_outcome(&ctx.registry, &context.route, "abandoned");
        return;
    };
    let state = Arc::new(AtomicU8::new(RUNNING));
    let mut work = crate::time::spawn_blocking({
        let ctx = Arc::clone(&ctx);
        let context = context.clone();
        let state = Arc::clone(&state);
        move || {
            let _blocking = blocking;
            compare_and_record(&ctx, &context, &state, &primary, &shadow);
        }
    });
    match tokio::time::timeout_at(deadline, &mut work).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            // The work panicked. Count it as a shadow error so `mirrored` accounts for it.
            if commit(&state) {
                ctx.registry.record_shadow_error();
                record_outcome(&ctx.registry, &context.route, "error");
            }
            tracing::debug!(target: "autumn::shadow", route = %context.route, %error, "mirror comparison task failed");
        }
        Err(_) => {
            let won = state
                .compare_exchange(RUNNING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            if won {
                ctx.registry.record_comparison_abandoned();
                record_outcome(&ctx.registry, &context.route, "abandoned");
            }
            // Else the work already began recording. Do not wait: the slot frees at the deadline.
        }
    }
}

/// The mirror work is still running and may record.
const RUNNING: u8 = 0;
/// The deadline fired first; the work must record nothing.
const ABANDONED: u8 = 1;
/// The work won the race and is recording.
const COMMITTED: u8 = 2;

/// Claim the right to record. `false` means the deadline already fired.
fn commit(state: &AtomicU8) -> bool {
    state
        .compare_exchange(RUNNING, COMMITTED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Decode both bodies, compare them and record the result.
///
/// Runs on the blocking pool. It checks `state` between steps and records only
/// after it wins [`commit`].
fn compare_and_record(
    ctx: &MirrorContext,
    context: &RequestContext,
    state: &AtomicU8,
    primary: &ResponseFacts,
    shadow: &ResponseFacts,
) {
    // A job that waited in the pool past the deadline does nothing.
    if state.load(Ordering::Acquire) != RUNNING {
        return;
    }
    // Decode both sides: a precompressed handler makes the primary tee capture encoded bytes too.
    // A body that expands past the budget is a size skip; one that does not decode is an error.
    let primary_body = match decode_side(ctx, primary) {
        Ok(body) => body,
        Err(error) => {
            if commit(state) {
                record_decode_failure(ctx, context, PRIMARY, &error);
            }
            return;
        }
    };
    if state.load(Ordering::Acquire) != RUNNING {
        return;
    }
    let shadow_body = match decode_side(ctx, shadow) {
        Ok(body) => body,
        Err(error) => {
            if commit(state) {
                record_decode_failure(ctx, context, SHADOW, &error);
            }
            return;
        }
    };
    if state.load(Ordering::Acquire) != RUNNING {
        return;
    }
    let primary = ResponseFacts::encoded(
        primary.status,
        primary.content_type.clone(),
        None,
        primary_body,
    );
    let shadow = ResponseFacts::encoded(
        shadow.status,
        shadow.content_type.clone(),
        None,
        shadow_body,
    );

    // Blocking sleep on purpose: this runs on the blocking pool.
    #[cfg(test)]
    std::thread::sleep(ctx.compare_delay);

    let comparison = compare(
        &primary,
        &shadow,
        &ctx.filter,
        ctx.settings.max_sample_bytes,
    );
    if !commit(state) {
        return;
    }
    record_outcome(&ctx.registry, &context.route, comparison.outcome_label());
    let kind = match &comparison {
        Comparison::Match => None,
        Comparison::Diverged(divergence) => Some(divergence.kind),
    };
    if let Some(kind) = kind {
        record_divergence(&ctx.registry, &context.route, kind);
    }

    let observed_at_ms = ctx.clock.now().timestamp_millis().unsigned_abs();
    let recorded = ctx
        .registry
        .record_comparison(context, comparison, observed_at_ms);

    // One WARN per distinct divergence, not per occurrence: a systematic regression diverges on
    // every mirrored request. Repeats are counted in the actuator payload and the metric.
    if let Recorded::NewDivergence(divergence) = recorded {
        tracing::warn!(
            target: "autumn::shadow",
            route = %context.route,
            method = %context.method,
            kind = divergence.kind.as_str(),
            fingerprint = %divergence.fingerprint,
            primary_status = divergence.primary_status,
            shadow_status = divergence.shadow_status,
            "shadow build diverged from the live build; see the shadow actuator endpoint \
             for the redacted request target and response samples"
        );
    }
}

/// Label for the live build in [`decode_side`].
const PRIMARY: &str = "primary";

/// Label for the candidate build in [`decode_side`].
const SHADOW: &str = "shadow";

/// Decode one side's body.
fn decode_side(ctx: &MirrorContext, facts: &ResponseFacts) -> Result<Bytes, ShadowError> {
    crate::shadow::transport::decode_body(
        facts.content_encoding.as_deref(),
        facts.body.clone(),
        ctx.settings.max_body_bytes,
    )
}

/// Count a decode failure, telling the failure modes apart.
fn record_decode_failure(
    ctx: &MirrorContext,
    context: &RequestContext,
    side: &'static str,
    error: &ShadowError,
) {
    if matches!(error, ShadowError::Oversize) {
        ctx.registry.record_skipped_oversize();
        record_outcome(&ctx.registry, &context.route, "skipped");
        return;
    }
    // `shadow_errors` counts candidate failures only; the live build's own malformed response is
    // a primary error.
    if side == PRIMARY {
        ctx.registry.record_primary_error();
        record_outcome(&ctx.registry, &context.route, "primary_error");
    } else {
        ctx.registry.record_shadow_error();
        record_outcome(&ctx.registry, &context.route, error.as_str());
    }
    tracing::debug!(
        target: "autumn::shadow",
        route = %context.route,
        side,
        %error,
        "could not decode a mirrored response body"
    );
}

/// Record one comparison outcome against the `{route, outcome}` series the
/// actuator scrapes as [`COMPARISONS_METRIC`].
fn record_outcome(registry: &ShadowRegistry, route: &str, outcome: &'static str) {
    registry.record_outcome(route, outcome);
}

/// Record one divergence against the `{route, kind}` series the actuator
/// scrapes as [`DIVERGENCES_METRIC`].
fn record_divergence(registry: &ShadowRegistry, route: &str, kind: DivergenceKind) {
    registry.record_divergence_kind(route, kind.as_str());
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
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(1))
            });
    }
}

/// Statuses the framework's own admission control produces for a request that
/// never reached a handler: `503` from maintenance mode, load shedding, or the
/// request deadline, and `429` from the rate limiter.
///
/// A mirrored request that ends in one of these is not compared. The candidate
/// is a separate process under none of those pressures, so it answers normally
/// and the pair diverges on status class every single time — a divergence storm
/// through a planned maintenance window, filling the bounded record ring with
/// noise and driving the metric operators alert on. The cost is that a genuine
/// handler-produced `503`/`429` divergence is not reported either; that is the
/// conservative side of the trade.
const ADMISSION_CONTROL_STATUSES: [u16; 2] = [429, 503];

pin_project! {
    /// Future returned by [`ShadowMirrorService`].
    ///
    /// When the request was selected for mirroring, this is where the mirror is
    /// actually dispatched — see [`PendingMirror`] for why that waits for the
    /// response — and where the response body is wrapped in the tee. Otherwise
    /// it is a transparent pass-through.
    pub struct ShadowMirrorFuture<F> {
        #[pin]
        inner: F,
        ctx: Arc<MirrorContext>,
        pending: Option<PendingMirror>,
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
        let Some(pending) = this.pending.take() else {
            return Poll::Ready(Ok(response));
        };

        if ADMISSION_CONTROL_STATUSES.contains(&response.status().as_u16()) {
            this.ctx.registry.record_skipped_refused();
            record_outcome(&this.ctx.registry, &pending.route, "refused");
            return Poll::Ready(Ok(response));
        }

        // A `HEAD` response sends no body, so the tee might never be polled; and an upstream layer
        // that drains it would diff the `GET` body against the candidate's empty `HEAD`. Deliver
        // empty facts now: for a `HEAD`, status class is all there is to compare.
        let head = pending.method == Method::HEAD;
        let Some(tx) = this.ctx.begin_mirror(pending) else {
            return Poll::Ready(Ok(response));
        };
        if head {
            let (parts, body) = response.into_parts();
            let _ = tx.send(Some(ResponseFacts::encoded(
                parts.status.as_u16(),
                content_type_of(&parts.headers),
                content_encoding_of(&parts.headers),
                Bytes::new(),
            )));
            return Poll::Ready(Ok(Response::from_parts(parts, body)));
        }

        Poll::Ready(Ok(tee_response(
            response,
            tx,
            this.ctx.settings.max_body_bytes,
        )))
    }
}

/// The `Content-Type` header value, when the response carries a readable one.
fn content_type_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// The response's full `Content-Encoding` coding list — a handler may serve a
/// precompressed representation, and may emit the chain as repeated fields.
fn content_encoding_of(headers: &HeaderMap) -> Option<String> {
    crate::shadow::transport::joined_content_encoding(headers)
}

/// Wrap a response body so a copy of its bytes reaches `tx` once the client has
/// been served the last frame.
fn tee_response(
    response: Response<Body>,
    tx: oneshot::Sender<Option<ResponseFacts>>,
    max_body_bytes: usize,
) -> Response<Body> {
    let status = response.status().as_u16();
    let content_type = content_type_of(response.headers());
    let content_encoding = content_encoding_of(response.headers());

    let (parts, body) = response.into_parts();

    // A body already at end-of-stream (a `204`) may never be polled, so deliver the facts now.
    if http_body::Body::is_end_stream(&body) {
        let _ = tx.send(Some(ResponseFacts::encoded(
            status,
            content_type,
            content_encoding,
            Bytes::new(),
        )));
        return Response::from_parts(parts, body);
    }

    let teed = Body::new(MirrorTeeBody {
        inner: body,
        status,
        content_type,
        content_encoding,
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
    content_encoding: Option<String>,
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
        // The mirror task gave up or finished. Stop buffering, or tee memory would be bounded by
        // inbound concurrency instead of `max_in_flight`.
        if self.tx.as_ref().is_some_and(oneshot::Sender::is_closed) {
            self.tx = None;
            return;
        }
        if self.captured.len().saturating_add(data.len()) > self.max_body_bytes {
            // Abandon rather than truncate: half a body would manufacture false divergences.
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
            Some(ResponseFacts::encoded(
                self.status,
                self.content_type.clone(),
                self.content_encoding.clone(),
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
                // A body that announces its end with its last frame is finished: the consumer need
                // not poll again.
                if http_body::Body::is_end_stream(&this.inner) {
                    this.finish();
                }
            }
            Poll::Ready(None) => this.finish(),
            // A body that errored mid-stream never reached the client whole. Dropping the sender
            // cancels the waiting mirror task.
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

    /// gzip-encode `plain`, for the precompressed-representation tests.
    fn gzipped(plain: &[u8]) -> bytes::Bytes {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(plain).expect("encode");
        bytes::Bytes::from(encoder.finish().expect("finish"))
    }

    /// What a [`FakeTransport`] should do with each mirrored request.
    #[derive(Clone)]
    enum Behaviour {
        /// Answer with a gzip `Content-Encoding`, as a handler serving a
        /// precompressed representation would.
        ReplyGzipped {
            body: &'static str,
        },
        /// Answer claiming an encoding the bytes do not actually carry.
        ReplyMisencoded,
        Reply {
            status: u16,
            body: &'static str,
        },
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
                    Behaviour::ReplyGzipped { body } => Ok(ResponseFacts::encoded(
                        200,
                        Some("application/json".to_owned()),
                        Some("gzip".to_owned()),
                        gzipped(body.as_bytes()),
                    )),
                    Behaviour::ReplyMisencoded => Ok(ResponseFacts::encoded(
                        200,
                        Some("application/json".to_owned()),
                        Some("gzip".to_owned()),
                        bytes::Bytes::from_static(b"this is not gzip"),
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
        primary_responding(StatusCode::OK, body)
    }

    /// A primary handler that always answers with `status` and `body`.
    fn primary_responding(
        status: StatusCode,
        body: &'static str,
    ) -> impl tower::Service<
        Request<Body>,
        Response = Response<Body>,
        Error = std::convert::Infallible,
    > + Clone {
        service_fn(move |_req: Request<Body>| async move {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("valid response"),
            )
        })
    }

    /// A primary handler that *consumes* the request body and echoes it back —
    /// the search-API shape issue #2332 is about. A mirror that replayed the
    /// request without its body would hand the candidate a different request
    /// than this handler answered, manufacturing a divergence.
    fn echo_primary()
    -> impl tower::Service<
        Request<Body>,
        Response = Response<Body>,
        Error = std::convert::Infallible,
    > + Clone {
        service_fn(|req: Request<Body>| async move {
            let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
                .await
                .expect("request body");
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(bytes))
                    .expect("response"),
            )
        })
    }

    async fn read_body(response: Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Wait until `condition` holds, or fail the test naming it.
    ///
    /// The mirror runs on a detached task, so tests observe its effects rather
    /// than awaiting it. Panicking on exhaustion matters: a silent
    /// fall-through turns "the mirror never ran" into a confusing assertion
    /// failure three lines later, and on a loaded CI runner that is the
    /// difference between a diagnosable failure and a mystery.
    async fn settle(what: &str, mut condition: impl FnMut() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out after 5s waiting for: {what}");
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

    /// Issue #2335, the false-positive end: the client revalidates with the
    /// primary's validator, the primary answers `304`, and the candidate —
    /// whose validator is scoped to its own build — correctly answers `200`
    /// with the full body for byte-identical content. Before the fix the
    /// mirror replayed the conditional request and recorded a `status_class`
    /// divergence on ordinary cache traffic; now the request is never
    /// mirrored, so no divergence can be recorded.
    #[tokio::test]
    async fn a_conditional_revalidation_is_not_mirrored_and_records_no_divergence() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings())
            .layer(primary_responding(StatusCode::NOT_MODIFIED, ""));

        let request = Request::builder()
            .uri("/api/orders")
            .header(axum::http::header::IF_NONE_MATCH, "\"abc123\"")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

        // The skip happens synchronously in `decide`, so there is nothing to settle.
        assert!(
            transport.seen().is_empty(),
            "a conditional request must never reach the candidate"
        );
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 0);
        assert_eq!(stats.compared, 0);
        assert_eq!(stats.diverged, 0);
        assert_eq!(stats.skipped_conditional, 1);
    }

    /// Issue #2335, the masking end: both builds revalidate, so the differ
    /// would compare two empty `304` bodies and record a `match` — while
    /// comparing nothing, hiding a genuine body regression on exactly the
    /// traffic that revalidates. Conditional requests are excluded from the
    /// mirror, so no vacuous comparison is recorded.
    #[tokio::test]
    async fn a_conditional_revalidation_is_not_compared_vacuously() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 304,
            body: "",
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings())
            .layer(primary_responding(StatusCode::NOT_MODIFIED, ""));

        let request = Request::builder()
            .uri("/api/orders")
            .header(
                axum::http::header::IF_MODIFIED_SINCE,
                "Wed, 21 Oct 2015 07:28:00 GMT",
            )
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

        assert!(transport.seen().is_empty());
        let stats = registry.stats();
        assert_eq!(stats.compared, 0, "no comparison may be recorded");
        assert_eq!(
            stats.matched, 0,
            "an empty-vs-empty 304 pair is not a match"
        );
        assert_eq!(stats.skipped_conditional, 1);
    }

    #[tokio::test]
    async fn a_get_carrying_a_body_is_never_mirrored() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "{}",
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(echo_primary());

        // A GET with a body is legal (search APIs use it). The mirror replays no body, so it
        // would ask the candidate a different request (#2332).
        let body = r#"{"query":"shoes"}"#;
        let request = Request::builder()
            .uri("/api/orders")
            .header("content-length", body.len())
            .body(Body::from(body))
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        // The primary saw the whole body.
        assert_eq!(read_body(response).await, body);

        assert!(transport.seen().is_empty());
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 0);
        assert_eq!(stats.compared, 0);
        assert_eq!(stats.diverged, 0);
    }

    #[tokio::test]
    async fn a_get_with_an_undeclared_body_is_never_mirrored() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "{}",
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(echo_primary());

        // No framing headers: on HTTP/2 a client can send DATA frames on a GET stream undeclared.
        // Only the body's end-of-stream signal catches it (#2332).
        let body = r#"{"query":"shoes"}"#;
        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::from(body))
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_body(response).await, body);

        assert!(transport.seen().is_empty());
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 0);
        assert_eq!(stats.compared, 0);
        assert_eq!(stats.diverged, 0);
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

        settle("the shadow request to be sent", || {
            !transport.seen().is_empty()
        })
        .await;
        let seen = transport.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url, "http://shadow.invalid/api/orders?page=2");
        assert_eq!(seen[0].method, axum::http::Method::GET);
        assert!(seen[0].headers.contains_key(SHADOW_HEADER));

        settle("the comparison to be recorded", || {
            registry.stats().matched == 1
        })
        .await;
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 1);
        assert_eq!(stats.compared, 1);
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

        settle("the divergence record to land", || {
            !registry.recent().is_empty()
        })
        .await;
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

        settle("the transport failure to be counted", || {
            registry.stats().shadow_errors == 1
        })
        .await;
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
        // The shadow stalls for 30 s against a 50 ms deadline; the client must wait on neither.
        assert!(
            elapsed < Duration::from_millis(100),
            "the client waited {elapsed:?} on a stalled shadow"
        );

        settle("the shadow deadline to fire", || {
            registry.stats().shadow_timeouts == 1
        })
        .await;
        assert_eq!(registry.stats().shadow_timeouts, 1);
    }

    #[tokio::test]
    async fn one_deadline_covers_both_mirror_waits() {
        // A shadow that answers just under the deadline must not start a fresh deadline on the
        // primary wait, which would hold the permit for up to twice the timeout.
        let transport = FakeTransport::new(Behaviour::Stall(Duration::from_millis(120)));
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.timeout = Duration::from_millis(200);
        settings.max_in_flight = 1;
        let service =
            layer(transport.clone(), &registry, settings).layer(primary(r#"{"ok":true}"#));

        // Drop the response unread so only the shared deadline can end the mirror.
        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let started = std::time::Instant::now();
        drop(response);

        settle("the mirror to give up", || {
            registry.stats().primary_incomplete == 1
        })
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(360),
            "the mirror lived {elapsed:?}, close to twice the 200ms deadline"
        );
    }

    /// Layer whose comparison step takes `delay` of blocking time, standing in
    /// for a large compressed body (issue #2333).
    fn layer_with_slow_comparison(
        transport: Arc<dyn ShadowTransport>,
        registry: &ShadowRegistry,
        settings: MirrorSettings,
        delay: Duration,
    ) -> ShadowMirrorLayer {
        let mut layer = layer(transport, registry, settings);
        Arc::get_mut(&mut layer.ctx)
            .expect("sole owner")
            .compare_delay = delay;
        layer
    }

    #[tokio::test]
    async fn a_slow_comparison_is_abandoned_at_the_deadline() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.timeout = Duration::from_millis(100);
        settings.max_in_flight = 1;
        let layer =
            layer_with_slow_comparison(transport, &registry, settings, Duration::from_millis(600));
        let in_flight = Arc::clone(&layer.ctx.in_flight);
        let service = layer.layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;
        let started = std::time::Instant::now();

        settle("the comparison to be abandoned", || {
            registry.stats().comparisons_abandoned == 1
        })
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "the mirror outlived its deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(in_flight.load(Ordering::Acquire), 0, "the slot is free");
        assert!(
            registry
                .comparisons_by_route()
                .iter()
                .any(|c| c.label == "abandoned" && c.count == 1)
        );

        // The late work finishes but must record nothing.
        tokio::time::sleep(Duration::from_millis(1700)).await;
        let stats = registry.stats();
        assert_eq!(stats.compared, 0);
        assert_eq!(stats.matched, 0);
        assert_eq!(stats.comparisons_abandoned, 1);
    }

    #[tokio::test]
    async fn a_comparison_inside_the_deadline_is_recorded() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.timeout = Duration::from_millis(500);
        let service =
            layer_with_slow_comparison(transport, &registry, settings, Duration::from_millis(20))
                .layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle("the comparison to be recorded", || {
            registry.stats().matched == 1
        })
        .await;
        assert_eq!(registry.stats().comparisons_abandoned, 0);
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

        settle("the excess mirrors to be dropped", || {
            registry.stats().dropped_at_capacity >= 3
        })
        .await;
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

        settle("the oversize body to be skipped", || {
            registry.stats().skipped_oversize == 1
        })
        .await;
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

        settle("the oversize body to be skipped", || {
            registry.stats().skipped_oversize == 1
        })
        .await;
        assert_eq!(registry.stats().skipped_oversize, 1);
        assert_eq!(registry.stats().compared, 0);
    }

    #[tokio::test]
    async fn a_transport_reported_oversize_body_is_skipped_not_errored() {
        // The real transport stops reading a candidate body past the budget and returns this error.
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

        settle("the oversize body to be skipped", || {
            registry.stats().skipped_oversize == 1
        })
        .await;
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

        settle("the divergence record to land", || {
            !registry.recent().is_empty()
        })
        .await;
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
    async fn a_precompressed_body_is_decoded_on_both_sides_before_comparing() {
        let transport = FakeTransport::new(Behaviour::ReplyGzipped {
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(service_fn(
            |_req: Request<Body>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .header("content-encoding", "gzip")
                        .body(Body::from(gzipped(br#"{"ok":true}"#)))
                        .expect("valid response"),
                )
            },
        ));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle("the comparison to be recorded", || {
            registry.stats().compared == 1
        })
        .await;
        assert_eq!(
            registry.stats().matched,
            1,
            "identical precompressed bodies must not diverge"
        );
    }

    #[tokio::test]
    async fn a_precompressed_primary_matches_a_plain_candidate_with_the_same_content() {
        // An encoding difference is a header difference, and headers are not compared.
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(service_fn(
            |_req: Request<Body>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .header("content-encoding", "gzip")
                        .body(Body::from(gzipped(br#"{"ok":true}"#)))
                        .expect("valid response"),
                )
            },
        ));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle("the comparison to be recorded", || {
            registry.stats().compared == 1
        })
        .await;
        assert_eq!(registry.stats().matched, 1);
    }

    #[tokio::test]
    async fn a_head_on_a_precompressed_route_still_compares() {
        // Both sides arrive as zero bytes still declaring `Content-Encoding: gzip`, which a
        // decoder rejects with an unexpected EOF.
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "",
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(service_fn(
            |_req: Request<Body>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .header("content-encoding", "gzip")
                        .body(Body::from(gzipped(br#"{"ok":true}"#)))
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
        assert_eq!(response.status(), StatusCode::OK);

        settle("the HEAD comparison to be recorded", || {
            registry.stats().compared == 1
        })
        .await;
        let stats = registry.stats();
        assert_eq!(stats.matched, 1, "a HEAD compares on status class alone");
        assert_eq!(
            stats.shadow_errors, 0,
            "an empty body is not a decode failure"
        );
    }

    #[tokio::test]
    async fn a_malformed_primary_body_is_not_blamed_on_the_candidate() {
        // `shadow_errors` counts candidate failures only.
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings()).layer(service_fn(
            |_req: Request<Body>| async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .header("content-encoding", "gzip")
                        .body(Body::from("this is definitely not gzip"))
                        .expect("valid response"),
                )
            },
        ));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle("the primary decode failure to be counted", || {
            registry.stats().primary_errors == 1
        })
        .await;
        let stats = registry.stats();
        assert_eq!(stats.primary_errors, 1);
        assert_eq!(
            stats.shadow_errors, 0,
            "the candidate answered fine; it must not be blamed"
        );
        assert_eq!(stats.compared, 0);
    }

    #[tokio::test]
    async fn a_malformed_encoding_is_an_error_not_a_size_skip() {
        // `skipped_oversize` would send an operator looking for a body over `max_body_bytes`.
        let transport = FakeTransport::new(Behaviour::ReplyMisencoded);
        let registry = ShadowRegistry::new(10);
        let service =
            layer(transport.clone(), &registry, settings()).layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(read_body(response).await, r#"{"ok":true}"#);

        settle("the decode failure to be counted", || {
            registry.stats().shadow_errors == 1
        })
        .await;
        let stats = registry.stats();
        assert_eq!(stats.shadow_errors, 1);
        assert_eq!(
            stats.skipped_oversize, 0,
            "a malformed encoding is not a size-budget skip"
        );
        assert_eq!(stats.compared, 0);
    }

    #[tokio::test]
    async fn a_permit_is_released_so_mirroring_survives_the_first_batch() {
        // If `InFlightPermit::drop` were a no-op, mirroring would stop after `max_in_flight`
        // requests.
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.max_in_flight = 1;
        let mirror = layer(transport.clone(), &registry, settings);

        for expected in 1..=3 {
            let service = mirror.clone().layer(primary(r#"{"ok":true}"#));
            let request = Request::builder()
                .uri("/api/orders")
                .body(Body::empty())
                .expect("request");
            let response = service.oneshot(request).await.expect("response");
            let _ = read_body(response).await;
            settle("the slot to be released", || {
                registry.stats().matched == expected
            })
            .await;
        }

        let stats = registry.stats();
        assert_eq!(stats.mirrored, 3, "a single slot must be reusable");
        assert_eq!(stats.dropped_at_capacity, 0);
    }

    #[tokio::test]
    async fn a_request_the_live_build_refused_is_not_mirrored() {
        // This layer sits outside load shedding, maintenance mode and the rate limiter. The
        // candidate, under none of those pressures, would answer 200: a false divergence.
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let transport = FakeTransport::new(Behaviour::Reply {
                status: 200,
                body: r#"{"ok":true}"#,
            });
            let registry = ShadowRegistry::new(10);
            let service = layer(transport.clone(), &registry, settings()).layer(service_fn(
                move |_req: Request<Body>| async move {
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(status)
                            .body(Body::from("rejected"))
                            .expect("valid response"),
                    )
                },
            ));

            let request = Request::builder()
                .uri("/api/orders")
                .body(Body::empty())
                .expect("request");
            let response = service.oneshot(request).await.expect("response");
            assert_eq!(response.status(), status);
            let _ = read_body(response).await;

            settle("the refusal to be counted", || {
                registry.stats().skipped_refused == 1
            })
            .await;
            let stats = registry.stats();
            assert_eq!(stats.mirrored, 0, "{status} must not reach the candidate");
            assert_eq!(stats.diverged, 0);
            assert!(transport.seen().is_empty());
        }
    }

    #[tokio::test]
    async fn a_head_request_compares_on_status_class_alone() {
        // A `HEAD` response has no body to tee, so both sides compare empty.
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "",
        });
        let registry = ShadowRegistry::new(10);
        let service = layer(transport.clone(), &registry, settings())
            .layer(primary(r#"{"ok":true,"total":42}"#));

        let request = Request::builder()
            .method("HEAD")
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        settle("the HEAD comparison to be recorded", || {
            registry.stats().compared == 1
        })
        .await;
        assert_eq!(
            registry.stats().matched,
            1,
            "a HEAD must not diff the GET body against the candidate's empty one"
        );
        let seen = transport.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, axum::http::Method::HEAD);
    }

    #[tokio::test]
    async fn a_primary_body_the_client_never_finishes_is_counted_not_leaked() {
        // Dropping the response unread stands in for a client that disconnects. The counter
        // accounts for the gap between `mirrored` and every other outcome.
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: r#"{"ok":true}"#,
        });
        let registry = ShadowRegistry::new(10);
        let mut settings = settings();
        settings.timeout = Duration::from_millis(50);
        let service =
            layer(transport.clone(), &registry, settings).layer(primary(r#"{"ok":true}"#));

        let request = Request::builder()
            .uri("/api/orders")
            .body(Body::empty())
            .expect("request");
        let response = service.oneshot(request).await.expect("response");
        drop(response); // never read the body

        settle("the abandoned primary to be counted", || {
            registry.stats().primary_incomplete == 1
        })
        .await;
        let stats = registry.stats();
        assert_eq!(stats.mirrored, 1);
        assert_eq!(stats.compared, 0);
        assert_eq!(stats.primary_incomplete, 1);
    }

    #[tokio::test]
    async fn the_route_label_prefers_the_matched_route_template() {
        let transport = FakeTransport::new(Behaviour::Reply {
            status: 200,
            body: "{}",
        });
        let registry = ShadowRegistry::new(10);
        // A real axum Router sets `MatchedPath`, as the framework's ingress stack does.
        let router: axum::Router = axum::Router::new()
            .route(
                "/api/orders/{id}",
                axum::routing::get(|| async { r#"{"ok":true}"# }),
            )
            .layer(layer(transport.clone(), &registry, settings()));

        let request = Request::builder()
            .uri("/api/orders/42")
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.expect("response");
        let _ = read_body(response).await;

        settle("the divergence record to land", || {
            !registry.recent().is_empty()
        })
        .await;
        assert_eq!(
            registry.recent()[0].route,
            "/api/orders/{id}",
            "the bounded route template beats both the raw path and the fallback"
        );
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

        settle("the comparison to be recorded", || {
            registry.stats().matched == 1
        })
        .await;
        assert_eq!(registry.stats().compared, 1);
    }
}
