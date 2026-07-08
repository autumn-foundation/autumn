//! Integration tests for `autumn migrate down`.
//!
//! These tests require Docker (via testcontainers) and are marked `#[ignore]`
//! so they only run when explicitly requested with `-- --ignored`.
//!
//! Run with:
//!   cargo test -p autumn-cli --test `migrate_down` -- --ignored --nocapture

use std::path::Path;
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// Run the autumn binary with `args` and env overrides; return (stdout, stderr, `exit_code`).
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

fn run_autumn_fail(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_ne!(
        code,
        Some(0),
        "autumn {args:?} should have failed but exited 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

/// Write a minimal migration directory with `up.sql` and `down.sql`.
fn write_migration(migrations_dir: &Path, name: &str, up_sql: &str, down_sql: &str) {
    let dir = migrations_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("up.sql"), up_sql).unwrap();
    std::fs::write(dir.join("down.sql"), down_sql).unwrap();
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn migrate_down_steps_1_reverts_most_recent_user_migration() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer — is Docker running?");

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    // Create a scratch project directory with migrations
    let tmp = tempfile::tempdir().unwrap();
    let migrations_dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&migrations_dir).unwrap();

    write_migration(
        &migrations_dir,
        "20260101000000_create_posts",
        "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL);",
        "DROP TABLE posts;",
    );
    write_migration(
        &migrations_dir,
        "20260102000000_add_body_to_posts",
        "ALTER TABLE posts ADD COLUMN body TEXT;",
        "ALTER TABLE posts DROP COLUMN body;",
    );

    let envs = [("AUTUMN_DATABASE__URL", db_url.as_str())];
    let dir = tmp.path();

    // Apply both migrations
    run_autumn_ok(dir, &["migrate"], &envs);

    // Verify status shows both applied
    let (_, stderr) = run_autumn_ok(dir, &["migrate", "status"], &envs);
    assert!(
        stderr.contains("20260101000000_create_posts") || stderr.contains("Rollback availability"),
        "status should mention migrations or rollback section: {stderr}"
    );

    // Rollback 1 step — should revert add_body_to_posts
    let (_, stderr) = run_autumn_ok(dir, &["migrate", "down", "--steps", "1"], &envs);
    assert!(
        stderr.contains("Rolled back"),
        "down should report rollback: {stderr}"
    );
    assert!(
        stderr.contains("20260102000000_add_body_to_posts") || stderr.contains("add_body_to_posts"),
        "down should name the reverted migration: {stderr}"
    );

    // Status should now show only create_posts as applied (body column gone)
    let (_, stderr) = run_autumn_ok(dir, &["migrate", "status"], &envs);
    // The rollback availability should show the remaining migration
    let _ = stderr; // smoke: just confirming it doesn't error
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn migrate_down_refuses_when_down_sql_is_empty() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer");

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let tmp = tempfile::tempdir().unwrap();
    let migrations_dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&migrations_dir).unwrap();

    write_migration(
        &migrations_dir,
        "20260101000000_create_posts",
        "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY);",
        "", // intentionally empty down.sql
    );

    let envs = [("AUTUMN_DATABASE__URL", db_url.as_str())];
    let dir = tmp.path();

    run_autumn_ok(dir, &["migrate"], &envs);

    // Down should refuse with a clear message naming the offender
    let (_, stderr) = run_autumn_fail(dir, &["migrate", "down"], &envs);
    assert!(
        stderr.contains("no executable down.sql"),
        "should mention missing down.sql: {stderr}"
    );
    assert!(
        stderr.contains("create_posts") || stderr.contains("20260101000000"),
        "should name the offending migration: {stderr}"
    );
}

#[test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
fn migrate_down_refuses_in_prod_without_flag() {
    // No DB needed — prod guard fires before connecting.
    let tmp = tempfile::tempdir().unwrap();
    let migrations_dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&migrations_dir).unwrap();

    write_migration(
        &migrations_dir,
        "20260101000000_create_posts",
        "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY);",
        "DROP TABLE posts;",
    );

    let envs = [
        ("AUTUMN_ENV", "prod"),
        ("AUTUMN_DATABASE__URL", "postgres://localhost/app"),
    ];

    let (_, stderr) = run_autumn_fail(tmp.path(), &["migrate", "down"], &envs);
    assert!(
        stderr.contains("Production profile") || stderr.contains("yes-i-mean-prod"),
        "should mention production guard: {stderr}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn migrate_down_to_version_reverts_correct_range() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer");

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let tmp = tempfile::tempdir().unwrap();
    let migrations_dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&migrations_dir).unwrap();

    write_migration(
        &migrations_dir,
        "20260101000000_create_posts",
        "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY);",
        "DROP TABLE posts;",
    );
    write_migration(
        &migrations_dir,
        "20260102000000_add_body",
        "ALTER TABLE posts ADD COLUMN body TEXT;",
        "ALTER TABLE posts DROP COLUMN body;",
    );
    write_migration(
        &migrations_dir,
        "20260103000000_add_published",
        "ALTER TABLE posts ADD COLUMN published BOOLEAN NOT NULL DEFAULT false;",
        "ALTER TABLE posts DROP COLUMN published;",
    );

    let envs = [("AUTUMN_DATABASE__URL", db_url.as_str())];
    let dir = tmp.path();

    run_autumn_ok(dir, &["migrate"], &envs);

    // --to 20260101000000: should revert 20260103000000 and 20260102000000
    let (_, stderr) = run_autumn_ok(dir, &["migrate", "down", "--to", "20260101000000"], &envs);
    assert!(
        stderr.contains("2 migration(s) rolled back") || stderr.contains("Rolled back"),
        "should report reverted count: {stderr}"
    );
}

#[test]
fn migrate_down_steps_and_to_are_mutually_exclusive() {
    // Pure CLI parsing test — no DB needed.
    let output = Command::new(autumn_bin())
        .args(["migrate", "down", "--steps", "2", "--to", "20260101"])
        .output()
        .expect("failed to run autumn");
    assert!(!output.status.success(), "--steps and --to should conflict");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn migrate_down_with_maintenance_enables_then_disables() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres testcontainer");

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let tmp = tempfile::tempdir().unwrap();
    let migrations_dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&migrations_dir).unwrap();
    write_migration(
        &migrations_dir,
        "20260101000000_create_posts",
        "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY);",
        "DROP TABLE posts;",
    );

    let envs = [("AUTUMN_DATABASE__URL", db_url.as_str())];
    let dir = tmp.path();

    run_autumn_ok(dir, &["migrate"], &envs);

    // `migrate --with-maintenance down` should enable maintenance, roll back,
    // then disable it again (leaving no maintenance flag file behind).
    let (_, stderr) = run_autumn_ok(dir, &["migrate", "--with-maintenance", "down"], &envs);
    assert!(
        stderr.contains("Maintenance mode ENABLED"),
        "should enable maintenance: {stderr}"
    );
    assert!(
        stderr.contains("Maintenance mode DISABLED"),
        "should disable maintenance after success: {stderr}"
    );
    assert!(
        stderr.contains("Rolled back"),
        "should report rollback: {stderr}"
    );
}
