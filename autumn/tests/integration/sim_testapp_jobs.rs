//! Sim-testing (issue #1797): `TestApp::jobs` registers `#[job]`s directly.
//!
//! The simulation-testing guide mounts an app with
//! `TestApp::new().routes(..).jobs(jobs![..])`. Before this, jobs reached a
//! `TestApp` only through a plugin. This test mounts a job the documented way
//! and drains it with [`Sim::run_to_idle`].

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::job;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use serde::{Deserialize, Serialize};

static RECEIPTS_SENT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiptArgs;

#[job(name = "sim_testapp_send_receipt")]
async fn sim_testapp_send_receipt(_state: AppState, _args: ReceiptArgs) -> AutumnResult<()> {
    RECEIPTS_SENT.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[sim_test]
async fn jobs_registered_on_the_test_app_run_under_the_sim(mut sim: Sim) {
    // The job runtime uses the process-global job client, so serialize with
    // the other global-runtime tests and start from a clean client.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    sim.build(TestApp::new().jobs(jobs![sim_testapp_send_receipt]));

    SimTestappSendReceiptJob::enqueue(ReceiptArgs)
        .await
        .expect("enqueue");
    sim.run_to_idle().await;

    assert_eq!(RECEIPTS_SENT.load(Ordering::SeqCst), 1);
}
