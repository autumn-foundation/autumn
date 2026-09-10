//! Stripe provider: fixture decoding and the HTTP client against
//! `TestApp::http_mock("stripe")`.

use autumn_billing::event::{BillingEvent, BillingEventKind};
use autumn_billing::money::{Currency, Money};
use autumn_billing::provider::{
    BillingProvider, CheckoutRequest, CustomerRequest, PaymentAttemptOutcome, PortalRequest,
};
use autumn_billing::{BillingError, InvoiceStatus, ProviderId, StripeProvider, SubscriptionStatus};
use autumn_web::test::{TestApp, TestClient};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::support::{self, TEST_SECRET_KEY, fixture};

/// `created` of the first batch of fixtures: 2026-09-10T00:00:00Z.
const T0: i64 = 1_788_998_400;
/// `created` of the follow-up fixtures: 2026-09-13T00:00:00Z.
const T3D: i64 = 1_789_257_600;
/// Period end of the test subscription: 2026-10-10T00:00:00Z.
const PERIOD_END: i64 = 1_791_590_400;

fn at(unix: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(unix, 0).unwrap()
}

fn parse(name: &str) -> BillingEvent {
    StripeProvider::parse_event_body(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn parse_value(value: &Value) -> Result<BillingEvent, BillingError> {
    StripeProvider::parse_event_body(&serde_json::to_vec(value).unwrap())
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_slice(&fixture(name)).unwrap()
}

fn usd(minor: i64) -> Money {
    Money::from_minor(minor, Currency::USD)
}

// ── Fixture decoding ────────────────────────────────────────────────────────

#[test]
fn checkout_session_completed_maps_checkout_snapshot() {
    let event = parse("checkout_session_completed");
    assert_eq!(event.id, "evt_test_checkout_1");
    assert_eq!(event.occurred_at, at(T0));
    let BillingEventKind::CheckoutCompleted(snapshot) = event.kind else {
        panic!("expected CheckoutCompleted, got {:?}", event.kind);
    };
    assert_eq!(snapshot.provider_customer_id, ProviderId::new("cus_test_1"));
    assert_eq!(
        snapshot.local_customer_ref.as_deref(),
        Some("local-customer-1")
    );
    assert_eq!(snapshot.email.as_deref(), Some("buyer@example.com"));
    assert_eq!(
        snapshot.provider_subscription_id,
        Some(ProviderId::new("sub_test_1"))
    );
}

#[test]
fn checkout_without_subscription_or_reference_is_still_valid() {
    let mut value = fixture_json("checkout_session_completed");
    value["data"]["object"]["subscription"] = Value::Null;
    value["data"]["object"]["client_reference_id"] = Value::Null;
    value["data"]["object"]["customer_details"] = Value::Null;
    let event = parse_value(&value).unwrap();
    let BillingEventKind::CheckoutCompleted(snapshot) = event.kind else {
        panic!("expected CheckoutCompleted");
    };
    assert_eq!(snapshot.provider_subscription_id, None);
    assert_eq!(snapshot.local_customer_ref, None);
    assert_eq!(snapshot.email, None);
}

#[test]
fn subscription_created_reads_period_end_from_items_new_api_shape() {
    let event = parse("customer_subscription_created");
    assert_eq!(event.id, "evt_test_sub_created_1");
    assert_eq!(event.occurred_at, at(T0));
    let BillingEventKind::SubscriptionChanged(snapshot) = event.kind else {
        panic!("expected SubscriptionChanged, got {:?}", event.kind);
    };
    assert_eq!(
        snapshot.provider_subscription_id,
        ProviderId::new("sub_test_1")
    );
    assert_eq!(snapshot.provider_customer_id, ProviderId::new("cus_test_1"));
    assert_eq!(
        snapshot.provider_price_id,
        Some(ProviderId::new("price_pro_monthly"))
    );
    assert_eq!(snapshot.status, SubscriptionStatus::Active);
    assert_eq!(snapshot.quantity, 1);
    assert_eq!(snapshot.current_period_end, Some(at(PERIOD_END)));
    assert!(!snapshot.cancel_at_period_end);
}

#[test]
fn subscription_updated_reads_top_level_period_end_old_api_shape() {
    let event = parse("customer_subscription_updated");
    assert_eq!(event.id, "evt_test_sub_updated_1");
    assert_eq!(event.occurred_at, at(T3D));
    let BillingEventKind::SubscriptionChanged(snapshot) = event.kind else {
        panic!("expected SubscriptionChanged, got {:?}", event.kind);
    };
    assert_eq!(snapshot.status, SubscriptionStatus::PastDue);
    assert_eq!(snapshot.quantity, 3);
    assert_eq!(snapshot.current_period_end, Some(at(PERIOD_END)));
    assert!(snapshot.cancel_at_period_end);
    assert_eq!(
        snapshot.provider_price_id,
        Some(ProviderId::new("price_pro_monthly"))
    );
}

#[test]
fn subscription_period_end_prefers_item_over_top_level() {
    let mut value = fixture_json("customer_subscription_updated");
    value["data"]["object"]["items"]["data"][0]["current_period_end"] = json!(PERIOD_END + 60);
    let event = parse_value(&value).unwrap();
    let BillingEventKind::SubscriptionChanged(snapshot) = event.kind else {
        panic!("expected SubscriptionChanged");
    };
    assert_eq!(snapshot.current_period_end, Some(at(PERIOD_END + 60)));
}

#[test]
fn subscription_without_items_or_period_end_has_no_price_and_no_end() {
    let mut value = fixture_json("customer_subscription_updated");
    value["data"]["object"]["items"]["data"] = json!([]);
    value["data"]["object"]["current_period_end"] = Value::Null;
    let event = parse_value(&value).unwrap();
    let BillingEventKind::SubscriptionChanged(snapshot) = event.kind else {
        panic!("expected SubscriptionChanged");
    };
    assert_eq!(snapshot.provider_price_id, None);
    assert_eq!(snapshot.current_period_end, None);
    assert_eq!(snapshot.quantity, 1, "quantity defaults to 1");
}

#[test]
fn subscription_status_mapping_covers_every_stripe_status() {
    let cases = [
        ("trialing", SubscriptionStatus::Trialing),
        ("active", SubscriptionStatus::Active),
        ("past_due", SubscriptionStatus::PastDue),
        ("unpaid", SubscriptionStatus::Unpaid),
        ("canceled", SubscriptionStatus::Canceled),
        ("incomplete", SubscriptionStatus::Incomplete),
        ("incomplete_expired", SubscriptionStatus::IncompleteExpired),
        ("paused", SubscriptionStatus::Paused),
    ];
    for (stripe, expected) in cases {
        let mut value = fixture_json("customer_subscription_updated");
        value["data"]["object"]["status"] = json!(stripe);
        let event = parse_value(&value).unwrap();
        let BillingEventKind::SubscriptionChanged(snapshot) = event.kind else {
            panic!("expected SubscriptionChanged for {stripe}");
        };
        assert_eq!(snapshot.status, expected, "status {stripe}");
    }
}

#[test]
fn unknown_subscription_status_is_malformed() {
    let mut value = fixture_json("customer_subscription_updated");
    value["data"]["object"]["status"] = json!("on_fire");
    let err = parse_value(&value).unwrap_err();
    assert!(matches!(err, BillingError::Malformed(_)), "{err:?}");
}

#[test]
fn subscription_deleted_maps_to_deleted_with_canceled_status() {
    let event = parse("customer_subscription_deleted");
    assert_eq!(event.id, "evt_test_sub_deleted_1");
    assert_eq!(event.occurred_at, at(PERIOD_END));
    let BillingEventKind::SubscriptionDeleted(snapshot) = event.kind else {
        panic!("expected SubscriptionDeleted, got {:?}", event.kind);
    };
    assert_eq!(
        snapshot.provider_subscription_id,
        ProviderId::new("sub_test_1")
    );
    assert_eq!(snapshot.provider_customer_id, ProviderId::new("cus_test_1"));
    assert_eq!(snapshot.status, SubscriptionStatus::Canceled);
    assert_eq!(snapshot.current_period_end, Some(at(PERIOD_END)));
}

#[test]
fn subscription_deleted_forces_canceled_even_when_object_says_active() {
    let mut value = fixture_json("customer_subscription_deleted");
    value["data"]["object"]["status"] = json!("active");
    let event = parse_value(&value).unwrap();
    let BillingEventKind::SubscriptionDeleted(snapshot) = event.kind else {
        panic!("expected SubscriptionDeleted");
    };
    assert_eq!(snapshot.status, SubscriptionStatus::Canceled);
}

#[test]
fn invoice_payment_failed_maps_money_and_new_parent_subscription_path() {
    let event = parse("invoice_payment_failed");
    assert_eq!(event.id, "evt_test_inv_failed_1");
    assert_eq!(event.occurred_at, at(T0));
    let BillingEventKind::InvoicePaymentFailed(snapshot) = event.kind else {
        panic!("expected InvoicePaymentFailed, got {:?}", event.kind);
    };
    assert_eq!(snapshot.provider_invoice_id, ProviderId::new("in_test_1"));
    assert_eq!(snapshot.provider_customer_id, ProviderId::new("cus_test_1"));
    assert_eq!(
        snapshot.provider_subscription_id,
        Some(ProviderId::new("sub_test_1")),
        "read from parent.subscription_details.subscription"
    );
    assert_eq!(snapshot.status, InvoiceStatus::Open);
    assert_eq!(snapshot.amount_due, usd(1999));
    assert_eq!(snapshot.amount_paid, usd(0));
    assert_eq!(snapshot.attempt_count, 1);
    assert_eq!(snapshot.next_payment_attempt, Some(at(T3D)));
}

#[test]
fn invoice_paid_maps_old_top_level_subscription_path() {
    let event = parse("invoice_paid");
    assert_eq!(event.id, "evt_test_inv_paid_1");
    assert_eq!(event.occurred_at, at(T3D));
    let BillingEventKind::InvoicePaid(snapshot) = event.kind else {
        panic!("expected InvoicePaid, got {:?}", event.kind);
    };
    assert_eq!(snapshot.provider_invoice_id, ProviderId::new("in_test_1"));
    assert_eq!(snapshot.provider_customer_id, ProviderId::new("cus_test_1"));
    assert_eq!(
        snapshot.provider_subscription_id,
        Some(ProviderId::new("sub_test_1")),
        "read from top-level subscription"
    );
    assert_eq!(snapshot.status, InvoiceStatus::Paid);
    assert_eq!(snapshot.amount_due, usd(1999));
    assert_eq!(snapshot.amount_paid, usd(1999));
    assert_eq!(snapshot.attempt_count, 2);
    assert_eq!(snapshot.next_payment_attempt, None);
}

#[test]
fn invoice_prefers_parent_path_when_both_shapes_are_present() {
    let mut value = fixture_json("invoice_paid");
    value["data"]["object"]["parent"] = json!({
        "type": "subscription_details",
        "subscription_details": { "subscription": "sub_from_parent" }
    });
    let event = parse_value(&value).unwrap();
    let BillingEventKind::InvoicePaid(snapshot) = event.kind else {
        panic!("expected InvoicePaid");
    };
    assert_eq!(
        snapshot.provider_subscription_id,
        Some(ProviderId::new("sub_from_parent"))
    );
}

#[test]
fn invoice_paid_falls_back_to_old_path_when_parent_has_no_subscription() {
    let mut value = fixture_json("invoice_paid");
    value["data"]["object"]["parent"] =
        json!({ "type": "quote_details", "subscription_details": null });
    let event = parse_value(&value).unwrap();
    let BillingEventKind::InvoicePaid(snapshot) = event.kind else {
        panic!("expected InvoicePaid");
    };
    assert_eq!(
        snapshot.provider_subscription_id,
        Some(ProviderId::new("sub_test_1"))
    );
}

#[test]
fn invoice_without_any_subscription_has_none() {
    let mut value = fixture_json("invoice_payment_failed");
    value["data"]["object"]["parent"] = Value::Null;
    let event = parse_value(&value).unwrap();
    let BillingEventKind::InvoicePaymentFailed(snapshot) = event.kind else {
        panic!("expected InvoicePaymentFailed");
    };
    assert_eq!(snapshot.provider_subscription_id, None);
}

#[test]
fn invoice_payment_succeeded_maps_to_invoice_paid() {
    let mut value = fixture_json("invoice_paid");
    value["type"] = json!("invoice.payment_succeeded");
    value["id"] = json!("evt_test_inv_succeeded_1");
    let event = parse_value(&value).unwrap();
    assert_eq!(event.id, "evt_test_inv_succeeded_1");
    assert!(
        matches!(event.kind, BillingEventKind::InvoicePaid(_)),
        "{:?}",
        event.kind
    );
}

#[test]
fn invoice_status_mapping_covers_every_stripe_status() {
    let cases = [
        ("draft", InvoiceStatus::Draft),
        ("open", InvoiceStatus::Open),
        ("paid", InvoiceStatus::Paid),
        ("uncollectible", InvoiceStatus::Uncollectible),
        ("void", InvoiceStatus::Void),
    ];
    for (stripe, expected) in cases {
        let mut value = fixture_json("invoice_paid");
        value["data"]["object"]["status"] = json!(stripe);
        let event = parse_value(&value).unwrap();
        let BillingEventKind::InvoicePaid(snapshot) = event.kind else {
            panic!("expected InvoicePaid for {stripe}");
        };
        assert_eq!(snapshot.status, expected, "status {stripe}");
    }
}

#[test]
fn invoice_currency_is_uppercased_and_minor_units_are_kept_verbatim() {
    let mut value = fixture_json("invoice_paid");
    value["data"]["object"]["currency"] = json!("jpy");
    value["data"]["object"]["amount_due"] = json!(500);
    value["data"]["object"]["amount_paid"] = json!(500);
    let event = parse_value(&value).unwrap();
    let BillingEventKind::InvoicePaid(snapshot) = event.kind else {
        panic!("expected InvoicePaid");
    };
    assert_eq!(snapshot.amount_due, Money::from_minor(500, Currency::JPY));
    assert_eq!(snapshot.amount_paid.currency(), Currency::JPY);
}

#[test]
fn invoice_with_invalid_currency_is_malformed() {
    let mut value = fixture_json("invoice_paid");
    value["data"]["object"]["currency"] = json!("dollars");
    let err = parse_value(&value).unwrap_err();
    assert!(matches!(err, BillingError::Malformed(_)), "{err:?}");
}

#[test]
fn charge_refunded_is_ignored_with_its_event_type() {
    let event = parse("charge_refunded");
    assert_eq!(event.id, "evt_test_charge_refunded_1");
    assert_eq!(event.occurred_at, at(T3D));
    assert_eq!(
        event.kind,
        BillingEventKind::Ignored {
            event_type: "charge.refunded".to_owned()
        }
    );
}

#[test]
fn unknown_type_with_garbage_object_is_still_ignored() {
    let value = json!({
        "id": "evt_test_unknown_1",
        "object": "event",
        "created": T0,
        "type": "payout.paid",
        "data": { "object": "not an object" }
    });
    let event = parse_value(&value).unwrap();
    assert_eq!(
        event.kind,
        BillingEventKind::Ignored {
            event_type: "payout.paid".to_owned()
        }
    );
}

#[test]
fn known_type_with_malformed_object_is_malformed_and_never_echoes_the_body() {
    const SENTINEL: &str = "BODY_MUST_NOT_LEAK_a1b2c3";
    let value = json!({
        "id": "evt_test_bad_1",
        "object": "event",
        "created": T0,
        "type": "invoice.paid",
        "data": { "object": { "id": SENTINEL, "customer": 42 } }
    });
    let err = parse_value(&value).unwrap_err();
    let BillingError::Malformed(message) = &err else {
        panic!("expected Malformed, got {err:?}");
    };
    assert!(!message.contains(SENTINEL), "message leaks body: {message}");
    assert!(!err.to_string().contains(SENTINEL));
}

#[test]
fn event_without_type_or_id_is_malformed() {
    let no_type = json!({ "id": "evt_x", "created": T0, "data": { "object": {} } });
    assert!(matches!(
        parse_value(&no_type),
        Err(BillingError::Malformed(_))
    ));
    let no_id = json!({ "type": "invoice.paid", "created": T0, "data": { "object": {} } });
    assert!(matches!(
        parse_value(&no_id),
        Err(BillingError::Malformed(_))
    ));
}

#[test]
fn body_that_is_not_json_is_malformed() {
    let err = StripeProvider::parse_event_body(b"<html>nope</html>").unwrap_err();
    assert!(matches!(err, BillingError::Malformed(_)), "{err:?}");
    assert!(!err.to_string().contains("<html>"));
}

#[test]
fn parse_event_on_the_trait_matches_parse_event_body() {
    let client = TestApp::new().build();
    let provider = provider_for(&client);
    let raw = fixture("invoice_paid");
    assert_eq!(
        provider.parse_event(&raw).unwrap(),
        StripeProvider::parse_event_body(&raw).unwrap()
    );
}

// ── HTTP client ─────────────────────────────────────────────────────────────

/// A provider whose `stripe` client is served by the app's mock registry.
fn provider_for(client: &TestClient) -> StripeProvider {
    StripeProvider::from_state(client.state(), &support::config().stripe).unwrap()
}

#[test]
fn new_rejects_a_missing_secret_key() {
    let config = autumn_billing::StripeConfig::default();
    let err = StripeProvider::new(config, autumn_web::http::Client::new()).unwrap_err();
    assert!(matches!(err, BillingError::Config(_)), "{err:?}");
}

#[tokio::test]
async fn create_customer_posts_to_customers_and_returns_id() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/customers")
        .respond_with(200, json!({ "id": "cus_test_1", "object": "customer" }));
    let client = app.build();
    let provider = provider_for(&client);

    let id = provider
        .create_customer(CustomerRequest::new("local-customer-1", "42").with_email("a@b.test"))
        .await
        .unwrap();

    assert_eq!(id, ProviderId::new("cus_test_1"));
    mock.expect_called(1);
}

#[tokio::test]
async fn create_checkout_posts_to_checkout_sessions_and_returns_hosted_session() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/checkout/sessions")
        .respond_with(
            200,
            json!({
                "id": "cs_test_1",
                "object": "checkout.session",
                "url": "https://checkout.stripe.com/c/pay/cs_test_1"
            }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let session = provider
        .create_checkout(
            CheckoutRequest::new(
                "cus_test_1",
                "local-customer-1",
                support::PRO_PRICE,
                "https://app.test/billing/success",
                "https://app.test/billing/cancel",
            )
            .with_quantity(2),
        )
        .await
        .unwrap();

    assert_eq!(session.id, ProviderId::new("cs_test_1"));
    assert_eq!(session.url, "https://checkout.stripe.com/c/pay/cs_test_1");
    mock.expect_called(1);
}

#[tokio::test]
async fn create_portal_posts_to_billing_portal_sessions() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/billing_portal/sessions")
        .respond_with(
            200,
            json!({
                "id": "bps_test_1",
                "object": "billing_portal.session",
                "url": "https://billing.stripe.com/session/bps_test_1"
            }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let session = provider
        .create_portal(PortalRequest::new("cus_test_1", "https://app.test/account"))
        .await
        .unwrap();

    assert_eq!(session.id, ProviderId::new("bps_test_1"));
    assert_eq!(session.url, "https://billing.stripe.com/session/bps_test_1");
    mock.expect_called(1);
}

#[tokio::test]
async fn create_customer_without_an_id_in_the_reply_is_a_provider_error() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/customers")
        .respond_with(200, json!({ "object": "customer" }));
    let client = app.build();
    let provider = provider_for(&client);

    let err = provider
        .create_customer(CustomerRequest::new("local-customer-1", "42"))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            BillingError::Provider {
                provider: "stripe",
                ..
            }
        ),
        "{err:?}"
    );
    mock.expect_called(1);
}

#[tokio::test]
async fn create_checkout_4xx_is_a_provider_error_without_the_secret() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/checkout/sessions")
        .respond_with(
            400,
            json!({ "error": { "type": "invalid_request_error", "code": "resource_missing",
                                "message": "No such price: 'price_pro_monthly'" } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let err = provider
        .create_checkout(CheckoutRequest::new(
            "cus_test_1",
            "local-customer-1",
            support::PRO_PRICE,
            "https://app.test/s",
            "https://app.test/c",
        ))
        .await
        .unwrap_err();

    let text = err.to_string();
    assert!(
        matches!(
            err,
            BillingError::Provider {
                provider: "stripe",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(text.contains("400"), "{text}");
    assert!(text.contains("resource_missing"), "{text}");
    assert!(!text.contains(TEST_SECRET_KEY), "{text}");
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_paid_reply_is_paid() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            200,
            json!({ "id": "in_test_1", "object": "invoice", "status": "paid" }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let outcome = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:1")
        .await
        .unwrap();

    assert_eq!(outcome, PaymentAttemptOutcome::Paid);
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_2xx_but_not_paid_is_declined() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            200,
            json!({ "id": "in_test_1", "object": "invoice", "status": "open" }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let outcome = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:1")
        .await
        .unwrap();

    assert!(
        matches!(outcome, PaymentAttemptOutcome::Declined { .. }),
        "{outcome:?}"
    );
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_card_error_is_declined_with_the_code() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            402,
            json!({ "error": { "type": "card_error", "code": "card_declined",
                                "decline_code": "insufficient_funds",
                                "message": "Your card was declined." } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let outcome = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:2")
        .await
        .unwrap();

    assert_eq!(
        outcome,
        PaymentAttemptOutcome::Declined {
            reason: "card_declined".to_owned()
        }
    );
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_requires_action_is_declined() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            400,
            json!({ "error": { "type": "invalid_request_error",
                                "code": "invoice_payment_intent_requires_action",
                                "message": "This payment requires authentication." } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let outcome = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:2")
        .await
        .unwrap();

    assert_eq!(
        outcome,
        PaymentAttemptOutcome::Declined {
            reason: "invoice_payment_intent_requires_action".to_owned()
        }
    );
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_other_4xx_without_code_is_declined_generic() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            400,
            json!({ "error": { "type": "invalid_request_error",
                                "message": "Something odd." } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let outcome = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:2")
        .await
        .unwrap();

    assert_eq!(
        outcome,
        PaymentAttemptOutcome::Declined {
            reason: "declined".to_owned()
        }
    );
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_already_paid_code_is_already_paid() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            400,
            json!({ "error": { "type": "invalid_request_error",
                                "code": "invoice_already_paid",
                                "message": "Invoice is already paid" } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let outcome = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:3")
        .await
        .unwrap();

    assert_eq!(outcome, PaymentAttemptOutcome::AlreadyPaid);
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_already_paid_message_without_code_is_already_paid() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            400,
            json!({ "error": { "type": "invalid_request_error",
                                "message": "This invoice is already paid." } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let outcome = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:3")
        .await
        .unwrap();

    assert_eq!(outcome, PaymentAttemptOutcome::AlreadyPaid);
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_500_is_a_provider_error_that_never_shows_the_secret() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .post("/v1/invoices/in_test_1/pay")
        .respond_with(
            500,
            json!({ "error": { "type": "api_error", "message": "Stripe is down" } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let err = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:1")
        .await
        .unwrap_err();

    let text = err.to_string();
    assert!(
        matches!(
            err,
            BillingError::Provider {
                provider: "stripe",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(text.contains("500"), "{text}");
    assert!(!text.contains(TEST_SECRET_KEY), "{text}");
    assert!(!format!("{err:?}").contains(TEST_SECRET_KEY));
    mock.expect_called(1);
}

#[tokio::test]
async fn retry_invoice_payment_transport_failure_is_a_provider_error() {
    // A mock registry with no matching entry fails the send: the transport path.
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .get("/v1/unrelated")
        .respond_with_status(200);
    let client = app.build();
    let provider = provider_for(&client);

    let err = provider
        .retry_invoice_payment(&ProviderId::new("in_test_1"), "autumn-billing:inv-1:1")
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            BillingError::Provider {
                provider: "stripe",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains(TEST_SECRET_KEY));
    mock.expect_called(0);
}

#[tokio::test]
async fn cancel_subscription_deletes_and_accepts_2xx() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .delete("/v1/subscriptions/sub_test_1")
        .respond_with(200, json!({ "id": "sub_test_1", "status": "canceled" }));
    let client = app.build();
    let provider = provider_for(&client);

    provider
        .cancel_subscription(&ProviderId::new("sub_test_1"))
        .await
        .unwrap();
    mock.expect_called(1);
}

#[tokio::test]
async fn cancel_subscription_treats_404_as_already_gone() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .delete("/v1/subscriptions/sub_test_1")
        .respond_with(
            404,
            json!({ "error": { "type": "invalid_request_error", "code": "resource_missing",
                                "message": "No such subscription" } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    provider
        .cancel_subscription(&ProviderId::new("sub_test_1"))
        .await
        .unwrap();
    mock.expect_called(1);
}

#[tokio::test]
async fn cancel_subscription_500_is_a_provider_error() {
    let mut app = TestApp::new();
    let mock = app
        .http_mock("stripe")
        .delete("/v1/subscriptions/sub_test_1")
        .respond_with(
            500,
            json!({ "error": { "type": "api_error", "message": "boom" } }),
        );
    let client = app.build();
    let provider = provider_for(&client);

    let err = provider
        .cancel_subscription(&ProviderId::new("sub_test_1"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            BillingError::Provider {
                provider: "stripe",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains(TEST_SECRET_KEY));
    mock.expect_called(1);
}

#[tokio::test]
async fn requests_go_to_the_configured_api_base() {
    // The mock matches on path only, so a request to the configured base is
    // the same as one to api.stripe.com; a different alias must not match.
    let mut app = TestApp::new();
    let other = app
        .http_mock("not-stripe")
        .post("/v1/customers")
        .respond_with(200, json!({ "id": "cus_wrong" }));
    let client = app.build();
    let provider = provider_for(&client);

    let err = provider
        .create_customer(CustomerRequest::new("local-customer-1", "42"))
        .await
        .unwrap_err();
    assert!(matches!(err, BillingError::Provider { .. }), "{err:?}");
    other.expect_called(0);
}

#[test]
fn debug_output_never_shows_the_secret_key() {
    let client = TestApp::new().build();
    let provider = provider_for(&client);
    let debug = format!("{provider:?}");
    assert!(!debug.contains(TEST_SECRET_KEY), "{debug}");
}
