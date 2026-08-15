//! Database-level integration tests for declarative counter caches
//! (`#[belongs_to(Parent, counter_cache)]`, issue #1325).
//!
//! The behavioural tests **require Docker** (testcontainers) and are
//! `#[ignore]`d by default; CI's ignored-test sweep runs them. Two non-ignored
//! tests prove the generated surface exists and that the compile-time spec
//! metadata is what the SQL will be built from, without a live database (the
//! #1592 guard pattern).
//!
//! Fixtures span every generated branch:
//!
//! | Child | Attribute | Parent | Counter column |
//! |---|---|---|---|
//! | `CcComment` | `#[belongs_to(CcPost, counter_cache)]` (pure default) | `cc_posts` | `cc_comment_count` |
//! | `CcMember` | `… counter_cache = "member_count"` (explicit column override) | `cc_teams` | `member_count` |
//! | `CcRevision` | `… counter_cache` on a `soft_delete` repository | `cc_pages` | `cc_revision_count` |
//! | `CcMessage` | two counter-cached legs to two different parents | `cc_users` / `cc_rooms` | `sent_count` / `cc_message_count` |
//! | `CcCapped` | `… counter_cache` onto a `CHECK`ed column | `cc_capped_parents` | `cc_capped_count` |
//!
//! `CcCapped` exists purely to make **same-transaction** atomicity observable:
//! its parent's counter column carries `CHECK (cc_capped_count <= 2)`, so the
//! *counter update* is what fails on the third insert. If the increment were
//! issued outside the insert's transaction the child row would survive; because
//! it is inside, the whole thing rolls back and the child is never persisted.
//! That is the only shape that can distinguish "same transaction" from "two
//! statements that usually both succeed".
//!
//! Concurrency assertions are **exact, not bounded**: the increment is a single
//! `UPDATE … SET c = c + $1` statement, so N concurrent inserts commute and the
//! final value is N under *every* interleaving. A read-modify-write
//! implementation loses updates and fails this test; nothing about it can flake
//! on scheduling.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::sync::Arc;

use autumn_web::Patch;
use autumn_web::repository::AutumnCounterCaches as _;
use autumn_web::tenancy::CURRENT_TENANT;
use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Pure defaults: `CcComment` -> `cc_posts.cc_comment_count` ────────────────

diesel::table! {
    cc_posts (id) {
        id -> Int8,
        title -> Text,
        cc_comment_count -> Int8,
    }
}

#[autumn_web::model(table = "cc_posts")]
pub struct CcPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub cc_comment_count: i64,
}

#[autumn_web::repository(CcPost, table = "cc_posts")]
pub trait CcPostRepository {}

diesel::table! {
    cc_comments (id) {
        id -> Int8,
        body -> Text,
        post_id -> Int8,
    }
}

#[autumn_web::model(table = "cc_comments")]
#[belongs_to(CcPost, fk = post_id, counter_cache)]
pub struct CcComment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}

#[autumn_web::repository(CcComment, table = "cc_comments")]
pub trait CcCommentRepository {}

// ── Column override + nullable foreign key ──────────────────────────────────

diesel::table! {
    cc_teams (id) {
        id -> Int8,
        name -> Text,
        member_count -> Int8,
    }
}

#[autumn_web::model(table = "cc_teams")]
pub struct CcTeam {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub member_count: i64,
}

#[autumn_web::repository(CcTeam, table = "cc_teams")]
pub trait CcTeamRepository {}

diesel::table! {
    cc_members (id) {
        id -> Int8,
        nick -> Text,
        team_id -> Int8,
    }
}

/// The explicit-column override: the convention would give
/// `cc_teams.cc_member_count`, the attribute names `member_count` instead.
#[autumn_web::model(table = "cc_members")]
#[belongs_to(CcTeam, fk = team_id, counter_cache = "member_count")]
pub struct CcMember {
    #[id]
    pub id: i64,
    pub nick: String,
    pub team_id: i64,
}

#[autumn_web::repository(CcMember, table = "cc_members")]
pub trait CcMemberRepository {}

// ── Soft-deleting child ─────────────────────────────────────────────────────

diesel::table! {
    cc_pages (id) {
        id -> Int8,
        title -> Text,
        cc_revision_count -> Int8,
    }
}

#[autumn_web::model(table = "cc_pages")]
pub struct CcPage {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub cc_revision_count: i64,
}

#[autumn_web::repository(CcPage, table = "cc_pages")]
pub trait CcPageRepository {}

diesel::table! {
    cc_revisions (id) {
        id -> Int8,
        page_id -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "cc_revisions")]
#[belongs_to(CcPage, fk = page_id, counter_cache)]
pub struct CcRevision {
    #[id]
    pub id: i64,
    pub page_id: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(CcRevision, table = "cc_revisions", soft_delete)]
pub trait CcRevisionRepository {}

// ── Two counter-cached legs on one child ────────────────────────────────────

diesel::table! {
    cc_users (id) {
        id -> Int8,
        name -> Text,
        sent_count -> Int8,
    }
}

#[autumn_web::model(table = "cc_users")]
pub struct CcUser {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub sent_count: i64,
}

#[autumn_web::repository(CcUser, table = "cc_users")]
pub trait CcUserRepository {}

diesel::table! {
    cc_rooms (id) {
        id -> Int8,
        topic -> Text,
        cc_message_count -> Int8,
    }
}

#[autumn_web::model(table = "cc_rooms")]
pub struct CcRoom {
    #[id]
    pub id: i64,
    pub topic: String,
    #[default]
    pub cc_message_count: i64,
}

#[autumn_web::repository(CcRoom, table = "cc_rooms")]
pub trait CcRoomRepository {}

/// Two counter-cached legs to two *different* parents: one insert has to move
/// both columns, and a re-parenting update has to move only the leg that
/// actually changed.
#[autumn_web::model(table = "cc_messages")]
#[belongs_to(CcUser, fk = sender_id, name = sender, counter_cache = "sent_count")]
#[belongs_to(CcRoom, fk = room_id, counter_cache)]
pub struct CcMessage {
    #[id]
    pub id: i64,
    pub body: String,
    pub sender_id: i64,
    pub room_id: i64,
}

diesel::table! {
    cc_messages (id) {
        id -> Int8,
        body -> Text,
        sender_id -> Int8,
        room_id -> Int8,
    }
}

#[autumn_web::repository(CcMessage, table = "cc_messages")]
pub trait CcMessageRepository {}

// ── Tenant isolation ────────────────────────────────────────────────────────

diesel::table! {
    cc_tenant_posts (id) {
        id -> Int8,
        title -> Text,
        tenant_id -> Text,
        cc_tenant_comment_count -> Int8,
    }
}

#[autumn_web::model(table = "cc_tenant_posts")]
pub struct CcTenantPost {
    #[id]
    pub id: i64,
    pub title: String,
    pub tenant_id: String,
    #[default]
    pub cc_tenant_comment_count: i64,
}

#[autumn_web::repository(CcTenantPost, table = "cc_tenant_posts", tenant_scoped)]
pub trait CcTenantPostRepository {}

diesel::table! {
    cc_tenant_comments (id) {
        id -> Int8,
        body -> Text,
        post_id -> Int8,
        tenant_id -> Text,
    }
}

/// A counter update writes a parent row the caller only had to name the id of,
/// so on a shared multi-tenant table it has to be confined to the caller's own
/// tenant — the same hazard `#[votable]`'s aggregate `UPDATE` is scoped for.
///
/// `counter_cache_tenant` is explicit rather than inferred: `#[model]` on the
/// child cannot see the parent's fields, so naming the column is the author
/// asserting that **both** tables carry one by that name.
#[autumn_web::model(table = "cc_tenant_comments")]
#[belongs_to(
    CcTenantPost,
    fk = post_id,
    counter_cache = "cc_tenant_comment_count",
    counter_cache_tenant = "tenant_id"
)]
pub struct CcTenantComment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
    pub tenant_id: String,
}

#[autumn_web::repository(CcTenantComment, table = "cc_tenant_comments", tenant_scoped)]
pub trait CcTenantCommentRepository {}

// ── Atomicity fixture: the counter column itself is `CHECK`ed ───────────────

diesel::table! {
    cc_capped_parents (id) {
        id -> Int8,
        label -> Text,
        cc_capped_count -> Int8,
    }
}

#[autumn_web::model(table = "cc_capped_parents")]
pub struct CcCappedParent {
    #[id]
    pub id: i64,
    pub label: String,
    #[default]
    pub cc_capped_count: i64,
}

#[autumn_web::repository(CcCappedParent, table = "cc_capped_parents")]
pub trait CcCappedParentRepository {}

diesel::table! {
    cc_cappeds (id) {
        id -> Int8,
        parent_id -> Int8,
    }
}

#[autumn_web::model(table = "cc_cappeds")]
#[belongs_to(CcCappedParent, fk = parent_id, counter_cache)]
pub struct CcCapped {
    #[id]
    pub id: i64,
    pub parent_id: i64,
}

#[autumn_web::repository(CcCapped, table = "cc_cappeds")]
pub trait CcCappedRepository {}

// ── Setup & helpers ─────────────────────────────────────────────────────────

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cc_posts \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      cc_comment_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS cc_comments \
     (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL, \
      post_id BIGINT NOT NULL REFERENCES cc_posts(id))",
    "CREATE TABLE IF NOT EXISTS cc_teams \
     (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, \
      member_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS cc_members \
     (id BIGSERIAL PRIMARY KEY, nick TEXT NOT NULL, \
      team_id BIGINT NOT NULL REFERENCES cc_teams(id))",
    "CREATE TABLE IF NOT EXISTS cc_pages \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      cc_revision_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS cc_revisions \
     (id BIGSERIAL PRIMARY KEY, page_id BIGINT NOT NULL REFERENCES cc_pages(id), \
      deleted_at TIMESTAMP NULL)",
    "CREATE TABLE IF NOT EXISTS cc_users \
     (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, \
      sent_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS cc_rooms \
     (id BIGSERIAL PRIMARY KEY, topic TEXT NOT NULL, \
      cc_message_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS cc_messages \
     (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL, \
      sender_id BIGINT NOT NULL REFERENCES cc_users(id), \
      room_id BIGINT NOT NULL REFERENCES cc_rooms(id))",
    // Two tenants share one physical table — the shape the tenant predicate has
    // to separate. No FK to `cc_tenant_posts`, deliberately: a `REFERENCES`
    // constraint does not check `tenant_id`, which is exactly why the counter
    // update needs its own predicate.
    "CREATE TABLE IF NOT EXISTS cc_tenant_posts \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, tenant_id TEXT NOT NULL, \
      cc_tenant_comment_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS cc_tenant_comments \
     (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL, post_id BIGINT NOT NULL, \
      tenant_id TEXT NOT NULL)",
    // The `CHECK` is on the *counter* column, so the increment is what fails.
    "CREATE TABLE IF NOT EXISTS cc_capped_parents \
     (id BIGSERIAL PRIMARY KEY, label TEXT NOT NULL, \
      cc_capped_count BIGINT NOT NULL DEFAULT 0 CHECK (cc_capped_count <= 2))",
    "CREATE TABLE IF NOT EXISTS cc_cappeds \
     (id BIGSERIAL PRIMARY KEY, \
      parent_id BIGINT NOT NULL REFERENCES cc_capped_parents(id))",
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
    // Comfortably exceeds the 50 concurrent callers in the race test plus the
    // observer connection, so no caller ever queues on pool acquisition.
    let pool = Pool::builder(manager).max_size(60).build().expect("pool");

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

/// The maintained column and the ground truth in **one** statement, so both
/// necessarily come from the same snapshot. Drift is `persisted - ground_truth`.
#[derive(diesel::QueryableByName)]
struct SnapshotRow {
    #[diesel(sql_type = BigInt)]
    persisted: i64,
    #[diesel(sql_type = BigInt)]
    ground_truth: i64,
}

async fn seed_post(conn: &mut AsyncPgConnection, title: &str) -> i64 {
    diesel::sql_query("INSERT INTO cc_posts (title) VALUES ($1) RETURNING id")
        .bind::<diesel::sql_types::Text, _>(title)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed post")
        .id
}

async fn seed_one_col(conn: &mut AsyncPgConnection, table: &str, column: &str, value: &str) -> i64 {
    diesel::sql_query(format!(
        "INSERT INTO {table} ({column}) VALUES ($1) RETURNING id"
    ))
    .bind::<diesel::sql_types::Text, _>(value)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed row")
    .id
}

/// The child's tenant column is what the counter predicate keys off, so a
/// tenant-bearing parent has to be seeded with its tenant explicitly.
async fn seed_tenant_post(conn: &mut AsyncPgConnection, tenant: &str, title: &str) -> i64 {
    diesel::sql_query("INSERT INTO cc_tenant_posts (title, tenant_id) VALUES ($1, $2) RETURNING id")
        .bind::<diesel::sql_types::Text, _>(title)
        .bind::<diesel::sql_types::Text, _>(tenant)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed tenant post")
        .id
}

async fn seed_capped_parent(conn: &mut AsyncPgConnection) -> i64 {
    seed_one_col(conn, "cc_capped_parents", "label", "capped").await
}

/// Read a counter column directly, bypassing the repository under test.
async fn counter(conn: &mut AsyncPgConnection, table: &str, column: &str, id: i64) -> i64 {
    diesel::sql_query(format!(
        "SELECT {column} AS count FROM {table} WHERE id = $1"
    ))
    .bind::<BigInt, _>(id)
    .get_result::<CountRow>(conn)
    .await
    .expect("read counter")
    .count
}

async fn row_count(conn: &mut AsyncPgConnection, table: &str, predicate: &str) -> i64 {
    diesel::sql_query(format!(
        "SELECT COUNT(*)::BIGINT AS count FROM {table} WHERE {predicate}"
    ))
    .get_result::<CountRow>(conn)
    .await
    .expect("count rows")
    .count
}

/// `cc_posts.cc_comment_count` alongside `COUNT(cc_comments.*)`, in one statement.
async fn post_snapshot(conn: &mut AsyncPgConnection, post_id: i64) -> SnapshotRow {
    diesel::sql_query(
        "SELECT p.cc_comment_count AS persisted, \
         (SELECT COUNT(*) FROM cc_comments c WHERE c.post_id = p.id)::BIGINT AS ground_truth \
         FROM cc_posts p WHERE p.id = $1",
    )
    .bind::<BigInt, _>(post_id)
    .get_result::<SnapshotRow>(conn)
    .await
    .expect("read post snapshot")
}

/// Total drift across **every** parent row — the Success Metric's "observed
/// counter drift across all parents is 0".
async fn total_post_drift(conn: &mut AsyncPgConnection) -> i64 {
    diesel::sql_query(
        "SELECT COALESCE(SUM(ABS(p.cc_comment_count - \
           (SELECT COUNT(*) FROM cc_comments c WHERE c.post_id = p.id))), 0)::BIGINT AS count \
         FROM cc_posts p",
    )
    .get_result::<CountRow>(conn)
    .await
    .expect("read total drift")
    .count
}

// ── Non-ignored: the generated surface exists without a live database ────────

/// The compile-time spec metadata is exactly what the generated SQL is built
/// from, so pinning it here pins the SQL. This is the #1592-style non-ignored
/// guard for the Docker tests below.
#[test]
fn counter_cache_specs_are_generated_from_the_conventions() {
    // Pure default: column from the *child* model name, parent table from the
    // target type, foreign key from the association.
    let specs = CcComment::counter_caches();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].counter_column, "cc_comment_count");
    assert_eq!(specs[0].parent_table, "cc_posts");
    assert_eq!(specs[0].parent_pk, "id");
    assert_eq!(specs[0].fk_column, "post_id");
    assert_eq!(specs[0].child_table, "cc_comments");
    assert_eq!(specs[0].child_pk, "id");
    assert!(
        !specs[0].child_soft_delete,
        "a child with no deleted_at column must not emit a live-row predicate"
    );
    // `const` blocks, because the flag exists precisely so the maintenance
    // folds away at compile time for a model that declares no counter cache.
    const { assert!(CcComment::HAS_COUNTER_CACHES) };

    // Explicit override, nullable foreign key.
    let specs = CcMember::counter_caches();
    assert_eq!(specs[0].counter_column, "member_count");
    assert_eq!(specs[0].parent_table, "cc_teams");
    assert_eq!(
        (specs[0].fk_of)(&CcMember {
            id: 1,
            nick: "x".into(),
            team_id: 7
        }),
        Some(7)
    );

    // A `deleted_at` column flips the live-row predicate on.
    assert!(CcRevision::counter_caches()[0].child_soft_delete);

    // Two legs, in declaration order.
    let specs = CcMessage::counter_caches();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].counter_column, "sent_count");
    assert_eq!(specs[0].parent_table, "cc_users");
    assert_eq!(specs[1].counter_column, "cc_message_count");
    assert_eq!(specs[1].parent_table, "cc_rooms");

    // `counter_cache_tenant` confines the parent UPDATE to the child's own
    // tenant. An association without it emits no predicate at all, leaving a
    // single-tenant app's SQL unchanged.
    assert_eq!(
        CcTenantComment::counter_caches()[0].tenant_column,
        Some("tenant_id")
    );
    assert_eq!(CcComment::counter_caches()[0].tenant_column, None);

    // A model with no counter cache resolves to the empty blanket impl, so it
    // issues no extra statement on any mutation.
    assert!(CcPost::counter_caches().is_empty());
    const { assert!(!CcPost::HAS_COUNTER_CACHES) };
}

/// The recompute/backfill surface exists on the generated repository.
#[test]
fn recompute_methods_are_generated() {
    fn assert_is_fn<F>(_f: F) {}
    assert_is_fn(PgCcCommentRepository::recompute_counter_caches);
    assert_is_fn(PgCcCommentRepository::recompute_counter_caches_for);
}

// ── AC2: creating a child increments, in the same transaction ───────────────

/// AC2: `save` increments the parent's counter, and the maintained column
/// equals ground truth.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn save_increments_the_parent_counter() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "hello").await;

    for i in 0..3 {
        repo.save(&NewCcComment {
            body: format!("c{i}"),
            post_id: post,
        })
        .await
        .expect("save comment");
    }

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 3);
    assert_eq!(snap.persisted, snap.ground_truth);
}

/// AC2: the counter update and the row creation commit or roll back
/// **together**. The parent's counter column is `CHECK (… <= 2)`, so the third
/// insert's *increment* fails — and the child row must not survive it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_failing_counter_update_rolls_the_child_insert_back() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCappedRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let parent = seed_capped_parent(&mut conn).await;

    repo.save(&NewCcCapped { parent_id: parent })
        .await
        .expect("first");
    repo.save(&NewCcCapped { parent_id: parent })
        .await
        .expect("second");

    let err = repo.save(&NewCcCapped { parent_id: parent }).await;
    assert!(err.is_err(), "the CHECK on the counter column must fail");

    assert_eq!(
        row_count(&mut conn, "cc_cappeds", &format!("parent_id = {parent}")).await,
        2,
        "the child insert must roll back with the failed counter update — \
         a third row here means the two statements were not in one transaction"
    );
    assert_eq!(
        counter(&mut conn, "cc_capped_parents", "cc_capped_count", parent).await,
        2
    );
}

/// A child row whose foreign key is `NULL` in the database moves no counter and
/// does not error — the maintenance sub-selects filter `fk IS NOT NULL`, and the
/// recompute's correlated join simply never matches it.
///
/// The column is `NOT NULL` in every fixture (autumn's `#[belongs_to]` requires a
/// non-null `i64` foreign key today), so the NULL is introduced at the SQL layer,
/// which is exactly where the framework has to tolerate it: a legacy table can
/// carry NULLs a `NOT NULL` constraint has not been added to yet.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_null_foreign_key_moves_no_counter() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcMemberRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let team = seed_one_col(&mut conn, "cc_teams", "name", "core").await;

    diesel::sql_query("ALTER TABLE cc_members ALTER COLUMN team_id DROP NOT NULL")
        .execute(&mut conn)
        .await
        .expect("relax the constraint for this test");
    diesel::sql_query("INSERT INTO cc_members (nick, team_id) VALUES ('drifter', NULL)")
        .execute(&mut conn)
        .await
        .expect("seed an unparented member");

    repo.save(&NewCcMember {
        nick: "joiner".into(),
        team_id: team,
    })
    .await
    .expect("save assigned member");

    assert_eq!(
        counter(&mut conn, "cc_teams", "member_count", team).await,
        1,
        "only the assigned child counts"
    );

    // Recompute must reach the same answer, ignoring the NULL-parented row.
    repo.recompute_counter_caches().await.expect("recompute");
    assert_eq!(
        counter(&mut conn, "cc_teams", "member_count", team).await,
        1
    );
}

/// Two counter-cached legs on one child both move on a single insert.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn both_counter_cached_legs_move_on_one_insert() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcMessageRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let user = seed_one_col(&mut conn, "cc_users", "name", "ada").await;
    let room = seed_one_col(&mut conn, "cc_rooms", "topic", "general").await;

    repo.save(&NewCcMessage {
        body: "hi".into(),
        sender_id: user,
        room_id: room,
    })
    .await
    .expect("save message");

    assert_eq!(counter(&mut conn, "cc_users", "sent_count", user).await, 1);
    assert_eq!(
        counter(&mut conn, "cc_rooms", "cc_message_count", room).await,
        1
    );
}

/// `save_many` increments once per inserted child.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn save_many_increments_once_per_child() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let a = seed_post(&mut conn, "a").await;
    let b = seed_post(&mut conn, "b").await;

    repo.save_many(&[
        NewCcComment {
            body: "1".into(),
            post_id: a,
        },
        NewCcComment {
            body: "2".into(),
            post_id: a,
        },
        NewCcComment {
            body: "3".into(),
            post_id: b,
        },
    ])
    .await
    .expect("save_many");

    assert_eq!(post_snapshot(&mut conn, a).await.persisted, 2);
    assert_eq!(post_snapshot(&mut conn, b).await.persisted, 1);
    assert_eq!(total_post_drift(&mut conn).await, 0);
}

// ── AC3: destroying a child decrements ──────────────────────────────────────

/// AC3: a hard delete decrements.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_decrements_the_parent_counter() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "hello").await;

    let first = repo
        .save(&NewCcComment {
            body: "one".into(),
            post_id: post,
        })
        .await
        .expect("save");
    repo.save(&NewCcComment {
        body: "two".into(),
        post_id: post,
    })
    .await
    .expect("save");

    repo.delete_by_id(first.id).await.expect("delete");

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 1);
    assert_eq!(snap.persisted, snap.ground_truth);
}

/// `delete_many` decrements once per removed child.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn delete_many_decrements_once_per_child() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "hello").await;

    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(
            repo.save(&NewCcComment {
                body: format!("c{i}"),
                post_id: post,
            })
            .await
            .expect("save")
            .id,
        );
    }

    repo.delete_many(&ids[..3]).await.expect("delete_many");

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 1);
    assert_eq!(snap.persisted, snap.ground_truth);
}

/// AC3: soft-deleting a child decrements too, and the count then reflects only
/// **live** rows. Soft-deleting the same child again must not double-decrement.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn soft_delete_decrements_and_counts_only_live_rows() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcRevisionRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let page = seed_one_col(&mut conn, "cc_pages", "title", "home").await;

    let first = repo
        .save(&NewCcRevision { page_id: page })
        .await
        .expect("save");
    repo.save(&NewCcRevision { page_id: page })
        .await
        .expect("save");
    assert_eq!(
        counter(&mut conn, "cc_pages", "cc_revision_count", page).await,
        2
    );

    repo.delete_by_id(first.id).await.expect("soft delete");
    assert_eq!(
        counter(&mut conn, "cc_pages", "cc_revision_count", page).await,
        1,
        "a soft delete decrements"
    );
    assert_eq!(
        row_count(&mut conn, "cc_revisions", &format!("page_id = {page}")).await,
        2,
        "the row itself survives a soft delete"
    );

    // Deleting an already-soft-deleted row must not move the counter again.
    let _ = repo.delete_by_id(first.id).await;
    assert_eq!(
        counter(&mut conn, "cc_pages", "cc_revision_count", page).await,
        1,
        "re-deleting a soft-deleted child must not double-decrement"
    );
}

/// Restoring a soft-deleted child puts the count back.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn restore_increments_the_parent_counter_again() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcRevisionRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let page = seed_one_col(&mut conn, "cc_pages", "title", "home").await;

    let rev = repo
        .save(&NewCcRevision { page_id: page })
        .await
        .expect("save");
    repo.delete_by_id(rev.id).await.expect("soft delete");
    assert_eq!(
        counter(&mut conn, "cc_pages", "cc_revision_count", page).await,
        0
    );

    repo.restore(rev.id).await.expect("restore");
    assert_eq!(
        counter(&mut conn, "cc_pages", "cc_revision_count", page).await,
        1,
        "restore puts the count back"
    );

    // And restoring a live row must not inflate it.
    let _ = repo.restore(rev.id).await;
    assert_eq!(
        counter(&mut conn, "cc_pages", "cc_revision_count", page).await,
        1,
        "restoring an already-live child must not double-increment"
    );
}

// ── AC4: reassigning the foreign key moves the count ────────────────────────

/// AC4: `update`ing the child's parent decrements the old parent and increments
/// the new one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reassigning_the_parent_moves_the_count() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let old = seed_post(&mut conn, "old").await;
    let new = seed_post(&mut conn, "new").await;

    let comment = repo
        .save(&NewCcComment {
            body: "moving".into(),
            post_id: old,
        })
        .await
        .expect("save");
    assert_eq!(post_snapshot(&mut conn, old).await.persisted, 1);

    repo.update(
        comment.id,
        &UpdateCcComment {
            post_id: Patch::Set(new),
            ..Default::default()
        },
    )
    .await
    .expect("reassign");

    assert_eq!(post_snapshot(&mut conn, old).await.persisted, 0);
    assert_eq!(post_snapshot(&mut conn, new).await.persisted, 1);
    assert_eq!(total_post_drift(&mut conn).await, 0);
}

/// An update that does **not** touch the foreign key leaves the counters alone.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_update_that_keeps_the_parent_moves_nothing() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "stable").await;

    let comment = repo
        .save(&NewCcComment {
            body: "before".into(),
            post_id: post,
        })
        .await
        .expect("save");

    repo.update(
        comment.id,
        &UpdateCcComment {
            body: Patch::Set("after".into()),
            ..Default::default()
        },
    )
    .await
    .expect("edit body");

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 1);
    assert_eq!(snap.persisted, snap.ground_truth);
}

/// Re-parenting a child with two counter-cached legs moves only the leg that
/// actually changed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn only_the_changed_leg_moves() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcMessageRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let user = seed_one_col(&mut conn, "cc_users", "name", "ada").await;
    let old_room = seed_one_col(&mut conn, "cc_rooms", "topic", "old").await;
    let new_room = seed_one_col(&mut conn, "cc_rooms", "topic", "new").await;

    let msg = repo
        .save(&NewCcMessage {
            body: "hi".into(),
            sender_id: user,
            room_id: old_room,
        })
        .await
        .expect("save");

    repo.update(
        msg.id,
        &UpdateCcMessage {
            room_id: Patch::Set(new_room),
            ..Default::default()
        },
    )
    .await
    .expect("move room");

    assert_eq!(
        counter(&mut conn, "cc_users", "sent_count", user).await,
        1,
        "the sender leg did not change, so its counter must not move"
    );
    assert_eq!(
        counter(&mut conn, "cc_rooms", "cc_message_count", old_room).await,
        0
    );
    assert_eq!(
        counter(&mut conn, "cc_rooms", "cc_message_count", new_room).await,
        1
    );
}

/// `update_many` moves the count for every reassigned child.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn update_many_moves_every_reassigned_child() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let old = seed_post(&mut conn, "old").await;
    let new = seed_post(&mut conn, "new").await;

    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(
            repo.save(&NewCcComment {
                body: format!("c{i}"),
                post_id: old,
            })
            .await
            .expect("save")
            .id,
        );
    }

    repo.update_many(
        &ids,
        &UpdateCcComment {
            post_id: Patch::Set(new),
            ..Default::default()
        },
    )
    .await
    .expect("update_many");

    assert_eq!(post_snapshot(&mut conn, old).await.persisted, 0);
    assert_eq!(post_snapshot(&mut conn, new).await.persisted, 3);
    assert_eq!(total_post_drift(&mut conn).await, 0);
}

// ── AC5: atomic increment under concurrency ─────────────────────────────────

/// AC5: N concurrent inserts against one parent yield **exactly** N. A
/// read-modify-write implementation loses updates here; a single
/// `SET c = c + 1` cannot.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_inserts_yield_exactly_n() {
    const N: usize = 50;

    let (pool, _container) = setup_pool().await;
    let repo = Arc::new(PgCcCommentRepository::with_pool_untracked(pool.clone()));
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "storm").await;

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let repo = Arc::clone(&repo);
        handles.push(tokio::spawn(async move {
            repo.save(&NewCcComment {
                body: format!("c{i}"),
                post_id: post,
            })
            .await
            .expect("concurrent save");
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(
        snap.persisted,
        i64::try_from(N).expect("N fits in i64"),
        "concurrent inserts must not lose counter updates"
    );
    assert_eq!(snap.persisted, snap.ground_truth);
}

/// The mirror image: N concurrent deletes drain the counter back to exactly 0.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_deletes_drain_to_exactly_zero() {
    const N: usize = 30;

    let (pool, _container) = setup_pool().await;
    let repo = Arc::new(PgCcCommentRepository::with_pool_untracked(pool.clone()));
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "drain").await;

    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        ids.push(
            repo.save(&NewCcComment {
                body: format!("c{i}"),
                post_id: post,
            })
            .await
            .expect("save")
            .id,
        );
    }

    let mut handles = Vec::with_capacity(N);
    for id in ids {
        let repo = Arc::clone(&repo);
        handles.push(tokio::spawn(async move {
            repo.delete_by_id(id).await.expect("concurrent delete");
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 0);
    assert_eq!(snap.persisted, snap.ground_truth);
}

// ── AC6: recompute / backfill ───────────────────────────────────────────────

/// AC6: `recompute_counter_caches` repairs drift from the source of truth and
/// is idempotent — running it twice changes nothing the second time.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn recompute_repairs_drift_and_is_idempotent() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let a = seed_post(&mut conn, "a").await;
    let b = seed_post(&mut conn, "b").await;

    for i in 0..3 {
        repo.save(&NewCcComment {
            body: format!("a{i}"),
            post_id: a,
        })
        .await
        .expect("save");
    }

    // Drift injected the way a legacy table adopting the column would look:
    // rows that predate the counter, plus a counter nobody maintained.
    diesel::sql_query("INSERT INTO cc_comments (body, post_id) VALUES ('legacy', $1)")
        .bind::<BigInt, _>(b)
        .execute(&mut conn)
        .await
        .expect("legacy insert");
    diesel::sql_query("UPDATE cc_posts SET cc_comment_count = 99 WHERE id = $1")
        .bind::<BigInt, _>(a)
        .execute(&mut conn)
        .await
        .expect("inflate");
    assert_ne!(total_post_drift(&mut conn).await, 0, "drift is present");

    let touched = repo
        .recompute_counter_caches()
        .await
        .expect("recompute all parents");
    assert_eq!(touched, 2, "both drifted parents are repaired");

    assert_eq!(post_snapshot(&mut conn, a).await.persisted, 3);
    assert_eq!(post_snapshot(&mut conn, b).await.persisted, 1);
    assert_eq!(
        total_post_drift(&mut conn).await,
        0,
        "observed counter drift across all parents is 0 after recompute"
    );

    // Idempotent: a second run finds nothing to repair and leaves the same
    // values. The count is a *repair* count, so a healthy table returns 0 and
    // writes no rows at all.
    assert_eq!(
        repo.recompute_counter_caches()
            .await
            .expect("recompute again"),
        0,
        "a second sweep must repair nothing"
    );
    assert_eq!(post_snapshot(&mut conn, a).await.persisted, 3);
    assert_eq!(post_snapshot(&mut conn, b).await.persisted, 1);
    assert_eq!(total_post_drift(&mut conn).await, 0);
}

/// AC6: a single parent can be repaired without sweeping the whole table.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn recompute_for_repairs_only_the_named_parent() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let a = seed_post(&mut conn, "a").await;
    let b = seed_post(&mut conn, "b").await;

    diesel::sql_query("UPDATE cc_posts SET cc_comment_count = 7")
        .execute(&mut conn)
        .await
        .expect("inflate both");

    repo.recompute_counter_caches_for(a)
        .await
        .expect("recompute a");

    assert_eq!(post_snapshot(&mut conn, a).await.persisted, 0);
    assert_eq!(
        counter(&mut conn, "cc_posts", "cc_comment_count", b).await,
        7,
        "the untargeted parent is left alone"
    );
}

/// AC6: recompute counts only **live** rows for a soft-deleting child.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn recompute_ignores_soft_deleted_children() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcRevisionRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let page = seed_one_col(&mut conn, "cc_pages", "title", "home").await;

    let first = repo
        .save(&NewCcRevision { page_id: page })
        .await
        .expect("save");
    repo.save(&NewCcRevision { page_id: page })
        .await
        .expect("save");
    repo.delete_by_id(first.id).await.expect("soft delete");

    diesel::sql_query("UPDATE cc_pages SET cc_revision_count = 42")
        .execute(&mut conn)
        .await
        .expect("inflate");

    repo.recompute_counter_caches().await.expect("recompute");

    assert_eq!(
        counter(&mut conn, "cc_pages", "cc_revision_count", page).await,
        1,
        "the soft-deleted revision must not be counted"
    );
}

/// A repair sweep run against live traffic must not *introduce* the drift it
/// exists to remove.
///
/// Before the sweep locked each batch of parents ahead of counting them, this
/// lost increments on Postgres: a child's transaction takes the parent's row
/// lock for its `SET c = c + 1` and has not yet committed, so the sweep's
/// `UPDATE` queues behind that lock, then resumes and writes a `COUNT(*)` from a
/// snapshot taken before the child existed — silently overwriting the increment
/// that committed in between.
///
/// The assertion can only fail if that race is present: with the lock in place
/// both orderings converge, so this is a regression test rather than a flaky
/// stress test.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_repair_sweep_cannot_overwrite_a_concurrent_increment() {
    const N: usize = 40;
    const SWEEPS: usize = 25;

    let (pool, _container) = setup_pool().await;
    let repo = Arc::new(PgCcCommentRepository::with_pool_untracked(pool.clone()));
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "repaired under load").await;

    let mut handles = Vec::with_capacity(N + 1);
    for i in 0..N {
        let repo = Arc::clone(&repo);
        handles.push(tokio::spawn(async move {
            repo.save(&NewCcComment {
                body: format!("c{i}"),
                post_id: post,
            })
            .await
            .expect("concurrent save");
        }));
    }
    // Sweep repeatedly *while* the inserts are in flight, so a sweep lands in
    // the window between a child's INSERT and the counter UPDATE that follows it
    // in the same transaction.
    let sweeper = Arc::clone(&repo);
    handles.push(tokio::spawn(async move {
        for _ in 0..SWEEPS {
            sweeper
                .recompute_counter_caches()
                .await
                .expect("recompute under load");
        }
    }));
    for h in handles {
        h.await.expect("join");
    }

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(
        snap.persisted,
        i64::try_from(N).expect("N fits in i64"),
        "a repair sweep must never overwrite a committed increment"
    );
    assert_eq!(snap.persisted, snap.ground_truth);
}

// ── AC8: the in-process test client can assert the maintained value ─────────

/// AC8: create children, reload the parent through its own repository, and read
/// the maintained column off the model — the O(1) read a list view would do.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_parent_model_carries_the_maintained_count() {
    let (pool, _container) = setup_pool().await;
    let comments = PgCcCommentRepository::with_pool_untracked(pool.clone());
    let posts = PgCcPostRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post_id = seed_post(&mut conn, "hello").await;

    for i in 0..2 {
        comments
            .save(&NewCcComment {
                body: format!("c{i}"),
                post_id,
            })
            .await
            .expect("save");
    }

    let post = posts
        .find_by_id(post_id)
        .await
        .expect("reload")
        .expect("post exists");
    assert_eq!(post.cc_comment_count, 2);

    // Success Metric: rendering N parents with their counts is one query, not
    // N+1 — the count is already on each row `find_all` returns.
    let all = posts.find_all().await.expect("list");
    assert_eq!(all.iter().map(|p| p.cc_comment_count).sum::<i64>(), 2);
}

// ── Tenant isolation ────────────────────────────────────────────────────────

/// A tenant-scoped child whose foreign key names **another tenant's** parent
/// must not move that parent's counter.
///
/// The child row itself is already cross-tenant (nothing in the schema stops a
/// caller supplying a foreign key it does not own — a `REFERENCES` constraint
/// checks the id, not the tenant), so without a tenant predicate on the counter
/// `UPDATE` the write lands on a row the caller has no access to.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_cross_tenant_foreign_key_moves_no_counter() {
    let (pool, _container) = setup_pool().await;
    let repo = PgCcTenantCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");

    let mine = seed_tenant_post(&mut conn, "acme", "mine").await;
    let theirs = seed_tenant_post(&mut conn, "globex", "theirs").await;

    // Acting as tenant `acme`, aim the foreign key at `globex`'s post.
    CURRENT_TENANT
        .scope(Some("acme".to_owned()), async {
            repo.save(&NewCcTenantComment {
                body: "trespass".into(),
                post_id: theirs,
                // Overwritten by the tenant-scoped insert; the struct still
                // declares the column.
                tenant_id: "acme".into(),
            })
            .await
            .expect("the insert itself is not what we are testing");
            repo.save(&NewCcTenantComment {
                body: "legitimate".into(),
                post_id: mine,
                tenant_id: "acme".into(),
            })
            .await
            .expect("save");
        })
        .await;

    assert_eq!(
        counter(
            &mut conn,
            "cc_tenant_posts",
            "cc_tenant_comment_count",
            theirs
        )
        .await,
        0,
        "a cross-tenant foreign key must not move the other tenant's counter"
    );
    assert_eq!(
        counter(
            &mut conn,
            "cc_tenant_posts",
            "cc_tenant_comment_count",
            mine
        )
        .await,
        1,
        "the caller's own tenant still works"
    );
}

// ── The documented escape hatch for hand-written inserts ────────────────────

/// A hand-written insert inside the caller's own transaction can opt into the
/// same maintenance, so an application that does not go through the repository
/// still gets one line instead of a hand-rolled `count + 1`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_manual_escape_hatch_maintains_the_counter() {
    use diesel_async::AsyncConnection as _;

    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "manual").await;

    conn.transaction::<(), autumn_web::AutumnError, _>(async move |conn| {
        let id = diesel::sql_query(
            "INSERT INTO cc_comments (body, post_id) VALUES ('manual', $1) RETURNING id",
        )
        .bind::<BigInt, _>(post)
        .get_result::<IdRow>(&mut *conn)
        .await?
        .id;
        autumn_web::repository::counter_cache_after_insert_by_id(
            conn,
            CcComment::counter_caches(),
            id,
        )
        .await?;
        Ok(())
    })
    .await
    .expect("manual insert");

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 1);
    assert_eq!(snap.persisted, snap.ground_truth);
}
