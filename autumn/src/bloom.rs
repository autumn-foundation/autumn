//! Bounded-memory membership: "have I seen this before?" in fixed memory.
//!
//! "Has this webhook delivery id been processed?", "is this the first time
//! this device logged in?", "was this password reset token already spent?",
//! "has this crawler fetched this URL today?" An exact answer keeps every key
//! it has seen, and that set grows with the traffic. [`BloomFilter`] answers
//! in a fixed number of bits, whatever the traffic: about 1.2 bytes a key
//! for a 1 % false-positive rate, 1.8 bytes for 0.1 %.
//!
//! It is a Bloom filter (Bloom, 1970) probed by enhanced double hashing
//! (Kirsch and Mitzenmacher, 2006): each key is hashed once to 64 bits, and
//! the `k` bit positions are derived from that one hash.
//!
//! - **No false negatives.** A key that was inserted is always reported as
//!   present. A key that was never inserted is reported present with the
//!   probability [`false_positive_rate`](BloomFilter::false_positive_rate),
//!   which the filter computes from how full it is. Use it where a false
//!   "seen" is cheap (skip a cache warm, take the slow exact path, show a
//!   "welcome back") and a false "new" would be wrong.
//! - [`insert`](BloomFilter::insert) says whether the key was new, so
//!   check-and-mark is one call.
//! - [`BloomFilter::new`] sizes the filter for an expected number of keys
//!   and a target false-positive rate. Past that many keys it keeps working;
//!   the rate just climbs, and [`false_positive_rate`](BloomFilter::false_positive_rate)
//!   says by how much.
//! - Two filters of the same shape [`merge`](BloomFilter::merge) into the
//!   filter of the union (a bitwise OR). Each replica marks what it saw, and
//!   the merged filter knows what the fleet saw.
//!   [`to_bytes`](BloomFilter::to_bytes) and
//!   [`from_bytes`](BloomFilter::from_bytes) carry a filter between
//!   processes.
//! - [`estimated_len`](BloomFilter::estimated_len) reads back roughly how
//!   many distinct keys went in, from the share of bits set.
//!
//! Keys are hashed with the same fixed, unkeyed hash as
//! [`crate::distinct`], with the same consequences: filters from different
//! processes and machines agree, and a client who picks keys can aim them
//! at bits to raise the false-positive rate. Insert keys the server derives,
//! or hash untrusted input with your own keyed hash and pass it to
//! [`insert_hash`](BloomFilter::insert_hash).
//!
//! [`SeenSet`] is the shared form: a [`BloomFilter`] behind a mutex, to put
//! in an `Arc` and check from every handler.
//!
//! ```rust
//! use autumn_web::bloom::SeenSet;
//!
//! let deliveries = SeenSet::new(10_000, 0.001);
//! assert!(deliveries.insert("evt_1"));  // surely new: no exact lookup needed
//! assert!(deliveries.insert("evt_2"));
//! assert!(!deliveries.insert("evt_1")); // a retry, or a false positive:
//!                                       // confirm against the exact store
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
    /// The `(bits, hashes)` of the filter merged into.
    pub expected: (u64, u32),
    /// The `(bits, hashes)` of the filter offered.
    pub found: (u64, u32),
}

impl fmt::Display for ShapeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge a {}-bit, {}-hash filter into a {}-bit, {}-hash filter",
            self.found.0, self.found.1, self.expected.0, self.expected.1
        )
    }
}

impl std::error::Error for ShapeMismatch {}

/// A fixed-size membership filter. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BloomFilter {
    words: Vec<u64>,
    hashes: u32,
}

impl BloomFilter {
    /// The most bit positions probed per key.
    pub const MAX_HASHES: u32 = 32;
    /// The largest filter: 2^30 bits (128 MiB).
    pub const MAX_BITS: u64 = 1 << 30;
    /// The lowest false-positive rate [`new`](Self::new) sizes for.
    pub const MIN_FALSE_POSITIVE_RATE: f64 = 1e-9;

    /// An empty filter sized to hold `expected_keys` keys at
    /// `false_positive_rate`. `expected_keys` is at least 1, and the rate is
    /// clamped to [`MIN_FALSE_POSITIVE_RATE`](Self::MIN_FALSE_POSITIVE_RATE)..=0.5
    /// (a NaN rate is read as 0.01). The size is capped at
    /// [`MAX_BITS`](Self::MAX_BITS).
    #[must_use]
    pub fn new(expected_keys: u64, false_positive_rate: f64) -> Self {
        let keys = to_f64(expected_keys.max(1));
        let rate = if false_positive_rate.is_nan() {
            0.01
        } else {
            false_positive_rate.clamp(Self::MIN_FALSE_POSITIVE_RATE, 0.5)
        };
        // m = -n ln p / ln(2)^2 bits, and k = m / n * ln 2 probes per key.
        let bits = to_u64((-keys * rate.ln() / (LN_2 * LN_2)).ceil());
        let bits = bits.clamp(WORD_BITS, Self::MAX_BITS);
        let hashes = to_u64((to_f64(bits) / keys * LN_2).round());
        Self::with_shape(bits, u32::try_from(hashes).unwrap_or(Self::MAX_HASHES))
    }

    /// An empty filter of exactly `bits` bits (rounded up to a multiple of
    /// 64, at least 64, at most [`MAX_BITS`](Self::MAX_BITS)) probed at
    /// `hashes` positions per key (1..=[`MAX_HASHES`](Self::MAX_HASHES)).
    #[must_use]
    pub fn with_shape(bits: u64, hashes: u32) -> Self {
        let words = bits.clamp(WORD_BITS, Self::MAX_BITS).div_ceil(WORD_BITS);
        Self {
            // At most 2^24 words, which fits any `usize`.
            words: vec![0; usize::try_from(words).unwrap_or(1 << 24)],
            hashes: hashes.clamp(1, Self::MAX_HASHES),
        }
    }

    /// The number of bits in the filter.
    #[must_use]
    pub const fn bits(&self) -> u64 {
        (self.words.len() as u64).saturating_mul(WORD_BITS)
    }

    /// The number of bit positions probed per key.
    #[must_use]
    pub const fn hashes(&self) -> u32 {
        self.hashes
    }

    /// Marks `key` as seen. `true` when it was new: at least one of its bits
    /// was clear. `false` when it was already present, or is a false positive.
    pub fn insert<T: Hash + ?Sized>(&mut self, key: &T) -> bool {
        self.insert_hash(stable_hash(key))
    }

    /// [`insert`](Self::insert) for a key already hashed to 64 bits.
    pub fn insert_hash(&mut self, hash: u64) -> bool {
        let mut new = false;
        for (word, mask) in self.probes(hash) {
            if let Some(word) = self.words.get_mut(word) {
                new |= *word & mask == 0;
                *word |= mask;
            }
        }
        new
    }

    /// `true` when `key` may have been inserted; `false` when it surely was
    /// not.
    #[must_use]
    pub fn contains<T: Hash + ?Sized>(&self, key: &T) -> bool {
        self.contains_hash(stable_hash(key))
    }

    /// [`contains`](Self::contains) for a key already hashed to 64 bits.
    #[must_use]
    pub fn contains_hash(&self, hash: u64) -> bool {
        self.probes(hash)
            .all(|(word, mask)| self.words.get(word).is_some_and(|word| word & mask != 0))
    }

    /// `true` when nothing has been inserted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&word| word == 0)
    }

    /// How many bits are set.
    #[must_use]
    pub fn set_bits(&self) -> u64 {
        self.words
            .iter()
            .map(|word| u64::from(word.count_ones()))
            .sum()
    }

    /// The chance that [`contains`](Self::contains) says `true` for a key
    /// never inserted, given how full the filter is now:
    /// `(set_bits / bits) ^ hashes`.
    #[must_use]
    pub fn false_positive_rate(&self) -> f64 {
        let fill = to_f64(self.set_bits()) / to_f64(self.bits());
        fill.powi(i32::try_from(self.hashes).unwrap_or(i32::MAX))
    }

    /// Roughly how many distinct keys were inserted, read from the share of
    /// bits set (Swamidass and Baldi, 2007). A merged filter counts the
    /// union. Saturates at `u64::MAX` when every bit is set.
    #[must_use]
    pub fn estimated_len(&self) -> u64 {
        let (set, bits) = (self.set_bits(), self.bits());
        if set >= bits {
            return u64::MAX;
        }
        // n = -(m / k) ln(1 - X / m), for X of m bits set.
        let (set, bits) = (to_f64(set), to_f64(bits));
        to_u64((-(bits / f64::from(self.hashes)) * (1.0 - set / bits).ln()).round())
    }

    /// Adds every key of `other` (a bitwise OR).
    ///
    /// # Errors
    ///
    /// [`ShapeMismatch`] when the bit counts or hash counts differ; `self`
    /// is left unchanged.
    pub fn merge(&mut self, other: &Self) -> Result<(), ShapeMismatch> {
        if (self.bits(), self.hashes) != (other.bits(), other.hashes) {
            return Err(ShapeMismatch {
                expected: (self.bits(), self.hashes),
                found: (other.bits(), other.hashes),
            });
        }
        for (word, theirs) in self.words.iter_mut().zip(&other.words) {
            *word |= theirs;
        }
        Ok(())
    }

    /// Forgets every key; the shape stays.
    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// The filter as bytes: the hash count (`u32`), then each 64-bit word,
    /// all little-endian.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.words.len().saturating_mul(8).saturating_add(4));
        bytes.extend_from_slice(&self.hashes.to_le_bytes());
        for word in &self.words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// A filter from [`to_bytes`](Self::to_bytes) output. `None` when the
    /// bytes are not a filter: a hash count out of range, no words, too many
    /// words, or a length that is not whole words.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (hashes, words) = bytes.split_first_chunk::<4>()?;
        let hashes = u32::from_le_bytes(*hashes);
        let max_len = usize::try_from(Self::MAX_BITS / 8).unwrap_or(usize::MAX);
        if !(1..=Self::MAX_HASHES).contains(&hashes)
            || words.is_empty()
            || words.len() > max_len
            || !words.len().is_multiple_of(8)
        {
            return None;
        }
        let words = words
            .chunks_exact(8)
            .map(|word| <[u8; 8]>::try_from(word).map(u64::from_le_bytes))
            .collect::<Result<_, _>>()
            .ok()?;
        Some(Self { words, hashes })
    }
}

impl BloomFilter {
    /// The `hashes` bit positions of `hash`, as `(word index, bit mask)`.
    ///
    /// Enhanced double hashing: the first probe is `hash`, the stride is a
    /// second hash mixed from it, and the stride grows by `i` each step so
    /// two keys that share a first probe and a stride still part ways. Each
    /// probe is mapped onto the bits by its high half (Lemire's
    /// multiply-shift), which needs no division.
    fn probes(&self, hash: u64) -> impl Iterator<Item = (usize, u64)> + use<> {
        let bits = u128::from(self.bits());
        let mut probe = hash;
        let mut stride = mix(hash) | 1;
        (0..u64::from(self.hashes)).map(move |i| {
            let position = (u128::from(probe).wrapping_mul(bits)).wrapping_shr(64);
            probe = probe.wrapping_add(stride);
            stride = stride.wrapping_add(i);
            let position = u64::try_from(position).unwrap_or(0);
            let word = usize::try_from(position.wrapping_shr(6)).unwrap_or(usize::MAX);
            (
                word,
                1_u64.wrapping_shl(u32::try_from(position & 63).unwrap_or(0)),
            )
        })
    }
}

const WORD_BITS: u64 = 64;
const LN_2: f64 = std::f64::consts::LN_2;

/// The `SplitMix64` finalizer, for a second hash independent of the first.
const fn mix(hash: u64) -> u64 {
    let mut z = hash ^ 0x9e37_79b9_7f4a_7c15;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Sizes and fill counts are at most 2^30 here, or are estimates where the
/// rounding of a larger `u64` does not matter.
#[allow(clippy::cast_precision_loss)]
const fn to_f64(n: u64) -> f64 {
    n as f64
}

/// Saturating: negative and NaN become 0, too large becomes `u64::MAX`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn to_u64(x: f64) -> u64 {
    x as u64
}

fn stable_hash<T: Hash + ?Sized>(key: &T) -> u64 {
    let mut hasher = StableHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

/// A [`BloomFilter`] shared behind a mutex. See the [module docs](self).
///
/// A poisoned lock is recovered: a bit is only ever set, so a filter left
/// by a panicking writer is still a valid filter.
#[derive(Debug)]
pub struct SeenSet {
    filter: Mutex<BloomFilter>,
}

impl SeenSet {
    /// An empty shared filter. See [`BloomFilter::new`].
    #[must_use]
    pub fn new(expected_keys: u64, false_positive_rate: f64) -> Self {
        Self::from_filter(BloomFilter::new(expected_keys, false_positive_rate))
    }

    /// Shares `filter`, for example one read back with
    /// [`BloomFilter::from_bytes`].
    #[must_use]
    pub const fn from_filter(filter: BloomFilter) -> Self {
        Self {
            filter: Mutex::new(filter),
        }
    }

    /// See [`BloomFilter::insert`]. Check-and-mark is one locked step, so
    /// two handlers racing on the same new key see `true` exactly once.
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
    /// one step: no key falls between the two windows. Rotate on a schedule
    /// to bound the false-positive rate of a filter that never stops filling.
    #[must_use]
    pub fn take(&self) -> BloomFilter {
        self.with(|filter| {
            let mut fresh = filter.clone();
            fresh.clear();
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

    #[test]
    fn empty_filter_contains_nothing() {
        let filter = BloomFilter::new(1_000, 0.01);
        assert!(filter.is_empty());
        assert_eq!(filter.set_bits(), 0);
        assert_eq!(filter.estimated_len(), 0);
        assert!(filter.false_positive_rate().abs() < f64::EPSILON);
        assert!(!filter.contains("anything"));
    }

    #[test]
    fn new_sizes_from_the_standard_formulas() {
        // m = -n ln p / ln(2)^2 = 9585.06, rounded up to whole words: 9600.
        // k = m / n * ln 2 = 6.64, rounded: 7.
        let filter = BloomFilter::new(1_000, 0.01);
        assert_eq!(filter.bits(), 9_600);
        assert_eq!(filter.hashes(), 7);
    }

    #[test]
    fn new_clamps_its_inputs() {
        let tiny = BloomFilter::new(0, 0.9);
        assert_eq!(tiny.bits(), 64);
        assert!((1..=BloomFilter::MAX_HASHES).contains(&tiny.hashes()));

        let huge = BloomFilter::new(u64::MAX, 1e-30);
        assert_eq!(huge.bits(), BloomFilter::MAX_BITS);
        // Capped bits over that many keys: one probe is the best shape.
        assert_eq!(huge.hashes(), 1);

        let nan = BloomFilter::new(1_000, f64::NAN);
        assert_eq!(nan, BloomFilter::new(1_000, 0.01));
    }

    #[test]
    fn with_shape_rounds_and_clamps() {
        let filter = BloomFilter::with_shape(100, 0);
        assert_eq!(filter.bits(), 128);
        assert_eq!(filter.hashes(), 1);
        let filter = BloomFilter::with_shape(0, 99);
        assert_eq!(filter.bits(), 64);
        assert_eq!(filter.hashes(), BloomFilter::MAX_HASHES);
    }

    #[test]
    fn insert_reports_new_keys_once() {
        let mut filter = BloomFilter::new(1_000, 0.001);
        assert!(filter.insert("evt_1"));
        assert!(!filter.insert("evt_1"));
        assert!(filter.insert("evt_2"));
        assert!(!filter.is_empty());
    }

    #[test]
    fn inserted_keys_are_never_missed() {
        let mut filter = BloomFilter::new(10_000, 0.01);
        for i in 0..10_000_u64 {
            filter.insert(&i);
        }
        assert!((0..10_000_u64).all(|i| filter.contains(&i)));
    }

    #[test]
    fn false_positives_stay_near_the_target() {
        for target in [0.05, 0.01, 0.001] {
            let mut filter = BloomFilter::new(10_000, target);
            for i in 0..10_000_u64 {
                filter.insert(&format!("member-{i}"));
            }
            let trials = 200_000_u32;
            let hits = (0..trials)
                .filter(|i| filter.contains(&format!("stranger-{i}")))
                .count();
            let measured = f64::from(u32::try_from(hits).unwrap()) / f64::from(trials);
            assert!(
                measured < target * 1.5,
                "target {target}, measured {measured}"
            );
            let predicted = filter.false_positive_rate();
            assert!(
                (measured - predicted).abs() < predicted * 0.35 + 1e-4,
                "predicted {predicted}, measured {measured}"
            );
        }
    }

    #[test]
    fn false_positive_rate_climbs_past_capacity() {
        let mut filter = BloomFilter::new(1_000, 0.01);
        for i in 0..1_000_u64 {
            filter.insert(&i);
        }
        let at_capacity = filter.false_positive_rate();
        assert!(at_capacity < 0.015, "{at_capacity}");
        for i in 1_000..3_000_u64 {
            filter.insert(&i);
        }
        assert!(filter.false_positive_rate() > at_capacity * 10.0);
    }

    #[test]
    fn estimated_len_tracks_distinct_keys() {
        let mut filter = BloomFilter::new(20_000, 0.01);
        for _ in 0..3 {
            for i in 0..10_000_u64 {
                filter.insert(&i);
            }
        }
        let estimate = filter.estimated_len();
        assert!((9_700..=10_300).contains(&estimate), "{estimate}");
    }

    #[test]
    fn saturated_filter_estimates_the_maximum() {
        let mut filter = BloomFilter::with_shape(64, 1);
        for i in 0..10_000_u64 {
            filter.insert(&i);
        }
        assert_eq!(filter.set_bits(), 64);
        assert_eq!(filter.estimated_len(), u64::MAX);
        assert!((filter.false_positive_rate() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn merge_holds_the_union() {
        let mut left = BloomFilter::new(2_000, 0.01);
        let mut right = BloomFilter::new(2_000, 0.01);
        for i in 0..1_000_u64 {
            left.insert(&i);
        }
        for i in 1_000..2_000_u64 {
            right.insert(&i);
        }
        let mut both = BloomFilter::new(2_000, 0.01);
        for i in 0..2_000_u64 {
            both.insert(&i);
        }
        left.merge(&right).unwrap();
        assert_eq!(left, both);
        assert!((0..2_000_u64).all(|i| left.contains(&i)));
    }

    #[test]
    fn merge_rejects_a_different_shape() {
        let mut filter = BloomFilter::with_shape(128, 3);
        filter.insert("kept");
        let before = filter.clone();
        let error = filter.merge(&BloomFilter::with_shape(128, 4)).unwrap_err();
        assert_eq!(
            error,
            ShapeMismatch {
                expected: (128, 3),
                found: (128, 4)
            }
        );
        assert_eq!(
            error.to_string(),
            "cannot merge a 128-bit, 4-hash filter into a 128-bit, 3-hash filter"
        );
        assert!(filter.merge(&BloomFilter::with_shape(256, 3)).is_err());
        assert_eq!(filter, before);
    }

    #[test]
    fn clear_forgets_everything_but_the_shape() {
        let mut filter = BloomFilter::new(100, 0.01);
        filter.insert("x");
        filter.clear();
        assert!(filter.is_empty());
        assert!(!filter.contains("x"));
        assert_eq!(filter.bits(), BloomFilter::new(100, 0.01).bits());
    }

    #[test]
    fn bytes_round_trip() {
        let mut filter = BloomFilter::new(500, 0.01);
        for i in 0..300_u64 {
            filter.insert(&i);
        }
        let bytes = filter.to_bytes();
        let back = BloomFilter::from_bytes(&bytes).unwrap();
        assert_eq!(back, filter);
        assert!((0..300_u64).all(|i| back.contains(&i)));
    }

    #[test]
    fn from_bytes_rejects_what_is_not_a_filter() {
        let good = BloomFilter::with_shape(128, 3).to_bytes();
        assert!(BloomFilter::from_bytes(&[]).is_none());
        assert!(BloomFilter::from_bytes(&good[..4]).is_none(), "no words");
        assert!(BloomFilter::from_bytes(&good[..good.len() - 1]).is_none());
        let mut zero_hashes = good.clone();
        zero_hashes[..4].copy_from_slice(&0_u32.to_le_bytes());
        assert!(BloomFilter::from_bytes(&zero_hashes).is_none());
        let mut many_hashes = good;
        many_hashes[..4].copy_from_slice(&33_u32.to_le_bytes());
        assert!(BloomFilter::from_bytes(&many_hashes).is_none());
    }

    #[test]
    fn same_keys_give_the_same_bits_on_every_run() {
        // Pinned so a change to the hash or the probe sequence, which would
        // break filters already shipped between processes, fails here.
        let mut filter = BloomFilter::with_shape(64, 2);
        filter.insert("autumn");
        assert_eq!(
            filter.to_bytes(),
            [2, 0, 0, 0, 0, 0, 64, 4, 0, 0, 0, 0],
            "bits 22 and 26 of one word, after a 2-hash header"
        );
    }

    #[test]
    fn seen_set_marks_each_key_once_across_threads() {
        let seen = Arc::new(SeenSet::new(10_000, 0.001));
        let firsts: usize = (0..8)
            .map(|_| {
                let seen = Arc::clone(&seen);
                std::thread::spawn(move || (0..1_000_u64).filter(|i| seen.insert(i)).count())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum();
        assert_eq!(firsts, 1_000);
        assert!(seen.contains(&999_u64));
        assert!(seen.false_positive_rate() < 0.001);
    }

    #[test]
    fn take_starts_a_fresh_window() {
        let seen = SeenSet::new(100, 0.01);
        assert!(seen.insert("a"));
        let window = seen.take();
        assert!(window.contains("a"));
        assert!(!seen.contains("a"));
        assert!(seen.insert("a"));
        assert_eq!(window.bits(), seen.snapshot().bits());
    }

    #[test]
    fn seen_set_merges_a_peer_filter() {
        let seen = SeenSet::new(100, 0.01);
        let mut peer = BloomFilter::new(100, 0.01);
        peer.insert("from-peer");
        seen.merge(&peer).unwrap();
        assert!(seen.contains("from-peer"));
        assert!(!seen.insert("from-peer"));
        assert!(seen.merge(&BloomFilter::new(10_000, 0.01)).is_err());
    }

    #[test]
    fn hashed_keys_round_trip() {
        let seen = SeenSet::from_filter(BloomFilter::new(100, 0.01));
        assert!(seen.insert_hash(0xdead_beef));
        assert!(seen.contains_hash(0xdead_beef));
        assert!(!seen.insert_hash(0xdead_beef));
    }
}
