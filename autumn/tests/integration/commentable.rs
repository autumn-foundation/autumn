//! Database-level integration tests for the **polymorphic** `#[commentable]`
//! association (issue #1367).
//!
//! `#[commentable]` is autumn's fifth association kind: unlike
//! `belongs_to`/`has_many`/`has_one`/`through`, the child does not name one
//! parent table in its foreign key. A single `comments` table keyed on
//! `(commentable_type, commentable_id)` attaches to **any** number of parent
//! models, and `parent_id` threads replies under their parent comment.
//!
//! The behavioural tests **require Docker** (testcontainers) and are
//! `#[ignore]`d by default; CI's ignored-test sweep runs them. The
//! non-ignored tests prove the generated surface and the compile-time spec
//! metadata the SQL is built from, without a live database (the #1592 guard
//! pattern).
//!
//! Fixtures — two *different* parent models sharing one `cmt_comments` table,
//! which is the whole point of AC1/AC5:
//!
//! | Parent | Attribute | Counter column |
//! |---|---|---|
//! | `CmtPost` | `#[commentable(by = CmtUser, table = cmt_comments, author_name = name)]` | `comment_count` |
//! | `CmtPhoto` | same table, different `commentable_type` | `comment_count` |
//! | `CmtCapped` | counter column carries `CHECK (comment_count <= 2)` | `comment_count` |
//!
//! `CmtCapped` exists to make **same-transaction** atomicity observable (AC6):
//! its counter column is `CHECK`ed, so the *counter update* is what fails on
//! the third comment. If the increment were issued outside the insert's
//! transaction the comment row would survive; because it is inside, the whole
//! thing rolls back. That is the only shape that distinguishes "same
//! transaction" from "two statements that usually both succeed".

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::commentable::{CommentNode, commentable_spec_for, registered_commentable_types};
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Fixtures ────────────────────────────────────────────────────────────────

diesel::table! {
    cmt_users (id) {
        id -> Int8,
        name -> Text,
    }
}

#[autumn_web::model(table = "cmt_users")]
pub struct CmtUser {
    #[id]
    pub id: i64,
    pub name: String,
}

diesel::table! {
    cmt_posts (id) {
        id -> Int8,
        title -> Text,
        comment_count -> Int8,
    }
}

#[autumn_web::model(table = "cmt_posts")]
#[commentable(by = CmtUser, table = cmt_comments, author_name = name)]
pub struct CmtPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(CmtPost, table = "cmt_posts")]
pub trait CmtPostRepository {}

diesel::table! {
    cmt_photos (id) {
        id -> Int8,
        caption -> Text,
        comment_count -> Int8,
    }
}

/// AC5: the *second* model. It declares nothing but the attribute — no table,
/// no routes, no queries of its own — and shares `cmt_comments` with
/// `CmtPost`, told apart only by `commentable_type`.
#[autumn_web::model(table = "cmt_photos")]
#[commentable(by = CmtUser, table = cmt_comments, author_name = name)]
pub struct CmtPhoto {
    #[id]
    pub id: i64,
    pub caption: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(CmtPhoto, table = "cmt_photos")]
pub trait CmtPhotoRepository {}

diesel::table! {
    cmt_shallows (id) {
        id -> Int8,
        title -> Text,
        comment_count -> Int8,
    }
}

/// `max_depth = 1`: a top-level comment plus exactly one level of replies.
#[autumn_web::model(table = "cmt_shallows")]
#[commentable(by = CmtUser, table = cmt_comments, max_depth = 1)]
pub struct CmtShallow {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(CmtShallow, table = "cmt_shallows")]
pub trait CmtShallowRepository {}

diesel::table! {
    cmt_cappeds (id) {
        id -> Int8,
        title -> Text,
        comment_count -> Int8,
    }
}

#[autumn_web::model(table = "cmt_cappeds")]
#[commentable(by = CmtUser, table = cmt_comments)]
pub struct CmtCapped {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(CmtCapped, table = "cmt_cappeds")]
pub trait CmtCappedRepository {}

diesel::table! {
    cmt_uncounteds (id) {
        id -> Int8,
        title -> Text,
    }
}

/// A parent with **no** counter column: `counter_cache = false` opts out, and
/// the generated SQL must then issue no counter statement at all.
#[autumn_web::model(table = "cmt_uncounteds")]
#[commentable(by = CmtUser, table = cmt_comments, counter_cache = false)]
pub struct CmtUncounted {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(CmtUncounted, table = "cmt_uncounteds")]
pub trait CmtUncountedRepository {}

// ── DDL ─────────────────────────────────────────────────────────────────────

const DDL: &[&str] = &[
    "CREATE TABLE cmt_users (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
    "CREATE TABLE cmt_posts \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE cmt_photos \
     (id BIGSERIAL PRIMARY KEY, caption TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE cmt_shallows \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE cmt_cappeds \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0 CHECK (comment_count <= 2))",
    "CREATE TABLE cmt_uncounteds (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
    // The one shared, polymorphic comments table (AC1).
    "CREATE TABLE cmt_comments (\
       id BIGSERIAL PRIMARY KEY, \
       commentable_type TEXT NOT NULL, \
       commentable_id BIGINT NOT NULL, \
       parent_id BIGINT REFERENCES cmt_comments(id) ON DELETE CASCADE, \
       author_id BIGINT NOT NULL REFERENCES cmt_users(id), \
       body TEXT NOT NULL, \
       created_at TIMESTAMP NOT NULL DEFAULT NOW(), \
       deleted_at TIMESTAMP)",
    "CREATE INDEX idx_cmt_comments_target \
     ON cmt_comments (commentable_type, commentable_id)",
    "CREATE INDEX idx_cmt_comments_parent ON cmt_comments (parent_id)",
];

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    // Comfortably exceeds the concurrent callers in the race test plus the
    // observer connection, so no caller ever queues on pool acquisition.
    let pool = Pool::builder(manager).max_size(40).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    for stmt in DDL {
        diesel::sql_query(*stmt)
            .execute(&mut conn)
            .await
            .unwrap_or_else(|e| panic!("DDL failed ({stmt}): {e}"));
    }
    drop(conn);

    (pool, container)
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

async fn seed_user(conn: &mut AsyncPgConnection, name: &str) -> i64 {
    diesel::sql_query("INSERT INTO cmt_users (name) VALUES ($1) RETURNING id")
        .bind::<Text, _>(name)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed user")
        .id
}

async fn seed_one_col(conn: &mut AsyncPgConnection, table: &str, column: &str, value: &str) -> i64 {
    diesel::sql_query(format!(
        "INSERT INTO {table} ({column}) VALUES ($1) RETURNING id"
    ))
    .bind::<Text, _>(value)
    .get_result::<IdRow>(conn)
    .await
    .unwrap_or_else(|e| panic!("seed {table}: {e}"))
    .id
}

async fn counter(conn: &mut AsyncPgConnection, table: &str, id: i64) -> i64 {
    diesel::sql_query(format!(
        "SELECT comment_count AS count FROM {table} WHERE id = $1"
    ))
    .bind::<BigInt, _>(id)
    .get_result::<CountRow>(conn)
    .await
    .expect("read counter")
    .count
}

async fn row_count(conn: &mut AsyncPgConnection, predicate: &str) -> i64 {
    diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM cmt_comments WHERE {predicate}"
    ))
    .get_result::<CountRow>(conn)
    .await
    .expect("count rows")
    .count
}

/// Flatten a nested thread to `(depth, body)` in render order.
fn flatten(nodes: &[CommentNode]) -> Vec<(usize, String)> {
    fn walk(nodes: &[CommentNode], out: &mut Vec<(usize, String)>) {
        for node in nodes {
            out.push((node.depth, node.comment.body.clone()));
            walk(&node.replies, out);
        }
    }
    let mut out = Vec::new();
    walk(nodes, &mut out);
    out
}

// ── AC1/AC2: the spec is derived from the documented conventions ────────────

#[test]
fn commentable_spec_uses_the_documented_conventions() {
    let spec = CmtPost::commentable_spec();
    assert_eq!(CmtPost::COMMENTABLE_TYPE, "CmtPost");
    assert_eq!(spec.comments_table, "cmt_comments");
    assert_eq!(spec.comment_pk, "id");
    assert_eq!(spec.type_column, "commentable_type");
    assert_eq!(spec.id_column, "commentable_id");
    assert_eq!(spec.parent_column, "parent_id");
    assert_eq!(spec.author_column, "author_id");
    assert_eq!(spec.body_column, "body");
    assert_eq!(spec.created_at_column, "created_at");
    assert!(spec.soft_delete, "the comments table always soft-deletes");
    assert_eq!(spec.parent_table, "cmt_posts");
    assert_eq!(spec.parent_pk, "id");
    assert_eq!(spec.counter_column, Some("comment_count"));
    assert_eq!(spec.author_table, Some("cmt_users"));
    assert_eq!(spec.author_name_column, Some("name"));
    assert_eq!(spec.max_depth, 5, "the default nesting depth");
    assert_eq!(spec.parent_tenant_column, None);

    // AC5's second model: same table, different discriminator.
    let photo = CmtPhoto::commentable_spec();
    assert_eq!(CmtPhoto::COMMENTABLE_TYPE, "CmtPhoto");
    assert_eq!(photo.comments_table, spec.comments_table);
    assert_eq!(photo.parent_table, "cmt_photos");

    // Overrides.
    assert_eq!(CmtShallow::commentable_spec().max_depth, 1);
    assert_eq!(CmtShallow::commentable_spec().author_name_column, None);
    assert_eq!(CmtUncounted::commentable_spec().counter_column, None);
}

/// AC5: every `#[commentable]` model registers itself, so a generic router can
/// dispatch on `commentable_type` without the app naming its models twice.
#[test]
fn every_commentable_model_registers_itself() {
    let types = registered_commentable_types();
    for expected in ["CmtPost", "CmtPhoto", "CmtShallow", "CmtCapped"] {
        assert!(
            types.contains(&expected),
            "{expected} missing from the commentable registry: {types:?}"
        );
    }
    let spec = commentable_spec_for("CmtPhoto").expect("CmtPhoto is registered");
    assert_eq!(spec.parent_table, "cmt_photos");
    assert!(commentable_spec_for("NoSuchModel").is_none());
}

/// AC3: the repository helpers exist on the generated repository.
#[test]
fn comment_helpers_are_generated() {
    fn assert_is_fn<F>(_f: F) {}
    assert_is_fn(<PgCmtPostRepository as CmtPostComments>::add_comment);
    assert_is_fn(<PgCmtPostRepository as CmtPostComments>::comment_thread);
    assert_is_fn(<PgCmtPostRepository as CmtPostComments>::delete_comment);
    assert_is_fn(<PgCmtPhotoRepository as CmtPhotoComments>::add_comment);
}

// ── AC3/AC6: create ─────────────────────────────────────────────────────────

/// AC3 + AC6: a root comment lands and the parent's `comment_count` moves with
/// it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_comment_creates_a_root_comment_and_increments_the_counter() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let comment = repo
        .add_comment(post, author, "first!", None)
        .await
        .expect("add_comment");

    assert_eq!(comment.body, "first!");
    assert_eq!(comment.author_id, author);
    assert_eq!(comment.parent_id, None);
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);
    assert_eq!(
        row_count(
            &mut conn,
            &format!("commentable_type = 'CmtPost' AND commentable_id = {post}")
        )
        .await,
        1
    );
}

/// AC1 + AC5: two different parent models, one physical table. Neither can see
/// the other's thread even when the ids collide.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn two_models_share_one_comments_table() {
    let (pool, _container) = setup_pool().await;
    let posts = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let photos = PgCmtPhotoRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "p").await;
    let photo = seed_one_col(&mut conn, "cmt_photos", "caption", "c").await;
    assert_eq!(post, photo, "fixture relies on colliding ids");

    posts
        .add_comment(post, author, "on the post", None)
        .await
        .expect("post comment");
    photos
        .add_comment(photo, author, "on the photo", None)
        .await
        .expect("photo comment");

    let post_thread = posts.comment_thread(post).await.expect("post thread");
    let photo_thread = photos.comment_thread(photo).await.expect("photo thread");
    assert_eq!(flatten(&post_thread), vec![(0, "on the post".to_owned())]);
    assert_eq!(flatten(&photo_thread), vec![(0, "on the photo".to_owned())]);
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);
    assert_eq!(counter(&mut conn, "cmt_photos", photo).await, 1);
}

/// AC3: replies nest under their parent, in stable creation order.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn comment_thread_nests_replies_in_stable_order() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let a = repo.add_comment(post, author, "a", None).await.expect("a");
    let a1 = repo
        .add_comment(post, author, "a1", Some(a.id))
        .await
        .expect("a1");
    repo.add_comment(post, author, "a1x", Some(a1.id))
        .await
        .expect("a1x");
    repo.add_comment(post, author, "a2", Some(a.id))
        .await
        .expect("a2");
    repo.add_comment(post, author, "b", None).await.expect("b");

    let thread = repo.comment_thread(post).await.expect("thread");
    assert_eq!(
        flatten(&thread),
        vec![
            (0, "a".to_owned()),
            (1, "a1".to_owned()),
            (2, "a1x".to_owned()),
            (1, "a2".to_owned()),
            (0, "b".to_owned()),
        ]
    );
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 5);
}

/// A reply may only attach to a comment on the **same** parent — otherwise a
/// caller who knows any comment id could graft a thread onto someone else's
/// record.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_comment_rejects_a_reply_to_another_parents_comment() {
    let (pool, _container) = setup_pool().await;
    let posts = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let photos = PgCmtPhotoRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "p").await;
    let photo = seed_one_col(&mut conn, "cmt_photos", "caption", "c").await;

    let on_post = posts
        .add_comment(post, author, "root", None)
        .await
        .expect("root");

    let err = photos
        .add_comment(photo, author, "graft", Some(on_post.id))
        .await
        .expect_err("a cross-parent reply must be rejected");
    assert_eq!(err.status().as_u16(), 422);
    assert_eq!(counter(&mut conn, "cmt_photos", photo).await, 0);
}

/// `max_depth` bounds nesting on the write path, so the render never has to.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_comment_rejects_a_reply_past_max_depth() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtShallowRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let target = seed_one_col(&mut conn, "cmt_shallows", "title", "t").await;

    let root = repo
        .add_comment(target, author, "root", None)
        .await
        .expect("root");
    let reply = repo
        .add_comment(target, author, "reply", Some(root.id))
        .await
        .expect("depth 1 is allowed at max_depth = 1");

    let err = repo
        .add_comment(target, author, "too deep", Some(reply.id))
        .await
        .expect_err("depth 2 exceeds max_depth = 1");
    assert_eq!(err.status().as_u16(), 422);
    assert_eq!(counter(&mut conn, "cmt_shallows", target).await, 2);
}

/// A blank body is rejected before anything is written.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_comment_rejects_a_blank_body() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let err = repo
        .add_comment(post, author, "   \n ", None)
        .await
        .expect_err("a blank body must be rejected");
    assert_eq!(err.status().as_u16(), 422);
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 0);
}

/// An unknown parent id is `404`, not a dangling comment row: the polymorphic
/// `commentable_id` carries no database foreign key, so this probe **is** the
/// referential check.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_comment_rejects_an_unknown_parent() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;

    let err = repo
        .add_comment(9_999_999, author, "ghost", None)
        .await
        .expect_err("an unknown parent must be rejected");
    assert_eq!(err.status().as_u16(), 404);
    assert_eq!(row_count(&mut conn, "1 = 1").await, 0);
}

/// AC6: the comment row and the counter update commit or roll back
/// **together**. The parent's counter carries `CHECK (comment_count <= 2)`, so
/// the third comment's *increment* is what fails — and the comment row must not
/// survive it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_failing_counter_update_rolls_the_comment_back() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtCappedRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let target = seed_one_col(&mut conn, "cmt_cappeds", "title", "t").await;

    repo.add_comment(target, author, "one", None)
        .await
        .expect("one");
    repo.add_comment(target, author, "two", None)
        .await
        .expect("two");
    repo.add_comment(target, author, "three", None)
        .await
        .expect_err("the third increment violates the CHECK");

    assert_eq!(counter(&mut conn, "cmt_cappeds", target).await, 2);
    assert_eq!(
        row_count(
            &mut conn,
            &format!("commentable_type = 'CmtCapped' AND commentable_id = {target}")
        )
        .await,
        2,
        "the rolled-back comment must not survive its failed increment"
    );
}

/// AC6: the increment is a single `SET c = c + 1` statement, so N concurrent
/// callers commute and the final count is exactly N under every interleaving.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_comments_all_count() {
    let (pool, _container) = setup_pool().await;
    let repo = std::sync::Arc::new(PgCmtPostRepository::with_pool_untracked(pool.clone()));
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let mut handles = Vec::new();
    for i in 0..20 {
        let repo = repo.clone();
        handles.push(tokio::spawn(async move {
            repo.add_comment(post, author, &format!("c{i}"), None).await
        }));
    }
    for handle in handles {
        handle.await.expect("join").expect("add_comment");
    }

    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 20);
    assert_eq!(
        row_count(
            &mut conn,
            &format!("commentable_type = 'CmtPost' AND commentable_id = {post}")
        )
        .await,
        20
    );
}

// ── AC3/AC6: delete cascades and the thread is soft-delete aware ────────────

/// AC3 + AC6: deleting a comment takes its whole descendant subtree with it and
/// decrements the counter by exactly the number of rows removed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_comment_cascades_to_descendants_and_decrements() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let a = repo.add_comment(post, author, "a", None).await.expect("a");
    let a1 = repo
        .add_comment(post, author, "a1", Some(a.id))
        .await
        .expect("a1");
    repo.add_comment(post, author, "a1x", Some(a1.id))
        .await
        .expect("a1x");
    let b = repo.add_comment(post, author, "b", None).await.expect("b");
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 4);

    let removed = repo.delete_comment(a.id).await.expect("delete");
    assert_eq!(removed, 3, "the root plus both descendants");
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);

    // AC6: the thread query is soft-delete aware.
    let thread = repo.comment_thread(post).await.expect("thread");
    assert_eq!(flatten(&thread), vec![(0, "b".to_owned())]);
    assert_eq!(thread[0].comment.id, b.id);

    // Deleting again is a no-op — the counter must not run away.
    assert_eq!(repo.delete_comment(a.id).await.expect("second delete"), 0);
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);
}

/// A parent with no counter column runs the same write path minus the counter
/// statement.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_parent_without_a_counter_column_still_threads() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtUncountedRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let target = seed_one_col(&mut conn, "cmt_uncounteds", "title", "t").await;

    let root = repo
        .add_comment(target, author, "root", None)
        .await
        .expect("root");
    repo.add_comment(target, author, "reply", Some(root.id))
        .await
        .expect("reply");

    let thread = repo.comment_thread(target).await.expect("thread");
    assert_eq!(
        flatten(&thread),
        vec![(0, "root".to_owned()), (1, "reply".to_owned())]
    );
    assert_eq!(repo.delete_comment(root.id).await.expect("delete"), 2);
}

/// Author display names come back with the thread when the model declares
/// `author_name`, so the widget never has to N+1 per node.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn comment_thread_resolves_author_names() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let ada = seed_user(&mut conn, "ada").await;
    let grace = seed_user(&mut conn, "grace").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let root = repo.add_comment(post, ada, "a", None).await.expect("a");
    repo.add_comment(post, grace, "g", Some(root.id))
        .await
        .expect("g");

    let thread = repo.comment_thread(post).await.expect("thread");
    assert_eq!(thread[0].comment.author_name.as_deref(), Some("ada"));
    assert_eq!(
        thread[0].replies[0].comment.author_name.as_deref(),
        Some("grace")
    );

    // A model that declares no `author_name` leaves it unresolved rather than
    // guessing a column.
    let shallow = PgCmtShallowRepository::with_pool_untracked(pool.clone());
    let target = seed_one_col(&mut conn, "cmt_shallows", "title", "t").await;
    shallow
        .add_comment(target, ada, "x", None)
        .await
        .expect("x");
    let thread = shallow.comment_thread(target).await.expect("thread");
    assert_eq!(thread[0].comment.author_name, None);
}
