//! Integration tests for `examples/blog`.
//!
//! Demonstrates Autumn's first-party test surface (`TestApp`, `TestClient`,
//! `TestResponse`, `TestDb`) as described in `docs/guide/testing.md`.
//!
//! # Running
//!
//! ```text
//! cargo test -p blog                                              # smoke tests (instant)
//! cargo test -p blog -- --include-ignored --test-threads=1       # DB tests (needs Docker)
//! ```
//!
//! `--test-threads=1` is required when running multiple DB-backed ignored
//! tests: each test truncates the shared table in `setup_posts_table()`, so
//! concurrent execution would cause data races. With a single DB test this
//! flag is optional, but keeping it explicit avoids surprises as the suite grows.
//!
//! # What these tests cover
//!
//! | Test | Requirement |
//! |------|-------------|
//! | `autumn_middleware_adds_request_id` | routed request + autumn-specific assertion |
//! | `create_post_round_trip` | DB round-trip via TestDb testcontainer |

use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestDb};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};

// ── Inline schema (matches blog's migrations) ──────────────────────────────

diesel::table! {
    posts (id) {
        id -> Int8,
        title -> Text,
        slug -> Text,
        body -> Text,
        published -> Bool,
    }
}

// ── Local model types ──────────────────────────────────────────────────────

#[derive(Debug, Queryable, Selectable, Serialize)]
#[diesel(table_name = posts)]
struct Post {
    id: i64,
    title: String,
    slug: String,
    body: String,
    published: bool,
}

#[derive(Debug, Insertable, Deserialize)]
#[diesel(table_name = posts)]
struct NewPost {
    title: String,
    slug: String,
    body: String,
    #[serde(default)]
    published: bool,
}

// ── Blog-shaped route handlers (mirror routes/api.rs) ─────────────────────

/// Return all published posts as a JSON array.
#[get("/api/posts")]
async fn list_published(mut db: Db) -> AutumnResult<Json<Vec<Post>>> {
    let published = posts::table
        .filter(posts::published.eq(true))
        .select(Post::as_select())
        .load(&mut *db)
        .await?;
    Ok(Json(published))
}

/// Create a new blog post from a JSON body.
#[post("/api/posts")]
async fn create_post(mut db: Db, Json(body): Json<NewPost>) -> AutumnResult<Json<Post>> {
    let created = diesel::insert_into(posts::table)
        .values(&body)
        .returning(Post::as_returning())
        .get_result(&mut *db)
        .await?;
    Ok(Json(created))
}

// ── DB setup helper ────────────────────────────────────────────────────────

// TRUNCATE resets table state before each test. This is safe when DB tests
// run serially (`--test-threads=1`). Do not add concurrent DB tests without
// either that flag or per-test schema isolation.
async fn setup_posts_table()
-> diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection> {
    let db = TestDb::shared().await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS posts (
            id       BIGSERIAL PRIMARY KEY,
            title    TEXT    NOT NULL,
            slug     TEXT    NOT NULL DEFAULT '',
            body     TEXT    NOT NULL DEFAULT '',
            published BOOLEAN NOT NULL DEFAULT false
        )",
    )
    .await;
    db.execute_sql("TRUNCATE posts RESTART IDENTITY").await;
    db.pool()
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// **Routed request + autumn-specific assertion** — no Docker required.
///
/// `TestApp::new()` boots the full Autumn middleware pipeline in-process
/// (routes, exception filters, security middleware, `RequestIdLayer`, …)
/// without binding a TCP listener.
///
/// `X-Request-Id` is added by Autumn's `RequestIdLayer` on every response;
/// asserting its presence proves the complete middleware stack ran.
#[tokio::test]
async fn autumn_middleware_adds_request_id() {
    let client = TestApp::new()
        .routes(routes![list_published, create_post])
        .build();

    // Any route will do — we're proving the middleware stack ran.
    let resp = client.get("/api/posts").send().await;

    assert!(
        resp.header("x-request-id").is_some(),
        "Autumn's RequestIdLayer must attach X-Request-Id to every response"
    );
}

/// **DB round-trip** — requires Docker (testcontainers starts Postgres).
///
/// 1. Starts a shared Postgres container via `TestDb::shared()`.
/// 2. Creates the `posts` table and truncates it for test isolation.
/// 3. `POST /api/posts` inserts a row and returns the created post as JSON.
/// 4. `GET  /api/posts` confirms the row persisted (DB read-after-write).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn create_post_round_trip() {
    let pool = setup_posts_table().await;
    let client = TestApp::new()
        .routes(routes![list_published, create_post])
        .with_db(pool)
        .build();

    // Initially the list is empty.
    client
        .get("/api/posts")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("[]");

    // Create a post (DB write).
    let resp = client
        .post("/api/posts")
        .json(&serde_json::json!({
            "title": "Hello from Autumn tests",
            "slug":  "hello-autumn-tests",
            "body":  "Created inside an integration test.",
            "published": true
        }))
        .send()
        .await;

    // Autumn-specific: JSON responses include the correct Content-Type.
    resp.assert_ok()
        .assert_header_contains("content-type", "application/json")
        .assert_json::<serde_json::Value, _>(|post| {
            assert_eq!(post["title"], "Hello from Autumn tests");
            assert_eq!(post["published"], true);
            assert!(post["id"].as_i64().unwrap() > 0, "DB must assign a real ID");
        });

    // Verify the write persisted (DB read).
    client
        .get("/api/posts")
        .send()
        .await
        .assert_ok()
        .assert_json::<Vec<serde_json::Value>, _>(|posts| {
            assert_eq!(posts.len(), 1, "exactly one published post");
            assert_eq!(posts[0]["title"], "Hello from Autumn tests");
        });
}

// ── Factory tests ─────────────────────────────────────────────────────────────
//
// Uses `#[autumn_web::model]` to exercise the generated factory builder.
// Unique table name (`blog_factory_posts`) avoids conflicts with the `posts`
// table used above. In-memory tests run without Docker; DB tests are ignored.

mod factory_tests {
    autumn_web::reexports::diesel::table! {
        blog_factory_posts (id) {
            id -> Int8,
            title -> Text,
            slug -> Text,
            published -> Bool,
        }
    }

    #[autumn_web::model(table = "blog_factory_posts")]
    pub struct BlogFactoryPost {
        #[id]
        pub id: i64,
        pub title: String,
        pub slug: String,
        pub published: bool,
    }

    // ── In-memory tests (no Docker needed) ───────────────────────────────────

    #[test]
    fn factory_build_defaults_all_fields() {
        let p = BlogFactoryPost::factory().build();
        assert_eq!(p.title, "");
        assert_eq!(p.slug, "");
        assert!(!p.published);
    }

    #[test]
    fn factory_build_override_title() {
        let p = BlogFactoryPost::factory().title("Hello Autumn").build();
        assert_eq!(p.title, "Hello Autumn");
        assert_eq!(p.slug, ""); // untouched
    }

    #[test]
    fn factory_build_all_fields() {
        let p = BlogFactoryPost::factory()
            .title("All set")
            .slug("all-set")
            .published(true)
            .build();
        assert_eq!(p.title, "All set");
        assert_eq!(p.slug, "all-set");
        assert!(p.published);
    }

    #[test]
    fn factory_builds_are_independent() {
        let a = BlogFactoryPost::factory().title("Alpha").build();
        let b = BlogFactoryPost::factory().title("Beta").build();
        assert_eq!(a.title, "Alpha");
        assert_eq!(b.title, "Beta");
    }

    #[test]
    fn factory_build_returns_insertable_type() {
        let _: NewBlogFactoryPost = BlogFactoryPost::factory().build();
    }

    // ── DB test (requires Docker) ─────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn factory_create_persists_blog_post() {
        use autumn_web::test::TestDb;

        let db = TestDb::shared().await;
        db.execute_sql(
            "CREATE TABLE IF NOT EXISTS blog_factory_posts (
                id        BIGSERIAL PRIMARY KEY,
                title     TEXT NOT NULL DEFAULT '',
                slug      TEXT NOT NULL DEFAULT '',
                published BOOL NOT NULL DEFAULT false
            )",
        )
        .await;
        db.execute_sql("TRUNCATE blog_factory_posts RESTART IDENTITY")
            .await;

        let post = BlogFactoryPost::factory()
            .title("Factory post")
            .slug("factory-post")
            .published(true)
            .create(&db.pool())
            .await;

        assert!(post.id > 0);
        assert_eq!(post.title, "Factory post");
        assert_eq!(post.slug, "factory-post");
        assert!(post.published);
    }
}

// ── Soft-delete lifecycle tests ────────────────────────────────────────────
//
// Exercises the `#[repository(soft_delete)]` macro-generated methods:
// `delete_by_id` → sets `deleted_at`; `find_by_id` excludes soft-deleted rows;
// `restore` clears `deleted_at`; `purge` hard-deletes.
//
// These are in-memory macro-level tests — they verify code-gen correctness
// without a running database.

mod soft_delete_tests {
    // Verify the generated code compiles with soft_delete enabled.
    autumn_web::reexports::diesel::table! {
        sd_articles (id) {
            id -> Int8,
            title -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    #[autumn_web::model(table = "sd_articles")]
    pub struct SdArticle {
        #[id]
        pub id: i64,
        pub title: String,
        pub deleted_at: Option<autumn_web::time::civil::DateTime>,
    }

    #[autumn_web::repository(SdArticle, soft_delete)]
    pub trait SdArticleRepository {}

    #[test]
    fn soft_delete_repository_trait_is_generated() {
        // Trait and struct exist at compile time — if this compiles, the
        // macro expanded without errors.
        fn _assert_send_sync<T: Send + Sync>() {}
        _assert_send_sync::<PgSdArticleRepository>();
    }
}
