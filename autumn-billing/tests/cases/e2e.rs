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
use autumn_billing::store::CustomerUpsert;
use autumn_billing::{ProviderId, SubscriptionStatus};
use autumn_web::notifications::Notifications;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::FixedClock;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::support::{self, FakeProvider, Harness, fixture};

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

    let second = support::post_webhook(&h.client, &body).await;
    match second.status.as_u16() {
        // `SignedWebhook` replay window.
        409 => {}
        // Ledger duplicate.
        200 => assert_eq!(second.json::<Value>()["outcome"], "duplicate"),
        other => panic!("second delivery: {other} {}", second.text()),
    }

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
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let armed = h
            .client
            .enqueued_jobs()
            .iter()
            .any(|job| job.name == RETRY_JOB_NAME);
        if armed {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "retry job was not re-armed within 2s: {:?}",
            h.client.enqueued_jobs()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        h.provider.retry_calls(),
        0,
        "re-arm schedules, never retries"
    );
}
