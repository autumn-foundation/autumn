//! Sim-testing W2 Definition of Done (issue #1797): a `#[job]` whose retry
//! backs off **24 hours** fires in **virtual time** with essentially zero
//! wall-clock sleep, driven entirely through the [`Sim`] handle's clock + drain
//! wiring (`build` / `advance` / `run_to_idle`).
//!
//! The job fails on its first attempt, which arms a 24h retry-backoff timer on
//! tokio's paused clock. Advancing the sim clock 24h fires that timer (and moves
//! the injected wall clock in lockstep), then `run_to_idle` drains the retry —
//! all in microseconds of real time.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;
use serde::{Deserialize, Serialize};

/// 24 hours — the retry backoff under test.
const TWENTY_FOUR_HOURS: Duration = Duration::from_secs(24 * 3600);

/// Empty payload for the probe job.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeArgs;

/// Times the probe job's handler has run this test. The global job-runtime lock
/// serializes access across the consolidated binary; the test resets it to 0.
static BACKOFF_RUNS: AtomicUsize = AtomicUsize::new(0);

// `backoff_ms = 86_400_000` (24h) with `max_attempts = 2`: the first attempt
// fails, arming a 24h backoff before the single retry, which succeeds. The
// retry delay is `backoff_ms * 2^(attempt-1)` = 24h for the first retry, slept
// on tokio's paused timer — so only `Sim::advance` can make it come due.
#[job(name = "sim_backoff_probe", max_attempts = 2, backoff_ms = 86_400_000)]
async fn sim_backoff_probe(_state: AppState, _args: ProbeArgs) -> AutumnResult<()> {
    let prior = BACKOFF_RUNS.fetch_add(1, Ordering::SeqCst);
    if prior == 0 {
        Err(AutumnError::internal_server_error(std::io::Error::other(
            "first attempt fails to force the 24h retry backoff",
        )))
    } else {
        Ok(())
    }
}

/// Registers the probe job on the mounted app.
struct BackoffJobPlugin;

impl Plugin for BackoffJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![sim_backoff_probe])
    }
}

/// Reads the injected (virtual) wall clock, so the test can prove it advanced.
#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

#[sim_test]
async fn backoff_job_fires_in_virtual_time_with_zero_wall_sleep(mut sim: Sim) {
    // This job path uses the process-global job client, so serialize on the
    // shared runtime lock and start from a clean client + counter, exactly like
    // the other global-runtime integration tests.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    BACKOFF_RUNS.store(0, Ordering::SeqCst);

    // Mount the app on the paused runtime with the sim's virtual clock (starting
    // at the fixed sim epoch) and the in-process job runtime.
    sim.build(
        TestApp::new()
            .plugin(BackoffJobPlugin)
            .routes(routes![now_route]),
    );

    // The injected clock starts pinned at the fixed sim epoch.
    let before = sim.client().get("/now").send().await;
    before.assert_ok();
    assert_eq!(before.text(), "2020-01-01T00:00:00+00:00");

    // Enqueue the job and drain: attempt 1 runs, fails, and arms a 24h retry
    // backoff timer on the paused tokio clock.
    SimBackoffProbeJob::enqueue(ProbeArgs).await.unwrap();
    sim.run_to_idle().await;
    assert_eq!(
        BACKOFF_RUNS.load(Ordering::SeqCst),
        1,
        "attempt 1 should have run once and failed"
    );

    // Jump virtual time forward 24h. This fires the retry backoff sleep AND
    // steps the injected wall clock the same 24h — with no real sleeping.
    //
    // The wall-clock budget starts *here*, not at the top of the test, and stops
    // as soon as the retry has drained. That window — the 24h advance and the
    // drain it fires — is the one the assertion below is actually about, and it
    // is where the sibling sim tests start their clocks too
    // (`sim_strict_wall_clock`, `sim_advance_to`; this test was the outlier).
    // Started any earlier it also covers `sim.build`, which mounts an app and
    // pays the one-time warm-up for every lazy static behind it — cheap on a
    // quiet machine, but enough to blow a 1s budget under `llvm-cov`
    // instrumentation, which is how the coverage job caught it.
    let wall_start = Instant::now();
    sim.advance(TWENTY_FOUR_HOURS).await;
    sim.run_to_idle().await;
    let wall_elapsed = wall_start.elapsed();

    // The retry fired and succeeded, all in virtual time.
    assert_eq!(
        BACKOFF_RUNS.load(Ordering::SeqCst),
        2,
        "the 24h-backoff retry should have fired and succeeded in virtual time"
    );

    // The injected clock advanced exactly 24h (epoch + 1 day)...
    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), "2020-01-02T00:00:00+00:00");

    // ...while essentially no wall-clock time elapsed: the 24h backoff was slept
    // in virtual time, never on the real clock. (`wall_elapsed` was captured
    // above, the moment the retry drained.)
    assert!(
        wall_elapsed < Duration::from_secs(1),
        "expected near-zero wall time for a 24h virtual backoff, but {wall_elapsed:?} elapsed"
    );

    job::clear_global_job_client();
}
