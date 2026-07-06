//! Postgres-backed integration tests for scoped service tokens (issue #1158).
//!
//! Spins up a real Postgres instance via testcontainers and exercises
//! `DbApiTokenStore` against the additive `api_tokens` schema: the full
//! issue → list → revoke → rotate lifecycle, SQL-level expiry filtering,
//! `last_used_at` recording, and back-compat with a legacy-shaped row
//! (`name = ''`, `scopes = '[]'`).
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]

use std::sync::Arc;

use autumn_web::auth::{
    ApiTokenStore, DbApiTokenStore, IssueTokenSpec, hash_api_token, issue_scoped_api_token,
    list_api_tokens, rotate_api_token,
};
use autumn_web::time::FixedClock;
use chrono::{Duration as ChronoDuration, TimeZone as _, Utc};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// The managed `api_tokens` schema after the two framework migrations
/// (`create_api_tokens` + `add_scoped_token_columns`).
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

fn scopes(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn db_store_issue_list_revoke_rotate_round_trip() {
    let (pool, _container) = setup_pool().await;
    let store = DbApiTokenStore::new(pool);
    let granted = scopes(&["posts:read", "posts:write"]);

    let raw = issue_scoped_api_token(
        &store,
        IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &granted,
            expires_at: None,
        },
    )
    .await
    .unwrap();

    // verify_scoped round-trips principal + scopes.
    let verified = store.verify_scoped(&raw).await.unwrap().unwrap();
    assert_eq!(verified.principal_id, "service:ci");
    assert_eq!(verified.scopes, granted);

    // list exposes metadata (name/scopes) and no secret.
    let listed = list_api_tokens(&store, "service:ci").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "ci");
    assert_eq!(listed[0].scopes, granted);
    assert!(listed[0].revoked_at.is_none());

    // rotate revokes the old and preserves scopes.
    let rotated = rotate_api_token(&store, &raw).await.unwrap().unwrap();
    assert_ne!(rotated, raw);
    assert!(store.verify_scoped(&raw).await.unwrap().is_none());
    let new_verified = store.verify_scoped(&rotated).await.unwrap().unwrap();
    assert_eq!(new_verified.scopes, granted);

    // revoke the rotated token → no longer verifies.
    store.revoke(&rotated).await.unwrap();
    assert!(store.verify_scoped(&rotated).await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn db_store_filters_expired_tokens_in_sql() {
    let (pool, _container) = setup_pool().await;
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let store = DbApiTokenStore::new(pool).with_clock(Arc::new(FixedClock::at(now)));

    let raw = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:read"]),
            expires_at: Some(now - ChronoDuration::seconds(1)),
        })
        .await
        .unwrap();

    assert!(store.verify(&raw).await.unwrap().is_none());
    assert!(store.verify_scoped(&raw).await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn db_store_records_last_used_at() {
    let (pool, _container) = setup_pool().await;
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let store = DbApiTokenStore::new(pool).with_clock(Arc::new(FixedClock::at(now)));

    let raw = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "service:ci",
            name: "ci",
            scopes: &scopes(&["posts:read"]),
            expires_at: None,
        })
        .await
        .unwrap();

    assert!(
        list_api_tokens(&store, "service:ci").await.unwrap()[0]
            .last_used_at
            .is_none()
    );

    assert!(store.verify_scoped(&raw).await.unwrap().is_some());

    let listed = list_api_tokens(&store, "service:ci").await.unwrap();
    assert_eq!(listed[0].last_used_at, Some(now));
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn db_store_verifies_legacy_shaped_row() {
    // A row written before scopes existed (name = '', scopes = '[]') must still
    // authenticate — proving the additive migration is backward compatible.
    let (pool, _container) = setup_pool().await;
    let raw = "legacy_raw_token_value";
    let hash = hash_api_token(raw);

    let mut conn = pool.get().await.unwrap();
    diesel::sql_query(format!(
        "INSERT INTO api_tokens (token_hash, principal_id) VALUES ('{hash}', 'user:legacy')"
    ))
    .execute(&mut conn)
    .await
    .unwrap();

    let store = DbApiTokenStore::new(pool);
    let verified = store.verify_scoped(raw).await.unwrap().unwrap();
    assert_eq!(verified.principal_id, "user:legacy");
    assert!(verified.scopes.is_empty());
}
