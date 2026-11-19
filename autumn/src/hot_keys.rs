//! Bounded-memory top-K: which keys take the most traffic right now?
//!
//! [`HotKeys`] answers "who is hammering us?" — the client IPs, tenants,
//! API tokens or routes that account for the most events — without keeping
//! a counter per key. It runs the Space-Saving algorithm (Metwally, Agrawal
//! and El Abbadi, 2005): a table of at most `capacity` counters. When a new
//! key arrives at a full table, it takes over the slot of the key with the
//! smallest count, inherits that count, and records it as its possible
//! over-count (`error`).
//!
//! That gives three guarantees, whatever the traffic:
//!
//! - Memory is `capacity` entries. A flood of one-off keys cannot grow it.
//! - A count never under-counts: the true count lies in
//!   `[count - error, count]` ([`HotKey::guaranteed`] is the lower end).
//! - Every key whose true share of traffic is over `1 / capacity` is in the
//!   table. [`HotKeys::heavy_hitters`] lists keys over a share you choose,
//!   and says which of them are certainly over it.
//!
//! [`HotKeys::decay`] halves every count, so a tracker decayed on a timer
//! (for example from a scheduled task each minute) reports what is hot
//! *now* rather than what was hot since boot.
//!
//! ```
//! use autumn_web::hot_keys::HotKeys;
//!
//! let hot = HotKeys::new(32);
//! for n in 0..10_000u32 {
//!     // 30% of requests come from one address; the rest are all different.
//!     let ip = if n % 10 < 3 { "203.0.113.9".to_string() } else { format!("198.51.100.{n}") };
//!     hot.record(ip);
//! }
//!
//! let top = hot.top(1);
//! assert_eq!(top[0].key, "203.0.113.9");
//! assert!(top[0].guaranteed() <= 3_000 && 3_000 <= top[0].count);
//!
//! // Keys certainly over 10% of traffic: candidates for a stricter rate limit.
//! let offenders: Vec<_> = hot
//!     .heavy_hitters(100_000)
//!     .into_iter()
//!     .filter(|h| h.certain)
//!     .map(|h| h.key)
//!     .collect();
//! assert_eq!(offenders, vec!["203.0.113.9".to_string()]);
//! ```
//!
//! Recording a key already in the table is one hash lookup. Recording a new
//! key at a full table also scans the table for its minimum, so keep
//! `capacity` in the tens to low thousands. All shares are integer parts per
//! million, the same unit as [`crate::slo`] uses.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

/// One part per million: a `share_ppm` of `PPM` is all traffic.
const PPM: u128 = 1_000_000;

/// A tracked key and its estimated count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotKey<K> {
    /// The key.
    pub key: K,
    /// The estimated count. Never below the true count.
    pub count: u64,
    /// The most `count` can be over the true count.
    pub error: u64,
}

impl<K> HotKey<K> {
    /// The count this key is certain to have reached: `count - error`.
    #[must_use]
    pub const fn guaranteed(&self) -> u64 {
        self.count.saturating_sub(self.error)
    }
}

/// A key over a share of traffic, from [`HotKeys::heavy_hitters`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeavyHitter<K> {
    /// The key.
    pub key: K,
    /// The estimated count. Never below the true count.
    pub count: u64,
    /// The most `count` can be over the true count.
    pub error: u64,
    /// `true` when even the guaranteed count is over the share, so the key
    /// is over it for certain. `false` when only the estimate is.
    pub certain: bool,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    count: u64,
    error: u64,
}

#[derive(Debug)]
struct Table<K> {
    slots: HashMap<K, Slot>,
    total: u64,
}

/// A thread-safe Space-Saving top-K tracker. See the [module docs](self).
///
/// Keep one per thing you count (client IPs, tenants, routes) in shared
/// state, for example behind an `Arc`.
#[derive(Debug)]
pub struct HotKeys<K> {
    capacity: usize,
    table: Mutex<Table<K>>,
}

impl<K: Eq + Hash> HotKeys<K> {
    /// A tracker that keeps at most `capacity` keys. A `capacity` of 0 is
    /// raised to 1.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            table: Mutex::new(Table {
                slots: HashMap::with_capacity(capacity),
                total: 0,
            }),
        }
    }

    /// The most keys this tracker keeps.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Count one event for `key`.
    pub fn record(&self, key: K) {
        self.record_weighted(key, 1);
    }

    /// Count `weight` events for `key`, for example bytes sent or query
    /// time. A `weight` of 0 does nothing. Counts saturate at `u64::MAX`.
    pub fn record_weighted(&self, key: K, weight: u64) {
        if weight == 0 {
            return;
        }
        let mut table = self.lock();
        table.total = table.total.saturating_add(weight);
        if let Some(slot) = table.slots.get_mut(&key) {
            slot.count = slot.count.saturating_add(weight);
            return;
        }
        let mut floor = 0;
        if table.slots.len() >= self.capacity
            && let Some(min) = table.slots.values().map(|slot| slot.count).min()
        {
            // Stops at the first match; the rest of the table stays.
            if let Some((_, slot)) = table.slots.extract_if(|_, slot| slot.count == min).next() {
                floor = slot.count;
            }
        }
        table.slots.insert(
            key,
            Slot {
                count: floor.saturating_add(weight),
                error: floor,
            },
        );
    }

    /// The estimate for `key`, or `None` if it is not tracked.
    #[must_use]
    pub fn estimate(&self, key: &K) -> Option<HotKey<K>>
    where
        K: Clone,
    {
        self.lock()
            .slots
            .get_key_value(key)
            .map(|(k, slot)| HotKey {
                key: k.clone(),
                count: slot.count,
                error: slot.error,
            })
    }

    /// Every event counted, tracked key or not.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.lock().total
    }

    /// How many keys are tracked now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().slots.len()
    }

    /// `true` when no key is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().slots.is_empty()
    }

    /// Halve every count and error, and forget keys whose count reaches 0.
    ///
    /// Call it on a timer to age old traffic out: after `n` calls, an event
    /// weighs `1 / 2^n` of a new one.
    pub fn decay(&self) {
        let mut table = self.lock();
        table.slots.retain(|_, slot| {
            // Halve the bound on each side: the guaranteed count rounds
            // down, so an uncertain count never becomes a certain one.
            let guaranteed = (slot.count - slot.error) / 2;
            slot.count /= 2;
            slot.error = slot.count - guaranteed;
            slot.count > 0
        });
        table.total = table
            .slots
            .values()
            .fold(0, |sum: u64, slot| sum.saturating_add(slot.count));
    }

    /// Forget every key and the total.
    pub fn clear(&self) {
        let mut table = self.lock();
        table.slots.clear();
        table.total = 0;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table<K>> {
        // A panic mid-update leaves at worst one count off, which an
        // estimate already allows for, so a poisoned lock is still usable.
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<K: Eq + Hash + Clone + Ord> HotKeys<K> {
    /// The `n` keys with the highest counts, highest first. Ties go to the
    /// smaller error, then the smaller key.
    #[must_use]
    pub fn top(&self, n: usize) -> Vec<HotKey<K>> {
        let (_, mut all) = self.snapshot();
        all.truncate(n);
        all
    }

    /// Keys whose estimated count is over `share_ppm` parts per million of
    /// [`total`](Self::total), highest first. A key is
    /// [`certain`](HeavyHitter::certain) when its guaranteed count is over
    /// the share too.
    ///
    /// No key is missed when `share_ppm` is at least `1_000_000 / capacity`.
    #[must_use]
    pub fn heavy_hitters(&self, share_ppm: u32) -> Vec<HeavyHitter<K>> {
        let (total, ranked) = self.snapshot();
        let total = u128::from(total);
        let over = |count: u64| u128::from(count) * PPM > total * u128::from(share_ppm);
        ranked
            .into_iter()
            .filter(|hot| over(hot.count))
            .map(|hot| HeavyHitter {
                certain: over(hot.guaranteed()),
                key: hot.key,
                count: hot.count,
                error: hot.error,
            })
            .collect()
    }

    /// The total and every tracked key, ranked, read under one lock so the
    /// two agree. Sorting happens after the lock is released.
    fn snapshot(&self) -> (u64, Vec<HotKey<K>>) {
        let (total, mut all) = {
            let table = self.lock();
            let all: Vec<HotKey<K>> = table
                .slots
                .iter()
                .map(|(k, slot)| HotKey {
                    key: k.clone(),
                    count: slot.count,
                    error: slot.error,
                })
                .collect();
            (table.total, all)
        };
        all.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(a.error.cmp(&b.error))
                .then_with(|| a.key.cmp(&b.key))
        });
        (total, all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys<K: Clone>(hot: &[HotKey<K>]) -> Vec<K> {
        hot.iter().map(|h| h.key.clone()).collect()
    }

    #[test]
    fn counts_exactly_while_under_capacity() {
        let hot = HotKeys::new(4);
        for k in ["a", "b", "a", "c", "a", "b"] {
            hot.record(k);
        }
        let top = hot.top(10);
        assert_eq!(keys(&top), vec!["a", "b", "c"]);
        assert_eq!(top[0].count, 3);
        assert_eq!(top[0].error, 0);
        assert_eq!(top[0].guaranteed(), 3);
        assert_eq!(hot.total(), 6);
        assert_eq!(hot.len(), 3);
    }

    #[test]
    fn top_truncates_to_n() {
        let hot = HotKeys::new(8);
        for (k, n) in [("a", 5), ("b", 4), ("c", 3), ("d", 2)] {
            hot.record_weighted(k, n);
        }
        assert_eq!(keys(&hot.top(2)), vec!["a", "b"]);
    }

    #[test]
    fn ties_break_by_smaller_error_then_key_order() {
        let hot = HotKeys::new(4);
        hot.record("b");
        hot.record("a");
        assert_eq!(keys(&hot.top(2)), vec!["a", "b"]);
    }

    #[test]
    fn eviction_replaces_the_minimum_and_carries_its_count_as_error() {
        let hot = HotKeys::new(2);
        hot.record_weighted("a", 10);
        hot.record_weighted("b", 3);
        hot.record("c"); // evicts "b" (3): c = 3 + 1, error 3
        assert_eq!(hot.len(), 2);
        assert!(hot.estimate(&"b").is_none());
        let c = hot.estimate(&"c").unwrap();
        assert_eq!((c.count, c.error, c.guaranteed()), (4, 3, 1));
        assert_eq!(hot.total(), 14);
    }

    #[test]
    fn a_true_heavy_hitter_survives_a_flood_of_one_off_keys() {
        // 30% of 10_000 events go to "attacker"; the rest are unique.
        let hot = HotKeys::new(16);
        let mut i = 0u32;
        for n in 0..10_000u32 {
            if n % 10 < 3 {
                hot.record("attacker".to_string());
            } else {
                i += 1;
                hot.record(format!("user-{i}"));
            }
        }
        let top = hot.top(1);
        assert_eq!(top[0].key, "attacker");
        // Space-Saving never under-counts, and the error is at most N / k.
        assert!(top[0].count >= 3_000);
        assert!(top[0].error <= 10_000 / 16);
        assert!(top[0].guaranteed() <= 3_000);
    }

    #[test]
    fn heavy_hitters_returns_keys_over_a_share_of_traffic() {
        let hot = HotKeys::new(8);
        hot.record_weighted("big", 60);
        hot.record_weighted("mid", 30);
        hot.record_weighted("small", 10);
        // Over 25% of 100 events.
        let hh = hot.heavy_hitters(250_000);
        assert_eq!(
            hh.iter().map(|h| h.key).collect::<Vec<_>>(),
            vec!["big", "mid"]
        );
        assert!(hh.iter().all(|h| h.certain));
        assert!(hot.heavy_hitters(1_000_000).is_empty());
    }

    #[test]
    fn heavy_hitters_marks_keys_that_only_might_be_over() {
        let hot = HotKeys::new(1);
        hot.record_weighted("a", 5);
        hot.record_weighted("b", 5); // count 10, error 5
        let hh = hot.heavy_hitters(600_000); // over 6 of 10
        assert_eq!(hh.iter().map(|h| h.key).collect::<Vec<_>>(), vec!["b"]);
        assert!(!hh[0].certain);
    }

    #[test]
    fn heavy_hitters_on_an_empty_tracker_is_empty() {
        let hot: HotKeys<&str> = HotKeys::new(4);
        assert!(hot.heavy_hitters(0).is_empty());
        assert!(hot.top(3).is_empty());
        assert!(hot.is_empty());
    }

    #[test]
    fn decay_halves_counts_and_drops_cold_keys() {
        let hot = HotKeys::new(4);
        hot.record_weighted("hot", 8);
        hot.record("cold");
        hot.decay();
        assert_eq!(hot.estimate(&"hot").unwrap().count, 4);
        assert!(hot.estimate(&"cold").is_none());
        assert_eq!(hot.total(), 4);
    }

    #[test]
    fn decay_lets_a_new_hot_key_overtake_an_old_one() {
        let hot = HotKeys::new(2);
        hot.record_weighted("yesterday", 1_000);
        for _ in 0..4 {
            hot.decay();
        }
        hot.record_weighted("today", 100);
        assert_eq!(keys(&hot.top(1)), vec!["today"]);
    }

    #[test]
    fn decay_never_raises_the_guaranteed_count_above_the_decayed_true_count() {
        // "a" (3) is evicted by "b" (1): b = (4, error 3), true weight 1.
        let hot = HotKeys::new(1);
        hot.record_weighted("a", 3);
        hot.record("b");
        hot.decay();
        // Decayed, b's true weight is 0.5, so nothing is guaranteed.
        let b = hot.estimate(&"b").unwrap();
        assert_eq!((b.count, b.guaranteed()), (2, 0));
        let hh = hot.heavy_hitters(400_000);
        assert_eq!(hh.iter().map(|h| h.key).collect::<Vec<_>>(), vec!["b"]);
        assert!(!hh[0].certain);
    }

    #[test]
    fn heavy_hitters_reads_total_and_counts_from_one_snapshot() {
        let hot = HotKeys::new(4);
        let (total, ranked) = hot.snapshot();
        assert_eq!((total, ranked.len()), (0, 0));
        hot.record_weighted("a", 3);
        let (total, ranked) = hot.snapshot();
        assert_eq!(total, 3);
        assert_eq!(ranked[0].key, "a");
    }

    #[test]
    fn zero_weight_is_a_no_op() {
        let hot = HotKeys::new(2);
        hot.record_weighted("a", 0);
        assert!(hot.is_empty());
        assert_eq!(hot.total(), 0);
    }

    #[test]
    fn capacity_zero_is_raised_to_one() {
        let hot = HotKeys::new(0);
        assert_eq!(hot.capacity(), 1);
        hot.record("a");
        assert_eq!(hot.len(), 1);
    }

    #[test]
    fn clear_forgets_everything() {
        let hot = HotKeys::new(2);
        hot.record("a");
        hot.clear();
        assert!(hot.is_empty());
        assert_eq!(hot.total(), 0);
    }

    #[test]
    fn counts_saturate_instead_of_overflowing() {
        let hot = HotKeys::new(2);
        hot.record_weighted("a", u64::MAX);
        hot.record("a");
        assert_eq!(hot.estimate(&"a").unwrap().count, u64::MAX);
        assert_eq!(hot.total(), u64::MAX);
    }

    #[test]
    fn is_shareable_across_threads() {
        let hot = std::sync::Arc::new(HotKeys::new(8));
        std::thread::scope(|s| {
            for t in 0..4 {
                let hot = &hot;
                s.spawn(move || {
                    for _ in 0..250 {
                        hot.record(t % 2);
                    }
                });
            }
        });
        assert_eq!(hot.total(), 1_000);
        assert_eq!(hot.estimate(&0).unwrap().count, 500);
    }
}
