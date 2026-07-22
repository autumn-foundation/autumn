//! Model-field type conversions on the `SQLite` runtime backend (issue #1924).
//!
//! The `SQLite` generator foundation (#1918) mapped column DDL for every field
//! kind but rejected the ones whose Rust model type had no working diesel
//! `SQLite` `FromSql`/`ToSql`. #1924 wires the conversions that reach Postgres
//! parity for two of those kinds and un-rejects them at generate time:
//!
//! - **`DateTime<Utc>`** → diesel's `TimestamptzSqlite` sql-type. Its
//!   `sqlite`+`chrono` conversion stores the value as an RFC 3339 UTC string
//!   (`SQLite` `TEXT` affinity), which sorts and compares chronologically.
//! - **`Attachment`** (`autumn_web::storage::Blob`) → a `TEXT` column holding
//!   the metadata JSON. `Blob` is a local type, so `autumn-web` supplies
//!   `FromSql`/`ToSql<Text, Sqlite>` directly (orphan-rule-legal).
//!
//! `NaiveDateTime` (core, ungated `Timestamp`) was already supported and is
//! exercised here for good measure.
//!
//! `Uuid` (`uuid::Uuid`) and `Decimal` (`rust_decimal::Decimal`) remain rejected
//! at generate time: their Rust types are foreign to `autumn-web`, so the orphan
//! rule forbids adding a `Sqlite` conversion directly, and diesel/`rust_decimal`
//! implement only Postgres-side impls. They need a per-field
//! `serialize_as`/`deserialize_as` wrapper threaded through the `#[model]` macro
//! (including nullable columns), tracked as follow-up work under #1924 — so they
//! are deliberately NOT declared as model fields here (they would not compile).
//!
//! This test declares a model carrying a `DateTime<Utc>`, a `NaiveDateTime`, and
//! an `Option<Blob>` (attachment), boots an in-memory `SQLite` pool through the
//! real [`autumn_web::db::create_pool`] path, and round-trips every field
//! (insert → read back → assert equality), plus asserts that `DateTime<Utc>`
//! values order chronologically on the `TEXT`-backed column.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_field_conversions`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{chrono, diesel, diesel_async};
use autumn_web::storage::Blob;

use chrono::{DateTime, NaiveDateTime, Utc};
use diesel::{ExpressionMethods as _, QueryDsl as _};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        conversion_rows (id) {
            id -> Int8,
            label -> Text,
            // `DateTime<Utc>` -> diesel's SQLite `TimestamptzSqlite` (#1924).
            happened_at -> TimestamptzSqlite,
            // `NaiveDateTime` -> core, ungated `Timestamp`.
            naive_at -> Timestamp,
            // `Attachment` (`Blob`) -> nullable `TEXT` metadata JSON (#1924).
            avatar -> Nullable<Text>,
        }
    }
}

use schema::conversion_rows;

#[autumn_web::model]
pub struct ConversionRow {
    #[id]
    pub id: i64,
    pub label: String,
    pub happened_at: DateTime<Utc>,
    pub naive_at: NaiveDateTime,
    pub avatar: Option<Blob>,
}

#[autumn_web::repository(ConversionRow)]
pub trait ConversionRowRepository {}

async fn boot_pool(db_name: &str) -> SqlitePool {
    // A shared-cache in-memory database so every pooled checkout observes the
    // same schema (a bare `:memory:` target is private per connection). Each
    // test passes a DISTINCT name so the two `#[tokio::test]`s — which run in
    // parallel — never share rows through the process-wide shared cache.
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query(
            "CREATE TABLE conversion_rows (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 label TEXT NOT NULL, \
                 happened_at TIMESTAMP NOT NULL, \
                 naive_at TIMESTAMP NOT NULL, \
                 avatar TEXT\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create conversion_rows table");
    }

    pool
}

#[tokio::test]
async fn field_conversions_round_trip_on_sqlite() {
    let pool = boot_pool("field_conversions_roundtrip").await;
    let repo = PgConversionRowRepository::with_pool_untracked(pool);

    // A timestamp with sub-second (microsecond) precision to prove fractional
    // seconds survive the RFC 3339 text round-trip, not just whole seconds.
    let happened_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 123_456_000)
        .expect("valid timestamp with microsecond precision");
    let naive_at = DateTime::<Utc>::from_timestamp(1_699_999_000, 0)
        .expect("valid timestamp")
        .naive_utc();
    let avatar = Blob::new("local", "avatars/42.png", "image/png", 2048).with_etag("deadbeef");

    // insert with a present attachment
    let created = repo
        .save(&NewConversionRow {
            label: "with-avatar".to_string(),
            happened_at,
            naive_at,
            avatar: Some(avatar.clone()),
        })
        .await
        .expect("save inserts a row and returns it");
    assert!(created.id > 0, "autoincrement id assigned");

    // read back — every converted field must be byte-identical
    let found = repo
        .find_by_id(created.id)
        .await
        .expect("find_by_id query")
        .expect("row exists");
    assert_eq!(found.label, "with-avatar");
    assert_eq!(
        found.happened_at, happened_at,
        "DateTime<Utc> round-trips exactly (incl. microseconds) via TimestamptzSqlite"
    );
    assert_eq!(
        found.naive_at, naive_at,
        "NaiveDateTime round-trips via the core Timestamp sql-type"
    );
    assert_eq!(
        found.avatar,
        Some(avatar),
        "Option<Blob> attachment round-trips via the SQLite Text/JSON conversion"
    );

    // a NULL attachment round-trips as None
    let no_avatar = repo
        .save(&NewConversionRow {
            label: "no-avatar".to_string(),
            happened_at,
            naive_at,
            avatar: None,
        })
        .await
        .expect("save row with NULL attachment");
    let reloaded = repo
        .find_by_id(no_avatar.id)
        .await
        .expect("find_by_id query")
        .expect("row exists");
    assert_eq!(reloaded.avatar, None, "NULL attachment round-trips as None");
}

#[tokio::test]
async fn datetime_column_orders_chronologically_on_sqlite() {
    use conversion_rows::dsl as t;

    let pool = boot_pool("field_conversions_ordering").await;
    let repo = PgConversionRowRepository::with_pool_untracked(pool.clone());

    // Insert three rows out of chronological order.
    let earliest = DateTime::<Utc>::from_timestamp(1_600_000_000, 0).unwrap();
    let middle = DateTime::<Utc>::from_timestamp(1_700_000_000, 500_000_000).unwrap();
    let latest = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap();
    let naive = earliest.naive_utc();

    for (label, ts) in [
        ("b-middle", middle),
        ("c-latest", latest),
        ("a-earliest", earliest),
    ] {
        repo.save(&NewConversionRow {
            label: label.to_string(),
            happened_at: ts,
            naive_at: naive,
            avatar: None,
        })
        .await
        .expect("save row");
    }

    // Order by the DateTime column directly in SQL — proves the RFC 3339 UTC
    // text representation sorts chronologically (not lexically-wrong).
    let mut conn = pool.get().await.expect("checkout");
    let ordered: Vec<(i64, String, DateTime<Utc>)> = t::conversion_rows
        .select((t::id, t::label, t::happened_at))
        .order(t::happened_at.asc())
        .load(&mut *conn)
        .await
        .expect("ordered load by happened_at");

    let labels: Vec<&str> = ordered.iter().map(|(_, l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        ["a-earliest", "b-middle", "c-latest"],
        "DateTime<Utc> values sort chronologically on the TEXT-backed column"
    );
    // And the values themselves survive the round-trip.
    assert_eq!(ordered[0].2, earliest);
    assert_eq!(ordered[1].2, middle);
    assert_eq!(ordered[2].2, latest);
}
