//! A tracked job's terminal status follows the queue ack (issue #2583).
//!
//! A worker writes `succeeded` or `failed` only after the queue accepts its
//! claim-checked ack. These tests run it on the Postgres and Redis backends:
//!
//! - An attempt that owns its claim settles its record: success, a final
//!   failure and a panic.
//! - A job with no handler on this process dead-letters and settles `failed`.
//! - Postgres only: an attempt that lost its claim leaves the record
//!   `running`, and the new owner can still settle it.
//!
//! Requires Docker (testcontainers). Run:
//!
//! ```text
//! cargo test -p autumn-web --features redis,db \
//!   --test integration_tests job_tracking_settle_ack -- --ignored
//! ```

#![cfg(any(feature = "redis", all(feature = "db", not(feature = "sqlite"))))]

use std::time::{Duration, Instant};

use autumn_web::job::JobInfo;
use autumn_web::job_tracking::TrackedJobStatus;

/// Poll until `f` holds or the deadline passes. Returns whether it held.
async fn wait_for<F, Fut>(deadline: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if f().await {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Succeeds and records a result.
fn ok_job(name: &'static str) -> JobInfo {
    JobInfo::new(name, 1, 10, |_state, _payload| {
        Box::pin(async move {
            autumn_web::job::JobContext::current().set_result(serde_json::json!({ "ok": true }));
            Ok(())
        })
    })
}

/// Fails. With one attempt, the first failure is final.
fn err_job(name: &'static str) -> JobInfo {
    JobInfo::new(name, 1, 10, |_state, _payload| {
        Box::pin(async move { Err(autumn_web::AutumnError::internal_server_error_msg("boom")) })
    })
}

/// Panics. A panic is terminal even when attempts remain.
fn panic_job(name: &'static str) -> JobInfo {
    JobInfo::new(name, 3, 10, |_state, _payload| {
        Box::pin(async move { panic!("owned attempt panicked") })
    })
}

/// Names the tracked jobs the owned-claim tests run.
const OK: &str = "settle_ack_ok";
const ERR: &str = "settle_ack_err";
const PANIC: &str = "settle_ack_panic";

/// Stale-attempt state. File level, not in `postgres_backend`:
/// `diesel_async::RunQueryDsl` has a `load` method that shadows
/// `Atomic*::load` inside that module.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod stale_state {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);
    static GATE: AtomicBool = AtomicBool::new(false);
    static DONE: AtomicBool = AtomicBool::new(false);

    pub(super) fn reset() {
        RUNS.store(0, Ordering::SeqCst);
        GATE.store(false, Ordering::SeqCst);
        DONE.store(false, Ordering::SeqCst);
    }

    pub(super) fn note_run() {
        RUNS.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn runs() -> usize {
        RUNS.load(Ordering::SeqCst)
    }

    pub(super) fn open_gate() {
        GATE.store(true, Ordering::SeqCst);
    }

    pub(super) fn gate_is_open() -> bool {
        GATE.load(Ordering::SeqCst)
    }

    pub(super) fn note_done() {
        DONE.store(true, Ordering::SeqCst);
    }

    pub(super) fn is_done() -> bool {
        DONE.load(Ordering::SeqCst)
    }
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod postgres_backend {
    use super::{
        Duration, ERR, OK, PANIC, TrackedJobStatus, err_job, ok_job, panic_job, stale_state,
        wait_for,
    };

    use autumn_web::AppState;
    use autumn_web::config::{JobConfig, JobPostgresConfig};
    use autumn_web::job::{self, JobInfo};
    use autumn_web::job_tracking::{JobTrackingStore as _, PgJobTrackingStore};
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl as _};
    use testcontainers::ContainerAsync;
    use testcontainers_modules::postgres::Postgres;

    type PgPool = Pool<AsyncPgConnection>;

    /// A long timeout, so the heartbeat cannot notice a moved claim in time.
    const VISIBILITY_MS: u64 = 60_000;

    fn config(workers: usize) -> JobConfig {
        JobConfig {
            backend: "postgres".to_owned(),
            workers,
            postgres: JobPostgresConfig {
                visibility_timeout_ms: VISIBILITY_MS,
                ..JobPostgresConfig::default()
            },
            ..Default::default()
        }
    }

    async fn start_postgres() -> (ContainerAsync<Postgres>, String) {
        use testcontainers::runners::AsyncRunner as _;

        let container = Postgres::default()
            .start()
            .await
            .expect("start Postgres container");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        autumn_web::migrate::run_pending(&url, autumn_web::migrate::FRAMEWORK_MIGRATIONS)
            .expect("apply framework migrations");
        (container, url)
    }

    fn pool(url: &str) -> PgPool {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        Pool::builder(manager).max_size(8).build().expect("pool")
    }

    async fn status(pool: &PgPool, token: &str) -> TrackedJobStatus {
        PgJobTrackingStore::new(pool.clone(), 3_600)
            .get(&autumn_web::auth::hash_api_token(token))
            .await
            .expect("get tracked record")
            .expect("the tracked record is live")
            .status
    }

    /// Owned attempts settle: success, final failure, panic.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn postgres_owned_attempts_settle_the_tracked_status_after_the_ack() {
        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        let (_container, url) = start_postgres().await;
        let pool = pool(&url);
        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![ok_job(OK), err_job(ERR), panic_job(PANIC)],
            &state,
            &shutdown,
            &config(2),
            true,
        )
        .expect("postgres runtime starts");

        let ok = job::enqueue_tracked(OK, serde_json::json!({}))
            .await
            .expect("tracked enqueue");
        let err = job::enqueue_tracked(ERR, serde_json::json!({}))
            .await
            .expect("tracked enqueue");
        let panicked = job::enqueue_tracked(PANIC, serde_json::json!({}))
            .await
            .expect("tracked enqueue");

        assert!(
            wait_for(Duration::from_secs(30), || async {
                status(&pool, &ok.token).await == TrackedJobStatus::Succeeded
                    && status(&pool, &err.token).await == TrackedJobStatus::Failed
                    && status(&pool, &panicked.token).await == TrackedJobStatus::Failed
            })
            .await,
            "every owned attempt settles its record"
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }

    /// A job with no handler dead-letters and settles `failed`.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn postgres_unknown_tracked_job_settles_failed_after_the_ack() {
        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        let (_container, url) = start_postgres().await;
        let pool = pool(&url);
        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());

        // Web role: knows the job and runs no workers.
        let web_shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![ok_job("settle_ack_unknown")],
            &state,
            &web_shutdown,
            &config(1),
            false,
        )
        .expect("enqueue-only runtime starts");
        let handle = job::enqueue_tracked("settle_ack_unknown", serde_json::json!({}))
            .await
            .expect("tracked enqueue");
        web_shutdown.cancel();
        job::clear_global_job_client();

        // Worker role: does not know the job.
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![ok_job("settle_ack_other")],
            &state,
            &shutdown,
            &config(1),
            true,
        )
        .expect("worker runtime starts");

        assert!(
            wait_for(Duration::from_secs(30), || async {
                status(&pool, &handle.token).await == TrackedJobStatus::Failed
            })
            .await,
            "the unknown job's record settles failed"
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }

    fn stale_job() -> JobInfo {
        JobInfo::new("settle_ack_stale", 1, 10, |_state, _payload| {
            Box::pin(async move {
                stale_state::note_run();
                while !stale_state::gate_is_open() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                stale_state::note_done();
                Err(autumn_web::AutumnError::internal_server_error_msg(
                    "stale attempt failed",
                ))
            })
        })
    }

    /// An attempt that lost its claim leaves the record `running`, and the
    /// new owner can still settle it.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn postgres_stale_attempt_does_not_publish_a_terminal_tracked_status() {
        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        stale_state::reset();
        let (_container, url) = start_postgres().await;
        let pool = pool(&url);
        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(vec![stale_job()], &state, &shutdown, &config(1), true)
            .expect("postgres runtime starts");

        let handle = job::enqueue_tracked("settle_ack_stale", serde_json::json!({}))
            .await
            .expect("tracked enqueue");
        assert!(
            wait_for(Duration::from_secs(30), || async {
                stale_state::runs() == 1
            })
            .await,
            "the first attempt starts"
        );

        // Another worker takes the claim before the first attempt ends.
        {
            let mut conn = pool.get().await.expect("conn");
            diesel::sql_query(
                "UPDATE autumn_jobs SET claimed_by = 'other-worker', \
                 claimed_at = NOW() + INTERVAL '1 day'",
            )
            .execute(&mut conn)
            .await
            .expect("move the claim");
        }
        stale_state::open_gate();
        assert!(
            wait_for(Duration::from_secs(30), || async { stale_state::is_done() }).await,
            "the stale attempt ends"
        );
        // Wait long enough for a wrong settle to appear.
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            status(&pool, &handle.token).await,
            TrackedJobStatus::Running,
            "a disowned attempt must not settle the record"
        );

        // The new owner's result is not refused.
        PgJobTrackingStore::new(pool.clone(), 3_600)
            .complete(
                &autumn_web::auth::hash_api_token(&handle.token),
                serde_json::json!({ "owner": "second" }),
            )
            .await
            .expect("complete");
        assert_eq!(
            status(&pool, &handle.token).await,
            TrackedJobStatus::Succeeded
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }
}

#[cfg(feature = "redis")]
mod redis_backend {
    use super::{Duration, ERR, OK, PANIC, TrackedJobStatus, err_job, ok_job, panic_job, wait_for};

    use autumn_web::AppState;
    use autumn_web::config::{JobConfig, JobRedisConfig};
    use autumn_web::job;
    use testcontainers::ContainerAsync;
    use testcontainers_modules::redis::Redis as RedisImage;

    const KEY_PREFIX: &str = "autumn:jobs";

    fn config(url: &str) -> JobConfig {
        JobConfig {
            backend: "redis".to_owned(),
            workers: 2,
            redis: JobRedisConfig {
                url: Some(url.to_owned()),
                key_prefix: KEY_PREFIX.to_owned(),
                ..JobRedisConfig::default()
            },
            ..Default::default()
        }
    }

    async fn start_redis() -> (ContainerAsync<RedisImage>, String) {
        use testcontainers::runners::AsyncRunner as _;

        let container = RedisImage::default()
            .start()
            .await
            .expect("start Redis container");
        let port = container
            .get_host_port_ipv4(6379)
            .await
            .expect("redis port");
        (container, format!("redis://127.0.0.1:{port}"))
    }

    /// The status of a tracked record, read straight from Redis.
    async fn status(url: &str, token: &str) -> Option<TrackedJobStatus> {
        use redis::AsyncCommands as _;

        let client = autumn_web::redis_tls::open_client(url).expect("redis client");
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("redis connection");
        let key = format!(
            "{KEY_PREFIX}:tracking:{}",
            autumn_web::auth::hash_api_token(token)
        );
        let payload: Option<String> = conn.get(key).await.expect("GET");
        let record: serde_json::Value = serde_json::from_str(&payload?).expect("record JSON");
        serde_json::from_value(record["status"].clone()).ok()
    }

    /// Owned attempts settle: success, final failure, panic.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_owned_attempts_settle_the_tracked_status_after_the_ack() {
        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        let (_container, url) = start_redis().await;
        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![ok_job(OK), err_job(ERR), panic_job(PANIC)],
            &state,
            &shutdown,
            &config(&url),
            true,
        )
        .expect("redis runtime starts");

        let ok = job::enqueue_tracked(OK, serde_json::json!({}))
            .await
            .expect("tracked enqueue");
        let err = job::enqueue_tracked(ERR, serde_json::json!({}))
            .await
            .expect("tracked enqueue");
        let panicked = job::enqueue_tracked(PANIC, serde_json::json!({}))
            .await
            .expect("tracked enqueue");

        assert!(
            wait_for(Duration::from_secs(30), || async {
                status(&url, &ok.token).await == Some(TrackedJobStatus::Succeeded)
                    && status(&url, &err.token).await == Some(TrackedJobStatus::Failed)
                    && status(&url, &panicked.token).await == Some(TrackedJobStatus::Failed)
            })
            .await,
            "every owned attempt settles its record"
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }
}
