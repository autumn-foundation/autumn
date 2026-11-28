//! Bounded-memory distinct counts: how many *different* keys a stream held.
//!
//! "How many unique visitors today?", "how many distinct client IPs hit the
//! login form this hour?", "how many tenants used the export endpoint?" An
//! exact answer keeps every key it has seen, and that set grows with the
//! traffic. [`HyperLogLog`] answers in a fixed `2^precision` bytes, whatever
//! the traffic: 16 KiB counts a billion distinct keys to within about 1 %.
//!
//! It is the `HyperLogLog` algorithm (Flajolet, Fusy, Gandouet and Meunier,
//! 2007), read with Ertl's improved estimator (2017), which stays unbiased
//! from a handful of keys to billions with no switch between ranges. Each key
//! is hashed to 64 bits.
//! The top `precision` bits pick a register, and the register keeps the
//! longest run of leading zeros seen in the rest. Long runs are rare, so the
//! registers together say how many different hashes went by.
//!
//! - The relative standard error is `1.04 / sqrt(2^precision)`
//!   ([`HyperLogLog::standard_error`]): 1.6 % at precision 12 (4 KiB),
//!   0.81 % at 14 (16 KiB).
//! - Inserting a key twice changes nothing, so retries and replays do not
//!   inflate the count.
//! - Two sketches of the same precision [`merge`](HyperLogLog::merge) into
//!   the sketch of the union. Each replica counts its own visitors, and the
//!   merged sketch counts the fleet's, without double-counting a visitor who
//!   hit two replicas. [`to_bytes`](HyperLogLog::to_bytes) and
//!   [`from_bytes`](HyperLogLog::from_bytes) carry a sketch between processes.
//!
//! Keys are hashed with a fixed, unkeyed hash, so the same key lands in the
//! same register on every run. That is what makes sketches from different
//! processes mergeable, and it makes a stream replay identically. Strings,
//! byte slices, integers (written little-endian, `usize` and `isize` as 64
//! bits) and tuples or structs of those hash the same on every machine too.
//! A slice or array of multi-byte integers does not: the standard library
//! hashes it as native-endian bytes. Encode such a key yourself (for example
//! as a little-endian byte string) before merging sketches across
//! architectures. It also
//! means a client who picks keys can aim them at registers: count keys the
//! server derives (a session id, a tenant), or hash untrusted input with your
//! own keyed hash and pass it to [`insert_hash`](HyperLogLog::insert_hash).
//!
//! [`DistinctCounter`] is the shared form: a [`HyperLogLog`] behind a mutex,
//! to put in an `Arc` and record from every handler.
//!
//! ```rust
//! use autumn_sketch::distinct::DistinctCounter;
//!
//! let visitors = DistinctCounter::new(12);
//! for visitor in ["ada", "grace", "ada", "linus", "grace"] {
//!     visitors.insert(visitor);
//! }
//! assert_eq!(visitors.estimate(), 3);
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

/// Two sketches of different precisions cannot be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrecisionMismatch {
    /// The precision of the sketch merged into.
    pub expected: u8,
    /// The precision of the sketch offered.
    pub found: u8,
}

impl fmt::Display for PrecisionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge a precision-{} sketch into a precision-{} sketch",
            self.found, self.expected
        )
    }
}

impl std::error::Error for PrecisionMismatch {}

/// A fixed-size distinct-count sketch. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperLogLog {
    precision: u8,
    registers: Vec<u8>,
}

impl HyperLogLog {
    /// The smallest precision: 16 registers, about 26 % error.
    pub const MIN_PRECISION: u8 = 4;
    /// The largest precision: 262,144 registers (256 KiB), about 0.2 % error.
    pub const MAX_PRECISION: u8 = 18;

    /// An empty sketch of `2^precision` registers. `precision` is clamped to
    /// [`MIN_PRECISION`](Self::MIN_PRECISION)..=[`MAX_PRECISION`](Self::MAX_PRECISION).
    #[must_use]
    pub fn new(precision: u8) -> Self {
        let precision = precision.clamp(Self::MIN_PRECISION, Self::MAX_PRECISION);
        Self {
            precision,
            registers: vec![0; 1_usize << precision],
        }
    }

    /// The precision: the sketch holds `2^precision` one-byte registers.
    #[must_use]
    pub const fn precision(&self) -> u8 {
        self.precision
    }

    /// The relative standard error of [`estimate`](Self::estimate):
    /// `1.04 / sqrt(2^precision)`. About 68 % of estimates fall within one
    /// standard error of the truth, and nearly all within three.
    #[must_use]
    pub fn standard_error(&self) -> f64 {
        1.04 / self.register_count().sqrt()
    }

    /// Counts `key`. A key already counted changes nothing.
    pub fn insert<T: Hash + ?Sized>(&mut self, key: &T) {
        let mut hasher = StableHasher::default();
        key.hash(&mut hasher);
        self.insert_hash(hasher.finish());
    }

    /// Counts a key by its 64-bit hash, for a caller who hashes keys
    /// itself (for example with a keyed hash over untrusted input). The hash
    /// should be uniform over all 64 bits.
    pub fn insert_hash(&mut self, hash: u64) {
        let p = u32::from(self.precision);
        // The top `p` bits pick the register. `p` is at most 18, so the index
        // fits a usize on every target.
        let index = usize::try_from(hash.wrapping_shr(64_u32.wrapping_sub(p))).unwrap_or(0);
        // The rank is the position of the first one bit in the remaining
        // `64 - p` bits. A sentinel bit caps it at `64 - p + 1` when they are
        // all zero.
        let tail = hash.wrapping_shl(p) | 1_u64.wrapping_shl(p.wrapping_sub(1));
        let rank = u8::try_from(tail.leading_zeros().wrapping_add(1)).unwrap_or(u8::MAX);
        if let Some(register) = self.registers.get_mut(index) {
            *register = (*register).max(rank);
        }
    }

    /// Whether nothing has been counted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registers.iter().all(|&register| register == 0)
    }

    /// The estimated number of distinct keys counted.
    #[must_use]
    pub fn estimate(&self) -> u64 {
        // Ertl's improved estimator ("New cardinality estimation algorithms
        // for HyperLogLog sketches", 2017). It reads the histogram of
        // register values and needs no switch between small and large ranges,
        // so it has no bias spike where classic HyperLogLog hands over from
        // linear counting to the raw estimate.
        let m = self.register_count();
        let q = 64_u8.wrapping_sub(self.precision);
        let mut histogram = [0_u32; 66];
        for &register in &self.registers {
            if let Some(bucket) = histogram.get_mut(usize::from(register)) {
                *bucket = bucket.saturating_add(1);
            }
        }
        let count = |rank: u8| f64::from(histogram.get(usize::from(rank)).copied().unwrap_or(0));
        let empty = count(0);
        if empty >= m {
            return 0;
        }
        let mut z = m * tau(1.0 - count(q.wrapping_add(1)) / m);
        for rank in (1..=q).rev() {
            z = f64::midpoint(z, count(rank));
        }
        z = m.mul_add(sigma(empty / m), z);
        let estimate = m * m / (2.0 * std::f64::consts::LN_2 * z);
        // The estimate is finite, non-negative and far below `u64::MAX`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rounded = estimate.round() as u64;
        rounded
    }

    /// Folds `other` in, so this sketch counts the union of both streams.
    ///
    /// # Errors
    ///
    /// [`PrecisionMismatch`] when the precisions differ. Nothing changes.
    pub fn merge(&mut self, other: &Self) -> Result<(), PrecisionMismatch> {
        if self.precision != other.precision {
            return Err(PrecisionMismatch {
                expected: self.precision,
                found: other.precision,
            });
        }
        for (mine, theirs) in self.registers.iter_mut().zip(&other.registers) {
            *mine = (*mine).max(*theirs);
        }
        Ok(())
    }

    /// Forgets every key counted.
    pub fn clear(&mut self) {
        self.registers.fill(0);
    }

    /// The sketch as bytes: the precision, then one byte per register.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.registers.len().saturating_add(1));
        bytes.push(self.precision);
        bytes.extend_from_slice(&self.registers);
        bytes
    }

    /// A sketch from [`to_bytes`](Self::to_bytes) output. `None` when the
    /// bytes are not a sketch: a precision out of range, a length that does
    /// not match it, or a register no 64-bit hash could produce.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (&precision, registers) = bytes.split_first()?;
        if !(Self::MIN_PRECISION..=Self::MAX_PRECISION).contains(&precision)
            || registers.len() != 1_usize << precision
        {
            return None;
        }
        let max_rank = 65_u8.wrapping_sub(precision);
        if registers.iter().any(|&register| register > max_rank) {
            return None;
        }
        Some(Self {
            precision,
            registers: registers.to_vec(),
        })
    }

    fn register_count(&self) -> f64 {
        f64::from(1_u32 << self.precision)
    }
}

/// `x + sum(2^(k-1) * x^(2^k))` over `k >= 1`, the correction for empty
/// registers. `x` is below 1 here, so the terms shrink to nothing. The loop
/// stops once a term no longer changes `z` (an exact float comparison).
#[allow(clippy::float_cmp)]
fn sigma(mut x: f64) -> f64 {
    let mut y = 1.0;
    let mut z = x;
    for _ in 0..128 {
        x *= x;
        let previous = z;
        z = x.mul_add(y, z);
        y += y;
        if z == previous {
            break;
        }
    }
    z
}

/// `(1 - x - sum((1 - x^(2^-k))^2 * 2^-k)) / 3` over `k >= 1`, the
/// correction for saturated registers. Stops like [`sigma`].
#[allow(clippy::float_cmp)]
fn tau(mut x: f64) -> f64 {
    if x <= 0.0 || x >= 1.0 {
        return 0.0;
    }
    let mut y = 1.0;
    let mut z = 1.0 - x;
    for _ in 0..128 {
        x = x.sqrt();
        let previous = z;
        y *= 0.5;
        z = ((1.0 - x) * (1.0 - x)).mul_add(-y, z);
        if z == previous {
            break;
        }
    }
    z / 3.0
}

/// FNV-1a over the bytes, then the `MurmurHash3` 64-bit finalizer so every
/// output bit depends on every input bit. Unkeyed and fixed, so a key hashes
/// the same in every process. Integers are written little-endian, so it is
/// the same on every machine too.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StableHasher(u64);

impl Default for StableHasher {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for StableHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn write_u16(&mut self, i: u16) {
        self.write(&i.to_le_bytes());
    }

    fn write_u32(&mut self, i: u32) {
        self.write(&i.to_le_bytes());
    }

    fn write_u64(&mut self, i: u64) {
        self.write(&i.to_le_bytes());
    }

    fn write_u128(&mut self, i: u128) {
        self.write(&i.to_le_bytes());
    }

    fn write_usize(&mut self, i: usize) {
        // As a u64, so a 32-bit and a 64-bit machine agree.
        self.write_u64(i as u64);
    }

    fn write_isize(&mut self, i: isize) {
        // Sign-extended to 64 bits, for the same reason.
        self.write_u64((i as i64).cast_unsigned());
    }

    fn finish(&self) -> u64 {
        let mut h = self.0;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^= h >> 33;
        h
    }
}

/// A [`HyperLogLog`] shared behind a mutex. See the [module docs](self).
///
/// A poisoned lock is recovered: a register only ever grows, so a sketch
/// left by a panicking writer is still a valid sketch.
#[derive(Debug)]
pub struct DistinctCounter {
    sketch: Mutex<HyperLogLog>,
}

impl DistinctCounter {
    /// An empty shared sketch. See [`HyperLogLog::new`].
    #[must_use]
    pub fn new(precision: u8) -> Self {
        Self {
            sketch: Mutex::new(HyperLogLog::new(precision)),
        }
    }

    /// See [`HyperLogLog::insert`].
    pub fn insert<T: Hash + ?Sized>(&self, key: &T) {
        self.with(|sketch| sketch.insert(key));
    }

    /// See [`HyperLogLog::insert_hash`].
    pub fn insert_hash(&self, hash: u64) {
        self.with(|sketch| sketch.insert_hash(hash));
    }

    /// See [`HyperLogLog::estimate`].
    #[must_use]
    pub fn estimate(&self) -> u64 {
        self.with(|sketch| sketch.estimate())
    }

    /// See [`HyperLogLog::merge`].
    ///
    /// # Errors
    ///
    /// [`PrecisionMismatch`] when the precisions differ.
    pub fn merge(&self, other: &HyperLogLog) -> Result<(), PrecisionMismatch> {
        self.with(|sketch| sketch.merge(other))
    }

    /// A copy of the sketch, to merge elsewhere or ship with
    /// [`HyperLogLog::to_bytes`].
    #[must_use]
    pub fn snapshot(&self) -> HyperLogLog {
        self.with(|sketch| sketch.clone())
    }

    /// Hands back the sketch and starts an empty one of the same precision,
    /// in one step: no key falls between the two windows.
    #[must_use]
    pub fn take(&self) -> HyperLogLog {
        self.with(|sketch| {
            let fresh = HyperLogLog::new(sketch.precision);
            std::mem::replace(sketch, fresh)
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut HyperLogLog) -> R) -> R {
        let mut sketch = self.sketch.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut sketch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Asserts `estimate` is within `sigmas` standard errors of `truth`.
    #[allow(clippy::cast_precision_loss)]
    fn assert_close(sketch: &HyperLogLog, truth: u64, sigmas: f64) {
        let estimate = sketch.estimate() as f64;
        let truth_f = truth as f64;
        let error = (estimate - truth_f).abs() / truth_f;
        let bound = sigmas * sketch.standard_error();
        assert!(
            error <= bound,
            "estimate {estimate} for {truth} distinct keys is off by {:.3} %, over {:.3} %",
            error * 100.0,
            bound * 100.0
        );
    }

    #[test]
    fn empty_sketch_counts_nothing() {
        let sketch = HyperLogLog::new(12);
        assert!(sketch.is_empty());
        assert_eq!(sketch.estimate(), 0);
    }

    #[test]
    fn precision_is_clamped() {
        assert_eq!(HyperLogLog::new(0).precision(), HyperLogLog::MIN_PRECISION);
        assert_eq!(HyperLogLog::new(40).precision(), HyperLogLog::MAX_PRECISION);
        assert_eq!(HyperLogLog::new(14).precision(), 14);
    }

    #[test]
    fn standard_error_follows_the_register_count() {
        let error = HyperLogLog::new(14).standard_error();
        assert!((error - 1.04 / 128.0).abs() < 1e-12);
    }

    #[test]
    fn small_sets_are_counted_exactly() {
        let mut sketch = HyperLogLog::new(12);
        for i in 0..10_u32 {
            sketch.insert(&i);
        }
        assert!(!sketch.is_empty());
        assert_eq!(sketch.estimate(), 10);
    }

    #[test]
    fn duplicates_do_not_inflate_the_count() {
        let mut sketch = HyperLogLog::new(12);
        for _ in 0..1_000 {
            for name in ["ada", "grace", "linus"] {
                sketch.insert(name);
            }
        }
        assert_eq!(sketch.estimate(), 3);
    }

    #[test]
    fn linear_counting_range_is_accurate() {
        let mut sketch = HyperLogLog::new(12);
        for i in 0..2_000_u64 {
            sketch.insert(&i);
        }
        assert_close(&sketch, 2_000, 3.0);
    }

    #[test]
    fn large_sets_are_within_three_standard_errors() {
        for truth in [50_000_u64, 250_000] {
            let mut sketch = HyperLogLog::new(12);
            for i in 0..truth {
                sketch.insert(&i);
            }
            assert_close(&sketch, truth, 3.0);
        }
    }

    #[test]
    fn no_bias_across_the_small_range_cutoff() {
        // Classic HyperLogLog switches from linear counting to the raw
        // estimate at 2.5 m and is biased high just past it. Sweep 1 m..6 m
        // at a high precision, where the standard error is tightest.
        let mut sketch = HyperLogLog::new(16);
        let m = 1_u64 << 16;
        let mut inserted = 0_u64;
        for step in 1..=12_u64 {
            let truth = step * m / 2;
            while inserted < truth {
                sketch.insert(&inserted);
                inserted += 1;
            }
            assert_close(&sketch, truth, 3.0);
        }
    }

    #[test]
    fn isize_keys_hash_as_64_bit() {
        // A negative isize must not depend on the target's pointer width.
        let mut sketch = HyperLogLog::new(4);
        sketch.insert(&-1_isize);
        let mut expected = HyperLogLog::new(4);
        expected.insert(&-1_i64);
        assert_eq!(sketch, expected);
    }

    #[test]
    fn string_keys_are_within_three_standard_errors() {
        let mut sketch = HyperLogLog::new(14);
        for i in 0..100_000_u32 {
            sketch.insert(&format!("visitor-{i}"));
        }
        assert_close(&sketch, 100_000, 3.0);
    }

    #[test]
    fn same_stream_gives_the_same_sketch() {
        let mut a = HyperLogLog::new(10);
        let mut b = HyperLogLog::new(10);
        for i in 0..5_000_u64 {
            a.insert(&i);
            b.insert(&i);
        }
        assert_eq!(a, b);
    }

    #[test]
    fn insertion_order_does_not_matter() {
        let mut forward = HyperLogLog::new(10);
        let mut backward = HyperLogLog::new(10);
        for i in 0..5_000_u64 {
            forward.insert(&i);
            backward.insert(&(4_999 - i));
        }
        assert_eq!(forward, backward);
    }

    #[test]
    fn merge_counts_the_union_once() {
        // Two replicas whose visitors overlap by half.
        let mut east = HyperLogLog::new(12);
        let mut west = HyperLogLog::new(12);
        for i in 0..60_000_u64 {
            east.insert(&i);
        }
        for i in 30_000..90_000_u64 {
            west.insert(&i);
        }
        let mut whole = HyperLogLog::new(12);
        for i in 0..90_000_u64 {
            whole.insert(&i);
        }

        east.merge(&west).unwrap();
        assert_eq!(east, whole, "a merge is the sketch of the union");
        assert_close(&east, 90_000, 3.0);
    }

    #[test]
    fn merge_rejects_a_different_precision() {
        let mut a = HyperLogLog::new(12);
        a.insert("ada");
        let before = a.clone();
        let err = a.merge(&HyperLogLog::new(10)).unwrap_err();
        assert_eq!(
            err,
            PrecisionMismatch {
                expected: 12,
                found: 10
            }
        );
        assert_eq!(a, before);
        assert!(err.to_string().contains("precision-10"));
    }

    #[test]
    fn clear_forgets_everything() {
        let mut sketch = HyperLogLog::new(8);
        sketch.insert("ada");
        sketch.clear();
        assert!(sketch.is_empty());
        assert_eq!(sketch.precision(), 8);
    }

    #[test]
    fn bytes_round_trip() {
        let mut sketch = HyperLogLog::new(9);
        for i in 0..3_000_u64 {
            sketch.insert(&i);
        }
        let bytes = sketch.to_bytes();
        assert_eq!(bytes.len(), 1 + 512);
        assert_eq!(bytes[0], 9);
        assert_eq!(HyperLogLog::from_bytes(&bytes), Some(sketch));
    }

    #[test]
    fn from_bytes_rejects_what_is_not_a_sketch() {
        assert_eq!(HyperLogLog::from_bytes(&[]), None);
        // Precision out of range.
        assert_eq!(HyperLogLog::from_bytes(&[3; 9]), None);
        // Length does not match the precision.
        assert_eq!(HyperLogLog::from_bytes(&[4; 16]), None);
        // A register past 64 - precision + 1.
        let mut bytes = HyperLogLog::new(4).to_bytes();
        bytes[1] = 62;
        assert_eq!(HyperLogLog::from_bytes(&bytes), None);
        bytes[1] = 61;
        assert!(HyperLogLog::from_bytes(&bytes).is_some());
    }

    #[test]
    fn extreme_hashes_stay_in_range() {
        let mut sketch = HyperLogLog::new(4);
        sketch.insert_hash(0);
        sketch.insert_hash(u64::MAX);
        let bytes = sketch.to_bytes();
        assert_eq!(bytes[1], 61, "an all-zero tail ranks 64 - precision + 1");
        assert!(HyperLogLog::from_bytes(&bytes).is_some());
        assert!(sketch.estimate() >= 1);
    }

    #[test]
    fn integer_keys_hash_the_same_on_every_platform() {
        // Pinned: a change here breaks merging with sketches already stored.
        let mut sketch = HyperLogLog::new(4);
        sketch.insert(&1_u64);
        let mut hasher = StableHasher::default();
        hasher.write(&1_u64.to_le_bytes());
        let mut expected = HyperLogLog::new(4);
        expected.insert_hash(hasher.finish());
        assert_eq!(sketch, expected);
    }

    #[test]
    fn counter_is_shared_across_threads() {
        let counter = Arc::new(DistinctCounter::new(12));
        let workers: Vec<_> = (0..4_u64)
            .map(|worker| {
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    for i in 0..1_000_u64 {
                        // Every worker also sees the shared keys 0..100.
                        counter.insert(&(worker * 1_000 + i));
                        counter.insert(&(i % 100));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_close(&counter.snapshot(), 4_000, 3.0);
    }

    #[test]
    fn take_starts_a_fresh_window() {
        let counter = DistinctCounter::new(10);
        counter.insert("ada");
        counter.insert_hash(42);
        let window = counter.take();
        assert_eq!(window.estimate(), 2);
        assert_eq!(window.precision(), 10);
        assert_eq!(counter.estimate(), 0);
    }

    #[test]
    fn counter_merges_a_peer_sketch() {
        let counter = DistinctCounter::new(10);
        counter.insert("ada");
        let mut peer = HyperLogLog::new(10);
        peer.insert("grace");
        counter.merge(&peer).unwrap();
        assert_eq!(counter.estimate(), 2);
        assert!(counter.merge(&HyperLogLog::new(11)).is_err());
    }
}
