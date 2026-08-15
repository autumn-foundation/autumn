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
//! Replicated [`MemberStatus`] is `Alive`/`Left` only. Records merge pairwise:
//!
//! 1. higher `incarnation` wins;
//! 2. at equal incarnation, `Left` beats `Alive`;
//! 3. at equal incarnation and equal status, the lexicographically greater
//!    `addr` wins — a tie-break that exists only to keep the merge commutative.
//!
//! Liveness (`Alive` → `Suspect` → `Down`) is **never** replicated: it lives in
//! [`LivenessOverlay`], a pure function of per-peer last-receipt instants read
//! through the injected [`ClockSource`](crate::time::ClockSource). That split is
//! what makes "views are local and eventually consistent" true in the type
//! system rather than only in the docs.
//!
//! # Refutation
//!
//! Incarnations are seeded at boot from Unix **milliseconds** through the
//! injected clock, so a restart normally comes back strictly higher with no
//! persistence anywhere. Refutation covers the residual case (a clock that
//! stepped backwards) and keeps a live node from being buried: a node that
//! sees **any** record about itself at an incarnation `>=` its own — `Left` or
//! a stale `Alive` — adopts `observed + 1`, marks itself `Alive`, and pushes
//! immediately. See [`ClusterState::refute`].

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
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::counter::CounterShards;
use super::{Incarnation, NodeId};
use crate::time::MonotonicInstant;

/// How many suspicion timeouts a `Left` tombstone is kept before pruning.
pub const TOMBSTONE_TIMEOUT_MULTIPLE: u32 = 10;

/// Replicated member status. Deliberately only two values: liveness is local
/// (see [`LivenessOverlay`]) and is never gossiped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberStatus {
    /// The member is a participating peer.
    Alive,
    /// The member announced a clean departure at this incarnation.
    Left,
}

impl MemberStatus {
    /// This status's rank *within one incarnation*: `Left` outranks `Alive`.
    ///
    /// Merge rule 2 in one function — a leave is never undone by an in-flight
    /// older push carrying the same incarnation.
    const fn rank(self) -> u8 {
        match self {
            Self::Alive => 0,
            Self::Left => 1,
        }
    }
}

/// One member's replicated record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRecord {
    /// The address peers dial to reach this member.
    #[serde(default)]
    pub addr: String,
    /// The member's boot generation; higher always wins.
    #[serde(default)]
    pub incarnation: Incarnation,
    /// Replicated status at that incarnation.
    pub status: MemberStatus,
}

impl MemberRecord {
    /// An `Alive` record for `addr` at `incarnation`.
    pub fn alive(addr: impl Into<String>, incarnation: Incarnation) -> Self {
        Self {
            addr: addr.into(),
            incarnation,
            status: MemberStatus::Alive,
        }
    }

    /// A `Left` record for `addr` at `incarnation`.
    pub fn left(addr: impl Into<String>, incarnation: Incarnation) -> Self {
        Self {
            addr: addr.into(),
            incarnation,
            status: MemberStatus::Left,
        }
    }

    /// The three merge rules expressed as one comparable key:
    /// `(incarnation, status rank, addr)`.
    ///
    /// Records are ordered *totally* by this key, so the merge below is a
    /// maximum — which is what makes it commutative, associative and
    /// idempotent for free rather than by inspection of three separate cases.
    const fn merge_key(&self) -> (Incarnation, u8, &str) {
        (self.incarnation, self.status.rank(), self.addr.as_str())
    }

    /// Merge `other` into `self` by the three-rule order in the module docs.
    pub fn merge(&mut self, other: &Self) {
        if other.merge_key() > self.merge_key() {
            // The winning record wins whole: its address travels with its
            // incarnation, so a member that moved address is not left half
            // merged.
            self.clone_from(other);
        }
    }
}

/// The single replicated document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterState {
    /// `node id -> member record`. `BTreeMap` keeps the serialized form stable.
    #[serde(default)]
    pub members: BTreeMap<NodeId, MemberRecord>,
    /// `counter name -> per-cell tallies`.
    #[serde(default)]
    pub counters: BTreeMap<String, CounterShards>,
}

impl ClusterState {
    /// Merge a peer's document into this one, field by field.
    ///
    /// Commutative, associative and idempotent — the property the counter's
    /// cross-node consistency reduces to.
    pub fn merge(&mut self, other: &Self) {
        for (id, theirs) in &other.members {
            if let Some(ours) = self.members.get_mut(id) {
                ours.merge(theirs);
            } else {
                // A member we have never heard of: learning it *is* discovery.
                // Tombstones (`Left`) are learned too, so a leave keeps
                // propagating through a node that never met the departed peer.
                self.members.insert(id.clone(), theirs.clone());
            }
        }
        for (name, theirs) in &other.counters {
            // Per-cell max, delegated: the counter owns its own join.
            self.counters.entry(name.clone()).or_default().merge(theirs);
        }
    }

    /// Reconcile this document's record for `me` against `own`, the node's own
    /// authoritative self-record.
    ///
    /// Returns `Some(new_incarnation)` when a **refutation** is required: the
    /// document holds a record about us at an incarnation `>=` ours that is not
    /// the record we published (a `Left`, or a stale `Alive` from an earlier
    /// boot at the same second). The node must adopt that incarnation, mark
    /// itself `Alive`, and push immediately.
    ///
    /// Returns `None` when nothing needs refuting — including when the document
    /// simply echoes our own record back at us, which must **not** bump (an
    /// unconditional bump would ratchet forever).
    pub fn refute(&mut self, me: &str, own: &MemberRecord) -> Option<Incarnation> {
        let observed = self.members.get_mut(me)?;

        if observed == own {
            // Our own record, echoed back by a peer. Bumping here would ratchet
            // the incarnation on every single push round.
            return None;
        }
        if observed.incarnation < own.incarnation {
            // Already loses merge rule 1 against the record we publish; the
            // document heals itself on the next push without a bump.
            return None;
        }

        // Rule 1 outranks rule 2, so `observed + 1` buries a `Left` (or a stale
        // `Alive` from an earlier boot) everywhere it has spread. Saturating
        // because the panic gate forbids the wrapping alternative; a node that
        // reaches `u64::MAX` here stops bumping rather than wrapping to zero and
        // losing every argument.
        let bumped = observed.incarnation.saturating_add(1);
        observed.incarnation = bumped;
        observed.status = MemberStatus::Alive;
        // We are authoritative about our own address, whatever the document
        // claimed.
        observed.addr.clone_from(&own.addr);
        Some(bumped)
    }

    /// Drop `Left` tombstones that have outlived their window, forgetting the
    /// pruned members in `overlay` too. Returns how many records were dropped.
    ///
    /// A tombstone exists so a leave *propagates*; once every peer has had
    /// [`TOMBSTONE_TIMEOUT_MULTIPLE`] suspicion timeouts to hear it, keeping it
    /// only grows the document. Pruning is deliberately **local** and driven by
    /// the caller's clock reading: it removes no information any peer still
    /// needs, and a straggling push that re-teaches the tombstone simply
    /// re-inserts a record that is still `Left` and still out of the view.
    ///
    /// A tombstone for a member this node has never accepted a frame from has
    /// no receipt to measure the window against and is therefore kept.
    pub fn prune_tombstones(
        &mut self,
        overlay: &mut LivenessOverlay,
        now: MonotonicInstant,
    ) -> usize {
        let window = overlay.tombstone_timeout();
        let expired: Vec<NodeId> = self
            .members
            .iter()
            .filter_map(|(id, record)| {
                let lapsed = record.status == MemberStatus::Left
                    && overlay
                        .last_receipt(id)
                        .is_some_and(|seen| now.saturating_duration_since(seen) > window);
                lapsed.then(|| id.clone())
            })
            .collect();

        for id in &expired {
            self.members.remove(id);
            overlay.forget(id);
        }
        expired.len()
    }
}

/// Locally-observed liveness of a peer. Never serialized, never gossiped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// A frame arrived recently. In the view.
    Alive,
    /// Silence past two push intervals — a warning, not an eviction. Still in
    /// the view.
    Suspect,
    /// Silence past the suspicion timeout. Drops out of the view.
    Down,
}

impl Liveness {
    /// Whether a member in this state appears in [`members`](super::ClusterHandle::members).
    pub const fn in_view(self) -> bool {
        matches!(self, Self::Alive | Self::Suspect)
    }
}

/// The local failure detector: last-receipt instants plus a pure
/// classification.
///
/// The contract, in terms of the configured `push_interval` `P` and
/// `suspicion_timeout` `T` (config validation forces `T >= 3P`, so `Suspect`
/// always strictly precedes `Down`):
///
/// | silence since the last accepted frame | liveness  | in view |
/// |---------------------------------------|-----------|---------|
/// | `<= 2P`                               | `Alive`   | yes     |
/// | `> 2P` and `<= T`                     | `Suspect` | yes     |
/// | `> T`                                 | `Down`    | no      |
///
/// A peer that has never been heard from is `Down`.
#[derive(Debug)]
pub struct LivenessOverlay {
    push_interval: Duration,
    suspicion_timeout: Duration,
    last_seen: BTreeMap<NodeId, MonotonicInstant>,
}

impl LivenessOverlay {
    /// A fresh overlay with no observations.
    pub const fn new(push_interval: Duration, suspicion_timeout: Duration) -> Self {
        Self {
            push_interval,
            suspicion_timeout,
            last_seen: BTreeMap::new(),
        }
    }

    /// The push interval this overlay measures `Suspect` against.
    pub const fn push_interval(&self) -> Duration {
        self.push_interval
    }

    /// The suspicion timeout this overlay measures `Down` against.
    pub const fn suspicion_timeout(&self) -> Duration {
        self.suspicion_timeout
    }

    /// Record that a frame from `node` was accepted at `at`.
    pub fn record_receipt(&mut self, node: &str, at: MonotonicInstant) {
        self.last_seen.insert(node.to_owned(), at);
    }

    /// Drop every observation of `node` (it left cleanly, or was pruned).
    pub fn forget(&mut self, node: &str) {
        self.last_seen.remove(node);
    }

    /// When a frame from `node` was last accepted, if ever.
    fn last_receipt(&self, node: &str) -> Option<MonotonicInstant> {
        self.last_seen.get(node).copied()
    }

    /// How long a `Left` tombstone is kept before
    /// [`ClusterState::prune_tombstones`] drops it:
    /// [`TOMBSTONE_TIMEOUT_MULTIPLE`] suspicion timeouts.
    const fn tombstone_timeout(&self) -> Duration {
        self.suspicion_timeout()
            .saturating_mul(TOMBSTONE_TIMEOUT_MULTIPLE)
    }

    /// Classify `node` as of `now`. Pure: no clock read, no mutation.
    pub fn liveness(&self, node: &str, now: MonotonicInstant) -> Liveness {
        // Never heard from — `Down`, not `Alive`. Optimism here would put a peer
        // that has never once answered into the view.
        let Some(last) = self.last_receipt(node) else {
            return Liveness::Down;
        };

        let silence = now.saturating_duration_since(last);
        // `saturating_mul` rather than `2 *`: the panic gate forbids arithmetic
        // that can overflow, and a configured push interval near `Duration::MAX`
        // must clamp rather than wrap the threshold down to something tiny.
        if silence > self.suspicion_timeout {
            Liveness::Down
        } else if silence > self.push_interval.saturating_mul(2) {
            Liveness::Suspect
        } else {
            Liveness::Alive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClusterState, Liveness, LivenessOverlay, MemberRecord, MemberStatus,
        TOMBSTONE_TIMEOUT_MULTIPLE,
    };
    use crate::time::MonotonicInstant;
    use std::time::Duration;

    /// The shipped defaults: `suspicion_timeout` is 5x the push interval, so
    /// `Suspect` (at 2x push) strictly precedes `Down`.
    const PUSH: Duration = Duration::from_millis(500);
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

        // Rule 3: at equal incarnation AND equal status the greater addr wins,
        // purely so the merge stays commutative.
        let mut lower_addr = MemberRecord::alive("127.0.0.1:7001", 7);
        lower_addr.merge(&MemberRecord::alive("127.0.0.1:7009", 7));
        assert_eq!(
            lower_addr.addr, "127.0.0.1:7009",
            "the lexicographically greater addr must win the tie; observed {lower_addr:?}"
        );
    }

    /// The generalized refutation rule: ANY self-record at an incarnation `>=`
    /// our own — `Left` or a stale `Alive` — is refuted at `observed + 1`.
    #[test]
    fn refutation_bumps_incarnation_over_stale_leave() {
        let own = MemberRecord::alive("127.0.0.1:7001", 4);

        // A replayed / merged Leave at our own incarnation.
        let mut buried = ClusterState::default();
        buried
            .members
            .insert("node-a".to_owned(), MemberRecord::left("127.0.0.1:7001", 4));
        assert_eq!(
            buried.refute("node-a", &own),
            Some(5),
            "a Left about ourselves must be refuted one incarnation higher"
        );
        let record = buried
            .members
            .get("node-a")
            .expect("the refuting node must stay in its own document");
        assert_eq!(
            (record.status, record.incarnation),
            (MemberStatus::Alive, 5),
            "refutation must restore Alive at the bumped incarnation; observed {record:?}"
        );

        // A STALE ALIVE from a previous boot at a higher incarnation (a
        // same-second restart, or a clock that stepped backwards).
        let mut stale_alive = ClusterState::default();
        stale_alive.members.insert(
            "node-a".to_owned(),
            MemberRecord::alive("127.0.0.1:9999", 7),
        );
        assert_eq!(
            stale_alive.refute("node-a", &own),
            Some(8),
            "a stale Alive about ourselves at a higher incarnation must also be refuted"
        );

        // Our own record echoed back must NOT bump, or the incarnation
        // ratchets forever on every push.
        let mut echoed = ClusterState::default();
        echoed.members.insert("node-a".to_owned(), own.clone());
        assert_eq!(
            echoed.refute("node-a", &own),
            None,
            "an exact echo of our own record must not be refuted"
        );

        // An older record about us needs no refutation either.
        let mut older = ClusterState::default();
        older
            .members
            .insert("node-a".to_owned(), MemberRecord::left("127.0.0.1:7001", 2));
        assert_eq!(
            older.refute("node-a", &own),
            None,
            "a record older than ours loses the merge already; no bump needed"
        );
    }

    #[test]
    fn missed_pushes_transition_member_to_down() {
        let mut overlay = LivenessOverlay::new(PUSH, SUSPICION);
        overlay.record_receipt("node-b", at(0));

        assert_eq!(
            overlay.liveness("node-b", at(900)),
            Liveness::Alive,
            "within two push intervals a member is Alive"
        );
        assert_eq!(
            overlay.liveness("node-b", at(1_100)),
            Liveness::Suspect,
            "past two push intervals a member is Suspect — a warning, not an eviction"
        );
        assert!(
            overlay.liveness("node-b", at(1_100)).in_view(),
            "a Suspect member must still appear in the view"
        );
        assert_eq!(
            overlay.liveness("node-b", at(2_600)),
            Liveness::Down,
            "past the suspicion timeout a member is Down"
        );
        assert!(
            !overlay.liveness("node-b", at(2_600)).in_view(),
            "a Down member must leave the view"
        );
        assert_eq!(
            overlay.liveness("never-seen", at(0)),
            Liveness::Down,
            "a peer never heard from is Down, not Alive"
        );
    }

    #[test]
    fn push_receipt_resets_suspicion() {
        let mut overlay = LivenessOverlay::new(PUSH, SUSPICION);
        overlay.record_receipt("node-b", at(0));

        assert_eq!(
            overlay.liveness("node-b", at(1_500)),
            Liveness::Suspect,
            "sanity: the member must be Suspect before the reset, or this test \
             proves nothing"
        );

        overlay.record_receipt("node-b", at(1_500));

        assert_eq!(
            overlay.liveness("node-b", at(1_900)),
            Liveness::Alive,
            "a fresh receipt must reset the suspicion clock"
        );
        assert_eq!(
            overlay.liveness("node-b", at(2_700)),
            Liveness::Suspect,
            "…and silence must then be measured from the NEW receipt"
        );
        assert_eq!(
            overlay.liveness("node-b", at(4_100)),
            Liveness::Down,
            "…including the Down threshold"
        );
    }

    /// Coverage added in the green phase: the guide says tombstones are "pruned
    /// locally after ten suspicion timeouts", and no red test pinned it.
    #[test]
    fn left_tombstones_prune_after_ten_suspicion_timeouts() {
        let window = SUSPICION.saturating_mul(TOMBSTONE_TIMEOUT_MULTIPLE);
        let window_ms = u64::try_from(window.as_millis()).expect("the window fits in a u64 of ms");

        let mut overlay = LivenessOverlay::new(PUSH, SUSPICION);
        overlay.record_receipt("node-a", at(0));
        overlay.record_receipt("node-b", at(0));

        let mut state = ClusterState::default();
        state.members.insert(
            "node-a".to_owned(),
            MemberRecord::alive("127.0.0.1:7001", 4),
        );
        state
            .members
            .insert("node-b".to_owned(), MemberRecord::left("127.0.0.1:7002", 9));

        assert_eq!(
            state.prune_tombstones(&mut overlay, at(window_ms)),
            0,
            "a tombstone must survive its whole window — pruning it early would \
             let a straggling push resurrect the departed member; observed {state:?}"
        );

        assert_eq!(
            state.prune_tombstones(&mut overlay, at(window_ms.saturating_add(1))),
            1,
            "past ten suspicion timeouts the tombstone must be pruned; observed {state:?}"
        );
        assert!(
            !state.members.contains_key("node-b"),
            "the pruned tombstone must leave the document; observed {state:?}"
        );
        assert_eq!(
            overlay.liveness("node-b", at(window_ms)),
            Liveness::Down,
            "pruning must forget the overlay row too, so a pruned member reads \
             as never-seen rather than as a stale receipt"
        );
        assert!(
            state.members.contains_key("node-a"),
            "an Alive member must never be pruned, however long it has been \
             silent — silence is the overlay's business, not the document's; \
             observed {state:?}"
        );
    }
}
