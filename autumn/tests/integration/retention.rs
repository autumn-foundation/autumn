//! Database-level integration tests for declarative data-retention sweeps
//! (`#[repository(Model, retention(...))]`, issue #1342).
//!
//! Behavioral tests **require Docker** (testcontainers) and are `#[ignore]`d
//! by default; CI's ignored-test sweep runs them. Two non-ignored tests prove
//! the generated surface (auto-registered `TaskInfo`, fleet coordination)
//! without a live database — the `#1592` guard pattern used throughout this
//! suite (see `model_counter_cache.rs`).
//!
//! Fixtures:
//!
//! | Model | Attribute | Behavior under test |
//! |---|---|---|
//! | `RtSession` | `retention(after = "30d", basis = created_at, batch_size = 2)` | hard-delete, batched |
//! | `RtPost` | `soft_delete, retention(after = "30d", basis = created_at, purge_deleted_after = "90d")` | soft-delete-aware age sweep + hard purge |
//! | `RtPlain` | none | opt-in: behaves exactly as today |
//! | `RtCcChild` | `retention(after = "30d", basis = created_at)`, `#[belongs_to(RtCcParent, counter_cache)]` | sweep decrements the parent's counter cache exactly like every other delete path (#1342 review round 3) |

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::AppState;
use autumn_web::task::{Schedule, TaskCoordination};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    rt_sessions (id) {
        id -> Int8,
        token -> Text,
        created_at -> Timestamp,
    }
}

#[autumn_web::model(table = "rt_sessions")]
pub struct RtSession {
    #[id]
    pub id: i64,
    pub token: String,
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::repository(RtSession, table = "rt_sessions", retention(after = "30d", basis = created_at, batch_size = 2))]
pub trait RtSessionRepository {}

diesel::table! {
    rt_posts (id) {
        id -> Int8,
        title -> Text,
        created_at -> Timestamp,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "rt_posts")]
pub struct RtPost {
    #[id]
    pub id: i64,
    pub title: String,
    pub created_at: chrono::NaiveDateTime,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    RtPost,
    table = "rt_posts",
    soft_delete,
    retention(after = "30d", basis = created_at, purge_deleted_after = "90d")
)]
pub trait RtPostRepository {}

diesel::table! {
    rt_plains (id) {
        id -> Int8,
        title -> Text,
        created_at -> Timestamp,
    }
}

#[autumn_web::model(table = "rt_plains")]
pub struct RtPlain {
    #[id]
    pub id: i64,
    pub title: String,
    pub created_at: chrono::NaiveDateTime,
}

// Opt-in (issue #1342 AC): no `retention(...)` attribute means no sweep is
// generated or registered — proven at the macro level by
// `repository_macro_without_retention_generates_no_sweep` in
// `autumn-macros/src/repository.rs`, since a runtime test cannot observe the
// *absence* of a method without a compile error. This repository exists here
// purely so the fixture table list below stays honest about what's under
// test; it participates in no retention-specific assertions.
#[autumn_web::repository(RtPlain, table = "rt_plains")]
pub trait RtPlainRepository {}

diesel::table! {
    rt_cc_parents (id) {
        id -> Int8,
        name -> Text,
        child_count -> Int8,
    }
}

#[autumn_web::model(table = "rt_cc_parents")]
pub struct RtCcParent {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub child_count: i64,
}

#[autumn_web::repository(RtCcParent, table = "rt_cc_parents")]
pub trait RtCcParentRepository {}

diesel::table! {
    rt_cc_children (id) {
        id -> Int8,
        parent_id -> Int8,
        created_at -> Timestamp,
    }
}

// Regression (#1342 review round 3): a retained model that is also the
// counter-cached child of a `belongs_to(...)` relationship must move its
// parent's counter when the sweep deletes it, exactly like `delete_many`
// already does. No `soft_delete` here on purpose — this exercises the
// hard-delete counter-cache arm of the age branch.
#[autumn_web::model(table = "rt_cc_children")]
#[belongs_to(RtCcParent, fk = parent_id, counter_cache = "child_count")]
pub struct RtCcChild {
    #[id]
    pub id: i64,
    pub parent_id: i64,
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::repository(
    RtCcChild,
    table = "rt_cc_children",
    retention(after = "30d", basis = created_at)
)]
pub trait RtCcChildRepository {}

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS rt_sessions \
     (id BIGSERIAL PRIMARY KEY, token TEXT NOT NULL, created_at TIMESTAMP NOT NULL)",
    "CREATE TABLE IF NOT EXISTS rt_posts \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, created_at TIMESTAMP NOT NULL, \
      deleted_at TIMESTAMP NULL)",
    "CREATE TABLE IF NOT EXISTS rt_plains \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, created_at TIMESTAMP NOT NULL)",
    "CREATE TABLE IF NOT EXISTS rt_cc_parents \
     (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, child_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS rt_cc_children \
     (id BIGSERIAL PRIMARY KEY, parent_id BIGINT NOT NULL, created_at TIMESTAMP NOT NULL)",
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
    let pool = Pool::builder(manager).max_size(10).build().expect("pool");

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

fn state_with_pool(pool: Pool<AsyncPgConnection>) -> AppState {
    AppState::for_test().with_pool(pool)
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

async fn seed_session(conn: &mut AsyncPgConnection, token: &str, age_days: i64) -> i64 {
    diesel::sql_query(
        "INSERT INTO rt_sessions (token, created_at) \
         VALUES ($1, NOW() - ($2 || ' days')::interval) RETURNING id",
    )
    .bind::<diesel::sql_types::Text, _>(token)
    .bind::<diesel::sql_types::BigInt, _>(age_days)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed session")
    .id
}

async fn seed_post(conn: &mut AsyncPgConnection, title: &str, age_days: i64) -> i64 {
    diesel::sql_query(
        "INSERT INTO rt_posts (title, created_at) \
         VALUES ($1, NOW() - ($2 || ' days')::interval) RETURNING id",
    )
    .bind::<diesel::sql_types::Text, _>(title)
    .bind::<diesel::sql_types::BigInt, _>(age_days)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed post")
    .id
}

async fn soft_delete_post_aged(conn: &mut AsyncPgConnection, id: i64, deleted_age_days: i64) {
    diesel::sql_query(
        "UPDATE rt_posts SET deleted_at = NOW() - ($2 || ' days')::interval WHERE id = $1",
    )
    .bind::<diesel::sql_types::BigInt, _>(id)
    .bind::<diesel::sql_types::BigInt, _>(deleted_age_days)
    .execute(conn)
    .await
    .expect("soft delete post");
}

async fn count_rows(conn: &mut AsyncPgConnection, table: &str) -> i64 {
    diesel::sql_query(format!("SELECT COUNT(*) AS count FROM {table}"))
        .get_result::<CountRow>(conn)
        .await
        .expect("count rows")
        .count
}

async fn count_deleted_rows(conn: &mut AsyncPgConnection) -> i64 {
    diesel::sql_query("SELECT COUNT(*) AS count FROM rt_posts WHERE deleted_at IS NOT NULL")
        .get_result::<CountRow>(conn)
        .await
        .expect("count deleted rows")
        .count
}

async fn seed_cc_parent(conn: &mut AsyncPgConnection, name: &str, child_count: i64) -> i64 {
    diesel::sql_query("INSERT INTO rt_cc_parents (name, child_count) VALUES ($1, $2) RETURNING id")
        .bind::<diesel::sql_types::Text, _>(name)
        .bind::<diesel::sql_types::BigInt, _>(child_count)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed cc parent")
        .id
}

async fn seed_cc_child(conn: &mut AsyncPgConnection, parent_id: i64, age_days: i64) -> i64 {
    diesel::sql_query(
        "INSERT INTO rt_cc_children (parent_id, created_at) \
         VALUES ($1, NOW() - ($2 || ' days')::interval) RETURNING id",
    )
    .bind::<diesel::sql_types::BigInt, _>(parent_id)
    .bind::<diesel::sql_types::BigInt, _>(age_days)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed cc child")
    .id
}

async fn fetch_cc_parent_child_count(conn: &mut AsyncPgConnection, parent_id: i64) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct ChildCountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        child_count: i64,
    }
    diesel::sql_query("SELECT child_count FROM rt_cc_parents WHERE id = $1")
        .bind::<diesel::sql_types::BigInt, _>(parent_id)
        .get_result::<ChildCountRow>(conn)
        .await
        .expect("fetch cc parent child_count")
        .child_count
}

// ── Non-Docker: generated surface + auto-registration ────────────────────
//
// Proves AC "Declaring a policy registers exactly one recurring sweep with
// the existing scheduler/job substrate — the user writes no #[scheduled] fn
// and no SQL" and AC "multi-replica safe" (fleet coordination) without a
// live database.

#[test]
fn retention_task_info_uses_fleet_coordination_and_declared_batch_interval() {
    let info = PgRtSessionRepository::__autumn_retention_task_info();
    assert_eq!(info.coordination, TaskCoordination::Fleet);
    // Table-qualified (not model-qualified) so same-named models in
    // different modules can't collide on the generated task name.
    assert_eq!(info.name, "retention-sweep-rt_sessions");
    match info.schedule {
        Schedule::FixedDelay(d) => assert_eq!(d, std::time::Duration::from_secs(3600)),
        _ => panic!("retention sweeps use FixedDelay, not cron"),
    }
}

#[test]
fn retention_task_is_auto_collected_with_no_tasks_wiring() {
    // No `tasks![...]` entry exists anywhere for RtSession/RtPost — the
    // descriptor is purely `inventory`-collected.
    let names: Vec<String> = autumn_web::retention::collect_retention_tasks()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        names.contains(&"retention-sweep-rt_sessions".to_string()),
        "RtSession's retention sweep must be auto-collected: {names:?}"
    );
    assert!(
        names.contains(&"retention-sweep-rt_posts".to_string()),
        "RtPost's retention sweep must be auto-collected: {names:?}"
    );
    assert!(
        !names.contains(&"retention-sweep-rt_plains".to_string()),
        "RtPlain declared no retention(...) and must not be collected: {names:?}"
    );
}

// ── Docker-gated behavioral tests ─────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn retention_age_based_hard_delete_sweeps_stale_rows_in_batches() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    // 5 stale (40d old, > 30d threshold) with batch_size = 2 forces 3 loop
    // iterations (2 + 2 + 1); 2 fresh (5d old) must survive untouched.
    for i in 0..5 {
        seed_session(&mut conn, &format!("stale-{i}"), 40).await;
    }
    for i in 0..2 {
        seed_session(&mut conn, &format!("fresh-{i}"), 5).await;
    }
    drop(conn);

    let state = state_with_pool(pool.clone());
    let report = PgRtSessionRepository::__autumn_retention_sweep(&state)
        .await
        .expect("sweep should succeed");

    assert_eq!(report.model, "RtSession");
    assert_eq!(report.table, "rt_sessions");
    assert_eq!(report.rows_swept, 5);
    assert!(!report.dry_run);

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        count_rows(&mut conn, "rt_sessions").await,
        2,
        "only the 2 fresh sessions should remain"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn retention_age_based_soft_delete_sets_deleted_at_not_hard_delete() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    for i in 0..3 {
        seed_post(&mut conn, &format!("stale-{i}"), 40).await;
    }
    for i in 0..2 {
        seed_post(&mut conn, &format!("fresh-{i}"), 5).await;
    }
    drop(conn);

    let state = state_with_pool(pool.clone());
    let report = PgRtPostRepository::__autumn_retention_sweep(&state)
        .await
        .expect("sweep should succeed");

    assert_eq!(
        report.rows_swept, 3,
        "age-based branch soft-deletes the 3 stale posts"
    );

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        count_rows(&mut conn, "rt_posts").await,
        5,
        "soft-delete-aware age sweep must not hard-delete: all 5 rows remain"
    );
    assert_eq!(
        count_deleted_rows(&mut conn).await,
        3,
        "exactly the 3 stale posts must be marked deleted_at"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn retention_purge_deleted_after_hard_purges_old_soft_deleted_rows() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    // Old enough to purge (soft-deleted 100d ago, > 90d threshold).
    let purge_id = seed_post(&mut conn, "long-deleted", 200).await;
    soft_delete_post_aged(&mut conn, purge_id, 100).await;
    // Soft-deleted recently (10d ago): must survive the purge sweep.
    let recent_id = seed_post(&mut conn, "recently-deleted", 200).await;
    soft_delete_post_aged(&mut conn, recent_id, 10).await;
    // Never deleted at all: must never be touched by either branch here
    // (fresh enough to dodge the age-based branch too).
    seed_post(&mut conn, "alive", 5).await;
    drop(conn);

    let state = state_with_pool(pool.clone());
    let report = PgRtPostRepository::__autumn_retention_sweep(&state)
        .await
        .expect("sweep should succeed");

    // 1 hard purge from the purge_deleted_after branch; the age-based branch
    // sweeps nothing here (the two deleted rows are already deleted_at IS
    // NOT NULL, so the age branch's is_null() guard skips them, and "alive"
    // is only 5 days old).
    assert_eq!(report.rows_swept, 1);

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        count_rows(&mut conn, "rt_posts").await,
        2,
        "the long-deleted row is hard-purged; recently-deleted and alive remain"
    );
    assert_eq!(
        count_deleted_rows(&mut conn).await,
        1,
        "recently-deleted is still soft-deleted, not purged"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn retention_dry_run_counts_without_deleting_anything() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    for i in 0..4 {
        seed_session(&mut conn, &format!("stale-{i}"), 40).await;
    }
    drop(conn);

    let state = state_with_pool(pool.clone());
    let report = PgRtSessionRepository::__autumn_retention_dry_run(state)
        .await
        .expect("dry run should succeed");

    assert_eq!(report.rows_swept, 4);
    assert!(report.dry_run);

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        count_rows(&mut conn, "rt_sessions").await,
        4,
        "a dry run must never delete anything"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn retention_run_dry_run_registry_walk_matches_direct_call() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    for i in 0..2 {
        seed_session(&mut conn, &format!("stale-{i}"), 40).await;
    }
    drop(conn);

    let state = state_with_pool(pool.clone());
    let reports = autumn_web::retention::run_retention_dry_run(&state, Some("RtSession"))
        .await
        .expect("registry dry run should succeed");

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].model, "RtSession");
    assert_eq!(reports[0].rows_swept, 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn retention_sweep_maintains_belongs_to_counter_cache() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    let parent_id = seed_cc_parent(&mut conn, "team", 5).await;
    for _ in 0..3 {
        seed_cc_child(&mut conn, parent_id, 40).await;
    }
    for _ in 0..2 {
        seed_cc_child(&mut conn, parent_id, 5).await;
    }
    drop(conn);

    let state = state_with_pool(pool.clone());
    let report = PgRtCcChildRepository::__autumn_retention_sweep(&state)
        .await
        .expect("sweep should succeed");

    assert_eq!(report.rows_swept, 3, "3 stale children are hard-deleted");

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        count_rows(&mut conn, "rt_cc_children").await,
        2,
        "only the 2 fresh children should remain"
    );
    assert_eq!(
        fetch_cc_parent_child_count(&mut conn, parent_id).await,
        2,
        "the parent's counter cache must drop by exactly the 3 swept children, \
         matching what delete_many would have done"
    );
}
