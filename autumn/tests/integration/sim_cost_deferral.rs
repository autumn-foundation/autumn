//! Cost-aware deferral (issue #1720), in virtual time.
//!
//! A synthetic high-cost window: the cost signal goes above the threshold,
//! stays there for ten virtual minutes, then falls. The tests check that:
//!
//! - deferrable jobs and tasks do not run in the window,
//! - they run when the signal falls, and none is dropped,
//! - work that is not deferrable, and request handlers, are not affected.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::cost::CostSignal;
use autumn_web::job;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use serde::{Deserialize, Serialize};

static DEFERRABLE_RUNS: AtomicUsize = AtomicUsize::new(0);
static URGENT_RUNS: AtomicUsize = AtomicUsize::new(0);
static DEFERRABLE_TICKS: AtomicUsize = AtomicUsize::new(0);
static URGENT_TICKS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Args;

#[job(name = "sim_cost_rebuild_index", deferrable)]
async fn sim_cost_rebuild_index(_state: AppState, _args: Args) -> AutumnResult<()> {
    DEFERRABLE_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[job(name = "sim_cost_send_receipt")]
async fn sim_cost_send_receipt(_state: AppState, _args: Args) -> AutumnResult<()> {
    URGENT_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[scheduled(every = "1m", name = "sim_cost_compact", deferrable)]
async fn sim_cost_compact(_state: AppState) -> AutumnResult<()> {
    DEFERRABLE_TICKS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[scheduled(every = "1m", name = "sim_cost_heartbeat")]
async fn sim_cost_heartbeat(_state: AppState) -> AutumnResult<()> {
    URGENT_TICKS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

const THRESHOLD: f64 = 400.0;
const RECHECK_SECS: u64 = 30;

fn config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.cost.defer_threshold = Some(THRESHOLD);
    config.cost.defer_recheck_secs = RECHECK_SECS;
    config
}

fn signal(sim: &Sim) -> CostSignal {
    sim.client()
        .state()
        .extension::<CostSignal>()
        .map(|s| (*s).clone())
        .expect("the framework installs a CostSignal")
}

/// Step one minute at a time and settle the work each step releases.
async fn advance_minutes(sim: &Sim, minutes: u64) {
    for _ in 0..minutes {
        sim.advance(Duration::from_secs(60)).await;
        sim.run_to_idle().await;
    }
}

#[sim_test]
async fn sim_deferrable_jobs_wait_for_the_window_then_all_run(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    DEFERRABLE_RUNS.store(0, Ordering::SeqCst);
    URGENT_RUNS.store(0, Ordering::SeqCst);

    sim.build(
        TestApp::new()
            .config(config())
            .routes(routes![ping])
            .jobs(jobs![sim_cost_rebuild_index, sim_cost_send_receipt]),
    );
    let signal = signal(&sim);
    assert_eq!(signal.threshold(), Some(THRESHOLD));

    // The window opens.
    signal.set(520.0);
    for _ in 0..3 {
        SimCostRebuildIndexJob::enqueue(Args).await.expect("enqueue");
    }
    SimCostSendReceiptJob::enqueue(Args).await.expect("enqueue");
    sim.run_to_idle().await;

    assert_eq!(URGENT_RUNS.load(Ordering::SeqCst), 1, "urgent work runs now");
    assert_eq!(DEFERRABLE_RUNS.load(Ordering::SeqCst), 0, "deferrable work waits");

    // Ten minutes of high signal: nothing deferrable runs. Requests do.
    for _ in 0..10 {
        advance_minutes(&sim, 1).await;
        sim.client().get("/ping").send().await.assert_ok();
    }
    assert_eq!(DEFERRABLE_RUNS.load(Ordering::SeqCst), 0, "no run in the window");
    assert!(signal.snapshot().deferrals >= 3, "{:?}", signal.snapshot());

    // The window closes. Every deferred job runs at the next recheck.
    signal.set(120.0);
    sim.advance(Duration::from_secs(RECHECK_SECS)).await;
    sim.run_to_idle().await;
    assert_eq!(
        DEFERRABLE_RUNS.load(Ordering::SeqCst),
        3,
        "all deferred jobs run, none is dropped"
    );
    assert_eq!(URGENT_RUNS.load(Ordering::SeqCst), 1);
}

#[sim_test]
async fn sim_deferrable_tasks_wait_for_the_window_then_resume(mut sim: Sim) {
    DEFERRABLE_TICKS.store(0, Ordering::SeqCst);
    URGENT_TICKS.store(0, Ordering::SeqCst);

    sim.build(
        TestApp::new()
            .config(config())
            .tasks(tasks![sim_cost_compact, sim_cost_heartbeat]),
    );
    let signal = signal(&sim);

    advance_minutes(&sim, 2).await;
    assert_eq!(DEFERRABLE_TICKS.load(Ordering::SeqCst), 2);
    assert_eq!(URGENT_TICKS.load(Ordering::SeqCst), 2);

    // Five minutes of high signal.
    signal.set(THRESHOLD + 1.0);
    advance_minutes(&sim, 5).await;
    assert_eq!(
        DEFERRABLE_TICKS.load(Ordering::SeqCst),
        2,
        "the deferrable task does not tick in the window"
    );
    assert_eq!(URGENT_TICKS.load(Ordering::SeqCst), 7, "other tasks tick");

    // The signal falls: the waiting tick runs at the next recheck, then the
    // task ticks as before.
    signal.set(THRESHOLD - 1.0);
    sim.advance(Duration::from_secs(RECHECK_SECS)).await;
    sim.run_to_idle().await;
    assert_eq!(DEFERRABLE_TICKS.load(Ordering::SeqCst), 3, "the tick resumes");

    advance_minutes(&sim, 2).await;
    assert_eq!(DEFERRABLE_TICKS.load(Ordering::SeqCst), 5);
}

#[sim_test]
async fn sim_deferrable_work_runs_when_no_threshold_is_set(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    DEFERRABLE_RUNS.store(0, Ordering::SeqCst);

    // No `defer_threshold`: the signal is never high.
    sim.build(TestApp::new().jobs(jobs![sim_cost_rebuild_index]));
    signal(&sim).set(1.0e6);

    SimCostRebuildIndexJob::enqueue(Args).await.expect("enqueue");
    sim.run_to_idle().await;
    assert_eq!(DEFERRABLE_RUNS.load(Ordering::SeqCst), 1);
}
