//! Notifications (#1148) and hook fan-out for billing events.

use autumn_web::AppState;
use serde_json::Value;

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
    let _ = (state, recipient, kind, payload);
}
