//! Database-backed [`BillingStore`] over `autumn_web::RuntimeConnection`.
//!
//! Backend-portable diesel only (`Text`, `BigInt`, `Timestamp`); no
//! Postgres-only SQL, so the Postgres and `SQLite` lanes both compile. The
//! matching migration is `migrations/20260910000000_billing_mirror`.
//!
//! Every guarded upsert runs select-then-write inside one transaction. The
//! ledger claim and the dunning claim are single conditional statements, so
//! two processes never both win.

use std::time::Duration;

use autumn_web::RuntimeConnection;
use chrono::{DateTime, NaiveDateTime, Utc};
use diesel::prelude::*;
use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncConnection, RunQueryDsl};

use super::{
    BillingStore, CustomerUpsert, EventClaim, InvoiceUpsert, StoreFuture, SubscriptionUpsert,
    Write, should_apply,
};
use crate::error::BillingError;
use crate::model::{
    Customer, DunningAttempt, DunningState, Invoice, InvoiceStatus, ProviderId, Subscription,
    SubscriptionStatus,
};
use crate::money::{Currency, Money, MoneyError};
use crate::plan::PlanId;

// ── Schema ──────────────────────────────────────────────────────────────
//
// Mirrors `migrations/20260910000000_billing_mirror/up.sql`. Booleans are
// `BigInt` 0/1 and timestamps are `Timestamp` (UTC, no zone) so both lanes
// share one schema.

diesel::table! {
    billing_customers (id) {
        id -> Text,
        user_id -> Nullable<Text>,
        provider -> Text,
        provider_customer_id -> Text,
        email -> Nullable<Text>,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    billing_subscriptions (id) {
        id -> Text,
        customer_id -> Text,
        provider_subscription_id -> Text,
        provider_price_id -> Nullable<Text>,
        plan_id -> Nullable<Text>,
        status -> Text,
        quantity -> BigInt,
        current_period_end -> Nullable<Timestamp>,
        cancel_at_period_end -> BigInt,
        last_event_at -> Timestamp,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    billing_invoices (id) {
        id -> Text,
        customer_id -> Text,
        subscription_id -> Nullable<Text>,
        provider_invoice_id -> Text,
        status -> Text,
        amount_due_minor -> BigInt,
        amount_paid_minor -> BigInt,
        currency -> Text,
        attempt_count -> BigInt,
        next_payment_attempt -> Nullable<Timestamp>,
        last_event_at -> Timestamp,
        created_at -> Timestamp,
        updated_at -> Timestamp,
    }
}

diesel::table! {
    billing_events (provider_event_id) {
        provider_event_id -> Text,
        kind -> Text,
        claimed_at -> Timestamp,
        applied_at -> Nullable<Timestamp>,
    }
}

diesel::table! {
    billing_dunning (invoice_id) {
        invoice_id -> Text,
        customer_id -> Text,
        subscription_id -> Nullable<Text>,
        attempt -> BigInt,
        next_attempt_at -> Timestamp,
        state -> Text,
        updated_at -> Timestamp,
    }
}

// ── Rows ────────────────────────────────────────────────────────────────

#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = billing_customers)]
#[diesel(treat_none_as_null = true)]
struct CustomerRow {
    id: String,
    user_id: Option<String>,
    provider: String,
    provider_customer_id: String,
    email: Option<String>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = billing_subscriptions)]
#[diesel(treat_none_as_null = true)]
struct SubscriptionRow {
    id: String,
    customer_id: String,
    provider_subscription_id: String,
    provider_price_id: Option<String>,
    plan_id: Option<String>,
    status: String,
    quantity: i64,
    current_period_end: Option<NaiveDateTime>,
    cancel_at_period_end: i64,
    last_event_at: NaiveDateTime,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = billing_invoices)]
#[diesel(treat_none_as_null = true)]
struct InvoiceRow {
    id: String,
    customer_id: String,
    subscription_id: Option<String>,
    provider_invoice_id: String,
    status: String,
    amount_due_minor: i64,
    amount_paid_minor: i64,
    currency: String,
    attempt_count: i64,
    next_payment_attempt: Option<NaiveDateTime>,
    last_event_at: NaiveDateTime,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = billing_events)]
#[diesel(treat_none_as_null = true)]
struct EventRow {
    provider_event_id: String,
    kind: String,
    claimed_at: NaiveDateTime,
    applied_at: Option<NaiveDateTime>,
}

#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = billing_dunning)]
#[diesel(treat_none_as_null = true)]
struct DunningRow {
    invoice_id: String,
    customer_id: String,
    subscription_id: Option<String>,
    attempt: i64,
    next_attempt_at: NaiveDateTime,
    state: String,
    updated_at: NaiveDateTime,
}

// ── Conversions ─────────────────────────────────────────────────────────

const fn to_naive(at: DateTime<Utc>) -> NaiveDateTime {
    at.naive_utc()
}

const fn to_utc(at: NaiveDateTime) -> DateTime<Utc> {
    at.and_utc()
}

const fn bool_to_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn bad_stored(kind: &str, value: &str) -> BillingError {
    BillingError::store(format!("stored {kind} {value:?} is not valid"))
}

impl CustomerRow {
    fn into_model(self) -> Customer {
        Customer {
            id: self.id,
            user_id: self.user_id,
            provider: self.provider,
            provider_customer_id: ProviderId::new(self.provider_customer_id),
            email: self.email,
            created_at: to_utc(self.created_at),
            updated_at: to_utc(self.updated_at),
        }
    }
}

impl SubscriptionRow {
    fn from_upsert(id: String, created_at: DateTime<Utc>, upsert: SubscriptionUpsert) -> Self {
        Self {
            id,
            customer_id: upsert.customer_id,
            provider_subscription_id: upsert.provider_subscription_id.as_str().to_owned(),
            provider_price_id: upsert.provider_price_id.map(|p| p.as_str().to_owned()),
            plan_id: upsert.plan_id.map(|p| p.as_str().to_owned()),
            status: upsert.status.as_str().to_owned(),
            quantity: upsert.quantity,
            current_period_end: upsert.current_period_end.map(to_naive),
            cancel_at_period_end: bool_to_int(upsert.cancel_at_period_end),
            last_event_at: to_naive(upsert.occurred_at),
            created_at: to_naive(created_at),
            updated_at: to_naive(upsert.now),
        }
    }

    fn into_model(self) -> Result<Subscription, BillingError> {
        let status = SubscriptionStatus::parse(&self.status)
            .ok_or_else(|| bad_stored("subscription status", &self.status))?;
        Ok(Subscription {
            id: self.id,
            customer_id: self.customer_id,
            provider_subscription_id: ProviderId::new(self.provider_subscription_id),
            provider_price_id: self.provider_price_id.map(ProviderId::new),
            plan_id: self.plan_id.map(PlanId::new),
            status,
            quantity: self.quantity,
            current_period_end: self.current_period_end.map(to_utc),
            cancel_at_period_end: self.cancel_at_period_end != 0,
            last_event_at: to_utc(self.last_event_at),
            created_at: to_utc(self.created_at),
            updated_at: to_utc(self.updated_at),
        })
    }
}

impl InvoiceRow {
    fn from_upsert(
        id: String,
        created_at: DateTime<Utc>,
        subscription_id: Option<String>,
        upsert: InvoiceUpsert,
    ) -> Result<Self, BillingError> {
        let currency = upsert.amount_due.currency();
        if upsert.amount_paid.currency() != currency {
            return Err(
                MoneyError::CurrencyMismatch(currency, upsert.amount_paid.currency()).into(),
            );
        }
        Ok(Self {
            id,
            customer_id: upsert.customer_id,
            subscription_id,
            provider_invoice_id: upsert.provider_invoice_id.as_str().to_owned(),
            status: upsert.status.as_str().to_owned(),
            amount_due_minor: upsert.amount_due.minor(),
            amount_paid_minor: upsert.amount_paid.minor(),
            currency: currency.code().to_owned(),
            attempt_count: upsert.attempt_count,
            next_payment_attempt: upsert.next_payment_attempt.map(to_naive),
            last_event_at: to_naive(upsert.occurred_at),
            created_at: to_naive(created_at),
            updated_at: to_naive(upsert.now),
        })
    }

    fn into_model(self) -> Result<Invoice, BillingError> {
        let status = InvoiceStatus::parse(&self.status)
            .ok_or_else(|| bad_stored("invoice status", &self.status))?;
        let currency = Currency::new(&self.currency)?;
        Ok(Invoice {
            id: self.id,
            customer_id: self.customer_id,
            subscription_id: self.subscription_id,
            provider_invoice_id: ProviderId::new(self.provider_invoice_id),
            status,
            amount_due: Money::from_minor(self.amount_due_minor, currency),
            amount_paid: Money::from_minor(self.amount_paid_minor, currency),
            attempt_count: self.attempt_count,
            next_payment_attempt: self.next_payment_attempt.map(to_utc),
            last_event_at: to_utc(self.last_event_at),
            created_at: to_utc(self.created_at),
            updated_at: to_utc(self.updated_at),
        })
    }
}

impl DunningRow {
    fn from_model(attempt: DunningAttempt) -> Self {
        Self {
            invoice_id: attempt.invoice_id,
            customer_id: attempt.customer_id,
            subscription_id: attempt.subscription_id,
            attempt: attempt.attempt,
            next_attempt_at: to_naive(attempt.next_attempt_at),
            state: attempt.state.as_str().to_owned(),
            updated_at: to_naive(attempt.updated_at),
        }
    }

    fn into_model(self) -> Result<DunningAttempt, BillingError> {
        let state = DunningState::parse(&self.state)
            .ok_or_else(|| bad_stored("dunning state", &self.state))?;
        Ok(DunningAttempt {
            invoice_id: self.invoice_id,
            customer_id: self.customer_id,
            subscription_id: self.subscription_id,
            attempt: self.attempt,
            next_attempt_at: to_utc(self.next_attempt_at),
            state,
            updated_at: to_utc(self.updated_at),
        })
    }
}

// ── Errors ──────────────────────────────────────────────────────────────

/// Error inside a transaction closure: diesel's own, or a billing error
/// raised while decoding a row. `AsyncConnection::transaction` needs
/// `From<diesel::result::Error>`, which [`BillingError`] does not implement.
#[derive(Debug)]
enum TxError {
    Db(DieselError),
    Billing(BillingError),
}

impl From<DieselError> for TxError {
    fn from(err: DieselError) -> Self {
        Self::Db(err)
    }
}

impl From<BillingError> for TxError {
    fn from(err: BillingError) -> Self {
        Self::Billing(err)
    }
}

impl From<TxError> for BillingError {
    fn from(err: TxError) -> Self {
        match err {
            TxError::Db(err) => db_err(&err),
            TxError::Billing(err) => err,
        }
    }
}

/// Log the cause and map it to a `Store` error. The message names the
/// operation class only; the SQL detail stays in the log.
fn db_err(err: &dyn std::fmt::Display) -> BillingError {
    tracing::warn!(error = %err, "🍂 Autumn Billing: mirror store query failed");
    BillingError::store(format!("database query failed: {err}"))
}

const fn is_unique_violation(err: &DieselError) -> bool {
    matches!(
        err,
        DieselError::DatabaseError(DatabaseErrorKind::UniqueViolation, _)
    )
}

// ── Store ───────────────────────────────────────────────────────────────

/// Database mirror store.
pub struct DbBillingStore {
    pool: Pool<RuntimeConnection>,
}

impl std::fmt::Debug for DbBillingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbBillingStore").finish_non_exhaustive()
    }
}

type PooledConn = diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>;

impl DbBillingStore {
    /// Build over the app's primary pool.
    #[must_use]
    pub const fn new(pool: Pool<RuntimeConnection>) -> Self {
        Self { pool }
    }

    async fn conn(&self) -> Result<PooledConn, BillingError> {
        self.pool.get().await.map_err(|err| db_err(&err))
    }
}

/// The cutoff before which a `processing` claim counts as abandoned.
fn stale_cutoff(now: DateTime<Utc>, stale_after: Duration) -> Option<NaiveDateTime> {
    chrono::Duration::from_std(stale_after)
        .ok()
        .and_then(|d| now.checked_sub_signed(d))
        .map(to_naive)
}

impl BillingStore for DbBillingStore {
    fn claim_event<'a>(
        &'a self,
        event_id: &'a str,
        kind: &'a str,
        now: DateTime<Utc>,
        stale_after: Duration,
    ) -> StoreFuture<'a, EventClaim> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let existing: Option<Option<NaiveDateTime>> = billing_events::table
                .find(event_id)
                .select(billing_events::applied_at)
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            match existing {
                // First delivery: insert-if-absent. A concurrent insert of the
                // same id loses on the primary key and is a duplicate.
                None => {
                    let row = EventRow {
                        provider_event_id: event_id.to_owned(),
                        kind: kind.to_owned(),
                        claimed_at: to_naive(now),
                        applied_at: None,
                    };
                    match diesel::insert_into(billing_events::table)
                        .values(&row)
                        .execute(&mut conn)
                        .await
                    {
                        Ok(_) => Ok(EventClaim::Claimed),
                        Err(err) if is_unique_violation(&err) => Ok(EventClaim::Duplicate),
                        Err(err) => Err(db_err(&err)),
                    }
                }
                Some(Some(_)) => Ok(EventClaim::Duplicate),
                // In flight: re-claim only when the claim is stale. One
                // conditional update, so two re-claimers cannot both win.
                Some(None) => {
                    let Some(cutoff) = stale_cutoff(now, stale_after) else {
                        return Ok(EventClaim::Duplicate);
                    };
                    let updated = diesel::update(
                        billing_events::table.filter(
                            billing_events::provider_event_id
                                .eq(event_id)
                                .and(billing_events::applied_at.is_null())
                                .and(billing_events::claimed_at.le(cutoff)),
                        ),
                    )
                    .set(billing_events::claimed_at.eq(to_naive(now)))
                    .execute(&mut conn)
                    .await
                    .map_err(|err| db_err(&err))?;
                    Ok(if updated == 1 {
                        EventClaim::Claimed
                    } else {
                        EventClaim::Duplicate
                    })
                }
            }
        })
    }

    fn finish_event<'a>(&'a self, event_id: &'a str, now: DateTime<Utc>) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            diesel::update(billing_events::table.find(event_id))
                .set(billing_events::applied_at.eq(Some(to_naive(now))))
                .execute(&mut conn)
                .await
                .map_err(|err| db_err(&err))?;
            Ok(())
        })
    }

    fn release_event<'a>(&'a self, event_id: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            diesel::delete(
                billing_events::table.filter(
                    billing_events::provider_event_id
                        .eq(event_id)
                        .and(billing_events::applied_at.is_null()),
                ),
            )
            .execute(&mut conn)
            .await
            .map_err(|err| db_err(&err))?;
            Ok(())
        })
    }

    fn applied_event_count(&self) -> StoreFuture<'_, u64> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let count: i64 = billing_events::table
                .filter(billing_events::applied_at.is_not_null())
                .count()
                .get_result(&mut conn)
                .await
                .map_err(|err| db_err(&err))?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
    }

    fn upsert_customer(&self, upsert: CustomerUpsert) -> StoreFuture<'_, Customer> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: CustomerRow = conn
                .transaction(async move |conn| -> Result<CustomerRow, TxError> {
                    let existing: Option<CustomerRow> = billing_customers::table
                        .filter(
                            billing_customers::provider_customer_id
                                .eq(upsert.provider_customer_id.as_str()),
                        )
                        .select(CustomerRow::as_select())
                        .first(conn)
                        .await
                        .optional()?;
                    let Some(mut current) = existing else {
                        let row = CustomerRow {
                            id: upsert.new_id,
                            user_id: upsert.user_id,
                            provider: upsert.provider,
                            provider_customer_id: upsert.provider_customer_id.as_str().to_owned(),
                            email: upsert.email,
                            created_at: to_naive(upsert.now),
                            updated_at: to_naive(upsert.now),
                        };
                        diesel::insert_into(billing_customers::table)
                            .values(&row)
                            .execute(conn)
                            .await?;
                        return Ok(row);
                    };
                    // A link is set once; an email is replaced.
                    if current.user_id.is_none() && upsert.user_id.is_some() {
                        current.user_id = upsert.user_id;
                    }
                    if upsert.email.is_some() {
                        current.email = upsert.email;
                    }
                    current.updated_at = to_naive(upsert.now);
                    diesel::update(billing_customers::table.find(&current.id))
                        .set(&current)
                        .execute(conn)
                        .await?;
                    Ok(current)
                })
                .await?;
            Ok(row.into_model())
        })
    }

    fn customer_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Customer>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<CustomerRow> = billing_customers::table
                .find(id)
                .select(CustomerRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            Ok(row.map(CustomerRow::into_model))
        })
    }

    fn customer_by_user<'a>(&'a self, user_id: &'a str) -> StoreFuture<'a, Option<Customer>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<CustomerRow> = billing_customers::table
                .filter(billing_customers::user_id.eq(user_id))
                .order(billing_customers::created_at.asc())
                .select(CustomerRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            Ok(row.map(CustomerRow::into_model))
        })
    }

    fn customer_by_provider_id<'a>(
        &'a self,
        provider_customer_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Customer>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<CustomerRow> = billing_customers::table
                .filter(billing_customers::provider_customer_id.eq(provider_customer_id.as_str()))
                .select(CustomerRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            Ok(row.map(CustomerRow::into_model))
        })
    }

    fn upsert_subscription(
        &self,
        upsert: SubscriptionUpsert,
    ) -> StoreFuture<'_, Write<Subscription>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let write: Write<SubscriptionRow> = conn
                .transaction(
                    async move |conn| -> Result<Write<SubscriptionRow>, TxError> {
                        let existing: Option<SubscriptionRow> = billing_subscriptions::table
                            .filter(
                                billing_subscriptions::provider_subscription_id
                                    .eq(upsert.provider_subscription_id.as_str()),
                            )
                            .select(SubscriptionRow::as_select())
                            .first(conn)
                            .await
                            .optional()?;
                        let Some(current) = existing else {
                            let row = SubscriptionRow::from_upsert(
                                upsert.new_id.clone(),
                                upsert.now,
                                upsert,
                            );
                            diesel::insert_into(billing_subscriptions::table)
                                .values(&row)
                                .execute(conn)
                                .await?;
                            return Ok(Write::Applied(row));
                        };
                        let current_status = SubscriptionStatus::parse(&current.status)
                            .ok_or_else(|| bad_stored("subscription status", &current.status))?;
                        if !should_apply(
                            to_utc(current.last_event_at),
                            current_status.rank(),
                            current_status.is_terminal(),
                            upsert.occurred_at,
                            upsert.status.rank(),
                        ) {
                            return Ok(Write::Stale(current));
                        }
                        let row = SubscriptionRow::from_upsert(
                            current.id.clone(),
                            to_utc(current.created_at),
                            upsert,
                        );
                        diesel::update(billing_subscriptions::table.find(&row.id))
                            .set(&row)
                            .execute(conn)
                            .await?;
                        Ok(Write::Applied(row))
                    },
                )
                .await?;
            Ok(match write {
                Write::Applied(row) => Write::Applied(row.into_model()?),
                Write::Stale(row) => Write::Stale(row.into_model()?),
            })
        })
    }

    fn subscription_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Subscription>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<SubscriptionRow> = billing_subscriptions::table
                .find(id)
                .select(SubscriptionRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            row.map(SubscriptionRow::into_model).transpose()
        })
    }

    fn subscription_by_provider_id<'a>(
        &'a self,
        provider_subscription_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Subscription>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<SubscriptionRow> = billing_subscriptions::table
                .filter(
                    billing_subscriptions::provider_subscription_id
                        .eq(provider_subscription_id.as_str()),
                )
                .select(SubscriptionRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            row.map(SubscriptionRow::into_model).transpose()
        })
    }

    fn subscriptions_for_customer<'a>(
        &'a self,
        customer_id: &'a str,
    ) -> StoreFuture<'a, Vec<Subscription>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let rows: Vec<SubscriptionRow> = billing_subscriptions::table
                .filter(billing_subscriptions::customer_id.eq(customer_id))
                .order((
                    billing_subscriptions::last_event_at.desc(),
                    billing_subscriptions::id.asc(),
                ))
                .select(SubscriptionRow::as_select())
                .load(&mut conn)
                .await
                .map_err(|err| db_err(&err))?;
            rows.into_iter().map(SubscriptionRow::into_model).collect()
        })
    }

    fn set_subscription_status<'a>(
        &'a self,
        id: &'a str,
        status: SubscriptionStatus,
        now: DateTime<Utc>,
    ) -> StoreFuture<'a, Option<Subscription>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let updated = diesel::update(billing_subscriptions::table.find(id))
                .set((
                    billing_subscriptions::status.eq(status.as_str()),
                    billing_subscriptions::updated_at.eq(to_naive(now)),
                ))
                .execute(&mut conn)
                .await
                .map_err(|err| db_err(&err))?;
            if updated == 0 {
                return Ok(None);
            }
            let row: Option<SubscriptionRow> = billing_subscriptions::table
                .find(id)
                .select(SubscriptionRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            row.map(SubscriptionRow::into_model).transpose()
        })
    }

    fn upsert_invoice(&self, upsert: InvoiceUpsert) -> StoreFuture<'_, Write<Invoice>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let write: Write<InvoiceRow> = conn
                .transaction(async move |conn| -> Result<Write<InvoiceRow>, TxError> {
                    let existing: Option<InvoiceRow> = billing_invoices::table
                        .filter(
                            billing_invoices::provider_invoice_id
                                .eq(upsert.provider_invoice_id.as_str()),
                        )
                        .select(InvoiceRow::as_select())
                        .first(conn)
                        .await
                        .optional()?;
                    let Some(current) = existing else {
                        let row = InvoiceRow::from_upsert(
                            upsert.new_id.clone(),
                            upsert.now,
                            upsert.subscription_id.clone(),
                            upsert,
                        )?;
                        diesel::insert_into(billing_invoices::table)
                            .values(&row)
                            .execute(conn)
                            .await?;
                        return Ok(Write::Applied(row));
                    };
                    let current_status = InvoiceStatus::parse(&current.status)
                        .ok_or_else(|| bad_stored("invoice status", &current.status))?;
                    if !should_apply(
                        to_utc(current.last_event_at),
                        current_status.rank(),
                        false,
                        upsert.occurred_at,
                        upsert.status.rank(),
                    ) {
                        return Ok(Write::Stale(current));
                    }
                    // `None` keeps the stored link.
                    let subscription_id =
                        upsert.subscription_id.clone().or(current.subscription_id);
                    let row = InvoiceRow::from_upsert(
                        current.id.clone(),
                        to_utc(current.created_at),
                        subscription_id,
                        upsert,
                    )?;
                    diesel::update(billing_invoices::table.find(&row.id))
                        .set(&row)
                        .execute(conn)
                        .await?;
                    Ok(Write::Applied(row))
                })
                .await?;
            Ok(match write {
                Write::Applied(row) => Write::Applied(row.into_model()?),
                Write::Stale(row) => Write::Stale(row.into_model()?),
            })
        })
    }

    fn invoice_by_id<'a>(&'a self, id: &'a str) -> StoreFuture<'a, Option<Invoice>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<InvoiceRow> = billing_invoices::table
                .find(id)
                .select(InvoiceRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            row.map(InvoiceRow::into_model).transpose()
        })
    }

    fn invoice_by_provider_id<'a>(
        &'a self,
        provider_invoice_id: &'a ProviderId,
    ) -> StoreFuture<'a, Option<Invoice>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<InvoiceRow> = billing_invoices::table
                .filter(billing_invoices::provider_invoice_id.eq(provider_invoice_id.as_str()))
                .select(InvoiceRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            row.map(InvoiceRow::into_model).transpose()
        })
    }

    fn upsert_dunning(&self, attempt: DunningAttempt) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row = DunningRow::from_model(attempt);
            conn.transaction(async move |conn| -> Result<(), TxError> {
                let updated = diesel::update(billing_dunning::table.find(&row.invoice_id))
                    .set(&row)
                    .execute(conn)
                    .await?;
                if updated == 0 {
                    diesel::insert_into(billing_dunning::table)
                        .values(&row)
                        .execute(conn)
                        .await?;
                }
                Ok(())
            })
            .await?;
            Ok(())
        })
    }

    fn dunning_by_invoice<'a>(
        &'a self,
        invoice_id: &'a str,
    ) -> StoreFuture<'a, Option<DunningAttempt>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let row: Option<DunningRow> = billing_dunning::table
                .find(invoice_id)
                .select(DunningRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(|err| db_err(&err))?;
            row.map(DunningRow::into_model).transpose()
        })
    }

    fn claim_dunning_attempt<'a>(
        &'a self,
        invoice_id: &'a str,
        attempt: i64,
        now: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            // One conditional update is the compare-and-set: only a row still
            // in `pending` at this attempt number changes, so a second caller
            // touches zero rows.
            let updated = diesel::update(
                billing_dunning::table.filter(
                    billing_dunning::invoice_id
                        .eq(invoice_id)
                        .and(billing_dunning::state.eq(DunningState::Pending.as_str()))
                        .and(billing_dunning::attempt.eq(attempt)),
                ),
            )
            .set((
                billing_dunning::state.eq(DunningState::Running.as_str()),
                billing_dunning::updated_at.eq(to_naive(now)),
            ))
            .execute(&mut conn)
            .await
            .map_err(|err| db_err(&err))?;
            Ok(updated == 1)
        })
    }

    fn open_dunning(&self) -> StoreFuture<'_, Vec<DunningAttempt>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let rows: Vec<DunningRow> = billing_dunning::table
                .filter(billing_dunning::state.eq_any([
                    DunningState::Pending.as_str(),
                    DunningState::Running.as_str(),
                ]))
                .order((
                    billing_dunning::next_attempt_at.asc(),
                    billing_dunning::invoice_id.asc(),
                ))
                .select(DunningRow::as_select())
                .load(&mut conn)
                .await
                .map_err(|err| db_err(&err))?;
            rows.into_iter().map(DunningRow::into_model).collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_cutoff_is_now_minus_stale_after() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).expect("valid");
        let cutoff = stale_cutoff(now, Duration::from_secs(300)).expect("cutoff");
        assert_eq!(cutoff, to_naive(now) - chrono::Duration::seconds(300));
        assert!(stale_cutoff(now, Duration::MAX).is_none());
    }

    #[test]
    fn bool_round_trips_through_bigint() {
        assert_eq!(bool_to_int(true), 1);
        assert_eq!(bool_to_int(false), 0);
    }
}
