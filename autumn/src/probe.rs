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
    ///     fn pool(&self) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>> { None }
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
#[derive(Clone, Debug, Default)]
pub struct ProbeState {
    startup_complete: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    #[cfg(feature = "db")]
    replica_dependency: Arc<RwLock<ReplicaDependency>>,
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
}

#[derive(Serialize)]
pub(crate) struct PoolStatus {
    size: u64,
    available: u64,
    waiting: u64,
}

#[allow(clippy::missing_const_for_fn, unused_variables)]
fn dependency_readiness<S: ProvideProbeState>(state: &S) -> (bool, Option<PoolStatus>) {
    #[cfg(feature = "db")]
    {
        let replica_ready_for_policy = state.probes().replica_allows_readiness();
        let (pool_ready, pool_status) = state.pool().map_or((true, None), |pool| {
            let status = pool.status();
            let available = status.available as u64;
            let size = status.max_size as u64;
            let waiting = status.waiting as u64;

            (
                available > 0 || waiting == 0,
                Some(PoolStatus {
                    size,
                    available,
                    waiting,
                }),
            )
        });

        (pool_ready && replica_ready_for_policy, pool_status)
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

    match replica_pool.get().await {
        Ok(conn) => {
            drop(conn);
            state.probes().mark_replica_connection_ready();
            refresh_replica_migration_readiness(state).await;
        }
        Err(error) => state
            .probes()
            .mark_replica_connection_unready(format!("replica connection failed: {error}")),
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
    indicator_ready: bool,
) -> (StatusCode, Json<ProbeResponse>) {
    let startup_complete = state.probes().is_startup_complete();
    let shutting_down = state.probes().is_shutting_down();
    let (dependencies_ready, pool_status) = dependency_readiness(state);

    let (status_code, status) = match kind {
        ProbeKind::Live => (StatusCode::OK, "ok"),
        ProbeKind::Startup if startup_complete => (StatusCode::OK, "ok"),
        ProbeKind::Startup => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
        ProbeKind::Ready
            if startup_complete && !shutting_down && dependencies_ready && indicator_ready =>
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
    };

    (status_code, Json(body))
}

/// Return `true` when the `/ready` response will be 503 regardless of indicator
/// results — avoids running potentially slow indicators unnecessarily.
fn already_degraded<S: ProvideProbeState>(state: &S) -> bool {
    let probes = state.probes();
    !probes.is_startup_complete() || probes.is_shutting_down() || !dependency_readiness(state).0
}

/// Run all readiness-group [`HealthIndicator`]s and return `false` if any are
/// `Down` or `OutOfService`.
async fn check_readiness_indicators<S: ProvideProbeState + Sync>(state: &S) -> bool {
    let Some(registry) = state.health_indicator_registry() else {
        return true;
    };
    let results = registry.run_readiness().await;
    let statuses: Vec<crate::actuator::HealthStatus> =
        results.iter().map(|r| r.output.status).collect();
    crate::actuator::HealthIndicatorRegistry::aggregate_status(&statuses).is_healthy()
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
    #[cfg(feature = "db")]
    refresh_replica_readiness(&state).await;
    // Skip slow indicator checks when the probe will be 503 regardless.
    let indicator_ready = if already_degraded(&state) {
        true
    } else {
        check_readiness_indicators(&state).await
    };
    probe_response(&state, ProbeKind::Ready, indicator_ready)
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
    #[cfg(feature = "db")]
    refresh_replica_readiness(state).await;
    let indicator_ready = if already_degraded(state) {
        true
    } else {
        check_readiness_indicators(state).await
    };
    probe_response(state, ProbeKind::Ready, indicator_ready)
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
}
