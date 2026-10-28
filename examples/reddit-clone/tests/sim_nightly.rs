//! The reddit-clone DST nightly: a seed sweep over the whole app.
//!
//! Each seed mounts the app on the sim runtime and runs one full scenario:
//! votes that race, posts that age in virtual time, jobs, scheduled tasks,
//! replicas with skewed clocks, a crash, an outbound webhook over `SimNet`,
//! an LLM stub and SMTP faults. See `docs/guide/simulation-testing.md`,
//! "The reddit-clone nightly".
//!
//! # Run
//!
//! ```text
//! cargo test -p reddit-clone --release --test sim_nightly -- --nocapture sim_nightly_sweep
//! AUTUMN_SIM_SEED_START=40 AUTUMN_SIM_SEEDS=1 cargo test -p reddit-clone --release \
//!     --test sim_nightly -- --nocapture sim_nightly_sweep
//! ```
//!
//! - `AUTUMN_SIM_SEED_START`: the first seed (decimal, default 0).
//! - `AUTUMN_SIM_SEEDS`: how many seeds (decimal, default 8).
//! - `AUTUMN_SIM_SHARD`: `k/n` runs only part `k` of `n` of the window.
//! - `AUTUMN_SIM_TRACE_CHECKS`: how many of the first seeds run twice (default 2).
//! - `AUTUMN_SIM_REPORT`: a file path. The sweep writes a JSON report there.
//!
//! The `sim_` test names put these tests in the single-thread sim lane of CI.

mod sim_nightly {
    pub mod app;
    pub mod scenario;
    pub mod store;
    pub mod surface;
    pub mod sweep;
}

use autumn_web::sim::sweep::{SweepOutcome, sweep_proptest};
use autumn_web::sim::trace::capture;
use autumn_web::sim::{Sim, assert_all_sometimes_satisfied, reset_sometimes_registry};
use autumn_web::sim_test;

use sim_nightly::scenario::{self, Variant};
use sim_nightly::sweep::{self, Outcome, SweepConfig, one_sim_at_a_time};

/// The nightly entry. A small default window runs on every PR. The nightly
/// workflow sets the environment for a large window.
#[test]
fn sim_nightly_sweep() {
    let _serial = one_sim_at_a_time();
    let config = SweepConfig::from_env();
    let outcome = sweep::run(&config, Variant::Correct);
    sweep::write_report(&config, &outcome);
    assert!(
        matches!(outcome, Outcome::Passed { .. }),
        "{}",
        sweep::render(&config, &outcome)
    );
}

/// The sweep must find a real race. This variant gives `vote_core::cast` a new
/// `PublishOrder` for each request, so an older score can reach subscribers
/// last.
#[test]
fn sim_nightly_catches_unordered_publish() {
    let _serial = one_sim_at_a_time();
    let config = SweepConfig::window(0..64);
    let outcome = sweep::run(&config, Variant::UnorderedPublish);
    let Outcome::Failed { seed, reason, .. } = outcome else {
        panic!("the sweep must catch the unordered publish, got {outcome:?}");
    };
    assert!(reason.contains("older score"), "{reason}");

    // The failing seed fails again alone, with the same reason.
    let replay = scenario::run(seed, Variant::UnorderedPublish).expect_err("replay must fail");
    assert_eq!(first_line(&replay), first_line(&reason));
}

/// One seed gives one trace. Two seeds give two different traces.
#[test]
fn sim_nightly_same_seed_same_trace() {
    let _serial = one_sim_at_a_time();
    let first = scenario::run(7, Variant::Correct).expect("seed 7 passes");
    let second = scenario::run(7, Variant::Correct).expect("seed 7 passes");
    assert!(!first.is_empty(), "the scenario must log trace lines");
    assert_eq!(first.diff(&second), None);

    let other = scenario::run(8, Variant::Correct).expect("seed 8 passes");
    assert!(
        first.diff(&other).is_some(),
        "two seeds gave one trace: the seed does not reach the workload"
    );
}

/// Seeds 0..32 make each observed `sometimes!` label true, so a green sweep is
/// not vacuous.
#[test]
fn sim_nightly_window_is_not_vacuous() {
    let _serial = one_sim_at_a_time();
    let config = SweepConfig::window(0..32);
    let outcome = sweep::run(&config, Variant::Correct);
    assert!(
        matches!(outcome, Outcome::Passed { .. }),
        "{}",
        sweep::render(&config, &outcome)
    );
}

/// `#[sim_test]` runs one seed of the same scenario. `AUTUMN_SIM_SEED`
/// selects the seed.
#[sim_test]
#[allow(
    clippy::await_holding_lock,
    reason = "the lock keeps other sims out for the whole test; the sim runtime has one thread"
)]
async fn sim_nightly_one_seed_under_sim_test(mut sim: Sim) {
    let _serial = one_sim_at_a_time();
    let ((), trace) = capture(scenario::body(&mut sim, Variant::Correct)).await;
    assert!(!trace.is_empty(), "the framework logs during the scenario");

    // These labels hold at every seed, so one run can require all of them.
    reset_sometimes_registry();
    scenario::rank_bands(&mut sim).await;
    assert_all_sometimes_satisfied();
}

/// The proptest lane: `sweep_proptest` shrinks a failing case to its minimum.
/// It checks the real `calculate_hot_rank` on generated posts.
#[test]
fn sim_nightly_hot_rank_proptest_lane() {
    let _serial = one_sim_at_a_time();
    let strategy = scenario::rank_case_strategy();
    match sweep_proptest(0..4, &strategy, scenario::check_rank_cases) {
        SweepOutcome::Passed { seeds_run } => assert_eq!(seeds_run, 4),
        other => panic!("the hot-rank proptest lane failed: {other:?}"),
    }
    // One seed through `Sim::run_proptest` directly.
    if let Err(error) = Sim::run_proptest(9, &strategy, scenario::check_rank_cases) {
        panic!("seed 9 of the hot-rank lane failed: {error}");
    }
}

/// This nightly calls each public sim function, or `EXCLUDED` lists it with a
/// reason.
#[test]
fn dst_api_surface_is_fully_exercised() {
    let missing = sim_nightly::surface::missing_items();
    assert!(
        missing.is_empty(),
        "these public sim API items have no call in tests/sim_nightly/: {missing:#?}\n\
         Call each one from the scenario, or add it to `EXCLUDED` in surface.rs with a reason."
    );
}

/// An exclusion must name an item that still exists.
#[test]
fn dst_api_surface_exclusions_are_current() {
    let stale = sim_nightly::surface::stale_exclusions();
    assert!(stale.is_empty(), "remove these stale exclusions: {stale:?}");
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}
