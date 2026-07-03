//! Integration tests for `autumn experiments` (issue #1243).
//!
//! These prove the command works end-to-end against real Postgres **without**
//! the `psql` binary on `PATH` — the exact failure mode reported on Windows,
//! reproduced here on Linux by pointing `PATH` at an empty directory.
//!
//! Require Docker (via testcontainers); marked `#[ignore]` so they only run
//! when explicitly requested.
//!
//! Run with:
//!   cargo test -p autumn-cli --test experiments -- --ignored --nocapture

mod common;

use common::{apply_sql, run_autumn_fail, run_autumn_ok, start_postgres};

const EXPERIMENTS_DDL: &str =
    include_str!("../../autumn/migrations/20260530300000_create_experiments/up.sql");

async fn start_experiments_db() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
) {
    let (container, host, port) = start_postgres().await;
    let admin_url = format!(
        "postgres://postgres:{}@{host}:{port}/postgres",
        common::SECRET_PW
    );
    apply_sql(&admin_url, &["CREATE DATABASE experiments_app;"]).await;
    let url = format!(
        "postgres://postgres:{}@{host}:{port}/experiments_app",
        common::SECRET_PW
    );
    apply_sql(&url, &[EXPERIMENTS_DDL]).await;
    (container, url)
}

async fn seed_experiment(url: &str, name: &str, variants_json: &str) {
    apply_sql(
        url,
        &[&format!(
            "INSERT INTO autumn_experiments (name, state, variants) VALUES ('{name}', 'running', '{variants_json}'::jsonb);"
        )],
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn experiments_list_and_status_work_without_psql_on_path() {
    let (_container, url) = start_experiments_db().await;
    seed_experiment(
        &url,
        "checkout_flow",
        r#"[{"name":"control","weight":50},{"name":"treatment","weight":50}]"#,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    let (stdout, _) = run_autumn_ok(tmp.path(), &["experiments", "list"], &envs);
    assert!(stdout.contains("checkout_flow"), "stdout: {stdout}");

    let (stdout, _) = run_autumn_ok(
        tmp.path(),
        &["experiments", "status", "checkout_flow"],
        &envs,
    );
    assert!(stdout.contains("running"), "stdout: {stdout}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn experiments_set_weights_without_psql() {
    let (_container, url) = start_experiments_db().await;
    seed_experiment(
        &url,
        "checkout_flow",
        r#"[{"name":"control","weight":50},{"name":"treatment","weight":50}]"#,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(
        tmp.path(),
        &[
            "experiments",
            "set-weights",
            "checkout_flow",
            "control=30,treatment=70",
        ],
        &envs,
    );

    let variants: Option<String> = common::query_one_text(
        &url,
        "SELECT variants::text FROM autumn_experiments WHERE name = $1",
        &[&"checkout_flow"],
    )
    .await;
    let variants = variants.unwrap_or_default();
    assert!(
        variants.contains("30") && variants.contains("70"),
        "variants: {variants}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn experiments_conclude_and_override_without_psql() {
    let (_container, url) = start_experiments_db().await;
    seed_experiment(
        &url,
        "checkout_flow",
        r#"[{"name":"control","weight":50},{"name":"treatment","weight":50}]"#,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_ok(
        tmp.path(),
        &[
            "experiments",
            "override",
            "checkout_flow",
            "user:1",
            "treatment",
        ],
        &envs,
    );
    let variant: Option<String> = common::query_one_text(
        &url,
        "SELECT variant FROM autumn_experiment_overrides WHERE experiment = $1 AND actor = $2",
        &[&"checkout_flow", &"user:1"],
    )
    .await;
    assert_eq!(variant.as_deref(), Some("treatment"));

    run_autumn_ok(
        tmp.path(),
        &["experiments", "conclude", "checkout_flow", "treatment"],
        &envs,
    );
    let state: Option<String> = common::query_one_text(
        &url,
        "SELECT state::text FROM autumn_experiments WHERE name = $1",
        &[&"checkout_flow"],
    )
    .await;
    assert_eq!(state.as_deref(), Some("concluded"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn experiments_conclude_rejects_unknown_winner_without_psql() {
    let (_container, url) = start_experiments_db().await;
    seed_experiment(
        &url,
        "checkout_flow",
        r#"[{"name":"control","weight":50},{"name":"treatment","weight":50}]"#,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let envs = [("AUTUMN_DATABASE__URL", url.as_str())];

    run_autumn_fail(
        tmp.path(),
        &[
            "experiments",
            "conclude",
            "checkout_flow",
            "nonexistent_variant",
        ],
        &envs,
    );

    let state: Option<String> = common::query_one_text(
        &url,
        "SELECT state::text FROM autumn_experiments WHERE name = $1",
        &[&"checkout_flow"],
    )
    .await;
    assert_eq!(
        state.as_deref(),
        Some("running"),
        "must not conclude on invalid winner"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn experiments_never_leak_credentials_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let bad_url = format!("postgres://postgres:{}@127.0.0.1:1/nope", common::SECRET_PW);
    let envs = [("AUTUMN_DATABASE__URL", bad_url.as_str())];

    let (stdout, stderr) = run_autumn_fail(tmp.path(), &["experiments", "list"], &envs);
    assert!(
        !stdout.contains(common::SECRET_PW) && !stderr.contains(common::SECRET_PW),
        "credentials leaked!\nstdout: {stdout}\nstderr: {stderr}",
    );
}
