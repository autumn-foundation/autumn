//! W5.0 Definition-of-Done proof (sim-testing, issue #1797): the chaos lane
//! injects **deterministic, seed-driven** faults, and the same seed replays the
//! same fault schedule.
//!
//! This is the W5.0 "chaos scaffolding + chaos-v1 base" acceptance test. It:
//!
//! 1. Builds a [`Sim`] with an active [`Chaos`] config
//!    ([`db_transient_errors`](autumn_web::sim::Chaos::db_transient_errors),
//!    [`job_duplicate_delivery`](autumn_web::sim::Chaos::job_duplicate_delivery),
//!    and [`clock_skew`](autumn_web::sim::Chaos::clock_skew)) mounted on a fresh,
//!    migrated in-memory `SQLite` substrate
//!    ([`SqliteSubstrate`](autumn_web::sim::substrate::SqliteSubstrate)).
//! 2. Drives **real DB checkouts** (requests to a `Db`-extractor route, which
//!    the transient-error interceptor sits on) and **real job enqueues** (the
//!    duplicate-delivery interceptor fires at the enqueue seam and re-enqueues
//!    the same job), then drains the local job runtime.
//! 3. Reads the recorded fault schedule via `Sim::__chaos_events()` and asserts
//!    **two same-seed runs produce identical schedules**, while a different seed
//!    (overwhelmingly likely) diverges. The number of jobs actually drained
//!    (`perform_enqueued_jobs().len()`) confirms each fired duplicate really
//!    caused an extra delivery.
//! 4. Confirms a **default (no-chaos) sim records zero events** and behaves
//!    exactly as before (requests succeed, no job is duplicated).
//!
//! Needs no Docker: the substrate is in-memory `SQLite`, so this is a fast,
//! non-`#[ignore]`d test. The tests run on the same paused, current-thread
//! runtime `#[sim_test]` uses (`start_paused = true`), which the sim's
//! [`advance`](autumn_web::sim::Sim::advance) requires.
//!
//! A **standalone** `[[test]]` binary (not part of the consolidated
//! `integration_tests` binary, which is Postgres-typed and does not compile under
//! `--features sqlite`), mirroring `sim_sqlite_substrate.rs`. It needs
//! `test-support` for the public [`TestApp`] harness, so the file is
//! `#![cfg(all(feature = "sqlite", feature = "test-support"))]` — a default
//! `cargo test` compiles it to an empty (passing) binary. Run it with:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_chaos`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{Chaos, ChaosEvent, ChaosHook, Sim};
use autumn_web::test::TestApp;

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Reuse the substrate lane's migration set (creates `sim_job_marks`), so the
/// mounted app has a live schema behind the `Db` extractor.
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/sim_sqlite_substrate");

/// A `Db`-extractor route: merely resolving `Db` checks out a connection, which
/// is exactly the seam the chaos transient-error interceptor sits on. A fired
/// fault turns this into a 503; otherwise it returns `ok`.
#[get("/touch")]
async fn touch(_db: Db) -> &'static str {
    "ok"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MarkArgs {
    /// Logical id; identical across a duplicated delivery (the duplicate is a
    /// re-enqueue of the same payload).
    id: i64,
}

/// A trivial background job. Its handler does no I/O — the W5.0 `DoD` asserts on
/// the *chaos schedule* (recorded at enqueue time) and the deterministic drain
/// count, not on a handler side effect, so a no-op handler keeps the drain fully
/// deterministic under the paused runtime.
#[job(name = "sim_chaos_mark", max_attempts = 1, backoff_ms = 1)]
async fn sim_chaos_mark(_state: AppState, _args: MarkArgs) -> AutumnResult<()> {
    Ok(())
}

/// Registers the `/touch` route and the drain job on the sim app.
struct ChaosPlugin;

impl Plugin for ChaosPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![touch]).jobs(jobs![sim_chaos_mark])
    }
}

const REQUESTS: usize = 8;
const ENQUEUES: i64 = 8;

/// Drive one full chaos scenario for `seed` and return the recorded fault
/// schedule plus the number of jobs the local runtime actually drained.
async fn run_chaos_scenario(seed: u64) -> (Vec<ChaosEvent>, usize) {
    // The `#[job]::enqueue` path uses the process-global job client; serialize on
    // its lock and start clean, mirroring the substrate DoD test.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");

    let mut sim = Sim::from_seed(seed);
    sim.chaos(
        Chaos::default()
            .db_transient_errors(0.5)
            .job_duplicate_delivery(0.5)
            .clock_skew(Duration::from_secs(3)),
    );
    sim.build(TestApp::new().plugin(ChaosPlugin).with_db(substrate.pool()));

    // Exercise the virtual clock (with the chaos skew wrapper installed) before
    // any work is enqueued.
    sim.advance(Duration::from_secs(1)).await;

    // Real DB checkouts through the chaos interceptor. A fired transient fault
    // makes this a 503; either way exactly one checkout decision is recorded.
    for _ in 0..REQUESTS {
        let _ = sim.client().get("/touch").send().await;
    }

    // Real job enqueues; a fired duplicate re-enqueues the same job at the
    // enqueue seam, so it is delivered (and drained) twice.
    for i in 0..ENQUEUES {
        job::enqueue("sim_chaos_mark", json!({ "id": i }))
            .await
            .expect("enqueue via the local job backend");
    }

    // Deterministic local drain: `perform_enqueued_jobs` dispatches every
    // recorded enqueue (originals + injected duplicate copies) exactly once and
    // returns the count. We use it rather than `Sim::run_to_idle` because
    // `run_to_idle`'s commit-hook drain queries `autumn_repository_commit_hooks`,
    // which the SQLite sim substrate does not create (only the app's registered
    // migrations are applied — see `sim/substrate.rs`), so `run_to_idle` panics
    // on this substrate unless the caller manually provisions the commit-hook
    // table (as the W4↔W2 integration test #2124 does).
    let drained = sim.client().perform_enqueued_jobs().await.len();

    let events = sim.__chaos_events();
    job::clear_global_job_client();
    (events, drained)
}

/// The W5.0 `DoD`: same seed ⇒ identical fault schedule; different seed ⇒ (very
/// likely) different; and the recorded schedule is internally consistent with
/// the observed duplicate deliveries.
#[tokio::test(start_paused = true)]
async fn same_seed_replays_identical_chaos_schedule() {
    let (events_a, drained_a) = run_chaos_scenario(0x5EED).await;
    let (events_b, drained_b) = run_chaos_scenario(0x5EED).await;

    assert_eq!(
        events_a, events_b,
        "same seed must replay the exact same fault schedule"
    );
    assert_eq!(
        drained_a, drained_b,
        "same seed must drain the same number of jobs"
    );

    // The schedule actually exercised both hooks.
    assert!(
        events_a.iter().any(|e| e.hook == ChaosHook::DbCheckout),
        "the run drove DB-checkout decisions"
    );
    assert!(
        events_a.iter().any(|e| e.hook == ChaosHook::JobDelivery),
        "the run drove job-delivery decisions"
    );

    // Exactly one job-delivery decision per enqueue (the injected duplicate's
    // re-entrant pass makes no new decision).
    let job_decisions = events_a
        .iter()
        .filter(|e| e.hook == ChaosHook::JobDelivery)
        .count();
    assert_eq!(
        job_decisions,
        usize::try_from(ENQUEUES).unwrap(),
        "one delivery decision per enqueue, duplicates excluded"
    );

    // Drained == originals + fired duplicates: each fired duplicate really
    // caused one extra delivery.
    let fired_dups = events_a
        .iter()
        .filter(|e| e.hook == ChaosHook::JobDelivery && e.fired)
        .count();
    assert!(
        fired_dups > 0,
        "at p=0.5 over {ENQUEUES} enqueues at least one duplicate should fire"
    );
    assert_eq!(
        drained_a,
        usize::try_from(ENQUEUES).unwrap() + fired_dups,
        "each fired duplicate adds exactly one extra delivery"
    );
}

/// A different seed produces (overwhelmingly likely) a different schedule — the
/// faults are seed-driven, not fixed.
#[tokio::test(start_paused = true)]
async fn different_seed_diverges() {
    let (events_a, _) = run_chaos_scenario(0x1111).await;
    let (events_b, _) = run_chaos_scenario(0x2222).await;
    assert_ne!(
        events_a, events_b,
        "different seeds should produce different fault schedules"
    );
}

/// A default (empty) `Chaos` installs nothing: no events are recorded, requests
/// succeed, and no job is duplicated — the pre-W5 behavior, unchanged.
#[tokio::test(start_paused = true)]
async fn default_sim_records_no_chaos_and_is_unchanged() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");

    // No `sim.chaos(...)` call: default Chaos is inactive.
    let mut sim = Sim::from_seed(0x5EED);
    sim.build(TestApp::new().plugin(ChaosPlugin).with_db(substrate.pool()));

    for _ in 0..REQUESTS {
        // No transient faults: every checkout succeeds.
        sim.client().get("/touch").send().await.assert_status(200);
    }
    for i in 0..ENQUEUES {
        job::enqueue("sim_chaos_mark", json!({ "id": i }))
            .await
            .expect("enqueue");
    }
    let drained = sim.client().perform_enqueued_jobs().await.len();

    assert!(
        sim.__chaos_events().is_empty(),
        "an inactive chaos config records no events"
    );
    assert_eq!(
        drained,
        usize::try_from(ENQUEUES).unwrap(),
        "no duplicate delivery: exactly one delivery per enqueue"
    );

    job::clear_global_job_client();
}
