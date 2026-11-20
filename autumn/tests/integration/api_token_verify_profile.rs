//! Ledger harness for the database API token store on its request path
//! (`DbApiTokenStore::verify_scoped` in `autumn/src/auth.rs`).
//!
//! It drives real `GET` requests with an `Authorization: Bearer` header
//! through the `RequireApiToken` layer of a `TestApp`, against an
//! `api_tokens` table shaped like a production one, and prints the
//! `pg_stat_statements` profile of each phase: statements, rows, shared
//! buffers and WAL bytes per request, plus the table's update and HOT update
//! counts.
//!
//! Point `AUTUMN_LEDGER_PG_URL` at a Postgres 16 server that preloads
//! `pg_stat_statements`, or leave it unset and the harness starts a
//! testcontainer. `AUTUMN_LEDGER_API_TOKENS` sets the fixture size (default
//! 1,000,000 rows). Reproduce with:
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests -- --ignored \
//!   api_token_verify_profile --nocapture --test-threads=1
//! ```
//!
//! See `docs/perf/api-token-verify/README.md` for the recorded results.
//!
//! ## Fixture
//!
//! `AUTUMN_LEDGER_API_TOKENS` rows. The raw token of row `i` is
//! `fixture-token-<i>`, stored as its SHA-256 hex, as `hash_api_token`
//! writes it.
//!
//! - About three tokens per principal, created over the last two years.
//! - 25% revoked (rotation history), 8% expired, 30% with a future expiry,
//!   the rest with none.
//! - One to four scopes each. 20% never used; the rest last used at some
//!   point in the last 90 days.
//! - 3% of rows are updated before `ANALYZE` with no `VACUUM`, so dead
//!   tuples are on the pages.
//! - 64 hot live tokens carry the traffic. 48 of them were last used within
//!   the last four minutes, so the 5-minute `last_used_at` stamp is not due.
//!   16 were last used an hour ago, so it is due once.
//!
//! ## Workload
//!
//! Each phase sends 2,048 requests. A seeded generator picks a hot token
//! with Zipf-like skew for 98% of them, and an unknown or revoked token for
//! 2% (answered 401).
//!
//! 1. `steady`: the fixture as above.
//! 2. `all-due`: every hot token's stamp is due before the phase starts.

#![cfg(all(feature = "db", feature = "postgres"))]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::auth::{DbApiTokenStore, RequireApiToken};
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use diesel::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _};
use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CREATE_UP: &str = include_str!("../../migrations/20260512000000_create_api_tokens/up.sql");
const SCOPED_UP: &str =
    include_str!("../../migrations/20260630000000_add_scoped_token_columns/up.sql");

/// Ids of the hot tokens: 160 rows spread over the table from 1,000, less the
/// ones the fill makes revoked or expired.
const HOT: &str = "SELECT i FROM generate_series(1000, :rows, :rows / 160) AS i \
                   WHERE i % 100 >= 33 ORDER BY i LIMIT 64";

/// Fills the table. `:rows` is the fixture size. Times are UTC wall clock,
/// as the store writes them.
const FILL: &str = r#"
INSERT INTO api_tokens
    (token_hash, principal_id, created_at, revoked_at, name, scopes, expires_at, last_used_at)
SELECT encode(sha256(('fixture-token-' || i)::bytea), 'hex'),
       'user:' || ((i::bigint * 2654435761) % (:rows / 3 + 1)),
       n - make_interval(secs => (i::bigint * 7907) % 63072000),
       CASE WHEN i % 100 < 25 THEN n - make_interval(secs => (i::bigint * 104729) % 31536000) END,
       CASE (i % 5) WHEN 0 THEN '' WHEN 1 THEN 'ci' WHEN 2 THEN 'deploy bot'
                    WHEN 3 THEN 'laptop ' || (i % 97) ELSE 'integration ' || (i % 13) END,
       (ARRAY['["posts:read"]', '["posts:read", "posts:write"]',
              '["admin", "posts:read", "posts:write"]',
              '["billing:read", "posts:read", "posts:write", "users:read"]'])[1 + i % 4]::jsonb,
       CASE WHEN i % 100 BETWEEN 25 AND 32 THEN n - make_interval(secs => 1 + (i::bigint * 31) % 2592000)
            WHEN i % 100 BETWEEN 33 AND 62 THEN n + make_interval(secs => 86400 + (i::bigint * 31) % 31536000)
       END,
       CASE WHEN i % 10 < 2 THEN NULL
            ELSE n - make_interval(secs => 600 + (i::bigint * 7877) % 7776000) END
FROM generate_series(1, :rows) AS i,
     (SELECT (now() AT TIME ZONE 'UTC') AS n) c;
UPDATE api_tokens SET name = name WHERE abs(hashtext(token_hash)) % 100 < 3;
ANALYZE api_tokens;
"#;

/// The steady state: 48 hot tokens used within the last four minutes, 16
/// last used an hour ago.
const STEADY: &str = r"
UPDATE api_tokens t SET last_used_at = (now() AT TIME ZONE 'UTC')
    - CASE WHEN h.rank % 4 = 3 THEN interval '1 hour'
           ELSE make_interval(secs => 10 + (h.rank * 3) % 230) END
FROM (SELECT i, row_number() OVER (ORDER BY i) - 1 AS rank FROM (:hot) hot(i)) h
WHERE t.token_hash = encode(sha256(('fixture-token-' || h.i)::bytea), 'hex');
";

/// Every hot token's stamp is due.
const ALL_DUE: &str = r"
UPDATE api_tokens t SET last_used_at = (now() AT TIME ZONE 'UTC') - interval '1 hour'
FROM (:hot) hot(i)
WHERE t.token_hash = encode(sha256(('fixture-token-' || hot.i)::bytea), 'hex');
";

/// Which hot tokens were stamped during the phase that started at `:since`.
const STAMPED: &str = r"
SELECT COALESCE(string_agg(t.id::text, ',' ORDER BY t.id), '') AS s
FROM api_tokens t JOIN (:hot) hot(i)
  ON t.token_hash = encode(sha256(('fixture-token-' || hot.i)::bytea), 'hex')
WHERE t.last_used_at >= ':since'::timestamp
";

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct Str {
    #[diesel(sql_type = Text)]
    s: String,
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

async fn text(conn: &mut AsyncPgConnection, sql: &str) -> String {
    diesel::sql_query(sql)
        .get_result::<Str>(conn)
        .await
        .expect("text")
        .s
}

/// `n_tup_upd` and `n_tup_hot_upd` of `api_tokens`, once the other
/// backends' flushes settle.
async fn table_counters(conn: &mut AsyncPgConnection) -> [i64; 2] {
    let mut last = [-1; 2];
    for _ in 0..50 {
        conn.batch_execute("SELECT pg_stat_force_next_flush(); SELECT pg_stat_clear_snapshot();")
            .await
            .expect("flush stats");
        let mut now = [0; 2];
        for (slot, column) in ["n_tup_upd", "n_tup_hot_upd"].iter().enumerate() {
            now[slot] = count(
                conn,
                &format!(
                    "SELECT {column} AS n FROM pg_stat_user_tables WHERE relname = 'api_tokens'"
                ),
            )
            .await;
        }
        if now == last {
            return now;
        }
        last = now;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    last
}

async fn profile(conn: &mut AsyncPgConnection, label: &str, requests: i64, before: [i64; 2]) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = table_counters(conn).await;
    let rows = diesel::sql_query(
        "SELECT query, calls, rows, \
                (shared_blks_hit + shared_blks_read)::bigint AS buffers, \
                (temp_blks_read + temp_blks_written)::bigint AS temp, \
                wal_bytes::bigint AS wal \
         FROM pg_stat_statements \
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
           AND query NOT ILIKE '%pg_stat%' \
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
        let short: String = text.chars().take(170).collect();
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
        "-- total: statements={calls} ({:.2}/req) buffers={buffers} ({:.2}/req) \
         statement_wal={wal} ({:.0}/req) --",
        per(calls),
        per(buffers),
        per(wal)
    );
    println!(
        "-- api_tokens: updates={} hot_updates={} --",
        after[0] - before[0],
        after[1] - before[1]
    );
}

#[get("/data")]
async fn data() -> &'static str {
    "ok"
}

fn client(pool: &Pool<AsyncPgConnection>) -> TestClient {
    let store = Arc::new(DbApiTokenStore::new(pool.clone()));
    TestApp::new()
        .with_db(pool.clone())
        .routes(routes![data])
        .layer(RequireApiToken::new(store))
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

/// The seeded request sequence: a raw token and the status it must get.
fn requests(hot: &[i64], rows: i64) -> Vec<(String, StatusCode)> {
    let mut state: u64 = 0x5eed_1ed6_e400_0001;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    (0..2048)
        .map(|_| {
            let r = next();
            if r % 100 < 2 {
                // An unknown token, or a revoked one (`i % 100 < 25`).
                if r % 2 == 0 {
                    (format!("unknown-token-{r}"), StatusCode::UNAUTHORIZED)
                } else {
                    let revoked = i64::try_from(r % u64::try_from(rows / 100).expect("rows"))
                        .expect("id")
                        * 100
                        + 7;
                    (
                        format!("fixture-token-{revoked}"),
                        StatusCode::UNAUTHORIZED,
                    )
                }
            } else {
                // Zipf-like: rank k with weight 1 / (k + 1).
                let weights: f64 = (1..=hot.len()).map(|k| 1.0 / k as f64).sum();
                #[allow(clippy::cast_precision_loss)]
                let mut pick = (next() % 1_000_000) as f64 / 1_000_000.0 * weights;
                let mut index = hot.len() - 1;
                for (k, _) in hot.iter().enumerate() {
                    #[allow(clippy::cast_precision_loss)]
                    let w = 1.0 / (k + 1) as f64;
                    if pick < w {
                        index = k;
                        break;
                    }
                    pick -= w;
                }
                (format!("fixture-token-{}", hot[index]), StatusCode::OK)
            }
        })
        .collect()
}

/// One measured phase. The app gets a pool of its own, warmed by one request
/// before the counters reset, and dropped before they are read: its backends
/// exit, so their table statistics are flushed.
async fn phase(
    url: &str,
    conn: &mut AsyncPgConnection,
    label: &str,
    hot_sql: &str,
    requests: &[(String, StatusCode)],
) {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager).max_size(4).build().expect("pool");
    let client = client(&pool);
    let warm = client
        .get("/data")
        .header("authorization", "Bearer warm-up-unknown")
        .send()
        .await;
    assert_eq!(warm.status, StatusCode::UNAUTHORIZED);
    let since = text(
        conn,
        "SELECT ((now() AT TIME ZONE 'UTC') - interval '1 second')::text AS s",
    )
    .await;
    conn.batch_execute("CHECKPOINT; SELECT pg_stat_statements_reset()")
        .await
        .expect("reset pg_stat_statements");
    let before = table_counters(conn).await;
    let (mut ok, mut denied) = (0, 0);
    for (token, expect) in requests {
        let response = client
            .get("/data")
            .header("authorization", &format!("Bearer {token}"))
            .send()
            .await;
        assert_eq!(response.status, *expect, "{token}");
        if *expect == StatusCode::OK {
            ok += 1;
        } else {
            denied += 1;
        }
    }
    drop(client);
    pool.close();
    drop(pool);
    profile(
        conn,
        label,
        i64::try_from(requests.len()).expect("len"),
        before,
    )
    .await;
    let stamped = text(
        conn,
        &STAMPED.replace(":hot", hot_sql).replace(":since", &since),
    )
    .await;
    println!(
        "-- outcome: 200={ok} 401={denied} stamped_hot_ids=[{stamped}] ({} tokens) --",
        if stamped.is_empty() {
            0
        } else {
            stamped.split(',').count()
        }
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn api_token_verify_profile() {
    let (url, _container) = server().await;
    let rows: i64 = std::env::var("AUTUMN_LEDGER_API_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(2).build().expect("pool");
    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .await
        .expect("pg_stat_statements");
    conn.batch_execute("DROP TABLE IF EXISTS api_tokens")
        .await
        .expect("drop");
    conn.batch_execute(CREATE_UP).await.expect("create");
    conn.batch_execute(SCOPED_UP).await.expect("scoped columns");
    conn.batch_execute(&FILL.replace(":rows", &rows.to_string()))
        .await
        .expect("fill");
    let hot_sql = HOT.replace(":rows", &rows.to_string());
    let hot: Vec<i64> = diesel::sql_query(hot_sql.replace("SELECT i FROM", "SELECT i::bigint AS n FROM"))
        .load::<Count>(&mut conn)
        .await
        .expect("hot ids")
        .into_iter()
        .map(|c| c.n)
        .collect();
    assert_eq!(hot.len(), 64, "fixture too small for 64 hot tokens");
    println!(
        "fixture: {rows} rows, dead tuples {}, heap {} MB, indexes {} MB",
        count(
            &mut conn,
            "SELECT n_dead_tup AS n FROM pg_stat_user_tables WHERE relname = 'api_tokens'"
        )
        .await,
        count(&mut conn, "SELECT pg_table_size('api_tokens') AS n").await / 1_048_576,
        count(&mut conn, "SELECT pg_indexes_size('api_tokens') AS n").await / 1_048_576,
    );

    let requests = requests(&hot, rows);
    conn.batch_execute(&STEADY.replace(":hot", &hot_sql))
        .await
        .expect("steady");
    phase(&url, &mut conn, "steady", &hot_sql, &requests).await;
    conn.batch_execute(&ALL_DUE.replace(":hot", &hot_sql))
        .await
        .expect("all due");
    phase(&url, &mut conn, "all-due", &hot_sql, &requests).await;
}
