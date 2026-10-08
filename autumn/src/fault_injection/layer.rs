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
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response};
use axum::response::IntoResponse;
use tower::{Layer, Service};

use super::{DeferredFault, FAULT_HEADER, FaultTarget, Injector, RequestScope, RouteFault, SCOPE};

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
        // A request in a scope for the same path already keeps it: the
        // SSG/ISG path has two scope layers. An MCP replay has another path,
        // so it gets a scope of its own.
        // An internal SSG/ISR render has no client: it gets no faults, as it
        // gets no request timeout.
        if req
            .extensions()
            .get::<crate::static_gen::RenderDeadlineExempt>()
            .is_some()
        {
            return Box::pin(self.inner.call(req));
        }
        let path = req.uri().path();
        let outer = SCOPE.try_with(|scope| &*scope.path == path);
        if matches!(outer, Ok(true)) {
            return Box::pin(self.inner.call(req));
        }
        let Some(scope) = self.injector.scope_for(path, req.method()) else {
            if outer.is_ok() {
                // Another path in an outer scope, and no rule matches this
                // one: run it with no faults, so the outer rules do not leak
                // in. It does not count.
                let empty = self.injector.empty_scope(path);
                let inner = SCOPE.sync_scope(Arc::clone(&empty), || self.inner.call(req));
                return Box::pin(SCOPE.scope(empty, inner));
            }
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
                if let Some(sequence) = scope.injector.record(scope.generation, error) {
                    scope.injector.audit_stop(sequence, STOP_REASON);
                }
            }
            result
        })
    }
}

/// Counts a request that is dropped before it completes (a timeout, or a
/// closed client connection). It counts as an error only when a fault fired,
/// so injected latency that causes a timeout counts against the error budget.
/// A dropped request with no fault counts as a good result.
struct Pending(Option<Arc<RequestScope>>);

impl Drop for Pending {
    fn drop(&mut self) {
        let Some(scope) = self.0.take() else {
            return;
        };
        // Each matched request counts. A dropped one is bad only when a
        // fault fired in it.
        let fired = scope.fired.load(Ordering::Relaxed);
        if let Some(sequence) = scope.injector.record(scope.generation, fired) {
            scope.injector.audit_stop(sequence, STOP_REASON);
        }
    }
}

/// Tower [`Layer`] that applies the route faults of `[fault_injection]`.
///
/// The router installs it when the section is enabled. Outside a fault
/// scope, it passes each request through. Route faults roll once per
/// request, also when two of these layers are in the path.
#[derive(Clone, Debug, Default)]
pub struct FaultInjectionLayer {
    placement: Placement,
}

/// Where a route layer sits.
#[derive(Clone, Copy, Debug, Default)]
enum Placement {
    /// Inside the request timeout layer: it applies route faults.
    #[default]
    Inner,
    /// Outside it, on the SSG/ISG path: it hands route faults to the inner
    /// layer. A cached page gets them here, with the wait capped at
    /// `deadline` (the global request timeout) when there is one.
    Outer { deadline: Option<Duration> },
}

impl FaultInjectionLayer {
    pub(super) const fn inner() -> Self {
        Self {
            placement: Placement::Inner,
        }
    }

    pub(super) const fn outer(deadline: Option<Duration>) -> Self {
        Self {
            placement: Placement::Outer { deadline },
        }
    }
}

impl<S> Layer<S> for FaultInjectionLayer {
    type Service = FaultInjectionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        FaultInjectionService {
            inner,
            placement: self.placement,
        }
    }
}

/// Tower [`Service`] produced by [`FaultInjectionLayer`].
#[derive(Clone, Debug)]
pub struct FaultInjectionService<S> {
    inner: S,
    placement: Placement,
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

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let mut fault = SCOPE
            .try_with(|scope| {
                if scope.route_rolled.swap(true, Ordering::AcqRel) {
                    RouteFault::default()
                } else {
                    scope.roll(FaultTarget::Route)
                }
            })
            .unwrap_or_default();
        if matches!(self.placement, Placement::Inner)
            && let Some(deferred) = req.extensions().get::<DeferredFault>()
            && deferred.take()
        {
            // The outer layer rolled this fault. Apply it here, inside the
            // request timeout of the route.
            fault.latency = deferred.fault.latency;
            fault.error = fault.error.or(deferred.fault.error);
        }
        if fault.latency.is_zero() {
            return match fault.error {
                Some(status) => Box::pin(async move { Ok(injected_error(status)) }),
                None => Box::pin(self.inner.call(req)),
            };
        }
        // Use the service that `poll_ready` made ready. Keep a clone in its
        // place.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let Placement::Outer { deadline } = self.placement else {
            return Box::pin(async move {
                tokio::time::sleep(fault.latency).await;
                match fault.error {
                    Some(status) => Ok(injected_error(status)),
                    None => inner.call(req).await,
                }
            });
        };
        // Outside the timeout layer: let the inner route layer apply the
        // fault. A response that did not pass it (a cached page) gets the
        // fault here: a wait of at most the request timeout, then a failure
        // as a timeout gives.
        let deferred = DeferredFault::new(fault);
        req.extensions_mut().insert(deferred.clone());
        Box::pin(async move {
            let response = inner.call(req).await?;
            if !deferred.take() {
                return Ok(response);
            }
            if let Some(deadline) = deadline
                && fault.latency >= deadline
            {
                tokio::time::sleep(deadline).await;
                return Ok(injected_error(axum::http::StatusCode::SERVICE_UNAVAILABLE));
            }
            tokio::time::sleep(fault.latency).await;
            Ok(fault.error.map_or(response, injected_error))
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
                FaultInjectionLayer::inner(),
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

    /// Two layer pairs (the SSG/ISG path) give one scope and one roll.
    #[tokio::test]
    async fn stacked_layers_fault_a_request_once() {
        let injector = injector(vec![error_rule(1_000_000)], 1_000);
        let app = axum::Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer((
                FaultScopeLayer::new(Arc::clone(&injector)),
                FaultInjectionLayer::inner(),
            ))
            .layer((
                FaultScopeLayer::new(Arc::clone(&injector)),
                FaultInjectionLayer::inner(),
            ));
        assert_eq!(status(app, "/x").await, 503);
        assert_eq!(injector.injected.load(Ordering::Relaxed), 1);
        let handle = crate::fault_injection::FaultInjection::for_test(&injector);
        assert_eq!(handle.snapshot().window_requests, 1);
    }

    /// An MCP replay crosses the inner scope layer with another path. It gets
    /// a scope of its own, so its rules apply.
    #[tokio::test]
    async fn a_nested_request_for_another_path_gets_its_own_scope() {
        let mut mcp = error_rule(0);
        mcp.routes = vec![super::super::RoutePattern::parse("/mcp")];
        let mut api = error_rule(1_000_000);
        api.routes = vec![super::super::RoutePattern::parse("/api/*")];
        let injector = injector(vec![mcp, api], 1_000);
        let inner = axum::Router::new()
            .route("/api/x", get(|| async { "ok" }))
            .layer((
                FaultScopeLayer::new(Arc::clone(&injector)),
                FaultInjectionLayer::inner(),
            ));
        let replay = inner.clone();
        let outer = axum::Router::new()
            .route(
                "/mcp",
                get(move || {
                    let replay = replay.clone();
                    async move { status(replay, "/api/x").await.to_string() }
                }),
            )
            .layer((
                FaultScopeLayer::new(Arc::clone(&injector)),
                FaultInjectionLayer::inner(),
            ));
        let response = outer
            .oneshot(Request::get("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"503", "the replayed /api/x request is faulted");
    }

    /// A nested request for a path with no rule does not inherit the outer
    /// scope's faults.
    #[tokio::test]
    async fn a_nested_request_with_no_rule_gets_no_faults() {
        let mut mcp = error_rule(1_000_000);
        mcp.target = FaultTarget::Database;
        mcp.routes = vec![super::super::RoutePattern::parse("/mcp")];
        let injector = injector(vec![mcp], 1_000);
        let outer_scope = injector
            .scope_for("/mcp", &axum::http::Method::GET)
            .unwrap();
        let inner = axum::Router::new()
            .route(
                "/api/x",
                get(|| async {
                    match crate::fault_injection::inject(FaultTarget::Database).await {
                        Ok(()) => "clean",
                        Err(_) => "faulted",
                    }
                }),
            )
            .layer(FaultScopeLayer::new(Arc::clone(&injector)));
        let response = SCOPE
            .scope(
                outer_scope,
                inner.oneshot(Request::get("/api/x").body(Body::empty()).unwrap()),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"clean");
    }

    /// Outside the timeout layer, a long latency fault ends at the deadline
    /// with a `503`, as the timeout layer would end it.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_caps_latency_and_fails() {
        let mut rule = error_rule(1_000_000);
        rule.kind = FaultKind::Latency;
        rule.latency = Duration::from_secs(300);
        let injector = injector(vec![rule], 1_000);
        let app = axum::Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer((
                FaultScopeLayer::new(Arc::clone(&injector)),
                FaultInjectionLayer::outer(Some(Duration::from_secs(2))),
            ));
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 503);
        assert_eq!(started.elapsed(), Duration::from_secs(2));
    }

    fn latency_injector(latency: Duration) -> Arc<Injector> {
        let mut rule = error_rule(1_000_000);
        rule.kind = FaultKind::Latency;
        rule.latency = latency;
        injector(vec![rule], 1_000)
    }

    /// The SSG/ISG shape: the outer pair, outside an inner router with its
    /// own route timeout and the inner route layer.
    fn static_path_app(injector: &Arc<Injector>, route_timeout: Option<Duration>) -> axum::Router {
        static_path_app_with(injector, route_timeout, Some(Duration::from_secs(1)))
    }

    /// [`static_path_app`] with the global request timeout `global`.
    fn static_path_app_with(
        injector: &Arc<Injector>,
        route_timeout: Option<Duration>,
        global: Option<Duration>,
    ) -> axum::Router {
        let inner = axum::Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer(FaultInjectionLayer::inner());
        let inner = match route_timeout {
            Some(limit) => inner.layer(axum::middleware::from_fn(
                move |req: Request<Body>, next: axum::middleware::Next| async move {
                    tokio::time::timeout(limit, next.run(req))
                        .await
                        .unwrap_or_else(|_| {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                        })
                },
            )),
            None => inner,
        };
        inner.layer((
            FaultScopeLayer::new(Arc::clone(injector)),
            FaultInjectionLayer::outer(global),
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_timeout_bounds_an_outer_latency() {
        let injector = latency_injector(Duration::from_millis(900));
        let app = static_path_app(&injector, Some(Duration::from_millis(500)));
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 503);
        assert_eq!(started.elapsed(), Duration::from_millis(500));
    }

    /// With no global timeout, the outer layer still hands its latency to
    /// the inner layer, so a route timeout bounds it.
    #[tokio::test(start_paused = true)]
    async fn a_route_timeout_bounds_an_outer_latency_with_no_global_timeout() {
        let injector = latency_injector(Duration::from_millis(900));
        let app = static_path_app_with(&injector, Some(Duration::from_millis(500)), None);
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 503);
        assert_eq!(started.elapsed(), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_with_no_timeout_keeps_a_long_outer_latency() {
        let injector = latency_injector(Duration::from_secs(3));
        let app = static_path_app(&injector, None);
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 200);
        assert_eq!(started.elapsed(), Duration::from_secs(3));
    }

    /// On the outer layer, an error with a long latency also stops at the
    /// deadline.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_caps_the_latency_of_an_error() {
        let mut latency = error_rule(1_000_000);
        latency.kind = FaultKind::Latency;
        latency.latency = Duration::from_secs(300);
        let injector = injector(vec![latency, error_rule(1_000_000)], 1_000);
        let app = axum::Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer((
                FaultScopeLayer::new(Arc::clone(&injector)),
                FaultInjectionLayer::outer(Some(Duration::from_secs(2))),
            ));
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 503);
        assert_eq!(started.elapsed(), Duration::from_secs(2));
    }

    fn latency_and_error_injector(latency: Duration) -> Arc<Injector> {
        let mut slow = error_rule(1_000_000);
        slow.kind = FaultKind::Latency;
        slow.latency = latency;
        injector(vec![slow, error_rule(1_000_000)], 1_000)
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_timeout_bounds_an_outer_latency_with_an_error() {
        let injector = latency_and_error_injector(Duration::from_millis(900));
        let app = static_path_app(&injector, Some(Duration::from_millis(500)));
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 503);
        assert_eq!(started.elapsed(), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_with_no_timeout_keeps_a_long_latency_with_an_error() {
        let injector = latency_and_error_injector(Duration::from_secs(3));
        let app = static_path_app(&injector, None);
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 503);
        assert_eq!(started.elapsed(), Duration::from_secs(3));
    }

    /// A cached page does not reach the inner route layer. It waits in the
    /// outer layer.
    #[tokio::test(start_paused = true)]
    async fn a_cached_page_waits_in_the_outer_layer() {
        let injector = latency_injector(Duration::from_millis(300));
        let app = axum::Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer((
                FaultScopeLayer::new(Arc::clone(&injector)),
                FaultInjectionLayer::outer(Some(Duration::from_secs(1))),
            ));
        let started = tokio::time::Instant::now();
        assert_eq!(status(app, "/x").await, 200);
        assert_eq!(started.elapsed(), Duration::from_millis(300));
        assert_eq!(injector.injected.load(Ordering::Relaxed), 1);
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
        let scope = injector.scope_for("/x", &axum::http::Method::GET).unwrap();
        let result = SCOPE
            .scope(scope, crate::fault_injection::inject(FaultTarget::Database))
            .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "fault injection: injected database error"
        );
    }
}
