//! Offline-first local storage and background synchronization.
//!
//! This module gives occasionally-connected apps (the primary consumer is
//! the in-process Tauri mobile shell, issue #1508) a local, in-process
//! `SQLite` store plus a sync engine that reconciles it with a remote Autumn
//! app backed by `PostgreSQL`.
//!
//! # Architecture
//!
//! - [`SyncStore`] — the local store. App data lives in it as JSON payloads
//!   keyed by `(collection, pk)`. Every write also journals a pending change
//!   in the same `SQLite` transaction (write-through change tracking), and
//!   deletes are recorded as tombstone rows so they replicate.
//! - [`SyncEngine`] — the client sync loop: push pending changes, then pull
//!   rows newer than the local cursor. At-least-once with server-side
//!   dedup; safe to retry; tolerates being offline indefinitely.
//! - [`server`] — an [`axum::Router`] with `POST /push` + `GET /pull`,
//!   mountable on the remote app via `AppBuilder::nest("/sync", ...)`,
//!   persisting into Postgres shadow tables (or an in-memory backend for
//!   tests and demos).
//! - [`ConflictResolver`] — pluggable conflict policy, applied server-side
//!   when a pushed change was based on a stale version. The default is
//!   last-write-wins on the conflicting writes' `updated_at`, with the
//!   device id as a deterministic tiebreak.
//!
//! Ordering is server-authoritative: the server assigns every accepted
//! change a monotonically increasing version from one global sequence, and
//! clients pull "rows with version greater than my cursor". Device clocks
//! never order the change feed.
//!
//! Server-side data is partitioned per tenant/principal by a scope key
//! ([`SyncScope`]) derived from the **authenticated** request — never
//! client-supplied. The default [`server::router`] is single-tenant (every
//! request shares one constant "global" scope); multi-user deployments
//! mount [`server::scoped_router`] and have their auth middleware insert a
//! `SyncScope` derived from the authenticated user.
//!
//! # Client wiring
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use autumn_web::sync::{SyncConfig, SyncEngine, SyncStore};
//!
//! # async fn wire() -> Result<(), autumn_web::sync::SyncError> {
//! // Open (or create) the local store, e.g. inside the app's data dir.
//! let store = SyncStore::open("/data/app/sync.db")?;
//!
//! // Reads and writes work fully offline; every write is journaled.
//! store.put(
//!     "notes",
//!     "6b3f2c1e-5f43-4a67-9d31-2f4f6a8e9b10", // client-generated UUID pk
//!     &serde_json::json!({ "title": "works offline" }),
//! )?;
//! let notes: Vec<(String, serde_json::Value)> = store.list("notes")?;
//!
//! // Sync in the background whenever connectivity allows.
//! let engine = SyncEngine::new(store, SyncConfig::new("https://example.com/sync"));
//! let _task = engine.spawn_background(Duration::from_secs(30));
//! # Ok(())
//! # }
//! ```
//!
//! The server side mounts in one line — see [`server::router`].
//!
//! # Scope
//!
//! Existing diesel `#[repository]` models are **not** transparently
//! offline. Data the app wants available offline goes through the
//! [`SyncStore`] API and is mirrored to the remote shadow tables.

pub mod engine;
pub mod protocol;
pub mod resolver;
pub mod server;
pub mod store;

pub use engine::{SyncConfig, SyncEngine, SyncReport, SyncStatus};
pub use protocol::{
    Change, ChangeOutcome, MAX_PULL_LIMIT, MAX_PUSH_CHANGES, Op, PullQuery, PullResponse,
    PushRequest, PushResponse, RemoteRow, Version,
};
pub use resolver::{ConflictResolver, LwwResolver, Resolution};
pub use server::{MemorySyncBackend, PgSyncBackend, SyncBackend, SyncScope};
pub use store::SyncStore;

/// Errors produced by the sync store, engine, and server backends.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// A local `SQLite` store operation failed.
    #[error("sync store error: {0}")]
    Store(String),
    /// A payload could not be serialized or deserialized.
    #[error("sync serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// The remote server could not be reached (offline, DNS, timeout).
    #[error("sync transport error: {0}")]
    Transport(String),
    /// The remote server answered with an error response.
    #[error("sync server error: {0}")]
    Server(String),
    /// A server-side backend (Postgres or in-memory) operation failed.
    #[error("sync backend error: {0}")]
    Backend(String),
    /// A request violated the sync protocol (client error — the server
    /// rejects it with a 4xx status and applies nothing). Example: an
    /// [`Op::Upsert`] change without a payload.
    #[error("sync protocol error: {0}")]
    Protocol(String),
}
