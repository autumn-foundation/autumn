//! `unique_violation_field` against a real `SQLite` unique-violation (issue
//! #2698).
//!
//! Diesel boxes a `SQLite` `DatabaseErrorInformation` as a bare `String` (the
//! raw `SQLite` message), and its `constraint_name()` always returns `None`
//! — there is no `SQLite` equivalent of Postgres's named constraint.
//! `unique_violation_field` used to match purely on `constraint_name()`, so
//! every generated app's friendly-conflict handling for a `--unique` column
//! (`autumn generate scaffold ... field:String:unique`, or the teams starter's
//! pending-invitation index) was unreachable on `SQLite`: a real duplicate
//! insert reached the blanket `500` instead of the mapped message, even
//! though the mapping itself was correct and already worked on Postgres.
//!
//! `autumn/src/error.rs`'s unit tests cover the fallback with a fake
//! `DatabaseErrorInformation` (diesel's own `String` box can't be constructed
//! outside diesel's `SQLite` backend code). This file is the real-engine
//! proof: boot an in-memory `SQLite` pool through the same
//! [`autumn_web::db::create_pool`] path a generated app uses, create a
//! `UNIQUE` index the way `autumn generate`'s `schema_edit::unique_index_sql`
//! names one, insert a duplicate, and confirm `unique_violation_field`
//! resolves the *actual* diesel error `SQLite` hands back to the same
//! `(field, message)` pair it already resolves on Postgres.
//!
//! Only meaningful under `--features sqlite`; the file is `#![cfg(feature =
//! "sqlite")]` so a default `cargo test` compiles it to an empty (passing)
//! binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_unique_violation_field`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::error::{AutumnError, unique_violation_field};
use autumn_web::reexports::{diesel, diesel_async};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

// Named exactly the way `autumn generate`'s `schema_edit::unique_index_sql`
// names a `--unique` column's index — this is what `MAPPING` keys on for the
// Postgres match, and exactly what SQLite's own error message never repeats
// back (see the doc comment on `unique_violation_field`).
const MAPPING: &[(&str, &str, &str)] = &[(
    "idx_widgets_email_unique",
    "email",
    "has already been taken",
)];

async fn boot_pool_with_duplicate_ready() -> SqlitePool {
    let config = DatabaseConfig {
        url: Some("sqlite://file:unique_violation_field?mode=memory&cache=shared".to_string()),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    diesel::sql_query(
        "CREATE TABLE widgets (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             email TEXT NOT NULL\
         )",
    )
    .execute(&mut *conn)
    .await
    .expect("create widgets table");
    diesel::sql_query("CREATE UNIQUE INDEX idx_widgets_email_unique ON widgets (email)")
        .execute(&mut *conn)
        .await
        .expect("create unique index");
    diesel::sql_query("INSERT INTO widgets (email) VALUES ('taken@example.com')")
        .execute(&mut *conn)
        .await
        .expect("seed the row the duplicate below collides with");
    drop(conn);

    pool
}

#[tokio::test]
async fn duplicate_insert_resolves_through_unique_violation_field() {
    let pool = boot_pool_with_duplicate_ready().await;
    let mut conn = pool.get().await.expect("checkout a sqlite connection");

    let result = diesel::sql_query("INSERT INTO widgets (email) VALUES ('taken@example.com')")
        .execute(&mut *conn)
        .await;

    let diesel_err = result.expect_err("duplicate email must violate the unique index");
    assert!(
        matches!(
            diesel_err,
            diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::UniqueViolation,
                _
            )
        ),
        "expected a UniqueViolation, got: {diesel_err:?}"
    );

    let err: AutumnError = diesel_err.into();
    assert_eq!(
        unique_violation_field(&err, MAPPING),
        Some(("email", "has already been taken")),
        "a real SQLite unique violation must resolve through unique_violation_field \
         the same way a Postgres one already does — see issue #2698",
    );
}
