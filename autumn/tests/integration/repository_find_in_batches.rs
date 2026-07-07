//! Database-level integration tests for bounded-memory batched iteration
//! (`find_in_batches` / `find_each`, issue #1395).
//!
//! The seeded-table tests **require Docker** (testcontainers) and are
//! `#[ignore]`d by default. The read-routing test runs without a live
//! database (two lazily-created pools + a `FailReadiness` policy), mirroring
//! `repository_replica_routing.rs`.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::AppState;
use autumn_web::config::{DatabaseConfig, ReplicaFallback};
use autumn_web::db;
use autumn_web::reexports::axum::extract::FromRequestParts;
use autumn_web::reexports::http::Request;
use autumn_web::repository::ReadRoute;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Plain model ───────────────────────────────────────────────────────────────

diesel::table! {
    test_batch_records (id) {
        id -> Int8,
        name -> Text,
    }
}

#[autumn_web::model(table = "test_batch_records")]
#[derive(PartialEq, Eq)]
pub struct BatchRecord {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(BatchRecord, table = "test_batch_records")]
pub trait BatchRecordRepository {}

// ── Soft-delete model ─────────────────────────────────────────────────────────

diesel::table! {
    test_batch_soft_records (id) {
        id -> Int8,
        name -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "test_batch_soft_records")]
pub struct BatchSoftRecord {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(BatchSoftRecord, table = "test_batch_soft_records", soft_delete)]
pub trait BatchSoftRecordRepository {}

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
        "CREATE TABLE IF NOT EXISTS test_batch_records (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_batch_records");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_batch_soft_records (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, deleted_at TIMESTAMP)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_batch_soft_records");

    (pool, container)
}

const fn build_batch_repo(pool: Pool<AsyncPgConnection>) -> PgBatchRecordRepository {
    PgBatchRecordRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_soft_repo(pool: Pool<AsyncPgConnection>) -> PgBatchSoftRecordRepository {
    PgBatchSoftRecordRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

async fn seed_records(repo: &PgBatchRecordRepository, n: usize) -> Vec<i64> {
    let new: Vec<NewBatchRecord> = (0..n)
        .map(|i| NewBatchRecord {
            name: format!("row-{i}"),
        })
        .collect();
    let inserted = repo.save_many(&new).await.expect("seed rows");
    inserted.into_iter().map(|r| r.id).collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC1/AC2/AC3: every row visited exactly once, no dupes/gaps, and never more
/// than `batch_size` models resident at a time (each chunk is bounded and is
/// dropped before the next fetch).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_in_batches_visits_all_rows_no_dupes_no_gaps() {
    const N: usize = 250;
    const B: usize = 40;

    let (pool, _container) = setup_pool().await;
    let repo = build_batch_repo(pool);
    let mut expected: Vec<i64> = seed_records(&repo, N).await;
    expected.sort_unstable();

    let mut seen: Vec<i64> = Vec::new();
    let mut chunk_count = 0usize;
    let mut batches = repo.find_in_batches(B);
    while let Some(chunk) = batches.next_batch().await.expect("batch fetch") {
        // AC3 high-water-mark: a single chunk holds at most B models.
        assert!(
            chunk.len() <= B,
            "chunk exceeded batch_size: {}",
            chunk.len()
        );
        assert!(!chunk.is_empty(), "no empty chunk should ever be yielded");
        chunk_count += 1;
        for row in &chunk {
            seen.push(row.id);
        }
        // Chunk dropped here before the next fetch: O(batch_size) residency.
    }

    // 250 / 40 = 7 chunks (6 full + 1 short of 10).
    assert_eq!(chunk_count, N.div_ceil(B));
    seen.sort_unstable();
    assert_eq!(seen.len(), N, "visited exactly N rows");
    assert_eq!(seen, expected, "no dupes, no gaps — exact seeded id set");
}

/// AC1: `find_each` yields every model individually.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_each_visits_all_rows() {
    const N: usize = 250;
    const B: usize = 40;

    let (pool, _container) = setup_pool().await;
    let repo = build_batch_repo(pool);
    let mut expected = seed_records(&repo, N).await;
    expected.sort_unstable();

    let mut seen = Vec::new();
    let mut each = repo.find_each(B);
    while let Some(row) = each.next().await.expect("row fetch") {
        seen.push(row.id);
    }

    seen.sort_unstable();
    assert_eq!(seen.len(), N);
    assert_eq!(seen, expected);
}

/// AC: `batch_size > 100` is honored (no clamp to `MAX_PAGE_SIZE`).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_in_batches_batch_size_over_100() {
    const N: usize = 300;
    const B: usize = 150;

    let (pool, _container) = setup_pool().await;
    let repo = build_batch_repo(pool);
    seed_records(&repo, N).await;

    let mut chunk_sizes = Vec::new();
    let mut batches = repo.find_in_batches(B);
    while let Some(chunk) = batches.next_batch().await.expect("batch fetch") {
        chunk_sizes.push(chunk.len());
    }

    // 300 rows / 150 => two full chunks of 150, proving no clamp to 100.
    assert_eq!(chunk_sizes, vec![150, 150]);
}

/// AC4: soft-deleted rows are never visited, matching `find_all`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_in_batches_soft_delete_excludes_trashed() {
    let (pool, _container) = setup_pool().await;
    let repo = build_soft_repo(pool);

    // Seed 20 live + trash the even ids.
    let new: Vec<NewBatchSoftRecord> = (0..20)
        .map(|i| NewBatchSoftRecord {
            name: format!("s-{i}"),
        })
        .collect();
    let inserted = repo.save_many(&new).await.expect("seed soft rows");
    let mut trashed: Vec<i64> = Vec::new();
    for row in inserted.iter().filter(|r| r.id % 2 == 0) {
        repo.delete_by_id(row.id).await.expect("soft delete");
        trashed.push(row.id);
    }

    let mut seen = Vec::new();
    let mut batches = repo.find_in_batches(5);
    while let Some(chunk) = batches.next_batch().await.expect("batch fetch") {
        for row in &chunk {
            seen.push(row.id);
        }
    }

    for id in &trashed {
        assert!(!seen.contains(id), "trashed row {id} must not be visited");
    }
    // Should match find_all, which also filters soft-deleted rows.
    let mut via_find_all: Vec<i64> = repo
        .find_all()
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    via_find_all.sort_unstable();
    seen.sort_unstable();
    assert_eq!(
        seen, via_find_all,
        "batches match find_all soft-delete semantics"
    );
}

/// AC: empty table yields nothing immediately.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_in_batches_empty_table() {
    let (pool, _container) = setup_pool().await;
    let repo = build_batch_repo(pool);

    let mut batches = repo.find_in_batches(10);
    assert!(batches.next_batch().await.expect("empty fetch").is_none());
    // Stays ended.
    assert!(batches.next_batch().await.expect("empty fetch").is_none());

    let mut each = repo.find_each(10);
    assert!(each.next().await.expect("empty each").is_none());
}

/// AC6-adjacent: `batch_size == 0` returns an error rather than spinning.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_in_batches_batch_size_zero() {
    let (pool, _container) = setup_pool().await;
    let repo = build_batch_repo(pool);
    seed_records(&repo, 5).await;

    let mut batches = repo.find_in_batches(0);
    let err = batches
        .next_batch()
        .await
        .expect_err("batch_size 0 must error");
    assert!(
        err.to_string().to_lowercase().contains("batch_size"),
        "error should mention batch_size, got: {err}"
    );
    // Stays ended (no silent success) after the error.
    assert!(batches.next_batch().await.expect("stays ended").is_none());
}

/// AC6: an error mid-iteration surfaces on the failing batch, stops iteration,
/// and does not silently resume. We drop the table between batches to force a
/// DB error on the next fetch.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_in_batches_error_mid_iteration_stops() {
    const N: usize = 100;
    const B: usize = 10;

    let (pool, _container) = setup_pool().await;
    let repo = build_batch_repo(pool.clone());
    seed_records(&repo, N).await;

    let mut batches = repo.find_in_batches(B);
    // Pull the first batch successfully.
    let first = batches
        .next_batch()
        .await
        .expect("first batch ok")
        .expect("first batch present");
    assert_eq!(first.len(), B);

    // Break the table out from under the iterator.
    let mut conn = pool.get().await.unwrap();
    diesel::sql_query("DROP TABLE test_batch_records")
        .execute(&mut conn)
        .await
        .expect("drop table");

    // Next batch surfaces the DB error, not a silent Ok(None)/Ok(Some).
    let err = batches
        .next_batch()
        .await
        .expect_err("dropped table must surface an error");
    assert!(!err.to_string().is_empty());

    // Once errored it stays ended: no silent success on the next call.
    assert!(
        batches.next_batch().await.expect("stays ended").is_none(),
        "iterator must not resume after an error"
    );
}

// ── Read-routing (no live database required) ──────────────────────────────────

const PRIMARY_POOL_SIZE: usize = 5;
const REPLICA_POOL_SIZE: usize = 2;

fn make_pool(database: &str, pool_size: usize) -> Pool<AsyncPgConnection> {
    let config = DatabaseConfig {
        url: Some(format!("postgres://localhost/{database}")),
        pool_size,
        ..Default::default()
    };
    db::create_pool(&config)
        .expect("pool config is valid")
        .expect("url is set")
}

async fn extract<R>(state: &AppState) -> R
where
    R: FromRequestParts<AppState, Rejection = autumn_web::AutumnError>,
{
    let (mut parts, ()) = Request::builder().body(()).unwrap().into_parts();
    R::from_request_parts(&mut parts, state)
        .await
        .expect("repository extraction succeeds")
}

/// AC5: the batch iterator routes through the repository's read role. Under a
/// `FailReadiness` policy with an unready replica the read route is
/// `Unavailable`, so the first `next_batch()` fails fast with the same
/// "replica unavailable" error a finder would — before touching any pool,
/// proving the iterator honors read routing (no live database needed).
#[tokio::test]
async fn find_in_batches_honors_read_route_and_fails_fast_when_unavailable() {
    let state = AppState::for_test()
        .with_pool(make_pool("primary", PRIMARY_POOL_SIZE))
        .with_replica_pool(make_pool("replica", REPLICA_POOL_SIZE));
    state
        .probes()
        .configure_replica_dependency(ReplicaFallback::FailReadiness);
    state
        .probes()
        .mark_replica_unready("replica connection failed");

    let repo: PgBatchRecordRepository = extract(&state).await;
    assert!(matches!(repo.__autumn_read_route(), ReadRoute::Unavailable));

    let mut batches = repo.find_in_batches(50);
    let err = batches
        .next_batch()
        .await
        .expect_err("batched iteration must honor the unavailable read route");
    assert!(
        err.to_string().to_lowercase().contains("replica"),
        "batch iterator should route through the read role and report the \
         replica is unavailable, got: {err}"
    );

    // A replica-routed repo (healthy) snapshots the replica pool for its reads.
    let healthy = AppState::for_test()
        .with_pool(make_pool("primary", PRIMARY_POOL_SIZE))
        .with_replica_pool(make_pool("replica", REPLICA_POOL_SIZE));
    let repo: PgBatchRecordRepository = extract(&healthy).await;
    assert!(
        matches!(repo.__autumn_read_route(), ReadRoute::ReadPool(_)),
        "reads (including batched iteration) target the replica pool"
    );
}
