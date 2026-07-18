//! Postgres-backed integration tests for `TokenAdminModel` (issue #1158).
//!
//! Spins up a real Postgres container via testcontainers and exercises every
//! `AdminModel` method on `TokenAdminModel`: create (returns raw token), list
//! (with search/pagination), get, update (name/scopes), and delete (revoke).
//!
//! **Requires Docker** to be running.

use autumn_admin_plugin::tokens::TokenAdminModel;
use autumn_admin_plugin::{AdminModel, ListParams, SortDirection};
use diesel_async::AsyncPgConnection;
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
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");

    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
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
