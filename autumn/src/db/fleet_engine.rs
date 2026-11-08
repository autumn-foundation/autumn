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
//!   with its files. To restore a copy, core puts that entry at a database
//!   path.
//! - `remove` deletes the database at the path and its sidecars. Core holds
//!   the path lock and has closed the database in this process.
//! - In [`ShipMode::Generations`], the snapshot of a database with no change
//!   must be the same bytes. Core then ships nothing.
//!
//! # Shipping
//!
//! Core owns every write to the replica of a database: the generations, the
//! `epoch-<n>` markers, the tombstone marker and the restores. An engine
//! gives files to core (`snapshot`) and never writes the replica itself.
//! Core checks the fence before each `put`, in every mode. See
//! [`FencedDestination`](crate::replication::FencedDestination).

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::fleet::FleetError;
use crate::fleet_layout::FleetDbKey;
use crate::lock::FencingToken;

/// How the databases of an engine get to object storage.
#[derive(Clone)]
#[non_exhaustive]
pub enum ShipMode {
    /// The engine streams its changes. Core drives the shipper and fences
    /// each `put`. Core has one continuous shipper: `SQLite` WAL shipping.
    Continuous(Arc<dyn ContinuousShipper>),
    /// Core calls [`FleetEngine::snapshot`] every `every` while the
    /// database is open, and once at close. It ships only the files whose
    /// content hash is not in the previous generation. The RPO is `every`.
    Generations {
        /// How often core ships a generation. Zero means one second.
        every: Duration,
    },
    /// No replica. A restore calls [`FleetEngine::rebuild`]. No RPO: the
    /// data is derived from a source.
    Rebuild,
}

impl ShipMode {
    /// The name for health and stats: `continuous`, `generations` or
    /// `rebuild`.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Continuous(_) => "continuous",
            Self::Generations { .. } => "generations",
            Self::Rebuild => "rebuild",
        }
    }

    /// The interval of `Generations`, at least one second. `None` in the
    /// other modes.
    #[must_use]
    pub const fn every(&self) -> Option<Duration> {
        match self {
            Self::Generations { every } if every.is_zero() => Some(Duration::from_secs(1)),
            Self::Generations { every } => Some(*every),
            _ => None,
        }
    }
}

impl fmt::Debug for ShipMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Continuous(shipper) => {
                f.debug_tuple("Continuous").field(&shipper.name()).finish()
            }
            Self::Generations { every } => {
                f.debug_struct("Generations").field("every", every).finish()
            }
            Self::Rebuild => f.write_str("Rebuild"),
        }
    }
}

mod sealed {
    /// Only core implements [`ContinuousShipper`](super::ContinuousShipper).
    pub trait Sealed {}
}

/// A shipper that streams the changes of a database. Core drives it and
/// fences each `put`. The trait is sealed: core has one, `SQLite` WAL
/// shipping. An engine plugin uses [`ShipMode::Generations`].
pub trait ContinuousShipper: sealed::Sealed + Send + Sync + 'static {
    /// The name of the shipper, for health and logs.
    fn name(&self) -> &'static str;
}

/// `SQLite` WAL shipping: the continuous shipper of `SqliteEngine`.
#[cfg(feature = "sqlite")]
#[derive(Debug)]
pub(crate) struct WalShipper;

#[cfg(feature = "sqlite")]
impl sealed::Sealed for WalShipper {}

#[cfg(feature = "sqlite")]
impl ContinuousShipper for WalShipper {
    fn name(&self) -> &'static str {
        "sqlite-wal"
    }
}

/// One database engine of a fleet. See the module docs for the contract.
pub trait FleetEngine: Send + Sync + 'static {
    /// What a handler uses: a connection pool, or a database handle.
    type Handle: Clone + Send + Sync + 'static;

    /// Opens the database of `key` at `path`, or makes it when none is
    /// there. Stamps `epoch` on it. A new database has the stamp before it
    /// returns. With `epoch` `None` (no ownership), stamp and check no
    /// epoch. Make the check and the stamp one atomic step: two processes
    /// can open one path at once.
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
    /// Core calls this with its state lock held: keep it fast, do not
    /// block, and do not call the fleet.
    fn in_use(&self, handle: &Self::Handle) -> bool;

    /// Writes a consistent copy of the database at `path` into `dir`, as
    /// `dir/<file name of path>`. Returns every file it wrote.
    ///
    /// In [`ShipMode::Generations`], core also calls it with a closed
    /// `handle`, for the last generation of a close. Every use has ended:
    /// copy the files at `path`.
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
    /// against `max_resident_bytes`. Core calls this with its state lock
    /// held: keep it fast, do not block, and do not call the fleet.
    fn resident_bytes(&self, handle: &Self::Handle) -> u64;

    /// How the databases get to object storage.
    fn ship_mode(&self) -> ShipMode;

    /// Whether a database is at `path`. Default: anything is there. The
    /// `SQLite` engine wants a file: a directory at the path is no database.
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

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

    /// The epoch stamp of the database at `path`. Runs on a blocking
    /// thread. Default: `None`, unknown.
    ///
    /// Core reads it before an open under ownership. When the replica holds
    /// a newer epoch of another owner, core restores the database from the
    /// replica first. With `None`, in [`ShipMode::Generations`], core
    /// counts the stamp as 0: it restores at each new epoch that another
    /// owner wrote. In the other modes it keeps the database on disk.
    ///
    /// # Errors
    ///
    /// A [`FleetError`] when the stamp cannot be read.
    fn epoch_stamp(&self, key: &FleetDbKey, path: &Path) -> Result<Option<u64>, FleetError> {
        let _ = (key, path);
        Ok(None)
    }

    /// Builds the database of `key` at `path` again from its source. For
    /// [`ShipMode::Rebuild`]: core calls it for a restore of a database that
    /// is on disk. A deleted database is not rebuilt. Core has closed the
    /// database and keeps new opens out. Stamp the key, as `open` does.
    /// Default: [`FleetError::NotReplicated`].
    ///
    /// # Errors
    ///
    /// A [`FleetError`] when the rebuild fails.
    fn rebuild(
        &self,
        key: &FleetDbKey,
        path: &Path,
    ) -> impl Future<Output = Result<(), FleetError>> + Send {
        let _ = path;
        let name = key.name();
        async move { Err(FleetError::NotReplicated { name }) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine that keeps every default method.
    struct Bare;

    #[derive(Clone)]
    struct BareHandle;

    impl FleetEngine for Bare {
        type Handle = BareHandle;

        async fn open(
            &self,
            _: &FleetDbKey,
            _: &Path,
            _: Option<FencingToken>,
        ) -> Result<BareHandle, FleetError> {
            Ok(BareHandle)
        }

        fn close(&self, _: &BareHandle) {}

        fn in_use(&self, _: &BareHandle) -> bool {
            false
        }

        async fn snapshot(
            &self,
            _: &BareHandle,
            _: &Path,
            _: &Path,
        ) -> Result<Vec<PathBuf>, FleetError> {
            Ok(Vec::new())
        }

        fn remove(&self, _: &FleetDbKey, _: &Path) -> Result<bool, FleetError> {
            Ok(false)
        }

        fn resident_bytes(&self, _: &BareHandle) -> u64 {
            0
        }

        fn ship_mode(&self) -> ShipMode {
            ShipMode::Rebuild
        }
    }

    #[test]
    fn each_mode_has_a_name_and_an_interval() {
        let generations = ShipMode::Generations {
            every: Duration::from_secs(30),
        };
        assert_eq!(generations.name(), "generations");
        assert_eq!(generations.every(), Some(Duration::from_secs(30)));
        assert_eq!(
            ShipMode::Generations {
                every: Duration::ZERO
            }
            .every(),
            Some(Duration::from_secs(1)),
            "zero ships every second"
        );
        assert_eq!(ShipMode::Rebuild.name(), "rebuild");
        assert_eq!(ShipMode::Rebuild.every(), None);
        assert_eq!(format!("{generations:?}"), "Generations { every: 30s }");
        assert_eq!(format!("{:?}", ShipMode::Rebuild), "Rebuild");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn the_continuous_mode_names_its_shipper() {
        let mode = ShipMode::Continuous(Arc::new(WalShipper));
        assert_eq!(mode.name(), "continuous");
        assert_eq!(mode.every(), None);
        assert_eq!(format!("{mode:?}"), "Continuous(\"sqlite-wal\")");
    }

    #[tokio::test]
    async fn the_defaults_know_no_stamp_and_rebuild_nothing() {
        let key = FleetDbKey::Slot(3);
        let path = Path::new("db");
        assert_eq!(Bare.epoch_stamp(&key, path).unwrap(), None);
        assert!(matches!(
            Bare.rebuild(&key, path).await,
            Err(FleetError::NotReplicated { .. })
        ));
        assert!(Bare.sidecars(path).is_empty());
        assert_eq!(Bare.migrations_applied(), 0);
        assert!(!Bare.exists(Path::new("/no/such/db")));
        assert!(Bare.open(&key, path, None).await.is_ok());
        assert!(
            Bare.snapshot(&BareHandle, path, path)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!Bare.remove(&key, path).unwrap());
        assert_eq!(Bare.resident_bytes(&BareHandle), 0);
        assert!(!Bare.in_use(&BareHandle));
        Bare.close(&BareHandle);
    }
}
