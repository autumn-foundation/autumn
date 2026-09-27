//! Stripe implementation of [`BillingProvider`].
//!
//! Uses `autumn_web::http::Client` named `stripe`, so tests mock it with
//! `TestApp::http_mock("stripe")`. Stripe types never leave this module:
//! [`events`] decodes webhook bodies and [`client`] talks to the REST API.
//!
//! Only `retry_invoice_payment` sends an `Idempotency-Key` header. The
//! `create_*` calls send none; see the `client` module for why.

mod client;
mod events;

use autumn_web::AppState;
use autumn_web::webhook::WebhookEndpointConfig;

use crate::config::StripeConfig;
use crate::error::BillingError;
use crate::event::BillingEvent;
use crate::model::ProviderId;
use crate::provider::{
    BillingProvider, CheckoutRequest, CustomerRequest, HostedSession, PaymentAttemptOutcome,
    PortalRequest, ProviderFuture,
};

/// Provider name.
pub const PROVIDER_NAME: &str = "stripe";

/// The Stripe provider.
pub struct StripeProvider {
    config: StripeConfig,
    client: autumn_web::http::Client,
}

impl std::fmt::Debug for StripeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeProvider")
            .field("api_base", &self.config.api_base)
            .finish_non_exhaustive()
    }
}

impl StripeProvider {
    /// Provider name, as returned by [`BillingProvider::name`].
    pub const NAME: &'static str = PROVIDER_NAME;

    /// Build from app state (shared HTTP client, mocks in tests).
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the secret key is missing.
    pub fn from_state(state: &AppState, config: &StripeConfig) -> Result<Self, BillingError> {
        let client = autumn_web::http::Client::from_state(state).named(PROVIDER_NAME);
        Self::new(config.clone(), client)
    }

    /// Build with an explicit client. Relative paths resolve against
    /// `config.api_base`.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the secret key is missing.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "public signature; `with_base_url` borrows and returns a fresh client"
    )]
    pub fn new(
        config: StripeConfig,
        client: autumn_web::http::Client,
    ) -> Result<Self, BillingError> {
        if config
            .secret_key
            .as_ref()
            .is_none_or(crate::config::SecretString::is_empty)
        {
            return Err(BillingError::Config(format!(
                "{} is not set",
                crate::config::STRIPE_SECRET_KEY_ENV
            )));
        }
        let client = client.with_base_url(&config.api_base);
        Ok(Self { config, client })
    }

    /// Decode a raw Stripe event body. Exposed for fixture tests; application
    /// code uses [`BillingProvider::parse_event`].
    ///
    /// Known event types map to their [`crate::event::BillingEventKind`]; any
    /// other type decodes to `Ignored`. Both the current and the previous
    /// Stripe API shapes of subscription and invoice objects are accepted.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Malformed`] for a body that is not an event
    /// envelope, or for a known type whose object cannot be decoded. The
    /// message never contains the body.
    #[doc(hidden)]
    pub fn parse_event_body(raw: &[u8]) -> Result<BillingEvent, BillingError> {
        events::parse(raw)
    }
}

impl BillingProvider for StripeProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn webhook_endpoint(
        &self,
        name: &str,
        path: &str,
    ) -> Result<WebhookEndpointConfig, BillingError> {
        let secret = self
            .config
            .webhook_secret
            .as_ref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                BillingError::Config(format!(
                    "{} is not set",
                    crate::config::STRIPE_WEBHOOK_SECRET_ENV
                ))
            })?;
        Ok(WebhookEndpointConfig::stripe(name, path, secret.expose()))
    }

    fn create_customer(&self, request: CustomerRequest) -> ProviderFuture<'_, ProviderId> {
        Box::pin(self.customer(request))
    }

    fn create_checkout(&self, request: CheckoutRequest) -> ProviderFuture<'_, HostedSession> {
        Box::pin(self.checkout(request))
    }

    fn create_portal(&self, request: PortalRequest) -> ProviderFuture<'_, HostedSession> {
        Box::pin(self.portal(request))
    }

    fn parse_event(&self, raw: &[u8]) -> Result<BillingEvent, BillingError> {
        Self::parse_event_body(raw)
    }

    fn retry_invoice_payment<'a>(
        &'a self,
        invoice: &'a ProviderId,
        idempotency_key: &'a str,
    ) -> ProviderFuture<'a, PaymentAttemptOutcome> {
        Box::pin(self.pay_invoice(invoice, idempotency_key))
    }

    fn cancel_subscription<'a>(&'a self, subscription: &'a ProviderId) -> ProviderFuture<'a, ()> {
        Box::pin(self.cancel(subscription))
    }
}
