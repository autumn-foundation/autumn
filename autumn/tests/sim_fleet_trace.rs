//! Same-seed trace check of the framework scenarios (issue #3067).
//!
//! Its own binary: `tracing` caches callsite interest for the whole process,
//! so a test on another thread can hide events from a capture. Run it with
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_fleet_trace`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use autumn_web::sim::fleet::{self, FleetOutcome};

/// Each scenario replays the same trace for the same seed.
#[test]
fn sim_fleet_scenarios_replay_the_same_trace() {
    for scenario in fleet::SCENARIOS {
        for seed in [0_u64, 1, 0x3067] {
            let first = fleet::run(scenario, seed)
                .unwrap_or_else(|error| panic!("{scenario} seed {seed}: {error}"));
            let second = fleet::run(scenario, seed)
                .unwrap_or_else(|error| panic!("{scenario} seed {seed}: {error}"));
            assert!(!first.is_empty(), "{scenario} seed {seed}: empty trace");
            if let Some(diff) = first.diff(&second) {
                panic!("{scenario} seed {seed}: nondeterministic trace\n{diff}");
            }
        }
    }
}

/// Another seed gives another run.
#[test]
fn sim_fleet_seeds_change_the_run() {
    let traces: Vec<_> = (0..4)
        .map(|seed| fleet::run("jobs", seed).expect("jobs"))
        .collect();
    assert!(
        traces.windows(2).any(|pair| pair[0] != pair[1]),
        "four seeds gave one trace"
    );
}

/// A short sweep passes, with every `sometimes!` label reached across its
/// seeds.
#[test]
fn sim_fleet_sweep_passes_and_is_not_vacuous() {
    match fleet::sweep(0..24, fleet::SCENARIOS, 2) {
        FleetOutcome::Passed { runs } => assert_eq!(runs, 24 * 3),
        other => panic!("{other:?}"),
    }
}

#[test]
fn sim_fleet_scenarios_fail_loudly_on_a_bad_name() {
    let error = fleet::run("no-such-scenario", 0).expect_err("unknown");
    assert!(error.contains("no-such-scenario"), "{error}");
}
