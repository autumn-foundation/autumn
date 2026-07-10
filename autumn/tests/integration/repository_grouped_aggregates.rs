//! Database-level integration tests for typed grouped aggregate queries
//! (`count_grouped_by_*`, `sum_*_grouped_by_*`, `avg_*`, `min_*`, `max_*`;
//! issue #1364).
//!
//! The seeded-table tests **require Docker** (testcontainers) and are
//! `#[ignore]`d by default. One non-ignored test proves the generated method
//! surface exists (and the codegen ran) without a live database.
//!
//! Fixtures deliberately span the generated branches — plain, tenant-scoped,
//! soft-delete, and a timestamp column for date-bucketed time series — so
//! `cargo test -p autumn-web --no-run --features db` type-checks every branch
//! even though the ignored tests can't run here.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::aggregate::DateBucket;
use autumn_web::repository::ReadRoute;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Plain model (numeric + group + timestamp columns) ─────────────────────────

diesel::table! {
    test_agg_events (id) {
        id -> Int8,
        post_id -> Int8,
        score -> Int4,
        created_at -> Timestamp,
    }
}

#[autumn_web::model(table = "test_agg_events")]
pub struct AggEvent {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub score: i32,
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::repository(AggEvent, table = "test_agg_events")]
pub trait AggEventRepository {
    /// COUNT(*) GROUP BY post_id.
    fn count_grouped_by_post_id() -> Vec<(i64, i64)>;
    /// SUM(score) GROUP BY post_id (i32 column summed into i64).
    fn sum_score_grouped_by_post_id() -> Vec<(i64, Option<i64>)>;
    /// AVG(score) GROUP BY post_id → double precision.
    fn avg_score_grouped_by_post_id() -> Vec<(i64, Option<f64>)>;
    /// MIN(score) GROUP BY post_id.
    fn min_score_grouped_by_post_id() -> Vec<(i64, Option<i32>)>;
    /// MAX(score) GROUP BY post_id.
    fn max_score_grouped_by_post_id() -> Vec<(i64, Option<i32>)>;
    /// COUNT(*) GROUP BY created_at — used for `DateBucket` time series.
    fn count_grouped_by_created_at() -> Vec<(chrono::NaiveDateTime, i64)>;
}

// ── Tenant-scoped model ───────────────────────────────────────────────────────

diesel::table! {
    test_agg_tenant_events (id) {
        id -> Int8,
        kind -> Text,
        amount -> Int8,
        tenant_id -> Text,
    }
}

#[autumn_web::model(table = "test_agg_tenant_events")]
pub struct AggTenantEvent {
    #[id]
    pub id: i64,
    pub kind: String,
    pub amount: i64,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(AggTenantEvent, table = "test_agg_tenant_events", tenant_scoped)]
pub trait AggTenantEventRepository {
    /// SUM(amount) GROUP BY kind, scoped to the active tenant.
    fn sum_amount_grouped_by_kind() -> Vec<(String, Option<i64>)>;
    /// COUNT(*) GROUP BY kind, scoped to the active tenant.
    fn count_grouped_by_kind() -> Vec<(String, i64)>;
}

// ── Soft-delete model ─────────────────────────────────────────────────────────

diesel::table! {
    test_agg_soft_events (id) {
        id -> Int8,
        bucket -> Text,
        value -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "test_agg_soft_events")]
pub struct AggSoftEvent {
    #[id]
    pub id: i64,
    pub bucket: String,
    pub value: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(AggSoftEvent, table = "test_agg_soft_events", soft_delete)]
pub trait AggSoftEventRepository {
    /// SUM(value) GROUP BY bucket, excluding soft-deleted rows.
    fn sum_value_grouped_by_bucket() -> Vec<(String, Option<i64>)>;
}

// ── Nullable-value model (AC7 all-NULL group → None) ──────────────────────────
//
// The group key (`post_id`) stays NOT NULL — nullable group keys are
// unsupported — but the aggregated `score` column is genuinely NULLABLE, so a
// group with only NULL scores exercises the null-safe `→ None` path.

diesel::table! {
    test_agg_nullable_events (id) {
        id -> Int8,
        post_id -> Int8,
        score -> Nullable<Int4>,
    }
}

#[autumn_web::model(table = "test_agg_nullable_events")]
pub struct AggNullableEvent {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub score: Option<i32>,
}

#[autumn_web::repository(AggNullableEvent, table = "test_agg_nullable_events")]
pub trait AggNullableEventRepository {
    /// SUM(score) GROUP BY post_id over a NULLABLE column.
    fn sum_score_grouped_by_post_id() -> Vec<(i64, Option<i64>)>;
    /// MIN(score) GROUP BY post_id over a NULLABLE column.
    fn min_score_grouped_by_post_id() -> Vec<(i64, Option<i32>)>;
    /// MAX(score) GROUP BY post_id over a NULLABLE column.
    fn max_score_grouped_by_post_id() -> Vec<(i64, Option<i32>)>;
    /// AVG(score) GROUP BY post_id over a NULLABLE column.
    fn avg_score_grouped_by_post_id() -> Vec<(i64, Option<f64>)>;
}

// ── Setup & helpers ───────────────────────────────────────────────────────────

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
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_agg_events \
         (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL, score INT NOT NULL, \
          created_at TIMESTAMP NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_agg_events");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_agg_tenant_events \
         (id BIGSERIAL PRIMARY KEY, kind TEXT NOT NULL, amount BIGINT NOT NULL, \
          tenant_id TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_agg_tenant_events");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_agg_soft_events \
         (id BIGSERIAL PRIMARY KEY, bucket TEXT NOT NULL, value BIGINT NOT NULL, \
          deleted_at TIMESTAMP)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_agg_soft_events");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_agg_nullable_events \
         (id BIGSERIAL PRIMARY KEY, post_id BIGINT NOT NULL, score INT)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_agg_nullable_events");

    (pool, container)
}

fn build_repo(pool: Pool<AsyncPgConnection>) -> PgAggEventRepository {
    PgAggEventRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

fn build_tenant_repo(pool: Pool<AsyncPgConnection>) -> PgAggTenantEventRepository {
    PgAggTenantEventRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

fn build_soft_repo(pool: Pool<AsyncPgConnection>) -> PgAggSoftEventRepository {
    PgAggSoftEventRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

fn build_nullable_repo(pool: Pool<AsyncPgConnection>) -> PgAggNullableEventRepository {
    PgAggNullableEventRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

fn ts(secs: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp(secs, 0)
        .expect("valid timestamp")
        .naive_utc()
}

// ── Non-ignored: the generated surface exists without a live database ──────────

/// The generated inherent methods type-check and can be named as function
/// items (proving the codegen ran) even where Docker is unavailable. This is
/// the primary verification available in the sandbox.
#[test]
fn grouped_aggregate_methods_are_generated() {
    fn assert_is_fn<F>(_f: F) {}
    assert_is_fn(PgAggEventRepository::count_grouped_by_post_id);
    assert_is_fn(PgAggEventRepository::sum_score_grouped_by_post_id);
    assert_is_fn(PgAggEventRepository::avg_score_grouped_by_post_id);
    assert_is_fn(PgAggEventRepository::min_score_grouped_by_post_id);
    assert_is_fn(PgAggEventRepository::max_score_grouped_by_post_id);
    assert_is_fn(PgAggEventRepository::count_grouped_by_created_at);
    // Tenant-scoped branch (tenant predicate composed like `count`).
    assert_is_fn(PgAggTenantEventRepository::sum_amount_grouped_by_kind);
    assert_is_fn(PgAggTenantEventRepository::count_grouped_by_kind);
    // Soft-delete branch (`deleted_at IS NULL` predicate).
    assert_is_fn(PgAggSoftEventRepository::sum_value_grouped_by_bucket);
    // Nullable-value branch (all-NULL group → None).
    assert_is_fn(PgAggNullableEventRepository::sum_score_grouped_by_post_id);
    assert_is_fn(PgAggNullableEventRepository::min_score_grouped_by_post_id);
    assert_is_fn(PgAggNullableEventRepository::max_score_grouped_by_post_id);
    assert_is_fn(PgAggNullableEventRepository::avg_score_grouped_by_post_id);
}

// ── Tests (require Docker) ────────────────────────────────────────────────────

async fn seed_events(repo: &PgAggEventRepository, rows: &[(i64, i32, i64)]) {
    let new: Vec<NewAggEvent> = rows
        .iter()
        .map(|&(post_id, score, at)| NewAggEvent {
            post_id,
            score,
            created_at: ts(at),
        })
        .collect();
    repo.save_many(&new).await.expect("seed events");
}

/// AC1: a plain grouped `COUNT(*)`/`SUM` returns one `(key, value)` pair per
/// group with the correct aggregate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_count_and_sum_basic() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);
    seed_events(
        &repo,
        &[(1, 10, 0), (1, 5, 1), (2, 7, 2), (2, 3, 3), (2, 100, 4)],
    )
    .await;

    let mut counts = repo.count_grouped_by_post_id().load().await.expect("count");
    counts.sort_by_key(|(k, _)| *k);
    assert_eq!(counts, vec![(1, 2), (2, 3)]);

    let mut sums = repo
        .sum_score_grouped_by_post_id()
        .load()
        .await
        .expect("sum");
    sums.sort_by_key(|(k, _)| *k);
    assert_eq!(sums, vec![(1, Some(15)), (2, Some(110))]);
}

/// AC2: `avg`/`min`/`max` roll-ups return the expected typed values.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_avg_min_max() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);
    seed_events(&repo, &[(1, 10, 0), (1, 20, 1), (1, 30, 2)]).await;

    let avg = repo
        .avg_score_grouped_by_post_id()
        .load()
        .await
        .expect("avg");
    assert_eq!(avg, vec![(1, Some(20.0))]);

    let min = repo
        .min_score_grouped_by_post_id()
        .load()
        .await
        .expect("min");
    assert_eq!(min, vec![(1, Some(10))]);

    let max = repo
        .max_score_grouped_by_post_id()
        .load()
        .await
        .expect("max");
    assert_eq!(max, vec![(1, Some(30))]);
}

/// AC3: `order_by_aggregate_desc().limit(n)` yields the top-N groups by the
/// aggregated value, highest first.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_top_n_order_and_limit() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);
    seed_events(
        &repo,
        &[(1, 1, 0), (2, 50, 1), (2, 50, 2), (3, 10, 3), (4, 5, 4)],
    )
    .await;

    let top2 = repo
        .sum_score_grouped_by_post_id()
        .order_by_aggregate_desc()
        .limit(2)
        .load()
        .await
        .expect("top-2");

    // post 2 = 100, post 3 = 10 are the two largest.
    assert_eq!(top2, vec![(2, Some(100)), (3, Some(10))]);
}

/// AC4: a pre-group range filter scopes the rows before aggregation, and the
/// value is bound as a parameter (no interpolation).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_range_filter_before_grouping() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);
    seed_events(
        &repo,
        &[(1, 10, 0), (2, 10, 0), (3, 10, 0), (4, 10, 0), (5, 10, 0)],
    )
    .await;

    // Only post_id in [2, 4] should survive → three groups.
    let mut got = repo
        .count_grouped_by_post_id()
        .filter_range(2, 4)
        .load()
        .await
        .expect("ranged count");
    got.sort_by_key(|(k, _)| *k);
    assert_eq!(got, vec![(2, 1), (3, 1), (4, 1)]);

    // Equality filter narrows to a single group.
    let eqd = repo
        .count_grouped_by_post_id()
        .filter_eq(3)
        .load()
        .await
        .expect("eq count");
    assert_eq!(eqd, vec![(3, 1)]);
}

/// AC5: `DateBucket` groups a timestamp column into day/month buckets keyed by
/// the bucket-start timestamp, and a date range scopes the window first.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_date_bucket_time_series() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // Two events on 1970-01-01 and one on 1970-01-02 (86_400s = one day).
    seed_events(&repo, &[(1, 1, 10), (1, 1, 20), (1, 1, 86_400 + 5)]).await;

    let mut per_day = repo
        .count_grouped_by_created_at()
        .bucket(DateBucket::Day)
        .filter_range(ts(0), ts(7 * 86_400))
        .load()
        .await
        .expect("per-day series");
    per_day.sort_by_key(|(k, _)| *k);

    assert_eq!(per_day, vec![(ts(0), 2), (ts(86_400), 1)]);
}

/// AC6: the aggregate acquires its connection through the repository's read
/// route (`__autumn_acquire_read_conn`), so a replica-routed repo reads from
/// the replica — exercised here on the primary route for a smoke check.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_aggregate_routes_through_read_role() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);
    seed_events(&repo, &[(1, 1, 0)]).await;

    // Primary route resolves and the aggregate succeeds.
    assert!(matches!(repo.__autumn_read_route(), ReadRoute::Primary));
    let got = repo.count_grouped_by_post_id().load().await.expect("count");
    assert_eq!(got, vec![(1, 1)]);
}

/// AC7 (empty half): an empty table yields an empty `Vec`. The null-safe
/// `→ None` half is covered by `grouped_all_null_group_yields_none` below (which
/// needs a genuinely NULLABLE value column).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_empty_and_null_safe() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // Empty table → empty result.
    let empty = repo
        .sum_score_grouped_by_post_id()
        .load()
        .await
        .expect("empty ok");
    assert!(empty.is_empty(), "empty table must yield an empty Vec");

    let empty_count = repo.count_grouped_by_post_id().load().await.expect("empty");
    assert!(empty_count.is_empty());
}

/// AC7 (null-safe half): a group whose numeric column is entirely NULL yields
/// `(key, None)` for `sum`/`min`/`max`/`avg` — never `(key, Some(0))`. Uses a
/// genuinely NULLABLE `score` column (the other fixtures are all NOT NULL), so
/// the group key is present but every aggregated value is NULL.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_all_null_group_yields_none() {
    let (pool, _container) = setup_pool().await;
    let repo = build_nullable_repo(pool);

    // One group (post_id = 1) with two rows, both with a NULL score.
    repo.save_many(&[
        NewAggNullableEvent {
            post_id: 1,
            score: None,
        },
        NewAggNullableEvent {
            post_id: 1,
            score: None,
        },
    ])
    .await
    .expect("seed all-NULL group");

    let sum = repo
        .sum_score_grouped_by_post_id()
        .load()
        .await
        .expect("sum");
    assert_eq!(sum, vec![(1, None)], "SUM over an all-NULL group is None");

    let min = repo
        .min_score_grouped_by_post_id()
        .load()
        .await
        .expect("min");
    assert_eq!(min, vec![(1, None)], "MIN over an all-NULL group is None");

    let max = repo
        .max_score_grouped_by_post_id()
        .load()
        .await
        .expect("max");
    assert_eq!(max, vec![(1, None)], "MAX over an all-NULL group is None");

    let avg = repo
        .avg_score_grouped_by_post_id()
        .load()
        .await
        .expect("avg");
    assert_eq!(avg, vec![(1, None)], "AVG over an all-NULL group is None");
}

/// Tenant scoping: the aggregate only sees the routed tenant's rows, matching
/// `count`'s tenant predicate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_aggregate_is_tenant_scoped() {
    use autumn_web::tenancy::with_tenant;

    let (pool, _container) = setup_pool().await;
    let repo = build_tenant_repo(pool);

    with_tenant("tenant-a".to_string(), async {
        repo.save_many(&[
            NewAggTenantEvent {
                kind: "click".to_string(),
                amount: 3,
            },
            NewAggTenantEvent {
                kind: "click".to_string(),
                amount: 4,
            },
        ])
        .await
        .expect("seed tenant-a");
    })
    .await;
    with_tenant("tenant-b".to_string(), async {
        repo.save_many(&[NewAggTenantEvent {
            kind: "click".to_string(),
            amount: 100,
        }])
        .await
        .expect("seed tenant-b");
    })
    .await;

    let sums = with_tenant("tenant-a".to_string(), async {
        repo.sum_amount_grouped_by_kind()
            .load()
            .await
            .expect("tenant-a sum")
    })
    .await;

    // Only tenant-a's rows (3 + 4 = 7); tenant-b's 100 must not leak.
    assert_eq!(sums, vec![("click".to_string(), Some(7))]);
}

/// Soft-delete: soft-deleted rows are excluded from the aggregate, matching
/// the other finders' `deleted_at IS NULL` filter.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn grouped_aggregate_excludes_soft_deleted() {
    let (pool, _container) = setup_pool().await;
    let repo = build_soft_repo(pool);

    let inserted = repo
        .save_many(&[
            NewAggSoftEvent {
                bucket: "a".to_string(),
                value: 10,
            },
            NewAggSoftEvent {
                bucket: "a".to_string(),
                value: 20,
            },
            NewAggSoftEvent {
                bucket: "b".to_string(),
                value: 5,
            },
        ])
        .await
        .expect("seed soft rows");

    // Soft-delete one of bucket "a"'s rows.
    repo.delete_by_id(inserted[0].id)
        .await
        .expect("soft delete");

    let mut sums = repo
        .sum_value_grouped_by_bucket()
        .load()
        .await
        .expect("sum excluding trashed");
    sums.sort_by(|a, b| a.0.cmp(&b.0));

    // bucket "a" now only counts the surviving 20; bucket "b" is unchanged.
    assert_eq!(
        sums,
        vec![("a".to_string(), Some(20)), ("b".to_string(), Some(5))]
    );
}
