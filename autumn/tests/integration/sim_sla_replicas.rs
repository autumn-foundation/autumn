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
    /// A met instant that a rival `track` writes just before the next
    /// `mark_met`, so that call loses the race.
    rival_met: Arc<Mutex<Option<DateTime<Utc>>>>,
    /// A deadline that a rival `reconcile` stores just before the next
    /// `mark_met`.
    rival_due: Arc<Mutex<Option<DateTime<Utc>>>>,
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
            rival_met: Arc::default(),
            rival_due: Arc::default(),
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
        let rival = self.rival_met.lock().unwrap().take();
        let rival_due = self.rival_due.lock().unwrap().take();
        Box::pin(async move {
            if let Some(due) = rival_due {
                self.inner.set_due(key, generation, due).await?;
            }
            if let Some(rival) = rival {
                self.inner.mark_met(key, generation, rival).await?;
            }
            self.inner.mark_met(key, generation, at).await
        })
    }

    fn set_due<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        due_at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        self.inner.set_due(key, generation, due_at)
    }

    fn begin_dispatch<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        due_at: DateTime<Utc>,
        token: Uuid,
    ) -> StoreFuture<'a, bool> {
        self.inner.begin_dispatch(key, generation, due_at, token)
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
        "token": Uuid::from_u128(41),
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

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_check_that_runs_hours_early_waits_and_escalates() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let clock = TickingClock::starting_at(start);
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);

    // A record with no check of its own, then one check that the queue runs
    // two hours early (its clock leads the app clock).
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let generation = Uuid::from_u128(7);
    store
        .inner
        .insert(ObligationRecord::new(ob.clone(), generation))
        .await
        .unwrap();
    let check = serde_json::json!({ "key": ob.key(), "generation": generation, "due_at": due });
    let client = app.state().extension::<job::JobClient>().unwrap();
    client
        .enqueue(autumn_web::sla::CHECK_JOB, check)
        .await
        .unwrap();
    settle().await;
    assert!(fired.lock().unwrap().is_empty(), "not before the deadline");

    // The app clock reaches the deadline; the waiting check escalates.
    let step = Duration::from_secs(3 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_an_early_check_never_uses_the_deadline_as_now() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let clock = TickingClock::starting_at(start);
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);

    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let generation = Uuid::from_u128(8);
    store
        .inner
        .insert(ObligationRecord::new(ob.clone(), generation))
        .await
        .unwrap();
    let check = serde_json::json!({ "key": ob.key(), "generation": generation, "due_at": due });
    let client = app.state().extension::<job::JobClient>().unwrap();
    client
        .enqueue(autumn_web::sla::CHECK_JOB, check)
        .await
        .unwrap();

    // The queue clock stays ahead for 12 hours, more than the retry budget
    // of the check. The check must not escalate, and must not give up.
    for _ in 0..72 {
        tokio::time::advance(Duration::from_secs(600)).await;
        settle().await;
    }
    assert!(
        fired.lock().unwrap().is_empty(),
        "not before the app deadline"
    );
    assert!(
        store
            .inner
            .get(&ob.key())
            .await
            .unwrap()
            .unwrap()
            .escalated_at
            .is_none()
    );

    // The app clock reaches the deadline; the check escalates.
    clock.advance(Duration::from_secs(3 * 3600));
    for _ in 0..18 {
        tokio::time::advance(Duration::from_secs(600)).await;
        settle().await;
    }
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_track_that_loses_the_met_race_reads_the_stored_met() {
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

    // A rival `track` marks the record met between this call's insert and
    // its `mark_met`. This call must return the stored met, not a stale
    // running status.
    clock.advance(Duration::from_secs(3600));
    let rival = start + chrono::Duration::minutes(20);
    *store.rival_met.lock().unwrap() = Some(rival);
    let status = sla
        .track(&ob.clone().met_at(start + chrono::Duration::minutes(40)))
        .await
        .unwrap();
    assert_eq!(status.state, autumn_web::sla::ObligationState::Met);
    assert_eq!(status.met_at, Some(rival));

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_stale_calendar_does_not_claim_before_the_stored_deadline() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let old_due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let new_due = Utc.with_ymd_and_hms(2020, 1, 1, 15, 0, 0).unwrap();
    let clock = TickingClock::starting_at(start);
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    // This replica has the old calendar: its deadline is 11:00.
    let app = replica(&clock, &store, &fired);

    // A replica with a new calendar reconciled the record to 15:00.
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let generation = Uuid::from_u128(11);
    store
        .inner
        .insert(ObligationRecord::new(ob.clone(), generation).with_due_at(new_due))
        .await
        .unwrap();
    // The old check, at the old deadline.
    let check = serde_json::json!({ "key": ob.key(), "generation": generation, "due_at": old_due });
    let client = app.state().extension::<job::JobClient>().unwrap();
    client
        .enqueue_due(autumn_web::sla::CHECK_JOB, check, Some(old_due))
        .await
        .unwrap();

    let step = Duration::from_secs(3 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert!(
        fired.lock().unwrap().is_empty(),
        "not before the stored deadline"
    );
    let record = store.inner.get(&ob.key()).await.unwrap().unwrap();
    assert!(record.escalated_at.is_none());

    // At the stored deadline, the check escalates.
    clock.advance(step);
    tokio::time::advance(step).await;
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_stale_calendar_reads_the_stored_deadline_status() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Wednesday 2020-01-01. This replica's calendar puts the deadline at
    // 11:00. The stored deadline is Thursday 15:00.
    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let stored = Utc.with_ymd_and_hms(2020, 1, 2, 15, 0, 0).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 18, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    store
        .inner
        .insert(ObligationRecord::new(ob.clone(), Uuid::from_u128(12)).with_due_at(stored))
        .await
        .unwrap();

    // 18:00 is outside working time: paused, with the time to the stored
    // deadline left (Thursday 09:00-15:00).
    let status = Sla::from_state(app.state())
        .unwrap()
        .get(&ob.key())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.state, autumn_web::sla::ObligationState::Paused);
    assert_eq!(status.due_at, Some(stored));
    assert_eq!(
        status.resumes_at,
        Some(Utc.with_ymd_and_hms(2020, 1, 2, 9, 0, 0).unwrap())
    );
    assert_eq!(status.remaining, Duration::from_secs(6 * 3600));

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_an_escalation_enqueued_again_after_it_ran_does_not_fire_again() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let generation = Uuid::from_u128(13);
    store
        .inner
        .insert(ObligationRecord::new(ob.clone(), generation).with_due_at(due))
        .await
        .unwrap();
    let breach = serde_json::json!({
        "key": ob.key(),
        "obligation": "first_response",
        "subject": "ticket:1",
        "calendar": "support",
        "zone": "UTC",
        "generation": generation,
        "started_at": start,
        "due_at": due,
        "escalated_at": due,
        "token": Uuid::from_u128(42),
    });
    let client = app.state().extension::<job::JobClient>().unwrap();

    // The first enqueue runs. Postgres can commit it and still report an
    // error, so the check enqueues the same breach again after it ran.
    client.enqueue(ESCALATE_JOB, breach.clone()).await.unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(fired.lock().unwrap().len(), 1);
    client.enqueue(ESCALATE_JOB, breach).await.unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_deadline_restored_while_its_check_runs_still_escalates() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Wednesday 2020-01-01 09:00. The deadline on this calendar is 11:00.
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
    settle().await;
    let generation = store
        .inner
        .get(&ob.key())
        .await
        .unwrap()
        .unwrap()
        .generation;

    // Another calendar version moved the stored deadline to 15:00.
    let later = Utc.with_ymd_and_hms(2020, 1, 1, 15, 0, 0).unwrap();
    assert!(
        store
            .inner
            .set_due(&ob.key(), generation, later)
            .await
            .unwrap()
    );

    // At 11:00 the old check reads the 15:00 record, then waits.
    store.armed.store(true, Ordering::SeqCst);
    let step = Duration::from_secs(2 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    for _ in 0..16 {
        if store.gets.load(Ordering::SeqCst) > 0 {
            break;
        }
        settle().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(store.gets.load(Ordering::SeqCst), 1, "the check waits");

    // While it waits, reconcile restores 11:00. Then the check continues.
    assert_eq!(sla.reconcile().await.unwrap(), 1);
    let _ = store.get(&ob.key()).await.unwrap();
    for _ in 0..4 {
        settle().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(
        *fired.lock().unwrap(),
        ["first_response/ticket:1"],
        "the restored 11:00 deadline escalates now, not at 15:00"
    );

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_reconcile_retry_is_not_absorbed_by_a_running_check() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Wednesday 2020-01-01 09:00. The deadline on this calendar is 11:00.
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
    settle().await;
    let key = ob.key();
    let generation = store.inner.get(&key).await.unwrap().unwrap().generation;
    let eleven = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let later = Utc.with_ymd_and_hms(2020, 1, 1, 15, 0, 0).unwrap();
    assert!(store.inner.set_due(&key, generation, later).await.unwrap());

    // At 11:00 the old check reads the 15:00 record, then waits.
    store.armed.store(true, Ordering::SeqCst);
    let step = Duration::from_secs(2 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    for _ in 0..16 {
        if store.gets.load(Ordering::SeqCst) > 0 {
            break;
        }
        settle().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(store.gets.load(Ordering::SeqCst), 1, "the check waits");

    // A reconcile stored 11:00 but its enqueue failed. The retry sees an
    // unchanged deadline and must still add a check.
    assert!(store.inner.set_due(&key, generation, eleven).await.unwrap());
    assert_eq!(sla.reconcile().await.unwrap(), 1);
    let _ = store.get(&key).await.unwrap();
    for _ in 0..4 {
        settle().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_an_on_time_met_clears_a_newer_claim_than_the_job_payload() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let first_claim = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 1).unwrap();
    let retry_claim = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 5).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let key = ob.key();
    let generation = Uuid::from_u128(14);
    store
        .inner
        .insert(ObligationRecord::new(ob, generation).with_due_at(due))
        .await
        .unwrap();
    // The retry of the check holds the claim; the queued job has the first
    // claim instant. Then a late `track` reports a reply before the deadline.
    assert!(
        store
            .inner
            .claim_escalation(&key, generation, due, retry_claim)
            .await
            .unwrap()
    );
    let met = Utc.with_ymd_and_hms(2020, 1, 1, 10, 30, 0).unwrap();
    assert!(store.inner.mark_met(&key, generation, met).await.unwrap());

    let breach = serde_json::json!({
        "key": key,
        "obligation": "first_response",
        "subject": "ticket:1",
        "calendar": "support",
        "zone": "UTC",
        "generation": generation,
        "started_at": start,
        "due_at": due,
        "escalated_at": first_claim,
        "token": Uuid::from_u128(43),
    });
    let client = app.state().extension::<job::JobClient>().unwrap();
    client.enqueue(ESCALATE_JOB, breach).await.unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;

    assert!(fired.lock().unwrap().is_empty(), "met on time: no breach");
    let record = store.inner.get(&key).await.unwrap().unwrap();
    assert_eq!(record.escalated_at, None, "the stored claim is released");

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_track_reads_a_deadline_that_changed_before_its_met_write() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Wednesday 2020-01-01 09:00. The deadline on this calendar is 11:00.
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

    // A rival reconcile stores 15:00 just before this call writes a 12:00
    // reply. The stored record is met on time.
    clock.advance(Duration::from_secs(4 * 3600));
    let later = Utc.with_ymd_and_hms(2020, 1, 1, 15, 0, 0).unwrap();
    *store.rival_due.lock().unwrap() = Some(later);
    let met = Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap();
    let status = sla.track(&ob.clone().met_at(met)).await.unwrap();
    assert_eq!(status.due_at, Some(later));
    assert_eq!(status.state, autumn_web::sla::ObligationState::Met);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_reconcile_stores_a_later_deadline_for_a_met_record() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // Budget 6 h from 09:00: 15:00 on this calendar. The record still has
    // the old 11:00 deadline and a 12:00 reply, so it reads as breached.
    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let old_due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let new_due = Utc.with_ymd_and_hms(2020, 1, 1, 15, 0, 0).unwrap();
    let met = Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 13, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let sla = Sla::from_state(app.state()).unwrap();
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(6))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC)
        .met_at(met);
    let key = ob.key();
    let generation = Uuid::from_u128(15);
    store
        .inner
        .insert(ObligationRecord::new(ob, generation).with_due_at(old_due))
        .await
        .unwrap();

    // reconcile stores 15:00; no check is needed for a met record.
    assert_eq!(sla.reconcile().await.unwrap(), 0);
    let record = store.inner.get(&key).await.unwrap().unwrap();
    assert_eq!(record.due_at, Some(new_due));

    // The old 11:00 check then finds the reply on time.
    let check = serde_json::json!({ "key": key, "generation": generation, "due_at": old_due });
    let client = app.state().extension::<job::JobClient>().unwrap();
    client
        .enqueue(autumn_web::sla::CHECK_JOB, check)
        .await
        .unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert!(fired.lock().unwrap().is_empty(), "no false breach");
    assert_eq!(
        sla.get(&key).await.unwrap().unwrap().state,
        autumn_web::sla::ObligationState::Met
    );

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_reconcile_with_no_deadline_stops_the_old_check() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let old_due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 10, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let key = ob.key();
    let generation = Uuid::from_u128(16);
    store
        .inner
        .insert(ObligationRecord::new(ob, generation).with_due_at(old_due))
        .await
        .unwrap();

    // The new calendar has no working time, so there is no deadline.
    {
        let new = TestApp::new()
            .with_clock(clock.clone())
            .plugin(
                SlaPlugin::new()
                    .calendar("support", BusinessCalendar::new())
                    .store(store.clone()),
            )
            .build();
        let sla = Sla::from_state(new.state()).unwrap();
        assert_eq!(sla.reconcile().await.unwrap(), 0);
        let status = sla.get(&key).await.unwrap().unwrap();
        assert_eq!(status.due_at, None);
    }
    job::clear_global_job_client();

    // A replica with the old calendar runs the old 11:00 check.
    let old = replica(&clock, &store, &fired);
    let check = serde_json::json!({ "key": key, "generation": generation, "due_at": old_due });
    let client = old.state().extension::<job::JobClient>().unwrap();
    clock.advance(Duration::from_secs(2 * 3600));
    client
        .enqueue(autumn_web::sla::CHECK_JOB, check)
        .await
        .unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert!(fired.lock().unwrap().is_empty(), "no deadline: no breach");
    let record = store.inner.get(&key).await.unwrap().unwrap();
    assert!(record.escalated_at.is_none());

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_second_escalation_after_the_unique_window_does_not_fire() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let generation = Uuid::from_u128(17);
    store
        .inner
        .insert(ObligationRecord::new(ob.clone(), generation).with_due_at(due))
        .await
        .unwrap();
    let breach = |token: u128| {
        serde_json::json!({
            "key": ob.key(),
            "obligation": "first_response",
            "subject": "ticket:1",
            "calendar": "support",
            "zone": "UTC",
            "generation": generation,
            "started_at": start,
            "due_at": due,
            "escalated_at": due,
            "token": Uuid::from_u128(token),
        })
    };
    let client = app.state().extension::<job::JobClient>().unwrap();

    client.enqueue(ESCALATE_JOB, breach(1)).await.unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(fired.lock().unwrap().len(), 1);

    // More than a day later the unique key has expired. A second
    // escalation for the same record must not run the handler again.
    let step = Duration::from_secs(25 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    client.enqueue(ESCALATE_JOB, breach(2)).await.unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_stale_escalation_does_not_fire_before_the_new_deadline() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    // The escalate job for 11:00 is still queued, its claim was released,
    // and reconcile then stored 15:00.
    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let old_due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let new_due = Utc.with_ymd_and_hms(2020, 1, 1, 15, 0, 0).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let key = ob.key();
    let generation = Uuid::from_u128(18);
    store
        .inner
        .insert(ObligationRecord::new(ob, generation).with_due_at(new_due))
        .await
        .unwrap();
    let stale = serde_json::json!({
        "key": key,
        "obligation": "first_response",
        "subject": "ticket:1",
        "calendar": "support",
        "zone": "UTC",
        "generation": generation,
        "started_at": start,
        "due_at": old_due,
        "escalated_at": old_due,
        "token": Uuid::from_u128(31),
    });
    let client = app.state().extension::<job::JobClient>().unwrap();
    client.enqueue(ESCALATE_JOB, stale).await.unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert!(fired.lock().unwrap().is_empty(), "open until 15:00");

    // At 15:00 the check for the stored deadline escalates one time.
    let check = serde_json::json!({ "key": key, "generation": generation, "due_at": new_due });
    let step = Duration::from_secs(4 * 3600);
    clock.advance(step);
    tokio::time::advance(step).await;
    client
        .enqueue(autumn_web::sla::CHECK_JOB, check)
        .await
        .unwrap();
    for _ in 0..4 {
        settle().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn sim_sla_a_dispatched_escalation_retries_after_an_on_time_backfill() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let start = Utc.with_ymd_and_hms(2020, 1, 1, 9, 0, 0).unwrap();
    let due = Utc.with_ymd_and_hms(2020, 1, 1, 11, 0, 0).unwrap();
    let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap());
    let store = RacingStore::new();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let app = replica(&clock, &store, &fired);
    let ob = Obligation::new("first_response", "ticket:1")
        .within(BusinessDuration::hours(2))
        .calendar("support")
        .starting_at(start)
        .zone(chrono_tz::Tz::UTC);
    let key = ob.key();
    let generation = Uuid::from_u128(19);
    let token = Uuid::from_u128(51);
    store
        .inner
        .insert(ObligationRecord::new(ob, generation).with_due_at(due))
        .await
        .unwrap();
    // The first attempt began the dispatch, then its handler failed. An
    // on-time reply is backfilled before the retry.
    assert!(
        store
            .inner
            .claim_escalation(&key, generation, due, due)
            .await
            .unwrap()
    );
    assert!(
        store
            .inner
            .begin_dispatch(&key, generation, due, token)
            .await
            .unwrap()
    );
    let met = Utc.with_ymd_and_hms(2020, 1, 1, 10, 30, 0).unwrap();
    assert!(store.inner.mark_met(&key, generation, met).await.unwrap());

    // The retry has the same token and must still run the handler.
    let retry = serde_json::json!({
        "key": key,
        "obligation": "first_response",
        "subject": "ticket:1",
        "calendar": "support",
        "zone": "UTC",
        "generation": generation,
        "started_at": start,
        "due_at": due,
        "escalated_at": due,
        "token": token,
    });
    let client = app.state().extension::<job::JobClient>().unwrap();
    client.enqueue(ESCALATE_JOB, retry).await.unwrap();
    settle().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(*fired.lock().unwrap(), ["first_response/ticket:1"]);

    job::clear_global_job_client();
}
