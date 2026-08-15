//! The cluster's single distributed primitive: a grow-only (G-counter) CRDT.
//!
//! Each named counter is a map from node id to that node's own tally. A node
//! only ever writes **its own** shard, so merge is per-shard `max` — which is
//! commutative, associative and idempotent, i.e. a join-semilattice. The
//! counter's value is the saturating sum of the shards.
//!
//! Decrement is deliberately absent in this slice; the wire structs carry
//! `#[serde(default)]` so a second (decrement) map can be added later without
//! breaking the format.
//!
//! # Saturation policy
//!
//! Every arithmetic step saturates at [`u64::MAX`]. A cluster that manages to
//! overflow a `u64` reports `u64::MAX` forever rather than wrapping; the panic
//! gate forbids the alternative.
//!
//! RED PHASE (TDD): bodies are inert stubs — see the module docs on
//! [`super`].

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
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
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{ClusterInner, NodeId};

/// Per-node shards of one named grow-only counter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CounterShards {
    /// `node id -> that node's own tally`. `BTreeMap` so the serialized form is
    /// byte-stable for a given document (deterministic tests, stable MACs).
    #[serde(default)]
    shards: BTreeMap<NodeId, u64>,
}

impl CounterShards {
    /// Add `by` to `node`'s own shard, saturating at [`u64::MAX`].
    ///
    /// Only ever called with the *local* node id: a node that writes another
    /// node's shard breaks the semilattice.
    pub(crate) fn increment_local(&mut self, node: &str, by: u64) {
        // RED-PHASE STUB: must saturate into `self.shards[node]`.
        let _ = (node, by);
    }

    /// Merge `other` into `self` by taking the per-shard maximum.
    pub(crate) fn merge(&mut self, other: &Self) {
        // RED-PHASE STUB: must be commutative, associative and idempotent.
        let _ = other;
    }

    /// This counter's value: the saturating sum of every shard.
    pub(crate) fn value(&self) -> u64 {
        // RED-PHASE STUB.
        0
    }

    /// The tally recorded for one specific node's shard.
    pub(crate) fn shard_value(&self, node: &str) -> u64 {
        self.shards.get(node).copied().unwrap_or(0)
    }
}

/// Handle onto one cluster-wide counter.
///
/// Cheap to clone; every clone addresses the same counter on the same node.
///
/// Reads are **eventually consistent**: [`get`](Self::get) can jump upward as
/// remote shards merge in, and never decreases.
#[derive(Clone)]
pub struct ClusterCounter {
    inner: Arc<ClusterInner>,
    name: String,
}

impl std::fmt::Debug for ClusterCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterCounter")
            .field("name", &self.name)
            .field("node_id", &self.inner.node_id)
            .finish_non_exhaustive()
    }
}

impl ClusterCounter {
    pub(crate) const fn new(inner: Arc<ClusterInner>, name: String) -> Self {
        Self { inner, name }
    }

    /// The counter's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Add one to this node's shard. Synchronous: the local document is
    /// updated immediately and the push loop is nudged.
    pub fn increment(&self) {
        self.increment_by(1);
    }

    /// Add `by` to this node's shard, saturating at [`u64::MAX`].
    pub fn increment_by(&self, by: u64) {
        {
            let mut state = self.inner.lock_state();
            state
                .counters
                .entry(self.name.clone())
                .or_default()
                .increment_local(&self.inner.node_id, by);
        }
        self.inner.notify.notify_one();
    }

    /// The counter's current value as this node sees it.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.inner
            .lock_state()
            .counters
            .get(&self.name)
            .map_or(0, CounterShards::value)
    }
}

#[cfg(test)]
mod tests {
    use super::CounterShards;

    /// Build shards by driving the real local-increment path, so a fixture can
    /// never be "more real" than the code under test.
    fn shards(entries: &[(&str, u64)]) -> CounterShards {
        let mut out = CounterShards::default();
        for (node, by) in entries {
            out.increment_local(node, *by);
        }
        out
    }

    fn merged(a: &CounterShards, b: &CounterShards) -> CounterShards {
        let mut out = a.clone();
        out.merge(b);
        out
    }

    #[test]
    fn merge_is_commutative() {
        let a = shards(&[("node-a", 3), ("shared", 1)]);
        let b = shards(&[("node-b", 2), ("shared", 4)]);

        assert_eq!(
            merged(&a, &b),
            merged(&b, &a),
            "merge(a, b) must equal merge(b, a)"
        );
        assert_eq!(
            merged(&a, &b).value(),
            9,
            "the merged value must keep the per-shard maximum (3 + 2 + max(1, 4)); \
             observed shards a={a:?} b={b:?}"
        );
    }

    #[test]
    fn merge_is_associative() {
        let a = shards(&[("node-a", 1)]);
        let b = shards(&[("node-b", 2)]);
        let c = shards(&[("node-c", 3)]);

        assert_eq!(
            merged(&merged(&a, &b), &c),
            merged(&a, &merged(&b, &c)),
            "merge must be associative"
        );
        assert_eq!(
            merged(&merged(&a, &b), &c).value(),
            6,
            "the associatively merged value must be 1 + 2 + 3"
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let a = shards(&[("node-a", 3), ("node-b", 4)]);

        assert_eq!(merged(&a, &a), a, "merge(a, a) must equal a");
        assert_eq!(
            a.value(),
            7,
            "the fixture must actually record its increments — an idempotence \
             check over an empty map proves nothing; observed {a:?}"
        );
    }

    #[test]
    fn concurrent_shard_updates_sum_after_merge() {
        let mut a = CounterShards::default();
        for _ in 0..3 {
            a.increment_local("node-a", 1);
        }
        let mut b = CounterShards::default();
        for _ in 0..2 {
            b.increment_local("node-b", 1);
        }

        assert_eq!(
            merged(&a, &b).value(),
            5,
            "3 increments on A plus 2 on B must read 5 after merging B into A; \
             observed a={a:?} b={b:?}"
        );
        assert_eq!(
            merged(&b, &a).value(),
            5,
            "…and the same in the other merge direction; observed a={a:?} b={b:?}"
        );
    }

    #[test]
    fn merge_saturates_on_u64_overflow() {
        let mut a = CounterShards::default();
        a.increment_local("node-a", u64::MAX);
        a.increment_local("node-a", 5);
        let mut b = CounterShards::default();
        b.increment_local("node-b", u64::MAX);

        assert_eq!(
            a.shard_value("node-a"),
            u64::MAX,
            "a local increment past u64::MAX must saturate, not wrap or panic"
        );
        assert_eq!(
            merged(&a, &b).value(),
            u64::MAX,
            "summing two saturated shards must saturate at u64::MAX"
        );
    }
}
