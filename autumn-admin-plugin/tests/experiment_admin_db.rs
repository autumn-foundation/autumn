//! Postgres-backed integration tests for `ExperimentAdminModel` (issue #2108).
//!
//! `token_admin_db.rs` covers `TokenAdminModel` the same way. This binary gives
//! `ExperimentAdminModel` the equivalent cover: create, get, list with search
//! and pagination, update, delete, and the History pane.
//!
//! The History assertions are the important ones. Issue #2108 changes the
//! `changed_at` and `updated_at` row fields. They now use the portable
//! `Timestamp` type, not the Postgres-only `Timestamptz` type, so the crate
//! also compiles against `SQLite`. Postgres sends both types in the same binary
//! form, in UTC microseconds, so the value must not move.
//! `experiment_admin_history_reads_utc_under_a_non_utc_session_timezone`
//! proves it. The test sets the SESSION time zone to `America/New_York`. The
//! timestamp still comes back in UTC.
//!
//! **Requires Docker**, or a Postgres URL in `AUTUMN_ADMIN_TEST_PG_URL`.

use autumn_admin_plugin::experiments::ExperimentAdminModel;
use autumn_admin_plugin::{AdminModel, ListParams, SortDirection};

#[path = "support/pg_fixture.rs"]
mod pg_fixture;

/// The real migration, included so this fixture cannot drift from the schema
/// the admin model reads and writes.
const CREATE_TABLES_SQL: &str =
    include_str!("../../autumn/migrations/20260530300000_create_experiments/up.sql");

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

/// A valid two-variant experiment payload.
fn draft_payload(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": format!("{name} description"),
        "state": "draft",
        "variants": "[{\"name\":\"control\",\"weight\":50},{\"name\":\"treatment\",\"weight\":50}]",
    })
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn experiment_admin_create_round_trips_through_get() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = ExperimentAdminModel;

    let created = model
        .create(pool, draft_payload("checkout_redesign"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");
    assert_eq!(created["name"], "checkout_redesign");
    assert_eq!(created["state"], "draft");

    let fetched = model.get(pool, id).await.expect("get").expect("record");
    assert_eq!(fetched["name"], "checkout_redesign");
    assert_eq!(fetched["description"], "checkout_redesign description");

    // `variants` decodes into a real JSON array, not an opaque string.
    let variants = fetched["variants"].as_array().expect("variants array");
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0]["name"], "control");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn experiment_admin_get_returns_none_for_unknown_id() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = ExperimentAdminModel;
    assert!(model.get(pool, 9_999_999).await.expect("get").is_none());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn experiment_admin_list_paginates_and_searches() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = ExperimentAdminModel;

    for i in 0..3u32 {
        model
            .create(pool, draft_payload(&format!("exp_{i}")))
            .await
            .expect("create");
    }

    let all = model.list(pool, page_of(10, None)).await.expect("list");
    assert_eq!(all.total, 3);
    assert_eq!(all.records.len(), 3);

    let one = model
        .list(pool, page_of(10, Some("exp_1")))
        .await
        .expect("list");
    assert_eq!(one.total, 1);
    assert_eq!(one.records[0]["name"], "exp_1");

    let first_page = model.list(pool, page_of(2, None)).await.expect("list");
    assert_eq!(first_page.total, 3);
    assert_eq!(first_page.records.len(), 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn experiment_admin_update_changes_state_and_winner() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = ExperimentAdminModel;

    let created = model
        .create(pool, draft_payload("pricing_test"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    let updated = model
        .update(
            pool,
            id,
            serde_json::json!({
                "description": "now concluded",
                "state": "concluded",
                "winner": "treatment",
                "variants": "[{\"name\":\"control\",\"weight\":50},{\"name\":\"treatment\",\"weight\":50}]",
            }),
        )
        .await
        .expect("update");
    assert_eq!(updated["state"], "concluded");
    assert_eq!(updated["winner"], "treatment");

    // A winner that is not a configured variant is refused.
    let bad = model
        .update(
            pool,
            id,
            serde_json::json!({
                "state": "concluded",
                "winner": "ghost",
                "variants": "[{\"name\":\"control\",\"weight\":50},{\"name\":\"treatment\",\"weight\":50}]",
            }),
        )
        .await;
    assert!(bad.is_err(), "an unknown winner must be refused");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn experiment_admin_delete_removes_the_record() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = ExperimentAdminModel;

    let created = model
        .create(pool, draft_payload("to_delete"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    model.delete(pool, id).await.expect("delete");
    assert!(model.get(pool, id).await.expect("get").is_none());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers), or a URL in AUTUMN_ADMIN_TEST_PG_URL"]
async fn experiment_admin_bulk_delete_removes_every_submitted_id() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = ExperimentAdminModel;

    let mut ids = Vec::new();
    for i in 0..3u32 {
        let created = model
            .create(pool, draft_payload(&format!("bulk_{i}")))
            .await
            .expect("create");
        ids.push(created["id"].as_i64().expect("id"));
    }
    // An id that does not exist must stay a no-op, not an error.
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
async fn experiment_admin_history_lists_the_audit_trail() {
    let fixture = setup().await;
    let pool = &fixture.pool;
    let model = ExperimentAdminModel;

    let created = model
        .create(pool, draft_payload("audited"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    model
        .update(
            pool,
            id,
            serde_json::json!({
                "description": "second revision",
                "state": "running",
                "variants": "[{\"name\":\"control\",\"weight\":50},{\"name\":\"treatment\",\"weight\":50}]",
            }),
        )
        .await
        .expect("update");

    let history = model.get_history(pool, id, 1, 25).await.expect("history");
    assert_eq!(history.total, 2, "one 'created' and one 'updated' entry");
    assert_eq!(history.entries.len(), 2);
    // Newest first.
    assert_eq!(history.entries[0].op, "updated");
    assert_eq!(history.entries[1].op, "created");

    // An unknown id gives an empty page, not an error.
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
async fn experiment_admin_history_reads_utc_under_a_non_utc_session_timezone() {
    use diesel_async::RunQueryDsl;

    let fixture = setup().await;
    // One connection, so every statement below shares one session.
    let pool = pg_fixture::pool_for(&fixture.url, 1);
    let model = ExperimentAdminModel;

    let created = model
        .create(&pool, draft_payload("tz_probe"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    // Write one audit row at a known UTC instant.
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "INSERT INTO autumn_experiment_changes (experiment, mutation, actor, changed_at) \
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
async fn experiment_admin_updated_at_reads_utc_under_a_non_utc_session_timezone() {
    use diesel_async::RunQueryDsl;

    let fixture = setup().await;
    let pool = pg_fixture::pool_for(&fixture.url, 1);
    let model = ExperimentAdminModel;

    let created = model
        .create(&pool, draft_payload("tz_probe"))
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "UPDATE autumn_experiments SET updated_at = TIMESTAMPTZ '2024-01-15 12:34:56+00' \
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
