//! Sim-testing, the **monotonic gap** (issue #1797, RFC Phase 2): elapsed-time
//! measurement inside the framework must be read through the injected
//! [`ClockSource`] seam, not from a raw `std::time::Instant`.
//!
//! `ClockSource` models only `DateTime<Utc>`, so every framework site that
//! measured a *duration* reached for `Instant::now()` directly. `Instant` is
//! wall-independent and — crucially — **tokio's `start_paused` runtime does not
//! virtualize it** (only `tokio::time::Instant` moves with the paused timer), so
//! under a `#[sim_test]` those measurements silently came from the real machine
//! clock. A 24-hour virtual advance registered as microseconds of elapsed time.
//!
//! [`crate::time::MonotonicInstant`] closes that gap: `ClockSource::monotonic`
//! is the monotonic twin of `ClockSource::now`, real in production
//! (`SystemClock` reads a process-origin `Instant`, so a wall-clock/NTP jump can
//! never make an elapsed duration negative) and virtual under the sim
//! (`TickingClock` derives it from the virtual instant, so `Sim::advance` moves
//! it).
//!
//! The observable proof used here is [`AppState::uptime`], which is
//! `started_at.elapsed()` — the single most direct read of the framework's
//! monotonic clock reachable from public API.
//!
//! Before the migration this test fails: `uptime()` measured real elapsed time,
//! which is microseconds in a paused sim no matter how far the virtual clock is
//! advanced.

use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;

/// 24 hours — the virtual advance under test. Chosen to be enormous relative to
/// any real elapsed time the test could accrue, so the assertion cannot pass by
/// accident on a slow runner.
const TWENTY_FOUR_HOURS: Duration = Duration::from_secs(24 * 3600);

/// Reads the injected (virtual) wall clock, so the test can prove it advanced.
#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

#[sim_test]
async fn uptime_is_measured_on_the_virtual_clock(mut sim: Sim) {
    assert_eq!(sim.seed, 0);

    sim.build(TestApp::new().routes(routes![now_route]));

    // Uptime starts at (virtually) zero: the app was mounted at the sim epoch.
    let at_mount = sim.client().state().uptime();
    assert!(
        at_mount < Duration::from_secs(1),
        "uptime should start near zero at mount, got {at_mount:?}"
    );

    // The injected wall clock starts pinned at the fixed sim epoch.
    let before = sim.client().get("/now").send().await;
    before.assert_ok();
    assert_eq!(before.text(), "2020-01-01T00:00:00+00:00");

    // Jump virtual time forward 24h with no real sleeping at all.
    sim.advance(TWENTY_FOUR_HOURS).await;

    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), "2020-01-02T00:00:00+00:00");

    // The monotonic seam moved in lockstep with the wall clock: uptime is the
    // full virtual day. Before the migration this read a raw `Instant::elapsed`
    // and reported microseconds.
    let uptime = sim.client().state().uptime();
    assert!(
        uptime >= TWENTY_FOUR_HOURS,
        "uptime must advance with virtual time; expected >= 24h, got {uptime:?}"
    );
    // Bounded from above too, so the assertion is a real equality claim rather
    // than "some big number": the only thing that moved the clock was the
    // 24h advance.
    assert!(
        uptime < TWENTY_FOUR_HOURS + Duration::from_secs(60),
        "uptime must track the virtual advance exactly, got {uptime:?}"
    );

    // The human-readable formatter reads the same seam.
    assert_eq!(sim.client().state().uptime_display(), "24h 0m");
}

#[sim_test]
async fn uptime_is_reproducible_across_two_same_seed_runs(mut sim: Sim) {
    // Determinism, not just virtualization: the same seed and the same advance
    // schedule must yield a byte-identical uptime. A real-`Instant` uptime
    // varies run to run and can never satisfy this.
    sim.build(TestApp::new().routes(routes![now_route]));
    sim.advance(Duration::from_secs(90)).await;
    let first = sim.client().state().uptime();

    let mut second_sim = Sim::from_seed(sim.seed);
    second_sim.build(TestApp::new().routes(routes![now_route]));
    second_sim.advance(Duration::from_secs(90)).await;
    let second = second_sim.client().state().uptime();

    assert_eq!(
        first, second,
        "the same seed and advance schedule must produce an identical uptime"
    );
    assert_eq!(first, Duration::from_secs(90));
}
