//! `autumn db backup` / `autumn db restore` against a `SQLite` app, end to end
//! through the real binary (issue #1909).
//!
//! The acceptance criterion is the zero-ops one: a `SQLite` deployment is one
//! binary and one data file, so a backup must work with **no external tools**.
//! This drives the shipped `autumn` binary with an EMPTY `PATH` and an empty
//! `AUTUMN_PG_BIN_DIR`, so `pg_dump`/`pg_restore` are unreachable by
//! construction. A regression to shelling out fails this test, even on a machine
//! with Postgres installed.
//!
//! No Docker: `SQLite` is a file.
//!
//! Deliberately NOT `#[cfg(feature = "sqlite")]`-gated, unlike its
//! `migrate_sqlite` neighbours. This path needs no backend flip (diesel's `SQLite`
//! backend is in the default graph), and gating it would leave the feature
//! untested in the only lane CI builds.

use std::path::Path;
use std::process::Command;

use diesel::connection::SimpleConnection as _;
use diesel::{Connection as _, QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

fn open(db: &Path) -> SqliteConnection {
    SqliteConnection::establish(&db.to_string_lossy()).expect("open sqlite")
}

fn row_count(db: &Path) -> i64 {
    let rows: Vec<Count> = sql_query("SELECT COUNT(*) AS n FROM notes")
        .load(&mut open(db))
        .expect("count");
    rows.first().map_or(-1, |r| r.n)
}

/// Run the real binary in `dir` with no PATH, so no Postgres client tool can be
/// found even if one is installed on the machine.
fn run_autumn(dir: &Path, args: &[&str]) -> (String, Option<i32>) {
    let empty = dir.join("no-tools");
    std::fs::create_dir_all(&empty).expect("mkdir");
    let out = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .env("PATH", &empty)
        .env("AUTUMN_PG_BIN_DIR", &empty)
        .env("AUTUMN_ENV", "dev")
        .output()
        .expect("run autumn");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.code())
}

/// The only backup run directory under `backups/dev`.
fn only_run_dir(root: &Path) -> std::path::PathBuf {
    let mut runs: Vec<_> = std::fs::read_dir(root.join("backups").join("dev"))
        .expect("profile dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(runs.len(), 1, "exactly one run directory: {runs:?}");
    runs.pop().expect("one run")
}

#[test]
fn sqlite_backup_and_restore_round_trip_without_any_postgres_tools() {
    let project = tempfile::tempdir().expect("tempdir");
    let root = project.path();
    std::fs::write(
        root.join("autumn.toml"),
        "[database]\nurl = \"sqlite://app.db\"\n",
    )
    .expect("write autumn.toml");

    let db = root.join("app.db");
    open(&db)
        .batch_execute(
            "PRAGMA journal_mode = WAL; \
             CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
             INSERT INTO notes (body) VALUES ('first'), ('second');",
        )
        .expect("seed");
    assert_eq!(row_count(&db), 2);

    let (out, code) = run_autumn(root, &["db", "backup", "--profile", "dev"]);
    assert_eq!(code, Some(0), "backup must succeed with no PATH:\n{out}");
    assert!(
        out.contains("SQLite snapshot"),
        "the run must report the snapshot it took:\n{out}"
    );

    let run_dir = only_run_dir(root);
    let artifact = run_dir.join("control.sqlite");
    assert!(artifact.is_file(), "artifact missing in {run_dir:?}");
    let manifest = std::fs::read_to_string(run_dir.join("manifest.json")).expect("manifest");
    assert!(
        manifest.contains("\"backend\": \"sqlite\""),
        "the manifest must record the backend:\n{manifest}"
    );
    // The artifact is a real SQLite database, readable without the CLI.
    assert_eq!(row_count(&artifact), 2);

    // Data loss, then restore.
    open(&db)
        .batch_execute("DELETE FROM notes;")
        .expect("delete");
    assert_eq!(row_count(&db), 0);

    let (out, code) = run_autumn(
        root,
        &[
            "db",
            "restore",
            run_dir.to_str().expect("utf-8"),
            "--profile",
            "dev",
        ],
    );
    assert_eq!(code, Some(0), "restore must succeed with no PATH:\n{out}");
    assert_eq!(row_count(&db), 2, "the restore must bring the rows back");
}

/// A backup of a database that is being written to must not corrupt or block the
/// app — the online-safety criterion. The writer keeps its connection (and its
/// `-wal`) open for the whole backup.
#[test]
fn a_live_writer_is_neither_blocked_nor_corrupted_by_a_backup() {
    let project = tempfile::tempdir().expect("tempdir");
    let root = project.path();
    std::fs::write(
        root.join("autumn.toml"),
        "[database]\nurl = \"sqlite://app.db\"\n",
    )
    .expect("write autumn.toml");

    let db = root.join("app.db");
    let mut live = open(&db);
    live.batch_execute(
        "PRAGMA journal_mode = WAL; \
         CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
         INSERT INTO notes (body) VALUES ('first');",
    )
    .expect("seed");

    let (out, code) = run_autumn(root, &["db", "backup", "--profile", "dev"]);
    assert_eq!(code, Some(0), "a live database must back up:\n{out}");

    // The app keeps writing afterwards, and the database is still sound.
    live.batch_execute("INSERT INTO notes (body) VALUES ('second');")
        .expect("the live app must still write after a backup");
    assert_eq!(row_count(&db), 2);

    let snapshot = only_run_dir(root).join("control.sqlite");
    assert_eq!(
        row_count(&snapshot),
        1,
        "the snapshot is the point in time the backup ran"
    );
}

/// An in-memory database cannot be snapshotted, and saying so beats writing an
/// artifact that describes nothing.
#[test]
fn an_in_memory_database_is_refused_with_an_actionable_message() {
    let project = tempfile::tempdir().expect("tempdir");
    let root = project.path();
    std::fs::write(
        root.join("autumn.toml"),
        "[database]\nurl = \"sqlite::memory:\"\n",
    )
    .expect("write autumn.toml");

    let (out, code) = run_autumn(root, &["db", "backup", "--profile", "dev"]);
    assert_eq!(code, Some(1), "an in-memory target must fail:\n{out}");
    assert!(
        out.contains("in-memory") && out.contains("sqlite://"),
        "the refusal must name the reason and the fix:\n{out}"
    );
    assert!(
        !root.join("backups").join("dev").exists()
            || std::fs::read_dir(root.join("backups").join("dev"))
                .into_iter()
                .flatten()
                .next()
                .is_none(),
        "a failed backup must leave no run directory behind"
    );
}
