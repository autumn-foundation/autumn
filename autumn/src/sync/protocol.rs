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

/// Server-side cap on the number of changes accepted in one push batch.
///
/// Requests with more changes are rejected with `413 Payload Too Large`
/// (request body size is additionally bounded by axum's default body
/// limit). The client engine clamps its `push_batch_size` to this.
pub const MAX_PUSH_CHANGES: usize = 1000;

/// Server-side cap on the pull page size.
///
/// The router clamps the requested `limit` into `1..=MAX_PULL_LIMIT`; the
/// client engine clamps its `pull_batch_size` to the same bound so its
/// "short page means caught up" termination stays valid.
pub const MAX_PULL_LIMIT: i64 = 1000;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    /// Stable per-device id (UUID v4 string, generated on first store open).
    pub device_id: String,
    /// Journaled changes, oldest first.
    pub changes: Vec<Change>,
}

/// A server-side row (or tombstone) as returned by pull and conflict
/// resolutions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChangeOutcome {
    /// The change applied cleanly and was assigned `version`.
    Applied {
        /// The server version assigned to the accepted change.
        version: Version,
    },
    /// This `change_id` was already applied earlier (retry of a lost
    /// response); the server state is unchanged. `version` is the version
    /// the change was assigned when it was first applied, so the client can
    /// record the ack it never received — without it, the client's next
    /// edit of the same row would push a stale `base_version` and trigger a
    /// spurious conflict.
    AlreadyApplied {
        /// The version assigned when this change was originally applied.
        version: Version,
    },
    /// The change conflicted with a newer server row; the resolver ran and
    /// `row` is the winning row state (with a **new** version, so every
    /// device converges on it via its next pull).
    Resolved {
        /// The post-resolution row state.
        row: RemoteRow,
    },
}

/// Body of the `POST <base>/push` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The cursor this sync session *started* from. All pages of one
    /// paginated catch-up send the same value, so the server can tell a
    /// genuinely stale cursor (session started behind the tombstone GC
    /// horizon → `FullResyncRequired`) from mid-pagination progress whose
    /// per-page cursor is legitimately still below the horizon. Omitted →
    /// defaults to `cursor` (single-shot pulls behave as before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Version>,
}

const fn default_pull_limit() -> i64 {
    500
}

impl PullQuery {
    /// The cursor the resync check applies to: the explicit session-start
    /// cursor when given, otherwise the page cursor itself.
    #[must_use]
    pub fn session_start(&self) -> Version {
        self.session.unwrap_or(self.cursor)
    }
}

impl Default for PullQuery {
    fn default() -> Self {
        Self {
            cursor: 0,
            limit: default_pull_limit(),
            session: None,
        }
    }
}

/// Body of the `GET <base>/pull` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_change() -> Change {
        Change {
            change_id: "11111111-1111-4111-8111-111111111111".into(),
            collection: "notes".into(),
            pk: "n1".into(),
            op: Op::Upsert,
            payload: Some(serde_json::json!({"title": "hi"})),
            base_version: 3,
            updated_at: Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn push_request_round_trips() {
        let request = PushRequest {
            device_id: "device-a".into(),
            changes: vec![sample_change()],
        };
        let json = serde_json::to_string(&request).unwrap();
        let back: PushRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, request);
        assert!(json.contains(r#""op":"upsert""#), "snake_case ops: {json}");
    }

    #[test]
    fn delete_change_omits_payload() {
        let mut change = sample_change();
        change.op = Op::Delete;
        change.payload = None;
        let json = serde_json::to_string(&change).unwrap();
        assert!(
            !json.contains("payload"),
            "no payload key for deletes: {json}"
        );
        let back: Change = serde_json::from_str(&json).unwrap();
        assert_eq!(back, change);
    }

    #[test]
    fn change_outcomes_are_status_tagged() {
        let applied = serde_json::to_value(ChangeOutcome::Applied { version: 7 }).unwrap();
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["version"], 7);
        let deduped = serde_json::to_value(ChangeOutcome::AlreadyApplied { version: 7 }).unwrap();
        assert_eq!(deduped["status"], "already_applied");
        assert_eq!(
            deduped["version"], 7,
            "already_applied must carry the originally assigned version"
        );
    }

    #[test]
    fn pull_response_round_trips_both_variants() {
        let ok = PullResponse::Ok {
            rows: vec![RemoteRow {
                collection: "notes".into(),
                pk: "n1".into(),
                payload: None,
                version: 9,
                deleted: true,
                updated_at: Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap(),
                device_id: "device-a".into(),
            }],
            next_cursor: 9,
            tombstone_horizon: 4,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains(r#""status":"ok""#));
        assert_eq!(serde_json::from_str::<PullResponse>(&json).unwrap(), ok);

        let resync = PullResponse::FullResyncRequired {
            tombstone_horizon: 4,
        };
        let json = serde_json::to_string(&resync).unwrap();
        assert!(json.contains(r#""status":"full_resync_required""#));
        assert_eq!(serde_json::from_str::<PullResponse>(&json).unwrap(), resync);
    }

    #[test]
    fn pull_query_defaults() {
        let query: PullQuery = serde_urlencoded::from_str("").unwrap();
        assert_eq!(query, PullQuery::default());
        assert_eq!(query.cursor, 0);
        assert_eq!(query.limit, 500);
        assert_eq!(query.session, None);
        assert_eq!(
            query.session_start(),
            0,
            "no session marker means the page cursor is the session start"
        );
        let query: PullQuery = serde_urlencoded::from_str("cursor=12&limit=50").unwrap();
        assert_eq!(query.cursor, 12);
        assert_eq!(query.limit, 50);
        assert_eq!(query.session_start(), 12);
        let query: PullQuery = serde_urlencoded::from_str("cursor=12&limit=50&session=3").unwrap();
        assert_eq!(query.session, Some(3));
        assert_eq!(
            query.session_start(),
            3,
            "an explicit session-start cursor wins over the page cursor"
        );
    }
}
