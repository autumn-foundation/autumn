//! Client sync loop: push pending changes, pull newer rows.

use std::time::Duration;

use super::SyncError;
use super::protocol::{PullResponse, PushRequest, PushResponse, Version};
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
    /// # Errors
    ///
    /// Returns [`SyncError::Transport`] when the server is unreachable —
    /// local state and the pending journal are untouched, the next call
    /// retries — or [`SyncError::Server`]/[`SyncError::Store`] on server or
    /// local failures.
    pub async fn sync_once(&self) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::default();
        self.push_pending(&mut report).await?;
        if self.pull_updates(&mut report).await? {
            // Stale cursor: reset synced state (pending survives), re-pull
            // everything, then replay whatever is still journaled.
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
        loop {
            let changes = self.store.pending_changes(self.config.push_batch_size)?;
            if changes.is_empty() {
                return Ok(());
            }
            let batch_len = changes.len();
            let request = PushRequest {
                device_id: device_id.clone(),
                changes,
            };
            let response = self
                .client
                .post(format!("{}/push", self.config.remote_base_url))
                .json(&request)
                .send()
                .await
                .map_err(|err| transport_err(&err))?;
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
    async fn pull_updates(&self, report: &mut SyncReport) -> Result<bool, SyncError> {
        loop {
            let cursor = self.store.cursor()?;
            let response = self
                .client
                .get(format!(
                    "{}/pull?cursor={cursor}&limit={}",
                    self.config.remote_base_url, self.config.pull_batch_size
                ))
                .send()
                .await
                .map_err(|err| transport_err(&err))?;
            let response = check_status(response).await?;
            let pull: PullResponse = response
                .json()
                .await
                .map_err(|err| SyncError::Server(format!("invalid pull response: {err}")))?;
            match pull {
                PullResponse::FullResyncRequired { .. } => return Ok(true),
                PullResponse::Ok {
                    rows, next_cursor, ..
                } => {
                    let page_len = rows.len();
                    report.pulled += self.store.apply_remote_rows(&rows)?;
                    self.store.set_cursor(next_cursor)?;
                    if i64::try_from(page_len).unwrap_or(i64::MAX) < self.config.pull_batch_size {
                        return Ok(false);
                    }
                }
            }
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

fn transport_err(err: &reqwest::Error) -> SyncError {
    SyncError::Transport(err.to_string())
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
