//! The provider seam. Stripe ships first; the trait never names Stripe types.

use std::future::Future;
use std::pin::Pin;

use autumn_web::webhook::WebhookEndpointConfig;
use serde::{Deserialize, Serialize};

use crate::error::BillingError;
use crate::event::BillingEvent;
use crate::model::ProviderId;

/// Boxed future returned by provider calls.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BillingError>> + Send + 'a>>;

/// A hosted provider page (checkout or portal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostedSession {
    /// Provider session id.
    pub id: ProviderId,
    /// URL to redirect the browser to.
    pub url: String,
}

impl HostedSession {
    /// Build a hosted session.
    #[must_use]
    pub fn new(id: impl Into<ProviderId>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
        }
    }
}

/// Request to create a provider customer for a local user.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CustomerRequest {
    /// Local customer id. Sent as provider metadata.
    pub local_customer_id: String,
    /// Application user id. Sent as provider metadata.
    pub user_id: String,
    /// Email, when known.
    pub email: Option<String>,
}

impl CustomerRequest {
    /// Build a request.
    #[must_use]
    pub fn new(local_customer_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            local_customer_id: local_customer_id.into(),
            user_id: user_id.into(),
            email: None,
        }
    }

    /// Set the email.
    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }
}

/// Request to start a hosted checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckoutRequest {
    /// Provider customer id.
    pub provider_customer_id: ProviderId,
    /// Local customer id. Sent to the provider as the checkout reference.
    pub local_customer_id: String,
    /// Provider price id to subscribe to.
    pub provider_price_id: ProviderId,
    /// Seat quantity.
    pub quantity: i64,
    /// Redirect after success. From config only.
    pub success_url: String,
    /// Redirect after cancel. From config only.
    pub cancel_url: String,
}

impl CheckoutRequest {
    /// Build a request with quantity 1.
    #[must_use]
    pub fn new(
        provider_customer_id: impl Into<ProviderId>,
        local_customer_id: impl Into<String>,
        provider_price_id: impl Into<ProviderId>,
        success_url: impl Into<String>,
        cancel_url: impl Into<String>,
    ) -> Self {
        Self {
            provider_customer_id: provider_customer_id.into(),
            local_customer_id: local_customer_id.into(),
            provider_price_id: provider_price_id.into(),
            quantity: 1,
            success_url: success_url.into(),
            cancel_url: cancel_url.into(),
        }
    }

    /// Set the quantity.
    #[must_use]
    pub const fn with_quantity(mut self, quantity: i64) -> Self {
        self.quantity = quantity;
        self
    }
}

/// Request to open the hosted billing portal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PortalRequest {
    /// Provider customer id.
    pub provider_customer_id: ProviderId,
    /// Redirect when the customer leaves the portal. From config only.
    pub return_url: String,
}

impl PortalRequest {
    /// Build a request.
    #[must_use]
    pub fn new(provider_customer_id: impl Into<ProviderId>, return_url: impl Into<String>) -> Self {
        Self {
            provider_customer_id: provider_customer_id.into(),
            return_url: return_url.into(),
        }
    }
}

/// Result of one payment retry. A transport failure is an `Err`, never a variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PaymentAttemptOutcome {
    /// The invoice is now paid.
    Paid,
    /// The provider declined the payment.
    Declined {
        /// Provider decline reason.
        reason: String,
    },
    /// The invoice was already paid.
    AlreadyPaid,
}

/// A billing provider.
///
/// Object safe. New capabilities land as defaulted methods.
pub trait BillingProvider: Send + Sync + 'static {
    /// Provider name (`stripe`).
    fn name(&self) -> &'static str;

    /// The `SignedWebhook` endpoint config that verifies this provider's
    /// webhooks at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Config`] when the webhook secret is missing.
    fn webhook_endpoint(
        &self,
        name: &str,
        path: &str,
    ) -> Result<WebhookEndpointConfig, BillingError>;

    /// Create a provider customer.
    fn create_customer(&self, request: CustomerRequest) -> ProviderFuture<'_, ProviderId>;

    /// Start a hosted checkout.
    fn create_checkout(&self, request: CheckoutRequest) -> ProviderFuture<'_, HostedSession>;

    /// Open the hosted billing portal.
    fn create_portal(&self, request: PortalRequest) -> ProviderFuture<'_, HostedSession>;

    /// Decode a verified raw webhook body.
    ///
    /// # Errors
    ///
    /// Returns [`BillingError::Malformed`] when a known event type cannot be
    /// decoded. Unknown event types decode to [`crate::event::BillingEventKind::Ignored`].
    fn parse_event(&self, raw: &[u8]) -> Result<BillingEvent, BillingError>;

    /// Ask the provider to collect an open invoice again.
    ///
    /// `idempotency_key` is stable per `(invoice, attempt)`.
    fn retry_invoice_payment<'a>(
        &'a self,
        invoice: &'a ProviderId,
        idempotency_key: &'a str,
    ) -> ProviderFuture<'a, PaymentAttemptOutcome>;

    /// Cancel a subscription now.
    fn cancel_subscription<'a>(&'a self, subscription: &'a ProviderId) -> ProviderFuture<'a, ()>;

    /// Report a usage quantity for a metered subscription. Optional.
    fn report_usage<'a>(
        &'a self,
        _subscription: &'a ProviderId,
        _quantity: i64,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async { Err(BillingError::Unsupported("usage reporting")) })
    }
}
