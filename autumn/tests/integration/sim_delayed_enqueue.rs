//! Sim-testing, the **direct `Utc::now()` gap** (issue #1797, RFC Phase 2): a
//! delayed enqueue must resolve its absolute due instant from the injected
//! [`ClockSource`], not from a direct `chrono::Utc::now()` call.
//!
//! `job::enqueue_in(name, payload, delay)` converts a relative delay into an
//! absolute due time (`delay_to_when`). That conversion read `Utc::now()`
//! directly, off the seam — while the job runtime's own due-at filter reads the
//! *injected* clock. Under a `#[sim_test]` the two disagree by the distance
//! between the fixed sim epoch (`2020-01-01T00:00:00Z`) and real wall-clock
//! time: a job asked to run one virtual second from now is stamped due years in
//! the future, so **no amount of virtual advancing ever makes it due** and the
//! job silently never runs.
//!
//! That is the exact class of bug the sim harness exists to catch, and it is
//! invisible to a conventional integration test (which runs on the real clock,
//! where the two agree).
//!
//! Before the migration this test fails: the job never runs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::job::{JobAdminQuery, job_admin_backend};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use serde::{Deserialize, Serialize};

/// The relative delay under test, in virtual time.
const DELAY: Duration = Duration::from_secs(60);

/// Empty payload for the probe job.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeArgs;

/// Times the delayed probe has run this test. The global job-runtime lock
/// serializes access across the consolidated binary; the test resets it to 0.
static DELAYED_RUNS: AtomicUsize = AtomicUsize::new(0);

#[job(name = "sim_delayed_probe")]
async fn sim_delayed_probe(_state: AppState, _args: ProbeArgs) -> AutumnResult<()> {
    DELAYED_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Registers the probe job on the mounted app.
struct DelayedJobPlugin;

impl Plugin for DelayedJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![sim_delayed_probe])
    }
}

#[sim_test]
async fn delayed_enqueue_becomes_due_under_virtual_time(mut sim: Sim) {
    // This job path uses the process-global job client, so serialize on the
    // shared runtime lock and start from a clean client + counter.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    DELAYED_RUNS.store(0, Ordering::SeqCst);

    sim.build(TestApp::new().plugin(DelayedJobPlugin));

    // Ask for the job one virtual minute from now. `enqueue_in` must stamp the
    // due instant from the *injected* clock (sim epoch + 60s), not real time.
    SimDelayedProbeJob::enqueue_in(ProbeArgs, DELAY)
        .await
        .expect("delayed enqueue should succeed");

    // The decisive assertion: the RECORDED absolute due instant is the sim
    // epoch plus the delay. Asserting only "the job eventually ran" would also
    // pass if `delay_to_when` and the runtime's due filter both read real time
    // and merely agreed with each other — this pins the due instant to the
    // injected clock at a single site.
    let backend = job_admin_backend(sim.client().state()).expect("job admin backend is installed");
    let snapshot = backend
        .snapshot(JobAdminQuery::default())
        .await
        .expect("admin snapshot");
    let scheduled = snapshot
        .scheduled
        .records
        .iter()
        .find(|r| r.name == "sim_delayed_probe")
        .expect("the delayed job should be recorded as scheduled, not runnable");
    assert_eq!(
        scheduled.scheduled_for.as_deref(),
        Some("2020-01-01T00:01:00Z"),
        "the due instant must be the sim epoch + 60s, not real wall time + 60s"
    );

    // Not yet due: draining now must not run it.
    sim.run_to_idle().await;
    assert_eq!(
        DELAYED_RUNS.load(Ordering::SeqCst),
        0,
        "the job must not run before its virtual due instant"
    );

    // Cross the due instant in virtual time, with zero real sleeping.
    sim.advance(DELAY + Duration::from_secs(1)).await;
    sim.run_to_idle().await;

    assert_eq!(
        DELAYED_RUNS.load(Ordering::SeqCst),
        1,
        "the delayed job must become due once virtual time passes the delay; \
         a due instant computed from real `Utc::now()` is years ahead of the sim \
         epoch and never comes due"
    );

    // Tear the process-global job client down *while still holding the guard*,
    // exactly as every other job-backed sim test in this suite does. Leaving it
    // to `Sim`'s `Drop` would run it AFTER `_guard` releases (locals drop before
    // parameters), so a test that had already taken the lock and installed its
    // own client would have it cleared out from under it.
    job::clear_global_job_client();
}
