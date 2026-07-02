use bytes::Bytes;
use futures::StreamExt as FuturesStreamExt;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode, request::Parts};
use axum::response::IntoResponse;
use sha2::Digest as _;
use tower::{Layer, Service};

static IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
static X_IDEMPOTENT_REPLAYED: &str = "x-idempotent-replayed";

/// Maximum response body size stored in the idempotency cache. Responses
/// larger than this are returned to the client as-is but not cached, so a
/// subsequent retry with the same key will re-execute the handler.
const MAX_CACHEABLE_RESPONSE_BODY: usize = 10 * 1024 * 1024; // 10 MiB

/// Fallback request body read limit used when the upload middleware extension
/// is absent (e.g. when `IdempotencyLayer` is used directly without the full
/// framework stack). Matches the framework default for
/// `security.upload.max_request_size_bytes`.
const DEFAULT_REQUEST_BODY_LIMIT: usize = 32 * 1024 * 1024; // 32 MiB

const fn is_mutating_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn compute_body_hash(bytes: &[u8], content_type: Option<&[u8]>) -> Vec<u8> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"content-type:");
    if let Some(content_type) = content_type {
        hasher.update(content_type);
    }
    hasher.update(b"\nbody:");
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(
        String::with_capacity(bytes.as_ref().len() * 2),
        |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

fn principal_scope_digest(session_id: Option<&str>) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"authorization:");
    hasher.update(b"\nsession:");
    if let Some(session_id) = session_id {
        hasher.update(session_id.as_bytes());
    }
    hex_lower(hasher.finalize())
}

fn push_storage_key_component(hasher: &mut sha2::Sha256, label: &str, value: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b":");
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(b":");
    hasher.update(value);
    hasher.update(b";");
}

/// Namespace the cache key by method, path, a stable principal digest, and the
/// client-supplied idempotency key.
///
/// Namespacing by method+path prevents cross-endpoint cache collisions (P2).
/// Namespacing by session scope prevents cross-principal collisions (P1) for
/// cookie-backed authenticated sessions. Request headers, including
/// `Authorization`, are intentionally excluded: client-controlled headers must
/// not let a retry force a fresh miss after a successful mutation. Opaque route
/// layers that resolve tenants, bearer principals, or policy state must use the
/// fail-closed replay path instead of storage-key partitioning. Each component
/// is length-delimited inside a SHA-256 digest so raw `:` bytes in paths or
/// client-controlled keys cannot synthesize another storage key.
#[derive(Clone)]
struct StorageKeyContext {
    idempotency_key: String,
    method: Method,
    target: String,
}

impl StorageKeyContext {
    fn from_parts(idempotency_key: String, parts: &axum::http::request::Parts) -> Self {
        let target = parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), |pq| pq.as_str().to_owned());
        Self {
            idempotency_key,
            method: parts.method.clone(),
            target,
        }
    }

    fn storage_key(&self, session_id: Option<&str>) -> String {
        build_storage_key(
            &self.idempotency_key,
            self.method.as_str(),
            &self.target,
            session_id,
        )
    }
}

fn build_storage_key(
    idempotency_key: &str,
    method: &str,
    target: &str,
    session_id: Option<&str>,
) -> String {
    let principal = principal_scope_digest(session_id);
    let mut hasher = sha2::Sha256::new();
    push_storage_key_component(&mut hasher, "method", method.as_bytes());
    push_storage_key_component(&mut hasher, "target", target.as_bytes());
    push_storage_key_component(&mut hasher, "scope-header-count", b"0");
    push_storage_key_component(&mut hasher, "principal", principal.as_bytes());
    push_storage_key_component(&mut hasher, "idempotency-key", idempotency_key.as_bytes());
    format!("v2:{}", hex_lower(hasher.finalize()))
}

async fn storage_session_id_for_parts(parts: &axum::http::request::Parts) -> Option<String> {
    let session_scope = parts
        .extensions
        .get::<IdempotencySessionScope>()
        .and_then(|scope| scope.session_id.as_deref().map(str::to_owned));
    let session = parts.extensions.get::<crate::session::Session>().cloned();
    if session_scope.is_some() {
        session_scope
    } else if let Some(session) = session
        && session.is_cookie_backed().await
    {
        Some(session.id().await)
    } else {
        None
    }
}

fn stale_cookie_session_id_for_parts(parts: &axum::http::request::Parts) -> Option<String> {
    parts
        .extensions
        .get::<IdempotencySessionScope>()
        .and_then(|scope| scope.stale_cookie_session_id.as_deref().map(str::to_owned))
}

fn extract_replay_headers(headers: &HeaderMap) -> Vec<(String, Vec<u8>)> {
    extract_replay_headers_with_policy(headers, false)
}

fn extract_finalized_session_replay_headers(headers: &HeaderMap) -> Vec<(String, Vec<u8>)> {
    extract_replay_headers_with_policy(headers, true)
}

fn extract_replay_headers_with_policy(
    headers: &HeaderMap,
    include_set_cookie: bool,
) -> Vec<(String, Vec<u8>)> {
    // Headers that must not be cached or replayed.
    // `set-cookie` is replayed only for finalized session-mutating responses
    // so lost successful mutations can deliver the session state they created.
    const SKIP: &[&str] = &[
        "connection",
        "transfer-encoding",
        "keep-alive",
        "upgrade",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "x-idempotent-replayed",
    ];
    headers
        .iter()
        .filter(|(name, _)| {
            !SKIP.contains(&name.as_str()) && (include_set_cookie || name.as_str() != "set-cookie")
        })
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
        .collect()
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Stored response associated with an idempotency key.
#[derive(Clone)]
pub struct IdempotencyRecord {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
    pub metadata: Vec<(String, Vec<u8>)>,
}

const FINALIZED_SESSION_SCOPE_METADATA: &str = "__autumn.idempotency.finalized-session-scope";
const FINALIZED_SESSION_OLD_SCOPE: &[u8] = b"old-session-scope";
const FINALIZED_SESSION_CURRENT_SCOPE: &[u8] = b"current-session-scope";

fn finalized_session_record(
    mut record: IdempotencyRecord,
    scope: &'static [u8],
) -> IdempotencyRecord {
    record
        .metadata
        .retain(|(name, _)| name != FINALIZED_SESSION_SCOPE_METADATA);
    record
        .metadata
        .push((FINALIZED_SESSION_SCOPE_METADATA.to_owned(), scope.to_vec()));
    record
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct IdempotencyCacheCommittedErrorResponse;

#[derive(Clone, Debug)]
pub(crate) struct IdempotencySessionScope {
    session_id: Option<String>,
    stale_cookie_session_id: Option<String>,
}

impl IdempotencySessionScope {
    #[must_use]
    pub(crate) const fn new(
        session_id: Option<String>,
        stale_cookie_session_id: Option<String>,
    ) -> Self {
        Self {
            session_id,
            stale_cookie_session_id,
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct IdempotencyReplayMetadata {
    entries: Vec<(String, Vec<u8>)>,
}

impl IdempotencyReplayMetadata {
    #[must_use]
    pub const fn new(entries: Vec<(String, Vec<u8>)>) -> Self {
        Self { entries }
    }

    fn into_entries(self) -> Vec<(String, Vec<u8>)> {
        self.entries
    }
}

/// Request-scoped idempotency metadata made available to inner handlers.
///
/// The raw `Idempotency-Key` header is available via [`Self::key`]. The
/// scoped key is the framework's collision-safe, principal-scoped storage key
/// and is the safer value to reuse for durable side-effect deduplication.
#[derive(Clone, Debug)]
pub struct IdempotencyContext {
    key: String,
    scoped_key: String,
    mutation_sequence: Arc<AtomicU64>,
}

impl IdempotencyContext {
    #[must_use]
    pub(crate) fn new(key: String, scoped_key: String) -> Self {
        Self {
            key,
            scoped_key,
            mutation_sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub fn scoped_key(&self) -> &str {
        &self.scoped_key
    }

    /// Return the next request-local mutation discriminator.
    ///
    /// Generated repository code uses this to distinguish multiple durable
    /// side effects produced by one idempotent request while keeping the same
    /// mutation slot stable across duplicate request attempts.
    #[must_use]
    pub fn next_mutation_discriminator(&self) -> String {
        self.mutation_sequence
            .fetch_add(1, Ordering::Relaxed)
            .to_string()
    }
}

impl PartialEq for IdempotencyContext {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.scoped_key == other.scoped_key
    }
}

impl Eq for IdempotencyContext {}

/// Cache entry wrapping a record with expiry and request body fingerprint.
#[derive(Clone)]
pub struct IdempotencyEntry {
    pub record: IdempotencyRecord,
    pub body_hash: Vec<u8>,
    pub expires_at: Instant,
}

// ── Store trait ───────────────────────────────────────────────────────────────

/// Error returned when an idempotency backend fails to persist a successful
/// mutation response.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct IdempotencyStoreError {
    message: String,
}

impl IdempotencyStoreError {
    #[must_use]
    pub fn backend(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Pluggable storage backend for idempotency entries.
///
/// Implementors must be `Send + Sync + 'static` to be used across async tasks.
/// All methods are synchronous; long-running I/O backends should use
/// [`tokio::task::block_in_place`] internally.
pub trait IdempotencyStore: Send + Sync + 'static {
    /// Return the cached entry if it exists and has not expired.
    fn get(&self, key: &str) -> Option<IdempotencyEntry>;

    /// Return the cached entry if it exists, surfacing backend read failures.
    ///
    /// Infallible stores can implement only [`Self::get`]. Fallible shared
    /// backends should override this method so lookup failures fail closed
    /// instead of being treated as cache misses that can duplicate mutations.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyStoreError`] when the backend cannot determine
    /// whether a record exists for this key.
    fn try_get(&self, key: &str) -> Result<Option<IdempotencyEntry>, IdempotencyStoreError> {
        Ok(self.get(key))
    }

    /// Persist a response with the given TTL.
    fn set(&self, key: &str, record: IdempotencyRecord, body_hash: Vec<u8>, ttl: Duration);

    /// Persist a response with the given TTL, surfacing backend failures.
    ///
    /// Existing infallible stores can implement only [`Self::set`]. Fallible
    /// backends should override this method so the middleware can fail closed
    /// rather than reporting a cacheable success that was not stored.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyStoreError`] when the backend cannot persist the
    /// response record.
    fn try_set(
        &self,
        key: &str,
        record: IdempotencyRecord,
        body_hash: Vec<u8>,
        ttl: Duration,
    ) -> Result<(), IdempotencyStoreError> {
        self.set(key, record, body_hash, ttl);
        Ok(())
    }

    /// Acquire an in-flight lock for `key`.
    ///
    /// Returns `true` if the lock was acquired (no concurrent request in flight)
    /// or `false` if another request is already processing this key.
    fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool;

    /// Acquire an in-flight lock owned by a unique request token.
    ///
    /// Stores that support expiring locks should override this together with
    /// [`Self::unlock_owned`] so a stale request cannot release a newer lock
    /// acquired after the first lock expired.
    fn try_lock_owned(&self, key: &str, owner: &str, lock_ttl: Duration) -> bool {
        let _ = owner;
        self.try_lock(key, lock_ttl)
    }

    /// Release the in-flight lock for `key`.
    fn unlock(&self, key: &str);

    /// Release the in-flight lock only if it is still owned by `owner`.
    fn unlock_owned(&self, key: &str, owner: &str) {
        let _ = owner;
        self.unlock(key);
    }

    /// The preferred TTL for this store. Used by [`IdempotencyLayer::new`] as
    /// the default expiry when no explicit `.with_ttl()` is given. Defaults to
    /// 24 hours if the store does not override this method.
    fn default_ttl(&self) -> Duration {
        Duration::from_secs(86_400)
    }
}

// ── Memory store ──────────────────────────────────────────────────────────────

/// In-memory idempotency store backed by a `RwLock<HashMap>`.
///
/// Evicts expired entries lazily on `get`. In-flight markers remain held until
/// `unlock` or until their configured in-flight lock TTL expires.
///
/// Suitable for single-process deployments and integration tests. For
/// multi-replica deployments configure `backend = "redis"` in `autumn.toml`.
pub struct MemoryIdempotencyStore {
    entries: RwLock<HashMap<String, IdempotencyEntry>>,
    in_flight: RwLock<HashMap<String, MemoryInFlightLock>>,
    /// Counts `set` calls to trigger periodic expired-entry eviction.
    write_count: AtomicU64,
    default_ttl: Duration,
}

struct MemoryInFlightLock {
    owner: String,
    expires_at: Instant,
}

impl MemoryIdempotencyStore {
    #[must_use]
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            in_flight: RwLock::new(HashMap::new()),
            write_count: AtomicU64::new(0),
            default_ttl,
        }
    }
}

impl IdempotencyStore for MemoryIdempotencyStore {
    fn get(&self, key: &str) -> Option<IdempotencyEntry> {
        // Release the read lock immediately after cloning.
        let entry = self.entries.read().unwrap().get(key).cloned();
        entry.filter(|e| e.expires_at > Instant::now())
    }

    fn set(&self, key: &str, record: IdempotencyRecord, body_hash: Vec<u8>, ttl: Duration) {
        let entry = IdempotencyEntry {
            record,
            body_hash,
            expires_at: Instant::now().checked_add(ttl).unwrap_or_else(Instant::now),
        };
        let mut entries = self.entries.write().unwrap();
        entries.insert(key.to_owned(), entry);
        // Periodically evict expired entries to bound memory growth for
        // long-running processes. O(N) scan is amortised over every 128 writes.
        let n = self.write_count.fetch_add(1, Ordering::Relaxed);
        if n.is_multiple_of(128) {
            let now = Instant::now();
            entries.retain(|_, v| v.expires_at > now);
        }
    }

    fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool {
        self.try_lock_owned(key, "", lock_ttl)
    }

    fn try_lock_owned(&self, key: &str, owner: &str, lock_ttl: Duration) -> bool {
        let now = Instant::now();
        let mut in_flight = self.in_flight.write().unwrap();
        // Check only the requested key's active in-flight marker.
        if let Some(lock) = in_flight.get(key)
            && lock.expires_at > now
        {
            return false; // still in flight
        }
        // Not locked: acquire until the handler finishes, unlocks, or the
        // safety TTL expires after cancellation.
        let ttl = if lock_ttl.is_zero() {
            Duration::from_secs(1)
        } else {
            lock_ttl
        };
        in_flight.insert(
            key.to_owned(),
            MemoryInFlightLock {
                owner: owner.to_owned(),
                expires_at: now.checked_add(ttl).unwrap_or(now),
            },
        );
        true
    }

    fn unlock(&self, key: &str) {
        self.in_flight.write().unwrap().remove(key);
    }

    fn unlock_owned(&self, key: &str, owner: &str) {
        let mut in_flight = self.in_flight.write().unwrap();
        if in_flight
            .get(key)
            .is_some_and(|lock| lock.owner.as_str() == owner)
        {
            in_flight.remove(key);
        }
    }

    fn default_ttl(&self) -> Duration {
        self.default_ttl
    }
}

// ── Redis store ───────────────────────────────────────────────────────────────

#[cfg(feature = "redis")]
mod redis_store {
    use super::{IdempotencyEntry, IdempotencyRecord, IdempotencyStore, IdempotencyStoreError};
    use redis::{AsyncCommands, Client, aio::ConnectionManager, aio::ConnectionManagerConfig};
    use serde::{Deserialize, Serialize};
    use std::time::{Duration, Instant};

    #[derive(Serialize, Deserialize)]
    struct StoredEntry {
        status: u16,
        headers: Vec<(String, Vec<u8>)>,
        body: Vec<u8>,
        #[serde(default)]
        metadata: Vec<(String, Vec<u8>)>,
        body_hash: Vec<u8>,
    }

    /// Redis-backed idempotency store for multi-replica deployments.
    ///
    /// Configured via `[idempotency.redis]` in `autumn.toml` or
    /// `AUTUMN_IDEMPOTENCY__REDIS__URL` env var.
    pub struct RedisIdempotencyStore {
        connection: ConnectionManager,
        key_prefix: String,
    }

    impl RedisIdempotencyStore {
        /// Creates a [`RedisIdempotencyStore`] from the application idempotency config.
        ///
        /// # Errors
        /// Returns an error string if no Redis URL is configured or if the Redis
        /// client cannot be opened.
        pub fn from_config(config: &crate::config::IdempotencyConfig) -> Result<Self, String> {
            let url = config
                .redis
                .url
                .as_deref()
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| {
                    "Redis idempotency backend requires a URL. \
                     Set AUTUMN_IDEMPOTENCY__REDIS__URL or \
                     [idempotency.redis] url in autumn.toml."
                        .to_owned()
                })?;
            let client = Client::open(url).map_err(|e| e.to_string())?;
            let connection =
                ConnectionManager::new_lazy_with_config(client, ConnectionManagerConfig::new())
                    .map_err(|e| e.to_string())?;
            Ok(Self {
                connection,
                key_prefix: config.redis.key_prefix.clone(),
            })
        }

        fn entry_key(&self, key: &str) -> String {
            format!("{}:entry:{}", self.key_prefix, key)
        }

        fn lock_key(&self, key: &str) -> String {
            format!("{}:lock:{}", self.key_prefix, key)
        }
    }

    impl IdempotencyStore for RedisIdempotencyStore {
        fn get(&self, key: &str) -> Option<IdempotencyEntry> {
            match self.try_get(key) {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "Redis GET failed for idempotency key"
                    );
                    None
                }
            }
        }

        fn try_get(&self, key: &str) -> Result<Option<IdempotencyEntry>, IdempotencyStoreError> {
            let redis_key = self.entry_key(key);
            let mut conn = self.connection.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    let data: Option<Vec<u8>> = conn.get(&redis_key).await.map_err(|e| {
                        IdempotencyStoreError::backend(format!(
                            "failed to read idempotency entry from Redis: {e}"
                        ))
                    })?;
                    data.map(|bytes| {
                        serde_json::from_slice::<StoredEntry>(&bytes)
                            .map(|e| {
                                IdempotencyEntry {
                                    record: IdempotencyRecord {
                                        status: e.status,
                                        headers: e.headers,
                                        body: e.body,
                                        metadata: e.metadata,
                                    },
                                    body_hash: e.body_hash,
                                    // Redis manages TTL natively. Use a fixed 24 h offset
                                    // so the in-process expiry check never fires early.
                                    expires_at: Instant::now().checked_add(Duration::from_secs(86_400)).unwrap_or_else(Instant::now),
                                }
                            })
                            .map_err(|e| {
                                IdempotencyStoreError::backend(format!(
                                    "failed to deserialize idempotency entry from Redis: {e}"
                                ))
                            })
                    })
                    .transpose()
                })
            })
        }

        fn set(&self, key: &str, record: IdempotencyRecord, body_hash: Vec<u8>, ttl: Duration) {
            if let Err(error) = self.try_set(key, record, body_hash, ttl) {
                tracing::warn!(
                    error = %error,
                    "Failed to persist idempotency entry to Redis"
                );
            }
        }

        fn try_set(
            &self,
            key: &str,
            record: IdempotencyRecord,
            body_hash: Vec<u8>,
            ttl: Duration,
        ) -> Result<(), IdempotencyStoreError> {
            let redis_key = self.entry_key(key);
            let mut conn = self.connection.clone();
            let entry = StoredEntry {
                status: record.status,
                headers: record.headers,
                body: record.body,
                metadata: record.metadata,
                body_hash,
            };
            let bytes = serde_json::to_vec(&entry).map_err(|e| {
                IdempotencyStoreError::backend(format!(
                    "failed to serialize idempotency entry: {e}"
                ))
            })?;
            let ttl_secs = ttl.as_secs().max(1);
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    conn.set_ex::<_, _, ()>(&redis_key, bytes, ttl_secs)
                        .await
                        .map_err(|e| {
                            IdempotencyStoreError::backend(format!(
                                "failed to persist idempotency entry to Redis: {e}"
                            ))
                        })
                })
            })
        }

        fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool {
            self.try_lock_owned(key, "", lock_ttl)
        }

        fn try_lock_owned(&self, key: &str, owner: &str, lock_ttl: Duration) -> bool {
            let lock_key = self.lock_key(key);
            let lock_ttl_secs = lock_ttl.as_secs().max(1);
            let mut conn = self.connection.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    let result: Result<Option<String>, _> = redis::cmd("SET")
                        .arg(&lock_key)
                        .arg(owner)
                        .arg("NX")
                        .arg("EX")
                        .arg(lock_ttl_secs)
                        .query_async(&mut conn)
                        .await;
                    match result {
                        Ok(opt) => opt.is_some(), // Some("OK") = acquired, None = already held
                        Err(e) => {
                            // Redis unavailable: fail closed so concurrent retries during an
                            // outage cannot both enter the handler and duplicate side effects.
                            // Clients receive 409 and should retry; once Redis recovers the
                            // lock can be acquired normally.
                            tracing::warn!(
                                error = %e,
                                "Redis idempotency lock unavailable; \
                                 failing closed to prevent duplicate processing"
                            );
                            false
                        }
                    }
                })
            })
        }

        fn unlock(&self, key: &str) {
            let lock_key = self.lock_key(key);
            let mut conn = self.connection.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    let _: Result<(), _> = conn.del(&lock_key).await;
                });
            });
        }

        fn unlock_owned(&self, key: &str, owner: &str) {
            let lock_key = self.lock_key(key);
            let mut conn = self.connection.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    let _: Result<i32, _> = redis::Script::new(
                        "if redis.call('GET', KEYS[1]) == ARGV[1] then \
                         return redis.call('DEL', KEYS[1]) else return 0 end",
                    )
                    .key(&lock_key)
                    .arg(owner)
                    .invoke_async(&mut conn)
                    .await;
                });
            });
        }
    }
}

#[cfg(feature = "redis")]
pub use redis_store::RedisIdempotencyStore;

#[doc(hidden)]
#[derive(Clone)]
pub struct IdempotencyReplayResponse {
    record: IdempotencyRecord,
}

impl IdempotencyReplayResponse {
    fn into_response(self) -> Response<Body> {
        response_from_record(self.record)
    }

    #[must_use]
    pub fn metadata(&self, key: &str) -> Option<&[u8]> {
        self.record
            .metadata
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_slice())
    }
}

#[doc(hidden)]
#[must_use]
pub fn __replay_response(
    replay: &Option<axum::extract::Extension<IdempotencyReplayResponse>>,
) -> Option<Response<Body>> {
    replay
        .as_ref()
        .map(|axum::extract::Extension(replay)| replay.clone().into_response())
}

#[doc(hidden)]
#[must_use]
pub fn __replay_finalized_session_response(
    replay: &Option<axum::extract::Extension<IdempotencyReplayResponse>>,
) -> Option<Response<Body>> {
    replay
        .as_ref()
        .and_then(|axum::extract::Extension(replay)| {
            let has_finalized_session_cookie = replay
                .record
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("set-cookie"));
            let is_old_session_scope = replay.metadata(FINALIZED_SESSION_SCOPE_METADATA)
                == Some(FINALIZED_SESSION_OLD_SCOPE);
            (has_finalized_session_cookie && is_old_session_scope)
                .then(|| replay.clone().into_response())
        })
}

#[doc(hidden)]
pub async fn __replay_finalized_session_response_for_anonymous(
    session: &crate::session::Session,
    auth_session_key: &str,
    replay: &Option<axum::extract::Extension<IdempotencyReplayResponse>>,
) -> Option<Response<Body>> {
    if session.get(auth_session_key).await.is_some() {
        return None;
    }
    __replay_finalized_session_response(replay)
}

#[doc(hidden)]
#[must_use]
pub const fn __cache_committed_error_response(error: crate::AutumnError) -> crate::AutumnError {
    error.cache_idempotency_response()
}

#[doc(hidden)]
#[must_use]
pub fn __replay_metadata(
    replay: &Option<axum::extract::Extension<IdempotencyReplayResponse>>,
    key: &str,
) -> Option<Vec<u8>> {
    replay
        .as_ref()
        .and_then(|axum::extract::Extension(replay)| replay.metadata(key).map(<[u8]>::to_vec))
}

#[doc(hidden)]
pub enum IdempotencyReplayOr<T> {
    Replay(Response<Body>),
    Inner(T),
    InnerWithReplayMetadata(T, Vec<(String, Vec<u8>)>),
}

impl<T> IntoResponse for IdempotencyReplayOr<T>
where
    T: IntoResponse,
{
    fn into_response(self) -> Response<Body> {
        match self {
            Self::Replay(response) => response,
            Self::Inner(inner) => inner.into_response(),
            Self::InnerWithReplayMetadata(inner, metadata) => {
                let mut response = inner.into_response();
                response
                    .extensions_mut()
                    .insert(IdempotencyReplayMetadata::new(metadata));
                response
            }
        }
    }
}

/// Inner route layer used by Autumn-generated handlers to stop before the
/// mutating handler when an outer idempotency layer has already found a replay.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdempotencyReplayLayer;

#[derive(Clone)]
pub struct IdempotencyReplayService<S> {
    inner: S,
}

impl<S> Layer<S> for IdempotencyReplayLayer {
    type Service = IdempotencyReplayService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        IdempotencyReplayService { inner }
    }
}

impl<S> Service<Request<Body>> for IdempotencyReplayService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        if let Some(replay) = req.extensions_mut().remove::<IdempotencyReplayResponse>() {
            return Box::pin(async move { Ok(replay.into_response()) });
        }

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

// ── Layer ─────────────────────────────────────────────────────────────────────

/// Tower [`Layer`] that enforces HTTP idempotency semantics per IETF
/// `draft-ietf-httpapi-idempotency-key-header`.
///
/// Applies only to mutating HTTP methods (POST, PUT, PATCH, DELETE).
/// Requests without an `Idempotency-Key` header are passed through unchanged.
///
/// - **Cache hit, same body**: replays the stored response with
///   `X-Idempotent-Replayed: true` and skips the handler.
/// - **Cache hit, different body**: returns `422 Unprocessable Entity`.
/// - **Concurrent duplicate** (same key, already in flight): returns
///   `409 Conflict` with `Retry-After: 1`.
/// - **Cache miss**: forwards to the handler, stores the response.
#[derive(Clone)]
pub struct IdempotencyLayer {
    store: Arc<dyn IdempotencyStore>,
    ttl: Duration,
    in_flight_ttl: Duration,
    replay_through_inner: bool,
    fail_closed_on_replay: bool,
    metrics: Option<crate::middleware::MetricsCollector>,
}

impl IdempotencyLayer {
    #[must_use]
    pub fn new(store: Arc<dyn IdempotencyStore>) -> Self {
        let ttl = store.default_ttl();
        Self {
            store,
            ttl,
            in_flight_ttl: ttl,
            replay_through_inner: false,
            fail_closed_on_replay: false,
            metrics: None,
        }
    }

    #[must_use]
    pub const fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    #[must_use]
    pub const fn with_in_flight_ttl(mut self, ttl: Duration) -> Self {
        self.in_flight_ttl = ttl;
        self
    }

    #[must_use]
    pub const fn replay_through_inner(mut self) -> Self {
        self.replay_through_inner = true;
        self.fail_closed_on_replay = false;
        self
    }

    #[must_use]
    pub const fn fail_closed_on_replay(mut self) -> Self {
        self.replay_through_inner = false;
        self.fail_closed_on_replay = true;
        self
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: crate::middleware::MetricsCollector) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

impl<S> Layer<S> for IdempotencyLayer {
    type Service = IdempotencyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        IdempotencyService {
            inner,
            store: self.store.clone(),
            ttl: self.ttl,
            in_flight_ttl: self.in_flight_ttl,
            replay_through_inner: self.replay_through_inner,
            fail_closed_on_replay: self.fail_closed_on_replay,
            metrics: self.metrics.clone(),
        }
    }
}

// ── Service ───────────────────────────────────────────────────────────────────

/// Tower [`Service`] produced by [`IdempotencyLayer`].
#[derive(Clone)]
pub struct IdempotencyService<S> {
    inner: S,
    store: Arc<dyn IdempotencyStore>,
    ttl: Duration,
    in_flight_ttl: Duration,
    replay_through_inner: bool,
    fail_closed_on_replay: bool,
    metrics: Option<crate::middleware::MetricsCollector>,
}

struct IdempotencyRequestConfig {
    store: Arc<dyn IdempotencyStore>,
    ttl: Duration,
    in_flight_ttl: Duration,
    replay_through_inner: bool,
    fail_closed_on_replay: bool,
    metrics: Option<crate::middleware::MetricsCollector>,
}

impl<S> Service<Request<Body>> for IdempotencyService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Clone-and-swap: the instance that was polled ready becomes `inner`
        // for this call; `self.inner` is replaced with a fresh clone for the
        // next call. This preserves Tower backpressure semantics.
        let clone = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, clone);
        let config = IdempotencyRequestConfig {
            store: self.store.clone(),
            ttl: self.ttl,
            in_flight_ttl: self.in_flight_ttl,
            replay_through_inner: self.replay_through_inner,
            fail_closed_on_replay: self.fail_closed_on_replay,
            metrics: self.metrics.clone(),
        };
        Box::pin(handle_idempotent_request(inner, config, req))
    }
}

fn request_body_too_large_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::PAYLOAD_TOO_LARGE)
        .body(Body::from(
            "request body too large for idempotency middleware",
        ))
        .unwrap()
}

fn in_flight_conflict_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::CONFLICT)
        .header("retry-after", "1")
        .body(Body::from(
            "a request with this idempotency key is already being processed; \
             retry after 1 second",
        ))
        .unwrap()
}

fn replay_requires_inner_stop_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::CONFLICT)
        .body(Body::from(
            "idempotency replay requires an inner replay stop for this route",
        ))
        .unwrap()
}

pub(crate) fn persistence_failed_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(Body::from("idempotency persistence unavailable"))
        .unwrap()
}

struct PreparedIdempotencyRequest {
    idempotency_key: String,
    storage_key: String,
    stale_cookie_storage_key: Option<String>,
    key_context: StorageKeyContext,
    body_hash: Vec<u8>,
    session: Option<crate::session::Session>,
    parts: Parts,
    body_bytes: Bytes,
}

struct InFlightLockGuard {
    store: Arc<dyn IdempotencyStore>,
    key: String,
    owner: String,
    /// When `true`, `drop` releases the in-flight lock. This is set only on
    /// outcomes we have *observed to completion* (a successful handler response,
    /// a cache double-check hit, a too-large/streamed body). It deliberately
    /// defaults to `false` so that if the inner handler future is dropped
    /// without one of those explicit outcomes — i.e. cancelled by the outer
    /// request-timeout layer, or unwound by a panic — the lock is left in place
    /// to expire via the store's in-flight safety TTL instead of being released
    /// immediately. A mutation may have committed its side effect before the
    /// cancellation point, so eagerly unlocking would let a retry carrying the
    /// same `Idempotency-Key` re-execute it; holding the lock fails closed
    /// (the retry gets an in-flight `409`) until the TTL elapses.
    unlock_on_drop: bool,
}

impl InFlightLockGuard {
    fn new(store: Arc<dyn IdempotencyStore>, key: String, owner: String) -> Self {
        Self {
            store,
            key,
            owner,
            // Fail closed by default: only the explicit completion paths below
            // arm the unlock. See the field doc above.
            unlock_on_drop: false,
        }
    }

    fn unlock_now(&mut self) {
        // Unconditional: this is called only on observed-complete outcomes, and
        // because the guard now defaults to *not* unlocking on drop, the unlock
        // must happen here regardless of the current flag value. `unlock_owned`
        // is owner-checked and idempotent, so a redundant call is harmless.
        self.store.unlock_owned(&self.key, &self.owner);
        self.unlock_on_drop = false;
    }

    const fn keep_locked_until_ttl(&mut self) {
        self.unlock_on_drop = false;
    }
}

impl Drop for InFlightLockGuard {
    fn drop(&mut self) {
        if self.unlock_on_drop {
            self.store.unlock_owned(&self.key, &self.owner);
        }
    }
}

#[derive(Clone)]
pub(crate) struct DeferredIdempotencyCommit {
    inner: Arc<Mutex<Option<DeferredIdempotencyState>>>,
}

struct DeferredIdempotencyState {
    store: Arc<dyn IdempotencyStore>,
    storage_key: String,
    key_context: StorageKeyContext,
    alias_storage_keys: Vec<String>,
    primary_replay_after_guard_denial: bool,
    idempotency_key: String,
    record: IdempotencyRecord,
    body_hash: Vec<u8>,
    ttl: Duration,
    lock_guard: InFlightLockGuard,
}

impl DeferredIdempotencyCommit {
    fn new(state: DeferredIdempotencyState) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(state))),
        }
    }

    fn commit_with_final_headers(&self, headers: &HeaderMap) -> Result<(), IdempotencyStoreError> {
        let Some(mut state) = self
            .inner
            .lock()
            .expect("deferred idempotency commit lock poisoned")
            .take()
        else {
            return Ok(());
        };

        state.record.headers = extract_finalized_session_replay_headers(headers);
        let primary_scope = if state.primary_replay_after_guard_denial {
            FINALIZED_SESSION_OLD_SCOPE
        } else {
            FINALIZED_SESSION_CURRENT_SCOPE
        };
        let primary_record = finalized_session_record(state.record.clone(), primary_scope);
        if let Err(error) = state.store.try_set(
            &state.storage_key,
            primary_record,
            state.body_hash.clone(),
            state.ttl,
        ) {
            tracing::error!(
                idempotency.key = %state.idempotency_key,
                error = %error,
                "Deferred idempotency persistence failed after finalized session response; failing closed"
            );
            state.lock_guard.keep_locked_until_ttl();
            return Err(error);
        }
        let alias_record = finalized_session_record(state.record, FINALIZED_SESSION_CURRENT_SCOPE);
        for storage_key in state.alias_storage_keys {
            if let Err(error) = state.store.try_set(
                &storage_key,
                alias_record.clone(),
                state.body_hash.clone(),
                state.ttl,
            ) {
                tracing::error!(
                    idempotency.key = %state.idempotency_key,
                    error = %error,
                    "Deferred idempotency persistence failed after finalized session response; failing closed"
                );
                state.lock_guard.keep_locked_until_ttl();
                return Err(error);
            }
        }
        state.lock_guard.unlock_now();
        Ok(())
    }

    fn add_session_alias(&self, session_id: Option<&str>, primary_replay_after_guard_denial: bool) {
        let mut guard = self
            .inner
            .lock()
            .expect("deferred idempotency commit lock poisoned");
        let Some(state) = guard.as_mut() else {
            return;
        };

        let storage_key = state.key_context.storage_key(session_id);
        if primary_replay_after_guard_denial && storage_key != state.storage_key {
            state.primary_replay_after_guard_denial = true;
        }
        if storage_key != state.storage_key
            && !state
                .alias_storage_keys
                .iter()
                .any(|existing| existing == &storage_key)
        {
            state.alias_storage_keys.push(storage_key);
        }
        drop(guard);
    }

    fn keep_locked_until_ttl(&self) {
        let Some(mut state) = self
            .inner
            .lock()
            .expect("deferred idempotency commit lock poisoned")
            .take()
        else {
            return;
        };
        state.lock_guard.keep_locked_until_ttl();
    }
}

pub(crate) fn finalize_deferred_session_commit(
    response: &mut Response<Body>,
) -> Result<(), IdempotencyStoreError> {
    let Some(commit) = response
        .extensions_mut()
        .remove::<DeferredIdempotencyCommit>()
    else {
        return Ok(());
    };
    commit.commit_with_final_headers(response.headers())
}

pub(crate) fn add_deferred_session_replay_key(
    response: &Response<Body>,
    session_id: Option<&str>,
    primary_replay_after_guard_denial: bool,
) {
    if let Some(commit) = response.extensions().get::<DeferredIdempotencyCommit>() {
        commit.add_session_alias(session_id, primary_replay_after_guard_denial);
    }
}

pub(crate) fn keep_deferred_session_commit_locked(response: &mut Response<Body>) {
    if let Some(commit) = response
        .extensions_mut()
        .remove::<DeferredIdempotencyCommit>()
    {
        commit.keep_locked_until_ttl();
    }
}

fn request_idempotency_key(req: &Request<Body>) -> Option<String> {
    let key = req
        .headers()
        .get(IDEMPOTENCY_KEY_HEADER)?
        .to_str()
        .unwrap_or("");
    (!key.is_empty()).then(|| key.to_owned())
}

fn in_flight_lock_owner() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn prepare_idempotency_request(
    idempotency_key: String,
    req: Request<Body>,
) -> Result<PreparedIdempotencyRequest, Response<Body>> {
    let (mut parts, body) = req.into_parts();
    let key_context = StorageKeyContext::from_parts(idempotency_key.clone(), &parts);
    let session_id = storage_session_id_for_parts(&parts).await;
    let storage_key = key_context.storage_key(session_id.as_deref());
    let stale_cookie_storage_key = stale_cookie_session_id_for_parts(&parts).and_then(|id| {
        let key = key_context.storage_key(Some(&id));
        (key != storage_key).then_some(key)
    });
    parts.extensions.insert(IdempotencyContext::new(
        idempotency_key.clone(),
        storage_key.clone(),
    ));
    let session = parts.extensions.get::<crate::session::Session>().cloned();
    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .map(axum::http::HeaderValue::as_bytes);
    let body_limit = parts
        .extensions
        .get::<crate::security::config::UploadConfig>()
        .map_or(DEFAULT_REQUEST_BODY_LIMIT, |c| c.max_request_size_bytes);
    let body_bytes = axum::body::to_bytes(body, body_limit)
        .await
        .map_err(|_| request_body_too_large_response())?;
    let body_hash = compute_body_hash(&body_bytes, content_type);

    Ok(PreparedIdempotencyRequest {
        idempotency_key,
        storage_key,
        stale_cookie_storage_key,
        key_context,
        body_hash,
        session,
        parts,
        body_bytes,
    })
}

fn lookup_prepared_entry(
    store: &dyn IdempotencyStore,
    prepared: &PreparedIdempotencyRequest,
) -> Result<Option<IdempotencyEntry>, IdempotencyStoreError> {
    if let Some(key) = prepared.stale_cookie_storage_key.as_deref()
        && let Some(entry) = store.try_get(key)?
    {
        return Ok(Some(entry));
    }

    store.try_get(&prepared.storage_key)
}

fn stale_cookie_fallback_in_flight(
    store: &dyn IdempotencyStore,
    prepared: &PreparedIdempotencyRequest,
    in_flight_ttl: Duration,
) -> bool {
    let Some(key) = prepared.stale_cookie_storage_key.as_deref() else {
        return false;
    };

    let owner = in_flight_lock_owner();
    if store.try_lock_owned(key, &owner, in_flight_ttl) {
        store.unlock_owned(key, &owner);
        false
    } else {
        true
    }
}

fn cacheable_response_record(
    status: u16,
    headers: &HeaderMap,
    body: &Bytes,
    metadata: Vec<(String, Vec<u8>)>,
) -> IdempotencyRecord {
    IdempotencyRecord {
        status,
        headers: extract_replay_headers(headers),
        body: body.to_vec(),
        metadata,
    }
}

async fn handle_idempotent_request<S>(
    mut inner: S,
    config: IdempotencyRequestConfig,
    req: Request<Body>,
) -> Result<Response<Body>, std::convert::Infallible>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let IdempotencyRequestConfig {
        store,
        ttl,
        in_flight_ttl,
        replay_through_inner,
        fail_closed_on_replay,
        metrics,
    } = config;

    if !is_mutating_method(req.method()) {
        return inner.call(req).await;
    }

    let Some(idempotency_key) = request_idempotency_key(&req) else {
        return inner.call(req).await;
    };

    let prepared = match prepare_idempotency_request(idempotency_key, req).await {
        Ok(prepared) => prepared,
        Err(response) => return Ok(response),
    };

    // ── Cache hit ──────────────────────────────────────────────────────────
    match lookup_prepared_entry(store.as_ref(), &prepared) {
        Ok(Some(entry)) => {
            return replay_cache_hit(
                &mut inner,
                entry,
                prepared,
                metrics.as_ref(),
                replay_through_inner,
                fail_closed_on_replay,
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(
                idempotency.key = %prepared.idempotency_key,
                error = %error,
                "Idempotency lookup failed; failing closed"
            );
            return Ok(persistence_failed_response());
        }
    }

    if stale_cookie_fallback_in_flight(store.as_ref(), &prepared, in_flight_ttl) {
        tracing::debug!(
            idempotency.key = %prepared.idempotency_key,
            "Stale session cookie idempotency key already in flight — returning 409"
        );
        metrics
            .as_ref()
            .inspect(|m| m.record_idempotency_conflict());
        return Ok(in_flight_conflict_response());
    }

    // ── In-flight check (concurrent duplicate) ─────────────────────────────
    let lock_owner = in_flight_lock_owner();
    if !store.try_lock_owned(&prepared.storage_key, &lock_owner, in_flight_ttl) {
        tracing::debug!(
            idempotency.key = %prepared.idempotency_key,
            "Idempotency key already in flight — returning 409"
        );
        metrics
            .as_ref()
            .inspect(|m| m.record_idempotency_conflict());
        return Ok(in_flight_conflict_response());
    }
    let mut lock_guard =
        InFlightLockGuard::new(store.clone(), prepared.storage_key.clone(), lock_owner);

    // Double-check after acquiring the lock: a concurrent request may have
    // completed between our miss check and lock acquisition.
    match lookup_prepared_entry(store.as_ref(), &prepared) {
        Ok(Some(entry)) => {
            lock_guard.unlock_now();
            return replay_cache_hit(
                &mut inner,
                entry,
                prepared,
                metrics.as_ref(),
                replay_through_inner,
                fail_closed_on_replay,
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => {
            lock_guard.keep_locked_until_ttl();
            tracing::error!(
                idempotency.key = %prepared.idempotency_key,
                error = %error,
                "Idempotency lookup failed after lock acquisition; failing closed"
            );
            return Ok(persistence_failed_response());
        }
    }

    handle_cache_miss(inner, store, ttl, prepared, metrics.as_ref(), lock_guard).await
}

async fn handle_cache_miss<S>(
    mut inner: S,
    store: Arc<dyn IdempotencyStore>,
    ttl: Duration,
    prepared: PreparedIdempotencyRequest,
    metrics: Option<&crate::middleware::MetricsCollector>,
    mut lock_guard: InFlightLockGuard,
) -> Result<Response<Body>, std::convert::Infallible>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let PreparedIdempotencyRequest {
        idempotency_key,
        storage_key,
        key_context,
        body_hash,
        session,
        parts,
        body_bytes,
        ..
    } = prepared;

    tracing::debug!(
        idempotency.key = %idempotency_key,
        "Idempotency cache miss — forwarding to handler"
    );

    let response = inner
        .call(Request::from_parts(parts, Body::from(body_bytes)))
        .await?;
    let (mut resp_parts, resp_body) = response.into_parts();

    // Collect up to the cache cap; stream oversized bodies through without
    // buffering to avoid materialising large responses in memory.
    let resp_bytes = match collect_response_for_cache(resp_body).await {
        CollectedResponseBody::StreamError(passthrough_body) => {
            lock_guard.unlock_now();
            tracing::warn!(
                idempotency.key = %idempotency_key,
                "I/O error reading response body; passing the body error through without storing idempotency entry"
            );
            return Ok(Response::from_parts(resp_parts, passthrough_body));
        }
        CollectedResponseBody::TooLarge {
            passthrough_body, ..
        } => {
            // Body exceeded MAX_CACHEABLE_RESPONSE_BODY — stream through.
            lock_guard.unlock_now();
            tracing::debug!(
                idempotency.key = %idempotency_key,
                limit_bytes = MAX_CACHEABLE_RESPONSE_BODY,
                "Response body exceeded cache limit; streaming through without caching"
            );
            return Ok(Response::from_parts(resp_parts, passthrough_body));
        }
        CollectedResponseBody::Cacheable(bytes) => bytes,
    };

    let replay_metadata = resp_parts
        .extensions
        .remove::<IdempotencyReplayMetadata>()
        .map_or_else(Vec::new, IdempotencyReplayMetadata::into_entries);
    let cache_committed_error = resp_parts
        .extensions
        .remove::<IdempotencyCacheCommittedErrorResponse>()
        .is_some();

    // Cache successful 2xx/3xx responses and explicit "mutation committed"
    // errors; store before unlocking so concurrent duplicates still see a
    // locked key rather than racing to re-execute the handler.
    let status = resp_parts.status.as_u16();
    if (200u32..400).contains(&u32::from(status)) || cache_committed_error {
        let session_mutated = if let Some(session) = &session {
            session.has_pending_changes().await
        } else {
            false
        };
        let record =
            cacheable_response_record(status, &resp_parts.headers, &resp_bytes, replay_metadata);
        if session_mutated {
            tracing::debug!(
                idempotency.key = %idempotency_key,
                "Session changed during idempotent request; deferring cache write until SessionLayer finalizes Set-Cookie"
            );
            resp_parts.extensions.insert(DeferredIdempotencyCommit::new(
                DeferredIdempotencyState {
                    store,
                    storage_key,
                    key_context,
                    alias_storage_keys: Vec::new(),
                    primary_replay_after_guard_denial: false,
                    idempotency_key,
                    record,
                    body_hash,
                    ttl,
                    lock_guard,
                },
            ));
            if let Some(m) = metrics {
                m.record_idempotency_miss();
            }
            return Ok(Response::from_parts(resp_parts, Body::from(resp_bytes)));
        }
        if let Err(error) = store.try_set(&storage_key, record, body_hash, ttl) {
            tracing::error!(
                idempotency.key = %idempotency_key,
                error = %error,
                "Idempotency persistence failed after handler success; failing closed"
            );
            lock_guard.keep_locked_until_ttl();
            return Ok(persistence_failed_response());
        }
    }
    lock_guard.unlock_now();

    if let Some(m) = metrics {
        m.record_idempotency_miss();
    }

    // Reconstruct from original parts — preserves set-cookie and extensions.
    Ok(Response::from_parts(resp_parts, Body::from(resp_bytes)))
}

/// Collect response body bytes up to `MAX_CACHEABLE_RESPONSE_BODY`.
///
/// Returns:
/// - `Cacheable(bytes)` — body is within the limit and fully collected
/// - `TooLarge(body)` — body exceeded the limit; the returned `Body` chains the
///   already-read bytes with the remaining stream for pass-through delivery
/// - `StreamError(body)` — reading the body stream failed; the returned `Body`
///   preserves the already-read bytes and then yields the original stream error
enum CollectedResponseBody {
    Cacheable(Bytes),
    TooLarge {
        passthrough_body: Body,
        #[cfg_attr(not(test), allow(dead_code))]
        buffered_len: usize,
    },
    StreamError(Body),
}

async fn collect_response_for_cache(body: Body) -> CollectedResponseBody {
    collect_response_for_cache_with_limit(body, MAX_CACHEABLE_RESPONSE_BODY).await
}

async fn collect_response_for_cache_with_limit(body: Body, limit: usize) -> CollectedResponseBody {
    let mut buf = Vec::<u8>::new();
    let mut data_stream = body.into_data_stream();
    loop {
        match data_stream.next().await {
            None => break,
            Some(Err(err)) => {
                let leading = Bytes::from(buf);
                let passthrough =
                    Body::from_stream(futures::stream::iter(vec![Ok(leading), Err(err)]));
                return CollectedResponseBody::StreamError(passthrough);
            }
            Some(Ok(chunk)) => {
                if chunk.len() > limit.saturating_sub(buf.len()) {
                    let buffered_len = buf.len();
                    let mut leading_chunks = Vec::with_capacity(2);
                    if !buf.is_empty() {
                        leading_chunks.push(Ok::<Bytes, axum::Error>(Bytes::from(buf)));
                    }
                    leading_chunks.push(Ok::<Bytes, axum::Error>(chunk));
                    let passthrough =
                        Body::from_stream(futures::stream::iter(leading_chunks).chain(data_stream));
                    return CollectedResponseBody::TooLarge {
                        passthrough_body: passthrough,
                        buffered_len,
                    };
                }
                buf.extend_from_slice(&chunk);
            }
        }
    }
    CollectedResponseBody::Cacheable(Bytes::from(buf))
}

async fn replay_cache_hit<S>(
    inner: &mut S,
    entry: IdempotencyEntry,
    prepared: PreparedIdempotencyRequest,
    metrics: Option<&crate::middleware::MetricsCollector>,
    replay_through_inner: bool,
    fail_closed_on_replay: bool,
) -> Result<Response<Body>, std::convert::Infallible>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let PreparedIdempotencyRequest {
        idempotency_key,
        body_hash,
        mut parts,
        body_bytes,
        ..
    } = prepared;

    if entry.body_hash != body_hash {
        tracing::debug!(
            idempotency.key = %idempotency_key,
            "Idempotency payload mismatch — returning 422"
        );
        return Ok(Response::builder()
            .status(StatusCode::UNPROCESSABLE_ENTITY)
            .body(Body::from("idempotency key reused with different payload"))
            .unwrap());
    }

    if fail_closed_on_replay {
        tracing::warn!(
            idempotency.key = %idempotency_key,
            "Idempotency cache hit reached a route without an inner replay stop; failing closed"
        );
        return Ok(replay_requires_inner_stop_response());
    }

    tracing::debug!(
        idempotency.key = %idempotency_key,
        idempotency.replayed = true,
        "Idempotency cache hit — replaying stored response"
    );

    if let Some(m) = metrics {
        m.record_idempotency_hit();
    }

    let replay = IdempotencyReplayResponse {
        record: entry.record,
    };
    if replay_through_inner {
        parts.extensions.insert(replay);
        return inner
            .call(Request::from_parts(parts, Body::from(body_bytes)))
            .await;
    }

    Ok(replay.into_response())
}

fn response_from_record(record: IdempotencyRecord) -> Response<Body> {
    let mut builder = Response::builder().status(record.status);
    for (name, value) in &record.headers {
        builder = builder.header(name.as_str(), value.as_slice());
    }
    builder
        .header(X_IDEMPOTENT_REPLAYED, "true")
        .body(Body::from(record.body))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::AUTHORIZATION;
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[derive(Clone, Default)]
    struct RecordingStore {
        keys: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingStore {
        fn keys(&self) -> Vec<String> {
            self.keys
                .lock()
                .expect("recording store lock poisoned")
                .clone()
        }

        fn record_key(&self, key: &str) {
            self.keys
                .lock()
                .expect("recording store lock poisoned")
                .push(key.to_owned());
        }
    }

    impl IdempotencyStore for RecordingStore {
        fn get(&self, key: &str) -> Option<IdempotencyEntry> {
            self.record_key(key);
            None
        }

        fn set(&self, key: &str, _record: IdempotencyRecord, _body_hash: Vec<u8>, _ttl: Duration) {
            self.record_key(key);
        }

        fn try_lock(&self, key: &str, _lock_ttl: Duration) -> bool {
            self.record_key(key);
            true
        }

        fn unlock(&self, key: &str) {
            self.record_key(key);
        }
    }

    fn idempotent_post(path: &str, key: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(IDEMPOTENCY_KEY_HEADER, key)
            .body(Body::from(body))
            .expect("request builder should be valid")
    }

    fn session_with_user(session_id: &str, user_id: &str) -> crate::session::Session {
        let mut data = HashMap::new();
        data.insert("user_id".to_owned(), user_id.to_owned());
        crate::session::Session::new_for_test(session_id.to_owned(), data)
    }

    fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().fold(
            String::with_capacity(bytes.as_ref().len() * 2),
            |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            },
        )
    }

    fn expected_principal_digest(session_id: Option<&str>) -> String {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"authorization:");
        hasher.update(b"\nsession:");
        if let Some(session_id) = session_id {
            hasher.update(session_id.as_bytes());
        }
        hex_lower(hasher.finalize())
    }

    fn expected_storage_key(
        method: &str,
        path: &str,
        session_id: Option<&str>,
        idempotency_key: &str,
    ) -> String {
        use sha2::Digest as _;
        let principal = expected_principal_digest(session_id);
        let mut hasher = sha2::Sha256::new();
        push_storage_key_component(&mut hasher, "method", method.as_bytes());
        push_storage_key_component(&mut hasher, "target", path.as_bytes());
        push_storage_key_component(&mut hasher, "scope-header-count", b"0");
        push_storage_key_component(&mut hasher, "principal", principal.as_bytes());
        push_storage_key_component(&mut hasher, "idempotency-key", idempotency_key.as_bytes());
        format!("v2:{}", hex_lower(hasher.finalize()))
    }

    #[test]
    fn idempotency_context_clones_share_mutation_discriminator_sequence() {
        let context = IdempotencyContext::new("client-key".to_owned(), "scoped-key".to_owned());
        let cloned = context.clone();

        assert_eq!(context.next_mutation_discriminator(), "0");
        assert_eq!(cloned.next_mutation_discriminator(), "1");
        assert_eq!(context.next_mutation_discriminator(), "2");
    }

    #[test]
    fn memory_lock_unlock_owned_does_not_release_newer_owner() {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(60));

        assert!(store.try_lock_owned("key", "owner-a", Duration::from_millis(5)));
        std::thread::sleep(Duration::from_millis(20));
        assert!(store.try_lock_owned("key", "owner-b", Duration::from_secs(60)));

        store.unlock_owned("key", "owner-a");
        assert!(
            !store.try_lock_owned("key", "owner-c", Duration::from_secs(60)),
            "stale owners must not release a newer in-flight lock"
        );

        store.unlock_owned("key", "owner-b");
        assert!(store.try_lock_owned("key", "owner-c", Duration::from_secs(60)));
    }

    #[test]
    fn in_flight_guard_holds_lock_when_dropped_without_explicit_unlock() {
        // Simulates the inner handler future being cancelled (by the outer
        // request-timeout layer) or unwound by a panic: the guard is dropped
        // without any of the explicit completion paths calling `unlock_now`.
        // The lock must stay held so a retry carrying the same Idempotency-Key
        // cannot re-run a mutation whose side effect may already have committed.
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
        assert!(store.try_lock_owned("key", "owner-a", Duration::from_secs(60)));

        {
            let _guard =
                InFlightLockGuard::new(store.clone(), "key".to_owned(), "owner-a".to_owned());
            // Dropped here with no explicit unlock — fail closed.
        }

        assert!(
            !store.try_lock_owned("key", "owner-b", Duration::from_secs(60)),
            "a cancelled/panicked handler must leave the in-flight lock held until its TTL"
        );
    }

    #[test]
    fn in_flight_guard_releases_lock_on_explicit_unlock() {
        // The normal completion paths call `unlock_now`, which must release the
        // lock immediately so a subsequent distinct request can proceed.
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
        assert!(store.try_lock_owned("key", "owner-a", Duration::from_secs(60)));

        {
            let mut guard =
                InFlightLockGuard::new(store.clone(), "key".to_owned(), "owner-a".to_owned());
            guard.unlock_now();
        }

        assert!(
            store.try_lock_owned("key", "owner-b", Duration::from_secs(60)),
            "an explicitly unlocked guard must release the in-flight lock"
        );
    }

    #[tokio::test]
    async fn response_body_stream_errors_are_not_replaced_with_empty_success() {
        let store = Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
        let service = IdempotencyLayer::new(store).layer(tower::service_fn(
            |_req: Request<Body>| async move {
                let stream = futures::stream::iter(vec![
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(b"partial")),
                    Err(std::io::Error::other("stream failed")),
                ]);
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from_stream(stream))
                        .expect("response builder should be valid"),
                )
            },
        ));

        let response = service
            .oneshot(idempotent_post("/stream", "stream-key", "same"))
            .await
            .expect("request should complete");

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await;
        assert!(
            body.is_err(),
            "idempotency middleware must preserve response body stream errors"
        );
    }

    #[tokio::test]
    async fn collect_response_checks_chunk_size_before_buffering_past_cap() {
        let chunk = Bytes::from(vec![b'x'; 64]);
        match collect_response_for_cache_with_limit(Body::from(chunk), 10).await {
            CollectedResponseBody::TooLarge {
                passthrough_body,
                buffered_len,
            } => {
                assert_eq!(
                    buffered_len, 0,
                    "single over-cap chunk must not be copied into the cache buffer first"
                );
                let delivered = axum::body::to_bytes(passthrough_body, usize::MAX)
                    .await
                    .expect("passthrough body should collect");
                assert_eq!(delivered.len(), 64);
            }
            CollectedResponseBody::Cacheable(_) => {
                panic!("over-cap chunk must not be considered cacheable")
            }
            CollectedResponseBody::StreamError(_) => panic!("body should not stream-error"),
        }
    }

    #[tokio::test]
    async fn cookie_session_extension_scopes_idempotency_storage_key() {
        let store = Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(60)));
        let mut service = IdempotencyLayer::new(store).layer(tower::service_fn(
            |req: Request<Body>| async move {
                let session = req
                    .extensions()
                    .get::<crate::session::Session>()
                    .cloned()
                    .expect("session extension should be present");
                let user_id = session
                    .get("user_id")
                    .await
                    .expect("session should contain user_id");
                Ok::<_, Infallible>(Response::new(Body::from(user_id)))
            },
        ));

        let mut alice_req = idempotent_post("/orders", "shared-key", "same");
        alice_req
            .extensions_mut()
            .insert(session_with_user("session-alice", "alice"));
        let alice_response = service
            .ready()
            .await
            .expect("service should be ready")
            .call(alice_req)
            .await
            .expect("alice request should complete");
        let alice_body = axum::body::to_bytes(alice_response.into_body(), usize::MAX)
            .await
            .expect("alice body should collect");
        assert_eq!(alice_body, Bytes::from_static(b"alice"));

        let mut bob_req = idempotent_post("/orders", "shared-key", "same");
        bob_req
            .extensions_mut()
            .insert(session_with_user("session-bob", "bob"));
        let bob_response = service
            .ready()
            .await
            .expect("service should be ready")
            .call(bob_req)
            .await
            .expect("bob request should complete");
        assert!(
            bob_response.headers().get(X_IDEMPOTENT_REPLAYED).is_none(),
            "a different cookie-backed session must not replay another user's response"
        );
        let bob_body = axum::body::to_bytes(bob_response.into_body(), usize::MAX)
            .await
            .expect("bob body should collect");
        assert_eq!(bob_body, Bytes::from_static(b"bob"));
    }

    #[tokio::test]
    async fn storage_key_hashes_length_delimited_components() {
        let observed_store = RecordingStore::default();
        let service = IdempotencyLayer::new(Arc::new(observed_store.clone())).layer(
            tower::service_fn(|_req: Request<Body>| async {
                Ok::<_, Infallible>(Response::new(Body::from("ok")))
            }),
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri("/payments")
            .header(IDEMPOTENCY_KEY_HEADER, "pay-once")
            .header(AUTHORIZATION, "Bearer stable-token")
            .body(Body::from("same"))
            .expect("request builder should be valid");

        let response = service
            .oneshot(request)
            .await
            .expect("request should complete");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should collect");
        assert_eq!(body, Bytes::from_static(b"ok"));

        let keys = observed_store.keys();
        let storage_key = keys.first().expect("storage key should be recorded");
        assert_eq!(
            storage_key,
            &expected_storage_key("POST", "/payments", None, "pay-once")
        );
        assert!(!storage_key.contains("/payments"));
        assert!(!storage_key.contains("pay-once"));
    }

    #[tokio::test]
    async fn forwarded_request_carries_scoped_idempotency_context() {
        let observed = Arc::new(Mutex::new(None::<(String, String)>));
        let observed_context = observed.clone();
        let service = IdempotencyLayer::new(Arc::new(MemoryIdempotencyStore::new(
            Duration::from_secs(60),
        )))
        .layer(tower::service_fn(move |req: Request<Body>| {
            let observed_context = observed_context.clone();
            async move {
                let context = req
                    .extensions()
                    .get::<IdempotencyContext>()
                    .cloned()
                    .expect("idempotency context should be available to inner handlers");
                *observed_context
                    .lock()
                    .expect("observed context lock poisoned") =
                    Some((context.key().to_owned(), context.scoped_key().to_owned()));
                Ok::<_, Infallible>(Response::new(Body::from("ok")))
            }
        }));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/payments")
            .header(IDEMPOTENCY_KEY_HEADER, "pay-once")
            .header(AUTHORIZATION, "Bearer stable-token")
            .body(Body::from("same"))
            .expect("request builder should be valid");

        let response = service
            .oneshot(request)
            .await
            .expect("request should complete");
        assert_eq!(response.status(), StatusCode::OK);

        let observed = observed
            .lock()
            .expect("observed context lock poisoned")
            .clone()
            .expect("inner handler should record idempotency context");
        assert_eq!(observed.0, "pay-once");
        assert_eq!(
            observed.1,
            expected_storage_key("POST", "/payments", None, "pay-once")
        );
    }
}
