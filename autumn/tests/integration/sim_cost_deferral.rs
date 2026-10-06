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
static CRON_TICKS: AtomicUsize = AtomicUsize::new(0);
static CANCELED_RUNS: AtomicUsize = AtomicUsize::new(0);

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

#[scheduled(cron = "0 * * * * *", name = "sim_cost_cron_rollup", deferrable)]
async fn sim_cost_cron_rollup(_state: AppState) -> AutumnResult<()> {
    CRON_TICKS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[job(name = "sim_cost_cancel_me", deferrable)]
async fn sim_cost_cancel_me(_state: AppState, _args: Args) -> AutumnResult<()> {
    CANCELED_RUNS.fetch_add(1, Ordering::SeqCst);
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
        SimCostRebuildIndexJob::enqueue(Args)
            .await
            .expect("enqueue");
    }
    SimCostSendReceiptJob::enqueue(Args).await.expect("enqueue");
    sim.run_to_idle().await;

    assert_eq!(
        URGENT_RUNS.load(Ordering::SeqCst),
        1,
        "urgent work runs now"
    );
    assert_eq!(
        DEFERRABLE_RUNS.load(Ordering::SeqCst),
        0,
        "deferrable work waits"
    );

    // Ten minutes of high signal: nothing deferrable runs. Requests do.
    for _ in 0..10 {
        advance_minutes(&sim, 1).await;
        sim.client().get("/ping").send().await.assert_ok();
    }
    assert_eq!(
        DEFERRABLE_RUNS.load(Ordering::SeqCst),
        0,
        "no run in the window"
    );
    assert_eq!(
        signal.snapshot().deferrals,
        3,
        "each job counts one time, however long it waits"
    );

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
    assert_eq!(
        DEFERRABLE_TICKS.load(Ordering::SeqCst),
        3,
        "the tick resumes"
    );

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

    SimCostRebuildIndexJob::enqueue(Args)
        .await
        .expect("enqueue");
    sim.run_to_idle().await;
    assert_eq!(DEFERRABLE_RUNS.load(Ordering::SeqCst), 1);
}

#[sim_test]
async fn sim_deferrable_cron_ticks_fold_into_one_run(mut sim: Sim) {
    CRON_TICKS.store(0, Ordering::SeqCst);

    // The sim clock starts on a whole minute, so the cron ticks fall on each
    // minute step below.
    sim.build(
        TestApp::new()
            .config(config())
            .tasks(tasks![sim_cost_cron_rollup]),
    );
    let signal = signal(&sim);

    advance_minutes(&sim, 2).await;
    assert_eq!(CRON_TICKS.load(Ordering::SeqCst), 2);

    // Five minutes of high signal: the 3:00 tick waits, the later ticks fold
    // into it.
    signal.set(THRESHOLD + 1.0);
    advance_minutes(&sim, 5).await;
    assert_eq!(
        CRON_TICKS.load(Ordering::SeqCst),
        2,
        "no tick runs in the window"
    );
    assert_eq!(signal.snapshot().deferrals, 1, "one tick waits");

    // The signal falls: the waiting tick runs one time at the next recheck.
    signal.set(THRESHOLD - 1.0);
    sim.advance(Duration::from_secs(RECHECK_SECS)).await;
    sim.run_to_idle().await;
    assert_eq!(
        CRON_TICKS.load(Ordering::SeqCst),
        3,
        "the folded ticks run one time"
    );

    advance_minutes(&sim, 2).await;
    assert_eq!(
        CRON_TICKS.load(Ordering::SeqCst),
        5,
        "the task ticks as before"
    );
}

#[sim_test]
async fn sim_an_operator_can_cancel_a_deferred_job(mut sim: Sim) {
    use autumn_web::job::{JobAdminQuery, job_admin_backend};

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    CANCELED_RUNS.store(0, Ordering::SeqCst);

    let mut config = config();
    config.actuator.sensitive = true;
    sim.build(
        TestApp::new()
            .config(config)
            .jobs(jobs![sim_cost_cancel_me]),
    );
    let signal = signal(&sim);
    signal.set(THRESHOLD + 1.0);

    SimCostCancelMeJob::enqueue(Args).await.expect("enqueue");
    sim.run_to_idle().await;

    let backend = job_admin_backend(sim.client().state()).expect("job admin backend");
    let snapshot = backend
        .snapshot(JobAdminQuery::default())
        .await
        .expect("snapshot");
    let id = snapshot
        .enqueued
        .records
        .iter()
        .find(|r| r.name == "sim_cost_cancel_me")
        .map(|r| r.id.clone())
        .expect("the deferred job is still enqueued");
    backend.cancel(&id).await.expect("cancel the deferred job");

    // The waiter sees the cancel at its next check and settles the job,
    // while the signal is still high.
    sim.advance(Duration::from_secs(RECHECK_SECS)).await;
    sim.run_to_idle().await;
    let jobs: serde_json::Value = sim.client().get("/actuator/jobs").send().await.json();
    assert_eq!(
        jobs["jobs"]["sim_cost_cancel_me"]["queued"], 0,
        "the cancel settles in the window: {jobs}"
    );

    // The window closes. The canceled job does not run.
    signal.set(THRESHOLD - 1.0);
    advance_minutes(&sim, 2).await;
    assert_eq!(
        CANCELED_RUNS.load(Ordering::SeqCst),
        0,
        "a canceled job does not run"
    );

    let snapshot = backend
        .snapshot(JobAdminQuery::default())
        .await
        .expect("snapshot");
    let listed = [
        &snapshot.enqueued,
        &snapshot.running,
        &snapshot.completed,
        &snapshot.failed,
    ]
    .iter()
    .flat_map(|page| page.records.iter())
    .any(|r| r.id == id);
    assert!(
        !listed,
        "the canceled job left the queue and did not run: {snapshot:?}"
    );
}

// ── Shift accounting (slice 2) ──────────────────────────────────────

static SHIFT_RUNS: AtomicUsize = AtomicUsize::new(0);

#[job(name = "sim_cost_shift_batch", deferrable)]
async fn sim_cost_shift_batch(_state: AppState, _args: Args) -> AutumnResult<()> {
    let mut acc = 0_u64;
    for i in 0..50_000_u64 {
        acc = std::hint::black_box(acc.wrapping_mul(31).wrapping_add(i));
    }
    std::hint::black_box(acc);
    SHIFT_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[scheduled(every = "1m", name = "sim_cost_shift_tick", deferrable)]
async fn sim_cost_shift_tick(_state: AppState) -> AutumnResult<()> {
    Ok(())
}

/// The success metric of issue #1720: during a synthetic high window, at
/// least 90% of the deferrable CPU moves out of the window. The meter shows
/// this: every deferred run is `shifted`, and no run is `in_window`. Requests in the
/// window are metered as usual.
#[sim_test]
async fn sim_deferral_shifts_deferrable_cpu_out_of_the_window(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    SHIFT_RUNS.store(0, Ordering::SeqCst);

    let mut config = config();
    config.cost.enabled = true;
    config.actuator.sensitive = true;
    sim.build(
        TestApp::new()
            .config(config)
            .routes(routes![ping])
            .jobs(jobs![sim_cost_shift_batch, sim_cost_send_receipt])
            .tasks(tasks![sim_cost_shift_tick]),
    );
    let signal = signal(&sim);

    signal.set(THRESHOLD + 100.0);
    for _ in 0..4 {
        SimCostShiftBatchJob::enqueue(Args).await.expect("enqueue");
    }
    SimCostSendReceiptJob::enqueue(Args).await.expect("enqueue");
    for _ in 0..10 {
        advance_minutes(&sim, 1).await;
        sim.client().get("/ping").send().await.assert_ok();
    }
    assert_eq!(SHIFT_RUNS.load(Ordering::SeqCst), 0, "no run in the window");

    signal.set(THRESHOLD - 100.0);
    sim.advance(Duration::from_secs(RECHECK_SECS)).await;
    sim.run_to_idle().await;
    assert_eq!(
        SHIFT_RUNS.load(Ordering::SeqCst),
        4,
        "all deferred jobs run"
    );

    let body: serde_json::Value = sim.client().get("/actuator/cost").send().await.json();
    let jobs = &body["jobs"];
    assert_eq!(jobs["shift"]["shifted_runs"], 4, "{body}");
    assert_eq!(jobs["shift"]["in_window_runs"], 0, "{body}");
    let ratio = jobs["shift"]["ratio"].as_f64().expect("a ratio");
    assert!(ratio >= 0.9, "shifted ratio {ratio} < 0.9: {body}");
    assert_eq!(
        jobs["total"]["runs"], 5,
        "the urgent job is metered too: {body}"
    );
    assert_eq!(body["tasks"]["shift"]["in_window_runs"], 0, "{body}");
    assert!(
        body["tasks"]["shift"]["shifted_runs"].as_u64() >= Some(1),
        "the waiting tick is shifted: {body}"
    );
    assert!(
        body["total"]["requests"].as_u64() >= Some(10),
        "requests in the window are served and metered: {body}"
    );
}
