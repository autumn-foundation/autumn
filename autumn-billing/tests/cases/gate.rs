//! Tests for the plan gate: `Entitled<R>` and `Billing`.
//!
//! The store is seeded directly. No webhook runs here.

use std::time::Duration;

use autumn_billing::SubscriptionStatus;
use autumn_billing::prelude::*;
use autumn_billing::store::{CustomerUpsert, SubscriptionUpsert};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::time::FixedClock;
use chrono::{DateTime, TimeZone, Utc};

use super::support::{self, FakeProvider, Harness, PRO_PRICE, TEAM_PRICE};

struct Pro;
impl PlanRequirement for Pro {
    fn rule() -> PlanRule {
        PlanRule::plan("pro")
    }
}

struct Sso;
impl PlanRequirement for Sso {
    fn rule() -> PlanRule {
        PlanRule::entitlement("sso")
    }
}

struct AnyPlan;
impl PlanRequirement for AnyPlan {
    fn rule() -> PlanRule {
        PlanRule::AnyActive
    }
}

#[get("/pro")]
async fn pro(_e: Entitled<Pro>) -> &'static str {
    "pro ok"
}

#[get("/sso")]
async fn sso(_e: Entitled<Sso>) -> &'static str {
    "sso ok"
}

#[get("/any")]
async fn any_plan(_e: Entitled<AnyPlan>) -> &'static str {
    "any ok"
}

/// The pinned app clock.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap()
}

const fn hours(n: i64) -> chrono::Duration {
    chrono::Duration::hours(n)
}

fn gate_routes(app: TestApp) -> TestApp {
    app.with_clock(FixedClock::at(now()))
        .routes(routes![pro, sso, any_plan])
}

fn build() -> Harness {
    support::harness(
        MemoryBillingStore::shared(),
        FakeProvider::new(),
        gate_routes,
    )
}

fn build_with(billing: BillingConfig) -> Harness {
    let autumn = support::autumn_config(&billing);
    support::harness_with(
        billing,
        autumn,
        MemoryBillingStore::shared(),
        FakeProvider::new(),
        gate_routes,
    )
}

async fn seed_customer(store: &MemoryBillingStore, user_id: &str) -> String {
    let upsert = CustomerUpsert::new(
        format!("cust-{user_id}"),
        "stripe",
        format!("cus_{user_id}"),
        now(),
    )
    .with_user(user_id);
    store.upsert_customer(upsert).await.expect("customer").id
}

fn sub(
    customer_id: &str,
    key: &str,
    price: Option<&str>,
    status: SubscriptionStatus,
    period_end: Option<DateTime<Utc>>,
    occurred_at: DateTime<Utc>,
) -> SubscriptionUpsert {
    let plan_id = match price {
        Some(PRO_PRICE) => Some(PlanId::new("pro")),
        Some(TEAM_PRICE) => Some(PlanId::new("team")),
        _ => None,
    };
    let mut upsert = SubscriptionUpsert::new(
        format!("sub-{customer_id}-{key}"),
        customer_id,
        format!("sub_{customer_id}_{key}"),
        status,
        occurred_at,
        now(),
    );
    if let Some(price) = price {
        upsert = upsert.with_price(price);
    }
    if let Some(plan_id) = plan_id {
        upsert = upsert.with_plan(plan_id);
    }
    if let Some(end) = period_end {
        upsert = upsert.with_period_end(end);
    }
    upsert
}

/// Seed one subscription for `user_id` on `price` with `status`, period end
/// one month out.
async fn seed(store: &MemoryBillingStore, user_id: &str, price: &str, status: SubscriptionStatus) {
    let customer = seed_customer(store, user_id).await;
    store
        .upsert_subscription(sub(
            &customer,
            "1",
            Some(price),
            status,
            Some(now() + hours(24 * 30)),
            now(),
        ))
        .await
        .expect("subscription");
}

#[tokio::test]
async fn anonymous_request_is_401() {
    let h = build();
    h.client.get("/pro").send().await.assert_status(401);
}

#[tokio::test]
async fn logged_in_without_subscription_is_403() {
    let h = build();
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);
    h.client.get("/any").send().await.assert_status(403);
}

#[tokio::test]
async fn active_pro_passes_the_gate() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::Active).await;
    h.client.acting_as("7").await;
    let resp = h.client.get("/pro").send().await;
    resp.assert_status(200);
    assert_eq!(resp.text(), "pro ok");
    h.client.get("/any").send().await.assert_status(200);
}

#[tokio::test]
async fn trialing_pro_passes_the_gate() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::Trialing).await;
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(200);
}

#[tokio::test]
async fn past_due_is_403_by_default() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::PastDue).await;
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);
}

#[tokio::test]
async fn past_due_passes_with_allow_past_due() {
    let h = build_with(support::config().allow_past_due(true));
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::PastDue).await;
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(200);
}

#[tokio::test]
async fn canceled_is_403() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::Canceled).await;
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);
    h.client.get("/any").send().await.assert_status(403);
}

#[tokio::test]
async fn unpaid_is_403() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::Unpaid).await;
    h.client.acting_as("7").await;
    h.client.get("/any").send().await.assert_status(403);
}

#[tokio::test]
async fn unknown_price_id_is_403() {
    let h = build();
    seed(&h.store, "7", "price_unknown", SubscriptionStatus::Active).await;
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);
    h.client.get("/any").send().await.assert_status(403);
}

#[tokio::test]
async fn team_plan_does_not_satisfy_pro_rule() {
    let h = build();
    seed(&h.store, "7", TEAM_PRICE, SubscriptionStatus::Active).await;
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);
    h.client.get("/any").send().await.assert_status(200);
}

#[tokio::test]
async fn period_end_plus_grace_in_the_past_is_403() {
    // Default grace is 72h: 73h ago is stale, 71h ago is still in grace.
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "stale",
            Some(PRO_PRICE),
            SubscriptionStatus::Active,
            Some(now() - hours(73)),
            now(),
        ))
        .await
        .unwrap();
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);

    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "grace",
            Some(PRO_PRICE),
            SubscriptionStatus::Active,
            Some(now() - hours(71)),
            now(),
        ))
        .await
        .unwrap();
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(200);
}

#[tokio::test]
async fn zero_grace_makes_expired_period_403() {
    let h = build_with(support::config().grace_period(Duration::ZERO));
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "1",
            Some(PRO_PRICE),
            SubscriptionStatus::Active,
            Some(now() - chrono::Duration::seconds(1)),
            now(),
        ))
        .await
        .unwrap();
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);
}

#[tokio::test]
async fn missing_period_end_uses_last_event_plus_grace() {
    // Default grace is 72h from the last event: 71h ago is entitled, 73h is not.
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "1",
            Some(PRO_PRICE),
            SubscriptionStatus::Active,
            None,
            now() - hours(71),
        ))
        .await
        .unwrap();
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(200);

    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "1",
            Some(PRO_PRICE),
            SubscriptionStatus::Active,
            None,
            now() - hours(73),
        ))
        .await
        .unwrap();
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(403);
}

#[tokio::test]
async fn active_row_beats_newer_incomplete_row() {
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "active",
            Some(PRO_PRICE),
            SubscriptionStatus::Active,
            Some(now() + hours(24)),
            now() - hours(2),
        ))
        .await
        .unwrap();
    // An abandoned second checkout: newer, live, not entitled.
    h.store
        .upsert_subscription(sub(
            &customer,
            "abandoned",
            Some(TEAM_PRICE),
            SubscriptionStatus::Incomplete,
            None,
            now() - hours(1),
        ))
        .await
        .unwrap();
    let billing = Billing::from_state(h.client.state()).expect("plugin started");
    let view = billing.current_subscription("7").await.unwrap().unwrap();
    assert_eq!(view.subscription.status, SubscriptionStatus::Active);
    assert_eq!(view.plan.as_ref().map(|p| p.id.as_str()), Some("pro"));
    assert!(view.entitled);
    h.client.acting_as("7").await;
    h.client.get("/pro").send().await.assert_status(200);
}

#[tokio::test]
async fn entitlement_rule_checks_plan_grants() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::Active).await;
    h.client.acting_as("7").await;
    h.client.get("/sso").send().await.assert_status(403);

    let h = build();
    seed(&h.store, "8", TEAM_PRICE, SubscriptionStatus::Active).await;
    h.client.acting_as("8").await;
    h.client.get("/sso").send().await.assert_status(200);
}

#[tokio::test]
async fn is_entitled_never_sees_another_users_subscription() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::Active).await;
    let billing = Billing::from_state(h.client.state()).expect("plugin started");

    assert!(
        billing
            .is_entitled("7", &PlanRule::AnyActive)
            .await
            .unwrap()
    );
    assert!(
        billing
            .is_entitled("7", &PlanRule::plan("pro"))
            .await
            .unwrap()
    );
    assert!(
        billing
            .is_entitled("7", &PlanRule::entitlement("export"))
            .await
            .unwrap()
    );
    assert!(
        !billing
            .is_entitled("8", &PlanRule::AnyActive)
            .await
            .unwrap()
    );
    assert!(
        !billing
            .is_entitled("8", &PlanRule::plan("pro"))
            .await
            .unwrap()
    );
    assert!(billing.current_subscription("8").await.unwrap().is_none());
}

#[tokio::test]
async fn require_returns_the_view_or_forbidden() {
    let h = build();
    seed(&h.store, "7", PRO_PRICE, SubscriptionStatus::Active).await;
    let billing = Billing::from_state(h.client.state()).expect("plugin started");

    let view = billing.require("7", &PlanRule::plan("pro")).await.unwrap();
    assert!(view.entitled);
    assert_eq!(view.plan.as_ref().map(|p| p.id.as_str()), Some("pro"));
    assert_eq!(view.subscription.status, SubscriptionStatus::Active);

    let err = billing
        .require("7", &PlanRule::plan("team"))
        .await
        .unwrap_err();
    assert!(matches!(err, BillingError::Forbidden(_)), "{err}");
    let err = billing
        .require("8", &PlanRule::AnyActive)
        .await
        .unwrap_err();
    assert!(matches!(err, BillingError::Forbidden(_)), "{err}");
}

#[tokio::test]
async fn current_subscription_prefers_a_live_row_over_a_newer_ended_one() {
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "old",
            Some(PRO_PRICE),
            SubscriptionStatus::Active,
            Some(now() + hours(24)),
            now() - hours(2),
        ))
        .await
        .unwrap();
    h.store
        .upsert_subscription(sub(
            &customer,
            "new",
            Some(TEAM_PRICE),
            SubscriptionStatus::Canceled,
            Some(now() + hours(24)),
            now() - hours(1),
        ))
        .await
        .unwrap();
    let billing = Billing::from_state(h.client.state()).expect("plugin started");
    let view = billing.current_subscription("7").await.unwrap().unwrap();
    assert_eq!(view.subscription.status, SubscriptionStatus::Active);
    assert_eq!(view.plan.as_ref().map(|p| p.id.as_str()), Some("pro"));
    assert!(view.entitled);
}

#[tokio::test]
async fn current_subscription_falls_back_to_the_newest_ended_row() {
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    h.store
        .upsert_subscription(sub(
            &customer,
            "a",
            Some(PRO_PRICE),
            SubscriptionStatus::Canceled,
            None,
            now() - hours(2),
        ))
        .await
        .unwrap();
    h.store
        .upsert_subscription(sub(
            &customer,
            "b",
            Some(TEAM_PRICE),
            SubscriptionStatus::Canceled,
            None,
            now() - hours(1),
        ))
        .await
        .unwrap();
    let billing = Billing::from_state(h.client.state()).expect("plugin started");
    let view = billing.current_subscription("7").await.unwrap().unwrap();
    assert_eq!(view.plan.as_ref().map(|p| p.id.as_str()), Some("team"));
    assert!(!view.entitled);
}

#[tokio::test]
async fn billing_extractor_is_503_without_the_plugin() {
    #[get("/billing-handle")]
    async fn handle(_b: Billing) -> &'static str {
        "ok"
    }
    let client = TestApp::new().routes(routes![handle]).build();
    client
        .get("/billing-handle")
        .send()
        .await
        .assert_status(503);
}
