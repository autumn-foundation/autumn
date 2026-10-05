//! Issue #3061 AC2: a store failure after the handler succeeds no longer locks
//! the key for 24 h under the default settings.
//!
//! The store write fails after the handler runs (a Redis blip). The middleware
//! fails closed and keeps the in-flight lock. The default in-flight TTL is now
//! 60 s, so a retry gets `409` for one minute, not one day.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::idempotency::{
    IdempotencyEntry, IdempotencyFuture, IdempotencyLayer, IdempotencyRecord, IdempotencyStore,
    IdempotencyStoreError, MemoryIdempotencyStore,
};
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use tower::ServiceExt as _;

/// A memory store whose `set` fails, like Redis during an outage.
struct SetFailsStore {
    inner: MemoryIdempotencyStore,
}

impl IdempotencyStore for SetFailsStore {
    fn get<'a>(&'a self, key: &'a str) -> IdempotencyFuture<'a, Option<IdempotencyEntry>> {
        self.inner.get(key)
    }

    fn set<'a>(
        &'a self,
        _key: &'a str,
        _record: IdempotencyRecord,
        _body_hash: Vec<u8>,
        _ttl: Duration,
    ) -> IdempotencyFuture<'a, ()> {
        Box::pin(async { Err(IdempotencyStoreError::backend("redis: connection reset")) })
    }

    fn try_lock<'a>(
        &'a self,
        key: &'a str,
        owner: &'a str,
        lock_ttl: Duration,
    ) -> IdempotencyFuture<'a, bool> {
        self.inner.try_lock(key, owner, lock_ttl)
    }

    fn unlock<'a>(&'a self, key: &'a str, owner: &'a str) -> IdempotencyFuture<'a, ()> {
        self.inner.unlock(key, owner)
    }
}

fn post(key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/charge")
        .header("idempotency-key", key)
        .body(Body::empty())
        .expect("request")
}

#[sim_test]
async fn sim_idempotency_failed_save_holds_the_key_for_the_in_flight_ttl_only(sim: Sim) {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    CALLS.store(0, Ordering::SeqCst);

    let store = Arc::new(SetFailsStore {
        inner: MemoryIdempotencyStore::new(Duration::from_secs(86_400)),
    });
    // The default layer: no explicit in-flight TTL.
    let app = axum::Router::new()
        .route(
            "/charge",
            axum::routing::post(|| async {
                CALLS.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(Response::new(Body::from("charged")))
            }),
        )
        .layer(IdempotencyLayer::new(store));

    let first = app.clone().oneshot(post("blip")).await.expect("infallible");
    assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);

    let retry = app.clone().oneshot(post("blip")).await.expect("infallible");
    assert_eq!(
        retry.status(),
        StatusCode::CONFLICT,
        "the key is still held"
    );

    sim.advance(Duration::from_secs(59)).await;
    let retry = app.clone().oneshot(post("blip")).await.expect("infallible");
    assert_eq!(retry.status(), StatusCode::CONFLICT, "one second is left");

    sim.advance(Duration::from_secs(2)).await;
    let retry = app.clone().oneshot(post("blip")).await.expect("infallible");
    assert_eq!(
        retry.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "after 60 s the key is free again; the handler runs, and the save fails again"
    );
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn sim_idempotency_config_default_in_flight_ttl_is_one_minute() {
    let config = autumn_web::config::IdempotencyConfig::default();
    assert_eq!(config.in_flight_ttl_secs, 60);
    assert_eq!(
        config.ttl_secs, 86_400,
        "the response retention does not change"
    );
}
