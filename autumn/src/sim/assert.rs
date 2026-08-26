//! Simulation assertion macros — [`always!`](crate::always) /
//! [`sometimes!`](crate::sometimes) — plus the per-run non-vacuity registry
//! (sim-testing W6, issue #1797).
//!
//! This is the **semantic core** of the op-driver stack: the two assertions a
//! sim-test author reaches for, and the thread-local reachability registry the
//! forthcoming `sim-sweep` binary aggregates across seeds to prove a green run
//! was *non-vacuous* (every reachability target a run claimed to observe was
//! actually satisfied at least once).
//!
//! # The two assertions
//!
//! - [`always!`](crate::always) is a **hard invariant**. `always!(cond)` (or
//!   `always!(cond, "fmt {}", args…)`) panics the instant `cond` is false. Under
//!   [`#[sim_test]`](crate::sim_test) the panic is caught, the copy-pasteable
//!   `AUTUMN_SIM_SEED=…` replay line is printed, and the test fails — so an
//!   `always!` violation reproduces bit-for-bit. The panic message is
//!   deliberately greppable (`always! invariant violated`) and carries the
//!   stringified condition plus any caller message.
//!
//! - [`sometimes!`](crate::sometimes) is a **reachability target**.
//!   `sometimes!(cond, "label")` records `"label"` as *observed* in the per-run
//!   registry, and marks it *satisfied* when `cond` is true. Within a single
//!   seed run a reachability target may legitimately never fire, so a
//!   [`#[sim_test]`](crate::sim_test) does **not** auto-fail on an unsatisfied
//!   label — that would make reachability assertions useless. Instead the
//!   registry is exposed for the sweep to aggregate across many seeds and fail
//!   the sweep if some label was seen but never satisfied by *any* seed
//!   (non-vacuous green). For an explicit single-run check, call
//!   [`assert_all_sometimes_satisfied`].
//!
//! # Registry model (thread-local, deterministic)
//!
//! The registry is a thread-local pair of [`BTreeSet`]s (observed / satisfied),
//! keyed by label string. `BTreeSet` (not `HashSet`) keeps panic-message and
//! snapshot ordering stable across runs, which the determinism contract
//! requires.
//!
//! Thread-local is the correct isolation boundary under the test harness: libtest
//! runs each `#[test]` on its own thread, so two tests touching the global
//! registry concurrently never interfere — each observes its own thread's
//! registry. [`Sim::from_seed`](crate::sim::Sim::from_seed) resets the current
//! thread's registry so every seed run starts clean, and the sweep (which runs
//! seeds sequentially on one thread) reads [`sometimes_snapshot`] after each seed
//! before resetting for the next.

use std::cell::RefCell;
use std::collections::BTreeSet;

thread_local! {
    /// The current thread's reachability registry. One per test thread, so
    /// concurrent tests never contend (see the module docs).
    static SOMETIMES_REGISTRY: RefCell<SometimesRegistry> =
        const { RefCell::new(SometimesRegistry::new()) };
}

/// The per-run reachability registry backing [`sometimes!`](crate::sometimes).
///
/// Tracks two label sets: every label **observed** (a `sometimes!` was reached)
/// and the subset that was **satisfied** (its condition was true at least once).
/// A label that is observed but never satisfied is the non-vacuity signal the
/// sweep fails on.
#[derive(Debug, Default, Clone)]
pub struct SometimesRegistry {
    observed: BTreeSet<String>,
    satisfied: BTreeSet<String>,
}

impl SometimesRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            observed: BTreeSet::new(),
            satisfied: BTreeSet::new(),
        }
    }

    /// Record that `label` was observed, marking it satisfied when `satisfied`.
    fn observe(&mut self, label: &str, satisfied: bool) {
        self.observed.insert(label.to_owned());
        if satisfied {
            self.satisfied.insert(label.to_owned());
        }
    }

    /// The labels that were observed but never satisfied (in stable sorted
    /// order). An empty set means the run was non-vacuous for every target it
    /// reached.
    #[must_use]
    pub fn unsatisfied(&self) -> BTreeSet<String> {
        self.observed.difference(&self.satisfied).cloned().collect()
    }

    /// Every label observed this run (in stable sorted order).
    #[must_use]
    pub const fn observed(&self) -> &BTreeSet<String> {
        &self.observed
    }

    /// Every label satisfied this run (in stable sorted order).
    #[must_use]
    pub const fn satisfied(&self) -> &BTreeSet<String> {
        &self.satisfied
    }

    /// Clear both label sets.
    fn reset(&mut self) {
        self.observed.clear();
        self.satisfied.clear();
    }
}

/// Record a [`sometimes!`](crate::sometimes) observation on the current thread's
/// registry.
///
/// Hidden plumbing the [`sometimes!`](crate::sometimes) macro expands to — not a
/// stable API. Call the macro, not this function.
#[doc(hidden)]
pub fn __sometimes_observe(label: &str, satisfied: bool) {
    SOMETIMES_REGISTRY.with(|registry| registry.borrow_mut().observe(label, satisfied));
}

/// Reset the current thread's reachability registry to empty.
///
/// Called by [`Sim::from_seed`](crate::sim::Sim::from_seed) so each seed run
/// starts clean, and exposed for the sweep to reset between seeds after reading
/// its [`sometimes_snapshot`].
pub fn reset_sometimes_registry() {
    SOMETIMES_REGISTRY.with(|registry| registry.borrow_mut().reset());
}

/// Snapshot the current thread's registry as `(observed, satisfied)` label sets.
///
/// The sweep reads this after each seed run to fold the seed's reachability into
/// its cross-seed aggregate before resetting for the next seed.
#[must_use]
pub fn sometimes_snapshot() -> (BTreeSet<String>, BTreeSet<String>) {
    SOMETIMES_REGISTRY.with(|registry| {
        let registry = registry.borrow();
        (registry.observed.clone(), registry.satisfied.clone())
    })
}

/// The labels observed but never satisfied on the current thread's registry
/// (stable sorted order).
///
/// Convenience over [`sometimes_snapshot`] for callers that only need the
/// non-vacuity gap.
#[must_use]
pub fn sometimes_unsatisfied() -> BTreeSet<String> {
    SOMETIMES_REGISTRY.with(|registry| registry.borrow().unsatisfied())
}

/// Assert that every [`sometimes!`](crate::sometimes) label observed this run was
/// also satisfied at least once, panicking otherwise.
///
/// For explicit single-run non-vacuity checks. The cross-seed aggregation the
/// sweep performs is the more powerful form (a label may need many seeds to be
/// satisfied once), so a [`#[sim_test]`](crate::sim_test) body typically does
/// **not** call this directly.
///
/// # Panics
///
/// Panics, listing the never-satisfied labels in stable sorted order, if any
/// observed label was never satisfied.
pub fn assert_all_sometimes_satisfied() {
    let unsatisfied = sometimes_unsatisfied();
    assert!(
        unsatisfied.is_empty(),
        "sometimes! reachability never satisfied for label(s): {}",
        unsatisfied.into_iter().collect::<Vec<_>>().join(", "),
    );
}

/// Assert a **hard invariant** that must hold at this point in the simulation.
///
/// `always!(cond)` panics the instant `cond` is false; `always!(cond, "fmt {}",
/// args…)` appends a caller-formatted message. Under
/// [`#[sim_test]`](crate::sim_test) the panic is caught and the deterministic
/// `AUTUMN_SIM_SEED=…` replay line is printed, so a violation reproduces exactly.
/// The message always contains the greppable prefix `always! invariant violated`
/// and the stringified condition.
///
/// ```
/// use autumn_web::always;
///
/// always!(1 + 1 == 2);
/// let balance = 10;
/// always!(balance >= 0, "balance must never go negative, was {}", balance);
/// ```
#[macro_export]
macro_rules! always {
    ($cond:expr $(,)?) => {
        if !$cond {
            ::std::panic!(
                "always! invariant violated: `{}`",
                ::std::stringify!($cond),
            );
        }
    };
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            ::std::panic!(
                "always! invariant violated: `{}`: {}",
                ::std::stringify!($cond),
                ::std::format_args!($($arg)+),
            );
        }
    };
}

/// Record a **reachability target** — an interesting state the simulation should
/// sometimes reach across seeds.
///
/// `sometimes!(cond, "label")` registers `"label"` as observed and marks it
/// satisfied when `cond` is true, evaluating `cond` exactly once. It never
/// panics and never fails the current run on its own — an unsatisfied label
/// fails the *sweep*, which aggregates reachability across many seeds (see the
/// [module docs](self)). Use [`assert_all_sometimes_satisfied`] for an explicit
/// single-run check.
///
/// ```
/// use autumn_web::sometimes;
///
/// let queued = 5;
/// sometimes!(queued > 0, "queue-was-non-empty");
/// sometimes!(queued > 100, "queue-hit-backpressure");
/// ```
#[macro_export]
macro_rules! sometimes {
    ($cond:expr, $label:expr $(,)?) => {
        $crate::sim::assert::__sometimes_observe($label, $cond)
    };
}

#[cfg(test)]
mod tests {
    use super::{SometimesRegistry, reset_sometimes_registry, sometimes_snapshot};

    // The pure registry: observe records observed; satisfied only when the flag
    // is set; unsatisfied is exactly observed − satisfied, sorted.
    #[test]
    fn registry_tracks_observed_and_satisfied() {
        let mut reg = SometimesRegistry::new();
        reg.observe("a", true);
        reg.observe("b", false);
        reg.observe("c", false);
        reg.observe("c", true); // a later satisfaction promotes the label

        assert_eq!(
            reg.observed().iter().cloned().collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(
            reg.satisfied().iter().cloned().collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        // Sorted, stable: only "b" was seen but never satisfied.
        assert_eq!(
            reg.unsatisfied().into_iter().collect::<Vec<_>>(),
            vec!["b".to_owned()]
        );
    }

    #[test]
    fn registry_reset_clears_both_sets() {
        let mut reg = SometimesRegistry::new();
        reg.observe("x", true);
        reg.reset();
        assert!(reg.observed().is_empty());
        assert!(reg.satisfied().is_empty());
        assert!(reg.unsatisfied().is_empty());
    }

    // The thread-local plumbing: this test owns its thread (libtest runs each
    // `#[test]` on its own thread), so resetting first keeps it hermetic.
    #[test]
    fn thread_local_snapshot_reflects_observations() {
        reset_sometimes_registry();
        crate::sometimes!(true, "tl-satisfied");
        crate::sometimes!(false, "tl-observed-only");

        let (observed, satisfied) = sometimes_snapshot();
        assert!(observed.contains("tl-satisfied"));
        assert!(observed.contains("tl-observed-only"));
        assert!(satisfied.contains("tl-satisfied"));
        assert!(!satisfied.contains("tl-observed-only"));
    }
}
