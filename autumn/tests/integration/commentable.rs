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
use autumn_web::reexports::axum;
use autumn_web::tenancy::CURRENT_TENANT;
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

diesel::table! {
    cmt_tenanted (id) {
        id -> Int8,
        title -> Text,
        tenant_id -> Text,
        comment_count -> Int8,
    }
}

/// A `tenant_id` column switches the parent probe to its tenant-scoped arm.
#[autumn_web::model(table = "cmt_tenanted")]
#[commentable(by = CmtUser, table = cmt_comments)]
pub struct CmtTenanted {
    #[id]
    pub id: i64,
    pub title: String,
    pub tenant_id: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(CmtTenanted, table = "cmt_tenanted", tenant_scoped)]
pub trait CmtTenantedRepository {}

diesel::table! {
    cmt_softs (id) {
        id -> Int8,
        title -> Text,
        comment_count -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

/// A soft-deleting **parent**: once it is gone it accepts no comments and
/// reports no thread.
#[autumn_web::model(table = "cmt_softs")]
#[commentable(by = CmtUser, table = cmt_comments)]
pub struct CmtSoft {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(CmtSoft, table = "cmt_softs", soft_delete)]
pub trait CmtSoftRepository {}

diesel::table! {
    cmt_hards (id) {
        id -> Int8,
        title -> Text,
        comment_count -> Int8,
    }
}

/// `soft_delete = false`: the comments table has no `deleted_at`, so deleting a
/// comment removes the rows outright. On a table whose `parent_id` cascades,
/// that is the shape where a naive affected-row count under-reports.
#[autumn_web::model(table = "cmt_hards")]
#[commentable(by = CmtUser, table = cmt_hard_comments, soft_delete = false)]
pub struct CmtHard {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(CmtHard, table = "cmt_hards")]
pub trait CmtHardRepository {}

diesel::table! {
    cmt_capped_bodies (id) {
        id -> Int8,
        title -> Text,
        comment_count -> Int8,
    }
}

/// `max_body` in bytes, so the runtime cap is observable.
#[autumn_web::model(table = "cmt_capped_bodies")]
#[commentable(by = CmtUser, table = cmt_comments, max_body = 16)]
pub struct CmtCappedBody {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub comment_count: i64,
}

#[autumn_web::repository(CmtCappedBody, table = "cmt_capped_bodies")]
pub trait CmtCappedBodyRepository {}

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
    "CREATE TABLE cmt_tenanted \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, tenant_id TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE cmt_softs \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0, deleted_at TIMESTAMP)",
    "CREATE TABLE cmt_hards \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE cmt_capped_bodies \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      comment_count BIGINT NOT NULL DEFAULT 0)",
    // A SECOND comments table, with no `deleted_at` and a cascading self-FK —
    // the `soft_delete = false` shape.
    "CREATE TABLE cmt_hard_comments (\
       id BIGSERIAL PRIMARY KEY, \
       commentable_type TEXT NOT NULL, \
       commentable_id BIGINT NOT NULL, \
       parent_id BIGINT REFERENCES cmt_hard_comments(id) ON DELETE CASCADE, \
       author_id BIGINT NOT NULL REFERENCES cmt_users(id), \
       body TEXT NOT NULL, \
       created_at TIMESTAMP NOT NULL DEFAULT NOW())",
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
    // Comfortably exceeds the concurrent callers in the race test plus the
    // observer connection, so no caller ever queues on pool acquisition.
    setup_pool_with_size(40).await
}

/// The same fixture with an explicit pool size, for the tests that care how
/// many connections a single request holds at once.
async fn setup_pool_with_size(
    max_size: usize,
) -> (
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
    let pool = Pool::builder(manager)
        .max_size(max_size)
        .build()
        .expect("pool");

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

async fn seed_tenanted(conn: &mut AsyncPgConnection, tenant: &str, title: &str) -> i64 {
    diesel::sql_query("INSERT INTO cmt_tenanted (tenant_id, title) VALUES ($1, $2) RETURNING id")
        .bind::<Text, _>(tenant)
        .bind::<Text, _>(title)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed tenanted")
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
    assert_is_fn(<PgCmtPostRepository as CmtPostComments>::recompute_comment_count);
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

    let removed = repo.delete_comment(post, a.id).await.expect("delete");
    assert_eq!(removed, 3, "the root plus both descendants");
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);

    // AC6: the thread query is soft-delete aware.
    let thread = repo.comment_thread(post).await.expect("thread");
    assert_eq!(flatten(&thread), vec![(0, "b".to_owned())]);
    assert_eq!(thread[0].comment.id, b.id);

    // Deleting again is a no-op — the counter must not run away.
    assert_eq!(
        repo.delete_comment(post, a.id)
            .await
            .expect("second delete"),
        0
    );
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
    assert_eq!(
        repo.delete_comment(target, root.id).await.expect("delete"),
        2
    );
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

// ── Branches the default fixtures never reach ───────────────────────────────

/// `soft_delete = false` removes the rows outright — and the counter delta is
/// the number of rows the subtree actually held.
///
/// This is the shape where inferring the delta from the statement's
/// affected-row count is wrong: `parent_id` cascades, and `SQLite` excludes
/// foreign-key-removed rows from `changes()`, so a three-row subtree would
/// report one and leave the counter permanently two too high.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_hard_delete_comments_table_removes_the_subtree_outright() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtHardRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let target = seed_one_col(&mut conn, "cmt_hards", "title", "t").await;

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
    assert_eq!(counter(&mut conn, "cmt_hards", target).await, 3);

    assert_eq!(
        repo.delete_comment(target, root.id).await.expect("delete"),
        3,
        "the whole subtree, however the engine reports affected rows"
    );
    assert_eq!(counter(&mut conn, "cmt_hards", target).await, 0);
    let remaining = diesel::sql_query("SELECT COUNT(*) AS count FROM cmt_hard_comments")
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count")
        .count;
    assert_eq!(remaining, 0, "hard delete leaves nothing behind");
}

/// The body cap is enforced in **bytes**, as documented — a multi-byte body
/// that is well under the cap in characters is still over it in bytes.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_comment_rejects_a_body_over_the_byte_cap() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtCappedBodyRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let target = seed_one_col(&mut conn, "cmt_capped_bodies", "title", "t").await;

    repo.add_comment(target, author, "0123456789abcdef", None)
        .await
        .expect("exactly 16 bytes fits");

    let err = repo
        .add_comment(target, author, "0123456789abcdefg", None)
        .await
        .expect_err("17 bytes is over the cap");
    assert_eq!(err.status().as_u16(), 422);

    // 9 characters, 18 bytes: the cap counts bytes.
    let err = repo
        .add_comment(target, author, &"é".repeat(9), None)
        .await
        .expect_err("18 bytes is over the cap even at 9 characters");
    assert_eq!(err.status().as_u16(), 422);

    assert_eq!(counter(&mut conn, "cmt_capped_bodies", target).await, 1);
}

/// A soft-deleted parent accepts no comments and reports no thread.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_soft_deleted_parent_refuses_comments_and_reports_no_thread() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtSoftRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let target = seed_one_col(&mut conn, "cmt_softs", "title", "t").await;

    repo.add_comment(target, author, "before", None)
        .await
        .expect("live parent accepts comments");

    diesel::sql_query("UPDATE cmt_softs SET deleted_at = NOW() WHERE id = $1")
        .bind::<BigInt, _>(target)
        .execute(&mut *conn)
        .await
        .expect("soft delete the parent");

    let err = repo
        .add_comment(target, author, "after", None)
        .await
        .expect_err("a soft-deleted parent takes no comments");
    assert_eq!(err.status().as_u16(), 404);
    let err = repo
        .comment_thread(target)
        .await
        .expect_err("nor reports a thread");
    assert_eq!(err.status().as_u16(), 404);
    assert_eq!(counter(&mut conn, "cmt_softs", target).await, 1);
}

/// The two 404s on the delete path: an unknown comment, and a comment that
/// belongs to a **different record of the same model**. The second is what
/// stops any signed-in user deleting any comment by walking ids.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_comment_refuses_an_unknown_or_foreign_comment() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let photos = PgCmtPhotoRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "a").await;
    let other = seed_one_col(&mut conn, "cmt_posts", "title", "b").await;
    let photo = seed_one_col(&mut conn, "cmt_photos", "caption", "c").await;

    let comment = repo
        .add_comment(post, author, "mine", None)
        .await
        .expect("comment");

    let err = repo
        .delete_comment(post, 9_999_999)
        .await
        .expect_err("an unknown comment id is not found");
    assert_eq!(err.status().as_u16(), 404);

    let err = repo
        .delete_comment(other, comment.id)
        .await
        .expect_err("another record's comment is not this record's to delete");
    assert_eq!(err.status().as_u16(), 404);

    let err = photos
        .delete_comment(photo, comment.id)
        .await
        .expect_err("another model's comment either");
    assert_eq!(err.status().as_u16(), 404);

    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);
    assert_eq!(
        repo.delete_comment(post, comment.id).await.expect("delete"),
        1
    );
}

/// `comment_thread` on a parent that does not exist is a `404`, not an empty
/// thread — otherwise a typo'd id renders a plausible-looking empty page.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn comment_thread_on_an_unknown_parent_is_not_found() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let err = repo
        .comment_thread(9_999_999)
        .await
        .expect_err("an unknown parent has no thread");
    assert_eq!(err.status().as_u16(), 404);
}

/// The repair path: a counter drifted by a hand-written write heals, and a
/// second model's comments sharing the id are not counted.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn recompute_comment_count_rebuilds_from_the_discriminator_pair() {
    let (pool, _container) = setup_pool().await;
    let posts = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let photos = PgCmtPhotoRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "p").await;
    let photo = seed_one_col(&mut conn, "cmt_photos", "caption", "c").await;
    assert_eq!(post, photo, "fixture relies on colliding ids");

    posts
        .add_comment(post, author, "counted", None)
        .await
        .expect("post comment");
    photos
        .add_comment(photo, author, "not counted", None)
        .await
        .expect("photo comment");

    diesel::sql_query("UPDATE cmt_posts SET comment_count = 99 WHERE id = $1")
        .bind::<BigInt, _>(post)
        .execute(&mut *conn)
        .await
        .expect("drift");

    assert_eq!(posts.recompute_comment_count(post).await.expect("heal"), 1);
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);
    // Idempotent, and it never counted the photo's comment.
    assert_eq!(posts.recompute_comment_count(post).await.expect("again"), 1);
    assert_eq!(counter(&mut conn, "cmt_photos", photo).await, 1);
}

/// A model with no counter runs the same path and writes nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn recompute_is_a_no_op_without_a_counter_column() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtUncountedRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let target = seed_one_col(&mut conn, "cmt_uncounteds", "title", "t").await;
    repo.add_comment(target, author, "x", None)
        .await
        .expect("comment");
    assert_eq!(
        repo.recompute_comment_count(target).await.expect("no-op"),
        0
    );
}

// ── Tenant isolation ────────────────────────────────────────────────────────

/// A `tenant_scoped` repository must not reach another tenant's record — the
/// comment write is a write to a row the caller only had to guess the id of.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_tenant_scoped_repository_cannot_comment_across_the_tenant_boundary() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtTenantedRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let acme = seed_tenanted(&mut conn, "acme", "theirs").await;

    // Inside `acme`, the record is reachable.
    let comment = CURRENT_TENANT
        .scope(Some("acme".to_owned()), async {
            repo.add_comment(acme, author, "ours", None).await
        })
        .await
        .expect("same-tenant comment");
    assert_eq!(comment.body, "ours");

    // From `globex`, it is `NotFound` before anything is written or read.
    let err = CURRENT_TENANT
        .scope(Some("globex".to_owned()), async {
            repo.add_comment(acme, author, "theirs", None).await
        })
        .await
        .expect_err("another tenant's record must be out of reach");
    assert_eq!(err.status().as_u16(), 404);
    let err = CURRENT_TENANT
        .scope(Some("globex".to_owned()), async {
            repo.comment_thread(acme).await
        })
        .await
        .expect_err("nor readable");
    assert_eq!(err.status().as_u16(), 404);

    assert_eq!(counter(&mut conn, "cmt_tenanted", acme).await, 1);
}

/// `across_tenants()` opts out of the predicate entirely — the documented
/// escape hatch. Branching on the *spec* rather than the resolved tenant would
/// bind NULL here and 404 on a record that plainly exists.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn across_tenants_sees_every_tenants_thread() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtTenantedRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let acme = seed_tenanted(&mut conn, "acme", "theirs").await;

    CURRENT_TENANT
        .scope(Some("acme".to_owned()), async {
            repo.add_comment(acme, author, "ours", None).await
        })
        .await
        .expect("same-tenant comment");

    let thread = repo
        .across_tenants()
        .comment_thread(acme)
        .await
        .expect("across_tenants must not apply a tenant predicate");
    assert_eq!(flatten(&thread), vec![(0, "ours".to_owned())]);
}

/// A `tenant_scoped` repository with no tenant context fails closed, before a
/// connection is taken.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_tenant_scoped_repository_with_no_context_refuses_to_comment() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtTenantedRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let acme = seed_tenanted(&mut conn, "acme", "theirs").await;

    repo.add_comment(acme, author, "no context", None)
        .await
        .expect_err("a tenant_scoped repository needs a tenant");
    assert_eq!(counter(&mut conn, "cmt_tenanted", acme).await, 0);
}

// ── The generic router ──────────────────────────────────────────────────────
//
// One pair of routes serves every registered model. Nothing else in the suite
// exercises it, and it is the surface an app actually mounts.

/// Minimal state exposing the pool to the `Db` extractor, so the real handlers
/// run against a real database.
#[derive(Clone)]
struct RouterState {
    pool: Pool<AsyncPgConnection>,
}

impl autumn_web::db::DbState for RouterState {
    fn pool(&self) -> Option<&Pool<autumn_web::db::RuntimeConnection>> {
        Some(&self.pool)
    }
}

/// The comment router mounted the way an app mounts it.
fn comment_app(
    pool: Pool<AsyncPgConnection>,
    config: autumn_web::commentable::CommentsConfig,
) -> axum::Router {
    axum::Router::new()
        .nest("/comments", autumn_web::commentable::router(config))
        .with_state(RouterState { pool })
}

/// Drive one request through the router and read back `(status, body)`.
async fn call(
    app: axum::Router,
    mut request: axum::http::Request<axum::body::Body>,
) -> (u16, String) {
    use tower::ServiceExt as _;
    // `Session` panics without the extension `SessionLayer` installs, so every
    // request carries one — an empty session is the signed-out shape.
    if request
        .extensions()
        .get::<autumn_web::session::Session>()
        .is_none()
    {
        request
            .extensions_mut()
            .insert(autumn_web::session::Session::new_for_test(
                "anonymous".to_owned(),
                std::collections::HashMap::new(),
            ));
    }
    let response = app.oneshot(request).await.expect("router response");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn get(path: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .expect("request")
}

/// A form `POST`, optionally carrying a session cookie the handler reads the
/// author id from.
fn form_post(path: &str, body: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(body.to_owned()))
        .expect("request")
}

/// AC4/AC5: the router renders a thread for a registered model, keyed only on
/// the path segment.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_renders_the_thread_for_any_registered_model() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;
    repo.add_comment(post, author, "first!", None)
        .await
        .expect("comment");

    let app = comment_app(pool, autumn_web::commentable::CommentsConfig::default());
    let (status, body) = call(app, get(&format!("/comments/CmtPost/{post}"))).await;

    assert_eq!(status, 200);
    assert!(body.contains("first!"), "{body}");
    // The router's own dom id — the one a host page must match, or the first
    // htmx swap lands somewhere the next one cannot find.
    assert!(
        body.contains(&autumn_web::commentable::thread_dom_id("CmtPost", post)),
        "{body}"
    );
    // Signed out: the thread renders, every form is gone.
    assert!(!body.contains("<form"), "{body}");
}

/// The registry is the whole gate: a type no model claims cannot be reached.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_rejects_an_unregistered_commentable_type() {
    let (pool, _container) = setup_pool().await;
    let app = comment_app(pool, autumn_web::commentable::CommentsConfig::default());
    let (status, _) = call(app, get("/comments/NoSuchModel/1")).await;
    assert_eq!(status, 404);
}

/// Posting without a session author is `401`, and writes nothing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_refuses_to_post_without_a_signed_in_author() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let app = comment_app(
        pool.clone(),
        autumn_web::commentable::CommentsConfig::default(),
    );
    let (status, _) = call(
        app,
        form_post(&format!("/comments/CmtPost/{post}"), "body=hi"),
    )
    .await;

    assert_eq!(status, 401);
    assert_eq!(row_count(&mut conn, "1 = 1").await, 0);
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 0);
}

/// The record-level hook is what an app with private records must set: the
/// router authorizes the tenant, never the record.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_honours_the_record_level_authorization_hook() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCmtPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let open = seed_one_col(&mut conn, "cmt_posts", "title", "open").await;
    let private = seed_one_col(&mut conn, "cmt_posts", "title", "private").await;
    repo.add_comment(private, author, "secret", None)
        .await
        .expect("comment");

    let config = autumn_web::commentable::CommentsConfig::default()
        .authorize(move |access| Box::pin(async move { access.parent_id != private }));

    let (status, body) = call(
        comment_app(pool.clone(), config.clone()),
        get(&format!("/comments/CmtPost/{open}")),
    )
    .await;
    assert_eq!(status, 200, "an allowed record still renders: {body}");

    let (status, body) = call(
        comment_app(pool.clone(), config),
        get(&format!("/comments/CmtPost/{private}")),
    )
    .await;
    assert_eq!(
        status, 404,
        "a refusal is 404, never 403 — no existence oracle"
    );
    assert!(!body.contains("secret"), "{body}");
}

/// The authorization hook is documented as the place to run a record-level
/// policy check, and a policy check reads the database. If it ran while the
/// request already held its `Db` checkout, an app on a small pool would
/// deadlock under concurrency rather than answer.
///
/// This drives the hook through a pool with exactly ONE connection, and has the
/// hook itself take a connection from that pool — the shape a real policy check
/// has. It can only pass if authorization completes before the handler's own
/// checkout, which is what declaring the extractor ahead of `Db` buys.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_authorizes_before_it_checks_out_a_connection() {
    let (pool, _container) = setup_pool_with_size(1).await;
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;
    // Release the fixture's connection before the repository takes its own:
    // this pool holds exactly one, so seeding and commenting cannot overlap.
    drop(conn);
    PgCmtPostRepository::with_pool_untracked(pool.clone())
        .add_comment(post, author, "visible", None)
        .await
        .expect("comment");

    // A hook that reads the database, exactly as a `Policy::can_show` would.
    let probe_pool = pool.clone();
    let config = autumn_web::commentable::CommentsConfig::default().authorize(move |_access| {
        let pool = probe_pool.clone();
        Box::pin(async move {
            // Taking a connection here IS the assertion: on a pool of one, this
            // can only succeed if the handler has not checked one out yet. A
            // regression that authorizes from the handler body deadlocks
            // instead of returning, which the timeout below reports.
            let mut conn = pool
                .get()
                .await
                .expect("authorize ran while the request already held the only connection");
            diesel::sql_query("SELECT 1")
                .execute(&mut conn)
                .await
                .expect("the connection handed to authorize must be usable");
            true
        })
    });

    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        call(
            comment_app(pool.clone(), config),
            get(&format!("/comments/CmtPost/{post}")),
        ),
    )
    .await
    .expect("a DB-backed authorize hook must not deadlock the pool");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("visible"), "{body}");
}

/// The router owns no app-specific side effects, so an app adopting it must be
/// able to put its own back — a notification, a live-feed broadcast, a search
/// index. Without this hook, replacing a hand-rolled comment route with the
/// generic one silently *loses* whatever that route did on create.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_reports_each_created_comment_to_the_app() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;
    drop(conn);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = std::sync::Arc::clone(&seen);
    let config = autumn_web::commentable::CommentsConfig::default().on_comment(move |created| {
        let recorder = std::sync::Arc::clone(&recorder);
        Box::pin(async move {
            recorder.lock().expect("not poisoned").push(created);
        })
    });

    let (status, _) = call_with_author(
        comment_app(pool.clone(), config.clone()),
        form_post(&format!("/comments/CmtPost/{post}"), "body=first"),
        author,
    )
    .await;
    assert_eq!(status, 200);

    let reports = seen.lock().expect("not poisoned").clone();
    assert_eq!(reports.len(), 1, "one create, one report");
    assert_eq!(reports[0].commentable_type, "CmtPost");
    assert_eq!(reports[0].parent_id, post);
    assert_eq!(reports[0].author_id, author);
    assert_eq!(reports[0].reply_to, None);
    // The body rides along, so a notifier need not read back the row it was
    // just told about.
    assert_eq!(reports[0].body, "first");
    assert!(reports[0].comment_id > 0);

    // A REJECTED comment must not be reported: the row does not exist.
    let (status, _) = call_with_author(
        comment_app(pool.clone(), config),
        form_post(&format!("/comments/CmtPost/{post}"), "body=+++"),
        author,
    )
    .await;
    assert_eq!(status, 200, "a blank body re-renders with an inline error");
    assert_eq!(
        seen.lock().expect("not poisoned").len(),
        1,
        "a rejected comment is not a created comment"
    );
}

/// `on_comment` is documented as the place for a database-backed side effect —
/// a notification that resolves a username, a search-index write. If the
/// request still held its connection while awaiting the hook, an app on a small
/// pool would deadlock instead of answering.
///
/// Same shape as the authorization test: a pool of exactly one, and a hook that
/// takes a connection from it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_creation_hook_runs_after_the_connection_is_released() {
    let (pool, _container) = setup_pool_with_size(1).await;
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;
    drop(conn);

    let hook_pool = pool.clone();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = std::sync::Arc::clone(&bodies);
    let config = autumn_web::commentable::CommentsConfig::default().on_comment(move |created| {
        let pool = hook_pool.clone();
        let recorder = std::sync::Arc::clone(&recorder);
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .expect("on_comment ran while the request still held the only connection");
            diesel::sql_query("SELECT 1")
                .execute(&mut conn)
                .await
                .expect("the hook's connection must be usable");
            recorder.lock().expect("not poisoned").push(created.body);
        })
    });

    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        call_with_author(
            comment_app(pool.clone(), config),
            form_post(
                &format!("/comments/CmtPost/{post}"),
                "body=++padded++&reply_to=",
            ),
            author,
        ),
    )
    .await
    .expect("a database-backed on_comment hook must not deadlock the pool");

    assert_eq!(status, 200, "{body}");
    assert_eq!(
        bodies.lock().expect("not poisoned").as_slice(),
        ["padded"],
        "the hook is told the body as ACCEPTED — add_comment trims before it \
         validates and inserts, so the submitted form value is not what was stored"
    );
}

/// `CommentAccess` tells the callback that `commentable_type` names a
/// registered model, so an unregistered one must 404 before the callback runs
/// — not hand arbitrary path strings to application policy code.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_unregistered_type_is_refused_before_the_authorization_hook() {
    let (pool, _container) = setup_pool().await;

    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&asked);
    let config = autumn_web::commentable::CommentsConfig::default().authorize(move |_access| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { true })
    });

    let (status, _) = call(
        comment_app(pool.clone(), config),
        get("/comments/NoSuchModel/1"),
    )
    .await;

    assert_eq!(status, 404);
    assert_eq!(
        // Fully qualified: diesel's `RunQueryDsl::load` is in scope here and
        // wins method resolution against the atomic's inherent `load`.
        std::sync::atomic::AtomicUsize::load(&asked, std::sync::atomic::Ordering::SeqCst),
        0,
        "an unregistered type must never reach the app's policy callback"
    );
}

/// A sharded repository routes every query through the tenant's shard, and it
/// does NOT require a `#[shard_key]` on the model — `sharding_across_tenants.rs`
/// has exactly that shape. Inferring shardedness from the model alone therefore
/// misses it, and the miss is silent: the router would serve the model from the
/// control pool, reading and writing the wrong database.
///
/// This pins the fact that the *repository* macro publishes shardedness, which
/// is the only place that knows it.
#[test]
fn a_sharded_repository_registers_its_model_even_without_a_shard_key() {
    assert!(
        autumn_web::commentable::model_has_sharded_repository("ShardedTenantPost"),
        "a #[repository(..., sharded)] model must be discoverable by name, so the \
         comment router's wiring-time guard can refuse to serve it"
    );
    assert!(
        !autumn_web::commentable::model_has_sharded_repository("CmtPost"),
        "an ordinary model must not be mistaken for a sharded one"
    );
}

/// A browser submits an *empty* hidden input as `reply_to=`. Reading that as
/// malformed would 422 every top-level comment posted from the widget.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_reads_an_empty_reply_to_as_a_top_level_comment() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let config = autumn_web::commentable::CommentsConfig::default();
    let app = comment_app(pool.clone(), config);
    let (status, body) = call_with_author(
        app,
        form_post(
            &format!("/comments/CmtPost/{post}"),
            "body=hello+there&reply_to=&return_to=",
        ),
        author,
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert!(body.contains("hello there"), "{body}");
    assert_eq!(
        row_count(&mut conn, "parent_id IS NULL").await,
        1,
        "an empty reply_to is a top-level comment, not a malformed one"
    );
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 1);
}

/// A `reply_to` that is not a number is a `400` — distinct from the `422` the
/// depth and cross-record checks give.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_rejects_a_malformed_reply_to() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let app = comment_app(
        pool.clone(),
        autumn_web::commentable::CommentsConfig::default(),
    );
    let (status, _) = call_with_author(
        app,
        form_post(&format!("/comments/CmtPost/{post}"), "body=hi&reply_to=abc"),
        author,
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(row_count(&mut conn, "1 = 1").await, 0);
}

/// A rejected body comes back as a rendered thread carrying the message, not
/// as a bare error htmx would refuse to swap.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_re_renders_with_an_inline_error_when_the_body_is_rejected() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;

    let app = comment_app(
        pool.clone(),
        autumn_web::commentable::CommentsConfig::default(),
    );
    let (status, body) = call_with_author(
        app,
        form_post(&format!("/comments/CmtPost/{post}"), "body=+++"),
        author,
    )
    .await;

    assert_eq!(status, 200, "the thread still renders: {body}");
    assert!(body.contains(r#"role="alert""#), "{body}");
    assert_eq!(row_count(&mut conn, "1 = 1").await, 0);
}

/// The no-JS round trip: a validated `return_to` redirects; a crafted one is
/// ignored rather than followed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn router_redirects_a_non_htmx_submit_only_to_a_relative_path() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let author = seed_user(&mut conn, "ada").await;
    let post = seed_one_col(&mut conn, "cmt_posts", "title", "hello").await;
    let config = autumn_web::commentable::CommentsConfig::default();

    let (status, _) = call_with_author(
        comment_app(pool.clone(), config.clone()),
        form_post(
            &format!("/comments/CmtPost/{post}"),
            "body=one&return_to=%2Fposts%2Fhello",
        ),
        author,
    )
    .await;
    assert_eq!(status, 303, "a relative path is honoured");

    let (status, _) = call_with_author(
        comment_app(pool.clone(), config),
        form_post(
            &format!("/comments/CmtPost/{post}"),
            // `/<TAB>/evil.example`: the URL parser strips the tab, so a
            // CR/LF-only check would turn this into `//evil.example`.
            "body=two&return_to=%2F%09%2Fevil.example",
        ),
        author,
    )
    .await;
    assert_eq!(
        status, 200,
        "a crafted return_to is ignored, not followed — the fragment is returned instead"
    );
    assert_eq!(counter(&mut conn, "cmt_posts", post).await, 2);
}

/// Drive a request as a signed-in author by pre-seeding the session the
/// handler reads `user_id` from.
async fn call_with_author(
    app: axum::Router,
    mut request: axum::http::Request<axum::body::Body>,
    author_id: i64,
) -> (u16, String) {
    // The `Session` extractor reads the request extension `SessionLayer`
    // would have installed, so seeding it here drives the real handler with a
    // signed-in author and no cookie plumbing.
    let mut data = std::collections::HashMap::new();
    data.insert("user_id".to_owned(), author_id.to_string());
    let session = autumn_web::session::Session::new_for_test("test-session".to_owned(), data);
    request.extensions_mut().insert(session);
    call(app, request).await
}
