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
//! # Seat cap and room deletes
//!
//! Join, leave and the reaper's room delete each run in one transaction that
//! first locks the room row. Thus two processes that join the last seat run one
//! after the other, and the room never has more seats than its cap (#2864,
//! #3104). A room delete cannot remove a seat that a join returned (#2407). The
//! lock is a no-op `UPDATE`, so it works the same on Postgres (row lock) and
//! `SQLite` (database write lock). The schema does not change.
//!
//! Join and leave run on their own task. Thus a dropped caller future (a client
//! disconnect) cannot leave a transaction open with the room locked. Leave
//! checks the token before it takes the lock, so a bad request takes no lock.
//!
//! With a Postgres default isolation level above READ COMMITTED, a join that
//! waits for the lock fails with `503` (serialization error), not a passed cap.
//!
//! # Reaper concurrency — last-write-wins (idempotent)
//!
//! [`reap_stale`](DbRoomStore::reap_stale) is a **last-write-wins** sweep, not a
//! lease. Phase 1 deletes each participant whose `last_seen_at` is older than
//! the injected `now - idle_ttl` cutoff. Phase 2, in one transaction, claims
//! each empty room whose `created_at` is older than the same cutoff, deletes the
//! claimed rooms that are still empty, and restores each claimed room that a
//! join filled. A second reaper finds no rows to delete, so **concurrent
//! reapers across processes converge with no corruption**. There is no shared
//! lease row to time out and no leader election to get wrong. Two reapers with
//! different query plans can lock rows in a different order. Then Postgres can
//! stop one with a deadlock error. That reaper logs a warning and deletes no
//! rooms on that tick. Deletes are keyed on the exact `(namespace, room_id)`
//! pair, so reaping **never crosses namespaces** (tenant isolation), exactly
//! like the in-memory sweep.

use chrono::{DateTime, Duration, SubsecRound, Utc};
use diesel::prelude::*;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncConnection as _, RunQueryDsl};
use uuid::Uuid;

use autumn_web::RuntimeConnection;

use crate::config::DEFAULT_ROOM_MAX_PARTICIPANTS;
use crate::rooms::{
    JoinRecord, MAX_ROOMS, ParticipantView, ReapFuture, ReapStats, RoomError, RoomSnapshot,
    RoomStore, RoomStoreFuture, SessionToken, renewed_expiry, validate_room_segment,
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

/// The error of a transaction body. Both kinds roll the transaction back.
enum TxError {
    /// A room outcome for the caller (`RoomFull`, `RoomNotFound`, ...).
    Room(RoomError),
    /// A database failure, mapped by [`map_db_err`].
    Db(diesel::result::Error),
}

impl From<diesel::result::Error> for TxError {
    fn from(err: diesel::result::Error) -> Self {
        Self::Db(err)
    }
}

impl From<RoomError> for TxError {
    fn from(err: RoomError) -> Self {
        Self::Room(err)
    }
}

impl TxError {
    fn into_room(self) -> RoomError {
        match self {
            Self::Room(err) => err,
            Self::Db(err) => map_db_err(err),
        }
    }
}

/// Lock the row of one room until the transaction ends. Returns `false` if
/// the room does not exist.
///
/// Call it first in the transaction. It is a no-op `UPDATE`, so it is a write
/// on both backends: Postgres locks the row, and `SQLite` takes the database
/// write lock (and waits on `busy_timeout`). A later statement takes a new
/// snapshot, so it sees each seat that another writer committed while it held
/// the lock. Join and leave change the seats of a room only while they hold
/// this lock, and the reaper claims a room the same way before it deletes it.
async fn lock_room(
    conn: &mut RuntimeConnection,
    namespace: &str,
    room_id: &str,
) -> QueryResult<bool> {
    let locked = diesel::update(
        media_rooms::table.filter(
            media_rooms::namespace
                .eq(namespace)
                .and(media_rooms::room_id.eq(room_id)),
        ),
    )
    .set(media_rooms::max_participants.eq(media_rooms::max_participants))
    .execute(conn)
    .await?;
    Ok(locked > 0)
}

/// Run `work` on its own task, so its transaction ends with `COMMIT` or
/// `ROLLBACK` even if the caller drops the future (a client disconnect or a
/// timeout). Without this, the room row can stay locked until the pool drops
/// that connection.
///
/// `work` gets a [`Caller`], so it can roll back when nobody waits for the
/// result. A panic in `work` continues in the caller.
async fn detached<T, Fut>(work: impl FnOnce(Caller) -> Fut) -> Result<T, RoomError>
where
    T: Send + 'static,
    Fut: Future<Output = Result<T, RoomError>> + Send + 'static,
{
    let (_alive, watch) = tokio::sync::oneshot::channel::<()>();
    match tokio::spawn(work(Caller(watch))).await {
        Ok(result) => result,
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => Err(map_db_err(err)),
    }
}

/// The caller of a [`detached`] call.
struct Caller(tokio::sync::oneshot::Receiver<()>);

impl Caller {
    /// `true` when the caller dropped its future.
    fn is_gone(&mut self) -> bool {
        matches!(
            self.0.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        )
    }

    /// Check out a connection, unless the caller goes first. A dropped caller
    /// does not wait in the pool queue, so it adds no hidden backlog.
    async fn checkout(
        &mut self,
        pool: &Pool<RuntimeConnection>,
    ) -> Result<diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>, RoomError>
    {
        tokio::select! {
            conn = pool.get() => conn.map_err(map_db_err),
            _ = &mut self.0 => Err(RoomError::Store),
        }
    }
}

/// Check that `participant_id` holds a seat in the room, with `token`.
///
/// The errors are the same as the in-memory store: `RoomNotFound`, then
/// `ParticipantNotFound`, then `Unauthorized`. The compare is value-only and
/// constant-time. Expiry is advisory: a value-correct token always leaves.
async fn check_seat(
    conn: &mut RuntimeConnection,
    namespace: &str,
    room_id: &str,
    participant_id: &str,
    token: &str,
) -> Result<(), TxError> {
    let room_exists: Option<String> = media_rooms::table
        .filter(
            media_rooms::namespace
                .eq(namespace)
                .and(media_rooms::room_id.eq(room_id)),
        )
        .select(media_rooms::room_id)
        .first(conn)
        .await
        .optional()?;
    if room_exists.is_none() {
        return Err(RoomError::RoomNotFound.into());
    }
    let stored: String = media_room_participants::table
        .filter(
            media_room_participants::namespace
                .eq(namespace)
                .and(media_room_participants::room_id.eq(room_id))
                .and(media_room_participants::participant_id.eq(participant_id)),
        )
        .select(media_room_participants::token)
        .first(conn)
        .await
        .optional()?
        .ok_or(RoomError::ParticipantNotFound)?;
    if !autumn_web::auth::constant_time_eq(token.as_bytes(), stored.as_bytes()) {
        return Err(RoomError::Unauthorized.into());
    }
    Ok(())
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
            // cap, an accepted backstop-only imprecision (the per-IP route limit
            // and authentication are the real controls — see the module-level
            // security note on the in-memory store).
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
        let pool = self.pool.clone();
        let (namespace, room_id) = (namespace.to_owned(), room_id.to_owned());
        Box::pin(detached(move |mut caller| async move {
            let (namespace, room_id) = (namespace.as_str(), room_id.as_str());
            let mut conn = caller.checkout(&pool).await?;
            // One transaction that holds the room row: two joins for the last
            // seat run one after the other, so the count never goes stale.
            conn.transaction(async move |conn| {
                // Fail-closed on a namespace mismatch: the filter keys on both
                // columns.
                if !lock_room(conn, namespace, room_id).await? {
                    return Err(RoomError::RoomNotFound.into());
                }
                let room: RoomRow = media_rooms::table
                    .filter(
                        media_rooms::namespace
                            .eq(namespace)
                            .and(media_rooms::room_id.eq(room_id)),
                    )
                    .select(RoomRow::as_select())
                    .first(conn)
                    .await?;
                let max = usize::try_from(room.max_participants).unwrap_or(0);

                let seats: i64 = media_room_participants::table
                    .filter(
                        media_room_participants::namespace
                            .eq(namespace)
                            .and(media_room_participants::room_id.eq(room_id)),
                    )
                    .count()
                    .get_result(conn)
                    .await?;
                if usize::try_from(seats).unwrap_or(usize::MAX) >= max {
                    return Err(RoomError::RoomFull { max }.into());
                }
                // A caller that is gone gets no seat: roll back, so it does
                // not hold a place in the cap until the reaper removes it.
                if caller.is_gone() {
                    return Err(RoomError::Store.into());
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
                    .execute(conn)
                    .await?;

                // Snapshot the room after the join (includes the new seat).
                let rows: Vec<ParticipantRow> = media_room_participants::table
                    .filter(
                        media_room_participants::namespace
                            .eq(namespace)
                            .and(media_room_participants::room_id.eq(room_id)),
                    )
                    .select(ParticipantRow::as_select())
                    .load(conn)
                    .await?;

                Ok(JoinRecord {
                    participant_id,
                    token,
                    token_expires_at,
                    room: snapshot_from(&room, &rows),
                })
            })
            .await
            .map_err(TxError::into_room)
        }))
    }

    fn leave_room<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        participant_id: &'a str,
        token: &'a str,
    ) -> RoomStoreFuture<'a, ()> {
        let pool = self.pool.clone();
        let owned = [namespace, room_id, participant_id, token].map(str::to_owned);
        Box::pin(detached(move |mut caller| async move {
            let [namespace, room_id, participant_id, token] = owned.each_ref().map(String::as_str);
            // A caller that goes while it waits for the pool gets no connection.
            // After the checkout, the leave runs to the end: the user asked for it.
            let mut conn = caller.checkout(&pool).await?;
            // Check the token before the lock, so a bad request takes no lock.
            check_seat(&mut conn, namespace, room_id, participant_id, token)
                .await
                .map_err(TxError::into_room)?;
            // One transaction that holds the room row, so a join cannot add a
            // seat between the last leave's count and its room delete.
            conn.transaction(async move |conn| {
                if !lock_room(conn, namespace, room_id).await? {
                    return Err(RoomError::RoomNotFound.into());
                }
                // Check again under the lock: the seat can go between the two.
                check_seat(conn, namespace, room_id, participant_id, token).await?;

                diesel::delete(
                    media_room_participants::table.filter(
                        media_room_participants::namespace
                            .eq(namespace)
                            .and(media_room_participants::room_id.eq(room_id))
                            .and(media_room_participants::participant_id.eq(participant_id)),
                    ),
                )
                .execute(conn)
                .await?;

                // Drop an emptied room so idle rooms never accumulate.
                let remaining: i64 = media_room_participants::table
                    .filter(
                        media_room_participants::namespace
                            .eq(namespace)
                            .and(media_room_participants::room_id.eq(room_id)),
                    )
                    .count()
                    .get_result(conn)
                    .await?;
                if remaining == 0 {
                    diesel::delete(
                        media_rooms::table.filter(
                            media_rooms::namespace
                                .eq(namespace)
                                .and(media_rooms::room_id.eq(room_id)),
                        ),
                    )
                    .execute(conn)
                    .await?;
                }
                Ok(())
            })
            .await
            .map_err(TxError::into_room)
        }))
    }

    fn roster<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        auth_token: &'a str,
        session_max: Duration,
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
            // A member past `joined_at + session_max` is refused the same way.
            let now = Utc::now();
            let member_id = rows
                .iter()
                .find(|row| {
                    autumn_web::auth::constant_time_eq(auth_token.as_bytes(), row.token.as_bytes())
                })
                .filter(|row| now < row.joined_at.and_utc() + session_max)
                .map(|row| row.participant_id.clone())
                .ok_or(RoomError::RoomNotFound)?;

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

    fn heartbeat<'a>(
        &'a self,
        namespace: &'a str,
        room_id: &'a str,
        participant_id: &'a str,
        token: &'a str,
        token_ttl: Duration,
        session_max: Duration,
    ) -> RoomStoreFuture<'a, DateTime<Utc>> {
        Box::pin(async move {
            let mut conn = self.pool.get().await.map_err(map_db_err)?;

            // Fail-closed: an absent row and a token mismatch are the same
            // `RoomNotFound`, so a heartbeat is no membership oracle. The room
            // row is not probed separately for the same reason.
            let (stored, joined_at): (String, chrono::NaiveDateTime) =
                media_room_participants::table
                    .filter(
                        media_room_participants::namespace
                            .eq(namespace)
                            .and(media_room_participants::room_id.eq(room_id))
                            .and(media_room_participants::participant_id.eq(participant_id)),
                    )
                    .select((
                        media_room_participants::token,
                        media_room_participants::joined_at,
                    ))
                    .first(&mut conn)
                    .await
                    .optional()
                    .map_err(map_db_err)?
                    .ok_or(RoomError::RoomNotFound)?;
            if !autumn_web::auth::constant_time_eq(token.as_bytes(), stored.as_bytes()) {
                return Err(RoomError::RoomNotFound);
            }

            let now = Utc::now();
            let renewed = renewed_expiry(joined_at.and_utc(), now, token_ttl, session_max)
                .ok_or(RoomError::RoomNotFound)?;
            // Truncate to microseconds — the `Timestamp` column's resolution —
            // so the expiry this call returns is exactly the one another process
            // reads back, instead of a nanosecond-precise value the row cannot
            // hold.
            let renewed = renewed.trunc_subsecs(6);
            let updated = diesel::update(
                media_room_participants::table.filter(
                    media_room_participants::namespace
                        .eq(namespace)
                        .and(media_room_participants::room_id.eq(room_id))
                        .and(media_room_participants::participant_id.eq(participant_id)),
                ),
            )
            .set((
                media_room_participants::last_seen_at.eq(now.naive_utc()),
                media_room_participants::token_expires_at.eq(renewed.naive_utc()),
            ))
            .execute(&mut conn)
            .await
            .map_err(map_db_err)?;
            // A concurrent reaper (or leave) can drop the seat between the read
            // and the write; renewing nothing is not a live seat.
            if updated == 0 {
                return Err(RoomError::RoomNotFound);
            }
            Ok(renewed)
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
            // The emptiness test is a correlated `NOT EXISTS` matching the
            // participants' full composite key, so reaping never crosses
            // namespaces.
            //
            // A join can add a seat while the delete waits for the room row.
            // On Postgres, the waiting delete then checks the new row again but
            // keeps its old `NOT EXISTS` result, and the cascade deletes the
            // seat that the join returned (#2407). So phase 2 is one
            // transaction of three statements, each a batch, not one per room:
            //
            // 1. Claim each candidate: negate its `max_participants` (always
            //    `>= 1` otherwise). This locks the row, as `lock_room` does.
            //    Other transactions never see the negative value: it is not
            //    committed, and step 3 restores it.
            // 2. Delete each claimed room that is still empty. A new statement
            //    takes a new snapshot, so it sees each seat that a join
            //    committed before the claim got its lock. A join that starts
            //    after the claim waits for this transaction to end.
            // 3. Restore the claimed rooms that a join filled.
            let empty = || {
                diesel::dsl::not(diesel::dsl::exists(
                    media_room_participants::table.filter(
                        media_room_participants::namespace
                            .eq(media_rooms::namespace)
                            .and(media_room_participants::room_id.eq(media_rooms::room_id)),
                    ),
                ))
            };
            let swept = conn
                .transaction(async |conn| {
                    diesel::update(
                        media_rooms::table.filter(
                            media_rooms::created_at
                                .lt(cutoff)
                                .and(media_rooms::max_participants.gt(0))
                                .and(empty()),
                        ),
                    )
                    .set(media_rooms::max_participants.eq(media_rooms::max_participants * -1))
                    .execute(conn)
                    .await?;
                    let reaped = diesel::delete(
                        media_rooms::table.filter(
                            media_rooms::created_at
                                .lt(cutoff)
                                .and(media_rooms::max_participants.lt(0))
                                .and(empty()),
                        ),
                    )
                    .execute(conn)
                    .await?;
                    diesel::update(
                        media_rooms::table.filter(
                            media_rooms::created_at
                                .lt(cutoff)
                                .and(media_rooms::max_participants.lt(0)),
                        ),
                    )
                    .set(media_rooms::max_participants.eq(media_rooms::max_participants * -1))
                    .execute(conn)
                    .await?;
                    Ok::<_, diesel::result::Error>(reaped)
                })
                .await;
            match swept {
                Ok(reaped) => stats.rooms_reaped = reaped,
                Err(err) => {
                    tracing::warn!(error = %err, "media rooms: reaper room sweep failed");
                    return stats;
                }
            }

            stats
        })
    }
}
