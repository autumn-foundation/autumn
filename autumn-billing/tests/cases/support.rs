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
        let n = {
            let mut next = self.next_customer.lock().unwrap();
            *next += 1;
            *next
        };
        let id = ProviderId::new(format!("cus_fake_{n}"));
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
            .unwrap_or_else(|| {
                Ok(PaymentAttemptOutcome::Declined {
                    reason: "card_declined".to_owned(),
                })
            });
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

/// Like [`harness`], with an explicit billing config and `AutumnConfig`
/// (for `allow_past_due`, a different grace period, or CSRF on).
pub fn harness_with(
    billing: BillingConfig,
    autumn: AutumnConfig,
    store: Arc<MemoryBillingStore>,
    provider: Arc<FakeProvider>,
    customize: impl FnOnce(TestApp) -> TestApp,
) -> Harness {
    let app = TestApp::new().config(autumn).plugin(
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

/// Post `body` to `/billing/webhook` with a signature computed over
/// `signed_body`. Equal inputs make a valid delivery; different inputs model
/// a tampered body.
pub async fn post_webhook_signed_as(
    client: &TestClient,
    signed_body: &[u8],
    body: &[u8],
) -> autumn_web::test::TestResponse {
    let sig = stripe_signature(TEST_WEBHOOK_SECRET, unix_now(), signed_body);
    client
        .post("/billing/webhook")
        .header("stripe-signature", &sig)
        .header("content-type", "application/json")
        .body(body.to_vec())
        .send()
        .await
}

// ── WP-D1 helpers (reconcile / dunning) ───────────────────────────────────

/// Fixed base instant for event times: 2026-01-01T00:00:00Z.
pub fn base_time() -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

/// `base_time()` plus `secs`.
pub fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
    base_time() + chrono::Duration::seconds(secs)
}

/// Like [`harness`], but with a custom config and hooks.
pub fn harness_with_hooks(
    billing: BillingConfig,
    hooks: Arc<dyn BillingHooks>,
    store: Arc<MemoryBillingStore>,
    provider: Arc<FakeProvider>,
    customize: impl FnOnce(TestApp) -> TestApp,
) -> Harness {
    let app = TestApp::new().config(autumn_config(&billing)).plugin(
        BillingPlugin::new()
            .config(billing)
            .plans(&catalog())
            .provider(provider.clone())
            .store(store.clone())
            .hooks(hooks),
    );
    let client = customize(app).build();
    Harness {
        client,
        store,
        provider,
    }
}

/// Build a `BillingEvent`.
pub fn event(
    id: &str,
    occurred_at: chrono::DateTime<chrono::Utc>,
    kind: autumn_billing::BillingEventKind,
) -> BillingEvent {
    BillingEvent::new(id, occurred_at, kind)
}

/// A `checkout_completed` kind. Built through serde because the snapshot is
/// `#[non_exhaustive]`.
pub fn checkout_kind(
    provider_customer_id: &str,
    local_customer_ref: Option<&str>,
    email: Option<&str>,
    provider_subscription_id: Option<&str>,
) -> autumn_billing::BillingEventKind {
    serde_json::from_value(serde_json::json!({
        "type": "checkout_completed",
        "provider_customer_id": provider_customer_id,
        "local_customer_ref": local_customer_ref,
        "email": email,
        "provider_subscription_id": provider_subscription_id,
    }))
    .expect("checkout kind json")
}

/// Run `reconcile::apply` against the plugin service installed on `client`.
pub async fn apply_event(
    client: &TestClient,
    event: BillingEvent,
) -> Result<autumn_billing::ReconcileOutcome, BillingError> {
    let service = autumn_billing::BillingService::require(client.state()).expect("plugin started");
    autumn_billing::reconcile::apply(client.state(), &service, event).await
}

/// Test route: the notification feed of `recipient`, read through the
/// `Notifications` extractor so handlers and the plugin share one store.
#[autumn_web::get("/_test/notifications/{recipient}")]
pub async fn list_notifications_route(
    notifications: autumn_web::notifications::Notifications,
    autumn_web::Path(recipient): autumn_web::Path<i64>,
) -> autumn_web::AutumnResult<autumn_web::Json<Vec<autumn_web::notifications::Notification>>> {
    let page = notifications
        .list(
            recipient,
            &autumn_web::pagination::ListQuery::default(),
            &autumn_web::pagination::PageRequest::default(),
        )
        .await?;
    Ok(autumn_web::Json(page.content))
}

/// Routes to mount with `app.routes(notification_routes())`.
pub fn notification_routes() -> Vec<autumn_web::Route> {
    autumn_web::routes![list_notifications_route]
}

/// Read the notification feed of `recipient` through the test route.
pub async fn notifications_for(
    client: &TestClient,
    recipient: i64,
) -> Vec<autumn_web::notifications::Notification> {
    let response = client
        .get(&format!("/_test/notifications/{recipient}"))
        .send()
        .await;
    response.assert_status(200);
    response.json()
}

/// Kinds of the notifications of `recipient`, oldest first.
pub async fn notification_kinds(client: &TestClient, recipient: i64) -> Vec<String> {
    let mut items = notifications_for(client, recipient).await;
    items.sort_by_key(|n| n.id);
    items.into_iter().map(|n| n.kind).collect()
}

// ── Fix round 2 helpers (failure injection) ───────────────────────────────

/// Boxed hook run before a store call.
pub type BeforeHook =
    Box<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// A [`BillingStore`] decorator that delegates to `inner`, errors on the
/// N-th call of a named method, and can run a hook before the N-th call of
/// a named method (to model a write that races the call). Every delegated
/// call yields first, so two requests on one runtime interleave.
pub struct FailingStore {
    inner: Arc<dyn BillingStore>,
    failures: Mutex<Vec<(String, usize)>>,
    hooks: Mutex<Vec<(String, usize, BeforeHook)>>,
    calls: Mutex<std::collections::HashMap<String, usize>>,
}

impl FailingStore {
    /// Wrap `inner`.
    pub fn wrap(inner: Arc<dyn BillingStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            failures: Mutex::new(Vec::new()),
            hooks: Mutex::new(Vec::new()),
            calls: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Error on the `nth` (1-based) call of `method`.
    pub fn fail_on(&self, method: &str, nth: usize) {
        self.failures.lock().unwrap().push((method.to_owned(), nth));
    }

    /// Run `hook` before the `nth` (1-based) call of `method`.
    pub fn before(&self, method: &str, nth: usize, hook: BeforeHook) {
        self.hooks
            .lock()
            .unwrap()
            .push((method.to_owned(), nth, hook));
    }

    /// Number of calls of `method` so far.
    pub fn calls(&self, method: &str) -> usize {
        self.calls.lock().unwrap().get(method).copied().unwrap_or(0)
    }

    async fn enter(&self, method: &str) -> Result<(), BillingError> {
        tokio::task::yield_now().await;
        let n = {
            let mut calls = self.calls.lock().unwrap();
            *calls
                .entry(method.to_owned())
                .and_modify(|n| *n += 1)
                .or_insert(1)
        };
        let hook = self
            .hooks
            .lock()
            .unwrap()
            .iter()
            .find(|(m, nth, _)| m == method && *nth == n)
            .map(|(_, _, hook)| hook());
        if let Some(hook) = hook {
            hook.await;
        }
        let fails = self
            .failures
            .lock()
            .unwrap()
            .iter()
            .any(|(m, nth)| m == method && *nth == n);
        if fails {
            return Err(BillingError::store(format!(
                "injected failure: {method} call {n}"
            )));
        }
        Ok(())
    }
}

macro_rules! delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        Box::pin(async move {
            $self.enter(stringify!($method)).await?;
            $self.inner.$method($($arg),*).await
        })
    };
}

impl BillingStore for FailingStore {
    fn claim_event<'a>(
        &'a self,
        event_id: &'a str,
        kind: &'a str,
        now: chrono::DateTime<chrono::Utc>,
        stale_after: std::time::Duration,
    ) -> autumn_billing::store::StoreFuture<'a, autumn_billing::store::EventClaim> {
        delegate!(self, claim_event, event_id, kind, now, stale_after)
    }

    fn finish_event<'a>(
        &'a self,
        event_id: &'a str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> autumn_billing::store::StoreFuture<'a, ()> {
        delegate!(self, finish_event, event_id, now)
    }

    fn release_event<'a>(
        &'a self,
        event_id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, ()> {
        delegate!(self, release_event, event_id)
    }

    fn applied_event_count(&self) -> autumn_billing::store::StoreFuture<'_, u64> {
        delegate!(self, applied_event_count)
    }

    fn upsert_customer(
        &self,
        upsert: autumn_billing::store::CustomerUpsert,
    ) -> autumn_billing::store::StoreFuture<'_, autumn_billing::Customer> {
        delegate!(self, upsert_customer, upsert)
    }

    fn customer_by_id<'a>(
        &'a self,
        id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, Option<autumn_billing::Customer>> {
        delegate!(self, customer_by_id, id)
    }

    fn customer_by_user<'a>(
        &'a self,
        user_id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, Option<autumn_billing::Customer>> {
        delegate!(self, customer_by_user, user_id)
    }

    fn customer_by_provider_id<'a>(
        &'a self,
        provider_customer_id: &'a ProviderId,
    ) -> autumn_billing::store::StoreFuture<'a, Option<autumn_billing::Customer>> {
        delegate!(self, customer_by_provider_id, provider_customer_id)
    }

    fn upsert_subscription(
        &self,
        upsert: autumn_billing::store::SubscriptionUpsert,
    ) -> autumn_billing::store::StoreFuture<'_, autumn_billing::store::Write<Subscription>> {
        delegate!(self, upsert_subscription, upsert)
    }

    fn subscription_by_id<'a>(
        &'a self,
        id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, Option<Subscription>> {
        delegate!(self, subscription_by_id, id)
    }

    fn subscription_by_provider_id<'a>(
        &'a self,
        provider_subscription_id: &'a ProviderId,
    ) -> autumn_billing::store::StoreFuture<'a, Option<Subscription>> {
        delegate!(self, subscription_by_provider_id, provider_subscription_id)
    }

    fn subscriptions_for_customer<'a>(
        &'a self,
        customer_id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, Vec<Subscription>> {
        delegate!(self, subscriptions_for_customer, customer_id)
    }

    fn set_subscription_status<'a>(
        &'a self,
        id: &'a str,
        status: SubscriptionStatus,
        now: chrono::DateTime<chrono::Utc>,
    ) -> autumn_billing::store::StoreFuture<'a, Option<Subscription>> {
        delegate!(self, set_subscription_status, id, status, now)
    }

    fn upsert_invoice(
        &self,
        upsert: autumn_billing::store::InvoiceUpsert,
    ) -> autumn_billing::store::StoreFuture<'_, autumn_billing::store::Write<autumn_billing::Invoice>>
    {
        delegate!(self, upsert_invoice, upsert)
    }

    fn invoice_by_id<'a>(
        &'a self,
        id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, Option<autumn_billing::Invoice>> {
        delegate!(self, invoice_by_id, id)
    }

    fn invoice_by_provider_id<'a>(
        &'a self,
        provider_invoice_id: &'a ProviderId,
    ) -> autumn_billing::store::StoreFuture<'a, Option<autumn_billing::Invoice>> {
        delegate!(self, invoice_by_provider_id, provider_invoice_id)
    }

    fn upsert_dunning(
        &self,
        attempt: autumn_billing::DunningAttempt,
    ) -> autumn_billing::store::StoreFuture<'_, ()> {
        delegate!(self, upsert_dunning, attempt)
    }

    fn dunning_by_invoice<'a>(
        &'a self,
        invoice_id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, Option<autumn_billing::DunningAttempt>> {
        delegate!(self, dunning_by_invoice, invoice_id)
    }

    fn claim_dunning_attempt<'a>(
        &'a self,
        invoice_id: &'a str,
        attempt: i64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> autumn_billing::store::StoreFuture<'a, bool> {
        delegate!(self, claim_dunning_attempt, invoice_id, attempt, now)
    }

    fn open_dunning(
        &self,
    ) -> autumn_billing::store::StoreFuture<'_, Vec<autumn_billing::DunningAttempt>> {
        delegate!(self, open_dunning)
    }

    fn open_dunning_for_subscription<'a>(
        &'a self,
        subscription_id: &'a str,
    ) -> autumn_billing::store::StoreFuture<'a, Vec<autumn_billing::DunningAttempt>> {
        delegate!(self, open_dunning_for_subscription, subscription_id)
    }

    fn settle_dunning<'a>(
        &'a self,
        invoice_id: &'a str,
        expected_attempt: i64,
        from: &'a [autumn_billing::DunningState],
        row: autumn_billing::DunningAttempt,
    ) -> autumn_billing::store::StoreFuture<'a, bool> {
        delegate!(
            self,
            settle_dunning,
            invoice_id,
            expected_attempt,
            from,
            row
        )
    }

    fn prune_events(
        &self,
        before: chrono::DateTime<chrono::Utc>,
    ) -> autumn_billing::store::StoreFuture<'_, u64> {
        delegate!(self, prune_events, before)
    }
}

/// A built app over any store (a decorated one, for failure injection).
pub struct DynHarness {
    pub client: TestClient,
    pub store: Arc<dyn BillingStore>,
    pub provider: Arc<FakeProvider>,
}

/// Like [`harness_with_hooks`] with `NoHooks`, over any store.
pub fn harness_dyn(
    billing: BillingConfig,
    store: Arc<dyn BillingStore>,
    provider: Arc<FakeProvider>,
    customize: impl FnOnce(TestApp) -> TestApp,
) -> DynHarness {
    let app = TestApp::new().config(autumn_config(&billing)).plugin(
        BillingPlugin::new()
            .config(billing)
            .plans(&catalog())
            .provider(provider.clone())
            .store(store.clone()),
    );
    let client = customize(app).build();
    DynHarness {
        client,
        store,
        provider,
    }
}

// ── Test-gap round helpers ────────────────────────────────────────────────

/// Poll `check` every 25 ms until it is `true`, for at most `timeout` of
/// real time. The only place the suite waits on the wall clock: the job
/// runtime and the startup re-arm run on their own tasks.
pub async fn wait_until<F, Fut>(timeout: std::time::Duration, check: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "condition not met within {timeout:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Real-clock budget for [`wait_until`] on a process restart.
pub const RESTART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A Stripe fixture with `patch` applied to its JSON before serialization.
pub fn fixture_with(name: &str, patch: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut json: serde_json::Value =
        serde_json::from_slice(&fixture(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    patch(&mut json);
    serde_json::to_vec(&json).expect("serialize fixture")
}

/// Hooks that record every callback as `"<name>:<detail>"`.
#[derive(Default)]
pub struct RecordingHooks {
    calls: Mutex<Vec<String>>,
    recipient: Option<i64>,
}

impl RecordingHooks {
    /// Hooks whose `recipient_for` returns `None`: no notifications.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Hooks that route every notification to `recipient`.
    pub fn with_recipient(recipient: i64) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            recipient: Some(recipient),
        })
    }

    /// Every callback so far, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl BillingHooks for RecordingHooks {
    fn recipient_for(&self, _user_id: &str) -> Option<i64> {
        self.recipient
    }

    fn on_subscription_changed<'a>(
        &'a self,
        subscription: &'a Subscription,
        previous: Option<&'a Subscription>,
    ) -> autumn_billing::hooks::HookFuture<'a> {
        self.record(format!(
            "subscription_changed:{}:{}",
            subscription.status.as_str(),
            previous.map_or("none", |p| p.status.as_str())
        ));
        Box::pin(async {})
    }

    fn on_payment_failed<'a>(
        &'a self,
        _invoice: &'a autumn_billing::Invoice,
        dunning: &'a autumn_billing::DunningAttempt,
    ) -> autumn_billing::hooks::HookFuture<'a> {
        self.record(format!("payment_failed:{}", dunning.attempt));
        Box::pin(async {})
    }

    fn on_payment_recovered<'a>(
        &'a self,
        _invoice: &'a autumn_billing::Invoice,
    ) -> autumn_billing::hooks::HookFuture<'a> {
        self.record("payment_recovered".to_owned());
        Box::pin(async {})
    }

    fn on_dunning_exhausted<'a>(
        &'a self,
        _invoice: &'a autumn_billing::Invoice,
        dunning: &'a autumn_billing::DunningAttempt,
    ) -> autumn_billing::hooks::HookFuture<'a> {
        self.record(format!("dunning_exhausted:{}", dunning.attempt));
        Box::pin(async {})
    }
}
