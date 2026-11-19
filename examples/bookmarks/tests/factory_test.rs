//! End-to-end factory tests for the bookmarks example.
//!
//! Demonstrates that `#[model]` factories reduce test-fixture boilerplate:
//! building a bookmark with intent-only overrides takes one line, not six.
//!
//! # Running
//!
//! ```text
//! cargo test -p bookmarks                                      # smoke tests (instant)
//! cargo test -p bookmarks -- --include-ignored                 # DB tests (needs Docker)
//! ```

use autumn_web::prelude::*;
use autumn_web::test::TestApp;

// ── Inline schema (mirrors src/schema.rs) ─────────────────────────────────────

diesel::table! {
    bookmarks (id) {
        id -> Int8,
        url -> Text,
        title -> Text,
        tag -> Text,
        alive -> Bool,
        created_at -> Timestamp,
    }
}

// ── Model defined with #[model] to exercise the generated factory ─────────────

#[autumn_web::model]
pub struct Bookmark {
    #[id]
    pub id: i64,
    #[validate(url)]
    pub url: String,
    #[validate(length(min = 1, max = 200))]
    pub title: String,
    pub tag: String,
    #[default]
    pub alive: bool,
    #[default]
    pub created_at: autumn_web::time::civil::DateTime,
}

// ── In-memory build tests (no Docker needed) ──────────────────────────────────

#[test]
fn bookmark_factory_zero_args() {
    // All factory fields start at their type default.
    let draft = Bookmark::factory().build();
    assert_eq!(draft.url, "");
    assert_eq!(draft.title, "");
    assert_eq!(draft.tag, "");
}

#[test]
fn bookmark_factory_override_url() {
    let draft = Bookmark::factory().url("https://rust-lang.org").build();
    assert_eq!(draft.url, "https://rust-lang.org");
    assert_eq!(draft.title, ""); // untouched
}

#[test]
fn bookmark_factory_override_all_fields() {
    let draft = Bookmark::factory()
        .url("https://docs.rs")
        .title("docs.rs — Rust API docs")
        .tag("rust")
        .build();

    assert_eq!(draft.url, "https://docs.rs");
    assert_eq!(draft.title, "docs.rs — Rust API docs");
    assert_eq!(draft.tag, "rust");
}

#[test]
fn bookmark_factory_builds_are_independent() {
    let a = Bookmark::factory().url("https://a.test").build();
    let b = Bookmark::factory().url("https://b.test").build();
    assert_eq!(a.url, "https://a.test");
    assert_eq!(b.url, "https://b.test");
}

/// Demonstrates the factory composition pattern: one model's factory id is
/// passed into another's field setter to express an association.
///
/// In a real multi-model scenario you would call `.create(&pool)` on the
/// parent first, then pass its `id` to the child's field setter.
#[test]
fn bookmark_factory_composition_pattern() {
    // Simulated: imagine 'categories' is a parent model.
    // Here we use `tag` as a stand-in for a FK value supplied externally.
    let rust_tag = "rust".to_string();

    let draft = Bookmark::factory()
        .url("https://crates.io")
        .tag(rust_tag.clone())
        .build();

    assert_eq!(draft.tag, rust_tag);
}

// ── DB + route round-trip (requires Docker) ───────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn bookmark_factory_create_persists() {
    use autumn_web::test::TestDb;

    let db = TestDb::shared().await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS bookmarks (
            id         BIGSERIAL PRIMARY KEY,
            url        TEXT NOT NULL DEFAULT '',
            title      TEXT NOT NULL DEFAULT '',
            tag        TEXT NOT NULL DEFAULT '',
            alive      BOOLEAN NOT NULL DEFAULT true,
            created_at TIMESTAMP NOT NULL DEFAULT now()
        )",
    )
    .await;
    db.execute_sql("TRUNCATE bookmarks RESTART IDENTITY").await;

    let bm = Bookmark::factory()
        .url("https://crates.io")
        .title("crates.io — The Rust package registry")
        .tag("rust")
        .create(&db.pool())
        .await;

    assert!(bm.id > 0, "id must be populated by the database");
    assert_eq!(bm.url, "https://crates.io");
    assert_eq!(bm.title, "crates.io — The Rust package registry");
    assert_eq!(bm.tag, "rust");
}

/// Full round-trip: factory → create → route handler → JSON assertion.
///
/// Seed data with the factory (3 lines of intent), then verify it
/// through the API endpoint — proving the factory integrates with
/// the standard Autumn test harness.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn bookmark_factory_route_roundtrip() {
    use autumn_web::test::TestDb;
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use serde::Serialize;

    let db = TestDb::shared().await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS bookmarks (
            id         BIGSERIAL PRIMARY KEY,
            url        TEXT NOT NULL DEFAULT '',
            title      TEXT NOT NULL DEFAULT '',
            tag        TEXT NOT NULL DEFAULT '',
            alive      BOOLEAN NOT NULL DEFAULT true,
            created_at TIMESTAMP NOT NULL DEFAULT now()
        )",
    )
    .await;
    db.execute_sql("TRUNCATE bookmarks RESTART IDENTITY").await;

    // Factory-based seeding: intent-only, no struct-literal boilerplate.
    let rust_bm = Bookmark::factory()
        .url("https://rust-lang.org")
        .title("The Rust Programming Language")
        .tag("rust")
        .create(&db.pool())
        .await;

    let docs_bm = Bookmark::factory()
        .url("https://docs.rs")
        .title("docs.rs")
        .tag("rust")
        .create(&db.pool())
        .await;

    assert_ne!(rust_bm.id, docs_bm.id);

    // Route handler that lists bookmarks by tag using the pool
    #[derive(Queryable, Selectable, Serialize, Debug)]
    #[diesel(table_name = bookmarks)]
    struct BookmarkRow {
        id: i64,
        url: String,
        title: String,
        tag: String,
        alive: bool,
        #[diesel(deserialize_as = autumn_web::db::SqlDateTime)]
        created_at: autumn_web::time::civil::DateTime,
    }

    #[get("/bookmarks")]
    async fn list_all(mut db: Db) -> AutumnResult<Json<Vec<BookmarkRow>>> {
        let rows = bookmarks::table
            .select(BookmarkRow::as_select())
            .load(&mut *db)
            .await?;
        Ok(Json(rows))
    }

    let client = TestApp::new()
        .routes(routes![list_all])
        .with_db(db.pool())
        .build();

    client
        .get("/bookmarks")
        .send()
        .await
        .assert_ok()
        .assert_json::<Vec<serde_json::Value>, _>(|items| {
            assert_eq!(items.len(), 2);
            let titles: Vec<&str> = items.iter().map(|b| b["title"].as_str().unwrap()).collect();
            assert!(titles.contains(&"The Rust Programming Language"));
            assert!(titles.contains(&"docs.rs"));
        });
}
