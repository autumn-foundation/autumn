//! Isolated integration test: allocation gate for `AppState`'s config
//! accessors (issue #2198).
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

/// Trivial handler: the ceiling below is about the framework's per-request
/// work, so the handler itself must contribute as close to nothing as possible.
#[autumn_web::get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

/// Per-request allocation ceiling for a `TestClient` round trip.
///
/// The framework reads the whole config once per request and takes an owned
/// deep clone to do it. That clone is worth about a fifth of everything a
/// request allocates, which is what makes this ceiling meaningful rather than
/// decorative.
///
/// Numbers behind the constant, all from the debug profile with default
/// features, and identical across repeated runs — the whole path is
/// deterministic, so there is no noise budget to reserve:
///
/// | tree | blocks/request | bytes/request |
/// | --- | ---: | ---: |
/// | before #2198's `config_arc` work | 320 | — |
/// | after #2198 | 220 | — |
/// | after #2205 (`AppState` strings behind `Arc<str>`) | 172 | 37,819 |
/// | after #2214 (two `from_fn` layers onto `GateLayer`) | **160** | **33,667** |
///
/// #2205 moved `AppState::profile` and `AppState::auth_session_key` from owned
/// `String`/`Option<String>` to `Arc<str>`
/// (`appstate_clone_allocates_nothing_for_profile_and_auth_session_key` above
/// pins that clone at zero), a 48-block drop, since `AppState` is cloned on
/// every hop of the ingress tower stack and each of those two fields used to
/// be deep-copied on every one of those clones.
///
/// #2214 then moved two ingress layers — the method-override rejection filter
/// and the trusted-host check — off `axum::middleware::from_fn` and onto
/// `GateLayer`. Each removal is worth several blocks rather than the 1 you
/// would expect from deleting a single `Box::pin`, because a
/// `from_fn`-generated service *also* clones its inner service on every call,
/// and the ingress stack is a tower of `BoxCloneSyncService`s: one
/// clone-on-call site removed deletes a whole deep-clone traversal of
/// everything beneath it. The bytes column falls faster than the block column
/// (-11.0% vs -7.0%) because the boxed `from_fn` futures are individually
/// large — they hold the whole downstream continuation across their single
/// `.await`.
///
/// The two fixes are independent and compose: #2205 removed `String` deep
/// clones from `AppState::clone`, #2214 removed boxed futures and
/// clone-on-call sites from the layer stack, and #2214's margin measured the
/// same (-7% blocks / -11% bytes) against the pre-#2205 and post-#2205 trees.
///
/// # Why both ceilings sit BELOW the previous measurement
///
/// A ceiling only protects a win if reverting the change trips it. Both
/// constants therefore sit strictly between the new measurement and the old
/// one, not at "new plus comfortable headroom": `BLOCK_CEILING` is under the
/// pre-#2214 172, and `BYTE_CEILING` is under the pre-#2214 37,819, so
/// restoring either `from_fn` layer fails this test rather than sliding
/// underneath it.
///
/// The byte ceiling is not decorative duplication of the block ceiling. This
/// change's effect is mostly on *size* rather than *count* (-11.0% bytes vs
/// -7.0% blocks) because a boxed `from_fn` future is individually large, so a
/// block-only gate would let most of the regression back in. Both numbers are
/// deterministic, so both can be pinned this tightly.
///
/// Ceilings this close to the measured values are a deliberate trade: they can
/// only stay honest while the numbers stay deterministic. If this ever fails
/// just over a line rather than by a regression-sized jump, re-measure and
/// re-derive rather than nudging the constants upwards.
#[test]
fn per_request_allocations_stay_under_the_ceiling() {
    use autumn_web::routes;
    use autumn_web::test::TestApp;

    // Measured 160 blocks / 33,667 bytes; pre-#2214 was 172 / 37,819.
    const BLOCK_CEILING: u64 = 166;
    const BYTE_CEILING: u64 = 35_500;
    const WARMUP: usize = 3;
    const MEASURED: u64 = 10;

    // Counting is thread-local, so the runtime has to be current-thread:
    // anything a worker thread allocated would go uncounted and the ceiling
    // would flatter whatever moved off this thread.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime should build");
    let client = runtime.block_on(async { TestApp::new().routes(routes![ping]).build() });

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
    let per_request = info.count_total / MEASURED;
    let bytes_per_request = info.bytes_total / MEASURED;

    assert!(
        per_request <= BLOCK_CEILING,
        "a request allocated {per_request} blocks, over the {BLOCK_CEILING} \
         ceiling ({} blocks and {} bytes across {MEASURED} requests)",
        info.count_total,
        info.bytes_total
    );

    assert!(
        bytes_per_request <= BYTE_CEILING,
        "a request allocated {bytes_per_request} bytes, over the \
         {BYTE_CEILING} ceiling ({} blocks and {} bytes across {MEASURED} \
         requests). This gate is what keeps #2214's -11% byte win from being \
         quietly given back — a boxed `from_fn` future is large, so a \
         regression shows up here before it shows up in the block count.",
        info.count_total,
        info.bytes_total
    );
}
