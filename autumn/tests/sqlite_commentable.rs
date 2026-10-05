//! `#[commentable]` hard-delete on the `SQLite` runtime backend (issue #2275
//! / PR #2983 follow-up, Snag charter).
//!
//! `docs/guide/commentable.md` states plainly: "Both backends are
//! supported." But the feature's only live-database suite,
//! `autumn/tests/integration/commentable.rs`, is written directly against
//! `AsyncPgConnection` + `testcontainers_modules::postgres::Postgres` — every
//! `#[ignore]`d test in it needs Docker and none of it runs under the
//! `sqlite` feature flip. `.github/workflows/ci.yml`'s `sqlite-runtime` job
//! names ~30 `sqlite_*` test targets explicitly and none of them mention
//! `comment`/`commentable`. So `#[commentable]`'s hard-delete path — which
//! just gained a new cross-record-cascade refusal in #2983 — has never
//! actually run against the second backend the docs say it supports.
//!
//! This proves the #2275 refusal, and the plain subtree-removal path it
//! guards, against a real SQLite connection: no Docker, in-memory database.
//!
//! It also proves that a copy of a registered spec finds the repository of
//! its model (issue #2286).
//!
//! Only meaningful under `--features sqlite` (same convention as
//! `sqlite_dependent_destroy.rs`): `cargo test -p autumn-web --features
//! sqlite --test sqlite_commentable`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel::QueryableByName;
use diesel::sql_types::BigInt;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        sqc_users (id) {
            id -> Int8,
            name -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        sqc_hards (id) {
            id -> Int8,
            title -> Text,
            comment_count -> Int8,
        }
    }

    autumn_web::reexports::diesel::table! {
        sqc_audits (id) {
            id -> Int8,
            title -> Text,
            comment_count -> Int8,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::{sqc_audits, sqc_hards, sqc_users};

#[autumn_web::model(table = "sqc_users")]
pub struct SqcUser {
    #[id]
    pub id: i64,
    pub name: String,
}

// `soft_delete = false` shape: a SECOND, dedicated comments table with no
// `deleted_at` and a cascading self-FK — same fixture shape as the
// Postgres-only `CmtHard`/`cmt_hard_comments` pair in
// `autumn/tests/integration/commentable.rs`.
#[autumn_web::model(table = "sqc_hards")]
#[commentable(by = SqcUser, table = sqc_hard_comments, soft_delete = false)]
pub struct SqcHard {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(SqcHard, table = "sqc_hards")]
pub trait SqcHardRepository {}

// Issue #2286: `deleted_at` is audit data here. The repository does not opt
// into `soft_delete`, so the column must not hide the row.
#[autumn_web::model(table = "sqc_audits")]
#[commentable(by = SqcUser, table = sqc_audit_comments)]
pub struct SqcAudit {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(SqcAudit, table = "sqc_audits")]
pub trait SqcAuditRepository {}

// Issue #2286: two models with equal specs. A copy cannot tell them apart.
// Only the registry lookup uses them, so `sqc_twin_comments` has no table.
mod twins {
    use super::SqcUser;
    use super::schema::sqc_audits;

    #[autumn_web::model(table = "sqc_audits")]
    #[commentable(by = SqcUser, table = sqc_twin_comments)]
    pub struct SqcTwinA {
        #[id]
        pub id: i64,
        pub title: String,
        #[default]
        pub comment_count: i64,
        pub deleted_at: Option<chrono::NaiveDateTime>,
    }

    #[autumn_web::model(table = "sqc_audits")]
    #[commentable(by = SqcUser, table = sqc_twin_comments)]
    pub struct SqcTwinB {
        #[id]
        pub id: i64,
        pub title: String,
        #[default]
        pub comment_count: i64,
        pub deleted_at: Option<chrono::NaiveDateTime>,
    }
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
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

async fn boot_pool(db_name: &str) -> SqlitePool {
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    for stmt in [
        "CREATE TABLE sqc_users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
        "CREATE TABLE sqc_hards (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             title TEXT NOT NULL, \
             comment_count BIGINT NOT NULL DEFAULT 0\
         )",
        "CREATE TABLE sqc_hard_comments (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             commentable_type TEXT NOT NULL, \
             commentable_id BIGINT NOT NULL, \
             parent_id BIGINT REFERENCES sqc_hard_comments(id) ON DELETE CASCADE, \
             author_id BIGINT NOT NULL REFERENCES sqc_users(id), \
             body TEXT NOT NULL, \
             created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\
         )",
        "CREATE INDEX idx_sqc_hard_comments_target ON sqc_hard_comments (commentable_type, commentable_id)",
        "CREATE INDEX idx_sqc_hard_comments_parent ON sqc_hard_comments (parent_id)",
        "CREATE TABLE sqc_audits (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             title TEXT NOT NULL, \
             comment_count BIGINT NOT NULL DEFAULT 0, \
             deleted_at TIMESTAMP\
         )",
        "CREATE TABLE sqc_audit_comments (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             commentable_type TEXT NOT NULL, \
             commentable_id BIGINT NOT NULL, \
             parent_id BIGINT REFERENCES sqc_audit_comments(id), \
             author_id BIGINT NOT NULL REFERENCES sqc_users(id), \
             body TEXT NOT NULL, \
             created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             deleted_at TIMESTAMP\
         )",
    ] {
        diesel::sql_query(stmt)
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("DDL failed: {stmt}: {e}"));
    }
    drop(conn);
    pool
}

async fn seed_user(pool: &SqlitePool, name: &str) -> i64 {
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query("INSERT INTO sqc_users (name) VALUES ($1)")
            .bind::<diesel::sql_types::Text, _>(name)
            .execute(&mut *conn)
            .await
            .expect("seed user");
    }
    count(
        pool,
        "SELECT id AS n FROM sqc_users ORDER BY id DESC LIMIT 1",
    )
    .await
}

async fn seed_hard(pool: &SqlitePool, title: &str) -> i64 {
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query("INSERT INTO sqc_hards (title) VALUES ($1)")
            .bind::<diesel::sql_types::Text, _>(title)
            .execute(&mut *conn)
            .await
            .expect("seed target");
    }
    count(
        pool,
        "SELECT id AS n FROM sqc_hards ORDER BY id DESC LIMIT 1",
    )
    .await
}

async fn counter(pool: &SqlitePool, id: i64) -> i64 {
    count(
        pool,
        &format!("SELECT comment_count AS n FROM sqc_hards WHERE id = {id}"),
    )
    .await
}

/// The plain path (no escape): a hard delete removes a whole subtree and the
/// counter reflects however many rows the delete actually held — the SQLite
/// analogue of `a_hard_delete_comments_table_removes_the_subtree_outright`.
#[tokio::test]
async fn a_hard_delete_removes_the_subtree_outright_on_sqlite() {
    let pool = boot_pool("sqc_commentable_subtree").await;
    let repo = PgSqcHardRepository::with_pool_untracked(pool.clone());
    let author = seed_user(&pool, "ada").await;
    let target = seed_hard(&pool, "t").await;

    let root = repo
        .add_comment(target, author, "a", None)
        .await
        .expect("root");
    let reply = repo
        .add_comment(target, author, "a1", Some(root.id))
        .await
        .expect("reply");
    repo.add_comment(target, author, "a1x", Some(reply.id))
        .await
        .expect("grandchild");
    assert_eq!(counter(&pool, target).await, 3);

    let removed = repo
        .delete_comment(target, root.id)
        .await
        .expect("delete succeeds on sqlite");
    assert_eq!(removed, 3, "the whole subtree, on sqlite too");
    assert_eq!(counter(&pool, target).await, 0);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS n FROM sqc_hard_comments").await,
        0,
        "hard delete leaves nothing behind on sqlite"
    );
}

/// Issue #2275 on SQLite: a hard delete must refuse a subtree with a reply
/// grafted onto another record, not just on Postgres. Mirrors
/// `a_hard_delete_refuses_a_subtree_with_a_reply_on_another_record` from the
/// Docker-only Postgres suite, byte-for-byte in scenario shape.
#[tokio::test]
async fn a_hard_delete_refuses_a_cross_record_graft_on_sqlite() {
    let pool = boot_pool("sqc_commentable_graft").await;
    let repo = PgSqcHardRepository::with_pool_untracked(pool.clone());
    let author = seed_user(&pool, "ada").await;
    let mine = seed_hard(&pool, "a").await;
    let other = seed_hard(&pool, "b").await;

    let root = repo
        .add_comment(mine, author, "a", None)
        .await
        .expect("root");
    let reply = repo
        .add_comment(mine, author, "a1", Some(root.id))
        .await
        .expect("reply");
    let foreign = repo
        .add_comment(other, author, "b", None)
        .await
        .expect("foreign");
    repo.add_comment(other, author, "b1", Some(foreign.id))
        .await
        .expect("foreign reply");

    // The framework cannot write this edge; raw SQL (an import, hand-edited
    // data) can.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query("UPDATE sqc_hard_comments SET parent_id = $1 WHERE id = $2")
            .bind::<BigInt, _>(reply.id)
            .bind::<BigInt, _>(foreign.id)
            .execute(&mut *conn)
            .await
            .expect("graft across records");
    }

    let err = repo
        .delete_comment(mine, root.id)
        .await
        .expect_err("the cascade would cross into another record, even on sqlite");
    assert_eq!(err.status().as_u16(), 422, "{err}");
    let message = err.to_string();
    assert!(message.contains("not on this record"), "{message}");
    assert!(
        message.contains(&format!("reply {} ", foreign.id)),
        "{message}"
    );

    assert_eq!(
        counter(&pool, mine).await,
        2,
        "refused delete: mine untouched"
    );
    assert_eq!(
        counter(&pool, other).await,
        2,
        "refused delete: other untouched"
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) AS n FROM sqc_hard_comments").await,
        4,
        "a refused delete removes nothing on sqlite"
    );

    // Repair the edge as the 422 message says. The delete then succeeds.
    {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query("UPDATE sqc_hard_comments SET parent_id = NULL WHERE id = $1")
            .bind::<BigInt, _>(foreign.id)
            .execute(&mut *conn)
            .await
            .expect("repair the edge");
    }
    assert_eq!(
        repo.delete_comment(mine, root.id)
            .await
            .expect("delete after repair"),
        2
    );
    assert_eq!(counter(&pool, mine).await, 0);
    assert_eq!(counter(&pool, other).await, 2);
}

/// Issue #2286: a copy of the registered spec has a new address. The
/// registry lookup must still find the model.
#[test]
fn a_copied_spec_resolves_to_its_registered_model() {
    let copy = *SqcAudit::commentable_spec();
    assert!(
        !std::ptr::eq(&copy, SqcAudit::commentable_spec()),
        "the copy must have its own address"
    );
    assert_eq!(
        autumn_web::commentable::commentable_model_for_spec(&copy),
        Some(std::any::type_name::<SqcAudit>()),
    );
}

/// Issue #2286: when two models register equal specs, a copy is ambiguous
/// and matches no model. Each registered reference still matches its own.
#[test]
fn a_copy_of_a_shared_spec_matches_no_model() {
    use autumn_web::commentable::commentable_model_for_spec;
    use twins::{SqcTwinA, SqcTwinB};

    let copy = *SqcTwinA::commentable_spec();
    assert_eq!(&copy, SqcTwinB::commentable_spec());
    assert_eq!(
        commentable_model_for_spec(&copy),
        None,
        "a copy must not pick one of two models"
    );
    assert_eq!(
        commentable_model_for_spec(SqcTwinA::commentable_spec()),
        Some(std::any::type_name::<SqcTwinA>()),
    );
    assert_eq!(
        commentable_model_for_spec(SqcTwinB::commentable_spec()),
        Some(std::any::type_name::<SqcTwinB>()),
    );
}

/// Issue #2286: every public helper must accept a copy of the spec. An
/// audit `deleted_at` must not hide the parent, because the repository does
/// not opt into `soft_delete`.
#[tokio::test]
async fn a_copied_spec_keeps_the_repository_soft_delete_rule_on_sqlite() {
    use autumn_web::commentable::{
        add_comment, comment_thread, delete_comment, recompute_comment_count,
    };

    let pool = boot_pool("sqc_commentable_copied_spec").await;
    let author = seed_user(&pool, "ada").await;
    let target = {
        let mut conn = pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO sqc_audits (title, deleted_at) VALUES ('t', CURRENT_TIMESTAMP)",
        )
        .execute(&mut *conn)
        .await
        .expect("seed an audited parent");
        drop(conn);
        count(
            &pool,
            "SELECT id AS n FROM sqc_audits ORDER BY id DESC LIMIT 1",
        )
        .await
    };

    let spec = *SqcAudit::commentable_spec();
    let kind = SqcAudit::COMMENTABLE_TYPE;
    let mut conn = pool.get().await.expect("conn");

    let comment = add_comment(&mut *conn, &spec, kind, target, author, "hi", None, None)
        .await
        .expect("add_comment: an audit deleted_at must not hide the parent");
    let thread = comment_thread(&mut *conn, &spec, kind, target, None)
        .await
        .expect("comment_thread");
    assert_eq!(thread.len(), 1);
    assert_eq!(
        recompute_comment_count(&mut *conn, &spec, kind, target, None)
            .await
            .expect("recompute_comment_count"),
        1
    );
    assert_eq!(
        delete_comment(&mut *conn, &spec, kind, target, comment.id, None)
            .await
            .expect("delete_comment"),
        1
    );
}
