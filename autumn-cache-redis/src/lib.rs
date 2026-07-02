//! Redis-backed shared cache for Autumn applications.
//!
//! This crate provides [`RedisCache`], an implementation of the
//! `autumn_web::cache::Cache` trait backed by Redis, plus
//! [`RedisCachePlugin`] which wires it into the app via the plugin system.
//!
//! # Usage
//!
//! ```toml
//! # autumn.toml
//! [cache]
//! backend = "redis"
//!
//! [cache.redis]
//! url = "redis://redis:6379"
//! key_prefix = "myapp:cache"
//! ```
//!
//! ```rust,ignore
//! use autumn_cache_redis::RedisCachePlugin;
//!
//! autumn_web::app()
//!     .plugin(RedisCachePlugin::new())
//!     .routes(routes![...])
//!     .run()
//!     .await;
//! ```
//!
//! Values are serialized as JSON so they survive across replicas and restarts.
//! Invalidation on replica A is immediately visible on replica B — no TTL lag.
//!
//! Use [`autumn_web::cache::insert_cached`] / [`autumn_web::cache::get_cached`]
//! (which the `#[cached]` macro generates) to read and write values that are
//! `serde::Serialize + serde::Deserialize`. The plain [`autumn_web::cache::insert`]
//! / [`autumn_web::cache::get`] functions work only for in-process backends.
//! `CacheResponseLayer` uses Autumn's serde-aware cache path internally, so
//! HTTP response caching is supported with this Redis backend.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::cache::{Cache, FillLockStatus, RawCacheBytes};
use redis::AsyncCommands as _;
use redis::aio::ConnectionManager;
use thiserror::Error;
use tracing::debug;

/// Lua script for a compare-and-delete fill-lock release: only deletes the
/// lock key if it still holds the caller's token, so a caller can never
/// release a lock that another replica has since acquired (e.g. after this
/// caller's lock expired and was taken over).
const RELEASE_FILL_LOCK_SCRIPT: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('DEL', KEYS[1])
else
    return 0
end
";

/// Errors that can occur when constructing or using a [`RedisCache`].
#[derive(Debug, Error)]
pub enum RedisCacheError {
    #[error("Redis connection error: {0}")]
    Connection(#[from] redis::RedisError),
    #[error("missing Redis URL in cache config")]
    MissingUrl,
}

/// A [`Cache`] implementation backed by Redis.
///
/// Values are stored as JSON blobs. Multiple replicas share the same Redis
/// namespace so writes on replica A are immediately visible on replica B, and
/// invalidations propagate within a single round-trip.
///
/// Use [`autumn_web::cache::insert_cached`] and [`autumn_web::cache::get_cached`]
/// (or equivalently the `#[cached]` macro) to store and retrieve values — both
/// functions handle JSON serialization transparently. The plain
/// `autumn_web::cache::insert` / `get` functions perform only in-memory
/// downcasts and will miss on cross-replica reads. `CacheResponseLayer` is
/// safe to use because it stores response entries through the serialized path.
///
/// # Runtime requirement
///
/// `RedisCache` bridges the synchronous [`Cache`] trait to async Redis
/// operations via [`tokio::task::block_in_place`]. This requires a
/// **multi-thread** Tokio runtime. Using `RedisCache` from a single-thread
/// runtime (e.g. the default `#[tokio::test]` flavor) will panic. In tests,
/// use `#[tokio::test(flavor = "multi_thread")]`.
#[derive(Clone)]
pub struct RedisCache {
    manager: ConnectionManager,
    key_prefix: String,
}

fn ttl_millis_for_redis(ttl: std::time::Duration) -> u64 {
    let millis = ttl.as_millis().max(1);
    u64::try_from(millis).unwrap_or(u64::MAX)
}

impl RedisCache {
    /// Connect using an explicit URL and key prefix.
    ///
    /// # Errors
    ///
    /// Returns [`RedisCacheError::Connection`] if the initial connection fails.
    pub async fn connect(
        url: &str,
        key_prefix: impl Into<String>,
    ) -> Result<Self, RedisCacheError> {
        let client = redis::Client::open(url)?;
        let manager = ConnectionManager::new(client).await?;
        Ok(Self {
            manager,
            key_prefix: key_prefix.into(),
        })
    }

    /// Build a `RedisCache` from the `[cache]` section of `autumn.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`RedisCacheError::MissingUrl`] if `cache.redis.url` is absent.
    /// Returns [`RedisCacheError::Connection`] on connection failure.
    pub async fn from_config(
        config: &autumn_web::config::CacheRedisConfig,
    ) -> Result<Self, RedisCacheError> {
        let url = config.url.as_deref().ok_or(RedisCacheError::MissingUrl)?;
        Self::connect(url, &config.key_prefix).await
    }

    fn prefixed(&self, key: &str) -> String {
        format!("{}:{}", self.key_prefix, key)
    }

    fn fill_lock_key(&self, key: &str) -> String {
        format!("{}:{}:fill-lock", self.key_prefix, key)
    }

    fn redis_get(&self, key: &str) -> Option<Vec<u8>> {
        let prefixed = self.prefixed(key);
        let mut conn = self.manager.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { conn.get(&prefixed).await.ok().flatten() })
        })
    }

    fn redis_set(&self, key: &str, bytes: Vec<u8>, ttl: Option<std::time::Duration>) {
        let prefixed = self.prefixed(key);
        let mut conn = self.manager.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                if let Some(ttl) = ttl {
                    let millis = ttl_millis_for_redis(ttl);
                    let _: Result<(), _> = redis::cmd("PSETEX")
                        .arg(&prefixed)
                        .arg(millis)
                        .arg(bytes)
                        .query_async(&mut conn)
                        .await;
                } else {
                    let _: Result<(), _> = conn.set(&prefixed, bytes).await;
                }
            });
        });
    }
}

impl Cache for RedisCache {
    /// Retrieves a value from Redis and returns it as [`RawCacheBytes`].
    ///
    /// Callers using [`autumn_web::cache::get_cached`] (which the `#[cached]`
    /// macro generates) will have the bytes automatically deserialized into the
    /// concrete return type `V` via `serde_json`. Direct callers that need the
    /// concrete type should also use `get_cached` rather than `get_value`.
    fn get_value(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.redis_get(key)
            .map(|bytes| Arc::new(RawCacheBytes(bytes)) as Arc<dyn Any + Send + Sync>)
    }

    /// Stores a value in Redis by serializing the most common primitive types.
    ///
    /// For arbitrary serde types — including structs and collections — use
    /// [`insert_raw_bytes`] (called automatically by
    /// [`autumn_web::cache::insert_cached`]) instead. Unknown types are
    /// silently skipped here because [`autumn_web::cache::insert_cached`] will have already
    /// written the serialized form via [`insert_raw_bytes`].
    ///
    /// [`insert_raw_bytes`]: Cache::insert_raw_bytes
    fn insert_value(&self, key: &str, value: Arc<dyn Any + Send + Sync>) {
        // Only handle RawCacheBytes round-trips and the primitive types used by
        // direct `cache::insert` callers. Serde types arrive via insert_raw_bytes.
        let bytes: Option<Vec<u8>> = value
            .downcast_ref::<RawCacheBytes>()
            .map(|raw| raw.0.clone())
            .or_else(|| {
                value
                    .downcast_ref::<String>()
                    .and_then(|s| serde_json::to_vec(s).ok())
            })
            .or_else(|| {
                value
                    .downcast_ref::<i64>()
                    .and_then(|n| serde_json::to_vec(n).ok())
            })
            .or_else(|| {
                value
                    .downcast_ref::<i32>()
                    .and_then(|n| serde_json::to_vec(n).ok())
            });

        if let Some(bytes) = bytes {
            // insert_value has no TTL context; use no-expiry SET for these callers.
            self.redis_set(key, bytes, None);
            debug!(key, "RedisCache: inserted via insert_value");
        }
    }

    /// Stores pre-serialized JSON bytes in Redis, applying the TTL when provided.
    ///
    /// This is the primary write path for `#[cached]`-annotated functions (via
    /// [`autumn_web::cache::insert_cached`]). It handles all `serde::Serialize`
    /// types, including structs and collections that `insert_value` cannot
    /// serialize from an erased `Arc<dyn Any>`.
    ///
    /// When `ttl` is `Some`, the entry is stored with millisecond precision so
    /// Redis expires it automatically, matching the TTL declared on `#[cached]`.
    fn insert_raw_bytes(&self, key: &str, bytes: Vec<u8>, ttl: Option<std::time::Duration>) {
        self.redis_set(key, bytes, ttl);
        debug!(key, "RedisCache: inserted via insert_raw_bytes");
    }

    fn invalidate(&self, key: &str) {
        let prefixed = self.prefixed(key);
        let mut conn = self.manager.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let _: Result<(), _> = conn.del(&prefixed).await;
            });
        });
        debug!(key, "RedisCache: invalidated");
    }

    fn clear(&self) {
        // Use SCAN instead of KEYS to avoid blocking the Redis server on large
        // keyspaces. SCAN is O(1) per call and processes the keyspace in batches.
        let pattern = format!("{}:*", self.key_prefix);
        let mut conn = self.manager.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut cursor: u64 = 0;
                loop {
                    let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(&pattern)
                        .arg("COUNT")
                        .arg(100u32)
                        .query_async(&mut conn)
                        .await
                        .unwrap_or((0, vec![]));
                    if !keys.is_empty() {
                        let _: Result<(), _> = conn.del(keys).await;
                    }
                    cursor = next_cursor;
                    if cursor == 0 {
                        break;
                    }
                }
            });
        });
    }

    /// Acquires a cross-replica fill lock via `SET NX PX`. Redis errors are
    /// treated as [`FillLockStatus::Held`] (fail closed toward "someone else
    /// may be filling") — the caller's `lock_wait_timeout` bounds how long it
    /// waits before falling back to filling locally.
    fn try_acquire_fill_lock(&self, key: &str, token: &str, ttl: Duration) -> FillLockStatus {
        let lock_key = self.fill_lock_key(key);
        let millis = ttl_millis_for_redis(ttl);
        let mut conn = self.manager.clone();
        let token = token.to_owned();
        let acquired = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                redis::cmd("SET")
                    .arg(&lock_key)
                    .arg(&token)
                    .arg("NX")
                    .arg("PX")
                    .arg(millis)
                    .query_async::<Option<String>>(&mut conn)
                    .await
            })
        });
        match acquired {
            Ok(Some(_)) => FillLockStatus::Acquired,
            Ok(None) => FillLockStatus::Held,
            Err(e) => {
                debug!(key, error = %e, "RedisCache: fill lock acquire failed, treating as held");
                FillLockStatus::Held
            }
        }
    }

    /// Releases the fill lock only if `token` still owns it (compare-and-delete
    /// via a Lua script), so a caller can never release a lock another replica
    /// has since taken over after this caller's lock expired.
    fn release_fill_lock(&self, key: &str, token: &str) {
        let lock_key = self.fill_lock_key(key);
        let mut conn = self.manager.clone();
        let token = token.to_owned();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let script = redis::Script::new(RELEASE_FILL_LOCK_SCRIPT);
                let _: Result<i64, _> = script
                    .key(&lock_key)
                    .arg(&token)
                    .invoke_async(&mut conn)
                    .await;
            });
        });
        debug!(key, "RedisCache: released fill lock");
    }
}

// ── Plugin ────────────────────────────────────────────────────────────────────

/// Autumn plugin that wires `RedisCache` as the global application cache.
///
/// Reads `[cache.redis]` from the active `AutumnConfig` and installs a
/// `RedisCache` into `AppState` via `with_cache_backend`.
///
/// When `cache.backend != "redis"` the plugin is a no-op and the default
/// per-function Moka caches continue to work.
pub struct RedisCachePlugin;

impl RedisCachePlugin {
    /// Create the plugin.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for RedisCachePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl autumn_web::plugin::Plugin for RedisCachePlugin {
    fn build(self, app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder {
        app.on_startup(|state| async move {
            // Read the config the framework already stored as an extension.
            let config = state
                .extension::<autumn_web::config::AutumnConfig>()
                .expect("AutumnConfig must be registered before RedisCachePlugin startup");

            if !config.cache.is_redis() {
                return Ok(());
            }

            let redis_cfg = &config.cache.redis;
            let cache = RedisCache::from_config(redis_cfg).await.map_err(|e| {
                autumn_web::AutumnError::service_unavailable_msg(format!(
                    "Failed to connect RedisCache: {e}"
                ))
            })?;

            state.set_cache(Arc::new(cache));
            tracing::info!("RedisCache registered as global application cache");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::cache::{get_cached, insert_cached};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis as RedisImage;

    #[test]
    fn redis_ttl_millis_preserves_subsecond_precision() {
        assert_eq!(
            ttl_millis_for_redis(std::time::Duration::from_millis(100)),
            100
        );
        assert_eq!(
            ttl_millis_for_redis(std::time::Duration::from_millis(1_500)),
            1_500
        );
    }

    #[test]
    fn redis_ttl_millis_never_uses_zero() {
        assert_eq!(ttl_millis_for_redis(std::time::Duration::ZERO), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_cache_insert_get_invalidate() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");

        let cache = RedisCache::connect(&url, "test").await.unwrap();

        // insert_cached serializes via serde_json and stores via insert_raw_bytes
        insert_cached(&cache, "hello", "world".to_string(), None);

        // get_value returns RawCacheBytes wrapping the JSON
        let raw = cache.get_value("hello").expect("should be present");
        let raw_bytes = raw.downcast_ref::<RawCacheBytes>().expect("RawCacheBytes");
        let v: serde_json::Value = serde_json::from_slice(&raw_bytes.0).unwrap();
        assert_eq!(v, serde_json::json!("world"));

        // get_cached deserializes back to the concrete type
        let s: Option<String> = get_cached(&cache, "hello");
        assert_eq!(s.as_deref(), Some("world"));

        // Invalidate
        autumn_web::cache::Cache::invalidate(&cache, "hello");
        assert!(cache.get_value("hello").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_cache_cross_replica_invalidation() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");

        // Two "replicas" sharing the same Redis
        let replica_a = RedisCache::connect(&url, "xreplica").await.unwrap();
        let replica_b = RedisCache::connect(&url, "xreplica").await.unwrap();

        // A writes via insert_cached (the path used by #[cached])
        let start = std::time::Instant::now();
        insert_cached(&replica_a, "key", "value".to_string(), None);

        // B can read it via get_cached and gets the correctly-typed value
        let seen: Option<String> = get_cached(&replica_b, "key");
        let elapsed = start.elapsed();

        assert_eq!(
            seen.as_deref(),
            Some("value"),
            "replica B must read the value written by replica A"
        );

        // A invalidates
        autumn_web::cache::Cache::invalidate(&replica_a, "key");

        // B no longer sees it (within one round-trip = < 50 ms p99)
        let gone: Option<String> = get_cached(&replica_b, "key");
        assert!(gone.is_none());
        assert!(
            elapsed.as_millis() < 50,
            "cross-replica lag {elapsed:?} exceeded 50 ms"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_cache_serde_struct_round_trip() {
        #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Item {
            id: i32,
            name: String,
        }

        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");

        let cache_a = RedisCache::connect(&url, "structs").await.unwrap();
        let cache_b = RedisCache::connect(&url, "structs").await.unwrap();

        let item = Item {
            id: 1,
            name: "widget".into(),
        };
        insert_cached(&cache_a, "item:1", item.clone(), None);

        // Same replica
        let retrieved: Option<Item> = get_cached(&cache_a, "item:1");
        assert_eq!(retrieved, Some(item.clone()));

        // Cross-replica
        let from_b: Option<Item> = get_cached(&cache_b, "item:1");
        assert_eq!(from_b, Some(item));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_cache_response_layer_caches_http_gets() {
        use std::convert::Infallible;

        use autumn_web::cache::CacheResponseLayer;
        use autumn_web::reexports::axum::body::{Body, to_bytes};
        use autumn_web::reexports::http::{Request, StatusCode};
        use tower::{Service, ServiceBuilder, ServiceExt};

        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");

        let cache: Arc<dyn autumn_web::cache::Cache> =
            Arc::new(RedisCache::connect(&url, "http-response").await.unwrap());
        let counter = Arc::new(AtomicUsize::new(0));

        let inner = {
            let counter = counter.clone();
            tower::service_fn(move |_req: Request<Body>| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(
                        autumn_web::reexports::axum::response::Response::builder()
                            .status(StatusCode::OK)
                            .header("x-cache-test", "redis")
                            .body(Body::from("redis-body"))
                            .expect("infallible response builder"),
                    )
                }
            })
        };

        let mut svc = ServiceBuilder::new()
            .layer(CacheResponseLayer::from_shared(cache.clone()))
            .service(inner);

        let req = Request::get("/redis-backed")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("service ready")
            .call(req)
            .await
            .expect("infallible service");
        assert_eq!(resp.status(), StatusCode::OK);

        let req = Request::get("/redis-backed")
            .body(Body::empty())
            .expect("infallible response builder");
        let resp = svc
            .ready()
            .await
            .expect("service ready")
            .call(req)
            .await
            .expect("infallible service");

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-cache-test")
                .and_then(|v| v.to_str().ok()),
            Some("redis")
        );
        let body = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body collection");
        assert_eq!(body.as_ref(), b"redis-body");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        cache.invalidate("http:/redis-backed");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_fill_lock_acquire_conflict_release() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let cache = RedisCache::connect(&url, "fill-lock-test").await.unwrap();

        assert_eq!(
            cache.try_acquire_fill_lock("k", "token-a", Duration::from_secs(10)),
            FillLockStatus::Acquired
        );
        assert_eq!(
            cache.try_acquire_fill_lock("k", "token-b", Duration::from_secs(10)),
            FillLockStatus::Held,
            "a second replica must not acquire a lock already held"
        );

        // Releasing with the wrong token must not free the lock.
        cache.release_fill_lock("k", "token-b");
        assert_eq!(
            cache.try_acquire_fill_lock("k", "token-b", Duration::from_secs(10)),
            FillLockStatus::Held,
            "releasing with a stale/foreign token must be a no-op"
        );

        // Releasing with the owning token frees it for the next acquirer.
        cache.release_fill_lock("k", "token-a");
        assert_eq!(
            cache.try_acquire_fill_lock("k", "token-b", Duration::from_secs(10)),
            FillLockStatus::Acquired
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_fill_lock_expires_after_ttl() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let cache = RedisCache::connect(&url, "fill-lock-ttl-test").await.unwrap();

        assert_eq!(
            cache.try_acquire_fill_lock("k", "crashed-filler", Duration::from_millis(200)),
            FillLockStatus::Acquired
        );
        // Never release — simulate a filler that crashed while holding the lock.
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert_eq!(
            cache.try_acquire_fill_lock("k", "new-filler", Duration::from_secs(10)),
            FillLockStatus::Acquired,
            "the lock TTL must bound damage from a crashed filler"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_lock_wait_timeout_falls_back_to_self_fill() {
        use autumn_web::cache::GetOrComputeOptions;

        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let cache: Arc<dyn Cache> =
            Arc::new(RedisCache::connect(&url, "lock-timeout-test").await.unwrap());

        // Simulate a stuck holder that never releases and outlives the test.
        let key = "stuck-key";
        assert_eq!(
            cache.try_acquire_fill_lock(key, "stuck-holder", Duration::from_secs(60)),
            FillLockStatus::Acquired
        );

        let fill_count = Arc::new(AtomicUsize::new(0));
        let fc = fill_count.clone();
        let opts = GetOrComputeOptions::new()
            .distributed_fill_lock(true)
            .lock_wait_timeout(Duration::from_millis(300))
            .lock_poll_interval(Duration::from_millis(50));

        let value: i32 = autumn_web::cache::get_or_compute_with(&cache, key, opts, move || async move {
            fc.fetch_add(1, Ordering::SeqCst);
            Ok::<i32, String>(123)
        })
        .await
        .unwrap();

        assert_eq!(value, 123);
        assert_eq!(
            fill_count.load(Ordering::SeqCst),
            1,
            "after the wait timeout the caller must fall back to filling locally"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_get_or_compute_round_trip() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let cache: Arc<dyn Cache> =
            Arc::new(RedisCache::connect(&url, "read-through-test").await.unwrap());

        let fill_count = Arc::new(AtomicUsize::new(0));
        let fc = fill_count.clone();
        let v: String = autumn_web::cache::get_or_compute(
            &cache,
            "rt-key",
            Some(Duration::from_secs(60)),
            move || async move {
                fc.fetch_add(1, Ordering::SeqCst);
                Ok::<String, String>("value-from-redis".to_string())
            },
        )
        .await
        .unwrap();
        assert_eq!(v, "value-from-redis");

        let fc = fill_count.clone();
        let v: String = autumn_web::cache::get_or_compute(
            &cache,
            "rt-key",
            Some(Duration::from_secs(60)),
            move || async move {
                fc.fetch_add(1, Ordering::SeqCst);
                Ok::<String, String>("should-not-run".to_string())
            },
        )
        .await
        .unwrap();
        assert_eq!(v, "value-from-redis", "second call must hit via RawCacheBytes deserialization");
        assert_eq!(fill_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_cross_replica_single_fill() {
        use autumn_web::cache::GetOrComputeOptions;

        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");

        // Two independent connections sharing the same key prefix, modeling
        // two application replicas.
        let replica_a: Arc<dyn Cache> =
            Arc::new(RedisCache::connect(&url, "xrepl-fill").await.unwrap());
        let replica_b: Arc<dyn Cache> =
            Arc::new(RedisCache::connect(&url, "xrepl-fill").await.unwrap());

        let fill_count = Arc::new(AtomicUsize::new(0));
        let key = "hot-key";

        let mut handles = Vec::new();
        for replica in [replica_a, replica_b] {
            for _ in 0..4 {
                let replica = replica.clone();
                let fc = fill_count.clone();
                let opts = GetOrComputeOptions::new()
                    .distributed_fill_lock(true)
                    .lock_poll_interval(Duration::from_millis(30))
                    .lock_wait_timeout(Duration::from_secs(5));
                handles.push(tokio::spawn(async move {
                    autumn_web::cache::get_or_compute_with::<String, String, _, _>(
                        &replica, key, opts, move || async move {
                            fc.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            Ok("cross-replica-value".to_string())
                        },
                    )
                    .await
                }));
            }
        }

        for h in handles {
            let v = h.await.unwrap().unwrap();
            assert_eq!(v, "cross-replica-value");
        }

        assert_eq!(
            fill_count.load(Ordering::SeqCst),
            1,
            "the distributed fill lock must limit total fills to 1 across both replicas"
        );
    }
}
