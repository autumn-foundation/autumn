//! Actuator endpoints for operational observability.
//!
//! Provides health, info, env, metrics, configprops, loggers, and tasks
//! endpoints under the configured actuator prefix.
//!
//! Sensitive endpoints are gated by profile-aware defaults:
//! - **dev**: all endpoints enabled
//! - **prod**: only health, info, and metrics

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

/// Scaffold-level accessibility posture reported by `/actuator/a11y`.
///
/// Each field indicates whether a foundational WCAG 2.1 AA scaffold concern is
/// addressed in the application.  Apps generated with `autumn new` satisfy all
/// three by default; existing apps can opt in incrementally.
#[derive(Debug, Clone, Serialize, Default)]
pub struct A11yPosture {
    /// `<html lang="…">` is set in the page template.
    pub lang_set: bool,
    /// A skip-to-content link is present as the first focusable element.
    pub skip_link_present: bool,
    /// Semantic landmark regions (`<header>`, `<main>`, `<nav>`, `<footer>`)
    /// are used in the page layout.
    pub landmark_regions_present: bool,
}

impl A11yPosture {
    /// Returns `true` when all scaffold-level a11y concerns are addressed.
    #[must_use]
    pub const fn is_compliant(&self) -> bool {
        self.lang_set && self.skip_link_present && self.landmark_regions_present
    }
}

// ── Plugin-contributed metrics ──────────────────────────────────

/// Kind of a Prometheus metric family.
///
/// Used in [`MetricFamily`] to emit the correct `# TYPE` line in the
/// Prometheus text format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricKind {
    /// A monotonically increasing value (e.g., request count).
    Counter,
    /// An arbitrary up-or-down value (e.g., queue depth, active connections).
    Gauge,
}

impl MetricKind {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// A single metric sample with optional label set and a value.
///
/// Labels are `(name, value)` pairs rendered as `{name="value"}` in
/// Prometheus text format.
#[derive(Debug, Clone)]
pub struct MetricSample {
    /// Label key-value pairs. Empty means no labels.
    pub labels: Vec<(String, String)>,
    /// The metric value.
    pub value: f64,
}

/// A complete metric family: name, kind, help text, and current samples.
///
/// Each `MetricFamily` is rendered as one `# HELP` / `# TYPE` block followed
/// by one line per sample in the Prometheus text format.
#[derive(Debug, Clone)]
pub struct MetricFamily {
    /// Unique metric name (e.g., `"harvest_workflow_completions_total"`).
    ///
    /// Use a stable namespace prefix so names don't collide with the built-in
    /// `autumn_*` families or other registered sources.
    pub name: String,
    /// One-line description emitted as `# HELP` in the Prometheus output.
    pub help: String,
    /// Metric type: counter, gauge, or histogram.
    pub kind: MetricKind,
    /// Current samples.  Each sample produces one line in the scrape output.
    pub samples: Vec<MetricSample>,
}

/// Contract for a subsystem that contributes metrics to the unified actuator endpoints.
///
/// Implement this trait and register the implementation via
/// [`crate::app::AppBuilder::metrics_source`] to publish metric families that appear in
/// `/actuator/prometheus` alongside the built-in `autumn_http_*` families, and
/// in `/actuator/metrics` under the `sources` key.
///
/// # Naming rules
///
/// Prefix every metric name with a stable namespace (e.g. `harvest_` for
/// autumn-harvest, `myapp_` for an application-level source).  The registry
/// enforces that two sources cannot share the same **registration name**; metric
/// family name uniqueness is the source's responsibility.
///
/// # Sync-snapshot contract
///
/// `collect` is called synchronously on the HTTP request goroutine.
/// Implementations **must not block on I/O** — read from atomics,
/// `RwLock`-protected snapshots, or channels that already have buffered data.
/// If async work is needed, collect it into a pre-computed cache and update
/// that cache from a background task.
pub trait MetricsSource: Send + Sync + 'static {
    /// Return zero or more metric families, all read from in-memory state.
    fn collect(&self) -> Vec<MetricFamily>;
}

/// Registry of named [`MetricsSource`] implementations.
///
/// Maintained by [`crate::app::AppBuilder`] and stored on
/// [`crate::AppState`]. Provides duplicate-registration detection at startup
/// and per-source panic isolation at scrape time.
#[derive(Clone, Default)]
pub struct MetricsSourceRegistry {
    inner: Arc<RwLock<MetricsSourceRegistryInner>>,
}

#[derive(Default)]
struct MetricsSourceRegistryInner {
    /// Registered sources in insertion order.
    sources: Vec<(String, Arc<dyn MetricsSource>)>,
    /// Per-source scrape-error counter incremented when a source panics.
    error_counts: HashMap<String, u64>,
}

impl MetricsSourceRegistry {
    /// Create a new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a named source.
    ///
    /// Returns `Err` containing a message if a source with `name` has already
    /// been registered (startup-time collision detection).
    ///
    /// # Errors
    ///
    /// Returns an error string when `name` is already registered.
    pub fn register(
        &self,
        name: impl Into<String>,
        source: Arc<dyn MetricsSource>,
    ) -> Result<(), String> {
        let name = name.into();
        {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.sources.iter().any(|(n, _)| n == &name) {
                return Err(format!(
                    "MetricsSource '{name}' is already registered; skipping duplicate"
                ));
            }
            inner.sources.push((name, source));
        }
        Ok(())
    }

    /// Collect from all registered sources, isolating panics.
    ///
    /// Returns one entry per registered source; panicking sources contribute an
    /// empty `Vec<MetricFamily>` and increment their error counter.
    pub fn collect_all(&self) -> Vec<(String, Vec<MetricFamily>)> {
        let sources: Vec<(String, Arc<dyn MetricsSource>)> = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sources
            .clone();

        let mut results = Vec::with_capacity(sources.len());
        let mut panicked = Vec::new();

        for (name, source) in &sources {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.collect()));
            if let Ok(families) = result {
                results.push((name.clone(), families));
            } else {
                tracing::error!(source_name = %name, "MetricsSource panicked during collection");
                panicked.push(name.clone());
                results.push((name.clone(), vec![]));
            }
        }

        if !panicked.is_empty() {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for name in panicked {
                *inner.error_counts.entry(name).or_insert(0) += 1;
            }
        }

        results
    }

    /// Current per-source scrape-error counts (incremented on panic isolation).
    #[must_use]
    pub fn error_counts(&self) -> HashMap<String, u64> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error_counts
            .clone()
    }

    /// Names of all registered sources, in insertion order.
    #[must_use]
    pub fn source_names(&self) -> Vec<String> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sources
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Returns `true` when no sources have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sources
            .is_empty()
    }
}

/// Trait to abstract the state requirements for actuator handlers.
///
/// Implement this trait on your application's state type to provide
/// the necessary dependencies for actuator endpoints (e.g. `/actuator/metrics`).
/// This avoids tight coupling between the actuator middleware and the specific `AppState`.
pub trait ProvideActuatorState {
    /// Returns a reference to the [`crate::middleware::MetricsCollector`]
    /// tracking current HTTP traffic metrics.
    fn metrics(&self) -> &crate::middleware::MetricsCollector;

    /// Returns a reference to the dynamic [`LogLevels`] configuration
    /// allowing runtime adjustment of `tracing` filters.
    fn log_levels(&self) -> &LogLevels;

    /// Returns a reference to the [`TaskRegistry`] holding status and metadata
    /// for async scheduled background tasks.
    fn task_registry(&self) -> &TaskRegistry;

    /// Returns a reference to the [`JobRegistry`] holding queue and failure
    /// information for ad-hoc background jobs.
    fn job_registry(&self) -> &JobRegistry;

    /// Returns a reference to the [`ConfigProperties`] snapshot, providing
    /// active configuration state for the environment endpoint.
    fn config_props(&self) -> &ConfigProperties;

    /// Returns the currently active execution profile (e.g. "dev", "prod")
    /// which modifies what sensitive endpoints are exposed.
    fn profile(&self) -> &str;

    /// Returns a human-readable string displaying how long the application
    /// has been running (e.g., "2d 4h 13m").
    fn uptime_display(&self) -> String;

    /// Returns a reference to the system [`crate::channels::Channels`] which
    /// broadcasts operational events to WebSocket streams.
    #[cfg(feature = "ws")]
    fn channels(&self) -> &crate::channels::Channels;

    /// Returns the main cancellation token that triggers a graceful framework shutdown.
    #[cfg(feature = "ws")]
    fn shutdown_token(&self) -> tokio_util::sync::CancellationToken;

    /// Returns an optional reference to the database connection pool,
    /// used to expose database connection metrics in the `/actuator/metrics` endpoint.
    #[cfg(feature = "db")]
    fn pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>>;

    /// Returns the configured shard set, used to expose per-shard pool
    /// metrics in the `/actuator/metrics` endpoint. Defaults to `None`.
    #[cfg(feature = "db")]
    fn shards(&self) -> Option<&crate::sharding::ShardSet> {
        None
    }

    /// Returns the scaffold-level accessibility posture reported by `/actuator/a11y`.
    ///
    /// Override this in your `AppState` implementation to declare which
    /// WCAG 2.1 AA scaffold concerns your application addresses.  The default
    /// returns all-false (no concerns addressed) — a conservative safe default.
    fn a11y_posture(&self) -> A11yPosture {
        A11yPosture::default()
    }

    /// Returns the registry of plugin-contributed [`MetricsSource`] implementations.
    ///
    /// The default returns `None`, meaning no plugin sources are consulted.
    /// [`crate::AppState`] overrides this to return its registry, which is
    /// populated by [`crate::app::AppBuilder::metrics_source`].
    fn metrics_source_registry(&self) -> Option<&MetricsSourceRegistry> {
        None
    }

    /// Returns the registry of [`HealthIndicator`] implementations.
    ///
    /// The default returns `None`, meaning no custom indicators are consulted.
    /// [`crate::AppState`] overrides this to return its registry, which is
    /// populated by [`crate::app::AppBuilder::health_indicator`].
    fn health_indicator_registry(&self) -> Option<&HealthIndicatorRegistry> {
        None
    }

    /// Returns whether detailed health information should be included in responses.
    ///
    /// When `false`, per-component `details` maps are omitted from
    /// `/actuator/health` output. Defaults to `true`.
    fn health_detailed(&self) -> bool {
        true
    }

    /// Returns the deploy-version label for this replica (e.g. `"stable"` or
    /// `"canary"`), used to tag Prometheus metrics so a canary controller can
    /// compare canary vs. stable cohorts.
    ///
    /// Defaults to [`crate::canary::STABLE`]. [`crate::AppState`] overrides this
    /// to return the value resolved from `AUTUMN_DEPLOY_VERSION` /
    /// `AUTUMN_CANARY` (see [`crate::canary`]).
    fn deploy_version(&self) -> String {
        crate::canary::STABLE.to_owned()
    }

    #[cfg(feature = "http-client")]
    /// Returns the optional webhook outbound manager if enabled/registered.
    fn webhook_outbound(&self) -> Option<crate::webhook_outbound::WebhookOutboundManager> {
        None
    }

    /// Returns the in-memory log capture buffer, if capture is enabled.
    ///
    /// The default returns `None` (capture disabled). [`crate::AppState`]
    /// overrides this to return the buffer installed at startup when
    /// `log.capture.enabled = true`.
    fn log_buffer(&self) -> Option<crate::log::capture::LogBuffer> {
        None
    }
}

// ── Shared types for AppState ──────────────────────────────────

/// Runtime log level management for the loggers actuator endpoint.
///
/// Stores the current effective log level and per-logger overrides.
/// Changes are ephemeral -- they reset on restart.
#[derive(Clone)]
pub struct LogLevels {
    inner: Arc<RwLock<LogLevelsInner>>,
}

struct LogLevelsInner {
    /// The current global log level.
    current_level: String,
    /// Per-logger level overrides applied at runtime.
    logger_overrides: HashMap<String, String>,
}

impl LogLevels {
    /// Create a new `LogLevels` with the given initial level.
    #[must_use]
    pub fn new(initial_level: &str) -> Self {
        Self {
            inner: Arc::new(RwLock::new(LogLevelsInner {
                current_level: initial_level.to_string(),
                logger_overrides: HashMap::new(),
            })),
        }
    }

    /// Get the current global log level.
    #[must_use]
    pub fn current_level(&self) -> String {
        self.inner
            .read()
            .map_or_else(|_| "info".to_string(), |guard| guard.current_level.clone())
    }

    /// Get all per-logger overrides.
    #[must_use]
    pub fn logger_overrides(&self) -> HashMap<String, String> {
        self.inner
            .read()
            .map(|guard| guard.logger_overrides.clone())
            .unwrap_or_default()
    }

    /// Set the level for a specific logger. Returns the previous level if any.
    #[must_use]
    pub fn set_logger_level(&self, name: &str, level: &str) -> Option<String> {
        let Ok(mut guard) = self.inner.write() else {
            return None;
        };
        // Prevent unbounded memory growth from arbitrary logger names
        if guard.logger_overrides.len() >= 1000 && !guard.logger_overrides.contains_key(name) {
            return None;
        }

        let previous = guard.logger_overrides.get(name).cloned();
        guard
            .logger_overrides
            .insert(name.to_string(), level.to_string());
        // If setting the root level, update current_level too
        if name == "root" || name.is_empty() {
            let prev = Some(guard.current_level.clone());
            guard.current_level = level.to_string();
            return prev;
        }
        previous
    }
}

impl std::fmt::Debug for LogLevels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogLevels")
            .field("current_level", &self.current_level())
            .finish()
    }
}

/// Scheduled task status information.
#[derive(Debug, Clone, Serialize)]
pub struct TaskStatus {
    /// The schedule description (e.g., "every 5m" or "cron 0 0 * * *").
    pub schedule: String,
    /// Whether this task is coordinated across the fleet or per replica.
    pub coordination: crate::task::TaskCoordination,
    /// Scheduler backend currently coordinating this task.
    pub scheduler_backend: String,
    /// Replica id for this process.
    pub replica_id: String,
    /// Replica id that last acquired leadership for this task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_leader: Option<String>,
    /// Last global tick key observed for this task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tick: Option<String>,
    /// Last time this task fired (ISO 8601), if ever.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    /// Next scheduled run time (ISO 8601), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    /// Current task state.
    pub status: String,
    /// Last time the task ran (ISO 8601), if ever.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<String>,
    /// Duration of last run in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    /// Result of last run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    /// Last error message, if the task failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Total number of times the task has run.
    pub total_runs: u64,
    /// Total number of failures.
    pub total_failures: u64,
}

/// Registry of scheduled tasks and their runtime status.
#[derive(Clone)]
pub struct TaskRegistry {
    inner: Arc<RwLock<HashMap<String, TaskStatus>>>,
}

/// On-demand background job status information.
#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    /// Approximate queued jobs waiting to run.
    pub queued: u64,
    /// Number of currently running jobs.
    pub in_flight: u64,
    /// Approximate jobs currently waiting on a free concurrency slot.
    pub blocked_on_concurrency: u64,
    /// Total successful executions.
    pub total_successes: u64,
    /// Total failed executions.
    pub total_failures: u64,
    /// Total dead-lettered executions.
    pub dead_letters: u64,
    /// Total enqueues coalesced because a matching unique job was already held.
    pub total_deduplicated: u64,
    /// Last observed error for this job, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl JobStatus {
    const fn empty() -> Self {
        Self {
            queued: 0,
            in_flight: 0,
            blocked_on_concurrency: 0,
            total_successes: 0,
            total_failures: 0,
            dead_letters: 0,
            total_deduplicated: 0,
            last_error: None,
        }
    }
}

/// Registry of ad-hoc jobs and their runtime status.
#[derive(Clone)]
pub struct JobRegistry {
    inner: Arc<RwLock<HashMap<String, JobStatus>>>,
}

impl JobRegistry {
    /// Create a new empty job registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a job name with initial counters.
    pub fn register(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write() {
            guard.entry(name.to_string()).or_insert(JobStatus::empty());
        }
    }

    /// Record that a new job instance was enqueued.
    pub fn record_enqueue(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write() {
            let status = guard.entry(name.to_string()).or_insert(JobStatus::empty());
            status.queued = status.queued.saturating_add(1);
        }
    }

    /// Record that an enqueue was coalesced into an existing unique job.
    ///
    /// Reverses the `record_enqueue` bookkeeping for the coalesced instance
    /// and bumps the deduplication counter.
    pub fn record_deduplicated(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.queued = status.queued.saturating_sub(1);
            status.total_deduplicated = status.total_deduplicated.saturating_add(1);
        }
    }

    /// Record that a job is parked waiting on a free concurrency slot.
    pub fn record_concurrency_blocked(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.blocked_on_concurrency = status.blocked_on_concurrency.saturating_add(1);
        }
    }

    /// Record that a parked job was released back to the queue.
    pub fn record_concurrency_unblocked(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.blocked_on_concurrency = status.blocked_on_concurrency.saturating_sub(1);
        }
    }

    /// Replace the blocked-on-concurrency gauges from a backend-wide survey.
    ///
    /// Names absent from `counts` are reset to zero. Used by the durable
    /// backends whose blocked set is observed periodically rather than
    /// tracked per event.
    pub fn set_concurrency_blocked_counts(&self, counts: &HashMap<String, u64>) {
        if let Ok(mut guard) = self.inner.write() {
            for (name, status) in guard.iter_mut() {
                status.blocked_on_concurrency = counts.get(name).copied().unwrap_or(0);
            }
        }
    }

    /// Record that a queued job started execution.
    pub fn record_start(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.queued = status.queued.saturating_sub(1);
            status.in_flight = status.in_flight.saturating_add(1);
        }
    }

    /// Record that a queued job was canceled before execution.
    pub fn record_cancel(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.queued = status.queued.saturating_sub(1);
        }
    }

    /// Record a successful execution.
    pub fn record_success(&self, name: &str) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.in_flight = status.in_flight.saturating_sub(1);
            status.total_successes = status.total_successes.saturating_add(1);
            status.last_error = None;
        }
    }

    /// Record a retriable failure.
    pub fn record_retry(&self, name: &str, error: &str, _attempt: u32) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.in_flight = status.in_flight.saturating_sub(1);
            status.last_error = Some(error.to_string());
        }
    }

    /// Record a terminal failure.
    pub fn record_failure(&self, name: &str, error: String, dead_lettered: bool) {
        if let Ok(mut guard) = self.inner.write()
            && let Some(status) = guard.get_mut(name)
        {
            status.in_flight = status.in_flight.saturating_sub(1);
            status.total_failures = status.total_failures.saturating_add(1);
            status.last_error = Some(error);
            if dead_lettered {
                status.dead_letters = status.dead_letters.saturating_add(1);
            }
        }
    }

    /// Snapshot all registered jobs.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, JobStatus> {
        self.inner.read().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskRegistry {
    /// Create a new empty task registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a task with its schedule description.
    pub fn register(&self, name: &str, schedule: &str) {
        self.register_scheduled(
            name,
            schedule,
            crate::task::TaskCoordination::Fleet,
            "in_process",
            "unknown",
        );
    }

    /// Register a scheduled task with scheduler coordination metadata.
    pub fn register_scheduled(
        &self,
        name: &str,
        schedule: &str,
        coordination: crate::task::TaskCoordination,
        scheduler_backend: &str,
        replica_id: &str,
    ) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        guard.insert(
            name.to_string(),
            TaskStatus {
                schedule: schedule.to_string(),
                coordination,
                scheduler_backend: scheduler_backend.to_string(),
                replica_id: replica_id.to_string(),
                current_leader: None,
                last_tick: None,
                last_fired_at: None,
                next_run_at: None,
                status: "idle".to_string(),
                last_run: None,
                last_duration_ms: None,
                last_result: None,
                last_error: None,
                total_runs: 0,
                total_failures: 0,
            },
        );
    }

    /// Record the replica that acquired leadership for a global task tick.
    pub fn record_leader(&self, name: &str, leader_id: &str, tick_key: &str) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        let Some(task) = guard.get_mut(name) else {
            return;
        };
        task.current_leader = Some(leader_id.to_string());
        task.last_tick = Some(tick_key.to_string());
    }

    /// Record that a task started running.
    pub fn record_start(&self, name: &str) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        let Some(task) = guard.get_mut(name) else {
            return;
        };
        task.status = "running".to_string();
        task.next_run_at = None;
    }

    /// Record the next scheduled run time for an idle task.
    pub fn record_next_run_at(&self, name: &str, next_run_at: &str) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        let Some(task) = guard.get_mut(name) else {
            return;
        };
        task.next_run_at = Some(next_run_at.to_string());
    }

    /// Record that a task completed successfully.
    pub fn record_success(&self, name: &str, duration_ms: u64) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        let Some(task) = guard.get_mut(name) else {
            return;
        };
        task.status = "idle".to_string();
        let now = chrono::Utc::now().to_rfc3339();
        task.last_run = Some(now.clone());
        task.last_fired_at = Some(now);
        task.last_duration_ms = Some(duration_ms);
        task.last_result = Some("ok".to_string());
        task.last_error = None;
        task.total_runs += 1;
    }

    /// Record that a task failed.
    pub fn record_failure(&self, name: &str, duration_ms: u64, error: &str) {
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        let Some(task) = guard.get_mut(name) else {
            return;
        };
        task.status = "idle".to_string();
        let now = chrono::Utc::now().to_rfc3339();
        task.last_run = Some(now.clone());
        task.last_fired_at = Some(now);
        task.last_duration_ms = Some(duration_ms);
        task.last_result = Some("failed".to_string());
        task.last_error = Some(error.to_string());
        task.total_runs += 1;
        task.total_failures += 1;
    }

    /// Get a snapshot of all task statuses.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, TaskStatus> {
        self.inner
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TaskRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskRegistry")
            .field("count", &self.snapshot().len())
            .finish()
    }
}

/// Resolved config property with source provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigProperty {
    /// The resolved value (redacted if sensitive).
    pub value: serde_json::Value,
    /// Where the value came from.
    pub source: String,
}

/// Collection of resolved config properties with source tracking.
#[derive(Debug, Clone, Default)]
pub struct ConfigProperties {
    inner: Arc<RwLock<HashMap<String, ConfigProperty>>>,
}

impl ConfigProperties {
    /// Build config properties with source tracking from the loaded config.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn from_config(config: &crate::config::AutumnConfig) -> Self {
        let profile = config.profile.as_deref().unwrap_or("default");
        let defaults = crate::config::AutumnConfig::default();

        // Avoids dynamic reallocation since we know roughly how many config properties are tracked.
        let mut props = HashMap::with_capacity(32);
        let profile_str = profile.to_string();

        Self::track_server_props(&mut props, config, &defaults, &profile_str);
        Self::track_db_props(&mut props, config, &defaults, &profile_str);
        Self::track_log_props(&mut props, config, &defaults, &profile_str);
        Self::track_telemetry_props(&mut props, config, &defaults, &profile_str);
        Self::track_health_props(&mut props, config, &defaults, &profile_str);
        Self::track_actuator_props(&mut props, config, &defaults, &profile_str);
        Self::track_session_props(&mut props, config, &defaults, &profile_str);
        Self::track_channels_props(&mut props, config, &defaults, &profile_str);

        Self {
            inner: Arc::new(RwLock::new(props)),
        }
    }

    fn track_server_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        Self::track_property(
            props,
            "server.host",
            &config.server.host,
            &defaults.server.host,
            profile_str,
        );
        Self::track_property(
            props,
            "server.port",
            &config.server.port.to_string(),
            &defaults.server.port.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "server.shutdown_timeout_secs",
            &config.server.shutdown_timeout_secs.to_string(),
            &defaults.server.shutdown_timeout_secs.to_string(),
            profile_str,
        );
    }

    fn track_db_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        let db_url = config.database.url.as_deref().unwrap_or("").to_string();
        let primary_url = config
            .database
            .primary_url
            .as_deref()
            .unwrap_or("")
            .to_string();
        let replica_url = config
            .database
            .replica_url
            .as_deref()
            .unwrap_or("")
            .to_string();
        Self::track_property(props, "database.url", &db_url, "", profile_str);
        Self::track_property(props, "database.primary_url", &primary_url, "", profile_str);
        Self::track_property(props, "database.replica_url", &replica_url, "", profile_str);
        Self::track_property(
            props,
            "database.pool_size",
            &config.database.pool_size.to_string(),
            &defaults.database.pool_size.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "database.primary_pool_size",
            &config.database.effective_primary_pool_size().to_string(),
            &defaults.database.effective_primary_pool_size().to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "database.replica_pool_size",
            &config.database.effective_replica_pool_size().to_string(),
            &defaults.database.effective_replica_pool_size().to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "database.replica_fallback",
            &format!("{:?}", config.database.replica_fallback),
            &format!("{:?}", defaults.database.replica_fallback),
            profile_str,
        );
    }

    fn track_log_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        Self::track_property(
            props,
            "log.level",
            &config.log.level,
            &defaults.log.level,
            profile_str,
        );
        Self::track_property(
            props,
            "log.format",
            &format!("{:?}", config.log.format),
            &format!("{:?}", defaults.log.format),
            profile_str,
        );
        Self::track_property(
            props,
            "log.capture.enabled",
            &config.log.capture.enabled.to_string(),
            &defaults.log.capture.enabled.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "log.capture.capacity",
            &config.log.capture.capacity.to_string(),
            &defaults.log.capture.capacity.to_string(),
            profile_str,
        );
    }

    fn track_telemetry_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        Self::track_property(
            props,
            "telemetry.enabled",
            &config.telemetry.enabled.to_string(),
            &defaults.telemetry.enabled.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "telemetry.service_name",
            &config.telemetry.service_name,
            &defaults.telemetry.service_name,
            profile_str,
        );
        Self::track_property(
            props,
            "telemetry.service_namespace",
            config.telemetry.service_namespace.as_deref().unwrap_or(""),
            defaults
                .telemetry
                .service_namespace
                .as_deref()
                .unwrap_or(""),
            profile_str,
        );
        Self::track_property(
            props,
            "telemetry.service_version",
            &config.telemetry.service_version,
            &defaults.telemetry.service_version,
            profile_str,
        );
        Self::track_property(
            props,
            "telemetry.environment",
            &config.telemetry.environment,
            &defaults.telemetry.environment,
            profile_str,
        );
        Self::track_property(
            props,
            "telemetry.otlp_endpoint",
            config.telemetry.otlp_endpoint.as_deref().unwrap_or(""),
            defaults.telemetry.otlp_endpoint.as_deref().unwrap_or(""),
            profile_str,
        );
        Self::track_property(
            props,
            "telemetry.protocol",
            &format!("{:?}", config.telemetry.protocol),
            &format!("{:?}", defaults.telemetry.protocol),
            profile_str,
        );
        Self::track_property(
            props,
            "telemetry.strict",
            &config.telemetry.strict.to_string(),
            &defaults.telemetry.strict.to_string(),
            profile_str,
        );
    }

    fn track_health_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        Self::track_property(
            props,
            "health.path",
            &config.health.path,
            &defaults.health.path,
            profile_str,
        );
        Self::track_property(
            props,
            "health.live_path",
            &config.health.live_path,
            &defaults.health.live_path,
            profile_str,
        );
        Self::track_property(
            props,
            "health.ready_path",
            &config.health.ready_path,
            &defaults.health.ready_path,
            profile_str,
        );
        Self::track_property(
            props,
            "health.startup_path",
            &config.health.startup_path,
            &defaults.health.startup_path,
            profile_str,
        );
        Self::track_property(
            props,
            "health.detailed",
            &config.health.detailed.to_string(),
            &defaults.health.detailed.to_string(),
            profile_str,
        );
    }

    fn track_actuator_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        Self::track_property(
            props,
            "actuator.prefix",
            &config.actuator.prefix,
            &defaults.actuator.prefix,
            profile_str,
        );
        Self::track_property(
            props,
            "actuator.sensitive",
            &config.actuator.sensitive.to_string(),
            &defaults.actuator.sensitive.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "actuator.prometheus",
            &config.actuator.prometheus.to_string(),
            &defaults.actuator.prometheus.to_string(),
            profile_str,
        );
    }

    fn track_session_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        Self::track_property(
            props,
            "session.backend",
            &format!("{:?}", config.session.backend),
            &format!("{:?}", defaults.session.backend),
            profile_str,
        );
        Self::track_property(
            props,
            "session.cookie_name",
            &config.session.cookie_name,
            &defaults.session.cookie_name,
            profile_str,
        );
        Self::track_property(
            props,
            "session.max_age_secs",
            &config.session.max_age_secs.to_string(),
            &defaults.session.max_age_secs.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "session.secure",
            &config.session.secure.to_string(),
            &defaults.session.secure.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "session.same_site",
            &config.session.same_site,
            &defaults.session.same_site,
            profile_str,
        );
        Self::track_property(
            props,
            "session.http_only",
            &config.session.http_only.to_string(),
            &defaults.session.http_only.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "session.path",
            &config.session.path,
            &defaults.session.path,
            profile_str,
        );
        Self::track_property(
            props,
            "session.allow_memory_in_production",
            &config.session.allow_memory_in_production.to_string(),
            &defaults.session.allow_memory_in_production.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "session.redis.url",
            config.session.redis.url.as_deref().unwrap_or(""),
            defaults.session.redis.url.as_deref().unwrap_or(""),
            profile_str,
        );
        Self::track_property(
            props,
            "session.redis.key_prefix",
            &config.session.redis.key_prefix,
            &defaults.session.redis.key_prefix,
            profile_str,
        );
    }

    fn track_channels_props(
        props: &mut HashMap<String, ConfigProperty>,
        config: &crate::config::AutumnConfig,
        defaults: &crate::config::AutumnConfig,
        profile_str: &str,
    ) {
        Self::track_property(
            props,
            "channels.backend",
            &format!("{:?}", config.channels.backend),
            &format!("{:?}", defaults.channels.backend),
            profile_str,
        );
        Self::track_property(
            props,
            "channels.capacity",
            &config.channels.capacity.to_string(),
            &defaults.channels.capacity.to_string(),
            profile_str,
        );
        Self::track_property(
            props,
            "channels.redis.url",
            config.channels.redis.url.as_deref().unwrap_or(""),
            defaults.channels.redis.url.as_deref().unwrap_or(""),
            profile_str,
        );
        Self::track_property(
            props,
            "channels.redis.key_prefix",
            &config.channels.redis.key_prefix,
            &defaults.channels.redis.key_prefix,
            profile_str,
        );
    }

    fn track_property(
        props: &mut HashMap<String, ConfigProperty>,
        key: &str,
        value: &str,
        default_value: &str,
        profile: &str,
    ) {
        // Check if there's an env var override
        let env_key = format!("AUTUMN_{}", key.replace('.', "__").to_uppercase());
        let source = if std::env::var(&env_key).is_ok() {
            env_key
        } else if value != default_value && (profile == "dev" || profile == "prod") {
            format!("profile_default:{profile}")
        } else if value != default_value {
            "autumn.toml".to_string()
        } else {
            "default".to_string()
        };

        let display_value = if should_redact(key) {
            serde_json::Value::String("****".into())
        } else {
            serde_json::Value::String(value.to_string())
        };

        props.insert(
            key.to_string(),
            ConfigProperty {
                value: display_value,
                source,
            },
        );
    }

    /// Get a snapshot of all properties.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, ConfigProperty> {
        self.inner
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

// ── Health Indicator ─────────────────────────────────────────────

/// Health status reported by a [`HealthIndicator`].
///
/// Follows Spring Boot precedence:
/// `Down` > `OutOfService` > `Unknown` > `Up`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HealthStatus {
    /// The component is functioning normally.
    #[serde(rename = "UP")]
    Up,
    /// The component is unavailable.
    #[serde(rename = "DOWN")]
    Down,
    /// The component is out of service (maintenance, etc.).
    #[serde(rename = "OUT_OF_SERVICE")]
    OutOfService,
    /// The component status cannot be determined.
    #[serde(rename = "UNKNOWN")]
    Unknown,
}

impl HealthStatus {
    /// Human-readable string for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "UP",
            Self::Down => "DOWN",
            Self::OutOfService => "OUT_OF_SERVICE",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Returns `true` when this status does not indicate a failure
    /// (`Up` and `Unknown` are healthy; `Down` and `OutOfService` are not).
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Up | Self::Unknown)
    }
}

/// Which group a [`HealthIndicator`] belongs to.
///
/// `Readiness` indicators gate both `/ready` and `/actuator/health`.
/// `HealthOnly` indicators appear only in `/actuator/health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorGroup {
    /// Participates in `/ready` and `/actuator/health`.
    Readiness,
    /// Participates only in `/actuator/health`.
    HealthOnly,
}

/// Output from a single [`HealthIndicator::check`] call.
#[derive(Debug, Clone)]
pub struct HealthCheckOutput {
    /// The health status of this component.
    pub status: HealthStatus,
    /// Optional human-readable key-value detail map.
    pub details: HashMap<String, serde_json::Value>,
}

impl HealthCheckOutput {
    /// Create an `Up` output with no details.
    #[must_use]
    pub fn up() -> Self {
        Self {
            status: HealthStatus::Up,
            details: HashMap::new(),
        }
    }

    /// Create a `Down` output with no details.
    #[must_use]
    pub fn down() -> Self {
        Self {
            status: HealthStatus::Down,
            details: HashMap::new(),
        }
    }

    /// Attach a detail map to this output.
    #[must_use]
    pub fn with_details(mut self, details: HashMap<String, serde_json::Value>) -> Self {
        self.details = details;
        self
    }
}

/// Contract for a custom health check.
///
/// Implement this trait and register it via [`crate::app::AppBuilder::health_indicator`]
/// to surface the health of an external dependency in `/actuator/health` and optionally
/// in `/ready`.
///
/// # Example
///
/// ```rust
/// use autumn_web::actuator::{HealthCheckOutput, HealthIndicator, HealthStatus};
///
/// pub struct StripeIndicator;
///
/// impl HealthIndicator for StripeIndicator {
///     fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
///         Box::pin(async move {
///             // TODO: ping Stripe API
///             HealthCheckOutput::up()
///         })
///     }
/// }
/// ```
pub trait HealthIndicator: Send + Sync + 'static {
    /// Run the check and return the current health output.
    ///
    /// The future is polled inside a per-indicator timeout; if it does not
    /// resolve within [`Self::timeout_ms`] milliseconds it is cancelled and
    /// the indicator is reported as `Unknown` with `timed_out: true`.
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput>;

    /// Per-indicator timeout in milliseconds. Default: 2 000 ms.
    fn timeout_ms(&self) -> u64 {
        2000
    }

    /// Which probe group this indicator belongs to. Default: [`IndicatorGroup::Readiness`].
    fn group(&self) -> IndicatorGroup {
        IndicatorGroup::Readiness
    }
}

/// A single result returned from [`HealthIndicatorRegistry::run_all`] or
/// [`HealthIndicatorRegistry::run_readiness`].
#[derive(Debug, Clone)]
pub struct HealthRunResult {
    /// The registration name of this indicator.
    pub name: String,
    /// Which probe group this indicator belongs to.
    pub group: IndicatorGroup,
    /// The output of the check (possibly timed-out).
    pub output: HealthCheckOutput,
}

type IndicatorList = Vec<(String, IndicatorGroup, Arc<dyn HealthIndicator>)>;

/// Registry of named [`HealthIndicator`] implementations.
///
/// Populated by [`crate::app::AppBuilder::health_indicator`] and stored on
/// [`crate::AppState`]. Provides duplicate-registration detection at startup
/// and per-indicator timeout enforcement at request time.
#[derive(Clone, Default)]
pub struct HealthIndicatorRegistry {
    inner: Arc<RwLock<IndicatorList>>,
}

impl HealthIndicatorRegistry {
    /// Create a new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a named indicator with its group.
    ///
    /// Returns `Err` if a indicator with `name` was already registered.
    ///
    /// # Errors
    ///
    /// Returns an error string when `name` is already registered.
    pub fn register(
        &self,
        name: impl Into<String>,
        group: IndicatorGroup,
        indicator: Arc<dyn HealthIndicator>,
    ) -> Result<(), String> {
        let name = name.into();
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.iter().any(|(n, _, _)| n == &name) {
            return Err(format!(
                "HealthIndicator '{name}' is already registered; skipping duplicate"
            ));
        }
        inner.push((name, group, indicator));
        drop(inner);
        Ok(())
    }

    /// Returns `true` when no indicators have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Run all registered indicators (both groups) with per-indicator timeouts.
    ///
    /// All indicators execute **concurrently**; total wall time is bounded by
    /// the slowest single indicator rather than N × timeout.
    pub async fn run_all(&self) -> Vec<HealthRunResult> {
        let entries = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        let mut results = futures::future::join_all(entries.into_iter().map(
            |(name, group, indicator)| async move {
                let output = run_with_timeout(indicator.as_ref()).await;
                HealthRunResult {
                    name,
                    group,
                    output,
                }
            },
        ))
        .await;

        for breaker in crate::circuit_breaker::global_registry().all_breakers() {
            let state = breaker.state();
            let status = match state {
                crate::circuit_breaker::CircuitState::Open
                | crate::circuit_breaker::CircuitState::HalfOpen => HealthStatus::Down,
                crate::circuit_breaker::CircuitState::Closed => HealthStatus::Up,
            };

            let mut details = HashMap::new();
            details.insert(
                "state".to_string(),
                serde_json::Value::String(state.as_str().to_string()),
            );
            if let Some(ratio_num) = serde_json::Number::from_f64(breaker.failure_ratio()) {
                details.insert(
                    "failure_ratio".to_string(),
                    serde_json::Value::Number(ratio_num),
                );
            }

            results.push(HealthRunResult {
                name: format!("circuit_breaker.{}", breaker.name()),
                group: IndicatorGroup::HealthOnly,
                output: HealthCheckOutput { status, details },
            });
        }

        results
    }

    /// Run only `Readiness`-group indicators with per-indicator timeouts.
    ///
    /// All indicators execute **concurrently**; total wall time is bounded by
    /// the slowest single indicator rather than N × timeout.
    pub async fn run_readiness(&self) -> Vec<HealthRunResult> {
        // Clone the full list to release the read lock before async work begins.
        let entries = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        futures::future::join_all(
            entries
                .into_iter()
                .filter(|(_, g, _)| *g == IndicatorGroup::Readiness)
                .map(|(name, group, indicator)| async move {
                    let output = run_with_timeout(indicator.as_ref()).await;
                    HealthRunResult {
                        name,
                        group,
                        output,
                    }
                }),
        )
        .await
    }

    /// Compute the aggregate status following Spring Boot precedence.
    ///
    /// Precedence: `Down` > `OutOfService` > `Unknown` > `Up`.
    /// An empty slice returns `Up`.
    #[must_use]
    pub fn aggregate_status(statuses: &[HealthStatus]) -> HealthStatus {
        let mut overall = HealthStatus::Up;
        for &s in statuses {
            overall = match (overall, s) {
                (_, HealthStatus::Down) | (HealthStatus::Down, _) => HealthStatus::Down,
                (_, HealthStatus::OutOfService) | (HealthStatus::OutOfService, _) => {
                    HealthStatus::OutOfService
                }
                (_, HealthStatus::Unknown) | (HealthStatus::Unknown, _) => HealthStatus::Unknown,
                _ => HealthStatus::Up,
            };
        }
        overall
    }
}

/// Run a single indicator with its declared timeout. Returns `Unknown` with
/// `timed_out: true` when the future does not resolve in time.
async fn run_with_timeout(indicator: &dyn HealthIndicator) -> HealthCheckOutput {
    let duration = tokio::time::Duration::from_millis(indicator.timeout_ms());
    match tokio::time::timeout(duration, indicator.check()).await {
        Ok(output) => output,
        Err(_elapsed) => {
            let mut details = HashMap::new();
            details.insert("timed_out".to_string(), serde_json::Value::Bool(true));
            HealthCheckOutput {
                status: HealthStatus::Unknown,
                details,
            }
        }
    }
}

// ── Health ──────────────────────────────────────────────────────

/// Enhanced health response for the actuator health endpoint.
#[derive(Serialize)]
struct ActuatorHealth {
    /// Overall aggregate status following Spring Boot precedence.
    status: &'static str,
    version: &'static str,
    profile: String,
    uptime: String,
    #[cfg(feature = "db")]
    autumn_after_commit_failures_total: u64,
    /// Per-component health, keyed by indicator name.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    components: HashMap<String, ComponentHealth>,
    /// Backwards-compatible checks block (populated by built-in db indicator).
    #[serde(skip_serializing_if = "Option::is_none")]
    checks: Option<HealthChecks>,
}

#[derive(Serialize, Clone)]
struct ComponentHealth {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct HealthChecks {
    #[serde(skip_serializing_if = "Option::is_none")]
    database: Option<DatabaseCheck>,
}

#[derive(Serialize)]
struct DatabaseCheck {
    status: &'static str,
    pool_size: u64,
    active_connections: u64,
    idle_connections: u64,
}

fn build_health_components(
    db_status: Option<HealthStatus>,
    db_check: Option<&DatabaseCheck>,
    indicator_results: &[HealthRunResult],
    detailed: bool,
) -> HashMap<String, ComponentHealth> {
    let mut components: HashMap<String, ComponentHealth> = HashMap::new();
    // Custom indicators first so the built-in "db" key inserted below can never
    // be overwritten by a user-registered indicator with the same name.
    for result in indicator_results {
        if !detailed
            && result.name.starts_with("circuit_breaker.")
            && result.output.status.is_healthy()
        {
            continue;
        }
        let details = (detailed && !result.output.details.is_empty())
            .then(|| serde_json::to_value(&result.output.details).unwrap_or_default());
        components.insert(
            result.name.clone(),
            ComponentHealth {
                status: result.output.status.as_str(),
                details,
            },
        );
    }
    if let Some(s) = db_status {
        let details = detailed
            .then(|| {
                db_check.map(|d| {
                    serde_json::json!({
                        "status": d.status,
                        "pool_size": d.pool_size,
                        "active_connections": d.active_connections,
                        "idle_connections": d.idle_connections,
                    })
                })
            })
            .flatten();
        components.insert(
            "db".to_string(),
            ComponentHealth {
                status: s.as_str(),
                details,
            },
        );
    }
    components
}

/// `GET <actuator-prefix>/health`
pub async fn health<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> impl IntoResponse {
    let detailed = state.health_detailed();

    // ── built-in db component ────────────────────────────────────
    let (db_component_status, db_check) = {
        #[cfg(feature = "db")]
        {
            #[allow(clippy::option_if_let_else)]
            if let Some(pool) = state.pool() {
                let status = pool.status();
                let available = status.available as u64;
                let size = status.max_size as u64;
                let waiting = status.waiting as u64;
                let idle = available;
                let active = size.saturating_sub(available);

                let healthy = available > 0 || waiting == 0;
                let db_status = if healthy {
                    HealthStatus::Up
                } else {
                    HealthStatus::Down
                };
                let db_check = Some(DatabaseCheck {
                    status: if healthy { "ok" } else { "down" },
                    pool_size: size,
                    active_connections: active,
                    idle_connections: idle,
                });
                (Some(db_status), db_check)
            } else {
                (None, None)
            }
        }
        #[cfg(not(feature = "db"))]
        {
            (None::<HealthStatus>, None::<DatabaseCheck>)
        }
    };

    // ── registered custom indicators ───────────────────────────
    let indicator_results = if let Some(registry) = state.health_indicator_registry() {
        registry.run_all().await
    } else {
        Vec::new()
    };

    // ── aggregate status ────────────────────────────────────────
    let mut all_statuses: Vec<HealthStatus> =
        indicator_results.iter().map(|r| r.output.status).collect();
    if let Some(s) = db_component_status {
        all_statuses.push(s);
    }
    let overall = HealthIndicatorRegistry::aggregate_status(&all_statuses);

    // ── build components map ────────────────────────────────────
    let components = build_health_components(
        db_component_status,
        db_check.as_ref(),
        &indicator_results,
        detailed,
    );

    let checks = db_check.map(|db| HealthChecks { database: Some(db) });

    let body = ActuatorHealth {
        status: overall.as_str(),
        version: env!("CARGO_PKG_VERSION"),
        profile: state.profile().to_owned(),
        uptime: state.uptime_display(),
        #[cfg(feature = "db")]
        autumn_after_commit_failures_total: crate::db::AFTER_COMMIT_FAILURES_TOTAL
            .load(std::sync::atomic::Ordering::Relaxed),
        components,
        checks,
    };

    let code = if overall.is_healthy() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body))
}

// ── Info ────────────────────────────────────────────────────────

/// Application info response.
#[derive(Serialize)]
pub(crate) struct ActuatorInfo {
    app: AppInfo,
    autumn: FrameworkInfo,
    runtime: RuntimeInfo,
}

#[derive(Serialize)]
struct AppInfo {
    name: String,
    version: String,
}

#[derive(Serialize)]
struct FrameworkInfo {
    version: &'static str,
    profile: String,
}

#[derive(Serialize)]
struct RuntimeInfo {
    uptime: String,
}

/// `GET <actuator-prefix>/info`
pub(crate) async fn info<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<ActuatorInfo> {
    Json(ActuatorInfo {
        app: AppInfo {
            name: std::env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "unknown".into()),
            version: std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into()),
        },
        autumn: FrameworkInfo {
            version: env!("CARGO_PKG_VERSION"),
            profile: state.profile().to_owned(),
        },
        runtime: RuntimeInfo {
            uptime: state.uptime_display(),
        },
    })
}

// ── Env (sensitive) ─────────────────────────────────────────────

/// Config environment response with redacted secrets.
#[derive(Serialize)]
pub(crate) struct ActuatorEnv {
    active_profile: String,
    properties: std::collections::HashMap<String, serde_json::Value>,
}

/// Keys that trigger value redaction.
const REDACT_PATTERNS: &[&str] = &[
    "password",
    "secret",
    "key",
    "token",
    "credential",
    "auth",
    "url",
];

fn should_redact(key: &str) -> bool {
    let lower = key.to_lowercase();
    REDACT_PATTERNS.iter().any(|p| lower.contains(p))
}

/// `GET /actuator/env` — only available when actuator sensitive mode is enabled.
pub(crate) async fn env_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<ActuatorEnv> {
    let properties = state
        .config_props()
        .snapshot()
        .into_iter()
        .map(|(key, prop)| (key, prop.value))
        .collect();

    Json(ActuatorEnv {
        active_profile: state.profile().to_owned(),
        properties,
    })
}

// ── Metrics ────────────────────────────────────────────────────

/// `GET <actuator-prefix>/metrics` -- request metrics, latency, status codes, DB pool stats.
pub(crate) async fn metrics_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<serde_json::Value> {
    let snapshot = state.metrics().snapshot();
    let mut result = serde_json::to_value(&snapshot).unwrap_or_default();

    // Include read-through cache stampede-protection counters (always
    // present; the read-through API works standalone without app state).
    if let serde_json::Value::Object(ref mut map) = result {
        map.insert(
            "cache".to_string(),
            serde_json::to_value(crate::cache::read_through_metrics().snapshot())
                .unwrap_or_default(),
        );
    }

    // Include DB pool stats if available
    #[cfg(feature = "db")]
    if let Some(pool) = state.pool() {
        let status = pool.status();
        let db_stats = serde_json::json!({
            "pool_size": status.max_size,
            "active_connections": (status.size as u64).saturating_sub(status.available as u64),
            "idle_connections": status.available,
        });
        if let serde_json::Value::Object(ref mut map) = result {
            map.insert("database".to_string(), db_stats);
        }
    }

    // Include per-shard pool stats keyed by shard name
    #[cfg(feature = "db")]
    if let Some(shards) = state.shards() {
        let mut shard_stats = serde_json::Map::new();
        for shard in shards.iter() {
            let status = shard.primary_pool().status();
            let mut entry = serde_json::json!({
                "pool_size": status.max_size,
                "active_connections":
                    (status.size as u64).saturating_sub(status.available as u64),
                "idle_connections": status.available,
                "slots": shard.slots().len(),
            });
            if let Some(replica) = shard.replica_pool() {
                let replica_status = replica.status();
                if let serde_json::Value::Object(ref mut entry_map) = entry {
                    entry_map.insert(
                        "replica".to_string(),
                        serde_json::json!({
                            "pool_size": replica_status.max_size,
                            "active_connections": (replica_status.size as u64)
                                .saturating_sub(replica_status.available as u64),
                            "idle_connections": replica_status.available,
                        }),
                    );
                }
            }
            shard_stats.insert(shard.name().to_owned(), entry);
        }
        if let serde_json::Value::Object(ref mut map) = result {
            map.insert(
                "database_shards".to_string(),
                serde_json::Value::Object(shard_stats),
            );
        }
    }

    // Include plugin-contributed sources under the "sources" key
    if let Some(registry) = state.metrics_source_registry() {
        let all = registry.collect_all();
        let mut sources = serde_json::Map::new();
        for (source_name, families) in all {
            let families_json: Vec<serde_json::Value> = families
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "name": f.name,
                        "help": f.help,
                        "kind": f.kind.as_str(),
                        "samples": f.samples.iter().map(|s| {
                            let labels: serde_json::Map<String, serde_json::Value> = s.labels
                                .iter()
                                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                                .collect();
                            serde_json::json!({
                                "labels": labels,
                                "value": s.value,
                            })
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect();
            sources.insert(source_name, serde_json::Value::Array(families_json));
        }
        if let serde_json::Value::Object(ref mut map) = result {
            map.insert("sources".to_string(), serde_json::Value::Object(sources));
        }
    }

    Json(result)
}

#[derive(Serialize)]
pub(crate) struct CircuitBreakerActuatorResponse {
    pub name: String,
    pub state: &'static str,
    pub failure_ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_ratio_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_window_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_sample_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub half_open_trial_count: Option<u64>,
}

/// `GET <actuator-prefix>/circuitbreakers`
pub(crate) async fn circuitbreakers_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<Vec<CircuitBreakerActuatorResponse>> {
    let detailed = state.health_detailed();
    let mut responses = Vec::new();

    for breaker in crate::circuit_breaker::global_registry().all_breakers() {
        let policy = breaker.config();
        responses.push(CircuitBreakerActuatorResponse {
            name: breaker.name().to_string(),
            state: breaker.state().as_str(),
            failure_ratio: breaker.failure_ratio(),
            failure_ratio_threshold: detailed.then_some(policy.failure_ratio_threshold),
            sample_window_secs: detailed.then_some(policy.sample_window.as_secs()),
            minimum_sample_count: detailed.then_some(policy.minimum_sample_count),
            open_duration_secs: detailed.then_some(policy.open_duration.as_secs()),
            half_open_trial_count: detailed.then_some(policy.half_open_trial_count),
        });
    }

    Json(responses)
}

// ── Prometheus ─────────────────────────────────────────────────

/// Render label set `{k="v",...}` or empty string for no labels.
///
/// Writes directly into a pre-allocated `String` to avoid per-pair heap allocations.
fn render_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(64);
    out.push('{');
    for (i, (k, v)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push_str("=\"");
        for c in v.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '"' => out.push_str("\\\""),
                other => out.push(other),
            }
        }
        out.push('"');
    }
    out.push('}');
    out
}

/// Returns true if `s` is a valid Prometheus metric name (`[a-zA-Z_:][a-zA-Z0-9_:]*`).
fn is_valid_metric_name(s: &str) -> bool {
    let mut it = s.chars();
    matches!(it.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':')
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Returns true if `s` is a valid Prometheus label name (`[a-zA-Z_][a-zA-Z0-9_]*`).
fn is_valid_label_name(s: &str) -> bool {
    let mut it = s.chars();
    matches!(it.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Escape a Prometheus label value (backslash, newline, and double-quote).
fn escape_prometheus_label_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

/// Escape a Prometheus HELP string (backslash and newline only).
fn escape_help_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Format a sample value per Prometheus text format (handles ±Inf and NaN).
fn format_sample_value(v: f64) -> String {
    if v == f64::INFINITY {
        "+Inf".to_string()
    } else if v == f64::NEG_INFINITY {
        "-Inf".to_string()
    } else if v.is_nan() {
        "NaN".to_string()
    } else {
        v.to_string()
    }
}

/// Append all plugin-contributed metric families (and their error counters) to `out`.
///
/// `emitted_families` must be pre-seeded with the names of every built-in
/// metric family already written to `out`; families whose names collide are
/// skipped with a warning so no duplicate `# HELP`/`# TYPE` blocks are emitted.
fn render_plugin_sources(
    registry: &MetricsSourceRegistry,
    out: &mut String,
    emitted_families: &mut std::collections::HashSet<String>,
) {
    use std::fmt::Write;

    let all_sources = registry.collect_all();
    for (_source_name, families) in &all_sources {
        for family in families {
            if !is_valid_metric_name(&family.name) {
                tracing::warn!(name = %family.name, "MetricsSource returned invalid metric name; skipping family");
                continue;
            }
            if !emitted_families.insert(family.name.clone()) {
                tracing::warn!(name = %family.name, "MetricsSource returned duplicate metric family name; skipping family");
                continue;
            }
            let _ = writeln!(
                out,
                "# HELP {} {}",
                family.name,
                escape_help_text(&family.help)
            );
            let _ = writeln!(out, "# TYPE {} {}", family.name, family.kind.as_str());
            let mut emitted_series: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for sample in &family.samples {
                let mut bad_key = false;
                let mut seen_keys = std::collections::HashSet::new();
                let mut valid_labels: Vec<(String, String)> = Vec::new();
                for (k, v) in &sample.labels {
                    if !is_valid_label_name(k) {
                        tracing::warn!(
                            label_name = %k,
                            metric = %family.name,
                            "MetricsSource returned invalid label name; skipping sample"
                        );
                        bad_key = true;
                        break;
                    }
                    if !seen_keys.insert(k.as_str()) {
                        tracing::warn!(label_name = %k, "MetricsSource returned duplicate label name; dropping duplicate");
                        continue;
                    }
                    valid_labels.push((k.clone(), v.clone()));
                }
                if bad_key {
                    continue;
                }
                // Sort by key so {a="1",b="2"} and {b="2",a="1"} produce the
                // same canonical string and are treated as one series.
                valid_labels.sort_by(|(a, _), (b, _)| a.cmp(b));
                let labels = render_labels(&valid_labels);
                if !emitted_series.insert(labels.clone()) {
                    tracing::warn!(
                        metric = %family.name,
                        labels = %labels,
                        "MetricsSource returned duplicate series; skipping sample"
                    );
                    continue;
                }
                let _ = writeln!(
                    out,
                    "{}{} {}",
                    family.name,
                    labels,
                    format_sample_value(sample.value)
                );
            }
        }
    }

    let error_counts = registry.error_counts();
    if !error_counts.is_empty() {
        out.push_str(
            "# HELP autumn_metrics_source_errors_total \
             Number of scrape errors (panics) per plugin metrics source\n",
        );
        out.push_str("# TYPE autumn_metrics_source_errors_total counter\n");
        let mut names: Vec<&String> = error_counts.keys().collect();
        names.sort();
        for name in names {
            let label = render_labels(&[("source".to_string(), name.clone())]);
            let _ = writeln!(
                out,
                "autumn_metrics_source_errors_total{} {}",
                label, error_counts[name]
            );
        }
    }
}

/// Render the built-in `autumn_http_*` metric families into `out`, tagged with
/// the replica's deploy `version` label so canary and stable cohorts can be
/// compared by a controller scraping both.
fn write_builtin_http_metrics(
    out: &mut String,
    version: &str,
    snapshot: &crate::middleware::metrics::MetricsSnapshot,
) {
    use std::fmt::Write;

    // requests_total
    out.push_str("# HELP autumn_http_requests_total Total number of HTTP requests\n");
    out.push_str("# TYPE autumn_http_requests_total counter\n");
    let _ = writeln!(
        out,
        "autumn_http_requests_total{{version=\"{version}\"}} {}",
        snapshot.http.requests_total
    );

    // requests_active
    out.push_str("# HELP autumn_http_requests_active Currently active HTTP requests\n");
    out.push_str("# TYPE autumn_http_requests_active gauge\n");
    let _ = writeln!(
        out,
        "autumn_http_requests_active{{version=\"{version}\"}} {}",
        snapshot.http.requests_active
    );

    // by_status
    out.push_str("# HELP autumn_http_responses_total HTTP responses by status code\n");
    out.push_str("# TYPE autumn_http_responses_total counter\n");
    for (status, count) in [
        ("2xx", snapshot.http.by_status.s2xx),
        ("3xx", snapshot.http.by_status.s3xx),
        ("4xx", snapshot.http.by_status.s4xx),
        ("5xx", snapshot.http.by_status.s5xx),
    ] {
        let _ = writeln!(
            out,
            "autumn_http_responses_total{{version=\"{version}\",status=\"{status}\"}} {count}"
        );
    }

    // request_duration_seconds — global latency percentiles exposed as Prometheus
    // summary-style quantiles, labelled by deploy version so a canary controller
    // can gate promotion on p99 latency per cohort.
    out.push_str(
        "# HELP autumn_http_request_duration_seconds HTTP request latency percentiles in seconds\n",
    );
    out.push_str("# TYPE autumn_http_request_duration_seconds summary\n");
    for (quantile, millis) in [
        ("0.5", snapshot.http.latency_ms.p50),
        ("0.95", snapshot.http.latency_ms.p95),
        ("0.99", snapshot.http.latency_ms.p99),
    ] {
        #[allow(clippy::cast_precision_loss)]
        let seconds = millis as f64 / 1000.0;
        let _ = writeln!(
            out,
            "autumn_http_request_duration_seconds{{version=\"{version}\",quantile=\"{quantile}\"}} {seconds}"
        );
    }

    // autumn_shutdown_aborted_requests_total
    out.push_str(
        "# HELP autumn_shutdown_aborted_requests_total \
         HTTP requests forcibly dropped when the graceful-shutdown drain deadline expired\n",
    );
    out.push_str("# TYPE autumn_shutdown_aborted_requests_total counter\n");
    let _ = writeln!(
        out,
        "autumn_shutdown_aborted_requests_total{{version=\"{version}\"}} {}",
        snapshot.http.shutdown_aborted_requests_total
    );

    // autumn_request_timeouts_total
    out.push_str(
        "# HELP autumn_request_timeouts_total \
         HTTP requests that exceeded the configured per-request timeout\n",
    );
    out.push_str("# TYPE autumn_request_timeouts_total counter\n");
    let _ = writeln!(
        out,
        "autumn_request_timeouts_total{{version=\"{version}\"}} {}",
        snapshot.http.request_timeouts_total
    );

    // autumn_read_your_writes_pins_total
    out.push_str(
        "# HELP autumn_read_your_writes_pins_total \
         Replica reads redirected to the primary by the read-your-own-writes pin\n",
    );
    out.push_str("# TYPE autumn_read_your_writes_pins_total counter\n");
    let _ = writeln!(
        out,
        "autumn_read_your_writes_pins_total{{version=\"{version}\"}} {}",
        snapshot.read_your_writes_pins_total
    );

    // by_route
    if !snapshot.http.by_route.is_empty() {
        out.push_str("# HELP autumn_http_route_requests_total HTTP requests by route and method\n");
        out.push_str("# TYPE autumn_http_route_requests_total counter\n");
        let mut route_keys: Vec<&String> = snapshot.http.by_route.keys().collect();
        route_keys.sort();
        for route_key in route_keys {
            let metrics = &snapshot.http.by_route[route_key];
            // route_key is formatted as "METHOD /path"
            if let Some((method, path)) = route_key.split_once(' ') {
                let _ = writeln!(
                    out,
                    "autumn_http_route_requests_total{{version=\"{version}\",method=\"{method}\",route=\"{path}\"}} {}",
                    metrics.count
                );
            }
        }
    }
}

/// Render the built-in `autumn_cache_*` read-through stampede-protection
/// counters into `out`, tagged with the replica's deploy `version` label.
/// These counters are process-wide (the read-through API works standalone
/// without app state), unlike the HTTP metrics which come from per-app state.
fn write_builtin_cache_metrics(
    out: &mut String,
    version: &str,
    snapshot: &crate::cache::ReadThroughMetricsSnapshot,
) {
    use std::fmt::Write;

    for (name, help, value) in [
        (
            "autumn_cache_read_through_hits_total",
            "Read-through cache reads served from a fresh cached value",
            snapshot.hits,
        ),
        (
            "autumn_cache_read_through_misses_total",
            "Read-through cache reads that found no fresh cached value",
            snapshot.misses,
        ),
        (
            "autumn_cache_read_through_coalesced_waits_total",
            "Read-through callers that awaited a concurrent in-process fill instead of \
             computing their own (single-flight coalescing)",
            snapshot.coalesced_waits,
        ),
        (
            "autumn_cache_read_through_fills_total",
            "Read-through fill closures that completed successfully",
            snapshot.fills,
        ),
        (
            "autumn_cache_read_through_fill_failures_total",
            "Read-through fill closures that returned an error",
            snapshot.fill_failures,
        ),
        (
            "autumn_cache_read_through_stale_serves_total",
            "Stale-while-revalidate reads that served a stale value while a background \
             refresh ran",
            snapshot.stale_serves,
        ),
        (
            "autumn_cache_fill_lock_acquires_total",
            "Distributed cache fill locks acquired by this process",
            snapshot.fill_lock_acquires,
        ),
        (
            "autumn_cache_fill_lock_contended_total",
            "Distributed cache fill lock attempts that found the lock held by another replica",
            snapshot.fill_lock_contended,
        ),
    ] {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} counter");
        let _ = writeln!(out, "{name}{{version=\"{version}\"}} {value}");
    }
}

/// `GET <actuator-prefix>/prometheus` -- export metrics in Prometheus format.
pub(crate) async fn prometheus_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> impl IntoResponse {
    let snapshot = state.metrics().snapshot();
    // Deploy-version label so a canary controller can compare canary vs. stable
    // cohorts. Escaped defensively in case an operator sets an exotic value via
    // AUTUMN_DEPLOY_VERSION.
    let version = escape_prometheus_label_value(&state.deploy_version());
    let mut out = String::with_capacity(2048);

    write_builtin_http_metrics(&mut out, &version, &snapshot);
    write_builtin_cache_metrics(
        &mut out,
        &version,
        &crate::cache::read_through_metrics().snapshot(),
    );

    // Plugin-contributed metric families — seed with built-in names so
    // plugins cannot shadow or duplicate them.
    if let Some(registry) = state.metrics_source_registry() {
        let mut emitted_families: std::collections::HashSet<String> = [
            "autumn_http_requests_total",
            "autumn_http_requests_active",
            "autumn_http_responses_total",
            "autumn_http_request_duration_seconds",
            "autumn_shutdown_aborted_requests_total",
            "autumn_request_timeouts_total",
            "autumn_read_your_writes_pins_total",
            "autumn_http_route_requests_total",
            "autumn_metrics_source_errors_total",
            "autumn_cache_read_through_hits_total",
            "autumn_cache_read_through_misses_total",
            "autumn_cache_read_through_coalesced_waits_total",
            "autumn_cache_read_through_fills_total",
            "autumn_cache_read_through_fill_failures_total",
            "autumn_cache_read_through_stale_serves_total",
            "autumn_cache_fill_lock_acquires_total",
            "autumn_cache_fill_lock_contended_total",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        render_plugin_sources(registry, &mut out, &mut emitted_families);
    }

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        out,
    )
}

// ── Config Properties (sensitive) ──────────────────────────────

/// `GET <actuator-prefix>/configprops` -- all config properties with source tracking.
pub(crate) async fn configprops_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<serde_json::Value> {
    let props = state.config_props().snapshot();

    Json(serde_json::json!({
        "active_profile": state.profile(),
        "properties": props,
    }))
}

// ── Loggers (sensitive) ────────────────────────────────────────

/// Available log levels for the loggers endpoint.
const AVAILABLE_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

/// Response for `GET <actuator-prefix>/loggers`.
#[derive(Serialize)]
pub(crate) struct LoggersResponse {
    current_level: String,
    available_levels: Vec<&'static str>,
    loggers: HashMap<String, String>,
}

/// `GET <actuator-prefix>/loggers` -- view current log levels.
pub(crate) async fn loggers_get<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<LoggersResponse> {
    Json(LoggersResponse {
        current_level: state.log_levels().current_level(),
        available_levels: AVAILABLE_LEVELS.to_vec(),
        loggers: state.log_levels().logger_overrides(),
    })
}

/// Request body for `PUT <actuator-prefix>/loggers/{name}`.
#[derive(Deserialize)]
pub(crate) struct SetLoggerRequest {
    level: String,
}

/// `PUT <actuator-prefix>/loggers/{name}` -- change a logger's level at runtime.
pub(crate) async fn loggers_put<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
    Path(name): Path<String>,
    Json(body): Json<SetLoggerRequest>,
) -> impl IntoResponse {
    let level = body.level.to_lowercase();

    // Validate the level
    if !AVAILABLE_LEVELS.contains(&level.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "status": "error",
                "message": format!(
                    "Invalid level '{}'. Available levels: {}",
                    level,
                    AVAILABLE_LEVELS.join(", ")
                ),
            })),
        );
    }

    let previous = state.log_levels().set_logger_level(&name, &level);

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "message": format!("Logger '{}' set to '{}'", name, level),
            "previous": previous,
        })),
    )
}

// ── Logfile (sensitive) ────────────────────────────────────────

/// Query parameters for `GET <actuator-prefix>/logfile`.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct LogfileQuery {
    /// Minimum log level to include (case-insensitive).
    ///
    /// Valid values: `trace`, `debug`, `info`, `warn`, `error`.
    /// When absent all levels are returned.
    pub level: Option<String>,
    /// Maximum number of entries to return (most-recent N, newest-last).
    pub limit: Option<usize>,
}

/// JSON response shape for `GET <actuator-prefix>/logfile`.
#[derive(Debug, Serialize)]
pub(crate) struct LogfileResponse {
    /// Captured log entries, oldest first.
    pub entries: Vec<crate::log::capture::CapturedLogEntry>,
    /// Total entries in the buffer (before `limit` is applied).
    pub total: usize,
    /// `true` when the capture buffer is enabled and populated by the layer.
    pub capture_enabled: bool,
}

/// `GET <actuator-prefix>/logfile` — recent structured log entries.
///
/// Returns entries from the in-memory capture buffer, filtered by `?level=`
/// and capped by `?limit=`. Only available when `actuator.sensitive = true`
/// and `log.capture.enabled = true`.  When capture is disabled the endpoint
/// still responds with `200` and an empty list so API consumers can handle
/// the case uniformly.
///
/// Returns `400 Bad Request` when an unrecognised `?level=` value is supplied
/// so that typos (e.g. `?level=warning`) are rejected rather than silently
/// broadening the response to all captured entries.
pub(crate) async fn logfile_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
    axum::extract::Query(query): axum::extract::Query<LogfileQuery>,
) -> Result<axum::Json<LogfileResponse>, (StatusCode, axum::Json<serde_json::Value>)> {
    let min_level = match query.level.as_deref() {
        None => None,
        Some(s) => match crate::log::capture::level_from_str(s) {
            Some(level) => Some(level),
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": format!(
                            "invalid level {:?}; valid values: TRACE, DEBUG, INFO, WARN, ERROR",
                            s
                        )
                    })),
                ));
            }
        },
    };

    Ok(match state.log_buffer() {
        None => axum::Json(LogfileResponse {
            entries: vec![],
            total: 0,
            capture_enabled: false,
        }),
        Some(buf) => {
            let total = buf.len();
            let entries = buf.snapshot(min_level, query.limit);
            axum::Json(LogfileResponse {
                entries,
                total,
                capture_enabled: true,
            })
        }
    })
}

// ── Tasks (sensitive) ──────────────────────────────────────────

/// `GET <actuator-prefix>/tasks` -- scheduled task status.
pub(crate) async fn tasks_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<serde_json::Value> {
    let tasks = state.task_registry().snapshot();

    Json(serde_json::json!({
        "scheduled_tasks": tasks,
    }))
}

/// `GET <actuator-prefix>/jobs` -- ad-hoc background job status.
pub(crate) async fn jobs_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<serde_json::Value> {
    let jobs = state.job_registry().snapshot();
    Json(serde_json::json!({ "jobs": jobs }))
}

#[cfg(feature = "http-client")]
/// Request body for `POST <actuator-prefix>/webhooks/replay`.
#[derive(Deserialize)]
pub(crate) struct ReplayRequest {
    log_id: String,
}

#[cfg(feature = "http-client")]
async fn enqueue_webhook_replay_job(log_id: &str) -> Result<(), String> {
    let job_payload = serde_json::json!({
        "log_id": log_id,
        "replay": true,
    });

    let Some(job_client) = crate::job::global_job_client() else {
        return Err("Global job client is not available".to_string());
    };

    job_client
        .enqueue("autumn_webhook_delivery", job_payload)
        .await
        .map_err(|e| format!("Failed to enqueue job: {e}"))
}

#[cfg(feature = "http-client")]
/// `GET <actuator-prefix>/webhooks/dlq` -- list dead-lettered webhook logs.
pub(crate) async fn webhooks_dlq_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> impl IntoResponse {
    let Some(manager) = state.webhook_outbound() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "status": "error",
                "message": "Outbound webhook support is not configured or enabled"
            })),
        )
            .into_response();
    };

    match manager.store().get_dlq_logs().await {
        Ok(logs) => (StatusCode::OK, Json(logs)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": format!("Failed to fetch DLQ logs: {}", e)
            })),
        )
            .into_response(),
    }
}

#[cfg(feature = "http-client")]
/// `POST <actuator-prefix>/webhooks/replay` -- replay a dead-lettered webhook log.
pub(crate) async fn webhooks_replay_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
    Json(body): Json<ReplayRequest>,
) -> impl IntoResponse {
    let Some(manager) = state.webhook_outbound() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "status": "error",
                "message": "Outbound webhook support is not configured or enabled"
            })),
        )
            .into_response();
    };

    let log_opt = match manager.store().get_delivery_log(&body.log_id).await {
        Ok(log) => log,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "message": format!("Failed to retrieve log: {}", e)
                })),
            )
                .into_response();
        }
    };

    let Some(log) = log_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "status": "error",
                "message": format!("Log with ID {} not found", body.log_id)
            })),
        )
            .into_response();
    };

    if !log.is_dlq {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "status": "error",
            "message": format!("Log with ID {} is not in the Dead Letter Queue (DLQ)", body.log_id)
        }))).into_response();
    }

    if let Some(response) = blocked_webhook_replay_response(&manager, &log, &body.log_id).await {
        return response;
    }

    let subscription_id = log.subscription_id.clone();
    let original_log = log.clone();
    let log = reset_webhook_replay_log(log);

    if let Err(e) = manager.store().log_delivery(log).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": format!("Failed to update delivery log state: {}", e)
            })),
        )
            .into_response();
    }

    // Enqueue background delivery job now that the log state is safely reset in the store
    if let Err(message) = enqueue_webhook_replay_job(&body.log_id).await {
        if let Err(rollback_error) = manager.store().replace_delivery_log(original_log).await {
            tracing::error!(
                log_id = %body.log_id,
                "Failed to roll back webhook replay log after enqueue failure: {}",
                rollback_error
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "message": format!("{message}; failed to restore DLQ log state: {rollback_error}")
                })),
            )
                .into_response();
        }

        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": message
            })),
        )
            .into_response();
    }

    // Reactivate auto-failed subscriptions only after the replay job is queued.
    if let Err(e) = manager
        .store()
        .reactivate_failed_subscription(&subscription_id)
        .await
    {
        tracing::warn!(subscription_id = %subscription_id, "Failed to reactivate subscription during replay: {}", e);
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "message": format!("Replay successfully enqueued for log {}", body.log_id)
        })),
    )
        .into_response()
}

#[cfg(feature = "http-client")]
fn reset_webhook_replay_log(
    mut log: crate::webhook_outbound::WebhookDeliveryLog,
) -> crate::webhook_outbound::WebhookDeliveryLog {
    log.is_dlq = false;
    log.attempt = 1;
    log.last_error = None;
    log.response_status = None;
    log.response_body = None;
    log.timestamp = chrono::Utc::now();
    log
}

#[cfg(feature = "http-client")]
async fn blocked_webhook_replay_response(
    manager: &crate::webhook_outbound::WebhookOutboundManager,
    log: &crate::webhook_outbound::WebhookDeliveryLog,
    log_id: &str,
) -> Option<axum::response::Response> {
    let subscription = match manager.store().get_subscription(&log.subscription_id).await {
        Ok(subscription) => subscription,
        Err(e) => {
            return Some(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "status": "error",
                        "message": format!("Failed to retrieve subscription: {}", e)
                    })),
                )
                    .into_response(),
            );
        }
    };

    let Some(subscription) = subscription else {
        return Some(
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "status": "error",
                    "message": format!(
                        "Subscription {} for replay log {} was not found",
                        log.subscription_id, log_id
                    )
                })),
            )
                .into_response(),
        );
    };

    if subscription.status != crate::webhook_outbound::WebhookSubscriptionStatus::Disabled {
        return None;
    }

    Some(
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "status": "error",
                "message": format!(
                    "Subscription {} is disabled; re-enable it before replaying log {}",
                    log.subscription_id, log_id
                )
            })),
        )
            .into_response(),
    )
}

// ── A11y ───────────────────────────────────────────────────────

/// `GET <actuator-prefix>/a11y` -- scaffold-level accessibility posture.
///
/// Returns a JSON object describing which WCAG 2.1 AA scaffold concerns the
/// application addresses.  Available in all profiles (like `/actuator/health`).
pub(crate) async fn a11y_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<A11yPosture> {
    Json(state.a11y_posture())
}

// ── Channels (sensitive) ───────────────────────────────────────

/// `GET <actuator-prefix>/channels` -- get current channel snapshots.
#[cfg(feature = "ws")]
pub(crate) async fn channels_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
) -> Json<serde_json::Value> {
    let channels = state.channels().snapshot();
    Json(serde_json::json!({
        "channels": channels,
    }))
}

// ── Tasks Stream (WebSocket) ───────────────────────────────────

/// `GET <actuator-prefix>/tasks/stream` -- stream scheduled task events.
#[cfg(feature = "ws")]
pub(crate) async fn tasks_stream_endpoint<S: ProvideActuatorState + Send + Sync + 'static>(
    State(state): State<S>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = state.channels().subscribe("sys:tasks");
        let shutdown = state.shutdown_token();

        loop {
            tokio::select! {
                res = rx.recv() => {
                    match res {
                        Ok(msg) => {
                            let ws_msg = axum::extract::ws::Message::Text(msg.into_string().into());
                            if socket.send(ws_msg).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                () = shutdown.cancelled() => {
                    let _ = socket.send(axum::extract::ws::Message::Close(None)).await;
                    break;
                }
                else => break,
            }
        }
    })
}

// ── Router builder ──────────────────────────────────────────────

pub(crate) fn normalize_actuator_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() || trimmed == "/" {
        String::new()
    } else {
        let trimmed = trimmed.trim_end_matches('/');
        if trimmed.starts_with('/') {
            trimmed.to_owned()
        } else {
            format!("/{trimmed}")
        }
    }
}

pub(crate) fn actuator_route_glob(prefix: &str) -> String {
    let prefix = normalize_actuator_prefix(prefix);
    if prefix.is_empty() {
        "/*".to_owned()
    } else {
        format!("{prefix}/*")
    }
}

pub(crate) fn actuator_route_path(prefix: &str, suffix: &str) -> String {
    let prefix = normalize_actuator_prefix(prefix);
    if prefix.is_empty() {
        suffix.to_owned()
    } else {
        format!("{prefix}{suffix}")
    }
}

pub(crate) fn actuator_endpoint_paths(
    prefix: &str,
    sensitive: bool,
    prometheus_enabled: bool,
) -> Vec<String> {
    let mut paths = vec![
        actuator_route_path(prefix, "/health"),
        actuator_route_path(prefix, "/info"),
        actuator_route_path(prefix, "/metrics"),
        actuator_route_path(prefix, "/a11y"),
        actuator_route_path(prefix, "/ui"),
        actuator_route_path(prefix, "/ui/metrics"),
    ];

    if prometheus_enabled {
        paths.push(actuator_route_path(prefix, "/prometheus"));
    }

    if sensitive {
        paths.push(actuator_route_path(prefix, "/circuitbreakers"));
        paths.push(actuator_route_path(prefix, "/env"));
        paths.push(actuator_route_path(prefix, "/configprops"));
        paths.push(actuator_route_path(prefix, "/loggers"));
        paths.push(actuator_route_path(prefix, "/logfile"));
        paths.push(actuator_route_path(prefix, "/tasks"));
        paths.push(actuator_route_path(prefix, "/jobs"));
        paths.push(actuator_route_path(prefix, "/ui/tasks"));
        #[cfg(feature = "system-info")]
        {
            paths.push(actuator_route_path(prefix, "/system"));
        }
        #[cfg(feature = "http-client")]
        {
            paths.push(actuator_route_path(prefix, "/webhooks/dlq"));
            paths.push(actuator_route_path(prefix, "/webhooks/replay"));
        }
        #[cfg(feature = "ws")]
        {
            paths.push(actuator_route_path(prefix, "/channels"));
            paths.push(actuator_route_path(prefix, "/tasks/stream"));
        }
    }

    paths
}

/// Build the actuator router with profile-aware endpoint exposure.
///
/// In dev mode (or when `actuator.sensitive = true`), all endpoints are
/// exposed. In prod mode, only health, info, and metrics are available.
///
/// The Prometheus scrape endpoint is mounted unconditionally here (independent
/// of `sensitive`). The framework router mounts the actuator from configuration
/// and gates `/actuator/prometheus` on the `actuator.prometheus` flag.
pub fn actuator_router<S: ProvideActuatorState + Send + Sync + Clone + 'static>(
    sensitive: bool,
) -> axum::Router<S> {
    actuator_router_with_prefix("/actuator", sensitive, true)
}

/// Build the actuator router at a configured prefix.
///
/// This is the prefix-aware variant used by the framework router.
///
/// `prometheus_enabled` controls the `/actuator/prometheus` scrape endpoint
/// independently of `sensitive`, so platform metrics scraping can be exposed
/// without also exposing sensitive actuator surfaces.
#[allow(clippy::too_many_lines)]
pub(crate) fn actuator_router_with_prefix<
    S: ProvideActuatorState + Send + Sync + Clone + 'static,
>(
    prefix: &str,
    sensitive: bool,
    prometheus_enabled: bool,
) -> axum::Router<S> {
    let mut router = axum::Router::new()
        .route(
            &actuator_route_path(prefix, "/health"),
            axum::routing::get(health::<S>),
        )
        .route(
            &actuator_route_path(prefix, "/info"),
            axum::routing::get(info::<S>),
        )
        .route(
            &actuator_route_path(prefix, "/metrics"),
            axum::routing::get(metrics_endpoint::<S>),
        )
        .route(
            &actuator_route_path(prefix, "/a11y"),
            axum::routing::get(a11y_endpoint::<S>),
        );

    if prometheus_enabled {
        router = router.route(
            &actuator_route_path(prefix, "/prometheus"),
            axum::routing::get(prometheus_endpoint::<S>),
        );
    }

    if sensitive {
        router = router
            .route(
                &actuator_route_path(prefix, "/circuitbreakers"),
                axum::routing::get(circuitbreakers_endpoint::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/env"),
                axum::routing::get(env_endpoint::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/configprops"),
                axum::routing::get(configprops_endpoint::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/loggers"),
                axum::routing::get(loggers_get::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/loggers/{name}"),
                axum::routing::put(loggers_put::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/logfile"),
                axum::routing::get(logfile_endpoint::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/tasks"),
                axum::routing::get(tasks_endpoint::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/jobs"),
                axum::routing::get(jobs_endpoint::<S>),
            )
            .route(
                &actuator_route_path(prefix, "/ui/tasks"),
                axum::routing::get(ui_tasks::<S>),
            );
        #[cfg(feature = "http-client")]
        {
            router = router
                .route(
                    &actuator_route_path(prefix, "/webhooks/dlq"),
                    axum::routing::get(webhooks_dlq_endpoint::<S>),
                )
                .route(
                    &actuator_route_path(prefix, "/webhooks/replay"),
                    axum::routing::post(webhooks_replay_endpoint::<S>),
                );
        }

        #[cfg(feature = "system-info")]
        {
            router = router.route(
                &actuator_route_path(prefix, "/system"),
                axum::routing::get(crate::system_info::system_info_handler),
            );
        }

        #[cfg(feature = "ws")]
        {
            router = router
                .route(
                    &actuator_route_path(prefix, "/channels"),
                    axum::routing::get(channels_endpoint::<S>),
                )
                .route(
                    &actuator_route_path(prefix, "/tasks/stream"),
                    axum::routing::get(tasks_stream_endpoint::<S>),
                );
        }
    }

    // Nova: Add HTMX UI endpoints available unconditionally like metrics
    router
        .route(
            &actuator_route_path(prefix, "/ui"),
            axum::routing::get(ui_dashboard),
        )
        .route(
            &actuator_route_path(prefix, "/ui/metrics"),
            axum::routing::get(ui_metrics::<S>),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutumnConfig;

    #[test]
    fn task_registry_flow() {
        let registry = TaskRegistry::new();

        registry.register_scheduled(
            "my_task",
            "0 * * * * *",
            crate::task::TaskCoordination::Fleet,
            "mock",
            "node-1",
        );
        let snap1 = registry.snapshot();
        assert_eq!(snap1.get("my_task").unwrap().total_runs, 0);

        registry.record_leader("my_task", "node-1", "mock_tick");
        let snap3 = registry.snapshot();
        assert_eq!(
            snap3.get("my_task").unwrap().current_leader.as_deref(),
            Some("node-1")
        );

        registry.record_start("my_task");
        let snap4 = registry.snapshot();
        assert_eq!(snap4.get("my_task").unwrap().status, "running");

        registry.record_next_run_at("my_task", "tomorrow");
        let snap5 = registry.snapshot();
        assert_eq!(
            snap5.get("my_task").unwrap().next_run_at.as_deref(),
            Some("tomorrow")
        );

        registry.record_success("my_task", 100);
        let snap6 = registry.snapshot();
        assert_eq!(snap6.get("my_task").unwrap().total_runs, 1);
        assert_eq!(snap6.get("my_task").unwrap().last_error, None);

        registry.record_failure("my_task", 150, "error message");
        let snap7 = registry.snapshot();
        assert_eq!(snap7.get("my_task").unwrap().total_runs, 2);
        assert_eq!(snap7.get("my_task").unwrap().total_failures, 1);
        assert_eq!(
            snap7.get("my_task").unwrap().last_error.as_deref(),
            Some("error message")
        );

        let registry2 = TaskRegistry::default();
        assert!(registry2.snapshot().is_empty());
    }
    #[test]
    fn job_registry_flow() {
        let registry = JobRegistry::new();

        registry.register("my_job");
        let snap1 = registry.snapshot();
        assert_eq!(snap1.get("my_job").unwrap().queued, 0);

        registry.record_enqueue("my_job");
        let snap2 = registry.snapshot();
        assert_eq!(snap2.get("my_job").unwrap().queued, 1);

        registry.record_start("my_job");
        let snap3 = registry.snapshot();
        assert_eq!(snap3.get("my_job").unwrap().queued, 0);
        assert_eq!(snap3.get("my_job").unwrap().in_flight, 1);

        registry.record_retry("my_job", "timeout", 1);
        let snap4 = registry.snapshot();
        assert_eq!(snap4.get("my_job").unwrap().in_flight, 0);
        assert_eq!(
            snap4.get("my_job").unwrap().last_error.as_deref(),
            Some("timeout")
        );

        registry.record_enqueue("my_job");
        registry.record_start("my_job");
        registry.record_success("my_job");
        let snap5 = registry.snapshot();
        assert_eq!(snap5.get("my_job").unwrap().in_flight, 0);
        assert_eq!(snap5.get("my_job").unwrap().total_successes, 1);
        assert_eq!(snap5.get("my_job").unwrap().last_error, None);

        registry.record_enqueue("my_job");
        registry.record_cancel("my_job");
        let snap6 = registry.snapshot();
        assert_eq!(snap6.get("my_job").unwrap().queued, 0);
        assert_eq!(snap6.get("my_job").unwrap().in_flight, 0);

        registry.record_enqueue("my_job");
        registry.record_start("my_job");
        registry.record_failure("my_job", "failure".to_string(), true);
        let snap7 = registry.snapshot();
        assert_eq!(snap7.get("my_job").unwrap().in_flight, 0);
        assert_eq!(snap7.get("my_job").unwrap().total_failures, 1);
        assert_eq!(snap7.get("my_job").unwrap().dead_letters, 1);
        assert_eq!(
            snap7.get("my_job").unwrap().last_error.as_deref(),
            Some("failure")
        );

        let registry2 = JobRegistry::default();
        let snap8 = registry2.snapshot();
        assert!(snap8.is_empty());
    }
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[derive(Clone)]
    struct TestActuatorState {
        profile: String,
        deploy_version: String,
        metrics: crate::middleware::MetricsCollector,
        log_levels: LogLevels,
        task_registry: TaskRegistry,
        job_registry: JobRegistry,
        config_props: ConfigProperties,
        metrics_source_registry: MetricsSourceRegistry,
        health_indicator_registry: HealthIndicatorRegistry,
        health_detailed: bool,
        log_buffer: Option<crate::log::capture::LogBuffer>,
        #[cfg(feature = "http-client")]
        webhook_outbound: Option<crate::webhook_outbound::WebhookOutboundManager>,
        #[cfg(feature = "db")]
        pool: Option<
            diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        >,
        #[cfg(feature = "db")]
        shards: Option<crate::sharding::ShardSet>,
        #[cfg(feature = "ws")]
        channels: crate::channels::Channels,
        #[cfg(feature = "ws")]
        shutdown: tokio_util::sync::CancellationToken,
    }

    impl ProvideActuatorState for TestActuatorState {
        fn metrics(&self) -> &crate::middleware::MetricsCollector {
            &self.metrics
        }
        fn log_levels(&self) -> &LogLevels {
            &self.log_levels
        }
        fn task_registry(&self) -> &TaskRegistry {
            &self.task_registry
        }
        fn job_registry(&self) -> &JobRegistry {
            &self.job_registry
        }
        fn config_props(&self) -> &ConfigProperties {
            &self.config_props
        }
        fn profile(&self) -> &str {
            &self.profile
        }
        fn uptime_display(&self) -> String {
            "test_uptime".to_string()
        }
        fn deploy_version(&self) -> String {
            self.deploy_version.clone()
        }
        fn metrics_source_registry(&self) -> Option<&MetricsSourceRegistry> {
            Some(&self.metrics_source_registry)
        }
        #[cfg(feature = "http-client")]
        fn webhook_outbound(&self) -> Option<crate::webhook_outbound::WebhookOutboundManager> {
            self.webhook_outbound.clone()
        }
        #[cfg(feature = "db")]
        fn pool(
            &self,
        ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>>
        {
            self.pool.as_ref()
        }
        #[cfg(feature = "db")]
        fn shards(&self) -> Option<&crate::sharding::ShardSet> {
            self.shards.as_ref()
        }
        #[cfg(feature = "ws")]
        fn channels(&self) -> &crate::channels::Channels {
            &self.channels
        }
        #[cfg(feature = "ws")]
        fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
            self.shutdown.clone()
        }
        fn health_indicator_registry(&self) -> Option<&HealthIndicatorRegistry> {
            Some(&self.health_indicator_registry)
        }
        fn health_detailed(&self) -> bool {
            self.health_detailed
        }
        fn log_buffer(&self) -> Option<crate::log::capture::LogBuffer> {
            self.log_buffer.clone()
        }
    }

    fn test_state() -> TestActuatorState {
        test_state_with_config(&AutumnConfig::default())
    }

    fn test_state_with_config(config: &AutumnConfig) -> TestActuatorState {
        TestActuatorState {
            profile: config.profile.clone().unwrap_or_else(|| "dev".into()),
            deploy_version: crate::canary::STABLE.to_owned(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: LogLevels::new("info"),
            task_registry: TaskRegistry::new(),
            job_registry: JobRegistry::new(),
            config_props: ConfigProperties::from_config(config),
            metrics_source_registry: MetricsSourceRegistry::new(),
            health_indicator_registry: HealthIndicatorRegistry::new(),
            health_detailed: config.health.detailed,
            log_buffer: None,
            #[cfg(feature = "http-client")]
            webhook_outbound: None,
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            shards: None,
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[cfg(feature = "http-client")]
    fn test_state_with_webhook_outbound(
        manager: crate::webhook_outbound::WebhookOutboundManager,
    ) -> TestActuatorState {
        let mut state = test_state();
        state.webhook_outbound = Some(manager);
        state
    }

    #[cfg(feature = "http-client")]
    fn replay_test_subscription() -> crate::webhook_outbound::WebhookSubscription {
        crate::webhook_outbound::WebhookSubscription {
            id: "sub-replay".to_string(),
            target_url: "https://example.test/webhook".to_string(),
            event_topics: vec!["order.created".to_string()],
            secret: "secret".to_string(),
            status: crate::webhook_outbound::WebhookSubscriptionStatus::Failed,
            consecutive_failures: 50,
        }
    }

    #[cfg(feature = "http-client")]
    fn replay_test_dlq_log() -> crate::webhook_outbound::WebhookDeliveryLog {
        crate::webhook_outbound::WebhookDeliveryLog {
            id: "log-replay".to_string(),
            subscription_id: "sub-replay".to_string(),
            topic: "order.created".to_string(),
            payload: "{\"id\":123}".to_string(),
            request_headers: std::collections::HashMap::new(),
            response_status: Some(503),
            response_body: Some("unavailable".to_string()),
            elapsed_ms: 42,
            attempt: 5,
            max_attempts: 5,
            is_dlq: true,
            last_error: Some("server returned status: 503".to_string()),
            timestamp: chrono::Utc::now(),
        }
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn webhooks_replay_preserves_dlq_log_and_failures_when_enqueue_is_unavailable() {
        use crate::webhook_outbound::{
            InMemoryOutboundWebhookHandler, OutboundWebhookHandler, WebhookOutboundManager,
        };

        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let handler = Arc::new(InMemoryOutboundWebhookHandler::new());
        handler
            .create_subscription(replay_test_subscription())
            .await
            .expect("subscription setup");
        let original_log = replay_test_dlq_log();
        handler
            .log_delivery(original_log.clone())
            .await
            .expect("dlq log setup");
        let failures_before_replay = handler
            .get_subscription("sub-replay")
            .await
            .expect("subscription lookup")
            .expect("subscription should exist")
            .consecutive_failures;

        let state = test_state_with_webhook_outbound(WebhookOutboundManager::new(handler.clone()));
        let response = webhooks_replay_endpoint(
            State(state),
            Json(ReplayRequest {
                log_id: original_log.id.clone(),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let stored_log = handler
            .get_delivery_log(&original_log.id)
            .await
            .expect("delivery log lookup")
            .expect("delivery log should still exist");
        assert!(stored_log.is_dlq, "failed enqueue must keep log in DLQ");
        assert_eq!(stored_log.attempt, original_log.attempt);
        assert_eq!(stored_log.last_error, original_log.last_error);
        assert_eq!(stored_log.response_status, original_log.response_status);
        assert_eq!(stored_log.response_body, original_log.response_body);

        let subscription = handler
            .get_subscription("sub-replay")
            .await
            .expect("subscription lookup")
            .expect("subscription should exist");
        assert_eq!(
            subscription.consecutive_failures, failures_before_replay,
            "failed enqueue must not reset subscription failure history"
        );
        assert_eq!(
            subscription.status,
            crate::webhook_outbound::WebhookSubscriptionStatus::Failed,
            "failed enqueue must not reactivate an auto-failed subscription"
        );

        crate::job::clear_global_job_client();
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn webhooks_replay_rejects_disabled_subscription_without_removing_dlq() {
        use crate::webhook_outbound::{
            InMemoryOutboundWebhookHandler, OutboundWebhookHandler, WebhookOutboundManager,
            WebhookSubscriptionStatus,
        };

        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let handler = Arc::new(InMemoryOutboundWebhookHandler::new());
        let mut subscription = replay_test_subscription();
        subscription.status = WebhookSubscriptionStatus::Disabled;
        subscription.consecutive_failures = 0;
        handler
            .create_subscription(subscription)
            .await
            .expect("subscription setup");
        let original_log = replay_test_dlq_log();
        handler
            .log_delivery(original_log.clone())
            .await
            .expect("dlq log setup");

        let state = test_state_with_webhook_outbound(WebhookOutboundManager::new(handler.clone()));
        let response = webhooks_replay_endpoint(
            State(state),
            Json(ReplayRequest {
                log_id: original_log.id.clone(),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);

        let stored_log = handler
            .get_delivery_log(&original_log.id)
            .await
            .expect("delivery log lookup")
            .expect("delivery log should still exist");
        assert!(stored_log.is_dlq);
        assert_eq!(stored_log.attempt, original_log.attempt);
        assert_eq!(stored_log.response_status, original_log.response_status);
        assert_eq!(stored_log.last_error, original_log.last_error);

        let subscription = handler
            .get_subscription("sub-replay")
            .await
            .expect("subscription lookup")
            .expect("subscription should exist");
        assert_eq!(subscription.status, WebhookSubscriptionStatus::Disabled);

        crate::job::clear_global_job_client();
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn webhooks_replay_rejects_missing_subscription_without_removing_dlq() {
        use crate::webhook_outbound::{
            InMemoryOutboundWebhookHandler, OutboundWebhookHandler, WebhookOutboundManager,
        };

        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let handler = Arc::new(InMemoryOutboundWebhookHandler::new());
        let original_log = replay_test_dlq_log();
        handler
            .log_delivery(original_log.clone())
            .await
            .expect("dlq log setup");

        let runtime_state = crate::AppState::for_test().with_profile("test");
        let shutdown = tokio_util::sync::CancellationToken::new();
        crate::job::start_runtime(
            vec![crate::job::JobInfo {
                name: "autumn_webhook_delivery".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &runtime_state,
            &shutdown,
            &crate::config::JobConfig::default(),
        )
        .expect("job runtime should start");

        let state = test_state_with_webhook_outbound(WebhookOutboundManager::new(handler.clone()));
        let response = webhooks_replay_endpoint(
            State(state),
            Json(ReplayRequest {
                log_id: original_log.id.clone(),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let stored_log = handler
            .get_delivery_log(&original_log.id)
            .await
            .expect("delivery log lookup")
            .expect("delivery log should still exist");
        assert!(stored_log.is_dlq);
        assert_eq!(stored_log.attempt, original_log.attempt);
        assert_eq!(stored_log.response_status, original_log.response_status);
        assert_eq!(stored_log.response_body, original_log.response_body);
        assert_eq!(stored_log.last_error, original_log.last_error);

        assert!(
            handler
                .get_subscription("sub-replay")
                .await
                .expect("subscription lookup")
                .is_none(),
            "test setup should leave the subscription missing"
        );

        shutdown.cancel();
        crate::job::clear_global_job_client();
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn webhooks_replay_resets_log_and_failures_after_enqueue_succeeds() {
        use crate::webhook_outbound::{
            InMemoryOutboundWebhookHandler, OutboundWebhookHandler, WebhookOutboundManager,
        };

        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let handler = Arc::new(InMemoryOutboundWebhookHandler::new());
        handler
            .create_subscription(replay_test_subscription())
            .await
            .expect("subscription setup");
        let original_log = replay_test_dlq_log();
        handler
            .log_delivery(original_log.clone())
            .await
            .expect("dlq log setup");

        let runtime_state = crate::AppState::for_test().with_profile("test");
        let shutdown = tokio_util::sync::CancellationToken::new();
        crate::job::start_runtime(
            vec![crate::job::JobInfo {
                name: "autumn_webhook_delivery".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: |_state, _payload| Box::pin(async move { Ok(()) }),
            }],
            &runtime_state,
            &shutdown,
            &crate::config::JobConfig::default(),
        )
        .expect("job runtime should start");

        let state = test_state_with_webhook_outbound(WebhookOutboundManager::new(handler.clone()));
        let response = webhooks_replay_endpoint(
            State(state),
            Json(ReplayRequest {
                log_id: original_log.id.clone(),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);

        let stored_log = handler
            .get_delivery_log(&original_log.id)
            .await
            .expect("delivery log lookup")
            .expect("delivery log should still exist");
        assert!(!stored_log.is_dlq);
        assert_eq!(stored_log.attempt, 1);
        assert_eq!(stored_log.last_error, None);
        assert_eq!(stored_log.response_status, None);
        assert_eq!(stored_log.response_body, None);

        let subscription = handler
            .get_subscription("sub-replay")
            .await
            .expect("subscription lookup")
            .expect("subscription should exist");
        assert_eq!(subscription.consecutive_failures, 0);
        assert_eq!(
            subscription.status,
            crate::webhook_outbound::WebhookSubscriptionStatus::Active
        );

        shutdown.cancel();
        crate::job::clear_global_job_client();
    }

    #[tokio::test]
    async fn actuator_health_returns_ok() {
        let app = actuator_router(true).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "UP");
        assert_eq!(json["profile"], "dev");
        assert!(json["uptime"].is_string());
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn actuator_health_exposes_after_commit_failure_counter() {
        let app = actuator_router(true).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["autumn_after_commit_failures_total"],
            crate::db::AFTER_COMMIT_FAILURES_TOTAL.load(std::sync::atomic::Ordering::Relaxed),
            "/actuator/health should expose the documented after_commit counter"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn actuator_circuitbreakers_returns_breakers() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();
        let breaker = crate::circuit_breaker::global_registry().get_or_create(
            "actuator_endpoint_test_breaker",
            crate::circuit_breaker::CircuitBreakerPolicy {
                failure_ratio_threshold: 0.5,
                sample_window: std::time::Duration::from_secs(10),
                minimum_sample_count: 2,
                open_duration: std::time::Duration::from_secs(60),
                half_open_trial_count: 2,
            },
        );
        assert_eq!(
            breaker.state(),
            crate::circuit_breaker::CircuitState::Closed
        );

        let mut detailed_config = AutumnConfig::default();
        detailed_config.health.detailed = true;
        let state = test_state_with_config(&detailed_config);
        let app = actuator_router(true).with_state(state);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/actuator/circuitbreakers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let list = json.as_array().expect("Should be a JSON array");
        let item = list
            .iter()
            .find(|i| i["name"] == "actuator_endpoint_test_breaker")
            .expect("Should find our breaker");
        assert_eq!(item["state"], "CLOSED");
        assert_eq!(item["failure_ratio_threshold"], 0.5);
        assert_eq!(item["minimum_sample_count"], 2);

        let mut undetailed_config = AutumnConfig::default();
        undetailed_config.health.detailed = false;
        let undetailed_state = test_state_with_config(&undetailed_config);
        let app_undetailed = actuator_router(true).with_state(undetailed_state);
        let resp_undetailed = app_undetailed
            .oneshot(
                Request::builder()
                    .uri("/actuator/circuitbreakers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp_undetailed.status(), StatusCode::OK);
        let body_undetailed = axum::body::to_bytes(resp_undetailed.into_body(), usize::MAX)
            .await
            .unwrap();
        let json_undetailed: serde_json::Value = serde_json::from_slice(&body_undetailed).unwrap();
        let list_undetailed = json_undetailed.as_array().expect("Should be a JSON array");
        let item_undetailed = list_undetailed
            .iter()
            .find(|i| i["name"] == "actuator_endpoint_test_breaker")
            .expect("Should find our breaker");
        assert_eq!(item_undetailed["state"], "CLOSED");
        assert!(item_undetailed.get("failure_ratio_threshold").is_none());
        assert!(item_undetailed.get("minimum_sample_count").is_none());
        crate::circuit_breaker::global_registry().clear();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_health_hides_circuit_breakers_when_undetailed() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let _breaker = crate::circuit_breaker::global_registry().get_or_create(
            "test_health_hide_breaker",
            crate::circuit_breaker::CircuitBreakerPolicy {
                failure_ratio_threshold: 0.5,
                sample_window: std::time::Duration::from_secs(10),
                minimum_sample_count: 2,
                open_duration: std::time::Duration::from_secs(60),
                half_open_trial_count: 2,
            },
        );

        let mut detailed_config = AutumnConfig::default();
        detailed_config.health.detailed = true;
        let state = test_state_with_config(&detailed_config);
        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["components"]["circuit_breaker.test_health_hide_breaker"].is_object());

        let mut undetailed_config = AutumnConfig::default();
        undetailed_config.health.detailed = false;
        let undetailed_state = test_state_with_config(&undetailed_config);
        let app_undetailed = actuator_router(true).with_state(undetailed_state);
        let resp_undetailed = app_undetailed
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp_undetailed.status(), StatusCode::OK);
        let body_undetailed = axum::body::to_bytes(resp_undetailed.into_body(), usize::MAX)
            .await
            .unwrap();
        let json_undetailed: serde_json::Value = serde_json::from_slice(&body_undetailed).unwrap();

        if let Some(components) = json_undetailed.get("components") {
            assert!(
                components
                    .get("circuit_breaker.test_health_hide_breaker")
                    .is_none()
            );
        }

        crate::circuit_breaker::global_registry().clear();
    }

    #[tokio::test]
    async fn actuator_routes_respect_custom_prefix() {
        let app = actuator_router_with_prefix("/ops", true, true).with_state(test_state());

        let prefixed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ops/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prefixed.status(), StatusCode::OK);

        let legacy = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn actuator_route_helpers_normalize_prefixes() {
        assert_eq!(actuator_route_glob("ops/"), "/ops/*");
        assert_eq!(actuator_route_path("ops/", "/health"), "/ops/health");
        assert_eq!(actuator_route_glob("/"), "/*");
    }

    #[tokio::test]
    async fn actuator_info_returns_metadata() {
        let app = actuator_router(true).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["autumn"]["version"].is_string());
        assert_eq!(json["autumn"]["profile"], "dev");
    }

    #[tokio::test]
    async fn actuator_env_available_in_sensitive_mode() {
        let config = AutumnConfig {
            profile: Some("prod".into()),
            server: crate::config::ServerConfig {
                port: 4100,
                ..crate::config::ServerConfig::default()
            },
            telemetry: crate::config::TelemetryConfig {
                enabled: true,
                service_name: "cloud-app".into(),
                ..crate::config::TelemetryConfig::default()
            },
            health: crate::config::HealthConfig {
                path: "/healthz".into(),
                ..crate::config::HealthConfig::default()
            },
            ..AutumnConfig::default()
        };

        let app = actuator_router(true).with_state(test_state_with_config(&config));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/env")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active_profile"], "prod");
        assert_eq!(json["properties"]["server.port"], "4100");
        assert_eq!(json["properties"]["telemetry.enabled"], "true");
        assert_eq!(json["properties"]["telemetry.service_name"], "cloud-app");
        assert_eq!(json["properties"]["health.path"], "/healthz");
    }

    #[tokio::test]
    async fn actuator_env_hidden_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/env")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn actuator_circuitbreakers_hidden_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/circuitbreakers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn redaction_patterns() {
        assert!(should_redact("database.url"));
        assert!(should_redact("api_token"));
        assert!(should_redact("secret_key"));
        assert!(!should_redact("server.port"));
        assert!(!should_redact("log.level"));
    }

    // ── Metrics endpoint tests ─────────────────────────────────

    #[tokio::test]
    async fn actuator_metrics_returns_http_stats() {
        let state = test_state();
        state.metrics().record("GET", "/test", 200, 10);
        state.metrics().record("POST", "/test", 500, 50);

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["http"]["requests_total"], 2);
        assert_eq!(json["http"]["by_status"]["2xx"], 1);
        assert_eq!(json["http"]["by_status"]["5xx"], 1);
    }

    #[tokio::test]
    async fn actuator_metrics_available_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    #[cfg(feature = "db")]
    async fn actuator_metrics_returns_per_shard_stats_when_sharded() {
        let mut state = test_state();
        let config = crate::config::DatabaseConfig {
            shards: vec![
                crate::config::ShardConfig {
                    name: "alpha".to_owned(),
                    primary_url: "postgres://localhost/alpha".to_owned(),
                    slots: None,
                    replica_url: None,
                    primary_pool_size: Some(4),
                    replica_pool_size: None,
                    replica_fallback: None,
                },
                crate::config::ShardConfig {
                    name: "beta".to_owned(),
                    primary_url: "postgres://localhost/beta".to_owned(),
                    slots: None,
                    replica_url: Some("postgres://localhost/beta_ro".to_owned()),
                    primary_pool_size: None,
                    replica_pool_size: Some(2),
                    replica_fallback: None,
                },
            ],
            ..Default::default()
        };
        state.shards = crate::sharding::create_shard_set(
            &config,
            std::sync::Arc::new(crate::sharding::HashShardRouter),
        )
        .expect("lazy pools build");

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let shards = json
            .get("database_shards")
            .expect("sharded state exposes database_shards");
        assert_eq!(shards["alpha"]["pool_size"], 4);
        assert_eq!(shards["alpha"]["slots"], 8192);
        assert_eq!(shards["beta"]["replica"]["pool_size"], 2);
    }

    #[tokio::test]
    #[cfg(feature = "db")]
    async fn actuator_metrics_returns_db_stats_when_pool_present() {
        use diesel_async::AsyncPgConnection;
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;
        use diesel_async::pooled_connection::deadpool::Pool;

        let mut state = test_state();

        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(
            "postgres://postgres:postgres@localhost:5432/postgres",
        );
        let pool = Pool::builder(manager).build().unwrap();

        state.pool = Some(pool);

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json.get("database").is_some());
    }

    // ── Config properties endpoint tests ───────────────────────

    #[tokio::test]
    async fn actuator_configprops_returns_properties() {
        let app = actuator_router(true).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/configprops")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active_profile"], "dev");
        assert!(json["properties"].is_object());
    }

    #[tokio::test]
    async fn actuator_configprops_hidden_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/configprops")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn configprops_redacts_sensitive_values() {
        let mut props = HashMap::new();
        ConfigProperties::track_property(
            &mut props,
            "database.url",
            "postgres://user:pass@host/db",
            "",
            "dev",
        );
        assert_eq!(props["database.url"].value, "****");
    }

    #[test]
    fn configprops_tracks_default_source() {
        let mut props = HashMap::new();
        ConfigProperties::track_property(&mut props, "server.port", "3000", "3000", "dev");
        assert_eq!(props["server.port"].source, "default");
        assert_eq!(props["server.port"].value, "3000");
    }

    #[test]
    fn configprops_tracks_profile_source() {
        let mut props = HashMap::new();
        ConfigProperties::track_property(&mut props, "log.level", "debug", "info", "dev");
        assert_eq!(props["log.level"].source, "profile_default:dev");
    }

    // ── Loggers endpoint tests ─────────────────────────────────

    #[tokio::test]
    async fn actuator_loggers_get_returns_levels() {
        let app = actuator_router(true).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/loggers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["current_level"], "info");
        assert!(json["available_levels"].is_array());
    }

    #[tokio::test]
    async fn actuator_loggers_put_changes_level() {
        let state = test_state();
        let app = actuator_router(true).with_state(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/actuator/loggers/autumn_web")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"level": "debug"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["message"], "Logger 'autumn_web' set to 'debug'");

        let overrides = state.log_levels().logger_overrides();
        assert_eq!(
            overrides.get("autumn_web").map(String::as_str),
            Some("debug")
        );
    }

    #[tokio::test]
    async fn actuator_loggers_put_rejects_invalid_level() {
        let app = actuator_router(true).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/actuator/loggers/autumn_web")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"level": "banana"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "error");
    }

    #[tokio::test]
    async fn actuator_loggers_hidden_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/loggers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn log_levels_set_and_get() {
        let levels = LogLevels::new("info");
        assert_eq!(levels.current_level(), "info");

        let _ = levels.set_logger_level("my_crate", "debug");
        let overrides = levels.logger_overrides();
        assert_eq!(overrides.get("my_crate").map(String::as_str), Some("debug"));
    }

    #[test]
    fn log_levels_root_updates_current() {
        let levels = LogLevels::new("info");
        let prev = levels.set_logger_level("root", "trace");
        assert_eq!(prev, Some("info".to_string()));
        assert_eq!(levels.current_level(), "trace");
    }

    // ── Prometheus endpoint tests ──────────────────────────────

    #[tokio::test]
    async fn actuator_prometheus_returns_metrics() {
        let state = test_state();
        state.metrics().record("GET", "/test", 200, 10);
        state.metrics().record("POST", "/test", 500, 50);

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/plain; version=0.0.4"
        );

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("# HELP autumn_http_requests_total Total number of HTTP requests"));
        assert!(text.contains("# TYPE autumn_http_requests_total counter"));
        assert!(text.contains("autumn_http_requests_total{version=\"stable\"} 2"));

        assert!(text.contains("autumn_http_requests_active{version=\"stable\"} "));
        assert!(text.contains("autumn_http_responses_total{version=\"stable\",status=\"2xx\"} 1"));
        assert!(text.contains("autumn_http_responses_total{version=\"stable\",status=\"5xx\"} 1"));

        // Latency percentiles are exposed in seconds, labelled by version.
        assert!(text.contains("# TYPE autumn_http_request_duration_seconds summary"));
        assert!(text.contains(
            "autumn_http_request_duration_seconds{version=\"stable\",quantile=\"0.99\"}"
        ));

        assert!(text.contains(
            "autumn_http_route_requests_total{version=\"stable\",method=\"GET\",route=\"/test\"} 1"
        ));
        assert!(text.contains(
            "autumn_http_route_requests_total{version=\"stable\",method=\"POST\",route=\"/test\"} 1"
        ));

        assert!(text.contains("# HELP autumn_request_timeouts_total"));
        assert!(text.contains("# TYPE autumn_request_timeouts_total counter"));
        assert!(text.contains("autumn_request_timeouts_total{version=\"stable\"} 0"));

        assert!(text.contains("# HELP autumn_read_your_writes_pins_total"));
        assert!(text.contains("# TYPE autumn_read_your_writes_pins_total counter"));
        assert!(text.contains("autumn_read_your_writes_pins_total{version=\"stable\"} 0"));
    }

    #[cfg(feature = "cache-moka")]
    #[tokio::test]
    async fn prometheus_includes_cache_read_through_counters() {
        // read_through_metrics() is a process-wide singleton shared with other
        // tests in this crate; assert presence and a monotonic delta rather
        // than an absolute value.
        let before = crate::cache::read_through_metrics().snapshot();
        let cache: std::sync::Arc<dyn crate::cache::Cache> =
            std::sync::Arc::new(crate::cache::MokaCache::new(10, None));
        let key = format!("actuator-prometheus-cache-test-{before:?}");
        let _: i32 =
            crate::cache::get_or_compute(
                &cache,
                &key,
                None,
                || async move { Ok::<i32, String>(1) },
            )
            .await
            .unwrap();
        let after = crate::cache::read_through_metrics().snapshot();
        assert!(after.fills > before.fills, "fills counter must increase");

        let state = test_state();
        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        for name in [
            "autumn_cache_read_through_hits_total",
            "autumn_cache_read_through_misses_total",
            "autumn_cache_read_through_coalesced_waits_total",
            "autumn_cache_read_through_fills_total",
            "autumn_cache_read_through_fill_failures_total",
            "autumn_cache_read_through_stale_serves_total",
            "autumn_cache_fill_lock_acquires_total",
            "autumn_cache_fill_lock_contended_total",
        ] {
            assert!(
                text.contains(&format!("# TYPE {name} counter")),
                "missing TYPE line for {name} in:\n{text}"
            );
            assert!(
                text.contains(&format!("{name}{{version=\"stable\"}}")),
                "missing value line for {name} in:\n{text}"
            );
        }
    }

    #[cfg(feature = "cache-moka")]
    #[tokio::test]
    async fn metrics_json_includes_cache_section() {
        let cache: std::sync::Arc<dyn crate::cache::Cache> =
            std::sync::Arc::new(crate::cache::MokaCache::new(10, None));
        let key = "actuator-metrics-json-cache-test";
        let _: i32 =
            crate::cache::get_or_compute(&cache, key, None, || async move { Ok::<i32, String>(1) })
                .await
                .unwrap();

        let state = test_state();
        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["cache"]["fills"].as_u64().unwrap() >= 1);
        assert!(json["cache"]["hits"].is_u64());
        assert!(json["cache"]["misses"].is_u64());
        assert!(json["cache"]["coalesced_waits"].is_u64());
        assert!(json["cache"]["fill_failures"].is_u64());
        assert!(json["cache"]["stale_serves"].is_u64());
        assert!(json["cache"]["fill_lock_acquires"].is_u64());
        assert!(json["cache"]["fill_lock_contended"].is_u64());
    }

    #[tokio::test]
    async fn actuator_prometheus_labels_metrics_with_canary_version() {
        // A replica whose deploy_version() is "canary" must tag its metric
        // families with version="canary" so a controller can compare cohorts.
        let mut state = test_state();
        state.deploy_version = crate::canary::CANARY.to_owned();
        // Latencies in ms: spread so p50 < p95/p99 and the slowest is 1200 ms.
        state.metrics().record("GET", "/test", 200, 10);
        state.metrics().record("GET", "/test", 200, 20);
        state.metrics().record("GET", "/test", 500, 1200);

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("autumn_http_requests_total{version=\"canary\"} 3"));
        assert!(text.contains("autumn_http_responses_total{version=\"canary\",status=\"5xx\"} 1"));
        // Must not leak the default "stable" label when running as canary.
        assert!(!text.contains("version=\"stable\""));

        // Verify the percentile math: values are reported in seconds (ms / 1000)
        // and satisfy the quantile invariant p50 <= p95 <= p99.
        let quantile = |q: &str| -> f64 {
            let needle = format!(
                "autumn_http_request_duration_seconds{{version=\"canary\",quantile=\"{q}\"}} "
            );
            let line = text
                .lines()
                .find(|l| l.starts_with(&needle))
                .unwrap_or_else(|| panic!("missing duration line for quantile {q}"));
            line[needle.len()..].trim().parse().unwrap()
        };
        let (p50, p95, p99) = (quantile("0.5"), quantile("0.95"), quantile("0.99"));
        assert!(p50 <= p95, "p50 ({p50}) must be <= p95 ({p95})");
        assert!(p95 <= p99, "p95 ({p95}) must be <= p99 ({p99})");
        // Slowest sample was 1200 ms, so the top quantile must read 1.2 seconds.
        assert!(
            (p99 - 1.2).abs() < f64::EPSILON,
            "p99 should be 1.2s, got {p99}"
        );
    }

    #[tokio::test]
    async fn actuator_prometheus_available_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn actuator_prometheus_available_when_export_enabled_and_nonsensitive() {
        // Metrics export decoupled from sensitive: prometheus is reachable even
        // though sensitive endpoints (env/configprops/loggers/tasks) are not.
        let app = actuator_router_with_prefix("/actuator", false, true).with_state(test_state());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Sensitive surfaces stay closed under the non-sensitive metrics config.
        for sensitive_path in [
            "/actuator/env",
            "/actuator/configprops",
            "/actuator/loggers",
            "/actuator/tasks",
            "/actuator/jobs",
            "/actuator/ui/tasks",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(sensitive_path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{sensitive_path} should be unavailable when actuator is non-sensitive"
            );
        }
    }

    #[tokio::test]
    async fn actuator_prometheus_unavailable_when_export_disabled() {
        // Regression: with metrics export disabled, the scrape endpoint is gone.
        let app = actuator_router_with_prefix("/actuator", false, false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn actuator_prometheus_unavailable_when_export_disabled_even_if_sensitive() {
        // Disabling export wins even when sensitive endpoints are enabled.
        let app = actuator_router_with_prefix("/actuator", true, false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn actuator_endpoint_paths_respects_prometheus_toggle() {
        let enabled = actuator_endpoint_paths("/actuator", false, true);
        assert!(
            enabled.iter().any(|p| p == "/actuator/prometheus"),
            "prometheus path should be listed when export is enabled: {enabled:?}"
        );

        let disabled = actuator_endpoint_paths("/actuator", false, false);
        assert!(
            !disabled.iter().any(|p| p == "/actuator/prometheus"),
            "prometheus path should be absent when export is disabled: {disabled:?}"
        );
    }

    // ── Tasks endpoint tests ───────────────────────────────────

    #[tokio::test]
    async fn actuator_tasks_returns_registered_tasks() {
        let state = test_state();
        state.task_registry().register("cleanup", "every 5m");
        state.task_registry().record_start("cleanup");
        state.task_registry().record_success("cleanup", 150);

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let task = &json["scheduled_tasks"]["cleanup"];
        assert_eq!(task["schedule"], "every 5m");
        assert_eq!(task["status"], "idle");
        assert_eq!(task["total_runs"], 1);
        assert_eq!(task["total_failures"], 0);
        assert_eq!(task["last_result"], "ok");
        assert_eq!(task["last_duration_ms"], 150);
    }

    #[tokio::test]
    async fn actuator_jobs_returns_registered_jobs() {
        let state = test_state();
        state.job_registry().register("send_email");
        state.job_registry().record_enqueue("send_email");
        state.job_registry().record_start("send_email");
        state.job_registry().record_success("send_email");

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/jobs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let job = &json["jobs"]["send_email"];
        assert_eq!(job["queued"], 0);
        assert_eq!(job["in_flight"], 0);
        assert_eq!(job["total_successes"], 1);
        assert_eq!(job["total_failures"], 0);
    }

    #[cfg(feature = "ws")]
    #[tokio::test]
    async fn actuator_channels_returns_metrics() {
        let state = test_state();
        let mut rx = state.channels().subscribe("feed");
        state
            .channels()
            .broadcast()
            .publish("feed", "hello")
            .expect("publish should succeed");
        rx.try_recv().expect("subscriber should receive payload");

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/channels")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let feed = &json["channels"]["feed"];
        assert_eq!(feed["subscriber_count"], 1);
        assert_eq!(feed["lifetime_publish_count"], 1);
        assert_eq!(feed["dropped_count"], 0);
        assert_eq!(feed["lagged_count"], 0);
    }

    #[tokio::test]
    async fn actuator_tasks_hidden_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn task_registry_records_failure() {
        let registry = TaskRegistry::new();
        registry.register("my_task", "cron 0 * * * *");
        registry.record_start("my_task");
        registry.record_failure("my_task", 200, "connection refused");

        let snapshot = registry.snapshot();
        let task = &snapshot["my_task"];
        assert_eq!(task.status, "idle");
        assert_eq!(task.total_runs, 1);
        assert_eq!(task.total_failures, 1);
        assert_eq!(task.last_result.as_deref(), Some("failed"));
        assert_eq!(task.last_error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn task_registry_empty_snapshot() {
        let registry = TaskRegistry::new();
        assert!(registry.snapshot().is_empty());
    }
    #[test]
    fn log_levels_rejects_new_key_at_capacity() {
        let levels = LogLevels::new("info");
        // Fill to capacity
        for i in 0..1000 {
            let _ = levels.set_logger_level(&format!("logger_{i}"), "debug");
        }

        // Try to add a new key, should be rejected
        let result = levels.set_logger_level("logger_1000", "warn");
        assert_eq!(result, None);
        assert_eq!(levels.logger_overrides().len(), 1000);
        assert_eq!(levels.logger_overrides().get("logger_1000"), None);
    }

    #[test]
    fn log_levels_accepts_existing_key_at_capacity() {
        let levels = LogLevels::new("info");
        // Fill to capacity
        for i in 0..1000 {
            let _ = levels.set_logger_level(&format!("logger_{i}"), "debug");
        }

        // Try to update an existing key, should succeed
        let prev = levels.set_logger_level("logger_999", "warn");
        assert_eq!(prev.as_deref(), Some("debug"));
        assert_eq!(levels.logger_overrides().len(), 1000);
        assert_eq!(
            levels
                .logger_overrides()
                .get("logger_999")
                .map(String::as_str),
            Some("warn")
        );
    }

    #[test]
    fn task_registry_records_multiple_successes_and_failures() {
        let registry = TaskRegistry::new();
        registry.register("my_task", "cron * * * * *");

        // 1st success
        registry.record_start("my_task");
        registry.record_success("my_task", 100);

        // 2nd success
        registry.record_start("my_task");
        registry.record_success("my_task", 110);

        let snapshot = registry.snapshot();
        let task = &snapshot["my_task"];
        assert_eq!(task.total_runs, 2);
        assert_eq!(task.total_failures, 0);

        // 1st failure
        registry.record_start("my_task");
        registry.record_failure("my_task", 50, "failed");

        let snapshot2 = registry.snapshot();
        let task2 = &snapshot2["my_task"];
        assert_eq!(task2.total_runs, 3);
        assert_eq!(task2.total_failures, 1);
    }

    #[test]
    fn configprops_tracks_custom_profile() {
        let mut props = HashMap::new();
        ConfigProperties::track_property(
            &mut props,
            "log.level",
            "debug",
            "info",
            "custom_profile",
        );
        assert_eq!(props["log.level"].source, "autumn.toml");
    }

    #[test]
    fn configprops_tracks_dev_prod_profiles() {
        let mut props = HashMap::new();
        ConfigProperties::track_property(&mut props, "log.level", "debug", "info", "dev");
        assert_eq!(props["log.level"].source, "profile_default:dev");

        ConfigProperties::track_property(&mut props, "log.format", "json", "text", "prod");
        assert_eq!(props["log.format"].source, "profile_default:prod");
    }

    #[test]
    fn configprops_returns_default_when_values_match() {
        let mut props = HashMap::new();
        ConfigProperties::track_property(&mut props, "log.level", "info", "info", "dev");
        assert_eq!(props["log.level"].source, "default");
    }

    #[tokio::test]
    async fn actuator_ui_dashboard_returns_html_or_unimplemented() {
        let app = actuator_router(true).with_state(test_state());

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/ui")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        if cfg!(feature = "maud") {
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(
                res.headers().get("content-type").unwrap(),
                "text/html; charset=utf-8"
            );
        } else {
            assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        }
    }

    #[tokio::test]
    async fn actuator_ui_metrics_returns_html_or_unimplemented() {
        let app = actuator_router(true).with_state(test_state());

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/ui/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        if cfg!(feature = "maud") {
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(
                res.headers().get("content-type").unwrap(),
                "text/html; charset=utf-8"
            );
        } else {
            assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        }
    }

    #[tokio::test]
    async fn actuator_ui_tasks_returns_html_or_unimplemented() {
        let app = actuator_router(true).with_state(test_state());

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/ui/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        if cfg!(feature = "maud") {
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(
                res.headers().get("content-type").unwrap(),
                "text/html; charset=utf-8"
            );
        } else {
            assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
        }
    }

    #[tokio::test]
    async fn test_actuator_router_calls_prefix_variant() {
        // The `actuator_router` function is a convenience wrapper around `actuator_router_with_prefix`
        // using "/actuator" as the prefix. We can test it by building it and hitting one of the endpoints.
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── RED: /actuator/a11y endpoint ───────────────────────────────

    #[tokio::test]
    async fn actuator_a11y_returns_posture_json() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/a11y")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["lang_set"].is_boolean(), "{json}");
        assert!(json["skip_link_present"].is_boolean(), "{json}");
        assert!(json["landmark_regions_present"].is_boolean(), "{json}");
    }

    #[tokio::test]
    async fn actuator_a11y_available_in_nonsensitive_mode() {
        let app = actuator_router(false).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/a11y")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn actuator_a11y_posture_default_values() {
        let app = actuator_router(true).with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/a11y")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Default test state should report false for all posture fields
        assert_eq!(json["lang_set"], false, "{json}");
        assert_eq!(json["skip_link_present"], false, "{json}");
        assert_eq!(json["landmark_regions_present"], false, "{json}");
    }

    #[test]
    fn a11y_posture_all_passing_is_compliant() {
        let posture = A11yPosture {
            lang_set: true,
            skip_link_present: true,
            landmark_regions_present: true,
        };
        assert!(posture.is_compliant());
    }

    #[test]
    fn a11y_posture_missing_lang_is_not_compliant() {
        let posture = A11yPosture {
            lang_set: false,
            skip_link_present: true,
            landmark_regions_present: true,
        };
        assert!(!posture.is_compliant());
    }

    #[tokio::test]
    async fn actuator_a11y_endpoint_paths_includes_a11y() {
        let paths = actuator_endpoint_paths("/actuator", false, true);
        assert!(
            paths.iter().any(|p| p == "/actuator/a11y"),
            "a11y path not found in: {paths:?}"
        );
    }

    // ── MetricsSource / MetricsSourceRegistry tests ────────────

    #[test]
    fn metrics_source_registry_registers_and_collects() {
        struct FixedSource;
        impl MetricsSource for FixedSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "plugin_requests_total".to_string(),
                    help: "Plugin request count".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![MetricSample {
                        labels: vec![],
                        value: 42.0,
                    }],
                }]
            }
        }

        let registry = MetricsSourceRegistry::new();
        registry
            .register("myplugin", Arc::new(FixedSource))
            .unwrap();

        let all = registry.collect_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "myplugin");
        assert_eq!(all[0].1[0].name, "plugin_requests_total");
        assert!((all[0].1[0].samples[0].value - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_source_registry_rejects_duplicate_name() {
        struct EmptySource;
        impl MetricsSource for EmptySource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![]
            }
        }

        let registry = MetricsSourceRegistry::new();
        registry.register("dup", Arc::new(EmptySource)).unwrap();
        let result = registry.register("dup", Arc::new(EmptySource));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("dup"));
    }

    #[test]
    fn metrics_source_registry_isolates_panicking_source() {
        struct PanickingSource;
        impl MetricsSource for PanickingSource {
            fn collect(&self) -> Vec<MetricFamily> {
                panic!("source panicked!")
            }
        }

        let registry = MetricsSourceRegistry::new();
        registry
            .register("panicker", Arc::new(PanickingSource))
            .unwrap();

        let all = registry.collect_all();
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0].1.len(),
            0,
            "panicking source should yield no families"
        );

        let errors = registry.error_counts();
        assert_eq!(errors.get("panicker"), Some(&1));
    }

    #[tokio::test]
    async fn prometheus_endpoint_includes_plugin_source_families() {
        struct GaugeSource;
        impl MetricsSource for GaugeSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "plugin_queue_depth".to_string(),
                    help: "Plugin queue depth".to_string(),
                    kind: MetricKind::Gauge,
                    samples: vec![MetricSample {
                        labels: vec![("shard".to_string(), "a".to_string())],
                        value: 7.0,
                    }],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("gauge_plugin", Arc::new(GaugeSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(
            text.contains("# HELP plugin_queue_depth Plugin queue depth"),
            "missing HELP line in:\n{text}"
        );
        assert!(
            text.contains("# TYPE plugin_queue_depth gauge"),
            "missing TYPE line in:\n{text}"
        );
        assert!(
            text.contains("plugin_queue_depth{shard=\"a\"} 7"),
            "missing sample line in:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_emits_error_counter_for_panicking_source() {
        struct PanickingSource;
        impl MetricsSource for PanickingSource {
            fn collect(&self) -> Vec<MetricFamily> {
                panic!("oops")
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("panic_src", Arc::new(PanickingSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(
            text.contains("autumn_metrics_source_errors_total{source=\"panic_src\"} 1"),
            "missing error counter in:\n{text}"
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_includes_sources_section() {
        struct SampleSource;
        impl MetricsSource for SampleSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "custom_counter".to_string(),
                    help: "A custom counter".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![MetricSample {
                        labels: vec![],
                        value: 5.0,
                    }],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("my_source", Arc::new(SampleSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(
            json.get("sources").is_some(),
            "metrics JSON missing 'sources' key"
        );
        assert!(
            json["sources"].get("my_source").is_some(),
            "sources missing 'my_source' key"
        );
    }

    #[test]
    fn metrics_source_registry_preserves_insertion_order() {
        struct NamedSource(&'static str);
        impl MetricsSource for NamedSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: self.0.to_string(),
                    help: String::new(),
                    kind: MetricKind::Counter,
                    samples: vec![],
                }]
            }
        }

        let registry = MetricsSourceRegistry::new();
        registry
            .register("alpha", Arc::new(NamedSource("alpha_metric")))
            .unwrap();
        registry
            .register("beta", Arc::new(NamedSource("beta_metric")))
            .unwrap();
        registry
            .register("gamma", Arc::new(NamedSource("gamma_metric")))
            .unwrap();

        let all = registry.collect_all();
        assert_eq!(all[0].0, "alpha");
        assert_eq!(all[1].0, "beta");
        assert_eq!(all[2].0, "gamma");
    }

    // ── render_plugin_sources edge-case coverage ──────────────────────────

    #[test]
    fn escape_help_text_escapes_backslash_and_newline() {
        assert_eq!(escape_help_text("a\\b\nc"), "a\\\\b\\nc");
        assert_eq!(escape_help_text("plain"), "plain");
        assert_eq!(escape_help_text(""), "");
    }

    #[test]
    fn format_sample_value_handles_special_floats() {
        assert_eq!(format_sample_value(f64::INFINITY), "+Inf");
        assert_eq!(format_sample_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_sample_value(f64::NAN), "NaN");
        assert_eq!(format_sample_value(0.0), "0");
        assert_eq!(format_sample_value(1.5), "1.5");
    }

    #[test]
    fn is_valid_metric_name_accepts_valid_and_rejects_invalid() {
        assert!(is_valid_metric_name("http_requests_total"));
        assert!(is_valid_metric_name("_private"));
        assert!(is_valid_metric_name("ns:metric"));
        assert!(!is_valid_metric_name(""));
        assert!(!is_valid_metric_name("0starts_with_digit"));
        assert!(!is_valid_metric_name("has-hyphen"));
    }

    #[test]
    fn is_valid_label_name_accepts_valid_and_rejects_invalid() {
        assert!(is_valid_label_name("shard"));
        assert!(is_valid_label_name("_internal"));
        assert!(is_valid_label_name("a1"));
        assert!(!is_valid_label_name(""));
        assert!(!is_valid_label_name("0starts_digit"));
        assert!(!is_valid_label_name("has-hyphen"));
        assert!(!is_valid_label_name("has.dot"));
    }

    #[tokio::test]
    async fn prometheus_endpoint_skips_family_with_invalid_metric_name() {
        struct BadNameSource;
        impl MetricsSource for BadNameSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![
                    MetricFamily {
                        name: "invalid-name".to_string(),
                        help: "should be skipped".to_string(),
                        kind: MetricKind::Counter,
                        samples: vec![],
                    },
                    MetricFamily {
                        name: "valid_name".to_string(),
                        help: "should appear".to_string(),
                        kind: MetricKind::Counter,
                        samples: vec![MetricSample {
                            labels: vec![],
                            value: 1.0,
                        }],
                    },
                ]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("bad_name_src", Arc::new(BadNameSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(
            !text.contains("invalid-name"),
            "invalid family must be skipped:\n{text}"
        );
        assert!(
            text.contains("valid_name"),
            "valid family must appear:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_skips_sample_with_invalid_label_key() {
        // A sample containing an invalid label key (bad-key) must be skipped
        // entirely — not emitted with the bad key dropped — to avoid creating
        // a phantom duplicate series in the Prometheus scrape.
        struct DirtyLabelsSource;
        impl MetricsSource for DirtyLabelsSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "dirty_labels_metric".to_string(),
                    help: "test".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![
                        MetricSample {
                            labels: vec![
                                ("good".to_string(), "a".to_string()),
                                ("bad-key".to_string(), "b".to_string()),
                            ],
                            value: 1.0,
                        },
                        MetricSample {
                            labels: vec![("good".to_string(), "a".to_string())],
                            value: 2.0,
                        },
                    ],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("dirty", Arc::new(DirtyLabelsSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        // First sample (bad-key) must be absent entirely
        assert!(
            !text.contains("dirty_labels_metric{good=\"a\"} 1"),
            "sample with invalid label key must be skipped:\n{text}"
        );
        // Second (clean) sample must still appear
        assert!(
            text.contains("dirty_labels_metric{good=\"a\"} 2"),
            "clean sample must appear:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_deduplicates_label_keys() {
        struct DupLabelSource;
        impl MetricsSource for DupLabelSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "dup_label_metric".to_string(),
                    help: "test".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![MetricSample {
                        labels: vec![
                            ("env".to_string(), "prod".to_string()),
                            ("env".to_string(), "staging".to_string()),
                        ],
                        value: 5.0,
                    }],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("dup_src", Arc::new(DupLabelSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        // Only the first occurrence of `env` is kept
        assert!(
            text.contains("dup_label_metric{env=\"prod\"} 5"),
            "first env value must be kept:\n{text}"
        );
        assert!(
            !text.contains("staging"),
            "duplicate env key value must be dropped:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_escapes_help_text_and_formats_inf() {
        struct SpecialSource;
        impl MetricsSource for SpecialSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "inf_gauge".to_string(),
                    help: "has\\backslash and\nnewline".to_string(),
                    kind: MetricKind::Gauge,
                    samples: vec![
                        MetricSample {
                            labels: vec![("dir".to_string(), "pos".to_string())],
                            value: f64::INFINITY,
                        },
                        MetricSample {
                            labels: vec![("dir".to_string(), "neg".to_string())],
                            value: f64::NEG_INFINITY,
                        },
                    ],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("special", Arc::new(SpecialSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(
            text.contains("# HELP inf_gauge has\\\\backslash and\\nnewline"),
            "help text must be escaped in:\n{text}"
        );
        assert!(
            text.contains("inf_gauge{dir=\"pos\"} +Inf"),
            "must render +Inf in:\n{text}"
        );
        assert!(
            text.contains("inf_gauge{dir=\"neg\"} -Inf"),
            "must render -Inf in:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_skips_duplicate_family_name_across_sources() {
        struct FirstSource;
        impl MetricsSource for FirstSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "shared_counter".to_string(),
                    help: "from first".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![MetricSample {
                        labels: vec![],
                        value: 1.0,
                    }],
                }]
            }
        }
        struct SecondSource;
        impl MetricsSource for SecondSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "shared_counter".to_string(),
                    help: "from second".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![MetricSample {
                        labels: vec![],
                        value: 2.0,
                    }],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("first", Arc::new(FirstSource))
            .unwrap();
        state
            .metrics_source_registry
            .register("second", Arc::new(SecondSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        let occurrences = text.matches("# HELP shared_counter").count();
        assert_eq!(
            occurrences, 1,
            "must emit exactly one HELP block for shared_counter:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_skips_builtin_name_collision() {
        struct ShadowSource;
        impl MetricsSource for ShadowSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "autumn_http_requests_total".to_string(),
                    help: "plugin trying to shadow built-in".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![MetricSample {
                        labels: vec![],
                        value: 999.0,
                    }],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("shadow", Arc::new(ShadowSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        let occurrences = text.matches("# HELP autumn_http_requests_total").count();
        assert_eq!(
            occurrences, 1,
            "built-in must not be shadowed by plugin:\n{text}"
        );
        assert!(
            !text.contains("999"),
            "plugin shadow value must not appear:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_skips_builtin_duration_family_collision() {
        // The new built-in latency family must be in the duplicate guard so a
        // plugin emitting the same family cannot produce a second HELP/TYPE block.
        struct ShadowLatency;
        impl MetricsSource for ShadowLatency {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "autumn_http_request_duration_seconds".to_string(),
                    help: "plugin trying to shadow built-in latency".to_string(),
                    kind: MetricKind::Gauge,
                    samples: vec![MetricSample {
                        labels: vec![],
                        value: 999.0,
                    }],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("shadow_latency", Arc::new(ShadowLatency))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        let occurrences = text
            .matches("# HELP autumn_http_request_duration_seconds")
            .count();
        assert_eq!(
            occurrences, 1,
            "built-in latency family must not be shadowed by plugin:\n{text}"
        );
        assert!(
            !text.contains("999"),
            "plugin shadow value must not appear:\n{text}"
        );
    }

    #[tokio::test]
    async fn prometheus_endpoint_skips_duplicate_series_within_family() {
        struct DupSeriesSource;
        impl MetricsSource for DupSeriesSource {
            fn collect(&self) -> Vec<MetricFamily> {
                vec![MetricFamily {
                    name: "dup_series_metric".to_string(),
                    help: "test".to_string(),
                    kind: MetricKind::Counter,
                    samples: vec![
                        MetricSample {
                            labels: vec![("region".to_string(), "us".to_string())],
                            value: 10.0,
                        },
                        MetricSample {
                            labels: vec![("region".to_string(), "us".to_string())],
                            value: 20.0,
                        },
                    ],
                }]
            }
        }

        let state = test_state();
        state
            .metrics_source_registry
            .register("dup_series", Arc::new(DupSeriesSource))
            .unwrap();

        let app = actuator_router(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/prometheus")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        // First occurrence kept, second skipped
        assert!(
            text.contains("dup_series_metric{region=\"us\"} 10"),
            "first sample must appear:\n{text}"
        );
        assert!(
            !text.contains("dup_series_metric{region=\"us\"} 20"),
            "duplicate series must be dropped:\n{text}"
        );
    }

    // ── RED then GREEN: /actuator/logfile endpoint ─────────────

    fn make_log_buffer_with_entries() -> crate::log::capture::LogBuffer {
        use crate::log::capture::{CapturedLogEntry, LogBuffer};
        use crate::log::filter::ParameterFilter;
        let buf = LogBuffer::new(100, ParameterFilter::default());
        buf.push(CapturedLogEntry {
            timestamp: "2024-01-01T00:00:00.000Z".to_owned(),
            level: "INFO".to_owned(),
            target: "myapp::orders".to_owned(),
            message: "order created".to_owned(),
            fields: {
                let mut m = serde_json::Map::new();
                m.insert("order_id".to_owned(), serde_json::json!("A-1001"));
                m
            },
            request_id: Some("req-abc".to_owned()),
        });
        buf.push(CapturedLogEntry {
            timestamp: "2024-01-01T00:00:01.000Z".to_owned(),
            level: "WARN".to_owned(),
            target: "myapp::payments".to_owned(),
            message: "payment slow".to_owned(),
            fields: serde_json::Map::new(),
            request_id: None,
        });
        buf.push(CapturedLogEntry {
            timestamp: "2024-01-01T00:00:02.000Z".to_owned(),
            level: "ERROR".to_owned(),
            target: "myapp::payments".to_owned(),
            message: "payment failed".to_owned(),
            fields: serde_json::Map::new(),
            request_id: None,
        });
        buf
    }

    #[tokio::test]
    async fn green_logfile_returns_empty_when_capture_disabled() {
        let state = test_state(); // log_buffer = None
        let response =
            logfile_endpoint(State(state), axum::extract::Query(LogfileQuery::default()))
                .await
                .unwrap();
        let body = response.0;
        assert!(!body.capture_enabled);
        assert!(body.entries.is_empty());
        assert_eq!(body.total, 0);
    }

    #[tokio::test]
    async fn green_logfile_returns_all_entries_when_no_filter() {
        let mut state = test_state();
        state.log_buffer = Some(make_log_buffer_with_entries());

        let response =
            logfile_endpoint(State(state), axum::extract::Query(LogfileQuery::default()))
                .await
                .unwrap();
        let body = response.0;
        assert!(body.capture_enabled);
        assert_eq!(body.total, 3);
        assert_eq!(body.entries.len(), 3);
        // newest-last ordering
        assert_eq!(body.entries[0].level, "INFO");
        assert_eq!(body.entries[2].level, "ERROR");
    }

    #[tokio::test]
    async fn green_logfile_level_filter_excludes_info_when_min_warn() {
        let mut state = test_state();
        state.log_buffer = Some(make_log_buffer_with_entries());

        let response = logfile_endpoint(
            State(state),
            axum::extract::Query(LogfileQuery {
                level: Some("warn".to_owned()),
                limit: None,
            }),
        )
        .await
        .unwrap();
        let body = response.0;
        assert_eq!(body.entries.len(), 2);
        assert!(body.entries.iter().all(|e| e.level != "INFO"));
    }

    #[tokio::test]
    async fn green_logfile_limit_returns_most_recent_n() {
        let mut state = test_state();
        state.log_buffer = Some(make_log_buffer_with_entries());

        let response = logfile_endpoint(
            State(state),
            axum::extract::Query(LogfileQuery {
                level: None,
                limit: Some(2),
            }),
        )
        .await
        .unwrap();
        let body = response.0;
        assert_eq!(body.entries.len(), 2);
        // Most recent 2 = WARN and ERROR
        assert_eq!(body.entries[0].level, "WARN");
        assert_eq!(body.entries[1].level, "ERROR");
    }

    #[tokio::test]
    async fn green_logfile_sensitive_fields_in_response_are_served_scrubbed() {
        use crate::log::capture::{CapturedLogEntry, LogBuffer};
        use crate::log::filter::{FILTERED_PLACEHOLDER, ParameterFilter};
        let buf = LogBuffer::new(10, ParameterFilter::default());
        // The layer scrubs before storage; simulate stored entry with scrubbed value.
        buf.push(CapturedLogEntry {
            timestamp: "2024-01-01T00:00:00.000Z".to_owned(),
            level: "INFO".to_owned(),
            target: "auth".to_owned(),
            message: "login attempt".to_owned(),
            fields: {
                let mut m = serde_json::Map::new();
                m.insert(
                    "password".to_owned(),
                    serde_json::Value::String(FILTERED_PLACEHOLDER.to_owned()),
                );
                m
            },
            request_id: None,
        });

        let mut state = test_state();
        state.log_buffer = Some(buf);

        let response =
            logfile_endpoint(State(state), axum::extract::Query(LogfileQuery::default()))
                .await
                .unwrap();
        let body = response.0;
        assert_eq!(
            body.entries[0].fields["password"].as_str().unwrap(),
            FILTERED_PLACEHOLDER,
            "sensitive value must remain scrubbed in the response"
        );
    }

    #[tokio::test]
    async fn green_logfile_invalid_level_returns_400() {
        let state = test_state();
        let result = logfile_endpoint(
            State(state),
            axum::extract::Query(LogfileQuery {
                level: Some("warning".to_owned()), // invalid — should be "warn"
                limit: None,
            }),
        )
        .await;
        let (status, _body) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn green_logfile_endpoint_in_sensitive_router() {
        // The endpoint must be reachable when sensitive=true.
        let state = test_state();
        let app = actuator_router::<TestActuatorState>(true).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/logfile")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn green_logfile_endpoint_not_in_non_sensitive_router() {
        // The endpoint must NOT be reachable when sensitive=false.
        let state = test_state();
        let app = actuator_router::<TestActuatorState>(false).with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/actuator/logfile")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn green_logfile_structured_fields_preserved() {
        let mut state = test_state();
        state.log_buffer = Some(make_log_buffer_with_entries());

        let response =
            logfile_endpoint(State(state), axum::extract::Query(LogfileQuery::default()))
                .await
                .unwrap();
        let body = response.0;
        let first = &body.entries[0];
        assert_eq!(first.target, "myapp::orders");
        assert_eq!(first.fields["order_id"].as_str().unwrap(), "A-1001");
        assert_eq!(first.request_id.as_deref(), Some("req-abc"));
    }
}

#[cfg(test)]
mod health_indicator_tests {
    use super::*;

    struct AlwaysUp;
    impl HealthIndicator for AlwaysUp {
        fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
            Box::pin(async { HealthCheckOutput::up() })
        }
    }

    struct AlwaysDown;
    impl HealthIndicator for AlwaysDown {
        fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
            Box::pin(async { HealthCheckOutput::down() })
        }
    }

    #[test]
    fn health_status_as_str_values() {
        assert_eq!(HealthStatus::Up.as_str(), "UP");
        assert_eq!(HealthStatus::Down.as_str(), "DOWN");
        assert_eq!(HealthStatus::OutOfService.as_str(), "OUT_OF_SERVICE");
        assert_eq!(HealthStatus::Unknown.as_str(), "UNKNOWN");
    }

    #[test]
    fn health_status_is_healthy() {
        assert!(HealthStatus::Up.is_healthy());
        assert!(HealthStatus::Unknown.is_healthy());
        assert!(!HealthStatus::Down.is_healthy());
        assert!(!HealthStatus::OutOfService.is_healthy());
    }

    #[test]
    fn aggregate_status_precedence() {
        assert_eq!(
            HealthIndicatorRegistry::aggregate_status(&[HealthStatus::Up]),
            HealthStatus::Up
        );
        assert_eq!(
            HealthIndicatorRegistry::aggregate_status(&[HealthStatus::Up, HealthStatus::Unknown]),
            HealthStatus::Unknown
        );
        assert_eq!(
            HealthIndicatorRegistry::aggregate_status(&[
                HealthStatus::Unknown,
                HealthStatus::OutOfService
            ]),
            HealthStatus::OutOfService
        );
        assert_eq!(
            HealthIndicatorRegistry::aggregate_status(&[
                HealthStatus::OutOfService,
                HealthStatus::Down
            ]),
            HealthStatus::Down
        );
        assert_eq!(
            HealthIndicatorRegistry::aggregate_status(&[]),
            HealthStatus::Up
        );
    }

    #[tokio::test]
    async fn registry_run_all_collects_results() {
        let registry = HealthIndicatorRegistry::new();
        registry
            .register("svc_a", IndicatorGroup::Readiness, Arc::new(AlwaysUp))
            .unwrap();
        registry
            .register("svc_b", IndicatorGroup::HealthOnly, Arc::new(AlwaysDown))
            .unwrap();

        let results = registry.run_all().await;
        assert!(
            results
                .iter()
                .any(|r| r.name == "svc_a" && r.output.status == HealthStatus::Up)
        );
        assert!(
            results
                .iter()
                .any(|r| r.name == "svc_b" && r.output.status == HealthStatus::Down)
        );
    }

    #[tokio::test]
    async fn registry_run_readiness_filters_health_only() {
        let registry = HealthIndicatorRegistry::new();
        registry
            .register("probe_check", IndicatorGroup::Readiness, Arc::new(AlwaysUp))
            .unwrap();
        registry
            .register(
                "health_only",
                IndicatorGroup::HealthOnly,
                Arc::new(AlwaysDown),
            )
            .unwrap();

        let results = registry.run_readiness().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "probe_check");
    }

    #[tokio::test]
    async fn timed_out_indicator_reports_unknown_with_flag() {
        struct SlowIndicator;
        impl HealthIndicator for SlowIndicator {
            fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
                Box::pin(async {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    HealthCheckOutput::up()
                })
            }
            fn timeout_ms(&self) -> u64 {
                5
            }
        }
        let registry = HealthIndicatorRegistry::new();
        registry
            .register("slow", IndicatorGroup::Readiness, Arc::new(SlowIndicator))
            .unwrap();
        let results = registry.run_all().await;
        let slow_res = results
            .iter()
            .find(|r| r.name == "slow")
            .expect("slow indicator not found");
        assert_eq!(slow_res.output.status, HealthStatus::Unknown);
        assert_eq!(
            slow_res.output.details.get("timed_out"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_circuit_breakers_in_health_indicator_registry() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();
        let registry = HealthIndicatorRegistry::new();
        let breaker = crate::circuit_breaker::global_registry().get_or_create(
            "actuator_test_breaker",
            crate::circuit_breaker::CircuitBreakerPolicy {
                failure_ratio_threshold: 0.5,
                sample_window: std::time::Duration::from_secs(10),
                minimum_sample_count: 2,
                open_duration: std::time::Duration::from_secs(60),
                half_open_trial_count: 2,
            },
        );

        let results = registry.run_all().await;
        let found = results
            .iter()
            .find(|r| r.name == "circuit_breaker.actuator_test_breaker");
        assert!(found.is_some(), "Should find circuit breaker in run_all");
        let result = found.unwrap();
        assert_eq!(result.group, IndicatorGroup::HealthOnly);
        assert_eq!(result.output.status, HealthStatus::Up);
        assert_eq!(result.output.details.get("state").unwrap(), "CLOSED");

        breaker.after_call(false);
        breaker.after_call(false);
        assert_eq!(breaker.state(), crate::circuit_breaker::CircuitState::Open);

        let results = registry.run_all().await;
        let found = results
            .iter()
            .find(|r| r.name == "circuit_breaker.actuator_test_breaker");
        assert_eq!(found.unwrap().output.status, HealthStatus::Down);
        assert_eq!(found.unwrap().output.details.get("state").unwrap(), "OPEN");

        // Transition to HalfOpen manually to check status
        {
            let mut inner = breaker.inner.lock().unwrap();
            inner.state = crate::circuit_breaker::CircuitState::HalfOpen;
            inner.half_open_in_flight = 0;
            inner.half_open_successes = 0;
            inner.half_open_failures = 0;
        }
        assert_eq!(
            breaker.state(),
            crate::circuit_breaker::CircuitState::HalfOpen
        );

        let results = registry.run_all().await;
        let found = results
            .iter()
            .find(|r| r.name == "circuit_breaker.actuator_test_breaker");
        assert_eq!(found.unwrap().output.status, HealthStatus::Down);
        assert_eq!(
            found.unwrap().output.details.get("state").unwrap(),
            "HALF_OPEN"
        );

        let readiness_results = registry.run_readiness().await;
        let found_readiness = readiness_results
            .iter()
            .find(|r| r.name == "circuit_breaker.actuator_test_breaker");
        assert!(
            found_readiness.is_none(),
            "Should NOT find circuit breaker in run_readiness"
        );
        crate::circuit_breaker::global_registry().clear();
    }
}

#[cfg(test)]
mod havoc_proptest {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1))]
        #[test]
        fn log_levels_memory_exhaustion(names in proptest::collection::vec(".*", 5000)) {
            let levels = LogLevels::new("info");
            for name in names {
                let _ = levels.set_logger_level(&name, "debug");
            }
            assert!(levels.logger_overrides().len() <= 1000, "Memory leak: unbounded loggers inserted");
        }
    }
}

// ── Nova: Actuator HTMX Dashboard UI ──────────────────────────

#[cfg(all(feature = "maud", feature = "htmx"))]
async fn ui_dashboard() -> impl IntoResponse {
    let html = maud::html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Autumn Actuator Dashboard" }
                script src="/static/js/htmx.min.js" {}
                style {
                    (crate::ui::tokens::TOKENS_CSS)
                    "body { font-family: var(--font-family); background: var(--bg); color: var(--text); margin: 0; padding: 2rem; }"
                    "h1 { font-size: 1.5rem; font-weight: 600; margin-bottom: 1.5rem; }"
                    ".grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(300px, 1fr)); gap: 1.5rem; }"
                    ".card { background: var(--surface); padding: 1.5rem; border-radius: var(--radius); box-shadow: var(--shadow); }"
                    ".card h2 { font-size: 1.125rem; font-weight: 500; margin-top: 0; margin-bottom: 1rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }"
                    ".stat { display: flex; justify-content: space-between; margin-bottom: 0.5rem; }"
                    ".stat-label { color: var(--text-muted); }"
                    ".stat-value { font-weight: 500; }"
                    ".task-item { border: 1px solid var(--border); padding: 0.75rem; border-radius: 0.375rem; margin-bottom: 0.75rem; }"
                    ".task-name { font-weight: 600; display: block; margin-bottom: 0.25rem; }"
                    ".task-meta { font-size: 0.875rem; color: var(--text-muted); }"
                    ".badge { display: inline-block; padding: 0.125rem 0.375rem; border-radius: 9999px; font-size: 0.75rem; font-weight: 500; }"
                    ".badge-green { background: #dcfce7; color: #166534; }"
                    ".badge-gray { background: #f3f4f6; color: #374151; }"
                    ".badge-red { background: #fee2e2; color: #991b1b; }"
                }
            }
            body {
                h1 { "🍂 Autumn Actuator Dashboard" }
                div class="grid" {
                    div class="card" hx-get="ui/metrics" hx-trigger="load, every 2s" {
                        "Loading metrics..."
                    }
                    div class="card" hx-get="ui/tasks" hx-trigger="load, every 2s" {
                        "Loading tasks..."
                    }
                }
            }
        }
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html.into_string(),
    )
}

#[cfg(not(all(feature = "maud", feature = "htmx")))]
async fn ui_dashboard() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "Maud feature is required for the UI dashboard",
    )
}

#[cfg(all(feature = "maud", feature = "htmx"))]
async fn ui_metrics<S: ProvideActuatorState>(State(state): State<S>) -> impl IntoResponse {
    let metrics = state.metrics().snapshot();
    let uptime = state.uptime_display();

    let html = maud::html! {
        h2 { "System Metrics" }
        div class="stat" {
            span class="stat-label" { "Uptime" }
            span class="stat-value" { (uptime) }
        }
        div class="stat" {
            span class="stat-label" { "Total Requests" }
            span class="stat-value" { (metrics.http.requests_total) }
        }
        div class="stat" {
            span class="stat-label" { "Active Requests" }
            span class="stat-value" { (metrics.http.requests_active) }
        }
        div class="stat" {
            span class="stat-label" { "P95 Latency" }
            span class="stat-value" { (metrics.http.latency_ms.p95) " ms" }
        }
        div class="stat" {
            span class="stat-label" { "P99 Latency" }
            span class="stat-value" { (metrics.http.latency_ms.p99) " ms" }
        }
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html.into_string(),
    )
}

#[cfg(not(all(feature = "maud", feature = "htmx")))]
async fn ui_metrics<S: ProvideActuatorState>() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "Maud feature is required for the UI dashboard",
    )
}

#[cfg(all(feature = "maud", feature = "htmx"))]
async fn ui_tasks<S: ProvideActuatorState>(State(state): State<S>) -> impl IntoResponse {
    let tasks = state.task_registry().snapshot();

    let html = maud::html! {
        h2 { "Background Tasks" }
        @if tasks.is_empty() {
            p class="stat-label" { "No tasks registered." }
        } @else {
            @for (name, task) in tasks.iter() {
                div class="task-item" {
                    span class="task-name" { (name) }
                    div class="task-meta" {
                        @if task.status == "running" {
                            span class="badge badge-green" { "Running" }
                        } @else {
                            span class="badge badge-gray" { "Idle" }
                        }
                        " "
                        "Runs: " (task.total_runs)
                        @if task.total_failures > 0 {
                            " " span class="badge badge-red" { "Failures: " (task.total_failures) }
                        }
                    }
                }
            }
        }
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html.into_string(),
    )
}

#[cfg(not(all(feature = "maud", feature = "htmx")))]
async fn ui_tasks<S: ProvideActuatorState>() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "Maud feature is required for the UI dashboard",
    )
}
