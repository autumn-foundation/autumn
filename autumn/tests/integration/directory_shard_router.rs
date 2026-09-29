//! §4: `DirectoryShardRouter` — control-plane tenant→shard directory routing.
//!
//! The router consults `_autumn_shard_directory` on the control database and
//! pins listed tenants to named shards, caching results and falling back to
//! the hash router for everyone else. Verifies:
//!
//!   - a directory row routes a tenant to a non-hash shard,
//!   - the result is cached: deleting the row *without* invalidating still
//!     returns the pinned shard (the second route issues no SQL),
//!   - after `invalidate`, the tenant falls back to its hash owner.
//!
//! The control DB requires Docker; the shard pools are never connected to
//! (deadpool builds them lazily), so only one container is needed.
//!
//! Run with:
//!
//!     cargo test --test directory_shard_router -- --include-ignored

#![cfg(feature = "db")]

use std::sync::Arc;

use autumn_web::config::{DatabaseConfig, ShardConfig};
#[cfg(feature = "test-support")]
use autumn_web::sharding::ShardId;
use autumn_web::sharding::{
    DirectoryShardRouter, HashShardRouter, ShardKey, ShardRouter, create_shard_set,
};
#[cfg(feature = "test-support")]
use autumn_web::test::TestDb;
#[cfg(feature = "test-support")]
use diesel_async::RunQueryDsl;

fn two_shard_config() -> DatabaseConfig {
    DatabaseConfig {
        connect_timeout_secs: 1,
        shards: ["shard0", "shard1"]
            .iter()
            .map(|name| ShardConfig {
                name: (*name).to_owned(),
                primary_url: format!("postgres://localhost/{name}"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(feature = "test-support")]
async fn insert_directory_row(db: &TestDb, tenant_key: &str, shard_name: &str) {
    let mut conn = db.pool().get().await.expect("control connection");
    diesel::sql_query(
        "INSERT INTO _autumn_shard_directory (tenant_key, shard_name) VALUES ($1, $2) \
         ON CONFLICT (tenant_key) DO UPDATE SET shard_name = EXCLUDED.shard_name",
    )
    .bind::<diesel::sql_types::Text, _>(tenant_key)
    .bind::<diesel::sql_types::Text, _>(shard_name)
    .execute(&mut *conn)
    .await
    .expect("insert directory row");
}

#[cfg(feature = "test-support")]
async fn delete_directory_row(db: &TestDb, tenant_key: &str) {
    let mut conn = db.pool().get().await.expect("control connection");
    diesel::sql_query("DELETE FROM _autumn_shard_directory WHERE tenant_key = $1")
        .bind::<diesel::sql_types::Text, _>(tenant_key)
        .execute(&mut *conn)
        .await
        .expect("delete directory row");
}

#[cfg(feature = "test-support")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn directory_pins_tenant_then_invalidate_falls_back_to_hash() {
    let db = TestDb::shared().await;
    // The shard-directory migration `up.sql` is a multi-statement script
    // (CREATE TABLE + INDEX + FUNCTION + TRIGGER); apply it through the
    // simple-query protocol (`batch_execute`), matching the production
    // migration runner. The prepared/extended-query path (`execute_sql` ->
    // `sql_query(..).execute(..)`) rejects multi-statement input with "cannot
    // insert multiple commands into a prepared statement".
    {
        use diesel_async::SimpleAsyncConnection;
        let mut conn = db.pool().get().await.expect("control connection");
        conn.batch_execute(include_str!(
            "../../migrations/20260612000000_create_shard_directory/up.sql"
        ))
        .await
        .expect("apply shard_directory migration");
    }

    let shards = create_shard_set(&two_shard_config(), Arc::new(HashShardRouter))
        .expect("lazy shard pools build")
        .expect("two shards configured");

    // Pick a tenant and find its hash owner, then pin it to the *other* shard
    // so a directory hit is observably different from the hash route.
    let tenant = "tenant-directory-test";
    let hash_owner = shards
        .route(tenant)
        .await
        .expect("hash routes the tenant")
        .id();
    let other = ShardId(usize::from(hash_owner.0 == 0));
    let pinned_name = shards
        .get(other)
        .expect("other shard exists")
        .name()
        .to_owned();

    insert_directory_row(db, tenant, &pinned_name).await;

    let router = DirectoryShardRouter::new(db.pool());

    // 1. Directory hit routes to the pinned (non-hash) shard.
    let resolved = router
        .route(ShardKey::Str(tenant), &shards)
        .await
        .expect("directory routes the tenant");
    assert_eq!(
        resolved, other,
        "directory row must override the hash owner"
    );
    assert_ne!(
        resolved, hash_owner,
        "pinned shard differs from the hash owner"
    );

    // 2. Cached: delete the row but do NOT invalidate. The next route must
    //    still return the pinned shard, proving it served from cache (no SQL).
    delete_directory_row(db, tenant).await;
    let cached = router
        .route(ShardKey::Str(tenant), &shards)
        .await
        .expect("cached route");
    assert_eq!(cached, other, "second route must be served from cache");

    // 3. After invalidate, the directory miss falls back to the hash owner.
    router.invalidate(tenant);
    let fallen_back = router
        .route(ShardKey::Str(tenant), &shards)
        .await
        .expect("fallback route");
    assert_eq!(
        fallen_back, hash_owner,
        "after invalidate + deleted row, routing falls back to the hash owner"
    );
}

/// Integer keys are not tenant strings; they bypass the directory table
/// entirely and route through the fallback, so this needs no live database.
#[tokio::test]
async fn non_string_keys_route_through_fallback_without_querying() {
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;

    // A lazy control pool that is never connected to: Int keys short-circuit
    // to the fallback before any control query runs.
    let manager = AsyncDieselConnectionManager::<diesel_async::AsyncPgConnection>::new(
        "postgres://localhost/control-never-connected",
    );
    let control_pool = Pool::builder(manager)
        .build()
        .expect("lazy control pool builds");

    let shards = create_shard_set(&two_shard_config(), Arc::new(HashShardRouter))
        .expect("lazy shard pools build")
        .expect("two shards configured");
    let router = DirectoryShardRouter::new(control_pool);

    let via_directory = router
        .route(ShardKey::Int(42), &shards)
        .await
        .expect("int key routes via fallback");
    let via_hash = HashShardRouter
        .route(ShardKey::Int(42), &shards)
        .await
        .expect("hash routes int key");
    assert_eq!(via_directory, via_hash);
}
