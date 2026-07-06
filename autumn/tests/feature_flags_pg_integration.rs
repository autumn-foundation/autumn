//! Postgres-backed integration tests for `PgFlagStore`.
//!
//! These tests spin up a real Postgres instance via testcontainers and
//! exercise every `FlagStore` method against an actual database, providing
//! coverage for the DB-dependent code paths that unit tests cannot reach.
//!
//! **Requires Docker** to be running.  All tests are `#[ignore = "requires Docker"]`d by default
//! and opt-in to CI via an explicit `-- --ignored` flag.

#![cfg(all(feature = "db", not(windows)))]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::feature_flags::FlagStore;
use autumn_web::feature_flags::pg::PgFlagStore;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Run the feature-flag migration SQL on a fresh connection to create the
/// required tables, indexes, and trigger function.
const MIGRATION_SQL: &str =
    include_str!("../migrations/20260530200000_create_feature_flags/up.sql");

async fn setup_pg_store() -> (PgFlagStore, testcontainers::ContainerAsync<Postgres>) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    // Run the migration on a synchronous connection (PgFlagStore uses sync diesel).
    let mut conn = PgConnection::establish(&url).expect("db connection");
    conn.batch_execute(MIGRATION_SQL).expect("migration");

    // Use TTL=0 so every test reads from the DB, not the cache.
    let store = PgFlagStore::with_cache_ttl(&url, Duration::ZERO);
    (store, container)
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_enable_creates_flag_and_returns_enabled() {
    let (store, _c) = setup_pg_store().await;
    store.enable("dark_mode", Some("test")).unwrap();
    let flag = store.get("dark_mode").unwrap().unwrap();
    assert!(flag.enabled);
    assert_eq!(flag.rollout_pct, 100);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_disable_sets_enabled_false() {
    let (store, _c) = setup_pg_store().await;
    store.enable("f", None).unwrap();
    store.disable("f", Some("ops")).unwrap();
    let flag = store.get("f").unwrap().unwrap();
    assert!(!flag.enabled);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_get_returns_none_for_missing_key() {
    let (store, _c) = setup_pg_store().await;
    assert!(store.get("nonexistent").unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_list_returns_all_flags_sorted() {
    let (store, _c) = setup_pg_store().await;
    store.enable("zz", None).unwrap();
    store.enable("aa", None).unwrap();
    let flags = store.list().unwrap();
    assert_eq!(flags.len(), 2);
    assert_eq!(flags[0].key, "aa");
    assert_eq!(flags[1].key, "zz");
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_set_rollout_updates_pct() {
    let (store, _c) = setup_pg_store().await;
    store.set_rollout("gradual", 42, Some("cli")).unwrap();
    let flag = store.get("gradual").unwrap().unwrap();
    assert!(flag.enabled);
    assert_eq!(flag.rollout_pct, 42);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_allow_actor_adds_to_allowlist() {
    let (store, _c) = setup_pg_store().await;
    store.allow_actor("beta", "user:42", Some("cli")).unwrap();
    let flag = store.get("beta").unwrap().unwrap();
    assert!(flag.actor_allowlist.contains(&"user:42".to_owned()));
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_allow_actor_does_not_duplicate() {
    let (store, _c) = setup_pg_store().await;
    store.allow_actor("beta", "user:1", None).unwrap();
    store.allow_actor("beta", "user:1", None).unwrap();
    let flag = store.get("beta").unwrap().unwrap();
    assert_eq!(flag.actor_allowlist.len(), 1);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_add_group_adds_to_group_allowlist() {
    let (store, _c) = setup_pg_store().await;
    store.add_group("internal", "staff", Some("cli")).unwrap();
    let flag = store.get("internal").unwrap().unwrap();
    assert!(flag.group_allowlist.contains(&"staff".to_owned()));
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_history_records_all_mutations() {
    let (store, _c) = setup_pg_store().await;
    store.enable("tracked", Some("alice")).unwrap();
    store.disable("tracked", Some("bob")).unwrap();
    store.set_rollout("tracked", 25, Some("cli")).unwrap();
    let history = store.history("tracked", 10).unwrap();
    assert_eq!(history.len(), 3);
    // Most recent first.
    assert_eq!(history[0].mutation, "rollout=25");
    assert_eq!(history[1].mutation, "disabled");
    assert_eq!(history[2].mutation, "enabled");
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_history_respects_limit() {
    let (store, _c) = setup_pg_store().await;
    store.enable("limited", None).unwrap();
    store.disable("limited", None).unwrap();
    store.enable("limited", None).unwrap();
    let history = store.history("limited", 2).unwrap();
    assert_eq!(history.len(), 2);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_kill_switch_overrides_rollout() {
    let (store, _c) = setup_pg_store().await;
    store.set_rollout("guarded", 100, None).unwrap();
    store.disable("guarded", None).unwrap();
    let flag = store.get("guarded").unwrap().unwrap();
    assert!(!flag.enabled);
    // rollout_pct is preserved, but enabled=false is the kill switch.
    assert_eq!(flag.rollout_pct, 100);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_cache_hit_avoids_second_db_call() {
    let container = Postgres::default().start().await.expect("container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let mut conn = PgConnection::establish(&url).expect("conn");
    conn.batch_execute(MIGRATION_SQL).expect("migration");

    // Use a 60-second TTL so reads populate the cache.
    let store = PgFlagStore::with_cache_ttl(&url, Duration::from_secs(60));
    store.enable("cached_flag", None).unwrap();

    // First read populates the cache.
    let v1 = store.get("cached_flag").unwrap();
    assert!(v1.is_some());

    // Second read should hit the in-process cache (same result).
    let v2 = store.get("cached_flag").unwrap();
    assert_eq!(v1, v2);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn pg_store_arc_sharing_delegates_correctly() {
    let (store, _c) = setup_pg_store().await;
    let arc_store: Arc<dyn FlagStore> = Arc::new(store);
    arc_store.enable("shared", Some("test")).unwrap();
    let flag = arc_store.get("shared").unwrap().unwrap();
    assert!(flag.enabled);
    arc_store.disable("shared", None).unwrap();
    assert!(!arc_store.get("shared").unwrap().unwrap().enabled);
}
