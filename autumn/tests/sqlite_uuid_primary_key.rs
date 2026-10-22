//! UUID primary keys on the `SQLite` runtime backend (issue #2555).
//!
//! `autumn generate model --id uuid` emits `TEXT PRIMARY KEY NOT NULL DEFAULT
//! (<v4 expression>)` for a `SQLite` app. `SQLite` has no `gen_random_uuid()`,
//! so the database builds the id from `randomblob`. These tests run that exact
//! DDL, the same one the generator writes, through a `#[model]` with a
//! `SqliteUuid` key. `#[repository]` is not used: it supports `i64` keys only,
//! on both backends.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_uuid_primary_key`.
#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::collections::HashSet;

use autumn_schema_core::{Backend, IdKind, SQLITE_UUID_V4_SQL};
use autumn_web::db::sqlite_types::SqliteUuid;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::test::TestDb;
use diesel::prelude::{OptionalExtension as _, QueryDsl as _, SelectableHelper as _};
use diesel_async::RunQueryDsl as _;

mod schema {
    autumn_web::reexports::diesel::table! {
        uuid_notes (id) {
            id -> Text,
            title -> Text,
        }
    }
}

use schema::uuid_notes;

#[autumn_web::model]
pub struct UuidNote {
    #[id]
    pub id: SqliteUuid,
    pub title: String,
}

async fn save(db: &TestDb, title: &str) -> UuidNote {
    let mut conn = db.pool().get().await.expect("connection");
    diesel::insert_into(uuid_notes::table)
        .values(&NewUuidNote {
            title: title.to_owned(),
        })
        .returning(UuidNote::as_returning())
        .get_result(&mut *conn)
        .await
        .expect("insert")
}

async fn find(db: &TestDb, id: SqliteUuid) -> Option<UuidNote> {
    let mut conn = db.pool().get().await.expect("connection");
    uuid_notes::table
        .find(id)
        .select(UuidNote::as_select())
        .first(&mut *conn)
        .await
        .optional()
        .expect("select")
}

#[derive(diesel::QueryableByName)]
struct Row {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
}

async fn db_with_table() -> TestDb {
    let db = TestDb::new().await;
    db.execute_sql(&format!(
        "CREATE TABLE uuid_notes (id {}, title TEXT NOT NULL)",
        IdKind::Uuid.pk_sql(Backend::Sqlite)
    ))
    .await;
    db
}

#[tokio::test]
async fn save_returns_a_database_generated_canonical_v4_id() {
    let db = db_with_table().await;
    let note = save(&db, "first").await;

    let text = note.id.to_string();
    let parsed = uuid::Uuid::parse_str(&text).expect("id parses as a UUID");
    assert_eq!(text, parsed.hyphenated().to_string(), "canonical lowercase");
    assert_eq!(parsed.get_version_num(), 4);
    assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122);
}

#[tokio::test]
async fn saved_rows_are_found_by_their_id() {
    let db = db_with_table().await;
    let note = save(&db, "findable").await;
    let found = find(&db, note.id).await.expect("row exists");

    assert_eq!(found.title, "findable");
    assert_eq!(found.id, note.id);
}

#[tokio::test]
async fn generated_ids_do_not_repeat() {
    let db = db_with_table().await;
    let mut seen = HashSet::new();
    for n in 0..200 {
        let note = save(&db, &format!("n{n}")).await;
        assert!(seen.insert(note.id.to_string()), "duplicate id");
    }
}

#[tokio::test]
async fn the_id_column_rejects_null() {
    let db = db_with_table().await;
    let mut conn = db.pool().get().await.expect("connection");
    let result = diesel::sql_query("INSERT INTO uuid_notes (id, title) VALUES (NULL, 'x')")
        .execute(&mut *conn)
        .await;
    assert!(result.is_err(), "a NULL id must be rejected");
}

#[tokio::test]
async fn every_variant_nibble_is_valid_over_many_rows() {
    // The variant nibble is random in `8,9,a,b`. 2000 rows cover all four.
    let db = db_with_table().await;
    db.execute_sql(
        "WITH RECURSIVE g(g) AS (SELECT 1 UNION ALL SELECT g + 1 FROM g WHERE g < 2000) \
         INSERT INTO uuid_notes (title) SELECT 'x' FROM g",
    )
    .await;

    let mut conn = db.pool().get().await.expect("connection");
    let rows: Vec<Row> = diesel::sql_query("SELECT id FROM uuid_notes")
        .load(&mut *conn)
        .await
        .expect("select ids");
    let mut variants = HashSet::new();
    let mut ids = HashSet::new();
    for row in &rows {
        let parsed = uuid::Uuid::parse_str(&row.id).expect("parses");
        assert_eq!(row.id, parsed.hyphenated().to_string());
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122);
        variants.insert(row.id.as_bytes()[19]);
        ids.insert(row.id.clone());
    }
    assert_eq!(rows.len(), 2000);
    assert_eq!(ids.len(), 2000, "ids are unique");
    assert_eq!(variants.len(), 4, "all of 8, 9, a, b occur: {variants:?}");
}

#[test]
fn the_default_expression_is_shared_with_the_generator() {
    assert!(
        IdKind::Uuid
            .pk_sql(Backend::Sqlite)
            .contains(SQLITE_UUID_V4_SQL)
    );
}
