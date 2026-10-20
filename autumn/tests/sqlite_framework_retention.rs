//! The unified retention sweeps against a `SQLite`-backed app (issue #2553).
//!
//! `autumn db retention` runs the app binary, so the runtime sweeps are what it
//! reports. These tests run the real engine (`run_retention`) over a file
//! database that the real framework migrations built. No Docker.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]`, so a default `cargo test` compiles it to an
//! empty (passing) binary. Run:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_framework_retention`.
#![cfg(feature = "sqlite")]

use autumn_web::AppState;
use autumn_web::config::{AutumnConfig, DatabaseConfig};
use autumn_web::data_retention::{RetentionRunOptions, run_retention};
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};
use autumn_web::migrate::{FRAMEWORK_MIGRATIONS, run_pending_sqlite};
use autumn_web::reexports::diesel;

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

const DAY_MS: i64 = 86_400_000;

/// The job table, as `job/sqlite.rs::ensure_schema` creates it. The runtime
/// owns this table, so the framework migrations only shim it.
const JOBS_DDL: &str = "CREATE TABLE autumn_jobs ( \
    id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, \
    queue TEXT NOT NULL DEFAULT 'default', payload TEXT NOT NULL DEFAULT '{}', \
    status TEXT NOT NULL DEFAULT 'enqueued', attempt INTEGER NOT NULL DEFAULT 1, \
    max_attempts INTEGER NOT NULL DEFAULT 5, initial_backoff_ms BIGINT NOT NULL DEFAULT 250, \
    enqueued_at BIGINT NOT NULL DEFAULT 0, run_at BIGINT NOT NULL DEFAULT 0, \
    started_at BIGINT, finished_at BIGINT, claimed_by TEXT, claimed_at BIGINT, \
    last_error TEXT, unique_key TEXT, unique_window TEXT, unique_ttl_ms BIGINT, \
    pending_unique_key TEXT, concurrency_key TEXT, concurrency_limit INTEGER, \
    traceparent TEXT, tracestate TEXT)";

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

#[derive(diesel::QueryableByName)]
struct Value {
    #[diesel(sql_type = diesel::sql_types::Text)]
    v: String,
}

struct Fixture {
    pool: SqlitePool,
    _dir: tempfile::TempDir,
}

async fn start() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}", dir.path().join("app.db").display());
    run_pending_sqlite(&url, FRAMEWORK_MIGRATIONS).expect("framework migrations");
    let pool: SqlitePool = create_pool(&DatabaseConfig {
        url: Some(url),
        primary_pool_size: Some(2),
        ..Default::default()
    })
    .expect("pool builds")
    .expect("a url is configured");
    exec(&pool, JOBS_DDL).await;
    Fixture { pool, _dir: dir }
}

async fn exec(pool: &SqlitePool, sql: &str) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .execute(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    let rows: Vec<Count> = diesel::sql_query(sql)
        .load(&mut *conn)
        .await
        .expect("count");
    rows[0].n
}

async fn values(pool: &SqlitePool, sql: &str) -> Vec<String> {
    let mut conn = pool.get().await.expect("conn");
    let rows: Vec<Value> = diesel::sql_query(sql)
        .load(&mut *conn)
        .await
        .expect("values");
    rows.into_iter().map(|r| r.v).collect()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn insert_job(pool: &SqlitePool, id: &str, status: &str, age_days: i64) {
    let at = now_ms() - age_days * DAY_MS;
    let finished = if status == "enqueued" || status == "running" {
        "NULL".to_owned()
    } else {
        at.to_string()
    };
    exec(
        pool,
        &format!(
            "INSERT INTO autumn_jobs (id, name, status, enqueued_at, finished_at) \
             VALUES ('{id}', 'demo', '{status}', {at}, {finished})"
        ),
    )
    .await;
}

/// A terminal job that still holds a TTL dedup key of `ttl_days`.
async fn insert_ttl_unique_job(pool: &SqlitePool, id: &str, age_days: i64, ttl_days: i64) {
    let at = now_ms() - age_days * DAY_MS;
    exec(
        pool,
        &format!(
            "INSERT INTO autumn_jobs (id, name, status, enqueued_at, finished_at, \
                 unique_key, unique_window, unique_ttl_ms) \
             VALUES ('{id}', 'welcome', 'completed', {at}, {at}, '{id}', 'ttl', {})",
            ttl_days * DAY_MS
        ),
    )
    .await;
}

async fn insert_hook(pool: &SqlitePool, id: &str, status: &str, age_days: i64) {
    exec(
        pool,
        &format!(
            "INSERT INTO autumn_repository_commit_hooks \
                 (id, handler_key, hook_name, status, enqueued_at, finished_at) \
             VALUES ('{id}', 'h', 'after_save', '{status}', \
                 datetime('now', '-{age_days} days'), datetime('now', '-{age_days} days'))"
        ),
    )
    .await;
}

async fn insert_assignment(pool: &SqlitePool, experiment: &str, actor: &str, age_days: i64) {
    exec(
        pool,
        &format!(
            "INSERT INTO autumn_experiment_assignments (experiment, actor, variant, assigned_at) \
             VALUES ('{experiment}', '{actor}', 'control', datetime('now', '-{age_days} days'))"
        ),
    )
    .await;
}

async fn insert_experiment(pool: &SqlitePool, name: &str, state: &str) {
    exec(
        pool,
        &format!("INSERT INTO autumn_experiments (name, state) VALUES ('{name}', '{state}')"),
    )
    .await;
}

fn config_with_windows() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.retention.job_history = Some("30d".to_owned());
    config.retention.commit_hooks = Some("30d".to_owned());
    config.retention.experiment_assignments = Some("30d".to_owned());
    "sqlite".clone_into(&mut config.jobs.backend);
    config
}

fn state_with(pool: &SqlitePool, config: AutumnConfig) -> AppState {
    let state = AppState::for_test().with_pool(pool.clone());
    state.insert_extension(config);
    state
}

const fn options(dataset: Option<&'static str>, dry_run: bool) -> RetentionRunOptions<'static> {
    RetentionRunOptions { dry_run, dataset }
}

#[tokio::test]
async fn a_sweep_removes_aged_records_and_leaves_newer_ones() {
    let fx = start().await;
    let pool = &fx.pool;
    insert_job(pool, "old-completed", "completed", 90).await;
    insert_job(pool, "old-failed", "failed", 90).await;
    insert_job(pool, "recent-completed", "completed", 1).await;
    insert_hook(pool, "old-hook", "completed", 90).await;
    insert_hook(pool, "old-after-failed", "after_hook_failed", 90).await;
    insert_hook(pool, "recent-hook", "completed", 1).await;
    insert_assignment(pool, "exp", "old-actor", 90).await;
    insert_assignment(pool, "exp", "recent-actor", 1).await;

    let state = state_with(pool, config_with_windows());
    let reports = run_retention(&state, &options(None, false))
        .await
        .expect("sweep runs");

    for (dataset, removed) in [
        ("job_history", 2),
        ("commit_hooks", 2),
        ("experiment_assignments", 1),
    ] {
        let report = reports
            .iter()
            .find(|r| r.dataset == dataset)
            .unwrap_or_else(|| panic!("{dataset} reported"));
        assert_eq!(report.error, None, "{report:?}");
        assert_eq!(report.skipped, None, "{report:?}");
        assert_eq!(report.enforcement, "sweep", "{report:?}");
        assert_eq!(report.rows_removed, removed, "{report:?}");
        assert_eq!(report.eligible_rows, Some(removed), "{report:?}");
        assert!(!report.truncated, "{report:?}");
        assert!(report.cutoff.is_some(), "{report:?}");
    }
    assert_eq!(
        values(pool, "SELECT id AS v FROM autumn_jobs").await,
        ["recent-completed"]
    );
    assert_eq!(
        values(pool, "SELECT id AS v FROM autumn_repository_commit_hooks").await,
        ["recent-hook"]
    );
    assert_eq!(
        values(pool, "SELECT actor AS v FROM autumn_experiment_assignments").await,
        ["recent-actor"]
    );
}

#[tokio::test]
async fn a_sweep_never_touches_a_live_job_row_however_old() {
    let fx = start().await;
    insert_job(&fx.pool, "queued", "enqueued", 400).await;
    insert_job(&fx.pool, "busy", "running", 400).await;
    insert_job(&fx.pool, "done", "completed", 400).await;

    let state = state_with(&fx.pool, config_with_windows());
    run_retention(&state, &options(Some("job_history"), false))
        .await
        .expect("sweep");

    let mut left = values(&fx.pool, "SELECT id AS v FROM autumn_jobs ORDER BY id").await;
    left.sort();
    assert_eq!(left, ["busy", "queued"]);
}

#[tokio::test]
async fn a_dry_run_counts_without_deleting_and_has_the_postgres_report_shape() {
    let fx = start().await;
    insert_job(&fx.pool, "old-1", "completed", 90).await;
    insert_job(&fx.pool, "old-2", "completed", 90).await;

    let state = state_with(&fx.pool, config_with_windows());
    let reports = run_retention(&state, &options(Some("job_history"), true))
        .await
        .expect("dry run");

    assert_eq!(reports.len(), 1, "--dataset narrows the run");
    let report = &reports[0];
    assert_eq!(report.eligible_rows, Some(2));
    assert_eq!(report.rows_removed, 0);
    assert!(report.dry_run);
    assert_eq!(report.window_secs, Some(30 * 86_400));
    assert_eq!(report.enforcement, "sweep");
    assert_eq!(
        count(&fx.pool, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
        2
    );
}

#[tokio::test]
async fn the_report_cutoff_is_the_window_before_now() {
    let fx = start().await;
    insert_job(&fx.pool, "old", "completed", 90).await;
    let state = state_with(&fx.pool, config_with_windows());
    let reports = run_retention(&state, &options(Some("job_history"), true))
        .await
        .expect("run");
    let cutoff =
        chrono::DateTime::parse_from_rfc3339(reports[0].cutoff.as_deref().expect("cutoff"))
            .expect("rfc3339")
            .with_timezone(&chrono::Utc);
    let expected = chrono::Utc::now() - chrono::Duration::days(30);
    assert!(
        (cutoff - expected).num_seconds().abs() < 5,
        "{cutoff} vs {expected}"
    );
}

#[tokio::test]
async fn no_configured_window_sweeps_nothing() {
    let fx = start().await;
    insert_job(&fx.pool, "old", "completed", 900).await;
    let mut config = config_with_windows();
    config.retention.job_history = None;
    let state = state_with(&fx.pool, config);
    let reports = run_retention(&state, &options(Some("job_history"), false))
        .await
        .expect("run");
    assert_eq!(
        reports[0].skipped.as_deref(),
        Some("no retention window configured")
    );
    assert_eq!(
        count(&fx.pool, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
        1
    );
}

#[tokio::test]
async fn a_ttl_dedup_hold_keeps_a_job_until_its_ttl_ends() {
    let fx = start().await;
    // Finished 90 days ago. One dedup hold of 365 days is still live. One of 1
    // day has ended.
    insert_ttl_unique_job(&fx.pool, "held", 90, 365).await;
    insert_ttl_unique_job(&fx.pool, "ended", 90, 1).await;
    let state = state_with(&fx.pool, config_with_windows());
    run_retention(&state, &options(Some("job_history"), false))
        .await
        .expect("sweep");
    assert_eq!(
        values(&fx.pool, "SELECT id AS v FROM autumn_jobs").await,
        ["held"]
    );
}

#[tokio::test]
async fn a_restartable_experiment_keeps_its_assignments() {
    let fx = start().await;
    insert_experiment(&fx.pool, "running-exp", "running").await;
    insert_experiment(&fx.pool, "done-exp", "concluded").await;
    insert_experiment(&fx.pool, "old-exp", "archived").await;
    for experiment in ["running-exp", "done-exp", "old-exp", "deleted-exp"] {
        insert_assignment(&fx.pool, experiment, "actor", 400).await;
    }
    let state = state_with(&fx.pool, config_with_windows());
    run_retention(&state, &options(Some("experiment_assignments"), false))
        .await
        .expect("sweep");
    let mut left = values(
        &fx.pool,
        "SELECT experiment AS v FROM autumn_experiment_assignments",
    )
    .await;
    left.sort();
    assert_eq!(left, ["done-exp", "running-exp"]);
}

#[tokio::test]
async fn a_sweep_batches_past_its_batch_size() {
    let fx = start().await;
    let old = now_ms() - 90 * DAY_MS;
    exec(
        &fx.pool,
        &format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1200) \
             INSERT INTO autumn_jobs (id, name, status, enqueued_at, finished_at) \
             SELECT 'job-' || i, 'demo', 'completed', {old}, {old} FROM n"
        ),
    )
    .await;
    let state = state_with(&fx.pool, config_with_windows());
    let reports = run_retention(&state, &options(Some("job_history"), false))
        .await
        .expect("sweep");
    assert_eq!(reports[0].rows_removed, 1200, "{:?}", reports[0]);
    assert!(!reports[0].truncated);
    assert_eq!(
        count(&fx.pool, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
        0
    );
}

#[tokio::test]
async fn a_legal_hold_stops_the_sweep_from_deleting_anything() {
    let fx = start().await;
    insert_job(&fx.pool, "held", "completed", 400).await;
    insert_assignment(&fx.pool, "exp", "not-held", 400).await;
    let state = state_with(&fx.pool, config_with_windows());
    state.insert_extension(
        GdprRegistry::new().register(ModelRegistration::retain("autumn_jobs", "hold 2026-CV-1")),
    );
    let reports = run_retention(&state, &AutumnRetentionAll::all())
        .await
        .expect("run");
    let jobs = reports
        .iter()
        .find(|r| r.dataset == "job_history")
        .expect("reported");
    assert_eq!(jobs.skipped.as_deref(), Some("legal hold: hold 2026-CV-1"));
    assert_eq!(
        count(&fx.pool, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
        1
    );
    assert_eq!(
        count(
            &fx.pool,
            "SELECT COUNT(*) AS n FROM autumn_experiment_assignments"
        )
        .await,
        0,
        "the hold is per dataset"
    );
}

#[tokio::test]
async fn a_missing_table_is_skipped_with_a_reason_not_failed() {
    let fx = start().await;
    exec(&fx.pool, "DROP TABLE autumn_repository_commit_hooks").await;
    let state = state_with(&fx.pool, config_with_windows());
    let reports = run_retention(&state, &options(Some("commit_hooks"), false))
        .await
        .expect("run");
    assert_eq!(reports[0].error, None, "{:?}", reports[0]);
    assert!(
        reports[0]
            .skipped
            .as_deref()
            .is_some_and(|s| s.contains("autumn_repository_commit_hooks")),
        "{:?}",
        reports[0]
    );
}

/// `RetentionRunOptions::default()` under a short name.
struct AutumnRetentionAll;

impl AutumnRetentionAll {
    fn all() -> RetentionRunOptions<'static> {
        RetentionRunOptions::default()
    }
}

#[tokio::test]
async fn an_older_jobs_table_is_skipped_not_swept_and_not_failed() {
    let fx = start().await;
    // A table from before `unique_ttl_ms` existed.
    exec(&fx.pool, "DROP TABLE autumn_jobs").await;
    exec(
        &fx.pool,
        "CREATE TABLE autumn_jobs (id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, \
         status TEXT NOT NULL, enqueued_at BIGINT NOT NULL DEFAULT 0, finished_at BIGINT, \
         unique_key TEXT, unique_window TEXT)",
    )
    .await;
    insert_job(&fx.pool, "old", "completed", 90).await;
    let state = state_with(&fx.pool, config_with_windows());
    let reports = run_retention(&state, &options(Some("job_history"), false))
        .await
        .expect("run");
    assert_eq!(reports[0].error, None, "{:?}", reports[0]);
    assert_eq!(reports[0].rows_removed, 0);
    assert!(
        reports[0]
            .skipped
            .as_deref()
            .is_some_and(|s| s.contains("unique_ttl_ms")),
        "{:?}",
        reports[0]
    );
    assert_eq!(
        count(&fx.pool, "SELECT COUNT(*) AS n FROM autumn_jobs").await,
        1
    );
}
