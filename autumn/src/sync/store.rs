//! Local offline store: SQLite rows + write-through pending journal.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::SyncError;
use super::protocol::{Change, ChangeOutcome, RemoteRow, Version};

/// The local, in-process SQLite store for offline data.
///
/// App data is stored as JSON payloads keyed by `(collection, pk)`. Every
/// write journals a pending change in the same SQLite transaction, so the
/// [`crate::sync::SyncEngine`] can replay it to the server later; deletes
/// are recorded as tombstones. Cheap to clone (all clones share one
/// serialized connection).
#[derive(Clone)]
pub struct SyncStore {
    #[allow(dead_code)]
    inner: Arc<Mutex<()>>,
}

impl SyncStore {
    /// Open (creating if needed) the store at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] if the database cannot be opened or its
    /// schema cannot be created.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SyncError> {
        let _ = path;
        Err(SyncError::Store("unimplemented".into()))
    }

    /// Insert or update `value` under `(collection, pk)`.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Serde`] if `value` cannot be serialized, or
    /// [`SyncError::Store`] on database failure.
    pub fn put<T: Serialize>(
        &self,
        collection: &str,
        pk: &str,
        value: &T,
    ) -> Result<(), SyncError> {
        let _ = (collection, pk, value);
        Err(SyncError::Store("unimplemented".into()))
    }

    /// Fetch the row at `(collection, pk)`, or `None` if absent or deleted.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Serde`] if the payload cannot be deserialized as
    /// `T`, or [`SyncError::Store`] on database failure.
    pub fn get<T: DeserializeOwned>(
        &self,
        collection: &str,
        pk: &str,
    ) -> Result<Option<T>, SyncError> {
        let _ = (collection, pk);
        Err(SyncError::Store("unimplemented".into()))
    }

    /// Delete the row at `(collection, pk)`, recording a tombstone.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn delete(&self, collection: &str, pk: &str) -> Result<(), SyncError> {
        let _ = (collection, pk);
        Err(SyncError::Store("unimplemented".into()))
    }

    /// List all live (non-tombstoned) rows in `collection`, ordered by pk.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Serde`] if a payload cannot be deserialized as
    /// `T`, or [`SyncError::Store`] on database failure.
    pub fn list<T: DeserializeOwned>(&self, collection: &str) -> Result<Vec<(String, T)>, SyncError> {
        let _ = collection;
        Err(SyncError::Store("unimplemented".into()))
    }

    /// Number of journaled changes not yet acknowledged by the server.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn pending_count(&self) -> Result<u64, SyncError> {
        Err(SyncError::Store("unimplemented".into()))
    }

    /// The oldest pending changes, up to `limit`, in queue order.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn pending_changes(&self, limit: usize) -> Result<Vec<Change>, SyncError> {
        let _ = limit;
        Err(SyncError::Store("unimplemented".into()))
    }

    /// This device's stable id (UUID v4, generated on first open).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn device_id(&self) -> Result<String, SyncError> {
        Err(SyncError::Store("unimplemented".into()))
    }

    /// The last server version this store has pulled through.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn cursor(&self) -> Result<Version, SyncError> {
        Err(SyncError::Store("unimplemented".into()))
    }

    pub(crate) fn set_cursor(&self, cursor: Version) -> Result<(), SyncError> {
        let _ = cursor;
        Err(SyncError::Store("unimplemented".into()))
    }

    pub(crate) fn confirm_pushed(
        &self,
        changes: &[Change],
        outcomes: &[ChangeOutcome],
    ) -> Result<(), SyncError> {
        let _ = (changes, outcomes);
        Err(SyncError::Store("unimplemented".into()))
    }

    pub(crate) fn apply_remote_rows(&self, rows: &[RemoteRow]) -> Result<usize, SyncError> {
        let _ = rows;
        Err(SyncError::Store("unimplemented".into()))
    }

    pub(crate) fn begin_full_resync(&self) -> Result<(), SyncError> {
        Err(SyncError::Store("unimplemented".into()))
    }
}
