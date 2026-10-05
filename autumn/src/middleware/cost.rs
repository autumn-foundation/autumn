//! Layer that measures the cost of each request (issue #1720).
//!
//! [`CostLayer`] measures each request and adds it to a
//! [`CostAccountant`]. For each poll of the request future, it reads the
//! thread CPU clock (and the [`AllocationProbe`], if set) before and after
//! the poll. It counts DB queries through a task-local lane. When the request
//! completes, it reads the tenant from the request log context. When the
//! server drops the request before it completes (the client went away), the
//! layer records the cost that it measured until then.
//!
//! When `emit_header` is `true`, the layer appends these `Server-Timing`
//! metrics:
//!
//! ```text
//! cost-cpu;dur=1.234, cost-db;desc="3 queries", cost-alloc;desc="4096 bytes"
//! ```
//!
//! `cost-alloc` is present only when a probe is set. The router sets
//! `emit_header` from the `[observability] server_timing` setting, so cost data
//! does not go to clients when that header is off. `cost-cpu` is a precise
//! timing signal. Do not turn the header on for anonymous clients in
//! production.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::http::{HeaderName, HeaderValue, Request, Response};
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::cost::{AllocationProbe, CostAccountant, RequestCost, RequestCostCell};

static SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

/// Tower [`Layer`] that meters the cost of each request.
///
/// The framework installs it when `[cost] enabled = true`.
#[derive(Clone, Debug)]
pub struct CostLayer {
    accountant: CostAccountant,
    emit_header: bool,
}

impl CostLayer {
    /// Make a layer that records into `accountant`.
    ///
    /// Set `emit_header` to append the cost metrics to `Server-Timing`.
    #[must_use]
    pub const fn new(accountant: CostAccountant, emit_header: bool) -> Self {
        Self {
            accountant,
            emit_header,
        }
    }
}

impl<S> Layer<S> for CostLayer {
    type Service = CostService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CostService {
            inner,
            accountant: self.accountant.clone(),
            emit_header: self.emit_header,
        }
    }
}

/// Tower [`Service`] made by [`CostLayer`].
#[derive(Clone, Debug)]
pub struct CostService<S> {
    inner: S,
    accountant: CostAccountant,
    emit_header: bool,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for CostService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = CostFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let cell = Arc::new(RequestCostCell::default());
        // Measure the CPU time of the call too: some services do their work
        // here. The probe measures only the polls.
        let mark = crate::cost::cpu_mark();
        let inner = crate::cost::scope_request(Arc::clone(&cell), self.inner.call(req));
        let cpu = crate::cost::cpu_since(mark);
        CostFuture {
            inner,
            cell,
            // Hold the request's log context: tenancy writes the tenant into
            // it, and a dropped request is recorded outside its scope.
            log: crate::log::context::current(),
            probe: self.accountant.allocation_probe().cloned(),
            cpu,
            allocated: 0,
            accountant: self.accountant.clone(),
            emit_header: self.emit_header,
            recorded: false,
        }
    }
}

/// Run `f` and add its CPU time and allocated bytes to the totals.
fn measure(
    probe: Option<&dyn AllocationProbe>,
    cpu: &mut std::time::Duration,
    allocated: &mut u64,
    f: impl FnOnce(),
) {
    let mut f = Some(f);
    let mark = crate::cost::cpu_mark();
    if let Some(probe) = probe {
        let bytes = probe.measure(&mut || {
            if let Some(f) = f.take() {
                f();
            }
        });
        *allocated = allocated.saturating_add(bytes);
    }
    // No probe, or a probe that did not call `poll`: run it here.
    if let Some(f) = f.take() {
        f();
    }
    *cpu = cpu.saturating_add(crate::cost::cpu_since(mark));
}

pin_project! {
    /// Future made by [`CostService`]. It measures each poll.
    pub struct CostFuture<F> {
        #[pin]
        inner: crate::cost::ScopedRequest<F>,
        cell: Arc<RequestCostCell>,
        log: Option<crate::log::context::LogContext>,
        probe: Option<Arc<dyn AllocationProbe>>,
        cpu: std::time::Duration,
        allocated: u64,
        accountant: CostAccountant,
        emit_header: bool,
        // `true` after the cost is in the accountant.
        recorded: bool,
    }

    impl<F> PinnedDrop for CostFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            // A request dropped before it completed still used CPU.
            if !*this.recorded {
                record(this.accountant, this.log.as_ref(), *this.cpu, *this.allocated, this.cell);
            }
        }
    }
}

/// Add one request to `accountant`, with the tenant of its log context.
fn record(
    accountant: &CostAccountant,
    log: Option<&crate::log::context::LogContext>,
    cpu: std::time::Duration,
    allocated: u64,
    cell: &RequestCostCell,
) {
    let record = |tenant: Option<&str>| {
        accountant.record_parts(cpu, allocated, cell.db_queries(), tenant);
    };
    match log {
        Some(log) => log.with_tenant_id(record),
        None => record(None),
    }
}

impl<F, ResBody, E> Future for CostFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut inner = this.inner;
        let mut out = Poll::Pending;
        measure(this.probe.as_deref(), this.cpu, this.allocated, || {
            out = inner.as_mut().poll(cx);
        });
        match out {
            Poll::Ready(Ok(mut response)) => {
                *this.recorded = true;
                record(
                    this.accountant,
                    this.log.as_ref(),
                    *this.cpu,
                    *this.allocated,
                    this.cell,
                );
                if *this.emit_header {
                    let cost = RequestCost::new(*this.cpu, *this.allocated, this.cell.db_queries());
                    let value = build_header_value(&cost, this.probe.is_some());
                    if let Ok(value) = HeaderValue::from_str(&value) {
                        response.headers_mut().append(SERVER_TIMING.clone(), value);
                    }
                }
                Poll::Ready(Ok(response))
            }
            Poll::Ready(Err(error)) => {
                *this.recorded = true;
                record(
                    this.accountant,
                    this.log.as_ref(),
                    *this.cpu,
                    *this.allocated,
                    this.cell,
                );
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Build the `Server-Timing` value for one request cost.
///
/// `with_alloc` adds the `cost-alloc` metric. CPU time is in milliseconds with
/// three decimals, the same unit as the other `Server-Timing` metrics.
#[must_use]
pub fn build_header_value(cost: &RequestCost, with_alloc: bool) -> String {
    let cpu_ms = cost.cpu.as_secs_f64() * 1000.0;
    let noun = if cost.db_queries == 1 {
        "query"
    } else {
        "queries"
    };
    let mut out = format!(
        "cost-cpu;dur={cpu_ms:.3}, cost-db;desc=\"{} {noun}\"",
        cost.db_queries
    );
    if with_alloc {
        let _ = write!(out, ", cost-alloc;desc=\"{} bytes\"", cost.allocated_bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn header_has_cpu_and_db_metrics() {
        let cost = RequestCost::new(Duration::from_micros(1_234), 99, 3);
        assert_eq!(
            build_header_value(&cost, false),
            "cost-cpu;dur=1.234, cost-db;desc=\"3 queries\""
        );
    }

    #[test]
    fn header_uses_singular_query_word() {
        let cost = RequestCost::new(Duration::ZERO, 0, 1);
        assert_eq!(
            build_header_value(&cost, false),
            "cost-cpu;dur=0.000, cost-db;desc=\"1 query\""
        );
    }

    #[test]
    fn header_has_alloc_metric_with_a_probe() {
        let cost = RequestCost::new(Duration::from_millis(2), 4096, 0);
        assert_eq!(
            build_header_value(&cost, true),
            "cost-cpu;dur=2.000, cost-db;desc=\"0 queries\", cost-alloc;desc=\"4096 bytes\""
        );
    }

    struct FixedProbe(u64);

    impl AllocationProbe for FixedProbe {
        fn measure(&self, poll: &mut dyn FnMut()) -> u64 {
            poll();
            self.0
        }
    }

    #[tokio::test]
    async fn layer_records_each_request_with_probe_bytes() {
        use tower::ServiceExt as _;

        let accountant = CostAccountant::new(4).with_allocation_probe(Arc::new(FixedProbe(10)));
        let service = CostLayer::new(accountant.clone(), true).layer(tower::service_fn(
            |_req: Request<()>| async {
                // Yield one time so the future is polled two times.
                tokio::task::yield_now().await;
                Ok::<_, std::convert::Infallible>(Response::new(()))
            },
        ));

        let response = service.oneshot(Request::new(())).await.expect("infallible");
        let header = response
            .headers()
            .get("server-timing")
            .and_then(|v| v.to_str().ok())
            .expect("header is set")
            .to_owned();
        assert!(header.contains("cost-alloc;desc=\""), "{header}");

        let total = accountant.snapshot().total;
        assert_eq!(total.requests, 1);
        // At least two polls, each counted by the probe.
        assert!(total.allocated_bytes >= 20, "{total:?}");
    }

    /// A probe that does not call `poll`.
    struct LazyProbe;

    impl AllocationProbe for LazyProbe {
        fn measure(&self, _poll: &mut dyn FnMut()) -> u64 {
            0
        }
    }

    #[tokio::test]
    async fn a_probe_that_skips_poll_does_not_break_the_request() {
        use tower::ServiceExt as _;

        let accountant = CostAccountant::new(4).with_allocation_probe(Arc::new(LazyProbe));
        let service = CostLayer::new(accountant.clone(), false).layer(tower::service_fn(
            |_req: Request<()>| async { Ok::<_, std::convert::Infallible>(Response::new(())) },
        ));
        service.oneshot(Request::new(())).await.expect("infallible");
        assert_eq!(accountant.snapshot().total.requests, 1);
    }

    #[tokio::test]
    async fn a_dropped_request_keeps_its_tenant() {
        use crate::log::context::{LogContext, sync_scope};

        let accountant = CostAccountant::new(4);
        let mut service = CostLayer::new(accountant.clone(), false).layer(tower::service_fn(
            |_req: Request<()>| async {
                // Tenancy writes the tenant, then the handler never ends.
                if let Some(ctx) = crate::log::context::current() {
                    ctx.set_tenant_id("acme");
                }
                std::future::pending::<()>().await;
                Ok::<_, std::convert::Infallible>(Response::new(()))
            },
        ));
        // Call and poll the request one time inside the scope.
        let request = sync_scope(LogContext::new(None), || {
            let mut request = Box::pin(service.call(Request::new(())));
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(request.as_mut().poll(&mut cx).is_pending());
            request
        });
        // Drop the request outside the log-context scope.
        drop(request);
        assert_eq!(accountant.tenant("acme").map(|t| t.requests), Some(1));
    }

    #[tokio::test]
    async fn a_dropped_request_is_recorded_once() {
        let accountant = CostAccountant::new(4);
        let mut service = CostLayer::new(accountant.clone(), false).layer(tower::service_fn(
            |_req: Request<()>| async {
                std::future::pending::<()>().await;
                Ok::<_, std::convert::Infallible>(Response::new(()))
            },
        ));
        let mut future = Box::pin(service.call(Request::new(())));
        // Poll one time, then drop it: the client went away.
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        drop(future);
        assert_eq!(accountant.snapshot().total.requests, 1);
    }
}
