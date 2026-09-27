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
//! `AUTUMN_SIM_SEEDS` is the number of seeds to sweep; defaults to 256 if unset
//! or unparseable. `AUTUMN_SIM_SEED_START` is the first seed; defaults to `0`.
//! Both are decimal. A failing sweep prints a replay command that sets both, so
//! it reruns only the failing seed. Exits `0`
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

/// The seeds to sweep, from the raw `AUTUMN_SIM_SEED_START` and
/// `AUTUMN_SIM_SEEDS` values. An unset or unparseable value takes its default
/// (start `0`, count 256). The range saturates at `u64::MAX`.
fn seed_range(start: Option<&str>, count: Option<&str>) -> std::ops::Range<u64> {
    let start = start
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let count = count
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_SEED_COUNT);
    start..start.saturating_add(count)
}

/// This binary's own replay suggestion for a failing `seed`, printed after
/// `SweepFailure`'s caller-agnostic `Display` output, since only this binary
/// knows it is the one being invoked. It sweeps exactly the failing seed. Both
/// values are decimal, because `seed_range` parses decimal only: a hex value
/// would fall back to the defaults and sweep `0..256` instead.
fn replay_command(seed: u64) -> String {
    format!(
        "  replay: AUTUMN_SIM_SEED_START={seed} AUTUMN_SIM_SEEDS=1 cargo run -p autumn-web --release --features sim-testing --bin sim-sweep",
    )
}

fn main() {
    let seeds = seed_range(
        std::env::var("AUTUMN_SIM_SEED_START").ok().as_deref(),
        std::env::var("AUTUMN_SIM_SEEDS").ok().as_deref(),
    );
    let count = seeds.end - seeds.start;
    let strategy = proptest::collection::vec(any::<Op>(), 1..32);
    println!(
        "sim-sweep: sweeping {count} seed(s) ({}..{}) against the account demo scenario",
        seeds.start, seeds.end,
    );

    match sweep_proptest(seeds, &strategy, |_sim, ops| apply_ops(ops)) {
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

    /// Read one `KEY=value` from a replay command.
    fn env_value<'a>(command: &'a str, key: &str) -> &'a str {
        command
            .split_whitespace()
            .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("replay command must set {key}: {command}"))
    }

    #[test]
    fn replay_command_sweeps_exactly_the_failing_seed() {
        let command = replay_command(300);
        let seeds = seed_range(
            Some(env_value(&command, "AUTUMN_SIM_SEED_START")),
            Some(env_value(&command, "AUTUMN_SIM_SEEDS")),
        );
        assert_eq!(seeds, 300..301, "{command}");
    }

    #[test]
    fn replay_command_reaches_the_largest_sweepable_seed() {
        // A range's end is exclusive, so `u64::MAX - 1` is the last seed a
        // sweep can run.
        let seed = u64::MAX - 1;
        let command = replay_command(seed);
        let seeds = seed_range(
            Some(env_value(&command, "AUTUMN_SIM_SEED_START")),
            Some(env_value(&command, "AUTUMN_SIM_SEEDS")),
        );
        assert_eq!(seeds, seed..u64::MAX, "{command}");
    }

    #[test]
    fn seed_range_defaults_to_the_first_256_seeds() {
        assert_eq!(seed_range(None, None), 0..DEFAULT_SEED_COUNT);
        assert_eq!(
            seed_range(Some("0x10"), Some("many")),
            0..DEFAULT_SEED_COUNT,
            "hex and garbage fall back to the defaults",
        );
    }

    #[test]
    fn seed_range_starts_where_asked() {
        assert_eq!(seed_range(Some("40"), Some("8")), 40..48);
    }
}
