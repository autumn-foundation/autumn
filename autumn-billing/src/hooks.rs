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
    /// Under Autumn's tenancy feature, `user_id` is the tenant-scoped
    /// identity `{tenant}{SEP}{user_id}` (`gate::scope_identity_to_tenant`;
    /// `SEP` is a `\u{1}` control byte, never a character a tenant or user id
    /// itself contains), not the bare session id — two different tenants'
    /// sessions can otherwise stringify to the identical id (a sharded
    /// deployment's shard-local `BIGSERIAL`, `docs/guide/sharding.md`), and
    /// this is the same identity every billing lookup is keyed on. The
    /// default strips that prefix (splitting on the last `SEP`) before
    /// parsing, so it keeps working unchanged; an app overriding this hook
    /// and expecting the bare id under tenancy should do the same.
    fn recipient_for(&self, user_id: &str) -> Option<i64> {
        user_id
            .rsplit(crate::gate::TENANT_IDENTITY_SEPARATOR)
            .next()
            .unwrap_or(user_id)
            .parse()
            .ok()
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
