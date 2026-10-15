//! A/B experiments with deterministic bucketing and exposure telemetry.
//!
//! Provides multi-variant experiment assignment with stable per-actor bucketing,
//! sticky assignments, structured exposure events, override support for QA/staff,
//! mutual exclusion groups, and experiment lifecycle management.
//!
//! # Quick start
//!
//! ```rust
//! use autumn_web::experiments::{
//!     ExperimentConfig, ExperimentService, InMemoryExperimentStore, VariantConfig,
//! };
//! use std::sync::Arc;
//!
//! // 1. Create a store and service.
//! let store = Arc::new(InMemoryExperimentStore::new());
//! let svc = ExperimentService::new(store);
//!
//! // 2. Declare an experiment with two 50/50 variants.
//! svc.create(ExperimentConfig::new("checkout_v2", vec![
//!     VariantConfig::new("control", 50),
//!     VariantConfig::new("treatment", 50),
//! ])).unwrap();
//!
//! // 3. Start it.
//! svc.start("checkout_v2").unwrap();
//!
//! // 4. Assign an actor — stable and deterministic.
//! let variant = svc.assign("checkout_v2", "user:1").unwrap();
//! assert!(variant == "control" || variant == "treatment");
//!
//! // 5. Re-assignment returns the same sticky variant.
//! let again = svc.assign("checkout_v2", "user:1").unwrap();
//! assert_eq!(variant, again);
//! ```
//!
//! # Assignment semantics
//!
//! Assignment is deterministic per `(experiment_name, actor_id)` using a
//! FNV-1a 64-bit hash of the UTF-8 string `"<experiment>:<actor>"`, bucketed
//! into `[0, 10 000)`. This hash function is **stable**: the same inputs always
//! produce the same output across restarts, replicas, and library versions.
//! Changing the hash function requires a documented migration path (see
//! [`experiment_bucket`]).
//!
//! Variant selection maps the bucket to a variant proportionally by weight:
//! given variants `[("control", 30), ("treatment", 70)]`, actors with
//! buckets 0–2 999 are assigned `"control"` and the rest `"treatment"`.
//!
//! # Lifecycle
//!
//! Experiments move through states in order:
//!
//! ```text
//! draft ─────── running ──── concluded
//!        │              │
//!        └── archived ──┘   (archived from any state)
//! ```
//!
//! - **Draft**: declared but not yet accepting assignments.
//! - **Running**: assignments + exposures active.
//! - **Concluded**: winner pinned; all actors see the winner; no new exposures emitted.
//! - **Archived**: `assign()` returns `Err(ExperimentError::Archived)`.
//!
//! # Exposure events
//!
//! Every successful `assign()` call on a `Running` experiment emits one
//! [`ExposureRecord`] to the configured [`ExposureSink`]. The default sink
//! logs at `INFO` via `tracing`. Supply a custom sink to forward events to
//! your analytics pipeline.
//!
//! # Mutual exclusion groups
//!
//! Experiments can share a named group. An actor who has already been assigned
//! to any experiment in the group will be excluded from all sibling experiments
//! (`Err(ExperimentError::ExcludedByGroup)`). This prevents interaction effects
//! between experiments targeting the same funnel.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

// ── ExperimentState ──────────────────────────────────────────────────────────

/// Lifecycle state of an experiment.
///
/// Experiments move through `draft → running → concluded`. They may be
/// archived from any state. See module-level documentation for semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentState {
    /// Declared but not yet accepting assignments.
    Draft,
    /// Accepting assignments and emitting exposures.
    Running,
    /// Winner pinned; all actors see the winner variant; no new exposures.
    Concluded,
    /// Archived; `assign()` returns [`ExperimentError::Archived`].
    Archived,
}

impl std::fmt::Display for ExperimentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "draft"),
            Self::Running => write!(f, "running"),
            Self::Concluded => write!(f, "concluded"),
            Self::Archived => write!(f, "archived"),
        }
    }
}

// ── VariantConfig ────────────────────────────────────────────────────────────

/// A single variant in an experiment with its assignment weight.
///
/// Weights are relative — they do not need to sum to 100. For a 30/70 split
/// use `[VariantConfig::new("control", 30), VariantConfig::new("treatment", 70)]`.
/// Equal weights produce equal splits. Use weight `0` to disable a variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantConfig {
    /// Unique variant name within the experiment (e.g. `"control"`, `"treatment_a"`).
    pub name: String,
    /// Relative assignment weight. Use `0` to disable a variant without removing it.
    pub weight: u32,
}

impl VariantConfig {
    /// Create a new variant with the given name and weight.
    #[must_use]
    pub fn new(name: impl Into<String>, weight: u32) -> Self {
        Self {
            name: name.into(),
            weight,
        }
    }
}

// ── ExperimentConfig ─────────────────────────────────────────────────────────

/// Full configuration of a single experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentConfig {
    /// Unique experiment name (e.g. `"checkout_v2"`).
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Current lifecycle state.
    pub state: ExperimentState,
    /// Ordered list of variants and their relative weights.
    pub variants: Vec<VariantConfig>,
    /// Set when the experiment is concluded: name of the winning variant.
    pub winner: Option<String>,
    /// Named mutual-exclusion group. Actors assigned to any experiment in
    /// this group are excluded from all sibling experiments in the group.
    pub exclusion_group: Option<String>,
    /// Wall-clock time the experiment was last modified (seconds since UNIX epoch).
    pub updated_at_secs: u64,
}

impl ExperimentConfig {
    /// Create a new experiment in `Draft` state with the given variants.
    #[must_use]
    pub fn new(name: impl Into<String>, variants: Vec<VariantConfig>) -> Self {
        Self {
            name: name.into(),
            description: None,
            state: ExperimentState::Draft,
            variants,
            winner: None,
            exclusion_group: None,
            updated_at_secs: now_secs(),
        }
    }

    /// Set a human-readable description.
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set the mutual exclusion group name.
    #[must_use]
    pub fn exclusion_group(mut self, group: impl Into<String>) -> Self {
        self.exclusion_group = Some(group.into());
        self
    }
}

// ── Assignment ───────────────────────────────────────────────────────────────

/// A recorded sticky assignment for an actor in an experiment.
///
/// Once an actor is assigned, subsequent `assign()` calls return the same
/// variant without re-computing the bucket (sticky semantics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    /// Experiment name.
    pub experiment: String,
    /// Actor identifier.
    pub actor: String,
    /// Assigned variant name.
    pub variant: String,
    /// `true` when this assignment was produced by a staff/QA override rather
    /// than weight-based bucketing.
    pub is_override: bool,
    /// Wall-clock time of first assignment (seconds since UNIX epoch).
    pub assigned_at_secs: u64,
}

impl Assignment {
    fn new(experiment: &str, actor: &str, variant: &str, is_override: bool) -> Self {
        Self {
            experiment: experiment.to_owned(),
            actor: actor.to_owned(),
            variant: variant.to_owned(),
            is_override,
            assigned_at_secs: now_secs(),
        }
    }
}

// ── ChangeRecord ─────────────────────────────────────────────────────────────

/// A single mutation recorded in the experiment change log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRecord {
    /// Experiment name.
    pub experiment: String,
    /// Human-readable description of the mutation.
    pub mutation: String,
    /// Actor who performed the mutation (username, `"cli"`, etc.).
    pub actor: Option<String>,
    /// Wall-clock time (seconds since UNIX epoch).
    pub timestamp_secs: u64,
}

impl ChangeRecord {
    fn now(experiment: &str, mutation: impl Into<String>, actor: Option<&str>) -> Self {
        Self {
            experiment: experiment.to_owned(),
            mutation: mutation.into(),
            actor: actor.map(str::to_owned),
            timestamp_secs: now_secs(),
        }
    }
}

// ── ExposureRecord ────────────────────────────────────────────────────────────

/// Structured exposure event emitted by each successful `assign()` call.
///
/// Consumers pipe these into their analytics pipeline to join exposures with
/// outcome events (conversions, revenue, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposureRecord {
    /// Experiment name.
    pub experiment: String,
    /// Assigned variant name.
    pub variant: String,
    /// Actor identifier.
    pub actor: String,
    /// Optional request ID for correlation (propagated from the HTTP request).
    pub request_id: Option<String>,
    /// `true` when this assignment was produced by a staff/QA override.
    pub is_override: bool,
    /// Wall-clock time of exposure (seconds since UNIX epoch).
    pub timestamp_secs: u64,
}

// ── ExposureSink ─────────────────────────────────────────────────────────────

/// Pluggable sink for exposure events.
///
/// Every successful `assign()` call on a `Running` experiment emits one
/// [`ExposureRecord`] to the configured sink. The default is
/// [`TracingExposureSink`] which logs the event at `INFO` level via the
/// `tracing` crate.
///
/// # Custom sink
///
/// ```rust,ignore
/// use autumn_web::experiments::{ExposureSink, ExposureRecord, ExperimentService,
///     InMemoryExperimentStore};
/// use std::sync::Arc;
///
/// struct MyAnalyticsSink;
///
/// impl ExposureSink for MyAnalyticsSink {
///     fn record(&self, exposure: ExposureRecord) {
///         // Forward to your analytics pipeline.
///         send_to_segment(&exposure);
///     }
/// }
///
/// let svc = ExperimentService::new(Arc::new(InMemoryExperimentStore::new()))
///     .with_exposure_sink(Arc::new(MyAnalyticsSink));
/// ```
pub trait ExposureSink: Send + Sync + 'static {
    /// Record a single exposure event.
    fn record(&self, exposure: ExposureRecord);
}

/// Default [`ExposureSink`]: emits a structured `tracing::info!` event.
///
/// The event carries `experiment`, `variant`, `actor`, `request_id`, and
/// `is_override` fields so structured logging pipelines (JSON, OTLP) can
/// aggregate exposures without parsing message text.
pub struct TracingExposureSink;

impl ExposureSink for TracingExposureSink {
    fn record(&self, exposure: ExposureRecord) {
        tracing::info!(
            experiment = %exposure.experiment,
            variant    = %exposure.variant,
            actor      = %exposure.actor,
            request_id = exposure.request_id.as_deref().unwrap_or(""),
            is_override = %exposure.is_override,
            "experiment_exposure"
        );
    }
}

/// A no-op [`ExposureSink`] that discards all exposure events.
///
/// Useful in benchmarks or contexts where exposure recording is handled
/// out-of-band.
pub struct NoOpExposureSink;

impl ExposureSink for NoOpExposureSink {
    fn record(&self, _: ExposureRecord) {}
}

/// A recording [`ExposureSink`] that collects all exposure events in memory.
///
/// Primarily useful for integration tests.
///
/// ```rust
/// use autumn_web::experiments::{
///     RecordingExposureSink, ExperimentService, ExperimentConfig,
///     VariantConfig, InMemoryExperimentStore,
/// };
/// use std::sync::Arc;
///
/// let (sink, records) = RecordingExposureSink::new();
/// let store = Arc::new(InMemoryExperimentStore::new());
/// let svc = ExperimentService::new(store)
///     .with_exposure_sink(Arc::new(sink));
/// svc.create(ExperimentConfig::new("exp", vec![
///     VariantConfig::new("control", 1),
///     VariantConfig::new("treatment", 1),
/// ])).unwrap();
/// svc.start("exp").unwrap();
/// svc.assign("exp", "user:1").unwrap();
/// assert_eq!(records.lock().unwrap().len(), 1);
/// ```
pub struct RecordingExposureSink {
    records: Arc<Mutex<Vec<ExposureRecord>>>,
}

impl Default for RecordingExposureSink {
    fn default() -> Self {
        Self::new().0
    }
}

impl RecordingExposureSink {
    /// Create a new recording sink, returning it alongside a shared handle to
    /// the collected records.
    #[must_use]
    pub fn new() -> (Self, Arc<Mutex<Vec<ExposureRecord>>>) {
        let records = Arc::new(Mutex::new(Vec::new()));
        let sink = Self {
            records: Arc::clone(&records),
        };
        (sink, records)
    }
}

impl ExposureSink for RecordingExposureSink {
    fn record(&self, exposure: ExposureRecord) {
        self.records.lock().unwrap().push(exposure);
    }
}

// ── ExperimentStoreError ──────────────────────────────────────────────────────

/// Error from an [`ExperimentStore`] backend.
#[derive(Debug, thiserror::Error)]
pub enum ExperimentStoreError {
    /// The backend reported an I/O or connection failure.
    #[error("experiment store backend error: {0}")]
    Backend(String),
}

// ── ExperimentStore trait ─────────────────────────────────────────────────────

/// Pluggable storage backend for experiments.
///
/// All mutation methods should record an audit trail in the change log
/// (accessible via [`history`](ExperimentStore::history)).
pub trait ExperimentStore: Send + Sync + 'static {
    /// Return the current configuration for `name`, or `None` if unknown.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn get(&self, name: &str) -> Result<Option<ExperimentConfig>, ExperimentStoreError>;

    /// Return all known experiments, sorted by name.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn list(&self) -> Result<Vec<ExperimentConfig>, ExperimentStoreError>;

    /// Insert or update an experiment configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn upsert(&self, config: ExperimentConfig) -> Result<(), ExperimentStoreError>;

    /// Update the lifecycle state of an experiment.
    ///
    /// Set `winner` when concluding; ignored otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn set_state(
        &self,
        name: &str,
        state: ExperimentState,
        winner: Option<&str>,
    ) -> Result<(), ExperimentStoreError>;

    /// Update the variants and weights for an experiment.
    ///
    /// Existing sticky assignments are NOT re-bucketed.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn set_variants(
        &self,
        name: &str,
        variants: Vec<VariantConfig>,
        actor: Option<&str>,
    ) -> Result<(), ExperimentStoreError>;

    /// Return the sticky assignment for `(experiment, actor)`, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn get_assignment(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<Assignment>, ExperimentStoreError>;

    /// Record a sticky assignment.
    ///
    /// # Errors
    ///
    /// Returns the variant that was persisted (the existing row's variant on
    /// conflict, or the newly-inserted variant on first write). Callers must
    /// use this return value so that concurrent first-writers agree on the
    /// same sticky bucket.
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn record_assignment(&self, assignment: Assignment) -> Result<String, ExperimentStoreError>;

    /// Return the override variant name for `(experiment, actor)`, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn get_override(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<String>, ExperimentStoreError>;

    /// Pin `actor` to `variant` in `experiment`, bypassing weight-based bucketing.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn set_override(
        &self,
        experiment: &str,
        actor: &str,
        variant: &str,
    ) -> Result<(), ExperimentStoreError>;

    /// Return `true` if `actor` has an existing assignment in any experiment
    /// belonging to `group`, excluding `exclude_experiment` itself.
    ///
    /// Used to enforce mutual exclusion: once an actor is in one experiment
    /// of a group, they are excluded from all siblings.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn has_assignment_in_group(
        &self,
        actor: &str,
        group: &str,
        exclude_experiment: &str,
    ) -> Result<bool, ExperimentStoreError>;

    /// Return the change log for `experiment` (most-recent first), capped at
    /// `limit` entries. Returns an empty `Vec` for unknown experiments.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn history(
        &self,
        experiment: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRecord>, ExperimentStoreError>;

    /// Load the store before the first read. This can block.
    ///
    /// `AppBuilder::with_experiment_store` calls it once, on its own thread,
    /// when the app installs the store. The default does nothing.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    fn preload(&self) -> Result<(), ExperimentStoreError> {
        Ok(())
    }
}

impl<T: ExperimentStore> ExperimentStore for Arc<T> {
    fn get(&self, name: &str) -> Result<Option<ExperimentConfig>, ExperimentStoreError> {
        (**self).get(name)
    }
    fn list(&self) -> Result<Vec<ExperimentConfig>, ExperimentStoreError> {
        (**self).list()
    }
    fn upsert(&self, config: ExperimentConfig) -> Result<(), ExperimentStoreError> {
        (**self).upsert(config)
    }
    fn set_state(
        &self,
        name: &str,
        state: ExperimentState,
        winner: Option<&str>,
    ) -> Result<(), ExperimentStoreError> {
        (**self).set_state(name, state, winner)
    }
    fn set_variants(
        &self,
        name: &str,
        variants: Vec<VariantConfig>,
        actor: Option<&str>,
    ) -> Result<(), ExperimentStoreError> {
        (**self).set_variants(name, variants, actor)
    }
    fn get_assignment(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<Assignment>, ExperimentStoreError> {
        (**self).get_assignment(experiment, actor)
    }
    fn record_assignment(&self, assignment: Assignment) -> Result<String, ExperimentStoreError> {
        (**self).record_assignment(assignment)
    }
    fn get_override(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<String>, ExperimentStoreError> {
        (**self).get_override(experiment, actor)
    }
    fn set_override(
        &self,
        experiment: &str,
        actor: &str,
        variant: &str,
    ) -> Result<(), ExperimentStoreError> {
        (**self).set_override(experiment, actor, variant)
    }
    fn has_assignment_in_group(
        &self,
        actor: &str,
        group: &str,
        exclude_experiment: &str,
    ) -> Result<bool, ExperimentStoreError> {
        (**self).has_assignment_in_group(actor, group, exclude_experiment)
    }
    fn history(
        &self,
        experiment: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRecord>, ExperimentStoreError> {
        (**self).history(experiment, limit)
    }
    fn preload(&self) -> Result<(), ExperimentStoreError> {
        (**self).preload()
    }
}

// ── InMemoryExperimentStore ───────────────────────────────────────────────────

#[derive(Default)]
struct StoreInner {
    experiments: HashMap<String, ExperimentConfig>,
    assignments: HashMap<(String, String), Assignment>,
    overrides: HashMap<(String, String), String>,
    changes: HashMap<String, Vec<ChangeRecord>>,
}

/// In-memory [`ExperimentStore`] implementation.
///
/// All data is lost when the process exits. Best suited for tests and single-
/// replica development setups where cross-restart persistence is not required.
///
/// For production, use `pg::PgExperimentStore`. It persists assignments across
/// restarts.
#[derive(Default)]
pub struct InMemoryExperimentStore {
    inner: RwLock<StoreInner>,
}

impl InMemoryExperimentStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ExperimentStore for InMemoryExperimentStore {
    fn get(&self, name: &str) -> Result<Option<ExperimentConfig>, ExperimentStoreError> {
        Ok(self.inner.read().unwrap().experiments.get(name).cloned())
    }

    fn list(&self) -> Result<Vec<ExperimentConfig>, ExperimentStoreError> {
        let mut exps: Vec<ExperimentConfig> = {
            let inner = self.inner.read().unwrap();
            inner.experiments.values().cloned().collect()
        };
        exps.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(exps)
    }

    fn upsert(&self, config: ExperimentConfig) -> Result<(), ExperimentStoreError> {
        let name = config.name.clone();
        {
            let mut inner = self.inner.write().unwrap();
            let active_variants: std::collections::HashSet<String> = inner
                .assignments
                .values()
                .filter(|a| a.experiment == name)
                .map(|a| a.variant.clone())
                .collect();

            let new_variants: std::collections::HashSet<&str> =
                config.variants.iter().map(|v| v.name.as_str()).collect();

            for variant in active_variants {
                if !new_variants.contains(variant.as_str()) {
                    return Err(ExperimentStoreError::Backend(format!(
                        "cannot delete variant '{variant}' because it has active assignments"
                    )));
                }
            }

            let exists = inner.experiments.contains_key(&name);
            inner.experiments.insert(name.clone(), config);
            let mutation = if exists { "updated" } else { "created" };
            inner
                .changes
                .entry(name.clone())
                .or_default()
                .push(ChangeRecord::now(&name, mutation, None));
        }
        Ok(())
    }

    fn set_state(
        &self,
        name: &str,
        state: ExperimentState,
        winner: Option<&str>,
    ) -> Result<(), ExperimentStoreError> {
        {
            let mut inner = self.inner.write().unwrap();
            if let Some(exp) = inner.experiments.get_mut(name) {
                exp.state = state;
                if let Some(w) = winner {
                    exp.winner = Some(w.to_owned());
                }
                exp.updated_at_secs = now_secs();
            }
            inner
                .changes
                .entry(name.to_owned())
                .or_default()
                .push(ChangeRecord::now(
                    name,
                    winner.map_or_else(|| format!("state={state}"), |w| format!("concluded={w}")),
                    None,
                ));
        }
        Ok(())
    }

    fn set_variants(
        &self,
        name: &str,
        variants: Vec<VariantConfig>,
        actor: Option<&str>,
    ) -> Result<(), ExperimentStoreError> {
        {
            let mut inner = self.inner.write().unwrap();
            let active_variants: std::collections::HashSet<String> = inner
                .assignments
                .values()
                .filter(|a| a.experiment == name)
                .map(|a| a.variant.clone())
                .collect();

            let new_variants: std::collections::HashSet<&str> =
                variants.iter().map(|v| v.name.as_str()).collect();

            for variant in active_variants {
                if !new_variants.contains(variant.as_str()) {
                    return Err(ExperimentStoreError::Backend(format!(
                        "cannot delete variant '{variant}' because it has active assignments"
                    )));
                }
            }

            if let Some(exp) = inner.experiments.get_mut(name) {
                exp.variants = variants;
                exp.updated_at_secs = now_secs();
            }
            inner
                .changes
                .entry(name.to_owned())
                .or_default()
                .push(ChangeRecord::now(name, "set_weights", actor));
        }
        Ok(())
    }

    fn get_assignment(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<Assignment>, ExperimentStoreError> {
        let inner = self.inner.read().unwrap();
        Ok(inner
            .assignments
            .get(&(experiment.to_owned(), actor.to_owned()))
            .cloned())
    }

    fn record_assignment(&self, assignment: Assignment) -> Result<String, ExperimentStoreError> {
        let mut inner = self.inner.write().unwrap();

        if !assignment.is_override {
            // Find the exclusion group for this experiment.
            if let Some(group) = inner
                .experiments
                .get(&assignment.experiment)
                .and_then(|c| c.exclusion_group.as_ref())
            {
                // Check if there is a sibling assignment in the same group.
                for (exp_name, exp_config) in &inner.experiments {
                    if exp_name == &assignment.experiment {
                        continue;
                    }
                    if exp_config.exclusion_group.as_ref() == Some(group)
                        && inner
                            .assignments
                            .contains_key(&(exp_name.clone(), assignment.actor.clone()))
                    {
                        return Err(ExperimentStoreError::Backend(format!(
                            "ExcludedByGroup:{group}"
                        )));
                    }
                }
            }
        }

        let variant = assignment.variant.clone();
        let key = (assignment.experiment.clone(), assignment.actor.clone());
        inner.assignments.insert(key, assignment);
        drop(inner);
        Ok(variant)
    }

    fn get_override(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<String>, ExperimentStoreError> {
        let inner = self.inner.read().unwrap();
        Ok(inner
            .overrides
            .get(&(experiment.to_owned(), actor.to_owned()))
            .cloned())
    }

    fn set_override(
        &self,
        experiment: &str,
        actor: &str,
        variant: &str,
    ) -> Result<(), ExperimentStoreError> {
        let key = (experiment.to_owned(), actor.to_owned());
        self.inner
            .write()
            .unwrap()
            .overrides
            .insert(key, variant.to_owned());
        Ok(())
    }

    fn has_assignment_in_group(
        &self,
        actor: &str,
        group: &str,
        exclude_experiment: &str,
    ) -> Result<bool, ExperimentStoreError> {
        let inner = self.inner.read().unwrap();
        for (exp_name, config) in &inner.experiments {
            if exp_name == exclude_experiment {
                continue;
            }
            if config.exclusion_group.as_deref() != Some(group) {
                continue;
            }
            if inner
                .assignments
                .contains_key(&(exp_name.clone(), actor.to_owned()))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn history(
        &self,
        experiment: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRecord>, ExperimentStoreError> {
        let records = {
            let inner = self.inner.read().unwrap();
            inner
                .changes
                .get(experiment)
                .map(|v| {
                    if limit == 0 {
                        v.clone()
                    } else {
                        v.iter().rev().take(limit).cloned().collect()
                    }
                })
                .unwrap_or_default()
        };
        Ok(records)
    }
}

// ── Hash and bucketing helpers ────────────────────────────────────────────────

fn now_secs() -> u64 {
    crate::time::clock_unix_secs(&crate::time::AmbientClock)
}

/// FNV-1a 64-bit hash of a byte slice.
///
/// Stable, dependency-free, and specified by the FNV standard.
fn fnv1a_64(data: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    let mut hash = FNV_OFFSET;
    for &byte in data {
        #[allow(clippy::cast_lossless)]
        {
            hash ^= byte as u64;
        }
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Compute the assignment bucket for `(experiment_name, actor_id)`.
///
/// Returns a value in `[0, 10 000)`. The same inputs always produce the same
/// output across restarts, replicas, and library versions.
///
/// ## Algorithm
///
/// FNV-1a 64-bit hash of the UTF-8 encoding of
/// `"<experiment_name>:<actor_id>"`, reduced modulo 10 000.
///
/// **This function MUST NOT change** between releases without a documented
/// migration path: changing it silently re-buckets every actor in every running
/// experiment, corrupting all in-flight A/B tests. If the algorithm must change,
/// bump the experiment schema version, migrate existing assignments, and update
/// the regression test below.
#[must_use]
pub fn experiment_bucket(experiment: &str, actor_id: &str) -> u64 {
    let key = format!("{experiment}:{actor_id}");
    fnv1a_64(key.as_bytes()) % 10_000
}

/// Select a variant from `variants` given a `bucket` in `[0, 10 000)`.
///
/// Returns `None` if all variant weights are zero.
fn select_variant(variants: &[VariantConfig], bucket: u64) -> Option<&str> {
    let total_weight: u64 = variants.iter().map(|v| u64::from(v.weight)).sum();
    if total_weight == 0 {
        return None;
    }
    let threshold = bucket * total_weight / 10_000;
    let mut cumulative: u64 = 0;
    for v in variants {
        cumulative += u64::from(v.weight);
        if threshold < cumulative {
            return Some(&v.name);
        }
    }
    variants.last().map(|v| v.name.as_str())
}

// ── ExperimentError ───────────────────────────────────────────────────────────

/// Error from [`ExperimentService::assign`] or other service methods.
#[derive(Debug, thiserror::Error)]
pub enum ExperimentError {
    /// No experiment with this name was found.
    #[error("experiment '{0}' not found")]
    NotFound(String),

    /// The experiment exists but is not in `Running` state.
    #[error("experiment '{0}' is not running (state: {1})")]
    NotRunning(String, ExperimentState),

    /// The experiment is archived and rejects all assignments.
    #[error("experiment '{0}' is archived")]
    Archived(String),

    /// The actor is excluded from this experiment by a mutual exclusion group.
    #[error("actor excluded from experiment '{0}' by mutual exclusion group '{1}'")]
    ExcludedByGroup(String, String),

    /// No variant could be selected (all variant weights are zero).
    #[error("experiment '{0}' has no assignable variant (all weights are zero)")]
    NoVariant(String),

    /// Store backend failure.
    #[error(transparent)]
    Store(#[from] ExperimentStoreError),
}

// ── Validation helpers ────────────────────────────────────────────────────────

fn validate_variants(variants: &[VariantConfig]) -> Result<(), ExperimentError> {
    let mut seen = std::collections::HashSet::new();
    for v in variants {
        if v.name.trim().is_empty() {
            return Err(ExperimentError::NoVariant(
                "variant name must not be empty".into(),
            ));
        }
        if !seen.insert(v.name.as_str()) {
            return Err(ExperimentError::NoVariant(format!(
                "duplicate variant name: '{}'",
                v.name
            )));
        }
    }
    Ok(())
}

// ── ExperimentService ─────────────────────────────────────────────────────────

/// The main experiment service.
///
/// Wrap an [`ExperimentStore`] (for persistence) and an optional
/// [`ExposureSink`] (default: [`TracingExposureSink`]). The service is cheaply
/// clone-able and intended to be stored as an `AppState` extension.
///
/// # Example
///
/// ```rust
/// use autumn_web::experiments::{
///     ExperimentConfig, ExperimentService, InMemoryExperimentStore, VariantConfig,
/// };
/// use std::sync::Arc;
///
/// let store = Arc::new(InMemoryExperimentStore::new());
/// let svc = ExperimentService::new(store);
///
/// svc.create(ExperimentConfig::new("onboarding_v3", vec![
///     VariantConfig::new("control", 50),
///     VariantConfig::new("wizard", 50),
/// ])).unwrap();
/// svc.start("onboarding_v3").unwrap();
///
/// let variant = svc.assign("onboarding_v3", "user:42").unwrap();
/// assert!(matches!(variant.as_str(), "control" | "wizard"));
/// ```
#[derive(Clone)]
pub struct ExperimentService {
    store: Arc<dyn ExperimentStore>,
    exposure_sink: Arc<dyn ExposureSink>,
}

impl std::fmt::Debug for ExperimentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExperimentService").finish_non_exhaustive()
    }
}

impl ExperimentService {
    /// Create a new service backed by `store`, with the default
    /// [`TracingExposureSink`].
    #[must_use]
    pub fn new(store: Arc<dyn ExperimentStore>) -> Self {
        Self {
            store,
            exposure_sink: Arc::new(TracingExposureSink),
        }
    }

    /// Override the default [`ExposureSink`].
    #[must_use]
    pub fn with_exposure_sink(mut self, sink: Arc<dyn ExposureSink>) -> Self {
        self.exposure_sink = sink;
        self
    }

    /// Assign a variant to `actor` in `experiment`.
    ///
    /// Assignment rules (evaluated in order):
    /// 1. `Archived` → `Err(Archived)`.
    /// 2. `Concluded` → `Ok(winner)` without emitting an exposure.
    /// 3. `Draft` → `Err(NotRunning)`.
    /// 4. Staff/QA override present → return override variant (exposure emitted,
    ///    tagged `is_override = true`).
    /// 5. Sticky assignment already recorded → return cached variant (exposure
    ///    emitted each call).
    /// 6. Mutual exclusion check → if `actor` has any assignment in a sibling
    ///    experiment in the same group, return `Err(ExcludedByGroup)`.
    /// 7. Compute bucket → select variant by weight → store sticky → emit exposure.
    ///
    /// # Errors
    ///
    /// - [`ExperimentError::NotFound`] — no such experiment.
    /// - [`ExperimentError::Archived`] — experiment is archived.
    /// - [`ExperimentError::NotRunning`] — experiment is in `Draft` state.
    /// - [`ExperimentError::ExcludedByGroup`] — actor excluded by mutual exclusion.
    /// - [`ExperimentError::NoVariant`] — all variant weights are zero.
    /// - [`ExperimentError::Store`] — backend failure.
    pub fn assign(&self, experiment: &str, actor: &str) -> Result<String, ExperimentError> {
        self.assign_with_request_id(experiment, actor, None)
    }

    /// Like [`assign`](Self::assign), but run on the blocking pool.
    ///
    /// Use it in async handlers. The store calls can connect to a database,
    /// and `assign` would block the Tokio worker for that time.
    ///
    /// # Errors
    ///
    /// See [`assign`](Self::assign). A panic in the store call gives
    /// [`ExperimentError::Store`].
    pub async fn assign_async(
        &self,
        experiment: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<String, ExperimentError> {
        let service = self.clone();
        let (experiment, actor) = (experiment.to_owned(), actor.to_owned());
        let request_id = request_id.map(str::to_owned);
        crate::time::spawn_blocking(move || {
            service.assign_with_request_id(&experiment, &actor, request_id.as_deref())
        })
        .await
        .unwrap_or_else(|error| {
            Err(ExperimentError::Store(ExperimentStoreError::Backend(
                format!("assignment task failed: {error}"),
            )))
        })
    }

    /// Load the store before the first read. See [`ExperimentStore::preload`].
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentStoreError`] on backend failure.
    pub fn preload(&self) -> Result<(), ExperimentStoreError> {
        self.store.preload()
    }

    /// Like [`assign`](Self::assign) but propagates a `request_id` into the
    /// exposure record.
    ///
    /// Call this from HTTP handlers where a request trace ID is available.
    ///
    /// # Errors
    ///
    /// See [`assign`](Self::assign).
    pub fn assign_with_request_id(
        &self,
        experiment: &str,
        actor: &str,
        request_id: Option<&str>,
    ) -> Result<String, ExperimentError> {
        let config = self
            .store
            .get(experiment)?
            .ok_or_else(|| ExperimentError::NotFound(experiment.to_owned()))?;

        match config.state {
            ExperimentState::Archived => {
                return Err(ExperimentError::Archived(experiment.to_owned()));
            }
            ExperimentState::Concluded => {
                // Return winner for all actors; no exposure emitted (experiment is done).
                let winner = config
                    .winner
                    .ok_or_else(|| ExperimentError::NoVariant(experiment.to_owned()))?;
                return Ok(winner);
            }
            ExperimentState::Draft => {
                return Err(ExperimentError::NotRunning(
                    experiment.to_owned(),
                    ExperimentState::Draft,
                ));
            }
            ExperimentState::Running => {}
        }

        // Check for staff/QA override (takes precedence over sticky).
        // Skip stale overrides whose variant was removed from the experiment config.
        if let Some(override_variant) = self.store.get_override(experiment, actor)?
            && config.variants.iter().any(|v| v.name == override_variant)
        {
            let sticky = Assignment::new(experiment, actor, &override_variant, true);
            if let Err(e) = self.store.record_assignment(sticky) {
                match e {
                    ExperimentStoreError::Backend(msg) if msg.starts_with("ExcludedByGroup:") => {
                        let group = msg
                            .strip_prefix("ExcludedByGroup:")
                            .unwrap_or("")
                            .trim()
                            .to_owned();
                        return Err(ExperimentError::ExcludedByGroup(
                            experiment.to_owned(),
                            group,
                        ));
                    }
                    other @ ExperimentStoreError::Backend(_) => {
                        return Err(ExperimentError::Store(other));
                    }
                }
            }
            self.emit_exposure(experiment, &override_variant, actor, request_id, true);
            return Ok(override_variant);
        }

        // Return existing sticky assignment (emit exposure each call).
        if let Some(existing) = self.store.get_assignment(experiment, actor)? {
            self.emit_exposure(
                experiment,
                &existing.variant,
                actor,
                request_id,
                existing.is_override,
            );
            return Ok(existing.variant);
        }

        // Mutual exclusion group check.
        if let Some(group) = &config.exclusion_group
            && self
                .store
                .has_assignment_in_group(actor, group, experiment)?
        {
            return Err(ExperimentError::ExcludedByGroup(
                experiment.to_owned(),
                group.clone(),
            ));
        }

        // Bucket the actor and pick a variant.
        let bucket = experiment_bucket(experiment, actor);
        let variant_name = select_variant(&config.variants, bucket)
            .ok_or_else(|| ExperimentError::NoVariant(experiment.to_owned()))?
            .to_owned();

        // Store sticky assignment. Use the persisted variant (the winner of any
        // concurrent first-write race), not the locally-computed one.
        let assignment = Assignment::new(experiment, actor, &variant_name, false);
        let persisted_variant = match self.store.record_assignment(assignment) {
            Ok(v) => v,
            Err(ExperimentStoreError::Backend(msg)) if msg.starts_with("ExcludedByGroup:") => {
                let group = msg
                    .strip_prefix("ExcludedByGroup:")
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                return Err(ExperimentError::ExcludedByGroup(
                    experiment.to_owned(),
                    group,
                ));
            }
            Err(other @ ExperimentStoreError::Backend(_)) => {
                return Err(ExperimentError::Store(other));
            }
        };
        self.emit_exposure(experiment, &persisted_variant, actor, request_id, false);

        Ok(persisted_variant)
    }

    fn emit_exposure(
        &self,
        experiment: &str,
        variant: &str,
        actor: &str,
        request_id: Option<&str>,
        is_override: bool,
    ) {
        self.exposure_sink.record(ExposureRecord {
            experiment: experiment.to_owned(),
            variant: variant.to_owned(),
            actor: actor.to_owned(),
            request_id: request_id.map(str::to_owned),
            is_override,
            timestamp_secs: now_secs(),
        });
    }

    /// Declare a new experiment (starts in `Draft` state).
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::Store`] on backend failure.
    pub fn create(&self, config: ExperimentConfig) -> Result<(), ExperimentError> {
        validate_variants(&config.variants)?;
        if config.state == ExperimentState::Concluded {
            let winner = config
                .winner
                .as_deref()
                .filter(|w| !w.trim().is_empty())
                .ok_or_else(|| {
                    ExperimentError::NoVariant(
                        "concluded experiment requires a non-empty winner".into(),
                    )
                })?;
            if !config.variants.iter().any(|v| v.name == winner) {
                return Err(ExperimentError::NoVariant(format!(
                    "'{winner}' is not a configured variant"
                )));
            }
        }
        self.store.upsert(config)?;
        Ok(())
    }

    /// Transition a `Draft` or `Concluded` experiment to `Running`.
    ///
    /// `Archived` experiments are terminal and cannot be restarted.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::NotFound`] if the experiment is unknown.
    /// Returns [`ExperimentError::Archived`] if the experiment is archived.
    pub fn start(&self, name: &str) -> Result<(), ExperimentError> {
        let config = self
            .store
            .get(name)?
            .ok_or_else(|| ExperimentError::NotFound(name.to_owned()))?;
        if config.state == ExperimentState::Archived {
            return Err(ExperimentError::Archived(name.to_owned()));
        }
        self.store.set_state(name, ExperimentState::Running, None)?;
        Ok(())
    }

    /// Conclude a running experiment, pinning `winner` as the result.
    ///
    /// After concluding, `assign()` returns `winner` for all actors without
    /// emitting new exposure events.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::NotFound`] if the experiment is unknown.
    pub fn conclude(&self, name: &str, winner: &str) -> Result<(), ExperimentError> {
        let config = self
            .store
            .get(name)?
            .ok_or_else(|| ExperimentError::NotFound(name.to_owned()))?;
        if config.state == ExperimentState::Archived {
            return Err(ExperimentError::Archived(name.to_owned()));
        }
        if !config.variants.iter().any(|v| v.name == winner) {
            return Err(ExperimentError::NoVariant(format!(
                "'{winner}' is not a configured variant of experiment '{name}'"
            )));
        }
        self.store
            .set_state(name, ExperimentState::Concluded, Some(winner))?;
        Ok(())
    }

    /// Archive an experiment.
    ///
    /// Archived experiments reject all new assignments with
    /// [`ExperimentError::Archived`].
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::NotFound`] if the experiment is unknown.
    pub fn archive(&self, name: &str) -> Result<(), ExperimentError> {
        self.store
            .get(name)?
            .ok_or_else(|| ExperimentError::NotFound(name.to_owned()))?;
        self.store
            .set_state(name, ExperimentState::Archived, None)?;
        Ok(())
    }

    /// Update the variant weights for `name`.
    ///
    /// Existing sticky assignments are **not** re-bucketed: already-assigned
    /// actors keep their variant. New actors are bucketed against the updated
    /// weights.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::NotFound`] if the experiment is unknown.
    pub fn set_weights(
        &self,
        name: &str,
        variants: Vec<VariantConfig>,
        actor: Option<&str>,
    ) -> Result<(), ExperimentError> {
        validate_variants(&variants)?;
        let config = self
            .store
            .get(name)?
            .ok_or_else(|| ExperimentError::NotFound(name.to_owned()))?;
        match config.state {
            ExperimentState::Concluded => {
                return Err(ExperimentError::NotRunning(
                    name.to_owned(),
                    ExperimentState::Concluded,
                ));
            }
            ExperimentState::Archived => {
                return Err(ExperimentError::Archived(name.to_owned()));
            }
            _ => {}
        }
        self.store.set_variants(name, variants, actor)?;
        Ok(())
    }

    /// Pin `actor` to `variant` in `experiment`, bypassing weight-based bucketing.
    ///
    /// Overrides are used by staff/QA to force a specific variant during manual
    /// testing. Exposure events include `is_override: true`.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::NotFound`] if the experiment is unknown.
    pub fn set_override(
        &self,
        experiment: &str,
        actor: &str,
        variant: &str,
    ) -> Result<(), ExperimentError> {
        let config = self
            .store
            .get(experiment)?
            .ok_or_else(|| ExperimentError::NotFound(experiment.to_owned()))?;
        if !config.variants.iter().any(|v| v.name == variant) {
            return Err(ExperimentError::NoVariant(format!(
                "'{variant}' is not a configured variant of experiment '{experiment}'"
            )));
        }
        self.store.set_override(experiment, actor, variant)?;
        Ok(())
    }

    /// List all declared experiments.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::Store`] on backend failure.
    pub fn list(&self) -> Result<Vec<ExperimentConfig>, ExperimentError> {
        Ok(self.store.list()?)
    }

    /// Return the current configuration for `name`.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::NotFound`] if the experiment is unknown.
    pub fn status(&self, name: &str) -> Result<ExperimentConfig, ExperimentError> {
        self.store
            .get(name)?
            .ok_or_else(|| ExperimentError::NotFound(name.to_owned()))
    }

    /// Return the change log for `experiment` (most-recent first), capped at `limit`.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentError::Store`] on backend failure.
    pub fn history(
        &self,
        experiment: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRecord>, ExperimentError> {
        Ok(self.store.history(experiment, limit)?)
    }
}

/// Preload the store of `service` on its own thread, and wait up to `wait`.
///
/// The app calls it when it installs the service. A failure logs a warning;
/// it does not stop the app.
pub(crate) fn preload_blocking(service: &ExperimentService, wait: std::time::Duration) {
    let service = service.clone();
    crate::feature_flags::preload_with(
        "experiment",
        "assignments fail until the first refresh",
        wait,
        move || service.preload(),
    );
}

// ── Experiments extractor ─────────────────────────────────────────────────────

/// Request extractor that resolves the current user's experiment service handle.
///
/// Extracts [`ExperimentService`] from the `AppState` extension slot. Fails
/// with `500 Internal Server Error` if no service has been registered.
///
/// Also resolves:
/// - **`actor_id`** from the session `user_id` key (Autumn's default).
/// - **`request_id`** from the `x-request-id` HTTP header (if present).
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::experiments::Experiments;
///
/// #[get("/checkout")]
/// async fn checkout(exps: Experiments) -> AutumnResult<Markup> {
///     let variant = exps.assign_async("checkout_v2").await?;
///     Ok(html! {
///         @match variant.as_str() {
///             "treatment" => (render_new_checkout()),
///             _           => (render_classic_checkout()),
///         }
///     })
/// }
/// ```
pub struct Experiments {
    service: ExperimentService,
    actor_id: Option<String>,
    request_id: Option<String>,
}

impl Experiments {
    /// Assign a variant for the current session actor.
    ///
    /// Propagates the session actor ID and `x-request-id` header automatically.
    /// For logged-out sessions, falls back to the session ID so each visitor
    /// gets a stable, per-session bucket rather than collapsing all anonymous
    /// traffic into a single `"anonymous"` actor.
    ///
    /// # Errors
    ///
    /// See [`ExperimentService::assign_with_request_id`].
    ///
    /// With a Postgres store, this can block on a database connect. In an
    /// async handler, use [`assign_async`](Self::assign_async).
    pub fn assign(&self, experiment: &str) -> Result<String, ExperimentError> {
        let actor = self.actor_id.as_deref().unwrap_or("anonymous");
        self.service
            .assign_with_request_id(experiment, actor, self.request_id.as_deref())
    }

    /// Like [`assign`](Self::assign), but run on the blocking pool.
    ///
    /// Use it in async handlers: `assign` can block the Tokio worker on a
    /// database connect.
    ///
    /// # Errors
    ///
    /// See [`ExperimentService::assign_async`].
    pub async fn assign_async(&self, experiment: &str) -> Result<String, ExperimentError> {
        let actor = self.actor_id.as_deref().unwrap_or("anonymous");
        self.service
            .assign_async(experiment, actor, self.request_id.as_deref())
            .await
    }

    /// Return the underlying service for direct access to admin operations.
    #[must_use]
    pub const fn service(&self) -> &ExperimentService {
        &self.service
    }
}

impl axum::extract::FromRequestParts<crate::AppState> for Experiments {
    type Rejection = crate::AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        let service = state
            .extension::<ExperimentService>()
            .map(|arc| (*arc).clone())
            .ok_or_else(|| {
                crate::AutumnError::internal_server_error_msg(
                    "experiment service not registered; \
                     install an ExperimentStore via AppBuilder::with_experiment_store()",
                )
            })?;

        let actor_id = if let Some(session) = parts.extensions.get::<crate::session::Session>() {
            // Use the configured auth session key (e.g. "user_id"); fall back to
            // the session ID so each anonymous visitor gets a stable, per-session
            // bucket rather than all collapsing into a single "anonymous" actor.
            let session_key = state.auth_session_key();
            if let Some(uid) = session.get(session_key).await {
                Some(uid)
            } else {
                // Use or create a stable per-session anonymous actor. We must
                // insert into the session (marking it dirty) so the framework
                // sets a cookie and the ID persists across requests; a bare
                // session.id() call does not mark the session dirty.
                const ANON_KEY: &str = "_autumn_anon_actor";
                if let Some(existing) = session.get(ANON_KEY).await {
                    Some(existing)
                } else {
                    let id = session.id().await;
                    session.insert(ANON_KEY, &id).await;
                    Some(id)
                }
            }
        } else {
            None
        };

        let request_id = parts
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        Ok(Self {
            service,
            actor_id,
            request_id,
        })
    }
}

// ── pg module ─────────────────────────────────────────────────────────────────

#[cfg(feature = "db")]
pub mod pg {
    use super::{
        Assignment, ChangeRecord, ExperimentConfig, ExperimentState, ExperimentStore,
        ExperimentStoreError, VariantConfig,
    };
    use crate::feature_flags::pg::{STATEMENT_TIMEOUT_MS, retry_delay, with_connect_timeout};
    use diesel::connection::SimpleConnection;
    use diesel::prelude::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, PoisonError, RwLock};
    use std::time::{Duration, Instant};

    // ── Row structs ───────────────────────────────────────────────────────────

    #[derive(diesel::QueryableByName)]
    struct ExperimentRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        name: String,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        description: Option<String>,
        #[diesel(sql_type = diesel::sql_types::Text)]
        state: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        variants: String,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        winner: Option<String>,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        exclusion_group: Option<String>,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        updated_at_secs: i64,
    }

    impl ExperimentRow {
        fn into_config(self) -> ExperimentConfig {
            let variants: Vec<VariantConfig> =
                serde_json::from_str(&self.variants).unwrap_or_default();
            let state = match self.state.as_str() {
                "running" => ExperimentState::Running,
                "concluded" => ExperimentState::Concluded,
                "archived" => ExperimentState::Archived,
                _ => ExperimentState::Draft,
            };
            ExperimentConfig {
                name: self.name,
                description: self.description,
                state,
                variants,
                winner: self.winner,
                exclusion_group: self.exclusion_group,
                updated_at_secs: u64::try_from(self.updated_at_secs).unwrap_or(0),
            }
        }
    }

    #[derive(diesel::QueryableByName)]
    struct AssignmentRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        experiment: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        actor: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        variant: String,
        #[diesel(sql_type = diesel::sql_types::Bool)]
        is_override: bool,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        assigned_at_secs: i64,
    }

    #[derive(diesel::QueryableByName)]
    struct BoolRow {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        result: bool,
    }

    #[derive(diesel::QueryableByName)]
    struct VariantNameRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        variant: String,
    }

    #[derive(diesel::QueryableByName)]
    struct ExclusionGroupRow {
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        exclusion_group: Option<String>,
    }

    #[derive(diesel::QueryableByName)]
    struct ChangeRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        experiment: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        mutation: String,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        actor: Option<String>,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        timestamp_secs: i64,
    }

    // ── Snapshot ──────────────────────────────────────────────────────────────

    /// All experiments, as the last refresh left them.
    #[derive(Debug, Default)]
    struct Snapshot {
        experiments: HashMap<String, ExperimentConfig>,
        /// `true` after the first full load.
        loaded: bool,
        /// Start of the last refresh attempt, good or bad.
        checked_at: Option<Instant>,
    }

    /// State that a background refresh shares with the store.
    struct Shared {
        database_url: String,
        /// `database_url` with a connect timeout.
        connect_url: String,
        cache_ttl: Duration,
        snapshot: RwLock<Snapshot>,
        /// `true` while a background refresh runs. Only one runs at a time.
        refreshing: AtomicBool,
        /// The refresh connection, kept between refreshes. Loads hold this
        /// lock, so they run and install in order.
        refresh_conn: Mutex<Option<diesel::PgConnection>>,
        refresh_errors: AtomicU64,
        /// Failed refreshes in a row. Zero after a good refresh.
        failures: AtomicU32,
        #[cfg(test)]
        connect_threads: Mutex<Vec<std::thread::ThreadId>>,
    }

    impl std::fmt::Debug for Shared {
        /// Leaves out the URL: it can hold a password.
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PgExperimentStore")
                .field("cache_ttl", &self.cache_ttl)
                .field("refresh_errors", &self.refresh_errors)
                .finish_non_exhaustive()
        }
    }

    /// Holds the single refresh slot. Drop releases it, also when a spawned
    /// refresh never runs.
    struct RefreshSlot(Arc<Shared>);

    impl Drop for RefreshSlot {
        fn drop(&mut self) {
            self.0.refreshing.store(false, Ordering::Release);
        }
    }

    impl Shared {
        fn connect(&self) -> Result<diesel::PgConnection, ExperimentStoreError> {
            #[cfg(test)]
            self.connect_threads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(std::thread::current().id());
            crate::db_url::require_postgres_target(&self.database_url, "PgExperimentStore")
                .map_err(ExperimentStoreError::Backend)?;
            // libpq quotes the target in its error, so redact it.
            let mut conn = diesel::PgConnection::establish(&self.connect_url).map_err(|e| {
                ExperimentStoreError::Backend(crate::db_url::redact_driver_error(
                    &e.to_string(),
                    &self.database_url,
                ))
            })?;
            conn.batch_execute(&format!("SET statement_timeout = {STATEMENT_TIMEOUT_MS}"))
                .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;
            Ok(conn)
        }

        /// `true` when the last refresh attempt is older than the TTL, or
        /// older than the retry delay while refreshes fail.
        fn is_stale(&self) -> bool {
            let wait = retry_delay(self.cache_ttl, self.failures.load(Ordering::Relaxed));
            let snapshot = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
            snapshot.checked_at.is_none_or(|checked| {
                checked
                    .checked_add(wait)
                    .is_some_and(|due| crate::time::system_instant() >= due)
            })
        }

        fn is_loaded(&self) -> bool {
            self.snapshot
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .loaded
        }

        fn mark_stale(&self) {
            self.snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .checked_at = None;
        }

        fn read(&self, name: &str) -> Result<Option<ExperimentConfig>, ExperimentStoreError> {
            let snapshot = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
            match snapshot.experiments.get(name) {
                Some(config) => Ok(Some(config.clone())),
                None if snapshot.loaded => Ok(None),
                None => Err(ExperimentStoreError::Backend(
                    "experiment snapshot not loaded yet; call PgExperimentStore::refresh at startup"
                        .to_owned(),
                )),
            }
        }

        /// Take the refresh slot, or `None` when a refresh already runs.
        fn begin_refresh(self: &Arc<Self>) -> Option<RefreshSlot> {
            self.refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .ok()
                .map(|_| RefreshSlot(Arc::clone(self)))
        }

        /// Load all experiments on this thread and install them. This blocks.
        fn reload(&self) -> Result<(), ExperimentStoreError> {
            self.load().map(drop)
        }

        /// Load all experiments on this thread, install them, and return
        /// them. This blocks.
        fn load(&self) -> Result<Vec<ExperimentConfig>, ExperimentStoreError> {
            let mut conn = self
                .refresh_conn
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .checked_at = Some(crate::time::system_instant());
            let fresh = || {
                let mut open = self.connect()?;
                let all = load_all(&mut open)?;
                Ok((open, all))
            };
            // A kept connection can be dead. Then retry once on a new one.
            let loaded = conn
                .take()
                .map_or_else(fresh, |mut open| {
                    load_all(&mut open)
                        .map(|all| (open, all))
                        .or_else(|_| fresh())
                })
                .map(|(open, all)| {
                    // Keep the connection only after a good load.
                    *conn = Some(open);
                    all
                });
            if let Ok(all) = &loaded {
                // Install before the lock drops, so loads install in order.
                let mut snapshot = self
                    .snapshot
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);
                snapshot.experiments = all.iter().map(|e| (e.name.clone(), e.clone())).collect();
                snapshot.loaded = true;
            }
            drop(conn);
            match loaded {
                Ok(all) => {
                    if self.failures.swap(0, Ordering::AcqRel) > 0 {
                        tracing::info!("experiment refresh recovered");
                    }
                    Ok(all)
                }
                Err(error) => {
                    self.refresh_errors.fetch_add(1, Ordering::Relaxed);
                    if self.failures.fetch_add(1, Ordering::AcqRel) == 0 {
                        tracing::warn!(
                            %error,
                            "experiment refresh failed; serving the last-known snapshot"
                        );
                    } else {
                        tracing::debug!(%error, "experiment refresh failed again");
                    }
                    Err(error)
                }
            }
        }
    }

    fn load_all(
        conn: &mut diesel::PgConnection,
    ) -> Result<Vec<ExperimentConfig>, ExperimentStoreError> {
        diesel::sql_query(
            "SELECT name, description, state::text, variants::text, winner, \
                    exclusion_group, \
                    EXTRACT(EPOCH FROM updated_at)::bigint AS updated_at_secs \
             FROM autumn_experiments ORDER BY name",
        )
        .load::<ExperimentRow>(conn)
        .map(|rows| rows.into_iter().map(ExperimentRow::into_config).collect())
        .map_err(|e| ExperimentStoreError::Backend(e.to_string()))
    }

    // ── PgExperimentStore ─────────────────────────────────────────────────────

    /// Postgres-backed [`ExperimentStore`] that serves config reads from memory.
    ///
    /// `get` reads an in-memory snapshot of all experiments. It does not
    /// connect on the calling thread when a Tokio runtime is present. When the
    /// snapshot is older than the cache TTL, `get` starts one refresh on the
    /// blocking pool and returns the last-known value without waiting. With no runtime,
    /// `get` refreshes on the calling thread.
    ///
    /// When a refresh fails, the store keeps the last-known snapshot, logs a
    /// warning and counts the error ([`refresh_errors`](Self::refresh_errors)).
    /// Before the first load, `get` returns an error.
    ///
    /// Per-actor calls (`get_assignment`, `record_assignment`, `get_override`,
    /// `has_assignment_in_group`) still connect on the calling thread. In an
    /// async handler, use
    /// [`Experiments::assign_async`](super::Experiments::assign_async).
    ///
    /// Load the snapshot at startup: `AppBuilder::with_experiment_store` calls
    /// [`ExperimentStore::preload`], and
    /// [`spawn_poll_listener`](Self::spawn_poll_listener) refreshes on each
    /// interval. Replicas see a remote write on their next refresh.
    #[derive(Debug)]
    pub struct PgExperimentStore {
        shared: Arc<Shared>,
    }

    impl Clone for PgExperimentStore {
        /// Make a new store with the same URL and TTL and an empty snapshot.
        fn clone(&self) -> Self {
            Self::with_cache_ttl(self.shared.database_url.clone(), self.shared.cache_ttl)
        }
    }

    impl PgExperimentStore {
        /// Default snapshot lifetime before `get` starts a refresh.
        pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(1);

        /// Create a store with the default 1 s snapshot TTL.
        #[must_use]
        pub fn new(database_url: impl Into<String>) -> Self {
            Self::with_cache_ttl(database_url, Self::DEFAULT_CACHE_TTL)
        }

        /// Create a store with an explicit snapshot TTL. With `Duration::ZERO`,
        /// each `get` starts a refresh.
        #[must_use]
        pub fn with_cache_ttl(database_url: impl Into<String>, cache_ttl: Duration) -> Self {
            let database_url = database_url.into();
            Self {
                shared: Arc::new(Shared {
                    connect_url: with_connect_timeout(&database_url),
                    database_url,
                    cache_ttl,
                    snapshot: RwLock::new(Snapshot::default()),
                    refreshing: AtomicBool::new(false),
                    refresh_conn: Mutex::new(None),
                    refresh_errors: AtomicU64::new(0),
                    failures: AtomicU32::new(0),
                    #[cfg(test)]
                    connect_threads: Mutex::new(Vec::new()),
                }),
            }
        }

        /// Create a store from Autumn's primary database configuration.
        ///
        /// Returns `None` when no primary URL is configured, and — since it
        /// opens a `diesel::PgConnection` and issues `jsonb` /
        /// `pg_advisory_xact_lock` SQL — when the configured target does not
        /// name Postgres.
        #[must_use]
        pub fn from_database_config(config: &crate::config::DatabaseConfig) -> Option<Self> {
            config.effective_primary_postgres_url().map(Self::new)
        }

        /// Load all experiments into the snapshot now. This blocks: call it
        /// at startup or from `spawn_blocking`.
        ///
        /// # Errors
        ///
        /// Returns [`ExperimentStoreError::Backend`] when the load fails. The
        /// last-known snapshot stays in use.
        pub fn refresh(&self) -> Result<(), ExperimentStoreError> {
            self.shared.reload()
        }

        /// Number of failed refreshes since the store was made.
        #[must_use]
        pub fn refresh_errors(&self) -> u64 {
            self.shared.refresh_errors.load(Ordering::Relaxed)
        }

        fn connect(&self) -> Result<diesel::PgConnection, ExperimentStoreError> {
            self.shared.connect()
        }

        /// Make a local write visible to the next `get`. The writer already
        /// blocks on its own connect, so it can reload here. When the reload
        /// fails, the next `get` starts a refresh.
        fn refresh_after_write(&self) {
            if self.shared.reload().is_err() {
                self.shared.mark_stale();
            }
        }

        /// Spawn a thread that refreshes the snapshot at once and then on each
        /// `poll_interval`.
        ///
        /// Replicas see remote writes on the next refresh. Call this once at
        /// startup and share the `Arc` with the app. The thread stops after
        /// all clones of the `Arc` drop.
        #[allow(
            clippy::must_use_candidate,
            reason = "the thread runs detached; callers drop the handle"
        )]
        pub fn spawn_poll_listener(
            store: Arc<Self>,
            poll_interval: Duration,
        ) -> std::thread::JoinHandle<()> {
            let shared = Arc::downgrade(&store.shared);
            drop(store);
            std::thread::spawn(move || {
                while let Some(shared) = shared.upgrade() {
                    if let Some(slot) = shared.begin_refresh() {
                        // `reload` logs and counts its errors. A panic must
                        // not stop the polls.
                        let reload = std::panic::AssertUnwindSafe(|| slot.0.reload());
                        if std::panic::catch_unwind(reload).is_err() {
                            tracing::warn!("experiment refresh panicked");
                        }
                    }
                    let wait = retry_delay(poll_interval, shared.failures.load(Ordering::Relaxed));
                    drop(shared);
                    std::thread::sleep(wait);
                }
            })
        }

        #[cfg(test)]
        fn install_for_test(&self, experiments: Vec<ExperimentConfig>) {
            let mut snapshot = self
                .shared
                .snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            snapshot.experiments = experiments
                .into_iter()
                .map(|e| (e.name.clone(), e))
                .collect();
            snapshot.loaded = true;
            snapshot.checked_at = Some(crate::time::system_instant());
        }

        #[cfg(test)]
        fn connect_threads_for_test(&self) -> Vec<std::thread::ThreadId> {
            self.shared
                .connect_threads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        #[cfg(test)]
        fn refreshing_for_test(&self) -> bool {
            self.shared.refreshing.load(Ordering::Acquire)
        }

        #[cfg(test)]
        fn begin_refresh_for_test(&self) -> Option<RefreshSlot> {
            self.shared.begin_refresh()
        }
    }

    // ── ExperimentStore impl ──────────────────────────────────────────────────

    impl ExperimentStore for PgExperimentStore {
        fn get(&self, name: &str) -> Result<Option<ExperimentConfig>, ExperimentStoreError> {
            if self.shared.is_stale()
                && let Some(slot) = self.shared.begin_refresh()
            {
                match tokio::runtime::Handle::try_current() {
                    // On a runtime: refresh on the blocking pool, never here.
                    Ok(_) => {
                        drop(crate::time::spawn_blocking(move || {
                            // `reload` logs and counts its errors.
                            let _ = slot.0.reload();
                        }));
                    }
                    // No runtime: this thread can block. Before the first
                    // load, report why it failed.
                    Err(_) => {
                        if let Err(error) = slot.0.reload()
                            && !self.shared.is_loaded()
                        {
                            return Err(error);
                        }
                    }
                }
            }
            self.shared.read(name)
        }

        fn list(&self) -> Result<Vec<ExperimentConfig>, ExperimentStoreError> {
            self.shared.load()
        }

        fn preload(&self) -> Result<(), ExperimentStoreError> {
            self.refresh()
        }

        fn upsert(&self, config: ExperimentConfig) -> Result<(), ExperimentStoreError> {
            let mut conn = self.connect()?;

            let active_variants = diesel::sql_query(
                "SELECT DISTINCT variant FROM autumn_experiment_assignments WHERE experiment = $1",
            )
            .bind::<diesel::sql_types::Text, _>(&config.name)
            .load::<VariantNameRow>(&mut conn)
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;

            let new_variants: std::collections::HashSet<&str> =
                config.variants.iter().map(|v| v.name.as_str()).collect();

            for row in active_variants {
                if !new_variants.contains(row.variant.as_str()) {
                    return Err(ExperimentStoreError::Backend(format!(
                        "cannot delete variant '{}' because it has active assignments",
                        row.variant
                    )));
                }
            }

            let variants_json =
                serde_json::to_string(&config.variants).unwrap_or_else(|_| "[]".to_owned());
            let state_str = config.state.to_string();
            let rows_affected = diesel::sql_query(
                "WITH upserted AS ( \
                     INSERT INTO autumn_experiments \
                         (name, description, state, variants, winner, exclusion_group) \
                     VALUES ($1, $2, $3::autumn_experiment_state, $4::jsonb, $5, $6) \
                     ON CONFLICT (name) DO UPDATE SET \
                         description = EXCLUDED.description, \
                         state = EXCLUDED.state, \
                         variants = EXCLUDED.variants, \
                         winner = EXCLUDED.winner, \
                         exclusion_group = EXCLUDED.exclusion_group, \
                         updated_at = NOW() \
                     WHERE NOT EXISTS ( \
                         SELECT 1 FROM autumn_experiment_assignments a \
                         WHERE a.experiment = EXCLUDED.name \
                           AND a.variant NOT IN ( \
                               SELECT x.name FROM jsonb_to_recordset(EXCLUDED.variants) AS x(name text) \
                           ) \
                     ) \
                     RETURNING name, (xmax = 0) AS is_insert \
                 ) \
                 INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
                 SELECT name, CASE WHEN is_insert THEN 'created' ELSE 'updated' END, NULL FROM upserted",
            )
            .bind::<diesel::sql_types::Text, _>(&config.name)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(config.description)
            .bind::<diesel::sql_types::Text, _>(&state_str)
            .bind::<diesel::sql_types::Text, _>(&variants_json)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(config.winner)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(config.exclusion_group)
            .execute(&mut conn)
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;

            if rows_affected == 0 {
                return Err(ExperimentStoreError::Backend(
                    "cannot delete variant because it has active assignments".to_owned(),
                ));
            }

            self.refresh_after_write();
            Ok(())
        }

        fn set_state(
            &self,
            name: &str,
            state: ExperimentState,
            winner: Option<&str>,
        ) -> Result<(), ExperimentStoreError> {
            let state_str = state.to_string();
            let mutation =
                winner.map_or_else(|| format!("state={state}"), |w| format!("concluded={w}"));
            let mut conn = self.connect()?;
            diesel::sql_query(
                "WITH updated AS ( \
                     UPDATE autumn_experiments \
                     SET state = $2::autumn_experiment_state, \
                         winner = COALESCE($3, winner), \
                         updated_at = NOW() \
                     WHERE name = $1 \
                     RETURNING name \
                 ) \
                 INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
                 SELECT name, $4, NULL FROM updated",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::Text, _>(&state_str)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                winner.map(str::to_owned),
            )
            .bind::<diesel::sql_types::Text, _>(&mutation)
            .execute(&mut conn)
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;
            self.refresh_after_write();
            Ok(())
        }

        fn set_variants(
            &self,
            name: &str,
            variants: Vec<VariantConfig>,
            actor: Option<&str>,
        ) -> Result<(), ExperimentStoreError> {
            let mut conn = self.connect()?;

            let active_variants = diesel::sql_query(
                "SELECT DISTINCT variant FROM autumn_experiment_assignments WHERE experiment = $1",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .load::<VariantNameRow>(&mut conn)
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;

            let new_variants: std::collections::HashSet<&str> =
                variants.iter().map(|v| v.name.as_str()).collect();

            for row in active_variants {
                if !new_variants.contains(row.variant.as_str()) {
                    return Err(ExperimentStoreError::Backend(format!(
                        "cannot delete variant '{}' because it has active assignments",
                        row.variant
                    )));
                }
            }

            let variants_json =
                serde_json::to_string(&variants).unwrap_or_else(|_| "[]".to_owned());
            let rows_affected = diesel::sql_query(
                "WITH updated AS ( \
                     UPDATE autumn_experiments \
                     SET variants = $2::jsonb, updated_at = NOW() \
                     WHERE name = $1 \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM autumn_experiment_assignments a \
                           WHERE a.experiment = name \
                             AND a.variant NOT IN ( \
                                 SELECT x.name FROM jsonb_to_recordset($2::jsonb) AS x(name text) \
                             ) \
                       ) \
                     RETURNING name \
                 ) \
                 INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
                 SELECT name, 'set_weights', $3 FROM updated",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::Text, _>(&variants_json)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                actor.map(str::to_owned),
            )
            .execute(&mut conn)
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;

            if rows_affected == 0 {
                let exists_row = diesel::sql_query(
                    "SELECT EXISTS(SELECT 1 FROM autumn_experiments WHERE name = $1) AS result",
                )
                .bind::<diesel::sql_types::Text, _>(name)
                .get_result::<BoolRow>(&mut conn)
                .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;

                if exists_row.result {
                    return Err(ExperimentStoreError::Backend(
                        "cannot delete variant because it has active assignments".to_owned(),
                    ));
                }
            }

            self.refresh_after_write();
            Ok(())
        }

        fn get_assignment(
            &self,
            experiment: &str,
            actor: &str,
        ) -> Result<Option<Assignment>, ExperimentStoreError> {
            let mut conn = self.connect()?;
            diesel::sql_query(
                "SELECT experiment, actor, variant, is_override, \
                        EXTRACT(EPOCH FROM assigned_at)::bigint AS assigned_at_secs \
                 FROM autumn_experiment_assignments \
                 WHERE experiment = $1 AND actor = $2",
            )
            .bind::<diesel::sql_types::Text, _>(experiment)
            .bind::<diesel::sql_types::Text, _>(actor)
            .get_result::<AssignmentRow>(&mut conn)
            .optional()
            .map(|r| {
                r.map(|row| Assignment {
                    experiment: row.experiment,
                    actor: row.actor,
                    variant: row.variant,
                    is_override: row.is_override,
                    assigned_at_secs: u64::try_from(row.assigned_at_secs).unwrap_or(0),
                })
            })
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))
        }

        fn record_assignment(
            &self,
            assignment: Assignment,
        ) -> Result<String, ExperimentStoreError> {
            use diesel::connection::Connection as _;

            #[derive(Debug)]
            enum TxError {
                Database(diesel::result::Error),
                Excluded(String),
            }
            impl From<diesel::result::Error> for TxError {
                fn from(e: diesel::result::Error) -> Self {
                    Self::Database(e)
                }
            }

            let mut conn = self.connect()?;
            let result = conn.transaction::<String, TxError, _>(|conn| {
                // 1. Acquire advisory lock on actor to serialize concurrent assignments for this actor.
                diesel::sql_query("SELECT pg_advisory_xact_lock(hashtext($1))")
                    .bind::<diesel::sql_types::Text, _>(&assignment.actor)
                    .execute(conn)?;

                // 2. Check exclusion group if this is NOT an override.
                if !assignment.is_override {
                    // Fetch the exclusion group for this experiment.
                    let group_row = diesel::sql_query(
                        "SELECT exclusion_group FROM autumn_experiments WHERE name = $1"
                    )
                    .bind::<diesel::sql_types::Text, _>(&assignment.experiment)
                    .get_result::<ExclusionGroupRow>(conn)
                    .optional()?;

                    if let Some(ExclusionGroupRow { exclusion_group: Some(group) }) = group_row {
                        // Check if there are other assignments in the same group.
                        let exists = diesel::sql_query(
                            "SELECT EXISTS ( \
                                 SELECT 1 \
                                 FROM autumn_experiment_assignments a \
                                 JOIN autumn_experiments e ON e.name = a.experiment \
                                 WHERE a.actor = $1 \
                                   AND e.exclusion_group = $2 \
                                   AND a.experiment <> $3 \
                             ) AS result",
                        )
                        .bind::<diesel::sql_types::Text, _>(&assignment.actor)
                        .bind::<diesel::sql_types::Text, _>(&group)
                        .bind::<diesel::sql_types::Text, _>(&assignment.experiment)
                        .get_result::<BoolRow>(conn)
                        .map(|r| r.result)?;

                        if exists {
                            return Err(TxError::Excluded(group));
                        }
                    }
                }

                // 3. Insert or update on conflict.
                let variant_name = diesel::sql_query(
                    "INSERT INTO autumn_experiment_assignments \
                         (experiment, actor, variant, is_override) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (experiment, actor) DO UPDATE \
                         SET variant = CASE WHEN EXCLUDED.is_override THEN EXCLUDED.variant ELSE autumn_experiment_assignments.variant END, \
                             is_override = CASE WHEN EXCLUDED.is_override THEN EXCLUDED.is_override ELSE autumn_experiment_assignments.is_override END \
                     RETURNING variant",
                )
                .bind::<diesel::sql_types::Text, _>(&assignment.experiment)
                .bind::<diesel::sql_types::Text, _>(&assignment.actor)
                .bind::<diesel::sql_types::Text, _>(&assignment.variant)
                .bind::<diesel::sql_types::Bool, _>(assignment.is_override)
                .get_result::<VariantNameRow>(conn)
                .map(|r| r.variant)?;

                Ok(variant_name)
            });

            match result {
                Ok(v) => Ok(v),
                Err(TxError::Database(e)) => Err(ExperimentStoreError::Backend(e.to_string())),
                Err(TxError::Excluded(group)) => Err(ExperimentStoreError::Backend(format!(
                    "ExcludedByGroup:{group}"
                ))),
            }
        }

        fn get_override(
            &self,
            experiment: &str,
            actor: &str,
        ) -> Result<Option<String>, ExperimentStoreError> {
            let mut conn = self.connect()?;
            diesel::sql_query(
                "SELECT variant FROM autumn_experiment_overrides \
                 WHERE experiment = $1 AND actor = $2",
            )
            .bind::<diesel::sql_types::Text, _>(experiment)
            .bind::<diesel::sql_types::Text, _>(actor)
            .get_result::<VariantNameRow>(&mut conn)
            .optional()
            .map(|r| r.map(|row| row.variant))
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))
        }

        fn set_override(
            &self,
            experiment: &str,
            actor: &str,
            variant: &str,
        ) -> Result<(), ExperimentStoreError> {
            let mut conn = self.connect()?;
            diesel::sql_query(
                "WITH upserted AS ( \
                     INSERT INTO autumn_experiment_overrides (experiment, actor, variant) \
                     VALUES ($1, $2, $3) \
                     ON CONFLICT (experiment, actor) DO UPDATE SET variant = EXCLUDED.variant \
                     RETURNING experiment, actor, variant \
                 ) \
                 INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
                 SELECT experiment, 'override=' || actor || ':' || variant, NULL FROM upserted",
            )
            .bind::<diesel::sql_types::Text, _>(experiment)
            .bind::<diesel::sql_types::Text, _>(actor)
            .bind::<diesel::sql_types::Text, _>(variant)
            .execute(&mut conn)
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))?;
            Ok(())
        }

        fn has_assignment_in_group(
            &self,
            actor: &str,
            group: &str,
            exclude_experiment: &str,
        ) -> Result<bool, ExperimentStoreError> {
            let mut conn = self.connect()?;
            diesel::sql_query(
                "SELECT EXISTS ( \
                     SELECT 1 \
                     FROM autumn_experiment_assignments a \
                     JOIN autumn_experiments e ON e.name = a.experiment \
                     WHERE a.actor = $1 \
                       AND e.exclusion_group = $2 \
                       AND a.experiment <> $3 \
                 ) AS result",
            )
            .bind::<diesel::sql_types::Text, _>(actor)
            .bind::<diesel::sql_types::Text, _>(group)
            .bind::<diesel::sql_types::Text, _>(exclude_experiment)
            .get_result::<BoolRow>(&mut conn)
            .map(|r| r.result)
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))
        }

        fn history(
            &self,
            experiment: &str,
            limit: usize,
        ) -> Result<Vec<ChangeRecord>, ExperimentStoreError> {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut conn = self.connect()?;
            diesel::sql_query(
                "SELECT experiment, mutation, actor, \
                        EXTRACT(EPOCH FROM changed_at)::bigint AS timestamp_secs \
                 FROM autumn_experiment_changes \
                 WHERE experiment = $1 \
                 ORDER BY changed_at DESC \
                 LIMIT NULLIF($2::bigint, 0)",
            )
            .bind::<diesel::sql_types::Text, _>(experiment)
            .bind::<diesel::sql_types::BigInt, _>(limit)
            .load::<ChangeRow>(&mut conn)
            .map(|rows| {
                rows.into_iter()
                    .map(|r| ChangeRecord {
                        experiment: r.experiment,
                        mutation: r.mutation,
                        actor: r.actor,
                        timestamp_secs: u64::try_from(r.timestamp_secs).unwrap_or(0),
                    })
                    .collect()
            })
            .map_err(|e| ExperimentStoreError::Backend(e.to_string()))
        }
    }

    #[cfg(test)]
    mod pg_tests {
        use super::*;

        /// Nothing listens on port 1, so a connect fails at once.
        const DEAD_URL: &str = "postgres://autumn@127.0.0.1:1/autumn?connect_timeout=2";

        fn running(name: &str) -> ExperimentConfig {
            let mut config = ExperimentConfig::new(name, vec![VariantConfig::new("control", 100)]);
            config.state = ExperimentState::Running;
            // `new` stamps the wall clock. Pin it so equal configs compare equal.
            config.updated_at_secs = 0;
            config
        }

        fn current_thread_runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        /// Wait (on the runtime) until no background refresh runs.
        async fn settle(store: &PgExperimentStore) {
            for _ in 0..1000 {
                if !store.refreshing_for_test() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("background refresh did not finish");
        }

        #[test]
        fn get_serves_last_known_value_when_refresh_fails() {
            let store = PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::ZERO);
            store.install_for_test(vec![running("exp")]);

            // No runtime here, so `get` refreshes on this thread. It fails.
            assert_eq!(store.get("exp").unwrap(), Some(running("exp")));
            assert_eq!(store.get("absent").unwrap(), None);
            assert_eq!(store.refresh_errors(), 2);
        }

        #[test]
        fn get_within_ttl_does_not_connect() {
            let store = PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::from_secs(60));
            store.install_for_test(vec![running("exp")]);
            assert_eq!(store.get("exp").unwrap(), Some(running("exp")));
            assert!(store.connect_threads_for_test().is_empty());
        }

        #[test]
        fn get_on_runtime_never_connects_on_the_worker() {
            let store =
                std::sync::Arc::new(PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::ZERO));
            store.install_for_test(vec![running("exp")]);

            current_thread_runtime().block_on(async {
                let worker = std::thread::current().id();
                assert_eq!(store.get("exp").unwrap(), Some(running("exp")));
                settle(&store).await;

                let threads = store.connect_threads_for_test();
                assert!(!threads.is_empty(), "the stale snapshot is refreshed");
                assert!(!threads.contains(&worker), "no connect on the Tokio worker");
            });
            assert!(store.refresh_errors() >= 1);
        }

        #[test]
        fn cold_get_on_runtime_errors_and_loads_in_background() {
            let store = PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::ZERO);
            current_thread_runtime().block_on(async {
                let worker = std::thread::current().id();
                assert!(store.get("exp").is_err(), "no snapshot yet");
                settle(&store).await;
                let threads = store.connect_threads_for_test();
                assert!(!threads.is_empty());
                assert!(!threads.contains(&worker));
            });
        }

        #[test]
        fn only_one_refresh_runs_at_a_time() {
            let store = PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::ZERO);
            let slot = store.begin_refresh_for_test();
            assert!(slot.is_some());
            assert!(store.begin_refresh_for_test().is_none());
            drop(slot);
            assert!(store.begin_refresh_for_test().is_some());
        }

        #[test]
        fn failed_refresh_after_write_marks_the_snapshot_stale() {
            let store = PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::from_secs(60));
            store.install_for_test(vec![running("exp")]);
            assert!(!store.shared.is_stale());
            store.refresh_after_write();
            assert!(store.shared.is_stale());
        }

        #[test]
        fn cold_get_without_runtime_reports_the_refresh_error() {
            let store = PgExperimentStore::new(
                "postgres://autumn:hunter2@127.0.0.1:1/autumn?connect_timeout=2",
            );
            let error = store.get("exp").unwrap_err().to_string();
            assert!(!error.contains("not loaded"), "{error}");
            assert!(!error.contains("hunter2"), "{error}");
        }

        #[test]
        fn cold_get_while_a_refresh_runs_does_not_connect() {
            let store = PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::ZERO);
            let _slot = store.begin_refresh_for_test();
            let error = store.get("exp").unwrap_err().to_string();
            assert!(error.contains("not loaded"), "{error}");
            assert!(store.connect_threads_for_test().is_empty());
        }

        #[test]
        fn refresh_reports_backend_error() {
            let store = PgExperimentStore::new(DEAD_URL);
            assert!(store.refresh().is_err());
            assert_eq!(store.refresh_errors(), 1);
            assert!(store.preload().is_err());
        }

        #[test]
        fn refresh_error_hides_the_password() {
            let store = PgExperimentStore::new(
                "postgres://autumn:hunter2@127.0.0.1:1/autumn?connect_timeout=2",
            );
            let error = store.refresh().unwrap_err().to_string();
            assert!(!error.contains("hunter2"), "{error}");
        }

        #[test]
        fn debug_output_hides_the_password() {
            let store = PgExperimentStore::new(
                "postgres://autumn:hunter2@127.0.0.1:1/autumn?connect_timeout=2",
            );
            let shown = format!("{store:?}");
            assert!(shown.contains("PgExperimentStore"), "{shown}");
            assert!(!shown.contains("hunter2"), "{shown}");
        }

        #[test]
        fn clone_keeps_the_ttl_and_starts_with_an_empty_snapshot() {
            let store = PgExperimentStore::with_cache_ttl(DEAD_URL, Duration::from_secs(60));
            store.install_for_test(vec![running("exp")]);
            let copy = store.clone();
            assert_eq!(copy.shared.cache_ttl, Duration::from_secs(60));
            assert!(!copy.shared.is_loaded());
            assert!(store.shared.is_loaded(), "the original keeps its snapshot");
        }

        #[test]
        fn poll_listener_refreshes_until_the_store_drops() {
            let store = std::sync::Arc::new(PgExperimentStore::new(DEAD_URL));
            let handle = PgExperimentStore::spawn_poll_listener(
                std::sync::Arc::clone(&store),
                Duration::from_millis(10),
            );
            for _ in 0..1000 {
                if store.refresh_errors() >= 1 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(store.refresh_errors() >= 1, "the listener refreshed");
            drop(store);
            for _ in 0..3000 {
                if handle.is_finished() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("poll listener outlived its store");
        }

        #[test]
        fn poll_listener_stops_when_the_store_drops() {
            let store = std::sync::Arc::new(PgExperimentStore::new(DEAD_URL));
            let handle = PgExperimentStore::spawn_poll_listener(
                std::sync::Arc::clone(&store),
                Duration::from_millis(10),
            );
            drop(store);
            for _ in 0..1000 {
                if handle.is_finished() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("poll listener outlived its store");
        }
    }
}

// ── Test probe ────────────────────────────────────────────────────────────────

/// In-memory store that counts `preload` calls and records the thread of each
/// per-actor call.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ProbeStore {
    inner: InMemoryExperimentStore,
    pub(crate) preloads: std::sync::atomic::AtomicUsize,
    pub(crate) threads: Mutex<Vec<std::thread::ThreadId>>,
    /// Make `get_assignment` panic.
    panic_on_assignment: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl ProbeStore {
    fn seen(&self) {
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
}

#[cfg(test)]
impl ExperimentStore for ProbeStore {
    fn get(&self, name: &str) -> Result<Option<ExperimentConfig>, ExperimentStoreError> {
        self.inner.get(name)
    }
    fn list(&self) -> Result<Vec<ExperimentConfig>, ExperimentStoreError> {
        self.inner.list()
    }
    fn upsert(&self, config: ExperimentConfig) -> Result<(), ExperimentStoreError> {
        self.inner.upsert(config)
    }
    fn set_state(
        &self,
        name: &str,
        state: ExperimentState,
        winner: Option<&str>,
    ) -> Result<(), ExperimentStoreError> {
        self.inner.set_state(name, state, winner)
    }
    fn set_variants(
        &self,
        name: &str,
        variants: Vec<VariantConfig>,
        actor: Option<&str>,
    ) -> Result<(), ExperimentStoreError> {
        self.inner.set_variants(name, variants, actor)
    }
    fn get_assignment(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<Assignment>, ExperimentStoreError> {
        self.seen();
        assert!(
            !self
                .panic_on_assignment
                .load(std::sync::atomic::Ordering::SeqCst),
            "store call panicked"
        );
        self.inner.get_assignment(experiment, actor)
    }
    fn record_assignment(&self, assignment: Assignment) -> Result<String, ExperimentStoreError> {
        self.seen();
        self.inner.record_assignment(assignment)
    }
    fn get_override(
        &self,
        experiment: &str,
        actor: &str,
    ) -> Result<Option<String>, ExperimentStoreError> {
        self.seen();
        self.inner.get_override(experiment, actor)
    }
    fn set_override(
        &self,
        experiment: &str,
        actor: &str,
        variant: &str,
    ) -> Result<(), ExperimentStoreError> {
        self.inner.set_override(experiment, actor, variant)
    }
    fn has_assignment_in_group(
        &self,
        actor: &str,
        group: &str,
        exclude_experiment: &str,
    ) -> Result<bool, ExperimentStoreError> {
        self.seen();
        self.inner
            .has_assignment_in_group(actor, group, exclude_experiment)
    }
    fn history(
        &self,
        experiment: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRecord>, ExperimentStoreError> {
        self.inner.history(experiment, limit)
    }
    fn preload(&self) -> Result<(), ExperimentStoreError> {
        self.preloads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_svc() -> ExperimentService {
        ExperimentService::new(Arc::new(InMemoryExperimentStore::new()))
    }

    fn running_config(name: &str) -> ExperimentConfig {
        let mut config = ExperimentConfig::new(
            name,
            vec![
                VariantConfig::new("control", 50),
                VariantConfig::new("b", 50),
            ],
        );
        config.state = ExperimentState::Running;
        config
    }

    #[tokio::test(flavor = "current_thread")]
    async fn assign_async_runs_store_calls_off_the_worker() {
        let store = Arc::new(ProbeStore::default());
        store.upsert(running_config("exp")).unwrap();
        let svc = ExperimentService::new(store.clone());
        let worker = std::thread::current().id();

        let first = svc.assign_async("exp", "user:1", None).await.unwrap();
        let again = svc.assign_async("exp", "user:1", None).await.unwrap();

        assert_eq!(first, again, "assignment stays sticky");
        let threads = store.threads.lock().unwrap().clone();
        assert!(!threads.is_empty(), "the store was called");
        assert!(!threads.contains(&worker), "no store call on the worker");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn experiments_assign_async_uses_actor_and_runs_off_the_worker() {
        let store = Arc::new(ProbeStore::default());
        store.upsert(running_config("exp")).unwrap();
        let exps = Experiments {
            service: ExperimentService::new(store.clone()),
            actor_id: Some("user:7".to_owned()),
            request_id: None,
        };
        let worker = std::thread::current().id();

        let variant = exps.assign_async("exp").await.unwrap();

        let threads = store.threads.lock().unwrap().clone();
        assert!(!threads.contains(&worker));
        let sticky = store.get_assignment("exp", "user:7").unwrap().unwrap();
        assert_eq!(sticky.variant, variant);
    }

    #[tokio::test]
    async fn assign_async_maps_a_store_panic_to_a_store_error() {
        let store = Arc::new(ProbeStore::default());
        store.upsert(running_config("exp")).unwrap();
        store
            .panic_on_assignment
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let svc = ExperimentService::new(store);
        let result = svc.assign_async("exp", "user:1", None).await;
        assert!(
            matches!(
                &result,
                Err(ExperimentError::Store(ExperimentStoreError::Backend(m)))
                    if m.contains("assignment task failed")
            ),
            "{result:?}"
        );
    }

    #[test]
    fn preload_with_returns_after_the_wait_when_the_load_is_slow() {
        let started = std::time::Instant::now();
        crate::feature_flags::preload_with(
            "test",
            "none",
            std::time::Duration::from_millis(10),
            || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                Ok::<(), String>(())
            },
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
    }

    #[test]
    fn preload_with_returns_when_the_load_panics_or_fails() {
        let wait = std::time::Duration::from_secs(5);
        crate::feature_flags::preload_with("test", "none", wait, || -> Result<(), String> {
            panic!("load panicked")
        });
        crate::feature_flags::preload_with("test", "none", wait, || Err("failed".to_owned()));
    }

    #[test]
    fn probe_store_delegates_to_the_in_memory_store() {
        let store = ProbeStore::default();
        store.upsert(running_config("exp")).unwrap();
        store
            .set_state("exp", ExperimentState::Concluded, Some("control"))
            .unwrap();
        store
            .set_variants("exp", vec![VariantConfig::new("control", 100)], None)
            .unwrap();
        store.set_override("exp", "user:1", "control").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store.get("exp").unwrap().unwrap().winner.as_deref(),
            Some("control")
        );
        assert!(!store.history("exp", 10).unwrap().is_empty());
    }

    #[test]
    fn store_preload_defaults_to_ok() {
        assert!(InMemoryExperimentStore::new().preload().is_ok());
        let probe = Arc::new(ProbeStore::default());
        probe.preload().unwrap();
        assert_eq!(probe.preloads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn make_svc_with_sink() -> (ExperimentService, Arc<Mutex<Vec<ExposureRecord>>>) {
        let (sink, records) = RecordingExposureSink::new();
        let svc = ExperimentService::new(Arc::new(InMemoryExperimentStore::new()))
            .with_exposure_sink(Arc::new(sink));
        (svc, records)
    }

    fn fifty_fifty(name: &str) -> ExperimentConfig {
        ExperimentConfig::new(
            name,
            vec![
                VariantConfig::new("control", 50),
                VariantConfig::new("treatment", 50),
            ],
        )
    }

    fn running(svc: &ExperimentService, name: &str) {
        svc.create(fifty_fifty(name)).unwrap();
        svc.start(name).unwrap();
    }

    // ═══════════════════════════════════ RED PHASE ═══════════════════════════
    // These tests were written before the full implementation existed and drove
    // the design of ExperimentService, ExperimentStore, and ExposureSink.
    // ═════════════════════════════════════════════════════════════════════════

    // ── AC: assign() returns unknown-experiment error ─────────────────────────

    #[test]
    fn assign_unknown_experiment_returns_not_found() {
        let svc = make_svc();
        let err = svc.assign("ghost", "user:1").unwrap_err();
        assert!(
            matches!(err, ExperimentError::NotFound(_)),
            "expected NotFound, got {err}"
        );
    }

    // ── AC: lifecycle — draft rejects assignments ─────────────────────────────

    #[test]
    fn assign_draft_experiment_returns_not_running() {
        let svc = make_svc();
        svc.create(fifty_fifty("exp")).unwrap();
        let err = svc.assign("exp", "user:1").unwrap_err();
        assert!(
            matches!(err, ExperimentError::NotRunning(_, ExperimentState::Draft)),
            "expected NotRunning(Draft), got {err}"
        );
    }

    // ── AC: lifecycle — archived rejects assignments ──────────────────────────

    #[test]
    fn assign_archived_experiment_returns_archived() {
        let svc = make_svc();
        running(&svc, "exp");
        svc.archive("exp").unwrap();
        let err = svc.assign("exp", "user:1").unwrap_err();
        assert!(
            matches!(err, ExperimentError::Archived(_)),
            "expected Archived, got {err}"
        );
    }

    // ── AC: lifecycle — concluded returns winner for all actors ───────────────

    #[test]
    fn concluded_experiment_returns_winner_for_all_actors() {
        let svc = make_svc();
        running(&svc, "exp");
        svc.conclude("exp", "treatment").unwrap();
        // Every actor sees the winner — regardless of their bucket.
        for i in 0..100_u32 {
            let actor = format!("user:{i}");
            let v = svc.assign("exp", &actor).unwrap();
            assert_eq!(
                v, "treatment",
                "concluded experiment must return winner for {actor}"
            );
        }
    }

    // ── AC: concluded experiment emits no new exposures ───────────────────────

    #[test]
    fn concluded_experiment_emits_no_exposures() {
        let (svc, records) = make_svc_with_sink();
        let store = Arc::new(InMemoryExperimentStore::new());
        let (sink2, records2) = RecordingExposureSink::new();
        let svc2 = ExperimentService::new(store as Arc<dyn ExperimentStore>)
            .with_exposure_sink(Arc::new(sink2));
        svc2.create(fifty_fifty("exp")).unwrap();
        svc2.start("exp").unwrap();
        svc2.conclude("exp", "treatment").unwrap();
        svc2.assign("exp", "user:1").unwrap();
        assert_eq!(
            records2.lock().unwrap().len(),
            0,
            "concluded experiment must not emit exposure events"
        );
        let _ = records; // silence unused-variable warning for the other sink
        let _ = svc;
    }

    // ── AC: assign() returns deterministic variant for running experiment ─────

    #[test]
    fn assign_running_experiment_returns_valid_variant() {
        let svc = make_svc();
        running(&svc, "exp");
        let v = svc.assign("exp", "user:1").unwrap();
        assert!(
            v == "control" || v == "treatment",
            "variant must be one of the declared names, got {v:?}"
        );
    }

    // ── AC: deterministic bucketing — same actor always gets same variant ─────

    #[test]
    fn assign_is_deterministic_for_same_actor() {
        let svc = make_svc();
        running(&svc, "exp");
        let v1 = svc.assign("exp", "user:42").unwrap();
        let v2 = svc.assign("exp", "exp").unwrap(); // different key — just to exercise more
        // Re-create without sticky to verify pure hash determinism.
        let svc2 = make_svc();
        running(&svc2, "exp");
        let v3 = svc2.assign("exp", "user:42").unwrap();
        assert_eq!(
            v1, v3,
            "same actor must receive the same variant across different service instances"
        );
        let _ = v2;
    }

    // ── AC: 10 000 stable requests — zero reassignments ───────────────────────

    #[test]
    fn zero_reassignments_across_10000_requests() {
        let svc = make_svc();
        running(&svc, "exp");
        let first = svc.assign("exp", "stable_user").unwrap();
        for _ in 1..10_000 {
            let v = svc.assign("exp", "stable_user").unwrap();
            assert_eq!(v, first, "re-assignment must return the same variant");
        }
    }

    // ── AC: stable hash regression — known fixture inputs ────────────────────

    #[test]
    fn stable_hash_regression_known_fixtures() {
        // Pre-computed: FNV-1a 64-bit of "<experiment>:<actor>" mod 10 000.
        // MUST NOT change without a documented migration path for existing assignments.
        // To regenerate: run `experiment_bucket(name, actor)` and record the output.
        let b1 = experiment_bucket("checkout_v2", "user:1");
        let b2 = experiment_bucket("checkout_v2", "user:1");
        assert_eq!(
            b1, b2,
            "hash must be deterministic (same input → same output)"
        );

        // Fixture values established on first run — sentinel against algorithm drift.
        assert_eq!(
            b1, 4_830,
            "checkout_v2:user:1 bucket changed — hash regression"
        );
        assert_eq!(
            experiment_bucket("checkout_v2", "user:2"),
            6_619,
            "checkout_v2:user:2 bucket changed — hash regression"
        );
        assert_eq!(
            experiment_bucket("onboarding_v3", "user:100"),
            6_602,
            "onboarding_v3:user:100 bucket changed — hash regression"
        );
    }

    // ── AC: weights — 0% weight variant is never assigned ────────────────────

    #[test]
    fn zero_weight_variant_never_assigned() {
        let svc = make_svc();
        svc.create(ExperimentConfig::new(
            "exp",
            vec![
                VariantConfig::new("control", 100),
                VariantConfig::new("dead", 0),
            ],
        ))
        .unwrap();
        svc.start("exp").unwrap();
        for i in 0..200_u32 {
            let v = svc.assign("exp", &format!("user:{i}")).unwrap();
            assert_eq!(v, "control", "zero-weight variant must never be assigned");
        }
    }

    // ── AC: weights — single variant at 100% is always assigned ──────────────

    #[test]
    fn single_variant_always_assigned() {
        let svc = make_svc();
        svc.create(ExperimentConfig::new(
            "exp",
            vec![VariantConfig::new("only", 100)],
        ))
        .unwrap();
        svc.start("exp").unwrap();
        for i in 0..100_u32 {
            let v = svc.assign("exp", &format!("user:{i}")).unwrap();
            assert_eq!(v, "only");
        }
    }

    // ── AC: weights — roughly 50/50 split ────────────────────────────────────

    #[test]
    fn fifty_fifty_weights_split_roughly_evenly() {
        let svc = make_svc();
        running(&svc, "exp");
        let mut control_count = 0_u32;
        for i in 0..1000_u32 {
            if svc.assign("exp", &format!("user:{i}")).unwrap() == "control" {
                control_count += 1;
            }
        }
        assert!(
            (400..=600).contains(&control_count),
            "expected ~500 control assignments, got {control_count}"
        );
    }

    // ── AC: all-zero-weight returns NoVariant error ───────────────────────────

    #[test]
    fn all_zero_weights_returns_no_variant_error() {
        let svc = make_svc();
        svc.create(ExperimentConfig::new(
            "exp",
            vec![VariantConfig::new("a", 0), VariantConfig::new("b", 0)],
        ))
        .unwrap();
        svc.start("exp").unwrap();
        let err = svc.assign("exp", "user:1").unwrap_err();
        assert!(
            matches!(err, ExperimentError::NoVariant(_)),
            "expected NoVariant, got {err}"
        );
    }

    // ── AC: sticky assignment — subsequent calls return same variant ──────────

    #[test]
    fn sticky_assignment_returned_on_subsequent_calls() {
        let svc = make_svc();
        running(&svc, "exp");
        let first = svc.assign("exp", "user:1").unwrap();
        // Subsequent calls must return the same variant even with new service.
        for _ in 0..10 {
            assert_eq!(
                svc.assign("exp", "user:1").unwrap(),
                first,
                "sticky assignment must be returned on all subsequent calls"
            );
        }
    }

    // ── AC: sticky assignment not re-bucketed on weight change ────────────────

    #[test]
    fn set_weights_does_not_rebucket_existing_assignments() {
        let svc = make_svc();
        running(&svc, "exp");
        let original = svc.assign("exp", "user:1").unwrap();

        // Flip weights completely — existing assignment must remain unchanged.
        let (new_heavy, new_light) = if original == "control" {
            (0_u32, 100_u32)
        } else {
            (100_u32, 0_u32)
        };
        svc.set_weights(
            "exp",
            vec![
                VariantConfig::new("control", new_heavy),
                VariantConfig::new("treatment", new_light),
            ],
            None,
        )
        .unwrap();

        let after = svc.assign("exp", "user:1").unwrap();
        assert_eq!(
            original, after,
            "existing sticky assignment must not be re-bucketed after weight change"
        );
    }

    #[test]
    fn set_weights_rejects_deleting_assigned_variant() {
        let svc = make_svc();
        running(&svc, "exp");
        let original = svc.assign("exp", "user:1").unwrap();

        let remaining_variant = if original == "control" {
            "treatment"
        } else {
            "control"
        };
        let err = svc
            .set_weights(
                "exp",
                vec![VariantConfig::new(remaining_variant, 100)],
                None,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot delete variant"),
            "expected active assignment delete guard error, got {err}"
        );
    }

    // ── AC: exposure emitted exactly once per assign() call ───────────────────

    #[test]
    fn exposure_emitted_exactly_once_per_assign_call() {
        let (svc, records) = make_svc_with_sink();
        running(&svc, "exp");
        svc.assign("exp", "user:1").unwrap();
        assert_eq!(
            records.lock().unwrap().len(),
            1,
            "first assign → 1 exposure"
        );
        svc.assign("exp", "user:1").unwrap();
        assert_eq!(
            records.lock().unwrap().len(),
            2,
            "second assign → 2 total exposures"
        );
        svc.assign("exp", "user:2").unwrap();
        assert_eq!(
            records.lock().unwrap().len(),
            3,
            "different actor → 3 total exposures"
        );
    }

    // ── AC: exposure record contains correct fields ───────────────────────────

    #[test]
    fn exposure_record_contains_correct_fields() {
        let (svc, records) = make_svc_with_sink();
        running(&svc, "checkout_v2");
        let variant = svc
            .assign_with_request_id("checkout_v2", "user:42", Some("req-abc"))
            .unwrap();
        let (len, exp_name, exp_variant, exp_actor, exp_req_id, exp_is_override) = {
            let rec = records.lock().unwrap();
            let r = &rec[0];
            (
                rec.len(),
                r.experiment.clone(),
                r.variant.clone(),
                r.actor.clone(),
                r.request_id.clone(),
                r.is_override,
            )
        };
        assert_eq!(len, 1);
        assert_eq!(exp_name, "checkout_v2");
        assert_eq!(exp_variant, variant);
        assert_eq!(exp_actor, "user:42");
        assert_eq!(exp_req_id.as_deref(), Some("req-abc"));
        assert!(!exp_is_override);
    }

    // ── AC: override bypasses weight-based bucketing ──────────────────────────

    #[test]
    fn override_bypasses_weights() {
        let svc = make_svc();
        // 100% control — without override every actor gets "control".
        svc.create(ExperimentConfig::new(
            "exp",
            vec![
                VariantConfig::new("control", 100),
                VariantConfig::new("treatment", 0),
            ],
        ))
        .unwrap();
        svc.start("exp").unwrap();
        svc.set_override("exp", "qa:alice", "treatment").unwrap();
        let v = svc.assign("exp", "qa:alice").unwrap();
        assert_eq!(
            v, "treatment",
            "override must bypass weight-based bucketing"
        );
    }

    // ── AC: override emits exposure tagged as override ────────────────────────

    #[test]
    fn override_emits_exposure_tagged_as_override() {
        let (svc, records) = make_svc_with_sink();
        running(&svc, "exp");
        svc.set_override("exp", "qa:alice", "treatment").unwrap();
        svc.assign("exp", "qa:alice").unwrap();
        let (len, is_override, exp_variant) = {
            let rec = records.lock().unwrap();
            (rec.len(), rec[0].is_override, rec[0].variant.clone())
        };
        assert_eq!(len, 1);
        assert!(
            is_override,
            "exposure from override must be tagged is_override = true"
        );
        assert_eq!(exp_variant, "treatment");
    }

    // ── AC: mutual exclusion — second experiment in group is excluded ─────────

    #[test]
    fn mutual_exclusion_prevents_sibling_assignment() {
        let svc = make_svc();
        // Two experiments in the same group.
        svc.create(
            ExperimentConfig::new("exp_a", vec![VariantConfig::new("v1", 1)])
                .exclusion_group("checkout"),
        )
        .unwrap();
        svc.start("exp_a").unwrap();
        svc.create(
            ExperimentConfig::new("exp_b", vec![VariantConfig::new("v1", 1)])
                .exclusion_group("checkout"),
        )
        .unwrap();
        svc.start("exp_b").unwrap();

        // Assign actor to exp_a first.
        svc.assign("exp_a", "user:1").unwrap();

        // exp_b must exclude the same actor.
        let err = svc.assign("exp_b", "user:1").unwrap_err();
        assert!(
            matches!(err, ExperimentError::ExcludedByGroup(_, _)),
            "expected ExcludedByGroup, got {err}"
        );
    }

    // ── AC: mutual exclusion — different group allows both experiments ─────────

    #[test]
    fn different_groups_do_not_exclude_each_other() {
        let svc = make_svc();
        svc.create(
            ExperimentConfig::new("exp_a", vec![VariantConfig::new("v1", 1)])
                .exclusion_group("group_a"),
        )
        .unwrap();
        svc.start("exp_a").unwrap();
        svc.create(
            ExperimentConfig::new("exp_b", vec![VariantConfig::new("v1", 1)])
                .exclusion_group("group_b"),
        )
        .unwrap();
        svc.start("exp_b").unwrap();

        svc.assign("exp_a", "user:1").unwrap();
        let result = svc.assign("exp_b", "user:1");
        assert!(
            result.is_ok(),
            "experiments in different groups must not exclude each other"
        );
    }

    // ── AC: mutual exclusion — no group means no exclusion ────────────────────

    #[test]
    fn no_exclusion_group_allows_both_assignments() {
        let svc = make_svc();
        running(&svc, "exp_a");
        running(&svc, "exp_b");
        svc.assign("exp_a", "user:1").unwrap();
        let result = svc.assign("exp_b", "user:1");
        assert!(
            result.is_ok(),
            "experiments without exclusion groups must not exclude each other"
        );
    }

    // ── AC: experiment_bucket is stable and in range ──────────────────────────

    #[test]
    fn experiment_bucket_is_stable_and_in_range() {
        for i in 0..100_u32 {
            let actor = format!("user:{i}");
            let b1 = experiment_bucket("my_exp", &actor);
            let b2 = experiment_bucket("my_exp", &actor);
            assert_eq!(b1, b2, "bucket must be deterministic for {actor}");
            assert!(b1 < 10_000, "bucket must be in [0, 10000) for {actor}");
        }
    }

    // ── AC: experiment_bucket differs across actors ───────────────────────────

    #[test]
    fn experiment_bucket_produces_diverse_values() {
        let buckets: std::collections::HashSet<u64> = (0..100_u32)
            .map(|i| experiment_bucket("exp", &format!("user:{i}")))
            .collect();
        assert!(
            buckets.len() > 50,
            "expected diverse buckets across 100 actors, got {}",
            buckets.len()
        );
    }

    // ── AC: list returns all experiments ─────────────────────────────────────

    #[test]
    fn list_returns_all_experiments() {
        let svc = make_svc();
        svc.create(fifty_fifty("alpha")).unwrap();
        svc.create(fifty_fifty("beta")).unwrap();
        svc.create(fifty_fifty("gamma")).unwrap();
        let experiments = svc.list().unwrap();
        assert_eq!(experiments.len(), 3);
        assert_eq!(experiments[0].name, "alpha");
        assert_eq!(experiments[1].name, "beta");
        assert_eq!(experiments[2].name, "gamma");
    }

    // ── AC: status returns experiment config ─────────────────────────────────

    #[test]
    fn status_returns_current_config() {
        let svc = make_svc();
        running(&svc, "exp");
        let cfg = svc.status("exp").unwrap();
        assert_eq!(cfg.state, ExperimentState::Running);
        assert_eq!(cfg.variants.len(), 2);
    }

    // ── AC: history records mutations ─────────────────────────────────────────

    #[test]
    fn history_records_create_and_start_mutations() {
        let svc = make_svc();
        svc.create(fifty_fifty("exp")).unwrap();
        svc.start("exp").unwrap();
        let hist = svc.history("exp", 10).unwrap();
        assert!(!hist.is_empty(), "history must record mutations");
    }

    // ── AC: set_weights records mutation in history ───────────────────────────

    #[test]
    fn set_weights_recorded_in_history() {
        let svc = make_svc();
        running(&svc, "exp");
        svc.set_weights(
            "exp",
            vec![
                VariantConfig::new("control", 30),
                VariantConfig::new("treatment", 70),
            ],
            Some("ops@example.com"),
        )
        .unwrap();
        let hist = svc.history("exp", 10).unwrap();
        let has_set_weights = hist.iter().any(|r| r.mutation == "set_weights");
        assert!(has_set_weights, "set_weights must be recorded in history");
    }

    // ── AC: select_variant helper ─────────────────────────────────────────────

    #[test]
    fn select_variant_returns_none_for_empty_variants() {
        assert_eq!(select_variant(&[], 0), None);
    }

    #[test]
    fn select_variant_returns_none_for_all_zero_weights() {
        let vs = vec![VariantConfig::new("a", 0), VariantConfig::new("b", 0)];
        assert_eq!(select_variant(&vs, 5_000), None);
    }

    #[test]
    fn select_variant_50_50_boundary() {
        let vs = vec![
            VariantConfig::new("control", 50),
            VariantConfig::new("treatment", 50),
        ];
        // Bucket 0 (threshold = 0 * 100 / 10000 = 0) → cumulative[0]=50 > 0 → control
        assert_eq!(select_variant(&vs, 0), Some("control"));
        // Bucket 4999 (threshold = 4999 * 100 / 10000 = 49) → control
        assert_eq!(select_variant(&vs, 4_999), Some("control"));
        // Bucket 5000 (threshold = 5000 * 100 / 10000 = 50) → cumulative[0]=50 NOT > 50
        // → cumulative[1]=100 > 50 → treatment
        assert_eq!(select_variant(&vs, 5_000), Some("treatment"));
        // Bucket 9999 → treatment
        assert_eq!(select_variant(&vs, 9_999), Some("treatment"));
    }

    // ── AC: InMemoryExperimentStore — Arc delegation ──────────────────────────

    #[test]
    fn arc_experiment_store_delegates_all_operations() {
        let store = Arc::new(InMemoryExperimentStore::new());
        let arc_store: Arc<dyn ExperimentStore> = Arc::clone(&store) as _;

        let cfg = fifty_fifty("my_exp");
        arc_store.upsert(cfg).unwrap();
        assert!(arc_store.get("my_exp").unwrap().is_some());

        arc_store
            .set_state("my_exp", ExperimentState::Running, None)
            .unwrap();
        assert_eq!(
            arc_store.get("my_exp").unwrap().unwrap().state,
            ExperimentState::Running
        );

        arc_store
            .record_assignment(Assignment::new("my_exp", "user:1", "control", false))
            .unwrap();
        let asgn = arc_store.get_assignment("my_exp", "user:1").unwrap();
        assert_eq!(asgn.unwrap().variant, "control");

        arc_store
            .set_override("my_exp", "qa:1", "treatment")
            .unwrap();
        assert_eq!(
            arc_store.get_override("my_exp", "qa:1").unwrap().unwrap(),
            "treatment"
        );
    }

    // ── AC: ExperimentState display ───────────────────────────────────────────

    #[test]
    fn experiment_state_display_matches_expected() {
        assert_eq!(ExperimentState::Draft.to_string(), "draft");
        assert_eq!(ExperimentState::Running.to_string(), "running");
        assert_eq!(ExperimentState::Concluded.to_string(), "concluded");
        assert_eq!(ExperimentState::Archived.to_string(), "archived");
    }

    // ── AC: ExperimentError display ───────────────────────────────────────────

    #[test]
    fn experiment_error_display() {
        assert!(
            ExperimentError::NotFound("x".to_owned())
                .to_string()
                .contains("not found")
        );
        assert!(
            ExperimentError::Archived("x".to_owned())
                .to_string()
                .contains("archived")
        );
        assert!(
            ExperimentError::ExcludedByGroup("x".to_owned(), "g".to_owned())
                .to_string()
                .contains("mutual exclusion")
        );
        assert!(
            ExperimentError::NoVariant("x".to_owned())
                .to_string()
                .contains("weights are zero")
        );
    }

    // ── AC: service debug ─────────────────────────────────────────────────────

    #[test]
    fn service_debug_does_not_panic() {
        let svc = make_svc();
        let _ = format!("{svc:?}");
    }

    #[test]
    fn upsert_logs_created_or_updated() {
        let svc = make_svc();
        let exp = fifty_fifty("exp");
        svc.create(exp.clone()).unwrap();

        // Upsert the same experiment again to trigger update
        svc.create(exp).unwrap();

        let hist = svc.history("exp", 10).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[1].mutation, "created");
        assert_eq!(hist[0].mutation, "updated");
    }

    #[test]
    fn upsert_rejects_deleting_variant_with_active_assignments() {
        let store = InMemoryExperimentStore::new();
        let name = "test_exp";

        let config = ExperimentConfig {
            name: name.to_string(),
            description: None,
            state: ExperimentState::Running,
            variants: vec![
                VariantConfig {
                    name: "control".to_string(),
                    weight: 50,
                },
                VariantConfig {
                    name: "treatment".to_string(),
                    weight: 50,
                },
            ],
            winner: None,
            exclusion_group: None,
            updated_at_secs: 0,
        };
        store.upsert(config.clone()).unwrap();

        // Assign an actor to "treatment"
        store
            .record_assignment(Assignment {
                experiment: name.to_string(),
                actor: "user1".to_string(),
                variant: "treatment".to_string(),
                is_override: false,
                assigned_at_secs: 0,
            })
            .unwrap();

        // Attempting to upsert config without "treatment" should fail
        let mut new_config = config;
        new_config.variants = vec![VariantConfig {
            name: "control".to_string(),
            weight: 100,
        }];

        let res = store.upsert(new_config);
        assert!(
            res.is_err(),
            "expected upsert to fail due to deleting active variant"
        );
        if let Err(ExperimentStoreError::Backend(msg)) = res {
            assert!(
                msg.contains("treatment"),
                "expected error message to mention 'treatment', got: {msg}"
            );
        } else {
            panic!("expected Backend error");
        }
    }

    // ── Backend screening on the Postgres-only store ──────────────────────

    // `PgExperimentStore` opens a `diesel::PgConnection` and writes `jsonb` and
    // `pg_advisory_xact_lock` SQL — it cannot serve any other backend. Building one from a
    // SQLite target used to succeed and fail on first use with a driver-level
    // connection error naming Postgres, which is not a diagnosis an operator
    // who configured `sqlite://` can act on.
    #[cfg(feature = "db")]
    #[test]
    fn pg_experiment_store_refuses_a_non_postgres_target() {
        use crate::config::DatabaseConfig;

        let sqlite = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".to_owned()),
            ..Default::default()
        };
        assert!(
            pg::PgExperimentStore::from_database_config(&sqlite).is_none(),
            "a SQLite target has no Postgres experiment store"
        );

        // Fails closed: a target no backend claims is refused too.
        let unclassifiable = DatabaseConfig {
            primary_url: Some("/var/lib/app.db".to_owned()),
            ..Default::default()
        };
        assert!(
            pg::PgExperimentStore::from_database_config(&unclassifiable).is_none(),
            "an unclassifiable target has no Postgres experiment store"
        );

        // Both Postgres spellings still build.
        for url in [
            "postgres://localhost/app",
            "host=db user=app dbname=app sslmode=require",
        ] {
            let pg_config = DatabaseConfig {
                primary_url: Some(url.to_owned()),
                ..Default::default()
            };
            assert!(
                pg::PgExperimentStore::from_database_config(&pg_config).is_some(),
                "{url} is a Postgres target"
            );
        }
    }

    // `new(url)` does not screen the target. The first connect refuses it with
    // a message that names the cause, and no credential goes out (#2539 §5).
    #[cfg(feature = "db")]
    #[test]
    fn pg_experiment_store_new_refuses_a_non_postgres_target_at_first_use() {
        use crate::experiments::ExperimentStore as _;

        let store = pg::PgExperimentStore::new("sqlite://app:hunter2@host/app.db");
        let err = store
            .get("exp")
            .expect_err("a SQLite target is refused")
            .to_string();
        assert!(
            err.contains("Postgres"),
            "the refusal names the cause: {err}"
        );
        assert!(!err.contains("hunter2"), "the refusal leaks: {err}");

        // libpq quotes a malformed target in its error. That must not leak.
        let store = pg::PgExperimentStore::new("postgres://app:hunter2@[::1/db");
        let err = store
            .get("exp")
            .expect_err("a malformed target fails")
            .to_string();
        assert!(!err.contains("hunter2"), "the driver error leaks: {err}");

        // libpq can quote only the decoded password.
        let store = pg::PgExperimentStore::new("postgres://app:p%ss@localhost:1/db");
        let err = store
            .get("exp")
            .expect_err("a bad escape fails")
            .to_string();
        assert!(!err.contains("p%ss"), "the driver error leaks: {err}");
    }
}
