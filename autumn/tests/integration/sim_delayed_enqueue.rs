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

/// The queue-depth gauge must judge a delayed job on the same clock that stamped
/// its ready-at mark.
///
/// `record_enqueue_scheduled` stores the due instant the job runtime read from
/// the **injected** clock, but `JobRegistry`'s gauges used to compare those
/// marks against `SystemTime::now()`. Under a sim that mixes two timelines: a
/// job delayed from the 2020 sim epoch looks long-ready against a real 2026
/// host, so `/actuator/jobs` reports a ready job that cannot run and an
/// "oldest waiting" age of roughly six years.
///
/// Regression test for the P2 raised on #2192. It is the observable half of the
/// same invariant `sim_delayed_enqueue` covers for execution: whatever decides a
/// job is ready must read the clock the deadline was written on.
#[sim_test]
async fn queue_gauges_judge_a_delayed_job_on_the_virtual_clock(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    DELAYED_RUNS.store(0, Ordering::SeqCst);

    sim.build(TestApp::new().plugin(DelayedJobPlugin));

    SimDelayedProbeJob::enqueue_in(ProbeArgs, DELAY)
        .await
        .expect("delayed enqueue should succeed");

    // Settle the enqueue (and arm the local delay timer) before reading the
    // gauge, exactly as the sibling test does before advancing.
    sim.run_to_idle().await;
    assert_eq!(
        DELAYED_RUNS.load(Ordering::SeqCst),
        0,
        "the job must not have run before its virtual due instant"
    );

    // Before the due instant the job is *scheduled*, not ready: it contributes
    // no ready depth and no waiting age. Reading the real clock here instead
    // reports depth 1 and an age of years.
    let scheduled = sim.client().state().job_registry().queue_snapshot();
    // An absent entry means "nothing waiting", which satisfies the same claim.
    let (before_depth, before_age) = scheduled
        .get("default")
        .map_or((0, 0), |q| (q.depth, q.oldest_waiting_age_ms));
    assert_eq!(
        before_depth, 0,
        "a job that is not due yet must not count toward ready queue depth; \
         got depth {before_depth} (a real-clock comparison sees the sim-epoch \
         deadline as long past)"
    );
    assert_eq!(
        before_age, 0,
        "a not-yet-due job has no waiting age; got {before_age}ms — roughly the \
         gap between the sim epoch and real time when the timelines are mixed"
    );

    // Cross the deadline in virtual time. Now the gauge should see it as ready
    // for the moment before the worker drains it.
    sim.advance(DELAY + Duration::from_secs(1)).await;
    sim.run_to_idle().await;

    assert_eq!(
        DELAYED_RUNS.load(Ordering::SeqCst),
        1,
        "the job still has to actually run once virtual time passes the delay"
    );

    // Drained: the mark is retired, so depth returns to zero on the same clock.
    let after_depth = sim
        .client()
        .state()
        .job_registry()
        .queue_snapshot()
        .get("default")
        .map_or(0, |q| q.depth);
    assert_eq!(
        after_depth, 0,
        "after the job runs its waiting mark must be retired, not stranded"
    );

    job::clear_global_job_client();
}
