//! Postgres-backed integration tests for the DB-backed `RoomStore`
//! (`autumn_media_plugin::rooms_db::DbRoomStore`, epic #1974).
//!
//! Spins up a real Postgres container via testcontainers and exercises the
//! store's full lifecycle — create → join → roster → leave persistence, roster
//! member-gating, cross-instance sharing (the multi-process property), the
//! heartbeat's persisted liveness/expiry renewal, and the last-write-wins
//! `reap_stale` sweep — exactly mirroring
//! `autumn-admin-plugin/tests/token_admin_db.rs`.
//!
//! **Requires Docker.** These tests are `#[ignore]`d so a default `cargo test`
//! never needs a daemon; run them with `cargo test -p autumn-media-plugin --
//! --ignored`.
//!
//! The store's queries are written against autumn-web's `RuntimeConnection`
//! alias, which is `AsyncPgConnection` in the default (Postgres) build — the
//! backend this container provides — so `DbRoomStore` accepts the pool built
//! here directly. The `SQLite` lane compiles against the same alias but is not
//! exercised here (a `SQLite` test harness would require flipping the whole
//! build graph's `sqlite` feature; the queries are backend-portable by
//! construction).

use std::sync::Arc;

use autumn_media_plugin::rooms::{RoomError, RoomStore};
use autumn_media_plugin::rooms_db::DbRoomStore;
use chrono::{Duration, SubsecRound as _, Utc};
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Matches `migrations/20260720000000_media_rooms/up.sql` (portable TIMESTAMP
/// columns, composite keys, cascade + sweep indexes).
const CREATE_TABLES_SQL: &str = "
    CREATE TABLE IF NOT EXISTS media_rooms (
        namespace         TEXT      NOT NULL,
        room_id           TEXT      NOT NULL,
        max_participants  INTEGER   NOT NULL,
        created_at        TIMESTAMP NOT NULL,
        PRIMARY KEY (namespace, room_id)
    );
    CREATE TABLE IF NOT EXISTS media_room_participants (
        namespace         TEXT      NOT NULL,
        room_id           TEXT      NOT NULL,
        participant_id    TEXT      NOT NULL,
        display_name      TEXT,
        token             TEXT      NOT NULL,
        joined_at         TIMESTAMP NOT NULL,
        token_expires_at  TIMESTAMP NOT NULL,
        last_seen_at      TIMESTAMP NOT NULL,
        PRIMARY KEY (namespace, room_id, participant_id),
        FOREIGN KEY (namespace, room_id)
            REFERENCES media_rooms (namespace, room_id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS media_room_participants_last_seen_idx
        ON media_room_participants (last_seen_at);
    CREATE INDEX IF NOT EXISTS media_rooms_created_at_idx
        ON media_rooms (created_at);
";

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let (pool, _url, container) = setup_db().await;
    (pool, container)
}

/// [`setup_pool`], plus the database URL, so a test can open a second pool
/// over the same database (the shape of a second app process).
async fn setup_db() -> (
    Pool<AsyncPgConnection>,
    String,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");

    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("DROP TABLE IF EXISTS media_room_participants")
        .execute(&mut conn)
        .await
        .expect("drop participants");
    diesel::sql_query("DROP TABLE IF EXISTS media_rooms")
        .execute(&mut conn)
        .await
        .expect("drop rooms");
    // The multi-statement DDL string is split so each runs as its own query.
    for stmt in CREATE_TABLES_SQL.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        diesel::sql_query(stmt)
            .execute(&mut conn)
            .await
            .expect("create table");
    }

    (pool, url, container)
}

/// Seed a room and its participants with explicit timestamps via raw SQL, so the
/// reaper tests can control `created_at` / `last_seen_at` deterministically
/// (mirroring the in-memory `seed_room` helper).
async fn seed(
    pool: &Pool<AsyncPgConnection>,
    namespace: &str,
    room_id: &str,
    created_at: chrono::DateTime<Utc>,
    seats: &[(&str, &str, chrono::DateTime<Utc>)],
) {
    let mut conn = pool.get().await.expect("conn");
    let created = created_at.naive_utc().format("%Y-%m-%d %H:%M:%S%.6f");
    diesel::sql_query(format!(
        "INSERT INTO media_rooms (namespace, room_id, max_participants, created_at) \
         VALUES ('{namespace}', '{room_id}', 6, '{created}')"
    ))
    .execute(&mut conn)
    .await
    .expect("seed room");
    for (id, token, last_seen) in seats {
        let seen = last_seen.naive_utc().format("%Y-%m-%d %H:%M:%S%.6f");
        diesel::sql_query(format!(
            "INSERT INTO media_room_participants \
             (namespace, room_id, participant_id, display_name, token, joined_at, token_expires_at, last_seen_at) \
             VALUES ('{namespace}', '{room_id}', '{id}', NULL, '{token}', '{seen}', '{seen}', '{seen}')"
        ))
        .execute(&mut conn)
        .await
        .expect("seed participant");
    }
}

/// One `token_expires_at` column, for reading a renewal back out of the row.
#[derive(diesel::QueryableByName)]
struct ExpiryRow {
    #[diesel(sql_type = diesel::sql_types::Timestamp)]
    token_expires_at: chrono::NaiveDateTime,
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn create_join_roster_leave_round_trips_and_persists() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);

    // Create.
    let room = store.create_room("tenant-a", 4).await.expect("create");
    assert_eq!(room.max_participants, 4);
    assert!(room.participants.is_empty());
    assert_eq!(room.namespace, "tenant-a");

    // Join two participants.
    let first = store
        .join_room(
            "tenant-a",
            &room.id,
            Some("Ada".to_owned()),
            Duration::seconds(300),
        )
        .await
        .expect("join first");
    let second = store
        .join_room(
            "tenant-a",
            &room.id,
            Some("Grace".to_owned()),
            Duration::seconds(300),
        )
        .await
        .expect("join second");
    assert_ne!(first.participant_id, second.participant_id);
    assert!(!first.token.expose().is_empty());

    // Roster (member-gated) reflects both joins and persisted display names.
    let roster = store
        .roster(
            "tenant-a",
            &room.id,
            first.token.expose(),
            Duration::hours(12),
        )
        .await
        .expect("roster");
    assert_eq!(roster.participants.len(), 2);
    assert!(
        roster
            .participants
            .iter()
            .any(|p| p.display_name.as_deref() == Some("Grace"))
    );

    // A brand-new store instance over the SAME pool sees the SAME room — the
    // multi-process property the whole feature exists for.
    let other: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool.clone(), 6));
    let roster2 = other
        .roster(
            "tenant-a",
            &room.id,
            second.token.expose(),
            Duration::hours(12),
        )
        .await
        .expect("second instance roster");
    assert_eq!(roster2.participants.len(), 2);

    // Leave removes the seat.
    store
        .leave_room(
            "tenant-a",
            &room.id,
            &first.participant_id,
            first.token.expose(),
        )
        .await
        .expect("leave");
    let roster3 = store
        .roster(
            "tenant-a",
            &room.id,
            second.token.expose(),
            Duration::hours(12),
        )
        .await
        .expect("roster after leave");
    assert_eq!(roster3.participants.len(), 1);
    assert_eq!(roster3.participants[0].id, second.participant_id);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn roster_is_member_gated_fail_closed() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool, 6);

    let room = store.create_room("", 4).await.expect("create");
    store
        .join_room("", &room.id, None, Duration::seconds(300))
        .await
        .expect("join");

    // No token / a wrong token resolve to the SAME error a nonexistent room
    // returns — no membership oracle.
    assert!(matches!(
        store.roster("", &room.id, "", Duration::hours(12)).await,
        Err(RoomError::RoomNotFound)
    ));
    assert!(matches!(
        store
            .roster("", &room.id, "not-a-member", Duration::hours(12))
            .await,
        Err(RoomError::RoomNotFound)
    ));
    // Wrong namespace never leaks the room.
    assert!(matches!(
        store
            .roster("other", &room.id, "", Duration::hours(12))
            .await,
        Err(RoomError::RoomNotFound)
    ));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn join_enforces_capacity_and_leave_drops_empty_room() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool, 6);

    let room = store.create_room("", 1).await.expect("create");
    let only = store
        .join_room("", &room.id, None, Duration::seconds(300))
        .await
        .expect("join");
    // The room is now full (cap 1).
    assert!(matches!(
        store
            .join_room("", &room.id, None, Duration::seconds(300))
            .await,
        Err(RoomError::RoomFull { max: 1 })
    ));
    // The last participant leaving drops the room entirely.
    store
        .leave_room("", &room.id, &only.participant_id, only.token.expose())
        .await
        .expect("leave");
    // Rejoining a dropped room is RoomNotFound.
    assert!(matches!(
        store
            .join_room("", &room.id, None, Duration::seconds(300))
            .await,
        Err(RoomError::RoomNotFound)
    ));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reap_evicts_stale_participant_and_drops_emptied_room() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let now = Utc::now();
    let ttl = Duration::minutes(30);

    // room-keep: one stale + one fresh seat → stale evicted, room survives.
    seed(
        &pool,
        "",
        "room-keep",
        now - Duration::hours(2),
        &[
            ("stale", "tok-stale", now - Duration::hours(1)),
            ("fresh", "tok-fresh", now - Duration::minutes(5)),
        ],
    )
    .await;
    // room-drop: only a stale seat → emptied by reaping and dropped.
    seed(
        &pool,
        "",
        "room-drop",
        now - Duration::hours(2),
        &[("stale", "tok", now - Duration::hours(1))],
    )
    .await;

    let stats = store.reap_stale(now, ttl).await;
    assert_eq!(stats.participants_reaped, 2);
    assert_eq!(stats.rooms_reaped, 1);

    // room-keep still resolves for its fresh member; the stale one is gone.
    assert!(
        store
            .roster("", "room-keep", "tok-fresh", Duration::hours(12))
            .await
            .is_ok()
    );
    assert!(matches!(
        store
            .roster("", "room-keep", "tok-stale", Duration::hours(12))
            .await,
        Err(RoomError::RoomNotFound)
    ));
    // room-drop is gone entirely.
    assert!(matches!(
        store
            .roster("", "room-drop", "tok", Duration::hours(12))
            .await,
        Err(RoomError::RoomNotFound)
    ));

    // Idempotent / last-write-wins: a second sweep reaps nothing.
    let again = store.reap_stale(now, ttl).await;
    assert_eq!(again.participants_reaped, 0);
    assert_eq!(again.rooms_reaped, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reap_drops_created_never_joined_room_but_keeps_a_fresh_empty_room() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let now = Utc::now();
    let ttl = Duration::minutes(30);

    seed(&pool, "", "old-empty", now - Duration::hours(1), &[]).await;
    seed(&pool, "", "new-empty", now - Duration::minutes(5), &[]).await;

    let stats = store.reap_stale(now, ttl).await;
    assert_eq!(stats.participants_reaped, 0);
    assert_eq!(stats.rooms_reaped, 1);

    // A just-created empty room within the TTL survives (create→first-join
    // window is never reaped out from under a joiner); an old empty one is gone.
    let fresh = store
        .join_room("", "new-empty", None, Duration::seconds(300))
        .await;
    assert!(
        fresh.is_ok(),
        "fresh empty room survived and accepts a join"
    );
    assert!(matches!(
        store
            .join_room("", "old-empty", None, Duration::seconds(300))
            .await,
        Err(RoomError::RoomNotFound)
    ));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reap_never_crosses_namespaces() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let now = Utc::now();
    let ttl = Duration::minutes(30);

    // Same room_id in two namespaces: ns "a" is stale, ns "b" is fresh.
    seed(
        &pool,
        "a",
        "shared-id",
        now - Duration::hours(2),
        &[("stale", "tok-a", now - Duration::hours(1))],
    )
    .await;
    seed(
        &pool,
        "b",
        "shared-id",
        now - Duration::minutes(1),
        &[("fresh", "tok-b", now - Duration::minutes(1))],
    )
    .await;

    let stats = store.reap_stale(now, ttl).await;
    assert_eq!(stats.rooms_reaped, 1);
    assert_eq!(stats.participants_reaped, 1);

    // ns "a" room reaped; the identically-named ns "b" room is untouched.
    assert!(matches!(
        store
            .roster("a", "shared-id", "tok-a", Duration::hours(12))
            .await,
        Err(RoomError::RoomNotFound)
    ));
    assert!(
        store
            .roster("b", "shared-id", "tok-b", Duration::hours(12))
            .await
            .is_ok()
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reap_on_a_clean_store_is_a_zero_count_no_op() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let now = Utc::now();

    // A fresh room+seat: nothing to reap.
    seed(
        &pool,
        "",
        "room-1",
        now - Duration::minutes(1),
        &[("fresh", "tok", now - Duration::minutes(1))],
    )
    .await;
    let stats = store.reap_stale(now, Duration::minutes(30)).await;
    assert_eq!(stats.participants_reaped, 0);
    assert_eq!(stats.rooms_reaped, 0);
    assert!(
        store
            .roster("", "room-1", "tok", Duration::hours(12))
            .await
            .is_ok()
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn heartbeat_holds_a_seat_across_a_sweep_and_renews_the_advisory_expiry() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let stale = Utc::now() - Duration::hours(1);
    seed(
        &pool,
        "",
        "room-1",
        Utc::now() - Duration::hours(2),
        &[("beating", "tok-a", stale), ("silent", "tok-b", stale)],
    )
    .await;

    let before = Utc::now();
    let renewed = store
        .heartbeat(
            "",
            "room-1",
            "beating",
            "tok-a",
            Duration::seconds(300),
            Duration::hours(12),
        )
        .await
        .expect("heartbeat");
    // The renewal honors the supplied TTL, not some other horizon.
    assert!(renewed >= before + Duration::seconds(300) - Duration::microseconds(1));
    assert!(renewed <= Utc::now() + Duration::seconds(300));

    // The renewed expiry is persisted, so another process sees it.
    let persisted: chrono::NaiveDateTime = {
        let mut conn = pool.get().await.expect("conn");
        let row: ExpiryRow = diesel::sql_query(
            "SELECT token_expires_at FROM media_room_participants \
             WHERE namespace = '' AND room_id = 'room-1' AND participant_id = 'beating'",
        )
        .get_result(&mut conn)
        .await
        .expect("read expiry");
        row.token_expires_at
    };
    assert_eq!(persisted, renewed.naive_utc());

    // The heartbeat — not a roster poll — is what saves the seat.
    let stats = store.reap_stale(Utc::now(), Duration::minutes(30)).await;
    assert_eq!(stats.participants_reaped, 1, "only the silent seat reaped");
    assert!(
        store
            .roster("", "room-1", "tok-a", Duration::hours(12))
            .await
            .is_ok()
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn heartbeat_is_fail_closed_with_no_membership_oracle() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let now = Utc::now();
    seed(&pool, "tenant-a", "room-1", now, &[("p1", "tok", now)]).await;
    let ttl = Duration::seconds(300);

    for (namespace, room, participant, token, case) in [
        ("tenant-a", "nope", "p1", "tok", "unknown room"),
        ("tenant-a", "room-1", "ghost", "tok", "unknown participant"),
        ("tenant-a", "room-1", "p1", "wrong", "wrong token"),
        ("tenant-b", "room-1", "p1", "tok", "other namespace"),
    ] {
        assert!(
            matches!(
                store
                    .heartbeat(
                        namespace,
                        room,
                        participant,
                        token,
                        ttl,
                        Duration::hours(12)
                    )
                    .await,
                Err(RoomError::RoomNotFound)
            ),
            "{case} must be indistinguishable from a missing room"
        );
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn heartbeat_rejects_a_sibling_participants_token() {
    // The token is verified against the named participant, not against any
    // member of the room.
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let now = Utc::now();
    seed(
        &pool,
        "",
        "room-1",
        now,
        &[("p1", "tok-1", now), ("p2", "tok-2", now)],
    )
    .await;

    assert!(matches!(
        store
            .heartbeat(
                "",
                "room-1",
                "p2",
                "tok-1",
                Duration::seconds(300),
                Duration::hours(12)
            )
            .await,
        Err(RoomError::RoomNotFound)
    ));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn heartbeat_on_a_seat_reaped_concurrently_reports_it_gone() {
    // The store reads the token, then writes. A reaper (or another process's
    // leave) between the two renews nothing, which must not read as success.
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let stale = Utc::now() - Duration::hours(1);
    seed(&pool, "", "room-1", stale, &[("p1", "tok", stale)]).await;

    let stats = store.reap_stale(Utc::now(), Duration::minutes(30)).await;
    assert_eq!(stats.participants_reaped, 1);

    assert!(matches!(
        store
            .heartbeat(
                "",
                "room-1",
                "p1",
                "tok",
                Duration::seconds(300),
                Duration::hours(12)
            )
            .await,
        Err(RoomError::RoomNotFound)
    ));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn heartbeat_caps_the_expiry_at_the_session_limit() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    // Seeded rows hold microseconds, so truncate before the compare.
    let joined = (Utc::now() - Duration::hours(12) + Duration::minutes(1)).trunc_subsecs(6);
    seed(&pool, "", "room-1", joined, &[("p1", "tok", joined)]).await;

    let renewed = store
        .heartbeat(
            "",
            "room-1",
            "p1",
            "tok",
            Duration::seconds(300),
            Duration::hours(12),
        )
        .await
        .expect("heartbeat inside the session limit");

    assert_eq!(renewed, joined + Duration::hours(12));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn heartbeat_after_the_session_limit_is_refused() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let joined = (Utc::now() - Duration::hours(13)).trunc_subsecs(6);
    seed(&pool, "", "room-1", joined, &[("p1", "tok", joined)]).await;

    let result = store
        .heartbeat(
            "",
            "room-1",
            "p1",
            "tok",
            Duration::seconds(300),
            Duration::hours(12),
        )
        .await;

    assert!(matches!(result, Err(RoomError::RoomNotFound)));
    let (expires, seen) = seat_clocks(&pool, "p1").await;
    assert_eq!(
        expires,
        joined.naive_utc().trunc_subsecs(6),
        "no expiry renewal"
    );
    assert_eq!(
        seen,
        joined.naive_utc().trunc_subsecs(6),
        "no liveness refresh"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn roster_after_the_session_limit_is_refused() {
    let (pool, _container) = setup_pool().await;
    let store = DbRoomStore::new(pool.clone(), 6);
    let joined = (Utc::now() - Duration::hours(13)).trunc_subsecs(6);
    seed(&pool, "", "room-1", joined, &[("p1", "tok", joined)]).await;

    let result = store.roster("", "room-1", "tok", Duration::hours(12)).await;

    assert!(matches!(result, Err(RoomError::RoomNotFound)));
    let (_, seen) = seat_clocks(&pool, "p1").await;
    assert_eq!(seen, joined.naive_utc(), "no liveness refresh");
}

/// One seat's `(token_expires_at, last_seen_at)` in room `room-1`.
async fn seat_clocks(
    pool: &Pool<AsyncPgConnection>,
    participant_id: &str,
) -> (chrono::NaiveDateTime, chrono::NaiveDateTime) {
    let mut conn = pool.get().await.expect("conn");
    let row: ClockRow = diesel::sql_query(format!(
        "SELECT token_expires_at, last_seen_at FROM media_room_participants \
         WHERE namespace = '' AND room_id = 'room-1' AND participant_id = '{participant_id}'"
    ))
    .get_result(&mut conn)
    .await
    .expect("read seat clocks");
    (row.token_expires_at, row.last_seen_at)
}

/// The two clock columns of one seat.
#[derive(diesel::QueryableByName)]
struct ClockRow {
    #[diesel(sql_type = diesel::sql_types::Timestamp)]
    token_expires_at: chrono::NaiveDateTime,
    #[diesel(sql_type = diesel::sql_types::Timestamp)]
    last_seen_at: chrono::NaiveDateTime,
}

// ── Concurrency: the seat cap and a returned seat hold (#2864, #3104, #2407) ──

/// The longest a store call in these tests may take.
const CALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A checked-out test connection.
type PooledConn = diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>;

/// A pool of `size` connections over `url`, all opened before it is returned,
/// so a burst of callers does not wait on connection setup.
async fn warm_pool(url: &str, size: usize) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager).max_size(size).build().expect("pool");
    let mut held = Vec::with_capacity(size);
    for _ in 0..size {
        held.push(pool.get().await.expect("warm conn"));
    }
    drop(held);
    pool
}

/// One `COUNT(*)` result.
#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// One `COUNT(*)` query.
async fn count(pool: &Pool<AsyncPgConnection>, sql: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count")
        .n
}

/// Seat rows stored for `room_id`, in any namespace.
async fn seat_rows(pool: &Pool<AsyncPgConnection>, room_id: &str) -> i64 {
    count(
        pool,
        &format!("SELECT COUNT(*) AS n FROM media_room_participants WHERE room_id = '{room_id}'"),
    )
    .await
}

/// Room rows stored with `room_id`, in any namespace.
async fn room_rows(pool: &Pool<AsyncPgConnection>, room_id: &str) -> i64 {
    count(
        pool,
        &format!("SELECT COUNT(*) AS n FROM media_rooms WHERE room_id = '{room_id}'"),
    )
    .await
}

/// Client sessions that another session blocks right now.
async fn lock_waiters(pool: &Pool<AsyncPgConnection>) -> i64 {
    count(
        pool,
        "SELECT COUNT(*) AS n FROM pg_stat_activity \
         WHERE backend_type = 'client backend' AND datname = current_database() \
         AND cardinality(pg_blocking_pids(pid)) > 0",
    )
    .await
}

/// Wait until `waiters` sessions are blocked, or `task` is done.
async fn settle<T>(
    pool: &Pool<AsyncPgConnection>,
    task: &tokio::task::JoinHandle<T>,
    waiters: i64,
) {
    let deadline = tokio::time::Instant::now() + CALL_LIMIT;
    while tokio::time::Instant::now() < deadline {
        if task.is_finished() || lock_waiters(pool).await >= waiters {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("neither {waiters} blocked sessions nor a finished task in {CALL_LIMIT:?}");
}

/// Await `task`, but fail the test if it takes longer than [`CALL_LIMIT`].
async fn finish<T>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(CALL_LIMIT, task)
        .await
        .expect("the store call must not hang")
        .expect("task")
}

/// Hold the row of `room_id` from a connection outside every store, as an
/// in-flight writer does. Release it with [`release`].
async fn hold_room_row(pool: &Pool<AsyncPgConnection>, room_id: &str) -> PooledConn {
    use diesel_async::SimpleAsyncConnection as _;
    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute("BEGIN").await.expect("begin");
    conn.batch_execute(&format!(
        "UPDATE media_rooms SET max_participants = max_participants WHERE room_id = '{room_id}'"
    ))
    .await
    .expect("lock room row");
    conn
}

/// End the transaction that [`hold_room_row`] opened.
async fn release(holder: &mut PooledConn) {
    use diesel_async::SimpleAsyncConnection as _;
    holder.batch_execute("COMMIT").await.expect("release");
}

/// Spawn one join on `store`.
fn spawn_join(
    store: &Arc<DbRoomStore>,
    room_id: &str,
) -> tokio::task::JoinHandle<Result<autumn_media_plugin::rooms::JoinRecord, RoomError>> {
    let (store, room_id) = (store.clone(), room_id.to_owned());
    tokio::spawn(async move {
        store
            .join_room("", &room_id, None, Duration::seconds(300))
            .await
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_joins_from_two_processes_never_pass_the_seat_cap() {
    // #2864 / #3104: two pools over one database are two app processes. The
    // join was a COUNT, then a separate INSERT, so racers all saw a free seat.
    let (_pool, url, _container) = setup_db().await;
    let pool_a = warm_pool(&url, 8).await;
    let pool_b = warm_pool(&url, 8).await;
    let store_a = Arc::new(DbRoomStore::new(pool_a.clone(), 6));
    let store_b = Arc::new(DbRoomStore::new(pool_b, 6));

    // Cap 1 is the last-seat race. Cap 3 also catches an off-by-one.
    let caps = std::iter::repeat_n(1, 20).chain(std::iter::repeat_n(3, 5));
    for (trial, cap) in caps.enumerate() {
        let room = store_a.create_room("", cap).await.expect("create");
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut joins = Vec::new();
        for racer in 0..16 {
            let store = if racer % 2 == 0 {
                store_a.clone()
            } else {
                store_b.clone()
            };
            let barrier = barrier.clone();
            let room_id = room.id.clone();
            joins.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .join_room("", &room_id, None, Duration::seconds(300))
                    .await
            }));
        }

        let mut admitted = 0;
        for join in joins {
            match finish(join).await {
                Ok(_) => admitted += 1,
                Err(RoomError::RoomFull { max }) if max == cap => {}
                Err(other) => panic!("trial {trial}: unexpected join error: {other}"),
            }
        }
        let rows = seat_rows(&pool_a, &room.id).await;
        assert_eq!(
            (admitted, rows),
            (cap, i64::try_from(cap).unwrap()),
            "trial {trial}: a {cap}-seat room admitted {admitted} joins and stores {rows} seats"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn join_racing_the_reaper_never_returns_a_seat_that_is_gone() {
    // #2407: the reaper deletes an idle empty room. A join that starts while
    // the delete waits must not get a seat that the cascade then deletes.
    let (pool, _container) = setup_pool().await;
    let store = Arc::new(DbRoomStore::new(pool.clone(), 6));
    let idle = Utc::now() - Duration::hours(1);
    seed(&pool, "", "room-1", idle, &[]).await;
    // A second idle room proves that the sweep ran and committed.
    seed(&pool, "", "room-2", idle, &[]).await;

    let mut holder = hold_room_row(&pool, "room-1").await;
    let reaper = {
        let store = store.clone();
        tokio::spawn(async move { store.reap_stale(Utc::now(), Duration::minutes(30)).await })
    };
    settle(&pool, &reaper, 1).await;
    let join = spawn_join(&store, "room-1");
    settle(&pool, &join, 2).await;
    release(&mut holder).await;

    // The reaper waited first, so it gets the row first.
    let stats = finish(reaper).await;
    assert_eq!(stats.rooms_reaped, 2);
    assert!(
        matches!(finish(join).await, Err(RoomError::RoomNotFound)),
        "the join must see the room as gone"
    );
    assert_eq!(seat_rows(&pool, "room-1").await, 0);
    assert_eq!(room_rows(&pool, "room-1").await, 0);
    assert_eq!(room_rows(&pool, "room-2").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn join_racing_the_last_leave_never_returns_a_seat_that_is_gone() {
    // The last leave deletes its room. A join that starts between the leave's
    // count and its delete must not have its seat deleted by the cascade.
    let (pool, _container) = setup_pool().await;
    let store = Arc::new(DbRoomStore::new(pool.clone(), 6));
    seed(
        &pool,
        "",
        "room-1",
        Utc::now(),
        &[("p1", "tok", Utc::now())],
    )
    .await;

    let mut holder = hold_room_row(&pool, "room-1").await;
    let leave = {
        let store = store.clone();
        tokio::spawn(async move { store.leave_room("", "room-1", "p1", "tok").await })
    };
    settle(&pool, &leave, 1).await;
    let join = spawn_join(&store, "room-1");
    settle(&pool, &join, 2).await;
    release(&mut holder).await;

    // The leave waited first: it empties and deletes the room.
    finish(leave).await.expect("leave");
    assert!(
        matches!(finish(join).await, Err(RoomError::RoomNotFound)),
        "the join must see the room as gone"
    );
    assert_eq!(seat_rows(&pool, "room-1").await, 0);
    assert_eq!(room_rows(&pool, "room-1").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn reaper_racing_a_join_that_got_the_row_first_keeps_the_room_and_its_cap() {
    // The join gets the room row first, then the reaper reaches the same row.
    // Postgres claims it on its re-check (the `NOT EXISTS` keeps its old
    // snapshot), so the restore step must put the cap back.
    let (pool, _container) = setup_pool().await;
    let store = Arc::new(DbRoomStore::new(pool.clone(), 6));
    let idle = Utc::now() - Duration::hours(1);
    seed(&pool, "", "room-1", idle, &[]).await;
    seed(&pool, "", "room-2", idle, &[]).await;

    let mut holder = hold_room_row(&pool, "room-1").await;
    let join = spawn_join(&store, "room-1");
    settle(&pool, &join, 1).await;
    let reaper = {
        let store = store.clone();
        tokio::spawn(async move { store.reap_stale(Utc::now(), Duration::minutes(30)).await })
    };
    settle(&pool, &reaper, 2).await;
    release(&mut holder).await;

    let joined = finish(join).await.expect("join gets the seat");
    let stats = finish(reaper).await;
    assert_eq!(stats.rooms_reaped, 1, "only the empty room-2 is reaped");
    assert_eq!(room_rows(&pool, "room-2").await, 0);
    assert_eq!(seat_rows(&pool, "room-1").await, 1);
    let roster = store
        .roster("", "room-1", joined.token.expose(), Duration::hours(12))
        .await
        .expect("roster");
    assert_eq!(roster.max_participants, 6, "the claim is restored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn a_dropped_join_does_not_leave_the_room_row_locked() {
    // A client disconnect drops the handler future while the join waits for
    // the lock. The join transaction must still end, or the room stays
    // locked for every later join.
    let (pool, _container) = setup_pool().await;
    let store = Arc::new(DbRoomStore::new(pool.clone(), 6));
    seed(&pool, "", "room-1", Utc::now(), &[]).await;

    let mut holder = hold_room_row(&pool, "room-1").await;
    let dropped = spawn_join(&store, "room-1");
    settle(&pool, &dropped, 1).await;
    dropped.abort();
    let _ = dropped.await;
    release(&mut holder).await;

    let next = tokio::time::timeout(
        CALL_LIMIT,
        store.join_room("", "room-1", None, Duration::seconds(300)),
    )
    .await
    .expect("a later join must not wait on a dropped one");
    next.expect("join");
    // The dropped join rolls back, so it holds no seat.
    assert_eq!(seat_rows(&pool, "room-1").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn a_leave_with_a_wrong_token_takes_no_lock() {
    // Leave needs no session. A wrong token must fail before the room lock,
    // so it cannot queue behind (or block) the room's writers.
    let (pool, _container) = setup_pool().await;
    let store = Arc::new(DbRoomStore::new(pool.clone(), 6));
    seed(
        &pool,
        "",
        "room-1",
        Utc::now(),
        &[("p1", "tok", Utc::now())],
    )
    .await;

    let mut holder = hold_room_row(&pool, "room-1").await;
    let wrong = tokio::time::timeout(
        CALL_LIMIT,
        store.leave_room("", "room-1", "p1", "not-the-token"),
    )
    .await
    .expect("a wrong-token leave must not wait for the room lock");
    release(&mut holder).await;

    assert!(matches!(wrong, Err(RoomError::Unauthorized)));
    assert_eq!(seat_rows(&pool, "room-1").await, 1);
}
