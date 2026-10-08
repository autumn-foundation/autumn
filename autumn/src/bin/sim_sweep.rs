//! `sim-sweep`: the CI-facing seed sweep (issues #1797, #3067).
//!
//! It sweeps a batch of seeds, one after the other, through each selected
//! scenario:
//!
//! - `account`: the correct account demo in `autumn_web::sim::scenario`,
//!   through [`autumn_web::sim::sweep::sweep_proptest`]. It proves the sweep
//!   itself scales to many seeds with no false positive. The `sim_sweep_driver`
//!   `DoD` test proves it catches a real break, with the buggy variant.
//! - `jobs`, `scheduler`, `lock` (build with `--features sqlite`): the
//!   framework's own coordination, two or three replicas on one database under
//!   seeded faults, through `autumn_web::sim::fleet::sweep`. The first seeds
//!   also run twice, and their traces must match.
//!
//! Structured like the `loom` CI job: its own bounded CI step
//! (`.github/workflows/ci.yml`), not part of the normal `cargo test` run.
//!
//! # Usage
//!
//! ```text
//! AUTUMN_SIM_SEEDS=1000 cargo run -p autumn-web --release --features "sim-testing,sqlite" --bin sim-sweep
//! ```
//!
//! - `AUTUMN_SIM_SEEDS`: how many seeds (default 256).
//! - `AUTUMN_SIM_SEED_START`: the first seed (default 0). Both are decimal.
//! - `AUTUMN_SIM_SCENARIOS`: a comma list of scenarios (default: all that this
//!   build has).
//! - `AUTUMN_SIM_TRACE_CHECKS`: how many of the first seeds run twice for the
//!   trace check (default 16).
//!
//! It exits `0` when every seed passes and the sweep is non-vacuous. It exits
//! `1` and prints the failing scenario, seed and replay command, a trace
//! difference, or the `sometimes!` labels no seed reached.

use autumn_web::sim::scenario::{apply_ops, ops_strategy};
use autumn_web::sim::sweep::{SweepOutcome, sweep_proptest};

const DEFAULT_SEED_COUNT: u64 = 256;

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

/// This binary's own replay command for a failing `scenario` and `seed`. It
/// sweeps exactly that seed and scenario. Both values are decimal, because
/// `seed_range` parses decimal only: a hex value would fall back to the
/// defaults and sweep `0..256` instead.
fn replay_command(scenario: &str, seed: u64) -> String {
    format!(
        "  replay: AUTUMN_SIM_SCENARIOS={scenario} AUTUMN_SIM_SEED_START={seed} AUTUMN_SIM_SEEDS=1 \
         cargo run -p autumn-web --release --features \"sim-testing,sqlite\" --bin sim-sweep",
    )
}

/// The scenarios this build can run.
fn available_scenarios() -> Vec<&'static str> {
    let mut scenarios = vec!["account"];
    #[cfg(feature = "sqlite")]
    scenarios.extend_from_slice(autumn_web::sim::fleet::SCENARIOS);
    scenarios
}

/// The scenarios to run, from the raw `AUTUMN_SIM_SCENARIOS` value. Unset or
/// blank selects every available one.
fn select_scenarios(
    raw: Option<&str>,
    available: &[&'static str],
) -> Result<Vec<&'static str>, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(available.to_vec());
    };
    raw.split(',')
        .map(str::trim)
        .map(|name| {
            available
                .iter()
                .find(|known| **known == name)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "unknown scenario `{name}`; this build has {available:?} \
                         (the fleet scenarios need --features sqlite)"
                    )
                })
        })
        .collect()
}

/// A count from a raw environment value, or `default`.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
fn count(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

fn main() {
    let seeds = seed_range(
        std::env::var("AUTUMN_SIM_SEED_START").ok().as_deref(),
        std::env::var("AUTUMN_SIM_SEEDS").ok().as_deref(),
    );
    let scenarios = match select_scenarios(
        std::env::var("AUTUMN_SIM_SCENARIOS").ok().as_deref(),
        &available_scenarios(),
    ) {
        Ok(scenarios) => scenarios,
        Err(error) => {
            eprintln!("sim-sweep: {error}");
            std::process::exit(1);
        }
    };
    if seeds.is_empty() {
        // Fail loudly rather than let a misconfigured AUTUMN_SIM_SEEDS green
        // this CI job without testing anything.
        eprintln!("sim-sweep: EMPTY — AUTUMN_SIM_SEEDS swept zero seeds; nothing was tested");
        std::process::exit(1);
    }
    if scenarios.contains(&"account") {
        sweep_account(seeds.clone());
    }
    #[cfg(feature = "sqlite")]
    {
        let fleet: Vec<&str> = scenarios
            .iter()
            .copied()
            .filter(|scenario| *scenario != "account")
            .collect();
        if !fleet.is_empty() {
            let checks = count(std::env::var("AUTUMN_SIM_TRACE_CHECKS").ok().as_deref(), 16);
            sweep_fleet(seeds, &fleet, checks);
        }
    }
}

/// Sweep the account demo scenario, or exit `1`.
fn sweep_account(seeds: std::ops::Range<u64>) {
    let count = seeds.end - seeds.start;
    let strategy = ops_strategy();
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
            eprintln!("{}", replay_command("account", failure.seed));
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
            eprintln!("sim-sweep: EMPTY — the account sweep ran zero seeds; nothing was tested");
            std::process::exit(1);
        }
    }
}

/// Sweep the framework scenarios, or exit `1`.
#[cfg(feature = "sqlite")]
fn sweep_fleet(seeds: std::ops::Range<u64>, scenarios: &[&str], checks: u64) {
    use autumn_web::sim::fleet::{FleetOutcome, sweep};

    println!(
        "sim-sweep: sweeping {} seed(s) ({}..{}) against {scenarios:?}; the first {checks} \
         seed(s) run twice for the trace check",
        seeds.end - seeds.start,
        seeds.start,
        seeds.end,
    );
    match sweep(seeds, scenarios, checks) {
        FleetOutcome::Passed { runs } => {
            println!("sim-sweep: PASSED — {runs} framework run(s), non-vacuous");
        }
        FleetOutcome::Failed {
            scenario,
            seed,
            reason,
        } => {
            eprintln!("sim-sweep: FAILED — scenario `{scenario}`, seed {seed}");
            eprintln!("{reason}");
            eprintln!("{}", replay_command(&scenario, seed));
            std::process::exit(1);
        }
        FleetOutcome::Nondeterministic {
            scenario,
            seed,
            diff,
        } => {
            eprintln!(
                "sim-sweep: NONDETERMINISTIC — scenario `{scenario}`, seed {seed} logged a \
                 different trace on its second run"
            );
            eprintln!("{diff}");
            eprintln!("{}", replay_command(&scenario, seed));
            std::process::exit(1);
        }
        FleetOutcome::Vacuous { runs, unsatisfied } => {
            eprintln!(
                "sim-sweep: VACUOUS — {runs} framework run(s) passed, but no run reached: {}",
                unsatisfied.into_iter().collect::<Vec<_>>().join(", ")
            );
            std::process::exit(1);
        }
        other => {
            eprintln!("sim-sweep: unexpected outcome {other:?}");
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
        let command = replay_command("account", 300);
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
        let command = replay_command("jobs", seed);
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
    fn replay_command_names_the_scenario() {
        let command = replay_command("lock", 9);
        assert_eq!(env_value(&command, "AUTUMN_SIM_SCENARIOS"), "lock");
        assert!(command.contains("sqlite"), "{command}");
    }

    #[test]
    fn scenarios_default_to_all_and_reject_unknown_names() {
        let available = ["account", "jobs"];
        assert_eq!(select_scenarios(None, &available).unwrap(), available);
        assert_eq!(select_scenarios(Some(" "), &available).unwrap(), available);
        assert_eq!(
            select_scenarios(Some("jobs, account"), &available).unwrap(),
            ["jobs", "account"]
        );
        let error = select_scenarios(Some("jobs,nope"), &available).unwrap_err();
        assert!(
            error.contains("nope") && error.contains("sqlite"),
            "{error}"
        );
    }

    #[test]
    fn counts_fall_back_to_the_default() {
        assert_eq!(count(None, 16), 16);
        assert_eq!(count(Some("x"), 16), 16);
        assert_eq!(count(Some(" 3 "), 16), 3);
    }

    #[test]
    fn seed_range_starts_where_asked() {
        assert_eq!(seed_range(Some("40"), Some("8")), 40..48);
    }
}
