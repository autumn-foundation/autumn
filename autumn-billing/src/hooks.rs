//! Application callbacks for billing state changes.

use std::future::Future;
use std::pin::Pin;

use crate::model::{DunningAttempt, Invoice, Subscription};

/// Boxed future returned by hooks.
pub type HookFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Callbacks the application can install with `BillingPlugin::hooks`.
///
/// Every method has a no-op default.
pub trait BillingHooks: Send + Sync + 'static {
    /// Map an application user id to a notification recipient id.
    ///
    /// The default parses the id as `i64`. Return `None` to send no
    /// notification for this user.
    fn recipient_for(&self, user_id: &str) -> Option<i64> {
        user_id.parse().ok()
    }

    /// A subscription row was created or changed.
    fn on_subscription_changed<'a>(
        &'a self,
        _subscription: &'a Subscription,
        _previous: Option<&'a Subscription>,
    ) -> HookFuture<'a> {
        Box::pin(async {})
    }

    /// A payment failed and a dunning schedule is open.
    fn on_payment_failed<'a>(
        &'a self,
        _invoice: &'a Invoice,
        _dunning: &'a DunningAttempt,
    ) -> HookFuture<'a> {
        Box::pin(async {})
    }

    /// A failed invoice was paid.
    fn on_payment_recovered<'a>(&'a self, _invoice: &'a Invoice) -> HookFuture<'a> {
        Box::pin(async {})
    }

    /// Every retry failed.
    fn on_dunning_exhausted<'a>(
        &'a self,
        _invoice: &'a Invoice,
        _dunning: &'a DunningAttempt,
    ) -> HookFuture<'a> {
        Box::pin(async {})
    }
}

/// Hooks that do nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;

impl BillingHooks for NoHooks {}
