//! Integration tests for `autumn a11y verify` (issue #1932, part of #1706).
//!
//! Drives the real `autumn` binary over a committed red→green fixture pair
//! under `tests/fixtures/a11y/`:
//!
//!   - `red/dashboard.rs`  — raw `html!` markup with four genuine, static
//!     accessibility defects (a labelless `<input>`, a text-less `<button>`,
//!     an `<img>` with no `alt`, and a labelless `<select>`). The verifier must
//!     flag them and exit non-zero.
//!   - `green/dashboard.rs` — the same UI with every defect fixed. The verifier
//!     must report zero findings and exit 0.
//!
//! The fixtures live under `tests/fixtures/` (NOT `src/`) on purpose: CI's a11y
//! gate scans only `autumn/src` + `autumn-cli/src`, so the deliberately-broken
//! RED half never trips the workspace's own accessibility check. The fixture
//! `.rs` files are only ever *tokenized* by the scanner, so they need to be
//! valid Rust that parses but need not compile or link.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// Absolute path to a fixture half (`red` or `green`).
fn fixture_dir(half: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("a11y")
        .join(half)
}

/// Run `autumn a11y verify <path> [args...]` and capture its output.
fn run_verify(path: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("a11y")
        .arg("verify")
        .arg(path)
        .args(args)
        .output()
        .expect("failed to run autumn a11y verify")
}

/// The RED fixture must fail: a non-zero exit code and JSON findings that
/// include each defect's rule id (with two `label` findings for the input and
/// the select).
#[test]
fn red_fixture_is_flagged_with_expected_rules() {
    let out = run_verify(&fixture_dir("red"), &["--format", "json"]);

    assert!(
        !out.status.success(),
        "RED fixture must exit non-zero; got {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("`a11y verify --format json` must emit parseable JSON on stdout");
    let findings = report["findings"]
        .as_array()
        .expect("report must carry a `findings` array");

    let rule_ids: Vec<&str> = findings
        .iter()
        .filter_map(|f| f["rule_id"].as_str())
        .collect();

    // The expected rules are PRESENT (line numbers are intentionally not
    // asserted, so cosmetic fixture edits don't churn the test).
    assert!(
        rule_ids.contains(&"image-alt"),
        "expected an `image-alt` finding; got {rule_ids:?}",
    );
    assert!(
        rule_ids.contains(&"button-name"),
        "expected a `button-name` finding; got {rule_ids:?}",
    );
    let label_findings = rule_ids.iter().filter(|id| **id == "label").count();
    assert!(
        label_findings >= 2,
        "expected at least two `label` findings (input + select); got {rule_ids:?}",
    );
}

/// The GREEN fixture is the same UI with every defect fixed, so `a11y verify`
/// must report no findings and exit 0.
#[test]
fn green_fixture_passes_clean() {
    let out = run_verify(&fixture_dir("green"), &["--format", "json"]);

    assert!(
        out.status.success(),
        "GREEN fixture must exit 0; got {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("`a11y verify --format json` must emit parseable JSON on stdout");
    let findings = report["findings"]
        .as_array()
        .expect("report must carry a `findings` array");

    assert!(
        findings.is_empty(),
        "GREEN fixture must produce zero findings; got {findings:?}",
    );
}
