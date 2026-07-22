//! Sim-testing (issue #2111): the job runtime's recorded wall-clock timestamps
//! (`enqueued_at` / `finished_at`) are read through the injected [`ClockSource`]
//! seam, so under the [`Sim`] harness they are the **virtual** clock's time, not
//! real wall time. This is the clock twin of the seeded-entropy job-id work
//! (`sim_deterministic_ids`) and mirrors `sim_clock_drain`'s wiring.
//!
//! The probe jobs succeed immediately; the test reads the recorded timestamps
//! back through the public job-admin snapshot and asserts they equal the sim's
//! pinned virtual epoch (and epoch + an advance), proving the timestamp is
//! virtual rather than wall-clock.

use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::job::{JobAdminQuery, job_admin_backend};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;
use serde::{Deserialize, Serialize};

/// One hour — the virtual advance applied between the two probes.
const ONE_HOUR: Duration = Duration::from_secs(3600);

/// Empty payload for the probe jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeArgs;

// Two immediately-succeeding probes so each recorded lifecycle carries a
// distinct, addressable name in the admin snapshot.
#[job(name = "sim_clock_probe_a")]
async fn sim_clock_probe_a(_state: AppState, _args: ProbeArgs) -> AutumnResult<()> {
    Ok(())
}

#[job(name = "sim_clock_probe_b")]
async fn sim_clock_probe_b(_state: AppState, _args: ProbeArgs) -> AutumnResult<()> {
    Ok(())
}

/// Registers both probe jobs on the mounted app.
struct ClockProbeJobPlugin;

impl Plugin for ClockProbeJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![sim_clock_probe_a, sim_clock_probe_b])
    }
}

/// Reads the injected (virtual) wall clock, so the test can prove it advanced.
#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

/// Returns `(enqueued_at, finished_at)` (RFC3339, seconds, `Z`) for the newest
/// admin record whose job name matches, read through the public job-admin
/// backend that the built-in runtime shares with the injected clock.
async fn recorded_times(state: &AppState, name: &str) -> (String, String) {
    let backend = job_admin_backend(state).expect("job admin backend is installed");
    let snap = backend
        .snapshot(JobAdminQuery::default())
        .await
        .expect("admin snapshot");
    let record = snap
        .enqueued
        .records
        .iter()
        .chain(snap.scheduled.records.iter())
        .chain(snap.running.records.iter())
        .chain(snap.completed.records.iter())
        .chain(snap.failed.records.iter())
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no admin record found for job '{name}'"));
    (
        record.enqueued_at.clone().unwrap_or_default(),
        record.finished_at.clone().unwrap_or_default(),
    )
}

#[sim_test]
async fn recorded_job_timestamps_are_virtual_under_the_sim(mut sim: Sim) {
    // This job path uses the process-global job client, so serialize on the
    // shared runtime lock and start from a clean client, exactly like the other
    // global-runtime integration tests.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Mount the app on the paused runtime with the sim's virtual clock (pinned at
    // the fixed sim epoch) and the in-process job runtime.
    sim.build(
        TestApp::new()
            .plugin(ClockProbeJobPlugin)
            .routes(routes![now_route]),
    );

    // Seed stability, as the other sim tests assert.
    assert_eq!(sim.seed, 0, "the default sim seed is 0");

    // The injected clock starts pinned at the fixed sim epoch.
    let before = sim.client().get("/now").send().await;
    before.assert_ok();
    assert_eq!(before.text(), "2020-01-01T00:00:00+00:00");

    // Enqueue the first probe at the epoch and drain: it runs and succeeds, so
    // both `enqueued_at` and `finished_at` are recorded through the injected
    // clock at the virtual epoch — NOT real wall time.
    SimClockProbeAJob::enqueue(ProbeArgs).await.unwrap();
    sim.run_to_idle().await;

    let (a_enqueued, a_finished) = recorded_times(sim.client().state(), "sim_clock_probe_a").await;
    assert_eq!(
        a_enqueued, "2020-01-01T00:00:00Z",
        "enqueued_at is the virtual epoch, not wall-clock time",
    );
    assert_eq!(
        a_finished, "2020-01-01T00:00:00Z",
        "finished_at is the virtual epoch, not wall-clock time",
    );

    // Advance virtual time one hour, then enqueue + drain the second probe. Its
    // recorded timestamps must be the advanced virtual time (epoch + 1h),
    // proving a later lifecycle write also reads the injected clock.
    sim.advance(ONE_HOUR).await;

    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), "2020-01-01T01:00:00+00:00");

    SimClockProbeBJob::enqueue(ProbeArgs).await.unwrap();
    sim.run_to_idle().await;

    let (b_enqueued, b_finished) = recorded_times(sim.client().state(), "sim_clock_probe_b").await;
    assert_eq!(
        b_enqueued, "2020-01-01T01:00:00Z",
        "the later record's enqueued_at is epoch + the 1h virtual advance",
    );
    assert_eq!(
        b_finished, "2020-01-01T01:00:00Z",
        "the later record's finished_at is epoch + the 1h virtual advance",
    );

    job::clear_global_job_client();
}
