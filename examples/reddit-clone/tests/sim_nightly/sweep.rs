//! The seed sweep driver of the nightly.
//!
//! It runs the seeds of one window one after the other, in one process. The
//! framework keeps the job runtime and the event bus per process, so two seeds
//! cannot run at the same time in one process. The nightly workflow gets parallel
//! work from many processes, each with its own shard of the window.

use std::collections::BTreeSet;
use std::ops::Range;
use std::time::Instant;

use autumn_web::sim::{reset_sometimes_registry, sometimes_snapshot};

use super::scenario::{self, Variant};

/// The seed count when `AUTUMN_SIM_SEEDS` is not set. It is small, so the
/// sweep also runs on each PR in a debug build.
pub const DEFAULT_SEEDS: u64 = 8;

/// The trace check count when `AUTUMN_SIM_TRACE_CHECKS` is not set.
pub const DEFAULT_TRACE_CHECKS: u64 = 2;

/// A shard with fewer seeds skips the `sometimes!` check. One seed cannot
/// make every label true, so a one-seed replay must report its own result.
/// The 32-seed test `sim_nightly_window_is_not_vacuous` keeps the check on PRs.
pub const MIN_SEEDS_FOR_REACH: u64 = 32;

/// The framework keeps the job runtime and the event bus per process, so two
/// sims in one process disturb each other. Each scenario test holds this lock.
/// CI also runs the `sim_` tests with `--test-threads=1`.
static ONE_SIM_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the lock of [`ONE_SIM_AT_A_TIME`].
pub fn one_sim_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_SIM_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The memory one sweep process can gain after its first seeds. A seed
/// frees what it uses, so more growth is a leak. Before this check, a leak of
/// 0.5 MiB per seed filled the runner and the runner stopped with no report.
pub const MAX_GROWTH_KB: u64 = 256 * 1024;

/// The memory check runs after this many seeds, and again after each next block.
const MEMORY_CHECK_EVERY: u64 = 1_000;

/// The resident memory of this process in KiB. `None` off Linux.
pub fn rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()
}

/// The failure reason if memory grew from `baseline` to `now` (KiB) by more
/// than [`MAX_GROWTH_KB`] over `seeds` seeds.
pub fn leak(baseline: u64, now: u64, seeds: u64) -> Option<String> {
    let growth = now.saturating_sub(baseline);
    (growth > MAX_GROWTH_KB).then(|| {
        format!(
            "memory leak: the process grew from {} MiB to {} MiB over {seeds} seed(s). \
             A seed must free what it uses; find what keeps the apps of old seeds alive.",
            baseline / 1024,
            now / 1024,
        )
    })
}

/// What one sweep process runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepConfig {
    /// The full window, before the shard split.
    pub window: Range<u64>,
    /// This process runs part `shard.0` of `shard.1`.
    pub shard: (u64, u64),
    /// How many of the first seeds of this shard run twice.
    pub trace_checks: u64,
    /// Where to write the JSON report, if set.
    pub report: Option<String>,
}

impl SweepConfig {
    /// A config for `window`, one shard, no report.
    pub fn window(window: Range<u64>) -> Self {
        Self {
            window,
            shard: (0, 1),
            trace_checks: DEFAULT_TRACE_CHECKS,
            report: None,
        }
    }

    /// The config from the `AUTUMN_SIM_*` environment variables.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self::parse(
            var("AUTUMN_SIM_SEED_START").as_deref(),
            var("AUTUMN_SIM_SEEDS").as_deref(),
            var("AUTUMN_SIM_SHARD").as_deref(),
            var("AUTUMN_SIM_TRACE_CHECKS").as_deref(),
            var("AUTUMN_SIM_REPORT"),
        )
    }

    /// The config from raw values. A value that does not parse takes its
    /// default. A bad shard is a panic, because a wrong shard skips seeds.
    pub fn parse(
        start: Option<&str>,
        count: Option<&str>,
        shard: Option<&str>,
        trace_checks: Option<&str>,
        report: Option<String>,
    ) -> Self {
        let number = |raw: Option<&str>, default: u64| {
            raw.and_then(|value| value.trim().parse().ok())
                .unwrap_or(default)
        };
        let start = number(start, 0);
        let count = number(count, DEFAULT_SEEDS);
        Self {
            window: start..start.saturating_add(count),
            shard: shard.map_or((0, 1), parse_shard),
            trace_checks: number(trace_checks, DEFAULT_TRACE_CHECKS),
            report: report.filter(|path| !path.trim().is_empty()),
        }
    }

    /// The seeds this process runs.
    pub fn seeds(&self) -> Range<u64> {
        shard_range(self.window.clone(), self.shard.0, self.shard.1)
    }
}

/// Parse `k/n`. Panics on a bad value.
fn parse_shard(raw: &str) -> (u64, u64) {
    let parsed = raw
        .split_once('/')
        .and_then(|(index, total)| Some((index.trim().parse().ok()?, total.trim().parse().ok()?)));
    match parsed {
        Some((index, total)) if total > 0 && index < total => (index, total),
        _ => panic!("AUTUMN_SIM_SHARD must be `k/n` with k < n, got `{raw}`"),
    }
}

/// Part `index` of `total` of `window`. The parts are contiguous, have no
/// gaps and no overlap, and their sizes differ by one seed at most.
pub fn shard_range(window: Range<u64>, index: u64, total: u64) -> Range<u64> {
    assert!(total > 0 && index < total, "bad shard {index}/{total}");
    let len = window.end.saturating_sub(window.start);
    let base = len / total;
    let extra = len % total;
    let start = window.start + index * base + index.min(extra);
    let size = base + u64::from(index < extra);
    start..start + size
}

/// The command that runs only `seed`. It uses decimal values, because the
/// parser reads decimal only.
pub fn replay_command(seed: u64) -> String {
    format!(
        "AUTUMN_SIM_SEED_START={seed} AUTUMN_SIM_SEEDS=1 cargo test -p reddit-clone --release \
         --test sim_nightly -- --nocapture sim_nightly_sweep"
    )
}

/// If the seed passes alone, the earlier seeds of the process can be the cause.
/// This command runs them too, in the same order.
fn replay_in_context(start: u64, seed: u64) -> String {
    format!(
        "replay with the earlier seeds of this process: AUTUMN_SIM_SEED_START={start} \
         AUTUMN_SIM_SEEDS={} cargo test -p reddit-clone --release --test sim_nightly \
         -- --nocapture sim_nightly_sweep",
        seed - start + 1
    )
}

/// The result of one sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Each seed passed, each trace check matched, and each `sometimes!`
    /// label that a seed saw was true in a seed.
    Passed {
        seeds: u64,
        trace_checks: u64,
        millis: u128,
    },
    /// `seed` failed. The sweep stopped there.
    Failed {
        seeds: u64,
        seed: u64,
        reason: String,
    },
    /// `seed` gave a different trace on its second run.
    Nondeterministic { seeds: u64, seed: u64, diff: String },
    /// No seed made these labels true.
    Vacuous {
        seeds: u64,
        unsatisfied: BTreeSet<String>,
    },
    /// The window has no seeds. That is a failure: nothing ran.
    Empty,
}

/// Run the sweep.
pub fn run(config: &SweepConfig, variant: Variant) -> Outcome {
    let seeds = config.seeds();
    if seeds.is_empty() {
        return Outcome::Empty;
    }
    let started = Instant::now();
    let mut observed = BTreeSet::new();
    let mut satisfied = BTreeSet::new();
    let mut ran = 0;
    let mut baseline = None;
    for seed in seeds.clone() {
        ran += 1;
        // A log line now and then shows how far a process got, if it is killed.
        if ran % 10_000 == 0 {
            println!("reddit-clone DST nightly: {ran} seed(s) done, at seed {seed}");
        }
        let first = scenario::run(seed, variant);
        let (seen, true_once) = sometimes_snapshot();
        observed.extend(seen);
        satisfied.extend(true_once);
        reset_sometimes_registry();
        let trace = match first {
            Ok(trace) => trace,
            Err(reason) => {
                return Outcome::Failed {
                    seeds: ran,
                    seed,
                    reason,
                };
            }
        };
        if seed - seeds.start < config.trace_checks {
            let second = scenario::run(seed, variant);
            reset_sometimes_registry();
            match second {
                Ok(again) => {
                    if let Some(diff) = trace.diff(&again) {
                        return Outcome::Nondeterministic {
                            seeds: ran,
                            seed,
                            diff: diff.to_string(),
                        };
                    }
                }
                Err(reason) => {
                    return Outcome::Nondeterministic {
                        seeds: ran,
                        seed,
                        diff: format!("the first run passed, the second run failed: {reason}"),
                    };
                }
            }
        }
        // Sample after the seed ran, so the report names a seed that ran.
        // The first block warms the caches, so it sets the baseline.
        if ran % MEMORY_CHECK_EVERY == 0
            && let Some(now) = rss_kb()
        {
            let since = *baseline.get_or_insert((ran, now));
            if let Some(reason) = leak(since.1, now, ran - since.0) {
                return Outcome::Failed {
                    seeds: ran,
                    seed,
                    reason,
                };
            }
        }
    }
    let unsatisfied: BTreeSet<String> = observed.difference(&satisfied).cloned().collect();
    if ran >= MIN_SEEDS_FOR_REACH && !unsatisfied.is_empty() {
        return Outcome::Vacuous {
            seeds: ran,
            unsatisfied,
        };
    }
    Outcome::Passed {
        seeds: ran,
        trace_checks: config.trace_checks.min(ran),
        millis: started.elapsed().as_millis(),
    }
}

/// A human report of `outcome`.
pub fn render(config: &SweepConfig, outcome: &Outcome) -> String {
    let seeds = config.seeds();
    let head = format!(
        "reddit-clone DST nightly: window {}..{}, shard {}/{} = seeds {}..{}",
        config.window.start,
        config.window.end,
        config.shard.0,
        config.shard.1,
        seeds.start,
        seeds.end,
    );
    let start = seeds.start;
    let body = match outcome {
        Outcome::Passed {
            seeds,
            trace_checks,
            millis,
        } => {
            #[allow(clippy::cast_precision_loss)] // A rate for people to read.
            let rate = *seeds as f64 / (*millis).max(1) as f64 * 1000.0;
            let reach = if *seeds >= MIN_SEEDS_FOR_REACH {
                "not vacuous"
            } else {
                "vacuity not checked (fewer than 32 seeds)"
            };
            format!(
                "PASSED: {seeds} seed(s), {trace_checks} trace check(s), {millis} ms, \
                 {rate:.1} seeds/s, {reach}"
            )
        }
        Outcome::Failed {
            seeds,
            seed,
            reason,
        } => format!(
            "FAILED at seed {seed} after {seeds} seed(s)\n{reason}\nreplay: {}\n{}",
            replay_command(*seed),
            replay_in_context(start, *seed),
        ),
        Outcome::Nondeterministic { seeds, seed, diff } => format!(
            "NONDETERMINISTIC: seed {seed} (after {seeds} seed(s)) gave two traces\n{diff}\n\
             replay: {}\n{}",
            replay_command(*seed),
            replay_in_context(start, *seed),
        ),
        Outcome::Vacuous { seeds, unsatisfied } => format!(
            "VACUOUS: {seeds} seed(s) passed, but no seed made these labels true: {}",
            unsatisfied.iter().cloned().collect::<Vec<_>>().join(", ")
        ),
        Outcome::Empty => "EMPTY: the window has no seeds, so nothing ran".to_owned(),
    };
    format!("{head}\n{body}")
}

/// Print the report, and write it as JSON to `config.report` if set.
pub fn write_report(config: &SweepConfig, outcome: &Outcome) {
    println!("{}", render(config, outcome));
    let Some(path) = &config.report else {
        return;
    };
    let seeds = config.seeds();
    let (status, ran, failing_seed, millis) = match outcome {
        Outcome::Passed { seeds, millis, .. } => ("passed", *seeds, None, Some(*millis)),
        Outcome::Failed { seeds, seed, .. } => ("failed", *seeds, Some(*seed), None),
        Outcome::Nondeterministic { seeds, seed, .. } => {
            ("nondeterministic", *seeds, Some(*seed), None)
        }
        Outcome::Vacuous { seeds, .. } => ("vacuous", *seeds, None, None),
        Outcome::Empty => ("empty", 0, None, None),
    };
    let report = serde_json::json!({
        "status": status,
        "start": seeds.start,
        "end": seeds.end,
        "seeds_run": ran,
        "millis": millis.map(|m| m.to_string()),
        "failing_seed": failing_seed,
        "replay": failing_seed.map(replay_command),
        "summary": render(config, outcome),
        "unsatisfied": match outcome {
            Outcome::Vacuous { unsatisfied, .. } => unsatisfied.iter().cloned().collect(),
            _ => Vec::new(),
        },
    });
    std::fs::write(path, report.to_string())
        .unwrap_or_else(|error| panic!("write the sweep report to {path}: {error}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shards_cover_the_window_with_no_gap_and_no_overlap() {
        for (len, total) in [(0, 1), (1, 3), (10, 3), (4_000_000, 64), (7, 7), (5, 8)] {
            let window = 100..100 + len;
            let mut next = window.start;
            for index in 0..total {
                let part = shard_range(window.clone(), index, total);
                assert_eq!(part.start, next, "gap or overlap at {index}/{total}");
                assert!(part.end - part.start <= len / total + 1);
                next = part.end;
            }
            assert_eq!(next, window.end, "the shards must end at the window end");
        }
    }

    #[test]
    fn replay_command_runs_only_the_failing_seed() {
        let command = replay_command(4_000_123);
        let value = |key: &str| {
            command
                .split_whitespace()
                .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
                .map(str::to_owned)
        };
        let config = SweepConfig::parse(
            value("AUTUMN_SIM_SEED_START").as_deref(),
            value("AUTUMN_SIM_SEEDS").as_deref(),
            None,
            None,
            None,
        );
        assert_eq!(config.seeds(), 4_000_123..4_000_124, "{command}");
        assert!(command.contains("--release"), "{command}");
    }

    #[test]
    fn config_defaults_and_bad_numbers_fall_back() {
        let config = SweepConfig::parse(None, None, None, None, Some(" ".to_owned()));
        assert_eq!(config.window, 0..DEFAULT_SEEDS);
        assert_eq!(config.shard, (0, 1));
        assert_eq!(config.trace_checks, DEFAULT_TRACE_CHECKS);
        assert_eq!(config.report, None);
        let config = SweepConfig::parse(Some("0x10"), Some("many"), Some("2/4"), None, None);
        assert_eq!(config.window, 0..DEFAULT_SEEDS);
        assert_eq!(config.seeds(), 4..6);
    }

    #[test]
    #[should_panic(expected = "AUTUMN_SIM_SHARD")]
    fn a_bad_shard_is_a_panic_not_a_silent_skip() {
        let _ = SweepConfig::parse(None, None, Some("4/4"), None, None);
    }

    #[test]
    fn a_one_seed_replay_reports_its_own_result() {
        let _serial = one_sim_at_a_time();
        let outcome = run(&SweepConfig::window(3..4), Variant::Correct);
        assert!(
            matches!(outcome, Outcome::Passed { seeds: 1, .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn memory_growth_past_the_limit_is_a_leak() {
        assert_eq!(leak(100_000, 100_000 + MAX_GROWTH_KB, 5_000), None);
        let reason = leak(100_000, 100_001 + MAX_GROWTH_KB, 5_000).expect("a leak");
        assert!(reason.contains("5000 seed(s)"), "{reason}");
        assert!(reason.contains("MiB"), "{reason}");
        assert_eq!(
            leak(100_000, 50_000, 5_000),
            None,
            "memory that shrinks is not a leak"
        );
    }

    #[test]
    fn this_process_reports_its_memory_on_linux() {
        if cfg!(target_os = "linux") {
            assert!(rss_kb().is_some_and(|kb| kb > 0));
        }
    }

    #[test]
    fn an_empty_window_is_not_a_pass() {
        let config = SweepConfig::parse(Some("5"), Some("0"), None, None, None);
        assert_eq!(run(&config, Variant::Correct), Outcome::Empty);
    }
}
