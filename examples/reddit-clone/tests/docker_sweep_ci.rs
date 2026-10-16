//! The app's `#[ignore]`d Docker tests must run in CI (issue #2454).
//!
//! `cargo test --workspace` compiles them but skips them.
//! The ci.yml Docker step must run them with a bare `--ignored`.
//! Then a new test needs no workflow edit.

use std::path::Path;

fn ci_yml() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml");
    // A Windows checkout may use CRLF line endings.
    std::fs::read_to_string(path)
        .expect("read ci.yml")
        .replace("\r\n", "\n")
}

/// The reddit-clone run in the step: its subshell, log check and error line.
fn sweep(ci: &str) -> Option<String> {
    let start = ci.find("cargo test -p reddit-clone --lib --tests")?;
    let begin = ci[..start].rfind("(\n").unwrap_or(start);
    let end = ci[start..].find("exit 1; }")? + start;
    Some(ci[begin..end].to_owned())
}

#[test]
fn ci_runs_the_ignored_tests_of_this_app() {
    let ci = ci_yml();
    let sweep = sweep(&ci).expect("ci.yml must run `cargo test -p reddit-clone --lib --tests`");

    for needle in [
        "--ignored",
        "--test-threads=1",
        "set -o pipefail",
        "tee ",
        "grep -qE",
        "no reddit-clone Docker test ran",
    ] {
        assert!(sweep.contains(needle), "sweep lacks `{needle}`:\n{sweep}");
    }
}

#[test]
fn the_app_has_ignored_tests_to_run() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let ignored = std::fs::read_dir(tests)
        .expect("read tests dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
        .filter(|entry| {
            std::fs::read_to_string(entry.path()).is_ok_and(|src| src.contains("#[ignore"))
        })
        .count();

    assert!(
        ignored >= 5,
        "expected the Docker tests in tests/ — did the layout change? found {ignored}"
    );
}
