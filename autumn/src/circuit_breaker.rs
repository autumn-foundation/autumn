#![allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::missing_const_for_fn,
    clippy::items_after_statements,
    clippy::cast_precision_loss,
    clippy::collapsible_if
)]
// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::Open => "OPEN",
            Self::HalfOpen => "HALF_OPEN",
        }
    }
}

/// What a call cancelled at or after the slow-call threshold counts as.
///
/// A call cancelled before the threshold counts as nothing: the caller left,
/// and the dependency was not slow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CancelledCallOutcome {
    /// A slow call that did not fail.
    #[default]
    Slow,
    /// A slow call that failed.
    Failure,
}

impl CancelledCallOutcome {
    /// The config spelling: `slow` or `failure`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Failure => "failure",
        }
    }
}

impl std::str::FromStr for CancelledCallOutcome {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "slow" => Ok(Self::Slow),
            "failure" => Ok(Self::Failure),
            other => Err(format!(
                "unknown cancelled_call_outcome {other:?}: use \"slow\" or \"failure\""
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CircuitBreakerPolicy {
    pub failure_ratio_threshold: f64,
    pub sample_window: Duration,
    pub minimum_sample_count: u64,
    pub open_duration: Duration,
    pub half_open_trial_count: u64,
    /// A call that takes this long or longer is slow. `None` turns slow-call
    /// detection off. `Some(Duration::ZERO)` makes all calls slow.
    pub slow_call_duration_threshold: Option<Duration>,
    /// The breaker opens when this share of calls in the window is slow.
    pub slow_call_rate_threshold: f64,
    /// What a call cancelled at or after the slow-call threshold counts as.
    pub cancelled_call_outcome: CancelledCallOutcome,
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            failure_ratio_threshold: 0.5,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 10,
            open_duration: Duration::from_secs(60),
            half_open_trial_count: 3,
            slow_call_duration_threshold: Some(Duration::from_secs(60)),
            slow_call_rate_threshold: 1.0,
            cancelled_call_outcome: CancelledCallOutcome::Slow,
        }
    }
}

impl CircuitBreakerPolicy {
    pub fn from_config(rc: &crate::config::ResilienceConfig, name: &str) -> Self {
        let mut policy = Self::default();
        policy.apply(&rc.circuit_breaker.defaults);
        if let Some(host_cfg) = rc.circuit_breaker.hosts.get(name) {
            policy.apply(host_cfg);
        }
        policy
    }

    /// Sets each field that `cfg` sets.
    fn apply(&mut self, cfg: &crate::config::CircuitBreakerPolicyConfig) {
        if let Some(ratio) = cfg.failure_ratio_threshold {
            self.failure_ratio_threshold = clamp_ratio(ratio);
        }
        if let Some(window) = cfg.sample_window_secs {
            self.sample_window = Duration::from_secs(window);
        }
        if let Some(count) = cfg.minimum_sample_count {
            self.minimum_sample_count = count;
        }
        if let Some(duration) = cfg.open_duration_secs {
            self.open_duration = Duration::from_secs(duration);
        }
        if let Some(trials) = cfg.half_open_trial_count {
            self.half_open_trial_count = trials.max(1);
        }
        if let Some(ms) = cfg.slow_call_duration_threshold_ms {
            self.slow_call_duration_threshold = (ms > 0).then(|| Duration::from_millis(ms));
        }
        if let Some(ratio) = cfg.slow_call_rate_threshold {
            self.slow_call_rate_threshold = clamp_ratio(ratio);
        }
        if let Some(outcome) = cfg.cancelled_call_outcome {
            self.cancelled_call_outcome = outcome;
        }
    }

    /// Clamps both ratio thresholds into `(0, 1]`.
    fn clamped(mut self) -> Self {
        self.failure_ratio_threshold = clamp_ratio(self.failure_ratio_threshold);
        self.slow_call_rate_threshold = clamp_ratio(self.slow_call_rate_threshold);
        self
    }

    /// True when a call that took `elapsed` is slow.
    fn is_slow(&self, elapsed: Duration) -> bool {
        self.slow_call_duration_threshold
            .is_some_and(|threshold| elapsed >= threshold)
    }
}

/// A threshold of `0` opens the breaker on each call. The minimum is `0.0001`.
fn clamp_ratio(ratio: f64) -> f64 {
    ratio.clamp(0.000_1, 1.0)
}

#[derive(Debug, Error)]
pub enum CircuitBreakerError<E> {
    #[error("circuit breaker is open")]
    Open,
    #[error("execution failed: {0}")]
    Execution(E),
}

/// The number of buckets in a [`SampleWindow`].
pub(crate) const WINDOW_BUCKETS: usize = 10;

/// Call counters for one bucket or for the full window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WindowCounts {
    pub(crate) total: u64,
    pub(crate) failed: u64,
    pub(crate) slow: u64,
}

impl WindowCounts {
    fn add(&mut self, other: Self) {
        self.total = self.total.saturating_add(other.total);
        self.failed = self.failed.saturating_add(other.failed);
        self.slow = self.slow.saturating_add(other.slow);
    }

    pub(crate) fn failure_ratio(self) -> f64 {
        ratio(self.failed, self.total)
    }

    pub(crate) fn slow_call_ratio(self) -> f64 {
        ratio(self.slow, self.total)
    }
}

fn ratio(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    /// The time slice this bucket holds: `(now - origin) / width`.
    epoch: u64,
    counts: WindowCounts,
}

/// A fixed ring of counter buckets over the sample window.
///
/// Each bucket holds the calls that ended in one slice of
/// `window / WINDOW_BUCKETS`. The window is the current slice and the
/// `WINDOW_BUCKETS - 1` slices before it. Memory does not change with the
/// call rate.
#[derive(Debug, Clone)]
pub(crate) struct SampleWindow {
    origin: Instant,
    width_nanos: u128,
    buckets: [Bucket; WINDOW_BUCKETS],
}

impl SampleWindow {
    pub(crate) fn new(window: Duration, origin: Instant) -> Self {
        Self {
            origin,
            width_nanos: (window.as_nanos() / WINDOW_BUCKETS as u128).max(1),
            buckets: [Bucket::default(); WINDOW_BUCKETS],
        }
    }

    fn epoch(&self, now: Instant) -> u64 {
        let slice = now.saturating_duration_since(self.origin).as_nanos() / self.width_nanos;
        u64::try_from(slice).unwrap_or(u64::MAX)
    }

    fn slot(epoch: u64) -> usize {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the modulus is < WINDOW_BUCKETS"
        )]
        let slot = (epoch % WINDOW_BUCKETS as u64) as usize;
        slot
    }

    pub(crate) fn record(&mut self, now: Instant, failed: bool, slow: bool) {
        let epoch = self.epoch(now);
        // The clock went back: the old buckets are not in this timeline.
        if self.buckets.iter().any(|b| b.epoch > epoch) {
            self.buckets = [Bucket::default(); WINDOW_BUCKETS];
        }
        let bucket = &mut self.buckets[Self::slot(epoch)];
        if bucket.epoch != epoch {
            *bucket = Bucket {
                epoch,
                counts: WindowCounts::default(),
            };
        }
        bucket.counts.add(WindowCounts {
            total: 1,
            failed: u64::from(failed),
            slow: u64::from(slow),
        });
    }

    pub(crate) fn counts(&self, now: Instant) -> WindowCounts {
        let epoch = self.epoch(now);
        let mut sum = WindowCounts::default();
        for bucket in &self.buckets {
            // Skip a bucket after `now`: the clock went back.
            if bucket.epoch <= epoch && epoch - bucket.epoch < WINDOW_BUCKETS as u64 {
                sum.add(bucket.counts);
            }
        }
        sum
    }

    pub(crate) fn reset(&mut self, window: Duration, origin: Instant) {
        *self = Self::new(window, origin);
    }

    /// Changes the window length. The counts go into the current bucket, so
    /// a change does not clear them. They stay for one new window length.
    pub(crate) fn resize(&mut self, window: Duration, now: Instant) {
        let counts = self.counts(now);
        *self = Self::new(window, self.origin);
        let epoch = self.epoch(now);
        self.buckets[Self::slot(epoch)] = Bucket { epoch, counts };
    }
}

#[derive(Clone)]
pub struct CircuitBreaker {
    name: String,
    pub(crate) inner: Arc<Mutex<CircuitBreakerInner>>,
    /// Read the system clock, never a `Sim`'s. Set for breakers in the
    /// process-global registry: they outlive any `Sim`, so a virtual instant
    /// stored in one would later be compared with real time (issue #2967).
    system_clock: bool,
}

pub(crate) struct CircuitBreakerInner {
    pub(crate) state: CircuitState,
    pub(crate) window: SampleWindow,
    pub(crate) open_until: Option<Instant>,
    pub(crate) half_open_successes: u64,
    pub(crate) half_open_failures: u64,
    pub(crate) half_open_in_flight: u64,
    pub(crate) config: CircuitBreakerPolicy,
    /// Slow calls since the breaker was made, in all states.
    pub(crate) slow_calls_total: u64,
    /// Goes up at each state change. A call that started in an older
    /// generation does not change the window or the state.
    pub(crate) generation: u64,
}

impl CircuitBreakerInner {
    /// Moves an expired `Open` breaker to `HalfOpen`.
    fn half_open_if_due(&mut self, name: &str, now: Instant) {
        if self.state == CircuitState::Open && self.open_until.is_some_and(|until| now >= until) {
            self.half_open_successes = 0;
            self.half_open_failures = 0;
            self.half_open_in_flight = 0;
            self.open_until = None;
            self.transition_to(name, CircuitState::HalfOpen, 1.0, 0.0);
        }
    }

    /// Records a state transition and logs it.
    ///
    /// Must be the *last* mutation of every transition: callers set all
    /// associated fields (`open_until`, half-open counters, window) before
    /// calling this. The `tracing` call below can panic in a user-provided
    /// subscriber, and because [`CircuitBreaker::lock_inner`] recovers
    /// poisoned state, a mid-transition panic must still leave the breaker
    /// fully consistent (the state write below precedes the log, so the new
    /// state is complete by the time anything can panic).
    fn transition_to(
        &mut self,
        name: &str,
        new_state: CircuitState,
        failure_ratio: f64,
        slow_call_ratio: f64,
    ) {
        let old_state = self.state;
        self.state = new_state;
        self.generation = self.generation.wrapping_add(1);
        tracing::info!(
            circuit.name = name,
            circuit.state = new_state.as_str(),
            circuit.failure_ratio = failure_ratio,
            circuit.slow_call_ratio = slow_call_ratio,
            "circuit breaker state transition from {:?} to {:?}",
            old_state,
            new_state
        );
    }

    /// Counts one finished or cancelled call, and trips or closes the breaker.
    ///
    /// `generation` is the generation when the call started. A call from an
    /// older generation only adds to `slow_calls_total`. For example, a call
    /// that started before the breaker opened is not a half-open trial.
    fn record(&mut self, name: &str, now: Instant, generation: u64, failed: bool, slow: bool) {
        if slow {
            self.slow_calls_total = self.slow_calls_total.saturating_add(1);
        }
        if generation != self.generation {
            return;
        }
        match self.state {
            CircuitState::Closed => {
                self.window.record(now, failed, slow);
                let counts = self.window.counts(now);
                if counts.total < self.config.minimum_sample_count {
                    return;
                }
                let failure_ratio = counts.failure_ratio();
                let slow_call_ratio = counts.slow_call_ratio();
                let too_slow = self.config.slow_call_duration_threshold.is_some()
                    && slow_call_ratio >= self.config.slow_call_rate_threshold;
                if failure_ratio >= self.config.failure_ratio_threshold || too_slow {
                    self.open_until = Some(crate::time_math::saturating_deadline(
                        now,
                        self.config.open_duration,
                    ));
                    self.transition_to(name, CircuitState::Open, failure_ratio, slow_call_ratio);
                }
            }
            CircuitState::HalfOpen => {
                self.half_open_in_flight = self.half_open_in_flight.saturating_sub(1);
                // A slow trial is a failed trial: the dependency has not
                // recovered.
                if failed || slow {
                    self.half_open_failures += 1;
                    self.open_until = Some(crate::time_math::saturating_deadline(
                        now,
                        self.config.open_duration,
                    ));
                    let failure_ratio = if failed { 1.0 } else { 0.0 };
                    let slow_call_ratio = if slow { 1.0 } else { 0.0 };
                    self.transition_to(name, CircuitState::Open, failure_ratio, slow_call_ratio);
                } else {
                    self.half_open_successes += 1;
                    if self.half_open_successes >= self.config.half_open_trial_count {
                        let window = self.config.sample_window;
                        self.window.reset(window, now);
                        self.transition_to(name, CircuitState::Closed, 0.0, 0.0);
                    }
                }
            }
            CircuitState::Open => {}
        }
    }
}

impl CircuitBreaker {
    pub fn new(name: impl Into<String>, config: CircuitBreakerPolicy) -> Self {
        Self::with_clock(name, config, false)
    }

    /// A breaker on the system clock. See the `system_clock` field.
    fn new_on_system_clock(name: impl Into<String>, config: CircuitBreakerPolicy) -> Self {
        Self::with_clock(name, config, true)
    }

    fn with_clock(
        name: impl Into<String>,
        config: CircuitBreakerPolicy,
        system_clock: bool,
    ) -> Self {
        let config = config.clamped();
        let origin = read_clock(system_clock);
        Self {
            name: name.into(),
            inner: Arc::new(Mutex::new(CircuitBreakerInner {
                state: CircuitState::Closed,
                window: SampleWindow::new(config.sample_window, origin),
                open_until: None,
                half_open_successes: 0,
                half_open_failures: 0,
                half_open_in_flight: 0,
                config,
                slow_calls_total: 0,
                generation: 0,
            })),
            system_clock,
        }
    }

    /// The current instant on this breaker's clock.
    fn now(&self) -> Instant {
        read_clock(self.system_clock)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Locks the inner state, recovering from a poisoned mutex.
    ///
    /// Circuit breaker state is simple and self-correcting (a ring of window
    /// counters plus state counters), so the data behind a poisoned lock is
    /// still safe to use. Recovering here keeps a single panicking lock
    /// holder from permanently poisoning the breaker and panicking every
    /// subsequent call.
    fn lock_inner(&self) -> MutexGuard<'_, CircuitBreakerInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state(&self) -> CircuitState {
        let mut inner = self.lock_inner();
        inner.half_open_if_due(&self.name, self.now());
        inner.state
    }

    pub fn config(&self) -> CircuitBreakerPolicy {
        let inner = self.lock_inner();
        inner.config.clone()
    }

    /// Replaces the policy. A new `sample_window` keeps the counts. See
    /// [`SampleWindow::resize`].
    pub fn update_config(&self, config: CircuitBreakerPolicy) {
        let config = config.clamped();
        let now = self.now();
        let mut inner = self.lock_inner();
        if inner.config.sample_window != config.sample_window {
            inner.window.resize(config.sample_window, now);
        }
        inner.config = config;
    }

    fn window_counts(&self) -> WindowCounts {
        let now = self.now();
        self.lock_inner().window.counts(now)
    }

    pub fn failure_ratio(&self) -> f64 {
        self.window_counts().failure_ratio()
    }

    /// The share of slow calls in the current window.
    pub fn slow_call_ratio(&self) -> f64 {
        self.window_counts().slow_call_ratio()
    }

    /// Slow calls since the breaker was made. Cancelled slow calls are
    /// included.
    pub fn slow_calls_total(&self) -> u64 {
        self.lock_inner().slow_calls_total
    }

    /// Admits one call, or refuses it. Gives the generation of the state
    /// that admitted the call.
    #[allow(clippy::significant_drop_tightening)]
    pub(crate) fn before_call(&self) -> Result<u64, CircuitBreakerError<()>> {
        let mut inner = self.lock_inner();
        inner.half_open_if_due(&self.name, self.now());

        match inner.state {
            CircuitState::Open => Err(CircuitBreakerError::Open),
            CircuitState::HalfOpen => {
                let trial_count = inner.config.half_open_trial_count;
                if inner.half_open_successes + inner.half_open_in_flight >= trial_count {
                    Err(CircuitBreakerError::Open)
                } else {
                    inner.half_open_in_flight += 1;
                    Ok(inner.generation)
                }
            }
            CircuitState::Closed => Ok(inner.generation),
        }
    }

    /// Admits one call and gives its guard. The guard keeps the generation
    /// that admitted the call, so a state change after the admission does
    /// not move the call into the new state.
    pub(crate) fn admit(&self) -> Result<CircuitBreakerGuard, CircuitBreakerError<()>> {
        let generation = self.before_call()?;
        Ok(CircuitBreakerGuard::admitted(self.clone(), generation))
    }

    /// Counts a finished call that was not slow.
    #[cfg(test)]
    pub(crate) fn after_call(&self, success: bool) {
        let generation = self.lock_inner().generation;
        self.finish_call(generation, !success, Duration::ZERO);
    }

    fn generation(&self) -> u64 {
        self.lock_inner().generation
    }

    /// Counts a finished call that took `elapsed`.
    fn finish_call(&self, generation: u64, failed: bool, elapsed: Duration) {
        let now = self.now();
        let mut inner = self.lock_inner();
        let slow = inner.config.is_slow(elapsed);
        inner.record(&self.name, now, generation, failed, slow);
    }

    /// Counts a call that was dropped after `elapsed`, before it finished.
    fn cancel_call(&self, generation: u64, elapsed: Duration) {
        let now = self.now();
        let mut inner = self.lock_inner();
        // During a panic, do not change the state: a log in a panicking
        // subscriber would abort the process.
        if inner.config.is_slow(elapsed) && !std::thread::panicking() {
            let failed = inner.config.cancelled_call_outcome == CancelledCallOutcome::Failure;
            inner.record(&self.name, now, generation, failed, true);
        } else if inner.state == CircuitState::HalfOpen && inner.generation == generation {
            // Count nothing, but free the half-open slot.
            inner.half_open_in_flight = inner.half_open_in_flight.saturating_sub(1);
        }
    }

    pub async fn run<F, T, E>(&self, fut: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: Future<Output = Result<T, E>>,
    {
        let guard = self.admit().map_err(|_| CircuitBreakerError::Open)?;

        let res = fut.await;

        match &res {
            Ok(_) => guard.success(),
            Err(_) => guard.failure(),
        }

        res.map_err(CircuitBreakerError::Execution)
    }

    pub async fn run_with_fallback<F, T, E, FB>(&self, fut: F, fallback: FB) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
        FB: FnOnce(CircuitBreakerError<E>) -> Result<T, E>,
    {
        match self.run(fut).await {
            Ok(val) => Ok(val),
            Err(err) => fallback(err),
        }
    }
}

/// The current instant on the system clock or the ambient clock.
fn read_clock(system_clock: bool) -> Instant {
    if system_clock {
        crate::time::system_instant()
    } else {
        crate::time::ambient_instant()
    }
}

/// Counts one call. It keeps the start instant, so the breaker can see a slow
/// call. Drop it before `success` or `failure` to count a cancelled call.
pub struct CircuitBreakerGuard {
    breaker: CircuitBreaker,
    started: Instant,
    generation: u64,
    completed: bool,
}

impl CircuitBreakerGuard {
    pub fn new(breaker: CircuitBreaker) -> Self {
        let generation = breaker.generation();
        Self::admitted(breaker, generation)
    }

    fn admitted(breaker: CircuitBreaker, generation: u64) -> Self {
        Self {
            started: breaker.now(),
            generation,
            breaker,
            completed: false,
        }
    }

    fn elapsed(&self) -> Duration {
        self.breaker.now().saturating_duration_since(self.started)
    }

    pub fn success(mut self) {
        self.completed = true;
        self.breaker
            .finish_call(self.generation, false, self.elapsed());
    }

    pub fn failure(mut self) {
        self.completed = true;
        self.breaker
            .finish_call(self.generation, true, self.elapsed());
    }
}

impl Drop for CircuitBreakerGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.breaker.cancel_call(self.generation, self.elapsed());
        }
    }
}

pub struct CircuitBreakerRegistry {
    breakers: Mutex<HashMap<String, CircuitBreaker>>,
    /// Create breakers on the system clock. True only for the process-global
    /// registry.
    system_clock: bool,
}

impl CircuitBreakerRegistry {
    pub fn new() -> Self {
        Self {
            breakers: Mutex::new(HashMap::new()),
            system_clock: false,
        }
    }

    /// Create a breaker for this registry's clock.
    fn create(&self, name: &str, config: CircuitBreakerPolicy) -> CircuitBreaker {
        if self.system_clock {
            CircuitBreaker::new_on_system_clock(name, config)
        } else {
            CircuitBreaker::new(name, config)
        }
    }

    /// Locks the registry map, recovering from a poisoned mutex.
    ///
    /// See [`CircuitBreaker::lock_inner`] for the rationale: the map is
    /// always left in a consistent state, so a panicking lock holder must
    /// not permanently break every subsequent registry call.
    fn lock_breakers(&self) -> MutexGuard<'_, HashMap<String, CircuitBreaker>> {
        self.breakers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get_or_create(&self, name: &str, config: CircuitBreakerPolicy) -> CircuitBreaker {
        let mut breakers = self.lock_breakers();
        breakers
            .entry(name.to_owned())
            .or_insert_with(|| self.create(name, config))
            .clone()
    }

    pub fn get_or_create_with_config(
        &self,
        name: &str,
        config: CircuitBreakerPolicy,
    ) -> CircuitBreaker {
        let mut breakers = self.lock_breakers();
        if let Some(breaker) = breakers.get(name) {
            breaker.update_config(config);
            breaker.clone()
        } else {
            let breaker = self.create(name, config);
            breakers.insert(name.to_owned(), breaker.clone());
            breaker
        }
    }

    /// Returns a list of all currently registered circuit breakers.
    pub fn all_breakers(&self) -> Vec<CircuitBreaker> {
        let breakers = self.lock_breakers();
        breakers.values().cloned().collect()
    }

    /// Clears all registered circuit breakers from the registry.
    pub fn clear(&self) {
        let mut breakers = self.lock_breakers();
        breakers.clear();
    }
}

static REGISTRY: std::sync::OnceLock<CircuitBreakerRegistry> = std::sync::OnceLock::new();

/// The process-global registry. Its breakers read the system clock, because
/// they outlive any `Sim` (issue #2967).
pub fn global_registry() -> &'static CircuitBreakerRegistry {
    REGISTRY.get_or_init(|| CircuitBreakerRegistry {
        system_clock: true,
        ..CircuitBreakerRegistry::new()
    })
}

pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone)]
pub struct CircuitBreakerLayer {
    breaker: CircuitBreaker,
}

impl CircuitBreakerLayer {
    #[must_use]
    pub const fn new(breaker: CircuitBreaker) -> Self {
        Self { breaker }
    }
}

impl<S> tower::Layer<S> for CircuitBreakerLayer {
    type Service = CircuitBreakerService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CircuitBreakerService {
            inner,
            breaker: self.breaker.clone(),
        }
    }
}

#[derive(Clone)]
pub struct CircuitBreakerService<S> {
    inner: S,
    breaker: CircuitBreaker,
}

pin_project_lite::pin_project! {
    #[project = CircuitBreakerServiceFutureProj]
    pub enum CircuitBreakerServiceFuture<F> {
        Executing {
            #[pin]
            fut: F,
            guard: Option<CircuitBreakerGuard>,
        },
        Open,
    }
}

impl<F, T, E> std::future::Future for CircuitBreakerServiceFuture<F>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    type Output = Result<T, CircuitBreakerError<E>>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.project() {
            CircuitBreakerServiceFutureProj::Executing { fut, guard } => match fut.poll(cx) {
                std::task::Poll::Ready(Ok(val)) => {
                    if let Some(g) = guard.take() {
                        g.success();
                    }
                    std::task::Poll::Ready(Ok(val))
                }
                std::task::Poll::Ready(Err(err)) => {
                    if let Some(g) = guard.take() {
                        g.failure();
                    }
                    std::task::Poll::Ready(Err(CircuitBreakerError::Execution(err)))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
            CircuitBreakerServiceFutureProj::Open => {
                std::task::Poll::Ready(Err(CircuitBreakerError::Open))
            }
        }
    }
}

impl<S, Request> tower::Service<Request> for CircuitBreakerService<S>
where
    S: tower::Service<Request>,
{
    type Response = S::Response;
    type Error = CircuitBreakerError<S::Error>;
    type Future = CircuitBreakerServiceFuture<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map_err(CircuitBreakerError::Execution)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        match self.breaker.admit() {
            Ok(guard) => {
                // The guard comes first: if `call` panics, its drop frees a
                // half-open slot.
                let fut = self.inner.call(req);
                CircuitBreakerServiceFuture::Executing {
                    fut,
                    guard: Some(guard),
                }
            }
            Err(_) => CircuitBreakerServiceFuture::Open,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_transitions_to_open() {
        let policy = CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 5,
            open_duration: Duration::from_secs(60),
            half_open_trial_count: 2,
            ..CircuitBreakerPolicy::default()
        };
        let breaker = CircuitBreaker::new("test", policy);
        assert_eq!(breaker.state(), CircuitState::Closed);

        // Run 5 failing calls
        for _ in 0..5 {
            let res: Result<(), _> = breaker.run(async { Err("error") }).await;
            assert!(matches!(res, Err(CircuitBreakerError::Execution("error"))));
        }

        // The failure ratio is 100%, and we have 5 samples, so it should trip.
        assert_eq!(breaker.state(), CircuitState::Open);

        // Subsequent calls should fail fast with CircuitBreakerError::Open
        let mut executed = false;
        let res: Result<(), CircuitBreakerError<&'static str>> = breaker
            .run(async {
                executed = true;
                Ok(())
            })
            .await;
        assert!(matches!(res, Err(CircuitBreakerError::Open)));
        assert!(!executed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_tower_service() {
        use tower::{Layer, Service};
        let policy = CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 5,
            open_duration: Duration::from_secs(60),
            half_open_trial_count: 2,
            ..CircuitBreakerPolicy::default()
        };
        let breaker = CircuitBreaker::new("tower_test", policy);

        struct DummyService;
        impl tower::Service<&'static str> for DummyService {
            type Response = &'static str;
            type Error = &'static str;
            type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(
                &mut self,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: &'static str) -> Self::Future {
                if req == "fail" {
                    std::future::ready(Err("failed"))
                } else {
                    std::future::ready(Ok("ok"))
                }
            }
        }

        let mut svc = CircuitBreakerLayer::new(breaker.clone()).layer(DummyService);

        // Run 5 failing calls
        for _ in 0..5 {
            let res = svc.call("fail").await;
            assert!(matches!(res, Err(CircuitBreakerError::Execution("failed"))));
        }

        // Breaker should be Open
        assert_eq!(breaker.state(), CircuitState::Open);

        // Subsequent call should fail fast
        let res = svc.call("ok").await;
        assert!(matches!(res, Err(CircuitBreakerError::Open)));
    }

    #[test]
    fn test_circuit_breaker_policy_clamps_zero_half_open_trial_count() {
        let rc = crate::config::ResilienceConfig {
            circuit_breaker: crate::config::CircuitBreakerConfig {
                defaults: crate::config::CircuitBreakerPolicyConfig {
                    failure_ratio_threshold: None,
                    sample_window_secs: None,
                    minimum_sample_count: None,
                    open_duration_secs: None,
                    half_open_trial_count: Some(0),
                    ..Default::default()
                },
                hosts: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(
                        "override-zero".to_string(),
                        crate::config::CircuitBreakerPolicyConfig {
                            failure_ratio_threshold: None,
                            sample_window_secs: None,
                            minimum_sample_count: None,
                            open_duration_secs: None,
                            half_open_trial_count: Some(0),
                            ..Default::default()
                        },
                    );
                    m
                },
            },
        };

        // defaults check
        let policy_default = CircuitBreakerPolicy::from_config(&rc, "some-other-host");
        assert_eq!(policy_default.half_open_trial_count, 1);

        // host override check
        let policy_override = CircuitBreakerPolicy::from_config(&rc, "override-zero");
        assert_eq!(policy_override.half_open_trial_count, 1);
    }

    #[tokio::test]
    async fn test_circuit_breaker_tower_service_cancellation() {
        use tower::{Layer, Service};
        let policy = CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 5,
            open_duration: Duration::from_secs(60),
            half_open_trial_count: 2,
            ..CircuitBreakerPolicy::default()
        };
        let breaker = CircuitBreaker::new("tower_cancel_test", policy);

        // Put the breaker in HalfOpen state
        {
            let mut inner = breaker.inner.lock().unwrap();
            inner.state = CircuitState::HalfOpen;
            inner.half_open_in_flight = 0;
        }

        struct PendingService;
        impl tower::Service<&'static str> for PendingService {
            type Response = &'static str;
            type Error = &'static str;
            type Future = std::future::Pending<Result<Self::Response, Self::Error>>;

            fn poll_ready(
                &mut self,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn call(&mut self, _: &'static str) -> Self::Future {
                std::future::pending()
            }
        }

        let mut svc = CircuitBreakerLayer::new(breaker.clone()).layer(PendingService);

        // Call the service: this will increment half_open_in_flight since it's HalfOpen
        let fut = svc.call("ok");
        let in_flight_before = breaker.inner.lock().unwrap().half_open_in_flight;
        assert_eq!(in_flight_before, 1);

        // Drop the future (cancellation)
        drop(fut);

        // half_open_in_flight should be decremented back to 0!
        let in_flight_after = breaker.inner.lock().unwrap().half_open_in_flight;
        assert_eq!(in_flight_after, 0);
    }

    #[tokio::test]
    async fn test_circuit_breaker_clamps_zero_failure_ratio_threshold() {
        let policy = CircuitBreakerPolicy {
            failure_ratio_threshold: 0.0,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 5,
            open_duration: Duration::from_secs(60),
            half_open_trial_count: 2,
            ..CircuitBreakerPolicy::default()
        };
        let breaker = CircuitBreaker::new("clamp_test", policy);
        let config = breaker.config();
        assert!(config.failure_ratio_threshold > 0.0);
        assert!(config.failure_ratio_threshold <= 1.0);

        // Even with successful calls, it shouldn't trip
        for _ in 0..5 {
            let res: Result<(), CircuitBreakerError<&'static str>> =
                breaker.run(async { Ok::<(), &'static str>(()) }).await;
            assert!(res.is_ok());
        }
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_circuit_breaker_run_with_fallback() {
        let policy = CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 2,
            open_duration: Duration::from_secs(60),
            half_open_trial_count: 1,
            ..CircuitBreakerPolicy::default()
        };
        let breaker = CircuitBreaker::new("fallback_test", policy);

        // Test fallback on execution error
        let fallback_result = breaker
            .run_with_fallback(
                async { Err::<&'static str, &'static str>("failed") },
                |err| match err {
                    CircuitBreakerError::Execution(_) => Ok("fallback_success"),
                    CircuitBreakerError::Open => Err("wrong_error"),
                },
            )
            .await;
        assert_eq!(fallback_result, Ok("fallback_success"));

        // Force open by exceeding threshold
        let _ = breaker
            .run(async { Err::<(), &'static str>("fail1") })
            .await;
        let _ = breaker
            .run(async { Err::<(), &'static str>("fail2") })
            .await;
        assert_eq!(breaker.state(), CircuitState::Open);

        // Test fallback on Open state
        let fallback_result_open = breaker
            .run_with_fallback(
                async { Ok::<&'static str, &'static str>("won't run") },
                |err| match err {
                    CircuitBreakerError::Open => Ok("fallback_from_open"),
                    CircuitBreakerError::Execution(_) => Err("wrong_error"),
                },
            )
            .await;
        assert_eq!(fallback_result_open, Ok("fallback_from_open"));
    }

    #[test]
    fn test_circuit_breaker_recovers_from_poisoned_mutex() {
        let breaker = CircuitBreaker::new("poison_test", CircuitBreakerPolicy::default());

        // Poison the inner mutex by panicking while holding the lock.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = breaker.inner.lock().unwrap();
            panic!("poison the circuit breaker mutex");
        }));
        assert!(result.is_err());
        assert!(breaker.inner.is_poisoned());

        // Every breaker method must keep working instead of panicking.
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert!(breaker.before_call().is_ok());
        breaker.after_call(true);
        assert!(breaker.failure_ratio() < f64::EPSILON);
        let config = breaker.config();
        breaker.update_config(config);

        // The guard's Drop path (cancellation) must not panic either.
        drop(CircuitBreakerGuard::new(breaker.clone()));

        // Registry locks recover from poisoning too.
        let registry = CircuitBreakerRegistry::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.breakers.lock().unwrap();
            panic!("poison the registry mutex");
        }));
        assert!(result.is_err());
        assert!(registry.breakers.is_poisoned());

        let b = registry.get_or_create("poison_reg", CircuitBreakerPolicy::default());
        let b2 = registry.get_or_create_with_config("poison_reg", CircuitBreakerPolicy::default());
        assert_eq!(b.name(), b2.name());
        assert_eq!(registry.all_breakers().len(), 1);
        registry.clear();
        assert!(registry.all_breakers().is_empty());
    }

    #[test]
    fn test_circuit_breaker_survives_panic_during_state_transition() {
        // A tracing subscriber that panics on every event, simulating a
        // user-provided subscriber panicking inside `transition_to`'s log.
        struct PanickingSubscriber;
        impl tracing::Subscriber for PanickingSubscriber {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, _: &tracing::Event<'_>) {
                panic!("subscriber panic during circuit breaker transition");
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let policy = CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 1,
            open_duration: Duration::ZERO,
            half_open_trial_count: 1,
            ..CircuitBreakerPolicy::default()
        };
        // Panic mid-transition (Closed -> Open) while holding the lock; this
        // both poisons the mutex and interrupts `transition_to`.
        //
        // `tracing` callsite `Interest` is a single value cached per callsite
        // across the WHOLE PROCESS, combined from every concurrently active
        // dispatcher. `cargo test` runs this alongside thousands of other
        // unit tests in the same binary, dozens of which install their own
        // scoped subscribers, so the transition callsite can occasionally be
        // (re-)cached as "not interested" in the narrow window between this
        // thread's dispatcher registering and the event firing -- and then
        // the subscriber never runs and nothing panics. Rebuilding the cache
        // and re-firing on a fresh breaker converges almost immediately in
        // practice (the same remedy `router.rs`'s access-log test uses), so
        // retry a few times rather than flake.
        let mut breaker = None;
        for attempt in 1..=5 {
            let candidate = CircuitBreaker::new("transition_panic_test", policy.clone());
            tracing::callsite::rebuild_interest_cache();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tracing::subscriber::with_default(PanickingSubscriber, || {
                    candidate.after_call(false);
                });
            }));
            if result.is_err() {
                breaker = Some(candidate);
                break;
            }
            assert!(
                attempt < 5,
                "the panicking subscriber never observed the transition event after {attempt} attempts"
            );
        }
        let breaker = breaker.expect("a poisoned breaker");
        assert!(breaker.inner.is_poisoned());

        // The interrupted transition must still be complete: `open_until` was
        // set before `transition_to`, so the breaker recovers to HalfOpen
        // (instantly, since open_duration is zero) instead of being stuck
        // permanently Open with `open_until == None`.
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        assert!(breaker.before_call().is_ok());
        breaker.after_call(true);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    impl CircuitBreaker {
        /// The raw state and the half-open slots in use.
        fn trial_view(&self) -> (CircuitState, u64) {
            let inner = self.lock_inner();
            (inner.state, inner.half_open_in_flight)
        }
    }

    /// A policy that opens on slow calls only.
    fn slow_policy(threshold: Duration) -> CircuitBreakerPolicy {
        CircuitBreakerPolicy {
            minimum_sample_count: 2,
            slow_call_duration_threshold: Some(threshold),
            slow_call_rate_threshold: 0.5,
            ..CircuitBreakerPolicy::default()
        }
    }

    /// A guard that started `ago` before now.
    fn guard_started(breaker: &CircuitBreaker, ago: Duration) -> CircuitBreakerGuard {
        let mut guard = CircuitBreakerGuard::new(breaker.clone());
        guard.started = guard.started.checked_sub(ago).expect("instant in range");
        guard
    }

    #[test]
    fn slow_successes_open_the_breaker() {
        let breaker = CircuitBreaker::new("slow_ok", slow_policy(Duration::from_secs(5)));
        guard_started(&breaker, Duration::from_secs(10)).success();
        assert_eq!(breaker.state(), CircuitState::Closed, "1 of 2 samples");
        guard_started(&breaker, Duration::from_secs(10)).success();
        assert_eq!(breaker.state(), CircuitState::Open);
        assert!((breaker.slow_call_ratio() - 1.0).abs() < f64::EPSILON);
        assert!(breaker.failure_ratio() < f64::EPSILON);
        assert_eq!(breaker.slow_calls_total(), 2);
    }

    #[test]
    fn fast_calls_are_not_slow() {
        let breaker = CircuitBreaker::new("fast_ok", slow_policy(Duration::from_secs(5)));
        for _ in 0..4 {
            guard_started(&breaker, Duration::from_secs(1)).success();
        }
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.slow_calls_total(), 0);
    }

    #[test]
    fn no_threshold_turns_slow_detection_off() {
        let policy = CircuitBreakerPolicy {
            slow_call_duration_threshold: None,
            ..slow_policy(Duration::ZERO)
        };
        let breaker = CircuitBreaker::new("slow_off", policy);
        for _ in 0..4 {
            guard_started(&breaker, Duration::from_secs(61)).success();
        }
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.slow_calls_total(), 0);
    }

    #[test]
    fn cancelled_slow_call_counts_as_slow() {
        let breaker = CircuitBreaker::new("cancel_slow", slow_policy(Duration::from_secs(5)));
        drop(guard_started(&breaker, Duration::from_secs(5)));
        drop(guard_started(&breaker, Duration::from_secs(30)));
        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(breaker.failure_ratio() < f64::EPSILON, "slow, not failed");
        assert_eq!(breaker.slow_calls_total(), 2);
    }

    #[test]
    fn cancelled_slow_call_counts_as_failure_when_configured() {
        let policy = CircuitBreakerPolicy {
            slow_call_rate_threshold: 1.0,
            minimum_sample_count: 3,
            cancelled_call_outcome: CancelledCallOutcome::Failure,
            ..slow_policy(Duration::from_secs(5))
        };
        let breaker = CircuitBreaker::new("cancel_fail", policy);
        guard_started(&breaker, Duration::ZERO).success();
        drop(guard_started(&breaker, Duration::from_secs(30)));
        assert!((breaker.failure_ratio() - 0.5).abs() < f64::EPSILON);
        assert!((breaker.slow_call_ratio() - 0.5).abs() < f64::EPSILON);
        drop(guard_started(&breaker, Duration::from_secs(30)));
        assert_eq!(breaker.state(), CircuitState::Open, "failure ratio 2/3");
    }

    #[test]
    fn cancelled_fast_call_counts_nothing() {
        let breaker = CircuitBreaker::new("cancel_fast", slow_policy(Duration::from_secs(5)));
        for _ in 0..4 {
            drop(guard_started(&breaker, Duration::from_secs(1)));
        }
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert_eq!(breaker.lock_inner().window.counts(breaker.now()).total, 0);
    }

    #[test]
    fn dropped_tower_future_counts_as_slow_after_threshold() {
        use tower::{Layer, Service};
        struct Hang;
        impl tower::Service<()> for Hang {
            type Response = ();
            type Error = ();
            type Future = std::future::Pending<Result<(), ()>>;
            fn poll_ready(
                &mut self,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), ()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn call(&mut self, (): ()) -> Self::Future {
                std::future::pending()
            }
        }

        let breaker = CircuitBreaker::new("tower_slow", slow_policy(Duration::from_secs(5)));
        let mut svc = CircuitBreakerLayer::new(breaker.clone()).layer(Hang);
        for _ in 0..2 {
            let mut fut = svc.call(());
            if let CircuitBreakerServiceFuture::Executing { guard, .. } = &mut fut {
                let g = guard.as_mut().expect("guard");
                g.started = g.started.checked_sub(Duration::from_secs(6)).unwrap();
            }
            drop(fut);
        }
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn slow_half_open_trial_opens_the_breaker_again() {
        let breaker = CircuitBreaker::new("half_open_slow", slow_policy(Duration::from_secs(5)));
        breaker.lock_inner().state = CircuitState::HalfOpen;
        breaker.before_call().expect("trial slot");
        guard_started(&breaker, Duration::from_secs(10)).success();
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn slow_cancelled_half_open_trial_opens_the_breaker_again() {
        let breaker = CircuitBreaker::new("half_open_cancel", slow_policy(Duration::from_secs(5)));
        breaker.lock_inner().state = CircuitState::HalfOpen;
        breaker.before_call().expect("trial slot");
        drop(guard_started(&breaker, Duration::from_secs(10)));
        let (state, in_flight) = breaker.trial_view();
        assert_eq!(state, CircuitState::Open);
        assert_eq!(in_flight, 0);
    }

    #[test]
    fn new_sample_window_keeps_the_counts() {
        let breaker = CircuitBreaker::new("resize", CircuitBreakerPolicy::default());
        breaker.after_call(false);
        breaker.after_call(true);
        for window in [30, 10, 30] {
            breaker.update_config(CircuitBreakerPolicy {
                sample_window: Duration::from_secs(window),
                ..CircuitBreakerPolicy::default()
            });
            assert!((breaker.failure_ratio() - 0.5).abs() < f64::EPSILON);
        }
        assert_eq!(breaker.window_counts().total, 2);
    }

    #[test]
    fn switching_windows_does_not_stop_the_trip() {
        // Two clients with two window lengths share one breaker.
        let policy = |secs| CircuitBreakerPolicy {
            sample_window: Duration::from_secs(secs),
            minimum_sample_count: 4,
            ..CircuitBreakerPolicy::default()
        };
        let breaker = CircuitBreaker::new("flip", policy(10));
        for secs in [10, 20, 10, 20] {
            breaker.update_config(policy(secs));
            breaker.after_call(false);
        }
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn call_from_before_the_trip_is_not_a_half_open_trial() {
        let policy = CircuitBreakerPolicy {
            minimum_sample_count: 1,
            half_open_trial_count: 1,
            open_duration: Duration::ZERO,
            ..slow_policy(Duration::from_secs(5))
        };
        let breaker = CircuitBreaker::new("late_call", policy);
        // This call starts while the breaker is closed.
        let late = guard_started(&breaker, Duration::from_secs(10));
        breaker.after_call(false);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        breaker.before_call().expect("trial slot");
        late.success();
        let (state, in_flight) = breaker.trial_view();
        assert_eq!(state, CircuitState::HalfOpen, "not reopened");
        assert_eq!(in_flight, 1, "the trial keeps its slot");
        assert_eq!(breaker.slow_calls_total(), 1, "still a slow call");
    }

    #[test]
    fn admission_generation_is_kept_across_a_state_change() {
        let policy = CircuitBreakerPolicy {
            minimum_sample_count: 1,
            half_open_trial_count: 1,
            open_duration: Duration::ZERO,
            ..slow_policy(Duration::from_secs(5))
        };
        let breaker = CircuitBreaker::new("admit_gap", policy);
        // Admitted while closed. The guard is made after two state changes.
        let generation = breaker.before_call().expect("closed");
        breaker.after_call(false);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        breaker.before_call().expect("trial slot");
        let mut late = CircuitBreakerGuard::admitted(breaker.clone(), generation);
        late.started = late.started.checked_sub(Duration::from_secs(10)).unwrap();
        late.success();
        assert_eq!(breaker.trial_view(), (CircuitState::HalfOpen, 1));
    }

    #[test]
    fn drop_during_panic_frees_the_slot_and_keeps_the_state() {
        let breaker = CircuitBreaker::new("panic_drop", slow_policy(Duration::from_secs(5)));
        breaker.lock_inner().state = CircuitState::HalfOpen;
        breaker.before_call().expect("trial slot");
        let guard = guard_started(&breaker, Duration::from_secs(10));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("call body panics");
        }));
        assert!(res.is_err());
        assert_eq!(breaker.trial_view(), (CircuitState::HalfOpen, 0));
    }

    #[test]
    fn slow_failed_call_counts_in_both_ratios() {
        let breaker = CircuitBreaker::new("slow_fail", slow_policy(Duration::from_secs(5)));
        guard_started(&breaker, Duration::from_secs(10)).failure();
        assert!((breaker.failure_ratio() - 1.0).abs() < f64::EPSILON);
        assert!((breaker.slow_call_ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn host_override_sets_slow_call_keys() {
        let mut rc = crate::config::ResilienceConfig::default();
        rc.circuit_breaker.hosts.insert(
            "api".to_owned(),
            crate::config::CircuitBreakerPolicyConfig {
                slow_call_rate_threshold: Some(0.25),
                cancelled_call_outcome: Some(CancelledCallOutcome::Failure),
                ..Default::default()
            },
        );
        let host = CircuitBreakerPolicy::from_config(&rc, "api");
        assert!((host.slow_call_rate_threshold - 0.25).abs() < f64::EPSILON);
        assert_eq!(host.cancelled_call_outcome, CancelledCallOutcome::Failure);
        let other = CircuitBreakerPolicy::from_config(&rc, "other");
        assert!((other.slow_call_rate_threshold - 1.0).abs() < f64::EPSILON);
        assert_eq!(other.cancelled_call_outcome, CancelledCallOutcome::Slow);
    }

    #[test]
    fn policy_from_config_reads_slow_call_keys() {
        let mut rc = crate::config::ResilienceConfig::default();
        rc.circuit_breaker.defaults.slow_call_duration_threshold_ms = Some(2500);
        rc.circuit_breaker.defaults.slow_call_rate_threshold = Some(0.0);
        rc.circuit_breaker.defaults.cancelled_call_outcome = Some(CancelledCallOutcome::Failure);
        rc.circuit_breaker.hosts.insert(
            "off".to_owned(),
            crate::config::CircuitBreakerPolicyConfig {
                slow_call_duration_threshold_ms: Some(0),
                ..Default::default()
            },
        );

        let policy = CircuitBreakerPolicy::from_config(&rc, "any");
        assert_eq!(
            policy.slow_call_duration_threshold,
            Some(Duration::from_millis(2500))
        );
        assert!(policy.slow_call_rate_threshold > 0.0, "clamped above zero");
        assert_eq!(policy.cancelled_call_outcome, CancelledCallOutcome::Failure);

        let off = CircuitBreakerPolicy::from_config(&rc, "off");
        assert_eq!(off.slow_call_duration_threshold, None, "0 turns it off");
        assert_eq!(off.cancelled_call_outcome, CancelledCallOutcome::Failure);

        let defaults = CircuitBreakerPolicy::default();
        assert_eq!(
            defaults.slow_call_duration_threshold,
            Some(Duration::from_secs(60))
        );
        assert!((defaults.slow_call_rate_threshold - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cancelled_call_outcome_parses() {
        assert_eq!("slow".parse(), Ok(CancelledCallOutcome::Slow));
        assert_eq!(" failure ".parse(), Ok(CancelledCallOutcome::Failure));
        // Same spelling as TOML: lower case only.
        assert!("Failure".parse::<CancelledCallOutcome>().is_err());
        assert!("drop".parse::<CancelledCallOutcome>().is_err());
    }

    #[test]
    fn zero_sample_window_does_not_divide_by_zero() {
        let origin = Instant::now();
        let mut window = SampleWindow::new(Duration::ZERO, origin);
        window.record(origin, true, false);
        assert_eq!(window.counts(origin).total, 1);
        let later = origin + Duration::from_secs(1);
        assert_eq!(window.counts(later).total, 0);
    }

    #[test]
    fn window_skips_buckets_after_a_backward_clock_step() {
        let origin = Instant::now();
        let mut window = SampleWindow::new(Duration::from_secs(10), origin);
        window.record(origin + Duration::from_secs(5), true, true);
        assert_eq!(window.counts(origin + Duration::from_secs(1)).total, 0);
        // A record after the step clears the later bucket.
        window.record(origin + Duration::from_secs(1), false, false);
        assert_eq!(window.counts(origin + Duration::from_secs(5)).total, 1);
    }

    /// The naive model: it keeps every sample, and it puts each sample in
    /// the same time slice as [`SampleWindow`].
    struct ReferenceWindow {
        origin: Instant,
        width: Duration,
        samples: Vec<(Instant, bool, bool)>,
    }

    impl ReferenceWindow {
        fn slice(&self, t: Instant) -> u128 {
            t.duration_since(self.origin).as_nanos() / self.width.as_nanos().max(1)
        }

        fn counts(&self, now: Instant) -> WindowCounts {
            let current = self.slice(now);
            let mut counts = WindowCounts::default();
            for &(t, failed, slow) in &self.samples {
                if current - self.slice(t) < WINDOW_BUCKETS as u128 {
                    counts.total += 1;
                    counts.failed += u64::from(failed);
                    counts.slow += u64::from(slow);
                }
            }
            counts
        }

        /// Samples younger than `age`.
        fn younger_than(&self, now: Instant, age: Duration) -> u64 {
            self.samples
                .iter()
                .filter(|(t, _, _)| now.duration_since(*t) < age)
                .count() as u64
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        #[test]
        fn ring_window_agrees_with_reference(
            window_ms in 1_u64..30_000,
            steps in proptest::collection::vec(
                (0_u64..4_000, proptest::bool::ANY, proptest::bool::ANY, proptest::bool::ANY),
                1..300,
            ),
        ) {
            let origin = Instant::now();
            let window = Duration::from_millis(window_ms);
            let mut ring = SampleWindow::new(window, origin);
            let width = Duration::from_nanos(
                u64::try_from(window.as_nanos() / WINDOW_BUCKETS as u128).unwrap(),
            );
            let mut reference = ReferenceWindow {
                origin,
                width,
                samples: Vec::new(),
            };
            let mut now = origin;
            for (advance_ms, record, failed, slow) in steps {
                now += Duration::from_millis(advance_ms);
                // Some steps only read, so stale buckets are read too.
                if record {
                    ring.record(now, failed, slow);
                    reference.samples.push((now, failed, slow));
                }

                let got = ring.counts(now);
                let want = reference.counts(now);
                proptest::prop_assert_eq!(got, want);
                proptest::prop_assert_eq!(got.failure_ratio().to_bits(), want.failure_ratio().to_bits());
                proptest::prop_assert_eq!(got.slow_call_ratio().to_bits(), want.slow_call_ratio().to_bits());

                // Time bounds, free of the slice rule: the ring holds each
                // call younger than 9 buckets, and no call older than the
                // window.
                proptest::prop_assert!(reference.younger_than(now, width * 9) <= got.total);
                proptest::prop_assert!(got.total <= reference.younger_than(now, window));
            }
        }
    }

    /// The ring is a fixed array with no heap data.
    const _: () = assert!(std::mem::size_of::<SampleWindow>() <= 512);

    #[test]
    fn ring_window_memory_does_not_grow_with_rate() {
        let origin = Instant::now();
        let mut window = SampleWindow::new(Duration::from_secs(10), origin);
        // 10 s of calls at 100k calls per second. Bucket width is 1 s.
        for i in 0..1_000_000_u64 {
            window.record(origin + Duration::from_micros(i * 10), i % 3 == 0, false);
        }
        assert_eq!(window.buckets.len(), WINDOW_BUCKETS);
        // At t = 9.99999 s, all 10 buckets are in the window.
        let last = origin + Duration::from_micros(9_999_990);
        assert_eq!(window.counts(last).total, 1_000_000);
        // At t = 10 s, bucket 0 is out: 100k calls left the window.
        let ten = origin + Duration::from_secs(10);
        let counts = window.counts(ten);
        assert_eq!(counts.total, 900_000);
        assert_eq!(counts.failed, 300_000);
    }
}
