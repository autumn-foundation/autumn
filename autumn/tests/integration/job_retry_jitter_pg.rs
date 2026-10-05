//! Issue #3054 on the Postgres job backend: jobs that fail together must not
//! retry together.
//!
//! Each job fails once with a 60 s backoff. Before #3054 every retry was due
//! at `NOW() + 60 s`, so all `run_at` values fell in the short window in which
//! the jobs failed. Full jitter puts each one in `[0, 60 s]`.
//!
//! A retry can be due at once and fail again (its last attempt). Its row then
//! is `failed`, but it keeps `attempt = 2` and its `run_at`, so the test counts
//! rows by `attempt` only.
//!
//! Requires Docker. CI's `--ignored` sweep runs it (see CLAUDE.md).

#![cfg(all(feature = "db", not(feature = "sqlite")))]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use autumn_web::config::JobConfig;
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnError, AutumnResult};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _};
use serde_json::Value;

const STORM_JOBS: i64 = 8;

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

async fn value(pool: &Pool<AsyncPgConnection>, sql: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .get_result::<ValueRow>(&mut conn)
        .await
        .expect("query")
        .value
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_job_retries_spread_out() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::postgres::Postgres;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

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

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while value(
        &pool,
        "SELECT COUNT(*) AS value FROM autumn_jobs WHERE attempt = 2",
    )
    .await
        < STORM_JOBS
    {
        assert!(
            std::time::Instant::now() < deadline,
            "every job must schedule its retry"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let spread_ms = value(
        &pool,
        "SELECT (EXTRACT(EPOCH FROM MAX(run_at) - MIN(run_at)) * 1000)::BIGINT AS value \
         FROM autumn_jobs WHERE attempt = 2",
    )
    .await;
    assert!(
        spread_ms > 10_000,
        "{STORM_JOBS} retries are due within {spread_ms} ms of each other: they retry together"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
