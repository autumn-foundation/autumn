//! Canary deploy primitives: deploy-version labelling and rollback signalling.
//!
//! Autumn does not implement the load-balancer traffic split itself (that
//! remains a platform concern — Fly.io machine weights, Kubernetes
//! `TrafficPolicy`, Nginx upstream `weight`). Instead it provides the
//! framework-level hooks a canary controller drives:
//!
//! - **Deploy-version labelling**: every replica resolves a `version` label
//!   (`"stable"` / `"canary"` / a custom string) from configuration or the
//!   environment so its metrics can be compared cohort-against-cohort.
//! - **Traffic-routing identification**: a [`CanaryRoute`] extractor lets a
//!   request handler see whether the load balancer stamped `X-Canary` on the
//!   request, and the replica's own [`CanaryState::is_canary`] tells it whether
//!   it is the canary version.
//! - **Rollback signalling**: a file-flag protocol (modelled on
//!   [`crate::maintenance`]) lets a controller instruct a bad canary replica to
//!   flip `/ready` to 503, drain, and exit cleanly — without sending `SIGTERM`
//!   by hand.
//!
//! # Rollback file-flag protocol
//!
//! - **Rollback**: a controller writes [`CANARY_ROLLBACK_FLAG_FILE`] (JSON).
//!   The running replica notices within ~500 ms and begins the same graceful
//!   shutdown sequence it would run on `SIGTERM` (ready → 503, prestop grace,
//!   listener close, in-flight drain, clean exit).
//! - **Promote**: a controller removes the flag file. Promotion of traffic to
//!   100 % is a platform action; at the framework level promotion simply means
//!   "no rollback pending".

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;

/// Default path for the canary rollback flag file, relative to the project root.
pub const CANARY_ROLLBACK_FLAG_FILE: &str = "tmp/autumn-canary-rollback.json";

/// Environment variable that sets the explicit deploy-version label.
pub const DEPLOY_VERSION_ENV: &str = "AUTUMN_DEPLOY_VERSION";

/// Environment variable that, when truthy, marks the replica as the canary.
pub const CANARY_ENV: &str = "AUTUMN_CANARY";

/// Request header the load balancer stamps on canary-bound requests.
pub const CANARY_HEADER: &str = "x-canary";

/// Canonical label for the stable cohort.
pub const STABLE: &str = "stable";

/// Canonical label for the canary cohort.
pub const CANARY: &str = "canary";

/// Returns `true` when `value` is a recognised truthy string (case-insensitive).
#[must_use]
pub fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Resolve the deploy-version label from an explicit value and a canary flag.
///
/// Precedence:
/// 1. `explicit` (e.g. `AUTUMN_DEPLOY_VERSION`) when set and non-empty.
/// 2. `"canary"` when `canary_flag` is truthy.
/// 3. [`STABLE`] otherwise.
#[must_use]
pub fn resolve_deploy_version(explicit: Option<&str>, canary_flag: Option<&str>) -> String {
    if let Some(label) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return label.to_owned();
    }
    if canary_flag.is_some_and(is_truthy) {
        return CANARY.to_owned();
    }
    STABLE.to_owned()
}

/// Resolve the deploy-version label for this replica from the environment.
#[must_use]
pub fn deploy_version_from_env() -> String {
    let explicit = std::env::var(DEPLOY_VERSION_ENV).ok();
    let canary_flag = std::env::var(CANARY_ENV).ok();
    resolve_deploy_version(explicit.as_deref(), canary_flag.as_deref())
}

/// Payload written to the rollback flag file by a controller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSignal {
    /// Human-readable reason for the rollback (e.g. "p99 latency exceeded").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Identifier of the actor or controller that requested the rollback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

/// In-process canary state, cheaply cloneable across threads.
#[derive(Clone, Debug)]
pub struct CanaryState {
    version: Arc<str>,
    rollback_requested: Arc<AtomicBool>,
}

impl CanaryState {
    /// Create a [`CanaryState`] with an explicit deploy-version label.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: Arc::from(version.into().as_str()),
            rollback_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a [`CanaryState`] from the environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(deploy_version_from_env())
    }

    /// The deploy-version label for this replica.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns `true` when this replica is labelled as the canary cohort.
    #[must_use]
    pub fn is_canary(&self) -> bool {
        self.version() == CANARY
    }

    /// Mark that a rollback has been requested for this replica.
    pub fn request_rollback(&self) {
        self.rollback_requested.store(true, Ordering::SeqCst);
    }

    /// Returns `true` when a rollback has been requested for this replica.
    #[must_use]
    pub fn rollback_requested(&self) -> bool {
        self.rollback_requested.load(Ordering::SeqCst)
    }

    /// Returns `true` when the rollback flag file exists at `path`.
    #[must_use]
    pub fn rollback_flag_present(path: &Path) -> bool {
        path.exists()
    }

    /// Write a [`RollbackSignal`] to the flag file, creating parent dirs.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the directory cannot be created, the signal cannot be
    /// serialised, or the file cannot be written.
    pub fn write_rollback_flag(path: &Path, signal: &RollbackSignal) -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(signal).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Load a [`RollbackSignal`] from the flag file.
    ///
    /// Returns `Ok(None)` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns `Err` for I/O errors other than `NotFound`, or invalid JSON.
    pub fn load_rollback_flag(path: &Path) -> std::io::Result<Option<RollbackSignal>> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let signal: RollbackSignal = serde_json::from_str(&s)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some(signal))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Remove the rollback flag file.
    ///
    /// Returns `Ok(true)` when the file was deleted, `Ok(false)` when absent.
    ///
    /// # Errors
    ///
    /// Returns `Err` for filesystem errors other than `NotFound`.
    pub fn remove_rollback_flag(path: &Path) -> std::io::Result<bool> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Typed extractor exposing whether the load balancer routed this request to the
/// canary cohort, via the [`CANARY_HEADER`] (`X-Canary`) request header.
///
/// This lets application code react to canary routing without parsing headers
/// by hand. It never fails — a missing or unparsable header means
/// `routed_to_canary == false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CanaryRoute {
    /// `true` when the request carried a truthy `X-Canary` header.
    pub routed_to_canary: bool,
}

impl<S> FromRequestParts<S> for CanaryRoute
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let routed_to_canary = parts
            .headers
            .get(CANARY_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(is_truthy);
        Ok(Self { routed_to_canary })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_truthy_recognises_common_values() {
        for v in ["1", "true", "TRUE", "yes", "on", "True"] {
            assert!(is_truthy(v), "{v} should be truthy");
        }
    }

    #[test]
    fn is_truthy_rejects_falsey_values() {
        for v in ["0", "false", "no", "off", "", "canary"] {
            assert!(!is_truthy(v), "{v} should not be truthy");
        }
    }

    #[test]
    fn resolve_defaults_to_stable() {
        assert_eq!(resolve_deploy_version(None, None), STABLE);
    }

    #[test]
    fn resolve_canary_flag_yields_canary() {
        assert_eq!(resolve_deploy_version(None, Some("true")), CANARY);
        assert_eq!(resolve_deploy_version(None, Some("1")), CANARY);
    }

    #[test]
    fn resolve_falsey_canary_flag_yields_stable() {
        assert_eq!(resolve_deploy_version(None, Some("false")), STABLE);
    }

    #[test]
    fn resolve_explicit_version_takes_precedence() {
        assert_eq!(resolve_deploy_version(Some("v2"), Some("true")), "v2");
        assert_eq!(resolve_deploy_version(Some("canary"), None), CANARY);
    }

    #[test]
    fn resolve_ignores_empty_explicit() {
        assert_eq!(resolve_deploy_version(Some("  "), Some("true")), CANARY);
        assert_eq!(resolve_deploy_version(Some(""), None), STABLE);
    }

    #[test]
    fn from_env_reads_canary_flag() {
        temp_env::with_vars(
            [
                ("AUTUMN_DEPLOY_VERSION", None::<&str>),
                ("AUTUMN_CANARY", Some("true")),
            ],
            || assert_eq!(CanaryState::from_env().version(), CANARY),
        );
    }

    #[test]
    fn from_env_explicit_version_wins_over_flag() {
        temp_env::with_vars(
            [
                ("AUTUMN_DEPLOY_VERSION", Some("v2")),
                ("AUTUMN_CANARY", Some("true")),
            ],
            || assert_eq!(CanaryState::from_env().version(), "v2"),
        );
    }

    #[test]
    fn from_env_defaults_to_stable() {
        temp_env::with_vars(
            [
                ("AUTUMN_DEPLOY_VERSION", None::<&str>),
                ("AUTUMN_CANARY", None::<&str>),
            ],
            || assert_eq!(CanaryState::from_env().version(), STABLE),
        );
    }

    #[test]
    fn state_reports_version() {
        let state = CanaryState::new("canary");
        assert_eq!(state.version(), "canary");
        assert!(state.is_canary());
    }

    #[test]
    fn state_stable_is_not_canary() {
        let state = CanaryState::new(STABLE);
        assert!(!state.is_canary());
    }

    #[test]
    fn state_custom_version_is_not_canary() {
        let state = CanaryState::new("v2");
        assert!(!state.is_canary());
    }

    #[test]
    fn state_rollback_flag_round_trips() {
        let state = CanaryState::new(CANARY);
        assert!(!state.rollback_requested());
        state.request_rollback();
        assert!(state.rollback_requested());
    }

    #[test]
    fn state_clone_shares_rollback_flag() {
        let state = CanaryState::new(CANARY);
        let clone = state.clone();
        state.request_rollback();
        assert!(clone.rollback_requested());
    }

    #[test]
    fn rollback_flag_absent_by_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("canary-rollback.json");
        assert!(!CanaryState::rollback_flag_present(&path));
        assert!(CanaryState::load_rollback_flag(&path).unwrap().is_none());
    }

    #[test]
    fn rollback_flag_write_load_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("canary-rollback.json");
        let signal = RollbackSignal {
            reason: Some("p99 latency exceeded".into()),
            requested_by: Some("ci-canary-controller".into()),
        };
        CanaryState::write_rollback_flag(&path, &signal).unwrap();
        assert!(CanaryState::rollback_flag_present(&path));
        let loaded = CanaryState::load_rollback_flag(&path).unwrap().unwrap();
        assert_eq!(loaded, signal);
    }

    #[test]
    fn rollback_flag_remove_reports_deletion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("canary-rollback.json");
        CanaryState::write_rollback_flag(&path, &RollbackSignal::default()).unwrap();
        assert!(CanaryState::remove_rollback_flag(&path).unwrap());
        assert!(!CanaryState::remove_rollback_flag(&path).unwrap());
    }

    #[tokio::test]
    async fn canary_route_reads_header() {
        use axum::http::Request;
        let req = Request::builder()
            .header(CANARY_HEADER, "true")
            .body(())
            .unwrap();
        let (mut parts, ()) = req.into_parts();
        let route = CanaryRoute::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert!(route.routed_to_canary);
    }

    #[tokio::test]
    async fn canary_route_defaults_false_without_header() {
        use axum::http::Request;
        let req = Request::builder().body(()).unwrap();
        let (mut parts, ()) = req.into_parts();
        let route = CanaryRoute::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert!(!route.routed_to_canary);
    }
}
