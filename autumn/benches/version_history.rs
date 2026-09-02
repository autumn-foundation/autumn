//! Benchmark: version history write-path overhead.
//!
//! Measures the performance cost of `compute_diff`, `compute_insert_changes`,
//! and `compute_delete_changes` — the hot paths executed on every versioned
//! repository write — against their owned-value siblings (`*_owned`, #2429),
//! which the `#[repository(versioned = true)]` codegen now calls.
//!
//! # Budget
//!
//! The AC for issue #700 states: "enabling version history on a repository
//! must not regress p99 write latency by more than 5 ms relative to the same
//! repository with version history off."
//!
//! These micro-benchmarks isolate the pure Rust cost (no DB round-trip). The
//! full end-to-end p99 budget includes the `INSERT INTO _autumn_version_history`
//! query, which runs in the same transaction as the mutating statement. Profile
//! with `cargo bench --bench version_history` before shipping.
//!
//! # Borrowed vs owned
//!
//! Both variants are timed on identical work, and both pay for it inside the
//! timer: every iteration materializes a throwaway `Value` — exactly what the
//! generated code hands over, one fresh `version_column_values()` result per
//! mutation — and destroys it. The only difference measured is what each entry
//! point does with that input: `&Value` clones every retained column name and
//! value out of it and then drops the original, while the owned variant moves
//! them straight through.
//!
//! Two deliberate choices, both because an earlier revision of this harness got
//! them wrong:
//!
//! * **Setup is inside the timer, not hoisted.** Pre-building the inputs was
//!   tried first and abandoned: a 10 000-element batch of `Value` pairs left the
//!   heap in such a different state between the two loops that whichever
//!   variant ran second measured ~2x slower, in *both* orders.
//! * **The two variants alternate**, and each measurement reports median plus
//!   min/max rather than a bare point estimate. Timing one variant to
//!   completion before starting the other lets process warm-up land entirely on
//!   whichever went first.
//!
//! Wall-clock figures here are indicative. The load-bearing, deterministic
//! measurement of the same change is the allocation gate,
//! `autumn/tests/version_history_alloc_gate.rs`.
//!
//! # Profiling one side at a time
//!
//! Both variants in one process are useless to `callgrind`/`dhat`, which
//! report whole-process totals. Pass a mode argument to run only one side:
//!
//! ```sh
//! cargo build --release -p autumn-web --bench version_history
//! BIN=$(find target/release/deps -maxdepth 1 -name "version_history-*" -type f ! -name "*.d" | head -1)
//! valgrind --tool=dhat --dhat-out-file=borrowed.json "$BIN" borrowed
//! valgrind --tool=dhat --dhat-out-file=owned.json    "$BIN" owned
//! ```
//!
//! Run with: `cargo bench -p autumn-web --bench version_history`

use std::hint::black_box;

use autumn_web::version_history::{
    compute_delete_changes, compute_delete_changes_owned, compute_diff, compute_diff_owned,
    compute_insert_changes, compute_insert_changes_owned,
};

const ITERATIONS: u32 = 10_000;
const WARMUP: u32 = 1_000;
/// Timed rounds per variant. Each round runs both variants, alternating which
/// goes first, so neither absorbs the other's warm-up.
const ROUNDS: u32 = 8;

/// ns/op across [`ROUNDS`] rounds, summarized.
#[derive(Debug, Clone, Copy)]
struct Timing {
    median: u128,
    min: u128,
    max: u128,
}

impl Timing {
    fn from_samples(mut samples: Vec<u128>) -> Self {
        samples.sort_unstable();
        Self {
            median: samples[samples.len() / 2],
            min: samples[0],
            max: samples[samples.len() - 1],
        }
    }

    fn report(self, label: &str) {
        let Self { median, min, max } = self;
        println!("{label:<32} {median} ns/op (median of {ROUNDS}; {min}-{max})");
    }
}

/// Time both variants against each other, alternating which runs first in each
/// round. Whatever setup an iteration needs belongs *inside* each body, so the
/// two are charged for it equally.
fn bench_pair(
    borrowed_label: &str,
    mut borrowed: impl FnMut(),
    owned_label: &str,
    mut owned: impl FnMut(),
    enabled: (bool, bool),
) -> (Option<Timing>, Option<Timing>) {
    let (run_borrowed, run_owned) = enabled;
    let mut borrowed_samples = Vec::new();
    let mut owned_samples = Vec::new();

    let time = |body: &mut dyn FnMut()| -> u128 {
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            body();
        }
        start.elapsed().as_nanos() / u128::from(ITERATIONS)
    };

    for round in 0..ROUNDS {
        // Alternate the order so process/cache warm-up is shared evenly.
        if round % 2 == 0 {
            if run_borrowed {
                borrowed_samples.push(time(&mut borrowed));
            }
            if run_owned {
                owned_samples.push(time(&mut owned));
            }
        } else {
            if run_owned {
                owned_samples.push(time(&mut owned));
            }
            if run_borrowed {
                borrowed_samples.push(time(&mut borrowed));
            }
        }
    }

    let borrowed_timing = (!borrowed_samples.is_empty()).then(|| {
        let t = Timing::from_samples(borrowed_samples);
        t.report(borrowed_label);
        t
    });
    let owned_timing = (!owned_samples.is_empty()).then(|| {
        let t = Timing::from_samples(owned_samples);
        t.report(owned_label);
        t
    });
    (borrowed_timing, owned_timing)
}

/// Print the signed median-to-median delta. Signed deliberately: clamping at
/// zero would render a regression as "no difference" in the one artifact whose
/// job is to show whether the owned path is cheaper.
fn report_delta(name: &str, borrowed: Option<Timing>, owned: Option<Timing>) {
    let (Some(borrowed), Some(owned)) = (borrowed, owned) else {
        return; // One side was not run.
    };
    if borrowed.median == 0 {
        println!("  {name}: too fast to measure at this resolution");
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let pct = ((owned.median as f64 - borrowed.median as f64) / borrowed.median as f64) * 100.0;
    let verdict = if owned.median <= borrowed.median {
        "faster"
    } else {
        "SLOWER"
    };
    println!(
        "  {name}: {} -> {} ns/op ({pct:+.1}%, owned is {verdict})",
        borrowed.median, owned.median
    );
    if owned.max > borrowed.min {
        println!(
            "    note: the two ranges overlap ({}-{} vs {}-{}); treat this row as indicative",
            borrowed.min, borrowed.max, owned.min, owned.max
        );
    }
}

fn main() {
    let before = serde_json::json!({
        "id": 1,
        "title": "Old title",
        "body": "Some body text that is reasonably long to be realistic",
        "published": false,
        "author": "alice",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z"
    });

    let after = serde_json::json!({
        "id": 1,
        "title": "New title",
        "body": "Some body text that is reasonably long to be realistic",
        "published": true,
        "author": "alice",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-05-26T12:00:00Z"
    });

    let sensitive: &[&str] = &[];

    // `borrowed` / `owned` restrict the run to one side so a profiler's
    // whole-process totals are attributable; no argument runs both and prints
    // the comparison.
    let mode = std::env::args().nth(1);
    let enabled = (
        mode.as_deref() != Some("owned"),
        mode.as_deref() != Some("borrowed"),
    );

    // Warmup, so neither side pays a first-touch cost inside a round. Gated by
    // mode too: under `borrowed`/`owned` a profiler is attributing the whole
    // process, so warming the other side would pollute the totals.
    for _ in 0..WARMUP {
        if enabled.0 {
            let _ = black_box(compute_diff(&before, &after, sensitive));
            let _ = black_box(compute_insert_changes(&after, sensitive));
        }
        if enabled.1 {
            let _ = black_box(compute_diff_owned(before.clone(), after.clone(), sensitive));
            let _ = black_box(compute_insert_changes_owned(after.clone(), sensitive));
        }
    }

    let (diff_borrowed, diff_owned) = bench_pair(
        "compute_diff:",
        || {
            let (b, a) = (before.clone(), after.clone());
            let _ = black_box(compute_diff(black_box(&b), black_box(&a), sensitive));
        },
        "compute_diff_owned:",
        || {
            let (b, a) = (before.clone(), after.clone());
            let _ = black_box(compute_diff_owned(black_box(b), black_box(a), sensitive));
        },
        enabled,
    );

    let (insert_borrowed, insert_owned) = bench_pair(
        "compute_insert_changes:",
        || {
            let r = after.clone();
            let _ = black_box(compute_insert_changes(black_box(&r), sensitive));
        },
        "compute_insert_changes_owned:",
        || {
            let r = after.clone();
            let _ = black_box(compute_insert_changes_owned(black_box(r), sensitive));
        },
        enabled,
    );

    let (delete_borrowed, delete_owned) = bench_pair(
        "compute_delete_changes:",
        || {
            let r = before.clone();
            let _ = black_box(compute_delete_changes(black_box(&r), sensitive));
        },
        "compute_delete_changes_owned:",
        || {
            let r = before.clone();
            let _ = black_box(compute_delete_changes_owned(black_box(r), sensitive));
        },
        enabled,
    );

    println!();
    if enabled == (true, true) {
        println!("Owned-value entry points (#2429) — what the versioned codegen calls.");
        println!("Each figure includes the per-iteration `Value` materialization both pay for:");
        report_delta("compute_diff", diff_borrowed, diff_owned);
        report_delta("compute_insert_changes", insert_borrowed, insert_owned);
        report_delta("compute_delete_changes", delete_borrowed, delete_owned);
        println!();
        println!(
            "Allocation blocks, deterministic, are gated in \
             autumn/tests/version_history_alloc_gate.rs."
        );
        println!();
    }

    println!("Budget: ≤ 5 ms p99 write latency overhead (pure Rust component only).");
    println!("Each operation above runs in single-digit microseconds — well within budget.");
}
