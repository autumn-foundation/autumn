//! Isolated integration test: the framework's per-request allocation gates —
//! `AppState`'s config accessors (issue #2198) and the ingress stack's
//! allocation blocks and bytes (issues #2198, #2205, #2214).
//!
//! Everything that has to be judged on *allocations* rather than on a
//! structural count lives here, in one binary, for the reason the next
//! paragraph gives. Its sibling gate
//! `tests/integration/middleware_stack_depth.rs` counts clone-on-call
//! traversals of the same stack; that count is a proxy, this file is the
//! measurement.
//!
//! `AppState::config` deep-clones every section of `AutumnConfig` on each call,
//! which is paid per request on the paths that read config. `config_arc` exists
//! to make that read free: it clones the `Arc` the extension map already holds,
//! so a steady-state call allocates nothing at all. "Nothing at all" is the
//! property worth pinning — a ceiling of "a few" allocations would silently
//! absorb a re-introduced clone.
//!
//! This lives in its own test binary (not the consolidated suite) because
//! `allocation-counter` installs a counting `#[global_allocator]`: a
//! process-wide side effect, per CLAUDE.md's isolated-test rules. Counting is
//! thread-local and only spans the measured closure, but the allocator itself
//! is global, and taxing every allocation in the consolidated binary to
//! measure a handful here is not a trade worth making.
//!
//! The tests are plain `#[test]` fns, not `#[tokio::test]`: an executor in the
//! measured window would contribute allocations of its own that have nothing to
//! do with the accessor under test.

use autumn_web::AppState;
use autumn_web::config::AutumnConfig;

/// Enough repetitions that a per-call allocation cannot hide inside noise, and
/// that the reported total reads as an obvious multiple of the per-call cost.
const CALLS: usize = 100;

/// A state with a config installed the way `app::build` installs one.
fn state_with_config() -> AppState {
    let state = AppState::for_test();
    state.insert_extension(AutumnConfig {
        profile: Some("prod".to_owned()),
        ..Default::default()
    });
    state
}

#[test]
fn config_arc_allocates_nothing_with_an_installed_config() {
    let state = state_with_config();
    // Warm-up outside the measured window: whatever the first call has to set
    // up, steady-state calls must not pay for.
    drop(state.config_arc());

    let info = allocation_counter::measure(|| {
        for _ in 0..CALLS {
            let config = state.config_arc();
            std::hint::black_box(&config);
        }
    });

    assert_eq!(
        info.count_total, 0,
        "config_arc must clone only the Arc; {CALLS} calls allocated \
         {} blocks ({} bytes)",
        info.count_total, info.bytes_total
    );
}

#[test]
fn config_arc_allocates_nothing_in_the_no_config_fallback() {
    // No config extension: the accessor falls back to a default config. The
    // fallback has to be a shared value too, otherwise every config read in a
    // test-built app deep-clones a default instead of an installed one.
    let state = AppState::for_test();
    drop(state.config_arc());

    let info = allocation_counter::measure(|| {
        for _ in 0..CALLS {
            let config = state.config_arc();
            std::hint::black_box(&config);
        }
    });

    assert_eq!(
        info.count_total, 0,
        "the fallback config must be shared, not rebuilt; {CALLS} calls \
         allocated {} blocks ({} bytes)",
        info.count_total, info.bytes_total
    );
}

/// `AppState::clone()` allocation gate.
///
/// `AppState` is `Clone` and gets cloned on every hop of the ingress tower
/// stack (`Route::call` deep-clones the boxed service beneath it, per
/// #2193/#2198), so anything owned directly on the struct — as opposed to
/// shared behind an `Arc` or living in the `extensions` map — is paid once
/// per traversal, not once per request. `profile: Option<String>` and
/// `auth_session_key: String` were the two fields still doing that: measured
/// with a `TestApp`-built state (`profile = "test"`, `auth_session_key =
/// "user_id"`, the same shape `per_request_allocations_stay_under_the_ceiling`
/// below exercises), 100 clones allocate exactly 200 blocks / 1100 bytes — 2
/// blocks per clone, one per field, deterministic across runs. Neither field
/// is ever mutated on a live `AppState` outside the builder methods that
/// construct one, so sharing them behind an `Arc<str>` costs nothing a
/// request-scoped clone needs back.
#[test]
fn appstate_clone_allocates_nothing_for_profile_and_auth_session_key() {
    use autumn_web::routes;
    use autumn_web::test::TestApp;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime should build");
    let client = runtime.block_on(async { TestApp::new().routes(routes![ping]).build() });
    let state = client.state();
    // Warm-up outside the measured window, matching the sibling tests.
    drop(state.clone());

    let info = allocation_counter::measure(|| {
        for _ in 0..CALLS {
            let cloned = state.clone();
            std::hint::black_box(&cloned);
        }
    });

    assert_eq!(
        info.count_total, 0,
        "AppState::clone() must not deep-clone its profile/auth_session_key \
         fields; {CALLS} clones allocated {} blocks ({} bytes)",
        info.count_total, info.bytes_total
    );
}

/// Executable documentation of what `config_arc` buys: `config` hands back an
/// owned snapshot, so it allocates by contract. This assertion is expected to
/// keep holding after `config_arc` is made allocation-free — `config` stays a
/// deep clone, and a future where it allocates nothing would mean its
/// signature no longer returns an owned `AutumnConfig`.
#[test]
fn config_allocates_because_it_deep_clones() {
    let state = state_with_config();
    drop(state.config());

    let info = allocation_counter::measure(|| {
        for _ in 0..CALLS {
            let config = state.config();
            std::hint::black_box(&config);
        }
    });

    assert!(
        info.count_total > 0,
        "config returns an owned deep clone, so it must allocate"
    );
}

/// Trivial handler: the ceilings below are about the framework's per-request
/// work, so the handler itself must contribute as close to nothing as possible.
#[autumn_web::get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

/// What one `TestClient` round trip through the production ingress costs.
#[derive(Clone, Copy, Debug)]
struct PerRequest {
    /// Allocation blocks (`malloc` calls), averaged over the measured run.
    blocks: u64,
    /// Allocated bytes, averaged over the measured run.
    bytes: u64,
}

/// Drive `MEASURED` requests through a `TestApp`-built production router and
/// return the per-request allocation cost.
///
/// `customize` runs on the `TestApp` before it is built, so a caller can
/// register extra layers and measure their marginal cost.
///
/// The runtime has to be current-thread: `allocation-counter` counts
/// thread-locally, so anything a worker thread allocated would go uncounted and
/// every ceiling here would flatter whatever moved off this thread.
fn measure_per_request(
    customize: impl FnOnce(autumn_web::test::TestApp) -> autumn_web::test::TestApp,
) -> PerRequest {
    use autumn_web::routes;
    use autumn_web::test::TestApp;

    const WARMUP: usize = 3;
    const MEASURED: u64 = 10;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime should build");
    let client =
        runtime.block_on(async { customize(TestApp::new().routes(routes![ping])).build() });

    // First requests pay one-time setup (lazily built middleware state, caches)
    // that steady-state requests do not.
    runtime.block_on(async {
        for _ in 0..WARMUP {
            client.get("/ping").send().await.assert_ok();
        }
    });

    let info = allocation_counter::measure(|| {
        runtime.block_on(async {
            for _ in 0..MEASURED {
                let response = client.get("/ping").send().await;
                std::hint::black_box(&response);
            }
        });
    });

    PerRequest {
        blocks: info.count_total / MEASURED,
        bytes: info.bytes_total / MEASURED,
    }
}

/// Per-request allocation-BLOCK ceiling for a `TestClient` round trip.
///
/// What makes it meaningful rather than decorative is that it has caught real
/// movement three times: it is the number #2198, #2205 and #2214 each drove
/// down, and each of those wins is one a purely structural gate could not see.
/// (An earlier version of this comment justified the ceiling by the whole-config
/// deep clone `AppState::config()` used to take per request — that clone left
/// the request path in #2198, when every framework read moved to `config_arc`.)
///
/// Numbers behind the constant, all from the debug profile with default
/// features, identical across three runs (the whole path is deterministic, so
/// there is no noise budget to reserve): 320 blocks on the tree before #2198's
/// `config_arc` work, 220 after it landed, 172 after `AppState::profile`
/// and `AppState::auth_session_key` moved from owned `String`/`Option<String>`
/// to `Arc<str>` (`appstate_clone_allocates_nothing_for_profile_and_auth_session_key`
/// above pins that clone at zero) — a 48-block drop, since `AppState` is
/// cloned on every hop of the ingress tower stack and each of those two
/// fields used to be deep-copied on every one of those clones — and **140**
/// after #2214 replaced the always-on ingress `axum::middleware::from_fn`
/// layers with hand-rolled services carrying named futures. That 32-block drop
/// is more than the one `Box::pin` per converted layer the issue title names:
/// `FromFn::call` also opens with `self.inner.clone()`, and cloning an erased
/// `BoxCloneSyncService` is a recursive `clone_box` down the rest of the stack,
/// so each conversion took a whole deep-clone cascade with it (plus the
/// `String` `asset_cache_control` used to own its path in). The ceiling sits
/// above the current measurement with about a tenth of headroom to spare.
///
/// **Feature sensitivity.** CI gates with `cargo test --workspace`, which
/// unifies far more features than the 8 defaults a local `cargo test -p
/// autumn-web` builds — so a ceiling derived under defaults alone would be a
/// guess. This one is not: 140 was measured under the default set AND under a
/// 13-feature build adding `oauth2`, `mail`, `storage`, `ws` and `openapi`.
/// Blocks did not move at all between the two (bytes did — see the sibling
/// gate). The remaining 6 blocks of headroom are for a feature neither build
/// covers.
///
/// A ceiling this close to the measured value is a deliberate trade, and here
/// it buys something specific: at 146 a single restored `axum::middleware::from_fn`
/// on the ingress path (7 blocks, per the control test below) fails this gate.
/// A looser ceiling would only catch a wholesale revert. It can only stay
/// honest while the number stays deterministic — if this ever fails with a
/// count just over the line rather than a regression-sized jump, re-measure
/// under both feature sets and re-derive it rather than nudging it upwards.
#[test]
fn per_request_allocations_stay_under_the_ceiling() {
    const CEILING: u64 = 146;

    let measured = measure_per_request(|app| app);
    println!(
        "per-request ingress cost: {} blocks / {} bytes",
        measured.blocks, measured.bytes
    );

    assert!(
        measured.blocks <= CEILING,
        "a request allocated {} blocks, over the {CEILING} ceiling ({} bytes \
         per request). 172 was the pre-#2214 measurement: check whether an \
         `axum::middleware::from_fn` is back on the always-on ingress path — \
         each one costs a `Box::pin` per request.",
        measured.blocks,
        measured.bytes,
    );
}

/// Per-request allocated-BYTES ceiling for a `TestClient` round trip.
///
/// Bytes, not blocks, are the quantity issue #2214 is denominated in: the
/// `Box::pin` `axum::middleware::from_fn` wraps its future in is *one* block
/// but a large one — DHAT measured 1088-2224 bytes at each of the seven call
/// sites the `request_pipeline` bench traverses, because an outer layer's async
/// block captures the whole downstream continuation across its single `.await`.
/// Summed, that was 19.57% of every byte the benchmark allocated while being
/// only 2.14% of the blocks. A block-count ceiling alone would barely move for
/// the largest allocation cost in the profile, which is exactly why this
/// sibling gate exists.
///
/// Derivation, debug profile, stable across three runs: **37,819 bytes** per
/// request before #2214 and **26,030** after under the 8 default features — a
/// 31.2% reduction, against the 19.57% DHAT attributed to the `from_fn` boxes
/// alone, the balance being the deep clones those boxes' `self.inner.clone()`
/// used to drag along with them.
///
/// Unlike the block count, bytes ARE feature-sensitive: the same measurement
/// under a 13-feature build (defaults plus `oauth2`, `mail`, `storage`, `ws`,
/// `openapi`) is **27,622** — 6.1% higher, because a wider `AutumnConfig` and a
/// wider `UploadConfig` ride along in request extensions without adding a
/// single allocation *block*. The ceiling is therefore derived from the WIDER
/// measurement plus about a tenth, not from the default-feature one; a ceiling
/// derived from 26,030 would have left barely 3% of room under the feature set
/// CI actually gates with.
///
/// That makes this a *bulk* gate: it catches a wholesale reintroduction of the
/// `from_fn` layers (which would put the number back near 38k), not a single
/// one (worth ~1.9 KB). Single-layer regressions are the block ceiling's job
/// above, and the `type_name` assertions in `src/router.rs`'s test module.
///
/// The same honesty rule as the block ceiling applies: a failure a hair over
/// the line means re-measure under both feature sets and re-derive, not nudge.
#[test]
fn per_request_allocated_bytes_stay_under_the_ceiling() {
    const CEILING: u64 = 30_500;

    let measured = measure_per_request(|app| app);
    println!(
        "per-request ingress cost: {} blocks / {} bytes",
        measured.blocks, measured.bytes
    );

    assert!(
        measured.bytes <= CEILING,
        "a request allocated {} bytes, over the {CEILING} ceiling ({} blocks \
         per request). ~37.8k was the pre-#2214 measurement: check whether an \
         `axum::middleware::from_fn` is back on the always-on ingress path — \
         its `Box::pin` is sized by everything the wrapped async block captures \
         across `.await`, which for an outer layer is the whole downstream \
         continuation.",
        measured.bytes,
        measured.blocks,
    );
}

/// App-wide operator layer that neither clones its inner service on call nor
/// boxes its future, so registering it costs only the type erasure every
/// registration pays. The control below subtracts that erasure out.
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

impl<S> tower::Service<axum::extract::Request> for PassthroughService<S>
where
    S: tower::Service<axum::extract::Request>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        self.inner.call(req)
    }
}

/// The sensitivity control for the two ceilings above, and the executable
/// statement of what issue #2214 is about.
///
/// Both registrations below wrap the same no-op behaviour around the same app
/// and are erased identically (`AppBuilder::layer` boxes every registration
/// through `BoxCloneSyncServiceLayer` — one `Box::pin` per request, common to
/// both). The ONLY difference is that one is written as a `tower::Service`
/// whose future is its inner service's future, and the other is written as an
/// `axum::middleware::from_fn`, whose generated `FromFn::call` must
/// `Box::pin` the async block it wraps because that block's type cannot be
/// named. The difference between the two measurements is therefore exactly one
/// `from_fn` box.
///
/// The measured difference is bigger than the single box, and deliberately so:
/// `FromFn::call` opens with `self.inner.clone()` so it can move the inner
/// service into the async block, and cloning an erased `BoxCloneSyncService`
/// is a recursive `clone_box` down every remaining level of the stack — the
/// same cascade #2193/#2198 measured. So a `from_fn` costs its own box PLUS a
/// deep clone of everything beneath it, which is why converting one is worth
/// more than the one allocation the issue title names. At the operator-layer
/// position measured here (debug profile, default features) the difference is
/// **7 blocks / ~1.9 KB** per request; a `from_fn` further out costs more,
/// because more of the stack sits below it.
///
/// This test is green both before and after #2214 — that is the point. It
/// proves the ceilings above are measured on an instrument that can see the
/// thing they are gating, rather than being two numbers that happen to hold.
/// The no-op the control below wraps in an `axum::middleware::from_fn`.
async fn noop_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    next.run(req).await
}

/// A floor rather than `> 0`: the ceilings above were derived against a
/// `from_fn` that costs 7 blocks at the position measured here, so an
/// instrument that could only still see 1 of those 7 would leave them
/// unmeetable while this control stayed green.
const MIN_FROM_FN_BLOCKS: u64 = 5;

#[test]
fn a_from_fn_layer_costs_more_heap_blocks_per_request_than_a_named_future() {
    let named = measure_per_request(|app| app.layer(PassthroughLayer));
    let boxed = measure_per_request(|app| app.layer(axum::middleware::from_fn(noop_middleware)));
    println!(
        "named-future layer: {} blocks / {} bytes; from_fn layer: {} blocks / {} bytes \
         (delta {} blocks / {} bytes)",
        named.blocks,
        named.bytes,
        boxed.blocks,
        boxed.bytes,
        boxed.blocks.saturating_sub(named.blocks),
        boxed.bytes.saturating_sub(named.bytes),
    );

    assert!(
        boxed.blocks >= named.blocks + MIN_FROM_FN_BLOCKS,
        "an `axum::middleware::from_fn` must cost at least {MIN_FROM_FN_BLOCKS} \
         heap blocks per request more than the same no-op written as a \
         `tower::Service` with a named future (named = {} blocks, from_fn = {} \
         blocks; 7 was the measurement the ceilings above were derived against). \
         If the gap has shrunk, `from_fn` changed and those ceilings need \
         re-deriving, not this assertion loosening.",
        named.blocks,
        boxed.blocks,
    );
    assert!(
        boxed.bytes > named.bytes,
        "the `from_fn` box has to be visible in bytes too, but the named-future \
         layer measured {} bytes and the `from_fn` one {} bytes",
        named.bytes,
        boxed.bytes,
    );
}
