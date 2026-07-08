//! Client sync loop: push pending changes, pull newer rows.

use std::sync::Arc;
use std::time::Duration;

use super::SyncError;
use super::protocol::{
    MAX_PULL_LIMIT, MAX_PUSH_CHANGES, PullResponse, PushRequest, PushResponse, Version,
};
use super::store::SyncStore;

/// Configuration for a [`SyncEngine`].
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Base URL where the server sync router is mounted, e.g.
    /// `https://example.com/sync` (no trailing slash).
    pub remote_base_url: String,
    /// Maximum changes per push request. Clamped at send time into
    /// `1..=MAX_PUSH_CHANGES` (the server rejects larger batches).
    pub push_batch_size: usize,
    /// Maximum rows per pull page. Clamped at request time into
    /// `1..=MAX_PULL_LIMIT` (the server clamps to the same bound, and the
    /// engine's "short page means caught up" termination requires both
    /// sides to agree); `0` is treated as `1`, never as an infinite loop.
    pub pull_batch_size: i64,
    /// Per-request HTTP timeout.
    pub request_timeout: Duration,
    /// Initial backoff after a failed background sync.
    pub min_backoff: Duration,
    /// Backoff ceiling for the background loop.
    pub max_backoff: Duration,
    /// Optional bearer token sent as `Authorization: Bearer <token>` on
    /// every push/pull request. Pair it with authentication middleware on
    /// the server's `/sync` mount — the endpoints must never ship open
    /// (see the offline-sync guide). For anything richer than a bearer
    /// token, front the deployment with an authenticating proxy.
    pub bearer_token: Option<String>,
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
            bearer_token: None,
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
/// Owns an HTTP client and a [`SyncStore`]; [`Self::sync_once`] performs
/// one push→pull pass, and [`Self::spawn_background`] keeps retrying with
/// exponential backoff so the app "syncs in the background when connection
/// is restored". Sync is at-least-once: every journaled change carries a
/// client-generated `change_id` the server dedups on, so retrying after a
/// lost response never double-applies.
#[derive(Debug, Clone)]
pub struct SyncEngine {
    store: SyncStore,
    config: SyncConfig,
    client: reqwest::Client,
    /// Serializes [`Self::sync_once`] passes across all clones of this
    /// engine, so an ad-hoc kick (e.g. the Tauri shell's resume trigger)
    /// overlapping the background loop queues instead of double-pushing
    /// the same journal batch.
    sync_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SyncEngine {
    /// Build an engine over `store` targeting `config.remote_base_url`.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client's TLS backend cannot be initialized
    /// (`reqwest::Client` construction).
    #[must_use]
    pub fn new(store: SyncStore, config: SyncConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .expect("failed to construct the sync HTTP client");
        Self {
            store,
            config,
            client,
            sync_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The underlying local store.
    #[must_use]
    pub const fn store(&self) -> &SyncStore {
        &self.store
    }

    /// Run one full sync pass: push all pending changes in batches, then
    /// pull and apply every row newer than the local cursor. Handles
    /// `FullResyncRequired` transparently (synced rows are cleared and
    /// re-pulled from zero; pending changes are preserved and replayed).
    ///
    /// Passes are serialized: overlapping calls (from any clone of this
    /// engine — e.g. a foreground "sync now" kick racing the background
    /// loop) wait for the in-flight pass to finish instead of pushing the
    /// same journal batch twice.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Transport`] when the server is unreachable —
    /// local state and the pending journal are untouched, the next call
    /// retries — or [`SyncError::Server`]/[`SyncError::Store`] on server or
    /// local failures.
    pub async fn sync_once(&self) -> Result<SyncReport, SyncError> {
        let _pass = self.sync_lock.lock().await;
        let mut report = SyncReport::default();
        self.push_pending(&mut report).await?;
        if self.pull_updates(&mut report).await? {
            // Stale cursor: reset synced state (pending survives), re-pull
            // everything, then replay whatever is still journaled. The
            // re-pull runs with a session-start cursor of 0, which the
            // server never asks to resync, so multi-page re-pulls cannot
            // trip the horizon check mid-pagination.
            self.store.begin_full_resync()?;
            report.full_resync = true;
            if self.pull_updates(&mut report).await? {
                return Err(SyncError::Server(
                    "server demanded a full resync from cursor 0".into(),
                ));
            }
            self.push_pending(&mut report).await?;
        }
        Ok(report)
    }

    /// Push journaled changes in batches until the journal is drained.
    async fn push_pending(&self, report: &mut SyncReport) -> Result<(), SyncError> {
        let device_id = self.store.device_id()?;
        let batch_size = self.config.push_batch_size.clamp(1, MAX_PUSH_CHANGES);
        loop {
            let changes = self.store.pending_changes(batch_size)?;
            if changes.is_empty() {
                return Ok(());
            }
            let batch_len = changes.len();
            let request = PushRequest {
                device_id: device_id.clone(),
                changes,
            };
            let response = self
                .authorized(
                    self.client
                        .post(format!("{}/push", self.config.remote_base_url)),
                )
                .json(&request)
                .send()
                .await
                .map_err(transport_err)?;
            let response = check_status(response).await?;
            let push_response: PushResponse = response
                .json()
                .await
                .map_err(|err| SyncError::Server(format!("invalid push response: {err}")))?;
            if push_response.outcomes.len() != batch_len {
                return Err(SyncError::Server(format!(
                    "push returned {} outcomes for {batch_len} changes",
                    push_response.outcomes.len()
                )));
            }
            self.store
                .confirm_pushed(&request.changes, &push_response.outcomes)?;
            report.pushed += batch_len;
        }
    }

    /// Pull and apply pages until caught up. Returns `true` when the
    /// server demands a full resync instead.
    ///
    /// Every page of one call carries the same `session` marker — the
    /// cursor the catch-up started from — so the server's tombstone-horizon
    /// check applies to the *session*, not to intermediate page cursors
    /// (a fresh device paging its first sync would otherwise be told to
    /// resync mid-pagination, forever). When the final page arrives, the
    /// cursor is advanced to at least the server's `tombstone_horizon`:
    /// after a completed catch-up the client has, by construction, seen
    /// everything the GC kept, so parking the cursor below the horizon
    /// would only re-trigger a full resync on every subsequent pass.
    /// Local tombstones at or below the horizon are pruned at the same
    /// point (the server physically dropped them; no future pull can
    /// reference those versions again).
    async fn pull_updates(&self, report: &mut SyncReport) -> Result<bool, SyncError> {
        let session_start = self.store.cursor()?;
        let limit = self.config.pull_batch_size.clamp(1, MAX_PULL_LIMIT);
        loop {
            let cursor = self.store.cursor()?;
            let response = self
                .authorized(self.client.get(format!(
                    "{}/pull?cursor={cursor}&limit={limit}&session={session_start}",
                    self.config.remote_base_url
                )))
                .send()
                .await
                .map_err(transport_err)?;
            let response = check_status(response).await?;
            let pull: PullResponse = response
                .json()
                .await
                .map_err(|err| SyncError::Server(format!("invalid pull response: {err}")))?;
            match pull {
                PullResponse::FullResyncRequired { .. } => return Ok(true),
                PullResponse::Ok {
                    rows,
                    next_cursor,
                    tombstone_horizon,
                } => {
                    let page_len = rows.len();
                    report.pulled += self.store.apply_remote_rows(&rows)?;
                    let caught_up =
                        rows.is_empty() || i64::try_from(page_len).unwrap_or(i64::MAX) < limit;
                    if caught_up {
                        // End of pagination: land at/above the horizon so a
                        // horizon left above the newest surviving row (GC
                        // right after a delete) cannot strand the cursor in
                        // a perpetual full-resync loop.
                        self.store.set_cursor(next_cursor.max(tombstone_horizon))?;
                        if tombstone_horizon > 0 {
                            self.store.prune_acked_tombstones(tombstone_horizon)?;
                        }
                        return Ok(false);
                    }
                    self.store.set_cursor(next_cursor)?;
                }
            }
        }
    }

    /// Attach the configured bearer token (if any) to an outgoing request.
    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.config.bearer_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Snapshot the local sync status (cursor, pending count, device id).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn status(&self) -> Result<SyncStatus, SyncError> {
        Ok(SyncStatus {
            device_id: self.store.device_id()?,
            cursor: self.store.cursor()?,
            pending_changes: self.store.pending_count()?,
        })
    }

    /// Spawn a background tokio task that syncs every `interval`, backing
    /// off exponentially (between the configured min/max backoff) while
    /// the server is unreachable — the app keeps working offline and
    /// converges automatically when connectivity returns.
    #[must_use]
    pub fn spawn_background(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let engine = self.clone();
        tokio::spawn(async move {
            let mut backoff = engine.config.min_backoff;
            loop {
                match engine.sync_once().await {
                    Ok(report) => {
                        if report.pushed > 0 || report.pulled > 0 || report.full_resync {
                            tracing::debug!(
                                pushed = report.pushed,
                                pulled = report.pulled,
                                full_resync = report.full_resync,
                                "background sync pass completed"
                            );
                        }
                        backoff = engine.config.min_backoff;
                        tokio::time::sleep(interval).await;
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, backoff = ?backoff, "background sync failed; backing off");
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2).min(engine.config.max_backoff);
                    }
                }
            }
        })
    }
}

fn transport_err(err: reqwest::Error) -> SyncError {
    // `without_url` keeps credentials that may be embedded in the remote
    // URL (userinfo) out of logs and error strings.
    SyncError::Transport(err.without_url().to_string())
}

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, SyncError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(SyncError::Server(format!(
        "sync endpoint returned {status}: {body}"
    )))
}
