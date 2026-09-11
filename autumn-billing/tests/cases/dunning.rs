//! Tests for dunning: the retry job reads the schedule row, claims one
//! attempt at a time, and a restart re-arms from the store.
//!
//! `TestApp::build` starts an in-process worker that also runs due jobs, so
//! `perform_enqueued_jobs` can run a job the worker already ran. That is
//! safe: every write after the claim is a compare-and-set on the row's state
//! and attempt, so the second run finds the row settled and makes no
//! provider call. Assert through the row and the provider's recorded calls,
//! never through raw run counts.

use std::sync::Arc;
use std::time::Duration;

use autumn_billing::dunning::{RETRY_JOB_NAME, RUNNING_STALE_AFTER, TRANSPORT_RETRY_DELAY};
use autumn_billing::event::{InvoiceSnapshot, SubscriptionSnapshot};
use autumn_billing::model::{DunningAttempt, Invoice, Subscription};
use autumn_billing::provider::PaymentAttemptOutcome;
use autumn_billing::store::CustomerUpsert;
use autumn_billing::{
    BillingConfig, BillingError, BillingEventKind, BillingStore, Currency, DunningPolicy,
    DunningState, ExhaustionAction, InvoiceStatus, MemoryBillingStore, Money, NoHooks, ProviderId,
    SubscriptionStatus,
};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::TickingClock;

use super::support::{
    self, DynHarness, FailingStore, FakeCall, FakeParser, FakeProvider, Harness, PRO_PRICE,
    RecordingHooks, apply_event, at, event, harness_dyn, harness_with_hooks, notification_kinds,
    notification_routes, wait_until,
};

const RECIPIENT: i64 = 42;

fn clocked(app: TestApp) -> TestApp {
    app.with_clock(TickingClock::starting_at(support::base_time()))
        .routes(notification_routes())
}

/// Build an app on `store` and `provider` with `billing`, then mirror a linked
/// customer, an active subscription and one failed invoice (dunning row
/// attempt 1, due at `at(3600)`). Event times lie before the app clock
/// (`base_time()`), as they do in production.
async fn dunning_harness(
    billing: BillingConfig,
    store: Arc<MemoryBillingStore>,
    provider: Arc<FakeProvider>,
) -> Harness {
    let h = harness_with_hooks(billing, Arc::new(NoHooks), store, provider, clocked);
    seed_failed_invoice(&h.client, h.store.as_ref()).await;
    h
}

/// Like [`dunning_harness`] over any store (a failure-injecting one).
async fn dunning_harness_dyn(store: Arc<dyn BillingStore>) -> DynHarness {
    let provider = FakeProvider::with_parser(FakeParser::BillingEventJson);
    let h = harness_dyn(support::config(), store, provider, clocked);
    seed_failed_invoice(&h.client, h.store.as_ref()).await;
    h
}

/// Mirror a linked customer, an active subscription and one failed invoice
/// (dunning row attempt 1, due at `at(3600)`).
async fn seed_failed_invoice(client: &TestClient, store: &dyn BillingStore) {
    store
        .upsert_customer(
            CustomerUpsert::new("local-1", "fake", "cus_1", at(-300))
                .with_user("42")
                .with_email("a@example.test"),
        )
        .await
        .unwrap();
    let sub = BillingEventKind::SubscriptionChanged(
        SubscriptionSnapshot::new("sub_1", "cus_1", SubscriptionStatus::Active)
            .with_price(PRO_PRICE),
    );
    apply_event(client, event("evt_sub", at(-200), sub))
        .await
        .unwrap();
    let failed = BillingEventKind::InvoicePaymentFailed(
        InvoiceSnapshot::new(
            "in_1",
            "cus_1",
            InvoiceStatus::Open,
            Money::from_minor(1999, Currency::USD),
        )
        .with_subscription("sub_1")
        .with_attempt_count(1),
    );
    apply_event(client, event("evt_fail", at(-100), failed))
        .await
        .unwrap();
    client.assert_job_enqueued(RETRY_JOB_NAME);
}

async fn invoice_on(store: &dyn BillingStore) -> Invoice {
    store
        .invoice_by_provider_id(&ProviderId::new("in_1"))
        .await
        .unwrap()
        .expect("invoice mirrored")
}

async fn row_on(store: &dyn BillingStore) -> DunningAttempt {
    let invoice = invoice_on(store).await;
    store
        .dunning_by_invoice(&invoice.id)
        .await
        .unwrap()
        .expect("dunning row")
}

/// Run the retry job handler once for `invoice_id`, as the runtime would.
async fn run_retry_handler(client: &TestClient, invoice_id: &str) -> autumn_web::AutumnResult<()> {
    let job = autumn_billing::dunning::job_infos()
        .into_iter()
        .find(|info| info.name == RETRY_JOB_NAME)
        .expect("retry job registered");
    (job.handler)(
        client.state().clone(),
        serde_json::json!({ "invoice_id": invoice_id }),
    )
    .await
}

async fn standard_harness() -> Harness {
    dunning_harness(
        support::config(),
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
    )
    .await
}

async fn invoice(h: &Harness) -> Invoice {
    h.store
        .invoice_by_provider_id(&ProviderId::new("in_1"))
        .await
        .unwrap()
        .expect("invoice mirrored")
}

async fn row(h: &Harness) -> DunningAttempt {
    let invoice = invoice(h).await;
    h.store
        .dunning_by_invoice(&invoice.id)
        .await
        .unwrap()
        .expect("dunning row")
}

async fn subscription(h: &Harness) -> Subscription {
    h.store
        .subscription_by_provider_id(&ProviderId::new("sub_1"))
        .await
        .unwrap()
        .expect("subscription mirrored")
}

/// Run every recorded job and assert none returned an error.
async fn perform_ok(h: &Harness) {
    h.client
        .perform_enqueued_jobs()
        .await
        .assert_all_succeeded();
}

#[tokio::test]
async fn early_run_re_enqueues_without_a_provider_call() {
    let h = standard_harness().await;
    let before = row(&h).await;
    assert_eq!(before.state, DunningState::Pending);
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 0);
    let after = row(&h).await;
    assert_eq!(after.attempt, 1);
    assert_eq!(after.state, DunningState::Pending);
    assert_eq!(after.next_attempt_at, before.next_attempt_at);
    // The early run put itself back on the queue at the due time.
    h.client.assert_job_enqueued(RETRY_JOB_NAME);
}

#[tokio::test]
async fn due_run_declined_schedules_the_next_attempt() {
    let h = standard_harness().await;
    h.client.advance_clock(Duration::from_secs(3601));
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 1);
    let invoice = invoice(&h).await;
    assert!(h.provider.calls().contains(&FakeCall::RetryInvoice {
        invoice: ProviderId::new("in_1"),
        idempotency_key: format!("autumn-billing:{}:1", invoice.id),
    }));
    let row = row(&h).await;
    assert_eq!(row.attempt, 2);
    assert_eq!(row.state, DunningState::Pending);
    // Second delay in `config()` is two hours, counted from the run.
    assert_eq!(row.next_attempt_at, at(3601 + 7200));
    h.client.assert_job_enqueued(RETRY_JOB_NAME);
    assert_eq!(
        notification_kinds(&h.client, RECIPIENT).await,
        ["billing.payment_failed", "billing.payment_failed"]
    );
    // Running the same recorded job again before the next due time is a no-op.
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 1);
    assert_eq!(row_state(&h).await, (2, DunningState::Pending));
}

async fn row_state(h: &Harness) -> (i64, DunningState) {
    let row = row(h).await;
    (row.attempt, row.state)
}

#[tokio::test]
async fn due_run_paid_recovers_the_invoice() {
    let h = standard_harness().await;
    h.provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    h.client.advance_clock(Duration::from_secs(3601));
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 1);
    assert_eq!(row(&h).await.state, DunningState::Recovered);
    let invoice = invoice(&h).await;
    assert_eq!(invoice.status, InvoiceStatus::Paid);
    assert_eq!(invoice.amount_paid, invoice.amount_due);
    assert_eq!(
        notification_kinds(&h.client, RECIPIENT).await,
        ["billing.payment_failed", "billing.payment_recovered"]
    );
    assert!(h.store.open_dunning().await.unwrap().is_empty());
    // A duplicate run after recovery makes no provider call.
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 1);
}

#[tokio::test]
async fn already_paid_recovers_too() {
    let h = standard_harness().await;
    h.provider
        .script_retry(Ok(PaymentAttemptOutcome::AlreadyPaid));
    h.client.advance_clock(Duration::from_secs(3601));
    perform_ok(&h).await;
    assert_eq!(row(&h).await.state, DunningState::Recovered);
    assert_eq!(invoice(&h).await.status, InvoiceStatus::Paid);
}

#[tokio::test]
async fn exhaustion_marks_unpaid_and_cancels_once() {
    let hooks = RecordingHooks::with_recipient(RECIPIENT);
    let h = harness_with_hooks(
        support::config(),
        hooks.clone(),
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        clocked,
    );
    seed_failed_invoice(&h.client, h.store.as_ref()).await;
    // Three retries: 1h, 2h, 3h. Every one is declined.
    for delay in [3601, 7200, 10_800] {
        h.client.advance_clock(Duration::from_secs(delay));
        perform_ok(&h).await;
    }
    assert_eq!(h.provider.retry_calls(), 3);
    let exhausted: Vec<String> = hooks
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("dunning_exhausted"))
        .collect();
    assert_eq!(exhausted, ["dunning_exhausted:3"]);
    assert_eq!(row(&h).await.state, DunningState::Exhausted);
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Unpaid);
    assert_eq!(h.provider.cancel_calls(), 1);
    assert!(
        h.provider
            .calls()
            .contains(&FakeCall::CancelSubscription(ProviderId::new("sub_1")))
    );
    assert_eq!(
        notification_kinds(&h.client, RECIPIENT).await,
        [
            "billing.payment_failed",
            "billing.payment_failed",
            "billing.payment_failed",
            "billing.dunning_exhausted",
        ]
    );
    assert!(h.store.open_dunning().await.unwrap().is_empty());
    // Nothing left to run.
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 3);
    assert_eq!(h.provider.cancel_calls(), 1);
    assert_eq!(
        hooks
            .calls()
            .iter()
            .filter(|c| c.starts_with("dunning_exhausted"))
            .count(),
        1
    );
}

#[tokio::test]
async fn exhaustion_with_mark_unpaid_keeps_the_provider_subscription() {
    let billing = support::config().dunning(
        DunningPolicy::standard()
            .with_retry_delays(vec![Duration::from_secs(3600)])
            .with_on_exhausted(ExhaustionAction::MarkUnpaid),
    );
    let h = dunning_harness(
        billing,
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
    )
    .await;
    h.client.advance_clock(Duration::from_secs(3601));
    perform_ok(&h).await;
    assert_eq!(row(&h).await.state, DunningState::Exhausted);
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Unpaid);
    assert_eq!(h.provider.cancel_calls(), 0);
}

#[tokio::test]
async fn cancel_failure_on_exhaustion_is_logged_not_retried() {
    let billing = support::config()
        .dunning(DunningPolicy::standard().with_retry_delays(vec![Duration::from_secs(3600)]));
    let h = dunning_harness(
        billing,
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
    )
    .await;
    h.provider
        .script_cancel(Err(BillingError::provider("fake", "timeout")));
    h.client.advance_clock(Duration::from_secs(3601));
    perform_ok(&h).await;
    assert_eq!(row(&h).await.state, DunningState::Exhausted);
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Unpaid);
    assert_eq!(h.provider.cancel_calls(), 1);
    assert!(
        notification_kinds(&h.client, RECIPIENT)
            .await
            .contains(&"billing.dunning_exhausted".to_owned())
    );
}

#[tokio::test]
async fn transport_error_reschedules_the_same_attempt() {
    let h = standard_harness().await;
    h.provider
        .script_retry(Err(BillingError::provider("fake", "connection reset")));
    h.client.advance_clock(Duration::from_secs(3601));
    // The job succeeds: the schedule row stays the truth and the framework
    // never dead-letters it.
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 1);
    let pending = row(&h).await;
    assert_eq!(pending.attempt, 1, "the attempt is not consumed");
    assert_eq!(pending.state, DunningState::Pending);
    assert_eq!(
        pending.next_attempt_at,
        at(3601) + chrono::Duration::from_std(TRANSPORT_RETRY_DELAY).unwrap()
    );
    h.client.assert_job_enqueued(RETRY_JOB_NAME);
    assert_eq!(
        notification_kinds(&h.client, RECIPIENT).await,
        ["billing.payment_failed"]
    );
    // Not due yet: a run before the delay makes no provider call.
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 1);
    // Due: the same attempt runs again with the same idempotency key.
    h.provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    h.client.advance_clock(TRANSPORT_RETRY_DELAY);
    perform_ok(&h).await;
    assert_eq!(h.provider.retry_calls(), 2);
    let invoice = invoice(&h).await;
    let keys: Vec<String> = h
        .provider
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::RetryInvoice {
                idempotency_key, ..
            } => Some(idempotency_key),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec![format!("autumn-billing:{}:1", invoice.id); 2]);
    assert_eq!(row(&h).await.state, DunningState::Recovered);
    assert_eq!(invoice.status, InvoiceStatus::Paid);
}

#[tokio::test]
async fn store_error_after_claim_restores_pending() {
    let inner = MemoryBillingStore::shared();
    let store = FailingStore::wrap(inner.clone());
    // The first read after the claim fails.
    store.fail_on("invoice_by_id", 1);
    let h = dunning_harness_dyn(store.clone()).await;
    h.client.advance_clock(Duration::from_secs(3601));
    let report = h.client.perform_enqueued_jobs().await;
    let failures = report.failures();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].0, RETRY_JOB_NAME);
    assert_eq!(h.provider.retry_calls(), 0);
    // Not stuck in Running: same attempt, same due time.
    let restored = row_on(h.store.as_ref()).await;
    assert_eq!(restored.state, DunningState::Pending);
    assert_eq!(restored.attempt, 1);
    assert_eq!(restored.next_attempt_at, at(3600));
    // The framework's retry claims it again and completes.
    h.provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    let invoice = invoice_on(h.store.as_ref()).await;
    run_retry_handler(&h.client, &invoice.id).await.unwrap();
    assert_eq!(h.provider.retry_calls(), 1);
    assert_eq!(
        row_on(h.store.as_ref()).await.state,
        DunningState::Recovered
    );
    assert_eq!(
        invoice_on(h.store.as_ref()).await.status,
        InvoiceStatus::Paid
    );
}

#[tokio::test]
async fn stale_running_row_is_reclaimed() {
    let h = standard_harness().await;
    h.client.advance_clock(Duration::from_secs(3601));
    let invoice_id = invoice(&h).await.id;
    // A run claimed the attempt and died more than RUNNING_STALE_AFTER ago.
    let stale_since = at(3601) - chrono::Duration::from_std(RUNNING_STALE_AFTER).unwrap();
    assert!(
        h.store
            .claim_dunning_attempt(&invoice_id, 1, stale_since - chrono::Duration::seconds(1))
            .await
            .unwrap()
    );
    h.provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    run_retry_handler(&h.client, &invoice_id).await.unwrap();
    assert_eq!(h.provider.retry_calls(), 1);
    assert_eq!(row(&h).await.state, DunningState::Recovered);
    assert_eq!(invoice(&h).await.status, InvoiceStatus::Paid);

    // A row claimed less than RUNNING_STALE_AFTER ago is in flight elsewhere:
    // no provider call, the row stays Running, and a check is queued.
    let h = standard_harness().await;
    h.client.advance_clock(Duration::from_secs(3601));
    let invoice_id = invoice(&h).await.id;
    assert!(
        h.store
            .claim_dunning_attempt(&invoice_id, 1, at(3601))
            .await
            .unwrap()
    );
    run_retry_handler(&h.client, &invoice_id).await.unwrap();
    assert_eq!(h.provider.retry_calls(), 0);
    let running = row(&h).await;
    assert_eq!(running.state, DunningState::Running);
    assert_eq!(running.updated_at, at(3601));
    h.client.assert_job_enqueued(RETRY_JOB_NAME);
}

#[tokio::test]
async fn settle_after_reconcile_closed_the_row_is_a_no_op() {
    let inner = MemoryBillingStore::shared();
    let store = FailingStore::wrap(inner.clone());
    let h = dunning_harness_dyn(store.clone()).await;
    h.client.advance_clock(Duration::from_secs(3601));
    let invoice = invoice_on(h.store.as_ref()).await;
    // While the provider call is in flight, a subscription.deleted event
    // closes the row. Model it as a write before the job's first settle.
    let backing = inner.clone();
    let mut canceled_row = row_on(h.store.as_ref()).await;
    canceled_row.state = DunningState::Canceled;
    canceled_row.updated_at = at(3601);
    store.before(
        "settle_dunning",
        1,
        Box::new(move || {
            let backing = backing.clone();
            let canceled_row = canceled_row.clone();
            Box::pin(async move {
                backing.upsert_dunning(canceled_row).await.unwrap();
            })
        }),
    );
    h.provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    run_retry_handler(&h.client, &invoice.id).await.unwrap();
    assert_eq!(h.provider.retry_calls(), 1);
    assert_eq!(store.calls("settle_dunning"), 1);
    // The reconcile decision wins: no recovery written, no notification.
    assert_eq!(row_on(h.store.as_ref()).await.state, DunningState::Canceled);
    assert_eq!(
        invoice_on(h.store.as_ref()).await.status,
        InvoiceStatus::Open
    );
    assert_eq!(
        notification_kinds(&h.client, RECIPIENT).await,
        ["billing.payment_failed"]
    );
}

#[tokio::test]
async fn restart_re_arms_pending_rows() {
    let store = MemoryBillingStore::shared();
    let provider = FakeProvider::with_parser(FakeParser::BillingEventJson);
    let app_a = dunning_harness(support::config(), store.clone(), provider.clone()).await;
    let invoice_id = invoice(&app_a).await.id;
    // The process dies before the retry is due.
    drop(app_a);

    let app_b = harness_with_hooks(
        support::config(),
        Arc::new(NoHooks),
        store.clone(),
        provider.clone(),
        clocked,
    );
    wait_until(support::RESTART_TIMEOUT, || async {
        app_b
            .client
            .enqueued_jobs()
            .iter()
            .any(|j| j.name == RETRY_JOB_NAME && j.payload["invoice_id"] == invoice_id)
    })
    .await;
    // Not due yet: no provider call, row untouched.
    assert_eq!(provider.retry_calls(), 0);
    let row = store
        .dunning_by_invoice(&invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, DunningState::Pending);
    assert_eq!(row.next_attempt_at, at(3600));
}

#[tokio::test]
async fn restart_reclaims_an_abandoned_running_row_and_runs_it() {
    let store = MemoryBillingStore::shared();
    let provider = FakeProvider::with_parser(FakeParser::BillingEventJson);
    let app_a = dunning_harness(support::config(), store.clone(), provider.clone()).await;
    let invoice_id = invoice(&app_a).await.id;
    // A due retry was claimed and the process died mid-flight, long enough
    // ago that the new process's clock is past `updated_at + RUNNING_STALE_AFTER`.
    let stale = chrono::Duration::from_std(RUNNING_STALE_AFTER).unwrap();
    let mut abandoned = store
        .dunning_by_invoice(&invoice_id)
        .await
        .unwrap()
        .unwrap();
    abandoned.state = DunningState::Running;
    abandoned.next_attempt_at = at(-3600);
    abandoned.updated_at = at(0) - stale - chrono::Duration::seconds(1);
    store.upsert_dunning(abandoned).await.unwrap();
    drop(app_a);

    provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    let app_b = harness_with_hooks(
        support::config(),
        Arc::new(NoHooks),
        store.clone(),
        provider.clone(),
        clocked,
    );
    // Re-armed at max(next_attempt_at, updated_at + RUNNING_STALE_AFTER),
    // which is due: the worker reclaims it and runs it at once.
    wait_until(support::RESTART_TIMEOUT, || async {
        store
            .dunning_by_invoice(&invoice_id)
            .await
            .unwrap()
            .is_some_and(|r| r.state == DunningState::Recovered)
    })
    .await;
    assert_eq!(provider.retry_calls(), 1);
    assert_eq!(
        store
            .invoice_by_id(&invoice_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        InvoiceStatus::Paid
    );
    drop(app_b);
}

#[tokio::test]
async fn restart_does_not_reset_a_fresh_running_row() {
    let store = MemoryBillingStore::shared();
    let provider = FakeProvider::with_parser(FakeParser::BillingEventJson);
    let app_a = dunning_harness(support::config(), store.clone(), provider.clone()).await;
    let invoice_id = invoice(&app_a).await.id;
    // Claimed just now: another instance may still own it.
    assert!(
        store
            .claim_dunning_attempt(&invoice_id, 1, at(-100))
            .await
            .unwrap()
    );
    drop(app_a);

    let app_b = harness_with_hooks(
        support::config(),
        Arc::new(NoHooks),
        store.clone(),
        provider.clone(),
        clocked,
    );
    wait_until(support::RESTART_TIMEOUT, || async {
        app_b
            .client
            .enqueued_jobs()
            .iter()
            .any(|j| j.name == RETRY_JOB_NAME && j.payload["invoice_id"] == invoice_id)
    })
    .await;
    // Queued for the reclaim threshold, not run: the row is left Running.
    assert_eq!(provider.retry_calls(), 0);
    let row = store
        .dunning_by_invoice(&invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, DunningState::Running);
    assert_eq!(row.updated_at, at(-100));
    drop(app_b);
}

#[tokio::test]
async fn second_process_does_not_double_retry() {
    let store = MemoryBillingStore::shared();
    let provider = FakeProvider::with_parser(FakeParser::BillingEventJson);
    let app_a = dunning_harness(support::config(), store.clone(), provider.clone()).await;
    let invoice_id = invoice(&app_a).await.id;
    // A second instance on the same store re-arms the same row.
    let app_b = harness_with_hooks(
        support::config(),
        Arc::new(NoHooks),
        store.clone(),
        provider.clone(),
        clocked,
    );
    wait_until(support::RESTART_TIMEOUT, || async {
        app_b
            .client
            .enqueued_jobs()
            .iter()
            .any(|j| j.name == RETRY_JOB_NAME && j.payload["invoice_id"] == invoice_id)
    })
    .await;

    // Both clocks pass the due time and both instances run their queue.
    app_a.client.advance_clock(Duration::from_secs(3601));
    app_b.client.advance_clock(Duration::from_secs(3601));
    let (ran_a, ran_b) = tokio::join!(
        app_a.client.perform_enqueued_jobs(),
        app_b.client.perform_enqueued_jobs(),
    );
    ran_a.assert_all_succeeded();
    ran_b.assert_all_succeeded();

    // One claim wins; the other run sees the row taken and skips.
    assert_eq!(provider.retry_calls(), 1);
    let row = store
        .dunning_by_invoice(&invoice_id)
        .await
        .unwrap()
        .expect("dunning row");
    assert_eq!(row.attempt, 2);
    assert_eq!(row.state, DunningState::Pending);
    assert_eq!(row.next_attempt_at, at(3601 + 7200));
    // A later run on either instance is early for attempt 2: still one call.
    perform_ok(&app_a).await;
    perform_ok(&app_b).await;
    assert_eq!(provider.retry_calls(), 1);
}
