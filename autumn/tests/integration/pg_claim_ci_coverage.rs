//! The Postgres claim Docker tests must stay named in CI (#1720, #3182).
//!
//! These tests are `--lib` tests in `job.rs`'s `mod pg`. The consolidated
//! sweep cannot run them, so ci.yml names each test by full path. If a test
//! gets a new name, the ci.yml count check fails only after a Docker run.
//! This test finds the new name without Docker.

use super::pg_relative_delay_ci_coverage::{pg_tests_module, workspace_root};

/// Full test paths that ci.yml gives as libtest filters.
const CI_FILTERS: [&str; 4] = [
    "job::tests::pg::pg_deferred_claim_skips_parked_names_and_keeps_run_at_order",
    "job::tests::pg::pg_claim_drains_higher_priority_queue_first",
    "job::tests::pg::pg_multi_queue_claim_does_not_lock_lower_queue_heads",
    "job::tests::pg::pg_multi_queue_claim_cost_stays_flat_as_the_backlog_grows",
];

/// The logs of the Docker step and the coverage step that run `CI_FILTERS`.
const CI_LOGS: [&str; 2] = ["/pg-claim.log\"", "/pg-claim-cov.log\""];

#[test]
fn pg_claim_tests_are_named_in_ci() {
    let root = workspace_root();
    let source = std::fs::read_to_string(root.join("autumn/src/job.rs"))
        .expect("read job.rs")
        .replace("\r\n", "\n");
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read ci.yml")
        .replace("\r\n", "\n");
    let pg_mod = pg_tests_module(&source);

    for filter in CI_FILTERS {
        let test_name = filter.rsplit("::").next().expect("a test name");
        assert!(
            pg_mod.contains(&format!("async fn {test_name}(")),
            "job.rs's `mod pg` has no test `{test_name}`. Update CI_FILTERS and ci.yml"
        );
        // The Docker step and the coverage step must both name the test.
        assert!(
            ci.matches(&format!("{filter} \\")).count() >= 2,
            "ci.yml must name `{filter}` in the Docker step and the coverage step"
        );
    }

    // Each step checks for exactly one pass per filter.
    let passes = format!("ok\\. {} passed", CI_FILTERS.len());
    for log in CI_LOGS {
        let check = ci
            .lines()
            .find(|line| line.contains("grep") && line.contains(log))
            .unwrap_or_else(|| panic!("ci.yml has no pass-count check on `{log}`"));
        assert!(
            check.contains(&passes),
            "the check on `{log}` must expect `{passes}`: {check}"
        );
    }
}
