//! Integration tests for the global cache registry (issue #535).
//!
//! These tests verify:
//! - `AppState` can hold and return an `Arc<dyn Cache>` via `cache()`
//! - `CacheResponseLayer::from_app` wires to the configured backend
//! - The Moka fallback is preserved when no global cache is registered
//! - The `[cache]` config section selects the backend

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use autumn_web::AppState;
use autumn_web::cache::{Cache, CacheResponseLayer, MokaCache, get, insert};
use axum::body::Body;
use http::Request;
use http::StatusCode;
use tower::{Service, ServiceBuilder, ServiceExt};

// The global cache is process-wide, so tests that manipulate it (via
// `set_cache`/`clear_global_cache`) hold this mutex to prevent interference
// from each other — mirrors the pattern in `tests/cached_global_backend.rs`.
static GLOBAL_CACHE_LOCK: Mutex<()> = Mutex::new(());

// ── Helpers ──────────────────────────────────────────────────────────────────

fn counting_service(
    counter: Arc<AtomicUsize>,
    body: &'static str,
) -> impl Service<
    Request<Body>,
    Response = axum::response::Response,
    Error = Infallible,
    Future = impl std::future::Future<Output = Result<axum::response::Response, Infallible>> + Send,
> + Clone
+ Send
+ 'static {
    let body = body.to_owned();
    tower::service_fn(move |_req: Request<Body>| {
        let counter = counter.clone();
        let body = body.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(axum::response::Response::builder()
                .status(StatusCode::OK)
                .body(Body::from(body))
                .expect("infallible"))
        }
    })
}

// ── AppState::cache() ─────────────────────────────────────────────────────────

#[test]
fn app_state_has_no_cache_by_default() {
    let state = AppState::for_test();
    assert!(
        state.cache().is_none(),
        "no cache registered yet → should be None"
    );
}

#[test]
fn app_state_cache_returns_registered_backend() {
    let moka = Arc::new(MokaCache::new(100, None));
    let state = AppState::for_test().with_cache(moka.clone() as Arc<dyn Cache>);
    let cache = state.cache().expect("cache should be registered");
    // Round-trip: insert via the shared Arc, read via state.cache()
    insert(moka.as_ref(), "ping", "pong".to_string());
    assert_eq!(
        get::<String>(cache.as_ref(), "ping"),
        Some("pong".to_string())
    );
}

// ── CacheResponseLayer::from_app ──────────────────────────────────────────────

#[tokio::test]
async fn cache_response_layer_from_app_uses_registered_cache() {
    let moka = Arc::new(MokaCache::new(100, None));
    let state = AppState::for_test().with_cache(moka as Arc<dyn Cache>);
    let counter = Arc::new(AtomicUsize::new(0));

    let layer = CacheResponseLayer::from_app(&state).expect("cache must be registered");

    let mut svc = ServiceBuilder::new()
        .layer(layer)
        .service(counting_service(counter.clone(), "result"));

    // First call — miss
    let req = Request::get("/item/1").body(Body::empty()).unwrap();
    let resp = svc.ready().await.unwrap().call(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // Second call — hit
    let req = Request::get("/item/1").body(Body::empty()).unwrap();
    let resp = svc.ready().await.unwrap().call(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(counter.load(Ordering::SeqCst), 1, "should be a cache hit");
}

#[test]
fn cache_response_layer_from_app_returns_none_when_no_cache() {
    let state = AppState::for_test();
    assert!(
        CacheResponseLayer::from_app(&state).is_none(),
        "from_app with no cache should return None"
    );
}

// ── Moka fallback still works ─────────────────────────────────────────────────

#[tokio::test]
async fn cache_response_layer_from_cache_still_works() {
    let store = MokaCache::new(100, None);
    let counter = Arc::new(AtomicUsize::new(0));

    let mut svc = ServiceBuilder::new()
        .layer(CacheResponseLayer::from_cache(store))
        .service(counting_service(counter.clone(), "hello"));

    let req = Request::get("/v1").body(Body::empty()).unwrap();
    svc.ready().await.unwrap().call(req).await.unwrap();
    let req = Request::get("/v1").body(Body::empty()).unwrap();
    svc.ready().await.unwrap().call(req).await.unwrap();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "second call should be cached"
    );
}

// ── AppState::set_cache() ─────────────────────────────────────────────────────

#[test]
fn app_state_set_cache_installs_via_extension_map() {
    use autumn_web::cache::{clear_global_cache, global_cache};
    let _g = GLOBAL_CACHE_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    clear_global_cache();

    let state = AppState::for_test();
    assert!(state.cache().is_none(), "starts with no cache");

    let moka = Arc::new(MokaCache::new(10, None));
    state.set_cache(moka.clone() as Arc<dyn Cache>);

    // set_cache must also populate the global process-level cache
    assert!(global_cache().is_some(), "global cache must be set");

    // set_cache stores via extension map, so state.cache() must find it
    let retrieved = state
        .cache()
        .expect("set_cache must make cache() return Some");
    insert(moka.as_ref(), "x", 7_i32);
    assert_eq!(get::<i32>(retrieved.as_ref(), "x"), Some(7));

    clear_global_cache();
}

// ── set_cache overrides build-time with_cache ─────────────────────────────────

#[test]
fn set_cache_overrides_build_time_cache() {
    use autumn_web::cache::clear_global_cache;
    let _g = GLOBAL_CACHE_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    clear_global_cache();

    let build_time = Arc::new(MokaCache::new(10, None));
    let state = AppState::for_test().with_cache(build_time.clone() as Arc<dyn Cache>);

    // Initially cache() returns the build-time backend.
    insert(build_time.as_ref(), "k", "build".to_string());
    assert_eq!(
        get::<String>(state.cache().unwrap().as_ref(), "k"),
        Some("build".to_string())
    );

    // A startup hook replaces it via set_cache.
    let runtime = Arc::new(MokaCache::new(10, None));
    insert(runtime.as_ref(), "k", "runtime".to_string());
    state.set_cache(runtime as Arc<dyn Cache>);

    // cache() must now return the runtime backend, not the build-time one.
    assert_eq!(
        get::<String>(state.cache().unwrap().as_ref(), "k"),
        Some("runtime".to_string()),
        "set_cache must take priority over build-time with_cache"
    );

    clear_global_cache();
}

// ── CacheConfig deserialization ───────────────────────────────────────────────

#[test]
fn cache_config_defaults_to_memory() {
    use autumn_web::config::CacheConfig;
    let cfg: CacheConfig = toml::from_str("").unwrap();
    assert!(cfg.is_memory(), "default backend should be memory");
}

#[test]
fn cache_config_redis_variant() {
    use autumn_web::config::CacheConfig;
    let toml_str = r#"
        backend = "redis"
        [redis]
        url = "redis://localhost:6379"
    "#;
    let cfg: CacheConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.is_redis(), "should be redis backend");
    assert_eq!(cfg.redis.url.as_deref(), Some("redis://localhost:6379"));
}

#[test]
fn autumn_config_has_cache_section() {
    use autumn_web::config::AutumnConfig;
    let toml_str = r#"
        [cache]
        backend = "redis"
        [cache.redis]
        url = "redis://redis:6379"
    "#;
    let cfg: AutumnConfig = toml::from_str(toml_str).unwrap();
    assert!(cfg.cache.is_redis());
    assert_eq!(cfg.cache.redis.url.as_deref(), Some("redis://redis:6379"));
}
