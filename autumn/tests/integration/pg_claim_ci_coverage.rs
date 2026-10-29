//! The Postgres claim Docker tests must stay named in CI (#1720, #3182).
//!
//! These tests are `--lib` tests in `job.rs`'s `mod pg`. The consolidated
//! sweep cannot run them, so ci.yml names each one by full path. A renamed
//! test compiles, but CI then does not run it. This test finds that.

use super::pg_relative_delay_ci_coverage::{pg_tests_module, workspace_root};

/// Full test paths that ci.yml gives as libtest filters.
const CI_FILTERS: [&str; 4] = [
    "job::tests::pg::pg_deferred_claim_skips_parked_names_and_keeps_run_at_order",
    "job::tests::pg::pg_claim_drains_higher_priority_queue_first",
    "job::tests::pg::pg_multi_queue_claim_does_not_lock_lower_queue_heads",
    "job::tests::pg::pg_multi_queue_claim_cost_stays_flat_as_the_backlog_grows",
];

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
        // The test lane and the coverage lane both name it.
        assert!(
            ci.matches(filter).count() >= 2,
            "ci.yml must name `{filter}` in the Docker step and the coverage step"
        );
    }
}
