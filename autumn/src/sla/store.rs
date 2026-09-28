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
    /// Whether a `track` call finished on this record. A rollback does not
    /// remove a scheduled record.
    pub scheduled: bool,
}

impl ObligationRecord {
    /// Make a record with no escalation that is not scheduled.
    #[must_use]
    pub const fn new(obligation: Obligation) -> Self {
        Self {
            obligation,
            escalated_at: None,
            scheduled: false,
        }
    }
}

/// Storage of tracked obligations.
///
/// For more than one replica, all replicas must use the same store, and
/// [`claim_escalation`](Self::claim_escalation) must be atomic, for example
/// `UPDATE … SET escalated_at = $4 WHERE key = $1 AND started_at = $2 AND
/// escalated_at IS NULL AND (met_at IS NULL OR met_at > $3)`.
pub trait ObligationStore: Send + Sync + 'static {
    /// Add `record` if its key is new. Return the stored record, and `true`
    /// if this call created it. The check and the write must be atomic.
    fn insert(&self, record: ObligationRecord) -> StoreFuture<'_, (ObligationRecord, bool)>;

    /// Get the record for `key`.
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObligationRecord>>;

    /// Get all records, sorted by key.
    fn list(&self) -> StoreFuture<'_, Vec<ObligationRecord>>;

    /// Set the met instant if it is not set. Return `true` if it changed.
    fn mark_met<'a>(&'a self, key: &'a str, at: DateTime<Utc>) -> StoreFuture<'a, bool>;

    /// Set the escalation instant to `at` if the record for `key` started at
    /// `started_at` (the same instance), the escalation is not set, and the
    /// obligation was not met by `due_at`. Return `true` if this call set it.
    fn claim_escalation<'a>(
        &'a self,
        key: &'a str,
        started_at: DateTime<Utc>,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool>;

    /// Clear the escalation instant after a failed enqueue.
    fn release_escalation<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()>;

    /// Set `scheduled` on the record for `key`. Return `false` if there is
    /// no record.
    fn mark_scheduled<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool>;

    /// Remove the record for `key` only if it is not scheduled. The check and
    /// the delete must be atomic. Return `true` if it removed the record.
    fn remove_unscheduled<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool>;

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
    fn insert(&self, record: ObligationRecord) -> StoreFuture<'_, (ObligationRecord, bool)> {
        self.with(|records| match records.entry(record.obligation.key()) {
            std::collections::btree_map::Entry::Occupied(entry) => (entry.get().clone(), false),
            std::collections::btree_map::Entry::Vacant(entry) => {
                (entry.insert(record).clone(), true)
            }
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
        started_at: DateTime<Utc>,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        self.with(|records| match records.get_mut(key) {
            Some(record)
                if record.obligation.started_at() == Some(started_at)
                    && record.escalated_at.is_none()
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

    fn mark_scheduled<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool> {
        self.with(|records| {
            records.get_mut(key).is_some_and(|record| {
                record.scheduled = true;
                true
            })
        })
    }

    fn remove_unscheduled<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool> {
        self.with(|records| {
            let unscheduled = records.get(key).is_some_and(|record| !record.scheduled);
            unscheduled && records.remove(key).is_some()
        })
    }

    fn remove<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool> {
        self.with(|records| records.remove(key).is_some())
    }
}
