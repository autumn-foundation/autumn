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

// ── #1738: model-declared `#[has_many(dependent = ...)]` cascade ──────────────
//
// The SAME cascade as `DepPost` above, but declared on the MODEL struct via
// `#[has_many(Child, dependent = <action>)]` instead of the repository
// attribute. The parent repository declares NO `dependent(...)`, so the cascade
// is driven at run time by `ModelDepPost::dependents()`, resolving each child
// repository through the `Pg{Child}Repository` naming convention (`DepComment`
// → `PgDepCommentRepository`, etc.). Reuses the existing child tables/repos via
// an explicit `fk = "post_id"` (the default would infer `model_dep_post_id`).
//
// `dependent = nullify` is intentionally omitted here: a nullify child has a
// NULLABLE foreign key, and `#[has_many]` preload codegen does not yet support
// a nullable child FK (it groups children into a `HashMap<i64, _>` keyed by the
// non-`Option` FK). That is a pre-existing preload limitation, independent of
// the cascade dispatch; tracked as a #1738 follow-up. destroy/delete_all/
// restrict all target non-null-FK children and cascade from the model side.
#[autumn_web::model(table = "dep_posts")]
#[has_many(DepComment, fk = "post_id", name = md_comments, dependent = destroy)]
#[has_many(DepVote, fk = "post_id", name = md_votes, dependent = delete_all)]
#[has_many(DepAward, fk = "post_id", name = md_awards, dependent = restrict)]
pub struct ModelDepPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(ModelDepPost, table = "dep_posts")]
pub trait ModelDepPostRepository {}

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

// ── #1739: three-level grandchild destroy graph (Post → Comment → Reply) ──────
//
// Both `Dep3Post` and `Dep3Comment` declare `dependent = destroy`. Deleting a
// post must recurse: destroy its comments AND each comment's replies, in one
// transaction, leaving zero orphaned grandchildren. This is the exact #1739
// acceptance graph.

diesel::table! {
    dep3_posts (id) {
        id -> Int8,
        title -> Text,
    }
}

#[autumn_web::model(table = "dep3_posts")]
pub struct Dep3Post {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(
    Dep3Post,
    table = "dep3_posts",
    dependent(PgDep3CommentRepository, fk = "post_id", on_delete = destroy)
)]
pub trait Dep3PostRepository {}

diesel::table! {
    dep3_comments (id) {
        id -> Int8,
        post_id -> Int8,
        body -> Text,
    }
}

#[autumn_web::model(table = "dep3_comments")]
pub struct Dep3Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
}

#[autumn_web::repository(
    Dep3Comment,
    table = "dep3_comments",
    dependent(PgDep3ReplyRepository, fk = "comment_id", on_delete = destroy)
)]
pub trait Dep3CommentRepository {}

diesel::table! {
    dep3_replies (id) {
        id -> Int8,
        comment_id -> Int8,
        body -> Text,
    }
}

#[autumn_web::model(table = "dep3_replies")]
pub struct Dep3Reply {
    #[id]
    pub id: i64,
    pub comment_id: i64,
    pub body: String,
}

#[autumn_web::repository(Dep3Reply, table = "dep3_replies")]
pub trait Dep3ReplyRepository {}

// ── #1739 cycle guard: a self-referential `destroy` dependent ─────────────────
//
// `dep_nodes.parent_id` references `dep_nodes.id`, and the repository declares a
// `dependent = destroy` back onto itself. This is the self-referential cycle the
// visited-set guard must survive: deleting a node destroys its children, which
// recurse into THEIR children, etc.  Even with cyclic data (a descendant whose
// FK points back at an ancestor) the traversal must terminate rather than
// looping forever or overflowing the (boxed) recursive future.

diesel::table! {
    dep_nodes (id) {
        id -> Int8,
        parent_id -> Nullable<Int8>,
        name -> Text,
    }
}

#[autumn_web::model(table = "dep_nodes")]
pub struct DepNode {
    #[id]
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
}

#[autumn_web::repository(
    DepNode,
    table = "dep_nodes",
    dependent(PgDepNodeRepository, fk = "parent_id", on_delete = destroy)
)]
pub trait DepNodeRepository {}

// ── #1740: a HOOKED parent that also declares a dependent ─────────────────────
//
// All the dependent-declaring parents above are hook-free, so they exercise the
// no-hooks `delete_many` codegen branch. This one has `hooks = ...` AND a
// dependent, so it monomorphizes the HOOKS-path bulk-cascade branch (#1740) —
// otherwise that branch would never be type-checked. Used only by the codegen
// surface test (no Docker graph test), so it reuses an existing child repo.

#[derive(Clone, Default)]
pub struct DepHookedPostHooks;

impl MutationHooks for DepHookedPostHooks {
    type Model = DepHookedPost;
    type NewModel = NewDepHookedPost;
    type UpdateModel = UpdateDepHookedPost;
}

diesel::table! {
    dep_hooked_posts (id) {
        id -> Int8,
        title -> Text,
    }
}

#[autumn_web::model(table = "dep_hooked_posts")]
pub struct DepHookedPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(
    DepHookedPost,
    table = "dep_hooked_posts",
    hooks = DepHookedPostHooks,
    dependent(PgDepCommentRepository, fk = "post_id", on_delete = destroy)
)]
pub trait DepHookedPostRepository {}

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
        "CREATE TABLE dep3_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE dep3_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES dep3_posts(id), body TEXT NOT NULL)",
        "CREATE TABLE dep3_replies (id BIGSERIAL PRIMARY KEY, comment_id BIGINT NOT NULL REFERENCES dep3_comments(id), body TEXT NOT NULL)",
        // Self-referential FK: parent_id references the same table. The FK is
        // DEFERRABLE INITIALLY DEFERRED so a *cyclic* component can be hard-deleted
        // within one transaction (the constraint is checked at COMMIT, by which
        // point every row in the cycle is gone). ON DELETE has no DB-level cascade
        // — the framework cascade is what must clear the whole component.
        "CREATE TABLE dep_nodes (id BIGSERIAL PRIMARY KEY, parent_id BIGINT REFERENCES dep_nodes(id) DEFERRABLE INITIALLY DEFERRED, name TEXT NOT NULL)",
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
    // #1739: the grandchild graph and the self-referential cycle repo both
    // monomorphize — proving the recursive (boxed-future) cascade codegen
    // type-checks, including a repository that cascades onto its own type.
    assert_is_fn(<PgDep3PostRepository as Dep3PostRepository>::delete_by_id);
    assert_is_fn(PgDep3CommentRepository::__autumn_apply_dependent_on_conn);
    assert_is_fn(PgDep3ReplyRepository::__autumn_apply_dependent_on_conn);
    assert_is_fn(<PgDepNodeRepository as DepNodeRepository>::delete_by_id);
    assert_is_fn(PgDepNodeRepository::__autumn_apply_dependent_on_conn);
    // #1740: the bulk `delete_many` path now carries the dependent cascade too;
    // monomorphize it (hooks-child parent + restrict-only parent) to prove the
    // bulk-cascade codegen type-checks without Docker.
    assert_is_fn(<PgDepPostRepository as DepPostRepository>::delete_many);
    assert_is_fn(<PgDepPostRestrictOnlyRepository as DepPostRestrictOnlyRepository>::delete_many);
    // Hooks-path bulk cascade branch (#1740): a hooked parent with a dependent.
    assert_is_fn(<PgDepHookedPostRepository as DepHookedPostRepository>::delete_many);
    assert_is_fn(<PgDepHookedPostRepository as DepHookedPostRepository>::delete_by_id);
    // #1738: the model-declared `#[has_many(dependent = ...)]` parent's
    // `delete_by_id` monomorphizes — proving the runtime `RuntimeDependentSpec`
    // dispatch (fn-pointer thunks into each child's cascade leaf executor, wired
    // from the model side with no repository-attribute `dependent(...)`)
    // type-checks against the same four cascade actions.
    assert_is_fn(<PgModelDepPostRepository as ModelDepPostRepository>::delete_by_id);
    // The model exposes its runtime dependent specs (inherent shadow of
    // `AutumnDependents::dependents`): destroy + delete_all + restrict.
    assert_is_fn(ModelDepPost::dependents);
    assert_eq!(ModelDepPost::dependents().len(), 3);
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

/// #1740: `delete_many` on parents with `dependent(...)` runs the SAME cascade
/// as `delete_by_id` — `destroy`/`delete_all`/`nullify` per child association —
/// for every parent, in one transaction, leaving no orphaned or FK-dangling rows.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_many_cascades_dependents() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    // Two parents, each with a comment (destroy), a vote (delete_all) and a
    // bookmark (nullify).
    for p in 1..=2 {
        diesel::sql_query(format!(
            "INSERT INTO dep_posts (id, title) VALUES ({p}, 'p{p}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO dep_comments (id, post_id, body) VALUES ({p}, {p}, 'c{p}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO dep_votes (id, post_id, value) VALUES ({p}, {p}, 1)"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO dep_bookmarks (id, post_id, label) VALUES ({p}, {p}, 'b{p}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
    }

    DESTROYED_COMMENTS.store(0, Ordering::SeqCst);

    let repo = PgDepPostRepository::with_pool_untracked(pool.clone());
    repo.delete_many(&[1, 2])
        .await
        .expect("bulk cascade delete must not FK-error");

    assert_eq!(
        count(&mut conn, "SELECT COUNT(*) AS n FROM dep_posts").await,
        0,
        "both parents deleted"
    );
    assert_eq!(
        count(&mut conn, "SELECT COUNT(*) AS n FROM dep_comments").await,
        0,
        "#1740: destroy children cascaded on the bulk path"
    );
    assert_eq!(
        count(&mut conn, "SELECT COUNT(*) AS n FROM dep_votes").await,
        0,
        "#1740: delete_all children cascaded on the bulk path"
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_bookmarks WHERE post_id IS NOT NULL"
        )
        .await,
        0,
        "#1740: nullify children detached on the bulk path"
    );
    // Both destroy children fired their before_delete hook during the bulk cascade.
    assert_eq!(AtomicUsize::load(&DESTROYED_COMMENTS, Ordering::SeqCst), 2);
}

/// #1740: a `restrict` dependent still blocks a bulk `delete_many` with a typed
/// 409 and rolls the whole transaction back (no parent or child removed).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_many_restrict_blocks_and_rolls_back() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_posts (id, title) VALUES (7, 'guarded'), (8, 'free')")
        .execute(&mut conn)
        .await
        .unwrap();
    // Only post 7 has a restrict child; the bulk delete of [7, 8] must still be
    // blocked and rolled back wholesale (post 8 survives too).
    diesel::sql_query("INSERT INTO dep_awards (id, post_id, name) VALUES (1, 7, 'gold')")
        .execute(&mut conn)
        .await
        .unwrap();

    let repo = PgDepPostRestrictOnlyRepository::with_pool_untracked(pool.clone());
    let err = repo
        .delete_many(&[7, 8])
        .await
        .expect_err("restrict child must block the bulk delete");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::CONFLICT,
        "a blocking restrict must surface a 409 on the bulk path"
    );

    // Whole transaction rolled back: both parents and the child survive.
    assert_eq!(
        count(&mut conn, "SELECT COUNT(*) AS n FROM dep_posts").await,
        2,
        "both parents survive a blocked bulk delete"
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_awards WHERE post_id = 7"
        )
        .await,
        1,
        "the restrict child survives"
    );
}

/// #1739: deleting a `Dep3Post` with `dependent(Comment, destroy)` where
/// `Comment` has `dependent(Reply, destroy)` must recurse — leaving zero
/// `Dep3Reply` (grandchild) rows and zero orphaned comments, in one
/// transaction, with no FK error.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deleting_parent_recurses_into_grandchildren() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep3_posts (id, title) VALUES (1, 'p')")
        .execute(&mut conn)
        .await
        .unwrap();
    // 2 comments, each with 2 replies → 4 grandchildren.
    for c in 1..=2 {
        diesel::sql_query(format!(
            "INSERT INTO dep3_comments (id, post_id, body) VALUES ({c}, 1, 'c{c}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        for r in 1..=2 {
            let rid = c * 10 + r;
            diesel::sql_query(format!(
                "INSERT INTO dep3_replies (id, comment_id, body) VALUES ({rid}, {c}, 'r{rid}')"
            ))
            .execute(&mut conn)
            .await
            .unwrap();
        }
    }

    let repo = PgDep3PostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(1)
        .await
        .expect("recursive cascade must not FK-error");

    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep3_posts WHERE id = 1"
        )
        .await,
        0,
        "parent gone"
    );
    assert_eq!(
        count(&mut conn, "SELECT COUNT(*) AS n FROM dep3_comments").await,
        0,
        "all comments (children) destroyed"
    );
    assert_eq!(
        count(&mut conn, "SELECT COUNT(*) AS n FROM dep3_replies").await,
        0,
        "#1739: all replies (grandchildren) must be recursively destroyed — zero left"
    );
}

/// #1739 cycle guard: a self-referential `destroy` cascade over a cyclic graph
/// (a descendant whose `parent_id` points back at an ancestor) must terminate,
/// not infinite-loop, and remove the whole reachable component in one tx.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn self_referential_destroy_terminates_on_cycle() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    // Chain 1 → 2 → 3, then create a cycle by pointing 1's parent at 3.
    // (Insert with NULL parents first to satisfy the FK, then wire the edges.)
    for id in 1..=3 {
        diesel::sql_query(format!(
            "INSERT INTO dep_nodes (id, parent_id, name) VALUES ({id}, NULL, 'n{id}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
    }
    // 2's parent = 1, 3's parent = 2, 1's parent = 3 → a 3-cycle.
    diesel::sql_query("UPDATE dep_nodes SET parent_id = 1 WHERE id = 2")
        .execute(&mut conn)
        .await
        .unwrap();
    diesel::sql_query("UPDATE dep_nodes SET parent_id = 2 WHERE id = 3")
        .execute(&mut conn)
        .await
        .unwrap();
    diesel::sql_query("UPDATE dep_nodes SET parent_id = 3 WHERE id = 1")
        .execute(&mut conn)
        .await
        .unwrap();

    let repo = PgDepNodeRepository::with_pool_untracked(pool.clone());
    // Must return (not hang / overflow). The visited-set guard breaks the cycle.
    repo.delete_by_id(1)
        .await
        .expect("self-referential cascade over a cycle must terminate without error");

    // The entire cyclic component is reachable from node 1 and removed.
    assert_eq!(
        count(&mut conn, "SELECT COUNT(*) AS n FROM dep_nodes").await,
        0,
        "#1739 cycle guard: the whole reachable cyclic component is destroyed, no rows left"
    );
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

/// #1369 restrict ordering: `DepPost` declares `destroy(Comment)` BEFORE
/// `restrict(Award)`, and `DepComment`'s `before_delete` increments a counter.
/// When a restrict child (award) has rows, `delete_by_id` must return 409 and
/// roll back WITHOUT ever firing the destroy child's `before_delete` — the
/// restrict probe runs before any mutating dependent. Asserts: 409, the destroy
/// hook counter did NOT move, and nothing was deleted.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restrict_probes_before_destroy_fires_no_child_hooks() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_posts (id, title) VALUES (21, 'guarded')")
        .execute(&mut conn)
        .await
        .unwrap();
    for i in 1..=3 {
        diesel::sql_query(format!(
            "INSERT INTO dep_comments (id, post_id, body) VALUES ({i}, 21, 'c{i}')"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
    }
    // A restrict child (award) with a row → the delete must be blocked.
    diesel::sql_query("INSERT INTO dep_awards (id, post_id, name) VALUES (1, 21, 'gold')")
        .execute(&mut conn)
        .await
        .unwrap();

    DESTROYED_COMMENTS.store(0, Ordering::SeqCst);

    let repo = PgDepPostRepository::with_pool_untracked(pool.clone());
    let err = repo
        .delete_by_id(21)
        .await
        .expect_err("restrict child with rows must block the delete");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::CONFLICT,
        "a blocking restrict must surface a 409"
    );

    // The destroy child's before_delete hook must NOT have fired — the restrict
    // probe runs before any mutating dependent, so no non-transactional side
    // effect happened for a delete that never committed.
    assert_eq!(
        AtomicUsize::load(&DESTROYED_COMMENTS, Ordering::SeqCst),
        0,
        "restrict must be probed before destroy so the child before_delete hook never fires"
    );

    // Nothing was deleted (whole tx rolled back).
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_posts WHERE id = 21"
        )
        .await,
        1,
        "the parent must survive a blocked delete"
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_comments WHERE post_id = 21"
        )
        .await,
        3,
        "the destroy child rows must be untouched"
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_awards WHERE post_id = 21"
        )
        .await,
        1,
        "the restrict child row must survive"
    );
}

// ── #1738: model-declared cascade produces the same behavior ──────────────────

/// #1738 primary AC: `#[has_many(Child, dependent = <action>)]` declared on the
/// MODEL (no repository-attribute `dependent(...)`) produces the same
/// transactional cascade the repository-attribute form produces. Deleting a
/// `ModelDepPost` destroys its comments (firing their hooks) and bulk-deletes
/// its votes — driven entirely by the runtime `ModelDepPost::dependents()`
/// dispatch resolving `PgDepCommentRepository` / `PgDepVoteRepository` by
/// convention. (No awards are inserted, since the model also declares
/// `restrict(DepAward)`, exercised separately below.)
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn model_declared_dependent_cascades_like_repository_attribute() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_posts (id, title) VALUES (31, 'model-side')")
        .execute(&mut conn)
        .await
        .unwrap();
    for i in 1..=3 {
        diesel::sql_query(format!(
            "INSERT INTO dep_comments (id, post_id, body) VALUES ({}, 31, 'c{i}')",
            100 + i
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO dep_votes (id, post_id, value) VALUES ({}, 31, 1)",
            100 + i
        ))
        .execute(&mut conn)
        .await
        .unwrap();
    }

    DESTROYED_COMMENTS.store(0, Ordering::SeqCst);

    let repo = PgModelDepPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(31)
        .await
        .expect("model-declared cascade delete must not FK-error");

    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_posts WHERE id = 31"
        )
        .await,
        0,
        "parent gone"
    );
    // destroy: comments hard-deleted, and each fired its before_delete hook.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_comments WHERE post_id = 31"
        )
        .await,
        0,
        "destroy child rows removed via the model-declared cascade"
    );
    assert_eq!(
        AtomicUsize::load(&DESTROYED_COMMENTS, Ordering::SeqCst),
        3,
        "each destroyed comment fired its before_delete hook (child lifecycle honored)"
    );
    // delete_all: votes bulk-removed.
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_votes WHERE post_id = 31"
        )
        .await,
        0,
        "delete_all child rows removed via the model-declared cascade"
    );
}

/// #1738: model-declared `restrict` preserves the typed 409 + rollback, exactly
/// as the repository-attribute form does. An award (restrict child) blocks the
/// `ModelDepPost` delete, and the transaction rolls back leaving every row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn model_declared_restrict_blocks_with_conflict_and_rolls_back() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    diesel::sql_query("INSERT INTO dep_posts (id, title) VALUES (32, 'guarded-model')")
        .execute(&mut conn)
        .await
        .unwrap();
    diesel::sql_query("INSERT INTO dep_comments (id, post_id, body) VALUES (201, 32, 'c')")
        .execute(&mut conn)
        .await
        .unwrap();
    diesel::sql_query("INSERT INTO dep_awards (id, post_id, name) VALUES (11, 32, 'gold')")
        .execute(&mut conn)
        .await
        .unwrap();

    DESTROYED_COMMENTS.store(0, Ordering::SeqCst);

    let repo = PgModelDepPostRepository::with_pool_untracked(pool.clone());
    let err = repo
        .delete_by_id(32)
        .await
        .expect_err("model-declared restrict child must block the delete");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::CONFLICT,
        "a blocking model-declared restrict must surface a 409"
    );
    // Restrict probed before destroy → the comment's before_delete never fired.
    assert_eq!(
        AtomicUsize::load(&DESTROYED_COMMENTS, Ordering::SeqCst),
        0,
        "restrict must be probed before destroy in the model-declared cascade too"
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_posts WHERE id = 32"
        )
        .await,
        1,
        "the parent must survive a blocked model-declared delete"
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM dep_comments WHERE post_id = 32"
        )
        .await,
        1,
        "the destroy child rows must be untouched after a blocked delete"
    );
}
