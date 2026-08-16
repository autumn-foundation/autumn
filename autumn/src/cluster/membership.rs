//! The replicated document and the local failure detector.
//!
//! # Two lenses, one document
//!
//! [`ClusterState`] is the *only* replicated thing: a member table and the
//! named counters, merged by a single [`ClusterState::merge`]. Membership and
//! the counter are two lenses on one join-semilattice, which is what makes
//! discovery, counter convergence and leave-convergence the same mechanism
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

/// How many tombstone windows a node remembers what it pruned, in order to
/// refuse re-adopting it. Two: long enough that every peer holding the same
/// record has had a full window of its own to prune it.
pub const PRUNE_MEMORY_WINDOW_MULTIPLE: u32 = 2;

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
    /// cross-node convergence reduces to.
    ///
    /// # The recently-pruned guard
    ///
    /// `overlay` is only *read*: a `Left` record for a member this node pruned
    /// within the last [`PRUNE_MEMORY_WINDOW_MULTIPLE`] tombstone windows is
    /// not re-adopted unless it carries a **higher** incarnation than the one
    /// that was pruned. Without that, two peers pruning at different times
    /// re-teach each other the same departure forever: A prunes, B's copy
    /// arrives, A re-inserts it with a fresh local stamp and gossips it back,
    /// B prunes and re-learns it from A a window later — so a departed id is
    /// locally expirable at every step and still never leaves the cluster,
    /// which is how a long-lived pair grows its document toward the 64 KiB
    /// frame cap.
    ///
    /// This is a **local garbage-collection guard, not replicated state**:
    /// nothing about it is gossiped, two nodes hold different memories of what
    /// they collected, and all it withholds is information this node already
    /// had and deliberately discarded. The join itself is untouched where it
    /// carries information — a higher incarnation (a genuine rejoin, or a
    /// refutation) is always adopted, a member still in the document merges by
    /// the three rules as before, and the counters merge whatever the member
    /// table decides — and the memory expires on its own window, after which
    /// a record a peer still holds is learned normally again.
    pub fn merge(&mut self, other: &Self, overlay: &LivenessOverlay, now: MonotonicInstant) {
        for (id, theirs) in &other.members {
            if let Some(ours) = self.members.get_mut(id) {
                ours.merge(theirs);
            } else if !overlay.refuses_readoption(id, theirs, now) {
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
    /// the record we published (a `Left`, or a stale `Alive` from a boot whose
    /// clock ran ahead of this one). The node must adopt that incarnation, mark
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

    /// Stamp the local observation time of every `Left` record, and clear the
    /// stamp of every member that is `Alive` again.
    ///
    /// A tombstone's age is **not** the silence since the departed peer's last
    /// frame: pruning forgets that receipt, so a lagging peer that re-teaches
    /// the tombstone would re-insert a record whose age was then measured
    /// against the *sender's* receipts and never expired again — an unbounded
    /// document under repeated departures. The stamp is instead written the
    /// first time this node sees the departure and never rewritten while the
    /// record stays `Left`, so a reintroduced tombstone expires one window
    /// after it was re-learned rather than never.
    pub fn observe_tombstones(&self, overlay: &mut LivenessOverlay, now: MonotonicInstant) {
        for (id, record) in &self.members {
            match record.status {
                MemberStatus::Left => overlay.mark_tombstoned(id, now),
                // A member that came back (refutation, or a higher-incarnation
                // record) must not inherit the age of its previous departure.
                MemberStatus::Alive => overlay.clear_tombstone(id),
            }
        }
    }

    /// Drop `Left` tombstones that have outlived their window, forgetting the
    /// pruned members in `overlay` too. Returns the ids that were dropped.
    ///
    /// **The ids are returned, not counted, because a prune must forget the
    /// node *whole*.** The caller also owes the receive path's
    /// [`FrameVerifier`](super::wire::FrameVerifier) a
    /// [`forget`](super::wire::FrameVerifier::forget) for each id: once the
    /// tombstone is gone there is no record left to refute and nothing pushes
    /// to the departed address, so a node returning at a *lower* incarnation
    /// would be replay-dropped with no way back at all.
    ///
    /// A tombstone exists so a leave *propagates*; once every peer has had
    /// [`TOMBSTONE_TIMEOUT_MULTIPLE`] suspicion timeouts to hear it, keeping it
    /// only grows the document. Pruning is deliberately **local** and driven by
    /// the caller's clock reading: it removes no information any peer still
    /// needs. What it does record is what it collected — `(id, incarnation)`
    /// into the overlay's recently-pruned memory, for
    /// [`PRUNE_MEMORY_WINDOW_MULTIPLE`] windows — so a peer that prunes later
    /// cannot teach the same departure straight back into this document (see
    /// [`merge`](Self::merge)); that memory expires on its own window, so it
    /// stays bounded by the same departure count the member table is.
    ///
    /// A `Left` record nobody has stamped yet is kept — it is younger than one
    /// observation, not older than the window.
    pub fn prune_tombstones(
        &mut self,
        overlay: &mut LivenessOverlay,
        now: MonotonicInstant,
    ) -> Vec<NodeId> {
        let window = overlay.tombstone_timeout();
        let expired: Vec<(NodeId, Incarnation)> = self
            .members
            .iter()
            .filter_map(|(id, record)| {
                let lapsed = record.status == MemberStatus::Left
                    && overlay
                        .tombstoned_at(id)
                        .is_some_and(|seen| now.saturating_duration_since(seen) > window);
                lapsed.then(|| (id.clone(), record.incarnation))
            })
            .collect();

        for (id, incarnation) in &expired {
            self.members.remove(id);
            overlay.forget(id);
            // Forgotten as a member, remembered as a *collection*: the receipts
            // go, the record goes, and what stays is the one fact that keeps a
            // lagging peer from handing the whole thing back.
            overlay.remember_pruned(id, *incarnation, now);
        }
        // The memory is swept on the same pass that fills it, so it is bounded
        // by the departures of the last two windows rather than by the process
        // lifetime.
        overlay.expire_pruned(now);
        expired.into_iter().map(|(id, _)| id).collect()
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
    /// `node -> when this node first observed that member's departure`. Kept
    /// apart from `last_seen` on purpose: see
    /// [`ClusterState::observe_tombstones`].
    tombstoned_at: BTreeMap<NodeId, MonotonicInstant>,
    /// `node -> (incarnation collected, when it was collected)`. What this node
    /// has already pruned, remembered for
    /// [`PRUNE_MEMORY_WINDOW_MULTIPLE`] tombstone windows so a peer that prunes
    /// later cannot teach the departure back (see [`ClusterState::merge`]).
    /// Local garbage-collection memory: never serialized, never gossiped.
    recently_pruned: BTreeMap<NodeId, (Incarnation, MonotonicInstant)>,
}

impl LivenessOverlay {
    /// A fresh overlay with no observations.
    pub const fn new(push_interval: Duration, suspicion_timeout: Duration) -> Self {
        Self {
            push_interval,
            suspicion_timeout,
            last_seen: BTreeMap::new(),
            tombstoned_at: BTreeMap::new(),
            recently_pruned: BTreeMap::new(),
        }
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
    ///
    /// Deliberately does **not** touch the recently-pruned memory: that memory
    /// exists precisely to outlive the record and the receipts, and pruning
    /// writes it immediately after calling this.
    pub fn forget(&mut self, node: &str) {
        self.last_seen.remove(node);
        self.tombstoned_at.remove(node);
    }

    /// Remember that `node`'s tombstone at `incarnation` was collected at `at`.
    ///
    /// A later collection wins on both halves: a member that came back and left
    /// again must not be judged against the older departure it already
    /// outlived.
    fn remember_pruned(&mut self, node: &str, incarnation: Incarnation, at: MonotonicInstant) {
        let entry = self
            .recently_pruned
            .entry(node.to_owned())
            .or_insert((incarnation, at));
        if incarnation >= entry.0 {
            *entry = (incarnation, at);
        }
    }

    /// Drop memories older than [`PRUNE_MEMORY_WINDOW_MULTIPLE`] windows.
    fn expire_pruned(&mut self, now: MonotonicInstant) {
        let window = self.prune_memory_window();
        self.recently_pruned
            .retain(|_, (_, at)| now.saturating_duration_since(*at) <= window);
    }

    /// Whether `record` is a tombstone this node has already collected and is
    /// still within its window of remembering.
    ///
    /// `Alive` records are never refused — a returning node teaches its own
    /// liveness, and a higher incarnation always outranks what was collected,
    /// so the guard can only withhold a departure this node already processed.
    /// See [`ClusterState::merge`] for why that is a garbage-collection
    /// decision rather than a merge rule.
    fn refuses_readoption(&self, node: &str, record: &MemberRecord, now: MonotonicInstant) -> bool {
        if record.status != MemberStatus::Left {
            return false;
        }
        let window = self.prune_memory_window();
        self.recently_pruned
            .get(node)
            .is_some_and(|(collected, at)| {
                record.incarnation <= *collected && now.saturating_duration_since(*at) <= window
            })
    }

    /// Stamp `node`'s departure at `at`, unless it is already stamped: a
    /// tombstone ages from the FIRST observation, so a peer re-teaching it on
    /// every push cannot keep it young forever.
    fn mark_tombstoned(&mut self, node: &str, at: MonotonicInstant) {
        self.tombstoned_at.entry(node.to_owned()).or_insert(at);
    }

    /// Forget `node`'s departure stamp (it is `Alive` again).
    fn clear_tombstone(&mut self, node: &str) {
        self.tombstoned_at.remove(node);
    }

    /// When this node first observed `node`'s departure, if it has.
    fn tombstoned_at(&self, node: &str) -> Option<MonotonicInstant> {
        self.tombstoned_at.get(node).copied()
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

    /// How long a pruned `(id, incarnation)` is remembered:
    /// [`PRUNE_MEMORY_WINDOW_MULTIPLE`] tombstone windows.
    const fn prune_memory_window(&self) -> Duration {
        self.tombstone_timeout()
            .saturating_mul(PRUNE_MEMORY_WINDOW_MULTIPLE)
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
        PRUNE_MEMORY_WINDOW_MULTIPLE, TOMBSTONE_TIMEOUT_MULTIPLE,
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

    /// A node that has just collected `node-b`'s tombstone at incarnation 9,
    /// plus the overlay memory that collection left behind.
    fn after_collecting_node_b(pruned_at: u64) -> (ClusterState, LivenessOverlay) {
        let mut overlay = LivenessOverlay::new(PUSH, SUSPICION);
        let mut state = ClusterState::default();
        state
            .members
            .insert("node-b".to_owned(), MemberRecord::left("127.0.0.1:7002", 9));
        state.observe_tombstones(&mut overlay, at(0));
        assert_eq!(
            state.prune_tombstones(&mut overlay, at(pruned_at)),
            vec!["node-b".to_owned()],
            "sanity: the tombstone must prune, or nothing below is tested"
        );
        (state, overlay)
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

        // A STALE ALIVE from a previous boot at a higher incarnation (a clock
        // that stepped backwards, or two boots that read the same millisecond).
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
        // The window runs from the moment this node OBSERVED the departure.
        state.observe_tombstones(&mut overlay, at(0));

        assert_eq!(
            state.prune_tombstones(&mut overlay, at(window_ms)),
            Vec::<String>::new(),
            "a tombstone must survive its whole window — pruning it early would \
             let a straggling push resurrect the departed member; observed {state:?}"
        );

        // A FRESH receipt for the departing member, recorded a hair before the
        // prune. Without it the `Down` assertion below is vacuous: 25 s of
        // silence reads as `Down` whether or not the row was forgotten, so a
        // `forget` that stopped removing `last_seen` would pass unnoticed and
        // pruned members' receipt rows would accumulate forever under node-id
        // churn. With it, a surviving row reads `Alive` and the assertion bites.
        overlay.record_receipt("node-b", at(window_ms));

        assert_eq!(
            state.prune_tombstones(&mut overlay, at(window_ms.saturating_add(1))),
            vec!["node-b".to_owned()],
            "past ten suspicion timeouts the tombstone must be pruned, and the \
             pruned id must be REPORTED so the caller can forget the sender's \
             replay watermark too; observed {state:?}"
        );
        assert!(
            !state.members.contains_key("node-b"),
            "the pruned tombstone must leave the document; observed {state:?}"
        );
        assert_eq!(
            overlay.liveness("node-b", at(window_ms.saturating_add(1))),
            Liveness::Down,
            "pruning must forget the overlay row too, so a pruned member reads \
             as never-seen rather than as a one-millisecond-old receipt"
        );
        assert!(
            state.members.contains_key("node-a"),
            "an Alive member must never be pruned, however long it has been \
             silent — silence is the overlay's business, not the document's; \
             observed {state:?}"
        );
    }

    /// A tombstone re-taught by a lagging peer AFTER it was pruned must age
    /// from that re-observation and expire again.
    ///
    /// The bug this pins: measuring a tombstone's age against the departed
    /// member's last *receipt* means a pruned-then-reintroduced record has no
    /// receipt of its own (pruning forgot it) and can never expire, so repeated
    /// departures grow the document toward the 64 KiB frame cap.
    ///
    /// The reintroduction happens after the recently-pruned memory has expired,
    /// because inside that memory's window the record is not re-adopted at all
    /// (`pruned_tombstones_are_not_re_adopted_during_the_memory_window` covers
    /// that half). Both guards answer the same growth question from opposite
    /// ends: nothing is re-learned while the collection is remembered, and
    /// anything re-learned afterwards still expires on its own.
    #[test]
    fn reintroduced_tombstone_expires_again() {
        let window = SUSPICION.saturating_mul(TOMBSTONE_TIMEOUT_MULTIPLE);
        let window_ms = u64::try_from(window.as_millis()).expect("the window fits in a u64 of ms");

        let mut overlay = LivenessOverlay::new(PUSH, SUSPICION);
        let mut state = ClusterState::default();
        state
            .members
            .insert("node-b".to_owned(), MemberRecord::left("127.0.0.1:7002", 9));
        state.observe_tombstones(&mut overlay, at(0));
        assert_eq!(
            state.prune_tombstones(&mut overlay, at(window_ms.saturating_add(1))),
            vec!["node-b".to_owned()],
            "sanity: the first tombstone must prune, or this test proves nothing"
        );

        // A lagging peer pushes the same departure again, long after the
        // pruning — exactly what `merge` does with a record we no longer hold,
        // and late enough that the recently-pruned memory has lapsed.
        let reintroduced =
            at(window_ms.saturating_mul(u64::from(PRUNE_MEMORY_WINDOW_MULTIPLE).saturating_add(2)));
        let mut lagging = ClusterState::default();
        lagging
            .members
            .insert("node-b".to_owned(), MemberRecord::left("127.0.0.1:7002", 9));
        state.merge(&lagging, &overlay, reintroduced);
        assert!(
            state.members.contains_key("node-b"),
            "sanity: past the memory window the record must be re-learned, or \
             the expiry assertions below prove nothing; observed {state:?}"
        );
        state.observe_tombstones(&mut overlay, reintroduced);
        // The sender of that push is a DIFFERENT node, so attributing the age
        // to receipts would restart the clock on every one of its pushes.
        overlay.record_receipt("node-c", reintroduced);

        assert_eq!(
            state.prune_tombstones(&mut overlay, reintroduced.saturating_add(window)),
            Vec::<String>::new(),
            "a freshly re-learned tombstone must survive a full window; observed {state:?}"
        );
        assert_eq!(
            state.prune_tombstones(
                &mut overlay,
                reintroduced
                    .saturating_add(window)
                    .saturating_add(Duration::from_millis(1)),
            ),
            vec!["node-b".to_owned()],
            "a REINTRODUCED tombstone must expire one window after it was \
             re-observed — otherwise repeated departures grow the document \
             without bound; observed {state:?}"
        );
        assert!(
            !state.members.contains_key("node-b"),
            "the re-expired tombstone must leave the document; observed {state:?}"
        );
    }

    /// Pruning must actually remove a departed id from the cluster, not just
    /// from this node for a while.
    ///
    /// The bug this pins: two peers prune the same `Left` record at different
    /// times, so each prune is followed by the other's copy arriving, being
    /// re-inserted with a fresh local stamp and gossiped straight back. Every
    /// copy is locally expirable and the id never leaves, which is how a
    /// long-lived pair grows its document toward the 64 KiB frame cap on
    /// departures alone. A node therefore remembers what it collected for
    /// `PRUNE_MEMORY_WINDOW_MULTIPLE` windows and refuses to re-adopt it — a
    /// local garbage-collection guard, never replicated, and never applied to a
    /// higher incarnation, which is a genuine rejoin rather than an echo.
    #[test]
    fn pruned_tombstones_are_not_re_adopted_during_the_memory_window() {
        let window = SUSPICION.saturating_mul(TOMBSTONE_TIMEOUT_MULTIPLE);
        let window_ms = u64::try_from(window.as_millis()).expect("the window fits in a u64 of ms");
        let pruned_at = window_ms.saturating_add(1);

        // The lagging peer that has not pruned yet, pushing what it still holds.
        let mut lagging = ClusterState::default();
        lagging
            .members
            .insert("node-b".to_owned(), MemberRecord::left("127.0.0.1:7002", 9));

        // 1. Inside the memory window: the departure is not re-adopted.
        let (mut state, overlay) = after_collecting_node_b(pruned_at);
        state.merge(&lagging, &overlay, at(pruned_at.saturating_add(1)));
        assert!(
            !state.members.contains_key("node-b"),
            "a Left record this node already collected must not come back from \
             a peer that prunes later — that exchange is the recycling loop \
             that keeps departed ids in the document forever; observed {state:?}"
        );

        // 2. …for the whole window, not merely for an instant.
        let inside = pruned_at
            .saturating_add(window_ms.saturating_mul(u64::from(PRUNE_MEMORY_WINDOW_MULTIPLE)));
        state.merge(&lagging, &overlay, at(inside));
        assert!(
            !state.members.contains_key("node-b"),
            "the refusal must hold for the whole memory window — the peer keeps \
             pushing every interval; observed {state:?}"
        );

        // 3. Past it, the guard is gone: the document is a join again, and a
        //    record a peer still holds is learned (and re-expires — see
        //    `reintroduced_tombstone_expires_again`).
        state.merge(&lagging, &overlay, at(inside.saturating_add(1)));
        assert!(
            state.members.contains_key("node-b"),
            "the memory is bounded: past its window a tombstone a peer still \
             holds must merge normally, or this is a permanent local override \
             of the join rather than garbage collection; observed {state:?}"
        );

        // 4. A HIGHER incarnation is a rejoin, not an echo — adopted at once,
        //    inside the window, and its later departure with it.
        let (mut rejoining, overlay) = after_collecting_node_b(pruned_at);
        let mut returned = ClusterState::default();
        returned.members.insert(
            "node-b".to_owned(),
            MemberRecord::alive("127.0.0.1:7002", 11),
        );
        rejoining.merge(&returned, &overlay, at(pruned_at.saturating_add(1)));
        assert_eq!(
            rejoining.members.get("node-b").map(|record| record.status),
            Some(MemberStatus::Alive),
            "a node that came back must never be held out by what this node \
             collected about its previous life; observed {rejoining:?}"
        );

        let (mut departing_again, overlay) = after_collecting_node_b(pruned_at);
        let mut left_higher = ClusterState::default();
        left_higher.members.insert(
            "node-b".to_owned(),
            MemberRecord::left("127.0.0.1:7002", 12),
        );
        departing_again.merge(&left_higher, &overlay, at(pruned_at.saturating_add(1)));
        assert_eq!(
            departing_again
                .members
                .get("node-b")
                .map(|record| record.incarnation),
            Some(12),
            "a LATER departure outranks the collected one and must propagate \
             normally; refusing it would strand a real leave; observed \
             {departing_again:?}"
        );
    }
}
