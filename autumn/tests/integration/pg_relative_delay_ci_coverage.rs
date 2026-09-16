//! The Postgres relative-delay clock-skew regression tests must stay named in
//! CI (issue #2111 follow-up).
//!
//! `autumn/src/job.rs`'s Postgres relative-delay Docker tests are `--lib`
//! unit tests: they drive crate-private types (`PgDueAt`, `pg_enqueue_job_at`,
//! `pg_enqueue_on_conn_at`), so the consolidated `integration_tests` sweep
//! cannot reach them. ci.yml's Docker step names them by full test path
//! instead of a shared `pg_` prefix — job.rs carries ~30 other ignored
//! `pg_*` tests that prefix would also revive. A renamed test would compile,
//! pass locally, and never run in CI — the pass-count check beside the
//! invocation only catches the whole lane going empty, not one test
//! dropping out.

use std::path::{Path, PathBuf};

/// Full test paths ci.yml's Docker step passes as libtest filters.
const CI_FILTERS: [&str; 2] = [
    "job::tests::pg::pg_relative_delay_computes_run_at_on_the_database_clock",
    "job::tests::pg::pg_on_conn_relative_delay_ignores_how_long_the_transaction_was_already_open",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn pg_relative_delay_tests_are_named_in_ci() {
    let root = workspace_root();
    let source = std::fs::read_to_string(root.join("autumn/src/job.rs")).expect("read job.rs");
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml")).expect("read ci.yml");

    for filter in CI_FILTERS {
        let test_name = filter
            .rsplit("::")
            .next()
            .expect("filter has a trailing test name");
        assert!(
            source.contains(&format!("async fn {test_name}(")),
            "job.rs no longer has a test named `{test_name}` — update CI_FILTERS \
             or the ci.yml filter it names"
        );
        assert!(
            ci.contains(filter),
            "ci.yml no longer passes the `{filter}` libtest filter"
        );
    }
}
