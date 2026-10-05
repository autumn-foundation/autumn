//! Claim lease and execution timeout on the container backends (issue #3051).
//!
//! Each backend gets the same four proofs:
//!
//! 1. A job that runs for 3x the visibility timeout runs exactly once. The
//!    heartbeat keeps the claim fresh, so recovery does not give it to the
//!    second worker.
//! 2. A killed worker's job is recovered after the visibility timeout. The
//!    heartbeat dies with the worker, so crash recovery still works.
//! 3. A job that exceeds its timeout fails, retries, and frees the worker.
//! 4. Redis only: the stale-claim check uses Redis server time. A worker with
//!    a skewed clock does not steal a live claim.
//!
//! A "kill" runs worker A on its own Tokio runtime and shuts that runtime
//! down. Every task it owns stops at once, the heartbeat too.
//!
//! Requires Docker (testcontainers). Run:
//!
//! ```text
//! cargo test -p autumn-web --features redis,db \
//!   --test integration_tests job_lease_heartbeat -- --ignored
//! ```

#![cfg(any(feature = "redis", all(feature = "db", not(feature = "sqlite"))))]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use autumn_web::job::JobInfo;
use tokio::time::sleep;

/// Poll until `f` holds or the deadline passes. Returns whether it held.
async fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// Run `phase` on a new multi-thread runtime, then shut that runtime down
/// without waiting. That stops every task it spawned, like a process kill.
fn run_then_kill<F>(phase: F)
where
    F: FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("worker runtime");
        runtime.block_on(phase());
        runtime.shutdown_background();
    })
    .join()
    .expect("worker thread");
}

static SLOW_RUNS: AtomicUsize = AtomicUsize::new(0);
static KILLED_RUNS: AtomicUsize = AtomicUsize::new(0);
static HUNG_RUNS: AtomicUsize = AtomicUsize::new(0);
static AFTER_HUNG_RAN: AtomicUsize = AtomicUsize::new(0);

// Free functions, not `X.load(..)` at call sites: `diesel_async::RunQueryDsl`
// has a `load` method that shadows `AtomicUsize::load` in the Postgres module.
fn slow_runs() -> usize {
    SLOW_RUNS.load(Ordering::SeqCst)
}

fn killed_runs() -> usize {
    KILLED_RUNS.load(Ordering::SeqCst)
}

fn hung_runs() -> usize {
    HUNG_RUNS.load(Ordering::SeqCst)
}

fn after_hung_ran() -> usize {
    AFTER_HUNG_RAN.load(Ordering::SeqCst)
}

fn reset_counters() {
    SLOW_RUNS.store(0, Ordering::SeqCst);
    KILLED_RUNS.store(0, Ordering::SeqCst);
    HUNG_RUNS.store(0, Ordering::SeqCst);
    AFTER_HUNG_RAN.store(0, Ordering::SeqCst);
}

/// How long `slow_job` sleeps. Handlers are bare `fn` pointers, so a static.
static SLOW_SLEEP_MS: AtomicU64 = AtomicU64::new(0);

/// A job that runs for 3x `visibility_timeout_ms`.
fn slow_job(visibility_timeout_ms: u64) -> JobInfo {
    SLOW_SLEEP_MS.store(3 * visibility_timeout_ms, Ordering::SeqCst);
    JobInfo::new("lease_slow_job", 3, 10, |_state, _payload| {
        Box::pin(async move {
            SLOW_RUNS.fetch_add(1, Ordering::SeqCst);
            sleep(Duration::from_millis(SLOW_SLEEP_MS.load(Ordering::SeqCst))).await;
            Ok(())
        })
    })
}

/// The first run hangs until its worker is killed. Later runs complete.
fn killed_job() -> JobInfo {
    JobInfo::new("lease_killed_job", 3, 10, |_state, _payload| {
        Box::pin(async move {
            if KILLED_RUNS.fetch_add(1, Ordering::SeqCst) == 0 {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    })
}

/// Every run hangs. A 300ms timeout and two attempts dead-letter it.
fn hung_job() -> JobInfo {
    let mut info = JobInfo::new("lease_hung_job", 2, 10, |_state, _payload| {
        Box::pin(async move {
            HUNG_RUNS.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        })
    });
    info.timeout = Some(Duration::from_millis(300));
    info
}

fn after_hung_job() -> JobInfo {
    JobInfo::new("lease_after_hung_job", 1, 10, |_state, _payload| {
        Box::pin(async move {
            AFTER_HUNG_RAN.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    })
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod postgres_backend {
    use super::{
        Duration, Instant, after_hung_job, after_hung_ran, hung_job, hung_runs, killed_job,
        killed_runs, reset_counters, run_then_kill, slow_job, slow_runs, wait_until,
    };

    use autumn_web::AppState;
    use autumn_web::config::{JobConfig, JobPostgresConfig};
    use autumn_web::job;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl as _};
    use testcontainers::ContainerAsync;
    use testcontainers_modules::postgres::Postgres;

    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }

    #[derive(diesel::QueryableByName)]
    struct TextRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        value: String,
    }

    fn config(workers: usize, visibility_timeout_ms: u64) -> JobConfig {
        JobConfig {
            backend: "postgres".to_owned(),
            workers,
            postgres: JobPostgresConfig {
                visibility_timeout_ms,
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

    fn pool(url: &str) -> Pool<AsyncPgConnection> {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        Pool::builder(manager).max_size(8).build().expect("pool")
    }

    async fn count(pool: &Pool<AsyncPgConnection>, sql: &str) -> i64 {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(sql)
            .get_result::<CountRow>(&mut conn)
            .await
            .expect("count query")
            .count
    }

    async fn text(pool: &Pool<AsyncPgConnection>, sql: &str) -> String {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(sql)
            .get_result::<TextRow>(&mut conn)
            .await
            .expect("text query")
            .value
    }

    /// AC1 on Postgres. The sweep runs every 5s, so the visibility timeout is
    /// 2s and the job runs for 6s: on code with no heartbeat the 5s sweep
    /// finds a 5s-old claim and gives it to the idle worker.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn postgres_job_longer_than_the_visibility_timeout_runs_exactly_once() {
        const VISIBILITY_MS: u64 = 2_000;

        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();
        let (_container, url) = start_postgres().await;
        let pool = pool(&url);

        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![slow_job(VISIBILITY_MS)],
            &state,
            &shutdown,
            &config(2, VISIBILITY_MS),
            true,
        )
        .expect("postgres runtime starts");

        job::enqueue("lease_slow_job", serde_json::json!({}))
            .await
            .expect("enqueue");
        assert!(
            wait_until(Duration::from_secs(10), || slow_runs() == 1).await,
            "the slow job starts"
        );
        // Wait out the run, plus one more sweep for a duplicate to show.
        tokio::time::sleep(Duration::from_millis(4 * VISIBILITY_MS)).await;

        assert_eq!(
            slow_runs(),
            1,
            "a job longer than the visibility timeout must run exactly once"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) AS count FROM autumn_jobs \
                 WHERE status = 'completed' AND attempt = 1"
            )
            .await,
            1,
            "the first claim completes the job"
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }

    /// AC3 on Postgres: a killed worker's job is recovered and run again.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn postgres_killed_workers_job_is_recovered_after_the_visibility_timeout() {
        const VISIBILITY_MS: u64 = 1_000;

        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();
        let (_container, url) = start_postgres().await;

        let url_a = url.clone();
        run_then_kill(move || {
            Box::pin(async move {
                let state = AppState::for_test()
                    .with_profile("dev")
                    .with_pool(pool(&url_a));
                let shutdown = tokio_util::sync::CancellationToken::new();
                job::start_runtime(
                    vec![killed_job()],
                    &state,
                    &shutdown,
                    &config(1, VISIBILITY_MS),
                    true,
                )
                .expect("worker A starts");
                job::enqueue("lease_killed_job", serde_json::json!({}))
                    .await
                    .expect("enqueue");
                assert!(
                    wait_until(Duration::from_secs(10), || killed_runs() == 1).await,
                    "worker A starts the job"
                );
                // Let at least one heartbeat land before the kill.
                tokio::time::sleep(Duration::from_millis(VISIBILITY_MS)).await;
            })
        });
        let killed_at = Instant::now();
        job::clear_global_job_client();

        let pool = pool(&url);
        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![killed_job()],
            &state,
            &shutdown,
            &config(1, VISIBILITY_MS),
            true,
        )
        .expect("worker B starts");

        assert!(
            wait_until(Duration::from_secs(20), || killed_runs() == 2).await,
            "worker B recovers and runs the job"
        );
        assert!(
            killed_at.elapsed() >= Duration::from_millis(VISIBILITY_MS / 2),
            "recovery waits for the visibility timeout; took {:?}",
            killed_at.elapsed()
        );
        let mut completed = false;
        for _ in 0..200 {
            if count(
                &pool,
                "SELECT COUNT(*) AS count FROM autumn_jobs \
                 WHERE status = 'completed' AND attempt = 2",
            )
            .await
                == 1
            {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(completed, "the recovered second attempt completes");

        shutdown.cancel();
        job::clear_global_job_client();
    }

    /// AC4 on Postgres: a job over its timeout fails, retries, and frees the
    /// only worker.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn postgres_job_exceeding_its_timeout_is_failed_retried_and_frees_the_worker() {
        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();
        let (_container, url) = start_postgres().await;
        let pool = pool(&url);

        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![hung_job(), after_hung_job()],
            &state,
            &shutdown,
            &config(1, 30_000),
            true,
        )
        .expect("postgres runtime starts");

        job::enqueue("lease_hung_job", serde_json::json!({}))
            .await
            .expect("enqueue hung job");
        assert!(
            wait_until(Duration::from_secs(10), || hung_runs() == 1).await,
            "the hung job starts"
        );
        job::enqueue("lease_after_hung_job", serde_json::json!({}))
            .await
            .expect("enqueue second job");

        assert!(
            wait_until(Duration::from_secs(10), || after_hung_ran() == 1).await,
            "the second job runs on the freed worker"
        );
        let mut failed = false;
        for _ in 0..400 {
            if count(
                &pool,
                "SELECT COUNT(*) AS count FROM autumn_jobs \
                 WHERE name = 'lease_hung_job' AND status = 'failed'",
            )
            .await
                == 1
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(failed, "the hung job dead-letters after its last attempt");
        assert_eq!(hung_runs(), 2, "the timeout was retried");
        let error = text(
            &pool,
            "SELECT last_error AS value FROM autumn_jobs WHERE name = 'lease_hung_job'",
        )
        .await;
        assert!(
            error.contains("timed out"),
            "the row keeps the timeout error; got: {error}"
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }
}

#[cfg(feature = "redis")]
mod redis_backend {
    use super::{
        Duration, Instant, after_hung_job, after_hung_ran, hung_job, hung_runs, killed_job,
        killed_runs, reset_counters, run_then_kill, slow_job, slow_runs, wait_until,
    };

    use std::sync::Arc;

    use autumn_web::AppState;
    use autumn_web::config::{JobConfig, JobRedisConfig};
    use autumn_web::job;
    use testcontainers::ContainerAsync;
    use testcontainers_modules::redis::Redis as RedisImage;

    const KEY_PREFIX: &str = "autumn:jobs";

    fn config(url: &str, workers: usize, visibility_timeout_ms: u64) -> JobConfig {
        JobConfig {
            backend: "redis".to_owned(),
            workers,
            redis: JobRedisConfig {
                url: Some(url.to_owned()),
                key_prefix: KEY_PREFIX.to_owned(),
                visibility_timeout_ms,
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

    /// The encoded records on a Redis list, newest first.
    async fn list(url: &str, key: &str) -> Vec<String> {
        let client = redis::Client::open(url).expect("redis client");
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("redis connection");
        redis::cmd("LRANGE")
            .arg(key)
            .arg(0)
            .arg(-1)
            .query_async(&mut conn)
            .await
            .expect("LRANGE")
    }

    /// AC2 on Redis. The idle worker sweeps every second, so on code with no
    /// heartbeat it steals the claim one visibility timeout after the claim.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_job_longer_than_the_visibility_timeout_runs_exactly_once() {
        const VISIBILITY_MS: u64 = 1_000;

        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();
        let (_container, url) = start_redis().await;

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![slow_job(VISIBILITY_MS)],
            &state,
            &shutdown,
            &config(&url, 2, VISIBILITY_MS),
            true,
        )
        .expect("redis runtime starts");

        job::enqueue("lease_slow_job", serde_json::json!({}))
            .await
            .expect("enqueue");
        assert!(
            wait_until(Duration::from_secs(10), || slow_runs() == 1).await,
            "the slow job starts"
        );
        tokio::time::sleep(Duration::from_millis(4 * VISIBILITY_MS)).await;

        assert_eq!(
            slow_runs(),
            1,
            "a job longer than the visibility timeout must run exactly once"
        );
        let completed = list(&url, &format!("{KEY_PREFIX}:completed")).await;
        assert_eq!(completed.len(), 1, "the job completed once: {completed:?}");
        assert!(
            completed[0].contains("\"attempt\":1"),
            "the first claim completed it: {completed:?}"
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }

    /// AC3 on Redis: a killed worker's job is recovered and run again.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_killed_workers_job_is_recovered_after_the_visibility_timeout() {
        const VISIBILITY_MS: u64 = 1_000;

        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();
        let (_container, url) = start_redis().await;

        let url_a = url.clone();
        run_then_kill(move || {
            Box::pin(async move {
                let state = AppState::for_test().with_profile("dev");
                let shutdown = tokio_util::sync::CancellationToken::new();
                job::start_runtime(
                    vec![killed_job()],
                    &state,
                    &shutdown,
                    &config(&url_a, 1, VISIBILITY_MS),
                    true,
                )
                .expect("worker A starts");
                job::enqueue("lease_killed_job", serde_json::json!({}))
                    .await
                    .expect("enqueue");
                assert!(
                    wait_until(Duration::from_secs(10), || killed_runs() == 1).await,
                    "worker A starts the job"
                );
                tokio::time::sleep(Duration::from_millis(VISIBILITY_MS)).await;
            })
        });
        let killed_at = Instant::now();
        job::clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![killed_job()],
            &state,
            &shutdown,
            &config(&url, 1, VISIBILITY_MS),
            true,
        )
        .expect("worker B starts");

        assert!(
            wait_until(Duration::from_secs(20), || killed_runs() == 2).await,
            "worker B recovers and runs the job"
        );
        assert!(
            killed_at.elapsed() >= Duration::from_millis(VISIBILITY_MS / 2),
            "recovery waits for the visibility timeout; took {:?}",
            killed_at.elapsed()
        );
        let completed_key = format!("{KEY_PREFIX}:completed");
        let mut completed = Vec::new();
        for _ in 0..200 {
            completed = list(&url, &completed_key).await;
            if !completed.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(completed.len(), 1, "the job completed once: {completed:?}");
        assert!(
            completed[0].contains("\"attempt\":2"),
            "the recovered second attempt completed it: {completed:?}"
        );

        shutdown.cancel();
        job::clear_global_job_client();
    }

    /// AC4 on Redis: a job over its timeout fails, retries, and frees the
    /// only worker.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_job_exceeding_its_timeout_is_failed_retried_and_frees_the_worker() {
        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();
        let (_container, url) = start_redis().await;

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![hung_job(), after_hung_job()],
            &state,
            &shutdown,
            &config(&url, 1, 30_000),
            true,
        )
        .expect("redis runtime starts");

        job::enqueue("lease_hung_job", serde_json::json!({}))
            .await
            .expect("enqueue hung job");
        assert!(
            wait_until(Duration::from_secs(10), || hung_runs() == 1).await,
            "the hung job starts"
        );
        job::enqueue("lease_after_hung_job", serde_json::json!({}))
            .await
            .expect("enqueue second job");

        assert!(
            wait_until(Duration::from_secs(10), || after_hung_ran() == 1).await,
            "the second job runs on the freed worker"
        );
        let dead_key = format!("{KEY_PREFIX}:dead");
        let mut dead = Vec::new();
        for _ in 0..400 {
            dead = list(&url, &dead_key).await;
            if !dead.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(dead.len(), 1, "the hung job dead-letters: {dead:?}");
        assert!(
            dead[0].contains("timed out"),
            "the dead letter keeps the timeout error: {dead:?}"
        );
        assert_eq!(hung_runs(), 2, "the timeout was retried");

        shutdown.cancel();
        job::clear_global_job_client();
    }

    /// A clock one hour ahead of real time.
    struct SkewedClock;

    impl autumn_web::time::ClockSource for SkewedClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            #[allow(clippy::disallowed_methods, reason = "a test clock reads real time")]
            let now = chrono::Utc::now();
            now + chrono::Duration::hours(1)
        }
    }

    static SKEW_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn skew_runs() -> usize {
        SKEW_RUNS.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// AC5: the stale-claim check uses Redis server time. Worker A claims a
    /// job with a correct clock. Worker B's clock is one hour ahead. On a
    /// worker-clock comparison, B sees A's live claim as expired and runs the
    /// job a second time. The visibility timeout (10s) is far longer than the
    /// job (2s), so the heartbeat alone cannot hide a worker-clock check.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_stale_claim_check_uses_server_time() {
        const VISIBILITY_MS: u64 = 10_000;

        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        SKEW_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
        let (_container, url) = start_redis().await;

        let skew_job = || {
            autumn_web::job::JobInfo::new("lease_skew_job", 3, 10, |_state, _payload| {
                Box::pin(async move {
                    SKEW_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Ok(())
                })
            })
        };

        let state_a = AppState::for_test().with_profile("dev");
        let shutdown_a = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![skew_job()],
            &state_a,
            &shutdown_a,
            &config(&url, 1, VISIBILITY_MS),
            true,
        )
        .expect("worker A starts");
        job::enqueue("lease_skew_job", serde_json::json!({}))
            .await
            .expect("enqueue");
        assert!(
            wait_until(Duration::from_secs(10), || skew_runs() == 1).await,
            "worker A claims the job"
        );

        let state_b = AppState::for_test()
            .with_profile("dev")
            .with_clock(Arc::new(SkewedClock));
        let shutdown_b = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![skew_job()],
            &state_b,
            &shutdown_b,
            &config(&url, 1, VISIBILITY_MS),
            true,
        )
        .expect("worker B starts");

        // Worker B sweeps at once and then every second.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(
            skew_runs(),
            1,
            "a worker with a skewed clock must not steal a live claim"
        );

        shutdown_a.cancel();
        shutdown_b.cancel();
        job::clear_global_job_client();
    }
}
