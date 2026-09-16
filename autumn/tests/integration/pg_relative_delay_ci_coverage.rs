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
//! dropping out. Checking the bare function name is not enough either: a
//! test moved out of `mod pg` into a different module keeps the same
//! function name, so a bare substring search on the whole file would still
//! find it and call the filter's `job::tests::pg::` prefix satisfied, even
//! though CI's actual libtest filter would then match nothing. This checks
//! the function inside `mod pg`'s own text span instead.

use std::path::{Path, PathBuf};

/// Full test paths ci.yml's Docker step passes as libtest filters.
const CI_FILTERS: [&str; 3] = [
    "job::tests::pg::pg_relative_delay_computes_run_at_on_the_database_clock",
    "job::tests::pg::pg_on_conn_relative_delay_ignores_how_long_the_transaction_was_already_open",
    "job::tests::pg::pg_cancel_classification_uses_the_database_clock_not_the_app_clock",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// The text of `job.rs`'s `mod pg { ... }` block, exclusive of its own
/// opening/closing brace lines.
///
/// `mod pg` sits one level inside `mod tests`, so (rustfmt's consistent
/// 4-space-per-level indent) its own `mod pg {` / closing `}` lines sit at
/// exactly 4 spaces, and everything inside sits at 8+. Bounding on that
/// avoids matching a same-named function some other module might define.
fn pg_tests_module(source: &str) -> String {
    let open = "\n    mod pg {\n";
    let start = source
        .find(open)
        .expect("job.rs no longer has a `mod pg` block inside `mod tests`");
    let body = &source[start + open.len()..];
    let close = "\n    }\n";
    let end = body
        .find(close)
        .expect("could not find the end of job.rs's `mod pg` block");
    body[..end].to_string()
}

#[test]
fn pg_relative_delay_tests_are_named_in_ci() {
    let root = workspace_root();
    // Normalize CRLF to LF: Windows checkouts (`.gitattributes`' `* text=auto`
    // with no override for these files) give `\r\n` line endings, which would
    // never match `pg_tests_module`'s hardcoded `\n`-terminated boundaries.
    let source = std::fs::read_to_string(root.join("autumn/src/job.rs"))
        .expect("read job.rs")
        .replace("\r\n", "\n");
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read ci.yml")
        .replace("\r\n", "\n");
    let pg_mod = pg_tests_module(&source);

    for filter in CI_FILTERS {
        let test_name = filter
            .rsplit("::")
            .next()
            .expect("filter has a trailing test name");
        assert!(
            pg_mod.contains(&format!("async fn {test_name}(")),
            "job.rs's `mod pg` no longer has a test named `{test_name}` — it was renamed, \
             deleted, or moved out of `mod pg` (whose full path, `job::tests::pg::{test_name}`, \
             is exactly what ci.yml's filter targets). Update CI_FILTERS or the ci.yml filter it \
             names"
        );
        assert!(
            ci.contains(filter),
            "ci.yml no longer passes the `{filter}` libtest filter"
        );
    }
}
