//! Background-job enqueue for a sandboxed plugin (issue #1632).
//!
//! A plugin granted `jobs` may enqueue the job types its manifest declares, and
//! only those. The interesting half is not the enqueue — it is what the job
//! carries:
//!
//! ```text
//! PluginJob { plugin, job_type, tenant, payload }
//!             ─┬────  ─┬──────  ─┬────
//!              │       │         └─ the tenant that was active when it was enqueued
//!              │       └─ one of `[grants].job_types`, checked at enqueue
//!              └─ whose grants and quotas the run executes under
//! ```
//!
//! Every field is the *host's*, taken from the manifest and the request rather
//! than from the frame. A job therefore runs under exactly the grants the plugin
//! that enqueued it holds — the acceptance criterion's "enqueued jobs run under
//! the enqueuing plugin's grants and quotas" is a property of the record, not a
//! rule a runner has to remember, because there is nowhere in the record for a
//! different plugin or tenant to be written.
//!
//! A runner reconstructs the sandbox from `plugin`, builds
//! [`CapabilityServices`](super::CapabilityServices) bound to `tenant`, and runs
//! the plugin's job entry point. It cannot widen the grant, because the grant
//! comes from the manifest the operator approved and not from the job.

use std::sync::{Arc, Mutex, PoisonError};

use super::{
    CallResult, CallValue, CapabilityCall, CapabilityRuntime, DenialReason, PluginRow, check_row,
};

/// One job a sandboxed plugin asked for.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PluginJob {
    /// The plugin whose grants and quotas the run executes under.
    pub plugin: String,
    /// One of the plugin's declared job types.
    pub job_type: String,
    /// The tenant that was active at enqueue, and that the run binds to.
    pub tenant: String,
    /// The job's arguments.
    pub payload: PluginRow,
}

/// Somewhere a plugin's jobs are queued.
pub trait JobSink: Send + Sync + 'static {
    /// Enqueue `job`, returning the id assigned to it.
    ///
    /// # Errors
    ///
    /// One line for the guest and the audit ledger.
    fn enqueue(&self, job: PluginJob) -> Result<String, String>;
}

/// Answer one `job-enqueue`. Capability, scope and quota are already checked.
pub(super) fn perform(
    runtime: &CapabilityRuntime,
    call: &CapabilityCall,
    job_type: &str,
) -> CallResult {
    let id = call.id();
    let CapabilityCall::JobEnqueue { payload, .. } = call else {
        return CallResult::denied(id, DenialReason::Malformed, "not a job call");
    };
    let Some(sink) = runtime.services.jobs.clone() else {
        return CallResult::denied(
            id,
            DenialReason::Unavailable,
            "this host has no job queue wired for sandboxed plugins",
        );
    };
    if let Err(detail) = check_row(payload) {
        return CallResult::denied(id, DenialReason::Malformed, detail);
    }
    let job = PluginJob {
        plugin: runtime.plugin.clone(),
        job_type: job_type.to_owned(),
        tenant: runtime.tenant().to_owned(),
        payload: payload.clone(),
    };
    match sink.enqueue(job) {
        Ok(job_id) => CallResult::Ok {
            id,
            value: CallValue::JobId { job_id },
        },
        Err(detail) => CallResult::denied(id, DenialReason::BackendError, detail),
    }
}

// ── An in-process queue ──────────────────────────────────────────────────

/// A [`JobSink`] that keeps what it was handed.
///
/// The property worth testing about job enqueue is *what the record says*, and
/// that needs somewhere to read it back from rather than a live queue.
#[derive(Debug)]
pub struct MemoryJobSink {
    queued: Mutex<Vec<PluginJob>>,
    /// How many jobs the queue will hold before refusing — the "queue depth"
    /// ceiling.
    depth: usize,
}

/// How many jobs the zero-configuration queue holds before refusing.
///
/// There is no unbounded spelling, deliberately. This slice ships no consumer
/// that removes a queued job, so an unbounded queue is a granted plugin's
/// memory-exhaustion channel: the per-request and per-second quotas slow the
/// growth and never stop it, and a job payload may be [`MAX_ROW_COLUMNS`]
/// columns of text. A default that has to be *tightened* to be safe is a default
/// that will be shipped as it is.
///
/// [`MAX_ROW_COLUMNS`]: super::MAX_ROW_COLUMNS
pub const DEFAULT_JOB_DEPTH: usize = 1024;

impl Default for MemoryJobSink {
    fn default() -> Self {
        Self {
            queued: Mutex::new(Vec::new()),
            depth: DEFAULT_JOB_DEPTH,
        }
    }
}

impl MemoryJobSink {
    /// A queue holding [`DEFAULT_JOB_DEPTH`] jobs.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A queue that refuses past `depth` entries.
    #[must_use]
    pub fn bounded(depth: usize) -> Arc<Self> {
        Arc::new(Self {
            queued: Mutex::new(Vec::new()),
            depth,
        })
    }

    /// Everything enqueued so far.
    #[must_use]
    pub fn queued(&self) -> Vec<PluginJob> {
        self.queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl JobSink for MemoryJobSink {
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the body is one critical section over the shared map; releasing early would \
                  either split a ceiling check from the write it guards or let the snapshot \
                  this returns be torn"
    )]
    fn enqueue(&self, job: PluginJob) -> Result<String, String> {
        let mut queued = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        if queued.len() >= self.depth {
            return Err(format!(
                "the plugin job queue is at its depth ceiling of {}",
                self.depth
            ));
        }
        queued.push(job);
        Ok(format!("j{}", queued.len()))
    }
}
