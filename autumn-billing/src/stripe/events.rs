//! Decode a Stripe webhook body into a [`BillingEvent`].
//!
//! Only the fields the mirror needs are declared. Everything else in the
//! payload is ignored, so a new Stripe API version does not break decoding.
//!
//! Two shapes are accepted for each versioned field:
//!
//! - The subscription of an invoice: `parent.subscription_details.subscription`
//!   (API 2025-03-31 and later) or the top-level `subscription` (older).
//! - The period end of a subscription: `items.data[0].current_period_end`
//!   (API 2025-03-31 and later) or the top-level `current_period_end` (older).
//!
//! The new path wins when both are present.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::error::BillingError;
use crate::event::{
    BillingEvent, BillingEventKind, CheckoutSnapshot, InvoiceSnapshot, SubscriptionSnapshot,
};
use crate::model::{InvoiceStatus, ProviderId, SubscriptionStatus};
use crate::money::{Currency, Money};

/// The event envelope. `data.object` stays raw so an unknown event type never
/// fails on its object shape.
#[derive(Deserialize)]
struct Envelope {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    created: i64,
    data: EnvelopeData,
}

#[derive(Deserialize)]
struct EnvelopeData {
    object: Value,
}

/// A reference that Stripe sends as an id string or as an expanded object.
#[derive(Deserialize)]
#[serde(untagged)]
enum ObjectRef {
    Id(String),
    Expanded { id: String },
}

impl ObjectRef {
    fn into_id(self) -> ProviderId {
        match self {
            Self::Id(id) | Self::Expanded { id } => ProviderId::new(id),
        }
    }
}

#[derive(Deserialize)]
struct CheckoutSession {
    customer: Option<ObjectRef>,
    client_reference_id: Option<String>,
    customer_details: Option<CustomerDetails>,
    customer_email: Option<String>,
    subscription: Option<ObjectRef>,
}

#[derive(Deserialize)]
struct CustomerDetails {
    email: Option<String>,
}

#[derive(Deserialize)]
struct Subscription {
    id: String,
    customer: ObjectRef,
    status: String,
    #[serde(default)]
    cancel_at_period_end: bool,
    current_period_end: Option<i64>,
    items: Option<ItemList>,
}

#[derive(Deserialize)]
struct ItemList {
    #[serde(default)]
    data: Vec<SubscriptionItem>,
}

#[derive(Deserialize)]
struct SubscriptionItem {
    price: Option<ObjectRef>,
    quantity: Option<i64>,
    current_period_end: Option<i64>,
}

#[derive(Deserialize)]
struct Invoice {
    id: String,
    customer: ObjectRef,
    status: String,
    amount_due: i64,
    amount_paid: i64,
    currency: String,
    #[serde(default)]
    attempt_count: i64,
    next_payment_attempt: Option<i64>,
    subscription: Option<ObjectRef>,
    parent: Option<InvoiceParent>,
}

#[derive(Deserialize)]
struct InvoiceParent {
    subscription_details: Option<SubscriptionDetails>,
}

#[derive(Deserialize)]
struct SubscriptionDetails {
    subscription: Option<ObjectRef>,
}

/// Decode `raw`. Public entry point of this module.
pub(super) fn parse(raw: &[u8]) -> Result<BillingEvent, BillingError> {
    let envelope: Envelope = serde_json::from_slice(raw)
        .map_err(|e| BillingError::Malformed(format!("stripe event envelope: {}", position(&e))))?;
    let occurred_at = unix(envelope.created).ok_or_else(|| {
        BillingError::Malformed(format!(
            "stripe event {}: created is out of range",
            envelope.id
        ))
    })?;
    let context = Context {
        event_id: &envelope.id,
        event_type: &envelope.event_type,
    };
    let object = envelope.data.object;
    let kind = match envelope.event_type.as_str() {
        "checkout.session.completed" => {
            BillingEventKind::CheckoutCompleted(checkout(context.decode(object)?, context)?)
        }
        "customer.subscription.created" | "customer.subscription.updated" => {
            BillingEventKind::SubscriptionChanged(subscription(context.decode(object)?, context)?)
        }
        "customer.subscription.deleted" => {
            let mut snapshot = subscription(context.decode(object)?, context)?;
            // The provider reports the last state it saw. The event itself
            // says the subscription ended.
            snapshot.status = SubscriptionStatus::Canceled;
            BillingEventKind::SubscriptionDeleted(snapshot)
        }
        "invoice.payment_failed" => {
            BillingEventKind::InvoicePaymentFailed(invoice(context.decode(object)?, context)?)
        }
        "invoice.paid" | "invoice.payment_succeeded" => {
            BillingEventKind::InvoicePaid(invoice(context.decode(object)?, context)?)
        }
        _ => BillingEventKind::Ignored {
            event_type: envelope.event_type.clone(),
        },
    };
    Ok(BillingEvent {
        id: envelope.id,
        occurred_at,
        kind,
    })
}

/// Event id and type, for error messages. Never the body.
#[derive(Clone, Copy)]
struct Context<'a> {
    event_id: &'a str,
    event_type: &'a str,
}

impl Context<'_> {
    /// Decode `data.object` as `T`.
    fn decode<T: serde::de::DeserializeOwned>(self, object: Value) -> Result<T, BillingError> {
        serde_json::from_value(object).map_err(|e| {
            self.malformed(format!("object does not match the expected shape ({})", position(&e)))
        })
    }

    fn malformed(self, detail: impl std::fmt::Display) -> BillingError {
        BillingError::Malformed(format!(
            "stripe {} {}: {detail}",
            self.event_type, self.event_id
        ))
    }
}

/// Line and column of a decode error. The message of `serde_json::Error` can
/// echo a value from the body, so only its position is reported.
fn position(error: &serde_json::Error) -> String {
    format!("line {}, column {}", error.line(), error.column())
}

fn unix(secs: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(secs, 0)
}

fn checkout(session: CheckoutSession, context: Context<'_>) -> Result<CheckoutSnapshot, BillingError> {
    let provider_customer_id = session
        .customer
        .map(ObjectRef::into_id)
        .ok_or_else(|| context.malformed("checkout session has no customer"))?;
    let email = session
        .customer_details
        .and_then(|d| d.email)
        .or(session.customer_email);
    Ok(CheckoutSnapshot {
        provider_customer_id,
        local_customer_ref: session.client_reference_id,
        email,
        provider_subscription_id: session.subscription.map(ObjectRef::into_id),
    })
}

fn subscription(
    subscription: Subscription,
    context: Context<'_>,
) -> Result<SubscriptionSnapshot, BillingError> {
    let status = SubscriptionStatus::parse(&subscription.status)
        .ok_or_else(|| context.malformed("unknown subscription status"))?;
    let first_item = subscription
        .items
        .and_then(|items| items.data.into_iter().next());
    let (provider_price_id, quantity, item_period_end) = match first_item {
        Some(item) => (
            item.price.map(ObjectRef::into_id),
            item.quantity.unwrap_or(1),
            item.current_period_end,
        ),
        None => (None, 1, None),
    };
    let current_period_end = item_period_end
        .or(subscription.current_period_end)
        .map(|secs| {
            unix(secs).ok_or_else(|| context.malformed("current_period_end is out of range"))
        })
        .transpose()?;
    Ok(SubscriptionSnapshot {
        provider_subscription_id: ProviderId::new(subscription.id),
        provider_customer_id: subscription.customer.into_id(),
        provider_price_id,
        status,
        quantity,
        current_period_end,
        cancel_at_period_end: subscription.cancel_at_period_end,
    })
}

fn invoice(invoice: Invoice, context: Context<'_>) -> Result<InvoiceSnapshot, BillingError> {
    let status = InvoiceStatus::parse(&invoice.status)
        .ok_or_else(|| context.malformed("unknown invoice status"))?;
    let currency = Currency::new(&invoice.currency)
        .map_err(|_| context.malformed("currency is not a three-letter code"))?;
    let provider_subscription_id = invoice
        .parent
        .and_then(|parent| parent.subscription_details)
        .and_then(|details| details.subscription)
        .or(invoice.subscription)
        .map(ObjectRef::into_id);
    let next_payment_attempt = invoice
        .next_payment_attempt
        .map(|secs| {
            unix(secs).ok_or_else(|| context.malformed("next_payment_attempt is out of range"))
        })
        .transpose()?;
    Ok(InvoiceSnapshot {
        provider_invoice_id: ProviderId::new(invoice.id),
        provider_customer_id: invoice.customer.into_id(),
        provider_subscription_id,
        status,
        amount_due: Money::from_minor(invoice.amount_due, currency),
        amount_paid: Money::from_minor(invoice.amount_paid, currency),
        attempt_count: invoice.attempt_count,
        next_payment_attempt,
    })
}
