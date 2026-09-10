//! Database-backed [`BillingStore`] over `autumn_web::RuntimeConnection`.
//!
//! Backend-portable diesel only (`Text`, `BigInt`, `Timestamp`); no
//! Postgres-only SQL, so the Postgres and `SQLite` lanes both compile. The
//! matching migration is `migrations/20260910000000_billing_mirror`.

use std::time::Duration;

use autumn_web::RuntimeConnection;
use chrono::{DateTime, Utc};
use diesel_async::pooled_connection::deadpool::Pool;

use super::{
    BillingStore, CustomerUpsert, EventClaim, InvoiceUpsert, StoreFuture, SubscriptionUpsert, Write,
};
use crate::error::BillingError;
use crate::model::{
    Customer, DunningAttempt, Invoice, ProviderId, Subscription, SubscriptionStatus,
};

/// Database mirror store.
pub struct DbBillingStore {
    pool: Pool<RuntimeConnection>,
}

impl std::fmt::Debug for DbBillingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbBillingStore").finish_non_exhaustive()
    }
}

impl DbBillingStore {
    /// Build over the app's primary pool.
    #[must_use]
    pub const fn new(pool: Pool<RuntimeConnection>) -> Self {
        Self { pool }
    }

    #[allow(dead_code, reason = "used once the queries are implemented")]
    const fn pool(&self) -> &Pool<RuntimeConnection> {
        &self.pool
    }
}

fn unsupported<'a, T: Send + 'a>() -> StoreFuture<'a, T> {
    Box::pin(async { Err(BillingError::Unsupported("db store")) })
}

impl BillingStore for DbBillingStore {
    fn claim_event<'a>(
        &'a self,
        _event_id: &'a str,
        _kind: &'a str,
        _now: DateTime<Utc>,
        _stale_after: Duration,
    ) -> StoreFuture<'a, EventClaim> {
        unsupported()
    }

    fn finish_event<'a>(&'a self, _event_id: &'a str, _now: DateTime<Utc>) -> StoreFuture<'a, ()> {
        unsupported()
    }

    fn release_event<'a>(&'a self, _event_id: &'a str) -> StoreFuture<'a, ()> {
        unsupported()
    }

    fn applied_event_count(&self) -> StoreFuture<'_, u64> {
        unsupported()
    }

    fn upsert_customer(&self, _upsert: CustomerUpsert) -> StoreFuture<'_, Customer> {
        unsupported()
    }

    fn customer_by_id<'a>(&'a self, _id: &'a str) -> StoreFuture<'a, Option<Customer>> {
        unsupported()
    }

    fn customer_by_user<'a>(&'a self, _user_id: &'a str) -> StoreFuture<'a, Option<Customer>> {
        unsupported()
    }

    fn customer_by_provider_id<'a>(
        &'a self,
        _provider_customer_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Customer>> {
        unsupported()
    }

    fn upsert_subscription(
        &self,
        _upsert: SubscriptionUpsert,
    ) -> StoreFuture<'_, Write<Subscription>> {
        unsupported()
    }

    fn subscription_by_id<'a>(&'a self, _id: &'a str) -> StoreFuture<'a, Option<Subscription>> {
        unsupported()
    }

    fn subscription_by_provider_id<'a>(
        &'a self,
        _provider_subscription_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Subscription>> {
        unsupported()
    }

    fn subscriptions_for_customer<'a>(
        &'a self,
        _customer_id: &'a str,
    ) -> StoreFuture<'a, Vec<Subscription>> {
        unsupported()
    }

    fn set_subscription_status<'a>(
        &'a self,
        _id: &'a str,
        _status: SubscriptionStatus,
        _now: DateTime<Utc>,
    ) -> StoreFuture<'a, Option<Subscription>> {
        unsupported()
    }

    fn upsert_invoice(&self, _upsert: InvoiceUpsert) -> StoreFuture<'_, Write<Invoice>> {
        unsupported()
    }

    fn invoice_by_id<'a>(&'a self, _id: &'a str) -> StoreFuture<'a, Option<Invoice>> {
        unsupported()
    }

    fn invoice_by_provider_id<'a>(
        &'a self,
        _provider_invoice_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Invoice>> {
        unsupported()
    }

    fn upsert_dunning(&self, _attempt: DunningAttempt) -> StoreFuture<'_, ()> {
        unsupported()
    }

    fn dunning_by_invoice<'a>(
        &'a self,
        _invoice_id: &'a str,
    ) -> StoreFuture<'a, Option<DunningAttempt>> {
        unsupported()
    }

    fn claim_dunning_attempt<'a>(
        &'a self,
        _invoice_id: &'a str,
        _attempt: i64,
        _now: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        unsupported()
    }

    fn open_dunning(&self) -> StoreFuture<'_, Vec<DunningAttempt>> {
        unsupported()
    }
}
