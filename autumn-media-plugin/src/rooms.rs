//! Small mesh (no-`SFU`) multi-participant call rooms and their `MediaMTX`
//! `WHIP`/`WHEP` signaling surface.
//!
//! A **room** is an ephemeral, in-memory rendezvous: participants join, each
//! publishes their own `WebRTC` track to a `MediaMTX` path and subscribes to
//! every *other* participant's path, forming a full mesh. Because the topology
//! is a mesh, each of `N` participants holds `N - 1` subscriptions —
//! `O(N * (N - 1))` connections room-wide — so the room is hard-capped at
//! [`config::DEFAULT_ROOM_MAX_PARTICIPANTS`](crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS)
//! (6) participants. A selective-forwarding unit (`SFU`) that would lift that
//! cap is explicitly **out of scope** for this slice.
//!
//! # Isolation & `MediaMTX` paths
//!
//! Every participant maps to one `MediaMTX` path. The path carries an optional
//! `namespace` segment so two deployments (or tenants) sharing a `MediaMTX`
//! never collide:
//!
//! - namespace empty → `room/{room_id}/{participant_id}`
//! - namespace set → `room/{namespace}/{room_id}/{participant_id}`
//!
//! `room_id` and `participant_id` are server-minted v4 UUIDs (slug-safe), while
//! `namespace` is caller/operator-supplied and is the real guard target:
//! [`validate_room_segment`] rejects empty, `.`, `..`, and any character
//! outside `[A-Za-z0-9_-]` before a path is composed, so a crafted namespace
//! can never traverse the `MediaMTX` path space. Room lookups key on the
//! `(namespace, room_id)` pair and **fail closed**: a request under the wrong
//! namespace resolves to [`RoomError::RoomNotFound`], never another namespace's
//! room.
//!
//! # Operator requirements
//!
//! - **`MediaMTX` path matcher**: this crate ships no `mediamtx.yml` (the
//!   template lives in the consumer app), so an operator enabling rooms must add
//!   a `path: "~^room/.+$"` matcher alongside the live `~^live/.+$` one, so
//!   `MediaMTX` accepts the `room/…` publish/read paths.
//! - **`CSP`**: an embedding page's `connect-src` must allow the `MediaMTX`
//!   `WebRTC` origin (`:8889` by default), exactly as the broadcast player does.
//!
//! # Single-process limitation
//!
//! [`InMemoryRoomStore`] keeps all room state in process memory, so it is
//! **single-process only** — a multi-process / multi-replica deployment needs a
//! shared backing store. [`RoomStore`] is the swap seam for that: a networked
//! (and necessarily async) store would revisit these synchronous signatures.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use serde::{Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

use autumn_web::reexports::axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use autumn_web::route_listing::RouteInfo;
use autumn_web::{AppState, AutumnError, AutumnResult};

use crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS;
use crate::transport::MediaUrls;

// ── Path segment validation ──────────────────────────────────────────────────

/// Validate one `MediaMTX` room-path segment (namespace / room / participant).
///
/// Rejects an empty segment, the dot segments `.` / `..`, and any segment
/// containing a character outside `[A-Za-z0-9_-]` — the join-within-root
/// equivalent guard for the transport path surface, so a crafted namespace can
/// never traverse the `MediaMTX` path space.
///
/// # Errors
///
/// Returns [`RoomError::InvalidSegment`] for any rejected segment.
pub fn validate_room_segment(segment: &str) -> Result<(), RoomError> {
    let ok = !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(RoomError::InvalidSegment {
            value: segment.to_owned(),
        })
    }
}

/// Compose the `MediaMTX` path for a room participant, validating every
/// caller-influenced segment first.
///
/// The namespace is validated only when non-empty (an empty namespace inserts
/// no segment); `room_id` and `participant_id` are always validated.
///
/// # Errors
///
/// Returns [`RoomError::InvalidSegment`] when any segment fails
/// [`validate_room_segment`].
pub fn room_participant_path(
    namespace: &str,
    room_id: &str,
    participant_id: &str,
) -> Result<String, RoomError> {
    validate_room_segment(room_id)?;
    validate_room_segment(participant_id)?;
    if namespace.is_empty() {
        Ok(format!("room/{room_id}/{participant_id}"))
    } else {
        validate_room_segment(namespace)?;
        Ok(format!("room/{namespace}/{room_id}/{participant_id}"))
    }
}

// ── Session token ─────────────────────────────────────────────────────────────

/// An opaque, single-use-per-session room token minted for a joining
/// participant and presented on leave.
///
/// The inner value is **never** rendered by [`std::fmt::Debug`] (it prints
/// `SessionToken("<redacted>")`, mirroring the storage-layer redaction
/// discipline) so it cannot leak into logs, but it **is** serialized verbatim,
/// because the join response returns it to the participant that owns it.
/// Verification is always by value **and** expiry against the stored
/// participant record, using a constant-time compare.
#[derive(Clone)]
pub struct SessionToken(String);

impl SessionToken {
    /// Mint a fresh random token (a hyphen-free v4 UUID, ~122 bits of entropy).
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }

    /// Borrow the raw token value (for a constant-time verify or to return it
    /// to the owning participant).
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("SessionToken")
            .field(&"<redacted>")
            .finish()
    }
}

impl Serialize for SessionToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by the room primitive and its [`RoomStore`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RoomError {
    /// No room exists for the requested `(namespace, room_id)` pair. Also
    /// returned for a namespace mismatch (rooms never leak across namespaces).
    #[error("room not found")]
    RoomNotFound,

    /// The room is already at its participant cap.
    #[error("room is full (max {max} participants)")]
    RoomFull {
        /// The room's participant cap.
        max: usize,
    },

    /// No participant with the given id exists in the room.
    #[error("participant not found")]
    ParticipantNotFound,

    /// The presented session token was wrong or expired. The message is
    /// deliberately token-free so it can never echo a secret.
    #[error("invalid or expired session token")]
    Unauthorized,

    /// A caller-supplied path segment (namespace / room / participant) was
    /// invalid (see [`validate_room_segment`]).
    #[error("invalid room path segment `{value}`")]
    InvalidSegment {
        /// The rejected segment (a path label, never a token).
        value: String,
    },

    /// A room was requested with a participant cap outside `1..=cap`.
    #[error("max_participants {requested} is out of range (1..={cap})")]
    InvalidMaxParticipants {
        /// The rejected value.
        requested: usize,
        /// The hard cap.
        cap: usize,
    },
}

impl RoomError {
    /// Map this error to an [`AutumnError`] carrying the appropriate HTTP
    /// status, keeping every message token-free.
    ///
    /// This is an inherent method (not a `From` impl) because autumn-web already
    /// carries a blanket `From<E: std::error::Error>` for [`AutumnError`] that
    /// would otherwise flatten every `RoomError` to a `500`; mapping explicitly
    /// preserves the `404`/`409`/`401`/`400` distinctions. Handlers call it via
    /// `.map_err(RoomError::into_autumn)`.
    #[must_use]
    pub fn into_autumn(self) -> AutumnError {
        match self {
            Self::RoomNotFound | Self::ParticipantNotFound => {
                AutumnError::not_found_msg(self.to_string())
            }
            // A full room is a genuine conflict (409), distinct from a bad request.
            Self::RoomFull { .. } => AutumnError::conflict_msg(self.to_string()),
            Self::Unauthorized => AutumnError::unauthorized_msg(self.to_string()),
            Self::InvalidSegment { .. } | Self::InvalidMaxParticipants { .. } => {
                AutumnError::bad_request_msg(self.to_string())
            }
        }
    }
}

// ── Participant / room state ──────────────────────────────────────────────────

/// One participant in a room (internal state — carries the secret token).
#[derive(Debug)]
struct RoomParticipant {
    id: String,
    display_name: Option<String>,
    joined_at: DateTime<Utc>,
    token: SessionToken,
    token_expires_at: DateTime<Utc>,
}

impl RoomParticipant {
    /// Constant-time-verify a presented token against this participant, treating
    /// an expired token as a failure. `now` is passed in so verification is
    /// deterministic under test.
    fn verify(&self, candidate: &str, now: DateTime<Utc>) -> bool {
        now < self.token_expires_at
            && autumn_web::auth::constant_time_eq(
                candidate.as_bytes(),
                self.token.expose().as_bytes(),
            )
    }
}

/// A public, token-free view of a participant, safe to serialize into a roster.
#[derive(Clone, Debug, Serialize)]
pub struct ParticipantView {
    /// The participant's id.
    pub id: String,
    /// The participant's optional display name.
    pub display_name: Option<String>,
    /// When the participant joined.
    pub joined_at: DateTime<Utc>,
}

/// A room's public snapshot (never carries a token).
#[derive(Clone, Debug, Serialize)]
pub struct RoomSnapshot {
    /// The room id.
    pub id: String,
    /// The room's namespace (omitted from JSON when empty).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    /// The room's participant cap.
    pub max_participants: usize,
    /// When the room was created.
    pub created_at: DateTime<Utc>,
    /// The current roster (token-free), ordered by join time then id.
    pub participants: Vec<ParticipantView>,
}

/// A room and its live roster (internal state).
struct Room {
    id: String,
    namespace: String,
    max_participants: usize,
    created_at: DateTime<Utc>,
    participants: HashMap<String, RoomParticipant>,
}

impl Room {
    fn is_full(&self) -> bool {
        self.participants.len() >= self.max_participants
    }

    /// Build a token-free snapshot with a deterministic roster order.
    fn snapshot(&self) -> RoomSnapshot {
        let mut participants: Vec<ParticipantView> = self
            .participants
            .values()
            .map(|participant| ParticipantView {
                id: participant.id.clone(),
                display_name: participant.display_name.clone(),
                joined_at: participant.joined_at,
            })
            .collect();
        participants.sort_by(|a, b| a.joined_at.cmp(&b.joined_at).then_with(|| a.id.cmp(&b.id)));
        RoomSnapshot {
            id: self.id.clone(),
            namespace: self.namespace.clone(),
            max_participants: self.max_participants,
            created_at: self.created_at,
            participants,
        }
    }
}

// ── Store seam ────────────────────────────────────────────────────────────────

/// The outcome of a successful join: the minted participant identity plus a
/// token-free snapshot of the room (transport-agnostic — URL composition lives
/// in [`RoomService`], never in the store).
#[derive(Debug)]
pub struct JoinRecord {
    /// The minted participant id.
    pub participant_id: String,
    /// The minted session token (returned to the joining participant).
    pub token: SessionToken,
    /// When the token expires.
    pub token_expires_at: DateTime<Utc>,
    /// A snapshot of the room *after* the join (includes the new participant).
    pub room: RoomSnapshot,
}

/// The pluggable room-state store — the swap seam for a shared/durable backend.
///
/// Every method keys on the `(namespace, room_id)` pair and **fails closed**: a
/// namespace mismatch resolves to [`RoomError::RoomNotFound`], never another
/// namespace's room. The signatures are synchronous because the shipped
/// [`InMemoryRoomStore`] is in-memory; a networked backing store would revisit
/// them (returning futures).
pub trait RoomStore: Send + Sync {
    /// Create a room in `namespace` capped at `max_participants`, returning its
    /// snapshot.
    ///
    /// `max_participants` must be within `1..=`
    /// [`DEFAULT_ROOM_MAX_PARTICIPANTS`](crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS)
    /// — 6 is the absolute mesh ceiling (no SFU), a per-room seat count is
    /// configurable only within that range, and `0` is nonsense.
    ///
    /// # Errors
    ///
    /// [`RoomError::InvalidSegment`] for an invalid non-empty namespace, and
    /// [`RoomError::InvalidMaxParticipants`] when `max_participants` is `0` or
    /// exceeds the absolute ceiling.
    fn create_room(
        &self,
        namespace: &str,
        max_participants: usize,
    ) -> Result<RoomSnapshot, RoomError>;

    /// Join the room `(namespace, room_id)`, minting a participant + token that
    /// stays valid for `token_ttl`.
    ///
    /// # Errors
    ///
    /// [`RoomError::RoomNotFound`] (including a namespace mismatch) or
    /// [`RoomError::RoomFull`] when the room is at capacity.
    fn join_room(
        &self,
        namespace: &str,
        room_id: &str,
        display_name: Option<String>,
        token_ttl: Duration,
    ) -> Result<JoinRecord, RoomError>;

    /// Remove a participant from the room after verifying its token.
    ///
    /// # Errors
    ///
    /// [`RoomError::RoomNotFound`], [`RoomError::ParticipantNotFound`], or
    /// [`RoomError::Unauthorized`] on a wrong/expired token.
    fn leave_room(
        &self,
        namespace: &str,
        room_id: &str,
        participant_id: &str,
        token: &str,
    ) -> Result<(), RoomError>;

    /// Return the current roster snapshot for `(namespace, room_id)`.
    ///
    /// # Errors
    ///
    /// [`RoomError::RoomNotFound`] (including a namespace mismatch).
    fn roster(&self, namespace: &str, room_id: &str) -> Result<RoomSnapshot, RoomError>;
}

/// A single-process, in-memory [`RoomStore`].
///
/// Keyed on `(namespace, room_id)` under one `RwLock`. When the last
/// participant leaves a room, the room is dropped so an idle room never lingers.
pub struct InMemoryRoomStore {
    rooms: RwLock<HashMap<(String, String), Room>>,
    hard_cap: usize,
}

impl InMemoryRoomStore {
    /// Create an empty store whose rooms may hold up to `hard_cap` participants.
    ///
    /// `hard_cap` is further bounded by the absolute mesh ceiling: a room can
    /// never seat more than
    /// [`DEFAULT_ROOM_MAX_PARTICIPANTS`](crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS)
    /// (6, no SFU) regardless of `hard_cap`, so with a larger `hard_cap`
    /// [`create_room`](RoomStore::create_room) still rejects any request above 6
    /// as a backstop.
    #[must_use]
    pub fn new(hard_cap: usize) -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            hard_cap,
        }
    }
}

impl Default for InMemoryRoomStore {
    fn default() -> Self {
        Self::new(DEFAULT_ROOM_MAX_PARTICIPANTS)
    }
}

impl RoomStore for InMemoryRoomStore {
    fn create_room(
        &self,
        namespace: &str,
        max_participants: usize,
    ) -> Result<RoomSnapshot, RoomError> {
        if !namespace.is_empty() {
            validate_room_segment(namespace)?;
        }
        // The mesh is O(N²), so the absolute ceiling is a fixed
        // `DEFAULT_ROOM_MAX_PARTICIPANTS` (6) — enforced here structurally, so
        // this backstop stays non-vacuous even if a larger `hard_cap` slipped
        // past the builder/config clamp. A 0-seat room is nonsense. Both are
        // rejected outright (no lower clamp).
        let ceiling = self.hard_cap.min(DEFAULT_ROOM_MAX_PARTICIPANTS);
        if max_participants == 0 || max_participants > ceiling {
            return Err(RoomError::InvalidMaxParticipants {
                requested: max_participants,
                cap: ceiling,
            });
        }
        let id = Uuid::new_v4().to_string();
        let room = Room {
            id: id.clone(),
            namespace: namespace.to_owned(),
            max_participants,
            created_at: Utc::now(),
            participants: HashMap::new(),
        };
        let snapshot = room.snapshot();
        self.rooms
            .write()
            .expect("room store lock poisoned")
            .insert((namespace.to_owned(), id), room);
        Ok(snapshot)
    }

    fn join_room(
        &self,
        namespace: &str,
        room_id: &str,
        display_name: Option<String>,
        token_ttl: Duration,
    ) -> Result<JoinRecord, RoomError> {
        let mut rooms = self.rooms.write().expect("room store lock poisoned");
        let room = rooms
            .get_mut(&(namespace.to_owned(), room_id.to_owned()))
            .ok_or(RoomError::RoomNotFound)?;
        if room.is_full() {
            return Err(RoomError::RoomFull {
                max: room.max_participants,
            });
        }
        let now = Utc::now();
        let participant_id = Uuid::new_v4().to_string();
        let token = SessionToken::generate();
        let token_expires_at = now + token_ttl;
        room.participants.insert(
            participant_id.clone(),
            RoomParticipant {
                id: participant_id.clone(),
                display_name,
                joined_at: now,
                token: token.clone(),
                token_expires_at,
            },
        );
        let snapshot = room.snapshot();
        drop(rooms);
        Ok(JoinRecord {
            participant_id,
            token,
            token_expires_at,
            room: snapshot,
        })
    }

    fn leave_room(
        &self,
        namespace: &str,
        room_id: &str,
        participant_id: &str,
        token: &str,
    ) -> Result<(), RoomError> {
        let mut rooms = self.rooms.write().expect("room store lock poisoned");
        let key = (namespace.to_owned(), room_id.to_owned());
        let room = rooms.get_mut(&key).ok_or(RoomError::RoomNotFound)?;
        let participant = room
            .participants
            .get(participant_id)
            .ok_or(RoomError::ParticipantNotFound)?;
        if !participant.verify(token, Utc::now()) {
            return Err(RoomError::Unauthorized);
        }
        room.participants.remove(participant_id);
        // Drop an emptied room so idle rooms never accumulate.
        let now_empty = room.participants.is_empty();
        if now_empty {
            rooms.remove(&key);
        }
        drop(rooms);
        Ok(())
    }

    fn roster(&self, namespace: &str, room_id: &str) -> Result<RoomSnapshot, RoomError> {
        self.rooms
            .read()
            .expect("room store lock poisoned")
            .get(&(namespace.to_owned(), room_id.to_owned()))
            .map(Room::snapshot)
            .ok_or(RoomError::RoomNotFound)
    }
}

// ── Service (URL composition + AppState extension) ────────────────────────────

/// A participant's own publish target.
#[derive(Clone, Debug, Serialize)]
pub struct PublishTarget {
    /// The `MediaMTX` path the participant publishes to.
    pub path: String,
    /// The `WHIP` publish URL for that path.
    pub whip_url: String,
}

/// A subscribe target for one *other* participant in the mesh.
#[derive(Clone, Debug, Serialize)]
pub struct SubscribeTarget {
    /// The peer participant's id.
    pub participant_id: String,
    /// The peer's `MediaMTX` path.
    pub path: String,
    /// The `WHEP` read (subscribe) URL for that path.
    pub whep_url: String,
}

/// The full result of a join: identity, token, and the mesh transport targets.
#[derive(Debug, Serialize)]
pub struct JoinResponse {
    /// The minted participant id.
    pub participant_id: String,
    /// The minted session token (returned only to the joining participant).
    pub session_token: SessionToken,
    /// When the token expires.
    pub token_expires_at: DateTime<Utc>,
    /// The participant's own publish target.
    pub publish: PublishTarget,
    /// One subscribe target per *other* participant (the mesh).
    pub subscribe: Vec<SubscribeTarget>,
    /// A token-free snapshot of the room after the join.
    pub room: RoomSnapshot,
}

/// The programmatic room API installed on `AppState`.
///
/// Owns the [`RoomStore`] plus the [`MediaUrls`] used to compose each
/// participant's `WHIP`/`WHEP` URLs, and carries the deployment's namespace,
/// token TTL, and participant cap. `Clone` is cheap (the store is an `Arc`),
/// mirroring how [`MediaWorkflows`](crate::workflows::MediaWorkflows) is
/// installed and read.
#[derive(Clone)]
pub struct RoomService {
    store: Arc<dyn RoomStore>,
    urls: MediaUrls,
    namespace: String,
    token_ttl: Duration,
    max_participants: usize,
}

impl RoomService {
    /// Build a room service.
    #[must_use]
    pub fn new(
        store: Arc<dyn RoomStore>,
        urls: MediaUrls,
        namespace: impl Into<String>,
        token_ttl: Duration,
        max_participants: usize,
    ) -> Self {
        Self {
            store,
            urls,
            namespace: namespace.into(),
            token_ttl,
            max_participants,
        }
    }

    /// The configured namespace (empty string = none).
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Create a new room under the service's namespace and participant cap.
    ///
    /// # Errors
    ///
    /// Propagates any [`RoomError`] from the store.
    pub fn create(&self) -> Result<RoomSnapshot, RoomError> {
        self.store
            .create_room(&self.namespace, self.max_participants)
    }

    /// Join `room_id`, composing the joiner's publish target and one subscribe
    /// target per existing participant (the mesh).
    ///
    /// # Errors
    ///
    /// Propagates any [`RoomError`] from the store, or
    /// [`RoomError::InvalidSegment`] if a path cannot be composed.
    pub fn join(
        &self,
        room_id: &str,
        display_name: Option<String>,
    ) -> Result<JoinResponse, RoomError> {
        let record =
            self.store
                .join_room(&self.namespace, room_id, display_name, self.token_ttl)?;

        let publish_path = room_participant_path(&self.namespace, room_id, &record.participant_id)?;
        let publish = PublishTarget {
            whip_url: self.urls.whip_publish_url_for_path(&publish_path),
            path: publish_path,
        };

        let mut subscribe = Vec::new();
        for peer in &record.room.participants {
            if peer.id == record.participant_id {
                continue;
            }
            let peer_path = room_participant_path(&self.namespace, room_id, &peer.id)?;
            subscribe.push(SubscribeTarget {
                participant_id: peer.id.clone(),
                whep_url: self.urls.whep_read_url(&peer_path),
                path: peer_path,
            });
        }

        Ok(JoinResponse {
            participant_id: record.participant_id,
            session_token: record.token,
            token_expires_at: record.token_expires_at,
            publish,
            subscribe,
            room: record.room,
        })
    }

    /// Leave `room_id` as `participant_id`, verifying `token`.
    ///
    /// # Errors
    ///
    /// Propagates any [`RoomError`] from the store.
    pub fn leave(&self, room_id: &str, participant_id: &str, token: &str) -> Result<(), RoomError> {
        self.store
            .leave_room(&self.namespace, room_id, participant_id, token)
    }

    /// Return the roster for `room_id`.
    ///
    /// # Errors
    ///
    /// Propagates any [`RoomError`] from the store.
    pub fn roster(&self, room_id: &str) -> Result<RoomSnapshot, RoomError> {
        self.store.roster(&self.namespace, room_id)
    }
}

// ── HTTP surface ──────────────────────────────────────────────────────────────

/// Body of a room-join request.
#[derive(Debug, serde::Deserialize)]
pub struct JoinRequest {
    /// Optional display name for the joining participant.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Body of a room-leave request.
///
/// [`std::fmt::Debug`] renders `participant_id` verbatim but redacts
/// `session_token` (it prints `<redacted>`, mirroring [`SessionToken`] and the
/// storage-layer redaction discipline) so a request body cannot leak the secret
/// into logs.
#[derive(serde::Deserialize)]
pub struct LeaveRequest {
    /// The participant id being removed.
    pub participant_id: String,
    /// The session token minted at join time.
    pub session_token: String,
}

impl std::fmt::Debug for LeaveRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LeaveRequest")
            .field("participant_id", &self.participant_id)
            .field("session_token", &"<redacted>")
            .finish()
    }
}

/// Resolve the installed [`RoomService`] or fail with a `500`.
fn room_service(state: &AppState) -> AutumnResult<Arc<RoomService>> {
    state.extension::<RoomService>().ok_or_else(|| {
        AutumnError::internal_server_error_msg("RoomService extension is not installed")
    })
}

/// `POST {prefix}/rooms` — create a room.
async fn rooms_create(State(state): State<AppState>) -> AutumnResult<Json<RoomSnapshot>> {
    let snapshot = room_service(&state)?
        .create()
        .map_err(RoomError::into_autumn)?;
    Ok(Json(snapshot))
}

/// `POST {prefix}/rooms/{room_id}/join` — join a room.
async fn rooms_join(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<JoinRequest>,
) -> AutumnResult<Json<JoinResponse>> {
    let response = room_service(&state)?
        .join(&room_id, body.display_name)
        .map_err(RoomError::into_autumn)?;
    Ok(Json(response))
}

/// `POST {prefix}/rooms/{room_id}/leave` — leave a room.
async fn rooms_leave(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Json(body): Json<LeaveRequest>,
) -> AutumnResult<Json<RoomLeaveResponse>> {
    room_service(&state)?
        .leave(&room_id, &body.participant_id, &body.session_token)
        .map_err(RoomError::into_autumn)?;
    Ok(Json(RoomLeaveResponse { left: true }))
}

/// `GET {prefix}/rooms/{room_id}` — the room roster.
async fn rooms_roster(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> AutumnResult<Json<RoomSnapshot>> {
    let snapshot = room_service(&state)?
        .roster(&room_id)
        .map_err(RoomError::into_autumn)?;
    Ok(Json(snapshot))
}

/// Acknowledgement returned by a successful leave.
#[derive(Debug, Serialize)]
pub struct RoomLeaveResponse {
    /// Always `true` — the participant was removed.
    pub left: bool,
}

/// The room signaling router (nested under the plugin's API prefix in
/// [`MediaPlugin::build`](crate::MediaPlugin)).
pub fn room_router() -> Router<AppState> {
    Router::new()
        .route("/rooms", post(rooms_create))
        .route("/rooms/{room_id}/join", post(rooms_join))
        .route("/rooms/{room_id}/leave", post(rooms_leave))
        .route("/rooms/{room_id}", get(rooms_roster))
}

/// The [`RouteInfo`] set the room router serves under `api_prefix`, for
/// `autumn routes` listing / conformance.
#[must_use]
pub fn room_route_infos(api_prefix: &str) -> Vec<RouteInfo> {
    let prefix = api_prefix.trim_end_matches('/');
    vec![
        room_route("POST", format!("{prefix}/rooms"), "rooms::rooms_create"),
        room_route(
            "POST",
            format!("{prefix}/rooms/{{room_id}}/join"),
            "rooms::rooms_join",
        ),
        room_route(
            "POST",
            format!("{prefix}/rooms/{{room_id}}/leave"),
            "rooms::rooms_leave",
        ),
        room_route(
            "GET",
            format!("{prefix}/rooms/{{room_id}}"),
            "rooms::rooms_roster",
        ),
    ]
}

/// Build one plugin [`RouteInfo`] (source is overwritten by
/// `declare_plugin_routes` with the plugin attribution).
fn room_route(method: &str, path: String, handler: &str) -> RouteInfo {
    RouteInfo {
        method: method.to_owned(),
        path,
        handler: handler.to_owned(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InMemoryRoomStore, JoinRequest, LeaveRequest, RoomError, RoomService, RoomStore,
        SessionToken, room_participant_path, room_route_infos, rooms_create, rooms_join,
        rooms_roster, validate_room_segment,
    };
    use crate::config::MediaMtxConfig;
    use crate::transport::MediaUrls;
    use autumn_web::AppState;
    use autumn_web::reexports::axum::extract::{Path, State};
    use chrono::{Duration, Utc};
    use std::sync::Arc;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    fn urls() -> MediaUrls {
        MediaUrls::from_config(&MediaMtxConfig::default())
    }

    fn service(namespace: &str) -> RoomService {
        RoomService::new(
            Arc::new(InMemoryRoomStore::new(6)),
            urls(),
            namespace,
            Duration::seconds(300),
            6,
        )
    }

    // ── Segment guard ────────────────────────────────────────────────────────

    #[test]
    fn segment_guard_accepts_uuid_and_normal_labels_rejects_traversal() {
        assert!(validate_room_segment("tenant-a").is_ok());
        assert!(validate_room_segment("a1b2_c3-d4").is_ok());
        assert!(validate_room_segment(&uuid::Uuid::new_v4().to_string()).is_ok());
        for bad in ["", ".", "..", "a/b", "a b", "a.b", "a%2e", "café"] {
            assert!(
                matches!(
                    validate_room_segment(bad),
                    Err(RoomError::InvalidSegment { .. })
                ),
                "segment {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn participant_path_scheme_with_and_without_namespace() {
        assert_eq!(
            room_participant_path("", "room1", "part1").unwrap(),
            "room/room1/part1"
        );
        assert_eq!(
            room_participant_path("tenant-a", "room1", "part1").unwrap(),
            "room/tenant-a/room1/part1"
        );
        // A traversal namespace never composes a path.
        assert!(matches!(
            room_participant_path("..", "room1", "part1"),
            Err(RoomError::InvalidSegment { .. })
        ));
    }

    #[test]
    fn room_error_maps_to_expected_http_status() {
        use autumn_web::reexports::http::StatusCode;
        assert_eq!(
            RoomError::RoomNotFound.into_autumn().status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            RoomError::ParticipantNotFound.into_autumn().status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            RoomError::RoomFull { max: 6 }.into_autumn().status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            RoomError::Unauthorized.into_autumn().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            RoomError::InvalidSegment {
                value: "..".to_owned()
            }
            .into_autumn()
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            RoomError::InvalidMaxParticipants {
                requested: 9,
                cap: 6
            }
            .into_autumn()
            .status(),
            StatusCode::BAD_REQUEST
        );
    }

    // ── Session token ────────────────────────────────────────────────────────

    #[test]
    fn session_token_debug_is_redacted() {
        let token = SessionToken::generate();
        let raw = token.expose().to_owned();
        assert!(!raw.is_empty());
        let rendered = format!("{token:?}");
        assert!(
            !rendered.contains(&raw),
            "raw token must not appear in Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "Debug: {rendered}");
    }

    #[test]
    fn leave_request_debug_redacts_session_token() {
        let req = LeaveRequest {
            participant_id: "part-123".to_owned(),
            session_token: "super-secret-token-value".to_owned(),
        };
        let rendered = format!("{req:?}");
        assert!(
            !rendered.contains("super-secret-token-value"),
            "raw token must not appear in Debug: {rendered}"
        );
        assert!(
            rendered.contains("part-123"),
            "participant id must appear: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "Debug: {rendered}");
    }

    #[test]
    fn session_token_serializes_the_real_value() {
        let token = SessionToken::generate();
        let json = serde_json::to_string(&token).unwrap();
        assert_eq!(json, format!("\"{}\"", token.expose()));
    }

    // ── Create / cap ─────────────────────────────────────────────────────────

    #[test]
    fn create_room_mints_uuid_id_and_rejects_out_of_range_cap() {
        let store = InMemoryRoomStore::new(6);
        let snapshot = store.create_room("", 4).unwrap();
        assert_eq!(uuid::Uuid::parse_str(&snapshot.id).map(|_| ()), Ok(()));
        assert_eq!(snapshot.max_participants, 4);
        assert!(snapshot.participants.is_empty());

        // A 0-seat room is nonsense → rejected (never clamped up to 1).
        assert!(matches!(
            store.create_room("", 0),
            Err(RoomError::InvalidMaxParticipants {
                requested: 0,
                cap: 6
            })
        ));

        // Above the absolute ceiling → rejected.
        assert!(matches!(
            store.create_room("", 7),
            Err(RoomError::InvalidMaxParticipants {
                requested: 7,
                cap: 6
            })
        ));
    }

    #[test]
    fn create_room_backstops_the_absolute_ceiling_even_with_a_larger_hard_cap() {
        // Defense-in-depth: even if a larger `hard_cap` slipped past the
        // builder/config fail-fast, the store enforces the fixed 6 ceiling — a
        // 50-seat request is rejected, reporting the effective ceiling (6).
        let store = InMemoryRoomStore::new(50);
        assert!(matches!(
            store.create_room("", 50),
            Err(RoomError::InvalidMaxParticipants {
                requested: 50,
                cap: 6
            })
        ));
        // A within-ceiling value still works, capped at the absolute 6.
        assert_eq!(store.create_room("", 6).unwrap().max_participants, 6);
    }

    // ── Join / mesh URLs / cap enforcement ───────────────────────────────────

    #[test]
    fn join_mints_token_and_composes_publish_and_subscribe_urls() {
        let service = service("tenant-a");
        let room = service.create().unwrap();

        let first = service.join(&room.id, Some("Ada".to_owned())).unwrap();
        assert!(first.subscribe.is_empty(), "first joiner sees no peers");
        // Publish path + exact WHIP URL for the joiner's own path.
        let publish_path = format!("room/tenant-a/{}/{}", room.id, first.participant_id);
        assert_eq!(first.publish.path, publish_path);
        assert_eq!(
            first.publish.whip_url,
            format!("http://127.0.0.1:8889/{publish_path}/whip")
        );
        assert!(!first.session_token.expose().is_empty());

        // A second joiner now subscribes to the first (mesh).
        let second = service.join(&room.id, Some("Grace".to_owned())).unwrap();
        assert_eq!(second.subscribe.len(), 1);
        let peer = &second.subscribe[0];
        assert_eq!(peer.participant_id, first.participant_id);
        let peer_path = format!("room/tenant-a/{}/{}", room.id, first.participant_id);
        assert_eq!(peer.path, peer_path);
        assert_eq!(
            peer.whep_url,
            format!("http://127.0.0.1:8889/{peer_path}/whep")
        );
        assert_eq!(second.room.participants.len(), 2);
    }

    #[test]
    fn join_enforces_participant_cap() {
        let service = RoomService::new(
            Arc::new(InMemoryRoomStore::new(6)),
            urls(),
            "",
            Duration::seconds(300),
            6,
        );
        let room = service.create().unwrap();
        for _ in 0..6 {
            service.join(&room.id, None).unwrap();
        }
        // The 7th join exceeds the cap.
        assert!(matches!(
            service.join(&room.id, None),
            Err(RoomError::RoomFull { max: 6 })
        ));
    }

    // ── Fail-closed namespace isolation ──────────────────────────────────────

    #[test]
    fn rooms_never_leak_across_namespaces() {
        let store = Arc::new(InMemoryRoomStore::new(6));
        let ns_a = RoomService::new(store.clone(), urls(), "a", Duration::seconds(300), 6);
        let ns_b = RoomService::new(store, urls(), "b", Duration::seconds(300), 6);

        let room = ns_a.create().unwrap();
        // The same store, but namespace "b" cannot see namespace "a"'s room.
        assert!(matches!(
            ns_b.roster(&room.id),
            Err(RoomError::RoomNotFound)
        ));
        assert!(matches!(
            ns_b.join(&room.id, None),
            Err(RoomError::RoomNotFound)
        ));
        // The owning namespace still resolves it.
        assert!(ns_a.roster(&room.id).is_ok());
    }

    // ── Token verification (correct / wrong / expired) ───────────────────────

    #[test]
    fn participant_verify_matches_only_unexpired_correct_token() {
        use super::RoomParticipant;
        let now = Utc::now();
        let participant = RoomParticipant {
            id: "p1".to_owned(),
            display_name: None,
            joined_at: now,
            token: SessionToken("secret-token-value".to_owned()),
            token_expires_at: now + Duration::seconds(300),
        };
        assert!(participant.verify("secret-token-value", now));
        assert!(!participant.verify("wrong-token", now), "wrong token fails");
        // Past the expiry, even the correct token fails.
        assert!(
            !participant.verify("secret-token-value", now + Duration::seconds(301)),
            "expired token fails"
        );
    }

    #[test]
    fn leave_rejects_wrong_and_expired_tokens_then_accepts_correct() {
        // Expired: join with a negative TTL so the token is already expired.
        let expired = RoomService::new(
            Arc::new(InMemoryRoomStore::new(6)),
            urls(),
            "",
            Duration::seconds(-10),
            6,
        );
        let room = expired.create().unwrap();
        let joined = expired.join(&room.id, None).unwrap();
        assert!(matches!(
            expired.leave(
                &room.id,
                &joined.participant_id,
                joined.session_token.expose()
            ),
            Err(RoomError::Unauthorized)
        ));

        // Valid TTL: wrong token rejected, correct token accepted.
        let service = service("");
        let room = service.create().unwrap();
        let joined = service.join(&room.id, None).unwrap();
        assert!(matches!(
            service.leave(&room.id, &joined.participant_id, "not-the-token"),
            Err(RoomError::Unauthorized)
        ));
        assert!(
            service
                .leave(
                    &room.id,
                    &joined.participant_id,
                    joined.session_token.expose()
                )
                .is_ok()
        );
        // Emptying the room drops it.
        assert!(matches!(
            service.roster(&room.id),
            Err(RoomError::RoomNotFound)
        ));
    }

    // ── Roster serialization excludes tokens ─────────────────────────────────

    #[test]
    fn roster_snapshot_json_never_carries_tokens() {
        let service = service("tenant-a");
        let room = service.create().unwrap();
        let joined = service.join(&room.id, Some("Ada".to_owned())).unwrap();
        let snapshot = service.roster(&room.id).unwrap();
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(
            !json.contains(joined.session_token.expose()),
            "roster json leaked a token: {json}"
        );
        assert!(
            !json.contains("token"),
            "roster json has a token field: {json}"
        );
        assert!(json.contains("\"namespace\":\"tenant-a\""));
        assert!(json.contains("Ada"));
    }

    #[test]
    fn empty_namespace_is_omitted_from_snapshot_json() {
        let service = service("");
        let room = service.create().unwrap();
        let json = serde_json::to_string(&service.roster(&room.id).unwrap()).unwrap();
        assert!(
            !json.contains("namespace"),
            "empty namespace must be omitted: {json}"
        );
    }

    // ── Route metadata ───────────────────────────────────────────────────────

    #[test]
    fn room_route_infos_cover_the_four_routes_under_prefix() {
        let infos = room_route_infos("/api/media");
        let pairs: Vec<(&str, &str)> = infos
            .iter()
            .map(|info| (info.method.as_str(), info.path.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("POST", "/api/media/rooms"),
                ("POST", "/api/media/rooms/{room_id}/join"),
                ("POST", "/api/media/rooms/{room_id}/leave"),
                ("GET", "/api/media/rooms/{room_id}"),
            ]
        );
        // A trailing slash on the prefix does not double up.
        assert_eq!(room_route_infos("/api/media/")[0].path, "/api/media/rooms");
    }

    // ── Handler round-trip (create → join → roster through RoomService ext) ──

    #[tokio::test]
    async fn handlers_round_trip_create_join_roster() {
        let state = AppState::for_test();
        state.insert_extension(service("tenant-a"));

        // Create.
        let created = rooms_create(State(state.clone())).await.expect("create").0;
        assert!(created.participants.is_empty());
        let room_id = created.id.clone();

        // Join.
        let joined = rooms_join(
            State(state.clone()),
            Path(room_id.clone()),
            axum_json(JoinRequest {
                display_name: Some("Ada".to_owned()),
            }),
        )
        .await
        .expect("join")
        .0;
        assert!(!joined.session_token.expose().is_empty());
        assert!(!joined.participant_id.is_empty());

        // Roster reflects the join.
        let roster = rooms_roster(State(state.clone()), Path(room_id))
            .await
            .expect("roster")
            .0;
        assert_eq!(roster.participants.len(), 1);
        assert_eq!(roster.participants[0].id, joined.participant_id);
    }

    #[tokio::test]
    async fn handler_missing_service_extension_is_500() {
        let state = AppState::for_test();
        let err = rooms_create(State(state)).await.expect_err("no ext → 500");
        assert_eq!(
            err.status(),
            autumn_web::reexports::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// Wrap a value in an axum `Json` extractor for direct handler calls.
    fn axum_json<T>(value: T) -> autumn_web::reexports::axum::Json<T> {
        autumn_web::reexports::axum::Json(value)
    }
}
