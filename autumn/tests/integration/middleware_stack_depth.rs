//! Regression gate on the depth of Autumn's ingress middleware stack (#2193).
//!
//! # What is being gated, and why depth is the right quantity
//!
//! `axum::routing::Route` is a newtype over `tower::util::BoxCloneSyncService`,
//! and `Router::layer` re-boxes: `Route::layer` ends in `Route::new(..)`, which
//! calls `BoxCloneSyncService::new(..)` again. So *N* sequential `.layer()`
//! calls produce *N* **nested** boxed services, not one flat stack.
//!
//! That nesting is not merely N boxes at build time — it costs on every
//! request, quadratically. `Route::call` runs `self.0.clone().oneshot(req)`,
//! and cloning a `BoxCloneSyncService` is `Box::new(self.clone())`, which
//! deep-clones the service it wraps — including the next `Route` down, which
//! boxes again, all the way to the leaf. A request descending *N* levels
//! therefore triggers a full deep clone at each level: `N + (N-1) + … + 1`
//! heap allocations. Measured against axum 0.8.9, per-request allocations for
//! *N* stacked no-op layers come to `N(N+1)/2 + 2N` — 263 allocations at
//! N = 20, 1388 at N = 50 — while the *same* layers composed into a single
//! `Router::layer(ServiceBuilder…)` call cost a constant 16 regardless of N.
//!
//! # How the depth is observed
//!
//! Every one of those deep clones passes through *every* service below the
//! level that initiated it. So a probe service installed at the innermost
//! position — directly on the handler's `MethodRouter`, inside all global
//! middleware — is cloned exactly once per traversal of the stack above it.
//! Counting its clones over a single request yields the ingress depth as an
//! exact integer: no timing, no allocator hooks, no sampling, and identical on
//! every platform and in debug or release.
//!
//! # Why the budget is a ceiling, not an equality
//!
//! The absolute figure moves with the enabled Cargo features: `cargo test
//! -p autumn-web` builds the 8 default features while CI's `cargo test
//! --workspace` unifies ~29 across the workspace, and some ingress layers are
//! feature-gated (`oauth2`'s HTTP interceptor, `telemetry-otlp`'s trace
//! context). The budget is therefore set with headroom above the widest
//! configuration, and the test prints the observed number so a failure says
//! what the depth actually is.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use autumn_web::test::TestApp;
use autumn_web::{AppState, get, routes};
use axum::extract::Request;

/// Counts how many times the service it wraps is cloned.
#[derive(Clone)]
struct CloneCountLayer {
    clones: Arc<AtomicUsize>,
}

impl<S> tower::Layer<S> for CloneCountLayer {
    type Service = CloneCountService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CloneCountService {
            inner,
            clones: Arc::clone(&self.clones),
        }
    }
}

struct CloneCountService<S> {
    inner: S,
    clones: Arc<AtomicUsize>,
}

// Hand-written (not derived) so the clone can be counted. Deriving would also
// wrongly require `S: Clone` on the struct itself rather than on the impl.
impl<S: Clone> Clone for CloneCountService<S> {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
            clones: Arc::clone(&self.clones),
        }
    }
}

impl<S> tower::Service<Request> for CloneCountService<S>
where
    S: tower::Service<Request>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        self.inner.call(req)
    }
}

#[get("/unused")]
async fn unused() -> &'static str {
    "unused"
}

/// Ceiling on how many times a single request may traverse (deep-clone) the
/// framework's ingress stack.
///
/// Measured on the default feature set: **29 before #2193, 16 after**.
/// `apply_middleware` alone made 16 separate `Router::layer` calls, each adding
/// one `BoxCloneSyncService` nesting level; composing each contiguous run into a
/// single `Router::layer` call collapses those levels without changing the order
/// any of them run in.
///
/// The ceiling sits above the measured 16 rather than at it, because CI's
/// `cargo test --workspace` unifies ~29 Cargo features where a plain
/// `cargo test -p autumn-web` builds 8, and a couple of ingress layers are
/// feature-gated. It is low enough that reverting the collapse fails it by a
/// wide margin.
const MAX_INGRESS_TRAVERSALS: usize = 20;

/// Build the production router with a clone-counting probe at the innermost
/// position, drive one request through it, and return the traversal count.
async fn ingress_traversals_per_request() -> usize {
    let clones = Arc::new(AtomicUsize::new(0));

    // `merge` mounts a raw `Router<AppState>`, and the probe is attached to the
    // `MethodRouter` *before* the route is mounted — so it ends up inside every
    // global layer `apply_middleware` and `build_router_pre_state` apply.
    let probed = axum::Router::<AppState>::new().route(
        "/probe",
        axum::routing::get(async || "probe").layer(CloneCountLayer {
            clones: Arc::clone(&clones),
        }),
    );

    let client = TestApp::new().routes(routes![unused]).merge(probed).build();

    // Router assembly itself clones services (e.g. the MCP dispatch snapshot);
    // only per-request traversals are being measured.
    clones.store(0, Ordering::Relaxed);
    client.get("/probe").send().await.assert_status(200);

    clones.load(Ordering::Relaxed)
}

#[tokio::test]
async fn ingress_stack_depth_stays_within_budget() {
    let traversals = ingress_traversals_per_request().await;
    println!("ingress traversals/request: {traversals} (budget {MAX_INGRESS_TRAVERSALS})");

    assert!(
        traversals <= MAX_INGRESS_TRAVERSALS,
        "a single request deep-cloned the ingress stack {traversals} times \
         (budget {MAX_INGRESS_TRAVERSALS}). Every `Router::layer` call boxes the \
         whole downstream stack in a `BoxCloneSyncService`, and axum clones that \
         box on each call — so the per-request cost is quadratic in the number of \
         `.layer()` calls (issue #2193). Compose consecutive layers into one \
         `Router::layer((outermost, .., innermost))` call instead. NOTE: a \
         `tower-layer` tuple (like a `tower::ServiceBuilder` chain) puts its \
         FIRST element outermost, whereas repeated `Router::layer` calls put the \
         LAST call outermost — so a run being collapsed must be reversed."
    );
}

#[tokio::test]
async fn ingress_stack_depth_is_deterministic() {
    let first = ingress_traversals_per_request().await;
    let second = ingress_traversals_per_request().await;
    assert_eq!(
        first, second,
        "ingress depth must be a fixed property of the assembled stack, but two \
         identically-configured apps measured {first} and {second}"
    );
}
