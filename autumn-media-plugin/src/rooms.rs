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
//! # Security & host responsibilities
//!
//! The room signaling router ([`room_router`]) ships **no built-in
//! authentication or rate limiting** on create/join in this slice — `POST
//! {prefix}/rooms` and `POST {prefix}/rooms/{room_id}/join` are open. It
//! therefore **MUST be mounted behind the host application's auth / rate-limit
//! middleware**; the plugin does not gate who may create or join a room. As a
//! defense-in-depth backstop against unbounded memory growth from a create loop
//! (a created-but-never-joined room is only reaped when its last participant
//! leaves), [`InMemoryRoomStore`] caps the registry at [`MAX_ROOMS`] rooms and
//! rejects further creation with [`RoomError::RegistryFull`] (mapped to a
//! transient `503`). The background reaper ([`spawn_room_reaper_loop`]) also
//! reclaims idle/created-never-joined rooms (see *Known limitations*), but the
//! cap remains the hard backstop — neither is a substitute for the host
//! auth/rate-limit layer.
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
//! # Peer discovery
//!
//! Peer discovery is by client polling of the member-gated roster
//! (`GET {prefix}/rooms/{room_id}`), not server push — there is no `WebSocket`
//! or `SSE` signaling channel in this slice. The join response seeds a client
//! with the peers present at join time; to pick up later joiners a client
//! re-polls the roster, which returns a subscribe target (`WHEP` URL) for every
//! current member. The roster is **member-gated and fail-closed**: only a caller
//! presenting a valid participant token for that room may read it (see
//! [`RoomStore::roster`]), because it hands out per-peer media endpoints.
//!
//! # Known limitations
//!
//! - **Best-effort participant reaping (no true liveness)**: an idle-room /
//!   stale-participant reaper ([`spawn_room_reaper_loop`] →
//!   [`RoomStore::reap_stale`]) reclaims seats and rooms that have gone quiet, so
//!   a client that crashes or never sends leave no longer holds its seat until
//!   process restart. It keys on `last_seen_at`, which is seeded at join and
//!   refreshed **only** when the participant polls the member-gated roster — the
//!   sole per-participant liveness signal this slice has (there is no heartbeat,
//!   and `MediaMTX` does not verify the advisory token). This is an
//!   *approximation*: a participant whose media is live but which has **stopped
//!   polling the roster** for the whole idle TTL would be reaped from the store
//!   even though it is connected. Reaping only removes the in-memory signaling
//!   record (seat + advisory token); it does **not** tear down the participant's
//!   live `MediaMTX` `WebRTC` path, so a wrongly-reaped participant keeps its
//!   media flowing and can re-join — it simply stops appearing in peers' future
//!   roster polls. A real client heartbeat (renewal-on-activity) or `MediaMTX`
//!   token verification is the documented follow-up that would make this a true
//!   liveness guarantee; the reaper is deliberately conservative (a generous
//!   idle TTL, empty-room drops keyed on `created_at`) until then.
//! - **Advisory token expiry**: `token_expires_at` is returned to the joiner but
//!   is **not** enforced anywhere yet (`MediaMTX` does not verify these tokens),
//!   so it never gates a lifecycle operation — a participant can always leave
//!   with a value-correct token regardless of its age.
//!
//! # Single-process limitation
//!
//! [`InMemoryRoomStore`] keeps all room state in process memory, so it is
//! **single-process only** — a multi-process / multi-replica deployment needs a
//! shared backing store. [`RoomStore`] is the swap seam for that: a networked
//! (and necessarily async) store would revisit these synchronous signatures.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
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
use autumn_web::reexports::http::HeaderMap;
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
/// Verification for lifecycle/auth operations is **value-only**, using a
/// constant-time compare against the stored participant record — token expiry
/// is advisory and never enforced.
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

    /// The presented session token did not match (value-only compare; expiry is
    /// advisory and never checked). The message is deliberately token-free so it
    /// can never echo a secret.
    #[error("invalid session token")]
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

    /// The in-memory room registry is at its capacity backstop of [`MAX_ROOMS`]
    /// rooms, so no new room can be created right now. A transient overload
    /// condition, not a client error — mapped to a `503`. The message names no
    /// internals beyond the cap.
    #[error("room registry is at capacity ({max} rooms); try again later")]
    RegistryFull {
        /// The registry capacity that was reached.
        max: usize,
    },

    /// The backing room store could not complete the operation — a database
    /// error on the shared [`DbRoomStore`](crate::rooms_db::DbRoomStore), a lost
    /// pool connection, and so on. A transient backend failure, mapped to a
    /// `503`; the message is deliberately generic so it never echoes connection
    /// or query internals into a response.
    #[error("room store is temporarily unavailable")]
    Store,
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
            // A full registry is a transient overload condition (503), not a
            // client error — the caller can retry once rooms drain.
            Self::RegistryFull { .. } | Self::Store => {
                AutumnError::service_unavailable_msg(self.to_string())
            }
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
    /// Advisory metadata only, for a future media-auth / renewal slice.
    ///
    /// Returned to the joiner and surfaced in the join response, but **not**
    /// enforced anywhere — `MediaMTX` does not verify these tokens yet — so it
    /// must never gate a lifecycle operation (leave, roster auth). Verification
    /// is value-only (see [`verify_token`](Self::verify_token)).
    token_expires_at: DateTime<Utc>,
    /// Liveness clock for the idle-participant reaper (see [`reap_stale`]).
    ///
    /// Seeded to `joined_at` and **refreshed to `now` every time this
    /// participant is observed polling the member-gated roster** (see
    /// [`RoomStore::roster`]) — the roster poll is the only per-participant
    /// liveness signal this slice has (there is no heartbeat, and
    /// `token_expires_at` is advisory). An actively-polling participant stays
    /// fresh; one that has stopped polling ages out and is reclaimed. This is a
    /// best-effort liveness *approximation*, not a guarantee — see the
    /// module-level *Known limitations* note.
    last_seen_at: DateTime<Utc>,
}

impl RoomParticipant {
    /// Constant-time-verify a presented token against this participant by
    /// **value only**.
    ///
    /// The token's expiry is deliberately ignored, so a lifecycle/auth operation
    /// (leave, member-gated roster read) always succeeds for the value-correct
    /// token regardless of its age.
    fn verify_token(&self, candidate: &str) -> bool {
        autumn_web::auth::constant_time_eq(candidate.as_bytes(), self.token.expose().as_bytes())
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

/// The outcome tallies of one [`reap_stale`](RoomStore::reap_stale) sweep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReapStats {
    /// Rooms dropped this sweep (emptied by reaping, or created-never-joined
    /// rooms that lingered past the idle TTL).
    pub rooms_reaped: usize,
    /// Participants evicted this sweep (idle past the horizon).
    pub participants_reaped: usize,
}

/// The boxed future a fallible [`RoomStore`] operation returns.
///
/// The trait is written with hand-rolled boxed futures (mirroring
/// [`crate::sink::MediaSinkFuture`] and autumn-web's own webhook seam) rather
/// than `#[async_trait]`, so the seam stays object-safe and dependency-free
/// while a networked/durable backing store (e.g. [`DbRoomStore`](crate::rooms_db::DbRoomStore))
/// can `.await` real I/O. The lifetime ties the future to `&self` and the
/// borrowed arguments, so a synchronous in-memory impl still borrows without
/// cloning.
pub type RoomStoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, RoomError>> + Send + 'a>>;

/// The boxed future [`RoomStore::reap_stale`] returns (an infallible tally).
pub type ReapFuture<'a> = Pin<Box<dyn Future<Output = ReapStats> + Send + 'a>>;

/// The pluggable room-state store — the swap seam for a shared/durable backend.
///
/// Every method keys on the `(namespace, room_id)` pair and **fails closed**: a
/// namespace mismatch resolves to [`RoomError::RoomNotFound`], never another
/// namespace's room. The methods are **async** (returning [`RoomStoreFuture`]),
/// so a networked or durable backing store can `.await` its I/O; the shipped
/// [`InMemoryRoomStore`] resolves each call synchronously under one `RwLock` and
/// returns an already-ready future.
//
// The explicit `<'a>` lifetimes are load-bearing: the returned boxed future
// borrows both `&self` and the `&str` arguments for its whole lifetime (a
// networked store `.await`s while holding them), so they cannot be elided down
// to `&self` alone — clippy's `needless_lifetimes` is a false positive for this
// boxed-future seam.
#[allow(clippy::needless_lifetimes)]
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
    fn create_room<'a>(
        &'a self,
        namespace: &'a str,
        max_participants: usize,
    ) -> RoomStoreFuture<'a, RoomSnapshot>;

    /// Join the room `(namespace, room_id)`, minting a participant + token that
    /// stays valid for `token_ttl`.
    ///
    /// # Errors
    ///
    /// [`RoomError::RoomNotFound`] (including a namespace mismatch) or
    /// [`RoomError::RoomFull`] when the room is at capacity.
    fn join_room<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        display_name: Option<String>,
        token_ttl: Duration,
    ) -> RoomStoreFuture<'a, JoinRecord>;

    /// Remove a participant from the room after verifying its token by value.
    ///
    /// Verification is value-only (token expiry is advisory and never checked),
    /// so a participant can always leave with a value-correct token.
    ///
    /// # Errors
    ///
    /// [`RoomError::RoomNotFound`], [`RoomError::ParticipantNotFound`], or
    /// [`RoomError::Unauthorized`] on a wrong token.
    fn leave_room<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        participant_id: &'a str,
        token: &'a str,
    ) -> RoomStoreFuture<'a, ()>;

    /// Return the current roster snapshot for `(namespace, room_id)`, gated on
    /// the caller presenting a valid member token.
    ///
    /// The roster hands out per-peer media endpoints, so the read is
    /// **member-gated and fail-closed**: `auth_token` is verified value-only
    /// against every current member and, unless one matches, this returns
    /// [`RoomError::RoomNotFound`] — the *same* error as a nonexistent room, so a
    /// non-member with a guessable `room_id` cannot distinguish "room exists but
    /// I'm not a member" from "no such room" (no existence/membership oracle).
    ///
    /// # Errors
    ///
    /// [`RoomError::RoomNotFound`] for an unknown room, a namespace mismatch, or
    /// an `auth_token` matching no current member.
    fn roster<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        auth_token: &'a str,
    ) -> RoomStoreFuture<'a, RoomSnapshot>;

    /// Reclaim stale participants and idle rooms, returning what was reaped.
    ///
    /// Walks every room and evicts each participant whose liveness clock
    /// (`last_seen_at` — seeded at join, refreshed on every member roster poll)
    /// is older than `idle_ttl`; a room emptied by that eviction — or an empty
    /// created-never-joined room older than `idle_ttl` — is dropped. `now` is
    /// **injected** (never the wall clock) so the sweep is deterministically
    /// testable. Reaping keys strictly on the `(namespace, room_id)` map and
    /// never moves state across namespaces (fail-closed tenant isolation).
    ///
    /// This is a legitimate store concern (the store owns the seat lifecycle),
    /// so it lives on the trait seam: a shared/durable [`RoomStore`] must
    /// implement its own equivalent reclamation. A durable backend should make
    /// this a single atomic conditional delete so concurrent reapers across
    /// processes converge (last-write-wins) — see
    /// [`DbRoomStore::reap_stale`](crate::rooms_db::DbRoomStore).
    fn reap_stale(&self, now: DateTime<Utc>, idle_ttl: Duration) -> ReapFuture<'_>;
}

/// Capacity backstop on the number of rooms an [`InMemoryRoomStore`] holds.
///
/// `rooms_create` is unauthenticated in this slice and a created-but-never-joined
/// room is only reaped when its last participant leaves, so an unbounded create
/// loop would grow process memory for the lifetime of the process. This cap is a
/// defense-in-depth backstop against that (mirroring the workspace's
/// `MAX_BUCKETS` capacity-cap idiom for in-memory registries), **not** a
/// substitute for the host app's auth / rate-limit middleware — see the
/// module-level *Security & host responsibilities* note.
pub const MAX_ROOMS: usize = 10_000;

/// A single-process, in-memory [`RoomStore`].
///
/// Keyed on `(namespace, room_id)` under one `RwLock`. When the last
/// participant leaves a room, the room is dropped so an idle room never lingers.
/// The registry is capped at `max_rooms` (default [`MAX_ROOMS`]) as a
/// memory-growth backstop.
pub struct InMemoryRoomStore {
    rooms: RwLock<HashMap<(String, String), Room>>,
    hard_cap: usize,
    max_rooms: usize,
}

impl InMemoryRoomStore {
    /// Create an empty store whose rooms may hold up to `hard_cap` participants.
    ///
    /// `hard_cap` is further bounded by the absolute mesh ceiling: a room can
    /// never seat more than
    /// [`DEFAULT_ROOM_MAX_PARTICIPANTS`](crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS)
    /// (6, no SFU) regardless of `hard_cap`, so with a larger `hard_cap`
    /// [`create_room`](RoomStore::create_room) still rejects any request above 6
    /// as a backstop. The registry-size cap defaults to [`MAX_ROOMS`]; override
    /// it with [`with_max_rooms`](Self::with_max_rooms).
    #[must_use]
    pub fn new(hard_cap: usize) -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            hard_cap,
            max_rooms: MAX_ROOMS,
        }
    }

    /// Override the registry-size backstop (the maximum number of rooms the
    /// store holds before [`create_room`](RoomStore::create_room) rejects with
    /// [`RoomError::RegistryFull`]). Chiefly for tests that need a tiny cap
    /// without allocating [`MAX_ROOMS`] rooms.
    #[must_use]
    pub const fn with_max_rooms(mut self, max_rooms: usize) -> Self {
        self.max_rooms = max_rooms;
        self
    }
}

impl Default for InMemoryRoomStore {
    fn default() -> Self {
        Self::new(DEFAULT_ROOM_MAX_PARTICIPANTS)
    }
}

#[allow(clippy::needless_lifetimes)] // boxed futures borrow the args; see the trait.
impl RoomStore for InMemoryRoomStore {
    fn create_room<'a>(
        &'a self,
        namespace: &'a str,
        max_participants: usize,
    ) -> RoomStoreFuture<'a, RoomSnapshot> {
        Box::pin(async move {
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
            let mut rooms = self.rooms.write().expect("room store lock poisoned");
            // Registry capacity backstop: reject once the store is at `max_rooms`
            // rooms (checked under the write lock, so no create can race past it).
            // The signaling router is unauthenticated in this slice, so this guards
            // against unbounded memory growth from a create loop — a transient 503,
            // not a client error.
            if rooms.len() >= self.max_rooms {
                return Err(RoomError::RegistryFull {
                    max: self.max_rooms,
                });
            }
            rooms.insert((namespace.to_owned(), id), room);
            drop(rooms);
            Ok(snapshot)
        })
    }

    fn join_room<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        display_name: Option<String>,
        token_ttl: Duration,
    ) -> RoomStoreFuture<'a, JoinRecord> {
        Box::pin(async move {
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
            let participant = RoomParticipant {
                id: participant_id.clone(),
                display_name,
                joined_at: now,
                token: token.clone(),
                token_expires_at: now + token_ttl,
                last_seen_at: now,
            };
            // The stored record is the single source of truth for the advisory
            // expiry surfaced in the join response.
            let token_expires_at = participant.token_expires_at;
            room.participants
                .insert(participant_id.clone(), participant);
            let snapshot = room.snapshot();
            drop(rooms);
            Ok(JoinRecord {
                participant_id,
                token,
                token_expires_at,
                room: snapshot,
            })
        })
    }

    fn leave_room<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        participant_id: &'a str,
        token: &'a str,
    ) -> RoomStoreFuture<'a, ()> {
        Box::pin(async move {
            let mut rooms = self.rooms.write().expect("room store lock poisoned");
            let key = (namespace.to_owned(), room_id.to_owned());
            let room = rooms.get_mut(&key).ok_or(RoomError::RoomNotFound)?;
            let participant = room
                .participants
                .get(participant_id)
                .ok_or(RoomError::ParticipantNotFound)?;
            if !participant.verify_token(token) {
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
        })
    }

    fn roster<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        auth_token: &'a str,
    ) -> RoomStoreFuture<'a, RoomSnapshot> {
        Box::pin(async move {
            // A write lock (not a shared read) because a successful member read
            // doubles as the liveness signal: it refreshes the matching
            // participant's `last_seen_at`, so an actively-polling participant is
            // never reclaimed by the idle reaper. Contention is negligible (≤ 6
            // seats per room), so serializing roster reads costs nothing here.
            let mut rooms = self.rooms.write().expect("room store lock poisoned");
            let room = rooms
                .get_mut(&(namespace.to_owned(), room_id.to_owned()))
                .ok_or(RoomError::RoomNotFound)?;
            // Member-gate, fail-closed: a caller whose token matches no current
            // member gets the same `RoomNotFound` as a nonexistent room, so a
            // non-member cannot probe room existence or their own membership. On a
            // match, touch that member's liveness clock (see `reap_stale`).
            let now = Utc::now();
            let mut is_member = false;
            for participant in room.participants.values_mut() {
                if participant.verify_token(auth_token) {
                    participant.last_seen_at = now;
                    is_member = true;
                    break;
                }
            }
            if !is_member {
                return Err(RoomError::RoomNotFound);
            }
            let snapshot = room.snapshot();
            drop(rooms);
            Ok(snapshot)
        })
    }

    fn reap_stale(&self, now: DateTime<Utc>, idle_ttl: Duration) -> ReapFuture<'_> {
        Box::pin(async move {
            let mut stats = ReapStats::default();
            let mut rooms = self.rooms.write().expect("room store lock poisoned");
            // `retain` keys strictly on the `(namespace, room_id)` map and mutates
            // each room in place, so state never moves between namespaces —
            // tenant isolation is preserved by construction.
            rooms.retain(|_key, room| {
                let before = room.participants.len();
                // Evict any participant idle past the horizon (last observed —
                // via join or a roster poll — more than `idle_ttl` ago).
                room.participants.retain(|_id, participant| {
                    now.signed_duration_since(participant.last_seen_at) < idle_ttl
                });
                let reaped = before - room.participants.len();
                stats.participants_reaped += reaped;
                if room.participants.is_empty() {
                    // Drop the room when it (a) just emptied via reaping, or (b) is
                    // a created-never-joined room that has lingered past the idle
                    // TTL. Gating (b) on `created_at` age avoids reaping a room in
                    // the brief create→first-join window. Dropping an empty room can
                    // never harm a live participant.
                    let drop_room =
                        reaped > 0 || now.signed_duration_since(room.created_at) >= idle_ttl;
                    if drop_room {
                        stats.rooms_reaped += 1;
                        return false;
                    }
                }
                true
            });
            stats
        })
    }
}

// ── Idle-room / stale-participant reaper loop ─────────────────────────────────

/// Default seconds between reaper sweeps.
const ROOM_REAPER_INTERVAL_SECONDS: u64 = 60;

/// Default participant idle TTL (and empty-room grace), in seconds.
///
/// 15 minutes — deliberately generous. The default room token TTL is only 300s
/// (5 min), and a live client re-polls the member-gated roster every few seconds
/// for peer discovery, refreshing its `last_seen_at`; 15 min is 3× the token TTL
/// and far longer than any reasonable poll cadence, so an actively-connected
/// participant is never reclaimed — only a client that has stopped polling for a
/// full 15 minutes (crashed / abandoned / never sent leave) ages out.
const ROOM_IDLE_TTL_SECONDS: u64 = 900;

/// Upper bound (10 years, in seconds) applied to the reaper idle-TTL before it
/// is turned into a [`chrono::Duration`]. `chrono::Duration::seconds` panics on
/// values that overflow its internal millisecond representation, so a hostile /
/// fat-fingered `AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS` is clamped here rather
/// than crashing the reaper spawn.
const MAX_REAPER_TTL_SECONDS: i64 = 315_360_000;

/// Read a positive `u64` from `key`, falling back to `default` on
/// unset / blank / unparseable / zero. Keeps the reaper tunable without
/// threading a field through `MediaConfig` (a documented follow-up).
fn env_positive_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
}

/// Clamp a raw idle-TTL seconds value into a chrono [`Duration`].
///
/// `chrono::Duration::seconds` panics on values that overflow its internal
/// higher-precision (millisecond) representation, so a hostile / fat-fingered
/// `AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS` (up to `u64::MAX`) is clamped to
/// [`MAX_REAPER_TTL_SECONDS`] rather than crashing the reaper spawn.
fn clamp_reaper_ttl(idle_ttl_secs: u64) -> Duration {
    Duration::seconds(
        i64::try_from(idle_ttl_secs)
            .unwrap_or(MAX_REAPER_TTL_SECONDS)
            .min(MAX_REAPER_TTL_SECONDS),
    )
}

/// Spawn the background idle-room / stale-participant reaper for `store`.
///
/// Ticks every `AUTUMN_MEDIA__ROOM_REAPER_INTERVAL_SECONDS` (default
/// [`ROOM_REAPER_INTERVAL_SECONDS`], 60s) and reclaims participants/rooms idle
/// past `AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS` (default [`ROOM_IDLE_TTL_SECONDS`],
/// 900s). Both env overrides fall back to their default on an unset / blank /
/// unparseable / zero value. Mirrors
/// [`spawn_retention_sweep_loop`](crate::retention::spawn_retention_sweep_loop):
/// `tokio::spawn` + `tokio::time::interval` with `MissedTickBehavior::Skip`, one
/// independent tick each period, never panics. The clock is read per tick and
/// handed to the deterministically-testable [`RoomStore::reap_stale`].
///
/// Promoting the interval / idle-TTL knobs to `MediaConfig` is a documented
/// follow-up; they live as env-overridable constants here to keep this hardening
/// slice out of the config-load surface.
pub fn spawn_room_reaper_loop(store: Arc<dyn RoomStore>) {
    let interval_secs = env_positive_u64(
        "AUTUMN_MEDIA__ROOM_REAPER_INTERVAL_SECONDS",
        ROOM_REAPER_INTERVAL_SECONDS,
    );
    let idle_ttl_secs =
        env_positive_u64("AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS", ROOM_IDLE_TTL_SECONDS);
    let idle_ttl = clamp_reaper_ttl(idle_ttl_secs);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let stats = store.reap_stale(Utc::now(), idle_ttl).await;
            if stats.rooms_reaped > 0 || stats.participants_reaped > 0 {
                tracing::info!(
                    rooms_reaped = stats.rooms_reaped,
                    participants_reaped = stats.participants_reaped,
                    "media rooms: reaped stale participants / idle rooms"
                );
            }
        }
    });
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

/// One entry in a member-gated roster: a participant's identity plus its
/// subscribe target, so a polling member can subscribe to every peer.
///
/// Carries the peer's `WHEP` subscribe URL — never a token — so a member that
/// polls the roster picks up later joiners the join response could not have
/// seen. Clients exclude their own `participant_id`.
#[derive(Clone, Debug, Serialize)]
pub struct RosterEntry {
    /// The participant's id.
    pub participant_id: String,
    /// The participant's optional display name.
    pub display_name: Option<String>,
    /// When the participant joined.
    pub joined_at: DateTime<Utc>,
    /// The participant's `MediaMTX` path.
    pub path: String,
    /// The `WHEP` read (subscribe) URL for that path.
    pub whep_url: String,
}

/// The member-gated roster view: a token-free room snapshot whose participants
/// each carry a per-peer subscribe target.
#[derive(Clone, Debug, Serialize)]
pub struct RoomRoster {
    /// The room id.
    pub id: String,
    /// The room's namespace (omitted from JSON when empty).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    /// The room's participant cap.
    pub max_participants: usize,
    /// When the room was created.
    pub created_at: DateTime<Utc>,
    /// The current roster, each entry carrying a subscribe target.
    pub participants: Vec<RosterEntry>,
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
    pub async fn create(&self) -> Result<RoomSnapshot, RoomError> {
        self.store
            .create_room(&self.namespace, self.max_participants)
            .await
    }

    /// Join `room_id`, composing the joiner's publish target and one subscribe
    /// target per existing participant (the mesh).
    ///
    /// # Errors
    ///
    /// Propagates any [`RoomError`] from the store, or
    /// [`RoomError::InvalidSegment`] if a path cannot be composed.
    pub async fn join(
        &self,
        room_id: &str,
        display_name: Option<String>,
    ) -> Result<JoinResponse, RoomError> {
        let record = self
            .store
            .join_room(&self.namespace, room_id, display_name, self.token_ttl)
            .await?;

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
    pub async fn leave(
        &self,
        room_id: &str,
        participant_id: &str,
        token: &str,
    ) -> Result<(), RoomError> {
        self.store
            .leave_room(&self.namespace, room_id, participant_id, token)
            .await
    }

    /// Return the member-gated roster for `room_id`, composing a subscribe
    /// target for every participant.
    ///
    /// `auth_token` must be a current member's session token: the store gates
    /// the read fail-closed, returning [`RoomError::RoomNotFound`] (the same
    /// error as a nonexistent room) to any non-member, so the roster's per-peer
    /// media endpoints are never handed to an outsider.
    ///
    /// # Errors
    ///
    /// Propagates any [`RoomError`] from the store, or
    /// [`RoomError::InvalidSegment`] if a path cannot be composed.
    pub async fn roster(&self, room_id: &str, auth_token: &str) -> Result<RoomRoster, RoomError> {
        let snapshot = self
            .store
            .roster(&self.namespace, room_id, auth_token)
            .await?;
        let mut participants = Vec::with_capacity(snapshot.participants.len());
        for participant in snapshot.participants {
            let path = room_participant_path(&self.namespace, room_id, &participant.id)?;
            participants.push(RosterEntry {
                participant_id: participant.id,
                display_name: participant.display_name,
                joined_at: participant.joined_at,
                whep_url: self.urls.whep_read_url(&path),
                path,
            });
        }
        Ok(RoomRoster {
            id: snapshot.id,
            namespace: snapshot.namespace,
            max_participants: snapshot.max_participants,
            created_at: snapshot.created_at,
            participants,
        })
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
        .await
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
        .await
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
        .await
        .map_err(RoomError::into_autumn)?;
    Ok(Json(RoomLeaveResponse { left: true }))
}

/// Extract a bearer token from the `Authorization` header (case-insensitive
/// `Bearer` scheme).
///
/// Returns `None` when the header is absent, non-`ASCII`, or not a `Bearer`
/// scheme — callers treat that as "no token", which the member-gated roster
/// then fails closed on.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers
        .get(autumn_web::reexports::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

/// `GET {prefix}/rooms/{room_id}` — the member-gated room roster.
///
/// Requires an `Authorization: Bearer <token>` header carrying a current
/// member's session token. A missing/malformed header or a non-member token
/// yields the same `404` [`RoomError::RoomNotFound`] as a nonexistent room
/// (fail-closed, no membership oracle), because the roster hands out per-peer
/// media endpoints.
async fn rooms_roster(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> AutumnResult<Json<RoomRoster>> {
    let auth_token = bearer_token(&headers).unwrap_or_default();
    let roster = room_service(&state)?
        .roster(&room_id, auth_token)
        .await
        .map_err(RoomError::into_autumn)?;
    Ok(Json(roster))
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
        InMemoryRoomStore, JoinRequest, LeaveRequest, MAX_REAPER_TTL_SECONDS, ReapStats, Room,
        RoomError, RoomParticipant, RoomService, RoomStore, SessionToken, bearer_token,
        clamp_reaper_ttl, room_participant_path, room_route_infos, rooms_create, rooms_join,
        rooms_roster, validate_room_segment,
    };
    use crate::config::MediaMtxConfig;
    use crate::transport::MediaUrls;
    use autumn_web::AppState;
    use autumn_web::reexports::axum::extract::{Path, State};
    use autumn_web::reexports::http::HeaderMap;
    use chrono::{DateTime, Duration, Utc};
    use std::collections::HashMap;
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

    // ── Reaper TTL clamp ─────────────────────────────────────────────────────

    #[test]
    fn reaper_ttl_clamps_overflowing_value_without_panicking() {
        // A sane operator value passes through unchanged.
        assert_eq!(clamp_reaper_ttl(900), Duration::seconds(900));
        // A hostile / fat-fingered value that would overflow
        // `chrono::Duration::seconds`'s internal millisecond representation is
        // clamped to the 10-year maximum instead of panicking.
        assert_eq!(
            clamp_reaper_ttl(u64::MAX),
            Duration::seconds(MAX_REAPER_TTL_SECONDS)
        );
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
        // A full registry is a transient overload → 503, not a client error.
        assert_eq!(
            RoomError::RegistryFull { max: 10_000 }
                .into_autumn()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
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

    #[tokio::test]
    async fn create_room_mints_uuid_id_and_rejects_out_of_range_cap() {
        let store = InMemoryRoomStore::new(6);
        let snapshot = store.create_room("", 4).await.unwrap();
        assert_eq!(uuid::Uuid::parse_str(&snapshot.id).map(|_| ()), Ok(()));
        assert_eq!(snapshot.max_participants, 4);
        assert!(snapshot.participants.is_empty());

        // A 0-seat room is nonsense → rejected (never clamped up to 1).
        assert!(matches!(
            store.create_room("", 0).await,
            Err(RoomError::InvalidMaxParticipants {
                requested: 0,
                cap: 6
            })
        ));

        // Above the absolute ceiling → rejected.
        assert!(matches!(
            store.create_room("", 7).await,
            Err(RoomError::InvalidMaxParticipants {
                requested: 7,
                cap: 6
            })
        ));
    }

    #[tokio::test]
    async fn create_room_backstops_the_absolute_ceiling_even_with_a_larger_hard_cap() {
        // Defense-in-depth: even if a larger `hard_cap` slipped past the
        // builder/config fail-fast, the store enforces the fixed 6 ceiling — a
        // 50-seat request is rejected, reporting the effective ceiling (6).
        let store = InMemoryRoomStore::new(50);
        assert!(matches!(
            store.create_room("", 50).await,
            Err(RoomError::InvalidMaxParticipants {
                requested: 50,
                cap: 6
            })
        ));
        // A within-ceiling value still works, capped at the absolute 6.
        assert_eq!(store.create_room("", 6).await.unwrap().max_participants, 6);
    }

    #[tokio::test]
    async fn create_room_rejects_once_registry_is_at_capacity() {
        // Inject a tiny registry cap so we can trip it without allocating
        // MAX_ROOMS rooms: two creates succeed, the third is RegistryFull.
        let store = InMemoryRoomStore::new(6).with_max_rooms(2);
        assert!(store.create_room("", 4).await.is_ok());
        assert!(store.create_room("", 4).await.is_ok());
        assert!(matches!(
            store.create_room("", 4).await,
            Err(RoomError::RegistryFull { max: 2 })
        ));

        // The default cap is the MAX_ROOMS backstop — an ordinary store never
        // trips it under a normal number of rooms.
        assert_eq!(super::MAX_ROOMS, 10_000);
    }

    // ── Join / mesh URLs / cap enforcement ───────────────────────────────────

    #[tokio::test]
    async fn join_mints_token_and_composes_publish_and_subscribe_urls() {
        let service = service("tenant-a");
        let room = service.create().await.unwrap();

        let first = service
            .join(&room.id, Some("Ada".to_owned()))
            .await
            .unwrap();
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
        let second = service
            .join(&room.id, Some("Grace".to_owned()))
            .await
            .unwrap();
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

    #[tokio::test]
    async fn join_enforces_participant_cap() {
        let service = RoomService::new(
            Arc::new(InMemoryRoomStore::new(6)),
            urls(),
            "",
            Duration::seconds(300),
            6,
        );
        let room = service.create().await.unwrap();
        for _ in 0..6 {
            service.join(&room.id, None).await.unwrap();
        }
        // The 7th join exceeds the cap.
        assert!(matches!(
            service.join(&room.id, None).await,
            Err(RoomError::RoomFull { max: 6 })
        ));
    }

    // ── Fail-closed namespace isolation ──────────────────────────────────────

    #[tokio::test]
    async fn rooms_never_leak_across_namespaces() {
        let store = Arc::new(InMemoryRoomStore::new(6));
        let ns_a = RoomService::new(store.clone(), urls(), "a", Duration::seconds(300), 6);
        let ns_b = RoomService::new(store, urls(), "b", Duration::seconds(300), 6);

        let room = ns_a.create().await.unwrap();
        let member = ns_a.join(&room.id, None).await.unwrap();
        // The same store, but namespace "b" cannot see namespace "a"'s room —
        // even presenting namespace "a"'s valid member token.
        assert!(matches!(
            ns_b.roster(&room.id, member.session_token.expose()).await,
            Err(RoomError::RoomNotFound)
        ));
        assert!(matches!(
            ns_b.join(&room.id, None).await,
            Err(RoomError::RoomNotFound)
        ));
        // The owning namespace still resolves it for its member.
        assert!(
            ns_a.roster(&room.id, member.session_token.expose())
                .await
                .is_ok()
        );
    }

    // ── Token verification (correct / wrong / expired) ───────────────────────

    #[test]
    fn participant_verify_token_matches_by_value_only_ignoring_expiry() {
        use super::RoomParticipant;
        let now = Utc::now();
        // An already-expired token: `verify_token` ignores expiry entirely, so
        // the value-correct token still verifies and a wrong value still fails.
        let participant = RoomParticipant {
            id: "p1".to_owned(),
            display_name: None,
            joined_at: now,
            token: SessionToken("secret-token-value".to_owned()),
            token_expires_at: now - Duration::seconds(1),
            last_seen_at: now,
        };
        assert!(
            participant.verify_token("secret-token-value"),
            "value-correct token verifies even though it is past its expiry"
        );
        assert!(
            !participant.verify_token("wrong-token"),
            "a wrong token value still fails"
        );
    }

    #[tokio::test]
    async fn leave_succeeds_with_expired_token_and_rejects_wrong_value() {
        // Join with a negative TTL so `token_expires_at` is already in the past;
        // the participant must still be able to leave with the correct value.
        let service = RoomService::new(
            Arc::new(InMemoryRoomStore::new(6)),
            urls(),
            "",
            Duration::seconds(-10),
            6,
        );
        let room = service.create().await.unwrap();
        let joined = service.join(&room.id, None).await.unwrap();
        assert!(
            joined.token_expires_at < Utc::now(),
            "token is already expired (advisory only)"
        );
        // A wrong token value is still rejected.
        assert!(matches!(
            service
                .leave(&room.id, &joined.participant_id, "not-the-token")
                .await,
            Err(RoomError::Unauthorized)
        ));
        // The value-correct (but expired) token still leaves successfully.
        assert!(
            service
                .leave(
                    &room.id,
                    &joined.participant_id,
                    joined.session_token.expose()
                )
                .await
                .is_ok(),
            "a participant can always leave with a value-correct token"
        );
    }

    // ── Roster serialization excludes tokens ─────────────────────────────────

    #[tokio::test]
    async fn roster_returns_per_peer_subscribe_targets_for_a_member() {
        let service = service("tenant-a");
        let room = service.create().await.unwrap();
        let first = service
            .join(&room.id, Some("Ada".to_owned()))
            .await
            .unwrap();
        let second = service
            .join(&room.id, Some("Grace".to_owned()))
            .await
            .unwrap();

        // A member polling the roster sees a subscribe target for every peer,
        // with the exact same WHEP URL construction the join response uses.
        let roster = service
            .roster(&room.id, first.session_token.expose())
            .await
            .unwrap();
        assert_eq!(roster.participants.len(), 2);
        let grace = roster
            .participants
            .iter()
            .find(|entry| entry.participant_id == second.participant_id)
            .expect("second joiner is in the first member's roster");
        let grace_path = format!("room/tenant-a/{}/{}", room.id, second.participant_id);
        assert_eq!(grace.path, grace_path);
        assert_eq!(
            grace.whep_url,
            format!("http://127.0.0.1:8889/{grace_path}/whep")
        );
        assert_eq!(grace.display_name.as_deref(), Some("Grace"));
    }

    #[tokio::test]
    async fn roster_is_member_gated_fail_closed_no_oracle() {
        let service = service("tenant-a");
        let room = service.create().await.unwrap();
        service
            .join(&room.id, Some("Ada".to_owned()))
            .await
            .unwrap();

        // No token and a wrong token both resolve to the SAME error a
        // nonexistent room returns — a non-member cannot tell them apart.
        assert!(matches!(
            service.roster(&room.id, "").await,
            Err(RoomError::RoomNotFound)
        ));
        assert!(matches!(
            service.roster(&room.id, "not-a-member-token").await,
            Err(RoomError::RoomNotFound)
        ));
        assert!(matches!(
            service
                .roster("00000000-0000-0000-0000-000000000000", "")
                .await,
            Err(RoomError::RoomNotFound)
        ));
    }

    #[tokio::test]
    async fn roster_poll_surfaces_a_late_joiner_to_an_earlier_member() {
        let service = service("tenant-a");
        let room = service.create().await.unwrap();

        // A joins first; at that point A's join response saw no peers.
        let a = service
            .join(&room.id, Some("Ada".to_owned()))
            .await
            .unwrap();
        assert!(
            service
                .roster(&room.id, a.session_token.expose())
                .await
                .unwrap()
                .participants
                .iter()
                .all(|entry| entry.participant_id == a.participant_id)
        );

        // B joins later; A's next roster poll now lists B with B's WHEP target.
        let b = service
            .join(&room.id, Some("Grace".to_owned()))
            .await
            .unwrap();
        let roster = service
            .roster(&room.id, a.session_token.expose())
            .await
            .unwrap();
        let b_entry = roster
            .participants
            .iter()
            .find(|entry| entry.participant_id == b.participant_id)
            .expect("A's later poll surfaces the late joiner B");
        let b_path = format!("room/tenant-a/{}/{}", room.id, b.participant_id);
        assert_eq!(
            b_entry.whep_url,
            format!("http://127.0.0.1:8889/{b_path}/whep")
        );
    }

    #[tokio::test]
    async fn roster_json_never_carries_tokens() {
        let service = service("tenant-a");
        let room = service.create().await.unwrap();
        let joined = service
            .join(&room.id, Some("Ada".to_owned()))
            .await
            .unwrap();
        let roster = service
            .roster(&room.id, joined.session_token.expose())
            .await
            .unwrap();
        let json = serde_json::to_string(&roster).unwrap();
        assert!(
            !json.contains(joined.session_token.expose()),
            "roster json leaked a token: {json}"
        );
        assert!(
            !json.contains("token"),
            "roster json has a token field: {json}"
        );
        assert!(json.contains("\"namespace\":\"tenant-a\""));
        assert!(json.contains("whep_url"));
        assert!(json.contains("Ada"));
    }

    #[tokio::test]
    async fn empty_namespace_is_omitted_from_roster_json() {
        let service = service("");
        let room = service.create().await.unwrap();
        let joined = service.join(&room.id, None).await.unwrap();
        let roster = service
            .roster(&room.id, joined.session_token.expose())
            .await
            .unwrap();
        let json = serde_json::to_string(&roster).unwrap();
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

    // ── Bearer token parsing ─────────────────────────────────────────────────

    #[test]
    fn bearer_token_parses_case_insensitive_scheme_and_rejects_malformed() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                autumn_web::reexports::http::header::AUTHORIZATION,
                value.parse().unwrap(),
            );
            headers
        };
        assert_eq!(bearer_token(&with("Bearer abc123")), Some("abc123"));
        // Scheme match is case-insensitive; the token is trimmed.
        assert_eq!(bearer_token(&with("bearer   abc123  ")), Some("abc123"));
        assert_eq!(bearer_token(&with("BEARER abc123")), Some("abc123"));
        // A different scheme or a bare value is not a bearer token.
        assert_eq!(bearer_token(&with("Basic abc123")), None);
        assert_eq!(bearer_token(&with("abc123")), None);
        // An absent header yields no token.
        assert_eq!(bearer_token(&HeaderMap::new()), None);
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

        // Roster reflects the join — read with the member's Bearer token.
        let mut headers = HeaderMap::new();
        headers.insert(
            autumn_web::reexports::http::header::AUTHORIZATION,
            format!("Bearer {}", joined.session_token.expose())
                .parse()
                .unwrap(),
        );
        let roster = rooms_roster(State(state.clone()), Path(room_id.clone()), headers)
            .await
            .expect("roster")
            .0;
        assert_eq!(roster.participants.len(), 1);
        assert_eq!(roster.participants[0].participant_id, joined.participant_id);
        assert!(!roster.participants[0].whep_url.is_empty());

        // A roster read with no Authorization header is fail-closed → 404.
        let unauth = rooms_roster(State(state), Path(room_id), HeaderMap::new())
            .await
            .expect_err("no bearer → not found");
        assert_eq!(
            unauth.status(),
            autumn_web::reexports::http::StatusCode::NOT_FOUND
        );
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

    // ── Idle-room / stale-participant reaper ─────────────────────────────────
    //
    // The reaper keys on each participant's `last_seen_at` and each room's
    // `created_at` against an INJECTED `now`, so these tests build store state
    // with explicit timestamps (mirroring how `retention.rs` controls file
    // mtimes) and never touch the wall clock in the assertion — fully
    // deterministic, no sleeps. Constructing the private `Room` /
    // `RoomParticipant` from the child test module follows the existing
    // `participant_verify_token_*` test's precedent.

    /// Insert a room with explicit `created_at` and `(participant_id, token,
    /// last_seen_at)` seats directly into the store registry.
    fn seed_room(
        store: &InMemoryRoomStore,
        namespace: &str,
        room_id: &str,
        created_at: DateTime<Utc>,
        seats: &[(&str, &str, DateTime<Utc>)],
    ) {
        let mut participants = HashMap::new();
        for (id, token, last_seen_at) in seats {
            participants.insert(
                (*id).to_owned(),
                RoomParticipant {
                    id: (*id).to_owned(),
                    display_name: None,
                    joined_at: *last_seen_at,
                    token: SessionToken((*token).to_owned()),
                    token_expires_at: *last_seen_at,
                    last_seen_at: *last_seen_at,
                },
            );
        }
        store.rooms.write().unwrap().insert(
            (namespace.to_owned(), room_id.to_owned()),
            Room {
                id: room_id.to_owned(),
                namespace: namespace.to_owned(),
                max_participants: 6,
                created_at,
                participants,
            },
        );
    }

    /// The current participant count for a room, or `None` if the room is gone.
    fn seat_count(store: &InMemoryRoomStore, namespace: &str, room_id: &str) -> Option<usize> {
        store
            .rooms
            .read()
            .unwrap()
            .get(&(namespace.to_owned(), room_id.to_owned()))
            .map(|room| room.participants.len())
    }

    #[tokio::test]
    async fn reap_evicts_stale_participant_but_keeps_a_fresh_one_and_the_room() {
        // (a) + (b): a participant past the horizon is evicted while a fresh
        // co-tenant is kept, and the room (still non-empty) survives.
        let store = InMemoryRoomStore::new(6);
        let now = Utc::now();
        let ttl = Duration::minutes(30);
        seed_room(
            &store,
            "",
            "room-1",
            now - Duration::hours(2),
            &[
                ("stale", "tok-stale", now - Duration::hours(1)),
                ("fresh", "tok-fresh", now - Duration::minutes(5)),
            ],
        );

        let stats = store.reap_stale(now, ttl).await;

        assert_eq!(stats.participants_reaped, 1);
        assert_eq!(stats.rooms_reaped, 0);
        assert_eq!(seat_count(&store, "", "room-1"), Some(1));
        // The kept seat is the fresh one.
        assert!(store.roster("", "room-1", "tok-fresh").await.is_ok());
        assert!(matches!(
            store.roster("", "room-1", "tok-stale").await,
            Err(RoomError::RoomNotFound)
        ));
    }

    #[tokio::test]
    async fn reap_drops_a_room_emptied_by_reaping() {
        // (c): a room whose only participant ages out is dropped.
        let store = InMemoryRoomStore::new(6);
        let now = Utc::now();
        seed_room(
            &store,
            "",
            "room-1",
            now - Duration::hours(2),
            &[("stale", "tok", now - Duration::hours(1))],
        );

        let stats = store.reap_stale(now, Duration::minutes(30)).await;

        assert_eq!(stats.participants_reaped, 1);
        assert_eq!(stats.rooms_reaped, 1);
        assert_eq!(
            seat_count(&store, "", "room-1"),
            None,
            "emptied room dropped"
        );
    }

    #[tokio::test]
    async fn reap_drops_created_never_joined_room_past_ttl_but_keeps_a_fresh_empty_room() {
        // (d): an empty (created-never-joined) room lingering past the idle TTL
        // is dropped; a freshly-created empty room within the TTL is kept (so a
        // room in the create→first-join window is never reaped out from under a
        // joiner).
        let store = InMemoryRoomStore::new(6);
        let now = Utc::now();
        let ttl = Duration::minutes(30);
        seed_room(&store, "", "old-empty", now - Duration::hours(1), &[]);
        seed_room(&store, "", "new-empty", now - Duration::minutes(5), &[]);

        let stats = store.reap_stale(now, ttl).await;

        assert_eq!(stats.rooms_reaped, 1);
        assert_eq!(stats.participants_reaped, 0);
        assert_eq!(
            seat_count(&store, "", "old-empty"),
            None,
            "lingering empty room dropped"
        );
        assert_eq!(
            seat_count(&store, "", "new-empty"),
            Some(0),
            "a just-created empty room within the TTL survives"
        );
    }

    #[tokio::test]
    async fn reap_never_crosses_namespaces() {
        // (e): reaping a stale room in namespace "a" leaves an identically-named
        // fresh room in namespace "b" untouched — tenant isolation holds.
        let store = InMemoryRoomStore::new(6);
        let now = Utc::now();
        let ttl = Duration::minutes(30);
        seed_room(
            &store,
            "a",
            "shared-id",
            now - Duration::hours(2),
            &[("stale", "tok-a", now - Duration::hours(1))],
        );
        seed_room(
            &store,
            "b",
            "shared-id",
            now - Duration::minutes(1),
            &[("fresh", "tok-b", now - Duration::minutes(1))],
        );

        let stats = store.reap_stale(now, ttl).await;

        assert_eq!(stats.rooms_reaped, 1);
        assert_eq!(stats.participants_reaped, 1);
        assert_eq!(
            seat_count(&store, "a", "shared-id"),
            None,
            "ns a room reaped"
        );
        assert_eq!(
            seat_count(&store, "b", "shared-id"),
            Some(1),
            "same room_id in ns b is untouched"
        );
    }

    #[tokio::test]
    async fn reap_on_a_clean_store_is_a_zero_count_no_op() {
        // (f): nothing to reap → default (all-zero) stats, no state change.
        let store = InMemoryRoomStore::new(6);
        let now = Utc::now();
        seed_room(
            &store,
            "",
            "room-1",
            now - Duration::minutes(1),
            &[("fresh", "tok", now - Duration::minutes(1))],
        );

        let stats = store.reap_stale(now, Duration::minutes(30)).await;

        assert_eq!(stats, ReapStats::default());
        assert_eq!(seat_count(&store, "", "room-1"), Some(1));

        // And a truly empty store is likewise a no-op.
        let empty = InMemoryRoomStore::new(6);
        assert_eq!(
            empty.reap_stale(Utc::now(), Duration::minutes(30)).await,
            ReapStats::default()
        );
    }

    #[tokio::test]
    async fn roster_poll_refreshes_last_seen_so_an_active_participant_survives_a_sweep() {
        // Liveness touch: a participant that would otherwise be reaped survives
        // because it polled the member-gated roster. Both seats start stale
        // (last_seen an hour ago); polling the roster with A's token refreshes
        // A's `last_seen_at` to *now*, so a reap at *now* keeps A and evicts B.
        let store = InMemoryRoomStore::new(6);
        let stale = Utc::now() - Duration::hours(1);
        seed_room(
            &store,
            "",
            "room-1",
            Utc::now() - Duration::hours(2),
            &[("active", "tok-a", stale), ("idle", "tok-b", stale)],
        );

        // A polls the roster — this is the liveness signal.
        assert!(store.roster("", "room-1", "tok-a").await.is_ok());

        // Reap as of now with a 30-minute TTL: A was just touched (fresh), B is
        // still an hour idle (stale).
        let stats = store.reap_stale(Utc::now(), Duration::minutes(30)).await;

        assert_eq!(
            stats.participants_reaped, 1,
            "only the un-polled seat is reaped"
        );
        assert_eq!(stats.rooms_reaped, 0);
        assert!(
            store.roster("", "room-1", "tok-a").await.is_ok(),
            "the actively-polling participant survived the sweep"
        );
        assert!(matches!(
            store.roster("", "room-1", "tok-b").await,
            Err(RoomError::RoomNotFound)
        ));
    }
}
