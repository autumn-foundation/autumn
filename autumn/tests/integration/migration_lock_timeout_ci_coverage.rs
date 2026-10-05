//! The migration `lock_timeout` Docker tests must stay named in CI (#3057).
//!
//! They are `--lib` tests in `autumn/src/migrate.rs`, so the consolidated
//! `integration_tests` sweep does not reach them. ci.yml names each one by
//! full path. A renamed test would compile, pass locally, and never run in CI.

use std::path::{Path, PathBuf};

/// Full test paths that ci.yml's Docker step passes as libtest filters.
const CI_FILTERS: [&str; 4] = [
    "migrate::tests::migration_blocked_by_a_held_lock_fails_fast_after_retries",
    "migrate::tests::migration_retry_applies_after_the_lock_is_released",
    "migrate::tests::non_transactional_migration_waits_without_lock_timeout",
    "migrate::tests::superuser_may_set_lc_messages",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn migration_lock_timeout_tests_are_named_in_ci() {
    let root = workspace_root();
    let source = std::fs::read_to_string(root.join("autumn/src/migrate.rs"))
        .expect("read migrate.rs")
        .replace("\r\n", "\n");
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read ci.yml")
        .replace("\r\n", "\n");
    let tests_start = source
        .find("\nmod tests {\n")
        .expect("migrate.rs has a `mod tests` block");
    let tests = &source[tests_start..];

    for filter in CI_FILTERS {
        let name = filter.rsplit("::").next().expect("filter has a test name");
        assert!(
            tests.contains(&format!("async fn {name}(")),
            "migrate.rs `mod tests` has no test `{name}`. Update CI_FILTERS and ci.yml"
        );
        assert!(
            ci.contains(filter),
            "ci.yml no longer passes the `{filter}` libtest filter"
        );
    }
}
