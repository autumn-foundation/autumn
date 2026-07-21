//! W4 ↔ W2 integration proof (sim-testing, issue #1797): the W2 virtual-clock +
//! run-to-idle drain runs **end-to-end against the W4 `SQLite` substrate through
//! the real public [`Sim`] API**.
//!
//! W2 (#2104) shipped `Sim::build(TestApp)` / `advance` / `run_to_idle`; W4
//! (#2106) shipped `sim::substrate::SqliteSubstrate`, but wired it only through a
//! bare `TestApp` + `perform_enqueued_jobs` (the substrate `DoD`). This test closes
//! the loop the two waves left open: it attaches the substrate's `SQLite` pool to a
//! `TestApp` via [`TestApp::with_db`], mounts it with **`sim.build(app)`**, then
//! drives a delayed `#[job]` to completion purely with **`sim.advance` +
//! `sim.run_to_idle`** — no `perform_enqueued_jobs`, no wall-clock sleep — and
//! proves the drain executed against the migrated in-memory `SQLite` schema by
//! reading the row the job's handler wrote back with real SQL.
//!
//! It combines both waves' Definition-of-Done shapes in one path:
//!
//! * **W2 virtual time.** The job fails its first attempt, arming a **24-hour**
//!   retry backoff on tokio's paused timer. Only `sim.advance(24h)` (which steps
//!   the injected wall clock and tokio's timer wheel in lockstep) can make that
//!   retry come due, and it does so in microseconds of real time.
//! * **W4 `SQLite` substrate.** The mounted app runs on a fresh, migrated,
//!   in-process in-memory `SQLite` database (unique per sim, anchored by the
//!   substrate's kept-alive guard connection). The job's handler `INSERT`s a row
//!   through the app's pool, so the successful retry's effect is observable by
//!   querying `SQLite` afterwards.
//!
//! # Divergence (documented, consistent with RFC §12)
//!
//! A green run proves the orchestration / timing / ordering of the **local**
//! scheduler + job paths (the in-process scheduler coordinator and the local
//! `JobAdminMemoryBackend`, which are what a single-node `SQLite` app really runs).
//! It does **not** validate the Postgres advisory-lock scheduler leasing or the
//! durable Postgres `LISTEN`/`NOTIFY` + `SKIP LOCKED` job-queue claim/lock
//! semantics — both are compiled out under `--features sqlite` and remain the
//! province of the Postgres integration tests. See `autumn/src/sim/substrate.rs`.
//!
//! Needs no Docker: the substrate is in-memory `SQLite`.
//!
//! A **standalone** `[[test]]` binary (not part of the consolidated
//! `integration_tests` binary, which is Postgres-typed and does not compile under
//! `--features sqlite`), mirroring `sim_sqlite_substrate` and every other
//! `sqlite_*` integration test in this crate. It needs `test-support` for the
//! public [`TestApp`] harness, so the file is
//! `#![cfg(all(feature = "sqlite", feature = "test-support"))]` — a default
//! `cargo test` compiles it to an empty (passing) binary. Run it with:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_sqlite_integration`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;

use serde::{Deserialize, Serialize};

/// 24 hours — the retry backoff exercised through `sim.advance`.
const TWENTY_FOUR_HOURS: Duration = Duration::from_secs(24 * 3600);

/// The app-domain migration set: creates `sim_job_marks`. Reuses the fixture
/// #2106 added for the W4 substrate `DoD`.
const MARK_MIGRATIONS: EmbeddedMigrations =
    embed_migrations!("tests/fixtures/sim_sqlite_substrate");

/// The framework durable-repository-commit-hook queue table (`SQLite` variant),
/// copied verbatim from `autumn/repository_commit_hook_migrations_sqlite`.
///
/// This is load-bearing for the W4↔W2 integration: the merged W2
/// [`Sim::run_to_idle`] drains a **third** ready-work source — durable repository
/// commit hooks — whenever the mounted app has a DB pool, by querying
/// `autumn_repository_commit_hooks`. The W2 `DoD` test mounts **no** DB, so it never
/// exercises that lane; a DB-backed sim app (this test) does, so the substrate
/// must provision the table or `run_to_idle` panics on "no such table". The
/// framework's canonical migration set is `pub(crate)`, so this test fixture
/// copies the DDL; if the framework schema drifts, the framework's own
/// `sqlite_drain_ready_repository_commit_hooks` query fails against the stale
/// fixture here and this test catches it.
const COMMIT_HOOK_MIGRATIONS: EmbeddedMigrations =
    embed_migrations!("tests/fixtures/sim_sqlite_commit_hooks");

/// Times the delayed job's handler has entered this test. The global job-runtime
/// lock serializes access across the process; the test resets it to 0.
static MARK_RUNS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MarkArgs {
    id: i64,
    tag: String,
}

/// A background job whose retry backs off 24h and whose *successful* attempt
/// writes one row into the migration-created `sim_job_marks` table on the
/// substrate's `SQLite` pool.
///
/// The first attempt fails **before** writing (arming the 24h backoff on tokio's
/// paused timer), so the row appears only once `sim.advance(24h)` lets the retry
/// come due — making the row a clean end-to-end signal that (a) virtual time
/// drove the drain and (b) the drain executed real SQL against the in-memory
/// `SQLite` substrate mounted through `sim.build`.
#[job(name = "sim_delayed_mark", max_attempts = 2, backoff_ms = 86_400_000)]
async fn sim_delayed_mark(state: AppState, args: MarkArgs) -> AutumnResult<()> {
    // Import at the top of the fn scope (not after statements) so the
    // diesel-async `execute` trait method is in scope only here — never shadowing
    // `AtomicUsize::load` in the test body's assertions.
    use diesel_async::RunQueryDsl as _;

    let prior = MARK_RUNS.fetch_add(1, Ordering::SeqCst);
    if prior == 0 {
        // Fail the first attempt before touching the DB, arming the 24h backoff.
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "first attempt fails to force the 24h retry backoff",
        )));
    }

    let pool = state
        .pool()
        .expect("substrate pool must be wired into AppState via TestApp::with_db")
        .clone();
    let mut conn = pool
        .get()
        .await
        .map_err(|e| AutumnError::internal_server_error(std::io::Error::other(e.to_string())))?;
    diesel::sql_query("INSERT INTO sim_job_marks (id, tag) VALUES (?, ?)")
        .bind::<diesel::sql_types::BigInt, _>(args.id)
        .bind::<diesel::sql_types::Text, _>(args.tag)
        .execute(&mut *conn)
        .await
        .map_err(|e| AutumnError::internal_server_error(std::io::Error::other(e.to_string())))?;
    Ok(())
}

/// Registers the delayed drain job on the mounted app.
struct SimJobsPlugin;

impl Plugin for SimJobsPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![sim_delayed_mark])
    }
}

/// Reads the injected (virtual) wall clock, so the test can prove the same
/// `sim.build` mounted both the `SQLite` substrate and the sim clock.
#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

#[derive(diesel::QueryableByName)]
struct MarkRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    tag: String,
}

/// Query the substrate's `SQLite` pool for the `tag`s recorded so far, ordered by
/// id — the observable effect of the drained job.
///
/// The future holds a `SQLite` pooled connection (not `Send`) across an `await`,
/// which is inherent to the diesel-async `SQLite` backend and fine for this
/// single-threaded, current-thread sim test — hence the `future_not_send` allow.
#[allow(clippy::future_not_send)]
async fn recorded_tags(substrate: &SqliteSubstrate) -> Vec<String> {
    use diesel_async::RunQueryDsl as _;

    let pool = substrate.pool();
    let mut conn = pool.get().await.expect("checkout substrate connection");
    let rows: Vec<MarkRow> = diesel::sql_query("SELECT tag FROM sim_job_marks ORDER BY id")
        .load(&mut *conn)
        .await
        .expect("read sim_job_marks from the migrated SQLite schema");
    rows.into_iter().map(|r| r.tag).collect()
}

/// The W4 ↔ W2 integration `DoD`: a migrated in-memory `SQLite` substrate is mounted
/// through the **real** `Sim::build(TestApp::with_db(...))` seam, and a 24h-backoff
/// `#[job]` is driven to completion purely via `sim.advance` + `sim.run_to_idle`,
/// writing a row a follow-up SQL query reads back — proving the W2 drain runs
/// end-to-end against the W4 substrate through the public `Sim` API.
#[sim_test]
async fn w2_drain_runs_against_w4_sqlite_substrate(mut sim: Sim) {
    // The `#[job]` enqueue path uses the process-global job client, so serialize
    // on the shared runtime lock and start from a clean client + counter, exactly
    // like the W2 and W4 DoD tests.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    MARK_RUNS.store(0, Ordering::SeqCst);

    let wall_start = Instant::now();

    // (1) Fresh, migrated, in-process in-memory SQLite substrate (W4). It owns the
    //     kept-alive guard connection anchoring the shared-cache in-memory DB, so
    //     it must outlive the mounted app — it is a local binding that lives to the
    //     end of the test, keeping the schema alive for every pooled checkout.
    let substrate = SqliteSubstrate::with_migrations(&[&COMMIT_HOOK_MIGRATIONS, &MARK_MIGRATIONS])
        .expect("migrated substrate builds");

    // The migrated schema is live on the substrate pool, and empty, before mount.
    assert!(
        recorded_tags(&substrate).await.is_empty(),
        "no rows before the drain"
    );

    // (2) Attach the substrate's SQLite pool to a TestApp (the W4 seam) and mount
    //     it with the REAL W2 `sim.build` — which installs the sim's virtual clock
    //     and starts the in-process job runtime. This is the W4↔W2 wiring: one
    //     `sim.build` owns both the SQLite substrate and the sim clock.
    sim.build(
        TestApp::new()
            .plugin(SimJobsPlugin)
            .with_db(substrate.pool())
            .routes(routes![now_route]),
    );

    // The injected clock starts pinned at the fixed sim epoch — proving the sim
    // clock was installed by the same build that wired the SQLite pool.
    let before = sim.client().get("/now").send().await;
    before.assert_ok();
    assert_eq!(before.text(), "2020-01-01T00:00:00+00:00");

    // (3) Enqueue the job and drain once: attempt 1 runs, fails, and arms a 24h
    //     retry backoff on the paused tokio clock. Driven via `sim.run_to_idle`,
    //     not `perform_enqueued_jobs`.
    SimDelayedMarkJob::enqueue(MarkArgs {
        id: 1,
        tag: "drained".to_owned(),
    })
    .await
    .expect("enqueue via the local job backend");
    sim.client().assert_job_enqueued("sim_delayed_mark");

    sim.run_to_idle().await;
    assert_eq!(
        MARK_RUNS.load(Ordering::SeqCst),
        1,
        "attempt 1 should have run once and failed"
    );
    assert!(
        recorded_tags(&substrate).await.is_empty(),
        "the failed first attempt must not have written a row — the retry is still backing off in virtual time"
    );

    // (4) Jump virtual time forward 24h through the REAL `sim.advance`. This fires
    //     the retry-backoff timer AND steps the injected wall clock the same 24h,
    //     with no real sleeping; `run_to_idle` then settles the released retry.
    sim.advance(TWENTY_FOUR_HOURS).await;
    sim.run_to_idle().await;

    assert_eq!(
        MARK_RUNS.load(Ordering::SeqCst),
        2,
        "the 24h-backoff retry should have fired and succeeded in virtual time"
    );

    // The end-to-end proof: the retry executed against the in-memory SQLite
    // substrate mounted through `sim.build`, writing exactly one row that this
    // real SQL query reads back.
    assert_eq!(
        recorded_tags(&substrate).await,
        vec!["drained".to_owned()],
        "the W2 drain wrote exactly one row to the migrated W4 SQLite schema via the Sim API"
    );

    // The injected clock advanced exactly 24h (epoch + 1 day)...
    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), "2020-01-02T00:00:00+00:00");

    // ...while essentially no wall-clock time elapsed: the 24h backoff was slept
    // in virtual time, never on the real clock.
    let wall_elapsed = wall_start.elapsed();
    assert!(
        wall_elapsed < Duration::from_secs(1),
        "expected near-zero wall time for a 24h virtual backoff, but {wall_elapsed:?} elapsed"
    );

    job::clear_global_job_client();
}
