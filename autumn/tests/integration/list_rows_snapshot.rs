//! Issue #2185: `list_rows` / `list_scoped_rows`, the count-free read the
//! scaffolded CSV export uses.
//!
//! These tests prove three things:
//!
//! - The read is one statement with no `COUNT(*)`, also for a full-cap export.
//! - One statement is one snapshot: a concurrent write cannot duplicate or
//!   skip a row.
//! - The rows are the rows `list`/`list_scoped` show: same allowlist, same
//!   soft-delete filter, same owner scope.
//!
//! **Requires Docker**, or set `AUTUMN_TEST_PG_URL` to a Postgres that loads
//! `pg_stat_statements`. CI runs it in the Docker sweep (see CLAUDE.md).

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::pagination::{ListQuery, PageRequest, SortDir};
use diesel::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    snapshot_rows (id) {
        id -> Int8,
        owner_id -> Int8,
        status -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "snapshot_rows")]
pub struct SnapshotRow {
    #[id]
    pub id: i64,
    pub owner_id: i64,
    pub status: String,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(SnapshotRow, table = "snapshot_rows", soft_delete, owner = owner_id)]
pub trait SnapshotRowRepository {}

/// The cap the scaffolded export uses.
const MAX_EXPORT_ROWS: usize = 10_000;

static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Keeps the Postgres alive for the test.
enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    External,
}

async fn start_postgres() -> (PgHandle, String) {
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        return (PgHandle::External, url);
    }
    // Pinned like `export_csv_list_count_profile`; CI pre-pulls this tag.
    let container = Postgres::default()
        .with_tag("16-alpine")
        .with_cmd([
            "-c",
            "fsync=off",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
        ])
        .start()
        .await
        .expect("start Postgres container");
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Postgres port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    (PgHandle::Container(Box::new(container)), url)
}

async fn exec(pool: &Pool<AsyncPgConnection>, sql: &str) {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
}

/// Makes an empty `snapshot_rows` table and returns a pool and a repository.
async fn setup() -> (
    tokio::sync::MutexGuard<'static, ()>,
    PgHandle,
    Pool<AsyncPgConnection>,
    PgSnapshotRowRepository,
) {
    let guard = DB_LOCK.lock().await;
    let (handle, url) = start_postgres().await;
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager)
        .max_size(5)
        .build()
        .expect("build pool");
    exec(&pool, "CREATE EXTENSION IF NOT EXISTS pg_stat_statements").await;
    exec(&pool, "DROP TABLE IF EXISTS snapshot_rows").await;
    exec(
        &pool,
        "CREATE TABLE snapshot_rows (\
           id BIGSERIAL PRIMARY KEY, \
           owner_id BIGINT NOT NULL, \
           status TEXT NOT NULL, \
           deleted_at TIMESTAMP)",
    )
    .await;
    let repo = PgSnapshotRowRepository::with_pool_untracked(pool.clone());
    (guard, handle, pool, repo)
}

fn ids(rows: &[SnapshotRow]) -> Vec<i64> {
    rows.iter().map(|r| r.id).collect()
}

#[derive(QueryableByName)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
}

/// Returns `(count_calls, select_calls)` on `snapshot_rows` since the last
/// `pg_stat_statements_reset()`.
async fn statement_calls(pool: &Pool<AsyncPgConnection>) -> (i64, i64) {
    let mut conn = pool.get().await.expect("conn");
    let rows = diesel::sql_query(
        "SELECT query, calls FROM pg_stat_statements \
         WHERE query ILIKE '%snapshot_rows%' \
           AND query NOT ILIKE '%pg_stat_statements%'",
    )
    .load::<StatementRow>(&mut conn)
    .await
    .expect("read pg_stat_statements");
    let (mut counts, mut selects) = (0, 0);
    for row in rows {
        let query = row.query.to_uppercase();
        if query.contains("COUNT(") {
            counts += row.calls;
        } else if query.trim_start().starts_with("SELECT") {
            selects += row.calls;
        }
    }
    (counts, selects)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn list_rows_returns_the_rows_list_shows() {
    let (_guard, _pg, pool, repo) = setup().await;
    // Owners 1 and 2, statuses `a` and `b`, every fourth row soft-deleted.
    exec(
        &pool,
        "INSERT INTO snapshot_rows (owner_id, status, deleted_at) \
         SELECT 1 + i % 2, CASE WHEN i % 3 = 0 THEN 'a' ELSE 'b' END, \
                CASE WHEN i % 4 = 0 THEN TIMESTAMP '2026-01-01' END \
         FROM generate_series(1, 60) AS i",
    )
    .await;

    let queries = [
        ListQuery::default(),
        ListQuery::new(Some("status"), SortDir::Desc, &[]),
        ListQuery::new(None, SortDir::Asc, &[("status", "a")]),
        // Not a column: the allowlist ignores both keys.
        ListQuery::new(
            Some("id;DROP TABLE snapshot_rows"),
            SortDir::Asc,
            &[("x", "1")],
        ),
    ];
    let first_page = PageRequest::new(1, 100);
    for query in &queries {
        let page = repo.list(query, &first_page).await.expect("list");
        let rows = repo.list_rows(query, 1_000).await.expect("list_rows");
        assert_eq!(
            ids(&rows),
            ids(&page.content),
            "list_rows != list for {query:?}"
        );
        assert!(
            rows.iter().all(|r| r.deleted_at.is_none()),
            "soft-deleted row read"
        );

        let first_five = repo.list_rows(query, 5).await.expect("list_rows limit");
        assert_eq!(
            ids(&first_five),
            ids(&page.content)[..5],
            "limit must keep list order"
        );

        let scoped_page = repo
            .list_scoped(1, query, &first_page)
            .await
            .expect("list_scoped");
        let scoped = repo
            .list_scoped_rows(1, query, 1_000)
            .await
            .expect("list_scoped_rows");
        assert_eq!(
            ids(&scoped),
            ids(&scoped_page.content),
            "scoped rows != list_scoped"
        );
        assert!(
            scoped.iter().all(|r| r.owner_id == 1),
            "another owner's row read"
        );
    }

    // A request filter can narrow the owner's rows, never widen them.
    let other_owner = ListQuery::new(None, SortDir::Asc, &[("owner_id", "2")]);
    let scoped = repo
        .list_scoped_rows(1, &other_owner, 1_000)
        .await
        .expect("list_scoped_rows");
    assert!(
        scoped.is_empty(),
        "filter[owner_id] widened the owner scope"
    );

    assert!(
        repo.list_rows(&queries[0], 0)
            .await
            .expect("limit 0")
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn full_cap_export_is_one_statement_with_no_count() {
    let (_guard, _pg, pool, repo) = setup().await;
    // Exactly MAX_EXPORT_ROWS rows of `cap`, and 500 more of `over`.
    exec(
        &pool,
        &format!(
            "INSERT INTO snapshot_rows (owner_id, status) \
             SELECT 1, CASE WHEN i <= {MAX_EXPORT_ROWS} THEN 'cap' ELSE 'over' END \
             FROM generate_series(1, {}) AS i",
            MAX_EXPORT_ROWS + 500
        ),
    )
    .await;

    // Over the cap: the read gets one row past it, so truncation is visible.
    exec(&pool, "SELECT pg_stat_statements_reset()").await;
    let rows = repo
        .list_rows(&ListQuery::default(), MAX_EXPORT_ROWS + 1)
        .await
        .expect("list_rows");
    assert_eq!(
        rows.len(),
        MAX_EXPORT_ROWS + 1,
        "over-cap read must show the extra row"
    );
    assert_eq!(
        statement_calls(&pool).await,
        (0, 1),
        "a full-cap read must be one SELECT and no COUNT(*)"
    );

    // Exactly the cap: no row past it, so the export is not truncated.
    let exact = ListQuery::new(None, SortDir::Asc, &[("status", "cap")]);
    let rows = repo
        .list_rows(&exact, MAX_EXPORT_ROWS + 1)
        .await
        .expect("list_rows");
    assert_eq!(
        rows.len(),
        MAX_EXPORT_ROWS,
        "an exactly-cap set is not truncated"
    );

    let rows = repo
        .list_scoped_rows(1, &ListQuery::default(), MAX_EXPORT_ROWS + 1)
        .await
        .expect("list_scoped_rows");
    assert_eq!(rows.len(), MAX_EXPORT_ROWS + 1);
    assert_eq!(
        statement_calls(&pool).await,
        (0, 3),
        "list_scoped_rows must also be one SELECT and no COUNT(*)"
    );
}

// Multi-thread, so the writer runs in parallel with the reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_writes_cannot_duplicate_or_skip_a_row() {
    const LIVE: usize = 2_000;
    let (_guard, _pg, pool, repo) = setup().await;
    exec(
        &pool,
        &format!(
            "INSERT INTO snapshot_rows (owner_id, status) \
             SELECT 1, 'a' FROM generate_series(1, {LIVE}) AS i"
        ),
    )
    .await;

    // Each write deletes the oldest row and adds a new one in one transaction,
    // so every committed state has exactly LIVE rows. A paged read under this
    // load sees the new row shift the offsets, and reads a row twice.
    let stop = Arc::new(AtomicBool::new(false));
    let write_count = Arc::new(AtomicU64::new(0));
    let writer = {
        let pool = pool.clone();
        let stop = stop.clone();
        let write_count = write_count.clone();
        tokio::spawn(async move {
            // Qualified: the module's `RunQueryDsl` import also has a `load`.
            while !AtomicBool::load(&stop, Ordering::Relaxed) {
                exec(
                    &pool,
                    "WITH gone AS (\
                       DELETE FROM snapshot_rows \
                       WHERE id = (SELECT min(id) FROM snapshot_rows) RETURNING 1) \
                     INSERT INTO snapshot_rows (owner_id, status) \
                     SELECT 1, 'a' FROM gone",
                )
                .await;
                write_count.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    // Start to read only after the first write, so the writes overlap the reads.
    while AtomicU64::load(&write_count, Ordering::Relaxed) == 0 {
        tokio::task::yield_now().await;
    }
    let writes_before = AtomicU64::load(&write_count, Ordering::Relaxed);
    for _ in 0..50 {
        let rows = repo
            .list_rows(&ListQuery::default(), LIVE + 1)
            .await
            .expect("list_rows");
        let mut seen = ids(&rows);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), rows.len(), "a row was read twice");
        assert_eq!(rows.len(), LIVE, "a row was skipped or added");
    }

    let writes_during = AtomicU64::load(&write_count, Ordering::Relaxed) - writes_before;
    stop.store(true, Ordering::Relaxed);
    writer.await.expect("writer");
    assert!(
        writes_during > 0,
        "no write overlapped the reads, so the test proved nothing"
    );
}
