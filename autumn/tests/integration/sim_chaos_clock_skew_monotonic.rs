//! Sim-testing guardrail (issue #1797): [`Chaos::clock_skew`] is a **wall-clock**
//! fault and must not corrupt *elapsed* time.
//!
//! `SkewClock` wraps the sim's virtual clock and reports `now()` offset forward
//! by a seed-derived amount, modelling a machine whose calendar disagrees with
//! reality. A real monotonic clock is immune to exactly that — an NTP step does
//! not change how long something took — so `SkewClock` forwards
//! [`ClockSource::monotonic`] to the wrapped clock **unskewed**.
//!
//! Without that forward, `SkewClock` would inherit the trait's default body,
//! which reads the *real* process-monotonic clock. Every elapsed measurement
//! that is on the seam today — uptime, DB checkout and per-statement timings,
//! scheduled-task durations, the shutdown drain window, local job uniqueness
//! windows — would silently revert to real time on the chaos path, reopening
//! the very gap this work closes. (Per-request latency middleware is not yet on
//! the seam; see CONTRIBUTING.md's ungated remainder.) The current skew is a single constant that would cancel
//! under subtraction anyway; this test is what makes that an invariant rather
//! than a coincidence, so a future drifting or jittering skew cannot quietly
//! break it.

use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::sim::{Chaos, Sim};
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;

/// The skew injected by the chaos config under test — large enough that a
/// leaked skew would be unmistakable in the assertions below.
const SKEW: Duration = Duration::from_secs(6 * 3600);

/// The virtual advance applied mid-test.
const ADVANCE: Duration = Duration::from_secs(24 * 3600);

/// Reads the injected (skewed) wall clock.
#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

#[sim_test]
async fn clock_skew_shifts_wall_time_but_not_elapsed_time(mut sim: Sim) {
    sim.chaos(Chaos::default().clock_skew(SKEW));
    sim.build(TestApp::new().routes(routes![now_route]));

    // Uptime starts at zero even though wall time is skewed: `started_at` and
    // every later reading come from the same unskewed monotonic timeline.
    let at_mount = sim.client().state().uptime();
    assert!(
        at_mount < Duration::from_secs(1),
        "a skewed wall clock must not inflate uptime at mount; got {at_mount:?}"
    );

    // The wall clock IS skewed — proving the fault is actually installed, so a
    // green run below is not vacuous.
    let skewed = sim.client().get("/now").send().await;
    skewed.assert_ok();
    assert_ne!(
        skewed.text(),
        "2020-01-01T00:00:00+00:00",
        "clock_skew should have shifted the reported wall-clock instant"
    );

    sim.advance(ADVANCE).await;

    // Elapsed time tracks the advance exactly, with no skew folded in.
    let uptime = sim.client().state().uptime();
    assert!(
        uptime >= ADVANCE && uptime < ADVANCE + Duration::from_secs(60),
        "elapsed time must follow the virtual advance alone, unaffected by a \
         {SKEW:?} wall-clock skew; expected ~{ADVANCE:?}, got {uptime:?}"
    );
}

#[sim_test]
async fn skewed_and_unskewed_runs_agree_on_elapsed_time(mut sim: Sim) {
    // The sharpest form of the invariant: two sims driven identically, one with
    // clock skew and one without, must report byte-identical elapsed time.
    sim.chaos(Chaos::default().clock_skew(SKEW));
    sim.build(TestApp::new().routes(routes![now_route]));
    sim.advance(ADVANCE).await;
    let with_skew = sim.client().state().uptime();

    let mut clean = Sim::from_seed(sim.seed);
    clean.build(TestApp::new().routes(routes![now_route]));
    clean.advance(ADVANCE).await;
    let without_skew = clean.client().state().uptime();

    assert_eq!(
        with_skew, without_skew,
        "clock skew is a wall-clock fault; it must leave elapsed time untouched"
    );
    assert_eq!(with_skew, ADVANCE);
}
