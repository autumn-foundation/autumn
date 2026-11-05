//! Golden tests for `autumn contracts check`.
//!
//! Each case under `tests/golden/contracts/<case>/` holds `current.json`, a
//! release-2 bundle. The deployed side is `release-1.json`, an export of
//! `examples/mesh-catalog` and `examples/mesh-storefront`. The test runs the
//! binary and compares its report with `report.txt` and its exit code with the
//! table below. Set `UPDATE_GOLDEN=1` to write the reports again, then review
//! the diff.

use std::path::{Path, PathBuf};
use std::process::Command;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/contracts")
}

fn check(current: &Path, against: &Path, extra: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(["contracts", "check", "--against"])
        .arg(against)
        .arg("--current")
        .arg(current)
        .args(extra)
        .output()
        .expect("run autumn");
    (
        out.status.code().expect("exit code"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run one case and compare it with its golden report.
fn golden(case: &str, want_exit: i32) -> String {
    let dir = golden_dir();
    let (code, stdout, stderr) = check(
        &dir.join(case).join("current.json"),
        &dir.join("release-1.json"),
        &[],
    );
    let report = dir.join(case).join("report.txt");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&report, &stdout).expect("write golden");
    }
    let expected = std::fs::read_to_string(&report)
        .unwrap_or_else(|e| panic!("missing golden {}: {e}", report.display()));
    assert_eq!(
        stdout, expected,
        "{case}: the report differs from its golden file. If the change is intended, run \
         `UPDATE_GOLDEN=1 cargo test -p autumn-cli --test cli_tests contracts_check` and \
         review the diff."
    );
    assert_eq!(code, want_exit, "{case}: exit code\n{stdout}\n{stderr}");
    stdout
}

#[test]
fn a_removed_response_field_that_a_caller_reads_fails() {
    let report = golden("removed_read_response_field", 2);
    assert!(
        report.contains("reads response field `price_cents`"),
        "{report}"
    );
    assert!(
        report.contains("Deploy mesh-storefront before catalog"),
        "{report}"
    );
}

#[test]
fn a_removed_response_field_that_no_caller_reads_passes() {
    golden("removed_unread_response_field", 0);
}

#[test]
fn a_new_required_request_field_fails() {
    let report = golden("new_required_request_field", 2);
    assert!(report.contains("requires request field `sku`"), "{report}");
}

#[test]
fn a_new_optional_request_field_passes() {
    golden("new_optional_request_field", 0);
}

#[test]
fn a_path_change_fails() {
    let report = golden("path_changed", 1);
    assert!(report.contains("not safe in any deploy order"), "{report}");
}

#[test]
fn a_method_change_fails() {
    golden("method_changed", 1);
}

#[test]
fn a_path_change_that_keeps_the_old_route_names_the_deploy_order() {
    let report = golden("path_changed_old_route_kept", 2);
    assert!(
        report.contains("Deploy catalog before mesh-storefront"),
        "{report}"
    );
}

#[test]
fn a_new_response_field_a_new_caller_reads_names_the_deploy_order() {
    let report = golden("new_response_field_read_by_new_caller", 2);
    assert!(
        report.contains("safe only if the callees ship first"),
        "{report}"
    );
}

#[test]
fn an_unchanged_release_passes() {
    golden("unchanged", 0);
}

#[test]
fn a_declared_safe_order_passes_and_the_other_order_fails() {
    let dir = golden_dir();
    let current = dir.join("path_changed_old_route_kept/current.json");
    let against = dir.join("release-1.json");
    let (code, stdout, _) = check(&current, &against, &["--order", "callee-first"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("Declared order: callee-first. Safe."),
        "{stdout}"
    );
    let (code, stdout, _) = check(&current, &against, &["--order", "caller-first"]);
    assert_eq!(code, 1, "{stdout}");
}

#[test]
fn a_file_that_is_not_a_bundle_fails_with_a_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, b"{\"not\": \"a bundle\"}").expect("write");
    let (code, _, stderr) = check(&bad, &golden_dir().join("release-1.json"), &[]);
    assert_eq!(code, 1);
    assert!(stderr.contains("not a contract bundle"), "{stderr}");
}

#[test]
fn the_golden_release_matches_the_example_release() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/mesh-storefront/contracts/release-1.json");
    assert_eq!(
        std::fs::read(golden_dir().join("release-1.json")).expect("golden"),
        std::fs::read(example).expect("example"),
        "copy examples/mesh-storefront/contracts/release-1.json to the golden dir"
    );
}
