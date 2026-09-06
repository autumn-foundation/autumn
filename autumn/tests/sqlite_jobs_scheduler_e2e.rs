//! End-to-end proof that durable jobs and the single-host scheduler run on the
//! `SQLite` backend (issue #1907).
//!
//! Issue #1907 asks for a single-host scheduling strategy — in-process
//! coordination or file/table-based locking — to replace the Postgres
//! advisory-lock approach, so jobs and scheduled tasks run correctly on
//! `SQLite`. This suite covers both halves of that answer.
//!
//! **The in-process substitutes** (tests 1-3) are the defaults, and cost
//! nothing to run:
//!
//! - `jobs.backend = "local"` drives an in-process Tokio queue — no Postgres
//!   `LISTEN/NOTIFY`, `FOR UPDATE SKIP LOCKED`, or advisory locks.
//! - `scheduler.backend = "in_process"` coordinates ticks with
//!   [`InProcessSchedulerCoordinator`], which needs no `pg_advisory_lock`.
//! - The Postgres-only paths are refused under the `sqlite` feature with
//!   actionable messages, rather than mis-typed against a `SQLite` pool.
//!
//! **The durable, table-backed substitutes** (tests 4-15) are what an app picks
//! when work must survive a restart or be shared by two processes on the host:
//!
//! - `jobs.backend = "sqlite"` keeps the queue in `autumn_jobs` in the app's own
//!   file. Tests cover a job running end to end, durability across a restart,
//!   recovery of a claim a crashed worker left behind, retry-then-dead-letter,
//!   delayed enqueues, exactly-once claiming under four competing workers,
//!   uniqueness coalescing, concurrency limits, and the table-backed dashboard.
//! - `scheduler.backend = "sqlite"` leases each `(task, tick)`, so exactly one
//!   of two coordinators wins a tick, a released tick frees, a per-replica task
//!   never contends, and an expired lease is stealable.
//!
//! Boundary note: the scheduler tests drive the coordinator directly — the exact
//! acquire/run/release the scheduler loop performs per tick — so the
//! coordination path is asserted without booting a full timer loop. Registering
//! a `#[scheduled]` task and letting the app's loop fire it is an
//! `AppBuilder`-level concern exercised elsewhere.
//!
//! The job client is process-global, so every test that installs a runtime holds
//! [`job::global_job_runtime_test_lock`] for its duration.
//!
//! A **file** target (tempfile) is deliberate throughout: an in-memory `SQLite`
//! database is private per connection, so a second process — or a restarted one —
//! would see nothing.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features "sqlite,test-support" \
//!     --test sqlite_jobs_scheduler_e2e
//! ```
#![cfg(feature = "sqlite")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::AppState;
use autumn_web::config::{DatabaseConfig, JobConfig, SchedulerBackend, SchedulerConfig};
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::job::{
    self, JobAdminQuery, JobConcurrency, JobInfo, JobUniqueness, JobUniquenessWindow,
};
use autumn_web::lock;
use autumn_web::scheduler;
use autumn_web::task::TaskCoordination;

use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

/// Incremented from inside the job handler so the test can prove the handler
/// body actually ran (job handlers are bare `fn` pointers and cannot capture,
/// so a process-global counter is the faithful in-band signal).
static JOB_RAN: AtomicUsize = AtomicUsize::new(0);

/// Build a real, tempfile-backed `SQLite` pool through the public `create_pool`
/// entry point — the same path the runtime uses for a `sqlite:` URL.
fn build_sqlite_pool(tmp: &tempfile::TempDir) -> SqlitePool {
    let db_path = tmp.path().join("jobs_scheduler.db");
    let url = format!("sqlite://{}", db_path.display());
    let config = DatabaseConfig {
        url: Some(url),
        ..Default::default()
    };
    create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool path")
        .expect("a url is configured")
}

/// (1) The default in-process (`local`) job backend runs an enqueued job to
/// completion on a `SQLite`-backed app — no Postgres queue primitives involved.
#[tokio::test]
async fn local_job_backend_runs_a_job_end_to_end_on_sqlite() {
    // The job client is process-global, so runtime-installing tests in this
    // binary must not overlap.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    JOB_RAN.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);

    // A real SQLite-backed AppState.
    let state = AppState::for_test().with_profile("dev").with_pool(pool);
    let shutdown = tokio_util::sync::CancellationToken::new();

    // Start the DEFAULT job runtime. `JobConfig::default().backend == "local"`,
    // i.e. the in-process Tokio queue that is the documented SQLite default.
    let config = JobConfig::default();
    assert_eq!(
        config.backend, "local",
        "the default job backend must be the in-process `local` queue"
    );
    job::start_runtime(
        vec![JobInfo {
            version: 1,
            name: "sqlite_e2e_job".to_string(),
            max_attempts: 3,
            initial_backoff_ms: 10,
            queue: "default".to_string(),
            uniqueness: None,
            concurrency: None,
            handler: |_state, _payload| {
                Box::pin(async move {
                    JOB_RAN.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            },
        }],
        &state,
        &shutdown,
        &config,
        /* run_workers = */ true,
    )
    .expect("the local (in-process) job runtime starts on a sqlite app");

    // Enqueue through the public free function (the global client the runtime
    // just installed drives it onto the in-process queue).
    job::enqueue("sqlite_e2e_job", serde_json::json!({ "hello": "sqlite" }))
        .await
        .expect("enqueue routes to the running local runtime");

    // Drive to completion: poll the admin backend the runtime installed until
    // exactly one completion is recorded (bounded wait).
    let admin = job::job_admin_backend(&state).expect("local runtime installs an admin backend");
    let mut completed = 0;
    for _ in 0..200 {
        let snapshot = admin
            .snapshot(JobAdminQuery::default())
            .await
            .expect("admin snapshot");
        completed = snapshot.completed.total;
        if completed >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(
        completed, 1,
        "the enqueued job ran to completion through the in-process local backend"
    );
    assert_eq!(
        JOB_RAN.load(Ordering::SeqCst),
        1,
        "the job handler body actually executed exactly once"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// (2) The default `in_process` scheduler coordinator grants a lease, the task
/// body runs under it, and the lease releases — the single-host scheduling
/// strategy #1907 asks for, on a `SQLite`-backed app.
#[tokio::test]
async fn in_process_scheduler_coordinator_fires_a_task_on_sqlite() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test().with_profile("dev").with_pool(pool);

    // Build the DEFAULT scheduler coordinator from config against the real
    // SQLite AppState. `SchedulerConfig::default().backend == InProcess`.
    let config = SchedulerConfig::default();
    assert_eq!(
        config.backend,
        SchedulerBackend::InProcess,
        "the default scheduler backend must be the in-process coordinator"
    );
    let coordinator = scheduler::coordinator_from_config(&config, &state)
        .expect("the in_process coordinator builds on a sqlite app");
    assert_eq!(
        coordinator.backend(),
        "in_process",
        "the sqlite default resolves to the in-process coordinator"
    );

    // Fire one task tick exactly as the scheduler loop's `execute_fixed_delay_task`
    // does: acquire a lease, run the task body under it, then release.
    let ran = Arc::new(AtomicBool::new(false));
    let tick_key = scheduler::fixed_delay_tick_key(
        "sqlite_e2e_task",
        Duration::from_secs(60),
        autumn_web::time::clock_unix_duration(&autumn_web::time::SystemClock),
    );
    let lease = coordinator
        .try_acquire("sqlite_e2e_task", &tick_key, TaskCoordination::Fleet)
        .await
        .expect("try_acquire does not error on the in-process coordinator")
        .expect("the in-process coordinator always grants the single-host lease");
    assert_eq!(
        lease.backend(),
        "in_process",
        "a fleet task is coordinated in-process under the sqlite default"
    );

    // The task body runs while the lease is held.
    ran.store(true, Ordering::SeqCst);

    lease
        .release()
        .await
        .expect("the in-process lease releases cleanly");
    assert!(
        ran.load(Ordering::SeqCst),
        "the scheduled task body executed under the in-process lease"
    );
}

/// (3) The Postgres-only coordination paths are refused under the `sqlite`
/// feature with the documented, actionable messages — the guards cited on
/// #1907 that redirect operators to the in-process substitutes.
#[tokio::test]
async fn postgres_only_coordination_is_refused_under_sqlite() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test().with_profile("dev").with_pool(pool);

    // scheduler.backend = "postgres" is unsupported under sqlite.
    let pg_scheduler = SchedulerConfig {
        backend: SchedulerBackend::Postgres,
        ..SchedulerConfig::default()
    };
    // (`Arc<dyn SchedulerCoordinator>` is not `Debug`, so match rather than
    // `expect_err`.)
    let msg = match scheduler::coordinator_from_config(&pg_scheduler, &state) {
        Ok(_) => panic!("a postgres scheduler coordinator must be refused under sqlite"),
        Err(err) => err.to_string(),
    };
    assert!(
        msg.contains("scheduler.backend = \"in_process\""),
        "the refusal points at the in-process substitute; got: {msg}"
    );

    // jobs.backend = "postgres" is unsupported under sqlite.
    let pg_jobs = JobConfig {
        backend: "postgres".to_string(),
        ..JobConfig::default()
    };
    let shutdown = tokio_util::sync::CancellationToken::new();
    let err = job::start_runtime(Vec::new(), &state, &shutdown, &pg_jobs, false)
        .expect_err("a postgres job runtime must be refused under sqlite");
    let msg = err.to_string();
    assert!(
        msg.contains("jobs.backend=sqlite"),
        "the refusal points at the durable sqlite queue; got: {msg}"
    );
    assert!(
        msg.contains("jobs.backend=local"),
        "the refusal also names the in-process substitute; got: {msg}"
    );

    // Advisory locks require the Postgres backend. (`Lock` is not `Debug`.)
    let msg = match lock::Lock::from_state(&state, "sqlite_e2e_lock") {
        Ok(_) => panic!("advisory locks must be refused under sqlite"),
        Err(err) => err.to_string(),
    };
    assert!(
        msg.contains("advisory locks require the Postgres backend"),
        "the advisory-lock refusal names the Postgres requirement; got: {msg}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Durable SQLite job backend (`jobs.backend = "sqlite"`) and the single-host
// lease scheduler (`scheduler.backend = "sqlite"`) — issue #1907.
//
// The `local` backend above is in-process: a restart loses queued work, and two
// processes on one host share nothing. These tests cover the durable substitute
// the SQLite tier promises — a job queue that is a table in the same SQLite
// file, claimed with a single-writer claim and reclaimable after a crash.
// ─────────────────────────────────────────────────────────────────────────────

use autumn_web::config::{JobSqliteConfig, ProcessRole, split_role_requires_durable_backend};
use autumn_web::reexports::diesel;

/// One `BIGINT` column, for the counting assertions below.
#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    value: i64,
}

/// One `TEXT` column, for the status/error assertions below.
#[derive(diesel::QueryableByName)]
struct TextRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    // Imported inside the helpers only: `RunQueryDsl` has a blanket impl, so a
    // file-scope import would shadow `AtomicUsize::load` with its own `load`.
    use diesel_async::RunQueryDsl as _;
    let mut conn = pool.get().await.expect("sqlite connection");
    diesel::sql_query(sql)
        .get_result::<CountRow>(&mut *conn)
        .await
        .expect("count query")
        .value
}

/// Whether the runtime has created the durable queue table yet.
async fn queue_table_exists(pool: &SqlitePool) -> bool {
    use diesel_async::RunQueryDsl as _;
    let Ok(mut conn) = pool.get().await else {
        return false;
    };
    diesel::sql_query("SELECT COUNT(*) AS value FROM autumn_jobs")
        .get_result::<CountRow>(&mut *conn)
        .await
        .is_ok()
}

async fn text(pool: &SqlitePool, sql: &str) -> String {
    use diesel_async::RunQueryDsl as _;
    let mut conn = pool.get().await.expect("sqlite connection");
    diesel::sql_query(sql)
        .get_result::<TextRow>(&mut *conn)
        .await
        .expect("text query")
        .value
}

/// Poll `f` until it reports true, or fail after `tries` 25ms rounds.
async fn eventually(tries: usize, label: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..tries {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {label}");
}

/// A durable job config pinned to the SQLite backend.
fn sqlite_job_config(max_attempts: u32) -> JobConfig {
    JobConfig {
        backend: "sqlite".to_string(),
        workers: 2,
        max_attempts,
        initial_backoff_ms: 10,
        sqlite: JobSqliteConfig {
            visibility_timeout_ms: 500,
            poll_interval_ms: 20,
        },
        ..JobConfig::default()
    }
}

fn job_info(name: &str, max_attempts: u32, handler: autumn_web::job::JobHandler) -> JobInfo {
    JobInfo {
        version: 1,
        name: name.to_string(),
        max_attempts,
        initial_backoff_ms: 10,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler,
    }
}

static DURABLE_RAN: AtomicUsize = AtomicUsize::new(0);

/// (4) `jobs.backend = "sqlite"` runs an enqueued job to completion, and the
/// work is a row in the app's own SQLite file rather than in-process state.
#[tokio::test]
async fn sqlite_job_backend_runs_a_job_end_to_end() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    DURABLE_RAN.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    job::start_runtime(
        vec![job_info("sqlite_durable_job", 3, |_state, _payload| {
            Box::pin(async move {
                DURABLE_RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })],
        &state,
        &shutdown,
        &sqlite_job_config(3),
        true,
    )
    .expect("the durable sqlite job runtime starts");

    job::enqueue("sqlite_durable_job", serde_json::json!({ "n": 1 }))
        .await
        .expect("enqueue routes to the sqlite backend");

    eventually(400, "the durable job to complete", async || {
        DURABLE_RAN.load(Ordering::SeqCst) == 1
    })
    .await;

    eventually(400, "the row to settle as completed", async || {
        count(
            &pool,
            "SELECT COUNT(*) AS value FROM autumn_jobs WHERE status = 'completed'",
        )
        .await
            == 1
    })
    .await;

    shutdown.cancel();
    job::clear_global_job_client();
}

static RESTART_RAN: AtomicUsize = AtomicUsize::new(0);

/// (5) The queue is durable: a job enqueued by a process that runs no workers
/// survives in the file and runs when a worker process starts later.
#[tokio::test]
async fn sqlite_job_backend_is_durable_across_a_restart() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    RESTART_RAN.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let handler: autumn_web::job::JobHandler = |_state, _payload| {
        Box::pin(async move {
            RESTART_RAN.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    };

    // Web role: installs the enqueue client, runs no workers.
    {
        let state = AppState::for_test()
            .with_profile("dev")
            .with_pool(pool.clone());
        let shutdown = tokio_util::sync::CancellationToken::new();
        job::start_runtime(
            vec![job_info("sqlite_restart_job", 3, handler)],
            &state,
            &shutdown,
            &sqlite_job_config(3),
            false,
        )
        .expect("the enqueue-only sqlite runtime starts");
        job::enqueue("sqlite_restart_job", serde_json::json!({}))
            .await
            .expect("enqueue persists the job");
        shutdown.cancel();
        job::clear_global_job_client();
    }

    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) AS value FROM autumn_jobs WHERE status = 'enqueued'"
        )
        .await,
        1,
        "the job waits in the SQLite file with no worker process alive"
    );
    assert_eq!(
        RESTART_RAN.load(Ordering::SeqCst),
        0,
        "no worker ran it yet"
    );

    // Worker role on the same file, as if the process restarted.
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(
        vec![job_info("sqlite_restart_job", 3, handler)],
        &state,
        &shutdown,
        &sqlite_job_config(3),
        true,
    )
    .expect("the worker sqlite runtime starts");

    eventually(400, "the persisted job to run after restart", async || {
        RESTART_RAN.load(Ordering::SeqCst) == 1
    })
    .await;

    shutdown.cancel();
    job::clear_global_job_client();
}

static RECOVERED_RAN: AtomicUsize = AtomicUsize::new(0);

/// (6) A crash mid-job leaves the row reclaimable: a claim older than the
/// visibility timeout is recovered and run.
#[tokio::test]
async fn sqlite_job_backend_recovers_a_crashed_claim() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    RECOVERED_RAN.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    job::start_runtime(
        vec![job_info("sqlite_recovered_job", 3, |_state, _payload| {
            Box::pin(async move {
                RECOVERED_RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })],
        &state,
        &shutdown,
        &sqlite_job_config(3),
        true,
    )
    .expect("the durable sqlite job runtime starts");

    // The runtime creates the queue schema on first use, so wait for the table
    // before writing to it directly.
    eventually(400, "the queue schema to be created", async || {
        queue_table_exists(&pool).await
    })
    .await;

    // Forge the row a dead worker would have left behind: claimed long ago and
    // never settled.
    {
        use diesel_async::RunQueryDsl as _;
        let mut conn = pool.get().await.expect("sqlite connection");
        diesel::sql_query(
            "INSERT INTO autumn_jobs \
             (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
              enqueued_at, run_at, started_at, claimed_by, claimed_at) \
             VALUES ('crashed-1', 'sqlite_recovered_job', 'default', '{}', 'running', 1, 3, 10, \
                     0, 0, 0, 'dead-worker', 0)",
        )
        .execute(&mut *conn)
        .await
        .expect("insert a stale claim");
    }

    eventually(400, "the stale claim to be recovered and run", async || {
        RECOVERED_RAN.load(Ordering::SeqCst) == 1
    })
    .await;

    shutdown.cancel();
    job::clear_global_job_client();
}

static FAILING_RUNS: AtomicUsize = AtomicUsize::new(0);

/// (7) A failing job retries with backoff and dead-letters on the final
/// attempt, exactly as on Postgres.
#[tokio::test]
async fn sqlite_job_backend_retries_then_dead_letters() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    FAILING_RUNS.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    job::start_runtime(
        vec![job_info("sqlite_failing_job", 2, |_state, _payload| {
            Box::pin(async move {
                FAILING_RUNS.fetch_add(1, Ordering::SeqCst);
                Err(autumn_web::AutumnError::internal_server_error_msg(
                    "always fails",
                ))
            })
        })],
        &state,
        &shutdown,
        &sqlite_job_config(2),
        true,
    )
    .expect("the durable sqlite job runtime starts");

    job::enqueue("sqlite_failing_job", serde_json::json!({}))
        .await
        .expect("enqueue");

    eventually(400, "the job to exhaust its attempts", async || {
        count(
            &pool,
            "SELECT COUNT(*) AS value FROM autumn_jobs WHERE status = 'failed'",
        )
        .await
            == 1
    })
    .await;

    assert_eq!(
        FAILING_RUNS.load(Ordering::SeqCst),
        2,
        "the handler ran once per attempt before dead-lettering"
    );
    let error = text(
        &pool,
        "SELECT last_error AS value FROM autumn_jobs WHERE status = 'failed'",
    )
    .await;
    assert!(
        error.contains("always fails"),
        "the dead-lettered row keeps the last error; got: {error}"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

static DELAYED_RAN: AtomicUsize = AtomicUsize::new(0);

/// (8) A delayed enqueue is not claimable before it is due.
#[tokio::test]
async fn sqlite_job_backend_honors_a_delayed_enqueue() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    DELAYED_RAN.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    job::start_runtime(
        vec![job_info("sqlite_delayed_job", 3, |_state, _payload| {
            Box::pin(async move {
                DELAYED_RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })],
        &state,
        &shutdown,
        &sqlite_job_config(3),
        true,
    )
    .expect("the durable sqlite job runtime starts");

    job::enqueue_in(
        "sqlite_delayed_job",
        serde_json::json!({}),
        Duration::from_secs(3600),
    )
    .await
    .expect("delayed enqueue");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        DELAYED_RAN.load(Ordering::SeqCst),
        0,
        "a job due in an hour must not run now"
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) AS value FROM autumn_jobs WHERE status = 'enqueued'"
        )
        .await,
        1,
        "the delayed job waits in the table"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

static ONCE_RAN: AtomicUsize = AtomicUsize::new(0);

/// (9) The single-writer claim runs each row exactly once, even with several
/// worker loops competing for the same backlog.
#[tokio::test]
async fn sqlite_job_backend_claims_each_job_exactly_once() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    ONCE_RAN.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    let mut config = sqlite_job_config(3);
    config.workers = 4;
    job::start_runtime(
        vec![job_info("sqlite_once_job", 3, |_state, _payload| {
            Box::pin(async move {
                ONCE_RAN.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })],
        &state,
        &shutdown,
        &config,
        true,
    )
    .expect("the durable sqlite job runtime starts");

    for n in 0..20 {
        job::enqueue("sqlite_once_job", serde_json::json!({ "n": n }))
            .await
            .expect("enqueue");
    }

    eventually(800, "all 20 jobs to complete", async || {
        count(
            &pool,
            "SELECT COUNT(*) AS value FROM autumn_jobs WHERE status = 'completed'",
        )
        .await
            == 20
    })
    .await;

    // Give any duplicate claim a chance to surface before asserting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        ONCE_RAN.load(Ordering::SeqCst),
        20,
        "each enqueued row ran exactly once across 4 competing workers"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// (10) A durable SQLite queue makes a web/worker split on one host valid.
#[test]
fn sqlite_jobs_backend_counts_as_durable_for_split_roles() {
    assert!(
        !split_role_requires_durable_backend(ProcessRole::Worker, "sqlite"),
        "a worker role on the durable sqlite queue is a valid split"
    );
    assert!(
        split_role_requires_durable_backend(ProcessRole::Worker, "local"),
        "the in-process local queue still cannot back a split role"
    );
}

/// (11) `scheduler.backend = "sqlite"` elects one leader per tick across
/// processes on the host, and a released lease frees the tick.
#[tokio::test]
async fn sqlite_scheduler_lease_elects_one_leader_per_tick() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test().with_profile("dev").with_pool(pool);

    let config_a = SchedulerConfig {
        backend: SchedulerBackend::Sqlite,
        replica_id: Some("replica-a".to_string()),
        ..SchedulerConfig::default()
    };
    let config_b = SchedulerConfig {
        backend: SchedulerBackend::Sqlite,
        replica_id: Some("replica-b".to_string()),
        ..SchedulerConfig::default()
    };
    let a = scheduler::coordinator_from_config(&config_a, &state).expect("coordinator a");
    let b = scheduler::coordinator_from_config(&config_b, &state).expect("coordinator b");
    assert_eq!(a.backend(), "sqlite");
    assert!(
        a.is_fleet_distributed(),
        "the lease coordinator distributes ticks across processes"
    );

    let lease_a = a
        .try_acquire("digest", "digest:1", TaskCoordination::Fleet)
        .await
        .expect("acquire does not error")
        .expect("the first coordinator wins the tick");
    assert_eq!(lease_a.leader_id(), "replica-a");

    let lost = b
        .try_acquire("digest", "digest:1", TaskCoordination::Fleet)
        .await
        .expect("acquire does not error");
    assert!(
        lost.is_none(),
        "the second coordinator must observe the tick as taken"
    );

    lease_a.release().await.expect("release");

    let lease_b = b
        .try_acquire("digest", "digest:1", TaskCoordination::Fleet)
        .await
        .expect("acquire does not error")
        .expect("a released tick is free again");
    assert_eq!(lease_b.leader_id(), "replica-b");
    lease_b.release().await.expect("release");

    // A per-replica task never contends: both coordinators run it.
    for coordinator in [&a, &b] {
        let lease = coordinator
            .try_acquire("heartbeat", "heartbeat:1", TaskCoordination::PerReplica)
            .await
            .expect("acquire does not error")
            .expect("per-replica ticks are never leased away");
        assert_eq!(lease.backend(), "per_replica");
        lease.release().await.expect("release");
    }
}

/// (12) An expired lease is stealable, so a crashed leader cannot wedge a task.
#[tokio::test]
async fn sqlite_scheduler_lease_expires_after_its_ttl() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test().with_profile("dev").with_pool(pool);

    let config = SchedulerConfig {
        backend: SchedulerBackend::Sqlite,
        lease_ttl_secs: 1,
        replica_id: Some("replica-a".to_string()),
        ..SchedulerConfig::default()
    };
    let a = scheduler::coordinator_from_config(&config, &state).expect("coordinator a");
    let b = scheduler::coordinator_from_config(
        &SchedulerConfig {
            replica_id: Some("replica-b".to_string()),
            ..config.clone()
        },
        &state,
    )
    .expect("coordinator b");

    // Drop the lease without releasing it: release is explicit, so this is the
    // leader crashing mid-tick.
    let lease = a
        .try_acquire("sweep", "sweep:1", TaskCoordination::Fleet)
        .await
        .expect("acquire")
        .expect("first acquire wins");
    drop(lease);

    assert!(
        b.try_acquire("sweep", "sweep:1", TaskCoordination::Fleet)
            .await
            .expect("acquire")
            .is_none(),
        "the lease is held until its TTL expires"
    );

    tokio::time::sleep(Duration::from_millis(1_100)).await;

    assert!(
        b.try_acquire("sweep", "sweep:1", TaskCoordination::Fleet)
            .await
            .expect("acquire")
            .is_some(),
        "an expired lease is stealable so a crashed leader cannot wedge the task"
    );
}

/// (13) A unique job coalesces duplicate enqueues, using the same partial
/// unique index the Postgres backend uses.
#[tokio::test]
async fn sqlite_job_backend_deduplicates_unique_enqueues() {
    let _guard = job::global_job_runtime_test_lock().lock().await;

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    let mut info = job_info("sqlite_unique_job", 3, |_state, _payload| {
        Box::pin(async move { Ok(()) })
    });
    info.uniqueness = Some(JobUniqueness {
        by: vec!["order_id".to_string()],
        window: JobUniquenessWindow::Running,
    });

    // No workers: nothing drains the queue, so the row count is the dedup
    // answer on its own.
    job::start_runtime(vec![info], &state, &shutdown, &sqlite_job_config(3), false)
        .expect("the enqueue-only sqlite runtime starts");

    for _ in 0..3 {
        job::enqueue("sqlite_unique_job", serde_json::json!({ "order_id": 7 }))
            .await
            .expect("enqueue");
    }
    // A different key is a different job.
    job::enqueue("sqlite_unique_job", serde_json::json!({ "order_id": 8 }))
        .await
        .expect("enqueue");

    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS value FROM autumn_jobs").await,
        2,
        "three enqueues of one unique key coalesce to a single row"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

static CONCURRENT_NOW: AtomicUsize = AtomicUsize::new(0);
static CONCURRENT_PEAK: AtomicUsize = AtomicUsize::new(0);

/// (14) A `#[job(concurrency = 1)]` limit is honored at claim time, so the
/// claim query never runs more than the declared number at once.
#[tokio::test]
async fn sqlite_job_backend_honors_a_concurrency_limit() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    CONCURRENT_NOW.store(0, Ordering::SeqCst);
    CONCURRENT_PEAK.store(0, Ordering::SeqCst);

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    let mut info = job_info("sqlite_capped_job", 3, |_state, _payload| {
        Box::pin(async move {
            let running = CONCURRENT_NOW.fetch_add(1, Ordering::SeqCst) + 1;
            CONCURRENT_PEAK.fetch_max(running, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(120)).await;
            CONCURRENT_NOW.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
    });
    info.concurrency = Some(JobConcurrency {
        limit: 1,
        key: None,
    });

    let mut config = sqlite_job_config(3);
    config.workers = 4;
    job::start_runtime(vec![info], &state, &shutdown, &config, true)
        .expect("the durable sqlite job runtime starts");

    for n in 0..4 {
        job::enqueue("sqlite_capped_job", serde_json::json!({ "n": n }))
            .await
            .expect("enqueue");
    }

    eventually(800, "all capped jobs to complete", async || {
        count(
            &pool,
            "SELECT COUNT(*) AS value FROM autumn_jobs WHERE status = 'completed'",
        )
        .await
            == 4
    })
    .await;

    assert_eq!(
        CONCURRENT_PEAK.load(Ordering::SeqCst),
        1,
        "the declared concurrency limit of 1 was never exceeded"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// (15) The durable backend installs a table-backed dashboard, so the admin
/// view reports the shared queue rather than one process's memory.
#[tokio::test]
async fn sqlite_job_backend_reports_the_shared_queue_to_the_dashboard() {
    let _guard = job::global_job_runtime_test_lock().lock().await;

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    job::start_runtime(
        vec![job_info("sqlite_admin_job", 3, |_state, _payload| {
            Box::pin(async move { Ok(()) })
        })],
        &state,
        &shutdown,
        &sqlite_job_config(3),
        false,
    )
    .expect("the enqueue-only sqlite runtime starts");

    job::enqueue("sqlite_admin_job", serde_json::json!({ "n": 1 }))
        .await
        .expect("enqueue");
    job::enqueue_in(
        "sqlite_admin_job",
        serde_json::json!({ "n": 2 }),
        Duration::from_secs(3600),
    )
    .await
    .expect("delayed enqueue");

    let admin = job::job_admin_backend(&state).expect("the sqlite runtime installs a dashboard");
    let snapshot = admin
        .snapshot(JobAdminQuery::default())
        .await
        .expect("admin snapshot");

    assert_eq!(
        snapshot.enqueued.total, 1,
        "the ready job shows as enqueued: {:?}",
        snapshot.enqueued.records
    );
    assert_eq!(
        snapshot.scheduled.total, 1,
        "the delayed job shows as scheduled: {:?}",
        snapshot.scheduled.records
    );
    let scheduled = snapshot
        .scheduled
        .records
        .first()
        .expect("one scheduled record");
    assert_eq!(scheduled.name, "sqlite_admin_job");
    assert!(
        scheduled.scheduled_for.is_some(),
        "a scheduled record reports its due time"
    );

    // A cancel of a still-enqueued job takes it out of every list.
    let ready = snapshot
        .enqueued
        .records
        .first()
        .expect("one enqueued record");
    admin.cancel(&ready.id).await.expect("cancel the ready job");
    let after = admin
        .snapshot(JobAdminQuery::default())
        .await
        .expect("admin snapshot");
    assert_eq!(
        after.enqueued.total, 0,
        "a canceled job leaves the enqueued list"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// (16) A tracked job's status record lives in the same file as the queue, so
/// it survives a restart and a web/worker split sees one record — not the
/// per-process memory the in-process backend falls back to.
#[tokio::test]
async fn sqlite_job_backend_tracks_job_status_durably() {
    let _guard = job::global_job_runtime_test_lock().lock().await;

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    // Enqueue-only, so the record is written by a process that never runs the
    // job — the web half of a split.
    job::start_runtime(
        vec![job_info("sqlite_tracked_job", 3, |_state, _payload| {
            Box::pin(async move { Ok(()) })
        })],
        &state,
        &shutdown,
        &sqlite_job_config(3),
        false,
    )
    .expect("the enqueue-only sqlite runtime starts");

    let handle = autumn_web::job_tracking::enqueue_tracked(
        "sqlite_tracked_job",
        serde_json::json!({ "n": 1 }),
    )
    .await
    .expect("tracked enqueue");
    assert!(!handle.token.is_empty());

    let key = autumn_web::auth::hash_api_token(&handle.token);
    assert_eq!(
        count(
            &pool,
            &format!("SELECT COUNT(*) AS value FROM autumn_job_tracking WHERE key = '{key}'"),
        )
        .await,
        1,
        "the tracked record is a row in the app's own file, not process memory"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// (17) A `unique_for_ms` window dedupes on time rather than on status, so a
/// second enqueue inside the window coalesces. The TTL window takes its own
/// query text, so it needs its own coverage.
#[tokio::test]
async fn sqlite_job_backend_deduplicates_inside_a_ttl_window() {
    let _guard = job::global_job_runtime_test_lock().lock().await;

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    let mut info = job_info("sqlite_ttl_unique_job", 3, |_state, _payload| {
        Box::pin(async move { Ok(()) })
    });
    info.uniqueness = Some(JobUniqueness {
        by: vec!["order_id".to_string()],
        window: JobUniquenessWindow::TtlMs(60_000),
    });

    job::start_runtime(vec![info], &state, &shutdown, &sqlite_job_config(3), false)
        .expect("the enqueue-only sqlite runtime starts");

    for _ in 0..3 {
        job::enqueue(
            "sqlite_ttl_unique_job",
            serde_json::json!({ "order_id": 7 }),
        )
        .await
        .expect("enqueue");
    }
    job::enqueue(
        "sqlite_ttl_unique_job",
        serde_json::json!({ "order_id": 8 }),
    )
    .await
    .expect("enqueue");

    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS value FROM autumn_jobs").await,
        2,
        "repeat enqueues inside the TTL window coalesce; a different key does not"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// (18) The dashboard's retry and discard operate on the table, so an operator
/// action on one process is visible to every process on the host.
#[tokio::test]
async fn sqlite_job_backend_dashboard_retries_and_discards_failed_jobs() {
    let _guard = job::global_job_runtime_test_lock().lock().await;

    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = build_sqlite_pool(&tmp);
    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();

    // Enqueue-only, so nothing re-runs a row this test moves back to enqueued.
    job::start_runtime(
        vec![job_info("sqlite_dashboard_job", 1, |_state, _payload| {
            Box::pin(async move { Ok(()) })
        })],
        &state,
        &shutdown,
        &sqlite_job_config(1),
        false,
    )
    .expect("the enqueue-only sqlite runtime starts");

    job::enqueue("sqlite_dashboard_job", serde_json::json!({}))
        .await
        .expect("enqueue");
    eventually(200, "the queue schema to be created", async || {
        queue_table_exists(&pool).await
    })
    .await;

    // Settle the row as failed, the state an operator retries or discards from.
    {
        use diesel_async::RunQueryDsl as _;
        let mut conn = pool.get().await.expect("sqlite connection");
        // `finished_at` must be inside the dashboard's 7-day failed window, so
        // reuse the row's own enqueue instant.
        diesel::sql_query(
            "UPDATE autumn_jobs SET status = 'failed', finished_at = enqueued_at, \
             last_error = 'boom'",
        )
        .execute(&mut *conn)
        .await
        .expect("mark failed");
    }

    let admin = job::job_admin_backend(&state).expect("the sqlite runtime installs a dashboard");
    let failed = admin
        .snapshot(JobAdminQuery::default())
        .await
        .expect("admin snapshot")
        .failed;
    assert_eq!(failed.total, 1, "the failed job shows on the dashboard");
    let id = failed
        .records
        .first()
        .expect("one failed record")
        .id
        .clone();

    admin.retry(&id).await.expect("retry the failed job");
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) AS value FROM autumn_jobs WHERE status = 'enqueued' AND attempt = 1"
        )
        .await,
        1,
        "retry puts the row back in the queue on a fresh attempt"
    );

    // A retry of a row that is no longer failed is a not-found, not a silent
    // no-op.
    let err = admin
        .retry(&id)
        .await
        .expect_err("a job that is not failed cannot be retried");
    assert_eq!(err.status().as_u16(), 404, "got: {err:?}");

    // Discard needs a failed row again.
    {
        use diesel_async::RunQueryDsl as _;
        let mut conn = pool.get().await.expect("sqlite connection");
        diesel::sql_query("UPDATE autumn_jobs SET status = 'failed', finished_at = enqueued_at")
            .execute(&mut *conn)
            .await
            .expect("mark failed");
    }
    admin.discard(&id).await.expect("discard the failed job");
    let after = admin
        .snapshot(JobAdminQuery::default())
        .await
        .expect("admin snapshot");
    assert_eq!(
        after.failed.total, 0,
        "a discarded job leaves every dashboard list"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
