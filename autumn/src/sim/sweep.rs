//! Seed sweep runner (sim-testing W6 PR3, issue #1797).
//!
//! This is the **sweep** lane of the sim harness: running
//! [`Sim::run_proptest`] across a batch of seeds, stopping at the first
//! failing seed and reporting it — its shrunk minimal op-sequence (proptest's
//! own shrink loop, run per-seed by [`Sim::run_proptest`]) plus a
//! deterministic replay seed. The `sim-sweep` `[[bin]]` (`autumn/src/bin/sim_sweep.rs`)
//! is the CI-facing driver; [`sweep_proptest`] is the reusable library
//! function it (and the `sim_sweep_driver` `DoD` test) calls.
//!
//! # Design: sequential, not parallel
//!
//! The RFC that opened issue #1797 sketched a *parallel* seed sweep ("runs
//! seeds in parallel, stops on the first failure"). This module runs seeds
//! **sequentially on one thread instead**, because the W6 PR1 [`sometimes!`](crate::sometimes)
//! non-vacuity registry ([`sim::assert`](super::assert)) that already shipped is
//! **thread-local** and documents exactly this sweep as its aggregator:
//!
//! > "the sweep (which runs seeds sequentially on one thread) reads
//! > `sometimes_snapshot` after each seed before resetting for the next"
//!
//! Spreading seeds across OS threads would fragment that registry — each
//! worker thread would accumulate its own independent, unaggregated
//! observed/satisfied sets — breaking cross-seed non-vacuity detection
//! ([`SweepOutcome::Vacuous`] below). Honoring the already-shipped contract
//! took priority over the earlier draft's "in parallel" framing.
//!
//! # Non-vacuity
//!
//! A sweep is only meaningfully green if it is also **non-vacuous**: every
//! [`sometimes!`](crate::sometimes) reachability label observed across the
//! *entire* swept range was satisfied by *some* seed. [`sweep_proptest`] folds
//! [`sometimes_snapshot`] into a cross-seed aggregate after every seed and
//! reports [`SweepOutcome::Vacuous`] instead of [`SweepOutcome::Passed`] if any
//! label was observed but never satisfied — the same "never ship a
//! vacuously-green suite" guarantee [`sim::assert`](super::assert) exists for,
//! now applied across the whole sweep rather than a single run.

use std::collections::BTreeSet;
use std::fmt;

use proptest::strategy::Strategy;
use proptest::test_runner::TestError;

use super::Sim;
use super::assert::{reset_sometimes_registry, sometimes_snapshot};

/// The seed that stopped a sweep, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepFailure<T> {
    /// The first seed (in sweep order) whose [`Sim::run_proptest`] run failed.
    pub seed: u64,
    /// The minimal op-sequence proptest shrunk this seed's failure down to.
    /// Empty for a [`TestError::Abort`] (the run itself could not proceed —
    /// there is no failing case to report).
    pub shrunk_ops: Vec<T>,
    /// The panic/assertion message (or, for an abort, the abort reason).
    pub reason: String,
}

impl<T: fmt::Debug> fmt::Display for SweepFailure<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AUTUMN_SIM_SEED=0x{:x} — shrunk to {} op(s): {:?} ({})\n  \
             replay: AUTUMN_SIM_SEEDS=0x{:x} cargo run -p autumn-web --features sim-testing --bin sim-sweep",
            self.seed,
            self.shrunk_ops.len(),
            self.shrunk_ops,
            self.reason,
            self.seed.wrapping_add(1),
        )
    }
}

/// The outcome of sweeping [`sweep_proptest`] over a range of seeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepOutcome<T> {
    /// Every swept seed passed, and every [`sometimes!`](crate::sometimes)
    /// label observed across the whole sweep was satisfied at least once.
    Passed {
        /// How many seeds were run before the sweep concluded.
        seeds_run: u64,
    },
    /// `failure.seed` was the first seed (in sweep order) to fail; the sweep
    /// stopped there without running any later seed.
    Failed {
        /// How many seeds were run before the sweep stopped (includes the
        /// failing seed itself).
        seeds_run: u64,
        /// The failing seed and its shrunk reproduction.
        failure: SweepFailure<T>,
    },
    /// Every swept seed passed individually, but the sweep was **vacuous**:
    /// at least one [`sometimes!`](crate::sometimes) label was observed and
    /// never satisfied by any seed in the range (see the module docs).
    Vacuous {
        /// How many seeds were run.
        seeds_run: u64,
        /// The unsatisfied labels, in stable sorted order.
        unsatisfied: BTreeSet<String>,
    },
}

/// Sweep `seeds` sequentially through [`Sim::run_proptest`], stopping at the
/// first failing seed (fail-fast) and folding each seed's
/// [`sometimes_snapshot`] into a cross-seed non-vacuity aggregate.
///
/// `strategy` and `body` are each shared by reference across every seed —
/// neither needs to be `Clone`. See the module docs for why this runs on one
/// thread rather than in parallel.
pub fn sweep_proptest<T, S, F>(
    seeds: impl IntoIterator<Item = u64>,
    strategy: &S,
    body: F,
) -> SweepOutcome<T>
where
    T: fmt::Debug,
    S: Strategy<Value = Vec<T>>,
    F: Fn(&mut Sim, &[T]),
{
    let mut seeds_run = 0u64;
    let mut all_observed = BTreeSet::new();
    let mut all_satisfied = BTreeSet::new();

    for seed in seeds {
        seeds_run += 1;
        let result = Sim::run_proptest_with_ref(seed, strategy, &body);

        let (observed, satisfied) = sometimes_snapshot();
        all_observed.extend(observed);
        all_satisfied.extend(satisfied);
        reset_sometimes_registry();

        if let Err(err) = result {
            let (reason, shrunk_ops) = match err {
                TestError::Fail(reason, shrunk_ops) => (reason.to_string(), shrunk_ops),
                TestError::Abort(reason) => (format!("aborted: {reason}"), Vec::new()),
            };
            return SweepOutcome::Failed {
                seeds_run,
                failure: SweepFailure {
                    seed,
                    shrunk_ops,
                    reason,
                },
            };
        }
    }

    let unsatisfied: BTreeSet<String> = all_observed.difference(&all_satisfied).cloned().collect();
    if unsatisfied.is_empty() {
        SweepOutcome::Passed { seeds_run }
    } else {
        SweepOutcome::Vacuous {
            seeds_run,
            unsatisfied,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TinyOp {
        Inc,
        Dec,
    }

    fn tiny_op_strategy() -> impl Strategy<Value = Vec<TinyOp>> {
        proptest::collection::vec(prop_oneof![Just(TinyOp::Inc), Just(TinyOp::Dec)], 0..16)
    }

    #[test]
    fn sweep_passes_when_every_seed_passes_and_is_non_vacuous() {
        let strategy = tiny_op_strategy();
        let outcome = sweep_proptest(0..8, &strategy, |_sim, ops| {
            crate::sometimes!(!ops.is_empty(), "generated-at-least-one-op");
            for op in ops {
                let _ = op;
            }
        });
        assert_eq!(outcome, SweepOutcome::Passed { seeds_run: 8 });
    }

    #[test]
    fn sweep_reports_a_vacuous_sometimes_label_across_the_whole_range() {
        let strategy = tiny_op_strategy();
        // Never satisfied by construction, but always observed.
        let outcome = sweep_proptest(0..4, &strategy, |_sim, _ops| {
            crate::sometimes!(false, "never-satisfied-label");
        });
        match outcome {
            SweepOutcome::Vacuous {
                seeds_run,
                unsatisfied,
            } => {
                assert_eq!(seeds_run, 4);
                assert_eq!(
                    unsatisfied.into_iter().collect::<Vec<_>>(),
                    vec!["never-satisfied-label".to_owned()]
                );
            }
            other => panic!("expected a vacuous outcome, got {other:?}"),
        }
    }

    #[test]
    fn sweep_stops_at_the_first_failing_seed_and_shrinks_it() {
        // Fails whenever three or more `Inc`s appear back to back.
        let strategy = tiny_op_strategy();
        let outcome = sweep_proptest(0..16, &strategy, |_sim, ops| {
            let mut run = 0;
            for op in ops {
                run = if *op == TinyOp::Inc { run + 1 } else { 0 };
                assert!(run < 3, "three Incs in a row");
            }
        });
        match outcome {
            SweepOutcome::Failed { seeds_run, failure } => {
                assert!(seeds_run >= 1);
                assert_eq!(
                    failure.shrunk_ops,
                    vec![TinyOp::Inc, TinyOp::Inc, TinyOp::Inc]
                );
            }
            other => panic!("expected a failing sweep, got {other:?}"),
        }
    }

    #[test]
    fn sweep_failure_is_deterministic_across_runs() {
        let strategy = tiny_op_strategy();
        let run = || {
            sweep_proptest(0..16, &strategy, |_sim, ops| {
                let mut run = 0;
                for op in ops {
                    run = if *op == TinyOp::Inc { run + 1 } else { 0 };
                    assert!(run < 3, "three Incs in a row");
                }
            })
        };
        assert_eq!(run(), run());
    }
}
