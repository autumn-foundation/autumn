//! Tests for reconcile: one event applied once, ordering, customer linking,
//! dunning open/close and notifications.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_billing::dunning::RETRY_JOB_NAME;
use autumn_billing::event::{InvoiceSnapshot, SubscriptionSnapshot};
use autumn_billing::hooks::HookFuture;
use autumn_billing::model::{DunningAttempt, DunningState, Invoice, Subscription};
use autumn_billing::store::CustomerUpsert;
use autumn_billing::{
    BillingEventKind, BillingHooks, BillingStore, Currency, DunningPolicy, InvoiceStatus,
    MemoryBillingStore, Money, PlanId, ProviderId, ReconcileOutcome, SubscriptionStatus,
};
use autumn_web::time::TickingClock;
use serde_json::json;

use super::support::{
    self, FailingStore, FakeParser, FakeProvider, Harness, PRO_PRICE, apply_event, at,
    checkout_kind, event, harness, harness_dyn, harness_with_hooks, notification_kinds,
    notification_routes, notifications_for, post_webhook,
};

const USER: &str = "42";
const RECIPIENT: i64 = 42;

/// A harness with a linked customer `cus_1` (user 42), a ticking clock at
/// `base_time()`, and the notification test route.
async fn linked_harness() -> Harness {
    let h = harness(
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        |app| {
            app.with_clock(TickingClock::starting_at(support::base_time()))
                .routes(notification_routes())
        },
    );
    link_customer(&h, "cus_1").await;
    h
}

async fn link_customer(h: &Harness, provider_customer_id: &str) {
    h.store
        .upsert_customer(
            CustomerUpsert::new("local-1", "fake", provider_customer_id, at(0))
                .with_user(USER)
                .with_email("a@example.test"),
        )
        .await
        .unwrap();
}

fn sub_changed(status: SubscriptionStatus) -> BillingEventKind {
    BillingEventKind::SubscriptionChanged(
        SubscriptionSnapshot::new("sub_1", "cus_1", status).with_price(PRO_PRICE),
    )
}

fn sub_deleted() -> BillingEventKind {
    BillingEventKind::SubscriptionDeleted(
        SubscriptionSnapshot::new("sub_1", "cus_1", SubscriptionStatus::Canceled)
            .with_price(PRO_PRICE),
    )
}

fn invoice_failed(attempt_count: i64) -> BillingEventKind {
    BillingEventKind::InvoicePaymentFailed(
        InvoiceSnapshot::new(
            "in_1",
            "cus_1",
            InvoiceStatus::Open,
            Money::from_minor(1999, Currency::USD),
        )
        .with_subscription("sub_1")
        .with_attempt_count(attempt_count),
    )
}

fn invoice_paid() -> BillingEventKind {
    BillingEventKind::InvoicePaid(
        InvoiceSnapshot::new(
            "in_1",
            "cus_1",
            InvoiceStatus::Paid,
            Money::from_minor(1999, Currency::USD),
        )
        .with_subscription("sub_1")
        .with_amount_paid(Money::from_minor(1999, Currency::USD)),
    )
}

async fn subscription(h: &Harness) -> Subscription {
    h.store
        .subscription_by_provider_id(&ProviderId::new("sub_1"))
        .await
        .unwrap()
        .expect("subscription mirrored")
}

async fn invoice(h: &Harness) -> Invoice {
    h.store
        .invoice_by_provider_id(&ProviderId::new("in_1"))
        .await
        .unwrap()
        .expect("invoice mirrored")
}

async fn dunning(h: &Harness) -> Option<DunningAttempt> {
    let invoice = invoice(h).await;
    h.store.dunning_by_invoice(&invoice.id).await.unwrap()
}

/// Active subscription, then a failed invoice at `t = 200`.
async fn failed_invoice_harness() -> Harness {
    let h = linked_harness().await;
    apply_event(
        &h.client,
        event("evt_sub", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    let outcome = apply_event(&h.client, event("evt_fail", at(200), invoice_failed(1)))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ReconcileOutcome::Applied {
            kind: "invoice_payment_failed"
        }
    );
    h
}

#[tokio::test]
async fn subscribe_event_mirrors_active_subscription() {
    let h = linked_harness().await;
    let outcome = apply_event(
        &h.client,
        event("evt_1", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        ReconcileOutcome::Applied {
            kind: "subscription_changed"
        }
    );
    let sub = subscription(&h).await;
    assert_eq!(sub.status, SubscriptionStatus::Active);
    assert_eq!(sub.plan_id, Some(PlanId::new("pro")));
    assert_eq!(sub.customer_id, "local-1");
    assert_eq!(sub.last_event_at, at(100));
    assert_eq!(h.store.applied_event_count().await.unwrap(), 1);
}

#[tokio::test]
async fn duplicate_event_id_is_applied_once() {
    let h = linked_harness().await;
    let first = event("evt_1", at(100), sub_changed(SubscriptionStatus::Active));
    apply_event(&h.client, first.clone()).await.unwrap();
    let again = apply_event(&h.client, first).await.unwrap();
    assert_eq!(again, ReconcileOutcome::Duplicate);
    assert_eq!(h.store.applied_event_count().await.unwrap(), 1);
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Active);
}

#[tokio::test]
async fn ignored_event_is_recorded_once() {
    let h = linked_harness().await;
    let ignored = event(
        "evt_x",
        at(100),
        BillingEventKind::Ignored {
            event_type: "charge.refunded".to_owned(),
        },
    );
    let outcome = apply_event(&h.client, ignored.clone()).await.unwrap();
    assert_eq!(
        outcome,
        ReconcileOutcome::Ignored {
            event_type: "charge.refunded".to_owned()
        }
    );
    assert_eq!(
        apply_event(&h.client, ignored).await.unwrap(),
        ReconcileOutcome::Duplicate
    );
    assert_eq!(h.store.applied_event_count().await.unwrap(), 1);
}

#[tokio::test]
async fn out_of_order_deleted_then_updated_stays_canceled() {
    let h = linked_harness().await;
    apply_event(&h.client, event("evt_del", at(200), sub_deleted()))
        .await
        .unwrap();
    let late = apply_event(
        &h.client,
        event("evt_upd", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    assert!(matches!(late, ReconcileOutcome::Applied { .. }));
    let sub = subscription(&h).await;
    assert_eq!(sub.status, SubscriptionStatus::Canceled);
    assert_eq!(sub.last_event_at, at(200));
    assert_eq!(h.store.applied_event_count().await.unwrap(), 2);
}

#[tokio::test]
async fn same_second_deleted_and_updated_stays_canceled() {
    // deleted first, then updated
    let h = linked_harness().await;
    apply_event(&h.client, event("evt_del", at(100), sub_deleted()))
        .await
        .unwrap();
    apply_event(
        &h.client,
        event("evt_upd", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Canceled);

    // updated first, then deleted
    let h = linked_harness().await;
    apply_event(
        &h.client,
        event("evt_upd", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    apply_event(&h.client, event("evt_del", at(100), sub_deleted()))
        .await
        .unwrap();
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Canceled);
}

#[tokio::test]
async fn unknown_customer_event_creates_unlinked_customer() {
    let h = harness(
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        |app| app,
    );
    let kind = BillingEventKind::SubscriptionChanged(
        SubscriptionSnapshot::new("sub_9", "cus_9", SubscriptionStatus::Trialing)
            .with_price(PRO_PRICE),
    );
    apply_event(&h.client, event("evt_1", at(100), kind))
        .await
        .unwrap();
    let customer = h
        .store
        .customer_by_provider_id(&ProviderId::new("cus_9"))
        .await
        .unwrap()
        .expect("customer created");
    assert_eq!(customer.user_id, None);
    assert_eq!(customer.provider, "fake");
    let sub = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_9"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sub.customer_id, customer.id);
    assert_eq!(sub.status, SubscriptionStatus::Trialing);
}

#[tokio::test]
async fn checkout_with_foreign_local_ref_does_not_link() {
    let h = linked_harness().await;
    // `local-1` belongs to `cus_1`; the event is for `cus_2`.
    let kind = checkout_kind("cus_2", Some("local-1"), Some("b@example.test"), None);
    let outcome = apply_event(&h.client, event("evt_co", at(100), kind))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ReconcileOutcome::Applied {
            kind: "checkout_completed"
        }
    );
    let foreign = h
        .store
        .customer_by_provider_id(&ProviderId::new("cus_2"))
        .await
        .unwrap()
        .expect("customer created");
    assert_eq!(foreign.user_id, None);
    assert_eq!(foreign.email.as_deref(), Some("b@example.test"));
    let own = h.store.customer_by_id("local-1").await.unwrap().unwrap();
    assert_eq!(own.user_id.as_deref(), Some(USER));
    assert_eq!(own.provider_customer_id, ProviderId::new("cus_1"));
}

#[tokio::test]
async fn checkout_with_own_local_ref_keeps_link_and_refreshes_email() {
    let h = linked_harness().await;
    let kind = checkout_kind(
        "cus_1",
        Some("local-1"),
        Some("new@example.test"),
        Some("sub_1"),
    );
    apply_event(&h.client, event("evt_co", at(100), kind))
        .await
        .unwrap();
    let own = h.store.customer_by_id("local-1").await.unwrap().unwrap();
    assert_eq!(own.user_id.as_deref(), Some(USER));
    assert_eq!(own.email.as_deref(), Some("new@example.test"));
    // No email match ever links: a checkout for an unknown customer stays unlinked.
    let kind = checkout_kind("cus_3", None, Some("a@example.test"), None);
    apply_event(&h.client, event("evt_co2", at(101), kind))
        .await
        .unwrap();
    let other = h
        .store
        .customer_by_provider_id(&ProviderId::new("cus_3"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(other.user_id, None);
}

#[tokio::test]
async fn payment_failed_opens_dunning_enqueues_retry_and_notifies() {
    let h = failed_invoice_harness().await;
    let invoice = invoice(&h).await;
    assert_eq!(invoice.status, InvoiceStatus::Open);
    assert_eq!(invoice.attempt_count, 1);
    let sub = subscription(&h).await;
    assert_eq!(invoice.subscription_id.as_deref(), Some(sub.id.as_str()));

    let row = dunning(&h).await.expect("dunning row opened");
    assert_eq!(row.attempt, 1);
    assert_eq!(row.state, DunningState::Pending);
    assert_eq!(row.customer_id, "local-1");
    assert_eq!(row.subscription_id.as_deref(), Some(sub.id.as_str()));
    // `config()` sets the first retry delay to one hour; the clock is at base_time.
    assert_eq!(row.next_attempt_at, at(3600));

    h.client
        .assert_job_enqueued_with(RETRY_JOB_NAME, json!({ "invoice_id": invoice.id }));

    let notes = notifications_for(&h.client, RECIPIENT).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].kind, "billing.payment_failed");
    assert_eq!(notes[0].payload["invoice_id"], json!(invoice.id));
    assert_eq!(notes[0].payload["provider_invoice_id"], json!("in_1"));
    assert_eq!(notes[0].payload["attempt"], json!(1));
    assert_eq!(notes[0].payload["amount_due"]["minor"], json!(1999));
    assert_eq!(h.provider.retry_calls(), 0);
}

#[tokio::test]
async fn second_payment_failed_keeps_pending_row() {
    let h = failed_invoice_harness().await;
    h.client.advance_clock(Duration::from_secs(600));
    let outcome = apply_event(&h.client, event("evt_fail2", at(300), invoice_failed(2)))
        .await
        .unwrap();
    assert!(matches!(outcome, ReconcileOutcome::Applied { .. }));
    assert_eq!(invoice(&h).await.attempt_count, 2);
    let row = dunning(&h).await.unwrap();
    assert_eq!(row.attempt, 1);
    assert_eq!(row.state, DunningState::Pending);
    assert_eq!(row.next_attempt_at, at(3600));
    let kinds = notification_kinds(&h.client, RECIPIENT).await;
    assert_eq!(kinds, ["billing.payment_failed", "billing.payment_failed"]);
}

#[tokio::test]
async fn invoice_paid_recovers_dunning() {
    let h = failed_invoice_harness().await;
    let outcome = apply_event(&h.client, event("evt_paid", at(300), invoice_paid()))
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ReconcileOutcome::Applied {
            kind: "invoice_paid"
        }
    );
    assert_eq!(invoice(&h).await.status, InvoiceStatus::Paid);
    assert_eq!(dunning(&h).await.unwrap().state, DunningState::Recovered);
    let kinds = notification_kinds(&h.client, RECIPIENT).await;
    assert_eq!(
        kinds,
        ["billing.payment_failed", "billing.payment_recovered"]
    );
    // A stale paid event (older than the failure) changes nothing.
    let outcome = apply_event(&h.client, event("evt_old", at(150), invoice_paid()))
        .await
        .unwrap();
    assert!(matches!(outcome, ReconcileOutcome::Applied { .. }));
    assert_eq!(notification_kinds(&h.client, RECIPIENT).await.len(), 2);
}

#[tokio::test]
async fn paid_invoice_without_dunning_sends_no_recovery_notification() {
    let h = linked_harness().await;
    apply_event(
        &h.client,
        event("evt_sub", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    apply_event(&h.client, event("evt_paid", at(200), invoice_paid()))
        .await
        .unwrap();
    assert_eq!(invoice(&h).await.status, InvoiceStatus::Paid);
    assert!(dunning(&h).await.is_none());
    assert!(notifications_for(&h.client, RECIPIENT).await.is_empty());
    h.client.assert_no_jobs_enqueued();
}

#[tokio::test]
async fn subscription_deleted_closes_open_dunning_and_notifies() {
    let h = failed_invoice_harness().await;
    apply_event(&h.client, event("evt_del", at(300), sub_deleted()))
        .await
        .unwrap();
    assert_eq!(subscription(&h).await.status, SubscriptionStatus::Canceled);
    assert_eq!(dunning(&h).await.unwrap().state, DunningState::Canceled);
    assert!(h.store.open_dunning().await.unwrap().is_empty());
    let kinds = notification_kinds(&h.client, RECIPIENT).await;
    assert_eq!(
        kinds,
        ["billing.payment_failed", "billing.subscription_canceled"]
    );
}

#[tokio::test]
async fn redelivery_after_schedule_failure_completes_the_side_effects() {
    let inner = MemoryBillingStore::shared();
    let store = FailingStore::wrap(inner.clone());
    let h = harness_dyn(
        support::config(),
        store.clone(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        |app| {
            app.with_clock(TickingClock::starting_at(support::base_time()))
                .routes(notification_routes())
        },
    );
    h.store
        .upsert_customer(
            CustomerUpsert::new("local-1", "fake", "cus_1", at(0))
                .with_user(USER)
                .with_email("a@example.test"),
        )
        .await
        .unwrap();
    apply_event(
        &h.client,
        event("evt_sub", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    let applied_before = h.store.applied_event_count().await.unwrap();
    // The invoice is mirrored, then opening the dunning row fails.
    store.fail_on("upsert_dunning", 1);
    let body = serde_json::to_vec(&event("evt_fail", at(200), invoice_failed(1))).unwrap();
    let first = post_webhook(&h.client, &body).await;
    assert_eq!(first.status.as_u16(), 500, "{}", first.text());
    assert_eq!(
        h.store.applied_event_count().await.unwrap(),
        applied_before,
        "the claim was released"
    );
    let invoice = h
        .store
        .invoice_by_provider_id(&ProviderId::new("in_1"))
        .await
        .unwrap()
        .expect("invoice mirrored by the failed delivery");
    assert!(
        h.store
            .dunning_by_invoice(&invoice.id)
            .await
            .unwrap()
            .is_none()
    );
    h.client.assert_no_jobs_enqueued();

    // The provider redelivers. The invoice write is `Unchanged`; the
    // dunning row and the job are still made.
    let second = post_webhook(&h.client, &body).await;
    assert_eq!(second.status.as_u16(), 200, "{}", second.text());
    assert_eq!(second.json::<serde_json::Value>()["outcome"], "applied");
    assert_eq!(
        h.store.applied_event_count().await.unwrap(),
        applied_before + 1
    );
    let row = h
        .store
        .dunning_by_invoice(&invoice.id)
        .await
        .unwrap()
        .expect("dunning row opened on redelivery");
    assert_eq!(row.state, DunningState::Pending);
    assert_eq!(row.attempt, 1);
    h.client
        .assert_job_enqueued_with(RETRY_JOB_NAME, json!({ "invoice_id": invoice.id }));
    // Notifications and hooks belong to the delivery that applied the write.
    assert!(notifications_for(&h.client, RECIPIENT).await.is_empty());
}

#[tokio::test]
async fn payment_failed_on_a_canceled_subscription_opens_no_dunning() {
    for ended in [SubscriptionStatus::Canceled, SubscriptionStatus::Unpaid] {
        let h = linked_harness().await;
        apply_event(&h.client, event("evt_sub", at(100), sub_changed(ended)))
            .await
            .unwrap();
        let outcome = apply_event(&h.client, event("evt_fail", at(200), invoice_failed(1)))
            .await
            .unwrap();
        assert!(matches!(outcome, ReconcileOutcome::Applied { .. }));
        let invoice = invoice(&h).await;
        assert_eq!(invoice.status, InvoiceStatus::Open);
        assert!(dunning(&h).await.is_none(), "{ended:?}: no dunning row");
        h.client.assert_no_jobs_enqueued();
        // The customer is still told (after the cancel notice, when there is one).
        let failed: Vec<_> = notifications_for(&h.client, RECIPIENT)
            .await
            .into_iter()
            .filter(|n| n.kind == "billing.payment_failed")
            .collect();
        assert_eq!(failed.len(), 1, "{ended:?}");
        assert_eq!(failed[0].payload["attempt"], json!(null));
    }
}

#[tokio::test]
async fn payment_failed_before_subscription_backfills_the_dunning_link() {
    let h = linked_harness().await;
    // The failure arrives first: the row has no subscription link.
    apply_event(&h.client, event("evt_fail", at(200), invoice_failed(1)))
        .await
        .unwrap();
    let row = dunning(&h).await.expect("dunning row");
    assert_eq!(row.subscription_id, None);
    apply_event(
        &h.client,
        event("evt_sub", at(100), sub_changed(SubscriptionStatus::PastDue)),
    )
    .await
    .unwrap();
    // The next failure event links the invoice and back-fills the row.
    apply_event(&h.client, event("evt_fail2", at(300), invoice_failed(2)))
        .await
        .unwrap();
    let sub = subscription(&h).await;
    assert_eq!(
        invoice(&h).await.subscription_id.as_deref(),
        Some(sub.id.as_str())
    );
    let row = dunning(&h).await.expect("dunning row");
    assert_eq!(row.subscription_id.as_deref(), Some(sub.id.as_str()));
    assert_eq!(row.attempt, 1);
    assert_eq!(row.state, DunningState::Pending);
}

#[tokio::test]
async fn dunning_disabled_notifies_without_a_job() {
    let billing = support::config().dunning(DunningPolicy::disabled());
    let h = harness_with_hooks(
        billing,
        Arc::new(autumn_billing::NoHooks),
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        |app| {
            app.with_clock(TickingClock::starting_at(support::base_time()))
                .routes(notification_routes())
        },
    );
    link_customer(&h, "cus_1").await;
    apply_event(
        &h.client,
        event("evt_sub", at(100), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    apply_event(&h.client, event("evt_fail", at(200), invoice_failed(1)))
        .await
        .unwrap();
    assert!(dunning(&h).await.is_none());
    h.client.assert_no_jobs_enqueued();
    let notes = notifications_for(&h.client, RECIPIENT).await;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, "billing.payment_failed");
}

/// Hooks that record every callback.
#[derive(Default)]
struct RecordingHooks {
    calls: Mutex<Vec<String>>,
    recipient: Option<i64>,
}

impl BillingHooks for RecordingHooks {
    fn recipient_for(&self, _user_id: &str) -> Option<i64> {
        self.recipient
    }

    fn on_subscription_changed<'a>(
        &'a self,
        subscription: &'a Subscription,
        previous: Option<&'a Subscription>,
    ) -> HookFuture<'a> {
        self.calls.lock().unwrap().push(format!(
            "subscription_changed:{}:{}",
            subscription.status.as_str(),
            previous.map_or("none", |p| p.status.as_str())
        ));
        Box::pin(async {})
    }

    fn on_payment_failed<'a>(
        &'a self,
        _invoice: &'a Invoice,
        dunning: &'a DunningAttempt,
    ) -> HookFuture<'a> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("payment_failed:{}", dunning.attempt));
        Box::pin(async {})
    }

    fn on_payment_recovered<'a>(&'a self, _invoice: &'a Invoice) -> HookFuture<'a> {
        self.calls
            .lock()
            .unwrap()
            .push("payment_recovered".to_owned());
        Box::pin(async {})
    }
}

#[tokio::test]
async fn hooks_fire_and_a_missing_recipient_skips_notifications() {
    let hooks = Arc::new(RecordingHooks::default());
    let h = harness_with_hooks(
        support::config(),
        hooks.clone(),
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        |app| {
            app.with_clock(TickingClock::starting_at(support::base_time()))
                .routes(notification_routes())
        },
    );
    link_customer(&h, "cus_1").await;
    apply_event(
        &h.client,
        event(
            "evt_sub",
            at(100),
            sub_changed(SubscriptionStatus::Trialing),
        ),
    )
    .await
    .unwrap();
    apply_event(
        &h.client,
        event("evt_sub2", at(150), sub_changed(SubscriptionStatus::Active)),
    )
    .await
    .unwrap();
    // Stale: no hook call.
    apply_event(
        &h.client,
        event("evt_old", at(120), sub_changed(SubscriptionStatus::PastDue)),
    )
    .await
    .unwrap();
    apply_event(&h.client, event("evt_fail", at(200), invoice_failed(1)))
        .await
        .unwrap();
    apply_event(&h.client, event("evt_paid", at(300), invoice_paid()))
        .await
        .unwrap();
    assert_eq!(
        *hooks.calls.lock().unwrap(),
        [
            "subscription_changed:trialing:none",
            "subscription_changed:active:trialing",
            "payment_failed:1",
            "payment_recovered",
        ]
    );
    // `recipient_for` returned `None`: nothing stored, nothing failed.
    assert!(notifications_for(&h.client, RECIPIENT).await.is_empty());
    assert_eq!(dunning(&h).await.unwrap().state, DunningState::Recovered);
}
