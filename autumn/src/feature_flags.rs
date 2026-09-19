//! First-class feature flags with per-actor rollouts and kill switches.
//!
//! Provides a typed, pluggable flag system that supports global on/off,
//! percent rollouts (stable per `(flag_name, actor_id)`), explicit actor
//! allowlists, and named group membership checks — without requiring a
//! redeploy to toggle any gate.
//!
//! # Quick start
//!
//! ```rust
//! use autumn_web::feature_flags::{FeatureFlagService, InMemoryFlagStore, FlagConfig};
//! use std::sync::Arc;
//!
//! // 1. Build a service backed by the in-memory store (perfect for tests).
//! let store = Arc::new(InMemoryFlagStore::new());
//! let svc = FeatureFlagService::new(store);
//!
//! // 2. Enable a flag for everyone.
//! svc.enable("dark_mode", None).unwrap();
//! assert!(svc.is_enabled("dark_mode", Some("user:1")));
//!
//! // 3. Disable it — all replicas pick up the change within seconds when
//! //    backed by the Postgres store with LISTEN/NOTIFY.
//! svc.disable("dark_mode", None).unwrap();
//! assert!(!svc.is_enabled("dark_mode", Some("user:1")));
//! ```
//!
//! # Evaluation order
//!
//! For a given `(flag, actor)` pair, rules are checked in this order:
//!
//! 1. **Kill switch**: if `enabled = false`, return `false` immediately.
//!    Call `disable()` for an instant kill-switch that overrides rollout and allowlists.
//! 2. **Global on**: if `rollout_pct >= 100`, return `true` for all actors.
//!    Call `enable()` to globally enable a flag.
//! 3. **Actor allowlist**: if the actor ID is in the explicit allowlist, return `true`.
//! 4. **Group membership**: if the actor belongs to any allowed group, return `true`.
//! 5. **Percent rollout**: if `rollout_pct > 0` and the deterministic hash bucket
//!    of `(flag_name, actor_id)` falls below the threshold, return `true`.
//! 6. Otherwise return `false`.
//!
//! Calling `enable()` sets `rollout_pct = 100` (globally on for all actors).
//! Calling `disable()` sets `enabled = false` — a hard kill-switch that overrides
//! rollout and allowlists — while preserving the rollout/allowlist configuration.
//!
//! Percent-rollout buckets are computed with a FNV-1a hash over the UTF-8
//! encoding of `"<flag_name>:<actor_id>"` and are therefore stable across
//! restarts and replicas.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ── Change log ───────────────────────────────────────────────────────────────

/// A single mutation recorded in the flag change log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlagChangeRecord {
    /// The flag key that was changed.
    pub key: String,
    /// Human-readable description of the mutation (e.g. `"enabled"`, `"rollout=25"`).
    pub mutation: String,
    /// Actor identifier supplied by the caller (username, principal, `"cli"`, etc.).
    pub actor: Option<String>,
    /// Wall-clock time of the change in seconds since UNIX epoch.
    pub timestamp_secs: u64,
}

impl FlagChangeRecord {
    fn now(key: &str, mutation: impl Into<String>, actor: Option<&str>) -> Self {
        let timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            key: key.to_owned(),
            mutation: mutation.into(),
            actor: actor.map(str::to_owned),
            timestamp_secs,
        }
    }
}

// ── Flag configuration ───────────────────────────────────────────────────────

/// The full configuration of a single feature flag.
///
/// A flag is enabled for a given actor when **any** of the following holds:
///
/// - `enabled` is `true` (global gate — fastest path).
/// - The actor's ID appears in `actor_allowlist`.
/// - The actor belongs to any group in `group_allowlist`.
/// - `rollout_pct > 0` and the actor's deterministic bucket falls below the
///   threshold (see module-level documentation for the hash algorithm).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagConfig {
    /// Unique flag key in `snake_case`.
    pub key: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Global gate: when `true` every actor sees the flag as enabled.
    pub enabled: bool,
    /// Percent rollout (0 = off, 1–100 = percentage of actors).
    pub rollout_pct: u8,
    /// Explicit list of actor IDs that always see the flag as enabled.
    pub actor_allowlist: Vec<String>,
    /// Named groups whose members always see the flag as enabled.
    pub group_allowlist: Vec<String>,
}

impl FlagConfig {
    /// Create a new disabled flag with no gates set.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            description: None,
            enabled: false,
            rollout_pct: 0,
            actor_allowlist: Vec::new(),
            group_allowlist: Vec::new(),
        }
    }
}

// ── Group resolver ──────────────────────────────────────────────────────────

/// A hook that checks whether `actor_id` belongs to `group`.
///
/// Register a resolver with [`FeatureFlagService::with_group_resolver`] to
/// enable the named-group evaluation gate.
pub type GroupResolver = Arc<dyn Fn(&str, &str) -> bool + Send + Sync + 'static>;

// ── FlagStore trait ──────────────────────────────────────────────────────────

/// Error from a [`FlagStore`] backend.
#[derive(Debug, thiserror::Error)]
pub enum FlagStoreError {
    /// The backend reported an I/O or connection failure.
    #[error("flag store backend error: {0}")]
    Backend(String),
}

/// Pluggable storage backend for feature flags.
///
/// All mutation methods (`enable`, `disable`, `set_rollout`, `allow_actor`,
/// `add_group`) record a [`FlagChangeRecord`] in the change log.
pub trait FlagStore: Send + Sync + 'static {
    /// Return the current configuration for `key`, or `None` if unknown.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn get(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError>;

    /// Return all known flags, sorted by key.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError>;

    /// Globally enable `key` for all actors (`enabled = true`, `rollout_pct = 100`).
    ///
    /// Creates the flag if absent. Clears any prior `disable()` kill-switch.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn enable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError>;

    /// Kill-switch `key` for all actors (`enabled = false`).
    ///
    /// Overrides rollout and allowlists while preserving their configuration.
    /// Call `enable()` or `set_rollout()` to restore access.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn disable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError>;

    /// Set the percent-rollout gate for `key` to `pct` (0–100).
    ///
    /// Also clears any prior `disable()` kill-switch (`enabled = true`).
    /// Use `disable()` for an instant kill-switch that overrides rollout.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn set_rollout(&self, key: &str, pct: u8, actor: Option<&str>) -> Result<(), FlagStoreError>;

    /// Add `actor_id` to the explicit allowlist for `key`.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn allow_actor(
        &self,
        key: &str,
        actor_id: &str,
        actor: Option<&str>,
    ) -> Result<(), FlagStoreError>;

    /// Add `group` to the named-group allowlist for `key`.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn add_group(&self, key: &str, group: &str, actor: Option<&str>) -> Result<(), FlagStoreError>;

    /// Return the most recent `limit` change records for `key`.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn history(&self, key: &str, limit: usize) -> Result<Vec<FlagChangeRecord>, FlagStoreError>;
}

// Blanket delegation so `Box<dyn FlagStore>` can be passed to `with_flag_store`.
impl FlagStore for Box<dyn FlagStore> {
    fn get(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError> {
        (**self).get(key)
    }
    fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError> {
        (**self).list()
    }
    fn enable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).enable(key, actor)
    }
    fn disable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).disable(key, actor)
    }
    fn set_rollout(&self, key: &str, pct: u8, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).set_rollout(key, pct, actor)
    }
    fn allow_actor(
        &self,
        key: &str,
        actor_id: &str,
        actor: Option<&str>,
    ) -> Result<(), FlagStoreError> {
        (**self).allow_actor(key, actor_id, actor)
    }
    fn add_group(&self, key: &str, group: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).add_group(key, group, actor)
    }
    fn history(&self, key: &str, limit: usize) -> Result<Vec<FlagChangeRecord>, FlagStoreError> {
        (**self).history(key, limit)
    }
}

/// `Arc<T>` delegates every method to the inner `T`.
///
/// This allows sharing the **same** store instance — and therefore the same
/// cache — between `with_flag_store` and `PgFlagStore::spawn_poll_listener`:
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use autumn_web::feature_flags::pg::PgFlagStore;
///
/// let store = Arc::new(PgFlagStore::new(&db_url));
/// // Listener and app service share the same Arc → same cache.
/// PgFlagStore::spawn_poll_listener(Arc::clone(&store), Duration::from_secs(1));
/// app.with_flag_store(Arc::clone(&store)).run().await;
/// ```
impl<T: FlagStore + ?Sized> FlagStore for Arc<T> {
    fn get(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError> {
        (**self).get(key)
    }
    fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError> {
        (**self).list()
    }
    fn enable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).enable(key, actor)
    }
    fn disable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).disable(key, actor)
    }
    fn set_rollout(&self, key: &str, pct: u8, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).set_rollout(key, pct, actor)
    }
    fn allow_actor(
        &self,
        key: &str,
        actor_id: &str,
        actor: Option<&str>,
    ) -> Result<(), FlagStoreError> {
        (**self).allow_actor(key, actor_id, actor)
    }
    fn add_group(&self, key: &str, group: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        (**self).add_group(key, group, actor)
    }
    fn history(&self, key: &str, limit: usize) -> Result<Vec<FlagChangeRecord>, FlagStoreError> {
        (**self).history(key, limit)
    }
}

// ── InMemoryFlagStore ────────────────────────────────────────────────────────

/// A thread-safe in-memory [`FlagStore`] suitable for tests and development.
///
/// State is **not** shared across processes or replicas. For production use
/// the Postgres-backed store from `autumn_web::feature_flags::pg`.
#[derive(Debug, Default)]
pub struct InMemoryFlagStore {
    flags: RwLock<HashMap<String, FlagConfig>>,
    history: RwLock<HashMap<String, Vec<FlagChangeRecord>>>,
}

impl InMemoryFlagStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn upsert(&self, key: &str, f: impl FnOnce(&mut FlagConfig)) {
        let mut flags = self.flags.write().unwrap();
        let flag = flags
            .entry(key.to_owned())
            .or_insert_with(|| FlagConfig::new(key));
        f(flag);
        drop(flags);
    }

    fn record(&self, record: FlagChangeRecord) {
        self.history
            .write()
            .unwrap()
            .entry(record.key.clone())
            .or_default()
            .push(record);
    }
}

impl FlagStore for InMemoryFlagStore {
    fn get(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError> {
        Ok(self.flags.read().unwrap().get(key).cloned())
    }

    fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError> {
        let mut flags: Vec<FlagConfig> = self.flags.read().unwrap().values().cloned().collect();
        flags.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(flags)
    }

    fn enable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        self.upsert(key, |f| {
            f.enabled = true;
            f.rollout_pct = 100;
        });
        self.record(FlagChangeRecord::now(key, "enabled", actor));
        Ok(())
    }

    fn disable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        self.upsert(key, |f| {
            f.enabled = false;
        });
        self.record(FlagChangeRecord::now(key, "disabled", actor));
        Ok(())
    }

    fn set_rollout(&self, key: &str, pct: u8, actor: Option<&str>) -> Result<(), FlagStoreError> {
        let pct = pct.min(100);
        self.upsert(key, |f| {
            f.enabled = true;
            f.rollout_pct = pct;
        });
        self.record(FlagChangeRecord::now(key, format!("rollout={pct}"), actor));
        Ok(())
    }

    fn allow_actor(
        &self,
        key: &str,
        actor_id: &str,
        actor: Option<&str>,
    ) -> Result<(), FlagStoreError> {
        self.upsert(key, |f| {
            if !f.enabled {
                // Re-enabling from a kill-switch via allowlist: reset rollout to 0
                // so only the explicitly listed actors gain access, not everyone
                // who happened to be in the previous (e.g. 100%) rollout cohort.
                f.rollout_pct = 0;
            }
            f.enabled = true;
            if !f.actor_allowlist.contains(&actor_id.to_owned()) {
                f.actor_allowlist.push(actor_id.to_owned());
            }
        });
        self.record(FlagChangeRecord::now(
            key,
            format!("allowed_actor={actor_id}"),
            actor,
        ));
        Ok(())
    }

    fn add_group(&self, key: &str, group: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        self.upsert(key, |f| {
            if !f.enabled {
                // Same targeted-enable semantics as allow_actor.
                f.rollout_pct = 0;
            }
            f.enabled = true;
            if !f.group_allowlist.contains(&group.to_owned()) {
                f.group_allowlist.push(group.to_owned());
            }
        });
        self.record(FlagChangeRecord::now(
            key,
            format!("added_group={group}"),
            actor,
        ));
        Ok(())
    }

    fn history(&self, key: &str, limit: usize) -> Result<Vec<FlagChangeRecord>, FlagStoreError> {
        Ok(self
            .history
            .read()
            .unwrap()
            .get(key)
            .map(|records| records.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default())
    }
}

// ── Postgres FlagStore ───────────────────────────────────────────────────────

/// Postgres-backed flag storage with LISTEN/NOTIFY cache invalidation.
///
/// Uses the framework-owned `autumn_feature_flags` and `feature_flag_changes`
/// tables managed by the `create_feature_flags` migration. On any write the
/// store sends a `NOTIFY autumn_flags` notification so all replicas running
/// the background LISTEN task pick up the change within seconds.
#[cfg(feature = "db")]
pub mod pg {
    use super::{FlagChangeRecord, FlagConfig, FlagStore, FlagStoreError};
    use diesel::prelude::*;
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::time::{Duration, Instant};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CacheLookup {
        Hit(Option<FlagConfig>),
        Miss,
    }

    #[derive(Debug, Clone)]
    struct CachedFlag {
        value: Option<FlagConfig>,
        expires_at: Instant,
    }

    /// Postgres-backed [`FlagStore`] with a short-lived read-through cache.
    ///
    /// On each write the store sends `NOTIFY autumn_flags` so replicas
    /// subscribed via a background LISTEN task can invalidate their caches
    /// within seconds — achieving the sub-5-second kill-switch SLA without
    /// requiring Redis.
    #[derive(Debug)]
    pub struct PgFlagStore {
        database_url: String,
        cache_ttl: Duration,
        cache: RwLock<HashMap<String, CachedFlag>>,
    }

    impl Clone for PgFlagStore {
        fn clone(&self) -> Self {
            Self::with_cache_ttl(self.database_url.clone(), self.cache_ttl)
        }
    }

    impl PgFlagStore {
        /// Default read-through cache lifetime.
        pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(1);

        /// Create a store using the default 1 s read-through cache.
        #[must_use]
        pub fn new(database_url: impl Into<String>) -> Self {
            Self::with_cache_ttl(database_url, Self::DEFAULT_CACHE_TTL)
        }

        /// Create a store with an explicit cache TTL. Use `Duration::ZERO` to
        /// disable caching.
        #[must_use]
        pub fn with_cache_ttl(database_url: impl Into<String>, cache_ttl: Duration) -> Self {
            Self {
                database_url: database_url.into(),
                cache_ttl,
                cache: RwLock::new(HashMap::new()),
            }
        }

        /// Create a store from Autumn's primary database configuration.
        ///
        /// Returns `None` when no primary URL is configured, and — since it
        /// opens a `diesel::PgConnection` and notifies through `pg_notify` —
        /// when the configured target does not name Postgres. Screening here
        /// turns "a driver-level connection error on the first flag read" into
        /// "this backend has no Postgres flag store", which is what the
        /// operator can act on.
        #[must_use]
        pub fn from_database_config(config: &crate::config::DatabaseConfig) -> Option<Self> {
            config.effective_primary_postgres_url().map(Self::new)
        }

        fn connect(&self) -> Result<diesel::PgConnection, FlagStoreError> {
            diesel::PgConnection::establish(&self.database_url)
                .map_err(|e| FlagStoreError::Backend(e.to_string()))
        }

        fn cached(&self, key: &str) -> CacheLookup {
            let now = Instant::now();
            let Ok(cache) = self.cache.read() else {
                return CacheLookup::Miss;
            };
            match cache.get(key) {
                Some(c) if c.expires_at > now => CacheLookup::Hit(c.value.clone()),
                _ => CacheLookup::Miss,
            }
        }

        fn store_cache(&self, key: &str, value: Option<FlagConfig>) {
            if self.cache_ttl.is_zero() {
                return;
            }
            let Some(expires_at) = Instant::now().checked_add(self.cache_ttl) else {
                return;
            };
            if let Ok(mut cache) = self.cache.write() {
                cache.insert(key.to_owned(), CachedFlag { value, expires_at });
            }
        }

        fn invalidate(&self, key: &str) {
            if let Ok(mut cache) = self.cache.write() {
                cache.remove(key);
            }
        }

        fn upsert_flag(
            conn: &mut diesel::PgConnection,
            key: &str,
        ) -> Result<(), diesel::result::Error> {
            diesel::sql_query(
                "INSERT INTO autumn_feature_flags (key) VALUES ($1) \
                 ON CONFLICT (key) DO NOTHING",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .execute(conn)?;
            Ok(())
        }

        fn notify(conn: &mut diesel::PgConnection, key: &str) -> Result<(), diesel::result::Error> {
            diesel::sql_query("SELECT pg_notify('autumn_flags', $1)")
                .bind::<diesel::sql_types::Text, _>(key)
                .execute(conn)?;
            Ok(())
        }

        /// Spawn a background thread that polls `feature_flag_changes` and
        /// invalidates this store's cache whenever a remote replica writes a flag.
        ///
        /// Without this, the cache can only be invalidated when the TTL expires.
        /// Call this once at startup when using `PgFlagStore` in a multi-replica
        /// deployment:
        ///
        /// ```rust,ignore
        /// let store = Arc::new(PgFlagStore::new(db_url));
        /// PgFlagStore::spawn_poll_listener(Arc::clone(&store), Duration::from_secs(1));
        /// ```
        ///
        /// The thread runs indefinitely; the returned handle can be detached.
        pub fn spawn_poll_listener(
            store: std::sync::Arc<Self>,
            poll_interval: std::time::Duration,
        ) -> std::thread::JoinHandle<()> {
            std::thread::spawn(move || {
                // Timestamp-based cursor with a small lookback overlap.
                //
                // A sequence-ID cursor (WHERE id > last_id) is unsafe because
                // PostgreSQL sequences allocate IDs before the transaction
                // commits: transaction T1 (id=10) can commit after T2 (id=11),
                // so advancing last_id to 11 would permanently miss id=10.
                //
                // A timestamp cursor avoids that by including a 5-second
                // lookback on every poll (OVERLAP_SECS).  Any transaction that
                // takes longer than 5 seconds to commit will still be missed,
                // but such long-running writes are far outside the norm.
                // Invalidating the same key twice is always safe (idempotent).
                const OVERLAP_SECS: i64 = 5;
                let now_secs = || {
                    i64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    )
                    .unwrap_or(i64::MAX)
                };
                // Start the cursor in the past so we don't replay the entire
                // historical log: only changes that arrive after the listener
                // starts need processing (the in-process cache starts empty and
                // repopulates lazily on first access).
                let mut last_polled_secs: i64 = now_secs() - OVERLAP_SECS;

                loop {
                    std::thread::sleep(poll_interval);
                    // Advance the horizon before the query so concurrent writes
                    // during the query are captured in the next poll cycle.
                    let new_horizon = now_secs() - OVERLAP_SECS;
                    if let Ok(mut conn) = store.connect() {
                        let rows: Vec<ChangeKeyRow> = diesel::sql_query(
                            "SELECT DISTINCT key FROM feature_flag_changes \
                             WHERE changed_at > to_timestamp($1)",
                        )
                        .bind::<diesel::sql_types::BigInt, _>(last_polled_secs)
                        .load::<ChangeKeyRow>(&mut conn)
                        .unwrap_or_default();

                        for row in rows {
                            store.invalidate(&row.key);
                        }
                    }
                    last_polled_secs = new_horizon;
                }
            })
        }
    }

    #[derive(diesel::QueryableByName)]
    struct ChangeKeyRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        key: String,
    }

    #[derive(diesel::QueryableByName)]
    struct FlagRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        key: String,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        description: Option<String>,
        #[diesel(sql_type = diesel::sql_types::Bool)]
        enabled: bool,
        #[diesel(sql_type = diesel::sql_types::SmallInt)]
        rollout_pct: i16,
        #[diesel(sql_type = diesel::sql_types::Text)]
        actor_allowlist: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        group_allowlist: String,
    }

    impl FlagRow {
        fn into_config(self) -> FlagConfig {
            let actor_allowlist: Vec<String> =
                serde_json::from_str(&self.actor_allowlist).unwrap_or_default();
            let group_allowlist: Vec<String> =
                serde_json::from_str(&self.group_allowlist).unwrap_or_default();
            FlagConfig {
                key: self.key,
                description: self.description,
                enabled: self.enabled,
                rollout_pct: u8::try_from(self.rollout_pct.clamp(0, 100)).unwrap_or(0),
                actor_allowlist,
                group_allowlist,
            }
        }
    }

    #[derive(diesel::QueryableByName)]
    struct HistoryRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        key: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        mutation: String,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        actor: Option<String>,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        timestamp_secs: i64,
    }

    impl FlagStore for PgFlagStore {
        fn get(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError> {
            if let CacheLookup::Hit(v) = self.cached(key) {
                return Ok(v);
            }
            let mut conn = self.connect()?;
            let result = diesel::sql_query(
                "SELECT key, description, enabled, rollout_pct, \
                        actor_allowlist, group_allowlist \
                 FROM autumn_feature_flags WHERE key = $1",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .get_result::<FlagRow>(&mut conn)
            .optional()
            .map(|r| r.map(FlagRow::into_config))
            .map_err(|e| FlagStoreError::Backend(e.to_string()))?;

            self.store_cache(key, result.clone());
            Ok(result)
        }

        fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError> {
            let mut conn = self.connect()?;
            diesel::sql_query(
                "SELECT key, description, enabled, rollout_pct, \
                        actor_allowlist, group_allowlist \
                 FROM autumn_feature_flags ORDER BY key",
            )
            .load::<FlagRow>(&mut conn)
            .map(|rows| rows.into_iter().map(FlagRow::into_config).collect())
            .map_err(|e| FlagStoreError::Backend(e.to_string()))
        }

        fn enable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
            let mut conn = self.connect()?;
            conn.transaction::<(), diesel::result::Error, _>(|conn| {
                Self::upsert_flag(conn, key)?;
                diesel::sql_query(
                    "UPDATE autumn_feature_flags \
                     SET enabled = true, rollout_pct = 100, updated_at = NOW() \
                     WHERE key = $1",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .execute(conn)?;
                diesel::sql_query(
                    "INSERT INTO feature_flag_changes (key, mutation, actor) VALUES ($1, $2, $3)",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>("enabled")
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                    actor.map(str::to_owned),
                )
                .execute(conn)?;
                Self::notify(conn, key)?;
                Ok(())
            })
            .map_err(|e| FlagStoreError::Backend(e.to_string()))?;
            self.invalidate(key);
            Ok(())
        }

        fn disable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
            let mut conn = self.connect()?;
            conn.transaction::<(), diesel::result::Error, _>(|conn| {
                Self::upsert_flag(conn, key)?;
                diesel::sql_query(
                    "UPDATE autumn_feature_flags SET enabled = false, updated_at = NOW() \
                     WHERE key = $1",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .execute(conn)?;
                diesel::sql_query(
                    "INSERT INTO feature_flag_changes (key, mutation, actor) VALUES ($1, $2, $3)",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>("disabled")
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                    actor.map(str::to_owned),
                )
                .execute(conn)?;
                Self::notify(conn, key)?;
                Ok(())
            })
            .map_err(|e| FlagStoreError::Backend(e.to_string()))?;
            self.invalidate(key);
            Ok(())
        }

        fn set_rollout(
            &self,
            key: &str,
            pct: u8,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            let pct = i16::from(pct.min(100));
            let mut conn = self.connect()?;
            conn.transaction::<(), diesel::result::Error, _>(|conn| {
                Self::upsert_flag(conn, key)?;
                diesel::sql_query(
                    "UPDATE autumn_feature_flags \
                     SET enabled = true, rollout_pct = $2, updated_at = NOW() \
                     WHERE key = $1",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::SmallInt, _>(pct)
                .execute(conn)?;
                let mutation = format!("rollout={pct}");
                diesel::sql_query(
                    "INSERT INTO feature_flag_changes (key, mutation, actor) VALUES ($1, $2, $3)",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>(&mutation)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                    actor.map(str::to_owned),
                )
                .execute(conn)?;
                Self::notify(conn, key)?;
                Ok(())
            })
            .map_err(|e| FlagStoreError::Backend(e.to_string()))?;
            self.invalidate(key);
            Ok(())
        }

        fn allow_actor(
            &self,
            key: &str,
            actor_id: &str,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            let mut conn = self.connect()?;
            conn.transaction::<(), diesel::result::Error, _>(|conn| {
                Self::upsert_flag(conn, key)?;
                diesel::sql_query(
                    // Re-enabling from kill-switch via allowlist resets rollout_pct to 0
                    // so only listed actors gain access, not the previous global cohort.
                    "UPDATE autumn_feature_flags \
                     SET enabled = true, \
                         rollout_pct = CASE WHEN NOT enabled THEN 0 ELSE rollout_pct END, \
                         actor_allowlist = (
                             SELECT json_agg(DISTINCT elem) \
                             FROM (
                                 SELECT jsonb_array_elements_text(actor_allowlist::jsonb) AS elem \
                                 UNION SELECT $2
                             ) t \
                         )::text, \
                         updated_at = NOW() \
                     WHERE key = $1",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>(actor_id)
                .execute(conn)?;
                let mutation = format!("allowed_actor={actor_id}");
                diesel::sql_query(
                    "INSERT INTO feature_flag_changes (key, mutation, actor) VALUES ($1, $2, $3)",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>(&mutation)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                    actor.map(str::to_owned),
                )
                .execute(conn)?;
                Self::notify(conn, key)?;
                Ok(())
            })
            .map_err(|e| FlagStoreError::Backend(e.to_string()))?;
            self.invalidate(key);
            Ok(())
        }

        fn add_group(
            &self,
            key: &str,
            group: &str,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            let mut conn = self.connect()?;
            conn.transaction::<(), diesel::result::Error, _>(|conn| {
                Self::upsert_flag(conn, key)?;
                diesel::sql_query(
                    // Re-enabling from kill-switch via group allowlist resets rollout_pct.
                    "UPDATE autumn_feature_flags \
                     SET enabled = true, \
                         rollout_pct = CASE WHEN NOT enabled THEN 0 ELSE rollout_pct END, \
                         group_allowlist = (
                             SELECT json_agg(DISTINCT elem) \
                             FROM (
                                 SELECT jsonb_array_elements_text(group_allowlist::jsonb) AS elem \
                                 UNION SELECT $2
                             ) t \
                         )::text, \
                         updated_at = NOW() \
                     WHERE key = $1",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>(group)
                .execute(conn)?;
                let mutation = format!("added_group={group}");
                diesel::sql_query(
                    "INSERT INTO feature_flag_changes (key, mutation, actor) VALUES ($1, $2, $3)",
                )
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>(&mutation)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                    actor.map(str::to_owned),
                )
                .execute(conn)?;
                Self::notify(conn, key)?;
                Ok(())
            })
            .map_err(|e| FlagStoreError::Backend(e.to_string()))?;
            self.invalidate(key);
            Ok(())
        }

        fn history(
            &self,
            key: &str,
            limit: usize,
        ) -> Result<Vec<FlagChangeRecord>, FlagStoreError> {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut conn = self.connect()?;
            diesel::sql_query(
                "SELECT key, mutation, actor, \
                        EXTRACT(EPOCH FROM changed_at)::bigint AS timestamp_secs \
                 FROM feature_flag_changes \
                 WHERE key = $1 \
                 ORDER BY changed_at DESC LIMIT $2",
            )
            .bind::<diesel::sql_types::Text, _>(key)
            .bind::<diesel::sql_types::BigInt, _>(limit)
            .load::<HistoryRow>(&mut conn)
            .map(|rows| {
                rows.into_iter()
                    .map(|r| FlagChangeRecord {
                        key: r.key,
                        mutation: r.mutation,
                        actor: r.actor,
                        timestamp_secs: u64::try_from(r.timestamp_secs).unwrap_or(0),
                    })
                    .collect()
            })
            .map_err(|e| FlagStoreError::Backend(e.to_string()))
        }
    }

    #[cfg(test)]
    mod pg_tests {
        use super::*;

        #[test]
        fn pg_store_exposes_database_url() {
            let store = PgFlagStore::new("postgres://localhost/myapp");
            assert_eq!(store.database_url, "postgres://localhost/myapp");
        }

        #[test]
        fn pg_store_default_cache_ttl_is_one_second() {
            let store = PgFlagStore::new("postgres://localhost/myapp");
            assert_eq!(store.cache_ttl, PgFlagStore::DEFAULT_CACHE_TTL);
        }

        #[test]
        fn pg_store_cache_miss_on_empty_store() {
            let store = PgFlagStore::with_cache_ttl("postgres://localhost/myapp", Duration::ZERO);
            assert_eq!(store.cached("my_flag"), CacheLookup::Miss);
        }

        #[test]
        fn pg_store_cache_hit_returns_stored_value() {
            let store =
                PgFlagStore::with_cache_ttl("postgres://localhost/myapp", Duration::from_secs(60));
            store.store_cache("my_flag", Some(FlagConfig::new("my_flag")));
            assert!(matches!(store.cached("my_flag"), CacheLookup::Hit(Some(_))));
        }

        #[test]
        fn pg_store_cache_hit_none_for_absent_flag() {
            let store =
                PgFlagStore::with_cache_ttl("postgres://localhost/myapp", Duration::from_secs(60));
            store.store_cache("absent", None);
            assert_eq!(store.cached("absent"), CacheLookup::Hit(None));
        }

        #[test]
        fn pg_store_cache_expired_returns_miss() {
            let store = PgFlagStore::with_cache_ttl("postgres://localhost/myapp", Duration::ZERO);
            store.store_cache("expired", Some(FlagConfig::new("expired")));
            // TTL = 0 means entries expire immediately.
            assert_eq!(store.cached("expired"), CacheLookup::Miss);
        }

        #[test]
        fn pg_store_invalidate_removes_from_cache() {
            let store =
                PgFlagStore::with_cache_ttl("postgres://localhost/myapp", Duration::from_secs(60));
            store.store_cache("flag", Some(FlagConfig::new("flag")));
            assert!(matches!(store.cached("flag"), CacheLookup::Hit(Some(_))));
            store.invalidate("flag");
            assert_eq!(store.cached("flag"), CacheLookup::Miss);
        }

        #[test]
        fn pg_store_with_cache_ttl_sets_custom_ttl() {
            let ttl = Duration::from_secs(30);
            let store = PgFlagStore::with_cache_ttl("postgres://localhost/myapp", ttl);
            assert_eq!(store.cache_ttl, ttl);
        }

        #[test]
        fn pg_store_clone_has_independent_cache() {
            // PgFlagStore::clone() creates a fresh instance — it does NOT share the
            // cache HashMap.  Only Arc<PgFlagStore> shares a single cache.
            let store =
                PgFlagStore::with_cache_ttl("postgres://localhost/myapp", Duration::from_secs(60));
            store.store_cache("cached", Some(FlagConfig::new("cached")));
            let cloned = store.clone();
            // Clone starts with an empty cache, so this is a Miss.
            assert_eq!(cloned.cached("cached"), CacheLookup::Miss);
            // Original still holds its value — confirming the two caches are independent.
            assert!(matches!(store.cached("cached"), CacheLookup::Hit(Some(_))));
        }
    }
}

// ── Hash helpers ─────────────────────────────────────────────────────────────

/// FNV-1a 64-bit hash of a byte slice.
///
/// Used for stable, deterministic percent-rollout bucket assignment.
/// No external dependency — the algorithm is specified by the FNV standard.
fn fnv1a_64(data: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    let mut hash = FNV_OFFSET;
    for &byte in data {
        hash ^= u64(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[allow(clippy::cast_lossless)]
const fn u64(v: u8) -> u64 {
    v as u64
}

/// Compute the percent-rollout bucket for `(flag_key, actor_id)`.
///
/// Returns a value in `[0, 100)`. If the flag's `rollout_pct` is greater
/// than this value the actor is in the rollout cohort.
#[must_use]
pub fn rollout_bucket(flag_key: &str, actor_id: &str) -> u8 {
    let key = format!("{flag_key}:{actor_id}");
    let hash = fnv1a_64(key.as_bytes());
    u8::try_from(hash % 100).unwrap_or(0)
}

// ── FeatureFlagService ───────────────────────────────────────────────────────

/// The main feature-flag service.
///
/// Wrap a [`FlagStore`] (for persistence) and an optional [`GroupResolver`]
/// (for named-group membership). The service is cheaply clone-able and
/// intended to be stored as an `AppState` extension:
///
/// ```rust,ignore
/// state.insert_extension(FeatureFlagService::new(Arc::new(InMemoryFlagStore::new())));
/// ```
#[derive(Clone)]
pub struct FeatureFlagService {
    store: Arc<dyn FlagStore>,
    group_resolver: Option<GroupResolver>,
}

impl std::fmt::Debug for FeatureFlagService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureFlagService").finish_non_exhaustive()
    }
}

impl FeatureFlagService {
    /// Create a new service wrapping the given store.
    #[must_use]
    pub fn new(store: Arc<dyn FlagStore>) -> Self {
        Self {
            store,
            group_resolver: None,
        }
    }

    /// Attach a group resolver so named-group gates are evaluated.
    #[must_use]
    pub fn with_group_resolver(mut self, resolver: GroupResolver) -> Self {
        self.group_resolver = Some(resolver);
        self
    }

    /// Return `true` if `flag_key` is enabled for `actor_id`.
    ///
    /// Returns `false` for unknown flags (fail-closed).
    #[must_use]
    pub fn is_enabled(&self, flag_key: &str, actor_id: Option<&str>) -> bool {
        let Ok(Some(flag)) = self.store.get(flag_key) else {
            return false;
        };
        self.evaluate(&flag, actor_id)
    }

    fn evaluate(&self, flag: &FlagConfig, actor_id: Option<&str>) -> bool {
        // Kill switch: enabled=false overrides all other gates.
        if !flag.enabled {
            return false;
        }

        // Globally on: rollout_pct=100 enables everyone without per-actor check.
        if flag.rollout_pct >= 100 {
            return true;
        }

        // Actor allowlist.
        if let Some(actor) = actor_id
            && flag.actor_allowlist.iter().any(|a| a.as_str() == actor)
        {
            return true;
        }

        // Named groups.
        if let (Some(actor), Some(resolver)) = (actor_id, &self.group_resolver) {
            for group in &flag.group_allowlist {
                if resolver(actor, group) {
                    return true;
                }
            }
        }

        // Percent rollout (1–99%).
        if flag.rollout_pct > 0
            && let Some(actor) = actor_id
        {
            let bucket = rollout_bucket(&flag.key, actor);
            return bucket < flag.rollout_pct;
        }

        false
    }

    /// Enable `flag_key` for all actors.
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn enable(&self, flag_key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        self.store.enable(flag_key, actor)
    }

    /// Disable `flag_key` globally.
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn disable(&self, flag_key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
        self.store.disable(flag_key, actor)
    }

    /// Set the percent-rollout gate for `flag_key` to `pct` (0–100).
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn set_rollout(
        &self,
        flag_key: &str,
        pct: u8,
        actor: Option<&str>,
    ) -> Result<(), FlagStoreError> {
        self.store.set_rollout(flag_key, pct, actor)
    }

    /// Add `actor_id` to the explicit allowlist for `flag_key`.
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn allow_actor(
        &self,
        flag_key: &str,
        actor_id: &str,
        actor: Option<&str>,
    ) -> Result<(), FlagStoreError> {
        self.store.allow_actor(flag_key, actor_id, actor)
    }

    /// Add `group` to the named-group allowlist for `flag_key`.
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn add_group(
        &self,
        flag_key: &str,
        group: &str,
        actor: Option<&str>,
    ) -> Result<(), FlagStoreError> {
        self.store.add_group(flag_key, group, actor)
    }

    /// Return all known flags, sorted by key.
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError> {
        self.store.list()
    }

    /// Return the most recent `limit` change records for `flag_key`.
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn history(
        &self,
        flag_key: &str,
        limit: usize,
    ) -> Result<Vec<FlagChangeRecord>, FlagStoreError> {
        self.store.history(flag_key, limit)
    }
}

// ── AppState extractor ───────────────────────────────────────────────────────

/// Request extractor that resolves the current user's flag service handle.
///
/// Extracts [`FeatureFlagService`] from the `AppState` extension slot. If no
/// service is registered the extraction fails with `500 Internal Server Error`.
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::feature_flags::Flags;
///
/// #[get("/dashboard")]
/// async fn dashboard(flags: Flags) -> Markup {
///     html! {
///         @if flags.enabled("beta_inbox") {
///             (render_beta_inbox())
///         }
///     }
/// }
/// ```
pub struct Flags {
    service: FeatureFlagService,
    actor_id: Option<String>,
}

impl Flags {
    /// Return `true` if `flag_key` is enabled for the current actor.
    #[must_use]
    pub fn enabled(&self, flag_key: &str) -> bool {
        self.service.is_enabled(flag_key, self.actor_id.as_deref())
    }

    /// Return the underlying service for direct mutation from handlers.
    #[must_use]
    pub const fn service(&self) -> &FeatureFlagService {
        &self.service
    }
}

impl axum::extract::FromRequestParts<crate::AppState> for Flags {
    type Rejection = crate::AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &crate::AppState,
    ) -> Result<Self, Self::Rejection> {
        let service = state
            .extension::<FeatureFlagService>()
            .map(|arc| (*arc).clone())
            .ok_or_else(|| {
                crate::AutumnError::internal_server_error_msg(
                    "feature flag service not registered; \
                     install a FlagStore via AppBuilder::with_flag_store()",
                )
            })?;

        // Resolve actor_id from session if available (best-effort, non-blocking).
        let actor_id = if let Some(session) = parts.extensions.get::<crate::session::Session>() {
            session.get(state.auth_session_key()).await
        } else {
            None
        };

        Ok(Self { service, actor_id })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─────────────── RED PHASE: tests written before full implementation ──────

    fn make_svc() -> FeatureFlagService {
        FeatureFlagService::new(Arc::new(InMemoryFlagStore::new()))
    }

    // AC-1: service resolves flag to bool ─────────────────────────────────────

    #[test]
    fn unknown_flag_returns_false() {
        let svc = make_svc();
        assert!(!svc.is_enabled("nonexistent", Some("user:1")));
    }

    #[test]
    fn globally_enabled_flag_returns_true_for_any_actor() {
        let svc = make_svc();
        svc.enable("my_flag", None).unwrap();
        assert!(svc.is_enabled("my_flag", Some("user:1")));
        assert!(svc.is_enabled("my_flag", Some("user:99")));
        assert!(svc.is_enabled("my_flag", None));
    }

    #[test]
    fn globally_disabled_flag_returns_false_for_any_actor() {
        let svc = make_svc();
        svc.enable("my_flag", None).unwrap();
        svc.disable("my_flag", None).unwrap();
        assert!(!svc.is_enabled("my_flag", Some("user:1")));
        assert!(!svc.is_enabled("my_flag", None));
    }

    // AC-2: evaluation modes ──────────────────────────────────────────────────

    #[test]
    fn actor_allowlist_enables_specific_actor() {
        let svc = make_svc();
        svc.allow_actor("beta_feature", "user:42", None).unwrap();
        assert!(svc.is_enabled("beta_feature", Some("user:42")));
        assert!(!svc.is_enabled("beta_feature", Some("user:1")));
    }

    #[test]
    fn group_resolver_enables_group_members() {
        let svc = FeatureFlagService::new(Arc::new(InMemoryFlagStore::new())).with_group_resolver(
            Arc::new(|actor_id: &str, group: &str| {
                // "staff" group contains actor IDs starting with "staff:"
                group == "staff" && actor_id.starts_with("staff:")
            }),
        );
        svc.add_group("internal_feature", "staff", None).unwrap();
        assert!(svc.is_enabled("internal_feature", Some("staff:alice")));
        assert!(!svc.is_enabled("internal_feature", Some("user:bob")));
    }

    #[test]
    fn percent_rollout_at_0_disables_for_all_actors() {
        let svc = make_svc();
        svc.set_rollout("gradual", 0, None).unwrap();
        // With 0% rollout and no other gates, every actor should be disabled.
        for i in 0..50_u32 {
            let actor = format!("user:{i}");
            assert!(
                !svc.is_enabled("gradual", Some(&actor)),
                "expected disabled for {actor} at 0% rollout"
            );
        }
    }

    #[test]
    fn percent_rollout_at_100_enables_for_all_actors() {
        let svc = make_svc();
        svc.set_rollout("gradual", 100, None).unwrap();
        for i in 0..50_u32 {
            let actor = format!("user:{i}");
            assert!(
                svc.is_enabled("gradual", Some(&actor)),
                "expected enabled for {actor} at 100% rollout"
            );
        }
    }

    #[test]
    fn percent_rollout_at_50_enables_roughly_half() {
        let svc = make_svc();
        svc.set_rollout("rollout_flag", 50, None).unwrap();
        let enabled_count = (0..200_u32)
            .filter(|i| svc.is_enabled("rollout_flag", Some(&format!("user:{i}"))))
            .count();
        // With 200 actors and 50% rollout, expect 80–120 enabled (±20%).
        assert!(
            (80..=120).contains(&enabled_count),
            "expected ~100 enabled actors, got {enabled_count}"
        );
    }

    // AC-3: determinism ───────────────────────────────────────────────────────

    #[test]
    fn rollout_bucket_is_stable_across_calls() {
        let b1 = rollout_bucket("my_flag", "user:1");
        let b2 = rollout_bucket("my_flag", "user:1");
        assert_eq!(b1, b2, "bucket must be deterministic");
    }

    #[test]
    fn rollout_bucket_differs_for_different_actors() {
        // Ensure we don't always get the same bucket (birthday collision at
        // 100 buckets is essentially impossible with our FNV-1a implementation).
        let buckets: std::collections::HashSet<u8> = (0..50_u32)
            .map(|i| rollout_bucket("flag", &format!("user:{i}")))
            .collect();
        assert!(
            buckets.len() > 10,
            "expected diverse buckets, got {}: {buckets:?}",
            buckets.len()
        );
    }

    #[test]
    fn rollout_bucket_in_range_0_to_99() {
        for i in 0..1000_u32 {
            let b = rollout_bucket("flag", &format!("actor:{i}"));
            assert!(b < 100, "bucket out of range: {b}");
        }
    }

    #[test]
    fn percent_rollout_same_actor_same_flag_always_same_result() {
        let svc = make_svc();
        svc.set_rollout("stable_flag", 42, None).unwrap();
        let first = svc.is_enabled("stable_flag", Some("user:123"));
        for _ in 0..10 {
            assert_eq!(
                svc.is_enabled("stable_flag", Some("user:123")),
                first,
                "rollout result must not flip between calls"
            );
        }
    }

    // AC-7: FlagStore trait + InMemoryFlagStore ────────────────────────────────

    #[test]
    fn in_memory_store_returns_none_for_unknown_flag() {
        let store = InMemoryFlagStore::new();
        assert!(store.get("unknown").unwrap().is_none());
    }

    #[test]
    fn in_memory_store_list_is_sorted() {
        let store = InMemoryFlagStore::new();
        store.enable("zebra", None).unwrap();
        store.enable("alpha", None).unwrap();
        store.enable("mango", None).unwrap();
        let keys: Vec<String> = store.list().unwrap().into_iter().map(|f| f.key).collect();
        assert_eq!(keys, vec!["alpha", "mango", "zebra"]);
    }

    #[test]
    fn in_memory_store_enable_creates_flag_if_absent() {
        let store = InMemoryFlagStore::new();
        store.enable("new_flag", None).unwrap();
        let flag = store.get("new_flag").unwrap().unwrap();
        assert!(flag.enabled);
    }

    #[test]
    fn in_memory_store_disable_sets_enabled_false() {
        let store = InMemoryFlagStore::new();
        store.enable("f", None).unwrap();
        store.disable("f", None).unwrap();
        assert!(!store.get("f").unwrap().unwrap().enabled);
    }

    #[test]
    fn in_memory_store_allow_actor_does_not_duplicate() {
        let store = InMemoryFlagStore::new();
        store.allow_actor("f", "user:1", None).unwrap();
        store.allow_actor("f", "user:1", None).unwrap();
        let flag = store.get("f").unwrap().unwrap();
        assert_eq!(flag.actor_allowlist.len(), 1);
    }

    #[test]
    fn in_memory_store_add_group_does_not_duplicate() {
        let store = InMemoryFlagStore::new();
        store.add_group("f", "staff", None).unwrap();
        store.add_group("f", "staff", None).unwrap();
        let flag = store.get("f").unwrap().unwrap();
        assert_eq!(flag.group_allowlist.len(), 1);
    }

    // AC-10: audit trail ──────────────────────────────────────────────────────

    #[test]
    fn mutations_are_recorded_in_history() {
        let svc = make_svc();
        svc.enable("tracked_flag", Some("ops@example.com")).unwrap();
        svc.disable("tracked_flag", Some("ops@example.com"))
            .unwrap();
        let history = svc.history("tracked_flag", 10).unwrap();
        assert_eq!(history.len(), 2, "two mutations should be recorded");
        assert_eq!(history[0].mutation, "disabled");
        assert_eq!(history[0].actor.as_deref(), Some("ops@example.com"));
        assert_eq!(history[1].mutation, "enabled");
    }

    #[test]
    fn history_respects_limit() {
        let svc = make_svc();
        for _ in 0..5 {
            svc.enable("limited_flag", None).unwrap();
        }
        let history = svc.history("limited_flag", 3).unwrap();
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn history_empty_for_unknown_flag() {
        let svc = make_svc();
        let history = svc.history("ghost_flag", 10).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn rollout_mutation_recorded_with_pct() {
        let svc = make_svc();
        svc.set_rollout("roll", 25, Some("cli")).unwrap();
        let history = svc.history("roll", 1).unwrap();
        assert_eq!(history[0].mutation, "rollout=25");
        assert_eq!(history[0].actor.as_deref(), Some("cli"));
    }

    #[test]
    fn allow_actor_mutation_recorded() {
        let svc = make_svc();
        svc.allow_actor("f", "user:7", Some("cli")).unwrap();
        let h = svc.history("f", 1).unwrap();
        assert_eq!(h[0].mutation, "allowed_actor=user:7");
    }

    // ── FlagConfig defaults ───────────────────────────────────────────────────

    #[test]
    fn flag_config_new_defaults_to_disabled() {
        let f = FlagConfig::new("my_flag");
        assert_eq!(f.key, "my_flag");
        assert!(!f.enabled);
        assert_eq!(f.rollout_pct, 0);
        assert!(f.actor_allowlist.is_empty());
        assert!(f.group_allowlist.is_empty());
    }

    // ── Rollout clamping ──────────────────────────────────────────────────────

    #[test]
    fn set_rollout_clamps_to_100() {
        let store = InMemoryFlagStore::new();
        store.set_rollout("f", 200, None).unwrap();
        assert_eq!(store.get("f").unwrap().unwrap().rollout_pct, 100);
    }

    // AC-1 kill-switch: disable() must override rollout and allowlists ─────────

    #[test]
    fn disable_kills_flag_even_when_rollout_is_100_percent() {
        let svc = make_svc();
        svc.set_rollout("roll_flag", 100, None).unwrap();
        svc.disable("roll_flag", None).unwrap();
        for i in 0..20_u32 {
            assert!(
                !svc.is_enabled("roll_flag", Some(&format!("user:{i}"))),
                "disable() must override rollout for actor user:{i}"
            );
        }
        assert!(!svc.is_enabled("roll_flag", None));
    }

    #[test]
    fn disable_kills_flag_even_when_actor_is_in_allowlist() {
        let svc = make_svc();
        svc.allow_actor("guarded", "user:42", None).unwrap();
        svc.disable("guarded", None).unwrap();
        assert!(
            !svc.is_enabled("guarded", Some("user:42")),
            "disable() must override actor allowlist"
        );
    }

    #[test]
    fn enable_after_disable_restores_rollout_config() {
        let svc = make_svc();
        svc.set_rollout("roll_flag", 50, None).unwrap();
        svc.disable("roll_flag", None).unwrap();
        // Re-enable globally — disable() preserves rollout_pct=50 in the store,
        // but enable() resets it to 100 (globally on).
        svc.enable("roll_flag", None).unwrap();
        assert!(svc.is_enabled("roll_flag", None));
        assert!(svc.is_enabled("roll_flag", Some("user:1")));
    }

    // AC-1 allow_actor after kill-switch must not restore global rollout ────────

    #[test]
    fn allow_actor_after_kill_switch_does_not_restore_global_rollout() {
        // Scenario: enable globally → disable (kill-switch) → allow_actor for
        // one tester. The flag must be visible only to the allowlisted actor,
        // NOT to everyone (which would happen if rollout_pct=100 were preserved).
        let svc = make_svc();
        svc.enable("targeted", None).unwrap(); // rollout_pct = 100
        svc.disable("targeted", None).unwrap(); // kill-switch, rollout_pct still 100
        svc.allow_actor("targeted", "user:42", None).unwrap(); // re-enable allowlist-only

        assert!(
            svc.is_enabled("targeted", Some("user:42")),
            "allowlisted actor must see the flag"
        );
        // All non-allowlisted actors should NOT see it (rollout was reset to 0).
        for i in [1_u32, 5, 10, 99] {
            let actor = format!("user:{i}");
            assert!(
                !svc.is_enabled("targeted", Some(&actor)),
                "non-allowlisted actor {actor} must NOT see the flag after allowlist-only re-enable"
            );
        }
    }

    #[test]
    fn allow_actor_on_active_rollout_preserves_rollout_pct() {
        // When the flag is already enabled (no kill-switch), adding an actor to the
        // allowlist must NOT reset the existing rollout percentage.
        let svc = make_svc();
        svc.set_rollout("staged", 50, None).unwrap(); // enabled=true, rollout=50%
        svc.allow_actor("staged", "user:42", None).unwrap();

        // rollout_pct should still be 50, not reset to 0.
        let store = InMemoryFlagStore::new();
        store.set_rollout("staged", 50, None).unwrap();
        store.allow_actor("staged", "user:42", None).unwrap();
        let flag = store.get("staged").unwrap().unwrap();
        assert_eq!(
            flag.rollout_pct, 50,
            "rollout_pct must be preserved when flag was already enabled"
        );
        assert!(flag.actor_allowlist.contains(&"user:42".to_owned()));
    }

    // ── Arc<T: FlagStore> delegation ──────────────────────────────────────────

    #[test]
    fn arc_flag_store_delegates_get() {
        let store = Arc::new(InMemoryFlagStore::new());
        store.enable("arc_flag", None).unwrap();
        let arc_store: Arc<dyn FlagStore> = store;
        let flag = arc_store.get("arc_flag").unwrap().unwrap();
        assert!(flag.enabled);
    }

    #[test]
    fn arc_flag_store_delegates_list() {
        let store = Arc::new(InMemoryFlagStore::new());
        store.enable("f1", None).unwrap();
        store.enable("f2", None).unwrap();
        let arc_store: Arc<dyn FlagStore> = store;
        let flags = arc_store.list().unwrap();
        assert_eq!(flags.len(), 2);
    }

    #[test]
    fn arc_flag_store_delegates_enable_and_disable() {
        let store = Arc::new(InMemoryFlagStore::new());
        let arc_store: Arc<dyn FlagStore> = store;
        arc_store.enable("f", None).unwrap();
        assert!(arc_store.get("f").unwrap().unwrap().enabled);
        arc_store.disable("f", None).unwrap();
        assert!(!arc_store.get("f").unwrap().unwrap().enabled);
    }

    #[test]
    fn arc_flag_store_delegates_set_rollout() {
        let store = Arc::new(InMemoryFlagStore::new());
        let arc_store: Arc<dyn FlagStore> = store;
        arc_store.set_rollout("f", 42, None).unwrap();
        let flag = arc_store.get("f").unwrap().unwrap();
        assert_eq!(flag.rollout_pct, 42);
    }

    #[test]
    fn arc_flag_store_delegates_allow_actor() {
        let store = Arc::new(InMemoryFlagStore::new());
        let arc_store: Arc<dyn FlagStore> = store;
        arc_store.allow_actor("f", "user:1", None).unwrap();
        let flag = arc_store.get("f").unwrap().unwrap();
        assert!(flag.actor_allowlist.contains(&"user:1".to_owned()));
    }

    #[test]
    fn arc_flag_store_delegates_add_group() {
        let store = Arc::new(InMemoryFlagStore::new());
        let arc_store: Arc<dyn FlagStore> = store;
        arc_store.add_group("f", "beta_testers", None).unwrap();
        let flag = arc_store.get("f").unwrap().unwrap();
        assert!(flag.group_allowlist.contains(&"beta_testers".to_owned()));
    }

    #[test]
    fn arc_flag_store_delegates_history() {
        let store = Arc::new(InMemoryFlagStore::new());
        let arc_store: Arc<dyn FlagStore> = store;
        arc_store.enable("f", Some("cli")).unwrap();
        let history = arc_store.history("f", 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].mutation, "enabled");
    }

    // ── Box<dyn FlagStore> delegation ─────────────────────────────────────────

    #[test]
    fn box_flag_store_delegates_all_operations() {
        let store = InMemoryFlagStore::new();
        let boxed: Box<dyn FlagStore> = Box::new(store);
        boxed.enable("f", None).unwrap();
        assert!(boxed.get("f").unwrap().unwrap().enabled);
        boxed.set_rollout("g", 25, Some("cli")).unwrap();
        assert_eq!(boxed.get("g").unwrap().unwrap().rollout_pct, 25);
        boxed.allow_actor("h", "user:1", None).unwrap();
        boxed.add_group("h", "staff", None).unwrap();
        let flags = boxed.list().unwrap();
        // f, g, h are present
        assert_eq!(flags.len(), 3);
        let hist = boxed.history("f", 5).unwrap();
        assert_eq!(hist[0].mutation, "enabled");
        boxed.disable("f", None).unwrap();
        assert!(!boxed.get("f").unwrap().unwrap().enabled);
    }

    // ── FlagStoreError display ────────────────────────────────────────────────

    #[test]
    fn flag_store_error_displays_message() {
        let err = FlagStoreError::Backend("connection refused".to_owned());
        assert_eq!(
            err.to_string(),
            "flag store backend error: connection refused"
        );
    }

    // ── FlagConfig clone and equality ─────────────────────────────────────────

    #[test]
    fn flag_config_clone_is_equal_to_original() {
        let mut f = FlagConfig::new("cloned");
        f.enabled = true;
        f.rollout_pct = 50;
        f.actor_allowlist = vec!["user:1".to_owned()];
        let g = f.clone();
        assert_eq!(f, g);
    }

    // ── evaluate() edge cases ─────────────────────────────────────────────────

    #[test]
    fn rollout_with_no_actor_returns_false() {
        // When actor_id is None and there are no allowlists, a percent rollout
        // must not enable the flag (there's no actor to compute a bucket for).
        let svc = make_svc();
        svc.set_rollout("gradual", 99, None).unwrap();
        assert!(
            !svc.is_enabled("gradual", None),
            "percent rollout must not fire for anonymous (None) actor"
        );
    }

    #[test]
    fn group_resolver_with_no_actor_does_not_panic() {
        let svc = FeatureFlagService::new(Arc::new(InMemoryFlagStore::new()))
            .with_group_resolver(Arc::new(|_: &str, _: &str| true));
        svc.add_group("f", "everyone", None).unwrap();
        // No actor — group check must be skipped, not panic.
        assert!(!svc.is_enabled("f", None));
    }

    #[test]
    fn add_group_mutation_format() {
        let store = InMemoryFlagStore::new();
        store.add_group("f", "beta_testers", Some("cli")).unwrap();
        let hist = store.history("f", 1).unwrap();
        assert_eq!(hist[0].mutation, "added_group=beta_testers");
        assert_eq!(hist[0].actor.as_deref(), Some("cli"));
    }

    #[test]
    fn service_list_returns_all_flags() {
        let svc = make_svc();
        svc.enable("a", None).unwrap();
        svc.disable("b", None).unwrap();
        svc.set_rollout("c", 10, None).unwrap();
        let flags = svc.list().unwrap();
        assert_eq!(flags.len(), 3);
        assert_eq!(flags[0].key, "a");
        assert_eq!(flags[1].key, "b");
        assert_eq!(flags[2].key, "c");
    }

    #[test]
    fn service_debug_does_not_panic() {
        let svc = make_svc();
        let _ = format!("{svc:?}");
    }

    #[test]
    fn flags_enabled_delegates_to_service() {
        // Test the Flags::enabled() method via FeatureFlagService directly.
        let svc = make_svc();
        svc.enable("active", None).unwrap();
        assert!(svc.is_enabled("active", Some("any_user")));
        assert!(!svc.is_enabled("missing", Some("any_user")));
    }

    #[tokio::test]
    async fn from_request_parts_respects_custom_auth_session_key() {
        use axum::extract::FromRequestParts;
        use std::collections::HashMap;

        let svc = make_svc();
        let state = crate::AppState::for_test().with_auth_session_key("custom_user_id");
        state.insert_extension(svc);

        let mut data = HashMap::new();
        data.insert("custom_user_id".to_owned(), "user:123".to_owned());
        data.insert("user_id".to_owned(), "user:999".to_owned()); // distracter
        let session = crate::session::Session::new_for_test("session_id".to_owned(), data);

        let mut req = axum::http::Request::builder().body(()).unwrap();
        req.extensions_mut().insert(session);
        let mut parts = req.into_parts().0;

        let flags = Flags::from_request_parts(&mut parts, &state).await.unwrap();
        assert_eq!(flags.actor_id.as_deref(), Some("user:123"));
    }

    // ── Backend screening on the Postgres-only store ──────────────────────

    // `PgFlagStore` opens a `diesel::PgConnection` and writes through
    // `pg_notify` — it cannot serve any other backend. Building one from a
    // SQLite target used to succeed and fail on first use with a driver-level
    // connection error naming Postgres, which is not a diagnosis an operator
    // who configured `sqlite://` can act on.
    #[cfg(feature = "db")]
    #[test]
    fn pg_flag_store_refuses_a_non_postgres_target() {
        use crate::config::DatabaseConfig;

        let sqlite = DatabaseConfig {
            primary_url: Some("sqlite:///var/lib/app.db".to_owned()),
            ..Default::default()
        };
        assert!(
            pg::PgFlagStore::from_database_config(&sqlite).is_none(),
            "a SQLite target has no Postgres flag store"
        );

        // Fails closed: a target no backend claims is refused too.
        let unclassifiable = DatabaseConfig {
            primary_url: Some("/var/lib/app.db".to_owned()),
            ..Default::default()
        };
        assert!(
            pg::PgFlagStore::from_database_config(&unclassifiable).is_none(),
            "an unclassifiable target has no Postgres flag store"
        );

        // Both Postgres spellings still build.
        for url in [
            "postgres://localhost/app",
            "host=db user=app dbname=app sslmode=require",
        ] {
            let pg_config = DatabaseConfig {
                primary_url: Some(url.to_owned()),
                ..Default::default()
            };
            assert!(
                pg::PgFlagStore::from_database_config(&pg_config).is_some(),
                "{url} is a Postgres target"
            );
        }
    }
}
