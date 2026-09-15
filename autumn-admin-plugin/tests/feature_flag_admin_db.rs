//! Postgres-backed integration tests for `FeatureFlagAdminModel` (issue #2108).
//!
//! The companion of `experiment_admin_db.rs`. It covers create, get, list with
//! search and pagination, update, delete, bulk delete, and the History pane —
//! including the rename ancestry the recursive audit CTE follows.
//!
//! `feature_flag_admin_history_reads_utc_under_a_non_utc_session_timezone` is
//! the timestamp semantics guard for issue #2108: `changed_at` is a
//! `timestamptz` column that the row now reads as the portable `Timestamp`
//! type, so the value must not move with the session time zone.
//!
//! **Requires Docker**, or a Postgres URL in `AUTUMN_ADMIN_TEST_PG_URL`.

use autumn_admin_plugin::feature_flags::FeatureFlagAdminModel;
use autumn_admin_plugin::{AdminModel, ListParams, SortDirection};

#[path = "support/pg_fixture.rs"]
mod pg_fixture;

/// The real migration, included so this fixture cannot drift from the schema
/// the admin model reads and writes.
const CREATE_TABLES_SQL: &str =
    include_str!("../../autumn/migrations/20260530200000_create_feature_flags/up.sql");

/// Start Postgres, create the schema, and return the fixture.
async fn setup() -> pg_fixture::PgFixture {
    pg_fixture::setup(CREATE_TABLES_SQL).await
}

/// Default list parameters for one page of `per_page` records.
fn page_of(per_page: u64, search: Option<&str>) -> ListParams {
    ListParams {
        page: 1,
        per_page,
        search: search.map(str::to_owned),
        sort_by: None,
        sort_dir: SortDirection::default(),
        filters: Vec::new(),
    }
}

/// An enabled flag payload.
fn flag_payload(key: &str) -> serde_json::Value {
    serde_json::json!({
        "key": key,
        "description": format!("{key} description"),
        "enabled": true,
        "rollout_pct": 50,
    })
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_create_round_trips_through_get() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = FeatureFlagAdminModel;

    let created = model
        .create(pool, flag_payload("new_checkout"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");
    assert_eq!(created["key"], "new_checkout");
    assert_eq!(created["enabled"], true);

    let fetched = model.get(pool, id).await.expect("get").expect("record");
    assert_eq!(fetched["key"], "new_checkout");
    assert_eq!(fetched["rollout_pct"], 50);

    let updated_at = fetched["updated_at"].as_str().expect("updated_at");
    assert!(
        updated_at.ends_with('Z') || updated_at.contains("+00:00"),
        "updated_at must be UTC, got {updated_at}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_rejects_a_duplicate_key() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = FeatureFlagAdminModel;

    model
        .create(pool, flag_payload("only_once"))
        .await
        .expect("create");
    let again = model.create(pool, flag_payload("only_once")).await;
    assert!(again.is_err(), "a duplicate key must be refused");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_list_paginates_and_searches() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = FeatureFlagAdminModel;

    for i in 0..3u32 {
        model
            .create(pool, flag_payload(&format!("flag_{i}")))
            .await
            .expect("create");
    }

    let all = model.list(pool, page_of(10, None)).await.expect("list");
    assert_eq!(all.total, 3);
    assert_eq!(all.records.len(), 3);

    let one = model
        .list(pool, page_of(10, Some("flag_1")))
        .await
        .expect("list");
    assert_eq!(one.total, 1);
    assert_eq!(one.records[0]["key"], "flag_1");

    let first_page = model.list(pool, page_of(2, None)).await.expect("list");
    assert_eq!(first_page.total, 3);
    assert_eq!(first_page.records.len(), 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_delete_removes_the_record() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = FeatureFlagAdminModel;

    let created = model
        .create(pool, flag_payload("to_delete"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    model.delete(pool, id).await.expect("delete");
    assert!(model.get(pool, id).await.expect("get").is_none());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_bulk_delete_removes_every_submitted_id() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = FeatureFlagAdminModel;

    let mut ids = Vec::new();
    for i in 0..3u32 {
        let created = model
            .create(pool, flag_payload(&format!("bulk_{i}")))
            .await
            .expect("create");
        ids.push(created["id"].as_i64().expect("id"));
    }
    ids.push(9_999_999);

    let applied = model
        .execute_action(pool, "delete", ids.clone())
        .await
        .expect("bulk delete");
    assert_eq!(
        applied,
        ids.len() as u64,
        "the count reports ids submitted, not rows removed"
    );
    for id in ids {
        assert!(model.get(pool, id).await.expect("get").is_none());
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_history_follows_the_rename_ancestry() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = FeatureFlagAdminModel;

    let created = model
        .create(pool, flag_payload("old_name"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    model
        .update(pool, id, flag_payload("new_name"))
        .await
        .expect("rename");

    let history = model.get_history(pool, id, 1, 25).await.expect("history");
    // 'enabled' at create, then the rename writes 'deleted' for the old key,
    // a 'renamed_from=old_name' breadcrumb, and 'enabled' for the new key.
    assert_eq!(history.total, 4, "the ancestry walk must reach the old key");
    assert!(
        history
            .entries
            .iter()
            .any(|e| e.op == "renamed_from=old_name"),
        "the rename breadcrumb must appear in the history"
    );

    let empty = model
        .get_history(pool, 9_999_999, 1, 25)
        .await
        .expect("history");
    assert_eq!(empty.total, 0);
    assert!(empty.entries.is_empty());
}

/// One row of `SELECT current_setting('TimeZone')`.
#[derive(diesel::QueryableByName)]
struct ZoneRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    zone: String,
}

/// The timestamp semantics guard for issue #2108.
///
/// `changed_at` is a `timestamptz` column. The row now reads it as the portable
/// `Timestamp` type, because `SQLite` has no `Timestamptz`. Postgres sends both
/// types in the same binary form — microseconds from 2000-01-01 UTC — so the
/// value must not move.
///
/// The pool holds ONE connection, and this test sets its session time zone to
/// `America/New_York`. `get_history` therefore reads on a non-UTC session. A
/// text-format read, or a `::timestamp` cast, would return 07:34:56 here. The
/// assertion below demands 12:34:56 UTC.
#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_history_reads_utc_under_a_non_utc_session_timezone() {
    use diesel_async::RunQueryDsl;

    let fixture = setup().await;
    // One connection, so every statement below shares one session.
    let pool = pg_fixture::pool_for(&fixture.url, 1);
    let model = FeatureFlagAdminModel;

    let created = model
        .create(&pool, flag_payload("tz_probe"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    let mut conn = pool.get().await.expect("conn");
    // Write one audit row at a known UTC instant.
    diesel::sql_query(
        "INSERT INTO feature_flag_changes (key, mutation, actor, changed_at) \
         VALUES ('tz_probe', 'pinned', NULL, TIMESTAMPTZ '2024-01-15 12:34:56+00')",
    )
    .execute(&mut conn)
    .await
    .expect("insert the pinned audit row");
    // diesel-async pins each new connection to UTC, so move this one off UTC.
    diesel::sql_query("SET TIME ZONE 'America/New_York'")
        .execute(&mut conn)
        .await
        .expect("set the session time zone");

    let zone = diesel::sql_query("SELECT current_setting('TimeZone') AS zone")
        .get_result::<ZoneRow>(&mut conn)
        .await
        .expect("read the session time zone")
        .zone;
    assert_eq!(zone, "America/New_York", "the session must not be on UTC");
    drop(conn);

    let history = model.get_history(&pool, id, 1, 25).await.expect("history");
    let pinned = history
        .entries
        .iter()
        .find(|e| e.op == "pinned")
        .expect("the pinned audit row");
    assert_eq!(
        pinned.recorded_at.to_rfc3339(),
        "2024-01-15T12:34:56+00:00",
        "the audit timestamp must stay UTC under a non-UTC session time zone"
    );
}
