//! Postgres-backed integration tests for the DB-backed `RoomStore`
//! (`autumn_media_plugin::rooms_db::DbRoomStore`, epic #1974).
//!
//! Spins up a real Postgres container via testcontainers and exercises the
//! store's full lifecycle — create → join → roster → leave persistence, roster
//! member-gating, cross-instance sharing (the multi-process property), and the
//! last-write-wins `reap_stale` sweep — exactly mirroring
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
use chrono::{Duration, Utc};
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

    (pool, container)
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
        .roster("tenant-a", &room.id, first.token.expose())
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
        .roster("tenant-a", &room.id, second.token.expose())
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
        .roster("tenant-a", &room.id, second.token.expose())
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
        store.roster("", &room.id, "").await,
        Err(RoomError::RoomNotFound)
    ));
    assert!(matches!(
        store.roster("", &room.id, "not-a-member").await,
        Err(RoomError::RoomNotFound)
    ));
    // Wrong namespace never leaks the room.
    assert!(matches!(
        store.roster("other", &room.id, "").await,
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
    assert!(store.roster("", "room-keep", "tok-fresh").await.is_ok());
    assert!(matches!(
        store.roster("", "room-keep", "tok-stale").await,
        Err(RoomError::RoomNotFound)
    ));
    // room-drop is gone entirely.
    assert!(matches!(
        store.roster("", "room-drop", "tok").await,
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
        store.roster("a", "shared-id", "tok-a").await,
        Err(RoomError::RoomNotFound)
    ));
    assert!(store.roster("b", "shared-id", "tok-b").await.is_ok());
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
    assert!(store.roster("", "room-1", "tok").await.is_ok());
}
