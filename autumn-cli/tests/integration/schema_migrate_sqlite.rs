//! `SQLite` integration test for `autumn schema migrate` (issue #1975 slice 6).
//!
//! Compiles and runs **only** under the non-default `sqlite` cargo feature (the
//! backend-flip). A default `cargo test -p autumn-cli` neither compiles nor
//! requires this file — it is `#[cfg(feature = "sqlite")]`-gated in
//! `tests/integration/mod.rs` and additionally guarded here.
//!
//! Unlike the Postgres path this needs **no Docker**: it applies against a
//! temp-FILE `SQLite` database (not `:memory:`, which the runtime rejects for
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
fn schema_migrate_applies_to_a_sqlite_file_and_advances_snapshot_at_generation() {
    let (_tmp, project) = fresh_project("sqlite_migrate_app");

    // A file-backed SQLite database inside the project (NOT `:memory:` — the
    // runtime rejects in-memory targets for registered migrations).
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

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

    // 2. Generate the SQLite migration from the diff. The snapshot advances HERE
    //    (#2041), to the generated plan's target — Sqlite-tagged `posts`.
    let (diff_out, _) = run_autumn_ok(
        &project,
        &[
            "schema",
            "diff",
            "--write-migration",
            "--name",
            "create_posts",
        ],
        &envs,
    );
    assert!(diff_out.contains("wrote migration"), "diff: {diff_out}");
    assert!(
        diff_out.contains("advanced schema snapshot"),
        "diff --write-migration reports advancing the snapshot: {diff_out}"
    );
    let snap_after_generation = std::fs::read_to_string(&snapshot_path).expect("snapshot");
    assert!(
        snap_after_generation.contains("\"backend\": \"Sqlite\""),
        "snapshot: {snap_after_generation}"
    );
    assert!(
        snap_after_generation.contains("\"name\": \"posts\""),
        "snapshot advanced at generation: {snap_after_generation}"
    );

    // 3. Apply against the SQLite file: reports applied, exit 0, and does NOT
    //    touch the snapshot. (No advisory lock — SQLite is single-writer, #1999.)
    let (mig_out, _) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        mig_out.contains("Applied 1 migration(s)."),
        "migrate stdout: {mig_out}"
    );
    assert!(
        !mig_out.contains("refreshed schema snapshot"),
        "migrate must NOT refresh the snapshot anymore: {mig_out}"
    );
    assert!(db_path.is_file(), "sqlite db file created");
    let snap_after_apply = std::fs::read_to_string(&snapshot_path).expect("snapshot");
    assert_eq!(
        snap_after_generation, snap_after_apply,
        "migrate must leave the snapshot byte-for-byte unchanged"
    );

    // Finding 1 (#2036): introduce a NEW, ungenerated model change (add `body`)
    // WITHOUT generating/applying a migration for it. The next `schema migrate`
    // has nothing pending to apply, so it is a no-op — and MUST leave the
    // snapshot untouched (migrate no longer touches the snapshot at all).
    std::fs::write(
        project.join("src/models.rs"),
        "#[autumn_web::model(managed)]\npub struct Post {\n    #[id]\n    pub id: i64,\n    pub title: String,\n    pub body: Option<String>,\n}\n",
    )
    .expect("edit models.rs with ungenerated drift");

    // 4. A second migrate is a clean no-op — it does NOT report a snapshot
    //    refresh and does NOT advance the on-disk snapshot.
    let (rerun_out, _) = run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(
        rerun_out.contains("Database already up to date."),
        "second migrate: {rerun_out}"
    );
    assert!(
        !rerun_out.contains("refreshed schema snapshot"),
        "a no-op migrate must NOT refresh the snapshot: {rerun_out}"
    );
    let snap_after_noop = std::fs::read_to_string(&snapshot_path).expect("snapshot");
    assert_eq!(
        snap_after_apply, snap_after_noop,
        "a no-op migrate must leave the snapshot byte-for-byte unchanged"
    );
    assert!(
        !snap_after_noop.contains("body"),
        "the ungenerated `body` edit must NOT be baked into the baseline: {snap_after_noop}"
    );

    // 5. Because the snapshot never advanced for the ungenerated edit, it is still
    //    visible as drift: `schema diff` still generates the pending column add.
    let (diff_after, _) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        !diff_after.contains("No schema changes"),
        "drift must still be detected after the no-op migrate: {diff_after}"
    );
    assert!(
        diff_after.contains("body"),
        "the pending `body` column add still diffs: {diff_after}"
    );
}
