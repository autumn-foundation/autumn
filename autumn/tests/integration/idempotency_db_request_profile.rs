//! Ledger harness for the database idempotency store on its request path
//! (`autumn/src/idempotency/db_store.rs`, `backend = "database"`).
//!
//! It drives real `POST` requests with an `Idempotency-Key` header through
//! the idempotency middleware of a `TestApp`, against an
//! `autumn_idempotency_keys` table with a day of keys in it, and prints the
//! `pg_stat_statements` profile of each phase: statements, rows, shared
//! buffers and WAL bytes per request, plus the table's update and HOT update
//! counts and the WAL the phase wrote.
//!
//! Point `AUTUMN_LEDGER_PG_URL` at a Postgres 16 server that preloads
//! `pg_stat_statements`, or leave it unset and the harness starts a
//! testcontainer. `AUTUMN_LEDGER_IDEMPOTENCY_KEYS` sets the fixture size
//! (default 1,000,000 rows). Reproduce with:
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests -- --ignored \
//!   idempotency_db_request_profile --nocapture --test-threads=1
//! ```
//!
//! See `docs/perf/idempotency-db-request/README.md` for the recorded results.
//!
//! ## Fixture
//!
//! One day of traffic at the default 24 h TTL:
//!
//! - `AUTUMN_LEDGER_IDEMPOTENCY_KEYS` rows keyed `v2:<sha256 hex>`, as the
//!   middleware writes them. Their expiry is spread evenly over the next 24 h.
//! - 96% hold a stored response of 200 to 1,400 bytes. 2% hold a recovery
//!   point and no response. 0.5% are in flight: a live lock and no record.
//! - 1,500 rows are already expired and wait for the sweep.
//! - 3% of rows are updated before `ANALYZE` with no `VACUUM`, so dead
//!   tuples are on the pages.
//!
//! ## Workload
//!
//! Three phases, each measured on its own:
//!
//! 1. `fresh-tx`: 1,024 first-time `POST /pay` requests. The handler inserts a
//!    payment and writes its response with `IdempotencyTx::commit` in the
//!    same transaction.
//! 2. `fresh-plain`: 1,024 first-time `POST /plain` requests. The handler
//!    returns a response and the middleware stores it.
//! 3. `replay`: 512 retries of keys from phase 1, which replay the stored
//!    response.

#![cfg(feature = "db")]

use std::time::Duration;

use autumn_web::config::{AutumnConfig, IdempotencyBackend};
use autumn_web::idempotency::IdempotencyTx;
use autumn_web::prelude::*;
use autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
use autumn_web::test::{TestApp, TestClient};
use diesel::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _};
use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const IDEMPOTENCY_UP: &str =
    include_str!("../../migrations/20261005200000_create_idempotency_keys/up.sql");

const FIXTURE: &str = r"
DROP TABLE IF EXISTS autumn_idempotency_keys;
DROP TABLE IF EXISTS ledger_idem_payments;
CREATE TABLE ledger_idem_payments (id BIGSERIAL PRIMARY KEY, amount BIGINT NOT NULL);
";

/// Fills the table. `:rows` is the fixture size; times are Unix ms.
const FILL: &str = r"
INSERT INTO autumn_idempotency_keys
    (storage_key, record, recovery_point, recovery_body_hash, locked_by,
     locked_until_ms, expires_at_ms, ttl_ms, record_owner)
SELECT 'v2:' || encode(sha256(('fixture-' || i)::bytea), 'hex'),
       CASE WHEN i % 200 < 192
            THEN decode(repeat(md5(i::text), 13 + (i * 7919) % 75), 'hex') END,
       CASE WHEN i % 200 BETWEEN 192 AND 195 THEN 'step-' || (i % 4) END,
       CASE WHEN i % 200 BETWEEN 192 AND 195 THEN sha256(i::text::bytea) END,
       CASE WHEN i % 200 = 196 THEN md5('owner' || i) END,
       CASE WHEN i % 200 = 196 THEN now_ms + 60000 ELSE 0 END,
       CASE WHEN i <= 1500 THEN now_ms - 1000 - i
            ELSE now_ms + (i::bigint * 86400000) / :rows END,
       86400000,
       CASE WHEN i % 200 < 192 THEN md5('owner' || i) END
FROM generate_series(1, :rows) AS i,
     (SELECT (extract(epoch FROM clock_timestamp()) * 1000)::bigint AS now_ms) c;
UPDATE autumn_idempotency_keys SET ttl_ms = ttl_ms
WHERE abs(hashtext(storage_key)) % 100 < 3;
ANALYZE autumn_idempotency_keys;
";

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    rows: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
    #[diesel(sql_type = BigInt)]
    temp: i64,
    #[diesel(sql_type = BigInt)]
    wal: i64,
}

async fn count(conn: &mut AsyncPgConnection, sql: &str) -> i64 {
    diesel::sql_query(sql)
        .get_result::<Count>(conn)
        .await
        .expect("count")
        .n
}

/// Table counters (`n_tup_upd`, `n_tup_hot_upd`, `n_tup_ins`, `n_tup_del`)
/// and the WAL position. Flushes this session's stats first, and waits for
/// the other backends' flushes to settle.
async fn table_counters(conn: &mut AsyncPgConnection) -> [i64; 5] {
    let mut last = [-1; 5];
    for _ in 0..50 {
        conn.batch_execute("SELECT pg_stat_force_next_flush(); SELECT pg_stat_clear_snapshot();")
            .await
            .expect("flush stats");
        let mut now = [0; 5];
        for (slot, column) in ["n_tup_upd", "n_tup_hot_upd", "n_tup_ins", "n_tup_del"]
            .iter()
            .enumerate()
        {
            now[slot] = count(
                conn,
                &format!(
                    "SELECT {column} AS n FROM pg_stat_user_tables \
                     WHERE relname = 'autumn_idempotency_keys'"
                ),
            )
            .await;
        }
        now[4] = count(
            conn,
            "SELECT (pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint AS n",
        )
        .await;
        if now[..4] == last[..4] {
            return now;
        }
        last = now;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    last
}

async fn profile(conn: &mut AsyncPgConnection, label: &str, requests: i64, before: [i64; 5]) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = table_counters(conn).await;
    let rows = diesel::sql_query(
        "SELECT query, calls, rows, \
                (shared_blks_hit + shared_blks_read)::bigint AS buffers, \
                (temp_blks_read + temp_blks_written)::bigint AS temp, \
                wal_bytes::bigint AS wal \
         FROM pg_stat_statements \
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
           AND query NOT ILIKE '%pg_stat%' AND query NOT ILIKE '%pg_current_wal_lsn%' \
         ORDER BY buffers DESC, calls DESC, query",
    )
    .load::<StatementRow>(conn)
    .await
    .expect("read pg_stat_statements");
    println!("\n=== {label}: {requests} requests ===");
    println!(
        "{:>6} {:>8} {:>9} {:>5} {:>10}  statement",
        "calls", "rows", "buffers", "temp", "wal_bytes"
    );
    let (mut calls, mut buffers, mut wal) = (0, 0, 0);
    for row in &rows {
        let text: String = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        let short: String = text.chars().take(150).collect();
        println!(
            "{:>6} {:>8} {:>9} {:>5} {:>10}  {short}",
            row.calls, row.rows, row.buffers, row.temp, row.wal
        );
        calls += row.calls;
        buffers += row.buffers;
        wal += row.wal;
    }
    #[allow(clippy::cast_precision_loss)]
    let per = |v: i64| v as f64 / requests as f64;
    println!(
        "-- total: statements={calls} ({:.2}/req) buffers={buffers} ({:.1}/req) \
         statement_wal={wal} ({:.0}/req) --",
        per(calls),
        per(buffers),
        per(wal)
    );
    let updates = after[0] - before[0];
    let hot = after[1] - before[1];
    println!(
        "-- autumn_idempotency_keys: inserts={} updates={updates} hot_updates={hot} \
         deletes={} | wal_lsn_delta={} ({:.0}/req) --",
        after[2] - before[2],
        after[3] - before[3],
        after[4] - before[4],
        per(after[4] - before[4])
    );
}

async fn reset(conn: &mut AsyncPgConnection) -> [i64; 5] {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .await
        .expect("reset pg_stat_statements");
    table_counters(conn).await
}

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[post("/pay")]
async fn pay(idem: IdempotencyTx, mut db: Db) -> AutumnResult<axum::response::Response> {
    db.tx(|conn| {
        async move {
            let row: IdRow = diesel::sql_query(
                "INSERT INTO ledger_idem_payments (amount) VALUES (100) RETURNING id",
            )
            .get_result(conn)
            .await?;
            let body = Json(serde_json::json!({ "payment": row.id, "status": "captured" }));
            idem.commit(conn, (StatusCode::CREATED, body)).await
        }
        .scope_boxed()
    })
    .await
}

#[post("/plain")]
async fn plain() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "accepted": true, "status": "queued" })),
    )
}

fn client(pool: &Pool<AsyncPgConnection>) -> TestClient {
    let mut config = AutumnConfig::default();
    config.idempotency.backend = IdempotencyBackend::Database;
    TestApp::new()
        .config(config)
        .idempotent()
        .with_db(pool.clone())
        .routes(routes![pay, plain])
        .build()
}

async fn server() -> (String, Option<testcontainers::ContainerAsync<Postgres>>) {
    if let Ok(url) = std::env::var("AUTUMN_LEDGER_PG_URL") {
        return (url, None);
    }
    let container = Postgres::default()
        .with_tag("16-alpine")
        .with_cmd([
            "-c",
            "fsync=off",
            "-c",
            "autovacuum=off",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
        ])
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        Some(container),
    )
}

async fn post_all(client: &TestClient, path: &str, keys: &[String], expect: StatusCode) {
    for key in keys {
        let response = client
            .post(path)
            .header("idempotency-key", key)
            .send()
            .await;
        assert_eq!(response.status, expect, "{path} {key}");
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn idempotency_db_request_profile() {
    let (url, _container) = server().await;
    let rows: i64 = std::env::var("AUTUMN_LEDGER_IDEMPOTENCY_KEYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(4).build().expect("pool");
    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .await
        .expect("pg_stat_statements");
    conn.batch_execute(FIXTURE).await.expect("fixture");
    conn.batch_execute(IDEMPOTENCY_UP).await.expect("migration");
    conn.batch_execute(&FILL.replace(":rows", &rows.to_string()))
        .await
        .expect("fill");
    let size = count(
        &mut conn,
        "SELECT pg_total_relation_size('autumn_idempotency_keys') AS n",
    )
    .await;
    println!(
        "fixture: {rows} rows, dead tuples {}, total size {} MB",
        count(
            &mut conn,
            "SELECT n_dead_tup AS n FROM pg_stat_user_tables \
             WHERE relname = 'autumn_idempotency_keys'"
        )
        .await,
        size / 1_048_576
    );

    let client = client(&pool);
    // One request first, so lazy statics and the pool are warm.
    post_all(&client, "/plain", &["warm-up".to_owned()], StatusCode::CREATED).await;

    let fresh_tx: Vec<String> = (0..1024).map(|i| format!("pay-{i:05}")).collect();
    let fresh_plain: Vec<String> = (0..1024).map(|i| format!("plain-{i:05}")).collect();
    let replay: Vec<String> = fresh_tx.iter().step_by(2).cloned().collect();

    let before = reset(&mut conn).await;
    post_all(&client, "/pay", &fresh_tx, StatusCode::CREATED).await;
    profile(&mut conn, "fresh-tx", 1024, before).await;

    let before = reset(&mut conn).await;
    post_all(&client, "/plain", &fresh_plain, StatusCode::CREATED).await;
    profile(&mut conn, "fresh-plain", 1024, before).await;

    let before = reset(&mut conn).await;
    post_all(&client, "/pay", &replay, StatusCode::CREATED).await;
    profile(&mut conn, "replay", 512, before).await;

    let payments = count(&mut conn, "SELECT COUNT(*) AS n FROM ledger_idem_payments").await;
    assert_eq!(payments, 1024, "a replay must not charge twice");
}
