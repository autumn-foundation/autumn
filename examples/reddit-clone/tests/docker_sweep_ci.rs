//! The app's `#[ignore]`d Docker suites must run in CI (issue #2454).
//!
//! `cargo test --workspace` compiles them and skips them. ci.yml's Docker step
//! must sweep them with a bare `--ignored` run, so a new suite needs no
//! workflow edit.

use std::path::Path;

fn ci_yml() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml");
    std::fs::read_to_string(path).expect("read ci.yml")
}

/// The command lines of the step's reddit-clone sweep.
fn sweep(ci: &str) -> Option<String> {
    let start = ci.find("cargo test -p reddit-clone --lib --tests")?;
    let rest = &ci[start..];
    let end = rest.find("\n          #").unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

#[test]
fn ci_sweeps_the_ignored_suites_of_this_app() {
    let ci = ci_yml();
    let sweep = sweep(&ci).expect("ci.yml must run `cargo test -p reddit-clone --lib --tests`");

    assert!(sweep.contains("--ignored"), "sweep must pass --ignored");
    assert!(
        sweep.contains("--test-threads=1"),
        "suites share one job runtime, so the sweep must run serially"
    );
    assert!(
        sweep.contains("reddit-clone-docker-sweep.log"),
        "sweep must keep its log for the pass-count check"
    );
    assert!(
        ci.contains("no reddit-clone Docker tests ran"),
        "sweep needs a pass-count check: an empty filter exits 0"
    );
}

#[test]
fn the_sweep_has_suites_to_run() {
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
        "expected the Docker suites in tests/ — did the layout change? found {ignored}"
    );
}
