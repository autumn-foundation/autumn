//! A failure detector that learns how often a peer speaks.
//!
//! A fixed timeout asks "has this peer been quiet for more than `T`?". The
//! right `T` depends on the peer: a link that heartbeats every 100 ms with
//! little jitter is suspicious after 400 ms of silence, while a peer across a
//! busy WAN is not. [`PhiDetector`] is the phi accrual detector of Hayashibara
//! et al. (*The φ Accrual Failure Detector*, 2004), in the form Cassandra and
//! Akka use. It keeps the last `window` heartbeat intervals and, given the
//! time since the last heartbeat, returns a suspicion level `phi`:
//!
//! ```text
//! phi = -log10(P(a heartbeat arrives later than now))
//! ```
//!
//! `phi = 1` means about a 10 % chance the peer is still fine, `phi = 3`
//! 0.1 %, `phi = 8` one in a hundred million. The caller picks a threshold
//! once, in units of confidence, and the detector turns it into a timeout per
//! peer that follows that peer's rhythm as it speeds up or slows down.
//!
//! Intervals are modelled as a normal distribution. `min_std_dev` keeps a
//! perfectly regular peer from becoming suspicious one tick after it is late,
//! and `acceptable_pause` is added to the mean to absorb a known pause (a GC,
//! a deploy restart) without raising `phi`.
//!
//! Time is passed in, so a [`Sim`](crate::sim::Sim) or a test replays it
//! exactly.
//!
//! ```
//! use std::time::{Duration, Instant};
//! use autumn_web::phi_accrual::PhiDetector;
//!
//! let start = Instant::now();
//! let mut peer = PhiDetector::new(Duration::from_millis(100));
//! for tick in 0..10 {
//!     peer.heartbeat_at(start + Duration::from_millis(100 * tick));
//! }
//! let last = start + Duration::from_millis(900);
//!
//! assert!(peer.is_available_at(last + Duration::from_millis(100), 8.0));
//! assert!(!peer.is_available_at(last + Duration::from_secs(5), 8.0));
//! ```

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The default number of heartbeat intervals kept.
pub const DEFAULT_WINDOW: usize = 1000;
/// The default floor on the standard deviation of intervals.
pub const DEFAULT_MIN_STD_DEV: Duration = Duration::from_millis(100);
/// The suspicion level Akka and Cassandra treat as "down" by default.
pub const DEFAULT_THRESHOLD: f64 = 8.0;
/// The smallest standard deviation used, whatever `min_std_dev` says, so a
/// perfectly regular window never divides by zero.
const STD_DEV_FLOOR_SECS: f64 = 1e-6;

/// A phi accrual failure detector for one peer. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct PhiDetector {
    first_interval: Duration,
    min_std_dev: Duration,
    acceptable_pause: Duration,
    window: usize,
    last: Option<Instant>,
    intervals: VecDeque<Duration>,
}

impl PhiDetector {
    /// A detector that expects a heartbeat about every `first_interval`
    /// until it has measured some.
    #[must_use]
    pub const fn new(first_interval: Duration) -> Self {
        Self {
            first_interval,
            min_std_dev: DEFAULT_MIN_STD_DEV,
            acceptable_pause: Duration::ZERO,
            window: DEFAULT_WINDOW,
            last: None,
            intervals: VecDeque::new(),
        }
    }

    /// Keep the last `window` intervals (at least 1).
    #[must_use]
    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window.max(1);
        self.trim();
        self
    }

    /// Never model the intervals as tighter than `min_std_dev`. Values under
    /// 1 µs are treated as 1 µs.
    #[must_use]
    pub const fn with_min_std_dev(mut self, min_std_dev: Duration) -> Self {
        self.min_std_dev = min_std_dev;
        self
    }

    /// Add `pause` to the expected interval before computing `phi`.
    #[must_use]
    pub const fn with_acceptable_pause(mut self, pause: Duration) -> Self {
        self.acceptable_pause = pause;
        self
    }

    /// Record a heartbeat received at `now`.
    ///
    /// The first heartbeat seeds the window with `first_interval`, give or
    /// take a quarter. A heartbeat older than the last one is ignored.
    pub fn heartbeat_at(&mut self, now: Instant) {
        match self.last {
            Some(last) if now < last => return,
            Some(last) => self.intervals.push_back(now.duration_since(last)),
            None => {
                let spread = self.first_interval / 4;
                self.intervals
                    .push_back(self.first_interval.saturating_sub(spread));
                self.intervals
                    .push_back(self.first_interval.saturating_add(spread));
            }
        }
        self.last = Some(now);
        self.trim();
    }

    /// The suspicion level at `now`. `0.0` before the first heartbeat.
    #[must_use]
    pub fn phi_at(&self, now: Instant) -> f64 {
        let Some(last) = self.last else {
            return 0.0;
        };
        let (mean, std_dev) = self.distribution();
        let elapsed = now.saturating_duration_since(last).as_secs_f64();
        phi(elapsed, mean + self.acceptable_pause.as_secs_f64(), std_dev)
    }

    /// Whether `phi_at(now)` is below `threshold`.
    #[must_use]
    pub fn is_available_at(&self, now: Instant, threshold: f64) -> bool {
        self.phi_at(now) < threshold
    }

    /// The number of intervals currently in the window.
    #[must_use]
    pub fn samples(&self) -> usize {
        self.intervals.len()
    }

    /// Mean and floored standard deviation of the window, in seconds.
    fn distribution(&self) -> (f64, f64) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window never nears 2^52 samples"
        )]
        let n = self.intervals.len().max(1) as f64;
        let mean = self
            .intervals
            .iter()
            .map(Duration::as_secs_f64)
            .sum::<f64>()
            / n;
        let variance = self
            .intervals
            .iter()
            .map(|d| (d.as_secs_f64() - mean).powi(2))
            .sum::<f64>()
            / n;
        let floor = self.min_std_dev.as_secs_f64().max(STD_DEV_FLOOR_SECS);
        (mean, variance.sqrt().max(floor))
    }

    fn trim(&mut self) {
        while self.intervals.len() > self.window {
            self.intervals.pop_front();
        }
    }
}

/// `-log10(P(X > elapsed))` for `X ~ N(mean, std_dev)`.
///
/// Uses the logistic approximation of the normal CDF from the Akka detector,
/// `P(X > t) ~ 1 / (1 + e^z)` with `z = y (1.5976 + 0.070566 y^2)` and
/// `y = (t - mean) / std_dev`, so `phi = log10(1 + e^z)`. That is computed as
/// a softplus so neither a long silence nor an early check overflows.
fn phi(elapsed: f64, mean: f64, std_dev: f64) -> f64 {
    let y = (elapsed - mean) / std_dev;
    let z = y * 0.070_566f64.mul_add(y * y, 1.5976);
    let softplus = if z > 0.0 {
        z + (-z).exp().ln_1p()
    } else {
        z.exp().ln_1p()
    };
    softplus / std::f64::consts::LN_10
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn steady(detector: &mut PhiDetector, start: Instant, every: Duration, beats: u32) -> Instant {
        let mut last = start;
        for beat in 0..beats {
            last = start + every * beat;
            detector.heartbeat_at(last);
        }
        last
    }

    #[test]
    fn unknown_peer_is_not_suspected() {
        let detector = PhiDetector::new(100 * MS);
        assert!(detector.phi_at(Instant::now()).abs() < f64::EPSILON);
        assert_eq!(detector.samples(), 0);
    }

    #[test]
    fn first_heartbeat_seeds_from_first_interval() {
        let start = Instant::now();
        let mut detector = PhiDetector::new(1000 * MS);
        detector.heartbeat_at(start);
        assert!(detector.samples() > 0);
        assert!(detector.is_available_at(start + 1000 * MS, DEFAULT_THRESHOLD));
        assert!(!detector.is_available_at(start + 10_000 * MS, DEFAULT_THRESHOLD));
    }

    #[test]
    fn phi_at_the_mean_is_a_coin_flip() {
        let at_mean = phi(1.0, 1.0, 0.1);
        assert!((at_mean - 2f64.log10()).abs() < 1e-9, "{at_mean}");
        assert!(phi(0.0, 1.0, 0.1) < 1e-6);
    }

    #[test]
    fn phi_rises_with_silence() {
        let start = Instant::now();
        let mut detector = PhiDetector::new(100 * MS);
        let last = steady(&mut detector, start, 100 * MS, 50);
        let mut previous = detector.phi_at(last);
        for quiet in [50, 100, 200, 400, 800, 1600] {
            let phi = detector.phi_at(last + quiet * MS);
            assert!(phi > previous, "{quiet} ms: {phi} <= {previous}");
            previous = phi;
        }
    }

    #[test]
    fn suspicion_follows_the_peer_rhythm() {
        let start = Instant::now();
        let mut fast = PhiDetector::new(100 * MS).with_min_std_dev(10 * MS);
        let mut slow = PhiDetector::new(100 * MS).with_min_std_dev(10 * MS);
        let fast_last = steady(&mut fast, start, 100 * MS, 100);
        let slow_last = steady(&mut slow, start, 2000 * MS, 100);

        let silence = 1000 * MS;
        assert!(!fast.is_available_at(fast_last + silence, DEFAULT_THRESHOLD));
        assert!(slow.is_available_at(slow_last + silence, DEFAULT_THRESHOLD));
    }

    #[test]
    fn jitter_widens_tolerance() {
        let start = Instant::now();
        let mut regular = PhiDetector::new(100 * MS).with_min_std_dev(MS);
        let mut jittery = PhiDetector::new(100 * MS).with_min_std_dev(MS);
        let regular_last = steady(&mut regular, start, 100 * MS, 100);
        let mut jittery_last = start;
        for beat in 0..100u32 {
            jittery.heartbeat_at(jittery_last);
            jittery_last += if beat % 2 == 0 { 40 * MS } else { 160 * MS };
        }
        jittery_last += 40 * MS;
        jittery.heartbeat_at(jittery_last);

        let late = 200 * MS;
        assert!(jittery.phi_at(jittery_last + late) < regular.phi_at(regular_last + late));
    }

    #[test]
    fn acceptable_pause_lowers_phi() {
        let start = Instant::now();
        let mut plain = PhiDetector::new(100 * MS);
        let mut paused = PhiDetector::new(100 * MS).with_acceptable_pause(1000 * MS);
        let last = steady(&mut plain, start, 100 * MS, 50);
        steady(&mut paused, start, 100 * MS, 50);
        let at = last + 800 * MS;
        assert!(paused.phi_at(at) < plain.phi_at(at));
        assert!(paused.is_available_at(at, DEFAULT_THRESHOLD));
    }

    #[test]
    fn window_caps_samples_and_forgets_old_rhythm() {
        let start = Instant::now();
        let mut detector = PhiDetector::new(100 * MS)
            .with_window(10)
            .with_min_std_dev(10 * MS);
        let last = steady(&mut detector, start, 2000 * MS, 20);
        assert_eq!(detector.samples(), 10);
        assert!(detector.is_available_at(last + 1500 * MS, DEFAULT_THRESHOLD));

        let last = steady(&mut detector, last + 100 * MS, 100 * MS, 20);
        assert_eq!(detector.samples(), 10);
        assert!(!detector.is_available_at(last + 1500 * MS, DEFAULT_THRESHOLD));
    }

    #[test]
    fn shrinking_window_trims_history() {
        let start = Instant::now();
        let mut detector = PhiDetector::new(100 * MS);
        steady(&mut detector, start, 100 * MS, 30);
        let detector = detector.with_window(5);
        assert_eq!(detector.samples(), 5);
    }

    #[test]
    fn heartbeat_out_of_order_is_ignored() {
        let start = Instant::now();
        let mut detector = PhiDetector::new(100 * MS);
        let last = steady(&mut detector, start + 1000 * MS, 100 * MS, 10);
        let samples = detector.samples();
        detector.heartbeat_at(start);
        assert_eq!(detector.samples(), samples);
        assert!(detector.is_available_at(last + 100 * MS, DEFAULT_THRESHOLD));
    }

    #[test]
    fn zero_min_std_dev_on_a_perfect_rhythm_stays_defined() {
        let start = Instant::now();
        let mut detector = PhiDetector::new(100 * MS)
            .with_window(1)
            .with_min_std_dev(Duration::ZERO);
        let last = steady(&mut detector, start, 100 * MS, 3);
        let on_time = detector.phi_at(last + 100 * MS);
        assert!(on_time.is_finite(), "{on_time}");
        assert!(detector.is_available_at(last + 100 * MS, DEFAULT_THRESHOLD));
        let late = detector.phi_at(last + Duration::from_hours(1));
        assert!(late.is_finite() && late > DEFAULT_THRESHOLD, "{late}");
    }

    #[test]
    fn phi_is_finite_far_past_the_last_heartbeat() {
        let start = Instant::now();
        let mut detector = PhiDetector::new(100 * MS).with_min_std_dev(MS);
        let last = steady(&mut detector, start, 100 * MS, 10);
        let phi = detector.phi_at(last + Duration::from_hours(1));
        assert!(phi.is_finite());
        assert!(phi > DEFAULT_THRESHOLD);
    }
}
