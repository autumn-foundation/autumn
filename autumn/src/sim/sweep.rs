//! Seed sweep runner (sim-testing W6 PR3, issue #1797).
//!
//! This is the **sweep** lane of the sim harness: running
//! [`Sim::run_proptest`] across a batch of seeds, sequentially, and reporting
//! the first failing seed (if any) and its shrunk minimal op-sequence
//! (proptest's own shrink loop, run per-seed by [`Sim::run_proptest`]).
//! [`sweep_proptest`] is a caller-agnostic public library function — it does
//! not prescribe a replay command, since it has no idea what test or binary
//! is calling it with what strategy; a caller that knows its own invocation
//! context appends its own replay suggestion when it reports the failure
//! (see `autumn/src/bin/sim_sweep.rs`, the CI-facing driver, for a worked
//! example).
//!
//! # Design: sequential, not parallel — and why that's not a regression
//!
//! An earlier revision of this module parallelized the sweep across a
//! `std::thread`-per-core worker pool. Review caught four escalating,
//! genuinely real bugs in that design, and the last two are not
//! patch-level fixes — they're a hard architectural wall:
//!
//! - **Process-global state.** `TestApp::build` (what `Sim::build`/`body`
//!   calls to mount a real app) unconditionally clears process-global state
//!   at the top of the function — the cache (`crate::cache::clear_global_cache`)
//!   and the event bus (`crate::events::clear_global_event_bus`) — and, when
//!   the app configures jobs, installs a process-global `GLOBAL_JOB_CLIENT`
//!   (`job.rs`). None of this is per-`Sim`-instance or thread-local. Two
//!   `body` closures calling `sim.build(...)` **concurrently** on different
//!   worker threads race on all of it — one seed's app can clear or
//!   redirect another seed's cache, event bus, or job routing mid-run. This
//!   isn't something a worker-local fix (like the paused-runtime-per-worker
//!   fix an intermediate revision added) can solve; it needs either a global
//!   lock serializing every `sim.build`-calling case (which defeats the
//!   point of parallelizing exactly the scenarios that matter most) or
//!   process-per-worker isolation (far outside this PR's scope).
//! - **The sync-only `body` signature can't drive spawned work.**
//!   `Sim::run_proptest`'s `body: Fn(&mut Sim, &[T])` is plain synchronous —
//!   it cannot `.await` anything, so it can never call [`Sim::advance`] or
//!   [`Sim::run_to_idle`] (both `async fn`) to actually drain a mounted
//!   app's background job workers. Merely `.enter()`ing a Tokio runtime on
//!   each worker (as the intermediate revision did) stops `tokio::spawn`
//!   from panicking, but never *polls* the runtime, so any spawned
//!   background tasks stay permanently inert — a job-backed property would
//!   silently test against an app whose workers never run. Fixing this for
//!   real needs `Sim::run_proptest`'s body to become async — a breaking
//!   change to the already-shipped W6 PR2 signature, out of scope here.
//!
//! Both are specific to `body` closures that call `sim.build`/mount a real
//! app — every scenario this PR actually ships (the `sim-sweep` bin's demo,
//! `sim_sweep_driver`'s `DoD` tests, this module's own unit tests) is a
//! self-contained op-application model with no app-mounting at all, so
//! neither bug is reachable from anything merged here. But `sweep_proptest`
//! is a *public* function; a future caller mounting a real app is exactly
//! the scenario the whole sim-testing effort exists for, and it must not
//! silently corrupt itself. Sequential execution sidesteps both bugs
//! entirely — there is never more than one `body` invocation in flight, so
//! there is nothing to race regardless of what `body` does — while costing
//! only sweep wall-clock, not correctness or coverage: **sweep-level
//! threading is orthogonal to the harness's actual concurrency-bug-finding
//! power**, which comes from exploring different seeds (each driving a
//! *single-threaded, deterministic* `Sim` executor internally, per W1's
//! design), not from how many OS threads process the outer seed range.
//!
//! This also matches the already-shipped W6 PR1 [`sometimes!`](crate::sometimes)
//! non-vacuity registry ([`sim::assert`](super::assert)), whose own module
//! docs already named this exact sweep as "the sweep (which runs seeds
//! sequentially on one thread)" — sequential was the original, reviewed
//! design; the parallel revision was a detour that review correctly walked
//! back.
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

use proptest::strategy::Strategy;
use proptest::test_runner::TestError;

use super::Sim;
use super::assert::{reset_sometimes_registry, sometimes_snapshot};

/// The seed a sweep reports as failing, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepFailure<T> {
    /// The first seed (in sweep order) whose [`Sim::run_proptest`] run
    /// failed; the sweep stopped there without running any later seed.
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
        // Caller-agnostic — [`sweep_proptest`] is a public library function
        // any application can call with its own strategy/body, so this must
        // not prescribe a specific command (that was a real bug: it used to
        // hard-code the `sim-sweep` bin's own invocation, which runs an
        // unrelated built-in demo and can pass even when the reported seed
        // reliably fails the *caller's* property). Mirrors
        // [`Sim::run_proptest`]'s own replay line for the same reason — a
        // caller that knows its own invocation context (like the `sim-sweep`
        // bin) appends its own replay suggestion when it prints this.
        write!(
            f,
            "AUTUMN_SIM_SEED=0x{:x} — shrunk to {} op(s): {:?} ({})",
            self.seed,
            self.shrunk_ops.len(),
            self.shrunk_ops,
            self.reason,
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

/// Sweep `seeds` sequentially through [`Sim::run_proptest`].
///
/// Stops at the first failing seed (fail-fast) and folds every case's
/// [`sometimes!`](crate::sometimes) observations (not just the last case per
/// seed) into a cross-seed non-vacuity aggregate. See the module docs for why
/// this runs sequentially rather than in parallel.
///
/// `strategy` and `body` are each shared by reference across every seed —
/// neither needs to be `Clone`.
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

        let mut seed_observed = BTreeSet::new();
        let mut seed_satisfied = BTreeSet::new();
        let result = Sim::run_proptest_with_case_hook(seed, strategy, &body, || {
            let (observed, satisfied) = sometimes_snapshot();
            seed_observed.extend(observed);
            seed_satisfied.extend(satisfied);
            reset_sometimes_registry();
        });

        all_observed.extend(seed_observed);
        all_satisfied.extend(seed_satisfied);

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
    fn failure_display_is_caller_agnostic_and_does_not_prescribe_a_replay_command() {
        // `sweep_proptest` is a public library function any application can
        // call with its own strategy/body — `Display` used to hard-code a
        // `cargo run ... --bin sim-sweep` suggestion, which is wrong for
        // every caller except that one specific demo binary (that command
        // runs sim-sweep's *own* built-in scenario, not the caller's). A
        // caller that knows its own invocation context appends its own
        // suggestion instead (see `sim_sweep.rs`'s `main`).
        let failure = SweepFailure {
            seed: 300u64,
            shrunk_ops: vec![TinyOp::Inc],
            reason: "example".to_owned(),
        };
        let rendered = failure.to_string();
        assert!(
            !rendered.contains("cargo run") && !rendered.contains("sim-sweep"),
            "Display must not prescribe a specific replay command, got: {rendered:?}"
        );
        assert!(
            rendered.contains("0x12c"),
            "Display must still report the failing seed, got: {rendered:?}"
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
