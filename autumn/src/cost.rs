//! Per-request cost records and a live cost signal (issue #1720).
//!
//! This module has three parts:
//!
//! - [`CostAccountant`] records the CPU time, allocated bytes and DB-query
//!   count of each request. It adds each request to the total of its tenant.
//!   The [`CostLayer`](crate::middleware::CostLayer) measures the request.
//! - [`CostSignal`] holds a live value (carbon g/kWh or price per unit) and a
//!   threshold. Set it from code, or from the runtime-config key
//!   [`COST_SIGNAL_KEY`](crate::runtime_config::COST_SIGNAL_KEY).
//! - Deferral. Work marked `deferrable` (`#[job(deferrable)]`,
//!   `#[scheduled(..., deferrable)]`) waits while the signal is above the
//!   threshold. The work runs when the signal falls. The runtime never drops
//!   the work. Request handlers do not wait. Scheduled tasks wait on all
//!   backends. Only the `local` jobs backend makes jobs wait.
//!
//! Set `[cost] enabled = true` to measure requests. See `docs/guide/cost.md`.
//!
//! # How the CPU time is measured
//!
//! The layer reads the thread CPU clock before and after each poll of the
//! request future. On Linux, Android, macOS, iOS and FreeBSD this is
//! `CLOCK_THREAD_CPUTIME_ID`. On other targets it is the wall time of the
//! poll. The layer does not count work that the handler moves to another task
//! (`tokio::spawn`, `spawn_blocking`). The layer does not count the time to
//! stream the response body.
//!
//! # How the allocated bytes are measured
//!
//! The framework does not install a global allocator. Give it an
//! [`AllocationProbe`] that counts the bytes a closure allocates on the
//! current thread. Without a probe, the allocated bytes are zero.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use serde::Serialize;

use crate::actuator::{MetricFamily, MetricKind, MetricSample, MetricsSource};

/// The tenant key for a request that has no tenant.
pub const UNATTRIBUTED_TENANT: &str = "_none";

/// The tenant key for tenants past the [`CostAccountant`] tenant limit.
pub const OVERFLOW_TENANT: &str = "_other";

/// The name of the cost metrics source in the actuator registry.
pub const METRICS_SOURCE_NAME: &str = "autumn.cost";

/// The default time between two checks while work waits, in milliseconds.
const DEFAULT_RECHECK_MS: u64 = 30_000;

/// The shortest time between two checks while work waits, in milliseconds.
const MIN_RECHECK_MS: u64 = 1_000;

/// The longest tenant id that gets its own key. A longer id goes into
/// [`OVERFLOW_TENANT`], so a client cannot make large keys.
pub const MAX_TENANT_KEY_BYTES: usize = 64;

// ── Request cost ────────────────────────────────────────────────────

/// The measured cost of one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestCost {
    /// CPU time that the request used on the worker threads.
    pub cpu: Duration,
    /// Bytes allocated while the request ran. Zero without a probe.
    pub allocated_bytes: u64,
    /// Number of DB queries that the request ran.
    pub db_queries: u64,
    /// The tenant that caused the request, if any.
    pub tenant: Option<String>,
}

impl RequestCost {
    /// Make a request cost with no tenant.
    #[must_use]
    pub const fn new(cpu: Duration, allocated_bytes: u64, db_queries: u64) -> Self {
        Self {
            cpu,
            allocated_bytes,
            db_queries,
            tenant: None,
        }
    }

    /// Set the tenant.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }
}

/// The total cost of all requests for one tenant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct TenantCost {
    /// Number of requests.
    pub requests: u64,
    /// Total CPU time, in microseconds.
    pub cpu_micros: u64,
    /// Total allocated bytes.
    pub allocated_bytes: u64,
    /// Total DB queries.
    pub db_queries: u64,
}

impl TenantCost {
    /// Total CPU time, in seconds.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn cpu_seconds(&self) -> f64 {
        self.cpu_micros as f64 / 1_000_000.0
    }

    const fn add(&mut self, cpu_micros: u64, allocated_bytes: u64, db_queries: u64) {
        self.requests = self.requests.saturating_add(1);
        self.cpu_micros = self.cpu_micros.saturating_add(cpu_micros);
        self.allocated_bytes = self.allocated_bytes.saturating_add(allocated_bytes);
        self.db_queries = self.db_queries.saturating_add(db_queries);
    }
}

/// A point-in-time copy of the [`CostAccountant`] totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CostSnapshot {
    /// The total for all tenants.
    pub total: TenantCost,
    /// The total for each tenant key.
    pub tenants: BTreeMap<String, TenantCost>,
}

// ── Allocation probe ────────────────────────────────────────────────

/// Counts the bytes that a closure allocates on the current thread.
///
/// The [`CostLayer`](crate::middleware::CostLayer) calls [`measure`] once for
/// each poll of the request future. Make it cheap.
///
/// ```rust,ignore
/// struct Counting;
///
/// impl autumn_web::cost::AllocationProbe for Counting {
///     fn measure(&self, poll: &mut dyn FnMut()) -> u64 {
///         allocation_counter::measure(poll).bytes_total
///     }
/// }
/// ```
///
/// [`measure`]: AllocationProbe::measure
pub trait AllocationProbe: Send + Sync + 'static {
    /// Run `poll` one time. Return the bytes that it allocated.
    fn measure(&self, poll: &mut dyn FnMut()) -> u64;
}

// ── Accountant ──────────────────────────────────────────────────────

/// Records request costs and keeps a total for each tenant.
///
/// Clones share the same totals. The framework puts one in the app state
/// when `[cost] enabled = true`. It registers it as the
/// [`METRICS_SOURCE_NAME`] metrics source.
///
/// The tenant key is the tenant id. These ids go into [`OVERFLOW_TENANT`]:
/// an id longer than [`MAX_TENANT_KEY_BYTES`], an id equal to a reserved key,
/// and a new id when the accountant has `max_tenants` keys.
#[derive(Clone)]
pub struct CostAccountant {
    inner: Arc<AccountantInner>,
}

struct AccountantInner {
    max_tenants: usize,
    probe: Option<Arc<dyn AllocationProbe>>,
    tenant_labels: bool,
    totals: Mutex<Totals>,
}

#[derive(Default)]
struct Totals {
    total: TenantCost,
    tenants: HashMap<String, TenantCost>,
}

impl std::fmt::Debug for CostAccountant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CostAccountant")
            .field("max_tenants", &self.inner.max_tenants)
            .field("probe", &self.inner.probe.is_some())
            .field("tenant_labels", &self.inner.tenant_labels)
            .finish_non_exhaustive()
    }
}

impl CostAccountant {
    /// Make an accountant that keeps at most `max_tenants` tenant keys.
    ///
    /// When the accountant has `max_tenants` keys, it adds a new tenant to
    /// [`OVERFLOW_TENANT`]. This keeps memory and metric labels bounded. The
    /// two reserved keys, [`UNATTRIBUTED_TENANT`] and [`OVERFLOW_TENANT`], do
    /// not count toward the limit.
    #[must_use]
    pub fn new(max_tenants: usize) -> Self {
        Self::build(max_tenants, None, true)
    }

    /// Use `probe` to count allocated bytes.
    ///
    /// Call this before you use the accountant: it starts with empty totals.
    #[must_use]
    pub fn with_allocation_probe(self, probe: Arc<dyn AllocationProbe>) -> Self {
        Self::build(
            self.inner.max_tenants,
            Some(probe),
            self.inner.tenant_labels,
        )
    }

    /// Set whether the metrics source shows one sample for each tenant.
    ///
    /// With `false`, each metric has one sample: the total. The framework sets
    /// `false` when `[actuator] sensitive` is off, because `/actuator/metrics`
    /// and `/actuator/prometheus` are public. Call this before you use the
    /// accountant: it starts with empty totals.
    #[must_use]
    pub fn with_tenant_labels(self, tenant_labels: bool) -> Self {
        Self::build(
            self.inner.max_tenants,
            self.inner.probe.clone(),
            tenant_labels,
        )
    }

    fn build(
        max_tenants: usize,
        probe: Option<Arc<dyn AllocationProbe>>,
        tenant_labels: bool,
    ) -> Self {
        Self {
            inner: Arc::new(AccountantInner {
                max_tenants,
                probe,
                tenant_labels,
                totals: Mutex::new(Totals::default()),
            }),
        }
    }

    /// The allocation probe, if one is set.
    #[must_use]
    pub fn allocation_probe(&self) -> Option<&Arc<dyn AllocationProbe>> {
        self.inner.probe.as_ref()
    }

    /// Add one request to the totals.
    pub fn record(&self, cost: &RequestCost) {
        self.record_parts(
            cost.cpu,
            cost.allocated_bytes,
            cost.db_queries,
            cost.tenant.as_deref(),
        );
    }

    /// Add one request to the totals, with a borrowed tenant id.
    pub(crate) fn record_parts(
        &self,
        cpu: Duration,
        allocated_bytes: u64,
        db_queries: u64,
        tenant: Option<&str>,
    ) {
        let micros = u64::try_from(cpu.as_micros()).unwrap_or(u64::MAX);
        let key = match tenant {
            None => UNATTRIBUTED_TENANT,
            Some(id)
                if id.len() > MAX_TENANT_KEY_BYTES
                    || id == UNATTRIBUTED_TENANT
                    || id == OVERFLOW_TENANT =>
            {
                OVERFLOW_TENANT
            }
            Some(id) => id,
        };
        let mut totals = self.lock();
        totals.total.add(micros, allocated_bytes, db_queries);
        // `get_mut` first: a known tenant does not allocate a key.
        if let Some(entry) = totals.tenants.get_mut(key) {
            entry.add(micros, allocated_bytes, db_queries);
            return;
        }
        // The reserved keys do not count toward the limit, so requests with
        // no tenant keep their own key.
        let reserved = [UNATTRIBUTED_TENANT, OVERFLOW_TENANT]
            .iter()
            .filter(|reserved| totals.tenants.contains_key(**reserved))
            .count();
        let key = if key == UNATTRIBUTED_TENANT
            || totals.tenants.len() - reserved < self.inner.max_tenants
        {
            key
        } else {
            OVERFLOW_TENANT
        };
        totals
            .tenants
            .entry(key.to_owned())
            .or_default()
            .add(micros, allocated_bytes, db_queries);
    }

    /// Copy the current totals.
    #[must_use]
    pub fn snapshot(&self) -> CostSnapshot {
        let totals = self.lock();
        CostSnapshot {
            total: totals.total,
            tenants: totals
                .tenants
                .iter()
                .map(|(key, cost)| (key.clone(), *cost))
                .collect(),
        }
    }

    /// The total for one tenant key, if it has a request.
    #[must_use]
    pub fn tenant(&self, tenant: &str) -> Option<TenantCost> {
        self.lock().tenants.get(tenant).copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Totals> {
        self.inner
            .totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl MetricsSource for CostAccountant {
    fn collect(&self) -> Vec<MetricFamily> {
        self.metric_families(self.inner.tenant_labels)
    }
}

/// The registered metrics view of an accountant that the app inserted: it
/// shows tenant labels only when the accountant and the actuator both allow
/// them, so the public endpoints keep tenant ids out by default.
struct PublicCostMetrics {
    accountant: CostAccountant,
    tenant_labels: bool,
}

impl MetricsSource for PublicCostMetrics {
    fn collect(&self) -> Vec<MetricFamily> {
        self.accountant.metric_families(self.tenant_labels)
    }
}

impl CostAccountant {
    #[allow(clippy::cast_precision_loss)]
    fn metric_families(&self, tenant_labels: bool) -> Vec<MetricFamily> {
        let snapshot = self.snapshot();
        let family = |name: &str, help: &str, value: fn(&TenantCost) -> f64| MetricFamily {
            name: name.to_owned(),
            help: help.to_owned(),
            kind: MetricKind::Counter,
            samples: if tenant_labels {
                snapshot
                    .tenants
                    .iter()
                    .map(|(tenant, cost)| MetricSample {
                        labels: vec![("tenant".to_owned(), tenant.clone())],
                        value: value(cost),
                    })
                    .collect()
            } else {
                vec![MetricSample {
                    labels: Vec::new(),
                    value: value(&snapshot.total),
                }]
            },
        };
        vec![
            family("autumn_cost_requests_total", "Measured requests.", |c| {
                c.requests as f64
            }),
            family(
                "autumn_cost_cpu_seconds_total",
                "CPU seconds that requests used.",
                TenantCost::cpu_seconds,
            ),
            family(
                "autumn_cost_allocated_bytes_total",
                "Bytes that requests allocated. Zero without a probe.",
                |c| c.allocated_bytes as f64,
            ),
            family(
                "autumn_cost_db_queries_total",
                "DB queries that requests ran.",
                |c| c.db_queries as f64,
            ),
        ]
    }
}

// ── Cost signal ─────────────────────────────────────────────────────

/// A live cost value (carbon g/kWh or price per unit) and a threshold.
///
/// Clones share the same value. The framework always puts one in the app
/// state. Its threshold comes from `[cost] defer_threshold`. When the value is
/// above the threshold, the signal is *high* and deferrable work waits.
///
/// ```rust
/// use autumn_web::cost::CostSignal;
///
/// let signal = CostSignal::new(Some(400.0));
/// signal.set(250.0);
/// assert!(!signal.is_high());
/// signal.set(520.0);
/// assert!(signal.is_high());
/// ```
#[derive(Clone, Debug)]
pub struct CostSignal {
    inner: Arc<SignalInner>,
}

#[derive(Debug)]
struct SignalInner {
    /// `f64` bits.
    value: AtomicU64,
    /// `f64` bits. `+inf` means no threshold.
    threshold: AtomicU64,
    deferrals: AtomicU64,
    /// Milliseconds between two checks while work waits.
    recheck_ms: AtomicU64,
    /// `true` until the first value arrives from runtime config.
    pending: std::sync::atomic::AtomicBool,
}

/// A point-in-time copy of a [`CostSignal`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CostSignalSnapshot {
    /// The current value.
    pub value: f64,
    /// The threshold. `None` means that work never waits.
    pub threshold: Option<f64>,
    /// `true` when the value is above the threshold, or when the first value
    /// from runtime config has not arrived yet.
    pub high: bool,
    /// `true` until the first value arrives from runtime config.
    pub pending: bool,
    /// How many times work started to wait. A job or a task tick that waits
    /// adds one, however long it waits.
    pub deferrals: u64,
}

impl Default for CostSignal {
    fn default() -> Self {
        Self::new(None)
    }
}

impl CostSignal {
    /// Make a signal with the value `0.0`.
    ///
    /// `threshold = None` means that the signal is never high.
    #[must_use]
    pub fn new(threshold: Option<f64>) -> Self {
        let signal = Self {
            inner: Arc::new(SignalInner {
                value: AtomicU64::new(0.0_f64.to_bits()),
                threshold: AtomicU64::new(f64::INFINITY.to_bits()),
                deferrals: AtomicU64::new(0),
                recheck_ms: AtomicU64::new(DEFAULT_RECHECK_MS),
                pending: std::sync::atomic::AtomicBool::new(false),
            }),
        };
        signal.set_threshold(threshold);
        signal
    }

    /// Set the value. This function ignores a value that is not finite.
    pub fn set(&self, value: f64) {
        if value.is_finite() {
            self.inner.value.store(value.to_bits(), Ordering::Relaxed);
            self.inner.pending.store(false, Ordering::Release);
        }
    }

    /// Hold work until the first value arrives: a store can already hold a
    /// high value when the app starts.
    fn mark_pending(&self) {
        self.inner.pending.store(true, Ordering::Release);
    }

    /// `true` until the first value arrives from runtime config.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.inner.pending.load(Ordering::Acquire)
    }

    /// The current value.
    #[must_use]
    pub fn value(&self) -> f64 {
        f64::from_bits(self.inner.value.load(Ordering::Relaxed))
    }

    /// Set the threshold. `None`, or a value that is not finite, turns
    /// deferral off.
    pub fn set_threshold(&self, threshold: Option<f64>) {
        let bits = threshold
            .filter(|t| t.is_finite())
            .unwrap_or(f64::INFINITY)
            .to_bits();
        self.inner.threshold.store(bits, Ordering::Relaxed);
    }

    /// The threshold, if one is set.
    #[must_use]
    pub fn threshold(&self) -> Option<f64> {
        let threshold = f64::from_bits(self.inner.threshold.load(Ordering::Relaxed));
        threshold.is_finite().then_some(threshold)
    }

    /// `true` when the value is above the threshold. With a threshold, it is
    /// also `true` until the first value arrives from runtime config, so
    /// deferrable work does not run before the app knows the signal.
    #[must_use]
    pub fn is_high(&self) -> bool {
        self.threshold()
            .is_some_and(|threshold| self.is_pending() || self.value() > threshold)
    }

    /// Copy the current state.
    #[must_use]
    pub fn snapshot(&self) -> CostSignalSnapshot {
        CostSignalSnapshot {
            value: self.value(),
            threshold: self.threshold(),
            high: self.is_high(),
            pending: self.is_pending(),
            deferrals: self.inner.deferrals.load(Ordering::Relaxed),
        }
    }

    /// Read [`COST_SIGNAL_KEY`](crate::runtime_config::COST_SIGNAL_KEY) from
    /// `config` and set the value.
    ///
    /// # Errors
    ///
    /// Returns the error of [`RuntimeConfigService::get`], or
    /// [`ConfigError::TypeMismatch`] when the value is not a number.
    ///
    /// [`RuntimeConfigService::get`]: crate::runtime_config::RuntimeConfigService::get
    /// [`ConfigError::TypeMismatch`]: crate::runtime_config::ConfigError::TypeMismatch
    pub fn sync_from(
        &self,
        config: &crate::runtime_config::RuntimeConfigService,
    ) -> Result<(), crate::runtime_config::ConfigError> {
        use crate::runtime_config::{COST_SIGNAL_KEY, ConfigError};
        let value = config.get(COST_SIGNAL_KEY)?;
        let value = value.as_float().ok_or_else(|| ConfigError::TypeMismatch {
            key: COST_SIGNAL_KEY.to_owned(),
            reason: format!("expected float, got {}", value.value_type()),
        })?;
        // A stored value that is not finite is an error, not a silent no-op:
        // the refresher logs it, and the signal keeps its last good value.
        if !value.is_finite() {
            return Err(ConfigError::TypeMismatch {
                key: COST_SIGNAL_KEY.to_owned(),
                reason: format!("expected a finite float, got {value}"),
            });
        }
        self.set(value);
        Ok(())
    }

    /// The time between two checks while work waits.
    pub(crate) fn recheck(&self) -> Duration {
        Duration::from_millis(self.inner.recheck_ms.load(Ordering::Relaxed))
    }

    fn set_recheck(&self, every: Duration) {
        let ms = u64::try_from(every.as_millis())
            .unwrap_or(u64::MAX)
            .max(MIN_RECHECK_MS);
        self.inner.recheck_ms.store(ms, Ordering::Relaxed);
    }

    fn note_deferral(&self) {
        self.inner.deferrals.fetch_add(1, Ordering::Relaxed);
    }
}

// ── Request lane ────────────────────────────────────────────────────

/// The DB-query counter for one metered request.
#[derive(Debug, Default)]
pub(crate) struct RequestCostCell {
    db_queries: AtomicU64,
}

impl RequestCostCell {
    pub(crate) fn db_queries(&self) -> u64 {
        self.db_queries.load(Ordering::Relaxed)
    }
}

tokio::task_local! {
    /// The cost cell of the request that the `CostLayer` meters.
    static REQUEST_COST: Arc<RequestCostCell>;
}

/// The request future, in the scope of its cost cell.
pub(crate) type ScopedRequest<F> = tokio::task::futures::TaskLocalFuture<Arc<RequestCostCell>, F>;

/// Run `fut` in the scope of `cell`, so DB queries count into it.
pub(crate) fn scope_request<F: std::future::Future>(
    cell: Arc<RequestCostCell>,
    fut: F,
) -> ScopedRequest<F> {
    REQUEST_COST.scope(cell, fut)
}

/// `true` inside a metered request. The DB layer installs its query timer
/// only when a lane is active.
#[cfg(feature = "db")]
pub(crate) fn request_cost_active() -> bool {
    REQUEST_COST.try_with(|_| ()).is_ok()
}

/// Count one DB query for the current metered request, if any.
#[cfg(feature = "db")]
pub(crate) fn record_db_query() {
    let _ = REQUEST_COST.try_with(|cell| cell.db_queries.fetch_add(1, Ordering::Relaxed));
}

/// A start mark for the CPU clock of the current thread.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CpuMark {
    /// Thread CPU time at the mark.
    Thread(Duration),
    /// Wall time at the mark. Used when no thread CPU clock is available.
    Wall(std::time::Instant),
}

/// Read the CPU clock of the current thread.
pub(crate) fn cpu_mark() -> CpuMark {
    thread_cpu_time().map_or_else(wall_mark, CpuMark::Thread)
}

/// The CPU time of the current thread since `mark`.
///
/// Read the mark and this value on the same thread. A request poll does not
/// move between threads, so this is true for the `CostLayer`.
pub(crate) fn cpu_since(mark: CpuMark) -> Duration {
    match mark {
        CpuMark::Thread(start) => {
            thread_cpu_time().map_or(Duration::ZERO, |now| now.saturating_sub(start))
        }
        CpuMark::Wall(start) => start.elapsed(),
    }
}

// Real wall time, not the injected clock: this measures how long a poll ran on
// the CPU. The sim clock does not move during a poll, so it would read zero.
#[allow(
    clippy::disallowed_methods,
    reason = "measures on-CPU poll time; the injected clock does not move during a poll"
)]
fn wall_mark() -> CpuMark {
    CpuMark::Wall(std::time::Instant::now())
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
))]
fn thread_cpu_time() -> Option<Duration> {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID)
        .ok()
        .map(Duration::from)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)))]
const fn thread_cpu_time() -> Option<Duration> {
    None
}

// ── Deferrable work ─────────────────────────────────────────────────

/// The kind of deferrable work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorkKind {
    /// A `#[job]`.
    Job,
    /// A `#[scheduled]` task.
    Task,
}

/// The deferrable job and task names.
#[derive(Default)]
struct Deferrable {
    /// `true` once any name is marked. The check is then one atomic load for
    /// an app that marks nothing.
    any: AtomicBool,
    jobs: RwLock<HashSet<String>>,
    tasks: RwLock<HashSet<String>>,
}

impl Deferrable {
    const fn set(&self, kind: WorkKind) -> &RwLock<HashSet<String>> {
        match kind {
            WorkKind::Job => &self.jobs,
            WorkKind::Task => &self.tasks,
        }
    }
}

fn deferrable() -> &'static Deferrable {
    static NAMES: OnceLock<Deferrable> = OnceLock::new();
    NAMES.get_or_init(Deferrable::default)
}

/// Mark the job or task `name` as deferrable.
///
/// `#[job(deferrable)]` and `#[scheduled(..., deferrable)]` call this. Call it
/// yourself for a `JobInfo` or `TaskInfo` that you make by hand. The mark is
/// process-wide. You cannot remove it.
pub fn mark_deferrable(kind: WorkKind, name: &str) {
    if is_deferrable(kind, name) {
        return;
    }
    let names = deferrable();
    names
        .set(kind)
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_owned());
    names.any.store(true, Ordering::Release);
}

/// `true` when the job or task `name` is deferrable.
#[must_use]
pub fn is_deferrable(kind: WorkKind, name: &str) -> bool {
    let names = deferrable();
    names.any.load(Ordering::Acquire)
        && names
            .set(kind)
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name)
}

/// Return the app signal when `name` is deferrable and the signal is high.
pub(crate) fn deferral_signal(
    state: &crate::AppState,
    kind: WorkKind,
    name: &str,
) -> Option<Arc<CostSignal>> {
    if !is_deferrable(kind, name) {
        return None;
    }
    state
        .extension::<CostSignal>()
        .filter(|signal| signal.is_high())
}

/// Wait while `name` must defer. Return `false` when `shutdown` fires first.
///
/// The scheduler calls this after it takes the tick lease, or before it with
/// a lease that expires. `waiting` is set to `true` when the wait starts. The
/// caller clears it when the tick no longer needs the reservation.
pub(crate) async fn wait_while_deferred(
    state: &crate::AppState,
    kind: WorkKind,
    name: &str,
    shutdown: &tokio_util::sync::CancellationToken,
    waiting: Option<&AtomicBool>,
) -> bool {
    let Some(signal) = deferral_signal(state, kind, name) else {
        return true;
    };
    signal.note_deferral();
    tracing::info!(
        task = name,
        value = signal.value(),
        "cost signal is high; task waits"
    );
    if let Some(flag) = waiting {
        flag.store(true, Ordering::Release);
    }
    let mut resumed = true;
    while signal.is_high() {
        tokio::select! {
            () = shutdown.cancelled() => {
                resumed = false;
                break;
            }
            () = tokio::time::sleep(signal.recheck()) => {}
        }
    }
    if resumed {
        tracing::info!(task = name, "cost signal is low; task resumes");
    }
    resumed
}

/// Note one deferral on the app signal.
pub(crate) fn note_deferral(signal: &CostSignal) {
    signal.note_deferral();
}

// ── Install ─────────────────────────────────────────────────────────

/// Put the cost plane into the app state. Every boot path calls this after the
/// app state initializers, so a value that the app inserted wins.
///
/// - A [`CostSignal`] is always present. Its threshold comes from
///   `[cost] defer_threshold`.
/// - With `[cost] enabled`, a [`CostAccountant`] is present and registered as
///   the [`METRICS_SOURCE_NAME`] metrics source. It uses an
///   `Arc<dyn AllocationProbe>` extension when the app inserted one. Its
///   metrics show tenant labels only when `[actuator] sensitive` is on.
/// - With an `Arc<RuntimeConfigService>` extension that declares
///   [`COST_SIGNAL_KEY`](crate::runtime_config::COST_SIGNAL_KEY), a task
///   copies the key into the signal every `signal_refresh_secs`.
pub(crate) fn install(state: &crate::AppState, config: &crate::config::AutumnConfig) {
    let cost = &config.cost;
    let signal = state.extension_or_insert_with(|| CostSignal::new(cost.defer_threshold));
    signal.set_recheck(Duration::from_secs(cost.defer_recheck_secs));

    if cost.enabled {
        let sensitive = config.actuator.sensitive;
        // An accountant that the app inserted is still the metrics source;
        // its tenant labels show only in sensitive mode.
        let source: Arc<dyn MetricsSource> =
            if let Some(accountant) = state.extension::<CostAccountant>() {
                Arc::new(PublicCostMetrics {
                    tenant_labels: accountant.inner.tenant_labels && sensitive,
                    accountant: (*accountant).clone(),
                })
            } else {
                let mut accountant =
                    CostAccountant::new(cost.max_tenants).with_tenant_labels(sensitive);
                if let Some(probe) = state.extension::<Arc<dyn AllocationProbe>>() {
                    accountant = accountant.with_allocation_probe((*probe).clone());
                }
                state.insert_extension(accountant.clone());
                Arc::new(accountant)
            };
        let registry = state.metrics_source_registry();
        if !registry.contains(METRICS_SOURCE_NAME)
            && let Err(error) = registry.register(METRICS_SOURCE_NAME, source)
        {
            tracing::warn!("{error}");
        }
    }

    if let Some(service) = state.extension::<Arc<crate::runtime_config::RuntimeConfigService>>()
        && service
            .registry()
            .get(crate::runtime_config::COST_SIGNAL_KEY)
            .is_some()
    {
        spawn_signal_refresh(
            &signal,
            Arc::clone(&*service),
            Duration::from_secs(cost.signal_refresh_secs.max(1)),
        );
    }
}

/// Copy the runtime-config signal into `signal` until the signal is dropped.
///
/// The first read is in the task too: a Postgres store can block, and boot
/// must not wait for it.
fn spawn_signal_refresh(
    signal: &CostSignal,
    service: Arc<crate::runtime_config::RuntimeConfigService>,
    every: Duration,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("no tokio runtime; the cost signal does not follow runtime config");
        return;
    };
    signal.mark_pending();
    let weak = Arc::downgrade(&signal.inner);
    handle.spawn(async move {
        loop {
            let Some(inner) = weak.upgrade() else { return };
            let signal = CostSignal { inner };
            let service = Arc::clone(&service);
            // The store can block (Postgres), so read it off the reactor.
            let result = crate::time::spawn_blocking(move || signal.sync_from(&service)).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "cannot read the cost signal from runtime config");
                }
                Err(error) => tracing::warn!(%error, "cost signal refresh task failed"),
            }
            tokio::time::sleep(every).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(cpu_us: u64, alloc: u64, db: u64) -> RequestCost {
        RequestCost::new(Duration::from_micros(cpu_us), alloc, db)
    }

    #[test]
    fn records_totals_for_each_tenant() {
        let accountant = CostAccountant::new(10);
        accountant.record(&cost(100, 10, 1).with_tenant("acme"));
        accountant.record(&cost(300, 30, 2).with_tenant("acme"));
        accountant.record(&cost(50, 5, 0).with_tenant("globex"));

        let acme = accountant.tenant("acme").expect("acme has requests");
        assert_eq!(acme.requests, 2);
        assert_eq!(acme.cpu_micros, 400);
        assert_eq!(acme.allocated_bytes, 40);
        assert_eq!(acme.db_queries, 3);

        let snapshot = accountant.snapshot();
        assert_eq!(snapshot.total.requests, 3);
        assert_eq!(snapshot.total.cpu_micros, 450);
        assert_eq!(snapshot.tenants.len(), 2);
    }

    #[test]
    fn request_without_tenant_goes_to_unattributed() {
        let accountant = CostAccountant::new(10);
        accountant.record(&cost(10, 0, 0));
        assert_eq!(
            accountant.tenant(UNATTRIBUTED_TENANT).map(|t| t.requests),
            Some(1)
        );
    }

    #[test]
    fn tenants_past_the_limit_go_to_overflow() {
        let accountant = CostAccountant::new(2);
        for tenant in ["a", "b", "c", "d"] {
            accountant.record(&cost(1, 0, 0).with_tenant(tenant));
        }
        // Known tenants keep their own key after the limit.
        accountant.record(&cost(1, 0, 0).with_tenant("a"));

        let snapshot = accountant.snapshot();
        assert_eq!(snapshot.tenants.len(), 3, "{:?}", snapshot.tenants);
        assert_eq!(snapshot.tenants["a"].requests, 2);
        assert_eq!(snapshot.tenants[OVERFLOW_TENANT].requests, 2);
        assert_eq!(snapshot.total.requests, 5);
    }

    /// Requests with no tenant keep `_none` after the limit (#1720).
    #[test]
    fn unattributed_requests_keep_their_key_past_the_limit() {
        let accountant = CostAccountant::new(2);
        for tenant in ["a", "b", "c"] {
            accountant.record(&cost(1, 0, 0).with_tenant(tenant));
        }
        accountant.record(&cost(1, 0, 0));
        accountant.record(&cost(1, 0, 0).with_tenant("d"));

        let snapshot = accountant.snapshot();
        assert_eq!(snapshot.tenants[UNATTRIBUTED_TENANT].requests, 1);
        assert_eq!(snapshot.tenants[OVERFLOW_TENANT].requests, 2);
        assert_eq!(snapshot.tenants.len(), 4, "{:?}", snapshot.tenants);
    }

    #[test]
    fn long_and_reserved_tenant_ids_go_to_overflow() {
        let accountant = CostAccountant::new(10);
        let long = "x".repeat(MAX_TENANT_KEY_BYTES + 1);
        accountant.record(&cost(1, 0, 0).with_tenant(long.as_str()));
        accountant.record(&cost(1, 0, 0).with_tenant(UNATTRIBUTED_TENANT));
        accountant.record(&cost(1, 0, 0).with_tenant(OVERFLOW_TENANT));
        accountant.record(&cost(1, 0, 0).with_tenant("x".repeat(MAX_TENANT_KEY_BYTES)));

        let snapshot = accountant.snapshot();
        assert_eq!(snapshot.tenants[OVERFLOW_TENANT].requests, 3);
        assert!(!snapshot.tenants.contains_key(UNATTRIBUTED_TENANT));
        assert!(!snapshot.tenants.contains_key(long.as_str()));
        assert_eq!(snapshot.tenants.len(), 2, "{:?}", snapshot.tenants.keys());
    }

    #[test]
    fn metrics_without_tenant_labels_show_only_the_total() {
        let accountant = CostAccountant::new(10).with_tenant_labels(false);
        accountant.record(&cost(1_000_000, 0, 2).with_tenant("acme"));
        accountant.record(&cost(1_000_000, 0, 1).with_tenant("globex"));

        for family in accountant.collect() {
            assert_eq!(family.samples.len(), 1, "{}", family.name);
            assert!(family.samples[0].labels.is_empty(), "{}", family.name);
        }
        let db = &accountant.collect()[3];
        assert!((db.samples[0].value - 3.0).abs() < f64::EPSILON);
    }

    /// An accountant that the app inserted is still the metrics source, and
    /// it keeps tenant ids off the public endpoints (#1720).
    #[tokio::test]
    async fn install_registers_an_accountant_the_app_inserted() {
        for (sensitive, labeled) in [(false, false), (true, true)] {
            let state = crate::AppState::for_test();
            let mine = CostAccountant::new(8).with_tenant_labels(true);
            state.insert_extension(mine.clone());
            let mut config = crate::config::AutumnConfig::default();
            config.cost.enabled = true;
            config.actuator.sensitive = sensitive;
            install(&state, &config);

            mine.record(&cost(1_000_000, 0, 1).with_tenant("acme"));
            let sources = state.metrics_source_registry().collect_all();
            let (_, families) = sources
                .iter()
                .find(|(name, _)| name == METRICS_SOURCE_NAME)
                .expect("the inserted accountant is registered");
            let requests = &families[0];
            assert_eq!(
                requests.samples.iter().any(|s| !s.labels.is_empty()),
                labeled,
                "sensitive = {sensitive}"
            );
            assert!(
                state
                    .extension::<CostAccountant>()
                    .is_some_and(|a| Arc::ptr_eq(&a.inner, &mine.inner)),
                "the app's accountant stays in the state"
            );
        }
    }

    #[test]
    fn cpu_seconds_converts_micros() {
        let accountant = CostAccountant::new(1);
        accountant.record(&cost(1_500_000, 0, 0));
        let total = accountant.snapshot().total;
        assert!((total.cpu_seconds() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_source_emits_per_tenant_counters() {
        let accountant = CostAccountant::new(10);
        accountant.record(&cost(2_000_000, 64, 3).with_tenant("acme"));

        let families = accountant.collect();
        let names: Vec<&str> = families.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "autumn_cost_requests_total",
                "autumn_cost_cpu_seconds_total",
                "autumn_cost_allocated_bytes_total",
                "autumn_cost_db_queries_total",
            ]
        );
        let cpu = &families[1];
        assert_eq!(cpu.kind, MetricKind::Counter);
        assert_eq!(
            cpu.samples[0].labels,
            vec![("tenant".to_owned(), "acme".to_owned())]
        );
        assert!((cpu.samples[0].value - 2.0).abs() < f64::EPSILON);
        assert!((families[3].samples[0].value - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn signal_is_high_only_above_threshold() {
        let signal = CostSignal::new(Some(400.0));
        assert!(!signal.is_high());
        signal.set(400.0);
        assert!(!signal.is_high(), "equal to the threshold is not high");
        signal.set(400.5);
        assert!(signal.is_high());
        signal.set(10.0);
        assert!(!signal.is_high());
    }

    /// Until the first runtime-config value arrives, a signal with a threshold
    /// holds deferrable work (#1720).
    #[test]
    fn a_pending_signal_holds_work_until_the_first_value() {
        let signal = CostSignal::new(Some(1.0));
        signal.mark_pending();
        assert!(signal.is_high(), "a store can hold a high value at boot");
        assert!(signal.snapshot().pending);
        signal.set(0.5);
        assert!(!signal.is_high());
        assert!(!signal.is_pending());

        let no_threshold = CostSignal::new(None);
        no_threshold.mark_pending();
        assert!(!no_threshold.is_high(), "without a threshold nothing waits");
    }

    #[test]
    fn signal_without_threshold_is_never_high() {
        let signal = CostSignal::new(None);
        signal.set(1.0e9);
        assert!(!signal.is_high());
        assert_eq!(signal.threshold(), None);
    }

    #[test]
    fn signal_ignores_values_that_are_not_finite() {
        let signal = CostSignal::new(Some(1.0));
        signal.set(5.0);
        signal.set(f64::NAN);
        signal.set(f64::INFINITY);
        assert!((signal.value() - 5.0).abs() < f64::EPSILON);
        signal.set_threshold(Some(f64::NAN));
        assert_eq!(signal.threshold(), None);
    }

    #[test]
    fn signal_clones_share_state() {
        let signal = CostSignal::new(Some(1.0));
        let clone = signal.clone();
        clone.set(2.0);
        clone.set_threshold(Some(3.0));
        assert!((signal.value() - 2.0).abs() < f64::EPSILON);
        assert_eq!(signal.threshold(), Some(3.0));
        let snapshot = signal.snapshot();
        assert!(!snapshot.high);
    }

    #[test]
    fn signal_syncs_from_runtime_config() {
        use crate::runtime_config::{
            COST_SIGNAL_KEY, ConfigRegistry, InMemoryConfigStore, RuntimeConfigService,
        };
        let mut registry = ConfigRegistry::new();
        registry.define_cost_signal().expect("define cost keys");
        let svc =
            RuntimeConfigService::new(Arc::new(registry), Arc::new(InMemoryConfigStore::new()));

        let signal = CostSignal::new(Some(300.0));
        signal.sync_from(&svc).expect("default value syncs");
        assert!(!signal.is_high());

        svc.set(COST_SIGNAL_KEY, "512.5", Some("ops"))
            .expect("set signal");
        signal.sync_from(&svc).expect("override syncs");
        assert!((signal.value() - 512.5).abs() < f64::EPSILON);
        assert!(signal.is_high());
    }

    #[test]
    fn deferrable_marks_are_kept_by_kind() {
        mark_deferrable(WorkKind::Job, "cost_unit_test_job");
        assert!(is_deferrable(WorkKind::Job, "cost_unit_test_job"));
        assert!(!is_deferrable(WorkKind::Task, "cost_unit_test_job"));
        assert!(!is_deferrable(WorkKind::Job, "cost_unit_test_other"));
    }
}
