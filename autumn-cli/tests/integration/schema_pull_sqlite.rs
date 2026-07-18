//! `SQLite` integration test for `autumn schema pull` (issue #1975).
//!
//! Compiles and runs **only** under the non-default `sqlite` cargo feature (the
//! backend-flip). A default `cargo test -p autumn-cli` neither compiles nor requires
//! this file — it is `#[cfg(feature = "sqlite")]`-gated in `tests/integration/mod.rs`
//! and additionally guarded here.
//!
//! Unlike the Postgres pull suite this needs **no Docker**: it drives the real
//! binary end-to-end against a temp-FILE `SQLite` database (not `:memory:`, which the
//! runtime rejects for registered migrations):
//!
//!   `cargo test -p autumn-cli --no-default-features --features sqlite --test cli_tests schema_pull_sqlite`
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

/// Overwrite the project's single-file models, dropping any scaffolded
/// `src/models/` directory so the single-file layout resolves.
fn write_models(project: &Path, content: &str) {
    let src = project.join("src");
    let _ = std::fs::remove_dir_all(src.join("models"));
    std::fs::write(src.join("models.rs"), content).expect("write models.rs");
}

/// Two well-behaved models exercising the full clean-round-trip `SQLite` type surface:
/// a `BigSerial` id (→ `INTEGER PRIMARY KEY AUTOINCREMENT`), a unique column (→ a
/// standalone `CREATE UNIQUE INDEX`), a foreign key (`Post.author_id → authors`, →
/// an auto-index + `REFERENCES`), a nullable column, a `f64` (→ `REAL`), plain text
/// columns, and the synthesized `created_at` (→ `TEXT DEFAULT CURRENT_TIMESTAMP`,
/// recovered as a `Timestamp` with a `Now` default).
const MODELS: &str = r"
#[autumn_web::model(managed)]
pub struct Author {
    #[id]
    pub id: i64,
    #[unique]
    pub email: String,
    pub name: String,
}

#[autumn_web::model(managed)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: Option<String>,
    pub rating: f64,
    #[references(authors)]
    pub author_id: i64,
}
";

/// THE ACCEPTANCE: a schema built from `#[model]` structs, migrated into a live
/// `SQLite` file, then `schema pull`ed back, produces a snapshot a subsequent
/// `schema diff` reports as EMPTY — the `SQLite`-introspected snapshot is byte-faithful
/// to what the models produce, so the round-trip is clean. Also asserts `doctor`
/// agrees the database matches the pulled snapshot.
#[test]
fn schema_pull_round_trips_models_through_a_sqlite_file() {
    let (_tmp, project) = fresh_project("pull_sqlite_roundtrip");
    let db_path = project.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let snapshot_path = project.join(".autumn/schema-snapshot.json");

    // 1. Declare the models + an empty baseline snapshot (Sqlite-tagged), generate
    //    the CREATE-TABLE migration, and apply it into the SQLite file.
    write_models(&project, MODELS);
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
    run_autumn_ok(
        &project,
        &["schema", "diff", "--write-migration", "--name", "init"],
        &envs,
    );
    run_autumn_ok(&project, &["schema", "migrate"], &envs);
    assert!(db_path.is_file(), "sqlite db file created");

    // 2. Pull the live SQLite database back into the snapshot, OVERWRITING the
    //    model-derived baseline with the DB-introspected one.
    let (pull_out, _) = run_autumn_ok(&project, &["schema", "pull"], &envs);
    assert!(
        pull_out.contains("pulled schema snapshot") && pull_out.contains("2 table(s)"),
        "pull reports the table count: {pull_out}"
    );

    // 3. The pulled snapshot faithfully carries the SQLite shapes.
    let snap = std::fs::read_to_string(&snapshot_path).expect("pulled snapshot");
    assert!(
        snap.contains("\"backend\": \"Sqlite\""),
        "sqlite-tagged: {snap}"
    );
    assert!(
        snap.contains("\"name\": \"authors\""),
        "authors table: {snap}"
    );
    assert!(snap.contains("\"name\": \"posts\""), "posts table: {snap}");
    // The AUTOINCREMENT id is recovered as a BigSerial serial marker.
    assert!(
        snap.contains("\"serial\": \"BigSerial\""),
        "the INTEGER PRIMARY KEY AUTOINCREMENT id carries the serial marker: {snap}"
    );
    // The FK, the unique index, and the created_at Timestamp recovery all survive.
    assert!(
        snap.contains("\"table\": \"authors\""),
        "the FK references authors: {snap}"
    );
    assert!(
        snap.contains("idx_authors_email_unique"),
        "unique index preserved: {snap}"
    );
    assert!(
        snap.contains("\"Timestamp\"") && snap.contains("\"Now\""),
        "created_at recovered as a Timestamp with a Now default: {snap}"
    );
    // No framework/bookkeeping tables leaked.
    assert!(
        !snap.contains("__diesel_schema_migrations") && !snap.contains("sqlite_sequence"),
        "no framework/internal tables: {snap}"
    );

    // 4. THE ACCEPTANCE: `schema diff` (models vs the pulled snapshot) reports no
    //    changes — the SQLite round-trip is clean.
    let (diff_out, _) = run_autumn_ok(&project, &["schema", "diff"], &envs);
    assert!(
        diff_out.contains("No schema changes"),
        "the SQLite round-trip must yield an empty diff:\n{diff_out}"
    );

    // 5. A re-pull is byte-for-byte stable.
    run_autumn_ok(&project, &["schema", "pull"], &envs);
    let snap_after = std::fs::read_to_string(&snapshot_path).expect("second pull");
    assert_eq!(snap, snap_after, "a re-pull must be byte-for-byte stable");

    // 6. doctor agrees the database matches the pulled snapshot baseline.
    let (doc_out, _) = run_autumn_ok(&project, &["schema", "doctor"], &envs);
    assert!(
        doc_out.contains("database schema matches the snapshot baseline"),
        "doctor reports the SQLite DB matches the snapshot:\n{doc_out}"
    );
}
