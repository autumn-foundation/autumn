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
/// features, identical across three runs (the whole path is deterministic, so
/// there is no noise budget to reserve): 320 blocks on the tree before #2198's
/// `config_arc` work, 220 after it landed, and 172 after `AppState::profile`
/// and `AppState::auth_session_key` moved from owned `String`/`Option<String>`
/// to `Arc<str>` (`appstate_clone_allocates_nothing_for_profile_and_auth_session_key`
/// above pins that clone at zero) — a 48-block drop, since `AppState` is
/// cloned on every hop of the ingress tower stack and each of those two
/// fields used to be deep-copied on every one of those clones. The ceiling
/// sits above the current 172 with about a tenth of headroom to spare.
///
/// A ceiling this close to the measured value is a deliberate trade: it can
/// only stay honest while the number stays deterministic. If this ever fails
/// with a count just over the line rather than a regression-sized jump,
/// re-measure and re-derive it rather than nudging it upwards.
#[test]
fn per_request_allocations_stay_under_the_ceiling() {
    use autumn_web::routes;
    use autumn_web::test::TestApp;

    const CEILING: u64 = 190;
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

    assert!(
        per_request <= CEILING,
        "a request allocated {per_request} blocks, over the {CEILING} ceiling \
         ({} blocks and {} bytes across {MEASURED} requests)",
        info.count_total,
        info.bytes_total
    );
}
