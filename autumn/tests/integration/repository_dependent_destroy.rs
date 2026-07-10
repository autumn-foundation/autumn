//! Database-level integration tests for `dependent` destroy/nullify/restrict
//! on repository associations (issue #1369).
//!
//! The seeded-graph tests **require Docker** (testcontainers) and are
//! `#[ignore]`d by default. Two non-ignored tests prove the generated method
//! surface exists (the `#1592`-style guard) without a live database, so
//! `cargo test -p autumn --no-run --features db` type-checks every branch —
//! plain `destroy`/`delete_all`/`nullify`/`restrict`, a soft-delete-aware
//! `destroy`, and a hook-firing `destroy` — even where Docker is unavailable.
//!
//! Graph under test (the reddit-clone shape from the issue):
//!
//! ```text
//!   dep_posts (parent)
//!     - dep_comments   dependent = destroy    (per-row delete path)
//!     - dep_votes      dependent = delete_all  (bulk hard delete)
//!     - dep_bookmarks  dependent = nullify     (FK set to NULL)
//!     - dep_awards     dependent = restrict    (409 if any exist)
//! ```

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Parent ────────────────────────────────────────────────────────────────

diesel::table! {
    dep_posts (id) {
        id -> Int8,
        title -> Text,
    }
}

#[autumn_web::model(table = "dep_posts")]
pub struct DepPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(
    DepPost,
    table = "dep_posts",
    dependent(PgDepCommentRepository, fk = "post_id", on_delete = destroy),
    dependent(PgDepVoteRepository, fk = "post_id", on_delete = delete_all),
    dependent(PgDepBookmarkRepository, fk = "post_id", on_delete = nullify),
    dependent(PgDepAwardRepository, fk = "post_id", on_delete = restrict)
)]
pub trait DepPostRepository {}

// A second parent whose only dependent is `restrict`, to exercise the blocking
// path in isolation.
#[autumn_web::model(table = "dep_posts")]
pub struct DepPostRestrictOnly {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(
    DepPostRestrictOnly,
    table = "dep_posts",
    dependent(PgDepAwardRepository, fk = "post_id", on_delete = restrict)
)]
pub trait DepPostRestrictOnlyRepository {}

// ── Children ────────────────────────────────────────────────────────────────

diesel::table! {
    dep_comments (id) {
        id -> Int8,
        post_id -> Int8,
        body -> Text,
    }
}

pub static DESTROYED_COMMENTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Default)]
pub struct DepCommentHooks;

impl MutationHooks for DepCommentHooks {
    type Model = DepComment;
    type NewModel = NewDepComment;
    type UpdateModel = UpdateDepComment;

    async fn before_delete(
        &self,
        _ctx: &mut MutationContext,
        _record: &DepComment,
    ) -> AutumnResult<()> {
        // AC4: proves the child's lifecycle hook fires during a cascade.
        DESTROYED_COMMENTS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[autumn_web::model(table = "dep_comments")]
pub struct DepComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
}

#[autumn_web::repository(DepComment, table = "dep_comments", hooks = DepCommentHooks)]
pub trait DepCommentRepository {}

diesel::table! {
    dep_votes (id) {
        id -> Int8,
        post_id -> Int8,
        value -> Int4,
    }
}

#[autumn_web::model(table = "dep_votes")]
pub struct DepVote {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub value: i32,
}

#[autumn_web::repository(DepVote, table = "dep_votes")]
pub trait DepVoteRepository {}

diesel::table! {
    dep_bookmarks (id) {
        id -> Int8,
        post_id -> Nullable<Int8>,
        label -> Text,
    }
}

#[autumn_web::model(table = "dep_bookmarks")]
pub struct DepBookmark {
    #[id]
    pub id: i64,
    pub post_id: Option<i64>,
    pub label: String,
}

#[autumn_web::repository(DepBookmark, table = "dep_bookmarks")]
pub trait DepBookmarkRepository {}

diesel::table! {
    dep_awards (id) {
        id -> Int8,
        post_id -> Int8,
        name -> Text,
    }
}

#[autumn_web::model(table = "dep_awards")]
pub struct DepAward {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub name: String,
}

#[autumn_web::repository(DepAward, table = "dep_awards")]
pub trait DepAwardRepository {}

// ── AC3: both-soft destroy graph (soft parent + soft children) ────────────────
//
// The parent is `#[soft_delete]` too, so `delete_by_id` soft-deletes the parent
// (row remains, FK stays valid) and the cascade soft-deletes the children — the
// AC3 "parent soft-deleted, cascade still runs, live graph stays consistent"
// case, satisfiable against real Postgres.

diesel::table! {
    dep_soft_posts (id) {
        id -> Int8,
        title -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "dep_soft_posts")]
pub struct DepSoftPost {
    #[id]
    pub id: i64,
    pub title: String,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    DepSoftPost,
    table = "dep_soft_posts",
    soft_delete,
    dependent(PgDepSoftCommentRepository, fk = "post_id", on_delete = destroy)
)]
pub trait DepSoftPostRepository {}

diesel::table! {
    dep_soft_comments (id) {
        id -> Int8,
        post_id -> Int8,
        body -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "dep_soft_comments")]
pub struct DepSoftComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(DepSoftComment, table = "dep_soft_comments", soft_delete)]
pub trait DepSoftCommentRepository {}

// ── #1369 P1: hard parent + soft-delete child destroy graph ───────────────────
//
// A NON-soft parent that `destroy`s a `#[soft_delete]` child. Because the parent
// is hard-deleted, the child must be HARD-deleted too (not merely soft-deleted),
// otherwise a NOT NULL FK to the removed parent would be violated and the row
// would be a semantic orphan. Locks in the P1 fix (child follows parent kind).

diesel::table! {
    dep_hard_posts (id) {
        id -> Int8,
        title -> Text,
    }
}

#[autumn_web::model(table = "dep_hard_posts")]
pub struct DepHardPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(
    DepHardPost,
    table = "dep_hard_posts",
    dependent(PgDepHardSoftCommentRepository, fk = "post_id", on_delete = destroy)
)]
pub trait DepHardPostRepository {}

diesel::table! {
    dep_hard_soft_comments (id) {
        id -> Int8,
        post_id -> Int8,
        body -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "dep_hard_soft_comments")]
pub struct DepHardSoftComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(DepHardSoftComment, table = "dep_hard_soft_comments", soft_delete)]
pub trait DepHardSoftCommentRepository {}

// ── Row-count helpers ─────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

async fn count(conn: &mut AsyncPgConnection, sql: &str) -> i64 {
    let row: CountRow = diesel::sql_query(sql)
        .get_result::<CountRow>(conn)
        .await
        .expect("count query");
    row.n
}

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
    let pool = Pool::builder(manager).max_size(8).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    for stmt in [
        "CREATE TABLE dep_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE dep_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES dep_posts(id), body TEXT NOT NULL)",
        "CREATE TABLE dep_votes (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES dep_posts(id), value INT NOT NULL)",
        "CREATE TABLE dep_bookmarks (id BIGSERIAL PRIMARY KEY, post_id BIGINT REFERENCES dep_posts(id), label TEXT NOT NULL)",
        "CREATE TABLE dep_awards (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES dep_posts(id), name TEXT NOT NULL)",
        "CREATE TABLE dep_soft_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, deleted_at TIMESTAMP NULL)",
        "CREATE TABLE dep_soft_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES dep_soft_posts(id), body TEXT NOT NULL, deleted_at TIMESTAMP NULL)",
        "CREATE TABLE dep_hard_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE dep_hard_soft_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES dep_hard_posts(id), body TEXT NOT NULL, deleted_at TIMESTAMP NULL)",
    ] {
        diesel::sql_query(stmt)
            .execute(&mut conn)
            .await
            .unwrap_or_else(|e| panic!("failed to create schema ({stmt}): {e}"));
    }

    (pool, container)
}

// ── Non-ignored: generated surface exists without a live database ─────────────

/// AC1: proves the `dependent(...)` codegen ran on every variant — the delete
/// path (`delete_by_id`) and the connection-taking cascade helper monomorphize
/// as nameable function items without needing Docker.
#[test]
fn dependent_repository_surface_is_generated() {
    fn assert_is_fn<F>(_f: F) {}
    assert_is_fn(<PgDepPostRepository as DepPostRepository>::delete_by_id);
    assert_is_fn(<PgDepPostRestrictOnlyRepository as DepPostRestrictOnlyRepository>::delete_by_id);
    assert_is_fn(PgDepCommentRepository::__autumn_apply_dependent_on_conn);
    assert_is_fn(PgDepVoteRepository::__autumn_apply_dependent_on_conn);
    assert_is_fn(PgDepBookmarkRepository::__autumn_apply_dependent_on_conn);
    assert_is_fn(PgDepAwardRepository::__autumn_apply_dependent_on_conn);
    assert_is_fn(PgDepSoftCommentRepository::__autumn_apply_dependent_on_conn);
    assert_is_fn(<PgDepSoftPostRepository as DepSoftPostRepository>::delete_by_id);
    assert_is_fn(<PgDepHardPostRepository as DepHardPostRepository>::delete_by_id);
    assert_is_fn(PgDepHardSoftCommentRepository::__autumn_apply_dependent_on_conn);
}

// ── Tests (require Docker) ────────────────────────────────────────────────────

/// AC1/AC2/AC4/AC6 + success metric: deleting a post with N comments + votes +
/// bookmarks leaves **zero** orphaned/dangling child rows and **zero** FK
/// errors, in one transaction. Comments are destroyed via the repository delete
/// path (firing hooks), votes are bulk-deleted, bookmarks are nullified.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deleting_parent_cascades_and_leaves_no_orphans() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_posts (id, title) VALUES (1, 'hello')")
        .execute(&mut conn)
        .await
        .unwrap();
    for i in 1..=3 {
        diesel::sql_query(format!(
            "INSERT INTO dep_comments (id, post_id, body) VALUES ({i}, 1, 'c{i}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO dep_votes (id, post_id, value) VALUES ({i}, 1, 1)"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO dep_bookmarks (id, post_id, label) VALUES ({i}, 1, 'b{i}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
    }

    DESTROYED_COMMENTS.store(0, Ordering::SeqCst);

    let repo = PgDepPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(1)
        .await
        .expect("cascade delete must not FK-error");

    // Parent gone.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_posts WHERE id = 1"
        )
        .await,
        0
    );
    // destroy: comments hard-deleted (not soft-delete repo) → 0 orphans.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_comments WHERE post_id = 1"
        )
        .await,
        0
    );
    // delete_all: votes gone.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_votes WHERE post_id = 1"
        )
        .await,
        0
    );
    // nullify: bookmarks survive but detached.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_bookmarks WHERE post_id = 1"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_bookmarks WHERE post_id IS NULL"
        )
        .await,
        3
    );
    // AC4: each destroyed comment fired its before_delete hook.
    // (UFCS: `RunQueryDsl` is in scope, so disambiguate from diesel's `.load`.)
    assert_eq!(AtomicUsize::load(&DESTROYED_COMMENTS, Ordering::SeqCst), 3);
}

/// AC5: `dependent = restrict` blocks the delete with a typed 409 Conflict when
/// children exist, and rolls back (parent survives).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restrict_blocks_delete_with_conflict_and_rolls_back() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_posts (id, title) VALUES (5, 'guarded')")
        .execute(&mut conn)
        .await
        .unwrap();
    diesel::sql_query("INSERT INTO dep_awards (id, post_id, name) VALUES (1, 5, 'gold')")
        .execute(&mut conn)
        .await
        .unwrap();

    let repo = PgDepPostRestrictOnlyRepository::with_pool_untracked(pool.clone());
    let err = repo
        .delete_by_id(5)
        .await
        .expect_err("restrict must block the delete");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::CONFLICT,
        "restrict must surface a 409, not a 500"
    );

    // Rolled back: parent and child both survive.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_posts WHERE id = 5"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_awards WHERE post_id = 5"
        )
        .await,
        1
    );
}

/// AC3 (both-soft): a `#[soft_delete]` parent with a `dependent = destroy` on a
/// `#[soft_delete]` child. `delete_by_id` soft-deletes the parent (row remains,
/// FK stays valid) and the cascade soft-deletes the children — all rows remain
/// with `deleted_at` set, no FK error. This is the AC3 "parent soft-deleted,
/// cascade still runs so the live graph stays consistent" scenario.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn destroy_soft_parent_soft_deletes_soft_children() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_soft_posts (id, title) VALUES (9, 'soft')")
        .execute(&mut conn)
        .await
        .unwrap();
    for i in 1..=2 {
        diesel::sql_query(format!(
            "INSERT INTO dep_soft_comments (id, post_id, body) VALUES ({i}, 9, 's{i}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
    }

    let repo = PgDepSoftPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(9).await.expect("soft cascade delete");

    // Parent row remains (soft-deleted, FK targets stay valid).
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_soft_posts WHERE id = 9 AND deleted_at IS NOT NULL"
        )
        .await,
        1,
        "a soft-delete parent row must remain with deleted_at set"
    );
    // Children remain (soft-deleted, not hard-deleted) with deleted_at set.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_soft_comments WHERE post_id = 9"
        )
        .await,
        2,
        "soft-delete children must not be hard-deleted when the parent is soft-deleted"
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_soft_comments WHERE post_id = 9 AND deleted_at IS NOT NULL"
        )
        .await,
        2,
        "each child must be soft-deleted (deleted_at set)"
    );
}

/// #1369 P1 (hard parent + soft child, incl. a PRE-soft-deleted child): a
/// NON-soft parent that `destroy`s a `#[soft_delete]` child. Because the parent
/// is hard-deleted, the children must be HARD-deleted too (rows gone) — even a
/// child that was ALREADY soft-deleted (`deleted_at` set) whose NOT NULL FK still
/// points at the parent. The parent-soft-gated live filter is dropped for a hard
/// parent, so the cascade selects every physically-present child. Assert zero
/// surviving children, zero FK errors, parent gone.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn destroy_hard_parent_hard_deletes_soft_children() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_hard_posts (id, title) VALUES (11, 'hard')")
        .execute(&mut conn)
        .await
        .unwrap();
    for i in 1..=3 {
        diesel::sql_query(format!(
            "INSERT INTO dep_hard_soft_comments (id, post_id, body) VALUES ({i}, 11, 'h{i}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
    }
    // Pre-soft-delete one child: its deleted_at is already set, but its NOT NULL
    // FK still references the parent. This row must NOT be skipped by the live
    // filter on a hard-parent cascade, or the parent DELETE would FK-fail.
    diesel::sql_query("UPDATE dep_hard_soft_comments SET deleted_at = now() WHERE id = 2")
        .execute(&mut conn)
        .await
        .unwrap();

    let repo = PgDepHardPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(11)
        .await
        .expect("hard-parent cascade must not FK-error, even with a pre-soft-deleted child");

    // Parent gone.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_hard_posts WHERE id = 11"
        )
        .await,
        0
    );
    // All children HARD-deleted (rows gone), including the pre-soft-deleted one.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_hard_soft_comments WHERE post_id = 11"
        )
        .await,
        0,
        "a hard-deleted parent must hard-delete every physically-present soft-delete child (incl. pre-soft-deleted), leaving no orphan and no FK error"
    );
}
