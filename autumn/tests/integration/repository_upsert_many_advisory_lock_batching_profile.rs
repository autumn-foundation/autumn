//! Ledger perf harness for the versioned `upsert_many` advisory-lock loop —
//! profiles `#[repository(versioned = true)]`'s generated `upsert_many()`
//! through the real repository entry point, against a production-shaped
//! batch of unique ids.
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep
//! (`-- --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "db,test-support" --test integration_tests \
//!   -- --ignored repository_upsert_many_advisory_lock_batching_profile \
//!   --nocapture --test-threads=1
//! ```
//!
//! ## Workload
//!
//! Every `#[repository(versioned = true)]` model's generated `upsert_many()`
//! takes a Postgres advisory transaction lock (`pg_advisory_xact_lock`) on
//! each id in the batch *before* doing any read or write, so a concurrent
//! `upsert_many` for an overlapping id set can't race the pre-read snapshot
//! `#[version_history]` bookkeeping relies on (`autumn-macros/src/repository.rs`,
//! the `vh_upsert_lock_keys` block feeding the `upsert_many_body` quote).
//! Before this change that was a `for` loop issuing one
//! `SELECT pg_advisory_xact_lock($1)` round trip per unique id, sequentially
//! awaited, inside the same transaction as the rest of the upsert — an N+1
//! independent of how the surrounding read/write chunking (`ids.chunks(1000)`)
//! already batches everything else in this path.
//!
//! `upsert_many` is the batch write path an app author reaches for
//! specifically to avoid N round trips (a CSV import, a reconciliation job,
//! a bulk admin edit) — so a per-id lock loop defeats the exact call site it
//! guards.

#![cfg(feature = "db")]
#![allow(clippy::cast_possible_wrap)] // fixture indices are bounded well under i64::MAX

use autumn_web::hooks::MutationHooks;
use diesel::PgConnection;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// No top-level `diesel::prelude::*` / `diesel_async::RunQueryDsl` glob: this
// file's `#[autumn_web::model]`/`#[autumn_web::repository]`-generated code
// (async) and this harness's own diagnostic `PgConnection` helpers (sync)
// each need a DIFFERENT `RunQueryDsl`, and Rust's ambiguous-trait-method
// resolution does not let a function-local `use` shadow a module-level glob
// that also provides a same-named method (E0034) — so each `RunQueryDsl`
// variant is imported only inside the function that needs it, never at
// module scope, and never both at once.

diesel::table! {
    ledger_upsert_lock_records (id) {
        id -> Int8,
        name -> Text,
        value -> Int4,
    }
}

#[autumn_web::model(table = "ledger_upsert_lock_records")]
#[derive(PartialEq, Eq)]
pub struct LedgerUpsertLockRecord {
    #[id]
    pub id: i64,
    pub name: String,
    pub value: i32,
}

// A no-op hooks impl, matching `HookedVersionedRecord` in
// `repository_bulk_operations.rs` — the proven-working
// `hooks + versioned = true` combination this harness's model mirrors,
// rather than the untested hooks-free + versioned=true corner.
#[derive(Clone, Default)]
pub struct LedgerUpsertLockRecordHooks;

impl MutationHooks for LedgerUpsertLockRecordHooks {
    type Model = LedgerUpsertLockRecord;
    type NewModel = NewLedgerUpsertLockRecord;
    type UpdateModel = UpdateLedgerUpsertLockRecord;
}

#[autumn_web::repository(
    LedgerUpsertLockRecord,
    table = "ledger_upsert_lock_records",
    hooks = LedgerUpsertLockRecordHooks,
    versioned = true
)]
pub trait LedgerUpsertLockRecordRepository {}

/// Three batch sizes standing in for a small reconciliation job, a mid-size
/// CSV import, and a large multi-tenant backfill — the same tier shape
/// `offline_sync_push_batching_perf.rs` and
/// `offline_sync_gc_tombstones_batching_perf.rs` use.
const TIERS: [usize; 3] = [50, 250, 1000];

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    String,
    testcontainers::ContainerAsync<Postgres>,
) {
    use diesel_async::RunQueryDsl as _;

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
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(&mut conn)
        .await
        .expect("create pg_stat_statements extension");
    diesel::sql_query(
        "CREATE TABLE ledger_upsert_lock_records (\
            id BIGINT PRIMARY KEY, name TEXT NOT NULL, value INT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create ledger_upsert_lock_records");
    diesel::sql_query(
        "CREATE TABLE _autumn_version_history (
        id          BIGSERIAL   PRIMARY KEY,
        table_name  TEXT        NOT NULL,
        tenant_id   TEXT,
        record_id   BIGINT      NOT NULL,
        op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
        actor       TEXT        NOT NULL DEFAULT 'system',
        request_id  TEXT,
        changes     JSONB       NOT NULL DEFAULT '[]',
        recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    )",
    )
    .execute(&mut conn)
    .await
    .expect("create _autumn_version_history");

    (pool, url, container)
}

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgLedgerUpsertLockRecordRepository {
    PgLedgerUpsertLockRecordRepository {
        pool,
        hooks: LedgerUpsertLockRecordHooks,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

#[derive(diesel::QueryableByName, Debug)]
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

/// Prints the full profile and returns `(lock_calls, lock_buffers)` — the
/// statement(s) that call `pg_advisory_xact_lock`, isolated from the
/// unchanged read (`ledger_upsert_lock_records` `SELECT ... FOR UPDATE`) and
/// write (`INSERT ... ON CONFLICT`) statements this fix doesn't touch.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl as _;

    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%ledger_upsert_lock_records%' \
            OR query ILIKE '%pg_advisory_xact_lock%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");
    let mut lock_calls = 0i64;
    let mut lock_buffers = 0i64;
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<6} {normalized}",
            row.calls, row.buffers
        );
        if normalized.contains("pg_advisory_xact_lock") {
            lock_calls += row.calls;
            lock_buffers += row.buffers;
        }
    }
    println!("-- advisory-lock statement(s): calls={lock_calls} buffers={lock_buffers} --");
    (lock_calls, lock_buffers)
}

#[derive(diesel::QueryableByName, Debug)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

/// EXPLAIN of the two advisory-lock SHAPES (per-id single-row vs. the batched
/// `unnest ... WITH ORDINALITY` form), independent of which one the code
/// currently issues — same technique as the sibling
/// `offline_sync_gc_tombstones_batching_perf.rs` harness's `explain()`. Not
/// the claim under test (see 📈 Profile): a `pg_advisory_xact_lock` call has
/// no scan to speed up, so this section is illustrative of the PLAN SHAPE,
/// not a buffers claim.
fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
    use diesel::RunQueryDsl as _;

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
#[allow(clippy::too_many_lines)] // one linear profiling script, clearest unsplit
async fn repository_upsert_many_advisory_lock_batching_profile() {
    use diesel::Connection as _;

    let (pool, url, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // A plain (synchronous) `diesel::PgConnection` for pg_stat_statements /
    // EXPLAIN, mirroring the sibling harnesses' diagnostic connection —
    // deadpool's async connection type doesn't implement the sync
    // `diesel::Connection` trait these helpers need.
    let mut diag_conn = PgConnection::establish(&url).expect("diagnostic connection");

    let mut results = Vec::new();
    let mut next_id = 1i64;
    for &n in &TIERS {
        let records: Vec<LedgerUpsertLockRecord> = (0..n)
            .map(|i| LedgerUpsertLockRecord {
                id: next_id + i as i64,
                name: format!("row-{i}"),
                value: i as i32,
            })
            .collect();
        next_id += n as i64 + 1;

        reset_stats(&mut diag_conn);
        let upserted = repo.upsert_many(&records).await.expect("upsert_many");
        assert_eq!(upserted.len(), n, "tier {n}: every new row must upsert");

        let (calls, buffers) =
            print_profile(&mut diag_conn, &format!("upsert_many, {n} new rows"));
        assert!(
            calls > 0,
            "expected at least one advisory-lock statement for a non-empty batch"
        );
        results.push((n, calls, buffers));
    }

    println!("\n=== advisory-lock statement-count scaling ===");
    for (n, calls, buffers) in &results {
        println!("batch size={n:<6} advisory-lock calls={calls:<6} buffers={buffers}");
    }

    explain(
        &mut diag_conn,
        "single-id advisory lock (per-id shape, issued once per unique id today)",
        "SELECT pg_advisory_xact_lock(1234)",
    );
    explain(
        &mut diag_conn,
        "batched unnest advisory lock (20 ids in one statement, the after shape)",
        "SELECT pg_advisory_xact_lock(t.key) \
         FROM unnest(ARRAY[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20]::bigint[]) \
           WITH ORDINALITY AS t(key, ord) \
         ORDER BY t.ord",
    );

    // Result-equivalence: every row landed with the expected content,
    // regardless of which lock-acquisition shape ran.
    let all_rows = repo.find_all().await.expect("load all rows");
    let expected_total: usize = TIERS.iter().sum();
    assert_eq!(all_rows.len(), expected_total);
}
