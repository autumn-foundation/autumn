//! Bounded-memory quantiles: the p50, p99 and p999 of an endless stream.
//!
//! "What is our p99 latency on the checkout route?", "how big is the
//! 95th-percentile upload?", "how long does the slowest 1 % of jobs take?"
//! An exact answer keeps every sample and sorts it, and that buffer grows
//! with the traffic. A fixed window of recent samples forgets the tail as
//! soon as it scrolls by. [`QuantileSketch`] keeps a few kilobytes and
//! answers any quantile of everything it has seen.
//!
//! It is `DDSketch` (Masson, Rim and Lee, 2019). Each positive value goes
//! into a bucket whose bounds grow geometrically by
//! `gamma = (1 + a) / (1 - a)`, for a relative accuracy `a`. Every value in a
//! bucket is within `a` of the bucket's midpoint, so a quantile read off the
//! buckets is within `a` of the true sample at that rank, whatever the
//! distribution. Latencies spanning microseconds to minutes need about 1,000
//! buckets at 1 %, and the count does not grow with the traffic.
//!
//! - Accuracy is relative, not by rank: the p99 of a long-tailed latency
//!   stream comes back as "250 ms, give or take 2.5 ms", not as "somewhere
//!   between the p98.5 and the p99.5", which on a long tail can be seconds
//!   apart.
//! - The lowest and highest samples are kept exactly, so the 0th and 100th
//!   percentile are the true min and max. The exact sum and count give the
//!   mean.
//! - Two sketches of the same accuracy [`merge`](QuantileSketch::merge)
//!   exactly: the merged sketch is the sketch of both streams. Each replica
//!   measures its own requests and the merged sketch gives the fleet's p99,
//!   which averaging the replicas' p99s cannot.
//!   [`to_bytes`](QuantileSketch::to_bytes) and
//!   [`from_bytes`](QuantileSketch::from_bytes) carry a sketch between
//!   processes.
//! - Memory is capped by [`with_max_bins`](QuantileSketch::with_max_bins)
//!   (2048 by default, a few tens of KiB at most). Past the cap the lowest buckets
//!   fold into their neighbour, so the low quantiles read high and the tail,
//!   the part worth alerting on, stays exact to `a`.
//!
//! Values are non-negative and finite: durations, sizes, counts. Zero (and
//! anything below the smallest normal `f64`) has its own bucket. Negative,
//! NaN and infinite values are refused, so one bad reading cannot poison the
//! sketch.
//!
//! [`QuantileMeter`] is the shared form: a [`QuantileSketch`] behind a mutex,
//! to put in an `Arc` and record from every handler.
//!
//! ```rust
//! use std::time::Duration;
//! use autumn_web::quantiles::QuantileMeter;
//!
//! let latency = QuantileMeter::new(0.01);
//! for ms in 1..=1000 {
//!     latency.record(Duration::from_millis(ms));
//! }
//! let p99 = latency.quantile(0.99).unwrap();
//! assert!((p99 - 0.990).abs() <= 0.990 * 0.01);
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

/// Two sketches of different relative accuracies cannot be merged: their
/// buckets have different bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccuracyMismatch {
    /// The relative accuracy of the sketch merged into.
    pub expected: f64,
    /// The relative accuracy of the sketch offered.
    pub found: f64,
}

impl fmt::Display for AccuracyMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge a sketch of relative accuracy {} into one of {}",
            self.found, self.expected
        )
    }
}

impl std::error::Error for AccuracyMismatch {}

/// The version byte [`QuantileSketch::to_bytes`] writes first.
const FORMAT_VERSION: u8 = 1;

/// A bounded-memory quantile sketch. See the [module docs](self).
#[derive(Debug, Clone, PartialEq)]
pub struct QuantileSketch {
    relative_accuracy: f64,
    ln_gamma: f64,
    /// The midpoint of bucket `i` is `gamma^i * midpoint_factor`.
    midpoint_factor: f64,
    max_bins: usize,
    /// Bucket `i` counts the values in `(gamma^(i-1), gamma^i]`.
    bins: BTreeMap<i32, u64>,
    /// Values below `f64::MIN_POSITIVE`, zero included.
    zeros: u64,
    count: u64,
    sum: f64,
    /// Exact extremes. Both zero while the sketch is empty.
    min: f64,
    max: f64,
}

impl Default for QuantileSketch {
    /// A sketch of [`DEFAULT_RELATIVE_ACCURACY`](Self::DEFAULT_RELATIVE_ACCURACY).
    fn default() -> Self {
        Self::new(Self::DEFAULT_RELATIVE_ACCURACY)
    }
}

impl QuantileSketch {
    /// The finest relative accuracy: 0.01 %.
    pub const MIN_RELATIVE_ACCURACY: f64 = 0.0001;
    /// The coarsest relative accuracy: 50 %.
    pub const MAX_RELATIVE_ACCURACY: f64 = 0.5;
    /// 1 %: a p99 of 250 ms comes back between 247.5 and 252.5 ms.
    pub const DEFAULT_RELATIVE_ACCURACY: f64 = 0.01;
    /// The smallest bucket cap.
    pub const MIN_MAX_BINS: usize = 16;
    /// The largest bucket cap.
    pub const MAX_MAX_BINS: usize = 1 << 16;
    /// The default bucket cap: at 1 % it spans 17 orders of magnitude before
    /// a bucket is folded.
    pub const DEFAULT_MAX_BINS: usize = 2048;

    /// An empty sketch whose quantiles are within `relative_accuracy` of the
    /// truth. It is clamped to
    /// [`MIN_RELATIVE_ACCURACY`](Self::MIN_RELATIVE_ACCURACY)..=[`MAX_RELATIVE_ACCURACY`](Self::MAX_RELATIVE_ACCURACY),
    /// and NaN means [`DEFAULT_RELATIVE_ACCURACY`](Self::DEFAULT_RELATIVE_ACCURACY).
    #[must_use]
    pub fn new(relative_accuracy: f64) -> Self {
        let relative_accuracy = if relative_accuracy.is_nan() {
            Self::DEFAULT_RELATIVE_ACCURACY
        } else {
            relative_accuracy.clamp(Self::MIN_RELATIVE_ACCURACY, Self::MAX_RELATIVE_ACCURACY)
        };
        Self::with_parts(relative_accuracy, Self::DEFAULT_MAX_BINS)
    }

    /// Caps the number of buckets, clamped to
    /// [`MIN_MAX_BINS`](Self::MIN_MAX_BINS)..=[`MAX_MAX_BINS`](Self::MAX_MAX_BINS).
    /// Past the cap the lowest buckets fold upward.
    #[must_use]
    pub fn with_max_bins(mut self, max_bins: usize) -> Self {
        self.max_bins = max_bins.clamp(Self::MIN_MAX_BINS, Self::MAX_MAX_BINS);
        self.collapse();
        self
    }

    fn with_parts(relative_accuracy: f64, max_bins: usize) -> Self {
        let gamma = (1.0 + relative_accuracy) / (1.0 - relative_accuracy);
        Self {
            relative_accuracy,
            ln_gamma: gamma.ln(),
            midpoint_factor: 2.0 / (1.0 + gamma),
            max_bins,
            bins: BTreeMap::new(),
            zeros: 0,
            count: 0,
            sum: 0.0,
            min: 0.0,
            max: 0.0,
        }
    }

    /// The relative accuracy every quantile is within.
    #[must_use]
    pub const fn relative_accuracy(&self) -> f64 {
        self.relative_accuracy
    }

    /// The bucket cap.
    #[must_use]
    pub const fn max_bins(&self) -> usize {
        self.max_bins
    }

    /// The number of buckets in use, the zero bucket aside.
    #[must_use]
    pub fn bin_count(&self) -> usize {
        self.bins.len()
    }

    /// Records `value`. Returns `false`, and records nothing, when it is
    /// negative, NaN or infinite. Negative zero is zero.
    pub fn insert(&mut self, value: f64) -> bool {
        if !value.is_finite() || value < 0.0 {
            return false;
        }
        let value = value.abs();
        if value < f64::MIN_POSITIVE {
            self.zeros = self.zeros.saturating_add(1);
        } else {
            let bin = self.bins.entry(self.index(value)).or_insert(0);
            *bin = bin.saturating_add(1);
            self.collapse();
        }
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count = self.count.saturating_add(1);
        self.sum += value;
        true
    }

    /// Records a duration in seconds.
    pub fn insert_duration(&mut self, duration: Duration) -> bool {
        self.insert(duration.as_secs_f64())
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// How many values were recorded.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The exact sum of the values recorded.
    #[must_use]
    pub const fn sum(&self) -> f64 {
        self.sum
    }

    /// The exact mean, or `None` when empty.
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        let count = self.count as f64;
        (self.count > 0).then(|| self.sum / count)
    }

    /// The exact smallest value, or `None` when empty.
    #[must_use]
    pub fn min(&self) -> Option<f64> {
        (self.count > 0).then_some(self.min)
    }

    /// The exact largest value, or `None` when empty.
    #[must_use]
    pub fn max(&self) -> Option<f64> {
        (self.count > 0).then_some(self.max)
    }

    /// The value at quantile `q` (0.5 for the median, 0.99 for the p99),
    /// within [`relative_accuracy`](Self::relative_accuracy) of the recorded
    /// value at rank `floor(q * (count - 1))`. `q` is clamped to `[0, 1]`.
    /// `None` when the sketch is empty or `q` is NaN.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 || q.is_nan() {
            return None;
        }
        let last = self.count.saturating_sub(1);
        #[allow(clippy::cast_precision_loss)]
        let rank = q.clamp(0.0, 1.0) * last as f64;
        // The rank is in `[0, last]`, so the cast is exact up to rounding.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rank = (rank.floor() as u64).min(last);
        if rank == 0 {
            return Some(self.min);
        }
        if rank == last {
            return Some(self.max);
        }
        // Rank `r` (0-based) is in the first bucket whose running total
        // passes `r`.
        let mut seen = self.zeros;
        if seen > rank {
            return Some(self.min);
        }
        for (&index, &count) in &self.bins {
            seen = seen.saturating_add(count);
            if seen > rank {
                return Some(self.midpoint(index).clamp(self.min, self.max));
            }
        }
        Some(self.max)
    }

    /// Folds `other` in, so this sketch describes both streams. The bucket
    /// cap of this sketch is kept.
    ///
    /// # Errors
    ///
    /// [`AccuracyMismatch`] when the relative accuracies differ. Nothing
    /// changes.
    pub fn merge(&mut self, other: &Self) -> Result<(), AccuracyMismatch> {
        if self.relative_accuracy.to_bits() != other.relative_accuracy.to_bits() {
            return Err(AccuracyMismatch {
                expected: self.relative_accuracy,
                found: other.relative_accuracy,
            });
        }
        if other.count == 0 {
            return Ok(());
        }
        for (&index, &count) in &other.bins {
            let bin = self.bins.entry(index).or_insert(0);
            *bin = bin.saturating_add(count);
        }
        self.collapse();
        if self.count == 0 {
            self.min = other.min;
            self.max = other.max;
        } else {
            self.min = self.min.min(other.min);
            self.max = self.max.max(other.max);
        }
        self.zeros = self.zeros.saturating_add(other.zeros);
        self.count = self.count.saturating_add(other.count);
        self.sum += other.sum;
        Ok(())
    }

    /// Forgets every value recorded. The accuracy and cap stay.
    pub fn clear(&mut self) {
        *self = Self::with_parts(self.relative_accuracy, self.max_bins);
    }

    /// The sketch as bytes: a version byte, the accuracy, the cap, the zero
    /// count, the total count, the sum, min and max, then each bucket as an
    /// index and a count. Integers and floats are little-endian.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.bins.len().saturating_mul(12).saturating_add(57));
        bytes.push(FORMAT_VERSION);
        bytes.extend_from_slice(&self.relative_accuracy.to_le_bytes());
        // The cap is at most `MAX_MAX_BINS`, which fits a u32.
        bytes.extend_from_slice(
            &u32::try_from(self.max_bins)
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&self.zeros.to_le_bytes());
        bytes.extend_from_slice(&self.count.to_le_bytes());
        bytes.extend_from_slice(&self.sum.to_le_bytes());
        bytes.extend_from_slice(&self.min.to_le_bytes());
        bytes.extend_from_slice(&self.max.to_le_bytes());
        bytes.extend_from_slice(
            &u32::try_from(self.bins.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        for (&index, &count) in &self.bins {
            bytes.extend_from_slice(&index.to_le_bytes());
            bytes.extend_from_slice(&count.to_le_bytes());
        }
        bytes
    }

    /// A sketch from [`to_bytes`](Self::to_bytes) output. `None` when the
    /// bytes are not a sketch: an unknown version, an accuracy or cap out of
    /// range, more buckets than the cap, buckets out of order or empty,
    /// counts that do not add up, extremes out of order, or bytes left over.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader(bytes);
        if reader.array::<1>()? != [FORMAT_VERSION] {
            return None;
        }
        let relative_accuracy = f64::from_le_bytes(reader.array()?);
        if !(Self::MIN_RELATIVE_ACCURACY..=Self::MAX_RELATIVE_ACCURACY).contains(&relative_accuracy)
        {
            return None;
        }
        let max_bins = usize::try_from(u32::from_le_bytes(reader.array()?)).ok()?;
        if !(Self::MIN_MAX_BINS..=Self::MAX_MAX_BINS).contains(&max_bins) {
            return None;
        }
        let mut sketch = Self::with_parts(relative_accuracy, max_bins);
        sketch.zeros = u64::from_le_bytes(reader.array()?);
        sketch.count = u64::from_le_bytes(reader.array()?);
        sketch.sum = f64::from_le_bytes(reader.array()?);
        sketch.min = f64::from_le_bytes(reader.array()?);
        sketch.max = f64::from_le_bytes(reader.array()?);
        let bins = usize::try_from(u32::from_le_bytes(reader.array()?)).ok()?;
        if bins > max_bins {
            return None;
        }
        let mut total = sketch.zeros;
        let mut previous = None;
        for _ in 0..bins {
            let index = i32::from_le_bytes(reader.array()?);
            let count = u64::from_le_bytes(reader.array()?);
            if count == 0 || previous.is_some_and(|previous| index <= previous) {
                return None;
            }
            previous = Some(index);
            total = total.checked_add(count)?;
            sketch.bins.insert(index, count);
        }
        let extremes_ok = if sketch.count == 0 {
            sketch.min.to_bits() == 0 && sketch.max.to_bits() == 0 && sketch.sum.to_bits() == 0
        } else {
            sketch.min >= 0.0
                && sketch.min <= sketch.max
                && sketch.max.is_finite()
                && !sketch.sum.is_nan()
        };
        (reader.0.is_empty() && total == sketch.count && extremes_ok).then_some(sketch)
    }

    /// The bucket holding a positive, normal, finite `value`.
    fn index(&self, value: f64) -> i32 {
        // `ln(value)` is in about `[-708, 710]` and `ln_gamma` is at least
        // `2e-4`, so the quotient fits an i32; the cast saturates anyway.
        #[allow(clippy::cast_possible_truncation)]
        let index = (value.ln() / self.ln_gamma).ceil() as i32;
        index
    }

    /// The midpoint of bucket `index`, within the relative accuracy of every
    /// value in it.
    fn midpoint(&self, index: i32) -> f64 {
        (self.ln_gamma * f64::from(index)).exp() * self.midpoint_factor
    }

    /// Folds the lowest buckets into their neighbour until the cap holds.
    fn collapse(&mut self) {
        while self.bins.len() > self.max_bins {
            let Some((_, lowest)) = self.bins.pop_first() else {
                return;
            };
            if let Some(mut next) = self.bins.first_entry() {
                *next.get_mut() = next.get().saturating_add(lowest);
            }
        }
    }
}

/// Reads fixed-size fields off the front of a byte slice.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
}

/// A [`QuantileSketch`] shared behind a mutex. See the [module docs](self).
///
/// A poisoned lock is recovered: the sketch's own updates never panic, so a
/// panic elsewhere under the lock leaves a valid sketch behind.
#[derive(Debug)]
pub struct QuantileMeter {
    sketch: Mutex<QuantileSketch>,
}

impl Default for QuantileMeter {
    fn default() -> Self {
        Self::new(QuantileSketch::DEFAULT_RELATIVE_ACCURACY)
    }
}

impl QuantileMeter {
    /// An empty shared sketch. See [`QuantileSketch::new`].
    #[must_use]
    pub fn new(relative_accuracy: f64) -> Self {
        Self::from_sketch(QuantileSketch::new(relative_accuracy))
    }

    /// Shares `sketch`, for example one built with
    /// [`QuantileSketch::with_max_bins`].
    #[must_use]
    pub const fn from_sketch(sketch: QuantileSketch) -> Self {
        Self {
            sketch: Mutex::new(sketch),
        }
    }

    /// See [`QuantileSketch::insert`].
    pub fn insert(&self, value: f64) -> bool {
        self.with(|sketch| sketch.insert(value))
    }

    /// Records a duration in seconds. See [`QuantileSketch::insert_duration`].
    pub fn record(&self, duration: Duration) -> bool {
        self.with(|sketch| sketch.insert_duration(duration))
    }

    /// See [`QuantileSketch::count`].
    #[must_use]
    pub fn count(&self) -> u64 {
        self.with(|sketch| sketch.count())
    }

    /// See [`QuantileSketch::quantile`].
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<f64> {
        self.with(|sketch| sketch.quantile(q))
    }

    /// See [`QuantileSketch::merge`].
    ///
    /// # Errors
    ///
    /// [`AccuracyMismatch`] when the relative accuracies differ.
    pub fn merge(&self, other: &QuantileSketch) -> Result<(), AccuracyMismatch> {
        self.with(|sketch| sketch.merge(other))
    }

    /// A copy of the sketch, to read several quantiles from one moment, to
    /// merge elsewhere, or to ship with [`QuantileSketch::to_bytes`].
    #[must_use]
    pub fn snapshot(&self) -> QuantileSketch {
        self.with(|sketch| sketch.clone())
    }

    /// Hands back the sketch and starts an empty one of the same accuracy
    /// and cap, in one step: no value falls between the two windows.
    #[must_use]
    pub fn take(&self) -> QuantileSketch {
        self.with(|sketch| {
            let fresh = QuantileSketch::with_parts(sketch.relative_accuracy, sketch.max_bins);
            std::mem::replace(sketch, fresh)
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut QuantileSketch) -> R) -> R {
        let mut sketch = self.sketch.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut sketch)
    }
}

#[cfg(test)]
// The sketch keeps min, max, sum and its accuracy exactly, so the tests
// compare them exactly.
#[allow(clippy::float_cmp, clippy::cast_precision_loss)]
mod tests {
    use super::*;

    /// The value a perfect quantile would return: the sorted sample at
    /// 0-based rank `floor(q * (n - 1))`.
    fn exact(sorted: &[f64], q: f64) -> f64 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rank = (q * (sorted.len() - 1) as f64).floor() as usize;
        sorted[rank]
    }

    fn assert_within(sketch: &QuantileSketch, sorted: &[f64], q: f64) {
        let truth = exact(sorted, q);
        let estimate = sketch.quantile(q).unwrap();
        let error = (estimate - truth).abs() / truth.abs().max(f64::MIN_POSITIVE);
        assert!(
            error <= sketch.relative_accuracy() + 1e-12,
            "q={q}: estimate {estimate}, truth {truth}, error {error}"
        );
    }

    const QS: [f64; 9] = [0.0, 0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99, 1.0];

    #[test]
    fn an_empty_sketch_has_no_quantiles() {
        let sketch = QuantileSketch::new(0.01);
        assert!(sketch.is_empty());
        assert_eq!(sketch.count(), 0);
        assert_eq!(sketch.quantile(0.5), None);
        assert_eq!(sketch.min(), None);
        assert_eq!(sketch.max(), None);
        assert_eq!(sketch.mean(), None);
    }

    #[test]
    fn a_single_value_is_every_quantile() {
        let mut sketch = QuantileSketch::new(0.01);
        assert!(sketch.insert(0.25));
        for q in QS {
            assert_eq!(sketch.quantile(q), Some(0.25));
        }
    }

    #[test]
    fn uniform_quantiles_stay_within_the_relative_accuracy() {
        let mut sketch = QuantileSketch::new(0.01);
        let values: Vec<f64> = (1..=10_000).map(f64::from).collect();
        for &v in values.iter().rev() {
            sketch.insert(v);
        }
        assert_eq!(sketch.count(), 10_000);
        for q in QS {
            assert_within(&sketch, &values, q);
        }
    }

    #[test]
    fn values_across_fifteen_orders_of_magnitude_stay_within_accuracy() {
        let mut sketch = QuantileSketch::new(0.02);
        let mut values = Vec::new();
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..50_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let unit = (state >> 11) as f64 / (1_u64 << 53) as f64;
            let value = 10_f64.powf(unit.mul_add(15.0, -6.0));
            values.push(value);
            sketch.insert(value);
        }
        values.sort_by(f64::total_cmp);
        for q in QS {
            assert_within(&sketch, &values, q);
        }
    }

    #[test]
    fn the_ends_are_the_exact_min_and_max() {
        let mut sketch = QuantileSketch::new(0.05);
        for v in [3.7, 0.0012, 812.5, 42.0] {
            sketch.insert(v);
        }
        assert_eq!(sketch.quantile(0.0), Some(0.0012));
        assert_eq!(sketch.quantile(1.0), Some(812.5));
        assert_eq!(sketch.min(), Some(0.0012));
        assert_eq!(sketch.max(), Some(812.5));
        assert_eq!(sketch.quantile(-3.0), Some(0.0012));
        assert_eq!(sketch.quantile(7.0), Some(812.5));
        assert_eq!(sketch.quantile(f64::NAN), None);
    }

    #[test]
    fn zeros_are_counted_in_their_own_bucket() {
        let mut sketch = QuantileSketch::new(0.01);
        for _ in 0..90 {
            sketch.insert(0.0);
        }
        for _ in 0..10 {
            sketch.insert(5.0);
        }
        assert_eq!(sketch.quantile(0.5), Some(0.0));
        assert_eq!(sketch.quantile(0.89), Some(0.0));
        let p95 = sketch.quantile(0.95).unwrap();
        assert!((p95 - 5.0).abs() / 5.0 <= 0.01, "{p95}");
        assert_eq!(sketch.bin_count(), 1);
    }

    #[test]
    fn negative_and_non_finite_values_are_refused() {
        let mut sketch = QuantileSketch::new(0.01);
        for v in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0001] {
            assert!(!sketch.insert(v), "{v} was accepted");
        }
        assert!(sketch.is_empty());
        assert!(sketch.insert(-0.0), "negative zero is zero");
        assert_eq!(sketch.count(), 1);
    }

    #[test]
    fn mean_and_sum_are_exact() {
        let mut sketch = QuantileSketch::new(0.01);
        for v in [1.0, 2.0, 3.0, 10.0] {
            sketch.insert(v);
        }
        assert_eq!(sketch.sum(), 16.0);
        assert_eq!(sketch.mean(), Some(4.0));
    }

    #[test]
    fn durations_are_recorded_in_seconds() {
        let mut sketch = QuantileSketch::new(0.01);
        sketch.insert_duration(Duration::from_millis(250));
        assert_eq!(sketch.quantile(0.5), Some(0.25));
    }

    #[test]
    fn merged_sketches_answer_like_the_sketch_of_the_union() {
        let mut left = QuantileSketch::new(0.01);
        let mut right = QuantileSketch::new(0.01);
        let mut both = QuantileSketch::new(0.01);
        for i in 1..=5_000 {
            let v = f64::from(i) * 0.37;
            if i % 3 == 0 {
                left.insert(v);
            } else {
                right.insert(v);
            }
            both.insert(v);
        }
        left.merge(&right).unwrap();
        assert_eq!(left.count(), both.count());
        for q in QS {
            assert_eq!(left.quantile(q), both.quantile(q), "q={q}");
        }
    }

    #[test]
    fn sketches_of_different_accuracy_do_not_merge() {
        let mut fine = QuantileSketch::new(0.01);
        fine.insert(1.0);
        let mut coarse = QuantileSketch::new(0.05);
        coarse.insert(2.0);
        let err = fine.merge(&coarse).unwrap_err();
        assert_eq!(err.expected, 0.01);
        assert_eq!(err.found, 0.05);
        assert_eq!(fine.count(), 1, "a refused merge changes nothing");
        assert!(err.to_string().contains("0.05"));
    }

    #[test]
    fn the_bin_cap_bounds_memory_and_keeps_the_tail_accurate() {
        let mut sketch = QuantileSketch::new(0.01).with_max_bins(64);
        let mut values = Vec::new();
        for i in 0..100_000 {
            let value = 1.0001_f64.powi(i);
            values.push(value);
            sketch.insert(value);
        }
        assert!(sketch.bin_count() <= 64, "{} bins", sketch.bin_count());
        for q in [0.99, 0.999, 1.0] {
            assert_within(&sketch, &values, q);
        }
        // The lowest ranks are folded upward: an estimate is never below
        // the truth.
        assert!(sketch.quantile(0.01).unwrap() >= exact(&values, 0.01) * (1.0 - 0.01));
    }

    #[test]
    fn accuracy_and_bin_cap_are_clamped() {
        assert_eq!(
            QuantileSketch::new(0.0).relative_accuracy(),
            QuantileSketch::MIN_RELATIVE_ACCURACY
        );
        assert_eq!(
            QuantileSketch::new(0.9).relative_accuracy(),
            QuantileSketch::MAX_RELATIVE_ACCURACY
        );
        assert_eq!(
            QuantileSketch::new(f64::NAN).relative_accuracy(),
            QuantileSketch::DEFAULT_RELATIVE_ACCURACY
        );
        assert_eq!(
            QuantileSketch::new(0.01).with_max_bins(0).max_bins(),
            QuantileSketch::MIN_MAX_BINS
        );
        assert_eq!(QuantileSketch::default().relative_accuracy(), 0.01);
    }

    #[test]
    fn the_tiniest_and_largest_finite_values_are_indexed() {
        let mut sketch = QuantileSketch::new(0.0001);
        for v in [f64::MIN_POSITIVE, 5e-324, f64::MAX] {
            assert!(sketch.insert(v));
        }
        assert_eq!(sketch.quantile(1.0), Some(f64::MAX));
        assert_eq!(sketch.quantile(0.0), Some(5e-324));
        assert!(sketch.quantile(0.5).unwrap() > 0.0);
    }

    #[test]
    fn bytes_round_trip() {
        let mut sketch = QuantileSketch::new(0.02).with_max_bins(100);
        for i in 0..1_000 {
            sketch.insert(f64::from(i) * 1.5);
        }
        let back = QuantileSketch::from_bytes(&sketch.to_bytes()).unwrap();
        assert_eq!(back, sketch);
        let empty = QuantileSketch::new(0.01);
        assert_eq!(QuantileSketch::from_bytes(&empty.to_bytes()), Some(empty));
    }

    #[test]
    fn bytes_that_are_not_a_sketch_are_refused() {
        let mut sketch = QuantileSketch::new(0.01);
        for v in [0.0, 1.0, 2.0, 400.0] {
            sketch.insert(v);
        }
        let bytes = sketch.to_bytes();
        assert_eq!(QuantileSketch::from_bytes(&[]), None);
        for cut in 0..bytes.len() {
            assert_eq!(QuantileSketch::from_bytes(&bytes[..cut]), None, "cut {cut}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(QuantileSketch::from_bytes(&trailing), None);
        let mut version = bytes.clone();
        version[0] = 99;
        assert_eq!(QuantileSketch::from_bytes(&version), None);
        // Flipping any single byte either is refused or still decodes to a
        // self-consistent sketch (never a panic).
        for i in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[i] ^= 0xff;
            if let Some(decoded) = QuantileSketch::from_bytes(&flipped) {
                let _ = decoded.quantile(0.5);
            }
        }
    }

    #[test]
    fn a_count_that_disagrees_with_the_bins_is_refused() {
        let mut sketch = QuantileSketch::new(0.01);
        sketch.insert(1.0);
        let mut bytes = sketch.to_bytes();
        // The total count sits after the version, accuracy, cap and zero count.
        let at = 1 + 8 + 4 + 8;
        bytes[at] = bytes[at].wrapping_add(1);
        assert_eq!(QuantileSketch::from_bytes(&bytes), None);
    }

    #[test]
    fn the_meter_is_shared_across_threads() {
        let meter = std::sync::Arc::new(QuantileMeter::new(0.01));
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let meter = std::sync::Arc::clone(&meter);
                std::thread::spawn(move || {
                    for i in 1..=1_000 {
                        meter.insert(f64::from(t * 1_000 + i));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(meter.count(), 4_000);
        let p50 = meter.quantile(0.5).unwrap();
        assert!((p50 - 2_000.0).abs() / 2_000.0 <= 0.01, "{p50}");
    }

    #[test]
    fn take_hands_back_the_window_and_starts_a_fresh_one() {
        let meter = QuantileMeter::new(0.01);
        meter.record(Duration::from_millis(5));
        meter.insert(0.010);
        let window = meter.take();
        assert_eq!(window.count(), 2);
        assert_eq!(meter.count(), 0);
        assert_eq!(meter.snapshot().relative_accuracy(), 0.01);

        let mut other = QuantileSketch::new(0.01);
        other.insert(1.0);
        meter.merge(&other).unwrap();
        assert_eq!(meter.quantile(1.0), Some(1.0));
        assert!(meter.merge(&QuantileSketch::new(0.1)).is_err());
    }
}
