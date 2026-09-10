//! Notifications (#1148) and hook fan-out for billing events.
//!
//! The plugin writes to the same [`Notifications`] service the extractor
//! resolves for handlers: the store registered with
//! `AppBuilder::with_notification_store`, else the database store when a
//! pool exists, else the in-memory store.

use autumn_web::AppState;
use autumn_web::notifications::{MemoryNotificationStore, Notifications};
use serde_json::{Value, json};

use crate::BillingService;
use crate::model::{Customer, Invoice, Subscription};

/// Notification kind: a payment failed and dunning started.
pub const KIND_PAYMENT_FAILED: &str = "billing.payment_failed";
/// Notification kind: a failed invoice was paid.
pub const KIND_PAYMENT_RECOVERED: &str = "billing.payment_recovered";
/// Notification kind: every retry failed.
pub const KIND_DUNNING_EXHAUSTED: &str = "billing.dunning_exhausted";
/// Notification kind: a subscription ended.
pub const KIND_SUBSCRIPTION_CANCELED: &str = "billing.subscription_canceled";

/// Send an in-app notification to `recipient`. Never fails the caller: a
/// store error is logged.
pub async fn send(state: &AppState, recipient: i64, kind: &str, payload: Value) {
    let notifications = state.extension_or_insert_with::<Notifications>(|| default_for(state));
    if let Err(error) = notifications.notify(recipient, kind, payload).await {
        tracing::warn!(
            kind,
            recipient,
            error = %error,
            "🍂 Autumn Billing: notification not stored"
        );
    }
}

/// Send `kind` to the user linked to `customer`, when there is one and the
/// hooks map it to a recipient.
pub(crate) async fn send_to_customer(
    state: &AppState,
    service: &BillingService,
    customer: &Customer,
    kind: &str,
    payload: Value,
) {
    let recipient = customer
        .user_id
        .as_deref()
        .and_then(|user_id| service.hooks().recipient_for(user_id));
    match recipient {
        Some(recipient) => send(state, recipient, kind, payload).await,
        None => tracing::debug!(
            kind,
            customer_id = %customer.id,
            "🍂 Autumn Billing: no notification recipient for customer; skipped"
        ),
    }
}

/// The default service, mirroring the extractor: the database store when a
/// pool exists, else memory.
fn default_for(state: &AppState) -> Notifications {
    #[cfg(feature = "db")]
    if let Some(pool) = autumn_web::db::DbState::pool(state) {
        return Notifications::new(autumn_web::notifications::DbNotificationStore::new(
            pool.clone(),
        ));
    }
    Notifications::new(MemoryNotificationStore::new())
}

/// JSON form of a money value; `null` when it cannot be serialized.
fn money(value: &crate::money::Money) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Payload of [`KIND_PAYMENT_FAILED`].
///
/// `attempt` is the number of the retry the plugin scheduled; `null` when
/// dunning is disabled. `reason` is the provider decline reason, when known.
pub(crate) fn payment_failed_payload(
    invoice: &Invoice,
    attempt: Option<i64>,
    reason: Option<&str>,
) -> Value {
    json!({
        "invoice_id": invoice.id,
        "provider_invoice_id": invoice.provider_invoice_id,
        "subscription_id": invoice.subscription_id,
        "amount_due": money(&invoice.amount_due),
        "attempt": attempt,
        "provider_attempt_count": invoice.attempt_count,
        "reason": reason,
    })
}

/// Payload of [`KIND_PAYMENT_RECOVERED`].
pub(crate) fn payment_recovered_payload(invoice: &Invoice) -> Value {
    json!({
        "invoice_id": invoice.id,
        "provider_invoice_id": invoice.provider_invoice_id,
        "subscription_id": invoice.subscription_id,
        "amount_paid": money(&invoice.amount_paid),
    })
}

/// Payload of [`KIND_DUNNING_EXHAUSTED`].
pub(crate) fn dunning_exhausted_payload(invoice: &Invoice, attempts: i64, action: &str) -> Value {
    json!({
        "invoice_id": invoice.id,
        "provider_invoice_id": invoice.provider_invoice_id,
        "subscription_id": invoice.subscription_id,
        "amount_due": money(&invoice.amount_due),
        "attempts": attempts,
        "action": action,
    })
}

/// Payload of [`KIND_SUBSCRIPTION_CANCELED`].
pub(crate) fn subscription_canceled_payload(subscription: &Subscription) -> Value {
    json!({
        "subscription_id": subscription.id,
        "provider_subscription_id": subscription.provider_subscription_id,
        "plan_id": subscription.plan_id,
    })
}
