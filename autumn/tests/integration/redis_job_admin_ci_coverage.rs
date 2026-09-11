//! Every ignored Redis job-admin test must stay named in CI (issue #1186).
//!
//! `autumn/src/job.rs`'s job-admin Docker tests are `--lib` unit tests: they
//! drive crate-private types, so the consolidated `integration_tests` sweep
//! cannot reach them. ci.yml's Docker step names them by two prefix filters
//! instead. A test renamed off those prefixes would compile, pass locally, and
//! never run in CI — the pass-count check beside the invocation only catches
//! the whole lane going empty, not one test dropping out.

use std::path::{Path, PathBuf};

/// Prefixes ci.yml's Docker step passes as libtest filters.
const CI_FILTERS: [&str; 2] = ["job::tests::redis_admin_", "job::tests::redis_job_admin_"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Names of `#[ignore]`d test functions in `job.rs` that look like job-admin
/// Redis tests: a `redis_` name mentioning the admin dashboard.
fn ignored_redis_job_admin_tests(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut ignored = false;
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with("#[ignore") {
            ignored = true;
            continue;
        }
        let Some(rest) = line
            .strip_prefix("async fn ")
            .or_else(|| line.strip_prefix("fn "))
        else {
            // Attributes between `#[ignore]` and the signature keep the flag.
            if !line.starts_with('#') && !line.is_empty() {
                ignored = false;
            }
            continue;
        };
        let name = rest.split('(').next().unwrap_or_default().to_owned();
        if ignored && name.starts_with("redis_") && name.contains("admin") {
            names.push(name);
        }
        ignored = false;
    }
    names
}

#[test]
fn ignored_redis_job_admin_tests_are_named_in_ci() {
    let root = workspace_root();
    let source = std::fs::read_to_string(root.join("autumn/src/job.rs")).expect("read job.rs");
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("read ci.yml");

    for filter in CI_FILTERS {
        assert!(
            ci.contains(filter),
            "ci.yml no longer passes the `{filter}` libtest filter"
        );
    }

    let tests = ignored_redis_job_admin_tests(&source);
    assert!(
        tests.len() >= 5,
        "expected the job-admin Redis Docker tests — did job.rs change shape? found {tests:?}"
    );

    let uncovered: Vec<&String> = tests
        .iter()
        .filter(|name| {
            !CI_FILTERS
                .iter()
                .any(|filter| name.starts_with(filter.trim_start_matches("job::tests::")))
        })
        .collect();
    assert!(
        uncovered.is_empty(),
        "these ignored job-admin Redis tests match no ci.yml filter, so they run nowhere: \
         {uncovered:?}. Rename them back under a covered prefix, or add a filter to ci.yml's \
         \"Run Docker-dependent tests\" step."
    );
}
