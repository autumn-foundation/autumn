//! Integration tests for `autumn config` (issue #1243).
//!
//! These prove the command works end-to-end against real Postgres **without**
//! the `psql` binary on `PATH` — the exact failure mode reported on Windows,
//! reproduced here on Linux by pointing `PATH` at an empty directory.
//!
//! Require Docker (via testcontainers); marked `#[ignore]` so they only run
//! when explicitly requested.
//!
//! Run with:
//!   cargo test -p autumn-cli --test config -- --ignored --nocapture

mod common;

use common::{apply_sql, run_autumn_fail, run_autumn_ok, start_postgres};

const RUNTIME_CONFIG_DDL: &str =
    include_str!("../../autumn/migrations/20260530000000_create_runtime_config/up.sql");

async fn start_config_db() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
) {
    let (container, host, port) = start_postgres().await;
    let admin_url = format!(
        "postgres://postgres:{}@{host}:{port}/postgres",
        common::SECRET_PW
    );
    apply_sql(&admin_url, &["CREATE DATABASE config_app;"]).await;
    let url = format!(
        "postgres://postgres:{}@{host}:{port}/config_app",
        common::SECRET_PW
    );
    apply_sql(&url, &[RUNTIME_CONFIG_DDL]).await;
    (container, url)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn config_set_get_unset_round_trip_without_psql() {
    let (_container, url) = start_config_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (_, stderr) = run_autumn_ok(
        tmp.path(),
        &["config", "set", "rate_limit.max_rps", "500"],
        &envs,
    );
    assert!(stderr.contains("Set"), "stderr: {stderr}");

    let (stdout, _) = run_autumn_ok(tmp.path(), &["config", "get", "rate_limit.max_rps"], &envs);
    assert!(stdout.contains("500"), "stdout: {stdout}");

    let value: Option<String> = common::query_one_text(
        &url,
        "SELECT raw_value FROM autumn_runtime_config_values WHERE key = $1",
        &[&"rate_limit.max_rps"],
    )
    .await;
    assert_eq!(value.as_deref(), Some("500"));

    run_autumn_ok(
        tmp.path(),
        &["config", "unset", "rate_limit.max_rps"],
        &envs,
    );

    let (_, stderr) = run_autumn_fail(tmp.path(), &["config", "get", "rate_limit.max_rps"], &envs);
    assert!(stderr.contains("no active override"), "stderr: {stderr}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn config_set_records_audit_history_without_psql() {
    let (_container, url) = start_config_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(tmp.path(), &["config", "set", "feature.x", "a"], &envs);
    run_autumn_ok(tmp.path(), &["config", "set", "feature.x", "b"], &envs);

    let (stdout, _) = run_autumn_ok(tmp.path(), &["config", "history", "feature.x"], &envs);
    assert!(
        stdout.contains('a') && stdout.contains('b'),
        "stdout: {stdout}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn config_list_works_without_psql_on_path() {
    let (_container, url) = start_config_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(tmp.path(), &["config", "set", "a.b", "1"], &envs);
    let (stdout, _) = run_autumn_ok(tmp.path(), &["config", "list"], &envs);
    assert!(stdout.contains("a.b"), "stdout: {stdout}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn config_never_leaks_credentials_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let bad_url = format!("postgres://postgres:{}@127.0.0.1:1/nope", common::SECRET_PW);
    let envs = [("AUTUMN_DATABASE__URL", bad_url.as_str())];

    let (stdout, stderr) = run_autumn_fail(tmp.path(), &["config", "list"], &envs);
    assert!(
        !stdout.contains(common::SECRET_PW) && !stderr.contains(common::SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}",
    );
}
