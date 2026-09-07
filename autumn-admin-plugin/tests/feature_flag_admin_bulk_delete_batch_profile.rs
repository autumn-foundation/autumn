//! Ledger findings/fix harness for `FeatureFlagAdminModel::execute_action`'s
//! bulk `"delete"` action (`autumn-admin-plugin/src/feature_flags.rs`), the
//! default [`AdminModel::execute_action`] trait method (`traits.rs`) drives
//! when a model doesn't override it.
//!
//! Drives the REAL production path: `POST /admin/{slug}/actions`
//! (`autumn-admin-plugin/src/routes.rs`, `model_action`) parses an
//! uncapped, repeated `ids=` form field and calls
//! `model.execute_action(&pool, "delete", ids)` directly — this harness
//! calls `FeatureFlagAdminModel::execute_action` the same way, skipping
//! only the HTTP form-decoding step, against a production-shaped
//! `autumn_feature_flags` table.
//!
//! Before the fix in this same PR, the trait's default `execute_action`
//! looped over `ids` and called `self.delete(&pool, id)` once per id — a
//! full `pool.get()` + single-row `DELETE ... WHERE id = $1` CTE round trip
//! per id, with no batching at all (traits.rs:544-576). An operator
//! selecting hundreds of stale/retired flags in the admin list and clicking
//! "Delete selected" therefore cost hundreds of statements for what is, on
//! the wire, one predicate — exactly the shape the already-fixed
//! `TokenAdminModel` case (`docs/reports/2026-08-31-ledger-admin-bulk-delete-batch/`)
//! closed for `/admin/api-tokens/`; `FeatureFlagAdminModel` never got the
//! same override.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p autumn-admin-plugin --test feature_flag_admin_bulk_delete_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! This crate has no consolidated `tests/integration/mod.rs` (unlike
//! `autumn`/`autumn-cli`, see CLAUDE.md) and CI does not run a bare
//! `--ignored` sweep over this package either — this binary needs, and has,
//! an explicit `--test feature_flag_admin_bulk_delete_batch_profile` line in
//! `.github/workflows/ci.yml`'s "Run Docker-dependent tests" step (and again
//! in the coverage step), right next to the existing `token_admin_*` lines —
//! a bare sweep would silently never compile or run it.
//!
//! ## Fixture
//!
//! A 4,000-row `autumn_feature_flags` table (the real schema from
//! `autumn/migrations/20260530200000_create_feature_flags/up.sql`, included
//! verbatim via `include_str!` so the fixture can't drift from what the
//! admin UI actually manages) — a plausible size for a long-lived,
//! many-team app that never prunes retired experiment flags. Keys are
//! spread across 12 team namespaces, 40% NULL `description` (many flags are
//! never documented), 15% still `enabled` (most accumulate as stale/retired
//! long-tail), rollout percentages cycling through the same six values the
//! admin UI's own `Select` field offers, and real dead tuples from a
//! follow-up `UPDATE` before `ANALYZE`. `feature_flag_changes` (the audit
//! log the real `delete()` CTE writes to) is pre-seeded with 3 rows per
//! flag (12,000 rows) — "created" / "enabled" / "rollout=25" — so the audit
//! table is a realistic size, not empty, when the bulk action adds to it.
//!
//! The bulk-delete selection is 800 ids — a plausible one-shift "prune
//! every flag this quarter's cleanup marked dead" operator action —
//! scattered every 5th id across the table (not a contiguous head block),
//! of which 60 are force-deleted *before* the action runs (a previous,
//! narrower cleanup already caught them — must stay a no-op, not error),
//! plus 20 ids past `TOTAL_ROWS` that never existed at all (same
//! requirement). The exact pre-existing count is measured with a
//! `COUNT(*)`, not assumed, per the Ledger process and the review-caught
//! lesson in the `TokenAdminModel` harness this one mirrors.

#![allow(clippy::cast_precision_loss)]

use autumn_admin_plugin::AdminModel;
use autumn_admin_plugin::feature_flags::FeatureFlagAdminModel;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Verbatim copy of the real feature-flags migration — the admin plugin's
/// actual `autumn_feature_flags` / `feature_flag_changes` schema (plus the
/// `pg_notify` trigger every write already fires), so this fixture can't
/// drift from what `FeatureFlagAdminModel` actually reads/writes.
const CREATE_TABLES_SQL: &str =
    include_str!("../../autumn/migrations/20260530200000_create_feature_flags/up.sql");

const TOTAL_ROWS: i64 = 4_000;
/// Every 5th id, `1..=4_000` -> 800 ids selected.
const BULK_IDS_STEP: i64 = 5;
/// How many of the 800 selected ids are force-deleted before the bulk
/// action runs (a narrower cleanup already caught them; must stay no-ops).
const PRE_DELETED_SELECTED: i64 = 60;
/// How many selected ids don't exist in the table at all (past
/// `TOTAL_ROWS`) (must stay no-ops, not errors).
const NONEXISTENT_SELECTED: i64 = 20;

/// Skewed team-namespace cardinality (12 teams), 40% NULL `description`,
/// 15% `enabled`, a realistic rollout-pct spread, a pre-seeded audit log (3
/// rows/flag), and real dead tuples from a follow-up `UPDATE`+`ANALYZE` —
/// the fixture shape the Ledger process requires (real row counts, real
/// cardinality skew, real NULL density, real dead-tuple ratio).
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO autumn_feature_flags \
         (key, description, enabled, rollout_pct, actor_allowlist, group_allowlist, created_at, updated_at) \
         SELECT \
           'team' || (gs % 12) || '.flag_' || gs, \
           CASE WHEN gs % 10 < 4 THEN NULL ELSE 'flag description ' || gs END, \
           (gs % 100 < 15), \
           (ARRAY[0,10,25,50,75,100])[1 + (gs % 6)], \
           '[]', '[]', \
           TIMESTAMP '2024-01-01 00:00:00' + (gs || ' hours')::interval, \
           TIMESTAMP '2024-01-01 00:00:00' + (gs || ' hours')::interval \
         FROM generate_series(1, {TOTAL_ROWS}) AS gs"
    ))
    .expect("seed autumn_feature_flags");

    // A realistic, non-empty audit trail: 3 change rows per flag, predating
    // the bulk action this harness measures.
    conn.batch_execute(
        "INSERT INTO feature_flag_changes (key, mutation, actor, changed_at) \
         SELECT f.key, m.mutation, 'system', f.created_at + (m.ord || ' hours')::interval \
         FROM autumn_feature_flags f \
         CROSS JOIN LATERAL (VALUES ('created', 1), ('enabled', 2), ('rollout=25', 3)) AS m(mutation, ord)",
    )
    .expect("seed feature_flag_changes");

    // Real dead tuples: touch a slice of rows post-insert, same technique
    // the other Ledger fixtures use.
    conn.batch_execute("UPDATE autumn_feature_flags SET updated_at = NOW() WHERE id % 7 = 0")
        .expect("create dead tuples");
    conn.batch_execute("ANALYZE autumn_feature_flags")
        .expect("analyze flags");
    conn.batch_execute("ANALYZE feature_flag_changes")
        .expect("analyze changes");
}

#[derive(QueryableByName, Debug)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName, Debug)]
struct KeyRow {
    #[diesel(sql_type = Text)]
    key: String,
}

/// 800 ids: every 5th id in range (scattered across the table), of which
/// `PRE_DELETED_SELECTED` are force-deleted up front, plus
/// `NONEXISTENT_SELECTED` extra ids past `TOTAL_ROWS` appended. Returns
/// `(ids, existing_before_action)` — the actual, measured number of the 800
/// that still exist right before the bulk action runs.
fn select_bulk_delete_ids(conn: &mut PgConnection) -> (Vec<i64>, i64) {
    use diesel::RunQueryDsl;

    // Force-delete the first `PRE_DELETED_SELECTED` of the selected ids
    // (a narrower cleanup that already ran) so the bulk action's "id
    // doesn't exist" branch is exercised on a known subset, not just the
    // out-of-range tail.
    conn.batch_execute(&format!(
        "DELETE FROM autumn_feature_flags WHERE id IN ( \
           SELECT (gs * {BULK_IDS_STEP})::bigint \
           FROM generate_series(1, {PRE_DELETED_SELECTED}) AS gs \
         )"
    ))
    .expect("force-delete pre-deleted subset of the selection");
    conn.batch_execute("ANALYZE autumn_feature_flags")
        .expect("analyze after pre-delete");

    let rows = diesel::sql_query(format!(
        "SELECT (gs * {BULK_IDS_STEP})::bigint AS id \
         FROM generate_series(1, {}) AS gs",
        TOTAL_ROWS / BULK_IDS_STEP
    ))
    .load::<IdRow>(conn)
    .expect("select scattered ids");
    let mut ids: Vec<i64> = rows.into_iter().map(|r| r.id).collect();

    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let existing_before_action = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM autumn_feature_flags WHERE id IN ({list})"
    ))
    .get_result::<CountRow>(conn)
    .expect("count pre-existing ids")
    .n;

    ids.extend((TOTAL_ROWS + 1)..=(TOTAL_ROWS + NONEXISTENT_SELECTED));
    (ids, existing_before_action)
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

/// Prints every `autumn_feature_flags` statement from this run and returns
/// `(calls, buffers)` for the delete-CTE statement shape specifically.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%autumn_feature_flags%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut delete_calls, mut delete_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.contains("DELETE FROM autumn_feature_flags") {
            delete_calls += row.calls;
            delete_buffers += row.buffers;
        }
    }
    println!("-- delete CTE statement: calls={delete_calls} buffers={delete_buffers} --");
    (delete_calls, delete_buffers)
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
async fn feature_flag_admin_bulk_delete_batch_profile() {
    use diesel::RunQueryDsl;

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
    conn.batch_execute(CREATE_TABLES_SQL)
        .expect("create autumn_feature_flags / feature_flag_changes");

    seed_fixture(&mut conn);
    let (ids, existing_before_action) = select_bulk_delete_ids(&mut conn);
    let expected_ids_len = ids.len();
    println!(
        "\n-- bulk-delete selection: {expected_ids_len} ids ({existing_before_action} exist, \
         {PRE_DELETED_SELECTED} pre-deleted, {NONEXISTENT_SELECTED} nonexistent) --"
    );

    // Watermark: only audit rows written AFTER this point belong to the
    // bulk action being measured, not the fixture's own pre-seeded history.
    let audit_watermark =
        diesel::sql_query("SELECT COALESCE(MAX(id), 0) AS n FROM feature_flag_changes")
            .get_result::<CountRow>(&mut conn)
            .expect("audit watermark")
            .n;

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let model = FeatureFlagAdminModel;

    reset_stats(&mut conn);
    let applied = model
        .execute_action(&pool, "delete", ids.clone())
        .await
        .expect("bulk delete");

    assert_eq!(
        applied, expected_ids_len as u64,
        "count reflects ids submitted, not rows actually deleted \
         (already-missing ids still count as applied, matching the pre-fix loop)"
    );

    let (delete_calls, delete_buffers) = print_profile(&mut conn, "bulk delete 800 ids");

    println!(
        "\n-- statement-count claim: {expected_ids_len} ids submitted, \
         delete CTE calls={delete_calls} buffers={delete_buffers} --"
    );

    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let still_present = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM autumn_feature_flags WHERE id IN ({list})"
    ))
    .get_result::<CountRow>(&mut conn)
    .expect("count still-present ids")
    .n;
    assert_eq!(
        still_present, 0,
        "every submitted id (existing or not) must be gone after the bulk action"
    );

    let audit_new_count = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM feature_flag_changes \
         WHERE id > {audit_watermark} AND mutation = 'deleted'"
    ))
    .get_result::<CountRow>(&mut conn)
    .expect("count new deleted-audit rows")
    .n;
    assert_eq!(
        audit_new_count, existing_before_action,
        "exactly one 'deleted' audit row per id that actually existed, \
         not one per id submitted"
    );

    let deleted_keys = diesel::sql_query(format!(
        "SELECT key FROM feature_flag_changes \
         WHERE id > {audit_watermark} AND mutation = 'deleted' \
         ORDER BY key"
    ))
    .load::<KeyRow>(&mut conn)
    .expect("list new deleted-audit keys")
    .into_iter()
    .map(|r| r.key)
    .collect::<Vec<_>>()
    .join(",");
    println!("\n=== deleted-audit keys dump ({audit_new_count} keys, sorted) ===\n{deleted_keys}");

    // The N+1 floor claim, pinned as an assertion: one bulk action now costs
    // exactly one delete statement, regardless of how many ids were
    // submitted -- not `expected_ids_len` calls, one per id, the way the
    // trait-default loop this replaces would have produced (see
    // docs/reports/2026-09-06-ledger-feature-flag-admin-bulk-delete-batch/baseline/).
    assert_eq!(
        delete_calls, 1,
        "the batched execute_action must issue exactly one delete statement \
         for the whole bulk action, not one per id"
    );

    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        explain(
            conn,
            "point DELETE by id (the pre-fix loop's per-id statement shape), \
             rolled back — diagnostic only",
            "WITH deleted AS ( \
                 DELETE FROM autumn_feature_flags WHERE id = 2 RETURNING key \
             ), \
             _audit AS ( \
                 INSERT INTO feature_flag_changes (key, mutation, actor) \
                 SELECT key, 'deleted', NULL FROM deleted \
             ) \
             SELECT COUNT(*) AS count FROM deleted",
        );
        Err(diesel::result::Error::RollbackTransaction)
    })
    .ok();
}
