//! Integration tests for `autumn test`.
//!
//! The Docker-backed cases (create / migrate / run lifecycle, `--reset`, exit
//! code propagation) require a Postgres container via testcontainers and are
//! marked `#[ignore]` so they only run when explicitly requested. The help /
//! guard cases need no database and run in the default suite.
//!
//! Run the Docker cases with:
//!   `cargo test -p autumn-cli --test cli_tests test_command -- --ignored`

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
        // Clear any inherited database URL vars so resolution depends only on
        // `envs` — otherwise a machine/CI that exports `DATABASE_URL` would leak
        // a real URL into the "no URL configured" case. Explicit `envs` below
        // still win, since they are applied after these removals.
        .env_remove("DATABASE_URL")
        .env_remove("AUTUMN_DATABASE__URL")
        .env_remove("AUTUMN_DATABASE__PRIMARY_URL")
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

/// Scaffold a minimal, dependency-free Cargo project in `dir` whose single
/// library test either passes or fails, so `autumn test`'s `cargo test` step
/// exercises real exit-code propagation. Also writes one valid migration so the
/// migrate step has work to do.
fn write_cargo_fixture(dir: &Path, test_passes: bool) {
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"autumn_test_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let body = if test_passes {
        "#[test]\nfn fixture_passes() { assert_eq!(2 + 2, 4); }\n"
    } else {
        "#[test]\nfn fixture_fails() { panic!(\"deliberate failure\"); }\n"
    };
    std::fs::write(dir.join("src/lib.rs"), body).unwrap();

    let migrations = dir.join("migrations");
    std::fs::create_dir_all(&migrations).unwrap();
    write_migration(
        &migrations,
        "20260101000000_create_widgets",
        "CREATE TABLE widgets (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);",
        "DROP TABLE widgets;",
    );
}

/// Start a Postgres container with a distinctive password and return its
/// host/port. The per-test target database does **not** yet exist.
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

// ── Non-Docker: help + guard ───────────────────────────────────────────────

#[test]
fn test_help_documents_env_vars_and_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let (stdout, _stderr) = run_autumn_ok(tmp.path(), &["test", "--help"], &[]);
    // AC-8: the help text documents the env vars it sets…
    assert!(stdout.contains("AUTUMN_ENV=test"), "help: {stdout}");
    assert!(stdout.contains("DATABASE_URL"), "help: {stdout}");
    // …and the create → migrate → run lifecycle, plus --reset.
    assert!(stdout.contains("create"), "help: {stdout}");
    assert!(stdout.contains("migrate"), "help: {stdout}");
    assert!(stdout.contains("cargo test"), "help: {stdout}");
    assert!(stdout.contains("--reset"), "help: {stdout}");
}

#[test]
fn test_refuses_url_that_cannot_be_made_test_safe() {
    // AC-6: a target URL with no database name cannot be made test-shaped, so
    // the command refuses with a clear message and a non-zero exit — before it
    // ever connects (so this needs no Docker).
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__PRIMARY_URL", "postgres://localhost/")];
    let (stdout, stderr) = run_autumn_fail(tmp.path(), &["test"], &envs);
    assert!(
        stderr.contains("does not name a database"),
        "stdout: {stdout}\nstderr: {stderr}",
    );
}

#[test]
fn test_refuses_when_no_url_configured() {
    // With no autumn.toml and no DB env vars, there is nothing to resolve.
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr) = run_autumn_fail(tmp.path(), &["test"], &[]);
    assert!(
        stderr.contains("No test database URL found"),
        "stderr: {stderr}",
    );
}

// ── Docker-backed: create → migrate → run lifecycle ────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn test_derives_test_suffix_creates_migrates_and_runs() {
    let (_container, host, port) = start_postgres().await;
    // A bare base name — `autumn test` must derive `myapp_test` (AC-1/AC-2).
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/myapp");
    let envs = [("AUTUMN_DATABASE__PRIMARY_URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_cargo_fixture(dir, true);

    let (stdout, stderr) = run_autumn_ok(dir, &["test"], &envs);
    let combined = format!("{stdout}{stderr}");
    // Derivation targeted the `_test` database and created it.
    assert!(combined.contains("myapp_test"), "output: {combined}");
    assert!(combined.contains("Created database"), "output: {combined}");
    // Migrations ran, and the passing suite made the whole command exit 0.
    assert!(
        combined.contains("Migrations applied") || combined.contains("up to date"),
        "output: {combined}",
    );
    assert!(
        !combined.contains(SECRET_PW),
        "credentials leaked!\n{combined}",
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn test_create_if_missing_is_idempotent_and_preserves_data() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/keepdata_test");
    let envs = [("AUTUMN_DATABASE__PRIMARY_URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_cargo_fixture(dir, true);

    // First run creates + migrates + runs. `keepdata_test` is already
    // test-shaped, so the derived URL is exactly `url`.
    run_autumn_ok(dir, &["test"], &envs);

    // Write a sentinel row into the migrated `widgets` table (created by the
    // fixture migration). If the second run recreated or dropped the database,
    // this row would vanish.
    let sentinel = "sentinel-preserve-me";
    crate::common::execute_params(&url, "INSERT INTO widgets (name) VALUES ($1)", &[&sentinel])
        .await;

    // Second run (no --reset) must leave the existing database intact: the
    // create step reports the idempotent "already exists" notice.
    let (stdout, stderr) = run_autumn_ok(dir, &["test"], &envs);
    let combined = format!("{stdout}{stderr}");
    assert!(combined.contains("already exists"), "output: {combined}");

    // The sentinel row survived — proving the database (and its data) was
    // preserved rather than recreated.
    let survived = crate::common::query_one_text(
        &url,
        "SELECT name FROM widgets WHERE name = $1",
        &[&sentinel],
    )
    .await;
    assert_eq!(
        survived.as_deref(),
        Some(sentinel),
        "sentinel row was lost — the database was not preserved",
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn test_reset_drops_and_recreates() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/resetme_test");
    let envs = [("AUTUMN_DATABASE__PRIMARY_URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_cargo_fixture(dir, true);

    // Seed an initial database.
    run_autumn_ok(dir, &["test"], &envs);

    // --reset drops then recreates (AC-5): both the drop and a fresh create run.
    let (stdout, stderr) = run_autumn_ok(dir, &["test", "--reset"], &envs);
    let combined = format!("{stdout}{stderr}");
    assert!(combined.contains("Dropped database"), "output: {combined}");
    assert!(combined.contains("Created database"), "output: {combined}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn test_failing_suite_fails_the_command() {
    let (_container, host, port) = start_postgres().await;
    let url = format!("postgres://postgres:{SECRET_PW}@{host}:{port}/exitcode_test");
    let envs = [("AUTUMN_DATABASE__PRIMARY_URL", url.as_str())];
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A fixture whose test panics: `cargo test` exits non-zero, so must the
    // command (AC-7), even though create + migrate succeeded.
    write_cargo_fixture(dir, false);

    let (stdout, stderr) = run_autumn_fail(dir, &["test"], &envs);
    let combined = format!("{stdout}{stderr}");
    // Provisioning still happened before the suite failed.
    assert!(combined.contains("Created database"), "output: {combined}");
}
