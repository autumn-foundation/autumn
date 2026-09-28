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
    BusinessCalendar, BusinessDuration, ESCALATE_JOB, MemoryObligationStore, Obligation,
    ObligationRecord, ObligationStore, Sla, SlaBreach, SlaError, SlaPlugin, StoreFuture,
};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::TickingClock;
use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::Barrier;
use uuid::Uuid;

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

    fn mark_met<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        if self.fail_mark_met.load(Ordering::SeqCst) {
            return Box::pin(std::future::ready(Err(SlaError::Store("down".to_owned()))));
        }
        self.inner.mark_met(key, generation, at)
    }

    fn claim_escalation<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        self.inner.claim_escalation(key, generation, due_at, at)
    }

    fn release_escalation<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> StoreFuture<'a, ()> {
        self.inner.release_escalation(key, generation, claimed_at)
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

    // Two checks read the store, then the escalate job reads it once.
    assert_eq!(store.gets.load(Ordering::SeqCst), 3, "both checks ran");
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
async fn sim_sla_a_failed_track_can_be_retried() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let clock = TickingClock::starting_at(start);
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let sla = Sla::from_state(app.state()).unwrap();
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support");
    sla.track(&ob).await.unwrap();

    // The store fails while a later `track` reports the reply.
    let met = start + chrono::Duration::minutes(30);
    store.fail_mark_met.store(true, Ordering::SeqCst);
    assert!(sla.track(&ob.clone().met_at(met)).await.is_err());
    let status = sla.get(&ob.key()).await.unwrap().unwrap();
    assert_eq!(status.met_at, None, "the record stays, not met yet");

    // The same call again succeeds.
    store.fail_mark_met.store(false, Ordering::SeqCst);
    clock.advance(Duration::from_secs(3600));
    let status = sla.track(&ob.clone().met_at(met)).await.unwrap();
    assert_eq!(status.met_at, Some(met));
    assert_eq!(status.state, autumn_web::sla::ObligationState::Met);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_on_time_met_after_the_claim_cancels_the_breach() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let clock = TickingClock::starting_at(start);
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let sla = Sla::from_state(app.state()).unwrap();
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support");
    sla.track(&ob).await.unwrap();
    let record = store.inner.get(&ob.key()).await.unwrap().unwrap();

    // A check claimed the breach at 11:05. Then a late `track` wrote an
    // on-time met instant (10:30) before the escalate job ran.
    let claimed = due + chrono::Duration::minutes(5);
    assert!(
        store
            .inner
            .claim_escalation(&ob.key(), record.generation, due, claimed)
            .await
            .unwrap()
    );
    let met = Utc.with_ymd_and_hms(2020, 1, 1, 10, 30, 0).unwrap();
    store
        .inner
        .mark_met(&ob.key(), record.generation, met)
        .await
        .unwrap();

    let breach = serde_json::json!({
        "key": ob.key(),
        "obligation": "first_response",
        "subject": "ticket:1",
        "calendar": "support",
        "zone": "UTC",
        "generation": record.generation,
        "started_at": start,
        "due_at": due,
        "escalated_at": claimed,
    });
    clock.advance(Duration::from_secs(3 * 3600));
    let client = app.state().extension::<job::JobClient>().unwrap();
    client.enqueue(ESCALATE_JOB, breach).await.unwrap();
    settle().await;

    assert!(
        fired.lock().unwrap().is_empty(),
        "the breach handler must not run"
    );
    let status = sla.get(&ob.key()).await.unwrap().unwrap();
    assert_eq!(status.state, autumn_web::sla::ObligationState::Met);
    assert_eq!(status.escalated_at, None, "the claim is released");

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_reconcile_moves_a_check_to_an_earlier_deadline() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Wednesday 2020-01-01 09:00 UTC.
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support");

    // Old deploy: work 09:00-10:00, so the deadline is Thursday 10:00.
    {
        let sink = Arc::clone(&fired);
        let old = TestApp::new()
            .with_clock(clock.clone())
            .plugin(
                SlaPlugin::new()
                    .calendar(
                        "support",
                        BusinessCalendar::weekdays("09:00-10:00".parse().unwrap()),
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
                    ),
            )
            .build();
        let status = Sla::from_state(old.state())
            .unwrap()
            .track(&ob)
            .await
            .unwrap();
        assert_eq!(
            status.due_at,
            Some(Utc.with_ymd_and_hms(2020, 1, 2, 10, 0, 0).unwrap())
        );
    }
    job::clear_global_job_client();

    // New deploy: work 09:00-17:00, so the deadline moves to Wednesday 11:00.
    let new = replica(&clock, &store, &fired);
    let sla = Sla::from_state(new.state()).unwrap();
    assert_eq!(sla.reconcile().await.unwrap(), 1);
    settle().await;

    let step = Duration::from_secs(3 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}
