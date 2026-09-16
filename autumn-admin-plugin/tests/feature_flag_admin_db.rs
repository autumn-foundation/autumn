//! Postgres-backed integration tests for `FeatureFlagAdminModel` (issue #2108).
//!
//! The companion of `experiment_admin_db.rs`. It covers create, get, list with
//! search and pagination, update, delete, bulk delete, and the History pane —
//! including the rename ancestry the recursive audit CTE follows.
//!
//! `feature_flag_admin_history_reads_utc_under_a_non_utc_session_timezone`
//! guards the timestamp behaviour for issue #2108. `changed_at` is a
//! `timestamptz` column. The row now reads it as the portable `Timestamp`
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

/// Guard the audit timestamp for issue #2108.
///
/// `changed_at` is a `timestamptz` column. The row now reads it as the portable
/// `Timestamp` type, because `SQLite` has no `Timestamptz`. Postgres sends both
/// in the same binary form — microseconds from 2000-01-01 UTC — so the value
/// must not move.
///
/// The pool holds ONE connection, moved off UTC. A text-format read, or a
/// `::timestamp` cast, would return 07:34:56 here.
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

    // Write one audit row at a known UTC instant.
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "INSERT INTO feature_flag_changes (key, mutation, actor, changed_at) \
         VALUES ('tz_probe', 'pinned', NULL, TIMESTAMPTZ '2024-01-15 12:34:56+00')",
    )
    .execute(&mut conn)
    .await
    .expect("insert the pinned audit row");
    drop(conn);

    pg_fixture::pin_session_off_utc(&pool).await;

    let history = pg_fixture::within("get_history", model.get_history(&pool, id, 1, 25))
        .await
        .expect("history");

    // The read has happened. Prove it ran on the non-UTC session, so this test
    // cannot pass by silently landing on a fresh UTC connection.
    pg_fixture::assert_session_off_utc(&pool).await;

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

/// The same guard for `updated_at`, the other flipped `timestamptz` field.
///
/// `get()` and `list()` read it through two different row types. An
/// `ends_with('Z')` check would pass on a value that is five hours wrong, so
/// this pins the instant.
#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn feature_flag_admin_updated_at_reads_utc_under_a_non_utc_session_timezone() {
    use diesel_async::RunQueryDsl;

    let fixture = setup().await;
    let pool = pg_fixture::pool_for(&fixture.url, 1);
    let model = FeatureFlagAdminModel;

    let created = model
        .create(&pool, flag_payload("tz_probe"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "UPDATE autumn_feature_flags SET updated_at = TIMESTAMPTZ '2024-01-15 12:34:56+00' \
         WHERE id = $1",
    )
    .bind::<diesel::sql_types::BigInt, _>(id)
    .execute(&mut conn)
    .await
    .expect("pin updated_at");
    drop(conn);

    pg_fixture::pin_session_off_utc(&pool).await;

    let fetched = pg_fixture::within("get", model.get(&pool, id))
        .await
        .expect("get")
        .expect("record");
    let listed = pg_fixture::within("list", model.list(&pool, page_of(10, None)))
        .await
        .expect("list");
    pg_fixture::assert_session_off_utc(&pool).await;

    assert_eq!(
        fetched["updated_at"], "2024-01-15T12:34:56+00:00",
        "get() must report the pinned instant in UTC"
    );
    assert_eq!(
        listed.records[0]["updated_at"], "2024-01-15T12:34:56+00:00",
        "list() must report the pinned instant in UTC"
    );
}
