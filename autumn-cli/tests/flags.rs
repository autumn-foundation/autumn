//! Integration tests for `autumn flags` (issue #1243).
//!
//! These prove the command works end-to-end against real Postgres **without**
//! the `psql` binary on `PATH` — the exact failure mode reported on Windows,
//! reproduced here on Linux by pointing `PATH` at an empty directory.
//!
//! Require Docker (via testcontainers); marked `#[ignore]` so they only run
//! when explicitly requested.
//!
//! Run with:
//!   cargo test -p autumn-cli --test flags -- --ignored --nocapture

mod common;

use common::{apply_sql, run_autumn_fail, run_autumn_ok, start_postgres};

const FEATURE_FLAGS_DDL: &str =
    include_str!("../../autumn/migrations/20260530200000_create_feature_flags/up.sql");

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn flags_list_works_without_psql_on_path() {
    let (_container, url) = start_flags_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (stdout, _stderr) = run_autumn_ok(tmp.path(), &["flags", "list"], &envs);
    assert!(
        stdout.contains("key") || stdout.is_empty() || stdout.contains("no flags"),
        "unexpected output: {stdout}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn flags_enable_disable_round_trip_without_psql() {
    let (_container, url) = start_flags_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (stdout, _) = run_autumn_ok(tmp.path(), &["flags", "enable", "new_checkout"], &envs);
    assert!(stdout.contains("enabled"), "stdout: {stdout}");

    let enabled: Option<String> = common::query_one_text(
        &url,
        "SELECT CASE WHEN enabled THEN 'YES' ELSE 'no' END FROM autumn_feature_flags WHERE key = $1",
        &[&"new_checkout"],
    )
    .await;
    assert_eq!(enabled.as_deref(), Some("YES"));

    let change: Option<String> = common::query_one_text(
        &url,
        "SELECT mutation FROM feature_flag_changes WHERE key = $1 ORDER BY id DESC LIMIT 1",
        &[&"new_checkout"],
    )
    .await;
    assert_eq!(change.as_deref(), Some("enabled"));

    let (stdout, _) = run_autumn_ok(tmp.path(), &["flags", "disable", "new_checkout"], &envs);
    assert!(stdout.contains("disabled"), "stdout: {stdout}");

    let enabled: Option<String> = common::query_one_text(
        &url,
        "SELECT CASE WHEN enabled THEN 'YES' ELSE 'no' END FROM autumn_feature_flags WHERE key = $1",
        &[&"new_checkout"],
    )
    .await;
    assert_eq!(enabled.as_deref(), Some("no"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn flags_set_rollout_and_allow_without_psql() {
    let (_container, url) = start_flags_db().await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(
        tmp.path(),
        &["flags", "set-rollout", "beta_ui", "25"],
        &envs,
    );
    let rollout: Option<String> = common::query_one_text(
        &url,
        "SELECT rollout_pct::text FROM autumn_feature_flags WHERE key = $1",
        &[&"beta_ui"],
    )
    .await;
    assert_eq!(rollout.as_deref(), Some("25"));

    run_autumn_ok(tmp.path(), &["flags", "allow", "beta_ui", "user:42"], &envs);
    let allowlist: Option<String> = common::query_one_text(
        &url,
        "SELECT actor_allowlist FROM autumn_feature_flags WHERE key = $1",
        &[&"beta_ui"],
    )
    .await;
    assert!(
        allowlist.as_deref().unwrap_or_default().contains("user:42"),
        "allowlist: {allowlist:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn flags_never_leak_credentials_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    // Deliberately bad URL: connection fails, but the password must never
    // be echoed anywhere (previously psql error output could).
    let bad_url = format!("postgres://postgres:{}@127.0.0.1:1/nope", common::SECRET_PW);
    let envs = [("AUTUMN_DATABASE__URL", bad_url.as_str())];

    let (stdout, stderr) = run_autumn_fail(tmp.path(), &["flags", "list"], &envs);
    assert!(
        !stdout.contains(common::SECRET_PW) && !stderr.contains(common::SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}",
    );
}

async fn start_flags_db() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
) {
    let (container, host, port) = start_postgres().await;
    let admin_url = format!(
        "postgres://postgres:{}@{host}:{port}/postgres",
        common::SECRET_PW
    );
    apply_sql(&admin_url, &["CREATE DATABASE flags_app;"]).await;
    let url = format!(
        "postgres://postgres:{}@{host}:{port}/flags_app",
        common::SECRET_PW
    );
    apply_sql(&url, &[FEATURE_FLAGS_DDL]).await;
    (container, url)
}
