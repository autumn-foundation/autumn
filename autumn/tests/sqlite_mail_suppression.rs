//! `DbSuppressionStore` against the generator's own SQLite DDL (issue #2697).
//!
//! `autumn generate mailer --list-unsubscribe` emits a SQLite-dialect
//! `mail_unsubscribes` migration on SQLite apps (`INTEGER PRIMARY KEY
//! AUTOINCREMENT`, `unsubscribed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP`
//! — issue #1927), but the framework's `DbSuppressionStore` declared the
//! table with a single un-forked `Timestamptz` column that is Postgres-only,
//! and no test ever ran the store against the emitted SQLite DDL. This file
//! is the real-engine proof: boot an in-memory SQLite pool through the same
//! [`autumn_web::db::create_pool`] path a generated app uses, apply the
//! generator's SQLite DDL verbatim, and drive `suppress` → `is_suppressed`
//! → `is_suppressed_many` through [`DbSuppressionStore`], including the
//! DB-defaulted `unsubscribed_at` the store never names.
//!
//! Only meaningful under `--features sqlite,mail`; the file is
//! `#![cfg(all(feature = "sqlite", feature = "mail"))]` so a default
//! `cargo test` compiles it to an empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite,mail --test sqlite_mail_suppression`.
#![cfg(all(feature = "sqlite", feature = "mail"))]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::mail::SuppressionStore;
use autumn_web::mail::db_suppression::DbSuppressionStore;
use autumn_web::reexports::{diesel, diesel_async};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

// Verbatim the SQLite DDL `autumn generate mailer --list-unsubscribe` emits
// (`autumn-cli/src/generate/mailer.rs`, `UNSUBSCRIBE_MIGRATION_UP_SQLITE`,
// issue #1927) — the store must work against exactly this shape.
const GENERATOR_SQLITE_DDL: &str = "\
CREATE TABLE mail_unsubscribes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    subscriber TEXT NOT NULL,
    list_id TEXT NOT NULL,
    unsubscribed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (subscriber, list_id)
);";

async fn boot_pool(test_name: &str) -> SqlitePool {
    // Each test gets its own named shared-cache database: the `cache=shared`
    // name is process-global, so a single fixed name would let parallel tests
    // collide on `mail_unsubscribes` (and leak rows into each other).
    let url = format!("sqlite://file:mail_suppression_{test_name}?mode=memory&cache=shared");
    let config = DatabaseConfig {
        url: Some(url),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    diesel::sql_query(GENERATOR_SQLITE_DDL)
        .execute(&mut *conn)
        .await
        .expect("generator's SQLite DDL applies cleanly");
    drop(conn);

    pool
}

#[tokio::test]
async fn suppress_round_trips_through_the_store() {
    let pool = boot_pool("round_trip").await;
    let store = DbSuppressionStore::new(pool);

    assert!(
        !store
            .is_suppressed("ada@example.com", "news")
            .await
            .expect("is_suppressed runs"),
        "nothing suppressed yet"
    );

    store
        .suppress("ada@example.com", "news")
        .await
        .expect("suppress inserts");
    // Idempotent: the UNIQUE (subscriber, list_id) constraint makes a repeat
    // a no-op, not an error.
    store
        .suppress("ada@example.com", "news")
        .await
        .expect("suppress is idempotent");

    assert!(
        store
            .is_suppressed("ada@example.com", "news")
            .await
            .expect("is_suppressed runs"),
        "the suppressed (subscriber, list_id) pair reads back"
    );
    assert!(
        !store
            .is_suppressed("ada@example.com", "other-list")
            .await
            .expect("is_suppressed runs"),
        "suppression is scoped to the list it was recorded for"
    );
    assert!(
        !store
            .is_suppressed("grace@example.com", "news")
            .await
            .expect("is_suppressed runs"),
        "other subscribers are unaffected"
    );
}

#[tokio::test]
async fn is_suppressed_many_resolves_the_batch() {
    let pool = boot_pool("batch").await;
    let store = DbSuppressionStore::new(pool);

    store
        .suppress("ada@example.com", "news")
        .await
        .expect("suppress ada");
    store
        .suppress("grace@example.com", "news")
        .await
        .expect("suppress grace");

    let hits = store
        .is_suppressed_many(
            &["ada@example.com", "hopper@example.com", "grace@example.com"],
            "news",
        )
        .await
        .expect("is_suppressed_many runs");
    assert_eq!(hits.len(), 2, "exactly the two suppressed subscribers hit");
    assert!(hits.contains("ada@example.com"));
    assert!(hits.contains("grace@example.com"));

    let empty = store
        .is_suppressed_many(&["hopper@example.com"], "news")
        .await
        .expect("is_suppressed_many runs");
    assert!(empty.is_empty(), "no suppression, no hits");
}

#[tokio::test]
async fn db_default_fills_unsubscribed_at() {
    let pool = boot_pool("db_default").await;
    let store = DbSuppressionStore::new(pool.clone());

    store
        .suppress("ada@example.com", "news")
        .await
        .expect("suppress inserts");

    // The store never names `unsubscribed_at`; the generator's
    // `DEFAULT CURRENT_TIMESTAMP` must fill it, proving the declared column
    // and the emitted DDL agree (the mismatch issue #2697 records).
    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    let filled: i64 = diesel::dsl::select(diesel::dsl::sql::<diesel::sql_types::BigInt>(
        "COUNT(*) FROM mail_unsubscribes WHERE unsubscribed_at IS NOT NULL",
    ))
    .get_result(&mut *conn)
    .await
    .expect("count rows with a defaulted timestamp");
    assert_eq!(filled, 1, "the DB default filled unsubscribed_at");
}
