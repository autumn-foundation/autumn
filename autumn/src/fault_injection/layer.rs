//! The two fault injection layers. See the module docs in `mod.rs`.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate".
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
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response};
use axum::response::IntoResponse;
use tower::{Layer, Service};

use super::{FAULT_HEADER, FaultTarget, Injector, RequestScope, SCOPE};

type BoxFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

/// The reason in the audit event when the stop condition trips.
const STOP_REASON: &str = "stop condition: the error budget burns too fast";

/// The outer layer. It scopes the matched faults for the request and counts
/// the final status for the stop condition.
#[derive(Clone)]
pub struct FaultScopeLayer {
    injector: Arc<Injector>,
}

impl FaultScopeLayer {
    pub(super) const fn new(injector: Arc<Injector>) -> Self {
        Self { injector }
    }
}

impl<S> Layer<S> for FaultScopeLayer {
    type Service = FaultScopeService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        FaultScopeService {
            inner,
            injector: Arc::clone(&self.injector),
        }
    }
}

/// Tower [`Service`] of the outer fault scope layer.
#[derive(Clone)]
pub struct FaultScopeService<S> {
    inner: S,
    injector: Arc<Injector>,
}

impl<S, B, R> Service<Request<B>> for FaultScopeService<S>
where
    S: Service<Request<B>, Response = Response<R>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    R: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let Some(scope) = self.injector.scope_for(req.uri().path()) else {
            return Box::pin(self.inner.call(req));
        };
        // Build the inner future in the scope too: an inner layer can read
        // the scope in its `call`, before the future is polled.
        let inner = SCOPE.sync_scope(Arc::clone(&scope), || self.inner.call(req));
        let inner = SCOPE.scope(Arc::clone(&scope), inner);
        // Before the first poll: a fault can fire in `call` already.
        let mut pending = Pending(Some(scope));
        Box::pin(async move {
            let result = inner.await;
            if let Some(scope) = pending.0.take() {
                // An injected error is bad at any status (a `4xx` too).
                let error = scope.errored.load(Ordering::Relaxed)
                    || result
                        .as_ref()
                        .map_or(true, |response| response.status().is_server_error());
                if scope.injector.record(scope.generation, error) {
                    scope.injector.audit_stop(STOP_REASON);
                }
            }
            result
        })
    }
}

/// Counts a request that is dropped before it completes (a timeout, or a
/// closed client connection). It counts as an error only when a fault fired,
/// so injected latency that causes a timeout counts against the error budget.
struct Pending(Option<Arc<RequestScope>>);

impl Drop for Pending {
    fn drop(&mut self) {
        let Some(scope) = self.0.take() else {
            return;
        };
        if scope.fired.load(Ordering::Relaxed) && scope.injector.record(scope.generation, true) {
            scope.injector.audit_stop(STOP_REASON);
        }
    }
}

/// Tower [`Layer`] that applies the route faults of `[fault_injection]`.
///
/// The router installs it inside the timeout layer when the section is
/// enabled. Outside a fault scope, it passes each request through.
#[derive(Clone, Debug, Default)]
pub struct FaultInjectionLayer {
    _private: (),
}

impl FaultInjectionLayer {
    pub(super) const fn new() -> Self {
        Self { _private: () }
    }
}

impl<S> Layer<S> for FaultInjectionLayer {
    type Service = FaultInjectionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        FaultInjectionService { inner }
    }
}

/// Tower [`Service`] produced by [`FaultInjectionLayer`].
#[derive(Clone, Debug)]
pub struct FaultInjectionService<S> {
    inner: S,
}

impl<S, B> Service<Request<B>> for FaultInjectionService<S>
where
    S: Service<Request<B>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<S::Response, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let fault = SCOPE
            .try_with(|scope| scope.roll(FaultTarget::Route))
            .unwrap_or_default();
        if let Some(status) = fault.error {
            return Box::pin(async move {
                if !fault.latency.is_zero() {
                    tokio::time::sleep(fault.latency).await;
                }
                Ok(injected_error(status))
            });
        }
        if fault.latency.is_zero() {
            return Box::pin(self.inner.call(req));
        }
        // Use the service that `poll_ready` made ready. Keep a clone in its
        // place.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            tokio::time::sleep(fault.latency).await;
            inner.call(req).await
        })
    }
}

fn injected_error(status: axum::http::StatusCode) -> Response<Body> {
    let mut response =
        crate::error::AutumnError::service_unavailable_msg("fault injection: injected route error")
            .with_status(status)
            .into_response();
    response
        .headers_mut()
        .insert(FAULT_HEADER, HeaderValue::from_static("injected"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use axum::routing::get;
    use tower::ServiceExt;

    use crate::fault_injection::{CompiledRule, FaultKind};

    fn injector(rules: Vec<CompiledRule>, min_requests: u64) -> Arc<Injector> {
        crate::fault_injection::test_injector(rules, min_requests)
    }

    fn error_rule(rate_ppm: u32) -> CompiledRule {
        CompiledRule {
            routes: Vec::new(),
            target: FaultTarget::Route,
            kind: FaultKind::Error,
            rate_ppm,
            latency: Duration::ZERO,
            status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn app(injector: &Arc<Injector>) -> axum::Router {
        axum::Router::new()
            .route("/x", get(|| async { "ok" }))
            .route("/live", get(|| async { "ok" }))
            .route("/actuator/health", get(|| async { "ok" }))
            .layer((
                FaultScopeLayer::new(Arc::clone(injector)),
                FaultInjectionLayer::new(),
            ))
    }

    async fn status(app: axum::Router, uri: &str) -> u16 {
        app.oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn exempt_paths_are_never_faulted() {
        let injector = injector(vec![error_rule(1_000_000)], 100);
        assert_eq!(status(app(&injector), "/x").await, 503);
        assert_eq!(status(app(&injector), "/live").await, 200);
        assert_eq!(status(app(&injector), "/actuator/health").await, 200);
    }

    #[tokio::test]
    async fn a_zero_rate_never_fires() {
        let injector = injector(vec![error_rule(0)], 100);
        for _ in 0..20 {
            assert_eq!(status(app(&injector), "/x").await, 200);
        }
        assert_eq!(injector.injected.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_partial_rate_fires_some_of_the_time() {
        let injector = injector(vec![error_rule(500_000)], 100_000);
        let mut failed = 0;
        for _ in 0..200 {
            if status(app(&injector), "/x").await == 503 {
                failed += 1;
            }
        }
        assert!((50..150).contains(&failed), "{failed} of 200 failed");
    }

    #[tokio::test]
    async fn the_stop_condition_counts_real_errors_too() {
        // A matched rule that never fires: only real errors count.
        let injector = injector(vec![error_rule(0)], 4);
        let failing = axum::Router::new()
            .route(
                "/x",
                get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .layer(FaultScopeLayer::new(Arc::clone(&injector)));
        for _ in 0..4 {
            let _ = status(failing.clone(), "/x").await;
        }
        assert!(!crate::fault_injection::FaultInjection::for_test(&injector).is_armed());
    }

    #[tokio::test]
    async fn a_dropped_request_with_a_fault_counts_as_an_error() {
        let mut rule = error_rule(1_000_000);
        rule.kind = FaultKind::Latency;
        rule.latency = Duration::from_secs(3_600);
        let injector = injector(vec![rule], 1);
        let request = status(app(&injector), "/x");
        let timed_out = tokio::time::timeout(Duration::from_millis(10), request).await;
        assert!(timed_out.is_err());
        assert!(!crate::fault_injection::FaultInjection::for_test(&injector).is_armed());
    }

    #[tokio::test]
    async fn the_dependency_seam_is_inert_outside_a_scope() {
        assert!(
            crate::fault_injection::inject(FaultTarget::Database)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_dependency_seam_fires_inside_a_scope() {
        let mut rule = error_rule(1_000_000);
        rule.target = FaultTarget::Database;
        let injector = injector(vec![rule], 100);
        let scope = injector.scope_for("/x").unwrap();
        let result = SCOPE
            .scope(scope, crate::fault_injection::inject(FaultTarget::Database))
            .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "fault injection: injected database error"
        );
    }
}
