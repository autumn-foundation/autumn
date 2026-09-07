//! DB-backed sessions store on the `SQLite` runtime backend (issue #1908).
//!
//! `autumn generate auth` scaffolds a tracked-sessions table plus a store whose
//! query functions used to be bound to `diesel::pg::Pg`. That bound rejects the
//! `SQLite` `RuntimeConnection`, so the scaffolded app could not compile on a
//! `SQLite` target. The store now binds `::autumn_web::RuntimeBackend`, which
//! resolves to the backend the app was built for.
//!
//! This test mirrors the generated shape — the `SQLite`-dialect DDL the
//! generator emits, the `schema.rs` block, and the store functions with their
//! `RuntimeBackend` bound — and runs it against a real `SQLite` database. Green
//! means the sessions store type-checks **and** executes on `SQLite`: login
//! tracking, the per-request revocation gate, `last_seen_at` refresh, rotation
//! rebinding, the three revoke paths, the documented retention sweep, and
//! `ON DELETE CASCADE` on account deletion.
//!
//! Limits of the mirror, so it is not read as more than it is. It is a copy,
//! not the generator's output: a change to the templates does not change this
//! file. It also uses plain diesel derives where the generated model uses
//! `#[autumn_web::model]`, so the macro expansion itself is out of scope here.
//! The CLI-side `auth_store_connection_bounds_are_backend_agnostic` covers the
//! generated text; nothing yet cargo-checks a scaffolded `SQLite` auth app.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_tracked_sessions`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

// The named (non-glob) import shadows `diesel::prelude`'s sync `RunQueryDsl`,
// exactly as the generated store's header does.
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

// The `src/schema.rs` blocks `generate auth` appends for the two tables. The
// session field types are backend-independent (`Int8`/`Text`/`Timestamp`), so
// this is the same block a Postgres app gets.
mod schema {
    autumn_web::reexports::diesel::table! {
        users (id) {
            id -> Int8,
            email -> Text,
            created_at -> Timestamp,
        }
    }

    autumn_web::reexports::diesel::table! {
        user_sessions (id) {
            id -> Int8,
            user_id -> Int8,
            token_digest -> Text,
            ip -> Text,
            user_agent -> Text,
            ua_family -> Text,
            ua_os -> Text,
            ua_device -> Text,
            label -> Nullable<Text>,
            last_seen_at -> Timestamp,
            created_at -> Timestamp,
        }
    }

    autumn_web::reexports::diesel::allow_tables_to_appear_in_same_query!(users, user_sessions);
}

use schema::{user_sessions, users};

#[derive(Debug, Queryable, Selectable, Identifiable)]
#[diesel(table_name = user_sessions)]
#[diesel(check_for_backend(autumn_web::RuntimeBackend))]
struct UserSession {
    id: i64,
    user_id: i64,
    token_digest: String,
    #[allow(dead_code)]
    ip: String,
    #[allow(dead_code)]
    user_agent: String,
    ua_family: String,
    ua_os: String,
    #[allow(dead_code)]
    ua_device: String,
    label: Option<String>,
    last_seen_at: chrono::NaiveDateTime,
    #[allow(dead_code)]
    created_at: chrono::NaiveDateTime,
}

/// The insert shape: `id`, `label`, `last_seen_at` and `created_at` are left to
/// the column defaults, exactly as the generated `NewUserSession` does.
#[derive(Insertable)]
#[diesel(table_name = user_sessions)]
struct NewUserSession {
    user_id: i64,
    token_digest: String,
    ip: String,
    user_agent: String,
    ua_family: String,
    ua_os: String,
    ua_device: String,
}

// ── The generated store ─────────────────────────────────────────────────────

async fn sessions_for(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    user_id: i64,
) -> Vec<UserSession> {
    user_sessions::table
        .filter(user_sessions::user_id.eq(user_id))
        .order(user_sessions::last_seen_at.desc())
        .select(UserSession::as_select())
        .load(conn)
        .await
        .expect("load sessions")
}

async fn revoke_session(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    user_id: i64,
    session_id: i64,
) -> bool {
    let rows = diesel::delete(
        user_sessions::table
            .filter(user_sessions::id.eq(session_id))
            .filter(user_sessions::user_id.eq(user_id)),
    )
    .execute(conn)
    .await
    .expect("revoke session");
    rows == 1
}

async fn revoke_other_sessions(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    user_id: i64,
    current_token_digest: &str,
) -> usize {
    diesel::delete(
        user_sessions::table
            .filter(user_sessions::user_id.eq(user_id))
            .filter(user_sessions::token_digest.ne(current_token_digest)),
    )
    .execute(conn)
    .await
    .expect("revoke other sessions")
}

async fn revoke_all_sessions(
    conn: &mut impl diesel_async::AsyncConnection<Backend = ::autumn_web::RuntimeBackend>,
    user_id: i64,
) -> usize {
    diesel::delete(user_sessions::table.filter(user_sessions::user_id.eq(user_id)))
        .execute(conn)
        .await
        .expect("revoke all sessions")
}

// ── The per-request gate from the generated `routes/auth.rs` ────────────────

/// The `require_tracked_session` lookup: a missing row means revoked.
async fn tracked_session(
    conn: &mut RuntimeConnection,
    user_id: i64,
    token_digest: &str,
) -> Option<UserSession> {
    user_sessions::table
        .filter(user_sessions::token_digest.eq(token_digest))
        .filter(user_sessions::user_id.eq(user_id))
        .select(UserSession::as_select())
        .first(conn)
        .await
        .optional()
        .expect("tracked session lookup")
}

async fn record_login_session(conn: &mut RuntimeConnection, row: &NewUserSession) {
    diesel::insert_into(user_sessions::table)
        .values(row)
        .execute(conn)
        .await
        .expect("record login session");
}

/// Re-point the row at the rotated session id (step-up reauth).
async fn rebind_tracked_session(conn: &mut RuntimeConnection, old: &str, new: &str) {
    diesel::update(user_sessions::table.filter(user_sessions::token_digest.eq(old)))
        .set(user_sessions::token_digest.eq(new))
        .execute(conn)
        .await
        .expect("rebind tracked session");
}

async fn untrack_current_session(conn: &mut RuntimeConnection, token_digest: &str) {
    diesel::delete(user_sessions::table.filter(user_sessions::token_digest.eq(token_digest)))
        .execute(conn)
        .await
        .expect("untrack current session");
}

fn new_session(user_id: i64, digest: &str, family: &str) -> NewUserSession {
    NewUserSession {
        user_id,
        token_digest: digest.to_owned(),
        ip: "203.0.113.7".to_owned(),
        user_agent: "curl/8".to_owned(),
        ua_family: family.to_owned(),
        ua_os: "Linux".to_owned(),
        ua_device: "desktop".to_owned(),
    }
}

/// Boot a pool over a file-backed database and apply the `SQLite`-dialect DDL
/// `generate auth` emits for the two tables.
///
/// File-backed with several connections, not a single in-memory one: the
/// revocation contract is that a delete committed on one pooled connection is
/// visible to the next request on another, and an in-memory database also
/// ignores `journal_mode = WAL`. Mirrors `sqlite_foreign_keys.rs`.
async fn boot_pool(dir: &tempfile::TempDir) -> SqlitePool {
    let path = dir.path().join("tracked_sessions.db");
    let config = DatabaseConfig {
        url: Some(format!("sqlite://{}", path.display())),
        primary_pool_size: Some(4),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        for ddl in [
            "CREATE TABLE users (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 email TEXT NOT NULL, \
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\
             )",
            "CREATE TABLE user_sessions (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, \
                 token_digest TEXT NOT NULL UNIQUE, \
                 ip TEXT NOT NULL DEFAULT '', \
                 user_agent TEXT NOT NULL DEFAULT '', \
                 ua_family TEXT NOT NULL DEFAULT '', \
                 ua_os TEXT NOT NULL DEFAULT '', \
                 ua_device TEXT NOT NULL DEFAULT '', \
                 label TEXT NULL, \
                 last_seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\
             )",
            "CREATE INDEX user_sessions_user_id_idx ON user_sessions (user_id)",
        ] {
            diesel::sql_query(ddl)
                .execute(&mut *conn)
                .await
                .expect("apply generated auth DDL");
        }
        diesel::insert_into(users::table)
            .values(users::email.eq("owner@example.com"))
            .execute(&mut *conn)
            .await
            .expect("seed a user");
        diesel::insert_into(users::table)
            .values(users::email.eq("other@example.com"))
            .execute(&mut *conn)
            .await
            .expect("seed a second user");
    }

    pool
}

#[tokio::test]
async fn tracked_sessions_store_runs_on_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = boot_pool(&dir).await;
    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    let (owner, other) = (1_i64, 2_i64);

    // Two logins on two devices, plus one belonging to another account.
    record_login_session(&mut conn, &new_session(owner, "digest-a", "Firefox")).await;
    record_login_session(&mut conn, &new_session(owner, "digest-b", "Safari")).await;
    record_login_session(&mut conn, &new_session(other, "digest-x", "Chrome")).await;

    // The device list is scoped to the account.
    let listed = sessions_for(&mut conn, owner).await;
    assert_eq!(listed.len(), 2, "both of this account's devices are listed");
    assert!(
        listed.iter().all(|s| s.user_id == owner),
        "another account's session must not leak into the list"
    );
    assert!(
        listed
            .iter()
            .any(|s| s.ua_family == "Firefox" && s.ua_os == "Linux"),
        "the parsed User-Agent columns round-trip"
    );
    assert!(
        listed.iter().all(|s| s.label.is_none()),
        "the nullable label column defaults to NULL"
    );

    // The per-request gate finds the row and treats a missing one as revoked.
    let tracked = tracked_session(&mut conn, owner, "digest-a")
        .await
        .expect("an unrevoked session is tracked");
    assert!(
        tracked_session(&mut conn, owner, "digest-unknown")
            .await
            .is_none(),
        "an unknown digest reads as revoked"
    );
    assert!(
        tracked_session(&mut conn, other, "digest-a")
            .await
            .is_none(),
        "the gate is scoped to the authenticated account"
    );

    // `last_seen_at` refresh: the column defaults to CURRENT_TIMESTAMP and is
    // then stamped from Rust, so both encodings must read back as a timestamp.
    let stamped = tracked
        .last_seen_at
        .checked_add_signed(chrono::Duration::seconds(90))
        .expect("in-range timestamp");
    diesel::update(user_sessions::table.find(tracked.id))
        .set(user_sessions::last_seen_at.eq(stamped))
        .execute(&mut *conn)
        .await
        .expect("refresh last_seen_at");
    let refreshed = tracked_session(&mut conn, owner, "digest-a")
        .await
        .expect("still tracked");
    assert_eq!(
        refreshed.last_seen_at, stamped,
        "the refreshed timestamp round-trips"
    );
    assert_eq!(
        sessions_for(&mut conn, owner).await[0].token_digest,
        "digest-a",
        "the most recently seen session sorts first"
    );

    // `token_digest` is UNIQUE. `rebind_tracked_session` and
    // `untrack_current_session` key on it alone, unscoped by account, so that
    // constraint is what keeps them from touching another account's row.
    let duplicate = diesel::insert_into(user_sessions::table)
        .values(&new_session(other, "digest-a", "Chrome"))
        .execute(&mut *conn)
        .await;
    assert!(
        duplicate.is_err(),
        "a duplicate token_digest must be rejected by the UNIQUE constraint"
    );

    // A revoke committed on one pooled connection is visible to the next
    // request on another — the "revoked = row missing" contract.
    {
        let mut revoker = pool.get().await.expect("a second sqlite connection");
        assert!(revoke_session(&mut revoker, owner, tracked.id).await);
    }
    assert!(
        tracked_session(&mut conn, owner, "digest-a")
            .await
            .is_none(),
        "a revoke on another connection is visible immediately"
    );
    record_login_session(&mut conn, &new_session(owner, "digest-a", "Firefox")).await;

    // Rotation (step-up reauth) re-points the row at the new session id.
    rebind_tracked_session(&mut conn, "digest-a", "digest-a2").await;
    assert!(
        tracked_session(&mut conn, owner, "digest-a")
            .await
            .is_none(),
        "the pre-rotation digest no longer authenticates"
    );
    assert!(
        tracked_session(&mut conn, owner, "digest-a2")
            .await
            .is_some(),
        "the rotated digest authenticates"
    );

    // Logout drops just this device's row.
    untrack_current_session(&mut conn, "digest-a2").await;
    assert_eq!(sessions_for(&mut conn, owner).await.len(), 1);

    // "Sign out everywhere else" keeps only the current session.
    record_login_session(&mut conn, &new_session(owner, "digest-c", "Edge")).await;
    assert_eq!(revoke_other_sessions(&mut conn, owner, "digest-c").await, 1);
    let remaining = sessions_for(&mut conn, owner).await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].token_digest, "digest-c");

    // Revoking one row by id is scoped to the account.
    let victim = remaining[0].id;
    assert!(
        !revoke_session(&mut conn, other, victim).await,
        "another account cannot revoke this session"
    );
    assert!(revoke_session(&mut conn, owner, victim).await);
    assert!(sessions_for(&mut conn, owner).await.is_empty());

    // Password change revokes every session for the account, and only that one.
    record_login_session(&mut conn, &new_session(owner, "digest-d", "Firefox")).await;
    record_login_session(&mut conn, &new_session(owner, "digest-e", "Safari")).await;
    assert_eq!(revoke_all_sessions(&mut conn, owner).await, 2);
    assert!(sessions_for(&mut conn, owner).await.is_empty());
    assert_eq!(
        sessions_for(&mut conn, other).await.len(),
        1,
        "the other account's session survives"
    );
}

/// The retention sweep the scaffolded guide hands the operator, plus the
/// `ON DELETE CASCADE` the privacy posture depends on.
///
/// The sweep compares a TEXT column against `datetime('now', …)`, and the column
/// carries two encodings: `CURRENT_TIMESTAMP` writes `YYYY-MM-DD HH:MM:SS` while
/// diesel writes a `NaiveDateTime` with a fractional-second suffix. Both must
/// order correctly against the cutoff, or the sweep deletes live sessions or
/// retains personal data past the window.
#[tokio::test]
async fn retention_sweep_and_cascade_run_on_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = boot_pool(&dir).await;
    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    let (owner, other) = (1_i64, 2_i64);

    // `fresh_default` keeps the CURRENT_TIMESTAMP default (no fraction).
    record_login_session(&mut conn, &new_session(owner, "fresh-default", "Firefox")).await;
    // The other two are stamped from Rust, so they carry a fractional suffix.
    let now = chrono::Utc::now().naive_utc();
    for (digest, age_days) in [("fresh-stamped", 1), ("stale-stamped", 200)] {
        record_login_session(&mut conn, &new_session(owner, digest, "Safari")).await;
        let at = now
            .checked_sub_signed(chrono::Duration::days(age_days))
            .expect("in-range timestamp");
        diesel::update(user_sessions::table.filter(user_sessions::token_digest.eq(digest)))
            .set(user_sessions::last_seen_at.eq(at))
            .execute(&mut *conn)
            .await
            .expect("stamp last_seen_at");
    }
    record_login_session(&mut conn, &new_session(other, "other-fresh", "Chrome")).await;

    // Mixed encodings still sort chronologically: the CURRENT_TIMESTAMP row is
    // the most recent, and the 200-day-old Rust-stamped row is last.
    let ordered: Vec<String> = sessions_for(&mut conn, owner)
        .await
        .into_iter()
        .map(|s| s.token_digest)
        .collect();
    assert_eq!(
        ordered,
        vec![
            "fresh-default".to_owned(),
            "fresh-stamped".to_owned(),
            "stale-stamped".to_owned()
        ],
        "the two timestamp encodings must order against each other"
    );

    // The exact SQL in the scaffolded docs/guide/session-management.md.
    let swept = diesel::sql_query(
        "DELETE FROM user_sessions WHERE last_seen_at < datetime('now', '-90 days')",
    )
    .execute(&mut *conn)
    .await
    .expect("run the documented retention sweep");
    assert_eq!(swept, 1, "the sweep deletes only the row past the window");
    let survivors: Vec<String> = sessions_for(&mut conn, owner)
        .await
        .into_iter()
        .map(|s| s.token_digest)
        .collect();
    assert_eq!(
        survivors,
        vec!["fresh-default".to_owned(), "fresh-stamped".to_owned()],
        "neither encoding of a live session is swept"
    );

    // Account deletion erases the stored IP / User-Agent data through the
    // cascade, which needs `PRAGMA foreign_keys = ON` on the pooled connection.
    diesel::delete(users::table.filter(users::id.eq(owner)))
        .execute(&mut *conn)
        .await
        .expect("delete the account");
    assert!(
        sessions_for(&mut conn, owner).await.is_empty(),
        "ON DELETE CASCADE must erase the account's session rows"
    );
    assert_eq!(
        sessions_for(&mut conn, other).await.len(),
        1,
        "the other account's session survives"
    );
}
