//! Provider-neutral billing events.
//!
//! A provider parses its raw webhook body into one [`BillingEvent`]. The
//! reconciler never sees provider types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{InvoiceStatus, ProviderId, SubscriptionStatus};
use crate::money::Money;

/// One normalized provider event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BillingEvent {
    /// Provider event id. The idempotency key of the ledger.
    pub id: String,
    /// Provider time the event was created. The ordering key.
    pub occurred_at: DateTime<Utc>,
    /// What happened.
    pub kind: BillingEventKind,
}

impl BillingEvent {
    /// Build an event.
    #[must_use]
    pub fn new(id: impl Into<String>, occurred_at: DateTime<Utc>, kind: BillingEventKind) -> Self {
        Self {
            id: id.into(),
            occurred_at,
            kind,
        }
    }
}

/// The payload of a [`BillingEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BillingEventKind {
    /// A hosted checkout completed.
    CheckoutCompleted(CheckoutSnapshot),
    /// A subscription was created or updated.
    SubscriptionChanged(SubscriptionSnapshot),
    /// A subscription ended.
    SubscriptionDeleted(SubscriptionSnapshot),
    /// An invoice payment failed.
    InvoicePaymentFailed(InvoiceSnapshot),
    /// An invoice was paid.
    InvoicePaid(InvoiceSnapshot),
    /// A provider event the plugin does not mirror. Recorded, not applied.
    Ignored {
        /// Provider event type (for example `charge.refunded`).
        event_type: String,
    },
}

impl BillingEventKind {
    /// Short stable label for logs and the ledger.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::CheckoutCompleted(_) => "checkout_completed",
            Self::SubscriptionChanged(_) => "subscription_changed",
            Self::SubscriptionDeleted(_) => "subscription_deleted",
            Self::InvoicePaymentFailed(_) => "invoice_payment_failed",
            Self::InvoicePaid(_) => "invoice_paid",
            Self::Ignored { .. } => "ignored",
        }
    }
}

/// Snapshot of a completed checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CheckoutSnapshot {
    /// Provider customer id.
    pub provider_customer_id: ProviderId,
    /// The `client_reference_id` the plugin set at checkout: the local customer id.
    pub local_customer_ref: Option<String>,
    /// Customer email reported by the provider.
    pub email: Option<String>,
    /// Provider subscription id created by the checkout, when present.
    pub provider_subscription_id: Option<ProviderId>,
}

/// Snapshot of a subscription as reported by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscriptionSnapshot {
    /// Provider subscription id.
    pub provider_subscription_id: ProviderId,
    /// Provider customer id.
    pub provider_customer_id: ProviderId,
    /// Provider price id of the first item.
    pub provider_price_id: Option<ProviderId>,
    /// Status.
    pub status: SubscriptionStatus,
    /// Seat quantity.
    pub quantity: i64,
    /// End of the current period.
    pub current_period_end: Option<DateTime<Utc>>,
    /// Cancels at period end.
    pub cancel_at_period_end: bool,
}

impl SubscriptionSnapshot {
    /// Build a minimal snapshot.
    #[must_use]
    pub fn new(
        provider_subscription_id: impl Into<ProviderId>,
        provider_customer_id: impl Into<ProviderId>,
        status: SubscriptionStatus,
    ) -> Self {
        Self {
            provider_subscription_id: provider_subscription_id.into(),
            provider_customer_id: provider_customer_id.into(),
            provider_price_id: None,
            status,
            quantity: 1,
            current_period_end: None,
            cancel_at_period_end: false,
        }
    }

    /// Set the price id.
    #[must_use]
    pub fn with_price(mut self, price_id: impl Into<ProviderId>) -> Self {
        self.provider_price_id = Some(price_id.into());
        self
    }

    /// Set the period end.
    #[must_use]
    pub const fn with_period_end(mut self, end: DateTime<Utc>) -> Self {
        self.current_period_end = Some(end);
        self
    }

    /// Set the quantity.
    #[must_use]
    pub const fn with_quantity(mut self, quantity: i64) -> Self {
        self.quantity = quantity;
        self
    }

    /// Set `cancel_at_period_end`.
    #[must_use]
    pub const fn with_cancel_at_period_end(mut self, cancel: bool) -> Self {
        self.cancel_at_period_end = cancel;
        self
    }
}

/// Snapshot of an invoice as reported by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct InvoiceSnapshot {
    /// Provider invoice id.
    pub provider_invoice_id: ProviderId,
    /// Provider customer id.
    pub provider_customer_id: ProviderId,
    /// Provider subscription id, when the invoice belongs to one.
    pub provider_subscription_id: Option<ProviderId>,
    /// Status.
    pub status: InvoiceStatus,
    /// Amount due.
    pub amount_due: Money,
    /// Amount paid.
    pub amount_paid: Money,
    /// Provider payment attempts so far.
    pub attempt_count: i64,
    /// Provider's own next attempt time, when reported.
    pub next_payment_attempt: Option<DateTime<Utc>>,
}

impl InvoiceSnapshot {
    /// Build a minimal snapshot.
    #[must_use]
    pub fn new(
        provider_invoice_id: impl Into<ProviderId>,
        provider_customer_id: impl Into<ProviderId>,
        status: InvoiceStatus,
        amount_due: Money,
    ) -> Self {
        Self {
            provider_invoice_id: provider_invoice_id.into(),
            provider_customer_id: provider_customer_id.into(),
            provider_subscription_id: None,
            status,
            amount_due,
            amount_paid: Money::zero(amount_due.currency()),
            attempt_count: 0,
            next_payment_attempt: None,
        }
    }

    /// Set the subscription id.
    #[must_use]
    pub fn with_subscription(mut self, id: impl Into<ProviderId>) -> Self {
        self.provider_subscription_id = Some(id.into());
        self
    }

    /// Set the amount paid.
    #[must_use]
    pub const fn with_amount_paid(mut self, amount: Money) -> Self {
        self.amount_paid = amount;
        self
    }

    /// Set the provider attempt count.
    #[must_use]
    pub const fn with_attempt_count(mut self, count: i64) -> Self {
        self.attempt_count = count;
        self
    }

    /// Set the provider's next attempt time.
    #[must_use]
    pub const fn with_next_payment_attempt(mut self, at: DateTime<Utc>) -> Self {
        self.next_payment_attempt = Some(at);
        self
    }
}
