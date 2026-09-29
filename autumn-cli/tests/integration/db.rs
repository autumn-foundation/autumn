//! Integration tests for `autumn db create | drop | reset`.
//!
//! These tests require Docker (via testcontainers) and are marked `#[ignore]`
//! so they only run when explicitly requested with `-- --ignored`.
//!
//! Run with:
//!   cargo test -p autumn-cli --test db -- --ignored --nocapture

use std::path::Path;
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// A distinctive password so tests can assert it never leaks into output.
const SECRET_PW: &str = "s3cr3t_pw_do_not_leak";

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

/// Start a Postgres container with a distinctive password and return its
/// `host:port`. The default `postgres` maintenance database is present; the
/// per-test target database (named in the URL) does **not** yet exist.
async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
    u16,
) {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .with_password(SECRET_PW)
        .start()
        .await
        .expect("failed to start Postgres testcontainer — is Docker running?");
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (container, host, port)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn db_create_creates_database_and_is_idempotent() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/created_app");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // First create: makes the database.
    let (_, stderr) = run_autumn_ok(dir, &["db", "create"], &envs);
    assert!(stderr.contains("Created database"), "stderr: {stderr}");
    assert!(stderr.contains("created_app"), "stderr: {stderr}");

    // Second create: idempotent notice, still exits 0.
    let (_, stderr) = run_autumn_ok(dir, &["db", "create"], &envs);
    assert!(stderr.contains("already exists"), "stderr: {stderr}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn db_drop_is_idempotent_and_never_leaks_credentials() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/dropped_app");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    run_autumn_ok(dir, &["db", "create"], &envs);

    // Drop under the default (dev) profile: succeeds without --force.
    let (stdout, stderr) = run_autumn_ok(dir, &["db", "drop"], &envs);
    assert!(stderr.contains("Dropped database"), "stderr: {stderr}");
    assert!(
        !stdout.contains(SECRET_PW) && !stderr.contains(SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}",
    );

    // Drop again: idempotent notice, still exits 0.
    let (_, stderr) = run_autumn_ok(dir, &["db", "drop"], &envs);
    assert!(stderr.contains("does not exist"), "stderr: {stderr}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn db_drop_refuses_production_without_force() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/prod_app");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Create under prod (creation is non-destructive, always allowed).
    let prod_envs = [
        ("AUTUMN_DATABASE__URL", url.as_str()),
        ("AUTUMN_ENV", "prod"),
    ];
    run_autumn_ok(dir, &["db", "create"], &prod_envs);

    // Drop under prod without --force: refused, non-zero, no credential leak.
    let (stdout, stderr) = run_autumn_fail(dir, &["db", "drop"], &prod_envs);
    assert!(stderr.contains("Refusing"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");
    assert!(
        !stdout.contains(SECRET_PW) && !stderr.contains(SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}",
    );

    // Drop under prod WITH --force: allowed.
    let (_, stderr) = run_autumn_ok(dir, &["db", "drop", "--force"], &prod_envs);
    assert!(stderr.contains("Dropped database"), "stderr: {stderr}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn db_reset_runs_drop_create_migrate_and_skips_seed() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/reset_app");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // A valid migration so the migrate step has work to do.
    let migrations = dir.join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    write_migration(
        &migrations,
        "20260101000000_create_widgets",
        "CREATE TABLE widgets (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);",
        "DROP TABLE widgets;",
    );

    // Reset from scratch: drop (idempotent no-op) → create → migrate → seed (skipped).
    let (_, stderr) = run_autumn_ok(dir, &["db", "reset"], &envs);
    assert!(stderr.contains("reset: drop"), "stderr: {stderr}");
    assert!(stderr.contains("reset: create"), "stderr: {stderr}");
    assert!(stderr.contains("reset: migrate"), "stderr: {stderr}");
    assert!(
        stderr.contains("skipping the seed step"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("Database reset complete"),
        "stderr: {stderr}"
    );

    // Running it a second time proves the full round-trip is repeatable
    // (drop now actually drops the populated DB, then re-creates + migrates).
    let (_, stderr) = run_autumn_ok(dir, &["db", "reset"], &envs);
    assert!(
        stderr.contains("Database reset complete"),
        "stderr: {stderr}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn db_reset_stops_and_names_failing_migrate_step() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/reset_fail_app");
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // A deliberately broken migration so the migrate step fails.
    let migrations = dir.join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    write_migration(
        &migrations,
        "20260101000000_broken",
        "THIS IS NOT VALID SQL;",
        "SELECT 1;",
    );

    let (stdout, stderr) = run_autumn_fail(dir, &["db", "reset"], &envs);
    assert!(
        stderr.contains("migrate") && stderr.contains("reset failed"),
        "expected reset to name the failing migrate step\nstdout: {stdout}\nstderr: {stderr}",
    );
}
