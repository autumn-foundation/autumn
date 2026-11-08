//! A lock-free latency histogram that answers "what is the p95 right now?".
//!
//! [`hedged`](crate::hedge::hedged) says to pick a delay near the p95 of the
//! call. [`LatencyTracker`] measures that p95 so the delay follows the
//! upstream instead of a number someone guessed in a config file.
//!
//! Samples land in log-linear buckets: four per power of two of microseconds,
//! so a reported quantile is at most 25 % above the true value and never
//! below it. Under 4 µs the buckets are one microsecond wide, so a quantile
//! there is rounded up to the next whole microsecond. Recording is one atomic add. Memory is fixed (128 counters), no
//! matter how many samples arrive. Call [`LatencyTracker::decay`] on a timer
//! to let old samples fade so the quantile tracks the recent past.
//!
//! ```
//! use std::time::Duration;
//! use autumn_web::latency_tracker::LatencyTracker;
//!
//! let t = LatencyTracker::new();
//! for ms in 1..=100 {
//!     t.record(Duration::from_millis(ms));
//! }
//! let p95 = t.quantile(0.95).unwrap();
//! assert!(p95 >= Duration::from_millis(95) && p95 <= Duration::from_millis(120));
//!
//! // Hedge a call after its own p95.
//! let policy = autumn_web::hedge::HedgePolicy::from_tracker(
//!     &t, 0.95, Duration::from_millis(200),
//! );
//! assert_eq!(policy.delay(), p95);
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const SUB_BITS: u32 = 2;
const SUBS: usize = 1 << SUB_BITS;
const OCTAVES: usize = 32;
const BUCKETS: usize = OCTAVES * SUBS;

/// A fixed-size streaming latency histogram. Cheap to share: wrap it in an
/// `Arc` and record from any task.
#[derive(Debug)]
pub struct LatencyTracker {
    buckets: [AtomicU64; BUCKETS],
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Count one sample. Anything under 1 µs counts as 1 µs; anything over
    /// about 71 minutes counts as the top bucket.
    pub fn record(&self, latency: Duration) {
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        self.buckets[bucket_of(micros)].fetch_add(1, Ordering::Relaxed);
    }

    /// How many samples are counted.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
    }

    /// The latency at or below which a fraction `q` (0.0 to 1.0) of the
    /// samples fall. `None` while the tracker is empty. `q` is clamped.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<Duration> {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let q = if q.is_nan() { 1.0 } else { q.clamp(0.0, 1.0) };
        // Rank of the sample we want, 1-based.
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        #[allow(clippy::cast_sign_loss)]
        let rank = ((q * total as f64).ceil() as u64).clamp(1, total);
        let mut seen = 0;
        for (i, c) in counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Some(Duration::from_micros(upper_bound(i)));
            }
        }
        None
    }

    /// Halve every counter so old samples count for less. Call it on a timer
    /// (every minute, say) to make the quantile follow recent traffic.
    pub fn decay(&self) {
        for b in &self.buckets {
            let mut cur = b.load(Ordering::Relaxed);
            while let Err(seen) =
                b.compare_exchange_weak(cur, cur / 2, Ordering::Relaxed, Ordering::Relaxed)
            {
                cur = seen;
            }
        }
    }

    /// Forget every sample.
    pub fn reset(&self) {
        for b in &self.buckets {
            b.store(0, Ordering::Relaxed);
        }
    }
}

fn bucket_of(micros: u64) -> usize {
    let v = micros.max(1);
    let octave = v.ilog2() as usize;
    if octave >= OCTAVES {
        return BUCKETS - 1;
    }
    let sub = if octave >= SUB_BITS as usize {
        (v >> (octave - SUB_BITS as usize)) & (SUBS as u64 - 1)
    } else {
        (v << (SUB_BITS as usize - octave)) & (SUBS as u64 - 1)
    };
    octave * SUBS + usize::try_from(sub).unwrap_or(0)
}

/// The first microsecond value past bucket `i`, rounded up so a quantile is
/// never reported below the samples it covers.
fn upper_bound(i: usize) -> u64 {
    let octave = i / SUBS;
    let sub = u64::try_from(i % SUBS).unwrap_or(0);
    let subs = SUBS as u64;
    ((subs + sub + 1) << octave).div_ceil(subs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn empty_tracker_has_no_quantile() {
        assert_eq!(LatencyTracker::new().quantile(0.5), None);
    }

    #[test]
    fn quantile_is_never_below_and_within_a_quarter_above() {
        let t = LatencyTracker::new();
        for ms in 1..=1_000 {
            t.record(MS(ms));
        }
        for (q, truth) in [(0.5, 500u64), (0.95, 950), (0.99, 990)] {
            let got = t.quantile(q).unwrap();
            assert!(got >= MS(truth), "p{q} {got:?} < {truth}ms");
            assert!(got <= MS(truth) * 5 / 4 + MS(1), "p{q} {got:?} too high");
        }
    }

    #[test]
    fn every_bucket_bound_covers_its_samples() {
        for micros in [4u64, 7, 8, 100, 1_023, 1_024, 999_999, 5_000_000] {
            let t = LatencyTracker::new();
            t.record(Duration::from_micros(micros));
            let got = u64::try_from(t.quantile(1.0).unwrap().as_micros()).unwrap();
            assert!(got >= micros, "{micros}µs reported as {got}µs");
            assert!(got * 4 <= micros * 5, "{micros}µs reported as {got}µs");
        }
    }

    #[test]
    fn sub_four_micro_samples_round_up_to_the_next_microsecond() {
        for micros in [1u64, 2, 3] {
            let t = LatencyTracker::new();
            t.record(Duration::from_micros(micros));
            assert_eq!(t.quantile(1.0), Some(Duration::from_micros(micros + 1)));
        }
    }

    #[test]
    fn huge_and_zero_samples_do_not_panic() {
        let t = LatencyTracker::new();
        t.record(Duration::ZERO);
        t.record(Duration::MAX);
        assert_eq!(t.count(), 2);
        assert!(t.quantile(0.0).unwrap() <= Duration::from_micros(2));
        assert!(t.quantile(f64::NAN).unwrap() > Duration::from_mins(1));
    }

    #[test]
    fn decay_lets_recent_traffic_win() {
        let t = LatencyTracker::new();
        for _ in 0..64 {
            t.record(MS(1_000));
        }
        for _ in 0..6 {
            t.decay();
        }
        for _ in 0..64 {
            t.record(MS(10));
        }
        assert!(t.quantile(0.5).unwrap() < MS(20));
        t.reset();
        assert_eq!(t.count(), 0);
    }
}
