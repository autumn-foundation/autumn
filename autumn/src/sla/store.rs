//! Storage of tracked obligations.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::{Obligation, SlaError};

/// The future that an [`ObligationStore`] method returns.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SlaError>> + Send + 'a>>;

/// One tracked obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObligationRecord {
    /// The obligation. Its start instant and time zone are set.
    pub obligation: Obligation,
    /// The instant when the escalation was claimed.
    pub escalated_at: Option<DateTime<Utc>>,
}

impl ObligationRecord {
    /// Make a record with no escalation.
    #[must_use]
    pub const fn new(obligation: Obligation) -> Self {
        Self {
            obligation,
            escalated_at: None,
        }
    }
}

/// Storage of tracked obligations.
///
/// For more than one replica, all replicas must use the same store, and
/// [`claim_escalation`](Self::claim_escalation) must be atomic, for example
/// `UPDATE … SET escalated_at = $3 WHERE key = $1 AND escalated_at IS NULL
/// AND (met_at IS NULL OR met_at > $2)`.
pub trait ObligationStore: Send + Sync + 'static {
    /// Add `record` if its key is new. Return the stored record.
    fn insert(&self, record: ObligationRecord) -> StoreFuture<'_, ObligationRecord>;

    /// Get the record for `key`.
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObligationRecord>>;

    /// Get all records, sorted by key.
    fn list(&self) -> StoreFuture<'_, Vec<ObligationRecord>>;

    /// Set the met instant if it is not set. Return `true` if it changed.
    fn mark_met<'a>(&'a self, key: &'a str, at: DateTime<Utc>) -> StoreFuture<'a, bool>;

    /// Set the escalation instant to `at` if it is not set and the
    /// obligation was not met by `due_at`. Return `true` if this call set it.
    fn claim_escalation<'a>(
        &'a self,
        key: &'a str,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool>;

    /// Clear the escalation instant after a failed enqueue.
    fn release_escalation<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()>;

    /// Remove the record for `key`. Return `true` if it existed.
    fn remove<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool>;
}

/// An in-process [`ObligationStore`].
///
/// Clones share the same records. The records are lost when the process
/// stops.
#[derive(Debug, Clone, Default)]
pub struct MemoryObligationStore {
    records: Arc<Mutex<BTreeMap<String, ObligationRecord>>>,
}

impl MemoryObligationStore {
    /// Make an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `f` on the records and return its result as a ready future.
    fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, ObligationRecord>) -> T,
    ) -> StoreFuture<'_, T> {
        let value = f(&mut self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
        Box::pin(std::future::ready(Ok(value)))
    }
}

impl ObligationStore for MemoryObligationStore {
    fn insert(&self, record: ObligationRecord) -> StoreFuture<'_, ObligationRecord> {
        self.with(|records| {
            records
                .entry(record.obligation.key())
                .or_insert(record)
                .clone()
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObligationRecord>> {
        self.with(|records| records.get(key).cloned())
    }

    fn list(&self) -> StoreFuture<'_, Vec<ObligationRecord>> {
        self.with(|records| records.values().cloned().collect())
    }

    fn mark_met<'a>(&'a self, key: &'a str, at: DateTime<Utc>) -> StoreFuture<'a, bool> {
        self.with(|records| match records.get_mut(key) {
            Some(record) if record.obligation.met().is_none() => {
                record.obligation.set_met(at);
                true
            }
            _ => false,
        })
    }

    fn claim_escalation<'a>(
        &'a self,
        key: &'a str,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        self.with(|records| match records.get_mut(key) {
            Some(record)
                if record.escalated_at.is_none()
                    && record.obligation.met().is_none_or(|met| met > due_at) =>
            {
                record.escalated_at = Some(at);
                true
            }
            _ => false,
        })
    }

    fn release_escalation<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.with(|records| {
            if let Some(record) = records.get_mut(key) {
                record.escalated_at = None;
            }
        })
    }

    fn remove<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool> {
        self.with(|records| records.remove(key).is_some())
    }
}
