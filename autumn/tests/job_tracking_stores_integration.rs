//! Docker-gated integration tests for the Redis/Postgres tracked-job stores
//! (issue #1373): prove `enqueue_tracked` actually persists through the
//! backend selected by `jobs.backend`, not silently through the in-memory
//! fallback, and that the configured TTL expires records on each backend.
//!
//! Both tests require Docker and are marked `#[ignore]`. Run them explicitly
//! with:
//!
//! ```text
//! cargo test -p autumn-web --features redis,db --test job_tracking_stores_integration -- --ignored
//! ```

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use autumn_web::config::JobConfig;
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnResult};
use serde_json::Value;

fn noop_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move { Ok(()) })
}

fn noop_job_info() -> JobInfo {
    JobInfo {
        name: "noop".to_string(),
        max_attempts: 1,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: noop_handler,
    }
}

#[cfg(feature = "redis")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn redis_backend_persists_tracked_job_and_expires_it() {
    use redis::AsyncCommands as _;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis as RedisImage;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let redis_url = format!("redis://127.0.0.1:{port}");

    let mut config = JobConfig::default();
    config.backend = "redis".to_owned();
    config.redis.url = Some(redis_url.clone());
    config.tracking.ttl_secs = 1;

    let state = AppState::for_test().with_profile("dev");
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(vec![noop_job_info()], &state, &shutdown, &config)
        .expect("start redis-backed job runtime");

    let handle = job::enqueue_tracked("noop", serde_json::json!({}))
        .await
        .expect("enqueue_tracked");

    // Query Redis directly, bypassing the JobTrackingStore abstraction, to
    // prove the config-selected backend really is Redis — not the in-memory
    // fallback, which would leave nothing here to find.
    let client = redis::Client::open(redis_url.clone()).expect("open redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to redis");
    let key = format!(
        "{}:tracking:{}",
        config.redis.key_prefix,
        autumn_web::auth::hash_api_token(&handle.token)
    );

    let payload: String = conn
        .get(&key)
        .await
        .expect("tracked record should be persisted in redis");
    let record: Value = serde_json::from_str(&payload).expect("stored record is valid JSON");
    assert!(
        record["status"].is_string(),
        "expected a status field: {record}"
    );

    // The 1s TTL configured above (rather than the 86400s default) should
    // expire the key via Redis's own EX mechanism.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let exists: bool = conn.exists(&key).await.expect("check key existence");
    assert!(!exists, "tracked record should have expired via redis TTL");

    shutdown.cancel();
    job::clear_global_job_client();
}

#[cfg(feature = "db")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_backend_persists_tracked_job_and_expires_it() {
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    const CREATE_AUTUMN_JOBS: &str =
        include_str!("../migrations/20260513000000_create_job_queue/up.sql");
    const CREATE_JOB_TRACKING: &str =
        include_str!("../migrations/20260702000000_create_job_tracking/up.sql");

    #[derive(diesel::QueryableByName)]
    struct RecordRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        record: String,
    }

    #[derive(diesel::QueryableByName)]
    struct ExpiredRow {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        expired: bool,
    }

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager).max_size(4).build().expect("build pool");

    {
        let mut conn = pool.get().await.expect("get connection");
        diesel::sql_query(CREATE_AUTUMN_JOBS)
            .execute(&mut *conn)
            .await
            .expect("apply create_job_queue migration");
        // Proves the migration in this PR actually creates the table the
        // store depends on — not just that hand-written SQL happens to work.
        diesel::sql_query(CREATE_JOB_TRACKING)
            .execute(&mut *conn)
            .await
            .expect("apply create_job_tracking migration");
    }

    let mut config = JobConfig::default();
    config.backend = "postgres".to_owned();
    config.tracking.ttl_secs = 1;

    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(vec![noop_job_info()], &state, &shutdown, &config)
        .expect("start postgres-backed job runtime");

    let handle = job::enqueue_tracked("noop", serde_json::json!({}))
        .await
        .expect("enqueue_tracked");

    let key = autumn_web::auth::hash_api_token(&handle.token);
    let mut conn = pool.get().await.expect("get connection");
    let row = diesel::sql_query("SELECT record FROM autumn_job_tracking WHERE key = $1")
        .bind::<diesel::sql_types::Text, _>(&key)
        .get_result::<RecordRow>(&mut *conn)
        .await
        .expect("tracked record should be persisted in postgres");
    let record: Value = serde_json::from_str(&row.record).expect("stored record is valid JSON");
    assert!(
        record["status"].is_string(),
        "expected a status field: {record}"
    );

    // The 1s TTL configured above should push expires_at into the past;
    // lazy expiry means the row itself isn't deleted, only ignored on read.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let expired = diesel::sql_query(
        "SELECT (expires_at <= NOW()) AS expired FROM autumn_job_tracking WHERE key = $1",
    )
    .bind::<diesel::sql_types::Text, _>(&key)
    .get_result::<ExpiredRow>(&mut *conn)
    .await
    .expect("row should still exist")
    .expired;
    assert!(expired, "record should be past its configured TTL");

    shutdown.cancel();
    job::clear_global_job_client();
}
