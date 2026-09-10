//! The mirror store: customers, subscriptions, invoices, the event ledger
//! and the dunning schedule.
//!
//! Two implementations: [`MemoryBillingStore`] (tests, DB-less apps) and
//! [`DbBillingStore`] (Postgres / `SQLite` through `RuntimeConnection`).
//! The ordering guard for upserts lives in the store: an upsert applies only
//! when [`should_apply`] says so.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::error::BillingError;
use crate::model::{
    Customer, DunningAttempt, Invoice, InvoiceStatus, ProviderId, Subscription, SubscriptionStatus,
};
use crate::money::Money;
use crate::plan::PlanId;

pub mod memory;
pub use memory::MemoryBillingStore;

#[cfg(feature = "db")]
pub mod db;
#[cfg(feature = "db")]
pub use db::DbBillingStore;

/// Boxed future returned by store calls.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BillingError>> + Send + 'a>>;

/// Result of claiming an event id in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClaim {
    /// First delivery: the caller owns processing.
    Claimed,
    /// Already applied, or in flight elsewhere.
    Duplicate,
}

/// Result of a guarded upsert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write<T> {
    /// The row was inserted or updated.
    Applied(T),
    /// An equal or newer event was already applied. `T` is the stored row.
    Stale(T),
}

impl<T> Write<T> {
    /// The stored row after the call.
    #[must_use]
    pub fn into_inner(self) -> T {
        match self {
            Self::Applied(row) | Self::Stale(row) => row,
        }
    }

    /// `true` when the row changed.
    #[must_use]
    pub const fn is_applied(&self) -> bool {
        matches!(self, Self::Applied(_))
    }
}

/// Ordering guard shared by every store.
///
/// Apply when the incoming event is newer, or is at the same instant and its
/// status ranks higher. Never leave a terminal status.
#[must_use]
pub fn should_apply(
    existing_at: DateTime<Utc>,
    existing_rank: u8,
    existing_terminal: bool,
    incoming_at: DateTime<Utc>,
    incoming_rank: u8,
) -> bool {
    if existing_terminal {
        return false;
    }
    incoming_at > existing_at || (incoming_at == existing_at && incoming_rank > existing_rank)
}

/// Customer upsert keyed by `provider_customer_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CustomerUpsert {
    /// Local id used when the row is inserted.
    pub new_id: String,
    /// Provider name.
    pub provider: String,
    /// Provider customer id (the key).
    pub provider_customer_id: ProviderId,
    /// Link to this user. Never replaces an existing link.
    pub user_id: Option<String>,
    /// Email. Replaces the stored email when `Some`.
    pub email: Option<String>,
    /// App clock.
    pub now: DateTime<Utc>,
}

impl CustomerUpsert {
    /// Build an upsert.
    #[must_use]
    pub fn new(
        new_id: impl Into<String>,
        provider: impl Into<String>,
        provider_customer_id: impl Into<ProviderId>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            new_id: new_id.into(),
            provider: provider.into(),
            provider_customer_id: provider_customer_id.into(),
            user_id: None,
            email: None,
            now,
        }
    }

    /// Link the user.
    #[must_use]
    pub fn with_user(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Set the email.
    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }
}

/// Subscription upsert keyed by `provider_subscription_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubscriptionUpsert {
    /// Local id used when the row is inserted.
    pub new_id: String,
    /// Local customer id.
    pub customer_id: String,
    /// Provider subscription id (the key).
    pub provider_subscription_id: ProviderId,
    /// Provider price id.
    pub provider_price_id: Option<ProviderId>,
    /// Resolved plan.
    pub plan_id: Option<PlanId>,
    /// Status.
    pub status: SubscriptionStatus,
    /// Seat quantity.
    pub quantity: i64,
    /// Period end.
    pub current_period_end: Option<DateTime<Utc>>,
    /// Cancels at period end.
    pub cancel_at_period_end: bool,
    /// Provider event time (ordering key).
    pub occurred_at: DateTime<Utc>,
    /// App clock.
    pub now: DateTime<Utc>,
}

/// Invoice upsert keyed by `provider_invoice_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InvoiceUpsert {
    /// Local id used when the row is inserted.
    pub new_id: String,
    /// Local customer id.
    pub customer_id: String,
    /// Local subscription id.
    pub subscription_id: Option<String>,
    /// Provider invoice id (the key).
    pub provider_invoice_id: ProviderId,
    /// Status.
    pub status: InvoiceStatus,
    /// Amount due.
    pub amount_due: Money,
    /// Amount paid.
    pub amount_paid: Money,
    /// Provider attempt count.
    pub attempt_count: i64,
    /// Provider next attempt.
    pub next_payment_attempt: Option<DateTime<Utc>>,
    /// Provider event time (ordering key).
    pub occurred_at: DateTime<Utc>,
    /// App clock.
    pub now: DateTime<Utc>,
}

/// The mirror store.
///
/// Object safe. Every method is idempotent so a retried reconcile converges.
pub trait BillingStore: Send + Sync + 'static {
    // ── Event ledger ────────────────────────────────────────────────────

    /// Claim `event_id`. A row in `processing` older than `stale_after` is
    /// re-claimable (the previous owner died).
    fn claim_event<'a>(
        &'a self,
        event_id: &'a str,
        kind: &'a str,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> StoreFuture<'a, EventClaim>;

    /// Mark `event_id` applied.
    fn finish_event<'a>(&'a self, event_id: &'a str, now: DateTime<Utc>) -> StoreFuture<'a, ()>;

    /// Drop the claim so the provider's redelivery is processed again.
    fn release_event<'a>(&'a self, event_id: &'a str) -> StoreFuture<'a, ()>;

    /// Number of events in the ledger with `applied` set.
    fn applied_event_count(&self) -> StoreFuture<'_, u64>;

    // ── Customers ───────────────────────────────────────────────────────

    /// Insert or update a customer.
    fn upsert_customer(&self, upsert: CustomerUpsert) -> StoreFuture<'_, Customer>;

    /// Find by local id.
    fn customer_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Customer>>;

    /// Find the customer linked to `user_id`.
    fn customer_by_user<'a>(&'a self, user_id: &'a str) -> StoreFuture<'a, Option<Customer>>;

    /// Find by provider customer id.
    fn customer_by_provider_id<'a>(
        &'a self,
        provider_customer_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Customer>>;

    // ── Subscriptions ───────────────────────────────────────────────────

    /// Guarded insert or update.
    fn upsert_subscription(
        &self,
        upsert: SubscriptionUpsert,
    ) -> StoreFuture<'_, Write<Subscription>>;

    /// Find by local id.
    fn subscription_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Subscription>>;

    /// Find by provider subscription id.
    fn subscription_by_provider_id<'a>(
        &'a self,
        provider_subscription_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Subscription>>;

    /// All subscriptions of a customer, newest `last_event_at` first.
    fn subscriptions_for_customer<'a>(
        &'a self,
        customer_id: &'a str,
    ) -> StoreFuture<'a, Vec<Subscription>>;

    /// Set the status without an ordering guard (local decision, e.g. dunning exhausted).
    fn set_subscription_status<'a>(
        &'a self,
        id: &'a str,
        status: SubscriptionStatus,
        now: DateTime<Utc>,
    ) -> StoreFuture<'a, Option<Subscription>>;

    // ── Invoices ────────────────────────────────────────────────────────

    /// Guarded insert or update.
    fn upsert_invoice(&self, upsert: InvoiceUpsert) -> StoreFuture<'_, Write<Invoice>>;

    /// Find by local id.
    fn invoice_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Invoice>>;

    /// Find by provider invoice id.
    fn invoice_by_provider_id<'a>(
        &'a self,
        provider_invoice_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Invoice>>;

    // ── Dunning ─────────────────────────────────────────────────────────

    /// Insert or replace the schedule row for `attempt.invoice_id`.
    fn upsert_dunning(&self, attempt: DunningAttempt) -> StoreFuture<'_, ()>;

    /// The schedule row for a local invoice id.
    fn dunning_by_invoice<'a>(
        &'a self,
        invoice_id: &'a str,
    ) -> StoreFuture<'a, Option<DunningAttempt>>;

    /// Compare-and-set `Pending` → `Running` when the row's attempt equals
    /// `attempt`. Returns `true` when this caller won.
    fn claim_dunning_attempt<'a>(
        &'a self,
        invoice_id: &'a str,
        attempt: i64,
        now: DateTime<Utc>,
    ) -> StoreFuture<'a, bool>;

    /// Every row in `Pending` or `Running`, ordered by `next_attempt_at`.
    fn open_dunning(&self) -> StoreFuture<'_, Vec<DunningAttempt>>;
}
