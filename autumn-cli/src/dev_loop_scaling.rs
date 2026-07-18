//! Macro-scaling benchmark: measures how warm incremental rebuild p95 grows
//! as an Autumn app adds routes and models (issue #983).
//!
//! This module is the **pure, unit-tested** half of the scaling sweep:
//! - A deterministic synthetic-app generator (N `#[get]` handlers + N
//!   `#[model]`/`#[repository]` pairs) used by the live driver to scaffold
//!   compile targets without checking in fixtures.
//! - Budget constants: p95 ≤ **8 s** absolute per size; slope
//!   (p95\@N\_max / p95\@N\_min) ≤ **2×**.
//! - `ScalingReport` with per-size `ScalingPoint`s, slope, and baseline-
//!   regression check (skipped until an accepted baseline is established,
//!   mirroring the cold-start policy).
//! - Formatters and emitters reusing the `dev_loop_bench` plumbing so the
//!   output shape is identical to every other Autumn latency report.
//!
//! The **live driver** (tempdir scaffold → warm cargo build → single-file edit
//! → timed incremental rebuild) lives in [`crate::scaling_driver`], which is
//! excluded from coverage like `cold_start_driver.rs`.
//!
//! See `docs/guide/dev-loop-latency.md` (§ Scaling) for the budget table and
//! methodology.

use std::fmt::Write as FmtWrite;

use serde::{Deserialize, Serialize};

use crate::dev_loop_bench::{ClassStats, chrono_utc_now, compute_stats, rust_version_string};

// ── Budget constants ─────────────────────────────────────────────────────────

/// Absolute p95 ceiling per size point (ms). Any N whose p95 exceeds this
/// fails the gate regardless of slope.
pub const SCALING_ABS_P95_MS: u64 = 8_000;

/// Maximum allowed slope: p95\@N\_max / p95\@N\_min must be ≤ this value.
/// Enforces that the edit-refresh loop at 100 routes is no more than 2× slower
/// than at 1 route.
pub const SCALING_SLOPE_MAX: f64 = 2.0;

/// Baseline regression allowance: if an accepted slope baseline is on record,
/// the measured slope may not exceed `accepted × SCALING_BASELINE_REGRESSION`
/// before the run is considered a regression.
pub const SCALING_BASELINE_REGRESSION: f64 = 1.20;

/// Default size sweep (CSV) used by `--sizes` when the flag is omitted.
pub const DEFAULT_SIZES: &str = "1,25,50,100";

// ── Generated-app types ──────────────────────────────────────────────────────

/// A complete synthetic Autumn app ready to be written to disk and compiled.
///
/// `files` contains `(relative_path, content)` pairs; `cargo_toml` is the
/// root manifest. Both are byte-for-byte deterministic for a given `n`.
#[derive(Debug, Clone)]
pub struct GeneratedApp {
    /// Content of `Cargo.toml` at the project root.
    pub cargo_toml: String,
    /// All source files: `(relpath_from_project_root, content)`.
    pub files: Vec<(String, String)>,
}

// ── Baseline ─────────────────────────────────────────────────────────────────

/// Accepted slope baseline, loaded from `benchmarks/dev-loop-scaling/baseline.json`.
///
/// When `established` is `false` (the initial committed state), the
/// `> 20%-over-baseline` regression check is skipped entirely so the first CI
/// run can record a baseline number without failing. The absolute (8 s) and 2×
/// slope gates always apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingBaseline {
    pub established: bool,
    pub accepted_slope: Option<f64>,
    #[serde(default)]
    pub note: Option<String>,
}

// ── Report types ─────────────────────────────────────────────────────────────

/// Measured result for a single app size N.
#[derive(Debug, Clone, Serialize)]
pub struct ScalingPoint {
    /// Number of handlers / model+repository pairs in this synthetic app.
    pub n: usize,
    /// p50/p95/max timing statistics (ms) across `--runs` incremental rebuild samples.
    pub stats: ClassStats,
    /// Whether this size's p95 is within the 8 s absolute ceiling.
    pub abs_passed: bool,
}

/// Complete scaling benchmark report.
#[derive(Debug, Serialize)]
pub struct ScalingReport {
    // ── Environment metadata ──
    pub timestamp_utc: String,
    pub runner_os: String,
    pub rust_version: String,
    pub autumn_version: String,
    // ── Sweep configuration ──
    /// Sizes that were swept, in the order measured.
    pub sizes: Vec<usize>,
    // ── Per-size results ──
    pub points: Vec<ScalingPoint>,
    // ── Slope gate ──
    /// p95\@N\_max / p95\@N\_min (1.0 when flat or only one size measured).
    pub slope: f64,
    /// Budget ceiling for the slope (always `SCALING_SLOPE_MAX`).
    pub slope_max: f64,
    pub slope_passed: bool,
    // ── Baseline regression gate ──
    /// The accepted slope from the baseline file, when established.
    pub baseline_slope: Option<f64>,
    /// `true` when no baseline is established OR regression is within allowance.
    pub baseline_regression_passed: bool,
    // ── Overall ──
    /// `true` iff every per-size p95 ≤ 8 s AND slope ≤ 2× AND (baseline
    /// regression ≤ 20% when an accepted baseline exists).
    pub all_passed: bool,
}

// ── Generator ────────────────────────────────────────────────────────────────

/// Generate a deterministic synthetic Autumn app with `n` handlers and `n`
/// model/repository pairs.
///
/// The generated app:
/// - Is a **standalone workspace root** (contains `[workspace]`) so that the
///   `[patch.crates-io]` section appended by the live driver is valid.
/// - Depends only on `autumn-web` at default features (which include `db`,
///   bringing in diesel and diesel-async).
/// - Uses `autumn_web::reexports::diesel::table!` (no separate diesel dep)
///   and `#[autumn_web::model]` / `#[autumn_web::repository]`, exactly
///   mirroring `autumn/tests/compile-pass/repository_no_hooks.rs`.
/// - Is **compile-only** — no Postgres is required because we never boot the
///   binary, only `cargo build` it.
///
/// The file named `src/handlers.rs` contains a `BENCH_EDIT_SENTINEL` constant
/// in `handler_0` whose value can be changed by [`apply_handler_edit`] to
/// force a warm incremental recompile of exactly one file.
pub fn generate_synthetic_app(n: usize) -> GeneratedApp {
    // `autumn-web = "*"`: the live driver always appends a `[patch.crates-io]`
    // pointing this dependency at the workspace's local `autumn-web` source, so
    // the resolved version is whatever the repo currently is. A wildcard avoids
    // a hardcoded version that would silently break `[patch]` resolution after a
    // workspace version bump; `publish = false` makes the wildcard valid.
    let cargo_toml = "[package]\n\
         name = \"scaling_bench_app\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [workspace]\n\
         \n\
         [dependencies]\n\
         autumn-web = \"*\"\n"
        .to_string();

    let files = vec![
        ("src/schema.rs".to_string(), generate_schema(n)),
        ("src/models.rs".to_string(), generate_models(n)),
        ("src/repositories.rs".to_string(), generate_repositories(n)),
        ("src/handlers.rs".to_string(), generate_handlers(n, 0)),
        ("src/main.rs".to_string(), generate_main(n)),
    ];

    GeneratedApp { cargo_toml, files }
}

fn generate_schema(n: usize) -> String {
    let mut out = String::new();
    for k in 0..n {
        writeln!(
            out,
            "autumn_web::reexports::diesel::table! {{\n\
             \tentity_{k} (id) {{\n\
             \t\tid -> Int8,\n\
             \t\tcontent -> Text,\n\
             \t}}\n\
             }}\n"
        )
        .unwrap();
    }
    out
}

fn generate_models(n: usize) -> String {
    let mut out = String::from(
        "// Bring all diesel table DSLs into scope for derive macros.\nuse crate::schema::*;\n\n",
    );
    for k in 0..n {
        writeln!(
            out,
            "#[autumn_web::model(table = \"entity_{k}\")]\npub struct Entity{k} {{\n\
             \t#[id]\n\
             \tpub id: i64,\n\
             \tpub content: String,\n\
             }}\n"
        )
        .unwrap();
    }
    out
}

fn generate_repositories(n: usize) -> String {
    let mut out = String::from("use crate::models::*;\nuse crate::schema::*;\n\n");
    for k in 0..n {
        writeln!(
            out,
            "#[autumn_web::repository(Entity{k})]\npub trait Entity{k}Repository {{\n\
             \tfn find_by_content(content: String) -> Vec<Entity{k}>;\n\
             }}\n"
        )
        .unwrap();
    }
    out
}

/// Generate `src/handlers.rs` with `n` route handlers.
///
/// `handler_0` contains the `BENCH_EDIT_SENTINEL` constant, which
/// [`apply_handler_edit`] bumps to force a warm incremental recompile of
/// exactly this one file. All other handlers are static strings.
fn generate_handlers(n: usize, revision: u32) -> String {
    let mut out = String::from("use autumn_web::prelude::*;\n\n");
    // handler_0 — the editable handler whose BENCH_EDIT_SENTINEL is bumped
    // each time the live driver wants to trigger a warm incremental rebuild.
    writeln!(
        out,
        "const BENCH_EDIT_SENTINEL: &str = \"bench_edit_rev_{revision}\";\n\n\
         #[get(\"/entity_0\")]\npub async fn handler_0() -> String {{ \
         BENCH_EDIT_SENTINEL.to_string() }}\n"
    )
    .unwrap();
    for k in 1..n {
        writeln!(
            out,
            "#[get(\"/entity_{k}\")]\npub async fn handler_{k}() -> String {{ \
             \"handler_{k}\".to_string() }}\n"
        )
        .unwrap();
    }
    out
}

fn generate_main(n: usize) -> String {
    let handler_list = (0..n)
        .map(|k| format!("handler_{k}"))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "use autumn_web::prelude::*;\n\
         mod schema;\n\
         mod models;\n\
         mod repositories;\n\
         mod handlers;\n\
         use handlers::*;\n\n\
         #[autumn_web::main]\nasync fn main() {{\n\
         \tautumn_web::app()\n\
         \t\t.routes(routes![{handler_list}])\n\
         \t\t.run()\n\
         \t\t.await;\n\
         }}\n"
    )
}

// ── Editing ──────────────────────────────────────────────────────────────────

/// Return a copy of `handlers_src` with the `BENCH_EDIT_SENTINEL` constant
/// bumped to `revision`.
///
/// This changes exactly one line in one file, causing `cargo build` to
/// recompile only `handlers.rs` — a genuine warm incremental rebuild that
/// exercises macro re-expansion without touching any other module.
pub fn apply_handler_edit(handlers_src: &str, revision: u32) -> String {
    // Use split('\n') instead of lines() so trailing newlines are preserved
    // when the result is rejoined: lines() silently drops trailing \n, which
    // would cause the output to have a different line count than the input.
    handlers_src
        .split('\n')
        .map(|line| {
            if line.trim_start().starts_with("const BENCH_EDIT_SENTINEL:") {
                format!("const BENCH_EDIT_SENTINEL: &str = \"bench_edit_rev_{revision}\";")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Size parsing ─────────────────────────────────────────────────────────────

/// Parse a comma-separated list of positive integers (e.g. `"1,25,50,100"`).
///
/// Returns `Err` if the string is empty, any token is not a positive integer,
/// or any token is `0` (zero-size apps would yield meaningless slope data).
pub fn parse_sizes(csv: &str) -> Result<Vec<usize>, String> {
    if csv.trim().is_empty() {
        return Err("--sizes must not be empty".to_string());
    }
    let mut sizes = Vec::new();
    for part in csv.split(',') {
        let tok = part.trim();
        match tok.parse::<usize>() {
            Ok(0) => return Err("app size must be ≥ 1 (got 0)".to_string()),
            Ok(n) => sizes.push(n),
            Err(_) => return Err(format!("invalid size {tok:?}: not a positive integer")),
        }
    }
    if sizes.is_empty() {
        return Err("no valid sizes found".to_string());
    }
    Ok(sizes)
}

// ── Slope ────────────────────────────────────────────────────────────────────

/// Compute the slope: `p95_max_n / p95_min_n`.
///
/// Returns `1.0` (flat / no regression) when `p95_min_n` is `0` to avoid
/// division-by-zero from placeholder or empty samples.
pub fn compute_slope(p95_min_n: u64, p95_max_n: u64) -> f64 {
    if p95_min_n == 0 {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let result = p95_max_n as f64 / p95_min_n as f64;
    result
}

// ── Report builder ───────────────────────────────────────────────────────────

/// Assemble a `ScalingReport` from raw per-size timing samples.
///
/// `measurements` is `[(N, samples_ms), …]` in size order. The slope is
/// computed from the first point's p95 to the last point's p95.
///
/// When fewer than two sizes are present the slope defaults to `1.0` (flat).
///
/// The baseline-regression check is **skipped** (passes) when `baseline` is
/// `None` or `baseline.established` is `false`, mirroring the cold-start
/// policy: the first weekly run records the number; until then only the
/// absolute and 2× slope gates apply.
pub fn build_scaling_report(
    measurements: &[(usize, Vec<u64>)],
    baseline: Option<&ScalingBaseline>,
) -> ScalingReport {
    let mut points = Vec::new();
    for (n, samples) in measurements {
        let stats = compute_stats(samples);
        // An empty sample vector means the measurement for this size failed:
        // `compute_stats` yields p95 = 0, which would otherwise sail under the
        // absolute ceiling and silently report a missing measurement as a pass.
        // Treat a sample-less point as a hard fail.
        let abs_passed = !samples.is_empty() && stats.p95_ms <= SCALING_ABS_P95_MS;
        points.push(ScalingPoint {
            n: *n,
            stats,
            abs_passed,
        });
    }

    // Compute the slope from the smallest-N to the largest-N point, ordered by
    // N rather than by input position. `parse_sizes` preserves caller order, so
    // a sweep passed out of order (e.g. `--sizes 100,50,1`) must not invert the
    // ratio and let a real regression pass the gate.
    let min_n_p95 = points
        .iter()
        .min_by_key(|p| p.n)
        .map_or(0, |p| p.stats.p95_ms);
    let max_n_p95 = points
        .iter()
        .max_by_key(|p| p.n)
        .map_or(0, |p| p.stats.p95_ms);
    let slope = if points.len() < 2 {
        1.0
    } else {
        compute_slope(min_n_p95, max_n_p95)
    };
    let slope_passed = slope <= SCALING_SLOPE_MAX;

    let (baseline_slope, baseline_regression_passed) = match baseline {
        Some(b) if b.established => b.accepted_slope.map_or((None, true), |accepted| {
            let passes = slope <= accepted * SCALING_BASELINE_REGRESSION;
            (Some(accepted), passes)
        }),
        _ => (None, true),
    };

    let all_abs_passed = points.is_empty() || points.iter().all(|p| p.abs_passed);
    let all_passed = all_abs_passed && slope_passed && baseline_regression_passed;

    ScalingReport {
        timestamp_utc: chrono_utc_now(),
        runner_os: std::env::consts::OS.to_string(),
        rust_version: rust_version_string(),
        autumn_version: env!("CARGO_PKG_VERSION").to_string(),
        sizes: measurements.iter().map(|(n, _)| *n).collect(),
        points,
        slope,
        slope_max: SCALING_SLOPE_MAX,
        slope_passed,
        baseline_slope,
        baseline_regression_passed,
        all_passed,
    }
}

// ── Formatters ───────────────────────────────────────────────────────────────

/// Serialise a `ScalingReport` as a machine-readable JSON string.
pub fn format_scaling_json(report: &ScalingReport) -> String {
    serde_json::to_string_pretty(report)
        .unwrap_or_else(|e| format!("{{\"error\": \"serialisation failed: {e}\"}}"))
}

/// Format a `ScalingReport` as a human-readable summary table.
pub fn format_scaling_human(report: &ScalingReport) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "Autumn macro-scaling benchmark — {}",
        report.timestamp_utc
    )
    .unwrap();
    writeln!(
        out,
        "Runner: {runner}  Rust: {rust}  autumn-web: {av}",
        runner = report.runner_os,
        rust = report.rust_version,
        av = report.autumn_version,
    )
    .unwrap();
    writeln!(
        out,
        "Budget: p95 ≤ {SCALING_ABS_P95_MS} ms absolute; slope ≤ {SCALING_SLOPE_MAX}×",
    )
    .unwrap();
    out.push('\n');

    writeln!(
        out,
        "{:>6}  {:>8}  {:>8}  {:>8}  Abs",
        "N", "p50 ms", "p95 ms", "max ms",
    )
    .unwrap();
    writeln!(out, "{}", "-".repeat(44)).unwrap();

    for p in &report.points {
        let abs = if p.abs_passed { "PASS" } else { "FAIL" };
        writeln!(
            out,
            "{:>6}  {:>8}  {:>8}  {:>8}  {abs}",
            p.n, p.stats.p50_ms, p.stats.p95_ms, p.stats.max_ms,
        )
        .unwrap();
    }

    out.push('\n');
    let slope_status = if report.slope_passed { "PASS" } else { "FAIL" };
    writeln!(
        out,
        "Slope (p95@Nmax / p95@Nmin): {:.2}× ≤ {:.1}× → {slope_status}",
        report.slope, report.slope_max,
    )
    .unwrap();

    match report.baseline_slope {
        Some(accepted) => {
            let reg_status = if report.baseline_regression_passed {
                "PASS"
            } else {
                "FAIL"
            };
            writeln!(
                out,
                "Baseline regression: slope {:.2}× vs accepted {:.2}× (≤ {:.0}%) → {reg_status}",
                report.slope,
                accepted,
                SCALING_BASELINE_REGRESSION.mul_add(100.0, -100.0),
            )
            .unwrap();
        }
        None => {
            writeln!(out, "Baseline regression: N/A (no established baseline)").unwrap();
        }
    }

    out.push('\n');
    let overall = if report.all_passed { "PASS" } else { "FAIL" };
    writeln!(out, "Overall: {overall}").unwrap();

    out
}

/// Render the scaling budget table for `--dry-run` (no build needed).
pub fn format_scaling_budget_table() -> String {
    let mut out = String::new();
    writeln!(out, "Autumn macro-scaling budget (issue #983)\n").unwrap();
    writeln!(out, "{:<12}  {:>12}  Notes", "Metric", "Budget").unwrap();
    writeln!(out, "{}", "-".repeat(50)).unwrap();
    writeln!(
        out,
        "{:<12}  {:>11} ms  p95 warm incremental rebuild at any N ∈ {{1,25,50,100}}",
        "Abs p95", SCALING_ABS_P95_MS,
    )
    .unwrap();
    writeln!(
        out,
        "{:<12}  {:>11.1}×  p95@N=100 / p95@N=1 (near-flat growth)",
        "Slope max", SCALING_SLOPE_MAX,
    )
    .unwrap();
    writeln!(
        out,
        "{:<12}  {:>10.0}%   vs accepted baseline slope (skipped until baseline established)",
        "Regression",
        SCALING_BASELINE_REGRESSION.mul_add(100.0, -100.0),
    )
    .unwrap();
    writeln!(
        out,
        "\nSee docs/guide/dev-loop-latency.md (§ Scaling) for methodology."
    )
    .unwrap();
    out
}

// ── Emitter ──────────────────────────────────────────────────────────────────

/// Print a `ScalingReport` (human or JSON), optionally write to `output`, and
/// return the process exit code. Mirrors `dev_loop_bench::emit_report`.
pub fn emit_scaling_report(
    report: &ScalingReport,
    json: bool,
    output: Option<&str>,
    fail_on_regression: bool,
) -> i32 {
    let human = format_scaling_human(report);
    let machine = format_scaling_json(report);

    if json {
        println!("{machine}");
    } else {
        println!("{human}");
    }

    let mut exit = 0;

    if let Some(path) = output {
        if let Err(e) = std::fs::write(path, &machine) {
            eprintln!("Error: could not write scaling report to {path}: {e}");
            exit = 1;
        } else {
            eprintln!("Scaling report written to {path}");
        }
    }

    if fail_on_regression && !report.all_passed {
        eprintln!("Scaling benchmark failed: p95 ceiling or slope budget exceeded. Exiting 1.");
        exit = 1;
    }

    exit
}

// ── CLI entry point (placeholder / dry-run) ───────────────────────────────────

/// Run `autumn dev-loop-bench --scaling`.
///
/// In `--dry-run` mode prints the budget table and exits. Otherwise the live
/// measurement driver in [`crate::scaling_driver`] is invoked; this function
/// is used only for the dry-run path and for tests (placeholder all-zero
/// samples always pass the budget).
pub fn run_scaling_dry_run() -> i32 {
    print!("{}", format_scaling_budget_table());
    0
}

/// Load a `ScalingBaseline` from a JSON file.
///
/// Returns `Ok(None)` when no path is given, and `Err` when the file cannot be
/// read or parsed — surfacing the error rather than silently skipping the
/// regression gate on a corrupt baseline.
pub fn load_baseline(path: Option<&str>) -> Result<Option<ScalingBaseline>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read baseline file {path:?}: {e}"))?;
    let baseline = serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse baseline file {path:?}: {e}"))?;
    Ok(Some(baseline))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── generate_synthetic_app ────────────────────────────────────────────

    #[test]
    fn generate_app_cargo_toml_contains_package_and_workspace() {
        let app = generate_synthetic_app(3);
        assert!(
            app.cargo_toml.contains("[package]"),
            "Cargo.toml must have [package]"
        );
        assert!(
            app.cargo_toml.contains("[workspace]"),
            "Cargo.toml must declare a standalone [workspace] so [patch] is valid"
        );
        assert!(
            app.cargo_toml.contains("autumn-web"),
            "Cargo.toml must depend on autumn-web"
        );
    }

    #[test]
    fn generate_app_has_five_source_files() {
        let app = generate_synthetic_app(2);
        let paths: Vec<&str> = app.files.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"src/schema.rs"), "missing schema.rs");
        assert!(paths.contains(&"src/models.rs"), "missing models.rs");
        assert!(
            paths.contains(&"src/repositories.rs"),
            "missing repositories.rs"
        );
        assert!(paths.contains(&"src/handlers.rs"), "missing handlers.rs");
        assert!(paths.contains(&"src/main.rs"), "missing main.rs");
        assert_eq!(paths.len(), 5, "expected exactly 5 source files");
    }

    #[test]
    fn generate_app_schema_has_n_tables() {
        let n = 5;
        let app = generate_synthetic_app(n);
        let schema = file_content(&app, "src/schema.rs");
        for k in 0..n {
            assert!(
                schema.contains(&format!("entity_{k} (id)")),
                "schema must define table entity_{k}"
            );
        }
    }

    #[test]
    fn generate_app_models_has_n_structs() {
        let n = 4;
        let app = generate_synthetic_app(n);
        let models = file_content(&app, "src/models.rs");
        for k in 0..n {
            assert!(
                models.contains(&format!("pub struct Entity{k}")),
                "models must define Entity{k}"
            );
        }
    }

    #[test]
    fn generate_app_repositories_has_n_traits() {
        let n = 3;
        let app = generate_synthetic_app(n);
        let repos = file_content(&app, "src/repositories.rs");
        for k in 0..n {
            assert!(
                repos.contains(&format!("pub trait Entity{k}Repository")),
                "repositories must define Entity{k}Repository"
            );
        }
    }

    #[test]
    fn generate_app_handlers_has_n_handlers() {
        let n = 4;
        let app = generate_synthetic_app(n);
        let handlers = file_content(&app, "src/handlers.rs");
        for k in 0..n {
            assert!(
                handlers.contains(&format!("async fn handler_{k}()")),
                "handlers must define handler_{k}"
            );
        }
    }

    #[test]
    fn generate_app_main_routes_list_all_handlers() {
        let n = 3;
        let app = generate_synthetic_app(n);
        let main_src = file_content(&app, "src/main.rs");
        assert!(
            main_src.contains("routes![handler_0, handler_1, handler_2]"),
            "main.rs must register all {n} handlers in routes![], got:\n{main_src}"
        );
    }

    #[test]
    fn generate_app_n1_routes_list_just_handler_0() {
        let app = generate_synthetic_app(1);
        let main_src = file_content(&app, "src/main.rs");
        assert!(
            main_src.contains("routes![handler_0]"),
            "n=1 must have routes![handler_0], got:\n{main_src}"
        );
    }

    #[test]
    fn generate_app_is_deterministic() {
        let a = generate_synthetic_app(10);
        let b = generate_synthetic_app(10);
        assert_eq!(
            a.cargo_toml, b.cargo_toml,
            "Cargo.toml must be deterministic"
        );
        assert_eq!(a.files.len(), b.files.len());
        for ((pa, ca), (pb, cb)) in a.files.iter().zip(b.files.iter()) {
            assert_eq!(pa, pb, "file paths must be deterministic");
            assert_eq!(ca, cb, "file content must be deterministic for {pa}");
        }
    }

    #[test]
    fn generate_app_handlers_contain_bench_edit_sentinel() {
        let app = generate_synthetic_app(2);
        let handlers = file_content(&app, "src/handlers.rs");
        assert!(
            handlers.contains("BENCH_EDIT_SENTINEL"),
            "handlers.rs must contain the BENCH_EDIT_SENTINEL constant"
        );
        assert!(
            handlers.contains("bench_edit_rev_0"),
            "initial revision must be 0"
        );
    }

    #[test]
    fn generate_app_models_use_explicit_table_attr() {
        let app = generate_synthetic_app(2);
        let models = file_content(&app, "src/models.rs");
        assert!(
            models.contains("table = \"entity_0\""),
            "model macro must use explicit table attribute to avoid pluralization guessing"
        );
        assert!(
            models.contains("table = \"entity_1\""),
            "every model must have its own explicit table attribute"
        );
    }

    #[test]
    fn generate_app_schema_uses_reexport_path() {
        // The generated app only depends on autumn-web, not a separate diesel
        // dep. It must use autumn_web::reexports::diesel::table! to declare
        // schema, mirroring the compile-pass test pattern.
        let app = generate_synthetic_app(1);
        let schema = file_content(&app, "src/schema.rs");
        assert!(
            schema.contains("autumn_web::reexports::diesel::table!"),
            "schema must use autumn_web::reexports::diesel::table! (no direct diesel dep)"
        );
    }

    // ── apply_handler_edit ────────────────────────────────────────────────

    #[test]
    fn apply_handler_edit_bumps_sentinel() {
        let app = generate_synthetic_app(2);
        let handlers = file_content(&app, "src/handlers.rs");
        let edited = apply_handler_edit(handlers, 1);
        assert!(
            edited.contains("bench_edit_rev_1"),
            "edited handlers must contain rev 1"
        );
        assert!(
            !edited.contains("bench_edit_rev_0"),
            "old sentinel must be gone after edit"
        );
    }

    #[test]
    fn apply_handler_edit_only_changes_sentinel_line() {
        let app = generate_synthetic_app(3);
        let handlers = file_content(&app, "src/handlers.rs");
        let edited = apply_handler_edit(handlers, 5);
        // Lines other than the sentinel line are unchanged.
        let orig_lines: Vec<&str> = handlers.lines().collect();
        let edit_lines: Vec<&str> = edited.lines().collect();
        assert_eq!(
            orig_lines.len(),
            edit_lines.len(),
            "line count must not change"
        );
        let changed: Vec<usize> = orig_lines
            .iter()
            .zip(edit_lines.iter())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            changed.len(),
            1,
            "exactly one line must change, changed: {changed:?}"
        );
    }

    #[test]
    fn apply_handler_edit_stable_across_multiple_revisions() {
        let app = generate_synthetic_app(2);
        let handlers = file_content(&app, "src/handlers.rs");
        let r1 = apply_handler_edit(handlers, 1);
        let r2 = apply_handler_edit(&r1, 2);
        let r3 = apply_handler_edit(&r2, 3);
        assert!(r3.contains("bench_edit_rev_3"), "final revision must be 3");
        assert!(!r3.contains("bench_edit_rev_1"), "old rev 1 must be gone");
        assert!(!r3.contains("bench_edit_rev_2"), "old rev 2 must be gone");
    }

    // ── parse_sizes ───────────────────────────────────────────────────────

    #[test]
    fn parse_sizes_valid_csv() {
        let sizes = parse_sizes("1,25,50,100").expect("valid CSV must parse");
        assert_eq!(sizes, vec![1, 25, 50, 100]);
    }

    #[test]
    fn parse_sizes_single_value() {
        let sizes = parse_sizes("42").expect("single value must parse");
        assert_eq!(sizes, vec![42]);
    }

    #[test]
    fn parse_sizes_ignores_surrounding_whitespace() {
        let sizes = parse_sizes(" 1 , 10 , 100 ").expect("whitespace-padded CSV must parse");
        assert_eq!(sizes, vec![1, 10, 100]);
    }

    #[test]
    fn parse_sizes_rejects_zero() {
        assert!(parse_sizes("1,0,100").is_err(), "size 0 must be rejected");
    }

    #[test]
    fn parse_sizes_rejects_empty_string() {
        assert!(parse_sizes("").is_err(), "empty string must be rejected");
        assert!(
            parse_sizes("  ").is_err(),
            "whitespace-only must be rejected"
        );
    }

    #[test]
    fn parse_sizes_rejects_non_numeric() {
        assert!(
            parse_sizes("1,abc,100").is_err(),
            "non-numeric token must be rejected"
        );
        assert!(
            parse_sizes("foo").is_err(),
            "all-non-numeric must be rejected"
        );
    }

    #[test]
    fn parse_sizes_rejects_negative() {
        // usize parse rejects leading "-" so this is an Err.
        assert!(
            parse_sizes("1,-5,100").is_err(),
            "negative must be rejected"
        );
    }

    // ── compute_slope ─────────────────────────────────────────────────────

    #[test]
    fn compute_slope_flat() {
        let s = compute_slope(500, 500);
        assert!((s - 1.0).abs() < 1e-9, "equal p95s → slope 1.0, got {s}");
    }

    #[test]
    fn compute_slope_doubled() {
        let s = compute_slope(1000, 2000);
        assert!((s - 2.0).abs() < 1e-9, "doubled p95 → slope 2.0, got {s}");
    }

    #[test]
    fn compute_slope_zero_baseline_returns_one() {
        // Placeholder / empty-sample baseline must not cause divide-by-zero.
        let s = compute_slope(0, 0);
        assert!(
            (s - 1.0).abs() < 1e-9,
            "zero baseline → slope 1.0 (flat), got {s}"
        );
        let s2 = compute_slope(0, 5000);
        assert!(
            (s2 - 1.0).abs() < 1e-9,
            "zero baseline (any max) → slope 1.0, got {s2}"
        );
    }

    #[test]
    fn compute_slope_below_two_passes_gate() {
        let s = compute_slope(1000, 1900);
        assert!(s <= SCALING_SLOPE_MAX, "1.9× must be within the 2× budget");
    }

    #[test]
    fn compute_slope_above_two_fails_gate() {
        let s = compute_slope(1000, 2100);
        assert!(s > SCALING_SLOPE_MAX, "2.1× must exceed the 2× budget");
    }

    // ── build_scaling_report ──────────────────────────────────────────────

    fn make_measurements(sizes: &[usize], p95s: &[u64]) -> Vec<(usize, Vec<u64>)> {
        sizes
            .iter()
            .zip(p95s.iter())
            .map(|(&n, &p95)| (n, vec![p95]))
            .collect()
    }

    #[test]
    fn build_report_all_pass_under_budget() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        assert!(r.points[0].abs_passed, "2000 ms must pass 8000 ms ceiling");
        assert!(r.points[1].abs_passed, "3000 ms must pass 8000 ms ceiling");
        assert!(r.slope_passed, "slope 1.5× must pass 2× budget");
        assert!(r.baseline_regression_passed);
        assert!(r.all_passed);
    }

    #[test]
    fn build_report_fails_abs_p95_exceeded() {
        let m = make_measurements(&[1, 100], &[2000, 9000]);
        let r = build_scaling_report(&m, None);
        assert!(!r.points[1].abs_passed, "9000 ms must fail 8000 ms ceiling");
        assert!(!r.all_passed, "abs failure must fail overall gate");
    }

    #[test]
    fn build_report_fails_slope_exceeded() {
        // slope = 7000 / 1000 = 7.0 > 2.0
        let m = make_measurements(&[1, 100], &[1000, 7000]);
        let r = build_scaling_report(&m, None);
        assert!(!r.slope_passed, "slope 7× must fail 2× budget");
        assert!(!r.all_passed);
    }

    #[test]
    fn build_report_baseline_not_established_always_passes() {
        // slope = 5.0 (violates 2× budget, but baseline not established → reg check skipped)
        // The 2× slope gate still applies regardless of baseline.
        let m = make_measurements(&[1, 100], &[1000, 3000]);
        let baseline = ScalingBaseline {
            established: false,
            accepted_slope: Some(1.2), // won't matter — not established
            note: None,
        };
        let r = build_scaling_report(&m, Some(&baseline));
        assert!(
            r.baseline_regression_passed,
            "non-established baseline must always pass regression"
        );
        // slope = 3.0 still > 2× so all_passed should be false due to slope gate
        assert!(!r.all_passed, "slope 3× still fails 2× absolute slope gate");
    }

    #[test]
    fn build_report_baseline_established_regression_fails() {
        // accepted = 1.2; current slope = 3.0 > 1.2 * 1.2 = 1.44 → regression
        let m = make_measurements(&[1, 100], &[1000, 3000]);
        let baseline = ScalingBaseline {
            established: true,
            accepted_slope: Some(1.2),
            note: None,
        };
        let r = build_scaling_report(&m, Some(&baseline));
        assert!(
            !r.baseline_regression_passed,
            "slope 3× vs accepted 1.2 must fail regression"
        );
        assert!(!r.all_passed);
    }

    #[test]
    fn build_report_baseline_established_regression_passes() {
        // accepted = 1.5; current slope = 1.6 ≤ 1.5 * 1.2 = 1.80 → passes
        let m = make_measurements(&[1, 100], &[1000, 1600]);
        let baseline = ScalingBaseline {
            established: true,
            accepted_slope: Some(1.5),
            note: None,
        };
        let r = build_scaling_report(&m, Some(&baseline));
        assert!(
            r.baseline_regression_passed,
            "1.6 within 1.5×1.2=1.80 must pass"
        );
        assert!(
            r.all_passed,
            "within abs, slope, and regression → all_passed"
        );
    }

    #[test]
    fn build_report_single_size_slope_is_one() {
        // Single size → no meaningful slope → defaults to 1.0 (flat).
        let m = make_measurements(&[50], &[4000]);
        let r = build_scaling_report(&m, None);
        assert!(
            (r.slope - 1.0).abs() < 1e-9,
            "single-size slope must be 1.0"
        );
        assert!(r.slope_passed);
        assert!(r.all_passed);
    }

    #[test]
    fn build_report_empty_measurements_passes() {
        let r = build_scaling_report(&[], None);
        assert!(
            r.all_passed,
            "empty measurement set must pass (nothing to fail)"
        );
    }

    #[test]
    fn build_report_sizes_preserved_in_output() {
        let m = make_measurements(&[1, 25, 50, 100], &[1000, 1200, 1500, 2000]);
        let r = build_scaling_report(&m, None);
        assert_eq!(r.sizes, vec![1, 25, 50, 100]);
    }

    #[test]
    fn build_report_slope_is_size_ordered_not_input_ordered() {
        // Same data, ascending vs. reversed input order. The slope must be
        // p95@N_max / p95@N_min (5000/1000 = 5.0) regardless of input order —
        // a reversed sweep must not invert the ratio and pass the gate.
        let ascending = make_measurements(&[1, 100], &[1000, 5000]);
        let reversed = make_measurements(&[100, 1], &[5000, 1000]);
        let ra = build_scaling_report(&ascending, None);
        let rr = build_scaling_report(&reversed, None);
        assert!((ra.slope - 5.0).abs() < 1e-9, "ascending slope must be 5.0");
        assert!(
            (rr.slope - 5.0).abs() < 1e-9,
            "reversed-input slope must also be 5.0"
        );
        assert!(
            !ra.slope_passed && !rr.slope_passed,
            "slope 5× must fail either way"
        );
        assert!(
            !ra.all_passed && !rr.all_passed,
            "reversed sweep must not pass the gate"
        );
    }

    #[test]
    fn build_report_empty_samples_point_fails() {
        // A size whose measurement produced no samples (build failure) must not
        // be reported as a pass via compute_stats' zero p95.
        let m: Vec<(usize, Vec<u64>)> = vec![(1, vec![2000]), (100, vec![])];
        let r = build_scaling_report(&m, None);
        assert!(r.points[0].abs_passed, "the measured size still passes");
        assert!(
            !r.points[1].abs_passed,
            "an empty-sample size must fail the abs gate"
        );
        assert!(
            !r.all_passed,
            "a missing measurement must fail the overall gate"
        );
    }

    // ── format_scaling_json ───────────────────────────────────────────────

    #[test]
    fn format_json_produces_valid_json() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        let s = format_scaling_json(&r);
        serde_json::from_str::<serde_json::Value>(&s).expect("must be valid JSON");
    }

    #[test]
    fn format_json_contains_env_metadata() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        let s = format_scaling_json(&r);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("timestamp_utc").is_some(), "must have timestamp_utc");
        assert!(v.get("runner_os").is_some(), "must have runner_os");
        assert!(v.get("rust_version").is_some(), "must have rust_version");
        assert!(
            v.get("autumn_version").is_some(),
            "must have autumn_version"
        );
        assert!(v.get("all_passed").is_some(), "must have all_passed");
        assert!(v.get("slope").is_some(), "must have slope");
        assert!(v.get("points").is_some(), "must have points array");
    }

    #[test]
    fn format_json_no_path_leak() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        let s = format_scaling_json(&r);
        assert!(!s.contains("/home/"), "must not leak /home/ paths");
        assert!(!s.contains("C:\\Users\\"), "must not leak Windows paths");
    }

    // ── format_scaling_human ──────────────────────────────────────────────

    #[test]
    fn format_human_shows_slope_and_overall() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        let s = format_scaling_human(&r);
        assert!(s.contains("Slope"), "must mention slope");
        assert!(
            s.contains("PASS") || s.contains("FAIL"),
            "must show PASS or FAIL"
        );
        assert!(s.contains("Overall"), "must show Overall");
    }

    #[test]
    fn format_human_passing_shows_pass() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        let s = format_scaling_human(&r);
        assert!(s.contains("Overall: PASS"), "passing report must show PASS");
    }

    #[test]
    fn format_human_failing_shows_fail() {
        let m = make_measurements(&[1, 100], &[2000, 9000]);
        let r = build_scaling_report(&m, None);
        let s = format_scaling_human(&r);
        assert!(
            s.contains("FAIL"),
            "failing report must show FAIL, got:\n{s}"
        );
    }

    #[test]
    fn format_human_shows_n_values() {
        let m = make_measurements(&[1, 25, 100], &[1000, 1500, 2000]);
        let r = build_scaling_report(&m, None);
        let s = format_scaling_human(&r);
        assert!(s.contains('1'), "must show N=1");
        assert!(s.contains("25"), "must show N=25");
        assert!(s.contains("100"), "must show N=100");
    }

    // ── format_scaling_budget_table ───────────────────────────────────────

    #[test]
    fn budget_table_shows_abs_ceiling() {
        let t = format_scaling_budget_table();
        assert!(
            t.contains("8000") || t.contains("8 000"),
            "budget table must show 8000 ms ceiling, got:\n{t}"
        );
    }

    #[test]
    fn budget_table_shows_slope_max() {
        let t = format_scaling_budget_table();
        assert!(
            t.contains("2.0") || t.contains("2×"),
            "budget table must mention 2× slope max, got:\n{t}"
        );
    }

    #[test]
    fn budget_table_references_docs() {
        let t = format_scaling_budget_table();
        assert!(
            t.contains("dev-loop-latency.md"),
            "budget table must reference docs, got:\n{t}"
        );
    }

    // ── emit_scaling_report ───────────────────────────────────────────────

    #[test]
    fn emit_report_output_writes_valid_json() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_str().unwrap().to_string();
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        let exit = emit_scaling_report(&r, false, Some(&path), false);
        assert_eq!(exit, 0);
        let content = std::fs::read_to_string(&path).expect("report file");
        serde_json::from_str::<serde_json::Value>(&content)
            .expect("output file must be valid JSON");
    }

    #[test]
    fn emit_report_output_write_failure_returns_nonzero() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        let exit = emit_scaling_report(
            &r,
            false,
            Some("/dev/full/nonexistent/path/report.json"),
            false,
        );
        assert_eq!(exit, 1, "failed output write must return 1");
    }

    #[test]
    fn emit_report_fail_on_regression_passing_report_returns_zero() {
        let m = make_measurements(&[1, 100], &[2000, 3000]);
        let r = build_scaling_report(&m, None);
        assert!(r.all_passed);
        let exit = emit_scaling_report(&r, false, None, true);
        assert_eq!(
            exit, 0,
            "--fail-on-regression must not trip on a passing report"
        );
    }

    #[test]
    fn emit_report_fail_on_regression_failing_report_returns_nonzero() {
        let m = make_measurements(&[1, 100], &[2000, 9000]);
        let r = build_scaling_report(&m, None);
        assert!(!r.all_passed);
        let exit = emit_scaling_report(&r, false, None, true);
        assert_eq!(
            exit, 1,
            "--fail-on-regression must return 1 for a failing report"
        );
    }

    // ── run_scaling_dry_run ───────────────────────────────────────────────

    #[test]
    fn run_scaling_dry_run_returns_zero() {
        assert_eq!(run_scaling_dry_run(), 0);
    }

    // ── load_baseline ─────────────────────────────────────────────────────

    #[test]
    fn load_baseline_none_path_returns_none() {
        assert!(load_baseline(None).unwrap().is_none());
    }

    #[test]
    fn load_baseline_missing_file_errors() {
        assert!(load_baseline(Some("/nonexistent/path/baseline.json")).is_err());
    }

    #[test]
    fn load_baseline_valid_json_parses() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(
            tmp.path(),
            r#"{"established":false,"accepted_slope":null,"note":"initial"}"#,
        )
        .expect("write baseline");
        let b = load_baseline(Some(tmp.path().to_str().unwrap()))
            .expect("must parse")
            .expect("must be Some");
        assert!(!b.established);
        assert!(b.accepted_slope.is_none());
    }

    #[test]
    fn load_baseline_established_with_slope_parses() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), r#"{"established":true,"accepted_slope":1.5}"#)
            .expect("write baseline");
        let b = load_baseline(Some(tmp.path().to_str().unwrap()))
            .expect("must parse")
            .expect("must be Some");
        assert!(b.established);
        assert!((b.accepted_slope.unwrap() - 1.5).abs() < 1e-9);
    }

    // ── helper ────────────────────────────────────────────────────────────

    fn file_content<'a>(app: &'a GeneratedApp, relpath: &str) -> &'a str {
        app.files.iter().find(|(p, _)| p == relpath).map_or_else(
            || panic!("file {relpath:?} not found in GeneratedApp"),
            |(_, c)| c.as_str(),
        )
    }
}
