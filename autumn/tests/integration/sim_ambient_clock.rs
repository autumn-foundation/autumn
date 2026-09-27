//! Sim Phase 2 (issue #2967): the ambient clock.
//!
//! Framework code with no clock handle reads time through
//! `time::ambient_now` and its siblings. They read the running `Sim`'s virtual
//! clock on this thread, and the system clock otherwise.

use std::time::Duration;

use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::time::{
    AmbientClock, ClockSource, ambient_instant, ambient_monotonic, ambient_now, ambient_system_time,
};
use chrono::{TimeZone, Utc};

const HOUR: Duration = Duration::from_secs(3600);

fn sim_epoch() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()
}

#[sim_test]
async fn sim_ambient_clock_reads_virtual_time(sim: Sim) {
    assert_eq!(ambient_now(), sim_epoch());
    assert_eq!(AmbientClock.now(), sim_epoch());
    let mono = ambient_monotonic();
    let instant = ambient_instant();

    sim.advance(HOUR).await;

    assert_eq!(ambient_now(), sim_epoch() + chrono::Duration::hours(1));
    assert_eq!(ambient_monotonic().saturating_duration_since(mono), HOUR);
    assert_eq!(ambient_instant().saturating_duration_since(instant), HOUR);
    let unix = ambient_system_time()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    assert_eq!(unix.as_secs(), 1_577_836_800 + 3600);
}

#[sim_test]
async fn sim_ambient_clock_is_restored_when_the_sim_drops(sim: Sim) {
    let inner = Sim::from_seed(sim.seed.wrapping_add(1));
    inner.advance(HOUR).await;
    assert_eq!(
        ambient_now(),
        sim_epoch() + chrono::Duration::hours(1),
        "the newest sim on the thread wins"
    );
    drop(inner);
    assert_eq!(ambient_now(), sim_epoch(), "the outer sim is back");
}

#[test]
fn sim_ambient_clock_outside_a_sim_is_the_system_clock() {
    let before = Utc::now();
    let now = ambient_now();
    assert!(now >= before && now - before < chrono::Duration::seconds(5));
    let sim = Sim::from_seed(0);
    assert_eq!(ambient_now(), sim_epoch());
    drop(sim);
    assert!(
        ambient_now() >= before,
        "real time again after the sim drops"
    );
}

#[sim_test]
async fn sim_ambient_clock_deadline_follows_tokio_sleeps(_sim: Sim) {
    // No `Sim::advance`: the paused runtime moves itself to each sleep's end.
    // An ambient deadline must see that time, or this loop never ends.
    let deadline = ambient_instant() + Duration::from_secs(5);
    let mut rounds = 0;
    while ambient_instant() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rounds += 1;
        assert!(rounds <= 5, "the deadline passed after 5 sleeps");
    }
    assert_eq!(rounds, 5);
}

#[sim_test]
async fn sim_ambient_clock_nested_sim_time_stays_off_the_outer_timeline(sim: Sim) {
    // Tokio's clock is one per runtime. An inner sim's advance moves it, but
    // must not move the outer sim's elapsed time.
    let start = ambient_instant();
    let inner = Sim::from_seed(sim.seed.wrapping_add(1));
    inner.advance(HOUR).await;
    drop(inner);
    assert_eq!(
        ambient_instant().saturating_duration_since(start),
        Duration::ZERO,
        "the inner hour is not on the outer timeline"
    );
    sim.advance(HOUR).await;
    assert_eq!(ambient_instant().saturating_duration_since(start), HOUR);
}
