//! The engine seam of a fleet (ADR 0040 §1).
//!
//! A [`DatabaseFleet`](super::fleet::DatabaseFleet) does the work that does
//! not depend on the database engine: keys and paths, the path lock,
//! single-flight opens, eviction, the memory budget, leases, ownership and
//! fencing. A [`FleetEngine`] does the rest: it opens, closes, copies and
//! removes one database.
//!
//! `SqliteEngine` (with the `sqlite` feature) is the first engine. An engine
//! for another database goes in that database's plugin, not in core. Test it
//! with [`plugin_conformance::fleet`](crate::plugin_conformance::fleet).
//!
//! # Contract
//!
//! - `open` makes a new database when none is at the path, and stamps the
//!   key and the epoch in it. It refuses a database that records another key
//!   ([`FleetError::Misplaced`]) or a newer epoch ([`FleetError::NewerEpoch`]).
//!   It never lowers the stamp.
//! - Core calls `open` only once at a time for one key in one process. Two
//!   processes can open one path at once: make the create atomic.
//! - `close` runs once, after core stops new use. Core then waits until
//!   `in_use` is `false`.
//! - `snapshot` writes `dir/<file name of path>`: one file, or one directory
//!   with its files. Core restores a copy by putting that entry at a
//!   database path.
//! - `remove` deletes the database at the path and its sidecars. Core holds
//!   the path lock and has closed the database in this process.

use std::future::Future;
use std::path::{Path, PathBuf};

use super::fleet::FleetError;
use crate::fleet_layout::FleetDbKey;
use crate::lock::FencingToken;

/// How the databases of an engine get to object storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShipMode {
    /// The engine streams its changes itself, through a lifecycle hook.
    /// `SQLite` WAL shipping.
    Continuous,
    /// No replica. The engine rebuilds the data from its source.
    Rebuild,
}

/// One database engine of a fleet. See the module docs for the contract.
pub trait FleetEngine: Send + Sync + 'static {
    /// What a handler uses: a connection pool, or a database handle.
    type Handle: Clone + Send + Sync + 'static;

    /// Opens the database of `key` at `path`, or makes it when none is
    /// there. Stamps `epoch` on it. A new database has the stamp before it
    /// returns.
    ///
    /// # Errors
    ///
    /// [`FleetError::Misplaced`] when the database records another key.
    /// [`FleetError::NewerEpoch`] when it records an epoch above `epoch`.
    /// Another [`FleetError`] when the open fails.
    fn open(
        &self,
        key: &FleetDbKey,
        path: &Path,
        epoch: Option<FencingToken>,
    ) -> impl Future<Output = Result<Self::Handle, FleetError>> + Send;

    /// Closes `handle`. New use fails after this. Runs on a blocking thread.
    fn close(&self, handle: &Self::Handle);

    /// Whether a user still holds part of `handle` (a checked-out
    /// connection, or a waiter). Core does not evict a database in use.
    fn in_use(&self, handle: &Self::Handle) -> bool;

    /// Writes a consistent copy of the database at `path` into `dir`, as
    /// `dir/<file name of path>`. Returns every file it wrote.
    ///
    /// # Errors
    ///
    /// A [`FleetError`] when the copy fails.
    fn snapshot(
        &self,
        handle: &Self::Handle,
        path: &Path,
        dir: &Path,
    ) -> impl Future<Output = Result<Vec<PathBuf>, FleetError>> + Send;

    /// Deletes the database at `path` and its sidecars. Returns `false`
    /// when no database was there. Runs on a blocking thread.
    ///
    /// # Errors
    ///
    /// [`FleetError::InUseElsewhere`] when another process has it open. An
    /// I/O [`FleetError`] when a file cannot be removed.
    fn remove(&self, key: &FleetDbKey, path: &Path) -> Result<bool, FleetError>;

    /// The memory the open database holds, in bytes. Eviction counts it
    /// against `max_resident_bytes`.
    fn resident_bytes(&self, handle: &Self::Handle) -> u64;

    /// How the databases get to object storage.
    fn ship_mode(&self) -> ShipMode;

    /// Files beside `path` that belong to its database (a `-wal`). Core
    /// removes them when it finds them with no database. Default: none.
    fn sidecars(&self, path: &Path) -> Vec<PathBuf> {
        let _ = path;
        Vec::new()
    }

    /// Schema migrations the engine applied since boot. Default: `0`.
    fn migrations_applied(&self) -> u64 {
        0
    }
}
