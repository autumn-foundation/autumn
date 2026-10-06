//! Issue #1720, slice 2: deferrable jobs wait on the durable job backends.
//!
//! While the cost signal is high, a worker does not claim a deferrable job.
//! The job stays enqueued and uses no attempt. A job that is not deferrable
//! on the same queue runs. When the signal falls, the deferred job runs.
//!
//! The SQLite case is in `tests/sqlite_jobs_scheduler_e2e.rs`. These cases
//! require Docker. CI's `--ignored` sweep runs them (see CLAUDE.md).

#![cfg(any(all(feature = "db", not(feature = "sqlite")), feature = "redis"))]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::config::JobConfig;
use autumn_web::cost::{CostAccountant, CostSignal, WorkKind, mark_deferrable};
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnResult};
use serde_json::Value;

const THRESHOLD: f64 = 100.0;

type Handler = fn(AppState, Value) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>>;

/// Poll `f` until it is true, or fail after 20 s.
async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Run the window scenario on `state` with `config`. `deferred` and `urgent`
/// count the runs of the two jobs.
async fn window_scenario(
    state: &AppState,
    config: &JobConfig,
    names: (&str, &str),
    handlers: (Handler, Handler),
    runs: (&'static AtomicUsize, &'static AtomicUsize),
) -> CostAccountant {
    let (deferred_name, urgent_name) = names;
    let (deferred, urgent) = runs;
    deferred.store(0, Ordering::SeqCst);
    urgent.store(0, Ordering::SeqCst);
    mark_deferrable(WorkKind::Job, deferred_name);

    let signal = CostSignal::new(Some(THRESHOLD));
    signal.set_recheck(Duration::from_secs(1));
    signal.set(THRESHOLD * 5.0);
    let accountant = CostAccountant::new(10);
    state.insert_extension(signal.clone());
    state.insert_extension(accountant.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    job::start_runtime(
        vec![
            JobInfo::new(deferred_name, 3, 10, handlers.0),
            JobInfo::new(urgent_name, 3, 10, handlers.1),
        ],
        state,
        &shutdown,
        config,
        true,
    )
    .expect("the job runtime starts");

    // The deferrable job is first in the queue.
    job::enqueue(deferred_name, serde_json::json!({}))
        .await
        .expect("enqueue");
    job::enqueue(urgent_name, serde_json::json!({}))
        .await
        .expect("enqueue");

    wait_for("the urgent job runs in the window", || {
        urgent.load(Ordering::SeqCst) == 1
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        deferred.load(Ordering::SeqCst),
        0,
        "no deferrable run in the window"
    );

    signal.set(THRESHOLD / 5.0);
    wait_for("the deferred job runs after the window", || {
        deferred.load(Ordering::SeqCst) == 1
    })
    .await;
    wait_for("both runs are metered", || {
        accountant.snapshot().jobs.total.runs == 2
    })
    .await;
    let jobs = accountant.snapshot().jobs;
    assert_eq!(jobs.shift.in_window_runs, 0, "{jobs:?}");
    assert_eq!(
        jobs.shift.shifted_runs, 1,
        "the held job is shifted: {jobs:?}"
    );
    assert_eq!(jobs.shift.ratio(), Some(1.0), "{jobs:?}");

    shutdown.cancel();
    job::clear_global_job_client();
    accountant
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod postgres {
    use super::*;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl as _};

    static DEFERRED: AtomicUsize = AtomicUsize::new(0);
    static URGENT: AtomicUsize = AtomicUsize::new(0);

    fn deferred(_: AppState, _: Value) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        Box::pin(async {
            DEFERRED.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn urgent(_: AppState, _: Value) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        Box::pin(async {
            URGENT.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[derive(diesel::QueryableByName)]
    struct ValueRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        value: i64,
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn postgres_deferrable_job_waits_for_the_cost_signal() {
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
            .with_pool(pool.clone());
        let config = JobConfig {
            backend: "postgres".to_owned(),
            workers: 2,
            ..Default::default()
        };
        window_scenario(
            &state,
            &config,
            ("pg_cost_deferred_job", "pg_cost_urgent_job"),
            (deferred, urgent),
            (&DEFERRED, &URGENT),
        )
        .await;

        let mut conn = pool.get().await.expect("conn");
        let first_attempt = diesel::sql_query(
            "SELECT COUNT(*) AS value FROM autumn_jobs \
             WHERE name = 'pg_cost_deferred_job' AND status = 'completed' AND attempt = 1",
        )
        .get_result::<ValueRow>(&mut conn)
        .await
        .expect("query")
        .value;
        assert_eq!(first_attempt, 1, "the deferred job used no attempt");
    }
}

#[cfg(feature = "redis")]
mod redis_backend {
    use super::*;
    use autumn_web::config::JobRedisConfig;

    static DEFERRED: AtomicUsize = AtomicUsize::new(0);
    static URGENT: AtomicUsize = AtomicUsize::new(0);

    fn deferred(_: AppState, _: Value) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        Box::pin(async {
            DEFERRED.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn urgent(_: AppState, _: Value) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        Box::pin(async {
            URGENT.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_deferrable_job_waits_for_the_cost_signal() {
        use testcontainers::runners::AsyncRunner as _;
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
        let config = JobConfig {
            backend: "redis".to_owned(),
            workers: 2,
            redis: JobRedisConfig {
                url: Some(format!("redis://127.0.0.1:{port}")),
                key_prefix: "autumn:cost-test".to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let state = AppState::for_test().with_profile("dev");
        window_scenario(
            &state,
            &config,
            ("redis_cost_deferred_job", "redis_cost_urgent_job"),
            (deferred, urgent),
            (&DEFERRED, &URGENT),
        )
        .await;
        assert_eq!(DEFERRED.load(Ordering::SeqCst), 1, "the job ran one time");
    }
}
