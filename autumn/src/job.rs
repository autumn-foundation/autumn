//! On-demand background job infrastructure.
//!
//! Provides [`JobInfo`] metadata used by `#[job]` and `jobs![]`, plus local
//! and Redis-backed queue backends.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};

use futures::FutureExt as _;
#[cfg(feature = "redis")]
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::{AppState, AutumnError, AutumnResult};

pub use crate::job_tracking::{
    JobContext, JobTrackingStore, JobTrackingStoreEntry, TrackedJobHandle, TrackedJobOwner,
    TrackedJobRecord, TrackedJobStatus, enqueue_tracked, enqueue_tracked_for,
};

/// The asynchronous function signature for a background job.
///
/// Handlers receive the full `AppState` and a JSON `Value` representing the job's payload.
pub type JobHandler =
    fn(AppState, Value) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>>;

const DEFAULT_JOB_ADMIN_HISTORY_LIMIT: usize = 1_000;
const DEFAULT_JOB_ADMIN_PER_PAGE: u64 = 25;

/// Uniqueness window controlling how long a unique job's key stays held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobUniquenessWindow {
    /// The key is held while the job is pending and released when execution
    /// starts, so a duplicate may be enqueued once the original is running.
    Pending,
    /// The key is held while the job is pending **or** running and released
    /// when it finishes (success or terminal failure). This is the default.
    Running,
    /// The key is held for this many milliseconds from enqueue time, deduping
    /// bursts even after the original job completed within the window.
    TtlMs(u64),
}

impl JobUniquenessWindow {
    /// Stable serialization tag persisted with durable job records.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::TtlMs(_) => "ttl",
        }
    }
}

/// Uniqueness configuration declared with `#[job(unique, ...)]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobUniqueness {
    /// Payload field names the unique key derives from.
    ///
    /// Empty means the key is a stable hash of the full args payload.
    pub by: Vec<String>,
    /// How long the unique key stays held.
    pub window: JobUniquenessWindow,
}

/// Concurrency limit configuration declared with `#[job(concurrency = N)]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobConcurrency {
    /// Maximum simultaneously-executing jobs of this type.
    pub limit: u32,
    /// Optional payload field that scopes the limit per distinct value.
    pub key: Option<String>,
}

/// Metadata describing a registered background job.
#[derive(Clone)]
pub struct JobInfo {
    /// The unique identifier for this job type.
    pub name: String,
    /// Maximum number of times a failing job will be retried.
    pub max_attempts: u32,
    /// Base delay in milliseconds before the first retry (scales exponentially).
    pub initial_backoff_ms: u64,
    /// Named queue this job is routed to. Workers drain queues in the priority
    /// order configured under `[jobs] queues`. Defaults to `"default"`.
    pub queue: String,
    /// Uniqueness (dedup) configuration; `None` means no dedup.
    pub uniqueness: Option<JobUniqueness>,
    /// In-flight concurrency cap; `None` means unbounded per-type concurrency.
    pub concurrency: Option<JobConcurrency>,
    /// The async function that executes the job logic.
    pub handler: JobHandler,
}

impl JobInfo {
    /// Construct job metadata with no uniqueness or concurrency constraints.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        max_attempts: u32,
        initial_backoff_ms: u64,
        handler: JobHandler,
    ) -> Self {
        Self {
            name: name.into(),
            max_attempts,
            initial_backoff_ms,
            queue: "default".to_string(),
            uniqueness: None,
            concurrency: None,
            handler,
        }
    }
}

/// The queue every job lands on when none is declared.
pub(crate) const DEFAULT_QUEUE: &str = "default";

/// Normalize a declared queue name, mapping empty/whitespace to `"default"`.
pub(crate) fn normalize_queue_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        DEFAULT_QUEUE.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The worker's queue drain plan: an ordered/weighted set of queues plus the
/// strict-vs-weighted strategy. Backend-agnostic and shared by all three
/// backends so priority logic lives in one place.
#[derive(Debug, Clone)]
pub(crate) struct QueueSchedule {
    /// `(name, weight)` pairs, highest priority first.
    queues: Vec<(String, u32)>,
    strict: bool,
}

impl QueueSchedule {
    /// Build a schedule from parsed `[jobs] queues` config.
    pub(crate) fn from_config(cfg: &crate::config::JobQueuesConfig) -> Self {
        let queues: Vec<(String, u32)> = cfg
            .queues
            .iter()
            .map(|q| (normalize_queue_name(&q.name), q.weight.max(1)))
            .collect();
        let queues = if queues.is_empty() {
            vec![(DEFAULT_QUEUE.to_string(), 1)]
        } else {
            queues
        };
        Self {
            queues,
            strict: cfg.strict,
        }
    }

    /// Build the effective schedule, appending any queue that a registered job
    /// declares but the operator did not configure — at lowest priority, so the
    /// job still drains instead of silently stalling. Returns the names of those
    /// unconfigured queues so the caller can log a loud warning.
    pub(crate) fn effective(
        cfg: &crate::config::JobQueuesConfig,
        declared: &[String],
    ) -> (Self, Vec<String>) {
        let mut schedule = Self::from_config(cfg);
        let mut warnings = Vec::new();
        for declared_queue in declared {
            let name = normalize_queue_name(declared_queue);
            if !schedule.queues.iter().any(|(n, _)| *n == name) {
                schedule.queues.push((name.clone(), 1));
                warnings.push(name);
            }
        }
        (schedule, warnings)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn is_strict(&self) -> bool {
        self.strict
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.queues.iter().any(|(n, _)| n == name)
    }

    /// Queue names highest priority first.
    #[cfg_attr(not(any(test, feature = "redis")), allow(dead_code))]
    pub(crate) fn names(&self) -> Vec<String> {
        self.queues.iter().map(|(n, _)| n.clone()).collect()
    }

    /// A per-worker cursor producing each claim iteration's attempt order.
    pub(crate) fn cursor(&self) -> QueueCursor {
        QueueCursor {
            names: Arc::new(self.queues.iter().map(|(n, _)| n.clone()).collect()),
            weights: self.queues.iter().map(|(_, w)| *w).collect(),
            current: vec![0_i64; self.queues.len()],
            strict: self.strict,
        }
    }
}

/// Per-worker draining cursor. For strict schedules it always yields the
/// configured order; for weighted schedules it uses smooth weighted round-robin
/// so each queue is served in proportion to its weight and none is ever starved.
#[derive(Debug, Clone)]
pub(crate) struct QueueCursor {
    names: Arc<Vec<String>>,
    weights: Vec<u32>,
    current: Vec<i64>,
    strict: bool,
}

impl QueueCursor {
    /// Ordered queue names to attempt for this claim iteration. The first entry
    /// is the queue to serve now; the rest follow (so a worker never idles while
    /// any queue has work).
    pub(crate) fn next_order(&mut self) -> Arc<Vec<String>> {
        if self.strict || self.names.len() <= 1 {
            return Arc::clone(&self.names);
        }
        // Smooth weighted round-robin (nginx-style): every queue is the first
        // choice exactly `weight` times per cycle of length `sum(weights)`.
        let total: i64 = self.weights.iter().map(|w| i64::from(*w)).sum();
        let mut best = 0_usize;
        for i in 0..self.names.len() {
            self.current[i] += i64::from(self.weights[i]);
            if self.current[i] > self.current[best] {
                best = i;
            }
        }
        self.current[best] -= total;
        // Chosen queue first, then the rest by descending remaining credit.
        let mut rest: Vec<usize> = (0..self.names.len()).filter(|&i| i != best).collect();
        rest.sort_by(|&a, &b| self.current[b].cmp(&self.current[a]));
        let mut order = Vec::with_capacity(self.names.len());
        order.push(self.names[best].clone());
        order.extend(rest.into_iter().map(|i| self.names[i].clone()));
        Arc::new(order)
    }
}

/// The runtime client for interacting with the job queue.
///
/// Used to enqueue jobs to the active backend (local or Redis).
#[derive(Clone)]
pub struct JobClient {
    local_sender: Option<tokio::sync::mpsc::Sender<QueuedJob>>,
    local_coordination: Option<Arc<LocalJobCoordination>>,
    #[cfg(feature = "redis")]
    redis: Option<RedisClient>,
    #[cfg(feature = "db")]
    pg_pool: Option<PgPool>,
    registry: crate::actuator::JobRegistry,
    job_admin: JobAdminMemoryBackend,
    default_max_attempts: u32,
    default_initial_backoff_ms: u64,
    per_job_settings: HashMap<String, JobRuntimeSettings>,
    pub interceptor: Option<Arc<dyn crate::interceptor::JobInterceptor>>,
    resilience_config: Option<Arc<crate::config::ResilienceConfig>>,
}

/// Per-job configuration captured from [`JobInfo`] at runtime start.
#[derive(Debug, Clone, Default)]
struct JobRuntimeSettings {
    max_attempts: u32,
    initial_backoff_ms: u64,
    /// Named queue the job is routed to (defaults to `"default"`).
    queue: String,
    uniqueness: Option<JobUniqueness>,
    concurrency: Option<JobConcurrency>,
}

#[cfg(test)]
impl JobRuntimeSettings {
    fn basic(max_attempts: u32, initial_backoff_ms: u64) -> Self {
        Self {
            max_attempts,
            initial_backoff_ms,
            ..Self::default()
        }
    }
}

/// Uniqueness/concurrency values resolved against one concrete payload.
#[derive(Debug, Clone, Default)]
struct ResolvedJobConstraints {
    unique_key: Option<String>,
    unique_window: Option<JobUniquenessWindow>,
    concurrency_limit: Option<u32>,
    concurrency_scope: Option<String>,
}

impl ResolvedJobConstraints {
    fn for_payload(settings: &JobRuntimeSettings, payload: &Value) -> Self {
        let (unique_key, unique_window) = settings.uniqueness.as_ref().map_or((None, None), |u| {
            (Some(job_unique_key(u, payload)), Some(u.window))
        });
        let (concurrency_limit, concurrency_scope) =
            settings.concurrency.as_ref().map_or((None, None), |c| {
                (Some(c.limit), job_concurrency_scope(c, payload))
            });
        Self {
            unique_key,
            unique_window,
            concurrency_limit,
            concurrency_scope,
        }
    }

    #[cfg(any(feature = "redis", feature = "db"))]
    fn unique_window_tag(&self) -> Option<&'static str> {
        self.unique_window.map(JobUniquenessWindow::tag)
    }
}

/// Whether an enqueue stored a new job or coalesced into an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Queued,
    Deduplicated,
}

/// Specifies the due instant for an after-commit enqueue.
///
/// `At` carries a pre-resolved absolute instant (or `None` for immediate).
/// `After` carries a relative delay to be converted to an absolute instant
/// **inside the after-commit callback** so the delay is measured from commit
/// time rather than from the original API call time.
#[derive(Debug, Clone, Copy)]
enum AfterCommitDue {
    At(Option<chrono::DateTime<chrono::Utc>>),
    After(std::time::Duration),
}

#[derive(Debug)]
struct QueuedJob {
    id: String,
    name: String,
    /// Named queue this job is routed to (defaults to `"default"`).
    queue: String,
    payload: Value,
    attempt: u32,
    max_attempts: u32,
    initial_backoff_ms: u64,
    /// W3C `traceparent` serialized at enqueue time.  `None` when the
    /// `telemetry-otlp` feature is disabled or no active span was present.
    #[cfg(feature = "telemetry-otlp")]
    traceparent: Option<String>,
    /// W3C `tracestate` serialized at enqueue time.
    #[cfg(feature = "telemetry-otlp")]
    tracestate: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum JobExecutionOutcome {
    Succeeded,
    Failed(String),
    Panicked(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobAdminStartDecision {
    Started,
    Canceled,
    Missing,
    AlreadyTransitioned,
}

/// Boxed future returned by job-admin backends.
pub type JobAdminFuture<'a, T> = Pin<Box<dyn Future<Output = AutumnResult<T>> + Send + 'a>>;

/// Human-facing lifecycle status for a background job entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobAdminStatus {
    /// Waiting to be picked up by a worker.
    Enqueued,
    /// Enqueued with a future due time (delayed/one-shot scheduled work).
    /// Not visible to workers until the due time passes.
    Scheduled,
    /// Currently executing in a worker.
    Running,
    /// Failed but already scheduled for an automatic retry.
    Retrying,
    /// Finished successfully.
    Completed,
    /// Finished with a terminal error.
    Failed,
    /// Removed from the failed set by an operator.
    Discarded,
    /// Canceled before it started.
    Canceled,
    /// Re-enqueued by an operator from a failed entry.
    Retried,
    /// Coalesced at enqueue time into an already-held unique job.
    Deduplicated,
}

impl JobAdminStatus {
    /// Stable display string used by the admin UI.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Enqueued => "enqueued",
            Self::Scheduled => "scheduled",
            Self::Running => "running",
            Self::Retrying => "retrying",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Discarded => "discarded",
            Self::Canceled => "canceled",
            Self::Retried => "retried",
            Self::Deduplicated => "deduplicated",
        }
    }
}

/// A job row exposed to the admin dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct JobAdminRecord {
    /// Stable runtime id for this job attempt.
    pub id: String,
    /// Job kind/name from `#[job(name = "...")]`.
    pub name: String,
    /// Named queue the job is routed to (from `#[job(queue = "...")]`).
    pub queue: String,
    /// Current lifecycle status.
    pub status: JobAdminStatus,
    /// Time the job entered the queue.
    pub enqueued_at: Option<String>,
    /// Due time for a delayed/scheduled job, if it is not yet runnable.
    pub scheduled_for: Option<String>,
    /// Time the job started running.
    pub started_at: Option<String>,
    /// Time the job finished, failed, or was operated on.
    pub finished_at: Option<String>,
    /// Current attempt number.
    pub attempt: u32,
    /// Maximum attempts configured for this job.
    pub max_attempts: u32,
    /// Last observed error, if any.
    pub last_error: Option<String>,
    /// Principal/user id extracted from common payload fields, if present.
    pub principal_id: Option<String>,
    /// Correlation/request id extracted from common payload fields, if present.
    pub correlation_id: Option<String>,
}

/// Paginated records for one job status group.
#[derive(Debug, Clone, Serialize)]
pub struct JobAdminPage {
    /// Records for the requested page, sorted newest-first.
    pub records: Vec<JobAdminRecord>,
    /// Total records matching this status/time window.
    pub total: u64,
    /// Current page number, 1-indexed.
    pub page: u64,
    /// Records per page.
    pub per_page: u64,
}

impl JobAdminPage {
    /// Construct a page from preselected records.
    #[must_use]
    pub const fn new(records: Vec<JobAdminRecord>, total: u64, page: u64, per_page: u64) -> Self {
        Self {
            records,
            total,
            page,
            per_page,
        }
    }

    /// Total page count for this status group.
    #[must_use]
    pub const fn total_pages(&self) -> u64 {
        if self.per_page == 0 {
            return 0;
        }
        self.total.div_ceil(self.per_page)
    }
}

/// Scheduled task summary shown alongside ad-hoc jobs.
#[derive(Debug, Clone, Serialize)]
pub struct JobScheduleSummary {
    /// Registered scheduled task name.
    pub name: String,
    /// Human-readable schedule expression.
    pub schedule: String,
    /// Next scheduled run time, if the scheduler backend can report it.
    pub next_run_at: Option<String>,
    /// Last run result/status, if any.
    pub last_run_status: Option<String>,
}

/// Complete dashboard snapshot for `/admin/jobs`.
#[derive(Debug, Clone, Serialize)]
pub struct JobAdminSnapshot {
    /// Enqueued jobs, newest-first.
    pub enqueued: JobAdminPage,
    /// Scheduled (delayed) jobs awaiting their due time, soonest-due first.
    pub scheduled: JobAdminPage,
    /// Running jobs, newest-first.
    pub running: JobAdminPage,
    /// Completed jobs from the last 24 hours, newest-first.
    pub completed: JobAdminPage,
    /// Failed jobs from the last 7 days, newest-first.
    pub failed: JobAdminPage,
    /// Scheduled task summaries.
    pub schedules: Vec<JobScheduleSummary>,
    /// Maximum number of lifecycle entries retained by the default backend.
    pub bounded_history_limit: usize,
}

impl JobAdminSnapshot {
    /// Empty snapshot for apps that have not initialized a jobs runtime.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            enqueued: JobAdminPage::new(Vec::new(), 0, 1, DEFAULT_JOB_ADMIN_PER_PAGE),
            scheduled: JobAdminPage::new(Vec::new(), 0, 1, DEFAULT_JOB_ADMIN_PER_PAGE),
            running: JobAdminPage::new(Vec::new(), 0, 1, DEFAULT_JOB_ADMIN_PER_PAGE),
            completed: JobAdminPage::new(Vec::new(), 0, 1, DEFAULT_JOB_ADMIN_PER_PAGE),
            failed: JobAdminPage::new(Vec::new(), 0, 1, DEFAULT_JOB_ADMIN_PER_PAGE),
            schedules: Vec::new(),
            bounded_history_limit: DEFAULT_JOB_ADMIN_HISTORY_LIMIT,
        }
    }
}

/// Per-list pagination for the job dashboard.
#[derive(Debug, Clone)]
pub struct JobAdminQuery {
    /// Page number for enqueued jobs.
    pub enqueued_page: u64,
    /// Page number for scheduled (delayed) jobs.
    pub scheduled_page: u64,
    /// Page number for running jobs.
    pub running_page: u64,
    /// Page number for completed jobs.
    pub completed_page: u64,
    /// Page number for failed jobs.
    pub failed_page: u64,
    /// Shared page size for all lists.
    pub per_page: u64,
}

impl Default for JobAdminQuery {
    fn default() -> Self {
        Self {
            enqueued_page: 1,
            scheduled_page: 1,
            running_page: 1,
            completed_page: 1,
            failed_page: 1,
            per_page: DEFAULT_JOB_ADMIN_PER_PAGE,
        }
    }
}

/// Read/operate surface consumed by first-party and custom job dashboards.
///
/// The default implementation is process-local and bounded. Durable external
/// queues can install their own backend in [`AppState`] by inserting
/// [`JobAdminBackendEntry`].
pub trait JobAdminBackend: Send + Sync + 'static {
    /// Return the dashboard snapshot for the supplied pagination.
    fn snapshot(&self, query: JobAdminQuery) -> JobAdminFuture<'_, JobAdminSnapshot>;

    /// Retry a failed job using its original payload.
    fn retry(&self, id: &str) -> JobAdminFuture<'_, ()>;

    /// Discard a failed job so it no longer appears in the failed list.
    fn discard(&self, id: &str) -> JobAdminFuture<'_, ()>;

    /// Cancel an enqueued job that has not started.
    fn cancel(&self, id: &str) -> JobAdminFuture<'_, ()>;
}

/// Typed [`AppState`] extension carrying a job-admin backend.
#[derive(Clone)]
pub struct JobAdminBackendEntry(pub Arc<dyn JobAdminBackend>);

/// Resolve the active job-admin backend from application state.
#[must_use]
pub fn job_admin_backend(state: &AppState) -> Option<Arc<dyn JobAdminBackend>> {
    state
        .extension::<JobAdminBackendEntry>()
        .map(|entry| Arc::clone(&entry.0))
}

#[derive(Debug, Clone)]
struct JobAdminStoredRecord {
    id: String,
    name: String,
    queue: String,
    payload: Value,
    status: JobAdminStatus,
    enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    scheduled_for: Option<chrono::DateTime<chrono::Utc>>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
    attempt: u32,
    max_attempts: u32,
    last_error: Option<String>,
    principal_id: Option<String>,
    correlation_id: Option<String>,
}

impl JobAdminStoredRecord {
    fn sort_time(&self) -> chrono::DateTime<chrono::Utc> {
        self.finished_at
            .or(self.started_at)
            .or(self.enqueued_at)
            .unwrap_or_else(chrono::Utc::now)
    }

    fn to_public(&self) -> JobAdminRecord {
        JobAdminRecord {
            id: self.id.clone(),
            name: self.name.clone(),
            queue: self.queue.clone(),
            status: self.status,
            enqueued_at: self.enqueued_at.map(format_job_admin_time),
            scheduled_for: self.scheduled_for.map(format_job_admin_time),
            started_at: self.started_at.map(format_job_admin_time),
            finished_at: self.finished_at.map(format_job_admin_time),
            attempt: self.attempt,
            max_attempts: self.max_attempts,
            last_error: self.last_error.clone(),
            principal_id: self.principal_id.clone(),
            correlation_id: self.correlation_id.clone(),
        }
    }
}

#[derive(Debug)]
struct JobAdminMemoryInner {
    records: HashMap<String, JobAdminStoredRecord>,
    order: VecDeque<String>,
    history_limit: usize,
    /// Cancellation tokens for in-flight local delayed timers, keyed by job id.
    /// When a Scheduled local job is canceled via the admin API, the token is
    /// fired so the spawned timer task exits immediately and releases the unique
    /// lock rather than holding it until the original due time fires.
    delay_cancelers: HashMap<String, tokio_util::sync::CancellationToken>,
}

/// Bounded process-local job dashboard backend used by the built-in runtime.
#[derive(Clone)]
pub struct JobAdminMemoryBackend {
    inner: Arc<RwLock<JobAdminMemoryInner>>,
}

impl JobAdminMemoryBackend {
    /// Create a backend retaining the default number of lifecycle entries.
    #[must_use]
    pub fn new() -> Self {
        Self::with_history_limit(DEFAULT_JOB_ADMIN_HISTORY_LIMIT)
    }

    /// Create a backend retaining at most `history_limit` finished entries.
    #[must_use]
    pub fn with_history_limit(history_limit: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(JobAdminMemoryInner {
                records: HashMap::new(),
                order: VecDeque::new(),
                history_limit: history_limit.max(1),
                delay_cancelers: HashMap::new(),
            })),
        }
    }

    /// Record an enqueue that may carry a future due time. When `due_at` is in
    /// the future the record starts in the [`JobAdminStatus::Scheduled`] state
    /// so the dashboard surfaces it as a delayed job until it becomes runnable.
    #[allow(clippy::too_many_arguments)]
    fn record_enqueue_due(
        &self,
        id: String,
        name: &str,
        queue: &str,
        payload: Value,
        attempt: u32,
        max_attempts: u32,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        let (principal_id, correlation_id) = job_payload_identity(&payload);
        let scheduled_for = due_at.filter(|due| *due > now);
        let status = if scheduled_for.is_some() {
            JobAdminStatus::Scheduled
        } else {
            JobAdminStatus::Enqueued
        };
        if let Ok(mut inner) = self.inner.write() {
            inner.order.push_back(id.clone());
            inner.records.insert(
                id.clone(),
                JobAdminStoredRecord {
                    id,
                    name: name.to_owned(),
                    queue: normalize_queue_name(queue),
                    payload,
                    status,
                    enqueued_at: Some(now),
                    scheduled_for,
                    started_at: None,
                    finished_at: None,
                    attempt,
                    max_attempts,
                    last_error: None,
                    principal_id,
                    correlation_id,
                },
            );
            prune_job_admin_history(&mut inner);
        }
    }

    /// Transition a job back to [`JobAdminStatus::Enqueued`] (retry or promotion
    /// from the delayed ZSET) and return `true` when the prior status was
    /// `Scheduled`.  Callers use this to avoid double-incrementing the `queued`
    /// actuator counter for initially-delayed jobs that were already counted at
    /// enqueue time.
    fn record_requeued(&self, id: &str, attempt: u32) -> bool {
        if let Ok(mut inner) = self.inner.write()
            && let Some(record) = inner.records.get_mut(id)
        {
            let was_scheduled = record.status == JobAdminStatus::Scheduled;
            record.status = JobAdminStatus::Enqueued;
            record.enqueued_at = Some(chrono::Utc::now());
            record.scheduled_for = None;
            record.started_at = None;
            record.finished_at = None;
            record.attempt = attempt;
            return was_scheduled;
        }
        false
    }

    fn try_record_start(&self, id: &str, attempt: u32) -> JobAdminStartDecision {
        let Ok(mut inner) = self.inner.write() else {
            return JobAdminStartDecision::Missing;
        };
        // Always clean up any delay canceler — the timer has fired so the
        // token is no longer needed regardless of the transition outcome.
        inner.delay_cancelers.remove(id);
        let Some(record) = inner.records.get_mut(id) else {
            return JobAdminStartDecision::Missing;
        };
        match record.status {
            // A `Scheduled` job becomes runnable once its delayed-send fires, at
            // which point it starts like any other enqueued job.
            JobAdminStatus::Enqueued | JobAdminStatus::Scheduled => {
                record.status = JobAdminStatus::Running;
                record.started_at = Some(chrono::Utc::now());
                record.scheduled_for = None;
                record.finished_at = None;
                record.attempt = attempt;
                JobAdminStartDecision::Started
            }
            JobAdminStatus::Canceled => JobAdminStartDecision::Canceled,
            _ => JobAdminStartDecision::AlreadyTransitioned,
        }
    }

    fn record_success(&self, id: &str) {
        if let Ok(mut inner) = self.inner.write()
            && let Some(record) = inner.records.get_mut(id)
        {
            record.status = JobAdminStatus::Completed;
            record.finished_at = Some(chrono::Utc::now());
            record.last_error = None;
            prune_job_admin_history(&mut inner);
        }
    }

    fn record_retrying(&self, id: &str, error: &str) {
        if let Ok(mut inner) = self.inner.write()
            && let Some(record) = inner.records.get_mut(id)
        {
            record.status = JobAdminStatus::Retrying;
            record.finished_at = Some(chrono::Utc::now());
            record.last_error = Some(error.to_owned());
        }
    }

    fn record_failure(&self, id: &str, error: String) {
        if let Ok(mut inner) = self.inner.write()
            && let Some(record) = inner.records.get_mut(id)
        {
            record.status = JobAdminStatus::Failed;
            record.finished_at = Some(chrono::Utc::now());
            record.last_error = Some(error);
            prune_job_admin_history(&mut inner);
        }
    }

    fn record_cancelled(&self, id: &str) {
        if let Ok(mut inner) = self.inner.write()
            && let Some(record) = inner.records.get_mut(id)
        {
            record.status = JobAdminStatus::Canceled;
            record.finished_at = Some(chrono::Utc::now());
        }
    }

    fn record_deduplicated(&self, id: &str) {
        if let Ok(mut inner) = self.inner.write()
            && let Some(record) = inner.records.get_mut(id)
        {
            record.status = JobAdminStatus::Deduplicated;
            record.finished_at = Some(chrono::Utc::now());
            prune_job_admin_history(&mut inner);
        }
    }

    fn retry_payload(&self, id: &str) -> AutumnResult<(String, Value)> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| AutumnError::internal_server_error_msg("job admin store lock poisoned"))?;
        let record = inner
            .records
            .get_mut(id)
            .ok_or_else(|| AutumnError::not_found_msg(format!("job '{id}' not found")))?;
        if record.status != JobAdminStatus::Failed {
            return Err(AutumnError::bad_request_msg(
                "only failed jobs can be retried",
            ));
        }
        let retry = (record.name.clone(), record.payload.clone());
        record.status = JobAdminStatus::Retried;
        record.finished_at = Some(chrono::Utc::now());
        drop(inner);
        Ok(retry)
    }

    fn restore_failed_retry(&self, id: &str) {
        if let Ok(mut inner) = self.inner.write()
            && let Some(record) = inner.records.get_mut(id)
            && record.status == JobAdminStatus::Retried
        {
            record.status = JobAdminStatus::Failed;
            record.finished_at = Some(chrono::Utc::now());
        }
    }

    fn ensure_retryable(&self, id: &str) -> AutumnResult<()> {
        let inner = self
            .inner
            .read()
            .map_err(|_| AutumnError::internal_server_error_msg("job admin store lock poisoned"))?;
        let record = inner
            .records
            .get(id)
            .ok_or_else(|| AutumnError::not_found_msg(format!("job '{id}' not found")))?;
        let status = record.status;
        drop(inner);
        if status != JobAdminStatus::Failed {
            return Err(AutumnError::bad_request_msg(
                "only failed jobs can be retried",
            ));
        }
        Ok(())
    }

    fn discard_failed(&self, id: &str) -> AutumnResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| AutumnError::internal_server_error_msg("job admin store lock poisoned"))?;
        let record = inner
            .records
            .get_mut(id)
            .ok_or_else(|| AutumnError::not_found_msg(format!("job '{id}' not found")))?;
        if record.status != JobAdminStatus::Failed {
            return Err(AutumnError::bad_request_msg(
                "only failed jobs can be discarded",
            ));
        }
        record.status = JobAdminStatus::Discarded;
        record.finished_at = Some(chrono::Utc::now());
        drop(inner);
        Ok(())
    }

    fn cancel_enqueued(&self, id: &str) -> AutumnResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| AutumnError::internal_server_error_msg("job admin store lock poisoned"))?;
        let record = inner
            .records
            .get_mut(id)
            .ok_or_else(|| AutumnError::not_found_msg(format!("job '{id}' not found")))?;
        if !matches!(
            record.status,
            JobAdminStatus::Enqueued | JobAdminStatus::Scheduled
        ) {
            return Err(AutumnError::bad_request_msg(
                "only enqueued or scheduled jobs can be canceled",
            ));
        }
        record.status = JobAdminStatus::Canceled;
        record.scheduled_for = None;
        record.finished_at = Some(chrono::Utc::now());
        // Pull any pending timer canceler out while we still hold the lock.
        let canceler = inner.delay_cancelers.remove(id);
        drop(inner);
        // Fire outside the lock so the spawned timer task can release the
        // unique lock without waiting for the write-lock to clear.
        if let Some(token) = canceler {
            token.cancel();
        }
        Ok(())
    }

    /// Register a cancellation token for a local delayed timer.  When the
    /// admin cancels a Scheduled job the token is fired so the timer exits and
    /// releases the unique lock immediately rather than holding it until due.
    ///
    /// Returns `true` when the record is already `Canceled` (the admin canceled
    /// the job in the window between `record_enqueue_due` and here); the caller
    /// must then skip the timer entirely and clean up the unique lock and gauge.
    fn register_delay_canceler(
        &self,
        id: String,
        token: tokio_util::sync::CancellationToken,
    ) -> bool {
        if let Ok(mut inner) = self.inner.write() {
            // If admin already canceled during an interceptor, do not register
            // the token — the caller will do immediate cleanup instead.
            if inner
                .records
                .get(&id)
                .is_some_and(|r| r.status == JobAdminStatus::Canceled)
            {
                return true;
            }
            inner.delay_cancelers.insert(id, token);
        }
        false
    }

    fn snapshot_sync(&self, query: &JobAdminQuery) -> JobAdminSnapshot {
        let Ok(inner) = self.inner.read() else {
            return JobAdminSnapshot::empty();
        };
        let now = chrono::Utc::now();
        let per_page = query.per_page.clamp(1, 100);
        JobAdminSnapshot {
            enqueued: paginate_job_admin_records(
                &inner,
                JobAdminStatus::Enqueued,
                None,
                query.enqueued_page,
                per_page,
            ),
            scheduled: paginate_job_admin_records(
                &inner,
                JobAdminStatus::Scheduled,
                None,
                query.scheduled_page,
                per_page,
            ),
            running: paginate_job_admin_records(
                &inner,
                JobAdminStatus::Running,
                None,
                query.running_page,
                per_page,
            ),
            completed: paginate_job_admin_records(
                &inner,
                JobAdminStatus::Completed,
                Some(now - chrono::TimeDelta::hours(24)),
                query.completed_page,
                per_page,
            ),
            failed: paginate_job_admin_records(
                &inner,
                JobAdminStatus::Failed,
                Some(now - chrono::TimeDelta::days(7)),
                query.failed_page,
                per_page,
            ),
            schedules: Vec::new(),
            bounded_history_limit: inner.history_limit,
        }
    }

    #[cfg(test)]
    fn new_for_test(history_limit: usize) -> Self {
        Self::with_history_limit(history_limit)
    }

    #[cfg(test)]
    fn record_enqueue_for_test(
        &self,
        name: &str,
        payload: Value,
        attempt: u32,
        max_attempts: u32,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.record_enqueue_due(
            id.clone(),
            name,
            DEFAULT_QUEUE,
            payload,
            attempt,
            max_attempts,
            None,
            chrono::Utc::now(),
        );
        id
    }

    #[cfg(test)]
    fn record_start_for_test(&self, id: &str, attempt: u32) {
        let _ = self.try_record_start(id, attempt);
    }

    #[cfg(test)]
    fn record_success_for_test(&self, id: &str) {
        self.record_success(id);
    }

    #[cfg(test)]
    fn record_failure_for_test(&self, id: &str, error: &str) {
        self.record_failure(id, error.to_owned());
    }
}

impl Default for JobAdminMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl JobAdminBackend for JobAdminMemoryBackend {
    fn snapshot(&self, query: JobAdminQuery) -> JobAdminFuture<'_, JobAdminSnapshot> {
        Box::pin(async move { Ok(self.snapshot_sync(&query)) })
    }

    fn retry(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move {
            self.ensure_retryable(&id)?;
            let client = global_job_client().ok_or_else(|| {
                AutumnError::service_unavailable_msg("job runtime is not initialized")
            })?;
            let (name, payload) = self.retry_payload(&id)?;
            match client.enqueue_with_outcome(&name, payload).await {
                Ok(EnqueueOutcome::Queued) => Ok(()),
                Ok(EnqueueOutcome::Deduplicated) => {
                    // No retry was actually queued: an equivalent unique job
                    // already holds the key. Restore the failed record and
                    // surface the same conflict the durable backends report.
                    self.restore_failed_retry(&id);
                    Err(AutumnError::bad_request_msg(
                        "an equivalent unique job is already pending or running; \
                         retry after it settles",
                    ))
                }
                Err(error) => {
                    self.restore_failed_retry(&id);
                    Err(error)
                }
            }
        })
    }

    fn discard(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.discard_failed(&id) })
    }

    fn cancel(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.cancel_enqueued(&id) })
    }
}

fn format_job_admin_time(time: chrono::DateTime<chrono::Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn prune_job_admin_history(inner: &mut JobAdminMemoryInner) {
    let mut scanned = 0;
    while inner.order.len() > inner.history_limit && scanned < inner.order.len() {
        let Some(id) = inner.order.pop_front() else {
            break;
        };
        let is_active = inner.records.get(&id).is_some_and(|record| {
            matches!(
                record.status,
                JobAdminStatus::Enqueued
                    | JobAdminStatus::Scheduled
                    | JobAdminStatus::Running
                    | JobAdminStatus::Retrying
            )
        });
        if is_active {
            inner.order.push_back(id);
            scanned += 1;
        } else {
            inner.records.remove(&id);
        }
    }
}

fn paginate_job_admin_records(
    inner: &JobAdminMemoryInner,
    status: JobAdminStatus,
    since: Option<chrono::DateTime<chrono::Utc>>,
    page: u64,
    per_page: u64,
) -> JobAdminPage {
    let page = page.max(1);
    let mut records: Vec<_> = inner
        .records
        .values()
        .filter(|record| {
            record.status == status
                && since.is_none_or(|cutoff| {
                    record
                        .finished_at
                        .or(record.started_at)
                        .or(record.enqueued_at)
                        .is_some_and(|time| time >= cutoff)
                })
        })
        .cloned()
        .collect();
    records.sort_by_key(JobAdminStoredRecord::sort_time);
    records.reverse();

    let total = records.len() as u64;
    let start =
        usize::try_from(page.saturating_sub(1).saturating_mul(per_page)).unwrap_or(usize::MAX);
    let take = usize::try_from(per_page).unwrap_or(usize::MAX);
    let page_records = records
        .into_iter()
        .skip(start)
        .take(take)
        .map(|record| record.to_public())
        .collect();

    JobAdminPage::new(page_records, total, page, per_page)
}

/// Append a canonical (sorted-key) JSON encoding of `value` to `out`.
///
/// `serde_json::to_string` is already deterministic for a given `Value`, but
/// two semantically-equal payloads can carry different key orders (e.g. when
/// built manually vs. via struct serialization). Sorting object keys makes the
/// derived unique key stable across producers and app instances.
fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                write_canonical_json(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// FNV-1a 64-bit hash: deterministic across processes, releases, and replicas,
/// unlike `std::hash::DefaultHasher` whose output is not a stability guarantee.
fn fnv1a_64(input: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Derive the uniqueness key for a job payload.
///
/// With `unique_by` fields configured the key concatenates the canonical JSON
/// of each selected field (missing fields read as `null`); otherwise it is a
/// stable hash of the full canonicalized payload.
fn job_unique_key(uniqueness: &JobUniqueness, payload: &Value) -> String {
    let (_, payload) = crate::job_tracking::split_tracked_payload(payload);
    if uniqueness.by.is_empty() {
        let mut canonical = String::new();
        write_canonical_json(payload, &mut canonical);
        return format!("args:{:016x}", fnv1a_64(&canonical));
    }
    let mut key = String::new();
    for (index, field) in uniqueness.by.iter().enumerate() {
        if index > 0 {
            key.push('\u{1f}');
        }
        key.push_str(field);
        key.push('=');
        let value = payload.get(field).unwrap_or(&Value::Null);
        write_canonical_json(value, &mut key);
    }
    key
}

/// Resolve the concurrency scope value for a job payload.
///
/// Returns `None` when the limit is unscoped (one shared slot pool per job
/// type). A configured-but-missing field reads as canonical `null` so all
/// payloads lacking the field share one scope.
fn job_concurrency_scope(concurrency: &JobConcurrency, payload: &Value) -> Option<String> {
    let (_, payload) = crate::job_tracking::split_tracked_payload(payload);
    concurrency.key.as_ref().map(|field| {
        let mut scope = String::new();
        write_canonical_json(payload.get(field).unwrap_or(&Value::Null), &mut scope);
        scope
    })
}

fn job_payload_identity(payload: &Value) -> (Option<String>, Option<String>) {
    let (_, payload) = crate::job_tracking::split_tracked_payload(payload);
    let principal = first_payload_string(payload, &["principal_id", "principal", "user_id"]);
    let correlation = first_payload_string(payload, &["correlation_id", "request_id"]);
    (principal, correlation)
}

fn first_payload_string(payload: &Value, keys: &[&str]) -> Option<String> {
    let object = payload.as_object()?;
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        if let Some(raw) = value.as_str() {
            if !raw.is_empty() {
                return Some(raw.to_owned());
            }
        } else if value.is_number() || value.is_boolean() {
            return Some(value.to_string());
        }
    }
    None
}

fn default_job_admin_backend_for_state(state: &AppState) -> JobAdminMemoryBackend {
    let backend = JobAdminMemoryBackend::new();
    if job_admin_backend(state).is_none() {
        state.insert_extension(JobAdminBackendEntry(Arc::new(backend.clone())));
    }
    backend
}

#[cfg(feature = "redis")]
fn default_redis_queue() -> String {
    DEFAULT_QUEUE.to_string()
}

#[cfg(feature = "redis")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RedisJobRecord {
    id: String,
    name: String,
    /// Named queue (defaults to `"default"`; `serde(default)` keeps records
    /// written before queues existed readable after an upgrade).
    #[serde(default = "default_redis_queue")]
    queue: String,
    payload: Value,
    attempt: u32,
    max_attempts: u32,
    initial_backoff_ms: u64,
    #[serde(default)]
    enqueued_at_ms: Option<u64>,
    #[serde(default)]
    started_at_ms: Option<u64>,
    #[serde(default)]
    finished_at_ms: Option<u64>,
    #[serde(default)]
    claimed_by: Option<String>,
    #[serde(default)]
    claimed_at_ms: Option<u64>,
    #[serde(default)]
    last_error: Option<String>,
    /// Resolved uniqueness key; absent for non-unique jobs.
    ///
    /// `skip_serializing_if` keeps the field truly absent (not `null`) so the
    /// claim script's `record['unique_key']` checks read `nil` in Lua.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unique_key: Option<String>,
    /// Uniqueness window tag: "pending", "running", or "ttl".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unique_window: Option<String>,
    /// Resolved concurrency scope value; absent for unscoped limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    concurrency_key: Option<String>,
    /// In-flight cap for this job's concurrency group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    concurrency_limit: Option<u32>,
    /// W3C `traceparent` captured when the job was enqueued.
    #[cfg(feature = "telemetry-otlp")]
    #[serde(default)]
    traceparent: Option<String>,
    /// W3C `tracestate` captured when the job was enqueued.
    #[cfg(feature = "telemetry-otlp")]
    #[serde(default)]
    tracestate: Option<String>,
}

#[cfg(all(feature = "redis", test))]
#[derive(Debug, Clone)]
struct RedisClaimedRecord {
    record: RedisJobRecord,
    deadline_ms: u64,
}

#[cfg(feature = "redis")]
#[derive(Debug, Clone)]
struct RedisRetrySchedule {
    record: RedisJobRecord,
    due_at_ms: u64,
}

#[cfg(feature = "redis")]
#[derive(Debug, Clone)]
enum RedisFailureAction {
    Retry(RedisRetrySchedule),
    DeadLetter(RedisJobRecord),
}

#[cfg(feature = "redis")]
#[derive(Debug, Clone)]
enum RedisStaleRecovery {
    Requeue(RedisJobRecord),
    DeadLetter(RedisJobRecord),
}

#[cfg(feature = "redis")]
struct RedisMaintenanceThrottle {
    next_run_at: std::time::Instant,
    interval: std::time::Duration,
}

#[cfg(feature = "redis")]
impl RedisMaintenanceThrottle {
    const fn new(now: std::time::Instant, interval: std::time::Duration) -> Self {
        Self {
            next_run_at: now,
            interval,
        }
    }

    fn take_due(&mut self, now: std::time::Instant) -> bool {
        if now < self.next_run_at {
            return false;
        }
        self.next_run_at = now + self.interval;
        true
    }
}

#[cfg(feature = "redis")]
const REDIS_STALE_MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a parked (concurrency-blocked) job waits before re-entering the
/// queue for another claim attempt.
#[cfg(feature = "redis")]
const REDIS_CONCURRENCY_REQUEUE_DELAY_MS: u64 = 100;

/// Cadence for promoting parked jobs back into the queue.
#[cfg(feature = "redis")]
const REDIS_BLOCKED_PROMOTION_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(REDIS_CONCURRENCY_REQUEUE_DELAY_MS);

/// Maximum queue entries one claim call will scan past blocked jobs.
#[cfg(feature = "redis")]
const REDIS_CLAIM_SCAN_LIMIT: usize = 8;

/// Safety TTL on unique locks for the pending/running windows.
///
/// Those locks are normally released by the claim/transition scripts; the TTL
/// only bounds the damage if the job record itself is lost (e.g. a flushed
/// keyspace), so a dead key can never deadlock uniqueness forever.
#[cfg(feature = "redis")]
const REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS: u64 = 86_400_000;

#[cfg(feature = "redis")]
fn redis_unique_lock_key(unique_prefix: &str, name: &str, unique_key: &str) -> String {
    format!("{unique_prefix}{name}:{unique_key}")
}

#[cfg(feature = "redis")]
fn redis_concurrency_counter_key(
    concurrency_prefix: &str,
    name: &str,
    scope: Option<&str>,
) -> String {
    format!("{concurrency_prefix}{name}:{}", scope.unwrap_or(""))
}

/// Lock TTL for a unique job record: the window TTL itself, or the crash
/// backstop for the pending/running windows.
#[cfg(feature = "redis")]
const fn redis_unique_lock_ttl_ms(window: Option<JobUniquenessWindow>) -> u64 {
    match window {
        Some(JobUniquenessWindow::TtlMs(ms)) => ms,
        _ => REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS,
    }
}

/// Whether a settling transition should release the unique lock.
///
/// TTL-window locks expire by time so a burst keeps coalescing after
/// completion; retry transitions keep the lock because the job is still in
/// flight.
#[cfg(feature = "redis")]
fn redis_release_unique_on_settle(record: &RedisJobRecord, mode: &str) -> bool {
    record.unique_key.is_some()
        && record.unique_window.as_deref() != Some("ttl")
        && matches!(mode, "success" | "dead")
}

/// Lock maintenance a requeueing transition (retry backoff, stale requeue)
/// must perform: pending-window keys were released at claim and need to be
/// re-acquired for the again-pending job; running-window locks get their
/// crash backstop refreshed so long-lived jobs never outlive their lock.
#[cfg(feature = "redis")]
fn redis_requeue_unique_action(record: &RedisJobRecord) -> &'static str {
    if record.unique_key.is_none() {
        return "";
    }
    match record.unique_window.as_deref() {
        Some("pending") => "pending",
        Some("running") => "running",
        _ => "",
    }
}

#[cfg(feature = "redis")]
const REDIS_WORKER_IDLE_SLEEP_MAX: std::time::Duration = std::time::Duration::from_millis(200);

/// Maximum delay between delayed-ZSET promotion scans.
///
/// One-shot delayed jobs (`enqueue_in` / `enqueue_at`) share the same ZSET as
/// retries.  Without a cap, a deployment whose jobs all have large retry
/// backoffs (e.g. 60 s) would inherit that long promotion interval, causing
/// short-delay jobs to run far later than requested.  Capping at 1 s keeps
/// timing accurate while adding only negligible Redis load (ZRANGEBYSCORE is
/// O(log N)).
#[cfg(feature = "redis")]
const REDIS_DELAYED_PROMOTION_MAX_INTERVAL_MS: u64 = 1_000;

#[cfg(feature = "redis")]
fn redis_retry_promotion_interval_ms(default_backoff_ms: u64, jobs: &[JobInfo]) -> u64 {
    let mut interval_ms = default_backoff_ms.max(1);
    for job in jobs {
        if job.initial_backoff_ms > 0 {
            interval_ms = interval_ms.min(job.initial_backoff_ms);
        }
    }
    interval_ms.min(REDIS_DELAYED_PROMOTION_MAX_INTERVAL_MS)
}

#[cfg(feature = "redis")]
fn redis_worker_idle_sleep(retry_promotion_interval: std::time::Duration) -> std::time::Duration {
    retry_promotion_interval.min(REDIS_WORKER_IDLE_SLEEP_MAX)
}

#[cfg(feature = "redis")]
#[derive(Clone)]
struct RedisWorkerConfig {
    /// Default-queue list key (`{prefix}:queue`); retained for the test suite
    /// and as the promotion fallback target.
    #[cfg_attr(not(test), allow(dead_code))]
    queue_key: String,
    /// Base key prefix used to derive per-queue list keys.
    key_prefix: String,
    /// Priority drain schedule across named queues.
    schedule: QueueSchedule,
    processing_key: String,
    delayed_key: String,
    dead_key: String,
    completed_key: String,
    blocked_key: String,
    record_prefix: String,
    dead_record_prefix: String,
    unique_prefix: String,
    concurrency_prefix: String,
    worker_id: String,
    visibility_timeout_ms: u64,
    default_attempts: u32,
    default_backoff: u64,
    retry_promotion_interval: std::time::Duration,
}

#[cfg(feature = "redis")]
impl RedisWorkerConfig {
    /// Ordered queue list keys to attempt for one claim iteration.
    fn queue_keys_for(&self, order: &[String]) -> Vec<String> {
        order
            .iter()
            .map(|queue| redis_queue_key(&self.key_prefix, queue))
            .collect()
    }
}

#[cfg(feature = "redis")]
impl RedisWorkerConfig {
    fn unique_lock_key_for(&self, record: &RedisJobRecord) -> String {
        record.unique_key.as_deref().map_or_else(
            || format!("{}-", self.unique_prefix),
            |key| redis_unique_lock_key(&self.unique_prefix, &record.name, key),
        )
    }

    fn concurrency_counter_key_for(&self, record: &RedisJobRecord) -> String {
        redis_concurrency_counter_key(
            &self.concurrency_prefix,
            &record.name,
            record.concurrency_key.as_deref(),
        )
    }
}

static GLOBAL_JOB_CLIENT: OnceLock<RwLock<Option<Arc<JobClient>>>> = OnceLock::new();

pub fn global_job_runtime_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

// ── W3C Trace Context helpers ────────────────────────────────────────────────
//
// These are compiled only when `telemetry-otlp` is enabled.  The inject/
// extract helpers use a plain `HashMap` as the carrier so no HTTP crate is
// required here.

/// Serialize the current active span's W3C trace context into portable
/// strings `(traceparent, tracestate)`.  Returns `(None, None)` when no
/// global propagator is installed or no active span exists.
#[cfg(feature = "telemetry-otlp")]
fn capture_job_trace_context() -> (Option<String>, Option<String>) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let cx = tracing::Span::current().context();
    let mut map = std::collections::HashMap::<String, String>::new();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut JobMapInjector(&mut map));
    });
    (map.remove("traceparent"), map.remove("tracestate"))
}

/// Reconstruct an OpenTelemetry [`Context`](opentelemetry::Context) from
/// serialized W3C `traceparent` / `tracestate` strings captured at enqueue
/// time.  Returns `None` when the `traceparent` is absent or unparseable so
/// the caller can fall back to a fresh root span instead of propagating a
/// broken context.
#[cfg(feature = "telemetry-otlp")]
fn restore_job_trace_context(
    traceparent: Option<&str>,
    tracestate: Option<&str>,
) -> Option<opentelemetry::Context> {
    use opentelemetry::trace::TraceContextExt as _;

    let tp = traceparent?;
    let mut map = std::collections::HashMap::<String, String>::new();
    map.insert("traceparent".to_owned(), tp.to_owned());
    if let Some(ts) = tracestate {
        map.insert("tracestate".to_owned(), ts.to_owned());
    }
    let cx = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&JobMapExtractor(&map))
    });
    if cx.span().span_context().is_valid() {
        Some(cx)
    } else {
        None
    }
}

#[cfg(feature = "telemetry-otlp")]
struct JobMapExtractor<'a>(&'a std::collections::HashMap<String, String>);

#[cfg(feature = "telemetry-otlp")]
impl opentelemetry::propagation::Extractor for JobMapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

#[cfg(feature = "telemetry-otlp")]
struct JobMapInjector<'a>(&'a mut std::collections::HashMap<String, String>);

#[cfg(feature = "telemetry-otlp")]
impl opentelemetry::propagation::Injector for JobMapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
}

fn build_job_consumer_span(name: &str, attempt: u32) -> tracing::Span {
    tracing::info_span!("job.execute", "otel.kind" = "consumer", job.name = %name, job.attempt = attempt)
}

async fn run_job_handler(
    name: &str,
    handler: JobHandler,
    state: AppState,
    payload: Value,
    final_attempt: bool,
) -> JobExecutionOutcome {
    // Tracked jobs carry their args wrapped in an envelope keyed by a hash of
    // the polling token (never the raw token). Strip it here — the single
    // choke point all three backends run handlers through — so the handler
    // itself only ever sees the caller's original args, and make a
    // `JobContext` ambient for the duration of execution so `ctx.set_progress`
    // works from anywhere inside the handler.
    let (tracked_key, payload) = crate::job_tracking::take_tracked_payload(payload);
    let ctx = match &tracked_key {
        Some(key) => match crate::job_tracking::tracking_store_from_state(&state) {
            Some(store) => {
                let _ = store.mark_running(key).await;
                crate::job_tracking::JobContext::tracked(key.clone(), store)
            }
            None => crate::job_tracking::JobContext::none(),
        },
        None => crate::job_tracking::JobContext::none(),
    };

    // Make this job's app the ambient event context so a job (or durable event
    // listener) that calls the free `events::publish` dispatches against its own
    // app rather than the process-global bus.
    let event_app = state.clone();
    let interceptor = state
        .extension::<Arc<dyn crate::interceptor::JobInterceptor>>()
        .map(|arc| (*arc).clone());

    let payload_for_handler = payload.clone();
    // Defer the handler invocation into a lazy Pin<Box<dyn Future>>
    let next = Box::pin(async move {
        let future_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (handler)(state, payload_for_handler)
        }));

        let future = match future_res {
            Ok(f) => f,
            Err(panic) => {
                std::panic::resume_unwind(panic);
            }
        };

        future.await
    });

    let interceptor_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(interceptor) = &interceptor {
            interceptor.intercept_execute(name, &payload, next)
        } else {
            next
        }
    }));

    let outcome = match interceptor_res {
        Ok(future) => {
            let execution = std::panic::AssertUnwindSafe(future).catch_unwind();
            match crate::job_tracking::scope(
                ctx.clone(),
                crate::events::scope_event_app(event_app, execution),
            )
            .await
            {
                Ok(Ok(())) => JobExecutionOutcome::Succeeded,
                Ok(Err(error)) => JobExecutionOutcome::Failed(error.to_string()),
                Err(panic) => JobExecutionOutcome::Panicked(format_job_panic(panic.as_ref())),
            }
        }
        Err(panic) => JobExecutionOutcome::Panicked(format_job_panic(panic.as_ref())),
    };

    if tracked_key.is_some() {
        match &outcome {
            JobExecutionOutcome::Succeeded => ctx.settle_success().await,
            // Panics always dead-letter regardless of remaining attempts
            // (matching every backend's worker loop), so they are always
            // terminal for tracking purposes too.
            JobExecutionOutcome::Panicked(_) => {
                ctx.settle_failure(crate::job_tracking::GENERIC_FAILURE_MESSAGE)
                    .await;
            }
            JobExecutionOutcome::Failed(_) if final_attempt => {
                ctx.settle_failure(crate::job_tracking::GENERIC_FAILURE_MESSAGE)
                    .await;
            }
            JobExecutionOutcome::Failed(_) => {
                // A retry is pending; leave the record running so progress
                // persists across attempts.
            }
        }
    }

    outcome
}

fn format_job_panic(panic: &(dyn std::any::Any + Send)) -> String {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&'static str>().copied())
        .unwrap_or("non-string panic payload");
    format!("job handler panicked: {detail}")
}

fn format_enqueue_panic(panic: &(dyn std::any::Any + Send)) -> AutumnError {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&'static str>().copied())
        .unwrap_or("non-string panic payload");
    AutumnError::internal_server_error(std::io::Error::other(format!(
        "job enqueue panicked: {detail}"
    )))
}

async fn run_enqueue_interceptor(
    interceptor: Arc<dyn crate::interceptor::JobInterceptor>,
    name: &str,
    payload: &Value,
    actual_enqueue: std::pin::Pin<
        Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + '_>,
    >,
) -> AutumnResult<()> {
    let setup_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        interceptor.intercept_enqueue(name, payload, actual_enqueue)
    }));
    let fut = match setup_res {
        Ok(f) => f,
        Err(panic) => return Err(format_enqueue_panic(panic.as_ref())),
    };
    match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(res) => res,
        Err(panic) => Err(format_enqueue_panic(panic.as_ref())),
    }
}

/// Retrieves the global initialized job client.
///
/// Returns `None` if the job runtime hasn't been started yet.
#[must_use]
pub fn global_job_client() -> Option<Arc<JobClient>> {
    GLOBAL_JOB_CLIENT
        .get()
        .and_then(|lock| lock.read().ok().and_then(|guard| guard.clone()))
}

/// Install the runtime's [`JobClient`] both as the process-global client (used
/// by the free [`enqueue`] functions and `#[job]` handlers) **and** as an
/// [`AppState`] extension, so callers that hold an `AppState` — notably the
/// event bus's durable dispatch — can enqueue against *this* app's client
/// rather than racing on the process-global one.
pub(crate) fn install_job_client(state: &AppState, client: JobClient) {
    state.insert_extension(client.clone());
    init_global_job_client(client);
    // `enqueue_tracked` needs a tracking store the moment a job runtime is
    // live, even for backends/tests that build a `JobClient` directly rather
    // than going through `start_runtime` (which installs a config-driven
    // store before the backend starter runs and gets here).
    crate::job_tracking::ensure_tracking_store_installed(state);
}

pub(crate) fn init_global_job_client(client: JobClient) {
    if let Some(lock) = GLOBAL_JOB_CLIENT.get() {
        if let Ok(mut guard) = lock.write() {
            *guard = Some(Arc::new(client));
        }
        return;
    }
    let _ = GLOBAL_JOB_CLIENT.set(RwLock::new(Some(Arc::new(client))));
}

pub fn clear_global_job_client() {
    if let Some(lock) = GLOBAL_JOB_CLIENT.get() {
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    } else {
        let _ = GLOBAL_JOB_CLIENT.set(RwLock::new(None));
    }
    crate::job_tracking::clear_global_tracking_store();
}

/// Enqueue a job payload on the configured runtime backend.
///
/// # Errors
///
/// Returns an internal error when the jobs runtime is not initialized, when
/// `name` does not match a registered job, or when the active backend rejects
/// the enqueue operation.
pub async fn enqueue(name: &str, payload: Value) -> AutumnResult<()> {
    let Some(client) = global_job_client() else {
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        )));
    };
    client.enqueue(name, payload).await
}

/// Convert a relative delay into an absolute due instant.
///
/// Saturates to `DateTime::MAX` on overflow (practically impossible).
fn delay_to_when(delay: std::time::Duration) -> chrono::DateTime<chrono::Utc> {
    // chrono::TimeDelta::from_std returns Err on overflow (>i64::MAX nanoseconds).
    // Fall back to MAX_UTC rather than panicking.
    let Ok(delta) = chrono::TimeDelta::from_std(delay) else {
        return chrono::DateTime::<chrono::Utc>::MAX_UTC;
    };
    chrono::Utc::now()
        .checked_add_signed(delta)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
}

/// Enqueue a one-shot job to run once after `delay` elapses.
///
/// This is the deferred-execution companion to [`enqueue`]: the job is recorded
/// immediately but is not delivered to a worker until `delay` has passed, then
/// runs through the normal execution path (retries, backoff, dead-letter).
///
/// On the durable backends (`postgres`, `redis`) the due time is persisted, so a
/// pending delay survives a worker/process restart. The in-process (`local`)
/// backend is local-safe only: a pending delay is lost if the process restarts
/// before the job becomes due.
///
/// For recurring work use `#[scheduled]`; for durable multi-step orchestration
/// use Autumn Harvest. See `docs/guide/jobs.md`.
///
/// # Errors
///
/// Returns an internal error when the jobs runtime is not initialized, when
/// `name` does not match a registered job, or when the active backend rejects
/// the enqueue operation.
pub async fn enqueue_in(
    name: &str,
    payload: Value,
    delay: std::time::Duration,
) -> AutumnResult<()> {
    let when = delay_to_when(delay);
    enqueue_at(name, payload, when).await
}

/// Enqueue a one-shot job to run once at the absolute instant `when`.
///
/// Behaves like [`enqueue_in`] but takes an absolute due time. A `when` in the
/// past runs the job immediately. Calendar/timezone math is the caller's
/// concern — `when` is an absolute UTC instant.
///
/// # Errors
///
/// Returns an internal error when the jobs runtime is not initialized, when
/// `name` does not match a registered job, or when the active backend rejects
/// the enqueue operation.
pub async fn enqueue_at(
    name: &str,
    payload: Value,
    when: chrono::DateTime<chrono::Utc>,
) -> AutumnResult<()> {
    let Some(client) = global_job_client() else {
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        )));
    };
    client.enqueue_due(name, payload, Some(when)).await
}

/// Enqueue a job using an **already-open connection** so the INSERT
/// participates in the caller's transaction.
///
/// For the `postgres` backend this provides atomic enqueue: if the
/// surrounding `db.tx` rolls back, the job disappears with it. For
/// `redis` and `local` backends the `conn` argument is ignored and the
/// call falls back to the normal enqueue path.
///
/// # Errors
///
/// Returns an error if the job runtime is not initialized, if `args`
/// cannot be serialized to JSON, or if the database INSERT fails.
///
/// # Example
///
/// ```rust,ignore
/// db.tx(move |conn| async move {
///     diesel::insert_into(orders::table).values(&order).execute(conn).await?;
///     autumn_web::job::enqueue_on_conn("send_confirmation", &args, conn).await?;
///     Ok(())
/// }.scope_boxed()).await?;
/// ```
#[cfg(feature = "db")]
pub async fn enqueue_on_conn<A: serde::Serialize>(
    name: &str,
    args: A,
    conn: &mut diesel_async::AsyncPgConnection,
) -> AutumnResult<()> {
    let payload = serde_json::to_value(&args).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "job args serialization failed: {e}"
        )))
    })?;
    let Some(client) = global_job_client() else {
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        )));
    };
    client.enqueue_on_conn(name, payload, conn).await
}

/// Transactional delayed enqueue.
///
/// Like [`enqueue_on_conn`] but the job becomes runnable only after `delay`
/// elapses (and after the surrounding transaction commits). On the `postgres`
/// backend this is crash-safe — the future `run_at` is persisted in the same
/// transaction as the domain write.
///
/// # Errors
///
/// Returns an error if the job runtime is not initialized, if `args` cannot be
/// serialized to JSON, or if the database INSERT fails.
#[cfg(feature = "db")]
pub async fn enqueue_in_on_conn<A: serde::Serialize>(
    name: &str,
    args: A,
    delay: std::time::Duration,
    conn: &mut diesel_async::AsyncPgConnection,
) -> AutumnResult<()> {
    let when = delay_to_when(delay);
    enqueue_at_on_conn(name, args, when, conn).await
}

/// Transactional delayed enqueue at an absolute instant. See
/// [`enqueue_in_on_conn`].
///
/// # Errors
///
/// Returns an error if the job runtime is not initialized, if `args` cannot be
/// serialized to JSON, or if the database INSERT fails.
#[cfg(feature = "db")]
pub async fn enqueue_at_on_conn<A: serde::Serialize>(
    name: &str,
    args: A,
    when: chrono::DateTime<chrono::Utc>,
    conn: &mut diesel_async::AsyncPgConnection,
) -> AutumnResult<()> {
    let payload = serde_json::to_value(&args).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "job args serialization failed: {e}"
        )))
    })?;
    let Some(client) = global_job_client() else {
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        )));
    };
    client
        .enqueue_on_conn_due(name, payload, conn, Some(when))
        .await
}

/// Enqueue a job that fires **only after the surrounding transaction commits**.
///
/// This is the module-level companion to [`JobClient::enqueue_after_commit`].
/// It delegates to the globally initialized job client.
///
/// When called inside a [`Db::tx`](crate::db::Db::tx) block, the enqueue is
/// deferred until the transaction commits. On rollback the job is dropped.
/// This process-local deferral is not crash-safe: if the process exits after
/// the commit but before the callback runs, no job may be recorded.
///
/// When called outside any active transaction, the job is enqueued
/// immediately with a `debug`-level log noting the eager path.
///
/// For the `postgres` backend, prefer [`enqueue_in_tx`] when you have the
/// connection available: writing the job row inside the same transaction
/// gives exactly-once enqueue with no after-commit indirection.
///
/// # Errors
///
/// Returns an error if the job runtime is not initialized or if `args`
/// cannot be serialized to JSON.
pub async fn enqueue_after_commit<A: serde::Serialize>(name: &str, args: A) -> AutumnResult<()> {
    let payload = serde_json::to_value(&args).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "job args serialization failed: {e}"
        )))
    })?;
    let Some(client) = global_job_client() else {
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        )));
    };
    client.enqueue_after_commit(name, payload).await
}

/// Delayed variant of [`enqueue_after_commit`]: after the surrounding
/// transaction commits, the job is enqueued to become runnable `delay` later.
///
/// Like [`enqueue_after_commit`], the after-commit deferral is process-local and
/// not crash-safe. For a crash-safe transactional delay on the `postgres`
/// backend, prefer [`enqueue_in_on_conn`].
///
/// # Errors
///
/// Returns an error if the job runtime is not initialized or if `args` cannot
/// be serialized to JSON.
pub async fn enqueue_in_after_commit<A: serde::Serialize>(
    name: &str,
    args: A,
    delay: std::time::Duration,
) -> AutumnResult<()> {
    let payload = serde_json::to_value(&args).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "job args serialization failed: {e}"
        )))
    })?;
    let Some(client) = global_job_client() else {
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        )));
    };
    // `enqueue_after_commit_delay` computes `when` inside the callback so
    // the delay is measured from commit time, not from this call site.
    client
        .enqueue_after_commit_delay(name, payload, delay)
        .await
}

/// Absolute-instant variant of [`enqueue_in_after_commit`].
///
/// # Errors
///
/// Returns an error if the job runtime is not initialized or if `args` cannot
/// be serialized to JSON.
pub async fn enqueue_at_after_commit<A: serde::Serialize>(
    name: &str,
    args: A,
    when: chrono::DateTime<chrono::Utc>,
) -> AutumnResult<()> {
    let payload = serde_json::to_value(&args).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "job args serialization failed: {e}"
        )))
    })?;
    let Some(client) = global_job_client() else {
        return Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime is not initialized; register jobs with AppBuilder::jobs()",
        )));
    };
    client
        .enqueue_after_commit_due(name, payload, Some(when))
        .await
}

/// Enqueue a job inside an **already-open connection**, writing the job row
/// inside the caller's transaction for exactly-once semantics.
///
/// This is the optimal-path API for the `postgres` backend: the job row
/// is written inside the user's own DB transaction. If the transaction rolls
/// back, the job row disappears with it — no after-commit indirection needed.
///
/// For `redis` and `local` backends `conn` is ignored and the call falls back
/// to the normal enqueue path (same as [`enqueue_on_conn`]).
///
/// # Errors
///
/// Returns an error if the job runtime is not initialized, if `args`
/// cannot be serialized to JSON, or if the database INSERT fails.
///
/// # Example
///
/// ```rust,ignore
/// db.tx(move |conn| {
///     scoped_boxed(async move {
///         let user = diesel::insert_into(users::table).values(&new_user)
///             .get_result(conn).await?;
///         autumn_web::job::enqueue_in_tx("welcome_email", &WelcomeArgs { user_id: user.id }, conn).await?;
///         Ok(user)
///     })
/// }).await?;
/// ```
#[cfg(feature = "db")]
pub async fn enqueue_in_tx<A: serde::Serialize>(
    name: &str,
    args: A,
    conn: &mut diesel_async::AsyncPgConnection,
) -> AutumnResult<()> {
    enqueue_on_conn(name, args, conn).await
}

impl JobClient {
    /// Enqueue a job by name with a JSON payload.
    ///
    /// # Errors
    ///
    /// Returns an internal error when `name` does not match a registered job
    /// or enqueueing fails in the active backend.
    #[allow(clippy::too_many_lines)]
    pub async fn enqueue(&self, name: &str, payload: Value) -> AutumnResult<()> {
        self.enqueue_with_outcome(name, payload).await.map(|_| ())
    }

    /// Enqueue a job that becomes runnable at `due_at` (or immediately when
    /// `due_at` is `None` or in the past). Backs [`enqueue_in`] / [`enqueue_at`].
    ///
    /// # Errors
    ///
    /// Returns an internal error when `name` does not match a registered job or
    /// enqueueing fails in the active backend.
    pub async fn enqueue_due(
        &self,
        name: &str,
        payload: Value,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> AutumnResult<()> {
        self.enqueue_with_outcome_due(name, payload, due_at)
            .await
            .map(|_| ())
    }

    /// Enqueue like [`Self::enqueue`], reporting whether the job was stored
    /// or coalesced into an existing unique job. Used by operator paths that
    /// must distinguish "queued a retry" from "an equivalent job already
    /// exists".
    pub(crate) async fn enqueue_with_outcome(
        &self,
        name: &str,
        payload: Value,
    ) -> AutumnResult<EnqueueOutcome> {
        self.enqueue_with_outcome_due(name, payload, None).await
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn enqueue_with_outcome_due(
        &self,
        name: &str,
        payload: Value,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> AutumnResult<EnqueueOutcome> {
        // Capture the reference instant once so every downstream decision
        // (filter, admin record status, local-backend sleep) uses a consistent
        // clock reading and near-due jobs cannot be misclassified.
        let now = chrono::Utc::now();
        // Only treat a due time strictly in the future as "delayed"; a past or
        // absent due time enqueues for immediate execution exactly as before.
        let due_at = due_at.filter(|due| *due > now);
        let Some(settings) = self.per_job_settings.get(name) else {
            return Err(AutumnError::internal_server_error(std::io::Error::other(
                format!("job '{name}' is not registered; add it to AppBuilder::jobs()"),
            )));
        };
        let job_max_attempts = if settings.max_attempts != 0 {
            settings.max_attempts
        } else {
            self.default_max_attempts
        };
        let job_backoff_ms = if settings.initial_backoff_ms != 0 {
            settings.initial_backoff_ms
        } else {
            self.default_initial_backoff_ms
        };
        let job_queue = normalize_queue_name(&settings.queue);
        let constraints = ResolvedJobConstraints::for_payload(settings, &payload);
        let id = uuid::Uuid::new_v4().to_string();
        self.registry.record_enqueue(name);
        self.job_admin.record_enqueue_due(
            id.clone(),
            name,
            &job_queue,
            payload.clone(),
            1,
            job_max_attempts,
            due_at,
            now,
        );

        let started = ::std::sync::Arc::new(::std::sync::atomic::AtomicBool::new(false));
        let started_clone = started.clone();
        let deduplicated = ::std::sync::Arc::new(::std::sync::atomic::AtomicBool::new(false));
        let deduplicated_clone = deduplicated.clone();

        let id_for_enqueue = id.clone();
        let payload_clone = payload.clone();
        let actual_enqueue = async move {
            started_clone.store(true, ::std::sync::atomic::Ordering::SeqCst);
            let outcome = if let Some(sender) = &self.local_sender {
                if let (Some(unique_key), Some(window), Some(coordination)) = (
                    constraints.unique_key.as_deref(),
                    constraints.unique_window,
                    self.local_coordination.as_deref(),
                ) && !coordination.try_acquire_unique(name, unique_key, &id_for_enqueue, window)
                {
                    self.record_deduplicated_enqueue(name, &id_for_enqueue);
                    deduplicated_clone.store(true, ::std::sync::atomic::Ordering::SeqCst);
                    return Ok(());
                }
                #[cfg(feature = "telemetry-otlp")]
                let (traceparent, tracestate) = capture_job_trace_context();
                let queued = QueuedJob {
                    id: id_for_enqueue.clone(),
                    name: name.to_string(),
                    queue: job_queue.clone(),
                    payload: payload_clone.clone(),
                    attempt: 1,
                    max_attempts: job_max_attempts,
                    initial_backoff_ms: job_backoff_ms,
                    #[cfg(feature = "telemetry-otlp")]
                    traceparent,
                    #[cfg(feature = "telemetry-otlp")]
                    tracestate,
                };
                let send_result = if let Some(due) = due_at {
                    // Delayed enqueue on the in-process backend: hand the job to
                    // a detached timer that sleeps until the due time and then
                    // delivers it to a worker. This is local-safe only — a
                    // pending delay is lost if the process restarts before the
                    // job becomes due (durable backends persist the due time).
                    // Recompute remaining delay at the moment actual_enqueue
                    // runs (after any interceptor) so the sleep duration stays
                    // accurate even if the interceptor took non-trivial time.
                    let delay = (due - chrono::Utc::now())
                        .to_std()
                        .unwrap_or(std::time::Duration::ZERO);
                    let sender = sender.clone();
                    let cancel_token = tokio_util::sync::CancellationToken::new();
                    let already_canceled = self
                        .job_admin
                        .register_delay_canceler(id_for_enqueue.clone(), cancel_token.clone());
                    if already_canceled {
                        // The admin canceled this job during an interceptor's
                        // async work, before the token was registered.  The
                        // admin record is already Canceled; just release the
                        // unique lock and decrement the queued gauge.
                        if let (Some(unique_key), Some(coord)) =
                            (&constraints.unique_key, self.local_coordination.as_deref())
                        {
                            coord.release_unique(name, unique_key, &id_for_enqueue);
                        }
                        self.registry.record_cancel(name);
                        return Ok(());
                    }
                    // Capture what we need on cancel: unique lock release,
                    // admin status update, and queued-gauge decrement.
                    let cancel_unique_key = constraints.unique_key.clone();
                    let cancel_coordination = self.local_coordination.clone();
                    let cancel_admin = self.job_admin.clone();
                    let cancel_registry = self.registry.clone();
                    let cancel_name = name.to_string();
                    let cancel_id = id_for_enqueue.clone();
                    tokio::spawn(async move {
                        tokio::select! {
                            biased;
                            () = cancel_token.cancelled() => {
                                // Admin-canceled before the due time: release the
                                // unique lock immediately so re-enqueueing works
                                // without waiting for the original timer to fire.
                                // Also update the admin record and queued gauge so
                                // /actuator does not report phantom queued work.
                                if let (Some(unique_key), Some(coord)) =
                                    (cancel_unique_key, cancel_coordination)
                                {
                                    coord.release_unique(&cancel_name, &unique_key, &cancel_id);
                                }
                                cancel_registry.record_cancel(&cancel_name);
                                cancel_admin.record_cancelled(&cancel_id);
                            }
                            () = tokio::time::sleep(delay) => {
                                let _ = sender.send(queued).await;
                            }
                        }
                    });
                    Ok(())
                } else {
                    sender.send(queued).await.map_err(|e| {
                        AutumnError::internal_server_error(std::io::Error::other(format!(
                            "failed to enqueue job: {e}"
                        )))
                    })
                };
                if send_result.is_err()
                    && let (Some(unique_key), Some(coordination)) = (
                        constraints.unique_key.as_deref(),
                        self.local_coordination.as_deref(),
                    )
                {
                    coordination.release_unique(name, unique_key, &id_for_enqueue);
                }
                send_result.map(|()| EnqueueOutcome::Queued)
            } else {
                self.enqueue_durable(
                    id_for_enqueue.clone(),
                    name,
                    &job_queue,
                    payload_clone.clone(),
                    job_max_attempts,
                    job_backoff_ms,
                    due_at,
                    &constraints,
                )
                .await
            };
            let result = match outcome {
                Ok(EnqueueOutcome::Queued) => Ok(()),
                Ok(EnqueueOutcome::Deduplicated) => {
                    self.record_deduplicated_enqueue(name, &id_for_enqueue);
                    deduplicated_clone.store(true, ::std::sync::atomic::Ordering::SeqCst);
                    return Ok(());
                }
                Err(error) => Err(error),
            };
            if result.is_err() {
                self.registry.record_cancel(name);
                self.job_admin.record_cancelled(&id_for_enqueue);
            }
            result
        };

        let res = if let Some(interceptor) = &self.interceptor {
            let interceptor = (*interceptor).clone();
            run_enqueue_interceptor(interceptor, name, &payload, Box::pin(actual_enqueue)).await
        } else {
            actual_enqueue.await
        };

        if !started.load(::std::sync::atomic::Ordering::SeqCst) {
            self.registry.record_cancel(name);
            self.job_admin.record_cancelled(&id);
        }
        res.map(|()| {
            if deduplicated.load(::std::sync::atomic::Ordering::SeqCst) {
                EnqueueOutcome::Deduplicated
            } else {
                EnqueueOutcome::Queued
            }
        })
    }

    /// Enqueue a job that fires **only after the surrounding transaction commits**.
    ///
    /// When called inside a [`Db::tx`](crate::db::Db::tx) block, the enqueue is
    /// deferred until the transaction commits successfully. If the transaction
    /// rolls back, the job is never enqueued.
    ///
    /// The deferred enqueue callback runs in-process after commit. Use
    /// [`enqueue_in_tx`] / `enqueue_on_conn` with the
    /// Postgres backend when the job row itself must be committed atomically
    /// with the domain write.
    ///
    /// When called **outside** any active transaction, the job is enqueued
    /// immediately (equivalent to [`enqueue`](Self::enqueue)) and a `debug`-level
    /// log entry is emitted to make the no-op deferral visible.
    ///
    /// # Errors
    ///
    /// Returns an error if `payload` cannot be serialized to JSON, or if the
    /// underlying enqueue fails (backend error, unregistered job name, etc.).
    ///
    /// # Panics
    ///
    /// Panics if the internal after-commit registry mutex is poisoned (only
    /// possible if a previous thread holding the lock panicked, which should
    /// not occur in normal operation).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// db.tx(move |conn| scoped_boxed(async move {
    ///     let user = repo.create(new_user, conn).await?;
    ///     job_client
    ///         .enqueue_after_commit("welcome_email", WelcomeArgs { user_id: user.id })
    ///         .await?;
    ///     Ok(user)
    /// })).await?;
    /// ```
    pub async fn enqueue_after_commit(
        &self,
        name: &str,
        payload: impl serde::Serialize,
    ) -> AutumnResult<()> {
        self.enqueue_after_commit_due(name, payload, None).await
    }

    /// Delayed variant of [`Self::enqueue_after_commit`]: after the transaction
    /// commits, the job is enqueued to become runnable at `due_at`. When
    /// `due_at` is `None` or in the past this is exactly `enqueue_after_commit`.
    ///
    /// # Errors
    ///
    /// Returns an error if `payload` cannot be serialized to JSON, or if the
    /// underlying enqueue fails (backend error, unregistered job name, etc.).
    ///
    /// # Panics
    ///
    /// Panics if the internal after-commit registry mutex is poisoned.
    pub async fn enqueue_after_commit_due(
        &self,
        name: &str,
        payload: impl serde::Serialize,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> AutumnResult<()> {
        self.enqueue_after_commit_inner(name, payload, AfterCommitDue::At(due_at))
            .await
    }

    /// Like [`Self::enqueue_after_commit_due`] but accepts a relative delay that
    /// is resolved to an absolute instant **at commit time**, not at call time.
    ///
    /// This preserves "delay from commit" semantics: even if the surrounding
    /// transaction takes longer than `delay`, the due time is always measured
    /// from when the transaction actually commits.
    pub(crate) async fn enqueue_after_commit_delay(
        &self,
        name: &str,
        payload: impl serde::Serialize,
        delay: std::time::Duration,
    ) -> AutumnResult<()> {
        self.enqueue_after_commit_inner(name, payload, AfterCommitDue::After(delay))
            .await
    }

    async fn enqueue_after_commit_inner(
        &self,
        name: &str,
        payload: impl serde::Serialize,
        due: AfterCommitDue,
    ) -> AutumnResult<()> {
        // Validate name eagerly so a typo/unregistered job fails the
        // transaction (before any DB commit) rather than being silently
        // dropped later when the deferred callback runs.
        if !self.per_job_settings.contains_key(name) {
            return Err(AutumnError::internal_server_error(std::io::Error::other(
                format!("job '{name}' is not registered; add it to AppBuilder::jobs()"),
            )));
        }

        let name = name.to_string();
        let payload = serde_json::to_value(payload).map_err(|e| {
            AutumnError::internal_server_error(std::io::Error::other(format!(
                "enqueue_after_commit: failed to serialize payload for job '{name}': {e}"
            )))
        })?;
        let client = self.clone();
        // Keep a copy for the debug log in the eager path (name is moved into f_opt).
        let name_for_log = name.clone();

        // Capture the caller's span now so that capture_job_trace_context() inside
        // client.enqueue() sees the originating request span even when the callback
        // runs in the after-commit task, which has no request span of its own.
        let enqueue_span = tracing::Span::current();

        let mut f_opt = Some(move || {
            let client = client.clone();
            let name = name.clone();
            let payload = payload.clone();
            // Resolve the due instant here, inside the callback, so that an
            // AfterCommitDue::After delay is measured from commit time.
            let due_at = match due {
                AfterCommitDue::At(at) => at,
                AfterCommitDue::After(d) => Some(delay_to_when(d)),
            };
            async move { client.enqueue_due(&name, payload, due_at).await }
        });

        #[cfg(feature = "db")]
        crate::db::AFTER_COMMIT_REGISTRY
            .try_with(|registry| {
                let f = f_opt.take().expect("closure only entered once");
                let span = enqueue_span.clone();
                let boxed: crate::db::CommitCallback =
                    Box::new(move || Box::pin(tracing::Instrument::instrument(f(), span)));
                registry.lock().expect("registry lock").push(boxed);
            })
            .ok();

        if let Some(f) = f_opt {
            // Not inside a db.tx (or db feature is off) — enqueue immediately.
            tracing::debug!(
                "enqueue_after_commit: no active transaction; enqueueing '{name_for_log}' immediately"
            );
            f().await?;
        }

        Ok(())
    }

    /// Mark a coalesced enqueue in the registry counters and admin record.
    fn record_deduplicated_enqueue(&self, name: &str, id: &str) {
        tracing::debug!(job = %name, job_id = %id, "job enqueue coalesced into existing unique job");
        self.registry.record_deduplicated(name);
        self.job_admin.record_deduplicated(id);
    }

    #[allow(clippy::too_many_arguments)]
    async fn enqueue_durable(
        &self,
        id: String,
        name: &str,
        queue: &str,
        payload: Value,
        max_attempts: u32,
        backoff_ms: u64,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
        constraints: &ResolvedJobConstraints,
    ) -> AutumnResult<EnqueueOutcome> {
        let breaker = self.resilience_config.as_ref().map_or_else(
            || {
                crate::circuit_breaker::global_registry().get_or_create(
                    "job_queue",
                    crate::circuit_breaker::CircuitBreakerPolicy::default(),
                )
            },
            |rc| {
                let policy =
                    crate::circuit_breaker::CircuitBreakerPolicy::from_config(rc, "job_queue");
                crate::circuit_breaker::global_registry()
                    .get_or_create_with_config("job_queue", policy)
            },
        );

        if breaker.before_call().is_err() {
            return Err(AutumnError::service_unavailable(std::io::Error::other(
                "job queue circuit breaker is open",
            )));
        }
        let guard = crate::circuit_breaker::CircuitBreakerGuard::new(breaker.clone());

        let res = self
            .enqueue_durable_inner(
                id,
                name,
                queue,
                payload,
                max_attempts,
                backoff_ms,
                due_at,
                constraints,
            )
            .await;
        if res.is_ok() {
            guard.success();
        } else {
            guard.failure();
        }
        res
    }

    #[allow(clippy::too_many_arguments)]
    async fn enqueue_durable_inner(
        &self,
        id: String,
        name: &str,
        queue: &str,
        payload: Value,
        max_attempts: u32,
        backoff_ms: u64,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
        constraints: &ResolvedJobConstraints,
    ) -> AutumnResult<EnqueueOutcome> {
        #[cfg(feature = "redis")]
        if let Some(redis) = &self.redis {
            let due_at_ms = due_at.map(|due| u64::try_from(due.timestamp_millis()).unwrap_or(0));
            return redis
                .enqueue(
                    id,
                    name,
                    queue,
                    payload,
                    max_attempts,
                    backoff_ms,
                    due_at_ms,
                    constraints,
                )
                .await;
        }
        #[cfg(feature = "db")]
        if let Some(pool) = &self.pg_pool {
            return pg_enqueue_job_at(
                pool,
                id,
                name,
                queue,
                payload,
                max_attempts,
                backoff_ms,
                due_at,
                constraints,
            )
            .await;
        }
        let _ = (
            id,
            name,
            queue,
            payload,
            max_attempts,
            backoff_ms,
            due_at,
            constraints,
        );
        Err(AutumnError::internal_server_error(std::io::Error::other(
            "job runtime backend is unavailable",
        )))
    }

    /// Enqueue a job using an **already-open connection**, so the INSERT
    /// participates in the caller's transaction.
    ///
    /// For the `postgres` backend this provides exactly-once-per-commit
    /// enqueue semantics: if the surrounding `db.tx` rolls back, the job row
    /// disappears atomically. For `redis` and `local` backends the `conn`
    /// argument is ignored and the call falls back to the normal enqueue path.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a registered job, or if the
    /// database INSERT fails.
    #[cfg(feature = "db")]
    pub async fn enqueue_on_conn(
        &self,
        name: &str,
        payload: Value,
        conn: &mut diesel_async::AsyncPgConnection,
    ) -> AutumnResult<()> {
        self.enqueue_on_conn_due(name, payload, conn, None).await
    }

    /// Transactional enqueue (see [`Self::enqueue_on_conn`]) with an explicit
    /// `due_at`. On the Postgres backend the job row is written inside the
    /// caller's transaction with a future `run_at`, so it is delivered to a
    /// worker only after **both** the transaction commits **and** the due time
    /// passes — crash-safe delayed enqueue.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a registered job, or if the database
    /// INSERT fails.
    #[cfg(feature = "db")]
    pub async fn enqueue_on_conn_due(
        &self,
        name: &str,
        payload: Value,
        conn: &mut diesel_async::AsyncPgConnection,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> AutumnResult<()> {
        let due_at = due_at.filter(|due| *due > chrono::Utc::now());
        let Some(settings) = self.per_job_settings.get(name) else {
            return Err(AutumnError::internal_server_error(std::io::Error::other(
                format!("job '{name}' is not registered; add it to AppBuilder::jobs()"),
            )));
        };
        let job_max_attempts = if settings.max_attempts != 0 {
            settings.max_attempts
        } else {
            self.default_max_attempts
        };
        let job_backoff_ms = if settings.initial_backoff_ms != 0 {
            settings.initial_backoff_ms
        } else {
            self.default_initial_backoff_ms
        };
        let job_queue = normalize_queue_name(&settings.queue);
        let constraints = ResolvedJobConstraints::for_payload(settings, &payload);
        let id = uuid::Uuid::new_v4().to_string();

        // Postgres transactional path: the caller controls when the surrounding
        // transaction commits, so we cannot safely update process-local counters
        // here — the row may disappear on rollback while the counter persists.
        if self.pg_pool.is_some() {
            let breaker = self.resilience_config.as_ref().map_or_else(
                || {
                    crate::circuit_breaker::global_registry().get_or_create(
                        "job_queue",
                        crate::circuit_breaker::CircuitBreakerPolicy::default(),
                    )
                },
                |rc| {
                    let policy =
                        crate::circuit_breaker::CircuitBreakerPolicy::from_config(rc, "job_queue");
                    crate::circuit_breaker::global_registry()
                        .get_or_create_with_config("job_queue", policy)
                },
            );

            if breaker.before_call().is_err() {
                return Err(AutumnError::service_unavailable(std::io::Error::other(
                    "job queue circuit breaker is open",
                )));
            }
            let guard = crate::circuit_breaker::CircuitBreakerGuard::new(breaker.clone());

            let id_for_enqueue = id.clone();
            let payload_for_enqueue = payload.clone();
            let constraints_ref = &constraints;
            let actual_enqueue = async move {
                let outcome = pg_enqueue_on_conn_at(
                    conn,
                    id_for_enqueue.clone(),
                    name,
                    &job_queue,
                    payload_for_enqueue,
                    job_max_attempts,
                    job_backoff_ms,
                    due_at,
                    constraints_ref,
                )
                .await;

                match &outcome {
                    Ok(EnqueueOutcome::Deduplicated) => {
                        guard.success();
                        // A dedup decision is final even if the surrounding
                        // transaction rolls back (no row was ever written), so the
                        // counter can be recorded immediately. Balance the queued
                        // gauge that record_deduplicated decrements.
                        self.registry.record_enqueue(name);
                        self.record_deduplicated_enqueue(name, &id_for_enqueue);
                    }
                    Ok(_) => {
                        guard.success();
                    }
                    Err(_) => {
                        guard.failure();
                    }
                }

                outcome.map(|_| ())
            };
            return if let Some(interceptor) = &self.interceptor {
                let interceptor = (*interceptor).clone();
                run_enqueue_interceptor(interceptor, name, &payload, Box::pin(actual_enqueue)).await
            } else {
                actual_enqueue.await
            };
        }

        // For the redis and local backends `conn` is irrelevant; the normal
        // enqueue path already applies interceptors, bookkeeping, uniqueness,
        // and concurrency metadata.
        self.enqueue_due(name, payload, due_at).await
    }
}

/// Starts the background job execution runtime.
///
/// This initializes the configured job worker backend (local, redis, or postgres)
/// and launches background worker tasks that run until the shutdown cancellation token is triggered.
///
/// # Errors
///
/// Returns an error if:
/// - There are duplicate job names registered in the workspace
/// - Redis or Postgres connection/initialization fails (if those backends are selected)
pub fn start_runtime(
    jobs: Vec<JobInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    config: &crate::config::JobConfig,
) -> AutumnResult<()> {
    validate_unique_job_names(&jobs).map_err(|error| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "invalid jobs configuration: {error}"
        )))
    })?;

    match config.backend.as_str() {
        "local" => {
            start_local_runtime(
                jobs,
                state,
                shutdown,
                config.workers,
                config.max_attempts,
                config.initial_backoff_ms,
                &config.queues,
            );
            Ok(())
        }
        "postgres" => {
            #[cfg(feature = "db")]
            {
                start_postgres_runtime(jobs, state, shutdown, config)
            }
            #[cfg(not(feature = "db"))]
            {
                let _ = (jobs, state, shutdown, config);
                Err(AutumnError::internal_server_error(std::io::Error::other(
                    "jobs.backend=postgres requested but db feature is disabled",
                )))
            }
        }
        "redis" => {
            #[cfg(feature = "redis")]
            {
                start_redis_runtime(jobs, state, shutdown, config)
            }
            #[cfg(not(feature = "redis"))]
            {
                let _ = jobs;
                let _ = state;
                let _ = shutdown;
                let _ = config;
                Err(AutumnError::internal_server_error(std::io::Error::other(
                    "jobs.backend=redis requested but redis feature is disabled",
                )))
            }
        }
        other => {
            tracing::warn!(backend = %other, "unknown jobs backend; falling back to local backend");
            start_local_runtime(
                jobs,
                state,
                shutdown,
                config.workers,
                config.max_attempts,
                config.initial_backoff_ms,
                &config.queues,
            );
            Ok(())
        }
    }
}

/// Process-local uniqueness holds and concurrency slots for the local backend.
///
/// The local backend is in-process and non-durable, so a plain mutex-guarded
/// map is sufficient: a crashed process loses the queue itself along with any
/// held keys, which means a dead worker can never deadlock a key beyond the
/// process lifetime.
#[derive(Default)]
pub(crate) struct LocalJobCoordination {
    inner: std::sync::Mutex<LocalJobCoordinationInner>,
}

#[derive(Default)]
struct LocalJobCoordinationInner {
    unique_holds: HashMap<String, LocalUniqueHold>,
    running_slots: HashMap<String, u32>,
    waiting: HashMap<String, VecDeque<QueuedJob>>,
}

struct LocalUniqueHold {
    job_id: String,
    expires_at: Option<std::time::Instant>,
}

fn local_unique_hold_key(name: &str, unique_key: &str) -> String {
    format!("{name}\u{1f}{unique_key}")
}

fn local_concurrency_group(name: &str, scope: Option<&str>) -> String {
    scope.map_or_else(|| name.to_string(), |scope| format!("{name}\u{1f}{scope}"))
}

enum LocalSlotDecision {
    Acquired(QueuedJob),
    Parked,
}

impl LocalJobCoordination {
    /// Try to hold the unique key for `job_id`; `false` means an equivalent
    /// job already holds it (the enqueue should coalesce).
    fn try_acquire_unique(
        &self,
        name: &str,
        unique_key: &str,
        job_id: &str,
        window: JobUniquenessWindow,
    ) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return true;
        };
        let key = local_unique_hold_key(name, unique_key);
        let now = std::time::Instant::now();
        if let Some(hold) = inner.unique_holds.get(&key) {
            let expired = hold.expires_at.is_some_and(|expires_at| expires_at <= now);
            if !expired {
                return false;
            }
        }
        let expires_at = match window {
            JobUniquenessWindow::TtlMs(ms) => Some(now + std::time::Duration::from_millis(ms)),
            JobUniquenessWindow::Pending | JobUniquenessWindow::Running => None,
        };
        inner.unique_holds.insert(
            key,
            LocalUniqueHold {
                job_id: job_id.to_owned(),
                expires_at,
            },
        );
        true
    }

    /// Release the unique key if `job_id` is still the holder.
    ///
    /// TTL-window holds are intentionally never released here; they expire by
    /// time so a burst keeps coalescing even after the job completed.
    fn release_unique(&self, name: &str, unique_key: &str, job_id: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let key = local_unique_hold_key(name, unique_key);
        let holder_matches = inner
            .unique_holds
            .get(&key)
            .is_some_and(|hold| hold.job_id == job_id && hold.expires_at.is_none());
        if holder_matches {
            inner.unique_holds.remove(&key);
        }
    }

    /// Acquire a concurrency slot for `group`, or park the job until one
    /// frees up. Parked jobs are resumed by [`Self::release_slot`].
    fn acquire_slot_or_park(&self, group: &str, limit: u32, job: QueuedJob) -> LocalSlotDecision {
        let Ok(mut inner) = self.inner.lock() else {
            return LocalSlotDecision::Acquired(job);
        };
        let running = inner.running_slots.get(group).copied().unwrap_or(0);
        if running >= limit {
            inner
                .waiting
                .entry(group.to_string())
                .or_default()
                .push_back(job);
            return LocalSlotDecision::Parked;
        }
        *inner.running_slots.entry(group.to_string()).or_insert(0) += 1;
        LocalSlotDecision::Acquired(job)
    }

    /// Free one slot for `group` and hand back the next parked job, if any.
    fn release_slot(&self, group: &str) -> Option<QueuedJob> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        if let Some(count) = inner.running_slots.get_mut(group) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                inner.running_slots.remove(group);
            }
        }
        let next = inner
            .waiting
            .get_mut(group)
            .and_then(std::collections::VecDeque::pop_front);
        if inner.waiting.get(group).is_some_and(VecDeque::is_empty) {
            inner.waiting.remove(group);
        }
        next
    }
}

pub(crate) fn start_local_runtime(
    jobs: Vec<JobInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    workers: usize,
    default_max_attempts: u32,
    default_initial_backoff_ms: u64,
    queues_config: &crate::config::JobQueuesConfig,
) {
    let job_admin = default_job_admin_backend_for_state(state);
    let per_job_settings = build_per_job_settings(&jobs);
    let jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>> = Arc::new(RwLock::new(
        jobs.into_iter().map(|j| (j.name.clone(), j)).collect(),
    ));

    {
        let guard = jobs_by_name.read().expect("job registry lock poisoned");
        for name in guard.keys() {
            state.job_registry.register(name);
        }
    }

    let worker_count = workers.max(1);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<QueuedJob>(1024);
    let coordination = Arc::new(LocalJobCoordination::default());

    // Build the priority drain schedule from `[jobs] queues`, appending any
    // queue declared on a job but missing from config at lowest priority so it
    // still drains. Warn loudly about those so the operator can fix the config.
    let declared_queues = collect_declared_queues(&jobs_by_name);
    let (schedule, unconfigured) = QueueSchedule::effective(queues_config, &declared_queues);
    for queue in &unconfigured {
        tracing::warn!(
            queue = %queue,
            "job declares queue '{queue}' which is not in [jobs] queues; draining it at \
             lowest priority. Add it to the configured queue list to control its priority.",
        );
    }
    let buffer = Arc::new(LocalQueueBuffer::new());

    let client = JobClient {
        local_sender: Some(tx.clone()),
        local_coordination: Some(Arc::clone(&coordination)),
        #[cfg(feature = "redis")]
        redis: None,
        #[cfg(feature = "db")]
        pg_pool: None,
        registry: state.job_registry.clone(),
        job_admin: job_admin.clone(),
        default_max_attempts,
        default_initial_backoff_ms,
        per_job_settings,
        interceptor: state
            .extension::<Arc<dyn crate::interceptor::JobInterceptor>>()
            .map(|arc| (*arc).clone()),
        resilience_config: state
            .extension::<crate::config::AutumnConfig>()
            .map(|c| Arc::new(c.resilience.clone())),
    };
    install_job_client(state, client);

    // Ingress task: own the channel receiver and route each job into its named
    // queue in the shared priority buffer. Retries and concurrency-unparked jobs
    // re-enter here too, preserving their queue.
    {
        let buffer = Arc::clone(&buffer);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    maybe = rx.recv() => {
                        match maybe {
                            Some(job) => buffer.push(job),
                            None => break,
                        }
                    }
                }
            }
        });
    }

    for _ in 0..worker_count {
        let state = state.clone();
        let tx = tx.clone();
        let job_admin = job_admin.clone();
        let jobs_by_name = Arc::clone(&jobs_by_name);
        let buffer = Arc::clone(&buffer);
        let shutdown = shutdown.clone();
        let coordination = Arc::clone(&coordination);
        let mut cursor = schedule.cursor();

        tokio::spawn(async move {
            loop {
                // Register interest before checking so an enqueue that lands
                // between the pop attempt and the await is never lost.
                let notified = buffer.notify.notified();
                if let Some(job) = buffer.try_pop(&cursor.next_order()) {
                    execute_local_job(job, &jobs_by_name, &tx, &state, &job_admin, &coordination)
                        .await;
                    continue;
                }
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = notified => {}
                }
            }
        });
    }
}

/// Distinct queue names declared by the registered jobs.
fn collect_declared_queues(jobs_by_name: &Arc<RwLock<HashMap<String, JobInfo>>>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut queues = Vec::new();
    {
        let guard = jobs_by_name.read().expect("job registry lock poisoned");
        for job in guard.values() {
            let name = normalize_queue_name(&job.queue);
            if seen.insert(name.clone()) {
                queues.push(name);
            }
        }
    }
    queues
}

const LOCAL_QUEUE_WARN_THRESHOLD: usize = 10_000;

/// Shared, priority-ordered job buffer for the in-process backend.
///
/// Jobs are bucketed per named queue; workers pop the highest-priority
/// non-empty queue according to their [`QueueCursor`]. A fallback sweep ensures
/// a job whose queue is somehow outside the drain order is never stranded.
struct LocalQueueBuffer {
    inner: std::sync::Mutex<HashMap<String, VecDeque<QueuedJob>>>,
    notify: tokio::sync::Notify,
}

impl LocalQueueBuffer {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    fn push(&self, job: QueuedJob) {
        {
            let mut map = self.inner.lock().expect("local job buffer lock poisoned");
            let bucket = map.entry(normalize_queue_name(&job.queue)).or_default();
            if bucket.len() == LOCAL_QUEUE_WARN_THRESHOLD {
                tracing::warn!(
                    queue = %job.queue,
                    threshold = LOCAL_QUEUE_WARN_THRESHOLD,
                    "local job queue has grown past the warning threshold; \
                     memory use is unbounded — consider reducing enqueue rate or \
                     switching to the Redis or Postgres backend"
                );
            }
            bucket.push_back(job);
        }
        self.notify.notify_one();
    }

    #[allow(clippy::significant_drop_tightening)]
    fn try_pop(&self, order: &[String]) -> Option<QueuedJob> {
        let mut map = self.inner.lock().expect("local job buffer lock poisoned");
        for name in order {
            if let Some(job) = map.get_mut(name).and_then(VecDeque::pop_front) {
                return Some(job);
            }
        }
        // Never strand work: drain any queue outside the configured order.
        for queue in map.values_mut() {
            if let Some(job) = queue.pop_front() {
                return Some(job);
            }
        }
        None
    }
}

#[allow(clippy::too_many_lines)]
async fn execute_local_job(
    job: QueuedJob,
    jobs_by_name: &Arc<RwLock<HashMap<String, JobInfo>>>,
    tx: &tokio::sync::mpsc::Sender<QueuedJob>,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
    coordination: &Arc<LocalJobCoordination>,
) {
    let maybe_info = jobs_by_name
        .read()
        .expect("job registry lock poisoned")
        .get(&job.name)
        .map(|info| {
            (
                info.handler,
                info.max_attempts,
                info.initial_backoff_ms,
                info.uniqueness.clone(),
                info.concurrency.clone(),
            )
        });
    let Some((handler, info_max_attempts, info_backoff_ms, uniqueness, concurrency)) = maybe_info
    else {
        if job_admin.try_record_start(&job.id, job.attempt) == JobAdminStartDecision::Canceled {
            state.job_registry.record_cancel(&job.name);
            job_admin.record_cancelled(&job.id);
            return;
        }
        state.job_registry.record_start(&job.name);
        state
            .job_registry
            .record_failure(&job.name, format!("unknown job '{}'", job.name), true);
        job_admin.record_failure(&job.id, format!("unknown job '{}'", job.name));
        return;
    };

    // Concurrency gate: park the job when its group is saturated. Parked jobs
    // keep their enqueued status and resume when release_slot pops them.
    let job_name = job.name.clone();
    let concurrency_group = concurrency.as_ref().map(|conc| {
        let scope = job_concurrency_scope(conc, &job.payload);
        (
            local_concurrency_group(&job.name, scope.as_deref()),
            conc.limit,
        )
    });
    let job = if let Some((group, limit)) = &concurrency_group {
        match coordination.acquire_slot_or_park(group, *limit, job) {
            LocalSlotDecision::Acquired(job) => job,
            LocalSlotDecision::Parked => {
                state.job_registry.record_concurrency_blocked(&job_name);
                return;
            }
        }
    } else {
        job
    };

    if job_admin.try_record_start(&job.id, job.attempt) == JobAdminStartDecision::Canceled {
        state.job_registry.record_cancel(&job.name);
        job_admin.record_cancelled(&job.id);
        release_local_unique_hold(
            coordination,
            uniqueness.as_ref(),
            &job.name,
            &job.payload,
            &job.id,
        );
        finish_local_slot(coordination, concurrency_group.as_ref(), tx, state);
        return;
    }
    state.job_registry.record_start(&job.name);

    // A pending-window unique key is held only until execution starts.
    if let Some(unique) = &uniqueness
        && unique.window == JobUniquenessWindow::Pending
    {
        let key = job_unique_key(unique, &job.payload);
        coordination.release_unique(&job.name, &key, &job.id);
    }

    let max_attempts = if job.max_attempts != 0 {
        job.max_attempts
    } else if info_max_attempts != 0 {
        info_max_attempts
    } else {
        5
    };
    let backoff_ms = if job.initial_backoff_ms != 0 {
        job.initial_backoff_ms
    } else if info_backoff_ms != 0 {
        info_backoff_ms
    } else {
        250
    };

    let job_span = build_job_consumer_span(&job.name, job.attempt);
    #[cfg(feature = "telemetry-otlp")]
    if let Some(cx) =
        restore_job_trace_context(job.traceparent.as_deref(), job.tracestate.as_deref())
    {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let _ = job_span.set_parent(cx);
    }
    let final_attempt = job.attempt >= max_attempts;
    let f = run_job_handler(
        &job.name,
        handler,
        state.clone(),
        job.payload.clone(),
        final_attempt,
    );
    let outcome = tracing::Instrument::instrument(f, job_span).await;
    match outcome {
        JobExecutionOutcome::Succeeded => {
            state.job_registry.record_success(&job.name);
            job_admin.record_success(&job.id);
            release_local_unique_hold(
                coordination,
                uniqueness.as_ref(),
                &job.name,
                &job.payload,
                &job.id,
            );
        }
        JobExecutionOutcome::Failed(error) => {
            if job.attempt < max_attempts {
                // Running-window keys stay held across retries (the job is
                // still in flight until it settles). A pending-window key was
                // released when execution started, so re-acquire it now to
                // keep duplicates coalescing while the retry waits out its
                // backoff as a pending job again. If a duplicate was accepted
                // while this job ran it now owns the key; in that case drop
                // the retry (coalesce into the duplicate) rather than letting
                // both run unprotected.
                if let Some(unique) = &uniqueness
                    && unique.window == JobUniquenessWindow::Pending
                {
                    let key = job_unique_key(unique, &job.payload);
                    if !coordination.try_acquire_unique(&job.name, &key, &job.id, unique.window) {
                        state.job_registry.record_deduplicated(&job.name);
                        job_admin.record_deduplicated(&job.id);
                        finish_local_slot(coordination, concurrency_group.as_ref(), tx, state);
                        return;
                    }
                }
                state
                    .job_registry
                    .record_retry(&job.name, &error, job.attempt);
                job_admin.record_retrying(&job.id, &error);
                let sender = tx.clone();
                let registry = state.job_registry.clone();
                let job_admin = job_admin.clone();
                let id = job.id.clone();
                let name = job.name.clone();
                let queue = job.queue.clone();
                let payload = job.payload;
                #[cfg(feature = "telemetry-otlp")]
                let traceparent = job.traceparent;
                #[cfg(feature = "telemetry-otlp")]
                let tracestate = job.tracestate;
                let delay = backoff_ms.saturating_mul(2_u64.saturating_pow(job.attempt - 1));
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    registry.record_enqueue(&name);
                    job_admin.record_requeued(&id, job.attempt + 1);
                    let _ = sender
                        .send(QueuedJob {
                            id,
                            name,
                            queue,
                            payload,
                            attempt: job.attempt + 1,
                            max_attempts,
                            initial_backoff_ms: backoff_ms,
                            #[cfg(feature = "telemetry-otlp")]
                            traceparent,
                            #[cfg(feature = "telemetry-otlp")]
                            tracestate,
                        })
                        .await;
                });
            } else {
                state
                    .job_registry
                    .record_failure(&job.name, error.clone(), true);
                job_admin.record_failure(&job.id, error);
                release_local_unique_hold(
                    coordination,
                    uniqueness.as_ref(),
                    &job.name,
                    &job.payload,
                    &job.id,
                );
            }
        }
        JobExecutionOutcome::Panicked(error) => {
            tracing::error!(job = %job.name, error = %error, "local job handler panicked");
            state
                .job_registry
                .record_failure(&job.name, error.clone(), true);
            job_admin.record_failure(&job.id, error);
            release_local_unique_hold(
                coordination,
                uniqueness.as_ref(),
                &job.name,
                &job.payload,
                &job.id,
            );
        }
    }

    // The concurrency slot frees as soon as the handler is no longer running,
    // including while a retry waits out its backoff.
    finish_local_slot(coordination, concurrency_group.as_ref(), tx, state);
}

/// Release a unique hold after a job settles. No-op for TTL-window holds
/// (they expire by time) and when another job has since taken the key.
fn release_local_unique_hold(
    coordination: &Arc<LocalJobCoordination>,
    uniqueness: Option<&JobUniqueness>,
    name: &str,
    payload: &Value,
    job_id: &str,
) {
    if let Some(unique) = uniqueness {
        let key = job_unique_key(unique, payload);
        coordination.release_unique(name, &key, job_id);
    }
}

/// Free the job's concurrency slot and resume the next parked job, if any.
fn finish_local_slot(
    coordination: &Arc<LocalJobCoordination>,
    concurrency_group: Option<&(String, u32)>,
    tx: &tokio::sync::mpsc::Sender<QueuedJob>,
    state: &AppState,
) {
    let Some((group, _limit)) = concurrency_group else {
        return;
    };
    if let Some(next) = coordination.release_slot(group) {
        state.job_registry.record_concurrency_unblocked(&next.name);
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(next).await;
        });
    }
}

#[cfg(feature = "redis")]
#[derive(Clone)]
struct RedisClient {
    connection: redis::aio::ConnectionManager,
    /// Base key prefix (e.g. `autumn:jobs`) used to derive per-queue list keys.
    key_prefix: String,
    /// ZSET keyed by due-time-ms used for delayed enqueues and retries. A
    /// future-dated job is `ZADD`-ed here instead of pushed to its queue key, and
    /// the worker's promotion loop moves it onto the queue once due.
    delayed_key: String,
    record_prefix: String,
    unique_prefix: String,
}

#[cfg(feature = "redis")]
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(feature = "redis")]
fn redis_record_key(record_prefix: &str, id: &str) -> String {
    format!("{record_prefix}{id}")
}

/// List key for a named queue. The `default` queue keeps the legacy
/// `{prefix}:queue` key so an upgrade that doesn't opt into priority queues
/// keeps draining its existing backlog unchanged.
#[cfg(feature = "redis")]
fn redis_queue_key(key_prefix: &str, queue: &str) -> String {
    let queue = normalize_queue_name(queue);
    if queue == DEFAULT_QUEUE {
        format!("{key_prefix}:queue")
    } else {
        format!("{key_prefix}:queue:{queue}")
    }
}

#[cfg(feature = "redis")]
const fn redis_retry_delay_ms(initial_backoff_ms: u64, attempt: u32) -> u64 {
    initial_backoff_ms.saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1)))
}

#[cfg(feature = "redis")]
fn clear_redis_claim(record: &mut RedisJobRecord) {
    record.claimed_by = None;
    record.claimed_at_ms = None;
}

#[cfg(all(feature = "redis", test))]
fn claim_redis_record(
    mut record: RedisJobRecord,
    worker_id: &str,
    now_ms: u64,
    visibility_timeout_ms: u64,
) -> RedisClaimedRecord {
    record.claimed_by = Some(worker_id.to_string());
    record.claimed_at_ms = Some(now_ms);
    RedisClaimedRecord {
        record,
        deadline_ms: now_ms.saturating_add(visibility_timeout_ms),
    }
}

#[cfg(feature = "redis")]
fn prepare_redis_failure_action(
    mut record: RedisJobRecord,
    error: String,
    now_ms: u64,
) -> RedisFailureAction {
    clear_redis_claim(&mut record);
    record.last_error = Some(error);
    record.finished_at_ms = Some(now_ms);

    if record.attempt < record.max_attempts {
        let due_at_ms = now_ms.saturating_add(redis_retry_delay_ms(
            record.initial_backoff_ms,
            record.attempt,
        ));
        record.attempt = record.attempt.saturating_add(1);
        RedisFailureAction::Retry(RedisRetrySchedule { record, due_at_ms })
    } else {
        RedisFailureAction::DeadLetter(record)
    }
}

#[cfg(feature = "redis")]
fn prepare_redis_panic_dead_letter(
    mut record: RedisJobRecord,
    error: String,
    now_ms: u64,
) -> RedisJobRecord {
    clear_redis_claim(&mut record);
    record.last_error = Some(error);
    record.finished_at_ms = Some(now_ms);
    record
}

#[cfg(feature = "redis")]
fn recover_stale_redis_record(
    mut record: RedisJobRecord,
    now_ms: u64,
    visibility_timeout_ms: u64,
) -> Option<RedisStaleRecovery> {
    let claimed_at_ms = record.claimed_at_ms?;
    if claimed_at_ms.saturating_add(visibility_timeout_ms) > now_ms {
        return None;
    }

    let claimed_by = record
        .claimed_by
        .clone()
        .unwrap_or_else(|| "unknown worker".to_string());
    record.last_error = Some(format!(
        "visibility timeout expired for claim by {claimed_by} at {claimed_at_ms}"
    ));
    record.finished_at_ms = Some(now_ms);
    clear_redis_claim(&mut record);

    if record.attempt < record.max_attempts {
        record.attempt = record.attempt.saturating_add(1);
        Some(RedisStaleRecovery::Requeue(record))
    } else {
        Some(RedisStaleRecovery::DeadLetter(record))
    }
}

#[cfg(feature = "redis")]
fn encode_redis_record(record: &RedisJobRecord) -> AutumnResult<String> {
    serde_json::to_string(record).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "serialize durable job failed: {e}"
        )))
    })
}

/// Atomic enqueue: optionally takes the unique lock (`SET NX PX`), and only
/// when the lock is acquired stores the record and pushes the queue entry.
/// Returns 0 when the lock is already held, i.e. the enqueue coalesced.
#[cfg(feature = "redis")]
const REDIS_ENQUEUE_SCRIPT: &str = r"
if ARGV[3] == '1' then
  if not redis.call('SET', KEYS[3], ARGV[2], 'NX', 'PX', tonumber(ARGV[4])) then
    return 0
  end
end
redis.call('SET', KEYS[1], ARGV[1])
if ARGV[5] ~= '' and tonumber(ARGV[5]) ~= nil then
  redis.call('ZADD', KEYS[4], tonumber(ARGV[5]), ARGV[2])
else
  redis.call('LPUSH', KEYS[2], ARGV[2])
end
return 1
";

#[cfg(feature = "redis")]
impl RedisClient {
    #[allow(clippy::too_many_arguments)]
    async fn enqueue(
        &self,
        id: String,
        name: &str,
        queue: &str,
        payload: Value,
        default_max_attempts: u32,
        default_initial_backoff_ms: u64,
        due_at_ms: Option<u64>,
        constraints: &ResolvedJobConstraints,
    ) -> AutumnResult<EnqueueOutcome> {
        #[cfg(feature = "telemetry-otlp")]
        let (traceparent, tracestate) = capture_job_trace_context();
        let mut connection = self.connection.clone();
        let queue = normalize_queue_name(queue);
        let queue_key = redis_queue_key(&self.key_prefix, &queue);
        let msg = RedisJobRecord {
            id: id.clone(),
            name: name.to_string(),
            queue,
            payload,
            attempt: 1,
            max_attempts: default_max_attempts,
            initial_backoff_ms: default_initial_backoff_ms,
            enqueued_at_ms: Some(now_unix_ms()),
            started_at_ms: None,
            finished_at_ms: None,
            claimed_by: None,
            claimed_at_ms: None,
            last_error: None,
            unique_key: constraints.unique_key.clone(),
            unique_window: constraints.unique_window_tag().map(str::to_owned),
            concurrency_key: if constraints.concurrency_limit.is_some() {
                constraints.concurrency_scope.clone()
            } else {
                None
            },
            concurrency_limit: constraints.concurrency_limit,
            #[cfg(feature = "telemetry-otlp")]
            traceparent,
            #[cfg(feature = "telemetry-otlp")]
            tracestate,
        };
        let encoded = encode_redis_record(&msg)?;
        let record_key = redis_record_key(&self.record_prefix, &id);
        let unique_lock_key = constraints.unique_key.as_deref().map_or_else(
            || format!("{}-", self.unique_prefix),
            |key| redis_unique_lock_key(&self.unique_prefix, name, key),
        );
        let has_unique = if constraints.unique_key.is_some() {
            "1"
        } else {
            "0"
        };
        let lock_ttl_ms = {
            let base = redis_unique_lock_ttl_ms(constraints.unique_window);
            // For non-TTL windows the backstop is sized for pending/running
            // windows (~24 h). Extend it when a delayed job's due time would
            // outlast that backstop so the unique lock stays valid until the
            // job fires and is claimed.  TTL-window jobs keep their explicit
            // user-specified TTL unchanged.
            match due_at_ms {
                Some(due_ms)
                    if !matches!(
                        constraints.unique_window,
                        Some(JobUniquenessWindow::TtlMs(_))
                    ) =>
                {
                    let delay_ms = due_ms.saturating_sub(now_unix_ms());
                    delay_ms
                        .saturating_add(REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS)
                        .max(base)
                }
                _ => base,
            }
        };
        // Empty string => immediate (LPUSH); a millisecond score => delayed (ZADD).
        let due_at_arg = due_at_ms.map_or_else(String::new, |ms| ms.to_string());

        let stored: i64 = redis::cmd("EVAL")
            .arg(REDIS_ENQUEUE_SCRIPT)
            .arg(4)
            .arg(record_key)
            .arg(&queue_key)
            .arg(unique_lock_key)
            .arg(&self.delayed_key)
            .arg(encoded)
            .arg(id)
            .arg(has_unique)
            .arg(lock_ttl_ms)
            .arg(due_at_arg)
            .query_async(&mut connection)
            .await
            .map_err(|e| {
                AutumnError::internal_server_error(std::io::Error::other(format!(
                    "enqueue durable job failed: {e}"
                )))
            })?;
        if stored == 1 {
            Ok(EnqueueOutcome::Queued)
        } else {
            Ok(EnqueueOutcome::Deduplicated)
        }
    }
}

#[cfg(feature = "redis")]
#[derive(Clone)]
struct RedisJobAdminBackend {
    connection: redis::aio::ConnectionManager,
    /// Per-queue list keys (priority order) the dashboard reads enqueued jobs
    /// from. A single-queue app has just `{prefix}:queue`.
    queue_keys: Vec<String>,
    /// Base key prefix, used to route admin retry/cancel to a job's own queue.
    key_prefix: String,
    delayed_key: String,
    processing_key: String,
    dead_key: String,
    completed_key: String,
    blocked_key: String,
    record_prefix: String,
    dead_record_prefix: String,
    unique_prefix: String,
    history_limit: usize,
    registry: crate::actuator::JobRegistry,
}

#[cfg(feature = "redis")]
impl RedisJobAdminBackend {
    #[allow(clippy::too_many_arguments)]
    fn new(
        connection: redis::aio::ConnectionManager,
        queue_keys: Vec<String>,
        key_prefix: String,
        delayed_key: String,
        processing_key: String,
        dead_key: String,
        completed_key: String,
        blocked_key: String,
        record_prefix: String,
        dead_record_prefix: String,
        unique_prefix: String,
        history_limit: usize,
        registry: crate::actuator::JobRegistry,
    ) -> Self {
        Self {
            connection,
            queue_keys,
            key_prefix,
            delayed_key,
            processing_key,
            dead_key,
            completed_key,
            blocked_key,
            record_prefix,
            dead_record_prefix,
            unique_prefix,
            history_limit: history_limit.max(1),
            registry,
        }
    }

    async fn snapshot_redis(&self, query: &JobAdminQuery) -> AutumnResult<JobAdminSnapshot> {
        let mut connection = self.connection.clone();
        let per_page = query.per_page.clamp(1, 100);
        let now_ms = now_unix_ms();
        let completed_since = now_ms.saturating_sub(86_400_000);
        let failed_since = now_ms.saturating_sub(604_800_000);

        let enqueued = redis_admin_active_list_page(
            &mut connection,
            &self.queue_keys,
            &self.record_prefix,
            JobAdminStatus::Enqueued,
            query.enqueued_page,
            per_page,
        )
        .await?;
        let scheduled = redis_admin_delayed_page(
            &mut connection,
            &self.delayed_key,
            &self.record_prefix,
            query.scheduled_page,
            per_page,
        )
        .await?;
        let running = redis_admin_running_page(
            &mut connection,
            &self.processing_key,
            &self.record_prefix,
            query.running_page,
            per_page,
        )
        .await?;
        let completed = redis_admin_encoded_list_page(
            &mut connection,
            &self.completed_key,
            JobAdminStatus::Completed,
            Some(completed_since),
            query.completed_page,
            per_page,
            self.history_limit,
        )
        .await?;
        let failed = redis_admin_encoded_list_page(
            &mut connection,
            &self.dead_key,
            JobAdminStatus::Failed,
            Some(failed_since),
            query.failed_page,
            per_page,
            self.history_limit,
        )
        .await?;

        Ok(JobAdminSnapshot {
            enqueued,
            scheduled,
            running,
            completed,
            failed,
            schedules: Vec::new(),
            bounded_history_limit: self.history_limit,
        })
    }

    async fn retry_failed_redis(&self, id: &str) -> AutumnResult<()> {
        let mut connection = self.connection.clone();
        let new_id = uuid::Uuid::new_v4().to_string();
        let dead_record_key = format!("{}{id}", self.dead_record_prefix);
        // The unique lock was released when the job dead-lettered, so a
        // retried unique job must take it again under its new id — and the
        // retry must be refused when an equivalent job is already holding it,
        // otherwise the retry duplicates the very execution `unique` guards.
        let result: i64 = redis::cmd("EVAL")
            .arg(
                r"
local failed = redis.call('GET', KEYS[1])
if not failed then
  return 0
end
local ok, record = pcall(cjson.decode, failed)
if not ok then
  return -2
end
local lock = nil
if record['unique_key'] and record['unique_key'] ~= cjson.null
   and record['unique_window'] ~= 'ttl' then
  lock = KEYS[5] .. record['name'] .. ':' .. record['unique_key']
  if not redis.call('SET', lock, ARGV[1], 'NX', 'PX', tonumber(ARGV[3])) then
    return -3
  end
end
if redis.call('LREM', KEYS[2], 0, failed) == 0 then
  if lock and redis.call('GET', lock) == ARGV[1] then
    redis.call('DEL', lock)
  end
  return -1
end
redis.call('DEL', KEYS[1])
record['id'] = ARGV[1]
record['attempt'] = 1
record['enqueued_at_ms'] = tonumber(ARGV[2])
record['started_at_ms'] = nil
record['finished_at_ms'] = nil
record['claimed_by'] = nil
record['claimed_at_ms'] = nil
record['last_error'] = nil
local active = cjson.encode(record)
redis.call('SET', KEYS[3] .. ARGV[1], active)
local queue = 'default'
if record['queue'] and record['queue'] ~= cjson.null then
  queue = record['queue']
end
local qkey
if queue == 'default' then
  qkey = KEYS[4] .. ':queue'
else
  qkey = KEYS[4] .. ':queue:' .. queue
end
redis.call('LPUSH', qkey, ARGV[1])
return 1
",
            )
            .arg(5)
            .arg(dead_record_key)
            .arg(&self.dead_key)
            .arg(&self.record_prefix)
            .arg(&self.key_prefix)
            .arg(&self.unique_prefix)
            .arg(new_id)
            .arg(now_unix_ms())
            .arg(REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS)
            .query_async(&mut connection)
            .await
            .map_err(|error| redis_admin_error("retry failed job", &error))?;
        redis_admin_operation_result(result, id, "retry failed job")
    }

    async fn discard_failed_redis(&self, id: &str) -> AutumnResult<()> {
        let mut connection = self.connection.clone();
        let dead_record_key = format!("{}{id}", self.dead_record_prefix);
        let result: i64 = redis::cmd("EVAL")
            .arg(
                r"
local failed = redis.call('GET', KEYS[1])
if not failed then
  return 0
end
if redis.call('LREM', KEYS[2], 0, failed) == 0 then
  return -1
end
redis.call('DEL', KEYS[1])
return 1
",
            )
            .arg(2)
            .arg(dead_record_key)
            .arg(&self.dead_key)
            .query_async(&mut connection)
            .await
            .map_err(|error| redis_admin_error("discard failed job", &error))?;
        redis_admin_operation_result(result, id, "discard failed job")
    }

    async fn cancel_enqueued_redis(&self, id: &str) -> AutumnResult<()> {
        let mut connection = self.connection.clone();
        let active_record_key = redis_record_key(&self.record_prefix, id);
        // Fetch the record first so we know the job name for gauge accounting.
        let raw_record: Option<String> = redis::cmd("GET")
            .arg(&active_record_key)
            .query_async(&mut connection)
            .await
            .ok()
            .flatten();
        let parsed_record = raw_record
            .as_deref()
            .and_then(|s| serde_json::from_str::<RedisJobRecord>(s).ok());
        let job_name: Option<String> = parsed_record.as_ref().map(|r| r.name.clone());
        // Remove the job from its own queue list, not just the default one.
        let queue_key = parsed_record.as_ref().map_or_else(
            || redis_queue_key(&self.key_prefix, DEFAULT_QUEUE),
            |r| redis_queue_key(&self.key_prefix, &r.queue),
        );
        // A concurrency-parked job lives in the blocked zset rather than the
        // queue list, and a canceled unique job must hand its lock back so
        // future enqueues are not coalesced against work that will never run.
        let result: i64 = redis::cmd("EVAL")
            .arg(
                r"
local body = redis.call('GET', KEYS[1])
if not body then
  return 0
end
local removed = redis.call('LREM', KEYS[2], 0, ARGV[1])
if removed == 0 then
  removed = redis.call('ZREM', KEYS[3], ARGV[1])
end
if removed == 0 then
  removed = redis.call('ZREM', KEYS[5], ARGV[1])
end
if removed == 0 then
  return -1
end
local ok, record = pcall(cjson.decode, body)
if ok and record['unique_key'] and record['unique_key'] ~= cjson.null
   and record['unique_window'] ~= 'ttl' then
  local lock = KEYS[4] .. record['name'] .. ':' .. record['unique_key']
  if redis.call('GET', lock) == ARGV[1] then
    redis.call('DEL', lock)
  end
end
redis.call('DEL', KEYS[1])
return 1
",
            )
            .arg(5)
            .arg(&active_record_key)
            .arg(&queue_key)
            .arg(&self.blocked_key)
            .arg(&self.unique_prefix)
            .arg(&self.delayed_key)
            .arg(id)
            .query_async(&mut connection)
            .await
            .map_err(|error| redis_admin_error("cancel enqueued job", &error))?;
        if result == 1
            && let Some(name) = job_name
        {
            self.registry.record_cancel(&name);
        }
        redis_admin_operation_result(result, id, "cancel enqueued job")
    }
}

#[cfg(feature = "redis")]
impl JobAdminBackend for RedisJobAdminBackend {
    fn snapshot(&self, query: JobAdminQuery) -> JobAdminFuture<'_, JobAdminSnapshot> {
        Box::pin(async move { self.snapshot_redis(&query).await })
    }

    fn retry(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.retry_failed_redis(&id).await })
    }

    fn discard(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.discard_failed_redis(&id).await })
    }

    fn cancel(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.cancel_enqueued_redis(&id).await })
    }
}

#[cfg(feature = "redis")]
fn redis_admin_error(operation: &str, error: &redis::RedisError) -> AutumnError {
    AutumnError::internal_server_error(std::io::Error::other(format!(
        "redis job admin {operation} failed: {error}"
    )))
}

#[cfg(feature = "redis")]
fn redis_admin_operation_result(result: i64, id: &str, operation: &str) -> AutumnResult<()> {
    match result {
        1 => Ok(()),
        0 => Err(AutumnError::not_found_msg(format!("job '{id}' not found"))),
        -1 => Err(AutumnError::bad_request_msg(format!(
            "job '{id}' is not in the expected state for {operation}"
        ))),
        -2 => Err(AutumnError::internal_server_error_msg(format!(
            "job '{id}' has an invalid stored payload"
        ))),
        -3 => Err(AutumnError::bad_request_msg(format!(
            "an equivalent unique job is already pending or running; \
             retry job '{id}' after it settles"
        ))),
        _ => Err(AutumnError::internal_server_error_msg(format!(
            "redis job admin {operation} returned unexpected code {result}"
        ))),
    }
}

#[cfg(feature = "redis")]
fn redis_admin_time(ms: Option<u64>) -> Option<String> {
    let ms = i64::try_from(ms?).ok()?;
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(format_job_admin_time)
}

#[cfg(feature = "redis")]
fn redis_record_sort_time(record: &RedisJobRecord) -> u64 {
    record
        .finished_at_ms
        .or(record.started_at_ms)
        .or(record.enqueued_at_ms)
        .unwrap_or_default()
}

#[cfg(feature = "redis")]
fn redis_record_to_admin_record(record: &RedisJobRecord, status: JobAdminStatus) -> JobAdminRecord {
    let (principal_id, correlation_id) = job_payload_identity(&record.payload);
    JobAdminRecord {
        id: record.id.clone(),
        name: record.name.clone(),
        queue: normalize_queue_name(&record.queue),
        status,
        enqueued_at: redis_admin_time(record.enqueued_at_ms),
        scheduled_for: None,
        started_at: redis_admin_time(record.started_at_ms),
        finished_at: redis_admin_time(record.finished_at_ms),
        attempt: record.attempt,
        max_attempts: record.max_attempts,
        last_error: record.last_error.clone(),
        principal_id,
        correlation_id,
    }
}

#[cfg(feature = "redis")]
async fn redis_records_for_ids(
    connection: &mut redis::aio::ConnectionManager,
    record_prefix: &str,
    ids: &[String],
) -> Result<Vec<RedisJobRecord>, redis::RedisError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<String> = ids
        .iter()
        .map(|id| redis_record_key(record_prefix, id))
        .collect();
    let bodies: Vec<Option<String>> = redis::cmd("MGET").arg(keys).query_async(connection).await?;
    Ok(bodies
        .into_iter()
        .flatten()
        .filter_map(|body| serde_json::from_str::<RedisJobRecord>(&body).ok())
        .collect())
}

#[cfg(feature = "redis")]
async fn redis_admin_active_list_page(
    connection: &mut redis::aio::ConnectionManager,
    queue_keys: &[String],
    record_prefix: &str,
    status: JobAdminStatus,
    page: u64,
    per_page: u64,
) -> AutumnResult<JobAdminPage> {
    let page = page.max(1);
    let start = page.saturating_sub(1).saturating_mul(per_page);

    // Per-queue lengths so we can paginate across the priority-ordered queues as
    // one logical list (highest-priority queue first).
    let mut lens = Vec::with_capacity(queue_keys.len());
    let mut total = 0_u64;
    for queue_key in queue_keys {
        let len: u64 = redis::cmd("LLEN")
            .arg(queue_key)
            .query_async(connection)
            .await
            .map_err(|error| redis_admin_error("read enqueued length", &error))?;
        lens.push(len);
        total = total.saturating_add(len);
    }

    let mut ids: Vec<String> = Vec::new();
    let mut global_offset = 0_u64;
    for (queue_key, len) in queue_keys.iter().zip(lens) {
        if u64::try_from(ids.len()).unwrap_or(u64::MAX) >= per_page {
            break;
        }
        // Global indices covered by this queue: [global_offset, global_offset+len).
        if start >= global_offset.saturating_add(len) {
            global_offset = global_offset.saturating_add(len);
            continue;
        }
        let local_start = start.saturating_sub(global_offset);
        let remaining = per_page.saturating_sub(u64::try_from(ids.len()).unwrap_or(u64::MAX));
        let local_stop = local_start.saturating_add(remaining).saturating_sub(1);
        let chunk: Vec<String> = redis::cmd("LRANGE")
            .arg(queue_key)
            .arg(local_start)
            .arg(local_stop)
            .query_async(connection)
            .await
            .map_err(|error| redis_admin_error("read enqueued page", &error))?;
        ids.extend(chunk);
        global_offset = global_offset.saturating_add(len);
    }

    let records = redis_records_for_ids(connection, record_prefix, &ids)
        .await
        .map_err(|error| redis_admin_error("read enqueued records", &error))?
        .into_iter()
        .map(|record| redis_record_to_admin_record(&record, status))
        .collect();
    Ok(JobAdminPage::new(records, total, page, per_page))
}

/// Page over the delayed ZSET, surfacing future-due jobs as
/// [`JobAdminStatus::Scheduled`] with their due time (the ZSET score, in ms),
/// soonest-due first.
#[cfg(feature = "redis")]
async fn redis_admin_delayed_page(
    connection: &mut redis::aio::ConnectionManager,
    delayed_key: &str,
    record_prefix: &str,
    page: u64,
    per_page: u64,
) -> AutumnResult<JobAdminPage> {
    let page = page.max(1);
    let start = page.saturating_sub(1).saturating_mul(per_page);
    let stop = start.saturating_add(per_page).saturating_sub(1);
    // Fetch (id, score) pairs soonest-due-first, plus the total ZSET size.
    let (id_scores, total): (Vec<(String, f64)>, u64) = redis::pipe()
        .cmd("ZRANGE")
        .arg(delayed_key)
        .arg(start)
        .arg(stop)
        .arg("WITHSCORES")
        .cmd("ZCARD")
        .arg(delayed_key)
        .query_async(connection)
        .await
        .map_err(|error| redis_admin_error("read scheduled page", &error))?;
    let ids: Vec<String> = id_scores.iter().map(|(id, _)| id.clone()).collect();
    let due_by_id: std::collections::HashMap<String, f64> = id_scores.into_iter().collect();
    let records = redis_records_for_ids(connection, record_prefix, &ids)
        .await
        .map_err(|error| redis_admin_error("read scheduled records", &error))?
        .into_iter()
        .map(|record| {
            let mut admin = redis_record_to_admin_record(&record, JobAdminStatus::Scheduled);
            if let Some(score) = due_by_id.get(&record.id) {
                // ZSET scores are due-time-in-ms; clamp the f64 back to u64.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let due_ms = score.max(0.0) as u64;
                admin.scheduled_for = redis_admin_time(Some(due_ms));
            }
            admin
        })
        .collect();
    Ok(JobAdminPage::new(records, total, page, per_page))
}

#[cfg(feature = "redis")]
async fn redis_admin_running_page(
    connection: &mut redis::aio::ConnectionManager,
    processing_key: &str,
    record_prefix: &str,
    page: u64,
    per_page: u64,
) -> AutumnResult<JobAdminPage> {
    let page = page.max(1);
    let start = page.saturating_sub(1).saturating_mul(per_page);
    let stop = start.saturating_add(per_page).saturating_sub(1);
    let (ids, total): (Vec<String>, u64) = redis::pipe()
        .cmd("ZREVRANGE")
        .arg(processing_key)
        .arg(start)
        .arg(stop)
        .cmd("ZCARD")
        .arg(processing_key)
        .query_async(connection)
        .await
        .map_err(|error| redis_admin_error("read running page", &error))?;
    let mut records: Vec<_> = redis_records_for_ids(connection, record_prefix, &ids)
        .await
        .map_err(|error| redis_admin_error("read running records", &error))?
        .into_iter()
        .map(|record| redis_record_to_admin_record(&record, JobAdminStatus::Running))
        .collect();
    records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(JobAdminPage::new(records, total, page, per_page))
}

#[cfg(feature = "redis")]
async fn redis_admin_encoded_list_page(
    connection: &mut redis::aio::ConnectionManager,
    list_key: &str,
    status: JobAdminStatus,
    since_ms: Option<u64>,
    page: u64,
    per_page: u64,
    history_limit: usize,
) -> AutumnResult<JobAdminPage> {
    let page = page.max(1);
    let stop = isize::try_from(history_limit.saturating_sub(1)).unwrap_or(isize::MAX);
    let bodies: Vec<String> = redis::cmd("LRANGE")
        .arg(list_key)
        .arg(0)
        .arg(stop)
        .query_async(connection)
        .await
        .map_err(|error| redis_admin_error("read completed/failed list", &error))?;
    let mut records: Vec<_> = bodies
        .into_iter()
        .filter_map(|body| serde_json::from_str::<RedisJobRecord>(&body).ok())
        .filter(|record| since_ms.is_none_or(|since| redis_record_sort_time(record) >= since))
        .collect();
    records.sort_by_key(redis_record_sort_time);
    records.reverse();

    let total = records.len() as u64;
    let start =
        usize::try_from(page.saturating_sub(1).saturating_mul(per_page)).unwrap_or(usize::MAX);
    let take = usize::try_from(per_page).unwrap_or(usize::MAX);
    let page_records = records
        .into_iter()
        .skip(start)
        .take(take)
        .map(|record| redis_record_to_admin_record(&record, status))
        .collect();
    Ok(JobAdminPage::new(page_records, total, page, per_page))
}

#[cfg(feature = "redis")]
fn new_redis_connection_manager(
    client: &redis::Client,
    label: &str,
) -> Result<redis::aio::ConnectionManager, AutumnError> {
    use redis::aio::ConnectionManagerConfig;

    redis::aio::ConnectionManager::new_lazy_with_config(
        client.clone(),
        ConnectionManagerConfig::new(),
    )
    .map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "failed to create {label}: {e}"
        )))
    })
}

#[cfg(feature = "redis")]
async fn push_json_list_item<T: ?Sized + Serialize + Sync>(
    connection: &mut redis::aio::ConnectionManager,
    key: &str,
    value: &T,
) {
    use redis::AsyncCommands as _;

    if let Ok(encoded) = serde_json::to_string(value) {
        let _ = connection.lpush::<_, _, ()>(key, encoded).await;
    }
}

#[cfg(feature = "redis")]
#[allow(clippy::too_many_lines)]
async fn claim_next_redis_job(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    queue_keys: &[String],
) -> Result<Option<RedisJobRecord>, redis::RedisError> {
    // Walks the priority-ordered queue list keys (ARGV[9..]) highest first,
    // popping entries until one is claimable. Jobs whose concurrency group is
    // saturated are parked into the blocked zset (KEYS[3]) with a short due time
    // and retried via promotion; the scan bound keeps one call from walking an
    // arbitrarily long queue. The concurrency counter INCR is atomic with the
    // claim itself, so two workers can never both observe a free slot for the
    // last opening in a group.
    const CLAIM_SCRIPT: &str = r"
local function scope_string(value)
  if value == nil or value == cjson.null then
    return ''
  end
  return tostring(value)
end
local queue_count = tonumber(ARGV[9])
for qi = 1, queue_count do
  local queue_key = ARGV[9 + qi]
  for attempt = 1, tonumber(ARGV[6]) do
    local id = redis.call('RPOP', queue_key)
    if not id then
      break
    end
    local key = KEYS[2] .. id
    local body = redis.call('GET', key)
    if body then
      local ok, record = pcall(cjson.decode, body)
      if not ok then
        redis.call('ZADD', KEYS[1], ARGV[3], id)
        return { id, body }
      end
      local blocked = false
      if record['concurrency_limit'] and record['concurrency_limit'] ~= cjson.null then
        local counter = ARGV[4] .. record['name'] .. ':' .. scope_string(record['concurrency_key'])
        local current = tonumber(redis.call('GET', counter) or '0')
        if current >= tonumber(record['concurrency_limit']) then
          redis.call('ZADD', KEYS[3], ARGV[5], id)
          blocked = true
        else
          redis.call('INCR', counter)
        end
      end
      if not blocked then
        if record['unique_key'] and record['unique_key'] ~= cjson.null then
          local lock = ARGV[7] .. record['name'] .. ':' .. record['unique_key']
          if record['unique_window'] == 'pending' then
            if redis.call('GET', lock) == record['id'] then
              redis.call('DEL', lock)
            end
          elseif record['unique_window'] == 'running' then
            redis.call('PEXPIRE', lock, tonumber(ARGV[8]))
          end
        end
        record['claimed_by'] = ARGV[1]
        record['claimed_at_ms'] = tonumber(ARGV[2])
        record['started_at_ms'] = tonumber(ARGV[2])
        record['finished_at_ms'] = nil
        local updated = cjson.encode(record)
        redis.call('SET', key, updated)
        redis.call('ZADD', KEYS[1], ARGV[3], id)
        return { id, updated }
      end
    end
  end
end
return nil
";

    let now_ms = now_unix_ms();
    let deadline_ms = now_ms.saturating_add(worker_config.visibility_timeout_ms);
    let blocked_due_ms = now_ms.saturating_add(REDIS_CONCURRENCY_REQUEUE_DELAY_MS);
    let mut cmd = redis::cmd("EVAL");
    cmd.arg(CLAIM_SCRIPT)
        .arg(3)
        .arg(&worker_config.processing_key)
        .arg(&worker_config.record_prefix)
        .arg(&worker_config.blocked_key)
        .arg(&worker_config.worker_id)
        .arg(now_ms)
        .arg(deadline_ms)
        .arg(&worker_config.concurrency_prefix)
        .arg(blocked_due_ms)
        .arg(REDIS_CLAIM_SCAN_LIMIT)
        .arg(&worker_config.unique_prefix)
        .arg(REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS)
        .arg(queue_keys.len());
    for queue_key in queue_keys {
        cmd.arg(queue_key);
    }
    let response: Option<(String, String)> = cmd.query_async(connection).await?;

    let Some((id, body)) = response else {
        return Ok(None);
    };

    match serde_json::from_str::<RedisJobRecord>(&body) {
        Ok(record) => Ok(Some(record)),
        Err(error) => {
            tracing::warn!(job_id = %id, error = %error, "invalid durable job record");
            let malformed_id = id.clone();
            // The Lua side may have already taken a concurrency slot for this
            // record (cjson decoded it even though serde did not); read the
            // raw fields back to settle the counter and unique lock.
            settle_malformed_redis_claim(connection, worker_config, &body).await;
            let malformed = serde_json::json!({
                "id": id,
                "error": error.to_string(),
                "raw_payload": body,
            });
            push_json_list_item(connection, &worker_config.dead_key, &malformed).await;
            let _ = redis::cmd("ZREM")
                .arg(&worker_config.processing_key)
                .arg(malformed_id)
                .query_async::<usize>(connection)
                .await;
            Ok(None)
        }
    }
}

/// Settle the concurrency counter and unique lock for a record that the claim
/// script decoded (and therefore claimed a slot for) but serde rejected.
#[cfg(feature = "redis")]
async fn settle_malformed_redis_claim(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    body: &str,
) {
    let Ok(raw) = serde_json::from_str::<Value>(body) else {
        return;
    };
    let Some(name) = raw.get("name").and_then(Value::as_str) else {
        return;
    };
    if raw.get("concurrency_limit").is_some_and(Value::is_u64) {
        let scope = raw
            .get("concurrency_key")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let counter = redis_concurrency_counter_key(
            &worker_config.concurrency_prefix,
            name,
            scope.as_deref(),
        );
        let _ = redis::cmd("EVAL")
            .arg(REDIS_COUNTER_DECREMENT_SCRIPT)
            .arg(1)
            .arg(counter)
            .query_async::<i64>(connection)
            .await;
    }
    if let (Some(unique_key), Some(id)) = (
        raw.get("unique_key").and_then(Value::as_str),
        raw.get("id").and_then(Value::as_str),
    ) && raw.get("unique_window").and_then(Value::as_str) != Some("ttl")
    {
        let lock = redis_unique_lock_key(&worker_config.unique_prefix, name, unique_key);
        let _ = redis::cmd("EVAL")
            .arg("if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('DEL', KEYS[1]) end return 0")
            .arg(1)
            .arg(lock)
            .arg(id)
            .query_async::<i64>(connection)
            .await;
    }
}

/// Decrement a concurrency counter, deleting it at zero.
#[cfg(feature = "redis")]
const REDIS_COUNTER_DECREMENT_SCRIPT: &str = r"
local current = tonumber(redis.call('GET', KEYS[1]) or '0')
if current <= 1 then
  redis.call('DEL', KEYS[1])
  return 0
end
redis.call('SET', KEYS[1], current - 1)
return current - 1
";

#[cfg(feature = "redis")]
async fn record_enqueues_for_redis_ids(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
    ids: &[String],
) -> Result<(), redis::RedisError> {
    if ids.is_empty() {
        return Ok(());
    }

    let keys: Vec<String> = ids
        .iter()
        .map(|id| redis_record_key(&worker_config.record_prefix, id))
        .collect();
    let bodies: Vec<Option<String>> = redis::cmd("MGET")
        .arg(&keys)
        .query_async(connection)
        .await?;

    for body in bodies.into_iter().flatten() {
        if let Ok(mut record) = serde_json::from_str::<RedisJobRecord>(&body) {
            record.enqueued_at_ms = Some(now_unix_ms());
            record.started_at_ms = None;
            record.finished_at_ms = None;
            clear_redis_claim(&mut record);
            if let Ok(encoded) = encode_redis_record(&record) {
                let key = redis_record_key(&worker_config.record_prefix, &record.id);
                let _ = redis::cmd("SET")
                    .arg(key)
                    .arg(encoded)
                    .query_async::<()>(&mut *connection)
                    .await;
            }
            // Skip record_enqueue for initially-delayed jobs: they were already
            // counted at enqueue time (queued += 1). Only retries and stale
            // claim recoveries (prior status != Scheduled) need a fresh count.
            let was_scheduled = job_admin.record_requeued(&record.id, record.attempt);
            if !was_scheduled {
                state.job_registry.record_enqueue(&record.name);
            }
        }
    }

    Ok(())
}

#[cfg(feature = "redis")]
async fn promote_due_redis_retries(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
) -> Result<(), redis::RedisError> {
    // Route each promoted job back onto its own named queue list. `ARGV[3]` is
    // the base key prefix and `KEYS[2]` the record-key prefix; the queue is read
    // from the stored record (defaulting to `default`).
    const PROMOTE_SCRIPT: &str = r"
local ids = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, ARGV[2])
local promoted = {}
local key_prefix = ARGV[3]
for _, id in ipairs(ids) do
  if redis.call('ZREM', KEYS[1], id) == 1 then
    local queue = 'default'
    local body = redis.call('GET', KEYS[2] .. id)
    if body then
      local ok, record = pcall(cjson.decode, body)
      if ok and record['queue'] and record['queue'] ~= cjson.null then
        queue = record['queue']
      end
    end
    local qkey
    if queue == 'default' then
      qkey = key_prefix .. ':queue'
    else
      qkey = key_prefix .. ':queue:' .. queue
    end
    redis.call('LPUSH', qkey, id)
    table.insert(promoted, id)
  end
end
return promoted
";

    let promoted: Vec<String> = redis::cmd("EVAL")
        .arg(PROMOTE_SCRIPT)
        .arg(2)
        .arg(&worker_config.delayed_key)
        .arg(&worker_config.record_prefix)
        .arg(now_unix_ms())
        .arg(64_usize)
        .arg(&worker_config.key_prefix)
        .query_async(connection)
        .await?;

    record_enqueues_for_redis_ids(connection, worker_config, state, job_admin, &promoted).await?;
    Ok(())
}

/// Move parked (concurrency-blocked) jobs whose retry time arrived back into
/// the queue. Unlike retry promotion this records no bookkeeping: a parked
/// job never stopped being enqueued from the dashboard's point of view.
#[cfg(feature = "redis")]
async fn promote_due_blocked_redis_jobs(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
) -> Result<(), redis::RedisError> {
    const PROMOTE_BLOCKED_SCRIPT: &str = r"
local ids = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, ARGV[2])
local key_prefix = ARGV[3]
for _, id in ipairs(ids) do
  if redis.call('ZREM', KEYS[1], id) == 1 then
    local queue = 'default'
    local body = redis.call('GET', KEYS[2] .. id)
    if body then
      local ok, record = pcall(cjson.decode, body)
      if ok and record['queue'] and record['queue'] ~= cjson.null then
        queue = record['queue']
      end
    end
    local qkey
    if queue == 'default' then
      qkey = key_prefix .. ':queue'
    else
      qkey = key_prefix .. ':queue:' .. queue
    end
    redis.call('LPUSH', qkey, id)
  end
end
return #ids
";
    let _promoted: i64 = redis::cmd("EVAL")
        .arg(PROMOTE_BLOCKED_SCRIPT)
        .arg(2)
        .arg(&worker_config.blocked_key)
        .arg(&worker_config.record_prefix)
        .arg(now_unix_ms())
        .arg(64_usize)
        .arg(&worker_config.key_prefix)
        .query_async(connection)
        .await?;
    Ok(())
}

/// Publish per-name blocked-on-concurrency gauges from the blocked zset.
#[cfg(feature = "redis")]
async fn update_redis_blocked_gauges(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    state: &AppState,
) -> Result<(), redis::RedisError> {
    let ids: Vec<String> = redis::cmd("ZRANGE")
        .arg(&worker_config.blocked_key)
        .arg(0)
        .arg(1023)
        .query_async(connection)
        .await?;
    let mut counts: HashMap<String, u64> = HashMap::new();
    if !ids.is_empty() {
        let keys: Vec<String> = ids
            .iter()
            .map(|id| redis_record_key(&worker_config.record_prefix, id))
            .collect();
        let bodies: Vec<Option<String>> =
            redis::cmd("MGET").arg(keys).query_async(connection).await?;
        for body in bodies.into_iter().flatten() {
            if let Ok(record) = serde_json::from_str::<RedisJobRecord>(&body) {
                *counts.entry(record.name).or_insert(0) += 1;
            }
        }
    }
    state.job_registry.set_concurrency_blocked_counts(&counts);
    Ok(())
}

#[cfg(feature = "redis")]
fn expected_claim_args(record: &RedisJobRecord) -> Option<(&str, u64)> {
    Some((record.claimed_by.as_deref()?, record.claimed_at_ms?))
}

#[cfg(feature = "redis")]
const CLAIMED_REDIS_TRANSITION_SCRIPT: &str = r"
local function trim_dead_history(dead_key, dead_record_prefix, limit)
  local trimmed_records = redis.call('LRANGE', dead_key, limit, -1)
  for _, encoded in ipairs(trimmed_records) do
    local trimmed_ok, trimmed = pcall(cjson.decode, encoded)
    if trimmed_ok and trimmed['id'] then
      redis.call('DEL', dead_record_prefix .. trimmed['id'])
    end
  end
  redis.call('LTRIM', dead_key, 0, limit - 1)
end
local key = KEYS[2] .. ARGV[1]
local body = redis.call('GET', key)
if not body then
  return 0
end
local ok, record = pcall(cjson.decode, body)
if not ok then
  return 0
end
if record['claimed_by'] ~= ARGV[2] then
  return 0
end
if record['claimed_at_ms'] ~= tonumber(ARGV[3]) then
  return 0
end
redis.call('ZREM', KEYS[1], ARGV[1])
if ARGV[9] == '1' then
  local slots = tonumber(redis.call('GET', KEYS[8]) or '0')
  if slots <= 1 then
    redis.call('DEL', KEYS[8])
  else
    redis.call('SET', KEYS[8], slots - 1)
  end
end
if ARGV[8] == '1' and redis.call('GET', KEYS[7]) == ARGV[1] then
  redis.call('DEL', KEYS[7])
end
if ARGV[4] == 'success' then
  redis.call('LPUSH', KEYS[5], ARGV[5])
  redis.call('LTRIM', KEYS[5], 0, tonumber(ARGV[7]) - 1)
  redis.call('DEL', key)
elseif ARGV[4] == 'retry' then
  if ARGV[10] == 'pending' then
    if not redis.call('SET', KEYS[7], ARGV[1], 'NX', 'PX', tonumber(ARGV[11])) then
      redis.call('DEL', key)
      return 1
    end
  end
  redis.call('SET', key, ARGV[5])
  redis.call('ZADD', KEYS[3], ARGV[6], ARGV[1])
  if ARGV[10] == 'running' then
    redis.call('PEXPIRE', KEYS[7], tonumber(ARGV[11]))
  end
elseif ARGV[4] == 'dead' then
  redis.call('LPUSH', KEYS[4], ARGV[5])
  redis.call('SET', KEYS[6] .. ARGV[1], ARGV[5])
  trim_dead_history(KEYS[4], KEYS[6], tonumber(ARGV[7]))
  redis.call('DEL', key)
else
  return 0
end
return 1
";

#[cfg(feature = "redis")]
async fn apply_claimed_redis_transition(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    expected: &RedisJobRecord,
    mode: &str,
    encoded_record: Option<String>,
    due_at_ms: Option<u64>,
) -> Result<bool, redis::RedisError> {
    let Some((claimed_by, claimed_at_ms)) = expected_claim_args(expected) else {
        return Ok(false);
    };

    // The concurrency slot frees on every settle (success, retry backoff,
    // dead-letter): the handler is no longer executing in any of them. The
    // unique lock is only released on terminal settles for non-TTL windows.
    let release_unique = if redis_release_unique_on_settle(expected, mode) {
        "1"
    } else {
        "0"
    };
    let decrement_slot = if expected.concurrency_limit.is_some() {
        "1"
    } else {
        "0"
    };
    let applied: usize = redis::cmd("EVAL")
        .arg(CLAIMED_REDIS_TRANSITION_SCRIPT)
        .arg(8)
        .arg(&worker_config.processing_key)
        .arg(&worker_config.record_prefix)
        .arg(&worker_config.delayed_key)
        .arg(&worker_config.dead_key)
        .arg(&worker_config.completed_key)
        .arg(&worker_config.dead_record_prefix)
        .arg(worker_config.unique_lock_key_for(expected))
        .arg(worker_config.concurrency_counter_key_for(expected))
        .arg(&expected.id)
        .arg(claimed_by)
        .arg(claimed_at_ms)
        .arg(mode)
        .arg(encoded_record.unwrap_or_default())
        .arg(due_at_ms.unwrap_or_default())
        .arg(DEFAULT_JOB_ADMIN_HISTORY_LIMIT)
        .arg(release_unique)
        .arg(decrement_slot)
        .arg(if mode == "retry" {
            redis_requeue_unique_action(expected)
        } else {
            ""
        })
        .arg(REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS)
        .query_async(connection)
        .await?;

    Ok(applied == 1)
}

#[cfg(feature = "redis")]
async fn ack_redis_success(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    record: &RedisJobRecord,
) -> Result<bool, redis::RedisError> {
    let mut completed = record.clone();
    clear_redis_claim(&mut completed);
    completed.finished_at_ms = Some(now_unix_ms());
    completed.last_error = None;
    let Ok(encoded) = encode_redis_record(&completed) else {
        tracing::warn!(job_id = %record.id, "failed to serialize redis completed record");
        return Ok(false);
    };
    apply_claimed_redis_transition(
        connection,
        worker_config,
        record,
        "success",
        Some(encoded),
        None,
    )
    .await
}

#[cfg(feature = "redis")]
async fn schedule_redis_retry(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    expected: &RedisJobRecord,
    schedule: &RedisRetrySchedule,
) -> Result<bool, redis::RedisError> {
    let Ok(encoded) = encode_redis_record(&schedule.record) else {
        tracing::warn!(job_id = %schedule.record.id, "failed to serialize redis retry record");
        return Ok(false);
    };
    apply_claimed_redis_transition(
        connection,
        worker_config,
        expected,
        "retry",
        Some(encoded),
        Some(schedule.due_at_ms),
    )
    .await
}

#[cfg(feature = "redis")]
async fn dead_letter_redis_job(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    expected: &RedisJobRecord,
    record: &RedisJobRecord,
) -> Result<bool, redis::RedisError> {
    let Ok(encoded) = encode_redis_record(record) else {
        tracing::warn!(job_id = %record.id, "failed to serialize redis dead-letter record");
        return Ok(false);
    };
    apply_claimed_redis_transition(
        connection,
        worker_config,
        expected,
        "dead",
        Some(encoded),
        None,
    )
    .await
}

#[cfg(feature = "redis")]
const STALE_REDIS_RECOVERY_SCRIPT: &str = r"
local function trim_dead_history(dead_key, dead_record_prefix, limit)
  local trimmed_records = redis.call('LRANGE', dead_key, limit, -1)
  for _, encoded in ipairs(trimmed_records) do
    local trimmed_ok, trimmed = pcall(cjson.decode, encoded)
    if trimmed_ok and trimmed['id'] then
      redis.call('DEL', dead_record_prefix .. trimmed['id'])
    end
  end
  redis.call('LTRIM', dead_key, 0, limit - 1)
end
local key = KEYS[2] .. ARGV[1]
local body = redis.call('GET', key)
if not body then
  redis.call('ZREM', KEYS[1], ARGV[1])
  return 0
end
local ok, record = pcall(cjson.decode, body)
if not ok then
  redis.call('ZREM', KEYS[1], ARGV[1])
  return 0
end
if record['claimed_by'] ~= ARGV[2] then
  return 0
end
if record['claimed_at_ms'] ~= tonumber(ARGV[3]) then
  return 0
end
redis.call('ZREM', KEYS[1], ARGV[1])
if ARGV[8] == '1' then
  local slots = tonumber(redis.call('GET', KEYS[7]) or '0')
  if slots <= 1 then
    redis.call('DEL', KEYS[7])
  else
    redis.call('SET', KEYS[7], slots - 1)
  end
end
if ARGV[7] == '1' and redis.call('GET', KEYS[6]) == ARGV[1] then
  redis.call('DEL', KEYS[6])
end
if ARGV[4] == 'requeue' then
  if ARGV[9] == 'pending' then
    if not redis.call('SET', KEYS[6], ARGV[1], 'NX', 'PX', tonumber(ARGV[10])) then
      redis.call('DEL', key)
      return 1
    end
  end
  redis.call('SET', key, ARGV[5])
  redis.call('LPUSH', KEYS[3], ARGV[1])
  if ARGV[9] == 'running' then
    redis.call('PEXPIRE', KEYS[6], tonumber(ARGV[10]))
  end
elseif ARGV[4] == 'dead' then
  redis.call('LPUSH', KEYS[4], ARGV[5])
  redis.call('SET', KEYS[5] .. ARGV[1], ARGV[5])
  trim_dead_history(KEYS[4], KEYS[5], tonumber(ARGV[6]))
  redis.call('DEL', key)
else
  return 0
end
return 1
";

#[cfg(feature = "redis")]
async fn apply_stale_redis_recovery(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    expected: &RedisJobRecord,
    action: &RedisStaleRecovery,
) -> Result<bool, redis::RedisError> {
    let Some((claimed_by, claimed_at_ms)) = expected_claim_args(expected) else {
        return Ok(false);
    };
    let (mode, record) = match action {
        RedisStaleRecovery::Requeue(record) => ("requeue", record),
        RedisStaleRecovery::DeadLetter(record) => ("dead", record),
    };
    let Ok(encoded) = encode_redis_record(record) else {
        tracing::warn!(job_id = %record.id, "failed to serialize stale redis record");
        return Ok(false);
    };

    // A reclaimed worker crash must free the concurrency slot in both modes
    // (the handler is gone either way); the unique lock is released only when
    // the job dead-letters — a requeued job is still logically in flight.
    let release_unique = if redis_release_unique_on_settle(expected, mode) {
        "1"
    } else {
        "0"
    };
    let decrement_slot = if expected.concurrency_limit.is_some() {
        "1"
    } else {
        "0"
    };
    // A requeued stale job returns to its own named queue, not the default one.
    let requeue_key = redis_queue_key(&worker_config.key_prefix, &record.queue);
    let applied: usize = redis::cmd("EVAL")
        .arg(STALE_REDIS_RECOVERY_SCRIPT)
        .arg(7)
        .arg(&worker_config.processing_key)
        .arg(&worker_config.record_prefix)
        .arg(&requeue_key)
        .arg(&worker_config.dead_key)
        .arg(&worker_config.dead_record_prefix)
        .arg(worker_config.unique_lock_key_for(expected))
        .arg(worker_config.concurrency_counter_key_for(expected))
        .arg(&expected.id)
        .arg(claimed_by)
        .arg(claimed_at_ms)
        .arg(mode)
        .arg(encoded)
        .arg(DEFAULT_JOB_ADMIN_HISTORY_LIMIT)
        .arg(release_unique)
        .arg(decrement_slot)
        .arg(if mode == "requeue" {
            redis_requeue_unique_action(expected)
        } else {
            ""
        })
        .arg(REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS)
        .query_async(connection)
        .await?;

    Ok(applied == 1)
}

#[cfg(feature = "redis")]
async fn recover_stale_redis_jobs(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
) -> Result<(), redis::RedisError> {
    let stale_ids: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg(&worker_config.processing_key)
        .arg("-inf")
        .arg(now_unix_ms())
        .arg("LIMIT")
        .arg(0)
        .arg(64)
        .query_async(connection)
        .await?;

    if stale_ids.is_empty() {
        return Ok(());
    }

    let keys: Vec<String> = stale_ids
        .iter()
        .map(|id| redis_record_key(&worker_config.record_prefix, id))
        .collect();
    let bodies: Vec<Option<String>> = redis::cmd("MGET")
        .arg(&keys)
        .query_async(connection)
        .await?;

    for (id, body) in stale_ids.into_iter().zip(bodies) {
        let Some(body) = body else {
            let _ = redis::cmd("ZREM")
                .arg(&worker_config.processing_key)
                .arg(&id)
                .query_async::<usize>(connection)
                .await?;
            continue;
        };
        let Ok(record) = serde_json::from_str::<RedisJobRecord>(&body) else {
            let _ = redis::cmd("ZREM")
                .arg(&worker_config.processing_key)
                .arg(&id)
                .query_async::<usize>(connection)
                .await?;
            continue;
        };
        let Some(action) = recover_stale_redis_record(
            record.clone(),
            now_unix_ms(),
            worker_config.visibility_timeout_ms,
        ) else {
            continue;
        };

        if apply_stale_redis_recovery(connection, worker_config, &record, &action).await? {
            match &action {
                RedisStaleRecovery::Requeue(requeued) => {
                    if let Some(error) = requeued.last_error.as_deref() {
                        state
                            .job_registry
                            .record_retry(&requeued.name, error, record.attempt);
                        job_admin.record_retrying(&requeued.id, error);
                    }
                    state.job_registry.record_enqueue(&requeued.name);
                    job_admin.record_requeued(&requeued.id, requeued.attempt);
                }
                RedisStaleRecovery::DeadLetter(dead) => {
                    let error = dead
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "visibility timeout expired".to_string());
                    state
                        .job_registry
                        .record_failure(&dead.name, error.clone(), true);
                    job_admin.record_failure(&dead.id, error);
                }
            }
        }
    }

    Ok(())
}

#[cfg(feature = "redis")]
fn spawn_redis_worker(
    client: &redis::Client,
    jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>>,
    state: AppState,
    job_admin: JobAdminMemoryBackend,
    shutdown: tokio_util::sync::CancellationToken,
    worker_config: RedisWorkerConfig,
) -> Result<(), AutumnError> {
    let mut connection =
        new_redis_connection_manager(client, "jobs redis worker connection manager")?;

    tokio::spawn(async move {
        let mut retry_promotion_throttle = RedisMaintenanceThrottle::new(
            std::time::Instant::now(),
            worker_config.retry_promotion_interval,
        );
        let mut stale_recovery_throttle = RedisMaintenanceThrottle::new(
            std::time::Instant::now(),
            REDIS_STALE_MAINTENANCE_INTERVAL,
        );
        let mut blocked_promotion_throttle = RedisMaintenanceThrottle::new(
            std::time::Instant::now(),
            REDIS_BLOCKED_PROMOTION_INTERVAL,
        );
        let idle_sleep = redis_worker_idle_sleep(worker_config.retry_promotion_interval);
        let mut queue_cursor = worker_config.schedule.cursor();

        loop {
            if shutdown.is_cancelled() {
                break;
            }

            if retry_promotion_throttle.take_due(std::time::Instant::now()) {
                match promote_due_redis_retries(&mut connection, &worker_config, &state, &job_admin)
                    .await
                {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "redis job worker retry promotion failed");
                    }
                }
            }

            if stale_recovery_throttle.take_due(std::time::Instant::now()) {
                match recover_stale_redis_jobs(&mut connection, &worker_config, &state, &job_admin)
                    .await
                {
                    Ok(()) => {}
                    Err(error) => {
                        tracing::warn!(error = %error, "redis job worker stale recovery failed");
                    }
                }
                if let Err(error) =
                    update_redis_blocked_gauges(&mut connection, &worker_config, &state).await
                {
                    tracing::warn!(error = %error, "redis blocked-concurrency survey failed");
                }
            }

            if blocked_promotion_throttle.take_due(std::time::Instant::now())
                && let Err(error) =
                    promote_due_blocked_redis_jobs(&mut connection, &worker_config).await
            {
                tracing::warn!(error = %error, "redis blocked job promotion failed");
            }

            let queue_keys = worker_config.queue_keys_for(&queue_cursor.next_order());
            let claimed =
                match claim_next_redis_job(&mut connection, &worker_config, &queue_keys).await {
                    Ok(record) => record,
                    Err(error) => {
                        tracing::warn!(error = %error, "redis job worker claim failed");
                        tokio::time::sleep(idle_sleep).await;
                        continue;
                    }
                };
            let Some(record) = claimed else {
                tokio::time::sleep(idle_sleep).await;
                continue;
            };

            process_redis_job_record(
                &mut connection,
                record,
                &jobs_by_name,
                &state,
                &job_admin,
                &worker_config,
            )
            .await;
        }
    });

    Ok(())
}

#[cfg(feature = "redis")]
#[allow(clippy::cognitive_complexity)]
async fn settle_failed_redis_job(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    state: &AppState,
    record: &RedisJobRecord,
    error: String,
    outcome: &str,
    job_admin: &JobAdminMemoryBackend,
) {
    let action = prepare_redis_failure_action(record.clone(), error.clone(), now_unix_ms());
    match action {
        RedisFailureAction::Retry(schedule) => {
            match schedule_redis_retry(connection, worker_config, record, &schedule).await {
                Ok(true) => {
                    state
                        .job_registry
                        .record_retry(&schedule.record.name, &error, record.attempt);
                    job_admin.record_retrying(&schedule.record.id, &error);
                }
                Ok(false) => tracing::warn!(
                    job = %record.name,
                    job_id = %record.id,
                    outcome = %outcome,
                    "redis job retry skipped because claim changed"
                ),
                Err(error) => tracing::warn!(
                    job = %record.name,
                    job_id = %record.id,
                    outcome = %outcome,
                    error = %error,
                    "redis job retry scheduling failed"
                ),
            }
        }
        RedisFailureAction::DeadLetter(dead) => {
            match dead_letter_redis_job(connection, worker_config, record, &dead).await {
                Ok(true) => {
                    state
                        .job_registry
                        .record_failure(&dead.name, error.clone(), true);
                    job_admin.record_failure(&dead.id, error);
                }
                Ok(false) => tracing::warn!(
                    job = %record.name,
                    job_id = %record.id,
                    outcome = %outcome,
                    "redis job dead-letter skipped because claim changed"
                ),
                Err(error) => tracing::warn!(
                    job = %record.name,
                    job_id = %record.id,
                    outcome = %outcome,
                    error = %error,
                    "redis job dead-letter failed"
                ),
            }
        }
    }
}

#[cfg(feature = "redis")]
async fn dead_letter_panicked_redis_job(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    state: &AppState,
    record: &RedisJobRecord,
    error: String,
    job_admin: &JobAdminMemoryBackend,
) {
    let dead = prepare_redis_panic_dead_letter(record.clone(), error.clone(), now_unix_ms());
    match dead_letter_redis_job(connection, worker_config, record, &dead).await {
        Ok(true) => {
            state
                .job_registry
                .record_failure(&dead.name, error.clone(), true);
            job_admin.record_failure(&dead.id, error);
        }
        Ok(false) => tracing::warn!(
            job = %record.name,
            job_id = %record.id,
            "redis job panic dead-letter skipped because claim changed"
        ),
        Err(error) => tracing::warn!(
            job = %record.name,
            job_id = %record.id,
            error = %error,
            "redis job panic dead-letter failed"
        ),
    }
}

#[cfg(feature = "redis")]
async fn dead_letter_invalid_redis_job(
    connection: &mut redis::aio::ConnectionManager,
    worker_config: &RedisWorkerConfig,
    state: &AppState,
    record: &RedisJobRecord,
    error: &str,
    job_admin: &JobAdminMemoryBackend,
) {
    state
        .job_registry
        .record_failure(&record.name, error.to_owned(), true);
    job_admin.record_failure(&record.id, error.to_owned());
    let mut dead = record.clone();
    clear_redis_claim(&mut dead);
    dead.last_error = Some(error.to_owned());
    let _ = dead_letter_redis_job(connection, worker_config, record, &dead).await;
}

#[cfg(feature = "redis")]
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
async fn process_redis_job_record(
    connection: &mut redis::aio::ConnectionManager,
    mut record: RedisJobRecord,
    jobs_by_name: &Arc<RwLock<HashMap<String, JobInfo>>>,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
    worker_config: &RedisWorkerConfig,
) {
    if job_admin.try_record_start(&record.id, record.attempt) == JobAdminStartDecision::Canceled {
        state.job_registry.record_cancel(&record.name);
        job_admin.record_cancelled(&record.id);
        let _ = ack_redis_success(connection, worker_config, &record).await;
        return;
    }
    state.job_registry.record_start(&record.name);

    let maybe_info = {
        let guard = jobs_by_name.read().expect("job registry lock poisoned");
        guard
            .get(&record.name)
            .map(|info| (info.handler, info.max_attempts, info.initial_backoff_ms))
    };
    let Some((handler, info_max_attempts, info_backoff_ms)) = maybe_info else {
        dead_letter_invalid_redis_job(
            connection,
            worker_config,
            state,
            &record,
            "unknown job type",
            job_admin,
        )
        .await;
        return;
    };

    let max_attempts = if record.max_attempts != 0 {
        record.max_attempts
    } else if info_max_attempts != 0 {
        info_max_attempts
    } else {
        worker_config.default_attempts
    };
    let backoff_ms = if record.initial_backoff_ms != 0 {
        record.initial_backoff_ms
    } else if info_backoff_ms != 0 {
        info_backoff_ms
    } else {
        worker_config.default_backoff
    };
    record.max_attempts = max_attempts;
    record.initial_backoff_ms = backoff_ms;

    if record.attempt == 0 {
        dead_letter_invalid_redis_job(
            connection,
            worker_config,
            state,
            &record,
            "invalid job payload: attempt must be >= 1",
            job_admin,
        )
        .await;
        return;
    }

    let job_span = build_job_consumer_span(&record.name, record.attempt);
    #[cfg(feature = "telemetry-otlp")]
    if let Some(cx) =
        restore_job_trace_context(record.traceparent.as_deref(), record.tracestate.as_deref())
    {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let _ = job_span.set_parent(cx);
    }
    let final_attempt = record.attempt >= record.max_attempts;
    let f = run_job_handler(
        &record.name,
        handler,
        state.clone(),
        record.payload.clone(),
        final_attempt,
    );
    match tracing::Instrument::instrument(f, job_span).await {
        JobExecutionOutcome::Succeeded => {
            match ack_redis_success(connection, worker_config, &record).await {
                Ok(true) => {
                    state.job_registry.record_success(&record.name);
                    job_admin.record_success(&record.id);
                }
                Ok(false) => tracing::warn!(
                    job = %record.name,
                    job_id = %record.id,
                    "redis job success ack skipped because claim changed"
                ),
                Err(error) => tracing::warn!(
                    job = %record.name,
                    job_id = %record.id,
                    error = %error,
                    "redis job success ack failed"
                ),
            }
        }
        JobExecutionOutcome::Failed(error) => {
            settle_failed_redis_job(
                connection,
                worker_config,
                state,
                &record,
                error,
                "failed",
                job_admin,
            )
            .await;
        }
        JobExecutionOutcome::Panicked(error) => {
            tracing::error!(job = %record.name, error = %error, "redis job handler panicked");
            dead_letter_panicked_redis_job(
                connection,
                worker_config,
                state,
                &record,
                error,
                job_admin,
            )
            .await;
        }
    }
}

#[cfg(feature = "redis")]
#[allow(clippy::too_many_lines)]
fn start_redis_runtime(
    jobs: Vec<JobInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    config: &crate::config::JobConfig,
) -> Result<(), AutumnError> {
    let job_admin = JobAdminMemoryBackend::new();
    let url = config
        .redis
        .url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            AutumnError::internal_server_error(std::io::Error::other(
                "jobs.backend=redis requires jobs.redis.url",
            ))
        })?;

    let client = redis::Client::open(url).map_err(|e| {
        AutumnError::internal_server_error(std::io::Error::other(format!(
            "invalid jobs redis url: {e}"
        )))
    })?;
    let producer_connection =
        new_redis_connection_manager(&client, "jobs redis connection manager")?;
    let admin_connection =
        new_redis_connection_manager(&client, "jobs redis admin connection manager")?;

    let queue_key = format!("{}:queue", config.redis.key_prefix);
    let processing_key = format!("{}:processing", config.redis.key_prefix);
    let delayed_key = format!("{}:delayed", config.redis.key_prefix);
    let dead_key = format!("{}:dead", config.redis.key_prefix);
    let completed_key = format!("{}:completed", config.redis.key_prefix);
    let blocked_key = format!("{}:blocked", config.redis.key_prefix);
    let record_prefix = format!("{}:record:", config.redis.key_prefix);
    let dead_record_prefix = format!("{}:dead-record:", config.redis.key_prefix);
    let unique_prefix = format!("{}:unique:", config.redis.key_prefix);
    let concurrency_prefix = format!("{}:concurrency:", config.redis.key_prefix);

    // Build the priority drain schedule; declared-but-unconfigured queues are
    // appended at lowest priority (warned about) so jobs never silently stall.
    let declared_queues = {
        let mut seen = std::collections::HashSet::new();
        let mut queues = Vec::new();
        for job in &jobs {
            let name = normalize_queue_name(&job.queue);
            if seen.insert(name.clone()) {
                queues.push(name);
            }
        }
        queues
    };
    let (schedule, unconfigured) = QueueSchedule::effective(&config.queues, &declared_queues);
    for queue in &unconfigured {
        tracing::warn!(
            queue = %queue,
            "job declares queue '{queue}' which is not in [jobs] queues; draining it at \
             lowest priority. Add it to the configured queue list to control its priority.",
        );
    }
    let admin_queue_keys: Vec<String> = schedule
        .names()
        .iter()
        .map(|queue| redis_queue_key(&config.redis.key_prefix, queue))
        .collect();

    if job_admin_backend(state).is_none() {
        state.insert_extension(JobAdminBackendEntry(Arc::new(RedisJobAdminBackend::new(
            admin_connection,
            admin_queue_keys,
            config.redis.key_prefix.clone(),
            delayed_key.clone(),
            processing_key.clone(),
            dead_key.clone(),
            completed_key.clone(),
            blocked_key.clone(),
            record_prefix.clone(),
            dead_record_prefix.clone(),
            unique_prefix.clone(),
            DEFAULT_JOB_ADMIN_HISTORY_LIMIT,
            state.job_registry.clone(),
        ))));
    }

    let per_job_settings = build_per_job_settings(&jobs);
    let retry_promotion_interval = std::time::Duration::from_millis(
        redis_retry_promotion_interval_ms(config.initial_backoff_ms, &jobs),
    );
    let jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>> = Arc::new(RwLock::new(
        jobs.into_iter().map(|j| (j.name.clone(), j)).collect(),
    ));

    {
        let guard = jobs_by_name.read().expect("job registry lock poisoned");
        for name in guard.keys() {
            state.job_registry.register(name);
        }
    }

    install_job_client(
        state,
        JobClient {
            local_sender: None,
            local_coordination: None,
            redis: Some(RedisClient {
                connection: producer_connection,
                key_prefix: config.redis.key_prefix.clone(),
                delayed_key: delayed_key.clone(),
                record_prefix: record_prefix.clone(),
                unique_prefix: unique_prefix.clone(),
            }),
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: state.job_registry.clone(),
            job_admin: job_admin.clone(),
            default_max_attempts: config.max_attempts,
            default_initial_backoff_ms: config.initial_backoff_ms,
            per_job_settings,
            interceptor: state
                .extension::<Arc<dyn crate::interceptor::JobInterceptor>>()
                .map(|arc| (*arc).clone()),
            resilience_config: state
                .extension::<crate::config::AutumnConfig>()
                .map(|c| Arc::new(c.resilience.clone())),
        },
    );

    let worker_count = config.workers.max(1);
    for _ in 0..worker_count {
        spawn_redis_worker(
            &client,
            Arc::clone(&jobs_by_name),
            state.clone(),
            job_admin.clone(),
            shutdown.clone(),
            RedisWorkerConfig {
                queue_key: queue_key.clone(),
                key_prefix: config.redis.key_prefix.clone(),
                schedule: schedule.clone(),
                processing_key: processing_key.clone(),
                delayed_key: delayed_key.clone(),
                dead_key: dead_key.clone(),
                completed_key: completed_key.clone(),
                blocked_key: blocked_key.clone(),
                record_prefix: record_prefix.clone(),
                dead_record_prefix: dead_record_prefix.clone(),
                unique_prefix: unique_prefix.clone(),
                concurrency_prefix: concurrency_prefix.clone(),
                worker_id: format!("{}:{}", std::process::id(), uuid::Uuid::new_v4()),
                visibility_timeout_ms: config.redis.visibility_timeout_ms,
                default_attempts: config.max_attempts,
                default_backoff: config.initial_backoff_ms,
                retry_promotion_interval,
            },
        )?;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Postgres job backend (feature = "db")
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "db")]
type PgPool = diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>;

#[cfg(feature = "db")]
const PG_STATUS_ENQUEUED: &str = "enqueued";
#[cfg(feature = "db")]
const PG_STATUS_RUNNING: &str = "running";
#[cfg(feature = "db")]
const PG_STATUS_COMPLETED: &str = "completed";
#[cfg(feature = "db")]
const PG_STATUS_FAILED: &str = "failed";

#[cfg(feature = "db")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PgLifecycleRecord<'a> {
    Success,
    Retry { error: &'a str, attempt: u32 },
    Failure { error: &'a str },
}

#[cfg(feature = "db")]
const fn pg_claim_transition_applied(rows_affected: usize) -> bool {
    rows_affected > 0
}

#[cfg(feature = "db")]
fn record_pg_lifecycle_after_ack(
    ack_applied: bool,
    job_name: &str,
    job_id: &str,
    lifecycle: PgLifecycleRecord<'_>,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
) -> bool {
    if !ack_applied {
        // The claim was evicted by stale-claim recovery before this ack ran.
        // The recovery task already transitioned the row in the database:
        // - non-terminal attempts are requeued (attempt < max_attempts)
        // - terminal attempts are dead-lettered (attempt >= max_attempts)
        // Mirror whichever outcome the worker intended so /actuator metrics stay
        // consistent with the database row.
        if let PgLifecycleRecord::Failure { error } = lifecycle {
            // Stale recovery dead-lettered the row (final attempt).
            state
                .job_registry
                .record_failure(job_name, error.to_owned(), true);
            job_admin.record_failure(job_id, error.to_owned());
        } else {
            // Non-terminal or successful outcome: decrement in_flight and
            // mark as retrying; the row is already back in the queue.
            state
                .job_registry
                .record_retry(job_name, "visibility timeout expired", 0);
            job_admin.record_retrying(job_id, "visibility timeout expired");
        }
        return false;
    }

    match lifecycle {
        PgLifecycleRecord::Success => {
            state.job_registry.record_success(job_name);
            job_admin.record_success(job_id);
        }
        PgLifecycleRecord::Retry { error, attempt } => {
            state.job_registry.record_retry(job_name, error, attempt);
            job_admin.record_retrying(job_id, error);
            // The row is back in autumn_jobs with status='enqueued'; reflect
            // that in the process-local counters so /actuator shows it as queued.
            state.job_registry.record_enqueue(job_name);
            job_admin.record_requeued(job_id, attempt + 1);
        }
        PgLifecycleRecord::Failure { error } => {
            state
                .job_registry
                .record_failure(job_name, error.to_owned(), true);
            job_admin.record_failure(job_id, error.to_owned());
        }
    }

    true
}

#[cfg(feature = "db")]
fn record_pg_lifecycle_ack_result(
    ack_result: AutumnResult<bool>,
    job_name: &str,
    job_id: &str,
    outcome: &str,
    lifecycle: PgLifecycleRecord<'_>,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
) -> bool {
    match ack_result {
        Ok(applied) => {
            let recorded = record_pg_lifecycle_after_ack(
                applied, job_name, job_id, lifecycle, state, job_admin,
            );
            if !recorded {
                tracing::warn!(
                    job = %job_name,
                    job_id = %job_id,
                    outcome = %outcome,
                    "postgres job ack skipped because claim changed"
                );
            }
            recorded
        }
        Err(error) => {
            tracing::warn!(
                job = %job_name,
                job_id = %job_id,
                outcome = %outcome,
                error = %error,
                "postgres job ack failed"
            );
            false
        }
    }
}

#[cfg(feature = "db")]
fn record_pg_row_lifecycle_ack_result(
    ack_result: AutumnResult<bool>,
    row: &PgJobRow,
    outcome: &str,
    lifecycle: PgLifecycleRecord<'_>,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
) -> bool {
    record_pg_lifecycle_ack_result(
        ack_result, &row.name, &row.id, outcome, lifecycle, state, job_admin,
    )
}

#[cfg(feature = "db")]
fn record_pg_cancel_after_ack(
    ack_result: AutumnResult<bool>,
    job_name: &str,
    job_id: &str,
    state: &AppState,
) -> bool {
    match ack_result {
        Ok(true) => {
            state.job_registry.record_cancel(job_name);
            true
        }
        Ok(false) => {
            tracing::warn!(
                job = %job_name,
                job_id = %job_id,
                "postgres job cancel ack skipped because claim changed"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                job = %job_name,
                job_id = %job_id,
                error = %error,
                "postgres job cancel ack failed"
            );
            false
        }
    }
}

#[cfg(feature = "db")]
fn record_pg_row_cancel_after_ack(
    ack_result: AutumnResult<bool>,
    row: &PgJobRow,
    state: &AppState,
) -> bool {
    record_pg_cancel_after_ack(ack_result, &row.name, &row.id, state)
}

#[cfg(feature = "db")]
const PG_WORKER_IDLE_SLEEP: std::time::Duration = std::time::Duration::from_millis(200);
#[cfg(feature = "db")]
const PG_MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Columns returned by every SELECT from `autumn_jobs` when OTLP is disabled.
#[cfg(all(feature = "db", not(feature = "telemetry-otlp")))]
const PG_JOB_SELECT_COLS: &str = "id, name, queue, payload::TEXT AS payload, status, attempt, \
    max_attempts, initial_backoff_ms, enqueued_at, run_at, started_at, finished_at, \
    claimed_by, claimed_at, last_error";

/// Columns returned by every SELECT from `autumn_jobs` when OTLP is enabled.
/// Includes the nullable `traceparent` and `tracestate` columns added by the
/// `add_trace_context_to_jobs` migration.
#[cfg(all(feature = "db", feature = "telemetry-otlp"))]
const PG_JOB_SELECT_COLS: &str = "id, name, queue, payload::TEXT AS payload, status, attempt, \
    max_attempts, initial_backoff_ms, enqueued_at, run_at, started_at, finished_at, \
    claimed_by, claimed_at, last_error, traceparent, tracestate";

/// A job row read from the `autumn_jobs` Postgres table.
#[cfg(feature = "db")]
#[derive(diesel::QueryableByName, Debug, Clone)]
#[allow(dead_code)]
struct PgJobRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    queue: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    payload: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    attempt: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    max_attempts: i32,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    initial_backoff_ms: i64,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>)]
    enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>)]
    run_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>)]
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>)]
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    claimed_by: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>)]
    claimed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    last_error: Option<String>,
    /// W3C `traceparent` captured at enqueue time.
    #[cfg(feature = "telemetry-otlp")]
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    traceparent: Option<String>,
    /// W3C `tracestate` captured at enqueue time.
    #[cfg(feature = "telemetry-otlp")]
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    tracestate: Option<String>,
}

#[cfg(feature = "db")]
impl PgJobRow {
    fn to_admin_record(&self, status: JobAdminStatus) -> JobAdminRecord {
        let payload = serde_json::from_str::<Value>(&self.payload).unwrap_or(Value::Null);
        let (principal_id, correlation_id) = job_payload_identity(&payload);
        JobAdminRecord {
            id: self.id.clone(),
            name: self.name.clone(),
            queue: normalize_queue_name(&self.queue),
            status,
            enqueued_at: self.enqueued_at.map(format_job_admin_time),
            scheduled_for: if status == JobAdminStatus::Scheduled {
                self.run_at.map(format_job_admin_time)
            } else {
                None
            },
            started_at: self.started_at.map(format_job_admin_time),
            finished_at: self.finished_at.map(format_job_admin_time),
            attempt: u32::try_from(self.attempt).unwrap_or(0),
            max_attempts: u32::try_from(self.max_attempts).unwrap_or(1),
            last_error: self.last_error.clone(),
            principal_id,
            correlation_id,
        }
    }
}

/// A simple count row for admin queries.
#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct PgCount {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct PgEnqueuedCounts {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    enqueued_count: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    scheduled_count: i64,
}

/// Exponential backoff delay in ms for attempt `attempt` (1-indexed).
#[cfg(feature = "db")]
fn pg_retry_delay_ms(initial_backoff_ms: i64, attempt: i32) -> i64 {
    let exp = u32::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    initial_backoff_ms.saturating_mul(2_i64.saturating_pow(exp))
}

#[cfg(feature = "db")]
async fn pg_evict_expired_unique_key(
    conn: &mut diesel_async::AsyncPgConnection,
    name: &str,
    key: &str,
    ttl_ms: i64,
) {
    use diesel_async::RunQueryDsl as _;
    let _ = diesel::sql_query(
        "UPDATE autumn_jobs \
         SET unique_key = NULL \
         WHERE name = $1 AND unique_key = $2 \
           AND unique_window = 'ttl' \
           AND enqueued_at <= NOW() - ($3::BIGINT * INTERVAL '1 millisecond') \
           AND status IN ('enqueued', 'running')",
    )
    .bind::<diesel::sql_types::Text, _>(name)
    .bind::<diesel::sql_types::Text, _>(key)
    .bind::<diesel::sql_types::BigInt, _>(ttl_ms)
    .execute(conn)
    .await;
}

/// Shared INSERT for new job rows, with uniqueness dedup applied in SQL.
///
/// The `WHERE ... NOT EXISTS` guard handles the common dedup paths (an
/// in-flight twin, or — for TTL windows — any twin enqueued within the
/// window), and the `ON CONFLICT DO NOTHING` against the partial unique index
/// `idx_autumn_jobs_unique_inflight` closes the race where two app instances
/// pass the guard simultaneously. Zero rows inserted for a unique job means
/// the enqueue was coalesced.
#[cfg(feature = "db")]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn pg_insert_job(
    conn: &mut diesel_async::AsyncPgConnection,
    id: String,
    name: &str,
    queue: &str,
    payload: Value,
    max_attempts: u32,
    initial_backoff_ms: u64,
    run_at: Option<chrono::DateTime<chrono::Utc>>,
    constraints: &ResolvedJobConstraints,
) -> AutumnResult<EnqueueOutcome> {
    use diesel_async::RunQueryDsl as _;

    // For TTL-window jobs we check ONLY the time window so a long-running job
    // that outlives its TTL does not block replacement enqueues.  For all other
    // windows (pending, running) we check status instead.
    const DEDUP_GUARD: &str = "($6::TEXT IS NULL OR NOT EXISTS ( \
           SELECT 1 FROM autumn_jobs dup \
           WHERE dup.name = $2 AND dup.unique_key = $6 \
             AND CASE WHEN $8::BIGINT IS NOT NULL \
                      THEN dup.enqueued_at > NOW() - ($8::BIGINT * INTERVAL '1 millisecond') \
                      ELSE dup.status IN ('enqueued', 'running') \
                 END \
         ))";
    const UNIQUE_CONFLICT: &str = "ON CONFLICT (name, unique_key) \
         WHERE unique_key IS NOT NULL AND status IN ('enqueued', 'running') DO NOTHING";

    let queue = normalize_queue_name(queue);
    #[cfg(feature = "telemetry-otlp")]
    let (traceparent, tracestate) = capture_job_trace_context();
    let payload_str = serde_json::to_string(&payload).map_err(|e| {
        AutumnError::internal_server_error_msg(format!("serialize job payload: {e}"))
    })?;
    let unique_ttl_ms = match constraints.unique_window {
        Some(JobUniquenessWindow::TtlMs(ms)) => Some(i64::try_from(ms).unwrap_or(i64::MAX)),
        _ => None,
    };
    let has_unique_key = constraints.unique_key.is_some();
    let concurrency_limit = constraints
        .concurrency_limit
        .map(|limit| i32::try_from(limit).unwrap_or(i32::MAX));
    // Scope the concurrency key to a canonical value only when a limit is set;
    // an unscoped limit shares one pool per job name (NULL concurrency_key).
    let concurrency_key = if constraints.concurrency_limit.is_some() {
        constraints.concurrency_scope.clone()
    } else {
        None
    };

    // For TTL-window jobs, evict any expired unique holds before the INSERT.
    // Without this, a long-running job whose TTL has elapsed would still occupy
    // the partial unique index (idx_autumn_jobs_unique_inflight) and cause the
    // ON CONFLICT DO NOTHING to silently drop a legitimate replacement enqueue.
    if let (Some(ttl), Some(key)) = (unique_ttl_ms, &constraints.unique_key) {
        pg_evict_expired_unique_key(conn, name, key.as_str(), ttl).await;
    }

    #[cfg(not(feature = "telemetry-otlp"))]
    let query = diesel::sql_query(format!(
        "INSERT INTO autumn_jobs \
         (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
          enqueued_at, run_at, unique_key, unique_window, concurrency_key, concurrency_limit) \
         SELECT $1, $2, $12, $3::JSONB, 'enqueued', 1, $4, $5, NOW(), COALESCE($11, NOW()), $6, $7, $9, $10 \
         WHERE {DEDUP_GUARD} \
         {UNIQUE_CONFLICT}"
    ))
    .bind::<diesel::sql_types::Text, _>(id)
    .bind::<diesel::sql_types::Text, _>(name)
    .bind::<diesel::sql_types::Text, _>(payload_str)
    .bind::<diesel::sql_types::Integer, _>(i32::try_from(max_attempts).unwrap_or(i32::MAX))
    .bind::<diesel::sql_types::BigInt, _>(i64::try_from(initial_backoff_ms).unwrap_or(i64::MAX))
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(constraints.unique_key.clone())
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
        constraints.unique_window_tag().map(str::to_owned),
    )
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(unique_ttl_ms)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(concurrency_key)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Integer>, _>(concurrency_limit)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>, _>(run_at)
    .bind::<diesel::sql_types::Text, _>(queue.clone());
    #[cfg(feature = "telemetry-otlp")]
    let query = diesel::sql_query(format!(
        "INSERT INTO autumn_jobs \
         (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
          enqueued_at, run_at, unique_key, unique_window, concurrency_key, concurrency_limit, \
          traceparent, tracestate) \
         SELECT $1, $2, $14, $3::JSONB, 'enqueued', 1, $4, $5, NOW(), COALESCE($13, NOW()), $6, $7, $9, $10, $11, $12 \
         WHERE {DEDUP_GUARD} \
         {UNIQUE_CONFLICT}"
    ))
    .bind::<diesel::sql_types::Text, _>(id)
    .bind::<diesel::sql_types::Text, _>(name)
    .bind::<diesel::sql_types::Text, _>(payload_str)
    .bind::<diesel::sql_types::Integer, _>(i32::try_from(max_attempts).unwrap_or(i32::MAX))
    .bind::<diesel::sql_types::BigInt, _>(i64::try_from(initial_backoff_ms).unwrap_or(i64::MAX))
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(constraints.unique_key.clone())
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
        constraints.unique_window_tag().map(str::to_owned),
    )
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(unique_ttl_ms)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(concurrency_key)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Integer>, _>(concurrency_limit)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(traceparent)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(tracestate)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Timestamptz>, _>(run_at)
    .bind::<diesel::sql_types::Text, _>(queue.clone());

    let inserted = query.execute(conn).await.map_err(|e| {
        AutumnError::internal_server_error_msg(format!("pg job enqueue failed: {e}"))
    })?;
    if inserted == 0 && has_unique_key {
        return Ok(EnqueueOutcome::Deduplicated);
    }
    Ok(EnqueueOutcome::Queued)
}

/// Insert a new job row into `autumn_jobs` for immediate execution.
///
/// Thin wrapper over [`pg_enqueue_job_at`] with no delay; retained for the
/// Postgres backend's test suite, which exercises the immediate path directly.
#[cfg(all(feature = "db", test))]
#[allow(clippy::too_many_arguments)]
async fn pg_enqueue_job(
    pool: &PgPool,
    id: String,
    name: &str,
    queue: &str,
    payload: Value,
    max_attempts: u32,
    initial_backoff_ms: u64,
    constraints: &ResolvedJobConstraints,
) -> AutumnResult<EnqueueOutcome> {
    pg_enqueue_job_at(
        pool,
        id,
        name,
        queue,
        payload,
        max_attempts,
        initial_backoff_ms,
        None,
        constraints,
    )
    .await
}

/// Insert a new job row into `autumn_jobs` with an explicit `run_at` due time.
///
/// When `run_at` is in the future the row is durable but invisible to the claim
/// query (`WHERE run_at <= NOW()`) until then — a crash-safe delayed enqueue.
#[cfg(feature = "db")]
#[allow(clippy::too_many_arguments)]
async fn pg_enqueue_job_at(
    pool: &PgPool,
    id: String,
    name: &str,
    queue: &str,
    payload: Value,
    max_attempts: u32,
    initial_backoff_ms: u64,
    run_at: Option<chrono::DateTime<chrono::Utc>>,
    constraints: &ResolvedJobConstraints,
) -> AutumnResult<EnqueueOutcome> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg job pool error: {e}")))?;
    pg_insert_job(
        &mut conn,
        id,
        name,
        queue,
        payload,
        max_attempts,
        initial_backoff_ms,
        run_at,
        constraints,
    )
    .await
}

/// Insert a job into `autumn_jobs` using an **already-open connection**.
///
/// Unlike [`pg_enqueue_job_at`], this function does not acquire a new connection
/// from the pool. The INSERT participates in whatever transaction the caller
/// has open, so if the caller rolls back, the job row disappears atomically.
/// When `run_at` is in the future the row is also a crash-safe delayed enqueue:
/// invisible to workers until **both** the transaction commits **and** the due
/// time passes.
#[cfg(feature = "db")]
#[allow(clippy::too_many_arguments)]
async fn pg_enqueue_on_conn_at(
    conn: &mut diesel_async::AsyncPgConnection,
    id: String,
    name: &str,
    queue: &str,
    payload: Value,
    max_attempts: u32,
    initial_backoff_ms: u64,
    run_at: Option<chrono::DateTime<chrono::Utc>>,
    constraints: &ResolvedJobConstraints,
) -> AutumnResult<EnqueueOutcome> {
    pg_insert_job(
        conn,
        id,
        name,
        queue,
        payload,
        max_attempts,
        initial_backoff_ms,
        run_at,
        constraints,
    )
    .await
}

/// Atomically claim the next ready job with `SELECT … FOR UPDATE SKIP LOCKED`.
///
/// Returns `None` if the queue is empty or all ready rows are locked by
/// competing workers.
/// Advisory lock key serializing claims when concurrency-limited jobs exist.
///
/// The claim query counts running jobs per concurrency group; without
/// serialization two workers could both observe a free slot and exceed the
/// cap. The lock is transaction-scoped and only taken when at least one
/// registered job declares a concurrency limit, so unconstrained deployments
/// keep fully parallel claims.
#[cfg(feature = "db")]
const PG_CLAIM_ADVISORY_LOCK_KEY: i64 = 0x6175_7475_6d6e_6a62; // "autumnjb"

#[cfg(feature = "db")]
fn pg_claim_sql() -> String {
    // `$2` is the worker's ordered queue list for this claim. Restricting to it
    // and ordering by `array_position` drains higher-priority queues first;
    // passing a per-iteration rotation of the list yields weighted draining.
    // A single-queue (`['default']`) app orders only by `run_at` — today's behavior.
    format!(
        "UPDATE autumn_jobs \
         SET status = 'running', started_at = NOW(), claimed_by = $1, claimed_at = NOW(), \
             pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END, \
             unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END \
         WHERE id = ( \
           SELECT candidate.id FROM autumn_jobs candidate \
           WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW() \
             AND candidate.queue = ANY($2) \
             AND (candidate.concurrency_limit IS NULL OR ( \
               SELECT COUNT(*) FROM autumn_jobs running \
               WHERE running.status = 'running' \
                 AND running.name = candidate.name \
                 AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key \
             ) < candidate.concurrency_limit) \
           ORDER BY array_position($2::text[], candidate.queue), candidate.run_at ASC \
           LIMIT 1 \
           FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING {PG_JOB_SELECT_COLS}"
    )
}

#[cfg(feature = "db")]
async fn pg_claim_next_job(
    pool: &PgPool,
    worker_id: &str,
    serialize_claims: bool,
    queue_order: &[String],
) -> Option<PgJobRow> {
    use diesel::OptionalExtension as _;
    use diesel_async::{AsyncConnection as _, RunQueryDsl as _};
    use scoped_futures::ScopedFutureExt as _;

    let mut conn = pool.get().await.ok()?;
    let sql = pg_claim_sql();
    let queue_order = queue_order.to_vec();
    let claimed = if serialize_claims {
        let worker_id = worker_id.to_owned();
        conn.transaction::<Option<PgJobRow>, diesel::result::Error, _>(move |conn| {
            async move {
                diesel::sql_query("SELECT pg_advisory_xact_lock($1)")
                    .bind::<diesel::sql_types::BigInt, _>(PG_CLAIM_ADVISORY_LOCK_KEY)
                    .execute(conn)
                    .await?;
                diesel::sql_query(sql)
                    .bind::<diesel::sql_types::Text, _>(worker_id)
                    .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(queue_order)
                    .get_result::<PgJobRow>(conn)
                    .await
                    .optional()
            }
            .scope_boxed()
        })
        .await
    } else {
        diesel::sql_query(sql)
            .bind::<diesel::sql_types::Text, _>(worker_id)
            .bind::<diesel::sql_types::Array<diesel::sql_types::Text>, _>(queue_order)
            .get_result::<PgJobRow>(&mut *conn)
            .await
            .optional()
    };
    claimed.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "postgres job claim query failed");
        None
    })
}

/// Row shape for per-name aggregate count queries.
#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct PgNameCount {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

/// Survey enqueued jobs whose concurrency group is saturated and publish the
/// per-name counts as `blocked_on_concurrency` gauges.
#[cfg(feature = "db")]
async fn pg_update_concurrency_blocked_gauges(pool: &PgPool, state: &AppState) {
    use diesel_async::RunQueryDsl as _;

    let Ok(mut conn) = pool.get().await else {
        return;
    };
    let rows = diesel::sql_query(
        "SELECT blocked.name AS name, COUNT(*) AS count \
         FROM autumn_jobs blocked \
         WHERE blocked.status = 'enqueued' \
           AND blocked.run_at <= NOW() \
           AND blocked.concurrency_limit IS NOT NULL \
           AND ( \
             SELECT COUNT(*) FROM autumn_jobs running \
             WHERE running.status = 'running' \
               AND running.name = blocked.name \
               AND running.concurrency_key IS NOT DISTINCT FROM blocked.concurrency_key \
           ) >= blocked.concurrency_limit \
         GROUP BY blocked.name",
    )
    .load::<PgNameCount>(&mut *conn)
    .await;
    match rows {
        Ok(rows) => {
            let counts: HashMap<String, u64> = rows
                .into_iter()
                .map(|row| (row.name, u64::try_from(row.count).unwrap_or(0)))
                .collect();
            state.job_registry.set_concurrency_blocked_counts(&counts);
        }
        Err(error) => {
            tracing::warn!(error = %error, "postgres blocked-concurrency survey failed");
        }
    }
}

/// Mark a running job as completed.
#[cfg(feature = "db")]
async fn pg_ack_success(pool: &PgPool, job_id: &str, worker_id: &str) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg pool error: {e}")))?;
    diesel::sql_query(
        "UPDATE autumn_jobs \
         SET status = 'completed', finished_at = NOW(), \
             claimed_by = NULL, claimed_at = NULL, last_error = NULL \
         WHERE id = $1 AND claimed_by = $2 AND status = 'running'",
    )
    .bind::<diesel::sql_types::Text, _>(job_id)
    .bind::<diesel::sql_types::Text, _>(worker_id)
    .execute(&mut *conn)
    .await
    .map(pg_claim_transition_applied)
    .map_err(|e| AutumnError::internal_server_error_msg(format!("pg job ack failed: {e}")))
}

/// Handle a job failure: schedule a retry with exponential backoff or dead-letter.
#[cfg(feature = "db")]
async fn pg_nack_failure(
    pool: &PgPool,
    job_id: &str,
    worker_id: &str,
    error: &str,
    row: &PgJobRow,
    pending_unique_key: Option<&str>,
) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg pool error: {e}")))?;

    if row.attempt < row.max_attempts {
        let delay_ms = pg_retry_delay_ms(row.initial_backoff_ms, row.attempt);
        // Re-enqueue and restore the pending-window unique key atomically in one
        // UPDATE to eliminate the window where status='enqueued' and
        // unique_key=NULL co-exist, which would let a concurrent enqueue bypass
        // the dedup index.  The CASE subquery checks for an already-committed
        // duplicate; pending-window jobs keep NULL if a duplicate won the race,
        // while running/ttl-window jobs keep their existing key (unique_key was
        // never cleared at claim time, so $5 is NULL and the ELSE branch applies).
        let applied = diesel::sql_query(
            "UPDATE autumn_jobs \
             SET status = 'enqueued', \
                 attempt = attempt + 1, \
                 run_at = NOW() + ($1::BIGINT * INTERVAL '1 millisecond'), \
                 started_at = NULL, \
                 finished_at = NULL, \
                 claimed_by = NULL, \
                 claimed_at = NULL, \
                 last_error = $2, \
                 unique_key = CASE \
                   WHEN $5::TEXT IS NOT NULL \
                        AND NOT EXISTS ( \
                            SELECT 1 FROM autumn_jobs dup \
                            WHERE dup.name = autumn_jobs.name \
                              AND dup.unique_key = $5::TEXT \
                              AND dup.id != autumn_jobs.id \
                              AND dup.status IN ('enqueued', 'running') \
                        ) \
                   THEN $5::TEXT \
                   ELSE unique_key \
                   END, \
                 pending_unique_key = NULL \
             WHERE id = $3 AND claimed_by = $4 AND status = 'running'",
        )
        .bind::<diesel::sql_types::BigInt, _>(delay_ms)
        .bind::<diesel::sql_types::Text, _>(error)
        .bind::<diesel::sql_types::Text, _>(job_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(pending_unique_key)
        .execute(&mut *conn)
        .await
        .map(pg_claim_transition_applied)
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg job retry failed: {e}")))?;
        Ok(applied)
    } else {
        diesel::sql_query(
            "UPDATE autumn_jobs \
             SET status = 'failed', \
                 finished_at = NOW(), \
                 claimed_by = NULL, \
                 claimed_at = NULL, \
                 last_error = $1 \
             WHERE id = $2 AND claimed_by = $3 AND status = 'running'",
        )
        .bind::<diesel::sql_types::Text, _>(error)
        .bind::<diesel::sql_types::Text, _>(job_id)
        .bind::<diesel::sql_types::Text, _>(worker_id)
        .execute(&mut *conn)
        .await
        .map(pg_claim_transition_applied)
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!("pg job dead-letter failed: {e}"))
        })
    }
}

/// Dead-letter a job unconditionally, regardless of remaining attempts.
///
/// Used for panics, which are always terminal regardless of `max_attempts`.
#[cfg(feature = "db")]
async fn pg_ack_dead_letter(
    pool: &PgPool,
    job_id: &str,
    worker_id: &str,
    error: &str,
) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool
        .get()
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg pool error: {e}")))?;
    diesel::sql_query(
        "UPDATE autumn_jobs \
         SET status = 'failed', \
             finished_at = NOW(), \
             claimed_by = NULL, \
             claimed_at = NULL, \
             last_error = $1 \
         WHERE id = $2 AND claimed_by = $3 AND status = 'running'",
    )
    .bind::<diesel::sql_types::Text, _>(error)
    .bind::<diesel::sql_types::Text, _>(job_id)
    .bind::<diesel::sql_types::Text, _>(worker_id)
    .execute(&mut *conn)
    .await
    .map(pg_claim_transition_applied)
    .map_err(|e| AutumnError::internal_server_error_msg(format!("pg job dead-letter failed: {e}")))
}

/// Recover jobs whose visibility timeout has expired.
///
/// Uses a single `UPDATE … WHERE id IN (SELECT … FOR UPDATE SKIP LOCKED)` so
/// concurrent maintenance tasks from multiple replicas each recover disjoint
/// sets of stale jobs.
#[cfg(feature = "db")]
async fn pg_recover_stale_claims(pool: &PgPool, visibility_timeout_ms: u64) {
    use diesel_async::RunQueryDsl as _;

    let Ok(mut conn) = pool.get().await else {
        tracing::warn!("postgres stale-claim recovery could not acquire connection");
        return;
    };
    // Restore pending-window unique keys atomically with the status change so
    // there is no window where status='enqueued' and unique_key=NULL co-exist.
    // The CASE subquery checks for already-committed duplicates; if one exists
    // the key stays NULL (best-effort, same behaviour as pg_nack_failure).
    let _ = diesel::sql_query(
        "UPDATE autumn_jobs \
         SET \
           status = CASE \
             WHEN attempt < max_attempts THEN 'enqueued'::TEXT \
             ELSE 'failed'::TEXT \
           END, \
           attempt = CASE \
             WHEN attempt < max_attempts THEN attempt + 1 \
             ELSE attempt \
           END, \
           run_at = CASE \
             WHEN attempt < max_attempts THEN NOW() \
             ELSE run_at \
           END, \
           started_at = NULL, \
           finished_at = CASE \
             WHEN attempt >= max_attempts THEN NOW() \
             ELSE NULL \
           END, \
           claimed_by = NULL, \
           claimed_at = NULL, \
           last_error = 'visibility timeout expired', \
           unique_key = CASE \
             WHEN attempt < max_attempts \
                  AND pending_unique_key IS NOT NULL \
                  AND NOT EXISTS ( \
                    SELECT 1 FROM autumn_jobs dup \
                    WHERE dup.unique_key = autumn_jobs.pending_unique_key \
                      AND dup.name = autumn_jobs.name \
                      AND dup.id != autumn_jobs.id \
                      AND dup.status IN ('enqueued', 'running') \
                  ) \
             THEN pending_unique_key \
             ELSE unique_key \
           END, \
           pending_unique_key = CASE \
             WHEN attempt < max_attempts AND pending_unique_key IS NOT NULL \
             THEN NULL \
             ELSE pending_unique_key \
           END \
         WHERE id IN ( \
           SELECT id FROM autumn_jobs \
           WHERE status = 'running' \
             AND claimed_at < NOW() - ($1::BIGINT * INTERVAL '1 millisecond') \
           FOR UPDATE SKIP LOCKED \
           LIMIT 100 \
         )",
    )
    .bind::<diesel::sql_types::BigInt, _>(i64::try_from(visibility_timeout_ms).unwrap_or(i64::MAX))
    .execute(&mut *conn)
    .await
    .map_err(|e| tracing::warn!(error = %e, "postgres stale claim recovery failed"));
}

/// Execute one claimed job and ack/nack based on the outcome.
#[cfg(feature = "db")]
async fn pg_execute_job(
    row: PgJobRow,
    jobs_by_name: &Arc<RwLock<HashMap<String, JobInfo>>>,
    pool: &PgPool,
    worker_id: &str,
    state: &AppState,
    job_admin: &JobAdminMemoryBackend,
) {
    let attempt = u32::try_from(row.attempt).unwrap_or(0);
    let max_attempts = u32::try_from(row.max_attempts).unwrap_or(1);

    if job_admin.try_record_start(&row.id, attempt) == JobAdminStartDecision::Canceled {
        let ack =
            pg_nack_failure(pool, &row.id, worker_id, "canceled by operator", &row, None).await;
        record_pg_row_cancel_after_ack(ack, &row, state);
        return;
    }
    state.job_registry.record_start(&row.name);

    let payload = serde_json::from_str::<Value>(&row.payload).unwrap_or(Value::Null);
    let job_info_snapshot = jobs_by_name
        .read()
        .expect("job registry lock poisoned")
        .get(&row.name)
        .map(|info| (info.handler, info.uniqueness.clone()));
    let pending_unique_key = job_info_snapshot
        .as_ref()
        .and_then(|(_, uniqueness)| uniqueness.as_ref())
        .filter(|unique| unique.window == JobUniquenessWindow::Pending)
        .map(|unique| job_unique_key(unique, &payload));
    let handler_opt = job_info_snapshot.map(|(handler, _)| handler);

    let Some(handler) = handler_opt else {
        // Dead-letter immediately: no handler will ever exist on this process,
        // so requeueing (pg_nack_failure) would cause every worker to
        // repeatedly claim and discard the job until attempts are exhausted.
        let error = format!("unknown job '{}'", row.name);
        let ack = pg_ack_dead_letter(pool, &row.id, worker_id, &error).await;
        let lifecycle = PgLifecycleRecord::Failure { error: &error };
        record_pg_row_lifecycle_ack_result(ack, &row, "unknown-type", lifecycle, state, job_admin);
        return;
    };

    let job_span = build_job_consumer_span(&row.name, attempt);
    #[cfg(feature = "telemetry-otlp")]
    if let Some(cx) =
        restore_job_trace_context(row.traceparent.as_deref(), row.tracestate.as_deref())
    {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let _ = job_span.set_parent(cx);
    }
    let final_attempt = attempt >= max_attempts;
    let f = run_job_handler(&row.name, handler, state.clone(), payload, final_attempt);
    match tracing::Instrument::instrument(f, job_span).await {
        JobExecutionOutcome::Succeeded => {
            let ack = pg_ack_success(pool, &row.id, worker_id).await;
            record_pg_row_lifecycle_ack_result(
                ack,
                &row,
                "success",
                PgLifecycleRecord::Success,
                state,
                job_admin,
            );
        }
        JobExecutionOutcome::Failed(error) => {
            let lifecycle = if attempt < max_attempts {
                PgLifecycleRecord::Retry {
                    error: &error,
                    attempt,
                }
            } else {
                PgLifecycleRecord::Failure { error: &error }
            };
            let ack = pg_nack_failure(
                pool,
                &row.id,
                worker_id,
                &error,
                &row,
                pending_unique_key.as_deref(),
            )
            .await;
            record_pg_row_lifecycle_ack_result(ack, &row, "failure", lifecycle, state, job_admin);
        }
        // Panics dead-letter immediately regardless of remaining attempts,
        // matching the local and redis backend behaviour.
        JobExecutionOutcome::Panicked(error) => {
            tracing::error!(job = %row.name, error = %error, "postgres job handler panicked");
            let ack = pg_ack_dead_letter(pool, &row.id, worker_id, &error).await;
            let lifecycle = PgLifecycleRecord::Failure { error: &error };
            record_pg_row_lifecycle_ack_result(ack, &row, "panic", lifecycle, state, job_admin);
        }
    }
}

/// Dedicated maintenance task: runs stale-claim recovery on a fixed interval.
///
/// Spawned once per runtime rather than per-worker so maintenance always runs
/// even when all workers are occupied with long-running jobs.
#[cfg(feature = "db")]
async fn pg_maintenance_loop(
    pool: PgPool,
    visibility_timeout_ms: u64,
    state: AppState,
    survey_blocked: bool,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut interval = tokio::time::interval(PG_MAINTENANCE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                pg_recover_stale_claims(&pool, visibility_timeout_ms).await;
                if survey_blocked {
                    pg_update_concurrency_blocked_gauges(&pool, &state).await;
                }
            }
            () = shutdown.cancelled() => break,
        }
    }
}

#[cfg(feature = "db")]
#[allow(clippy::too_many_arguments)]
async fn pg_worker_loop(
    pool: PgPool,
    worker_id: String,
    jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>>,
    state: AppState,
    job_admin: JobAdminMemoryBackend,
    serialize_claims: bool,
    schedule: QueueSchedule,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut cursor = schedule.cursor();
    loop {
        let queue_order = cursor.next_order();
        match pg_claim_next_job(&pool, &worker_id, serialize_claims, &queue_order).await {
            Some(row) => {
                pg_execute_job(row, &jobs_by_name, &pool, &worker_id, &state, &job_admin).await;
                if shutdown.is_cancelled() {
                    break;
                }
            }
            None => {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(PG_WORKER_IDLE_SLEEP) => {}
                }
            }
        }
    }
}

/// Postgres-backed job admin dashboard.
#[cfg(feature = "db")]
#[derive(Clone)]
struct PgJobAdminBackend {
    pool: PgPool,
}

#[cfg(feature = "db")]
impl PgJobAdminBackend {
    async fn pg_snapshot(&self, query: &JobAdminQuery) -> AutumnResult<JobAdminSnapshot> {
        let mut conn = self.pool.get().await.map_err(|e| {
            AutumnError::internal_server_error_msg(format!("pg admin pool error: {e}"))
        })?;
        let per_page = i64::try_from(query.per_page.clamp(1, 100)).unwrap_or(10);
        let now = chrono::Utc::now();

        let (enqueued, scheduled) = pg_enqueued_and_scheduled_pages(
            &mut conn,
            query.enqueued_page,
            query.scheduled_page,
            per_page,
        )
        .await?;
        let running = pg_admin_page(
            &mut conn,
            PG_STATUS_RUNNING,
            "started_at",
            None,
            query.running_page,
            per_page,
        )
        .await?;
        let completed = pg_admin_page(
            &mut conn,
            PG_STATUS_COMPLETED,
            "finished_at",
            Some(now - chrono::TimeDelta::hours(24)),
            query.completed_page,
            per_page,
        )
        .await?;
        let failed = pg_admin_page(
            &mut conn,
            PG_STATUS_FAILED,
            "finished_at",
            Some(now - chrono::TimeDelta::days(7)),
            query.failed_page,
            per_page,
        )
        .await?;

        Ok(JobAdminSnapshot {
            enqueued,
            scheduled,
            running,
            completed,
            failed,
            schedules: Vec::new(),
            bounded_history_limit: DEFAULT_JOB_ADMIN_HISTORY_LIMIT,
        })
    }

    async fn pg_retry_failed(&self, id: &str) -> AutumnResult<()> {
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.pool.get().await.map_err(|e| {
            AutumnError::internal_server_error_msg(format!("pg admin pool error: {e}"))
        })?;
        let updated = diesel::sql_query(
            "UPDATE autumn_jobs \
             SET status = 'enqueued', attempt = 1, run_at = NOW(), enqueued_at = NOW(), \
                 started_at = NULL, finished_at = NULL, \
                 claimed_by = NULL, claimed_at = NULL, last_error = NULL \
             WHERE id = $1 AND status = 'failed'",
        )
        .bind::<diesel::sql_types::Text, _>(id)
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            // The retried row keeps its unique_key, so re-enqueueing while an
            // equivalent job is already in flight trips the partial unique
            // index — surface that as an operator-actionable conflict rather
            // than silently dropping uniqueness for the retried job.
            if e.to_string().contains("idx_autumn_jobs_unique_inflight") {
                AutumnError::bad_request_msg(
                    "an equivalent unique job is already pending or running; \
                     retry after it settles",
                )
            } else {
                AutumnError::internal_server_error_msg(format!("pg admin retry failed: {e}"))
            }
        })?;
        if updated == 0 {
            return Err(AutumnError::not_found_msg(format!(
                "job '{id}' not found or not in failed state"
            )));
        }
        Ok(())
    }

    async fn pg_discard_failed(&self, id: &str) -> AutumnResult<()> {
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.pool.get().await.map_err(|e| {
            AutumnError::internal_server_error_msg(format!("pg admin pool error: {e}"))
        })?;
        let updated = diesel::sql_query(
            "UPDATE autumn_jobs \
             SET status = 'discarded', finished_at = NOW() \
             WHERE id = $1 AND status = 'failed'",
        )
        .bind::<diesel::sql_types::Text, _>(id)
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!("pg admin discard failed: {e}"))
        })?;
        if updated == 0 {
            return Err(AutumnError::not_found_msg(format!(
                "job '{id}' not found or not in failed state"
            )));
        }
        Ok(())
    }

    async fn pg_cancel_enqueued(&self, id: &str) -> AutumnResult<()> {
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.pool.get().await.map_err(|e| {
            AutumnError::internal_server_error_msg(format!("pg admin pool error: {e}"))
        })?;
        let updated = diesel::sql_query(
            "UPDATE autumn_jobs \
             SET status = 'discarded', finished_at = NOW() \
             WHERE id = $1 AND status = 'enqueued'",
        )
        .bind::<diesel::sql_types::Text, _>(id)
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            AutumnError::internal_server_error_msg(format!("pg admin cancel failed: {e}"))
        })?;
        if updated == 0 {
            return Err(AutumnError::not_found_msg(format!(
                "job '{id}' not found or not in enqueued state"
            )));
        }
        Ok(())
    }
}

#[cfg(feature = "db")]
impl JobAdminBackend for PgJobAdminBackend {
    fn snapshot(&self, query: JobAdminQuery) -> JobAdminFuture<'_, JobAdminSnapshot> {
        Box::pin(async move { self.pg_snapshot(&query).await })
    }

    fn retry(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.pg_retry_failed(&id).await })
    }

    fn discard(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.pg_discard_failed(&id).await })
    }

    fn cancel(&self, id: &str) -> JobAdminFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { self.pg_cancel_enqueued(&id).await })
    }
}

/// Paginated query for one status group in the admin dashboard.
///
/// `sort_col` must be the literal column name that is indexed for this status
/// (e.g. `"enqueued_at"`, `"started_at"`, `"finished_at"`). It is a `&'static
/// str` from our own call sites — never user input — so embedding it via
/// `format!` is safe.
#[cfg(feature = "db")]
async fn pg_admin_page(
    conn: &mut diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
    status: &str,
    sort_col: &'static str,
    since: Option<chrono::DateTime<chrono::Utc>>,
    page: u64,
    per_page: i64,
) -> AutumnResult<JobAdminPage> {
    use diesel_async::RunQueryDsl as _;

    let page = page.max(1);
    let offset = i64::try_from((page - 1).saturating_mul(u64::try_from(per_page).unwrap_or(10)))
        .unwrap_or(0);
    let admin_status = match status {
        PG_STATUS_ENQUEUED => JobAdminStatus::Enqueued,
        PG_STATUS_RUNNING => JobAdminStatus::Running,
        PG_STATUS_COMPLETED => JobAdminStatus::Completed,
        _ => JobAdminStatus::Failed,
    };

    let (total, rows) = if let Some(since) = since {
        let total = diesel::sql_query(format!(
            "SELECT COUNT(*) AS count FROM autumn_jobs \
             WHERE status = $1 AND {sort_col} >= $2"
        ))
        .bind::<diesel::sql_types::Text, _>(status)
        .bind::<diesel::sql_types::Timestamptz, _>(since)
        .get_result::<PgCount>(&mut **conn)
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg admin count: {e}")))?
        .count;

        let rows = diesel::sql_query(format!(
            "SELECT {PG_JOB_SELECT_COLS} FROM autumn_jobs \
             WHERE status = $1 AND {sort_col} >= $2 \
             ORDER BY {sort_col} DESC \
             LIMIT $3 OFFSET $4"
        ))
        .bind::<diesel::sql_types::Text, _>(status)
        .bind::<diesel::sql_types::Timestamptz, _>(since)
        .bind::<diesel::sql_types::BigInt, _>(per_page)
        .bind::<diesel::sql_types::BigInt, _>(offset)
        .load::<PgJobRow>(&mut **conn)
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg admin page: {e}")))?;

        (total, rows)
    } else {
        let total =
            diesel::sql_query("SELECT COUNT(*) AS count FROM autumn_jobs WHERE status = $1")
                .bind::<diesel::sql_types::Text, _>(status)
                .get_result::<PgCount>(&mut **conn)
                .await
                .map_err(|e| {
                    AutumnError::internal_server_error_msg(format!("pg admin count: {e}"))
                })?
                .count;

        let rows = diesel::sql_query(format!(
            "SELECT {PG_JOB_SELECT_COLS} FROM autumn_jobs \
             WHERE status = $1 \
             ORDER BY {sort_col} DESC NULLS LAST \
             LIMIT $2 OFFSET $3"
        ))
        .bind::<diesel::sql_types::Text, _>(status)
        .bind::<diesel::sql_types::BigInt, _>(per_page)
        .bind::<diesel::sql_types::BigInt, _>(offset)
        .load::<PgJobRow>(&mut **conn)
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("pg admin page: {e}")))?;

        (total, rows)
    };

    let records = rows
        .iter()
        .map(|r| r.to_admin_record(admin_status))
        .collect();
    Ok(JobAdminPage::new(
        records,
        u64::try_from(total).unwrap_or(0),
        page,
        u64::try_from(per_page).unwrap_or(10),
    ))
}

/// Fetch both the ready-enqueued page and the scheduled page for the Postgres
/// admin dashboard in 3 queries (1 shared COUNT, 2 separate SELECTs) rather
/// than 4.
///
/// Ready rows surface as [`JobAdminStatus::Enqueued`] (newest-first); scheduled
/// rows surface as [`JobAdminStatus::Scheduled`] with their `run_at` due time,
/// soonest-due first.
#[cfg(feature = "db")]
async fn pg_enqueued_and_scheduled_pages(
    conn: &mut diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
    enqueued_page: u64,
    scheduled_page: u64,
    per_page: i64,
) -> AutumnResult<(JobAdminPage, JobAdminPage)> {
    use diesel_async::RunQueryDsl as _;

    // One query for both counts.
    let counts = diesel::sql_query(
        "SELECT \
           COALESCE(SUM(CASE WHEN run_at IS NULL OR run_at <= NOW() THEN 1 ELSE 0 END), 0) AS enqueued_count, \
           COALESCE(SUM(CASE WHEN run_at > NOW()  THEN 1 ELSE 0 END), 0) AS scheduled_count \
         FROM autumn_jobs WHERE status = 'enqueued'",
    )
    .get_result::<PgEnqueuedCounts>(&mut **conn)
    .await
    .map_err(|e| AutumnError::internal_server_error_msg(format!("pg admin count: {e}")))?;

    let enq_page = enqueued_page.max(1);
    let enq_offset =
        i64::try_from((enq_page - 1).saturating_mul(u64::try_from(per_page).unwrap_or(10)))
            .unwrap_or(0);
    let enqueued_rows = diesel::sql_query(format!(
        "SELECT {PG_JOB_SELECT_COLS} FROM autumn_jobs \
         WHERE status = 'enqueued' AND (run_at IS NULL OR run_at <= NOW()) \
         ORDER BY enqueued_at DESC NULLS LAST \
         LIMIT $1 OFFSET $2"
    ))
    .bind::<diesel::sql_types::BigInt, _>(per_page)
    .bind::<diesel::sql_types::BigInt, _>(enq_offset)
    .load::<PgJobRow>(&mut **conn)
    .await
    .map_err(|e| AutumnError::internal_server_error_msg(format!("pg admin page: {e}")))?;

    let sch_page = scheduled_page.max(1);
    let sch_offset =
        i64::try_from((sch_page - 1).saturating_mul(u64::try_from(per_page).unwrap_or(10)))
            .unwrap_or(0);
    let scheduled_rows = diesel::sql_query(format!(
        "SELECT {PG_JOB_SELECT_COLS} FROM autumn_jobs \
         WHERE status = 'enqueued' AND run_at > NOW() \
         ORDER BY run_at ASC \
         LIMIT $1 OFFSET $2"
    ))
    .bind::<diesel::sql_types::BigInt, _>(per_page)
    .bind::<diesel::sql_types::BigInt, _>(sch_offset)
    .load::<PgJobRow>(&mut **conn)
    .await
    .map_err(|e| AutumnError::internal_server_error_msg(format!("pg admin page: {e}")))?;

    let enqueued = JobAdminPage::new(
        enqueued_rows
            .iter()
            .map(|r| r.to_admin_record(JobAdminStatus::Enqueued))
            .collect(),
        u64::try_from(counts.enqueued_count).unwrap_or(0),
        enq_page,
        u64::try_from(per_page).unwrap_or(10),
    );
    let scheduled = JobAdminPage::new(
        scheduled_rows
            .iter()
            .map(|r| r.to_admin_record(JobAdminStatus::Scheduled))
            .collect(),
        u64::try_from(counts.scheduled_count).unwrap_or(0),
        sch_page,
        u64::try_from(per_page).unwrap_or(10),
    );
    Ok((enqueued, scheduled))
}

/// Start the Postgres job runtime.
#[cfg(feature = "db")]
fn start_postgres_runtime(
    jobs: Vec<JobInfo>,
    state: &AppState,
    shutdown: &tokio_util::sync::CancellationToken,
    config: &crate::config::JobConfig,
) -> AutumnResult<()> {
    let pool = state.pool().cloned().ok_or_else(|| {
        AutumnError::internal_server_error(std::io::Error::other(
            "jobs.backend=postgres requires a configured database; \
             set database.url or call AppBuilder::with_pool()",
        ))
    })?;

    let job_admin = JobAdminMemoryBackend::new();
    let per_job_settings = build_per_job_settings(&jobs);
    let serialize_claims = any_job_has_concurrency(&jobs);
    let jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>> = Arc::new(RwLock::new(
        jobs.into_iter().map(|j| (j.name.clone(), j)).collect(),
    ));

    {
        let guard = jobs_by_name.read().expect("job registry lock poisoned");
        for name in guard.keys() {
            state.job_registry.register(name);
        }
    }

    if job_admin_backend(state).is_none() {
        state.insert_extension(JobAdminBackendEntry(Arc::new(PgJobAdminBackend {
            pool: pool.clone(),
        })));
    }

    let (schedule, unconfigured) =
        QueueSchedule::effective(&config.queues, &collect_declared_queues(&jobs_by_name));
    for queue in &unconfigured {
        tracing::warn!(
            queue = %queue,
            "job declares queue '{queue}' which is not in [jobs] queues; draining it at \
             lowest priority. Add it to the configured queue list to control its priority.",
        );
    }

    install_job_client(
        state,
        JobClient {
            local_sender: None,
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            pg_pool: Some(pool.clone()),
            registry: state.job_registry.clone(),
            job_admin: job_admin.clone(),
            default_max_attempts: config.max_attempts,
            default_initial_backoff_ms: config.initial_backoff_ms,
            per_job_settings,
            interceptor: state
                .extension::<Arc<dyn crate::interceptor::JobInterceptor>>()
                .map(|arc| (*arc).clone()),
            resilience_config: state
                .extension::<crate::config::AutumnConfig>()
                .map(|c| Arc::new(c.resilience.clone())),
        },
    );

    let visibility_timeout_ms = config.postgres.visibility_timeout_ms;
    let worker_count = config.workers.max(1);

    // Single maintenance task shared across all workers.
    {
        let pool = pool.clone();
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            pg_maintenance_loop(
                pool,
                visibility_timeout_ms,
                state,
                serialize_claims,
                shutdown,
            )
            .await;
        });
    }

    for _ in 0..worker_count {
        let pool = pool.clone();
        let jobs_by_name = Arc::clone(&jobs_by_name);
        let state = state.clone();
        let job_admin = job_admin.clone();
        let shutdown = shutdown.clone();
        let schedule = schedule.clone();
        tokio::spawn(async move {
            let worker_id = format!("{}:{}", std::process::id(), uuid::Uuid::new_v4());
            pg_worker_loop(
                pool,
                worker_id,
                jobs_by_name,
                state,
                job_admin,
                serialize_claims,
                schedule,
                shutdown,
            )
            .await;
        });
    }

    Ok(())
}

fn build_per_job_settings(jobs: &[JobInfo]) -> HashMap<String, JobRuntimeSettings> {
    jobs.iter()
        .map(|job| {
            (
                job.name.clone(),
                JobRuntimeSettings {
                    max_attempts: job.max_attempts,
                    initial_backoff_ms: job.initial_backoff_ms,
                    queue: normalize_queue_name(&job.queue),
                    uniqueness: job.uniqueness.clone(),
                    concurrency: job.concurrency.clone(),
                },
            )
        })
        .collect()
}

/// Whether any registered job declares a concurrency limit.
#[cfg(feature = "db")]
fn any_job_has_concurrency(jobs: &[JobInfo]) -> bool {
    jobs.iter().any(|job| job.concurrency.is_some())
}

fn validate_unique_job_names(jobs: &[JobInfo]) -> Result<(), String> {
    let mut names = std::collections::HashSet::new();
    for job in jobs {
        if !names.insert(job.name.clone()) {
            return Err(format!("duplicate job name '{}'", job.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "redis")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    #[cfg(feature = "redis")]
    static REDIS_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn always_fail_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            Err(AutumnError::internal_server_error(std::io::Error::other(
                "forced failure",
            )))
        })
    }

    #[cfg(feature = "redis")]
    fn redis_counting_success_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            REDIS_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[cfg(feature = "redis")]
    fn redis_counting_failure_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            REDIS_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Err(AutumnError::internal_server_error(std::io::Error::other(
                "redis forced failure",
            )))
        })
    }

    fn panicking_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            panic!("forced panic");
        })
    }

    fn instantly_panicking_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        panic!("panic before future")
    }

    #[tokio::test]
    async fn job_admin_backend_lists_and_operates_failed_jobs() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let enqueued_id = backend.record_enqueue_for_test(
            "send_email",
            serde_json::json!({
                "user_id": 42,
                "correlation_id": "req-123",
                "subject": "Welcome"
            }),
            1,
            5,
        );
        let running_id = backend.record_enqueue_for_test("reindex", serde_json::json!({}), 1, 3);
        backend.record_start_for_test(&running_id, 1);
        let completed_id = backend.record_enqueue_for_test("digest", serde_json::json!({}), 1, 3);
        backend.record_start_for_test(&completed_id, 1);
        backend.record_success_for_test(&completed_id);
        let failed_id =
            backend.record_enqueue_for_test("send_email", serde_json::json!({"user_id": 7}), 2, 5);
        backend.record_start_for_test(&failed_id, 2);
        backend.record_failure_for_test(&failed_id, "smtp refused recipient");

        let snapshot = backend
            .snapshot(JobAdminQuery {
                enqueued_page: 1,
                scheduled_page: 1,
                running_page: 1,
                completed_page: 1,
                failed_page: 1,
                per_page: 10,
            })
            .await
            .expect("snapshot should render");

        assert_eq!(snapshot.enqueued.records[0].id, enqueued_id);
        assert_eq!(
            snapshot.enqueued.records[0].principal_id.as_deref(),
            Some("42")
        );
        assert_eq!(
            snapshot.enqueued.records[0].correlation_id.as_deref(),
            Some("req-123")
        );
        assert_eq!(snapshot.running.records[0].id, running_id);
        assert_eq!(snapshot.completed.records[0].id, completed_id);
        assert_eq!(snapshot.failed.records[0].id, failed_id);
        assert_eq!(
            snapshot.failed.records[0].last_error.as_deref(),
            Some("smtp refused recipient")
        );

        backend
            .discard(&failed_id)
            .await
            .expect("failed job should be discardable");
        backend
            .cancel(&enqueued_id)
            .await
            .expect("enqueued job should be cancelable");
        assert_eq!(
            backend.try_record_start(&enqueued_id, 1),
            JobAdminStartDecision::Canceled,
            "canceled jobs must not race into running"
        );

        let snapshot = backend
            .snapshot(JobAdminQuery::default())
            .await
            .expect("snapshot after operations");
        assert!(snapshot.failed.records.is_empty());
        assert!(snapshot.enqueued.records.is_empty());
    }

    #[tokio::test]
    async fn job_admin_retry_reenqueues_failed_payload() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let backend = JobAdminMemoryBackend::new_for_test(32);
        let (tx, mut rx) = mpsc::channel(1);
        init_global_job_client(JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: backend.clone(),
            default_max_attempts: 5,
            default_initial_backoff_ms: 250,
            per_job_settings: HashMap::from([(
                "send_email".to_string(),
                JobRuntimeSettings::basic(5, 250),
            )]),
            interceptor: None,
            resilience_config: None,
        });

        let failed_id = backend.record_enqueue_for_test(
            "send_email",
            serde_json::json!({
                "user_id": 7,
                "correlation_id": "req-retry"
            }),
            2,
            5,
        );
        backend.record_start_for_test(&failed_id, 2);
        backend.record_failure_for_test(&failed_id, "smtp refused recipient");

        backend
            .retry(&failed_id)
            .await
            .expect("failed job should be retried");
        let queued = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("retry should enqueue promptly")
            .expect("retry should enqueue a job");

        assert_eq!(queued.name, "send_email");
        assert_eq!(queued.attempt, 1);
        assert_eq!(queued.max_attempts, 5);
        assert_eq!(queued.payload["user_id"], 7);
        assert_eq!(queued.payload["correlation_id"], "req-retry");

        let snapshot = backend
            .snapshot(JobAdminQuery::default())
            .await
            .expect("snapshot after retry");
        assert!(snapshot.failed.records.is_empty());
        assert_eq!(snapshot.enqueued.total, 1);

        clear_global_job_client();
    }

    #[tokio::test]
    async fn job_admin_retry_restores_failed_record_when_enqueue_fails() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let backend = JobAdminMemoryBackend::new_for_test(32);
        let registry = crate::actuator::JobRegistry::new();
        registry.register("send_email");
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        init_global_job_client(JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: registry.clone(),
            job_admin: backend.clone(),
            default_max_attempts: 5,
            default_initial_backoff_ms: 250,
            per_job_settings: HashMap::from([(
                "send_email".to_string(),
                JobRuntimeSettings::basic(5, 250),
            )]),
            interceptor: None,
            resilience_config: None,
        });

        let failed_id =
            backend.record_enqueue_for_test("send_email", serde_json::json!({"user_id": 7}), 2, 5);
        backend.record_start_for_test(&failed_id, 2);
        backend.record_failure_for_test(&failed_id, "smtp refused recipient");

        let error = backend
            .retry(&failed_id)
            .await
            .expect_err("closed worker channel should make retry enqueue fail");
        assert!(
            error.to_string().contains("failed to enqueue job"),
            "unexpected retry error: {error}"
        );

        let snapshot = backend
            .snapshot(JobAdminQuery::default())
            .await
            .expect("snapshot after failed retry enqueue");
        assert_eq!(snapshot.failed.total, 1);
        assert_eq!(snapshot.failed.records[0].id, failed_id);
        assert_eq!(
            snapshot.failed.records[0].last_error.as_deref(),
            Some("smtp refused recipient")
        );
        assert_eq!(snapshot.enqueued.total, 0);
        let status = registry.snapshot()["send_email"].clone();
        assert_eq!(status.queued, 0);

        clear_global_job_client();
    }

    #[tokio::test]
    async fn job_admin_retry_claims_failed_record_before_enqueueing() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let backend = JobAdminMemoryBackend::new_for_test(32);
        let (tx, mut rx) = mpsc::channel(2);
        init_global_job_client(JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: backend.clone(),
            default_max_attempts: 5,
            default_initial_backoff_ms: 250,
            per_job_settings: HashMap::from([(
                "send_email".to_string(),
                JobRuntimeSettings::basic(5, 250),
            )]),
            interceptor: None,
            resilience_config: None,
        });

        let failed_id =
            backend.record_enqueue_for_test("send_email", serde_json::json!({"user_id": 7}), 2, 5);
        backend.record_start_for_test(&failed_id, 2);
        backend.record_failure_for_test(&failed_id, "smtp refused recipient");

        let (first, second) = tokio::join!(backend.retry(&failed_id), backend.retry(&failed_id));
        assert!(
            first.is_ok() ^ second.is_ok(),
            "exactly one concurrent retry should claim the failed job"
        );
        let queued = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("one retry should enqueue promptly")
            .expect("one retry should enqueue a job");
        assert_eq!(queued.name, "send_email");
        assert!(timeout(Duration::from_millis(25), rx.recv()).await.is_err());

        clear_global_job_client();
    }

    #[tokio::test]
    async fn job_admin_retry_payload_claim_is_single_use() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let failed_id =
            backend.record_enqueue_for_test("send_email", serde_json::json!({"user_id": 7}), 2, 5);
        backend.record_start_for_test(&failed_id, 2);
        backend.record_failure_for_test(&failed_id, "smtp refused recipient");

        let first = backend
            .retry_payload(&failed_id)
            .expect("first retry claim should return the payload");
        assert_eq!(first.0, "send_email");
        let second = backend
            .retry_payload(&failed_id)
            .expect_err("second retry claim must be rejected before enqueue");
        assert!(
            second
                .to_string()
                .contains("only failed jobs can be retried"),
            "unexpected second retry error: {second}"
        );
    }

    #[tokio::test]
    async fn run_job_handler_reports_immediate_panics() {
        let state = AppState::for_test().with_profile("dev");
        let outcome = run_job_handler(
            "test_job",
            instantly_panicking_handler,
            state,
            serde_json::json!({}),
            true,
        )
        .await;
        assert_eq!(
            outcome,
            JobExecutionOutcome::Panicked("job handler panicked: panic before future".to_string())
        );
    }

    #[tokio::test]
    async fn run_job_handler_catches_interceptor_setup_panics() {
        struct PanickingJobInterceptor;
        impl crate::interceptor::JobInterceptor for PanickingJobInterceptor {
            fn intercept_enqueue<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                next
            }

            fn intercept_execute<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                _next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                panic!("interceptor execution setup panicked")
            }
        }

        fn success_handler(
            _state: AppState,
            _payload: Value,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
            Box::pin(async move { Ok(()) })
        }

        let state = AppState::for_test().with_profile("dev");
        state.insert_extension(
            Arc::new(PanickingJobInterceptor) as Arc<dyn crate::interceptor::JobInterceptor>
        );

        let outcome = run_job_handler(
            "test_job",
            success_handler,
            state,
            serde_json::json!({}),
            true,
        )
        .await;

        assert_eq!(
            outcome,
            JobExecutionOutcome::Panicked(
                "job handler panicked: interceptor execution setup panicked".to_string()
            )
        );
    }

    #[tokio::test]
    async fn run_job_handler_interceptor_short_circuit_prevents_sync_execution() {
        struct ShortCircuitInterceptor;
        impl crate::interceptor::JobInterceptor for ShortCircuitInterceptor {
            fn intercept_enqueue<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                next
            }

            fn intercept_execute<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                _next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                Box::pin(async move {
                    Err(crate::AutumnError::bad_request_msg(
                        "blocked by interceptor",
                    ))
                })
            }
        }

        static SYNC_CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

        fn side_effect_handler(
            _state: AppState,
            _payload: Value,
        ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
            SYNC_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok(()) })
        }

        let state = AppState::for_test().with_profile("dev");
        state.insert_extension(
            Arc::new(ShortCircuitInterceptor) as Arc<dyn crate::interceptor::JobInterceptor>
        );

        SYNC_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);

        let outcome = run_job_handler(
            "test_job",
            side_effect_handler,
            state,
            serde_json::json!({}),
            true,
        )
        .await;

        assert_eq!(
            outcome,
            JobExecutionOutcome::Failed("blocked by interceptor".to_string())
        );

        assert_eq!(SYNC_CALLS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn job_client_enqueue_catches_interceptor_setup_panic() {
        struct PanickingEnqueueInterceptor;
        impl crate::interceptor::JobInterceptor for PanickingEnqueueInterceptor {
            fn intercept_enqueue<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                _next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                panic!("interceptor enqueue setup panicked")
            }

            fn intercept_execute<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                next
            }
        }

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let client = JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: JobAdminMemoryBackend::new_for_test(32),
            default_max_attempts: 3,
            default_initial_backoff_ms: 100,
            per_job_settings: std::collections::HashMap::from([(
                "test_job".to_string(),
                JobRuntimeSettings::basic(3, 100),
            )]),
            interceptor: Some(Arc::new(PanickingEnqueueInterceptor)),
            resilience_config: None,
        };

        let res = client.enqueue("test_job", serde_json::json!({})).await;
        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("job enqueue panicked: interceptor enqueue setup panicked"),
            "expected panic error message, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn job_client_enqueue_catches_interceptor_async_panic() {
        struct AsyncPanickingEnqueueInterceptor;
        impl crate::interceptor::JobInterceptor for AsyncPanickingEnqueueInterceptor {
            fn intercept_enqueue<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                _next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                Box::pin(async move { panic!("interceptor enqueue async panicked") })
            }

            fn intercept_execute<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                next
            }
        }

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let client = JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: JobAdminMemoryBackend::new_for_test(32),
            default_max_attempts: 3,
            default_initial_backoff_ms: 100,
            per_job_settings: std::collections::HashMap::from([(
                "test_job".to_string(),
                JobRuntimeSettings::basic(3, 100),
            )]),
            interceptor: Some(Arc::new(AsyncPanickingEnqueueInterceptor)),
            resilience_config: None,
        };

        let res = client.enqueue("test_job", serde_json::json!({})).await;
        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("job enqueue panicked: interceptor enqueue async panicked"),
            "expected panic error message, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn local_enqueue_p99_is_under_5ms() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "noop".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 10,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let mut samples = Vec::new();
        for _ in 0..300 {
            let started = std::time::Instant::now();
            enqueue("noop", serde_json::json!({})).await.unwrap();
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let p99 = samples[(samples.len() * 99) / 100];
        assert!(
            p99 < std::time::Duration::from_millis(5),
            "expected p99 enqueue latency < 5ms, got {p99:?}",
        );

        shutdown.cancel();
        clear_global_job_client();
    }

    #[tokio::test]
    async fn local_enqueue_in_delays_then_runs_through_normal_path() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "delayed".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 10,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        enqueue_in("delayed", serde_json::json!({}), Duration::from_millis(400))
            .await
            .unwrap();

        // Before the due time elapses, the job sits in the "scheduled" list and
        // must not have executed.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let admin = job_admin_backend(&state).unwrap();
        let snap = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(
            snap.scheduled.total, 1,
            "delayed job should be listed as scheduled before its due time"
        );
        assert_eq!(
            snap.completed.total, 0,
            "delayed job must not run before its due time"
        );
        assert_eq!(snap.enqueued.total, 0);

        // After the due time it runs through the normal claim/execute path.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let snap = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(
            snap.completed.total, 1,
            "delayed job should run once its due time passes"
        );
        assert_eq!(snap.scheduled.total, 0);

        shutdown.cancel();
        clear_global_job_client();
    }

    #[tokio::test]
    async fn local_enqueue_at_in_the_past_runs_immediately() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "past".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 10,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let when = chrono::Utc::now() - chrono::TimeDelta::seconds(60);
        enqueue_at("past", serde_json::json!({}), when)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let admin = job_admin_backend(&state).unwrap();
        let snap = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(
            snap.completed.total, 1,
            "a job scheduled for the past should run immediately"
        );
        assert_eq!(snap.scheduled.total, 0);

        shutdown.cancel();
        clear_global_job_client();
    }

    #[tokio::test]
    async fn local_scheduled_job_can_be_canceled_before_due() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "cancelable".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 10,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        enqueue_in(
            "cancelable",
            serde_json::json!({}),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();

        let admin = job_admin_backend(&state).unwrap();
        let snap = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(snap.scheduled.total, 1);
        let id = snap.scheduled.records[0].id.clone();
        assert!(snap.scheduled.records[0].scheduled_for.is_some());

        admin
            .cancel(&id)
            .await
            .expect("scheduled job should cancel");

        let snap = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(
            snap.scheduled.total, 0,
            "canceled scheduled job should leave the scheduled list"
        );

        shutdown.cancel();
        clear_global_job_client();
    }

    #[tokio::test]
    async fn test_interceptor_rejection_rolls_back_enqueue_bookkeeping() {
        struct RejectingInterceptor;
        impl crate::interceptor::JobInterceptor for RejectingInterceptor {
            fn intercept_enqueue<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                _next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                Box::pin(async move {
                    Err(crate::AutumnError::bad_request_msg(
                        "blocked by interceptor",
                    ))
                })
            }

            fn intercept_execute<'a>(
                &'a self,
                _name: &'a str,
                _payload: &'a serde_json::Value,
                next: std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
                >,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
            > {
                next
            }
        }

        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        state.insert_extension(
            Arc::new(RejectingInterceptor) as Arc<dyn crate::interceptor::JobInterceptor>
        );

        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "noop".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 10,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let res = enqueue("noop", serde_json::json!({})).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "blocked by interceptor");

        // The bookkeeping must be rolled back!
        let snapshot = state.job_registry().snapshot();
        assert_eq!(snapshot["noop"].queued, 0);

        let admin = job_admin_backend(&state).unwrap();
        let admin_snapshot = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(admin_snapshot.enqueued.total, 0);

        shutdown.cancel();
        clear_global_job_client();
    }

    #[tokio::test]
    async fn local_panicking_handler_records_terminal_failure_without_requeue() {
        let state = AppState::for_test().with_profile("dev");
        state.job_registry().register("panic");
        state.job_registry().record_enqueue("panic");

        let mut jobs = HashMap::new();
        jobs.insert(
            "panic".to_string(),
            JobInfo {
                name: "panic".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: panicking_handler,
            },
        );
        let jobs_by_name = Arc::new(RwLock::new(jobs));

        let (tx, mut rx) = mpsc::channel(1);
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        let job_id = job_admin.record_enqueue_for_test("panic", serde_json::json!({}), 1, 3);
        execute_local_job(
            QueuedJob {
                id: job_id,
                name: "panic".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 3,
                initial_backoff_ms: 1,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            &jobs_by_name,
            &tx,
            &state,
            &job_admin,
            &Arc::new(LocalJobCoordination::default()),
        )
        .await;

        assert!(timeout(Duration::from_millis(25), rx.recv()).await.is_err());

        let snapshot = state.job_registry().snapshot();
        let status = snapshot.get("panic").expect("job should be registered");
        assert_eq!(status.queued, 0);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.total_failures, 1);
        assert_eq!(status.dead_letters, 1);
        assert_eq!(
            status.last_error.as_deref(),
            Some("job handler panicked: forced panic")
        );
    }

    #[tokio::test]
    async fn local_retry_records_enqueue_before_requeue() {
        let state = AppState::for_test().with_profile("dev");
        state.job_registry().register("flaky");
        state.job_registry().record_enqueue("flaky");

        let mut jobs = HashMap::new();
        jobs.insert(
            "flaky".to_string(),
            JobInfo {
                name: "flaky".to_string(),
                max_attempts: 2,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: always_fail_handler,
            },
        );
        let jobs_by_name = Arc::new(RwLock::new(jobs));

        let (tx, mut rx) = mpsc::channel(1);
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        let job_id = job_admin.record_enqueue_for_test("flaky", serde_json::json!({}), 1, 2);
        execute_local_job(
            QueuedJob {
                id: job_id.clone(),
                name: "flaky".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 2,
                initial_backoff_ms: 1,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            &jobs_by_name,
            &tx,
            &state,
            &job_admin,
            &Arc::new(LocalJobCoordination::default()),
        )
        .await;

        let retried = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("retry should be scheduled")
            .expect("retry payload should be sent");
        assert_eq!(retried.id, job_id);
        assert_eq!(retried.name, "flaky");
        assert_eq!(retried.attempt, 2);

        let snapshot = state.job_registry().snapshot();
        let status = snapshot.get("flaky").expect("job should be registered");
        assert_eq!(status.queued, 1);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.total_failures, 0);
        assert!(status.last_error.is_some());
    }

    #[tokio::test]
    async fn local_terminal_failure_does_not_requeue() {
        let state = AppState::for_test().with_profile("dev");
        state.job_registry().register("flaky");
        state.job_registry().record_enqueue("flaky");

        let mut jobs = HashMap::new();
        jobs.insert(
            "flaky".to_string(),
            JobInfo {
                name: "flaky".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: always_fail_handler,
            },
        );
        let jobs_by_name = Arc::new(RwLock::new(jobs));

        let (tx, mut rx) = mpsc::channel(1);
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        let job_id = job_admin.record_enqueue_for_test("flaky", serde_json::json!({}), 1, 1);
        execute_local_job(
            QueuedJob {
                id: job_id,
                name: "flaky".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 1,
                initial_backoff_ms: 1,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            &jobs_by_name,
            &tx,
            &state,
            &job_admin,
            &Arc::new(LocalJobCoordination::default()),
        )
        .await;

        assert!(timeout(Duration::from_millis(25), rx.recv()).await.is_err());

        let snapshot = state.job_registry().snapshot();
        let status = snapshot.get("flaky").expect("job should be registered");
        assert_eq!(status.queued, 0);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.total_failures, 1);
        assert_eq!(status.dead_letters, 1);
        assert!(status.last_error.is_some());
    }

    #[tokio::test]
    async fn job_admin_local_retriable_failure_is_not_operator_retryable_failed_work() {
        let state = AppState::for_test().with_profile("dev");
        state.job_registry().register("flaky");
        state.job_registry().record_enqueue("flaky");

        let mut jobs = HashMap::new();
        jobs.insert(
            "flaky".to_string(),
            JobInfo {
                name: "flaky".to_string(),
                max_attempts: 2,
                initial_backoff_ms: 60_000,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: always_fail_handler,
            },
        );
        let jobs_by_name = Arc::new(RwLock::new(jobs));

        let (tx, mut rx) = mpsc::channel(1);
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        let job_id = job_admin.record_enqueue_for_test("flaky", serde_json::json!({}), 1, 2);
        execute_local_job(
            QueuedJob {
                id: job_id.clone(),
                name: "flaky".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 2,
                initial_backoff_ms: 60_000,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            &jobs_by_name,
            &tx,
            &state,
            &job_admin,
            &Arc::new(LocalJobCoordination::default()),
        )
        .await;

        let snapshot = job_admin
            .snapshot(JobAdminQuery::default())
            .await
            .expect("job admin snapshot");
        assert!(
            snapshot.failed.records.is_empty(),
            "automatic retries must stay out of terminal failed work"
        );
        let retry_error = job_admin
            .retry(&job_id)
            .await
            .expect_err("operator retry must reject sleeping automatic retries");
        assert!(
            retry_error
                .to_string()
                .contains("only failed jobs can be retried"),
            "unexpected retry error: {retry_error}"
        );
        assert!(timeout(Duration::from_millis(25), rx.recv()).await.is_err());
    }

    #[cfg(feature = "redis")]
    fn redis_test_record(attempt: u32, max_attempts: u32) -> RedisJobRecord {
        RedisJobRecord {
            id: "job-1".to_string(),
            name: "send_email".to_string(),
            queue: "default".to_string(),
            payload: serde_json::json!({ "user_id": 42 }),
            attempt,
            max_attempts,
            initial_backoff_ms: 250,
            enqueued_at_ms: Some(1_000),
            started_at_ms: None,
            finished_at_ms: None,
            claimed_by: None,
            claimed_at_ms: None,
            last_error: None,
            unique_key: None,
            unique_window: None,
            concurrency_key: None,
            concurrency_limit: None,
            #[cfg(feature = "telemetry-otlp")]
            traceparent: None,
            #[cfg(feature = "telemetry-otlp")]
            tracestate: None,
        }
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_claim_metadata_records_worker_and_deadline() {
        let claimed = claim_redis_record(redis_test_record(1, 3), "worker-a", 10_000, 30_000);

        assert_eq!(claimed.deadline_ms, 40_000);
        assert_eq!(claimed.record.claimed_by.as_deref(), Some("worker-a"));
        assert_eq!(claimed.record.claimed_at_ms, Some(10_000));
        assert_eq!(claimed.record.attempt, 1);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_maintenance_throttle_runs_immediately_then_waits_for_interval() {
        let start = std::time::Instant::now();
        let mut throttle = RedisMaintenanceThrottle::new(start, Duration::from_secs(1));

        assert!(throttle.take_due(start));
        assert!(!throttle.take_due(start + Duration::from_millis(999)));
        assert!(throttle.take_due(start + Duration::from_secs(1)));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_retry_promotion_interval_uses_smallest_configured_backoff() {
        let jobs = vec![
            JobInfo {
                name: "slow".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 250,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: redis_counting_success_handler,
            },
            JobInfo {
                name: "fast".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 25,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: redis_counting_success_handler,
            },
        ];

        assert_eq!(redis_retry_promotion_interval_ms(250, &jobs), 25);
        assert_eq!(redis_retry_promotion_interval_ms(0, &[]), 1);

        // Large retry backoffs must not delay one-shot delayed-job promotion.
        let slow_jobs = vec![JobInfo {
            name: "very_slow".to_string(),
            max_attempts: 3,
            initial_backoff_ms: 60_000,
            queue: "default".to_string(),
            uniqueness: None,
            concurrency: None,
            handler: redis_counting_success_handler,
        }];
        assert_eq!(
            redis_retry_promotion_interval_ms(60_000, &slow_jobs),
            REDIS_DELAYED_PROMOTION_MAX_INTERVAL_MS,
            "large backoff must be capped so delayed jobs are promoted within ~1s"
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_worker_idle_sleep_is_bounded_by_retry_promotion_interval() {
        assert_eq!(
            redis_worker_idle_sleep(Duration::from_millis(25)),
            Duration::from_millis(25)
        );
        assert_eq!(
            redis_worker_idle_sleep(Duration::from_millis(250)),
            Duration::from_millis(200)
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_failed_job_schedules_next_attempt_with_exponential_backoff() {
        let mut record = redis_test_record(2, 4);
        record.claimed_by = Some("worker-a".to_string());
        record.claimed_at_ms = Some(20_000);

        let action = prepare_redis_failure_action(record, "stripe timed out".to_string(), 50_000);

        let RedisFailureAction::Retry(schedule) = action else {
            panic!("second attempt below max should be scheduled for retry");
        };
        assert_eq!(schedule.due_at_ms, 50_500);
        assert_eq!(schedule.record.attempt, 3);
        assert_eq!(schedule.record.claimed_by, None);
        assert_eq!(schedule.record.claimed_at_ms, None);
        assert_eq!(
            schedule.record.last_error.as_deref(),
            Some("stripe timed out")
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_failed_job_dead_letters_after_max_attempts() {
        let mut record = redis_test_record(3, 3);
        record.claimed_by = Some("worker-a".to_string());
        record.claimed_at_ms = Some(20_000);

        let action = prepare_redis_failure_action(record, "permanent failure".to_string(), 50_000);

        let RedisFailureAction::DeadLetter(record) = action else {
            panic!("max attempt failure should dead-letter");
        };
        assert_eq!(record.attempt, 3);
        assert_eq!(record.claimed_by, None);
        assert_eq!(record.claimed_at_ms, None);
        assert_eq!(record.last_error.as_deref(), Some("permanent failure"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_panicking_job_dead_letters_without_retry_even_when_attempts_remain() {
        let mut record = redis_test_record(1, 3);
        record.claimed_by = Some("worker-a".to_string());
        record.claimed_at_ms = Some(20_000);

        let dead =
            prepare_redis_panic_dead_letter(record, "job handler panicked".to_string(), 50_000);

        assert_eq!(dead.attempt, 1);
        assert_eq!(dead.max_attempts, 3);
        assert_eq!(dead.claimed_by, None);
        assert_eq!(dead.claimed_at_ms, None);
        assert_eq!(dead.finished_at_ms, Some(50_000));
        assert_eq!(dead.last_error.as_deref(), Some("job handler panicked"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_stale_claim_recovery_requeues_next_attempt() {
        let mut record = redis_test_record(1, 3);
        record.claimed_by = Some("worker-a".to_string());
        record.claimed_at_ms = Some(10_000);

        let action = recover_stale_redis_record(record, 45_000, 30_000)
            .expect("expired claim should be recovered");

        let RedisStaleRecovery::Requeue(record) = action else {
            panic!("stale nonterminal claim should requeue");
        };
        assert_eq!(record.attempt, 2);
        assert_eq!(record.claimed_by, None);
        assert_eq!(record.claimed_at_ms, None);
        assert!(
            record
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("visibility timeout expired")),
            "stale recovery should record a useful last_error"
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_stale_claim_recovery_dead_letters_exhausted_job() {
        let mut record = redis_test_record(1, 1);
        record.claimed_by = Some("worker-a".to_string());
        record.claimed_at_ms = Some(10_000);

        let action = recover_stale_redis_record(record, 45_000, 30_000)
            .expect("expired claim should be recovered");

        let RedisStaleRecovery::DeadLetter(record) = action else {
            panic!("stale terminal claim should dead-letter");
        };
        assert_eq!(record.attempt, 1);
        assert_eq!(record.claimed_by, None);
        assert_eq!(record.claimed_at_ms, None);
        assert!(
            record
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("visibility timeout expired")),
            "dead-lettered stale claims should retain the recovery reason"
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_dead_letter_scripts_delete_trimmed_dead_record_metadata() {
        assert!(
            CLAIMED_REDIS_TRANSITION_SCRIPT
                .contains("trim_dead_history(KEYS[4], KEYS[6], tonumber(ARGV[7]))"),
            "claimed-job dead-letter trim should delete metadata for records beyond the history limit"
        );
        assert!(
            STALE_REDIS_RECOVERY_SCRIPT
                .contains("trim_dead_history(KEYS[4], KEYS[5], tonumber(ARGV[6]))"),
            "stale-recovery dead-letter trim should delete metadata for records beyond the history limit"
        );
        assert!(
            CLAIMED_REDIS_TRANSITION_SCRIPT
                .matches("redis.call('DEL', dead_record_prefix .. trimmed['id'])")
                .count()
                >= 1,
            "claimed-job dead-letter script should remove trimmed per-id metadata"
        );
        assert!(
            STALE_REDIS_RECOVERY_SCRIPT
                .matches("redis.call('DEL', dead_record_prefix .. trimmed['id'])")
                .count()
                >= 1,
            "stale-recovery dead-letter script should remove trimmed per-id metadata"
        );
    }

    #[cfg(feature = "redis")]
    fn redis_test_worker_config(
        prefix: &str,
        worker_id: &str,
        visibility_timeout_ms: u64,
    ) -> RedisWorkerConfig {
        RedisWorkerConfig {
            queue_key: format!("{prefix}:queue"),
            key_prefix: prefix.to_string(),
            schedule: QueueSchedule::from_config(&crate::config::JobQueuesConfig::single_default()),
            processing_key: format!("{prefix}:processing"),
            delayed_key: format!("{prefix}:delayed"),
            dead_key: format!("{prefix}:dead"),
            completed_key: format!("{prefix}:completed"),
            blocked_key: format!("{prefix}:blocked"),
            record_prefix: format!("{prefix}:record:"),
            dead_record_prefix: format!("{prefix}:dead-record:"),
            unique_prefix: format!("{prefix}:unique:"),
            concurrency_prefix: format!("{prefix}:concurrency:"),
            worker_id: worker_id.to_string(),
            visibility_timeout_ms,
            default_attempts: 3,
            default_backoff: 1,
            retry_promotion_interval: Duration::from_millis(1),
        }
    }

    #[cfg(feature = "redis")]
    async fn redis_test_client() -> (
        testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
        redis::Client,
    ) {
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::redis::Redis as RedisImage;

        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        (container, redis::Client::open(url).unwrap())
    }

    #[cfg(feature = "redis")]
    fn redis_jobs_by_name(
        handler: JobHandler,
        max_attempts: u32,
    ) -> Arc<RwLock<HashMap<String, JobInfo>>> {
        Arc::new(RwLock::new(HashMap::from([(
            "send_email".to_string(),
            JobInfo {
                name: "send_email".to_string(),
                max_attempts,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler,
            },
        )])))
    }

    #[cfg(feature = "redis")]
    async fn redis_enqueue_test_job(
        client: &redis::Client,
        worker_config: &RedisWorkerConfig,
        max_attempts: u32,
    ) {
        let connection = new_redis_connection_manager(client, "test redis producer").unwrap();
        let producer = RedisClient {
            connection,
            key_prefix: worker_config.key_prefix.clone(),
            delayed_key: worker_config.delayed_key.clone(),
            record_prefix: worker_config.record_prefix.clone(),
            unique_prefix: worker_config.unique_prefix.clone(),
        };
        producer
            .enqueue(
                uuid::Uuid::new_v4().to_string(),
                "send_email",
                "default",
                serde_json::json!({ "user_id": 42 }),
                max_attempts,
                1,
                None,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();
    }

    #[cfg(feature = "redis")]
    struct RedisAdminSeedRecords {
        enqueued: RedisJobRecord,
        running: RedisJobRecord,
        completed: RedisJobRecord,
        failed_retry: RedisJobRecord,
        failed_discard: RedisJobRecord,
    }

    #[cfg(feature = "redis")]
    fn redis_admin_test_backend(
        client: &redis::Client,
        worker_config: &RedisWorkerConfig,
    ) -> RedisJobAdminBackend {
        let admin_connection = new_redis_connection_manager(client, "test redis admin").unwrap();
        RedisJobAdminBackend::new(
            admin_connection,
            vec![worker_config.queue_key.clone()],
            worker_config.key_prefix.clone(),
            worker_config.delayed_key.clone(),
            worker_config.processing_key.clone(),
            worker_config.dead_key.clone(),
            worker_config.completed_key.clone(),
            worker_config.blocked_key.clone(),
            worker_config.record_prefix.clone(),
            worker_config.dead_record_prefix.clone(),
            worker_config.unique_prefix.clone(),
            128,
            crate::actuator::JobRegistry::new(),
        )
    }

    #[cfg(feature = "redis")]
    #[allow(clippy::too_many_lines)]
    fn redis_admin_seed_records(now: u64) -> RedisAdminSeedRecords {
        RedisAdminSeedRecords {
            enqueued: RedisJobRecord {
                id: "job-enqueued".to_string(),
                name: "send_email".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({"user_id": 42, "correlation_id": "req-redis"}),
                attempt: 1,
                max_attempts: 5,
                initial_backoff_ms: 250,
                enqueued_at_ms: Some(now.saturating_sub(4_000)),
                started_at_ms: None,
                finished_at_ms: None,
                claimed_by: None,
                claimed_at_ms: None,
                last_error: None,
                unique_key: None,
                unique_window: None,
                concurrency_key: None,
                concurrency_limit: None,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            running: RedisJobRecord {
                id: "job-running".to_string(),
                name: "reindex".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 3,
                initial_backoff_ms: 250,
                enqueued_at_ms: Some(now.saturating_sub(3_000)),
                started_at_ms: Some(now.saturating_sub(2_000)),
                finished_at_ms: None,
                claimed_by: Some("worker-a".to_string()),
                claimed_at_ms: Some(now.saturating_sub(2_000)),
                last_error: None,
                unique_key: None,
                unique_window: None,
                concurrency_key: None,
                concurrency_limit: None,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            completed: RedisJobRecord {
                id: "job-completed".to_string(),
                name: "digest".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 3,
                initial_backoff_ms: 250,
                enqueued_at_ms: Some(now.saturating_sub(3_000)),
                started_at_ms: Some(now.saturating_sub(2_000)),
                finished_at_ms: Some(now.saturating_sub(1_000)),
                claimed_by: None,
                claimed_at_ms: None,
                last_error: None,
                unique_key: None,
                unique_window: None,
                concurrency_key: None,
                concurrency_limit: None,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            failed_retry: RedisJobRecord {
                id: "job-failed-retry".to_string(),
                name: "send_email".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({ "user_id": 7 }),
                attempt: 5,
                max_attempts: 5,
                initial_backoff_ms: 250,
                enqueued_at_ms: Some(now.saturating_sub(4_000)),
                started_at_ms: Some(now.saturating_sub(3_000)),
                finished_at_ms: Some(now.saturating_sub(500)),
                claimed_by: None,
                claimed_at_ms: None,
                last_error: Some("smtp refused recipient".to_string()),
                unique_key: None,
                unique_window: None,
                concurrency_key: None,
                concurrency_limit: None,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            failed_discard: RedisJobRecord {
                id: "job-failed-discard".to_string(),
                name: "webhook".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 2,
                max_attempts: 2,
                initial_backoff_ms: 250,
                enqueued_at_ms: Some(now.saturating_sub(4_000)),
                started_at_ms: Some(now.saturating_sub(3_000)),
                finished_at_ms: Some(now.saturating_sub(250)),
                claimed_by: None,
                claimed_at_ms: None,
                last_error: Some("endpoint returned 410".to_string()),
                unique_key: None,
                unique_window: None,
                concurrency_key: None,
                concurrency_limit: None,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
        }
    }

    #[cfg(feature = "redis")]
    async fn redis_store_active_admin_record(
        connection: &mut redis::aio::ConnectionManager,
        worker_config: &RedisWorkerConfig,
        record: &RedisJobRecord,
        now: u64,
    ) {
        use redis::AsyncCommands as _;

        connection
            .set::<_, _, ()>(
                redis_record_key(&worker_config.record_prefix, &record.id),
                encode_redis_record(record).unwrap(),
            )
            .await
            .unwrap();
        match record.started_at_ms {
            Some(_) => {
                connection
                    .zadd::<_, _, _, ()>(
                        &worker_config.processing_key,
                        &record.id,
                        now.saturating_add(30_000),
                    )
                    .await
                    .unwrap();
            }
            None => {
                connection
                    .lpush::<_, _, ()>(&worker_config.queue_key, &record.id)
                    .await
                    .unwrap();
            }
        }
    }

    #[cfg(feature = "redis")]
    async fn redis_store_history_admin_record(
        connection: &mut redis::aio::ConnectionManager,
        worker_config: &RedisWorkerConfig,
        record: &RedisJobRecord,
        failed: bool,
    ) {
        use redis::AsyncCommands as _;

        let encoded = encode_redis_record(record).unwrap();
        if failed {
            connection
                .lpush::<_, _, ()>(&worker_config.dead_key, &encoded)
                .await
                .unwrap();
            connection
                .set::<_, _, ()>(
                    format!("{}{}", worker_config.dead_record_prefix, record.id),
                    encoded,
                )
                .await
                .unwrap();
        } else {
            connection
                .lpush::<_, _, ()>(&worker_config.completed_key, encoded)
                .await
                .unwrap();
        }
    }

    #[cfg(feature = "redis")]
    async fn seed_redis_admin_storage(
        connection: &mut redis::aio::ConnectionManager,
        worker_config: &RedisWorkerConfig,
        now: u64,
    ) -> RedisAdminSeedRecords {
        let records = redis_admin_seed_records(now);
        redis_store_active_admin_record(connection, worker_config, &records.enqueued, now).await;
        redis_store_active_admin_record(connection, worker_config, &records.running, now).await;
        redis_store_history_admin_record(connection, worker_config, &records.completed, false)
            .await;
        redis_store_history_admin_record(connection, worker_config, &records.failed_retry, true)
            .await;
        redis_store_history_admin_record(connection, worker_config, &records.failed_discard, true)
            .await;
        records
    }

    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_job_admin_dashboard_reads_cluster_storage_and_operates() {
        use redis::AsyncCommands as _;

        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("autumn:test:admin", "worker-a", 30_000);
        let backend = redis_admin_test_backend(&client, &worker_config);
        let mut connection = new_redis_connection_manager(&client, "test redis setup").unwrap();
        let records =
            seed_redis_admin_storage(&mut connection, &worker_config, now_unix_ms()).await;

        let snapshot = backend
            .snapshot(JobAdminQuery {
                enqueued_page: 1,
                scheduled_page: 1,
                running_page: 1,
                completed_page: 1,
                failed_page: 1,
                per_page: 10,
            })
            .await
            .expect("redis dashboard snapshot");
        assert_eq!(snapshot.enqueued.records[0].id, records.enqueued.id);
        assert_eq!(
            snapshot.enqueued.records[0].correlation_id.as_deref(),
            Some("req-redis")
        );
        assert_eq!(snapshot.running.records[0].id, records.running.id);
        assert_eq!(snapshot.completed.records[0].id, records.completed.id);
        assert_eq!(snapshot.failed.total, 2);

        backend
            .cancel(&records.enqueued.id)
            .await
            .expect("enqueued redis job should be cancelable");
        backend
            .retry(&records.failed_retry.id)
            .await
            .expect("failed redis job should be retryable");
        backend
            .discard(&records.failed_discard.id)
            .await
            .expect("failed redis job should be discardable");

        let queue_len: usize = connection.llen(&worker_config.queue_key).await.unwrap();
        let dead_len: usize = connection.llen(&worker_config.dead_key).await.unwrap();
        assert_eq!(queue_len, 1, "retry should enqueue a replacement job");
        assert_eq!(dead_len, 0, "retry and discard should clear failed jobs");
    }

    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_claim_drains_higher_priority_queue_first() {
        let (_container, client) = redis_test_client().await;
        let prefix = "autumn:test:prio";
        let worker_config = redis_test_worker_config(prefix, "worker-prio", 30_000);
        let mut connection = new_redis_connection_manager(&client, "test prio").unwrap();
        let producer = RedisClient {
            connection: connection.clone(),
            key_prefix: prefix.to_string(),
            delayed_key: worker_config.delayed_key.clone(),
            record_prefix: worker_config.record_prefix.clone(),
            unique_prefix: worker_config.unique_prefix.clone(),
        };

        // Low enqueued first, then critical.
        producer
            .enqueue(
                "low-1".to_string(),
                "bulk",
                "low",
                serde_json::json!({}),
                5,
                1,
                None,
                &ResolvedJobConstraints::default(),
            )
            .await
            .expect("enqueue low");
        producer
            .enqueue(
                "crit-1".to_string(),
                "urgent",
                "critical",
                serde_json::json!({}),
                5,
                1,
                None,
                &ResolvedJobConstraints::default(),
            )
            .await
            .expect("enqueue critical");

        // Strict priority: critical's queue key is attempted first.
        let order_keys = vec![
            redis_queue_key(prefix, "critical"),
            redis_queue_key(prefix, "default"),
            redis_queue_key(prefix, "low"),
        ];
        let first = claim_next_redis_job(&mut connection, &worker_config, &order_keys)
            .await
            .unwrap()
            .expect("first claim");
        assert_eq!(first.id, "crit-1");
        assert_eq!(first.queue, "critical");
        let second = claim_next_redis_job(&mut connection, &worker_config, &order_keys)
            .await
            .unwrap()
            .expect("second claim");
        assert_eq!(second.id, "low-1");
        assert_eq!(second.queue, "low");
    }

    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_claim_ack_deletes_record_only_after_success() {
        use redis::AsyncCommands as _;

        REDIS_HANDLER_CALLS.store(0, Ordering::SeqCst);
        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("autumn:test:ack", "worker-a", 30_000);
        redis_enqueue_test_job(&client, &worker_config, 2).await;

        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();
        let record = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("job should be claimed");
        let record_key = redis_record_key(&worker_config.record_prefix, &record.id);
        let processing_count: usize = connection
            .zcard(&worker_config.processing_key)
            .await
            .unwrap();
        assert_eq!(processing_count, 1);

        let state = AppState::for_test().with_profile("dev");
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        state.job_registry().register("send_email");
        state.job_registry().record_enqueue("send_email");
        process_redis_job_record(
            &mut connection,
            record,
            &redis_jobs_by_name(redis_counting_success_handler, 2),
            &state,
            &job_admin,
            &worker_config,
        )
        .await;

        let exists: bool = connection.exists(record_key).await.unwrap();
        let processing_count: usize = connection
            .zcard(&worker_config.processing_key)
            .await
            .unwrap();
        let dead_count: usize = connection.llen(&worker_config.dead_key).await.unwrap();
        assert!(!exists, "successful ack should delete the durable record");
        assert_eq!(processing_count, 0);
        assert_eq!(dead_count, 0);
        assert_eq!(REDIS_HANDLER_CALLS.load(Ordering::SeqCst), 1);
        let status = state.job_registry().snapshot()["send_email"].clone();
        assert_eq!(status.queued, 0);
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.total_successes, 1);
    }

    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_failure_retries_with_backoff_then_dead_letters() {
        use redis::AsyncCommands as _;

        REDIS_HANDLER_CALLS.store(0, Ordering::SeqCst);
        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("autumn:test:retry", "worker-a", 30_000);
        redis_enqueue_test_job(&client, &worker_config, 2).await;

        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();
        let state = AppState::for_test().with_profile("dev");
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        state.job_registry().register("send_email");
        state.job_registry().record_enqueue("send_email");
        let jobs = redis_jobs_by_name(redis_counting_failure_handler, 2);

        let first = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("first attempt should be claimed");
        process_redis_job_record(
            &mut connection,
            first,
            &jobs,
            &state,
            &job_admin,
            &worker_config,
        )
        .await;
        let delayed_count: usize = connection.zcard(&worker_config.delayed_key).await.unwrap();
        let processing_count: usize = connection
            .zcard(&worker_config.processing_key)
            .await
            .unwrap();
        assert_eq!(delayed_count, 1);
        assert_eq!(processing_count, 0);

        tokio::time::sleep(Duration::from_millis(5)).await;
        promote_due_redis_retries(&mut connection, &worker_config, &state, &job_admin)
            .await
            .unwrap();
        let queued_count: usize = connection.llen(&worker_config.queue_key).await.unwrap();
        assert_eq!(queued_count, 1);

        let second = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("retry attempt should be claimed");
        assert_eq!(second.attempt, 2);
        process_redis_job_record(
            &mut connection,
            second,
            &jobs,
            &state,
            &job_admin,
            &worker_config,
        )
        .await;

        let dead_count: usize = connection.llen(&worker_config.dead_key).await.unwrap();
        let delayed_count: usize = connection.zcard(&worker_config.delayed_key).await.unwrap();
        assert_eq!(dead_count, 1);
        assert_eq!(delayed_count, 0);
        assert_eq!(REDIS_HANDLER_CALLS.load(Ordering::SeqCst), 2);
        let status = state.job_registry().snapshot()["send_email"].clone();
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.total_failures, 1);
        assert_eq!(status.dead_letters, 1);
    }

    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_panicking_handler_dead_letters_without_retry() {
        use redis::AsyncCommands as _;

        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("autumn:test:panic", "worker-a", 30_000);
        redis_enqueue_test_job(&client, &worker_config, 3).await;

        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();
        let state = AppState::for_test().with_profile("dev");
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        state.job_registry().register("send_email");
        state.job_registry().record_enqueue("send_email");

        let record = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("panicking job should be claimed");
        process_redis_job_record(
            &mut connection,
            record,
            &redis_jobs_by_name(panicking_handler, 3),
            &state,
            &job_admin,
            &worker_config,
        )
        .await;

        let queued_count: usize = connection.llen(&worker_config.queue_key).await.unwrap();
        let delayed_count: usize = connection.zcard(&worker_config.delayed_key).await.unwrap();
        let processing_count: usize = connection
            .zcard(&worker_config.processing_key)
            .await
            .unwrap();
        let dead_count: usize = connection.llen(&worker_config.dead_key).await.unwrap();
        assert_eq!(queued_count, 0);
        assert_eq!(delayed_count, 0);
        assert_eq!(processing_count, 0);
        assert_eq!(dead_count, 1);

        let status = state.job_registry().snapshot()["send_email"].clone();
        assert_eq!(status.in_flight, 0);
        assert_eq!(status.total_failures, 1);
        assert_eq!(status.dead_letters, 1);
    }

    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_stale_claim_recovery_requeues_for_another_worker() {
        use redis::AsyncCommands as _;

        let (_container, client) = redis_test_client().await;
        let worker_a = redis_test_worker_config("autumn:test:stale", "worker-a", 1);
        let worker_b = redis_test_worker_config("autumn:test:stale", "worker-b", 30_000);
        redis_enqueue_test_job(&client, &worker_a, 3).await;

        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();
        let claimed = claim_next_redis_job(
            &mut connection,
            &worker_a,
            std::slice::from_ref(&worker_a.queue_key),
        )
        .await
        .unwrap()
        .expect("first worker should claim the job");
        assert_eq!(claimed.claimed_by.as_deref(), Some("worker-a"));
        assert_eq!(claimed.attempt, 1);

        tokio::time::sleep(Duration::from_millis(5)).await;
        let state = AppState::for_test().with_profile("dev");
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        state.job_registry().register("send_email");
        recover_stale_redis_jobs(&mut connection, &worker_b, &state, &job_admin)
            .await
            .unwrap();

        let queued_count: usize = connection.llen(&worker_b.queue_key).await.unwrap();
        assert_eq!(queued_count, 1);
        let reclaimed = claim_next_redis_job(
            &mut connection,
            &worker_b,
            std::slice::from_ref(&worker_b.queue_key),
        )
        .await
        .unwrap()
        .expect("second worker should reclaim stale job");
        assert_eq!(reclaimed.claimed_by.as_deref(), Some("worker-b"));
        assert_eq!(reclaimed.attempt, 2);
        assert!(
            reclaimed
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("visibility timeout expired"))
        );
    }

    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_stale_terminal_failure_keeps_retry_discard_metadata() {
        use redis::AsyncCommands as _;

        let (_container, client) = redis_test_client().await;
        let worker_a = redis_test_worker_config("autumn:test:stale-dead", "worker-a", 1);
        let worker_b = redis_test_worker_config("autumn:test:stale-dead", "worker-b", 30_000);
        redis_enqueue_test_job(&client, &worker_a, 1).await;

        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();
        let claimed = claim_next_redis_job(
            &mut connection,
            &worker_a,
            std::slice::from_ref(&worker_a.queue_key),
        )
        .await
        .unwrap()
        .expect("first worker should claim the final attempt");
        assert_eq!(claimed.claimed_by.as_deref(), Some("worker-a"));
        assert_eq!(claimed.attempt, 1);
        let failed_id = claimed.id.clone();

        tokio::time::sleep(Duration::from_millis(5)).await;
        let state = AppState::for_test().with_profile("dev");
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        state.job_registry().register("send_email");
        recover_stale_redis_jobs(&mut connection, &worker_b, &state, &job_admin)
            .await
            .unwrap();

        let dead_record_key = format!("{}{}", worker_b.dead_record_prefix, failed_id);
        let dead_record: Option<String> = connection.get(&dead_record_key).await.unwrap();
        assert!(
            dead_record.is_some(),
            "stale terminal failures need per-id metadata for admin actions"
        );
        let dead_count: usize = connection.llen(&worker_b.dead_key).await.unwrap();
        assert_eq!(dead_count, 1);

        let backend = redis_admin_test_backend(&client, &worker_b);
        backend
            .retry(&failed_id)
            .await
            .expect("stale terminal failure should be retryable from the dashboard");

        let queued_count: usize = connection.llen(&worker_b.queue_key).await.unwrap();
        let dead_count: usize = connection.llen(&worker_b.dead_key).await.unwrap();
        let dead_record_exists: bool = connection.exists(&dead_record_key).await.unwrap();
        assert_eq!(queued_count, 1);
        assert_eq!(dead_count, 0);
        assert!(!dead_record_exists);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_release_unique_on_settle_applies_to_terminal_non_ttl_only() {
        let mut record = redis_test_record(1, 3);
        record.unique_key = Some("k".to_string());
        record.unique_window = Some("running".to_string());
        assert!(redis_release_unique_on_settle(&record, "success"));
        assert!(redis_release_unique_on_settle(&record, "dead"));
        assert!(!redis_release_unique_on_settle(&record, "retry"));

        record.unique_window = Some("ttl".to_string());
        assert!(
            !redis_release_unique_on_settle(&record, "success"),
            "TTL-window locks expire by time, never by settle"
        );

        record.unique_key = None;
        record.unique_window = None;
        assert!(!redis_release_unique_on_settle(&record, "success"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_unique_lock_ttl_uses_window_ttl_or_crash_backstop() {
        assert_eq!(
            redis_unique_lock_ttl_ms(Some(JobUniquenessWindow::TtlMs(5_000))),
            5_000
        );
        assert_eq!(
            redis_unique_lock_ttl_ms(Some(JobUniquenessWindow::Running)),
            REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS
        );
        assert_eq!(
            redis_unique_lock_ttl_ms(None),
            REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_delayed_unique_lock_ttl_extends_for_long_delays() {
        // Helper mirroring the exact formula in RedisClient::enqueue so we
        // can test it without a live Redis connection.
        fn compute_lock_ttl(window: Option<JobUniquenessWindow>, due_at_ms: Option<u64>) -> u64 {
            let base = redis_unique_lock_ttl_ms(window);
            match due_at_ms {
                Some(due_ms) if !matches!(window, Some(JobUniquenessWindow::TtlMs(_))) => {
                    let delay_ms = due_ms.saturating_sub(now_unix_ms());
                    delay_ms
                        .saturating_add(REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS)
                        .max(base)
                }
                _ => base,
            }
        }

        let two_days_ms: u64 = 2 * 24 * 60 * 60 * 1_000;
        let due_ms = now_unix_ms() + two_days_ms;

        // Non-TTL window + long delay: lock must outlast the 24h backstop.
        let ttl_running = compute_lock_ttl(Some(JobUniquenessWindow::Running), Some(due_ms));
        assert!(
            ttl_running >= two_days_ms,
            "Running-window lock {ttl_running}ms must cover the 2-day delay"
        );

        // TTL-window: lock must stay at the user-specified value regardless.
        let user_ttl_ms: u64 = 3_600_000; // 1h
        let ttl_explicit =
            compute_lock_ttl(Some(JobUniquenessWindow::TtlMs(user_ttl_ms)), Some(due_ms));
        assert_eq!(
            ttl_explicit, user_ttl_ms,
            "TtlMs-window lock must not be extended for delayed jobs"
        );

        // No delay: lock stays at the 24h backstop.
        let ttl_immediate = compute_lock_ttl(Some(JobUniquenessWindow::Running), None);
        assert_eq!(ttl_immediate, REDIS_UNIQUE_LOCK_TTL_BACKSTOP_MS);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_record_without_constraint_fields_deserializes_as_none() {
        // Records written by pre-#829 producers have none of the new fields.
        let legacy = r#"{"id":"a","name":"send_email","payload":{},"attempt":1,
            "max_attempts":3,"initial_backoff_ms":10}"#;
        let record: RedisJobRecord = serde_json::from_str(legacy).unwrap();
        assert!(record.unique_key.is_none());
        assert!(record.unique_window.is_none());
        assert!(record.concurrency_key.is_none());
        assert!(record.concurrency_limit.is_none());

        // And None fields stay absent on the wire so Lua sees nil, not null.
        let encoded = encode_redis_record(&record).unwrap();
        assert!(!encoded.contains("unique_key"));
        assert!(!encoded.contains("concurrency_limit"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_admin_retry_conflict_code_maps_to_actionable_error() {
        let error = redis_admin_operation_result(-3, "job-1", "retry failed job").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("an equivalent unique job is already pending or running"),
            "{error}"
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_requeue_unique_action_matches_window() {
        let mut record = redis_test_record(1, 3);
        assert_eq!(redis_requeue_unique_action(&record), "");

        record.unique_key = Some("k".to_string());
        record.unique_window = Some("pending".to_string());
        assert_eq!(redis_requeue_unique_action(&record), "pending");

        record.unique_window = Some("running".to_string());
        assert_eq!(redis_requeue_unique_action(&record), "running");

        // TTL locks expire by time; requeues neither re-acquire nor refresh.
        record.unique_window = Some("ttl".to_string());
        assert_eq!(redis_requeue_unique_action(&record), "");
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_admin_cancel_releases_unique_lock_and_covers_blocked_jobs() {
        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("cancel", "worker-1", 30_000);
        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();
        let admin = redis_admin_test_backend(&client, &worker_config);

        let constraints = ResolvedJobConstraints {
            unique_key: Some("invoice-9".to_string()),
            unique_window: Some(JobUniquenessWindow::Running),
            concurrency_limit: None,
            concurrency_scope: None,
        };
        assert_eq!(
            redis_enqueue_with_constraints(
                &client,
                &worker_config,
                "k1",
                "send_invoice",
                &constraints
            )
            .await,
            EnqueueOutcome::Queued
        );

        // Canceling the queued job must hand the unique lock back so the next
        // enqueue is accepted instead of coalescing against canceled work.
        admin.cancel_enqueued_redis("k1").await.unwrap();
        let lock: Option<String> = redis::cmd("GET")
            .arg(redis_unique_lock_key(
                &worker_config.unique_prefix,
                "send_invoice",
                "invoice-9",
            ))
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(lock.is_none(), "cancel must release the unique lock");
        assert_eq!(
            redis_enqueue_with_constraints(
                &client,
                &worker_config,
                "k2",
                "send_invoice",
                &constraints
            )
            .await,
            EnqueueOutcome::Queued
        );

        // A concurrency-parked job (in the blocked zset, not the queue list)
        // must be cancelable too.
        let limited = ResolvedJobConstraints {
            unique_key: None,
            unique_window: None,
            concurrency_limit: Some(1),
            concurrency_scope: None,
        };
        for id in ["b1", "b2"] {
            redis_enqueue_with_constraints(&client, &worker_config, id, "recalculate", &limited)
                .await;
        }
        // Claim k2 out of the way first, then claim b1 so b2 parks.
        let mut parked_target = None;
        while let Some(record) = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        {
            if record.name == "recalculate" {
                parked_target = Some(record);
            }
        }
        let _running = parked_target.expect("one recalculate claimed");
        let parked: i64 = redis::cmd("ZCARD")
            .arg(&worker_config.blocked_key)
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(parked, 1, "second recalculate should be parked");
        let parked_id: Vec<String> = redis::cmd("ZRANGE")
            .arg(&worker_config.blocked_key)
            .arg(0)
            .arg(-1)
            .query_async(&mut connection)
            .await
            .unwrap();
        admin
            .cancel_enqueued_redis(&parked_id[0])
            .await
            .expect("parked jobs must be cancelable");
    }

    #[cfg(feature = "redis")]
    async fn redis_enqueue_with_constraints(
        client: &redis::Client,
        worker_config: &RedisWorkerConfig,
        id: &str,
        name: &str,
        constraints: &ResolvedJobConstraints,
    ) -> EnqueueOutcome {
        let connection = new_redis_connection_manager(client, "test redis producer").unwrap();
        let producer = RedisClient {
            connection,
            key_prefix: worker_config.key_prefix.clone(),
            delayed_key: worker_config.delayed_key.clone(),
            record_prefix: worker_config.record_prefix.clone(),
            unique_prefix: worker_config.unique_prefix.clone(),
        };
        producer
            .enqueue(
                id.to_string(),
                name,
                "default",
                serde_json::json!({ "marker": id }),
                3,
                1,
                None,
                constraints,
            )
            .await
            .unwrap()
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_unique_enqueue_coalesces_burst_and_releases_on_success() {
        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("uniq", "worker-1", 30_000);
        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();

        let constraints = ResolvedJobConstraints {
            unique_key: Some("invoice-7".to_string()),
            unique_window: Some(JobUniquenessWindow::Running),
            concurrency_limit: None,
            concurrency_scope: None,
        };
        let first = redis_enqueue_with_constraints(
            &client,
            &worker_config,
            "u1",
            "send_invoice",
            &constraints,
        )
        .await;
        let second = redis_enqueue_with_constraints(
            &client,
            &worker_config,
            "u2",
            "send_invoice",
            &constraints,
        )
        .await;
        assert_eq!(first, EnqueueOutcome::Queued);
        assert_eq!(
            second,
            EnqueueOutcome::Deduplicated,
            "burst of two identical unique enqueues must coalesce"
        );
        let queued: i64 = redis::cmd("LLEN")
            .arg(&worker_config.queue_key)
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(queued, 1, "exactly one queue entry for the burst");

        // While in flight, the key is still held.
        let record = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("claim the single job");
        let inflight = redis_enqueue_with_constraints(
            &client,
            &worker_config,
            "u3",
            "send_invoice",
            &constraints,
        )
        .await;
        assert_eq!(inflight, EnqueueOutcome::Deduplicated);

        // Success releases the lock; a new enqueue is accepted.
        assert!(
            ack_redis_success(&mut connection, &worker_config, &record)
                .await
                .unwrap()
        );
        let after = redis_enqueue_with_constraints(
            &client,
            &worker_config,
            "u4",
            "send_invoice",
            &constraints,
        )
        .await;
        assert_eq!(after, EnqueueOutcome::Queued);
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_concurrency_limit_blocks_claims_until_settle() {
        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("cap", "worker-1", 30_000);
        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();

        let constraints = ResolvedJobConstraints {
            unique_key: None,
            unique_window: None,
            concurrency_limit: Some(1),
            concurrency_scope: Some("acct-9".to_string()),
        };
        for id in ["c1", "c2"] {
            assert_eq!(
                redis_enqueue_with_constraints(
                    &client,
                    &worker_config,
                    id,
                    "recalculate",
                    &constraints
                )
                .await,
                EnqueueOutcome::Queued
            );
        }

        let first = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("first claim");
        assert!(
            claim_next_redis_job(
                &mut connection,
                &worker_config,
                std::slice::from_ref(&worker_config.queue_key)
            )
            .await
            .unwrap()
            .is_none(),
            "second claim must park behind the concurrency limit"
        );
        let parked: i64 = redis::cmd("ZCARD")
            .arg(&worker_config.blocked_key)
            .query_async(&mut connection)
            .await
            .unwrap();
        assert_eq!(parked, 1);

        // Settle the running job, wait out the requeue delay, promote, claim.
        assert!(
            ack_redis_success(&mut connection, &worker_config, &first)
                .await
                .unwrap()
        );
        tokio::time::sleep(Duration::from_millis(
            REDIS_CONCURRENCY_REQUEUE_DELAY_MS + 50,
        ))
        .await;
        promote_due_blocked_redis_jobs(&mut connection, &worker_config)
            .await
            .unwrap();
        let second = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("slot freed after settle");
        assert_ne!(second.id, first.id);
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_stale_recovery_frees_slot_and_dead_letter_releases_lock() {
        let (_container, client) = redis_test_client().await;
        // 10ms visibility timeout: an unsettled claim is immediately stale.
        let worker_config = redis_test_worker_config("crash", "dead-worker", 10);
        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();
        let state = AppState::for_test().with_profile("dev");
        state.job_registry().register("crashy");
        let job_admin = JobAdminMemoryBackend::new_for_test(32);

        let constraints = ResolvedJobConstraints {
            unique_key: Some("crash-key".to_string()),
            unique_window: Some(JobUniquenessWindow::Running),
            concurrency_limit: Some(1),
            concurrency_scope: None,
        };
        let connection_producer =
            new_redis_connection_manager(&client, "test redis producer").unwrap();
        let producer = RedisClient {
            connection: connection_producer,
            key_prefix: worker_config.key_prefix.clone(),
            delayed_key: worker_config.delayed_key.clone(),
            record_prefix: worker_config.record_prefix.clone(),
            unique_prefix: worker_config.unique_prefix.clone(),
        };
        // max_attempts = 1 so stale recovery dead-letters instead of requeueing.
        assert_eq!(
            producer
                .enqueue(
                    "x1".to_string(),
                    "crashy",
                    "default",
                    serde_json::json!({}),
                    1,
                    1,
                    None,
                    &constraints,
                )
                .await
                .unwrap(),
            EnqueueOutcome::Queued
        );

        // Simulate a crashed worker: claim, never settle.
        let _claimed = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("claim");
        tokio::time::sleep(Duration::from_millis(30)).await;
        recover_stale_redis_jobs(&mut connection, &worker_config, &state, &job_admin)
            .await
            .unwrap();

        // The dead-letter released both the unique lock and the slot.
        let counter: Option<String> = redis::cmd("GET")
            .arg(redis_concurrency_counter_key(
                &worker_config.concurrency_prefix,
                "crashy",
                None,
            ))
            .query_async(&mut connection)
            .await
            .unwrap();
        assert!(counter.is_none(), "slot must be freed after stale recovery");
        assert_eq!(
            producer
                .enqueue(
                    "x2".to_string(),
                    "crashy",
                    "default",
                    serde_json::json!({}),
                    1,
                    1,
                    None,
                    &constraints,
                )
                .await
                .unwrap(),
            EnqueueOutcome::Queued,
            "a dead worker must not deadlock the unique key"
        );
    }

    // Success metric (#1025) on Redis: a delayed enqueue lands on the `:delayed`
    // ZSET (not the queue), is not claimable before its due time, survives a
    // reconnect mid-delay, and is promoted to the queue and claimed exactly once
    // after the due time passes.
    #[cfg(feature = "redis")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_delayed_enqueue_waits_for_due_time_and_survives_restart() {
        use redis::AsyncCommands as _;

        let (_container, client) = redis_test_client().await;
        let worker_config = redis_test_worker_config("autumn:test:delayed", "worker-d", 30_000);
        let state = AppState::for_test().with_profile("dev");
        state.job_registry().register("send_email");
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        let mut connection = new_redis_connection_manager(&client, "test redis worker").unwrap();

        let producer = RedisClient {
            connection: new_redis_connection_manager(&client, "test redis producer").unwrap(),
            key_prefix: worker_config.key_prefix.clone(),
            delayed_key: worker_config.delayed_key.clone(),
            record_prefix: worker_config.record_prefix.clone(),
            unique_prefix: worker_config.unique_prefix.clone(),
        };

        // Enqueue due ~2s in the future.
        let due_at_ms = now_unix_ms() + 2_000;
        assert_eq!(
            producer
                .enqueue(
                    "d1".to_string(),
                    "send_email",
                    "default",
                    serde_json::json!({ "user_id": 7 }),
                    3,
                    1,
                    Some(due_at_ms),
                    &ResolvedJobConstraints::default(),
                )
                .await
                .unwrap(),
            EnqueueOutcome::Queued
        );

        // It is parked on the delayed ZSET, not the work queue.
        let queue_len: usize = connection.llen(&worker_config.queue_key).await.unwrap();
        let delayed_len: usize = connection.zcard(&worker_config.delayed_key).await.unwrap();
        assert_eq!(
            queue_len, 0,
            "delayed job must not be on the work queue yet"
        );
        assert_eq!(delayed_len, 1, "delayed job must be on the delayed ZSET");

        // Promotion before the due time is a no-op; nothing becomes claimable.
        promote_due_redis_retries(&mut connection, &worker_config, &state, &job_admin)
            .await
            .unwrap();
        assert!(
            claim_next_redis_job(
                &mut connection,
                &worker_config,
                std::slice::from_ref(&worker_config.queue_key)
            )
            .await
            .unwrap()
            .is_none(),
            "delayed job must not be claimable before its due time"
        );

        // Simulate a worker restart mid-delay: reconnect. The ZSET entry persists.
        drop(connection);
        let mut connection = new_redis_connection_manager(&client, "test redis worker 2").unwrap();

        // After the due time: promotion moves it onto the queue and it claims once.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        promote_due_redis_retries(&mut connection, &worker_config, &state, &job_admin)
            .await
            .unwrap();
        let claimed = claim_next_redis_job(
            &mut connection,
            &worker_config,
            std::slice::from_ref(&worker_config.queue_key),
        )
        .await
        .unwrap()
        .expect("due job should be claimable after its due time");
        assert_eq!(claimed.id, "d1");
        assert_eq!(claimed.attempt, 1);
        assert!(
            claim_next_redis_job(
                &mut connection,
                &worker_config,
                std::slice::from_ref(&worker_config.queue_key)
            )
            .await
            .unwrap()
            .is_none(),
            "a due job must be delivered to exactly one worker"
        );
    }

    #[tokio::test]
    async fn enqueue_rejects_unregistered_job_name_before_queueing() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "known".to_string(),
                max_attempts: 3,
                initial_backoff_ms: 10,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let error = enqueue("typoed-job", serde_json::json!({}))
            .await
            .expect_err("unknown job names should be rejected before queueing");
        assert!(
            error
                .to_string()
                .contains("job 'typoed-job' is not registered"),
            "unexpected error: {error}"
        );

        let snapshot = state.job_registry().snapshot();
        assert!(
            !snapshot.contains_key("typoed-job"),
            "unknown jobs must not be recorded as queued"
        );
        let known = snapshot
            .get("known")
            .expect("registered job should remain in the registry");
        assert_eq!(known.queued, 0);
        assert_eq!(known.in_flight, 0);

        shutdown.cancel();
        clear_global_job_client();
    }

    #[tokio::test]
    async fn start_runtime_rejects_duplicate_job_names() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let error = start_runtime(
            vec![
                JobInfo {
                    name: "dupe".to_string(),
                    max_attempts: 1,
                    initial_backoff_ms: 1,
                    queue: "default".to_string(),
                    uniqueness: None,
                    concurrency: None,
                    handler: |_state, _payload| Box::pin(async move { Ok(()) }),
                },
                JobInfo {
                    name: "dupe".to_string(),
                    max_attempts: 1,
                    initial_backoff_ms: 1,
                    queue: "default".to_string(),
                    uniqueness: None,
                    concurrency: None,
                    handler: |_state, _payload| Box::pin(async move { Ok(()) }),
                },
            ],
            &state,
            &shutdown,
            &crate::config::JobConfig::default(),
        )
        .expect_err("duplicate job names should surface as init errors");

        assert!(
            error
                .to_string()
                .contains("invalid jobs configuration: duplicate job name 'dupe'"),
            "unexpected error: {error}"
        );
        assert!(global_job_client().is_none());
    }

    #[tokio::test]
    #[cfg(not(feature = "redis"))]
    async fn start_runtime_rejects_redis_backend_when_feature_disabled() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let config = crate::config::JobConfig {
            backend: "redis".to_string(),
            ..Default::default()
        };

        let error = start_runtime(
            vec![JobInfo {
                name: "known".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            &config,
        )
        .expect_err("redis backend must fail without the redis feature");

        assert!(
            error
                .to_string()
                .contains("jobs.backend=redis requested but redis feature is disabled"),
            "unexpected error: {error}"
        );
        assert!(global_job_client().is_none());
    }

    #[tokio::test]
    #[cfg(feature = "redis")]
    async fn start_runtime_rejects_redis_backend_without_url() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let config = crate::config::JobConfig {
            backend: "redis".to_string(),
            redis: crate::config::JobRedisConfig {
                url: None,
                ..Default::default()
            },
            ..Default::default()
        };

        let error = start_runtime(
            vec![JobInfo {
                name: "known".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            &config,
        )
        .expect_err("redis backend must fail when its url is missing");

        assert!(
            error
                .to_string()
                .contains("jobs.backend=redis requires jobs.redis.url"),
            "unexpected error: {error}"
        );
        assert!(global_job_client().is_none());
    }

    #[tokio::test]
    async fn clear_global_job_client_resets_client() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        assert!(global_job_client().is_none());

        init_global_job_client(JobClient {
            local_sender: None,
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: JobAdminMemoryBackend::new_for_test(32),
            default_max_attempts: 3,
            default_initial_backoff_ms: 250,
            per_job_settings: HashMap::new(),
            interceptor: None,
            resilience_config: None,
        });
        assert!(global_job_client().is_some());

        clear_global_job_client();
        assert!(global_job_client().is_none());
    }

    // ── Pure-logic unit tests (no infrastructure required) ───────────────────

    #[test]
    fn job_admin_status_label_is_stable() {
        assert_eq!(JobAdminStatus::Enqueued.label(), "enqueued");
        assert_eq!(JobAdminStatus::Running.label(), "running");
        assert_eq!(JobAdminStatus::Retrying.label(), "retrying");
        assert_eq!(JobAdminStatus::Completed.label(), "completed");
        assert_eq!(JobAdminStatus::Failed.label(), "failed");
        assert_eq!(JobAdminStatus::Discarded.label(), "discarded");
        assert_eq!(JobAdminStatus::Canceled.label(), "canceled");
        assert_eq!(JobAdminStatus::Retried.label(), "retried");
    }

    #[test]
    fn job_admin_page_total_pages_rounds_up() {
        assert_eq!(JobAdminPage::new(Vec::new(), 11, 1, 5).total_pages(), 3);
        assert_eq!(JobAdminPage::new(Vec::new(), 10, 1, 5).total_pages(), 2);
        assert_eq!(JobAdminPage::new(Vec::new(), 0, 1, 5).total_pages(), 0);
        assert_eq!(JobAdminPage::new(Vec::new(), 1, 1, 5).total_pages(), 1);
    }

    #[test]
    fn job_admin_page_total_pages_is_zero_when_per_page_is_zero() {
        assert_eq!(JobAdminPage::new(Vec::new(), 5, 1, 0).total_pages(), 0);
    }

    #[test]
    fn job_admin_snapshot_empty_has_correct_shape() {
        let snap = JobAdminSnapshot::empty();
        assert_eq!(snap.enqueued.total, 0);
        assert_eq!(snap.running.total, 0);
        assert_eq!(snap.completed.total, 0);
        assert_eq!(snap.failed.total, 0);
        assert!(snap.schedules.is_empty());
        assert_eq!(snap.bounded_history_limit, DEFAULT_JOB_ADMIN_HISTORY_LIMIT);
        assert_eq!(snap.enqueued.per_page, DEFAULT_JOB_ADMIN_PER_PAGE);
    }

    #[test]
    fn job_admin_query_default_starts_at_page_one() {
        let q = JobAdminQuery::default();
        assert_eq!(q.enqueued_page, 1);
        assert_eq!(q.running_page, 1);
        assert_eq!(q.completed_page, 1);
        assert_eq!(q.failed_page, 1);
        assert_eq!(q.per_page, DEFAULT_JOB_ADMIN_PER_PAGE);
    }

    #[test]
    fn format_job_panic_extracts_owned_string_message() {
        let panic: Box<dyn std::any::Any + Send> = Box::new("stripe timed out".to_owned());
        assert_eq!(
            format_job_panic(panic.as_ref()),
            "job handler panicked: stripe timed out"
        );
    }

    #[test]
    fn format_job_panic_extracts_static_str() {
        let s: &'static str = "static panic message";
        let panic: Box<dyn std::any::Any + Send> = Box::new(s);
        assert_eq!(
            format_job_panic(panic.as_ref()),
            "job handler panicked: static panic message"
        );
    }

    #[test]
    fn format_job_panic_handles_non_string_payload() {
        let panic: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(
            format_job_panic(panic.as_ref()),
            "job handler panicked: non-string panic payload"
        );
    }

    #[test]
    fn job_payload_identity_prefers_principal_id_over_principal_and_user_id() {
        let (principal, _) = job_payload_identity(&serde_json::json!({
            "principal_id": "pid-1",
            "principal": "pid-2",
            "user_id": 3
        }));
        assert_eq!(principal.as_deref(), Some("pid-1"));
    }

    #[test]
    fn job_payload_identity_falls_back_to_principal_then_user_id() {
        let (p1, _) = job_payload_identity(&serde_json::json!({"principal": "p-abc"}));
        assert_eq!(p1.as_deref(), Some("p-abc"));

        let (p2, _) = job_payload_identity(&serde_json::json!({"user_id": 42}));
        assert_eq!(p2.as_deref(), Some("42"));
    }

    #[test]
    fn job_payload_identity_prefers_correlation_id_over_request_id() {
        let (_, correlation) = job_payload_identity(&serde_json::json!({
            "correlation_id": "cid-1",
            "request_id": "cid-2"
        }));
        assert_eq!(correlation.as_deref(), Some("cid-1"));
    }

    #[test]
    fn job_payload_identity_falls_back_to_request_id() {
        let (_, correlation) = job_payload_identity(&serde_json::json!({"request_id": "req-abc"}));
        assert_eq!(correlation.as_deref(), Some("req-abc"));
    }

    #[test]
    fn job_payload_identity_ignores_empty_string_values() {
        let (principal, correlation) = job_payload_identity(&serde_json::json!({
            "principal_id": "",
            "user_id": 99,
            "correlation_id": "",
            "request_id": "req-fallback"
        }));
        assert_eq!(principal.as_deref(), Some("99"));
        assert_eq!(correlation.as_deref(), Some("req-fallback"));
    }

    #[test]
    fn job_payload_identity_stringifies_numeric_values() {
        let (principal, _) = job_payload_identity(&serde_json::json!({"user_id": 123}));
        assert_eq!(principal.as_deref(), Some("123"));
    }

    #[test]
    fn job_payload_identity_stringifies_boolean_values() {
        let (principal, _) = job_payload_identity(&serde_json::json!({"user_id": true}));
        assert_eq!(principal.as_deref(), Some("true"));
    }

    #[test]
    fn job_payload_identity_returns_none_for_non_object_payload() {
        let (principal, correlation) = job_payload_identity(&serde_json::json!("not an object"));
        assert!(principal.is_none());
        assert!(correlation.is_none());
    }

    #[test]
    fn job_payload_identity_returns_none_when_no_matching_keys() {
        let (principal, correlation) =
            job_payload_identity(&serde_json::json!({"unrelated": "value"}));
        assert!(principal.is_none());
        assert!(correlation.is_none());
    }

    #[test]
    fn job_admin_start_returns_missing_for_unknown_id() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        assert_eq!(
            backend.try_record_start("nonexistent", 1),
            JobAdminStartDecision::Missing
        );
    }

    #[test]
    fn job_admin_start_returns_already_transitioned_for_non_enqueued_job() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("work", serde_json::json!({}), 1, 3);
        backend.record_start_for_test(&id, 1);
        backend.record_success_for_test(&id);

        assert_eq!(
            backend.try_record_start(&id, 1),
            JobAdminStartDecision::AlreadyTransitioned
        );
    }

    #[tokio::test]
    async fn job_admin_record_retrying_transitions_to_retrying_status() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("work", serde_json::json!({}), 1, 3);
        backend.record_start_for_test(&id, 1);
        backend.record_retrying(&id, "temporary glitch");

        let snapshot = backend
            .snapshot(JobAdminQuery::default())
            .await
            .expect("snapshot");
        assert!(
            snapshot.running.records.is_empty(),
            "running should be empty after retrying"
        );
        assert!(
            snapshot.failed.records.is_empty(),
            "retrying is not terminal-failed"
        );
        assert!(
            snapshot.enqueued.records.is_empty(),
            "retrying is not enqueued"
        );
    }

    #[tokio::test]
    async fn job_admin_record_requeued_transitions_back_to_enqueued() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("work", serde_json::json!({}), 1, 3);
        backend.record_start_for_test(&id, 1);
        backend.record_retrying(&id, "glitch");
        backend.record_requeued(&id, 2);

        let snapshot = backend
            .snapshot(JobAdminQuery::default())
            .await
            .expect("snapshot after requeue");
        assert_eq!(snapshot.enqueued.total, 1);
        assert_eq!(snapshot.enqueued.records[0].attempt, 2);
    }

    #[tokio::test]
    async fn job_admin_discard_rejects_non_failed_job() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("work", serde_json::json!({}), 1, 3);

        let error = backend
            .discard(&id)
            .await
            .expect_err("enqueued job must not be discardable");
        assert!(
            error
                .to_string()
                .contains("only failed jobs can be discarded"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn job_admin_cancel_rejects_non_enqueued_job() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("work", serde_json::json!({}), 1, 3);
        backend.record_start_for_test(&id, 1);

        let error = backend
            .cancel(&id)
            .await
            .expect_err("running job must not be cancelable");
        assert!(
            error
                .to_string()
                .contains("only enqueued or scheduled jobs can be canceled"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn job_admin_history_limit_evicts_finished_jobs_keeping_active() {
        let backend = JobAdminMemoryBackend::with_history_limit(3);
        for _ in 0..3 {
            let id = backend.record_enqueue_for_test("done", serde_json::json!({}), 1, 1);
            backend.record_start_for_test(&id, 1);
            backend.record_success_for_test(&id);
        }
        let active_id = backend.record_enqueue_for_test("active", serde_json::json!({}), 1, 3);
        let overflow_id = backend.record_enqueue_for_test("overflow", serde_json::json!({}), 1, 1);
        backend.record_start_for_test(&overflow_id, 1);
        backend.record_success_for_test(&overflow_id);

        let snapshot = backend
            .snapshot(JobAdminQuery::default())
            .await
            .expect("snapshot");
        assert_eq!(
            snapshot.enqueued.total, 1,
            "active job must survive eviction"
        );
        assert_eq!(snapshot.enqueued.records[0].id, active_id);
    }

    #[tokio::test]
    async fn job_admin_snapshot_pagination_second_page() {
        let backend = JobAdminMemoryBackend::new_for_test(100);
        for i in 0..5u32 {
            backend.record_enqueue_for_test("work", serde_json::json!({"n": i}), 1, 3);
        }

        let snapshot = backend
            .snapshot(JobAdminQuery {
                enqueued_page: 2,
                scheduled_page: 1,
                running_page: 1,
                completed_page: 1,
                failed_page: 1,
                per_page: 3,
            })
            .await
            .expect("snapshot page 2");

        assert_eq!(snapshot.enqueued.total, 5);
        assert_eq!(snapshot.enqueued.records.len(), 2);
        assert_eq!(snapshot.enqueued.page, 2);
        assert_eq!(snapshot.enqueued.total_pages(), 2);
    }

    #[tokio::test]
    async fn run_job_handler_reports_async_panics() {
        let state = AppState::for_test().with_profile("dev");
        let outcome =
            run_job_handler(
                "test_job",
                panicking_handler,
                state,
                serde_json::json!({}),
                true,
            )
            .await;
        assert_eq!(
            outcome,
            JobExecutionOutcome::Panicked("job handler panicked: forced panic".to_string())
        );
    }

    #[tokio::test]
    async fn local_unknown_job_name_records_failure_and_does_not_requeue() {
        let state = AppState::for_test().with_profile("dev");
        let jobs_by_name: Arc<RwLock<HashMap<String, JobInfo>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel(1);
        let job_admin = JobAdminMemoryBackend::new_for_test(32);
        let job_id = job_admin.record_enqueue_for_test("ghost", serde_json::json!({}), 1, 1);

        execute_local_job(
            QueuedJob {
                id: job_id.clone(),
                name: "ghost".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 1,
                initial_backoff_ms: 1,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            },
            &jobs_by_name,
            &tx,
            &state,
            &job_admin,
            &Arc::new(LocalJobCoordination::default()),
        )
        .await;

        assert!(timeout(Duration::from_millis(25), rx.recv()).await.is_err());
        let snapshot = job_admin
            .snapshot(JobAdminQuery::default())
            .await
            .expect("snapshot");
        assert_eq!(snapshot.failed.total, 1);
        assert!(
            snapshot.failed.records[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("unknown job")),
            "unknown job error message expected"
        );
    }

    // ── Postgres backend (RED → GREEN) ────────────────────────────────────────

    #[cfg(feature = "db")]
    mod pg {
        use super::*;
        use diesel_async::RunQueryDsl as _;

        // ── Pure-logic unit tests (no Postgres required) ──────────────────

        #[test]
        fn pg_config_default_visibility_timeout_is_thirty_seconds() {
            let config = crate::config::JobPostgresConfig::default();
            assert_eq!(config.visibility_timeout_ms, 30_000);
        }

        #[test]
        fn pg_retry_delay_grows_exponentially() {
            assert_eq!(pg_retry_delay_ms(250, 1), 250);
            assert_eq!(pg_retry_delay_ms(250, 2), 500);
            assert_eq!(pg_retry_delay_ms(250, 3), 1_000);
            assert_eq!(pg_retry_delay_ms(250, 4), 2_000);
        }

        fn pg_test_row(id: &str, name: &str, attempt: i32, max_attempts: i32) -> PgJobRow {
            PgJobRow {
                id: id.to_owned(),
                name: name.to_owned(),
                queue: "default".to_owned(),
                payload: "{}".to_owned(),
                status: PG_STATUS_RUNNING.to_owned(),
                attempt,
                max_attempts,
                initial_backoff_ms: 1,
                enqueued_at: None,
                run_at: None,
                started_at: None,
                finished_at: None,
                claimed_by: Some("worker".to_owned()),
                claimed_at: None,
                last_error: None,
                #[cfg(feature = "telemetry-otlp")]
                traceparent: None,
                #[cfg(feature = "telemetry-otlp")]
                tracestate: None,
            }
        }

        #[test]
        fn pg_claim_transition_requires_affected_row() {
            assert!(!pg_claim_transition_applied(0));
            assert!(pg_claim_transition_applied(1));
        }

        #[test]
        fn pg_success_lifecycle_is_skipped_when_ack_does_not_apply() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("slow_success");
            state.job_registry().record_enqueue("slow_success");
            state.job_registry().record_start("slow_success");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("slow_success", serde_json::json!({}), 1, 3);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(!record_pg_lifecycle_ack_result(
                Ok(false),
                "slow_success",
                &job_id,
                "success",
                PgLifecycleRecord::Success,
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["slow_success"].clone();
            assert_eq!(status.in_flight, 0);
            assert_eq!(status.total_successes, 0);
            let snapshot = job_admin.snapshot_sync(&JobAdminQuery::default());
            assert_eq!(snapshot.completed.total, 0);
            assert_eq!(snapshot.running.total, 0);
        }

        #[test]
        fn pg_success_lifecycle_is_recorded_after_ack_applies() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("slow_success");
            state.job_registry().record_enqueue("slow_success");
            state.job_registry().record_start("slow_success");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("slow_success", serde_json::json!({}), 1, 3);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(record_pg_lifecycle_ack_result(
                Ok(true),
                "slow_success",
                &job_id,
                "success",
                PgLifecycleRecord::Success,
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["slow_success"].clone();
            assert_eq!(status.in_flight, 0);
            assert_eq!(status.total_successes, 1);
            let snapshot = job_admin.snapshot_sync(&JobAdminQuery::default());
            assert_eq!(snapshot.completed.total, 1);
            assert_eq!(snapshot.running.total, 0);
        }

        #[test]
        fn pg_terminal_failure_stale_eviction_records_dead_letter() {
            // When a final-attempt job's ack returns Ok(false), stale-claim
            // recovery already dead-lettered the row; total_failures and
            // dead_letters must be incremented to stay in sync with the DB.
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("slow_failure");
            state.job_registry().record_enqueue("slow_failure");
            state.job_registry().record_start("slow_failure");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("slow_failure", serde_json::json!({}), 1, 1);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(!record_pg_lifecycle_ack_result(
                Ok(false),
                "slow_failure",
                &job_id,
                "failure",
                PgLifecycleRecord::Failure {
                    error: "visibility timeout expired"
                },
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["slow_failure"].clone();
            assert_eq!(status.in_flight, 0);
            assert_eq!(
                status.total_failures, 1,
                "terminal stale eviction must increment total_failures"
            );
            assert_eq!(
                status.dead_letters, 1,
                "terminal stale eviction must increment dead_letters"
            );
            let snapshot = job_admin.snapshot_sync(&JobAdminQuery::default());
            assert_eq!(
                snapshot.failed.total, 1,
                "failed list must show the dead-lettered job"
            );
            assert_eq!(snapshot.running.total, 0);
        }

        #[test]
        fn pg_failure_lifecycle_is_recorded_after_ack_applies() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("slow_failure");
            state.job_registry().record_enqueue("slow_failure");
            state.job_registry().record_start("slow_failure");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("slow_failure", serde_json::json!({}), 1, 1);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(record_pg_lifecycle_ack_result(
                Ok(true),
                "slow_failure",
                &job_id,
                "failure",
                PgLifecycleRecord::Failure {
                    error: "worker failed"
                },
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["slow_failure"].clone();
            assert_eq!(status.in_flight, 0);
            assert_eq!(status.total_failures, 1);
            assert_eq!(status.dead_letters, 1);
            let snapshot = job_admin.snapshot_sync(&JobAdminQuery::default());
            assert_eq!(snapshot.failed.total, 1);
            assert_eq!(
                snapshot.failed.records[0].last_error.as_deref(),
                Some("worker failed")
            );
        }

        #[test]
        fn pg_retry_lifecycle_is_recorded_after_ack_applies() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("slow_retry");
            state.job_registry().record_enqueue("slow_retry");
            state.job_registry().record_start("slow_retry");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("slow_retry", serde_json::json!({}), 1, 3);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(record_pg_lifecycle_ack_result(
                Ok(true),
                "slow_retry",
                &job_id,
                "failure",
                PgLifecycleRecord::Retry {
                    error: "try again",
                    attempt: 1,
                },
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["slow_retry"].clone();
            assert_eq!(status.in_flight, 0);
            assert_eq!(status.total_failures, 0);
            assert_eq!(status.last_error.as_deref(), Some("try again"));
            let admin_status = job_admin
                .inner
                .read()
                .expect("job admin lock")
                .records
                .get(&job_id)
                .expect("admin record")
                .status;
            assert_eq!(admin_status, JobAdminStatus::Enqueued);
        }

        #[test]
        fn pg_lifecycle_is_skipped_when_ack_errors() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("slow_success");
            state.job_registry().record_enqueue("slow_success");
            state.job_registry().record_start("slow_success");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("slow_success", serde_json::json!({}), 1, 3);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(!record_pg_lifecycle_ack_result(
                Err(AutumnError::internal_server_error_msg("ack failed")),
                "slow_success",
                &job_id,
                "success",
                PgLifecycleRecord::Success,
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["slow_success"].clone();
            assert_eq!(status.in_flight, 1);
            assert_eq!(status.total_successes, 0);
        }

        #[test]
        fn pg_lifecycle_balances_inflight_on_stale_eviction() {
            // When ack returns Ok(false) the claim was evicted by stale-claim
            // recovery; in_flight must be decremented so metrics don't leak.
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("evicted_job");
            state.job_registry().record_enqueue("evicted_job");
            state.job_registry().record_start("evicted_job");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("evicted_job", serde_json::json!({}), 1, 3);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(!record_pg_lifecycle_ack_result(
                Ok(false),
                "evicted_job",
                &job_id,
                "success",
                PgLifecycleRecord::Success,
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["evicted_job"].clone();
            assert_eq!(
                status.in_flight, 0,
                "in_flight must be balanced after stale eviction"
            );
            assert_eq!(status.total_successes, 0);
            assert_eq!(
                status.last_error.as_deref(),
                Some("visibility timeout expired")
            );
            let admin_status = job_admin
                .inner
                .read()
                .expect("job admin lock")
                .records
                .get(&job_id)
                .expect("admin record")
                .status;
            assert_eq!(admin_status, JobAdminStatus::Retrying);
        }

        #[test]
        fn pg_terminal_stale_eviction_records_failure_and_dead_letter() {
            // When ack returns Ok(false) on a final-attempt job (lifecycle=Failure),
            // stale recovery already dead-lettered the row in the DB.  The
            // in-memory metrics must reflect a dead-letter, not a retry.
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("terminal_job");
            state.job_registry().record_enqueue("terminal_job");
            state.job_registry().record_start("terminal_job");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("terminal_job", serde_json::json!({}), 1, 1);
            job_admin.record_start_for_test(&job_id, 1);

            assert!(!record_pg_lifecycle_ack_result(
                Ok(false),
                "terminal_job",
                &job_id,
                "failure",
                PgLifecycleRecord::Failure {
                    error: "handler timed out"
                },
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["terminal_job"].clone();
            assert_eq!(status.in_flight, 0, "in_flight must be balanced");
            assert_eq!(
                status.total_failures, 1,
                "terminal stale eviction must increment total_failures"
            );
            assert_eq!(
                status.dead_letters, 1,
                "terminal stale eviction must increment dead_letters"
            );
            let admin_status = job_admin
                .inner
                .read()
                .expect("job admin lock")
                .records
                .get(&job_id)
                .expect("admin record")
                .status;
            assert_eq!(
                admin_status,
                JobAdminStatus::Failed,
                "admin must show Failed, not Retrying, after terminal stale eviction"
            );
        }

        #[test]
        fn pg_cancel_lifecycle_waits_for_ack() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("cancel_me");
            state.job_registry().record_enqueue("cancel_me");

            assert!(!record_pg_cancel_after_ack(
                Ok(false),
                "cancel_me",
                "job-1",
                &state
            ));
            assert_eq!(state.job_registry().snapshot()["cancel_me"].queued, 1);

            assert!(record_pg_cancel_after_ack(
                Ok(true),
                "cancel_me",
                "job-1",
                &state
            ));
            assert_eq!(state.job_registry().snapshot()["cancel_me"].queued, 0);
        }

        #[test]
        fn pg_cancel_lifecycle_is_skipped_when_ack_errors() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("cancel_me");
            state.job_registry().record_enqueue("cancel_me");

            assert!(!record_pg_cancel_after_ack(
                Err(AutumnError::internal_server_error_msg("ack failed")),
                "cancel_me",
                "job-1",
                &state
            ));
            assert_eq!(state.job_registry().snapshot()["cancel_me"].queued, 1);
        }

        #[test]
        fn pg_row_lifecycle_uses_row_identity_after_ack_applies() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("row_success");
            state.job_registry().record_enqueue("row_success");
            state.job_registry().record_start("row_success");
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id =
                job_admin.record_enqueue_for_test("row_success", serde_json::json!({}), 1, 3);
            job_admin.record_start_for_test(&job_id, 1);
            let row = pg_test_row(&job_id, "row_success", 1, 3);

            assert!(record_pg_row_lifecycle_ack_result(
                Ok(true),
                &row,
                "success",
                PgLifecycleRecord::Success,
                &state,
                &job_admin
            ));

            let status = state.job_registry().snapshot()["row_success"].clone();
            assert_eq!(status.total_successes, 1);
            let snapshot = job_admin.snapshot_sync(&JobAdminQuery::default());
            assert_eq!(snapshot.completed.records[0].id, job_id);
        }

        #[test]
        fn pg_row_cancel_uses_row_identity_after_ack_applies() {
            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("row_cancel");
            state.job_registry().record_enqueue("row_cancel");
            let row = pg_test_row("row-cancel-1", "row_cancel", 1, 3);

            assert!(record_pg_row_cancel_after_ack(Ok(true), &row, &state));

            assert_eq!(state.job_registry().snapshot()["row_cancel"].queued, 0);
        }

        #[tokio::test]
        async fn pg_start_runtime_without_pool_fails_with_actionable_error() {
            let _guard = global_job_runtime_test_lock().lock().await;
            clear_global_job_client();

            let state = crate::AppState::for_test().with_profile("dev");
            let shutdown = tokio_util::sync::CancellationToken::new();
            let config = crate::config::JobConfig {
                backend: "postgres".to_string(),
                ..Default::default()
            };

            let error = start_runtime(
                vec![JobInfo {
                    name: "test_job".to_string(),
                    max_attempts: 1,
                    initial_backoff_ms: 1,
                    queue: "default".to_string(),
                    uniqueness: None,
                    concurrency: None,
                    handler: |_state, _payload| Box::pin(async move { Ok(()) }),
                }],
                &state,
                &shutdown,
                &config,
            )
            .expect_err("postgres backend must fail when no db pool is configured");

            assert!(
                error
                    .to_string()
                    .contains("jobs.backend=postgres requires a configured database"),
                "unexpected error: {error}"
            );
            assert!(global_job_client().is_none());
            clear_global_job_client();
        }

        // ── Integration tests (Docker required) ───────────────────────────

        fn pg_test_pool(url: &str) -> PgPool {
            use diesel_async::pooled_connection::AsyncDieselConnectionManager;
            use diesel_async::pooled_connection::deadpool::Pool;
            let manager = AsyncDieselConnectionManager::<diesel_async::AsyncPgConnection>::new(url);
            Pool::builder(manager).max_size(4).build().unwrap()
        }

        async fn pg_run_migration(pool: &PgPool) {
            let mut conn = pool.get().await.unwrap();

            let sql1 = include_str!("../migrations/20260513000000_create_job_queue/up.sql");
            for stmt in sql1.split(';') {
                let stmt = stmt.trim();
                if !stmt.is_empty() {
                    diesel::sql_query(stmt).execute(&mut *conn).await.unwrap();
                }
            }

            let sql2 =
                include_str!("../migrations/20260610000000_add_job_uniqueness_concurrency/up.sql");
            for stmt in sql2.split(';') {
                let stmt = stmt.trim();
                if !stmt.is_empty() {
                    diesel::sql_query(stmt).execute(&mut *conn).await.unwrap();
                }
            }

            let sql3 = include_str!("../migrations/20260628000000_add_queue_to_jobs/up.sql");
            for stmt in sql3.split(';') {
                let stmt = stmt.trim();
                if !stmt.is_empty() {
                    diesel::sql_query(stmt).execute(&mut *conn).await.unwrap();
                }
            }
        }

        fn unique_constraints(key: &str, window: JobUniquenessWindow) -> ResolvedJobConstraints {
            ResolvedJobConstraints {
                unique_key: Some(key.to_string()),
                unique_window: Some(window),
                ..ResolvedJobConstraints::default()
            }
        }

        fn limited_constraints(limit: u32, scope: Option<&str>) -> ResolvedJobConstraints {
            ResolvedJobConstraints {
                concurrency_limit: Some(limit),
                concurrency_scope: scope.map(str::to_owned),
                ..ResolvedJobConstraints::default()
            }
        }

        async fn pg_exec(pool: &PgPool, sql: &str) {
            let mut conn = pool.get().await.unwrap();
            diesel::sql_query(sql).execute(&mut *conn).await.unwrap();
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_enqueue_claim_ack_roundtrip() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let job_id = uuid::Uuid::new_v4().to_string();
            pg_enqueue_job(
                &pool,
                job_id.clone(),
                "send_email",
                "default",
                serde_json::json!({ "user_id": 42 }),
                5,
                250,
                &ResolvedJobConstraints::default(),
            )
            .await
            .expect("enqueue should succeed");

            let claimed = pg_claim_next_job(&pool, "test-worker", false, &["default".to_string()])
                .await
                .expect("claim should return a job");

            assert_eq!(claimed.id, job_id);
            assert_eq!(claimed.name, "send_email");
            assert_eq!(claimed.status, PG_STATUS_RUNNING);
            assert_eq!(claimed.attempt, 1);
            assert_eq!(claimed.claimed_by.as_deref(), Some("test-worker"));

            pg_ack_success(&pool, &job_id, "test-worker")
                .await
                .expect("ack should succeed");

            let finished = pg_fetch_by_id(&pool, &job_id)
                .await
                .expect("job should exist after ack");
            assert_eq!(finished.status, PG_STATUS_COMPLETED);
            assert!(finished.finished_at.is_some());
            assert!(finished.claimed_by.is_none());
        }

        // Success metric (#1025): a job enqueued with a future `run_at` is not
        // delivered before its due time (±1s) and is delivered within one poll
        // window after. Crash-restart is modeled by dropping the pool and
        // reconnecting mid-delay — the durable row persists and the job still
        // fires exactly once.
        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_delayed_enqueue_is_not_claimable_until_due_and_survives_restart() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            // Enqueue due ~2s in the future.
            let job_id = uuid::Uuid::new_v4().to_string();
            let due = chrono::Utc::now() + chrono::TimeDelta::seconds(2);
            pg_enqueue_job_at(
                &pool,
                job_id.clone(),
                "send_email",
                "default",
                serde_json::json!({ "user_id": 7 }),
                5,
                250,
                Some(due),
                &ResolvedJobConstraints::default(),
            )
            .await
            .expect("delayed enqueue should succeed");

            // Before the due time: not claimable.
            assert!(
                pg_claim_next_job(&pool, "worker-1", false, &["default".to_string()])
                    .await
                    .is_none(),
                "delayed job must not be claimable before its due time"
            );

            // Simulate a worker/process restart mid-delay: drop the pool and
            // reconnect. The durable row outlives the connection.
            drop(pool);
            let pool = pg_test_pool(&url);
            assert!(
                pg_claim_next_job(&pool, "worker-2", false, &["default".to_string()])
                    .await
                    .is_none(),
                "delayed job must still be invisible right after a restart"
            );

            // After the due time: claimable exactly once, then runs the normal path.
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            let claimed = pg_claim_next_job(&pool, "worker-2", false, &["default".to_string()])
                .await
                .expect("delayed job should be claimable once due");
            assert_eq!(claimed.id, job_id);
            assert_eq!(claimed.attempt, 1);
            assert!(
                pg_claim_next_job(&pool, "worker-2", false, &["default".to_string()])
                    .await
                    .is_none(),
                "a due job must be delivered to exactly one worker"
            );

            pg_ack_success(&pool, &job_id, "worker-2")
                .await
                .expect("ack should succeed");
            let finished = pg_fetch_by_id(&pool, &job_id).await.expect("row exists");
            assert_eq!(finished.status, PG_STATUS_COMPLETED);
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_claim_drains_higher_priority_queue_first() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            // Low-priority work enqueued first, the critical job after it.
            pg_enqueue_job(
                &pool,
                "low-1".to_string(),
                "bulk",
                "low",
                serde_json::json!({}),
                5,
                250,
                &ResolvedJobConstraints::default(),
            )
            .await
            .expect("enqueue low");
            pg_enqueue_job(
                &pool,
                "crit-1".to_string(),
                "urgent",
                "critical",
                serde_json::json!({}),
                5,
                250,
                &ResolvedJobConstraints::default(),
            )
            .await
            .expect("enqueue critical");

            // Strict priority: critical drains before low despite enqueue order,
            // and each row carries (round-trips) its queue.
            let order = [
                "critical".to_string(),
                "default".to_string(),
                "low".to_string(),
            ];
            let first = pg_claim_next_job(&pool, "w1", false, &order)
                .await
                .expect("first claim");
            assert_eq!(first.id, "crit-1");
            assert_eq!(first.queue, "critical");
            let second = pg_claim_next_job(&pool, "w1", false, &order)
                .await
                .expect("second claim");
            assert_eq!(second.id, "low-1");
            assert_eq!(second.queue, "low");
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        #[allow(clippy::await_holding_lock)]
        async fn pg_enqueue_on_conn_circuit_breaker() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let _lock = crate::circuit_breaker::TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::circuit_breaker::global_registry().clear();

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let policy = crate::circuit_breaker::CircuitBreakerPolicy {
                failure_ratio_threshold: 0.5,
                sample_window: Duration::from_secs(10),
                minimum_sample_count: 3,
                open_duration: Duration::from_secs(60),
                half_open_trial_count: 2,
            };
            let breaker =
                crate::circuit_breaker::global_registry().get_or_create("job_queue", policy);

            // Construct a client configured with the postgres backend
            let mut settings = std::collections::HashMap::new();
            settings.insert("send_email".to_string(), JobRuntimeSettings::basic(5, 250));

            let client = JobClient {
                local_sender: None,
                local_coordination: None,
                #[cfg(feature = "redis")]
                redis: None,
                #[cfg(feature = "db")]
                pg_pool: Some(pool.clone()),
                registry: crate::actuator::JobRegistry::new(),
                job_admin: JobAdminMemoryBackend::new_for_test(32),
                default_max_attempts: 1,
                default_initial_backoff_ms: 1000,
                per_job_settings: settings,
                interceptor: None,
                resilience_config: None,
            };

            let mut conn = pool.get().await.unwrap();

            // Run a few successful enqueues on the connection.
            let res = client
                .enqueue_on_conn(
                    "send_email",
                    serde_json::json!({ "user_id": 42 }),
                    &mut conn,
                )
                .await;
            assert!(res.is_ok());
            assert_eq!(
                breaker.state(),
                crate::circuit_breaker::CircuitState::Closed
            );

            // Intentionally terminate the backend to make enqueues fail.
            // Run 3 failing attempts to trip the breaker.
            for _ in 0..3 {
                let mut conn_fail = pool.get().await.unwrap();
                let _ = diesel::sql_query("SELECT pg_terminate_backend(pg_backend_pid())")
                    .execute(&mut conn_fail)
                    .await;
                let res = client
                    .enqueue_on_conn(
                        "send_email",
                        serde_json::json!({ "user_id": 42 }),
                        &mut conn_fail,
                    )
                    .await;
                assert!(res.is_err());
            }

            // Breaker should now be Open!
            assert_eq!(breaker.state(), crate::circuit_breaker::CircuitState::Open);

            // Subsequent enqueues should fail fast without hitting the database connection
            let res = client
                .enqueue_on_conn(
                    "send_email",
                    serde_json::json!({ "user_id": 42 }),
                    &mut conn,
                )
                .await;
            assert!(res.is_err());
            assert!(res.err().unwrap().to_string().contains("circuit breaker"));

            crate::circuit_breaker::global_registry().clear();
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_skip_locked_prevents_double_claim_of_same_job() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            pg_enqueue_job(
                &pool,
                uuid::Uuid::new_v4().to_string(),
                "send_email",
                "default",
                serde_json::json!({}),
                5,
                250,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();

            let order = ["default".to_string()];
            let (claim_a, claim_b) = tokio::join!(
                pg_claim_next_job(&pool, "worker-a", false, &order),
                pg_claim_next_job(&pool, "worker-b", false, &order)
            );

            let both = claim_a.is_some() && claim_b.is_some();
            assert!(!both, "two workers must not claim the same job");
            let one = claim_a.is_some() || claim_b.is_some();
            assert!(one, "at least one worker should claim the job");
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_failure_retries_with_backoff_then_dead_letters() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let job_id = uuid::Uuid::new_v4().to_string();
            pg_enqueue_job(
                &pool,
                job_id.clone(),
                "flaky",
                "default",
                serde_json::json!({}),
                2,
                1,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();

            // Attempt 1: claim and fail
            let job = pg_claim_next_job(&pool, "worker-1", false, &["default".to_string()])
                .await
                .expect("first claim should succeed");
            assert_eq!(job.attempt, 1);
            pg_nack_failure(&pool, &job_id, "worker-1", "first failure", &job, None)
                .await
                .unwrap();

            let after_first = pg_fetch_by_id(&pool, &job_id).await.unwrap();
            assert_eq!(after_first.status, PG_STATUS_ENQUEUED);
            assert_eq!(after_first.attempt, 2);

            // Fast-forward run_at so claim is immediately available
            pg_exec(
                &pool,
                &format!("UPDATE autumn_jobs SET run_at = NOW() WHERE id = '{job_id}'"),
            )
            .await;

            // Attempt 2: claim and fail again (max_attempts = 2 → dead-letter)
            let job2 = pg_claim_next_job(&pool, "worker-1", false, &["default".to_string()])
                .await
                .expect("second claim should succeed");
            assert_eq!(job2.attempt, 2);
            pg_nack_failure(&pool, &job_id, "worker-1", "second failure", &job2, None)
                .await
                .unwrap();

            let final_row = pg_fetch_by_id(&pool, &job_id).await.unwrap();
            assert_eq!(final_row.status, PG_STATUS_FAILED);
            assert_eq!(final_row.last_error.as_deref(), Some("second failure"));
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_stale_claim_requeues_within_visibility_timeout() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let job_id = uuid::Uuid::new_v4().to_string();
            pg_enqueue_job(
                &pool,
                job_id.clone(),
                "crashy",
                "default",
                serde_json::json!({}),
                3,
                1,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();

            let _ = pg_claim_next_job(&pool, "crashed-worker", false, &["default".to_string()])
                .await
                .unwrap();

            // Backdate claimed_at to simulate visibility timeout expiry
            pg_exec(&pool, &format!(
                "UPDATE autumn_jobs SET claimed_at = NOW() - INTERVAL '1 hour' WHERE id = '{job_id}'"
            )).await;

            // Recover stale claims with a 1-second timeout
            pg_recover_stale_claims(&pool, 1_000).await;

            let row = pg_fetch_by_id(&pool, &job_id).await.unwrap();
            assert_eq!(
                row.status, PG_STATUS_ENQUEUED,
                "stale job should be re-enqueued"
            );
            assert_eq!(row.attempt, 2, "attempt should be incremented");
            assert!(row.claimed_by.is_none(), "claim should be cleared");
            assert!(
                row.claimed_at.is_none(),
                "claim timestamp should be cleared"
            );
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_job_admin_snapshot_returns_all_status_groups() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            // Enqueued
            pg_enqueue_job(
                &pool,
                "enq-1".to_string(),
                "digest",
                "default",
                serde_json::json!({}),
                5,
                250,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();

            // Running: enqueue then claim (don't ack)
            pg_enqueue_job(
                &pool,
                "run-1".to_string(),
                "reindex",
                "default",
                serde_json::json!({}),
                5,
                250,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();
            let _ = pg_claim_next_job(&pool, "w1", false, &["default".to_string()]).await;

            // Completed
            pg_enqueue_job(
                &pool,
                "cmp-1".to_string(),
                "send_email",
                "default",
                serde_json::json!({}),
                5,
                250,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();
            // claim must pick up the enqueued one (both enqueued and run-1 compete; run-1 is running)
            // so we need to force a specific claim
            pg_exec(
                &pool,
                "UPDATE autumn_jobs SET run_at = NOW() - INTERVAL '1 second' WHERE id = 'cmp-1'",
            )
            .await;
            let job_c = pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                .await
                .expect("completed job to claim");
            pg_ack_success(&pool, &job_c.id, "w1").await.unwrap();

            // Failed
            pg_enqueue_job(
                &pool,
                "fail-1".to_string(),
                "webhook",
                "default",
                serde_json::json!({}),
                1,
                1,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();
            pg_exec(
                &pool,
                "UPDATE autumn_jobs SET run_at = NOW() - INTERVAL '1 second' WHERE id = 'fail-1'",
            )
            .await;
            let job_f = pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                .await
                .expect("failed job to claim");
            pg_nack_failure(&pool, &job_f.id, "w1", "server down", &job_f, None)
                .await
                .unwrap();

            let backend = PgJobAdminBackend { pool: pool.clone() };
            let snapshot = backend.snapshot(JobAdminQuery::default()).await.unwrap();

            assert!(
                snapshot.enqueued.total >= 1,
                "expected at least one enqueued"
            );
            assert!(snapshot.running.total >= 1, "expected at least one running");
            assert!(
                snapshot.completed.total >= 1,
                "expected at least one completed"
            );
            assert!(snapshot.failed.total >= 1, "expected at least one failed");
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_admin_retry_discard_cancel_operate_correctly() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;
            let backend = PgJobAdminBackend { pool: pool.clone() };

            // --- Retry ---
            pg_enqueue_job(
                &pool,
                "fail-r".to_string(),
                "job",
                "default",
                serde_json::json!({}),
                1,
                1,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();
            let jf = pg_claim_next_job(&pool, "w", false, &["default".to_string()])
                .await
                .unwrap();
            pg_nack_failure(&pool, &jf.id, "w", "boom", &jf, None)
                .await
                .unwrap();

            backend.retry("fail-r").await.expect("retry should succeed");
            let row = pg_fetch_by_id(&pool, "fail-r").await.unwrap();
            assert_eq!(row.status, PG_STATUS_ENQUEUED);
            assert_eq!(row.attempt, 1);

            // --- Discard ---
            pg_enqueue_job(
                &pool,
                "fail-d".to_string(),
                "job",
                "default",
                serde_json::json!({}),
                1,
                1,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();
            pg_exec(
                &pool,
                "UPDATE autumn_jobs SET run_at = NOW() - INTERVAL '1 second' WHERE id = 'fail-d'",
            )
            .await;
            let jd = pg_claim_next_job(&pool, "w", false, &["default".to_string()])
                .await
                .unwrap();
            pg_nack_failure(&pool, &jd.id, "w", "boom", &jd, None)
                .await
                .unwrap();

            backend
                .discard("fail-d")
                .await
                .expect("discard should succeed");
            let row = pg_fetch_by_id(&pool, "fail-d").await.unwrap();
            assert_eq!(row.status, "discarded");

            // --- Cancel ---
            pg_enqueue_job(
                &pool,
                "cancel-c".to_string(),
                "job",
                "default",
                serde_json::json!({}),
                5,
                1,
                &ResolvedJobConstraints::default(),
            )
            .await
            .unwrap();
            backend
                .cancel("cancel-c")
                .await
                .expect("cancel should succeed");
            let row = pg_fetch_by_id(&pool, "cancel-c").await.unwrap();
            assert_eq!(row.status, "discarded");
        }

        /// Helper: fetch a single job row by id for test assertions.
        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_unique_enqueue_coalesces_burst_then_releases_on_completion() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let constraints = unique_constraints("invoice-7", JobUniquenessWindow::Running);
            let first = pg_enqueue_job(
                &pool,
                "uniq-1".to_string(),
                "send_invoice",
                "default",
                serde_json::json!({"invoice": 7}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            let second = pg_enqueue_job(
                &pool,
                "uniq-2".to_string(),
                "send_invoice",
                "default",
                serde_json::json!({"invoice": 7}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            assert_eq!(first, EnqueueOutcome::Queued);
            assert_eq!(
                second,
                EnqueueOutcome::Deduplicated,
                "a burst of two identical unique enqueues must coalesce"
            );

            // Exactly one row exists and exactly one execution happens.
            let row = pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                .await
                .expect("one job");
            assert!(
                pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                    .await
                    .is_none()
            );

            // While running, the key is still held.
            let blocked = pg_enqueue_job(
                &pool,
                "uniq-3".to_string(),
                "send_invoice",
                "default",
                serde_json::json!({"invoice": 7}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            assert_eq!(blocked, EnqueueOutcome::Deduplicated);

            // Success releases the key; the next enqueue is accepted.
            assert!(pg_ack_success(&pool, &row.id, "w1").await.unwrap());
            let after = pg_enqueue_job(
                &pool,
                "uniq-4".to_string(),
                "send_invoice",
                "default",
                serde_json::json!({"invoice": 7}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            assert_eq!(after, EnqueueOutcome::Queued);
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_unique_pending_window_releases_key_when_claimed() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let constraints = unique_constraints("acct-1", JobUniquenessWindow::Pending);
            pg_enqueue_job(
                &pool,
                "pend-1".to_string(),
                "sync_account",
                "default",
                serde_json::json!({}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();

            // While pending the key is held.
            let dup = pg_enqueue_job(
                &pool,
                "pend-2".to_string(),
                "sync_account",
                "default",
                serde_json::json!({}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            assert_eq!(dup, EnqueueOutcome::Deduplicated);

            // Claiming clears the key, so a new enqueue is accepted while the
            // original is still running.
            let claimed = pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                .await
                .expect("claim");
            assert_eq!(claimed.id, "pend-1");
            let while_running = pg_enqueue_job(
                &pool,
                "pend-3".to_string(),
                "sync_account",
                "default",
                serde_json::json!({}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            assert_eq!(while_running, EnqueueOutcome::Queued);
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_unique_ttl_window_dedupes_past_completion_until_expiry() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let constraints = unique_constraints("hourly", JobUniquenessWindow::TtlMs(400));
            pg_enqueue_job(
                &pool,
                "ttl-1".to_string(),
                "rebuild_index",
                "default",
                serde_json::json!({}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            let row = pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                .await
                .expect("claim");
            assert!(pg_ack_success(&pool, &row.id, "w1").await.unwrap());

            // Completed, but still inside the TTL window: coalesced.
            let inside = pg_enqueue_job(
                &pool,
                "ttl-2".to_string(),
                "rebuild_index",
                "default",
                serde_json::json!({}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            assert_eq!(inside, EnqueueOutcome::Deduplicated);

            tokio::time::sleep(Duration::from_millis(500)).await;
            let outside = pg_enqueue_job(
                &pool,
                "ttl-3".to_string(),
                "rebuild_index",
                "default",
                serde_json::json!({}),
                3,
                10,
                &constraints,
            )
            .await
            .unwrap();
            assert_eq!(outside, EnqueueOutcome::Queued);
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_claim_enforces_concurrency_limit_and_frees_slot_on_settle() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let constraints = limited_constraints(1, Some("acct-9"));
            for id in ["cap-1", "cap-2", "cap-3"] {
                pg_enqueue_job(
                    &pool,
                    id.to_string(),
                    "recalculate",
                    "default",
                    serde_json::json!({}),
                    3,
                    10,
                    &constraints,
                )
                .await
                .unwrap();
            }

            // Limit 1: only one claim succeeds even with serialized claims on.
            let first = pg_claim_next_job(&pool, "w1", true, &["default".to_string()])
                .await
                .expect("claim one");
            assert!(
                pg_claim_next_job(&pool, "w2", true, &["default".to_string()])
                    .await
                    .is_none(),
                "second claim must wait for the concurrency slot"
            );

            // Settling the running job frees the slot for the next claim.
            assert!(pg_ack_success(&pool, &first.id, "w1").await.unwrap());
            let second = pg_claim_next_job(&pool, "w2", true, &["default".to_string()])
                .await
                .expect("next claim");
            assert_ne!(second.id, first.id);
            assert!(
                pg_claim_next_job(&pool, "w3", true, &["default".to_string()])
                    .await
                    .is_none()
            );

            // A different scope is not blocked by this group.
            let other_scope = limited_constraints(1, Some("acct-10"));
            pg_enqueue_job(
                &pool,
                "cap-other".to_string(),
                "recalculate",
                "default",
                serde_json::json!({}),
                3,
                10,
                &other_scope,
            )
            .await
            .unwrap();
            let other = pg_claim_next_job(&pool, "w3", true, &["default".to_string()])
                .await
                .expect("other scope");
            assert_eq!(other.id, "cap-other");
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_stale_claim_recovery_frees_unique_key_and_concurrency_slot() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;

            let constraints = ResolvedJobConstraints {
                unique_key: Some("crash-key".to_string()),
                unique_window: Some(JobUniquenessWindow::Running),
                concurrency_limit: Some(1),
                concurrency_scope: None,
            };
            pg_enqueue_job(
                &pool,
                "crash-1".to_string(),
                "crashy",
                "default",
                serde_json::json!({}),
                1,
                10,
                &constraints,
            )
            .await
            .unwrap();

            // Simulate a worker crash: claim and never settle.
            let row = pg_claim_next_job(&pool, "dead-worker", true, &["default".to_string()])
                .await
                .expect("claim");
            assert_eq!(row.id, "crash-1");
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Stale recovery dead-letters the final attempt, which must free
            // both the unique key and the concurrency slot.
            pg_recover_stale_claims(&pool, 10).await;
            let recovered = pg_fetch_by_id(&pool, "crash-1").await.unwrap();
            assert_eq!(recovered.status, "failed");

            let constraints_again = ResolvedJobConstraints {
                unique_key: Some("crash-key".to_string()),
                unique_window: Some(JobUniquenessWindow::Running),
                concurrency_limit: Some(1),
                concurrency_scope: None,
            };
            let outcome = pg_enqueue_job(
                &pool,
                "crash-2".to_string(),
                "crashy",
                "default",
                serde_json::json!({}),
                1,
                10,
                &constraints_again,
            )
            .await
            .unwrap();
            assert_eq!(
                outcome,
                EnqueueOutcome::Queued,
                "a dead worker must not deadlock the unique key"
            );
            assert!(
                pg_claim_next_job(&pool, "w2", true, &["default".to_string()])
                    .await
                    .is_some(),
                "the concurrency slot must be free after stale recovery"
            );
        }

        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn pg_admin_retry_keeps_unique_key_and_conflicts_with_inflight_twin() {
            use testcontainers::runners::AsyncRunner as _;
            use testcontainers_modules::postgres::Postgres;

            let container = Postgres::default().start().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            let pool = pg_test_pool(&url);
            pg_run_migration(&pool).await;
            let backend = PgJobAdminBackend { pool: pool.clone() };

            let constraints = unique_constraints("invoice-3", JobUniquenessWindow::Running);
            pg_enqueue_job(
                &pool,
                "fail-uq".to_string(),
                "send_invoice",
                "default",
                serde_json::json!({}),
                1,
                10,
                &constraints,
            )
            .await
            .unwrap();
            // Dead-letter it: claim, then terminal nack (attempt == max).
            let row = pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                .await
                .expect("claim");
            assert!(
                pg_nack_failure(&pool, &row.id, "w1", "boom", &row, None)
                    .await
                    .unwrap()
            );

            // The key is free after dead-letter, so a twin can be enqueued.
            assert_eq!(
                pg_enqueue_job(
                    &pool,
                    "twin".to_string(),
                    "send_invoice",
                    "default",
                    serde_json::json!({}),
                    1,
                    10,
                    &constraints,
                )
                .await
                .unwrap(),
                EnqueueOutcome::Queued
            );

            // Retrying the failed job while the twin is in flight must be
            // refused — uniqueness is preserved, not silently dropped.
            let error = backend.pg_retry_failed("fail-uq").await.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("an equivalent unique job is already pending or running"),
                "{error}"
            );

            // Once the twin settles, the retry goes through and the retried
            // row still carries its unique key.
            let twin = pg_claim_next_job(&pool, "w1", false, &["default".to_string()])
                .await
                .expect("claim twin");
            assert!(pg_ack_success(&pool, &twin.id, "w1").await.unwrap());
            backend.pg_retry_failed("fail-uq").await.unwrap();
            let retried = pg_fetch_by_id(&pool, "fail-uq").await.unwrap();
            assert_eq!(retried.status, "enqueued");
            // And duplicates coalesce against the retried job again.
            assert_eq!(
                pg_enqueue_job(
                    &pool,
                    "dup".to_string(),
                    "send_invoice",
                    "default",
                    serde_json::json!({}),
                    1,
                    10,
                    &constraints,
                )
                .await
                .unwrap(),
                EnqueueOutcome::Deduplicated
            );
        }

        async fn pg_fetch_by_id(pool: &PgPool, id: &str) -> Option<PgJobRow> {
            use diesel::OptionalExtension as _;
            let mut conn = pool.get().await.unwrap();
            diesel::sql_query(format!(
                "SELECT {PG_JOB_SELECT_COLS} FROM autumn_jobs WHERE id = $1"
            ))
            .bind::<diesel::sql_types::Text, _>(id)
            .get_result::<PgJobRow>(&mut *conn)
            .await
            .optional()
            .unwrap_or(None)
        }
    }

    // ── enqueue_after_commit tests ────────────────────────────────

    fn make_test_client() -> (JobClient, tokio::sync::mpsc::Receiver<QueuedJob>) {
        let (tx, rx) = mpsc::channel(16);
        let client = JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: JobAdminMemoryBackend::new_for_test(32),
            default_max_attempts: 3,
            default_initial_backoff_ms: 100,
            per_job_settings: HashMap::from([(
                "test_job".to_string(),
                JobRuntimeSettings::basic(3, 100),
            )]),
            interceptor: None,
            resilience_config: None,
        };
        (client, rx)
    }

    #[tokio::test]
    async fn enqueue_after_commit_outside_tx_enqueues_immediately() {
        use std::time::Duration;
        let (client, mut rx) = make_test_client();

        client
            .enqueue_after_commit("test_job", serde_json::json!({"x": 1}))
            .await
            .expect("enqueue_after_commit should succeed outside tx");

        // The job should have been enqueued immediately (not deferred)
        let received = timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be received immediately outside tx"
        );
        let job = received.unwrap().expect("channel should not be closed");
        assert_eq!(job.name, "test_job");
    }

    #[tokio::test]
    async fn enqueue_after_commit_inside_scope_defers_enqueue() {
        use crate::db::{AFTER_COMMIT_REGISTRY, CommitCallback};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let (client, mut rx) = make_test_client();
        let registry = Arc::new(Mutex::new(Vec::<CommitCallback>::new()));

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                client
                    .enqueue_after_commit("test_job", serde_json::json!({"x": 2}))
                    .await
                    .expect("enqueue_after_commit should succeed inside scope");
            })
            .await;

        // Job must NOT have been enqueued yet
        let not_received = timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(
            not_received.is_err(),
            "job must not be enqueued before commit"
        );

        // Drain callbacks (simulating commit)
        let callbacks: Vec<CommitCallback> = {
            let mut reg = registry.lock().unwrap();
            std::mem::take(&mut *reg)
        };
        for cb in callbacks {
            cb().await.unwrap();
        }

        // Now the job should appear
        let received = timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be enqueued after commit callbacks run"
        );
        let job = received.unwrap().expect("channel should not be closed");
        assert_eq!(job.name, "test_job");
    }

    // ── W3C Trace Context propagation tests ─────────────────────────────────

    /// Tests in this module verify the trace-context data model and helper
    /// functions introduced to propagate W3C `traceparent` / `tracestate`
    /// across job queue boundaries.  They are gated on `telemetry-otlp`
    /// because the propagation helpers and struct fields are only compiled in
    /// when that feature is enabled.
    #[cfg(feature = "telemetry-otlp")]
    mod trace_propagation {
        use super::*;

        /// Compile-time structural check: `QueuedJob` must expose
        /// `traceparent` and `tracestate` fields when `telemetry-otlp` is
        /// enabled so the in-process queue can carry the W3C context.
        #[test]
        fn queued_job_has_trace_context_fields() {
            let _job = QueuedJob {
                id: "t".to_string(),
                name: "t".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 1,
                initial_backoff_ms: 0,
                traceparent: Some(
                    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_string(),
                ),
                tracestate: None,
            };
        }

        #[test]
        fn restore_job_trace_context_parses_valid_traceparent() {
            use opentelemetry::trace::TraceContextExt as _;
            use opentelemetry_sdk::propagation::TraceContextPropagator;

            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let cx = restore_job_trace_context(
                Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
                None,
            )
            .expect("valid traceparent should parse into an OTel context");

            let span = cx.span();
            let sc = span.span_context();
            assert!(sc.is_valid(), "restored span context must be valid");
            assert_eq!(
                sc.trace_id().to_string(),
                "0af7651916cd43dd8448eb211c80319c",
            );
            assert_eq!(sc.span_id().to_string(), "b7ad6b7169203331");
        }

        #[test]
        fn restore_job_trace_context_returns_none_when_traceparent_absent() {
            use opentelemetry_sdk::propagation::TraceContextPropagator;
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            assert!(
                restore_job_trace_context(None, None).is_none(),
                "absent traceparent must yield None"
            );
        }

        #[test]
        fn restore_job_trace_context_returns_none_for_invalid_traceparent() {
            use opentelemetry_sdk::propagation::TraceContextPropagator;
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            assert!(
                restore_job_trace_context(Some("not-a-real-traceparent"), None).is_none(),
                "malformed traceparent must yield None"
            );
        }

        #[cfg(feature = "redis")]
        #[test]
        fn redis_record_has_trace_context_fields() {
            let _record = RedisJobRecord {
                id: "r".to_string(),
                name: "j".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 1,
                initial_backoff_ms: 0,
                enqueued_at_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                claimed_by: None,
                claimed_at_ms: None,
                last_error: None,
                unique_key: None,
                unique_window: None,
                concurrency_key: None,
                concurrency_limit: None,
                traceparent: Some(
                    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_string(),
                ),
                tracestate: None,
            };
        }

        #[cfg(feature = "redis")]
        #[test]
        fn redis_record_missing_trace_context_deserializes_as_none() {
            let old_json = r#"{"id":"x","name":"y","payload":{},"attempt":1,"max_attempts":3,"initial_backoff_ms":250}"#;
            let record: RedisJobRecord = serde_json::from_str(old_json)
                .expect("old-format record without traceparent must deserialize");
            assert!(
                record.traceparent.is_none(),
                "missing field must default to None"
            );
            assert!(
                record.tracestate.is_none(),
                "missing field must default to None"
            );
        }

        #[cfg(feature = "telemetry-otlp")]
        #[test]
        fn job_map_injector_set_inserts_key_value() {
            use opentelemetry::propagation::Injector as _;
            let mut map = std::collections::HashMap::new();
            let mut injector = JobMapInjector(&mut map);
            injector.set(
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_owned(),
            );
            assert_eq!(
                map.get("traceparent").map(String::as_str),
                Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
            );
        }

        #[cfg(feature = "telemetry-otlp")]
        #[test]
        fn capture_job_trace_context_returns_none_when_no_active_span() {
            use opentelemetry_sdk::propagation::TraceContextPropagator;
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let (tp, ts) = capture_job_trace_context();
            assert!(tp.is_none(), "no traceparent expected without active span");
            assert!(ts.is_none(), "no tracestate expected without active span");
        }

        #[cfg(feature = "telemetry-otlp")]
        #[tokio::test]
        async fn execute_local_job_with_traceparent_restores_context() {
            use opentelemetry_sdk::propagation::TraceContextPropagator;
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

            let state = AppState::for_test().with_profile("dev");
            state.job_registry().register("noop");
            state.job_registry().record_enqueue("noop");

            let mut jobs = HashMap::new();
            jobs.insert(
                "noop".to_string(),
                JobInfo {
                    name: "noop".to_string(),
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    queue: "default".to_string(),
                    uniqueness: None,
                    concurrency: None,
                    handler: |_state, _payload| Box::pin(async { Ok(()) }),
                },
            );
            let jobs_by_name = Arc::new(RwLock::new(jobs));
            let (tx, _rx) = mpsc::channel(1);
            let job_admin = JobAdminMemoryBackend::new_for_test(32);
            let job_id = job_admin.record_enqueue_for_test("noop", serde_json::json!({}), 1, 1);

            execute_local_job(
                QueuedJob {
                    id: job_id,
                    name: "noop".to_string(),
                    queue: "default".to_string(),
                    payload: serde_json::json!({}),
                    attempt: 1,
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    traceparent: Some(
                        "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_string(),
                    ),
                    tracestate: None,
                },
                &jobs_by_name,
                &tx,
                &state,
                &job_admin,
                &Arc::new(LocalJobCoordination::default()),
            )
            .await;

            let snapshot = state.job_registry().snapshot();
            assert_eq!(
                snapshot.get("noop").map(|s| s.total_successes),
                Some(1),
                "job with traceparent must execute successfully"
            );
        }

        #[cfg(feature = "redis")]
        #[test]
        fn redis_record_trace_context_survives_json_roundtrip() {
            use opentelemetry::trace::TraceContextExt as _;
            use opentelemetry_sdk::propagation::TraceContextPropagator;

            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let original = RedisJobRecord {
                id: "r".to_string(),
                name: "j".to_string(),
                queue: "default".to_string(),
                payload: serde_json::json!({}),
                attempt: 1,
                max_attempts: 1,
                initial_backoff_ms: 0,
                enqueued_at_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                claimed_by: None,
                claimed_at_ms: None,
                last_error: None,
                unique_key: None,
                unique_window: None,
                concurrency_key: None,
                concurrency_limit: None,
                traceparent: Some(
                    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_string(),
                ),
                tracestate: None,
            };
            let encoded = serde_json::to_string(&original).expect("encode");
            let decoded: RedisJobRecord = serde_json::from_str(&encoded).expect("decode");
            let cx = restore_job_trace_context(
                decoded.traceparent.as_deref(),
                decoded.tracestate.as_deref(),
            )
            .expect("roundtrip traceparent must restore to a valid context");
            assert_eq!(
                cx.span().span_context().trace_id().to_string(),
                "0af7651916cd43dd8448eb211c80319c",
            );
        }

        #[test]
        fn job_map_extractor_keys_returns_all_keys() {
            use opentelemetry::propagation::Extractor as _;
            let mut map = std::collections::HashMap::new();
            map.insert("traceparent".to_owned(), "00-abc-def-01".to_owned());
            map.insert("tracestate".to_owned(), "vendor=val".to_owned());
            let extractor = JobMapExtractor(&map);
            let mut keys = extractor.keys();
            keys.sort_unstable();
            assert_eq!(keys, vec!["traceparent", "tracestate"]);
        }

        #[test]
        fn restore_job_trace_context_with_tracestate_parses_correctly() {
            use opentelemetry::trace::TraceContextExt as _;
            use opentelemetry_sdk::propagation::TraceContextPropagator;
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let cx = restore_job_trace_context(
                Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
                Some("vendor=value"),
            )
            .expect("valid traceparent with tracestate should parse");
            assert!(cx.span().span_context().is_valid());
        }

        #[test]
        fn capture_job_trace_context_returns_some_when_active_otel_span() {
            use opentelemetry::trace::TracerProvider as _;
            use opentelemetry_sdk::propagation::TraceContextPropagator;
            use opentelemetry_sdk::trace::SdkTracerProvider;
            use tracing_subscriber::prelude::*;

            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let provider = SdkTracerProvider::builder().build();
            let tracer = provider.tracer("test");
            let sub = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(tracer));

            tracing::subscriber::with_default(sub, || {
                let span = tracing::info_span!("capture_test");
                let _guard = span.enter();
                let (tp, _ts) = capture_job_trace_context();
                assert!(
                    tp.is_some(),
                    "traceparent must be Some when an OTel-linked span is active"
                );
            });
        }

        #[test]
        fn enqueue_after_commit_span_is_included_in_queued_job() {
            use opentelemetry_sdk::propagation::TraceContextPropagator;
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let (tx, mut rx) = mpsc::channel(16);
            let client = JobClient {
                local_sender: Some(tx),
                local_coordination: None,
                #[cfg(feature = "redis")]
                redis: None,
                #[cfg(feature = "db")]
                pg_pool: None,
                registry: crate::actuator::JobRegistry::new(),
                job_admin: JobAdminMemoryBackend::new_for_test(32),
                default_max_attempts: 3,
                default_initial_backoff_ms: 100,
                per_job_settings: HashMap::from([(
                    "test_job".to_string(),
                    JobRuntimeSettings::basic(3, 100),
                )]),
                interceptor: None,
                resilience_config: None,
            };
            rt.block_on(async {
                client
                    .enqueue_after_commit("test_job", serde_json::json!({}))
                    .await
                    .expect("outside tx enqueues immediately");
                let job = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                    .await
                    .expect("job should arrive")
                    .expect("channel open");
                assert_eq!(job.name, "test_job");
            });
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_job_enqueue_durable_circuit_breaker() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();
        let policy = crate::circuit_breaker::CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 3,
            open_duration: Duration::from_secs(60),
            half_open_trial_count: 2,
        };
        let breaker = crate::circuit_breaker::global_registry().get_or_create("job_queue", policy);

        // Ensure it is closed initially
        assert_eq!(
            breaker.state(),
            crate::circuit_breaker::CircuitState::Closed
        );

        let client = JobClient {
            local_sender: None,
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: JobAdminMemoryBackend::new_for_test(32),
            default_max_attempts: 1,
            default_initial_backoff_ms: 1000,
            per_job_settings: std::collections::HashMap::new(),
            interceptor: None,
            resilience_config: None,
        };

        for _ in 0..3 {
            let res = client
                .enqueue_durable(
                    "job_id".to_string(),
                    "job_name",
                    "default",
                    serde_json::Value::Null,
                    1,
                    1000,
                    None,
                    &ResolvedJobConstraints::default(),
                )
                .await;
            assert!(res.is_err());
        }

        // Breaker should be Open now!
        assert_eq!(breaker.state(), crate::circuit_breaker::CircuitState::Open);

        let res = client
            .enqueue_durable(
                "job_id".to_string(),
                "job_name",
                "default",
                serde_json::Value::Null,
                1,
                1000,
                None,
                &ResolvedJobConstraints::default(),
            )
            .await;

        assert!(res.is_err());
        let err_str = res.err().unwrap().to_string();
        assert!(
            err_str.contains("circuit breaker")
                || err_str.contains("open")
                || err_str.contains("Open")
        );
        crate::circuit_breaker::global_registry().clear();
    }

    // ── delay_to_when unit tests ──────────────────────────────────────────────

    #[test]
    fn delay_to_when_zero_returns_approximately_now() {
        let before = chrono::Utc::now();
        let result = delay_to_when(std::time::Duration::ZERO);
        let after = chrono::Utc::now();
        assert!(
            result >= before && result <= after,
            "zero delay should resolve to approximately now"
        );
    }

    #[test]
    fn delay_to_when_overflow_returns_max_utc() {
        // u64::MAX seconds overflows i64 nanoseconds in TimeDelta::from_std.
        let huge = std::time::Duration::from_secs(u64::MAX);
        let result = delay_to_when(huge);
        assert_eq!(
            result,
            chrono::DateTime::<chrono::Utc>::MAX_UTC,
            "overflow duration must return MAX_UTC rather than panic"
        );
    }

    #[test]
    fn delay_to_when_small_delay_is_in_the_future() {
        let before = chrono::Utc::now();
        let result = delay_to_when(std::time::Duration::from_secs(60));
        assert!(
            result > before,
            "a positive delay must resolve to the future"
        );
    }

    // ── JobAdminMemoryBackend unit tests ──────────────────────────────────────

    #[test]
    fn record_requeued_returns_true_for_scheduled_job() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let future_due = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        let id = uuid::Uuid::new_v4().to_string();
        backend.record_enqueue_due(
            id.clone(),
            "myjob",
            DEFAULT_QUEUE,
            serde_json::json!({}),
            1,
            3,
            Some(future_due),
            chrono::Utc::now(),
        );

        // Job should be Scheduled.
        let snap = backend.snapshot_sync(&JobAdminQuery::default());
        assert_eq!(snap.scheduled.total, 1, "job should start as Scheduled");

        // Transition back to Enqueued (e.g. promotion from delayed ZSET).
        let was_scheduled = backend.record_requeued(&id, 1);
        assert!(
            was_scheduled,
            "record_requeued must return true for a Scheduled job"
        );

        let snap2 = backend.snapshot_sync(&JobAdminQuery::default());
        assert_eq!(
            snap2.scheduled.total, 0,
            "job should no longer be Scheduled"
        );
        assert_eq!(snap2.enqueued.total, 1, "job should now be Enqueued");
    }

    #[test]
    fn record_requeued_returns_false_for_already_enqueued_job() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("myjob", serde_json::json!({}), 1, 3);

        let was_scheduled = backend.record_requeued(&id, 1);
        assert!(
            !was_scheduled,
            "record_requeued must return false for an Enqueued (not Scheduled) job"
        );
    }

    #[test]
    fn record_requeued_returns_false_for_missing_id() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let was_scheduled = backend.record_requeued("nonexistent-id", 1);
        assert!(
            !was_scheduled,
            "record_requeued must return false for a missing id"
        );
    }

    #[test]
    fn cancel_enqueued_fires_cancellation_token() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("myjob", serde_json::json!({}), 1, 3);

        let token = tokio_util::sync::CancellationToken::new();
        let child = token.child_token();
        backend.register_delay_canceler(id.clone(), token);

        assert!(
            !child.is_cancelled(),
            "token must not be cancelled before cancel_enqueued"
        );
        backend.cancel_enqueued(&id).expect("cancel should succeed");
        assert!(
            child.is_cancelled(),
            "cancel_enqueued must fire the cancellation token"
        );
    }

    #[test]
    fn cancel_enqueued_removes_canceler_from_map() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("myjob", serde_json::json!({}), 1, 3);

        let token = tokio_util::sync::CancellationToken::new();
        backend.register_delay_canceler(id.clone(), token);

        assert!(
            backend
                .inner
                .read()
                .unwrap()
                .delay_cancelers
                .contains_key(&id),
            "canceler must be registered before cancel"
        );

        backend.cancel_enqueued(&id).expect("cancel should succeed");

        assert!(
            !backend
                .inner
                .read()
                .unwrap()
                .delay_cancelers
                .contains_key(&id),
            "cancel_enqueued must remove the delay canceler from the map"
        );
    }

    #[test]
    fn try_record_start_removes_canceler() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("myjob", serde_json::json!({}), 1, 3);

        let token = tokio_util::sync::CancellationToken::new();
        backend.register_delay_canceler(id.clone(), token);

        assert!(
            backend
                .inner
                .read()
                .unwrap()
                .delay_cancelers
                .contains_key(&id),
            "canceler should be registered"
        );

        let decision = backend.try_record_start(&id, 1);
        assert!(
            matches!(decision, JobAdminStartDecision::Started),
            "try_record_start must succeed for an Enqueued job"
        );

        assert!(
            !backend
                .inner
                .read()
                .unwrap()
                .delay_cancelers
                .contains_key(&id),
            "try_record_start must remove the delay canceler"
        );
    }

    #[test]
    fn try_record_start_accepts_scheduled_job() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let future_due = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        let id = uuid::Uuid::new_v4().to_string();
        backend.record_enqueue_due(
            id.clone(),
            "myjob",
            DEFAULT_QUEUE,
            serde_json::json!({}),
            1,
            3,
            Some(future_due),
            chrono::Utc::now(),
        );

        let snap = backend.snapshot_sync(&JobAdminQuery::default());
        assert_eq!(
            snap.scheduled.total, 1,
            "job must be Scheduled before start"
        );

        let decision = backend.try_record_start(&id, 1);
        assert!(
            matches!(decision, JobAdminStartDecision::Started),
            "try_record_start must accept a Scheduled job (timer fired)"
        );
    }

    #[test]
    fn prune_job_admin_history_preserves_scheduled_jobs() {
        // Fill the history beyond its limit with completed jobs, then add a
        // Scheduled job.  The Scheduled job must survive pruning.
        let backend = JobAdminMemoryBackend::new_for_test(3);

        // Fill with completed jobs to trigger pruning.
        for _ in 0..5 {
            let id = backend.record_enqueue_for_test("done", serde_json::json!({}), 1, 1);
            backend.record_start_for_test(&id, 1);
            backend.record_success_for_test(&id);
        }

        // Add a Scheduled (future-due) job.
        let future_due = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        let sched_id = uuid::Uuid::new_v4().to_string();
        backend.record_enqueue_due(
            sched_id.clone(),
            "delayed_job",
            DEFAULT_QUEUE,
            serde_json::json!({}),
            1,
            3,
            Some(future_due),
            chrono::Utc::now(),
        );

        // Force prune by completing another job.
        let id2 = backend.record_enqueue_for_test("done2", serde_json::json!({}), 1, 1);
        backend.record_start_for_test(&id2, 1);
        backend.record_success_for_test(&id2);

        let snap = backend.snapshot_sync(&JobAdminQuery::default());
        assert_eq!(
            snap.scheduled.total, 1,
            "prune_job_admin_history must not evict Scheduled (active) jobs"
        );
        assert!(
            snap.scheduled.records.iter().any(|r| r.id == sched_id),
            "the specific Scheduled job must still be present after pruning"
        );
    }

    #[test]
    fn cancel_enqueued_rejects_running_job() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = backend.record_enqueue_for_test("myjob", serde_json::json!({}), 1, 3);
        backend.record_start_for_test(&id, 1);

        let result = backend.cancel_enqueued(&id);
        assert!(result.is_err(), "canceling a Running job must fail");
    }

    #[test]
    fn register_delay_canceler_returns_true_when_already_canceled() {
        // Simulate the race: admin cancels between record_enqueue_due and
        // register_delay_canceler.
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let future_due = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        let id = uuid::Uuid::new_v4().to_string();
        backend.record_enqueue_due(
            id.clone(),
            "myjob",
            DEFAULT_QUEUE,
            serde_json::json!({}),
            1,
            3,
            Some(future_due),
            chrono::Utc::now(),
        );

        // Admin cancels before the timer is registered.
        backend
            .cancel_enqueued(&id)
            .expect("cancel before token registration must succeed");

        // register_delay_canceler must detect the Canceled status and return true.
        let token = tokio_util::sync::CancellationToken::new();
        let already_canceled = backend.register_delay_canceler(id.clone(), token);
        assert!(
            already_canceled,
            "register_delay_canceler must return true when record is already Canceled"
        );
        // The token must NOT have been stored (cancel already happened).
        assert!(
            !backend
                .inner
                .read()
                .unwrap()
                .delay_cancelers
                .contains_key(&id),
            "token must not be stored for an already-canceled job"
        );
    }

    #[test]
    fn cancel_enqueued_scheduled_job_succeeds() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let future_due = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        let id = uuid::Uuid::new_v4().to_string();
        backend.record_enqueue_due(
            id.clone(),
            "myjob",
            DEFAULT_QUEUE,
            serde_json::json!({}),
            1,
            3,
            Some(future_due),
            chrono::Utc::now(),
        );

        backend
            .cancel_enqueued(&id)
            .expect("canceling a Scheduled job must succeed");

        let snap = backend.snapshot_sync(&JobAdminQuery::default());
        assert_eq!(
            snap.scheduled.total, 0,
            "job must leave Scheduled after cancel"
        );
    }

    // ── enqueue_after_commit_delay tests ─────────────────────────────────────

    #[tokio::test]
    async fn enqueue_after_commit_delay_outside_tx_enqueues_with_delay() {
        use std::time::Duration;
        let (client, mut rx) = make_test_client();

        // Outside any tx scope — should enqueue immediately (bypass after-commit).
        client
            .enqueue_after_commit_delay("test_job", serde_json::json!({"x": 42}), Duration::ZERO)
            .await
            .expect("enqueue_after_commit_delay should succeed outside tx");

        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be delivered when delay is zero and outside tx"
        );
        let job = received.unwrap().expect("channel must not be closed");
        assert_eq!(job.name, "test_job");
    }

    #[tokio::test]
    async fn enqueue_after_commit_delay_inside_scope_defers_and_resolves_at_commit_time() {
        use crate::db::{AFTER_COMMIT_REGISTRY, CommitCallback};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let (client, mut rx) = make_test_client();
        let registry = Arc::new(Mutex::new(Vec::<CommitCallback>::new()));

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                client
                    .enqueue_after_commit_delay(
                        "test_job",
                        serde_json::json!({"x": 99}),
                        Duration::ZERO,
                    )
                    .await
                    .expect("enqueue_after_commit_delay should succeed inside scope");
            })
            .await;

        // Job must NOT have been delivered before commit.
        let not_received = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(
            not_received.is_err(),
            "job must not be delivered before commit callbacks run"
        );

        // Fire commit callbacks (simulating a DB commit).
        let callbacks: Vec<CommitCallback> = {
            let mut reg = registry.lock().unwrap();
            std::mem::take(&mut *reg)
        };
        for cb in callbacks {
            cb().await.unwrap();
        }

        // Now the job should arrive.
        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be delivered after commit callbacks run"
        );
        let job = received.unwrap().expect("channel must not be closed");
        assert_eq!(job.name, "test_job");
    }

    // ── local cancel releases unique lock via CancellationToken ─────────────

    #[tokio::test]
    async fn local_scheduled_cancel_releases_unique_lock_immediately() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "unique_cancelable".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 10,
                queue: "default".to_string(),
                uniqueness: Some(JobUniqueness {
                    by: vec![],
                    window: JobUniquenessWindow::Running,
                }),
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        // Enqueue the first job with a long delay — it holds the unique lock.
        enqueue_in(
            "unique_cancelable",
            serde_json::json!({}),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();

        let admin = job_admin_backend(&state).unwrap();
        let snap = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(snap.scheduled.total, 1, "first job should be Scheduled");
        let id = snap.scheduled.records[0].id.clone();

        // Cancel it — this must fire the CancellationToken and release the unique lock.
        admin.cancel(&id).await.expect("cancel must succeed");

        // Allow the spawned timer task a brief moment to process the cancellation.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now a second enqueue with the same unique key must succeed (not deduplicate).
        let result = enqueue_in(
            "unique_cancelable",
            serde_json::json!({}),
            Duration::from_secs(3600),
        )
        .await;
        assert!(
            result.is_ok(),
            "second enqueue must succeed after unique lock is released by cancel"
        );

        let snap2 = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(
            snap2.scheduled.total, 1,
            "second delayed job should be Scheduled"
        );

        shutdown.cancel();
        clear_global_job_client();
    }

    // ── record_enqueue_due: None/past due → Enqueued status ─────────────────

    #[test]
    fn record_enqueue_due_with_none_due_at_produces_enqueued_status() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = uuid::Uuid::new_v4().to_string();
        backend.record_enqueue_due(
            id,
            "myjob",
            DEFAULT_QUEUE,
            serde_json::json!({}),
            1,
            3,
            None,
            chrono::Utc::now(),
        );
        let snap = backend.snapshot_sync(&JobAdminQuery::default());
        assert_eq!(
            snap.enqueued.total, 1,
            "None due_at must produce Enqueued status"
        );
        assert_eq!(
            snap.scheduled.total, 0,
            "None due_at must not produce Scheduled status"
        );
    }

    #[test]
    fn record_enqueue_due_with_past_due_at_produces_enqueued_status() {
        let backend = JobAdminMemoryBackend::new_for_test(32);
        let id = uuid::Uuid::new_v4().to_string();
        let past = chrono::Utc::now() - chrono::TimeDelta::hours(1);
        backend.record_enqueue_due(
            id,
            "myjob",
            DEFAULT_QUEUE,
            serde_json::json!({}),
            1,
            3,
            Some(past),
            chrono::Utc::now(),
        );
        let snap = backend.snapshot_sync(&JobAdminQuery::default());
        assert_eq!(
            snap.enqueued.total, 1,
            "past due_at must produce Enqueued status"
        );
        assert_eq!(
            snap.scheduled.total, 0,
            "past due_at must not produce Scheduled status"
        );
    }

    // ── enqueue_after_commit_due: None / past / deferred / error ────────────

    #[tokio::test]
    async fn enqueue_after_commit_due_with_none_enqueues_immediately() {
        use std::time::Duration;
        let (client, mut rx) = make_test_client();

        client
            .enqueue_after_commit_due("test_job", serde_json::json!({"x": 10}), None)
            .await
            .expect("enqueue_after_commit_due(None) should succeed outside tx");

        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be delivered immediately when due_at is None"
        );
        assert_eq!(received.unwrap().unwrap().name, "test_job");
    }

    #[tokio::test]
    async fn enqueue_after_commit_due_with_past_enqueues_immediately() {
        use std::time::Duration;
        let (client, mut rx) = make_test_client();

        let past = chrono::Utc::now() - chrono::TimeDelta::hours(1);
        client
            .enqueue_after_commit_due("test_job", serde_json::json!({"x": 20}), Some(past))
            .await
            .expect("enqueue_after_commit_due(Some(past)) should succeed outside tx");

        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be delivered immediately when due_at is in the past"
        );
        assert_eq!(received.unwrap().unwrap().name, "test_job");
    }

    #[tokio::test]
    async fn enqueue_after_commit_due_inside_scope_defers_and_fires() {
        use crate::db::{AFTER_COMMIT_REGISTRY, CommitCallback};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let (client, mut rx) = make_test_client();
        let registry = Arc::new(Mutex::new(Vec::<CommitCallback>::new()));
        let past = chrono::Utc::now() - chrono::TimeDelta::hours(1);

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                client
                    .enqueue_after_commit_due("test_job", serde_json::json!({"x": 30}), Some(past))
                    .await
                    .expect("enqueue_after_commit_due should succeed inside scope");
            })
            .await;

        let not_received = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(
            not_received.is_err(),
            "job must not be delivered before commit callbacks run"
        );

        let callbacks: Vec<CommitCallback> = {
            let mut reg = registry.lock().unwrap();
            std::mem::take(&mut *reg)
        };
        for cb in callbacks {
            cb().await.unwrap();
        }

        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be delivered after commit callbacks run"
        );
        assert_eq!(received.unwrap().unwrap().name, "test_job");
    }

    #[tokio::test]
    async fn enqueue_after_commit_due_rejects_unregistered_job() {
        let (client, _rx) = make_test_client();
        let result = client
            .enqueue_after_commit_due("unregistered_job", serde_json::json!({}), None)
            .await;
        assert!(result.is_err(), "unregistered job must fail eagerly");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not registered"),
            "error must mention 'not registered', got: {msg}"
        );
    }

    // ── module-level enqueue_in_after_commit / enqueue_at_after_commit ───────

    #[tokio::test]
    async fn module_enqueue_in_after_commit_fails_without_global_client() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let result =
            enqueue_in_after_commit("some_job", serde_json::json!({}), std::time::Duration::ZERO)
                .await;
        assert!(
            result.is_err(),
            "enqueue_in_after_commit without global client must fail"
        );

        clear_global_job_client();
    }

    #[tokio::test]
    async fn module_enqueue_at_after_commit_fails_without_global_client() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let when = chrono::Utc::now();
        let result = enqueue_at_after_commit("some_job", serde_json::json!({}), when).await;
        assert!(
            result.is_err(),
            "enqueue_at_after_commit without global client must fail"
        );

        clear_global_job_client();
    }

    #[tokio::test]
    async fn module_enqueue_in_after_commit_delivers_job_outside_tx() {
        use std::time::Duration;
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let (tx, mut rx) = mpsc::channel(16);
        init_global_job_client(JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: JobAdminMemoryBackend::new_for_test(32),
            default_max_attempts: 3,
            default_initial_backoff_ms: 100,
            per_job_settings: HashMap::from([(
                "test_job".to_string(),
                JobRuntimeSettings::basic(3, 100),
            )]),
            interceptor: None,
            resilience_config: None,
        });

        enqueue_in_after_commit("test_job", serde_json::json!({"x": 1}), Duration::ZERO)
            .await
            .expect("enqueue_in_after_commit should succeed outside tx");

        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be delivered immediately with zero delay outside tx"
        );
        assert_eq!(received.unwrap().unwrap().name, "test_job");

        clear_global_job_client();
    }

    #[tokio::test]
    async fn module_enqueue_at_after_commit_delivers_job_outside_tx() {
        use std::time::Duration;
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let (tx, mut rx) = mpsc::channel(16);
        init_global_job_client(JobClient {
            local_sender: Some(tx),
            local_coordination: None,
            #[cfg(feature = "redis")]
            redis: None,
            #[cfg(feature = "db")]
            pg_pool: None,
            registry: crate::actuator::JobRegistry::new(),
            job_admin: JobAdminMemoryBackend::new_for_test(32),
            default_max_attempts: 3,
            default_initial_backoff_ms: 100,
            per_job_settings: HashMap::from([(
                "test_job".to_string(),
                JobRuntimeSettings::basic(3, 100),
            )]),
            interceptor: None,
            resilience_config: None,
        });

        let past = chrono::Utc::now() - chrono::TimeDelta::hours(1);
        enqueue_at_after_commit("test_job", serde_json::json!({"x": 2}), past)
            .await
            .expect("enqueue_at_after_commit should succeed outside tx");

        let received = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            received.is_ok(),
            "job should be delivered immediately for past due time outside tx"
        );
        assert_eq!(received.unwrap().unwrap().name, "test_job");

        clear_global_job_client();
    }
}

#[cfg(test)]
mod uniqueness_concurrency_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Duration;

    /// Poll `cond` every few milliseconds until it holds or `deadline_ms` passes.
    async fn wait_for(deadline_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(deadline_ms);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cond()
    }

    fn unique_job(name: &str, window: JobUniquenessWindow, handler: JobHandler) -> JobInfo {
        JobInfo {
            name: name.to_string(),
            max_attempts: 1,
            initial_backoff_ms: 1,
            queue: "default".to_string(),
            uniqueness: Some(JobUniqueness {
                by: Vec::new(),
                window,
            }),
            concurrency: None,
            handler,
        }
    }

    fn successes(state: &AppState, name: &str) -> u64 {
        state
            .job_registry()
            .snapshot()
            .get(name)
            .map_or(0, |s| s.total_successes)
    }

    fn deduplicated(state: &AppState, name: &str) -> u64 {
        state
            .job_registry()
            .snapshot()
            .get(name)
            .map_or(0, |s| s.total_deduplicated)
    }

    // ── unique key derivation ────────────────────────────────────────────────

    #[test]
    fn default_unique_key_is_stable_for_equal_args_regardless_of_field_order() {
        let uniqueness = JobUniqueness {
            by: Vec::new(),
            window: JobUniquenessWindow::Running,
        };
        let a = serde_json::json!({"x": 1, "y": {"b": 2, "a": [1, 2]}});
        let b = serde_json::json!({"y": {"a": [1, 2], "b": 2}, "x": 1});
        let c = serde_json::json!({"x": 1, "y": {"b": 2, "a": [2, 1]}});
        assert_eq!(
            job_unique_key(&uniqueness, &a),
            job_unique_key(&uniqueness, &b)
        );
        assert_ne!(
            job_unique_key(&uniqueness, &a),
            job_unique_key(&uniqueness, &c)
        );
    }

    #[test]
    fn unique_by_key_uses_selected_fields_only() {
        let uniqueness = JobUniqueness {
            by: vec!["account_id".to_string()],
            window: JobUniquenessWindow::Running,
        };
        let a = serde_json::json!({"account_id": 7, "attempt_marker": "first"});
        let b = serde_json::json!({"account_id": 7, "attempt_marker": "second"});
        let c = serde_json::json!({"account_id": 8, "attempt_marker": "first"});
        assert_eq!(
            job_unique_key(&uniqueness, &a),
            job_unique_key(&uniqueness, &b)
        );
        assert_ne!(
            job_unique_key(&uniqueness, &a),
            job_unique_key(&uniqueness, &c)
        );
    }

    #[test]
    fn unique_by_key_treats_missing_fields_as_null() {
        let uniqueness = JobUniqueness {
            by: vec!["account_id".to_string()],
            window: JobUniquenessWindow::Running,
        };
        let a = serde_json::json!({});
        let b = serde_json::json!({"other": true});
        assert_eq!(
            job_unique_key(&uniqueness, &a),
            job_unique_key(&uniqueness, &b)
        );
    }

    // ── registry counters ────────────────────────────────────────────────────

    #[test]
    fn registry_records_deduplicated_enqueues() {
        let registry = crate::actuator::JobRegistry::new();
        registry.register("dedup_job");
        registry.record_enqueue("dedup_job");
        registry.record_deduplicated("dedup_job");
        let snapshot = registry.snapshot();
        let status = &snapshot["dedup_job"];
        assert_eq!(status.queued, 0);
        assert_eq!(status.total_deduplicated, 1);
    }

    #[test]
    fn registry_tracks_blocked_on_concurrency_gauge() {
        let registry = crate::actuator::JobRegistry::new();
        registry.register("limited");
        registry.record_concurrency_blocked("limited");
        registry.record_concurrency_blocked("limited");
        assert_eq!(registry.snapshot()["limited"].blocked_on_concurrency, 2);
        registry.record_concurrency_unblocked("limited");
        assert_eq!(registry.snapshot()["limited"].blocked_on_concurrency, 1);

        let mut counts = HashMap::new();
        counts.insert("limited".to_string(), 5_u64);
        registry.set_concurrency_blocked_counts(&counts);
        assert_eq!(registry.snapshot()["limited"].blocked_on_concurrency, 5);
        registry.set_concurrency_blocked_counts(&HashMap::new());
        assert_eq!(registry.snapshot()["limited"].blocked_on_concurrency, 0);
    }

    #[test]
    fn deduplicated_admin_status_label_is_stable() {
        assert_eq!(JobAdminStatus::Deduplicated.label(), "deduplicated");
    }

    // ── local backend: uniqueness ────────────────────────────────────────────

    static UNIQUE_BURST_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn unique_burst_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            UNIQUE_BURST_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_unique_job_coalesces_duplicate_burst_enqueues() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        UNIQUE_BURST_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![unique_job(
                "unique_burst",
                JobUniquenessWindow::Running,
                unique_burst_handler,
            )],
            &state,
            &shutdown,
            2,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let payload = serde_json::json!({"invoice_id": 42});
        enqueue("unique_burst", payload.clone()).await.unwrap();
        enqueue("unique_burst", payload.clone()).await.unwrap();

        assert!(
            wait_for(2_000, || successes(&state, "unique_burst") >= 1).await,
            "first execution should complete"
        );
        // Give a would-be duplicate ample time to run.
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(
            UNIQUE_BURST_CALLS.load(Ordering::SeqCst),
            1,
            "burst of two identical enqueues must execute exactly once"
        );
        assert_eq!(deduplicated(&state, "unique_burst"), 1);
        assert_eq!(successes(&state, "unique_burst"), 1);

        shutdown.cancel();
        clear_global_job_client();
    }

    static UNIQUE_RELEASE_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn unique_release_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            UNIQUE_RELEASE_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_unique_key_is_released_on_success() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        UNIQUE_RELEASE_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![unique_job(
                "unique_release",
                JobUniquenessWindow::Running,
                unique_release_handler,
            )],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let payload = serde_json::json!({"invoice_id": 1});
        enqueue("unique_release", payload.clone()).await.unwrap();
        assert!(wait_for(2_000, || successes(&state, "unique_release") == 1).await);

        enqueue("unique_release", payload).await.unwrap();
        assert!(
            wait_for(2_000, || successes(&state, "unique_release") == 2).await,
            "key must be released after success so the job can run again"
        );
        assert_eq!(UNIQUE_RELEASE_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(deduplicated(&state, "unique_release"), 0);

        shutdown.cancel();
        clear_global_job_client();
    }

    static UNIQUE_FAIL_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn unique_fail_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            UNIQUE_FAIL_CALLS.fetch_add(1, Ordering::SeqCst);
            Err(AutumnError::internal_server_error(std::io::Error::other(
                "forced failure",
            )))
        })
    }

    #[tokio::test]
    async fn local_unique_key_is_released_on_terminal_failure() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        UNIQUE_FAIL_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![unique_job(
                "unique_terminal",
                JobUniquenessWindow::Running,
                unique_fail_handler,
            )],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let failures = |state: &AppState| {
            state
                .job_registry()
                .snapshot()
                .get("unique_terminal")
                .map_or(0, |s| s.total_failures)
        };

        let payload = serde_json::json!({"invoice_id": 2});
        enqueue("unique_terminal", payload.clone()).await.unwrap();
        assert!(wait_for(2_000, || failures(&state) == 1).await);

        enqueue("unique_terminal", payload).await.unwrap();
        assert!(
            wait_for(2_000, || failures(&state) == 2).await,
            "key must be released after terminal failure"
        );
        assert_eq!(UNIQUE_FAIL_CALLS.load(Ordering::SeqCst), 2);

        shutdown.cancel();
        clear_global_job_client();
    }

    static UNIQUE_PENDING_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn unique_pending_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            UNIQUE_PENDING_CALLS.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_unique_pending_window_releases_key_when_execution_starts() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        UNIQUE_PENDING_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![unique_job(
                "unique_pending",
                JobUniquenessWindow::Pending,
                unique_pending_handler,
            )],
            &state,
            &shutdown,
            2,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let payload = serde_json::json!({"invoice_id": 3});
        enqueue("unique_pending", payload.clone()).await.unwrap();
        assert!(
            wait_for(2_000, || UNIQUE_PENDING_CALLS.load(Ordering::SeqCst) >= 1).await,
            "first job should start"
        );

        // The original is still running, but the pending window released the
        // key when execution started, so a second enqueue is allowed.
        enqueue("unique_pending", payload).await.unwrap();
        assert!(
            wait_for(2_000, || successes(&state, "unique_pending") == 2).await,
            "second enqueue should run while the first is mid-flight"
        );
        assert_eq!(UNIQUE_PENDING_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(deduplicated(&state, "unique_pending"), 0);

        shutdown.cancel();
        clear_global_job_client();
    }

    static UNIQUE_TTL_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn unique_ttl_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            UNIQUE_TTL_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_unique_ttl_window_dedupes_after_completion_until_expiry() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        UNIQUE_TTL_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![unique_job(
                "unique_ttl",
                JobUniquenessWindow::TtlMs(250),
                unique_ttl_handler,
            )],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let payload = serde_json::json!({"invoice_id": 4});
        enqueue("unique_ttl", payload.clone()).await.unwrap();
        assert!(wait_for(2_000, || successes(&state, "unique_ttl") == 1).await);

        // Inside the TTL window: coalesced even though the first run finished.
        enqueue("unique_ttl", payload.clone()).await.unwrap();
        assert!(wait_for(2_000, || deduplicated(&state, "unique_ttl") == 1).await);

        // After expiry: a fresh enqueue runs.
        tokio::time::sleep(Duration::from_millis(300)).await;
        enqueue("unique_ttl", payload).await.unwrap();
        assert!(wait_for(2_000, || successes(&state, "unique_ttl") == 2).await);
        assert_eq!(UNIQUE_TTL_CALLS.load(Ordering::SeqCst), 2);

        shutdown.cancel();
        clear_global_job_client();
    }

    static UNIQUE_BY_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn unique_by_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            UNIQUE_BY_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_unique_by_scopes_dedup_to_selected_fields() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        UNIQUE_BY_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "unique_by_field".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: Some(JobUniqueness {
                    by: vec!["account_id".to_string()],
                    window: JobUniquenessWindow::Running,
                }),
                concurrency: None,
                handler: unique_by_handler,
            }],
            &state,
            &shutdown,
            2,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        enqueue(
            "unique_by_field",
            serde_json::json!({"account_id": 1, "note": "a"}),
        )
        .await
        .unwrap();
        // Same account, different other fields: coalesced.
        enqueue(
            "unique_by_field",
            serde_json::json!({"account_id": 1, "note": "b"}),
        )
        .await
        .unwrap();
        // Different account: runs.
        enqueue(
            "unique_by_field",
            serde_json::json!({"account_id": 2, "note": "a"}),
        )
        .await
        .unwrap();

        assert!(wait_for(2_000, || successes(&state, "unique_by_field") == 2).await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(UNIQUE_BY_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(deduplicated(&state, "unique_by_field"), 1);

        shutdown.cancel();
        clear_global_job_client();
    }

    // ── local backend: concurrency limits ────────────────────────────────────

    static CONC_CURRENT: AtomicUsize = AtomicUsize::new(0);
    static CONC_MAX: AtomicUsize = AtomicUsize::new(0);
    static CONC_DONE: AtomicUsize = AtomicUsize::new(0);
    fn concurrency_probe_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            let current = CONC_CURRENT.fetch_add(1, Ordering::SeqCst) + 1;
            CONC_MAX.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(40)).await;
            CONC_CURRENT.fetch_sub(1, Ordering::SeqCst);
            CONC_DONE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_concurrency_limit_caps_simultaneous_executions() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        CONC_CURRENT.store(0, Ordering::SeqCst);
        CONC_MAX.store(0, Ordering::SeqCst);
        CONC_DONE.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "recalculate".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: Some(JobConcurrency {
                    limit: 2,
                    key: None,
                }),
                handler: concurrency_probe_handler,
            }],
            &state,
            &shutdown,
            4,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        for marker in 0..6 {
            enqueue("recalculate", serde_json::json!({"marker": marker}))
                .await
                .unwrap();
        }

        assert!(
            wait_for(5_000, || CONC_DONE.load(Ordering::SeqCst) == 6).await,
            "all K > limit jobs must eventually complete; got {}",
            CONC_DONE.load(Ordering::SeqCst)
        );
        assert!(
            CONC_MAX.load(Ordering::SeqCst) <= 2,
            "observed {} simultaneous executions for limit 2",
            CONC_MAX.load(Ordering::SeqCst)
        );
        assert_eq!(successes(&state, "recalculate"), 6);

        shutdown.cancel();
        clear_global_job_client();
    }

    static KEYED_CURRENT_A: AtomicUsize = AtomicUsize::new(0);
    static KEYED_CURRENT_B: AtomicUsize = AtomicUsize::new(0);
    static KEYED_MAX_A: AtomicUsize = AtomicUsize::new(0);
    static KEYED_MAX_B: AtomicUsize = AtomicUsize::new(0);
    static KEYED_DONE: AtomicUsize = AtomicUsize::new(0);
    fn keyed_concurrency_handler(
        _state: AppState,
        payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            let account = payload["account_id"].as_str().unwrap_or("a").to_string();
            let (current, max) = if account == "a" {
                (&KEYED_CURRENT_A, &KEYED_MAX_A)
            } else {
                (&KEYED_CURRENT_B, &KEYED_MAX_B)
            };
            let now = current.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(40)).await;
            current.fetch_sub(1, Ordering::SeqCst);
            KEYED_DONE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_concurrency_key_scopes_limit_per_key_value() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        for counter in [
            &KEYED_CURRENT_A,
            &KEYED_CURRENT_B,
            &KEYED_MAX_A,
            &KEYED_MAX_B,
            &KEYED_DONE,
        ] {
            counter.store(0, Ordering::SeqCst);
        }

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "per_account".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: Some(JobConcurrency {
                    limit: 1,
                    key: Some("account_id".to_string()),
                }),
                handler: keyed_concurrency_handler,
            }],
            &state,
            &shutdown,
            4,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        for marker in 0..2 {
            enqueue(
                "per_account",
                serde_json::json!({"account_id": "a", "marker": marker}),
            )
            .await
            .unwrap();
            enqueue(
                "per_account",
                serde_json::json!({"account_id": "b", "marker": marker}),
            )
            .await
            .unwrap();
        }

        assert!(
            wait_for(5_000, || KEYED_DONE.load(Ordering::SeqCst) == 4).await,
            "all keyed jobs must complete; got {}",
            KEYED_DONE.load(Ordering::SeqCst)
        );
        assert!(KEYED_MAX_A.load(Ordering::SeqCst) <= 1);
        assert!(KEYED_MAX_B.load(Ordering::SeqCst) <= 1);

        shutdown.cancel();
        clear_global_job_client();
    }

    fn keyed_fail_or_slow_handler(
        _state: AppState,
        payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            if payload["mode"] == "fail" {
                return Err(AutumnError::internal_server_error(std::io::Error::other(
                    "forced failure",
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_admin_retry_reports_conflict_when_equivalent_unique_job_is_held() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "retry_conflict".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: Some(JobUniqueness {
                    by: vec!["k".to_string()],
                    window: JobUniquenessWindow::Running,
                }),
                concurrency: None,
                handler: keyed_fail_or_slow_handler,
            }],
            &state,
            &shutdown,
            2,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        // First instance fails terminally, releasing the key.
        enqueue(
            "retry_conflict",
            serde_json::json!({"k": 1, "mode": "fail"}),
        )
        .await
        .unwrap();
        let failures = |state: &AppState| {
            state
                .job_registry()
                .snapshot()
                .get("retry_conflict")
                .map_or(0, |s| s.total_failures)
        };
        assert!(wait_for(2_000, || failures(&state) == 1).await);

        // An equivalent job takes the key and holds it while running slowly.
        enqueue(
            "retry_conflict",
            serde_json::json!({"k": 1, "mode": "slow"}),
        )
        .await
        .unwrap();
        let in_flight = |state: &AppState| {
            state
                .job_registry()
                .snapshot()
                .get("retry_conflict")
                .map_or(0, |s| s.in_flight)
        };
        assert!(wait_for(2_000, || in_flight(&state) == 1).await);

        // Retrying the failed record must report a conflict, not a silent
        // success that queued nothing.
        let admin = job_admin_backend(&state).expect("admin backend");
        let snapshot = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        let failed_id = snapshot.failed.records[0].id.clone();
        let error = admin.retry(&failed_id).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("an equivalent unique job is already pending or running"),
            "{error}"
        );

        // The record is restored to failed so the operator can retry later.
        let snapshot = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        assert_eq!(snapshot.failed.records[0].id, failed_id);

        shutdown.cancel();
        clear_global_job_client();
    }

    static PENDING_RETRY_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn pending_retry_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            if PENDING_RETRY_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(AutumnError::internal_server_error(std::io::Error::other(
                    "first attempt fails",
                )));
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_pending_window_key_is_reacquired_while_retry_waits_out_backoff() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        PENDING_RETRY_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "pending_retry".to_string(),
                max_attempts: 2,
                initial_backoff_ms: 400,
                queue: "default".to_string(),
                uniqueness: Some(JobUniqueness {
                    by: Vec::new(),
                    window: JobUniquenessWindow::Pending,
                }),
                concurrency: None,
                handler: pending_retry_handler,
            }],
            &state,
            &shutdown,
            2,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        let payload = serde_json::json!({"invoice_id": 11});
        enqueue("pending_retry", payload.clone()).await.unwrap();

        // Wait until the first attempt has failed and the retry is scheduled:
        // record_retry stores the error after the pending key is re-acquired.
        let retry_scheduled = |state: &AppState| {
            state
                .job_registry()
                .snapshot()
                .get("pending_retry")
                .is_some_and(|status| status.last_error.is_some())
        };
        assert!(wait_for(2_000, || retry_scheduled(&state)).await);

        // While the retry waits out its backoff the job is pending again, so
        // a duplicate enqueue must coalesce against the re-acquired key.
        enqueue("pending_retry", payload).await.unwrap();
        assert!(
            wait_for(2_000, || deduplicated(&state, "pending_retry") == 1).await,
            "duplicate enqueued during retry backoff must coalesce"
        );

        assert!(wait_for(3_000, || successes(&state, "pending_retry") == 1).await);
        assert_eq!(
            PENDING_RETRY_CALLS.load(Ordering::SeqCst),
            2,
            "exactly the original two attempts run; the duplicate never does"
        );

        shutdown.cancel();
        clear_global_job_client();
    }

    static SLOT_RELEASE_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn slot_release_failing_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            SLOT_RELEASE_CALLS.fetch_add(1, Ordering::SeqCst);
            Err(AutumnError::internal_server_error(std::io::Error::other(
                "forced failure",
            )))
        })
    }

    #[tokio::test]
    async fn local_concurrency_slot_is_released_on_failure() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        SLOT_RELEASE_CALLS.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![JobInfo {
                name: "limited_failing".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: Some(JobConcurrency {
                    limit: 1,
                    key: None,
                }),
                handler: slot_release_failing_handler,
            }],
            &state,
            &shutdown,
            2,
            5,
            250,
            &crate::config::JobQueuesConfig::default(),
        );

        enqueue("limited_failing", serde_json::json!({"marker": 1}))
            .await
            .unwrap();
        enqueue("limited_failing", serde_json::json!({"marker": 2}))
            .await
            .unwrap();

        assert!(
            wait_for(5_000, || SLOT_RELEASE_CALLS.load(Ordering::SeqCst) == 2).await,
            "slot must be released after a failure so the next job runs; got {}",
            SLOT_RELEASE_CALLS.load(Ordering::SeqCst)
        );

        shutdown.cancel();
        clear_global_job_client();
    }

    static PRIO_URGENT_STARTED: AtomicUsize = AtomicUsize::new(0);
    static PRIO_LOW_BEFORE_URGENT: AtomicUsize = AtomicUsize::new(0);
    static PRIO_URGENT_DONE: AtomicUsize = AtomicUsize::new(0);

    fn priority_low_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            if PRIO_URGENT_STARTED.load(Ordering::SeqCst) == 0 {
                PRIO_LOW_BEFORE_URGENT.fetch_add(1, Ordering::SeqCst);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(())
        })
    }

    fn priority_urgent_handler(
        _state: AppState,
        _payload: Value,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
        Box::pin(async move {
            PRIO_URGENT_STARTED.store(1, Ordering::SeqCst);
            PRIO_URGENT_DONE.store(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn local_strict_priority_runs_critical_before_backlog_of_low() {
        let _guard = global_job_runtime_test_lock().lock().await;
        clear_global_job_client();
        PRIO_URGENT_STARTED.store(0, Ordering::SeqCst);
        PRIO_LOW_BEFORE_URGENT.store(0, Ordering::SeqCst);
        PRIO_URGENT_DONE.store(0, Ordering::SeqCst);

        let state = AppState::for_test().with_profile("dev");
        let shutdown = tokio_util::sync::CancellationToken::new();
        start_local_runtime(
            vec![
                JobInfo {
                    name: "bulk".to_string(),
                    max_attempts: 1,
                    initial_backoff_ms: 1,
                    queue: "low".to_string(),
                    uniqueness: None,
                    concurrency: None,
                    handler: priority_low_handler,
                },
                JobInfo {
                    name: "urgent".to_string(),
                    max_attempts: 1,
                    initial_backoff_ms: 1,
                    queue: "critical".to_string(),
                    uniqueness: None,
                    concurrency: None,
                    handler: priority_urgent_handler,
                },
            ],
            &state,
            &shutdown,
            1,
            5,
            250,
            &crate::config::JobQueuesConfig::strict_list(["critical", "default", "low"]),
        );

        // A flood of low-priority work, then a single critical job behind it.
        for marker in 0..200 {
            enqueue("bulk", serde_json::json!({ "marker": marker }))
                .await
                .unwrap();
        }
        enqueue("urgent", serde_json::json!({})).await.unwrap();

        assert!(
            wait_for(5_000, || PRIO_URGENT_DONE.load(Ordering::SeqCst) == 1).await,
            "critical job must run while low backlog remains"
        );
        // Under strict priority a single worker runs at most the one in-flight
        // low job before the critical one — never the whole backlog (FIFO ≈ 200).
        let low_before = PRIO_LOW_BEFORE_URGENT.load(Ordering::SeqCst);
        assert!(
            low_before <= 5,
            "critical job should jump the low backlog; {low_before} low jobs ran first"
        );

        // The admin view surfaces which queue each job is on.
        let admin = job_admin_backend(&state).unwrap();
        let snap = admin.snapshot(JobAdminQuery::default()).await.unwrap();
        let urgent_record = snap
            .completed
            .records
            .iter()
            .find(|r| r.name == "urgent")
            .expect("urgent job present in admin snapshot");
        assert_eq!(urgent_record.queue, "critical");

        shutdown.cancel();
        clear_global_job_client();
    }
}

#[cfg(test)]
mod queue_schedule_tests {
    use super::*;
    use crate::config::JobQueuesConfig;

    #[test]
    fn strict_schedule_drains_high_priority_first_every_iteration() {
        let cfg = JobQueuesConfig::strict_list(["critical", "default", "low"]);
        let schedule = QueueSchedule::from_config(&cfg);
        assert!(schedule.is_strict());
        let mut cursor = schedule.cursor();
        for _ in 0..10 {
            assert_eq!(
                cursor.next_order().as_slice(),
                [
                    "critical".to_string(),
                    "default".to_string(),
                    "low".to_string()
                ]
            );
        }
    }

    #[test]
    fn weighted_schedule_picks_each_queue_proportional_to_weight() {
        // Smooth weighted round-robin: over one full cycle (sum of weights = 7),
        // each queue is the *first* choice exactly `weight` times.
        let cfg = JobQueuesConfig::weighted([("critical", 4), ("default", 2), ("low", 1)]);
        let schedule = QueueSchedule::from_config(&cfg);
        assert!(!schedule.is_strict());
        let mut cursor = schedule.cursor();
        let mut firsts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for _ in 0..7 {
            let order = cursor.next_order();
            // Every iteration still lists *all* queues so a worker never idles
            // while another queue has work.
            assert_eq!(order.len(), 3, "order must include all queues: {order:?}");
            *firsts.entry(order[0].clone()).or_default() += 1;
        }
        assert_eq!(firsts.get("critical").copied(), Some(4));
        assert_eq!(firsts.get("default").copied(), Some(2));
        assert_eq!(firsts.get("low").copied(), Some(1));
    }

    #[test]
    fn weighted_schedule_does_not_starve_low_queue() {
        // No queue may wait indefinitely: each is chosen-first within one cycle.
        let cfg = JobQueuesConfig::weighted([("critical", 100), ("low", 1)]);
        let schedule = QueueSchedule::from_config(&cfg);
        let mut cursor = schedule.cursor();
        let mut saw_low_first = false;
        for _ in 0..101 {
            if cursor.next_order()[0] == "low" {
                saw_low_first = true;
                break;
            }
        }
        assert!(
            saw_low_first,
            "low queue must be served within one weight cycle"
        );
    }

    #[test]
    fn effective_appends_unconfigured_declared_queue_at_lowest_priority() {
        let cfg = JobQueuesConfig::strict_list(["critical", "default"]);
        let declared = vec![
            "critical".to_string(),
            "default".to_string(),
            "ghost".to_string(),
        ];
        let (schedule, warnings) = QueueSchedule::effective(&cfg, &declared);
        assert_eq!(schedule.names(), vec!["critical", "default", "ghost"]);
        assert!(schedule.contains("ghost"));
        assert_eq!(warnings, vec!["ghost".to_string()]);
    }

    #[test]
    fn effective_has_no_warnings_when_all_declared_queues_configured() {
        let cfg = JobQueuesConfig::strict_list(["critical", "default"]);
        let declared = vec!["default".to_string(), "critical".to_string()];
        let (_schedule, warnings) = QueueSchedule::effective(&cfg, &declared);
        assert!(warnings.is_empty());
    }
}
