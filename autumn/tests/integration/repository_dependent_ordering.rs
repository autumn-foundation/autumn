//! Hook ordering and restrict hoisting for dependent cascades (issue #2427).
//!
//! Contract under test, identical for `delete_by_id` and `delete_many`:
//!
//! 1. Every `restrict` in the cascade tree is probed before any hook fires.
//!    A blocked delete fires no hook.
//! 2. A parent's `before_delete` runs before any hook of its children.
//!    A parent veto fires no child hook.
//! 3. A child's `after_delete_commit` runs once, after commit.
//!
//! **Requires Docker** (testcontainers). Each test starts its own database and
//! uses its own id range, because hook logs are process-wide.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::sync::Mutex;

use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const COMMIT_HOOK_UP: &str = include_str!(
    "../../repository_commit_hook_migrations/20260515000000_create_repository_commit_hook_queue/up.sql"
);

/// Process-wide hook log. Entries read `<tag>:<id>`.
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log_push(entry: String) {
    LOG.lock().expect("log lock").push(entry);
}

/// Entries tagged `prefix*` whose id lies in `lo..=hi`, in firing order.
/// Graphs share one log, so a test names its graph's tag prefix.
fn log_range(prefix: &str, lo: i64, hi: i64) -> Vec<String> {
    LOG.lock()
        .expect("log lock")
        .iter()
        .filter(|entry| {
            entry.starts_with(prefix)
                && entry
                    .rsplit(':')
                    .next()
                    .and_then(|id| id.parse::<i64>().ok())
                    .is_some_and(|id| (lo..=hi).contains(&id))
        })
        .cloned()
        .collect()
}

macro_rules! logging_hooks {
    ($hooks:ident, $model:ident, $new:ident, $update:ident, $tag:literal) => {
        #[derive(Clone, Default)]
        pub struct $hooks;

        impl MutationHooks for $hooks {
            type Model = $model;
            type NewModel = $new;
            type UpdateModel = $update;

            async fn before_delete(
                &self,
                _ctx: &mut MutationContext,
                record: &$model,
            ) -> AutumnResult<()> {
                log_push(format!("{}:{}", $tag, record.id));
                Ok(())
            }
        }
    };
}

// ── Graph G: repository-attribute cascade ───────────────────────────────────
//
//   g_posts ─ destroy → g_comments (hooked)
//           └ destroy → g_pages (hooked) ─ destroy → g_sections ─ restrict → g_locks

diesel::table! { g_posts (id) { id -> Int8, title -> Text, } }
diesel::table! { g_comments (id) { id -> Int8, post_id -> Int8, } }
diesel::table! { g_pages (id) { id -> Int8, post_id -> Int8, } }
diesel::table! { g_sections (id) { id -> Int8, page_id -> Int8, } }
diesel::table! { g_locks (id) { id -> Int8, section_id -> Int8, } }

#[autumn_web::model(table = "g_posts")]
pub struct GPost {
    #[id]
    pub id: i64,
    pub title: String,
}
#[autumn_web::model(table = "g_comments")]
pub struct GComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}
#[autumn_web::model(table = "g_pages")]
pub struct GPage {
    #[id]
    pub id: i64,
    pub post_id: i64,
}
#[autumn_web::model(table = "g_sections")]
pub struct GSection {
    #[id]
    pub id: i64,
    pub page_id: i64,
}
#[autumn_web::model(table = "g_locks")]
pub struct GLock {
    #[id]
    pub id: i64,
    pub section_id: i64,
}

logging_hooks!(
    GCommentHooks,
    GComment,
    NewGComment,
    UpdateGComment,
    "gcomment"
);
logging_hooks!(GPageHooks, GPage, NewGPage, UpdateGPage, "gpage");

#[autumn_web::repository(
    GPost,
    table = "g_posts",
    dependent(PgGCommentRepository, fk = "post_id", on_delete = destroy),
    dependent(PgGPageRepository, fk = "post_id", on_delete = destroy)
)]
pub trait GPostRepository {}

#[autumn_web::repository(GComment, table = "g_comments", hooks = GCommentHooks)]
pub trait GCommentRepository {}

#[autumn_web::repository(
    GPage,
    table = "g_pages",
    hooks = GPageHooks,
    dependent(PgGSectionRepository, fk = "page_id", on_delete = destroy)
)]
pub trait GPageRepository {}

#[autumn_web::repository(
    GSection,
    table = "g_sections",
    dependent(PgGLockRepository, fk = "section_id", on_delete = restrict)
)]
pub trait GSectionRepository {}

#[autumn_web::repository(GLock, table = "g_locks")]
pub trait GLockRepository {}

// ── Graph H: the same shape, declared on the models ─────────────────────────

diesel::table! { h_posts (id) { id -> Int8, title -> Text, } }
diesel::table! { h_comments (id) { id -> Int8, post_id -> Int8, } }
diesel::table! { h_pages (id) { id -> Int8, post_id -> Int8, } }
diesel::table! { h_sections (id) { id -> Int8, page_id -> Int8, } }
diesel::table! { h_locks (id) { id -> Int8, section_id -> Int8, } }

#[autumn_web::model(table = "h_posts")]
#[has_many(HComment, fk = "post_id", dependent = destroy)]
#[has_many(HPage, fk = "post_id", dependent = destroy)]
pub struct HPost {
    #[id]
    pub id: i64,
    pub title: String,
}
#[autumn_web::model(table = "h_comments")]
pub struct HComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}
#[autumn_web::model(table = "h_pages")]
#[has_many(HSection, fk = "page_id", dependent = destroy)]
pub struct HPage {
    #[id]
    pub id: i64,
    pub post_id: i64,
}
#[autumn_web::model(table = "h_sections")]
#[has_many(HLock, fk = "section_id", dependent = restrict)]
pub struct HSection {
    #[id]
    pub id: i64,
    pub page_id: i64,
}
#[autumn_web::model(table = "h_locks")]
pub struct HLock {
    #[id]
    pub id: i64,
    pub section_id: i64,
}

logging_hooks!(
    HCommentHooks,
    HComment,
    NewHComment,
    UpdateHComment,
    "hcomment"
);
logging_hooks!(HPageHooks, HPage, NewHPage, UpdateHPage, "hpage");

#[autumn_web::repository(HPost, table = "h_posts")]
pub trait HPostRepository {}
#[autumn_web::repository(HComment, table = "h_comments", hooks = HCommentHooks)]
pub trait HCommentRepository {}
#[autumn_web::repository(HPage, table = "h_pages", hooks = HPageHooks)]
pub trait HPageRepository {}
#[autumn_web::repository(HSection, table = "h_sections")]
pub trait HSectionRepository {}
#[autumn_web::repository(HLock, table = "h_locks")]
pub trait HLockRepository {}

// ── Graph O: hooked parent, hooked child (repository-attribute) ─────────────

diesel::table! { o_posts (id) { id -> Int8, title -> Text, } }
diesel::table! { o_comments (id) { id -> Int8, post_id -> Int8, } }

#[autumn_web::model(table = "o_posts")]
pub struct OPost {
    #[id]
    pub id: i64,
    pub title: String,
}
#[autumn_web::model(table = "o_comments")]
pub struct OComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

/// Logs the hook, and vetoes a post titled `veto`.
#[derive(Clone, Default)]
pub struct OPostHooks;

impl MutationHooks for OPostHooks {
    type Model = OPost;
    type NewModel = NewOPost;
    type UpdateModel = UpdateOPost;

    async fn before_delete(&self, _ctx: &mut MutationContext, record: &OPost) -> AutumnResult<()> {
        log_push(format!("opost:{}", record.id));
        if record.title == "veto" {
            return Err(autumn_web::AutumnError::conflict_msg("parent veto"));
        }
        Ok(())
    }
}
logging_hooks!(
    OCommentHooks,
    OComment,
    NewOComment,
    UpdateOComment,
    "ocomment"
);

#[autumn_web::repository(
    OPost,
    table = "o_posts",
    hooks = OPostHooks,
    dependent(PgOCommentRepository, fk = "post_id", on_delete = destroy)
)]
pub trait OPostRepository {}
#[autumn_web::repository(OComment, table = "o_comments", hooks = OCommentHooks)]
pub trait OCommentRepository {}

// ── Graph P: hooked parent, hooked child (model-declared) ───────────────────

diesel::table! { p_posts (id) { id -> Int8, title -> Text, } }
diesel::table! { p_comments (id) { id -> Int8, post_id -> Int8, } }

#[autumn_web::model(table = "p_posts")]
#[has_many(PComment, fk = "post_id", dependent = destroy)]
pub struct PPost {
    #[id]
    pub id: i64,
    pub title: String,
}
#[autumn_web::model(table = "p_comments")]
pub struct PComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

#[derive(Clone, Default)]
pub struct PPostHooks;

impl MutationHooks for PPostHooks {
    type Model = PPost;
    type NewModel = NewPPost;
    type UpdateModel = UpdatePPost;

    async fn before_delete(&self, _ctx: &mut MutationContext, record: &PPost) -> AutumnResult<()> {
        log_push(format!("ppost:{}", record.id));
        if record.title == "veto" {
            return Err(autumn_web::AutumnError::conflict_msg("parent veto"));
        }
        Ok(())
    }
}
logging_hooks!(
    PCommentHooks,
    PComment,
    NewPComment,
    UpdatePComment,
    "pcomment"
);

#[autumn_web::repository(PPost, table = "p_posts", hooks = PPostHooks)]
pub trait PPostRepository {}
#[autumn_web::repository(PComment, table = "p_comments", hooks = PCommentHooks)]
pub trait PCommentRepository {}

// ── Graph C: child with `commit_hooks = true` ───────────────────────────────

diesel::table! { c_posts (id) { id -> Int8, title -> Text, } }
diesel::table! { c_comments (id) { id -> Int8, post_id -> Int8, } }
diesel::table! { c_locks (id) { id -> Int8, post_id -> Int8, } }
diesel::table! { c_notes (id) { id -> Int8, comment_id -> Int8, } }

#[autumn_web::model(table = "c_posts")]
pub struct CPost {
    #[id]
    pub id: i64,
    pub title: String,
}
#[autumn_web::model(table = "c_comments")]
pub struct CComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}
#[autumn_web::model(table = "c_locks")]
pub struct CLock {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

#[autumn_web::model(table = "c_notes")]
pub struct CNote {
    #[id]
    pub id: i64,
    pub comment_id: i64,
}

#[derive(Clone, Default)]
pub struct CCommentHooks;

impl MutationHooks for CCommentHooks {
    type Model = CComment;
    type NewModel = NewCComment;
    type UpdateModel = UpdateCComment;

    async fn after_delete_commit(
        &self,
        _ctx: &mut MutationContext,
        record: &CComment,
    ) -> AutumnResult<()> {
        log_push(format!("ccommit:{}", record.id));
        Ok(())
    }
}

#[autumn_web::repository(
    CPost,
    table = "c_posts",
    dependent(PgCCommentRepository, fk = "post_id", on_delete = destroy),
    dependent(PgCLockRepository, fk = "post_id", on_delete = restrict)
)]
pub trait CPostRepository {}
#[autumn_web::repository(
    CComment,
    table = "c_comments",
    hooks = CCommentHooks,
    commit_hooks = true,
    dependent(PgCNoteRepository, fk = "comment_id", on_delete = restrict)
)]
pub trait CCommentRepository {}
#[autumn_web::repository(CNote, table = "c_notes")]
pub trait CNoteRepository {}
#[autumn_web::repository(CLock, table = "c_locks")]
pub trait CLockRepository {}

// ── Graph S: self-referential, hooked, with commit hooks ────────────────────

diesel::table! { s_nodes (id) { id -> Int8, parent_id -> Nullable<Int8>, } }

#[autumn_web::model(table = "s_nodes")]
pub struct SNode {
    #[id]
    pub id: i64,
    pub parent_id: Option<i64>,
}

#[derive(Clone, Default)]
pub struct SNodeHooks;

impl MutationHooks for SNodeHooks {
    type Model = SNode;
    type NewModel = NewSNode;
    type UpdateModel = UpdateSNode;

    async fn before_delete(&self, _ctx: &mut MutationContext, record: &SNode) -> AutumnResult<()> {
        log_push(format!("snode:{}", record.id));
        Ok(())
    }

    async fn after_delete_commit(
        &self,
        _ctx: &mut MutationContext,
        record: &SNode,
    ) -> AutumnResult<()> {
        log_push(format!("scommit:{}", record.id));
        Ok(())
    }
}

#[autumn_web::repository(
    SNode,
    table = "s_nodes",
    hooks = SNodeHooks,
    commit_hooks = true,
    dependent(PgSNodeRepository, fk = "parent_id", on_delete = destroy)
)]
pub trait SNodeRepository {}

// ── Harness ─────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

async fn count(conn: &mut AsyncPgConnection, table: &str) -> i64 {
    let row: CountRow = diesel::sql_query(format!("SELECT COUNT(*) AS n FROM {table}"))
        .get_result(conn)
        .await
        .expect("count query");
    row.n
}

async fn run(conn: &mut AsyncPgConnection, sql: &str) {
    diesel::sql_query(sql)
        .execute(conn)
        .await
        .unwrap_or_else(|e| panic!("sql failed: {e}\n{sql}"));
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
    conn.batch_execute(COMMIT_HOOK_UP)
        .await
        .expect("hook queue");
    for stmt in [
        "CREATE TABLE g_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE g_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES g_posts(id))",
        "CREATE TABLE g_pages (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES g_posts(id))",
        "CREATE TABLE g_sections (id BIGSERIAL PRIMARY KEY, page_id BIGINT NOT NULL REFERENCES g_pages(id))",
        "CREATE TABLE g_locks (id BIGSERIAL PRIMARY KEY, section_id BIGINT NOT NULL REFERENCES g_sections(id))",
        "CREATE TABLE h_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE h_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES h_posts(id))",
        "CREATE TABLE h_pages (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES h_posts(id))",
        "CREATE TABLE h_sections (id BIGSERIAL PRIMARY KEY, page_id BIGINT NOT NULL REFERENCES h_pages(id))",
        "CREATE TABLE h_locks (id BIGSERIAL PRIMARY KEY, section_id BIGINT NOT NULL REFERENCES h_sections(id))",
        "CREATE TABLE o_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE o_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES o_posts(id))",
        "CREATE TABLE p_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE p_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES p_posts(id))",
        "CREATE TABLE c_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
        "CREATE TABLE c_comments (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES c_posts(id))",
        "CREATE TABLE c_locks (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES c_posts(id))",
        "CREATE TABLE c_notes (id BIGSERIAL PRIMARY KEY, comment_id BIGINT NOT NULL REFERENCES c_comments(id))",
        "CREATE TABLE s_nodes (id BIGSERIAL PRIMARY KEY, parent_id BIGINT REFERENCES s_nodes(id))",
    ] {
        run(&mut conn, stmt).await;
    }
    (pool, container)
}

fn assert_conflict(err: &autumn_web::AutumnError) {
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::CONFLICT,
        "a restrict must surface as 409"
    );
}

// ── Item 1: deeper `restrict` hoisting ──────────────────────────────────────

/// Seeds post 1 with a hooked comment and a hooked page whose section is locked.
async fn seed_sibling_legs(conn: &mut AsyncPgConnection, prefix: &str) {
    run(
        conn,
        &format!("INSERT INTO {prefix}_posts (id, title) VALUES (1, 'p')"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_comments (id, post_id) VALUES (1, 1)"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_pages (id, post_id) VALUES (1, 1)"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_sections (id, page_id) VALUES (1, 1)"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_locks (id, section_id) VALUES (1, 1)"),
    )
    .await;
}

/// Seeds post 11 (hooked comment only) and post 12 (page, section, lock).
async fn seed_batch_parents(conn: &mut AsyncPgConnection, prefix: &str) {
    run(
        conn,
        &format!("INSERT INTO {prefix}_posts (id, title) VALUES (11, 'a'), (12, 'b')"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_comments (id, post_id) VALUES (11, 11)"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_pages (id, post_id) VALUES (12, 12)"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_sections (id, page_id) VALUES (12, 12)"),
    )
    .await;
    run(
        conn,
        &format!("INSERT INTO {prefix}_locks (id, section_id) VALUES (12, 12)"),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn repo_attr_deep_restrict_in_a_later_sibling_leg_fires_no_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    seed_sibling_legs(&mut conn, "g").await;

    let repo = PgGPostRepository::with_pool_untracked(pool.clone());
    let err = repo.delete_by_id(1).await.expect_err("lock must block");
    assert_conflict(&err);
    assert_eq!(
        log_range("g", 1, 1),
        Vec::<String>::new(),
        "no hook may fire"
    );
    assert_eq!(count(&mut conn, "g_posts").await, 1);
    assert_eq!(count(&mut conn, "g_comments").await, 1);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn model_declared_deep_restrict_in_a_later_sibling_leg_fires_no_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    seed_sibling_legs(&mut conn, "h").await;

    let repo = PgHPostRepository::with_pool_untracked(pool.clone());
    let err = repo.delete_by_id(1).await.expect_err("lock must block");
    assert_conflict(&err);
    assert_eq!(
        log_range("h", 1, 1),
        Vec::<String>::new(),
        "no hook may fire"
    );
    assert_eq!(count(&mut conn, "h_comments").await, 1);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn repo_attr_delete_many_deep_restrict_in_a_later_parent_fires_no_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    seed_batch_parents(&mut conn, "g").await;

    let repo = PgGPostRepository::with_pool_untracked(pool.clone());
    let err = repo
        .delete_many(&[11, 12])
        .await
        .expect_err("lock must block");
    assert_conflict(&err);
    assert_eq!(
        log_range("g", 11, 12),
        Vec::<String>::new(),
        "no hook may fire"
    );
    assert_eq!(count(&mut conn, "g_posts").await, 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn model_declared_delete_many_deep_restrict_in_a_later_parent_fires_no_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    seed_batch_parents(&mut conn, "h").await;

    let repo = PgHPostRepository::with_pool_untracked(pool.clone());
    let err = repo
        .delete_many(&[11, 12])
        .await
        .expect_err("lock must block");
    assert_conflict(&err);
    assert_eq!(
        log_range("h", 11, 12),
        Vec::<String>::new(),
        "no hook may fire"
    );
    assert_eq!(count(&mut conn, "h_posts").await, 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unblocked_deep_probe_still_cascades_every_level() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO g_posts (id, title) VALUES (21, 'p')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO g_comments (id, post_id) VALUES (21, 21)",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO g_pages (id, post_id) VALUES (21, 21)",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO g_sections (id, page_id) VALUES (21, 21)",
    )
    .await;

    let repo = PgGPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(21).await.expect("no lock, so it deletes");
    assert_eq!(
        log_range("g", 21, 21),
        vec!["gcomment:21".to_owned(), "gpage:21".to_owned()],
        "each child hook fires once, in leg order"
    );
    assert_eq!(count(&mut conn, "g_posts").await, 0);
    assert_eq!(count(&mut conn, "g_sections").await, 0);
}

// ── Item 2: parent hook before child hooks, on both paths ───────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_by_id_fires_the_parent_hook_before_child_hooks() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(&mut conn, "INSERT INTO o_posts (id, title) VALUES (1, 'p')").await;
    run(
        &mut conn,
        "INSERT INTO o_comments (id, post_id) VALUES (1, 1), (2, 1)",
    )
    .await;

    let repo = PgOPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(1).await.expect("delete");
    assert_eq!(
        log_range("o", 1, 2),
        vec!["opost:1", "ocomment:1", "ocomment:2"],
        "parent first, then children"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_many_fires_every_parent_hook_before_any_child_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO o_posts (id, title) VALUES (11, 'a'), (12, 'b')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO o_comments (id, post_id) VALUES (11, 11), (12, 12)",
    )
    .await;

    let repo = PgOPostRepository::with_pool_untracked(pool.clone());
    repo.delete_many(&[11, 12]).await.expect("delete_many");
    assert_eq!(
        log_range("o", 11, 12),
        vec!["opost:11", "opost:12", "ocomment:11", "ocomment:12"],
        "all parents first, then children; each hook exactly once"
    );
    assert_eq!(count(&mut conn, "o_posts").await, 0);
    assert_eq!(count(&mut conn, "o_comments").await, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_by_id_parent_veto_fires_no_child_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO o_posts (id, title) VALUES (31, 'veto')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO o_comments (id, post_id) VALUES (31, 31)",
    )
    .await;

    let repo = PgOPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(31).await.expect_err("parent vetoes");
    assert_eq!(
        log_range("o", 31, 31),
        vec!["opost:31"],
        "only the parent hook ran"
    );
    assert_eq!(count(&mut conn, "o_comments").await, 1);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_many_parent_veto_fires_no_child_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO o_posts (id, title) VALUES (41, 'ok'), (42, 'veto')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO o_comments (id, post_id) VALUES (41, 41), (42, 42)",
    )
    .await;

    let repo = PgOPostRepository::with_pool_untracked(pool.clone());
    repo.delete_many(&[41, 42])
        .await
        .expect_err("parent vetoes");
    assert_eq!(
        log_range("o", 41, 42),
        vec!["opost:41", "opost:42"],
        "both parent hooks run, in id order, and no child hook runs"
    );
    assert_eq!(count(&mut conn, "o_posts").await, 2, "rolled back");
    assert_eq!(count(&mut conn, "o_comments").await, 2, "rolled back");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn model_declared_delete_many_orders_parent_hooks_first_and_honours_veto() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO p_posts (id, title) VALUES (51, 'a'), (52, 'b')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO p_comments (id, post_id) VALUES (51, 51), (52, 52)",
    )
    .await;
    let repo = PgPPostRepository::with_pool_untracked(pool.clone());
    repo.delete_many(&[51, 52]).await.expect("delete_many");
    assert_eq!(
        log_range("p", 51, 52),
        vec!["ppost:51", "ppost:52", "pcomment:51", "pcomment:52"]
    );

    run(
        &mut conn,
        "INSERT INTO p_posts (id, title) VALUES (61, 'ok'), (62, 'veto')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO p_comments (id, post_id) VALUES (61, 61)",
    )
    .await;
    repo.delete_many(&[61, 62])
        .await
        .expect_err("parent vetoes");
    assert_eq!(
        log_range("p", 61, 62),
        vec!["ppost:61", "ppost:62"],
        "no child hook may fire after a parent veto"
    );
    assert_eq!(count(&mut conn, "p_posts").await, 2, "rolled back");
    assert_eq!(count(&mut conn, "p_comments").await, 1, "rolled back");
}

// ── Item 5: a cascaded child's `after_delete_commit` ────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cascaded_child_commit_hook_runs_once_per_child_after_commit() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(&mut conn, "INSERT INTO c_posts (id, title) VALUES (1, 'p')").await;
    run(
        &mut conn,
        "INSERT INTO c_comments (id, post_id) VALUES (1, 1), (2, 1), (3, 1)",
    )
    .await;

    let repo = PgCPostRepository::with_pool_untracked(pool.clone());
    repo.delete_by_id(1).await.expect("delete");
    assert_eq!(
        log_range("c", 1, 3),
        Vec::<String>::new(),
        "commit hooks run from the queue, not inline"
    );

    let drained = autumn_web::test::drain_ready_repository_commit_hooks(&pool, 16).await;
    assert_eq!(drained, 3, "one queued hook per destroyed child");
    let mut log = log_range("c", 1, 3);
    log.sort();
    assert_eq!(log, vec!["ccommit:1", "ccommit:2", "ccommit:3"]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cascaded_child_commit_hook_runs_once_per_child_on_delete_many() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO c_posts (id, title) VALUES (11, 'a'), (12, 'b')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO c_comments (id, post_id) VALUES (11, 11), (12, 12)",
    )
    .await;

    let repo = PgCPostRepository::with_pool_untracked(pool.clone());
    repo.delete_many(&[11, 12]).await.expect("delete_many");
    let drained = autumn_web::test::drain_ready_repository_commit_hooks(&pool, 16).await;
    assert_eq!(drained, 2);
    let mut log = log_range("c", 11, 12);
    log.sort();
    assert_eq!(log, vec!["ccommit:11", "ccommit:12"]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn rolled_back_cascade_enqueues_no_child_commit_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO c_posts (id, title) VALUES (21, 'p')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO c_comments (id, post_id) VALUES (21, 21)",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO c_locks (id, post_id) VALUES (21, 21)",
    )
    .await;

    let repo = PgCPostRepository::with_pool_untracked(pool.clone());
    let err = repo.delete_by_id(21).await.expect_err("lock must block");
    assert_conflict(&err);
    assert_eq!(count(&mut conn, "autumn_repository_commit_hooks").await, 0);
    let drained = autumn_web::test::drain_ready_repository_commit_hooks(&pool, 16).await;
    assert_eq!(drained, 0);
    assert_eq!(log_range("c", 21, 21), Vec::<String>::new());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn rolled_back_cascade_with_a_deep_restrict_enqueues_no_commit_hook() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO c_posts (id, title) VALUES (31, 'p')",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO c_comments (id, post_id) VALUES (31, 31)",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO c_notes (id, comment_id) VALUES (31, 31)",
    )
    .await;

    let repo = PgCPostRepository::with_pool_untracked(pool.clone());
    let err = repo.delete_by_id(31).await.expect_err("note must block");
    assert_conflict(&err);
    assert_eq!(count(&mut conn, "autumn_repository_commit_hooks").await, 0);
    assert_eq!(count(&mut conn, "c_comments").await, 1);
}

/// A batch root that is also another root's descendant fires each hook once.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_many_hooks_a_root_that_is_also_a_descendant_once() {
    let (pool, _c) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    run(
        &mut conn,
        "INSERT INTO s_nodes (id, parent_id) VALUES (1, NULL)",
    )
    .await;
    run(
        &mut conn,
        "INSERT INTO s_nodes (id, parent_id) VALUES (2, 1), (3, 2)",
    )
    .await;

    let repo = PgSNodeRepository::with_pool_untracked(pool.clone());
    repo.delete_many(&[1, 2]).await.expect("delete_many");
    assert_eq!(
        log_range("snode", 1, 3),
        vec!["snode:1", "snode:2", "snode:3"],
        "roots 1 and 2 hook once each, then descendant 3"
    );
    assert_eq!(count(&mut conn, "s_nodes").await, 0);

    assert_eq!(
        count(&mut conn, "autumn_repository_commit_hooks").await,
        3,
        "one queued commit hook per deleted node"
    );
    // The dispatcher kick after commit may run some hooks before this drain, so
    // the drain count is not stable here. Wait for the log instead.
    for _ in 0..50 {
        autumn_web::test::drain_ready_repository_commit_hooks(&pool, 16).await;
        if log_range("scommit", 1, 3).len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut commits = log_range("scommit", 1, 3);
    commits.sort();
    assert_eq!(commits, vec!["scommit:1", "scommit:2", "scommit:3"]);
}
