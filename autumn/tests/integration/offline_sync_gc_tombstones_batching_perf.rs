//! Ledger perf harness for `PgSyncBackend::gc_tombstones` — profiles the
//! per-scope tombstone-horizon UPSERT loop through the real
//! `SyncBackend::gc_tombstones` entry point (the exact call a scheduled
//! maintenance task makes), against a production-shaped multi-tenant
//! fixture.
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep
//! (`-- --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "test-support,offline-sync" \
//!   --test integration_tests -- --ignored offline_sync_gc_tombstones_batching_profile \
//!   --nocapture --test-threads=1
//! ```
//!
//! The committed `docs/reports/2026-08-18-ledger-sync-gc-horizon-batch/{baseline,after}`
//! files are this command's stdout, captured before and after the batching
//! change in `autumn/src/sync/server.rs`.
//!
//! ## Fixture
//!
//! A multi-tenant offline-sync deployment: `autumn_sync_rows` seeded across
//! three disjoint scope tiers (50 / 250 / 1000 distinct scopes — the shared
//! sizes used across this suite's harnesses), 20 rows per scope, 30%
//! tombstoned (deterministic: rows 0..5 of each 20-row scope block), the
//! rest live. Each tier occupies a disjoint, contiguous version range so one
//! [`PgSyncBackend::gc_tombstones`] call per tier only ever exposes that
//! tier's tombstones — mirroring the disjoint-slice technique
//! `offline_sync_push_batching_perf.rs` uses for its three batch sizes. One
//! scope (`tenant-000000`) is pre-seeded with an existing horizon HIGHER
//! than anything this sweep would compute, to prove the batched UPSERT still
//! honors `GREATEST` and never lowers a horizon.

#![cfg(feature = "offline-sync")]
#![allow(clippy::cast_possible_wrap)] // fixture indices are bounded well under i64::MAX

use autumn_web::sync::{PgSyncBackend, SyncBackend};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Text};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const SCOPE_PREFIX: &str = "tenant-";
const ROWS_PER_SCOPE: usize = 20;
const TOMBSTONED_PER_SCOPE: usize = 6; // 30% of ROWS_PER_SCOPE, deterministic

/// `(n_scopes, scope_offset)` for the three tiers, sized to disjoint scope
/// ranges so all three tiers can be seeded once and swept in three separate
/// `gc_tombstones` calls without one tier's rows leaking into another's
/// statement count.
const TIERS: [(usize, usize); 3] = [(50, 0), (250, 50), (1000, 300)];

fn scope_name(scope_idx: usize) -> String {
    format!("{SCOPE_PREFIX}{scope_idx:06}")
}

/// Seed all three tiers in one pass. Row `i` (1-based, global) maps to a
/// tier by cumulative row count, then to a scope within that tier by
/// `(i - tier_row_start) / ROWS_PER_SCOPE`, and is tombstoned iff its
/// position within its 20-row scope block is `< TOMBSTONED_PER_SCOPE` —
/// every scope in every tier is guaranteed at least one droppable
/// tombstone, so "distinct scopes touched" is exactly `n_scopes` for each
/// tier's sweep, deterministically (no flaky row-count-dependent skips).
fn seed_fixture(conn: &mut PgConnection) -> Vec<(usize, usize, usize)> {
    // (tier_row_start, n_scopes, scope_offset) plus the running total, used
    // both to build the seed SQL's CASE expression and to hand back each
    // tier's up_to boundary to the caller.
    let mut boundaries = Vec::new();
    let mut row_start = 1usize;
    for (n_scopes, scope_offset) in TIERS {
        let tier_rows = n_scopes * ROWS_PER_SCOPE;
        boundaries.push((row_start, tier_rows, scope_offset));
        row_start += tier_rows;
    }
    let total_rows = row_start - 1;

    let case_scope = boundaries
        .iter()
        .map(|&(start, rows, offset)| {
            let end = start + rows - 1;
            format!(
                "WHEN i BETWEEN {start} AND {end} THEN {offset} + (i - {start}) / {ROWS_PER_SCOPE}"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let case_block_pos = boundaries
        .iter()
        .map(|&(start, rows, _)| {
            let end = start + rows - 1;
            format!("WHEN i BETWEEN {start} AND {end} THEN (i - {start}) % {ROWS_PER_SCOPE}")
        })
        .collect::<Vec<_>>()
        .join(" ");

    conn.batch_execute(&format!(
        "INSERT INTO autumn_sync_rows \
         (scope, collection, pk, payload, version, deleted, updated_at, device_id, created_version) \
         SELECT \
           'tenant-' || lpad((CASE {case_scope} END)::text, 6, '0'), \
           'notes', \
           'row-' || i::text, \
           jsonb_build_object('n', i), \
           i, \
           (CASE {case_block_pos} END) < {TOMBSTONED_PER_SCOPE}, \
           TIMESTAMPTZ '2025-01-01 00:00:00Z' + (i || ' seconds')::interval, \
           'seed-device', \
           i \
         FROM generate_series(1, {total_rows}) AS i"
    ))
    .expect("seed autumn_sync_rows");
    conn.batch_execute(&format!(
        "SELECT setval('autumn_sync_version_seq', {total_rows}, true)"
    ))
    .expect("advance version sequence past seeded rows");

    // GREATEST edge case: pre-seed tenant-000000's horizon above anything
    // tier 1's sweep would compute (tier 1 tombstone versions top out under
    // ROWS_PER_SCOPE * 50 = 1000), so the batched UPSERT must leave it
    // untouched rather than overwriting it with a lower value.
    conn.batch_execute(&format!(
        "INSERT INTO autumn_sync_horizons (scope, horizon) VALUES ('{}', 999999)",
        scope_name(0)
    ))
    .expect("pre-seed a horizon above the sweep's computed values");

    // Real dead tuples before ANALYZE, same as the sibling apply_push
    // harness: touch a slice of rows that are NOT about to be deleted by
    // any tier's sweep (the untombstoned tail of each scope block).
    conn.batch_execute(
        "UPDATE autumn_sync_rows SET device_id = 'seed-device-2' \
         WHERE NOT deleted AND right(pk, 1) IN ('0', '1')",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE autumn_sync_rows, autumn_sync_horizons")
        .expect("analyze");

    boundaries
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
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

/// Prints the profile and returns `(horizon_upsert_calls, horizon_upsert_buffers)`
/// — the statement(s) touching `autumn_sync_horizons`, isolated from the
/// `DELETE ... RETURNING` and advisory-lock statements that are unchanged
/// by this fix and so are not the claim under test.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers, \
                wal_bytes::bigint AS wal_bytes \
         FROM pg_stat_statements \
         WHERE query ILIKE '%autumn_sync%' AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");
    let mut horizon_calls = 0i64;
    let mut horizon_buffers = 0i64;
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<6} wal_bytes={:<8} {normalized}",
            row.calls, row.buffers, row.wal_bytes
        );
        if normalized.contains("autumn_sync_horizons") {
            horizon_calls += row.calls;
            horizon_buffers += row.buffers;
        }
    }
    println!("-- horizon-upsert statement: calls={horizon_calls} buffers={horizon_buffers} --");
    (horizon_calls, horizon_buffers)
}

#[derive(QueryableByName, Debug)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

/// EXPLAIN of the two horizon-upsert SHAPES (per-scope single-row vs. the
/// batched UNNEST form), independent of which one the code currently issues
/// — a raw diagnostic query, same technique
/// `offline_sync_push_batching_perf.rs` uses for its point-lookup EXPLAINs.
/// This section's output is unaffected by the `src/sync/server.rs` change:
/// both shapes are always valid SQL against the fixture's `autumn_sync_horizons`
/// table, so the baseline and after runs render identical EXPLAIN output —
/// the claim under test is the statement COUNT (see 📈 Profile), not a plan
/// shape change (same "same access method reading a different number of
/// rows" story `offline_sync_push_batching_perf.rs`'s sibling report used).
fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
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

#[derive(QueryableByName, Debug, PartialEq, Eq)]
struct HorizonDump {
    #[diesel(sql_type = Text)]
    scope: String,
    #[diesel(sql_type = BigInt)]
    horizon: i64,
}

fn dump_horizons(conn: &mut PgConnection) -> Vec<HorizonDump> {
    diesel::sql_query("SELECT scope, horizon FROM autumn_sync_horizons ORDER BY scope")
        .load::<HorizonDump>(conn)
        .expect("dump horizons")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)] // one linear profiling script, clearest unsplit
async fn offline_sync_gc_tombstones_batching_profile() {
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

    let backend = PgSyncBackend::new(url.clone());
    backend.ensure_schema().expect("ensure schema");

    let mut conn = PgConnection::establish(&url).expect("db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");

    let boundaries = seed_fixture(&mut conn);

    let mut results = Vec::new();
    let mut cumulative_up_to = 0i64;
    for (idx, &(n_scopes, _offset)) in TIERS.iter().enumerate() {
        let (tier_row_start, tier_rows, _) = boundaries[idx];
        cumulative_up_to = (tier_row_start + tier_rows - 1) as i64;

        reset_stats(&mut conn);
        let removed = backend
            .gc_tombstones(cumulative_up_to)
            .expect("gc_tombstones");
        assert_eq!(
            removed as usize,
            n_scopes * TOMBSTONED_PER_SCOPE,
            "tier {idx} (n_scopes={n_scopes}) must drop exactly its own tombstones, no more/less"
        );
        let (calls, buffers) = print_profile(
            &mut conn,
            &format!("gc_tombstones sweep, {n_scopes} scopes"),
        );
        assert!(
            calls > 0,
            "expected at least one horizon-upsert statement for a non-empty sweep"
        );
        results.push((n_scopes, calls, buffers));
    }

    println!("\n=== horizon-upsert statement-count scaling ===");
    for (n_scopes, calls, buffers) in &results {
        println!(
            "distinct scopes touched={n_scopes:<6} horizon-upsert calls={calls:<6} buffers={buffers}"
        );
    }

    // Idempotency: a second sweep at the same up_to must touch zero
    // tombstones (already dropped) and issue zero horizon-upsert calls.
    reset_stats(&mut conn);
    let removed_again = backend
        .gc_tombstones(cumulative_up_to)
        .expect("gc_tombstones replay");
    assert_eq!(removed_again, 0, "replay sweep must drop nothing new");
    let (replay_calls, _) = print_profile(&mut conn, "replay sweep (idempotency, nothing to drop)");
    assert_eq!(
        replay_calls, 0,
        "replay sweep must issue zero horizon-upserts"
    );

    explain(
        &mut conn,
        "single-row horizon UPSERT (per-scope shape, issued once per distinct scope today)",
        "INSERT INTO autumn_sync_horizons (scope, horizon) VALUES ('tenant-000999', 12345) \
         ON CONFLICT (scope) DO UPDATE SET \
         horizon = GREATEST(autumn_sync_horizons.horizon, excluded.horizon)",
    );
    // 20 scopes only, kept small so the EXPLAIN output stays readable — this
    // is illustrative of the PLAN SHAPE (one `Function Scan on t` feeding one
    // `Insert`), not the scale claim: the scale claim (statement count and
    // buffers at 50/250/1000 distinct scopes) is proven above by
    // `pg_stat_statements`, not by this diagnostic EXPLAIN.
    let unnest_scopes = (300..320)
        .map(|i| format!("'tenant-{i:06}'"))
        .collect::<Vec<_>>()
        .join(",");
    let unnest_horizons = (300..320)
        .map(|i| (24000 + i).to_string())
        .collect::<Vec<_>>()
        .join(",");
    explain(
        &mut conn,
        "batched UNNEST horizon UPSERT (20 scopes in one statement, the after shape)",
        &format!(
            "INSERT INTO autumn_sync_horizons (scope, horizon) \
             SELECT * FROM UNNEST(ARRAY[{unnest_scopes}]::TEXT[], ARRAY[{unnest_horizons}]::BIGINT[]) \
               AS t(scope, horizon) \
             ON CONFLICT (scope) DO UPDATE SET \
             horizon = GREATEST(autumn_sync_horizons.horizon, excluded.horizon)"
        ),
    );

    println!("\n=== result-equivalence: final autumn_sync_horizons contents ===");
    let horizons = dump_horizons(&mut conn);
    for row in &horizons {
        println!("{row:?}");
    }
    // GREATEST edge case: the pre-seeded 999999 horizon on tenant-000000
    // must survive untouched even though tier 1's sweep computed a lower
    // value for that scope.
    let tenant_0 = horizons
        .iter()
        .find(|h| h.scope == scope_name(0))
        .expect("tenant-000000 horizon present");
    assert_eq!(
        tenant_0.horizon, 999999,
        "GREATEST must preserve a pre-existing higher horizon"
    );
}
