//! Issue #3054 on the Postgres job backend: jobs that fail together must not
//! retry together.
//!
//! Each job fails once with a 60 s backoff. Before #3054 every retry was due
//! at `NOW() + 60 s`, so all `run_at` values fell in the short window in which
//! the jobs failed. Full jitter puts each one in `[0, 60 s]`.
//!
//! A retry can be due at once and run again. Its row keeps `attempt = 2` and
//! its `run_at` when it settles, so the tests count rows by `attempt` only.
//!
//! Requires Docker. CI's `--ignored` sweep runs it (see CLAUDE.md).

#![cfg(all(feature = "db", not(feature = "sqlite")))]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use autumn_web::config::{JobConfig, JobPostgresConfig};
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnError, AutumnResult};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _};
use serde_json::Value;

const STORM_JOBS: i64 = 8;

/// The spread of the due times of all rows at their second attempt, in ms.
const SPREAD_SQL: &str = "SELECT (EXTRACT(EPOCH FROM MAX(run_at) - MIN(run_at)) * 1000)::BIGINT \
                          AS value FROM autumn_jobs WHERE attempt = 2";

const SECOND_ATTEMPTS_SQL: &str = "SELECT COUNT(*) AS value FROM autumn_jobs WHERE attempt = 2";

#[derive(diesel::QueryableByName)]
struct ValueRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    value: i64,
}

fn always_fails(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move { Err(AutumnError::internal_server_error_msg("blip")) })
}

fn succeeds(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move { Ok(()) })
}

async fn value(pool: &Pool<AsyncPgConnection>, sql: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .get_result::<ValueRow>(&mut conn)
        .await
        .expect("query")
        .value
}

/// Poll until `sql` returns at least `want`, or fail after 20 s.
async fn wait_for(pool: &Pool<AsyncPgConnection>, sql: &str, want: i64, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while value(pool, sql).await < want {
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A migrated Postgres in a container. Keep the container alive for the test.
async fn postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    Pool<AsyncPgConnection>,
) {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    autumn_web::migrate::run_pending(&url, autumn_web::migrate::FRAMEWORK_MIGRATIONS)
        .expect("apply framework migrations");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(8).build().expect("pool");
    (container, pool)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_job_retries_spread_out() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let (_container, pool) = postgres().await;

    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone())
        .with_entropy(autumn_web::entropy::SeededEntropy::shared(0x3054));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = JobConfig {
        backend: "postgres".to_owned(),
        workers: 2,
        ..Default::default()
    };
    job::start_runtime(
        vec![JobInfo::new("pg_storm_job", 2, 60_000, always_fails)],
        &state,
        &shutdown,
        &config,
        true,
    )
    .expect("postgres job runtime starts");

    for n in 0..STORM_JOBS {
        job::enqueue("pg_storm_job", serde_json::json!({ "n": n }))
            .await
            .expect("enqueue");
    }

    wait_for(
        &pool,
        SECOND_ATTEMPTS_SQL,
        STORM_JOBS,
        "every job must schedule its retry",
    )
    .await;

    let spread_ms = value(&pool, SPREAD_SQL).await;
    assert!(
        spread_ms > 10_000,
        "{STORM_JOBS} retries are due within {spread_ms} ms of each other: they retry together"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// Claims that expire together (handlers that hung on one dependency) must
/// not all run again at once. Before the fix the recovery set
/// `run_at = NOW()` for every row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_recovered_claims_spread_out() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let (_container, pool) = postgres().await;

    // Rows a dead worker left behind, each with a 60 s backoff.
    {
        let mut conn = pool.get().await.expect("conn");
        for n in 0..STORM_JOBS {
            diesel::sql_query(format!(
                "INSERT INTO autumn_jobs \
                 (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
                  enqueued_at, run_at, started_at, claimed_by, claimed_at) \
                 VALUES ('stale-{n}', 'pg_stale_storm_job', 'default', '{{}}'::JSONB, 'running', \
                         1, 3, 60000, NOW() - INTERVAL '1 hour', NOW() - INTERVAL '1 hour', \
                         NOW() - INTERVAL '1 hour', 'dead-worker', NOW() - INTERVAL '1 hour')"
            ))
            .execute(&mut conn)
            .await
            .expect("insert a stale claim");
        }
    }

    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = JobConfig {
        backend: "postgres".to_owned(),
        workers: 2,
        postgres: JobPostgresConfig {
            visibility_timeout_ms: 500,
            ..JobPostgresConfig::default()
        },
        ..Default::default()
    };
    // A recovered row with a near-0 delay runs at once. It succeeds, so it
    // keeps `attempt = 2`.
    job::start_runtime(
        vec![JobInfo::new("pg_stale_storm_job", 3, 60_000, succeeds)],
        &state,
        &shutdown,
        &config,
        true,
    )
    .expect("postgres job runtime starts");

    wait_for(
        &pool,
        SECOND_ATTEMPTS_SQL,
        STORM_JOBS,
        "every stale claim must be recovered",
    )
    .await;

    let spread_ms = value(&pool, SPREAD_SQL).await;
    assert!(
        spread_ms > 10_000,
        "{STORM_JOBS} recovered claims are due within {spread_ms} ms of each other"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
