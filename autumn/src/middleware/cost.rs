//! Per-request cost metering layer (issue #1720).
//!
//! [`CostLayer`] measures each request and adds it to a
//! [`CostAccountant`]. For each poll of the request future it reads the
//! thread CPU clock (and the [`AllocationProbe`], if set) before and after
//! the poll. It counts DB queries through a task-local lane. When the request
//! completes, it reads the tenant from the request log context.
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
//! does not go to clients when that header is off.

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
        // Measure the call too: some services do their work here.
        let mut inner = None;
        let mut cpu = std::time::Duration::ZERO;
        let mut allocated = 0;
        let probe = self.accountant.allocation_probe().cloned();
        measure(probe.as_deref(), &mut cpu, &mut allocated, || {
            inner = Some(crate::cost::scope_request(
                Arc::clone(&cell),
                self.inner.call(req),
            ));
        });
        CostFuture {
            inner: inner.expect("measure runs the closure"),
            cell,
            probe,
            cpu,
            allocated,
            accountant: self.accountant.clone(),
            emit_header: self.emit_header,
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
    match probe {
        Some(probe) => {
            let bytes = probe.measure(&mut || {
                if let Some(f) = f.take() {
                    f();
                }
            });
            *allocated = allocated.saturating_add(bytes);
        }
        None => {
            if let Some(f) = f.take() {
                f();
            }
        }
    }
    *cpu = cpu.saturating_add(crate::cost::cpu_since(mark));
}

pin_project! {
    /// Future made by [`CostService`]. It measures each poll.
    pub struct CostFuture<F> {
        #[pin]
        inner: crate::cost::ScopedRequest<F>,
        cell: Arc<RequestCostCell>,
        probe: Option<Arc<dyn AllocationProbe>>,
        cpu: std::time::Duration,
        allocated: u64,
        accountant: CostAccountant,
        emit_header: bool,
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
        let mut out = None;
        measure(this.probe.as_deref(), this.cpu, this.allocated, || {
            out = Some(inner.as_mut().poll(cx));
        });
        match out.expect("measure runs the closure") {
            Poll::Ready(Ok(mut response)) => {
                let mut cost = RequestCost::new(*this.cpu, *this.allocated, this.cell.db_queries());
                cost.tenant = crate::log::context::current_tenant_id();
                this.accountant.record(&cost);
                if *this.emit_header {
                    let value = build_header_value(&cost, this.probe.is_some());
                    if let Ok(value) = HeaderValue::from_str(&value) {
                        response.headers_mut().append(SERVER_TIMING.clone(), value);
                    }
                }
                Poll::Ready(Ok(response))
            }
            other => other,
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

        let accountant = CostAccountant::with_allocation_probe(4, Arc::new(FixedProbe(10)));
        let service = CostLayer::new(accountant.clone(), true).layer(tower::service_fn(
            |_req: Request<()>| async {
                // Yield one time so the future is polled two times.
                tokio::task::yield_now().await;
                Ok::<_, std::convert::Infallible>(Response::new(()))
            },
        ));

        let response = service
            .oneshot(Request::new(()))
            .await
            .expect("infallible");
        let header = response
            .headers()
            .get("server-timing")
            .and_then(|v| v.to_str().ok())
            .expect("header is set")
            .to_owned();
        assert!(header.contains("cost-alloc;desc=\""), "{header}");

        let total = accountant.snapshot().total;
        assert_eq!(total.requests, 1);
        // One call and at least two polls, each counted by the probe.
        assert!(total.allocated_bytes >= 30, "{total:?}");
    }
}
