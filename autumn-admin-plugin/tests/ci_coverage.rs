//! Keep every `#[ignore]`d test binary of this crate named in CI (issue #2108).
//!
//! CI runs no bare `--ignored` sweep over `autumn-admin-plugin`. Compare
//! `autumn`, whose consolidated `integration_tests` binary gets one, and
//! `autumn-cli`, whose `cli_tests` binary got one in #1945 (see CLAUDE.md).
//! Here, an `#[ignore]`d test runs ONLY when a person adds a
//! `--test <name>` line to `.github/workflows/ci.yml`. Nothing read that file
//! until this test, so a new binary could run nowhere and stay green.
//!
//! This test reads both sides and fails when they disagree. It is the same
//! guard `autumn/tests/integration/redis_job_admin_ci_coverage.rs` gives the
//! job-admin Redis tests.

use std::path::{Path, PathBuf};

/// Return the `tests` directory of this crate.
fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Return the CI workflow text.
fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".github/workflows/ci.yml");
    std::fs::read_to_string(&path).expect("read .github/workflows/ci.yml")
}

/// Return the name of every test binary that holds an `#[ignore]`d test.
fn ignored_test_binaries() -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(tests_dir()).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("read test file");
        // The ATTRIBUTE, at the start of a line. A doc comment or a string that
        // names `#[ignore]` — this file does both — is not one.
        let has_ignored = body
            .lines()
            .any(|line| line.trim_start().starts_with("#[ignore"));
        if has_ignored {
            out.push(
                path.file_stem()
                    .expect("file stem")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    out.sort();
    out
}

/// Every `#[ignore]`d binary must appear in a `--test <name>` line for this
/// package.
#[test]
fn every_ignored_test_binary_is_named_in_ci() {
    let ci = workflow();
    let mut missing = Vec::new();
    for name in ignored_test_binaries() {
        if !ci.contains(&format!("--test {name}")) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "these autumn-admin-plugin test binaries hold `#[ignore]`d tests but no \
         `--test <name>` line in .github/workflows/ci.yml, so CI never runs them \
         (this crate gets no bare `--ignored` sweep — see CLAUDE.md):\n  {}",
        missing.join("\n  ")
    );
}

/// The reverse direction: a `--test <name>` line must name a real binary.
///
/// A rename that updates the file but not the workflow leaves a line that
/// matches nothing. `cargo test --test <gone>` fails loudly, but only after CI
/// has spent the whole Docker job getting there.
#[test]
fn every_ci_named_admin_plugin_test_binary_exists() {
    let ci = workflow();
    let mut missing = Vec::new();
    for line in ci.lines() {
        if !line.contains("-p autumn-admin-plugin") {
            continue;
        }
        let Some((_, tail)) = line.split_once("--test ") else {
            continue;
        };
        let name = tail.split_whitespace().next().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if !tests_dir().join(format!("{name}.rs")).exists() {
            missing.push(name.to_owned());
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        ".github/workflows/ci.yml names these autumn-admin-plugin test binaries, \
         but tests/<name>.rs does not exist:\n  {}",
        missing.join("\n  ")
    );
}
