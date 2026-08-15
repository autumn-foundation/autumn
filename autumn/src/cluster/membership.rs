//! The replicated document and the local failure detector.
//!
//! # Two lenses, one document
//!
//! [`ClusterState`] is the *only* replicated thing: a member table and the
//! named counters, merged by a single [`ClusterState::merge`]. Membership and
//! the counter are two lenses on one join-semilattice, which is what makes
//! discovery, counter consistency and leave-convergence the same mechanism
//! observed three ways.
//!
//! # Replicated status vs. local liveness
//!
//! Replicated [`MemberStatus`] is `Alive`/`Left` only — a clean semilattice
//! ordered by `(incarnation, Left beats Alive at equal incarnation)`. Liveness
//! (`Alive` → `Suspect` → `Down`) is **never** replicated: it lives in
//! [`LivenessOverlay`], a pure function of per-peer last-push instants read
//! through the injected [`ClockSource`](crate::time::ClockSource). That split is
//! what makes "views are local and eventually consistent" true in the type
//! system rather than only in the docs.
//!
//! Incarnations exist even at two nodes: a node that sees *itself* marked
//! `Left` bumps its incarnation and re-pushes (refutation), which is what
//! defuses a replayed `Leave` as an eviction weapon.
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

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::counter::CounterShards;
use super::{Incarnation, NodeId};
use crate::time::MonotonicInstant;

/// Replicated member status. Deliberately only two values: liveness is local
/// (see [`LivenessOverlay`]) and is never gossiped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MemberStatus {
    /// The member is a participating peer.
    Alive,
    /// The member announced a clean departure at this incarnation.
    Left,
}

/// One member's replicated record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemberRecord {
    /// The address peers dial to reach this member.
    #[serde(default)]
    pub(crate) addr: String,
    /// The member's boot generation; higher always wins.
    #[serde(default)]
    pub(crate) incarnation: Incarnation,
    /// Replicated status at that incarnation.
    pub(crate) status: MemberStatus,
}

impl MemberRecord {
    /// An `Alive` record for `addr` at `incarnation`.
    pub(crate) fn alive(addr: impl Into<String>, incarnation: Incarnation) -> Self {
        Self {
            addr: addr.into(),
            incarnation,
            status: MemberStatus::Alive,
        }
    }

    /// A `Left` record for `addr` at `incarnation`.
    pub(crate) fn left(addr: impl Into<String>, incarnation: Incarnation) -> Self {
        Self {
            addr: addr.into(),
            incarnation,
            status: MemberStatus::Left,
        }
    }

    /// Merge `other` into `self`: higher incarnation wins outright, and at an
    /// equal incarnation `Left` beats `Alive`.
    pub(crate) fn merge(&mut self, other: &Self) {
        // RED-PHASE STUB: must implement the (incarnation, Left > Alive)
        // ordering that makes this type a join-semilattice.
        let _ = other;
    }
}

/// The single replicated document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClusterState {
    /// `node id -> member record`. `BTreeMap` keeps the serialized form stable.
    #[serde(default)]
    pub(crate) members: BTreeMap<NodeId, MemberRecord>,
    /// `counter name -> per-node shards`.
    #[serde(default)]
    pub(crate) counters: BTreeMap<String, CounterShards>,
}

impl ClusterState {
    /// Merge a peer's document into this one, field by field.
    ///
    /// Commutative, associative and idempotent — the property the counter's
    /// cross-node consistency reduces to.
    pub(crate) fn merge(&mut self, other: &Self) {
        // RED-PHASE STUB: must merge member records by their semilattice order
        // and counter shards by per-shard max.
        let _ = other;
    }

    /// Refute a record that marks **this** node as `Left` (or that carries a
    /// stale incarnation for it) by re-emerging `Alive` one incarnation higher.
    ///
    /// Returns the new incarnation when a refutation happened.
    pub(crate) fn refute(&mut self, me: &str) -> Option<Incarnation> {
        // RED-PHASE STUB: must bump `me`'s incarnation and set it back to Alive.
        let _ = me;
        None
    }
}

/// Locally-observed liveness of a peer. Never serialized, never gossiped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// A push arrived within the suspicion timeout.
    Alive,
    /// No push for at least one suspicion timeout.
    Suspect,
    /// No push for at least two suspicion timeouts; drops out of the view.
    Down,
}

/// The local failure detector: last-push instants plus a pure classification.
///
/// The contract this slice commits to, expressed in multiples of the configured
/// suspicion timeout `T`:
///
/// | time since last push | liveness |
/// |----------------------|----------|
/// | `< T`                | `Alive`  |
/// | `>= T` and `< 2T`    | `Suspect`|
/// | `>= 2T`              | `Down`   |
///
/// A peer that has never pushed is `Down`.
#[derive(Debug)]
pub(crate) struct LivenessOverlay {
    suspicion_timeout: Duration,
    last_push: BTreeMap<NodeId, MonotonicInstant>,
}

impl LivenessOverlay {
    /// A fresh overlay with no observations.
    pub(crate) const fn new(suspicion_timeout: Duration) -> Self {
        Self {
            suspicion_timeout,
            last_push: BTreeMap::new(),
        }
    }

    /// The suspicion timeout this overlay classifies against.
    pub(crate) const fn suspicion_timeout(&self) -> Duration {
        self.suspicion_timeout
    }

    /// Record that a state push from `node` arrived at `at`.
    pub(crate) fn record_push(&mut self, node: &str, at: MonotonicInstant) {
        self.last_push.insert(node.to_owned(), at);
    }

    /// Drop every observation of `node` (it left cleanly, or was pruned).
    pub(crate) fn forget(&mut self, node: &str) {
        self.last_push.remove(node);
    }

    /// Classify `node` as of `now`. Pure: no clock read, no mutation.
    pub(crate) fn liveness(&self, node: &str, now: MonotonicInstant) -> Liveness {
        // RED-PHASE STUB: must implement the Alive/Suspect/Down table above.
        let _ = (node, now);
        Liveness::Alive
    }
}

#[cfg(test)]
mod tests {
    use super::{Liveness, LivenessOverlay, MemberRecord, MemberStatus};
    use crate::cluster::membership::ClusterState;
    use crate::time::MonotonicInstant;
    use std::time::Duration;

    const SUSPICION: Duration = Duration::from_millis(2_500);

    fn at(millis: u64) -> MonotonicInstant {
        MonotonicInstant::from_origin_elapsed(Duration::from_millis(millis))
    }

    #[test]
    fn member_merge_prefers_higher_incarnation() {
        let mut older = MemberRecord::alive("127.0.0.1:7001", 3);
        let newer = MemberRecord::alive("127.0.0.1:7002", 4);

        older.merge(&newer);

        assert_eq!(
            older.incarnation, 4,
            "a higher incarnation must win outright; observed {older:?}"
        );
        assert_eq!(
            older.addr, "127.0.0.1:7002",
            "the winning record's address must win with it; observed {older:?}"
        );

        // …and merging a stale record back in must change nothing.
        let stale = MemberRecord::left("127.0.0.1:7000", 2);
        older.merge(&stale);
        assert_eq!(
            older.incarnation, 4,
            "a lower incarnation must never win; observed {older:?}"
        );
        assert_eq!(
            older.status,
            MemberStatus::Alive,
            "a stale Left must not evict a live member; observed {older:?}"
        );
    }

    #[test]
    fn leave_supersedes_alive_at_same_incarnation() {
        let mut alive = MemberRecord::alive("127.0.0.1:7001", 7);
        let left = MemberRecord::left("127.0.0.1:7001", 7);

        alive.merge(&left);

        assert_eq!(
            alive.status,
            MemberStatus::Left,
            "at an equal incarnation Left must beat Alive; observed {alive:?}"
        );

        // The order of the merge must not matter (it is a join).
        let mut left_first = MemberRecord::left("127.0.0.1:7001", 7);
        left_first.merge(&MemberRecord::alive("127.0.0.1:7001", 7));
        assert_eq!(
            left_first.status,
            MemberStatus::Left,
            "…in either merge direction; observed {left_first:?}"
        );
    }

    #[test]
    fn refutation_bumps_incarnation_over_stale_leave() {
        let mut state = ClusterState::default();
        state.members.insert(
            "node-a".to_owned(),
            MemberRecord::left("127.0.0.1:7001", 4),
        );

        let refuted = state.refute("node-a");

        assert_eq!(
            refuted,
            Some(5),
            "a node that sees itself Left must re-emerge one incarnation higher"
        );
        let record = state
            .members
            .get("node-a")
            .expect("the refuting node must stay in its own document");
        assert_eq!(
            record.status,
            MemberStatus::Alive,
            "refutation must restore Alive; observed {record:?}"
        );
        assert_eq!(
            record.incarnation, 5,
            "refutation must bump the incarnation; observed {record:?}"
        );

        // A document that does not mark us Left needs no refutation.
        let mut healthy = ClusterState::default();
        healthy.members.insert(
            "node-a".to_owned(),
            MemberRecord::alive("127.0.0.1:7001", 5),
        );
        assert_eq!(
            healthy.refute("node-a"),
            None,
            "an Alive self-record must not be refuted (that would bump forever)"
        );
    }

    #[test]
    fn missed_pushes_transition_member_to_down() {
        let mut overlay = LivenessOverlay::new(SUSPICION);
        overlay.record_push("node-b", at(0));

        assert_eq!(
            overlay.liveness("node-b", at(1_000)),
            Liveness::Alive,
            "inside the suspicion timeout a member is Alive"
        );
        assert_eq!(
            overlay.liveness("node-b", at(2_500)),
            Liveness::Suspect,
            "at exactly one suspicion timeout a member becomes Suspect"
        );
        assert_eq!(
            overlay.liveness("node-b", at(5_000)),
            Liveness::Down,
            "at two suspicion timeouts a member becomes Down and leaves the view"
        );
        assert_eq!(
            overlay.liveness("never-seen", at(0)),
            Liveness::Down,
            "a peer that has never pushed is Down, not Alive"
        );
    }

    #[test]
    fn push_receipt_resets_suspicion() {
        let mut overlay = LivenessOverlay::new(SUSPICION);
        overlay.record_push("node-b", at(0));

        assert_eq!(
            overlay.liveness("node-b", at(3_000)),
            Liveness::Suspect,
            "sanity: the member must be Suspect before the reset, or this test \
             proves nothing"
        );

        overlay.record_push("node-b", at(3_000));

        assert_eq!(
            overlay.liveness("node-b", at(3_100)),
            Liveness::Alive,
            "a fresh push must reset the suspicion clock"
        );
        assert_eq!(
            overlay.liveness("node-b", at(5_600)),
            Liveness::Suspect,
            "…and suspicion must then be measured from the NEW push"
        );
    }
}
