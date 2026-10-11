#![cfg(all(feature = "redis", feature = "test-support"))]
//! Issue #3061 AC3: the Redis idempotency store on a current-thread runtime.
//!
//! The store used `block_in_place` on every operation. That panics on a
//! current-thread runtime. The store is now async, so a full store, replay and
//! lock round trip runs here with no panic.
//!
//! **Requires Docker.** CI's `--ignored` sweep runs it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::config::IdempotencyConfig;
use autumn_web::idempotency::{
    IdempotencyLayer, IdempotencyRecord, IdempotencyStore as _, RedisIdempotencyStore,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::redis::Redis as RedisImage;
use tower::ServiceExt as _;

fn post(key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/charge")
        .header("idempotency-key", key)
        .body(Body::empty())
        .expect("request")
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn redis_store_replays_on_a_current_thread_runtime() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let container = RedisImage::default()
        .start()
        .await
        .expect("failed to start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let mut config = IdempotencyConfig::default();
    config.redis.url = Some(format!("redis://127.0.0.1:{port}"));
    let store = Arc::new(RedisIdempotencyStore::from_config(&config).expect("store"));

    let app = axum::Router::new()
        .route(
            "/charge",
            axum::routing::post(|| async {
                CALLS.fetch_add(1, Ordering::SeqCst);
                "charged"
            }),
        )
        .layer(IdempotencyLayer::new(store.clone()));

    // A unique key, so a reused Redis holds no record for it.
    let key = uuid::Uuid::new_v4().to_string();
    let first = app.clone().oneshot(post(&key)).await.expect("infallible");
    assert_eq!(first.status(), StatusCode::OK);
    let second = app.clone().oneshot(post(&key)).await.expect("infallible");
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        second
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);

    // Lock and unlock through the store directly, on the same runtime.
    let ttl = std::time::Duration::from_secs(5);
    let raw = format!("raw-{key}");
    assert!(store.try_lock(&raw, "a", ttl).await.expect("lock"));
    assert!(!store.try_lock(&raw, "b", ttl).await.expect("lock"));
    store.unlock(&raw, "a").await.expect("unlock");
    assert!(store.try_lock(&raw, "b", ttl).await.expect("lock"));

    // Only the holder renews, and its lock fences out another owner's write.
    assert!(!store.renew_lock(&raw, "a", ttl).await.expect("renew"));
    assert!(store.renew_lock(&raw, "b", ttl).await.expect("renew"));
    let record = || IdempotencyRecord {
        status: 201,
        headers: Vec::new(),
        body: b"done".to_vec(),
        metadata: Vec::new(),
    };
    assert!(
        !store
            .set(&raw, "a", record(), Vec::new(), ttl)
            .await
            .expect("set"),
        "b's lock fences out a's write"
    );
    assert!(
        store
            .set(&raw, "b", record(), Vec::new(), ttl)
            .await
            .expect("set")
    );
}
