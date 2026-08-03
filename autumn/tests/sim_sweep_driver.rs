//! W6 PR3 Definition-of-Done proof (sim-testing, issue #1797): the seed-sweep
//! runner finds and shrinks an injected invariant break across a batch of
//! seeds, the failing seed it reports is deterministic across repeated
//! sweeps, and the cross-seed `sometimes!` non-vacuity aggregate fires when a
//! reachability label is observed but never satisfied anywhere in the swept
//! range.
//!
//! Reuses `tests/sim_op_driver.rs`'s worked example (an account balance with
//! an intentionally unfloored `Withdraw`) rather than inventing a new bug —
//! this file's only job is the *sweep* mechanism (`sweep_proptest` batching
//! `Sim::run_proptest` across seeds, fail-fast, plus cross-seed
//! `sometimes!` aggregation), not re-proving the op-driver shrink mechanism
//! itself (that's `sim_op_driver`'s job).
//!
//! Isolated `[[test]]` binary for the same reason as `sim_op_driver`: needs
//! the `sim-testing` feature (`proptest` as a library dependency), which the
//! default-feature consolidated `integration_tests` binary does not enable,
//! so the file is `#![cfg(feature = "sim-testing")]` and a default
//! `cargo test` compiles it to an empty (passing) binary. Runs without
//! Docker. Run:
//! `cargo test -p autumn-web --features sim-testing --test sim_sweep_driver`.

#![cfg(feature = "sim-testing")]

use autumn_web::sim::sweep::{SweepOutcome, sweep_proptest};
use autumn_web::{always, sometimes};
use proptest::prelude::*;

/// A toy account op, mirroring `sim_op_driver.rs`'s worked example.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Deposit(u32),
    Withdraw(u32),
}

impl Arbitrary for Op {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with((): ()) -> Self::Strategy {
        prop_oneof![
            (1u32..100).prop_map(Op::Deposit),
            (1u32..100).prop_map(Op::Withdraw),
        ]
        .boxed()
    }
}

/// **Deliberately buggy**, same as `sim_op_driver.rs`: `Withdraw` subtracts
/// unconditionally, with no floor check against the current balance.
fn buggy_apply_ops(ops: &[Op]) {
    let mut balance: i64 = 0;
    for op in ops {
        match *op {
            Op::Deposit(amount) => balance += i64::from(amount),
            Op::Withdraw(amount) => balance -= i64::from(amount),
        }
        always!(balance >= 0, "balance went negative: {balance}");
    }
}

fn op_strategy() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(any::<Op>(), 1..32)
}

#[test]
fn sweep_finds_and_shrinks_the_injected_invariant_break() {
    let strategy = op_strategy();
    let outcome = sweep_proptest(0..16, &strategy, |_sim, ops| buggy_apply_ops(ops));

    match outcome {
        SweepOutcome::Failed { seeds_run, failure } => {
            assert!(
                seeds_run >= 1,
                "the sweep must have attempted at least one seed"
            );
            assert_eq!(
                failure.shrunk_ops,
                vec![Op::Withdraw(1)],
                "should shrink to the minimal single-withdraw counterexample, got {:?}",
                failure.shrunk_ops
            );
        }
        other => panic!("a Withdraw from a zero balance must violate the invariant, got {other:?}"),
    }
}

#[test]
fn the_sweeps_failing_seed_replays_deterministically() {
    let strategy = op_strategy();
    let run = || sweep_proptest(0..16, &strategy, |_sim, ops| buggy_apply_ops(ops));

    let (first, second) = (run(), run());
    assert_eq!(
        first, second,
        "the same swept seed range must report the identical failing seed and shrunk sequence"
    );
}

#[test]
fn sweep_passes_and_is_non_vacuous_for_a_correct_model() {
    let strategy = op_strategy();
    let outcome = sweep_proptest(0..16, &strategy, |_sim, ops| {
        let mut balance: i64 = 0;
        for op in ops {
            match *op {
                Op::Deposit(amount) => balance += i64::from(amount),
                // Correct: floors at zero instead of going negative.
                Op::Withdraw(amount) => balance -= i64::from(amount).min(balance),
            }
            always!(balance >= 0, "balance went negative: {balance}");
            sometimes!(balance == 0, "balance-returned-to-zero");
        }
    });

    match outcome {
        SweepOutcome::Passed { seeds_run } => assert_eq!(seeds_run, 16),
        other => panic!("expected every seed to pass non-vacuously, got {other:?}"),
    }
}

#[test]
fn sweep_reports_vacuous_when_a_sometimes_label_is_never_satisfied_across_the_range() {
    let strategy = op_strategy();
    // `sometimes!` is observed on every case but its condition is always
    // false, so no seed in the range can ever satisfy it.
    let outcome = sweep_proptest(0..8, &strategy, |_sim, ops| {
        sometimes!(false, "unreachable-across-the-whole-sweep");
        for op in ops {
            let _ = op;
        }
    });

    match outcome {
        SweepOutcome::Vacuous {
            seeds_run,
            unsatisfied,
        } => {
            assert_eq!(seeds_run, 8);
            assert!(unsatisfied.contains("unreachable-across-the-whole-sweep"));
        }
        other => panic!("expected a vacuous sweep, got {other:?}"),
    }
}
