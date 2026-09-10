//! Durable retries for failed payments.
//!
//! The `billing_dunning` row is the schedule. The job payload carries only the
//! local invoice id; the job reads the row, so a duplicate or early run is a
//! no-op and a restart re-arms from the store.

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

/// Put the retry for `invoice_id` on the queue at `when`. A uniqueness
/// coalesce (an equivalent job already waits) is a success.
async fn schedule(invoice_id: &str, when: DateTime<Utc>) -> Result<(), BillingError> {
    let args = DunningRetryArgs {
        invoice_id: invoice_id.to_owned(),
    };
    match DunningRetryJob::enqueue_at(args, when).await {
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
async fn run_retry(
    state: &AppState,
    service: &BillingService,
    invoice_id: &str,
) -> Result<(), BillingError> {
    let store = service.store();
    let now = state.clock().now();
    let Some(row) = store.dunning_by_invoice(invoice_id).await? else {
        tracing::debug!(invoice_id, "🍂 Autumn Billing: no dunning row; retry skipped");
        return Ok(());
    };
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
        return schedule(invoice_id, row.next_attempt_at).await;
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
    let Some(invoice) = store.invoice_by_id(invoice_id).await? else {
        tracing::error!(invoice_id, "🍂 Autumn Billing: dunning row without an invoice");
        restore_pending(service, &row, now).await?;
        return Err(BillingError::NotFound("invoice"));
    };
    let idempotency_key = format!("autumn-billing:{invoice_id}:{}", row.attempt);
    let outcome = service
        .provider()
        .retry_invoice_payment(&invoice.provider_invoice_id, &idempotency_key)
        .await;
    // An event settled the row while the call was in flight (paid, or the
    // subscription ended). Its decision wins.
    let current = store.dunning_by_invoice(invoice_id).await?;
    let Some(row) = current.filter(|current| current.state == DunningState::Running) else {
        tracing::info!(
            invoice_id,
            "🍂 Autumn Billing: dunning row settled during the retry; outcome not applied"
        );
        return Ok(());
    };
    match outcome {
        Ok(PaymentAttemptOutcome::Paid | PaymentAttemptOutcome::AlreadyPaid) => {
            recovered(state, service, row, invoice, now).await
        }
        Ok(PaymentAttemptOutcome::Declined { reason }) => {
            declined(state, service, row, invoice, &reason, now).await
        }
        Err(error) => {
            tracing::warn!(
                invoice_id,
                attempt = row.attempt,
                error = %error,
                "🍂 Autumn Billing: retry call failed; row left pending for the job retry"
            );
            restore_pending(service, &row, now).await?;
            Err(error)
        }
    }
}

/// Put a claimed row back to `Pending` with the same attempt and due time.
async fn restore_pending(
    service: &BillingService,
    row: &DunningAttempt,
    now: DateTime<Utc>,
) -> Result<(), BillingError> {
    let mut pending = row.clone();
    pending.state = DunningState::Pending;
    pending.updated_at = now;
    service.store().upsert_dunning(pending).await
}

/// The retry paid the invoice.
async fn recovered(
    state: &AppState,
    service: &BillingService,
    mut row: DunningAttempt,
    invoice: Invoice,
    now: DateTime<Utc>,
) -> Result<(), BillingError> {
    let store = service.store();
    row.state = DunningState::Recovered;
    row.updated_at = now;
    store.upsert_dunning(row).await?;
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
    mut row: DunningAttempt,
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
        row.attempt = next;
        row.next_attempt_at = due;
        row.state = DunningState::Pending;
        row.updated_at = now;
        store.upsert_dunning(row.clone()).await?;
        schedule(&invoice.id, due).await?;
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
        service.hooks().on_payment_failed(&invoice, &row).await;
        return Ok(());
    }

    // Exhausted: the mirror stops entitlement first, then the provider is told.
    row.state = DunningState::Exhausted;
    row.updated_at = now;
    store.upsert_dunning(row.clone()).await?;
    let action = policy.on_exhausted;
    if let Some(subscription_id) = &row.subscription_id {
        let subscription = store
            .set_subscription_status(subscription_id, SubscriptionStatus::Unpaid, now)
            .await?;
        if action == ExhaustionAction::CancelSubscription {
            match subscription {
                Some(subscription) => {
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
                }
                None => tracing::warn!(
                    subscription_id,
                    "🍂 Autumn Billing: dunning exhausted for an unknown subscription"
                ),
            }
        }
    }
    tracing::warn!(
        invoice_id = %invoice.id,
        attempts = row.attempt,
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
            notify::dunning_exhausted_payload(&invoice, row.attempt, action_label),
        )
        .await;
    }
    service.hooks().on_dunning_exhausted(&invoice, &row).await;
    Ok(())
}

/// Re-enqueue every open schedule row at its due time. Waits for the job
/// runtime (the test harness starts it after startup hooks).
pub fn rearm_pending(state: AppState, service: Arc<BillingService>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("🍂 Autumn Billing: no async runtime; dunning rows not re-armed");
        return;
    };
    handle.spawn(async move {
        let Some(client) = wait_for_job_client(&state).await else {
            tracing::warn!(
                "🍂 Autumn Billing: job runtime did not start within {REARM_WAIT:?}; dunning rows not re-armed"
            );
            return;
        };
        let rows = match service.store().open_dunning().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "🍂 Autumn Billing: could not read open dunning rows");
                return;
            }
        };
        let mut armed = 0_usize;
        for row in rows {
            if rearm_row(&state, &service, &client, row).await {
                armed = armed.saturating_add(1);
            }
        }
        if armed > 0 {
            tracing::info!(armed, "🍂 Autumn Billing: dunning retries re-armed");
        }
    });
}

/// Queue one open row. A row left `Running` by a crash runs now.
async fn rearm_row(
    state: &AppState,
    service: &BillingService,
    client: &JobClient,
    mut row: DunningAttempt,
) -> bool {
    if row.state == DunningState::Running {
        let now = state.clock().now();
        row.state = DunningState::Pending;
        row.next_attempt_at = now;
        row.updated_at = now;
        if let Err(error) = service.store().upsert_dunning(row.clone()).await {
            tracing::warn!(
                invoice_id = %row.invoice_id,
                error = %error,
                "🍂 Autumn Billing: could not reset a running dunning row"
            );
            return false;
        }
    }
    let payload = match serde_json::to_value(DunningRetryArgs {
        invoice_id: row.invoice_id.clone(),
    }) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(error = %error, "🍂 Autumn Billing: dunning args not serializable");
            return false;
        }
    };
    match client
        .enqueue_due(RETRY_JOB_NAME, payload, Some(row.next_attempt_at))
        .await
    {
        Ok(()) => true,
        Err(error) if error.to_string().contains("unique job is already") => true,
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

/// This app's job client, once the runtime installed it.
async fn wait_for_job_client(state: &AppState) -> Option<Arc<JobClient>> {
    let deadline = tokio::time::Instant::now() + REARM_WAIT;
    loop {
        if let Some(client) = state.extension::<JobClient>() {
            return Some(client);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(REARM_POLL).await;
    }
}
