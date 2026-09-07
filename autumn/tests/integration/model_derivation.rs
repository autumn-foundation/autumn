//! Database-level integration tests for maintained derived read models
//! (`#[derivation]`, issue #1769).
//!
//! A derivation is a counter cache with a filter and a per-row contribution, so
//! these tests are the counter-cache suite's siblings. They have the same shape,
//! but every assertion turns on two things: a row the filter rejects stays
//! invisible to the maintained value, and the contribution is weighted rather
//! than `+1` or `-1`.
//!
//! The behavioural tests need a Postgres. They prefer `AUTUMN_TEST_PG_URL` and
//! fall back to testcontainers, so they run both in CI's ignored-test sweep and
//! against a local server:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://postgres:postgres@127.0.0.1:54329/postgres \
//!   cargo test -p autumn-web --features test-support \
//!   --test integration_tests -- model_derivation --include-ignored
//! ```
//!
//! Because the fixture models bind table names at compile time, the shared-server
//! path cannot give each test its own tables. Every test therefore takes
//! [`DB_LOCK`] and truncates the fixture tables first, so the suite is safe
//! against a database it does not own.
//!
//! Fixtures, one per branch under test:
//!
//! | Child | Derivation | Parent column |
//! |---|---|---|
//! | `DvComment` | `filter = published` | `dv_posts.published_comment_count` |
//! | `DvComment` | `transform = sum(score), filter = published && score > 0` | `dv_posts.visible_score` |
//! | `DvCappedComment` | `filter = published`, onto a `CHECK`ed column | `dv_capped_posts.capped_count` |
//! | `DvRevision` | `filter = published` on a `soft_delete` repository | `dv_pages.live_revision_count` |
//!
//! `DvCappedComment` exists to make **same-transaction** atomicity observable.
//! Its parent's column carries `CHECK (capped_count <= 3)`, so the fourth
//! published insert fails on the *derivation update*. If that update ran outside
//! the insert's transaction the child row would survive. Because it is inside,
//! the whole thing rolls back. The same `CHECK` also makes a backfill batch fail
//! on demand, which is how the abort-inside-a-batch test works.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::sync::Arc;

use autumn_web::Patch;
use autumn_web::derivation::{
    BackfillOptions, BackfillState, DerivationDef, derivation_status, drift, ensure_derivations,
    recompute, registered_derivations, run_backfill,
};
use autumn_web::repository::{AutumnCounterCaches as _, counter_cache_after_insert_by_id};
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Filtered count + filtered sum on one child ──────────────────────────────

diesel::table! {
    dv_posts (id) {
        id -> Int8,
        title -> Text,
        published_comment_count -> Int8,
        visible_score -> Int8,
    }
}

#[autumn_web::model(table = "dv_posts")]
pub struct DvPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub published_comment_count: i64,
    #[default]
    pub visible_score: i64,
}

#[autumn_web::repository(DvPost, table = "dv_posts")]
pub trait DvPostRepository {}

diesel::table! {
    dv_comments (id) {
        id -> Int8,
        post_id -> Int8,
        published -> Bool,
        score -> Int8,
    }
}

/// Two derivations off one child: a filtered count and a filtered weighted sum.
/// One insert has to move both, and each filter has to be honoured
/// independently — a published comment with a negative score counts towards the
/// first and not the second.
#[autumn_web::model(table = "dv_comments")]
#[belongs_to(DvPost, fk = post_id)]
#[derivation(DvPost, column = "published_comment_count", filter = published)]
#[derivation(DvPost, column = "visible_score", transform = sum(score), filter = published && score > 0)]
pub struct DvComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub score: i64,
}

#[autumn_web::repository(DvComment, table = "dv_comments")]
pub trait DvCommentRepository {}

// ── Atomicity fixture: the derived column itself is `CHECK`ed ───────────────

diesel::table! {
    dv_capped_posts (id) {
        id -> Int8,
        label -> Text,
        capped_count -> Int8,
    }
}

#[autumn_web::model(table = "dv_capped_posts")]
pub struct DvCappedPost {
    #[id]
    pub id: i64,
    pub label: String,
    #[default]
    pub capped_count: i64,
}

#[autumn_web::repository(DvCappedPost, table = "dv_capped_posts")]
pub trait DvCappedPostRepository {}

diesel::table! {
    dv_capped_comments (id) {
        id -> Int8,
        post_id -> Int8,
        published -> Bool,
    }
}

#[autumn_web::model(table = "dv_capped_comments")]
#[belongs_to(DvCappedPost, fk = post_id)]
#[derivation(DvCappedPost, column = "capped_count", filter = published)]
pub struct DvCappedComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
}

#[autumn_web::repository(DvCappedComment, table = "dv_capped_comments")]
pub trait DvCappedCommentRepository {}

// ── Soft-deleting child ─────────────────────────────────────────────────────

diesel::table! {
    dv_pages (id) {
        id -> Int8,
        title -> Text,
        live_revision_count -> Int8,
    }
}

#[autumn_web::model(table = "dv_pages")]
pub struct DvPage {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub live_revision_count: i64,
}

#[autumn_web::repository(DvPage, table = "dv_pages")]
pub trait DvPageRepository {}

diesel::table! {
    dv_revisions (id) {
        id -> Int8,
        page_id -> Int8,
        published -> Bool,
        deleted_at -> Nullable<Timestamp>,
    }
}

/// The two exclusions have to compose: a revision counts only while it is both
/// live and published.
#[autumn_web::model(table = "dv_revisions")]
#[belongs_to(DvPage, fk = page_id)]
#[derivation(DvPage, column = "live_revision_count", filter = published)]
pub struct DvRevision {
    #[id]
    pub id: i64,
    pub page_id: i64,
    pub published: bool,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(DvRevision, table = "dv_revisions", soft_delete)]
pub trait DvRevisionRepository {}

// ── Setup & helpers ─────────────────────────────────────────────────────────

const COUNT_DERIVATION: &str = "dv_posts.published_comment_count";
const SUM_DERIVATION: &str = "dv_posts.visible_score";

/// The state table's own DDL, taken from the migration the framework ships, so
/// these tests exercise the shipped statement rather than a copy of it.
const DERIVATIONS_DDL: &str =
    include_str!("../../derivation_migrations/20260907000000_create_derivations/up.sql");

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS dv_posts \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      published_comment_count BIGINT NOT NULL DEFAULT 0, \
      visible_score BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS dv_comments \
     (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL REFERENCES dv_posts(id), \
      published BOOLEAN NOT NULL DEFAULT FALSE, score BIGINT NOT NULL DEFAULT 0)",
    // The `CHECK` is on the *derived* column, so the derivation update is what
    // fails on the fourth published insert.
    "CREATE TABLE IF NOT EXISTS dv_capped_posts \
     (id BIGSERIAL PRIMARY KEY, label TEXT NOT NULL, \
      capped_count BIGINT NOT NULL DEFAULT 0 CHECK (capped_count <= 3))",
    "CREATE TABLE IF NOT EXISTS dv_capped_comments \
     (id BIGSERIAL PRIMARY KEY, \
      post_id BIGINT NOT NULL REFERENCES dv_capped_posts(id), \
      published BOOLEAN NOT NULL DEFAULT FALSE)",
    "CREATE TABLE IF NOT EXISTS dv_pages \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      live_revision_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS dv_revisions \
     (id BIGSERIAL PRIMARY KEY, page_id BIGINT NOT NULL REFERENCES dv_pages(id), \
      published BOOLEAN NOT NULL DEFAULT FALSE, deleted_at TIMESTAMP NULL)",
];

/// Serializes the suite against a Postgres it may not own.
///
/// `AUTUMN_TEST_PG_URL` points every test at one database, and the fixture
/// models bind their table names at compile time, so per-test tables are not an
/// option. One lock plus a truncate per test is.
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Keeps whichever backing Postgres alive for the duration of the test.
enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    External,
}

async fn start_postgres() -> (PgHandle, Pool<AsyncPgConnection>) {
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        // Comfortably exceeds the 50 concurrent callers in the race test plus
        // the observer connection, so no caller queues on pool acquisition.
        let pool = Pool::builder(manager)
            .max_size(60)
            .build()
            .expect("build pool");
        return (PgHandle::External, pool);
    }
    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Postgres port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager)
        .max_size(60)
        .build()
        .expect("build pool");
    (PgHandle::Container(Box::new(container)), pool)
}

/// A pool over an empty schema, plus the guard that keeps this test alone in it.
async fn setup() -> (
    tokio::sync::MutexGuard<'static, ()>,
    PgHandle,
    Pool<AsyncPgConnection>,
) {
    let guard = DB_LOCK.lock().await;
    let (handle, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    create_schema(&mut conn).await;
    conn.batch_execute(DERIVATIONS_DDL)
        .await
        .expect("derivation state table DDL");
    conn.batch_execute(
        "TRUNCATE dv_comments, dv_posts, dv_capped_comments, dv_capped_posts, \
         dv_revisions, dv_pages RESTART IDENTITY CASCADE; \
         DELETE FROM _autumn_derivations;",
    )
    .await
    .expect("reset fixture tables");
    drop(conn);
    (guard, handle, pool)
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

/// The maintained column and its ground truth in **one** statement, so both
/// necessarily come from the same snapshot.
#[derive(diesel::QueryableByName)]
struct SnapshotRow {
    #[diesel(sql_type = BigInt)]
    persisted_count: i64,
    #[diesel(sql_type = BigInt)]
    truth_count: i64,
    #[diesel(sql_type = BigInt)]
    persisted_sum: i64,
    #[diesel(sql_type = BigInt)]
    truth_sum: i64,
}

async fn seed_post(conn: &mut AsyncPgConnection, title: &str) -> i64 {
    diesel::sql_query("INSERT INTO dv_posts (title) VALUES ($1) RETURNING id")
        .bind::<Text, _>(title)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed post")
        .id
}

async fn seed_one_col(conn: &mut AsyncPgConnection, table: &str, column: &str, value: &str) -> i64 {
    diesel::sql_query(format!(
        "INSERT INTO {table} ({column}) VALUES ($1) RETURNING id"
    ))
    .bind::<Text, _>(value)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed row")
    .id
}

/// Read a derived column directly, bypassing the repository under test.
async fn derived(conn: &mut AsyncPgConnection, table: &str, column: &str, id: i64) -> i64 {
    diesel::sql_query(format!(
        "SELECT {column} AS count FROM {table} WHERE id = $1"
    ))
    .bind::<BigInt, _>(id)
    .get_result::<CountRow>(conn)
    .await
    .expect("read derived column")
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

/// Both of a post's derived columns alongside both ground truths.
async fn post_snapshot(conn: &mut AsyncPgConnection, post_id: i64) -> SnapshotRow {
    diesel::sql_query(
        "SELECT p.published_comment_count AS persisted_count, \
         (SELECT COUNT(*) FROM dv_comments c \
          WHERE c.post_id = p.id AND c.published)::BIGINT AS truth_count, \
         p.visible_score AS persisted_sum, \
         (SELECT COALESCE(SUM(c.score), 0) FROM dv_comments c \
          WHERE c.post_id = p.id AND c.published AND c.score > 0)::BIGINT AS truth_sum \
         FROM dv_posts p WHERE p.id = $1",
    )
    .bind::<BigInt, _>(post_id)
    .get_result::<SnapshotRow>(conn)
    .await
    .expect("read post snapshot")
}

/// Every fixture table, child before parent.
const FIXTURE_TABLES: &[&str] = &[
    "dv_comments",
    "dv_posts",
    "dv_capped_comments",
    "dv_capped_posts",
    "dv_revisions",
    "dv_pages",
];

/// Create the fixture schema.
///
/// On the shared-server path the tables are dropped first, following the
/// reddit-clone precedent: a table left by an older revision of this suite would
/// otherwise keep a column this one has since changed, and one test here drops a
/// column on purpose. On the testcontainer path each test gets its own empty
/// database, so the drops are no-ops.
async fn create_schema(conn: &mut AsyncPgConnection) {
    if std::env::var("AUTUMN_TEST_PG_URL").is_ok() {
        for table in FIXTURE_TABLES {
            diesel::sql_query(format!("DROP TABLE IF EXISTS {table} CASCADE"))
                .execute(conn)
                .await
                .unwrap_or_else(|e| panic!("could not drop `{table}`: {e}"));
        }
    }
    for stmt in DDL {
        diesel::sql_query(*stmt)
            .execute(conn)
            .await
            .unwrap_or_else(|e| panic!("DDL failed ({stmt}): {e}"));
    }
}

/// Replace `dv_posts` with `total` parents, seeded **through the repository**.
///
/// Each parent gets two comments saved by `PgDvCommentRepository`: one published
/// with score 5 and one draft with score 100. So every parent ends at
/// `published_comment_count = 1` and `visible_score = 5`, and those values were
/// produced by the maintenance path this suite is about. Seeding the columns
/// directly, as this helper used to, made the read assertion unable to fail on a
/// regression in that path.
///
/// Ids restart at 1, so the expected rows are `(1..=total, 1, 5)`.
async fn reseed_posts_through_the_repository(pool: &Pool<AsyncPgConnection>, total: usize) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("TRUNCATE dv_comments, dv_posts RESTART IDENTITY CASCADE")
        .execute(&mut conn)
        .await
        .expect("reset");
    let mut posts = Vec::with_capacity(total);
    for i in 0..total {
        posts.push(seed_post(&mut conn, &format!("p{i}")).await);
    }
    drop(conn);

    let comments = PgDvCommentRepository::with_pool_untracked(pool.clone());
    for post in posts {
        comments
            .save(&NewDvComment {
                post_id: post,
                published: true,
                score: 5,
            })
            .await
            .expect("save the published comment");
        comments
            .save(&NewDvComment {
                post_id: post,
                published: false,
                score: 100,
            })
            .await
            .expect("save the draft");
    }
}

fn def(name: &str) -> &'static DerivationDef {
    registered_derivations()
        .into_iter()
        .find(|def| def.name == name)
        .unwrap_or_else(|| panic!("`{name}` must be a registered derivation"))
}

/// Record every registered derivation as backfilled and current, the state a
/// database that has already been through a boot is in.
async fn mark_all_complete(conn: &mut AsyncPgConnection) {
    for def in registered_derivations() {
        diesel::sql_query(
            "INSERT INTO _autumn_derivations \
               (name, definition_hash, backfill_state, checkpoint, backfilled_rows) \
             VALUES ($1, $2, 'complete', NULL, 0) \
             ON CONFLICT (name) DO UPDATE SET \
               definition_hash = excluded.definition_hash, backfill_state = 'complete'",
        )
        .bind::<Text, _>(def.name)
        .bind::<Text, _>(def.definition_hash())
        .execute(conn)
        .await
        .expect("seed derivation state");
    }
}

#[derive(diesel::QueryableByName)]
struct StateRow {
    #[diesel(sql_type = Text)]
    backfill_state: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<BigInt>)]
    checkpoint: Option<i64>,
    #[diesel(sql_type = BigInt)]
    backfilled_rows: i64,
}

async fn state_of(conn: &mut AsyncPgConnection, name: &str) -> StateRow {
    diesel::sql_query(
        "SELECT backfill_state, checkpoint, backfilled_rows \
         FROM _autumn_derivations WHERE name = $1",
    )
    .bind::<Text, _>(name)
    .get_result::<StateRow>(conn)
    .await
    .unwrap_or_else(|e| panic!("no state row for `{name}`: {e}"))
}

// ── Non-ignored: the generated surface exists without a live database ───────

/// The compile-time metadata is exactly what every statement is built from, so
/// pinning it here pins the SQL — the non-ignored guard for the Docker tests
/// below.
#[test]
fn derivation_defs_are_generated_from_the_declarations() {
    // Two derivations off one child, in declaration order, each carrying the
    // filter lowered to SQL and its own contribution.
    let specs = DvComment::counter_caches();
    assert_eq!(specs.len(), 2, "one spec per derivation");
    const { assert!(DvComment::HAS_COUNTER_CACHES) };

    let count = specs[0];
    assert_eq!(count.parent_table, "dv_posts");
    assert_eq!(count.counter_column, "published_comment_count");
    assert_eq!(count.child_table, "dv_comments");
    assert_eq!(count.fk_column, "post_id");
    assert_eq!(count.contrib_sql, "1", "a count weighs every row 1");
    assert!(
        count.filter_sql.starts_with(" AND ("),
        "a lowered filter is concatenable: {:?}",
        count.filter_sql
    );
    assert!(
        count.filter_sql.contains("{c}"),
        "the child alias is a placeholder each statement resolves: {:?}",
        count.filter_sql
    );

    let sum = specs[1];
    assert_eq!(sum.counter_column, "visible_score");
    assert!(
        sum.contrib_sql.contains("score"),
        "a sum weighs each row by its field: {:?}",
        sum.contrib_sql
    );

    // The Rust half of the filter has to agree with the SQL half, or the
    // record-shaped paths and the set-based ones drift apart.
    let unpublished = DvComment {
        id: 1,
        post_id: 7,
        published: false,
        score: 5,
    };
    let published = DvComment {
        published: true,
        ..unpublished
    };
    assert_eq!((count.contrib_of)(&unpublished), 0);
    assert_eq!((count.contrib_of)(&published), 1);
    assert_eq!((sum.contrib_of)(&unpublished), 0);
    assert_eq!((sum.contrib_of)(&published), 5);
    let negative = DvComment {
        published: true,
        score: -5,
        ..unpublished
    };
    assert_eq!(
        (count.contrib_of)(&negative),
        1,
        "the two filters are independent"
    );
    assert_eq!((sum.contrib_of)(&negative), 0);

    // Each spec points at the def the actuator and the backfill read.
    let registered = def(COUNT_DERIVATION);
    assert_eq!(
        count.derivation.map(|d| d.name),
        Some(registered.name),
        "the spec and the registry must name one definition, not two"
    );
    assert_eq!(registered.transform, "count");
    assert_eq!(def(SUM_DERIVATION).transform, "sum(score)");
    assert_eq!(registered.parent_table, "dv_posts");
    assert_eq!(registered.child_pk, "id");
    assert!(!registered.child_soft_delete);
    assert!(def("dv_pages.live_revision_count").child_soft_delete);

    let hash = registered.definition_hash();
    assert_eq!(hash.len(), 64, "sha256 renders as 64 hex characters: {hash}");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "the definition hash is a lowercase hex content address: {hash}"
    );
    assert_ne!(
        hash,
        def(SUM_DERIVATION).definition_hash(),
        "two derivations on one column pair must not share a content address"
    );

    // A plain counter cache stays neutral, so its SQL is unchanged.
    assert!(DvPost::counter_caches().is_empty());
}

// ── AC1: the filter decides what counts, the transform decides its weight ───

/// AC1: only published comments count, and only positive-scoring published ones
/// are summed.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac1_a_filtered_count_and_sum_ignore_rows_the_filter_rejects() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "hello").await;

    for (published, score) in [(true, 5), (true, 7), (false, 100), (true, -3)] {
        repo.save(&NewDvComment {
            post_id: post,
            published,
            score,
        })
        .await
        .expect("save comment");
    }

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(
        snap.persisted_count, 3,
        "the unpublished comment is invisible"
    );
    assert_eq!(snap.persisted_count, snap.truth_count);
    assert_eq!(
        snap.persisted_sum, 12,
        "only published, positively-scored comments are summed"
    );
    assert_eq!(snap.persisted_sum, snap.truth_sum);
    assert_eq!(
        drift(&mut conn, def(COUNT_DERIVATION))
            .await
            .expect("drift"),
        0
    );
    assert_eq!(
        drift(&mut conn, def(SUM_DERIVATION)).await.expect("drift"),
        0
    );
}

/// `save_many` folds a batch into one statement per parent and still honours
/// both filters.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac1_save_many_honours_the_filter_per_row() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let a = seed_post(&mut conn, "a").await;
    let b = seed_post(&mut conn, "b").await;

    repo.save_many(&[
        NewDvComment {
            post_id: a,
            published: true,
            score: 2,
        },
        NewDvComment {
            post_id: a,
            published: false,
            score: 50,
        },
        NewDvComment {
            post_id: b,
            published: true,
            score: 4,
        },
    ])
    .await
    .expect("save_many");

    let first = post_snapshot(&mut conn, a).await;
    assert_eq!((first.persisted_count, first.persisted_sum), (1, 2));
    assert_eq!(first.persisted_count, first.truth_count);
    assert_eq!(first.persisted_sum, first.truth_sum);
    let second = post_snapshot(&mut conn, b).await;
    assert_eq!((second.persisted_count, second.persisted_sum), (1, 4));
}

/// A soft-deleting child counts only while it is both live and published, and a
/// restore puts it back.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac1_a_soft_deleted_row_stops_counting_and_a_restore_resumes() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvRevisionRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let page = seed_one_col(&mut conn, "dv_pages", "title", "home").await;

    let live = repo
        .save(&NewDvRevision {
            page_id: page,
            published: true,
        })
        .await
        .expect("save published");
    repo.save(&NewDvRevision {
        page_id: page,
        published: false,
    })
    .await
    .expect("save draft");
    assert_eq!(
        derived(&mut conn, "dv_pages", "live_revision_count", page).await,
        1,
        "the draft never counted"
    );

    repo.delete_by_id(live.id).await.expect("soft delete");
    assert_eq!(
        derived(&mut conn, "dv_pages", "live_revision_count", page).await,
        0
    );
    assert_eq!(
        row_count(&mut conn, "dv_revisions", &format!("page_id = {page}")).await,
        2,
        "the row itself survives a soft delete"
    );

    repo.restore(live.id).await.expect("restore");
    assert_eq!(
        derived(&mut conn, "dv_pages", "live_revision_count", page).await,
        1
    );
    // A second restore must not inflate it.
    let _ = repo.restore(live.id).await;
    assert_eq!(
        derived(&mut conn, "dv_pages", "live_revision_count", page).await,
        1
    );
}

// ── AC2: the derived write is in the row mutation's transaction ─────────────

/// AC2: the derivation update and the row insert commit or roll back
/// **together**. The parent's column is `CHECK (capped_count <= 3)`, so the
/// fourth published insert's *derivation update* fails — and the child row must
/// not survive it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac2_a_failing_derived_update_rolls_the_child_insert_back() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCappedCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_one_col(&mut conn, "dv_capped_posts", "label", "capped").await;

    for _ in 0..3 {
        repo.save(&NewDvCappedComment {
            post_id: post,
            published: true,
        })
        .await
        .expect("published insert under the cap");
    }
    // An unpublished comment contributes nothing, so the cap does not see it —
    // the filter is enforced by the same statement the CHECK guards.
    repo.save(&NewDvCappedComment {
        post_id: post,
        published: false,
    })
    .await
    .expect("an unpublished comment moves nothing, so the cap is not reached");

    let err = repo
        .save(&NewDvCappedComment {
            post_id: post,
            published: true,
        })
        .await;
    assert!(
        err.is_err(),
        "the CHECK on the derived column must fail the fourth published insert"
    );

    assert_eq!(
        row_count(
            &mut conn,
            "dv_capped_comments",
            &format!("post_id = {post} AND published")
        )
        .await,
        3,
        "the child insert must roll back with the failed derivation update — a \
         fourth published row here means the two statements were not in one \
         transaction"
    );
    assert_eq!(
        derived(&mut conn, "dv_capped_posts", "capped_count", post).await,
        3
    );
}

/// AC2, the explicit form: an aborted transaction leaves the derived value
/// exactly as it was, even though the derivation update ran inside it.
///
/// Drives the documented escape hatch — a hand-written insert plus
/// `counter_cache_after_insert_by_id` — because that is the path an application
/// can reach without the generated repository, and it is where a maintenance
/// write outside the caller's transaction would show up.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac2_an_aborted_transaction_leaves_the_derived_value_unchanged() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "rollback").await;

    repo.save(&NewDvComment {
        post_id: post,
        published: true,
        score: 3,
    })
    .await
    .expect("committed comment");
    let before = post_snapshot(&mut conn, post).await;
    assert_eq!((before.persisted_count, before.persisted_sum), (1, 3));

    conn.batch_execute("BEGIN").await.expect("begin");
    let id = diesel::sql_query(
        "INSERT INTO dv_comments (post_id, published, score) \
         VALUES ($1, TRUE, 9) RETURNING id",
    )
    .bind::<BigInt, _>(post)
    .get_result::<IdRow>(&mut conn)
    .await
    .expect("hand insert")
    .id;
    counter_cache_after_insert_by_id(&mut conn, DvComment::counter_caches(), id)
        .await
        .expect("maintain both derivations");
    // Inside the transaction the maintenance is visible…
    assert_eq!(
        derived(&mut conn, "dv_posts", "visible_score", post).await,
        12
    );
    conn.batch_execute("ROLLBACK").await.expect("rollback");

    // …and outside it, nothing happened at all.
    let after = post_snapshot(&mut conn, post).await;
    assert_eq!((after.persisted_count, after.persisted_sum), (1, 3));
    assert_eq!(after.persisted_count, after.truth_count);
    assert_eq!(after.persisted_sum, after.truth_sum);
    assert_eq!(
        row_count(&mut conn, "dv_comments", &format!("post_id = {post}")).await,
        1
    );
}

// ── AC3: concurrency ───────────────────────────────────────────────────────

/// AC3: N concurrent published inserts yield **exactly** N and no drift. The
/// increment is one `SET c = c + $1`, so the N writers commute under every
/// interleaving; a read-modify-write loses updates here.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac3_concurrent_inserts_yield_exactly_n_with_zero_drift() {
    const N: usize = 50;

    let (_guard, _pg, pool) = setup().await;
    let repo = Arc::new(PgDvCommentRepository::with_pool_untracked(pool.clone()));
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "storm").await;

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let repo = Arc::clone(&repo);
        handles.push(tokio::spawn(async move {
            repo.save(&NewDvComment {
                post_id: post,
                published: true,
                score: 1,
            })
            .await
            .unwrap_or_else(|e| panic!("concurrent save {i}: {e}"));
        }));
    }
    for handle in handles {
        handle.await.expect("join");
    }

    let expected = i64::try_from(N).expect("N fits in i64");
    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(
        snap.persisted_count, expected,
        "concurrent inserts must not lose derived updates"
    );
    assert_eq!(snap.persisted_count, snap.truth_count);
    assert_eq!(snap.persisted_sum, expected);
    assert_eq!(snap.persisted_sum, snap.truth_sum);
    assert_eq!(
        drift(&mut conn, def(COUNT_DERIVATION))
            .await
            .expect("drift"),
        0
    );
    assert_eq!(
        drift(&mut conn, def(SUM_DERIVATION)).await.expect("drift"),
        0
    );
}

// ── AC4: updates move exactly what changed ─────────────────────────────────

/// AC4: reassigning the parent moves both derivations off the old parent and
/// onto the new one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac4_reparenting_moves_the_old_and_the_new_parent() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let old = seed_post(&mut conn, "old").await;
    let new = seed_post(&mut conn, "new").await;

    let comment = repo
        .save(&NewDvComment {
            post_id: old,
            published: true,
            score: 6,
        })
        .await
        .expect("save");
    // An unpublished sibling that moves with it must move nothing either way.
    let draft = repo
        .save(&NewDvComment {
            post_id: old,
            published: false,
            score: 99,
        })
        .await
        .expect("save draft");

    repo.update(
        comment.id,
        &UpdateDvComment {
            post_id: Patch::Set(new),
            ..Default::default()
        },
    )
    .await
    .expect("reparent");
    repo.update(
        draft.id,
        &UpdateDvComment {
            post_id: Patch::Set(new),
            ..Default::default()
        },
    )
    .await
    .expect("reparent the draft");

    let from = post_snapshot(&mut conn, old).await;
    assert_eq!((from.persisted_count, from.persisted_sum), (0, 0));
    let to = post_snapshot(&mut conn, new).await;
    assert_eq!((to.persisted_count, to.persisted_sum), (1, 6));
    assert_eq!(to.persisted_count, to.truth_count);
    assert_eq!(to.persisted_sum, to.truth_sum);
}

/// AC4: a filter flip on an unchanged parent is `+1`, and flipping back is
/// `-1`. This is the case a foreign-key diff alone cannot see: nothing about
/// the parent changed, only whether the row qualifies.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac4_a_filter_flip_on_the_same_parent_moves_the_derived_value() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "flip").await;

    let comment = repo
        .save(&NewDvComment {
            post_id: post,
            published: false,
            score: 4,
        })
        .await
        .expect("save draft");
    let start = post_snapshot(&mut conn, post).await;
    assert_eq!((start.persisted_count, start.persisted_sum), (0, 0));

    repo.update(
        comment.id,
        &UpdateDvComment {
            published: Patch::Set(true),
            ..Default::default()
        },
    )
    .await
    .expect("publish");
    let published = post_snapshot(&mut conn, post).await;
    assert_eq!((published.persisted_count, published.persisted_sum), (1, 4));
    assert_eq!(published.persisted_count, published.truth_count);
    assert_eq!(published.persisted_sum, published.truth_sum);

    repo.update(
        comment.id,
        &UpdateDvComment {
            published: Patch::Set(false),
            ..Default::default()
        },
    )
    .await
    .expect("unpublish");
    let withdrawn = post_snapshot(&mut conn, post).await;
    assert_eq!((withdrawn.persisted_count, withdrawn.persisted_sum), (0, 0));
    assert_eq!(withdrawn.persisted_sum, withdrawn.truth_sum);
}

/// AC4: editing the summed field on a qualifying row moves the sum by the
/// difference, and leaves the count alone.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac4_editing_the_summed_field_moves_the_sum_by_the_difference() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "rescore").await;

    let comment = repo
        .save(&NewDvComment {
            post_id: post,
            published: true,
            score: 3,
        })
        .await
        .expect("save");

    repo.update(
        comment.id,
        &UpdateDvComment {
            score: Patch::Set(10),
            ..Default::default()
        },
    )
    .await
    .expect("rescore");

    let snap = post_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted_sum, 10, "the sum moves by the difference");
    assert_eq!(snap.persisted_count, 1, "the count did not change");
    assert_eq!(snap.persisted_sum, snap.truth_sum);

    // Dropping below the sum's own threshold takes the whole contribution off,
    // while the count keeps it.
    repo.update(
        comment.id,
        &UpdateDvComment {
            score: Patch::Set(-2),
            ..Default::default()
        },
    )
    .await
    .expect("downvote");
    let after = post_snapshot(&mut conn, post).await;
    assert_eq!((after.persisted_count, after.persisted_sum), (1, 0));
    assert_eq!(after.persisted_sum, after.truth_sum);
}

/// AC4: deleting a row the filter rejects moves nothing, however it is deleted.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac4_deleting_an_unqualified_row_moves_nothing() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&mut conn, "delete").await;

    let kept = repo
        .save(&NewDvComment {
            post_id: post,
            published: true,
            score: 5,
        })
        .await
        .expect("save published");
    let draft = repo
        .save(&NewDvComment {
            post_id: post,
            published: false,
            score: 90,
        })
        .await
        .expect("save draft");
    let second_draft = repo
        .save(&NewDvComment {
            post_id: post,
            published: false,
            score: 91,
        })
        .await
        .expect("save second draft");

    repo.delete_by_id(draft.id).await.expect("delete the draft");
    let after_single = post_snapshot(&mut conn, post).await;
    assert_eq!(
        (after_single.persisted_count, after_single.persisted_sum),
        (1, 5)
    );

    // The bulk path computes its delta with one aggregate, so it has to filter
    // too — otherwise a batch of drafts would drive the value negative.
    repo.delete_many(&[second_draft.id])
        .await
        .expect("delete_many the drafts");
    let after_bulk = post_snapshot(&mut conn, post).await;
    assert_eq!(
        (after_bulk.persisted_count, after_bulk.persisted_sum),
        (1, 5)
    );
    assert_eq!(after_bulk.persisted_count, after_bulk.truth_count);
    assert_eq!(after_bulk.persisted_sum, after_bulk.truth_sum);

    // And deleting the qualifying row does move both.
    repo.delete_by_id(kept.id).await.expect("delete published");
    let drained = post_snapshot(&mut conn, post).await;
    assert_eq!((drained.persisted_count, drained.persisted_sum), (0, 0));
}

/// AC4: `update_many` moves every reassigned child, filters included.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac4_update_many_moves_only_the_qualifying_children() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgDvCommentRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let old = seed_post(&mut conn, "old").await;
    let new = seed_post(&mut conn, "new").await;

    let mut ids = Vec::new();
    for (published, score) in [(true, 2), (true, 3), (false, 40)] {
        ids.push(
            repo.save(&NewDvComment {
                post_id: old,
                published,
                score,
            })
            .await
            .expect("save")
            .id,
        );
    }
    let start = post_snapshot(&mut conn, old).await;
    assert_eq!((start.persisted_count, start.persisted_sum), (2, 5));

    repo.update_many(
        &ids,
        &UpdateDvComment {
            post_id: Patch::Set(new),
            ..Default::default()
        },
    )
    .await
    .expect("update_many");

    let from = post_snapshot(&mut conn, old).await;
    assert_eq!((from.persisted_count, from.persisted_sum), (0, 0));
    let to = post_snapshot(&mut conn, new).await;
    assert_eq!((to.persisted_count, to.persisted_sum), (2, 5));
    assert_eq!(to.persisted_count, to.truth_count);
    assert_eq!(to.persisted_sum, to.truth_sum);
}

// ── AC5: only a changed definition is enqueued ─────────────────────────────

/// AC5: a stored hash that no longer matches enqueues **that** derivation and
/// leaves its siblings exactly as they were.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac5_only_the_changed_derivation_is_enqueued() {
    let (_guard, _pg, pool) = setup().await;
    let mut conn = pool.get().await.expect("conn");
    mark_all_complete(&mut conn).await;

    // A boot against an unchanged database enqueues nothing at all.
    assert!(
        ensure_derivations(&mut conn)
            .await
            .expect("reconcile an unchanged database")
            .is_empty(),
        "an unchanged definition must not re-backfill"
    );

    // Now one definition changes under us: the recorded hash is stale.
    diesel::sql_query("UPDATE _autumn_derivations SET definition_hash = 'stale' WHERE name = $1")
        .bind::<Text, _>(COUNT_DERIVATION)
        .execute(&mut conn)
        .await
        .expect("stale the stored hash");

    let enqueued = ensure_derivations(&mut conn).await.expect("reconcile");
    assert_eq!(
        enqueued,
        vec![COUNT_DERIVATION],
        "only the derivation whose definition changed may be enqueued"
    );

    let changed = state_of(&mut conn, COUNT_DERIVATION).await;
    assert_eq!(changed.backfill_state, "pending");
    assert_eq!(
        changed.checkpoint, None,
        "a changed definition invalidates every parent the old one repaired"
    );
    assert_eq!(changed.backfilled_rows, 0);

    let sibling = state_of(&mut conn, SUM_DERIVATION).await;
    assert_eq!(
        sibling.backfill_state, "complete",
        "a sibling on the same tables must be left alone"
    );

    // And the row now records the definition this binary actually runs.
    let status = derivation_status(&mut conn).await.expect("status");
    let entry = status
        .iter()
        .find(|entry| entry.name == COUNT_DERIVATION)
        .expect("the changed derivation is reported");
    assert_eq!(entry.stored_hash, entry.definition_hash);
}

// ── AC6: resumable backfill ────────────────────────────────────────────────

/// AC6: a backfill stopped mid-sweep keeps its checkpoint, and resuming
/// finishes the job without repairing anything twice.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac6_a_killed_backfill_resumes_from_its_checkpoint() {
    let (_guard, _pg, pool) = setup().await;
    let mut conn = pool.get().await.expect("conn");

    // Five parents, each with one published comment nobody counted. This is the
    // shape of a table adopting a derivation it did not have before.
    let mut posts = Vec::new();
    for i in 0..5 {
        let post = seed_post(&mut conn, &format!("p{i}")).await;
        diesel::sql_query(
            "INSERT INTO dv_comments (post_id, published, score) VALUES ($1, TRUE, 1)",
        )
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("legacy comment");
        posts.push(post);
    }

    // Everything is current except the count, which is freshly enqueued.
    mark_all_complete(&mut conn).await;
    diesel::sql_query(
        "UPDATE _autumn_derivations SET backfill_state = 'pending', checkpoint = NULL, \
         backfilled_rows = 0 WHERE name = $1",
    )
    .bind::<Text, _>(COUNT_DERIVATION)
    .execute(&mut conn)
    .await
    .expect("enqueue the count");
    assert_eq!(
        drift(&mut conn, def(COUNT_DERIVATION))
            .await
            .expect("drift"),
        5,
        "every parent starts drifted"
    );

    // One batch of two, then stop: the kill.
    let first = run_backfill(
        &mut conn,
        &BackfillOptions {
            batch_size: 2,
            max_batches: Some(1),
        },
    )
    .await
    .expect("first backfill pass");
    assert_eq!(first.rows_repaired, 2);
    assert_eq!(first.in_progress, vec![COUNT_DERIVATION.to_owned()]);
    assert!(first.completed.is_empty());

    let stopped = state_of(&mut conn, COUNT_DERIVATION).await;
    assert_eq!(stopped.backfill_state, "running");
    assert_eq!(
        stopped.checkpoint,
        Some(posts[1]),
        "the checkpoint names the last repaired parent"
    );
    assert_eq!(stopped.backfilled_rows, 2);

    // The same three facts through the reported surface, which is what an
    // operator watching a rolling deploy actually reads.
    let mid = derivation_status(&mut conn).await.expect("status mid-sweep");
    let reported = mid
        .iter()
        .find(|entry| entry.name == COUNT_DERIVATION)
        .expect("the stopped derivation is reported");
    assert_eq!(reported.backfill_state, Some(BackfillState::Running));
    assert_eq!(
        reported.checkpoint,
        Some(posts[1]),
        "the checkpoint names the last repaired parent"
    );
    assert_eq!(reported.backfilled_rows, 2);
    assert_eq!(
        reported.drift,
        Some(3),
        "the three parents past the checkpoint still disagree"
    );

    assert_eq!(
        derived(&mut conn, "dv_posts", "published_comment_count", posts[0]).await,
        1
    );
    assert_eq!(
        derived(&mut conn, "dv_posts", "published_comment_count", posts[2]).await,
        0,
        "a parent past the checkpoint is untouched"
    );

    // Resume: the remaining three, then completion.
    let second = run_backfill(
        &mut conn,
        &BackfillOptions {
            batch_size: 2,
            max_batches: None,
        },
    )
    .await
    .expect("resumed backfill");
    assert_eq!(
        second.rows_repaired, 3,
        "resuming must not repair the first batch again"
    );
    assert_eq!(second.completed, vec![COUNT_DERIVATION.to_owned()]);

    let done = state_of(&mut conn, COUNT_DERIVATION).await;
    assert_eq!(done.backfill_state, "complete");
    assert_eq!(
        done.backfilled_rows, 5,
        "five parents, visited once each, with no double counting across the resume"
    );
    assert_eq!(
        done.checkpoint,
        Some(posts[4]),
        "the checkpoint stays populated after completion: it is the last \
         position the sweep applied, not an in-flight cursor"
    );
    for post in &posts {
        assert_eq!(
            derived(&mut conn, "dv_posts", "published_comment_count", *post).await,
            1
        );
    }
    assert_eq!(
        drift(&mut conn, def(COUNT_DERIVATION))
            .await
            .expect("drift"),
        0
    );

    // A completed derivation is not swept again.
    let third = run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("third pass");
    assert_eq!(third.rows_repaired, 0);
    assert!(third.completed.is_empty());
}

// ── AC7: status and repair ─────────────────────────────────────────────────

/// AC7: the status surface reports hash, state, checkpoint and drift, and a
/// recompute takes the drift to 0.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ac7_status_reports_state_and_recompute_clears_the_drift() {
    let (_guard, _pg, pool) = setup().await;
    let mut conn = pool.get().await.expect("conn");
    mark_all_complete(&mut conn).await;

    let post = seed_post(&mut conn, "drifted").await;
    diesel::sql_query("INSERT INTO dv_comments (post_id, published, score) VALUES ($1, TRUE, 7)")
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("legacy comment");
    diesel::sql_query("UPDATE dv_posts SET published_comment_count = 99 WHERE id = $1")
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("inflate the count");

    let drifted = derivation_status(&mut conn).await.expect("status");
    let count = drifted
        .iter()
        .find(|entry| entry.name == COUNT_DERIVATION)
        .expect("the count is reported");
    assert_eq!(count.stored_hash, count.definition_hash);
    assert_eq!(count.backfill_state, Some(BackfillState::Complete));
    assert_eq!(count.checkpoint, None);
    assert!(
        count.updated_at.is_some(),
        "the row records when it changed"
    );
    assert_eq!(
        count.drift,
        Some(1),
        "one parent disagrees with the source of truth"
    );
    assert_eq!(count.drift_error, None, "the scan ran");
    let sum = drifted
        .iter()
        .find(|entry| entry.name == SUM_DERIVATION)
        .expect("the sum is reported");
    assert_eq!(
        sum.drift,
        Some(1),
        "the sum never saw the legacy comment either"
    );

    assert_eq!(
        recompute(&mut conn, COUNT_DERIVATION)
            .await
            .expect("recompute"),
        1,
        "one parent is repaired"
    );
    assert_eq!(
        recompute(&mut conn, SUM_DERIVATION)
            .await
            .expect("recompute"),
        1
    );
    assert_eq!(
        derived(&mut conn, "dv_posts", "published_comment_count", post).await,
        1
    );
    assert_eq!(
        derived(&mut conn, "dv_posts", "visible_score", post).await,
        7
    );

    for entry in derivation_status(&mut conn).await.expect("status") {
        assert_eq!(
            entry.drift,
            Some(0),
            "no derivation may drift after a recompute: {entry:?}"
        );
    }

    // Idempotent: a healthy derivation is repaired zero times and written not
    // at all.
    assert_eq!(
        recompute(&mut conn, COUNT_DERIVATION)
            .await
            .expect("recompute again"),
        0
    );

    // An unknown name is an error, not a silent no-op.
    assert!(recompute(&mut conn, "nope.nope").await.is_err());
}

// ── AC8: reading a derivation is one query ─────────────────────────────────

/// AC8: a derived value is a plain column, so listing N parents with their
/// derived columns costs exactly one query — for N = 1 and N = 40 alike.
#[cfg(feature = "test-support")]
mod query_count {
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    // `diesel::QueryDsl` only — `diesel::prelude::*` would also bring the
    // *synchronous* `RunQueryDsl` into scope and make `load` ambiguous.
    use diesel::QueryDsl as _;
    use diesel_async::RunQueryDsl as _;

    use super::{
        DB_LOCK, create_schema, dv_posts, reseed_posts_through_the_repository, start_postgres,
    };

    /// One `SELECT` over the parent table, derived columns included — no join
    /// to the child table and no per-row query.
    #[get("/dv-posts")]
    async fn list_dv_posts(mut db: Db) -> AutumnResult<Json<Vec<(i64, i64, i64)>>> {
        let rows: Vec<(i64, i64, i64)> = dv_posts::table
            .select((
                dv_posts::id,
                dv_posts::published_comment_count,
                dv_posts::visible_score,
            ))
            .order(dv_posts::id)
            .load(&mut *db)
            .await?;
        Ok(Json(rows))
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn ac8_listing_parents_with_derived_columns_is_one_query() {
        let _guard = DB_LOCK.lock().await;
        let (_pg, pool) = start_postgres().await;
        create_schema(&mut pool.get().await.expect("conn")).await;

        let client = TestApp::new()
            .routes(routes![list_dv_posts])
            .with_db(pool.clone())
            .build();

        for total in [1usize, 40usize] {
            reseed_posts_through_the_repository(&pool, total).await;

            let resp = client.get("/dv-posts").send().await;
            resp.assert_ok();
            // The values, not just the row count: they were maintained by the
            // repository saves above, so a regression in the delta paths fails
            // here rather than passing as "one query returned nonsense".
            let expected: Vec<(i64, i64, i64)> = (1..=i64::try_from(total).expect("total fits"))
                .map(|id| (id, 1, 5))
                .collect();
            assert_eq!(
                resp.json::<Vec<(i64, i64, i64)>>(),
                expected,
                "one published comment of score 5 per parent, and the draft \
                 counts for neither column"
            );
            // Counted against the fixture table, not the whole request: a
            // recycled pooled connection is validated with a `SELECT $1` ping
            // on checkout, which is pool housekeeping rather than a read of the
            // derivation.
            let reads = resp
                .queries()
                .iter()
                .filter(|query| query.sql.contains("dv_posts"))
                .count();
            assert_eq!(
                reads,
                1,
                "a maintained derivation is read as a column, so the query count \
                 must not grow with the number of parents ({total}): {:?}",
                resp.queries()
            );
            resp.assert_no_n_plus_one();
        }
    }
}
