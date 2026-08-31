//! Postgres-backed integration tests for `TokenAdminModel` (issue #1158).
//!
//! Spins up a real Postgres container via testcontainers and exercises every
//! `AdminModel` method on `TokenAdminModel`: create (returns raw token), list
//! (with search/pagination), get, update (name/scopes), and delete (revoke).
//!
//! **Requires Docker** to be running.

use autumn_admin_plugin::tokens::TokenAdminModel;
use autumn_admin_plugin::{AdminModel, ListParams, SortDirection};
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

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

async fn setup_pool() -> (
    Pool<::autumn_web::RuntimeConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");

    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<::autumn_web::RuntimeConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("DROP TABLE IF EXISTS api_tokens")
        .execute(&mut conn)
        .await
        .expect("drop");
    diesel::sql_query(CREATE_TABLE_SQL)
        .execute(&mut conn)
        .await
        .expect("create");

    (pool, container)
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn token_admin_create_returns_raw_token_and_get_round_trips() {
    let (pool, _container) = setup_pool().await;
    let model = TokenAdminModel;

    let data = serde_json::json!({
        "principal_id": "service:ci",
        "name": "ci-token",
        "scopes": "[\"posts:read\",\"posts:write\"]",
        "expires_at": ""
    });

    let created = model.create(&pool, data).await.unwrap();

    // Raw token must be present in the create response (shown once).
    let raw_token = created
        .get("token")
        .and_then(|v| v.as_str())
        .expect("token field");
    assert!(!raw_token.is_empty(), "raw token must not be empty");

    // The stored row must not expose the hash — only metadata.
    assert!(created.get("token_hash").is_none());

    let id = created["id"].as_i64().expect("id");

    // get() round-trips the metadata (no token field on subsequent reads).
    let fetched = model.get(&pool, id).await.unwrap().expect("record");
    assert_eq!(fetched["name"], "ci-token");
    assert_eq!(fetched["principal_id"], "service:ci");
    assert!(
        fetched.get("token").is_none(),
        "raw token must not reappear"
    );

    // Scopes are parsed back from JSONB.
    let scopes = fetched["scopes"].as_array().expect("scopes array");
    assert!(scopes.iter().any(|s| s == "posts:read"));
    assert!(scopes.iter().any(|s| s == "posts:write"));
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn token_admin_get_returns_none_for_unknown_id() {
    let (pool, _container) = setup_pool().await;
    let model = TokenAdminModel;
    let result = model.get(&pool, 9_999_999).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn token_admin_list_paginates_and_searches() {
    let (pool, _container) = setup_pool().await;
    let model = TokenAdminModel;

    for i in 0..3u32 {
        model
            .create(
                &pool,
                serde_json::json!({
                    "principal_id": format!("service:{i}"),
                    "name": format!("token-{i}"),
                    "scopes": "[]",
                }),
            )
            .await
            .unwrap();
    }

    // List all — should see 3 records.
    let result = model
        .list(
            &pool,
            ListParams {
                page: 1,
                per_page: 10,
                search: None,
                sort_by: None,
                sort_dir: SortDirection::default(),
                filters: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.total, 3);
    assert_eq!(result.records.len(), 3);

    // Search by name prefix — "token-1" matches one record.
    let result = model
        .list(
            &pool,
            ListParams {
                page: 1,
                per_page: 10,
                search: Some("token-1".into()),
                sort_by: None,
                sort_dir: SortDirection::default(),
                filters: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.total, 1);
    assert_eq!(result.records[0]["name"], "token-1");

    // Search by principal.
    let result = model
        .list(
            &pool,
            ListParams {
                page: 1,
                per_page: 10,
                search: Some("service:2".into()),
                sort_by: None,
                sort_dir: SortDirection::default(),
                filters: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.total, 1);

    // Pagination: page 1 of size 2 returns 2, total is still 3.
    let result = model
        .list(
            &pool,
            ListParams {
                page: 1,
                per_page: 2,
                search: None,
                sort_by: None,
                sort_dir: SortDirection::default(),
                filters: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.total, 3);
    assert_eq!(result.records.len(), 2);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn token_admin_update_changes_name_and_scopes() {
    let (pool, _container) = setup_pool().await;
    let model = TokenAdminModel;

    let created = model
        .create(
            &pool,
            serde_json::json!({
                "principal_id": "service:ci",
                "name": "original",
                "scopes": "[\"posts:read\"]",
            }),
        )
        .await
        .unwrap();
    let id = created["id"].as_i64().expect("id");

    let updated = model
        .update(
            &pool,
            id,
            serde_json::json!({
                "name": "updated",
                "scopes": "[\"posts:read\",\"posts:write\"]",
            }),
        )
        .await
        .unwrap();

    assert_eq!(updated["name"], "updated");
    let scopes = updated["scopes"].as_array().expect("scopes");
    assert_eq!(scopes.len(), 2);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn token_admin_delete_revokes_token() {
    let (pool, _container) = setup_pool().await;
    let model = TokenAdminModel;

    let created = model
        .create(
            &pool,
            serde_json::json!({
                "principal_id": "service:ci",
                "name": "to-revoke",
                "scopes": "[]",
            }),
        )
        .await
        .unwrap();
    let id = created["id"].as_i64().expect("id");

    // Before delete: revoked_at is null.
    let before = model.get(&pool, id).await.unwrap().unwrap();
    assert!(before["revoked_at"].is_null());

    // Delete (= revoke).
    model.delete(&pool, id).await.unwrap();

    // After delete: revoked_at is set.
    let after = model.get(&pool, id).await.unwrap().unwrap();
    assert!(!after["revoked_at"].is_null(), "revoked_at must be set");

    // Idempotent — second delete is a no-op, not an error.
    model.delete(&pool, id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn token_admin_create_requires_principal_id() {
    let (pool, _container) = setup_pool().await;
    let model = TokenAdminModel;

    let err = model
        .create(&pool, serde_json::json!({"name": "x", "scopes": "[]"}))
        .await
        .unwrap_err();
    // Missing principal_id → Validation error.
    assert!(
        matches!(err, autumn_admin_plugin::AdminError::Validation(_)),
        "expected Validation error, got: {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn token_admin_create_accepts_rfc3339_expires_at() {
    let (pool, _container) = setup_pool().await;
    let model = TokenAdminModel;

    let created = model
        .create(
            &pool,
            serde_json::json!({
                "principal_id": "service:ci",
                "name": "expiring",
                "scopes": "[]",
                "expires_at": "2030-01-01T00:00:00Z",
            }),
        )
        .await
        .unwrap();

    let id = created["id"].as_i64().expect("id");
    let fetched = model.get(&pool, id).await.unwrap().unwrap();
    assert!(!fetched["expires_at"].is_null(), "expires_at must be set");
}

/// Ledger: `AdminModel::execute_action`'s default `"delete"` branch
/// (`autumn-admin-plugin/src/traits.rs`) loops over the submitted ids and
/// calls `self.delete(id)` once per id. For `TokenAdminModel` that is one
/// `UPDATE api_tokens SET revoked_at = ... WHERE id = $1 AND revoked_at IS
/// NULL` **per token**. `POST /admin/tokens/actions` with `action=delete`
/// and a batch of selected rows is the real, public entry point that drives
/// this loop directly — an operator revoking a batch of tokens (e.g.
/// incident response: revoke every token for a compromised principal).
///
/// This module puts a measured number on that N+1 via `pg_stat_statements`
/// against a production-shaped fixture. See
/// `docs/reports/2026-08-30-ledger-token-admin-bulk-delete-batch/README.md`.
mod bulk_delete_profile {
    use super::{AdminModel, AsyncDieselConnectionManager, Pool, TokenAdminModel};
    use diesel::connection::SimpleConnection;
    use diesel::sql_types::{Array, BigInt, Text};
    use diesel::{Connection, PgConnection, QueryableByName};
    use testcontainers::ImageExt;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    const PROFILE_TOTAL_TOKENS: i64 = 20_000;
    const TIERS: [(&str, i64); 3] = [("small", 100), ("medium", 500), ("large", 2_000)];

    async fn setup_profiling_env() -> (
        PgConnection,
        Pool<::autumn_web::RuntimeConnection>,
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

        let mut sync_conn = PgConnection::establish(&url).expect("sync db connection");
        sync_conn
            .batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
            .expect("create pg_stat_statements extension");
        sync_conn
            .batch_execute(super::CREATE_TABLE_SQL)
            .expect("create api_tokens");
        seed_fixture(&mut sync_conn);

        let manager = AsyncDieselConnectionManager::<::autumn_web::RuntimeConnection>::new(&url);
        let pool = Pool::builder(manager).max_size(10).build().expect("pool");

        (sync_conn, pool, container)
    }

    /// Production-shaped `api_tokens` fixture: 20,000 rows. Principal
    /// cardinality is skewed — 400 rows (2%) belong to 20 heavy-churn
    /// service accounts (~20 tokens each, the pattern of a rotated
    /// service-account credential), the remaining 19,600 spread across
    /// 15,000 `user:*` principals (mostly 1, some 2-3 — the real long tail).
    /// 30% of rows are already revoked (a real token store's history, not a
    /// freshly-seeded all-active table) and `scopes` varies from a 1-element
    /// to a 5-element JSON array. A follow-up `UPDATE` before `ANALYZE`
    /// leaves real dead tuples, same technique the other Ledger harnesses in
    /// this repo use.
    fn seed_fixture(conn: &mut PgConnection) {
        conn.batch_execute(&format!(
            "INSERT INTO api_tokens \
             (token_hash, principal_id, created_at, revoked_at, name, scopes) \
             SELECT \
               'hash_' || i, \
               CASE WHEN i % 50 = 0 THEN 'service:' || ((i / 50) % 20) \
                    ELSE 'user:' || (i % 15000) END, \
               TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval, \
               CASE WHEN i % 10 < 3 \
                    THEN TIMESTAMP '2024-01-01 00:00:00' + ((i + 5000) || ' minutes')::interval \
                    ELSE NULL END, \
               'token-' || i, \
               (CASE i % 5 \
                  WHEN 0 THEN '[\"read\"]' \
                  WHEN 1 THEN '[\"read\",\"write\"]' \
                  WHEN 2 THEN '[\"read\",\"write\",\"admin\"]' \
                  WHEN 3 THEN '[\"read\",\"billing\"]' \
                  ELSE '[\"read\",\"write\",\"admin\",\"billing\",\"deploy\"]' \
                END)::jsonb \
             FROM generate_series(1, {PROFILE_TOTAL_TOKENS}) AS i"
        ))
        .expect("seed api_tokens");

        // Real dead tuples: touch a slice of rows post-insert, same
        // technique the offline-sync and CSV-export Ledger harnesses use.
        conn.batch_execute(
            "UPDATE api_tokens SET last_used_at = created_at + INTERVAL '1 day' \
             WHERE id % 7 = 0",
        )
        .expect("create dead tuples");
        conn.batch_execute("ANALYZE api_tokens").expect("analyze");
    }

    fn reset_stats(conn: &mut PgConnection) {
        conn.batch_execute("SELECT pg_stat_statements_reset()")
            .expect("reset pg_stat_statements");
    }

    #[derive(QueryableByName, Debug, Clone, Copy)]
    struct HotUpdateRow {
        #[diesel(sql_type = BigInt)]
        n_tup_upd: i64,
        #[diesel(sql_type = BigInt)]
        n_tup_hot_upd: i64,
    }

    /// `pg_stat_user_tables.n_tup_upd`/`n_tup_hot_upd` for `api_tokens` —
    /// cumulative cluster-wide counters, not reset by
    /// [`reset_stats`]. Diff two snapshots to see how many of a window's
    /// updates were HOT (no index touch, same page) vs. non-HOT.
    fn hot_update_snapshot(conn: &mut PgConnection) -> HotUpdateRow {
        use diesel::RunQueryDsl;
        diesel::sql_query(
            "SELECT n_tup_upd, n_tup_hot_upd FROM pg_stat_user_tables \
             WHERE relname = 'api_tokens'",
        )
        .load::<HotUpdateRow>(conn)
        .expect("query pg_stat_user_tables")
        .remove(0)
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

    /// Every `api_tokens` `UPDATE` statement issued since the last
    /// [`reset_stats`], from `pg_stat_statements`. Returns the summed
    /// `(calls, buffers, wal_bytes)` across every distinct normalized
    /// statement shape (there is exactly one shape per code path here, but
    /// summing is robust regardless).
    fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64) {
        use diesel::RunQueryDsl;
        println!("\n=== pg_stat_statements: {label} ===");
        let rows = diesel::sql_query(
            // `pg_stat_statements.wal_bytes` is `numeric`, not `bigint` (it
            // has to outrun a lifetime of WAL past i64 range) — cast it down;
            // nowhere near overflow for one test run.
            "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers, \
                    wal_bytes::bigint AS wal_bytes \
             FROM pg_stat_statements \
             WHERE query ILIKE '%api_tokens%' AND query ILIKE '%UPDATE%' \
               AND query NOT ILIKE '%pg_stat_statements%' \
             ORDER BY calls DESC",
        )
        .load::<StatementRow>(conn)
        .expect("query pg_stat_statements");

        let mut total = (0i64, 0i64, 0i64);
        for row in &rows {
            let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
            println!(
                "calls={:<6} buffers={:<8} wal_bytes={:<10} {normalized}",
                row.calls, row.buffers, row.wal_bytes
            );
            total.0 += row.calls;
            total.1 += row.buffers;
            total.2 += row.wal_bytes;
        }
        println!(
            "-- UPDATE statements: calls={} buffers={} wal_bytes={} --",
            total.0, total.1, total.2
        );
        total
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

    #[derive(QueryableByName, Debug)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }

    /// The next `limit` currently-active token ids after `offset` (ordered
    /// by id) — a disjoint slice of the fixture's active pool, mirroring an
    /// operator paging through the admin list view and selecting rows.
    fn active_ids(conn: &mut PgConnection, limit: i64, offset: i64) -> Vec<i64> {
        use diesel::RunQueryDsl;
        diesel::sql_query(
            "SELECT id FROM api_tokens WHERE revoked_at IS NULL \
             ORDER BY id LIMIT $1 OFFSET $2",
        )
        .bind::<BigInt, _>(limit)
        .bind::<BigInt, _>(offset)
        .load::<IdRow>(conn)
        .expect("fetch active ids")
        .into_iter()
        .map(|r| r.id)
        .collect()
    }

    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }

    fn revoked_count(conn: &mut PgConnection, ids: &[i64]) -> i64 {
        use diesel::RunQueryDsl;
        diesel::sql_query(
            "SELECT COUNT(*) AS n FROM api_tokens WHERE id = ANY($1) AND revoked_at IS NOT NULL",
        )
        .bind::<Array<BigInt>, _>(ids.to_vec())
        .load::<CountRow>(conn)
        .expect("count revoked")
        .into_iter()
        .next()
        .map_or(0, |r| r.n)
    }

    fn count_all(conn: &mut PgConnection) -> i64 {
        use diesel::RunQueryDsl;
        diesel::sql_query("SELECT COUNT(*) AS n FROM api_tokens")
            .load::<CountRow>(conn)
            .expect("count all")
            .remove(0)
            .n
    }

    #[derive(QueryableByName)]
    struct TextRow {
        #[diesel(sql_type = Text)]
        v: String,
    }

    fn revoked_at_text(conn: &mut PgConnection, id: i64) -> String {
        use diesel::RunQueryDsl;
        diesel::sql_query("SELECT revoked_at::text AS v FROM api_tokens WHERE id = $1")
            .bind::<BigInt, _>(id)
            .load::<TextRow>(conn)
            .expect("read revoked_at")
            .remove(0)
            .v
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    #[allow(clippy::too_many_lines)]
    async fn token_bulk_delete_batch_profile() {
        let (mut conn, pool, _container) = setup_profiling_env().await;
        let model = TokenAdminModel;

        // ── BEFORE: the current `execute_action` "delete" loop, replicated
        // call-for-call (`self.delete(&pool, id)` per id) — this is exactly
        // what `traits.rs`'s default `execute_action` does today. ──────────
        let mut offset = 0i64;
        let mut baseline_results = Vec::new();
        for (label, size) in TIERS {
            let ids = active_ids(&mut conn, size, offset);
            offset += size;
            assert_eq!(
                i64::try_from(ids.len()).expect("tier size fits in i64"),
                size,
                "tier {label}: fixture must have enough active ids"
            );

            reset_stats(&mut conn);
            let hot_before = hot_update_snapshot(&mut conn);
            for &id in &ids {
                model.delete(&pool, id).await.expect("delete");
            }
            let hot_after = hot_update_snapshot(&mut conn);
            let (calls, buffers, wal_bytes) =
                print_profile(&mut conn, &format!("BEFORE tier={label} ({size} ids)"));
            assert_eq!(
                calls, size,
                "tier {label}: one UPDATE call per id, exactly (the N+1)"
            );
            assert_eq!(
                revoked_count(&mut conn, &ids),
                size,
                "tier {label}: every targeted id must end up revoked"
            );
            let hot = hot_after.n_tup_hot_upd - hot_before.n_tup_hot_upd;
            let non_hot = (hot_after.n_tup_upd - hot_before.n_tup_upd) - hot;
            println!("-- HOT updates: {hot} non-HOT updates: {non_hot} (of {size} total) --");
            baseline_results.push((label, size, calls, buffers, wal_bytes, hot, non_hot));
        }

        println!(
            "\n=== BEFORE: statement-count / buffer / WAL / HOT-update scaling across tiers ==="
        );
        println!(
            "{:<8} {:>8} {:>10} {:>12} {:>12} {:>6} {:>10}",
            "tier", "ids", "calls", "buffers", "wal_bytes", "hot", "non-hot"
        );
        for (label, size, calls, buffers, wal_bytes, hot, non_hot) in &baseline_results {
            println!(
                "{label:<8} {size:>8} {calls:>10} {buffers:>12} {wal_bytes:>12} {hot:>6} {non_hot:>10}"
            );
        }

        // Representative plan for the per-id statement (nonexistent id, so
        // `ANALYZE` doesn't mutate the fixture): shows the access method,
        // not the scale claim — the scale claim is the table above.
        explain(
            &mut conn,
            "single-row revoke UPDATE issued once per id (the pre-fix shape)",
            "UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' \
             WHERE id = 999999999 AND revoked_at IS NULL",
        );

        // ── AFTER: `TokenAdminModel::delete_many` — the fix. Same tier
        // sizes, a disjoint slice of active ids (continuing `offset` so
        // nothing here overlaps a row the BEFORE loop already touched). ──
        let mut after_results = Vec::new();
        for (label, size) in TIERS {
            let ids = active_ids(&mut conn, size, offset);
            offset += size;
            assert_eq!(
                i64::try_from(ids.len()).expect("tier size fits in i64"),
                size,
                "tier {label}: fixture must have enough active ids"
            );

            reset_stats(&mut conn);
            let hot_before = hot_update_snapshot(&mut conn);
            let count = model
                .delete_many(&pool, ids.clone())
                .await
                .expect("delete_many");
            let hot_after = hot_update_snapshot(&mut conn);
            assert_eq!(
                count,
                u64::try_from(size).expect("tier size fits in u64"),
                "tier {label}: delete_many's returned count matches the id count \
                 (same contract the old per-id loop had)"
            );
            let (calls, buffers, wal_bytes) =
                print_profile(&mut conn, &format!("AFTER tier={label} ({size} ids)"));
            assert_eq!(
                calls, 1,
                "tier {label}: one UPDATE call for the whole batch (the fix)"
            );
            assert_eq!(
                revoked_count(&mut conn, &ids),
                size,
                "tier {label}: every targeted id must end up revoked, same as BEFORE"
            );
            let hot = hot_after.n_tup_hot_upd - hot_before.n_tup_hot_upd;
            let non_hot = (hot_after.n_tup_upd - hot_before.n_tup_upd) - hot;
            println!("-- HOT updates: {hot} non-HOT updates: {non_hot} (of {size} total) --");
            after_results.push((label, size, calls, buffers, wal_bytes, hot, non_hot));
        }

        println!(
            "\n=== AFTER: statement-count / buffer / WAL / HOT-update scaling across tiers ==="
        );
        println!(
            "{:<8} {:>8} {:>10} {:>12} {:>12} {:>6} {:>10}",
            "tier", "ids", "calls", "buffers", "wal_bytes", "hot", "non-hot"
        );
        for (label, size, calls, buffers, wal_bytes, hot, non_hot) in &after_results {
            println!(
                "{label:<8} {size:>8} {calls:>10} {buffers:>12} {wal_bytes:>12} {hot:>6} {non_hot:>10}"
            );
        }

        println!("\n=== BEFORE vs AFTER ===");
        println!(
            "{:<8} {:>8} {:>14} {:>14} {:>16} {:>16} {:>14} {:>14} {:>12} {:>12}",
            "tier",
            "ids",
            "calls before",
            "calls after",
            "buffers before",
            "buffers after",
            "wal before",
            "wal after",
            "non-hot before",
            "non-hot after"
        );
        for (
            (label, size, b_calls, b_buffers, b_wal, _b_hot, b_non_hot),
            (_, _, a_calls, a_buffers, a_wal, _a_hot, a_non_hot),
        ) in baseline_results.iter().zip(after_results.iter())
        {
            println!(
                "{label:<8} {size:>8} {b_calls:>14} {a_calls:>14} {b_buffers:>16} \
                 {a_buffers:>16} {b_wal:>14} {a_wal:>14} {b_non_hot:>12} {a_non_hot:>12}"
            );
        }

        // Representative plan for the batched statement — same access
        // method (Index Scan via the primary key, now driven by
        // `= ANY($1)` instead of `= $1`), issued once for the whole batch.
        explain(
            &mut conn,
            "batched revoke UPDATE, 2-element array, both nonexistent \
             (plan shape only — see the 500-id EXPLAIN below for the buffer \
             cost at the scale the tiers above actually measured)",
            "UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' \
             WHERE id = ANY(ARRAY[999999998,999999999]) AND revoked_at IS NULL",
        );

        // The 2-element EXPLAIN above only shows the access method, not the
        // real per-row buffer cost at the scale the tiers were measured at —
        // a small `ANY()` array and a 500-element one can pick different
        // plans. Explain a REAL 500-id batch (fresh, still-active ids) so
        // the reported buffer/WAL shape in the README is read off an actual
        // representative plan, not extrapolated from a 2-row toy.
        let representative_ids = active_ids(&mut conn, 500, offset);
        let array_literal = representative_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        explain(
            &mut conn,
            "batched revoke UPDATE, 500 real active ids (representative scale)",
            &format!(
                "UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' \
                 WHERE id = ANY(ARRAY[{array_literal}]) AND revoked_at IS NULL"
            ),
        );
    }

    /// Result-equivalence edge cases the scale claim above doesn't exercise:
    /// an empty id list, duplicate ids, an already-revoked id, and a
    /// nonexistent id, all in the same call — the exact shapes an admin's
    /// "select all on this page" bulk action can produce. `delete_many` must
    /// match the old per-id loop's behavior on every one of them: never
    /// error, never touch a row twice, and count every id it was asked for
    /// (not just the ones that changed).
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn token_bulk_delete_batch_result_equivalence() {
        let (mut conn, pool, _container) = setup_profiling_env().await;
        let model = TokenAdminModel;

        // Empty list: a no-op, not an error.
        let count = model
            .delete_many(&pool, vec![])
            .await
            .expect("delete_many on an empty list must not error");
        assert_eq!(count, 0);

        let active = active_ids(&mut conn, 2, 0);
        let (active_a, active_b) = (active[0], active[1]);
        let already_revoked = active_ids(&mut conn, 1, 4_600); // untouched by the scale tiers above
        {
            use diesel::RunQueryDsl;
            diesel::sql_query("UPDATE api_tokens SET revoked_at = NOW() WHERE id = $1")
                .bind::<BigInt, _>(already_revoked[0])
                .execute(&mut conn)
                .expect("pre-revoke");
        }
        let before_revoked_at = revoked_at_text(&mut conn, already_revoked[0]);

        let nonexistent = 987_654_321i64;
        // duplicates: `active_a` appears twice.
        let ids = vec![
            active_a,
            active_a,
            active_b,
            already_revoked[0],
            nonexistent,
        ];
        let expected_count = u64::try_from(ids.len()).expect("id count fits in u64");
        let count = model
            .delete_many(&pool, ids.clone())
            .await
            .expect("delete_many with duplicates/already-revoked/nonexistent ids");
        assert_eq!(
            count, expected_count,
            "count is the number of ids asked for, duplicates included — matches the \
             old per-id loop's `count += 1` per iteration"
        );

        assert_eq!(
            revoked_count(&mut conn, &[active_a, active_b]),
            2,
            "both real, previously-active ids must now be revoked"
        );

        let after_revoked_at = revoked_at_text(&mut conn, already_revoked[0]);
        assert_eq!(
            before_revoked_at, after_revoked_at,
            "an already-revoked id must not be touched again (idempotent, same as delete())"
        );

        assert_eq!(
            count_all(&mut conn),
            PROFILE_TOTAL_TOKENS,
            "a nonexistent id in the batch must not insert or otherwise change row count"
        );
    }
}
