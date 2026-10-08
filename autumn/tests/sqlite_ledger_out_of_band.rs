//! A ledgered table must not be mutated by a framework write path that records
//! no revision (issue #2319).
//!
//! Counter-cache upkeep and `dependent(.., delete_all | nullify)` cascades
//! issue raw SQL against a table. If that table is ledgered, the SQL erases or
//! changes state with no revision. Each path must refuse with a typed error and
//! leave the table and its chain unchanged.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_ledger_out_of_band`.
#![cfg(feature = "sqlite")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::ledger::LedgerError;
use autumn_web::reexports::{chrono, diesel, diesel_async};
use diesel_async::SimpleAsyncConnection as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        lgo_posts (id) {
            id -> Int8,
            title -> Text,
            comment_count -> Int8,
            deleted_at -> Nullable<Timestamp>,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_comments (id) {
            id -> Int8,
            post_id -> Int8,
            body -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_plain_posts (id) {
            id -> Int8,
            title -> Text,
            comment_count -> Int8,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_plain_comments (id) {
            id -> Int8,
            post_id -> Int8,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_del_parents (id) {
            id -> Int8,
            name -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_del_children (id) {
            id -> Int8,
            parent_id -> Int8,
            deleted_at -> Nullable<Timestamp>,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_nul_parents (id) {
            id -> Int8,
            name -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_nul_children (id) {
            id -> Int8,
            parent_id -> Nullable<Int8>,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::{
    lgo_comments, lgo_del_children, lgo_del_parents, lgo_nul_children, lgo_nul_parents,
    lgo_plain_comments, lgo_plain_posts, lgo_posts,
};

// ── counter cache on a ledgered parent ───────────────────────────────

#[autumn_web::model(table = "lgo_posts")]
pub struct LgoPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(LgoPost, table = "lgo_posts", soft_delete, ledgered = true)]
pub trait LgoPostRepository {}

#[autumn_web::model(table = "lgo_comments")]
#[belongs_to(LgoPost, fk = post_id, counter_cache)]
pub struct LgoComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
}

#[autumn_web::repository(LgoComment, table = "lgo_comments")]
pub trait LgoCommentRepository {}

// ── the same paths on tables that are not ledgered ──────────────────

#[autumn_web::model(table = "lgo_plain_posts")]
pub struct LgoPlainPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(LgoPlainPost, table = "lgo_plain_posts")]
pub trait LgoPlainPostRepository {}

#[autumn_web::model(table = "lgo_plain_comments")]
#[belongs_to(LgoPlainPost, fk = post_id, counter_cache = "comment_count")]
pub struct LgoPlainComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

#[autumn_web::repository(LgoPlainComment, table = "lgo_plain_comments")]
pub trait LgoPlainCommentRepository {}

// ── delete_all cascade into a ledgered child ─────────────────────────

#[autumn_web::model(table = "lgo_del_children")]
pub struct LgoDelChild {
    #[id]
    pub id: i64,
    pub parent_id: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(LgoDelChild, table = "lgo_del_children", soft_delete, ledgered = true)]
pub trait LgoDelChildRepository {}

#[autumn_web::model(table = "lgo_del_parents")]
pub struct LgoDelParent {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(
    LgoDelParent,
    table = "lgo_del_parents",
    dependent(PgLgoDelChildRepository, fk = "parent_id", on_delete = delete_all)
)]
pub trait LgoDelParentRepository {}

// ── nullify cascade into a ledgered child ────────────────────────────

#[autumn_web::model(table = "lgo_nul_children")]
pub struct LgoNulChild {
    #[id]
    pub id: i64,
    pub parent_id: Option<i64>,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(LgoNulChild, table = "lgo_nul_children", soft_delete, ledgered = true)]
pub trait LgoNulChildRepository {}

#[autumn_web::model(table = "lgo_nul_parents")]
pub struct LgoNulParent {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(
    LgoNulParent,
    table = "lgo_nul_parents",
    dependent(PgLgoNulChildRepository, fk = "parent_id", on_delete = nullify)
)]
pub trait LgoNulParentRepository {}

// ── harness ──────────────────────────────────────────────────────────

const LEDGER_UP: &str = include_str!(
    "../version_history_migrations_sqlite/20260826000000_create_ledger_revisions/up.sql"
);
const LEDGER_HIGH_WATER_UP: &str = include_str!(
    "../version_history_migrations_sqlite/20260901213107_create_ledger_high_water/up.sql"
);
const VERSION_HISTORY_UP: &str = include_str!(
    "../version_history_migrations_sqlite/20260526000000_create_version_history/up.sql"
);

async fn boot_pool(db_name: &str) -> SqlitePool {
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");
    let mut conn = pool.get().await.expect("checkout a connection");
    for ddl in [
        "CREATE TABLE lgo_posts (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, \
         comment_count BIGINT NOT NULL DEFAULT 0, deleted_at TIMESTAMP)",
        "CREATE TABLE lgo_comments (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         post_id BIGINT NOT NULL, body TEXT NOT NULL)",
        "CREATE TABLE lgo_plain_posts (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         title TEXT NOT NULL, comment_count BIGINT NOT NULL DEFAULT 0)",
        "CREATE TABLE lgo_plain_comments (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         post_id BIGINT NOT NULL)",
        "CREATE TABLE lgo_del_parents (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
        "CREATE TABLE lgo_del_children (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         parent_id BIGINT NOT NULL, deleted_at TIMESTAMP)",
        "CREATE TABLE lgo_nul_parents (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
        "CREATE TABLE lgo_nul_children (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         parent_id BIGINT, deleted_at TIMESTAMP)",
        VERSION_HISTORY_UP,
        LEDGER_UP,
        LEDGER_HIGH_WATER_UP,
    ] {
        conn.batch_execute(ddl)
            .await
            .unwrap_or_else(|err| panic!("apply DDL: {err}\n{ddl}"));
    }
    drop(conn);
    pool
}

fn out_of_band(err: &autumn_web::AutumnError) -> (&str, &str) {
    match err
        .downcast_chain_ref::<LedgerError>()
        .expect("the refusal is a typed LedgerError")
    {
        LedgerError::OutOfBandWrite { table, path } => (table, path),
        other => panic!("expected OutOfBandWrite, got {other:?}"),
    }
}

// ── tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_counter_cache_bump_on_a_ledgered_parent_is_refused() {
    let pool = boot_pool("lgo_counter").await;
    let posts = PgLgoPostRepository::with_pool_untracked(pool.clone());
    let comments = PgLgoCommentRepository::with_pool_untracked(pool);

    let post = posts
        .save(&NewLgoPost {
            title: "p".to_string(),
        })
        .await
        .expect("insert parent");

    let err = comments
        .save(&NewLgoComment {
            post_id: post.id,
            body: "c".to_string(),
        })
        .await
        .expect_err("a counter bump must not move a ledgered row unrecorded");
    let (table, path) = out_of_band(&err);
    assert_eq!(table, "lgo_posts");
    assert_eq!(path, "counter cache");

    let live = posts.find_by_id(post.id).await.unwrap().unwrap();
    assert_eq!(live.comment_count, 0, "the refused bump must not land");
    assert!(
        comments.find_all().await.expect("list").is_empty(),
        "the refused write rolls the child insert back"
    );
    let report = posts.ledger_verify(post.id).await.expect("verify");
    assert!(report.broken.is_none(), "no drift: {:?}", report.broken);
}

#[tokio::test]
async fn a_delete_all_cascade_into_a_ledgered_child_is_refused() {
    let pool = boot_pool("lgo_delete_all").await;
    let parents = PgLgoDelParentRepository::with_pool_untracked(pool.clone());
    let children = PgLgoDelChildRepository::with_pool_untracked(pool);

    let parent = parents
        .save(&NewLgoDelParent {
            name: "p".to_string(),
        })
        .await
        .expect("insert parent");
    let child = children
        .save(&NewLgoDelChild {
            parent_id: parent.id,
        })
        .await
        .expect("insert child");

    let err = parents
        .delete_by_id(parent.id)
        .await
        .expect_err("delete_all must not erase a ledgered row");
    let (table, path) = out_of_band(&err);
    assert_eq!(table, "lgo_del_children");
    assert_eq!(path, "dependent delete_all");

    assert!(children.find_by_id(child.id).await.unwrap().is_some());
    assert!(parents.find_by_id(parent.id).await.unwrap().is_some());
    let report = children.ledger_verify(child.id).await.expect("verify");
    assert!(report.broken.is_none(), "{:?}", report.broken);
}

#[tokio::test]
async fn a_nullify_cascade_into_a_ledgered_child_is_refused() {
    let pool = boot_pool("lgo_nullify").await;
    let parents = PgLgoNulParentRepository::with_pool_untracked(pool.clone());
    let children = PgLgoNulChildRepository::with_pool_untracked(pool);

    let parent = parents
        .save(&NewLgoNulParent {
            name: "p".to_string(),
        })
        .await
        .expect("insert parent");
    let child = children
        .save(&NewLgoNulChild {
            parent_id: Some(parent.id),
        })
        .await
        .expect("insert child");

    let err = parents
        .delete_by_id(parent.id)
        .await
        .expect_err("nullify must not change a ledgered row unrecorded");
    let (table, path) = out_of_band(&err);
    assert_eq!(table, "lgo_nul_children");
    assert_eq!(path, "dependent nullify");

    let live = children.find_by_id(child.id).await.unwrap().unwrap();
    assert_eq!(live.parent_id, Some(parent.id));
    let report = children.ledger_verify(child.id).await.expect("verify");
    assert!(report.broken.is_none(), "{:?}", report.broken);
}

/// The same paths keep working on tables that are not ledgered.
#[tokio::test]
async fn the_guard_does_not_touch_unledgered_tables() {
    assert!(!autumn_web::ledger::is_ledgered_table("lgo_plain_posts"));
    assert!(autumn_web::ledger::is_ledgered_table("lgo_posts"));

    let pool = boot_pool("lgo_plain").await;
    let posts = PgLgoPlainPostRepository::with_pool_untracked(pool.clone());
    let comments = PgLgoPlainCommentRepository::with_pool_untracked(pool);
    let post = posts
        .save(&NewLgoPlainPost {
            title: "p".to_string(),
        })
        .await
        .expect("insert parent");
    comments
        .save(&NewLgoPlainComment { post_id: post.id })
        .await
        .expect("an unledgered counter bump still works");
    let live = posts.find_by_id(post.id).await.unwrap().unwrap();
    assert_eq!(live.comment_count, 1);
}
