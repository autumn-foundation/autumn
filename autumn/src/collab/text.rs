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
//! server.insert("server", 0, "hello world").expect("collab edit refused");
//!
//! // Two editors branch from the same state.
//! let mut ada = server.clone();
//! let mut linus = server.clone();
//! let from_ada = ada.insert("ada", 5, ",").expect("collab edit refused");        // "hello, world"
//! let from_linus = linus.insert("linus", 11, "!").expect("collab edit refused"); // "hello world!"
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

use std::collections::{HashMap, HashSet};
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

/// Error returned when an edit cannot be minted.
///
/// Both variants mean the edit was refused **whole**. Nothing is applied and
/// nothing is returned to broadcast: a partially-applied edit is worse than a
/// refused one, because the editor that sent it keeps a provisional character
/// the authority does not hold and every later edit anchored to it comes back
/// unknown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CollabEditError {
    /// The actor was empty.
    ///
    /// [`OpId::from_str`] rejects an empty actor, so an id minted with one
    /// formats as `"1@"` and cannot be parsed back: the document encodes but
    /// its own decoder refuses it, and
    /// [`decode_column`](CollabText::decode_column) then reads the encoded
    /// JSON as legacy plain text and shows it as the document's content.
    /// Refusing at the mint keeps every id the safe API produces parseable.
    ///
    /// [`OpId::from_str`]: std::str::FromStr
    #[error("actor must not be empty: an id minted with an empty actor cannot be parsed back")]
    EmptyActor,
    /// The counter space is exhausted.
    ///
    /// Reachable without 2^53 local keystrokes: [`CollabText::apply`] accepts
    /// a peer id of `MAX_COUNTER - 1`, which pins the clock one step below
    /// the ceiling, so a single hostile operation can put a document here.
    #[error(
        "the counter space is exhausted: {wanted} more character(s) would pass the ceiling, \
         with only {available} left"
    )]
    CounterExhausted {
        /// Characters the edit asked to mint.
        wanted: usize,
        /// Characters the counter space still has room for.
        available: u64,
    },
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
    ///
    /// # Errors
    ///
    /// Returns [`CollabEditError::EmptyActor`] when `actor` is empty. A fresh
    /// document starts at counter zero, so the ceiling is out of reach here
    /// for any `text` that fits in memory.
    pub fn from_text(actor: &str, text: &str) -> Result<Self, CollabEditError> {
        let mut doc = Self::new();
        doc.insert(actor, 0, text)?;
        Ok(doc)
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
    /// nothing when its target is already here **or lands in the same batch**
    /// — only a delete left with nothing to tombstone occupies the buffer.
    /// Judging each operation against the pre-batch state alone would charge
    /// an insert and its own delete twice over and refuse a history that fits.
    ///
    /// "Lands", not "appears": an insert whose own anchor is unknown stays in
    /// the buffer, and the delete waiting on it stays there too, so a batch
    /// carrying both would otherwise pay for one slot and occupy two.
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

        // Pass one: the characters this batch brings that will actually take
        // their place. Naming an id is not enough — an insert whose own anchor
        // is unknown stays in the buffer, and so does any delete waiting on
        // it, so treating it as satisfied charged two slots as one. It is a
        // fixed point because the batch can carry a chain: each insert that
        // lands may be the anchor the next one was waiting for.
        let mut integrable: HashSet<&OpId> = HashSet::new();
        loop {
            let mut grew = false;
            for op in &ops {
                let CollabOp::Insert { id, after, .. } = op else {
                    continue;
                };
                if self.index.contains(id) || integrable.contains(id) {
                    continue;
                }
                let anchored = after.as_ref().is_none_or(|anchor| {
                    self.index.contains(anchor) || integrable.contains(anchor)
                });
                if anchored {
                    integrable.insert(id);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }

        // Pass two: the distinct operations that will occupy a slot.
        let charged: HashSet<&&CollabOp> = ops
            .iter()
            .filter(|op| match op {
                // An id already here integrates as a no-op and stores nothing.
                CollabOp::Insert { id, .. } => !self.index.contains(id),
                // A target the batch *lands* is tombstoned, not buffered.
                CollabOp::Delete { target } => {
                    !self.index.contains(target) && !integrable.contains(target)
                }
            })
            // Anything already buffered is paid for.
            .filter(|op| !self.buffered.contains(**op))
            .collect();

        // Credit what the batch frees. A buffered delete occupies a slot only
        // until its target arrives: applying the insert tombstones it and the
        // delete leaves the buffer, so the pair costs one element, not two.
        // Charging both made a delete for an unknown id able to lock its own
        // insert out of a full document — permanently, since the delete can
        // never drain without it.
        //
        // Only deletes are credited. A buffered *insert* that integrates
        // moves from the buffer to the elements and occupies a slot either
        // way, so it frees nothing.
        let freed = self
            .pending
            .iter()
            .filter(|op| match op {
                CollabOp::Delete { target } => integrable.contains(target),
                CollabOp::Insert { .. } => false,
            })
            .count();

        charged.len().saturating_sub(freed)
    }

    /// Whether this operation would change nothing: the character is already
    /// here, or the tombstone is already set.
    ///
    /// [`apply`](Self::apply) cannot say — it answers `true` for an
    /// idempotent replay exactly as it does for a first arrival, because from
    /// the document's side both leave it correct. A caller deciding what to
    /// forward needs the difference: a reconnecting peer replays its whole
    /// history, and passing that on is a fan-out of everything to everyone,
    /// every time.
    #[must_use]
    pub fn already_applied(&self, op: &CollabOp) -> bool {
        match op {
            CollabOp::Insert { id, .. } => self.index.contains(id),
            CollabOp::Delete { target } => self
                .position_of(target)
                .is_some_and(|at| self.elems[at].deleted),
        }
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
    ///
    /// # Errors
    ///
    /// Returns [`CollabEditError`] when `actor` is empty or the counter space
    /// cannot seat the whole of `text`. Nothing is applied either way.
    pub fn insert(
        &mut self,
        actor: &str,
        index: usize,
        text: &str,
    ) -> Result<Vec<CollabOp>, CollabEditError> {
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
    ///
    /// # Errors
    ///
    /// Returns [`CollabEditError`] when `actor` is empty or the counter space
    /// cannot seat the whole of `text`. Nothing is applied either way.
    pub fn insert_after(
        &mut self,
        actor: &str,
        after: Option<&OpId>,
        text: &str,
    ) -> Result<Vec<CollabOp>, CollabEditError> {
        let wanted = text.chars().count();
        Self::preflight(actor, self.clock, wanted)?;
        Ok(self.mint(actor, after, text))
    }

    /// Mint one operation per character of `text`, without the preflight.
    ///
    /// Private, and every caller proves the preconditions itself:
    /// [`insert_after`](Self::insert_after) by the preflight it just ran, and
    /// [`seeded`](Self::seeded) because it builds a fresh document under an
    /// actor this module generates. Keeping it separate is what lets the
    /// import paths — the Diesel decode among them — seed a document without
    /// a validation they cannot fail and would have to unwrap.
    fn mint(&mut self, actor: &str, after: Option<&OpId>, text: &str) -> Vec<CollabOp> {
        let wanted = text.chars().count();
        let mut ops = Vec::with_capacity(wanted);
        let mut left = after.cloned();
        for ch in text.chars() {
            // Stop below the reserved ceiling, not at it. `apply` refuses a
            // counter of `MAX_COUNTER` or more, so minting one would return
            // an operation this very replica rejects — and the hub would
            // broadcast a character the authority does not hold.
            //
            // Unreachable after the preflight above, which is the point: this
            // loop used to `break` here and hand back a *prefix* of the
            // requested insert, silently. The editor kept a provisional
            // character the authority never took and went read-only for good,
            // because its next edit anchored to one the hub had refused.
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

    /// A fresh document holding `text`, under an actor this module generated.
    ///
    /// The import path. It cannot refuse, which is the point: it runs from
    /// `From<&str>` and from the Diesel decode, neither of which has anywhere
    /// to put an error.
    fn seeded(actor: &str, text: &str) -> Self {
        let mut doc = Self::new();
        doc.mint(actor, None, text);
        doc
    }

    /// Refuse an edit that cannot be minted whole.
    ///
    /// Called before anything is mutated — `set_text` in particular removes
    /// the replaced span before inserting its replacement, so a mid-edit
    /// refusal there would tombstone text and put back only part of it.
    const fn preflight(actor: &str, clock: u64, wanted: usize) -> Result<(), CollabEditError> {
        if actor.is_empty() {
            return Err(CollabEditError::EmptyActor);
        }
        // `insert_after` mints `wanted` counters above `clock` and `apply`
        // refuses `MAX_COUNTER` or more, so the last one must land below it.
        let available = MAX_COUNTER.saturating_sub(clock).saturating_sub(1);
        if wanted as u64 > available {
            return Err(CollabEditError::CounterExhausted { wanted, available });
        }
        Ok(())
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
    ///
    /// # Errors
    ///
    /// Returns [`CollabEditError`] when `actor` is empty or the counter space
    /// cannot seat the replacement. Nothing is applied either way — in
    /// particular the replaced span is not tombstoned.
    pub fn set_text(
        &mut self,
        actor: &str,
        new_text: &str,
    ) -> Result<Vec<CollabOp>, CollabEditError> {
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

        let added: String = new[prefix..new.len() - suffix].iter().collect();
        // Before the remove below, not after it. The remove tombstones the
        // replaced span, so refusing between the two would delete the old
        // text and put back only as much of the new as the counter space
        // happened to seat.
        Self::preflight(actor, self.clock, added.chars().count())?;

        let mut ops = self.remove(prefix, old.len() - prefix - suffix);
        if !added.is_empty() {
            ops.extend(self.insert(actor, prefix, &added)?);
        }
        Ok(ops)
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
                    // Idempotent replay — unless it is not the same operation.
                    // An id is supposed to name one character typed in one
                    // place; the same id carrying different text means a
                    // replica minted it twice, which the model has no answer
                    // for. Keep what is here and say so.
                    //
                    // This is first-wins, so two documents that disagree this
                    // way merge to whichever was merged into. Repairing that
                    // would mean repositioning the element, and its position
                    // comes from an anchor the conflicting copy disagrees
                    // about — so nothing short of replaying the whole document
                    // converges. `Deserialize` refuses such a document at the
                    // door instead, which is where a crafted one arrives.
                    if let Some(pos) = self.position_of(id) {
                        let held = &self.elems[pos];
                        if held.ch != *ch || held.after != *after {
                            tracing::warn!(
                                %id,
                                held = %held.ch,
                                incoming = %ch,
                                "collab: an id was reused for a different character; \
                                 keeping the one already integrated"
                            );
                        }
                    }
                    return true;
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
        Self::seeded(&import_actor_for(text), text)
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
        // An id at or past the ceiling is one `apply` drops on the floor. The
        // replay ignores that, so the document would deserialize "fine" minus
        // those characters — the server quietly deleting text from a document
        // it accepted. Refuse the whole thing instead.
        if let Some(id) = wire
            .elems
            .iter()
            .map(|elem| &elem.id)
            .find(|id| id.counter >= MAX_COUNTER)
        {
            return Err(serde::de::Error::custom(format!(
                "collaborative document carries the unusable id {id}: \
                 counter {} is at or past the ceiling of {MAX_COUNTER}",
                id.counter,
            )));
        }
        // One id, two different characters. An id names one character typed
        // in one place; reuse makes the document's own text depend on the
        // order it is read in, and makes a merge with it depend on which side
        // went first. In memory the replay can only keep the first and say so,
        // because the element's position comes from an anchor the other copy
        // disagrees about — so this door is where such a document is stopped.
        let mut seen: HashMap<&OpId, (&Option<OpId>, char)> =
            HashMap::with_capacity(wire.elems.len());
        for elem in &wire.elems {
            if let Some((after, ch)) = seen.insert(&elem.id, (&elem.after, elem.ch))
                && (*after != elem.after || ch != elem.ch)
            {
                return Err(serde::de::Error::custom(format!(
                    "collaborative document reuses the id {} for two different characters",
                    elem.id,
                )));
            }
        }
        if let Some(op) = wire
            .pending
            .iter()
            .find(|op| op.minted_counter() >= MAX_COUNTER)
        {
            return Err(serde::de::Error::custom(format!(
                "collaborative document buffers an unusable operation: counter {} \
                 is at or past the ceiling of {MAX_COUNTER}",
                op.minted_counter(),
            )));
        }
        // Replay under a cap. The order is not the problem — a scrambled
        // store replaying back to canonical is a guarantee this type makes —
        // but the buffer it passes through is. Ten thousand elements in
        // reverse causal order all land in the causal buffer, ten times what
        // `MAX_WIRE_PENDING` allows a document to be read with, and the drain
        // rescans that buffer every time one integrates. Bounding the buffer
        // during the replay bounds both the memory and that quadratic cost,
        // and it refuses exactly the documents that could not be read back
        // afterwards anyway.
        Self::replay_bounded(wire, MAX_WIRE_PENDING).map_err(|held| {
            serde::de::Error::custom(format!(
                "collaborative document needs {held} buffered operations to replay, \
                 over the limit of {MAX_WIRE_PENDING}"
            ))
        })
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
        Self::replay(wire, None)
    }

    /// [`from_wire`](Self::from_wire), refusing once the causal buffer passes
    /// `max_pending`.
    ///
    /// Returns the size the buffer reached. Used by [`Deserialize`], where the
    /// document is untrusted and the replay is the cost.
    fn replay_bounded(wire: Wire, max_pending: usize) -> Result<Self, usize> {
        let doc = Self::replay(wire, Some(max_pending));
        if doc.pending.len() > max_pending {
            return Err(doc.pending.len());
        }
        Ok(doc)
    }

    fn replay(wire: Wire, max_pending: Option<usize>) -> Self {
        // `apply` drops an id at or past the ceiling and says so only in a
        // return value this replay ignores, so a document carrying one comes
        // back short of those characters. `Deserialize` refuses such a
        // document outright, but `decode_column` reads a column rather than a
        // request and has no such door to close: refusing would strand the
        // row, and reading it as prose would be worse. So it is read, and the
        // loss is said out loud — silence here would let the next
        // `encode_column` write the truncated version back as canonical.
        let unusable = wire
            .elems
            .iter()
            .filter(|elem| elem.id.counter >= MAX_COUNTER)
            .count()
            + wire
                .pending
                .iter()
                .filter(|op| op.minted_counter() >= MAX_COUNTER)
                .count();
        if unusable > 0 {
            tracing::warn!(
                unusable,
                ceiling = MAX_COUNTER,
                "collab: stored document carries ids past the counter ceiling; \
                 they cannot be replayed and are dropped"
            );
        }
        let mut doc = Self::new();
        // Stop early when a cap is set: a replay that has already blown the
        // buffer is refused whatever the rest of the array holds, and every
        // further element makes the drain scan more.
        let over = |doc: &Self| max_pending.is_some_and(|max| doc.pending.len() > max);
        for elem in &wire.elems {
            if over(&doc) {
                return doc;
            }
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
        for op in wire.pending {
            if over(&doc) {
                return doc;
            }
            doc.apply(op);
        }
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
            |_| Self::seeded(&import_actor_for(raw), raw),
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
        let a_ops = a.insert("a", 0, "hello").expect("collab edit refused");
        let mut b = CollabText::new();
        let b_ops = b.insert("b", 0, "world").expect("collab edit refused");

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
        let ops = source.insert("a", 0, "abc").expect("collab edit refused");

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
        let ops = source.insert("a", 0, "hi").expect("collab edit refused");

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

    /// A buffered delete does not lock its own insert out of a full document.
    ///
    /// The delete waits for the character it names, so it holds a slot. The
    /// insert that would free it was charged as if it were new, and a
    /// document at its limit refused it — leaving the delete stuck forever,
    /// because nothing else can ever drain it.
    #[test]
    fn an_insert_that_drains_a_buffered_delete_is_free() {
        let mut doc = CollabText::new();
        let target = OpId::new(7, "ada");
        doc.apply(CollabOp::Delete {
            target: target.clone(),
        });
        assert_eq!(doc.pending_len(), 1, "the delete is waiting for its target");

        let batch = [CollabOp::Insert {
            id: target,
            after: None,
            ch: 'x',
        }];
        assert_eq!(
            doc.novel_count(batch.iter()),
            0,
            "one slot out, one slot in: the pair costs what the delete already holds"
        );

        // And that is what it really costs.
        doc.apply_all(batch);
        assert_eq!(doc.element_count(), 1);
        assert_eq!(doc.pending_len(), 0);
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

    /// A delete is only free when the insert it waits on actually lands.
    ///
    /// Naming the id in the batch is not enough: an insert whose own anchor is
    /// unknown stays in the buffer, and the delete stays with it. Charging the
    /// pair as one let a batch occupy two slots while paying for one, which is
    /// how a document gets past its own limit — and past `MAX_WIRE_PENDING`,
    /// into a state its own deserializer refuses.
    #[test]
    fn a_delete_waiting_on_an_unanchored_insert_is_charged() {
        let doc = CollabText::new();
        let target = OpId::new(2, "ada");
        let batch = [
            CollabOp::Delete {
                target: target.clone(),
            },
            CollabOp::Insert {
                id: target,
                // An anchor the document has never seen: this insert waits.
                after: Some(OpId::new(99, "ghost")),
                ch: 'b',
            },
        ];

        assert_eq!(
            doc.novel_count(batch.iter()),
            2,
            "neither one lands, so both sit in the buffer"
        );

        // And that is what it really costs.
        let mut doc = doc;
        doc.apply_all(batch);
        assert_eq!(doc.pending_len(), 2);
    }

    /// The waiver still applies down a chain the batch carries whole.
    #[test]
    fn a_delete_is_free_when_its_insert_lands_behind_another_in_the_batch() {
        let doc = CollabText::new();
        let first = OpId::new(1, "ada");
        let second = OpId::new(2, "ada");
        let batch = [
            // Deliberately out of causal order: the fixed point has to find
            // that `second` lands only once `first` does.
            CollabOp::Delete {
                target: second.clone(),
            },
            CollabOp::Insert {
                id: second,
                after: Some(first.clone()),
                ch: 'b',
            },
            CollabOp::Insert {
                id: first,
                after: None,
                ch: 'a',
            },
        ];

        assert_eq!(
            doc.novel_count(batch.iter()),
            2,
            "two characters land; the delete tombstones one of them for free"
        );

        let mut doc = doc;
        doc.apply_all(batch);
        assert_eq!(doc.element_count(), 2);
        assert_eq!(doc.pending_len(), 0);
        assert_eq!(doc.text(), "a");
    }

    /// A document that would need too large a causal buffer to replay is
    /// refused, however its elements are ordered.
    ///
    /// The element bound alone did not cover this: ten thousand elements with
    /// anchors that never arrive pass it, then all land in a buffer allowed a
    /// thousand — and the drain rescans that buffer every time one
    /// integrates, which is the quadratic shape the lower bound was measured
    /// against. Such a document could not have been read back afterwards
    /// either: its own `pending` would be over the limit.
    #[test]
    fn a_document_that_would_overflow_the_replay_buffer_is_refused() {
        let elems: Vec<serde_json::Value> = (1..=MAX_WIRE_PENDING + 1)
            .map(|n| {
                serde_json::json!({
                    "id": format!("{n}@evil"),
                    // An anchor that is in no document anywhere.
                    "after": format!("{}@ghost", 900_000 + n),
                    "ch": "x",
                })
            })
            .collect();
        let json = serde_json::json!({ "elems": elems, "pending": [] }).to_string();

        let refused = serde_json::from_str::<CollabText>(&json).expect_err("over the limit");
        assert!(
            refused
                .to_string()
                .contains("buffered operations to replay"),
            "the refusal says what it would have cost: {refused}"
        );
    }

    /// A scrambled store still replays back to canonical, so long as the
    /// buffer it passes through fits. That is a guarantee, not an accident.
    #[test]
    fn a_scrambled_store_within_the_buffer_still_re_canonicalizes() {
        let mut doc = CollabText::new();
        doc.insert("ada", 0, "hello").expect("collab edit refused");
        let encoded = serde_json::to_string(&doc).expect("encode");

        let scrambled = {
            let mut value: serde_json::Value = serde_json::from_str(&encoded).expect("parse");
            value["elems"].as_array_mut().expect("elems").reverse();
            serde_json::to_string(&value).expect("re-encode")
        };

        let repaired: CollabText = serde_json::from_str(&scrambled).expect("decode");
        assert_eq!(
            repaired.text(),
            "hello",
            "put back in the order the merge rule says"
        );
    }

    /// An empty actor mints `"1@"`, which the id parser refuses — so the
    /// document would encode and then fail to decode, and `decode_column`
    /// would show its own JSON as the text. Refuse at the mint instead.
    #[test]
    fn an_empty_actor_is_refused_before_it_mints_an_unparseable_id() {
        assert_eq!(
            CollabText::from_text("", "x").expect_err("empty actor"),
            CollabEditError::EmptyActor,
        );

        let mut doc = CollabText::new();
        assert_eq!(
            doc.insert("", 0, "x").expect_err("empty actor"),
            CollabEditError::EmptyActor,
        );
        assert_eq!(
            doc.set_text("", "x").expect_err("empty actor"),
            CollabEditError::EmptyActor,
        );
        assert_eq!(doc.text(), "", "a refused edit changes nothing");

        // The invariant the refusal protects: everything the safe API mints
        // parses back, so the document round-trips instead of decoding as
        // its own JSON.
        let good = CollabText::from_text("ada", "hi").expect("non-empty actor");
        let encoded = serde_json::to_string(&good).expect("encode");
        assert_eq!(
            serde_json::from_str::<CollabText>(&encoded)
                .expect("decode")
                .text(),
            "hi",
        );
    }

    /// A peer id of `MAX_COUNTER - 1` pins the clock one step below the
    /// ceiling. The insert that follows must be refused whole, not committed
    /// as a prefix: a half-applied insert leaves the editor holding a
    /// provisional character the authority never took.
    #[test]
    fn an_insert_that_would_cross_the_ceiling_is_refused_whole() {
        let mut doc = CollabText::new();
        doc.insert("ada", 0, "seed").expect("collab edit refused");
        doc.apply(CollabOp::Insert {
            id: OpId::new(MAX_COUNTER - 2, "peer"),
            after: None,
            ch: 'x',
        });
        let before = doc.text();

        let refused = doc.insert("ada", 0, "ab").expect_err("counter exhausted");
        assert!(
            matches!(
                refused,
                CollabEditError::CounterExhausted {
                    wanted: 2,
                    available: 1
                }
            ),
            "the refusal says what was asked and what was left: {refused}"
        );
        assert_eq!(doc.text(), before, "nothing of the refused insert landed");

        // One character still fits, so the bound is exact rather than
        // conservative.
        assert_eq!(
            doc.insert("ada", 0, "a")
                .expect("the last counter is usable")
                .len(),
            1,
        );
    }

    /// `set_text` removes the replaced span before inserting the
    /// replacement, so a refusal between the two would delete text and put
    /// back only part of it. It must refuse before the remove.
    #[test]
    fn a_refused_set_text_does_not_tombstone_the_replaced_span() {
        let mut doc = CollabText::new();
        doc.insert("ada", 0, "hello").expect("collab edit refused");
        doc.apply(CollabOp::Insert {
            id: OpId::new(MAX_COUNTER - 2, "peer"),
            after: None,
            ch: 'x',
        });

        let refused = doc
            .set_text("ada", "goodbye world")
            .expect_err("counter exhausted");
        assert!(matches!(refused, CollabEditError::CounterExhausted { .. }));
        assert!(
            doc.text().contains("hello"),
            "the old text survives a refused replacement: {:?}",
            doc.text()
        );
    }

    /// A document that reuses one id for two different characters is refused.
    ///
    /// Its own text would depend on the order it was read in, and a merge with
    /// it on which side went first.
    #[test]
    fn a_document_reusing_an_id_for_two_characters_is_refused() {
        let json = serde_json::json!({
            "elems": [
                { "id": "1@ada", "ch": "a" },
                { "id": "1@ada", "after": "1@ada", "ch": "z" },
            ],
            "pending": [],
        })
        .to_string();

        let refused = serde_json::from_str::<CollabText>(&json).expect_err("reused id");
        assert!(
            refused.to_string().contains("two different characters"),
            "the refusal says why: {refused}"
        );

        // An honest duplicate — the same character twice — is still fine: the
        // replay drops the repeat, which is what makes a document canonical.
        let honest = serde_json::json!({
            "elems": [
                { "id": "1@ada", "ch": "a" },
                { "id": "1@ada", "ch": "a" },
            ],
            "pending": [],
        })
        .to_string();
        let doc = serde_json::from_str::<CollabText>(&honest).expect("a repeat, not a conflict");
        assert_eq!(doc.text(), "a");
    }

    /// A stored column with an unusable id still reads, and says what it lost.
    ///
    /// The request door refuses such a document; this one cannot — refusing
    /// would strand the row and reading it as prose would be worse. What it
    /// must not do is lose the characters quietly, because the next
    /// `encode_column` writes the shortened document back as canonical.
    #[test]
    fn a_stored_document_with_an_unusable_counter_reads_without_it() {
        let raw = serde_json::json!({
            "elems": [
                { "id": "1@ada", "ch": "a" },
                { "id": format!("{MAX_COUNTER}@broken"), "after": "1@ada", "ch": "b" },
            ],
            "pending": [],
        })
        .to_string();

        let doc = CollabText::decode_column(&raw);
        assert_eq!(doc.text(), "a", "the usable character survives");
        assert_eq!(doc.pending_len(), 0, "and the unusable one is not buffered");
    }

    /// A document carrying an id past the counter ceiling is refused whole.
    ///
    /// `apply` drops such an id on the floor, and the replay ignores what it
    /// returns — so without this the document deserialized "successfully"
    /// minus those characters, the server quietly deleting text from a
    /// document it had just accepted.
    #[test]
    fn a_document_with_an_unusable_counter_is_refused() {
        let json = serde_json::json!({
            "elems": [
                { "id": "1@ada", "ch": "a" },
                { "id": format!("{MAX_COUNTER}@evil"), "after": "1@ada", "ch": "b" },
            ],
            "pending": [],
        })
        .to_string();

        let refused = serde_json::from_str::<CollabText>(&json).expect_err("unusable id");
        assert!(
            refused.to_string().contains("at or past the ceiling"),
            "the refusal says why: {refused}"
        );
    }

    /// The same for a buffered operation, which takes the other path in.
    #[test]
    fn a_buffered_operation_with_an_unusable_counter_is_refused() {
        let json = serde_json::json!({
            "elems": [],
            "pending": [
                {
                    "op": "insert",
                    "id": format!("{}@evil", MAX_COUNTER + 5),
                    "after": "1@ada",
                    "ch": "x",
                }
            ],
        })
        .to_string();

        let refused = serde_json::from_str::<CollabText>(&json).expect_err("unusable id");
        assert!(
            refused.to_string().contains("at or past the ceiling"),
            "the refusal says why: {refused}"
        );
    }

    /// A document at the ceiling still round trips, so the bound cannot be
    /// reached by anything the hub itself produces.
    #[test]
    fn a_document_at_the_ceiling_still_decodes() {
        let mut doc = CollabText::new();
        doc.insert("ada", 0, &"x".repeat(MAX_WIRE_ELEMENTS))
            .expect("collab edit refused");
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
        a.insert("a", 0, "abc").expect("collab edit refused");
        let mut b = a.clone();

        let del = a.remove(1, 1); // "ac"
        let ins = b.insert("b", 3, "!").expect("collab edit refused"); // "abc!"

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
        a.insert("a", 0, "abcd").expect("collab edit refused");
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
        doc.insert("a", 0, "ac").expect("collab edit refused");
        doc.insert("a", 1, "b").expect("collab edit refused");
        assert_eq!(doc.text(), "abc");
        assert!(!doc.is_empty());
    }

    /// The stored form keeps every character, not just the visible text.
    #[test]
    fn serde_round_trip_is_lossless() {
        let mut doc = CollabText::new();
        doc.insert("a", 0, "abc").expect("collab edit refused");
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
        let a_ops = a.insert("a", 0, "left").expect("collab edit refused");
        let mut b = CollabText::new();
        let b_ops = b.insert("b", 0, "right").expect("collab edit refused");
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
        let mut a = CollabText::from_text("seed", "the quick fox").expect("collab edit refused");
        let mut b = a.clone();

        let a_ops = a
            .set_text("a", "the quick brown fox")
            .expect("collab edit refused"); // insert mid-string
        let b_ops = b
            .set_text("b", "THE quick fox")
            .expect("collab edit refused"); // rewrite the head

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
        let mut server = CollabText::from_text("seed", "world").expect("collab edit refused");
        // A client resolved "after the 'w'" before anyone else typed.
        let anchor = server.id_at(0).expect("first character");
        // Meanwhile another editor prepends.
        server
            .insert("other", 0, "hello ")
            .expect("collab edit refused");
        // The stale client's edit still lands after the 'w', not at index 1.
        server
            .insert_after("client", Some(&anchor), "-")
            .expect("collab edit refused");
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
        let base = CollabText::from_text("seed", "base").expect("collab edit refused");
        let mut a = base.clone();
        a.insert("a", 0, "A").expect("collab edit refused");
        let mut b = base.clone();
        b.insert("b", 4, "B").expect("collab edit refused");
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
        let ins = source.insert("a", 0, "x").expect("collab edit refused");
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
        let mut doc = CollabText::from_text("seed", "hi").expect("collab edit refused");
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
        doc.insert("seed", 2, "!").expect("collab edit refused");
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
        let mut doc = CollabText::from_text("seed", "ab").expect("collab edit refused");
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
