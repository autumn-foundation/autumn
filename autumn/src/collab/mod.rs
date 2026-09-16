//! Conflict-free collaborative editing (issue #1806).
//!
//! Mark a model's text field `#[collaborative]` and its column holds a
//! [`CollabText`] — a text CRDT whose concurrent edits merge character by
//! character instead of clobbering each other. The merge is server
//! authoritative, deterministic and in-process: no external real-time
//! service, and no new dependency.
//!
//! # The layers
//!
//! - [`text`] — the merge. [`CollabText`] is a Replicated Growable Array;
//!   replicas that hold the same operations render the same text, in any
//!   delivery order.
//! - [`hub`] — the session. [`CollabHub`] keeps one live document per field
//!   instance, streams operations to every editor over
//!   [`Channels`](crate::channels::Channels), and reports who is editing and
//!   where their cursor is through [`Presence`](crate::presence::Presence).
//!   Needs the `presence` feature.
//! - [`resolver`] — the offline path. [`CollabResolver`] replaces
//!   last-write-wins for collaborative fields in the offline-sync engine, so
//!   an edit made offline merges on reconnect. Needs the `offline-sync`
//!   feature.
//! - [`registry`] — which columns hold a document, for surfaces with no
//!   compile-time view of the model.
//!
//! # Declaring a field
//!
//! ```rust,ignore
//! #[autumn_web::model(table = "notes")]
//! pub struct Note {
//!     #[id]
//!     pub id: i64,
//!     #[collaborative]
//!     pub body: autumn_web::collab::CollabText,
//! }
//! ```
//!
//! The macro generates `body_text()`, `body_insert(..)`, `body_remove(..)`,
//! `body_set_text(..)` and `body_merge(..)` on the model, plus the
//! field-name-keyed `collaborative(..)` accessors.
//!
//! See `docs/guide/collaboration.md` for the walkthrough and
//! `examples/collab-notes` for a runnable app.

pub mod registry;
pub mod text;

#[cfg(feature = "presence")]
pub mod hub;

#[cfg(feature = "offline-sync")]
pub mod resolver;

pub use registry::{
    CollaborativeColumnDescriptor, collaborative_columns_for_table,
    registered_collaborative_columns,
};
pub use text::{
    CollabEditError, CollabElement, CollabOp, CollabText, EMPTY_DOCUMENT, IMPORT_ACTOR,
    MAX_ACTOR_LEN, MAX_COUNTER, MAX_WIRE_ELEMENTS, MAX_WIRE_PENDING, OpId, OpIdParseError,
};

#[cfg(feature = "presence")]
pub use hub::{
    CollabClientMessage, CollabClose, CollabDoc, CollabError, CollabHub, CollabLimits,
    CollabParticipant, CollabServerMessage, CollabSession,
};

#[cfg(feature = "offline-sync")]
pub use resolver::CollabResolver;
