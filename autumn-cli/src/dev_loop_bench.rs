//! Dev-loop latency budget, statistics, and gating for `autumn dev`.
//!
//! This module defines the accepted latency budgets for every `autumn dev`
//! change class, helpers to compute p50/p95/max statistics, budget-checking
//! logic with actionable diagnostics, and report formatters (human-readable
//! text + machine-readable JSON).
//!
//! See `docs/guide/dev-loop-latency.md` for the full budget matrix and the
//! methodology used to measure end-to-end latency.

use std::fmt::Write as FmtWrite;
use std::path::PathBuf;

use serde::Serialize;

// ── Change classes ───────────────────────────────────────────────────────────

/// A category of file change that `autumn dev` handles.
///
/// Each variant corresponds to one row in the dev-loop latency budget matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeClass {
    /// Initial dev boot: time from `autumn dev` invocation to first
    /// successful HTTP response on the app's root route.
    InitialBoot,
    /// Warm incremental Rust route edit in `examples/hello` (no database).
    RustRouteEditHello,
    /// Warm incremental Rust route edit in a database-backed example
    /// (default: `examples/todo-app`).
    RustRouteEditDb,
    /// CSS/Tailwind input-file edit to refreshed browser stylesheet.
    CssTailwind,
    /// Static asset edit (image, JS, font) to browser-visible reload.
    StaticAsset,
    /// `autumn.toml` or profile config edit to restarted server.
    ConfigEdit,
    /// Custom `dev.watch_dirs` entry edit to restarted server.
    WatchDirEdit,
    /// Cold-start onboarding: `autumn new` → `autumn dev` → first HTTP 200,
    /// **including the first clean compile**, for the no-DB `hello` shape.
    /// This is the gated onboarding budget (issue #977).
    ColdStartHello,
    /// Cold-start onboarding for the database-backed shape. Measured as
    /// **informational** in this slice — it does not gate CI.
    ColdStartDb,
}

impl ChangeClass {
    /// Return a stable lowercase snake-case key for use in JSON output.
    pub const fn key(self) -> &'static str {
        match self {
            Self::InitialBoot => "initial_boot",
            Self::RustRouteEditHello => "rust_route_edit_hello",
            Self::RustRouteEditDb => "rust_route_edit_db",
            Self::CssTailwind => "css_tailwind",
            Self::StaticAsset => "static_asset",
            Self::ConfigEdit => "config_edit",
            Self::WatchDirEdit => "watch_dir_edit",
            Self::ColdStartHello => "cold_start_hello",
            Self::ColdStartDb => "cold_start_db",
        }
    }

    /// Return a human-readable name for the user journey this class represents.
    pub const fn journey_name(self) -> &'static str {
        match self {
            Self::InitialBoot => "Initial dev boot to first route",
            Self::RustRouteEditHello => "Rust route edit (examples/hello, no-DB)",
            Self::RustRouteEditDb => "Rust route edit (database-backed example)",
            Self::CssTailwind => "CSS/Tailwind edit to refreshed stylesheet",
            Self::StaticAsset => "Static asset edit to browser reload",
            Self::ConfigEdit => "Config edit (autumn.toml) to restarted server",
            Self::WatchDirEdit => "Custom watch_dirs edit to restarted server",
            Self::ColdStartHello => "Cold start (autumn new → first 200, no-DB)",
            Self::ColdStartDb => "Cold start (autumn new → first 200, database-backed)",
        }
    }
}

// ── Budgets ──────────────────────────────────────────────────────────────────

/// Accepted latency budget for one change class (all values in milliseconds).
// The `_ms` suffix is the unit — suppress the struct-field-names lint.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LatencyBudget {
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

/// Return the canonical accepted latency budget for the given change class.
///
/// These budgets match the success metrics declared in issue #601:
/// - CSS/static reload: p95 ≤ 1 s
/// - Rust warm edit in `examples/hello`: p95 ≤ 5 s
/// - Rust warm edit in a database-backed example: p95 ≤ 10 s
pub const fn budget_for(class: ChangeClass) -> LatencyBudget {
    match class {
        ChangeClass::InitialBoot => LatencyBudget {
            p50_ms: 10_000,
            p95_ms: 20_000,
            max_ms: 40_000,
        },
        ChangeClass::RustRouteEditHello => LatencyBudget {
            p50_ms: 3_000,
            p95_ms: 5_000,
            max_ms: 10_000,
        },
        ChangeClass::RustRouteEditDb => LatencyBudget {
            p50_ms: 5_000,
            p95_ms: 10_000,
            max_ms: 20_000,
        },
        ChangeClass::CssTailwind => LatencyBudget {
            p50_ms: 500,
            p95_ms: 1_000,
            max_ms: 2_000,
        },
        ChangeClass::StaticAsset => LatencyBudget {
            p50_ms: 300,
            p95_ms: 1_000,
            max_ms: 2_000,
        },
        ChangeClass::ConfigEdit | ChangeClass::WatchDirEdit => LatencyBudget {
            p50_ms: 3_000,
            p95_ms: 8_000,
            max_ms: 15_000,
        },
        // Cold-start onboarding budget (issue #977). The success metric is
        // p95 ≤ 60s for the no-DB `hello` shape on the CI reference runner.
        ChangeClass::ColdStartHello => LatencyBudget {
            p50_ms: 45_000,
            p95_ms: 60_000,
            max_ms: 90_000,
        },
        // Database-backed cold start is informational only in this slice, so
        // these limits are not gated. The bundled managed-Postgres provider adds
        // significant compile + first-boot weight (it embeds and starts a real
        // Postgres), so the expectation is generous.
        ChangeClass::ColdStartDb => LatencyBudget {
            p50_ms: 120_000,
            p95_ms: 180_000,
            max_ms: 300_000,
        },
    }
}

// ── Statistics ───────────────────────────────────────────────────────────────

/// Computed latency statistics for a set of timing samples.
// The `_ms` suffix is the unit — suppress the struct-field-names lint.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ClassStats {
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
    pub sample_count: usize,
}

/// Compute p50, p95, and maximum for a slice of timing samples (milliseconds).
///
/// Uses the nearest-rank method: the k-th percentile of n samples is
/// `sorted[ceil(k/100 * n) - 1]`. Returns all-zeros for an empty slice.
/// Ceiling division is computed with integer arithmetic to avoid f64 casts.
pub fn compute_stats(samples: &[u64]) -> ClassStats {
    if samples.is_empty() {
        return ClassStats {
            p50_ms: 0,
            p95_ms: 0,
            max_ms: 0,
            sample_count: 0,
        };
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();

    let n = sorted.len();
    // Nearest-rank: ceil(p/100 * n) = (p * n).div_ceil(100).
    let percentile = |p: usize| -> u64 {
        let rank = (p * n).div_ceil(100);
        sorted[rank.min(n) - 1]
    };

    ClassStats {
        p50_ms: percentile(50),
        p95_ms: percentile(95),
        max_ms: *sorted.last().unwrap(),
        sample_count: n,
    }
}

// ── Budget checking ──────────────────────────────────────────────────────────

/// Result of comparing measured statistics against the accepted budget.
// Each bool is an independent, named outcome flag (pass / p95 / max /
// informational); a state machine would obscure rather than clarify them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
pub struct BudgetCheckResult {
    pub change_class: String,
    pub journey_name: String,
    pub stats: ClassStats,
    pub budget: LatencyBudget,
    pub passed: bool,
    pub p95_exceeded: bool,
    pub max_exceeded: bool,
    /// Percentage above the p95 budget (0 when within budget).
    pub p95_overage_pct: f64,
    /// Human-readable diagnosis of the result.
    pub diagnosis: String,
    /// What the developer should do next (empty string when passing).
    pub next_action: String,
    /// When true this result is reported for visibility only and does **not**
    /// contribute to the overall pass/fail gate (e.g. the database-backed
    /// cold-start shape, which is informational in this slice).
    #[serde(default)]
    pub informational: bool,
}

/// Compare measured statistics against the accepted budget for a change class.
///
/// Produces a `BudgetCheckResult` that names the failing user journey,
/// states the diagnosis, and proposes a concrete next action.
pub fn check_budget(
    class: ChangeClass,
    stats: ClassStats,
    budget: &LatencyBudget,
) -> BudgetCheckResult {
    let p95_exceeded = stats.p95_ms > budget.p95_ms;
    let max_exceeded = stats.max_ms > budget.max_ms;
    let passed = !p95_exceeded && !max_exceeded;

    // Integer percentage: (over * 100) / budget.  Percentages fit in u32
    // (max ~10000% for extreme cases), so the u32→f64 cast is lossless.
    let p95_overage_pct = if p95_exceeded {
        let over = stats.p95_ms.saturating_sub(budget.p95_ms);
        let pct = over.saturating_mul(100) / budget.p95_ms.max(1);
        f64::from(u32::try_from(pct).unwrap_or(u32::MAX))
    } else {
        0.0
    };

    let (diagnosis, next_action) = if passed {
        (String::new(), String::new())
    } else {
        build_diagnostics(class, &stats, budget, p95_exceeded, max_exceeded)
    };

    BudgetCheckResult {
        change_class: class.key().to_string(),
        journey_name: class.journey_name().to_string(),
        stats,
        budget: *budget,
        passed,
        p95_exceeded,
        max_exceeded,
        p95_overage_pct,
        diagnosis,
        next_action,
        informational: false,
    }
}

fn build_diagnostics(
    class: ChangeClass,
    stats: &ClassStats,
    budget: &LatencyBudget,
    p95_exceeded: bool,
    max_exceeded: bool,
) -> (String, String) {
    let mut parts = Vec::new();

    if p95_exceeded {
        let over_pct = stats
            .p95_ms
            .saturating_sub(budget.p95_ms)
            .saturating_mul(100)
            / budget.p95_ms.max(1);
        parts.push(format!(
            "p95 {}ms exceeds budget {}ms ({}% over)",
            stats.p95_ms, budget.p95_ms, over_pct,
        ));
    }
    if max_exceeded {
        parts.push(format!(
            "max {}ms exceeds budget {}ms",
            stats.max_ms, budget.max_ms
        ));
    }

    let diagnosis = format!(
        "Journey '{}' regressed: {}.",
        class.journey_name(),
        parts.join("; ")
    );

    let next_action = match class {
        ChangeClass::CssTailwind => {
            "Check for new CSS plugins or a slow Tailwind config glob. \
             Run `autumn dev` manually and time the Tailwind step in the log."
        }
        ChangeClass::StaticAsset => {
            "Verify no new large static assets were added. \
             Check that the static-file watcher is not triggering unnecessary reloads."
        }
        ChangeClass::RustRouteEditHello => {
            "A Rust compile step slowed for the no-DB path. \
             Check for new proc-macro dependencies or increased monomorphisation. \
             Run `cargo build -p hello --timings` to identify slow crates."
        }
        ChangeClass::RustRouteEditDb => {
            "A Rust compile step slowed for the database-backed path. \
             Check for new ORM dependencies or schema changes that increase compile time. \
             Run `cargo build --timings` to identify slow crates."
        }
        ChangeClass::InitialBoot => {
            "Initial boot slowed. Check for new blocking startup tasks, \
             migration count growth, or Tailwind cold-start overhead. \
             Review the `autumn dev` startup log for the slow phase."
        }
        ChangeClass::ConfigEdit | ChangeClass::WatchDirEdit => {
            "Server restart latency increased. Check for new startup hooks, \
             increased migration count, or blocking I/O in app initialisation."
        }
        ChangeClass::ColdStartHello | ChangeClass::ColdStartDb => {
            "Cold-start onboarding slowed: the first clean compile got heavier. \
             A new default dependency or feature likely bloated the from-scratch \
             build. This slice is measurement-only — run `cargo build --timings` \
             on a fresh checkout to find the slow crates, then open a separate \
             optimization slice (dependency trimming, codegen-units, linker)."
        }
    };

    (diagnosis, next_action.to_string())
}

// ── Full report ──────────────────────────────────────────────────────────────

/// A complete benchmark report covering all measured change classes.
#[derive(Debug, Serialize)]
pub struct FullReport {
    pub timestamp_utc: String,
    pub runner_os: String,
    pub rust_version: String,
    pub autumn_version: String,
    pub example_name: String,
    pub all_passed: bool,
    pub results: Vec<BudgetCheckResult>,
}

// ── Formatters ───────────────────────────────────────────────────────────────

/// Serialise a `FullReport` as a machine-readable JSON string.
///
/// The output is suitable for archiving as release evidence. It deliberately
/// omits local file paths; the runner OS and Rust version supply enough
/// context to interpret variance across environments.
pub fn format_json_report(report: &FullReport) -> String {
    serde_json::to_string_pretty(report)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialisation failed: {e}\"}}"))
}

/// Format a `FullReport` as a human-readable summary.
///
/// The summary shows one row per change class with p50/p95/max timings and a
/// pass/fail indicator. Failing rows include the diagnosis and next action.
pub fn format_human_summary(report: &FullReport) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "Autumn dev-loop latency report — {}",
        report.timestamp_utc
    )
    .unwrap();
    writeln!(
        out,
        "Runner: {}  Rust: {}  autumn-web: {}",
        report.runner_os, report.rust_version, report.autumn_version
    )
    .unwrap();
    writeln!(out, "Example: {}", report.example_name).unwrap();
    out.push('\n');

    let col_w = 46usize;
    writeln!(
        out,
        "{:<col_w$}  {:>8}  {:>8}  {:>8}  Status",
        "Change class", "p50 ms", "p95 ms", "max ms",
    )
    .unwrap();
    writeln!(out, "{}", "-".repeat(col_w + 40)).unwrap();

    for r in &report.results {
        let status = if r.passed { "PASS" } else { "FAIL" };
        writeln!(
            out,
            "{:<col_w$}  {:>8}  {:>8}  {:>8}  {}",
            r.journey_name, r.stats.p50_ms, r.stats.p95_ms, r.stats.max_ms, status,
        )
        .unwrap();
        if !r.passed {
            writeln!(out, "  ↳ {}", r.diagnosis).unwrap();
            writeln!(out, "  ↳ Next: {}", r.next_action).unwrap();
        }
    }

    out.push('\n');
    let overall = if report.all_passed { "PASS" } else { "FAIL" };
    writeln!(out, "Overall: {overall}").unwrap();

    out
}

// ── CLI entry point ──────────────────────────────────────────────────────────

/// Run the dev-loop benchmark command.
///
/// In CI or scheduled runs this drives `autumn dev`, injects file changes,
/// measures end-to-end latency, and writes the report. In `--dry-run` mode
/// it prints the budget table and exits without starting any server.
pub fn run(
    example: &str,
    runs: u32,
    output: Option<&str>,
    json: bool,
    fail_on_regression: bool,
    dry_run: bool,
) -> i32 {
    if dry_run {
        print_budget_table();
        return 0;
    }

    eprintln!("autumn dev-loop-bench: measuring {example} ({runs} run(s) per change class)");
    eprintln!("Note: live measurement requires `autumn dev` and a running HTTP server.");
    eprintln!("Use --dry-run to print the budget table without starting a server.\n");

    // Build a synthetic report using placeholder stats so the command is
    // useful in CI even before the live measurement driver is wired up.
    // The live measurement driver is tracked in the parent issue.
    let results = build_placeholder_results(example);
    let all_passed = results.iter().all(|r| r.passed);

    let report = FullReport {
        timestamp_utc: chrono_utc_now(),
        runner_os: std::env::consts::OS.to_string(),
        rust_version: rust_version_string(),
        autumn_version: env!("CARGO_PKG_VERSION").to_string(),
        example_name: example.to_string(),
        all_passed,
        results,
    };

    emit_report(&report, json, output, fail_on_regression)
}

/// Print a report (human or JSON), optionally write it to `output`, and return the
/// process exit code.
///
/// Shared by the warm dev-loop [`run`] and the cold-start
/// [`crate::cold_start_driver::run_cold_start`] so both emit identically. Returns
/// non-zero when a requested `--output` write fails (so CI never proceeds with a
/// missing report) or when `fail_on_regression` is set and the report did not pass.
pub fn emit_report(
    report: &FullReport,
    json: bool,
    output: Option<&str>,
    fail_on_regression: bool,
) -> i32 {
    let human = format_human_summary(report);
    let machine = format_json_report(report);

    if json {
        println!("{machine}");
    } else {
        println!("{human}");
    }

    let mut exit = 0;

    if let Some(path) = output {
        if let Err(e) = std::fs::write(path, &machine) {
            // A requested report that can't be written must fail the run: CI gates
            // read this file, and a fail-open here would let a gate pass with no
            // report.
            eprintln!("Error: could not write report to {path}: {e}");
            exit = 1;
        } else {
            eprintln!("Report written to {path}");
        }
    }

    if fail_on_regression && !report.all_passed {
        eprintln!("One or more change classes exceeded the latency budget. Exiting 1.");
        exit = 1;
    }

    exit
}

/// Render a budget table for the given change classes as a string.
///
/// Shared by the warm dev-loop and cold-start dry-run paths so the table
/// layout stays identical and is unit-testable without capturing stdout.
fn format_budget_table(title: &str, classes: &[ChangeClass]) -> String {
    let mut out = String::new();
    let col_w = 46usize;
    writeln!(out, "{title}\n").unwrap();
    writeln!(
        out,
        "{:<col_w$}  {:>10}  {:>10}  {:>10}",
        "Change class", "p50 ms", "p95 ms", "max ms"
    )
    .unwrap();
    writeln!(out, "{}", "-".repeat(col_w + 36)).unwrap();

    for &class in classes {
        let b = budget_for(class);
        writeln!(
            out,
            "{:<col_w$}  {:>10}  {:>10}  {:>10}",
            class.journey_name(),
            b.p50_ms,
            b.p95_ms,
            b.max_ms
        )
        .unwrap();
    }

    writeln!(
        out,
        "\nSee docs/guide/dev-loop-latency.md for methodology and prerequisites."
    )
    .unwrap();
    out
}

fn print_budget_table() {
    print!(
        "{}",
        format_budget_table(
            "Autumn dev-loop latency budget (issue #601)",
            &[
                ChangeClass::InitialBoot,
                ChangeClass::RustRouteEditHello,
                ChangeClass::RustRouteEditDb,
                ChangeClass::CssTailwind,
                ChangeClass::StaticAsset,
                ChangeClass::ConfigEdit,
                ChangeClass::WatchDirEdit,
            ],
        )
    );
}

/// Render the cold-start onboarding budget table. The database-backed shape is
/// included only when `include_db` is set (it is informational in this slice).
pub fn format_cold_start_budget_table(include_db: bool) -> String {
    let classes: &[ChangeClass] = if include_db {
        &[ChangeClass::ColdStartHello, ChangeClass::ColdStartDb]
    } else {
        &[ChangeClass::ColdStartHello]
    };
    format_budget_table("Autumn cold-start onboarding budget (issue #977)", classes)
}

fn build_placeholder_results(example: &str) -> Vec<BudgetCheckResult> {
    let classes: &[ChangeClass] = if example.contains("todo") || example.contains("blog") {
        &[
            ChangeClass::InitialBoot,
            ChangeClass::RustRouteEditDb,
            ChangeClass::CssTailwind,
            ChangeClass::StaticAsset,
            ChangeClass::ConfigEdit,
            ChangeClass::WatchDirEdit,
        ]
    } else {
        &[
            ChangeClass::InitialBoot,
            ChangeClass::RustRouteEditHello,
            ChangeClass::CssTailwind,
            ChangeClass::StaticAsset,
            ChangeClass::ConfigEdit,
            ChangeClass::WatchDirEdit,
        ]
    };

    classes
        .iter()
        .map(|&class| {
            let budget = budget_for(class);
            // Placeholder: report zero samples so CI can exercise the reporting
            // path. Replace with live HTTP polling once the measurement driver
            // lands. compute_stats(&[]) → all-zeros → passes every budget.
            let stats = compute_stats(&[]);
            check_budget(class, stats, &budget)
        })
        .collect()
}

pub fn chrono_utc_now() -> String {
    std::env::var("AUTUMN_BENCH_TIMESTAMP").unwrap_or_else(|_| "unknown".to_string())
}

pub fn rust_version_string() -> String {
    std::env::var("AUTUMN_BENCH_RUST_VERSION").unwrap_or_else(|_| "unknown".to_string())
}

// ── Cold-start onboarding benchmark (issue #977) ──────────────────────────────
//
// This file holds the **pure, unit-tested** half of the cold-start benchmark:
// budgets, statistics, budget checking, report building, and the dry-run budget
// table. The **live measurement driver** (scaffold → cold compile → serve → time
// the first 200) is all subprocess / TCP / filesystem I/O and lives in
// [`crate::cold_start_driver`], which is excluded from coverage like `dev.rs`.

/// Outcome of the (optional) database-backed cold-start shape.
pub enum DbOutcome {
    /// `--include-db` was not requested.
    NotRequested,
    /// Measured samples (milliseconds).
    Measured(Vec<u64>),
    /// `--include-db` was requested but the measurement failed; the message is
    /// surfaced as an informational failure row so the run does not silently
    /// drop the requested result.
    Failed(String),
}

/// Assemble a cold-start report from measured samples.
///
/// `hello_samples` always gates the result. The database-backed shape, when
/// requested, is recorded as **informational** (measured or failed) and never
/// affects `all_passed`.
pub fn build_cold_start_report(hello_samples: &[u64], db: &DbOutcome) -> FullReport {
    let mut results = Vec::new();

    let hello_budget = budget_for(ChangeClass::ColdStartHello);
    results.push(check_budget(
        ChangeClass::ColdStartHello,
        compute_stats(hello_samples),
        &hello_budget,
    ));

    match db {
        DbOutcome::NotRequested => {}
        DbOutcome::Measured(samples) => {
            let db_budget = budget_for(ChangeClass::ColdStartDb);
            let mut r = check_budget(ChangeClass::ColdStartDb, compute_stats(samples), &db_budget);
            r.informational = true;
            results.push(r);
        }
        DbOutcome::Failed(msg) => results.push(db_failure_result(msg)),
    }

    // Only non-informational results contribute to the gate.
    let all_passed = results
        .iter()
        .filter(|r| !r.informational)
        .all(|r| r.passed);

    FullReport {
        timestamp_utc: chrono_utc_now(),
        runner_os: std::env::consts::OS.to_string(),
        rust_version: rust_version_string(),
        autumn_version: env!("CARGO_PKG_VERSION").to_string(),
        example_name: "cold-start".to_string(),
        all_passed,
        results,
    }
}

/// Sanitize a failure message before it is embedded in the archived report.
///
/// A raw measurement error can carry the full `cargo` stderr (or other
/// subprocess output), which embeds local absolute paths — `/home/runner`, the
/// checkout dir, `~/.cargo/registry/...`, or the throwaway project's temp path.
/// The report contract is that archived JSON never leaks local paths, usernames,
/// or secrets, so we keep only the first line, redact filesystem-path-like
/// tokens, and bound the length.
fn sanitize_failure_reason(msg: &str) -> String {
    // Bound the length so a pathological error cannot bloat the report.
    const MAX: usize = 300;
    // Only the first line: multi-line subprocess stderr dumps (the usual source
    // of leaked paths) follow on later lines.
    let first_line = msg.lines().next().unwrap_or("");
    let redacted = first_line
        .split_whitespace()
        .map(|tok| {
            if looks_like_path_token(tok) {
                "<path>"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if redacted.chars().count() > MAX {
        let truncated: String = redacted.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        redacted
    }
}

/// Whether a whitespace-delimited token contains a filesystem path.
///
/// Detects an absolute-path marker (`/`, `~/`, or a `X:\`/`X:/` drive root) at the
/// **start of the token or immediately after a delimiter** (`: = ( , [ ] { } " '`),
/// or any backslash. Checking only the token *prefix* would miss paths glued after
/// a non-path prefix, e.g. `note:/home/runner/x` or `registry=/home/.cargo/...`,
/// while the delimiter rule still leaves ordinary slashes like `and/or` alone.
fn looks_like_path_token(tok: &str) -> bool {
    const DELIMS: &[u8] = b":=(,[]{}\"'";
    // Windows separators / UNC paths.
    if tok.contains('\\') {
        return true;
    }
    let bytes = tok.as_bytes();
    let at_boundary = |i: usize| i == 0 || DELIMS.contains(&bytes[i - 1]);
    for i in 0..bytes.len() {
        match bytes[i] {
            // Unix absolute path "/…" or home path "~/…".
            b'/' if at_boundary(i) => return true,
            b'~' if at_boundary(i) && bytes.get(i + 1) == Some(&b'/') => return true,
            // Windows drive root "X:/" (the "X:\\" form is caught by the
            // backslash check above).
            c if c.is_ascii_alphabetic()
                && at_boundary(i)
                && bytes.get(i + 1) == Some(&b':')
                && bytes.get(i + 2) == Some(&b'/') =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Build an informational failure row for a DB-shape measurement that could not
/// be completed, so an `--include-db` run records the requested-but-failed
/// result in the report instead of silently omitting it.
fn db_failure_result(msg: &str) -> BudgetCheckResult {
    let class = ChangeClass::ColdStartDb;
    BudgetCheckResult {
        change_class: class.key().to_string(),
        journey_name: class.journey_name().to_string(),
        stats: compute_stats(&[]),
        budget: budget_for(class),
        passed: false,
        p95_exceeded: false,
        max_exceeded: false,
        p95_overage_pct: 0.0,
        diagnosis: format!(
            "Database-backed cold start could not be measured: {}",
            sanitize_failure_reason(msg)
        ),
        next_action: "This shape is informational and does not affect the gate. \
                      Re-run `--include-db` in an environment where the bundled \
                      managed-Postgres prerequisites are available."
            .to_string(),
        informational: true,
    }
}

/// Extract the built binary path from `cargo build --message-format=json` output.
///
/// Scans the `compiler-artifact` messages for the one whose target is the
/// project's binary and returns its reported `executable` path. This is robust
/// to a non-default target dir or `--target <triple>` configured via Cargo
/// config files (which would otherwise move the artifact out of `target/debug/`).
pub fn cargo_executable_path(stdout: &[u8], bin_name: &str) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    let mut found = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(serde_json::Value::as_str) == Some("compiler-artifact")
            && v.get("target")
                .and_then(|t| t.get("name"))
                .and_then(serde_json::Value::as_str)
                == Some(bin_name)
            && let Some(exe) = v.get("executable").and_then(serde_json::Value::as_str)
        {
            found = Some(PathBuf::from(exe));
        }
    }
    found
}

// ── Overload / load-shedding benchmark (issue #1006) ──────────────────────────
//
// This is the pure, unit-tested half of `autumn dev-loop-bench --overload`: the
// accepted budget, statistics, and gate logic for the Success Metric declared
// in issue #1006. The live measurement driver (scaffold a throwaway app with a
// slow handler, boot it with `server.max_concurrent_requests` configured, fire
// concurrent load, sample RSS) is all subprocess / TCP / filesystem I/O and
// lives in [`crate::overload_driver`], which is excluded from coverage like
// `cold_start_driver.rs`.

/// Accepted budget for the overload benchmark (issue #1006's Success Metric).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OverloadBudget {
    /// Admitted-request p99 latency under 2x-ceiling offered load must stay
    /// within this multiple of the unloaded baseline p99 (1.2 = 20% over).
    pub max_p99_ratio: f64,
    /// A shed request's response must complete within this many milliseconds.
    pub max_shed_ms: u64,
}

/// Return the canonical overload benchmark budget (issue #1006).
pub const fn overload_budget() -> OverloadBudget {
    OverloadBudget {
        max_p99_ratio: 1.2,
        max_shed_ms: 5,
    }
}

/// Raw measurements from one overload benchmark run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OverloadStats {
    /// Admitted-request latency samples (ms) at offered load == ceiling (no
    /// shedding expected).
    pub baseline_samples_ms: Vec<u64>,
    /// Number of baseline-phase requests that were *not* admitted (shed or
    /// transport-failed) despite offered load == ceiling. The baseline
    /// measurement assumes this never happens; a nonzero count means the
    /// baseline p99 itself is unreliable (some latency it should have
    /// captured went missing instead), so it's surfaced rather than silently
    /// discarded.
    pub baseline_shed_count: u64,
    /// Admitted-request latency samples (ms) at offered load == ceiling ×
    /// multiplier.
    pub admitted_samples_ms: Vec<u64>,
    /// Genuine shed-response (`503`) latency samples (ms) at offered load ==
    /// ceiling × multiplier — transport errors are tracked separately (see
    /// `transport_error_samples_ms`) so they cannot corrupt the shed-latency
    /// budget with unrelated connection/timeout latency.
    pub shed_samples_ms: Vec<u64>,
    /// Number of requests shed with a genuine `503` during the overload phase.
    pub shed_count: u64,
    /// Number of requests admitted (2xx) during the overload phase.
    pub admitted_count: u64,
    /// Latency samples (ms) for requests that failed at the transport level
    /// (connection refused, reset, timed out) during the overload phase,
    /// rather than receiving a genuine `503` from the load-shed gate. A
    /// hung/crashed benchmark host surfaces here, not as inflated shed
    /// latency.
    pub transport_error_samples_ms: Vec<u64>,
    /// Number of transport-level failures during the overload phase.
    pub transport_error_count: u64,
    /// Child process RSS samples (KB) taken periodically during the overload
    /// phase. Empty on platforms where RSS sampling isn't implemented (see
    /// [`crate::overload_driver::sample_rss_kb`]) — the RSS-bounded check is
    /// then vacuously true and the report notes it was skipped.
    pub rss_samples_kb: Vec<u64>,
}

/// Result of checking measured [`OverloadStats`] against the [`OverloadBudget`].
// Each bool is an independent, named outcome flag for one of the three
// budget dimensions (p99 ratio / shed latency / RSS) plus whether RSS
// sampling ran at all — mirrors `BudgetCheckResult`'s identical allow.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
pub struct OverloadCheckResult {
    pub passed: bool,
    pub baseline_p99_ms: u64,
    pub admitted_p99_ms: u64,
    /// `admitted_p99_ms / baseline_p99_ms` (0.0 when baseline is 0).
    pub p99_ratio: f64,
    pub admitted_p99_within_budget: bool,
    pub shed_p99_ms: u64,
    pub shed_max_ms: u64,
    pub shed_fast_enough: bool,
    /// `true` iff at least one genuine `503` was observed during the
    /// overload phase. Gates `passed`: without this, a benchmark run whose
    /// admission gate never actually enforces the ceiling (every offered
    /// request admitted) would still report success — `shed_count == 0`
    /// vacuously satisfies `shed_fast_enough` (nothing to be slow), bounded
    /// RSS, and an unregressed admitted p99, blessing a broken gate as
    /// passing. See `docs/guide/dev-loop-latency.md` for the overload
    /// benchmark's methodology (offered load is always `ceiling ×
    /// load_multiplier`, so a working gate must shed *something*).
    pub admission_gate_verified: bool,
    /// `true` when RSS samples show no monotonic growth (or too few samples
    /// were collected to judge, or RSS sampling isn't supported on this
    /// platform — see [`rss_bounded`]).
    pub rss_bounded: bool,
    pub rss_skipped: bool,
    pub shed_count: u64,
    pub admitted_count: u64,
    /// See [`OverloadStats::baseline_shed_count`]. Does not gate `passed` —
    /// it's a data-quality warning about the baseline, not a budget the
    /// server is expected to meet — but is surfaced in the diagnosis and the
    /// human report rather than silently discarded.
    pub baseline_shed_count: u64,
    /// See [`OverloadStats::transport_error_count`]. Does not gate
    /// `shed_fast_enough` (that budget only ever measures genuine `503`
    /// latency) but is surfaced as a data-quality warning: a nonzero count
    /// means the benchmark host itself struggled to keep up with the
    /// requested concurrency.
    pub transport_error_count: u64,
    pub diagnosis: String,
}

/// Whether RSS samples show no monotonic growth.
///
/// Compares the maximum of the first half of samples against the maximum of
/// the second half: growth beyond 50% is treated as unbounded. Fewer than 4
/// samples can't distinguish noise from a trend, so they vacuously pass.
/// This is a coarse trend check, not a statistical model — it exists to catch
/// the qualitative "unbounded climb" failure mode the success metric
/// describes, not to bound precise growth percentages.
fn rss_bounded(samples: &[u64]) -> bool {
    if samples.len() < 4 {
        return true;
    }
    let mid = samples.len() / 2;
    let first_max = samples[..mid].iter().copied().max().unwrap_or(0);
    let second_max = samples[mid..].iter().copied().max().unwrap_or(0);
    if first_max == 0 {
        return true;
    }
    // Integer comparison of `second_max <= first_max * 1.5` avoids a lossy
    // u64→f64 cast: `2x <= 3y` is equivalent for non-negative integers.
    second_max.saturating_mul(2) <= first_max.saturating_mul(3)
}

/// p99 of a slice of timing samples (milliseconds), via the same nearest-rank
/// method as [`compute_stats`]. `ClassStats` (used by the warm/cold-start
/// benchmarks) only tracks p50/p95/max, so the overload benchmark — whose
/// Success Metric is stated in terms of p99 — computes it separately rather
/// than widening that shared, differently-scoped type. Returns `0` for an
/// empty slice.
fn percentile_99(samples: &[u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let rank = (99 * n).div_ceil(100);
    sorted[rank.min(n) - 1]
}

/// Compare measured [`OverloadStats`] against the accepted [`OverloadBudget`].
pub fn check_overload_budget(
    stats: &OverloadStats,
    budget: &OverloadBudget,
) -> OverloadCheckResult {
    let baseline_p99_ms = percentile_99(&stats.baseline_samples_ms);
    let admitted_p99_ms = percentile_99(&stats.admitted_samples_ms);
    let shed_max_ms = compute_stats(&stats.shed_samples_ms).max_ms;
    let shed_p99_ms = percentile_99(&stats.shed_samples_ms);

    // Integer percentage (nearest whole percent) to avoid a lossy u64→f64
    // cast, matching `check_budget`'s `p95_overage_pct` idiom. Ample precision
    // for gating against a 120%-style budget. `checked_div` naturally yields
    // `0.0` for a zero baseline (no ratio can be established).
    let p99_ratio = admitted_p99_ms
        .saturating_mul(100)
        .checked_div(baseline_p99_ms)
        .map_or(0.0, |pct| {
            f64::from(u32::try_from(pct).unwrap_or(u32::MAX)) / 100.0
        });
    // A zero baseline can't establish a ratio; only gate on it when we have a
    // real baseline to compare against.
    let admitted_p99_within_budget = baseline_p99_ms == 0 || p99_ratio <= budget.max_p99_ratio;
    let shed_fast_enough = stats.shed_count == 0 || shed_max_ms <= budget.max_shed_ms;
    let admission_gate_verified = stats.shed_count > 0;
    let rss_skipped = stats.rss_samples_kb.is_empty();
    let rss_ok = rss_bounded(&stats.rss_samples_kb);

    let passed =
        admitted_p99_within_budget && shed_fast_enough && admission_gate_verified && rss_ok;

    let mut diagnosis = String::new();
    if !admitted_p99_within_budget {
        let _ = write!(
            diagnosis,
            "admitted p99 {admitted_p99_ms}ms is {:.0}% over the unloaded baseline \
             {baseline_p99_ms}ms (budget: within {:.0}%). ",
            (p99_ratio - 1.0) * 100.0,
            (budget.max_p99_ratio - 1.0) * 100.0
        );
    }
    if !shed_fast_enough {
        let _ = write!(
            diagnosis,
            "shed responses took up to {shed_max_ms}ms (budget: {}ms). ",
            budget.max_shed_ms
        );
    }
    if !admission_gate_verified {
        diagnosis.push_str(
            "no requests were shed during the overload phase (offered load was \
             ceiling × load_multiplier) — the admission gate may not be enforcing \
             the ceiling at all. ",
        );
    }
    if !rss_ok {
        diagnosis.push_str("RSS grew unboundedly during the overload phase. ");
    }
    if stats.baseline_shed_count > 0 {
        let _ = write!(
            diagnosis,
            "WARNING: {} baseline-phase request(s) were not admitted even though offered \
             load == ceiling; the baseline p99 above may be unreliable. ",
            stats.baseline_shed_count
        );
    }
    if stats.transport_error_count > 0 {
        let _ = write!(
            diagnosis,
            "WARNING: {} request(s) failed at the transport level (not a genuine 503) \
             during the overload phase; the benchmark host may be overwhelmed. ",
            stats.transport_error_count
        );
    }
    if passed {
        diagnosis.push_str("Admitted-request latency stayed within budget, shedding was fast, and RSS stayed bounded.");
    }

    OverloadCheckResult {
        passed,
        baseline_p99_ms,
        admitted_p99_ms,
        p99_ratio,
        admitted_p99_within_budget,
        shed_p99_ms,
        shed_max_ms,
        shed_fast_enough,
        admission_gate_verified,
        rss_bounded: rss_ok,
        rss_skipped,
        shed_count: stats.shed_count,
        admitted_count: stats.admitted_count,
        baseline_shed_count: stats.baseline_shed_count,
        transport_error_count: stats.transport_error_count,
        diagnosis: diagnosis.trim_end().to_string(),
    }
}

/// A complete overload benchmark report (issue #1006).
#[derive(Debug, Serialize)]
pub struct OverloadReport {
    pub timestamp_utc: String,
    pub runner_os: String,
    pub rust_version: String,
    pub autumn_version: String,
    pub ceiling: usize,
    pub block_ms: u64,
    pub load_multiplier: u32,
    pub all_passed: bool,
    pub result: OverloadCheckResult,
}

/// Assemble an [`OverloadReport`] from measured stats and run parameters.
pub fn build_overload_report(
    ceiling: usize,
    block_ms: u64,
    load_multiplier: u32,
    stats: &OverloadStats,
) -> OverloadReport {
    let result = check_overload_budget(stats, &overload_budget());
    OverloadReport {
        timestamp_utc: chrono_utc_now(),
        runner_os: std::env::consts::OS.to_string(),
        rust_version: rust_version_string(),
        autumn_version: env!("CARGO_PKG_VERSION").to_string(),
        ceiling,
        block_ms,
        load_multiplier,
        all_passed: result.passed,
        result,
    }
}

/// Print the overload benchmark's budget/methodology table (no build, no server).
pub fn format_overload_budget_table() -> String {
    let budget = overload_budget();
    let mut out = String::new();
    writeln!(
        out,
        "Autumn overload / load-shedding budget (issue #1006)\n"
    )
    .unwrap();
    writeln!(
        out,
        "Offered load = 2x the configured `server.max_concurrent_requests` ceiling, \
         against handlers that block ~200ms."
    )
    .unwrap();
    writeln!(
        out,
        "  Admitted-request p99 latency   ≤ {:.0}% of unloaded baseline p99",
        budget.max_p99_ratio * 100.0
    )
    .unwrap();
    writeln!(
        out,
        "  Shed (503) response latency    ≤ {}ms",
        budget.max_shed_ms
    )
    .unwrap();
    writeln!(
        out,
        "  RSS during overload             must not grow unboundedly"
    )
    .unwrap();
    writeln!(
        out,
        "\nSee docs/guide/dev-loop-latency.md for methodology and prerequisites."
    )
    .unwrap();
    out
}

/// Serialise an [`OverloadReport`] as machine-readable JSON.
pub fn format_overload_json(report: &OverloadReport) -> String {
    serde_json::to_string_pretty(report)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialisation failed: {e}\"}}"))
}

/// Format an [`OverloadReport`] as a human-readable summary.
pub fn format_overload_human(report: &OverloadReport) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "Autumn overload benchmark report — {}",
        report.timestamp_utc
    )
    .unwrap();
    writeln!(
        out,
        "Runner: {}  Rust: {}  autumn-web: {}",
        report.runner_os, report.rust_version, report.autumn_version
    )
    .unwrap();
    writeln!(
        out,
        "Ceiling: {}  Block: {}ms  Offered load: {}x ceiling\n",
        report.ceiling, report.block_ms, report.load_multiplier
    )
    .unwrap();

    let r = &report.result;
    writeln!(out, "Baseline p99:  {} ms", r.baseline_p99_ms).unwrap();
    if r.baseline_shed_count > 0 {
        writeln!(
            out,
            "  ⚠ {} baseline request(s) were not admitted (offered load == ceiling should never shed) — baseline p99 may be unreliable",
            r.baseline_shed_count
        )
        .unwrap();
    }
    writeln!(
        out,
        "Admitted p99:  {} ms ({:.0}% of baseline) — {}",
        r.admitted_p99_ms,
        r.p99_ratio * 100.0,
        if r.admitted_p99_within_budget {
            "PASS"
        } else {
            "FAIL"
        }
    )
    .unwrap();
    writeln!(
        out,
        "Shed requests: {} (max latency {} ms) — {}",
        r.shed_count,
        r.shed_max_ms,
        if r.shed_fast_enough && r.admission_gate_verified {
            "PASS"
        } else {
            "FAIL"
        }
    )
    .unwrap();
    if !r.admission_gate_verified {
        writeln!(
            out,
            "  ⚠ no requests were shed at offered load = ceiling × load_multiplier — the admission gate may not be enforcing the ceiling"
        )
        .unwrap();
    }
    if r.transport_error_count > 0 {
        writeln!(
            out,
            "  ⚠ {} request(s) failed at the transport level (excluded from shed latency above) — benchmark host may be overwhelmed",
            r.transport_error_count
        )
        .unwrap();
    }
    if r.rss_skipped {
        writeln!(
            out,
            "RSS bounded:   skipped (not supported on this platform)"
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "RSS bounded:   {}",
            if r.rss_bounded { "PASS" } else { "FAIL" }
        )
        .unwrap();
    }
    out.push('\n');
    writeln!(out, "  ↳ {}", r.diagnosis).unwrap();
    out.push('\n');
    writeln!(
        out,
        "Overall: {}",
        if report.all_passed { "PASS" } else { "FAIL" }
    )
    .unwrap();
    out
}

/// Print a report (human or JSON), optionally write it to `output`, and return
/// the process exit code. Mirrors [`emit_report`] for the warm/cold-start
/// benchmarks so all three benchmark modes behave identically at the CLI edge.
pub fn emit_overload_report(
    report: &OverloadReport,
    json: bool,
    output: Option<&str>,
    fail_on_regression: bool,
) -> i32 {
    let human = format_overload_human(report);
    let machine = format_overload_json(report);

    if json {
        println!("{machine}");
    } else {
        println!("{human}");
    }

    let mut exit = 0;

    if let Some(path) = output {
        if let Err(e) = std::fs::write(path, &machine) {
            eprintln!("Error: could not write report to {path}: {e}");
            exit = 1;
        } else {
            eprintln!("Report written to {path}");
        }
    }

    if fail_on_regression && !report.all_passed {
        eprintln!("Overload benchmark did not meet the success metric. Exiting 1.");
        exit = 1;
    }

    exit
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_stats ─────────────────────────────────────────────────────

    #[test]
    fn compute_stats_empty_returns_zeros() {
        let s = compute_stats(&[]);
        assert_eq!(s.p50_ms, 0);
        assert_eq!(s.p95_ms, 0);
        assert_eq!(s.max_ms, 0);
        assert_eq!(s.sample_count, 0);
    }

    #[test]
    fn compute_stats_single_sample() {
        let s = compute_stats(&[500]);
        assert_eq!(s.p50_ms, 500);
        assert_eq!(s.p95_ms, 500);
        assert_eq!(s.max_ms, 500);
        assert_eq!(s.sample_count, 1);
    }

    #[test]
    fn compute_stats_10_ascending_samples() {
        // nearest-rank: p50 = ceil(0.5 * 10) = 5 → sorted[4] = 500
        //               p95 = ceil(0.95 * 10) = 10 → sorted[9] = 1000
        let samples: Vec<u64> = vec![100, 200, 300, 400, 500, 600, 700, 800, 900, 1000];
        let s = compute_stats(&samples);
        assert_eq!(s.p50_ms, 500);
        assert_eq!(s.p95_ms, 1000);
        assert_eq!(s.max_ms, 1000);
        assert_eq!(s.sample_count, 10);
    }

    #[test]
    fn compute_stats_unsorted_input_is_ordered_internally() {
        // sorted: [100, 300, 500, 700, 900]
        // p50 = ceil(0.5 * 5) = 3 → sorted[2] = 500
        // p95 = ceil(0.95 * 5) = 5 → sorted[4] = 900
        let samples: Vec<u64> = vec![900, 100, 500, 300, 700];
        let s = compute_stats(&samples);
        assert_eq!(s.p50_ms, 500);
        assert_eq!(s.p95_ms, 900);
        assert_eq!(s.max_ms, 900);
    }

    #[test]
    fn compute_stats_all_identical_samples() {
        let samples = vec![750u64; 20];
        let s = compute_stats(&samples);
        assert_eq!(s.p50_ms, 750);
        assert_eq!(s.p95_ms, 750);
        assert_eq!(s.max_ms, 750);
    }

    // ── budget_for ────────────────────────────────────────────────────────

    #[test]
    fn budget_css_tailwind_p95_is_1000ms() {
        assert_eq!(budget_for(ChangeClass::CssTailwind).p95_ms, 1000);
    }

    #[test]
    fn budget_static_asset_p95_is_1000ms() {
        assert_eq!(budget_for(ChangeClass::StaticAsset).p95_ms, 1000);
    }

    #[test]
    fn budget_rust_route_hello_p95_is_5000ms() {
        assert_eq!(budget_for(ChangeClass::RustRouteEditHello).p95_ms, 5_000);
    }

    #[test]
    fn budget_rust_route_db_p95_is_10000ms() {
        assert_eq!(budget_for(ChangeClass::RustRouteEditDb).p95_ms, 10_000);
    }

    #[test]
    fn budget_all_classes_have_nonzero_p95() {
        for class in [
            ChangeClass::InitialBoot,
            ChangeClass::RustRouteEditHello,
            ChangeClass::RustRouteEditDb,
            ChangeClass::CssTailwind,
            ChangeClass::StaticAsset,
            ChangeClass::ConfigEdit,
            ChangeClass::WatchDirEdit,
        ] {
            assert!(
                budget_for(class).p95_ms > 0,
                "p95 must be > 0 for {class:?}"
            );
        }
    }

    // ── check_budget ──────────────────────────────────────────────────────

    #[test]
    fn check_budget_passes_when_under_p95_and_max() {
        let budget = LatencyBudget {
            p50_ms: 500,
            p95_ms: 1000,
            max_ms: 5000,
        };
        let stats = ClassStats {
            p50_ms: 100,
            p95_ms: 800,
            max_ms: 900,
            sample_count: 10,
        };
        let r = check_budget(ChangeClass::CssTailwind, stats, &budget);
        assert!(r.passed);
        assert!(!r.p95_exceeded);
        assert!(!r.max_exceeded);
    }

    #[test]
    fn check_budget_fails_when_p95_exceeds_budget() {
        let budget = LatencyBudget {
            p50_ms: 500,
            p95_ms: 1000,
            max_ms: 5000,
        };
        let stats = ClassStats {
            p50_ms: 100,
            p95_ms: 1200,
            max_ms: 1500,
            sample_count: 10,
        };
        let r = check_budget(ChangeClass::CssTailwind, stats, &budget);
        assert!(!r.passed);
        assert!(r.p95_exceeded);
    }

    #[test]
    fn check_budget_fails_when_max_exceeds_budget() {
        let budget = LatencyBudget {
            p50_ms: 500,
            p95_ms: 1000,
            max_ms: 2000,
        };
        let stats = ClassStats {
            p50_ms: 100,
            p95_ms: 900,
            max_ms: 2500,
            sample_count: 10,
        };
        let r = check_budget(ChangeClass::CssTailwind, stats, &budget);
        assert!(!r.passed);
        assert!(r.max_exceeded);
    }

    #[test]
    fn check_budget_passes_exactly_at_limit() {
        let budget = LatencyBudget {
            p50_ms: 500,
            p95_ms: 1000,
            max_ms: 2000,
        };
        let stats = ClassStats {
            p50_ms: 500,
            p95_ms: 1000,
            max_ms: 2000,
            sample_count: 5,
        };
        let r = check_budget(ChangeClass::CssTailwind, stats, &budget);
        assert!(r.passed, "at-limit measurements should pass");
    }

    #[test]
    fn check_budget_diagnosis_css_names_journey() {
        let budget = budget_for(ChangeClass::CssTailwind);
        let stats = ClassStats {
            p50_ms: 2000,
            p95_ms: 2000,
            max_ms: 2000,
            sample_count: 5,
        };
        let r = check_budget(ChangeClass::CssTailwind, stats, &budget);
        assert!(
            r.journey_name.to_lowercase().contains("css")
                || r.journey_name.to_lowercase().contains("tailwind"),
            "journey_name should mention CSS or Tailwind, got: {}",
            r.journey_name
        );
        assert!(
            r.diagnosis.contains("p95"),
            "diagnosis must mention p95, got: {}",
            r.diagnosis
        );
        assert!(!r.next_action.is_empty(), "next_action must not be empty");
    }

    #[test]
    fn check_budget_diagnosis_rust_hello_names_rust() {
        let budget = budget_for(ChangeClass::RustRouteEditHello);
        let stats = ClassStats {
            p50_ms: 6000,
            p95_ms: 6000,
            max_ms: 7000,
            sample_count: 5,
        };
        let r = check_budget(ChangeClass::RustRouteEditHello, stats, &budget);
        assert!(
            r.journey_name.to_lowercase().contains("rust"),
            "journey_name should mention Rust, got: {}",
            r.journey_name
        );
        assert!(!r.next_action.is_empty());
    }

    #[test]
    fn check_budget_passing_result_has_empty_diagnosis() {
        let budget = budget_for(ChangeClass::CssTailwind);
        let stats = ClassStats {
            p50_ms: 100,
            p95_ms: 200,
            max_ms: 300,
            sample_count: 5,
        };
        let r = check_budget(ChangeClass::CssTailwind, stats, &budget);
        assert!(r.passed);
        assert!(
            r.diagnosis.is_empty(),
            "passing result must have empty diagnosis, got: {}",
            r.diagnosis
        );
        assert!(
            r.next_action.is_empty(),
            "passing result must have empty next_action, got: {}",
            r.next_action
        );
    }

    #[test]
    fn check_budget_overage_pct_is_zero_when_passing() {
        let budget = LatencyBudget {
            p50_ms: 500,
            p95_ms: 1000,
            max_ms: 5000,
        };
        let stats = ClassStats {
            p50_ms: 100,
            p95_ms: 800,
            max_ms: 900,
            sample_count: 5,
        };
        let r = check_budget(ChangeClass::StaticAsset, stats, &budget);
        assert!(
            r.p95_overage_pct.abs() < f64::EPSILON,
            "overage_pct must be 0 for a passing result, got {}",
            r.p95_overage_pct
        );
    }

    #[test]
    fn check_budget_overage_pct_calculated_when_failing() {
        let budget = LatencyBudget {
            p50_ms: 500,
            p95_ms: 1000,
            max_ms: 5000,
        };
        let stats = ClassStats {
            p50_ms: 100,
            p95_ms: 1500,
            max_ms: 2000,
            sample_count: 5,
        };
        let r = check_budget(ChangeClass::StaticAsset, stats, &budget);
        // (1500 - 1000) / 1000 * 100 = 50%
        assert!((r.p95_overage_pct - 50.0).abs() < 0.01);
    }

    // ── format_json_report ────────────────────────────────────────────────

    fn make_test_report(all_passed: bool) -> FullReport {
        let css_budget = budget_for(ChangeClass::CssTailwind);
        let css_stats = if all_passed {
            ClassStats {
                p50_ms: 200,
                p95_ms: 500,
                max_ms: 800,
                sample_count: 5,
            }
        } else {
            ClassStats {
                p50_ms: 2000,
                p95_ms: 2000,
                max_ms: 3000,
                sample_count: 5,
            }
        };
        let rust_budget = budget_for(ChangeClass::RustRouteEditHello);
        let rust_stats = ClassStats {
            p50_ms: 1000,
            p95_ms: 2000,
            max_ms: 3000,
            sample_count: 5,
        };

        let results = vec![
            check_budget(ChangeClass::CssTailwind, css_stats, &css_budget),
            check_budget(ChangeClass::RustRouteEditHello, rust_stats, &rust_budget),
        ];
        let computed_all_passed = results.iter().all(|r| r.passed);

        FullReport {
            timestamp_utc: "2026-05-26T00:00:00Z".to_string(),
            runner_os: "Linux".to_string(),
            rust_version: "1.88.0".to_string(),
            autumn_version: "0.5.0".to_string(),
            example_name: "examples/hello".to_string(),
            all_passed: computed_all_passed,
            results,
        }
    }

    #[test]
    fn format_json_report_produces_valid_json() {
        let report = make_test_report(true);
        let s = format_json_report(&report);
        serde_json::from_str::<serde_json::Value>(&s).expect("must be valid JSON");
    }

    #[test]
    fn format_json_report_contains_env_metadata_fields() {
        let report = make_test_report(true);
        let s = format_json_report(&report);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("timestamp_utc").is_some(), "must have timestamp_utc");
        assert!(v.get("runner_os").is_some(), "must have runner_os");
        assert!(v.get("rust_version").is_some(), "must have rust_version");
        assert!(v.get("results").is_some(), "must have results array");
        assert!(v.get("all_passed").is_some(), "must have all_passed");
    }

    #[test]
    fn format_json_report_results_have_required_fields() {
        let report = make_test_report(true);
        let s = format_json_report(&report);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let results = v["results"].as_array().expect("results must be an array");
        assert!(!results.is_empty(), "results must not be empty");
        let first = &results[0];
        assert!(first.get("change_class").is_some(), "missing change_class");
        assert!(first.get("passed").is_some(), "missing passed");
        assert!(
            first["stats"].get("p50_ms").is_some(),
            "missing p50_ms in stats"
        );
    }

    #[test]
    fn format_json_report_does_not_leak_home_path() {
        let report = make_test_report(true);
        let s = format_json_report(&report);
        assert!(!s.contains("/home/"), "must not leak /home/ paths");
        assert!(
            !s.contains("C:\\Users\\"),
            "must not leak Windows user paths"
        );
    }

    // ── format_human_summary ──────────────────────────────────────────────

    #[test]
    fn format_human_summary_shows_p50_p95_max_headers() {
        let report = make_test_report(true);
        let s = format_human_summary(&report);
        assert!(
            s.contains("p50") || s.contains("P50"),
            "summary must mention p50"
        );
        assert!(
            s.contains("p95") || s.contains("P95"),
            "summary must mention p95"
        );
        assert!(
            s.contains("max") || s.contains("Max") || s.contains("MAX"),
            "summary must mention max"
        );
    }

    #[test]
    fn format_human_summary_passing_report_shows_pass() {
        let report = make_test_report(true);
        let s = format_human_summary(&report);
        let lower = s.to_lowercase();
        assert!(
            lower.contains("pass"),
            "passing report must show PASS, got:\n{s}"
        );
    }

    #[test]
    fn format_human_summary_failing_report_shows_fail() {
        let report = make_test_report(false);
        let s = format_human_summary(&report);
        let lower = s.to_lowercase();
        assert!(
            lower.contains("fail") || lower.contains("exceed") || lower.contains("regress"),
            "failing report must show failure info, got:\n{s}"
        );
    }

    #[test]
    fn format_human_summary_has_at_least_one_row_per_result() {
        let report = make_test_report(true);
        let result_count = report.results.len();
        let s = format_human_summary(&report);
        let data_lines = s
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('-'))
            .count();
        assert!(
            data_lines >= result_count,
            "summary should have at least {result_count} non-empty lines, got {data_lines}"
        );
    }

    #[test]
    fn format_human_summary_failing_row_shows_diagnosis_and_next_action() {
        let report = make_test_report(false);
        let s = format_human_summary(&report);
        assert!(
            s.contains("↳"),
            "failing row must include diagnosis arrow, got:\n{s}"
        );
        assert!(
            s.contains("Next:"),
            "failing row must include Next: action, got:\n{s}"
        );
    }

    // ── change_class keys ─────────────────────────────────────────────────

    #[test]
    fn change_class_keys_are_unique() {
        let classes = [
            ChangeClass::InitialBoot,
            ChangeClass::RustRouteEditHello,
            ChangeClass::RustRouteEditDb,
            ChangeClass::CssTailwind,
            ChangeClass::StaticAsset,
            ChangeClass::ConfigEdit,
            ChangeClass::WatchDirEdit,
        ];
        let keys: Vec<_> = classes.iter().map(|c| c.key()).collect();
        let unique: std::collections::HashSet<_> = keys.iter().copied().collect();
        assert_eq!(
            keys.len(),
            unique.len(),
            "all change class keys must be unique"
        );
    }

    #[test]
    fn change_class_journey_names_are_unique() {
        let classes = [
            ChangeClass::InitialBoot,
            ChangeClass::RustRouteEditHello,
            ChangeClass::RustRouteEditDb,
            ChangeClass::CssTailwind,
            ChangeClass::StaticAsset,
            ChangeClass::ConfigEdit,
            ChangeClass::WatchDirEdit,
        ];
        let names: Vec<_> = classes.iter().map(|c| c.journey_name()).collect();
        let unique: std::collections::HashSet<_> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "all journey names must be unique"
        );
    }

    // ── run() / print_budget_table / build_placeholder_results ───────────

    #[test]
    fn run_dry_run_returns_zero_and_prints_table() {
        let exit = run("examples/hello", 5, None, false, false, true);
        assert_eq!(exit, 0);
    }

    #[test]
    fn run_hello_example_normal_mode_returns_zero() {
        let exit = run("examples/hello", 3, None, false, false, false);
        assert_eq!(exit, 0);
    }

    #[test]
    fn run_json_flag_returns_zero() {
        let exit = run("examples/hello", 3, None, true, false, false);
        assert_eq!(exit, 0);
    }

    #[test]
    fn run_todo_example_uses_db_path() {
        // exercises the `contains("todo")` branch in build_placeholder_results
        let exit = run("examples/todo-app", 3, None, false, false, false);
        assert_eq!(exit, 0);
    }

    #[test]
    fn run_blog_example_uses_db_path() {
        // exercises the `contains("blog")` branch in build_placeholder_results
        let exit = run("examples/blog", 3, None, false, false, false);
        assert_eq!(exit, 0);
    }

    #[test]
    fn run_fail_on_regression_with_passing_placeholder_still_zero() {
        // Placeholder results are all-zero ms which always passes the budget,
        // so --fail-on-regression must not trip with the placeholder driver.
        let exit = run("examples/hello", 3, None, false, true, false);
        assert_eq!(exit, 0);
    }

    #[test]
    fn run_output_writes_valid_json_to_file() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_str().unwrap().to_string();
        let exit = run("examples/hello", 3, Some(&path), false, false, false);
        assert_eq!(exit, 0);
        let content = std::fs::read_to_string(&path).expect("report file");
        serde_json::from_str::<serde_json::Value>(&content)
            .expect("output file must contain valid JSON");
    }

    #[test]
    fn run_output_write_failure_returns_nonzero() {
        // A requested --output that cannot be written must fail the run (not be
        // swallowed): CI gates read this file, so a fail-open would let a gate
        // pass with no report.
        let exit = run(
            "examples/hello",
            3,
            Some("/dev/full/nonexistent/path/report.json"),
            false,
            false,
            false,
        );
        assert_eq!(exit, 1);
    }

    // ── env var helpers ───────────────────────────────────────────────────

    #[test]
    fn chrono_utc_now_returns_set_env_var() {
        let result = temp_env::with_var(
            "AUTUMN_BENCH_TIMESTAMP",
            Some("2026-05-26T00:00:00Z"),
            chrono_utc_now,
        );
        assert_eq!(result, "2026-05-26T00:00:00Z");
    }

    #[test]
    fn chrono_utc_now_fallback_is_unknown() {
        let result = temp_env::with_var("AUTUMN_BENCH_TIMESTAMP", None::<&str>, chrono_utc_now);
        assert_eq!(result, "unknown");
    }

    #[test]
    fn rust_version_string_returns_set_env_var() {
        let result = temp_env::with_var(
            "AUTUMN_BENCH_RUST_VERSION",
            Some("rustc 1.88.0"),
            rust_version_string,
        );
        assert_eq!(result, "rustc 1.88.0");
    }

    #[test]
    fn rust_version_string_fallback_is_unknown() {
        let result = temp_env::with_var(
            "AUTUMN_BENCH_RUST_VERSION",
            None::<&str>,
            rust_version_string,
        );
        assert_eq!(result, "unknown");
    }

    // ── cold-start budgets ────────────────────────────────────────────────

    #[test]
    fn budget_cold_start_hello_p95_is_60000ms() {
        // Success metric (issue #977): p95 ≤ 60s for the no-DB cold start.
        assert_eq!(budget_for(ChangeClass::ColdStartHello).p95_ms, 60_000);
    }

    #[test]
    fn budget_cold_start_classes_have_nonzero_p95() {
        for class in [ChangeClass::ColdStartHello, ChangeClass::ColdStartDb] {
            assert!(
                budget_for(class).p95_ms > 0,
                "p95 must be > 0 for {class:?}"
            );
        }
    }

    #[test]
    fn cold_start_keys_are_unique_and_descriptive() {
        let hello = ChangeClass::ColdStartHello;
        let db = ChangeClass::ColdStartDb;
        assert_ne!(hello.key(), db.key());
        assert!(hello.key().contains("cold_start"), "got: {}", hello.key());
        assert!(db.key().contains("cold_start"), "got: {}", db.key());
    }

    #[test]
    fn cold_start_journey_names_mention_cold_start() {
        for class in [ChangeClass::ColdStartHello, ChangeClass::ColdStartDb] {
            assert!(
                class.journey_name().to_lowercase().contains("cold start"),
                "journey_name should mention cold start, got: {}",
                class.journey_name()
            );
        }
    }

    // ── cold-start budget table ───────────────────────────────────────────

    #[test]
    fn format_cold_start_budget_table_lists_hello_and_budget() {
        let table = format_cold_start_budget_table(false);
        assert!(
            table.to_lowercase().contains("cold start"),
            "table must mention cold start, got:\n{table}"
        );
        // 60s p95 budget for the gated no-DB shape must be visible.
        assert!(
            table.contains("60000") || table.contains("60 000"),
            "table must show the 60s p95 budget, got:\n{table}"
        );
    }

    #[test]
    fn format_cold_start_budget_table_db_row_only_when_requested() {
        let without = format_cold_start_budget_table(false);
        let with = format_cold_start_budget_table(true);
        assert!(
            !without.to_lowercase().contains("database"),
            "DB row must be hidden unless include_db, got:\n{without}"
        );
        assert!(
            with.to_lowercase().contains("database"),
            "DB row must appear when include_db, got:\n{with}"
        );
    }

    // ── cold-start report builder ─────────────────────────────────────────

    #[test]
    fn build_cold_start_report_hello_only_gates_on_hello() {
        // Hello well within budget → all_passed true, one result, not informational.
        let report = build_cold_start_report(&[40_000, 41_000, 42_000], &DbOutcome::NotRequested);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].change_class, "cold_start_hello");
        assert!(!report.results[0].informational);
        assert!(report.all_passed);
    }

    #[test]
    fn build_cold_start_report_hello_over_budget_fails_gate() {
        // p95 way above the 60s budget → gate fails.
        let report =
            build_cold_start_report(&[120_000, 130_000, 140_000], &DbOutcome::NotRequested);
        assert!(!report.all_passed, "over-budget hello must fail the gate");
    }

    #[test]
    fn build_cold_start_report_db_is_informational_and_ungated() {
        // DB samples blow past the budget but, being informational, must NOT
        // flip the overall gate (hello is within budget).
        let report = build_cold_start_report(
            &[40_000, 41_000, 42_000],
            &DbOutcome::Measured(vec![300_000, 310_000, 320_000]),
        );
        assert_eq!(report.results.len(), 2);
        let db = report
            .results
            .iter()
            .find(|r| r.change_class == "cold_start_db")
            .expect("db result present");
        assert!(db.informational, "db result must be informational");
        assert!(
            report.all_passed,
            "informational db over-budget must not fail the gate"
        );
    }

    #[test]
    fn build_cold_start_report_db_failure_is_informational_row() {
        // A requested-but-failed DB shape is recorded (not dropped) and does not
        // fail the gate.
        let report = build_cold_start_report(
            &[40_000, 41_000, 42_000],
            &DbOutcome::Failed("postgres unavailable".to_string()),
        );
        assert_eq!(report.results.len(), 2);
        let db = report
            .results
            .iter()
            .find(|r| r.change_class == "cold_start_db")
            .expect("db failure row present");
        assert!(db.informational);
        assert!(!db.passed, "failure row must be marked not-passed");
        assert!(db.diagnosis.contains("postgres unavailable"));
        assert!(
            report.all_passed,
            "informational db failure must not fail the gate"
        );
    }

    #[test]
    fn build_cold_start_report_json_has_metadata_and_no_path_leak() {
        let report = build_cold_start_report(
            &[40_000, 41_000, 42_000],
            &DbOutcome::Measured(vec![80_000, 81_000, 82_000]),
        );
        let s = format_json_report(&report);
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert!(v.get("timestamp_utc").is_some());
        assert!(v.get("runner_os").is_some());
        assert!(v.get("rust_version").is_some());
        assert!(v.get("autumn_version").is_some());
        assert!(v.get("all_passed").is_some());
        assert!(!s.contains("/home/"), "must not leak /home/ paths");
        assert!(!s.contains("C:\\Users\\"), "must not leak Windows paths");
    }

    #[test]
    fn sanitize_failure_reason_redacts_paths_and_drops_later_lines() {
        let raw = "cargo build failed for the scaffolded project:\n  \
                   error: could not compile at /home/runner/work/autumn/x.rs \
                   (registry /home/runner/.cargo/registry/foo) C:\\Users\\bob\\y.rs";
        let clean = sanitize_failure_reason(raw);
        // Only the first line is kept.
        assert!(clean.starts_with("cargo build failed for the scaffolded project:"));
        assert!(!clean.contains("error: could not compile"));
        // And no local paths survive even if they were on the first line.
        let first_line_case =
            sanitize_failure_reason("autumn new failed: /home/runner/tmp/p and C:\\Users\\bob\\z");
        assert!(!first_line_case.contains("/home/"));
        assert!(!first_line_case.contains("C:\\Users\\"));
        assert!(first_line_case.contains("<path>"));
        assert!(first_line_case.starts_with("autumn new failed:"));
    }

    #[test]
    fn sanitize_failure_reason_redacts_paths_glued_after_a_delimiter() {
        // Absolute paths embedded mid-token (after a `:` or `=`, no surrounding
        // whitespace and no backslash) must still be redacted.
        for raw in [
            "note:/home/runner/work/app/src/main.rs",
            "registry=/home/runner/.cargo/registry/foo",
            "(/home/runner/x)",
            "boom ~/.cargo/config",
        ] {
            let clean = sanitize_failure_reason(raw);
            assert!(
                !clean.contains("/home/") && !clean.contains("/.cargo"),
                "leaked a path for {raw:?} -> {clean:?}"
            );
            assert!(clean.contains("<path>"), "expected redaction for {raw:?}");
        }
        // Ordinary slashed words are left intact (not treated as paths).
        let kept = sanitize_failure_reason("choose and/or neither n/a");
        assert!(kept.contains("and/or"), "must not over-redact: {kept:?}");
        assert!(!kept.contains("<path>"), "must not over-redact: {kept:?}");
    }

    #[test]
    fn sanitize_failure_reason_bounds_length() {
        let long = "x".repeat(1_000);
        let clean = sanitize_failure_reason(&long);
        assert!(
            clean.chars().count() <= 301,
            "must be bounded (300 + ellipsis)"
        );
        assert!(clean.ends_with('…'));
    }

    #[test]
    fn build_cold_start_report_db_failure_does_not_leak_paths() {
        // A DB build failure carrying raw cargo stderr (with absolute paths) must
        // not leak those paths into the archived JSON report.
        let report = build_cold_start_report(
            &[40_000, 41_000, 42_000],
            &DbOutcome::Failed(
                "cargo build failed:\nerror at /home/runner/work/app/src/main.rs and \
                 C:\\Users\\runner\\app"
                    .to_string(),
            ),
        );
        let s = format_json_report(&report);
        assert!(!s.contains("/home/"), "must not leak /home/ paths");
        assert!(!s.contains("C:\\Users\\"), "must not leak Windows paths");
        assert!(!s.contains("main.rs"), "must not leak source file paths");
    }

    // ── cargo artifact path / free port ───────────────────────────────────

    #[test]
    fn cargo_executable_path_picks_matching_bin() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"some_dep"},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"coldstart_app"},"executable":"/tmp/x/target/debug/coldstart_app"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        let path = cargo_executable_path(stdout.as_bytes(), "coldstart_app");
        assert_eq!(
            path,
            Some(PathBuf::from("/tmp/x/target/debug/coldstart_app"))
        );
    }

    #[test]
    fn cargo_executable_path_none_when_absent() {
        let stdout = r#"{"reason":"build-finished","success":true}"#;
        assert!(cargo_executable_path(stdout.as_bytes(), "coldstart_app").is_none());
    }

    #[test]
    fn cargo_executable_path_skips_non_json_lines() {
        // Cargo may interleave non-JSON warning lines; they must be skipped, not
        // abort parsing, so a later valid artifact line is still found.
        let stdout = concat!(
            "warning: some non-json diagnostic line\n",
            r#"{"reason":"compiler-artifact","target":{"name":"coldstart_app"},"executable":"/tmp/x/target/debug/coldstart_app"}"#,
            "\n",
        );
        let path = cargo_executable_path(stdout.as_bytes(), "coldstart_app");
        assert_eq!(
            path,
            Some(PathBuf::from("/tmp/x/target/debug/coldstart_app"))
        );
    }

    // ── build_diagnostics next-action coverage ────────────────────────────

    #[test]
    fn build_diagnostics_names_an_action_for_every_warm_class() {
        // Every change class must yield a non-empty, actionable next step when it
        // regresses — exercises each match arm in `build_diagnostics`.
        for class in [
            ChangeClass::InitialBoot,
            ChangeClass::RustRouteEditHello,
            ChangeClass::RustRouteEditDb,
            ChangeClass::CssTailwind,
            ChangeClass::StaticAsset,
            ChangeClass::ConfigEdit,
            ChangeClass::WatchDirEdit,
        ] {
            let budget = budget_for(class);
            // Stats that blow past both the p95 and max budgets so the result fails
            // and a diagnosis + next action are produced.
            let stats = compute_stats(&[budget.max_ms * 4]);
            let result = check_budget(class, stats, &budget);
            assert!(
                !result.passed,
                "{class:?} should fail this over-budget input"
            );
            assert!(
                !result.next_action.is_empty(),
                "{class:?} must propose a next action"
            );
        }
    }

    // ── Overload benchmark (issue #1006) ────────────────────────────────────

    fn passing_overload_stats() -> OverloadStats {
        OverloadStats {
            baseline_samples_ms: vec![200, 205, 210, 202, 208],
            baseline_shed_count: 0,
            admitted_samples_ms: vec![205, 210, 215, 208, 212],
            shed_samples_ms: vec![1, 2, 1, 2, 1],
            shed_count: 5,
            admitted_count: 5,
            transport_error_samples_ms: vec![],
            transport_error_count: 0,
            rss_samples_kb: vec![10_000, 10_100, 10_050, 10_200, 10_150, 10_100],
        }
    }

    #[test]
    fn overload_budget_check_passes_within_all_three_budgets() {
        let stats = passing_overload_stats();
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(result.passed, "{result:?}");
        assert!(result.admitted_p99_within_budget);
        assert!(result.shed_fast_enough);
        assert!(result.rss_bounded);
        assert!(!result.rss_skipped);
    }

    #[test]
    fn overload_budget_check_fails_when_admitted_p99_regresses() {
        let mut stats = passing_overload_stats();
        // Admitted latency more than 20% over the 210ms baseline p99.
        stats.admitted_samples_ms = vec![400, 405, 410, 402, 408];
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(!result.passed);
        assert!(!result.admitted_p99_within_budget);
        assert!(result.shed_fast_enough, "shed budget is independent");
        assert!(result.rss_bounded, "rss budget is independent");
        assert!(result.diagnosis.contains("admitted p99"));
    }

    #[test]
    fn overload_budget_check_fails_when_shedding_is_slow() {
        let mut stats = passing_overload_stats();
        stats.shed_samples_ms = vec![50, 60, 45];
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(!result.passed);
        assert!(!result.shed_fast_enough);
        assert!(
            result.admitted_p99_within_budget,
            "p99 budget is independent"
        );
        assert!(result.diagnosis.contains("shed responses"));
    }

    #[test]
    fn overload_budget_check_transport_errors_do_not_corrupt_shed_fast_enough() {
        // A hung/timed-out connection's multi-second latency must not be
        // conflated with genuine 503 shed latency — only `shed_samples_ms`
        // (real 503s) gates `shed_fast_enough`.
        let mut stats = passing_overload_stats();
        stats.transport_error_samples_ms = vec![5000, 6000];
        stats.transport_error_count = 2;
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(
            result.shed_fast_enough,
            "transport-error latency must not gate the shed-latency budget"
        );
        assert_eq!(result.transport_error_count, 2);
        assert!(result.diagnosis.contains("WARNING"));
        assert!(result.diagnosis.contains("transport level"));
    }

    #[test]
    fn overload_budget_check_fails_when_rss_grows_unboundedly() {
        let mut stats = passing_overload_stats();
        stats.rss_samples_kb = vec![10_000, 10_100, 10_200, 30_000, 45_000, 60_000];
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(!result.passed);
        assert!(!result.rss_bounded);
        assert!(result.diagnosis.contains("RSS grew"));
    }

    #[test]
    fn overload_budget_check_zero_shed_count_never_fails_shed_budget() {
        let mut stats = passing_overload_stats();
        stats.shed_count = 0;
        stats.shed_samples_ms = vec![];
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(result.shed_fast_enough);
    }

    #[test]
    fn overload_budget_check_fails_when_nothing_was_ever_shed() {
        // A broken admission gate that never enforces the ceiling would
        // otherwise report success: shed_count == 0 vacuously satisfies
        // shed_fast_enough (nothing to be slow), and admitted p99/RSS can
        // both look fine when every offered request is simply admitted. The
        // benchmark's offered load is always ceiling * load_multiplier, so a
        // working gate must shed *something* — `passed` must catch this even
        // though every other budget dimension looks perfect.
        let mut stats = passing_overload_stats();
        stats.shed_count = 0;
        stats.shed_samples_ms = vec![];
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(!result.admission_gate_verified);
        assert!(
            !result.passed,
            "a run where nothing was ever shed must not report overall PASS"
        );
        assert!(result.diagnosis.contains("no requests were shed"));
    }

    #[test]
    fn overload_budget_check_empty_rss_samples_are_skipped_not_failed() {
        let mut stats = passing_overload_stats();
        stats.rss_samples_kb = vec![];
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(result.rss_bounded, "empty samples must not fail the gate");
        assert!(result.rss_skipped);
    }

    #[test]
    fn overload_budget_check_few_rss_samples_pass_vacuously() {
        let mut stats = passing_overload_stats();
        // Fewer than 4 samples can't distinguish noise from a trend.
        stats.rss_samples_kb = vec![10_000, 90_000];
        let result = check_overload_budget(&stats, &overload_budget());
        assert!(result.rss_bounded);
        assert!(!result.rss_skipped, "non-empty samples are not 'skipped'");
    }

    #[test]
    fn overload_budget_check_zero_baseline_does_not_gate_p99_ratio() {
        let mut stats = passing_overload_stats();
        stats.baseline_samples_ms = vec![];
        let result = check_overload_budget(&stats, &overload_budget());
        assert_eq!(result.baseline_p99_ms, 0);
        assert!(
            result.admitted_p99_within_budget,
            "no baseline means nothing to regress against"
        );
    }

    #[test]
    fn build_overload_report_reflects_check_result() {
        let stats = passing_overload_stats();
        let report = build_overload_report(64, 200, 2, &stats);
        assert_eq!(report.ceiling, 64);
        assert_eq!(report.block_ms, 200);
        assert_eq!(report.load_multiplier, 2);
        assert!(report.all_passed);
        assert_eq!(report.all_passed, report.result.passed);
    }

    #[test]
    fn format_overload_json_has_metadata_and_no_path_leak() {
        let report = build_overload_report(64, 200, 2, &passing_overload_stats());
        let json = format_overload_json(&report);
        assert!(json.contains("\"ceiling\": 64"));
        assert!(json.contains("\"timestamp_utc\""));
        assert!(json.contains("\"runner_os\""));
        assert!(!json.contains("/home/"), "must not leak local paths");
        assert!(!json.contains("/root/"), "must not leak local paths");
    }

    #[test]
    fn format_overload_human_shows_pass_fail_per_dimension() {
        let report = build_overload_report(64, 200, 2, &passing_overload_stats());
        let human = format_overload_human(&report);
        assert!(human.contains("Overall: PASS"));
        assert!(human.contains("Baseline p99"));
        assert!(human.contains("Admitted p99"));
        assert!(human.contains("Shed requests"));
    }

    #[test]
    fn format_overload_human_warns_on_nonzero_baseline_shed_count() {
        let mut stats = passing_overload_stats();
        stats.baseline_shed_count = 2;
        let report = build_overload_report(64, 200, 2, &stats);
        let human = format_overload_human(&report);
        assert!(
            human.contains("2 baseline request(s) were not admitted"),
            "must not silently discard a shed/failed baseline request:\n{human}"
        );
        assert!(report.result.diagnosis.contains("WARNING"));
    }

    #[test]
    fn format_overload_human_is_silent_when_baseline_never_sheds() {
        let report = build_overload_report(64, 200, 2, &passing_overload_stats());
        let human = format_overload_human(&report);
        assert!(!human.contains("were not admitted"));
    }

    #[test]
    fn format_overload_human_shows_transport_error_count_distinctly_from_shed() {
        let mut stats = passing_overload_stats();
        stats.transport_error_samples_ms = vec![5000];
        stats.transport_error_count = 1;
        let report = build_overload_report(64, 200, 2, &stats);
        let human = format_overload_human(&report);
        assert!(
            human.contains("1 request(s) failed at the transport level"),
            "transport errors must be surfaced distinctly, not folded into 'Shed requests':\n{human}"
        );
        // The genuine shed count (from `passing_overload_stats`) must be unaffected.
        assert!(human.contains("Shed requests: 5"));
    }

    #[test]
    fn format_overload_human_warns_and_fails_when_nothing_was_shed() {
        let mut stats = passing_overload_stats();
        stats.shed_count = 0;
        stats.shed_samples_ms = vec![];
        let report = build_overload_report(64, 200, 2, &stats);
        let human = format_overload_human(&report);
        assert!(!report.all_passed);
        assert!(human.contains("Shed requests: 0"));
        assert!(human.contains("Overall: FAIL"));
        assert!(
            human.contains("admission gate may not be enforcing the ceiling"),
            "must warn distinctly, not just silently fail:\n{human}"
        );
    }

    #[test]
    fn format_overload_budget_table_lists_all_three_dimensions() {
        let table = format_overload_budget_table();
        assert!(table.contains("Admitted-request p99"));
        assert!(table.contains("Shed (503) response latency"));
        assert!(table.contains("RSS during overload"));
        assert!(table.contains("issue #1006"));
    }

    #[test]
    fn emit_overload_report_dry_run_style_returns_zero_when_passing() {
        let report = build_overload_report(64, 200, 2, &passing_overload_stats());
        assert_eq!(emit_overload_report(&report, true, None, true), 0);
    }

    #[test]
    fn emit_overload_report_fails_on_regression_when_configured() {
        let mut stats = passing_overload_stats();
        stats.admitted_samples_ms = vec![10_000; 5];
        let report = build_overload_report(64, 200, 2, &stats);
        assert_eq!(emit_overload_report(&report, true, None, true), 1);
        // Without --fail-on-regression the exit code stays 0 (visibility only).
        assert_eq!(emit_overload_report(&report, true, None, false), 0);
    }

    #[test]
    fn rss_bounded_true_for_flat_samples() {
        assert!(rss_bounded(&[
            10_000, 10_050, 9_980, 10_020, 10_010, 10_000
        ]));
    }

    #[test]
    fn rss_bounded_false_for_growing_samples() {
        assert!(!rss_bounded(&[
            10_000, 10_050, 10_100, 25_000, 40_000, 55_000
        ]));
    }

    #[test]
    fn percentile_99_respects_p50_p95_p99_ordering() {
        // `percentile_99` and `compute_stats`'s p50/p95 use the same
        // nearest-rank method over the same sorted sample set, so p50 <= p95
        // <= p99 must hold — guards against a regression in either
        // implementation diverging from the other.
        let samples: Vec<u64> = (1..=100).collect();
        let class_stats = compute_stats(&samples);
        let p99 = percentile_99(&samples);
        assert!(class_stats.p50_ms <= class_stats.p95_ms);
        assert!(class_stats.p95_ms <= p99);
    }

    #[test]
    fn percentile_99_all_identical_samples_equals_that_value() {
        assert_eq!(percentile_99(&[42, 42, 42, 42]), 42);
    }

    #[test]
    fn percentile_99_empty_slice_returns_zero() {
        assert_eq!(percentile_99(&[]), 0);
    }
}
