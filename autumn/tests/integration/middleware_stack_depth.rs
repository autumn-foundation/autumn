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
//! *N* stacked no-op layers fit `13 + N(N+1)/2 + 2N` (13 being the fixed
//! per-request baseline at N = 0): 263 allocations at N = 20, 1388 at N = 50 —
//! while the *same* layers composed into a single `Router::layer` call cost a
//! constant 16 regardless of N.
//!
//! # How the depth is observed
//!
//! Every one of those deep clones passes through *every* service below the
//! level that initiated it. So a probe service installed at the innermost
//! position is cloned exactly once per traversal of the stack above it.
//! Counting its clones over a single request yields an exact integer: no
//! timing, no allocator hooks, no sampling, identical on every platform and in
//! debug or release.
//!
//! What that integer counts is **every service above the probe that clones on
//! call**, which is the `Route` box levels *plus* each
//! `axum::middleware::from_fn` (its generated `Service::call` starts with
//! `self.inner.clone()`). Collapsing `Router::layer` calls removes box levels;
//! it does not remove `from_fn` traversals. So the number moves with both, and
//! a new `from_fn` inside an existing tuple raises it without adding a box.
//!
//! The probe is attached to a `MethodRouter` in a merged raw router, which
//! `mount_raw_routers` mounts *after* `build_router_pre_state` has already
//! applied the asset cache-control layer (and, under `i18n` locale prefixes,
//! the locale-routing extension). A route declared with `#[get]` therefore sits
//! one or two traversals deeper than the number measured here; the gate tracks
//! relative change, not a route's absolute cost.
//!
//! # Why the assertion is a window
//!
//! The absolute figure moves with the enabled Cargo features: `cargo test
//! -p autumn-web` builds the 8 default features and measures **16**, while
//! CI's `cargo test --workspace` unifies ~29 across the workspace — `oauth2`
//! (enabled by `examples/blog`) adds its HTTP-interceptor `from_fn` for **17**.
//! The upper bound is set just above that: reverting even one collapsed run
//! (e.g. the four-element outer group back to four chained `.layer()` calls)
//! lands at 19-20 and fails.
//!
//! The lower bound matters too. Without it, a refactor that mounted merged raw
//! routers *after* the middleware — which is exactly how `/mcp` is treated —
//! would drop the probe out of the framework stack entirely, and a
//! ceiling-only assertion would stay green while measuring nothing.

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

/// Accepted window for how many times a single request may traverse
/// (deep-clone) the framework's ingress stack.
///
/// Measured: **29 before #2193, 16 after** on the default feature set, 17 under
/// CI's workspace-unified feature set (`oauth2` adds one `from_fn`).
/// `apply_middleware` alone made 16 separate `Router::layer` calls, each adding
/// one `BoxCloneSyncService` nesting level; composing each contiguous run into a
/// single `Router::layer` call collapses those levels without changing the order
/// any of them run in.
///
/// Upper bound 18: one unit above the widest measured configuration, so
/// reverting any collapsed run (which lands at 19+) fails. Lower bound 13:
/// below it the probe is no longer inside the framework stack at all — see the
/// module header.
const INGRESS_TRAVERSAL_WINDOW: std::ops::RangeInclusive<usize> = 13..=18;

/// Build the production router with a clone-counting probe at the innermost
/// position, drive one request through it, and return the traversal count.
async fn ingress_traversals_per_request() -> usize {
    let clones = Arc::new(AtomicUsize::new(0));

    // `merge` mounts a raw `Router<AppState>`, and the probe is attached to the
    // `MethodRouter` *before* the route is mounted — so it sits inside every
    // layer applied at or after `mount_raw_routers`, which is all of
    // `apply_middleware` and the tail of `build_router_pre_state`. Layers
    // applied EARLIER than that mount point (the asset cache-control `from_fn`,
    // and the i18n locale-routing extension) are not counted; see the module
    // header.
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
    println!("ingress traversals/request: {traversals} (window {INGRESS_TRAVERSAL_WINDOW:?})");

    assert!(
        traversals <= *INGRESS_TRAVERSAL_WINDOW.end(),
        "a single request deep-cloned the ingress stack {traversals} times \
         (max {}). Every `Router::layer` call boxes the whole downstream stack \
         in a `BoxCloneSyncService`, and axum clones that box on each call — so \
         the per-request cost is quadratic in the number of `.layer()` calls \
         (issue #2193). Compose consecutive layers into one \
         `Router::layer((outermost, .., innermost))` call instead. NOTE: a \
         `tower-layer` tuple (like a `tower::ServiceBuilder` chain) puts its \
         FIRST element outermost, whereas repeated `Router::layer` calls put the \
         LAST call outermost — so a run being collapsed must be reversed.",
        INGRESS_TRAVERSAL_WINDOW.end(),
    );

    assert!(
        traversals >= *INGRESS_TRAVERSAL_WINDOW.start(),
        "the probe saw only {traversals} traversals (min {}), which means it is \
         no longer inside the framework's ingress stack — so this gate is \
         measuring nothing. Check that `mount_raw_routers` still runs BEFORE \
         `apply_middleware`; if merged routers moved after it (the way `/mcp` \
         is mounted), this test needs a different probe, not a lower bound.",
        INGRESS_TRAVERSAL_WINDOW.start(),
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
