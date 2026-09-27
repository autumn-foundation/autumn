//! `SQLite` integration test for the legacy `autumn migrate` verb (issue #2058).
//!
//! Compiles and runs **only** under the non-default `sqlite` cargo feature (the
//! backend-flip). A default `cargo test -p autumn-cli` neither compiles nor
//! requires this file — it is `#[cfg(feature = "sqlite")]`-gated in
//! `tests/integration/mod.rs` and additionally guarded here.
//!
//! Unlike the Postgres path this needs **no Docker** and **no `diesel` CLI**: it
//! drives `autumn migrate` (up) and `autumn migrate down` against a temp-FILE
//! `SQLite` database (not `:memory:`, which the runtime rejects for migrations)
//! through the unlocked in-process harness, so it is a real, runnable end-to-end
//! check:
//!
//!   `cargo test -p autumn-cli --no-default-features --features sqlite --test cli_tests migrate_sqlite`
#![cfg(feature = "sqlite")]

use std::path::Path;
use std::process::Command;

use diesel::prelude::*;
use diesel::sql_query;

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

#[derive(QueryableByName)]
struct TableName {
    #[diesel(sql_type = diesel::sql_types::Text)]
    #[allow(dead_code)]
    name: String,
}

/// Open the file-backed `SQLite` database directly (not through the CLI) and report
/// whether a `posts` table exists — the authoritative schema assertion.
fn posts_table_exists(db_path: &Path) -> bool {
    let mut conn = SqliteConnection::establish(db_path.to_str().expect("utf-8 path"))
        .expect("open sqlite file");
    let rows: Vec<TableName> =
        sql_query("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'posts'")
            .load(&mut conn)
            .expect("query sqlite_master");
    !rows.is_empty()
}

#[test]
fn migrate_up_then_down_against_a_sqlite_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // A real user migration on disk: create/drop a `posts` table.
    let migration = root.join("migrations/20990101000000_create_posts");
    std::fs::create_dir_all(&migration).expect("mkdir migrations");
    std::fs::write(
        migration.join("up.sql"),
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL);\n",
    )
    .expect("write up.sql");
    std::fs::write(migration.join("down.sql"), "DROP TABLE posts;\n").expect("write down.sql");

    // A file-backed SQLite database inside the temp dir (NOT `:memory:` — the
    // runtime rejects in-memory targets for registered migrations).
    let db_path = root.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    // 1. `autumn migrate` (up): applies the user migration through the unlocked
    //    SQLite harness — no advisory lock, no `diesel` CLI subprocess.
    let (_out, err, code) = run_autumn(root, &["migrate"], &envs);
    assert_eq!(code, Some(0), "migrate up failed (exit={code:?})\n{err}");
    assert!(
        err.contains("Migrations applied successfully"),
        "up reports success: {err}"
    );
    assert!(db_path.is_file(), "sqlite db file created");
    assert!(
        posts_table_exists(&db_path),
        "posts table exists after migrate up"
    );

    // 2. A second `autumn migrate` is a clean, idempotent no-op.
    let (_out2, err2, code2) = run_autumn(root, &["migrate"], &envs);
    assert_eq!(code2, Some(0), "second migrate up failed: {err2}");
    assert!(
        err2.contains("already up to date"),
        "second up is a no-op: {err2}"
    );

    // 3. `autumn migrate down`: reverts the user migration through the unlocked
    //    SQLite harness, dropping the table and clearing the applied row.
    let (_out3, err3, code3) = run_autumn(root, &["migrate", "down"], &envs);
    assert_eq!(
        code3,
        Some(0),
        "migrate down failed (exit={code3:?})\n{err3}"
    );
    assert!(
        err3.contains("rolled back"),
        "down reports rollback: {err3}"
    );
    assert!(
        !posts_table_exists(&db_path),
        "posts table is gone after migrate down"
    );

    // 4. After the rollback the migration is pending again — `autumn migrate`
    //    re-applies it, proving the down genuinely removed the applied record.
    let (_out4, err4, code4) = run_autumn(root, &["migrate"], &envs);
    assert_eq!(code4, Some(0), "re-apply after down failed: {err4}");
    assert!(
        err4.contains("Migrations applied successfully"),
        "re-apply after down: {err4}"
    );
    assert!(
        posts_table_exists(&db_path),
        "posts table exists again after re-apply"
    );
}

/// `autumn migrate status` is Postgres-only (it shells out to `diesel migration
/// list` and reads the Postgres migration connection). Under the `SQLite` backend
/// it must fail with a CLEAR provider-appropriate message rather than reaching
/// the `diesel` subprocess and dying with a raw "No such file or directory" OS
/// error — the `diesel` CLI preflight is deliberately skipped for all-`SQLite` runs
/// (issue #2058, codex P2).
#[test]
fn migrate_status_under_sqlite_is_a_clear_provider_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let migration = root.join("migrations/20990101000000_create_posts");
    std::fs::create_dir_all(&migration).expect("mkdir migrations");
    std::fs::write(
        migration.join("up.sql"),
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL);\n",
    )
    .expect("write up.sql");
    std::fs::write(migration.join("down.sql"), "DROP TABLE posts;\n").expect("write down.sql");

    let db_path = root.join("app.db");
    let url = format!("sqlite://{}", db_path.display());
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_out, err, code) = run_autumn(root, &["migrate", "status"], &envs);
    assert_eq!(code, Some(1), "status under sqlite exits non-zero: {err}");
    assert!(
        err.contains("not supported under the SQLite backend"),
        "status under sqlite gives a provider-appropriate message: {err}"
    );
    // It must NOT reach the `diesel` subprocess (the bug it replaces).
    assert!(
        !err.contains("diesel migration list") && !err.contains("os error 2"),
        "status under sqlite must not invoke the diesel subprocess: {err}"
    );
}

/// Write a throwaway `migrations/` dir so the guard is proven to fire BEFORE the
/// apply path (if it didn't, a real migration would create + mutate db files).
fn write_posts_migration(root: &Path) {
    let migration = root.join("migrations/20990101000000_create_posts");
    std::fs::create_dir_all(&migration).expect("mkdir migrations");
    std::fs::write(
        migration.join("up.sql"),
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL);\n",
    )
    .expect("write up.sql");
    std::fs::write(migration.join("down.sql"), "DROP TABLE posts;\n").expect("write down.sql");
}

/// `SQLite` anywhere in a sharded topology is boot-invalid (horizontal sharding is
/// Postgres-only; the runtime `sqlite_sharding_unsupported_guard` rejects it).
/// `autumn migrate` must fail closed with that provider-appropriate `#1614`
/// message BEFORE applying anything, rather than creating + migrating the control
/// and/or shard file for an app that cannot boot (issue #2058, codex). Covers
/// both branches: a `SQLite` control with shards, and a Postgres control with a
/// `SQLite` shard.
#[test]
fn migrate_sqlite_control_with_shards_is_rejected_before_applying() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_posts_migration(root);

    let control_db = root.join("app.db");
    let shard_db = root.join("shard0.db");
    let control_url = format!("sqlite://{}", control_db.display());
    let shard_url = format!("sqlite://{}", shard_db.display());
    let envs = [
        ("AUTUMN_DATABASE__URL", control_url.as_str()),
        ("AUTUMN_DATABASE__SHARDS__0__NAME", "shard0"),
        (
            "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL",
            shard_url.as_str(),
        ),
    ];

    let (_out, err, code) = run_autumn(root, &["migrate"], &envs);
    assert_eq!(
        code,
        Some(1),
        "sqlite control + shards exits non-zero: {err}"
    );
    assert!(
        err.contains("do not support sharding") && err.contains("#1614"),
        "clear provider-appropriate rejection: {err}"
    );
    // Fail-closed: NOTHING was applied to any file — neither db exists.
    assert!(
        !control_db.exists(),
        "control sqlite file must not be created/migrated"
    );
    assert!(
        !shard_db.exists(),
        "shard sqlite file must not be created/migrated"
    );
}

/// Sibling case: a Postgres control with a `SQLite` shard target. The shard loop
/// would route the `sqlite://` shard through the harness and create + migrate the
/// shard file; the guard must reject it before any apply (and never touch the
/// Postgres control either).
#[test]
fn migrate_postgres_control_with_sqlite_shard_is_rejected_before_applying() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_posts_migration(root);

    let shard_db = root.join("shard0.db");
    let shard_url = format!("sqlite://{}", shard_db.display());
    let envs = [
        // A Postgres control URL (never connected to — the guard fires first).
        (
            "AUTUMN_DATABASE__URL",
            "postgres://user@127.0.0.1:1/does_not_exist",
        ),
        ("AUTUMN_DATABASE__SHARDS__0__NAME", "shard0"),
        (
            "AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL",
            shard_url.as_str(),
        ),
    ];

    let (_out, err, code) = run_autumn(root, &["migrate"], &envs);
    assert_eq!(
        code,
        Some(1),
        "postgres control + sqlite shard exits non-zero: {err}"
    );
    assert!(
        err.contains("do not support sharding") && err.contains("shard") && err.contains("#1614"),
        "clear provider-appropriate rejection naming the sqlite shard: {err}"
    );
    // Fail-closed: the sqlite shard file must never be created.
    assert!(
        !shard_db.exists(),
        "sqlite shard file must not be created/migrated"
    );
}
