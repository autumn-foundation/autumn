//! Tracked job handles: unguessable-token status polling for `#[job]`.
//!
//! [`enqueue_tracked`](crate::job::enqueue_tracked) hands the caller a
//! [`TrackedJobHandle`] carrying a public, unguessable token distinct from the
//! internal job id. Inside the job handler, [`JobContext::current`] exposes
//! progress reporting (`set_progress`) and lets the handler record a terminal
//! result or a user-safe error. A [`JobTrackingStore`] persists that state,
//! keyed by a hash of the token, with a configurable TTL.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AutumnResult;
use crate::time::{ClockSource, SystemClock};

/// Who is allowed to poll a tracked job's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackedJobOwner {
    /// No owner bound — the token itself is the capability.
    Anonymous,
    /// Bound to a specific (unauthenticated) session id.
    Session(String),
    /// Bound to an authenticated user/principal id.
    User(String),
}

/// Lifecycle status of a tracked job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackedJobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl TrackedJobStatus {
    /// Terminal statuses stop htmx polling and are subject to TTL expiry.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// A snapshot of a tracked job's progress and (if terminal) its result.
#[derive(Debug, Clone)]
pub struct TrackedJobRecord {
    pub status: TrackedJobStatus,
    pub progress_pct: Option<u8>,
    pub progress_message: Option<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub owner: TrackedJobOwner,
    pub updated_at: DateTime<Utc>,
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Persists tracked-job progress/result, keyed by a hash of the public token.
///
/// Dyn-safe (boxed-future methods) so it can be installed as an
/// `Arc<dyn JobTrackingStore>` `AppState` extension, mirroring
/// [`crate::auth::ApiTokenStore`] and [`crate::job::JobAdminBackend`].
pub trait JobTrackingStore: Send + Sync + 'static {
    /// Create a new pending record for `key` (the token hash).
    fn create<'a>(&'a self, key: &'a str, owner: TrackedJobOwner) -> BoxFut<'a, AutumnResult<()>>;

    /// Transition a pending record to running.
    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>>;

    /// Record progress. `pct` is clamped to `0..=100`.
    fn set_progress<'a>(
        &'a self,
        key: &'a str,
        pct: u8,
        message: Option<String>,
    ) -> BoxFut<'a, AutumnResult<()>>;

    /// Mark the record succeeded with a small JSON result payload.
    fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>>;

    /// Mark the record failed with a user-safe error message.
    fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>>;

    /// Fetch the current record, or `None` if unknown or expired.
    fn get<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>>;
}

/// `AppState` extension carrying the installed [`JobTrackingStore`].
#[derive(Clone)]
pub struct JobTrackingStoreEntry(pub Arc<dyn JobTrackingStore>);

// ── In-memory store ───────────────────────────────────────────────────────────

struct MemoryEntry {
    record: TrackedJobRecord,
    expires_at: DateTime<Utc>,
}

/// In-memory [`JobTrackingStore`] for development, testing, and the `local`
/// job backend. State is lost on restart and not shared across processes.
#[derive(Clone)]
pub struct InMemoryJobTrackingStore {
    entries: Arc<RwLock<HashMap<String, MemoryEntry>>>,
    ttl: chrono::TimeDelta,
    clock: Arc<dyn ClockSource>,
}

impl InMemoryJobTrackingStore {
    /// Construct a store whose records expire `ttl_secs` after their last
    /// write.
    #[must_use]
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            ttl: chrono::TimeDelta::seconds(i64::try_from(ttl_secs).unwrap_or(i64::MAX)),
            clock: Arc::new(SystemClock),
        }
    }

    /// Replace the clock used to evaluate expiry.
    ///
    /// Defaults to [`SystemClock`]; tests pass a
    /// [`crate::time::FixedClock`] / [`crate::time::TickingClock`] to make
    /// expiry deterministic.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        self.clock = clock;
        self
    }

    fn is_live(&self, entry: &MemoryEntry) -> bool {
        entry.expires_at > self.clock.now()
    }

    fn with_record_mut<F>(&self, key: &str, f: F) -> AutumnResult<()>
    where
        F: FnOnce(&mut TrackedJobRecord),
    {
        let mut guard = self
            .entries
            .write()
            .expect("job tracking store lock poisoned");
        let now = self.clock.now();
        if let Some(entry) = guard.get_mut(key)
            && self.is_live(entry)
        {
            f(&mut entry.record);
            entry.record.updated_at = now;
            entry.expires_at = now + self.ttl;
        }
        Ok(())
    }
}

impl JobTrackingStore for InMemoryJobTrackingStore {
    fn create<'a>(&'a self, key: &'a str, owner: TrackedJobOwner) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let now = self.clock.now();
            let record = TrackedJobRecord {
                status: TrackedJobStatus::Pending,
                progress_pct: None,
                progress_message: None,
                result: None,
                error: None,
                owner,
                updated_at: now,
            };
            self.entries
                .write()
                .expect("job tracking store lock poisoned")
                .insert(
                    key.to_owned(),
                    MemoryEntry {
                        record,
                        expires_at: now + self.ttl,
                    },
                );
            Ok(())
        })
    }

    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.with_record_mut(key, |record| {
                if !record.status.is_terminal() {
                    record.status = TrackedJobStatus::Running;
                }
            })
        })
    }

    fn set_progress<'a>(
        &'a self,
        key: &'a str,
        pct: u8,
        message: Option<String>,
    ) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            let pct = pct.min(100);
            self.with_record_mut(key, |record| {
                if !record.status.is_terminal() {
                    record.progress_pct = Some(pct);
                    record.progress_message = message;
                }
            })
        })
    }

    fn complete<'a>(&'a self, key: &'a str, result: Value) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.with_record_mut(key, |record| {
                record.status = TrackedJobStatus::Succeeded;
                record.result = Some(result);
                record.error = None;
            })
        })
    }

    fn fail<'a>(&'a self, key: &'a str, error: String) -> BoxFut<'a, AutumnResult<()>> {
        Box::pin(async move {
            self.with_record_mut(key, |record| {
                record.status = TrackedJobStatus::Failed;
                record.error = Some(error);
                record.result = None;
            })
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFut<'a, AutumnResult<Option<TrackedJobRecord>>> {
        Box::pin(async move {
            let guard = self
                .entries
                .read()
                .expect("job tracking store lock poisoned");
            Ok(guard.get(key).filter(|entry| self.is_live(entry)).map(
                |entry| TrackedJobRecord {
                    status: entry.record.status,
                    progress_pct: entry.record.progress_pct,
                    progress_message: entry.record.progress_message.clone(),
                    result: entry.record.result.clone(),
                    error: entry.record.error.clone(),
                    owner: entry.record.owner.clone(),
                    updated_at: entry.record.updated_at,
                },
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::FixedClock;

    fn store() -> InMemoryJobTrackingStore {
        InMemoryJobTrackingStore::new(86_400)
    }

    #[tokio::test]
    async fn create_then_get_roundtrips_pending_with_owner() {
        let store = store();
        store
            .create("k1", TrackedJobOwner::User("user:42".to_owned()))
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Pending);
        assert_eq!(record.owner, TrackedJobOwner::User("user:42".to_owned()));
        assert!(record.progress_pct.is_none());
        assert!(record.result.is_none());
    }

    #[tokio::test]
    async fn set_progress_clamps_above_100_and_persists_message() {
        let store = store();
        store.create("k1", TrackedJobOwner::Anonymous).await.unwrap();
        store.mark_running("k1").await.unwrap();

        store
            .set_progress("k1", 250, Some("Rows 1200/5000".to_owned()))
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Running);
        assert_eq!(record.progress_pct, Some(100));
        assert_eq!(record.progress_message.as_deref(), Some("Rows 1200/5000"));
    }

    #[tokio::test]
    async fn complete_is_terminal_and_stores_result_json() {
        let store = store();
        store.create("k1", TrackedJobOwner::Anonymous).await.unwrap();
        store.mark_running("k1").await.unwrap();
        store.set_progress("k1", 50, None).await.unwrap();

        store
            .complete("k1", serde_json::json!({"download_url": "/blob/abc.csv"}))
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        assert_eq!(
            record.result,
            Some(serde_json::json!({"download_url": "/blob/abc.csv"}))
        );
        assert!(record.error.is_none());

        // A terminal record ignores further progress writes.
        store.set_progress("k1", 10, None).await.unwrap();
        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Succeeded);
        assert_eq!(record.progress_pct, Some(50));
    }

    #[tokio::test]
    async fn fail_stores_user_safe_error() {
        let store = store();
        store.create("k1", TrackedJobOwner::Anonymous).await.unwrap();

        store
            .fail("k1", "The export could not be completed.".to_owned())
            .await
            .unwrap();

        let record = store.get("k1").await.unwrap().expect("record");
        assert_eq!(record.status, TrackedJobStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("The export could not be completed.")
        );
        assert!(record.result.is_none());
    }

    #[tokio::test]
    async fn get_unknown_key_returns_none() {
        let store = store();
        assert!(store.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn record_expires_after_ttl() {
        let start = chrono::Utc::now();
        let store = InMemoryJobTrackingStore::new(10)
            .with_clock(std::sync::Arc::new(FixedClock::at(start)));
        store.create("k1", TrackedJobOwner::Anonymous).await.unwrap();

        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(11),
        )));
        assert!(store.get("k1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn terminal_write_refreshes_expiry() {
        let start = chrono::Utc::now();
        let store = InMemoryJobTrackingStore::new(10)
            .with_clock(std::sync::Arc::new(FixedClock::at(start)));
        store.create("k1", TrackedJobOwner::Anonymous).await.unwrap();

        // Complete at t=8s, before the original 10s TTL expires — this must
        // push expiry out to t=18s rather than leaving it at t=10s.
        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(8),
        )));
        store
            .complete("k1", serde_json::json!({"download_url": "/blob/abc.csv"}))
            .await
            .unwrap();

        // t=15s: past the original TTL, but within the refreshed window.
        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(15),
        )));
        let record = store.get("k1").await.unwrap();
        assert!(record.is_some(), "terminal write should have refreshed the TTL");
    }

    #[tokio::test]
    async fn write_to_expired_key_is_a_no_op() {
        let start = chrono::Utc::now();
        let store = InMemoryJobTrackingStore::new(5)
            .with_clock(std::sync::Arc::new(FixedClock::at(start)));
        store.create("k1", TrackedJobOwner::Anonymous).await.unwrap();

        let store = store.with_clock(std::sync::Arc::new(FixedClock::at(
            start + chrono::TimeDelta::seconds(6),
        )));
        // Expired: reads see it as gone, and writes must not resurrect it.
        store.set_progress("k1", 50, None).await.unwrap();
        assert!(store.get("k1").await.unwrap().is_none());
    }
}
