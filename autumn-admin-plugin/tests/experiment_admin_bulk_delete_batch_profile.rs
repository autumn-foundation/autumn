//! Ledger findings/fix harness for `ExperimentAdminModel::execute_action`'s
//! bulk `"delete"` action (`autumn-admin-plugin/src/experiments.rs`), the
//! default [`AdminModel::execute_action`] trait method (`traits.rs`) drives
//! when a model doesn't override it.
//!
//! Drives the REAL production path: `POST /admin/{slug}/actions`
//! (`autumn-admin-plugin/src/routes.rs`, `model_action`) parses an
//! uncapped, repeated `ids=` form field and calls
//! `model.execute_action(&pool, "delete", ids)` directly — this harness
//! calls `ExperimentAdminModel::execute_action` the same way, skipping only
//! the HTTP form-decoding step, against a production-shaped
//! `autumn_experiments` table plus its two cascading child tables
//! (`autumn_experiment_assignments`, `autumn_experiment_overrides`).
//!
//! Before the fix in this same PR, the trait's default `execute_action`
//! looped over `ids` and called `self.delete(&pool, id)` once per id — a
//! full `pool.get()` + single-row CTE round trip (delete the experiment,
//! cascade-delete its sticky assignments and staff overrides, write one
//! audit row) per id, with no batching at all (traits.rs). An operator
//! selecting hundreds of concluded/archived experiments in a
//! multi-year-old app's cleanup and clicking "Delete selected" therefore
//! cost hundreds of statements for what is, on the wire, one predicate —
//! exactly the shape already closed for `TokenAdminModel`
//! (`docs/reports/2026-08-31-ledger-admin-bulk-delete-batch/`) and
//! `FeatureFlagAdminModel`
//! (`docs/reports/2026-09-06-ledger-feature-flag-admin-bulk-delete-batch/`);
//! `ExperimentAdminModel` never got the same override.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p autumn-admin-plugin --test experiment_admin_bulk_delete_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! This crate has no consolidated `tests/integration/mod.rs` (unlike
//! `autumn`/`autumn-cli`, see CLAUDE.md) and CI does not run a bare
//! `--ignored` sweep over this package either — this binary needs, and has,
//! an explicit `--test experiment_admin_bulk_delete_batch_profile` line in
//! `.github/workflows/ci.yml`'s "Run Docker-dependent tests" step (and again
//! in the coverage step), right next to the existing `token_admin_*` /
//! `feature_flag_admin_*` lines — a bare sweep would silently never compile
//! or run it.
//!
//! ## Fixture
//!
//! A 3,000-row `autumn_experiments` table (the real schema from
//! `autumn/migrations/20260530300000_create_experiments/up.sql`, included
//! verbatim via `include_str!` so the fixture can't drift from what the
//! admin UI actually manages) — a plausible size for a long-lived app that
//! never prunes concluded/archived experiments. 35% NULL `description`,
//! state skewed toward `concluded` (50%) and `archived` (25%) over
//! `running` (15%) and `draft` (10%) — the long-tail shape of an app that
//! ships new experiments continuously but rarely deletes old ones — with
//! `winner` set on roughly 70% of concluded rows (measured and printed by
//! the harness, not assumed: `gs % 7 < 5` selects within `concluded` using
//! a modulus coprime to the 20-wide state-assignment cycle, so the two
//! conditions land independently rather than the two `CASE` predicates
//! silently correlating through a shared divisor) and NULL elsewhere, and
//! `exclusion_group` NULL for 60% of rows, else one of 15 group names
//! (cardinality skew).
//!
//! Each experiment gets a variable number of sticky assignments
//! (`autumn_experiment_assignments`, 10–170 rows per experiment depending on
//! how wide its rollout was, ~270k rows total) and 0–3 staff overrides
//! (`autumn_experiment_overrides`, ~4.5k rows total) — real cardinality
//! skew, not a uniform fixture. `autumn_experiment_changes` (the audit log
//! the real `delete()` CTE writes to) is pre-seeded with 3 rows per
//! experiment (9,000 rows) — "created" / "state=running" / "concluded" — so
//! the audit table is a realistic size, not empty, when the bulk action
//! adds to it. Real dead tuples come from a follow-up `UPDATE` before
//! `ANALYZE`.
//!
//! The bulk-delete selection is 600 ids — a plausible one-shift "prune
//! every experiment this quarter's cleanup marked dead" operator action —
//! scattered every 5th id across the table (not a contiguous head block),
//! of which 45 are force-deleted *before* the action runs (a previous,
//! narrower cleanup already caught them — must stay a no-op, not error),
//! plus 15 ids past `TOTAL_ROWS` that never existed at all (same
//! requirement). The exact pre-existing count is measured with a
//! `COUNT(*)`, not assumed, per the Ledger process and the review-caught
//! lesson the `TokenAdminModel`/`FeatureFlagAdminModel` harnesses this one
//! mirrors already learned.

#![allow(clippy::cast_precision_loss)]

use autumn_admin_plugin::AdminModel;
use autumn_admin_plugin::experiments::ExperimentAdminModel;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Verbatim copy of the real experiments migration — the admin plugin's
/// actual `autumn_experiments` / `autumn_experiment_assignments` /
/// `autumn_experiment_overrides` / `autumn_experiment_changes` schema (plus
/// the `pg_notify` trigger every audit write already fires), so this
/// fixture can't drift from what `ExperimentAdminModel` actually
/// reads/writes.
const CREATE_TABLES_SQL: &str =
    include_str!("../../autumn/migrations/20260530300000_create_experiments/up.sql");

const TOTAL_ROWS: i64 = 3_000;
/// Every 5th id, `1..=3_000` -> 600 ids selected.
const BULK_IDS_STEP: i64 = 5;
/// How many of the 600 selected ids are force-deleted before the bulk
/// action runs (a narrower cleanup already caught them; must stay no-ops).
const PRE_DELETED_SELECTED: i64 = 45;
/// How many selected ids don't exist in the table at all (past
/// `TOTAL_ROWS`) (must stay no-ops, not errors).
const NONEXISTENT_SELECTED: i64 = 15;

/// Skewed state/exclusion-group cardinality, NULL description/winner
/// density, variable-width assignment/override fan-out per experiment, a
/// pre-seeded audit log (3 rows/experiment), and real dead tuples from a
/// follow-up `UPDATE`+`ANALYZE` — the fixture shape the Ledger process
/// requires (real row counts, real cardinality skew, real NULL density,
/// real dead-tuple ratio).
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO autumn_experiments \
         (name, description, state, variants, winner, exclusion_group, created_at, updated_at) \
         SELECT \
           'exp_' || gs, \
           CASE WHEN gs % 20 < 7 THEN NULL ELSE 'experiment description ' || gs END, \
           (ARRAY['concluded','concluded','concluded','concluded','concluded','concluded','concluded', \
                  'concluded','concluded','concluded','archived','archived','archived','archived', \
                  'archived','running','running','running','draft','draft'])[1 + (gs % 20)]::autumn_experiment_state, \
           '[{{\"name\":\"control\",\"weight\":50}},{{\"name\":\"treatment\",\"weight\":50}}]', \
           CASE WHEN (gs % 20) < 10 AND gs % 7 < 5 \
                THEN (ARRAY['control','treatment'])[1 + (gs % 2)] ELSE NULL END, \
           CASE WHEN gs % 5 < 3 THEN NULL ELSE 'group_' || (gs % 15) END, \
           TIMESTAMP '2023-01-01 00:00:00' + (gs || ' hours')::interval, \
           TIMESTAMP '2023-01-01 00:00:00' + (gs || ' hours')::interval \
         FROM generate_series(1, {TOTAL_ROWS}) AS gs"
    ))
    .expect("seed autumn_experiments");

    // Variable-width sticky assignments per experiment (10-170 rows,
    // depending on how wide the experiment's rollout was) -- real
    // cardinality skew, ~270k rows total. `e.id % 17` (17 is coprime to
    // `BULK_IDS_STEP` = 5): the bulk-delete selection below is every id
    // divisible by 5, so a fanout modulus that shares a factor with 5
    // (the original draft used `% 5`) collapses to a single residue for
    // every selected id and silently measures only the cheapest cascade
    // case -- caught by review, see git history.
    conn.batch_execute(
        "INSERT INTO autumn_experiment_assignments \
         (experiment, actor, variant, is_override, assigned_at) \
         SELECT e.name, 'actor_' || e.id || '_' || a_gs, \
                (ARRAY['control','treatment'])[1 + (a_gs % 2)], FALSE, \
                e.created_at + (a_gs || ' minutes')::interval \
         FROM autumn_experiments e \
         CROSS JOIN LATERAL generate_series(1, 10 + (e.id % 17) * 10) AS a_gs",
    )
    .expect("seed autumn_experiment_assignments");

    // Sparse staff overrides (0-3 per experiment, ~4.5k rows total).
    conn.batch_execute(
        "INSERT INTO autumn_experiment_overrides (experiment, actor, variant, created_at) \
         SELECT e.name, 'staff_' || e.id || '_' || o_gs, 'treatment', e.created_at \
         FROM autumn_experiments e \
         CROSS JOIN LATERAL generate_series(1, e.id % 4) AS o_gs",
    )
    .expect("seed autumn_experiment_overrides");

    // A realistic, non-empty audit trail: 3 change rows per experiment,
    // predating the bulk action this harness measures.
    conn.batch_execute(
        "INSERT INTO autumn_experiment_changes (experiment, mutation, actor, changed_at) \
         SELECT e.name, m.mutation, 'system', e.created_at + (m.ord || ' hours')::interval \
         FROM autumn_experiments e \
         CROSS JOIN LATERAL (VALUES ('created', 1), ('state=running', 2), ('concluded', 3)) AS m(mutation, ord)",
    )
    .expect("seed autumn_experiment_changes");

    // Real dead tuples: touch a slice of rows post-insert, same technique
    // the other Ledger fixtures use.
    conn.batch_execute("UPDATE autumn_experiments SET updated_at = NOW() WHERE id % 7 = 0")
        .expect("create dead tuples");
    conn.batch_execute("ANALYZE autumn_experiments")
        .expect("analyze experiments");
    conn.batch_execute("ANALYZE autumn_experiment_assignments")
        .expect("analyze assignments");
    conn.batch_execute("ANALYZE autumn_experiment_overrides")
        .expect("analyze overrides");
    conn.batch_execute("ANALYZE autumn_experiment_changes")
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
struct NameRow {
    #[diesel(sql_type = Text)]
    name: String,
}

/// 600 ids: every 5th id in range (scattered across the table), of which
/// `PRE_DELETED_SELECTED` are force-deleted up front, plus
/// `NONEXISTENT_SELECTED` extra ids past `TOTAL_ROWS` appended. Returns
/// `(ids, existing_before_action)` — the actual, measured number of the 600
/// that still exist right before the bulk action runs.
fn select_bulk_delete_ids(conn: &mut PgConnection) -> (Vec<i64>, i64) {
    use diesel::RunQueryDsl;

    // Force-delete the first `PRE_DELETED_SELECTED` of the selected ids
    // (a narrower cleanup that already ran) so the bulk action's "id
    // doesn't exist" branch is exercised on a known subset, not just the
    // out-of-range tail. Cascades through assignments/overrides the same
    // way the real `delete()`/`execute_action` path does (a "narrower
    // cleanup" is still the same admin action, just with fewer ids), so it
    // leaves no orphaned child rows behind for the equivalence check below
    // to trip over.
    conn.batch_execute(&format!(
        "WITH targets AS ( \
             SELECT (gs * {BULK_IDS_STEP})::bigint AS id \
             FROM generate_series(1, {PRE_DELETED_SELECTED}) AS gs \
         ), \
         deleted AS ( \
             DELETE FROM autumn_experiments WHERE id IN (SELECT id FROM targets) RETURNING name \
         ), \
         _del_assignments AS ( \
             DELETE FROM autumn_experiment_assignments \
             WHERE experiment IN (SELECT name FROM deleted) \
         ), \
         _del_overrides AS ( \
             DELETE FROM autumn_experiment_overrides \
             WHERE experiment IN (SELECT name FROM deleted) \
         ) \
         INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
         SELECT name, 'deleted', NULL FROM deleted"
    ))
    .expect("force-delete pre-deleted subset of the selection");
    conn.batch_execute("ANALYZE autumn_experiments")
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
        "SELECT COUNT(*) AS n FROM autumn_experiments WHERE id IN ({list})"
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

/// Prints every `autumn_experiments` statement from this run and returns
/// `(calls, buffers)` for the delete-CTE statement shape specifically.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%autumn_experiments%' \
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
        if normalized.contains("DELETE FROM autumn_experiments") {
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
async fn experiment_admin_bulk_delete_batch_profile() {
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
    conn.batch_execute(CREATE_TABLES_SQL).expect(
        "create autumn_experiments / autumn_experiment_assignments / \
         autumn_experiment_overrides / autumn_experiment_changes",
    );

    seed_fixture(&mut conn);

    // Measure the fixture's actual state/winner distribution rather than
    // assume it (the same "measured, not assumed" rule the pre-existing
    // id count below follows) -- `gs % 20` and `gs % 7` are independent
    // moduli, but only a real count proves the two `CASE` predicates in
    // `seed_fixture` land where the doc comment above claims.
    let concluded_count =
        diesel::sql_query("SELECT COUNT(*) AS n FROM autumn_experiments WHERE state = 'concluded'")
            .get_result::<CountRow>(&mut conn)
            .expect("count concluded rows")
            .n;
    let concluded_with_winner = diesel::sql_query(
        "SELECT COUNT(*) AS n FROM autumn_experiments \
         WHERE state = 'concluded' AND winner IS NOT NULL",
    )
    .get_result::<CountRow>(&mut conn)
    .expect("count concluded rows with a winner")
    .n;
    let winner_pct = 100.0 * concluded_with_winner as f64 / concluded_count as f64;
    println!(
        "\n-- fixture: {concluded_count} concluded rows (50% of {TOTAL_ROWS} expected), \
         {concluded_with_winner} with a winner ({winner_pct:.1}%) --"
    );
    assert_eq!(
        concluded_count,
        TOTAL_ROWS / 2,
        "state array must assign exactly 50% of rows to 'concluded'"
    );
    assert!(
        (60.0..=80.0).contains(&winner_pct),
        "winner assignment must land near the documented ~70% of concluded rows, got {winner_pct:.1}%"
    );

    let (ids, existing_before_action) = select_bulk_delete_ids(&mut conn);
    let expected_ids_len = ids.len();
    println!(
        "\n-- bulk-delete selection: {expected_ids_len} ids ({existing_before_action} exist, \
         {PRE_DELETED_SELECTED} pre-deleted, {NONEXISTENT_SELECTED} nonexistent) --"
    );

    // The actual post-fix statement shape (`id = ANY($1)`), explained
    // against the EXACT same array the real action below submits (not a
    // handful of representative ids) and rolled back before that real
    // action runs -- Postgres can pick a different access method as array
    // cardinality/selectivity grows, so only an explain of the real,
    // full-sized submitted array supports the "same plan either side"
    // claim for the actual bulk workload, not a small stand-in array.
    let ids_array_literal = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        explain(
            conn,
            "batched DELETE by id = ANY($1) against the real, full-sized \
             submitted array (the actual post-fix statement execute_action \
             is about to issue), rolled back — diagnostic only",
            &format!(
                "WITH deleted AS ( \
                     DELETE FROM autumn_experiments WHERE id = ANY(ARRAY[{ids_array_literal}]) RETURNING name \
                 ), \
                 _del_assignments AS ( \
                     DELETE FROM autumn_experiment_assignments \
                     WHERE experiment IN (SELECT name FROM deleted) \
                 ), \
                 _del_overrides AS ( \
                     DELETE FROM autumn_experiment_overrides \
                     WHERE experiment IN (SELECT name FROM deleted) \
                 ), \
                 _audit AS ( \
                     INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
                     SELECT name, 'deleted', NULL FROM deleted \
                 ) \
                 SELECT COUNT(*) AS count FROM deleted"
            ),
        );
        Err(diesel::result::Error::RollbackTransaction)
    })
    .ok();

    // Watermarks: only rows written AFTER these points belong to the bulk
    // action being measured, not the fixture's own pre-seeded history.
    let audit_watermark =
        diesel::sql_query("SELECT COALESCE(MAX(id), 0) AS n FROM autumn_experiment_changes")
            .get_result::<CountRow>(&mut conn)
            .expect("audit watermark")
            .n;

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let model = ExperimentAdminModel;

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

    let (delete_calls, delete_buffers) = print_profile(&mut conn, "bulk delete 600 ids");

    println!(
        "\n-- statement-count claim: {expected_ids_len} ids submitted, \
         delete CTE calls={delete_calls} buffers={delete_buffers} --"
    );

    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let still_present = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM autumn_experiments WHERE id IN ({list})"
    ))
    .get_result::<CountRow>(&mut conn)
    .expect("count still-present ids")
    .n;
    assert_eq!(
        still_present, 0,
        "every submitted id (existing or not) must be gone after the bulk action"
    );

    let audit_new_count = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM autumn_experiment_changes \
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

    let deleted_names = diesel::sql_query(format!(
        "SELECT experiment AS name FROM autumn_experiment_changes \
         WHERE id > {audit_watermark} AND mutation = 'deleted' \
         ORDER BY experiment"
    ))
    .load::<NameRow>(&mut conn)
    .expect("list new deleted-audit names")
    .into_iter()
    .map(|r| r.name)
    .collect::<Vec<_>>()
    .join(",");
    println!(
        "\n=== deleted-audit names dump ({audit_new_count} names, sorted) ===\n{deleted_names}"
    );

    // Cascade correctness: every assignment/override row that belonged to a
    // deleted experiment must be gone too — cascading deletes are keyed on
    // `experiment` (the name), not `id`, so this is the one equivalence
    // check the feature-flags harness this mirrors didn't need.
    let orphan_assignments = diesel::sql_query(
        "SELECT COUNT(*) AS n FROM autumn_experiment_assignments a \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM autumn_experiments e WHERE e.name = a.experiment \
         ) AND a.experiment LIKE 'exp_%'",
    )
    .get_result::<CountRow>(&mut conn)
    .expect("count orphaned assignments")
    .n;
    assert_eq!(
        orphan_assignments, 0,
        "cascading assignment delete must remove every assignment for a deleted experiment"
    );
    let orphan_overrides = diesel::sql_query(
        "SELECT COUNT(*) AS n FROM autumn_experiment_overrides o \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM autumn_experiments e WHERE e.name = o.experiment \
         ) AND o.experiment LIKE 'exp_%'",
    )
    .get_result::<CountRow>(&mut conn)
    .expect("count orphaned overrides")
    .n;
    assert_eq!(
        orphan_overrides, 0,
        "cascading override delete must remove every override for a deleted experiment"
    );

    // The N+1 floor claim, pinned as an assertion: one bulk action now costs
    // exactly one delete statement, regardless of how many ids were
    // submitted -- not `expected_ids_len` calls, one per id, the way the
    // trait-default loop this replaces would have produced (see
    // docs/reports/2026-09-07-ledger-experiment-admin-bulk-delete-batch/baseline/).
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
                 DELETE FROM autumn_experiments WHERE id = 2 RETURNING name \
             ), \
             _del_assignments AS ( \
                 DELETE FROM autumn_experiment_assignments \
                 WHERE experiment IN (SELECT name FROM deleted) \
             ), \
             _del_overrides AS ( \
                 DELETE FROM autumn_experiment_overrides \
                 WHERE experiment IN (SELECT name FROM deleted) \
             ), \
             _audit AS ( \
                 INSERT INTO autumn_experiment_changes (experiment, mutation, actor) \
                 SELECT name, 'deleted', NULL FROM deleted \
             ) \
             SELECT COUNT(*) AS count FROM deleted",
        );
        Err(diesel::result::Error::RollbackTransaction)
    })
    .ok();
}
