//! Liveness, readiness, and startup probes.
//!
//! Autumn exposes explicit cloud-native probe contracts:
//! - liveness ignores startup and dependency state
//! - readiness reflects startup completion, shutdown draining, and core dependencies
//! - startup stays unavailable until startup hooks complete

use std::sync::Arc;
#[cfg(feature = "db")]
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::extract::State;
use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

/// Trait to abstract the state requirements for probe handlers.
///
/// Implement this trait on your application's state type to provide
/// the necessary dependencies for health/liveness probes.
/// This prevents tight coupling between probe handlers and the specific `AppState`.
pub trait ProvideProbeState {
    /// Returns a reference to the shared [`ProbeState`] that tracks
    /// lifecycle phases (startup, ready, draining).
    fn probes(&self) -> &ProbeState;

    /// Returns whether detailed health information (e.g., uptime, pool stats)
    /// should be included in the response.
    fn health_detailed(&self) -> bool;

    /// Returns the currently active execution profile (e.g. "dev", "prod").
    fn profile(&self) -> &str;

    /// Returns a human-readable string displaying how long the application
    /// has been running (e.g., "2d 4h 13m").
    fn uptime_display(&self) -> String;

    /// Returns an optional reference to the database connection pool,
    /// used to evaluate database connectivity during a readiness check.
    #[cfg(feature = "db")]
    fn pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>;

    /// Returns an optional read-replica pool for readiness checks.
    #[cfg(feature = "db")]
    fn replica_pool(
        &self,
    ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
    {
        None
    }

    /// Returns the registry of [`crate::actuator::HealthIndicator`] implementations.
    ///
    /// The default returns `None`. [`crate::AppState`] overrides this to return
    /// its registry so that `/ready` can run readiness-group indicators.
    fn health_indicator_registry(&self) -> Option<&crate::actuator::HealthIndicatorRegistry> {
        None
    }

    /// Helper method to mark the application startup as complete.
    ///
    /// Delegates to [`ProbeState::mark_startup_complete`].
    ///
    /// # Examples
    ///
    /// ```
    /// use autumn_web::probe::{ProvideProbeState, ProbeState};
    ///
    /// struct MyState { probes: ProbeState }
    /// impl ProvideProbeState for MyState {
    ///     fn probes(&self) -> &ProbeState { &self.probes }
    ///     fn health_detailed(&self) -> bool { false }
    ///     fn profile(&self) -> &str { "dev" }
    ///     fn uptime_display(&self) -> String { String::new() }
    ///     #[cfg(feature = "db")]
    ///     fn pool(&self) -> Option<&diesel_async::pooled_connection::deadpool::Pool<autumn_web::db::RuntimeConnection>> { None }
    /// }
    ///
    /// let state = MyState { probes: ProbeState::pending_startup() };
    /// assert!(!state.probes().is_startup_complete());
    /// state.mark_startup_complete();
    /// assert!(state.probes().is_startup_complete());
    /// ```
    fn mark_startup_complete(&self) {
        self.probes().mark_startup_complete();
    }
}

/// Shared probe lifecycle state stored in `AppState`.
///
/// Clones share all state, also the cached database pings and their
/// connections. A new `ProbeState` caches a ping result for 1 s and gives a
/// ping 2 s. Call [`Self::configure_db_check`] to change this.
#[derive(Clone, Debug, Default)]
pub struct ProbeState {
    startup_complete: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    #[cfg(feature = "db")]
    replica_dependency: Arc<RwLock<ReplicaDependency>>,
    #[cfg(feature = "db")]
    db_checks: Arc<DbChecks>,
}

/// Ping checks of the database roles, shared by all clones of a
/// [`ProbeState`].
#[cfg(feature = "db")]
#[derive(Debug)]
struct DbChecks {
    primary: DbPingCheck,
    replica: DbPingCheck,
    /// When `false`, a failed primary ping does not fail `/ready`.
    primary_gates_readiness: AtomicBool,
}

#[cfg(feature = "db")]
impl Default for DbChecks {
    fn default() -> Self {
        Self {
            primary: DbPingCheck::new("primary"),
            replica: DbPingCheck::new("replica"),
            primary_gates_readiness: AtomicBool::new(true),
        }
    }
}

#[cfg(feature = "db")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReplicaMigrationCheck {
    pub(crate) primary_url: String,
    pub(crate) replica_url: String,
}

#[cfg(feature = "db")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplicaDependency {
    configured: bool,
    fallback: crate::config::ReplicaFallback,
    connection_ready: bool,
    migrations_ready: bool,
    migration_check: Option<ReplicaMigrationCheck>,
    detail: Option<String>,
}

#[cfg(feature = "db")]
impl Default for ReplicaDependency {
    fn default() -> Self {
        Self {
            configured: false,
            fallback: crate::config::ReplicaFallback::default(),
            connection_ready: true,
            migrations_ready: true,
            migration_check: None,
            detail: None,
        }
    }
}

#[cfg(feature = "db")]
use crate::db_ping::{DbPingCheck, DbPingStatus, DbPool};

impl ProbeState {
    /// Create a probe state that starts in pending-startup mode.
    #[must_use]
    pub fn pending_startup() -> Self {
        Self::default()
    }

    /// Alias for pending startup used by application bootstrapping.
    #[must_use]
    pub fn starting() -> Self {
        Self::pending_startup()
    }

    /// Create a probe state that is immediately ready.
    #[must_use]
    pub fn ready_for_test() -> Self {
        let state = Self::pending_startup();
        state.mark_startup_complete();
        state
    }

    /// Mark startup as complete and readiness eligible.
    pub fn mark_startup_complete(&self) {
        self.startup_complete.store(true, Ordering::Relaxed);
    }

    /// Override startup completion for tests.
    pub fn set_startup_complete(&self, complete: bool) {
        self.startup_complete.store(complete, Ordering::Relaxed);
    }

    /// Mark the application as shutting down so readiness flips false.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
    }

    /// Alias for readiness drain used during graceful shutdown.
    pub fn begin_draining(&self) {
        self.begin_shutdown();
    }

    /// Override shutdown-draining state for tests.
    pub fn set_draining(&self, draining: bool) {
        self.shutting_down.store(draining, Ordering::Relaxed);
    }

    /// Configure runtime readiness behavior for a read replica.
    ///
    /// # Panics
    ///
    /// Panics if the replica dependency lock is poisoned.
    #[cfg(feature = "db")]
    pub fn configure_replica_dependency(&self, fallback: crate::config::ReplicaFallback) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        *dependency = ReplicaDependency {
            configured: true,
            fallback,
            connection_ready: false,
            migrations_ready: true,
            migration_check: None,
            detail: Some("replica has not passed a readiness check".to_owned()),
        };
    }

    /// Configure the database readiness pings (primary and read replica).
    ///
    /// `cache_ttl` is how long one ping result stays valid. Zero turns the
    /// cache off. `timeout` is the time limit for one ping. A ping that does
    /// not finish in time counts as `DOWN`.
    #[cfg(feature = "db")]
    pub fn configure_db_check(&self, cache_ttl: std::time::Duration, timeout: std::time::Duration) {
        self.db_checks.primary.configure(cache_ttl, timeout);
        self.db_checks.replica.configure(cache_ttl, timeout);
    }

    /// Set whether a failed primary database ping fails `/ready`. Default:
    /// `true`. When `false`, `/actuator/health` still reports the `db`
    /// component as `DOWN`.
    #[cfg(feature = "db")]
    pub fn set_db_readiness(&self, gate: bool) {
        self.db_checks
            .primary_gates_readiness
            .store(gate, Ordering::Relaxed);
    }

    #[cfg(feature = "db")]
    fn primary_gates_readiness(&self) -> bool {
        self.db_checks
            .primary_gates_readiness
            .load(Ordering::Relaxed)
    }

    /// A probe state whose primary ping is `ping`. For tests.
    #[cfg(all(test, feature = "db"))]
    pub(crate) fn with_primary_ping(ping: Arc<dyn crate::db_ping::DbPing>) -> Self {
        Self {
            db_checks: Arc::new(DbChecks {
                primary: DbPingCheck::with_ping("primary", ping),
                ..DbChecks::default()
            }),
            ..Self::default()
        }
    }

    /// A probe state whose primary and replica pings are fakes. For tests.
    #[cfg(all(test, feature = "db"))]
    pub(crate) fn with_db_pings(
        primary: Arc<dyn crate::db_ping::DbPing>,
        replica: Arc<dyn crate::db_ping::DbPing>,
    ) -> Self {
        Self {
            db_checks: Arc::new(DbChecks {
                primary: DbPingCheck::with_ping("primary", primary),
                replica: DbPingCheck::with_ping("replica", replica),
                ..DbChecks::default()
            }),
            ..Self::default()
        }
    }

    /// Ping the primary of `pool`, or return the cached result.
    #[cfg(feature = "db")]
    pub(crate) async fn check_primary_db(&self, pool: &DbPool) -> DbPingStatus {
        self.db_checks.primary.check(pool).await
    }

    /// Ping the primary of `pool` once, with no cache. For a state that has
    /// no [`ProbeState`]. Each call opens a new connection.
    #[cfg(feature = "db")]
    pub(crate) async fn check_primary_db_uncached(pool: &DbPool) -> DbPingStatus {
        let check = DbPingCheck::new("primary");
        check.configure(
            std::time::Duration::ZERO,
            crate::health_cache::DEFAULT_PING_TIMEOUT,
        );
        check.check(pool).await
    }

    /// Store URLs needed to retry replica migration readiness checks.
    #[cfg(feature = "db")]
    pub(crate) fn configure_replica_migration_check(
        &self,
        primary_url: impl Into<String>,
        replica_url: impl Into<String>,
    ) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        dependency.migration_check = Some(ReplicaMigrationCheck {
            primary_url: primary_url.into(),
            replica_url: replica_url.into(),
        });
    }

    /// Mark the configured read replica as reachable.
    ///
    /// # Panics
    ///
    /// Panics if the replica dependency lock is poisoned.
    #[cfg(feature = "db")]
    pub fn mark_replica_connection_ready(&self) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        dependency.connection_ready = true;
        if dependency.migrations_ready {
            dependency.detail = None;
        }
    }

    /// Mark the configured read replica as unreachable.
    ///
    /// # Panics
    ///
    /// Panics if the replica dependency lock is poisoned.
    #[cfg(feature = "db")]
    pub fn mark_replica_connection_unready(&self, detail: impl Into<String>) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        dependency.connection_ready = false;
        dependency.detail = Some(detail.into());
    }

    /// Mark the configured read replica's migration state as current.
    ///
    /// # Panics
    ///
    /// Panics if the replica dependency lock is poisoned.
    #[cfg(feature = "db")]
    pub fn mark_replica_migrations_ready(&self) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        dependency.migrations_ready = true;
        if dependency.connection_ready {
            dependency.detail = None;
        }
    }

    /// Mark the configured read replica's migration state as stale.
    #[cfg(feature = "db")]
    pub(crate) fn mark_replica_migrations_unready(&self, detail: impl Into<String>) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        dependency.migrations_ready = false;
        dependency.detail = Some(detail.into());
    }

    /// Mark the configured read replica as ready.
    ///
    /// # Panics
    ///
    /// Panics if the replica dependency lock is poisoned.
    #[cfg(feature = "db")]
    pub fn mark_replica_ready(&self) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        dependency.connection_ready = true;
        dependency.migrations_ready = true;
        dependency.detail = None;
    }

    /// Mark the configured read replica as unavailable or stale.
    ///
    /// # Panics
    ///
    /// Panics if the replica dependency lock is poisoned.
    #[cfg(feature = "db")]
    pub fn mark_replica_unready(&self, detail: impl Into<String>) {
        let mut dependency = self
            .replica_dependency
            .write()
            .expect("replica dependency lock poisoned");
        dependency.connection_ready = false;
        dependency.migrations_ready = false;
        dependency.detail = Some(detail.into());
    }

    /// Returns whether startup completed successfully.
    #[must_use]
    pub fn is_startup_complete(&self) -> bool {
        self.startup_complete.load(Ordering::Relaxed)
    }

    /// Returns whether graceful shutdown has started.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// Returns whether readiness is currently draining.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.is_shutting_down()
    }

    #[cfg(feature = "db")]
    pub(crate) fn replica_allows_readiness(&self) -> bool {
        let dependency = self
            .replica_dependency
            .read()
            .expect("replica dependency lock poisoned");
        let ready = dependency.connection_ready && dependency.migrations_ready;
        !dependency.configured
            || ready
            || matches!(dependency.fallback, crate::config::ReplicaFallback::Primary)
    }

    #[cfg(feature = "db")]
    pub(crate) fn should_route_reads_to_replica(&self) -> bool {
        let dependency = self
            .replica_dependency
            .read()
            .expect("replica dependency lock poisoned");
        !dependency.configured || (dependency.connection_ready && dependency.migrations_ready)
    }

    #[cfg(feature = "db")]
    pub(crate) fn should_fallback_reads_to_primary(&self) -> bool {
        let dependency = self
            .replica_dependency
            .read()
            .expect("replica dependency lock poisoned");
        dependency.configured
            && !(dependency.connection_ready && dependency.migrations_ready)
            && matches!(dependency.fallback, crate::config::ReplicaFallback::Primary)
    }

    #[cfg(feature = "db")]
    pub(crate) fn replica_configured(&self) -> bool {
        self.replica_dependency
            .read()
            .expect("replica dependency lock poisoned")
            .configured
    }

    #[cfg(feature = "db")]
    pub(crate) fn replica_migration_check(&self) -> Option<ReplicaMigrationCheck> {
        self.replica_dependency
            .read()
            .expect("replica dependency lock poisoned")
            .migration_check
            .clone()
    }
}

#[derive(Clone, Copy)]
enum ProbeKind {
    Live,
    Ready,
    Startup,
}

#[derive(Serialize)]
pub(crate) struct ProbeResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pool: Option<PoolStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    database: Option<DatabaseStatus>,
}

/// Primary database ping result in a detailed `/ready` body.
#[derive(Serialize)]
pub(crate) struct DatabaseStatus {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PoolStatus {
    size: u64,
    available: u64,
    waiting: u64,
}

/// Replica policy readiness and pool statistics.
///
/// Pool saturation does not affect readiness: a busy replica can still serve
/// requests. To shed load, set `server.max_concurrent_requests`.
#[allow(clippy::missing_const_for_fn, unused_variables)]
fn dependency_readiness<S: ProvideProbeState>(state: &S) -> (bool, Option<PoolStatus>) {
    #[cfg(feature = "db")]
    {
        let replica_ready_for_policy = state.probes().replica_allows_readiness();
        let pool_status = state.pool().map(|pool| {
            let status = pool.status();
            PoolStatus {
                size: status.max_size as u64,
                available: status.available as u64,
                waiting: status.waiting as u64,
            }
        });

        (replica_ready_for_policy, pool_status)
    }

    #[cfg(not(feature = "db"))]
    {
        (true, None)
    }
}

#[cfg(feature = "db")]
async fn refresh_replica_readiness<S: ProvideProbeState + Sync>(state: &S) {
    if !state.probes().replica_configured() {
        return;
    }

    let Some(replica_pool) = state.replica_pool() else {
        state
            .probes()
            .mark_replica_connection_unready("replica pool is not available");
        return;
    };

    // A cached ping on a dedicated connection. A busy replica pool does not
    // make the replica unready (#3059).
    let status = state.probes().db_checks.replica.check(replica_pool).await;
    if status.up {
        state.probes().mark_replica_connection_ready();
        refresh_replica_migration_readiness(state).await;
    } else {
        state.probes().mark_replica_connection_unready(
            status
                .error
                .unwrap_or_else(|| "replica ping failed".to_owned()),
        );
    }
}

#[cfg(feature = "db")]
async fn refresh_replica_migration_readiness<S: ProvideProbeState + Sync>(state: &S) {
    refresh_replica_migration_readiness_with(state, |check| {
        crate::migrate::check_replica_migration_readiness_blocking(
            check.primary_url,
            check.replica_url,
        )
    })
    .await;
}

#[cfg(feature = "db")]
async fn refresh_replica_migration_readiness_with<S, F, Fut>(state: &S, check_readiness: F)
where
    S: ProvideProbeState + Sync,
    F: FnOnce(ReplicaMigrationCheck) -> Fut,
    Fut: std::future::Future<Output = crate::migrate::ReplicaMigrationReadiness>,
{
    let Some(check) = state.probes().replica_migration_check() else {
        return;
    };

    let readiness = check_readiness(check).await;

    if readiness.is_ready() {
        state.probes().mark_replica_migrations_ready();
    } else if let Some(detail) = readiness.detail() {
        state.probes().mark_replica_migrations_unready(detail);
    }
}

fn probe_response<S: ProvideProbeState>(
    state: &S,
    kind: ProbeKind,
    checks_ready: bool,
) -> (StatusCode, Json<ProbeResponse>) {
    let startup_complete = state.probes().is_startup_complete();
    let shutting_down = state.probes().is_shutting_down();
    let (dependencies_ready, pool_status) = dependency_readiness(state);

    let (status_code, status) = match kind {
        ProbeKind::Live => (StatusCode::OK, "ok"),
        ProbeKind::Startup if startup_complete => (StatusCode::OK, "ok"),
        ProbeKind::Startup => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
        ProbeKind::Ready
            if startup_complete && !shutting_down && dependencies_ready && checks_ready =>
        {
            (StatusCode::OK, "ok")
        }
        ProbeKind::Ready => (StatusCode::SERVICE_UNAVAILABLE, "degraded"),
    };

    let detailed = state.health_detailed();
    let body = ProbeResponse {
        status,
        version: if detailed {
            Some(env!("CARGO_PKG_VERSION"))
        } else {
            None
        },
        profile: if detailed {
            Some(state.profile().to_owned())
        } else {
            None
        },
        uptime: if detailed {
            Some(state.uptime_display())
        } else {
            None
        },
        pool: if detailed { pool_status } else { None },
        database: None,
    };

    (status_code, Json(body))
}

/// Run all readiness-group [`HealthIndicator`]s and return `false` if any are
/// `Down` or `OutOfService`.
///
/// [`HealthIndicator`]: crate::actuator::HealthIndicator
async fn check_readiness_indicators<S: ProvideProbeState + Sync>(state: &S) -> bool {
    let Some(registry) = state.health_indicator_registry() else {
        return true;
    };
    let results = registry.run_readiness().await;
    let statuses: Vec<crate::actuator::HealthStatus> =
        results.iter().map(|r| r.output.status).collect();
    crate::actuator::HealthIndicatorRegistry::aggregate_status(&statuses).is_healthy()
}

/// Ping the primary database (cached). `None` when no pool is configured.
#[cfg(feature = "db")]
async fn check_primary_db<S: ProvideProbeState + Sync>(state: &S) -> Option<DbPingStatus> {
    let pool = state.pool()?;
    Some(state.probes().check_primary_db(pool).await)
}

/// Build the readiness response for `/ready` and `/health`.
async fn readiness<S: ProvideProbeState + Sync>(state: &S) -> (StatusCode, Json<ProbeResponse>) {
    // Before startup completes and while draining, `/ready` is `503` whatever
    // the checks report. Do not run the slow ones.
    let probes = state.probes();
    if !probes.is_startup_complete() || probes.is_shutting_down() {
        #[cfg(feature = "db")]
        refresh_replica_readiness(state).await;
        return probe_response(state, ProbeKind::Ready, true);
    }

    #[cfg(feature = "db")]
    {
        // Run all checks at the same time: the wall time is one ping budget,
        // not the sum.
        let ((), primary, indicators_ready) = tokio::join!(
            refresh_replica_readiness(state),
            check_primary_db(state),
            check_readiness_indicators(state)
        );
        let primary_ready = !state.probes().primary_gates_readiness()
            || primary.as_ref().is_none_or(|status| status.up);
        let (code, Json(mut body)) =
            probe_response(state, ProbeKind::Ready, primary_ready && indicators_ready);
        if state.health_detailed() {
            body.database = primary.map(|status| DatabaseStatus {
                status: if status.up { "ok" } else { "down" },
                error: status.error,
            });
        }
        (code, Json(body))
    }

    #[cfg(not(feature = "db"))]
    {
        let indicators_ready = check_readiness_indicators(state).await;
        probe_response(state, ProbeKind::Ready, indicators_ready)
    }
}

/// `GET /live`
pub async fn live_handler<S: ProvideProbeState + Send + Sync + 'static>(
    State(state): State<S>,
) -> impl IntoResponse {
    probe_response(&state, ProbeKind::Live, true)
}

/// `GET /ready`
pub async fn ready_handler<S: ProvideProbeState + Send + Sync + 'static>(
    State(state): State<S>,
) -> impl IntoResponse {
    readiness(&state).await
}

/// `GET /startup`
pub async fn startup_handler<S: ProvideProbeState + Send + Sync + 'static>(
    State(state): State<S>,
) -> impl IntoResponse {
    probe_response(&state, ProbeKind::Startup, true)
}

/// Compatibility alias for the legacy `/health` endpoint.
pub(crate) async fn readiness_response<S: ProvideProbeState + Sync>(
    state: &S,
) -> (StatusCode, Json<ProbeResponse>) {
    readiness(state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestProbeState {
        probes: ProbeState,
        health_detailed: bool,
        profile: String,
    }

    impl ProvideProbeState for TestProbeState {
        fn probes(&self) -> &ProbeState {
            &self.probes
        }

        fn health_detailed(&self) -> bool {
            self.health_detailed
        }

        fn profile(&self) -> &str {
            &self.profile
        }

        fn uptime_display(&self) -> String {
            "test uptime".to_string()
        }

        #[cfg(feature = "db")]
        fn pool(
            &self,
        ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
        {
            None
        }
    }

    impl TestProbeState {
        fn new() -> Self {
            Self {
                probes: ProbeState::pending_startup(),
                health_detailed: true,
                profile: "test".to_string(),
            }
        }
    }

    #[test]
    fn test_live_handler_returns_ok() {
        let state = TestProbeState::new();
        let (status, Json(response)) = probe_response(&state, ProbeKind::Live, true);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, "ok");
    }

    #[tokio::test]
    async fn test_startup_handler_pending() {
        let state = TestProbeState::new();
        let (status, Json(response)) = probe_response(&state, ProbeKind::Startup, true);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status, "starting");
    }

    #[tokio::test]
    async fn test_startup_handler_complete() {
        let state = TestProbeState::new();
        state.mark_startup_complete();
        let (status, Json(response)) = probe_response(&state, ProbeKind::Startup, true);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, "ok");
    }

    #[tokio::test]
    async fn test_ready_handler_pending_startup() {
        let state = TestProbeState::new();
        let (status, Json(response)) = probe_response(&state, ProbeKind::Ready, true);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status, "degraded");
    }

    #[tokio::test]
    async fn test_ready_handler_complete_startup() {
        let state = TestProbeState::new();
        state.mark_startup_complete();
        let (status, Json(response)) = probe_response(&state, ProbeKind::Ready, true);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, "ok");
    }

    #[tokio::test]
    async fn test_ready_handler_shutting_down() {
        let state = TestProbeState::new();
        state.mark_startup_complete();
        state.probes().begin_shutdown();
        let (status, Json(response)) = probe_response(&state, ProbeKind::Ready, true);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status, "degraded");
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn ready_fails_when_replica_is_unready_and_policy_is_fail_readiness() {
        let state = TestProbeState::new();
        state.mark_startup_complete();
        state
            .probes()
            .configure_replica_dependency(crate::config::ReplicaFallback::FailReadiness);
        state
            .probes()
            .mark_replica_unready("replica migrations lag primary");

        let (status, Json(response)) = probe_response(&state, ProbeKind::Ready, true);

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status, "degraded");
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn ready_fails_when_replica_is_configured_but_not_checked() {
        let state = TestProbeState::new();
        state.mark_startup_complete();
        state
            .probes()
            .configure_replica_dependency(crate::config::ReplicaFallback::FailReadiness);

        let (status, Json(response)) = readiness_response(&state).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status, "degraded");
    }

    #[cfg(feature = "db")]
    #[test]
    fn replica_migration_lag_can_recover_without_resetting_connection_readiness() {
        let probes = ProbeState::ready_for_test();
        probes.configure_replica_dependency(crate::config::ReplicaFallback::FailReadiness);
        probes.mark_replica_connection_ready();
        probes.mark_replica_migrations_unready("replica migrations lag primary");

        assert!(!probes.replica_allows_readiness());
        assert!(!probes.should_route_reads_to_replica());

        probes.mark_replica_connection_ready();
        assert!(!probes.replica_allows_readiness());
        assert!(!probes.should_route_reads_to_replica());

        probes.mark_replica_migrations_ready();
        assert!(probes.replica_allows_readiness());
        assert!(probes.should_route_reads_to_replica());
    }

    #[cfg(feature = "db")]
    #[test]
    fn replica_migration_retry_urls_are_stored_for_readiness_rechecks() {
        let probes = ProbeState::ready_for_test();
        probes.configure_replica_dependency(crate::config::ReplicaFallback::FailReadiness);
        probes.configure_replica_migration_check(
            "postgres://localhost/primary",
            "postgres://localhost/replica",
        );

        let check = probes
            .replica_migration_check()
            .expect("migration check should be configured");

        assert_eq!(check.primary_url, "postgres://localhost/primary");
        assert_eq!(check.replica_url, "postgres://localhost/replica");
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn replica_migration_readiness_rechecks_after_initial_ready_state() {
        let state = TestProbeState::new();
        state.mark_startup_complete();
        state
            .probes()
            .configure_replica_dependency(crate::config::ReplicaFallback::FailReadiness);
        state.probes().configure_replica_migration_check(
            "postgres://localhost/primary",
            "postgres://localhost/replica",
        );
        state.probes().mark_replica_connection_ready();
        state.probes().mark_replica_migrations_ready();

        let checks = std::sync::atomic::AtomicUsize::new(0);
        refresh_replica_migration_readiness_with(&state, |check| {
            checks.fetch_add(1, Ordering::Relaxed);
            assert_eq!(check.primary_url, "postgres://localhost/primary");
            assert_eq!(check.replica_url, "postgres://localhost/replica");
            std::future::ready(crate::migrate::ReplicaMigrationReadiness::Stale {
                primary_latest: Some("20260511000000".to_owned()),
                replica_latest: Some("20260510000000".to_owned()),
            })
        })
        .await;

        assert_eq!(checks.load(Ordering::Relaxed), 1);
        assert!(!state.probes().replica_allows_readiness());
        assert!(!state.probes().should_route_reads_to_replica());
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn ready_allows_primary_fallback_when_replica_is_unready() {
        let state = TestProbeState::new();
        state.mark_startup_complete();
        state
            .probes()
            .configure_replica_dependency(crate::config::ReplicaFallback::Primary);
        state
            .probes()
            .mark_replica_unready("replica migrations lag primary");

        let (status, Json(response)) = probe_response(&state, ProbeKind::Ready, true);

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, "ok");
    }

    #[tokio::test]
    async fn test_probe_state_set_draining() {
        let state = ProbeState::starting();
        assert!(!state.draining());
        state.set_draining(true);
        assert!(state.draining());
    }

    #[tokio::test]
    async fn test_probe_state_set_startup_complete() {
        let state = ProbeState::starting();
        assert!(!state.is_startup_complete());
        state.set_startup_complete(true);
        assert!(state.is_startup_complete());
    }

    #[tokio::test]
    async fn test_ready_for_test() {
        let state = ProbeState::ready_for_test();
        assert!(state.is_startup_complete());
    }

    #[tokio::test]
    async fn test_health_detailed_false() {
        let mut state = TestProbeState::new();
        state.health_detailed = false;

        let (_, Json(response)) = probe_response(&state, ProbeKind::Live, true);
        assert!(response.version.is_none());
        assert!(response.profile.is_none());
        assert!(response.uptime.is_none());
        assert!(response.pool.is_none());
    }

    #[tokio::test]
    async fn test_begin_draining() {
        let state = ProbeState::ready_for_test();
        assert!(!state.draining());
        state.begin_draining();
        assert!(state.draining());
    }

    // ── HealthIndicator integration with /ready ──────────────────

    struct TestProbeStateWithIndicators {
        probes: ProbeState,
        health_detailed: bool,
        profile: String,
        registry: crate::actuator::HealthIndicatorRegistry,
    }

    impl ProvideProbeState for TestProbeStateWithIndicators {
        fn probes(&self) -> &ProbeState {
            &self.probes
        }

        fn health_detailed(&self) -> bool {
            self.health_detailed
        }

        fn profile(&self) -> &str {
            &self.profile
        }

        fn uptime_display(&self) -> String {
            "test uptime".to_string()
        }

        #[cfg(feature = "db")]
        fn pool(
            &self,
        ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
        {
            None
        }

        fn health_indicator_registry(&self) -> Option<&crate::actuator::HealthIndicatorRegistry> {
            Some(&self.registry)
        }
    }

    impl TestProbeStateWithIndicators {
        fn new(registry: crate::actuator::HealthIndicatorRegistry) -> Self {
            let probes = ProbeState::pending_startup();
            probes.mark_startup_complete();
            Self {
                probes,
                health_detailed: true,
                profile: "test".to_string(),
                registry,
            }
        }
    }

    struct AlwaysDown;
    impl crate::actuator::HealthIndicator for AlwaysDown {
        fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
            Box::pin(async { crate::actuator::HealthCheckOutput::down() })
        }
    }

    struct AlwaysUp;
    impl crate::actuator::HealthIndicator for AlwaysUp {
        fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
            Box::pin(async { crate::actuator::HealthCheckOutput::up() })
        }
    }

    #[tokio::test]
    async fn ready_is_degraded_when_readiness_indicator_is_down() {
        let registry = crate::actuator::HealthIndicatorRegistry::new();
        registry
            .register(
                "svc",
                crate::actuator::IndicatorGroup::Readiness,
                std::sync::Arc::new(AlwaysDown),
            )
            .unwrap();
        let state = TestProbeStateWithIndicators::new(registry);
        let (status, Json(response)) = readiness_response(&state).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.status, "degraded");
    }

    #[tokio::test]
    async fn ready_is_ok_when_readiness_indicator_is_up() {
        let registry = crate::actuator::HealthIndicatorRegistry::new();
        registry
            .register(
                "svc",
                crate::actuator::IndicatorGroup::Readiness,
                std::sync::Arc::new(AlwaysUp),
            )
            .unwrap();
        let state = TestProbeStateWithIndicators::new(registry);
        let (status, Json(response)) = readiness_response(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, "ok");
    }

    #[tokio::test]
    async fn ready_is_ok_when_health_only_indicator_is_down() {
        let registry = crate::actuator::HealthIndicatorRegistry::new();
        registry
            .register(
                "svc",
                crate::actuator::IndicatorGroup::HealthOnly,
                std::sync::Arc::new(AlwaysDown),
            )
            .unwrap();
        let state = TestProbeStateWithIndicators::new(registry);
        let (status, Json(response)) = readiness_response(&state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response.status, "ok");
    }

    // ── Primary database readiness (#3059) ───────────────────────

    #[cfg(feature = "db")]
    mod primary_db {
        use super::*;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        /// Counts pings. Each ping waits `delay`, then returns `result`.
        struct FakePing {
            calls: AtomicUsize,
            delay: Duration,
            result: Result<(), String>,
        }

        impl FakePing {
            fn new(delay: Duration, result: Result<(), String>) -> Arc<Self> {
                Arc::new(Self {
                    calls: AtomicUsize::new(0),
                    delay,
                    result,
                })
            }

            fn calls(&self) -> usize {
                self.calls.load(Ordering::SeqCst)
            }
        }

        impl crate::db_ping::DbPing for FakePing {
            fn ping(
                self: Arc<Self>,
                _pool: DbPool,
                _budget: Duration,
            ) -> futures::future::BoxFuture<'static, Result<(), String>> {
                Box::pin(async move {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(self.delay).await;
                    self.result.clone()
                })
            }
        }

        struct DbProbeState {
            probes: ProbeState,
            pool: DbPool,
            replica: Option<DbPool>,
        }

        impl ProvideProbeState for DbProbeState {
            fn probes(&self) -> &ProbeState {
                &self.probes
            }

            fn health_detailed(&self) -> bool {
                true
            }

            fn profile(&self) -> &'static str {
                "test"
            }

            fn uptime_display(&self) -> String {
                String::new()
            }

            fn pool(&self) -> Option<&DbPool> {
                Some(&self.pool)
            }

            fn replica_pool(&self) -> Option<&DbPool> {
                self.replica.as_ref()
            }
        }

        /// A pool that never connects. Fake pings do not use it.
        fn idle_pool(url: &str) -> DbPool {
            #[cfg(feature = "sqlite")]
            let url = {
                let _ = url;
                "sqlite::memory:"
            };
            let config = crate::config::DatabaseConfig {
                url: Some(url.to_owned()),
                pool_size: 1,
                // Long pool waits: a test that fills the pool keeps it full.
                connect_timeout_secs: 30,
                ..Default::default()
            };
            crate::db::create_pool(&config)
                .expect("pool config is valid")
                .expect("pool url is set")
        }

        fn state_with(ping: Arc<dyn crate::db_ping::DbPing>) -> DbProbeState {
            let probes = ProbeState::with_primary_ping(ping);
            probes.mark_startup_complete();
            DbProbeState {
                probes,
                pool: idle_pool("postgres://autumn@127.0.0.1:1/unused"),
                replica: None,
            }
        }

        #[tokio::test(start_paused = true)]
        async fn ready_is_ok_when_primary_answers() {
            let ping = FakePing::new(Duration::ZERO, Ok(()));
            let state = state_with(ping.clone());

            let (status, Json(body)) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::OK);
            assert_eq!(body.status, "ok");
            assert_eq!(ping.calls(), 1);
        }

        #[tokio::test(start_paused = true)]
        async fn ready_is_degraded_when_primary_ping_fails() {
            let ping = FakePing::new(Duration::ZERO, Err("connection refused".to_owned()));
            let state = state_with(ping);

            let (status, Json(body)) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            let database = body.database.expect("detailed body reports the database");
            assert_eq!(database.status, "down");
            assert!(
                database
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("connection refused")),
                "error: {:?}",
                database.error
            );
        }

        #[tokio::test(start_paused = true)]
        async fn ready_is_degraded_within_timeout_when_primary_hangs() {
            let ping = FakePing::new(Duration::from_secs(3_600), Ok(()));
            let state = state_with(ping);
            state
                .probes
                .configure_db_check(Duration::from_secs(1), Duration::from_millis(250));

            let started = tokio::time::Instant::now();
            let (status, Json(body)) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(started.elapsed(), Duration::from_millis(250));
            let error = body.database.and_then(|d| d.error).unwrap_or_default();
            assert!(error.contains("timed out"), "error: {error}");
        }

        #[tokio::test(start_paused = true)]
        async fn concurrent_probes_ping_primary_at_most_once_per_window() {
            let ping = FakePing::new(Duration::from_millis(20), Ok(()));
            let state = Arc::new(state_with(ping.clone()));
            state
                .probes
                .configure_db_check(Duration::from_secs(1), Duration::from_secs(2));

            let burst = || {
                let tasks: Vec<_> = (0..64)
                    .map(|_| {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move { readiness_response(&*state).await.0 })
                    })
                    .collect();
                async move {
                    for task in tasks {
                        assert_eq!(task.await.unwrap(), StatusCode::OK);
                    }
                }
            };

            burst().await;
            assert_eq!(ping.calls(), 1, "one ping for the first window");

            tokio::time::advance(Duration::from_millis(500)).await;
            burst().await;
            assert_eq!(ping.calls(), 1, "still inside the first window");

            tokio::time::advance(Duration::from_millis(600)).await;
            burst().await;
            assert_eq!(ping.calls(), 2, "one ping for the second window");
        }

        #[tokio::test(start_paused = true)]
        async fn zero_ttl_pings_primary_on_every_probe() {
            let ping = FakePing::new(Duration::ZERO, Ok(()));
            let state = state_with(ping.clone());
            state
                .probes
                .configure_db_check(Duration::ZERO, Duration::from_secs(2));

            for _ in 0..3 {
                let _ = readiness_response(&state).await;
            }

            assert_eq!(ping.calls(), 3);
        }

        #[tokio::test(start_paused = true)]
        async fn primary_and_replica_pings_run_at_the_same_time() {
            let primary = FakePing::new(Duration::from_millis(400), Ok(()));
            let replica = FakePing::new(Duration::from_millis(400), Ok(()));
            let probes = ProbeState::with_db_pings(primary.clone(), replica.clone());
            probes.mark_startup_complete();
            probes.configure_replica_dependency(crate::config::ReplicaFallback::FailReadiness);
            let state = DbProbeState {
                probes,
                pool: idle_pool("postgres://autumn@127.0.0.1:1/unused"),
                replica: Some(idle_pool("postgres://autumn@127.0.0.1:1/replica")),
            };

            let started = tokio::time::Instant::now();
            let (status, _) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::OK);
            assert_eq!(primary.calls(), 1);
            assert_eq!(replica.calls(), 1);
            // One ping budget, not two: the pings do not wait for each other.
            assert_eq!(started.elapsed(), Duration::from_millis(400));
        }

        #[tokio::test(start_paused = true)]
        async fn ready_does_not_ping_primary_while_draining() {
            let ping = FakePing::new(Duration::ZERO, Ok(()));
            let state = state_with(ping.clone());
            state.probes.begin_draining();

            let (status, _) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(ping.calls(), 0);
        }

        #[tokio::test(start_paused = true)]
        async fn db_readiness_off_keeps_ready_when_primary_fails() {
            let ping = FakePing::new(Duration::ZERO, Err("connection refused".to_owned()));
            let state = state_with(ping.clone());
            state.probes.set_db_readiness(false);

            let (status, Json(body)) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::OK);
            assert_eq!(body.database.map(|d| d.status), Some("down"));
            assert_eq!(ping.calls(), 1, "the ping still runs for the report");
        }

        #[tokio::test(start_paused = true)]
        async fn clones_share_one_ping_cache() {
            let ping = FakePing::new(Duration::ZERO, Ok(()));
            let state = state_with(ping.clone());
            let clone = DbProbeState {
                probes: state.probes.clone(),
                pool: state.pool.clone(),
                replica: None,
            };

            let _ = readiness_response(&state).await;
            let _ = readiness_response(&clone).await;

            assert_eq!(ping.calls(), 1);
        }

        #[cfg(feature = "sqlite")]
        #[tokio::test]
        async fn sqlite_dedicated_ping_is_up_and_does_not_use_the_pool() {
            let probes = ProbeState::ready_for_test();
            probes.configure_db_check(Duration::ZERO, Duration::from_secs(5));
            let state = DbProbeState {
                probes,
                pool: idle_pool("sqlite::memory:"),
                replica: None,
            };

            let (status, _) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::OK);
            assert_eq!(state.pool.status().size, 0, "the ping is not in the pool");
        }

        /// Accepts TCP connections and never answers: a database that is
        /// down with no reset and no reply.
        #[cfg(not(feature = "sqlite"))]
        async fn black_hole() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind black hole");
            let addr = listener.local_addr().expect("local addr");
            let task = tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    // Hold the socket open and never answer.
                    tokio::spawn(async move {
                        let _socket = socket;
                        std::future::pending::<()>().await;
                    });
                }
            });
            (addr, task)
        }

        #[cfg(not(feature = "sqlite"))]
        #[tokio::test]
        async fn ready_is_ok_when_pool_is_saturated_and_primary_answers() {
            // A pool of 1 whose only slot hangs in connect, and one request
            // that waits for a slot: `available == 0` and `waiting > 0`. The
            // old rule `available > 0 || waiting == 0` gave `503` here.
            let (addr, server) = black_hole().await;
            let ping = FakePing::new(Duration::ZERO, Ok(()));
            let probes = ProbeState::with_primary_ping(ping);
            probes.mark_startup_complete();
            let state = DbProbeState {
                probes,
                pool: idle_pool(&format!("postgres://autumn@{addr}/autumn")),
                replica: None,
            };
            let holders: Vec<_> = (0..2)
                .map(|_| {
                    let pool = state.pool.clone();
                    tokio::spawn(async move { drop(pool.get().await) })
                })
                .collect();
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.pool.status().waiting == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("a request waits for a connection");
            assert_eq!(state.pool.status().available, 0);

            let (status, _) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::OK);
            for holder in holders {
                holder.abort();
            }
            server.abort();
        }

        #[cfg(not(feature = "sqlite"))]
        #[tokio::test]
        async fn dedicated_ping_is_down_within_timeout_when_primary_hangs() {
            let (addr, server) = black_hole().await;
            let probes = ProbeState::ready_for_test();
            probes.configure_db_check(Duration::ZERO, Duration::from_millis(300));
            let state = DbProbeState {
                probes,
                pool: idle_pool(&format!("postgres://autumn@{addr}/autumn")),
                replica: None,
            };

            let started = std::time::Instant::now();
            let (status, _) = readiness_response(&state).await;
            let elapsed = started.elapsed();

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                elapsed < Duration::from_millis(300) + Duration::from_secs(3),
                "took {elapsed:?}"
            );
            server.abort();
        }

        #[cfg(not(feature = "sqlite"))]
        #[tokio::test]
        async fn dedicated_ping_is_down_when_primary_refuses() {
            let addr = {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
                listener.local_addr().expect("local addr")
            };
            let probes = ProbeState::ready_for_test();
            probes.configure_db_check(Duration::ZERO, Duration::from_secs(2));
            let state = DbProbeState {
                probes,
                pool: idle_pool(&format!("postgres://autumn@{addr}/autumn")),
                replica: None,
            };

            let (status, _) = readiness_response(&state).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        }
    }
}
