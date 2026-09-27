//! Sim-testing `Sim::strict_wall_clock` **real-time leak guard** (issue #1797,
//! follow-on to W2's `advance` / `run_to_idle` and the `advance_to` DST work).
//!
//! The guard catches the observable consequence of the worst off-seam pattern:
//! **real wall-clock time leaking into a paused sim** — a real
//! `std::thread::sleep` / blocking call that escapes tokio's virtual timer. It
//! is deliberately *not* off-seam-read detection (a free-function `Utc::now()` /
//! `Instant::now()` has no runtime interception point in safe Rust; catching
//! those reads is the Phase-2 deny-lint's job). These tests pin the three
//! behaviours that matter: a clean virtual run passes even when it jumps a full
//! day of virtual time, a genuine real-time leak trips the guard with an
//! actionable panic, and the guard is off unless explicitly enabled.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;
use chrono::{TimeZone, Utc};
use futures::FutureExt as _;
use std::panic::AssertUnwindSafe;

/// Reads the injected (virtual) wall clock so the clean-run test can prove the
/// clock advanced a full day in virtual time.
#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

/// A clean virtual run with the guard **on** does not trip it: advancing a full
/// day of virtual time and draining costs microseconds of *real* time, far under
/// the budget — even though virtual time jumped 24 hours.
#[sim_test]
async fn strict_wall_clock_passes_on_clean_virtual_run(mut sim: Sim) {
    sim.strict_wall_clock();
    sim.build(TestApp::new().routes(routes![now_route]));

    let wall_start = Instant::now();

    // Jump a full day of virtual time, then drain — no real sleeping anywhere.
    sim.advance(Duration::from_secs(24 * 3600)).await;
    sim.run_to_idle().await;
    let wall_elapsed = wall_start.elapsed();

    // The injected clock advanced exactly 24h (epoch + 1 day)...
    let after = sim.client().get("/now").send().await;
    after.assert_ok();
    assert_eq!(
        after.text(),
        Utc.with_ymd_and_hms(2020, 1, 2, 0, 0, 0)
            .unwrap()
            .to_rfc3339(),
        "the virtual clock must have advanced a full day"
    );

    // ...in essentially zero real time, so the leak guard never trips.
    // Respect AUTUMN_SIM_STRICT_WALL_CLOCK_BUDGET_MS when set, defaulting to 2s to allow
    // for scheduling jitter on slow/contended CI runners.
    let max_allowed = std::env::var("AUTUMN_SIM_STRICT_WALL_CLOCK_BUDGET_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(2), Duration::from_millis);

    assert!(
        wall_elapsed < max_allowed,
        "a clean virtual advance must stay well under the leak-guard budget, but {wall_elapsed:?} elapsed (max allowed: {max_allowed:?})"
    );
}

/// With the guard **on** and a small budget, a real `std::thread::sleep` that
/// escapes the virtual timer (running inside a drained task) trips the guard:
/// `run_to_idle` panics, and the message names the elapsed, the budget, and the
/// `AUTUMN_SIM_STRICT_WALL_CLOCK_BUDGET_MS` override.
///
/// Written as a plain paused-runtime test (not `#[sim_test]`) so the panic can be
/// caught and its message asserted, rather than failing the test.
#[tokio::test(start_paused = true)]
async fn strict_wall_clock_fires_on_real_leak() {
    let mut sim = Sim::from_seed(0);
    // A tiny budget: the real 120ms sleep below is well over it, while ordinary
    // scheduling jitter is well under it.
    sim.strict_wall_clock_budget(Duration::from_millis(10));

    // A real thread sleep escapes tokio's paused virtual timer, burning genuine
    // wall-clock time when the task is polled during the drain.
    tokio::spawn(async {
        std::thread::sleep(Duration::from_millis(120));
    });

    let outcome = AssertUnwindSafe(sim.run_to_idle()).catch_unwind().await;
    let payload = outcome.expect_err("run_to_idle must panic when real time leaks into the sim");

    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("");

    assert!(
        message.contains("strict_wall_clock"),
        "panic must name the guard: {message}"
    );
    assert!(
        message.contains("budget"),
        "panic must mention the budget: {message}"
    );
    assert!(
        message.contains("120") || message.contains("ms"),
        "panic must report the real elapsed time: {message}"
    );
    assert!(
        message.contains("AUTUMN_SIM_STRICT_WALL_CLOCK_BUDGET_MS"),
        "panic must name the override env var: {message}"
    );
}

/// The guard is **off by default**: without a `strict_wall_clock*` call the same
/// real `std::thread::sleep` inside the drained region does not panic — the drain
/// completes normally (proving the guard is opt-in, not always-on).
#[sim_test]
async fn strict_wall_clock_off_by_default_does_not_panic(sim: Sim) {
    let ran = Arc::new(AtomicBool::new(false));
    let ran_task = Arc::clone(&ran);

    // Same real leak as the firing test — but with the guard never enabled, it is
    // tolerated (the drain just burns the real time and returns).
    tokio::spawn(async move {
        std::thread::sleep(Duration::from_millis(120));
        ran_task.store(true, Ordering::SeqCst);
    });

    // No panic here despite the real sleep, because the guard is off.
    sim.run_to_idle().await;

    assert!(
        ran.load(Ordering::SeqCst),
        "the real-sleep task should have been drained without tripping any guard"
    );
}
