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
    ///
    /// `None` in a single-tenant application, which is not the same as a tenant
    /// named `-`: a runner passes this straight back to
    /// [`CapabilityServices::for_tenant`](super::CapabilityServices::for_tenant)
    /// or leaves it unset, so the raw id is what belongs here and the
    /// namespacing stays the host's to derive.
    pub tenant: Option<String>,
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
        tenant: runtime.tenant().map(str::to_owned),
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
    /// The queue and the bytes it holds, under one lock.
    ///
    /// Together, not beside each other: a total kept in a second mutex can be
    /// read between the two writes that should have changed it, and a running
    /// total that can be observed stale is worse than no total at all.
    queued: Mutex<QueuedJobs>,
    /// How many jobs the queue will hold before refusing — the "queue depth"
    /// ceiling.
    depth: usize,
    /// How many bytes of payload the queue will hold, which is the ceiling that
    /// actually bounds memory: depth times the largest legal payload is
    /// hundreds of megabytes, and nothing in this slice drains the queue.
    byte_capacity: usize,
}

/// A queue and its running weight.
#[derive(Debug, Default)]
struct QueuedJobs {
    jobs: Vec<PluginJob>,
    bytes: usize,
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

/// Bytes of queued payload the zero-configuration queue holds before refusing.
///
/// The depth ceiling above bounds the queue in jobs; this bounds it in the unit
/// that actually runs out. A payload may approach `MAX_ROW_BYTES`, so
/// `DEFAULT_JOB_DEPTH` of them is hundreds of megabytes — reached in seconds at
/// the default call rate, and never released, because this slice ships no
/// consumer.
pub const DEFAULT_JOB_BYTE_CAPACITY: usize = 16 * 1024 * 1024;

impl Default for MemoryJobSink {
    fn default() -> Self {
        Self {
            queued: Mutex::new(QueuedJobs::default()),
            depth: DEFAULT_JOB_DEPTH,
            byte_capacity: DEFAULT_JOB_BYTE_CAPACITY,
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
            queued: Mutex::new(QueuedJobs::default()),
            depth,
            byte_capacity: DEFAULT_JOB_BYTE_CAPACITY,
        })
    }

    /// What one queued job costs: everything the record retains.
    fn job_weight(job: &PluginJob) -> usize {
        /// A `Vec` slot plus the three `String` headers a job carries.
        const PER_ENTRY: usize = 96;
        job.plugin
            .len()
            .saturating_add(job.job_type.len())
            .saturating_add(job.tenant.as_ref().map_or(0, String::len))
            .saturating_add(super::row_weight(&job.payload))
            .saturating_add(PER_ENTRY)
    }

    /// A queue bounded by both a depth and a total payload size.
    #[must_use]
    pub fn with_capacities(depth: usize, byte_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            queued: Mutex::new(QueuedJobs::default()),
            depth,
            byte_capacity,
        })
    }

    /// Everything enqueued so far.
    #[must_use]
    pub fn queued(&self) -> Vec<PluginJob> {
        self.queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs
            .clone()
    }

    /// Bytes the queue is currently holding, for assertions.
    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        self.queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes
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
        if queued.jobs.len() >= self.depth {
            return Err(format!(
                "the plugin job queue is at its depth ceiling of {}",
                self.depth
            ));
        }
        // And by the size of the whole record, not just its payload. Depth
        // alone bounds the queue in jobs rather than in the memory they hold,
        // and a job retains a tenant id, a plugin name and a job type beside
        // its arguments — a tenant arrives in a header with no length bound of
        // its own, so an empty-payload job charged nothing while holding one.
        // The running total, not a fresh scan. Re-weighing the whole queue on
        // every enqueue made a *full* queue the expensive case: each refusal
        // walked every payload while holding this lock, so a plugin that had
        // filled the queue could spend the host's CPU indefinitely at no cost
        // to its own quota. A refusal now costs the same as an acceptance.
        let incoming = Self::job_weight(&job);
        if queued.bytes.saturating_add(incoming) > self.byte_capacity {
            return Err(format!(
                "the plugin job queue is at its {}-byte ceiling",
                self.byte_capacity
            ));
        }
        queued.bytes = queued.bytes.saturating_add(incoming);
        queued.jobs.push(job);
        Ok(format!("j{}", queued.jobs.len()))
    }
}
