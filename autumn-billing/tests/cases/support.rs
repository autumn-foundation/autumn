//! Shared test helpers: a fake provider, Stripe request signing, and a test
//! app wired with the plugin. Append new helpers at the end; never change
//! existing signatures.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use autumn_billing::prelude::*;
use autumn_billing::provider::{
    CheckoutRequest, CustomerRequest, HostedSession, PaymentAttemptOutcome, PortalRequest,
    ProviderFuture,
};
use autumn_billing::{BillingEvent, ProviderId, StripeProvider};
use autumn_web::config::AutumnConfig;
use autumn_web::test::{TestApp, TestClient};
use autumn_web::webhook::{WebhookConfig, WebhookEndpointConfig, hmac_sha256_hex};

/// Webhook signing secret used by every test app.
pub const TEST_WEBHOOK_SECRET: &str = "whsec_test_secret_at_least_32_bytes_long";
/// Fake Stripe API key.
pub const TEST_SECRET_KEY: &str = "sk_test_fake_key_for_tests";
/// Price id of the `pro` plan.
pub const PRO_PRICE: &str = "price_pro_monthly";
/// Price id of the `team` plan.
pub const TEAM_PRICE: &str = "price_team_monthly";

/// The catalog every test app uses: `pro` (grants `export`) and `team`
/// (grants `export` + `sso`).
pub fn catalog() -> PlanCatalog {
    PlanCatalog::new()
        .plan(
            Plan::new(
                "pro",
                "Pro",
                PRO_PRICE,
                Money::from_minor(1999, Currency::USD),
                BillingInterval::Month,
            )
            .entitlement("export"),
        )
        .plan(
            Plan::new(
                "team",
                "Team",
                TEAM_PRICE,
                Money::from_minor(4999, Currency::USD),
                BillingInterval::Month,
            )
            .entitlement("export")
            .entitlement("sso"),
        )
}

/// Config with test keys and short dunning delays (1h, 2h, 3h).
pub fn config() -> BillingConfig {
    BillingConfig::default()
        .stripe_secret_key(TEST_SECRET_KEY)
        .stripe_webhook_secret(TEST_WEBHOOK_SECRET)
        .urls(
            "https://app.test/billing/success",
            "https://app.test/billing/cancel",
            "https://app.test/account",
        )
        .dunning(DunningPolicy::standard().with_retry_delays(vec![
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(7200),
            std::time::Duration::from_secs(10_800),
        ]))
}

/// `AutumnConfig` with the billing webhook endpoint declared (so CSRF is
/// exempt on it) and CSRF left at its default.
pub fn autumn_config(billing: &BillingConfig) -> AutumnConfig {
    let endpoint = billing.webhook_endpoint().expect("webhook secret set");
    let mut config = AutumnConfig::default();
    config.security.webhooks = WebhookConfig {
        endpoints: vec![endpoint],
        ..Default::default()
    };
    config
}

/// One recorded outbound call on the [`FakeProvider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    CreateCustomer(CustomerRequest),
    CreateCheckout(CheckoutRequest),
    CreatePortal(PortalRequest),
    RetryInvoice {
        invoice: ProviderId,
        idempotency_key: String,
    },
    CancelSubscription(ProviderId),
}

/// How the fake decodes webhook bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeParser {
    /// Real Stripe-shaped JSON via `StripeProvider::parse_event_body`.
    Stripe,
    /// The body is a serialized `BillingEvent`.
    BillingEventJson,
}

/// A scripted provider that records every call.
pub struct FakeProvider {
    calls: Mutex<Vec<FakeCall>>,
    retry_outcomes: Mutex<VecDeque<Result<PaymentAttemptOutcome, BillingError>>>,
    cancel_outcomes: Mutex<VecDeque<Result<(), BillingError>>>,
    parser: FakeParser,
    next_customer: Mutex<u32>,
}

impl FakeProvider {
    /// Fake that decodes Stripe fixtures.
    pub fn new() -> Arc<Self> {
        Self::with_parser(FakeParser::Stripe)
    }

    /// Fake with a chosen decoder.
    pub fn with_parser(parser: FakeParser) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            retry_outcomes: Mutex::new(VecDeque::new()),
            cancel_outcomes: Mutex::new(VecDeque::new()),
            parser,
            next_customer: Mutex::new(0),
        })
    }

    /// Queue the outcome of the next `retry_invoice_payment` call.
    pub fn script_retry(&self, outcome: Result<PaymentAttemptOutcome, BillingError>) {
        self.retry_outcomes.lock().unwrap().push_back(outcome);
    }

    /// Queue the outcome of the next `cancel_subscription` call.
    pub fn script_cancel(&self, outcome: Result<(), BillingError>) {
        self.cancel_outcomes.lock().unwrap().push_back(outcome);
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<FakeCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Number of `retry_invoice_payment` calls.
    pub fn retry_calls(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, FakeCall::RetryInvoice { .. }))
            .count()
    }

    /// Number of `cancel_subscription` calls.
    pub fn cancel_calls(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, FakeCall::CancelSubscription(_)))
            .count()
    }

    fn record(&self, call: FakeCall) {
        self.calls.lock().unwrap().push(call);
    }
}

impl BillingProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn webhook_endpoint(
        &self,
        name: &str,
        path: &str,
    ) -> Result<WebhookEndpointConfig, BillingError> {
        Ok(WebhookEndpointConfig::stripe(
            name,
            path,
            TEST_WEBHOOK_SECRET,
        ))
    }

    fn create_customer(&self, request: CustomerRequest) -> ProviderFuture<'_, ProviderId> {
        self.record(FakeCall::CreateCustomer(request));
        let mut next = self.next_customer.lock().unwrap();
        *next += 1;
        let id = ProviderId::new(format!("cus_fake_{}", *next));
        Box::pin(async move { Ok(id) })
    }

    fn create_checkout(&self, request: CheckoutRequest) -> ProviderFuture<'_, HostedSession> {
        let session = HostedSession::new(
            "cs_fake_1",
            format!(
                "https://checkout.fake/{}/{}",
                request.provider_customer_id, request.provider_price_id
            ),
        );
        self.record(FakeCall::CreateCheckout(request));
        Box::pin(async move { Ok(session) })
    }

    fn create_portal(&self, request: PortalRequest) -> ProviderFuture<'_, HostedSession> {
        let session = HostedSession::new(
            "bps_fake_1",
            format!("https://portal.fake/{}", request.provider_customer_id),
        );
        self.record(FakeCall::CreatePortal(request));
        Box::pin(async move { Ok(session) })
    }

    fn parse_event(&self, raw: &[u8]) -> Result<BillingEvent, BillingError> {
        match self.parser {
            FakeParser::Stripe => StripeProvider::parse_event_body(raw),
            FakeParser::BillingEventJson => serde_json::from_slice(raw)
                .map_err(|e| BillingError::Malformed(format!("billing event json: {e}"))),
        }
    }

    fn retry_invoice_payment<'a>(
        &'a self,
        invoice: &'a ProviderId,
        idempotency_key: &'a str,
    ) -> ProviderFuture<'a, PaymentAttemptOutcome> {
        self.record(FakeCall::RetryInvoice {
            invoice: invoice.clone(),
            idempotency_key: idempotency_key.to_owned(),
        });
        let outcome = self
            .retry_outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(PaymentAttemptOutcome::Declined {
                reason: "card_declined".to_owned(),
            }));
        Box::pin(async move { outcome })
    }

    fn cancel_subscription<'a>(&'a self, subscription: &'a ProviderId) -> ProviderFuture<'a, ()> {
        self.record(FakeCall::CancelSubscription(subscription.clone()));
        let outcome = self
            .cancel_outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()));
        Box::pin(async move { outcome })
    }
}

/// `Stripe-Signature` header value for `body` at unix time `ts`.
pub fn stripe_signature(secret: &str, ts: i64, body: &[u8]) -> String {
    let mut signed = ts.to_string().into_bytes();
    signed.push(b'.');
    signed.extend_from_slice(body);
    format!("t={ts},v1={}", hmac_sha256_hex(secret.as_bytes(), &signed))
}

/// Real wall-clock unix seconds. `SignedWebhook` checks skew against the
/// real clock, not the app clock.
pub fn unix_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// Read a Stripe fixture from `tests/fixtures/stripe/<name>.json`.
pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/stripe/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Everything a test needs from a built app.
pub struct Harness {
    pub client: TestClient,
    pub store: Arc<MemoryBillingStore>,
    pub provider: Arc<FakeProvider>,
}

/// Build a test app with the plugin mounted on `store` and `provider`.
/// `customize` runs on the `TestApp` before `build` (add routes, a clock…).
pub fn harness(
    store: Arc<MemoryBillingStore>,
    provider: Arc<FakeProvider>,
    customize: impl FnOnce(TestApp) -> TestApp,
) -> Harness {
    let billing = config();
    let app = TestApp::new().config(autumn_config(&billing)).plugin(
        BillingPlugin::new()
            .config(billing)
            .plans(&catalog())
            .provider(provider.clone())
            .store(store.clone()),
    );
    let client = customize(app).build();
    Harness {
        client,
        store,
        provider,
    }
}

/// Post a signed webhook body to `/billing/webhook`.
pub async fn post_webhook(client: &TestClient, body: &[u8]) -> autumn_web::test::TestResponse {
    let sig = stripe_signature(TEST_WEBHOOK_SECRET, unix_now(), body);
    client
        .post("/billing/webhook")
        .header("stripe-signature", &sig)
        .header("content-type", "application/json")
        .body(body.to_vec())
        .send()
        .await
}
