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

/// Return the `tests` directory of this crate.
fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

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
                // The arm name alone. rustfmt does not reformat inside a
                // macro invocation, so `sqlite =>` and its `{{` can sit on
                // separate lines — and then a brace-anchored marker misses the
                // arm and leaves the tracker on `pg`.
                if code.contains("sqlite =>") {
                    arm = "sqlite";
                } else if code.contains("pg =>") {
                    arm = "pg";
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

/// Return the code of every `pg` arm in `body`, comments removed.
///
/// The same brace-depth walk
/// [`every_array_bind_sits_in_the_postgres_arm_of_a_backend_fork`] uses.
fn postgres_arm_text(body: &str) -> String {
    let mut out = String::new();
    let mut arm = "none";
    let mut depth: i32 = 0;
    let mut in_fork = false;
    for line in body.lines() {
        let code = code_of(line);
        if !in_fork && code.contains("backend_select!") {
            in_fork = true;
            depth = 0;
            arm = "none";
        }
        if in_fork {
            if code.contains("sqlite =>") {
                arm = "sqlite";
            } else if code.contains("pg =>") {
                arm = "pg";
            }
            depth += i32::try_from(code.matches('{').count()).unwrap_or(0);
            depth -= i32::try_from(code.matches('}').count()).unwrap_or(0);
            if arm == "pg" {
                out.push_str(code);
                out.push('\n');
            }
            if depth <= 0 && code.contains('}') {
                in_fork = false;
                arm = "none";
            }
        }
    }
    out
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
        // Only the Postgres arm counts. The statement moving into the `SQLite`
        // arm, or into a comment, must fail this test, not pass it.
        let pg_arm = postgres_arm_text(&raw);
        // Compare on collapsed whitespace, so a re-wrap of the SQL literal does
        // not fail a test about the STATEMENT.
        let body = pg_arm.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            body.contains(fragment),
            "{file} must keep the batched Postgres statement `{fragment}`. \
             in the `pg` arm of its `backend_select!`. The \
             `*_bulk_delete_batch_profile` harness asserts one statement per \
             bulk action (issue #2108)"
        );
    }
}

/// No target of this crate may name `AsyncPgConnection`.
///
/// The issue's own words: the plugin "hardcodes `diesel_async::AsyncPgConnection`
/// … instead of the backend-agnostic `RuntimeConnection` / `RuntimeBackend`
/// aliases". A test target that does so stops compiling under the flip, which
/// narrows CI's `--all-targets` gate to whatever still builds.
#[test]
fn no_target_hardcodes_the_postgres_connection_type() {
    let mut hits = Vec::new();
    let mut files: Vec<PathBuf> = sources().into_iter().map(|(p, _)| p).collect();
    for entry in std::fs::read_dir(tests_dir()).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    for path in files {
        // This file names the type in its own assertion text.
        if path
            .file_name()
            .is_some_and(|f| f == "backend_portability.rs")
        {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("read source file");
        for (n, line) in body.lines().enumerate() {
            if code_of(line).contains("AsyncPgConnection") {
                hits.push(format!("{}:{}", path.display(), n + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "these lines name `AsyncPgConnection` instead of \
         `autumn_web::RuntimeConnection`, so they do not compile under \
         `autumn-web/sqlite` (issue #2108):\n  {}",
        hits.join("\n  ")
    );
}

/// Every method of a Postgres-only model must call `require_postgres` first.
///
/// The three built-in models compile under `autumn-web/sqlite` since issue
/// #2108, so a missing guard is no longer a build error. It is an operator
/// reading `near "ILIKE": syntax error` off a 500 page. Each `AdminModel`
/// method there opens with `Box::pin(async …)`, and the guard is that block's
/// first statement.
#[test]
fn every_postgres_only_model_method_opens_with_the_guard() {
    let mut missing = Vec::new();
    for file in ["tokens.rs", "experiments.rs", "feature_flags.rs"] {
        let body = std::fs::read_to_string(src_dir().join(file)).expect("source file");
        let lines: Vec<&str> = body.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let opener = line.trim();
            if opener != "Box::pin(async move {" && opener != "Box::pin(async {" {
                continue;
            }
            let guarded = lines
                .get(n + 1)
                .is_some_and(|next| next.contains("require_postgres("));
            if !guarded {
                missing.push(format!("{file}:{}", n + 1));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these async bodies in a Postgres-only admin model do not open with \
         `crate::traits::require_postgres(..)`, so on SQLite they would send \
         Postgres SQL to a SQLite driver (issue #2108):\n  {}",
        missing.join("\n  ")
    );
}
