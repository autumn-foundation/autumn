// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]
//! Apply a [`BillingEvent`] to the mirror, once.
//!
//! Sequence: claim the event id in the ledger → upsert snapshots (the store
//! applies the ordering guard) → open or close dunning → notify → mark
//! applied. A failure after the claim releases it, so the provider's
//! redelivery is applied again; every step is idempotent. A redelivery of a
//! snapshot already in the mirror (`Write::Unchanged`) repeats the idempotent
//! steps (dunning row, job) but not notifications or hooks.

use autumn_web::AppState;
use chrono::{DateTime, Utc};

use crate::dunning;
use crate::error::BillingError;
use crate::event::{
    BillingEvent, BillingEventKind, CheckoutSnapshot, InvoiceSnapshot, SubscriptionSnapshot,
};
use crate::model::{Customer, DunningAttempt, DunningState, Invoice, SubscriptionStatus};
use crate::notify;
use crate::store::{
    CustomerUpsert, EventClaim, InvoiceUpsert, SubscriptionUpsert, Write as StoreWrite,
};
use crate::{BillingService, EVENT_CLAIM_STALE_AFTER};

/// The states a reconcile may close.
const OPEN_STATES: &[DunningState] = &[DunningState::Pending, DunningState::Running];

/// What `apply` did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReconcileOutcome {
    /// The event changed, or confirmed, mirror state.
    Applied {
        /// Ledger label of the event kind.
        kind: &'static str,
    },
    /// The event id was already applied. Nothing changed.
    Duplicate,
    /// The provider event type is not mirrored. Recorded only.
    Ignored {
        /// Provider event type.
        event_type: String,
    },
}

/// Apply one event.
///
/// # Errors
///
/// Returns the store or hook error. The ledger claim is released first.
pub async fn apply(
    state: &AppState,
    service: &BillingService,
    event: BillingEvent,
) -> Result<ReconcileOutcome, BillingError> {
    let now = state.clock().now();
    let store = service.store();
    let kind = event.kind.label();
    let claim = store
        .claim_event(&event.id, kind, now, EVENT_CLAIM_STALE_AFTER)
        .await?;
    if claim == EventClaim::Duplicate {
        tracing::debug!(event_id = %event.id, kind, "🍂 Autumn Billing: duplicate event");
        return Ok(ReconcileOutcome::Duplicate);
    }
    let event_id = event.id.clone();
    let ctx = Ctx {
        state,
        service,
        now,
    };
    match ctx.apply_claimed(event).await {
        Ok(outcome) => {
            store.finish_event(&event_id, now).await?;
            Ok(outcome)
        }
        Err(error) => {
            tracing::warn!(
                event_id = %event_id,
                kind,
                error = %error,
                "🍂 Autumn Billing: event not applied; claim released for redelivery"
            );
            if let Err(release) = store.release_event(&event_id).await {
                tracing::error!(
                    event_id = %event_id,
                    error = %release,
                    "🍂 Autumn Billing: could not release the event claim"
                );
            }
            Err(error)
        }
    }
}

/// One claimed event in flight.
struct Ctx<'a> {
    state: &'a AppState,
    service: &'a BillingService,
    now: DateTime<Utc>,
}

impl Ctx<'_> {
    fn new_id(&self) -> String {
        self.state.entropy().uuid_v4().to_string()
    }

    async fn apply_claimed(&self, event: BillingEvent) -> Result<ReconcileOutcome, BillingError> {
        let kind = event.kind.label();
        let occurred_at = event.occurred_at;
        match event.kind {
            BillingEventKind::Ignored { event_type } => {
                return Ok(ReconcileOutcome::Ignored { event_type });
            }
            BillingEventKind::CheckoutCompleted(snapshot) => self.checkout(&snapshot).await?,
            BillingEventKind::SubscriptionChanged(snapshot) => {
                self.subscription(&snapshot, snapshot.status, occurred_at)
                    .await?;
            }
            BillingEventKind::SubscriptionDeleted(snapshot) => {
                self.subscription(&snapshot, SubscriptionStatus::Canceled, occurred_at)
                    .await?;
            }
            BillingEventKind::InvoicePaymentFailed(snapshot) => {
                self.payment_failed(&snapshot, occurred_at).await?;
            }
            BillingEventKind::InvoicePaid(snapshot) => self.paid(&snapshot, occurred_at).await?,
        }
        Ok(ReconcileOutcome::Applied { kind })
    }

    /// The customer for a provider id: found, or created without a user link.
    /// `email` replaces the stored email when `Some`.
    async fn customer(
        &self,
        provider_customer_id: &crate::model::ProviderId,
        email: Option<&str>,
    ) -> Result<Customer, BillingError> {
        let mut upsert = CustomerUpsert::new(
            self.new_id(),
            self.service.provider().name(),
            provider_customer_id.clone(),
            self.now,
        );
        if let Some(email) = email {
            upsert = upsert.with_email(email);
        }
        self.service.store().upsert_customer(upsert).await
    }

    /// A completed checkout refreshes the customer email. The user link was
    /// made by the checkout route before the redirect; the event never links
    /// a user, and never by email.
    async fn checkout(&self, snapshot: &CheckoutSnapshot) -> Result<(), BillingError> {
        let store = self.service.store();
        let own_row = match snapshot.local_customer_ref.as_deref() {
            Some(local_id) => store
                .customer_by_id(local_id)
                .await?
                .filter(|row| row.provider_customer_id == snapshot.provider_customer_id),
            None => None,
        };
        match (&own_row, &snapshot.local_customer_ref) {
            (Some(row), _) => tracing::debug!(
                customer_id = %row.id,
                linked = row.user_id.is_some(),
                "🍂 Autumn Billing: checkout completed for a known customer"
            ),
            (None, Some(reference)) => tracing::warn!(
                reference,
                provider_customer_id = %snapshot.provider_customer_id,
                "🍂 Autumn Billing: checkout reference does not match a local customer; not linked"
            ),
            (None, None) => {}
        }
        self.customer(&snapshot.provider_customer_id, snapshot.email.as_deref())
            .await?;
        Ok(())
    }

    async fn subscription(
        &self,
        snapshot: &SubscriptionSnapshot,
        status: SubscriptionStatus,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), BillingError> {
        let store = self.service.store();
        let customer = self.customer(&snapshot.provider_customer_id, None).await?;
        let previous = store
            .subscription_by_provider_id(&snapshot.provider_subscription_id)
            .await?;
        let mut upsert = SubscriptionUpsert::new(
            self.new_id(),
            customer.id.clone(),
            snapshot.provider_subscription_id.clone(),
            status,
            occurred_at,
            self.now,
        )
        .with_quantity(snapshot.quantity)
        .with_cancel_at_period_end(snapshot.cancel_at_period_end);
        if let Some(price_id) = &snapshot.provider_price_id {
            upsert = upsert.with_price(price_id.clone());
            if let Some(plan) = self.service.catalog().by_price_id(price_id) {
                upsert = upsert.with_plan(plan.id.clone());
            } else {
                tracing::warn!(
                    price_id = %price_id,
                    "🍂 Autumn Billing: provider price is not in the plan catalog"
                );
            }
        }
        if let Some(end) = snapshot.current_period_end {
            upsert = upsert.with_period_end(end);
        }
        let subscription = match store.upsert_subscription(upsert).await? {
            StoreWrite::Applied(subscription) => subscription,
            // A redelivery after a failure past the write: repeat the
            // idempotent step only. Notifications and hooks ran, or never
            // will, with the first delivery.
            StoreWrite::Unchanged(subscription) => {
                if subscription.status == SubscriptionStatus::Canceled {
                    self.close_dunning_for(&subscription.id).await?;
                }
                return Ok(());
            }
            StoreWrite::Stale(_) => {
                tracing::debug!(
                    provider_subscription_id = %snapshot.provider_subscription_id,
                    "🍂 Autumn Billing: stale subscription event; mirror unchanged"
                );
                return Ok(());
            }
        };
        if subscription.status == SubscriptionStatus::Canceled {
            self.close_dunning_for(&subscription.id).await?;
            notify::send_to_customer(
                self.state,
                self.service,
                &customer,
                notify::KIND_SUBSCRIPTION_CANCELED,
                notify::subscription_canceled_payload(&subscription),
            )
            .await;
        }
        self.service
            .hooks()
            .on_subscription_changed(&subscription, previous.as_ref())
            .await;
        Ok(())
    }

    /// Cancel every open dunning row of `subscription_id`. Compare-and-set,
    /// so a retry that settles the row at the same time is not overwritten.
    async fn close_dunning_for(&self, subscription_id: &str) -> Result<(), BillingError> {
        let store = self.service.store();
        for row in store.open_dunning().await? {
            if row.subscription_id.as_deref() != Some(subscription_id) {
                continue;
            }
            let mut closed = row.clone();
            closed.state = DunningState::Canceled;
            closed.updated_at = self.now;
            let written = store
                .settle_dunning(&row.invoice_id, row.attempt, OPEN_STATES, closed)
                .await?;
            if !written {
                tracing::debug!(
                    invoice_id = %row.invoice_id,
                    "🍂 Autumn Billing: dunning row settled elsewhere; not closed"
                );
            }
        }
        Ok(())
    }

    /// Guarded invoice upsert from a snapshot.
    async fn upsert_invoice(
        &self,
        snapshot: &InvoiceSnapshot,
        occurred_at: DateTime<Utc>,
    ) -> Result<(Customer, StoreWrite<Invoice>), BillingError> {
        let store = self.service.store();
        let customer = self.customer(&snapshot.provider_customer_id, None).await?;
        let subscription = match &snapshot.provider_subscription_id {
            Some(id) => store.subscription_by_provider_id(id).await?,
            None => None,
        };
        let mut upsert = InvoiceUpsert::new(
            self.new_id(),
            customer.id.clone(),
            snapshot.provider_invoice_id.clone(),
            snapshot.status,
            snapshot.amount_due,
            snapshot.amount_paid,
            occurred_at,
            self.now,
        )
        .with_attempt_count(snapshot.attempt_count);
        if let Some(subscription) = subscription {
            upsert = upsert.with_subscription(subscription.id);
        }
        if let Some(next) = snapshot.next_payment_attempt {
            upsert = upsert.with_next_payment_attempt(next);
        }
        let write = store.upsert_invoice(upsert).await?;
        Ok((customer, write))
    }

    async fn payment_failed(
        &self,
        snapshot: &InvoiceSnapshot,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), BillingError> {
        let (customer, write) = self.upsert_invoice(snapshot, occurred_at).await?;
        let (invoice, applied) = match write {
            StoreWrite::Applied(invoice) => (invoice, true),
            StoreWrite::Unchanged(invoice) => (invoice, false),
            StoreWrite::Stale(_) => {
                tracing::debug!(
                    provider_invoice_id = %snapshot.provider_invoice_id,
                    "🍂 Autumn Billing: stale payment_failed event; mirror unchanged"
                );
                return Ok(());
            }
        };
        let policy = &self.service.config().dunning;
        // Idempotent: a redelivery makes sure the row exists and is queued.
        let row = if policy.enabled {
            self.open_dunning(&invoice).await?
        } else {
            None
        };
        if !applied {
            return Ok(());
        }
        notify::send_to_customer(
            self.state,
            self.service,
            &customer,
            notify::KIND_PAYMENT_FAILED,
            notify::payment_failed_payload(&invoice, row.as_ref().map(|r| r.attempt), None),
        )
        .await;
        if let Some(row) = &row {
            self.service.hooks().on_payment_failed(&invoice, row).await;
        }
        Ok(())
    }

    /// The open schedule row for `invoice`: kept when one is in progress,
    /// else a new first attempt. No row when the invoice's subscription has
    /// ended or is `unpaid`: there is nothing left to collect for.
    async fn open_dunning(
        &self,
        invoice: &Invoice,
    ) -> Result<Option<DunningAttempt>, BillingError> {
        let store = self.service.store();
        if let Some(subscription_id) = &invoice.subscription_id
            && let Some(subscription) = store.subscription_by_id(subscription_id).await?
            && (subscription.status.is_terminal()
                || subscription.status == SubscriptionStatus::Unpaid)
        {
            tracing::info!(
                invoice_id = %invoice.id,
                subscription_id,
                status = subscription.status.as_str(),
                "🍂 Autumn Billing: subscription ended; dunning not opened"
            );
            return Ok(None);
        }
        if let Some(mut row) = store.dunning_by_invoice(&invoice.id).await?
            && matches!(row.state, DunningState::Pending | DunningState::Running)
        {
            // The failure arrived before the subscription: back-fill the link.
            if row.subscription_id.is_none() && invoice.subscription_id.is_some() {
                let mut linked = row.clone();
                linked.subscription_id.clone_from(&invoice.subscription_id);
                linked.updated_at = self.now;
                if store
                    .settle_dunning(&row.invoice_id, row.attempt, &[row.state], linked.clone())
                    .await?
                {
                    row = linked;
                }
            }
            tracing::debug!(
                invoice_id = %invoice.id,
                attempt = row.attempt,
                "🍂 Autumn Billing: dunning already open; schedule kept"
            );
            // The unique pending window dedupes an already queued retry.
            if row.state == DunningState::Pending {
                dunning::schedule(self.state, &invoice.id, row.next_attempt_at).await?;
            }
            return Ok(Some(row));
        }
        let Some(delay) = self.service.config().dunning.delay_for(1) else {
            tracing::debug!(
                invoice_id = %invoice.id,
                "🍂 Autumn Billing: no retry delays configured; dunning not opened"
            );
            return Ok(None);
        };
        let due = dunning::due_at(self.now, delay)?;
        let mut row = DunningAttempt::new(
            invoice.id.clone(),
            invoice.customer_id.clone(),
            1,
            due,
            DunningState::Pending,
            self.now,
        );
        if let Some(subscription_id) = &invoice.subscription_id {
            row = row.with_subscription(subscription_id.clone());
        }
        store.upsert_dunning(row.clone()).await?;
        dunning::schedule(self.state, &invoice.id, due).await?;
        tracing::info!(
            invoice_id = %invoice.id,
            due = %due,
            "🍂 Autumn Billing: dunning opened"
        );
        Ok(Some(row))
    }

    async fn paid(
        &self,
        snapshot: &InvoiceSnapshot,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), BillingError> {
        let (customer, write) = self.upsert_invoice(snapshot, occurred_at).await?;
        let (invoice, applied) = match write {
            StoreWrite::Applied(invoice) => (invoice, true),
            StoreWrite::Unchanged(invoice) => (invoice, false),
            StoreWrite::Stale(_) => return Ok(()),
        };
        let store = self.service.store();
        let Some(row) = store.dunning_by_invoice(&invoice.id).await? else {
            return Ok(());
        };
        if !matches!(row.state, DunningState::Pending | DunningState::Running) {
            return Ok(());
        }
        let mut recovered = row.clone();
        recovered.state = DunningState::Recovered;
        recovered.updated_at = self.now;
        let written = store
            .settle_dunning(&row.invoice_id, row.attempt, OPEN_STATES, recovered)
            .await?;
        if !written || !applied {
            return Ok(());
        }
        notify::send_to_customer(
            self.state,
            self.service,
            &customer,
            notify::KIND_PAYMENT_RECOVERED,
            notify::payment_recovered_payload(&invoice),
        )
        .await;
        self.service.hooks().on_payment_recovered(&invoice).await;
        Ok(())
    }
}
