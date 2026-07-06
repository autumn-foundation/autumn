//! Integration tests for the sharding extractors.
//!
//! These run without a live database: deadpool builds pools lazily, so
//! extractor wiring, key resolution, and rejection paths are all testable
//! with `postgres://localhost/...` URLs that are never connected to.
#![cfg(feature = "db")]

use std::sync::Arc;

use autumn_web::AppState;
use autumn_web::config::{AutumnConfig, DatabaseConfig, ShardConfig};
use autumn_web::sharding::{
    HashShardRouter, ShardKeyOverride, ShardedDb, ShardedReadDb, Shards, create_shard_set,
};
use axum::extract::FromRequestParts;
use axum::http::Request;

fn sharded_config(names: &[&str]) -> DatabaseConfig {
    DatabaseConfig {
        // Keep failing checkouts fast: nothing listens on these URLs.
        connect_timeout_secs: 1,
        shards: names
            .iter()
            .map(|name| ShardConfig {
                name: (*name).to_owned(),
                primary_url: format!("postgres://localhost/{name}"),
                slots: None,
                replica_url: None,
                primary_pool_size: None,
                replica_pool_size: None,
                replica_fallback: None,
            })
            .collect(),
        ..Default::default()
    }
}

fn sharded_state(names: &[&str]) -> AppState {
    let set = create_shard_set(&sharded_config(names), Arc::new(HashShardRouter))
        .expect("lazy pools build without a server")
        .expect("shards configured");
    AppState::for_test().with_shards(set)
}

fn request_parts(uri: &str) -> axum::http::request::Parts {
    let (parts, ()) = Request::builder()
        .uri(uri)
        .body(())
        .expect("request builds")
        .into_parts();
    parts
}

#[tokio::test]
async fn shards_extractor_rejects_when_unconfigured() {
    let state = AppState::for_test();
    let mut parts = request_parts("/");

    let rejection = Shards::from_request_parts(&mut parts, &state)
        .await
        .err()
        .expect("no shards configured should reject");
    let message = rejection.to_string();
    assert!(
        message.contains("database.shards"),
        "rejection should point at the config: {message}"
    );
}

#[tokio::test]
async fn shards_extractor_resolves_and_routes() {
    let state = sharded_state(&["alpha", "beta"]);
    let mut parts = request_parts("/");

    let shards = Shards::from_request_parts(&mut parts, &state)
        .await
        .expect("configured shards extract");
    assert_eq!(shards.set().len(), 2);

    let routed = shards.set().route("tenant-1").await.expect("routes");
    let routed_again = shards.set().route("tenant-1").await.expect("routes");
    assert_eq!(routed.id(), routed_again.id(), "routing is deterministic");
}

#[tokio::test]
async fn sharded_db_without_key_rejects_with_guidance() {
    let state = sharded_state(&["alpha"]);
    // Tenancy is disabled in the default config and no override is set.
    state.insert_extension(AutumnConfig::default());
    let mut parts = request_parts("/");

    let rejection = ShardedDb::from_request_parts(&mut parts, &state)
        .await
        .err()
        .expect("no shard key should reject");
    let message = rejection.to_string();
    assert!(
        message.contains("ShardKeyOverride") && message.contains("tenancy"),
        "rejection should explain both resolution paths: {message}"
    );
}

#[tokio::test]
async fn sharded_db_unconfigured_rejects_before_key_resolution() {
    let state = AppState::for_test();
    let mut parts = request_parts("/");
    parts
        .extensions
        .insert(ShardKeyOverride("tenant-1".to_owned()));

    let rejection = ShardedDb::from_request_parts(&mut parts, &state)
        .await
        .err()
        .expect("no shards configured should reject");
    assert!(rejection.to_string().contains("database.shards"));
}

#[autumn_web::get("/shard-names")]
async fn shard_names(shards: Shards) -> String {
    shards
        .iter()
        .map(|shard| shard.name().to_owned())
        .collect::<Vec<_>>()
        .join(",")
}

#[tokio::test]
async fn test_app_builds_shards_from_config() {
    let config = AutumnConfig {
        database: sharded_config(&["alpha", "beta"]),
        ..Default::default()
    };

    let client = autumn_web::test::TestApp::new()
        .routes(autumn_web::routes![shard_names])
        .config(config)
        .build();

    let response = client.get("/shard-names").send().await;
    response.assert_ok();
    assert_eq!(
        response.text(),
        "alpha,beta",
        "handlers must see the shard set declared in the test config"
    );
}

#[tokio::test]
async fn sharded_db_resolves_key_from_override_then_attempts_checkout() {
    let state = sharded_state(&["alpha"]);
    state.insert_extension(AutumnConfig::default());
    let mut parts = request_parts("/");
    parts
        .extensions
        .insert(ShardKeyOverride("tenant-1".to_owned()));

    // The override resolves the key, routing succeeds, and the failure is
    // the checkout against a nonexistent server — not key-resolution
    // guidance.
    let rejection = ShardedDb::from_request_parts(&mut parts, &state)
        .await
        .err()
        .expect("checkout should fail without a server");
    assert!(
        !rejection.to_string().contains("ShardKeyOverride"),
        "must get past key resolution: {rejection}"
    );
}

#[tokio::test]
async fn sharded_db_resolves_key_from_tenancy_task_local() {
    let state = sharded_state(&["alpha"]);
    state.insert_extension(AutumnConfig::default());

    let rejection = autumn_web::tenancy::with_tenant("tenant-1".to_owned(), async {
        let mut parts = request_parts("/");
        ShardedDb::from_request_parts(&mut parts, &state)
            .await
            .err()
            .expect("checkout should fail without a server")
    })
    .await;
    assert!(
        !rejection.to_string().contains("ShardKeyOverride"),
        "tenant context must resolve the key: {rejection}"
    );
}

#[tokio::test]
async fn sharded_read_db_without_replica_rejects_with_503() {
    // A state whose shard has no replica must reject with a 503 that names
    // the missing replica, not a ShardKeyOverride-guidance message.
    let state = sharded_state(&["alpha"]);
    state.insert_extension(AutumnConfig::default());
    let mut parts = request_parts("/");
    parts
        .extensions
        .insert(ShardKeyOverride("tenant-1".to_owned()));

    let rejection = ShardedReadDb::from_request_parts(&mut parts, &state)
        .await
        .err()
        .expect("no replica → must reject");
    let msg = rejection.to_string();
    assert!(
        msg.contains("replica"),
        "rejection must name the missing replica: {msg}"
    );
    assert!(
        !msg.contains("ShardKeyOverride"),
        "must get past key resolution: {msg}"
    );
}

#[tokio::test]
async fn each_shard_collects_results_in_declaration_order() {
    // Checkouts fail (no real database), but each_shard must still return
    // one entry per shard, in declaration order, with the failure captured
    // per shard rather than short-circuiting the fan-out.
    let state = sharded_state(&["alpha", "beta", "gamma"]);
    let mut parts = request_parts("/");
    let shards = Shards::from_request_parts(&mut parts, &state)
        .await
        .expect("configured shards extract");

    let results = shards
        .each_shard(|shard, _db| {
            let name = shard.name().to_owned();
            async move { Ok(name) }
        })
        .await;

    assert_eq!(results.len(), 3);
    let ids: Vec<usize> = results.iter().map(|(id, _)| id.0).collect();
    assert_eq!(ids, vec![0, 1, 2], "declaration order preserved");
    for (_, result) in results {
        // No Postgres is listening, so every checkout must surface its
        // own error instead of aborting the whole fan-out.
        result.expect_err("checkout should fail without a server");
    }
}
