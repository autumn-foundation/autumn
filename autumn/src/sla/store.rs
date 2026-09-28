//! Storage of tracked obligations.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{Obligation, SlaError};

/// The future that an [`ObligationStore`] method returns.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SlaError>> + Send + 'a>>;

/// One tracked obligation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObligationRecord {
    /// The obligation. Its start instant and time zone are set.
    pub obligation: Obligation,
    /// A unique id for this record. A `forget` and a new `track` of the same
    /// key make a record with a new generation.
    pub generation: Uuid,
    /// The instant when the escalation was claimed.
    pub escalated_at: Option<DateTime<Utc>>,
}

impl ObligationRecord {
    /// Make a new record with no escalation.
    #[must_use]
    pub const fn new(obligation: Obligation, generation: Uuid) -> Self {
        Self {
            obligation,
            generation,
            escalated_at: None,
        }
    }
}

/// Storage of tracked obligations.
///
/// For more than one replica, all replicas must use the same store. Each
/// write that takes a `generation` must change the record only when the key
/// and the generation both match, in one atomic step. For example:
///
/// ```sql
/// UPDATE sla_obligations SET escalated_at = $4
/// WHERE key = $1 AND generation = $2
///   AND escalated_at IS NULL AND (met_at IS NULL OR met_at > $3)
/// ```
pub trait ObligationStore: Send + Sync + 'static {
    /// Add `record` if its key is new. Return the stored record, and `true`
    /// if this call created it. The check and the write must be atomic.
    fn insert(&self, record: ObligationRecord) -> StoreFuture<'_, (ObligationRecord, bool)>;

    /// Get the record for `key`.
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObligationRecord>>;

    /// Get all records, sorted by key.
    fn list(&self) -> StoreFuture<'_, Vec<ObligationRecord>>;

    /// Set the met instant if it is not set. Return `true` if it changed.
    fn mark_met<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool>;

    /// Set the escalation instant to `at` if the escalation is not set and
    /// the obligation was not met by `due_at`. Return `true` if this call set
    /// it.
    fn claim_escalation<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool>;

    /// Clear the escalation instant after a failed enqueue, only if it is
    /// still `claimed_at`.
    fn release_escalation<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> StoreFuture<'a, ()>;

    /// Remove the record for `key`, whatever its generation. Return `true`
    /// if it existed.
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

    /// Run `f` on the record for `key` if its generation is `generation`.
    /// Return `None` if there is no such record.
    fn with_instance<T: Send + 'static>(
        &self,
        key: &str,
        generation: Uuid,
        f: impl FnOnce(&mut ObligationRecord) -> T,
    ) -> StoreFuture<'_, Option<T>> {
        self.with(|records| {
            records
                .get_mut(key)
                .filter(|record| record.generation == generation)
                .map(f)
        })
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

    fn mark_met<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        let changed = self.with_instance(key, generation, |record| {
            let unset = record.obligation.met().is_none();
            if unset {
                record.obligation.set_met(at);
            }
            unset
        });
        Box::pin(async move { Ok(changed.await?.unwrap_or(false)) })
    }

    fn claim_escalation<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        due_at: DateTime<Utc>,
        at: DateTime<Utc>,
    ) -> StoreFuture<'a, bool> {
        let claimed = self.with_instance(key, generation, |record| {
            let open = record.escalated_at.is_none()
                && record.obligation.met().is_none_or(|met| met > due_at);
            if open {
                record.escalated_at = Some(at);
            }
            open
        });
        Box::pin(async move { Ok(claimed.await?.unwrap_or(false)) })
    }

    fn release_escalation<'a>(
        &'a self,
        key: &'a str,
        generation: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> StoreFuture<'a, ()> {
        let released = self.with_instance(key, generation, |record| {
            if record.escalated_at == Some(claimed_at) {
                record.escalated_at = None;
            }
        });
        Box::pin(async move { released.await.map(|_| ()) })
    }

    fn remove<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool> {
        self.with(|records| records.remove(key).is_some())
    }
}
