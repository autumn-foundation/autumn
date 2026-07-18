//! `bench-runtime-gate` CLI — compare k6 p99 results against a committed budget.
//!
//! # Usage
//!
//! ## Check that budgets.toml covers the required paths (no server needed):
//! ```text
//! bench-runtime-gate --check-budgets --budgets benchmarks/runtime/budgets.toml
//! ```
//!
//! ## Compare 3 measured k6 runs against the budget:
//! ```text
//! bench-runtime-gate \
//!   --budgets benchmarks/runtime/budgets.toml \
//!   --summary run-1/gate-summary.json \
//!   --summary run-2/gate-summary.json \
//!   --summary run-3/gate-summary.json \
//!   --report gate-report.json
//! ```
//!
//! Exits 0 if all paths are within budget, 1 if any path exceeds its budget.

use std::collections::HashMap;
use std::path::PathBuf;

use bench_runtime_gate::{
    check_budgets_dryrun, check_gate, format_human_summary, format_json_report, median_p99,
    parse_budgets, parse_k6_summary,
};

/// Required gated paths — the two read paths from AC #1.
const REQUIRED_PATHS: &[&str] = &["GET /api/posts", "GET /posts"];

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut budgets_path: Option<PathBuf> = None;
    let mut summary_paths: Vec<PathBuf> = Vec::new();
    let mut report_path: Option<PathBuf> = None;
    let mut check_budgets_mode = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--check-budgets" => {
                check_budgets_mode = true;
                i += 1;
            }
            "--budgets" => {
                i += 1;
                budgets_path =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("--budgets requires a path argument")
                    })?));
                i += 1;
            }
            "--summary" => {
                i += 1;
                summary_paths
                    .push(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("--summary requires a path argument")
                    })?));
                i += 1;
            }
            "--report" => {
                i += 1;
                report_path =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("--report requires a path argument")
                    })?));
                i += 1;
            }
            other => {
                anyhow::bail!("unknown argument: {other}");
            }
        }
    }

    let budgets_file = budgets_path.ok_or_else(|| anyhow::anyhow!("--budgets is required"))?;
    let budgets_str = std::fs::read_to_string(&budgets_file)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", budgets_file.display()))?;
    let budgets = parse_budgets(&budgets_str)?;

    if check_budgets_mode {
        check_budgets_dryrun(&budgets, REQUIRED_PATHS)?;
        println!(
            "budgets.toml OK — covers all {} required path(s).",
            REQUIRED_PATHS.len()
        );
        return Ok(());
    }

    if summary_paths.is_empty() {
        anyhow::bail!("at least one --summary file is required (or use --check-budgets)");
    }

    // Parse each summary and collect per-path p99 values across runs.
    let mut per_path_runs: HashMap<String, Vec<f64>> = HashMap::new();
    for path in &summary_paths {
        let json = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let summary = parse_k6_summary(&json)?;
        for (gate_path, p99) in summary {
            per_path_runs.entry(gate_path).or_default().push(p99);
        }
    }

    // Compute median p99 per path across the runs. Every path must appear in
    // every summary file — a path missing from some runs (e.g. the app was down
    // and k6 recorded no samples for it) would otherwise have its median taken
    // over a partial set, letting a transient outage slip past the gate.
    let expected_runs = summary_paths.len();
    let mut medians: HashMap<String, f64> = HashMap::new();
    for (path, runs) in &mut per_path_runs {
        if runs.len() != expected_runs {
            anyhow::bail!(
                "path '{path}' has {} measurement(s), but {expected_runs} were expected \
                 (one per summary file) — a run may have failed to record this path",
                runs.len()
            );
        }
        medians.insert(path.clone(), median_p99(runs));
    }

    let report = check_gate(&budgets, &medians);
    let summary = format_human_summary(&report);
    println!("{summary}");

    if let Some(rp) = report_path {
        let json = format_json_report(&report)?;
        std::fs::write(&rp, json)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", rp.display()))?;
        eprintln!("Report written to: {}", rp.display());
    }

    if !report.all_passed {
        for result in report.results.iter().filter(|r| !r.passed) {
            eprintln!(
                "::error::{}",
                bench_runtime_gate::format_failure_message(result)
            );
        }
        std::process::exit(1);
    }

    Ok(())
}
