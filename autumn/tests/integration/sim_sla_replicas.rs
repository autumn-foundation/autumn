//! SLA escalations fire once across replicas (issue #1826).
//!
//! Two app instances share one clock and one obligation store, as two
//! replicas share one database. Each tracks the same obligation, so each has a
//! check job at the deadline. A barrier holds both checks after they read the
//! store, so both try to claim. Only one escalation may fire.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::job;
use autumn_web::prelude::*;
use autumn_web::sla::{
    BusinessCalendar, BusinessDuration, MemoryObligationStore, Obligation, ObligationRecord,
    ObligationStore, Sla, SlaBreach, SlaError, SlaPlugin, StoreFuture,
};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::TickingClock;
use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::Barrier;

/// A shared store. While armed, the first two `get` calls read, then wait for
/// each other. Thus two checks read "not escalated" before either one claims.
#[derive(Clone)]
struct RacingStore {
    inner: MemoryObligationStore,
    armed: Arc<AtomicBool>,
    barrier: Arc<Barrier>,
    gets: Arc<AtomicUsize>,
    claims: Arc<AtomicUsize>,
    fail_mark_met: Arc<AtomicBool>,
}

impl RacingStore {
    fn new() -> Self {
        Self {
            inner: MemoryObligationStore::new(),
            armed: Arc::default(),
            barrier: Arc::new(Barrier::new(2)),
            gets: Arc::default(),
            claims: Arc::default(),
            fail_mark_met: Arc::default(),
        }
    }
}

impl ObligationStore for RacingStore {
    fn insert(&self, record: ObligationRecord) -> StoreFuture<'_, (ObligationRecord, bool)> {
        self.inner.insert(record)
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObligationRecord>> {
        Box::pin(async move {
            // Read first, then wait: both checks see "not escalated".
            let record = self.inner.get(key).await;
            if self.armed.load(Ordering::SeqCst) && self.gets.fetch_add(1, Ordering::SeqCst) < 2 {
                self.barrier.wait().await;
            }
            record
        })
    }

    fn list(&self) -> StoreFuture<'_, Vec<ObligationRecord>> {
        self.inner.list()
    }

    fn mark_met<'a>(&'a self, key: &'a str, at: DateTime<Utc>) -> StoreFuture<'a, bool> {
        if self.fail_mark_met.load(Ordering::SeqCst) {
            return Box::pin(std::future::ready(Err(SlaError::Store("down".to_owned()))));
        }
        self.inner.mark_met(key, at)
    }

    fn claim_escalation<'a>(
        &'a self,
        key: &'a str,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        self.inner.claim_escalation(key, due_at, at)
    }

    fn release_escalation<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.release_escalation(key)
    }

    fn remove<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool> {
        self.inner.remove(key)
    }
}

fn replica(
    clock: &TickingClock,
    store: &RacingStore,
    fired: &Arc<Mutex<Vec<String>>>,
) -> TestClient {
    let sink = Arc::clone(fired);
    let plugin = SlaPlugin::new()
        .calendar(
            "support",
            BusinessCalendar::weekdays("09:00-17:00".parse().unwrap()),
        )
        .store(store.clone())
        .on_breach(
            "first_response",
            move |_state: AppState, breach: SlaBreach| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().unwrap().push(breach.key);
                    Ok(())
                }
            },
        );
    TestApp::new()
        .with_clock(clock.clone())
        .plugin(plugin)
        .build()
}

/// Let spawned job workers run.
async fn settle() {
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_two_replicas_escalate_once() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Wednesday 2020-01-01 09:00 UTC.
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap());
    let store = RacingStore::new();
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
    assert_eq!(
        due_a,
        Some(Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap())
    );
    settle().await;

    // Go three hours past the start: both check jobs come due and race.
    store.armed.store(true, Ordering::SeqCst);
    let step = Duration::from_secs(3 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    store.armed.store(false, Ordering::SeqCst);

    assert_eq!(store.gets.load(Ordering::SeqCst), 2, "both checks ran");
    assert_eq!(
        store.claims.load(Ordering::SeqCst),
        2,
        "both checks tried to claim"
    );
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);
    let status = sla_b.get(&obligation.key()).await.unwrap().unwrap();
    assert!(status.escalated_at.is_some());

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_track_rolls_back_only_the_record_it_made() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let clock = TickingClock::starting_at(start);
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let sla = Sla::from_state(app.state()).unwrap();
    store.fail_mark_met.store(true, Ordering::SeqCst);

    // A new record, then `mark_met` fails: the record goes away.
    let fresh = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .met_at(start);
    assert!(sla.track(&fresh).await.is_err());
    assert!(sla.get(&fresh.key()).await.unwrap().is_none());

    // A record that another call made stays.
    let shared = Obligation::new("first_response", "ticket:2")
        .within(BusinessDuration::hours(2))
        .calendar("support");
    sla.track(&shared).await.unwrap();
    assert!(sla.track(&shared.clone().met_at(start)).await.is_err());
    assert!(sla.get(&shared.key()).await.unwrap().is_some());

    job::clear_global_job_client();
}
