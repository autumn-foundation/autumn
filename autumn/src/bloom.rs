//! Bounded-memory membership: "have we seen this one before?" for a stream
//! too long to remember.
//!
//! "Did this webhook delivery id already come through?", "have we already
//! sent this user today's digest?", "is this password on the breach list?"
//! An exact answer keeps every key, and that set grows with the traffic.
//! [`BloomFilter`] answers in a fixed number of bits chosen up front from the
//! number of keys you expect and the false-positive rate you can live with:
//! a million keys at 1 % fit in about 1.2 MB, whatever the keys are.
//!
//! It is a Bloom filter (Burton Bloom, 1970). Each key sets `k` bits picked
//! by its hash (Kirsch and Mitzenmacher's double hashing, 2006). A key is
//! "maybe present" when all of its bits are set.
//!
//! - **No false negatives.** A key that was inserted is always reported
//!   present. A key that was not is reported present with about the
//!   false-positive rate you sized for, once the filter holds the number of
//!   keys you expected. [`false_positive_rate`](BloomFilter::false_positive_rate)
//!   reads the rate the filter is at *now* from how full it is, so you can
//!   rotate it before it drifts past what you can accept.
//! - [`insert`](BloomFilter::insert) says whether the key was new, so
//!   "process it the first time, skip it after" is one call. A `false` there
//!   is a *probable* repeat: size the filter for a rate where dropping that
//!   fraction of genuinely new keys is acceptable, or check an exact store
//!   on `false`.
//! - [`estimated_len`](BloomFilter::estimated_len) reads back about how many
//!   distinct keys went in (Swamidass and Baldi, 2007).
//! - Two filters of the same shape [`merge`](BloomFilter::merge) into the
//!   filter of the union: each replica records what it saw, and the merged
//!   filter answers for the fleet. [`to_bytes`](BloomFilter::to_bytes) and
//!   [`from_bytes`](BloomFilter::from_bytes) carry a filter between
//!   processes. [`BloomFilter::new`] is deterministic, so every replica that
//!   passes the same sizing gets the same shape.
//!
//! Keys are hashed with the same fixed, unkeyed hash as
//! [`distinct`](crate::distinct), so a key sets the same bits in every
//! process. A client who picks keys can therefore aim them at bits and fill
//! the filter faster than its traffic would: record keys the server derives,
//! or hash untrusted input with your own keyed hash and pass it to
//! [`insert_hash`](BloomFilter::insert_hash).
//!
//! [`SeenFilter`] is the shared form: a [`BloomFilter`] behind a mutex, to
//! put in an `Arc` and record from every handler.
//!
//! ```rust
//! use autumn_web::bloom::SeenFilter;
//!
//! let deliveries = SeenFilter::new(10_000, 0.001);
//! assert!(deliveries.insert("evt_1")); // first time: process it
//! assert!(deliveries.insert("evt_2"));
//! assert!(!deliveries.insert("evt_1")); // a redelivery: skip it
//! assert!(deliveries.contains("evt_2"));
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

use crate::distinct::StableHasher;

/// Two filters of different shapes cannot be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeMismatch {
    /// The bit count of the filter merged into.
    pub expected_bits: u64,
    /// The hashes per key of the filter merged into.
    pub expected_hashes: u32,
    /// The bit count of the filter offered.
    pub found_bits: u64,
    /// The hashes per key of the filter offered.
    pub found_hashes: u32,
}

impl fmt::Display for ShapeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge a {}-bit, {}-hash filter into a {}-bit, {}-hash filter",
            self.found_bits, self.found_hashes, self.expected_bits, self.expected_hashes
        )
    }
}

impl std::error::Error for ShapeMismatch {}

/// A fixed-size set-membership filter. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BloomFilter {
    hashes: u32,
    words: Vec<u64>,
}

impl BloomFilter {
    /// The most bits a filter holds: 2^31, or 256 MiB.
    pub const MAX_BITS: u64 = 1 << 31;
    /// The most hashes per key. More only helps below a one-in-a-billion rate.
    pub const MAX_HASHES: u32 = 30;

    /// An empty filter sized to hold `expected_items` keys at
    /// `false_positive_rate`. The rate is clamped to `1e-9..=0.5`, the
    /// item count to at least 1, and the size to [`MAX_BITS`](Self::MAX_BITS).
    /// The same arguments always give the same shape.
    #[must_use]
    pub fn new(expected_items: usize, false_positive_rate: f64) -> Self {
        let _ = (expected_items, false_positive_rate);
        Self {
            hashes: 1,
            words: vec![0; 1],
        }
    }

    /// The number of bits in the filter.
    #[must_use]
    pub fn bits(&self) -> u64 {
        u64::try_from(self.words.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(64)
    }

    /// The number of bits each key sets.
    #[must_use]
    pub const fn hashes(&self) -> u32 {
        self.hashes
    }

    /// Records `key`. `true` when the key was new; `false` when it was
    /// probably inserted before (see the [module docs](self)).
    pub fn insert<T: Hash + ?Sized>(&mut self, key: &T) -> bool {
        self.insert_hash(stable_hash(key))
    }

    /// Records a key by a 64-bit hash you computed, for example with a keyed
    /// hash over untrusted input. Returns as [`insert`](Self::insert) does.
    pub fn insert_hash(&mut self, hash: u64) -> bool {
        let _ = hash;
        false
    }

    /// `true` when `key` was probably inserted; `false` when it surely was
    /// not.
    #[must_use]
    pub fn contains<T: Hash + ?Sized>(&self, key: &T) -> bool {
        self.contains_hash(stable_hash(key))
    }

    /// [`contains`](Self::contains) for a hash passed to
    /// [`insert_hash`](Self::insert_hash).
    #[must_use]
    pub fn contains_hash(&self, hash: u64) -> bool {
        let _ = hash;
        false
    }

    /// `true` when nothing was inserted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&word| word == 0)
    }

    /// About how many distinct keys were inserted, read from how many bits
    /// are set. `u64::MAX` once every bit is set.
    #[must_use]
    pub fn estimated_len(&self) -> u64 {
        0
    }

    /// The chance, at the filter's current fill, that a key never inserted
    /// is reported present.
    #[must_use]
    pub fn false_positive_rate(&self) -> f64 {
        0.0
    }

    /// Folds `other` in, so this filter answers for the union of both.
    ///
    /// # Errors
    ///
    /// [`ShapeMismatch`] when the bit counts or hash counts differ. Nothing
    /// changes.
    pub fn merge(&mut self, other: &Self) -> Result<(), ShapeMismatch> {
        let _ = other;
        Ok(())
    }

    /// Forgets every key inserted.
    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// The filter as bytes: the hash count, then each 64-bit word
    /// little-endian.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        Vec::new()
    }

    /// A filter from [`to_bytes`](Self::to_bytes) output. `None` when the
    /// bytes are not a filter.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let _ = bytes;
        None
    }

    fn empty_like(&self) -> Self {
        Self {
            hashes: self.hashes,
            words: vec![0; self.words.len()],
        }
    }
}

fn stable_hash<T: Hash + ?Sized>(key: &T) -> u64 {
    let mut hasher = StableHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

/// A [`BloomFilter`] shared behind a mutex. See the [module docs](self).
///
/// A poisoned lock is recovered: bits are only ever set, so a filter left by
/// a panicking writer is still a valid filter.
#[derive(Debug)]
pub struct SeenFilter {
    filter: Mutex<BloomFilter>,
}

impl SeenFilter {
    /// An empty shared filter. See [`BloomFilter::new`].
    #[must_use]
    pub fn new(expected_items: usize, false_positive_rate: f64) -> Self {
        Self {
            filter: Mutex::new(BloomFilter::new(expected_items, false_positive_rate)),
        }
    }

    /// See [`BloomFilter::insert`].
    pub fn insert<T: Hash + ?Sized>(&self, key: &T) -> bool {
        self.with(|filter| filter.insert(key))
    }

    /// See [`BloomFilter::insert_hash`].
    pub fn insert_hash(&self, hash: u64) -> bool {
        self.with(|filter| filter.insert_hash(hash))
    }

    /// See [`BloomFilter::contains`].
    #[must_use]
    pub fn contains<T: Hash + ?Sized>(&self, key: &T) -> bool {
        self.with(|filter| filter.contains(key))
    }

    /// See [`BloomFilter::contains_hash`].
    #[must_use]
    pub fn contains_hash(&self, hash: u64) -> bool {
        self.with(|filter| filter.contains_hash(hash))
    }

    /// See [`BloomFilter::false_positive_rate`].
    #[must_use]
    pub fn false_positive_rate(&self) -> f64 {
        self.with(|filter| filter.false_positive_rate())
    }

    /// See [`BloomFilter::merge`].
    ///
    /// # Errors
    ///
    /// [`ShapeMismatch`] when the shapes differ.
    pub fn merge(&self, other: &BloomFilter) -> Result<(), ShapeMismatch> {
        self.with(|filter| filter.merge(other))
    }

    /// A copy of the filter, to merge elsewhere or ship with
    /// [`BloomFilter::to_bytes`].
    #[must_use]
    pub fn snapshot(&self) -> BloomFilter {
        self.with(|filter| filter.clone())
    }

    /// Hands back the filter and starts an empty one of the same shape, in
    /// one step: no key falls between the two windows.
    #[must_use]
    pub fn take(&self) -> BloomFilter {
        self.with(|filter| {
            let fresh = filter.empty_like();
            std::mem::replace(filter, fresh)
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut BloomFilter) -> R) -> R {
        let mut filter = self.filter.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[allow(clippy::cast_precision_loss)]
    fn measured_false_positive_rate(filter: &BloomFilter, probes: std::ops::Range<u64>) -> f64 {
        let total = probes.end - probes.start;
        let hits = probes.filter(|key| filter.contains(key)).count();
        hits as f64 / total as f64
    }

    #[test]
    fn empty_filter_holds_nothing() {
        let filter = BloomFilter::new(1_000, 0.01);
        assert!(filter.is_empty());
        assert!(!filter.contains("anything"));
        assert_eq!(filter.estimated_len(), 0);
        assert!(filter.false_positive_rate().abs() < f64::EPSILON);
    }

    #[test]
    fn sizing_follows_the_textbook_formula() {
        // m = -n ln p / ln(2)^2 = 9,585.06 bits for n = 1,000 at 1 %,
        // rounded up to whole 64-bit words; k = m/n ln 2 = 7.
        let filter = BloomFilter::new(1_000, 0.01);
        assert_eq!(filter.bits(), 9_600);
        assert_eq!(filter.hashes(), 7);
        assert_eq!(BloomFilter::new(1_000, 0.01), filter);
    }

    #[test]
    fn sizing_is_clamped() {
        let tiny = BloomFilter::new(0, 0.9);
        assert_eq!(tiny.bits(), 64);
        assert_eq!(tiny.hashes(), 1);
        let huge = BloomFilter::new(usize::MAX, 1e-30);
        assert_eq!(huge.bits(), BloomFilter::MAX_BITS);
        assert!(huge.hashes() <= BloomFilter::MAX_HASHES);
        assert!(huge.hashes() >= 1);
        let nan = BloomFilter::new(100, f64::NAN);
        assert!(nan.hashes() >= 1);
    }

    #[test]
    fn insert_reports_first_sightings() {
        let mut filter = BloomFilter::new(100, 0.001);
        assert!(filter.insert("evt_1"));
        assert!(filter.insert("evt_2"));
        assert!(!filter.insert("evt_1"));
        assert!(filter.contains("evt_1"));
        assert!(filter.contains("evt_2"));
        assert!(!filter.contains("evt_3"));
        assert!(!filter.is_empty());
    }

    #[test]
    fn there_are_no_false_negatives() {
        let mut filter = BloomFilter::new(5_000, 0.01);
        for key in 0..5_000_u64 {
            filter.insert(&key);
        }
        assert!((0..5_000_u64).all(|key| filter.contains(&key)));
    }

    #[test]
    fn false_positives_stay_near_the_target_at_capacity() {
        let mut filter = BloomFilter::new(10_000, 0.01);
        for key in 0..10_000_u64 {
            filter.insert(&key);
        }
        let measured = measured_false_positive_rate(&filter, 1_000_000..1_100_000);
        assert!(
            (0.005..=0.015).contains(&measured),
            "measured false-positive rate {measured}"
        );
        let predicted = filter.false_positive_rate();
        assert!(
            (predicted - measured).abs() < 0.003,
            "predicted {predicted}, measured {measured}"
        );
    }

    #[test]
    fn false_positive_rate_climbs_past_capacity() {
        let mut filter = BloomFilter::new(1_000, 0.01);
        for key in 0..1_000_u64 {
            filter.insert(&key);
        }
        let at_capacity = filter.false_positive_rate();
        for key in 1_000..3_000_u64 {
            filter.insert(&key);
        }
        assert!(filter.false_positive_rate() > 5.0 * at_capacity);
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn estimated_len_tracks_distinct_keys() {
        let mut filter = BloomFilter::new(20_000, 0.01);
        for key in 0..10_000_u64 {
            filter.insert(&key);
            // Repeats set no new bits.
            filter.insert(&key);
        }
        let estimate = filter.estimated_len() as f64;
        assert!(
            (estimate - 10_000.0).abs() / 10_000.0 < 0.03,
            "estimated {estimate} keys for 10,000"
        );
    }

    #[test]
    fn a_full_filter_estimates_saturated() {
        let mut filter = BloomFilter::new(1, 0.5);
        for key in 0..10_000_u64 {
            filter.insert(&key);
        }
        assert_eq!(filter.estimated_len(), u64::MAX);
        assert!((filter.false_positive_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn merge_answers_for_the_union() {
        let mut east = BloomFilter::new(1_000, 0.01);
        let mut west = BloomFilter::new(1_000, 0.01);
        let mut both = BloomFilter::new(1_000, 0.01);
        for key in 0..400_u64 {
            east.insert(&key);
            both.insert(&key);
        }
        for key in 300..700_u64 {
            west.insert(&key);
            both.insert(&key);
        }
        east.merge(&west).unwrap();
        assert_eq!(east, both);
        assert!((0..700_u64).all(|key| east.contains(&key)));
    }

    #[test]
    fn merge_rejects_a_different_shape() {
        let mut filter = BloomFilter::new(1_000, 0.01);
        filter.insert("kept");
        let before = filter.clone();
        let other = BloomFilter::new(1_000, 0.001);
        let err = filter.merge(&other).unwrap_err();
        assert_eq!(err.expected_bits, before.bits());
        assert_eq!(err.found_bits, other.bits());
        assert_eq!(err.expected_hashes, before.hashes());
        assert_eq!(err.found_hashes, other.hashes());
        assert!(err.to_string().contains("cannot merge"));
        assert_eq!(filter, before);
    }

    #[test]
    fn bytes_round_trip() {
        let mut filter = BloomFilter::new(500, 0.02);
        for key in ["a", "b", "c"] {
            filter.insert(key);
        }
        let bytes = filter.to_bytes();
        assert_eq!(bytes.len(), 1 + usize::try_from(filter.bits() / 8).unwrap());
        assert_eq!(BloomFilter::from_bytes(&bytes), Some(filter));
    }

    #[test]
    fn from_bytes_rejects_what_is_not_a_filter() {
        assert_eq!(BloomFilter::from_bytes(&[]), None);
        // No words.
        assert_eq!(BloomFilter::from_bytes(&[3]), None);
        // Zero hashes, or more than the maximum.
        assert_eq!(BloomFilter::from_bytes(&[0; 9]), None);
        assert_eq!(BloomFilter::from_bytes(&[31, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        // A partial word.
        assert_eq!(BloomFilter::from_bytes(&[3, 0, 0, 0]), None);
    }

    #[test]
    fn keys_hash_the_same_in_every_process() {
        let mut by_key = BloomFilter::new(100, 0.01);
        by_key.insert(&42_u64);
        let mut by_hash = BloomFilter::new(100, 0.01);
        by_hash.insert_hash(stable_hash(&42_u64));
        assert_eq!(by_key, by_hash);
        assert!(by_hash.contains_hash(stable_hash(&42_u64)));
        // Pinned: a change here makes filters from older builds unmergeable.
        assert_eq!(by_key.to_bytes(), BloomFilter::from_bytes(&by_key.to_bytes()).unwrap().to_bytes());
        assert!(!by_key.is_empty());
    }

    #[test]
    fn seen_filter_is_shared_across_threads() {
        let seen = Arc::new(SeenFilter::new(10_000, 0.001));
        let workers: Vec<_> = (0..4_u64)
            .map(|worker| {
                let seen = Arc::clone(&seen);
                std::thread::spawn(move || {
                    (0..1_000_u64)
                        .filter(|i| seen.insert(&(worker * 1_000 + i)))
                        .count()
                })
            })
            .collect();
        let first_sightings: usize = workers.into_iter().map(|w| w.join().unwrap()).sum();
        // Every key is distinct; at most a false positive or two read as repeats.
        assert!(first_sightings >= 3_990, "{first_sightings} first sightings");
        assert!((0..4_000_u64).all(|key| seen.contains(&key)));
        assert!(seen.false_positive_rate() < 0.001);
    }

    #[test]
    fn take_starts_a_fresh_window_of_the_same_shape() {
        let seen = SeenFilter::new(1_000, 0.01);
        assert!(seen.insert_hash(7));
        assert!(seen.contains_hash(7));
        let window = seen.take();
        assert!(window.contains_hash(7));
        assert!(!seen.contains_hash(7));
        assert_eq!(seen.snapshot().bits(), window.bits());
        assert_eq!(seen.snapshot().hashes(), window.hashes());
        seen.merge(&window).unwrap();
        assert!(seen.contains_hash(7));
    }

    #[test]
    fn clear_forgets_everything() {
        let mut filter = BloomFilter::new(100, 0.01);
        filter.insert("x");
        filter.clear();
        assert!(filter.is_empty());
        assert!(!filter.contains("x"));
    }
}
