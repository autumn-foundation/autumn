//! Sim-testing (issue #1797): `#[scheduled]` tasks run under [`Sim`].
//!
//! A task registered with [`TestApp::tasks`] (or by a plugin) starts on the
//! paused runtime when the app is built. Its ticks fire when virtual time
//! crosses their deadlines, with no real sleep. The test counts the ticks after
//! each [`Sim::advance`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;

static EVERY_MINUTE_RUNS: AtomicUsize = AtomicUsize::new(0);
static TOP_OF_HOUR_RUNS: AtomicUsize = AtomicUsize::new(0);
static PLUGIN_TASK_RUNS: AtomicUsize = AtomicUsize::new(0);
static KILLED_APP_RUNS: AtomicUsize = AtomicUsize::new(0);

#[scheduled(every = "1m", name = "sim_every_minute")]
async fn sim_every_minute(_state: AppState) -> AutumnResult<()> {
    EVERY_MINUTE_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[scheduled(cron = "0 0 * * * *", name = "sim_top_of_hour")]
async fn sim_top_of_hour(_state: AppState) -> AutumnResult<()> {
    TOP_OF_HOUR_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[scheduled(every = "10m", name = "sim_plugin_task")]
async fn sim_plugin_task(_state: AppState) -> AutumnResult<()> {
    PLUGIN_TASK_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[scheduled(every = "1m", name = "sim_killed_app_task")]
async fn sim_killed_app_task(_state: AppState) -> AutumnResult<()> {
    KILLED_APP_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Registers a task the way a plugin does, through `AppBuilder::tasks`.
struct TaskPlugin;

impl Plugin for TaskPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.tasks(tasks![sim_plugin_task])
    }
}

/// Advance in one-minute steps, so each fixed-delay tick is a separate timer
/// fire, and settle the work each step releases.
async fn advance_minutes(sim: &Sim, minutes: u64) {
    for _ in 0..minutes {
        sim.advance(Duration::from_secs(60)).await;
        sim.run_to_idle().await;
    }
}

#[sim_test]
async fn fixed_delay_ticks_fire_in_virtual_time(mut sim: Sim) {
    sim.build(TestApp::new().tasks(tasks![sim_every_minute]));
    sim.run_to_idle().await;
    assert_eq!(
        EVERY_MINUTE_RUNS.load(Ordering::SeqCst),
        0,
        "a fixed-delay task waits one full delay before its first tick",
    );

    advance_minutes(&sim, 3).await;
    assert_eq!(
        EVERY_MINUTE_RUNS.load(Ordering::SeqCst),
        3,
        "three virtual minutes give three ticks",
    );
}

#[sim_test]
async fn cron_ticks_fire_at_their_virtual_instants(mut sim: Sim) {
    // The sim clock starts at 2020-01-01T00:00:00Z, so the first top-of-hour
    // tick after build is 01:00.
    sim.build(TestApp::new().tasks(tasks![sim_top_of_hour]));
    sim.run_to_idle().await;

    advance_minutes(&sim, 59).await;
    assert_eq!(
        TOP_OF_HOUR_RUNS.load(Ordering::SeqCst),
        0,
        "no tick before 01:00",
    );

    advance_minutes(&sim, 1).await;
    assert_eq!(
        TOP_OF_HOUR_RUNS.load(Ordering::SeqCst),
        1,
        "one tick at 01:00",
    );

    advance_minutes(&sim, 60).await;
    assert_eq!(
        TOP_OF_HOUR_RUNS.load(Ordering::SeqCst),
        2,
        "a second tick at 02:00",
    );
}

#[sim_test]
async fn plugin_registered_tasks_run_too(mut sim: Sim) {
    sim.build(TestApp::new().plugin(TaskPlugin));
    advance_minutes(&sim, 20).await;
    assert_eq!(
        PLUGIN_TASK_RUNS.load(Ordering::SeqCst),
        2,
        "a plugin's 10-minute task ticks twice in 20 virtual minutes",
    );
}

#[sim_test]
async fn dropping_the_app_stops_its_tasks(mut sim: Sim) {
    sim.build(TestApp::new().tasks(tasks![sim_killed_app_task]));
    advance_minutes(&sim, 2).await;
    let before_kill = KILLED_APP_RUNS.load(Ordering::SeqCst);
    assert_eq!(before_kill, 2, "two ticks before the kill");

    sim.kill();
    advance_minutes(&sim, 5).await;
    assert_eq!(
        KILLED_APP_RUNS.load(Ordering::SeqCst),
        before_kill,
        "a killed app's scheduler loop is cancelled, so no tick fires after it",
    );
}
