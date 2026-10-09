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

use autumn_web::cache::{
    Cache, CacheFuture, FillEpoch, FillLockStatus, InvalidationError, RawCacheBytes, jittered_ttl,
    record_invalidation_failure,
};
use redis::AsyncCommands as _;
use redis::aio::ConnectionManager;
use thiserror::Error;
use tracing::{debug, warn};

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

/// Lua script for a fenced insert: store the value only if the namespace epoch
/// still equals the one the fill sampled. One script call, so an `INCR` cannot
/// land between the check and the store. `KEYS[2]` is the epoch key. `ARGV` is
/// the sampled epoch, the value, and the TTL in ms (empty for none).
const FENCED_INSERT_SCRIPT: &str = r"
local current = redis.call('GET', KEYS[2])
if not current then current = '0' end
if current ~= ARGV[1] then return 0 end
if ARGV[3] == '' then
    redis.call('SET', KEYS[1], ARGV[2])
else
    redis.call('PSETEX', KEYS[1], ARGV[3], ARGV[2])
end
return 1
";

/// [`FENCED_INSERT_SCRIPT`], hashed once.
static FENCED_INSERT: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| redis::Script::new(FENCED_INSERT_SCRIPT));

/// Errors that can occur when constructing or using a [`RedisCache`].
#[derive(Debug, Error)]
pub enum RedisCacheError {
    #[error("Redis connection error: {0}")]
    Connection(#[from] redis::RedisError),
    #[error("missing Redis URL in cache config")]
    MissingUrl,
}

/// Retry policy for invalidation (`DEL` and the namespace sweep).
///
/// A failed attempt sleeps, then tries again. The sleep starts at
/// `base_backoff`, doubles after each attempt, and stops at `max_backoff`.
/// Each sleep gets ±50 % jitter, so replicas do not retry at the same time.
/// If all attempts fail, the async methods return the error. The sync methods
/// log it with `warn!` and count it in `autumn_cache_invalidation_failures_total`.
///
/// Make one with [`InvalidationRetry::new`] or [`Default`]. The type is
/// `#[non_exhaustive]`, so new fields can come later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct InvalidationRetry {
    /// Total attempts, including the first. `0` counts as `1`.
    pub max_attempts: u32,
    /// Sleep before the second attempt.
    pub base_backoff: Duration,
    /// Upper limit for each sleep.
    pub max_backoff: Duration,
}

impl Default for InvalidationRetry {
    /// 3 attempts, 20 ms base, 200 ms cap. The two sleeps total at most about
    /// 90 ms.
    fn default() -> Self {
        Self::new(3, Duration::from_millis(20), Duration::from_millis(200))
    }
}

impl InvalidationRetry {
    /// A policy with `max_attempts` tries, a first sleep of `base_backoff`,
    /// and no sleep longer than `max_backoff`.
    #[must_use]
    pub const fn new(max_attempts: u32, base_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            max_attempts,
            base_backoff,
            max_backoff,
        }
    }

    /// Total attempts. Always at least 1.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        if self.max_attempts == 0 {
            1
        } else {
            self.max_attempts
        }
    }

    /// Sleep after failed attempt number `attempt` (0-based).
    ///
    /// The result is between half the nominal sleep and `max_backoff`.
    #[must_use]
    pub fn backoff(&self, attempt: u32) -> Duration {
        let nominal = self
            .base_backoff
            .saturating_mul(1_u32 << attempt.min(20))
            .min(self.max_backoff);
        jittered_ttl(nominal, 0.5).min(self.max_backoff)
    }
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
/// The sync [`Cache`] methods bridge to async Redis calls with
/// [`tokio::task::block_in_place`]. They need a **multi-thread** Tokio
/// runtime, and panic on a current-thread runtime (for example the default
/// `#[tokio::test]` flavor).
///
/// [`Cache::invalidate_async`] and [`Cache::invalidate_namespace_async`] are
/// real async calls. They work on any runtime and do not hold a worker.
/// Framework invalidation (generated repositories,
/// `coherence::invalidate_namespace_async`) uses them.
///
/// # Invalidation errors
///
/// Invalidation retries with [`InvalidationRetry`]. The async methods return
/// the final error. The sync `invalidate` and `clear` cannot return it, so
/// they log it with `warn!` and count it in
/// `autumn_cache_invalidation_failures_total`.
#[derive(Clone)]
pub struct RedisCache {
    manager: ConnectionManager,
    key_prefix: String,
    invalidation_retry: InvalidationRetry,
}

fn ttl_millis_for_redis(ttl: std::time::Duration) -> u64 {
    let millis = ttl.as_millis().max(1);
    u64::try_from(millis).unwrap_or(u64::MAX)
}

/// Escape Redis GLOB pattern metacharacters (`\`, `*`, `?`, `[`) in `s` so it
/// matches only literally when used inside a `SCAN`/`KEYS` `MATCH` pattern.
///
/// `key_prefix` is an operator-supplied config value (`cache.redis.key_prefix`
/// in `autumn.toml`), not attacker-controlled request input, but an operator
/// prefix that happens to contain one of these characters (e.g. `tenant:*`)
/// would otherwise be interpreted as a glob by [`RedisCache::clear`]'s `SCAN
/// MATCH {prefix}:*`, letting `clear()` match — and delete — keys outside the
/// namespace the prefix was meant to scope it to.
fn escape_redis_glob(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
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
        // `open_client`, not `redis::Client::open`: it installs the
        // process-wide rustls `CryptoProvider` that `redis`'s
        // `tokio-rustls-comp` needs before a `rediss://` URL can be dialled,
        // and it is the same guard every Redis client inside `autumn-web`
        // goes through. Scoped to TLS schemes there, so a plain `redis://`
        // connection never claims the process-wide default. See #2172.
        let client = autumn_web::redis_tls::open_client(url)?;
        let manager = ConnectionManager::new(client).await?;
        Ok(Self {
            manager,
            key_prefix: key_prefix.into(),
            invalidation_retry: InvalidationRetry::default(),
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

    /// Set the retry policy for invalidation. See [`InvalidationRetry`].
    #[must_use]
    pub const fn with_invalidation_retry(mut self, retry: InvalidationRetry) -> Self {
        self.invalidation_retry = retry;
        self
    }

    fn prefixed(&self, key: &str) -> String {
        format!("{}:{}", self.key_prefix, key)
    }

    /// Physical key for `key`'s distributed fill lock.
    ///
    /// Deliberately **not** nested under `key_prefix:` (i.e. not
    /// `prefixed(format!("{key}:fill-lock"))`): `prefixed()` maps every
    /// possible `key` onto `"{key_prefix}:{key}"`, so any suffix appended
    /// under that same namespace is reachable by *some* ordinary cache key
    /// (e.g. an app key literally named `"session:fill-lock"`) and could
    /// silently collide with a lock entry. Using a distinct top-level sentinel
    /// here means a collision would require the operator's own `key_prefix`
    /// to equal `"__autumn_fill_lock__"`, not merely an unlucky choice of
    /// cache key.
    fn fill_lock_key(&self, key: &str) -> String {
        format!("__autumn_fill_lock__:{}:{}", self.key_prefix, key)
    }

    /// Key of a namespace's shared fill epoch.
    ///
    /// Outside `key_prefix:`, like [`Self::fill_lock_key`]. This matters here:
    /// [`Cache::clear`] sweeps `{key_prefix}:*`. If `clear` removed an epoch,
    /// the counter would restart at 0, and a stale fill that sampled 0 would
    /// pass.
    fn epoch_key(&self, namespace: &str) -> String {
        format!("__autumn_epoch__:{}:{}", self.key_prefix, namespace)
    }

    /// Raise the namespace's shared epoch, with retries.
    async fn bump_epoch(&self, namespace: &str) -> Result<(), InvalidationError> {
        let key = self.epoch_key(namespace);
        self.retry_invalidation(&key, || {
            let mut conn = self.manager.clone();
            let key = key.clone();
            async move { conn.incr::<_, _, i64>(key, 1).await.map(|_| ()) }
        })
        .await
    }

    /// Drop a namespace for all replicas.
    ///
    /// The epoch rises first. Then a fill that sampled the old epoch is fenced
    /// out, and the sweep removes anything stored before the bump. The sweep
    /// runs even if the bump failed. The first error is returned.
    async fn invalidate_namespace_fenced(&self, namespace: &str) -> Result<(), InvalidationError> {
        let bumped = self.bump_epoch(namespace).await;
        let swept = self
            .sweep_with_retry(&self.namespace_pattern(namespace))
            .await;
        bumped.and(swept)
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

    /// Delete every key matching `pattern`, walking the keyspace with `SCAN`
    /// rather than `KEYS` so a large keyspace never blocks the server.
    ///
    /// Shared by [`Cache::clear`] and [`Cache::invalidate_namespace`], which
    /// differ only in how many segments of the key they pin.
    ///
    /// The first `SCAN` or `DEL` error stops the walk and is returned. A
    /// partial sweep is not a complete one. The sweep is idempotent, so a
    /// retry starts again from cursor 0.
    async fn sweep(&self, pattern: &str) -> Result<(), redis::RedisError> {
        let mut conn = self.manager.clone();
        let mut cursor: u64 = 0;
        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(100u32)
                .query_async(&mut conn)
                .await?;
            if !keys.is_empty() {
                conn.del::<_, ()>(keys).await?;
            }
            cursor = next_cursor;
            if cursor == 0 {
                return Ok(());
            }
        }
    }

    /// Run `op` until it succeeds or the retry budget is spent.
    ///
    /// Each failed attempt is logged at `debug`. The caller decides what to do
    /// with the final error: return it, or log it and count it.
    async fn retry_invalidation<F, Fut>(
        &self,
        target: &str,
        mut op: F,
    ) -> Result<(), InvalidationError>
    where
        F: FnMut() -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), redis::RedisError>> + Send,
    {
        let attempts = self.invalidation_retry.attempts();
        let mut last_error = String::new();
        for attempt in 0..attempts {
            if attempt > 0 {
                tokio::time::sleep(self.invalidation_retry.backoff(attempt - 1)).await;
            }
            match op().await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    debug!(
                        target,
                        attempt = attempt + 1,
                        attempts,
                        error = %error,
                        "RedisCache: invalidation attempt failed"
                    );
                    last_error = error.to_string();
                }
            }
        }
        Err(InvalidationError::new(attempts, last_error))
    }

    /// `DEL` one key, with retries.
    async fn delete_key(&self, key: &str) -> Result<(), InvalidationError> {
        let prefixed = self.prefixed(key);
        self.retry_invalidation(key, || {
            let mut conn = self.manager.clone();
            let prefixed = prefixed.clone();
            async move { conn.del::<_, ()>(prefixed).await }
        })
        .await
    }

    /// Sweep every key that matches `pattern`, with retries.
    async fn sweep_with_retry(&self, pattern: &str) -> Result<(), InvalidationError> {
        self.retry_invalidation(pattern, || self.sweep(pattern))
            .await
    }

    /// The `SCAN MATCH` pattern for one namespace.
    ///
    /// Cache keys are `{key_prefix}:{namespace}:{hash}`, so
    /// `{key_prefix}:{namespace}:*` drops exactly one cached read's entries.
    /// Both parts are escaped: an unescaped `*` or `[` would widen the sweep.
    fn namespace_pattern(&self, namespace: &str) -> String {
        format!(
            "{}:{}:*",
            escape_redis_glob(&self.key_prefix),
            escape_redis_glob(namespace)
        )
    }
}

/// Run an async invalidation from a sync [`Cache`] method. Needs a
/// multi-thread runtime, like every sync method here.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// Log and count an invalidation error that a sync method cannot return.
fn report_dropped_invalidation_error(target: &str, error: &InvalidationError) {
    warn!(
        target,
        error = %error,
        "RedisCache: invalidation failed; stale data can be served until its TTL"
    );
    record_invalidation_failure();
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

    /// Reads the namespace's shared epoch (one `GET`). A missing key is 0. A
    /// Redis error is [`FillEpoch::Unavailable`], so the fill skips its insert.
    fn fill_epoch(&self, namespace: &str) -> FillEpoch {
        let key = self.epoch_key(namespace);
        let mut conn = self.manager.clone();
        let read: Result<Option<u64>, _> = block_on(async move { conn.get(&key).await });
        match read {
            Ok(epoch) => FillEpoch::Sampled(epoch.unwrap_or(0)),
            Err(error) => {
                debug!(namespace, error = %error, "RedisCache: epoch read failed");
                FillEpoch::Unavailable
            }
        }
    }

    /// Stores the bytes through one Lua call that compares the epoch first. An
    /// error counts as not stored: the cost is one extra miss.
    fn insert_raw_bytes_if_epoch(
        &self,
        key: &str,
        bytes: Vec<u8>,
        ttl: Option<std::time::Duration>,
        namespace: &str,
        sampled: u64,
    ) -> bool {
        let prefixed = self.prefixed(key);
        let epoch_key = self.epoch_key(namespace);
        let ttl_ms = ttl.map_or_else(String::new, |ttl| ttl_millis_for_redis(ttl).to_string());
        let mut conn = self.manager.clone();
        let result: Result<i64, _> = block_on(async move {
            FENCED_INSERT
                .key(&prefixed)
                .key(&epoch_key)
                .arg(sampled.to_string())
                .arg(bytes)
                .arg(ttl_ms)
                .invoke_async(&mut conn)
                .await
        });
        match result {
            Ok(stored) => stored == 1,
            Err(error) => {
                debug!(key, error = %error, "RedisCache: fenced insert failed");
                false
            }
        }
    }

    /// Sync `DEL` with retries. The final error is logged with `warn!` and
    /// counted, because this method cannot return it. Prefer
    /// [`Cache::invalidate_async`].
    fn invalidate(&self, key: &str) {
        match block_on(self.delete_key(key)) {
            Ok(()) => debug!(key, "RedisCache: invalidated"),
            Err(error) => report_dropped_invalidation_error(key, &error),
        }
    }

    /// Sync namespace sweep. Returns `false` when the sweep failed after all
    /// retries. The caller owns that `false`, so it is not counted here.
    fn invalidate_namespace(&self, namespace: &str) -> bool {
        // Issue #1716: a declared invalidation edge must be complete on a
        // shared, cross-replica backend. The `bool` tells the caller whether
        // stale data can still be served.
        match block_on(self.invalidate_namespace_fenced(namespace)) {
            Ok(()) => true,
            Err(error) => {
                debug!(namespace, error = %error, "RedisCache: namespace sweep failed");
                false
            }
        }
    }

    fn clear(&self) {
        // SCAN, not KEYS, so a large keyspace never blocks the server.
        let pattern = format!("{}:*", escape_redis_glob(&self.key_prefix));
        if let Err(error) = block_on(self.sweep_with_retry(&pattern)) {
            report_dropped_invalidation_error(&pattern, &error);
        }
    }

    /// Async `DEL` with retries. No `block_in_place`, so it works on a
    /// current-thread runtime. Returns the final error.
    fn invalidate_async<'a>(
        &'a self,
        key: &'a str,
    ) -> CacheFuture<'a, Result<(), InvalidationError>> {
        Box::pin(self.delete_key(key))
    }

    /// Async namespace sweep with retries. Returns the final error.
    fn invalidate_namespace_async<'a>(
        &'a self,
        namespace: &'a str,
    ) -> CacheFuture<'a, Result<(), InvalidationError>> {
        Box::pin(self.invalidate_namespace_fenced(namespace))
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
        let result: Result<i64, _> = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let script = redis::Script::new(RELEASE_FILL_LOCK_SCRIPT);
                script
                    .key(&lock_key)
                    .arg(&token)
                    .invoke_async(&mut conn)
                    .await
            })
        });
        match result {
            Ok(_) => debug!(key, "RedisCache: released fill lock"),
            Err(e) => {
                debug!(key, error = %e, "RedisCache: fill lock release failed");
            }
        }
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
    /// This plugin ships in lockstep with `autumn-web` — see
    /// [`lockstep_contract`](autumn_web::plugin_contract::lockstep_contract).
    fn contract(&self) -> Option<autumn_web::plugin_contract::PluginContract> {
        Some(autumn_web::plugin_contract::lockstep_contract(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
    }

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
            register_cache_health_indicator(&state, &config);
            Ok(())
        })
    }
}

/// Register the `redis:cache` `PING` indicator (issue #3059), with the
/// `[health]` rules of the built-in Redis indicators. The framework does not
/// register it, because only this plugin installs the Redis cache.
fn register_cache_health_indicator(
    state: &autumn_web::AppState,
    config: &autumn_web::config::AutumnConfig,
) {
    let Some(url) = config.cache.redis.url.as_deref() else {
        return;
    };
    // Share the app's connection to this URL with the built-in indicators.
    match autumn_web::redis_health::RedisHealthIndicator::shared(state, url) {
        Ok(indicator) => {
            let indicator = indicator.configured(&config.health);
            let group = autumn_web::actuator::HealthIndicator::group(&indicator);
            if let Err(error) = state.health_indicator_registry().register(
                "redis:cache",
                group,
                Arc::new(indicator),
            ) {
                tracing::warn!("{error}");
            }
        }
        Err(error) => tracing::warn!(error = %error, "redis:cache health indicator not registered"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_health_indicator_follows_the_health_config() {
        let state = autumn_web::AppState::for_test();
        let mut config = autumn_web::config::AutumnConfig::default();
        config.cache.redis.url = Some("redis://127.0.0.1:1".to_owned());
        config.health.redis_readiness = true;

        register_cache_health_indicator(&state, &config);

        assert!(state.health_indicator_registry().contains("redis:cache"));
    }

    #[test]
    fn no_cache_health_indicator_without_a_url() {
        let state = autumn_web::AppState::for_test();
        let config = autumn_web::config::AutumnConfig::default();

        register_cache_health_indicator(&state, &config);

        assert!(!state.health_indicator_registry().contains("redis:cache"));
    }
    use autumn_web::cache::{FillEpoch, get_cached, insert_cached, insert_cached_fenced};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis as RedisImage;

    #[test]
    fn contract_declares_lockstep_with_own_crate() {
        use autumn_web::plugin::Plugin;

        let contract = RedisCachePlugin::new().contract().expect("a contract");
        assert_eq!(contract.plugin, env!("CARGO_PKG_NAME"));
        assert_eq!(
            contract.plugin_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            contract.autumn_web.as_deref(),
            Some(autumn_web::plugin_contract::lockstep_range(env!("CARGO_PKG_VERSION")).as_str())
        );
        assert!(contract.experimental_surfaces.is_empty());
    }

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

    /// This crate must not build its own Redis client: `RedisCache::connect`
    /// goes through `autumn_web::redis_tls::open_client`, which installs the
    /// process-level rustls `CryptoProvider` a `rediss://` URL needs before
    /// rustls is asked to resolve one (#2172). A second, hand-rolled install
    /// here is exactly how one copy of this logic drifts from the other.
    ///
    /// Walks `src/` rather than scanning `lib.rs` alone: the crate is a
    /// single file today, and a scan that silently stops covering the crate
    /// the day a second module appears is the drift it exists to prevent.
    #[test]
    fn the_cache_never_builds_a_redis_client_outside_the_shared_tls_guard() {
        // Split so the needles do not match this declaration.
        let needles = [concat!("Client::", "open("), concat!("build_with", "_tls(")];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_sources(&src, &mut files);
        assert!(!files.is_empty(), "no sources under {}", src.display());
        files.sort();

        let mut offenders = Vec::new();
        for file in files {
            let source = std::fs::read_to_string(&file).expect("read source");
            for (index, line) in source.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                if needles.iter().any(|needle| line.contains(needle)) {
                    offenders.push(format!(
                        "{}:{}: {}",
                        file.strip_prefix(&src).unwrap_or(&file).display(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "build Redis clients through `autumn_web::redis_tls::open_client`, \
             not directly — a `rediss://` URL otherwise reaches rustls with no \
             process-level CryptoProvider installed and panics (#2172):\n{}",
            offenders.join("\n")
        );
    }

    /// Recursively collect every `.rs` file under `dir`.
    fn collect_rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect_rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The TLS classification itself is `autumn_web::redis_tls`'s contract;
    /// this only pins that this crate is wired to the same one.
    #[test]
    fn the_shared_guard_classifies_the_tls_schemes_this_cache_is_deployed_on() {
        assert!(autumn_web::redis_tls::url_needs_tls_crypto_provider(
            "rediss://cache.redis.cache.windows.net:6380/"
        ));
        assert!(!autumn_web::redis_tls::url_needs_tls_crypto_provider(
            "redis://127.0.0.1:6379/"
        ));
    }

    #[test]
    fn escape_redis_glob_leaves_plain_prefixes_unchanged() {
        assert_eq!(escape_redis_glob("myapp:cache"), "myapp:cache");
    }

    #[test]
    fn escape_redis_glob_escapes_scan_match_metacharacters() {
        // Regression: `clear()` builds a SCAN MATCH pattern as
        // "{key_prefix}:*". An unescaped operator-supplied prefix containing
        // a glob metacharacter (e.g. "tenant:*") would let SCAN MATCH match
        // — and clear() then delete — keys well outside that prefix's
        // intended namespace.
        assert_eq!(escape_redis_glob("tenant:*"), r"tenant:\*");
        assert_eq!(escape_redis_glob("a?b"), r"a\?b");
        assert_eq!(escape_redis_glob("[ab]"), r"\[ab]");
        assert_eq!(escape_redis_glob(r"back\slash"), r"back\\slash");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rediss_scheme_is_usable_without_a_tls_cargo_feature_error() {
        // The azure-container-apps release target's generated Redis Cache
        // disables the non-TLS port (main.tf: non_ssl_port_enabled = false),
        // so it only ever hands the app a `rediss://` URL. Without a TLS
        // Cargo feature compiled in, `redis::Client::open` rejects that
        // scheme immediately at URL-parse time (before any network I/O) with
        // "can't connect with TLS, the feature is not enabled" — invisible
        // until a real Azure deploy, since no local test exercised it. This
        // needs no Docker/testcontainer TLS-capable Redis: with the
        // tls-rustls feature compiled in (workspace Cargo.toml), parsing
        // succeeds and the failure that follows (a closed loopback port) is
        // a genuine network error instead.
        let result = RedisCache::connect("rediss://127.0.0.1:1/", "test").await;
        let Err(err) = result else {
            panic!("connecting to a closed port must fail");
        };
        let message = err.to_string();
        assert!(
            !message.contains("feature is not enabled") && !message.contains("without the tls"),
            "the `redis` crate must be built with a TLS feature (tokio-rustls-comp) \
             so `rediss://` URLs actually connect: {message}"
        );
    }

    // ── #3056: invalidation errors are retried, then surfaced ──────────

    /// A plain connection for test set-up and fault injection.
    async fn admin_conn(url: &str) -> redis::aio::MultiplexedConnection {
        autumn_web::redis_tls::open_client(url)
            .expect("client")
            .get_multiplexed_async_connection()
            .await
            .expect("admin connection")
    }

    /// Make the server a replica of a master that does not exist. A replica
    /// is read-only, so each `DEL` gets a `READONLY` error. The data stays.
    async fn reject_writes(admin: &mut redis::aio::MultiplexedConnection) {
        redis::cmd("REPLICAOF")
            .arg("127.0.0.1")
            .arg(1)
            .query_async::<()>(admin)
            .await
            .expect("REPLICAOF");
    }

    /// Make the server a master again, so writes succeed.
    async fn accept_writes(admin: &mut redis::aio::MultiplexedConnection) {
        redis::cmd("REPLICAOF")
            .arg("NO")
            .arg("ONE")
            .query_async::<()>(admin)
            .await
            .expect("REPLICAOF NO ONE");
    }

    async fn key_exists(admin: &mut redis::aio::MultiplexedConnection, key: &str) -> bool {
        admin.exists::<_, bool>(key).await.expect("EXISTS")
    }

    const FAST_RETRY: InvalidationRetry = InvalidationRetry {
        max_attempts: 3,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
    };

    #[test]
    fn default_retry_policy_is_bounded() {
        let retry = InvalidationRetry::default();
        assert_eq!(retry.max_attempts, 3);
        assert!(retry.base_backoff <= retry.max_backoff);
        assert!(retry.max_backoff <= Duration::from_secs(1));
    }

    #[test]
    fn retry_backoff_grows_and_never_passes_the_cap() {
        let retry = InvalidationRetry {
            max_attempts: 10,
            base_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(80),
        };
        for _ in 0..200 {
            for attempt in 0..10 {
                let nominal = retry
                    .base_backoff
                    .saturating_mul(1 << attempt.min(20))
                    .min(retry.max_backoff);
                let delay = retry.backoff(attempt);
                assert!(delay <= retry.max_backoff, "{delay:?} passes the cap");
                assert!(delay >= nominal / 2, "{delay:?} below half of {nominal:?}");
            }
        }
    }

    #[test]
    fn a_zero_attempt_policy_still_tries_once() {
        let retry = InvalidationRetry {
            max_attempts: 0,
            ..InvalidationRetry::default()
        };
        assert_eq!(retry.attempts(), 1);
    }

    /// The async path must not use `block_in_place`, so this test uses the
    /// default current-thread runtime. `block_in_place` panics there.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_invalidate_async_surfaces_readonly_del_failure() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let mut admin = admin_conn(&url).await;
        let cache = RedisCache::connect(&url, "inv-fail")
            .await
            .unwrap()
            .with_invalidation_retry(FAST_RETRY);

        admin.set::<_, _, ()>("inv-fail:k", "1").await.unwrap();
        reject_writes(&mut admin).await;

        let err = cache
            .invalidate_async("k")
            .await
            .expect_err("a rejected DEL must surface as an error");
        assert_eq!(err.attempts(), 3, "all attempts must run: {err}");
        assert!(
            err.reason().to_ascii_lowercase().contains("read only"),
            "the reason must carry the Redis error: {err}"
        );
        assert!(
            key_exists(&mut admin, "inv-fail:k").await,
            "the key must still exist, so the error is true"
        );

        accept_writes(&mut admin).await;
        cache
            .invalidate_async("k")
            .await
            .expect("DEL succeeds once Redis accepts writes");
        assert!(!key_exists(&mut admin, "inv-fail:k").await);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_invalidate_retries_until_del_succeeds() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let mut admin = admin_conn(&url).await;
        // At least 49 sleeps of 5 ms or more: the fault clears long before
        // the budget ends.
        let cache = RedisCache::connect(&url, "inv-retry")
            .await
            .unwrap()
            .with_invalidation_retry(InvalidationRetry {
                max_attempts: 50,
                base_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
            });

        admin.set::<_, _, ()>("inv-retry:k", "1").await.unwrap();
        reject_writes(&mut admin).await;

        let mut healer = admin.clone();
        let heal = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            accept_writes(&mut healer).await;
        });

        cache
            .invalidate_async("k")
            .await
            .expect("a transient DEL failure must be retried, not dropped");
        heal.await.unwrap();
        assert!(!key_exists(&mut admin, "inv-retry:k").await);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_invalidate_namespace_async_surfaces_failure() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let mut admin = admin_conn(&url).await;
        let cache = RedisCache::connect(&url, "inv-ns")
            .await
            .unwrap()
            .with_invalidation_retry(FAST_RETRY);

        admin.set::<_, _, ()>("inv-ns:reads:1", "1").await.unwrap();
        reject_writes(&mut admin).await;

        let err = cache
            .invalidate_namespace_async("reads")
            .await
            .expect_err("a rejected sweep must surface as an error");
        assert_eq!(err.attempts(), 3, "{err}");
        assert!(key_exists(&mut admin, "inv-ns:reads:1").await);

        accept_writes(&mut admin).await;
        cache
            .invalidate_namespace_async("reads")
            .await
            .expect("the sweep succeeds once Redis accepts writes");
        assert!(!key_exists(&mut admin, "inv-ns:reads:1").await);
    }

    // ── #2356: the fill fence is shared across replicas ─────────────────

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_fill_on_another_replica_cannot_resurrect_an_invalidated_value() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let mut admin = admin_conn(&url).await;
        let replica_a = RedisCache::connect(&url, "fence").await.unwrap();
        let replica_b = RedisCache::connect(&url, "fence").await.unwrap();

        // B misses, samples the shared epoch, and starts computing.
        let epoch = replica_b.fill_epoch("reads");
        assert_eq!(epoch, FillEpoch::Sampled(0));

        // A writes, invalidates, and is told it is complete.
        replica_a
            .invalidate_namespace_async("reads")
            .await
            .expect("invalidation");

        // B finishes. Its insert must be fenced out.
        let stored = insert_cached_fenced(
            &replica_b,
            "reads:k",
            "old".to_string(),
            None,
            "reads",
            epoch,
        );
        assert!(!stored, "the shared epoch moved");
        assert!(!key_exists(&mut admin, "fence:reads:k").await);

        // Without a TTL the entry has no expiry.
        let epoch = replica_b.fill_epoch("reads");
        assert!(insert_cached_fenced(
            &replica_b,
            "reads:forever",
            1_i64,
            None,
            "reads",
            epoch
        ));
        let left: i64 = redis::cmd("PTTL")
            .arg("fence:reads:forever")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(left, -1, "no TTL means no expiry");

        // A fill that starts after the invalidation is stored, with its TTL.
        let epoch = replica_b.fill_epoch("reads");
        assert_eq!(epoch, FillEpoch::Sampled(1));
        let ttl = Some(Duration::from_secs(60));
        assert!(insert_cached_fenced(
            &replica_b,
            "reads:k",
            "new".to_string(),
            ttl,
            "reads",
            epoch
        ));
        assert_eq!(
            get_cached::<String>(&replica_a, "reads:k").as_deref(),
            Some("new")
        );
        let left: i64 = redis::cmd("PTTL")
            .arg("fence:reads:k")
            .query_async(&mut admin)
            .await
            .unwrap();
        assert!(left > 0, "the TTL must survive the Lua insert: {left}");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_epoch_is_per_namespace_and_survives_clear() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let cache = RedisCache::connect(&url, "fence-ns").await.unwrap();

        cache.invalidate_namespace_async("a").await.unwrap();
        assert_eq!(cache.fill_epoch("a"), FillEpoch::Sampled(1));
        assert_eq!(cache.fill_epoch("b"), FillEpoch::Sampled(0));

        // `clear` sweeps the prefix. If it took the epoch, a sampled 0 would
        // match again after a bump.
        cache.clear();
        assert_eq!(cache.fill_epoch("a"), FillEpoch::Sampled(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_failed_epoch_bump_fails_the_invalidation() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let mut admin = admin_conn(&url).await;
        let cache = RedisCache::connect(&url, "fence-fail")
            .await
            .unwrap()
            .with_invalidation_retry(FAST_RETRY);

        // No key matches, so the sweep alone would succeed.
        reject_writes(&mut admin).await;
        assert!(
            cache.invalidate_namespace_async("reads").await.is_err(),
            "a `true` must not be reported when the shared fence did not move"
        );
        assert!(!cache.invalidate_namespace("reads"));
        accept_writes(&mut admin).await;
        assert!(cache.invalidate_namespace_async("reads").await.is_ok());
    }

    /// Serializes tests that read the process-global failure counter or set
    /// the global cache.
    static GLOBAL_STATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The path a generated repository write takes: the framework sweep with a
    /// `RedisCache` as the global backend, on a current-thread runtime.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn coherence_async_invalidation_reaches_redis_and_reports_failure() {
        let _guard = GLOBAL_STATE.lock().await;
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let mut admin = admin_conn(&url).await;
        let cache = RedisCache::connect(&url, "e2e")
            .await
            .unwrap()
            .with_invalidation_retry(FAST_RETRY);
        autumn_web::cache::set_global_cache(Arc::new(cache));
        let namespace = "e2e::reads::count";

        admin
            .set::<_, _, ()>(format!("e2e:{namespace}:a"), "1")
            .await
            .unwrap();
        let complete = autumn_web::cache::coherence::invalidate_namespace_async(namespace).await;
        let gone = !key_exists(&mut admin, &format!("e2e:{namespace}:a")).await;

        admin
            .set::<_, _, ()>(format!("e2e:{namespace}:b"), "1")
            .await
            .unwrap();
        reject_writes(&mut admin).await;
        let before = autumn_web::cache::invalidation_failures_total();
        let refused = autumn_web::cache::coherence::invalidate_namespace_async(namespace).await;
        let after = autumn_web::cache::invalidation_failures_total();
        let kept = key_exists(&mut admin, &format!("e2e:{namespace}:b")).await;
        accept_writes(&mut admin).await;
        autumn_web::cache::clear_global_cache();

        assert!(complete && gone, "a healthy Redis drops the namespace");
        assert!(!refused, "a rejected sweep must report incomplete");
        assert_eq!(after, before + 1, "and count one failure");
        assert!(kept, "the key is still there, so the report is true");
    }

    /// The sync `invalidate` returns `()`, so it cannot surface the error. It
    /// must log it and count it instead of dropping it.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_sync_invalidate_counts_final_failure() {
        let _guard = GLOBAL_STATE.lock().await;
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let mut admin = admin_conn(&url).await;
        let cache = RedisCache::connect(&url, "inv-sync")
            .await
            .unwrap()
            .with_invalidation_retry(FAST_RETRY);

        admin.set::<_, _, ()>("inv-sync:k", "1").await.unwrap();
        reject_writes(&mut admin).await;

        let before = autumn_web::cache::invalidation_failures_total();
        autumn_web::cache::Cache::invalidate(&cache, "k");
        let after = autumn_web::cache::invalidation_failures_total();
        assert_eq!(
            after,
            before + 1,
            "a final DEL failure must increment autumn_cache_invalidation_failures_total once"
        );
        assert!(key_exists(&mut admin, "inv-sync:k").await);

        accept_writes(&mut admin).await;
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
        let cache = RedisCache::connect(&url, "fill-lock-ttl-test")
            .await
            .unwrap();

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
        let cache: Arc<dyn Cache> = Arc::new(
            RedisCache::connect(&url, "lock-timeout-test")
                .await
                .unwrap(),
        );

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

        let value: i32 =
            autumn_web::cache::get_or_compute_with(&cache, key, opts, move || async move {
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
    async fn redis_lock_wait_timeout_not_overshot_by_poll_interval() {
        use autumn_web::cache::GetOrComputeOptions;

        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let cache: Arc<dyn Cache> = Arc::new(
            RedisCache::connect(&url, "lock-timeout-overshoot-test")
                .await
                .unwrap(),
        );

        // Simulate a stuck holder that never releases and outlives the test.
        let key = "stuck-key-overshoot";
        assert_eq!(
            cache.try_acquire_fill_lock(key, "stuck-holder", Duration::from_secs(60)),
            FillLockStatus::Acquired
        );

        // A poll interval much larger than the wait timeout must not make the
        // caller sleep past the timeout before falling back: lock_wait_timeout
        // bounds the total wait regardless of the poll cadence.
        let opts = GetOrComputeOptions::new()
            .distributed_fill_lock(true)
            .lock_wait_timeout(Duration::from_millis(100))
            .lock_poll_interval(Duration::from_secs(5));

        let start = std::time::Instant::now();
        let value: i32 =
            autumn_web::cache::get_or_compute_with(&cache, key, opts, move || async move {
                Ok::<i32, String>(456)
            })
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(value, 456);
        assert!(
            elapsed < Duration::from_millis(500),
            "lock_wait_timeout must bound the wait even when lock_poll_interval is much larger; took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Docker (testcontainers)"]
    async fn redis_get_or_compute_round_trip() {
        let container = RedisImage::default().start().await.unwrap();
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let cache: Arc<dyn Cache> = Arc::new(
            RedisCache::connect(&url, "read-through-test")
                .await
                .unwrap(),
        );

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
        assert_eq!(
            v, "value-from-redis",
            "second call must hit via RawCacheBytes deserialization"
        );
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
                        &replica,
                        key,
                        opts,
                        move || async move {
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
