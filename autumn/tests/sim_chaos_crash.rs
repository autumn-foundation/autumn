//! W5.c Definition-of-Done proof (sim-testing, issue #1797, item 7):
//! **kill-between-awaits + durable-queue recovery**.
//!
//! This is the W5.c "crash-recovery" acceptance test. It proves the [`Sim`]
//! kill/restart primitive recovers a **durable repository commit hook** that was
//! committed to the DB-backed queue but left un-drained when the process
//! "crashed" mid-flight:
//!
//! 1. Build a fresh, migrated in-memory `SQLite` substrate
//!    ([`SqliteSubstrate`](autumn_web::sim::substrate::SqliteSubstrate)) — whose
//!    `autumn_repository_commit_hooks` control-plane table is auto-provisioned by
//!    the substrate — and create a small side-effect table on it.
//! 2. Mount an app on it via the real public `Sim::build`, then enqueue a **real
//!    durable repository commit hook** through the real enqueue surface
//!    (`enqueue_repository_commit_hook_on_conn`) on the substrate pool. This is
//!    the representative crash point: the durable row is committed, its hook has
//!    **not** drained.
//! 3. [`Sim::kill`] the app (the in-process job runtime's in-flight work is
//!    cancelled without completing — modelling a process crash), then
//!    [`Sim::restart`] a fresh app on the **same** `substrate.pool()`.
//! 4. `Sim::run_to_idle()` drains the durable commit-hook queue, recovering and
//!    running the hook — its side-effect row appears exactly once
//!    (at-least-once / idempotent).
//! 5. Assert **same seed ⇒ identical crash schedule** across two runs (the crash
//!    schedule is a pure function of the seed).
//!
//! # Durable vs in-memory (stated plainly)
//!
//! Under `--features sqlite` the sim runs the in-memory `local` job backend,
//! which is **not durable**: a crash drops its mid-flight and still-queued jobs
//! by design, exactly as a real process crash would drop an in-memory queue. So
//! item 7's durable guarantee is asserted against the DB-backed repository
//! commit-hook queue, **not** the local job queue — the in-memory job queue's
//! by-design loss is documented, never pretended durable.
//!
//! Needs no Docker: the substrate is in-memory `SQLite`, so this is a fast,
//! non-`#[ignore]`d test. Runs on the same paused, current-thread runtime the
//! chaos `DoD` uses.
//!
//! A **standalone** `[[test]]` binary (not part of the consolidated
//! `integration_tests` binary, which is Postgres-typed and does not compile under
//! `--features sqlite`), mirroring `sim_chaos.rs` / `sim_sqlite_substrate.rs`. It
//! needs `test-support` for the public [`TestApp`] harness, so the file is
//! `#![cfg(all(feature = "sqlite", feature = "test-support"))]` — a default
//! `cargo test` compiles it to an empty (passing) binary. Run it with:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_chaos_crash`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_web::job;
use autumn_web::prelude::*;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sim::Sim;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::test::TestApp;

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use serde_json::json;

type SqlitePool = Pool<autumn_web::db::RuntimeConnection>;

static HANDLER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Read an atomic counter. Fully qualified because `diesel_async::RunQueryDsl`'s
/// blanket `load(self, conn)` is in scope and otherwise shadows `AtomicU64::load`.
fn load(counter: &AtomicU64) -> u64 {
    std::sync::atomic::AtomicU64::load(counter, Ordering::SeqCst)
}

/// A leaked, process-unique commit-hook handler key so each test run registers an
/// isolated runner (the runner registry is global; a shared key would let one
/// run's drain execute another run's closure).
fn unique_handler_key() -> &'static str {
    let n = HANDLER_SEQ.fetch_add(1, Ordering::Relaxed);
    Box::leak(format!("test::sim_chaos_crash::handler::{n}").into_boxed_str())
}

/// Create the side-effect table the recovered commit hook writes into. A plain,
/// non-tenant-scoped table so the recovery assertion is about durability, not
/// tenancy (tenant round-trip is covered by `sqlite_commit_hook_worker.rs`).
async fn create_recovery_table(pool: &SqlitePool) {
    let mut conn = pool.get().await.expect("checkout substrate connection");
    diesel::sql_query(
        "CREATE TABLE crash_recovery_marks (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create crash_recovery_marks table");
}

/// Count rows in the side-effect table (the recovered hook's observable effect).
async fn count_marks(pool: &SqlitePool) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT COUNT(*) AS n FROM crash_recovery_marks")
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count marks")
        .n
}

/// Enqueue a durable repository commit hook through the REAL enqueue surface on
/// the substrate pool — the committed, not-yet-drained row a crash leaves behind.
async fn enqueue_durable_hook(pool: &SqlitePool, handler_key: &str, idempotency_key: &str) {
    let mut conn = pool.get().await.expect("checkout");
    autumn_web::__private::enqueue_repository_commit_hook_on_conn(
        &mut conn,
        handler_key,
        "create",
        Some(idempotency_key),
        None,
        &json!({ "op": "create" }),
        &json!({ "body": "recovered" }),
    )
    .await
    .expect("enqueue durable commit hook");
}

/// The core W5.c `DoD`: a durable commit hook committed before a mid-flight crash
/// is recovered and run exactly once on restart against the same substrate.
///
/// The **in-memory** local job queue is deliberately not exercised here: under
/// the `sqlite` substrate the `local` job backend is process-global memory, so
/// this in-process harness cannot faithfully model a real process's memory wipe
/// (a job enqueued before the "crash" would either have already run during an
/// intervening `await` or survive in global memory). Its by-design non-durability
/// is documented on [`Sim::kill`](autumn_web::sim::Sim::kill) and in the
/// [`crash`](autumn_web::sim::crash) module docs rather than asserted through a
/// brittle harness-specific check; item 7's durable guarantee is asserted against
/// the DB-backed commit-hook queue below.
#[tokio::test(start_paused = true)]
async fn durable_commit_hook_survives_crash_and_is_recovered_on_restart() {
    // `Sim::build`/`restart` start the in-process job runtime, which touches the
    // process-global job client; serialize on its lock and start clean, mirroring
    // the substrate / chaos DoD tests.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let handler_key = unique_handler_key();

    // The durable hook's runner: its side effect is a row in `crash_recovery_marks`.
    // Registered before the (first) build and left registered across the crash —
    // modelling a real app re-registering its hook runners on boot.
    let runs = Arc::new(AtomicU64::new(0));
    let substrate = SqliteSubstrate::new().expect("empty substrate builds");
    create_recovery_table(&substrate.pool()).await;

    {
        let runner_pool = substrate.pool();
        let runner_runs = runs.clone();
        autumn_web::__private::register_repository_commit_hook_runner(
            handler_key,
            move |_ctx, record| {
                let pool = runner_pool.clone();
                let runs = runner_runs.clone();
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let body = record
                        .get("body")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?")
                        .to_string();
                    let mut conn = pool.get().await.map_err(|e| {
                        AutumnError::internal_server_error(std::io::Error::other(e.to_string()))
                    })?;
                    diesel::sql_query("INSERT INTO crash_recovery_marks (body) VALUES (?)")
                        .bind::<diesel::sql_types::Text, _>(body)
                        .execute(&mut conn)
                        .await
                        .map_err(|e| {
                            AutumnError::internal_server_error(std::io::Error::other(e.to_string()))
                        })?;
                    Ok(())
                }
            },
            |_ctx, _record| async { Ok(()) },
            |_ctx, _record| async { Ok(()) },
        );
    }

    // (1) Build the app on the substrate.
    let mut sim = Sim::from_seed(0xC0FFEE);
    sim.build(TestApp::new().with_db(substrate.pool()));

    // (2) Commit a durable hook — the representative crash point: the durable row
    //     is committed, but its hook has NOT drained.
    enqueue_durable_hook(&substrate.pool(), handler_key, "crash-idem-1").await;

    // Nothing has drained yet: the durable row is committed but its hook has not run.
    assert_eq!(
        count_marks(&substrate.pool()).await,
        0,
        "no recovery-mark row before the crash — the hook has not drained"
    );
    assert_eq!(
        load(&runs),
        0,
        "the durable hook must not have run before the crash"
    );

    // (3) Crash mid-flight, then restart a fresh app on the SAME substrate pool.
    sim.crash_and_restart(TestApp::new().with_db(substrate.pool()));

    // (4) Drain to idle on the restarted app: the durable commit-hook queue is
    //     drained, recovering and running the hook committed before the crash.
    sim.run_to_idle().await;

    // The durable guarantee: the hook was recovered and ran exactly once.
    assert_eq!(
        count_marks(&substrate.pool()).await,
        1,
        "the durable commit hook must be recovered and run exactly once after the crash+restart"
    );
    assert_eq!(
        load(&runs),
        1,
        "the recovered hook runner fired exactly once (at-least-once / idempotent)"
    );

    // Draining again is idempotent: the completed hook is not re-run.
    sim.run_to_idle().await;
    assert_eq!(
        count_marks(&substrate.pool()).await,
        1,
        "re-draining must not re-run the already-completed durable hook"
    );

    job::clear_global_job_client();
}

/// Same seed ⇒ identical crash schedule; different seeds diverge. The schedule is
/// a pure function of the seed, independent of any mounted app.
#[tokio::test(start_paused = true)]
async fn same_seed_replays_identical_crash_schedule() {
    let a = Sim::from_seed(0x5EED).crash_schedule();
    let b = Sim::from_seed(0x5EED).crash_schedule();
    assert_eq!(a, b, "same seed must replay an identical crash schedule");

    let c = Sim::from_seed(0x5EEF).crash_schedule();
    assert_ne!(
        a, c,
        "different seeds should (overwhelmingly likely) diverge"
    );

    // The representative realized crash point is the schedule's first entry and is
    // deterministic for a given seed.
    assert_eq!(
        Sim::from_seed(0x5EED).crash_point(),
        a.points().first().cloned(),
        "crash_point is the first (representative) entry of the schedule"
    );
    assert!(
        Sim::from_seed(0x5EED).crash_point().is_some(),
        "the default-length schedule always yields a representative crash point"
    );
}

/// A `kill` with no prior `build` is a harmless no-op, and `crash_schedule`
/// works before any app is mounted (it depends only on the seed).
#[tokio::test(start_paused = true)]
async fn kill_before_build_is_a_noop() {
    let mut sim = Sim::from_seed(1);
    sim.kill(); // no app mounted yet — must not panic
    assert!(
        sim.try_client().is_none(),
        "no client is mounted after a kill with no prior build"
    );
    assert_eq!(sim.crash_schedule().points().len(), 8);
}
