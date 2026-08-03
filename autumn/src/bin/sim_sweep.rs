//! `sim-sweep`: the CI-facing driver for [`autumn_web::sim::sweep::sweep_proptest`]
//! (sim-testing W6 PR3, issue #1797).
//!
//! Sweeps a batch of seeds, sequentially, against a small, self-contained,
//! deliberately **correct** account demo scenario (mirroring
//! `tests/sim_op_driver.rs`'s worked example, but with the `Withdraw`
//! floor-check bug fixed) — proving the seed-sweep mechanism itself scales to
//! many seeds without false positives. It is a smoke check for the harness,
//! not a real app-level property; the `sim_sweep_driver` `DoD` test proves the
//! mechanism catches a *genuine* invariant break, using the intentionally
//! buggy variant of this same model.
//!
//! Structured like the `loom` CI job: its own bounded CI step
//! (`.github/workflows/ci.yml`), not part of the normal `cargo test` run.
//!
//! # Usage
//!
//! ```text
//! AUTUMN_SIM_SEEDS=1000 cargo run -p autumn-web --release --features sim-testing --bin sim-sweep
//! ```
//!
//! `AUTUMN_SIM_SEEDS` is the number of seeds to sweep, starting at `0`
//! (`0..AUTUMN_SIM_SEEDS`); defaults to 256 if unset or unparseable. Exits `0`
//! if every seed passes and the sweep is non-vacuous (see
//! [`autumn_web::sim::sweep`]'s module docs); exits `1` and prints either the
//! first failing seed's shrunk op-sequence plus a replay command, or the
//! unsatisfied `sometimes!` labels, on a failing or vacuous sweep.

use autumn_web::sim::sweep::{SweepOutcome, sweep_proptest};
use autumn_web::{always, sometimes};
use proptest::prelude::*;

const DEFAULT_SEED_COUNT: u64 = 256;

/// The demo scenario this binary sweeps: a toy account op, mirroring
/// `tests/sim_op_driver.rs`'s `Deposit`/`Withdraw` worked example.
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

/// Applies `ops` to a starting-from-zero balance and asserts the invariant
/// "balance never goes negative" via `always!`.
///
/// Unlike `tests/sim_op_driver.rs`'s deliberately buggy twin, `Withdraw` here
/// floors at zero (`amount.min(balance)`) instead of subtracting
/// unconditionally — this binary's own sweep is expected to stay green.
fn apply_ops(ops: &[Op]) {
    let mut balance: i64 = 0;
    for op in ops {
        match *op {
            Op::Deposit(amount) => balance += i64::from(amount),
            Op::Withdraw(amount) => balance -= i64::from(amount).min(balance),
        }
        always!(balance >= 0, "balance went negative: {balance}");
        sometimes!(balance == 0, "balance-returned-to-zero");
    }
}

fn seed_count() -> u64 {
    std::env::var("AUTUMN_SIM_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SEED_COUNT)
}

fn main() {
    let count = seed_count();
    let strategy = proptest::collection::vec(any::<Op>(), 1..32);
    println!("sim-sweep: sweeping {count} seed(s) (0..{count}) against the account demo scenario");

    match sweep_proptest(0..count, &strategy, |_sim, ops| apply_ops(ops)) {
        SweepOutcome::Passed { seeds_run } => {
            println!("sim-sweep: PASSED — {seeds_run} seed(s), non-vacuous");
        }
        SweepOutcome::Failed { seeds_run, failure } => {
            eprintln!("sim-sweep: FAILED after {seeds_run} seed(s)");
            eprintln!("{failure}");
            std::process::exit(1);
        }
        SweepOutcome::Vacuous {
            seeds_run,
            unsatisfied,
        } => {
            eprintln!(
                "sim-sweep: VACUOUS — {seeds_run} seed(s) all passed, but sometimes! label(s) \
                 were observed and never satisfied across the whole sweep: {}",
                unsatisfied.into_iter().collect::<Vec<_>>().join(", ")
            );
            std::process::exit(1);
        }
    }
}
