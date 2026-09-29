//! Sim Phase 2 (issue #2967): `run_to_idle` reports a drain that does not
//! settle.
//!
//! A job that enqueues itself again keeps the drain busy forever. Before this,
//! `run_to_idle` stopped after its step bound and gave no signal. Now it panics
//! with the seed, as the liveness watchdog does.

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::job;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Link;

static LINKS: AtomicUsize = AtomicUsize::new(0);

/// Enqueues itself again on every run, so the queue never empties.
#[job(name = "sim_stall_forever")]
async fn sim_stall_forever(_state: AppState, _args: Link) -> AutumnResult<()> {
    LINKS.fetch_add(1, Ordering::SeqCst);
    SimStallForeverJob::enqueue(Link).await?;
    Ok(())
}

/// Runs once and stops.
#[job(name = "sim_stall_once")]
async fn sim_stall_once(_state: AppState, _args: Link) -> AutumnResult<()> {
    LINKS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[sim_test]
async fn sim_drain_stall_is_reported_with_the_seed(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    LINKS.store(0, Ordering::SeqCst);

    sim.build(TestApp::new().jobs(jobs![sim_stall_forever]));
    SimStallForeverJob::enqueue(Link).await.unwrap();

    let stall = sim
        .try_run_to_idle()
        .await
        .expect_err("a job that re-enqueues itself never settles");
    assert_eq!(stall.seed, sim.seed);
    assert!(
        stall
            .to_string()
            .contains(&format!("seed=0x{:x}", sim.seed)),
        "{stall}"
    );
    assert!(LINKS.load(Ordering::SeqCst) > 1, "the chain did run");
    job::clear_global_job_client();
}

#[sim_test]
async fn sim_drain_stall_settled_work_is_ok(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    LINKS.store(0, Ordering::SeqCst);

    sim.build(TestApp::new().jobs(jobs![sim_stall_once]));
    SimStallOnceJob::enqueue(Link).await.unwrap();

    sim.try_run_to_idle()
        .await
        .expect("one job settles well inside the bound");
    assert_eq!(LINKS.load(Ordering::SeqCst), 1);
    job::clear_global_job_client();
}

#[sim_test]
async fn sim_drain_stall_panics_from_run_to_idle(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    sim.build(TestApp::new().jobs(jobs![sim_stall_forever]));
    SimStallForeverJob::enqueue(Link).await.unwrap();

    // `run_to_idle` is `try_run_to_idle` plus a panic. Catch it to check the
    // message, so the lock guard above still drops cleanly.
    let seed = sim.seed;
    let panic = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(sim.run_to_idle()))
        .await
        .expect_err("a stalled drain panics");
    let message = panic
        .downcast_ref::<String>()
        .expect("the stall panics with a formatted message");
    assert!(message.contains("sim drain stall"), "{message}");
    assert!(message.contains(&format!("seed=0x{seed:x}")), "{message}");
    job::clear_global_job_client();
}
