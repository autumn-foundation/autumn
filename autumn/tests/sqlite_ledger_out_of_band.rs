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
    autumn_web::reexports::diesel::table! {
        lgo_users (id) {
            id -> Int8,
            name -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_vote_posts (id) {
            id -> Int8,
            title -> Text,
            score -> Int8,
            deleted_at -> Nullable<Timestamp>,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_notes (id) {
            id -> Int8,
            title -> Text,
            comment_count -> Int8,
        }
    }
    autumn_web::reexports::diesel::table! {
        lgo_note_comments (id) {
            id -> Int8,
            commentable_type -> Text,
            commentable_id -> Int8,
            parent_id -> Nullable<Int8>,
            author_id -> Int8,
            body -> Text,
            created_at -> Timestamp,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::{
    lgo_comments, lgo_del_children, lgo_del_parents, lgo_note_comments, lgo_notes,
    lgo_nul_children, lgo_nul_parents, lgo_plain_comments, lgo_plain_posts, lgo_posts, lgo_users,
    lgo_vote_posts,
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

// ── #[votable] on a ledgered target ──────────────────────────────────

#[autumn_web::model(table = "lgo_users")]
pub struct LgoUser {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(LgoUser, table = "lgo_users")]
pub trait LgoUserRepository {}

#[autumn_web::model(table = "lgo_vote_posts")]
#[votable(
    by = LgoUser,
    aggregate = sum,
    table = lgo_post_votes,
    reactor_fk = voter_id,
    target_fk = post_id
)]
pub struct LgoVotePost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub score: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(LgoVotePost, table = "lgo_vote_posts", soft_delete, ledgered = true)]
pub trait LgoVotePostRepository {}

// ── #[commentable] into a ledgered comments table ────────────────────

#[autumn_web::model(table = "lgo_notes")]
#[commentable(by = LgoUser, table = lgo_note_comments)]
pub struct LgoNote {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(LgoNote, table = "lgo_notes")]
pub trait LgoNoteRepository {}

#[autumn_web::model(table = "lgo_note_comments")]
pub struct LgoNoteComment {
    #[id]
    pub id: i64,
    pub commentable_type: String,
    pub commentable_id: i64,
    pub parent_id: Option<i64>,
    pub author_id: i64,
    pub body: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LgoNoteComment,
    table = "lgo_note_comments",
    soft_delete,
    ledgered = true
)]
pub trait LgoNoteCommentRepository {}

// ── a second repository over a ledgered table, not ledgered ──────────

mod shadow {
    use super::schema::lgo_posts;
    use autumn_web::reexports::chrono;

    #[autumn_web::model(table = "lgo_posts")]
    pub struct LgoShadowPost {
        #[id]
        pub id: i64,
        pub title: String,
        #[default]
        pub comment_count: i64,
        #[default]
        pub deleted_at: Option<chrono::NaiveDateTime>,
    }

    #[autumn_web::repository(LgoShadowPost, table = "lgo_posts")]
    pub trait LgoShadowPostRepository {}
}

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
        "CREATE TABLE lgo_users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
        "CREATE TABLE lgo_vote_posts (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         title TEXT NOT NULL, score BIGINT NOT NULL DEFAULT 0, deleted_at TIMESTAMP)",
        "CREATE TABLE lgo_post_votes (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         voter_id BIGINT NOT NULL, post_id BIGINT NOT NULL, \
         value SMALLINT NOT NULL CHECK (value IN (-1, 1)), UNIQUE (voter_id, post_id))",
        "CREATE TABLE lgo_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         title TEXT NOT NULL, comment_count BIGINT NOT NULL DEFAULT 0)",
        "CREATE TABLE lgo_note_comments (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         commentable_type TEXT NOT NULL, commentable_id BIGINT NOT NULL, \
         parent_id BIGINT, author_id BIGINT NOT NULL, body TEXT NOT NULL, \
         created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, deleted_at TIMESTAMP)",
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

// ── hand-written SQL: detected, not prevented ────────────────────────

/// Autumn cannot refuse SQL it does not issue. `ledger_verify` reports it.
#[tokio::test]
async fn a_hand_written_update_is_reported_as_live_state_mismatch() {
    use autumn_web::ledger::LedgerBreak;
    use diesel_async::RunQueryDsl as _;

    let pool = boot_pool("lgo_hand_update").await;
    let posts = PgLgoPostRepository::with_pool_untracked(pool.clone());
    let post = posts
        .save(&NewLgoPost {
            title: "p".to_string(),
        })
        .await
        .expect("insert");
    assert!(posts.ledger_verify(post.id).await.unwrap().is_intact());

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("UPDATE lgo_posts SET title = 'edited' WHERE id = ?")
        .bind::<diesel::sql_types::BigInt, _>(post.id)
        .execute(&mut *conn)
        .await
        .expect("hand-written update");
    drop(conn);

    let broken = posts
        .ledger_verify(post.id)
        .await
        .expect("verify")
        .broken
        .expect("a hand-written update must be reported");
    assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
    assert_eq!(broken.seq, 1);
}

/// A hand-written `DELETE` is reported too.
#[tokio::test]
async fn a_hand_written_delete_is_reported_as_live_state_mismatch() {
    use autumn_web::ledger::LedgerBreak;
    use diesel_async::RunQueryDsl as _;

    let pool = boot_pool("lgo_hand_delete").await;
    let posts = PgLgoPostRepository::with_pool_untracked(pool.clone());
    let post = posts
        .save(&NewLgoPost {
            title: "p".to_string(),
        })
        .await
        .expect("insert");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("DELETE FROM lgo_posts WHERE id = ?")
        .bind::<diesel::sql_types::BigInt, _>(post.id)
        .execute(&mut *conn)
        .await
        .expect("hand-written delete");
    drop(conn);

    let broken = posts
        .ledger_verify(post.id)
        .await
        .expect("verify")
        .broken
        .expect("a hand-written delete must be reported");
    assert_eq!(broken.kind, LedgerBreak::LiveStateMismatch);
}

/// Zero false positives: repository writes and refused paths leave the
/// chain intact.
#[tokio::test]
async fn refused_paths_and_repository_writes_never_report_a_mismatch() {
    use autumn_web::hooks::Patch;

    let pool = boot_pool("lgo_no_false_positive").await;
    let posts = PgLgoPostRepository::with_pool_untracked(pool.clone());
    let comments = PgLgoCommentRepository::with_pool_untracked(pool);
    let post = posts
        .save(&NewLgoPost {
            title: "p".to_string(),
        })
        .await
        .expect("insert");
    for step in 0..3 {
        posts
            .update(
                post.id,
                &UpdateLgoPost {
                    title: Patch::Set(format!("p{step}")),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        // Refused every time. The chain must stay true.
        let err = comments
            .save(&NewLgoComment {
                post_id: post.id,
                body: "c".to_string(),
            })
            .await
            .expect_err("refused");
        assert_eq!(out_of_band(&err), ("lgo_posts", "counter cache"));
    }
    posts.delete_by_id(post.id).await.expect("soft delete");
    posts.restore(post.id).await.expect("restore");

    let report = posts.ledger_verify(post.id).await.expect("verify");
    assert!(report.is_intact(), "{report:?}");
    assert_eq!(report.revisions_checked, 6);
}

// ── #2319 audit: more framework paths ────────────────────────────────

/// `react()` updates the target's aggregate column with raw SQL.
#[tokio::test]
async fn a_reaction_on_a_ledgered_target_is_refused() {
    let pool = boot_pool("lgo_votable").await;
    let users = PgLgoUserRepository::with_pool_untracked(pool.clone());
    let posts = PgLgoVotePostRepository::with_pool_untracked(pool);
    let user = users
        .save(&NewLgoUser {
            name: "ada".to_string(),
        })
        .await
        .expect("user");
    let post = posts
        .save(&NewLgoVotePost {
            title: "p".to_string(),
        })
        .await
        .expect("post");

    let err = posts
        .react(user.id, post.id, 1)
        .await
        .expect_err("a reaction must not move a ledgered row unrecorded");
    assert_eq!(out_of_band(&err), ("lgo_vote_posts", "votable aggregate"));
    let live = posts.find_by_id(post.id).await.unwrap().unwrap();
    assert_eq!(live.score, 0);
    assert!(posts.ledger_verify(post.id).await.unwrap().is_intact());
}

/// `add_comment` / `delete_comment` write the comments table with raw SQL.
#[tokio::test]
async fn a_comment_write_into_a_ledgered_comments_table_is_refused() {
    let pool = boot_pool("lgo_commentable").await;
    let users = PgLgoUserRepository::with_pool_untracked(pool.clone());
    let notes = PgLgoNoteRepository::with_pool_untracked(pool.clone());
    let comments = PgLgoNoteCommentRepository::with_pool_untracked(pool);
    let user = users
        .save(&NewLgoUser {
            name: "ada".to_string(),
        })
        .await
        .expect("user");
    let note = notes
        .save(&NewLgoNote {
            title: "n".to_string(),
        })
        .await
        .expect("note");

    let err = notes
        .add_comment(note.id, user.id, "hi", None)
        .await
        .expect_err("add refused");
    assert_eq!(out_of_band(&err), ("lgo_note_comments", "commentable"));
    let err = notes
        .delete_comment(note.id, 1)
        .await
        .expect_err("delete refused");
    assert_eq!(out_of_band(&err), ("lgo_note_comments", "commentable"));
    assert!(comments.find_all().await.expect("list").is_empty());
}

/// A repository that is not ledgered cannot write to a ledgered table.
/// It can still read it.
#[tokio::test]
async fn an_unledgered_repository_cannot_write_to_a_ledgered_table() {
    use shadow::{LgoShadowPostRepository as _, NewLgoShadowPost, PgLgoShadowPostRepository};

    let pool = boot_pool("lgo_shadow").await;
    let posts = PgLgoPostRepository::with_pool_untracked(pool.clone());
    let shadow = PgLgoShadowPostRepository::with_pool_untracked(pool);
    let post = posts
        .save(&NewLgoPost {
            title: "p".to_string(),
        })
        .await
        .expect("insert");

    let err = shadow
        .save(&NewLgoShadowPost {
            title: "q".to_string(),
        })
        .await
        .expect_err("an unledgered write must be refused");
    assert_eq!(
        out_of_band(&err),
        ("lgo_posts", "a repository that is not ledgered")
    );
    let err = shadow
        .delete_by_id(post.id)
        .await
        .expect_err("an unledgered delete must be refused");
    assert_eq!(
        out_of_band(&err),
        ("lgo_posts", "a repository that is not ledgered")
    );

    assert_eq!(
        shadow.find_by_id(post.id).await.unwrap().unwrap().title,
        "p"
    );
    assert!(posts.ledger_verify(post.id).await.unwrap().is_intact());
}
