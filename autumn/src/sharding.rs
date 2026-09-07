//! Horizontal database sharding.
//!
//! Autumn routes sharded data in two steps: a routing key (typically the
//! tenant id) hashes onto a fixed set of [`SLOT_COUNT`] (16384) **logical
//! slots** — the same constant Redis Cluster and Valkey use — and each
//! slot maps to one physical shard via the `[[database.shards]]`
//! configuration. The key→slot hash is a permanent contract — it is
//! deterministic across processes, replicas, and Autumn versions — while
//! the slot→shard map is plain configuration. Resharding therefore means
//! moving whole slots between shards and flipping the map, never
//! rehashing keys.
//!
//! Each shard is a full [`DatabaseTopology`] (primary + optional read
//! replica), so the primary/replica story composes with sharding.
//!
//! Framework state (jobs, scheduler locks, sessions, feature flags) is
//! **not** sharded; it lives on the control topology configured by
//! `database.primary_url`/`database.url`.
//!
//! # Example
//!
//! ```toml
//! [database]
//! primary_url = "postgres://db-control/app"
//!
//! [[database.shards]]
//! name = "shard0"
//! primary_url = "postgres://db-shard0/app"
//! slots = ["0-8191"]
//!
//! [[database.shards]]
//! name = "shard1"
//! primary_url = "postgres://db-shard1/app"
//! slots = ["8192-16383"]
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::db::RuntimeConnection;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;

pub use crate::config::SLOT_COUNT;
use crate::config::{ConfigError, DatabaseConfig, ReplicaFallback};
use crate::db::{DatabaseTopology, PoolError};
use crate::error::AutumnError;

/// Index of a physical shard within the configured shard set.
///
/// Stable only for a given configuration; use [`Shard::name`] for
/// identity that survives configuration edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId(pub usize);

/// A logical routing slot in <code>0..[SLOT_COUNT]</code>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u16);

/// Borrowed routing key.
///
/// Tenant ids in Autumn are strings, so [`ShardKey::Str`] is the common
/// variant; `Int` and `Bytes` cover numeric primary keys and UUIDs
/// (`ShardKey::from(uuid.as_bytes())`).
#[derive(Debug, Clone, Copy)]
pub enum ShardKey<'a> {
    /// Numeric key (e.g. a `BIGINT` primary key).
    Int(i64),
    /// Textual key (e.g. a tenant id).
    Str(&'a str),
    /// Raw bytes (e.g. a UUID).
    Bytes(&'a [u8]),
}

impl From<i64> for ShardKey<'_> {
    fn from(key: i64) -> Self {
        Self::Int(key)
    }
}

impl From<i32> for ShardKey<'_> {
    fn from(key: i32) -> Self {
        Self::Int(i64::from(key))
    }
}

impl<'a> From<&'a str> for ShardKey<'a> {
    fn from(key: &'a str) -> Self {
        Self::Str(key)
    }
}

impl<'a> From<&'a String> for ShardKey<'a> {
    fn from(key: &'a String) -> Self {
        Self::Str(key)
    }
}

impl<'a> From<&'a [u8]> for ShardKey<'a> {
    fn from(key: &'a [u8]) -> Self {
        Self::Bytes(key)
    }
}

impl<'a> From<&'a [u8; 16]> for ShardKey<'a> {
    fn from(key: &'a [u8; 16]) -> Self {
        Self::Bytes(key)
    }
}

// ── Deterministic key hashing ────────────────────────────────────────────────
//
// The key→slot function is a PERMANENT CONTRACT: every process, replica,
// and future Autumn version must route the same key to the same slot, or
// data written by one replica becomes invisible to another. That rules out
// std's SipHash (randomly keyed per process). FNV-1a and splitmix64 are
// fixed, well-known functions; the golden-vector tests below pin their
// output forever.

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// splitmix64 finalizer — mixes integer keys so that sequential ids
/// spread uniformly across slots.
const fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Deterministic 64-bit hash of a routing key.
#[must_use]
fn key_hash64(key: ShardKey<'_>) -> u64 {
    match key {
        #[allow(clippy::cast_sign_loss)]
        ShardKey::Int(value) => splitmix64(value as u64),
        ShardKey::Str(value) => fnv1a_64(value.as_bytes()),
        ShardKey::Bytes(value) => fnv1a_64(value),
    }
}

/// Map a routing key onto a logical slot in <code>0..[SLOT_COUNT]</code>.
///
/// This function is deterministic across processes and versions; see the
/// module docs.
#[must_use]
pub fn slot_for_key(key: ShardKey<'_>) -> SlotId {
    let hash = key_hash64(key);
    #[allow(clippy::cast_possible_truncation)]
    SlotId((hash % u64::from(SLOT_COUNT)) as u16)
}

// ── Router ───────────────────────────────────────────────────────────────────

/// Pluggable shard routing strategy.
///
/// The default [`HashShardRouter`] hashes the key onto a logical slot and
/// resolves the slot's owner from configuration. Implement this trait for
/// directory/lookup routing (e.g. a control-plane table mapping tenants to
/// shards, with hot "whale" tenants pinned to dedicated shards) and
/// install it with
/// [`AppBuilder::with_shard_router`](crate::app::AppBuilder::with_shard_router).
///
/// Routing is async so directory routers can consult a cache or the
/// control database. Custom routers can still compose with the hash via
/// [`ShardSet::slot_for_key`] and [`ShardSet::shard_for_slot`].
pub trait ShardRouter: Send + Sync + 'static {
    /// Resolve the shard that owns `key`.
    fn route<'a>(
        &'a self,
        key: ShardKey<'a>,
        shards: &'a ShardSet,
    ) -> futures::future::BoxFuture<'a, Result<ShardId, AutumnError>>;
}

/// `Arc<R>` routes through its inner router. This lets a caller build a single
/// `Arc<DirectoryShardRouter>`, share one clone with
/// [`DirectoryShardRouter::spawn_invalidation_listener`] (which takes an
/// `Arc<Self>`) and install another clone via `AppBuilder::with_shard_router` —
/// both then read and invalidate the **same** cache. Without it the
/// manually-installed router
/// and the listener would hold separate caches, so directory re-pins would stay
/// stale until the TTL despite the documented manual-listener path.
impl<R: ShardRouter + ?Sized> ShardRouter for Arc<R> {
    fn route<'a>(
        &'a self,
        key: ShardKey<'a>,
        shards: &'a ShardSet,
    ) -> futures::future::BoxFuture<'a, Result<ShardId, AutumnError>> {
        (**self).route(key, shards)
    }
}

/// Default router: key → logical slot (deterministic hash) → shard
/// (configured slot map).
#[derive(Debug, Default, Clone, Copy)]
pub struct HashShardRouter;

impl ShardRouter for HashShardRouter {
    fn route<'a>(
        &'a self,
        key: ShardKey<'a>,
        shards: &'a ShardSet,
    ) -> futures::future::BoxFuture<'a, Result<ShardId, AutumnError>> {
        let slot = shards.slot_for_key(key);
        Box::pin(std::future::ready(
            shards
                .inner
                .slot_map
                .get(usize::from(slot.0))
                .map(|&idx| ShardId(idx))
                .ok_or_else(|| {
                    AutumnError::service_unavailable_msg(format!(
                        "slot {} has no shard assigned (slot map inconsistent)",
                        slot.0
                    ))
                }),
        ))
    }
}

/// A [`ShardRouter`] that consults an explicit `_autumn_shard_directory`
/// table on the control database, falling back to the hash router for any
/// tenant without a directory row.
///
/// This is the routing half of "move a tenant to a specific shard": a row in
/// `_autumn_shard_directory(tenant_key, shard_name)` pins that tenant to a
/// named shard regardless of where the slot hash would place it. Tenants with
/// no row route by [`HashShardRouter`], so the directory only needs entries
/// for relocated/"whale" tenants.
///
/// Directory **hits** (a real pin) are cached for
/// [`DEFAULT_DIRECTORY_CACHE_TTL`] so steady-state routing of pinned tenants
/// issues no control-DB query. **Misses are not cached** — an unpinned tenant
/// re-reads the directory on every route. This keeps the move workflow safe:
/// once an operator inserts a directory row, no other process can keep routing
/// that tenant to its old hash shard from a stale cached miss (there is no
/// cross-process invalidation), so `move-slot --confirm` won't delete rows that
/// late writes landed on the source. After changing a directory row, call
/// [`invalidate`](Self::invalidate) for that key so the next route re-reads it.
/// (NOTIFY-based cross-process invalidation is a planned follow-up; today the
/// TTL bounds hit staleness and `invalidate` clears the local entry
/// immediately.)
///
/// Install with
/// [`AppBuilder::with_directory_shard_router`](crate::app::AppBuilder::with_directory_shard_router).
///
/// Only string keys are looked up in the directory (tenants are strings);
/// numeric/byte keys route straight through the fallback.
pub struct DirectoryShardRouter {
    control_pool: Pool<RuntimeConnection>,
    fallback: Arc<dyn ShardRouter>,
    cache: std::sync::RwLock<HashMap<String, DirectoryCacheEntry>>,
    ttl: std::time::Duration,
    /// `statement_timeout` (ms) applied to the control-plane directory lookup so
    /// a stuck control query / lock on `_autumn_shard_directory` fails within
    /// the configured timeout instead of hanging every tenant-routed request.
    /// `0` disables it. The router checks out a raw pooled connection (no
    /// request context), so the timeout is set explicitly here.
    statement_timeout_ms: u64,
}

/// Default time a resolved tenant→shard mapping is cached before re-reading
/// the directory table.
pub const DEFAULT_DIRECTORY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// The `_autumn_shard_directory` table migration as a standalone embedded set.
///
/// Embedded separately so the app can auto-create the table at startup when
/// directory routing is enabled (the `migrations/` copy is applied by
/// `autumn migrate` for the control plane; this mirror lets auto-migrate
/// deployments create the table without a manual migrate). The migration is
/// `CREATE TABLE IF NOT EXISTS`, so applying it from either set is idempotent.
/// Keep both copies in sync.
#[cfg(feature = "db")]
pub const SHARD_DIRECTORY_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("shard_directory_migrations");

/// The `_autumn_shard_map` table migration as a standalone embedded set.
///
/// Embedded separately so the boot-time shard-map guard can auto-create its
/// control table at startup (the `migrations/` copy is applied by
/// `autumn migrate`). The migration is `CREATE TABLE IF NOT EXISTS`, so
/// applying it from either set is idempotent. Keep both copies in sync.
#[cfg(feature = "db")]
pub const SHARD_MAP_MIGRATIONS: diesel_migrations::EmbeddedMigrations =
    diesel_migrations::embed_migrations!("shard_map_migrations");

#[derive(Clone, Copy)]
struct DirectoryCacheEntry {
    shard: ShardId,
    expires_at: std::time::Instant,
}

#[derive(diesel::QueryableByName)]
struct ShardNameRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    shard_name: String,
}

/// Postgres `LISTEN`/`NOTIFY` channel the directory trigger fires on. The
/// invalidation listener subscribes to it; the trigger
/// (`autumn_notify_shard_directory_change`, in the shard-directory migration)
/// must `pg_notify` the same channel. Keep the two in sync.
const DIRECTORY_NOTIFY_CHANNEL: &str = "autumn_shard_directory";

/// How often the invalidation listener wakes while idle to sweep expired cache
/// entries and notice a dropped LISTEN connection.
///
/// Invalidation delivery itself is event-driven — a `NOTIFY` delivered at
/// commit — so this only bounds idle housekeeping; kept well under
/// [`DEFAULT_DIRECTORY_CACHE_TTL`].
pub const DEFAULT_DIRECTORY_INVALIDATION_SWEEP_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5);

impl std::fmt::Debug for DirectoryShardRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryShardRouter")
            .field("ttl", &self.ttl)
            .field("cached_keys", &self.cache.read().map_or(0, |c| c.len()))
            .finish_non_exhaustive()
    }
}

impl DirectoryShardRouter {
    /// Build a directory router over the given control pool, falling back to
    /// [`HashShardRouter`] and using [`DEFAULT_DIRECTORY_CACHE_TTL`].
    #[must_use]
    pub fn new(control_pool: Pool<RuntimeConnection>) -> Self {
        Self::with_fallback(control_pool, Arc::new(HashShardRouter))
    }

    /// Build a directory router with an explicit fallback router and the
    /// default cache TTL.
    #[must_use]
    pub fn with_fallback(
        control_pool: Pool<RuntimeConnection>,
        fallback: Arc<dyn ShardRouter>,
    ) -> Self {
        Self {
            control_pool,
            fallback,
            cache: std::sync::RwLock::new(HashMap::new()),
            ttl: DEFAULT_DIRECTORY_CACHE_TTL,
            statement_timeout_ms: 0,
        }
    }

    /// Bound the control-plane directory lookup with `statement_timeout`
    /// (milliseconds); `0` disables it. Typically the app's configured database
    /// statement timeout, so a stuck control query fails fast instead of hanging
    /// tenant routing.
    #[must_use]
    pub const fn with_statement_timeout_ms(mut self, statement_timeout_ms: u64) -> Self {
        self.statement_timeout_ms = statement_timeout_ms;
        self
    }

    /// Override the cache TTL.
    #[must_use]
    pub const fn with_cache_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Drop the cached mapping for `tenant_key`, forcing the next route to
    /// re-read the directory. Call this after inserting, updating, or deleting
    /// that tenant's directory row.
    pub fn invalidate(&self, tenant_key: &str) {
        if let Ok(mut cache) = self.cache.write() {
            cache.remove(tenant_key);
        }
    }

    /// Drop every cached mapping.
    pub fn invalidate_all(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    /// Spawn a background task that `LISTEN`s on the control DB's
    /// `autumn_shard_directory` notification channel and invalidates this
    /// router's cached pin whenever a tenant's directory row changes.
    ///
    /// This covers writes made on other replicas or directly via operator SQL
    /// (the channel is fired by a trigger, not app code). Without it the cache
    /// only refreshes when the TTL expires; with it a re-pin during a slot move
    /// is picked up the moment it commits.
    ///
    /// Postgres delivers `NOTIFY` at **commit** (never before), so the
    /// invalidation arrives exactly when the new mapping becomes visible: a
    /// slow-committing re-pin cannot be skipped the way a timestamp-cursor poll
    /// could. The cache TTL stays the backstop for any window where the LISTEN
    /// connection is down.
    ///
    /// `control_url` is the control database URL backing this router's control
    /// pool. Must be called from within a Tokio runtime; the returned handle can
    /// be detached, and the task runs for the life of the process. The framework
    /// spawns this automatically when directory routing is enabled via the
    /// built-in path. `sweep_interval` only bounds idle housekeeping (see
    /// [`DEFAULT_DIRECTORY_INVALIDATION_SWEEP_INTERVAL`]).
    #[must_use]
    pub fn spawn_invalidation_listener(
        router: Arc<Self>,
        control_url: String,
        sweep_interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        use diesel_async::{AsyncConnection as _, RunQueryDsl as _};
        use futures::StreamExt as _;

        tokio::spawn(async move {
            loop {
                // (Re)connect and subscribe. On any failure back off for one
                // sweep interval and retry; the cache TTL backstops staleness
                // while we're disconnected.
                let Ok(mut conn) = AsyncPgConnection::establish(&control_url).await else {
                    tokio::time::sleep(sweep_interval).await;
                    continue;
                };
                if diesel::sql_query(format!("LISTEN {DIRECTORY_NOTIFY_CHANNEL}"))
                    .execute(&mut conn)
                    .await
                    .is_err()
                {
                    tokio::time::sleep(sweep_interval).await;
                    continue;
                }
                // A re-pin may have committed between losing the previous
                // connection and (re)subscribing; those NOTIFYs are gone, so drop
                // the whole cache and let it repopulate lazily from the directory.
                router.invalidate_all();

                // Drain notifications until the stream errors or ends, then fall
                // through to the outer loop and reconnect. Each idle
                // `sweep_interval` we reclaim expired entries that were never
                // looked up again (lazy eviction in `cache_get` only fires on
                // re-observation).
                let mut notifications = std::pin::pin!(conn.notifications_stream());
                loop {
                    match tokio::time::timeout(sweep_interval, notifications.next()).await {
                        Ok(Some(Ok(notification))) => router.invalidate(&notification.payload),
                        Ok(Some(Err(_)) | None) => break,
                        Err(_elapsed) => router.sweep_expired(),
                    }
                }
            }
        })
    }

    fn cache_get(&self, key: &str) -> Option<ShardId> {
        let now = std::time::Instant::now();
        {
            let cache = self.cache.read().ok()?;
            match cache.get(key) {
                Some(entry) if entry.expires_at > now => return Some(entry.shard),
                // Miss, or present-but-expired: fall through. `None` is returned
                // either way; an expired entry is additionally evicted below so a
                // long-running process doesn't retain every pinned tenant it has
                // ever looked up (the TTL bounds staleness, not memory).
                Some(_) => {}
                None => return None,
            }
        }
        // Evict the expired entry under the write lock. Re-check expiry (against
        // the same `now`) so we don't drop a fresh entry written by `cache_put`
        // between releasing the read lock and taking the write lock.
        let mut cache = self.cache.write().ok()?;
        if cache.get(key).is_some_and(|entry| entry.expires_at <= now) {
            cache.remove(key);
        }
        None
    }

    /// Drop every expired entry from the cache. Lazy eviction in `cache_get`
    /// only reclaims keys that are looked up again; this bounds memory for
    /// pinned tenants that are never re-observed. Called periodically by the
    /// invalidation listener.
    fn sweep_expired(&self) {
        if let Ok(mut cache) = self.cache.write() {
            let now = std::time::Instant::now();
            cache.retain(|_, entry| entry.expires_at > now);
        }
    }

    fn cache_put(&self, key: String, shard: ShardId) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(
                key,
                DirectoryCacheEntry {
                    shard,
                    expires_at: std::time::Instant::now() + self.ttl,
                },
            );
        }
    }

    /// Look up a tenant key in the directory table. Returns the resolved
    /// `ShardId` on a directory hit, or `None` when the tenant has no row
    /// (the caller then falls back to the hash router).
    async fn lookup_directory(
        &self,
        key: &str,
        shards: &ShardSet,
    ) -> Result<Option<ShardId>, AutumnError> {
        use diesel::OptionalExtension as _;
        use diesel_async::RunQueryDsl;

        let mut conn = self.control_pool.get().await.map_err(|e| {
            AutumnError::service_unavailable_msg(format!(
                "DirectoryShardRouter could not acquire a control connection: {e}"
            ))
        })?;

        // Bound the lookup so a stuck control query / lock doesn't hang routing.
        // Always issued (even for 0 = disabled) because the raw pooled checkout
        // can return a connection carrying a shorter route-specific timeout set
        // by a prior `Db`/repository checkout; mirror the normal checkout path,
        // which always sets `statement_timeout`.
        diesel::sql_query(format!(
            "SET statement_timeout = {}",
            self.statement_timeout_ms
        ))
        .execute(&mut conn)
        .await
        .map_err(|e| {
            AutumnError::service_unavailable_msg(format!(
                "DirectoryShardRouter could not set statement_timeout: {e}"
            ))
        })?;

        let row = diesel::sql_query(
            "SELECT shard_name FROM _autumn_shard_directory WHERE tenant_key = $1",
        )
        .bind::<diesel::sql_types::Text, _>(key)
        .get_result::<ShardNameRow>(&mut conn)
        .await
        .optional()
        .map_err(|e| {
            AutumnError::service_unavailable_msg(format!(
                "DirectoryShardRouter directory lookup failed: {e}"
            ))
        })?;

        let Some(row) = row else {
            return Ok(None);
        };

        let shard = shards.by_name(&row.shard_name).ok_or_else(|| {
            AutumnError::service_unavailable_msg(format!(
                "shard directory pins tenant {key:?} to unknown shard {:?}",
                row.shard_name
            ))
        })?;
        Ok(Some(shard.id()))
    }
}

impl ShardRouter for DirectoryShardRouter {
    fn route<'a>(
        &'a self,
        key: ShardKey<'a>,
        shards: &'a ShardSet,
    ) -> futures::future::BoxFuture<'a, Result<ShardId, AutumnError>> {
        Box::pin(async move {
            // Only string keys participate in the directory (tenants are
            // strings); numeric/byte keys route straight through the fallback.
            let ShardKey::Str(key_str) = key else {
                return self.fallback.route(key, shards).await;
            };

            if let Some(cached) = self.cache_get(key_str) {
                return Ok(cached);
            }

            // Only cache real directory hits. A miss routes through the hash
            // fallback WITHOUT caching: during a tenant move the operator
            // inserts a directory row, and a cached miss on another process
            // (e.g. a replica) would keep routing that tenant to its old hash
            // shard until the TTL expired — there is no cross-process
            // invalidation — and `move-slot --confirm` could then delete rows
            // those stale writes had landed on the source. Re-querying unpinned
            // tenants each route is the safe default.
            match self.lookup_directory(key_str, shards).await? {
                Some(shard) => {
                    self.cache_put(key_str.to_owned(), shard);
                    Ok(shard)
                }
                None => self.fallback.route(key, shards).await,
            }
        })
    }
}

// ── Shard runtime state ──────────────────────────────────────────────────────

/// Mutable per-shard replica readiness, updated by the per-shard health
/// indicator on readiness probes (mirrors the control replica's
/// [`ProbeState`](crate::probe::ProbeState) lifecycle).
#[derive(Debug)]
pub(crate) struct ShardRuntime {
    replica_fallback: ReplicaFallback,
    replica_configured: bool,
    connection_ready: AtomicBool,
    migrations_ready: AtomicBool,
    detail: std::sync::RwLock<Option<String>>,
    /// `(primary_url, replica_url)` for re-running the migration parity
    /// check from the per-shard health indicator. `None` when the app
    /// registered no migrations.
    migration_check: std::sync::RwLock<Option<(String, String)>>,
    /// When the parity comparison last ran, for throttling: unlike the
    /// pooled connectivity check, parity opens fresh synchronous
    /// connections to both roles, so it must not run on every probe.
    parity_checked_at: std::sync::Mutex<Option<std::time::Instant>>,
}

/// Minimum interval between migration parity re-checks per shard.
///
/// Readiness probes fire every few seconds per replica; the parity check
/// opens fresh synchronous connections to the shard's primary *and*
/// replica, so running it per probe per shard would exhaust Postgres
/// connection limits as shard counts grow.
const PARITY_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

// Mutators are driven by startup migration parity checks and the
// per-shard health indicators; some are exercised only by tests until
// the health wiring lands.
#[cfg_attr(not(test), allow(dead_code))]
impl ShardRuntime {
    fn new(replica_fallback: ReplicaFallback, replica_configured: bool) -> Self {
        Self {
            replica_fallback,
            replica_configured,
            connection_ready: AtomicBool::new(false),
            migrations_ready: AtomicBool::new(true),
            detail: std::sync::RwLock::new(
                replica_configured.then(|| "replica has not passed a readiness check".to_owned()),
            ),
            migration_check: std::sync::RwLock::new(None),
            parity_checked_at: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn configure_migration_check(&self, primary_url: String, replica_url: String) {
        *self
            .migration_check
            .write()
            .expect("shard runtime lock poisoned") = Some((primary_url, replica_url));
    }

    fn migration_check(&self) -> Option<(String, String)> {
        self.migration_check
            .read()
            .expect("shard runtime lock poisoned")
            .clone()
    }

    /// Whether the throttle window has elapsed; claims the slot when it
    /// has, so concurrent probes run at most one parity check per window.
    pub(crate) fn parity_check_due(&self) -> bool {
        let mut checked_at = self
            .parity_checked_at
            .lock()
            .expect("shard runtime lock poisoned");
        if checked_at.is_none_or(|at| at.elapsed() >= PARITY_RECHECK_INTERVAL) {
            *checked_at = Some(std::time::Instant::now());
            true
        } else {
            false
        }
    }

    fn replica_ready(&self) -> bool {
        self.connection_ready.load(Ordering::Relaxed)
            && self.migrations_ready.load(Ordering::Relaxed)
    }

    fn refresh_detail(&self) {
        if self.replica_ready() {
            *self.detail.write().expect("shard runtime lock poisoned") = None;
        }
    }

    pub(crate) fn mark_replica_connection_ready(&self) {
        self.connection_ready.store(true, Ordering::Relaxed);
        self.refresh_detail();
    }

    pub(crate) fn mark_replica_connection_unready(&self, detail: impl Into<String>) {
        self.connection_ready.store(false, Ordering::Relaxed);
        *self.detail.write().expect("shard runtime lock poisoned") = Some(detail.into());
    }

    pub(crate) fn mark_replica_migrations_ready(&self) {
        self.migrations_ready.store(true, Ordering::Relaxed);
        self.refresh_detail();
    }

    pub(crate) fn mark_replica_migrations_unready(&self, detail: impl Into<String>) {
        self.migrations_ready.store(false, Ordering::Relaxed);
        *self.detail.write().expect("shard runtime lock poisoned") = Some(detail.into());
    }

    pub(crate) fn detail(&self) -> Option<String> {
        self.detail
            .read()
            .expect("shard runtime lock poisoned")
            .clone()
    }
}

// ── Shard / ShardSet ─────────────────────────────────────────────────────────

/// One physical shard: a named [`DatabaseTopology`] plus its slot
/// assignment and runtime replica state.
#[derive(Clone)]
pub struct Shard {
    name: Arc<str>,
    id: ShardId,
    slots: Arc<[u16]>,
    topology: DatabaseTopology,
    runtime: Arc<ShardRuntime>,
}

impl Shard {
    /// Stable shard name from configuration.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Position of this shard in the configured set.
    #[must_use]
    pub const fn id(&self) -> ShardId {
        self.id
    }

    /// Logical slots owned by this shard, in ascending order.
    #[must_use]
    pub fn slots(&self) -> &[u16] {
        &self.slots
    }

    /// This shard's primary/replica pool topology.
    #[must_use]
    pub const fn topology(&self) -> &DatabaseTopology {
        &self.topology
    }

    /// This shard's primary/write pool.
    #[must_use]
    pub const fn primary_pool(&self) -> &Pool<RuntimeConnection> {
        self.topology.primary()
    }

    /// This shard's replica pool, when configured.
    #[must_use]
    pub const fn replica_pool(&self) -> Option<&Pool<RuntimeConnection>> {
        self.topology.replica()
    }

    /// Pool for read-only work, honoring this shard's `replica_fallback`
    /// and runtime replica readiness (mirrors
    /// [`AppState::read_pool`](crate::AppState::read_pool)):
    ///
    /// - no replica configured → the primary pool;
    /// - replica configured and ready → the replica pool;
    /// - replica unready, fallback `primary` → the primary pool;
    /// - replica unready, fallback `fail_readiness` → `None`.
    #[must_use]
    pub fn read_pool(&self) -> Option<&Pool<RuntimeConnection>> {
        self.read_pool_with_role().map(|(pool, _)| pool)
    }

    /// Snapshot this shard's read-routing decision as a
    /// [`ReadRoute`](crate::repository::ReadRoute), the per-shard analogue of
    /// [`ReadRoute::from_state`](crate::repository::ReadRoute::from_state).
    ///
    /// [`ShardedDb`] captures this at extraction time so a generated
    /// `#[repository]` built with `from_shard` routes its read-only methods
    /// to the shard's replica automatically — mirroring [`read_pool`] and
    /// honoring the shard's `replica_fallback` policy and replica readiness:
    ///
    /// - no replica configured → [`Primary`](crate::repository::ReadRoute::Primary);
    /// - replica ready → [`ReadPool`](crate::repository::ReadRoute::ReadPool) over the replica;
    /// - replica unready, fallback `primary` → `ReadPool` over the primary;
    /// - replica unready, fallback `fail_readiness` →
    ///   [`Unavailable`](crate::repository::ReadRoute::Unavailable).
    ///
    /// [`read_pool`]: Self::read_pool
    #[must_use]
    pub fn read_route(&self) -> crate::repository::ReadRoute {
        use crate::repository::ReadRoute;
        if !self.runtime.replica_configured {
            return ReadRoute::Primary;
        }
        self.read_pool().map_or(ReadRoute::Unavailable, |pool| {
            ReadRoute::ReadPool(pool.clone())
        })
    }

    /// The shard's replica pool for **explicit replica-only** reads.
    ///
    /// Returns the replica pool only when a replica is configured *and* has
    /// passed its readiness checks. Never returns the primary pool — this is
    /// the `replica_fallback`-independent counterpart to [`read_pool`]:
    ///
    /// - no replica configured → `None`;
    /// - replica configured but unready (regardless of `replica_fallback`) → `None`;
    /// - replica configured and ready → `Some(replica_pool)`.
    ///
    /// Backs [`ShardedReadDb`], which always requires a healthy replica.
    ///
    /// [`read_pool`]: Self::read_pool
    pub(crate) fn replica_read_pool(&self) -> Option<&Pool<RuntimeConnection>> {
        if self.runtime.replica_configured && self.runtime.replica_ready() {
            self.topology.replica()
        } else {
            None
        }
    }

    /// [`read_pool`](Self::read_pool) plus the role label of the returned
    /// pool, for interceptor/metric naming.
    pub(crate) fn read_pool_with_role(&self) -> Option<(&Pool<RuntimeConnection>, &'static str)> {
        if !self.runtime.replica_configured {
            return Some((self.topology.primary(), "primary"));
        }
        if self.runtime.replica_ready() {
            return self.topology.replica().map(|pool| (pool, "replica"));
        }
        match self.runtime.replica_fallback {
            ReplicaFallback::Primary => Some((self.topology.primary(), "primary")),
            ReplicaFallback::FailReadiness => None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn runtime(&self) -> &ShardRuntime {
        &self.runtime
    }
}

impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard")
            .field("name", &self.name)
            .field("id", &self.id)
            .field("slots", &self.slots)
            .finish_non_exhaustive()
    }
}

struct ShardSetInner {
    shards: Vec<Shard>,
    by_name: HashMap<String, usize>,
    /// `slot_map[slot]` is the index into `shards` of the slot's owner.
    slot_map: Vec<usize>,
    router: Arc<dyn ShardRouter>,
}

/// The configured set of shards plus the routing strategy.
///
/// Cheap to clone (a single `Arc`). Available from
/// [`AppState::shards`](crate::AppState::shards) and through the
/// [`Shards`] extractor.
#[derive(Clone)]
pub struct ShardSet {
    inner: Arc<ShardSetInner>,
}

impl ShardSet {
    /// Number of configured shards.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.shards.len()
    }

    /// Whether the set contains no shards.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.shards.is_empty()
    }

    /// Number of logical slots — the fixed [`SLOT_COUNT`] (16384).
    #[must_use]
    pub const fn slot_count(&self) -> u16 {
        SLOT_COUNT
    }

    /// Shard by positional id.
    #[must_use]
    pub fn get(&self, id: ShardId) -> Option<&Shard> {
        self.inner.shards.get(id.0)
    }

    /// Shard by configured name.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&Shard> {
        self.inner
            .by_name
            .get(name)
            .and_then(|&idx| self.inner.shards.get(idx))
    }

    /// Iterate shards in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Shard> {
        self.inner.shards.iter()
    }

    /// Map a routing key onto its logical slot (deterministic hash; see
    /// the module docs for the permanence guarantee).
    #[must_use]
    pub fn slot_for_key<'k>(&self, key: impl Into<ShardKey<'k>>) -> SlotId {
        slot_for_key(key.into())
    }

    /// Owner of a logical slot per the configured slot map.
    #[must_use]
    pub fn shard_for_slot(&self, slot: SlotId) -> Option<&Shard> {
        self.inner
            .slot_map
            .get(usize::from(slot.0))
            .and_then(|&idx| self.inner.shards.get(idx))
    }

    /// Resolve the shard that owns `key` via the installed
    /// [`ShardRouter`].
    ///
    /// # Errors
    ///
    /// Returns the router's error, or an internal error if the router
    /// produced an out-of-range [`ShardId`].
    pub async fn route<'k>(&self, key: impl Into<ShardKey<'k>>) -> Result<&Shard, AutumnError> {
        let key = key.into();
        let id = self.inner.router.route(key, self).await?;
        self.get(id).ok_or_else(|| {
            AutumnError::service_unavailable_msg(format!(
                "shard router returned out-of-range shard id {} (have {} shards)",
                id.0,
                self.len()
            ))
        })
    }

    /// Total configured `max_size` across every pool in the set
    /// (primaries plus replicas). Logged at startup so N-shard
    /// deployments notice multiplied connection counts.
    #[must_use]
    pub fn total_max_connections(&self) -> usize {
        self.inner
            .shards
            .iter()
            .map(|shard| {
                shard.topology().primary().status().max_size
                    + shard
                        .topology()
                        .replica()
                        .map_or(0, |pool| pool.status().max_size)
            })
            .sum()
    }

    /// Whether `key` is owned by the shard at the given index in declaration order.
    ///
    /// Uses the hash-based slot assignment, **not** the installed router (which
    /// may override routing for individual tenants via a directory). Use this for
    /// tooling / slot-move scripts where you need to verify ownership without
    /// issuing an async router call.
    #[must_use]
    pub fn owns_key<'k>(&self, shard_id: ShardId, key: impl Into<ShardKey<'k>>) -> bool {
        let slot = self.slot_for_key(key);
        self.shard_for_slot(slot)
            .is_some_and(|s| s.id() == shard_id)
    }

    /// All logical slots assigned to the shard at index `shard_id`.
    ///
    /// Returns `None` when the id is out of range.
    #[must_use]
    pub fn slots_for_shard(&self, shard_id: ShardId) -> Option<&[u16]> {
        self.inner.shards.get(shard_id.0).map(Shard::slots)
    }

    /// Partition string `keys` by their owning shard based on hash-slot assignment.
    ///
    /// Keys are grouped in declaration order; the returned map may have fewer
    /// entries than `self.len()` when some shards own none of the given keys.
    /// Useful for slot-move tooling that needs to issue `WHERE tenant_id = ANY($1)`
    /// per destination shard.
    #[must_use]
    pub fn partition_by_shard<'k>(
        &self,
        keys: impl IntoIterator<Item = &'k str>,
    ) -> std::collections::HashMap<ShardId, Vec<&'k str>> {
        let mut map: std::collections::HashMap<ShardId, Vec<&'k str>> =
            std::collections::HashMap::new();
        for key in keys {
            let slot = self.slot_for_key(key);
            if let Some(shard) = self.shard_for_slot(slot) {
                map.entry(shard.id()).or_default().push(key);
            }
        }
        map
    }

    /// Fan out a closure over every shard concurrently, collecting one result
    /// per shard.  Fails the whole call if **any** shard errors.
    ///
    /// Intended for cross-shard read fan-out from `across_tenants()` reads on
    /// `#[repository(tenant_scoped, sharded)]` repositories.  The closure
    /// receives each [`Shard`] so it can build a sub-repo that honors that
    /// shard's read routing (replica/primary/fail-closed) and the parent
    /// request context; the sub-repo must set `__autumn_shards = None` so
    /// recursion is impossible.
    ///
    /// The closure is invoked synchronously per shard (the `&Shard` borrow ends
    /// when it returns the owned, `'static` future), so the futures can run
    /// concurrently without borrowing the [`ShardSet`].
    ///
    /// Concurrency is bounded at [`FAN_OUT_CONCURRENCY`] so a cross-tenant admin
    /// read does not check out a connection from every shard at once (which
    /// could spike load or exhaust connection limits on large fleets), matching
    /// the public [`Shards::each_shard`] pipeline. Results are returned in shard
    /// **declaration order** (not completion order), so order-dependent merges
    /// such as `search`'s per-shard ranking concatenation are deterministic.
    /// Fails the whole call on the first shard error.
    ///
    /// This is a framework-internal primitive used by generated repository
    /// code.  It is `pub` so that downstream crates can call it from
    /// `#[repository]`-generated `impl` blocks, but it is not part of the
    /// stable public API.
    #[doc(hidden)]
    pub async fn fan_out_shards<T, Fut, F>(&self, f: F) -> Result<Vec<T>, crate::AutumnError>
    where
        T: Send + 'static,
        Fut: std::future::Future<Output = Result<T, crate::AutumnError>> + Send + 'static,
        F: Fn(&Shard) -> Fut + Send + Sync,
    {
        use futures::StreamExt as _;

        // Results are placed by shard index so declaration order is preserved
        // even though `FuturesUnordered` yields them in completion order.
        let mut slots: Vec<Option<T>> = (0..self.inner.shards.len()).map(|_| None).collect();
        let mut in_flight = futures::stream::FuturesUnordered::new();

        for (idx, shard) in self.inner.shards.iter().enumerate() {
            if in_flight.len() >= FAN_OUT_CONCURRENCY
                && let Some((i, result)) = in_flight.next().await
            {
                slots[i] = Some(result?);
            }
            let fut = f(shard);
            in_flight.push(async move { (idx, fut.await) });
        }
        while let Some((i, result)) = in_flight.next().await {
            slots[i] = Some(result?);
        }
        // Every shard pushed exactly one future and all were drained above, so
        // on the success path every slot is filled.
        Ok(slots
            .into_iter()
            .map(|slot| slot.expect("every shard produced a result"))
            .collect())
    }
}

impl std::fmt::Debug for ShardSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardSet")
            .field("shards", &self.inner.shards)
            .finish_non_exhaustive()
    }
}

// ── Construction ─────────────────────────────────────────────────────────────

/// Error building a [`ShardSet`] from configuration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ShardSetBuildError {
    /// A shard's connection pool could not be constructed.
    #[error("failed to build pool for shard {shard:?}: {source}")]
    Pool {
        /// Name of the failing shard.
        shard: String,
        /// Underlying pool construction error.
        source: PoolError,
    },
    /// The slot map could not be resolved from configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A custom provider returned the wrong number of shard topologies.
    #[error("expected {expected} shard topologies, got {actual}")]
    TopologyCountMismatch {
        /// Number of configured shards.
        expected: usize,
        /// Number of topologies supplied.
        actual: usize,
    },
}

/// Build a [`ShardSet`] from configuration using the default deadpool
/// factory for every shard topology.
///
/// Returns `Ok(None)` when no `[[database.shards]]` entries are
/// configured.
///
/// # Errors
///
/// Returns [`ShardSetBuildError`] when a pool cannot be constructed or
/// the slot map is invalid.
pub fn create_shard_set(
    config: &DatabaseConfig,
    router: Arc<dyn ShardRouter>,
) -> Result<Option<ShardSet>, ShardSetBuildError> {
    if !config.has_shards() {
        return Ok(None);
    }
    let topologies = config
        .shards
        .iter()
        .map(|shard| {
            crate::db::create_shard_topology(shard, config).map_err(|source| {
                ShardSetBuildError::Pool {
                    shard: shard.name.clone(),
                    source,
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    build_shard_set(config, topologies, router).map(Some)
}

/// Build a [`ShardSet`] where every shard primary pool uses `max_size(1)` and
/// wraps each connection in a test transaction that is rolled back when the
/// connection is returned.
///
/// This mirrors the transactional control-pool logic in `TestApp` so that
/// shard repositories in integration tests see rolled-back state between test
/// runs.
///
/// **Deadlock caveat:** with `max_size(1)` a handler that checks out the same
/// shard connection twice in a single request will deadlock (same as the
/// control pool).  Use a separate non-transactional shard set when a test
/// requires concurrent shard checkouts.
///
/// # Errors
///
/// Returns [`ShardSetBuildError`] when no shards are configured, any pool
/// cannot be built, or the slot map is invalid.
///
/// Postgres-only: each shard pool is established with a `begin_test_transaction`
/// rollback hook (Postgres transactional test isolation), so this helper is not
/// compiled under the `sqlite` feature — its sole caller, the Postgres
/// transactional `TestApp` harness (`crate::test`), is likewise Postgres-only.
#[cfg(not(feature = "sqlite"))]
pub fn create_shard_set_transactional(
    config: &DatabaseConfig,
    router: Arc<dyn ShardRouter>,
) -> Result<Option<ShardSet>, ShardSetBuildError> {
    if !config.has_shards() {
        return Ok(None);
    }

    let timeout = std::time::Duration::from_secs(config.connect_timeout_secs);

    let topologies = config
        .shards
        .iter()
        .map(|shard| {
            let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
                diesel_async::AsyncPgConnection,
            >::new(&shard.primary_url);
            let pool = Pool::builder(manager)
                .max_size(1)
                .wait_timeout(Some(timeout))
                .create_timeout(Some(timeout))
                .runtime(deadpool::Runtime::Tokio1)
                .post_create(deadpool::managed::Hook::async_fn(
                    |conn: &mut diesel_async::AsyncPgConnection, _| {
                        Box::pin(async move {
                            use diesel_async::AsyncConnection as _;
                            use diesel_async::RunQueryDsl as _;
                            conn.begin_test_transaction().await.map_err(|e| {
                                deadpool::managed::HookError::Backend(
                                    diesel_async::pooled_connection::PoolError::QueryError(e),
                                )
                            })?;
                            diesel::sql_query("SET autumn.test_transaction_started = 'true'")
                                .execute(conn)
                                .await
                                .map_err(|e| {
                                    deadpool::managed::HookError::Backend(
                                        diesel_async::pooled_connection::PoolError::QueryError(e),
                                    )
                                })?;
                            Ok(())
                        })
                    },
                ))
                .build()
                .map_err(|source| ShardSetBuildError::Pool {
                    shard: shard.name.clone(),
                    source: crate::db::PoolError::Build(source),
                })?;
            Ok(crate::db::DatabaseTopology::primary_only(pool))
        })
        .collect::<Result<Vec<_>, ShardSetBuildError>>()?;
    build_shard_set(config, topologies, router).map(Some)
}

/// Assemble a [`ShardSet`] from pre-built topologies (one per configured
/// shard, in declaration order). Used by custom
/// [`DatabasePoolProvider`](crate::db::DatabasePoolProvider)s and tests.
///
/// # Errors
///
/// Returns [`ShardSetBuildError`] when the topology count does not match
/// the configuration or the slot map is invalid.
pub fn build_shard_set(
    config: &DatabaseConfig,
    topologies: Vec<DatabaseTopology>,
    router: Arc<dyn ShardRouter>,
) -> Result<ShardSet, ShardSetBuildError> {
    if topologies.len() != config.shards.len() {
        return Err(ShardSetBuildError::TopologyCountMismatch {
            expected: config.shards.len(),
            actual: topologies.len(),
        });
    }
    let slot_map = config.resolved_slot_map()?;

    let mut slots_per_shard: Vec<Vec<u16>> = vec![Vec::new(); config.shards.len()];
    for (slot, &owner) in slot_map.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        slots_per_shard[owner].push(slot as u16);
    }

    let shards: Vec<Shard> = config
        .shards
        .iter()
        .zip(topologies)
        .enumerate()
        .map(|(idx, (shard_config, topology))| {
            let replica_configured = topology.replica().is_some();
            Shard {
                name: Arc::from(shard_config.name.as_str()),
                id: ShardId(idx),
                slots: Arc::from(std::mem::take(&mut slots_per_shard[idx])),
                topology,
                runtime: Arc::new(ShardRuntime::new(
                    shard_config.effective_replica_fallback(config),
                    replica_configured,
                )),
            }
        })
        .collect();
    // `AutumnConfig::validate()` already rejects duplicate names, but this
    // builder is public and reachable with unvalidated configs (custom
    // loaders, direct callers); a silently-shadowed map would make one of
    // the duplicates unaddressable via by_name/db_on and health components.
    let mut by_name = HashMap::with_capacity(shards.len());
    for (idx, shard) in shards.iter().enumerate() {
        if by_name.insert(shard.name().to_owned(), idx).is_some() {
            return Err(ConfigError::Validation(format!(
                "database.shards: shard name {:?} is declared more than once; \
                 shard names must be unique",
                shard.name()
            ))
            .into());
        }
    }

    Ok(ShardSet {
        inner: Arc::new(ShardSetInner {
            shards,
            by_name,
            slot_map,
            router,
        }),
    })
}

// ── Health ───────────────────────────────────────────────────────────────────

/// Framework health indicator registered per shard as `db:shard:<name>`.
///
/// Mirrors the control topology's lifecycle: on every readiness probe it
/// live-checks primary and replica connectivity and re-runs the migration
/// parity comparison, feeding the shard's runtime state (which gates
/// [`Shard::read_pool`]). A shard whose primary is unreachable reports `Down`
/// even when a replica can still serve reads, since writes and primary reads
/// would fail.
///
/// Reports `Down` — gating `/ready` — when the shard primary is unreachable,
/// or when the shard's replica is unready **and** its `replica_fallback` is
/// `fail_readiness`. A `primary`-fallback shard with a reachable primary
/// degrades to primary reads and stays `Up` with the replica state in its
/// details.
pub(crate) struct ShardHealthIndicator {
    shard: Shard,
}

impl ShardHealthIndicator {
    pub(crate) const fn new(shard: Shard) -> Self {
        Self { shard }
    }

    async fn refresh_replica_readiness(&self) {
        let Some(replica_pool) = self.shard.replica_pool() else {
            return;
        };
        // Connectivity goes through the deadpool pool (cheap, reused
        // connections) and runs on every probe; the parity comparison
        // opens fresh connections to both roles and is throttled.
        match replica_pool.get().await {
            Ok(conn) => {
                drop(conn);
                self.shard.runtime().mark_replica_connection_ready();
                if self.shard.runtime().parity_check_due()
                    && let Some((primary_url, replica_url)) = self.shard.runtime().migration_check()
                {
                    let readiness = crate::migrate::check_replica_migration_readiness_blocking(
                        primary_url,
                        replica_url,
                    )
                    .await;
                    if readiness.is_ready() {
                        self.shard.runtime().mark_replica_migrations_ready();
                    } else if let Some(detail) = readiness.detail() {
                        self.shard.runtime().mark_replica_migrations_unready(detail);
                    }
                }
            }
            Err(error) => self
                .shard
                .runtime()
                .mark_replica_connection_unready(format!("replica connection failed: {error}")),
        }
    }
}

impl crate::actuator::HealthIndicator for ShardHealthIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        Box::pin(async move {
            self.refresh_replica_readiness().await;

            let mut details = HashMap::new();
            let status = self.shard.primary_pool().status();
            details.insert("pool_size".to_owned(), serde_json::json!(status.max_size));
            details.insert(
                "active_connections".to_owned(),
                serde_json::json!((status.max_size as u64).saturating_sub(status.available as u64)),
            );
            details.insert(
                "idle_connections".to_owned(),
                serde_json::json!(status.available),
            );
            details.insert(
                "slots".to_owned(),
                serde_json::json!(self.shard.slots().len()),
            );
            if self.shard.replica_pool().is_some() {
                details.insert(
                    "replica_ready".to_owned(),
                    serde_json::json!(self.shard.runtime().replica_ready()),
                );
                if let Some(detail) = self.shard.runtime().detail() {
                    details.insert("replica_detail".to_owned(), serde_json::json!(detail));
                }
            }

            // Live-check the shard primary. `read_pool()` alone is not enough:
            // a primary-only shard's `read_pool()` always returns the primary
            // pool (so it is `Some` even when the primary is down), and a
            // replicated shard's `read_pool()` can be `Some` via a healthy
            // replica while the primary is unreachable — yet all shard writes
            // and primary reads would fail at request time. Probe the primary
            // (like the replica connectivity check above) and gate `/ready` on
            // it so load balancers stop routing to an instance that cannot
            // reach a shard primary.
            let primary_ok = match self.shard.primary_pool().get().await {
                Ok(conn) => {
                    drop(conn);
                    true
                }
                Err(error) => {
                    details.insert(
                        "primary_detail".to_owned(),
                        serde_json::json!(format!("primary connection failed: {error}")),
                    );
                    false
                }
            };
            details.insert("primary_ready".to_owned(), serde_json::json!(primary_ok));

            // `read_pool()` is `None` exactly when the replica is unready under
            // `fail_readiness`. Report `Up` only when the primary is reachable
            // *and* a read pool is available; either failing gates `/ready`.
            let output = if primary_ok && self.shard.read_pool().is_some() {
                crate::actuator::HealthCheckOutput::up()
            } else {
                crate::actuator::HealthCheckOutput::down()
            };
            output.with_details(details)
        })
    }
}

/// Register one `db:shard:<name>` readiness indicator per configured
/// shard onto `registry`. Called once at startup by `build_state`.
pub(crate) fn register_shard_health_indicators(
    set: &ShardSet,
    registry: &crate::actuator::HealthIndicatorRegistry,
) {
    for shard in set.iter() {
        let name = format!("db:shard:{}", shard.name());
        if let Err(error) = registry.register(
            name,
            crate::actuator::IndicatorGroup::Readiness,
            Arc::new(ShardHealthIndicator::new(shard.clone())),
        ) {
            tracing::warn!("{error}");
        }
    }
}

// ── Extractors ───────────────────────────────────────────────────────────────

/// Request-extension escape hatch for [`ShardedDb`] key resolution.
///
/// Insert this from middleware (or tests) to route a request to a
/// specific shard key, bypassing tenant extraction:
///
/// ```rust,ignore
/// request.extensions_mut().insert(ShardKeyOverride("tenant-42".to_owned()));
/// ```
#[derive(Debug, Clone)]
pub struct ShardKeyOverride(pub String);

/// Explicit shard access extractor.
///
/// Extract once, then route per call. Captures the request's database
/// context (route-level statement timeout, metrics key, interceptors) at
/// extraction so every checkout carries the same instrumentation as the
/// plain [`Db`](crate::db::Db) extractor.
///
/// Rejects with `503 Service Unavailable` when no `[[database.shards]]`
/// are configured.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/users/{user_id}/bookmarks")]
/// async fn list(shards: Shards, Path(user_id): Path<i64>) -> AutumnResult<&'static str> {
///     let mut db = shards.db_for(user_id).await?;
///     // run Diesel queries against the owning shard's primary
///     Ok("ok")
/// }
/// ```
pub struct Shards {
    set: ShardSet,
    ctx: crate::db::RequestDbContext,
}

impl<S> axum::extract::FromRequestParts<S> for Shards
where
    S: crate::db::DbState + Send + Sync,
{
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let set = state.shards().cloned().ok_or_else(no_shards_configured)?;
        let ctx = crate::db::RequestDbContext::from_parts(parts, state);
        Ok(Self { set, ctx })
    }
}

fn no_shards_configured() -> AutumnError {
    AutumnError::service_unavailable_msg(
        "No shards configured: declare [[database.shards]] in autumn.toml \
         (see docs/guide/sharding.md)",
    )
}

/// Build a tenant-free repository seed for cross-shard admin reads.
///
/// Unlike [`__autumn_resolve_repo_seed`], this resolves no tenant key. It seeds
/// from the first configured shard (its primary pool and read route) so the
/// pre-fan-out connection the trait methods acquire succeeds, then strips the
/// shard tag from the route label — the fan-out re-tags each per-shard query
/// with the shard actually executing it (see [`reshard_route_label`]).
fn cross_shard_seed(
    set: &ShardSet,
    ctx: &crate::db::RequestDbContext,
) -> Result<ShardRepositorySeed, AutumnError> {
    let shard = set.iter().next().ok_or_else(no_shards_configured)?;
    let mut seed =
        ShardRepositorySeed::from_ctx(shard.primary_pool(), ctx, shard.name(), shard.read_route());
    seed.route.clone_from(&ctx.route_key);
    Ok(seed)
}

/// Marks a repository built for tenant-free cross-shard reads.
///
/// Implemented by the `#[repository(tenant_scoped, sharded)]` macro and used by
/// [`CrossShard`] to construct the repository from a [`ShardSet`] without
/// resolving a tenant. Not intended to be implemented by hand.
pub trait CrossShardRepository: Sized {
    /// Construct the repository in `across_tenants()` mode from a tenant-free
    /// seed and the full shard set.
    #[doc(hidden)]
    fn __autumn_from_cross_shard(seed: ShardRepositorySeed, set: ShardSet) -> Self;
}

/// Axum extractor for tenant-free cross-shard reads on a
/// `#[repository(tenant_scoped, sharded)]` repository.
///
/// Cross-tenant admin endpoints normally have no tenant header or task-local, so
/// the standard repository extractor — which resolves a tenant to route to a
/// single shard — rejects them during extraction. `CrossShard<R>` instead loads
/// the full [`ShardSet`] without a tenant and yields a repository already in
/// `across_tenants()` mode: reads fan out across every
/// shard, while writes are rejected (cross-shard writes are unsupported).
///
/// ```ignore
/// async fn admin_list(
///     CrossShard(repo): CrossShard<PgBookmarkRepository>,
/// ) -> AutumnResult<Json<Vec<Bookmark>>> {
///     // fans out across all shards
///     Ok(Json(repo.find_all().await?))
/// }
/// ```
pub struct CrossShard<R>(pub R);

impl<R> std::ops::Deref for CrossShard<R> {
    type Target = R;
    fn deref(&self) -> &R {
        &self.0
    }
}

impl<R> std::ops::DerefMut for CrossShard<R> {
    fn deref_mut(&mut self) -> &mut R {
        &mut self.0
    }
}

impl<S, R> axum::extract::FromRequestParts<S> for CrossShard<R>
where
    S: crate::db::DbState + Send + Sync,
    R: CrossShardRepository,
{
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        // Load the shard set without resolving a tenant (the whole point), then
        // seed from it and build the repo in across_tenants() fan-out mode.
        let shards =
            <Shards as axum::extract::FromRequestParts<S>>::from_request_parts(parts, state)
                .await?;
        let seed = cross_shard_seed(&shards.set, &shards.ctx)?;
        Ok(Self(R::__autumn_from_cross_shard(seed, shards.set)))
    }
}

/// How many shards `each_shard` queries concurrently.
const FAN_OUT_CONCURRENCY: usize = 8;

impl Shards {
    /// The underlying [`ShardSet`].
    #[must_use]
    pub const fn set(&self) -> &ShardSet {
        &self.set
    }

    /// Iterate shards in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Shard> {
        self.set.iter()
    }

    /// Check out a connection to the **primary** of the shard that owns
    /// `key`.
    ///
    /// # Errors
    ///
    /// Returns the router's error or a checkout failure.
    pub async fn db_for<'k>(
        &self,
        key: impl Into<ShardKey<'k>>,
    ) -> Result<crate::db::Db, AutumnError> {
        let shard = self.set.route(key).await?;
        self.checkout_primary(shard).await
    }

    /// Check out a **read** connection to the shard that owns `key`,
    /// honoring the shard's replica topology, readiness, and
    /// `replica_fallback`.
    ///
    /// # Errors
    ///
    /// Returns the router's error, a checkout failure, or
    /// `503 Service Unavailable` when the shard's replica is unready and
    /// its fallback is `fail_readiness`.
    pub async fn read_for<'k>(
        &self,
        key: impl Into<ShardKey<'k>>,
    ) -> Result<crate::db::Db, AutumnError> {
        let shard = self.set.route(key).await?;
        let (pool, role) = shard.read_pool_with_role().ok_or_else(|| {
            AutumnError::service_unavailable_msg(format!(
                "shard {:?} replica is not ready and replica_fallback = \"fail_readiness\"",
                shard.name()
            ))
        })?;
        self.checkout(shard, pool, role).await
    }

    /// Check out a **replica-only** connection to the shard that owns `key`.
    ///
    /// Unlike [`read_for`], this method ignores the shard's `replica_fallback`
    /// policy and **never** falls back to the primary. It returns `503 Service
    /// Unavailable` whenever a healthy, ready replica is unavailable — whether
    /// no replica is configured, or the replica has not yet passed its
    /// readiness checks. Use this for analytics/reporting paths that must
    /// guarantee replica-only semantics.
    ///
    /// # Errors
    ///
    /// Returns the router's error, a checkout failure, or
    /// `503 Service Unavailable` when no healthy replica is available for the
    /// resolved shard.
    ///
    /// [`read_for`]: Self::read_for
    pub async fn read_replica_for<'k>(
        &self,
        key: impl Into<ShardKey<'k>>,
    ) -> Result<crate::db::Db, AutumnError> {
        let shard = self.set.route(key).await?;
        let pool = shard.replica_read_pool().ok_or_else(|| {
            AutumnError::service_unavailable_msg(format!(
                "shard {:?} has no healthy replica; read_replica_for requires a \
                 configured, ready replica (no primary fallback)",
                shard.name()
            ))
        })?;
        self.checkout(shard, pool, "replica").await
    }

    /// Check out a connection to a shard's primary **by name** —
    /// intended for admin/operational paths, not request routing.
    ///
    /// # Errors
    ///
    /// Returns a bad-request error for an unknown name, or a checkout
    /// failure.
    pub async fn db_on(&self, shard_name: &str) -> Result<crate::db::Db, AutumnError> {
        let shard = self
            .set
            .by_name(shard_name)
            .ok_or_else(|| AutumnError::bad_request_msg(format!("unknown shard {shard_name:?}")))?;
        self.checkout_primary(shard).await
    }

    /// Run `f` against the primary of **every** shard, concurrently
    /// (bounded), collecting per-shard results in declaration order.
    ///
    /// Failures are collected rather than short-circuited so aggregate/
    /// admin endpoints can report partial outages. Fan-out latency is
    /// roughly the slowest shard, not the sum — but remember that
    /// scatter/gather amplifies tail latency: the more shards, the more
    /// likely one is slow.
    ///
    /// There are **no cross-shard transactions**: each closure invocation
    /// commits or fails independently, and concurrent writers mean the
    /// collected results can observe torn aggregates.
    ///
    /// The returned future cannot borrow the `&Shard` argument — copy
    /// what you need (e.g. `shard.name().to_owned()`) before the
    /// `async move` block:
    ///
    /// ```rust,ignore
    /// let counts = shards
    ///     .each_shard(|shard, mut db| {
    ///         let name = shard.name().to_owned();
    ///         async move { /* query with db, label with name */ Ok(0i64) }
    ///     })
    ///     .await;
    /// ```
    pub async fn each_shard<T, Fut, F>(&self, f: F) -> Vec<(ShardId, Result<T, AutumnError>)>
    where
        T: Send,
        Fut: std::future::Future<Output = Result<T, AutumnError>> + Send,
        F: Fn(&Shard, crate::db::Db) -> Fut + Send + Sync,
    {
        // FuturesUnordered keeps the pipeline full at FAN_OUT_CONCURRENCY
        // (no head-of-line blocking on a slow shard); results are placed
        // by ShardId so declaration order is preserved. Futures come from
        // a named async fn rather than a closure returning an async block,
        // which would trip rustc #89976 when the handler future is checked
        // for Send.
        use futures::StreamExt as _;

        let mut results: Vec<Option<(ShardId, Result<T, AutumnError>)>> =
            std::iter::repeat_with(|| None)
                .take(self.set.len())
                .collect();
        let mut in_flight: futures::stream::FuturesUnordered<
            futures::future::BoxFuture<'_, (ShardId, Result<T, AutumnError>)>,
        > = futures::stream::FuturesUnordered::new();

        for shard in self.set.iter() {
            if in_flight.len() >= FAN_OUT_CONCURRENCY
                && let Some((id, result)) = in_flight.next().await
            {
                results[id.0] = Some((id, result));
            }
            in_flight.push(Box::pin(self.run_on_shard(shard, &f)));
        }
        while let Some((id, result)) = in_flight.next().await {
            results[id.0] = Some((id, result));
        }
        results.into_iter().flatten().collect()
    }

    async fn run_on_shard<T, Fut, F>(
        &self,
        shard: &Shard,
        f: &F,
    ) -> (ShardId, Result<T, AutumnError>)
    where
        T: Send,
        Fut: std::future::Future<Output = Result<T, AutumnError>> + Send,
        F: Fn(&Shard, crate::db::Db) -> Fut + Send + Sync,
    {
        let result = match self.checkout_primary(shard).await {
            Ok(db) => f(shard, db).await,
            Err(error) => Err(error),
        };
        (shard.id(), result)
    }

    async fn checkout_primary(&self, shard: &Shard) -> Result<crate::db::Db, AutumnError> {
        self.checkout(shard, shard.primary_pool(), "primary").await
    }

    async fn checkout(
        &self,
        shard: &Shard,
        pool: &Pool<RuntimeConnection>,
        role: &str,
    ) -> Result<crate::db::Db, AutumnError> {
        let ctx = self.ctx.clone();
        crate::db::Db::checkout(crate::db::DbCheckoutParams {
            pool,
            pool_name: &format!("shard:{}:{role}", shard.name()),
            shard: Some(shard.name()),
            statement_timeout: ctx.statement_timeout,
            // Tag the route metric with the shard so per-shard latency
            // separates in /actuator/metrics.
            route_key: ctx
                .route_key
                .map(|key| format!("{key} shard={}", shard.name())),
            metrics: ctx.metrics,
            slow_query_threshold: ctx.slow_query_threshold,
            interceptors: ctx.interceptors,
            // Shard traffic is not recorded in this slice, and the shard gap
            // is noted separately by `note_shard_capture_gap` on this same
            // checkout path — the control topology's gap does not apply here.
            #[cfg(all(feature = "reporting", not(feature = "sqlite")))]
            capture_gap: None,
            clock: ctx.clock,
        })
        .await
    }
}

/// Instrumentation seed for building a `#[repository]` over a shard.
///
/// Carries the shard's primary pool plus the three request-derived
/// observability values captured by [`ShardedDb`] at extraction time.
/// Generated repositories read this via `__autumn_repository_seed()` when
/// their `from_shard` constructor is called, so they apply the same
/// statement timeout, slow-query threshold, and route label as the
/// [`Shards`] extractor does when checking out a [`Db`](crate::db::Db).
///
/// This type is sealed behind `#[doc(hidden)]`; it is part of the
/// framework's internal ABI for generated code and must not be considered
/// a stable public API.
#[doc(hidden)]
#[derive(Clone)]
pub struct ShardRepositorySeed {
    pub pool: Pool<RuntimeConnection>,
    /// Statement timeout in milliseconds (`0` = no limit, matching the
    /// Postgres `statement_timeout = 0` convention).  Capped at
    /// `i32::MAX` ms to match the Postgres signed-integer constraint.
    pub statement_timeout_ms: u64,
    pub slow_query_threshold: std::time::Duration,
    /// Shard-tagged route label (e.g. `"GET /bookmarks shard=shard0"`),
    /// or `None` when no `MatchedPath` was present in the request.
    pub route: Option<String>,
    /// The shard's read-routing decision, snapshotted at extraction time so
    /// `from_shard` repositories send read-only methods to the shard's
    /// replica when one is healthy (issue #1274). Built via
    /// [`Shard::read_route`].
    pub read_route: crate::repository::ReadRoute,
}

impl ShardRepositorySeed {
    pub(crate) fn from_ctx(
        pool: &Pool<RuntimeConnection>,
        ctx: &crate::db::RequestDbContext,
        shard_name: &str,
        read_route: crate::repository::ReadRoute,
    ) -> Self {
        // Postgres `statement_timeout` is a signed 32-bit integer (ms); cap
        // to `i32::MAX` so the cast back to a `u64` field is always lossless.
        const PG_TIMEOUT_MAX_MS: u64 = i32::MAX as u64;
        let statement_timeout_ms = ctx.statement_timeout.map_or(0, |d| {
            u64::try_from(d.as_millis().min(u128::from(PG_TIMEOUT_MAX_MS)))
                .unwrap_or(PG_TIMEOUT_MAX_MS)
        });
        Self {
            pool: pool.clone(),
            statement_timeout_ms,
            slow_query_threshold: ctx.slow_query_threshold,
            route: ctx
                .route_key
                .as_ref()
                .map(|key| format!("{key} shard={shard_name}")),
            read_route,
        }
    }
}

/// Re-tag a fan-out sub-repo's route label with the shard executing the query.
///
/// Keeps per-shard DB metrics and slow-query logs attributed to the shard that
/// actually runs the query rather than the originally-routed shard. The parent
/// label is `"<key> shard=<orig>"` (see `ShardRepositorySeed::from_ctx`); this
/// swaps the `shard=` tag for `shard_name` while preserving the base route key.
/// Returns `None` when the parent had no label (no `MatchedPath`), so unlabelled
/// repos stay unlabelled.
#[must_use]
pub fn reshard_route_label(parent: Option<&str>, shard_name: &str) -> Option<String> {
    let parent = parent?;
    let base = parent.rsplit_once(" shard=").map_or(parent, |(key, _)| key);
    Some(format!("{base} shard={shard_name}"))
}

/// Tenant-routed shard connection extractor.
///
/// Resolves the routing key automatically and checks out a connection to
/// the owning shard's primary. Key resolution order:
///
/// 1. a [`ShardKeyOverride`] request extension (middleware/test escape
///    hatch),
/// 2. the tenant id established by the tenancy middleware
///    ([`tenancy::CURRENT_TENANT`](crate::tenancy::CURRENT_TENANT)),
/// 3. direct tenant extraction from the request per the `[tenancy]`
///    configuration.
///
/// Dereferences to `AsyncPgConnection` exactly like
/// [`Db`](crate::db::Db), and exposes [`tx`](Self::tx) with the same
/// transaction semantics.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/bookmarks")]
/// async fn list(mut db: ShardedDb) -> AutumnResult<String> {
///     // queries run on the tenant's shard
///     Ok(format!("served from shard {}", db.shard()))
/// }
/// ```
pub struct ShardedDb {
    db: crate::db::Db,
    shard_name: Arc<str>,
    shard_id: ShardId,
    repo_seed: ShardRepositorySeed,
    // The full shard set, so `Repo::from_shard(&db).across_tenants()` can fan
    // out across shards exactly like the generated extractor path (cheap to
    // clone — `ShardSet` is `Arc`-backed).
    shards: ShardSet,
}

impl ShardedDb {
    /// Name of the shard this connection belongs to.
    #[must_use]
    pub fn shard(&self) -> &str {
        &self.shard_name
    }

    /// Id of the shard this connection belongs to.
    #[must_use]
    pub const fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Connection-scoped tracing span (see [`Db::span`](crate::db::Db::span)).
    #[must_use]
    pub const fn span(&self) -> &tracing::Span {
        self.db.span()
    }

    /// Run an async closure inside a transaction **on this shard**.
    /// Same semantics as [`Db::tx`](crate::db::Db::tx); the transaction
    /// never spans shards.
    ///
    /// # Errors
    ///
    /// See [`Db::tx`](crate::db::Db::tx).
    pub async fn tx<'a, T, E, F>(&'a mut self, f: F) -> Result<T, AutumnError>
    where
        T: Send + 'a,
        E: From<diesel::result::Error> + Send + Sync + 'a,
        AutumnError: From<E>,
        F: for<'r> FnOnce(
                &'r mut crate::db::PooledConnection,
            ) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
            + Send
            + 'a,
    {
        self.db.tx(f).await
    }

    /// Run an async closure inside a transaction **on this shard** with explicit
    /// [`TxOptions`](crate::db::TxOptions) (isolation level + retry). Same
    /// semantics as [`Db::tx_with`](crate::db::Db::tx_with); the transaction
    /// never spans shards.
    ///
    /// # Errors
    ///
    /// See [`Db::tx_with`](crate::db::Db::tx_with).
    pub async fn tx_with<'a, T, E, F>(
        &'a mut self,
        opts: crate::db::TxOptions,
        f: F,
    ) -> Result<T, AutumnError>
    where
        T: Send + 'a,
        E: From<diesel::result::Error> + Send + Sync + 'a,
        AutumnError: From<E>,
        F: for<'r> FnMut(
                &'r mut crate::db::RuntimeConnection,
            ) -> scoped_futures::ScopedBoxFuture<'a, 'r, Result<T, E>>
            + Send
            + 'a,
    {
        self.db.tx_with(opts, f).await
    }

    /// Borrow the underlying [`Db`](crate::db::Db) (e.g. to pass to
    /// helpers written against the unsharded extractor).
    pub const fn db_mut(&mut self) -> &mut crate::db::Db {
        &mut self.db
    }

    /// Instrumentation seed for `from_shard` on generated repositories.
    /// Internal ABI; not a stable public API.
    #[doc(hidden)]
    #[must_use]
    pub const fn __autumn_repository_seed(&self) -> &ShardRepositorySeed {
        &self.repo_seed
    }

    /// The full shard set, so `from_shard`-built repositories can fan out under
    /// `across_tenants()`. Internal ABI; not a stable public API.
    #[doc(hidden)]
    #[must_use]
    pub const fn __autumn_shard_set(&self) -> &ShardSet {
        &self.shards
    }
}

impl std::ops::Deref for ShardedDb {
    type Target = RuntimeConnection;
    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl std::ops::DerefMut for ShardedDb {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.db
    }
}

impl AsMut<crate::db::Db> for ShardedDb {
    fn as_mut(&mut self) -> &mut crate::db::Db {
        &mut self.db
    }
}

/// Internal ABI for generated `#[repository(sharded)]` extractors.
///
/// Resolves the tenant→shard routing from a request, builds the
/// [`ShardRepositorySeed`] that carries the shard's pool and observability
/// context, and returns a cheap clone of the [`ShardSet`] for cross-shard
/// fan-out. Unlike [`ShardedDb::from_request_parts`] it does **not** check
/// out a connection, so generated repositories can acquire their own lazily.
#[doc(hidden)]
pub async fn __autumn_resolve_repo_seed(
    parts: &mut axum::http::request::Parts,
    state: &crate::AppState,
) -> Result<(ShardRepositorySeed, ShardSet), AutumnError> {
    let shards = <Shards as axum::extract::FromRequestParts<crate::AppState>>::from_request_parts(
        parts, state,
    )
    .await?;
    let key = resolve_shard_key(parts, state).await?;
    let shard = shards.set.route(&key).await?;
    let shard_name = Arc::clone(&shard.name);
    let seed = ShardRepositorySeed::from_ctx(
        shard.primary_pool(),
        &shards.ctx,
        &shard_name,
        shard.read_route(),
    );
    let set = shards.set.clone();
    Ok((seed, set))
}

impl axum::extract::FromRequestParts<crate::AppState> for ShardedDb {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        let shards = Shards::from_request_parts(parts, state).await?;
        let key = resolve_shard_key(parts, state).await?;

        let shard = shards.set.route(&key).await?;
        let shard_name = Arc::clone(&shard.name);
        let shard_id = shard.id();
        let repo_seed = ShardRepositorySeed::from_ctx(
            shard.primary_pool(),
            &shards.ctx,
            &shard_name,
            shard.read_route(),
        );
        let shard_set = shards.set.clone();
        let db = shards.checkout_primary(shard).await?;
        crate::read_your_writes::mark_write();
        Ok(Self {
            db,
            shard_name,
            shard_id,
            repo_seed,
            shards: shard_set,
        })
    }
}

/// Explicit replica-only shard connection extractor.
///
/// Resolves the routing key exactly like [`ShardedDb`] and checks out a
/// connection to the **replica** of the owning shard. Unlike [`ShardedDb`]'s
/// transparent read-routing (which follows the shard's `replica_fallback`
/// policy), `ShardedReadDb` **always** requires a healthy replica and returns
/// `503 Service Unavailable` immediately if one is not available — it never
/// silently falls back to the primary.
///
/// Use this extractor for analytics, reporting, or admin scatter-gather
/// handlers where replica-only semantics must be guaranteed:
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
///
/// #[get("/analytics")]
/// async fn analytics(db: ShardedReadDb) -> impl IntoResponse {
///     // guaranteed replica connection; 503 if none is configured or healthy
///     "ok"
/// }
/// ```
///
/// Pairs with the transparent default routing provided by [`ShardedDb`]:
/// that is the opt-out-free default; `ShardedReadDb` is the explicit
/// replica-only override (see issue #1275).
pub struct ShardedReadDb {
    db: crate::db::Db,
    shard_name: Arc<str>,
    shard_id: ShardId,
}

impl ShardedReadDb {
    /// Name of the shard this connection belongs to.
    #[must_use]
    pub fn shard(&self) -> &str {
        &self.shard_name
    }

    /// Id of the shard this connection belongs to.
    #[must_use]
    pub const fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Connection-scoped tracing span (see [`Db::span`](crate::db::Db::span)).
    #[must_use]
    pub const fn span(&self) -> &tracing::Span {
        self.db.span()
    }

    /// Borrow the underlying [`Db`](crate::db::Db) (e.g. to pass to
    /// helpers written against the unsharded extractor).
    pub const fn db_mut(&mut self) -> &mut crate::db::Db {
        &mut self.db
    }
}

impl std::ops::Deref for ShardedReadDb {
    type Target = RuntimeConnection;
    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl std::ops::DerefMut for ShardedReadDb {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.db
    }
}

impl AsMut<crate::db::Db> for ShardedReadDb {
    fn as_mut(&mut self) -> &mut crate::db::Db {
        &mut self.db
    }
}

impl axum::extract::FromRequestParts<crate::AppState> for ShardedReadDb {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        let shards = Shards::from_request_parts(parts, state).await?;
        let key = resolve_shard_key(parts, state).await?;

        let shard = shards.set.route(&key).await?;
        let shard_name = Arc::clone(&shard.name);
        let shard_id = shard.id();
        let pool = shard.replica_read_pool().ok_or_else(|| {
            AutumnError::service_unavailable_msg(format!(
                "shard {:?} has no healthy replica; ShardedReadDb requires a \
                 configured, ready replica (no primary fallback)",
                shard.name()
            ))
        })?;
        let db = shards.checkout(shard, pool, "replica").await?;
        Ok(Self {
            db,
            shard_name,
            shard_id,
        })
    }
}

/// Resolve the routing key for [`ShardedDb`]; see its docs for the
/// resolution order.
async fn resolve_shard_key(
    parts: &mut axum::http::request::Parts,
    state: &crate::AppState,
) -> Result<String, AutumnError> {
    if let Some(overridden) = parts.extensions.get::<ShardKeyOverride>() {
        return Ok(overridden.0.clone());
    }
    if let Ok(Some(tenant)) = crate::tenancy::CURRENT_TENANT.try_with(std::clone::Clone::clone) {
        return Ok(tenant);
    }
    let config = state
        .extension::<crate::config::AutumnConfig>()
        .ok_or_else(|| AutumnError::service_unavailable_msg("Config is not available"))?;
    crate::tenancy::extract_tenant_from_parts(parts, &config)
        .await
        .map_err(|error| {
            AutumnError::bad_request_msg(format!(
                "ShardedDb could not resolve a shard key: {error}. Enable [tenancy] so \
                 the tenant id can route the request, or insert a ShardKeyOverride \
                 request extension from middleware (see docs/guide/sharding.md)"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ShardConfig, SlotSpec};

    #[test]
    fn directory_invalidation_channel_and_interval_are_sane() {
        // The listener LISTENs on the same channel the migration's trigger
        // fires via `pg_notify`. If these drift, invalidations are never
        // delivered.
        assert_eq!(DIRECTORY_NOTIFY_CHANNEL, "autumn_shard_directory");
        // The idle sweep interval must be shorter than the cache TTL so a
        // never-re-observed expired entry is reclaimed before the TTL would
        // have done so anyway (delivery of an actual invalidation is immediate,
        // independent of this interval).
        assert!(
            DEFAULT_DIRECTORY_INVALIDATION_SWEEP_INTERVAL < DEFAULT_DIRECTORY_CACHE_TTL,
            "sweep interval should beat the TTL"
        );
    }

    // Backend-parametric: `build_sqlite_pool` refuses a `postgres://` target,
    // so an inline spelling here made every shard test panic at fixture
    // construction under `--features sqlite`. The routing, slot-map, seed and
    // health-registration assertions below are backend-independent and now run
    // on both. Pools are lazy, so nothing connects unless a test checks out.
    //
    // WHICH LAYER THIS EXERCISES. `create_shard_set` is public API and applies
    // no backend screen, which is what lets these tests run on either backend.
    // A shard topology is nonetheless Postgres-only in PRODUCTION:
    // `sqlite_sharding_unsupported_guard` (app.rs) refuses any SQLite shard
    // primary at boot, and `database_backend_consistency` refuses `shards`
    // beside a SQLite primary at config time. So these tests say "the routing
    // and pool machinery is backend-independent", not "SQLite sharding is
    // supported" — and if a screen is ever added to `create_shard_set` itself,
    // they are meant to fail loudly and be gated then.
    fn shard_config(name: &str) -> ShardConfig {
        ShardConfig {
            name: name.to_owned(),
            primary_url: crate::test_urls::primary(name),
            slots: None,
            replica_url: None,
            primary_pool_size: None,
            replica_pool_size: None,
            replica_fallback: None,
        }
    }

    fn sharded_config(names: &[&str]) -> DatabaseConfig {
        DatabaseConfig {
            shards: names.iter().map(|name| shard_config(name)).collect(),
            ..Default::default()
        }
    }

    fn shard_set(names: &[&str]) -> ShardSet {
        create_shard_set(&sharded_config(names), Arc::new(HashShardRouter))
            .expect("lazy pools should build")
            .expect("shards configured")
    }

    // ── key→slot golden vectors ─────────────────────────────────────────
    //
    // These values are a PERMANENT CONTRACT. If one of these assertions
    // fails, the change re-routes every existing sharded deployment's
    // keys — do not update the expected values; fix the hash instead.

    #[test]
    fn golden_vector_str_keys() {
        // Expected slots computed independently (Python reference
        // implementation of FNV-1a 64 mod 16384) when the contract was
        // established.
        let cases: &[(&str, u16)] = &[
            ("tenant-1", 12427),
            ("tenant-2", 12862),
            ("tenant-3", 13297),
            ("acme-corp", 11394),
            ("globex", 12846),
            ("initech", 11329),
            ("hooli", 3974),
            ("", 8997),
            ("a", 11404),
            ("00000000-0000-0000-0000-000000000001", 6206),
        ];
        for (key, expected_slot) in cases {
            assert_eq!(
                slot_for_key(ShardKey::Str(key)),
                SlotId(*expected_slot),
                "key {key:?} must keep routing to slot {expected_slot} forever",
            );
        }
    }

    #[test]
    fn golden_vector_int_keys() {
        // Expected slots computed independently (Python reference
        // implementation of splitmix64 mod 16384) when the contract was
        // established.
        let cases: &[(i64, u16)] = &[
            (0, 3503),
            (1, 7361),
            (2, 5838),
            (42, 11925),
            (1_000_000, 1511),
            (-1, 11296),
            (i64::MAX, 7847),
            (i64::MIN, 13275),
        ];
        for (key, expected_slot) in cases {
            assert_eq!(
                slot_for_key(ShardKey::Int(*key)),
                SlotId(*expected_slot),
                "key {key} must keep routing to slot {expected_slot} forever",
            );
        }
    }

    #[test]
    fn golden_vector_bytes_match_equivalent_str() {
        // Str and Bytes share FNV-1a, so identical bytes route identically.
        assert_eq!(
            slot_for_key(ShardKey::Bytes(b"tenant-1")),
            slot_for_key(ShardKey::Str("tenant-1")),
        );
    }

    #[test]
    fn slots_stay_in_range_and_spread_roughly_uniformly() {
        // 10k keys over 16384 slots is too sparse for per-slot bounds, so
        // check uniformity over 16 contiguous buckets of 1024 slots each.
        let mut histogram = [0usize; 16];
        for i in 0..10_000i64 {
            let slot = slot_for_key(ShardKey::Int(i));
            assert!(slot.0 < SLOT_COUNT);
            histogram[usize::from(slot.0 / 1024)] += 1;
        }
        let expected = 10_000 / histogram.len();
        for (bucket, count) in histogram.iter().enumerate() {
            assert!(
                *count > expected / 2 && *count < expected * 2,
                "bucket {bucket} has {count} keys (expected ≈{expected})"
            );
        }
    }

    // ── ShardSet behavior ───────────────────────────────────────────────

    #[tokio::test]
    async fn db_for_and_read_for_attempt_routed_checkouts() {
        // Nothing can be reached, so both calls must surface checkout
        // failures (not routing errors) after resolving the shard. The target
        // is spelled per backend: the default fixture's SQLite target is a
        // live in-memory database, which would make both checkouts SUCCEED and
        // the assertions vacuous.
        let mut config = sharded_config(&["alpha"]);
        config.connect_timeout_secs = 1;
        config.shards[0].primary_url = crate::test_urls::unreachable("alpha");
        let shards = shards_handle_from(
            create_shard_set(&config, Arc::new(HashShardRouter))
                .expect("build")
                .expect("configured"),
        );
        let Err(error) = shards.db_for("tenant-1").await else {
            panic!("checkout must fail without a server");
        };
        assert!(!error.to_string().contains("Unknown shard"));

        // Without a replica, reads route to the primary role.
        let Err(error) = shards.read_for("tenant-1").await else {
            panic!("checkout must fail without a server");
        };
        assert!(!error.to_string().contains("fail_readiness"));
    }

    #[test]
    fn parity_recheck_is_throttled_per_window() {
        let set = shard_set(&["a"]);
        let runtime = set.get(ShardId(0)).expect("shard").runtime();
        runtime.configure_migration_check(
            "postgres://localhost/a".to_owned(),
            "postgres://localhost/a_ro".to_owned(),
        );
        assert!(runtime.migration_check().is_some());

        assert!(runtime.parity_check_due(), "first check claims the window");
        assert!(
            !runtime.parity_check_due(),
            "checks within the window are suppressed"
        );
    }

    #[tokio::test]
    async fn route_is_deterministic_and_respects_slot_map() {
        let mut config = sharded_config(&["a", "b"]);
        config.shards[0].slots = Some(vec![SlotSpec::Range("0-8191".to_owned())]);
        config.shards[1].slots = Some(vec![SlotSpec::Range("8192-16383".to_owned())]);
        let set = create_shard_set(&config, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");

        for key in ["k1", "k2", "k3", "k4", "k5"] {
            let slot = set.slot_for_key(key);
            let expected = if slot.0 >= 8192 { "b" } else { "a" };
            let routed = set.route(key).await.expect("route");
            assert_eq!(routed.name(), expected, "key {key:?} slot {}", slot.0);
            // Same key always lands on the same shard.
            assert_eq!(set.route(key).await.expect("route").id(), routed.id());
        }
    }

    #[tokio::test]
    async fn arc_shard_router_delegates_to_inner() {
        // `Arc<R>: ShardRouter` is what lets a custom DirectoryShardRouter be
        // shared between routing (`with_shard_router`) and its invalidation
        // listener (`spawn_invalidation_listener`, which needs `Arc<Self>`).
        // Install one through a ShardSet and confirm routing flows to the inner
        // router rather than failing the trait bound.
        let config = sharded_config(&["a", "b"]);
        let set = create_shard_set(&config, Arc::new(Arc::new(HashShardRouter)))
            .expect("build")
            .expect("configured");
        let first = set.route("tenant-42").await.expect("route");
        let again = set.route("tenant-42").await.expect("route");
        assert_eq!(
            first.id(),
            again.id(),
            "Arc<R> routes deterministically through its inner router"
        );
    }

    #[tokio::test]
    async fn moving_a_slot_in_config_moves_only_that_slot() {
        // "Reshard" by reassigning slots 12288-16383 from shard b to a new
        // shard c: keys in slots 0-12287 must not move.
        let mut before = sharded_config(&["a", "b"]);
        before.shards[0].slots = Some(vec![SlotSpec::Range("0-8191".to_owned())]);
        before.shards[1].slots = Some(vec![SlotSpec::Range("8192-16383".to_owned())]);

        let mut after = sharded_config(&["a", "b", "c"]);
        after.shards[0].slots = Some(vec![SlotSpec::Range("0-8191".to_owned())]);
        after.shards[1].slots = Some(vec![SlotSpec::Range("8192-12287".to_owned())]);
        after.shards[2].slots = Some(vec![SlotSpec::Range("12288-16383".to_owned())]);

        let set_before = create_shard_set(&before, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");
        let set_after = create_shard_set(&after, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");

        let mut moved = 0;
        for i in 0..200i64 {
            let slot = set_before.slot_for_key(i);
            assert_eq!(slot, set_after.slot_for_key(i), "key→slot never changes");
            let before_shard = set_before.route(i).await.expect("route");
            let after_shard = set_after.route(i).await.expect("route");
            if slot.0 >= 12288 {
                assert_eq!(before_shard.name(), "b");
                assert_eq!(after_shard.name(), "c");
                moved += 1;
            } else {
                assert_eq!(before_shard.name(), after_shard.name());
            }
        }
        assert!(moved > 0, "some keys must exercise the moved slot range");
    }

    #[test]
    fn by_name_and_get_resolve_shards() {
        let set = shard_set(&["alpha", "beta"]);
        assert_eq!(set.len(), 2);
        assert_eq!(set.by_name("beta").expect("beta").id(), ShardId(1));
        assert_eq!(set.get(ShardId(0)).expect("alpha").name(), "alpha");
        assert!(set.by_name("gamma").is_none());
        assert!(set.get(ShardId(9)).is_none());
        let names: Vec<&str> = set.iter().map(Shard::name).collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[test]
    fn auto_split_assigns_contiguous_slots() {
        let set = shard_set(&["a", "b"]);
        assert_eq!(set.slot_count(), SLOT_COUNT);
        assert_eq!(set.get(ShardId(0)).expect("a").slots().len(), 8192);
        assert_eq!(
            set.get(ShardId(1)).expect("b").slots(),
            (8192..16384).collect::<Vec<u16>>()
        );
    }

    // §3 slot-move helpers
    #[test]
    fn owns_key_agrees_with_route() {
        // shard a owns slots 0-8191, shard b owns 8192-16383.
        // "hooli" → slot 3974 → shard a (ShardId(0)).
        // "a"     → slot 11404 → shard b (ShardId(1)).
        let set = shard_set(&["a", "b"]);
        assert!(
            set.owns_key(ShardId(0), "hooli"),
            "hooli (slot 3974) must be shard a"
        );
        assert!(
            !set.owns_key(ShardId(1), "hooli"),
            "hooli must not be shard b"
        );
        assert!(
            set.owns_key(ShardId(1), "a"),
            "key 'a' (slot 11404) must be shard b"
        );
        assert!(
            !set.owns_key(ShardId(0), "a"),
            "key 'a' must not be shard a"
        );
    }

    #[test]
    fn slots_for_shard_returns_correct_slice() {
        let set = shard_set(&["a", "b"]);
        let a_slots = set.slots_for_shard(ShardId(0)).expect("shard a exists");
        let b_slots = set.slots_for_shard(ShardId(1)).expect("shard b exists");
        assert_eq!(a_slots.len(), 8192);
        assert_eq!(b_slots.len(), 8192);
        assert!(a_slots.iter().all(|&s| s < 8192));
        assert!(b_slots.iter().all(|&s| s >= 8192));
        assert!(set.slots_for_shard(ShardId(9)).is_none());
    }

    #[test]
    fn partition_by_shard_groups_golden_keys() {
        // Using the golden-vector keys: "hooli"→3974 (shard a), "a"→11404 (shard b).
        let set = shard_set(&["a", "b"]);
        let keys = ["hooli", "a", "tenant-1"]; // tenant-1 → 12427 → shard b
        let map = set.partition_by_shard(keys.iter().copied());
        #[allow(clippy::similar_names)]
        let keys_on_a = map.get(&ShardId(0)).map_or(&[][..], Vec::as_slice);
        #[allow(clippy::similar_names)]
        let keys_on_b = map.get(&ShardId(1)).map_or(&[][..], Vec::as_slice);
        assert!(keys_on_a.contains(&"hooli"), "hooli must go to shard a");
        assert!(keys_on_b.contains(&"a"), "key 'a' must go to shard b");
        assert!(
            keys_on_b.contains(&"tenant-1"),
            "tenant-1 (slot 12427) must go to shard b"
        );
        assert_eq!(
            keys_on_a.len() + keys_on_b.len(),
            keys.len(),
            "no key dropped"
        );
    }

    #[test]
    fn create_shard_set_returns_none_without_shards() {
        let config = DatabaseConfig::default();
        assert!(
            create_shard_set(&config, Arc::new(HashShardRouter))
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn build_shard_set_rejects_duplicate_names_without_config_validation() {
        // The builder is public: configs that bypassed
        // AutumnConfig::validate() must still not produce a shadowed
        // by_name map.
        let config = sharded_config(&["twin", "twin"]);
        let topologies = config
            .shards
            .iter()
            .map(|shard| crate::db::create_shard_topology(shard, &config).expect("lazy pools"))
            .collect();

        let result = build_shard_set(&config, topologies, Arc::new(HashShardRouter));

        let Err(ShardSetBuildError::Config(error)) = result else {
            panic!("duplicate shard names must be rejected, got {result:?}");
        };
        assert!(error.to_string().contains("twin"));
    }

    #[test]
    fn build_shard_set_rejects_topology_count_mismatch() {
        let config = sharded_config(&["a", "b"]);
        let result = build_shard_set(&config, Vec::new(), Arc::new(HashShardRouter));
        assert!(matches!(
            result,
            Err(ShardSetBuildError::TopologyCountMismatch {
                expected: 2,
                actual: 0
            })
        ));
    }

    // ── read_pool / replica fallback semantics ──────────────────────────

    // ── Postgres-only fixtures ───────────────────────────────────────────
    //
    // Everything from here that carries a `replica_url` is `#[cfg(not(feature
    // = "sqlite"))]`. A SQLite read replica is refused at both layers —
    // `database_backend_consistency` rejects any `replica_url` beside a SQLite
    // primary, and `reject_unusable_sqlite_replica` rejects one that is
    // in-memory or names a different file — so there is no SQLite spelling to
    // swap in. Gated rather than parametrised, so no test asserts a
    // configuration production refuses; the same treatment `db::`'s two
    // replica tests already got. The primary-only fixtures above run on both
    // backends.
    #[cfg(not(feature = "sqlite"))]
    fn shard_with_replica(fallback: ReplicaFallback) -> Shard {
        let mut config = sharded_config(&["a"]);
        config.shards[0].replica_url = Some("postgres://localhost/a_ro".to_owned());
        config.shards[0].replica_fallback = Some(fallback);
        let set = create_shard_set(&config, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");
        set.get(ShardId(0)).expect("shard").clone()
    }

    #[test]
    fn read_pool_uses_primary_when_no_replica() {
        let set = shard_set(&["a"]);
        let shard = set.get(ShardId(0)).expect("shard");
        assert!(shard.read_pool().is_some());
        assert!(shard.replica_pool().is_none());
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn read_pool_requires_readiness_check_before_replica_traffic() {
        let shard = shard_with_replica(ReplicaFallback::Primary);
        // Unchecked replica: fallback policy routes reads to the primary.
        assert!(shard.read_pool().is_some());
        assert!(shard.runtime().detail().is_some());

        shard.runtime().mark_replica_connection_ready();
        assert!(shard.runtime().replica_ready());
        assert!(shard.read_pool().is_some());
        assert!(shard.runtime().detail().is_none());
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn read_pool_fails_closed_under_fail_readiness() {
        let shard = shard_with_replica(ReplicaFallback::FailReadiness);
        assert!(
            shard.read_pool().is_none(),
            "unchecked replica fails closed"
        );

        shard.runtime().mark_replica_connection_ready();
        assert!(shard.read_pool().is_some());

        shard
            .runtime()
            .mark_replica_migrations_unready("replica lags primary");
        assert!(shard.read_pool().is_none());
        assert!(shard.runtime().detail().expect("detail").contains("lags"));
    }

    // ── read_route: per-shard ReadRoute snapshot (issue #1274) ───────────

    // Sizes only the Postgres-only replica fixtures below assert on.
    #[cfg(not(feature = "sqlite"))]
    const PRIMARY_SIZE: usize = 7;
    #[cfg(not(feature = "sqlite"))]
    const REPLICA_SIZE: usize = 3;

    #[cfg(not(feature = "sqlite"))]
    /// A one-shard set whose primary and replica pools have *distinct*
    /// `max_size` so `read_route()` reveals which pool it selected.
    fn shard_with_sized_replica(fallback: ReplicaFallback) -> Shard {
        let mut config = sharded_config(&["a"]);
        config.shards[0].replica_url = Some("postgres://localhost/a_ro".to_owned());
        config.shards[0].replica_fallback = Some(fallback);
        config.shards[0].primary_pool_size = Some(PRIMARY_SIZE);
        config.shards[0].replica_pool_size = Some(REPLICA_SIZE);
        let set = create_shard_set(&config, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");
        set.get(ShardId(0)).expect("shard").clone()
    }

    /// `max_size` of the pool a `ReadPool` route would acquire from, or
    /// `None` for the `Primary` / `Unavailable` variants.
    #[cfg(not(feature = "sqlite"))]
    fn read_pool_size(route: &crate::repository::ReadRoute) -> Option<usize> {
        match route {
            crate::repository::ReadRoute::ReadPool(pool) => Some(pool.status().max_size),
            crate::repository::ReadRoute::Primary | crate::repository::ReadRoute::Unavailable => {
                None
            }
        }
    }

    #[test]
    fn read_route_is_primary_without_replica() {
        let set = shard_set(&["a"]);
        let shard = set.get(ShardId(0)).expect("shard");
        assert!(
            matches!(shard.read_route(), crate::repository::ReadRoute::Primary),
            "a shard with no replica must keep reads on the primary"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn read_route_targets_replica_when_ready() {
        let shard = shard_with_sized_replica(ReplicaFallback::Primary);
        shard.runtime().mark_replica_connection_ready();
        assert!(shard.runtime().replica_ready());
        assert_eq!(
            read_pool_size(&shard.read_route()),
            Some(REPLICA_SIZE),
            "a ready replica must route reads to the replica pool"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn read_route_falls_back_to_primary_when_unready_and_policy_allows() {
        // Replica configured but never checked → fallback policy applies.
        let shard = shard_with_sized_replica(ReplicaFallback::Primary);
        assert_eq!(
            read_pool_size(&shard.read_route()),
            Some(PRIMARY_SIZE),
            "primary fallback must route reads to the primary pool"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn read_route_is_unavailable_when_unready_and_fallback_forbidden() {
        let shard = shard_with_sized_replica(ReplicaFallback::FailReadiness);
        assert!(
            matches!(
                shard.read_route(),
                crate::repository::ReadRoute::Unavailable
            ),
            "fail_readiness must not silently fall back to the primary"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn repository_seed_snapshots_the_shard_read_route() {
        let shard = shard_with_sized_replica(ReplicaFallback::Primary);
        shard.runtime().mark_replica_connection_ready();
        let ctx = crate::db::RequestDbContext {
            statement_timeout: None,
            route_key: Some("GET /notes".to_owned()),
            metrics: None,
            slow_query_threshold: std::time::Duration::from_millis(500),
            interceptors: Vec::new(),
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let seed = ShardRepositorySeed::from_ctx(
            shard.primary_pool(),
            &ctx,
            shard.name(),
            shard.read_route(),
        );
        assert_eq!(
            read_pool_size(&seed.read_route),
            Some(REPLICA_SIZE),
            "the seed must carry the shard's read route for from_shard"
        );
    }

    // ── Shards routing surface ──────────────────────────────────────────

    fn shards_handle(names: &[&str]) -> Shards {
        shards_handle_from(shard_set(names))
    }

    fn shards_handle_from(set: ShardSet) -> Shards {
        Shards {
            set,
            ctx: crate::db::RequestDbContext {
                statement_timeout: None,
                route_key: Some("GET /test".to_owned()),
                metrics: None,
                slow_query_threshold: std::time::Duration::from_millis(500),
                interceptors: Vec::new(),
                clock: std::sync::Arc::new(crate::time::SystemClock),
            },
        }
    }

    #[tokio::test]
    async fn db_on_rejects_unknown_shard_names() {
        let shards = shards_handle(&["alpha"]);
        let Err(error) = shards.db_on("beta").await else {
            panic!("unknown shard name must be rejected");
        };
        assert!(error.to_string().contains("beta"));
    }

    #[cfg(not(feature = "sqlite"))]
    #[tokio::test]
    async fn read_for_fails_closed_without_checkout_under_fail_readiness() {
        let mut config = sharded_config(&["a"]);
        config.shards[0].replica_url = Some("postgres://localhost/a_ro".to_owned());
        config.shards[0].replica_fallback = Some(ReplicaFallback::FailReadiness);
        let shards = Shards {
            set: create_shard_set(&config, Arc::new(HashShardRouter))
                .expect("build")
                .expect("configured"),
            ctx: crate::db::RequestDbContext {
                statement_timeout: None,
                route_key: None,
                metrics: None,
                slow_query_threshold: std::time::Duration::from_millis(500),
                interceptors: Vec::new(),
                clock: std::sync::Arc::new(crate::time::SystemClock),
            },
        };

        // The replica has not passed a readiness check, so the rejection
        // must be the fallback-policy error, not a connection failure.
        let Err(error) = shards.read_for("tenant-1").await else {
            panic!("unready replica under fail_readiness must be rejected");
        };
        assert!(error.to_string().contains("fail_readiness"));
    }

    #[test]
    fn shards_exposes_set_and_iter() {
        let shards = shards_handle(&["alpha", "beta"]);
        assert_eq!(shards.set().len(), 2);
        let names: Vec<&str> = shards.iter().map(Shard::name).collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[tokio::test]
    async fn route_rejects_out_of_range_router_results() {
        struct BadRouter;
        impl ShardRouter for BadRouter {
            fn route<'a>(
                &'a self,
                _key: ShardKey<'a>,
                _shards: &'a ShardSet,
            ) -> futures::future::BoxFuture<'a, Result<ShardId, AutumnError>> {
                Box::pin(std::future::ready(Ok(ShardId(99))))
            }
        }

        let set = create_shard_set(&sharded_config(&["a"]), Arc::new(BadRouter))
            .expect("build")
            .expect("configured");
        let error = set.route("k").await.expect_err("out of range");
        assert!(error.to_string().contains("out-of-range"));
    }

    #[test]
    fn shard_key_from_impls_route_consistently() {
        // i32 widens to the same slot as the equivalent i64.
        assert_eq!(
            slot_for_key(ShardKey::from(42i32)),
            slot_for_key(ShardKey::from(42i64)),
        );
        // Owned strings, str slices, and byte arrays agree.
        let owned = "tenant-1".to_owned();
        let bytes: [u8; 16] = *b"0123456789abcdef";
        assert_eq!(
            slot_for_key(ShardKey::from(&owned)),
            slot_for_key(ShardKey::from("tenant-1")),
        );
        assert_eq!(
            slot_for_key(ShardKey::from(&bytes)),
            slot_for_key(ShardKey::from(&b"0123456789abcdef"[..])),
        );
    }

    #[test]
    fn build_errors_and_debug_render_usefully() {
        let error = ShardSetBuildError::TopologyCountMismatch {
            expected: 2,
            actual: 0,
        };
        assert!(error.to_string().contains("expected 2"));

        let set = shard_set(&["alpha"]);
        let debug = format!("{set:?}");
        assert!(
            debug.contains("alpha"),
            "ShardSet Debug names shards: {debug}"
        );
        let shard_debug = format!("{:?}", set.get(ShardId(0)).expect("shard"));
        assert!(shard_debug.contains("alpha"));
        assert_eq!(
            set.shard_for_slot(SlotId(0)).expect("owner").name(),
            "alpha"
        );
    }

    // ── per-shard health indicator ──────────────────────────────────────

    #[cfg(not(feature = "sqlite"))]
    fn shard_with_unreachable_replica(fallback: ReplicaFallback) -> Shard {
        let mut config = sharded_config(&["a"]);
        // Nothing listens on these URLs; keep the failing checks fast.
        config.connect_timeout_secs = 1;
        config.shards[0].replica_url = Some("postgres://localhost:1/a_ro".to_owned());
        config.shards[0].replica_fallback = Some(fallback);
        let set = create_shard_set(&config, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");
        set.get(ShardId(0)).expect("shard").clone()
    }

    #[cfg(not(feature = "sqlite"))]
    #[tokio::test]
    async fn shard_indicator_gates_readiness_for_fail_readiness_replica() {
        use crate::actuator::HealthIndicator as _;

        let shard = shard_with_unreachable_replica(ReplicaFallback::FailReadiness);
        let indicator = ShardHealthIndicator::new(shard);
        let output = indicator.check().await;

        assert!(
            !output.status.is_healthy(),
            "unreachable replica under fail_readiness must report Down"
        );
        assert_eq!(output.details["replica_ready"], serde_json::json!(false));
        assert!(output.details.contains_key("replica_detail"));
    }

    #[cfg(not(feature = "sqlite"))]
    #[tokio::test]
    async fn shard_indicator_reports_down_when_primary_unreachable() {
        use crate::actuator::HealthIndicator as _;

        // `ReplicaFallback::Primary` would normally let a dead replica degrade
        // to primary reads and stay Up — but here the primary is also
        // unreachable, so the primary connectivity gate must force Down: an
        // instance that cannot reach the shard primary fails all writes and
        // primary reads, so `/ready` must not stay green. (The healthy-primary
        // + dead-replica fallback path needs a live primary and is exercised by
        // the `read_pool`/`read_route` fallback tests above, not the indicator.)
        let shard = shard_with_unreachable_replica(ReplicaFallback::Primary);
        let indicator = ShardHealthIndicator::new(shard);
        let output = indicator.check().await;

        assert!(
            !output.status.is_healthy(),
            "unreachable primary must report Down even under primary fallback"
        );
        assert_eq!(output.details["primary_ready"], serde_json::json!(false));
        assert!(output.details.contains_key("primary_detail"));
    }

    // The primary-connectivity gate, on both backends. The two indicator tests
    // above reach it through the unreachable-REPLICA fixture, which SQLite has
    // no spelling for; this one needs no replica, and "unreachable" is spelled
    // per backend (nothing listens on TCP port 1 / `sqlite3_open` cannot create
    // a file in a directory that does not exist).
    #[tokio::test]
    async fn shard_indicator_reports_down_when_primary_cannot_be_reached() {
        use crate::actuator::HealthIndicator as _;

        let mut config = sharded_config(&["a"]);
        config.connect_timeout_secs = 1;
        config.shards[0].primary_url = crate::test_urls::unreachable("a");
        let set = create_shard_set(&config, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");
        let shard = set.get(ShardId(0)).expect("shard").clone();

        let output = ShardHealthIndicator::new(shard).check().await;

        assert!(
            !output.status.is_healthy(),
            "an unreachable primary fails all writes and primary reads, so \
             /ready must not stay green"
        );
        assert_eq!(output.details["primary_ready"], serde_json::json!(false));
        assert!(output.details.contains_key("primary_detail"));
    }

    #[tokio::test]
    async fn register_shard_health_indicators_names_components() {
        let set = shard_set(&["alpha", "beta"]);
        let registry = crate::actuator::HealthIndicatorRegistry::new();

        register_shard_health_indicators(&set, &registry);
        // Re-registration is ignored with a warning rather than panicking.
        register_shard_health_indicators(&set, &registry);

        let results = registry.run_all().await;
        // run_all also appends process-global results (e.g. circuit
        // breakers created by concurrently-running tests), so assert on
        // the shard components only.
        let mut names: Vec<&str> = results
            .iter()
            .map(|r| r.name.as_str())
            .filter(|name| name.starts_with("db:shard:"))
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["db:shard:alpha", "db:shard:beta"]);
        assert!(
            results
                .iter()
                .filter(|r| r.name.starts_with("db:shard:"))
                .all(|r| matches!(r.group, crate::actuator::IndicatorGroup::Readiness)),
            "shard indicators gate readiness"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn total_max_connections_sums_every_pool() {
        let mut config = sharded_config(&["a", "b"]);
        config.pool_size = 7;
        config.shards[1].replica_url = Some("postgres://localhost/b_ro".to_owned());
        config.shards[1].replica_pool_size = Some(3);
        let set = create_shard_set(&config, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");
        // a primary (7) + b primary (7) + b replica (3).
        assert_eq!(set.total_max_connections(), 17);
    }

    // The replica-free half of the sum, on both backends. The fixture above
    // needs a replica to make the total interesting and so is Postgres-only;
    // `cache=shared` is exempt from the single-slot clamp, so a configured
    // `pool_size` reaches a SQLite pool too and the arithmetic still holds.
    #[test]
    fn total_max_connections_sums_every_primary_pool() {
        let mut config = sharded_config(&["a", "b"]);
        config.pool_size = 7;
        let set = create_shard_set(&config, Arc::new(HashShardRouter))
            .expect("build")
            .expect("configured");
        // a primary (7) + b primary (7).
        assert_eq!(set.total_max_connections(), 14);
    }

    // ── ShardRepositorySeed (#1273) ─────────────────────────────────────

    #[test]
    fn repo_seed_from_ctx_preserves_statement_timeout() {
        let set = shard_set(&["shard0"]);
        let shard = set.get(ShardId(0)).expect("shard");
        let ctx = crate::db::RequestDbContext {
            statement_timeout: Some(std::time::Duration::from_secs(3)),
            route_key: Some("GET /test".to_owned()),
            metrics: None,
            slow_query_threshold: std::time::Duration::from_millis(200),
            interceptors: Vec::new(),
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let seed =
            ShardRepositorySeed::from_ctx(shard.primary_pool(), &ctx, "shard0", shard.read_route());
        assert_eq!(seed.statement_timeout_ms, 3_000, "timeout preserved as ms");
        assert_eq!(
            seed.slow_query_threshold,
            std::time::Duration::from_millis(200),
            "slow threshold preserved"
        );
        assert_eq!(
            seed.route.as_deref(),
            Some("GET /test shard=shard0"),
            "route tagged with shard name"
        );
    }

    #[test]
    fn reshard_route_label_retags_with_target_shard() {
        // Fan-out sub-repo on shard2 must report under shard2, not the
        // originally-routed shard0, so per-shard metrics stay accurate.
        assert_eq!(
            reshard_route_label(Some("GET /admin shard=shard0"), "shard2").as_deref(),
            Some("GET /admin shard=shard2"),
        );
        // No parent label (no MatchedPath) stays unlabelled.
        assert_eq!(reshard_route_label(None, "shard2"), None);
        // A label without a shard tag still gets tagged for the target shard.
        assert_eq!(
            reshard_route_label(Some("GET /admin"), "shard2").as_deref(),
            Some("GET /admin shard=shard2"),
        );
    }

    #[test]
    fn cross_shard_wrapper_derefs_to_inner() {
        let mut w = CrossShard(7i32);
        assert_eq!(*w, 7); // Deref
        *w = 9; // DerefMut
        assert_eq!(w.0, 9);
    }

    #[test]
    fn cross_shard_seed_is_tenant_free_and_untagged() {
        // No tenant is resolved; the seed is built straight from the set so an
        // admin CrossShard<R> extractor can construct the repo without a header.
        let set = shard_set(&["shard0", "shard1"]);
        let ctx = crate::db::RequestDbContext {
            statement_timeout: Some(std::time::Duration::from_millis(1500)),
            route_key: Some("GET /admin".to_owned()),
            metrics: None,
            slow_query_threshold: std::time::Duration::from_millis(250),
            interceptors: Vec::new(),
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let seed = cross_shard_seed(&set, &ctx).expect("seed");
        // The route carries only the base key — the fan-out re-tags it per
        // executing shard via reshard_route_label, so it must NOT be pre-tagged
        // with the seed shard.
        assert_eq!(seed.route.as_deref(), Some("GET /admin"));
        assert!(!seed.route.as_deref().unwrap().contains("shard="));
        assert_eq!(seed.statement_timeout_ms, 1500);
        assert_eq!(
            seed.slow_query_threshold,
            std::time::Duration::from_millis(250)
        );
    }

    #[test]
    fn repo_seed_none_timeout_maps_to_zero() {
        let set = shard_set(&["shard0"]);
        let shard = set.get(ShardId(0)).expect("shard");
        let ctx = crate::db::RequestDbContext {
            statement_timeout: None,
            route_key: None,
            metrics: None,
            slow_query_threshold: std::time::Duration::from_millis(500),
            interceptors: Vec::new(),
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let seed =
            ShardRepositorySeed::from_ctx(shard.primary_pool(), &ctx, "shard0", shard.read_route());
        assert_eq!(seed.statement_timeout_ms, 0, "None timeout maps to 0");
        assert!(seed.route.is_none(), "None route_key propagates as None");
    }

    #[test]
    fn repo_seed_timeout_capped_at_i32_max() {
        let set = shard_set(&["shard0"]);
        let shard = set.get(ShardId(0)).expect("shard");
        let ctx = crate::db::RequestDbContext {
            statement_timeout: Some(std::time::Duration::from_secs(u64::MAX / 1_000)),
            route_key: None,
            metrics: None,
            slow_query_threshold: std::time::Duration::from_millis(500),
            interceptors: Vec::new(),
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let seed =
            ShardRepositorySeed::from_ctx(shard.primary_pool(), &ctx, "shard0", shard.read_route());
        assert_eq!(
            seed.statement_timeout_ms,
            i32::MAX as u64,
            "timeout capped at i32::MAX ms"
        );
    }

    // ── ShardedReadDb / replica_read_pool (issue #1275) ─────────────────

    #[test]
    fn replica_read_pool_is_none_without_replica() {
        let set = shard_set(&["a"]);
        let shard = set.get(ShardId(0)).expect("shard");
        assert!(
            shard.replica_read_pool().is_none(),
            "no replica configured → replica_read_pool must be None"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn replica_read_pool_is_none_when_unready_even_under_primary_fallback() {
        // The key difference from read_pool(): even with ReplicaFallback::Primary,
        // replica_read_pool never falls back to the primary — returns None.
        let shard = shard_with_sized_replica(ReplicaFallback::Primary);
        assert!(
            shard.replica_read_pool().is_none(),
            "unready replica under primary fallback must still return None for replica_read_pool"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn replica_read_pool_is_none_when_unready_under_fail_readiness() {
        let shard = shard_with_sized_replica(ReplicaFallback::FailReadiness);
        assert!(
            shard.replica_read_pool().is_none(),
            "unready replica under fail_readiness must return None"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn replica_read_pool_targets_replica_when_ready() {
        let shard = shard_with_sized_replica(ReplicaFallback::Primary);
        shard.runtime().mark_replica_connection_ready();
        assert!(shard.runtime().replica_ready());
        assert_eq!(
            shard.replica_read_pool().map(|p| p.status().max_size),
            Some(REPLICA_SIZE),
            "a ready replica must be returned by replica_read_pool"
        );
    }

    #[tokio::test]
    async fn read_replica_for_fails_when_no_replica_configured() {
        let shards = shards_handle(&["a"]);
        let Err(error) = shards.read_replica_for("tenant-1").await else {
            panic!("no replica configured must be rejected");
        };
        // Error must mention replica (not checkout failure) and must not
        // mention fail_readiness (this path is policy-independent).
        let msg = error.to_string();
        assert!(
            msg.contains("replica"),
            "error must name the missing replica: {msg}"
        );
        assert!(
            !msg.contains("fail_readiness"),
            "error must not mention fallback policy: {msg}"
        );
    }

    #[cfg(not(feature = "sqlite"))]
    #[tokio::test]
    async fn read_replica_for_fails_when_replica_unready_under_primary_fallback() {
        // Unlike read_for, read_replica_for must NOT fall back to the primary.
        let mut config = sharded_config(&["a"]);
        config.shards[0].replica_url = Some("postgres://localhost/a_ro".to_owned());
        config.shards[0].replica_fallback = Some(ReplicaFallback::Primary);
        let shards = Shards {
            set: create_shard_set(&config, Arc::new(HashShardRouter))
                .expect("build")
                .expect("configured"),
            ctx: crate::db::RequestDbContext {
                statement_timeout: None,
                route_key: None,
                metrics: None,
                slow_query_threshold: std::time::Duration::from_millis(500),
                interceptors: Vec::new(),
                clock: std::sync::Arc::new(crate::time::SystemClock),
            },
        };
        let Err(error) = shards.read_replica_for("tenant-1").await else {
            panic!("unready replica must be rejected even under primary fallback");
        };
        assert!(
            error.to_string().contains("replica"),
            "must name the replica: {error}"
        );
    }
}
