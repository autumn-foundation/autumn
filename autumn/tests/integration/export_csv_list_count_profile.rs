//! Ledger findings harness for the generated `list()` repository method
//! (`autumn-macros/src/repository.rs`, `list_impl_method`), driven through the
//! exact page-fetch loop `autumn-cli`'s scaffold generator emits for every
//! `GET /{plural}/export.csv` handler (`autumn-cli/src/generate/scaffold.rs`
//! ~6104-6158).
//!
//! That generator's own doc comment already names the cost qualitatively:
//! "`list` runs a filtered `COUNT(*)` before each page, so a full export is
//! ~100 page queries AND ~100 whole-result-set counts... The counts are pure
//! waste here: this loop never reads `total_elements`". This harness puts a
//! measured number on that claim against a production-shaped fixture, via
//! `pg_stat_statements`, and shows the cost is worse than "redundant" — the
//! `COUNT(*)` query carries no `LIMIT`, so it re-scans the *entire filtered
//! result set* on every single page, not just its own page's slice.
//!
//! This is a **findings issue** harness, not a before/after fix: the fix
//! (a count-free listing method) is a new, additive method on the generated
//! repository trait — every repository the `#[repository]` macro emits, not
//! one call site — which the Ledger process treats as a human design
//! decision, not a "smallest change that moves the counter".
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep
//! (`-- --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "test-support" \
//!   --test integration_tests -- --ignored export_csv_list_count_profile \
//!   --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! A 10,500-row `orders` table — a realistic CSV-export target (an
//! operator exporting an order ledger). Skewed customer cardinality (80% of
//! rows land on 200 repeat customers, 20% are one-off long-tail customers),
//! a skewed `status` distribution (50% paid / 30% pending / 15% cancelled /
//! 5% refunded — the four tiers below), 70% NULL density on `notes`, and
//! real dead tuples from a follow-up `UPDATE` before `ANALYZE`. The four
//! tiers use `status` as the export filter (`?filter[status]=paid`, the
//! actual `ListQuery` allowlist path `list()` serves), so each tier is a
//! disjoint, differently-sized slice of the SAME table rather than a
//! separate fixture — the unfiltered tier also exercises the generator's
//! `MAX_EXPORT_ROWS` truncation path (10,500 > 10,000).

#![cfg(feature = "db")]
#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use autumn_web::pagination::{ListQuery, MAX_PAGE_SIZE, PageRequest, SortDir};
use autumn_web::repository::ReadRoute;
// Deliberately NOT `diesel::prelude::*`: the real upstream prelude re-exports
// the SYNC `diesel::RunQueryDsl`, which conflicts with the `diesel_async`
// version the `#[repository]` macro's generated method bodies import locally
// (`autumn_web::reexports::diesel::prelude` is a curated subset that omits
// it for exactly this reason). Pulling the real prelude in at module scope
// makes every `.load()`/`.execute()` call inside the macro-generated
// `list()`/`page()` impl ambiguous (E0034: multiple applicable items), since
// an outer glob import stays a candidate for method resolution even inside
// an inner scope with its own explicit `use` of the same trait name. The
// sync trait is instead imported locally, only inside the plain-`PgConnection`
// helper functions below that actually need it.
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

mod schema {
    diesel::table! {
        ledger_export_orders (id) {
            id -> Int8,
            customer_email -> Text,
            status -> Text,
            total_cents -> Int8,
            notes -> Nullable<Text>,
            created_at -> Timestamp,
        }
    }
}
use schema::ledger_export_orders;

#[autumn_web::model(table = "ledger_export_orders")]
pub struct LedgerExportOrder {
    #[id]
    pub id: i64,
    pub customer_email: String,
    pub status: String,
    pub total_cents: i64,
    pub notes: Option<String>,
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::repository(LedgerExportOrder, table = "ledger_export_orders")]
pub trait LedgerExportOrderRepository {}

/// Mirrors `autumn-cli/src/generate/scaffold.rs`'s `export_csv` template
/// exactly (loop shape, batch size, cap, truncation check) but reads from the
/// generated `list()` method directly rather than through a compiled
/// scaffolded HTTP app — same generated code path, since `list()` is produced
/// by the same `#[repository]` macro either way and this harness's model
/// declares no fields the generator's template does not already handle
/// (plain `String`/`i64`/`Option<String>` columns).
const MAX_EXPORT_ROWS: usize = 10_000;

async fn run_export_loop(
    repo: &PgLedgerExportOrderRepository,
    query: &ListQuery,
) -> (usize, bool, u32) {
    let batch_size = MAX_PAGE_SIZE;
    let mut rows: Vec<LedgerExportOrder> = Vec::new();
    let mut page: u32 = 1;
    loop {
        let page_req = PageRequest::new(page, batch_size);
        let batch = repo.list(query, &page_req).await.expect("list");
        let exhausted = batch.content.len() < batch_size as usize;
        rows.extend(batch.content);
        if exhausted || rows.len() > MAX_EXPORT_ROWS {
            break;
        }
        page += 1;
    }
    let truncated = rows.len() > MAX_EXPORT_ROWS;
    if truncated {
        rows.truncate(MAX_EXPORT_ROWS);
    }
    (rows.len(), truncated, page)
}

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgLedgerExportOrderRepository {
    PgLedgerExportOrderRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const TOTAL_ROWS: i64 = 10_500;

/// Skewed customer cardinality, skewed status distribution (50/30/15/5),
/// 70% NULL `notes`, real dead tuples from a follow-up `UPDATE`+`ANALYZE` —
/// the fixture shape the Ledger process requires (real row counts, real
/// cardinality skew, real NULL density, real dead-tuple ratio).
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO ledger_export_orders \
         (customer_email, status, total_cents, notes, created_at) \
         SELECT \
           'customer' || (CASE WHEN i % 5 = 0 THEN 100000 + i ELSE i % 200 END) || '@example.com', \
           CASE \
             WHEN i % 20 < 10 THEN 'paid' \
             WHEN i % 20 < 16 THEN 'pending' \
             WHEN i % 20 < 19 THEN 'cancelled' \
             ELSE 'refunded' \
           END, \
           500 + (i * 137) % 100000, \
           CASE WHEN i % 10 < 7 THEN NULL ELSE 'left a note: ' || i END, \
           TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval \
         FROM generate_series(1, {TOTAL_ROWS}) AS i"
    ))
    .expect("seed ledger_export_orders");

    // Real dead tuples: touch a slice of rows post-insert, same technique
    // the offline-sync Ledger harnesses use.
    conn.batch_execute("UPDATE ledger_export_orders SET notes = 'reconciled' WHERE id % 7 = 0")
        .expect("create dead tuples");
    conn.batch_execute("ANALYZE ledger_export_orders")
        .expect("analyze");
}

#[derive(QueryableByName, Debug)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

/// Prints every `ledger_export_orders` statement from this run, and returns
/// `(count_calls, count_buffers, select_calls, select_buffers)` — the two
/// statement SHAPES the generator's loop issues, isolated from each other so
/// the wasted-`COUNT` claim is a direct number, not eyeballed off the dump.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%ledger_export_orders%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut count_calls, mut count_buffers) = (0i64, 0i64);
    let (mut select_calls, mut select_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.contains("COUNT(*)") {
            count_calls += row.calls;
            count_buffers += row.buffers;
        } else if normalized.starts_with("SELECT") {
            select_calls += row.calls;
            select_buffers += row.buffers;
        }
    }
    println!(
        "-- COUNT(*) statement: calls={count_calls} buffers={count_buffers} -- \
         row SELECT statement: calls={select_calls} buffers={select_buffers} --"
    );
    (count_calls, count_buffers, select_calls, select_buffers)
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn export_csv_list_count_profile() {
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

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    conn.batch_execute(
        "CREATE TABLE ledger_export_orders ( \
            id BIGSERIAL PRIMARY KEY, \
            customer_email TEXT NOT NULL, \
            status TEXT NOT NULL, \
            total_cents BIGINT NOT NULL, \
            notes TEXT, \
            created_at TIMESTAMP NOT NULL \
         )",
    )
    .expect("create ledger_export_orders");

    seed_fixture(&mut conn);

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let repo = build_repo(pool);

    // Four disjoint, differently-sized slices of the SAME table, driven
    // through the actual `?filter[status]=` allowlist path `list()` serves.
    // Expected row counts from the seed's 50/30/15/5 split of 10,500 rows.
    let tiers: [(&str, i64); 4] = [
        ("refunded", 525),
        ("cancelled", 1_575),
        ("pending", 3_150),
        ("paid", 5_250),
    ];

    let mut tier_results = Vec::new();
    for (status, expected_rows) in tiers {
        let query = ListQuery::new(None, SortDir::Asc, &[("status", status)]);
        reset_stats(&mut conn);
        let (exported, truncated, last_page) = run_export_loop(&repo, &query).await;
        assert_eq!(
            exported, expected_rows as usize,
            "tier {status} must export exactly its seeded row count"
        );
        assert!(!truncated, "tier {status} must not hit MAX_EXPORT_ROWS");
        let (count_calls, count_buffers, select_calls, select_buffers) = print_profile(
            &mut conn,
            &format!("export tier status={status} ({expected_rows} rows)"),
        );
        assert_eq!(
            count_calls,
            i64::from(last_page),
            "one COUNT(*) call per page fetched, exactly"
        );
        assert_eq!(
            select_calls,
            i64::from(last_page),
            "one row SELECT call per page fetched, exactly"
        );
        tier_results.push((
            status,
            expected_rows,
            count_calls,
            count_buffers,
            select_calls,
            select_buffers,
        ));
    }

    // Unfiltered tier: exercises MAX_EXPORT_ROWS truncation (10,500 > 10,000).
    let query = ListQuery::default();
    reset_stats(&mut conn);
    let (exported, truncated, last_page) = run_export_loop(&repo, &query).await;
    assert_eq!(
        exported, MAX_EXPORT_ROWS,
        "unfiltered tier truncates at the cap"
    );
    assert!(truncated, "unfiltered tier (10,500 rows) must truncate");
    let (count_calls, count_buffers, select_calls, select_buffers) =
        print_profile(&mut conn, "export tier unfiltered (10,500 rows, truncates)");
    assert_eq!(
        count_calls,
        i64::from(last_page),
        "unfiltered tier: one COUNT(*) call per page fetched, exactly"
    );
    tier_results.push((
        "(unfiltered)",
        TOTAL_ROWS,
        count_calls,
        count_buffers,
        select_calls,
        select_buffers,
    ));

    println!("\n=== statement-count / buffer scaling across tiers ===");
    println!(
        "{:<14} {:>10} {:>12} {:>14} {:>12} {:>14}",
        "tier", "rows", "COUNT calls", "COUNT buffers", "SELECT calls", "SELECT buffers"
    );
    let mut total_count_buffers = 0i64;
    let mut total_select_buffers = 0i64;
    for (status, rows, count_calls, count_buffers, select_calls, select_buffers) in &tier_results {
        println!(
            "{status:<14} {rows:>10} {count_calls:>12} {count_buffers:>14} {select_calls:>12} {select_buffers:>14}"
        );
        total_count_buffers += count_buffers;
        total_select_buffers += select_buffers;
    }
    let total_buffers = total_count_buffers + total_select_buffers;
    let wasted_share = if total_buffers > 0 {
        (total_count_buffers as f64 / total_buffers as f64) * 100.0
    } else {
        0.0
    };
    println!(
        "\n-- across all 5 tiers: COUNT(*) buffers={total_count_buffers} \
         SELECT buffers={total_select_buffers} total={total_buffers} \
         (COUNT share={wasted_share:.1}%) --"
    );

    // Illustrative EXPLAIN of the two statement shapes at the largest
    // (unfiltered) tier — a diagnostic, not the scale claim; the scale claim
    // is the pg_stat_statements table above.
    explain(
        &mut conn,
        "COUNT(*) issued once per page, no LIMIT — rescans the whole matching set every call",
        "SELECT COUNT(*) FROM ledger_export_orders",
    );
    explain(
        &mut conn,
        "row SELECT for one page — bounded by LIMIT/OFFSET",
        "SELECT * FROM ledger_export_orders ORDER BY id DESC LIMIT 100 OFFSET 0",
    );
}
