//! Adaptive admission control (issue #3068, ADR 0016).
//!
//! This module holds the parts of admission control that do not touch HTTP:
//!
//! - [`Criticality`]: the class of a request (`critical`, `default`,
//!   `sheddable`).
//! - [`PartitionShares`]: the share of the limit that each class can fill.
//!   A lower class is always rejected first.
//! - [`AdaptiveLimiter`]: a concurrency limit that changes with measured
//!   latency (Gradient2, Vegas or AIMD).
//!
//! [`crate::middleware::LoadShedLayer`] applies these to inbound requests.

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

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The header that carries a request's [`Criticality`] between services.
///
/// The HTTP client sends it on outbound calls. Inbound, the server reads it
/// only when `server.admission.trust_criticality_header = true`.
pub const CRITICALITY_HEADER: &str = "x-autumn-criticality";

/// The class of a request for admission control.
///
/// Under overload, `sheddable` requests are rejected first, then `default`
/// requests. `critical` requests are rejected last.
///
/// Set it on a route with `#[get("/x", criticality = "sheddable")]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Criticality {
    /// Rejected last. Can use the full limit.
    Critical,
    /// The class of a request that sets no criticality.
    #[default]
    Default,
    /// Rejected first.
    Sheddable,
}

impl Criticality {
    /// All classes, from the most important to the least important.
    pub const ALL: [Self; 3] = [Self::Critical, Self::Default, Self::Sheddable];

    /// Parse a wire name, ignoring ASCII case and outer spaces. Does not
    /// allocate, so the request path can call it on a header value.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(name))
    }

    /// The lowercase wire name (`"critical"`, `"default"`, `"sheddable"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Default => "default",
            Self::Sheddable => "sheddable",
        }
    }
}

impl fmt::Display for Criticality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error for a text that is not a [`Criticality`] name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown criticality {0:?}; use \"critical\", \"default\" or \"sheddable\"")]
pub struct ParseCriticalityError(String);

impl FromStr for Criticality {
    type Err = ParseCriticalityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s).ok_or_else(|| ParseCriticalityError(s.trim().to_owned()))
    }
}

tokio::task_local! {
    static CURRENT_CRITICALITY: Criticality;
}

/// The criticality of the inbound request that this task serves.
///
/// Returns `None` outside a request, and in a task that the handler spawned.
/// The HTTP client uses this value for [`CRITICALITY_HEADER`].
#[must_use]
pub fn current_criticality() -> Option<Criticality> {
    CURRENT_CRITICALITY.try_with(|c| *c).ok()
}

/// Run `fut` with `criticality` as [`current_criticality`].
pub async fn with_criticality<F: std::future::Future>(
    criticality: Criticality,
    fut: F,
) -> F::Output {
    scope_criticality(criticality, fut).await
}

/// [`with_criticality`] as a named future type, for a Tower service.
pub(crate) fn scope_criticality<F: std::future::Future>(
    criticality: Criticality,
    fut: F,
) -> tokio::task::futures::TaskLocalFuture<Criticality, F> {
    CURRENT_CRITICALITY.scope(criticality, fut)
}

/// The inbound deadline of a request. The request-timeout layer sets it as a
/// request extension. The adaptive limiter uses it to tell a deadline cancel
/// (overload) from a client that goes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboundDeadline {
    /// The request must end by this instant.
    At(tokio::time::Instant),
    /// The route has `timeout = "off"`. Its latency is not a capacity
    /// signal (for example, a long poll).
    Off,
}

/// An error in the `server.admission` settings.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AdmissionConfigError {
    /// A partition share is not in `0.0..=1.0`, or is not a number.
    #[error("server.admission.partitions.{class} = {value} is not in 0.0..=1.0")]
    ShareOutOfRange {
        /// The class name.
        class: &'static str,
        /// The value that was set.
        value: f64,
    },
    /// The `sheddable` share is larger than the `default` share.
    #[error(
        "server.admission.partitions.sheddable ({sheddable}) is larger than \
         partitions.default ({default}); a lower class must not get more of the limit"
    )]
    SharesOutOfOrder {
        /// The `default` share.
        default: f64,
        /// The `sheddable` share.
        sheddable: f64,
    },
    /// The limit bounds are not `1 <= min_limit <= max_limit`.
    #[error("server.admission limits must be 1 <= min_limit ({min}) <= max_limit ({max})")]
    LimitBounds {
        /// `min_limit`.
        min: usize,
        /// `max_limit`.
        max: usize,
    },
}

/// The fraction of the limit that each [`Criticality`] can fill.
///
/// A request of class `c` is admitted only while the in-flight count is less
/// than `threshold(c, limit)`. The shares are nested: `sheddable <= default
/// <= critical = 1.0`. Thus when the server rejects a `critical` request, it
/// also rejects `default` and `sheddable` requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionShares {
    default_permille: u16,
    sheddable_permille: u16,
}

/// Parts per thousand in a share of 1.0.
const PERMILLE: u16 = 1000;

impl Default for PartitionShares {
    /// `default = 1.0`, `sheddable = 0.5`. Requests without a criticality
    /// keep the full limit, so the defaults change nothing for them.
    fn default() -> Self {
        Self {
            default_permille: PERMILLE,
            sheddable_permille: PERMILLE / 2,
        }
    }
}

impl PartitionShares {
    /// Make shares from fractions of the limit.
    ///
    /// # Errors
    ///
    /// Returns an error when a share is not in `0.0..=1.0`, or when
    /// `sheddable > default`.
    pub fn new(default: f64, sheddable: f64) -> Result<Self, AdmissionConfigError> {
        let default_permille = share_permille("default", default)?;
        let sheddable_permille = share_permille("sheddable", sheddable)?;
        if sheddable_permille > default_permille {
            return Err(AdmissionConfigError::SharesOutOfOrder { default, sheddable });
        }
        Ok(Self {
            default_permille,
            sheddable_permille,
        })
    }

    /// The in-flight count below which a request of class `criticality` is
    /// admitted, for a total limit of `limit`.
    ///
    /// It is `floor(limit × share)`, but at least 1 when the share and the
    /// limit are above 0. Thus a small limit does not shed a class at idle.
    /// A share of 0 means that the class is always shed.
    #[must_use]
    pub fn threshold(&self, criticality: Criticality, limit: usize) -> usize {
        let permille = match criticality {
            Criticality::Default => self.default_permille,
            Criticality::Sheddable => self.sheddable_permille,
            Criticality::Critical => return limit,
        };
        if permille == 0 || limit == 0 {
            return 0;
        }
        scale_permille(limit, permille).max(1)
    }

    /// `true` when every class can use the full limit.
    #[must_use]
    pub const fn is_flat(&self) -> bool {
        self.default_permille == PERMILLE && self.sheddable_permille == PERMILLE
    }
}

/// Convert a share in `0.0..=1.0` to parts per thousand (rounded down).
fn share_permille(class: &'static str, value: f64) -> Result<u16, AdmissionConfigError> {
    if !(0.0..=1.0).contains(&value) {
        return Err(AdmissionConfigError::ShareOutOfRange { class, value });
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value is in 0.0..=1.0, so the product is in 0..=1000"
    )]
    let permille = (value * f64::from(PERMILLE)).floor() as u16;
    // A positive share below 1‰ must not become 0, which means "always shed".
    let permille = if value > 0.0 {
        permille.max(1)
    } else {
        permille
    };
    Ok(permille.min(PERMILLE))
}

/// `floor(limit * permille / 1000)`, without overflow.
fn scale_permille(limit: usize, permille: u16) -> usize {
    let scaled = (limit as u128)
        .saturating_mul(u128::from(permille))
        .checked_div(u128::from(PERMILLE))
        .unwrap_or(0);
    usize::try_from(scaled).unwrap_or(limit)
}

/// One completed request, as input to a limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// The time from admission to the response head.
    pub rtt: Duration,
    /// The in-flight count when the request was admitted (it included).
    pub in_flight: usize,
    /// `true` when the request shows overload: a `504`, an inner service
    /// error, or a cancel at the request deadline.
    pub dropped: bool,
    /// When the request ended, as time since the limiter started.
    pub at: Duration,
}

/// The bounds and start value of an adaptive limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitBounds {
    min: usize,
    max: usize,
    initial: usize,
}

impl LimitBounds {
    /// Check the bounds.
    ///
    /// # Errors
    ///
    /// Returns an error unless `1 <= min <= max`.
    pub const fn new(min: usize, max: usize, initial: usize) -> Result<Self, AdmissionConfigError> {
        if min == 0 || min > max {
            return Err(AdmissionConfigError::LimitBounds { min, max });
        }
        let initial = if initial < min {
            min
        } else if initial > max {
            max
        } else {
            initial
        };
        Ok(Self { min, max, initial })
    }

    /// The lowest limit. At least 1.
    #[must_use]
    pub const fn min(&self) -> usize {
        self.min
    }

    /// The highest limit.
    #[must_use]
    pub const fn max(&self) -> usize {
        self.max
    }

    /// The limit before the first sample, in `min..=max`.
    #[must_use]
    pub const fn initial(&self) -> usize {
        self.initial
    }

    const fn clamp_f64(&self, value: f64) -> f64 {
        let min = usize_to_f64(self.min);
        let max = usize_to_f64(self.max);
        if value.is_nan() {
            min
        } else {
            value.clamp(min, max)
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "limits are far below 2^52; a rounded limit is harmless"
)]
const fn usize_to_f64(value: usize) -> f64 {
    value as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "callers clamp the value to the limit bounds first"
)]
const fn f64_to_usize(value: f64) -> usize {
    value as usize
}

/// RTT in nanoseconds as `f64`, at least 1 ns, so a ratio never divides by
/// zero.
const fn rtt_nanos(rtt: Duration) -> f64 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "nanosecond RTTs fit an f64 mantissa for ~104 days"
    )]
    let nanos = rtt.as_nanos() as f64;
    nanos.max(1.0)
}

/// An exponential moving average that starts as a plain mean.
///
/// The same as Netflix `ExpAvgMeasurement`.
#[derive(Debug, Clone, Copy)]
struct ExpAvg {
    value: f64,
    count: u32,
    warmup: u32,
    factor: f64,
}

impl ExpAvg {
    fn new(window: u32, warmup: u32) -> Self {
        Self {
            value: 0.0,
            count: 0,
            warmup,
            factor: 2.0 / (f64::from(window) + 1.0),
        }
    }

    fn add(&mut self, sample: f64) -> f64 {
        if self.count < self.warmup {
            self.count = self.count.saturating_add(1);
            self.value += (sample - self.value) / f64::from(self.count);
        } else {
            self.value = self.value.mul_add(1.0 - self.factor, sample * self.factor);
        }
        self.value
    }
}

/// Netflix Gradient2.
///
/// It compares a short-term RTT (the last sample) with a long-term average.
/// When the short RTT is larger than `tolerance` × the long RTT, the limit
/// goes down. Otherwise it goes up by a small queue allowance.
///
/// Gradient2 averages samples over windows of at least 1 s and 10 samples,
/// as Netflix `WindowedLimit` does. The long-term average spans 600 windows
/// (about 10 minutes). Without windows, the average follows a high RTT in
/// seconds. Then the limit increases to the maximum under queueing.
#[derive(Debug, Clone)]
pub struct Gradient2 {
    bounds: LimitBounds,
    estimated: f64,
    long_rtt: ExpAvg,
    smoothing: f64,
    tolerance: f64,
    queue_size: f64,
    window: SampleWindow,
}

/// The samples of one Gradient2 window.
#[derive(Debug, Clone, Copy, Default)]
struct SampleWindow {
    start: Option<Duration>,
    rtt_sum: Duration,
    count: u32,
    max_in_flight: usize,
    dropped: bool,
}

/// The shortest Gradient2 window.
const WINDOW_MIN_TIME: Duration = Duration::from_secs(1);
/// The fewest samples in a Gradient2 window.
const WINDOW_MIN_SAMPLES: u32 = 10;

impl SampleWindow {
    /// Add `sample`. Returns the window's summary when it is complete, and
    /// starts a new window.
    fn add(&mut self, sample: Sample) -> Option<Sample> {
        let start = *self.start.get_or_insert(sample.at);
        self.rtt_sum = self.rtt_sum.saturating_add(sample.rtt);
        self.count = self.count.saturating_add(1);
        self.max_in_flight = self.max_in_flight.max(sample.in_flight);
        self.dropped |= sample.dropped;
        if sample.at.saturating_sub(start) < WINDOW_MIN_TIME || self.count < WINDOW_MIN_SAMPLES {
            return None;
        }
        let summary = Sample {
            rtt: self.rtt_sum.checked_div(self.count).unwrap_or_default(),
            in_flight: self.max_in_flight,
            dropped: self.dropped,
            at: sample.at,
        };
        *self = Self {
            start: Some(sample.at),
            ..Self::default()
        };
        Some(summary)
    }
}

impl Gradient2 {
    /// A Gradient2 limit with the Netflix defaults: long window 600
    /// windows, smoothing 0.2, tolerance 1.5, queue allowance 4.
    #[must_use]
    pub fn new(bounds: LimitBounds) -> Self {
        Self {
            bounds,
            estimated: usize_to_f64(bounds.initial),
            long_rtt: ExpAvg::new(600, 10),
            smoothing: 0.2,
            tolerance: 1.5,
            queue_size: 4.0,
            window: SampleWindow::default(),
        }
    }

    fn update(&mut self, sample: Sample) -> usize {
        match self.window.add(sample) {
            Some(summary) => self.update_window(summary),
            None => self.limit(),
        }
    }

    fn update_window(&mut self, sample: Sample) -> usize {
        let short = rtt_nanos(sample.rtt);
        let long = self.long_rtt.add(short);
        // After a load spike, let the stored long RTT come down fast. This
        // sample still uses the value before the decay, as Netflix does.
        if long / short > 2.0 {
            self.long_rtt.value = long * 0.95;
        }
        // An app that uses less than half the limit tells nothing about it.
        if usize_to_f64(sample.in_flight) < self.estimated / 2.0 {
            return self.limit();
        }
        let gradient = (self.tolerance * long / short).clamp(0.5, 1.0);
        let target = self.estimated.mul_add(gradient, self.queue_size);
        let next = self
            .estimated
            .mul_add(1.0 - self.smoothing, target * self.smoothing);
        self.estimated = self.bounds.clamp_f64(next);
        self.limit()
    }

    const fn limit(&self) -> usize {
        f64_to_usize(self.estimated)
    }
}

/// Netflix Vegas.
///
/// It keeps the lowest RTT seen (`rtt_noload`) and estimates the queue as
/// `limit × (1 − rtt_noload / rtt)`. A small queue increases the limit. A
/// large queue or a drop decreases it.
#[derive(Debug, Clone)]
pub struct Vegas {
    bounds: LimitBounds,
    estimated: f64,
    rtt_noload: f64,
    probe_count: u64,
    probe_multiplier: u64,
}

impl Vegas {
    /// A Vegas limit with the Netflix defaults (`alpha = 3`, `beta = 6`,
    /// both × `log10(limit)`; probe for a new base RTT every 30 × limit
    /// samples).
    #[must_use]
    pub const fn new(bounds: LimitBounds) -> Self {
        Self {
            bounds,
            estimated: usize_to_f64(bounds.initial),
            rtt_noload: 0.0,
            probe_count: 0,
            probe_multiplier: 30,
        }
    }

    /// `max(1, floor(log10(limit)))`, as Netflix `Log10RootFunction`.
    fn log10_root(limit: f64) -> f64 {
        limit.log10().floor().max(1.0)
    }

    fn update(&mut self, sample: Sample) -> usize {
        let rtt = rtt_nanos(sample.rtt);
        self.probe_count = self.probe_count.saturating_add(1);
        let probe_every = self
            .probe_multiplier
            .saturating_mul(u64::try_from(self.limit()).unwrap_or(u64::MAX));
        if self.probe_count >= probe_every {
            // Measure the base RTT again: it can change after a deploy.
            self.probe_count = 0;
            self.rtt_noload = rtt;
            return self.limit();
        }
        if self.rtt_noload <= 0.0 || rtt < self.rtt_noload {
            self.rtt_noload = rtt;
            return self.limit();
        }
        let limit = self.estimated;
        let step = Self::log10_root(limit);
        let next = if sample.dropped {
            limit - step
        } else if usize_to_f64(sample.in_flight) * 2.0 < limit {
            return self.limit();
        } else {
            let queue = (limit * (1.0 - self.rtt_noload / rtt)).ceil();
            if queue <= step {
                6.0f64.mul_add(step, limit)
            } else if queue < 3.0 * step {
                limit + step
            } else if queue > 6.0 * step {
                limit - step
            } else {
                return self.limit();
            }
        };
        let next = self.bounds.clamp_f64(next);
        if next < limit {
            // A queue is draining. Do not take a new base RTT until the
            // limit is stable again: that RTT would include the queue.
            self.probe_count = 0;
        }
        self.estimated = next;
        self.limit()
    }

    const fn limit(&self) -> usize {
        f64_to_usize(self.estimated)
    }
}

/// Additive increase, multiplicative decrease.
///
/// A drop, or an RTT above the threshold, multiplies the limit by 0.9. A
/// request that used at least half the limit adds 1.
#[derive(Debug, Clone)]
pub struct Aimd {
    bounds: LimitBounds,
    limit: usize,
    latency_threshold: Duration,
}

impl Aimd {
    /// An AIMD limit that backs off when an RTT is above `latency_threshold`.
    #[must_use]
    pub const fn new(bounds: LimitBounds, latency_threshold: Duration) -> Self {
        Self {
            bounds,
            limit: bounds.initial,
            latency_threshold,
        }
    }

    fn update(&mut self, sample: Sample) -> usize {
        let next = if sample.dropped || sample.rtt > self.latency_threshold {
            // limit × 0.9, rounded down.
            self.limit
                .saturating_mul(9)
                .checked_div(10)
                .unwrap_or(self.bounds.min)
        } else if sample.in_flight.saturating_mul(2) >= self.limit {
            self.limit.saturating_add(1)
        } else {
            self.limit
        };
        self.limit = next.clamp(self.bounds.min, self.bounds.max);
        self.limit
    }
}

/// A limit algorithm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LimitAlgorithm {
    /// See [`Gradient2`].
    Gradient2(Gradient2),
    /// See [`Vegas`].
    Vegas(Vegas),
    /// See [`Aimd`].
    Aimd(Aimd),
}

impl LimitAlgorithm {
    /// Feed one sample. Returns the new limit, in the configured bounds.
    pub fn update(&mut self, sample: Sample) -> usize {
        match self {
            Self::Gradient2(g) => g.update(sample),
            Self::Vegas(v) => v.update(sample),
            Self::Aimd(a) => a.update(sample),
        }
    }

    /// The current limit.
    #[must_use]
    pub const fn limit(&self) -> usize {
        match self {
            Self::Gradient2(g) => g.limit(),
            Self::Vegas(v) => v.limit(),
            Self::Aimd(a) => a.limit,
        }
    }
}

/// A concurrency limit that a [`LimitAlgorithm`] moves.
///
/// Admission reads the limit with one atomic load. Each completed request
/// gives a [`Sample`]. If another thread updates the algorithm, the limiter
/// ignores the sample. A request does not wait for a lock.
#[derive(Debug)]
pub struct AdaptiveLimiter {
    limit: AtomicUsize,
    algorithm: Mutex<LimitAlgorithm>,
    origin: tokio::time::Instant,
}

impl AdaptiveLimiter {
    /// A limiter that starts at the algorithm's current limit.
    #[must_use]
    pub fn new(algorithm: LimitAlgorithm) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicUsize::new(algorithm.limit()),
            algorithm: Mutex::new(algorithm),
            origin: tokio::time::Instant::now(),
        })
    }

    /// The time from the limiter's start to `now`, for [`Sample::at`].
    #[must_use]
    pub fn elapsed(&self, now: tokio::time::Instant) -> Duration {
        now.saturating_duration_since(self.origin)
    }

    /// The current limit.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Feed one completed request. Returns the new limit, or `None` when
    /// another thread holds the lock (the limiter ignores the sample).
    pub fn record(&self, sample: Sample) -> Option<usize> {
        self.record_with(sample, |_| {})
    }

    /// [`Self::record`], and call `on_update` with the new limit while the
    /// lock is held. Thus updates reach `on_update` in order.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the lock is held through `on_update` on purpose, so updates stay in order"
    )]
    pub fn record_with(&self, sample: Sample, on_update: impl FnOnce(usize)) -> Option<usize> {
        let mut algorithm = self.algorithm.try_lock().ok()?;
        let limit = algorithm.update(sample);
        self.limit.store(limit, Ordering::Relaxed);
        on_update(limit);
        Some(limit)
    }
}

impl AdaptiveLimiter {
    /// Build the limiter that `[server.admission]` describes. `ceiling` is
    /// the static ceiling, if any; it is the default `max_limit`.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit bounds are not valid.
    pub fn from_config(
        config: &crate::config::AdmissionConfig,
        ceiling: Option<usize>,
    ) -> Result<Arc<Self>, AdmissionConfigError> {
        use crate::config::AdmissionAlgorithm;
        let bounds = config.limit_bounds(ceiling)?;
        let algorithm = match config.algorithm {
            AdmissionAlgorithm::Vegas => LimitAlgorithm::Vegas(Vegas::new(bounds)),
            AdmissionAlgorithm::Aimd => LimitAlgorithm::Aimd(Aimd::new(
                bounds,
                Duration::from_millis(config.latency_threshold_ms),
            )),
            // `Gradient2` and any later default.
            _ => LimitAlgorithm::Gradient2(Gradient2::new(bounds)),
        };
        Ok(Self::new(algorithm))
    }
}

/// Google SRE client-side adaptive throttling, per host.
///
/// For each host, the throttle counts `requests` (every attempt, including
/// the attempts it rejects) and `accepts` (attempts the host did not reject
/// for overload) over a sliding window. A new attempt is rejected locally
/// with probability
///
/// ```text
/// max(0, (requests − K × accepts) / (requests + 1))
/// ```
///
/// While the host accepts more than `1/K` of the attempts, the probability
/// is 0 and nothing is rejected. Old counts leave the window. Thus the client
/// sends requests to a host again when the host is serviceable.
#[derive(Debug)]
pub struct AdaptiveThrottle {
    k: f64,
    bucket: Duration,
    hosts: Mutex<std::collections::HashMap<String, HostWindow>>,
}

/// The number of buckets in a throttle window.
const THROTTLE_BUCKETS: u64 = 12;

/// The most hosts a throttle tracks. At the cap, it first removes hosts with
/// no counts in the window. If it is still full, it does not track a new
/// host and does not reject calls to it.
pub const MAX_THROTTLE_HOSTS: usize = 4096;

/// Counts for one host, in time buckets.
#[derive(Debug)]
struct HostWindow {
    origin: std::time::Instant,
    buckets: std::collections::VecDeque<ThrottleBucket>,
}

#[derive(Debug, Clone, Copy)]
struct ThrottleBucket {
    epoch: u64,
    requests: u64,
    accepts: u64,
}

impl HostWindow {
    const fn new(now: std::time::Instant) -> Self {
        Self {
            origin: now,
            buckets: std::collections::VecDeque::new(),
        }
    }

    fn epoch(&self, now: std::time::Instant, bucket: Duration) -> u64 {
        let elapsed = now.saturating_duration_since(self.origin).as_nanos();
        let width = bucket.as_nanos().max(1);
        u64::try_from(elapsed.checked_div(width).unwrap_or(0)).unwrap_or(u64::MAX)
    }

    /// Drop the buckets that left the window and return the current one.
    fn current(&mut self, epoch: u64) -> Option<&mut ThrottleBucket> {
        while self
            .buckets
            .front()
            .is_some_and(|b| b.epoch.saturating_add(THROTTLE_BUCKETS) <= epoch)
        {
            self.buckets.pop_front();
        }
        if self.buckets.back().is_none_or(|b| b.epoch != epoch) {
            self.buckets.push_back(ThrottleBucket {
                epoch,
                requests: 0,
                accepts: 0,
            });
        }
        self.buckets.back_mut()
    }

    /// `true` when the window holds no counts at `epoch`.
    fn is_expired(&self, epoch: u64) -> bool {
        self.buckets
            .back()
            .is_none_or(|b| b.epoch.saturating_add(THROTTLE_BUCKETS) <= epoch)
    }

    fn totals(&self) -> (u64, u64) {
        self.buckets.iter().fold((0, 0), |(r, a), b| {
            (r.saturating_add(b.requests), a.saturating_add(b.accepts))
        })
    }
}

impl AdaptiveThrottle {
    /// A throttle with multiplier `k` (Google SRE uses 2) over `window`.
    #[must_use]
    pub fn new(k: f64, window: Duration) -> Self {
        let bucket = window
            .checked_div(u32::try_from(THROTTLE_BUCKETS).unwrap_or(1))
            .unwrap_or(window)
            .max(Duration::from_millis(1));
        Self {
            k: if k.is_finite() { k.max(1.0) } else { 2.0 },
            bucket,
            hosts: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The probability that the next attempt to `host` is rejected locally.
    #[must_use]
    pub fn reject_probability(&self, host: &str, now: std::time::Instant) -> f64 {
        let Ok(mut hosts) = self.hosts.lock() else {
            return 0.0;
        };
        let Some(window) = hosts.get_mut(host) else {
            return 0.0;
        };
        let epoch = window.epoch(now, self.bucket);
        let _ = window.current(epoch);
        let (requests, accepts) = window.totals();
        probability(requests, accepts, self.k)
    }

    /// Count one attempt to `host`. Returns `false` when the throttle
    /// rejects it locally. `draw` gives a uniform random `u64`. The throttle
    /// calls it only when the reject probability is above 0.
    pub fn admit(&self, host: &str, now: std::time::Instant, draw: impl FnOnce() -> u64) -> bool {
        let Ok(mut hosts) = self.hosts.lock() else {
            return true;
        };
        if !hosts.contains_key(host) {
            if hosts.len() >= MAX_THROTTLE_HOSTS {
                let bucket = self.bucket;
                hosts.retain(|_, w| !w.is_expired(w.epoch(now, bucket)));
                if hosts.len() >= MAX_THROTTLE_HOSTS {
                    return true;
                }
            }
            hosts.insert(host.to_owned(), HostWindow::new(now));
        }
        let Some(window) = hosts.get_mut(host) else {
            return true;
        };
        let epoch = window.epoch(now, self.bucket);
        let (requests, accepts) = {
            let _ = window.current(epoch);
            window.totals()
        };
        let p = probability(requests, accepts, self.k);
        if let Some(bucket) = window.current(epoch) {
            bucket.requests = bucket.requests.saturating_add(1);
        }
        if p <= 0.0 {
            return true;
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "a uniform draw keeps its distribution in an f64"
        )]
        let unit = draw() as f64 / (u64::MAX as f64 + 1.0);
        unit >= p
    }

    /// Record the outcome of an attempt that [`Self::admit`] let through.
    /// `accepted` is `false` when the host rejected it for overload.
    pub fn record(&self, host: &str, now: std::time::Instant, accepted: bool) {
        if !accepted {
            return;
        }
        let Ok(mut hosts) = self.hosts.lock() else {
            return;
        };
        if let Some(window) = hosts.get_mut(host) {
            let epoch = window.epoch(now, self.bucket);
            if let Some(bucket) = window.current(epoch) {
                bucket.accepts = bucket.accepts.saturating_add(1);
            }
        }
    }
}

/// `max(0, (requests − k × accepts) / (requests + 1))`.
fn probability(requests: u64, accepts: u64, k: f64) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "window counts far below 2^52")]
    let (r, a) = (requests as f64, accepts as f64);
    (k.mul_add(-a, r) / (r + 1.0)).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AdaptiveThrottle (Google SRE) ─────────────────────────────────────

    fn origin() -> std::time::Instant {
        crate::time::ambient_instant()
    }

    /// Feed `n` attempts, `accepted` of which the host accepts. Every draw
    /// is `u64::MAX`, so the throttle never rejects during the feed.
    fn feed(t: &AdaptiveThrottle, now: std::time::Instant, n: u64, accepted: u64) {
        for i in 0..n {
            assert!(t.admit("api", now, || u64::MAX));
            t.record("api", now, i < accepted);
        }
    }

    #[test]
    fn throttle_never_rejects_above_the_accept_ratio() {
        let t = AdaptiveThrottle::new(2.0, Duration::from_secs(120));
        let now = origin();
        // 60% accepted > 1/K = 50%.
        feed(&t, now, 100, 60);
        assert!(t.reject_probability("api", now) < f64::EPSILON);
        for draw in [0, 1, u64::MAX / 2] {
            assert!(t.admit("api", now, || draw), "draw {draw} must pass");
            t.record("api", now, true);
        }
    }

    #[test]
    fn throttle_rejects_locally_once_the_accept_ratio_falls_below_one_over_k() {
        let t = AdaptiveThrottle::new(2.0, Duration::from_secs(120));
        let now = origin();
        // 40% accepted < 1/K = 50%: p = (100 - 80) / 101.
        feed(&t, now, 100, 40);
        let p = t.reject_probability("api", now);
        assert!((p - 20.0 / 101.0).abs() < 1e-9, "p = {p}");
        assert!(!t.admit("api", now, || 0), "a low draw is rejected locally");
        assert!(t.admit("api", now, || u64::MAX), "a high draw still passes");
    }

    #[test]
    fn throttle_with_no_accepts_rejects_almost_everything() {
        let t = AdaptiveThrottle::new(2.0, Duration::from_secs(120));
        let now = origin();
        feed(&t, now, 1000, 0);
        let p = t.reject_probability("api", now);
        assert!(p > 0.99 && p < 1.0, "p = {p}: high, but never 1");
    }

    #[test]
    fn throttle_counts_hosts_apart() {
        let t = AdaptiveThrottle::new(2.0, Duration::from_secs(120));
        let now = origin();
        feed(&t, now, 100, 0);
        assert!(t.reject_probability("api", now) > 0.9);
        assert!(t.reject_probability("other", now) < f64::EPSILON);
        assert!(t.admit("other", now, || 0));
    }

    #[test]
    fn throttle_forgets_after_the_window() {
        let t = AdaptiveThrottle::new(2.0, Duration::from_secs(12));
        let now = origin();
        feed(&t, now, 100, 0);
        assert!(t.reject_probability("api", now) > 0.9);
        let later = now + Duration::from_secs(13);
        assert!(t.reject_probability("api", later) < f64::EPSILON);
        assert!(t.admit("api", later, || 0));
    }

    /// At exactly `1/K` accepts the probability is 0: the throttle starts
    /// only below the ratio.
    #[test]
    fn throttle_does_not_reject_at_exactly_one_over_k() {
        let t = AdaptiveThrottle::new(2.0, Duration::from_secs(120));
        let now = origin();
        feed(&t, now, 100, 50);
        assert!(t.reject_probability("api", now) < f64::EPSILON);
        let mut drew = false;
        assert!(t.admit("api", now, || {
            drew = true;
            0
        }));
        assert!(!drew, "no random draw while the probability is 0");
    }

    #[test]
    fn throttle_tracks_at_most_max_hosts() {
        let t = AdaptiveThrottle::new(2.0, Duration::from_secs(12));
        let now = origin();
        for i in 0..MAX_THROTTLE_HOSTS {
            assert!(t.admit(&format!("h{i}"), now, || 0));
        }
        // Full: a new host is not tracked, and never rejected.
        for _ in 0..50 {
            assert!(t.admit("new", now, || 0));
        }
        assert!(t.reject_probability("new", now) < f64::EPSILON);
        // After the window, the old hosts are expired and make room.
        let later = now + Duration::from_secs(13);
        for _ in 0..50 {
            t.admit("new", later, || u64::MAX);
        }
        assert!(t.reject_probability("new", later) > 0.9, "now tracked");
        assert!(t.hosts.lock().unwrap().len() <= MAX_THROTTLE_HOSTS);
    }

    #[test]
    fn throttle_k_below_one_is_raised_to_one() {
        // K < 1 would reject a host that accepts everything.
        let t = AdaptiveThrottle::new(0.5, Duration::from_secs(120));
        let now = origin();
        feed(&t, now, 100, 100);
        assert!(t.reject_probability("api", now) < f64::EPSILON);
    }

    #[test]
    fn from_config_uses_the_ceiling_as_the_default_max() {
        let mut config = crate::config::AdmissionConfig {
            initial_limit: 5000,
            ..Default::default()
        };
        let limiter = AdaptiveLimiter::from_config(&config, Some(64)).unwrap();
        assert_eq!(limiter.limit(), 64, "initial is clamped to the ceiling");
        let limiter = AdaptiveLimiter::from_config(&config, None).unwrap();
        assert_eq!(limiter.limit(), crate::config::DEFAULT_ADMISSION_MAX_LIMIT);
        config.max_limit = Some(100);
        let limiter = AdaptiveLimiter::from_config(&config, Some(64)).unwrap();
        assert_eq!(limiter.limit(), 100, "an explicit max_limit wins");
        config.min_limit = 200;
        assert!(AdaptiveLimiter::from_config(&config, None).is_err());
    }

    /// Regression: a static ceiling below `min_limit` must lower the
    /// minimum, not fail and drop back to static mode.
    #[test]
    fn from_config_lowers_min_to_a_small_ceiling() {
        let config = crate::config::AdmissionConfig::default();
        let limiter = AdaptiveLimiter::from_config(&config, Some(5)).unwrap();
        assert_eq!(limiter.limit(), 5);
    }

    fn bounds(min: usize, max: usize, initial: usize) -> LimitBounds {
        LimitBounds::new(min, max, initial).unwrap()
    }

    fn sample(rtt_ms: u64, in_flight: usize) -> Sample {
        Sample {
            rtt: Duration::from_millis(rtt_ms),
            in_flight,
            dropped: false,
            at: Duration::ZERO,
        }
    }

    /// Feed Gradient2 one full window (10 samples over 1 s) per call.
    fn g2_window(g: &mut Gradient2, clock: &mut Duration, rtt_ms: u64, in_flight: usize) -> usize {
        let mut limit = g.limit();
        for _ in 0..10 {
            *clock += Duration::from_millis(100);
            limit = g.update(Sample {
                at: *clock,
                ..sample(rtt_ms, in_flight)
            });
        }
        limit
    }

    // ── Criticality ───────────────────────────────────────────────────────

    #[test]
    fn criticality_parses_case_insensitively_and_round_trips() {
        for c in Criticality::ALL {
            assert_eq!(c.as_str().parse::<Criticality>(), Ok(c));
            assert_eq!(c.as_str().to_uppercase().parse::<Criticality>(), Ok(c));
        }
        assert!("urgent".parse::<Criticality>().is_err());
        assert_eq!(
            Criticality::from_name(" Sheddable "),
            Some(Criticality::Sheddable)
        );
        assert_eq!(Criticality::from_name("urgent"), None);
        assert_eq!(Criticality::default(), Criticality::Default);
    }

    #[test]
    fn criticality_serde_uses_lowercase_names() {
        let c: Criticality = serde_json::from_str("\"sheddable\"").unwrap();
        assert_eq!(c, Criticality::Sheddable);
        assert_eq!(
            serde_json::to_string(&Criticality::Critical).unwrap(),
            "\"critical\""
        );
    }

    #[tokio::test]
    async fn current_criticality_follows_the_scope() {
        assert_eq!(current_criticality(), None);
        let inner = with_criticality(Criticality::Sheddable, async { current_criticality() }).await;
        assert_eq!(inner, Some(Criticality::Sheddable));
        assert_eq!(current_criticality(), None);
    }

    // ── Partition shares ──────────────────────────────────────────────────

    #[test]
    fn default_shares_keep_the_full_limit_for_default_traffic() {
        let shares = PartitionShares::default();
        assert_eq!(shares.threshold(Criticality::Critical, 64), 64);
        assert_eq!(shares.threshold(Criticality::Default, 64), 64);
        assert_eq!(shares.threshold(Criticality::Sheddable, 64), 32);
    }

    #[test]
    fn thresholds_round_down() {
        let shares = PartitionShares::new(0.9, 0.5).unwrap();
        assert_eq!(shares.threshold(Criticality::Default, 10), 9);
        assert_eq!(shares.threshold(Criticality::Sheddable, 3), 1);
        assert_eq!(shares.threshold(Criticality::Critical, 1), 1);
    }

    /// A positive share keeps at least one slot, so a small limit does not
    /// shed a class at idle. A zero share always sheds.
    #[test]
    fn positive_shares_keep_one_slot() {
        let shares = PartitionShares::new(0.9, 0.1).unwrap();
        assert_eq!(shares.threshold(Criticality::Default, 1), 1);
        assert_eq!(shares.threshold(Criticality::Sheddable, 8), 1);
        assert_eq!(shares.threshold(Criticality::Sheddable, 0), 0);
        let never = PartitionShares::new(1.0, 0.0).unwrap();
        assert_eq!(never.threshold(Criticality::Sheddable, 100), 0);
    }

    #[test]
    fn thresholds_do_not_overflow_at_usize_max() {
        let shares = PartitionShares::new(1.0, 0.5).unwrap();
        assert_eq!(
            shares.threshold(Criticality::Default, usize::MAX),
            usize::MAX
        );
        assert_eq!(
            shares.threshold(Criticality::Sheddable, usize::MAX),
            usize::MAX / 2
        );
    }

    /// Regression (#3183 review): a positive share below 1‰ keeps a slot.
    #[test]
    fn a_tiny_positive_share_is_not_zero() {
        let shares = PartitionShares::new(1.0, 0.0009).unwrap();
        assert_eq!(shares.threshold(Criticality::Sheddable, 100), 1);
        assert_eq!(shares.threshold(Criticality::Sheddable, 1), 1);
    }

    #[test]
    fn shares_reject_bad_values() {
        assert!(matches!(
            PartitionShares::new(1.5, 0.5),
            Err(AdmissionConfigError::ShareOutOfRange {
                class: "default",
                ..
            })
        ));
        assert!(matches!(
            PartitionShares::new(0.9, -0.1),
            Err(AdmissionConfigError::ShareOutOfRange {
                class: "sheddable",
                ..
            })
        ));
        assert!(PartitionShares::new(f64::NAN, 0.5).is_err());
        assert!(matches!(
            PartitionShares::new(0.4, 0.5),
            Err(AdmissionConfigError::SharesOutOfOrder { .. })
        ));
    }

    /// The property behind "sheddable is rejected before critical": for any
    /// limit and in-flight count, a lower class is never admitted when a
    /// higher class is rejected.
    #[test]
    fn a_lower_class_is_never_admitted_when_a_higher_one_is_rejected() {
        for (d, s) in [(1.0, 0.5), (0.9, 0.5), (0.5, 0.5), (0.8, 0.0), (1.0, 1.0)] {
            let shares = PartitionShares::new(d, s).unwrap();
            for limit in 0..200 {
                let crit = shares.threshold(Criticality::Critical, limit);
                let def = shares.threshold(Criticality::Default, limit);
                let shed = shares.threshold(Criticality::Sheddable, limit);
                assert!(
                    shed <= def && def <= crit && crit == limit,
                    "{d} {s} {limit}"
                );
            }
        }
    }

    // ── Limit bounds ──────────────────────────────────────────────────────

    #[test]
    fn bounds_reject_zero_min_and_inverted_range() {
        assert!(LimitBounds::new(0, 10, 5).is_err());
        assert!(LimitBounds::new(11, 10, 10).is_err());
        assert_eq!(bounds(4, 10, 100).initial(), 10);
        assert_eq!(bounds(4, 10, 1).initial(), 4);
    }

    // ── Gradient2 ─────────────────────────────────────────────────────────

    #[test]
    fn gradient2_grows_under_steady_latency_and_full_use() {
        let mut g = Gradient2::new(bounds(1, 1000, 20));
        let mut clock = Duration::ZERO;
        let mut limit = 20;
        for _ in 0..20 {
            limit = g2_window(&mut g, &mut clock, 10, limit);
        }
        assert!(
            limit > 20,
            "steady latency at full use must grow the limit, got {limit}"
        );
    }

    #[test]
    fn gradient2_waits_for_a_full_window() {
        let mut g = Gradient2::new(bounds(1, 1000, 100));
        // 50 samples in 0.5 s: enough samples, but too short a window.
        for i in 0..50 {
            g.update(Sample {
                at: Duration::from_millis(i * 10),
                ..sample(500, 100)
            });
        }
        assert_eq!(g.limit(), 100);
    }

    #[test]
    fn gradient2_shrinks_when_latency_rises_5x() {
        let mut g = Gradient2::new(bounds(1, 1000, 100));
        let mut clock = Duration::ZERO;
        for _ in 0..10 {
            g2_window(&mut g, &mut clock, 10, 100);
        }
        let before = g.limit();
        let mut after = before;
        for _ in 0..10 {
            after = g2_window(&mut g, &mut clock, 50, after);
        }
        assert!(
            after * 2 < before,
            "5x latency must cut the limit: {before} -> {after}"
        );
    }

    /// Regression: per-sample updates let the long RTT follow a high RTT
    /// within seconds, and the limit grew to the maximum under queueing.
    #[test]
    fn gradient2_does_not_drift_up_under_sustained_high_latency() {
        let mut g = Gradient2::new(bounds(1, 1000, 100));
        let mut clock = Duration::ZERO;
        for _ in 0..10 {
            g2_window(&mut g, &mut clock, 10, 100);
        }
        let mut limit = g.limit();
        // 60 s at 5x latency, 400 samples per second.
        for _ in 0..60 {
            for _ in 0..400 {
                clock += Duration::from_micros(2500);
                limit = g.update(Sample {
                    at: clock,
                    ..sample(50, limit)
                });
            }
        }
        assert!(limit < 100, "the limit drifted up to {limit}");
    }

    #[test]
    fn gradient2_ignores_app_limited_samples() {
        let mut g = Gradient2::new(bounds(1, 1000, 100));
        let mut clock = Duration::ZERO;
        for _ in 0..5 {
            g2_window(&mut g, &mut clock, 10, 5);
        }
        assert_eq!(g.limit(), 100, "an idle app must not move the limit");
    }

    #[test]
    fn gradient2_stays_in_bounds_and_survives_zero_rtt() {
        let mut g = Gradient2::new(bounds(5, 30, 10));
        let mut clock = Duration::ZERO;
        for i in 0..100 {
            let rtt = if i % 2 == 0 { 0 } else { 10_000 };
            let l = g2_window(&mut g, &mut clock, rtt, 30);
            assert!((5..=30).contains(&l), "limit {l} left its bounds");
        }
    }

    /// Regression: a decrease that the minimum clamps must not restart the
    /// probe count, or Vegas never takes a new base RTT at the floor.
    #[test]
    fn vegas_probes_again_at_the_floor() {
        let mut v = Vegas::new(bounds(8, 1000, 20));
        let mut limit = 20;
        for _ in 0..20 {
            limit = v.update(sample(10, limit));
        }
        // The base RTT is now 10 ms. A lasting 50 ms RTT drives the limit
        // to the floor, then a probe takes 50 ms as the new base.
        for _ in 0..2000 {
            limit = v.update(sample(50, limit));
        }
        assert!(limit > 8, "Vegas stayed at the floor: limit {limit}");
    }

    // ── Vegas ─────────────────────────────────────────────────────────────

    #[test]
    fn vegas_grows_without_a_queue_and_shrinks_with_one() {
        let mut v = Vegas::new(bounds(1, 1000, 20));
        let mut limit = 20;
        for _ in 0..50 {
            limit = v.update(sample(10, limit));
        }
        assert!(limit > 20, "no queue must grow the limit, got {limit}");
        let peak = limit;
        // Vegas takes off `log10(limit)` per sample, so it needs a few
        // hundred samples (well under a second at production rates).
        for _ in 0..300 {
            limit = v.update(sample(50, limit));
        }
        assert!(
            limit < peak / 2,
            "5x RTT must shrink the limit: {peak} -> {limit}"
        );
    }

    /// Regression: a fast drop must not make Vegas take the queued RTT as
    /// its new base RTT and grow back to the maximum.
    #[test]
    fn vegas_does_not_rebase_while_the_queue_drains() {
        let mut v = Vegas::new(bounds(1, 1000, 20));
        let mut limit = 20;
        for _ in 0..50 {
            limit = v.update(sample(10, limit));
        }
        for _ in 0..350 {
            limit = v.update(sample(50, limit));
        }
        assert!(limit < 20, "Vegas re-based on a queued RTT: limit {limit}");
    }

    #[test]
    fn vegas_shrinks_on_drops() {
        let mut v = Vegas::new(bounds(1, 1000, 50));
        v.update(sample(10, 50));
        let before = v.limit();
        let after = v.update(Sample {
            dropped: true,
            ..sample(20, 50)
        });
        assert!(after < before);
    }

    #[test]
    fn vegas_stays_in_bounds() {
        let mut v = Vegas::new(bounds(3, 40, 10));
        for i in 0..2000 {
            let rtt = if i % 7 == 0 { 1 } else { 1 + (i % 50) };
            let l = v.update(sample(rtt, 40));
            assert!((3..=40).contains(&l), "limit {l} left its bounds");
        }
    }

    // ── AIMD ──────────────────────────────────────────────────────────────

    #[test]
    fn aimd_adds_one_and_backs_off_by_ten_percent() {
        let mut a = Aimd::new(bounds(1, 1000, 100), Duration::from_millis(100));
        assert_eq!(a.update(sample(10, 60)), 101);
        assert_eq!(a.update(sample(10, 10)), 101, "app-limited: no change");
        assert_eq!(a.update(sample(500, 60)), 90, "slow: x0.9");
        assert_eq!(
            a.update(Sample {
                dropped: true,
                ..sample(1, 60)
            }),
            81
        );
    }

    #[test]
    fn aimd_never_goes_below_min() {
        let mut a = Aimd::new(bounds(4, 10, 5), Duration::from_millis(1));
        for _ in 0..100 {
            assert!(a.update(sample(50, 5)) >= 4);
        }
    }

    // ── AdaptiveLimiter ───────────────────────────────────────────────────

    #[test]
    fn limiter_publishes_each_update() {
        let limiter = AdaptiveLimiter::new(LimitAlgorithm::Aimd(Aimd::new(
            bounds(1, 100, 10),
            Duration::from_secs(1),
        )));
        assert_eq!(limiter.limit(), 10);
        assert_eq!(limiter.record(sample(1, 10)), Some(11));
        assert_eq!(limiter.limit(), 11);
    }

    #[test]
    fn limiter_drops_a_sample_while_locked() {
        let limiter = AdaptiveLimiter::new(LimitAlgorithm::Aimd(Aimd::new(
            bounds(1, 100, 10),
            Duration::from_secs(1),
        )));
        let held = limiter.algorithm.lock().unwrap();
        assert_eq!(limiter.record(sample(1, 10)), None);
        drop(held);
        assert_eq!(limiter.limit(), 10);
    }
}
