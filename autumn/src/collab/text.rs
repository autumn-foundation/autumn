//! An RGA text CRDT: the merge behind `#[collaborative]`.
//!
//! [`CollabText`] holds one text field whose concurrent edits merge without
//! loss. It is a Replicated Growable Array (a causal tree): every character
//! carries a globally unique [`OpId`] and the id of the character it was
//! typed after. Merge integrates operations; it never compares wall clocks
//! and never discards a write, so it converges where the offline-sync
//! engine's last-write-wins resolver loses data.
//!
//! # Guarantees
//!
//! - **Convergence.** Replicas that hold the same set of operations render
//!   the same text, in any delivery order.
//! - **Causal safety.** An operation that arrives before the character it
//!   refers to waits in a buffer and integrates later. Nothing is dropped.
//! - **Idempotence.** Applying an operation twice changes nothing, so a
//!   client can safely replay after a reconnect.
//! - **Intention preservation.** An edit anchors to its neighbour character,
//!   not to an index, so it lands where the author meant even when the
//!   document changed in flight.
//!
//! # Example
//!
//! ```rust
//! use autumn_web::collab::CollabText;
//!
//! let mut server = CollabText::new();
//! server.insert("server", 0, "hello world");
//!
//! // Two editors branch from the same state.
//! let mut ada = server.clone();
//! let mut linus = server.clone();
//! let from_ada = ada.insert("ada", 5, ",");        // "hello, world"
//! let from_linus = linus.insert("linus", 11, "!"); // "hello world!"
//!
//! // Each side receives the other's operations, in either order.
//! for op in from_linus { ada.apply(op); }
//! for op in from_ada { linus.apply(op); }
//!
//! assert_eq!(ada.text(), "hello, world!");
//! assert_eq!(linus.text(), ada.text());
//! ```
//!
//! # Cost
//!
//! Integration scans the element list, so a merge is linear in document
//! length and a full replay is quadratic. Deleted characters stay as
//! tombstones. This suits note-sized and comment-sized fields, which is the
//! scope of the first slice.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Actor-id prefix for characters decoded from a plain-text column value.
///
/// A column that holds prose rather than a CRDT document — a field promoted
/// to `#[collaborative]` after the table already had rows — decodes into a
/// document seeded under `import:<digest of the text>`. The digest matters:
/// with one flat actor id, two seeds of *different* prose would number their
/// characters `1@import, 2@import, …` alike, and a merge — which dedups by
/// id — would drop the second document's characters as already-seen. Keying
/// on the content keeps the seed deterministic (the same prose always seeds
/// identically) while making different prose mint different ids.
pub const IMPORT_ACTOR: &str = "import";

/// The seed actor for one piece of imported prose: [`IMPORT_ACTOR`] plus a
/// digest of the text.
///
/// FNV-1a, not `DefaultHasher`: the value is written into stored documents,
/// and `DefaultHasher` gives no stability guarantee across Rust releases.
fn import_actor_for(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{IMPORT_ACTOR}:{hash:016x}")
}

/// The largest counter a character id may carry.
///
/// Two reasons, one bound. A peer can put any counter in an operation, and a
/// replica that adopted `u64::MAX` could never mint again — every keystroke
/// by every editor would silently do nothing, for good, because the document
/// persists and reloads that clock. And a browser replica compares counters
/// as a JavaScript `Number`, which is exact only to 2^53; above it two
/// distinct ids compare equal there and not here, which orders the same two
/// characters differently in the browser and in the stored document.
///
/// 2^53 characters is far beyond what this slice's linear merge can hold, so
/// the ceiling costs nothing real. A peer operation must stay *below* it; the
/// value itself is reserved so a replica can always mint one more id.
pub const MAX_COUNTER: u64 = 1 << 53;

/// The encoding of an empty document — the SQL default a `#[collaborative]`
/// column needs (`TEXT NOT NULL DEFAULT '{"elems":[]}'`).
///
/// `{}` would not do: the stored shape requires `elems`, so a bare `{}` reads
/// as prose rather than as an empty document.
pub const EMPTY_DOCUMENT: &str = r#"{"elems":[]}"#;

/// Globally unique id of one character.
///
/// Ordering is `(counter, actor)`. The counter is a Lamport clock, so a
/// character always sorts above the character it was typed after; RGA's
/// integration rule depends on that.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId {
    /// Lamport counter, unique per actor.
    pub counter: u64,
    /// Replica that minted the id.
    pub actor: String,
}

impl OpId {
    /// Build an id from its two parts.
    #[must_use]
    pub fn new(counter: u64, actor: impl Into<String>) -> Self {
        Self {
            counter,
            actor: actor.into(),
        }
    }
}

/// `"12@ada"` — counter, `@`, actor.
impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.counter, self.actor)
    }
}

/// Error returned when an [`OpId`] string is malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed collaborative id {0:?}: expected \"<counter>@<actor>\"")]
pub struct OpIdParseError(String);

impl FromStr for OpId {
    type Err = OpIdParseError;

    /// Splits on the **first** `@`, so an actor id may contain one.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (counter, actor) = s
            .split_once('@')
            .ok_or_else(|| OpIdParseError(s.to_owned()))?;
        if actor.is_empty() {
            return Err(OpIdParseError(s.to_owned()));
        }
        let counter = counter.parse().map_err(|_| OpIdParseError(s.to_owned()))?;
        Ok(Self {
            counter,
            actor: actor.to_owned(),
        })
    }
}

impl Serialize for OpId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for OpId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OpIdVisitor;

        impl Visitor<'_> for OpIdVisitor {
            type Value = OpId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a collaborative id of the form \"<counter>@<actor>\"")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse().map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(OpIdVisitor)
    }
}

/// One convergent edit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CollabOp {
    /// Add `ch` directly after `after`, or at the start when `after` is
    /// `None`.
    Insert {
        /// Id of the new character.
        id: OpId,
        /// Character it was typed after.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<OpId>,
        /// The character itself.
        ch: char,
    },
    /// Tombstone the character `target` names. Idempotent.
    Delete {
        /// Id of the character to remove.
        target: OpId,
    },
}

impl CollabOp {
    /// The counter this operation *mints*, which advances the receiving
    /// replica's Lamport clock.
    ///
    /// Only the operation's own id counts. A reference — an insert's `after`,
    /// a delete's `target` — must never move the clock: it is supplied by the
    /// sender and names a character this replica has not necessarily seen, so
    /// trusting it lets one message push the clock to `u64::MAX` and wedge the
    /// document. The referenced character advances the clock when *its* own
    /// insert arrives, which is the only moment it is real.
    const fn minted_counter(&self) -> u64 {
        match self {
            Self::Insert { id, .. } => id.counter,
            Self::Delete { .. } => 0,
        }
    }
}

/// One character in the document, tombstoned or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Elem {
    id: OpId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<OpId>,
    ch: char,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    deleted: bool,
}

/// One visible or tombstoned character, as the wire and the browser see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollabElement {
    /// Id of the character.
    pub id: OpId,
    /// The character.
    pub ch: char,
    /// Whether it has been deleted.
    #[serde(default)]
    pub deleted: bool,
}

/// A text field whose concurrent edits merge without loss.
///
/// Declared on a model with `#[collaborative]`; see the [module
/// docs](self) for the guarantees and the cost.
#[derive(Clone, Default)]
#[cfg_attr(feature = "db", derive(diesel::AsExpression, diesel::FromSqlRow))]
#[cfg_attr(feature = "db", diesel(sql_type = diesel::sql_types::Text))]
pub struct CollabText {
    /// Every character, in document order, tombstones included.
    elems: Vec<Elem>,
    /// Ids already integrated — dedup and causal-readiness in one lookup.
    index: HashSet<OpId>,
    /// Operations whose cause has not arrived yet. Never discarded.
    ///
    /// A `Vec` for deterministic order, with `buffered` as its membership
    /// index: the dedup check runs on every apply, and a linear scan over a
    /// large buffer would run under the caller's lock.
    pending: Vec<CollabOp>,
    /// Membership index for `pending`.
    buffered: HashSet<CollabOp>,
    /// Lamport clock: the highest counter this replica has seen.
    clock: u64,
}

impl CollabText {
    /// An empty document.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A document that holds `text`, authored by `actor`.
    #[must_use]
    pub fn from_text(actor: &str, text: &str) -> Self {
        let mut doc = Self::new();
        doc.insert(actor, 0, text);
        doc
    }

    /// The visible text.
    #[must_use]
    pub fn text(&self) -> String {
        self.elems
            .iter()
            .filter(|e| !e.deleted)
            .map(|e| e.ch)
            .collect()
    }

    /// Count of visible characters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.elems.iter().filter(|e| !e.deleted).count()
    }

    /// Whether no visible character remains.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.elems.iter().all(|e| e.deleted)
    }

    /// The replica's Lamport clock — the highest counter it has seen.
    #[must_use]
    pub const fn clock(&self) -> u64 {
        self.clock
    }

    /// Count of operations still waiting for their cause.
    #[must_use]
    pub const fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Every character in document order, tombstones included.
    ///
    /// This is the view a client keeps so it can anchor an edit to a
    /// neighbour id instead of an index.
    #[must_use]
    pub fn elements(&self) -> Vec<CollabElement> {
        self.elems
            .iter()
            .map(|e| CollabElement {
                id: e.id.clone(),
                ch: e.ch,
                deleted: e.deleted,
            })
            .collect()
    }

    /// Count of characters the document holds, tombstones included — what it
    /// costs, as opposed to what it shows.
    #[must_use]
    pub const fn element_count(&self) -> usize {
        self.elems.len()
    }

    /// How many of `ops` this replica does not already hold.
    ///
    /// A capacity preflight counts this, not the batch length: a reconnect
    /// replays the whole history, and charging a document for operations it
    /// already has would refuse an idempotent replay that adds nothing.
    ///
    /// The batch is weighed as a whole, in two passes, because an operation's
    /// cost depends on what the rest of the batch brings. A delete costs
    /// nothing when its target is already here **or arrives in the same
    /// batch** — only a delete left with nothing to tombstone occupies the
    /// buffer. Judging each operation against the pre-batch state alone would
    /// charge an insert and its own delete twice over and refuse a history
    /// that fits.
    ///
    /// Cost is per distinct **operation**, not per distinct id, because that
    /// is what the buffer holds. `buffered` keys on the whole operation, so
    /// one id paired with a thousand different unknown `after` values is a
    /// thousand buffer entries. Counting ids there would charge one and let
    /// the rest past [`CollabLimits::max_document_chars`].
    ///
    /// [`CollabLimits::max_document_chars`]: crate::collab::CollabLimits::max_document_chars
    #[must_use]
    pub fn novel_count<'a>(&self, ops: impl IntoIterator<Item = &'a CollabOp>) -> usize {
        let ops: Vec<&CollabOp> = ops.into_iter().collect();

        // Pass one: the characters this batch brings that are not here yet.
        let introduced: HashSet<&OpId> = ops
            .iter()
            .filter_map(|op| match op {
                CollabOp::Insert { id, .. } if !self.index.contains(id) => Some(id),
                _ => None,
            })
            .collect();

        // Pass two: the distinct operations that will occupy a slot.
        let charged: HashSet<&&CollabOp> = ops
            .iter()
            .filter(|op| match op {
                // An id already here integrates as a no-op and stores nothing.
                CollabOp::Insert { id, .. } => !self.index.contains(id),
                // A target the batch satisfies is tombstoned, not buffered.
                CollabOp::Delete { target } => {
                    !self.index.contains(target) && !introduced.contains(target)
                }
            })
            // Anything already buffered is paid for.
            .filter(|op| !self.buffered.contains(**op))
            .collect();

        charged.len()
    }

    /// The operations still waiting for the character they name.
    ///
    /// A snapshot must carry these: an editor who joins while one waits would
    /// otherwise never see it. The hub broadcasts an operation when it
    /// arrives, not when it later integrates, so there is no second chance.
    #[must_use]
    pub fn pending_ops(&self) -> &[CollabOp] {
        &self.pending
    }

    /// Whether `op` is waiting in the causal buffer.
    ///
    /// Distinguishes the two `false` answers [`apply`](Self::apply) gives:
    /// "buffered, it will land later" from "refused, it never will". A caller
    /// that forwards operations must not pass on a refused one.
    #[must_use]
    pub fn holds_pending(&self, op: &CollabOp) -> bool {
        self.buffered.contains(op)
    }

    /// Whether this replica already holds the character `id` names.
    ///
    /// The authority check a hub makes before accepting a client's anchor.
    #[must_use]
    pub fn knows(&self, id: &OpId) -> bool {
        self.index.contains(id)
    }

    /// The id of the visible character at `index`, or `None` when `index` is
    /// past the end.
    #[must_use]
    pub fn id_at(&self, index: usize) -> Option<OpId> {
        self.elems
            .iter()
            .filter(|e| !e.deleted)
            .nth(index)
            .map(|e| e.id.clone())
    }

    /// Insert `text` before the visible character at `index`.
    ///
    /// An `index` past the end appends. Returns the operations to send to
    /// the other replicas; they are already applied here.
    ///
    /// `actor` must name **one replica**. Two replicas editing under the same
    /// actor mint the same ids for different characters, and the merge — which
    /// dedups by id — then drops one side's text. A session hub takes care of
    /// this (see [`CollabDoc::join`](crate::collab::CollabDoc::join)); code
    /// calling this directly must not reuse an actor across replicas.
    pub fn insert(&mut self, actor: &str, index: usize, text: &str) -> Vec<CollabOp> {
        let anchor = self.anchor_for(index);
        self.insert_after(actor, anchor.as_ref(), text)
    }

    /// Insert `text` directly after the character `after` names, or at the
    /// start when `after` is `None`.
    ///
    /// The id-anchored form: a client that resolved an index against its own
    /// view sends this, and the edit lands next to the intended neighbour
    /// even if the document changed in flight. An unknown `after` buffers the
    /// operations rather than dropping them.
    pub fn insert_after(&mut self, actor: &str, after: Option<&OpId>, text: &str) -> Vec<CollabOp> {
        let mut ops = Vec::with_capacity(text.chars().count());
        let mut left = after.cloned();
        for ch in text.chars() {
            // Stop below the reserved ceiling, not at it. `apply` refuses a
            // counter of `MAX_COUNTER` or more, so minting one would return
            // an operation this very replica rejects — and the hub would
            // broadcast a character the authority does not hold.
            let next = self.clock.saturating_add(1);
            if next >= MAX_COUNTER {
                break;
            }
            self.clock = next;
            let id = OpId::new(self.clock, actor);
            let op = CollabOp::Insert {
                id: id.clone(),
                after: left,
                ch,
            };
            self.apply(op.clone());
            ops.push(op);
            left = Some(id);
        }
        ops
    }

    /// Delete `count` visible characters starting at `index`.
    ///
    /// A range past the end deletes what exists and stops.
    pub fn remove(&mut self, index: usize, count: usize) -> Vec<CollabOp> {
        let targets: Vec<OpId> = self
            .elems
            .iter()
            .filter(|e| !e.deleted)
            .skip(index)
            .take(count)
            .map(|e| e.id.clone())
            .collect();
        self.remove_ids(&targets)
    }

    /// Delete the characters `ids` names. Unknown ids are buffered.
    pub fn remove_ids(&mut self, ids: &[OpId]) -> Vec<CollabOp> {
        let mut ops = Vec::with_capacity(ids.len());
        for target in ids {
            let op = CollabOp::Delete {
                target: target.clone(),
            };
            self.apply(op.clone());
            ops.push(op);
        }
        ops
    }

    /// Delete only the characters `ids` names that this replica already has.
    ///
    /// The live-editor path. Unlike [`remove_ids`](Self::remove_ids) an
    /// unknown id is dropped, not buffered: buffering it would tombstone a
    /// character the moment somebody else typed it, which lets one client
    /// pre-delete another's future text. A replica merging a peer's history
    /// still wants the buffering form.
    pub fn remove_known(&mut self, ids: &[OpId]) -> Vec<CollabOp> {
        let known: Vec<OpId> = ids
            .iter()
            .filter(|id| self.index.contains(id))
            .cloned()
            .collect();
        self.remove_ids(&known)
    }

    /// Rewrite the document to `new_text` with the smallest edit that gets
    /// there: keep the common prefix and suffix, delete the rest, insert the
    /// replacement.
    ///
    /// This is what a plain form post needs — it turns "here is the whole
    /// field" into character-level operations, so a concurrent edit outside
    /// the changed span survives.
    pub fn set_text(&mut self, actor: &str, new_text: &str) -> Vec<CollabOp> {
        let old: Vec<char> = self.text().chars().collect();
        let new: Vec<char> = new_text.chars().collect();

        let mut prefix = 0;
        while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old.len() - prefix
            && suffix < new.len() - prefix
            && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
        {
            suffix += 1;
        }

        let mut ops = self.remove(prefix, old.len() - prefix - suffix);
        let added: String = new[prefix..new.len() - suffix].iter().collect();
        if !added.is_empty() {
            ops.extend(self.insert(actor, prefix, &added));
        }
        ops
    }

    /// Integrate one operation.
    ///
    /// Returns `true` when it took effect now, `false` when it is buffered
    /// until the character it refers to arrives. A buffered operation is
    /// never lost: every later integration retries the buffer.
    pub fn apply(&mut self, op: CollabOp) -> bool {
        // Refuse an id at or past the ceiling before the clock can adopt it.
        // Drop it rather than buffer it: it can never become valid, and a
        // buffered copy would just sit in the document forever.
        //
        // At or past, not past: a peer id of exactly `MAX_COUNTER` would pin
        // the clock to the ceiling, and every later local keystroke would
        // then mint nothing at all — silently, for good. Reserving the last
        // counter for this replica keeps minting possible after any
        // operation a peer can send.
        if op.minted_counter() >= MAX_COUNTER {
            return false;
        }
        self.clock = self.clock.max(op.minted_counter());
        if self.integrate(&op) {
            self.drain_pending();
            true
        } else {
            if self.buffered.insert(op.clone()) {
                self.pending.push(op);
            }
            false
        }
    }

    /// Integrate many operations, in any order.
    pub fn apply_all(&mut self, ops: impl IntoIterator<Item = CollabOp>) {
        for op in ops {
            self.apply(op);
        }
    }

    /// Merge `other` in. Commutative, associative and idempotent: merging
    /// twice, or in the other direction, gives the same document.
    pub fn merge(&mut self, other: &Self) {
        self.apply_all(other.ops());
    }

    /// Every operation this document holds, including the ones still
    /// waiting for their cause.
    ///
    /// Inserts come in document order, so a receiver that applies them in
    /// order never buffers.
    #[must_use]
    pub fn ops(&self) -> Vec<CollabOp> {
        let mut ops: Vec<CollabOp> = self
            .elems
            .iter()
            .map(|e| CollabOp::Insert {
                id: e.id.clone(),
                after: e.after.clone(),
                ch: e.ch,
            })
            .collect();
        ops.extend(
            self.elems
                .iter()
                .filter(|e| e.deleted)
                .map(|e| CollabOp::Delete {
                    target: e.id.clone(),
                }),
        );
        ops.extend(self.pending.iter().cloned());
        ops
    }

    // ── integration ──────────────────────────────────────────────────────

    /// Apply one operation if its cause is present. `false` means "not yet".
    fn integrate(&mut self, op: &CollabOp) -> bool {
        match op {
            CollabOp::Insert { id, after, ch } => {
                if self.index.contains(id) {
                    return true; // already integrated
                }
                let Some(start) = self.slot_after(after.as_ref()) else {
                    return false;
                };
                let at = self.rga_position(start, id);
                self.elems.insert(
                    at,
                    Elem {
                        id: id.clone(),
                        after: after.clone(),
                        ch: *ch,
                        deleted: false,
                    },
                );
                self.index.insert(id.clone());
                true
            }
            CollabOp::Delete { target } => {
                let Some(pos) = self.position_of(target) else {
                    return false;
                };
                self.elems[pos].deleted = true;
                true
            }
        }
    }

    /// Retry the buffer until nothing more integrates.
    fn drain_pending(&mut self) {
        while !self.pending.is_empty() {
            let mut blocked = Vec::new();
            let mut progressed = false;
            for op in std::mem::take(&mut self.pending) {
                if self.integrate(&op) {
                    self.buffered.remove(&op);
                    progressed = true;
                } else {
                    blocked.push(op);
                }
            }
            self.pending = blocked;
            if !progressed {
                break;
            }
        }
    }

    /// Index of the character `id` names.
    fn position_of(&self, id: &OpId) -> Option<usize> {
        if !self.index.contains(id) {
            return None;
        }
        self.elems.iter().position(|e| &e.id == id)
    }

    /// First slot an insert anchored at `after` may occupy.
    fn slot_after(&self, after: Option<&OpId>) -> Option<usize> {
        after.map_or(Some(0), |id| self.position_of(id).map(|p| p + 1))
    }

    /// RGA's placement rule: from `start`, step over every character that
    /// sorts above `id`, and stop at the first that sorts below.
    ///
    /// Two facts make this right, and the rule needs both.
    ///
    /// It never stops too early: stepping over a character steps over
    /// everything typed after it, because a Lamport counter only grows, so a
    /// descendant sorts above its ancestor and therefore above `id` too.
    ///
    /// It never runs too far: the first character past the anchor's subtree
    /// is a later sibling of the anchor or of one of its ancestors, and such
    /// a sibling sorts *below* that ancestor, which sorts below the anchor,
    /// which sorts below `id`. So the scan halts exactly at the subtree's
    /// edge.
    ///
    /// Together they place a character identically on every replica that
    /// holds the same operations, which is what convergence means here.
    fn rga_position(&self, start: usize, id: &OpId) -> usize {
        let mut at = start;
        while at < self.elems.len() && self.elems[at].id > *id {
            at += 1;
        }
        at
    }

    /// Anchor for an insert before the visible character at `index`: the
    /// character immediately to its left in document order, tombstones
    /// included.
    fn anchor_for(&self, index: usize) -> Option<OpId> {
        let slot = self
            .elems
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.deleted)
            .nth(index)
            .map_or(self.elems.len(), |(i, _)| i);
        slot.checked_sub(1).map(|i| self.elems[i].id.clone())
    }
}

/// Two documents are equal when they hold the same characters in the same
/// order with the same tombstones, and buffer the same operations. The
/// Lamport clock is excluded: two replicas that agree on every character can
/// still have seen a different number of operations.
impl PartialEq for CollabText {
    fn eq(&self, other: &Self) -> bool {
        if self.elems != other.elems || self.pending.len() != other.pending.len() {
            return false;
        }
        let (mut mine, mut theirs) = (self.pending.clone(), other.pending.clone());
        mine.sort();
        theirs.sort();
        mine == theirs
    }
}

impl Eq for CollabText {}

/// Renders the visible text.
impl fmt::Display for CollabText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

/// Shows the visible text plus the buffered-operation count: a `Debug` that
/// hid the buffer would make a stuck merge invisible in test output.
impl fmt::Debug for CollabText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `finish_non_exhaustive`: the derived state (`index`, `buffered`,
        // `clock`) restates what these three already show, and printing it
        // would bury the text this exists to surface.
        f.debug_struct("CollabText")
            .field("text", &self.text())
            .field("elements", &self.elems.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl From<&str> for CollabText {
    /// Seeds a document the same way a plain-text column value decodes.
    ///
    /// Use it to import prose once. It is **not** the way to apply a form
    /// post: a fresh document per request throws away the merge history, so
    /// two of them merged together interleave rather than converge. Load the
    /// record and call [`set_text`](CollabText::set_text) instead.
    fn from(text: &str) -> Self {
        Self::from_text(&import_actor_for(text), text)
    }
}

// ── Wire form ────────────────────────────────────────────────────────────────

/// The stored and transmitted shape of a document.
///
/// The Lamport clock is **not** stored: it is the highest counter in the
/// document, so two replicas that hold the same operations encode the same
/// bytes. Storing it would make the column differ between replicas that
/// agree on the text.
///
/// `elems` is **required**, and that is load-bearing. With a default, every
/// JSON object decodes as an empty document — so an unrelated object in a
/// collaborative field would read as "no text", and a merge would replace
/// real characters with nothing. An empty document encodes as
/// `{"elems":[]}`, which is also [`EMPTY_DOCUMENT`], the column default.
#[derive(Serialize, Deserialize)]
struct Wire {
    elems: Vec<Elem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<CollabOp>,
}

/// Lossless: emits every character and every buffered operation.
///
/// Record version history and durable commit-hook payloads snapshot models
/// through `serde` and reconstruct them, so a `Serialize` that emitted only
/// the visible text would destroy the merge history — and with it every
/// concurrent edit that had not yet arrived.
impl Serialize for CollabText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut pending = self.pending.clone();
        pending.sort(); // deterministic bytes for replicas that agree
        Wire {
            elems: self.elems.clone(),
            pending,
        }
        .serialize(serializer)
    }
}

/// Elements one **untrusted** wire document may carry.
///
/// [`CollabText::from_wire`] replays rather than trusts, and that replay is
/// quadratic. Measured on this machine, in release:
///
/// | elements | decode |
/// | --- | --- |
/// | 1 000 | 1.4 ms |
/// | 5 000 | 10.7 ms |
/// | 10 000 | 35.8 ms |
/// | 20 000 | 220 ms |
///
/// 10 000 keeps the worst case inside a few tens of milliseconds. It is the
/// ceiling on what [`Deserialize`] will accept, and therefore what a request
/// body or a sync payload can make the server replay.
pub const MAX_WIRE_ELEMENTS: usize = 10_000;

/// Buffered operations one **untrusted** wire document may carry.
///
/// Much lower than [`MAX_WIRE_ELEMENTS`], because a buffered operation is far
/// more expensive than an element: [`CollabText::drain_pending`] retries the
/// whole buffer every time one integrates, so a causal chain sent in reverse
/// costs a pass per operation. Measured on this machine, in release:
///
/// | pending | bytes | decode |
/// | --- | --- | --- |
/// | 250 | 15 KB | 2.7 ms |
/// | 1 000 | 60 KB | 30 ms |
/// | 2 000 | 122 KB | 117 ms |
/// | 4 000 | 246 KB | 463 ms |
///
/// That is ~12× the cost per byte of the ordinary shape, and it is the shape
/// an attacker sends: a 2 MB body extrapolates to roughly half a minute of
/// blocking CPU. A legitimate payload carries a handful — the buffer holds
/// only what is waiting for a cause still in flight.
pub const MAX_WIRE_PENDING: usize = 1_000;

/// The exact inverse of [`Serialize`].
///
/// A bare string is deliberately refused. Accepting one would let
/// `PUT /api/notes/1` with `{"body": "hi"}` replace a merged document with a
/// fresh one and silently drop every other editor's characters. Use
/// [`CollabText::set_text`] in Rust, or send operations, to change the text.
impl<'de> Deserialize<'de> for CollabText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        // Refuse before replaying, not after: the replay is the cost. This is
        // the untrusted door — a request body, a sync payload — so the
        // bounds apply here rather than in `from_wire`, which also serves
        // `decode_column` reading a column this crate wrote.
        if wire.elems.len() > MAX_WIRE_ELEMENTS {
            return Err(serde::de::Error::custom(format!(
                "collaborative document carries {} elements, over the limit of {MAX_WIRE_ELEMENTS}",
                wire.elems.len(),
            )));
        }
        if wire.pending.len() > MAX_WIRE_PENDING {
            return Err(serde::de::Error::custom(format!(
                "collaborative document carries {} buffered operations, over the limit of {MAX_WIRE_PENDING}",
                wire.pending.len(),
            )));
        }
        Ok(Self::from_wire(wire))
    }
}

impl CollabText {
    /// Rebuild a document from its wire form.
    ///
    /// The elements are **replayed**, not trusted. A stored array reaches this
    /// function from a database column and from a sync payload, so it can
    /// carry a duplicate id or an order no replica would ever have produced.
    /// Copying it in would render one text here and another everywhere else,
    /// and re-encoding would preserve the fault forever. Replaying puts every
    /// character where the merge rule says it goes and drops a repeated id,
    /// so a document is canonical the moment it is read.
    ///
    /// The cost is the documented full-replay cost: quadratic in length, which
    /// is the bound this slice accepts for note-sized fields. Untrusted input
    /// is held to [`MAX_WIRE_ELEMENTS`] and [`MAX_WIRE_PENDING`] by
    /// [`Deserialize`] before it reaches here; this function itself is
    /// unbounded, because [`CollabText::decode_column`] reads a column this
    /// crate wrote and capped on the way in.
    fn from_wire(wire: Wire) -> Self {
        let mut doc = Self::new();
        for elem in &wire.elems {
            doc.apply(CollabOp::Insert {
                id: elem.id.clone(),
                after: elem.after.clone(),
                ch: elem.ch,
            });
        }
        // Tombstones after every insert, so a delete never waits.
        for elem in wire.elems.iter().filter(|e| e.deleted) {
            doc.apply(CollabOp::Delete {
                target: elem.id.clone(),
            });
        }
        // The buffered operations last: one may have become integrable while
        // the document was at rest.
        doc.apply_all(wire.pending);
        doc
    }

    /// Encode for a `TEXT` column.
    #[must_use]
    pub fn encode_column(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| EMPTY_DOCUMENT.to_owned())
    }

    /// Decode a `TEXT` column.
    ///
    /// A value that is not a CRDT document is read as plain prose and seeded
    /// under an [`IMPORT_ACTOR`] namespace, so a column promoted to
    /// `#[collaborative]` after the table already had rows keeps its content.
    ///
    /// Three spellings of "empty" all give an empty document: the empty
    /// string, whitespace, and a bare `{}`. `{}` is listed because it is the
    /// default `#[translatable]` uses and the one a hand-written migration is
    /// most likely to copy; without this it would read as two characters of
    /// prose that somebody then has to delete.
    #[must_use]
    pub fn decode_column(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "{}" {
            return Self::new();
        }
        serde_json::from_str::<Wire>(raw).map_or_else(
            |_| Self::from_text(&import_actor_for(raw), raw),
            Self::from_wire,
        )
    }
}

// ── Diesel codec (`TEXT` on both backends) ───────────────────────────────────

#[cfg(feature = "db")]
mod db {
    use diesel::backend::Backend;
    use diesel::deserialize::{self, FromSql};
    use diesel::serialize::{self, IsNull, Output, ToSql};
    use diesel::sql_types::Text;

    use super::CollabText;

    impl ToSql<Text, diesel::sqlite::Sqlite> for CollabText {
        fn to_sql<'b>(
            &'b self,
            out: &mut Output<'b, '_, diesel::sqlite::Sqlite>,
        ) -> serialize::Result {
            out.set_value(self.encode_column());
            Ok(IsNull::No)
        }
    }

    impl ToSql<Text, diesel::pg::Pg> for CollabText {
        fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, diesel::pg::Pg>) -> serialize::Result {
            use std::io::Write as _;
            out.write_all(self.encode_column().as_bytes())?;
            Ok(IsNull::No)
        }
    }

    impl<DB> FromSql<Text, DB> for CollabText
    where
        DB: Backend,
        String: FromSql<Text, DB>,
    {
        fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
            Ok(Self::decode_column(&String::from_sql(bytes)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two replicas that see the same ops in opposite orders converge.
    #[test]
    fn opposite_delivery_orders_converge() {
        let mut a = CollabText::new();
        let a_ops = a.insert("a", 0, "hello");
        let mut b = CollabText::new();
        let b_ops = b.insert("b", 0, "world");

        for op in b_ops {
            a.apply(op);
        }
        for op in a_ops.into_iter().rev() {
            b.apply(op);
        }

        assert_eq!(a.text(), b.text(), "replicas converge");
        assert_eq!(a.len(), 10, "no character is lost");
    }

    /// An op that arrives before the op it depends on waits, then integrates.
    #[test]
    fn out_of_order_ops_are_buffered_not_dropped() {
        let mut source = CollabText::new();
        let ops = source.insert("a", 0, "abc");

        let mut target = CollabText::new();
        // Deliver last-to-first: each op's left neighbour is still missing.
        for op in ops.into_iter().rev() {
            target.apply(op);
        }
        assert_eq!(target.text(), "abc", "buffered ops integrate on arrival");
    }

    /// Applying the same op twice does not duplicate the character.
    #[test]
    fn duplicate_ops_are_idempotent() {
        let mut source = CollabText::new();
        let ops = source.insert("a", 0, "hi");

        let mut target = CollabText::new();
        for op in ops.clone() {
            target.apply(op);
        }
        for op in ops {
            target.apply(op);
        }
        assert_eq!(target.text(), "hi");
    }

    /// A batch pays once for a character it also deletes.
    ///
    /// A reconnecting replica replays characters it typed and then removed.
    /// Weighing each operation against the pre-batch state charged the insert
    /// and the delete separately, and refused a history that fits.
    #[test]
    fn a_batch_pays_once_for_a_character_it_also_deletes() {
        let mut doc = CollabText::new();
        let id = OpId::new(1, "ada");
        let batch = vec![
            CollabOp::Insert {
                id: id.clone(),
                after: None,
                ch: 'x',
            },
            CollabOp::Insert {
                id: id.clone(),
                after: None,
                ch: 'x',
            },
            CollabOp::Delete { target: id },
        ];
        assert_eq!(doc.novel_count(batch.iter()), 1);

        // The count is what the batch really costs.
        doc.apply_all(batch);
        assert_eq!(doc.element_count(), 1);
        assert_eq!(doc.pending_len(), 0);

        // A delete the batch does NOT satisfy still costs: it holds the
        // causal buffer until its target arrives.
        let orphan = [CollabOp::Delete {
            target: OpId::new(9, "bob"),
        }];
        assert_eq!(doc.novel_count(orphan.iter()), 1);
    }

    /// An untrusted document past the element ceiling is refused, not
    /// replayed. The replay is quadratic, so the refusal has to come first.
    #[test]
    fn an_oversized_wire_document_is_refused() {
        let elems: Vec<serde_json::Value> = (1..=MAX_WIRE_ELEMENTS + 1)
            .map(|n| serde_json::json!({ "id": format!("{n}@evil"), "ch": "x" }))
            .collect();
        let json = serde_json::json!({ "elems": elems, "pending": [] }).to_string();

        let refused = serde_json::from_str::<CollabText>(&json).expect_err("over the limit");
        assert!(
            refused.to_string().contains("over the limit"),
            "the refusal says why: {refused}"
        );
    }

    /// The buffer ceiling is separate and much lower: a causal chain sent in
    /// reverse costs a drain pass per operation, which is what makes a small
    /// payload expensive.
    #[test]
    fn an_oversized_pending_buffer_is_refused() {
        let pending: Vec<serde_json::Value> = (1..=MAX_WIRE_PENDING + 1)
            .map(|n| {
                serde_json::json!({
                    "op": "insert",
                    "id": format!("{n}@evil"),
                    "after": "99999@ghost",
                    "ch": "x",
                })
            })
            .collect();
        let json = serde_json::json!({ "elems": [], "pending": pending }).to_string();

        let refused = serde_json::from_str::<CollabText>(&json).expect_err("over the limit");
        assert!(
            refused.to_string().contains("buffered operations"),
            "the refusal names the buffer: {refused}"
        );
    }

    /// A document at the ceiling still round trips, so the bound cannot be
    /// reached by anything the hub itself produces.
    #[test]
    fn a_document_at_the_ceiling_still_decodes() {
        let mut doc = CollabText::new();
        doc.insert("ada", 0, &"x".repeat(MAX_WIRE_ELEMENTS));
        let json = serde_json::to_string(&doc).expect("encode");

        let back: CollabText = serde_json::from_str(&json).expect("at the ceiling, not over it");
        assert_eq!(back.len(), MAX_WIRE_ELEMENTS);
    }

    /// One id with many unknown anchors costs what it really occupies.
    ///
    /// `buffered` keys on the whole operation, so each variant is its own
    /// buffer entry. Counting distinct ids charged one and let the rest past
    /// the document limit — an unbounded buffer from a single batch.
    #[test]
    fn a_batch_pays_for_every_buffered_variant_of_one_id() {
        let doc = CollabText::new();
        let id = OpId::new(1, "ada");
        let batch: Vec<CollabOp> = (0..5)
            .map(|n| CollabOp::Insert {
                id: id.clone(),
                after: Some(OpId::new(100 + n, "ghost")),
                ch: 'x',
            })
            .collect();

        assert_eq!(doc.novel_count(batch.iter()), 5);

        // The count is what the batch really costs: every anchor is unknown,
        // so every variant lands in the buffer.
        let mut doc = doc;
        doc.apply_all(batch);
        assert_eq!(doc.pending_len(), 5);
    }

    /// A delete on one replica and an insert on another both survive.
    #[test]
    fn concurrent_delete_and_insert_both_apply() {
        let mut a = CollabText::new();
        a.insert("a", 0, "abc");
        let mut b = a.clone();

        let del = a.remove(1, 1); // "ac"
        let ins = b.insert("b", 3, "!"); // "abc!"

        for op in ins {
            a.apply(op);
        }
        for op in del {
            b.apply(op);
        }
        assert_eq!(a.text(), "ac!");
        assert_eq!(b.text(), "ac!");
    }

    /// `ops()` round-trips a whole document into another replica.
    #[test]
    fn ops_reproduce_the_document() {
        let mut a = CollabText::new();
        a.insert("a", 0, "abcd");
        a.remove(1, 2);

        let mut b = CollabText::new();
        for op in a.ops() {
            b.apply(op);
        }
        assert_eq!(b.text(), a.text());
        assert_eq!(b.text(), "ad");
    }

    /// Insertion at an interior index lands where the caller meant.
    #[test]
    fn insert_at_index_preserves_intent() {
        let mut doc = CollabText::new();
        doc.insert("a", 0, "ac");
        doc.insert("a", 1, "b");
        assert_eq!(doc.text(), "abc");
        assert!(!doc.is_empty());
    }

    /// The stored form keeps every character, not just the visible text.
    #[test]
    fn serde_round_trip_is_lossless() {
        let mut doc = CollabText::new();
        doc.insert("a", 0, "abc");
        doc.remove(1, 1);
        let json = serde_json::to_string(&doc).expect("encode");
        let back: CollabText = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, doc);
        assert_eq!(back.text(), "ac");
        // The tombstone survives, so a late delete cannot resurrect it.
        assert_eq!(back.elements().len(), 3);
    }

    /// A bare string is refused: it would replace a merged document.
    #[test]
    fn a_bare_string_is_refused() {
        let err = serde_json::from_str::<CollabText>("\"hello\"").unwrap_err();
        assert!(
            err.to_string().contains("invalid type"),
            "a string must not decode into a document: {err}"
        );
    }

    /// An unrelated JSON object is **not** an empty document.
    ///
    /// With `elems` defaulted, every object would decode as "no text", and a
    /// merge against it would replace real characters with nothing.
    #[test]
    fn an_unrelated_json_object_does_not_decode_as_an_empty_document() {
        assert!(serde_json::from_str::<CollabText>(r#"{"foo":"bar"}"#).is_err());
        assert!(serde_json::from_str::<CollabText>("{}").is_err());
        // The real empty document still decodes, and it is the column default.
        let empty: CollabText =
            serde_json::from_str(EMPTY_DOCUMENT).expect("the empty document decodes");
        assert_eq!(empty, CollabText::new());
        assert_eq!(CollabText::new().encode_column(), EMPTY_DOCUMENT);
        assert_eq!(CollabText::decode_column(EMPTY_DOCUMENT), CollabText::new());
    }

    /// Two replicas that hold the same operations encode the same bytes.
    #[test]
    fn convergent_replicas_encode_identical_bytes() {
        let mut a = CollabText::new();
        let a_ops = a.insert("a", 0, "left");
        let mut b = CollabText::new();
        let b_ops = b.insert("b", 0, "right");
        for op in b_ops {
            a.apply(op);
        }
        for op in a_ops {
            b.apply(op);
        }
        assert_eq!(
            serde_json::to_string(&a).expect("encode a"),
            serde_json::to_string(&b).expect("encode b"),
        );
    }

    /// `set_text` keeps the untouched span, so a concurrent edit elsewhere
    /// survives a whole-field form post.
    #[test]
    fn set_text_edits_only_the_changed_span() {
        let mut a = CollabText::from_text("seed", "the quick fox");
        let mut b = a.clone();

        let a_ops = a.set_text("a", "the quick brown fox"); // insert mid-string
        let b_ops = b.set_text("b", "THE quick fox"); // rewrite the head

        for op in b_ops {
            a.apply(op);
        }
        for op in a_ops {
            b.apply(op);
        }
        assert_eq!(a.text(), b.text(), "replicas converge");
        assert_eq!(a.text(), "THE quick brown fox");
    }

    /// An id-anchored insert lands next to its neighbour even when the
    /// document moved under it.
    #[test]
    fn id_anchored_insert_preserves_intent_against_a_stale_index() {
        let mut server = CollabText::from_text("seed", "world");
        // A client resolved "after the 'w'" before anyone else typed.
        let anchor = server.id_at(0).expect("first character");
        // Meanwhile another editor prepends.
        server.insert("other", 0, "hello ");
        // The stale client's edit still lands after the 'w', not at index 1.
        server.insert_after("client", Some(&anchor), "-");
        assert_eq!(server.text(), "hello w-orld");
    }

    /// A column holding prose rather than a document keeps its content.
    #[test]
    fn a_plain_text_column_decodes_as_seeded_prose() {
        let doc = CollabText::decode_column("legacy note");
        assert_eq!(doc.text(), "legacy note");
        assert_eq!(CollabText::decode_column("").text(), "");
        // Deterministic: every replica reading the row builds the same ids.
        assert_eq!(doc, CollabText::decode_column("legacy note"));
    }

    /// Merging is commutative, associative and idempotent.
    #[test]
    fn merge_is_order_independent_and_idempotent() {
        let base = CollabText::from_text("seed", "base");
        let mut a = base.clone();
        a.insert("a", 0, "A");
        let mut b = base.clone();
        b.insert("b", 4, "B");
        let mut c = base;
        c.remove(0, 1);

        let mut left = a.clone();
        left.merge(&b);
        left.merge(&c);
        let mut right = c.clone();
        right.merge(&b);
        right.merge(&a);
        right.merge(&a); // idempotent

        assert_eq!(left, right);
        assert_eq!(left.text(), right.text());
    }

    /// A delete whose target has not arrived waits, then takes effect.
    #[test]
    fn a_delete_for_an_unknown_character_waits() {
        let mut source = CollabText::new();
        let ins = source.insert("a", 0, "x");
        let del = source.remove(0, 1);

        let mut target = CollabText::new();
        assert!(!target.apply(del[0].clone()), "delete has no target yet");
        assert_eq!(target.pending_len(), 1);
        target.apply(ins[0].clone());
        assert_eq!(target.pending_len(), 0, "the buffer drains");
        assert_eq!(target.text(), "");
    }

    /// A reference a sender supplies must never move the Lamport clock.
    ///
    /// Trusting one lets a single message push the clock to `u64::MAX`, after
    /// which the next character either panics the mint or wraps into an id
    /// that already exists and is silently dropped.
    #[test]
    fn a_referenced_id_from_the_future_does_not_move_the_clock() {
        let mut doc = CollabText::from_text("seed", "hi");
        let before = doc.clock();

        doc.apply(CollabOp::Insert {
            id: OpId::new(before + 1, "x"),
            after: Some(OpId::new(u64::MAX, "x")),
            ch: 'z',
        });
        doc.apply(CollabOp::Delete {
            target: OpId::new(u64::MAX, "x"),
        });

        assert_eq!(
            doc.clock(),
            before + 1,
            "only the operation's own id advances the clock"
        );
        // And the document keeps working.
        doc.insert("seed", 2, "!");
        assert_eq!(doc.text(), "hi!");
    }

    /// A stored document is replayed, not trusted: a duplicate id is dropped
    /// and a bad order is re-placed, so a hand-edited column cannot make one
    /// replica render text no other replica agrees with.
    #[test]
    fn a_stored_document_is_rebuilt_rather_than_trusted() {
        // Two elements claiming the same id.
        let duplicated = r#"{"elems":[
            {"id":"1@a","ch":"A"},
            {"id":"1@a","ch":"Z"}
        ]}"#;
        let doc = CollabText::decode_column(duplicated);
        assert_eq!(doc.text(), "A", "the repeated id is dropped");

        // An order the merge rule would never produce: two concurrent
        // children of `1@a` listed with the lower id first.
        let misordered = r#"{"elems":[
            {"id":"1@a","ch":"A"},
            {"id":"5@b","after":"1@a","ch":"B"},
            {"id":"9@c","after":"1@a","ch":"C"}
        ]}"#;
        let doc = CollabText::decode_column(misordered);
        let replica = {
            let mut fresh = CollabText::new();
            fresh.apply_all(doc.ops());
            fresh
        };
        assert_eq!(
            doc.text(),
            replica.text(),
            "a decoded document agrees with a replica built from its own ops"
        );
        assert_eq!(doc, replica);
    }

    /// Seeding different prose must not mint colliding ids: the merge dedups
    /// by id, so a collision would silently discard characters.
    #[test]
    fn imported_prose_seeds_do_not_collide() {
        let mut hello = CollabText::decode_column("hello");
        hello.merge(&CollabText::decode_column("goodbye"));
        assert_eq!(
            hello.len(),
            "hello".len() + "goodbye".len(),
            "no character was dropped as already-seen: {:?}",
            hello.text()
        );

        // Still deterministic: the same prose always seeds the same way.
        assert_eq!(
            CollabText::decode_column("hello"),
            CollabText::decode_column("hello")
        );
    }

    /// Every spelling of "empty" reads as an empty document, including the
    /// `{}` a hand-written migration is most likely to copy.
    #[test]
    fn every_spelling_of_empty_decodes_to_an_empty_document() {
        for raw in ["", "   ", "{}", EMPTY_DOCUMENT] {
            assert_eq!(
                CollabText::decode_column(raw),
                CollabText::new(),
                "{raw:?} must decode as the empty document"
            );
        }
    }

    /// `remove_known` drops an id the replica has never seen; `remove_ids`
    /// buffers it. The hub uses the first so one client cannot pre-delete
    /// another's future characters.
    #[test]
    fn remove_known_refuses_a_character_that_does_not_exist_yet() {
        let mut doc = CollabText::from_text("seed", "ab");
        let future = OpId::new(doc.clock() + 50, "victim");

        assert!(doc.remove_known(std::slice::from_ref(&future)).is_empty());
        assert_eq!(doc.pending_len(), 0, "nothing was buffered");

        // The buffering form is still available for replica merges.
        doc.remove_ids(&[future]);
        assert_eq!(doc.pending_len(), 1);
    }

    /// Ids render and parse round-trip, including an actor containing `@`.
    #[test]
    fn op_ids_round_trip_through_their_string_form() {
        let id = OpId::new(7, "ada@host");
        assert_eq!(id.to_string(), "7@ada@host");
        assert_eq!(id.to_string().parse::<OpId>().expect("parse"), id);
        assert!("nope".parse::<OpId>().is_err());
        assert!("7@".parse::<OpId>().is_err());
    }
}
