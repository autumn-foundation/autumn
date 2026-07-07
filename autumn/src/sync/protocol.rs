//! Wire protocol for the sync endpoints.
//!
//! Serde types shared by the client engine ([`crate::sync::SyncEngine`]) and
//! the server router ([`crate::sync::server::router`]). The protocol is two
//! HTTP calls:
//!
//! - `POST <base>/push` with a [`PushRequest`] → [`PushResponse`] (one
//!   [`ChangeOutcome`] per change, in order).
//! - `GET <base>/pull?cursor=N&limit=M` → [`PullResponse`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Server-assigned change sequence number. `0` means "never synced".
pub type Version = i64;

/// The kind of local write a [`Change`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Insert-or-update of a row's JSON payload.
    Upsert,
    /// Deletion, replicated as a tombstone.
    Delete,
}

/// One journaled local write, pushed to the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// Client-generated unique id (UUID v4 string) used for at-least-once
    /// dedup on the server.
    pub change_id: String,
    /// Namespace for the row (e.g. `"notes"`).
    pub collection: String,
    /// Row primary key within the collection. Client-generated; use UUIDs,
    /// never serial integers.
    pub pk: String,
    /// Upsert or delete.
    pub op: Op,
    /// JSON payload for upserts; `None` for deletes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// The server version this write was based on (`0` for a row the
    /// device has never seen synced). A mismatch with the server's current
    /// version marks a conflict.
    pub base_version: Version,
    /// Wall-clock time of the local write, used only by conflict
    /// resolvers — never for change-feed ordering.
    pub updated_at: DateTime<Utc>,
}

/// Body of `POST <base>/push`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushRequest {
    /// Stable per-device id (UUID v4 string, generated on first store open).
    pub device_id: String,
    /// Journaled changes, oldest first.
    pub changes: Vec<Change>,
}

/// A server-side row (or tombstone) as returned by pull and conflict
/// resolutions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteRow {
    /// Namespace for the row.
    pub collection: String,
    /// Row primary key within the collection.
    pub pk: String,
    /// JSON payload; `None` for tombstones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Server-assigned version (monotonic across all rows).
    pub version: Version,
    /// `true` if the row is a tombstone.
    pub deleted: bool,
    /// `updated_at` of the write that produced this row state.
    pub updated_at: DateTime<Utc>,
    /// Device that produced this row state (empty for server-side writes).
    pub device_id: String,
}

/// Per-change result inside a [`PushResponse`], same order as the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChangeOutcome {
    /// The change applied cleanly and was assigned `version`.
    Applied {
        /// The server version assigned to the accepted change.
        version: Version,
    },
    /// This `change_id` was already applied earlier (retry of a lost
    /// response); the server state is unchanged.
    AlreadyApplied,
    /// The change conflicted with a newer server row; the resolver ran and
    /// `row` is the winning row state (with a **new** version, so every
    /// device converges on it via its next pull).
    Resolved {
        /// The post-resolution row state.
        row: RemoteRow,
    },
}

/// Body of the `POST <base>/push` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushResponse {
    /// One outcome per pushed change, in request order.
    pub outcomes: Vec<ChangeOutcome>,
}

/// Query string of `GET <base>/pull`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullQuery {
    /// Return rows with version strictly greater than this.
    #[serde(default)]
    pub cursor: Version,
    /// Page size cap.
    #[serde(default = "default_pull_limit")]
    pub limit: i64,
}

const fn default_pull_limit() -> i64 {
    500
}

impl Default for PullQuery {
    fn default() -> Self {
        Self {
            cursor: 0,
            limit: default_pull_limit(),
        }
    }
}

/// Body of the `GET <base>/pull` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PullResponse {
    /// A page of rows newer than the requested cursor.
    Ok {
        /// Rows ordered by ascending version. A page shorter than the
        /// requested limit means the client caught up.
        rows: Vec<RemoteRow>,
        /// The cursor to persist after applying `rows`.
        next_cursor: Version,
        /// Minimum version a client cursor may trail without requiring a
        /// full resync (advanced by tombstone GC).
        tombstone_horizon: Version,
    },
    /// The client's cursor predates the tombstone GC horizon; it must clear
    /// its synced rows (pending changes are preserved) and re-pull from
    /// cursor `0`.
    FullResyncRequired {
        /// The server's current tombstone GC horizon.
        tombstone_horizon: Version,
    },
}
