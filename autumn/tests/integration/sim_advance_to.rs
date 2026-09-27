//! Sim-testing DST/timezone-aware virtual-clock control (issue #1797): the
//! follow-on to W2's `advance` / `run_to_idle`, exercising
//! [`Sim::advance_to`](autumn_web::sim::Sim::advance_to) — advancing the sim
//! clock **to** a zoned target instant, stepping through a DST spring-forward
//! transition correctly, with any timer due inside the crossed wall-clock window
//! firing in virtual time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::America::New_York;

/// Reads the injected (virtual) wall clock so the test can prove where the sim
/// clock has advanced to.
#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

/// `advance_to` a plain future UTC target moves the sim clock to *exactly* that
/// instant (and never past it).
#[sim_test]
async fn advance_to_utc_target_lands_exactly(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![now_route]));

    // The injected clock starts pinned at the fixed sim epoch.
    let before = sim.client().get("/now").send().await;
    before.assert_ok();
    assert_eq!(before.text(), "2020-01-01T00:00:00+00:00");

    // Advance to a specific UTC instant a bit over two months out.
    let target = Utc.with_ymd_and_hms(2020, 3, 8, 12, 0, 0).unwrap();
    sim.advance_to(&target).await;

    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), target.to_rfc3339());
}

/// `advance_to` the *current* instant is a no-op (time never moves backward and
/// never spuriously advances).
#[sim_test]
async fn advance_to_current_instant_is_noop(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![now_route]));

    let epoch = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    sim.advance_to(&epoch).await;

    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), "2020-01-01T00:00:00+00:00");
}

/// `advance_to` across a **DST spring-forward** boundary (`America/New_York`,
/// 2020-03-08 02:00 EST → 03:00 EDT) resolves the zoned wall time to the correct
/// UTC instant and advances the sim clock by the correct real delta.
#[sim_test]
async fn advance_to_across_spring_forward_resolves_utc(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![now_route]));

    // 03:30 local on the spring-forward day is a *valid* wall time in EDT
    // (offset -04:00), so it resolves to 07:30 UTC — not the 08:30 UTC it would
    // be if the day were treated as a normal -05:00 EST day.
    let target_local = New_York.with_ymd_and_hms(2020, 3, 8, 3, 30, 0).unwrap();
    let expected_utc = Utc.with_ymd_and_hms(2020, 3, 8, 7, 30, 0).unwrap();
    assert_eq!(
        target_local.with_timezone(&Utc),
        expected_utc,
        "DST-aware zone must resolve 03:30 EDT to 07:30 UTC"
    );

    sim.advance_to(&target_local).await;

    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), expected_utc.to_rfc3339());
}

/// The DST heart of the feature: a `tokio::time::sleep`-backed effect whose
/// deadline falls **inside** the crossed spring-forward window fires during the
/// `advance_to`, in virtual time, with essentially zero wall-clock sleep — and
/// the sim clock still lands exactly on the zoned target.
#[sim_test]
async fn timer_inside_crossed_dst_window_fires_in_virtual_time(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![now_route]));

    let wall_start = Instant::now();

    // Target: 04:00 EDT on the spring-forward day = 08:00 UTC. The epoch is
    // 2020-01-01T00:00:00Z, so the crossed window is ~67 days long and includes
    // the 02:00→03:00 local jump.
    let target_local = New_York.with_ymd_and_hms(2020, 3, 8, 4, 0, 0).unwrap();
    let expected_utc = Utc.with_ymd_and_hms(2020, 3, 8, 8, 0, 0).unwrap();
    assert_eq!(target_local.with_timezone(&Utc), expected_utc);

    // Arm a timer to fire ~1 hour before the target (well inside the crossed
    // window, and after the DST transition instant at 07:00 UTC).
    let epoch = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    let until_fire = (expected_utc - chrono::Duration::hours(1) - epoch)
        .to_std()
        .unwrap();

    let fired = Arc::new(AtomicBool::new(false));
    let fired_task = Arc::clone(&fired);
    tokio::spawn(async move {
        tokio::time::sleep(until_fire).await;
        fired_task.store(true, Ordering::SeqCst);
    });

    // Let the spawned task reach its `sleep().await` so its timer is registered
    // on the paused wheel before we advance.
    sim.run_to_idle().await;
    assert!(
        !fired.load(Ordering::SeqCst),
        "the in-window timer must not fire before time advances"
    );

    // Cross the DST boundary by advancing to the zoned target.
    sim.advance_to(&target_local).await;
    sim.run_to_idle().await;

    assert!(
        fired.load(Ordering::SeqCst),
        "a timer due inside the crossed DST window should have fired in virtual time"
    );

    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(
        after.text(),
        expected_utc.to_rfc3339(),
        "the sim clock must land exactly on the zoned target instant"
    );

    // The multi-month virtual advance took essentially no real time.
    let wall_elapsed = wall_start.elapsed();
    assert!(
        wall_elapsed < Duration::from_secs(1),
        "expected near-zero wall time for a virtual DST-crossing advance, but {wall_elapsed:?} elapsed"
    );
}

/// The naive-local convenience form resolves a spring-forward **gap** (a wall
/// time that never exists) deterministically — no panic, no `.unwrap()` on a
/// `LocalResult::None` — and still lands the clock in the future.
#[sim_test]
async fn advance_to_local_gap_time_resolves_deterministically(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![now_route]));

    // 02:30 on 2020-03-08 does not exist in America/New_York (the clock jumps
    // 02:00 EST → 03:00 EDT). The nonexistent wall time is carried forward across
    // the gap deterministically, landing at 07:30 UTC (i.e. 03:30 EDT — shifted
    // past the transition by the gap length) rather than erroring.
    let gap_local = NaiveDate::from_ymd_opt(2020, 3, 8)
        .unwrap()
        .and_hms_opt(2, 30, 0)
        .unwrap();
    let expected_utc: DateTime<Utc> = Utc.with_ymd_and_hms(2020, 3, 8, 7, 30, 0).unwrap();

    sim.advance_to_local(gap_local, &New_York).await;

    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(after.text(), expected_utc.to_rfc3339());
}
