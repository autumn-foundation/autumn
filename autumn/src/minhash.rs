//! Bounded-memory similarity: "how much do these two sets overlap?" without
//! keeping either set.
//!
//! "Is this new post a reworded copy of one we already removed?", "do these
//! two tenants hit the same endpoints?", "how much of today's visitor set
//! came back tomorrow?" An exact answer keeps both sets and intersects them.
//! [`MinHash`] keeps a fixed number of 64-bit slots per set instead, chosen up
//! front, and estimates the Jaccard similarity `|A ∩ B| / |A ∪ B|` from those
//! alone: 128 slots is 1 KiB per set and answers within about ±0.09, whatever
//! the sets hold.
//!
//! It is `MinHash` (Andrei Broder, 1997). Slot `i` holds the smallest value
//! the `i`-th hash function gave any key in the set. For two sets, slot `i`
//! agrees exactly when the key with the smallest `i`-th hash in the union is
//! in both, which happens with probability equal to their Jaccard
//! similarity. The fraction of agreeing slots estimates it.
//!
//! - [`jaccard`](MinHash::jaccard) is unbiased with standard error at most
//!   `1/sqrt(slots)` ([`standard_error`](MinHash::standard_error)). Quadruple
//!   the slots to halve it.
//! - Order and repeats do not change a sketch: the same set always gives the
//!   same slots, in every process.
//! - Two sketches with the same slot count [`merge`](MinHash::merge) into the
//!   sketch of the union, exactly. [`to_bytes`](MinHash::to_bytes) and
//!   [`from_bytes`](MinHash::from_bytes) carry a sketch between processes or
//!   into a database column, so you can store one per document and compare
//!   a new one against them.
//! - [`of_text`](MinHash::of_text) sketches a text by its word shingles
//!   (runs of `width` consecutive words, case and punctuation ignored), the
//!   standard way to find near-duplicate documents.
//!
//! Keys are hashed with the same fixed, unkeyed hash as
//! [`distinct`](crate::distinct). A client who picks keys can choose ones
//! with small hashes and steer the estimate. Sketch keys the server derives,
//! or hash untrusted input with your own keyed hash and pass it to
//! [`insert_hash`](MinHash::insert_hash).
//!
//! ```rust
//! use autumn_web::minhash::MinHash;
//!
//! let removed = MinHash::of_text("Buy cheap watches now at the best price in town", 2, 128);
//! let incoming = MinHash::of_text("buy CHEAP watches now, at the best price in town!!", 2, 128);
//! let unrelated = MinHash::of_text("Meeting notes for the quarterly planning review", 2, 128);
//!
//! assert!(removed.jaccard(&incoming).unwrap() > 0.8); // the same spam
//! assert!(removed.jaccard(&unrelated).unwrap() < 0.2);
//! ```
//!
//! Needs the opt-in `sketches` feature.

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

use crate::bloom::mix;
use crate::distinct::StableHasher;

/// Two sketches with different slot counts cannot be compared or merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotMismatch {
    /// The slot count of the sketch called on.
    pub expected: usize,
    /// The slot count of the sketch offered.
    pub found: usize,
}

impl fmt::Display for SlotMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot combine a {}-slot MinHash with a {}-slot MinHash",
            self.found, self.expected
        )
    }
}

impl std::error::Error for SlotMismatch {}

/// A fixed-size similarity sketch of a set. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinHash {
    mins: Vec<u64>,
}

impl MinHash {
    /// The most slots a sketch holds: 4096, or 32 KiB, for a standard error
    /// of about 0.016.
    pub const MAX_SLOTS: usize = 4096;

    /// An empty sketch of `slots` slots, clamped to `1..=MAX_SLOTS`.
    #[must_use]
    pub fn new(slots: usize) -> Self {
        Self {
            mins: vec![u64::MAX; slots.clamp(1, Self::MAX_SLOTS)],
        }
    }

    /// A sketch of `text`'s word shingles: every run of `width` consecutive
    /// words, compared without case or punctuation. A text with fewer than
    /// `width` words is one shingle; a text with no words is the empty
    /// sketch. A `width` of 0 is taken as 1. Widths of 2 to 5 suit
    /// near-duplicate detection: wider is stricter.
    #[must_use]
    pub fn of_text(text: &str, width: usize, slots: usize) -> Self {
        let mut sketch = Self::new(slots);
        let words: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        if words.is_empty() {
            return sketch;
        }
        let width = width.clamp(1, words.len());
        for shingle in words.windows(width) {
            let mut hasher = StableHasher::default();
            for word in shingle {
                hasher.write(word.as_bytes());
                // 0xff never appears in UTF-8, so word boundaries are kept.
                hasher.write_u8(0xff);
            }
            sketch.insert_hash(hasher.finish());
        }
        sketch
    }

    /// The number of slots.
    #[must_use]
    pub const fn slots(&self) -> usize {
        self.mins.len()
    }

    /// `true` when nothing was inserted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mins.iter().all(|&min| min == u64::MAX)
    }

    /// Forgets every key inserted.
    pub fn clear(&mut self) {
        self.mins.fill(u64::MAX);
    }

    /// Adds `key` to the set.
    pub fn insert<T: Hash + ?Sized>(&mut self, key: &T) {
        self.insert_hash(stable_hash(key));
    }

    /// Adds a key by a 64-bit hash you computed, for example with a keyed
    /// hash over untrusted input.
    pub fn insert_hash(&mut self, hash: u64) {
        let mut seed = 0_u64;
        for min in &mut self.mins {
            // Slot `i` hashes with the `i`-th step of a SplitMix64 stream.
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            *min = (*min).min(mix(hash ^ seed));
        }
    }

    /// The estimated Jaccard similarity of the two sets, from 0.0 (nothing in
    /// common) to 1.0 (the same set). Two empty sets are the same set.
    ///
    /// # Errors
    ///
    /// [`SlotMismatch`] when the slot counts differ.
    #[allow(clippy::cast_precision_loss)]
    pub fn jaccard(&self, other: &Self) -> Result<f64, SlotMismatch> {
        self.check(other)?;
        let agree = self
            .mins
            .iter()
            .zip(&other.mins)
            .filter(|(mine, theirs)| mine == theirs)
            .count();
        Ok(agree as f64 / self.mins.len() as f64)
    }

    /// The worst-case standard error of [`jaccard`](Self::jaccard):
    /// `1/sqrt(slots)`, reached at a similarity of 0.5.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn standard_error(&self) -> f64 {
        1.0 / (self.mins.len() as f64).sqrt()
    }

    /// Folds `other` in, so this sketch is the sketch of the union of both
    /// sets, exactly as if every key had been inserted here.
    ///
    /// # Errors
    ///
    /// [`SlotMismatch`] when the slot counts differ. Nothing changes.
    pub fn merge(&mut self, other: &Self) -> Result<(), SlotMismatch> {
        self.check(other)?;
        for (mine, &theirs) in self.mins.iter_mut().zip(&other.mins) {
            *mine = (*mine).min(theirs);
        }
        Ok(())
    }

    /// The sketch as bytes: each slot as a 64-bit little-endian word.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.mins.iter().flat_map(|min| min.to_le_bytes()).collect()
    }

    /// A sketch from [`to_bytes`](Self::to_bytes) output. `None` when the
    /// bytes are not a sketch: empty, a partial word, or more than
    /// [`MAX_SLOTS`](Self::MAX_SLOTS) slots.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (chunks, remainder) = bytes.as_chunks::<8>();
        // Checked before decoding, so an oversized payload allocates nothing.
        if !remainder.is_empty() || chunks.is_empty() || chunks.len() > Self::MAX_SLOTS {
            return None;
        }
        Some(Self {
            mins: chunks
                .iter()
                .map(|&chunk| u64::from_le_bytes(chunk))
                .collect(),
        })
    }

    const fn check(&self, other: &Self) -> Result<(), SlotMismatch> {
        if self.mins.len() == other.mins.len() {
            Ok(())
        } else {
            Err(SlotMismatch {
                expected: self.mins.len(),
                found: other.mins.len(),
            })
        }
    }
}

fn stable_hash<T: Hash + ?Sized>(key: &T) -> u64 {
    let mut hasher = StableHasher::default();
    key.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(range: std::ops::Range<u64>, slots: usize) -> MinHash {
        let mut sketch = MinHash::new(slots);
        for key in range {
            sketch.insert(&key);
        }
        sketch
    }

    #[test]
    fn slots_are_clamped() {
        assert_eq!(MinHash::new(0).slots(), 1);
        assert_eq!(MinHash::new(128).slots(), 128);
        assert_eq!(MinHash::new(usize::MAX).slots(), MinHash::MAX_SLOTS);
    }

    #[test]
    fn empty_sketch_is_empty() {
        let mut sketch = MinHash::new(64);
        assert!(sketch.is_empty());
        sketch.insert("a");
        assert!(!sketch.is_empty());
        sketch.clear();
        assert!(sketch.is_empty());
    }

    #[test]
    fn identical_sets_are_fully_similar() {
        let a = set(0..1_000, 128);
        let b = set(0..1_000, 128);
        assert!((a.jaccard(&b).unwrap() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn insertion_order_and_repeats_do_not_matter() {
        let mut a = MinHash::new(64);
        let mut b = MinHash::new(64);
        for key in 0..100_u64 {
            a.insert(&key);
        }
        for key in (0..100_u64).rev().chain(0..50) {
            b.insert(&key);
        }
        assert_eq!(a, b);
    }

    #[test]
    fn disjoint_sets_are_dissimilar() {
        let a = set(0..1_000, 128);
        let b = set(10_000..11_000, 128);
        assert!(a.jaccard(&b).unwrap() < 0.05);
    }

    #[test]
    fn an_empty_set_shares_nothing_with_a_full_one() {
        let a = MinHash::new(64);
        let b = set(0..10, 64);
        assert!(a.jaccard(&b).unwrap().abs() < f64::EPSILON);
    }

    #[test]
    fn half_overlap_estimates_one_third() {
        // |A ∩ B| = 500, |A ∪ B| = 1500.
        let a = set(0..1_000, 512);
        let b = set(500..1_500, 512);
        let estimate = a.jaccard(&b).unwrap();
        let tolerance = 3.0 * a.standard_error();
        assert!(
            (estimate - 1.0 / 3.0).abs() < tolerance,
            "estimate {estimate} outside ±{tolerance}"
        );
    }

    #[test]
    fn standard_error_shrinks_with_slots() {
        assert!((MinHash::new(100).standard_error() - 0.1).abs() < 1e-12);
        assert!(MinHash::new(400).standard_error() < MinHash::new(100).standard_error());
    }

    #[test]
    fn merge_is_the_sketch_of_the_union() {
        let mut left = set(0..600, 128);
        let right = set(400..1_000, 128);
        left.merge(&right).unwrap();
        assert_eq!(left, set(0..1_000, 128));
    }

    #[test]
    fn mismatched_slots_are_refused() {
        let mut a = set(0..10, 64);
        let before = a.clone();
        let b = set(0..10, 128);
        let err = a.merge(&b).unwrap_err();
        assert_eq!(
            err,
            SlotMismatch {
                expected: 64,
                found: 128
            }
        );
        assert_eq!(a, before);
        assert_eq!(a.jaccard(&b).unwrap_err(), err);
        assert!(err.to_string().contains("128"));
    }

    #[test]
    fn insert_hash_matches_insert() {
        let mut a = MinHash::new(32);
        let mut b = MinHash::new(32);
        a.insert("key");
        b.insert_hash(stable_hash("key"));
        assert_eq!(a, b);
    }

    #[test]
    fn bytes_round_trip() {
        let sketch = set(0..100, 64);
        let bytes = sketch.to_bytes();
        assert_eq!(bytes.len(), 64 * 8);
        assert_eq!(MinHash::from_bytes(&bytes), Some(sketch));
    }

    #[test]
    fn bad_bytes_are_refused() {
        assert_eq!(MinHash::from_bytes(&[]), None);
        assert_eq!(MinHash::from_bytes(&[0; 7]), None);
        assert_eq!(MinHash::from_bytes(&[0; 12]), None);
        let too_many = vec![0; (MinHash::MAX_SLOTS + 1) * 8];
        assert_eq!(MinHash::from_bytes(&too_many), None);
    }

    #[test]
    fn near_duplicate_text_scores_high() {
        let original =
            "The quick brown fox jumps over the lazy dog while the cat sleeps by the warm fire";
        let reworded =
            "the quick brown fox JUMPS over the lazy dog, while the cat naps by the warm fire!";
        let unrelated =
            "Quarterly revenue grew eleven percent on strong subscription renewals in Europe";
        let a = MinHash::of_text(original, 3, 256);
        let b = MinHash::of_text(reworded, 3, 256);
        let c = MinHash::of_text(unrelated, 3, 256);
        let near = a.jaccard(&b).unwrap();
        let far = a.jaccard(&c).unwrap();
        assert!(near > 0.5, "near-duplicate scored {near}");
        assert!(far < 0.1, "unrelated text scored {far}");
    }

    #[test]
    fn text_shorter_than_a_shingle_is_still_recorded() {
        let a = MinHash::of_text("hello world", 5, 64);
        assert!(!a.is_empty());
        let b = MinHash::of_text("Hello, WORLD.", 5, 64);
        assert_eq!(a, b);
        assert!(MinHash::of_text("  ...  ", 3, 64).is_empty());
    }

    #[test]
    fn zero_width_shingles_are_one_word() {
        assert_eq!(
            MinHash::of_text("a b c", 0, 32),
            MinHash::of_text("a b c", 1, 32)
        );
    }
}
