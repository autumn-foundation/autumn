//! Queue-pinning contract tests for issue #1623 (AC3).
//!
//! AC3: *"A worker-role process can be pinned to a subset of queues via
//! config/flags; a pinned process never claims jobs from queues outside its
//! subset, **on both Postgres and Redis backends**."*
//!
//! The unit tests in `autumn/src/job.rs` cover `QueueSchedule::retain_pinned` as
//! a pure function — they prove the schedule is filtered, not that the running
//! worker honors it. These tests close that gap end to end on each durable
//! backend: with jobs waiting in *both* an in-subset and an out-of-subset queue,
//! a pinned worker must execute only the in-subset one and leave the other
//! untouched, then an unpinned worker against the same store must drain it (so
//! the assertion can't pass merely because the job was undeliverable).
//!
//! Both require Docker (testcontainers) and are marked `#[ignore]`. Run:
//!
//! ```text
//! cargo test -p autumn-web --features redis,db \
//!   --test integration_tests queue_pinning -- --ignored
//! ```

// Both backends are feature-gated; without either, the shared fixture below has
// no callers, so skip the module rather than leave dead code behind.
#![cfg(any(feature = "redis", all(feature = "db", not(feature = "sqlite"))))]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::AutumnResult;
use autumn_web::job::JobInfo;
use serde_json::Value;
use tokio::time::sleep;

/// How long a pinned worker is given to (wrongly) claim the out-of-subset job.
/// Generous relative to both backends' idle poll so the negative assertion is
/// about pinning, not about a poll that hadn't come round yet.
const STARVATION_WINDOW: Duration = Duration::from_millis(1500);

static CRITICAL_RAN: AtomicUsize = AtomicUsize::new(0);
static BULK_RAN: AtomicUsize = AtomicUsize::new(0);

fn critical_handler(
    _state: autumn_web::AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    CRITICAL_RAN.fetch_add(1, Ordering::SeqCst);
    Box::pin(async move { Ok(()) })
}

fn bulk_handler(
    _state: autumn_web::AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    BULK_RAN.fetch_add(1, Ordering::SeqCst);
    Box::pin(async move { Ok(()) })
}

fn pinning_jobs() -> Vec<JobInfo> {
    let mut critical = JobInfo::new("pin_critical", 1, 10, critical_handler);
    critical.queue = "critical".to_string();
    let mut bulk = JobInfo::new("pin_bulk", 1, 10, bulk_handler);
    bulk.queue = "bulk".to_string();
    vec![critical, bulk]
}

fn reset_counters() {
    CRITICAL_RAN.store(0, Ordering::SeqCst);
    BULK_RAN.store(0, Ordering::SeqCst);
}

// Read the counters through free functions rather than `X.load(..)` at the call
// sites: the Postgres module imports `diesel_async::RunQueryDsl`, whose `load`
// method otherwise shadows `AtomicUsize::load` inside the polling closures.
fn critical_ran() -> usize {
    CRITICAL_RAN.load(Ordering::SeqCst)
}

fn bulk_ran() -> usize {
    BULK_RAN.load(Ordering::SeqCst)
}

/// Poll until `f` holds or the deadline passes. Returns whether it held.
async fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
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

#[cfg(feature = "redis")]
mod redis_backend {
    use super::{
        Duration, STARVATION_WINDOW, bulk_ran, critical_ran, pinning_jobs, reset_counters,
        wait_until,
    };

    use autumn_web::AppState;
    use autumn_web::config::{JobConfig, JobQueuesConfig, JobRedisConfig};
    use autumn_web::job;

    fn config(url: &str, pin: &[&str]) -> JobConfig {
        JobConfig {
            backend: "redis".to_owned(),
            workers: 2,
            // Both queues are configured; only `pin` narrows what this process
            // claims, so the test isolates pinning from queue configuration.
            queues: JobQueuesConfig::strict_list(["critical", "bulk"]),
            pin: pin.iter().map(|p| (*p).to_owned()).collect(),
            redis: JobRedisConfig {
                url: Some(url.to_owned()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// AC3 on Redis: a worker pinned to `critical` drains `critical` and never
    /// claims the `bulk` job waiting in the same Redis; an unpinned worker
    /// against that same Redis then drains it.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pinned_redis_worker_never_claims_an_out_of_subset_queue() {
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::redis::Redis as RedisImage;

        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();

        let container = RedisImage::default()
            .start()
            .await
            .expect("start Redis container");
        let port = container
            .get_host_port_ipv4(6379)
            .await
            .expect("redis port");
        let url = format!("redis://127.0.0.1:{port}");

        // A worker tier pinned to `critical` only.
        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            pinning_jobs(),
            &state,
            &shutdown,
            &config(&url, &["critical"]),
            true,
        )
        .expect("pinned worker runtime should start");

        job::enqueue("pin_bulk", serde_json::json!({}))
            .await
            .expect("enqueue bulk job");
        job::enqueue("pin_critical", serde_json::json!({}))
            .await
            .expect("enqueue critical job");

        assert!(
            wait_until(Duration::from_secs(5), || critical_ran() >= 1).await,
            "the pinned worker must drain its own `critical` queue",
        );
        // Give it a long window to (wrongly) pick the bulk job up as well.
        assert!(
            !wait_until(STARVATION_WINDOW, || bulk_ran() >= 1).await,
            "a worker pinned to `critical` must never claim from `bulk`",
        );

        shutdown.cancel();
        job::clear_global_job_client();

        // Control: the bulk job really is claimable — an unpinned worker against
        // the same Redis drains it. Without this the negative assertion above
        // would also pass if the enqueue had silently failed.
        let unpinned_state = AppState::for_test().with_profile("dev");
        let unpinned_shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            pinning_jobs(),
            &unpinned_state,
            &unpinned_shutdown,
            &config(&url, &[]),
            true,
        )
        .expect("unpinned worker runtime should start");

        assert!(
            wait_until(Duration::from_secs(5), || bulk_ran() >= 1).await,
            "an unpinned worker must drain the `bulk` job the pinned one left",
        );

        unpinned_shutdown.cancel();
        job::clear_global_job_client();
    }
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod postgres_backend {
    use super::{
        Duration, STARVATION_WINDOW, bulk_ran, critical_ran, pinning_jobs, reset_counters,
        wait_until,
    };

    use autumn_web::AppState;
    use autumn_web::config::{JobConfig, JobQueuesConfig};
    use autumn_web::job;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl as _};

    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }

    fn config(pin: &[&str]) -> JobConfig {
        JobConfig {
            backend: "postgres".to_owned(),
            workers: 2,
            queues: JobQueuesConfig::strict_list(["critical", "bulk"]),
            pin: pin.iter().map(|p| (*p).to_owned()).collect(),
            ..Default::default()
        }
    }

    async fn enqueued_in(pool: &Pool<AsyncPgConnection>, queue: &str) -> i64 {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "SELECT COUNT(*) AS count FROM autumn_jobs \
             WHERE queue = $1 AND status = 'enqueued'",
        )
        .bind::<diesel::sql_types::Text, _>(queue)
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count enqueued")
        .count
    }

    /// AC3 on Postgres: a worker pinned to `critical` drains `critical` and
    /// leaves the `bulk` row sitting `enqueued` in `autumn_jobs`; an unpinned
    /// worker against the same database then claims it.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pinned_postgres_worker_never_claims_an_out_of_subset_queue() {
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::postgres::Postgres;

        let _guard = job::global_job_runtime_test_lock().lock().await;
        job::clear_global_job_client();
        reset_counters();

        let container = Postgres::default()
            .start()
            .await
            .expect("start Postgres container");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        // Apply the crate's whole embedded migration set, exactly as a real app
        // does at boot — deliberately not a hand-picked `include_str!` list. A
        // list that misses one jobs migration produces a table the claim query
        // errors against on every poll, and `pg_claim_next_job` swallows that
        // error, so the worker would look like it is "correctly not claiming"
        // and this test would pass for entirely the wrong reason. (Not
        // hypothetical: the first draft picked three migrations, omitted
        // `add_pending_unique_key_to_jobs`, and the pinned worker claimed
        // nothing at all.)
        autumn_web::migrate::run_pending(&url, autumn_web::migrate::FRAMEWORK_MIGRATIONS)
            .expect("apply framework migrations");
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
        let pool = Pool::builder(manager).max_size(8).build().expect("pool");

        // A worker tier pinned to `critical` only.
        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            pinning_jobs(),
            &state,
            &shutdown,
            &config(&["critical"]),
            true,
        )
        .expect("pinned worker runtime should start");

        job::enqueue("pin_bulk", serde_json::json!({}))
            .await
            .expect("enqueue bulk job");
        job::enqueue("pin_critical", serde_json::json!({}))
            .await
            .expect("enqueue critical job");

        assert!(
            wait_until(Duration::from_secs(10), || critical_ran() >= 1).await,
            "the pinned worker must drain its own `critical` queue",
        );
        assert!(
            !wait_until(STARVATION_WINDOW, || bulk_ran() >= 1).await,
            "a worker pinned to `critical` must never claim from `bulk`",
        );
        // The row is still there, unclaimed — proving it was skipped, not lost.
        assert_eq!(
            enqueued_in(&pool, "bulk").await,
            1,
            "the out-of-subset job must remain enqueued, not be claimed or dropped",
        );

        shutdown.cancel();
        job::clear_global_job_client();

        // Control: an unpinned worker against the same database drains it.
        let unpinned_state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let unpinned_shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            pinning_jobs(),
            &unpinned_state,
            &unpinned_shutdown,
            &config(&[]),
            true,
        )
        .expect("unpinned worker runtime should start");

        assert!(
            wait_until(Duration::from_secs(10), || bulk_ran() >= 1).await,
            "an unpinned worker must drain the `bulk` job the pinned one left",
        );

        unpinned_shutdown.cancel();
        job::clear_global_job_client();
    }
}
