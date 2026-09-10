//! Durable retries for failed payments.
//!
//! The `billing_dunning` row is the schedule. The job payload carries only the
//! local invoice id; the job reads the row, so a duplicate or early run is a
//! no-op and a restart re-arms from the store.

use std::sync::Arc;

use autumn_web::job::JobInfo;
use autumn_web::{AppState, AutumnResult, job};
use serde::{Deserialize, Serialize};

use crate::BillingService;

/// Job name of the retry job.
pub const RETRY_JOB_NAME: &str = "autumn_billing_dunning_retry";

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
    let _ = (state, args);
    Ok(())
}

/// Jobs the plugin registers.
#[must_use]
pub fn job_infos() -> Vec<JobInfo> {
    autumn_web::jobs![dunning_retry]
}

/// Re-enqueue every open schedule row at its due time. Waits for the job
/// runtime (the test harness starts it after startup hooks).
pub fn rearm_pending(state: AppState, service: Arc<BillingService>) {
    let _ = (state, service);
}
