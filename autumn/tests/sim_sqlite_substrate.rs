//! W4 Definition-of-Done proof (sim-testing, issue #1797): the job/scheduler
//! drain runs end-to-end against an **in-process, in-memory SQLite** substrate
//! over the *representative* local scheduler + job paths.
//!
//! This is the W4 "SQLite sim DB lane" acceptance test. It:
//!
//! 1. Builds a fresh, migrated in-memory SQLite substrate
//!    ([`autumn_web::sim::substrate::SqliteSubstrate`]) — a shared-cache in-memory
//!    database anchored by a kept-alive guard connection, with a registered
//!    migration applied so the schema is live for every pooled checkout.
//! 2. Mounts an app on it through the existing `TestApp` DB seam (the same seam
//!    W2 will reuse when it wires the substrate into `Sim::build`).
//! 3. Resolves the **representative scheduler path** — the in-process
//!    coordinator — and confirms the Postgres advisory-lock scheduler is refused
//!    under the `sqlite` feature (the feature-unification hazard resolution).
//! 4. Enqueues a `#[job]` and drains it through the **local job runtime**
//!    (`perform_enqueued_jobs`), whose handler writes a row into the migrated
//!    SQLite schema — proving the drain ran end-to-end against in-process SQLite.
//!
//! # Divergence (documented, consistent with RFC §12)
//!
//! A green run proves the orchestration/timing/ordering of the **local**
//! scheduler + job paths. It does **not** validate the Postgres advisory-lock
//! scheduler leasing or the durable Postgres `LISTEN`/`NOTIFY` + `SKIP LOCKED`
//! job-queue claim/lock semantics — both are compiled out under `--features
//! sqlite` and remain the province of the Postgres integration tests. See
//! `autumn/src/sim/substrate.rs` for the full rationale.
//!
//! Needs no Docker: the substrate is in-memory SQLite, so this is a fast,
//! non-`#[ignore]`d test.
//!
//! A **standalone** `[[test]]` binary (not part of the consolidated
//! `integration_tests` binary, which is Postgres-typed and does not compile under
//! `--features sqlite`), mirroring every other `sqlite_*` integration test in
//! this crate. It needs `test-support` for the public [`TestApp`] harness, so the
//! file is `#![cfg(all(feature = "sqlite", feature = "test-support"))]` — a
//! default `cargo test` compiles it to an empty (passing) binary. Run it with:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_sqlite_substrate`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use autumn_web::app::AppBuilder;
use autumn_web::config::{SchedulerBackend, SchedulerConfig};
use autumn_web::job;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::scheduler::coordinator_from_config;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::task::TaskCoordination;
use autumn_web::test::TestApp;

use diesel_async::RunQueryDsl as _;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The app-registered migration set for this test: creates `sim_job_marks`.
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/sim_sqlite_substrate");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MarkArgs {
    id: i64,
    tag: String,
}

/// A background job whose handler writes one row into the migration-created
/// `sim_job_marks` table on the substrate's SQLite pool. Its side effect is the
/// row itself, so the drain is observable by querying SQLite afterwards.
#[job(name = "sim_write_mark", max_attempts = 1, backoff_ms = 1)]
async fn sim_write_mark(state: AppState, args: MarkArgs) -> AutumnResult<()> {
    let pool = state
        .pool()
        .expect("substrate pool must be wired into AppState")
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

/// Registers the drain job.
struct SimJobsPlugin;

impl Plugin for SimJobsPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![sim_write_mark])
    }
}

#[derive(diesel::QueryableByName)]
struct MarkRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    tag: String,
}

/// The full W4 DoD: migrated in-memory SQLite substrate → mounted app → local
/// scheduler resolves in-process (and rejects Postgres) → local job drain writes
/// to the migrated SQLite schema end-to-end.
#[tokio::test]
async fn job_and_scheduler_drain_end_to_end_on_sqlite_substrate() {
    // Serialize on the global-job-runtime lock (the `#[job]::enqueue` path uses
    // the process-global client) and start from a clean client, mirroring the
    // other global-runtime tests.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // (1) Fresh, migrated, in-process in-memory SQLite substrate.
    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");

    // The migrated schema is live on the substrate pool before any app is mounted.
    {
        let pool = substrate.pool();
        let mut conn = pool.get().await.expect("checkout substrate connection");
        let rows: Vec<MarkRow> = diesel::sql_query("SELECT tag FROM sim_job_marks WHERE 1 = 0")
            .load(&mut *conn)
            .await
            .expect("migrated `sim_job_marks` table is visible to the substrate pool");
        assert!(rows.is_empty(), "no rows before the drain");
    }

    // (2) Mount an app on the substrate through the existing TestApp DB seam.
    let client = TestApp::new()
        .plugin(SimJobsPlugin)
        .with_db(substrate.pool())
        .build();

    // (3) Representative scheduler path (feature-unification hazard resolution):
    //     the default backend resolves to the in-process coordinator, and it
    //     grants a fleet tick locally...
    let coordinator = coordinator_from_config(&SchedulerConfig::default(), client.state())
        .expect("in-process scheduler resolves under the sqlite feature");
    assert_eq!(
        coordinator.backend(),
        "in_process",
        "the sim exercises the in-process scheduler, not the Postgres advisory-lock coordinator"
    );
    let lease = coordinator
        .try_acquire("sim_tick", "sim_tick:0", TaskCoordination::Fleet)
        .await
        .expect("in-process acquisition does not fail")
        .expect("in-process coordinator grants the tick locally");
    assert_eq!(lease.backend(), "in_process");

    //     ...while the Postgres advisory-lock scheduler is refused under sqlite
    //     (it leases via pg_advisory_lock, which SQLite has no primitive for).
    let pg_config = SchedulerConfig {
        backend: SchedulerBackend::Postgres,
        ..SchedulerConfig::default()
    };
    // `Arc<dyn SchedulerCoordinator>` is not `Debug`, so match rather than
    // `expect_err` (mirroring the scheduler coordination tests).
    let Err(pg_err) = coordinator_from_config(&pg_config, client.state()) else {
        panic!("the Postgres advisory-lock scheduler must be refused under the sqlite feature");
    };
    assert!(
        pg_err.to_string().contains("postgres"),
        "rejection names the postgres backend: {pg_err}"
    );

    // (4) Enqueue a job and drain it through the local job runtime; the handler
    //     writes to the migrated SQLite schema.
    job::enqueue("sim_write_mark", json!({ "id": 1, "tag": "drained" }))
        .await
        .expect("enqueue via the local job backend");
    client.assert_job_enqueued("sim_write_mark");

    let report = client.perform_enqueued_jobs().await;
    report.assert_all_succeeded();
    assert_eq!(report.len(), 1, "exactly the one enqueued job drained");

    // The end-to-end effect: the drained job's row is present in the in-memory
    // SQLite substrate.
    {
        let pool = substrate.pool();
        let mut conn = pool.get().await.expect("checkout substrate connection");
        let rows: Vec<MarkRow> = diesel::sql_query("SELECT tag FROM sim_job_marks ORDER BY id")
            .load(&mut *conn)
            .await
            .expect("read back the drained row");
        assert_eq!(
            rows.iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(),
            vec!["drained"],
            "the local job drain wrote exactly one row to the migrated SQLite schema"
        );
    }

    job::clear_global_job_client();
}

/// Two substrates are fully isolated in-memory databases — the "two sims never
/// share state" guarantee. A row written through one substrate's pool is never
/// visible through the other's.
#[tokio::test]
async fn two_substrates_are_isolated_databases() {
    let a = SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("substrate A builds");
    let b = SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("substrate B builds");

    assert_ne!(
        a.url(),
        b.url(),
        "each substrate gets a distinct database name"
    );

    {
        let pool = a.pool();
        let mut conn = pool.get().await.expect("checkout A");
        diesel::sql_query("INSERT INTO sim_job_marks (id, tag) VALUES (1, 'only-in-a')")
            .execute(&mut *conn)
            .await
            .expect("write into A");
    }

    let pool_b = b.pool();
    let mut conn_b = pool_b.get().await.expect("checkout B");
    let rows: Vec<MarkRow> = diesel::sql_query("SELECT tag FROM sim_job_marks")
        .load(&mut *conn_b)
        .await
        .expect("read B");
    assert!(
        rows.is_empty(),
        "substrate B must not see substrate A's row — they are isolated databases"
    );
}
