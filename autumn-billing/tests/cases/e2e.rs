//! End-to-end: signed Stripe fixtures through `SignedWebhook`, the Stripe
//! parser, reconcile, the gate and dunning.
//!
//! Fixtures live in `tests/fixtures/stripe/` and use the ids `cus_test_1`,
//! `sub_test_1`, `price_pro_monthly` and `in_test_1`.

use std::sync::Arc;
use std::time::Duration;

use autumn_billing::dunning::RETRY_JOB_NAME;
use autumn_billing::model::{DunningState, InvoiceStatus};
use autumn_billing::prelude::*;
use autumn_billing::provider::PaymentAttemptOutcome;
use autumn_billing::store::CustomerUpsert;
use autumn_billing::{ProviderId, SubscriptionStatus};
use autumn_web::notifications::Notifications;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::{FixedClock, TickingClock};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::support::{
    self, FakeCall, FakeProvider, Harness, fixture, fixture_with, notification_kinds,
    notification_routes,
};

const USER: &str = "7";

struct Pro;
impl PlanRequirement for Pro {
    fn rule() -> PlanRule {
        PlanRule::plan("pro")
    }
}

#[get("/pro")]
async fn pro(_e: Entitled<Pro>) -> &'static str {
    "pro ok"
}

/// Unread in-app notifications for user 7.
#[get("/test/unread")]
async fn unread(notifications: Notifications) -> AutumnResult<Json<u64>> {
    let recipient: i64 = USER.parse().expect("numeric user id");
    Ok(Json(notifications.unread_count(recipient).await?))
}

/// The pinned app clock. Fixture period ends must be later than this minus
/// the 72h grace.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap()
}

fn e2e_app(store: Arc<MemoryBillingStore>) -> Harness {
    support::harness(store, FakeProvider::new(), |app: TestApp| {
        app.with_clock(FixedClock::at(now()))
            .routes(routes![pro, unread])
    })
}

/// Like [`e2e_app`] with a ticking clock (the retry job needs to advance) and
/// the notification test route.
fn e2e_app_ticking(store: Arc<MemoryBillingStore>) -> Harness {
    support::harness(store, FakeProvider::new(), |app: TestApp| {
        app.with_clock(TickingClock::starting_at(now()))
            .routes(routes![pro, unread])
            .routes(notification_routes())
    })
}

/// A customer `cus_test_1` linked to user 7, as checkout would create it.
async fn seed_linked_customer(store: &MemoryBillingStore) -> String {
    let upsert = CustomerUpsert::new("local-cust-1", "stripe", "cus_test_1", now()).with_user(USER);
    store.upsert_customer(upsert).await.expect("customer").id
}

async fn subscription_json(client: &TestClient) -> Value {
    let resp = client.get("/billing/subscription").send().await;
    resp.assert_status(200);
    resp.json()
}

async fn unread_count(client: &TestClient) -> u64 {
    let resp = client.get("/test/unread").send().await;
    resp.assert_status(200);
    resp.json()
}

#[tokio::test]
async fn subscription_created_webhook_activates_pro() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;

    let resp = support::post_webhook(&h.client, &fixture("customer_subscription_created")).await;
    resp.assert_status(200);
    let body: Value = resp.json();
    assert_eq!(body["accepted"], true);
    assert_eq!(body["outcome"], "applied");
    assert!(body["event_id"].is_string(), "{body}");

    h.client.acting_as(USER).await;
    let view = subscription_json(&h.client).await;
    assert_eq!(view["entitled"], true, "{view}");
    assert_eq!(view["subscription"]["status"], "active");
    assert_eq!(
        view["subscription"]["provider_subscription_id"],
        "sub_test_1"
    );
    assert_eq!(view["plan"]["id"], "pro");
    assert_eq!(h.store.applied_event_count().await.unwrap(), 1);
}

#[tokio::test]
async fn plan_gate_flips_off_customer_subscription_deleted_webhook() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    support::post_webhook(&h.client, &fixture("customer_subscription_created"))
        .await
        .assert_status(200);

    h.client.acting_as(USER).await;
    h.client.get("/pro").send().await.assert_status(200);

    support::post_webhook(&h.client, &fixture("customer_subscription_deleted"))
        .await
        .assert_status(200);

    h.client.get("/pro").send().await.assert_status(403);
    let row = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
        .await
        .unwrap()
        .expect("mirror row");
    assert_eq!(row.status, SubscriptionStatus::Canceled);
}

#[tokio::test]
async fn past_due_update_flips_the_gate_off_by_default() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    support::post_webhook(&h.client, &fixture("customer_subscription_created"))
        .await
        .assert_status(200);
    support::post_webhook(&h.client, &fixture("customer_subscription_updated"))
        .await
        .assert_status(200);

    h.client.acting_as(USER).await;
    h.client.get("/pro").send().await.assert_status(403);
    let view = subscription_json(&h.client).await;
    assert_eq!(view["subscription"]["status"], "past_due");
    assert_eq!(view["entitled"], false);
}

#[tokio::test]
async fn duplicate_delivery_is_applied_once() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    let body = fixture("customer_subscription_created");

    support::post_webhook(&h.client, &body)
        .await
        .assert_status(200);
    let before = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
        .await
        .unwrap()
        .expect("mirror row");

    // `SignedWebhook` rejects the delivery id inside its replay window.
    let second = support::post_webhook(&h.client, &body).await;
    assert_eq!(second.status.as_u16(), 409, "{}", second.text());

    assert_eq!(h.store.applied_event_count().await.unwrap(), 1);
    let after = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
        .await
        .unwrap()
        .expect("mirror row");
    assert_eq!(after, before);
}

#[tokio::test]
async fn payment_failed_opens_dunning_and_invoice_paid_recovers() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    support::post_webhook(&h.client, &fixture("customer_subscription_created"))
        .await
        .assert_status(200);
    h.client.acting_as(USER).await;
    assert_eq!(unread_count(&h.client).await, 0);

    support::post_webhook(&h.client, &fixture("invoice_payment_failed"))
        .await
        .assert_status(200);

    let invoice = h
        .store
        .invoice_by_provider_id(&ProviderId::new("in_test_1"))
        .await
        .unwrap()
        .expect("invoice mirrored");
    assert_eq!(invoice.status, InvoiceStatus::Open);
    let dunning = h
        .store
        .dunning_by_invoice(&invoice.id)
        .await
        .unwrap()
        .expect("dunning row");
    assert_eq!(dunning.state, DunningState::Pending);
    assert_eq!(dunning.attempt, 1);
    assert_eq!(dunning.next_attempt_at, now() + chrono::Duration::hours(1));
    h.client.assert_job_enqueued(RETRY_JOB_NAME);
    assert_eq!(
        unread_count(&h.client).await,
        1,
        "payment_failed notification"
    );
    assert_eq!(h.provider.retry_calls(), 0, "retry is not due yet");

    support::post_webhook(&h.client, &fixture("invoice_paid"))
        .await
        .assert_status(200);

    let invoice = h
        .store
        .invoice_by_provider_id(&ProviderId::new("in_test_1"))
        .await
        .unwrap()
        .expect("invoice mirrored");
    assert_eq!(invoice.status, InvoiceStatus::Paid);
    let dunning = h
        .store
        .dunning_by_invoice(&invoice.id)
        .await
        .unwrap()
        .expect("dunning row");
    assert_eq!(dunning.state, DunningState::Recovered);
    assert_eq!(
        unread_count(&h.client).await,
        2,
        "payment_recovered notification"
    );
}

#[tokio::test]
async fn tampered_body_is_rejected_without_a_mirror_change() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    let signed = fixture("customer_subscription_created");
    let tampered = String::from_utf8(signed.clone())
        .unwrap()
        .replace("price_pro_monthly", "price_team_monthly")
        .into_bytes();
    assert_ne!(signed, tampered, "the fixture names the pro price");

    let resp = support::post_webhook_signed_as(&h.client, &signed, &tampered).await;
    assert!(
        matches!(resp.status.as_u16(), 400 | 401),
        "{} {}",
        resp.status,
        resp.text()
    );
    assert_eq!(h.store.applied_event_count().await.unwrap(), 0);
    assert!(
        h.store
            .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
            .await
            .unwrap()
            .is_none()
    );
    h.client.acting_as(USER).await;
    h.client.get("/pro").send().await.assert_status(403);
}

#[tokio::test]
async fn charge_refunded_is_accepted_and_ignored() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    let resp = support::post_webhook(&h.client, &fixture("charge_refunded")).await;
    resp.assert_status(200);
    let body: Value = resp.json();
    assert_eq!(body["accepted"], true);
    assert_eq!(body["outcome"], "ignored");
}

#[tokio::test]
async fn restart_rearms_the_pending_retry_job() {
    let store = MemoryBillingStore::shared();
    {
        let h = e2e_app(store.clone());
        seed_linked_customer(&h.store).await;
        support::post_webhook(&h.client, &fixture("customer_subscription_created"))
            .await
            .assert_status(200);
        support::post_webhook(&h.client, &fixture("invoice_payment_failed"))
            .await
            .assert_status(200);
        h.client.assert_job_enqueued(RETRY_JOB_NAME);
    }
    let open = store.open_dunning().await.unwrap();
    assert_eq!(open.len(), 1, "one pending row survives the restart");

    // Second process on the same store: startup re-arms the job.
    let h = e2e_app(store);
    support::wait_until(support::RESTART_TIMEOUT, || async {
        h.client
            .enqueued_jobs()
            .iter()
            .any(|job| job.name == RETRY_JOB_NAME)
    })
    .await;
    assert_eq!(
        h.provider.retry_calls(),
        0,
        "re-arm schedules, never retries"
    );
}

#[tokio::test]
async fn payment_failed_then_declined_retries_exhaust_and_cancel() {
    let h = e2e_app_ticking(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    support::post_webhook(&h.client, &fixture("customer_subscription_created"))
        .await
        .assert_status(200);
    support::post_webhook(&h.client, &fixture("invoice_payment_failed"))
        .await
        .assert_status(200);
    h.client.acting_as(USER).await;
    h.client.get("/pro").send().await.assert_status(200);

    // Retries at 1h, 2h and 3h after the previous run; every one is declined.
    // The in-process worker may run a due job too; the row's compare-and-set
    // makes that a no-op, so the provider sees exactly three calls.
    for delay in [3601, 7200, 10_800] {
        h.provider.script_retry(Ok(PaymentAttemptOutcome::Declined {
            reason: "card_declined".to_owned(),
        }));
        h.client.advance_clock(Duration::from_secs(delay));
        h.client
            .perform_enqueued_jobs()
            .await
            .assert_all_succeeded();
    }

    assert_eq!(h.provider.retry_calls(), 3);
    h.client.get("/pro").send().await.assert_status(403);
    let view = subscription_json(&h.client).await;
    assert_eq!(view["subscription"]["status"], "unpaid", "{view}");
    assert_eq!(view["entitled"], false);
    let row = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
        .await
        .unwrap()
        .expect("mirror row");
    assert_eq!(row.status, SubscriptionStatus::Unpaid);
    let cancels: Vec<FakeCall> = h
        .provider
        .calls()
        .into_iter()
        .filter(|c| matches!(c, FakeCall::CancelSubscription(_)))
        .collect();
    assert_eq!(
        cancels,
        [FakeCall::CancelSubscription(ProviderId::new("sub_test_1"))]
    );
    let recipient: i64 = USER.parse().unwrap();
    let kinds = notification_kinds(&h.client, recipient).await;
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "billing.dunning_exhausted")
            .count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("billing.dunning_exhausted")
    );
}

#[tokio::test]
async fn checkout_then_checkout_completed_fixture_links_customer() {
    let h = e2e_app(MemoryBillingStore::shared());
    h.client.acting_as(USER).await;
    h.client
        .post("/billing/checkout")
        .form("plan=pro")
        .send()
        .await
        .assert_status(303);
    let created = h
        .store
        .customer_by_user(USER)
        .await
        .unwrap()
        .expect("checkout linked a customer");
    assert_eq!(created.provider_customer_id, ProviderId::new("cus_fake_1"));
    assert_eq!(created.email, None);

    // Stripe echoes the provider customer and `client_reference_id` back.
    let completed = fixture_with("checkout_session_completed", |json| {
        json["data"]["object"]["customer"] = Value::from("cus_fake_1");
        json["data"]["object"]["client_reference_id"] = Value::from(created.id.as_str());
    });
    let resp = support::post_webhook(&h.client, &completed).await;
    resp.assert_status(200);
    assert_eq!(resp.json::<Value>()["outcome"], "applied");
    let sub_created = fixture_with("customer_subscription_created", |json| {
        json["data"]["object"]["customer"] = Value::from("cus_fake_1");
    });
    support::post_webhook(&h.client, &sub_created)
        .await
        .assert_status(200);

    let linked = h
        .store
        .customer_by_user(USER)
        .await
        .unwrap()
        .expect("still linked");
    assert_eq!(linked.id, created.id, "the same row");
    assert_eq!(linked.provider_customer_id, ProviderId::new("cus_fake_1"));
    assert_eq!(linked.email.as_deref(), Some("buyer@example.com"));
    let view = subscription_json(&h.client).await;
    assert_eq!(view["entitled"], true, "{view}");
    assert_eq!(view["subscription"]["status"], "active");
    assert_eq!(
        view["subscription"]["provider_subscription_id"],
        "sub_test_1"
    );
    assert_eq!(view["subscription"]["customer_id"], created.id.as_str());
    h.client.get("/pro").send().await.assert_status(200);
}

#[tokio::test]
async fn ledger_dedupes_when_replay_protection_is_off() {
    let billing = support::config();
    let mut autumn = support::autumn_config(&billing);
    let endpoint = autumn
        .security
        .webhooks
        .endpoints
        .pop()
        .expect("declared endpoint");
    autumn.security.webhooks.endpoints = vec![endpoint.without_replay_protection()];
    let h = support::harness_with(
        billing,
        autumn,
        MemoryBillingStore::shared(),
        FakeProvider::new(),
        |app: TestApp| {
            app.with_clock(FixedClock::at(now()))
                .routes(routes![pro, unread])
        },
    );
    seed_linked_customer(&h.store).await;
    let body = fixture("customer_subscription_created");

    let first = support::post_webhook(&h.client, &body).await;
    first.assert_status(200);
    assert_eq!(first.json::<Value>()["outcome"], "applied");
    let before = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
        .await
        .unwrap()
        .expect("mirror row");

    // No replay window: the request reaches the ledger, which dedupes it.
    let second = support::post_webhook(&h.client, &body).await;
    second.assert_status(200);
    assert_eq!(second.json::<Value>()["outcome"], "duplicate");
    assert_eq!(h.store.applied_event_count().await.unwrap(), 1);
    let after = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
        .await
        .unwrap()
        .expect("mirror row");
    assert_eq!(after, before);
}

#[tokio::test]
async fn out_of_order_deleted_then_created_stays_canceled() {
    let h = e2e_app(MemoryBillingStore::shared());
    seed_linked_customer(&h.store).await;
    support::post_webhook(&h.client, &fixture("customer_subscription_deleted"))
        .await
        .assert_status(200);
    let late = support::post_webhook(&h.client, &fixture("customer_subscription_created")).await;
    late.assert_status(200);
    assert_eq!(late.json::<Value>()["outcome"], "applied");

    let row = h
        .store
        .subscription_by_provider_id(&ProviderId::new("sub_test_1"))
        .await
        .unwrap()
        .expect("mirror row");
    assert_eq!(row.status, SubscriptionStatus::Canceled);
    assert_eq!(h.store.applied_event_count().await.unwrap(), 2);
    h.client.acting_as(USER).await;
    h.client.get("/pro").send().await.assert_status(403);
    let view = subscription_json(&h.client).await;
    assert_eq!(view["subscription"]["status"], "canceled", "{view}");
    assert_eq!(view["entitled"], false);
}
