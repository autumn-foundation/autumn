//! H4: one report at a time, as a per-route tower layer.
//!
//! The stock-value report reads every product. Ten callers at once would
//! hold ten pool connections and slow every page. The framework has two
//! close tools, and neither fits:
//!
//! - `#[throttle]` limits each caller's request rate. It does not limit how
//!   many reports run at once across all callers.
//! - `timeout_ms` stops a slow request. It does not stop requests that pile up.
//!
//! So `#[intercept(ReportGate)]` puts this layer on the report route only.
//! When the one slot is in use, a caller gets `503` with `Retry-After: 1` at
//! once. Callers do not queue.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, StatusCode, header::RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use tokio::sync::{Semaphore, SemaphorePermit};

/// One slot for the whole process.
static REPORT_SLOTS: Semaphore = Semaphore::const_new(1);

/// The layer for `#[intercept(ReportGate)]`. `#[intercept]` takes a path, not
/// a call, so the layer is a unit struct and its state is a `static`.
#[derive(Clone, Copy, Debug)]
pub struct ReportGate;

impl<S> tower::Layer<S> for ReportGate {
    type Service = ReportGateService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ReportGateService { inner }
    }
}

/// The service that [`ReportGate`] makes.
#[derive(Clone, Debug)]
pub struct ReportGateService<S> {
    inner: S,
}

impl<S> tower::Service<Request<Body>> for ReportGateService<S>
where
    S: tower::Service<Request<Body>, Response = Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let Some(slot) = hold_report_slot() else {
            return Box::pin(async { Ok(busy()) });
        };
        // Use the service that `poll_ready` made ready. Leave a clone behind.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let response = inner.call(request).await;
            drop(slot);
            response
        })
    }
}

/// Take the only report slot, if it is free. The slot is free again when
/// the permit drops. Tests use this to fill the gate.
#[must_use]
pub fn hold_report_slot() -> Option<SemaphorePermit<'static>> {
    REPORT_SLOTS.try_acquire().ok()
}

fn busy() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, "1")],
        "The report is busy. Try again in one second.",
    )
        .into_response()
}
