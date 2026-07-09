//! Server-Timing response header middleware (#1348).
//!
//! Emits a standards-conformant
//! [`Server-Timing`](https://www.w3.org/TR/server-timing/) header on
//! served responses when enabled by
//! [`ObservabilityConfig`](crate::config::ObservabilityConfig).
//!
//! The header carries at minimum a `total` metric (whole-request wall
//! time, measured with the same clock as the access log's `duration_ms`),
//! and a `db;dur=…;desc="N queries"` metric summarising cumulative query
//! time and query count when at least one instrumented DB query ran
//! during the request. The count is exposed via `desc` for N+1 visibility
//! in the browser's Network → Timing pane.
//!
//! The header is **off in production by default** and **on in the `dev`
//! profile**; see [`crate::config::server_timing_enabled`]. The middleware
//! is a no-op when disabled — it wraps the inner service directly and
//! never allocates.
//!
//! # Streaming responses
//!
//! Responses whose `content-type` is `text/event-stream` (SSE) carry only
//! the `total` metric. The `total` value reflects time to first byte —
//! response headers are flushed once the middleware returns — not the full
//! stream lifetime. Handlers that stream via other content types have a
//! full metric set attached; if the body's duration matters, prefer a
//! purpose-built metric emitted before the stream starts.
//!
//! # Units
//!
//! `dur` values are milliseconds as `f64`, rounded to three decimal places
//! (`format!("{:.3}", …)`). The `total` metric uses the identical formula
//! as [`AccessLogLayer`](crate::middleware::access_log) —
//! `elapsed().as_secs_f64() * 1000.0`. Because this layer is applied outer
//! to the access log, its `total` brackets the access-log `duration_ms`
//! and the two agree within a few microseconds for the same request (the
//! access log does path-matching work between the two `Instant` captures).
//! See
//! `docs/guide/observability/server-timing.md` in the repository for a
//! browser `DevTools` walk-through.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
#[cfg(feature = "db")]
use std::sync::Arc;
#[cfg(feature = "db")]
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http::{HeaderName, HeaderValue, Request, Response};
use pin_project_lite::pin_project;
use tower::{Layer, Service};

#[cfg(feature = "db")]
use crate::db::{REQUEST_DB_TIMINGS, RequestDbTimings};

/// Per-request DB-timing accumulator carried alongside the `Enabled` future.
///
/// With the `db` feature it is a shared [`RequestDbTimings`] the query
/// instrumentation records into via the task-local; without `db` there is
/// nothing to record, so it collapses to `()`.
#[cfg(feature = "db")]
type DbTimings = Arc<RequestDbTimings>;
#[cfg(not(feature = "db"))]
type DbTimings = ();

/// The inner future for the enabled path.
///
/// With the `db` feature the inner service future is wrapped in the
/// [`REQUEST_DB_TIMINGS`] task-local scope so DB query instrumentation can
/// record into the accumulator; without `db` it is the bare service future.
#[cfg(feature = "db")]
type ScopedInner<F> = tokio::task::futures::TaskLocalFuture<Arc<RequestDbTimings>, F>;
#[cfg(not(feature = "db"))]
type ScopedInner<F> = F;

/// Wrap the inner service future so DB query timings can be accumulated.
///
/// With the `db` feature this establishes the [`REQUEST_DB_TIMINGS`]
/// task-local scope and returns the accumulator to read back after poll;
/// without `db` it is a no-op passing the future through untouched.
#[cfg(feature = "db")]
fn scope_inner<F: Future>(fut: F) -> (DbTimings, ScopedInner<F>) {
    let timings = Arc::new(RequestDbTimings::default());
    // Scope the inner future so any DB query instrumentation that runs
    // during it can record into `timings` via the task-local. We keep a
    // clone of the Arc outside the scope to read back after poll.
    let scope = REQUEST_DB_TIMINGS.scope(Arc::clone(&timings), fut);
    (timings, scope)
}

#[cfg(not(feature = "db"))]
const fn scope_inner<F>(fut: F) -> (DbTimings, ScopedInner<F>) {
    ((), fut)
}

/// Snapshot the accumulated DB timing for a completed request.
///
/// Returns `(db_ms, query_count)`, with `db_ms` present only when at least
/// one instrumented query ran. Without the `db` feature the query
/// instrumentation is compiled out, so the snapshot is always `(None, 0)`.
#[cfg(feature = "db")]
fn read_db_snapshot(timings: &DbTimings) -> (Option<f64>, usize) {
    let query_count = timings.query_count.load(Ordering::Relaxed);
    let db_micros = timings.total_us.load(Ordering::Relaxed);
    let db_ms = if query_count == 0 {
        None
    } else {
        // Match the `total` formula: microseconds → f64 ms.
        #[allow(clippy::cast_precision_loss)]
        Some((db_micros as f64) / 1000.0)
    };
    (db_ms, query_count)
}

// Without the `db` feature there is no query instrumentation, so the snapshot
// is always empty. The `&DbTimings` (a `&()`) parameter keeps the signature
// identical to the `db` variant so the call site needs no feature gate.
#[cfg(not(feature = "db"))]
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn read_db_snapshot(_timings: &DbTimings) -> (Option<f64>, usize) {
    (None, 0)
}

/// Header name for the `Server-Timing` response header.
static SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

/// Tower [`Layer`] that stamps a `Server-Timing` response header on every
/// served response.
///
/// Applied automatically by the framework router when
/// [`server_timing_enabled`](crate::config::server_timing_enabled) resolves
/// to `true` for the active configuration/profile.
#[derive(Clone, Debug)]
pub struct ServerTimingLayer {
    enabled: bool,
}

impl ServerTimingLayer {
    /// Create a new layer. Pass `enabled = false` to make the layer a
    /// pass-through no-op — used in tests and by the router when the
    /// feature is turned off.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl<S> Layer<S> for ServerTimingLayer {
    type Service = ServerTimingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ServerTimingService {
            inner,
            enabled: self.enabled,
        }
    }
}

/// Tower [`Service`] produced by [`ServerTimingLayer`]. Constructed by the
/// layer — you do not build this directly.
#[derive(Clone, Debug)]
pub struct ServerTimingService<S> {
    inner: S,
    enabled: bool,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for ServerTimingService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ServerTimingFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        if !self.enabled {
            return ServerTimingFuture::Disabled {
                inner: self.inner.call(req),
            };
        }
        let (timings, inner) = scope_inner(self.inner.call(req));
        ServerTimingFuture::Enabled {
            start: Instant::now(),
            timings,
            inner,
        }
    }
}

pin_project! {
    /// Future that stamps the `Server-Timing` header on the returned response.
    ///
    /// Two variants: `Disabled` passes the inner future through untouched;
    /// `Enabled` scopes the DB task-local and computes the header when the
    /// inner future resolves.
    #[project = ServerTimingProj]
    pub enum ServerTimingFuture<F: Future> {
        Disabled {
            #[pin] inner: F,
        },
        Enabled {
            start: Instant,
            timings: DbTimings,
            #[pin] inner: ScopedInner<F>,
        },
    }
}

impl<F, ResBody, E> Future for ServerTimingFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ServerTimingProj::Disabled { inner } => inner.poll(cx),
            ServerTimingProj::Enabled {
                start,
                timings,
                inner,
            } => match inner.poll(cx) {
                Poll::Ready(Ok(mut response)) => {
                    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
                    let (db_ms, query_count) = read_db_snapshot(timings);
                    let streaming = is_streaming_response(&response);
                    let value = build_header_value(total_ms, db_ms, query_count, streaming);
                    if let Ok(hv) = HeaderValue::from_str(&value) {
                        // Append rather than insert so any `Server-Timing`
                        // metrics a handler already emitted are preserved —
                        // the W3C spec allows multiple comma-joined metrics
                        // across repeated header lines.
                        response.headers_mut().append(SERVER_TIMING.clone(), hv);
                    }
                    Poll::Ready(Ok(response))
                }
                other => other,
            },
        }
    }
}

/// Return `true` when the response's `content-type` starts with
/// `text/event-stream` (case-insensitive) — i.e. an SSE stream, whose
/// timing header should carry only the `total` metric.
fn is_streaming_response<B>(response: &Response<B>) -> bool {
    response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
}

/// Build the `Server-Timing` header value from measured request metrics.
///
/// Extracted for unit tests; the middleware calls this on every enabled
/// response. The output is always a `total;dur=<ms>` metric, and — unless
/// `streaming` is set — a `db;dur=<ms>;desc="N queries"` metric when at
/// least one DB query ran. Durations are rounded to three decimals.
pub fn build_header_value(
    total_ms: f64,
    db_ms: Option<f64>,
    query_count: usize,
    streaming: bool,
) -> String {
    let mut out = format!("total;dur={total_ms:.3}");
    if streaming {
        return out;
    }
    if let Some(db) = db_ms
        && query_count > 0
    {
        let noun = if query_count == 1 { "query" } else { "queries" };
        // Header format is bounded and small; write! into the buffer to
        // avoid an intermediate allocation.
        let _ = write!(out, ", db;dur={db:.3};desc=\"{query_count} {noun}\"");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_header_value_total_only_when_no_queries() {
        let v = build_header_value(12.345, None, 0, false);
        assert_eq!(v, "total;dur=12.345");
    }

    #[test]
    fn build_header_value_db_uses_singular_query_word() {
        let v = build_header_value(20.0, Some(3.5), 1, false);
        assert_eq!(v, "total;dur=20.000, db;dur=3.500;desc=\"1 query\"");
    }

    #[test]
    fn build_header_value_db_uses_plural_queries_word() {
        let v = build_header_value(50.0, Some(21.25), 14, false);
        assert_eq!(v, "total;dur=50.000, db;dur=21.250;desc=\"14 queries\"");
    }

    #[test]
    fn build_header_value_streaming_omits_db_even_when_present() {
        let v = build_header_value(9.0, Some(4.0), 2, true);
        assert_eq!(v, "total;dur=9.000");
    }

    #[test]
    fn build_header_value_zero_query_count_omits_db_even_with_ms() {
        let v = build_header_value(1.0, Some(0.0), 0, false);
        assert_eq!(v, "total;dur=1.000");
    }

    /// End-to-end coverage of the db-metric path without a live database:
    /// enter the `REQUEST_DB_TIMINGS` task-local scope the middleware sets,
    /// record two queries through the same crate-internal hook `db.rs`'s
    /// `run_instrumented` calls, snapshot the accumulator, then feed it into
    /// [`build_header_value`] and assert the emitted `db;dur=…;desc="N
    /// queries"` metric. This mirrors what a real DB request produces.
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn db_accumulator_feeds_header_value() {
        use std::time::Duration;

        let timings = Arc::new(RequestDbTimings::default());
        REQUEST_DB_TIMINGS
            .scope(Arc::clone(&timings), async {
                crate::db::record_request_db_query(Duration::from_micros(1_500));
                crate::db::record_request_db_query(Duration::from_micros(2_500));
            })
            .await;

        let query_count = timings.query_count.load(Ordering::Relaxed);
        let db_micros = timings.total_us.load(Ordering::Relaxed);
        assert_eq!(
            query_count, 2,
            "both instrumented queries should be counted"
        );
        assert_eq!(
            db_micros, 4_000,
            "microsecond totals should accumulate (1500 + 2500)"
        );

        #[allow(clippy::cast_precision_loss)]
        let db_ms = (db_micros as f64) / 1000.0;
        let value = build_header_value(50.0, Some(db_ms), query_count, false);
        assert_eq!(
            value, "total;dur=50.000, db;dur=4.000;desc=\"2 queries\"",
            "accumulated db time and query count should surface in the header"
        );
    }
}
