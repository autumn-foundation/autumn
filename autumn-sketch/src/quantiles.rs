//! Bounded-memory quantiles: p50, p99 and p99.9 of an endless stream.
//!
//! "What is the p99 latency of checkout?", "how big is the 95th-percentile
//! upload?" An exact answer keeps every sample and sorts them. A sketch keeps
//! a few hundred counters and answers within a relative error you choose.
//!
//! [`DdSketch`] is `DDSketch` (Masson, Rim and Lee, 2019). A positive value `x`
//! lands in bucket `ceil(log_γ x)`, where `γ = (1 + α) / (1 - α)` and `α` is
//! the relative accuracy. Every value in a bucket is reported as the same
//! representative, and that representative is within `α` of each of them. So:
//!
//! - **Every quantile is within `α` of the true sample at its rank**, whatever
//!   the shape of the data: long tails, bursts and bimodal latency included.
//! - **Sketches merge exactly.** Two sketches of the same accuracy and cap
//!   add bucket by bucket, and the sum equals the sketch of both streams
//!   (see [`DdSketch::merge`] for folded sketches). Each pod can
//!   sketch its own traffic and a reporter adds them up, which an average of
//!   per-pod percentiles can never do.
//! - **Memory is capped.** Past `max_buckets` the two lowest buckets fold
//!   together, so the low end loses accuracy first. A quantile whose rank
//!   falls in the folded bucket loses its guarantee, whatever `q` is: if 99 %
//!   of the samples sit below the kept range, p99 reads high too. Quantiles
//!   ranked above the folded bucket keep it. [`DdSketch::is_collapsed`] says
//!   when folding has happened; at 1 % accuracy the default cap covers a
//!   nanosecond to over a decade, so request latencies never fold.
//!
//! The rank of quantile `q` over `n` samples is `floor(q * (n - 1))`. `q = 0`
//! and `q = 1` return the exact minimum and maximum. Zero has its own bucket.
//! Negative, NaN and infinite values have no rank and are ignored. Buckets are
//! kept in key order, so the same stream gives the same sketch on every run
//! (simulation tests replay byte-identically).
//!
//! [`Quantiles`] is the shared form: a [`DdSketch`] behind a mutex, to put in
//! an `Arc` and record from every handler.
//!
//! ```rust
//! use autumn_sketch::quantiles::Quantiles;
//! use std::time::Duration;
//!
//! let latency = Quantiles::new(0.01);
//! for ms in 1..=1_000 {
//!     latency.observe_duration(Duration::from_millis(ms));
//! }
//! let p99 = latency.quantile_duration(0.99).unwrap();
//! assert!(p99 >= Duration::from_millis(980) && p99 <= Duration::from_millis(1_000));
//! ```

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

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// The relative accuracy [`DdSketch::new`] uses for a NaN: 1 %.
pub const DEFAULT_RELATIVE_ACCURACY: f64 = 0.01;
/// The finest accuracy a sketch takes: 0.01 %.
pub const MIN_RELATIVE_ACCURACY: f64 = 1e-4;
/// The coarsest accuracy a sketch takes: 50 %.
pub const MAX_RELATIVE_ACCURACY: f64 = 0.5;
/// The default bucket cap. At 1 % accuracy, 2048 buckets cover values from
/// one nanosecond to over a decade without folding.
pub const DEFAULT_MAX_BUCKETS: usize = 2048;

/// [`DdSketch::merge`] refused a sketch of another accuracy: their buckets
/// have different bounds, so adding them would break both guarantees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IncompatibleSketch {
    /// The accuracy of the sketch merged into.
    pub ours: f64,
    /// The accuracy of the sketch offered.
    pub theirs: f64,
}

impl fmt::Display for IncompatibleSketch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge a sketch of relative accuracy {} into one of {}",
            self.theirs, self.ours
        )
    }
}

impl std::error::Error for IncompatibleSketch {}

/// A relative-error quantile sketch. See the [module docs](self).
#[derive(Debug, Clone, PartialEq)]
pub struct DdSketch {
    alpha: f64,
    ln_gamma: f64,
    /// `ln(2 / (γ + 1))`: bucket `k` is reported as `exp(k ln γ + ln_offset)`.
    ln_offset: f64,
    max_buckets: usize,
    buckets: BTreeMap<i32, u64>,
    zeros: u64,
    count: u64,
    min: f64,
    max: f64,
    collapsed: bool,
}

impl DdSketch {
    /// An empty sketch whose quantiles are within `relative_accuracy` of the
    /// truth. The accuracy is clamped to
    /// [`MIN_RELATIVE_ACCURACY`] to [`MAX_RELATIVE_ACCURACY`]; a NaN is taken
    /// as [`DEFAULT_RELATIVE_ACCURACY`].
    #[must_use]
    pub fn new(relative_accuracy: f64) -> Self {
        let alpha = if relative_accuracy.is_nan() {
            DEFAULT_RELATIVE_ACCURACY
        } else {
            relative_accuracy.clamp(MIN_RELATIVE_ACCURACY, MAX_RELATIVE_ACCURACY)
        };
        let gamma = (1.0 + alpha) / (1.0 - alpha);
        Self {
            alpha,
            ln_gamma: gamma.ln(),
            ln_offset: (2.0 / (gamma + 1.0)).ln(),
            max_buckets: DEFAULT_MAX_BUCKETS,
            buckets: BTreeMap::new(),
            zeros: 0,
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            collapsed: false,
        }
    }

    /// Caps the sketch at `max_buckets` buckets. A cap of `0` is taken as `1`.
    #[must_use]
    pub fn with_max_buckets(mut self, max_buckets: usize) -> Self {
        self.max_buckets = max_buckets.max(1);
        self.fold_to_cap();
        self
    }

    /// The relative accuracy every quantile is within.
    #[must_use]
    pub const fn relative_accuracy(&self) -> f64 {
        self.alpha
    }

    /// The bucket cap.
    #[must_use]
    pub const fn max_buckets(&self) -> usize {
        self.max_buckets
    }

    /// The number of non-zero buckets in use.
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the cap has folded low buckets together. A quantile whose rank
    /// falls in the folded (lowest) bucket may then read high; quantiles
    /// ranked above it keep their accuracy.
    #[must_use]
    pub const fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// The number of values recorded. Saturates at `u64::MAX`.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The exact smallest value recorded.
    #[must_use]
    pub fn min(&self) -> Option<f64> {
        (self.count > 0).then_some(self.min)
    }

    /// The exact largest value recorded.
    #[must_use]
    pub fn max(&self) -> Option<f64> {
        (self.count > 0).then_some(self.max)
    }

    /// Records one value. Negative, NaN and infinite values are ignored.
    pub fn observe(&mut self, value: f64) {
        if !value.is_finite() || value < 0.0 {
            return;
        }
        if value > 0.0 {
            let slot = self.buckets.entry(self.key(value)).or_insert(0);
            *slot = slot.saturating_add(1);
            self.fold_to_cap();
        } else {
            self.zeros = self.zeros.saturating_add(1);
        }
        self.count = self.count.saturating_add(1);
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    /// Records a duration, in seconds.
    pub fn observe_duration(&mut self, duration: Duration) {
        self.observe(duration.as_secs_f64());
    }

    /// The value at quantile `q`, clamped to `0..=1`. `None` when the sketch
    /// is empty or `q` is NaN.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 || q.is_nan() {
            return None;
        }
        if q <= 0.0 {
            return Some(self.min);
        }
        if q >= 1.0 {
            return Some(self.max);
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "a rank past 2^53 samples is approximate either way"
        )]
        let rank = (q * (self.count.saturating_sub(1)) as f64).floor();
        let mut seen = self.zeros;
        #[allow(clippy::cast_precision_loss, reason = "as above")]
        if seen as f64 > rank {
            return Some(0.0);
        }
        for (&key, &n) in &self.buckets {
            seen = seen.saturating_add(n);
            #[allow(clippy::cast_precision_loss, reason = "as above")]
            if seen as f64 > rank {
                return Some(self.value(key).clamp(self.min, self.max));
            }
        }
        Some(self.max)
    }

    /// [`quantile`](Self::quantile) of values recorded as durations.
    #[must_use]
    pub fn quantile_duration(&self, q: f64) -> Option<Duration> {
        self.quantile(q)
            .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
    }

    /// Adds `other` into this sketch. The result equals the sketch of both
    /// streams, folded to this sketch's cap, when `other` has not folded or
    /// this cap is no larger than `other`'s. A folded `other` merged under a
    /// larger cap keeps its folded mass in its lowest bucket, so quantiles
    /// ranked there stay as inaccurate as they were in `other`.
    ///
    /// # Errors
    ///
    /// [`IncompatibleSketch`] when the accuracies differ. Nothing is changed.
    pub fn merge(&mut self, other: &Self) -> Result<(), IncompatibleSketch> {
        if self.alpha.to_bits() != other.alpha.to_bits() {
            return Err(IncompatibleSketch {
                ours: self.alpha,
                theirs: other.alpha,
            });
        }
        if other.count == 0 {
            return Ok(());
        }
        for (&key, &n) in &other.buckets {
            let slot = self.buckets.entry(key).or_insert(0);
            *slot = slot.saturating_add(n);
        }
        self.zeros = self.zeros.saturating_add(other.zeros);
        self.count = self.count.saturating_add(other.count);
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.collapsed |= other.collapsed;
        self.fold_to_cap();
        Ok(())
    }

    /// Forgets every value; keeps the accuracy and the cap.
    pub fn clear(&mut self) {
        *self = self.empty_like();
    }

    /// An empty sketch with this one's accuracy and cap.
    fn empty_like(&self) -> Self {
        Self::new(self.alpha).with_max_buckets(self.max_buckets)
    }

    fn key(&self, value: f64) -> i32 {
        // `ln` of a positive finite f64 is within ±745 and `ln_gamma` is at
        // least 2e-4, so the key fits an i32 with room to spare.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "bounded as above; `as` saturates in any case"
        )]
        let key = (value.ln() / self.ln_gamma).ceil() as i32;
        key
    }

    fn value(&self, key: i32) -> f64 {
        // `2 γ^k / (γ + 1)` in one exponent: `γ^k` alone overflows for the
        // top buckets of the f64 range, while the representative itself
        // stays within `α` of the values in its bucket.
        f64::from(key).mul_add(self.ln_gamma, self.ln_offset).exp()
    }

    fn fold_to_cap(&mut self) {
        while self.buckets.len() > self.max_buckets {
            let Some((_, low)) = self.buckets.pop_first() else {
                return;
            };
            if let Some(mut next) = self.buckets.first_entry() {
                let slot = next.get_mut();
                *slot = slot.saturating_add(low);
            }
            self.collapsed = true;
        }
    }
}

/// A [`DdSketch`] to share between threads. See the [module docs](self).
#[derive(Debug)]
pub struct Quantiles {
    sketch: Mutex<DdSketch>,
}

impl Quantiles {
    /// An empty shared sketch. See [`DdSketch::new`].
    #[must_use]
    pub fn new(relative_accuracy: f64) -> Self {
        Self::from_sketch(DdSketch::new(relative_accuracy))
    }

    /// Shares an existing sketch, for example one built with
    /// [`DdSketch::with_max_buckets`].
    #[must_use]
    pub const fn from_sketch(sketch: DdSketch) -> Self {
        Self {
            sketch: Mutex::new(sketch),
        }
    }

    /// Records one value. See [`DdSketch::observe`].
    pub fn observe(&self, value: f64) {
        self.with(|sketch| sketch.observe(value));
    }

    /// Records a duration, in seconds.
    pub fn observe_duration(&self, duration: Duration) {
        self.with(|sketch| sketch.observe_duration(duration));
    }

    /// The number of values recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.with(|sketch| sketch.count())
    }

    /// The value at quantile `q`. See [`DdSketch::quantile`].
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<f64> {
        self.with(|sketch| sketch.quantile(q))
    }

    /// The duration at quantile `q`. See [`DdSketch::quantile_duration`].
    #[must_use]
    pub fn quantile_duration(&self, q: f64) -> Option<Duration> {
        self.with(|sketch| sketch.quantile_duration(q))
    }

    /// A copy of the sketch as it stands, to merge or report.
    #[must_use]
    pub fn snapshot(&self) -> DdSketch {
        self.with(|sketch| sketch.clone())
    }

    /// Returns the sketch as it stands and starts an empty one, so each
    /// reporting window counts from zero.
    #[must_use]
    pub fn take(&self) -> DdSketch {
        self.with(|sketch| {
            let fresh = sketch.empty_like();
            std::mem::replace(sketch, fresh)
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut DdSketch) -> R) -> R {
        // A panic while recording leaves the sketch readable: every write is
        // a saturating add or a min/max.
        let mut sketch = self.sketch.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut sketch)
    }
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "min, max and the clamped accuracy are exact by contract"
)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The sample a sketch of `values` reports at `q`: rank `floor(q * (n - 1))`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn exact(values: &[f64], q: f64) -> f64 {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        sorted[(q * (sorted.len() - 1) as f64).floor() as usize]
    }

    fn assert_relative(got: f64, want: f64, alpha: f64) {
        let bound = want.abs() * alpha * (1.0 + 1e-9);
        assert!(
            (got - want).abs() <= bound,
            "got {got}, want {want} within {alpha}"
        );
    }

    #[test]
    fn an_empty_sketch_has_no_quantiles() {
        let sketch = DdSketch::new(0.01);
        assert_eq!(sketch.count(), 0);
        assert_eq!(sketch.quantile(0.5), None);
        assert_eq!(sketch.min(), None);
        assert_eq!(sketch.max(), None);
    }

    #[test]
    fn min_and_max_are_exact() {
        let mut sketch = DdSketch::new(0.05);
        for x in [3.0, 17.25, 0.5, 9.0] {
            sketch.observe(x);
        }
        assert_eq!(sketch.quantile(0.0), Some(0.5));
        assert_eq!(sketch.quantile(1.0), Some(17.25));
        assert_eq!(sketch.min(), Some(0.5));
        assert_eq!(sketch.max(), Some(17.25));
    }

    #[test]
    fn every_quantile_is_within_the_relative_accuracy() {
        let alpha = 0.01;
        let mut sketch = DdSketch::new(alpha);
        // A long-tailed latency shape: most requests fast, a few very slow.
        let values: Vec<f64> = (1..=10_000u32)
            .map(|i| {
                let i = f64::from(i);
                (i / 10_000.0).powi(8).mul_add(4.0, 0.002)
            })
            .collect();
        for &x in &values {
            sketch.observe(x);
        }
        assert_eq!(sketch.count(), 10_000);
        for q in [0.01, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99, 0.999] {
            let got = sketch.quantile(q).expect("non-empty");
            assert_relative(got, exact(&values, q), alpha);
        }
    }

    #[test]
    fn zeros_have_their_own_bucket() {
        let mut sketch = DdSketch::new(0.01);
        for x in [0.0, 0.0, 0.0, 5.0] {
            sketch.observe(x);
        }
        assert_eq!(sketch.count(), 4);
        assert_eq!(sketch.quantile(0.5), Some(0.0));
        assert_relative(sketch.quantile(0.99).expect("non-empty"), 0.0, 0.01);
        assert_eq!(sketch.quantile(1.0), Some(5.0));
    }

    #[test]
    fn values_with_no_rank_are_ignored() {
        let mut sketch = DdSketch::new(0.01);
        for x in [f64::NAN, -1.0, f64::INFINITY, f64::NEG_INFINITY] {
            sketch.observe(x);
        }
        assert_eq!(sketch.count(), 0);
        sketch.observe(2.0);
        assert_eq!(sketch.count(), 1);
        assert_eq!(sketch.quantile(0.5), Some(2.0));
    }

    #[test]
    fn out_of_range_quantiles_are_clamped() {
        let mut sketch = DdSketch::new(0.01);
        sketch.observe(1.0);
        sketch.observe(100.0);
        assert_eq!(sketch.quantile(-3.0), Some(1.0));
        assert_eq!(sketch.quantile(7.0), Some(100.0));
        assert_eq!(sketch.quantile(f64::NAN), None);
    }

    #[test]
    fn accuracy_is_clamped_to_a_usable_range() {
        assert_eq!(
            DdSketch::new(f64::NAN).relative_accuracy(),
            DEFAULT_RELATIVE_ACCURACY
        );
        assert_eq!(
            DdSketch::new(0.0).relative_accuracy(),
            MIN_RELATIVE_ACCURACY
        );
        assert_eq!(
            DdSketch::new(0.9).relative_accuracy(),
            MAX_RELATIVE_ACCURACY
        );
    }

    /// Merging two halves gives exactly the sketch of the whole stream, so
    /// every pod can sketch its own traffic and a reporter adds them up.
    #[test]
    fn merging_halves_equals_sketching_the_whole() {
        let mut whole = DdSketch::new(0.02);
        let mut left = DdSketch::new(0.02);
        let mut right = DdSketch::new(0.02);
        for i in 1..=2_000u32 {
            let x = f64::from(i) * 0.37;
            whole.observe(x);
            if i % 3 == 0 {
                left.observe(x);
            } else {
                right.observe(x);
            }
        }
        left.merge(&right).expect("same accuracy");
        assert_eq!(left, whole);
        assert_eq!(left.quantile(0.9), whole.quantile(0.9));
    }

    #[test]
    fn merging_an_empty_sketch_changes_nothing() {
        let mut sketch = DdSketch::new(0.01);
        sketch.observe(4.0);
        let before = sketch.clone();
        sketch.merge(&DdSketch::new(0.01)).expect("same accuracy");
        assert_eq!(sketch, before);
        let mut empty = DdSketch::new(0.01);
        empty.merge(&before).expect("same accuracy");
        assert_eq!(empty, before);
    }

    #[test]
    fn sketches_of_different_accuracy_do_not_merge() {
        let mut fine = DdSketch::new(0.01);
        let mut coarse = DdSketch::new(0.05);
        coarse.observe(1.0);
        let err = fine.merge(&coarse).expect_err("different accuracy");
        assert_eq!(
            err,
            IncompatibleSketch {
                ours: 0.01,
                theirs: 0.05
            }
        );
        assert_eq!(fine.count(), 0);
    }

    /// Past the bucket cap the lowest buckets fold together. The tail, which
    /// is what a latency SLO reads, keeps its accuracy.
    #[test]
    fn the_bucket_cap_folds_the_low_end_and_keeps_the_tail() {
        let alpha = 0.01;
        let mut sketch = DdSketch::new(alpha).with_max_buckets(64);
        let values: Vec<f64> = (0..20_000u32)
            .map(|i| 1e-6 * 1.001_f64.powi(i.try_into().expect("small")))
            .collect();
        for &x in &values {
            sketch.observe(x);
        }
        assert!(sketch.bucket_count() <= 64);
        assert!(sketch.is_collapsed());
        assert_eq!(sketch.count(), 20_000);
        for q in [0.99, 0.999] {
            let got = sketch.quantile(q).expect("non-empty");
            assert_relative(got, exact(&values, q), alpha);
        }
        assert_eq!(sketch.quantile(0.0), sketch.min());
    }

    #[test]
    fn values_near_f64_max_keep_their_accuracy() {
        let mut sketch = DdSketch::new(0.01);
        for x in [1e308, 1e308, 1.7e308] {
            sketch.observe(x);
        }
        assert_relative(sketch.quantile(0.5).expect("non-empty"), 1e308, 0.01);
    }

    /// A merge is exact when the recipient's cap is no larger than the cap
    /// the other sketch folded under.
    #[test]
    fn merging_a_folded_sketch_is_exact_at_its_cap_or_below() {
        let values = [1.0, 1.0, 1e3, 1e6];
        let mut folded = DdSketch::new(0.01).with_max_buckets(2);
        let mut whole = DdSketch::new(0.01).with_max_buckets(2);
        for x in values {
            folded.observe(x);
            whole.observe(x);
        }
        let mut recipient = DdSketch::new(0.01).with_max_buckets(2);
        recipient.merge(&folded).expect("same accuracy");
        assert_eq!(recipient, whole);
    }

    #[test]
    fn a_zero_bucket_cap_is_taken_as_one() {
        let mut sketch = DdSketch::new(0.01).with_max_buckets(0);
        assert_eq!(sketch.max_buckets(), 1);
        sketch.observe(1.0);
        sketch.observe(1_000.0);
        assert_eq!(sketch.bucket_count(), 1);
        assert_eq!(sketch.quantile(1.0), Some(1_000.0));
    }

    #[test]
    fn durations_are_recorded_in_seconds() {
        let mut sketch = DdSketch::new(0.01);
        sketch.observe_duration(Duration::from_millis(250));
        assert_relative(sketch.quantile(0.5).expect("non-empty"), 0.25, 0.01);
        assert_eq!(
            sketch.quantile_duration(0.5),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn clear_starts_over() {
        let mut sketch = DdSketch::new(0.01).with_max_buckets(2);
        for x in [1.0, 10.0, 100.0] {
            sketch.observe(x);
        }
        assert!(sketch.is_collapsed());
        sketch.clear();
        assert_eq!(sketch, DdSketch::new(0.01).with_max_buckets(2));
    }

    #[test]
    fn shared_sketch_records_every_thread_and_take_resets_it() {
        let latency = Arc::new(Quantiles::new(0.01));
        let workers: Vec<_> = (0..4u32)
            .map(|t| {
                let latency = Arc::clone(&latency);
                std::thread::spawn(move || {
                    for i in 1..=250u32 {
                        latency.observe(f64::from(t * 250 + i));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker panicked");
        }
        assert_eq!(latency.count(), 1_000);
        assert_relative(latency.quantile(0.5).expect("non-empty"), 500.0, 0.01);
        assert_eq!(latency.quantile(1.0), Some(1_000.0));

        let window = latency.take();
        assert_eq!(window.count(), 1_000);
        assert_eq!(latency.count(), 0);
        assert_eq!(latency.snapshot().relative_accuracy(), 0.01);
    }
}
