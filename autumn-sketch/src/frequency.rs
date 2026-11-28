//! Bounded-memory frequencies: "how many times have we seen *this* key?" for
//! any key of a stream too long to count exactly.
//!
//! "How many failed logins has this IP had this hour?", "how often has this
//! search query been typed today?", "how many times did this webhook endpoint
//! time out?" An exact answer keeps one counter per key ever seen.
//! [`heavy_hitters`](crate::heavy_hitters) keeps counters for the top keys
//! only, so it cannot answer for a key outside the top. [`CountMinSketch`]
//! answers for *every* key, in a fixed grid of counters chosen up front from
//! the error you can accept: ±0.1 % of the stream, 99 % of the time, is
//! about 106 KiB, whatever the keys are.
//!
//! It is the Count-Min sketch (Graham Cormode and S. Muthukrishnan, 2005).
//! The grid has `depth` rows of `width` counters. A key adds its weight to
//! one counter in each row, picked by that row's hash. Its estimate is the
//! smallest of its `depth` counters: every counter holds the key's own count
//! plus whatever other keys collided into it, so the smallest is the least
//! polluted.
//!
//! - **Never an undercount.** [`estimate`](CountMinSketch::estimate) is at
//!   least the key's true total, always. A key never added reads 0 or a
//!   small overcount, never a negative.
//! - **Bounded overcount.** With probability `1 - delta`, the estimate is
//!   above the truth by at most `epsilon × total`.
//!   [`error_bound`](CountMinSketch::error_bound) reads that bound for the
//!   sketch as it is now, so "over 50 failures, give or take 3" is a number
//!   you can print.
//! - Two sketches of the same shape [`merge`](CountMinSketch::merge) into
//!   the sketch of both streams, exactly: each replica counts what it saw,
//!   and the merged sketch answers for the fleet.
//!   [`to_bytes`](CountMinSketch::to_bytes) and
//!   [`from_bytes`](CountMinSketch::from_bytes) carry a sketch between
//!   processes. [`CountMinSketch::new`] is deterministic, so every replica
//!   that passes the same accuracy gets the same shape.
//!
//! Keys are hashed with the same fixed, unkeyed hash as
//! [`distinct`](crate::distinct), so a key lands in the same counters in
//! every process. A client who picks keys can aim them at a victim key's
//! counters and inflate its estimate: count keys the server derives, or hash
//! untrusted input with your own keyed hash and pass it to
//! [`add_hash`](CountMinSketch::add_hash).
//!
//! [`FrequencyCounter`] is the shared form: a [`CountMinSketch`] behind a
//! mutex, to put in an `Arc` and count from every handler.
//! [`take`](FrequencyCounter::take) swaps in an empty sketch, so a fixed
//! window ("this hour") is one call per tick.
//!
//! ```rust
//! use autumn_sketch::frequency::FrequencyCounter;
//!
//! let failures = FrequencyCounter::new(0.001, 0.01);
//! for ip in ["10.0.0.7", "10.0.0.7", "10.0.0.9", "10.0.0.7"] {
//!     failures.increment(ip);
//! }
//! assert_eq!(failures.estimate("10.0.0.7"), 3);
//! assert_eq!(failures.estimate("10.0.0.9"), 1);
//! assert_eq!(failures.estimate("10.0.0.1"), 0); // never seen
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

use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, PoisonError};

use crate::bloom::mix;
use crate::distinct::StableHasher;

/// Two sketches of different shapes cannot be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeMismatch {
    /// `(depth, width)` of the sketch called on.
    pub expected: (usize, usize),
    /// `(depth, width)` of the sketch offered.
    pub found: (usize, usize),
}

impl fmt::Display for ShapeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge a {}x{} Count-Min sketch into a {}x{} one",
            self.found.0, self.found.1, self.expected.0, self.expected.1
        )
    }
}

impl std::error::Error for ShapeMismatch {}

/// A fixed-size frequency sketch. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountMinSketch {
    depth: usize,
    width: usize,
    total: u64,
    /// `depth` rows of `width` counters, row after row.
    cells: Vec<u64>,
}

impl CountMinSketch {
    /// The most counters in one row: 2^20, for an `epsilon` near 2.6e-6.
    pub const MAX_WIDTH: usize = 1 << 20;
    /// The most rows: 16, for a `delta` near 1e-7.
    pub const MAX_DEPTH: usize = 16;

    /// An empty sketch whose estimates are within `epsilon × total` of the
    /// truth with probability `1 - delta`. `epsilon` is clamped to
    /// `1e-6..=0.5` and `delta` to `1e-7..=0.5`; a NaN takes the default
    /// (0.001 and 0.01). The grid is `ceil(e / epsilon)` wide and
    /// `ceil(ln(1 / delta))` deep, within [`MAX_WIDTH`](Self::MAX_WIDTH) and
    /// [`MAX_DEPTH`](Self::MAX_DEPTH). The same arguments always give the
    /// same shape.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn new(epsilon: f64, delta: f64) -> Self {
        let epsilon = if epsilon.is_nan() {
            0.001
        } else {
            epsilon.clamp(1e-6, 0.5)
        };
        let delta = if delta.is_nan() {
            0.01
        } else {
            delta.clamp(1e-7, 0.5)
        };
        // Finite and clamped, so the casts are exact.
        let width = (std::f64::consts::E / epsilon)
            .ceil()
            .clamp(1.0, 2_f64.powi(20)) as usize;
        let depth = (1.0 / delta).ln().ceil().clamp(1.0, 16.0) as usize;
        Self::with_shape(depth, width)
    }

    fn with_shape(depth: usize, width: usize) -> Self {
        Self {
            depth,
            width,
            total: 0,
            cells: vec![0; depth.saturating_mul(width)],
        }
    }

    /// The number of rows: one counter per row per key.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// The number of counters in each row.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// The total weight added, over every key.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// `true` when nothing was added.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Forgets every count.
    pub fn clear(&mut self) {
        self.cells.fill(0);
        self.total = 0;
    }

    /// Counts one more `key`, and returns its new estimate.
    pub fn increment<T: Hash + ?Sized>(&mut self, key: &T) -> u64 {
        self.add(key, 1)
    }

    /// Adds `weight` to `key` (bytes sent, rows written, ...), and returns
    /// its new estimate. Counters saturate at `u64::MAX`.
    pub fn add<T: Hash + ?Sized>(&mut self, key: &T, weight: u64) -> u64 {
        self.add_hash(stable_hash(key), weight)
    }

    /// Adds `weight` to a key by a 64-bit hash you computed, for example
    /// with a keyed hash over untrusted input. Returns as
    /// [`add`](Self::add) does.
    pub fn add_hash(&mut self, hash: u64, weight: u64) -> u64 {
        self.total = self.total.saturating_add(weight);
        let mut estimate = u64::MAX;
        for index in self.positions(hash) {
            if let Some(cell) = self.cells.get_mut(index) {
                *cell = cell.saturating_add(weight);
                estimate = estimate.min(*cell);
            }
        }
        estimate
    }

    /// The estimated total weight of `key`: never below the truth, and above
    /// it by at most [`error_bound`](Self::error_bound) with probability
    /// `1 - delta`.
    #[must_use]
    pub fn estimate<T: Hash + ?Sized>(&self, key: &T) -> u64 {
        self.estimate_hash(stable_hash(key))
    }

    /// [`estimate`](Self::estimate) for a hash passed to
    /// [`add_hash`](Self::add_hash).
    #[must_use]
    pub fn estimate_hash(&self, hash: u64) -> u64 {
        self.positions(hash)
            .filter_map(|index| self.cells.get(index).copied())
            .min()
            .unwrap_or(0)
    }

    /// The most an estimate overcounts, with probability `1 - delta`:
    /// `ceil(e × total / width)`, or `u64::MAX` once the total has
    /// saturated. It grows with the stream, so rotate the
    /// sketch (see [`FrequencyCounter::take`]) when it passes what you can
    /// accept.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn error_bound(&self) -> u64 {
        // A saturated total no longer bounds the counters: claim nothing.
        if self.total == u64::MAX {
            return u64::MAX;
        }
        // Saturating float-to-int cast: never wraps.
        (std::f64::consts::E * self.total as f64 / self.width as f64).ceil() as u64
    }

    /// Folds `other` in, so this sketch is the sketch of both streams,
    /// exactly as if every key had been added here.
    ///
    /// # Errors
    ///
    /// [`ShapeMismatch`] when the shapes differ. Nothing changes.
    pub fn merge(&mut self, other: &Self) -> Result<(), ShapeMismatch> {
        if (self.depth, self.width) != (other.depth, other.width) {
            return Err(ShapeMismatch {
                expected: (self.depth, self.width),
                found: (other.depth, other.width),
            });
        }
        for (mine, &theirs) in self.cells.iter_mut().zip(&other.cells) {
            *mine = mine.saturating_add(theirs);
        }
        self.total = self.total.saturating_add(other.total);
        Ok(())
    }

    /// The sketch as bytes: depth and width as 32-bit little-endian words,
    /// the total, then every counter as a 64-bit little-endian word.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let shape = [self.depth, self.width]
            .into_iter()
            .flat_map(|n| u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
        shape
            .chain(self.total.to_le_bytes())
            .chain(self.cells.iter().flat_map(|cell| cell.to_le_bytes()))
            .collect()
    }

    /// A sketch from [`to_bytes`](Self::to_bytes) output. `None` when the
    /// bytes are not a sketch: a shape of zero or past the limits, or a
    /// length that does not match the shape.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (depth, rest) = bytes.split_first_chunk::<4>()?;
        let (width, rest) = rest.split_first_chunk::<4>()?;
        let (total, rest) = rest.split_first_chunk::<8>()?;
        let depth = usize::try_from(u32::from_le_bytes(*depth)).ok()?;
        let width = usize::try_from(u32::from_le_bytes(*width)).ok()?;
        // Checked before decoding, so an oversized payload allocates nothing.
        if !(1..=Self::MAX_DEPTH).contains(&depth) || !(1..=Self::MAX_WIDTH).contains(&width) {
            return None;
        }
        let (chunks, remainder) = rest.as_chunks::<8>();
        if !remainder.is_empty() || chunks.len() != depth.checked_mul(width)? {
            return None;
        }
        Some(Self {
            depth,
            width,
            total: u64::from_le_bytes(*total),
            cells: chunks
                .iter()
                .map(|&chunk| u64::from_le_bytes(chunk))
                .collect(),
        })
    }

    /// The flat index of `hash`'s counter in each row.
    fn positions(&self, hash: u64) -> impl Iterator<Item = usize> + use<> {
        let width = self.width;
        let mut seed = 0_u64;
        (0..self.depth).map(move |row| {
            // Row `i` hashes with the `i`-th step of a SplitMix64 stream,
            // then maps into the row by multiply-shift (no modulo bias).
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let wide = u128::from(mix(hash ^ seed)).wrapping_mul(width as u128);
            let column = usize::try_from(wide.wrapping_shr(64)).unwrap_or(0);
            row.wrapping_mul(width).wrapping_add(column)
        })
    }
}

fn stable_hash<T: Hash + ?Sized>(key: &T) -> u64 {
    let mut hasher = StableHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

/// A [`CountMinSketch`] shared behind a mutex. See the [module docs](self).
///
/// A poisoned lock is recovered: counters only ever grow, so a sketch left by
/// a panicking writer is still a valid overcount.
#[derive(Debug)]
pub struct FrequencyCounter {
    sketch: Mutex<CountMinSketch>,
}

impl FrequencyCounter {
    /// An empty shared sketch. See [`CountMinSketch::new`].
    #[must_use]
    pub fn new(epsilon: f64, delta: f64) -> Self {
        Self {
            sketch: Mutex::new(CountMinSketch::new(epsilon, delta)),
        }
    }

    /// See [`CountMinSketch::increment`].
    pub fn increment<T: Hash + ?Sized>(&self, key: &T) -> u64 {
        self.with(|sketch| sketch.increment(key))
    }

    /// See [`CountMinSketch::add`].
    pub fn add<T: Hash + ?Sized>(&self, key: &T, weight: u64) -> u64 {
        self.with(|sketch| sketch.add(key, weight))
    }

    /// See [`CountMinSketch::add_hash`].
    pub fn add_hash(&self, hash: u64, weight: u64) -> u64 {
        self.with(|sketch| sketch.add_hash(hash, weight))
    }

    /// See [`CountMinSketch::estimate`].
    #[must_use]
    pub fn estimate<T: Hash + ?Sized>(&self, key: &T) -> u64 {
        self.with(|sketch| sketch.estimate(key))
    }

    /// See [`CountMinSketch::estimate_hash`].
    #[must_use]
    pub fn estimate_hash(&self, hash: u64) -> u64 {
        self.with(|sketch| sketch.estimate_hash(hash))
    }

    /// See [`CountMinSketch::merge`].
    ///
    /// # Errors
    ///
    /// [`ShapeMismatch`] when the shapes differ.
    pub fn merge(&self, other: &CountMinSketch) -> Result<(), ShapeMismatch> {
        self.with(|sketch| sketch.merge(other))
    }

    /// A copy of the sketch, to merge elsewhere or ship with
    /// [`CountMinSketch::to_bytes`].
    #[must_use]
    pub fn snapshot(&self) -> CountMinSketch {
        self.with(|sketch| sketch.clone())
    }

    /// Hands back the sketch and starts an empty one of the same shape, in
    /// one step: no count falls between the two windows.
    #[must_use]
    pub fn take(&self) -> CountMinSketch {
        self.with(|sketch| {
            let fresh = CountMinSketch::with_shape(sketch.depth, sketch.width);
            std::mem::replace(sketch, fresh)
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut CountMinSketch) -> R) -> R {
        let mut sketch = self.sketch.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut sketch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A skewed stream: key `k` appears `k + 1` times, for `k` in `0..keys`.
    fn skewed(sketch: &mut CountMinSketch, keys: u64) {
        for key in 0..keys {
            sketch.add(&key, key + 1);
        }
    }

    #[test]
    fn shape_follows_accuracy() {
        let sketch = CountMinSketch::new(0.001, 0.01);
        assert_eq!(sketch.width(), 2_719); // ceil(e / 0.001)
        assert_eq!(sketch.depth(), 5); // ceil(ln 100)
        assert_eq!(
            CountMinSketch::new(0.001, 0.01),
            CountMinSketch::new(0.001, 0.01)
        );
    }

    #[test]
    fn shape_is_clamped() {
        let tiny = CountMinSketch::new(0.0, 0.0);
        assert_eq!(tiny.width(), CountMinSketch::MAX_WIDTH);
        assert_eq!(tiny.depth(), CountMinSketch::MAX_DEPTH);
        let loose = CountMinSketch::new(10.0, 10.0);
        assert_eq!(loose.width(), 6); // ceil(e / 0.5)
        assert_eq!(loose.depth(), 1);
        let nan = CountMinSketch::new(f64::NAN, f64::NAN);
        assert_eq!(nan, CountMinSketch::new(0.001, 0.01));
    }

    #[test]
    fn empty_sketch_counts_nothing() {
        let mut sketch = CountMinSketch::new(0.01, 0.01);
        assert!(sketch.is_empty());
        assert_eq!(sketch.estimate("anything"), 0);
        assert_eq!(sketch.error_bound(), 0);
        sketch.increment("a");
        assert!(!sketch.is_empty());
        sketch.clear();
        assert!(sketch.is_empty());
        assert_eq!(sketch.estimate("a"), 0);
    }

    #[test]
    fn sparse_counts_are_exact() {
        let mut sketch = CountMinSketch::new(0.001, 0.01);
        assert_eq!(sketch.increment("a"), 1);
        assert_eq!(sketch.increment("a"), 2);
        assert_eq!(sketch.add("b", 40), 40);
        assert_eq!(sketch.estimate("a"), 2);
        assert_eq!(sketch.estimate("b"), 40);
        assert_eq!(sketch.total(), 42);
    }

    #[test]
    fn estimates_never_undercount_and_stay_within_bound() {
        let mut sketch = CountMinSketch::new(0.01, 0.001);
        skewed(&mut sketch, 2_000);
        let bound = sketch.error_bound();
        assert_eq!(sketch.total(), 2_000 * 2_001 / 2);
        let mut over_bound = 0;
        for key in 0..2_000_u64 {
            let estimate = sketch.estimate(&key);
            assert!(estimate > key, "key {key} undercounted: {estimate}");
            if estimate - (key + 1) > bound {
                over_bound += 1;
            }
        }
        // delta = 0.001: at most a handful of the 2000 keys may exceed it.
        assert!(over_bound <= 10, "{over_bound} keys past the bound");
    }

    #[test]
    fn unseen_keys_read_small() {
        let mut sketch = CountMinSketch::new(0.001, 0.01);
        skewed(&mut sketch, 500);
        let bound = sketch.error_bound();
        let past = (10_000..11_000_u64)
            .filter(|key| sketch.estimate(key) > bound)
            .count();
        assert!(past <= 20, "{past} unseen keys past the bound");
    }

    #[test]
    fn add_hash_matches_add() {
        let mut a = CountMinSketch::new(0.01, 0.01);
        let mut b = CountMinSketch::new(0.01, 0.01);
        a.add("key", 7);
        b.add_hash(stable_hash("key"), 7);
        assert_eq!(a, b);
        assert_eq!(b.estimate_hash(stable_hash("key")), 7);
    }

    #[test]
    fn counters_saturate() {
        let mut sketch = CountMinSketch::new(0.5, 0.5);
        sketch.add("a", u64::MAX);
        assert_eq!(sketch.add("a", 5), u64::MAX);
        assert_eq!(sketch.total(), u64::MAX);
        assert_eq!(sketch.error_bound(), u64::MAX);
    }

    #[test]
    fn merge_is_the_sketch_of_both_streams() {
        let mut left = CountMinSketch::new(0.01, 0.01);
        let mut right = CountMinSketch::new(0.01, 0.01);
        let mut whole = CountMinSketch::new(0.01, 0.01);
        for key in 0..300_u64 {
            let (side, other) = if key % 3 == 0 {
                (&mut left, 0)
            } else {
                (&mut right, 1)
            };
            side.add(&key, key + other);
            whole.add(&key, key + other);
        }
        left.merge(&right).unwrap();
        assert_eq!(left, whole);
    }

    #[test]
    fn mismatched_shapes_are_refused() {
        let mut a = CountMinSketch::new(0.01, 0.01);
        a.increment("x");
        let before = a.clone();
        let b = CountMinSketch::new(0.001, 0.01);
        let err = a.merge(&b).unwrap_err();
        assert_eq!(
            err,
            ShapeMismatch {
                expected: (5, 272),
                found: (5, 2_719)
            }
        );
        assert_eq!(a, before);
        assert!(err.to_string().contains("5x2719"));
    }

    #[test]
    fn bytes_round_trip() {
        let mut sketch = CountMinSketch::new(0.05, 0.05);
        skewed(&mut sketch, 100);
        let bytes = sketch.to_bytes();
        assert_eq!(bytes.len(), 16 + sketch.depth() * sketch.width() * 8);
        assert_eq!(CountMinSketch::from_bytes(&bytes), Some(sketch));
    }

    #[test]
    fn bad_bytes_are_refused() {
        let good = CountMinSketch::new(0.5, 0.5).to_bytes();
        assert!(CountMinSketch::from_bytes(&good).is_some());
        assert_eq!(CountMinSketch::from_bytes(&[]), None);
        assert_eq!(CountMinSketch::from_bytes(&good[..15]), None);
        assert_eq!(CountMinSketch::from_bytes(&good[..good.len() - 1]), None);
        let mut long = good.clone();
        long.extend_from_slice(&[0; 8]);
        assert_eq!(CountMinSketch::from_bytes(&long), None);
        let mut zero_depth = good.clone();
        zero_depth[..4].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(CountMinSketch::from_bytes(&zero_depth), None);
        let mut huge = good;
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(CountMinSketch::from_bytes(&huge), None);
    }

    #[test]
    fn shared_counter_counts_across_threads() {
        let counter = Arc::new(FrequencyCounter::new(0.001, 0.01));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    for _ in 0..250 {
                        counter.increment("hot");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(counter.estimate("hot"), 1_000);
        assert_eq!(counter.add("hot", 5), 1_005);
        assert_eq!(counter.estimate_hash(stable_hash("hot")), 1_005);
    }

    #[test]
    fn take_starts_a_fresh_window() {
        let counter = FrequencyCounter::new(0.01, 0.01);
        counter.add_hash(42, 3);
        let window = counter.take();
        assert_eq!(window.estimate_hash(42), 3);
        assert_eq!(counter.estimate_hash(42), 0);
        assert_eq!(counter.snapshot().width(), window.width());
        counter.merge(&window).unwrap();
        assert_eq!(counter.snapshot(), window);
    }
}
