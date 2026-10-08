//! Opt-in fault injection for a staging environment (issue #3071).
//!
//! The `[fault_injection]` section declares faults. Each fault adds latency
//! or an error to a route or to a dependency, at a rate, on matched paths.
//! The router installs two layers when the section is enabled:
//!
//! - an outer scope layer, outside the session layer. It selects the faults
//!   for the request path, puts them in a task-local scope for the database,
//!   Redis and HTTP client seams, and counts the final status for the stop
//!   condition;
//! - [`FaultInjectionLayer`], inside the timeout layer. It applies the route
//!   faults. The access log, error reporting and the timeout see an injected
//!   fault as a real one.
//!
//! Safety rules:
//!
//! - In `prod`, config validation fails and the router does not install the
//!   layers, unless `allow_in_production = true`.
//! - The stop condition disarms the faults when the error ratio burns the
//!   error budget too fast. They stay disarmed until [`FaultInjection::arm`].
//! - Probe and actuator paths are never faulted.
//! - Each toggle writes an audit event (`fault_injection.armed` or
//!   `fault_injection.disarmed`) and a `warn` log.
//!
//! Get the handle with `state.extension::<FaultInjection>()`.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate".
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

mod config;
mod layer;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::http::StatusCode;

use crate::audit::{AuditEvent, AuditLogger, AuditStatus};
use crate::entropy::Entropy;
use crate::slo::PPM;

pub use config::{FaultInjectionConfig, FaultKind, FaultRule, FaultStopConfig, FaultTarget};
pub(crate) use layer::FaultScopeLayer;
pub use layer::{FaultInjectionLayer, FaultInjectionService};

/// The response header on a response that an injected route error made.
pub const FAULT_HEADER: &str = "x-autumn-fault";

/// The actor of a toggle that the framework makes.
const SYSTEM_ACTOR: &str = "autumn";

/// The handle of the installed faults. Clones share one state.
#[derive(Clone)]
pub struct FaultInjection {
    inner: Arc<Injector>,
}

impl std::fmt::Debug for FaultInjection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultInjection")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

/// A point-in-time view of the installed faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FaultSnapshot {
    /// `true` while faults can fire.
    pub armed: bool,
    /// The number of faults that fired since boot.
    pub injected: u64,
    /// The requests in the current stop window.
    pub window_requests: u64,
    /// The `5xx` responses in the current stop window.
    pub window_errors: u64,
}

/// The error that an injected dependency fault returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("fault injection: injected {} error", target.name())]
pub struct InjectedFault {
    /// The dependency.
    pub target: FaultTarget,
}

impl FaultInjection {
    /// `true` while faults can fire.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.inner.armed.load(Ordering::Acquire)
    }

    /// The counters.
    #[must_use]
    pub fn snapshot(&self) -> FaultSnapshot {
        let window = self.inner.lock_window();
        FaultSnapshot {
            armed: self.is_armed(),
            injected: self.inner.injected.load(Ordering::Relaxed),
            window_requests: window.requests,
            window_errors: window.errors,
        }
    }

    /// Stop the faults, and write an audit event for `actor`.
    ///
    /// No event is written when the faults are already disarmed.
    pub async fn disarm(&self, actor: &str, reason: &str) {
        if self.inner.armed.swap(false, Ordering::AcqRel) {
            self.inner.audit(actor, false, reason).await;
        }
    }

    /// Start the faults again, with an empty stop window, and write an audit
    /// event for `actor`.
    ///
    /// No event is written when the faults are already armed.
    pub async fn arm(&self, actor: &str) {
        *self.inner.lock_window() = Window::default();
        if !self.inner.armed.swap(true, Ordering::AcqRel) {
            self.inner.audit(actor, true, "armed by operator").await;
        }
    }
}

/// One fault, ready to roll.
#[derive(Debug, Clone)]
struct CompiledRule {
    routes: Vec<RoutePattern>,
    target: FaultTarget,
    kind: FaultKind,
    rate_ppm: u32,
    latency: Duration,
    status: StatusCode,
}

#[derive(Debug, Clone)]
enum RoutePattern {
    Prefix(String),
    Exact(String),
}

impl RoutePattern {
    fn parse(raw: &str) -> Self {
        raw.strip_suffix('*').map_or_else(
            || Self::Exact(raw.to_owned()),
            |prefix| Self::Prefix(prefix.to_owned()),
        )
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Prefix(prefix) => path.starts_with(prefix.as_str()),
            Self::Exact(exact) => path == exact,
        }
    }
}

impl CompiledRule {
    fn new(rule: &FaultRule) -> Self {
        Self {
            routes: rule.routes.iter().map(|r| RoutePattern::parse(r)).collect(),
            target: rule.target,
            kind: rule.kind,
            rate_ppm: rule.rate_ppm(),
            latency: Duration::from_millis(rule.latency_ms),
            status: StatusCode::from_u16(rule.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        }
    }

    fn matches(&self, path: &str) -> bool {
        self.routes.is_empty() || self.routes.iter().any(|route| route.matches(path))
    }
}

/// Paths that are never faulted.
#[derive(Debug, Clone)]
struct Exempt {
    paths: Vec<String>,
    actuator_prefix: String,
    actuator_prefix_slash: String,
}

impl Exempt {
    fn contains(&self, path: &str) -> bool {
        self.paths.iter().any(|exempt| exempt == path)
            || path == self.actuator_prefix
            || path.starts_with(self.actuator_prefix_slash.as_str())
    }
}

/// The stop window.
#[derive(Debug, Default)]
struct Window {
    started: Option<tokio::time::Instant>,
    requests: u64,
    errors: u64,
}

struct Injector {
    rules: Vec<CompiledRule>,
    exempt: Exempt,
    max_error_ppm: u32,
    window_len: Duration,
    min_requests: u64,
    armed: AtomicBool,
    injected: AtomicU64,
    window: Mutex<Window>,
    entropy: Arc<dyn Entropy>,
    audit: Option<Arc<AuditLogger>>,
    profile: String,
    /// The boot audit write, until a later toggle waits for it. Thus the
    /// events stay in toggle order.
    boot: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Injector {
    fn lock_window(&self) -> std::sync::MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `true` with probability `ppm / 1_000_000`.
    fn roll(&self, ppm: u32) -> bool {
        if ppm == 0 {
            return false;
        }
        if ppm >= PPM {
            return true;
        }
        self.entropy
            .next_u64()
            .checked_rem(u64::from(PPM))
            .is_some_and(|draw| draw < u64::from(ppm))
    }

    /// The scope for a request to `path`, or `None` when nothing can fire.
    fn scope_for(self: &Arc<Self>, path: &str) -> Option<Arc<RequestScope>> {
        if !self.armed.load(Ordering::Acquire) || self.exempt.contains(path) {
            return None;
        }
        let rules = self
            .rules
            .iter()
            .filter(|rule| rule.matches(path))
            .cloned()
            .collect();
        Some(Arc::new(RequestScope {
            injector: Arc::clone(self),
            rules,
            fired: AtomicBool::new(false),
        }))
    }

    /// Count one response. Returns `true` when this response trips the stop
    /// condition.
    fn record(&self, error: bool) -> bool {
        let now = tokio::time::Instant::now();
        let mut window = self.lock_window();
        let expired = window
            .started
            .is_none_or(|started| now.saturating_duration_since(started) >= self.window_len);
        if expired {
            *window = Window {
                started: Some(now),
                requests: 0,
                errors: 0,
            };
        }
        window.requests = window.requests.saturating_add(1);
        if error {
            window.errors = window.errors.saturating_add(1);
        }
        if window.requests < self.min_requests {
            return false;
        }
        let errors_ppm = u128::from(window.errors).saturating_mul(u128::from(PPM));
        let limit_ppm = u128::from(self.max_error_ppm).saturating_mul(u128::from(window.requests));
        drop(window);
        errors_ppm > limit_ppm && self.armed.swap(false, Ordering::AcqRel)
    }

    /// Write a toggle, after the boot toggle.
    async fn audit(&self, actor: &str, armed: bool, reason: &str) {
        let boot = self.boot.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(boot) = boot {
            // A failed boot write is already logged; continue.
            let _ = boot.await;
        }
        self.write_audit(actor, armed, reason).await;
    }

    async fn write_audit(&self, actor: &str, armed: bool, reason: &str) {
        let action = if armed {
            "fault_injection.armed"
        } else {
            "fault_injection.disarmed"
        };
        tracing::warn!(
            target: "autumn.fault_injection",
            actor,
            action,
            reason,
            profile = %self.profile,
            "fault injection toggled"
        );
        let Some(logger) = &self.audit else {
            return;
        };
        let event = AuditEvent::new(actor, action, "fault_injection", None, AuditStatus::Success)
            .with_metadata("reason", reason)
            .with_metadata("profile", self.profile.as_str());
        if let Err(error) = logger.write(event).await {
            tracing::error!(
                target: "autumn.fault_injection",
                %error,
                "failed to write the fault injection audit event"
            );
        }
    }
}

/// The faults that matched one request.
struct RequestScope {
    injector: Arc<Injector>,
    rules: Vec<CompiledRule>,
    fired: AtomicBool,
}

/// A route fault decision.
#[derive(Debug, Default)]
struct RouteFault {
    latency: Duration,
    error: Option<StatusCode>,
}

impl RequestScope {
    fn armed(&self) -> bool {
        self.injector.armed.load(Ordering::Acquire)
    }

    fn mark_fired(&self) {
        self.fired.store(true, Ordering::Relaxed);
        self.injector.injected.fetch_add(1, Ordering::Relaxed);
    }

    /// Roll each rule for `target`: the sum of the latencies, and an error if
    /// one fired.
    fn roll(&self, target: FaultTarget) -> RouteFault {
        let mut fault = RouteFault::default();
        if !self.armed() {
            return fault;
        }
        for rule in self.rules.iter().filter(|rule| rule.target == target) {
            if !self.injector.roll(rule.rate_ppm) {
                continue;
            }
            self.mark_fired();
            match rule.kind {
                FaultKind::Latency => fault.latency = fault.latency.saturating_add(rule.latency),
                FaultKind::Error => {
                    fault.error.get_or_insert(rule.status);
                }
            }
        }
        fault
    }
}

tokio::task_local! {
    static SCOPE: Arc<RequestScope>;
}

/// The dependency seam: wait for injected latency, then fail when an
/// injected error fires.
///
/// It does nothing outside a request with fault injection.
///
/// # Errors
///
/// Returns [`InjectedFault`] when an error fault for `target` fires.
pub(crate) async fn inject(target: FaultTarget) -> Result<(), InjectedFault> {
    let Ok(fault) = SCOPE.try_with(|scope| scope.roll(target)) else {
        return Ok(());
    };
    if !fault.latency.is_zero() {
        tokio::time::sleep(fault.latency).await;
    }
    match fault.error {
        Some(_) => Err(InjectedFault { target }),
        None => Ok(()),
    }
}

/// Build the two layers and the handle, or `None` when the section is
/// disabled or refused. It has no side effects; call [`announce`] when the
/// router is built.
pub(crate) fn build(
    config: &crate::config::AutumnConfig,
    state: &crate::state::AppState,
    exempt_paths: Vec<String>,
) -> Option<(FaultScopeLayer, FaultInjectionLayer, FaultInjection)> {
    let section = &config.fault_injection;
    if !section.enabled {
        return None;
    }
    let profile = config.profile.as_deref();
    // Config validation refuses this at boot. Check again here for a path that
    // does not validate (a test app, a hand-built config).
    if let Err(error) = section.validate(profile) {
        tracing::error!(
            target: "autumn.fault_injection",
            %error,
            "fault injection is not installed"
        );
        return None;
    }
    let actuator_prefix = crate::actuator::normalize_actuator_prefix(&config.actuator.prefix);
    let injector = Arc::new(Injector {
        rules: section.faults.iter().map(CompiledRule::new).collect(),
        exempt: Exempt {
            paths: exempt_paths,
            actuator_prefix_slash: format!("{actuator_prefix}/"),
            actuator_prefix,
        },
        max_error_ppm: section.stop.max_error_ppm(),
        window_len: Duration::from_secs(section.stop.window_secs),
        min_requests: section.stop.min_requests,
        armed: AtomicBool::new(true),
        injected: AtomicU64::new(0),
        window: Mutex::new(Window::default()),
        entropy: state.entropy_arc(),
        audit: state.extension::<AuditLogger>(),
        profile: profile.unwrap_or("dev").to_owned(),
        boot: Mutex::new(None),
    });
    let handle = FaultInjection {
        inner: Arc::clone(&injector),
    };
    Some((
        FaultScopeLayer::new(Arc::clone(&injector)),
        FaultInjectionLayer::new(),
        handle,
    ))
}

/// Write the boot toggle: an audit event and a `warn` log.
///
/// The audit write needs a runtime. Without one, the `warn` log is the
/// record.
pub(crate) fn announce(handle: &FaultInjection) {
    let injector = Arc::clone(&handle.inner);
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let task = runtime.spawn(async move {
            injector
                .write_audit(SYSTEM_ACTOR, true, "enabled by config")
                .await;
        });
        *handle
            .inner
            .boot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(task);
    } else {
        tracing::warn!(
            target: "autumn.fault_injection",
            action = "fault_injection.armed",
            "fault injection toggled"
        );
    }
}

/// Run `future` in a fault scope with `rules`, as a request to `/` would.
#[cfg(test)]
pub(crate) async fn with_faults<F: std::future::Future>(
    rules: &[FaultRule],
    future: F,
) -> F::Output {
    let injector = Arc::new(Injector {
        rules: rules.iter().map(CompiledRule::new).collect(),
        exempt: Exempt {
            paths: Vec::new(),
            actuator_prefix: "/actuator".to_owned(),
            actuator_prefix_slash: "/actuator/".to_owned(),
        },
        max_error_ppm: PPM,
        window_len: Duration::from_secs(60),
        min_requests: u64::MAX,
        armed: AtomicBool::new(true),
        injected: AtomicU64::new(0),
        window: Mutex::new(Window::default()),
        entropy: Arc::new(crate::entropy::SeededEntropy::new(1)),
        audit: None,
        profile: "test".to_owned(),
        boot: Mutex::new(None),
    });
    let scope = injector.scope_for("/").expect("an armed injector scopes `/`");
    SCOPE.scope(scope, future).await
}
