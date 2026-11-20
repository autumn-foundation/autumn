//! Bounded-memory heavy hitters: the top keys of an endless stream.
//!
//! "Which tenant sends the most requests?", "which route burns the most
//! bytes?", "which client IP is hammering the login form?" An exact answer
//! needs one counter per key ever seen, and a stream of client IPs never stops
//! growing. [`SpaceSaving`] answers the same questions in a fixed number of
//! counters, with error bounds you can read off each answer.
//!
//! It is the Space-Saving algorithm (Metwally, Agrawal and El Abbadi, 2005).
//! The table holds `capacity` counters. A key already in the table adds to its
//! counter. A new key, when the table is full, takes over the smallest
//! counter: it inherits that count as its `error`, then adds its own weight.
//!
//! Each [`Hitter`] then carries two numbers:
//!
//! - `count` is never below the key's true total.
//! - `count - error` ([`Hitter::guaranteed`]) is never above it.
//!
//! And one promise holds for the whole table: **every key whose true total is
//! above `total / capacity` is in it.** So a table of 100 counters cannot miss
//! a key that carries more than 1 % of the stream.
//!
//! [`SpaceSaving::frequent`] uses the guaranteed count, so it never reports a
//! key that is not really over the share you ask for. Ties break by slot
//! order, never by hash order, so the same stream gives the same table on
//! every run (simulation tests replay byte-identically).
//!
//! [`HeavyHitters`] is the shared form: a [`SpaceSaving`] behind a mutex, to
//! put in an `Arc` and record from every handler.
//!
//! ```rust
//! use autumn_web::heavy_hitters::HeavyHitters;
//!
//! let hot = HeavyHitters::new(64);
//! for tenant in ["acme", "acme", "globex", "acme", "initech"] {
//!     hot.observe(tenant);
//! }
//! let top = hot.top(1);
//! assert_eq!(top[0].key, "acme");
//! assert_eq!(top[0].count, 3);
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

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, PoisonError};

/// One key of the table and its bounds. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hitter<K> {
    /// The key.
    pub key: K,
    /// An upper bound on the key's true total.
    pub count: u64,
    /// How much of `count` the key may have inherited from evicted keys.
    pub error: u64,
}

impl<K> Hitter<K> {
    /// A lower bound on the key's true total: `count - error`.
    #[must_use]
    pub const fn guaranteed(&self) -> u64 {
        self.count.saturating_sub(self.error)
    }
}

#[derive(Debug, Clone)]
struct Slot<K> {
    key: K,
    count: u64,
    error: u64,
}

/// A fixed-size Space-Saving table. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct SpaceSaving<K> {
    capacity: usize,
    slots: Vec<Slot<K>>,
    index: HashMap<K, usize>,
    total: u64,
}

impl<K: Eq + Hash + Clone> SpaceSaving<K> {
    /// An empty table of `capacity` counters. A capacity of `0` is taken as `1`.
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

    /// The number of counters.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The sum of every weight observed. Saturates at `u64::MAX`.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Counts one occurrence of `key`.
    pub fn observe(&mut self, key: K) {
        self.observe_n(key, 1);
    }

    /// Counts `weight` occurrences of `key` (for example a response's bytes).
    /// A weight of `0` changes nothing.
    pub fn observe_n(&mut self, key: K, weight: u64) {
        if weight == 0 {
            return;
        }
        self.total = self.total.saturating_add(weight);

        if let Some(slot) = self.index.get(&key).and_then(|&at| self.slots.get_mut(at)) {
            slot.count = slot.count.saturating_add(weight);
            return;
        }

        if self.slots.len() < self.capacity {
            self.index.insert(key.clone(), self.slots.len());
            self.slots.push(Slot {
                key,
                count: weight,
                error: 0,
            });
            return;
        }

        // The first slot with the smallest count, so ties break by slot order.
        let Some((at, _)) = self
            .slots
            .iter()
            .enumerate()
            .min_by_key(|(_, slot)| slot.count)
        else {
            return;
        };
        let Some(slot) = self.slots.get_mut(at) else {
            return;
        };
        let evicted = std::mem::replace(&mut slot.key, key.clone());
        slot.error = slot.count;
        slot.count = slot.count.saturating_add(weight);
        self.index.remove(&evicted);
        self.index.insert(key, at);
    }

    /// The bounds for `key`, or `None` when the table does not hold it.
    ///
    /// `None` means the key's true total is at most the smallest count in a
    /// full table (or `0` when the table is not full).
    #[must_use]
    pub fn estimate(&self, key: &K) -> Option<Hitter<K>> {
        let slot = self.index.get(key).and_then(|&at| self.slots.get(at))?;
        Some(Self::hitter(slot))
    }

    /// The `n` keys with the highest counts, highest first.
    #[must_use]
    pub fn top(&self, n: usize) -> Vec<Hitter<K>> {
        let mut ranked: Vec<&Slot<K>> = self.slots.iter().collect();
        // A stable sort keeps slot order between equal entries.
        ranked.sort_by(|a, b| b.count.cmp(&a.count).then(a.error.cmp(&b.error)));
        ranked.into_iter().take(n).map(Self::hitter).collect()
    }

    /// Every key whose guaranteed count is at least `share_ppm` parts per
    /// million of [`total`](Self::total), highest first.
    ///
    /// No key in the answer is a false positive. A key over the share can be
    /// missing only when the share is at most `1_000_000 / capacity` ppm or
    /// its error hides it; a larger table narrows both.
    #[must_use]
    pub fn frequent(&self, share_ppm: u32) -> Vec<Hitter<K>> {
        let threshold = u128::from(self.total).saturating_mul(u128::from(share_ppm));
        let mut hits: Vec<Hitter<K>> = self
            .top(self.capacity)
            .into_iter()
            .filter(|hit| u128::from(hit.guaranteed()).saturating_mul(1_000_000) >= threshold)
            .collect();
        hits.sort_by_key(|hit| std::cmp::Reverse(hit.guaranteed()));
        hits
    }

    /// Empties the table, for example at the end of a reporting window.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.index.clear();
        self.total = 0;
    }

    fn hitter(slot: &Slot<K>) -> Hitter<K> {
        Hitter {
            key: slot.key.clone(),
            count: slot.count,
            error: slot.error,
        }
    }
}

/// A [`SpaceSaving`] table to share between tasks. Every method takes `&self`.
#[derive(Debug)]
pub struct HeavyHitters<K> {
    table: Mutex<SpaceSaving<K>>,
}

impl<K: Eq + Hash + Clone> HeavyHitters<K> {
    /// An empty shared table of `capacity` counters.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            table: Mutex::new(SpaceSaving::new(capacity)),
        }
    }

    /// See [`SpaceSaving::observe`].
    pub fn observe(&self, key: K) {
        self.with(|table| table.observe(key));
    }

    /// See [`SpaceSaving::observe_n`].
    pub fn observe_n(&self, key: K, weight: u64) {
        self.with(|table| table.observe_n(key, weight));
    }

    /// See [`SpaceSaving::total`].
    #[must_use]
    pub fn total(&self) -> u64 {
        self.with(|table| table.total())
    }

    /// See [`SpaceSaving::estimate`].
    #[must_use]
    pub fn estimate(&self, key: &K) -> Option<Hitter<K>> {
        self.with(|table| table.estimate(key))
    }

    /// See [`SpaceSaving::top`].
    #[must_use]
    pub fn top(&self, n: usize) -> Vec<Hitter<K>> {
        self.with(|table| table.top(n))
    }

    /// See [`SpaceSaving::frequent`].
    #[must_use]
    pub fn frequent(&self, share_ppm: u32) -> Vec<Hitter<K>> {
        self.with(|table| table.frequent(share_ppm))
    }

    /// Returns the table as it stands and starts an empty one, so each
    /// reporting window counts from zero.
    #[must_use]
    pub fn take(&self) -> SpaceSaving<K> {
        self.with(|table| {
            let fresh = SpaceSaving::new(table.capacity());
            std::mem::replace(table, fresh)
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut SpaceSaving<K>) -> R) -> R {
        // A panic while counting leaves the table consistent enough to read:
        // every write is a single saturating add or a slot swap.
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn counts_are_exact_while_the_table_has_room() {
        let mut table = SpaceSaving::new(4);
        for key in ["a", "b", "a", "c", "a", "b"] {
            table.observe(key);
        }
        assert_eq!(table.total(), 6);
        let top = table.top(4);
        let summary: Vec<_> = top.iter().map(|h| (h.key, h.count, h.error)).collect();
        assert_eq!(summary, vec![("a", 3, 0), ("b", 2, 0), ("c", 1, 0)]);
    }

    #[test]
    fn a_new_key_takes_over_the_smallest_counter() {
        let mut table = SpaceSaving::new(2);
        table.observe_n("a", 5);
        table.observe_n("b", 2);
        table.observe("c");
        assert_eq!(table.estimate(&"b"), None);
        assert_eq!(
            table.estimate(&"c"),
            Some(Hitter {
                key: "c",
                count: 3,
                error: 2
            })
        );
        assert_eq!(table.estimate(&"c").map(|h| h.guaranteed()), Some(1));
    }

    #[test]
    fn ties_evict_the_first_slot_so_runs_replay() {
        let mut table = SpaceSaving::new(3);
        for key in ["x", "y", "z", "w"] {
            table.observe(key);
        }
        assert_eq!(table.estimate(&"x"), None);
        assert!(table.estimate(&"y").is_some());
        assert!(table.estimate(&"z").is_some());
        let top: Vec<_> = table.top(3).into_iter().map(|h| h.key).collect();
        assert_eq!(top, vec!["w", "y", "z"]);
    }

    /// A long tail of one-off keys around three hot ones. Every key over
    /// `total / capacity` must stay in the table, and its bounds must hold.
    #[test]
    fn keys_over_total_over_capacity_are_never_lost() {
        let capacity = 10;
        let mut table = SpaceSaving::new(capacity);
        let mut truth: HashMap<String, u64> = HashMap::new();
        for i in 0..5_000u32 {
            let key = match i % 10 {
                0..=2 => "hot-a".to_owned(),
                3 | 4 => "hot-b".to_owned(),
                5 => "hot-c".to_owned(),
                _ => format!("tail-{i}"),
            };
            *truth.entry(key.clone()).or_default() += 1;
            table.observe(key);
        }
        let floor = table.total() / capacity as u64;
        for (key, &real) in &truth {
            if real > floor {
                let hit = table.estimate(key).expect("heavy key evicted");
                assert!(hit.count >= real, "{key}: count below truth");
                assert!(hit.guaranteed() <= real, "{key}: guarantee above truth");
            }
        }
        let top: Vec<_> = table.top(3).into_iter().map(|h| h.key).collect();
        assert_eq!(top, vec!["hot-a", "hot-b", "hot-c"]);
    }

    #[test]
    fn frequent_reports_only_keys_surely_over_the_share() {
        let mut table = SpaceSaving::new(4);
        table.observe_n("big".to_owned(), 60);
        table.observe_n("mid".to_owned(), 25);
        for i in 0..15 {
            table.observe(format!("one-off-{i}"));
        }
        let keys: Vec<_> = table.frequent(200_000).into_iter().map(|h| h.key).collect();
        assert_eq!(keys, vec!["big", "mid"]);
        let keys: Vec<_> = table.frequent(500_000).into_iter().map(|h| h.key).collect();
        assert_eq!(keys, vec!["big"]);
        assert!(table.frequent(1_000_000).is_empty());
    }

    #[test]
    fn zero_weight_and_zero_capacity_are_harmless() {
        let mut table = SpaceSaving::new(0);
        assert_eq!(table.capacity(), 1);
        table.observe_n("a", 0);
        assert_eq!(table.total(), 0);
        assert!(table.top(5).is_empty());
        table.observe_n("a", u64::MAX);
        table.observe("a");
        assert_eq!(table.total(), u64::MAX);
        assert_eq!(table.estimate(&"a").map(|h| h.count), Some(u64::MAX));
    }

    #[test]
    fn clear_starts_over() {
        let mut table = SpaceSaving::new(2);
        table.observe("a");
        table.clear();
        assert_eq!(table.total(), 0);
        assert_eq!(table.estimate(&"a"), None);
    }

    #[test]
    fn shared_table_counts_every_thread_and_take_resets_it() {
        let hot = Arc::new(HeavyHitters::new(8));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let hot = Arc::clone(&hot);
                std::thread::spawn(move || {
                    for _ in 0..250 {
                        hot.observe("route:/login");
                    }
                    hot.observe_n("route:/upload", 10);
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker panicked");
        }
        assert_eq!(hot.total(), 1_040);
        assert_eq!(hot.estimate(&"route:/login").map(|h| h.count), Some(1_000));
        assert_eq!(hot.frequent(900_000).len(), 1);

        let window = hot.take();
        assert_eq!(window.top(1)[0].key, "route:/login");
        assert_eq!(hot.total(), 0);
        assert!(hot.top(1).is_empty());
    }
}
