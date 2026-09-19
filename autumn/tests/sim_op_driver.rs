//! W6 PR2 Definition-of-Done proof (sim-testing, issue #1797): the op-driver
//! finds an injected invariant break, shrinks it to a minimal counterexample,
//! and the shrunk counterexample replays deterministically.
//!
//! Deliberately does **not** touch `Sim::build`/`TestApp` — that's proven
//! end-to-end by the other `sim_*` `DoD` tests (`sim_chaos_crash`,
//! `sim_sqlite_integration`, …); this file's only job is the op-driver
//! mechanism itself: `Sim::gen_ops_with`/`Sim::run_proptest` against a small,
//! self-contained domain model (an account balance, mirroring the RFC's own
//! `Deposit`/`Transfer` worked example) with a real, intentional bug.
//!
//! Isolated `[[test]]` binary because it needs the `sim-testing` feature
//! (`proptest` as a library dependency, not just a dev-dependency — see
//! `autumn/Cargo.toml`), which the default-feature consolidated
//! `integration_tests` binary does not enable. The file is
//! `#![cfg(feature = "sim-testing")]`, so a default `cargo test` compiles it to
//! an empty (passing) binary. Run it with:
//! `cargo test -p autumn-web --features sim-testing --test sim_op_driver`.

#![cfg(feature = "sim-testing")]

use autumn_web::always;
use autumn_web::sim::Sim;
use proptest::prelude::*;
use proptest::test_runner::TestError;

/// A toy account op, mirroring the RFC's own `Deposit`/`Transfer` example
/// closely enough to be representative without needing a real app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Deposit(u32),
    Withdraw(u32),
}

impl Arbitrary for Op {
    type Parameters = ();
    type Strategy = BoxedStrategy<Op>;

    fn arbitrary_with((): ()) -> Self::Strategy {
        prop_oneof![
            (1u32..100).prop_map(Op::Deposit),
            (1u32..100).prop_map(Op::Withdraw),
        ]
        .boxed()
    }
}

/// Applies `ops` to a starting-from-zero balance and asserts the invariant
/// "balance never goes negative" via `always!`.
///
/// **Deliberately buggy**: `Withdraw` subtracts unconditionally, with no floor
/// check against the current balance — exactly the class of bug an op-driver
/// sweep exists to catch. A single `Withdraw` from a zero balance already
/// violates the invariant, so this is the smallest possible counterexample the
/// shrinker can converge on.
fn apply_ops(ops: &[Op]) {
    let mut balance: i64 = 0;
    for op in ops {
        match *op {
            Op::Deposit(amount) => balance += i64::from(amount),
            Op::Withdraw(amount) => balance -= i64::from(amount),
        }
        always!(balance >= 0, "balance went negative: {balance}");
    }
}

#[test]
fn run_proptest_finds_and_shrinks_the_injected_invariant_break() {
    let result = Sim::run_proptest(
        0,
        proptest::collection::vec(any::<Op>(), 1..32),
        |_sim, ops| {
            apply_ops(ops);
        },
    );

    let err = result.expect_err("a Withdraw from a zero balance must violate the invariant");
    let TestError::Fail(_, shrunk_ops) = err else {
        panic!("expected a shrunk failure, got an abort");
    };
    assert_eq!(
        shrunk_ops,
        vec![Op::Withdraw(1)],
        "should shrink to the minimal single-withdraw counterexample, got {shrunk_ops:?}"
    );
}

#[test]
fn the_shrunk_counterexample_replays_deterministically() {
    // Same base seed twice: the op-driver's own determinism contract (proptest's
    // case-generation stream is seeded from `seed`, see `sim::op`'s module docs)
    // means both runs must find and shrink to the identical minimal failure.
    let run = || {
        let result = Sim::run_proptest(
            0,
            proptest::collection::vec(any::<Op>(), 1..32),
            |_sim, ops| {
                apply_ops(ops);
            },
        );
        match result.expect_err("must fail") {
            TestError::Fail(_, shrunk_ops) => shrunk_ops,
            TestError::Abort(reason) => panic!("expected a shrunk failure, got an abort: {reason}"),
        }
    };

    assert_eq!(
        run(),
        run(),
        "the same base seed must replay the same shrunk counterexample"
    );
}
