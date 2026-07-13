//! Metrics middleware -- records request count, latency, and status code distribution.
//!
//! The [`MetricsLayer`] is applied automatically by the framework when building
//! the router. It instruments every request and records metrics into a shared
//! [`MetricsCollector`] stored in `AppState`.
//!
//! Metrics are exposed via the `/actuator/metrics` endpoint.

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

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::extract::MatchedPath;
use axum::http::{Request, Response};
use pin_project_lite::pin_project;
use serde::Serialize;
use tower::{Layer, Service};

/// Shared metrics collector that aggregates request statistics.
#[derive(Debug, Clone)]
pub struct MetricsCollector {
    inner: Arc<MetricsInner>,
}

#[derive(Debug, Default)]
struct Shard {
    by_route: HashMap<String, RouteMetrics>,
    by_query: HashMap<String, RouteMetrics>,
}

#[derive(Debug)]
struct MetricsInner {
    /// Total number of requests received.
    requests_total: AtomicU64,
    /// Currently active (in-flight) requests.
    requests_active: AtomicU64,
    /// Per-route metrics sharded to reduce lock contention: key is "METHOD /path".
    shards: Vec<RwLock<Shard>>,
    /// Status code buckets: 2xx, 3xx, 4xx, 5xx.
    by_status: StatusBuckets,
    /// Global latency samples (bounded ring buffer).
    latencies_ms: RwLock<VecDeque<u64>>,
    /// Idempotency-key cache hits (replayed responses).
    idempotency_hits: AtomicU64,
    /// Idempotency-key cache misses (new requests).
    idempotency_misses: AtomicU64,
    /// Idempotency-key conflicts (concurrent duplicate requests returned 409).
    idempotency_conflicts: AtomicU64,
    /// Requests still in-flight when the drain deadline expired and were
    /// forcibly dropped. Exposed as `autumn_shutdown_aborted_requests_total`.
    shutdown_aborted_requests: AtomicU64,
    /// Requests that exceeded the configured per-request timeout.
    /// Exposed as `autumn_request_timeouts_total`.
    request_timeouts_total: AtomicU64,
    /// Replica reads redirected to the primary by the RYWW pin.
    /// Exposed as `autumn_read_your_writes_pins_total`.
    read_your_writes_pins_total: AtomicU64,
    /// Requests rejected by the admission-control (load-shedding) layer
    /// because `server.max_concurrent_requests` was at its ceiling.
    /// Exposed as `autumn_requests_shed_total`.
    requests_shed_total: AtomicU64,
}

#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)]
struct StatusBuckets {
    status_2xx: AtomicU64,
    status_3xx: AtomicU64,
    status_4xx: AtomicU64,
    status_5xx: AtomicU64,
}

#[derive(Debug, Clone)]
struct RouteMetrics {
    count: u64,
    latencies_ms: VecDeque<u64>,
}

impl Default for RouteMetrics {
    fn default() -> Self {
        Self {
            count: 0,
            latencies_ms: VecDeque::with_capacity(MAX_LATENCY_SAMPLES),
        }
    }
}

/// Maximum number of latency samples to keep per route.
const MAX_LATENCY_SAMPLES: usize = 10_000;
const SHARD_COUNT: usize = 16;

impl MetricsCollector {
    /// Create a new empty metrics collector.
    #[must_use]
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            shards.push(RwLock::new(Shard::default()));
        }
        Self {
            inner: Arc::new(MetricsInner {
                requests_total: AtomicU64::new(0),
                requests_active: AtomicU64::new(0),
                shards,
                by_status: StatusBuckets::default(),
                latencies_ms: RwLock::new(VecDeque::with_capacity(MAX_LATENCY_SAMPLES)),
                idempotency_hits: AtomicU64::new(0),
                idempotency_misses: AtomicU64::new(0),
                idempotency_conflicts: AtomicU64::new(0),
                shutdown_aborted_requests: AtomicU64::new(0),
                request_timeouts_total: AtomicU64::new(0),
                read_your_writes_pins_total: AtomicU64::new(0),
                requests_shed_total: AtomicU64::new(0),
            }),
        }
    }

    /// Record `count` requests that were forcibly aborted because the drain
    /// deadline expired. Increments `autumn_shutdown_aborted_requests_total`.
    pub fn record_shutdown_aborted(&self, count: u64) {
        if count > 0 {
            self.inner
                .shutdown_aborted_requests
                .fetch_add(count, Ordering::Relaxed);
        }
    }

    /// Increment the per-request timeout counter (`autumn_request_timeouts_total`).
    pub fn record_request_timeout(&self) {
        self.inner
            .request_timeouts_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the load-shedding counter (`autumn_requests_shed_total`) —
    /// a request rejected because `server.max_concurrent_requests` was at
    /// its ceiling.
    pub fn record_request_shed(&self) {
        self.inner
            .requests_shed_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the read-your-own-writes pin redirect counter.
    pub fn record_read_your_writes_pin(&self) {
        self.inner
            .read_your_writes_pins_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the idempotency cache hit counter.
    pub fn record_idempotency_hit(&self) {
        self.inner.idempotency_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the idempotency cache miss counter.
    pub fn record_idempotency_miss(&self) {
        self.inner
            .idempotency_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the idempotency conflict counter (concurrent duplicate request).
    pub fn record_idempotency_conflict(&self) {
        self.inner
            .idempotency_conflicts
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a completed request.
    pub fn record(&self, method: &str, route: &str, status: u16, latency_ms: u64) {
        self.inner.requests_total.fetch_add(1, Ordering::Relaxed);
        self.record_status(status);
        self.record_global_latency(latency_ms);
        self.record_route(method, route, latency_ms);
    }

    fn record_status(&self, status: u16) {
        match status / 100 {
            2 => self
                .inner
                .by_status
                .status_2xx
                .fetch_add(1, Ordering::Relaxed),
            3 => self
                .inner
                .by_status
                .status_3xx
                .fetch_add(1, Ordering::Relaxed),
            4 => self
                .inner
                .by_status
                .status_4xx
                .fetch_add(1, Ordering::Relaxed),
            5 => self
                .inner
                .by_status
                .status_5xx
                .fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }

    fn record_global_latency(&self, latency_ms: u64) {
        if let Ok(mut latencies) = self.inner.latencies_ms.write() {
            if latencies.len() >= MAX_LATENCY_SAMPLES {
                latencies.pop_front();
            }
            latencies.push_back(latency_ms);
        }
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "shard_idx is hash % SHARD_COUNT so always in bounds; buf slice length is <= 256, the buffer size"
    )]
    fn record_route(&self, method: &str, route: &str, latency_ms: u64) {
        // ⚡ Bolt Optimization:
        // FNV-1a hash is faster than DefaultHasher (SipHash) for short strings like routes.
        // We don't need cryptographic security or HashDoS resistance here since this is internal.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in route.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }

        let shard_idx = usize::try_from(hash % (SHARD_COUNT as u64)).unwrap_or_default();

        // ⚡ Bolt Optimization:
        // Format the key into a stack-allocated buffer to avoid a heap allocation
        // on every request. Fall back to allocating a String only if the route
        // is exceptionally long (which is rare) or if it's a new route missing
        // from the metrics map.
        let mut buf = [0u8; 256];
        let key_str = {
            let mut cursor = &mut buf[..];
            if std::io::Write::write_fmt(&mut cursor, format_args!("{method} {route}")).is_ok() {
                let len = 256 - cursor.len();
                std::str::from_utf8(&buf[..len]).unwrap_or_default()
            } else {
                ""
            }
        };

        let mut is_new = false;
        if let Ok(mut shard) = self.inner.shards[shard_idx].write() {
            if let Some(entry) = shard.by_route.get_mut(key_str) {
                entry.count += 1;
                if entry.latencies_ms.len() >= MAX_LATENCY_SAMPLES {
                    entry.latencies_ms.pop_front();
                }
                entry.latencies_ms.push_back(latency_ms);
            } else {
                is_new = true;
            }
        }

        if is_new {
            let key = if key_str.is_empty() {
                format!("{method} {route}")
            } else {
                key_str.to_owned()
            };
            if let Ok(mut shard) = self.inner.shards[shard_idx].write() {
                let entry = shard.by_route.entry(key).or_default();
                entry.count += 1;
                if entry.latencies_ms.len() >= MAX_LATENCY_SAMPLES {
                    entry.latencies_ms.pop_front();
                }
                entry.latencies_ms.push_back(latency_ms);
            }
        }
    }

    /// Record a database query's duration.
    #[allow(
        clippy::indexing_slicing,
        reason = "shard_idx is hash % SHARD_COUNT so always in bounds"
    )]
    pub fn record_db_query(&self, key: &str, latency_ms: u64) {
        // Hash key to determine shard index using FNV-1a
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        let shard_idx = usize::try_from(hash % (SHARD_COUNT as u64)).unwrap_or_default();

        if let Ok(mut shard) = self.inner.shards[shard_idx].write() {
            let entry = shard.by_query.entry(key.to_owned()).or_default();
            entry.count += 1;
            if entry.latencies_ms.len() >= MAX_LATENCY_SAMPLES {
                entry.latencies_ms.pop_front();
            }
            entry.latencies_ms.push_back(latency_ms);
        }
    }

    fn increment_active(&self) {
        self.inner.requests_active.fetch_add(1, Ordering::Relaxed);
    }

    fn decrement_active(&self) {
        self.inner.requests_active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Produce a snapshot of current metrics for the `/actuator/metrics` endpoint.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let global_latency = self
            .inner
            .latencies_ms
            .read()
            .map(|v| compute_percentiles(&v))
            .unwrap_or_default();

        let mut by_route = HashMap::new();
        let mut db_queries = HashMap::new();
        for shard_lock in &self.inner.shards {
            if let Ok(shard) = shard_lock.read() {
                for (k, v) in &shard.by_route {
                    let pcts = compute_percentiles(&v.latencies_ms);
                    by_route.insert(
                        k.clone(),
                        RouteSnapshot {
                            count: v.count,
                            p50_ms: pcts.p50,
                            p95_ms: pcts.p95,
                            p99_ms: pcts.p99,
                        },
                    );
                }
                for (k, v) in &shard.by_query {
                    let pcts = compute_percentiles(&v.latencies_ms);
                    db_queries.insert(
                        k.clone(),
                        DbQueryMetric {
                            count: v.count,
                            p50_ms: pcts.p50,
                            p95_ms: pcts.p95,
                            p99_ms: pcts.p99,
                        },
                    );
                }
            }
        }

        MetricsSnapshot {
            http: HttpMetrics {
                requests_total: self.inner.requests_total.load(Ordering::Relaxed),
                requests_active: self.inner.requests_active.load(Ordering::Relaxed),
                latency_ms: LatencySnapshot {
                    p50: global_latency.p50,
                    p95: global_latency.p95,
                    p99: global_latency.p99,
                },
                by_route,
                by_status: StatusSnapshot {
                    s2xx: self.inner.by_status.status_2xx.load(Ordering::Relaxed),
                    s3xx: self.inner.by_status.status_3xx.load(Ordering::Relaxed),
                    s4xx: self.inner.by_status.status_4xx.load(Ordering::Relaxed),
                    s5xx: self.inner.by_status.status_5xx.load(Ordering::Relaxed),
                },
                shutdown_aborted_requests_total: self
                    .inner
                    .shutdown_aborted_requests
                    .load(Ordering::Relaxed),
                request_timeouts_total: self.inner.request_timeouts_total.load(Ordering::Relaxed),
                requests_shed_total: self.inner.requests_shed_total.load(Ordering::Relaxed),
            },
            idempotency: IdempotencyMetricsSnapshot {
                hits: self.inner.idempotency_hits.load(Ordering::Relaxed),
                misses: self.inner.idempotency_misses.load(Ordering::Relaxed),
                conflicts: self.inner.idempotency_conflicts.load(Ordering::Relaxed),
            },
            db_queries,
            read_your_writes_pins_total: self
                .inner
                .read_your_writes_pins_total
                .load(Ordering::Relaxed),
        }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Serializable DB query snapshot returned in the `/actuator/metrics` JSON object under "`db_queries`".
#[derive(Serialize, Clone, Debug)]
pub struct DbQueryMetric {
    pub count: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
}

/// Serializable metrics snapshot returned by `/actuator/metrics`.
#[derive(Serialize)]
pub struct MetricsSnapshot {
    /// HTTP-specific metrics including latency and status codes.
    pub http: HttpMetrics,
    /// Idempotency-key middleware counters (zero when middleware is not enabled).
    pub idempotency: IdempotencyMetricsSnapshot,
    /// Database queries tracked.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub db_queries: HashMap<String, DbQueryMetric>,
    /// Read-your-own-writes: number of replica reads redirected to the primary
    /// due to an active RYWW pin. Zero when `read_your_writes = "off"`.
    pub read_your_writes_pins_total: u64,
}

/// Idempotency-key middleware counters.
#[derive(Serialize, Default)]
pub struct IdempotencyMetricsSnapshot {
    /// Number of requests served from the idempotency cache (replayed responses).
    pub hits: u64,
    /// Number of new requests processed through to the handler.
    pub misses: u64,
    /// Number of 409 Conflict responses issued for concurrent duplicate keys.
    pub conflicts: u64,
}

/// A snapshot of HTTP metrics across the entire application.
#[derive(Serialize)]
pub struct HttpMetrics {
    /// Total number of requests processed.
    pub requests_total: u64,
    /// Number of currently active (in-flight) requests.
    pub requests_active: u64,
    /// Global latency percentiles (in milliseconds).
    pub latency_ms: LatencySnapshot,
    /// Metrics broken down by route ("METHOD /path").
    pub by_route: HashMap<String, RouteSnapshot>,
    /// Global distribution of HTTP status codes.
    pub by_status: StatusSnapshot,
    /// Requests forcibly dropped when the graceful-shutdown drain deadline
    /// expired (`autumn_shutdown_aborted_requests_total`).
    pub shutdown_aborted_requests_total: u64,
    /// Requests that exceeded the configured per-request timeout
    /// (`autumn_request_timeouts_total`).
    pub request_timeouts_total: u64,
    /// Requests rejected by the admission-control (load-shedding) layer
    /// because `server.max_concurrent_requests` was at its ceiling
    /// (`autumn_requests_shed_total`).
    pub requests_shed_total: u64,
}

/// Percentiles for latency measurements.
#[derive(Serialize, Default)]
pub struct LatencySnapshot {
    /// 50th percentile (median) latency in milliseconds.
    pub p50: u64,
    /// 95th percentile latency in milliseconds.
    pub p95: u64,
    /// 99th percentile latency in milliseconds.
    pub p99: u64,
}

/// Metrics for a specific route.
#[derive(Serialize)]
pub struct RouteSnapshot {
    /// Total number of requests to this route.
    pub count: u64,
    /// 50th percentile (median) latency for this route in milliseconds.
    pub p50_ms: u64,
    /// 95th percentile latency for this route in milliseconds.
    pub p95_ms: u64,
    /// 99th percentile latency for this route in milliseconds.
    pub p99_ms: u64,
}

/// Global distribution of HTTP status codes.
#[derive(Serialize)]
pub struct StatusSnapshot {
    /// Number of 2xx success responses.
    #[serde(rename = "2xx")]
    pub s2xx: u64,
    /// Number of 3xx redirection responses.
    #[serde(rename = "3xx")]
    pub s3xx: u64,
    /// Number of 4xx client error responses.
    #[serde(rename = "4xx")]
    pub s4xx: u64,
    /// Number of 5xx server error responses.
    #[serde(rename = "5xx")]
    pub s5xx: u64,
}

#[derive(Default)]
struct Percentiles {
    p50: u64,
    p95: u64,
    p99: u64,
}

#[allow(
    clippy::indexing_slicing,
    reason = "p95_idx <= p99_idx <= len - 1, so the inclusive ranges stay within data's bounds"
)]
fn compute_percentiles(latencies: &VecDeque<u64>) -> Percentiles {
    let len = latencies.len();
    if len == 0 {
        return Percentiles::default();
    }

    // Pre-allocate to exact capacity and use fast slice copying instead of iterating
    let mut data = Vec::with_capacity(len);
    let (slice1, slice2) = latencies.as_slices();
    data.extend_from_slice(slice1);
    data.extend_from_slice(slice2);

    let p50_idx = len * 50 / 100;
    let p95_idx = len.saturating_sub(1).min(len * 95 / 100);
    let p99_idx = len.saturating_sub(1).min(len * 99 / 100);

    // Use select_nth_unstable to find percentiles in O(N) time instead of O(N log N) sort
    let (_, &mut p99, _) = data.select_nth_unstable(p99_idx);

    // We only need to search the left partition for p95 since p95_idx <= p99_idx
    let p95 = if p95_idx == p99_idx {
        p99
    } else {
        let (_, &mut p95_val, _) = data[..=p99_idx].select_nth_unstable(p95_idx);
        p95_val
    };

    // We only need to search the left partition for p50 since p50_idx <= p95_idx
    let p50 = if p50_idx == p95_idx {
        p95
    } else {
        let (_, &mut p50_val, _) = data[..=p95_idx].select_nth_unstable(p50_idx);
        p50_val
    };

    Percentiles { p50, p95, p99 }
}

// ── Tower Layer / Service ───────────────────────────────────────

/// Tower [`Layer`] that wraps a service with `MetricsService`.
///
/// Records request count, latency, active connections, and status code
/// distribution into a shared [`MetricsCollector`].
#[derive(Clone)]
pub struct MetricsLayer {
    collector: MetricsCollector,
}

impl MetricsLayer {
    /// Create a new metrics layer backed by the given collector.
    #[must_use]
    pub const fn new(collector: MetricsCollector) -> Self {
        Self { collector }
    }
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MetricsService {
            inner,
            collector: self.collector.clone(),
        }
    }
}

/// Tower [`Service`] produced by [`MetricsLayer`].
#[derive(Clone)]
pub struct MetricsService<S> {
    inner: S,
    collector: MetricsCollector,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for MetricsService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = MetricsFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let method = req.method().clone();
        let route = req.extensions().get::<MatchedPath>().cloned();

        self.collector.increment_active();

        MetricsFuture {
            inner: self.inner.call(req),
            collector: Some(self.collector.clone()),
            method,
            route,
            start: Instant::now(),
        }
    }
}

pin_project! {
    #[project = MetricsFutureProj]
    /// Future that records metrics after the inner service completes.
    ///
    /// **Known limitation:** `requests_active` tracks the tower service future
    /// lifecycle, not the response-body lifecycle. For SSE / streaming handlers
    /// the service future completes when the handler returns the `Response`
    /// (with a streaming body), so `requests_active` is decremented before the
    /// body is fully sent to the client. This means the
    /// `autumn_shutdown_aborted_requests_total` watchdog counter may read `0`
    /// even when streaming connections are still open during graceful shutdown.
    /// Fixing this requires connection-level tracking at the Hyper layer.
    pub struct MetricsFuture<F> {
        #[pin]
        inner: F,
        collector: Option<MetricsCollector>,
        method: axum::http::Method,
        route: Option<MatchedPath>,
        start: Instant,
    }
    impl<F> PinnedDrop for MetricsFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(collector) = this.collector.take() {
                collector.decrement_active();
            }
        }
    }
}

impl<F, ResBody, E> Future for MetricsFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(Ok(response)) => {
                if let Some(collector) = this.collector.take() {
                    let latency_ms =
                        u64::try_from(this.start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let method_str = this.method.as_str();
                    let route_str = this.route.as_ref().map_or(
                        super::access_log::UNMATCHED_ROUTE,
                        axum::extract::MatchedPath::as_str,
                    );
                    let status = response.status().as_u16();
                    collector.record(method_str, route_str, status, latency_ms);
                    collector.decrement_active();
                }
                Poll::Ready(Ok(response))
            }
            Poll::Ready(Err(e)) => {
                if let Some(collector) = this.collector.take() {
                    collector.decrement_active();
                }
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_records_request() {
        let collector = MetricsCollector::new();
        collector.record("GET", "/test", 200, 10);
        collector.record("GET", "/test", 200, 20);
        collector.record("POST", "/test", 500, 50);

        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_total, 3);
        assert_eq!(snap.http.by_status.s2xx, 2);
        assert_eq!(snap.http.by_status.s5xx, 1);
        assert!(snap.http.by_route.contains_key("GET /test"));
        assert_eq!(snap.http.by_route["GET /test"].count, 2);
    }

    #[test]
    fn empty_collector_snapshot() {
        let collector = MetricsCollector::new();
        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_total, 0);
        assert_eq!(snap.http.requests_active, 0);
        assert_eq!(snap.http.latency_ms.p50, 0);
    }

    #[test]
    fn percentiles_computed_correctly() {
        let latencies: VecDeque<u64> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10].into();
        let pcts = compute_percentiles(&latencies);
        assert_eq!(pcts.p50, 6); // sorted[10*50/100] = sorted[5] = 6
        assert_eq!(pcts.p99, 10); // sorted[min(9, 10*99/100)] = sorted[9] = 10
    }

    #[test]
    fn active_connection_tracking() {
        let collector = MetricsCollector::new();
        collector.increment_active();
        collector.increment_active();
        assert_eq!(collector.inner.requests_active.load(Ordering::Relaxed), 2);
        collector.decrement_active();
        assert_eq!(collector.inner.requests_active.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn metrics_layer_records_requests() {
        use axum::Router;
        use axum::body::Body;
        use axum::routing::get;
        use tower::ServiceExt;

        let collector = MetricsCollector::new();
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(MetricsLayer::new(collector.clone()));

        let request = Request::builder()
            .method("GET")
            .uri("/test")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_total, 1);
        assert_eq!(snap.http.requests_active, 0);
        assert_eq!(snap.http.by_status.s2xx, 1);

        let route_key = "GET /test";
        assert!(snap.http.by_route.contains_key(route_key));
        assert_eq!(snap.http.by_route[route_key].count, 1);
    }

    #[tokio::test]
    async fn metrics_layer_handles_errors() {
        use axum::body::Body;
        use std::task::{Context, Poll};
        use tower::{Service, ServiceExt};

        // A custom service that fails instead of returning a response
        #[derive(Clone)]
        struct FailingService;

        impl Service<Request<Body>> for FailingService {
            type Response = Response<Body>;
            type Error = std::io::Error;
            type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _req: Request<Body>) -> Self::Future {
                std::future::ready(Err(std::io::Error::other("boom")))
            }
        }

        let collector = MetricsCollector::new();
        let mut svc = MetricsLayer::new(collector.clone()).layer(FailingService);

        // Active should be 0 initially
        assert_eq!(collector.inner.requests_active.load(Ordering::Relaxed), 0);

        let request = Request::builder()
            .method("GET")
            .uri("/fail")
            .body(Body::empty())
            .unwrap();

        // We know it will error
        let result = svc.ready().await.unwrap().call(request).await;
        assert!(result.is_err());

        // Ensure that even though it errored, the active connection count was decremented
        assert_eq!(collector.inner.requests_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn collector_records_request_very_long_route() {
        let collector = MetricsCollector::new();
        let long_route = "a".repeat(1000);

        collector.record("GET", &long_route, 200, 15);

        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_total, 1);

        // Let's verify the route was recorded correctly.
        let key = format!("GET {long_route}");
        let route_snap = snap.http.by_route.get(&key).unwrap();
        assert_eq!(route_snap.count, 1);

        // Record again to hit the other path
        collector.record("GET", &long_route, 200, 20);
        let snap2 = collector.snapshot();
        let route_snap2 = snap2.http.by_route.get(&key).unwrap();
        assert_eq!(route_snap2.count, 2);
    }

    // ── request_timeouts_total ─────────────────────────────────────────────

    #[test]
    fn collector_request_timeouts_starts_at_zero() {
        let collector = MetricsCollector::new();
        let snap = collector.snapshot();
        assert_eq!(snap.http.request_timeouts_total, 0);
    }

    #[test]
    fn collector_records_request_timeout() {
        let collector = MetricsCollector::new();
        collector.record_request_timeout();
        collector.record_request_timeout();
        let snap = collector.snapshot();
        assert_eq!(snap.http.request_timeouts_total, 2);
    }

    #[test]
    fn request_timeouts_total_independent_of_regular_requests() {
        let collector = MetricsCollector::new();
        collector.record("GET", "/api", 200, 5);
        collector.record_request_timeout();
        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_total, 1);
        assert_eq!(snap.http.request_timeouts_total, 1);
    }

    // ── requests_shed_total (#1006) ─────────────────────────────────────────

    #[test]
    fn collector_requests_shed_starts_at_zero() {
        let collector = MetricsCollector::new();
        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_shed_total, 0);
    }

    #[test]
    fn collector_records_request_shed() {
        let collector = MetricsCollector::new();
        collector.record_request_shed();
        collector.record_request_shed();
        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_shed_total, 2);
    }

    #[test]
    fn requests_shed_total_independent_of_regular_requests() {
        let collector = MetricsCollector::new();
        collector.record("GET", "/api", 200, 5);
        collector.record_request_shed();
        let snap = collector.snapshot();
        assert_eq!(snap.http.requests_total, 1);
        assert_eq!(snap.http.requests_shed_total, 1);
    }
}
