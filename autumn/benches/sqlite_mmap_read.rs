//! Criterion benches for `SQLite` memory-mapped reads: `mmap_size` off and on.
//!
//! The bench builds one file database with an index on `k`, and opens two
//! pools on it through `create_pool`, the path an app takes. One pool has
//! `mmap_size = 0` (the default). The other has `mmap_size = 1 GiB`. Each
//! iteration runs one query on one pooled connection:
//!
//! * `point_256`: 256 random index lookups on `k`, with their rows.
//! * `range_10pct`: one index range over a tenth of the rows.
//!
//! The rows are 1 KiB each. The default file is about 86 MiB, much larger
//! than `SQLite`'s default page cache (about 2 MiB). A warm-up scan loads the
//! file into the OS page cache first, so the bench measures the read path,
//! not the disk.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p autumn-web --features sqlite --bench sqlite_mmap_read
//! # A larger file (rows of 1 KiB):
//! AUTUMN_BENCH_SQLITE_ROWS=400000 cargo bench -p autumn-web --features sqlite \
//!     --bench sqlite_mmap_read
//! ```

use std::hint::black_box;
use std::time::Duration;

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use criterion::{Criterion, criterion_group, criterion_main};
use diesel::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

/// Rows in the bench database when `AUTUMN_BENCH_SQLITE_ROWS` is not set.
const DEFAULT_ROWS: u64 = 65_536;
/// Lookups in one `point_256` query.
const POINT_KEYS: usize = 256;
/// `mmap_size` of the "on" pool.
const MMAP_ON: u64 = 1 << 30;

#[derive(QueryableByName)]
struct Sum {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn rows() -> u64 {
    std::env::var("AUTUMN_BENCH_SQLITE_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|rows| *rows > 0)
        .unwrap_or(DEFAULT_ROWS)
}

/// Build the database: `rows` rows of 1 KiB, `k` a permutation of the row
/// ids, so the index order is not the table order.
fn seed(path: &std::path::Path, rows: u64) {
    use diesel::Connection as _;
    use diesel::connection::SimpleConnection as _;

    let mut conn = diesel::SqliteConnection::establish(&path.display().to_string())
        .expect("open the bench database");
    conn.batch_execute(&format!(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE items (id INTEGER PRIMARY KEY, k INTEGER NOT NULL, payload BLOB NOT NULL);
         WITH RECURSIVE s(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM s WHERE i < {last})
         INSERT INTO items (k, payload) SELECT (i * 2654435761) % {rows}, zeroblob(1024) FROM s;
         CREATE INDEX items_k ON items (k);
         PRAGMA wal_checkpoint(TRUNCATE);",
        last = rows - 1,
    ))
    .expect("seed the bench database");
}

fn pool(url: &str, mmap_size: u64) -> Pool<RuntimeConnection> {
    create_pool(&DatabaseConfig {
        url: Some(url.to_owned()),
        primary_pool_size: Some(1),
        mmap_size,
        ..Default::default()
    })
    .expect("build the pool")
    .expect("a primary pool")
}

/// Run one query on one pooled connection. Return the sum.
async fn read(pool: &Pool<RuntimeConnection>, sql: &str, keys: &str) -> i64 {
    let mut conn = pool.get().await.expect("a pooled connection");
    diesel::sql_query(sql)
        .bind::<Text, _>(keys)
        .get_result::<Sum>(&mut *conn)
        .await
        .expect("the bench query")
        .n
}

/// A JSON array of `POINT_KEYS` keys from a small LCG, new on each call.
fn point_keys(state: &mut u64, rows: u64) -> String {
    let mut keys = Vec::with_capacity(POINT_KEYS);
    for _ in 0..POINT_KEYS {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        keys.push((*state >> 33) % rows);
    }
    serde_json::to_string(&keys).expect("keys are JSON")
}

fn bench(c: &mut Criterion) {
    let rows = rows();
    let tmp = tempfile::tempdir().expect("a temporary directory");
    let path = tmp.path().join("mmap_read.db");
    seed(&path, rows);
    let url = format!("sqlite://{}", path.display());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let pools = [("off", pool(&url, 0)), ("on", pool(&url, MMAP_ON))];

    const POINT: &str = "SELECT COALESCE(SUM(LENGTH(payload)), 0) AS n FROM items \
                         WHERE k IN (SELECT value FROM json_each(?))";
    // `?1` holds `[low, high]` as JSON, so both queries bind one text value.
    const RANGE: &str = "SELECT COALESCE(SUM(LENGTH(payload)), 0) AS n FROM items \
                         WHERE k BETWEEN json_extract(?1, '$[0]') AND json_extract(?1, '$[1]')";
    let width = (rows / 10).max(1);

    // Warm-up: load the file into the OS page cache.
    for (_, pool) in &pools {
        let all = format!("[0, {rows}]");
        assert_eq!(
            runtime.block_on(read(pool, RANGE, &all)),
            i64::try_from(rows * 1024).expect("fits in i64")
        );
    }

    let mut group = c.benchmark_group("sqlite_mmap/point_256");
    group.measurement_time(Duration::from_secs(5));
    for (name, pool) in &pools {
        let mut state = 0x5eed_u64;
        group.bench_function(*name, |b| {
            b.iter(|| {
                let keys = point_keys(&mut state, rows);
                black_box(runtime.block_on(read(pool, POINT, &keys)))
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("sqlite_mmap/range_10pct");
    group.measurement_time(Duration::from_secs(5));
    for (name, pool) in &pools {
        let mut state = 0x5eed_u64;
        group.bench_function(*name, |b| {
            b.iter(|| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let low = (state >> 33) % (rows - width + 1);
                let bounds = format!("[{low}, {}]", low + width - 1);
                black_box(runtime.block_on(read(pool, RANGE, &bounds)))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
