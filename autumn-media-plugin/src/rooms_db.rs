//! A shared, database-backed [`RoomStore`] so mesh-room state survives across
//! processes / instances (multi-process safety, epic #1974).
//!
//! [`InMemoryRoomStore`](crate::rooms::InMemoryRoomStore) keeps every room in
//! one process's memory, so rooms vanish on restart and two app processes never
//! see the same rooms. [`DbRoomStore`] instead persists rooms and participants
//! in two tables (`media_rooms`, `media_room_participants`), so **every process
//! sharing the database sees the same rooms** — the correct backend for any
//! horizontally-scaled or multi-process deployment. It is selected via
//! [`MediaConfig::room_store_backend`](crate::config::MediaConfig::room_store_backend)
//! = `db`; `memory` (the default) keeps the single-process store.
//!
//! # Backend portability (pg + sqlite)
//!
//! Every query is written against autumn-web's `RuntimeConnection` /
//! `RuntimeBackend` aliases (Postgres by default, `SQLite` under
//! `autumn-web/sqlite`) using only backend-portable diesel query-builder
//! fragments and `Timestamp`/`NaiveDateTime` columns — no Postgres-only SQL — so
//! **both lanes compile**. This crate never enables the `sqlite` runtime lane
//! itself (that would trip the feature-unification hazard in `autumn/src/db.rs`);
//! the active backend is chosen by the end application.
//!
//! # Reaper concurrency — last-write-wins (idempotent)
//!
//! [`reap_stale`](DbRoomStore::reap_stale) is a **last-write-wins** sweep, not a
//! lease: it deletes every participant whose `last_seen_at` is older than the
//! injected `now - idle_ttl` cutoff, then deletes every room that is now empty
//! and whose `created_at` is older than the same cutoff. Both are unconditional
//! deletes keyed only on the injected clock, so **concurrent reapers across
//! processes converge with no corruption** — a second reaper's delete simply
//! affects zero rows. There is no shared lease row to time out and no leader
//! election to get wrong. Deletes are keyed on the exact `(namespace, room_id)`
//! pair, so reaping **never crosses namespaces** (tenant isolation), exactly
//! like the in-memory sweep.

use chrono::{DateTime, Duration, Utc};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::Pool;
use uuid::Uuid;

use autumn_web::RuntimeConnection;

use crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS;
use crate::rooms::{
    JoinRecord, MAX_ROOMS, ParticipantView, ReapFuture, ReapStats, RoomError, RoomSnapshot,
    RoomStore, RoomStoreFuture, SessionToken, validate_room_segment,
};

// ── Schema ────────────────────────────────────────────────────────────────────
//
// Two tables with composite primary keys. Timestamps are `Timestamp`
// (`NaiveDateTime`), never Postgres-only `Timestamptz`, so the schema compiles
// and runs on both the Postgres and SQLite lanes. The app ships the matching
// migration (see `migrations/` in this crate); the testcontainer suite creates
// the same shape from raw SQL.

diesel::table! {
    media_rooms (namespace, room_id) {
        namespace -> diesel::sql_types::Text,
        room_id -> diesel::sql_types::Text,
        max_participants -> diesel::sql_types::Integer,
        created_at -> diesel::sql_types::Timestamp,
    }
}

diesel::table! {
    media_room_participants (namespace, room_id, participant_id) {
        namespace -> diesel::sql_types::Text,
        room_id -> diesel::sql_types::Text,
        participant_id -> diesel::sql_types::Text,
        display_name -> diesel::sql_types::Nullable<diesel::sql_types::Text>,
        token -> diesel::sql_types::Text,
        joined_at -> diesel::sql_types::Timestamp,
        token_expires_at -> diesel::sql_types::Timestamp,
        last_seen_at -> diesel::sql_types::Timestamp,
    }
}

diesel::allow_tables_to_appear_in_same_query!(media_rooms, media_room_participants);

#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = media_rooms)]
struct RoomRow {
    namespace: String,
    room_id: String,
    max_participants: i32,
    created_at: chrono::NaiveDateTime,
}

#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = media_room_participants)]
struct ParticipantRow {
    namespace: String,
    room_id: String,
    participant_id: String,
    display_name: Option<String>,
    token: String,
    joined_at: chrono::NaiveDateTime,
    token_expires_at: chrono::NaiveDateTime,
    last_seen_at: chrono::NaiveDateTime,
}

// ── Store ─────────────────────────────────────────────────────────────────────

/// A shared, database-backed [`RoomStore`].
///
/// Holds a cloned handle to the application's primary connection pool
/// (`Pool<RuntimeConnection>`) and checks out a connection per operation, so it
/// is cheap to clone and share (mirroring how a `RoomService` clones its
/// `Arc<dyn RoomStore>`). `hard_cap` and `max_rooms` mirror
/// [`InMemoryRoomStore`](crate::rooms::InMemoryRoomStore): the absolute mesh
/// ceiling ([`DEFAULT_ROOM_MAX_PARTICIPANTS`], 6) is enforced structurally, and
/// `max_rooms` is a registry backstop.
pub struct DbRoomStore {
    pool: Pool<RuntimeConnection>,
    hard_cap: usize,
    max_rooms: usize,
}

impl DbRoomStore {
    /// Build a store over `pool`, capping each room at `hard_cap` seats (further
    /// bounded by the absolute mesh ceiling) and the registry at [`MAX_ROOMS`].
    #[must_use]
    pub const fn new(pool: Pool<RuntimeConnection>, hard_cap: usize) -> Self {
        Self {
            pool,
            hard_cap,
            max_rooms: MAX_ROOMS,
        }
    }

    /// Override the registry-size backstop (chiefly for tests that need a tiny
    /// cap). Mirrors [`InMemoryRoomStore::with_max_rooms`](crate::rooms::InMemoryRoomStore::with_max_rooms).
    #[must_use]
    pub const fn with_max_rooms(mut self, max_rooms: usize) -> Self {
        self.max_rooms = max_rooms;
        self
    }
}

/// Map any pool/query error onto a token-free, `503`-mapped [`RoomError::Store`]
/// after logging the real cause (which is never surfaced to the client).
fn map_db_err<E: std::fmt::Display>(err: E) -> RoomError {
    tracing::warn!(error = %err, "media rooms: db operation failed");
    RoomError::Store
}

/// Build a token-free [`RoomSnapshot`] from a room row and its participant rows,
/// with the same deterministic roster ordering (`joined_at`, then `id`) as the
/// in-memory store.
fn snapshot_from(room: &RoomRow, rows: &[ParticipantRow]) -> RoomSnapshot {
    let mut participants: Vec<ParticipantView> = rows
        .iter()
        .map(|row| ParticipantView {
            id: row.participant_id.clone(),
            display_name: row.display_name.clone(),
            joined_at: row.joined_at.and_utc(),
        })
        .collect();
    participants.sort_by(|a, b| a.joined_at.cmp(&b.joined_at).then_with(|| a.id.cmp(&b.id)));
    RoomSnapshot {
        id: room.room_id.clone(),
        namespace: room.namespace.clone(),
        max_participants: usize::try_from(room.max_participants).unwrap_or(0),
        created_at: room.created_at.and_utc(),
        participants,
    }
}

#[allow(clippy::needless_lifetimes)] // boxed futures borrow the args across `.await`; see the trait.
impl RoomStore for DbRoomStore {
    fn create_room<'a>(
        &'a self,
        namespace: &'a str,
        max_participants: usize,
    ) -> RoomStoreFuture<'a, RoomSnapshot> {
        Box::pin(async move {
            if !namespace.is_empty() {
                validate_room_segment(namespace)?;
            }
            // Structural mesh ceiling backstop, identical to the in-memory store.
            let ceiling = self.hard_cap.min(DEFAULT_ROOM_MAX_PARTICIPANTS);
            if max_participants == 0 || max_participants > ceiling {
                return Err(RoomError::InvalidMaxParticipants {
                    requested: max_participants,
                    cap: ceiling,
                });
            }
            let mut conn = self.pool.get().await.map_err(map_db_err)?;

            // Registry-capacity backstop (a transient 503 once at capacity).
            // Non-transactional: a rare race can admit one extra room above the
            // cap, an accepted backstop-only imprecision (the host app owns real
            // create-rate limiting — see the module-level security note on the
            // in-memory store).
            let count: i64 = media_rooms::table
                .count()
                .get_result(&mut conn)
                .await
                .map_err(map_db_err)?;
            if usize::try_from(count).unwrap_or(usize::MAX) >= self.max_rooms {
                return Err(RoomError::RegistryFull {
                    max: self.max_rooms,
                });
            }

            let id = Uuid::new_v4().to_string();
            let now = Utc::now();
            let row = RoomRow {
                namespace: namespace.to_owned(),
                room_id: id.clone(),
                max_participants: i32::try_from(max_participants).unwrap_or(i32::MAX),
                created_at: now.naive_utc(),
            };
            diesel::insert_into(media_rooms::table)
                .values(&row)
                .execute(&mut conn)
                .await
                .map_err(map_db_err)?;

            Ok(RoomSnapshot {
                id,
                namespace: namespace.to_owned(),
                max_participants,
                created_at: now,
                participants: Vec::new(),
            })
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
            let mut conn = self.pool.get().await.map_err(map_db_err)?;

            // The room must exist (fail-closed on a namespace mismatch — the
            // filter keys on both columns).
            let room: RoomRow = media_rooms::table
                .filter(
                    media_rooms::namespace
                        .eq(namespace)
                        .and(media_rooms::room_id.eq(room_id)),
                )
                .select(RoomRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(map_db_err)?
                .ok_or(RoomError::RoomNotFound)?;
            let max = usize::try_from(room.max_participants).unwrap_or(0);

            // Capacity check (non-transactional backstop, as above).
            let seats: i64 = media_room_participants::table
                .filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id)),
                )
                .count()
                .get_result(&mut conn)
                .await
                .map_err(map_db_err)?;
            if usize::try_from(seats).unwrap_or(usize::MAX) >= max {
                return Err(RoomError::RoomFull { max });
            }

            let now = Utc::now();
            let participant_id = Uuid::new_v4().to_string();
            let token = SessionToken::generate();
            let token_expires_at = now + token_ttl;
            let new = ParticipantRow {
                namespace: namespace.to_owned(),
                room_id: room_id.to_owned(),
                participant_id: participant_id.clone(),
                display_name,
                token: token.expose().to_owned(),
                joined_at: now.naive_utc(),
                token_expires_at: token_expires_at.naive_utc(),
                last_seen_at: now.naive_utc(),
            };
            diesel::insert_into(media_room_participants::table)
                .values(&new)
                .execute(&mut conn)
                .await
                .map_err(map_db_err)?;

            // Snapshot the room *after* the join (includes the new participant).
            let rows: Vec<ParticipantRow> = media_room_participants::table
                .filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id)),
                )
                .select(ParticipantRow::as_select())
                .load(&mut conn)
                .await
                .map_err(map_db_err)?;

            Ok(JoinRecord {
                participant_id,
                token,
                token_expires_at,
                room: snapshot_from(&room, &rows),
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
            let mut conn = self.pool.get().await.map_err(map_db_err)?;

            // Distinguish "no such room" from "no such participant" the same way
            // the in-memory store does.
            let room_exists: Option<String> = media_rooms::table
                .filter(
                    media_rooms::namespace
                        .eq(namespace)
                        .and(media_rooms::room_id.eq(room_id)),
                )
                .select(media_rooms::room_id)
                .first(&mut conn)
                .await
                .optional()
                .map_err(map_db_err)?;
            if room_exists.is_none() {
                return Err(RoomError::RoomNotFound);
            }

            let stored: String = media_room_participants::table
                .filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id))
                        .and(media_room_participants::participant_id.eq(participant_id)),
                )
                .select(media_room_participants::token)
                .first(&mut conn)
                .await
                .optional()
                .map_err(map_db_err)?
                .ok_or(RoomError::ParticipantNotFound)?;

            // Value-only, constant-time verify (expiry is advisory, never
            // checked — a value-correct token always leaves).
            if !autumn_web::auth::constant_time_eq(token.as_bytes(), stored.as_bytes()) {
                return Err(RoomError::Unauthorized);
            }

            diesel::delete(
                media_room_participants::table.filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id))
                        .and(media_room_participants::participant_id.eq(participant_id)),
                ),
            )
            .execute(&mut conn)
            .await
            .map_err(map_db_err)?;

            // Drop an emptied room so idle rooms never accumulate.
            let remaining: i64 = media_room_participants::table
                .filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id)),
                )
                .count()
                .get_result(&mut conn)
                .await
                .map_err(map_db_err)?;
            if remaining == 0 {
                diesel::delete(
                    media_rooms::table.filter(
                        media_rooms::namespace
                            .eq(namespace)
                            .and(media_rooms::room_id.eq(room_id)),
                    ),
                )
                .execute(&mut conn)
                .await
                .map_err(map_db_err)?;
            }
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
            let mut conn = self.pool.get().await.map_err(map_db_err)?;

            let room: RoomRow = media_rooms::table
                .filter(
                    media_rooms::namespace
                        .eq(namespace)
                        .and(media_rooms::room_id.eq(room_id)),
                )
                .select(RoomRow::as_select())
                .first(&mut conn)
                .await
                .optional()
                .map_err(map_db_err)?
                .ok_or(RoomError::RoomNotFound)?;

            let rows: Vec<ParticipantRow> = media_room_participants::table
                .filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id)),
                )
                .select(ParticipantRow::as_select())
                .load(&mut conn)
                .await
                .map_err(map_db_err)?;

            // Member-gate, fail-closed: a caller whose token matches no current
            // member gets the same `RoomNotFound` as a nonexistent room (no
            // membership oracle). On a match, refresh that member's liveness
            // clock so the reaper never reclaims an actively-polling participant.
            let now = Utc::now();
            let member_id = rows.iter().find_map(|row| {
                autumn_web::auth::constant_time_eq(auth_token.as_bytes(), row.token.as_bytes())
                    .then(|| row.participant_id.clone())
            });
            let Some(member_id) = member_id else {
                return Err(RoomError::RoomNotFound);
            };

            diesel::update(
                media_room_participants::table.filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id))
                        .and(media_room_participants::participant_id.eq(&member_id)),
                ),
            )
            .set(media_room_participants::last_seen_at.eq(now.naive_utc()))
            .execute(&mut conn)
            .await
            .map_err(map_db_err)?;

            Ok(snapshot_from(&room, &rows))
        })
    }

    fn reap_stale(&self, now: DateTime<Utc>, idle_ttl: Duration) -> ReapFuture<'_> {
        Box::pin(async move {
            // Best-effort, exactly like the reaper loop expects: a backend error
            // reaps nothing this tick (logged, zero stats) rather than failing.
            let mut stats = ReapStats::default();
            let cutoff = (now - idle_ttl).naive_utc();
            let mut conn = match self.pool.get().await {
                Ok(conn) => conn,
                Err(err) => {
                    tracing::warn!(error = %err, "media rooms: reaper db checkout failed");
                    return stats;
                }
            };

            // Phase 1 — evict every participant idle past the horizon. One
            // atomic conditional delete keyed on the injected clock: idempotent,
            // so concurrent reapers converge (last-write-wins).
            match diesel::delete(
                media_room_participants::table
                    .filter(media_room_participants::last_seen_at.lt(cutoff)),
            )
            .execute(&mut conn)
            .await
            {
                Ok(reaped) => stats.participants_reaped = reaped,
                Err(err) => {
                    tracing::warn!(error = %err, "media rooms: reaper participant sweep failed");
                    return stats;
                }
            }

            // Phase 2 — drop every now-empty room older than the cutoff. This
            // covers BOTH contract cases: a room emptied by phase 1 (its stale
            // participant implies `created_at <= last_seen < cutoff`) AND a
            // created-never-joined room lingering past the TTL. A fresh empty
            // room (`created_at >= cutoff`) is kept, so a room in the
            // create→first-join window is never reaped out from under a joiner.
            // Each delete is keyed on the exact `(namespace, room_id)` pair, so
            // reaping never crosses namespaces.
            let candidates: Vec<(String, String)> = match media_rooms::table
                .filter(media_rooms::created_at.lt(cutoff))
                .select((media_rooms::namespace, media_rooms::room_id))
                .load(&mut conn)
                .await
            {
                Ok(candidates) => candidates,
                Err(err) => {
                    tracing::warn!(error = %err, "media rooms: reaper room scan failed");
                    return stats;
                }
            };
            for (namespace, room_id) in candidates {
                let occupant_count: i64 = match media_room_participants::table
                    .filter(
                        media_room_participants::namespace
                            .eq(&namespace)
                            .and(media_room_participants::room_id.eq(&room_id)),
                    )
                    .count()
                    .get_result(&mut conn)
                    .await
                {
                    Ok(count) => count,
                    Err(err) => {
                        tracing::warn!(error = %err, "media rooms: reaper seat count failed");
                        continue;
                    }
                };
                if occupant_count == 0
                    && let Ok(reaped) = diesel::delete(
                        media_rooms::table.filter(
                            media_rooms::namespace
                                .eq(&namespace)
                                .and(media_rooms::room_id.eq(&room_id)),
                        ),
                    )
                    .execute(&mut conn)
                    .await
                {
                    stats.rooms_reaped += reaped;
                }
            }

            stats
        })
    }
}
