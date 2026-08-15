//! Regression gate on the depth of Autumn's ingress middleware stack (#2193,
//! #2198), plus the order characterization that gate must not be bought with.
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
//! The measured integer decomposes as **D + C**. *D* counts `Route` box levels
//! above the probe: one per `Router::layer` call applied above it, plus one for
//! the base `Route` box the leaf `MethodRouter` is stored in. *C* counts the
//! clone-on-call services above the probe (each `from_fn`). Only *D* moves when
//! `Router::layer` calls are collapsed; only *C* moves with the enabled Cargo
//! features. `cargo test -p autumn-web` builds the 8 default features and
//! measures **16** (D = 8, C = 8), while CI's `cargo test --workspace` unifies
//! ~29 features across the workspace — `oauth2` (enabled by `examples/blog`)
//! adds its HTTP-interceptor `from_fn` for **17** (D = 8, C = 9).
//!
//! Because *C* is feature-dependent, the assertion has to be a window rather
//! than an equality; its ceiling is derived from the widest *predicted*
//! configuration, not the widest current one, so the gate fails on the way back
//! up. See [`INGRESS_TRAVERSAL_WINDOW`] for the arithmetic.
//!
//! The lower bound matters too. Without it, a refactor that mounted merged raw
//! routers *after* the middleware — which is exactly how `/mcp` is treated —
//! would drop the probe out of the framework stack entirely (1-3 traversals),
//! and a ceiling-only assertion would stay green while measuring nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{AppState, get, post, routes};
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
/// # The arithmetic behind the bounds
///
/// The measurement is `D + C` (see the module header). *D* is the box-level
/// count — one per `Router::layer` call above the probe, plus the base `Route`
/// box — and *C* is the number of clone-on-call services above the probe, which
/// is fixed by the enabled features, not by how the layers are composed.
///
/// | configuration | D | C | measured |
/// | --- | --- | --- | --- |
/// | default features, before #2198 | 8 | 8 | **16** |
/// | CI workspace-unified features, before #2198 | 8 | 9 | **17** |
/// | default features, after #2198 | 5 | 8 | **13** |
/// | CI workspace-unified features, after #2198 | 5 | 9 | **14** |
///
/// #2198 removes three box levels by folding `apply_middleware`'s four
/// remaining `Router::layer` calls — the inner group, the middle group, the
/// session layer, and the outer group — into a single nested-tuple call.
///
/// **Upper bound 15** = the widest predicted configuration (14) plus one, so
/// restoring even ONE of the collapsed `.layer()` calls — which puts the
/// measurement straight back to 16-17 — fails this gate.
///
/// **Lower bound 6** is a sentinel, not a budget. It sits well under the
/// `from_fn` floor (C alone is 8-9, so no legitimate composition of the current
/// stack can reach it) and well above the 1-3 a probe measures once it has
/// fallen out of the framework stack entirely — the failure mode described in
/// the module header. It is deliberately slack: tightening it toward the real
/// measurement would make every feature-set difference a failure without
/// catching anything the ceiling does not already catch.
///
/// # What this gate is blind to
///
/// It counts clone **events**, not allocations — and those are not the same
/// quantity. Erasing an individual layer through
/// `tower::util::BoxCloneSyncServiceLayer` *removes* a clone event from this
/// count while KEEPING a box and ADDING a boxed future to every request:
/// `BoxCloneSyncService::new` wraps the service in `map_future(Box::pin)`, its
/// `call` forwards to the inner service without cloning it, and its `clone` is
/// a recursive `clone_box`. Per-layer type erasure would therefore drive this
/// number DOWN while driving real per-request allocations UP, and nothing here
/// would notice.
///
/// That is why the fix for #2198 collapses `Router::layer` calls — which
/// deletes whole box levels — rather than erasing layer types, and why a future
/// change that lowers this number by erasure must be judged on allocation
/// counts instead of on this gate.
const INGRESS_TRAVERSAL_WINDOW: std::ops::RangeInclusive<usize> = 6..=15;

/// Build the production router with a clone-counting probe at the innermost
/// position, drive one request through it, and return the traversal count.
async fn ingress_traversals_per_request() -> usize {
    ingress_traversals_with(|app| app).await
}

/// [`ingress_traversals_per_request`], with a hook to customize the `TestApp`
/// (register operator layers / static gates) before it is built.
async fn ingress_traversals_with(customize: impl FnOnce(TestApp) -> TestApp) -> usize {
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

    let client = customize(TestApp::new().routes(routes![unused]).merge(probed)).build();

    // Router assembly itself clones services (e.g. the MCP dispatch snapshot);
    // only per-request traversals are being measured.
    clones.store(0, Ordering::Relaxed);
    client.get("/probe").send().await.assert_status(200);

    clones.load(Ordering::Relaxed)
}

/// App-wide operator layer whose service forwards without cloning its inner —
/// so the ONLY thing registering it can add to the traversal count is the box
/// level its application costs. Generic over the inner service, like every
/// real-world tower layer, so it satisfies `IntoAppLayer` both before and
/// after #2198's registration-time erasure.
#[derive(Clone)]
struct PassthroughLayer;

impl<S> tower::Layer<S> for PassthroughLayer {
    type Service = PassthroughService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PassthroughService { inner }
    }
}

#[derive(Clone)]
struct PassthroughService<S> {
    inner: S,
}

impl<S> tower::Service<Request> for PassthroughService<S>
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

#[tokio::test]
async fn ingress_stack_depth_stays_within_budget() {
    let traversals = ingress_traversals_per_request().await;
    println!("ingress traversals/request: {traversals} (window {INGRESS_TRAVERSAL_WINDOW:?})");

    assert!(
        traversals <= *INGRESS_TRAVERSAL_WINDOW.end(),
        "a single request deep-cloned the ingress stack {traversals} times \
         (max {}). Expected 13 on the default feature set and 14 under CI's \
         workspace-unified features; 16-17 is the pre-#2198 measurement, i.e. \
         one of the four collapsed `Router::layer` calls in `apply_middleware` \
         is back. Every `Router::layer` call boxes the whole downstream stack in \
         a `BoxCloneSyncService`, and axum clones that box on each call — so the \
         per-request cost is quadratic in the number of `.layer()` calls (issues \
         #2193, #2198). Compose the layers into one \
         `Router::layer((outermost, .., innermost))` call instead, nesting a \
         sub-tuple where a group needs to stay a unit. NOTE: a `tower-layer` \
         tuple (like a `tower::ServiceBuilder` chain) puts its FIRST element \
         outermost, whereas repeated `Router::layer` calls put the LAST call \
         outermost — so a run being collapsed must be reversed. And do NOT chase \
         this number with `BoxCloneSyncServiceLayer`: erasing a layer's type \
         lowers this count while adding a boxed future per request — see \
         `INGRESS_TRAVERSAL_WINDOW`.",
        INGRESS_TRAVERSAL_WINDOW.end(),
    );

    assert!(
        traversals >= *INGRESS_TRAVERSAL_WINDOW.start(),
        "the probe saw only {traversals} traversals (min {}), which is below the \
         `from_fn` floor of the current stack (the clone-on-call services alone \
         account for 8-9) and means the probe is no longer inside the \
         framework's ingress stack — so this gate is measuring nothing. Check \
         that `mount_raw_routers` still runs BEFORE `apply_middleware`; if \
         merged routers moved after it (the way `/mcp` is mounted), this test \
         needs a different probe, not a lower bound.",
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

#[get("/ordered")]
async fn ordered() -> &'static str {
    "ordered"
}

#[post("/ordered")]
async fn ordered_post() -> &'static str {
    "posted"
}

/// CHARACTERIZATION (not a regression gate): pins the ingress *order* that the
/// depth collapse in #2198 must not disturb. It is green BEFORE the collapse —
/// against the four separate `Router::layer` calls — and must stay green AFTER
/// it, when those four become one nested tuple. A failure here means the
/// collapse reordered the stack, not that it failed to shrink it (that is
/// [`ingress_stack_depth_stays_within_budget`]).
///
/// The two composition forms run in OPPOSITE directions — consecutive
/// `Router::layer` calls put the LAST call outermost, a `tower-layer` tuple puts
/// its FIRST element outermost — and every framework layer is `Route -> Route`
/// with `Error = Infallible`, so a group written in the wrong order still
/// compiles and still type-checks. Both assertions below are therefore chosen to
/// FAIL under a reversal of the merged tuple's group order:
///
/// 1. A `400` produced by `method_override_rejection_filter` (innermost group)
///    still carries `x-request-id`, which only `RequestIdLayer` (middle group)
///    stamps. Reversed, the inner group would sit OUTSIDE `RequestIdLayer` and
///    the rejection would never reach it.
/// 2. That same `400` outranks CSRF's `403` on a request that carries neither a
///    valid `_method` nor a CSRF token — the ordering `router.rs` states as
///    "[method-override rejection] placed outside CSRF so ... a clear `400`
///    invalid `_method` outranks 'missing CSRF'". The paired control proves CSRF
///    is live in the very same app, so the `400` is an ordering result rather
///    than a disabled layer.
///
/// Deliberately NOT re-asserted here, to avoid duplicating an existing pin:
/// `ClientAddr` being populated before user/handler code (owned by
/// `trusted_proxy_resolution_runs_before_client_addr_is_read` in
/// `middleware_stack_order.rs`), and the bare `400`-with-framework-headers case
/// under DEFAULT config (owned by
/// `invalid_method_override_response_carries_framework_middleware` in
/// `src/test.rs`) — this test only adds the CSRF-enabled discriminator that
/// neither covers.
#[tokio::test]
async fn merged_stack_preserves_ingress_order() {
    // Positive control for the header assertions further down: on an ordinary
    // 200 both response-header layers are present, so a header missing from the
    // 400 below is evidence about ordering rather than about a disabled layer.
    let plain = TestApp::new().routes(routes![ordered]).build();
    let resp = plain.get("/ordered").send().await;
    resp.assert_status(200);
    assert!(
        resp.header("x-request-id").is_some() && resp.header("x-content-type-options").is_some(),
        "an ordinary 200 must carry both the request-id (middle group) and the \
         security headers (applied outside `apply_middleware`); headers = {:?}",
        resp.headers
    );

    let mut config = AutumnConfig {
        profile: Some("test".to_owned()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = true;
    let client = TestApp::new()
        .config(config)
        .routes(routes![ordered, ordered_post])
        .build();

    // Control: CSRF really is live in this app — a token-less POST is refused.
    client
        .post("/ordered")
        .form("title=x")
        .send()
        .await
        .assert_status(403);

    // The same token-less POST, now with an invalid `_method`, is refused by the
    // method-override filter FIRST: 400, not CSRF's 403.
    let resp = client.post("/ordered").form("_method=BREW").send().await;
    assert_eq!(
        resp.status.as_u16(),
        400,
        "an invalid `_method` must be rejected by \
         `method_override_rejection_filter`, which sits OUTSIDE the CSRF layer \
         in the innermost group; a 403 here means CSRF now runs first and the \
         inner group's order was inverted. body = {}",
        resp.text()
    );
    assert!(
        resp.header("x-request-id").is_some(),
        "the method-override 400 is produced by the INNERMOST group, so it can \
         only carry x-request-id while `RequestIdLayer` (middle group) is still \
         outside it; headers = {:?}",
        resp.headers
    );
    assert!(
        resp.header("x-content-type-options").is_some(),
        "the method-override 400 must still travel out through the security \
         headers; headers = {:?}",
        resp.headers
    );
}

/// The framework's per-request overhead must be a CONSTANT, not a function of
/// how many app-wide layers an operator (or a plugin — `Plugin::build` receives
/// the same `AppBuilder`, so plugin layers register through the identical path)
/// has attached. Before #2198 every `AppBuilder::layer`/`static_gate`
/// registration was applied as its own `Router::layer` call: one more
/// `BoxCloneSyncService` nesting level per registration, re-cloned by every
/// clone initiator above it on every request. After #2198 the registrations are
/// type-erased once at registration time and composed into the framework's
/// single merged application, so the box-level count `D` — and with it this
/// probe's traversal count — does not move at all.
///
/// The passthrough layers here neither box nor clone on call, so the ONLY
/// thing their registration can contribute is composition overhead — which is
/// exactly what this test pins to zero. A layer whose own `Service::call`
/// clones (a `from_fn`, say) still adds its clone-on-call traversal; that cost
/// belongs to the operator's code, not to the framework's composition, and is
/// deliberately out of scope here.
#[tokio::test]
async fn operator_layers_do_not_deepen_the_framework_cascade() {
    let bare = ingress_traversals_per_request().await;

    let with_layers = ingress_traversals_with(|app| {
        app.layer(PassthroughLayer)
            .layer(PassthroughLayer)
            .layer(PassthroughLayer)
    })
    .await;
    assert_eq!(
        with_layers, bare,
        "three registered app-wide operator layers moved the ingress traversal \
         count from {bare} to {with_layers}: each registration is being applied \
         as its own `Router::layer` call (one nesting level per layer) instead \
         of being erased into the framework's single merged application"
    );

    let with_gate = ingress_traversals_with(|app| app.static_gate(PassthroughLayer)).await;
    assert_eq!(
        with_gate, bare,
        "a registered static gate moved the ingress traversal count from \
         {bare} to {with_gate}: the gate group must compose into the same \
         `Router::layer` call as `SecurityHeadersLayer` at its existing \
         position (after the MCP dispatch clone), not add its own nesting level"
    );
}
