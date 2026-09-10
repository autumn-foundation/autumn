//! Tests for the plugin routes: checkout, portal, subscription, and the
//! webhook receiver's own status mapping. Reconcile behaviour lives in
//! `e2e.rs`.

use autumn_billing::prelude::*;
use autumn_billing::routes::route_infos;
use autumn_billing::store::{CustomerUpsert, SubscriptionUpsert};
use autumn_billing::{PortalRequest, ProviderId, SubscriptionStatus};
use autumn_web::test::TestApp;
use autumn_web::time::FixedClock;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};

use autumn_web::config::AutumnConfig;

use super::support::{
    self, FailingStore, FakeCall, FakeParser, FakeProvider, Harness, PRO_PRICE, fixture_with,
};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap()
}

fn pinned(app: TestApp) -> TestApp {
    app.with_clock(FixedClock::at(now()))
}

fn build() -> Harness {
    support::harness(MemoryBillingStore::shared(), FakeProvider::new(), pinned)
}

async fn seed_customer(store: &MemoryBillingStore, user_id: &str) -> String {
    let upsert = CustomerUpsert::new(
        format!("cust-{user_id}"),
        "fake",
        format!("cus_seeded_{user_id}"),
        now(),
    )
    .with_user(user_id);
    store.upsert_customer(upsert).await.expect("customer").id
}

async fn seed_subscription(
    store: &MemoryBillingStore,
    customer_id: &str,
    status: SubscriptionStatus,
) {
    store
        .upsert_subscription(
            SubscriptionUpsert::new(
                format!("sub-{customer_id}"),
                customer_id,
                format!("sub_{customer_id}"),
                status,
                now(),
                now(),
            )
            .with_price(PRO_PRICE)
            .with_plan("pro")
            .with_period_end(now() + chrono::Duration::days(30)),
        )
        .await
        .expect("subscription");
}

// ── checkout ────────────────────────────────────────────────────────────

#[tokio::test]
async fn checkout_requires_login() {
    let h = build();
    h.client
        .post("/billing/checkout")
        .json(&json!({ "plan": "pro" }))
        .send()
        .await
        .assert_status(401);
    assert!(h.provider.calls().is_empty());
}

#[tokio::test]
async fn checkout_unknown_plan_is_404() {
    let h = build();
    h.client.acting_as("7").await;
    h.client
        .post("/billing/checkout")
        .json(&json!({ "plan": "enterprise" }))
        .send()
        .await
        .assert_status(404);
    assert!(h.provider.calls().is_empty());
}

#[tokio::test]
async fn checkout_without_plan_is_400() {
    let h = build();
    h.client.acting_as("7").await;
    h.client
        .post("/billing/checkout")
        .form("quantity=1")
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn checkout_with_live_subscription_is_409() {
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    seed_subscription(&h.store, &customer, SubscriptionStatus::PastDue).await;
    h.client.acting_as("7").await;
    h.client
        .post("/billing/checkout")
        .form("plan=pro")
        .send()
        .await
        .assert_status(409);
    assert!(h.provider.calls().is_empty());
}

#[tokio::test]
async fn checkout_after_a_canceled_subscription_is_allowed() {
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    seed_subscription(&h.store, &customer, SubscriptionStatus::Canceled).await;
    h.client.acting_as("7").await;
    h.client
        .post("/billing/checkout")
        .form("plan=pro")
        .send()
        .await
        .assert_status(303);
    let checkouts: Vec<FakeCall> = h
        .provider
        .calls()
        .into_iter()
        .filter(|c| matches!(c, FakeCall::CreateCheckout(_)))
        .collect();
    assert_eq!(checkouts.len(), 1, "{checkouts:?}");
    let FakeCall::CreateCheckout(req) = &checkouts[0] else {
        unreachable!()
    };
    assert_eq!(req.provider_customer_id, ProviderId::new("cus_seeded_7"));
    assert_eq!(req.provider_price_id, ProviderId::new(PRO_PRICE));
}

#[tokio::test]
async fn checkout_after_an_incomplete_checkout_is_allowed() {
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    seed_subscription(&h.store, &customer, SubscriptionStatus::Incomplete).await;
    h.client.acting_as("7").await;
    h.client
        .post("/billing/checkout")
        .form("plan=pro")
        .send()
        .await
        .assert_status(303);
}

#[tokio::test]
async fn concurrent_checkouts_link_one_customer() {
    let inner = MemoryBillingStore::shared();
    // The decorator yields on every store call, so the two requests
    // interleave between the lookup and the link.
    let store = FailingStore::wrap(inner.clone());
    let h = support::harness_dyn(support::config(), store, FakeProvider::new(), pinned);
    h.client.acting_as("7").await;
    let (a, b) = tokio::join!(
        h.client.post("/billing/checkout").form("plan=pro").send(),
        h.client.post("/billing/checkout").form("plan=pro").send(),
    );
    for resp in [&a, &b] {
        assert!(
            matches!(resp.status.as_u16(), 303 | 409),
            "{} {}",
            resp.status,
            resp.text()
        );
    }
    // Exactly one mirrored customer carries user 7: whichever request linked
    // first. The other provider customer is an orphan, not a second link.
    let linked = inner
        .customer_by_user("7")
        .await
        .unwrap()
        .expect("one linked customer");
    let created = [ProviderId::new("cus_fake_1"), ProviderId::new("cus_fake_2")];
    assert!(created.contains(&linked.provider_customer_id));
    for id in &created {
        let mirrored = inner.customer_by_provider_id(id).await.unwrap();
        if *id == linked.provider_customer_id {
            assert_eq!(mirrored.as_ref(), Some(&linked));
        } else {
            assert!(mirrored.is_none(), "{id} must not be mirrored");
        }
    }
    let checkouts: Vec<ProviderId> = h
        .provider
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::CreateCheckout(req) => Some(req.provider_customer_id),
            _ => None,
        })
        .collect();
    assert!(!checkouts.is_empty());
    assert!(
        checkouts
            .iter()
            .all(|id| *id == linked.provider_customer_id),
        "every checkout uses the linked customer: {checkouts:?}"
    );
}

#[tokio::test]
async fn checkout_creates_customer_then_session_and_redirects() {
    let h = build();
    h.client.acting_as("7").await;
    let resp = h
        .client
        .post("/billing/checkout")
        .form("plan=pro")
        .send()
        .await;
    resp.assert_status(303);
    assert_eq!(
        resp.header("location"),
        Some("https://checkout.fake/cus_fake_1/price_pro_monthly")
    );

    let customer = h
        .store
        .customer_by_user("7")
        .await
        .unwrap()
        .expect("customer row linked to user 7");
    assert_eq!(customer.user_id.as_deref(), Some("7"));
    assert_eq!(customer.provider_customer_id, ProviderId::new("cus_fake_1"));
    assert_eq!(customer.provider, "fake");

    let calls = h.provider.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    match &calls[0] {
        FakeCall::CreateCustomer(req) => {
            assert_eq!(req.local_customer_id, customer.id);
            assert_eq!(req.user_id, "7");
        }
        other => panic!("first call: {other:?}"),
    }
    match &calls[1] {
        FakeCall::CreateCheckout(req) => {
            assert_eq!(req.provider_customer_id, ProviderId::new("cus_fake_1"));
            assert_eq!(req.local_customer_id, customer.id);
            assert_eq!(req.provider_price_id, ProviderId::new(PRO_PRICE));
            assert_eq!(req.quantity, 1);
            assert_eq!(req.success_url, "https://app.test/billing/success");
            assert_eq!(req.cancel_url, "https://app.test/billing/cancel");
        }
        other => panic!("second call: {other:?}"),
    }
}

#[tokio::test]
async fn second_checkout_reuses_the_customer() {
    let h = build();
    h.client.acting_as("7").await;
    for _ in 0..2 {
        h.client
            .post("/billing/checkout")
            .form("plan=team")
            .send()
            .await
            .assert_status(303);
    }
    let calls = h.provider.calls();
    let creates = calls
        .iter()
        .filter(|c| matches!(c, FakeCall::CreateCustomer(_)))
        .count();
    let checkouts: Vec<&ProviderId> = calls
        .iter()
        .filter_map(|c| match c {
            FakeCall::CreateCheckout(req) => Some(&req.provider_customer_id),
            _ => None,
        })
        .collect();
    assert_eq!(creates, 1);
    assert_eq!(checkouts, [&ProviderId::new("cus_fake_1"); 2]);
}

#[tokio::test]
async fn checkout_answers_json_when_accepted() {
    let h = build();
    h.client.acting_as("7").await;
    let resp = h
        .client
        .post("/billing/checkout")
        .header("accept", "application/json")
        .json(&json!({ "plan": "pro" }))
        .send()
        .await;
    resp.assert_status(200);
    let body: Value = resp.json();
    assert_eq!(
        body["url"],
        "https://checkout.fake/cus_fake_1/price_pro_monthly"
    );
    assert_eq!(body["id"], "cs_fake_1");
}

#[tokio::test]
async fn checkout_ignores_urls_in_the_body() {
    let h = build();
    h.client.acting_as("7").await;
    h.client
        .post("/billing/checkout")
        .json(&json!({
            "plan": "pro",
            "success_url": "https://evil.example/steal",
            "cancel_url": "https://evil.example/steal",
        }))
        .send()
        .await
        .assert_status(303);
    let Some(FakeCall::CreateCheckout(req)) = h.provider.calls().into_iter().nth(1) else {
        panic!("expected a checkout call: {:?}", h.provider.calls());
    };
    assert_eq!(req.success_url, "https://app.test/billing/success");
    assert_eq!(req.cancel_url, "https://app.test/billing/cancel");
}

// ── portal ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn portal_requires_login() {
    let h = build();
    h.client
        .post("/billing/portal")
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn portal_without_customer_is_404() {
    let h = build();
    h.client.acting_as("7").await;
    h.client
        .post("/billing/portal")
        .send()
        .await
        .assert_status(404);
    assert!(h.provider.calls().is_empty());
}

#[tokio::test]
async fn portal_redirects_to_the_hosted_session() {
    let h = build();
    seed_customer(&h.store, "7").await;
    h.client.acting_as("7").await;
    let resp = h.client.post("/billing/portal").send().await;
    resp.assert_status(303);
    assert_eq!(
        resp.header("location"),
        Some("https://portal.fake/cus_seeded_7")
    );
    assert_eq!(
        h.provider.calls(),
        vec![FakeCall::CreatePortal(PortalRequest::new(
            "cus_seeded_7",
            "https://app.test/account"
        ))]
    );
}

#[tokio::test]
async fn portal_answers_json_when_accepted() {
    let h = build();
    seed_customer(&h.store, "7").await;
    h.client.acting_as("7").await;
    let resp = h
        .client
        .post("/billing/portal")
        .header("accept", "application/json")
        .send()
        .await;
    resp.assert_status(200);
    let body: Value = resp.json();
    assert_eq!(body["url"], "https://portal.fake/cus_seeded_7");
    assert_eq!(body["id"], "bps_fake_1");
}

// ── subscription ────────────────────────────────────────────────────────

#[tokio::test]
async fn subscription_requires_login() {
    let h = build();
    h.client
        .get("/billing/subscription")
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn subscription_json_without_a_row() {
    let h = build();
    h.client.acting_as("7").await;
    let resp = h.client.get("/billing/subscription").send().await;
    resp.assert_status(200);
    assert_eq!(
        resp.json::<Value>(),
        json!({ "subscription": null, "plan": null, "entitled": false })
    );
}

#[tokio::test]
async fn subscription_json_with_an_active_row() {
    let h = build();
    let customer = seed_customer(&h.store, "7").await;
    seed_subscription(&h.store, &customer, SubscriptionStatus::Active).await;
    h.client.acting_as("7").await;
    let resp = h.client.get("/billing/subscription").send().await;
    resp.assert_status(200);
    let body: Value = resp.json();
    assert_eq!(body["entitled"], true);
    assert_eq!(body["subscription"]["status"], "active");
    assert_eq!(body["subscription"]["plan_id"], "pro");
    assert_eq!(body["subscription"]["provider_price_id"], PRO_PRICE);
    assert_eq!(body["plan"]["id"], "pro");
    assert_eq!(body["plan"]["entitlements"], json!(["export"]));
    assert!(h.provider.calls().is_empty(), "mirror only");
}

// ── webhook status mapping ──────────────────────────────────────────────

#[tokio::test]
async fn webhook_rejects_a_bad_signature_before_parsing() {
    let h = build();
    let body = br#"{"id":"evt_1","occurred_at":"2026-09-10T12:00:00Z","kind":{"type":"ignored","event_type":"x"}}"#;
    let resp = h
        .client
        .post("/billing/webhook")
        .header("stripe-signature", "t=1,v1=deadbeef")
        .header("content-type", "application/json")
        .body(body.to_vec())
        .send()
        .await;
    assert_eq!(resp.status.as_u16(), 401, "{}", resp.text());
    assert_eq!(h.store.applied_event_count().await.unwrap(), 0);
}

#[tokio::test]
async fn webhook_malformed_event_is_500_so_the_provider_redelivers() {
    let h = support::harness(
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        pinned,
    );
    // The delivery id is present, so `SignedWebhook` accepts the request;
    // the body is not a `BillingEvent`.
    let resp = support::post_webhook(&h.client, br#"{"id":"evt_bad_1","kind":"nope"}"#).await;
    assert_eq!(resp.status.as_u16(), 500, "{}", resp.text());
    assert_eq!(h.store.applied_event_count().await.unwrap(), 0);
}

// ── CSRF ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn csrf_exempts_the_webhook_but_not_checkout() {
    let billing = support::config();
    let mut autumn = support::autumn_config(&billing);
    autumn.security.csrf.enabled = true;
    let h = support::harness_with(
        billing,
        autumn,
        MemoryBillingStore::shared(),
        FakeProvider::with_parser(FakeParser::BillingEventJson),
        pinned,
    );

    h.client.acting_as("7").await;
    h.client
        .post("/billing/checkout")
        .form("plan=pro")
        .send()
        .await
        .assert_status(403);
    assert!(
        h.provider.calls().is_empty(),
        "CSRF blocked before the handler"
    );

    let body = br#"{"id":"evt_csrf_1","occurred_at":"2026-09-10T12:00:00Z","kind":{"type":"ignored","event_type":"charge.refunded"}}"#;
    let resp = support::post_webhook(&h.client, body).await;
    assert_ne!(resp.status.as_u16(), 403, "{}", resp.text());
    assert_ne!(resp.status.as_u16(), 401, "{}", resp.text());
}

// ── route listing ───────────────────────────────────────────────────────

#[test]
fn route_infos_match_the_mounted_paths() {
    let infos = route_infos(&support::config());
    let listed: Vec<(String, String)> = infos
        .iter()
        .map(|i| (i.method.clone(), i.path.clone()))
        .collect();
    assert_eq!(
        listed,
        [
            ("POST".to_owned(), "/billing/checkout".to_owned()),
            ("POST".to_owned(), "/billing/portal".to_owned()),
            ("GET".to_owned(), "/billing/subscription".to_owned()),
            ("POST".to_owned(), "/billing/webhook".to_owned()),
        ]
    );
    let prefixed = route_infos(&support::config().route_prefix("/pay/"));
    assert_eq!(prefixed[3].path, "/pay/webhook");
}

/// Config that passes production validation.
fn production_config() -> BillingConfig {
    support::config()
        .stripe_secret_key("sk_live_fake_key_for_tests")
        .stripe_webhook_secret(support::TEST_WEBHOOK_SECRET)
}

fn boot_panic_message(build: impl FnOnce() -> Harness) -> String {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
    let payload = result.err().expect("boot fails");
    payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .unwrap_or_default()
        })
}

#[tokio::test]
async fn production_refuses_the_memory_mirror_unless_allowed() {
    let billing = production_config();
    let autumn = support::autumn_config(&billing);
    let message = boot_panic_message(|| {
        // `config` replaces the whole config, so the profile comes after it.
        let app = TestApp::new().config(autumn).profile("prod").plugin(
            BillingPlugin::new()
                .config(billing)
                .plans(&support::catalog())
                .provider(FakeProvider::new()),
        );
        let client = pinned(app).build();
        Harness {
            client,
            store: MemoryBillingStore::shared(),
            provider: FakeProvider::new(),
        }
    });
    assert!(
        message.contains("allow_memory_store_in_production"),
        "{message}"
    );

    let billing = production_config().allow_memory_store_in_production(true);
    let autumn = support::autumn_config(&billing);
    let app = TestApp::new().config(autumn).profile("prod").plugin(
        BillingPlugin::new()
            .config(billing)
            .plans(&support::catalog())
            .provider(FakeProvider::new()),
    );
    let client = pinned(app).build();
    // Booted on the memory mirror.
    let service = autumn_billing::BillingService::require(client.state()).expect("plugin started");
    assert!(
        service
            .store()
            .customer_by_user("nobody")
            .await
            .unwrap()
            .is_none()
    );
    let resp = client.get("/billing/subscription").send().await;
    assert!(
        resp.status.as_u16() < 500,
        "{} {}",
        resp.status,
        resp.text()
    );
}

#[tokio::test]
async fn boot_fails_when_declared_endpoint_preset_differs_from_provider() {
    let billing = support::config();
    let mut autumn = support::autumn_config(&billing);
    let endpoint = autumn
        .security
        .webhooks
        .endpoints
        .first_mut()
        .expect("declared endpoint");
    endpoint.provider = autumn_web::webhook::WebhookProvider::Github;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        support::harness_with(
            billing,
            autumn,
            MemoryBillingStore::shared(),
            FakeProvider::new(),
            pinned,
        )
    }));
    let payload = result.err().expect("boot fails on a preset mismatch");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .unwrap_or_default()
        });
    // The payload carries the error's `Debug` form, so quotes are escaped.
    assert!(
        message.contains("declares provider = ") && message.contains("github"),
        "{message}"
    );
    assert!(
        message.contains("needs provider = ") && message.contains("stripe"),
        "{message}"
    );
}

#[tokio::test]
async fn boot_fails_when_webhook_endpoint_is_undeclared() {
    let billing = support::config();
    let autumn = AutumnConfig::default();
    assert!(autumn.security.webhooks.endpoints.is_empty());
    let message = boot_panic_message(|| {
        support::harness_with(
            billing,
            autumn,
            MemoryBillingStore::shared(),
            FakeProvider::new(),
            pinned,
        )
    });
    // The error prints the TOML entry to add.
    assert!(
        message.contains("no signed webhook endpoint is declared at /billing/webhook"),
        "{message}"
    );
    assert!(
        message.contains("[[security.webhooks.endpoints]]"),
        "{message}"
    );
    assert!(
        message.contains("path = ") && message.contains("/billing/webhook"),
        "{message}"
    );
    assert!(
        message.contains("provider = ") && message.contains("stripe"),
        "{message}"
    );
    assert!(message.contains("_WEBHOOK_SECRET"), "{message}");
}

#[tokio::test]
async fn boot_fails_in_production_with_a_test_key() {
    let billing = production_config().stripe_secret_key(support::TEST_SECRET_KEY);
    let autumn = support::autumn_config(&billing);
    let message = boot_panic_message(|| {
        let app = TestApp::new().config(autumn).profile("prod").plugin(
            BillingPlugin::new()
                .config(billing)
                .plans(&support::catalog())
                .provider(FakeProvider::new())
                .store(MemoryBillingStore::shared()),
        );
        let client = pinned(app).build();
        Harness {
            client,
            store: MemoryBillingStore::shared(),
            provider: FakeProvider::new(),
        }
    });
    assert!(message.contains("STRIPE_SECRET_KEY"), "{message}");
    assert!(message.contains("sk_test_"), "{message}");
    assert!(
        !message.contains(support::TEST_SECRET_KEY),
        "leaks the key: {message}"
    );
}

#[tokio::test]
async fn store_resolution_falls_back_to_memory() {
    // No `.store(..)` and no database pool: the plugin boots on the memory
    // mirror, and the routes read the same store the webhook writes.
    let billing = support::config();
    let provider = FakeProvider::new();
    let app = TestApp::new()
        .config(support::autumn_config(&billing))
        .plugin(
            BillingPlugin::new()
                .config(billing)
                .plans(&support::catalog())
                .provider(provider.clone()),
        );
    let client = pinned(app).build();

    client.acting_as("7").await;
    client
        .post("/billing/checkout")
        .form("plan=pro")
        .send()
        .await
        .assert_status(303);
    let resp = client.get("/billing/subscription").send().await;
    resp.assert_status(200);
    assert_eq!(resp.json::<Value>()["subscription"], Value::Null);

    let body = fixture_with("customer_subscription_created", |json| {
        json["data"]["object"]["customer"] = Value::from("cus_fake_1");
    });
    let resp = support::post_webhook(&client, &body).await;
    resp.assert_status(200);
    assert_eq!(resp.json::<Value>()["outcome"], "applied");

    let resp = client.get("/billing/subscription").send().await;
    resp.assert_status(200);
    let view: Value = resp.json();
    assert_eq!(view["entitled"], true, "{view}");
    assert_eq!(view["subscription"]["status"], "active");
    assert_eq!(
        view["subscription"]["provider_subscription_id"],
        "sub_test_1"
    );
    let service = autumn_billing::BillingService::require(client.state()).expect("plugin started");
    assert_eq!(service.store().applied_event_count().await.unwrap(), 1);
}
