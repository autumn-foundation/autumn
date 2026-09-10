//! Process-local [`BillingStore`]. Lost on restart. Share one `Arc` between
//! test apps to model a restart.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{
    BillingStore, CustomerUpsert, EventClaim, InvoiceUpsert, StoreFuture, SubscriptionUpsert,
    Write, should_apply,
};
use crate::error::BillingError;
use crate::model::{
    Customer, DunningAttempt, DunningState, Invoice, ProviderId, Subscription, SubscriptionStatus,
};

#[derive(Debug, Clone)]
struct LedgerRow {
    #[allow(dead_code, reason = "diagnostic; read by the db store")]
    kind: String,
    claimed_at: DateTime<Utc>,
    applied_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default)]
struct Inner {
    events: BTreeMap<String, LedgerRow>,
    customers: BTreeMap<String, Customer>,
    subscriptions: BTreeMap<String, Subscription>,
    invoices: BTreeMap<String, Invoice>,
    dunning: BTreeMap<String, DunningAttempt>,
}

/// In-memory mirror store.
#[derive(Debug, Default)]
pub struct MemoryBillingStore {
    inner: Mutex<Inner>,
}

impl MemoryBillingStore {
    /// Build an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an empty store behind an `Arc`.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, BillingError> {
        self.inner
            .lock()
            .map_err(|_| BillingError::store("memory store mutex poisoned"))
    }
}

fn ready<'a, T: Send + 'a>(result: Result<T, BillingError>) -> StoreFuture<'a, T> {
    Box::pin(async move { result })
}

impl BillingStore for MemoryBillingStore {
    fn claim_event<'a>(
        &'a self,
        event_id: &'a str,
        kind: &'a str,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> StoreFuture<'a, EventClaim> {
        ready(self.lock().map(|mut inner| {
            let stale_after =
                chrono::Duration::from_std(stale_after).unwrap_or(chrono::Duration::MAX);
            match inner.events.get_mut(event_id) {
                Some(row) if row.applied_at.is_some() => EventClaim::Duplicate,
                Some(row) if now.signed_duration_since(row.claimed_at) < stale_after => {
                    EventClaim::Duplicate
                }
                Some(row) => {
                    row.claimed_at = now;
                    EventClaim::Claimed
                }
                None => {
                    inner.events.insert(
                        event_id.to_owned(),
                        LedgerRow {
                            kind: kind.to_owned(),
                            claimed_at: now,
                            applied_at: None,
                        },
                    );
                    EventClaim::Claimed
                }
            }
        }))
    }

    fn finish_event<'a>(&'a self, event_id: &'a str, now: DateTime<Utc>) -> StoreFuture<'a, ()> {
        ready(self.lock().map(|mut inner| {
            if let Some(row) = inner.events.get_mut(event_id) {
                row.applied_at = Some(now);
            }
        }))
    }

    fn release_event<'a>(&'a self, event_id: &'a str) -> StoreFuture<'a, ()> {
        ready(self.lock().map(|mut inner| {
            if inner
                .events
                .get(event_id)
                .is_some_and(|row| row.applied_at.is_none())
            {
                inner.events.remove(event_id);
            }
        }))
    }

    fn applied_event_count(&self) -> StoreFuture<'_, u64> {
        ready(self.lock().map(|inner| {
            u64::try_from(
                inner
                    .events
                    .values()
                    .filter(|row| row.applied_at.is_some())
                    .count(),
            )
            .unwrap_or(u64::MAX)
        }))
    }

    fn upsert_customer(&self, upsert: CustomerUpsert) -> StoreFuture<'_, Customer> {
        ready(self.lock().map(|mut inner| {
            let existing = inner
                .customers
                .values()
                .find(|c| c.provider_customer_id == upsert.provider_customer_id)
                .map(|c| c.id.clone());
            let id = existing.unwrap_or_else(|| upsert.new_id.clone());
            let row = inner
                .customers
                .entry(id.clone())
                .or_insert_with(|| Customer {
                    id: id.clone(),
                    user_id: None,
                    provider: upsert.provider.clone(),
                    provider_customer_id: upsert.provider_customer_id.clone(),
                    email: None,
                    created_at: upsert.now,
                    updated_at: upsert.now,
                });
            if row.user_id.is_none() && upsert.user_id.is_some() {
                row.user_id.clone_from(&upsert.user_id);
            }
            if upsert.email.is_some() {
                row.email.clone_from(&upsert.email);
            }
            row.updated_at = upsert.now;
            row.clone()
        }))
    }

    fn customer_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Customer>> {
        ready(self.lock().map(|inner| inner.customers.get(id).cloned()))
    }

    fn customer_by_user<'a>(&'a self, user_id: &'a str) -> StoreFuture<'a, Option<Customer>> {
        ready(self.lock().map(|inner| {
            inner
                .customers
                .values()
                .find(|c| c.user_id.as_deref() == Some(user_id))
                .cloned()
        }))
    }

    fn customer_by_provider_id<'a>(
        &'a self,
        provider_customer_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Customer>> {
        ready(self.lock().map(|inner| {
            inner
                .customers
                .values()
                .find(|c| &c.provider_customer_id == provider_customer_id)
                .cloned()
        }))
    }

    fn upsert_subscription(
        &self,
        upsert: SubscriptionUpsert,
    ) -> StoreFuture<'_, Write<Subscription>> {
        ready(self.lock().map(|mut inner| {
            let existing = inner
                .subscriptions
                .values()
                .find(|s| s.provider_subscription_id == upsert.provider_subscription_id)
                .cloned();
            if let Some(current) = existing {
                if !should_apply(
                    current.last_event_at,
                    current.status.rank(),
                    current.status.is_terminal(),
                    upsert.occurred_at,
                    upsert.status.rank(),
                ) {
                    return Write::Stale(current);
                }
                let row = Subscription {
                    id: current.id.clone(),
                    customer_id: upsert.customer_id,
                    provider_subscription_id: upsert.provider_subscription_id,
                    provider_price_id: upsert.provider_price_id,
                    plan_id: upsert.plan_id,
                    status: upsert.status,
                    quantity: upsert.quantity,
                    current_period_end: upsert.current_period_end,
                    cancel_at_period_end: upsert.cancel_at_period_end,
                    last_event_at: upsert.occurred_at,
                    created_at: current.created_at,
                    updated_at: upsert.now,
                };
                inner.subscriptions.insert(row.id.clone(), row.clone());
                return Write::Applied(row);
            }
            let row = Subscription {
                id: upsert.new_id,
                customer_id: upsert.customer_id,
                provider_subscription_id: upsert.provider_subscription_id,
                provider_price_id: upsert.provider_price_id,
                plan_id: upsert.plan_id,
                status: upsert.status,
                quantity: upsert.quantity,
                current_period_end: upsert.current_period_end,
                cancel_at_period_end: upsert.cancel_at_period_end,
                last_event_at: upsert.occurred_at,
                created_at: upsert.now,
                updated_at: upsert.now,
            };
            inner.subscriptions.insert(row.id.clone(), row.clone());
            Write::Applied(row)
        }))
    }

    fn subscription_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Subscription>> {
        ready(
            self.lock()
                .map(|inner| inner.subscriptions.get(id).cloned()),
        )
    }

    fn subscription_by_provider_id<'a>(
        &'a self,
        provider_subscription_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Subscription>> {
        ready(self.lock().map(|inner| {
            inner
                .subscriptions
                .values()
                .find(|s| &s.provider_subscription_id == provider_subscription_id)
                .cloned()
        }))
    }

    fn subscriptions_for_customer<'a>(
        &'a self,
        customer_id: &'a str,
    ) -> StoreFuture<'a, Vec<Subscription>> {
        ready(self.lock().map(|inner| {
            let mut rows: Vec<Subscription> = inner
                .subscriptions
                .values()
                .filter(|s| s.customer_id == customer_id)
                .cloned()
                .collect();
            rows.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
            rows
        }))
    }

    fn set_subscription_status<'a>(
        &'a self,
        id: &'a str,
        status: SubscriptionStatus,
        now: DateTime<Utc>,
    ) -> StoreFuture<'a, Option<Subscription>> {
        ready(self.lock().map(|mut inner| {
            let row = inner.subscriptions.get_mut(id)?;
            row.status = status;
            row.updated_at = now;
            Some(row.clone())
        }))
    }

    fn upsert_invoice(&self, upsert: InvoiceUpsert) -> StoreFuture<'_, Write<Invoice>> {
        ready(self.lock().map(|mut inner| {
            let existing = inner
                .invoices
                .values()
                .find(|i| i.provider_invoice_id == upsert.provider_invoice_id)
                .cloned();
            if let Some(current) = existing {
                if !should_apply(
                    current.last_event_at,
                    current.status.rank(),
                    false,
                    upsert.occurred_at,
                    upsert.status.rank(),
                ) {
                    return Write::Stale(current);
                }
                let row = Invoice {
                    id: current.id.clone(),
                    customer_id: upsert.customer_id,
                    subscription_id: upsert.subscription_id.or(current.subscription_id),
                    provider_invoice_id: upsert.provider_invoice_id,
                    status: upsert.status,
                    amount_due: upsert.amount_due,
                    amount_paid: upsert.amount_paid,
                    attempt_count: upsert.attempt_count,
                    next_payment_attempt: upsert.next_payment_attempt,
                    last_event_at: upsert.occurred_at,
                    created_at: current.created_at,
                    updated_at: upsert.now,
                };
                inner.invoices.insert(row.id.clone(), row.clone());
                return Write::Applied(row);
            }
            let row = Invoice {
                id: upsert.new_id,
                customer_id: upsert.customer_id,
                subscription_id: upsert.subscription_id,
                provider_invoice_id: upsert.provider_invoice_id,
                status: upsert.status,
                amount_due: upsert.amount_due,
                amount_paid: upsert.amount_paid,
                attempt_count: upsert.attempt_count,
                next_payment_attempt: upsert.next_payment_attempt,
                last_event_at: upsert.occurred_at,
                created_at: upsert.now,
                updated_at: upsert.now,
            };
            inner.invoices.insert(row.id.clone(), row.clone());
            Write::Applied(row)
        }))
    }

    fn invoice_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Invoice>> {
        ready(self.lock().map(|inner| inner.invoices.get(id).cloned()))
    }

    fn invoice_by_provider_id<'a>(
        &'a self,
        provider_invoice_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Invoice>> {
        ready(self.lock().map(|inner| {
            inner
                .invoices
                .values()
                .find(|i| &i.provider_invoice_id == provider_invoice_id)
                .cloned()
        }))
    }

    fn upsert_dunning(&self, attempt: DunningAttempt) -> StoreFuture<'_, ()> {
        ready(self.lock().map(|mut inner| {
            inner.dunning.insert(attempt.invoice_id.clone(), attempt);
        }))
    }

    fn dunning_by_invoice<'a>(
        &'a self,
        invoice_id: &'a str,
    ) -> StoreFuture<'a, Option<DunningAttempt>> {
        ready(
            self.lock()
                .map(|inner| inner.dunning.get(invoice_id).cloned()),
        )
    }

    fn claim_dunning_attempt<'a>(
        &'a self,
        invoice_id: &'a str,
        attempt: i64,
        now: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        ready(self.lock().map(|mut inner| {
            let Some(row) = inner.dunning.get_mut(invoice_id) else {
                return false;
            };
            if row.state != DunningState::Pending || row.attempt != attempt {
                return false;
            }
            row.state = DunningState::Running;
            row.updated_at = now;
            true
        }))
    }

    fn open_dunning(&self) -> StoreFuture<'_, Vec<DunningAttempt>> {
        ready(self.lock().map(|inner| {
            let mut rows: Vec<DunningAttempt> = inner
                .dunning
                .values()
                .filter(|d| matches!(d.state, DunningState::Pending | DunningState::Running))
                .cloned()
                .collect();
            rows.sort_by_key(|d| d.next_attempt_at);
            rows
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DunningState, SubscriptionStatus};
    use crate::plan::PlanId;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).single().unwrap()
    }

    fn upsert(status: SubscriptionStatus, occurred: i64) -> SubscriptionUpsert {
        SubscriptionUpsert {
            new_id: format!("local-{occurred}"),
            customer_id: "cust".into(),
            provider_subscription_id: ProviderId::new("sub_1"),
            provider_price_id: Some(ProviderId::new("price_pro")),
            plan_id: Some(PlanId::new("pro")),
            status,
            quantity: 1,
            current_period_end: None,
            cancel_at_period_end: false,
            occurred_at: at(occurred),
            now: at(1000),
        }
    }

    #[tokio::test]
    async fn claim_is_once_until_released() {
        let store = MemoryBillingStore::new();
        let ttl = Duration::from_secs(300);
        assert_eq!(
            store.claim_event("evt_1", "x", at(0), ttl).await.unwrap(),
            EventClaim::Claimed
        );
        assert_eq!(
            store.claim_event("evt_1", "x", at(1), ttl).await.unwrap(),
            EventClaim::Duplicate
        );
        store.release_event("evt_1").await.unwrap();
        assert_eq!(
            store.claim_event("evt_1", "x", at(2), ttl).await.unwrap(),
            EventClaim::Claimed
        );
        store.finish_event("evt_1", at(3)).await.unwrap();
        store.release_event("evt_1").await.unwrap();
        assert_eq!(
            store.claim_event("evt_1", "x", at(4), ttl).await.unwrap(),
            EventClaim::Duplicate
        );
        assert_eq!(store.applied_event_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn stale_processing_claim_is_reclaimable() {
        let store = MemoryBillingStore::new();
        let ttl = Duration::from_secs(300);
        store.claim_event("evt_1", "x", at(0), ttl).await.unwrap();
        assert_eq!(
            store.claim_event("evt_1", "x", at(301), ttl).await.unwrap(),
            EventClaim::Claimed
        );
    }

    #[tokio::test]
    async fn older_event_never_overwrites_newer_state() {
        let store = MemoryBillingStore::new();
        let deleted = store
            .upsert_subscription(upsert(SubscriptionStatus::Canceled, 200))
            .await
            .unwrap();
        assert!(deleted.is_applied());
        let late = store
            .upsert_subscription(upsert(SubscriptionStatus::Active, 100))
            .await
            .unwrap();
        assert!(!late.is_applied());
        assert_eq!(late.into_inner().status, SubscriptionStatus::Canceled);
    }

    #[tokio::test]
    async fn same_instant_prefers_higher_rank() {
        let store = MemoryBillingStore::new();
        store
            .upsert_subscription(upsert(SubscriptionStatus::Canceled, 100))
            .await
            .unwrap();
        let w = store
            .upsert_subscription(upsert(SubscriptionStatus::Active, 100))
            .await
            .unwrap();
        assert!(!w.is_applied());

        let store = MemoryBillingStore::new();
        store
            .upsert_subscription(upsert(SubscriptionStatus::Active, 100))
            .await
            .unwrap();
        let w = store
            .upsert_subscription(upsert(SubscriptionStatus::Canceled, 100))
            .await
            .unwrap();
        assert!(w.is_applied());
    }

    #[tokio::test]
    async fn customer_link_is_never_replaced() {
        let store = MemoryBillingStore::new();
        let first = CustomerUpsert::new("c1", "stripe", "cus_1", at(0)).with_user("7");
        let c = store.upsert_customer(first).await.unwrap();
        assert_eq!(c.user_id.as_deref(), Some("7"));
        let again = CustomerUpsert::new("c2", "stripe", "cus_1", at(1))
            .with_user("8")
            .with_email("a@b.c");
        let c = store.upsert_customer(again).await.unwrap();
        assert_eq!(c.id, "c1");
        assert_eq!(c.user_id.as_deref(), Some("7"));
        assert_eq!(c.email.as_deref(), Some("a@b.c"));
        assert!(store.customer_by_user("7").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn dunning_claim_is_compare_and_set() {
        let store = MemoryBillingStore::new();
        store
            .upsert_dunning(DunningAttempt {
                invoice_id: "inv".into(),
                customer_id: "c".into(),
                subscription_id: None,
                attempt: 1,
                next_attempt_at: at(10),
                state: DunningState::Pending,
                updated_at: at(0),
            })
            .await
            .unwrap();
        assert!(!store.claim_dunning_attempt("inv", 2, at(1)).await.unwrap());
        assert!(store.claim_dunning_attempt("inv", 1, at(1)).await.unwrap());
        assert!(!store.claim_dunning_attempt("inv", 1, at(2)).await.unwrap());
        assert_eq!(store.open_dunning().await.unwrap().len(), 1);
    }
}
