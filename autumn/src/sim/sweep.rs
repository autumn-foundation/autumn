//! Seed sweep runner (sim-testing W6 PR3, issue #1797).
//!
//! This is the **sweep** lane of the sim harness: running
//! [`Sim::run_proptest`] across a batch of seeds, in parallel, and reporting
//! the lowest-index failing seed (if any) — its shrunk minimal op-sequence
//! (proptest's own shrink loop, run per-seed by [`Sim::run_proptest`]) plus a
//! deterministic replay command. The `sim-sweep` `[[bin]]`
//! (`autumn/src/bin/sim_sweep.rs`) is the CI-facing driver; [`sweep_proptest`]
//! is the reusable library function it (and the `sim_sweep_driver` `DoD`
//! test) calls.
//!
//! # Design: parallel across available cores, full batch, deterministic result
//!
//! [`sweep_proptest`] dispatches the swept seeds across a
//! [`std::thread::available_parallelism`]-sized worker pool
//! ([`std::thread::scope`], no new dependency) rather than one seed at a
//! time. Two things make that safe and still fully deterministic:
//!
//! - The only shared mutable state a `Sim` run touches is the
//!   [`sometimes!`](crate::sometimes) reachability registry
//!   ([`sim::assert`](super::assert)), and it's **thread-local** — every
//!   other seam (`SimRng`, the clock, chaos/crash schedules) is a pure
//!   function of the seed, owned by that seed's own `Sim` value. Each worker
//!   folds *its own* thread's registry into a seed-local accumulator (see
//!   below) before handing that off to a shared aggregate — never reading or
//!   resetting another thread's registry.
//! - Every seed in the requested range is run to completion (no worker
//!   aborts early just because a peer found a failure elsewhere in the
//!   batch). After every worker finishes, the **lowest-index** failing seed
//!   among everything collected is reported — deterministic regardless of
//!   which worker happened to reach a failure first. This trades "stop the
//!   instant any failure is found" for a simpler, race-free result; for a
//!   *bounded* batch (`AUTUMN_SIM_SEEDS=N`, not an open-ended search) the
//!   worst-case total work is the same either way (≤ N seed-runs), so the
//!   only real cost is CI wall-clock in the failing case, not correctness.
//!
//! # Non-vacuity
//!
//! A sweep is only meaningfully green if it is also **non-vacuous**: every
//! [`sometimes!`](crate::sometimes) reachability label observed across the
//! *entire* swept range was satisfied by *some* seed. A single seed's
//! `Sim::run_proptest` drives up to `Config::default().cases` (256) candidate
//! op-sequences internally, each starting with a fresh registry (a fresh
//! `Sim::from_seed` resets it) — so [`sweep_proptest`] folds *every case's*
//! snapshot via [`Sim::run_proptest_with_case_hook`], not just a single
//! snapshot read after the whole seed finishes (which would silently see
//! only the last of up to 256 cases). Each seed's case-folded observations
//! are then merged into the sweep-wide aggregate; if the whole range passes
//! but some label was observed and never satisfied anywhere in it, the
//! outcome is [`SweepOutcome::Vacuous`] instead of [`SweepOutcome::Passed`] —
//! the same "never ship a vacuously-green suite" guarantee
//! [`sim::assert`](super::assert) exists for, now applied across the whole
//! sweep rather than a single run.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use proptest::strategy::Strategy;
use proptest::test_runner::TestError;

use super::Sim;
use super::assert::{reset_sometimes_registry, sometimes_snapshot};

/// The seed a sweep reports as failing, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepFailure<T> {
    /// The lowest-index seed (in sweep order) whose [`Sim::run_proptest`] run
    /// failed among every seed in the swept batch.
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
        // The replay count is decimal, matching `AUTUMN_SIM_SEEDS`'s own
        // decimal parser (`sim_sweep.rs`'s `seed_count`) — printing it as hex
        // here would silently fail to parse and fall back to the default
        // seed count, producing a replay command that doesn't actually
        // replay the reported failure.
        write!(
            f,
            "AUTUMN_SIM_SEED=0x{:x} — shrunk to {} op(s): {:?} ({})\n  \
             replay: AUTUMN_SIM_SEEDS={} cargo run -p autumn-web --release --features sim-testing --bin sim-sweep",
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
        /// How many seeds were run.
        seeds_run: u64,
    },
    /// `failure` is the lowest-index seed (in sweep order) that failed.
    Failed {
        /// How many seeds were run (always the full requested batch — see
        /// the module docs for why the sweep doesn't abort early).
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

/// Sweep `seeds` across a worker pool through [`Sim::run_proptest`].
///
/// Reports the lowest-index failing seed (if any), folding every case's
/// [`sometimes!`](crate::sometimes) observations (not just the last case per
/// seed) into a cross-seed non-vacuity aggregate. See the module docs for the
/// parallelism and non-vacuity design.
///
/// `strategy` and `body` are each shared by reference across every seed and
/// every worker — neither needs to be `Clone`.
///
/// # Panics
///
/// Panics if a worker thread panics while holding the internal aggregation
/// lock — this does not happen in ordinary use: a panic inside `body` (e.g.
/// an [`always!`](crate::always) violation) is caught by proptest's
/// `TestRunner` itself and turned into a case failure, never propagating to
/// the worker thread.
pub fn sweep_proptest<T, S, F>(
    seeds: impl IntoIterator<Item = u64>,
    strategy: &S,
    body: F,
) -> SweepOutcome<T>
where
    T: fmt::Debug + Send,
    S: Strategy<Value = Vec<T>> + Sync,
    F: Fn(&mut Sim, &[T]) + Sync,
{
    let seeds: Vec<u64> = seeds.into_iter().collect();
    let seeds_run = seeds.len() as u64;
    let worker_count = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(seeds.len().max(1));

    let next_index = AtomicUsize::new(0);
    let aggregate: Mutex<(BTreeSet<String>, BTreeSet<String>)> =
        Mutex::new((BTreeSet::new(), BTreeSet::new()));
    // (seed index, failure) for every seed in the batch that failed —
    // reduced to the lowest-index entry after every worker has finished the
    // whole batch, so the reported seed never depends on thread scheduling.
    let failures: Mutex<Vec<(usize, SweepFailure<T>)>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    let Some(&seed) = seeds.get(index) else {
                        break;
                    };

                    let mut seed_observed = BTreeSet::new();
                    let mut seed_satisfied = BTreeSet::new();
                    let result = Sim::run_proptest_with_case_hook(seed, strategy, &body, || {
                        let (observed, satisfied) = sometimes_snapshot();
                        seed_observed.extend(observed);
                        seed_satisfied.extend(satisfied);
                        reset_sometimes_registry();
                    });

                    {
                        let mut agg = aggregate.lock().unwrap();
                        agg.0.extend(seed_observed);
                        agg.1.extend(seed_satisfied);
                    }

                    if let Err(err) = result {
                        let (reason, shrunk_ops) = match err {
                            TestError::Fail(reason, shrunk_ops) => (reason.to_string(), shrunk_ops),
                            TestError::Abort(reason) => (format!("aborted: {reason}"), Vec::new()),
                        };
                        failures.lock().unwrap().push((
                            index,
                            SweepFailure {
                                seed,
                                shrunk_ops,
                                reason,
                            },
                        ));
                    }
                }
            });
        }
    });

    let mut failures = failures.into_inner().unwrap();
    failures.sort_by_key(|(index, _)| *index);
    if let Some((_, failure)) = failures.into_iter().next() {
        return SweepOutcome::Failed { seeds_run, failure };
    }

    let (all_observed, all_satisfied) = aggregate.into_inner().unwrap();
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
    fn failure_display_prints_a_decimal_replay_count_that_covers_the_failing_seed() {
        // `AUTUMN_SIM_SEEDS` (`sim_sweep.rs`'s `seed_count`) parses with plain
        // `str::parse::<u64>()`, i.e. decimal only — printing the replay count
        // as `0x…` here would silently fail to parse there and fall back to
        // the default seed count, producing a replay command that doesn't
        // actually cover (and so can't reproduce) the reported failure.
        let failure = SweepFailure {
            seed: 300u64,
            shrunk_ops: vec![TinyOp::Inc],
            reason: "example".to_owned(),
        };
        let rendered = failure.to_string();
        let replay_line = rendered
            .lines()
            .find(|line| line.contains("AUTUMN_SIM_SEEDS="))
            .expect("Display must print a replay line");
        let count_str = replay_line
            .split("AUTUMN_SIM_SEEDS=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("replay line must carry a seed count");
        let count: u64 = count_str.parse().unwrap_or_else(|err| {
            panic!("replay count {count_str:?} must parse as plain decimal u64: {err}")
        });
        assert!(
            count > failure.seed,
            "replay count {count} must exceed the failing seed {} so re-sweeping 0..{count} reaches it",
            failure.seed
        );
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
    fn sweep_aggregates_sometimes_observations_across_every_case_in_a_seed() {
        // `Config::default().cases` is 256 — a `sometimes!` satisfied by only
        // SOME of those 256 candidate op-sequences (not necessarily the last
        // one tried) must still show up as satisfied in the aggregate. A
        // single seed (0..1) makes this a direct test of the per-case fold,
        // not an artifact of averaging across many seeds.
        let strategy = tiny_op_strategy();
        let outcome = sweep_proptest(0..1, &strategy, |_sim, ops| {
            let has_inc = ops.iter().any(|op| *op == TinyOp::Inc);
            crate::sometimes!(has_inc, "case-contained-an-inc");
        });
        match outcome {
            SweepOutcome::Passed { seeds_run } => assert_eq!(seeds_run, 1),
            other => panic!(
                "expected a non-vacuous pass — some of the 256 cases in seed 0 must contain an Inc, got {other:?}"
            ),
        }
    }

    #[test]
    fn sweep_finds_the_lowest_index_failing_seed_in_the_batch() {
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
                assert_eq!(
                    seeds_run, 16,
                    "the full batch runs regardless of where the failure is"
                );
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
