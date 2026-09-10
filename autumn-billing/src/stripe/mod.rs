//! Stripe implementation of [`BillingProvider`].
//!
//! Uses `autumn_web::http::Client` named `stripe`, so tests mock it with
//! `TestApp::http_mock("stripe")`. Stripe types never leave this module.

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
    /// Build from app state (shared HTTP client, mocks in tests).
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the secret key is missing.
    pub fn from_state(state: &AppState, config: &StripeConfig) -> Result<Self, BillingError> {
        let client = autumn_web::http::Client::from_state(state).named(PROVIDER_NAME);
        Self::new(config.clone(), client)
    }

    /// Build with an explicit client.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the secret key is missing.
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
        Ok(Self { config, client })
    }

    /// Decode a raw Stripe event body. Public for fixture tests.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Malformed`] for a known type that cannot be decoded.
    pub fn parse_event_body(raw: &[u8]) -> Result<BillingEvent, BillingError> {
        let _ = raw;
        Err(BillingError::Unsupported("stripe parse"))
    }

    #[allow(dead_code, reason = "used once the client is implemented")]
    fn client(&self) -> &autumn_web::http::Client {
        &self.client
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
        let _ = request;
        Box::pin(async { Err(BillingError::Unsupported("stripe create_customer")) })
    }

    fn create_checkout(&self, request: CheckoutRequest) -> ProviderFuture<'_, HostedSession> {
        let _ = request;
        Box::pin(async { Err(BillingError::Unsupported("stripe create_checkout")) })
    }

    fn create_portal(&self, request: PortalRequest) -> ProviderFuture<'_, HostedSession> {
        let _ = request;
        Box::pin(async { Err(BillingError::Unsupported("stripe create_portal")) })
    }

    fn parse_event(&self, raw: &[u8]) -> Result<BillingEvent, BillingError> {
        Self::parse_event_body(raw)
    }

    fn retry_invoice_payment<'a>(
        &'a self,
        invoice: &'a ProviderId,
        idempotency_key: &'a str,
    ) -> ProviderFuture<'a, PaymentAttemptOutcome> {
        let _ = (invoice, idempotency_key);
        Box::pin(async { Err(BillingError::Unsupported("stripe retry")) })
    }

    fn cancel_subscription<'a>(&'a self, subscription: &'a ProviderId) -> ProviderFuture<'a, ()> {
        let _ = subscription;
        Box::pin(async { Err(BillingError::Unsupported("stripe cancel")) })
    }
}
