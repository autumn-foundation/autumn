//! Shared helpers for the `flags` / `config` / `experiments` integration
//! tests (issue #1243). These prove the CLI talks to Postgres natively
//! instead of shelling out to `psql` — they run with `PATH` pointed at an
//! empty directory so a `Command::new("psql")` call would fail immediately.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A distinctive password so tests can assert it never leaks into output.
pub const SECRET_PW: &str = "s3cr3t_pw_do_not_leak";

pub const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// A `PATH` with no directories on it, so `Command::new("psql")` fails with
/// "program not found" instead of accidentally finding the system psql.
/// This is the Windows-parity check available on this (Linux) CI host: the
/// old psql-shelling implementation cannot function under this PATH.
pub fn psql_free_path() -> PathBuf {
    tempfile::tempdir().unwrap().keep()
}

/// Run the autumn binary with `args` and env overrides, with `PATH` pointed
/// at a directory containing no binaries (in particular, no `psql`).
pub fn run_autumn(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> (String, String, Option<i32>) {
    let empty_path = psql_free_path();
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .env("PATH", &empty_path)
        .envs(envs.iter().copied())
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

pub fn run_autumn_ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_eq!(
        code,
        Some(0),
        "autumn {args:?} failed (exit={code:?})\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

pub fn run_autumn_fail(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args, envs);
    assert_ne!(
        code,
        Some(0),
        "autumn {args:?} should have failed but exited 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
}

/// Start a Postgres container with a distinctive password and return its
/// `host:port` alongside the container handle (keep it alive for the test's
/// duration).
pub async fn start_postgres() -> (
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

/// Apply one or more `up.sql` migration bodies against `url` using a native
/// `tokio_postgres` connection (test setup only — production code under test
/// must never shell out to psql, but test *fixtures* connecting directly are
/// fine and in fact mirror the very driver the CLI now uses).
pub async fn apply_sql(url: &str, statements: &[&str]) {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("failed to connect to test Postgres database");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("test fixture connection error: {e}");
        }
    });
    for sql in statements {
        client
            .batch_execute(sql)
            .await
            .unwrap_or_else(|e| panic!("failed to apply fixture SQL: {e}\nSQL: {sql}"));
    }
}

/// Query a single text column back out for assertions.
pub async fn query_one_text(
    url: &str,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Option<String> {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("failed to connect to test Postgres database");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("test fixture connection error: {e}");
        }
    });
    let rows = client.query(sql, params).await.expect("query failed");
    rows.first().map(|r| r.get::<_, String>(0))
}
