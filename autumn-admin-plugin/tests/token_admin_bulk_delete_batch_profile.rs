//! Ledger findings/fix harness for `TokenAdminModel::execute_action`'s bulk
//! `"delete"` action (`autumn-admin-plugin/src/tokens.rs`), the default
//! [`AdminModel::execute_action`] trait method (`traits.rs`) drives when a
//! model doesn't override it.
//!
//! Drives the REAL production path: `POST /admin/{slug}/actions`
//! (`autumn-admin-plugin/src/routes.rs`, `model_action`) parses an
//! uncapped, repeated `ids=` form field and calls
//! `model.execute_action(&pool, "delete", ids)` directly — this harness
//! calls `TokenAdminModel::execute_action` the same way, skipping only the
//! HTTP form-decoding step, against a production-shaped `api_tokens` table.
//!
//! Before the fix in this same PR, the trait's default `execute_action`
//! looped over `ids` and called `self.delete(&pool, id)` once per id — a
//! full `pool.get()` + single-row `UPDATE ... WHERE id = $1` round trip per
//! id, with no batching at all (traits.rs:544-576). An operator selecting
//! hundreds of stale/compromised tokens in the admin list and clicking
//! "Delete selected" therefore cost hundreds of statements for what is, on
//! the wire, one predicate.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p autumn-admin-plugin --test token_admin_bulk_delete_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! CI runs it in the Docker-dependent sweep (`-- --ignored`, see CLAUDE.md):
//! this is a plain `#[ignore]`d test compiled into its own binary (this
//! crate has no consolidated `tests/integration/mod.rs`, unlike
//! `autumn`/`autumn-cli`), so the sweep's bare `--ignored` run over the
//! workspace picks it up with no workflow edit.
//!
//! ## Fixture
//!
//! A 50,000-row `api_tokens` table (the real schema from
//! `autumn-admin-plugin/tests/token_admin_db.rs`'s `CREATE_TABLE_SQL`,
//! included verbatim so the fixture can't drift from what the admin UI
//! actually manages): skewed `principal_id` cardinality (80% of rows land on
//! 400 repeat principals — services with many issued tokens — 20% are
//! one-off long-tail principals), 12% pre-revoked (exercises the idempotent
//! `revoked_at IS NULL` no-op branch), 35% NULL `last_used_at` (tokens that
//! were issued but never used), and real dead tuples from a follow-up
//! `UPDATE` before `ANALYZE`.
//!
//! The bulk-delete selection is 2,000 ids — a plausible one-shift "revoke
//! every stale service token" operator action — scattered every 25th id
//! across the table (not a contiguous head block, so this isn't just
//! measuring one sequential index range), deliberately including 200 ids
//! that are already revoked (must no-op, not double-count) and 50 ids past
//! `TOTAL_ROWS` that don't exist at all (must no-op, not error).

// Row/statement counts here top out in the low hundred-thousands, nowhere
// near f64's 52-bit mantissa limit -- every `as f64` below is a display
// ratio, never a value compared against anything, so the lossy cast is
// harmless.
#![allow(clippy::cast_precision_loss)]

use autumn_admin_plugin::AdminModel;
use autumn_admin_plugin::tokens::TokenAdminModel;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Verbatim copy of `token_admin_db.rs`'s `CREATE_TABLE_SQL` — the real
/// `api_tokens` schema the admin plugin manages, so this fixture can't drift
/// from what `TokenAdminModel` actually reads/writes.
const CREATE_TABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS api_tokens (
        id BIGSERIAL PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        principal_id TEXT NOT NULL,
        created_at TIMESTAMP NOT NULL DEFAULT NOW(),
        revoked_at TIMESTAMP,
        name TEXT NOT NULL DEFAULT '',
        scopes JSONB NOT NULL DEFAULT '[]'::jsonb,
        expires_at TIMESTAMP,
        last_used_at TIMESTAMP
    )
";

const TOTAL_ROWS: i64 = 50_000;
/// Every 25th id, `1..=50_000` -> 2,000 ids selected.
const BULK_IDS_STEP: i64 = 25;
/// How many of the selected ids are already revoked before the bulk action
/// runs (must stay no-ops, not get double-counted or re-touched).
const PRE_REVOKED_SELECTED: usize = 200;
/// How many selected ids don't exist in the table at all (past `TOTAL_ROWS`)
/// (must stay no-ops, not errors).
const NONEXISTENT_SELECTED: i64 = 50;

/// Skewed principal cardinality (80/20 across 400 repeat vs. long-tail
/// principals), 12% pre-revoked, 35% NULL `last_used_at`, real dead tuples
/// from a follow-up `UPDATE`+`ANALYZE` — the fixture shape the Ledger
/// process requires (real row counts, real cardinality skew, real NULL
/// density, real dead-tuple ratio).
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO api_tokens \
         (token_hash, principal_id, name, scopes, created_at, revoked_at, expires_at, last_used_at) \
         SELECT \
           'hash_' || gs, \
           'service:' || (CASE WHEN gs % 5 = 0 THEN 100000 + gs ELSE gs % 400 END), \
           'token-' || gs, \
           CASE WHEN gs % 3 = 0 THEN '[\"posts:read\"]'::jsonb ELSE '[\"posts:read\",\"posts:write\"]'::jsonb END, \
           TIMESTAMP '2025-01-01 00:00:00' + (gs || ' minutes')::interval, \
           CASE WHEN gs % 100 < 12 THEN TIMESTAMP '2025-06-01 00:00:00' + (gs || ' minutes')::interval ELSE NULL END, \
           TIMESTAMP '2026-01-01 00:00:00' + (gs || ' days')::interval, \
           CASE WHEN gs % 20 < 7 THEN NULL ELSE TIMESTAMP '2026-06-01 00:00:00' + (gs || ' minutes')::interval END \
         FROM generate_series(1, {TOTAL_ROWS}) AS gs"
    ))
    .expect("seed api_tokens");

    // Real dead tuples: touch a slice of rows post-insert (bump last_used_at
    // on already-revoked rows so it doesn't disturb the 12% revoked share),
    // same technique the other Ledger fixtures use.
    conn.batch_execute(
        "UPDATE api_tokens SET last_used_at = NOW() \
         WHERE id % 11 = 0 AND revoked_at IS NOT NULL",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE api_tokens").expect("analyze");
}

#[derive(QueryableByName, Debug)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

/// 2,000 ids: every 25th id in range (scattered across the table), with
/// `PRE_REVOKED_SELECTED` of them forced to already-revoked and
/// `NONEXISTENT_SELECTED` extra ids past `TOTAL_ROWS` appended.
fn select_bulk_delete_ids(conn: &mut PgConnection) -> Vec<i64> {
    use diesel::RunQueryDsl;
    // Force a known prefix of the scattered selection to already be revoked,
    // so the "must no-op, not double-count" edge case is exercised
    // deterministically rather than depending on which ids the 12%-revoked
    // seed happened to land on.
    conn.batch_execute(&format!(
        "UPDATE api_tokens SET revoked_at = TIMESTAMP '2025-07-01 00:00:00' \
         WHERE id IN (SELECT gs * {BULK_IDS_STEP} FROM generate_series(1, {PRE_REVOKED_SELECTED}) AS gs)"
    ))
    .expect("force pre-revoked subset of the selection");
    conn.batch_execute("ANALYZE api_tokens").expect("analyze");

    let rows = diesel::sql_query(format!(
        "SELECT (gs * {BULK_IDS_STEP})::bigint AS id \
         FROM generate_series(1, {}) AS gs",
        TOTAL_ROWS / BULK_IDS_STEP
    ))
    .load::<IdRow>(conn)
    .expect("select scattered ids");
    let mut ids: Vec<i64> = rows.into_iter().map(|r| r.id).collect();
    ids.extend((TOTAL_ROWS + 1)..=(TOTAL_ROWS + NONEXISTENT_SELECTED));
    ids
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

/// Prints every `api_tokens` revoke statement from this run and returns
/// `(calls, buffers)` for the `SET revoked_at` statement shape specifically
/// (isolated from the read-back queries the harness itself also issues).
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%api_tokens%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut revoke_calls, mut revoke_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.contains("SET revoked_at") {
            revoke_calls += row.calls;
            revoke_buffers += row.buffers;
        }
    }
    println!("-- revoke SET statement: calls={revoke_calls} buffers={revoke_buffers} --");
    (revoke_calls, revoke_buffers)
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

/// Dumps final `(id, revoked_at IS NULL)` for exactly the ids submitted to
/// the bulk action, sorted by id (deterministic, no ties) — the
/// result-equivalence artifact compared byte-for-byte between the baseline
/// (pre-fix) and after (post-fix) commits.
fn dump_revoked_state(conn: &mut PgConnection, ids: &[i64]) -> String {
    use diesel::RunQueryDsl;
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let rows = diesel::sql_query(format!(
        "SELECT id, (revoked_at IS NULL)::text AS revoked_at_is_null \
         FROM api_tokens WHERE id IN ({list}) ORDER BY id"
    ))
    .load::<IdRow2>(conn)
    .expect("dump revoked state");
    rows.into_iter()
        .map(|r| format!("{}:{}", r.id, r.revoked_at_is_null))
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(QueryableByName, Debug)]
struct IdRow2 {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    revoked_at_is_null: String,
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn token_admin_bulk_delete_batch_profile() {
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
    conn.batch_execute(CREATE_TABLE_SQL)
        .expect("create api_tokens");

    seed_fixture(&mut conn);
    let ids = select_bulk_delete_ids(&mut conn);
    let expected_ids_len = ids.len();
    println!(
        "\n-- bulk-delete selection: {expected_ids_len} ids ({PRE_REVOKED_SELECTED} pre-revoked, \
         {NONEXISTENT_SELECTED} nonexistent) --"
    );

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let model = TokenAdminModel;

    reset_stats(&mut conn);
    let applied = model
        .execute_action(&pool, "delete", ids.clone())
        .await
        .expect("bulk delete");

    assert_eq!(
        applied, expected_ids_len as u64,
        "count reflects ids submitted, not rows actually changed \
         (duplicates/no-ops still count as applied, matching the pre-fix loop)"
    );

    let (revoke_calls, revoke_buffers) = print_profile(&mut conn, "bulk delete 2,000 ids");

    println!(
        "\n-- statement-count claim: {expected_ids_len} ids submitted, \
         revoke SET statement calls={revoke_calls} buffers={revoke_buffers} --"
    );

    // The N+1 floor claim, pinned as an assertion: one bulk action now costs
    // exactly one revoke statement, regardless of how many ids were
    // submitted -- not `expected_ids_len` calls, one per id, the way the
    // trait-default loop this replaces would have produced (see
    // docs/reports/2026-08-31-ledger-admin-bulk-delete-batch/baseline/).
    assert_eq!(
        revoke_calls, 1,
        "the batched execute_action must issue exactly one revoke statement \
         for the whole bulk action, not one per id"
    );

    let state_dump = dump_revoked_state(&mut conn, &ids);
    println!("\n=== final revoked-state dump (id:revoked_at_is_null, sorted by id) ===");
    println!("{state_dump}");

    // Every submitted id must end up revoked: the 1,800 freshly-selected
    // ones transition, the 200 forced-pre-revoked ones stay revoked, and the
    // 50 nonexistent ones simply don't appear in the dump at all.
    let still_null = state_dump
        .split(',')
        .filter(|s| s.ends_with(":true"))
        .count();
    assert_eq!(
        still_null, 0,
        "every existing submitted id must be revoked after the bulk action"
    );
    let dumped_rows = state_dump.split(',').filter(|s| !s.is_empty()).count();
    assert_eq!(
        dumped_rows,
        expected_ids_len - usize::try_from(NONEXISTENT_SELECTED).expect("fits in usize"),
        "dump has exactly the existing ids (nonexistent ones are absent, not errored)"
    );

    explain(
        &mut conn,
        "point UPDATE by id (the pre-fix loop's per-id statement shape)",
        "UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' \
         WHERE id = 25 AND revoked_at IS NULL",
    );
}
