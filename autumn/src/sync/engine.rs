//! Client sync loop: push pending changes, pull newer rows.

use std::time::Duration;

use super::SyncError;
use super::protocol::Version;
use super::store::SyncStore;

/// Configuration for a [`SyncEngine`].
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Base URL where the server sync router is mounted, e.g.
    /// `https://example.com/sync` (no trailing slash).
    pub remote_base_url: String,
    /// Maximum changes per push request.
    pub push_batch_size: usize,
    /// Maximum rows per pull page.
    pub pull_batch_size: i64,
    /// Per-request HTTP timeout.
    pub request_timeout: Duration,
    /// Initial backoff after a failed background sync.
    pub min_backoff: Duration,
    /// Backoff ceiling for the background loop.
    pub max_backoff: Duration,
}

impl SyncConfig {
    /// Configuration with sensible defaults for `remote_base_url`.
    #[must_use]
    pub fn new(remote_base_url: impl Into<String>) -> Self {
        Self {
            remote_base_url: remote_base_url.into(),
            push_batch_size: 100,
            pull_batch_size: 500,
            request_timeout: Duration::from_secs(30),
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
        }
    }
}

/// What one [`SyncEngine::sync_once`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Changes pushed and acknowledged by the server.
    pub pushed: usize,
    /// Remote rows pulled and applied locally.
    pub pulled: usize,
    /// Whether the server demanded (and the engine performed) a full resync.
    pub full_resync: bool,
}

/// A point-in-time snapshot of the engine's local sync state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncStatus {
    /// This device's stable id.
    pub device_id: String,
    /// Last server version pulled through.
    pub cursor: Version,
    /// Journaled changes not yet acknowledged by the server.
    pub pending_changes: u64,
}

/// The client-side sync engine.
///
/// Owns an HTTP client and a [`SyncStore`]; [`Self::sync_once`] performs one
/// push→pull pass, and [`Self::spawn_background`] keeps retrying with
/// exponential backoff so the app "syncs in the background when connection
/// is restored".
#[derive(Clone)]
pub struct SyncEngine {
    store: SyncStore,
    config: SyncConfig,
}

impl SyncEngine {
    /// Build an engine over `store` targeting `config.remote_base_url`.
    #[must_use]
    pub fn new(store: SyncStore, config: SyncConfig) -> Self {
        Self { store, config }
    }

    /// The underlying local store.
    #[must_use]
    pub const fn store(&self) -> &SyncStore {
        &self.store
    }

    /// Run one full sync pass: push all pending changes in batches, then
    /// pull and apply every row newer than the local cursor. Handles
    /// `FullResyncRequired` transparently (pending changes are preserved
    /// and replayed).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Transport`] when the server is unreachable —
    /// local state and the pending journal are untouched, the next call
    /// retries — or [`SyncError::Server`]/[`SyncError::Store`] on server or
    /// local failures.
    pub async fn sync_once(&self) -> Result<SyncReport, SyncError> {
        let _ = &self.config;
        Err(SyncError::Transport("unimplemented".into()))
    }

    /// Snapshot the local sync status (cursor, pending count, device id).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn status(&self) -> Result<SyncStatus, SyncError> {
        Err(SyncError::Store("unimplemented".into()))
    }

    /// Spawn a background tokio task that syncs every `interval`, backing
    /// off exponentially (between the configured min/max) while the server
    /// is unreachable.
    pub fn spawn_background(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let _ = interval;
        let engine = self.clone();
        tokio::spawn(async move {
            let _ = engine;
        })
    }
}
