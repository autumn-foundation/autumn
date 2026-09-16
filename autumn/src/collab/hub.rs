//! Live collaborative sessions over the existing channel and presence seams.
//!
//! [`CollabHub`] keeps one live [`CollabText`] per collaborative field
//! instance — one note's body, one row's column. Editors join a document,
//! send operations, and receive every other editor's operations plus the
//! participant list and their cursors.
//!
//! # Where each part comes from
//!
//! | Concern | Seam |
//! |---|---|
//! | Operation fan-out | [`Channels`] topic `collab:{key}` |
//! | Who is editing | [`Presence`] topic `collab:{key}` |
//! | Cursor position | a `Cursor` message, merged into the participant list |
//!
//! The hub adds no transport of its own, so an app already serving
//! `#[ws]` routes gets collaboration on the socket it has.
//!
//! # Deployment: one process owns a document
//!
//! A document is owned by the process holding it. Run collaboration on a
//! single replica in this slice, or route every editor of one record to the
//! same replica.
//!
//! Two replicas sharing a Redis channel backend converge on the *text* —
//! operations flow both ways and the merge is order-independent — but three
//! things do not work:
//!
//! - **The participant list is per-replica.** [`Presence`] tracks membership
//!   in-process, so each replica broadcasts only its own editors and a client
//!   sees the roster flip between them.
//! - **The size limit is per-replica.** Two replicas can each accept a batch
//!   within [`CollabLimits`] and then merge the other's.
//! - **Persistence races.** Each replica writes the row from its own copy.
//!
//! # Authority
//!
//! The hub is the authority. A client sends an edit anchored to a **character
//! id**, never an index, so the hub places it correctly even when the document
//! changed in flight. The hub then broadcasts the resulting operations, and
//! every client applies them in the order they arrive.
//!
//! # Lifetime and persistence
//!
//! Nothing evicts a document on its own: once opened it stays in memory until
//! [`CollabHub::close`], which hands back the final state to persist. An app
//! that opens a document per record closes it when the last editor leaves.
//! Seed one from the database with [`CollabHub::open_with`] and write it back
//! with [`CollabDoc::document`].
//!
//! ```rust,no_run
//! use autumn_web::collab::{CollabHub, CollabText};
//! use autumn_web::prelude::*;
//!
//! # fn wire(state: AppState, stored: String) {
//! let hub = state.collab().clone();
//! let Ok(doc) = hub.open_with("notes:42:body", || CollabText::decode_column(&stored))
//! else {
//!     return; // the registry is full; ask the caller to retry
//! };
//! let session = doc.join("session-1", "Ada");
//!
//! // ... apply client messages, broadcast operations ...
//!
//! // Persist whenever it suits the app (on idle, on leave, on a timer).
//! let column = doc.document().encode_column();
//! drop(session);
//! # let _ = column;
//! # }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};

use crate::channels::{Channels, Subscriber};
use crate::presence::{Presence, PresenceHandle};

use super::text::{CollabElement, CollabOp, CollabText, MAX_WIRE_ELEMENTS, OpId};

/// Channel and presence topic for a document key.
#[must_use]
pub fn topic_for(key: &str) -> String {
    format!("collab:{key}")
}

/// Document key for one field instance: `{table}:{pk}:{column}`.
///
/// Any stable string works; this is the convention the framework's own
/// surfaces use, so an app that follows it stays legible to them.
#[must_use]
pub fn doc_key(table: &str, pk: impl std::fmt::Display, column: &str) -> String {
    format!("{table}:{pk}:{column}")
}

/// Bounds on what one client may send.
///
/// The hub is a shared, long-lived authority: without a bound, one client
/// could grow a document until the process runs out of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollabLimits {
    /// Characters one insert message may carry.
    pub max_insert_chars: usize,
    /// Characters one document may hold, tombstones and buffered operations
    /// included — what the document costs, not what it shows.
    ///
    /// Capped at [`MAX_WIRE_ELEMENTS`] however it is set. Past that a
    /// document stops surviving a round trip through an untrusted door — a
    /// sync payload, a request body — because [`CollabText`]'s `Deserialize`
    /// refuses to replay it. A hub that could build one would be writing
    /// documents its own resolver must skip.
    pub max_document_chars: usize,
    /// Ids one delete message may name.
    pub max_delete_ids: usize,
    /// Live documents the hub may hold at once.
    pub max_documents: usize,
}

impl CollabLimits {
    /// Hold `max_document_chars` to what an untrusted door will replay.
    ///
    /// A hub configured past [`MAX_WIRE_ELEMENTS`] would build documents that
    /// its own resolver, and any handler deserializing a request body, must
    /// then refuse — the document would be trapped in the hub, unable to
    /// round trip. Clamping is quieter than that and loses nothing a caller
    /// could have used.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            max_document_chars: self.max_document_chars.min(MAX_WIRE_ELEMENTS),
            ..self
        }
    }
}

impl Default for CollabLimits {
    fn default() -> Self {
        Self {
            max_insert_chars: 10_000,
            max_document_chars: MAX_WIRE_ELEMENTS,
            max_delete_ids: 10_000,
            max_documents: 10_000,
        }
    }
}

/// A client message the hub refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CollabError {
    /// An insert exceeded [`CollabLimits::max_insert_chars`].
    #[error("insert of {got} characters exceeds the limit of {limit}")]
    InsertTooLarge {
        /// Characters the message carried.
        got: usize,
        /// Configured limit.
        limit: usize,
    },
    /// The document is at [`CollabLimits::max_document_chars`].
    #[error("document holds {got} characters, at the limit of {limit}")]
    DocumentFull {
        /// Characters the document holds.
        got: usize,
        /// Configured limit.
        limit: usize,
    },
    /// A delete named more ids than [`CollabLimits::max_delete_ids`].
    #[error("delete of {got} ids exceeds the limit of {limit}")]
    DeleteTooLarge {
        /// Ids the message named.
        got: usize,
        /// Configured limit.
        limit: usize,
    },
    /// The registry is at [`CollabLimits::max_documents`].
    ///
    /// The hub refuses rather than serving an untracked document: a document
    /// outside the registry is a second authority for the same key, so the
    /// next editor of that record would silently start editing a different
    /// copy — the loss this feature exists to prevent — and it would count
    /// against no limit.
    #[error("the document registry holds {open} documents, at the limit of {limit}")]
    RegistryFull {
        /// Documents the registry holds.
        open: usize,
        /// Configured limit.
        limit: usize,
    },
    /// A message named a character the document has never seen.
    ///
    /// The hub is the authority, so a live editor can only anchor to an id the
    /// hub minted. An id from the future is a client fault, and accepting it
    /// would let one message tombstone characters nobody has typed yet.
    #[error("unknown character id {id}")]
    UnknownCharacter {
        /// The id the message named.
        id: String,
    },
}

/// What an editor sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CollabClientMessage {
    /// Add `text` directly after the character `after` names, or at the start
    /// when `after` is absent.
    Insert {
        /// Left neighbour, resolved by the client against its own view.
        #[serde(default)]
        after: Option<OpId>,
        /// Text to add.
        text: String,
    },
    /// Remove the characters `ids` names.
    Delete {
        /// Characters to remove.
        ids: Vec<OpId>,
    },
    /// Report where this editor's caret is, as a visible character index.
    Cursor {
        /// Caret position.
        index: usize,
    },
}

/// What the hub sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CollabServerMessage {
    /// The whole document, sent once when an editor joins.
    Snapshot {
        /// Every character, tombstones included, in document order.
        elems: Vec<CollabElement>,
        /// Operations the document holds but cannot place yet, because the
        /// character they name has not arrived.
        ///
        /// A joining editor needs them. The hub broadcasts an operation when
        /// it arrives, not when it later integrates, so an editor who joined
        /// after one was buffered would never hear of it and would diverge
        /// the moment its cause landed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending: Vec<CollabOp>,
        /// Who else is editing.
        participants: Vec<CollabParticipant>,
        /// The receiving editor's own actor id, when the snapshot was built
        /// for one ([`CollabSession::snapshot`]).
        ///
        /// A client needs it to recognise its own operations coming back. It
        /// sends an edit and waits for the echo before diffing again; without
        /// a way to tell "my edit landed" from "somebody else typed", a second
        /// keystroke inside one round trip would re-send the first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
    },
    /// Operations to apply, in order.
    Ops {
        /// The operations.
        ops: Vec<CollabOp>,
    },
    /// The participant list changed, or somebody moved their cursor.
    Presence {
        /// Who is editing, and where.
        participants: Vec<CollabParticipant>,
    },
    /// The hub refused a message. Sent only to the editor that sent it.
    Error {
        /// Why it was refused.
        message: String,
    },
}

/// One editor of a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollabParticipant {
    /// The editor's actor id, which is also the id its characters carry.
    pub actor: String,
    /// Display name.
    pub label: String,
    /// Caret position as a visible character index, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
}

/// Live state for one document.
struct DocState {
    doc: CollabText,
    /// `actor -> caret`. `BTreeMap` so the participant list is ordered and
    /// two replicas render the same thing.
    cursors: BTreeMap<String, usize>,
    /// Live [`CollabSession`]s. The last one to leave evicts the document, so
    /// an app that never calls [`CollabHub::close`] still does not leak one
    /// per record it ever opened.
    sessions: usize,
    /// Live [`CollabClose`] for this document, if one is out.
    ///
    /// A `Weak` so it needs no cleanup: however the guard goes — finalized,
    /// dropped, or lost to a panic — this stops upgrading and the next close
    /// may proceed.
    close_claim: Weak<()>,
    /// Bumped on every change to `doc`.
    ///
    /// [`CollabClose`] records it and refuses to evict a document that moved
    /// on: the state it handed the app to persist would no longer be the
    /// document's, and evicting would drop the difference. Occupancy alone
    /// cannot see this — an editor can arrive, type, and leave entirely
    /// inside the window.
    revision: u64,
}

impl DocState {
    const fn new(doc: CollabText) -> Self {
        Self {
            doc,
            cursors: BTreeMap::new(),
            sessions: 0,
            close_claim: Weak::new(),
            revision: 0,
        }
    }

    /// Mutate the document and record that it moved.
    fn edit<T>(&mut self, change: impl FnOnce(&mut CollabText) -> T) -> T {
        let out = change(&mut self.doc);
        self.revision = self.revision.wrapping_add(1);
        out
    }
}

/// Per-connection discriminator, so two connections that pass the same actor
/// id still get distinct presence keys, cursors and character ids.
static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// How often [`serve_socket`] renews an editor's presence lease. Comfortably
/// inside the 30-second default TTL, so one missed tick costs nothing.
const PRESENCE_REFRESH: std::time::Duration = std::time::Duration::from_secs(10);

/// How many full resends one editor gets before [`serve_socket`] gives up on
/// it. See the `Lagged` arm for why this is bounded.
const MAX_LAG_RESENDS: u32 = 3;

/// Registry of live collaborative documents.
///
/// Available from a handler as `state.collab()`, or as the [`CollabHub`]
/// extractor. Cloning is cheap and shares the same documents.
///
/// One pointer wide on purpose. [`AppState`](crate::state::AppState) holds a
/// hub by value and lives inside large futures, so four inline fields here
/// cost 72 bytes in every one of them — enough to push
/// `SystemTest::build()` past `clippy::large_futures`. The seams beside it
/// (`Channels`, `Presence`) are handles for the same reason.
#[derive(Clone)]
pub struct CollabHub {
    inner: Arc<HubInner>,
}

/// What a hub owns. Shared by every clone.
struct HubInner {
    /// `key -> weak handle`. **Weak** on purpose: the live [`CollabDoc`]
    /// handles own the document, so it stays findable for exactly as long as
    /// somebody holds one and disappears on its own afterwards.
    ///
    /// A strong map leaks a document per record ever opened. Evicting on the
    /// last editor's departure instead loses data: `CollabSession::drop` runs
    /// before the handler persists, so a reconnect in that window re-seeds
    /// from the stale row and the departing save overwrites it. A weak entry
    /// has neither problem — the handler's own handle keeps the document
    /// discoverable until it has finished writing it back.
    ///
    /// Its own `Arc` so [`CollabHub::with_limits`] can build a second hub
    /// over the same registry.
    docs: Arc<Mutex<HashMap<String, Weak<Mutex<DocState>>>>>,
    channels: Channels,
    presence: Presence,
    limits: CollabLimits,
}

impl CollabHub {
    /// Build a hub over a channel registry and a presence tracker.
    #[must_use]
    pub fn new(channels: Channels, presence: Presence) -> Self {
        Self {
            inner: Arc::new(HubInner {
                docs: Arc::new(Mutex::new(HashMap::new())),
                channels,
                presence,
                limits: CollabLimits::default(),
            }),
        }
    }

    /// Replace the per-message bounds.
    ///
    /// Configure the hub **before** it serves. The returned hub shares the
    /// live-document registry, so it sees the same documents — but the bounds
    /// are copied into each [`CollabDoc`] as it is handed out, so a handle
    /// taken before this call keeps the bounds it was given, and so does any
    /// surviving clone of the hub it came from. Tightening a limit on a hub
    /// that is already serving therefore binds the next handle, not the ones
    /// already in flight.
    ///
    /// Build it once, at startup, and put the result in state:
    ///
    /// ```ignore
    /// let hub = CollabHub::new(channels, presence).with_limits(CollabLimits {
    ///     max_document_chars: 4_000,
    ///     ..CollabLimits::default()
    /// });
    /// ```
    ///
    /// `max_document_chars` is held to [`MAX_WIRE_ELEMENTS`] by
    /// [`CollabLimits::clamped`]; see that method for why.
    #[must_use]
    pub fn with_limits(self, limits: CollabLimits) -> Self {
        Self {
            inner: Arc::new(HubInner {
                docs: Arc::clone(&self.inner.docs),
                channels: self.inner.channels.clone(),
                presence: self.inner.presence.clone(),
                limits: limits.clamped(),
            }),
        }
    }

    /// The bounds in effect.
    #[must_use]
    pub fn limits(&self) -> CollabLimits {
        self.inner.limits
    }

    /// Open `key`, starting from an empty document if it is not live yet.
    ///
    /// # Errors
    ///
    /// Returns [`CollabError::RegistryFull`] when the registry is at
    /// [`CollabLimits::max_documents`] and `key` is not already live.
    ///
    /// # Panics
    ///
    /// Panics if the internal document registry mutex is poisoned.
    pub fn document(&self, key: &str) -> Result<CollabDoc, CollabError> {
        self.open_with(key, CollabText::new)
    }

    /// Open `key`, calling `seed` only if it is not live yet.
    ///
    /// This is where a document comes back from the database: `seed` runs at
    /// most once per key, so a second editor joins the document the first one
    /// is already editing rather than a stale copy of the row.
    ///
    /// `seed` runs with the registry **unlocked**, so a race may build the
    /// value twice and discard the loser. Keep it cheap and free of side
    /// effects — load the row first and hand the closure the value, as
    /// `examples/collab-notes` does.
    ///
    /// # Errors
    ///
    /// Returns [`CollabError::RegistryFull`] when the registry is at
    /// [`CollabLimits::max_documents`] and `key` is not already live. An
    /// editor already on `key` always gets the live document, whatever the
    /// limit says: refusing them would split a document that is open.
    ///
    /// Returns [`CollabError::DocumentFull`] when `seed` produces a document
    /// already past [`CollabLimits::max_document_chars`] — a row written
    /// before the limit was lowered, or one whose elements and buffered
    /// operations are each within the wire bounds but together are not.
    ///
    /// # Panics
    ///
    /// Panics if the internal document registry mutex is poisoned.
    pub fn open_with(
        &self,
        key: &str,
        seed: impl FnOnce() -> CollabText,
    ) -> Result<CollabDoc, CollabError> {
        // Already live: hand back the shared document and never call the seed.
        if let Some(state) = self.live(key) {
            return Ok(self.handle_for(key, state));
        }

        // Not live: build the seed with the registry UNLOCKED. Under the lock
        // it would block every other document's joins, a panic in it would
        // poison the registry for the whole process, and a seed that touched
        // the hub again would deadlock on a non-reentrant mutex. A race builds
        // the value twice and discards the loser, which nothing observes.
        let seeded = seed();

        let mut docs = self
            .inner
            .docs
            .lock()
            .expect("collab registry lock poisoned");
        // Somebody may have won the race while the seed ran.
        if let Some(state) = docs.get(key).and_then(Weak::upgrade) {
            drop(docs);
            return Ok(self.handle_for(key, state));
        }
        // Drop entries whose document is gone before counting: a dead weak
        // reference costs a map slot, not a document.
        docs.retain(|_, weak| weak.strong_count() > 0);
        if docs.len() >= self.inner.limits.max_documents {
            let open = docs.len();
            drop(docs);
            drop(seeded);
            tracing::warn!(
                key,
                open,
                limit = self.inner.limits.max_documents,
                "collab: document registry is full; refusing to open"
            );
            return Err(CollabError::RegistryFull {
                open,
                limit: self.inner.limits.max_documents,
            });
        }
        // The seed is measured like anything else the document holds. A row
        // can be over budget without anyone having typed a character into
        // this hub: a limit lowered since it was written, or a document that
        // is wire-valid at 10 000 elements *plus* 1 000 buffered operations,
        // because `Deserialize` bounds those two separately. Installing it
        // would put the authority over its own advertised bound and leave it
        // refusing edits to a document it served.
        let held = seeded.element_count() + seeded.pending_len();
        if held > self.inner.limits.max_document_chars {
            drop(docs);
            tracing::warn!(
                key,
                held,
                limit = self.inner.limits.max_document_chars,
                "collab: seed is past the document limit; refusing to open"
            );
            return Err(CollabError::DocumentFull {
                got: held,
                limit: self.inner.limits.max_document_chars,
            });
        }
        let state = Arc::new(Mutex::new(DocState::new(seeded)));
        docs.insert(key.to_owned(), Arc::downgrade(&state));
        drop(docs);
        Ok(self.handle_for(key, state))
    }

    /// The live document for `key`, if one is still held somewhere.
    ///
    /// # Panics
    ///
    /// Panics if the internal document registry mutex is poisoned.
    fn live(&self, key: &str) -> Option<Arc<Mutex<DocState>>> {
        self.inner
            .docs
            .lock()
            .expect("collab registry lock poisoned")
            .get(key)
            .and_then(Weak::upgrade)
    }

    /// Wrap one document's state in a handle.
    fn handle_for(&self, key: &str, state: Arc<Mutex<DocState>>) -> CollabDoc {
        CollabDoc {
            key: key.to_owned(),
            state,
            channels: self.inner.channels.clone(),
            presence: self.inner.presence.clone(),
            limits: self.inner.limits,
        }
    }

    /// Begin evicting `key`, handing back its final state to persist.
    ///
    /// Returns `None` when `key` is not live, when an editor is still on it —
    /// an occupied document is not the app's to evict — or when another
    /// [`CollabClose`] for it is already outstanding, because two writers
    /// each holding a copy of one document is how the older copy wins.
    ///
    /// The document stays **discoverable** until the returned
    /// [`CollabClose`] is finalized or dropped, which is what makes the
    /// close-then-persist flow safe. See that type for why.
    ///
    /// # Panics
    ///
    /// Panics if the internal document registry mutex is poisoned.
    pub fn close(&self, key: &str) -> Option<CollabClose> {
        let docs = self
            .inner
            .docs
            .lock()
            .expect("collab registry lock poisoned");
        let state = docs.get(key).and_then(Weak::upgrade)?;
        let mut guard = state.lock().expect("collab document lock poisoned");
        // Refuse while an editor is still here. Evicting would not stop them:
        // they hold the same `Arc` and keep editing a document nobody can
        // find, while the next joiner re-seeds from the row and starts a
        // second, divergent copy. The last editor to leave evicts it.
        if guard.sessions > 0 {
            return None;
        }
        // One close at a time. Two persistence paths racing here — an idle
        // sweep against a disconnect hook — would each get a guard over the
        // same state and each start a write. The first to finalize releases
        // the document; an editor reopens and edits a fresh authority; the
        // second write then lands on top of it with the older text, and its
        // `finalize` sees a different `Arc` and says nothing.
        if guard.close_claim.upgrade().is_some() {
            return None;
        }
        let claim = Arc::new(());
        guard.close_claim = Arc::downgrade(&claim);
        let text = guard.doc.clone();
        let revision = guard.revision;
        drop(guard);
        drop(docs);
        Some(CollabClose {
            hub: self.clone(),
            key: key.to_owned(),
            state,
            text,
            revision,
            claim,
        })
    }

    /// Keys of every live document, in sorted order.
    ///
    /// # Panics
    ///
    /// Panics if the internal document registry mutex is poisoned.
    #[must_use]
    pub fn open_keys(&self) -> Vec<String> {
        let mut docs = self
            .inner
            .docs
            .lock()
            .expect("collab registry lock poisoned");
        docs.retain(|_, weak| weak.strong_count() > 0);
        let mut keys: Vec<String> = docs.keys().cloned().collect();
        drop(docs);
        keys.sort();
        keys
    }
}

/// A document on its way out, still discoverable until the write commits.
///
/// [`CollabHub::close`] hands back the final state so the app can persist it,
/// and that write is not instant. Removing the registry entry first opens a
/// window: an editor reconnecting inside it finds no live document, seeds a
/// second authority from the row the write has not reached yet, and the two
/// copies then overwrite each other. It is the same divergence `close`
/// already refuses to cause while an editor is on the document, one step
/// later in the document's life.
///
/// So the entry stays until this guard goes. An editor who reconnects inside
/// the window joins the **live** document and keeps every character;
/// [`finalize`](Self::finalize) then finds them there and leaves the document
/// alone, because it is no longer the app's to evict.
///
/// ```ignore
/// let mut closing = hub.close(&key);
/// while let Some(pending) = closing {
///     repo.update(id, body(pending.text())).await?;
///     closing = pending.finalize();
/// }
/// ```
///
/// The loop is not ceremony: [`finalize`](Self::finalize) hands the guard
/// back when the document changed while the write was in flight, because this
/// guard is the last thing keeping that text alive and no row has it yet.
///
/// Dropping it without finalizing is safe for the registry: nothing holds the
/// document after that, so the weak reference dies and the next open
/// re-seeds. It is how an app loses an edit made inside the window, though,
/// and it releases the slot against [`CollabLimits::max_documents`] only
/// eventually rather than promptly.
#[must_use = "the document stays open until this guard is finalized or dropped"]
pub struct CollabClose {
    hub: CollabHub,
    key: String,
    /// Keeps the registry's `Weak` upgradeable while the write is in flight.
    /// Without it the entry is already dead and the next open re-seeds from
    /// the stale row, which is the whole hazard.
    state: Arc<Mutex<DocState>>,
    text: CollabText,
    /// The document's revision when `text` was taken.
    revision: u64,
    /// Proof that this is the only close in flight for the key.
    ///
    /// [`CollabHub::close`] refuses while one of these is alive, so two
    /// writers cannot each hold a copy of the same document and overwrite one
    /// another. Dropping the guard releases the claim, whatever the path.
    claim: Arc<()>,
}

impl CollabClose {
    /// The final state to persist.
    #[must_use]
    pub const fn text(&self) -> &CollabText {
        &self.text
    }

    /// The key being closed.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Release the document: the write has committed.
    ///
    /// Returns `None` when the document is released, or when an editor is on
    /// it — an occupied document belongs to that editor's handler, which will
    /// persist it in turn.
    ///
    /// Returns `Some` when the document **changed** while the write was in
    /// flight. The state just committed is not the document's any more, so
    /// the returned guard carries the newer text: persist that and finalize
    /// again. This is not hypothetical, and occupancy cannot see it — an
    /// editor can reconnect, type, and leave again entirely inside the
    /// window, and nothing else will ever write their characters down. The
    /// loop ends as soon as a finalize finds the document where it left it:
    ///
    /// ```ignore
    /// let mut closing = hub.close(&key);
    /// while let Some(pending) = closing {
    ///     repo.update(id, body(pending.text())).await?;
    ///     closing = pending.finalize();
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the internal document registry mutex is poisoned.
    #[must_use = "a returned guard means the document moved on and still needs persisting"]
    pub fn finalize(self) -> Option<Self> {
        let mut docs = self
            .hub
            .inner
            .docs
            .lock()
            .expect("collab registry lock poisoned");
        let live = docs.get(&self.key).and_then(Weak::upgrade)?;
        // A different document now wears this key.
        if !Arc::ptr_eq(&live, &self.state) {
            return None;
        }
        let state = live.lock().expect("collab document lock poisoned");
        // Occupied: its editor's handler owns persisting it from here.
        if state.sessions > 0 {
            return None;
        }
        // Moved on since the state that was just written was taken. Hand the
        // caller what the document actually holds now. Releasing here would
        // drop it: this guard is the last thing keeping it alive, so the
        // difference would exist in no row and no memory.
        if state.revision != self.revision {
            let text = state.doc.clone();
            let revision = state.revision;
            drop(state);
            drop(docs);
            return Some(Self {
                hub: self.hub,
                key: self.key,
                state: self.state,
                text,
                revision,
                // Carried on: this is the same close, still the only one.
                claim: self.claim,
            });
        }
        drop(state);
        docs.remove(&self.key);
        None
    }
}

impl std::fmt::Debug for CollabClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollabClose")
            .field("key", &self.key)
            .field("chars", &self.text.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CollabHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollabHub")
            .field("open", &self.open_keys().len())
            .field("limits", &self.inner.limits)
            .finish_non_exhaustive()
    }
}

impl axum::extract::FromRequestParts<crate::state::AppState> for CollabHub {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        state: &crate::state::AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(state.collab().clone())
    }
}

/// A handle to one live document.
#[derive(Clone)]
pub struct CollabDoc {
    key: String,
    state: Arc<Mutex<DocState>>,
    channels: Channels,
    presence: Presence,
    limits: CollabLimits,
}

impl CollabDoc {
    /// The document key.
    #[must_use]
    pub const fn key(&self) -> &str {
        self.key.as_str()
    }

    /// The channel and presence topic this document broadcasts on.
    #[must_use]
    pub fn topic(&self) -> String {
        topic_for(&self.key)
    }

    /// The visible text.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    #[must_use]
    pub fn text(&self) -> String {
        self.with_doc(CollabText::text)
    }

    /// A copy of the document — what the app persists.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    #[must_use]
    pub fn document(&self) -> CollabText {
        self.with_doc(Clone::clone)
    }

    /// Subscribe to this document's server messages.
    #[must_use]
    pub fn subscribe(&self) -> Subscriber {
        self.channels.subscribe(&self.topic())
    }

    /// The opening message for a joining editor.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    #[must_use]
    pub fn snapshot(&self) -> CollabServerMessage {
        let (elems, pending) = self.with_doc(|doc| (doc.elements(), doc.pending_ops().to_vec()));
        CollabServerMessage::Snapshot {
            elems,
            pending,
            participants: self.participants(),
            actor: None,
        }
    }

    /// Register `actor` as an editor and announce the join.
    ///
    /// The returned [`CollabSession`] holds the presence lease: drop it and
    /// the editor leaves, its cursor disappears, and the remaining editors
    /// are told.
    ///
    /// `actor` is a **prefix**: the hub appends a per-connection number and
    /// uses the result as the presence key, the cursor key, and the actor id
    /// every character this editor types carries. Two tabs that pass the same
    /// name therefore keep their own caret and their own row in the
    /// participant list instead of silently collapsing into one.
    /// [`CollabSession::actor`] returns the id that was minted.
    ///
    /// Keep `actor` ASCII. Character ids break ties on the actor, and a
    /// browser replica compares those as UTF-16 while the server compares
    /// UTF-8 bytes; the two agree on ASCII and can disagree outside the basic
    /// multilingual plane.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    #[must_use = "dropping the session immediately ends the editor's presence lease"]
    pub fn join(&self, actor: impl Into<String>, label: impl Into<String>) -> CollabSession {
        let seat = NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let actor = format!("{}#{seat}", actor.into());
        let label = label.into();
        {
            let mut state = self.state.lock().expect("collab document lock poisoned");
            state.sessions += 1;
        }
        let presence = self.presence.track(
            self.topic(),
            actor.clone(),
            serde_json::json!({ "label": label }),
        );
        self.broadcast_presence();
        CollabSession {
            doc: self.clone(),
            actor,
            presence: Some(presence),
        }
    }

    /// Who is editing, and where their carets are.
    ///
    /// Membership comes from [`Presence`]; the caret comes from the last
    /// `Cursor` message each editor sent.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    #[must_use]
    pub fn participants(&self) -> Vec<CollabParticipant> {
        let cursors = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.cursors.clone()
        };
        self.presence
            .list(&self.topic())
            .into_iter()
            .map(|entry| {
                let label = entry
                    .metas
                    .first()
                    .and_then(|m| m.get("label"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(entry.key.as_str())
                    .to_owned();
                CollabParticipant {
                    cursor: cursors.get(&entry.key).copied(),
                    actor: entry.key,
                    label,
                }
            })
            .collect()
    }

    /// Apply one client message as `actor` and broadcast the result.
    ///
    /// Returns the operations the message produced — empty for a cursor
    /// move, which changes the participant list instead.
    ///
    /// # Errors
    ///
    /// Returns [`CollabError`] when the message exceeds the hub's limits: an
    /// oversized insert or delete, or a document already at its maximum size.
    /// Nothing is applied and nothing is broadcast.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    pub fn handle(
        &self,
        actor: &str,
        message: CollabClientMessage,
    ) -> Result<Vec<CollabOp>, CollabError> {
        match message {
            CollabClientMessage::Insert { after, text } => {
                let added = text.chars().count();
                if added > self.limits.max_insert_chars {
                    return Err(CollabError::InsertTooLarge {
                        got: added,
                        limit: self.limits.max_insert_chars,
                    });
                }
                let ops = {
                    let mut state = self.state.lock().expect("collab document lock poisoned");
                    // The hub minted every id in this document, so an anchor
                    // it has never seen is a client fault. Refusing it here
                    // stops two things at once: an anchor from the future
                    // would drag the Lamport clock up with it, and an
                    // unintegrable insert would sit in the buffer forever.
                    if let Some(anchor) = after.as_ref()
                        && !state.doc.knows(anchor)
                    {
                        return Err(CollabError::UnknownCharacter {
                            id: anchor.to_string(),
                        });
                    }
                    // Tombstones and buffered operations count too: they are
                    // what the document costs, not what it shows.
                    let held = state.doc.element_count() + state.doc.pending_len();
                    if held + added > self.limits.max_document_chars {
                        return Err(CollabError::DocumentFull {
                            got: held,
                            limit: self.limits.max_document_chars,
                        });
                    }
                    state.edit(|doc| doc.insert_after(actor, after.as_ref(), &text))
                };
                self.broadcast_ops(&ops);
                Ok(ops)
            }
            CollabClientMessage::Delete { ids } => {
                if ids.len() > self.limits.max_delete_ids {
                    return Err(CollabError::DeleteTooLarge {
                        got: ids.len(),
                        limit: self.limits.max_delete_ids,
                    });
                }
                let ops = {
                    let mut state = self.state.lock().expect("collab document lock poisoned");
                    // `remove_known`, not `remove_ids`: a delete for a
                    // character the hub has never seen must be dropped, not
                    // buffered. Buffered, it would tombstone that character
                    // the moment somebody typed it — one message could
                    // pre-delete another editor's next thousand keystrokes.
                    state.edit(|doc| doc.remove_known(&ids))
                };
                self.broadcast_ops(&ops);
                Ok(ops)
            }
            CollabClientMessage::Cursor { index } => {
                {
                    let mut state = self.state.lock().expect("collab document lock poisoned");
                    state.cursors.insert(actor.to_owned(), index);
                }
                self.broadcast_presence();
                Ok(Vec::new())
            }
        }
    }

    /// Merge operations that arrived from somewhere other than a live editor
    /// — an offline client reconnecting, another replica, a background job —
    /// and broadcast them.
    ///
    /// Returns how many operations are still waiting for their cause. A
    /// non-zero count means the peer sent an incomplete history; the
    /// operations are kept, not dropped.
    ///
    /// # Errors
    ///
    /// Returns [`CollabError::DocumentFull`] when the batch would take the
    /// document past [`CollabLimits::max_document_chars`]. Nothing is applied.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    pub fn apply_remote(&self, ops: &[CollabOp]) -> Result<usize, CollabError> {
        let (accepted, waiting) = {
            let mut state = self.state.lock().expect("collab document lock poisoned");
            let held = state.doc.element_count() + state.doc.pending_len();
            // Charge only what the batch would actually add. A reconnecting
            // peer replays its whole history, so counting the batch length
            // would refuse an idempotent replay that adds nothing.
            let novel = state.doc.novel_count(ops.iter());
            if held + novel > self.limits.max_document_chars {
                return Err(CollabError::DocumentFull {
                    got: held,
                    limit: self.limits.max_document_chars,
                });
            }
            // Keep only what the authority took, and only the first time it
            // takes it. `apply` refuses an id past `MAX_COUNTER` outright, and
            // a browser replica has no such check — broadcasting a refused
            // operation would put a character in every client that the
            // document does not have, and every later edit anchored to it
            // would then come back as unknown.
            //
            // `holds_pending` alone cannot tell "newly buffered" from "was
            // already buffered", so asking it after the fact re-broadcast
            // every idempotent replay. A reconnecting peer replays its whole
            // history, so an operation whose cause never arrives would go out
            // again on every reconnect and sit in every client's causal buffer
            // once more.
            let accepted: Vec<CollabOp> = ops
                .iter()
                .filter(|op| {
                    let held_before = state.doc.holds_pending(op);
                    let integrated = state.edit(|doc| doc.apply((*op).clone()));
                    integrated || (!held_before && state.doc.holds_pending(op))
                })
                .cloned()
                .collect();
            let waiting = state.doc.pending_len();
            drop(state);
            (accepted, waiting)
        };
        self.broadcast_ops(&accepted);
        Ok(waiting)
    }

    /// Merge operations that arrived **on this document's own channel** into
    /// the local replica, without broadcasting them again.
    ///
    /// The re-broadcast is what separates this from
    /// [`apply_remote`](Self::apply_remote): these operations are already on
    /// the channel, so publishing them once more would loop.
    ///
    /// # Errors
    ///
    /// Returns [`CollabError::DocumentFull`] when the batch would take the
    /// document past [`CollabLimits::max_document_chars`]. Nothing is
    /// applied. Refusing here means this replica falls behind whichever one
    /// accepted the batch — see the module docs on deployment: one process
    /// owns a document in this slice.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    pub fn merge_delivered(&self, ops: &[CollabOp]) -> Result<(), CollabError> {
        let mut state = self.state.lock().expect("collab document lock poisoned");
        let held = state.doc.element_count() + state.doc.pending_len();
        let novel = state.doc.novel_count(ops.iter());
        // The same bound `handle` and `apply_remote` enforce. Without it a
        // delivered batch is a way around the limit entirely.
        if held + novel > self.limits.max_document_chars {
            return Err(CollabError::DocumentFull {
                got: held,
                limit: self.limits.max_document_chars,
            });
        }
        state.edit(|doc| doc.apply_all(ops.iter().cloned()));
        drop(state);
        Ok(())
    }

    fn with_doc<T>(&self, f: impl FnOnce(&CollabText) -> T) -> T {
        let state = self.state.lock().expect("collab document lock poisoned");
        f(&state.doc)
    }

    fn broadcast_ops(&self, ops: &[CollabOp]) {
        if ops.is_empty() {
            return;
        }
        self.publish(&CollabServerMessage::Ops { ops: ops.to_vec() });
    }

    fn broadcast_presence(&self) {
        self.publish(&CollabServerMessage::Presence {
            participants: self.participants(),
        });
    }

    /// Publish a server message.
    ///
    /// A failed publish is logged, never returned: the operation is already in
    /// the document, and refusing it here would leave the authority and its
    /// editors disagreeing about whether it happened.
    ///
    /// But it is not simply dropped. The Redis backend returns before its own
    /// local fan-out when its publisher queue is full or closed, so a failure
    /// there costs the editors **on this node** the operation too — including
    /// the one who sent it, whose editor stays locked waiting for an echo that
    /// never comes, because a snapshot is only ever sent on join. So a failed
    /// publish falls back to the local topic directly. Editors on other nodes
    /// still miss it until they reconnect, which is what a broken Redis
    /// means; the node that applied the operation at least agrees with itself.
    fn publish(&self, message: &CollabServerMessage) {
        let Ok(json) = serde_json::to_string(message) else {
            tracing::warn!(key = %self.key, "collab: failed to encode server message");
            return;
        };
        let topic = self.topic();
        if let Err(error) = self.channels.publish(&topic, json.clone()) {
            let delivered = self
                .channels
                .backend()
                .ensure_topic(&topic)
                .send(json.into())
                .unwrap_or(0);
            tracing::warn!(
                key = %self.key,
                ?error,
                delivered,
                "collab: publish failed; delivered to this node's editors only"
            );
        }
    }
}

impl std::fmt::Debug for CollabDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollabDoc")
            .field("key", &self.key)
            .field("text", &self.text())
            .finish_non_exhaustive()
    }
}

/// One editor's membership of a document.
///
/// Holds the presence lease. Dropping it removes the editor, clears its
/// cursor, and tells the remaining editors.
pub struct CollabSession {
    doc: CollabDoc,
    actor: String,
    /// `Some` until the session drops. [`Drop`] takes it so the presence
    /// lease ends *before* the departure is announced.
    presence: Option<PresenceHandle>,
}

impl CollabSession {
    /// The actor id every character this editor types carries.
    #[must_use]
    pub const fn actor(&self) -> &str {
        self.actor.as_str()
    }

    /// The document this session edits.
    #[must_use]
    pub const fn document(&self) -> &CollabDoc {
        &self.doc
    }

    /// Apply a client message as this editor.
    ///
    /// # Errors
    ///
    /// Returns [`CollabError`] when the message exceeds the hub's limits.
    pub fn handle(&self, message: CollabClientMessage) -> Result<Vec<CollabOp>, CollabError> {
        self.doc.handle(&self.actor, message)
    }

    /// The opening message for this editor, naming its own actor id.
    ///
    /// # Panics
    ///
    /// Panics if the internal document mutex is poisoned.
    #[must_use]
    pub fn snapshot(&self) -> CollabServerMessage {
        match self.doc.snapshot() {
            CollabServerMessage::Snapshot {
                elems,
                pending,
                participants,
                ..
            } => CollabServerMessage::Snapshot {
                elems,
                pending,
                participants,
                actor: Some(self.actor.clone()),
            },
            other => other,
        }
    }

    /// Extend the presence lease. Call it from the socket's ping loop.
    pub fn refresh(&self) {
        if let Some(presence) = &self.presence {
            presence.refresh();
        }
    }
}

impl Drop for CollabSession {
    /// Ends the lease first, then announces the departure: the remaining
    /// editors must receive a participant list that no longer holds this one.
    ///
    /// Never panics. A `Drop` that unwrapped a poisoned lock would panic
    /// during another panic's unwind, and a panic while panicking aborts the
    /// process — one bad document would take the whole server down.
    fn drop(&mut self) {
        {
            let mut state = self
                .doc
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.cursors.remove(&self.actor);
            state.sessions = state.sessions.saturating_sub(1);
        }
        drop(self.presence.take());
        self.doc.broadcast_presence();

        // Nothing is evicted here on purpose. The registry points at the
        // document weakly, so it goes when the last `CollabDoc` handle does —
        // which is *after* the handler has persisted it. Removing it here
        // instead would let a reconnect in that window re-seed from the stale
        // row and then lose the departing editor's work to the late save.
    }
}

/// Drive one editor's WebSocket against `doc` until the socket closes.
///
/// This is the whole client protocol: join, send the snapshot, then forward
/// every broadcast to the socket and every socket message to the hub. An app
/// wires collaboration in one line of its `#[ws]` handler.
///
/// `actor` must be unique per connection — it is the id every character this
/// editor types carries.
///
/// ```rust,ignore
/// #[ws("/notes/{id}/collab")]
/// async fn collaborate(state: AppState, hub: CollabHub, id: Path<i64>) -> impl WsHandler {
///     let doc = hub.document(&doc_key("notes", *id, "body"));
///     // Through the injected entropy, never `Uuid::new_v4` (#1797).
///     let actor = state.entropy().uuid_v4().to_string();
///     move |socket| async move { serve_socket(&doc, actor, "Guest", socket).await }
/// }
/// ```
///
/// The snapshot is sent after the subscription opens, so an operation that
/// lands in between is delivered twice: once inside the snapshot and once as
/// an operation. Both the hub and a client that keys characters by id treat
/// the repeat as a no-op — the same idempotence a reconnect relies on.
pub async fn serve_socket(
    doc: &CollabDoc,
    actor: impl Into<String>,
    label: impl Into<String>,
    mut socket: crate::ws::WebSocket,
) {
    use crate::ws::Message;

    let mut updates = doc.subscribe();
    let session = doc.join(actor, label);

    if send_json(&mut socket, &session.snapshot()).await.is_err() {
        return;
    }

    // Presence entries expire on a TTL (30 s by default) and a background
    // sweep evicts them. Without a heartbeat every editor would vanish from
    // the participant list while still connected and typing.
    let mut heartbeat = tokio::time::interval(PRESENCE_REFRESH);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // A client that keeps falling behind the broadcast buffer costs a full
    // document resend each time. Give it a few chances, then let it reconnect.
    let mut lags = 0u32;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                session.refresh();
            }
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    continue;
                };
                match serde_json::from_str::<CollabClientMessage>(&text) {
                    Ok(client_message) => {
                        if let Err(error) = session.handle(client_message) {
                            let refusal = CollabServerMessage::Error {
                                message: error.to_string(),
                            };
                            if send_json(&mut socket, &refusal).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let refusal = CollabServerMessage::Error {
                            message: format!("unreadable message: {error}"),
                        };
                        if send_json(&mut socket, &refusal).await.is_err() {
                            break;
                        }
                    }
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(update) => {
                        let text = update.into_string();
                        // With a multi-replica backend (Redis), this is how
                        // another replica's operations reach us. Merge them
                        // into the local authority before forwarding: a
                        // client that saw a character here would otherwise
                        // anchor its next edit to one this replica has never
                        // heard of, and `handle` would refuse it as unknown.
                        // Locally-produced operations arrive here too and
                        // re-apply as no-ops, which is what idempotence is
                        // for.
                        if let Ok(CollabServerMessage::Ops { ops }) =
                            serde_json::from_str::<CollabServerMessage>(&text)
                            && let Err(error) = doc.merge_delivered(&ops)
                        {
                            tracing::warn!(
                                key = %doc.key(),
                                ?error,
                                "collab: refused a delivered batch; this replica is behind"
                            );
                        }
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    // Lagged: this editor fell behind the broadcast buffer,
                    // so it has missed operations. Resend the whole document
                    // rather than let it drift — but a resend is larger than
                    // the backlog it replaces, so a client that cannot keep
                    // up would lag again on every pass, taking the shared
                    // document lock each time. After a few tries, close and
                    // let it reconnect on a fresh subscription.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        lags += 1;
                        if lags > MAX_LAG_RESENDS {
                            tracing::warn!(
                                key = %doc.key(),
                                "collab: editor cannot keep up; closing the socket"
                            );
                            break;
                        }
                        if send_json(&mut socket, &session.snapshot()).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    drop(session);
}

/// Send one server message, or report that the socket is gone.
async fn send_json(
    socket: &mut crate::ws::WebSocket,
    message: &CollabServerMessage,
) -> Result<(), ()> {
    let json = serde_json::to_string(message).map_err(|_| ())?;
    socket
        .send(crate::ws::Message::Text(json.into()))
        .await
        .map_err(|_| ())
}
