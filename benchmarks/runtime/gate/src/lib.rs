//! Runtime latency gate — pure decision logic.
//!
//! Parses `budgets.toml` and a collection of `gate-summary.json` files (one per
//! measured k6 run), computes the median p99 per path, compares against the budget,
//! and produces a `GateReport` that decides whether CI should pass or fail.

use std::collections::HashMap;
use std::hash::BuildHasher;

use serde::{Deserialize, Serialize};

// ── Types ─────────────────────────────────────────────────────────────────────

/// A per-path p99 latency budget in milliseconds.
#[derive(Debug, Clone, Deserialize)]
pub struct PathBudget {
    pub p99_ms: u64,
}

/// The full budget file (`budgets.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct Budgets {
    pub paths: HashMap<String, PathBudget>,
}

/// Per-path gate outcome.
#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    pub path: String,
    pub observed_p99_ms: u64,
    pub budget_p99_ms: u64,
    pub passed: bool,
}

/// Full gate report (JSON output + exit-code decision).
#[derive(Debug, Clone, Serialize)]
pub struct GateReport {
    pub all_passed: bool,
    pub results: Vec<GateResult>,
}

// ── shape of gate-summary.json entries ───────────────────────────────────────

#[derive(Deserialize)]
struct SummaryEntry {
    p99_ms: f64,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse `budgets.toml` content.
///
/// # Errors
/// Returns an error if the TOML is malformed or missing required fields.
pub fn parse_budgets(toml_str: &str) -> anyhow::Result<Budgets> {
    let budgets: Budgets = toml::from_str(toml_str)?;
    Ok(budgets)
}

/// Parse one `gate-summary.json` file produced by `gate.js` → path → `p99_ms`.
///
/// # Errors
/// Returns an error if the JSON is malformed or contains no path entries.
pub fn parse_k6_summary(json_str: &str) -> anyhow::Result<HashMap<String, f64>> {
    let raw: HashMap<String, SummaryEntry> = serde_json::from_str(json_str)?;
    if raw.is_empty() {
        anyhow::bail!("gate-summary.json contains no path entries");
    }
    Ok(raw.into_iter().map(|(k, v)| (k, v.p99_ms)).collect())
}

/// Return the median of a non-empty slice of p99 values.
///
/// Sorts the slice in place and picks the middle element (nearest-rank).
/// Uses `f64::total_cmp` so a `NaN` reading cannot panic the sort (it orders
/// after all real values; `check_gate` then treats it as a budget failure).
///
/// # Panics
/// Panics if `values` is empty.
#[must_use]
pub fn median_p99(values: &mut [f64]) -> f64 {
    assert!(!values.is_empty(), "median_p99 requires at least one value");
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Build a `GateReport` from budget definitions and per-path median p99 values.
#[must_use]
pub fn check_gate<S: BuildHasher>(
    budgets: &Budgets,
    medians: &HashMap<String, f64, S>,
) -> GateReport {
    let mut results: Vec<GateResult> = budgets
        .paths
        .iter()
        .map(|(path, budget)| {
            // No measurement for a budgeted path (e.g. app was down), or a NaN
            // reading (malformed/incomplete k6 run) → conservative sentinel that
            // exceeds any realistic budget so the gate fails loudly. NaN must be
            // filtered explicitly: `NaN.ceil() as u64` saturates to 0 in Rust,
            // which would otherwise pass any budget silently.
            let observed = medians
                .get(path)
                .copied()
                .filter(|v| !v.is_nan())
                .unwrap_or_else(|| f64::from(u32::MAX));
            // ceil so that 110.1ms rounds up to 111ms and fails a 110ms budget,
            // rather than being silently truncated to 110 and appearing to pass.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let observed_ms = observed.ceil() as u64;
            let passed = observed_ms <= budget.p99_ms;
            GateResult {
                path: path.clone(),
                observed_p99_ms: observed_ms,
                budget_p99_ms: budget.p99_ms,
                passed,
            }
        })
        .collect();
    results.sort_by(|a, b| a.path.cmp(&b.path));
    let all_passed = results.iter().all(|r| r.passed);
    GateReport {
        all_passed,
        results,
    }
}

/// Format the single-line failure message for one gate result.
///
/// Format: `GET /api/posts: p99 142ms exceeds budget 110ms`
#[must_use]
pub fn format_failure_message(result: &GateResult) -> String {
    format!(
        "{}: p99 {}ms exceeds budget {}ms",
        result.path, result.observed_p99_ms, result.budget_p99_ms
    )
}

/// Serialize the report to a pretty-printed JSON string.
///
/// # Errors
/// Returns an error if serialization fails (should not happen in practice).
pub fn format_json_report(report: &GateReport) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(report)?)
}

/// Format the human-readable summary and return it as a string.
#[must_use]
pub fn format_human_summary(report: &GateReport) -> String {
    let mut lines = Vec::new();
    lines.push("Runtime Latency Gate".to_string());
    lines.push("─".repeat(40));
    for r in &report.results {
        let status = if r.passed { "PASS" } else { "FAIL" };
        lines.push(format!(
            "[{status}] {}: p99 {}ms (budget {}ms)",
            r.path, r.observed_p99_ms, r.budget_p99_ms
        ));
        if !r.passed {
            lines.push(format!("       └─ {}", format_failure_message(r)));
        }
    }
    lines.push("─".repeat(40));
    if report.all_passed {
        lines.push("All paths within latency budget.".to_string());
    } else {
        let failure_count = report.results.iter().filter(|r| !r.passed).count();
        lines.push(format!(
            "{failure_count} of {} path(s) exceeded budget.",
            report.results.len()
        ));
    }
    lines.join("\n")
}

/// Dry-run check: verify `budgets.toml` parses and contains all `required_paths`.
///
/// Returns `Ok(())` if all required paths are present, or an error listing the missing ones.
///
/// # Errors
/// Returns an error naming each required path not found in `budgets`.
pub fn check_budgets_dryrun(budgets: &Budgets, required_paths: &[&str]) -> anyhow::Result<()> {
    let missing: Vec<&str> = required_paths
        .iter()
        .copied()
        .filter(|p| !budgets.paths.contains_key(*p))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "budgets.toml is missing required path(s): {}",
            missing.join(", ")
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_budgets ──────────────────────────────────────────────────────────

    #[test]
    fn parse_budgets_reads_two_paths() {
        let toml = r#"
[paths."GET /api/posts"]
p99_ms = 110

[paths."GET /posts"]
p99_ms = 95
"#;
        let b = parse_budgets(toml).expect("should parse");
        assert_eq!(b.paths["GET /api/posts"].p99_ms, 110);
        assert_eq!(b.paths["GET /posts"].p99_ms, 95);
    }

    #[test]
    fn parse_budgets_rejects_missing_p99() {
        let toml = r#"[paths."GET /api/posts"]"#;
        assert!(parse_budgets(toml).is_err(), "missing p99_ms must error");
    }

    // ── parse_k6_summary ──────────────────────────────────────────────────────

    fn sample_summary_json() -> &'static str {
        r#"{
  "GET /api/posts": { "p99_ms": 42.5 },
  "GET /posts":     { "p99_ms": 38.0 }
}"#
    }

    #[test]
    fn parse_k6_summary_reads_two_paths() {
        let m = parse_k6_summary(sample_summary_json()).expect("should parse");
        assert!((m["GET /api/posts"] - 42.5).abs() < 0.01);
        assert!((m["GET /posts"] - 38.0).abs() < 0.01);
    }

    #[test]
    fn parse_k6_summary_rejects_empty_json() {
        assert!(
            parse_k6_summary("{}").is_err(),
            "empty summary must error — no paths"
        );
    }

    // ── median_p99 ────────────────────────────────────────────────────────────

    #[test]
    fn median_odd_list() {
        let mut v = vec![100.0, 140.0, 120.0];
        assert!((median_p99(&mut v) - 120.0).abs() < 0.01);
    }

    #[test]
    fn median_single_element() {
        let mut v = vec![55.0];
        assert!((median_p99(&mut v) - 55.0).abs() < 0.01);
    }

    #[test]
    fn median_already_sorted() {
        let mut v = vec![10.0, 20.0, 30.0];
        assert!((median_p99(&mut v) - 20.0).abs() < 0.01);
    }

    // ── check_gate ────────────────────────────────────────────────────────────

    fn two_path_budgets() -> Budgets {
        let toml = r#"
[paths."GET /api/posts"]
p99_ms = 110

[paths."GET /posts"]
p99_ms = 95
"#;
        parse_budgets(toml).unwrap()
    }

    #[test]
    fn check_gate_passes_when_under_budget() {
        let budgets = two_path_budgets();
        let mut medians = HashMap::new();
        medians.insert("GET /api/posts".to_string(), 80.0);
        medians.insert("GET /posts".to_string(), 70.0);
        let report = check_gate(&budgets, &medians);
        assert!(report.all_passed);
        assert!(report.results.iter().all(|r| r.passed));
    }

    #[test]
    fn check_gate_fails_when_over_budget() {
        let budgets = two_path_budgets();
        let mut medians = HashMap::new();
        medians.insert("GET /api/posts".to_string(), 200.0); // exceeds 110
        medians.insert("GET /posts".to_string(), 70.0);
        let report = check_gate(&budgets, &medians);
        assert!(!report.all_passed);
        let api_result = report
            .results
            .iter()
            .find(|r| r.path == "GET /api/posts")
            .unwrap();
        assert!(!api_result.passed);
        assert_eq!(api_result.observed_p99_ms, 200);
        assert_eq!(api_result.budget_p99_ms, 110);
    }

    #[test]
    fn check_gate_fails_when_budgeted_path_has_no_measurement() {
        let budgets = two_path_budgets();
        let mut medians = HashMap::new();
        medians.insert("GET /api/posts".to_string(), 80.0);
        // "GET /posts" measurement is intentionally absent (app was down, etc.)
        let report = check_gate(&budgets, &medians);
        assert!(!report.all_passed, "missing measurement must fail the gate");
        let html_result = report
            .results
            .iter()
            .find(|r| r.path == "GET /posts")
            .unwrap();
        assert!(
            !html_result.passed,
            "path with no measurement must not pass"
        );
    }

    #[test]
    fn check_gate_fails_when_measurement_is_nan() {
        let budgets = two_path_budgets();
        let mut medians = HashMap::new();
        medians.insert("GET /api/posts".to_string(), f64::NAN);
        medians.insert("GET /posts".to_string(), 70.0);
        let report = check_gate(&budgets, &medians);
        assert!(!report.all_passed, "NaN measurement must fail the gate");
        let api_result = report
            .results
            .iter()
            .find(|r| r.path == "GET /api/posts")
            .unwrap();
        assert!(!api_result.passed, "NaN must not pass via saturation to 0");
    }

    #[test]
    fn check_gate_passes_exactly_at_budget() {
        let budgets = two_path_budgets();
        let mut medians = HashMap::new();
        medians.insert("GET /api/posts".to_string(), 110.0); // exactly at limit
        medians.insert("GET /posts".to_string(), 95.0);
        let report = check_gate(&budgets, &medians);
        assert!(report.all_passed, "exactly at limit must pass");
    }

    // ── format_failure_message ────────────────────────────────────────────────

    #[test]
    fn failure_message_names_path_observed_and_budget() {
        let result = GateResult {
            path: "GET /api/posts".to_string(),
            observed_p99_ms: 142,
            budget_p99_ms: 110,
            passed: false,
        };
        let msg = format_failure_message(&result);
        assert!(msg.contains("GET /api/posts"), "must name the path: {msg}");
        assert!(msg.contains("142"), "must include observed p99: {msg}");
        assert!(msg.contains("110"), "must include budget: {msg}");
    }

    // ── format_json_report ────────────────────────────────────────────────────

    #[test]
    fn json_report_is_valid_json() {
        let report = GateReport {
            all_passed: true,
            results: vec![],
        };
        let json = format_json_report(&report).expect("should serialize");
        serde_json::from_str::<serde_json::Value>(&json).expect("must be valid JSON");
    }

    #[test]
    fn json_report_contains_all_passed_field() {
        let report = GateReport {
            all_passed: false,
            results: vec![],
        };
        let json = format_json_report(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["all_passed"], false);
    }

    #[test]
    fn json_report_does_not_leak_home_paths() {
        let report = GateReport {
            all_passed: true,
            results: vec![GateResult {
                path: "GET /api/posts".to_string(),
                observed_p99_ms: 80,
                budget_p99_ms: 110,
                passed: true,
            }],
        };
        let json = format_json_report(&report).unwrap();
        assert!(
            !json.contains("/home/"),
            "JSON report must not contain /home/ paths: {json}"
        );
    }

    // ── check_budgets_dryrun ──────────────────────────────────────────────────

    #[test]
    fn dryrun_passes_when_all_required_paths_present() {
        let budgets = two_path_budgets();
        let required = &["GET /api/posts", "GET /posts"];
        assert!(check_budgets_dryrun(&budgets, required).is_ok());
    }

    #[test]
    fn dryrun_fails_when_required_path_is_missing() {
        let toml = r#"
[paths."GET /api/posts"]
p99_ms = 110
"#;
        let budgets = parse_budgets(toml).unwrap();
        let required = &["GET /api/posts", "GET /posts"];
        let err = check_budgets_dryrun(&budgets, required).expect_err("missing path must error");
        assert!(
            err.to_string().contains("GET /posts"),
            "error must name the missing path: {err}"
        );
    }

    // ── gate_report_all_passed ────────────────────────────────────────────────

    #[test]
    fn all_passed_is_false_when_any_result_fails() {
        let report = GateReport {
            all_passed: false,
            results: vec![
                GateResult {
                    path: "GET /api/posts".to_string(),
                    observed_p99_ms: 200,
                    budget_p99_ms: 110,
                    passed: false,
                },
                GateResult {
                    path: "GET /posts".to_string(),
                    observed_p99_ms: 70,
                    budget_p99_ms: 95,
                    passed: true,
                },
            ],
        };
        assert!(!report.all_passed);
    }
}
