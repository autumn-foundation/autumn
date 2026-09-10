//! Tests for dunning: the retry job reads the schedule row, claims one
//! attempt at a time, and a restart re-arms from the store.

use std::sync::Arc;
use std::time::Duration;

use autumn_billing::dunning::RETRY_JOB_NAME;
use autumn_billing::event::{InvoiceSnapshot, SubscriptionSnapshot};
use autumn_billing::model::{DunningAttempt, Invoice, Subscription};
use autumn_billing::provider::PaymentAttemptOutcome;
use autumn_billing::store::CustomerUpsert;
use autumn_billing::{
    BillingConfig, BillingError, BillingEventKind, BillingStore, Currency, DunningPolicy,
    DunningState, ExhaustionAction, InvoiceStatus, MemoryBillingStore, Money, NoHooks,
    ProviderId, SubscriptionStatus,
};
use autumn_web::test::TestApp;
use autumn_web::time::TickingClock;

use super::support::{
    self, FakeCall, FakeParser, FakeProvider, Harness, PRO_PRICE, apply_event, at, event,
    harness_with, notification_kinds, notification_routes,
};

const RECIPIENT: i64 = 42;

fn clocked(app: TestApp) -> TestApp {
    app.with_clock(TickingClock::starting_at(support::base_time()))
        .routes(notification_routes())
}

/// Build an app on `store` and `provider` with `billing`, then mirror a linked
/// customer, an active subscription and one failed invoice (dunning row
/// attempt 1, due at `at(3600)`).
async fn dunning_harness(
    billing: BillingConfig,
    store: Arc<MemoryBillingStore>,
    provider: Arc<FakeProvider>,
) -> Harness {
    let h = harness_with(billing, Arc::new(NoHooks), store, provider, clocked);
    h.store
        .upsert_customer(
            CustomerUpsert::new("local-1", "fake", "cus_1", at(0))
                .with_user("42")
                .with_email("a@example.test"),
        )
        .await
        .unwrap();
    let sub = BillingEventKind::SubscriptionChanged(
        SubscriptionSnapshot::new("sub_1", "cus_1", SubscriptionStatus::Active)
            .with_price(PRO_PRICE),
    );
    apply_event(&h.client, event("evt_sub", at(100), sub))
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
    apply_event(&h.client, event("evt_fail", at(200), failed))
        .await
        .unwrap();
    h.client.assert_job_enqueued(RETRY_JOB_NAME);
    h
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
    h.client.perform_enqueued_jobs().await.assert_all_succeeded();
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
    let h = standard_harness().await;
    // Three retries: 1h, 2h, 3h. Every one is declined.
    for delay in [3601, 7200, 10_800] {
        h.client.advance_clock(Duration::from_secs(delay));
        perform_ok(&h).await;
    }
    assert_eq!(h.provider.retry_calls(), 3);
    assert_eq!(row(&h).await.state, DunningState::Exhausted);
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Unpaid);
    assert_eq!(h.provider.cancel_calls(), 1);
    assert!(h
        .provider
        .calls()
        .contains(&FakeCall::CancelSubscription(ProviderId::new("sub_1"))));
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
async fn transport_error_keeps_the_row_pending_and_fails_the_job() {
    let h = standard_harness().await;
    h.provider
        .script_retry(Err(BillingError::provider("fake", "connection reset")));
    h.client.advance_clock(Duration::from_secs(3601));
    let report = h.client.perform_enqueued_jobs().await;
    let failures = report.failures();
    assert_eq!(failures.len(), 1, "{report:?}");
    assert_eq!(failures[0].0, RETRY_JOB_NAME);
    assert_eq!(h.provider.retry_calls(), 1);
    let row = row(&h).await;
    assert_eq!(row.attempt, 1);
    assert_eq!(row.state, DunningState::Pending);
    assert_eq!(row.next_attempt_at, at(3600));
    assert_eq!(
        notification_kinds(&h.client, RECIPIENT).await,
        ["billing.payment_failed"]
    );
    // The framework's retry finds the row still due and claims it again.
    h.provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    let service = autumn_billing::BillingService::require(h.client.state()).unwrap();
    let invoice = invoice(&h).await;
    let again = autumn_web::job::enqueue(
        RETRY_JOB_NAME,
        serde_json::json!({ "invoice_id": invoice.id }),
    );
    // Direct enqueue goes through the worker; assert via the store.
    again.await.unwrap();
    wait_for(|| async { row(&h).await.state == DunningState::Recovered }).await;
    assert_eq!(h.provider.retry_calls(), 2);
    drop(service);
}

/// Poll `check` every 25 ms for up to five seconds.
async fn wait_for<F, Fut>(check: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if check().await {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "condition not met within five seconds"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn restart_re_arms_pending_rows() {
    let store = MemoryBillingStore::shared();
    let provider = FakeProvider::with_parser(FakeParser::BillingEventJson);
    let app_a = dunning_harness(support::config(), store.clone(), provider.clone()).await;
    let invoice_id = invoice(&app_a).await.id;
    // The process dies before the retry is due.
    drop(app_a);

    let app_b = harness_with(
        support::config(),
        Arc::new(NoHooks),
        store.clone(),
        provider.clone(),
        clocked,
    );
    wait_for(|| async {
        app_b
            .client
            .enqueued_jobs()
            .iter()
            .any(|j| j.name == RETRY_JOB_NAME && j.payload["invoice_id"] == invoice_id)
    })
    .await;
    // Not due yet: no provider call, row untouched.
    assert_eq!(provider.retry_calls(), 0);
    let row = store.dunning_by_invoice(&invoice_id).await.unwrap().unwrap();
    assert_eq!(row.state, DunningState::Pending);
    assert_eq!(row.next_attempt_at, at(3600));
}

#[tokio::test]
async fn restart_resets_a_running_row_and_runs_it() {
    let store = MemoryBillingStore::shared();
    let provider = FakeProvider::with_parser(FakeParser::BillingEventJson);
    let app_a = dunning_harness(support::config(), store.clone(), provider.clone()).await;
    let invoice_id = invoice(&app_a).await.id;
    // A retry was claimed and the process died mid-flight.
    assert!(store.claim_dunning_attempt(&invoice_id, 1, at(3600)).await.unwrap());
    drop(app_a);

    provider.script_retry(Ok(PaymentAttemptOutcome::Paid));
    let app_b = harness_with(
        support::config(),
        Arc::new(NoHooks),
        store.clone(),
        provider.clone(),
        clocked,
    );
    // The row goes back to Pending, due now; the worker runs it at once.
    wait_for(|| async {
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
