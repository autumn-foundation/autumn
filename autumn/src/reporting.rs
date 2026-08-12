//! Pluggable error reporting: capture handler panics and 5xx responses and
//! route them to one or more configured reporters.
//!
//! When an Autumn handler panics or returns a server error, the failure is
//! turned into a structured [`ErrorEvent`](crate::reporting::ErrorEvent) and
//! delivered to every registered
//! [`ErrorReporter`](crate::reporting::ErrorReporter). This is the "where do my
//! errors go?" seam: ship events to
//! Sentry, Honeycomb, Slack, or a custom sink by implementing a single trait
//! and wiring it once with
//! [`AppBuilder::with_error_reporter`](crate::app::AppBuilder::with_error_reporter).
//!
//! The design mirrors Rails' [Error Reporter] and Autumn's other pluggable
//! backends ([`BlobStore`](crate::storage::BlobStore),
//! [`Cache`](crate::cache::Cache)): a built-in
//! [`LogReporter`](crate::reporting::LogReporter) (which uses
//! `tracing`) ships as the default so the feature is useful with zero extra
//! dependencies, and one builder call swaps in your own sink.
//!
//! # What gets reported
//!
//! - **Handler panics.** A [`ReportingLayer`](crate::reporting::ReportingLayer)
//!   catches unwinding panics at the
//!   HTTP layer (so a single panicking handler can never abort the worker
//!   task), converts them into a sanitized [`AutumnError`](crate::AutumnError)
//!   `500` Problem Details response, and reports an
//!   [`ErrorEvent`](crate::reporting::ErrorEvent) carrying the
//!   panic payload and (when `RUST_BACKTRACE` is set) a backtrace.
//! - **Server errors.** Any response with a `5xx` status is reported with its
//!   status, message, and Problem Details type.
//!
//! Client (`4xx`) errors are intentionally *not* reported — this slice is
//! panics + server errors only.
//!
//! ## Scope: which 5xx are observed
//!
//! The layer is installed inner to
//! [`RequestIdLayer`](crate::middleware::RequestIdLayer) so every event carries
//! the request id (and a panic, which unwinds the inner stack, still has it).
//! A consequence of that placement is that 5xx responses produced by middleware
//! *outer* to it — most notably a `503` from the session layer when a session
//! store (e.g. Redis) is unavailable — are not observed here. That is a
//! deliberate trade-off: such failures are infrastructure outages already
//! surfaced by readiness/health probes, and moving reporting outside the
//! session layer would also move it outside `RequestIdLayer`, dropping the
//! request id from *every* event. Handler panics and handler/inner-middleware
//! server errors — the failures an app owner is expected to act on — are
//! reported with full context.
//!
//! # Example
//!
//! ```rust,no_run
//! use autumn_web::reporting::{ErrorEvent, ErrorReporter, ReportFuture};
//!
//! struct SlackReporter {
//!     webhook_url: String,
//! }
//!
//! impl ErrorReporter for SlackReporter {
//!     fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a> {
//!         Box::pin(async move {
//!             // post `event` to Slack, swallow any transport error
//!             let _ = (&self.webhook_url, event.status);
//!         })
//!     }
//! }
//!
//! # #[autumn_web::main]
//! # async fn main() {
//! autumn_web::app()
//!     .with_error_reporter(SlackReporter { webhook_url: "https://hooks.slack.example".into() })
//! #   .routes(vec![])
//! #   ;
//! # }
//! ```
//!
//! [Error Reporter]: https://guides.rubyonrails.org/error_reporting.html

use std::any::Any;
use std::backtrace::{Backtrace, BacktraceStatus};
use std::cell::RefCell;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Once};
use std::task::{Context, Poll};

use axum::extract::MatchedPath;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::FutureExt;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::middleware::RequestId;
use crate::middleware::exception_filter::AutumnErrorInfo;

/// The future returned by [`ErrorReporter::report`].
///
/// A boxed, pinned future mirroring the shape of
/// [`BlobFuture`](crate::storage::BlobFuture) so the trait stays object-safe
/// while remaining async-friendly.
pub type ReportFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// A structured description of a failure worth reporting.
///
/// Carries enough request context to locate the failure (route, method,
/// request id) plus the failure details (status, message, Problem Details
/// type). For panics, [`panic`](ErrorEvent::panic) carries the payload and an
/// optional backtrace.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ErrorEvent {
    /// HTTP status code of the failing response (always `5xx`).
    pub status: StatusCode,
    /// Human-readable error message. For panics this is the panic payload; for
    /// server errors it is the underlying error's message.
    pub message: String,
    /// Problem Details `type` URI, when the error carried one.
    pub problem_type: Option<String>,
    /// The request id (`X-Request-Id`) of the failing request, when available.
    pub request_id: Option<String>,
    /// The matched route template (e.g. `/users/{id}`), when available.
    pub route: Option<String>,
    /// The HTTP method of the failing request (e.g. `GET`), when available.
    pub method: Option<String>,
    /// Panic details, present only when the failure originated from a caught
    /// handler panic.
    pub panic: Option<PanicInfo>,
    /// The replay capsule written for this failure, when
    /// `[failure_capture] enabled = true` and persistence succeeded.
    ///
    /// The capsule file already exists on disk by the time a reporter is
    /// invoked, so a reporter may safely attach the path (or read the file)
    /// while handling the event.
    pub capsule: Option<crate::capsule::CapsuleRef>,
}

/// Details of a caught handler panic.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PanicInfo {
    /// The panic payload, downcast to a string when possible.
    pub payload: String,
    /// A captured backtrace, present only when `RUST_BACKTRACE` is set.
    pub backtrace: Option<String>,
}

/// A sink for [`ErrorEvent`]s.
///
/// Implement this trait to ship unhandled panics and server errors to an
/// external service. Register implementations with
/// [`AppBuilder::with_error_reporter`](crate::app::AppBuilder::with_error_reporter);
/// multiple reporters can be chained and each receives every event.
///
/// Reporting runs on a detached task, so [`report`](ErrorReporter::report) does
/// not block the client response. Any panic raised inside `report` is caught
/// and logged — a misbehaving reporter never affects the response.
pub trait ErrorReporter: Send + Sync + 'static {
    /// Deliver an [`ErrorEvent`] to the sink.
    ///
    /// Implementations should swallow their own transport errors; returning is
    /// the only signal the framework needs.
    fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a>;
}

/// The built-in default reporter: logs every event through `tracing`.
///
/// Installed automatically when no other reporter is registered, so error
/// reporting is useful out of the box with zero extra dependencies.
#[derive(Debug, Clone, Default)]
pub struct LogReporter;

impl ErrorReporter for LogReporter {
    fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a> {
        Box::pin(async move {
            if let Some(panic) = event.panic.as_ref() {
                tracing::error!(
                    status = %event.status,
                    method = event.method.as_deref().unwrap_or("-"),
                    route = event.route.as_deref().unwrap_or("-"),
                    request_id = event.request_id.as_deref().unwrap_or("-"),
                    backtrace = panic.backtrace.as_deref().unwrap_or("(set RUST_BACKTRACE=1 to capture)"),
                    "handler panic captured: {}",
                    panic.payload
                );
            } else {
                tracing::error!(
                    status = %event.status,
                    method = event.method.as_deref().unwrap_or("-"),
                    route = event.route.as_deref().unwrap_or("-"),
                    request_id = event.request_id.as_deref().unwrap_or("-"),
                    problem_type = event.problem_type.as_deref().unwrap_or("-"),
                    "server error captured: {}",
                    event.message
                );
            }
        })
    }
}

/// Runtime holder for the registered reporters, installed on
/// [`AppState`](crate::state::AppState) extensions so the
/// [`ReportingLayer`] can pick them up at router-build time.
#[derive(Clone, Default)]
pub(crate) struct RegisteredReporters(pub(crate) Vec<Arc<dyn ErrorReporter>>);

/// The shared reporter chain plus sampling/enable knobs.
struct ReporterChain {
    reporters: Vec<Arc<dyn ErrorReporter>>,
    enabled: bool,
    sample_rate: f64,
}

impl ReporterChain {
    /// Decide whether to deliver this event, then dispatch it on a detached
    /// task so reporting never blocks (or breaks) the client response.
    ///
    /// Capsule persistence rides along here rather than in the capture layer
    /// because it must happen on the same detached task and *before* any
    /// reporter runs: a reporter that receives
    /// [`ErrorEvent::capsule`](ErrorEvent::capsule) must find the file already
    /// on disk. That also means persistence is not gated on `enabled` or the
    /// sample rate — an app with reporting turned off still writes capsules.
    ///
    /// Writing the capsule is blocking filesystem work (a directory scan, a
    /// `write` + `sync_all`, a rename), so it goes to the blocking pool rather
    /// than running inline on an async worker: an error storm against slow
    /// storage would otherwise stall the workers that serve everyone else's
    /// requests — and stall a current-thread runtime outright. Awaiting the
    /// join handle before reporting keeps the ordering guarantee: the file is
    /// on disk before any reporter sees the reference to it.
    fn dispatch(self: &Arc<Self>, event: ErrorEvent, capture: Option<CaptureContext>) {
        let deliver = self.enabled && sampled(self.sample_rate);
        if !deliver && capture.is_none() {
            return;
        }
        // Reporting is best-effort: if we're somehow off-runtime, drop it
        // rather than panic.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let chain = Arc::clone(self);
            handle.spawn(async move {
                let mut event = event;
                if let Some(capture) = capture {
                    event.capsule = persist_capsule(capture).await;
                }
                if deliver {
                    chain.report_all(&event).await;
                }
            });
        }
    }

    async fn report_all(&self, event: &ErrorEvent) {
        for reporter in &self.reporters {
            // Guard both future construction and polling: a panicking reporter
            // must never escape to abort the reporting task.
            match std::panic::catch_unwind(AssertUnwindSafe(|| reporter.report(event))) {
                Ok(future) => {
                    if AssertUnwindSafe(future).catch_unwind().await.is_err() {
                        tracing::warn!("error reporter panicked while reporting; ignoring");
                    }
                }
                Err(_panic) => {
                    tracing::warn!("error reporter panicked constructing report future; ignoring");
                }
            }
        }
    }
}

/// A request's capture scope plus the outcome that seals it.
///
/// Passed to [`ReporterChain::dispatch`] alongside the event so the capsule is
/// written on the reporting task, before reporters see the event.
struct CaptureContext {
    handle: crate::capsule::CaptureHandle,
    outcome: crate::capsule::CapsuleOutcome,
}

/// Write the capsule for a failed request on the blocking pool, and wait for
/// it.
///
/// The wait is the point: [`ErrorEvent::capsule`](ErrorEvent::capsule) is only
/// worth carrying if the file it names is already on disk when a reporter
/// follows it, so this resolves before `report_all` runs. Moving the work off
/// the async worker is what keeps a slow disk (or a burst of failures) from
/// blocking the runtime while that happens.
///
/// A join failure — the blocking task panicked or the runtime is shutting down
/// — is logged and reported as "no capsule", exactly like a write failure:
/// capsule persistence must never make a bad request worse.
async fn persist_capsule(capture: CaptureContext) -> Option<crate::capsule::CapsuleRef> {
    let written = tokio::task::spawn_blocking(move || {
        crate::capsule::persist(capture.handle.scope(), capture.outcome)
    })
    .await;
    match written {
        Ok(reference) => reference,
        Err(error) => {
            tracing::error!(
                %error,
                "failure capsule could not be written on the blocking pool; \
                 the failure itself is still reported"
            );
            None
        }
    }
}

thread_local! {
    /// Per-thread Xorshift64 state, seeded once from the OS entropy source.
    /// Sampling does not need cryptographic randomness, so a userspace PRNG
    /// keeps the hot path (every panic / 5xx) free of `getrandom` syscalls.
    static RNG_STATE: std::cell::Cell<u64> = std::cell::Cell::new(seed_rng());
}

/// Seed the per-thread PRNG from the OS entropy source, falling back to a
/// non-zero constant if that ever fails (Xorshift must never start at zero).
fn seed_rng() -> u64 {
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_ok() {
        let seed = u64::from_ne_bytes(buf);
        if seed != 0 {
            return seed;
        }
    }
    0x5555_5555_5555_5555
}

/// Draw a fast, non-cryptographic `u64` from the per-thread Xorshift64 PRNG.
fn next_u64() -> u64 {
    RNG_STATE.with(|cell| {
        let mut x = cell.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        cell.set(x);
        x
    })
}

/// Draw a sampling decision for the given rate in `[0.0, 1.0]`.
///
/// The cast precision loss is irrelevant here: sampling tolerates a fuzzy
/// boundary, and a 53-bit draw is more than enough resolution for a rate knob.
#[allow(clippy::cast_precision_loss)]
fn sampled(rate: f64) -> bool {
    if rate >= 1.0 {
        return true;
    }
    if rate <= 0.0 {
        return false;
    }
    // Mask to 53 bits so the value converts to f64 without rounding.
    let draw = next_u64() >> 11;
    let value = draw as f64 / (1u64 << 53) as f64;
    value < rate
}

// ── Panic backtrace capture ─────────────────────────────────────────────────

thread_local! {
    static LAST_PANIC: RefCell<Option<CapturedPanic>> = const { RefCell::new(None) };
}

struct CapturedPanic {
    backtrace: Option<String>,
}

static HOOK_INSTALLED: Once = Once::new();

/// Install a panic hook (once) that records a backtrace for the panicking
/// thread so the [`ReportingLayer`] can attach it to the [`ErrorEvent`] after
/// `catch_unwind` returns. The previous hook is preserved and still runs, so
/// the default panic logging behavior is unchanged.
fn ensure_panic_hook() {
    HOOK_INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // `Backtrace::capture()` only captures when `RUST_BACKTRACE` is set,
            // so this is free when backtraces are disabled.
            let backtrace = Backtrace::capture();
            let backtrace =
                (backtrace.status() == BacktraceStatus::Captured).then(|| backtrace.to_string());
            LAST_PANIC.with(|cell| {
                *cell.borrow_mut() = Some(CapturedPanic { backtrace });
            });
            previous(info);
        }));
    });
}

/// Downcast a panic payload to a string, mirroring the formatting used for
/// repository commit hook panics.
fn format_panic_payload(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "handler panicked".to_owned())
}

// ── Tower layer ──────────────────────────────────────────────────────────────

/// Per-request context captured before the inner service runs, so it is still
/// available if the handler panics.
#[derive(Clone)]
struct RequestContext {
    method: String,
    route: Option<String>,
    request_id: Option<String>,
    /// Handle to the request's capsule buffer, snapshotted here for the same
    /// reason the rest of this struct is: it must survive a handler unwind.
    capture: Option<crate::capsule::CaptureHandle>,
}

/// Tower [`Layer`] that catches handler panics and reports panics + 5xx
/// responses to the registered [`ErrorReporter`]s.
///
/// Applied automatically by the framework inner to
/// [`RequestIdLayer`](crate::middleware::RequestIdLayer) (so the request id is
/// available) and outer to the route handler (so handler panics are caught).
#[derive(Clone)]
pub struct ReportingLayer {
    chain: Arc<ReporterChain>,
}

impl ReportingLayer {
    /// Build a reporting layer from the registered reporters and config knobs.
    ///
    /// When `reporters` is empty, the built-in [`LogReporter`] is installed so
    /// panics and server errors are still surfaced.
    #[must_use]
    pub(crate) fn new(
        reporters: Vec<Arc<dyn ErrorReporter>>,
        enabled: bool,
        sample_rate: f64,
    ) -> Self {
        ensure_panic_hook();
        let reporters = if reporters.is_empty() {
            vec![Arc::new(LogReporter) as Arc<dyn ErrorReporter>]
        } else {
            reporters
        };
        Self {
            chain: Arc::new(ReporterChain {
                reporters,
                enabled,
                sample_rate,
            }),
        }
    }
}

impl<S> Layer<S> for ReportingLayer {
    type Service = ReportingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ReportingService {
            inner,
            chain: Arc::clone(&self.chain),
        }
    }
}

/// Tower [`Service`] produced by [`ReportingLayer`].
#[derive(Clone)]
pub struct ReportingService<S> {
    inner: S,
    chain: Arc<ReporterChain>,
}

impl<S, ReqBody> Service<Request<ReqBody>> for ReportingService<S>
where
    S: Service<Request<ReqBody>, Response = Response>,
{
    type Response = Response;
    type Error = S::Error;
    type Future = ReportingFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let method = req.method().as_str().to_owned();
        let route = req
            .extensions()
            .get::<MatchedPath>()
            .map(|m| m.as_str().to_owned());
        let request_id = req
            .extensions()
            .get::<RequestId>()
            .map(std::string::ToString::to_string);
        let capture = req
            .extensions()
            .get::<crate::capsule::CaptureHandle>()
            .cloned();
        let context = Some(RequestContext {
            method,
            route,
            request_id,
            capture,
        });

        // Catch panics raised synchronously while the inner service constructs
        // its future (e.g. a handler closure or user Tower layer that panics in
        // `call` before returning a future), not just panics raised while
        // polling. Mirrors `tower_http::catch_panic`.
        let inner = &mut self.inner;
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.call(req))) {
            Ok(future) => ReportingFuture {
                inner: Some(future),
                pending_panic: None,
                context,
                chain: Arc::clone(&self.chain),
            },
            Err(panic) => ReportingFuture {
                inner: None,
                pending_panic: Some(panic),
                context,
                chain: Arc::clone(&self.chain),
            },
        }
    }
}

pin_project! {
    /// Future that catches panics from the inner service and dispatches error
    /// events for panics and 5xx responses.
    pub struct ReportingFuture<F> {
        #[pin]
        inner: Option<F>,
        // A panic captured from the inner service's `call`, surfaced on the
        // first poll. `inner` is `None` exactly when this is `Some`.
        pending_panic: Option<Box<dyn Any + Send>>,
        context: Option<RequestContext>,
        chain: Arc<ReporterChain>,
    }
}

impl<F, E> Future for ReportingFuture<F>
where
    F: Future<Output = Result<Response, E>>,
{
    type Output = Result<Response, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // A panic captured in `call` is surfaced as a sanitized 500 here.
        if let Some(panic) = this.pending_panic.take() {
            let context = this.context.take();
            return Poll::Ready(Ok(handle_panic(&*panic, context, this.chain)));
        }

        let Some(inner) = this.inner.as_pin_mut() else {
            // Already resolved a panic on a prior poll; nothing left to do.
            return Poll::Pending;
        };

        // Catch a panic raised while polling the handler future. Wrapping the
        // poll keeps a panicking handler from aborting the worker task.
        match std::panic::catch_unwind(AssertUnwindSafe(move || inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(Ok(response))) => {
                if let Some(context) = this.context.take() {
                    report_response(&response, context, this.chain);
                }
                Poll::Ready(Ok(response))
            }
            Ok(Poll::Ready(Err(error))) => Poll::Ready(Err(error)),
            Err(panic) => {
                let context = this.context.take();
                let response = handle_panic(&*panic, context, this.chain);
                Poll::Ready(Ok(response))
            }
        }
    }
}

/// Report a completed response when it is a server error.
fn report_response(response: &Response, context: RequestContext, chain: &Arc<ReporterChain>) {
    if !response.status().is_server_error() {
        return;
    }
    let info = response.extensions().get::<AutumnErrorInfo>();
    let (message, problem_type) = info.map_or_else(
        || {
            (
                response
                    .status()
                    .canonical_reason()
                    .unwrap_or("server error")
                    .to_owned(),
                None,
            )
        },
        |info| (info.message.clone(), info.problem_type.map(str::to_owned)),
    );

    let capture = context.capture.map(|handle| CaptureContext {
        handle,
        outcome: crate::capsule::CapsuleOutcome::Status {
            code: response.status().as_u16(),
            message: message.clone(),
            problem_type: problem_type.clone(),
        },
    });

    chain.dispatch(
        ErrorEvent {
            status: response.status(),
            message,
            problem_type,
            request_id: context.request_id,
            route: context.route,
            method: Some(context.method),
            panic: None,
            capsule: None,
        },
        capture,
    );
}

/// Convert a caught panic into a sanitized 500 response and report it.
fn handle_panic(
    payload: &(dyn Any + Send),
    context: Option<RequestContext>,
    chain: &Arc<ReporterChain>,
) -> Response {
    let message = format_panic_payload(payload);
    let backtrace = LAST_PANIC
        .with(|cell| cell.borrow_mut().take())
        .and_then(|captured| captured.backtrace);

    if let Some(context) = context {
        let capture = context.capture.map(|handle| CaptureContext {
            handle,
            outcome: crate::capsule::CapsuleOutcome::Panic {
                status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                payload: message.clone(),
                backtrace: backtrace.clone(),
            },
        });
        chain.dispatch(
            ErrorEvent {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: message.clone(),
                problem_type: None,
                request_id: context.request_id,
                route: context.route,
                method: Some(context.method),
                panic: Some(PanicInfo {
                    payload: message,
                    backtrace,
                }),
                capsule: None,
            },
            capture,
        );
    }

    // The client gets a clean, sanitized Problem Details 500 — the panic
    // payload only ever reaches the reporter, never the wire. The
    // `AutumnErrorInfo` stashed by `into_response` lets the exception-filter
    // chain negotiate HTML error pages as usual.
    crate::error::AutumnError::internal_server_error_msg("Internal server error").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn sampled_extremes_are_deterministic() {
        assert!(sampled(1.0));
        assert!(sampled(2.0));
        assert!(!sampled(0.0));
        assert!(!sampled(-1.0));
    }

    #[test]
    fn sampled_full_rate_always_true_over_many_draws() {
        for _ in 0..1000 {
            assert!(sampled(1.0));
        }
    }

    #[test]
    fn format_panic_payload_handles_str_and_string() {
        let s: &str = "boom";
        assert_eq!(format_panic_payload(&s), "boom");
        let owned: String = "kaboom".to_owned();
        assert_eq!(format_panic_payload(&owned), "kaboom");
        let other: u32 = 7;
        assert_eq!(format_panic_payload(&other), "handler panicked");
    }

    #[test]
    fn capture_context_can_cross_to_the_blocking_pool() {
        // `persist_capsule` hands the context to `spawn_blocking`, which needs
        // `Send + 'static`. A future field that is neither would only show up
        // as a confusing error inside `dispatch`, so pin it here.
        const fn assert_send_static<T: Send + 'static>() {}
        assert_send_static::<CaptureContext>();
    }

    #[test]
    fn log_reporter_is_the_default_when_empty() {
        let layer = ReportingLayer::new(Vec::new(), true, 1.0);
        assert_eq!(layer.chain.reporters.len(), 1);
    }

    #[tokio::test]
    async fn panic_in_inner_call_is_caught_as_500() {
        use axum::body::Body;
        use std::convert::Infallible;
        use tower::ServiceExt;

        // An inner service that panics synchronously in `call`, before ever
        // returning a future — the case poll-only catch_unwind would miss.
        #[derive(Clone)]
        struct PanicInCall;
        impl Service<Request<Body>> for PanicInCall {
            type Response = Response;
            type Error = Infallible;
            type Future = std::future::Ready<Result<Response, Infallible>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _req: Request<Body>) -> Self::Future {
                panic!("boom in call");
            }
        }

        let service = ReportingLayer::new(Vec::new(), true, 1.0).layer(PanicInCall);
        let response = service
            .oneshot(Request::new(Body::empty()))
            .await
            .expect("panic in call must be converted to a response, not propagated");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn disabled_chain_does_not_dispatch() {
        #[derive(Clone)]
        struct Counter(Arc<Mutex<u32>>);
        impl ErrorReporter for Counter {
            fn report<'a>(&'a self, _event: &'a ErrorEvent) -> ReportFuture<'a> {
                let count = self.0.clone();
                Box::pin(async move {
                    *count.lock().unwrap() += 1;
                })
            }
        }

        let count = Arc::new(Mutex::new(0));
        let chain = Arc::new(ReporterChain {
            reporters: vec![Arc::new(Counter(count.clone()))],
            enabled: false,
            sample_rate: 1.0,
        });
        chain.dispatch(
            ErrorEvent {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "x".into(),
                problem_type: None,
                request_id: None,
                route: None,
                method: None,
                panic: None,
                capsule: None,
            },
            None,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(*count.lock().unwrap(), 0);
    }

    fn server_error_event() -> ErrorEvent {
        ErrorEvent {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "boom".into(),
            problem_type: Some("https://autumn.dev/problems/x".into()),
            request_id: Some("req-1".into()),
            route: Some("/x".into()),
            method: Some("GET".into()),
            panic: None,
            capsule: None,
        }
    }

    fn panic_event() -> ErrorEvent {
        ErrorEvent {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "kaboom".into(),
            problem_type: None,
            request_id: None,
            route: None,
            method: None,
            panic: Some(PanicInfo {
                payload: "kaboom".into(),
                backtrace: Some("<backtrace>".into()),
            }),
            capsule: None,
        }
    }

    #[tokio::test]
    async fn log_reporter_reports_both_event_kinds() {
        // Exercises both branches of the default reporter, including the
        // `unwrap_or` fallbacks for absent context.
        let reporter = LogReporter;
        reporter.report(&server_error_event()).await;
        reporter.report(&panic_event()).await;
    }

    #[test]
    fn sampled_fractional_uses_prng_and_varies() {
        // Drives the thread-local Xorshift PRNG path (seed + draws + f64 math)
        // that the rate-1.0 short-circuit never reaches.
        let mut trues = 0;
        for _ in 0..10_000 {
            if sampled(0.5) {
                trues += 1;
            }
        }
        assert!(
            trues > 0 && trues < 10_000,
            "fractional sampling should produce a mix of decisions, got {trues}"
        );
    }

    #[tokio::test]
    async fn reporter_panicking_while_constructing_future_is_swallowed() {
        // A reporter whose `report` method panics *before* returning a future
        // exercises the `Err` arm of `report_all` (distinct from a future that
        // panics when polled).
        struct PanicOnConstruct;
        impl ErrorReporter for PanicOnConstruct {
            fn report<'a>(&'a self, _event: &'a ErrorEvent) -> ReportFuture<'a> {
                panic!("panic before returning the future");
            }
        }

        let chain = ReporterChain {
            reporters: vec![Arc::new(PanicOnConstruct)],
            enabled: true,
            sample_rate: 1.0,
        };
        // Must complete without unwinding.
        chain.report_all(&server_error_event()).await;
    }

    #[test]
    fn dispatch_without_a_runtime_is_a_noop() {
        // A plain `#[test]` has no current tokio runtime, so dispatch should
        // take the best-effort early return rather than panic on spawn.
        let chain = Arc::new(ReporterChain {
            reporters: vec![Arc::new(LogReporter)],
            enabled: true,
            sample_rate: 1.0,
        });
        chain.dispatch(server_error_event(), None);
    }
}
