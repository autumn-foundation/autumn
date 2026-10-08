//! Opt-in fault injection for a staging environment (issue #3071).
//!
//! The `[fault_injection]` section declares faults. Each fault adds latency
//! or an error to a route or to a dependency, at a rate, on matched paths.
//! The router installs two layers when the section is enabled:
//!
//! - an outer scope layer, outside the session layer. It selects the faults
//!   for the request path, puts them in a task-local scope for the database,
//!   Redis and HTTP client seams, and counts the result for the stop
//!   condition;
//! - [`FaultInjectionLayer`], the innermost framework layer. It applies the
//!   route faults after rate limiting and load shedding. The access log,
//!   error reporting and the timeout see an injected fault as a real one.
//!
//! Safety rules:
//!
//! - Config validation fails, and the router does not install the layers,
//!   when the profile is `prod` or not set, unless `allow_in_production =
//!   true`.
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

/// The header on a response from an injected route error.
pub const FAULT_HEADER: &str = "x-autumn-fault";

/// The audit actor when the framework arms or disarms the faults.
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

/// A snapshot of the installed faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FaultSnapshot {
    /// `true` while faults can fire.
    pub armed: bool,
    /// The number of faults that fired since boot.
    pub injected: u64,
    /// The faulted requests in the current stop window.
    pub window_requests: u64,
    /// The bad results in the current stop window.
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
            let sequence = self.inner.next_sequence();
            self.inner.audit(sequence, actor, false, reason).await;
        }
    }

    /// Start the faults again, with an empty stop window, and write an audit
    /// event for `actor`.
    ///
    /// It does nothing when the faults are already armed.
    pub async fn arm(&self, actor: &str) {
        // Reset the window and arm under one lock, so no request counts into
        // the old window after the arm. Release the lock before the await.
        let sequence = {
            let mut window = self.inner.lock_window();
            if self.inner.armed.swap(true, Ordering::AcqRel) {
                return;
            }
            *window = Window::default();
            drop(window);
            self.inner.next_sequence()
        };
        self.inner
            .audit(sequence, actor, true, "armed by operator")
            .await;
    }
}

/// One fault rule, with its rate in ppm.
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
    /// The normalized actuator prefix. Empty when the actuator is at the
    /// root; then only `paths` are exempt.
    actuator_prefix: String,
}

impl Exempt {
    fn contains(&self, path: &str) -> bool {
        if self.paths.iter().any(|exempt| exempt == path) {
            return true;
        }
        if self.actuator_prefix.is_empty() {
            return false;
        }
        path.strip_prefix(self.actuator_prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
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
    /// At most `config::MAX_RULES` rules, so a `u64` mask selects them.
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
    allow_in_production: bool,
    /// The pending boot audit write. The next toggle waits for it.
    boot: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// One audit write at a time.
    audit_order: tokio::sync::Mutex<()>,
    /// The toggle count. Each audit event has its number, so a reader can
    /// put the events in toggle order. The boot toggle is `0`.
    sequence: AtomicU64,
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

    /// The scope for a request to `path`. `None` when the faults are
    /// disarmed, the path is exempt, or no rule matches it.
    fn scope_for(self: &Arc<Self>, path: &str) -> Option<Arc<RequestScope>> {
        if !self.armed.load(Ordering::Acquire) || self.exempt.contains(path) {
            return None;
        }
        let matched = self
            .rules
            .iter()
            .zip(0_u32..)
            .filter(|(rule, _)| rule.matches(path))
            .fold(0_u64, |mask, (_, bit)| {
                mask | 1_u64.checked_shl(bit).unwrap_or(0)
            });
        (matched != 0).then(|| {
            Arc::new(RequestScope {
                injector: Arc::clone(self),
                matched,
                fired: AtomicBool::new(false),
                errored: AtomicBool::new(false),
            })
        })
    }

    /// Count one faulted request. Returns `true` when this request trips the
    /// stop condition.
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

    /// The number of the next toggle.
    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::AcqRel)
    }

    /// Write a toggle, after the boot toggle.
    async fn audit(&self, sequence: u64, actor: &str, armed: bool, reason: &str) {
        let boot = self
            .boot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(boot) = boot {
            // A failed boot write is already logged; continue.
            let _ = boot.await;
        }
        self.write_audit(sequence, actor, armed, reason).await;
    }

    async fn write_audit(&self, sequence: u64, actor: &str, armed: bool, reason: &str) {
        let _order = self.audit_order.lock().await;
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
            sequence,
            profile = %self.profile,
            allow_in_production = self.allow_in_production,
            "fault injection toggled"
        );
        let Some(logger) = &self.audit else {
            return;
        };
        let event = AuditEvent::new(actor, action, "fault_injection", None, AuditStatus::Success)
            .with_metadata("reason", reason)
            .with_metadata("sequence", sequence.to_string())
            .with_metadata("profile", self.profile.as_str())
            .with_metadata(
                "allow_in_production",
                if self.allow_in_production {
                    "true"
                } else {
                    "false"
                },
            );
        if let Err(error) = logger.write(event).await {
            tracing::error!(
                target: "autumn.fault_injection",
                %error,
                "failed to write the fault injection audit event"
            );
        }
    }

    /// Write a stop trip in the background. The request does not wait for
    /// the audit sink, and a dropped request cannot lose the event.
    fn audit_stop(self: &Arc<Self>, reason: &'static str) {
        let sequence = self.next_sequence();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let injector = Arc::clone(self);
            runtime.spawn(async move {
                injector.audit(sequence, SYSTEM_ACTOR, false, reason).await;
            });
        } else {
            tracing::warn!(
                target: "autumn.fault_injection",
                actor = SYSTEM_ACTOR,
                action = "fault_injection.disarmed",
                reason,
                sequence,
                profile = %self.profile,
                "fault injection toggled; no runtime, so no audit event"
            );
        }
    }
}

/// The faults that matched one request.
struct RequestScope {
    injector: Arc<Injector>,
    /// Bit `i` is set when rule `i` matches the path.
    matched: u64,
    /// A fault fired in this request.
    fired: AtomicBool,
    /// An error fault fired in this request.
    errored: AtomicBool,
}

/// A fault decision.
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

    /// Test each matched rule for `target` against its rate. Return the sum
    /// of the latencies (capped), and an error if one fired.
    fn roll(&self, target: FaultTarget) -> RouteFault {
        let mut fault = RouteFault::default();
        if !self.armed() {
            return fault;
        }
        let rules = self
            .injector
            .rules
            .iter()
            .zip(0_u32..)
            .filter(|(rule, bit)| {
                rule.target == target && self.matched & 1_u64.checked_shl(*bit).unwrap_or(0) != 0
            });
        for (rule, _) in rules {
            if !self.injector.roll(rule.rate_ppm) {
                continue;
            }
            self.mark_fired();
            match rule.kind {
                FaultKind::Latency => fault.latency = fault.latency.saturating_add(rule.latency),
                FaultKind::Error => {
                    self.errored.store(true, Ordering::Relaxed);
                    fault.error.get_or_insert(rule.status);
                }
            }
        }
        fault.latency = fault.latency.min(config::MAX_LATENCY);
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

/// The database seam for generated repositories. Not public API.
///
/// # Errors
///
/// Returns a `503` when an injected `database` error fires.
#[doc(hidden)]
pub async fn __database_fault() -> crate::AutumnResult<()> {
    inject(FaultTarget::Database)
        .await
        .map_err(|fault| crate::AutumnError::service_unavailable_msg(fault.to_string()))
}

/// Build the two layers and the handle, or `None` when the section is
/// disabled or refused. It has no side effects. Call [`announce`] after the
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
    let audit = state.extension::<AuditLogger>();
    if audit.is_none() {
        tracing::warn!(
            target: "autumn.fault_injection",
            "fault injection has no audit sink; toggles go to the log only. \
             Install one with `AppBuilder::with_audit_sink`"
        );
    }
    let injector = Arc::new(Injector {
        rules: section.faults.iter().map(CompiledRule::new).collect(),
        exempt: Exempt {
            paths: exempt_paths,
            actuator_prefix: crate::actuator::normalize_actuator_prefix(&config.actuator.prefix),
        },
        max_error_ppm: section.stop.max_error_ppm(),
        window_len: Duration::from_secs(section.stop.window_secs),
        min_requests: section.stop.min_requests,
        armed: AtomicBool::new(true),
        injected: AtomicU64::new(0),
        window: Mutex::new(Window::default()),
        entropy: state.entropy_arc(),
        audit,
        profile: profile.unwrap_or_default().to_owned(),
        allow_in_production: section.allow_in_production,
        boot: Mutex::new(None),
        audit_order: tokio::sync::Mutex::new(()),
        sequence: AtomicU64::new(1),
    });
    Some((
        FaultScopeLayer::new(Arc::clone(&injector)),
        FaultInjectionLayer::new(),
        FaultInjection { inner: injector },
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
                .write_audit(0, SYSTEM_ACTOR, true, "enabled by config")
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
            actor = SYSTEM_ACTOR,
            action = "fault_injection.armed",
            reason = "enabled by config",
            profile = %injector.profile,
            allow_in_production = injector.allow_in_production,
            "fault injection toggled; no runtime, so no audit event"
        );
    }
}

/// Run `future` in a fault scope with `rules`, as a request to `/` would.
#[cfg(test)]
pub(crate) async fn with_faults<F: std::future::Future>(
    rules: &[FaultRule],
    future: F,
) -> F::Output {
    let injector = test_injector(rules.iter().map(CompiledRule::new).collect(), u64::MAX);
    let scope = injector
        .scope_for("/")
        .expect("an armed injector scopes `/`");
    SCOPE.scope(scope, future).await
}

/// An injector for tests: `/live` and `/actuator` are exempt, and the stop
/// trips above 14.4% errors.
#[cfg(test)]
fn test_injector(rules: Vec<CompiledRule>, min_requests: u64) -> Arc<Injector> {
    Arc::new(Injector {
        rules,
        exempt: Exempt {
            paths: vec!["/live".to_owned()],
            actuator_prefix: "/actuator".to_owned(),
        },
        max_error_ppm: 144_000,
        window_len: Duration::from_secs(60),
        min_requests,
        armed: AtomicBool::new(true),
        injected: AtomicU64::new(0),
        window: Mutex::new(Window::default()),
        entropy: Arc::new(crate::entropy::SeededEntropy::new(7)),
        audit: None,
        profile: "test".to_owned(),
        allow_in_production: false,
        boot: Mutex::new(None),
        audit_order: tokio::sync::Mutex::new(()),
        sequence: AtomicU64::new(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_actuator_prefix_does_not_exempt_every_path() {
        let exempt = Exempt {
            paths: vec!["/health".to_owned()],
            actuator_prefix: String::new(),
        };
        assert!(exempt.contains("/health"));
        assert!(!exempt.contains("/api/orders"));
    }

    #[test]
    fn the_actuator_prefix_matches_on_a_segment_boundary() {
        let exempt = Exempt {
            paths: Vec::new(),
            actuator_prefix: "/actuator".to_owned(),
        };
        assert!(exempt.contains("/actuator"));
        assert!(exempt.contains("/actuator/metrics"));
        assert!(!exempt.contains("/actuators"));
    }

    #[test]
    fn a_path_with_no_matching_rule_gets_no_scope() {
        let mut rule = FaultRule::new(FaultTarget::Route, FaultKind::Error, 1.0);
        rule.routes = vec!["/api/*".to_owned()];
        let injector = test_injector(vec![CompiledRule::new(&rule)], 1);
        assert!(injector.scope_for("/api/orders").is_some());
        assert!(injector.scope_for("/other").is_none());
    }

    #[test]
    fn latency_is_capped_per_request() {
        let mut rule = FaultRule::new(FaultTarget::Route, FaultKind::Latency, 1.0);
        rule.latency_ms = 300_000;
        let rules = vec![CompiledRule::new(&rule), CompiledRule::new(&rule)];
        let injector = test_injector(rules, 1);
        let scope = injector.scope_for("/").unwrap();
        assert_eq!(scope.roll(FaultTarget::Route).latency, config::MAX_LATENCY);
    }

    #[tokio::test]
    async fn arm_does_not_reset_an_armed_window() {
        let injector = test_injector(Vec::new(), 100);
        let handle = FaultInjection {
            inner: Arc::clone(&injector),
        };
        assert!(!injector.record(true));
        handle.arm("ops").await;
        assert_eq!(handle.snapshot().window_requests, 1);
    }
}
