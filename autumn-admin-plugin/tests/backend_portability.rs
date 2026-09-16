//! Guard the backend portability of `autumn-admin-plugin` (issue #2108).
//!
//! The crate must compile against `autumn_web::RuntimeConnection`, which is a
//! Postgres connection by default and a `SQLite` connection under
//! `autumn-web/sqlite`. Two diesel constructs break that: the Postgres-only
//! `Timestamptz` SQL type, and the Postgres-only `Array` bind type. This test
//! reads the crate sources and fails when either one comes back.
//!
//! The test is a source scan, not a build. A build under the flipped backend
//! needs `--features autumn-web/sqlite`. No manifest of this crate can request
//! that feature. The feature changes the connection type for the whole
//! workspace, and `scripts/check-sqlite-unification.sh` refuses such an edge
//! from every crate except `autumn-web` and `autumn-cli`. CI runs that build in
//! the `sqlite-runtime` job. This scan runs in the default lane, so a
//! regression fails fast and names the file.

use std::path::{Path, PathBuf};

/// Return the `src` directory of this crate.
fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Return `(path, contents)` for each Rust source file of this crate.
///
/// The walk is recursive. A future `src/models/` subdirectory must not escape
/// the scan.
fn sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let body = std::fs::read_to_string(&path).expect("read source file");
                out.push((path, body));
            }
        }
    }
    let mut out = Vec::new();
    walk(&src_dir(), &mut out);
    assert!(!out.is_empty(), "the crate must have Rust sources");
    out
}

/// Drop a trailing line comment.
///
/// The scan reads code, not prose. Without this, a doc comment that names
/// `Timestamptz` reads as a use of it.
fn code_of(line: &str) -> &str {
    line.split_once("//").map_or(line, |(code, _)| code)
}

/// `Timestamptz` is a Postgres-only SQL type. `SQLite` does not implement
/// `HasSqlType<Timestamptz>`, so every `load` of a row that declares one fails
/// to compile under the flipped backend. Use `Timestamp` with `NaiveDateTime`
/// instead: it is the one timestamp type that both backends share, and
/// Postgres sends `timestamptz` in the same binary form, in UTC.
#[test]
fn no_source_declares_the_postgres_only_timestamptz_type() {
    let mut hits = Vec::new();
    for (path, body) in sources() {
        for (n, line) in body.lines().enumerate() {
            // The bare token, so a `use diesel::sql_types::Timestamptz;` plus a
            // short `sql_type = Timestamptz` cannot slip past.
            if code_of(line).contains("Timestamptz") {
                hits.push(format!("{}:{}", path.display(), n + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "these lines declare the Postgres-only `Timestamptz` SQL type, so the crate \
         cannot compile under `autumn-web/sqlite`. Use `Timestamp` with \
         `NaiveDateTime` (issue #2108):\n  {}",
        hits.join("\n  ")
    );
}

/// `Array` is a Postgres-only bind type. A bulk statement that binds one may
/// stay, but only inside the `pg` arm of `autumn_web::backend_select!`. The
/// macro keeps the tokens of one arm and drops the other, so the Postgres arm
/// is never type-checked under `SQLite`.
///
/// The scan keeps the most recent arm marker. This is sufficient here: an
/// `Array` bind outside a fork reads as arm `none`.
#[test]
fn every_array_bind_sits_in_the_postgres_arm_of_a_backend_fork() {
    let mut hits = Vec::new();
    for (path, body) in sources() {
        // `depth` counts braces from the `backend_select!` line, so the arm
        // resets when the fork closes. A sticky arm would hide every later
        // `Array` bind in the file — including one written after a fork whose
        // `sqlite` arm comes first.
        let mut arm = "none";
        let mut depth: i32 = 0;
        let mut in_fork = false;
        for (n, line) in body.lines().enumerate() {
            let code = code_of(line);
            if !in_fork && code.contains("backend_select!") {
                in_fork = true;
                depth = 0;
                arm = "none";
            }
            if in_fork {
                if code.contains("pg => {") {
                    arm = "pg";
                } else if code.contains("sqlite => {") {
                    arm = "sqlite";
                }
                depth += i32::try_from(code.matches('{').count()).unwrap_or(0);
                depth -= i32::try_from(code.matches('}').count()).unwrap_or(0);
            }
            if code.contains("sql_types::Array<") && arm != "pg" {
                hits.push(format!("{}:{} (arm: {arm})", path.display(), n + 1));
            }
            if in_fork && depth <= 0 && code.contains('}') {
                in_fork = false;
                arm = "none";
            }
        }
    }
    assert!(
        hits.is_empty(),
        "these `Array` binds are Postgres-only and sit outside a \
         `backend_select! {{ pg => …, sqlite => … }}` Postgres arm, so the crate \
         cannot compile under `autumn-web/sqlite` (issue #2108):\n  {}",
        hits.join("\n  ")
    );
}

/// The typed `ExperimentChange` model feeds the grouped-aggregate roll-up on the
/// experiment history page. Its `changed_at` field decides which SQL type the
/// generated DSL binds, so it must be `NaiveDateTime` too.
#[test]
fn the_experiment_change_model_uses_a_portable_timestamp_field() {
    let body = std::fs::read_to_string(src_dir().join("experiments.rs")).expect("experiments.rs");
    assert!(
        body.contains("pub changed_at: chrono::NaiveDateTime"),
        "`ExperimentChange::changed_at` must be `chrono::NaiveDateTime`. \
         `DateTime<Utc>` maps to the Postgres-only `Timestamptz` type and \
         breaks the generated DSL under `autumn-web/sqlite` (issue #2108)"
    );
    assert!(
        body.contains("changed_at -> diesel::sql_types::Timestamp,"),
        "the `autumn_experiment_changes` table! must declare `changed_at` as \
         `Timestamp` (issue #2108)"
    );
}

/// Three bulk actions batch every id into one Postgres statement. The
/// `*_bulk_delete_batch_profile` harnesses measure their cost, and
/// `docs/reports/` records the result. Do not change those statements. A
/// portability fix adds a `SQLite` arm.
#[test]
fn the_batched_postgres_bulk_statements_keep_their_shape() {
    let pinned: [(&str, &str); 3] = [
        (
            "experiments.rs",
            "DELETE FROM autumn_experiments WHERE id = ANY($1) RETURNING name",
        ),
        (
            "feature_flags.rs",
            "DELETE FROM autumn_feature_flags WHERE id = ANY($1) RETURNING key",
        ),
        ("tokens.rs", "WHERE id = ANY($1) AND revoked_at IS NULL"),
    ];
    for (file, fragment) in pinned {
        let raw = std::fs::read_to_string(src_dir().join(file)).expect("source file");
        // Compare on collapsed whitespace, so a re-wrap of the SQL literal does
        // not fail a test about the STATEMENT.
        let body = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            body.contains(fragment),
            "{file} must keep the batched Postgres statement `{fragment}`. \
             The `*_bulk_delete_batch_profile` harness asserts one statement per \
             bulk action (issue #2108)"
        );
    }
}
