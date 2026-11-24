//! Bounded-memory samples: a fair handful of a stream too long to keep.
//!
//! "Show me some of the requests that hit this endpoint today", "keep a few
//! example payloads that failed validation", "which tenants are in this
//! cohort, roughly?" Counting answers *how many*; a sample answers *which
//! ones look like what*. Keeping every item grows with the traffic. Keeping
//! the first `k`, or the last `k`, is biased toward the start or the end of
//! the window. [`Reservoir`] keeps `k` items, and every item the stream
//! offered had the same `k / n` chance of being one of them, however long
//! the stream ran.
//!
//! It is reservoir sampling (Vitter's Algorithm R, 1985): the first `k`
//! items fill the reservoir, and the `n`th item after that replaces a random
//! slot with probability `k / n`.
//!
//! - Memory is the `k` items and three integers, whatever `n` is.
//! - [`offer`](Reservoir::offer) says whether the item was kept, so a caller
//!   can capture something expensive (a full trace, a request body) only for
//!   the items that made it in.
//! - Two reservoirs of the same capacity [`merge`](Reservoir::merge) into a
//!   uniform sample of both streams together. The merge weighs each side by
//!   how many items it *saw*, not how many it holds, so a quiet replica's
//!   items are not over-represented next to a busy one's. Offering more
//!   items after a merge keeps the sample uniform over everything.
//! - [`Reservoir::with_seed`] replays a stream to the same sample, for tests
//!   and simulations. [`Reservoir::new`] seeds itself from the standard
//!   library's per-process randomness, so a client cannot predict which of
//!   its requests will be sampled. The generator is not cryptographic.
//!
//! [`Sampler`] is the shared form: a [`Reservoir`] behind a mutex, to put in
//! an `Arc` and offer to from every handler.
//!
//! ```rust
//! use autumn_web::reservoir::Reservoir;
//!
//! let mut slow_requests = Reservoir::with_seed(3, 42);
//! for path in ["/a", "/b", "/c", "/d", "/e", "/f"] {
//!     slow_requests.offer(path);
//! }
//! assert_eq!(slow_requests.len(), 3);
//! assert_eq!(slow_requests.seen(), 6);
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

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;
use std::sync::{Mutex, PoisonError};

/// Two reservoirs of different capacities cannot be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityMismatch {
    /// The capacity of the reservoir merged into.
    pub expected: usize,
    /// The capacity of the reservoir offered.
    pub found: usize,
}

impl fmt::Display for CapacityMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot merge a capacity-{} reservoir into a capacity-{} reservoir",
            self.found, self.expected
        )
    }
}

impl std::error::Error for CapacityMismatch {}

/// A fixed-size uniform sample of a stream. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct Reservoir<T> {
    capacity: usize,
    seen: u64,
    items: Vec<T>,
    rng: SplitMix64,
}

impl<T> Reservoir<T> {
    /// An empty reservoir of `capacity` items (at least 1), seeded from the
    /// standard library's per-process randomness.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_seed(capacity, RandomState::new().hash_one(0_u8))
    }

    /// An empty reservoir of `capacity` items (at least 1) whose choices
    /// follow from `seed`: the same stream offered to the same seed keeps
    /// the same sample.
    #[must_use]
    pub fn with_seed(capacity: usize, seed: u64) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            seen: 0,
            // A large capacity is reserved as it fills, not up front.
            items: Vec::with_capacity(capacity.min(1024)),
            rng: SplitMix64(seed),
        }
    }

    /// The most items the reservoir holds.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many items have been offered (or merged in) since the reservoir
    /// was made or cleared.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.seen
    }

    /// How many items the reservoir holds: `min(seen, capacity)`.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing has been offered.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The sample, in no meaningful order.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// The sample, in no meaningful order.
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }

    /// Offers `item` to the sample. Returns whether it was kept; a kept
    /// item may still be replaced by a later one.
    pub fn offer(&mut self, item: T) -> bool {
        self.seen = self.seen.saturating_add(1);
        if self.items.len() < self.capacity {
            self.items.push(item);
            return true;
        }
        let slot = usize::try_from(self.rng.below(self.seen)).unwrap_or(usize::MAX);
        self.items.get_mut(slot).is_some_and(|kept| {
            *kept = item;
            true
        })
    }

    /// Folds `other` in, so this reservoir is a uniform sample of both
    /// streams together, as if every item had been offered to it.
    ///
    /// # Errors
    ///
    /// [`CapacityMismatch`] when the capacities differ; nothing changes.
    pub fn merge(&mut self, other: &Self) -> Result<(), CapacityMismatch>
    where
        T: Clone,
    {
        if self.capacity != other.capacity {
            return Err(CapacityMismatch {
                expected: self.capacity,
                found: other.capacity,
            });
        }
        // A uniform `k`-subset of both streams takes a hypergeometric number
        // of its items from each: draw `k` times without replacement from
        // the two populations, by the items each side saw.
        let take = self
            .capacity
            .min(self.items.len().saturating_add(other.items.len()));
        let (mut left, mut right) = (self.seen, other.seen);
        let (mut from_self, mut from_other) = (0_usize, 0_usize);
        for _ in 0..take {
            if self.rng.below(left.saturating_add(right)) < left {
                from_self = from_self.saturating_add(1);
                left = left.saturating_sub(1);
            } else {
                from_other = from_other.saturating_add(1);
                right = right.saturating_sub(1);
            }
        }
        // Each side's sample is uniform over its own stream, so a uniform
        // subset of that sample is too.
        keep_random(&mut self.items, from_self, &mut self.rng);
        let mut theirs = other.items.clone();
        keep_random(&mut theirs, from_other, &mut self.rng);
        self.items.append(&mut theirs);
        self.seen = self.seen.saturating_add(other.seen);
        Ok(())
    }

    /// Empties the reservoir and resets [`seen`](Self::seen).
    pub fn clear(&mut self) {
        self.items.clear();
        self.seen = 0;
    }
}

/// Keeps a uniformly random `count` of `items` (a partial Fisher–Yates
/// shuffle), dropping the rest.
fn keep_random<T>(items: &mut Vec<T>, count: usize, rng: &mut SplitMix64) {
    let count = count.min(items.len());
    for i in 0..count {
        let remaining = u64::try_from(items.len().saturating_sub(i)).unwrap_or(u64::MAX);
        let offset = usize::try_from(rng.below(remaining)).unwrap_or(0);
        items.swap(i, i.saturating_add(offset));
    }
    items.truncate(count);
}

/// Steele, Lea and Flood's `SplitMix64`: small, fast, and enough for
/// sampling. Not for secrets.
#[derive(Debug, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ z.wrapping_shr(31)
    }

    /// A uniform value in `0..bound` (0 when `bound` is 0), by Lemire's
    /// multiply-and-reject, so no value is favoured.
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let threshold = bound.wrapping_neg().checked_rem(bound).unwrap_or(0);
        loop {
            let product = u128::from(self.next()).wrapping_mul(u128::from(bound));
            // The low half decides rejection, the high half is the value.
            #[allow(clippy::cast_possible_truncation)]
            let low = product as u64;
            if low >= threshold {
                #[allow(clippy::cast_possible_truncation)]
                return product.wrapping_shr(64) as u64;
            }
        }
    }
}

/// A [`Reservoir`] to share across handlers and threads: put it in an `Arc`
/// and [`offer`](Self::offer) from anywhere.
///
/// A poisoned lock is recovered: every step of an offer or a merge leaves
/// the items a valid sample of what was seen.
#[derive(Debug)]
pub struct Sampler<T> {
    reservoir: Mutex<Reservoir<T>>,
}

impl<T> Sampler<T> {
    /// An empty shared reservoir. See [`Reservoir::new`].
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::from_reservoir(Reservoir::new(capacity))
    }

    /// An empty shared reservoir with a fixed seed. See
    /// [`Reservoir::with_seed`].
    #[must_use]
    pub fn with_seed(capacity: usize, seed: u64) -> Self {
        Self::from_reservoir(Reservoir::with_seed(capacity, seed))
    }

    const fn from_reservoir(reservoir: Reservoir<T>) -> Self {
        Self {
            reservoir: Mutex::new(reservoir),
        }
    }

    /// See [`Reservoir::offer`].
    pub fn offer(&self, item: T) -> bool {
        self.with(|reservoir| reservoir.offer(item))
    }

    /// See [`Reservoir::seen`].
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.with(|reservoir| reservoir.seen())
    }

    /// See [`Reservoir::merge`].
    ///
    /// # Errors
    ///
    /// [`CapacityMismatch`] when the capacities differ.
    pub fn merge(&self, other: &Reservoir<T>) -> Result<(), CapacityMismatch>
    where
        T: Clone,
    {
        self.with(|reservoir| reservoir.merge(other))
    }

    /// A copy of the current sample.
    #[must_use]
    pub fn snapshot(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.with(|reservoir| reservoir.items.clone())
    }

    /// Hands back the reservoir and starts an empty one of the same
    /// capacity, in one step: no item falls between the two windows.
    #[must_use]
    pub fn take(&self) -> Reservoir<T> {
        self.with(|reservoir| {
            let fresh = Reservoir::with_seed(reservoir.capacity, reservoir.rng.next());
            std::mem::replace(reservoir, fresh)
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut Reservoir<T>) -> R) -> R {
        let mut reservoir = self
            .reservoir
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f(&mut reservoir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(reservoir: &mut Reservoir<u32>, items: std::ops::Range<u32>) {
        for item in items {
            reservoir.offer(item);
        }
    }

    /// How often each of `0..population` lands in the sample, over `trials`
    /// seeded runs of `build`.
    fn inclusion_counts(
        population: u32,
        trials: u64,
        build: impl Fn(u64) -> Reservoir<u32>,
    ) -> Vec<u32> {
        let mut counts = vec![0_u32; population as usize];
        for seed in 0..trials {
            for &item in build(seed).as_slice() {
                counts[item as usize] += 1;
            }
        }
        counts
    }

    fn assert_uniform(counts: &[u32], expected: f64) {
        for (item, &count) in counts.iter().enumerate() {
            let ratio = f64::from(count) / expected;
            assert!(
                (0.85..1.15).contains(&ratio),
                "item {item} sampled {count} times, expected about {expected}"
            );
        }
    }

    #[test]
    fn keeps_everything_until_full() {
        let mut reservoir = Reservoir::with_seed(5, 1);
        for item in 0..3 {
            assert!(reservoir.offer(item));
        }
        assert_eq!(reservoir.as_slice(), &[0, 1, 2]);
        assert_eq!(reservoir.seen(), 3);
        assert_eq!(reservoir.len(), 3);
        assert!(!reservoir.is_empty());
    }

    #[test]
    fn never_holds_more_than_capacity() {
        let mut reservoir = Reservoir::with_seed(4, 7);
        fill(&mut reservoir, 0..10_000);
        assert_eq!(reservoir.len(), 4);
        assert_eq!(reservoir.capacity(), 4);
        assert_eq!(reservoir.seen(), 10_000);
    }

    #[test]
    fn capacity_zero_is_clamped_to_one() {
        let mut reservoir = Reservoir::with_seed(0, 1);
        assert_eq!(reservoir.capacity(), 1);
        fill(&mut reservoir, 0..10);
        assert_eq!(reservoir.len(), 1);
    }

    #[test]
    fn offer_reports_whether_the_item_was_kept() {
        let mut reservoir = Reservoir::with_seed(2, 3);
        let mut kept = 0;
        for item in 0..1_000 {
            if reservoir.offer(item) {
                kept += 1;
                assert!(reservoir.as_slice().contains(&item));
            } else {
                assert!(!reservoir.as_slice().contains(&item));
            }
        }
        // About k (1 + ln(n / k)) items are ever kept: 2 * (1 + ln 500) ~ 14.
        assert!((2..60).contains(&kept), "kept {kept}");
    }

    #[test]
    fn same_seed_same_sample() {
        let run = |seed| {
            let mut reservoir = Reservoir::with_seed(8, seed);
            fill(&mut reservoir, 0..5_000);
            reservoir.into_vec()
        };
        assert_eq!(run(42), run(42));
        assert_ne!(run(42), run(43));
    }

    #[test]
    fn every_item_is_equally_likely() {
        // 10 of 100 items: each lands in the sample 10 % of the time.
        let counts = inclusion_counts(100, 20_000, |seed| {
            let mut reservoir = Reservoir::with_seed(10, seed);
            fill(&mut reservoir, 0..100);
            reservoir
        });
        assert_uniform(&counts, 2_000.0);
    }

    #[test]
    fn merge_of_unequal_streams_stays_uniform() {
        // Replica A saw 90 items, replica B saw 10. Taking half the merged
        // sample from each would sample B's items nine times too often.
        let counts = inclusion_counts(100, 20_000, |seed| {
            let mut a = Reservoir::with_seed(10, seed);
            fill(&mut a, 0..90);
            let mut b = Reservoir::with_seed(10, seed.wrapping_add(1 << 32));
            fill(&mut b, 90..100);
            a.merge(&b).unwrap();
            assert_eq!(a.len(), 10);
            assert_eq!(a.seen(), 100);
            a
        });
        assert_uniform(&counts, 2_000.0);
    }

    #[test]
    fn merge_then_keep_offering_stays_uniform() {
        let counts = inclusion_counts(60, 20_000, |seed| {
            let mut a = Reservoir::with_seed(6, seed);
            fill(&mut a, 0..20);
            let mut b = Reservoir::with_seed(6, !seed);
            fill(&mut b, 20..30);
            a.merge(&b).unwrap();
            fill(&mut a, 30..60);
            a
        });
        assert_uniform(&counts, 2_000.0);
    }

    #[test]
    fn merge_of_small_streams_keeps_both() {
        let mut a = Reservoir::with_seed(10, 1);
        fill(&mut a, 0..3);
        let mut b = Reservoir::with_seed(10, 2);
        fill(&mut b, 3..7);
        a.merge(&b).unwrap();
        let mut merged = a.into_vec();
        merged.sort_unstable();
        assert_eq!(merged, vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn merge_with_empty_changes_nothing() {
        let mut a = Reservoir::with_seed(4, 1);
        fill(&mut a, 0..100);
        let before = a.as_slice().to_vec();
        a.merge(&Reservoir::with_seed(4, 9)).unwrap();
        let mut after = a.as_slice().to_vec();
        let mut expected = before;
        after.sort_unstable();
        expected.sort_unstable();
        assert_eq!(after, expected);
        assert_eq!(a.seen(), 100);
    }

    #[test]
    fn merge_refuses_a_different_capacity() {
        let mut a = Reservoir::<u32>::with_seed(4, 1);
        let b = Reservoir::with_seed(5, 1);
        let err = a.merge(&b).unwrap_err();
        assert_eq!(
            err,
            CapacityMismatch {
                expected: 4,
                found: 5
            }
        );
        assert_eq!(
            err.to_string(),
            "cannot merge a capacity-5 reservoir into a capacity-4 reservoir"
        );
    }

    #[test]
    fn clear_starts_over() {
        let mut reservoir = Reservoir::with_seed(3, 1);
        fill(&mut reservoir, 0..10);
        reservoir.clear();
        assert!(reservoir.is_empty());
        assert_eq!(reservoir.seen(), 0);
        assert!(reservoir.offer(99));
        assert_eq!(reservoir.as_slice(), &[99]);
    }

    #[test]
    fn unseeded_reservoirs_differ() {
        let run = || {
            let mut reservoir = Reservoir::new(8);
            fill(&mut reservoir, 0..10_000);
            reservoir.into_vec()
        };
        assert_ne!(run(), run());
    }

    #[test]
    fn below_is_in_range() {
        let mut rng = SplitMix64(5);
        for bound in [1_u64, 2, 3, 7, 1 << 40, u64::MAX] {
            for _ in 0..200 {
                assert!(rng.below(bound) < bound);
            }
        }
    }

    #[test]
    fn sampler_shares_one_reservoir() {
        let sampler = std::sync::Arc::new(Sampler::with_seed(3, 11));
        std::thread::scope(|scope| {
            for worker in 0..4_u32 {
                let sampler = &sampler;
                scope.spawn(move || {
                    for item in 0..250 {
                        sampler.offer(worker * 1_000 + item);
                    }
                });
            }
        });
        assert_eq!(sampler.seen(), 1_000);
        assert_eq!(sampler.snapshot().len(), 3);
    }

    #[test]
    fn sampler_take_starts_a_fresh_window() {
        let sampler = Sampler::with_seed(2, 1);
        for item in 0..10 {
            sampler.offer(item);
        }
        let window = sampler.take();
        assert_eq!(window.seen(), 10);
        assert_eq!(window.len(), 2);
        assert_eq!(sampler.seen(), 0);
        assert!(sampler.snapshot().is_empty());
        assert!(sampler.offer(7));
        assert_eq!(sampler.snapshot(), vec![7]);

        let mut fleet = Reservoir::with_seed(2, 5);
        fleet.merge(&window).unwrap();
        sampler.merge(&fleet).unwrap();
        assert_eq!(sampler.seen(), 11);
    }
}
