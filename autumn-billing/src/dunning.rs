//! Durable retries for failed payments.
//!
//! The `billing_dunning` row is the schedule. The job payload carries only the
//! local invoice id; the job reads the row, so a duplicate or early run is a
//! no-op and a restart re-arms from the store. Every write after the claim is
//! a compare-and-set on the row's state and attempt, so a reconcile that
//! settles the row while a retry is in flight wins.

use std::sync::Arc;
use std::time::Duration;

use autumn_web::job::{JobClient, JobInfo};
use autumn_web::{AppState, AutumnResult, job};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::BillingService;
use crate::config::ExhaustionAction;
use crate::error::BillingError;
use crate::model::{DunningAttempt, DunningState, Invoice, InvoiceStatus, SubscriptionStatus};
use crate::notify;
use crate::provider::PaymentAttemptOutcome;
use crate::store::InvoiceUpsert;

/// Job name of the retry job.
pub const RETRY_JOB_NAME: &str = "autumn_billing_dunning_retry";

/// A `Running` row not updated for this long was abandoned by a crashed run.
/// The next job run resets it to `Pending` and claims it again.
pub const RUNNING_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// Delay before the same attempt runs again after a provider transport error.
pub const TRANSPORT_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);

/// Applied ledger rows older than this are deleted at startup.
pub const EVENT_RETENTION: Duration = Duration::from_secs(30 * 86_400);

/// How long startup waits for the job runtime before it gives up re-arming.
const REARM_WAIT: Duration = Duration::from_secs(30);
/// Poll interval while waiting for the job runtime.
const REARM_POLL: Duration = Duration::from_millis(25);

/// Payload of the retry job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DunningRetryArgs {
    /// Local invoice id.
    pub invoice_id: String,
}

#[job(
    name = "autumn_billing_dunning_retry",
    max_attempts = 5,
    backoff_ms = 60_000,
    queue = "billing",
    unique,
    unique_by = "invoice_id",
    unique_window = "pending"
)]
async fn dunning_retry(state: AppState, args: DunningRetryArgs) -> AutumnResult<()> {
    let service = BillingService::require(&state)?;
    run_retry(&state, &service, &args.invoice_id)
        .await
        .map_err(BillingError::into_autumn)
}

/// Jobs the plugin registers.
#[must_use]
pub fn job_infos() -> Vec<JobInfo> {
    autumn_web::jobs![dunning_retry]
}

/// `now + delay` as an instant.
///
/// # Errors
///
/// Returns [`BillingError::Config`] when the delay does not fit the calendar.
pub(crate) fn due_at(now: DateTime<Utc>, delay: Duration) -> Result<DateTime<Utc>, BillingError> {
    let delta = chrono::Duration::from_std(delay)
        .map_err(|error| BillingError::Config(format!("dunning delay out of range: {error}")))?;
    now.checked_add_signed(delta)
        .ok_or_else(|| BillingError::Config("dunning delay overflows the calendar".to_owned()))
}

/// `at - delay`, or `None` when it does not fit the calendar.
fn before(at: DateTime<Utc>, delay: Duration) -> Option<DateTime<Utc>> {
    chrono::Duration::from_std(delay)
        .ok()
        .and_then(|delta| at.checked_sub_signed(delta))
}

/// When a `Running` row may be reclaimed: `updated_at + RUNNING_STALE_AFTER`.
fn reclaim_at(row: &DunningAttempt) -> Option<DateTime<Utc>> {
    chrono::Duration::from_std(RUNNING_STALE_AFTER)
        .ok()
        .and_then(|delta| row.updated_at.checked_add_signed(delta))
}

/// `true` when a `Running` row was abandoned: not updated since before
/// `now - RUNNING_STALE_AFTER`.
fn is_stale_running(row: &DunningAttempt, now: DateTime<Utc>) -> bool {
    row.state == DunningState::Running
        && before(now, RUNNING_STALE_AFTER).is_some_and(|cutoff| row.updated_at < cutoff)
}

/// This app's job client: the runtime installs it on `AppState`; the
/// process-global client is the fallback.
///
/// # Errors
///
/// Returns [`BillingError::Store`] when no job runtime is started.
pub(crate) fn job_client(state: &AppState) -> Result<Arc<JobClient>, BillingError> {
    state
        .extension::<JobClient>()
        .or_else(job::global_job_client)
        .ok_or_else(|| BillingError::store("job runtime is not started"))
}

/// Put the retry for `invoice_id` on the queue at `when`. A uniqueness
/// coalesce (an equivalent job already waits) is a success.
///
/// # Errors
///
/// Returns [`BillingError::Store`] when the job cannot be queued.
pub(crate) async fn schedule(
    state: &AppState,
    invoice_id: &str,
    when: DateTime<Utc>,
) -> Result<(), BillingError> {
    let client = job_client(state)?;
    let payload = serde_json::to_value(DunningRetryArgs {
        invoice_id: invoice_id.to_owned(),
    })
    .map_err(|error| BillingError::store(format!("dunning args: {error}")))?;
    match client
        .enqueue_due(RETRY_JOB_NAME, payload, Some(when))
        .await
    {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("unique job is already") => {
            tracing::debug!(invoice_id, "🍂 Autumn Billing: retry already queued");
            Ok(())
        }
        Err(error) => Err(BillingError::store(format!(
            "enqueue dunning retry: {error}"
        ))),
    }
}

/// One run of the retry job for `invoice_id`.
///
/// Reads the row, claims the attempt, then runs [`attempt`]. Any error after
/// the claim puts the row back to `Pending` at the same attempt and due
/// time, so it never stays `Running`.
async fn run_retry(
    state: &AppState,
    service: &BillingService,
    invoice_id: &str,
) -> Result<(), BillingError> {
    let store = service.store();
    let now = state.clock().now();
    let Some(mut row) = store.dunning_by_invoice(invoice_id).await? else {
        tracing::debug!(
            invoice_id,
            "🍂 Autumn Billing: no dunning row; retry skipped"
        );
        return Ok(());
    };
    if row.state == DunningState::Running {
        if !is_stale_running(&row, now) {
            // In flight elsewhere. Check again once it could be abandoned.
            tracing::debug!(
                invoice_id,
                "🍂 Autumn Billing: retry in flight elsewhere; skipped"
            );
            if let Some(check_at) = reclaim_at(&row) {
                schedule(state, invoice_id, check_at).await?;
            }
            return Ok(());
        }
        let mut reset = row.clone();
        reset.state = DunningState::Pending;
        reset.next_attempt_at = now;
        reset.updated_at = now;
        if !store
            .settle_dunning(
                invoice_id,
                row.attempt,
                &[DunningState::Running],
                reset.clone(),
            )
            .await?
        {
            tracing::debug!(
                invoice_id,
                "🍂 Autumn Billing: stale running row settled elsewhere; retry skipped"
            );
            return Ok(());
        }
        tracing::warn!(
            invoice_id,
            attempt = row.attempt,
            "🍂 Autumn Billing: abandoned running retry reclaimed"
        );
        row = reset;
    }
    if row.state != DunningState::Pending {
        tracing::debug!(
            invoice_id,
            state = row.state.as_str(),
            "🍂 Autumn Billing: dunning row not pending; retry skipped"
        );
        return Ok(());
    }
    if row.next_attempt_at > now {
        tracing::debug!(
            invoice_id,
            due = %row.next_attempt_at,
            "🍂 Autumn Billing: retry not due; re-queued"
        );
        return schedule(state, invoice_id, row.next_attempt_at).await;
    }
    if !store
        .claim_dunning_attempt(invoice_id, row.attempt, now)
        .await?
    {
        tracing::debug!(
            invoice_id,
            attempt = row.attempt,
            "🍂 Autumn Billing: retry claimed elsewhere; skipped"
        );
        return Ok(());
    }
    row.state = DunningState::Running;
    row.updated_at = now;
    match attempt(state, service, &row, now).await {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::warn!(
                invoice_id,
                attempt = row.attempt,
                error = %error,
                "🍂 Autumn Billing: retry failed after the claim; row restored to pending"
            );
            if let Err(restore) = restore_pending(service, &row, now).await {
                tracing::error!(
                    invoice_id,
                    error = %restore,
                    "🍂 Autumn Billing: could not restore the dunning row to pending"
                );
            }
            Err(error)
        }
    }
}

/// The claimed attempt: call the provider and settle the row.
async fn attempt(
    state: &AppState,
    service: &BillingService,
    row: &DunningAttempt,
    now: DateTime<Utc>,
) -> Result<(), BillingError> {
    let store = service.store();
    let invoice_id = row.invoice_id.as_str();
    let Some(invoice) = store.invoice_by_id(invoice_id).await? else {
        tracing::error!(
            invoice_id,
            "🍂 Autumn Billing: dunning row without an invoice"
        );
        return Err(BillingError::NotFound("invoice"));
    };
    let idempotency_key = format!("autumn-billing:{invoice_id}:{}", row.attempt);
    let outcome = service
        .provider()
        .retry_invoice_payment(&invoice.provider_invoice_id, &idempotency_key)
        .await;
    match outcome {
        Ok(PaymentAttemptOutcome::Paid | PaymentAttemptOutcome::AlreadyPaid) => {
            recovered(state, service, row, invoice, now).await
        }
        Ok(PaymentAttemptOutcome::Declined { reason }) => {
            declined(state, service, row, invoice, &reason, now).await
        }
        Err(error) => {
            // The schedule row stays the truth: the same attempt runs again
            // after a delay. The job itself succeeds, so the framework never
            // dead-letters a retry the store still owns.
            let due = due_at(now, TRANSPORT_RETRY_DELAY)?;
            tracing::warn!(
                invoice_id,
                attempt = row.attempt,
                due = %due,
                error = %error,
                "🍂 Autumn Billing: retry call failed; same attempt rescheduled"
            );
            let mut pending = row.clone();
            pending.state = DunningState::Pending;
            pending.next_attempt_at = due;
            pending.updated_at = now;
            if settle(service, row, pending).await? {
                schedule(state, invoice_id, due).await?;
            }
            Ok(())
        }
    }
}

/// Compare-and-set `row` (in `Running` at its attempt) to `next`. `false`
/// means an event settled the row while the call was in flight; that
/// decision wins.
async fn settle(
    service: &BillingService,
    row: &DunningAttempt,
    next: DunningAttempt,
) -> Result<bool, BillingError> {
    let written = service
        .store()
        .settle_dunning(&row.invoice_id, row.attempt, &[DunningState::Running], next)
        .await?;
    if !written {
        tracing::info!(
            invoice_id = %row.invoice_id,
            attempt = row.attempt,
            "🍂 Autumn Billing: dunning row settled during the retry; outcome not applied"
        );
    }
    Ok(written)
}

/// Put a claimed row back to `Pending` with the same attempt and due time.
/// Returns `false` when the row is no longer `Running` at that attempt.
async fn restore_pending(
    service: &BillingService,
    row: &DunningAttempt,
    now: DateTime<Utc>,
) -> Result<bool, BillingError> {
    let mut pending = row.clone();
    pending.state = DunningState::Pending;
    pending.updated_at = now;
    service
        .store()
        .settle_dunning(
            &row.invoice_id,
            row.attempt,
            &[DunningState::Running],
            pending,
        )
        .await
}

/// The retry paid the invoice.
async fn recovered(
    state: &AppState,
    service: &BillingService,
    row: &DunningAttempt,
    invoice: Invoice,
    now: DateTime<Utc>,
) -> Result<(), BillingError> {
    let store = service.store();
    let mut settled = row.clone();
    settled.state = DunningState::Recovered;
    settled.updated_at = now;
    if !settle(service, row, settled).await? {
        return Ok(());
    }
    let mut paid = InvoiceUpsert::new(
        state.entropy().uuid_v4().to_string(),
        invoice.customer_id.clone(),
        invoice.provider_invoice_id.clone(),
        InvoiceStatus::Paid,
        invoice.amount_due,
        invoice.amount_due,
        now,
        now,
    )
    .with_attempt_count(invoice.attempt_count);
    if let Some(subscription_id) = &invoice.subscription_id {
        paid = paid.with_subscription(subscription_id.clone());
    }
    let invoice = store.upsert_invoice(paid).await?.into_inner();
    tracing::info!(invoice_id = %invoice.id, "🍂 Autumn Billing: payment recovered");
    if let Some(customer) = store.customer_by_id(&invoice.customer_id).await? {
        notify::send_to_customer(
            state,
            service,
            &customer,
            notify::KIND_PAYMENT_RECOVERED,
            notify::payment_recovered_payload(&invoice),
        )
        .await;
    }
    service.hooks().on_payment_recovered(&invoice).await;
    Ok(())
}

/// The provider declined the retry: schedule the next one, or exhaust.
async fn declined(
    state: &AppState,
    service: &BillingService,
    row: &DunningAttempt,
    invoice: Invoice,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), BillingError> {
    let store = service.store();
    let policy = &service.config().dunning;
    let customer = store.customer_by_id(&invoice.customer_id).await?;
    let next = row
        .attempt
        .checked_add(1)
        .ok_or_else(|| BillingError::Conflict("dunning attempt counter overflow".to_owned()))?;
    if let Some(delay) = policy.delay_for(next) {
        let due = due_at(now, delay)?;
        let mut scheduled = row.clone();
        scheduled.attempt = next;
        scheduled.next_attempt_at = due;
        scheduled.state = DunningState::Pending;
        scheduled.updated_at = now;
        if !settle(service, row, scheduled.clone()).await? {
            return Ok(());
        }
        schedule(state, &invoice.id, due).await?;
        tracing::info!(
            invoice_id = %invoice.id,
            attempt = next,
            due = %due,
            reason,
            "🍂 Autumn Billing: retry declined; next retry scheduled"
        );
        if let Some(customer) = &customer {
            notify::send_to_customer(
                state,
                service,
                customer,
                notify::KIND_PAYMENT_FAILED,
                notify::payment_failed_payload(&invoice, Some(next), Some(reason)),
            )
            .await;
        }
        service
            .hooks()
            .on_payment_failed(&invoice, &scheduled)
            .await;
        return Ok(());
    }
    exhausted(state, service, row, invoice, reason, now).await
}

/// Every retry failed: the mirror stops entitlement first, then the
/// provider is told.
async fn exhausted(
    state: &AppState,
    service: &BillingService,
    row: &DunningAttempt,
    invoice: Invoice,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), BillingError> {
    let store = service.store();
    let mut exhausted = row.clone();
    exhausted.state = DunningState::Exhausted;
    exhausted.updated_at = now;
    if !settle(service, row, exhausted.clone()).await? {
        return Ok(());
    }
    let customer = store.customer_by_id(&invoice.customer_id).await?;
    let action = service.config().dunning.on_exhausted;
    // The row was opened before the subscription was mirrored when the
    // failure arrived first; the invoice carries the link by now.
    let subscription_id = exhausted
        .subscription_id
        .clone()
        .or_else(|| invoice.subscription_id.clone());
    if let Some(subscription_id) = &subscription_id {
        let subscription = store
            .set_subscription_status(subscription_id, SubscriptionStatus::Unpaid, now)
            .await?;
        if action == ExhaustionAction::CancelSubscription {
            if let Some(subscription) = subscription {
                if let Err(error) = service
                    .provider()
                    .cancel_subscription(&subscription.provider_subscription_id)
                    .await
                {
                    // The mirror says unpaid; the operator reconciles the provider.
                    tracing::error!(
                        subscription_id,
                        provider_subscription_id = %subscription.provider_subscription_id,
                        error = %error,
                        "🍂 Autumn Billing: provider cancel failed after dunning exhausted"
                    );
                }
            } else {
                tracing::warn!(
                    subscription_id,
                    "🍂 Autumn Billing: dunning exhausted for an unknown subscription"
                );
            }
        }
    }
    tracing::warn!(
        invoice_id = %invoice.id,
        attempts = exhausted.attempt,
        reason,
        "🍂 Autumn Billing: dunning exhausted"
    );
    if let Some(customer) = &customer {
        let action_label = match action {
            ExhaustionAction::CancelSubscription => "cancel_subscription",
            ExhaustionAction::MarkUnpaid => "mark_unpaid",
        };
        notify::send_to_customer(
            state,
            service,
            customer,
            notify::KIND_DUNNING_EXHAUSTED,
            notify::dunning_exhausted_payload(&invoice, exhausted.attempt, action_label),
        )
        .await;
    }
    service
        .hooks()
        .on_dunning_exhausted(&invoice, &exhausted)
        .await;
    Ok(())
}

/// Re-enqueue every open schedule row at its due time and prune the event
/// ledger. Waits for the job runtime (the test harness starts it after
/// startup hooks).
pub(crate) fn rearm_pending(state: AppState, service: Arc<BillingService>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("🍂 Autumn Billing: no async runtime; dunning rows not re-armed");
        return;
    };
    handle.spawn(async move {
        if !wait_for_job_client(&state).await {
            tracing::warn!(
                "🍂 Autumn Billing: job runtime did not start within {REARM_WAIT:?}; dunning rows not re-armed"
            );
            return;
        }
        let rows = match service.store().open_dunning().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "🍂 Autumn Billing: could not read open dunning rows");
                return;
            }
        };
        let mut armed = 0_usize;
        for row in rows {
            if rearm_row(&state, &row).await {
                armed = armed.saturating_add(1);
            }
        }
        if armed > 0 {
            tracing::info!(armed, "🍂 Autumn Billing: dunning retries re-armed");
        }
        prune_events(&state, &service).await;
    });
}

/// Queue one open row. A `Running` row is queued for when it can be
/// reclaimed, not reset: another instance may still own it.
async fn rearm_row(state: &AppState, row: &DunningAttempt) -> bool {
    let mut when = row.next_attempt_at;
    if row.state == DunningState::Running
        && let Some(reclaim) = reclaim_at(row)
    {
        when = when.max(reclaim);
    }
    match schedule(state, &row.invoice_id, when).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                invoice_id = %row.invoice_id,
                error = %error,
                "🍂 Autumn Billing: could not re-arm a dunning retry"
            );
            false
        }
    }
}

/// Delete applied ledger rows older than [`EVENT_RETENTION`].
async fn prune_events(state: &AppState, service: &BillingService) {
    let now = state.clock().now();
    let Some(cutoff) = before(now, EVENT_RETENTION) else {
        return;
    };
    match service.store().prune_events(cutoff).await {
        Ok(0) => {}
        Ok(pruned) => tracing::info!(pruned, "🍂 Autumn Billing: old ledger events pruned"),
        Err(error) => {
            tracing::warn!(error = %error, "🍂 Autumn Billing: could not prune the event ledger");
        }
    }
}

/// `true` once the job runtime installed its client on `state`.
async fn wait_for_job_client(state: &AppState) -> bool {
    let deadline = tokio::time::Instant::now() + REARM_WAIT;
    loop {
        if state.extension::<JobClient>().is_some() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(REARM_POLL).await;
    }
}
