//! The cluster's single distributed primitive: a grow-only (G-counter) CRDT.
//!
//! Each named counter is a map from **cell** to that cell's tally, where a cell
//! is one node's *one boot*: the key is `node id # incarnation` (see
//! [`cell_key`]). A node only ever writes its own current cell, so no two
//! writers share a cell and merge is per-cell `max` — commutative, associative
//! and idempotent, i.e. a join-semilattice. The counter's value is the
//! saturating sum of every cell.
//!
//! # Why the incarnation is in the key
//!
//! Without it, a node restarting with a stable `node_id` would restart its cell
//! at zero while its peer still remembers the old, higher value; per-cell max
//! would then silently absorb every post-restart increment until the new count
//! overtook the old one. Keying cells by boot removes that failure by
//! construction — no recovery step, no readiness gate.
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
//! RED PHASE (TDD): bodies are inert stubs — see the module docs on [`super`].

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
// `pub` throughout this file is crate-visible only: the enclosing `cluster`
// submodule is itself `pub(crate)`, so nothing here escapes the crate
// (clippy::redundant_pub_crate).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use super::{ClusterInner, Incarnation};

/// The separator between a node id and its incarnation in a cell key.
///
/// Config validation forbids `#` in `node_id` and `cluster_name`, which is what
/// keeps this encoding unambiguous.
pub const CELL_SEPARATOR: char = '#';

/// The wire key for one node's one boot: `"{node_id}#{incarnation}"`.
pub fn cell_key(node_id: &str, incarnation: Incarnation) -> String {
    format!("{node_id}{CELL_SEPARATOR}{incarnation}")
}

/// Per-`(node, boot)` cells of one named grow-only counter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterShards {
    /// `"node#incarnation" -> that boot's tally`. `BTreeMap` so the serialized
    /// form is byte-stable for a given document (deterministic tests, stable
    /// MACs).
    #[serde(flatten)]
    cells: BTreeMap<String, u64>,
}

impl CounterShards {
    /// Add `by` to `cell`'s tally, saturating at [`u64::MAX`].
    ///
    /// Only ever called with the *local* node's current cell key: a node that
    /// writes another cell breaks the semilattice.
    pub fn increment_cell(&mut self, cell: &str, by: u64) {
        // RED-PHASE STUB: must saturate into `self.cells[cell]`.
        let _ = (cell, by);
    }

    /// Merge `other` into `self` by taking the per-cell maximum.
    pub fn merge(&mut self, other: &Self) {
        // RED-PHASE STUB: must be commutative, associative and idempotent.
        let _ = other;
    }

    /// This counter's value: the saturating sum of every cell.
    pub fn value(&self) -> u64 {
        // RED-PHASE STUB.
        0
    }

    /// The tally recorded for one specific cell.
    pub fn cell_value(&self, cell: &str) -> u64 {
        self.cells.get(cell).copied().unwrap_or(0)
    }

    /// How many distinct cells this counter holds (one per node per boot).
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }
}

/// Handle onto one cluster-wide counter.
///
/// Cheap to clone; every clone addresses the same counter on the same node.
///
/// Reads are **eventually consistent**: [`get`](Self::get) can jump upward as
/// remote cells merge in, and never decreases.
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
    /// `pub(crate)` rather than `pub` (the exception to this file's `pub`
    /// convention): `ClusterCounter` IS publicly re-exported, so a `pub`
    /// constructor taking the crate-private `ClusterInner` would leak a
    /// private type into the public API.
    pub(crate) const fn new(inner: Arc<ClusterInner>, name: String) -> Self {
        Self { inner, name }
    }

    /// The counter's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Add one to this node's current cell. Synchronous: the local document is
    /// updated immediately and the push task is nudged.
    pub fn increment(&self) {
        self.increment_by(1);
    }

    /// Add `by` to this node's current cell, saturating at [`u64::MAX`].
    pub fn increment_by(&self, by: u64) {
        let cell = cell_key(
            &self.inner.node_id,
            self.inner.incarnation.load(Ordering::Relaxed),
        );
        {
            let mut state = self.inner.lock_state();
            state
                .counters
                .entry(self.name.clone())
                .or_default()
                .increment_cell(&cell, by);
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
    use super::{CounterShards, cell_key};

    /// Build cells by driving the real increment path, so a fixture can never
    /// be "more real" than the code under test.
    fn cells(entries: &[(&str, u64, u64)]) -> CounterShards {
        let mut out = CounterShards::default();
        for (node, incarnation, by) in entries {
            out.increment_cell(&cell_key(node, *incarnation), *by);
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
        let a = cells(&[("node-a", 1, 3), ("node-shared", 1, 1)]);
        let b = cells(&[("node-b", 1, 2), ("node-shared", 1, 4)]);

        assert_eq!(
            merged(&a, &b),
            merged(&b, &a),
            "merge(a, b) must equal merge(b, a)"
        );
        assert_eq!(
            merged(&a, &b).value(),
            9,
            "the merged value must keep the per-cell maximum (3 + 2 + max(1, 4)); \
             observed a={a:?} b={b:?}"
        );
    }

    #[test]
    fn merge_is_associative() {
        let a = cells(&[("node-a", 1, 1)]);
        let b = cells(&[("node-b", 1, 2)]);
        let c = cells(&[("node-c", 1, 3)]);

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
        let a = cells(&[("node-a", 1, 3), ("node-b", 1, 4)]);

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
            a.increment_cell(&cell_key("node-a", 1), 1);
        }
        let mut b = CounterShards::default();
        for _ in 0..2 {
            b.increment_cell(&cell_key("node-b", 1), 1);
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

    /// The review finding that put the incarnation into the cell key: a node
    /// restarting under a **stable** `node_id` must open a FRESH cell, so
    /// per-cell max cannot absorb its post-restart increments.
    #[test]
    fn restart_writes_a_fresh_cell_and_total_counts_both_boots() {
        // Boot 1 of node-a counted to 10; the peer remembers that.
        let peer_memory = cells(&[("node-a", 100, 10)]);

        // node-a restarts (same id, higher incarnation) and counts 3.
        let after_restart = cells(&[("node-a", 200, 3)]);

        assert_eq!(
            after_restart.cell_value(&cell_key("node-a", 200)),
            3,
            "the restarted boot must write its own cell; observed {after_restart:?}"
        );
        assert_eq!(
            after_restart.cell_value(&cell_key("node-a", 100)),
            0,
            "the restarted boot must NOT resume the previous boot's cell"
        );

        let converged = merged(&peer_memory, &after_restart);
        assert_eq!(
            converged.cell_count(),
            2,
            "both boots must survive the merge as distinct cells; observed {converged:?}"
        );
        assert_eq!(
            converged.value(),
            13,
            "the total must count both boots (10 + 3) — if it reads 10 the \
             post-restart increments were absorbed by per-cell max"
        );
    }

    #[test]
    fn merge_saturates_on_u64_overflow() {
        let mut a = CounterShards::default();
        a.increment_cell(&cell_key("node-a", 1), u64::MAX);
        a.increment_cell(&cell_key("node-a", 1), 5);
        let mut b = CounterShards::default();
        b.increment_cell(&cell_key("node-b", 1), u64::MAX);

        assert_eq!(
            a.cell_value(&cell_key("node-a", 1)),
            u64::MAX,
            "a local increment past u64::MAX must saturate, not wrap or panic"
        );
        assert_eq!(
            merged(&a, &b).value(),
            u64::MAX,
            "summing two saturated cells must saturate at u64::MAX"
        );
    }
}
