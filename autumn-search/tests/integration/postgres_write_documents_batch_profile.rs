//! Ledger harness for `PostgresSearchStore::write_documents`
//! (`autumn-search/src/postgres.rs`) — the single write path behind both
//! `SearchBackend::index` and `SearchBackend::index_unless_newer`, and so the
//! statement `SearchClient::backfill` (`autumn-search/src/client.rs`) issues
//! for every batch of up to `DEFAULT_BACKFILL_BATCH` (500) documents.
//!
//! Before this fix, `write_documents` looped over its `documents` slice and
//! issued one `INSERT ... ON CONFLICT DO UPDATE` **per document**. A full
//! index backfill (`SearchClient::backfill_all`, the framework's own
//! `autumn_search_backfill` job) therefore cost one DB round trip per row
//! indexed rather than one per batch — invisible in a buffer-cost ranking
//! (each statement is cheap on its own) but dominant in
//! `pg_stat_statements.calls`, exactly the shape the Ledger process calls out
//! as hiding in plain sight. The fix batches every document in one
//! `write_documents` call into ONE multi-row statement (`VALUES` when
//! unconditional, `UNION ALL SELECT` when watermark-guarded, since the
//! watermark guard's `WHERE NOT EXISTS` has nowhere to attach to a bare
//! `VALUES` row) — see `upsert_sql`/`UpsertRow` in `postgres.rs`.
//!
//! **Requires Docker.** `#[ignore]`d so a default `cargo test` never needs
//! it; CI's Docker sweep runs `cargo test -p autumn-search -- --ignored`
//! automatically (see CLAUDE.md), no workflow edit needed. Run manually with:
//!
//! ```text
//! cargo test -p autumn-search --test search_tests -- --ignored \
//!   write_documents_batch_profile --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! A `search_tenant_articles` source table (the same shape the rest of this
//! suite's `TenantArticle` model uses), backfilled through the REAL public
//! entry point (`SearchClient::backfill`), at three cumulative tiers — 100,
//! 500, 2,000 rows — so the statement-count claim is shown scaling, not a
//! one-off. Tenant cardinality is skewed (80% of rows land on one of 15
//! repeat tenants, 20% are one-off long-tail tenants, 5% have no tenant at
//! all — a real NULL density), and every row carries a `#[searchable(embed)]`
//! body, so every write exercises the weighted-tsvector concatenation AND the
//! embedding-array column, not just the plain-text columns. The shared
//! `autumn_search_documents` table also carries 30,000 pre-existing rows
//! under an unrelated index name before any of this fixture's rows are
//! written, so `ON CONFLICT` / GIN index maintenance runs against a
//! realistically sized table rather than an empty one.

#![allow(
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::sync::Arc;

use autumn_search::{
    BackfillOptions, DocumentSource, HashingEmbedder, PostgresSearchStore, SearchBackend,
    SearchClient,
};
use autumn_web::search::SearchIndexed as _;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{Array, BigInt, Nullable, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

use super::support::TenantArticle;

/// Repeat-tenant pool size for the skewed cardinality below.
const BIG_TENANT_COUNT: i64 = 15;

/// Deterministic tenant assignment: 5% no tenant, ~76% one of
/// `BIG_TENANT_COUNT` repeat tenants, ~19% a unique long-tail tenant.
fn tenant_for(id: i64) -> Option<String> {
    if id % 20 == 0 {
        None
    } else if id % 5 != 4 {
        Some(format!("tenant-{:03}", id % BIG_TENANT_COUNT))
    } else {
        Some(format!("tenant-longtail-{id}"))
    }
}

/// Seed `[start_id, end_id]` source rows in ONE batched statement (the same
/// `UNNEST`-array batching this file's fix applies to the index writer).
fn seed_source(conn: &mut PgConnection, start_id: i64, end_id: i64) {
    use diesel::RunQueryDsl;

    let ids: Vec<i64> = (start_id..=end_id).collect();
    let titles: Vec<String> = ids
        .iter()
        .map(|id| format!("Report #{id} quarterly summary"))
        .collect();
    let bodies: Vec<String> = ids
        .iter()
        .map(|id| {
            format!(
                "Detailed narrative for record {id} covering revenue, headcount, and churn for \
                 the period."
            )
        })
        .collect();
    let tenants: Vec<Option<String>> = ids.iter().map(|id| tenant_for(*id)).collect();

    diesel::sql_query(
        "INSERT INTO search_tenant_articles (id, title, body, tenant_id) \
         SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::text[]) \
         ON CONFLICT (id) DO UPDATE SET title = EXCLUDED.title, body = EXCLUDED.body, \
           tenant_id = EXCLUDED.tenant_id",
    )
    .bind::<Array<BigInt>, _>(ids)
    .bind::<Array<Text>, _>(titles)
    .bind::<Array<Text>, _>(bodies)
    .bind::<Array<Nullable<Text>>, _>(tenants)
    .execute(conn)
    .expect("seed source rows");
}

/// Bulk pre-existing table volume under an unrelated index name, so the
/// shared `autumn_search_documents` table is not empty when the fixture's own
/// writes land.
fn seed_noise(conn: &mut PgConnection, count: i64) {
    use diesel::RunQueryDsl;

    diesel::sql_query(format!(
        "INSERT INTO autumn_search_documents (index_name, record_id, content, fields, \
           search_vector) \
         SELECT 'noise', gs, 'noise content ' || gs, jsonb_build_object('title', 'noise ' || gs), \
                to_tsvector('simple', 'noise content ' || gs) \
         FROM generate_series(1, {count}) AS gs"
    ))
    .execute(conn)
    .expect("seed noise rows");
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
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

/// Prints every statement from this run and returns
/// `(write_insert_calls, write_insert_buffers, total_calls, total_buffers)` —
/// the write statement isolated from everything else `backfill()` issues
/// (the source scan, the watermark read), so the N+1 claim is a direct
/// number.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;

    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY buffers DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut write_calls, mut write_buffers) = (0i64, 0i64);
    let (mut total_calls, mut total_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        total_calls += row.calls;
        total_buffers += row.buffers;
        if normalized.contains("INSERT INTO autumn_search_documents") {
            write_calls += row.calls;
            write_buffers += row.buffers;
        }
    }
    println!(
        "-- write_documents INSERT: calls={write_calls} buffers={write_buffers} -- \
         all statements this run: calls={total_calls} buffers={total_buffers} --"
    );
    (write_calls, write_buffers, total_calls, total_buffers)
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

/// Every written document, sorted deterministically (`record_id` is the
/// table's key suffix — no ties) — the result-equivalence snapshot. Run
/// against the pre-fix per-document loop and the post-fix batched statement
/// with the SAME fixture, this must be byte-identical.
#[derive(QueryableByName, Debug, PartialEq, Eq)]
struct RowDump {
    #[diesel(sql_type = BigInt)]
    record_id: i64,
    #[diesel(sql_type = Nullable<Text>)]
    tenant_id: Option<String>,
    #[diesel(sql_type = Text)]
    content: String,
    #[diesel(sql_type = Text)]
    fields: String,
    #[diesel(sql_type = Text)]
    search_vector: String,
    #[diesel(sql_type = Text)]
    embedding: String,
}

fn dump_documents(conn: &mut PgConnection) -> Vec<RowDump> {
    use diesel::RunQueryDsl;

    diesel::sql_query(
        "SELECT record_id, tenant_id, content, fields::text AS fields, \
                search_vector::text AS search_vector, embedding::text AS embedding \
         FROM autumn_search_documents \
         WHERE index_name = 'search_tenant_articles' \
         ORDER BY record_id",
    )
    .load::<RowDump>(conn)
    .expect("dump documents")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn write_documents_batch_profile() {
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
        "CREATE TABLE search_tenant_articles ( \
            id         BIGSERIAL PRIMARY KEY, \
            title      TEXT NOT NULL, \
            body       TEXT NOT NULL, \
            tenant_id  TEXT, \
            deleted_at TIMESTAMP \
         )",
    )
    .expect("create search_tenant_articles");

    let manager = AsyncDieselConnectionManager::<autumn_web::RuntimeConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(4).build().expect("pool");
    let store = Arc::new(PostgresSearchStore::new(None));
    store.install_pool(pool.clone());
    store
        .ensure_index(&TenantArticle::index_definition())
        .await
        .expect("ensure_index");

    seed_noise(&mut conn, 30_000);
    conn.batch_execute("ANALYZE autumn_search_documents")
        .expect("analyze");

    let client = SearchClient::builder()
        .backend(Arc::clone(&store) as Arc<dyn SearchBackend>)
        .source(Arc::clone(&store) as Arc<dyn DocumentSource>)
        .embedder(Arc::new(HashingEmbedder::new(32)))
        .index::<TenantArticle>()
        .build();

    // Cumulative tiers over the SAME table: each backfill re-scans and
    // re-upserts everything seeded so far, at the framework's real default
    // batch size (`BackfillOptions::default()`, 500) — not a size chosen to
    // flatter this harness.
    let tiers: [(i64, i64); 3] = [(1, 100), (101, 500), (501, 2_000)];
    let mut tier_results = Vec::new();
    for (start_id, end_id) in tiers {
        seed_source(&mut conn, start_id, end_id);
        reset_stats(&mut conn);
        let report = client
            .backfill("search_tenant_articles", &BackfillOptions::default())
            .await
            .expect("backfill");
        assert_eq!(
            report.indexed,
            u64::try_from(end_id).expect("end_id fits u64"),
            "backfill must index every row seeded so far"
        );
        let (write_calls, write_buffers, total_calls, total_buffers) = print_profile(
            &mut conn,
            &format!("backfill through id {end_id} ({end_id} total rows)"),
        );
        assert_eq!(
            write_calls,
            i64::try_from(report.batches).expect("batch count fits i64"),
            "one write_documents INSERT per backfill BATCH, not one per document \
             (report.batches={}, report.indexed={})",
            report.batches,
            report.indexed
        );
        tier_results.push((
            end_id,
            write_calls,
            write_buffers,
            total_calls,
            total_buffers,
        ));
    }

    println!("\n=== statement-count scaling across tiers ===");
    println!(
        "{:<10} {:>12} {:>14} {:>12} {:>14}",
        "rows", "INSERT calls", "INSERT buffers", "all calls", "all buffers"
    );
    let mut total_write_buffers = 0i64;
    let mut total_all_buffers = 0i64;
    for (rows, write_calls, write_buffers, total_calls, total_buffers) in &tier_results {
        println!(
            "{rows:<10} {write_calls:>12} {write_buffers:>14} {total_calls:>12} {total_buffers:>14}"
        );
        total_write_buffers += write_buffers;
        total_all_buffers += total_buffers;
    }
    println!(
        "-- write_documents INSERT share of buffers across all tiers: {:.1}% ({total_write_buffers} \
         / {total_all_buffers}) --",
        100.0 * total_write_buffers as f64 / total_all_buffers as f64
    );

    // Representative EXPLAIN of the statement `write_documents` now issues
    // (3 literal rows standing in for a batch — EXPLAIN needs concrete
    // values, not the driver's bind placeholders).
    explain(
        &mut conn,
        "write_documents batched upsert (3-row UNION ALL, watermark-guarded shape)",
        "INSERT INTO autumn_search_documents \
           (index_name, record_id, tenant_id, language, fields, content, search_vector, \
            embedding) \
         SELECT 'search_tenant_articles', 1, 'tenant-001', 'english', '{}'::jsonb, 'x', \
                to_tsvector('english', 'x'), ARRAY[0.1,0.2]::double precision[] \
           WHERE NOT EXISTS (SELECT 1 FROM autumn_search_deletes d \
             WHERE d.index_name = 'search_tenant_articles' AND d.record_id = 1 \
               AND d.deleted_at > '2020-01-01'::timestamptz) \
         UNION ALL \
         SELECT 'search_tenant_articles', 2, 'tenant-002', 'english', '{}'::jsonb, 'y', \
                to_tsvector('english', 'y'), ARRAY[0.3,0.4]::double precision[] \
           WHERE NOT EXISTS (SELECT 1 FROM autumn_search_deletes d \
             WHERE d.index_name = 'search_tenant_articles' AND d.record_id = 2 \
               AND d.deleted_at > '2020-01-01'::timestamptz) \
         UNION ALL \
         SELECT 'search_tenant_articles', 3, NULL, 'english', '{}'::jsonb, 'z', \
                to_tsvector('english', 'z'), ARRAY[0.5,0.6]::double precision[] \
           WHERE NOT EXISTS (SELECT 1 FROM autumn_search_deletes d \
             WHERE d.index_name = 'search_tenant_articles' AND d.record_id = 3 \
               AND d.deleted_at > '2020-01-01'::timestamptz) \
         ON CONFLICT (index_name, record_id) DO UPDATE SET \
           tenant_id = EXCLUDED.tenant_id, language = EXCLUDED.language, \
           fields = EXCLUDED.fields, content = EXCLUDED.content, \
           search_vector = EXCLUDED.search_vector, embedding = EXCLUDED.embedding, \
           updated_at = NOW() \
           WHERE autumn_search_documents.updated_at <= '2020-01-01'::timestamptz",
    );

    // Result-equivalence snapshot — printed so it is captured in both the
    // baseline and after artifacts for a byte-diff.
    let dump = dump_documents(&mut conn);
    println!("\n=== document dump ({} rows) ===", dump.len());
    for row in &dump {
        println!("{row:?}");
    }
    assert_eq!(dump.len(), 2_000, "every seeded row must have a document");

    let tenant1 = dump.iter().find(|r| r.record_id == 1).expect("row 1");
    assert_eq!(
        tenant1.tenant_id.as_deref(),
        Some("tenant-001"),
        "deterministic tenant assignment must be unaffected by batching"
    );
    let untenanted = dump.iter().find(|r| r.record_id == 20).expect("row 20");
    assert_eq!(
        untenanted.tenant_id, None,
        "id % 20 == 0 must have no tenant"
    );
    let longtail = dump.iter().find(|r| r.record_id == 4).expect("row 4");
    assert_eq!(
        longtail.tenant_id.as_deref(),
        Some("tenant-longtail-4"),
        "id % 5 == 4 must be a unique long-tail tenant"
    );
}
