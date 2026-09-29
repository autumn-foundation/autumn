//! `dependent(..., on_delete = destroy)` cascade proof on the `SQLite` runtime
//! backend (issue #1996 / PR #2021).
//!
//! The Destroy arm of the generated `__autumn_apply_dependent_on_conn` helper
//! selects the child ids to cascade with a **hand-built SQL string** (not the
//! diesel DSL the `maybe_for_update!` seam covers). That string used to emit a
//! trailing `FOR UPDATE`, which `SQLite` rejects outright — so `delete_by_id`
//! (and `delete_many`) on a parent that has a `destroy`-cascade dependent failed
//! at run time on `SQLite`. The fix gates the lock clause with `backend_select!`
//! (`" FOR UPDATE"` on Postgres, `""` on `SQLite`, whose single-writer database
//! lock already serializes the cascade).
//!
//! This test proves the runtime behaviour on `SQLite`: a parent repository with
//! a `destroy` child, seeded with a parent that has children, whose
//! `delete_by_id` (a) succeeds (no `FOR UPDATE` syntax error) and (b) actually
//! deletes the dependent child rows in the same transaction, leaving no orphans.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_dependent_destroy`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel::QueryableByName;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        casc_posts (id) {
            id -> Int8,
            title -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        casc_comments (id) {
            id -> Int8,
            post_id -> Int8,
            body -> Text,
        }
    }
}

use schema::{casc_comments, casc_posts};

#[autumn_web::model(table = "casc_posts")]
pub struct CascPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::model(table = "casc_comments")]
pub struct CascComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
}

// Child repository — the `destroy` cascade instantiates `PgCascCommentRepository`
// internally and hard-deletes its rows.
#[autumn_web::repository(CascComment, table = "casc_comments")]
pub trait CascCommentRepository {}

// Parent repository with a `destroy`-cascade dependent on the child. Deleting a
// parent must destroy its comments in the same transaction.
#[autumn_web::repository(
    CascPost,
    table = "casc_posts",
    dependent(PgCascCommentRepository, fk = "post_id", on_delete = destroy)
)]
pub trait CascPostRepository {}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = autumn_web::reexports::diesel::sql_types::BigInt)]
    n: i64,
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    diesel::sql_query(sql)
        .get_result::<CountRow>(&mut *conn)
        .await
        .expect("count query")
        .n
}

async fn boot_pool() -> SqlitePool {
    let config = DatabaseConfig {
        url: Some("sqlite://file:casc_destroy?mode=memory&cache=shared".to_string()),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query(
            "CREATE TABLE casc_posts (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 title TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create casc_posts table");
        diesel::sql_query(
            "CREATE TABLE casc_comments (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 post_id BIGINT NOT NULL REFERENCES casc_posts(id), \
                 body TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create casc_comments table");
    }

    pool
}

#[tokio::test]
async fn deleting_parent_destroys_dependent_children_on_sqlite() {
    let pool = boot_pool().await;

    // Seed one post with three comments plus a second, unrelated post + comment
    // so the cascade is proven to scope to the deleted parent only.
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query("INSERT INTO casc_posts (id, title) VALUES (1, 'hello'), (2, 'other')")
            .execute(&mut *conn)
            .await
            .expect("seed posts");
        diesel::sql_query(
            "INSERT INTO casc_comments (id, post_id, body) VALUES \
                 (1, 1, 'a'), (2, 1, 'b'), (3, 1, 'c'), (4, 2, 'keep')",
        )
        .execute(&mut *conn)
        .await
        .expect("seed comments");
    }

    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) AS n FROM casc_comments WHERE post_id = 1"
        )
        .await,
        3
    );

    let repo = PgCascPostRepository::with_pool_untracked(pool.clone());

    // The load-bearing assertion: this succeeds on SQLite (the child-id SELECT no
    // longer emits `FOR UPDATE`, which SQLite would reject) instead of erroring.
    repo.delete_by_id(1)
        .await
        .expect("destroy cascade must succeed on SQLite (no FOR UPDATE syntax error)");

    // Parent gone.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS n FROM casc_posts WHERE id = 1").await,
        0
    );
    // Its dependent children were destroyed in the same transaction — no orphans.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) AS n FROM casc_comments WHERE post_id = 1"
        )
        .await,
        0
    );
    // The unrelated post and its comment are untouched.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS n FROM casc_posts WHERE id = 2").await,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) AS n FROM casc_comments WHERE post_id = 2"
        )
        .await,
        1
    );
}
