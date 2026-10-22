//! Ledger profiling harness for the outbox relay's claim (`outbox::drain`).
//!
//! Hypothesis: the claim statement walks `autumn_outbox_pkey` in `seq` order
//! and filters `dispatched_at IS NULL`, so every claim reads all the sent rows
//! that are still inside the retention window before it reaches a pending one.
//!
//! Workload: 50 `Outbox::write` calls, then `outbox::drain(state, 300)` (three
//! claims of the default batch of 100), against `HISTORY` sent rows plus a
//! 50,000-row pending backlog (see `docs/perf/outbox-claim/fixture.sql`). The
//! history size is swept over three tiers so the plan shape is shown scaling.
//!
//! `#[ignore]`d and skipped by name in ci.yml's Docker sweep: it seeds up to 1M
//! rows. Run with Docker, or point `OUTBOX_PROFILE_DATABASE_URL` at a Postgres
//! with `shared_preload_libraries = pg_stat_statements`:
//!
//! ```text
//! cargo test -p autumn-web --features test-support --test integration_tests \
//!   outbox_claim_profile -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(all(feature = "db", feature = "test-support", not(feature = "sqlite")))]
#![allow(
    clippy::cast_precision_loss,
    clippy::items_after_statements,
    clippy::struct_field_names
)]

use std::sync::{Arc, Mutex};

use autumn_web::config::OutboxConfig;
use autumn_web::outbox::{self, Outbox, OutboxMessage};
use autumn_web::test::TestApp;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;

const FIXTURE: &str = include_str!("../../../docs/perf/outbox-claim/fixture.sql");
const TIERS: [i64; 3] = [100_000, 300_000, 950_000];
const TOPICS: [&str; 4] = [
    "order.placed",
    "mail.deliver",
    "webhook.dispatch",
    "job.enqueue",
];

#[derive(QueryableByName)]
struct Stmt {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
    #[diesel(sql_type = BigInt)]
    temp_written: i64,
    #[diesel(sql_type = BigInt)]
    wal_bytes: i64,
}

#[derive(QueryableByName)]
struct Line {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

#[derive(QueryableByName)]
struct Scan {
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = BigInt)]
    seq_tup_read: i64,
    #[diesel(sql_type = BigInt)]
    idx_scan: i64,
    #[diesel(sql_type = BigInt)]
    idx_tup_read: i64,
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn seed(conn: &mut PgConnection, history: i64) {
    conn.batch_execute("DROP TABLE IF EXISTS autumn_outbox, autumn_inbox")
        .unwrap();
    conn.batch_execute(outbox::SCHEMA_SQL).unwrap();
    conn.batch_execute(
        &FIXTURE
            .replace("{HISTORY}", &history.to_string())
            .replace("{NOW}", &now_ms().to_string()),
    )
    .unwrap();
}

fn profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers, \
                temp_blks_written AS temp_written, wal_bytes::bigint AS wal_bytes \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_%' AND query NOT ILIKE '%pg_catalog%' \
         ORDER BY buffers DESC, calls DESC, query",
    )
    .load::<Stmt>(conn)
    .unwrap();
    let total_b: i64 = rows.iter().map(|r| r.buffers).sum();
    let total_c: i64 = rows.iter().map(|r| r.calls).sum();
    println!("\n=== pg_stat_statements: {label} (ranked by buffers) ===");
    for r in &rows {
        let q = r.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "buffers={:<8} ({:>5.1}%) calls={:<5} ({:>5.1}%) temp_w={} wal={} {}",
            r.buffers,
            100.0 * r.buffers as f64 / total_b.max(1) as f64,
            r.calls,
            100.0 * r.calls as f64 / total_c.max(1) as f64,
            r.temp_written,
            r.wal_bytes,
            &q[..q.len().min(150)]
        );
    }
    println!("-- total: statements={total_c} buffers={total_b} --");
    let tables = diesel::sql_query(
        "SELECT relname AS name, seq_tup_read, idx_scan, idx_tup_fetch AS idx_tup_read \
         FROM pg_stat_user_tables WHERE relname = 'autumn_outbox'",
    )
    .load::<Scan>(conn)
    .unwrap();
    for t in tables {
        println!(
            "pg_stat_user_tables {}: seq_tup_read={} idx_scan={} idx_tup_fetch={}",
            t.name, t.seq_tup_read, t.idx_scan, t.idx_tup_read
        );
    }
    let idx = diesel::sql_query(
        "SELECT indexrelname AS name, 0::bigint AS seq_tup_read, idx_scan, idx_tup_read \
         FROM pg_stat_user_indexes WHERE relname = 'autumn_outbox' ORDER BY indexrelname",
    )
    .load::<Scan>(conn)
    .unwrap();
    for i in idx {
        println!(
            "pg_stat_user_indexes {}: idx_scan={} idx_tup_read={}",
            i.name, i.idx_scan, i.idx_tup_read
        );
    }
    (total_c, total_b)
}

fn explain_claim(conn: &mut PgConnection, label: &str) {
    use diesel::RunQueryDsl;
    let now = now_ms();
    let topics = TOPICS.map(|t| format!("'{t}'")).join(",");
    conn.batch_execute("BEGIN").unwrap();
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) \
         UPDATE autumn_outbox SET claim_token = 'tok', locked_until = {lease}, attempts = attempts + 1 \
         WHERE seq IN ( SELECT o.seq FROM autumn_outbox o \
           WHERE o.dispatched_at IS NULL AND o.dead_at IS NULL AND o.available_at <= {now} \
             AND (o.locked_until IS NULL OR o.locked_until <= {now}) AND o.topic IN ({topics}) \
             AND NOT EXISTS (SELECT 1 FROM autumn_outbox p WHERE p.aggregate = o.aggregate \
                 AND p.seq < o.seq AND p.dispatched_at IS NULL AND p.dead_at IS NULL) \
           ORDER BY o.seq LIMIT 100 FOR UPDATE SKIP LOCKED) \
         RETURNING seq, id, aggregate, topic, payload, attempts, created_at",
        lease = now + 60_000,
    ))
    .load::<Line>(conn)
    .unwrap();
    println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): claim, {label} ===");
    for l in lines {
        println!("{}", l.line);
    }
    conn.batch_execute("ROLLBACK").unwrap();
}

async fn url() -> (String, Option<Box<dyn std::any::Any>>) {
    if let Ok(url) = std::env::var("OUTBOX_PROFILE_DATABASE_URL") {
        return (url, None);
    }
    use testcontainers::ImageExt;
    use testcontainers::runners::AsyncRunner;
    let container = testcontainers_modules::postgres::Postgres::default()
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
        .expect("postgres container");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        Some(Box::new(container)),
    )
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or OUTBOX_PROFILE_DATABASE_URL"]
async fn outbox_claim_profile() {
    use diesel::RunQueryDsl;
    let (url, _guard) = url().await;
    let mut conn = PgConnection::establish(&url).expect("sync connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .unwrap();

    let pool = Pool::builder(AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url))
        .build()
        .unwrap();
    let handled: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut app = TestApp::new()
        .with_db(pool.clone())
        .with_outbox(OutboxConfig::default());
    for topic in TOPICS {
        let handled = handled.clone();
        app = app.outbox_handler(topic, move |_, message: OutboxMessage| {
            let handled = handled.clone();
            async move {
                handled.lock().unwrap().push(message.id);
                Ok(())
            }
        });
    }
    let state = app.build().state().clone();

    let mut summary = Vec::new();
    for history in TIERS {
        seed(&mut conn, history);
        explain_claim(&mut conn, &format!("history={history}"));
        handled.lock().unwrap().clear();
        diesel::sql_query("SELECT pg_stat_statements_reset()")
            .execute(&mut conn)
            .unwrap();
        diesel::sql_query("SELECT pg_stat_reset()")
            .execute(&mut conn)
            .unwrap();

        let mut c = pool.get().await.unwrap();
        for n in 0..50 {
            Outbox::new(&state)
                .write(
                    &mut c,
                    &format!("fresh-{n}"),
                    TOPICS[n % 4],
                    &serde_json::json!({}),
                )
                .await
                .unwrap();
        }
        drop(c);
        let drained = outbox::drain(&state, 300).await.unwrap();
        assert_eq!(drained, 300);

        let ids = handled.lock().unwrap().clone();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&ids, &mut hasher);
        println!(
            "\n-- history={history}: handled {} messages, ordered-id hash {:016x}, first {:?} --",
            ids.len(),
            std::hash::Hasher::finish(&hasher),
            &ids[..3]
        );
        // pg_stat_user_* counters are flushed lazily.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let (calls, buffers) = profile(&mut conn, &format!("history={history}"));
        summary.push((history, calls, buffers));
    }
    // Existing install: tables made before the claim index existed get it from
    // `ensure_schema`, without a table lock (CONCURRENTLY) and without DDL when
    // it is already there.
    #[derive(QueryableByName)]
    struct Valid {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        valid: bool,
    }
    let claim_index_valid = |conn: &mut PgConnection| {
        diesel::sql_query(
            "SELECT i.indisvalid AS valid FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'idx_autumn_outbox_claim'",
        )
        .load::<Valid>(conn)
        .unwrap()
        .into_iter()
        .map(|v| v.valid)
        .collect::<Vec<_>>()
    };
    conn.batch_execute("DROP INDEX idx_autumn_outbox_claim")
        .unwrap();
    assert!(claim_index_valid(&mut conn).is_empty());
    outbox::ensure_schema(&pool).await.unwrap();
    // The build runs in a background task; wait for it.
    for _ in 0..100 {
        if !claim_index_valid(&mut conn).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        claim_index_valid(&mut conn),
        [true],
        "ensure_schema adds the index"
    );
    outbox::ensure_schema(&pool).await.unwrap();
    assert_eq!(claim_index_valid(&mut conn), [true], "and is idempotent");

    println!("\n=== summary (workload: 50 writes + drain(300)) ===");
    for (history, calls, buffers) in summary {
        println!("history={history:<8} statements={calls:<5} buffers={buffers}");
    }
}
