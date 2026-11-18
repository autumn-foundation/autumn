//! Bounded-memory top-k counting: which keys are hot right now?
//!
//! A rate limiter knows when *a* client is over its limit, metrics know the
//! total request rate, and the cost meter knows what the whole app spent. None
//! of them answers "who is most of the traffic?" without one counter per key,
//! which grows with every IP, tenant or route an attacker can invent.
//!
//! [`HotKeys`] answers it in fixed memory with the Space-Saving algorithm
//! (Metwally, Agrawal and El Abbadi, 2005). It keeps at most `capacity`
//! counters. A new key, once the table is full, takes over the smallest
//! counter and inherits its count as an error bound. So each reported count
//! overestimates the truth by at most [`HotKey::error`], and any key whose
//! true share of the traffic is above `1 / capacity` is always in the table.
//!
//! ```
//! use autumn_web::hot_keys::HotKeys;
//!
//! let mut hot = HotKeys::new(4);
//! for i in 0..1_000u32 {
//!     // One tenant sends half of all requests; the rest are all distinct.
//!     let tenant = if i % 2 == 0 { "acme".to_owned() } else { format!("t{i}") };
//!     hot.record(tenant);
//! }
//! let top = hot.top(1);
//! assert_eq!(top[0].key, "acme");
//! assert!(top[0].guaranteed() >= 500);
//! assert_eq!(hot.len(), 4); // memory never grew past capacity
//! ```
//!
//! [`HotKeys::decay`] halves every counter, so calling it on a timer turns
//! all-time counts into an exponentially weighted "recently hot" ranking.
//! The type is not synchronised; share it behind a `Mutex`.

use std::collections::HashMap;
use std::hash::Hash;

/// One tracked key and its estimated count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotKey<K> {
    /// The key.
    pub key: K,
    /// Estimated weight recorded for the key. Never below the true weight.
    pub count: u64,
    /// The most `count` can overstate the true weight by.
    pub error: u64,
}

impl<K> HotKey<K> {
    /// Weight the key is certain to have received: `count - error`.
    #[must_use]
    pub const fn guaranteed(&self) -> u64 {
        self.count.saturating_sub(self.error)
    }
}

/// A Space-Saving top-k counter over at most `capacity` keys.
#[derive(Debug, Clone)]
pub struct HotKeys<K> {
    capacity: usize,
    slots: Vec<HotKey<K>>,
    index: HashMap<K, usize>,
    total: u64,
}

impl<K: Eq + Hash + Clone> HotKeys<K> {
    /// A counter that tracks at most `capacity` keys (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            slots: Vec::with_capacity(capacity),
            index: HashMap::with_capacity(capacity),
            total: 0,
        }
    }

    /// Record one event for `key`.
    pub fn record(&mut self, key: K) {
        self.record_weighted(key, 1);
    }

    /// Record `weight` for `key`, for example bytes sent or cost units.
    ///
    /// A miss on a full table scans for the smallest counter, so a miss costs
    /// `O(capacity)`. Keep `capacity` in the hundreds, not the millions.
    pub fn record_weighted(&mut self, key: K, weight: u64) {
        if weight == 0 {
            return;
        }
        self.total = self.total.saturating_add(weight);
        if let Some(&i) = self.index.get(&key) {
            let slot = &mut self.slots[i];
            slot.count = slot.count.saturating_add(weight);
            return;
        }
        if self.slots.len() < self.capacity {
            self.index.insert(key.clone(), self.slots.len());
            self.slots.push(HotKey {
                key,
                count: weight,
                error: 0,
            });
            return;
        }
        let i = self
            .slots
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| s.count)
            .map_or(0, |(i, _)| i);
        let slot = &mut self.slots[i];
        self.index.remove(&slot.key);
        self.index.insert(key.clone(), i);
        let floor = slot.count;
        *slot = HotKey {
            key,
            count: floor.saturating_add(weight),
            error: floor,
        };
    }

    /// The `n` heaviest keys, heaviest first. Ties keep a stable order by
    /// smaller error first.
    #[must_use]
    pub fn top(&self, n: usize) -> Vec<HotKey<K>> {
        let mut out = self.slots.clone();
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.error.cmp(&b.error)));
        out.truncate(n);
        out
    }

    /// Keys whose guaranteed weight is at least `ppm` parts per million of
    /// everything recorded. No false positives; a key just over the line can
    /// be missed while its error is large.
    #[must_use]
    pub fn heavy_hitters(&self, ppm: u64) -> Vec<HotKey<K>> {
        let line = u128::from(ppm) * u128::from(self.total);
        let mut out = self.top(self.slots.len());
        out.retain(|k| u128::from(k.guaranteed()) * 1_000_000 >= line);
        out
    }

    /// The estimate for one key, if it is tracked.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&HotKey<K>> {
        self.index.get(key).map(|&i| &self.slots[i])
    }

    /// Halve every count, error and the total, and forget keys that reach
    /// zero. Call it on a timer to age out old traffic.
    pub fn decay(&mut self) {
        self.total /= 2;
        for slot in &mut self.slots {
            slot.count /= 2;
            slot.error /= 2;
        }
        self.slots.retain(|s| s.count > 0);
        self.index.clear();
        for (i, s) in self.slots.iter().enumerate() {
            self.index.insert(s.key.clone(), i);
        }
    }

    /// Total weight recorded (after any decay).
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Number of keys tracked now.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether nothing is tracked.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The most keys this counter tracks.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_while_under_capacity() {
        let mut hot = HotKeys::new(3);
        for k in ["a", "b", "a", "c", "a", "b"] {
            hot.record(k);
        }
        let top = hot.top(3);
        assert_eq!(
            top,
            vec![
                HotKey {
                    key: "a",
                    count: 3,
                    error: 0
                },
                HotKey {
                    key: "b",
                    count: 2,
                    error: 0
                },
                HotKey {
                    key: "c",
                    count: 1,
                    error: 0
                },
            ]
        );
        assert_eq!(hot.total(), 6);
    }

    #[test]
    fn full_table_replaces_the_minimum_and_inherits_its_count() {
        let mut hot = HotKeys::new(2);
        hot.record_weighted("a", 5);
        hot.record_weighted("b", 2);
        hot.record("c");
        assert_eq!(hot.len(), 2);
        assert!(hot.get(&"b").is_none());
        assert_eq!(
            hot.get(&"c"),
            Some(&HotKey {
                key: "c",
                count: 3,
                error: 2
            })
        );
        assert_eq!(hot.get(&"a").map(HotKey::guaranteed), Some(5));
    }

    #[test]
    fn counts_never_underestimate_and_errors_bound_the_overcount() {
        let mut hot = HotKeys::new(8);
        let mut truth: HashMap<u32, u64> = HashMap::new();
        // A deterministic skewed stream: key k appears roughly 1/(k+1) of the time.
        let mut x: u32 = 7;
        for _ in 0..20_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let key = (x >> 16) % 64;
            let key = key % (key % 7 + 1);
            hot.record(key);
            *truth.entry(key).or_default() += 1;
        }
        for k in hot.top(8) {
            let real = truth[&k.key];
            assert!(k.count >= real, "{k:?} underestimates {real}");
            assert!(k.guaranteed() <= real, "{k:?} overstates {real}");
        }
    }

    #[test]
    fn a_key_over_one_over_capacity_is_always_kept() {
        let mut hot = HotKeys::new(10);
        for i in 0..10_000u64 {
            // "hot" is 20% of the stream, above the 10% guarantee.
            let key = if i % 5 == 0 { u64::MAX } else { i };
            hot.record(key);
        }
        let hit = hot.get(&u64::MAX).expect("heavy key evicted");
        assert!(hit.count >= 2_000);
        assert_eq!(hot.top(1)[0].key, u64::MAX);
        assert_eq!(hot.len(), 10);
    }

    #[test]
    fn heavy_hitters_uses_the_guaranteed_count() {
        let mut hot = HotKeys::new(2);
        hot.record_weighted("a", 60);
        hot.record_weighted("b", 30);
        hot.record_weighted("c", 10); // evicts b: count 40, error 30
        let hits = hot.heavy_hitters(300_000); // 30% of 100
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "a");
        assert_eq!(hot.heavy_hitters(0).len(), 2);
    }

    #[test]
    fn decay_halves_and_forgets_zeroes() {
        let mut hot = HotKeys::new(4);
        hot.record_weighted("a", 9);
        hot.record("b");
        hot.decay();
        assert_eq!(
            hot.get(&"a"),
            Some(&HotKey {
                key: "a",
                count: 4,
                error: 0
            })
        );
        assert!(hot.get(&"b").is_none());
        assert_eq!(hot.len(), 1);
        assert_eq!(hot.total(), 5);
        // The freed slot is reusable without eviction.
        hot.record("z");
        assert_eq!(hot.get(&"z").map(|k| k.error), Some(0));
    }

    #[test]
    fn zero_capacity_still_tracks_one_key_and_zero_weight_is_ignored() {
        let mut hot = HotKeys::new(0);
        assert_eq!(hot.capacity(), 1);
        hot.record_weighted("a", 0);
        assert!(hot.is_empty());
        hot.record("a");
        hot.record("b");
        assert_eq!(
            hot.top(5),
            vec![HotKey {
                key: "b",
                count: 2,
                error: 1
            }]
        );
    }

    #[test]
    fn weights_saturate_instead_of_overflowing() {
        let mut hot = HotKeys::new(1);
        hot.record_weighted("a", u64::MAX);
        hot.record_weighted("a", 5);
        assert_eq!(hot.get(&"a").map(|k| k.count), Some(u64::MAX));
        assert_eq!(hot.total(), u64::MAX);
        assert_eq!(hot.heavy_hitters(1_000_000).len(), 1);
    }
}
