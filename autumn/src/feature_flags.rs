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
//! // 3. Disable it. With the Postgres store, all replicas see the change on
//! //    their next refresh (about one poll interval).
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

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

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
        let timestamp_secs = crate::time::clock_unix_secs(&crate::time::AmbientClock);
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

    /// Load the store before the first read. This can block.
    ///
    /// [`AppBuilder::with_flag_store`](crate::app::AppBuilder::with_flag_store)
    /// calls it once at startup on the blocking pool. The default does nothing.
    ///
    /// # Errors
    ///
    /// Returns a [`FlagStoreError`] on backend failure.
    fn preload(&self) -> Result<(), FlagStoreError> {
        Ok(())
    }
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
    fn preload(&self) -> Result<(), FlagStoreError> {
        (**self).preload()
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
    fn preload(&self) -> Result<(), FlagStoreError> {
        (**self).preload()
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

/// Postgres-backed flag storage.
///
/// Uses the framework-owned `autumn_feature_flags` and `feature_flag_changes`
/// tables managed by the `create_feature_flags` migration. Reads come from an
/// in-memory snapshot of all flags. A refresh loads the snapshot off the
/// request path. Replicas see a remote write on their next refresh. Each write
/// also sends `NOTIFY autumn_flags`; Autumn does not `LISTEN` for it.
#[cfg(feature = "db")]
pub mod pg {
    use super::{FlagChangeRecord, FlagConfig, FlagStore, FlagStoreError};
    use diesel::prelude::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, PoisonError, RwLock};
    use std::time::{Duration, Instant};

    /// Columns that `FlagRow` reads.
    const FLAG_COLUMNS: &str =
        "key, description, enabled, rollout_pct, actor_allowlist, group_allowlist";

    /// All flags, as the last refresh and local writes left them.
    #[derive(Debug, Default)]
    struct Snapshot {
        flags: HashMap<String, FlagConfig>,
        /// `true` after the first full load.
        loaded: bool,
        /// Start of the last refresh attempt, good or bad.
        checked_at: Option<Instant>,
    }

    /// State that a background refresh shares with the store.
    #[derive(Debug)]
    struct Shared {
        database_url: String,
        cache_ttl: Duration,
        snapshot: RwLock<Snapshot>,
        /// Increments on each local write. A refresh that started before a
        /// write does not replace the snapshot.
        generation: AtomicU64,
        /// `true` while a refresh runs. Only one refresh runs at a time.
        refreshing: AtomicBool,
        refresh_errors: AtomicU64,
        /// `true` after a failed refresh, until a refresh succeeds.
        failing: AtomicBool,
        #[cfg(test)]
        connect_threads: std::sync::Mutex<Vec<std::thread::ThreadId>>,
    }

    /// Holds the single refresh slot. Drop releases it, also when a spawned
    /// refresh never runs.
    struct RefreshSlot(Arc<Shared>);

    impl Drop for RefreshSlot {
        fn drop(&mut self) {
            self.0.refreshing.store(false, Ordering::Release);
        }
    }

    impl Shared {
        fn connect(&self) -> Result<diesel::PgConnection, FlagStoreError> {
            #[cfg(test)]
            self.connect_threads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(std::thread::current().id());
            diesel::PgConnection::establish(&self.database_url)
                .map_err(|e| FlagStoreError::Backend(e.to_string()))
        }

        fn is_stale(&self) -> bool {
            let snapshot = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
            snapshot.checked_at.is_none_or(|checked| {
                checked
                    .checked_add(self.cache_ttl)
                    .is_some_and(|due| crate::time::ambient_instant() >= due)
            })
        }

        fn read(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError> {
            let snapshot = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
            match snapshot.flags.get(key) {
                Some(flag) => Ok(Some(flag.clone())),
                None if snapshot.loaded => Ok(None),
                None => Err(FlagStoreError::Backend(
                    "flag snapshot not loaded yet; call PgFlagStore::refresh at startup".to_owned(),
                )),
            }
        }

        /// Take the refresh slot, or `None` when a refresh already runs.
        fn begin_refresh(self: &Arc<Self>) -> Option<RefreshSlot> {
            self.refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .ok()
                .map(|_| RefreshSlot(Arc::clone(self)))
        }

        /// Load all flags on this thread. This blocks.
        fn reload(&self) -> Result<(), FlagStoreError> {
            let started = self.generation.load(Ordering::Acquire);
            self.snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .checked_at = Some(crate::time::ambient_instant());
            match self.connect().and_then(|mut conn| load_all(&mut conn)) {
                Ok(flags) => {
                    self.install(flags, started);
                    if self.failing.swap(false, Ordering::AcqRel) {
                        tracing::info!("feature flag refresh recovered");
                    }
                    Ok(())
                }
                Err(error) => {
                    self.refresh_errors.fetch_add(1, Ordering::Relaxed);
                    if self.failing.swap(true, Ordering::AcqRel) {
                        tracing::debug!(%error, "feature flag refresh failed again");
                    } else {
                        tracing::warn!(
                            %error,
                            "feature flag refresh failed; serving the last-known snapshot"
                        );
                    }
                    Err(error)
                }
            }
        }

        /// Replace the snapshot with `flags`, unless a local write came after
        /// generation `started`. Return `true` when it replaced the snapshot.
        fn install(&self, flags: Vec<FlagConfig>, started: u64) -> bool {
            let mut snapshot = self
                .snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if self.generation.load(Ordering::Acquire) != started {
                // The loaded data can be older than the write. Refresh again soon.
                snapshot.checked_at = None;
                return false;
            }
            snapshot.flags = flags.into_iter().map(|f| (f.key.clone(), f)).collect();
            snapshot.loaded = true;
            true
        }

        /// Put a flag that this process wrote into the snapshot.
        fn apply_local(&self, flag: FlagConfig) {
            let mut snapshot = self
                .snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            self.generation.fetch_add(1, Ordering::AcqRel);
            snapshot.flags.insert(flag.key.clone(), flag);
        }
    }

    fn load_all(conn: &mut diesel::PgConnection) -> Result<Vec<FlagConfig>, FlagStoreError> {
        diesel::sql_query(format!(
            "SELECT {FLAG_COLUMNS} FROM autumn_feature_flags ORDER BY key"
        ))
        .load::<FlagRow>(conn)
        .map(|rows| rows.into_iter().map(FlagRow::into_config).collect())
        .map_err(|e| FlagStoreError::Backend(e.to_string()))
    }

    /// Postgres-backed [`FlagStore`] that serves reads from memory.
    ///
    /// `get` reads an in-memory snapshot of all flags. It does not connect on
    /// the calling thread when a Tokio runtime is present. When the snapshot is
    /// older than the cache TTL, `get` starts one refresh on the blocking pool
    /// and returns the current value at once. With no runtime, `get` refreshes
    /// on the calling thread.
    ///
    /// When a refresh fails, the store keeps the last-known snapshot, logs a
    /// warning and counts the error ([`refresh_errors`](Self::refresh_errors)).
    /// Before the first load, `get` returns an error.
    ///
    /// Load the snapshot at startup: [`AppBuilder::with_flag_store`] calls
    /// [`FlagStore::preload`], and [`spawn_poll_listener`](Self::spawn_poll_listener)
    /// loads at once and then on each interval.
    ///
    /// [`AppBuilder::with_flag_store`]: crate::app::AppBuilder::with_flag_store
    #[derive(Debug)]
    pub struct PgFlagStore {
        shared: Arc<Shared>,
    }

    impl Clone for PgFlagStore {
        /// Make a new store with the same URL and TTL and an empty snapshot.
        fn clone(&self) -> Self {
            Self::with_cache_ttl(self.shared.database_url.clone(), self.shared.cache_ttl)
        }
    }

    impl PgFlagStore {
        /// Default snapshot lifetime before `get` starts a refresh.
        pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(1);

        /// Create a store with the default 1 s snapshot TTL.
        #[must_use]
        pub fn new(database_url: impl Into<String>) -> Self {
            Self::with_cache_ttl(database_url, Self::DEFAULT_CACHE_TTL)
        }

        /// Create a store with an explicit snapshot TTL. With `Duration::ZERO`,
        /// each `get` starts a refresh.
        #[must_use]
        pub fn with_cache_ttl(database_url: impl Into<String>, cache_ttl: Duration) -> Self {
            Self {
                shared: Arc::new(Shared {
                    database_url: database_url.into(),
                    cache_ttl,
                    snapshot: RwLock::new(Snapshot::default()),
                    generation: AtomicU64::new(0),
                    refreshing: AtomicBool::new(false),
                    refresh_errors: AtomicU64::new(0),
                    failing: AtomicBool::new(false),
                    #[cfg(test)]
                    connect_threads: std::sync::Mutex::new(Vec::new()),
                }),
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

        /// Load all flags into the snapshot now. This blocks: call it at
        /// startup or from `spawn_blocking`.
        ///
        /// # Errors
        ///
        /// Returns [`FlagStoreError::Backend`] when the load fails. The last-known
        /// snapshot stays in use.
        pub fn refresh(&self) -> Result<(), FlagStoreError> {
            self.shared.reload()
        }

        /// Number of failed refreshes since the store was made.
        #[must_use]
        pub fn refresh_errors(&self) -> u64 {
            self.shared.refresh_errors.load(Ordering::Relaxed)
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

        /// Run one flag mutation in a transaction, record it in the change log,
        /// and put the new row into the snapshot.
        ///
        /// `update` must end with `RETURNING {FLAG_COLUMNS}`.
        fn write(
            &self,
            key: &str,
            mutation: &str,
            actor: Option<&str>,
            update: impl FnOnce(&mut diesel::PgConnection) -> QueryResult<FlagRow>,
        ) -> Result<(), FlagStoreError> {
            let mut conn = self.shared.connect()?;
            let row = conn
                .transaction::<FlagRow, diesel::result::Error, _>(|conn| {
                    Self::upsert_flag(conn, key)?;
                    let row = update(conn)?;
                    diesel::sql_query(
                        "INSERT INTO feature_flag_changes (key, mutation, actor) \
                         VALUES ($1, $2, $3)",
                    )
                    .bind::<diesel::sql_types::Text, _>(key)
                    .bind::<diesel::sql_types::Text, _>(mutation)
                    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                        actor.map(str::to_owned),
                    )
                    .execute(conn)?;
                    Self::notify(conn, key)?;
                    Ok(row)
                })
                .map_err(|e| FlagStoreError::Backend(e.to_string()))?;
            self.shared.apply_local(row.into_config());
            Ok(())
        }

        /// Spawn a thread that refreshes the snapshot at once and then on each
        /// `poll_interval`.
        ///
        /// Replicas see remote writes on the next refresh. Call this once at
        /// startup and share the `Arc` with the app:
        ///
        /// ```rust,ignore
        /// let store = Arc::new(PgFlagStore::new(db_url));
        /// PgFlagStore::spawn_poll_listener(Arc::clone(&store), Duration::from_secs(1));
        /// ```
        ///
        /// The thread stops after all clones of the `Arc` drop.
        #[allow(
            clippy::must_use_candidate,
            reason = "the thread runs detached; callers drop the handle"
        )]
        pub fn spawn_poll_listener(
            store: Arc<Self>,
            poll_interval: Duration,
        ) -> std::thread::JoinHandle<()> {
            let shared = Arc::downgrade(&store.shared);
            drop(store);
            std::thread::spawn(move || {
                while let Some(shared) = shared.upgrade() {
                    if let Some(slot) = shared.begin_refresh() {
                        // `reload` logs and counts its errors.
                        let _ = slot.0.reload();
                    }
                    drop(shared);
                    std::thread::sleep(poll_interval);
                }
            })
        }

        #[cfg(test)]
        fn install_for_test(&self, flags: Vec<FlagConfig>) {
            assert!(self.install(flags, self.write_generation()));
            self.shared
                .snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .checked_at = Some(crate::time::ambient_instant());
        }

        #[cfg(test)]
        fn connect_threads_for_test(&self) -> Vec<std::thread::ThreadId> {
            self.shared
                .connect_threads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        #[cfg(test)]
        fn refreshing_for_test(&self) -> bool {
            self.shared.refreshing.load(Ordering::Acquire)
        }

        #[cfg(test)]
        fn write_generation(&self) -> u64 {
            self.shared.generation.load(Ordering::Acquire)
        }

        #[cfg(test)]
        fn apply_local(&self, flag: FlagConfig) {
            self.shared.apply_local(flag);
        }

        #[cfg(test)]
        fn install(&self, flags: Vec<FlagConfig>, generation: u64) -> bool {
            self.shared.install(flags, generation)
        }
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
            if self.shared.is_stale()
                && let Some(slot) = self.shared.begin_refresh()
            {
                match tokio::runtime::Handle::try_current() {
                    // On a runtime: refresh on the blocking pool, never here.
                    Ok(_) => {
                        drop(crate::time::spawn_blocking(move || {
                            // `reload` logs and counts its errors.
                            let _ = slot.0.reload();
                        }));
                    }
                    // No runtime: this thread can block.
                    Err(_) => {
                        let _ = slot.0.reload();
                    }
                }
            }
            self.shared.read(key)
        }

        fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError> {
            let started = self.shared.generation.load(Ordering::Acquire);
            let flags = load_all(&mut self.shared.connect()?)?;
            self.shared.install(flags.clone(), started);
            Ok(flags)
        }

        fn preload(&self) -> Result<(), FlagStoreError> {
            self.refresh()
        }

        fn enable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
            self.write(key, "enabled", actor, |conn| {
                diesel::sql_query(format!(
                    "UPDATE autumn_feature_flags \
                     SET enabled = true, rollout_pct = 100, updated_at = NOW() \
                     WHERE key = $1 RETURNING {FLAG_COLUMNS}"
                ))
                .bind::<diesel::sql_types::Text, _>(key)
                .get_result(conn)
            })
        }

        fn disable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
            self.write(key, "disabled", actor, |conn| {
                diesel::sql_query(format!(
                    "UPDATE autumn_feature_flags SET enabled = false, updated_at = NOW() \
                     WHERE key = $1 RETURNING {FLAG_COLUMNS}"
                ))
                .bind::<diesel::sql_types::Text, _>(key)
                .get_result(conn)
            })
        }

        fn set_rollout(
            &self,
            key: &str,
            pct: u8,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            let pct = i16::from(pct.min(100));
            self.write(key, &format!("rollout={pct}"), actor, |conn| {
                diesel::sql_query(format!(
                    "UPDATE autumn_feature_flags \
                     SET enabled = true, rollout_pct = $2, updated_at = NOW() \
                     WHERE key = $1 RETURNING {FLAG_COLUMNS}"
                ))
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::SmallInt, _>(pct)
                .get_result(conn)
            })
        }

        fn allow_actor(
            &self,
            key: &str,
            actor_id: &str,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            self.write(key, &format!("allowed_actor={actor_id}"), actor, |conn| {
                diesel::sql_query(format!(
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
                     WHERE key = $1 RETURNING {FLAG_COLUMNS}"
                ))
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>(actor_id)
                .get_result(conn)
            })
        }

        fn add_group(
            &self,
            key: &str,
            group: &str,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            self.write(key, &format!("added_group={group}"), actor, |conn| {
                diesel::sql_query(format!(
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
                     WHERE key = $1 RETURNING {FLAG_COLUMNS}"
                ))
                .bind::<diesel::sql_types::Text, _>(key)
                .bind::<diesel::sql_types::Text, _>(group)
                .get_result(conn)
            })
        }

        fn history(
            &self,
            key: &str,
            limit: usize,
        ) -> Result<Vec<FlagChangeRecord>, FlagStoreError> {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut conn = self.shared.connect()?;
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

        /// Nothing listens on port 1, so a connect fails at once.
        const DEAD_URL: &str = "postgres://autumn@127.0.0.1:1/autumn?connect_timeout=2";

        fn enabled(key: &str) -> FlagConfig {
            let mut flag = FlagConfig::new(key);
            flag.enabled = true;
            flag.rollout_pct = 100;
            flag
        }

        fn current_thread_runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        /// Wait (on the runtime) until no background refresh runs.
        async fn settle(store: &PgFlagStore) {
            for _ in 0..1000 {
                if !store.refreshing_for_test() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("background refresh did not finish");
        }

        #[test]
        fn pg_get_serves_snapshot_when_refresh_fails() {
            let store = PgFlagStore::with_cache_ttl(DEAD_URL, Duration::ZERO);
            store.install_for_test(vec![enabled("beta")]);

            // No runtime here, so `get` refreshes on this thread. It fails.
            assert_eq!(store.get("beta").unwrap(), Some(enabled("beta")));
            assert_eq!(store.get("absent").unwrap(), None);
            assert_eq!(store.refresh_errors(), 2);
        }

        #[test]
        fn pg_get_within_ttl_does_not_connect() {
            let store = PgFlagStore::with_cache_ttl(DEAD_URL, Duration::from_secs(60));
            store.install_for_test(vec![enabled("beta")]);
            assert_eq!(store.get("beta").unwrap(), Some(enabled("beta")));
            assert!(store.connect_threads_for_test().is_empty());
        }

        #[test]
        fn pg_get_on_current_thread_runtime_never_connects_on_the_worker() {
            let store = std::sync::Arc::new(PgFlagStore::with_cache_ttl(DEAD_URL, Duration::ZERO));
            store.install_for_test(vec![enabled("beta")]);
            let svc = crate::feature_flags::FeatureFlagService::new(store.clone());

            current_thread_runtime().block_on(async {
                let worker = std::thread::current().id();
                assert!(svc.is_enabled("beta", Some("user:1")));
                settle(&store).await;

                let threads = store.connect_threads_for_test();
                assert!(!threads.is_empty(), "the stale snapshot is refreshed");
                assert!(
                    !threads.contains(&worker),
                    "flag evaluation must not connect on the Tokio worker"
                );
            });
            assert!(store.refresh_errors() >= 1);
        }

        #[test]
        fn pg_cold_get_on_runtime_errors_and_loads_in_background() {
            let store = PgFlagStore::with_cache_ttl(DEAD_URL, Duration::ZERO);
            current_thread_runtime().block_on(async {
                let worker = std::thread::current().id();
                assert!(store.get("beta").is_err(), "no snapshot yet");
                settle(&store).await;
                let threads = store.connect_threads_for_test();
                assert!(!threads.is_empty());
                assert!(!threads.contains(&worker));
            });
        }

        #[test]
        fn pg_refresh_older_than_a_local_write_is_discarded() {
            let store = PgFlagStore::with_cache_ttl(DEAD_URL, Duration::from_secs(60));
            store.install_for_test(vec![]);

            let started = store.write_generation();
            store.apply_local(enabled("beta"));
            // A refresh that read the database before the write ends now.
            assert!(!store.install(vec![FlagConfig::new("beta")], started));
            assert_eq!(store.get("beta").unwrap(), Some(enabled("beta")));

            assert!(store.install(vec![FlagConfig::new("beta")], store.write_generation()));
            assert_eq!(store.get("beta").unwrap(), Some(FlagConfig::new("beta")));
        }

        #[test]
        fn pg_refresh_reports_backend_error() {
            let store = PgFlagStore::new(DEAD_URL);
            assert!(store.refresh().is_err());
            assert_eq!(store.refresh_errors(), 1);
            assert!(store.preload().is_err());
        }

        #[test]
        fn pg_poll_listener_stops_when_the_store_drops() {
            let store = std::sync::Arc::new(PgFlagStore::new(DEAD_URL));
            let handle = PgFlagStore::spawn_poll_listener(
                std::sync::Arc::clone(&store),
                Duration::from_millis(10),
            );
            drop(store);
            for _ in 0..1000 {
                if handle.is_finished() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("poll listener outlived its store");
        }

        #[test]
        fn pg_store_exposes_database_url() {
            let store = PgFlagStore::new("postgres://localhost/myapp");
            assert_eq!(store.shared.database_url, "postgres://localhost/myapp");
        }

        #[test]
        fn pg_store_default_cache_ttl_is_one_second() {
            let store = PgFlagStore::new("postgres://localhost/myapp");
            assert_eq!(store.shared.cache_ttl, PgFlagStore::DEFAULT_CACHE_TTL);
        }

        #[test]
        fn pg_store_with_cache_ttl_sets_custom_ttl() {
            let ttl = Duration::from_secs(30);
            let store = PgFlagStore::with_cache_ttl("postgres://localhost/myapp", ttl);
            assert_eq!(store.shared.cache_ttl, ttl);
        }

        #[test]
        fn pg_store_clone_has_independent_snapshot() {
            // Only `Arc<PgFlagStore>` shares one snapshot.
            let store = PgFlagStore::with_cache_ttl(DEAD_URL, Duration::from_secs(60));
            store.install_for_test(vec![enabled("cached")]);
            let cloned = store.clone();
            assert!(cloned.get("cached").is_err(), "a clone starts empty");
            assert_eq!(store.get("cached").unwrap(), Some(enabled("cached")));
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
///
/// # Store failures
///
/// When the store returns an error, the service uses the last value that it
/// read for that flag (the last-known value). With no last-known value, it
/// uses the declared default ([`with_default`](Self::with_default)), or
/// `false`. Clones share the last-known values and the error count.
#[derive(Clone)]
pub struct FeatureFlagService {
    store: Arc<dyn FlagStore>,
    group_resolver: Option<GroupResolver>,
    defaults: Arc<HashMap<String, bool>>,
    health: Arc<StoreHealth>,
}

/// Last-known flag values and store error state, shared by service clones.
#[derive(Default)]
struct StoreHealth {
    last_known: RwLock<HashMap<String, FlagConfig>>,
    errors: AtomicU64,
    /// `true` after a failed read, until a read succeeds.
    failing: AtomicBool,
}

impl StoreHealth {
    /// Keep the value that the store returned for `key`.
    fn remember(&self, key: &str, flag: Option<&FlagConfig>) {
        if self.failing.load(Ordering::Relaxed) && self.failing.swap(false, Ordering::AcqRel) {
            tracing::info!("feature flag store recovered");
        }
        let known = self
            .last_known
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if known.get(key) == flag {
            return;
        }
        drop(known);
        let mut known = self
            .last_known
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        match flag {
            Some(flag) => {
                known.insert(key.to_owned(), flag.clone());
            }
            None => {
                known.remove(key);
            }
        }
    }

    /// Count a failed read and return the last-known value for `key`.
    fn recall(&self, key: &str, error: &FlagStoreError) -> Option<FlagConfig> {
        self.errors.fetch_add(1, Ordering::Relaxed);
        if self.failing.swap(true, Ordering::AcqRel) {
            tracing::debug!(flag = key, %error, "feature flag store read failed again");
        } else {
            tracing::warn!(
                flag = key,
                %error,
                "feature flag store read failed; serving last-known values or declared defaults"
            );
        }
        self.last_known
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }
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
            defaults: Arc::default(),
            health: Arc::default(),
        }
    }

    /// Declare the value of `flag_key` when the store does not hold the flag,
    /// or cannot read it and no last-known value exists.
    ///
    /// Without a declared default, that value is `false`.
    #[must_use]
    pub fn with_default(mut self, flag_key: impl Into<String>, default: bool) -> Self {
        Arc::make_mut(&mut self.defaults).insert(flag_key.into(), default);
        self
    }

    /// Number of failed store reads since the service was made.
    #[must_use]
    pub fn store_errors(&self) -> u64 {
        self.health.errors.load(Ordering::Relaxed)
    }

    /// Load the store before the first read. This can block.
    ///
    /// # Errors
    ///
    /// Propagates [`FlagStoreError`] from the backing store.
    pub fn preload(&self) -> Result<(), FlagStoreError> {
        self.store.preload()
    }

    /// Attach a group resolver so named-group gates are evaluated.
    #[must_use]
    pub fn with_group_resolver(mut self, resolver: GroupResolver) -> Self {
        self.group_resolver = Some(resolver);
        self
    }

    /// Return `true` if `flag_key` is enabled for `actor_id`.
    ///
    /// An unknown flag gets its declared default, or `false`. A store error
    /// gets the last-known value (see [Store failures](Self#store-failures)).
    #[must_use]
    pub fn is_enabled(&self, flag_key: &str, actor_id: Option<&str>) -> bool {
        let flag = match self.store.get(flag_key) {
            Ok(flag) => {
                self.health.remember(flag_key, flag.as_ref());
                flag
            }
            Err(error) => self.health.recall(flag_key, &error),
        };
        flag.map_or_else(
            || self.defaults.get(flag_key).copied().unwrap_or(false),
            |flag| self.evaluate(&flag, actor_id),
        )
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

/// Start a preload of the store of `service` on the blocking pool.
///
/// The hook does not wait for the load, so a slow database does not delay
/// later startup hooks. A failure logs a warning. Until the store loads, reads
/// use declared defaults.
///
/// Call it on a Tokio runtime.
pub(crate) fn preload_at_startup(
    service: FeatureFlagService,
) -> std::future::Ready<crate::AutumnResult<()>> {
    drop(crate::time::spawn_blocking(move || {
        if let Err(error) = service.preload() {
            tracing::warn!(%error, "feature flag preload failed; flags use declared defaults");
        }
    }));
    std::future::ready(Ok(()))
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

    // ── Store failure: last-known value and declared default (#3063) ──────

    /// Wraps an in-memory store. The test can make `get` fail or hide a flag.
    #[derive(Default)]
    struct ScriptedStore {
        inner: InMemoryFlagStore,
        failing: std::sync::atomic::AtomicBool,
        hidden: std::sync::atomic::AtomicBool,
        preloads: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedStore {
        fn fail(&self, on: bool) {
            self.failing.store(on, std::sync::atomic::Ordering::SeqCst);
        }

        fn hide(&self, on: bool) {
            self.hidden.store(on, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl FlagStore for ScriptedStore {
        fn get(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError> {
            if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(FlagStoreError::Backend("brownout".to_owned()));
            }
            if self.hidden.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(None);
            }
            self.inner.get(key)
        }
        fn list(&self) -> Result<Vec<FlagConfig>, FlagStoreError> {
            self.inner.list()
        }
        fn enable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
            self.inner.enable(key, actor)
        }
        fn disable(&self, key: &str, actor: Option<&str>) -> Result<(), FlagStoreError> {
            self.inner.disable(key, actor)
        }
        fn set_rollout(
            &self,
            key: &str,
            pct: u8,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            self.inner.set_rollout(key, pct, actor)
        }
        fn allow_actor(
            &self,
            key: &str,
            actor_id: &str,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            self.inner.allow_actor(key, actor_id, actor)
        }
        fn add_group(
            &self,
            key: &str,
            group: &str,
            actor: Option<&str>,
        ) -> Result<(), FlagStoreError> {
            self.inner.add_group(key, group, actor)
        }
        fn history(
            &self,
            key: &str,
            limit: usize,
        ) -> Result<Vec<FlagChangeRecord>, FlagStoreError> {
            self.inner.history(key, limit)
        }
        fn preload(&self) -> Result<(), FlagStoreError> {
            self.preloads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn failing_store_serves_last_known_value() {
        let store = Arc::new(ScriptedStore::default());
        let svc = FeatureFlagService::new(store.clone());
        store.enable("beta", None).unwrap();
        assert!(svc.is_enabled("beta", Some("user:1")));

        store.fail(true);
        assert!(
            svc.is_enabled("beta", Some("user:1")),
            "a store error must not turn a known flag off"
        );
        assert_eq!(svc.store_errors(), 1);
    }

    #[test]
    fn failing_store_keeps_a_kill_switch_off() {
        let store = Arc::new(ScriptedStore::default());
        let svc = FeatureFlagService::new(store.clone()).with_default("payments", true);
        store.enable("payments", None).unwrap();
        store.disable("payments", None).unwrap();
        assert!(!svc.is_enabled("payments", None));

        store.fail(true);
        assert!(
            !svc.is_enabled("payments", None),
            "the last-known value wins over the declared default"
        );
    }

    #[test]
    fn failing_store_without_last_known_value_serves_declared_default() {
        let store = Arc::new(ScriptedStore::default());
        let svc = FeatureFlagService::new(store.clone()).with_default("fail_open", true);
        store.fail(true);
        assert!(svc.is_enabled("fail_open", None));
        assert!(!svc.is_enabled("no_default", None));
        assert_eq!(svc.store_errors(), 2);
    }

    #[test]
    fn unknown_flag_serves_declared_default() {
        let svc = make_svc().with_default("absent", true);
        assert!(svc.is_enabled("absent", None));
        assert!(!svc.is_enabled("other", None));
        assert_eq!(svc.store_errors(), 0);
    }

    #[test]
    fn flag_removed_from_store_forgets_last_known_value() {
        let store = Arc::new(ScriptedStore::default());
        let svc = FeatureFlagService::new(store.clone());
        store.enable("gone", None).unwrap();
        assert!(svc.is_enabled("gone", None));

        store.hide(true);
        assert!(!svc.is_enabled("gone", None));
        store.hide(false);
        store.fail(true);
        assert!(
            !svc.is_enabled("gone", None),
            "a flag the store reported absent has no last-known value"
        );
    }

    #[test]
    fn service_clones_share_last_known_values() {
        // The `Flags` extractor clones the service for each request.
        let store = Arc::new(ScriptedStore::default());
        let svc = FeatureFlagService::new(store.clone());
        store.enable("shared", None).unwrap();
        assert!(svc.is_enabled("shared", None));

        let per_request = svc.clone();
        store.fail(true);
        assert!(per_request.is_enabled("shared", None));
        assert_eq!(svc.store_errors(), 1, "clones share the error count");
    }

    #[test]
    fn service_preload_delegates_to_the_store() {
        let store = Arc::new(ScriptedStore::default());
        let svc = FeatureFlagService::new(store.clone());
        svc.preload().unwrap();
        assert_eq!(store.preloads.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The default `preload` does nothing and succeeds.
        make_svc().preload().unwrap();
    }

    #[test]
    fn feature_flag_guide_describes_polling_not_listen() {
        let guide = include_str!("../../docs/guide/feature-flags.md");
        assert!(
            !guide.contains("All replicas listening on that channel"),
            "Autumn does not LISTEN; replicas poll"
        );
        assert!(guide.contains("spawn_poll_listener"));
        assert!(guide.contains("last-known"));
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
