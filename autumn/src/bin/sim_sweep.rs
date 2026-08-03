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

/// This binary's own replay suggestion for a failing `seed` — appended after
/// `SweepFailure`'s (caller-agnostic) `Display` output, since only this
/// binary knows it's the one being invoked. The count is decimal, matching
/// `seed_count`'s decimal-only parser: printing it as hex here would
/// silently fail to parse there and fall back to the default seed count
/// instead of covering the failing seed.
fn replay_command(seed: u64) -> String {
    format!(
        "  replay: AUTUMN_SIM_SEEDS={} cargo run -p autumn-web --release --features sim-testing --bin sim-sweep",
        seed.wrapping_add(1),
    )
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
            eprintln!("{}", replay_command(failure.seed));
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
        SweepOutcome::Empty => {
            // `count` was 0 (or somehow otherwise produced an empty range) —
            // fail loudly rather than let a misconfigured AUTUMN_SIM_SEEDS
            // silently green this CI job without testing anything.
            eprintln!(
                "sim-sweep: EMPTY — AUTUMN_SIM_SEEDS={count} swept zero seeds; nothing was tested"
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_command_emits_a_seed_count_that_covers_the_failing_seed_and_parses_as_plain_decimal()
    {
        let command = replay_command(300);
        let count_str = command
            .split("AUTUMN_SIM_SEEDS=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("replay command must carry a seed count");
        // Mirrors `seed_count`'s own decimal-only parser — this is the exact
        // failure mode the P2 review comment caught: a hex count here would
        // silently fail this same parse and fall back to `DEFAULT_SEED_COUNT`.
        let count: u64 = count_str.parse().unwrap_or_else(|err| {
            panic!("replay count {count_str:?} must parse as plain decimal u64: {err}")
        });
        assert!(
            count > 300,
            "replay count {count} must exceed the failing seed 300 so re-sweeping 0..{count} reaches it"
        );
    }
}
