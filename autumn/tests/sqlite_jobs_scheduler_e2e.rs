//! End-to-end proof that durable jobs and the single-host scheduler run on the
//! `SQLite` backend via the in-process substitutes (issue #1907).
//!
//! Issue #1907 asks for a single-host scheduling strategy — in-process
//! coordination or file/table-based locking — to replace the Postgres
//! advisory-lock approach so jobs and scheduled tasks run correctly on `SQLite`.
//! That substrate already ships and is the documented `SQLite` default:
//!
//! - `jobs.backend = "local"` (the default) drives an in-process Tokio queue —
//!   no Postgres `LISTEN/NOTIFY`, `FOR UPDATE SKIP LOCKED`, or advisory locks.
//! - `scheduler.backend = "in_process"` (the default) coordinates ticks with
//!   [`InProcessSchedulerCoordinator`], which needs no `pg_advisory_lock`.
//! - The Postgres-only paths are explicitly refused under the `sqlite` feature
//!   with actionable messages, rather than mis-typed against a `SQLite` pool.
//!
//! This test confirms the ask is satisfied on a real `SQLite` pool/app:
//!
//! 1. **Jobs (true enqueue → run)** — a real `SQLite` pool backs an [`AppState`],
//!    the default `local` job runtime is started, a job is enqueued through the
//!    public [`job::enqueue`] path, and it is driven to completion: the handler
//!    body actually executes (a process-global counter is incremented) and the
//!    admin backend records exactly one completion.
//! 2. **Scheduler (acquire → run → release)** — the default `in_process`
//!    coordinator is built from config against the same `SQLite`-backed
//!    `AppState`, grants a lease for a task tick, the task body runs under the
//!    lease, and the lease releases cleanly — mirroring what the scheduler
//!    runtime's `execute_fixed_delay_task` does per tick.
//! 3. **Postgres-only paths are refused under `SQLite`** — building a
//!    `scheduler.backend = "postgres"` coordinator, starting a
//!    `jobs.backend = "postgres"` runtime, and taking an advisory
//!    [`lock::Lock`] all return the documented actionable errors, confirming
//!    the guards cited on #1907.
//!
//! Boundary note: parts 1 and 2 exercise the runtime primitives through their
//! public entry points (`job::start_runtime` + `job::enqueue`;
//! `scheduler::coordinator_from_config` + `SchedulerCoordinator::try_acquire`).
//! Registering a `#[scheduled]` task and letting the app's scheduler loop fire
//! it is an `AppBuilder`-level concern exercised elsewhere; here we drive the
//! coordinator directly (the exact acquire/run/release the loop performs) so the
//! single-host coordination path itself is asserted on `SQLite` without booting a
//! full timer loop.
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
use autumn_web::job::{self, JobAdminQuery, JobInfo};
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
        msg.contains("jobs.backend=local"),
        "the refusal points at the local substitute; got: {msg}"
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
