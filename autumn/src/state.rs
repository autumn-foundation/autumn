//! Shared application state.
//!
//! This module defines [`AppState`], the core state object passed to all
//! Axum route handlers. It contains framework-managed resources like the
//! database connection pool, metrics collector, and WebSocket channels.
//!
//! Handlers typically don't extract `AppState` directly. Instead, they use
//! specialized extractors like [`Db`](crate::Db) which pull what they need
//! from the state. However, custom extractors can access the state via
//! `crate::extract::State<AppState>`.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use crate::cache::Cache;
use crate::time::{ClockSource, MonotonicInstant, SystemClock};

/// Newtype wrapper used to store the global cache in the extension map so that
/// `set_cache` (called from startup hooks) is visible to all `AppState` clones.
pub struct GlobalCacheEntry(pub Arc<dyn Cache>);

use crate::actuator;
use crate::authorization::{ForbiddenResponse, Policy, PolicyRegistry, Scope};
#[cfg(feature = "ws")]
use crate::channels::Channels;
#[cfg(feature = "db")]
use crate::db::DbState;
use crate::middleware;
#[cfg(feature = "presence")]
use crate::presence::Presence;
use crate::probe;
#[cfg(feature = "ws")]
use tokio_util::sync::CancellationToken;

/// Shared application state passed to all route handlers.
///
/// Holds framework-managed resources such as the database connection pool.
/// Axum requires handler state to be [`Clone`], so internal resources use
/// `Arc` or are already cheaply cloneable (`deadpool::Pool` is `Arc`-wrapped
/// internally).
///
/// This struct is normally constructed by [`crate::app::AppBuilder::run`] and
/// should not need to be created manually. It is public so that custom
/// Axum extractors can access framework resources via
/// `State<AppState>`.
///
/// # Examples
///
/// ```rust
/// use autumn_web::AppState;
///
/// // State without a database (e.g., for testing)
/// let state = AppState::for_test().with_profile("dev");
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    /// Runtime-managed typed extensions installed by integrations after the app
    /// state has been constructed.
    pub(crate) extensions: Arc<std::sync::RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,

    /// Primary/write database connection pool, or `None` when no
    /// `database.primary_url` or legacy `database.url` is configured.
    #[cfg(feature = "db")]
    pub(crate) pool:
        Option<diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>,

    /// Read-replica connection pool, or `None` when no replica role is configured.
    #[cfg(feature = "db")]
    pub(crate) replica_pool:
        Option<diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>,

    /// Configured shard set, or `None` when no `[[database.shards]]`
    /// entries exist. The `pool`/`replica_pool` roles above are the
    /// control topology; tenant data routes across these shards.
    #[cfg(feature = "db")]
    pub(crate) shards: Option<crate::sharding::ShardSet>,

    /// Why failure-capsule capture cannot record this app's database traffic,
    /// copied from [`DatabaseTopology::capture_gap`](crate::db::DatabaseTopology::capture_gap)
    /// when the pools were built. Per-app by construction: another app in the
    /// same process carries its own state and its own gap.
    #[cfg(all(feature = "db", feature = "reporting"))]
    pub(crate) db_capture_gap: Option<Arc<str>>,

    /// Active profile name (e.g., "dev", "prod", "staging").
    ///
    /// `Arc<str>` rather than `String`: `AppState` is cloned once per tower
    /// ingress traversal (`Route::call` deep-clones the boxed service beneath
    /// it, per #2193/#2198), so an owned `String` here would be deep-copied
    /// on every one of those clones rather than once per request.
    pub(crate) profile: Option<Arc<str>>,

    /// Resolved process role for this replica, after config parsing and the
    /// `AUTUMN_ROLE` env override. This is the same value the framework uses to
    /// gate the job runtime, scheduler, and commit-hook worker, exposed here as
    /// a first-class accessor ([`role`](Self::role)) so `on_startup`/`on_shutdown`
    /// hooks, plugins, and handlers can self-gate app-owned background work
    /// without re-reading `AUTUMN_ROLE` by hand.
    pub(crate) role: crate::config::ProcessRole,

    /// When the application started, on the injected clock's monotonic
    /// timeline. Used for uptime calculation.
    ///
    /// Read through [`crate::time::ClockSource::monotonic`] rather than a raw
    /// [`std::time::Instant`] so uptime is virtual (and reproducible) under a
    /// [`#[sim_test]`](crate::sim_test). Re-stamped by
    /// [`with_clock`](Self::with_clock) so a clock installed after construction
    /// owns the origin uptime is measured from.
    pub(crate) started_at: MonotonicInstant,

    /// Whether the health endpoint should include detailed info.
    pub(crate) health_detailed: bool,

    /// Probe lifecycle state for liveness, readiness, and startup endpoints.
    pub(crate) probes: probe::ProbeState,

    /// In-memory metrics collector for the `/actuator/metrics` endpoint.
    pub(crate) metrics: middleware::MetricsCollector,

    /// Runtime log level state for the `/actuator/loggers` endpoint.
    pub(crate) log_levels: actuator::LogLevels,

    /// Scheduled task registry for the `/actuator/tasks` endpoint.
    pub(crate) task_registry: actuator::TaskRegistry,
    /// Job registry for the `/actuator/jobs` endpoint.
    pub(crate) job_registry: actuator::JobRegistry,

    /// Resolved config properties with source tracking for `/actuator/configprops`.
    pub(crate) config_props: actuator::ConfigProperties,

    /// Registry of plugin-contributed metrics sources, populated by
    /// [`crate::app::AppBuilder::metrics_source`].
    pub(crate) metrics_source_registry: actuator::MetricsSourceRegistry,

    /// Registry of custom health indicators, populated by
    /// [`crate::app::AppBuilder::health_indicator`].
    pub(crate) health_indicator_registry: actuator::HealthIndicatorRegistry,

    /// Named broadcast channel registry for real-time messaging.
    ///
    /// Available when the `ws` feature is enabled. Use
    /// [`channels()`](Self::channels) for convenient access.
    #[cfg(feature = "ws")]
    pub(crate) channels: Channels,

    /// Distributed presence tracker layered on top of [`Channels`].
    ///
    /// Available when the `presence` feature is enabled. Use
    /// [`presence()`](Self::presence) for convenient access.
    #[cfg(feature = "presence")]
    pub(crate) presence: Presence,

    /// Cancellation token signalled during graceful shutdown.
    ///
    /// WebSocket handlers receive a child token so they can clean up
    /// when the server is stopping.
    #[cfg(feature = "ws")]
    pub(crate) shutdown: CancellationToken,

    /// Per-resource policy + scope registry used by `#[authorize]`
    /// and `#[repository(policy = ...)]`-generated handlers.
    pub(crate) policy_registry: PolicyRegistry,

    /// HTTP status returned when a [`Policy`] denies a record-level
    /// action. Defaults to `404 Not Found` to mirror Rails / Phoenix
    /// posture and avoid leaking record existence.
    pub(crate) forbidden_response: ForbiddenResponse,

    /// Session key the `#[authorize]` machinery reads to resolve the
    /// authenticated user id for the
    /// [`PolicyContext`](crate::authorization::PolicyContext).
    /// Mirrors `[auth] session_key` (default: `"user_id"`).
    ///
    /// `Arc<str>` for the same reason as [`Self::profile`]: shared across the
    /// per-traversal clones instead of deep-copied by each one.
    pub(crate) auth_session_key: Arc<str>,

    /// Shared application cache backend. `None` means no global cache has been
    /// registered; `#[cached]` will fall back to its per-function Moka store.
    pub(crate) shared_cache: Option<Arc<dyn Cache>>,

    /// Injected wall-clock. Defaults to [`SystemClock`] (real time).
    /// Tests override via [`crate::test::TestApp::with_clock`].
    pub(crate) clock: Arc<dyn ClockSource>,

    /// Injected entropy source. Defaults to [`crate::entropy::OsEntropy`] (real
    /// OS randomness). Simulation tests override via [`Self::with_entropy`] with
    /// a [`crate::entropy::SeededEntropy`] so framework-minted identifiers
    /// replay byte-for-byte under a fixed seed. Read through the
    /// [`crate::entropy::Rng`] extractor in handlers.
    pub(crate) entropy: Arc<dyn crate::entropy::Entropy>,

    /// Process-unique identity assigned once per real `AppState` construction
    /// and preserved verbatim across `.clone()` (it is `Copy`).
    ///
    /// Two independently built apps that happen to share identical rate-limit
    /// config would otherwise collide in the process-global `#[throttle]`
    /// limiter registry (keyed only by route/name + config fingerprint), so
    /// traffic in one app would drain the other's per-route bucket. Folding
    /// this id into the registry key gives each app its own buckets. Sourced
    /// from a monotonic `AtomicU64` — never reused, unlike a pointer address.
    pub(crate) app_id: u64,
}

/// Monotonic source for [`AppState::app_id`]. Starts at 1 so `0` can never be a
/// live app id.
static NEXT_APP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Process-wide default config backing [`AppState::config_arc`]'s no-extension
/// fallback, so that path clones a refcount instead of building a fresh
/// `AutumnConfig` on every call.
///
/// Sharing one value across every config-less `AppState` is only sound because
/// `AutumnConfig` and its whole section tree are plain data: no `Mutex`,
/// `RwLock`, `Cell`, `OnceLock` or atomic anywhere in it, so there is no state
/// one app could mutate and another observe. Introducing interior mutability
/// into any config section invalidates that and this static must go back to a
/// per-call `Arc::new`.
static DEFAULT_CONFIG: std::sync::OnceLock<Arc<crate::config::AutumnConfig>> =
    std::sync::OnceLock::new();

/// Handle to the shared default config, built on first use.
fn default_config() -> &'static Arc<crate::config::AutumnConfig> {
    DEFAULT_CONFIG.get_or_init(|| Arc::new(crate::config::AutumnConfig::default()))
}

impl crate::authorization::ProvideAuthorizationState for AppState {
    fn policy_registry(&self) -> &crate::authorization::PolicyRegistry {
        &self.policy_registry
    }

    fn auth_session_key(&self) -> &str {
        &self.auth_session_key
    }

    fn forbidden_response(&self) -> &crate::authorization::ForbiddenResponse {
        &self.forbidden_response
    }

    #[cfg(feature = "db")]
    fn pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.pool.as_ref()
    }
}

impl AppState {
    /// Install or replace a typed runtime extension.
    ///
    /// Integrations use this to publish typed runtime resources, such as
    /// background-worker handles or dedicated storage pools, after startup.
    ///
    /// # Panics
    ///
    /// Panics if the internal extension map mutex is poisoned.
    pub fn insert_extension<T>(&self, value: T)
    where
        T: Any + Send + Sync + 'static,
    {
        self.extensions
            .write()
            .expect("app state extension lock poisoned")
            .insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Borrow a typed runtime extension if it has been installed.
    ///
    /// The returned [`Arc`] is cloned out of the internal registry so callers
    /// do not hold the state mutex while using the value.
    ///
    /// # Panics
    ///
    /// Panics if the internal extension map mutex is poisoned.
    #[must_use]
    pub fn extension<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        self.extensions
            .read()
            .expect("app state extension lock poisoned")
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| Arc::downcast::<T>(value).ok())
    }

    /// Borrow the app's designated live-state block, if one was registered
    /// with [`AppBuilder::with_live_state`](crate::app::AppBuilder::with_live_state).
    ///
    /// This is the block an in-place upgrade carries into the next build; see
    /// [`upgrade`](crate::upgrade).
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use autumn_web::AppState;
    /// # use autumn_web::upgrade::LiveState;
    /// # use serde::{Deserialize, Serialize};
    /// # #[derive(Serialize, Deserialize)] struct Stats { hits: u64 }
    /// # impl LiveState for Stats { const VERSION: u32 = 1; }
    /// # fn handler(state: &AppState) {
    /// if let Some(stats) = state.live_state::<Stats>() {
    ///     let hits = stats.read(|s| s.hits);
    ///     let _ = hits;
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn live_state<T>(&self) -> Option<crate::upgrade::LiveStateHandle<T>>
    where
        T: crate::upgrade::LiveState,
    {
        // The handle is itself an `Arc` over the state, so hand back a clone of
        // it rather than an `Arc<Arc<…>>` the caller has to keep alive.
        self.extension::<crate::upgrade::LiveStateHandle<T>>()
            .map(|handle| (*handle).clone())
    }

    /// Fetch the extension of type `T`, inserting `f()`'s result if absent.
    /// Atomic get-or-insert under the write lock: concurrent callers share one
    /// value. Used to lazily register process-wide registries.
    ///
    /// # Panics
    ///
    /// Panics if the internal extension map mutex is poisoned.
    pub fn extension_or_insert_with<T>(&self, f: impl FnOnce() -> T) -> Arc<T>
    where
        T: Any + Send + Sync + 'static,
    {
        if let Some(existing) = self.extension::<T>() {
            return existing;
        }
        let mut map = self
            .extensions
            .write()
            .expect("app state extension lock poisoned");
        if let Some(existing) = map
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| Arc::downcast::<T>(value).ok())
        {
            return existing;
        }
        let arc = Arc::new(f());
        map.insert(TypeId::of::<T>(), arc.clone() as Arc<dyn Any + Send + Sync>);
        arc
    }

    /// Returns the registered error reporters, if any were installed via
    /// [`AppBuilder::with_error_reporter`](crate::app::AppBuilder::with_error_reporter).
    ///
    /// Returns an empty `Vec` when none are registered; the
    /// [`ReportingLayer`](crate::reporting::ReportingLayer) then falls back to
    /// the built-in [`LogReporter`](crate::reporting::LogReporter).
    #[cfg(feature = "reporting")]
    #[must_use]
    pub(crate) fn error_reporters(
        &self,
    ) -> Vec<std::sync::Arc<dyn crate::reporting::ErrorReporter>> {
        self.extension::<crate::reporting::RegisteredReporters>()
            .map(|reporters| reporters.0.clone())
            .unwrap_or_default()
    }

    /// Returns the database connection pool.
    #[cfg(feature = "db")]
    #[must_use]
    pub const fn pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.pool.as_ref()
    }

    /// Returns the read-replica database connection pool, if configured.
    #[cfg(feature = "db")]
    #[must_use]
    pub const fn replica_pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.replica_pool.as_ref()
    }

    /// Returns the configured shard set, when `[[database.shards]]`
    /// entries exist.
    ///
    /// The control roles ([`pool`](Self::pool)/[`replica_pool`](Self::replica_pool))
    /// are unaffected by sharding; framework state lives there.
    #[cfg(feature = "db")]
    #[must_use]
    pub const fn shards(&self) -> Option<&crate::sharding::ShardSet> {
        self.shards.as_ref()
    }

    /// Returns the pool used for read-only work.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn read_pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        if self.replica_pool.is_some() && self.probes.should_route_reads_to_replica() {
            self.replica_pool.as_ref()
        } else if self.replica_pool.is_some() && self.probes.should_fallback_reads_to_primary() {
            self.pool.as_ref()
        } else if self.replica_pool.is_some() {
            None
        } else {
            self.pool.as_ref()
        }
    }

    /// Returns the metrics collector.
    #[must_use]
    pub const fn metrics(&self) -> &middleware::MetricsCollector {
        &self.metrics
    }

    /// Returns the log levels configuration.
    #[must_use]
    pub const fn log_levels(&self) -> &actuator::LogLevels {
        &self.log_levels
    }

    /// Returns the task registry.
    #[must_use]
    pub const fn task_registry(&self) -> &actuator::TaskRegistry {
        &self.task_registry
    }

    /// Returns the job registry.
    #[must_use]
    pub const fn job_registry(&self) -> &actuator::JobRegistry {
        &self.job_registry
    }

    /// Returns the config properties.
    #[must_use]
    pub const fn config_props(&self) -> &actuator::ConfigProperties {
        &self.config_props
    }

    /// Returns the registry of plugin-contributed metrics sources.
    #[must_use]
    pub const fn metrics_source_registry(&self) -> &actuator::MetricsSourceRegistry {
        &self.metrics_source_registry
    }

    /// Returns the registry of custom health indicators.
    #[must_use]
    pub const fn health_indicator_registry(&self) -> &actuator::HealthIndicatorRegistry {
        &self.health_indicator_registry
    }

    /// Returns the resolved [`crate::config::AutumnConfig`] from the extension map.
    ///
    /// Falls back to a default config if no config has been installed
    /// (typically only in tests that don't wire the full startup pipeline).
    ///
    /// This hands back an owned, independently mutable snapshot, which costs a
    /// deep clone of every config section; on request paths use
    /// [`config_arc`](Self::config_arc) instead.
    ///
    /// # Panics
    ///
    /// Panics if the internal extension map mutex is poisoned, inherited from
    /// [`extension`](Self::extension).
    #[must_use]
    pub fn config(&self) -> crate::config::AutumnConfig {
        (*self.config_arc()).clone()
    }

    /// Returns the resolved [`crate::config::AutumnConfig`] as a shared handle.
    ///
    /// The cheap accessor: it clones only the [`Arc`], never the
    /// configuration behind it, so callers pay a refcount bump instead of a
    /// deep copy of every config section. Prefer this over
    /// [`config`](Self::config) on request paths, and reach for `config()`
    /// only when an owned, independently mutable snapshot is genuinely needed.
    ///
    /// When no config extension has been installed (typically only in tests
    /// that don't wire the full startup pipeline) this yields a handle to a
    /// shared default-valued config, so the fallback is free too. That fallback
    /// is never written back into the extension map, so a config installed
    /// afterwards is still observed.
    ///
    /// # Panics
    ///
    /// Panics if the internal extension map mutex is poisoned, inherited from
    /// [`extension`](Self::extension).
    #[must_use]
    pub fn config_arc(&self) -> Arc<crate::config::AutumnConfig> {
        self.extension::<crate::config::AutumnConfig>()
            .unwrap_or_else(|| Arc::clone(default_config()))
    }

    /// Allocate the next process-unique app id.
    ///
    /// Called exactly once per genuine `AppState` construction; clones copy the
    /// resulting `u64` verbatim, so a cloned state (what `State<AppState>` hands
    /// a handler) reports the same id as its origin while a separately built
    /// state gets a fresh one.
    pub(crate) fn next_app_id() -> u64 {
        NEXT_APP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Process-unique identity of this app, stable across clones.
    ///
    /// Used to scope per-route `#[throttle]` limiters so two independently
    /// built apps with identical config never share a token bucket.
    #[must_use]
    pub(crate) const fn app_id(&self) -> u64 {
        self.app_id
    }

    /// Returns the shared probe lifecycle state.
    #[must_use]
    pub const fn probes(&self) -> &probe::ProbeState {
        &self.probes
    }

    /// Mark startup as complete so readiness can become healthy.
    pub fn mark_startup_complete(&self) {
        self.probes.mark_startup_complete();
    }

    /// Mark the application as draining so readiness flips unhealthy.
    pub fn begin_shutdown(&self) {
        self.probes.begin_shutdown();
    }

    /// Sets the database pool.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_pool(
        mut self,
        pool: diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
    ) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Sets the read-replica database pool.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_replica_pool(
        mut self,
        pool: diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
    ) -> Self {
        self.replica_pool = Some(pool);
        self
    }

    /// Sets the shard set.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_shards(mut self, shards: crate::sharding::ShardSet) -> Self {
        self.shards = Some(shards);
        self
    }

    /// Install a typed runtime extension while building test or ad-hoc state.
    #[must_use]
    pub fn with_extension<T>(self, value: T) -> Self
    where
        T: Any + Send + Sync + 'static,
    {
        self.insert_extension(value);
        self
    }

    /// Returns the registered global cache backend, if any.
    ///
    /// Checks the extension map first (populated at runtime by startup hooks
    /// via [`Self::set_cache`]) so that a plugin replacing a build-time backend
    /// is always visible. Falls back to `shared_cache` (set at build time via
    /// [`Self::with_cache`]).
    #[must_use]
    pub fn cache(&self) -> Option<Arc<dyn Cache>> {
        self.extension::<GlobalCacheEntry>()
            .map(|e| e.0.clone())
            .or_else(|| self.shared_cache.clone())
    }

    /// Register a global cache backend (builder / test helper, build-time).
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<dyn Cache>) -> Self {
        self.shared_cache = Some(cache);
        self
    }

    /// Returns the active clock source wired into this state.
    ///
    /// Handlers should prefer the [`crate::time::Clock`] extractor; this
    /// accessor exists for framework internals (middleware, storage) that
    /// need the time without going through Axum's extractor machinery.
    #[must_use]
    pub fn clock(&self) -> &dyn ClockSource {
        self.clock.as_ref()
    }

    /// Replace the clock (builder / test helper).
    ///
    /// Also re-stamps the app's start instant from the new clock, so
    /// [`uptime`](Self::uptime) is measured on the timeline that is actually
    /// installed. Without this, a state built with the default [`SystemClock`]
    /// and then handed a virtual clock would compare a *virtual* `now` against a
    /// *real* origin and report a nonsense uptime.
    ///
    /// A construction-time builder: it also gives the state a fresh job registry
    /// on `clock`, so a state's clock and the queue gauges judged by it can
    /// never belong to different timelines. A state cloned *before* this call
    /// keeps its own clock and its own registry, equally consistent — the two
    /// simply stop sharing queue gauges from here on.
    ///
    /// # Call this before starting a job runtime
    ///
    /// Once [`job::start_runtime`](mod@crate::job) has run, the runtime holds its
    /// own clone of the registry and keeps recording into it, and **no**
    /// behaviour here is correct:
    ///
    /// | if `with_clock` … | then |
    /// |---|---|
    /// | re-clocks this handle only | the runtime's clone judges shared marks on the old clock |
    /// | re-clocks a shared cell | the runtime stamps with the old clock into gauges moved to the new one |
    /// | takes a fresh registry (what it does) | the runtime keeps writing to the old one, and this state's gauges stay empty |
    ///
    /// The operation is simply not meaningful after initialization, so rather
    /// than pick the least-bad wrong answer this asserts in debug builds and
    /// logs at `error` in release. It is not silently tolerated.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        let already_initialized = self.job_registry.is_initialized();
        debug_assert!(
            !already_initialized,
            "AppState::with_clock is a construction-time builder: by the time job \
             names are registered a runtime holds its own clone of the registry, \
             and re-clocking the state can no longer reach it. Install the clock \
             before starting the job runtime."
        );
        if already_initialized {
            tracing::error!(
                "AppState::with_clock called after the job registry was populated; the \
                 job runtime holds its own clone and keeps recording there, so this \
                 state's actuator gauges will stay empty. Install the clock before \
                 starting the job runtime."
            );
        }
        self.started_at = clock.monotonic();
        // A registry's queue gauges compare ready-at marks against a clock, and
        // the marks come from the job runtime started off *this* state — so the
        // registry has to be the one this state's clock governs, and only that
        // one. Hence a fresh registry rather than re-clocking the existing one:
        //
        // * re-clocking only this handle leaves a clone's runtime stamping on
        //   the old clock while the shared gauges judge on the new one, and
        // * re-clocking through a shared cell inverts it — the *other* handle
        //   then stamps with its own clock into gauges moved out from under it.
        //
        // Detaching gives every state one clock and one registry that agree,
        // whichever order states are cloned and re-clocked in. Before a runtime
        // starts — the only point at which this builder is meaningful, checked
        // above — the registry is empty, so nothing is dropped.
        self.job_registry = actuator::JobRegistry::new().with_clock(Arc::clone(&clock));
        self.clock = clock;
        self
    }

    /// Returns the current instant on the injected clock's monotonic timeline —
    /// the deterministic replacement for [`std::time::Instant::now`] when
    /// measuring how long something took.
    ///
    /// Framework internals and app code alike should bracket work with two of
    /// these and take the difference via
    /// [`MonotonicInstant::saturating_duration_since`]. Handlers can reach the
    /// same value through the [`crate::time::Clock`] extractor's
    /// [`monotonic`](crate::time::Clock::monotonic).
    #[must_use]
    pub fn monotonic(&self) -> MonotonicInstant {
        self.clock.monotonic()
    }

    /// Clone the shared clock handle, e.g. to thread into a subsystem that needs
    /// the injected clock without going through Axum's extractor machinery.
    ///
    /// Mirrors [`Self::entropy_arc`].
    #[must_use]
    pub(crate) fn clock_arc(&self) -> Arc<dyn ClockSource> {
        Arc::clone(&self.clock)
    }

    /// Returns the active entropy source wired into this state.
    ///
    /// Handlers should prefer the [`crate::entropy::Rng`] extractor; this
    /// accessor exists for framework internals (id-minting subsystems) that need
    /// randomness without going through Axum's extractor machinery.
    #[must_use]
    pub fn entropy(&self) -> &dyn crate::entropy::Entropy {
        self.entropy.as_ref()
    }

    /// Clone the shared entropy handle, e.g. to thread into a subsystem or the
    /// [`crate::entropy::Rng`] extractor.
    #[must_use]
    pub(crate) fn entropy_arc(&self) -> Arc<dyn crate::entropy::Entropy> {
        self.entropy.clone()
    }

    /// Replace the entropy source (builder / simulation helper).
    ///
    /// Pass a [`crate::entropy::SeededEntropy`] to make every
    /// framework-minted identifier reproducible under a fixed seed. Mirrors
    /// [`Self::with_clock`].
    #[must_use]
    pub fn with_entropy(mut self, entropy: Arc<dyn crate::entropy::Entropy>) -> Self {
        self.entropy = entropy;
        self
    }

    /// Install or replace the global cache backend at runtime (e.g. from a startup hook).
    ///
    /// Updates both the process-level global (used by `#[cached]` functions) and
    /// the extension map (used by `CacheResponseLayer::from_app` and `state.cache()`).
    pub fn set_cache(&self, cache: Arc<dyn Cache>) {
        crate::cache::set_global_cache(cache.clone());
        self.insert_extension(GlobalCacheEntry(cache));
    }

    /// Sets the active profile.
    #[must_use]
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(Arc::from(profile.into()));
        self
    }

    /// Returns a reference to the [`PolicyRegistry`].
    #[must_use]
    pub const fn policy_registry(&self) -> &PolicyRegistry {
        &self.policy_registry
    }

    /// Resolve the registered [`Policy`] for resource `R`, if any.
    #[must_use]
    pub fn policy<R: Send + Sync + 'static>(&self) -> Option<std::sync::Arc<dyn Policy<R>>> {
        self.policy_registry.policy::<R>()
    }

    /// Resolve the registered [`Scope`] for resource `R`, if any.
    #[must_use]
    pub fn scope<R: Send + Sync + 'static>(&self) -> Option<std::sync::Arc<dyn Scope<R>>> {
        self.policy_registry.scope::<R>()
    }

    /// Configured deny-response shape. See
    /// [`ForbiddenResponse`] for the trade-off between `403` and
    /// `404` defaults.
    #[must_use]
    pub const fn forbidden_response(&self) -> ForbiddenResponse {
        self.forbidden_response
    }

    /// Session key used to resolve the authenticated user id for
    /// [`PolicyContext`](crate::authorization::PolicyContext).
    #[must_use]
    pub fn auth_session_key(&self) -> &str {
        &self.auth_session_key
    }

    /// Override the configured deny response (test helper).
    #[doc(hidden)]
    #[must_use]
    pub const fn with_forbidden_response(mut self, value: ForbiddenResponse) -> Self {
        self.forbidden_response = value;
        self
    }

    /// Override the auth session key (test helper).
    #[doc(hidden)]
    #[must_use]
    pub fn with_auth_session_key(mut self, value: impl Into<String>) -> Self {
        self.auth_session_key = Arc::from(value.into());
        self
    }

    /// Set the startup probe completion flag.
    #[doc(hidden)]
    #[must_use]
    pub fn with_startup_complete(self, startup_complete: bool) -> Self {
        self.probes.set_startup_complete(startup_complete);
        self
    }

    /// Set the readiness draining flag.
    #[doc(hidden)]
    #[must_use]
    pub fn with_draining(self, draining: bool) -> Self {
        self.probes.set_draining(draining);
        self
    }

    /// Returns the active profile name, or `"default"` if none is set.
    #[must_use]
    pub fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or("default")
    }

    /// Returns the resolved [`ProcessRole`](crate::config::ProcessRole) for this
    /// replica.
    ///
    /// This is the role after config parsing and the `AUTUMN_ROLE` env override
    /// — the exact same value the framework uses to gate the job runtime,
    /// scheduler, and commit-hook worker. Use it from `state_initializer`,
    /// `on_startup`/`on_shutdown` hooks, plugins, and request handlers to
    /// self-gate app-owned background work:
    ///
    /// ```rust
    /// # use autumn_web::AppState;
    /// # fn example(state: &AppState) {
    /// if state.role().runs_workers() {
    ///     // start an embedded worker loop only on replicas that run workers
    /// }
    /// # }
    /// ```
    ///
    /// [`serves_http`](crate::config::ProcessRole::serves_http) and
    /// [`runs_workers`](crate::config::ProcessRole::runs_workers) are reachable
    /// on the returned value.
    #[must_use]
    pub const fn role(&self) -> crate::config::ProcessRole {
        self.role
    }

    /// Returns how long the application has been running.
    ///
    /// Measured on the injected clock's monotonic timeline, so it is immune to
    /// wall-clock jumps in production and moves with
    /// [`Sim::advance`](crate::sim::Sim::advance) under a
    /// [`#[sim_test]`](crate::sim_test).
    #[must_use]
    pub fn uptime(&self) -> std::time::Duration {
        self.monotonic().saturating_duration_since(self.started_at)
    }

    /// Format uptime as a human-readable string (e.g., "2h 15m").
    #[must_use]
    pub fn uptime_display(&self) -> String {
        let secs = self.uptime().as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            let hours = secs / 3600;
            let mins = (secs % 3600) / 60;
            format!("{hours}h {mins}m")
        }
    }

    /// Returns a reference to the broadcast channel registry.
    ///
    /// Shorthand for accessing `self.channels` directly.
    #[cfg(feature = "ws")]
    #[must_use]
    pub const fn channels(&self) -> &Channels {
        &self.channels
    }

    /// Returns a reference to the distributed presence tracker.
    #[cfg(feature = "presence")]
    #[must_use]
    pub const fn presence(&self) -> &Presence {
        &self.presence
    }

    /// Returns a high-level broadcast facade for raw and htmx HTML payloads.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn broadcast(&self) -> crate::channels::Broadcast {
        self.channels.broadcast()
    }

    /// Returns a child cancellation token for the server shutdown signal.
    ///
    /// WebSocket handlers should select on this to clean up when the
    /// server is shutting down.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.child_token()
    }

    /// Helper for integration tests to simulate a server shutdown.
    #[cfg(feature = "ws")]
    #[doc(hidden)]
    pub fn trigger_shutdown_for_test(&self) {
        self.begin_shutdown();
        self.shutdown.cancel();
    }

    /// Update startup completion in tests after the router is already built.
    #[doc(hidden)]
    pub fn set_startup_complete_for_test(&self, startup_complete: bool) {
        self.probes.set_startup_complete(startup_complete);
    }

    /// Update draining state in tests after the router is already built.
    #[doc(hidden)]
    pub fn set_draining_for_test(&self, draining: bool) {
        self.probes.set_draining(draining);
    }

    /// Compatibility helper for tests that model shutdown as readiness drain.
    #[doc(hidden)]
    pub fn begin_shutdown_for_test(&self) {
        self.set_draining_for_test(true);
    }

    /// Create a minimal detached `AppState` without an HTTP server.
    ///
    /// This is useful for background runtimes or helper processes that still
    /// need framework-managed resources such as typed extensions, metrics, or
    /// WebSocket channel registries.
    #[must_use]
    pub fn detached() -> Self {
        #[cfg(feature = "ws")]
        let channels = Channels::new(32);
        Self {
            extensions: Arc::new(std::sync::RwLock::new(HashMap::new())),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            #[cfg(all(feature = "db", feature = "reporting"))]
            db_capture_gap: None,
            profile: None,
            role: crate::config::ProcessRole::Combined,
            started_at: crate::time::monotonic_now(),
            health_detailed: true,
            probes: probe::ProbeState::ready_for_test(),
            metrics: middleware::MetricsCollector::new(),
            log_levels: actuator::LogLevels::new("info"),
            task_registry: actuator::TaskRegistry::new(),
            job_registry: actuator::JobRegistry::new(),
            config_props: actuator::ConfigProperties::default(),
            metrics_source_registry: actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "presence")]
            presence: Presence::new(channels.clone()),
            #[cfg(feature = "ws")]
            channels,
            #[cfg(feature = "ws")]
            shutdown: CancellationToken::new(),
            policy_registry: PolicyRegistry::default(),
            forbidden_response: ForbiddenResponse::default(),
            auth_session_key: "user_id".into(),
            shared_cache: None,
            clock: Arc::new(SystemClock),
            entropy: std::sync::Arc::new(crate::entropy::OsEntropy),
            app_id: Self::next_app_id(),
        }
    }

    /// Create an `AppState` suitable for testing, with sensible defaults
    /// for all fields. Database pool is `None`.
    #[allow(dead_code)]
    #[must_use]
    pub fn for_test() -> Self {
        Self::detached()
    }
}

#[cfg(feature = "db")]
impl DbState for AppState {
    fn clock(&self) -> Arc<dyn ClockSource> {
        self.clock_arc()
    }

    fn metrics(&self) -> Option<&crate::middleware::MetricsCollector> {
        Some(&self.metrics)
    }

    fn pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.pool.as_ref()
    }

    fn replica_pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.replica_pool.as_ref()
    }

    #[cfg(feature = "reporting")]
    fn db_capture_gap(&self) -> Option<Arc<str>> {
        self.db_capture_gap.clone()
    }

    fn read_pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        Self::read_pool(self)
    }

    fn shards(&self) -> Option<&crate::sharding::ShardSet> {
        self.shards.as_ref()
    }

    fn db_interceptors(
        &self,
    ) -> Vec<std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>> {
        self.extension::<Arc<dyn crate::interceptor::DbConnectionInterceptor>>()
            .map(|arc| vec![(*arc).clone()])
            .unwrap_or_default()
    }
    fn statement_timeout(&self) -> Option<std::time::Duration> {
        self.extension::<crate::config::AutumnConfig>()
            .and_then(|cfg| cfg.database.statement_timeout)
    }

    fn slow_query_threshold(&self) -> std::time::Duration {
        self.extension::<crate::config::AutumnConfig>().map_or_else(
            || std::time::Duration::from_millis(500),
            |cfg| cfg.database.slow_query_threshold,
        )
    }
}

impl crate::probe::ProvideProbeState for AppState {
    fn probes(&self) -> &crate::probe::ProbeState {
        &self.probes
    }

    fn health_detailed(&self) -> bool {
        self.health_detailed
    }

    fn profile(&self) -> &str {
        self.profile()
    }

    fn uptime_display(&self) -> String {
        self.uptime_display()
    }

    #[cfg(feature = "db")]
    fn pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.pool.as_ref()
    }

    #[cfg(feature = "db")]
    fn replica_pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.replica_pool.as_ref()
    }

    fn health_indicator_registry(&self) -> Option<&crate::actuator::HealthIndicatorRegistry> {
        Some(&self.health_indicator_registry)
    }
}

impl crate::actuator::ProvideActuatorState for AppState {
    fn metrics(&self) -> &crate::middleware::MetricsCollector {
        &self.metrics
    }

    fn log_levels(&self) -> &crate::actuator::LogLevels {
        &self.log_levels
    }

    fn task_registry(&self) -> &crate::actuator::TaskRegistry {
        &self.task_registry
    }

    fn job_registry(&self) -> &crate::actuator::JobRegistry {
        &self.job_registry
    }

    fn config_props(&self) -> &crate::actuator::ConfigProperties {
        &self.config_props
    }

    fn profile(&self) -> &str {
        self.profile()
    }

    fn uptime_display(&self) -> String {
        self.uptime_display()
    }

    fn metrics_source_registry(&self) -> Option<&crate::actuator::MetricsSourceRegistry> {
        Some(&self.metrics_source_registry)
    }

    fn health_indicator_registry(&self) -> Option<&crate::actuator::HealthIndicatorRegistry> {
        Some(&self.health_indicator_registry)
    }

    fn health_detailed(&self) -> bool {
        self.health_detailed
    }

    /// The shadow-mirroring handle, installed into the runtime extension map
    /// when the framework router assembled a mirror layer for this app.
    ///
    /// Read from extensions rather than stored as an `AppState` field because
    /// the mirror is built during router assembly — after the state exists —
    /// and because an app that never enables `[shadow]` should carry no extra
    /// bytes for it.
    fn shadow(&self) -> Option<crate::shadow::ShadowHandle> {
        self.extension::<crate::shadow::ShadowHandle>()
            .map(|handle| (*handle).clone())
    }

    fn deploy_version(&self) -> String {
        self.extension::<crate::canary::CanaryState>().map_or_else(
            || crate::canary::STABLE.to_owned(),
            |c| c.version().to_owned(),
        )
    }

    #[cfg(feature = "ws")]
    fn channels(&self) -> &crate::channels::Channels {
        &self.channels
    }

    #[cfg(feature = "ws")]
    fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown_token()
    }

    #[cfg(feature = "db")]
    fn pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        self.pool.as_ref()
    }

    #[cfg(feature = "db")]
    fn shards(&self) -> Option<&crate::sharding::ShardSet> {
        self.shards.as_ref()
    }
    // a11y_posture() uses the trait default (all-false) intentionally: AppState
    // cannot know whether the application's layout is accessible.  Override this
    // method on your own state type — or in a custom ProvideActuatorState impl —
    // once you have verified that your pages include lang, a skip link, and
    // landmark regions.  See docs/guide/accessibility.md for details.

    #[cfg(feature = "http-client")]
    fn webhook_outbound(&self) -> Option<crate::webhook_outbound::WebhookOutboundManager> {
        self.extension::<crate::webhook_outbound::WebhookOutboundManager>()
            .map(|x| (*x).clone())
    }

    fn log_buffer(&self) -> Option<crate::log::capture::LogBuffer> {
        self.extension::<crate::log::capture::LogBuffer>()
            .map(|x| (*x).clone())
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("AppState");
        #[cfg(feature = "db")]
        s.field(
            "pool",
            &self
                .pool
                .as_ref()
                .map(|p| format!("Pool(max={})", p.status().max_size)),
        );
        s.field(
            "extensions",
            &self
                .extensions
                .read()
                .map_or(0, |extensions| extensions.len()),
        );
        s.field("profile", &self.profile)
            .field("started_at", &self.started_at)
            .field("health_detailed", &self.health_detailed)
            .field("probes", &self.probes)
            .field("metrics", &"MetricsCollector")
            .field("log_levels", &"LogLevels")
            .field("task_registry", &"TaskRegistry")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl AppState {
    /// Build a default `AppState` for unit tests.
    ///
    /// Every field here is the value duplicated verbatim across the crate's
    /// test helpers (`app::tests::test_router`, `router::tests::test_state`,
    /// `session::tests::test_state`, `auth::tests::test_app_state`, and
    /// several inline call sites) before they were consolidated onto this
    /// function. Callers that need one or two fields to differ do so with
    /// struct-update syntax (`AppState { field: ..., ..AppState::test_default() }`)
    /// rather than repeating the other twenty-odd fields.
    pub(crate) fn test_default() -> Self {
        Self {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            shards: None,
            #[cfg(all(feature = "db", feature = "reporting"))]
            db_capture_gap: None,
            profile: None,
            role: crate::config::ProcessRole::Combined,
            started_at: crate::time::monotonic_now(),
            health_detailed: false,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".into(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
            entropy: std::sync::Arc::new(crate::entropy::OsEntropy),
            app_id: Self::next_app_id(),
        }
    }
}

#[cfg(test)]
mod tests {
    /// Re-clocking a state that a runtime has already touched is refused loudly.
    ///
    /// By the time job names are registered, the runtime holds its own clone of
    /// the registry and keeps recording there — so no behaviour `with_clock`
    /// could pick is correct (see its docs for the three-way trade). It asserts
    /// in debug rather than silently leaving this state's gauges empty.
    #[test]
    #[should_panic(expected = "construction-time builder")]
    fn re_clocking_an_initialized_state_is_refused() {
        use chrono::{TimeZone, Utc};

        let state = AppState::detached();
        // What `job::start_runtime` does: register the app's job names.
        state.job_registry().register_on_queue("probe", "default");

        let epoch = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let _ = state.with_clock(Arc::new(crate::time::FixedClock::at(epoch)));
    }

    /// A state's clock and the gauges it judges must never come apart.
    ///
    /// The job runtime started off a state stamps ready-at marks with that
    /// state's clock, and the state's registry judges them. Re-clocking a state
    /// therefore has to move both together. Re-clocking only the handle leaves a
    /// clone's runtime stamping on the old clock while shared gauges judge on
    /// the new one; re-clocking through a shared cell inverts it, moving the
    /// gauges out from under the other handle's runtime. `with_clock` detaches
    /// instead, so each state keeps a matched pair.
    #[test]
    fn re_clocking_a_state_keeps_its_gauges_on_its_own_clock() {
        use chrono::{TimeZone, Utc};

        let real_state = AppState::detached();
        let cloned_before = real_state.clone();

        // Behind real time, like a sim epoch: the direction where a stale
        // judgement calls a not-yet-due job ready.
        let epoch = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let virtual_state = real_state.with_clock(Arc::new(crate::time::FixedClock::at(epoch)));

        // A job the virtual state's runtime would stamp: due a minute after its
        // epoch, so not yet ready *on that state's clock*.
        let ready_at = u64::try_from(epoch.timestamp_millis()).unwrap() + 60_000;
        virtual_state
            .job_registry()
            .register_on_queue("probe", "default");
        virtual_state
            .job_registry()
            .record_enqueue_scheduled("probe", ready_at);

        let virtual_depth = virtual_state.job_registry().queue_snapshot()["default"].depth;
        assert_eq!(
            virtual_depth, 0,
            "the state that stamped the mark must judge it as scheduled"
        );

        // The clone kept the real clock, so it must also have kept its own
        // registry — otherwise its runtime would stamp real-time deadlines into
        // gauges now judged against 2020, and every one of them would read as
        // scheduled for years.
        let clone_snapshot = cloned_before.job_registry().queue_snapshot();
        assert!(
            !clone_snapshot.contains_key("default"),
            "a state cloned before the clock swap must not share gauges with a \
             state on a different clock; it saw {clone_snapshot:?}"
        );
    }

    use super::*;
    #[cfg(feature = "db")]
    use crate::config;
    #[cfg(feature = "db")]
    use crate::db;

    #[test]
    fn app_state_debug_without_pool() {
        let state = AppState::for_test().with_profile("dev");
        let debug = format!("{state:?}");
        assert!(debug.contains("AppState"));
        assert!(debug.contains("dev"));
    }

    #[cfg(feature = "db")]
    #[test]
    fn app_state_debug_with_pool() {
        let config = config::DatabaseConfig {
            url: Some(crate::test_urls::primary("test")),
            pool_size: 5,
            ..Default::default()
        };
        let pool = db::create_pool(&config).unwrap().unwrap();
        let state = AppState::for_test().with_pool(pool);
        let debug = format!("{state:?}");
        assert!(debug.contains("Pool(max=5)"));
    }

    #[cfg(feature = "db")]
    #[test]
    fn database_topology_state_exposes_replica_as_read_pool() {
        let primary_config = config::DatabaseConfig {
            url: Some(crate::test_urls::primary("primary")),
            pool_size: 5,
            ..Default::default()
        };
        let replica_config = config::DatabaseConfig {
            url: Some(crate::test_urls::replica("primary", "replica")),
            pool_size: 2,
            ..Default::default()
        };
        let primary = db::create_pool(&primary_config).unwrap().unwrap();
        let replica = db::create_pool(&replica_config).unwrap().unwrap();

        let state = AppState::for_test()
            .with_pool(primary)
            .with_replica_pool(replica);

        assert_eq!(state.pool().expect("primary pool").status().max_size, 5);
        assert_eq!(
            state
                .replica_pool()
                .expect("replica pool")
                .status()
                .max_size,
            2
        );
        assert_eq!(state.read_pool().expect("read pool").status().max_size, 2);
    }

    #[cfg(feature = "db")]
    #[test]
    fn read_pool_uses_primary_when_replica_is_unready_and_policy_allows_fallback() {
        let primary_config = config::DatabaseConfig {
            url: Some(crate::test_urls::primary("primary")),
            pool_size: 5,
            ..Default::default()
        };
        let replica_config = config::DatabaseConfig {
            url: Some(crate::test_urls::replica("primary", "replica")),
            pool_size: 2,
            ..Default::default()
        };
        let primary = db::create_pool(&primary_config).unwrap().unwrap();
        let replica = db::create_pool(&replica_config).unwrap().unwrap();

        let state = AppState::for_test()
            .with_pool(primary)
            .with_replica_pool(replica);
        state
            .probes()
            .configure_replica_dependency(config::ReplicaFallback::Primary);
        state
            .probes()
            .mark_replica_unready("replica migrations lag primary");

        assert_eq!(state.read_pool().expect("read pool").status().max_size, 5);
        assert_eq!(
            db::DbState::read_pool(&state)
                .expect("trait read pool")
                .status()
                .max_size,
            5
        );
    }

    #[cfg(feature = "db")]
    #[test]
    fn read_pool_does_not_route_to_unready_replica_when_policy_fails_readiness() {
        let primary_config = config::DatabaseConfig {
            url: Some(crate::test_urls::primary("primary")),
            pool_size: 5,
            ..Default::default()
        };
        let replica_config = config::DatabaseConfig {
            url: Some(crate::test_urls::replica("primary", "replica")),
            pool_size: 2,
            ..Default::default()
        };
        let primary = db::create_pool(&primary_config).unwrap().unwrap();
        let replica = db::create_pool(&replica_config).unwrap().unwrap();

        let state = AppState::for_test()
            .with_pool(primary)
            .with_replica_pool(replica);
        state
            .probes()
            .configure_replica_dependency(config::ReplicaFallback::FailReadiness);
        state
            .probes()
            .mark_replica_unready("replica connection failed");

        assert!(state.read_pool().is_none());
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn readiness_fails_when_app_state_replica_is_unready_and_policy_is_fail_readiness() {
        let primary_config = config::DatabaseConfig {
            url: Some(crate::test_urls::primary("primary")),
            pool_size: 5,
            ..Default::default()
        };
        let replica_config = config::DatabaseConfig {
            url: Some(crate::test_urls::replica("primary", "replica")),
            pool_size: 2,
            ..Default::default()
        };
        let primary = db::create_pool(&primary_config).unwrap().unwrap();
        let replica = db::create_pool(&replica_config).unwrap().unwrap();

        let state = AppState::for_test()
            .with_pool(primary)
            .with_replica_pool(replica);
        state
            .probes()
            .configure_replica_dependency(config::ReplicaFallback::FailReadiness);
        state
            .probes()
            .mark_replica_unready("replica migrations lag primary");

        let (status, _) = crate::probe::readiness_response(&state).await;

        assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn detached_state_starts_without_profile() {
        let state = AppState::detached();

        assert_eq!(state.profile(), "default");
    }

    fn require_clone<T: Clone>(t: &T) -> T {
        t.clone()
    }

    #[test]
    fn app_state_is_clone() {
        let state = AppState::for_test();
        let _cloned = require_clone(&state);
    }

    #[test]
    fn app_state_profile_accessor() {
        let state = AppState::for_test().with_profile("staging");
        assert_eq!(state.profile(), "staging");
    }

    #[test]
    fn app_state_deploy_version_defaults_to_stable() {
        use crate::actuator::ProvideActuatorState;
        let state = AppState::for_test();
        assert_eq!(state.deploy_version(), crate::canary::STABLE);
    }

    #[test]
    fn app_state_deploy_version_reads_canary_extension() {
        use crate::actuator::ProvideActuatorState;
        let state = AppState::for_test();
        state.insert_extension(crate::canary::CanaryState::new(crate::canary::CANARY));
        assert_eq!(state.deploy_version(), crate::canary::CANARY);
    }

    #[test]
    fn app_state_profile_default() {
        let state = AppState::for_test();
        assert_eq!(state.profile(), "default");
    }

    #[test]
    fn app_state_uptime_display() {
        let state = AppState::for_test();
        let display = state.uptime_display();
        assert!(
            display.contains('s'),
            "uptime should contain 's': {display}"
        );
    }

    #[test]
    fn app_state_accessors() {
        let state = AppState::for_test();

        // Exercise the new getters to ensure they compile and return the expected types
        let _metrics = state.metrics();
        let _log_levels = state.log_levels();
        let _task_registry = state.task_registry();
        let _config_props = state.config_props();

        #[cfg(feature = "db")]
        {
            let _pool = state.pool();
        }
        let _missing = state.extension::<String>();
    }

    #[test]
    fn app_state_runtime_extensions_round_trip() {
        let state = AppState::for_test();
        state.insert_extension(String::from("haunted"));

        let stored = state
            .extension::<String>()
            .expect("runtime extension should be installed");

        assert_eq!(stored.as_str(), "haunted");
    }

    /// `config_arc` must hand back the very `Arc` the extension map holds, and
    /// must keep reading through to that map rather than caching a handle:
    /// `app::build` re-inserts a mutated config after `build_state` (static
    /// routes excluded from locale prefixing), so a cached handle would serve
    /// the pre-mutation config forever.
    #[test]
    fn config_arc_returns_the_installed_arc_without_deep_cloning() {
        let state = AppState::for_test();
        state.insert_extension(crate::config::AutumnConfig {
            profile: Some("staging".to_owned()),
            ..Default::default()
        });

        let installed = state
            .extension::<crate::config::AutumnConfig>()
            .expect("config extension should be installed");
        let first = state.config_arc();
        let second = state.config_arc();

        assert!(
            Arc::ptr_eq(&first, &installed),
            "config_arc must return the extension map's Arc, not a fresh allocation"
        );
        assert!(
            Arc::ptr_eq(&first, &second),
            "two config_arc calls must share one allocation"
        );

        state.insert_extension(crate::config::AutumnConfig {
            profile: Some("prod".to_owned()),
            ..Default::default()
        });
        let after_reinsert = state.config_arc();

        assert_eq!(
            after_reinsert.profile.as_deref(),
            Some("prod"),
            "config_arc must observe a config re-inserted after construction"
        );
        assert!(
            !Arc::ptr_eq(&first, &after_reinsert),
            "a re-inserted config must replace the handle, not alias the old one"
        );
    }

    #[test]
    fn config_arc_falls_back_to_default_without_extension() {
        let state = AppState::for_test();
        let defaults = crate::config::AutumnConfig::default();

        let fallback = state.config_arc();

        assert_eq!(fallback.profile, defaults.profile);
        assert_eq!(fallback.server.port, defaults.server.port);
        assert_eq!(fallback.server.host, defaults.server.host);
        // Reading config must not install one: a state that boots without a
        // config and gets one later must see the later one, and the fallback
        // must not become a phantom entry other `extension` callers observe.
        assert!(
            state.extension::<crate::config::AutumnConfig>().is_none(),
            "the fallback default must not be written into the extension map"
        );
    }

    /// `AutumnConfig` has no `PartialEq`, so `Debug` rendering stands in for
    /// value equality.
    #[test]
    fn config_matches_config_arc_by_value() {
        let absent = AppState::for_test();
        assert_eq!(
            format!("{:?}", absent.config()),
            format!("{:?}", *absent.config_arc()),
            "the two accessors must agree in the no-extension fallback case"
        );

        let present = AppState::for_test();
        present.insert_extension(crate::config::AutumnConfig {
            profile: Some("staging".to_owned()),
            ..Default::default()
        });
        assert_eq!(
            format!("{:?}", present.config()),
            format!("{:?}", *present.config_arc()),
            "the two accessors must agree when a config is installed"
        );
    }

    /// Both accessors' signatures are load-bearing beyond this crate: autumn-cli's
    /// auth generator emits reads against them as strings that CI never compiles,
    /// so a change here surfaces only in generated apps.
    ///
    /// `config_arc` is the one generated request handlers call — they bind the
    /// handle and borrow sections off it (`&config.auth.password`), so it has to
    /// keep returning an owned `Arc` that outlives the borrow. `config` stays
    /// pinned too: it remains the per-boot owned-snapshot accessor.
    #[test]
    fn config_signature_is_unchanged() {
        let _: fn(&AppState) -> crate::config::AutumnConfig = AppState::config;
        let _: fn(&AppState) -> Arc<crate::config::AutumnConfig> = AppState::config_arc;
    }
}
