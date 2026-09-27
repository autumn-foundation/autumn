//! SLA escalations fire once across replicas (issue #1826).
//!
//! Two app instances share one clock and one obligation store, as two
//! replicas share one database. Each tracks the same obligation, so each has a
//! check job at the deadline. Only one escalation may fire.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::job;
use autumn_web::prelude::*;
use autumn_web::sla::{
    BusinessCalendar, BusinessDuration, MemoryObligationStore, Obligation, Sla, SlaBreach,
    SlaPlugin,
};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::TickingClock;
use chrono::{TimeZone, Utc};

fn replica(clock: &TickingClock, store: &MemoryObligationStore, fired: &Arc<Mutex<Vec<String>>>) -> TestClient {
    let sink = Arc::clone(fired);
    let plugin = SlaPlugin::new()
        .calendar(
            "support",
            BusinessCalendar::weekdays("09:00-17:00".parse().unwrap()),
        )
        .store(store.clone())
        .on_breach("first_response", move |_state: AppState, breach: SlaBreach| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push(breach.key);
                Ok(())
            }
        });
    TestApp::new()
        .with_clock(clock.clone())
        .plugin(plugin)
        .build()
}

/// Let spawned job workers run.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_two_replicas_escalate_once() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Wednesday 2020-01-01 09:00 UTC.
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap());
    let store = MemoryObligationStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let a = replica(&clock, &store, &fired);
    let b = replica(&clock, &store, &fired);

    let obligation = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support");
    let sla_a = Sla::from_state(a.state()).unwrap();
    let sla_b = Sla::from_state(b.state()).unwrap();
    let due_a = sla_a.track(&obligation).await.unwrap().due_at;
    let due_b = sla_b.track(&obligation).await.unwrap().due_at;
    assert_eq!(due_a, due_b);
    assert_eq!(due_a, Some(Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap()));
    settle().await;

    // Go three hours past the start: both check jobs come due.
    let step = Duration::from_secs(3 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;

    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);
    let status = sla_b.get(&obligation.key()).await.unwrap().unwrap();
    assert!(status.escalated_at.is_some());

    job::clear_global_job_client();
}
