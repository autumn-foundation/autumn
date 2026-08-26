//! Property-based `Op` generation and shrinking (sim-testing W6 PR2, issue
//! #1797).
//!
//! This is the **op-driver** lane of the sim harness: turning a seeded [`Sim`]
//! into a source of arbitrary, reproducible operation sequences, and — when a
//! sequence uncovers an [`always!`](crate::always) violation — shrinking that
//! sequence down to a minimal reproduction.
//!
//! # Design (rulings taken 2026-08-01, issue #1797 comment thread)
//!
//! Three shape questions were open before this could be written faithfully.
//! This module answers all three; see the issue thread for the full reasoning,
//! summarized here:
//!
//! 1. **Generic, not a shipped concrete `Op`.** [`Sim::gen_ops`] is
//!    `fn gen_ops<T: Arbitrary>(&mut self) -> Vec<T>` — the app supplies its own
//!    operation enum and `Arbitrary`/`Strategy` impl. A frozen, domain-agnostic
//!    [`Sim`] cannot ship a fixed operation vocabulary (the RFC's own worked
//!    examples, `Deposit`/`Transfer`, are app-domain, not framework-domain).
//!    (Named `gen_ops`, not the ruling's proposed `gen` — `gen` became a
//!    reserved keyword under the 2024 edition this workspace builds on, so it
//!    is not available as a method identifier without an ugly `r#gen` raw-ident
//!    call site.)
//! 2. **A runner-owning shrink entrypoint, `gen_ops()` redefined as
//!    generation-only.** [`Sim::gen_ops`] / [`Sim::gen_ops_with`] draw exactly one
//!    `Vec<T>` — they cannot shrink, because shrinking requires proptest to
//!    observe a case's pass/fail and rebuild state before the next try.
//!    [`Sim::run_proptest`] is the shrink-capable entrypoint: it owns a
//!    proptest `TestRunner`, and for every case (including every shrink
//!    attempt) rebuilds a **fresh** `Sim` from the same base seed before
//!    invoking the case closure — mirroring [`#[sim_test]`](crate::sim_test)'s
//!    "exactly one `Sim` per run" contract at the per-case level instead of the
//!    per-test level. This is additive: `#[sim_test]` and `Sim::build` are
//!    untouched.
//! 3. **`proptest` promoted to an optional *library* dependency** behind the
//!    new `sim-testing` feature (see `autumn/Cargo.toml`), since this module
//!    lives in `src/`, not `tests/`, and the `sim-sweep` `[[bin]]` ([`sim::sweep`](super::sweep),
//!    W6 PR3) needs it as a lib dep too — a `[[bin]]` cannot use dev-deps.
//!
//! # Determinism
//!
//! Both [`Sim::gen_ops`]/[`Sim::gen_ops_with`] and [`Sim::run_proptest`] seed proptest's
//! own case-generation RNG from a **dedicated stream**, `seed ^
//! OP_GEN_STREAM_SALT` — independent of the app-facing entropy source
//! ([`Sim::seeded_entropy`]), the [`chaos`](crate::sim::chaos) decision stream,
//! and the [`crash`](crate::sim::crash) decision stream (each salted
//! separately, so none of these streams perturb each other). Same seed ⇒ same
//! generated op sequence ⇒ same shrink path, byte-for-byte.
//!
//! Both also disable proptest's own file-based failure persistence
//! (`proptest-regressions/*.txt`) — the sim harness's `AUTUMN_SIM_SEED=…`
//! replay line is the single source of truth for reproducing a failure, so a
//! second, proptest-owned persistence file would just be a redundant (and
//! potentially stale) source of truth — and force off proptest's fork mode
//! and fork timeout regardless of any ambient `PROPTEST_FORK` /
//! `PROPTEST_TIMEOUT` env vars: both runners are owned in-process (there's no
//! `test_name` to hand proptest's forking machinery), so inheriting fork mode
//! would panic instead of generating ops.
//!
//! # Calling `body` closures that mount an app
//!
//! [`Sim::run_proptest`] itself neither builds nor enters a Tokio runtime — it
//! calls `body` directly, on whatever thread and in whatever ambient context
//! the caller invoked it from (this predates the op-driver; it was already
//! true of every earlier `Sim` entrypoint). A `body` that mounts a real app
//! via [`Sim::build`] therefore needs the **caller** to already have an
//! entered Tokio runtime — [`Sim::build`] starts the local job runtime
//! through a bare `tokio::spawn` (`job.rs`) when the app configures jobs,
//! which panics with `"there is no reactor running"` outside one. Call
//! [`Sim::run_proptest`] synchronously from inside an already-running async
//! context (e.g. a `#[tokio::test]` function, or code already inside
//! `Runtime::block_on`) rather than from a bare `fn main()` or a plain,
//! non-async `#[test]`.
//!
//! Even with a runtime entered, `body: Fn(&mut Sim, &[T])` is plain
//! synchronous — it can never itself `.await` [`Sim::advance`] /
//! [`Sim::run_to_idle`] to drain a mounted app's background job workers
//! mid-body, so job-backed invariants are better proven with
//! [`#[sim_test]`](crate::sim_test) (whose body is `async` and can drive
//! `run_to_idle` directly) than with the op-driver until a future async-body
//! variant of `run_proptest` exists.
//!
//! # Example
//!
//! ```rust,ignore
//! use autumn_web::sim::Sim;
//! use proptest::prelude::*;
//!
//! #[derive(Debug, Clone, proptest_derive::Arbitrary)]
//! enum Op {
//!     Deposit(u32),
//!     Transfer { to: u8, amount: u32 },
//! }
//!
//! // Called from inside a `#[tokio::test]` (or otherwise already inside a
//! // running Tokio runtime) — see "Calling `body` closures that mount an
//! // app" above for why that's required here.
//! let result = Sim::run_proptest(0, proptest::collection::vec(any::<Op>(), 1..32), |sim, ops| {
//!     sim.build(app());
//!     for op in ops {
//!         apply(sim, op);
//!     }
//!     always!(invariant_holds(sim));
//! });
//! assert!(result.is_ok(), "{}", result.unwrap_err());
//! ```

use std::fmt;

use proptest::arbitrary::{Arbitrary, any};
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::{Config, RngAlgorithm, TestError, TestRng, TestRunner};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

use super::Sim;

/// Salt `XOR`ed into the sim seed to derive the op-generation RNG stream,
/// keeping it independent of the app-facing entropy, chaos-decision, and
/// crash-decision streams (each salted with their own distinct constant). An
/// arbitrary fixed non-zero constant.
const OP_GEN_STREAM_SALT: u64 = 0x0B_D1_2E_4A_0B_D1_2E_4A;

/// The default op-sequence length range [`Sim::gen_ops`] draws from. Callers who
/// need a different range (or a non-uniform shape) should use
/// [`Sim::gen_ops_with`] / [`Sim::run_proptest`] with an explicit strategy instead.
const DEFAULT_OP_COUNT: std::ops::Range<usize> = 1..64;

/// Derive a proptest [`TestRng`] deterministically from `seed ^ salt`, via the
/// same `ChaCha8Rng`-expansion pattern [`crash::CrashSchedule`](crate::sim::crash::CrashSchedule)
/// uses for its own seed-derived stream.
fn stream_rng(seed: u64, salt: u64) -> TestRng {
    let mut seeder = ChaCha8Rng::seed_from_u64(seed ^ salt);
    let mut bytes = [0u8; 32];
    seeder.fill_bytes(&mut bytes);
    TestRng::from_seed(RngAlgorithm::ChaCha, &bytes)
}

/// The case count [`embedded_config`] pins regardless of any ambient
/// `PROPTEST_CASES` — matches proptest's own documented default
/// (`Config::default().cases` absent any env override), so callers who never
/// set `PROPTEST_CASES` see no behavior change.
const EMBEDDED_CASES: u32 = 256;

/// A [`Config`] for the embedded, in-process runners [`Sim::gen_ops_with`] and
/// [`Sim::run_proptest`] own directly (as opposed to a runner `#[test]`-owned
/// proptest picks up via `proptest!`/`#[proptest_derive]`).
///
/// Disables proptest's own file-based failure persistence (see the module
/// docs' "Determinism" section for why) and, since [`Config::default`] also
/// pulls in whatever `PROPTEST_FORK` / `PROPTEST_TIMEOUT` / `PROPTEST_CASES`
/// env vars happen to be set process-wide:
///
/// - forces fork mode and the fork timeout off — this runner has no
///   `test_name` to give proptest's forking machinery, so inheriting fork
///   mode from the environment would panic (`Must supply test_name when
///   forking enabled`) before a single op could be generated;
/// - pins the case count to [`EMBEDDED_CASES`], entirely ignoring
///   `PROPTEST_CASES` — `PROPTEST_CASES=0` would otherwise make
///   [`TestRunner::run`] return `Ok(())` without ever invoking the case
///   closure, so `body` (and any `always!`/`sometimes!` inside it) never
///   runs at all while [`Sim::run_proptest`] still reports success, silently
///   defeating the whole point of the sweep this backs
///   ([`sim::sweep`](super::sweep)). A bare `.max(1)` clamp would avoid that
///   specific failure but still shrinks worse: proptest's automatic
///   `max_shrink_iters` is `cases.saturating_mul(4)`, so clamping to the
///   bare minimum of 1 case also collapses the shrink budget to 4
///   iterations, aborting shrinking early with a far-from-minimal
///   counterexample — pinning the whole case count sidesteps that too.
fn embedded_config() -> Config {
    Config {
        failure_persistence: None,
        fork: false,
        timeout: 0,
        cases: EMBEDDED_CASES,
        ..Config::default()
    }
}

impl Sim {
    /// Draw a single deterministic `Vec<T>` from `T`'s [`Arbitrary`] strategy,
    /// seeded from this simulation's seed.
    ///
    /// This is **generation-only** — it does not shrink. Reach for
    /// [`Sim::run_proptest`] when a generated sequence needs to shrink toward a
    /// minimal counterexample on failure. Use [`Sim::gen_ops_with`] for a custom
    /// strategy (length range, weighting, `prop_filter`, …) instead of `T`'s
    /// default `Arbitrary` impl.
    pub fn gen_ops<T>(&mut self) -> Vec<T>
    where
        T: fmt::Debug + Arbitrary,
    {
        self.gen_ops_with(proptest::collection::vec(any::<T>(), DEFAULT_OP_COUNT))
    }

    /// Draw a single deterministic value from an explicit `strategy`, seeded
    /// from this simulation's seed. See [`Sim::gen_ops`] for the default-strategy
    /// convenience form.
    ///
    /// # Panics
    ///
    /// Panics if `strategy` itself fails to produce a value (a `prop_filter`
    /// or similar strategy combinator rejecting every draw) — this mirrors
    /// proptest's own `Strategy::new_tree` failure mode and should not happen
    /// for a well-formed op strategy.
    pub fn gen_ops_with<T>(&mut self, strategy: impl Strategy<Value = Vec<T>>) -> Vec<T>
    where
        T: fmt::Debug,
    {
        let mut runner =
            TestRunner::new_with_rng(embedded_config(), stream_rng(self.seed, OP_GEN_STREAM_SALT));
        strategy
            .new_tree(&mut runner)
            .expect("op strategy generation should not fail")
            .current()
    }

    /// Run a shrink-capable property test: for every case `strategy` produces
    /// (including every shrink attempt after a failure), build a **fresh**
    /// `Sim::from_seed(seed)` and invoke `body(&mut sim, &ops)`.
    ///
    /// A panic inside `body` (e.g. an [`always!`](crate::always) violation) is
    /// caught by proptest and treated as a case failure, so an invariant break
    /// shrinks exactly like any other proptest failure. On failure, prints the
    /// `AUTUMN_SIM_SEED=…` replay line alongside the shrunk minimal op
    /// sequence, mirroring [`#[sim_test]`](crate::sim_test)'s panic report.
    ///
    /// `seed` is the **base** seed: it seeds both proptest's own case-
    /// generation stream (via a salted derivation, see the module docs) and
    /// every per-case `Sim`, so the whole run — generation, shrink path, and
    /// each case's simulated behavior — reproduces byte-for-byte from `seed`
    /// alone.
    ///
    /// # Errors
    ///
    /// Returns [`TestError::Fail`] with the shrunk minimal op sequence if any
    /// case (including a shrink attempt) panics or fails; [`TestError::Abort`]
    /// if the run itself could not proceed (e.g. too many rejected cases).
    pub fn run_proptest<T, S, F>(seed: u64, strategy: S, body: F) -> Result<(), TestError<Vec<T>>>
    where
        T: fmt::Debug,
        S: Strategy<Value = Vec<T>>,
        F: Fn(&mut Self, &[T]),
    {
        let result = Self::run_proptest_with_ref(seed, &strategy, body);
        if let Err(ref err) = result
            && let TestError::Fail(ref reason, ref shrunk_ops) = *err
        {
            eprintln!(
                "AUTUMN_SIM_SEED=0x{seed:x} — shrunk to {} op(s): {shrunk_ops:?} ({reason})",
                shrunk_ops.len()
            );
        }
        result
    }

    /// The shared core [`Sim::run_proptest`] delegates to, taking `strategy`
    /// by reference instead of by value.
    ///
    /// `pub(crate)` rather than a second public entrypoint: [`sim::sweep`](super::sweep)
    /// is the only other caller, and it needs to run the **same** strategy and
    /// body across many seeds without requiring either to be `Clone` — a
    /// reference is enough because [`TestRunner::run`] itself only ever
    /// borrows the strategy. Does not print the replay line; callers own their
    /// own failure reporting ([`Sim::run_proptest`] prints its single-seed
    /// line, [`sim::sweep`](super::sweep) reports the seed that stopped the
    /// sweep).
    pub(crate) fn run_proptest_with_ref<T, S, F>(
        seed: u64,
        strategy: &S,
        body: F,
    ) -> Result<(), TestError<Vec<T>>>
    where
        T: fmt::Debug,
        S: Strategy<Value = Vec<T>>,
        F: Fn(&mut Self, &[T]),
    {
        Self::run_proptest_with_case_hook(seed, strategy, body, || {})
    }

    /// As [`Sim::run_proptest_with_ref`], but additionally invokes `on_case`
    /// immediately after every case's `body` returns *successfully* — before
    /// proptest's runner tries the next case, which calls [`Sim::from_seed`]
    /// again and so resets the [`sometimes!`](crate::sometimes) registry.
    ///
    /// [`sim::sweep`](super::sweep) is the only caller: a single seed's
    /// `run_proptest` drives up to [`Config::default`]'s `cases` (256)
    /// candidate op-sequences internally, each starting with a fresh registry
    /// — reading [`sometimes_snapshot`](super::assert::sometimes_snapshot)
    /// only once, *after* the whole seed finishes, would see just the last
    /// case tried and silently drop every earlier case's observations. The
    /// hook lets the sweep fold each case's snapshot before it's overwritten.
    ///
    /// Not invoked for a case whose `body` panics — the panic unwinds past
    /// the hook call. That's fine here: a failing case stops the whole seed
    /// (and the sweep reports the failure itself), so that case's
    /// `sometimes!` observations never needed to feed into the non-vacuity
    /// aggregate in the first place.
    pub(crate) fn run_proptest_with_case_hook<T, S, F, H>(
        seed: u64,
        strategy: &S,
        body: F,
        on_case: H,
    ) -> Result<(), TestError<Vec<T>>>
    where
        T: fmt::Debug,
        S: Strategy<Value = Vec<T>>,
        F: Fn(&mut Self, &[T]),
        H: FnMut(),
    {
        // `TestRunner::run` requires the test closure to be `Fn`, but the hook
        // is `FnMut` (it accumulates into a caller-owned set across calls) —
        // a `RefCell` lets a single `Fn` closure call it repeatedly.
        let on_case = std::cell::RefCell::new(on_case);
        let mut runner =
            TestRunner::new_with_rng(embedded_config(), stream_rng(seed, OP_GEN_STREAM_SALT));
        runner.run(strategy, |ops| {
            let mut sim = Self::from_seed(seed);
            body(&mut sim, &ops);
            (on_case.borrow_mut())();
            Ok(())
        })
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
    fn gen_ops_with_is_deterministic_for_the_same_seed() {
        let mut a = Sim::from_seed(42);
        let mut b = Sim::from_seed(42);
        let ops_a = a.gen_ops_with(tiny_op_strategy());
        let ops_b = b.gen_ops_with(tiny_op_strategy());
        assert_eq!(ops_a, ops_b);
    }

    #[test]
    fn gen_ops_with_diverges_across_seeds() {
        let mut a = Sim::from_seed(1);
        let mut b = Sim::from_seed(2);
        let ops_a = a.gen_ops_with(tiny_op_strategy());
        let ops_b = b.gen_ops_with(tiny_op_strategy());
        assert_ne!(
            ops_a, ops_b,
            "different seeds should overwhelmingly diverge"
        );
    }

    #[test]
    fn gen_ops_with_is_independent_of_seeded_entropy_stream() {
        // Drawing from the app-facing RNG before `gen_ops_with` must not perturb
        // the op-generation stream — they're salted independently.
        let mut sim = Sim::from_seed(7);
        let baseline = sim.gen_ops_with(tiny_op_strategy());

        let mut sim2 = Sim::from_seed(7);
        let _ = sim2.rng().next_u64();
        let after_rng_use = sim2.gen_ops_with(tiny_op_strategy());

        assert_eq!(baseline, after_rng_use);
    }

    #[test]
    fn embedded_config_forces_fork_and_timeout_off() {
        // `Config::default()` folds in whatever `PROPTEST_FORK`/`PROPTEST_TIMEOUT`
        // env vars happen to be set process-wide (proptest's `contextualize_config`,
        // cached process-lifetime via a `LazyLock` — not something a per-test env var
        // override could exercise reliably here). Both embedded runners have no
        // `test_name` to hand proptest's forking machinery, so inheriting fork mode
        // would panic with "Must supply test_name when forking enabled" before a
        // single op could be generated. Assert the override directly instead.
        let config = embedded_config();
        assert!(
            !config.fork,
            "the embedded op-driver runner must never fork — it has no test_name"
        );
        assert_eq!(
            config.timeout, 0,
            "the embedded op-driver runner has no fork timeout to honor"
        );
    }

    #[test]
    fn embedded_config_pins_the_case_count_ignoring_proptest_cases() {
        // `PROPTEST_CASES=0` in the environment would otherwise make
        // `Config::default().cases` zero, so `TestRunner::run` returns
        // `Ok(())` without ever invoking the case closure — `body` (and any
        // `always!`/`sometimes!` inside it) never runs, yet `run_proptest`
        // still reports success. A bare `.max(1)` clamp would avoid that but
        // also collapse proptest's automatic shrink budget
        // (`cases.saturating_mul(4)`) to 4 iterations — assert the exact
        // pinned value instead, same reasoning as
        // `embedded_config_forces_fork_and_timeout_off` above (the env var
        // is read once, process-lifetime, via a `LazyLock`).
        assert_eq!(
            embedded_config().cases,
            EMBEDDED_CASES,
            "the embedded op-driver runner must always run exactly EMBEDDED_CASES cases"
        );
    }

    #[test]
    fn run_proptest_rebuilds_a_fresh_sim_per_case() {
        let seeds_seen = std::sync::Mutex::new(Vec::new());
        let result = Sim::run_proptest(0, tiny_op_strategy(), |sim, _ops| {
            seeds_seen.lock().unwrap().push(sim.seed);
        });
        assert!(result.is_ok());
        let seen = seeds_seen.into_inner().unwrap();
        assert!(!seen.is_empty());
        assert!(
            seen.iter().all(|&s| s == 0),
            "every case must see the same base seed"
        );
    }

    #[test]
    fn run_proptest_shrinks_a_failing_sequence_to_a_minimal_reproduction() {
        // Fails whenever three or more `Inc`s appear back to back — proptest
        // should shrink an arbitrary failing sequence down to exactly that.
        let result = Sim::run_proptest(123, tiny_op_strategy(), |_sim, ops| {
            let mut run = 0;
            for op in ops {
                run = if *op == TinyOp::Inc { run + 1 } else { 0 };
                assert!(run < 3, "three Incs in a row");
            }
        });
        let err = result.expect_err("the seeded sweep must find a failing sequence");
        match err {
            TestError::Fail(_, shrunk_ops) => {
                assert_eq!(
                    shrunk_ops,
                    vec![TinyOp::Inc, TinyOp::Inc, TinyOp::Inc],
                    "shrinking should reduce to the minimal 3-Inc counterexample, got {shrunk_ops:?}"
                );
            }
            TestError::Abort(reason) => panic!("expected a shrunk failure, got an abort: {reason}"),
        }
    }
}
