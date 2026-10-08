//! Opt-in fault injection for a staging environment (issue #3071).
//!
//! The `[fault_injection]` section declares faults. Each fault adds latency
//! or an error to a route or to a dependency, at a rate, on matched paths.
//! The router installs two layers when the section is enabled:
//!
//! - an outer scope layer, outside the exception filters and the session
//!   layer. It selects the faults
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::http::{Method, StatusCode};

use crate::audit::{AuditEvent, AuditLogger, AuditStatus};
use crate::entropy::Entropy;
use crate::route::RouteTimeout;
use crate::router::RouteAttrTable;
use crate::slo::PPM;

pub use config::{FaultInjectionConfig, FaultKind, FaultRule, FaultStopConfig, FaultTarget};
pub(crate) use layer::FaultScopeLayer;
pub use layer::{FaultInjectionLayer, FaultInjectionService};

/// The header on a response from an injected route error.
pub const FAULT_HEADER: &str = "x-autumn-fault";

/// Request extension: a route latency that the outer layer rolled.
///
/// The inner route layer waits for it, inside the request timeout of the
/// route. When no inner layer takes it (a cached page), the outer layer waits.
#[derive(Clone, Debug)]
struct DeferredLatency {
    latency: Duration,
    taken: Arc<AtomicBool>,
}

impl DeferredLatency {
    fn new(latency: Duration) -> Self {
        Self {
            latency,
            taken: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Takes the wait. Only the first call gets `true`.
    fn take(&self) -> bool {
        !self.taken.swap(true, Ordering::AcqRel)
    }
}

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
        is_armed(self.inner.state.load(Ordering::Acquire))
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
        // The sequence is taken under the window lock, as for every toggle,
        // so the sequence order is the toggle order.
        let sequence = {
            let window = self.inner.lock_window();
            let sequence = self.inner.disarm_any().then(|| self.inner.next_sequence());
            drop(window);
            sequence
        };
        if let Some(sequence) = sequence {
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
            let state = self.inner.state.load(Ordering::Acquire);
            if is_armed(state) {
                return;
            }
            *window = Window::default();
            // One store publishes the new generation and the armed bit
            // together, under the window lock that `record` also takes.
            self.inner.state.store(
                armed_state(generation_of(state).wrapping_add(1)),
                Ordering::Release,
            );
            let sequence = self.inner.next_sequence();
            drop(window);
            sequence
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
        // An exempt path also exempts its sub-paths, such as
        // `/loggers/{name}` under `/loggers` with the actuator at the root.
        if self.paths.iter().any(|exempt| under(path, exempt)) {
            return true;
        }
        if self.actuator_prefix.is_empty() {
            return false;
        }
        under(path, &self.actuator_prefix)
    }
}

/// `true` when `path` is `base` or a sub-path of it. An empty `base` (a
/// blank probe path) matches nothing.
fn under(path: &str, base: &str) -> bool {
    !base.is_empty()
        && path
            .strip_prefix(base)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The stop window.
#[derive(Debug, Default)]
struct Window {
    started: Option<tokio::time::Instant>,
    requests: u64,
    errors: u64,
}

/// The request timeout of a request: the route override, else
/// `server.timeouts.request_timeout_ms`, as the timeout layer sets it.
#[derive(Default)]
struct Deadlines {
    global: Option<Duration>,
    /// The route templates with an override, and the override per method.
    routes: matchit::Router<HashMap<Method, RouteTimeout>>,
}

impl Deadlines {
    fn new(global: Option<Duration>, overrides: &RouteAttrTable<RouteTimeout>) -> Self {
        let mut routes = matchit::Router::new();
        for (template, by_method) in overrides.iter() {
            if let Err(error) = routes.insert(template.as_str(), by_method.clone()) {
                // The route keeps the global timeout for its dependency waits.
                tracing::debug!(
                    target: "autumn.fault_injection",
                    template = %template,
                    %error,
                    "route timeout not used for fault deadlines"
                );
            }
        }
        Self { global, routes }
    }

    /// The request timeout of `method` on `path`. `None` means no timeout.
    fn for_request(&self, path: &str, method: &Method) -> Option<Duration> {
        let timeout = self
            .routes
            .at(path)
            .ok()
            .and_then(|found| found.value.get(method).copied())
            .unwrap_or_default();
        match timeout {
            RouteTimeout::Inherit => self.global,
            RouteTimeout::Override(limit) => Some(limit),
            RouteTimeout::Disabled => None,
        }
    }
}

struct Injector {
    /// At most `config::MAX_RULES` rules, so a `u64` mask selects them.
    rules: Vec<CompiledRule>,
    exempt: Exempt,
    max_error_ppm: u32,
    window_len: Duration,
    min_requests: u64,
    /// The arm state: the arm generation in the high bits, and the armed
    /// flag in bit 0. One word, so each transition is one atomic step.
    state: AtomicU64,
    injected: AtomicU64,
    window: Mutex<Window>,
    entropy: Arc<dyn Entropy>,
    audit: Option<Arc<AuditLogger>>,
    profile: String,
    allow_in_production: bool,
    /// The request timeouts: the cap of a dependency wait.
    deadlines: Deadlines,
    /// The pending boot audit write. The next toggle waits for it.
    boot: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// One audit write at a time.
    audit_order: tokio::sync::Mutex<()>,
    /// The toggle count. Each audit event has its number, so a reader can
    /// put the events in toggle order. The boot toggle is `0`.
    sequence: AtomicU64,
}

/// The state word of `generation`, armed.
const fn armed_state(generation: u64) -> u64 {
    generation.wrapping_shl(1) | 1
}

const fn is_armed(state: u64) -> bool {
    state & 1 == 1
}

const fn generation_of(state: u64) -> u64 {
    state.wrapping_shr(1)
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
    fn scope_for(self: &Arc<Self>, path: &str, method: &Method) -> Option<Arc<RequestScope>> {
        let state = self.state.load(Ordering::Acquire);
        if !is_armed(state) || self.exempt.contains(path) {
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
                path: path.into(),
                deadline_at: self.deadline_at(path, method),
                injector: Arc::clone(self),
                generation: generation_of(state),
                matched,
                fired: AtomicBool::new(false),
                errored: AtomicBool::new(false),
                route_rolled: AtomicBool::new(false),
            })
        })
    }

    /// When a request that starts now reaches the request timeout.
    fn deadline_at(&self, path: &str, method: &Method) -> Option<tokio::time::Instant> {
        self.deadlines
            .for_request(path, method)
            .and_then(|deadline| tokio::time::Instant::now().checked_add(deadline))
    }

    /// A scope that matches no rule, for a nested request with no rules.
    fn empty_scope(self: &Arc<Self>, path: &str) -> Arc<RequestScope> {
        Arc::new(RequestScope {
            path: path.into(),
            deadline_at: None,
            injector: Arc::clone(self),
            generation: generation_of(self.state.load(Ordering::Acquire)),
            matched: 0,
            fired: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            route_rolled: AtomicBool::new(false),
        })
    }

    /// Count one faulted request of arm `generation`. Returns the toggle
    /// sequence when this request trips the stop condition. A request of an
    /// earlier arm does not count.
    fn record(&self, generation: u64, error: bool) -> Option<u64> {
        let now = tokio::time::Instant::now();
        let mut window = self.lock_window();
        if generation_of(self.state.load(Ordering::Acquire)) != generation {
            return None;
        }
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
            return None;
        }
        let errors_ppm = u128::from(window.errors).saturating_mul(u128::from(PPM));
        let limit_ppm = u128::from(self.max_error_ppm).saturating_mul(u128::from(window.requests));
        // Disarm under the window lock, and only the arm of this request: a
        // re-arm between the check and the disarm cannot be undone.
        let tripped = (errors_ppm > limit_ppm && self.disarm_generation(generation))
            .then(|| self.next_sequence());
        drop(window);
        tripped
    }

    /// Disarm the current arm. Returns `true` when this call disarmed it.
    fn disarm_any(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        while is_armed(state) {
            match self.state.compare_exchange_weak(
                state,
                state & !1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => state = current,
            }
        }
        false
    }

    /// Disarm only when arm `generation` is current and armed.
    fn disarm_generation(&self, generation: u64) -> bool {
        let armed = armed_state(generation);
        self.state
            .compare_exchange(armed, armed & !1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
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
    fn audit_stop(self: &Arc<Self>, sequence: u64, reason: &'static str) {
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
    /// The request path that selected the rules.
    path: Box<str>,
    /// When the request timeout ends: one budget for all dependency waits.
    deadline_at: Option<tokio::time::Instant>,
    injector: Arc<Injector>,
    /// The arm of the injector when the request started.
    generation: u64,
    /// Bit `i` is set when rule `i` matches the path.
    matched: u64,
    /// A fault fired in this request.
    fired: AtomicBool,
    /// An error fault fired in this request.
    errored: AtomicBool,
    /// The route faults were rolled. A second route layer (the SSG/ISG
    /// path, or an MCP replay) does not roll them again.
    route_rolled: AtomicBool,
}

/// A fault decision.
#[derive(Debug, Default)]
struct RouteFault {
    latency: Duration,
    error: Option<StatusCode>,
}

impl RequestScope {
    /// `true` while the arm that started this request is current. A request
    /// of an earlier arm fires no more faults: its result does not count.
    fn armed(&self) -> bool {
        self.injector.state.load(Ordering::Acquire) == armed_state(self.generation)
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
        let mut fired = 0_u64;
        for (rule, _) in rules {
            if !self.injector.roll(rule.rate_ppm) {
                continue;
            }
            fired = fired.saturating_add(1);
            match rule.kind {
                FaultKind::Latency => fault.latency = fault.latency.saturating_add(rule.latency),
                FaultKind::Error => {
                    fault.error.get_or_insert(rule.status);
                }
            }
        }
        // Check the arm again before the decision counts: an arm change
        // during the loop drops it. The gap left is from here to the
        // injection, so a fault of an old arm almost never fires.
        if fired == 0 || !self.armed() {
            return RouteFault::default();
        }
        self.fired.store(true, Ordering::Relaxed);
        if fault.error.is_some() {
            self.errored.store(true, Ordering::Relaxed);
        }
        self.injector.injected.fetch_add(fired, Ordering::Relaxed);
        fault.latency = fault.latency.min(config::MAX_LATENCY);
        fault
    }
}

tokio::task_local! {
    static SCOPE: Arc<RequestScope>;
}

/// The fault decision of a dependency seam, with no wait and no error.
///
/// Capsule replay serves the recorded result of a seam, but must make the
/// same entropy draws as the capture did.
#[cfg(all(feature = "reporting", feature = "http-client"))]
pub(crate) fn replay_roll(target: FaultTarget) {
    let _decision = SCOPE.try_with(|scope| scope.roll(target));
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
    let Ok(mut fault) = SCOPE.try_with(|scope| {
        let mut fault = scope.roll(target);
        // A seam can be outside the timeout layer (the Redis session store
        // is). All waits of a request share the request timeout: a wait gets
        // at most the time left, then fails as a timeout would, so the stop
        // condition counts it.
        let left = scope
            .deadline_at
            .map(|at| at.saturating_duration_since(tokio::time::Instant::now()));
        if let Some(left) = left
            && !fault.latency.is_zero()
            && fault.latency >= left
        {
            fault.latency = left;
            fault.error = Some(StatusCode::SERVICE_UNAVAILABLE);
            scope.errored.store(true, Ordering::Relaxed);
        }
        fault
    }) else {
        return Ok(());
    };
    if !fault.latency.is_zero() {
        tokio::time::sleep(std::mem::take(&mut fault.latency)).await;
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
    route_timeouts: &RouteAttrTable<RouteTimeout>,
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
        state: AtomicU64::new(armed_state(0)),
        injected: AtomicU64::new(0),
        window: Mutex::new(Window::default()),
        entropy: state.entropy_arc(),
        audit,
        profile: profile.unwrap_or_default().to_owned(),
        allow_in_production: section.allow_in_production,
        deadlines: Deadlines::new(
            config
                .server
                .timeouts
                .request_timeout_ms
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis),
            route_timeouts,
        ),
        boot: Mutex::new(None),
        audit_order: tokio::sync::Mutex::new(()),
        sequence: AtomicU64::new(1),
    });
    Some((
        FaultScopeLayer::new(Arc::clone(&injector)),
        FaultInjectionLayer::new(None),
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

impl FaultInjection {
    /// Both layers again, on the same injector, for the SSG/ISG path. The
    /// route layer caps injected latency at `deadline` (the request
    /// timeout) and then fails with `503`, as the timeout layer does.
    pub(crate) fn layers(
        &self,
        deadline: Option<Duration>,
    ) -> (FaultScopeLayer, FaultInjectionLayer) {
        (
            FaultScopeLayer::new(Arc::clone(&self.inner)),
            FaultInjectionLayer::new(deadline),
        )
    }
}

#[cfg(test)]
impl FaultInjection {
    /// The handle of a test injector.
    fn for_test(inner: &Arc<Injector>) -> Self {
        Self {
            inner: Arc::clone(inner),
        }
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
        .scope_for("/", &Method::GET)
        .expect("an armed injector scopes `/`");
    SCOPE.scope(scope, future).await
}

/// [`with_faults`], and whether a fault fired in the scope.
#[cfg(test)]
pub(crate) async fn with_faults_fired<F: std::future::Future>(
    rules: &[FaultRule],
    future: F,
) -> (F::Output, bool) {
    let injector = test_injector(rules.iter().map(CompiledRule::new).collect(), u64::MAX);
    let scope = injector
        .scope_for("/", &Method::GET)
        .expect("an armed injector scopes `/`");
    let output = SCOPE.scope(Arc::clone(&scope), future).await;
    (output, scope.fired.load(Ordering::Relaxed))
}

/// An injector for tests: `/live` and `/actuator` are exempt, and the stop
/// trips above 14.4% errors.
#[cfg(test)]
fn test_injector(rules: Vec<CompiledRule>, min_requests: u64) -> Arc<Injector> {
    test_injector_with(rules, min_requests, Deadlines::default())
}

/// [`test_injector`] with a request timeout.
#[cfg(test)]
fn test_injector_with(
    rules: Vec<CompiledRule>,
    min_requests: u64,
    deadlines: Deadlines,
) -> Arc<Injector> {
    Arc::new(Injector {
        rules,
        exempt: Exempt {
            paths: vec!["/live".to_owned()],
            actuator_prefix: "/actuator".to_owned(),
        },
        max_error_ppm: 144_000,
        window_len: Duration::from_secs(60),
        min_requests,
        state: AtomicU64::new(armed_state(0)),
        injected: AtomicU64::new(0),
        window: Mutex::new(Window::default()),
        entropy: Arc::new(crate::entropy::SeededEntropy::new(7)),
        audit: None,
        profile: "test".to_owned(),
        allow_in_production: false,
        deadlines,
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
    fn an_exempt_path_exempts_its_sub_paths() {
        let exempt = Exempt {
            paths: vec!["/loggers".to_owned()],
            actuator_prefix: String::new(),
        };
        assert!(exempt.contains("/loggers/autumn_web"));
        assert!(!exempt.contains("/loggersx"));
    }

    #[test]
    fn a_blank_exempt_path_exempts_nothing() {
        let exempt = Exempt {
            paths: vec![String::new()],
            actuator_prefix: "/actuator".to_owned(),
        };
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
        assert!(injector.scope_for("/api/orders", &Method::GET).is_some());
        assert!(injector.scope_for("/other", &Method::GET).is_none());
    }

    #[test]
    fn latency_is_capped_per_request() {
        let mut rule = FaultRule::new(FaultTarget::Route, FaultKind::Latency, 1.0);
        rule.latency_ms = 300_000;
        let rules = vec![CompiledRule::new(&rule), CompiledRule::new(&rule)];
        let injector = test_injector(rules, 1);
        let scope = injector.scope_for("/", &Method::GET).unwrap();
        assert_eq!(scope.roll(FaultTarget::Route).latency, config::MAX_LATENCY);
    }

    /// A dependency wait is at most the request timeout, then fails: the
    /// Redis session store is outside the timeout layer.
    #[tokio::test(start_paused = true)]
    async fn a_dependency_wait_stops_at_the_request_timeout() {
        let mut rule = FaultRule::new(FaultTarget::Redis, FaultKind::Latency, 1.0);
        rule.latency_ms = 300_000;
        let injector = test_injector_with(
            vec![CompiledRule::new(&rule)],
            1_000,
            Deadlines::new(Some(Duration::from_secs(2)), &Arc::default()),
        );
        let scope = injector.scope_for("/", &Method::GET).unwrap();
        let started = tokio::time::Instant::now();
        let result = SCOPE
            .scope(Arc::clone(&scope), inject(FaultTarget::Redis))
            .await;
        assert!(result.is_err(), "the capped wait fails");
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert!(scope.errored.load(Ordering::Relaxed));
    }

    /// A route timeout override sets the dependency deadline of that route.
    #[tokio::test(start_paused = true)]
    async fn a_route_timeout_sets_the_dependency_deadline() {
        let mut rule = FaultRule::new(FaultTarget::Database, FaultKind::Latency, 1.0);
        rule.latency_ms = 2_000;
        let mut overrides = HashMap::new();
        for (template, timeout) in [
            ("/slow/{id}", RouteTimeout::Override(Duration::from_secs(5))),
            ("/off", RouteTimeout::Disabled),
        ] {
            overrides.insert(template.to_owned(), HashMap::from([(Method::GET, timeout)]));
        }
        let injector = test_injector_with(
            vec![CompiledRule::new(&rule)],
            1_000,
            Deadlines::new(Some(Duration::from_secs(1)), &Arc::new(overrides)),
        );
        for (path, method, ok) in [
            ("/slow/7", Method::GET, true),
            ("/off", Method::GET, true),
            ("/off", Method::POST, false),
            ("/other", Method::GET, false),
        ] {
            let scope = injector.scope_for(path, &method).unwrap();
            let result = SCOPE.scope(scope, inject(FaultTarget::Database)).await;
            assert_eq!(result.is_ok(), ok, "{method} {path}");
        }
    }

    /// The waits of one request share one budget: the second wait gets the
    /// time left, then fails.
    #[tokio::test(start_paused = true)]
    async fn dependency_waits_share_the_request_timeout() {
        let mut rule = FaultRule::new(FaultTarget::Redis, FaultKind::Latency, 1.0);
        rule.latency_ms = 900;
        let injector = test_injector_with(
            vec![CompiledRule::new(&rule)],
            1_000,
            Deadlines::new(Some(Duration::from_secs(1)), &Arc::default()),
        );
        let scope = injector.scope_for("/", &Method::GET).unwrap();
        let started = tokio::time::Instant::now();
        let (first, second) = SCOPE
            .scope(Arc::clone(&scope), async {
                (
                    inject(FaultTarget::Redis).await,
                    inject(FaultTarget::Redis).await,
                )
            })
            .await;
        assert!(first.is_ok());
        assert!(second.is_err(), "the second wait passes the budget");
        assert_eq!(started.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn arm_does_not_reset_an_armed_window() {
        let injector = test_injector(Vec::new(), 100);
        let handle = FaultInjection {
            inner: Arc::clone(&injector),
        };
        assert!(injector.record(0, true).is_none());
        handle.arm("ops").await;
        assert_eq!(handle.snapshot().window_requests, 1);
    }

    #[tokio::test]
    async fn a_request_of_an_earlier_arm_does_not_count_after_a_rearm() {
        let rule = FaultRule::new(FaultTarget::Route, FaultKind::Error, 1.0);
        let injector = test_injector(vec![CompiledRule::new(&rule)], 1);
        let handle = FaultInjection {
            inner: Arc::clone(&injector),
        };
        let old = injector.scope_for("/", &Method::GET).unwrap();
        handle.disarm("ops", "test").await;
        handle.arm("ops").await;
        assert!(
            injector.record(old.generation, true).is_none(),
            "the old request does not trip"
        );
        assert!(
            old.roll(FaultTarget::Route).error.is_none(),
            "the old request fires no more faults"
        );
        assert_eq!(handle.snapshot().window_requests, 0);
        assert!(handle.is_armed());
    }
}
