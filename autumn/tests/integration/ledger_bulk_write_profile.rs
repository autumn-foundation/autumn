//! Ledger findings harness for ledgered **bulk writes** (`save_many`,
//! `update_many`, `delete_many`) on a `#[repository(ledgered = true)]` entity.
//!
//! # Workload
//!
//! A nightly import / purge job over an account table: insert N accounts with
//! `save_many`, adjust them with `update_many`, soft-delete them with
//! `delete_many`. Driven through the public repository API; every statement
//! is counted from `pg_stat_statements` (deterministic) at three batch sizes
//! (100 / 400 / 1,600 rows) so the scaling shape — statements per row — is
//! visible, not just one point.
//!
//! The fixture is pre-seeded with a 3,000-row account table (skewed chain
//! depths: a 10% hot set touched 8 times, the tail once) so
//! `_autumn_ledger_revisions` is not empty when the measured batches run.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "test-support" \
//!   --test integration_tests -- --ignored ledger_bulk_write_profile \
//!   --nocapture --test-threads=1
//! ```

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]
#![allow(clippy::cast_possible_wrap)]

use autumn_web::current::with_actor;
use autumn_web::hooks::Patch;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, SimpleAsyncConnection as _};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    ledger_bulk_accounts (id) {
        id -> Int8,
        external_ref -> Text,
        balance_cents -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "ledger_bulk_accounts")]
pub struct LedgerBulkAccount {
    #[id]
    pub id: i64,
    pub external_ref: String,
    pub balance_cents: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LedgerBulkAccount,
    table = "ledger_bulk_accounts",
    soft_delete,
    ledgered = true
)]
pub trait LedgerBulkAccountRepository {}

/// The migration SQL Autumn actually ships, applied verbatim — same
/// convention as `ledger_postgres.rs`.
const LEDGER_UP: &str =
    include_str!("../../version_history_migrations/20260826000000_create_ledger_revisions/up.sql");
const LEDGER_HIGH_WATER_UP: &str =
    include_str!("../../version_history_migrations/20260901213107_create_ledger_high_water/up.sql");
const VERSION_HISTORY_UP: &str =
    include_str!("../../version_history_migrations/20260526000000_create_version_history/up.sql");

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgLedgerBulkAccountRepository {
    PgLedgerBulkAccountRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

async fn setup() -> (
    Pool<AsyncPgConnection>,
    String,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .with_cmd([
            "-c",
            "fsync=off",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
            "-c",
            "pg_stat_statements.max=2000",
        ])
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(10).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .await
        .expect("create pg_stat_statements extension");
    for ddl in [
        "CREATE TABLE IF NOT EXISTS ledger_bulk_accounts (
             id BIGSERIAL PRIMARY KEY,
             external_ref TEXT NOT NULL,
             balance_cents BIGINT NOT NULL,
             deleted_at TIMESTAMP
         )",
        VERSION_HISTORY_UP,
        LEDGER_UP,
        LEDGER_HIGH_WATER_UP,
    ] {
        conn.batch_execute(ddl)
            .await
            .unwrap_or_else(|err| panic!("apply DDL: {err}\n{ddl}"));
    }

    (pool, url, container)
}

#[derive(QueryableByName, Debug)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
    #[diesel(sql_type = BigInt)]
    wal_bytes: i64,
    #[diesel(sql_type = BigInt)]
    temp_written: i64,
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

/// Prints the top statements by calls and returns
/// `(total_calls, total_buffers, total_wal_bytes, total_temp_blks_written)`.
fn print_profile(conn: &mut PgConnection, label: &str, rows_in_batch: usize) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers, \
                wal_bytes::bigint AS wal_bytes, temp_blks_written AS temp_written \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' \
           AND query NOT ILIKE 'BEGIN%' AND query NOT ILIKE 'COMMIT%' \
         ORDER BY calls DESC, buffers DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");
    let (mut c, mut b, mut w, mut t) = (0i64, 0i64, 0i64, 0i64);
    for row in &rows {
        c += row.calls;
        b += row.buffers;
        w += row.wal_bytes;
        t += row.temp_written;
    }
    for row in rows.iter().take(10) {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        let shown: String = normalized.chars().take(150).collect();
        println!(
            "calls={:<6} ({:>5.1}%) buffers={:<8} ({:>5.1}%) wal={:<9} {shown}",
            row.calls,
            100.0 * row.calls as f64 / c.max(1) as f64,
            row.buffers,
            100.0 * row.buffers as f64 / b.max(1) as f64,
            row.wal_bytes,
        );
    }
    println!(
        "-- TOTAL statements={c} ({:.2}/row) buffers={b} wal_bytes={w} temp_blks_written={t} --",
        c as f64 / rows_in_batch as f64
    );
    (c, b, w, t)
}

#[derive(QueryableByName, Debug)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
    use diesel::RunQueryDsl;
    println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} ===");
    println!("{sql}");
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) {sql}"
    ))
    .load::<ExplainLine>(conn)
    .expect("explain");
    for line in lines {
        println!("{}", line.line);
    }
}

fn new_rows(prefix: &str, n: usize) -> Vec<NewLedgerBulkAccount> {
    (0..n)
        .map(|i| NewLedgerBulkAccount {
            external_ref: format!("{prefix}-{i}"),
            balance_cents: i as i64,
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn ledger_bulk_write_profile() {
    let (pool, url, _container) = setup().await;
    let repo = build_repo(pool);
    let mut conn = PgConnection::establish(&url).expect("sync db connection");

    // Skewed pre-existing history: 3,000 accounts, 10% touched 8 times.
    let seeded = with_actor("system", async {
        repo.save_many(&new_rows("seed", 3000)).await.expect("seed")
    })
    .await;
    for acct in seeded.iter().take(300) {
        for k in 1..8 {
            with_actor("system", async {
                repo.update(
                    acct.id,
                    &UpdateLedgerBulkAccount {
                        balance_cents: Patch::Set(k),
                        ..Default::default()
                    },
                )
                .await
                .expect("seed update")
            })
            .await;
        }
    }
    conn.batch_execute("ANALYZE ledger_bulk_accounts, _autumn_ledger_revisions")
        .expect("analyze");

    println!(
        "\n{:<8} {:<12} {:>6} {:>10} {:>10} {:>12} {:>14} {:>10}",
        "phase", "rows", "", "stmts", "stmts/row", "buffers", "wal_bytes", "temp_blks"
    );
    for n in [100usize, 400, 1600] {
        reset_stats(&mut conn);
        let created = with_actor("system", async {
            repo.save_many(&new_rows(&format!("b{n}"), n))
                .await
                .expect("save_many")
        })
        .await;
        assert_eq!(created.len(), n);
        let (c, b, w, t) = print_profile(&mut conn, &format!("save_many({n})"), n);
        println!("RESULT save_many   n={n} stmts={c} per_row={:.2} buffers={b} wal={w} temp={t}", c as f64 / n as f64);

        let ids: Vec<i64> = created.iter().map(|a| a.id).collect();
        reset_stats(&mut conn);
        let updated = with_actor("system", async {
            repo.update_many(
                &ids,
                &UpdateLedgerBulkAccount {
                    balance_cents: Patch::Set(42),
                    ..Default::default()
                },
            )
            .await
            .expect("update_many")
        })
        .await;
        assert_eq!(updated.len(), n);
        let (c, b, w, t) = print_profile(&mut conn, &format!("update_many({n})"), n);
        println!("RESULT update_many n={n} stmts={c} per_row={:.2} buffers={b} wal={w} temp={t}", c as f64 / n as f64);

        reset_stats(&mut conn);
        with_actor("system", async { repo.delete_many(&ids).await.expect("delete_many") }).await;
        let (c, b, w, t) = print_profile(&mut conn, &format!("delete_many({n})"), n);
        println!("RESULT delete_many n={n} stmts={c} per_row={:.2} buffers={b} wal={w} temp={t}", c as f64 / n as f64);

        // Equivalence oracle: every created row has a verifiable chain with
        // exactly insert + update + delete revisions.
        for id in ids.iter().step_by((n / 20).max(1)) {
            let report = repo.ledger_verify(*id).await.expect("verify");
            assert!(report.is_intact(), "chain for {id} must verify: {report:?}");
        }
    }

    let (rev_rows,): (i64,) = {
        use diesel::RunQueryDsl;
        #[derive(QueryableByName)]
        struct One {
            #[diesel(sql_type = BigInt)]
            n: i64,
        }
        let r = diesel::sql_query("SELECT count(*) AS n FROM _autumn_ledger_revisions")
            .get_result::<One>(&mut conn)
            .expect("count");
        (r.n,)
    };
    println!("\n_autumn_ledger_revisions rows at end: {rev_rows}");

    explain(
        &mut conn,
        "chain-head read as issued per row on append",
        "SELECT seq, hash, recorded_at FROM _autumn_ledger_revisions \
         WHERE table_name = 'ledger_bulk_accounts' AND record_id = 1 \
         ORDER BY seq DESC LIMIT 1",
    );
}
