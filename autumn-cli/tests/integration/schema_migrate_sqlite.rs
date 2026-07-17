//! SQLite integration test for `autumn schema migrate` (issue #1975 slice 6).
//!
//! Compiles and runs **only** under the non-default `sqlite` cargo feature (the
//! backend-flip). A default `cargo test -p autumn-cli` neither compiles nor
//! requires this file — it is `#[cfg(feature = "sqlite")]`-gated in
//! `tests/integration/mod.rs` and additionally guarded here.
//!
//! Unlike the Postgres path this needs **no Docker**: it applies against a
//! temp-FILE SQLite database (not `:memory:`, which the runtime rejects for
//! migrations), so it is a real, runnable end-to-end check:
//!
//!   `cargo test -p autumn-cli --features sqlite --test cli_tests schema_migrate_sqlite`
#![cfg(feature = "sqlite")]

use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String, Option<i32>) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

fn run_autumn_ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_eq!(
        code,
        Some(0),
        "autumn {args:?} failed (exit={code:?})\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

fn fresh_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name], &[]);
    let project = tmp.path().join(name);
    (tmp, project)
}

/// Overwrite the project's single-file models with a `Post` model and drop any
/// scaffolded `src/models/` directory so the single-file layout resolves.
fn write_post_model(project: &Path) {
    let src = project.join("src");
    let _ = std::fs::remove_dir_all(src.join("models"));
    std::fs::write(
        src.join("models.rs"),
        "#[autumn_web::model(managed)]\npub struct Post {\n    #[id]\n    pub id: i64,\n    pub title: String,\n}\n",
    )
    .expect("write models.rs");
}

#[test]
fn schema_migrate_applies_to_a_sqlite_file_and_refreshes_snapshot() {
    let (_tmp, project) = fresh_project("sqlite_migrate_app");

    // A file-backed SQLite database inside the project (NOT `:memory:` — the
    // runtime rejects in-memory targets for registered migrations).
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    // 1. Post model + empty baseline snapshot (Sqlite-tagged), so the diff
    //    produces a CREATE TABLE migration for SQLite.
    write_post_model(&project);
    std::fs::write(project.join("empty_models.rs"), "").expect("empty models");
    run_autumn_ok(
        &project,
        &[
            "schema",
            "snapshot",
            "--from",
            "empty_models.rs",
            "--backend",
            "sqlite",
        ],
        &envs,
    );

    // 2. Generate the SQLite migration from the diff.
    let (diff_out, _) = run_autumn_ok(
        &project,
        &["schema", "diff", "--write-migration", "--name", "create_posts"],
        &envs,
    );
    assert!(diff_out.contains("wrote migration"), "diff: {diff_out}");

    // 3. Apply against the SQLite file: reports applied, refreshes snapshot,
    //    exit 0. (No advisory lock — SQLite is single-writer, issue #1999.)
    let (mig_out, _) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        mig_out.contains("Applied 1 migration(s)."),
        "migrate stdout: {mig_out}"
    );
    assert!(
        mig_out.contains("refreshed schema snapshot"),
        "snapshot refresh reported: {mig_out}"
    );

    // The SQLite database file exists and the snapshot advanced to Sqlite-tagged
    // `posts`.
    assert!(db_path.is_file(), "sqlite db file created");
    let snap =
        std::fs::read_to_string(project.join(".autumn/schema-snapshot.json")).expect("snapshot");
    assert!(snap.contains("\"backend\": \"Sqlite\""), "snapshot: {snap}");
    assert!(snap.contains("\"name\": \"posts\""), "snapshot: {snap}");

    // 4. A second migrate is a clean no-op.
    let (rerun_out, _) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        rerun_out.contains("Database already up to date."),
        "second migrate: {rerun_out}"
    );
}
