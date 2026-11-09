//! A fleet of databases: one per tenant, or one per routing slot (ADR 0019,
//! ADR 0040).
//!
//! One database per tenant (or per slot) gives each tenant its own writer and
//! its own file to export, restore or delete. The fleet keeps everything on
//! the one volume a host can attach.
//!
//! A [`DatabaseFleet`] opens databases on first use and closes idle ones. It
//! does not depend on the engine: a [`FleetEngine`] opens, closes, copies and
//! removes one database. With the `sqlite` feature, `SqliteEngine` is the
//! default engine, and `DatabaseFleet` means `DatabaseFleet<SqliteEngine>`.
//!
//! ```text
//! open(key)
//!   wait while the key is draining or being deleted
//!   single-flight per key ──► database exists?  no ─► create_on_demand? no ─► 404
//!                              engine.open(key, path, epoch)
//!                                (create, stamp key and epoch, refuse a moved file)
//!                              run on_open hooks (commit-hook worker, replicator)
//!   ◄── FleetDatabase (cheap clone of the engine handle)
//!
//! close(key)   — LRU past max_open or max_resident_bytes, idle past
//!                idle_close, delete, shutdown
//!   remove from the map  ──► stop workers (wait, bounded)
//!   engine.close(handle) (new use fails fast)
//!   wait until the engine reports no use ──► on_close hooks (final ship)
//! ```
//!
//! # Migrations (`SQLite`)
//!
//! Every `SQLite` database gets the app's registered migrations and the shard
//! framework sets (version history, commit hooks, derivations) — never the
//! control-plane set, which stays on the control database. A database is
//! migrated when it is created. An existing database is migrated when it is
//! opened if the boot's migration policy auto-applies
//! (`database.auto_migrate` / `auto_migrate_in_production`); otherwise opening
//! one with pending migrations is refused with `503` until `AUTUMN_MIGRATE=1`
//! (which calls `DatabaseFleet::migrate_all`) has run. Each database's
//! sequence runs under one `BEGIN IMMEDIATE`, so it is atomic and two
//! processes cannot interleave it.
//!
//! The fleet is therefore mixed during a rollout: code must work against the
//! schema before and after each migration (expand, then contract).
//!
//! # Eviction safety
//!
//! A database is closed only when the engine reports no use of it and no
//! lease is held. Closing calls [`FleetEngine::close`], so a stale clone of
//! the handle can no longer be used, and the `on_close` hooks run only after
//! the last use has ended. A request for a database that is draining waits
//! for the drain, then opens it fresh.
//!
//! # Cancellation
//!
//! `open`, `close`, `delete`, and (`SQLite`) `restore` and `provision` do their work in owned
//! tasks. If the caller is dropped, the work still finishes: a drain always
//! ends, and a started open stays tracked. See ADR 0019, section 3.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, OnceCell};
use tokio_util::sync::CancellationToken;

pub use super::fleet_engine::{ContinuousShipper, FleetEngine, ShipMode};
use crate::config::DatabaseFleetConfig;
use crate::error::AutumnError;
use crate::fleet_layout::{FleetDbKey, FleetLayoutError, FleetMode, FleetPathTemplate, TenantDbId};

/// Replica steps of the core: tombstones, restores and purges. They need
/// `SQLite` replication.
#[cfg(feature = "sqlite")]
mod replica;
/// Without `sqlite` there is no replication: each replica step does nothing.
#[cfg(not(feature = "sqlite"))]
mod replica {
    use super::{DatabaseFleet, FleetDbKey, FleetEngine, FleetError, Path, ShipMode, ShipReport};

    /// No tombstone row to write.
    pub(super) struct DeleteRecord;

    /// No control connection to hold.
    pub(super) struct NoControl;

    impl DeleteRecord {
        #[allow(clippy::unused_async)]
        pub(super) async fn checkout(&self) -> Result<NoControl, FleetError> {
            Ok(NoControl)
        }

        #[allow(clippy::unused_async)]
        pub(super) async fn record(&self, _: &mut NoControl, _: bool) -> Result<(), FleetError> {
            Ok(())
        }

        #[allow(clippy::unused_async)]
        pub(super) async fn drop_pending(&self, _: &FleetDbKey) {}
    }

    #[allow(clippy::unused_async, clippy::unused_self)]
    impl<E: FleetEngine> DatabaseFleet<E> {
        pub(super) const fn unshipped(&self) -> bool {
            false
        }

        #[allow(dead_code)]
        pub(super) const fn replicates(&self) -> bool {
            false
        }

        pub(super) const fn delete_record(&self, _: &str) -> DeleteRecord {
            DeleteRecord
        }

        pub(super) async fn deleted_in_replica(&self, _: &FleetDbKey) -> Result<bool, FleetError> {
            Ok(false)
        }

        pub(super) const fn wire_fence_hook(&self) {}

        pub(super) const fn replica_base(&self, _: &FleetDbKey) -> Option<super::ReplicaBase> {
            None
        }

        pub(super) async fn check_epoch_stamp(
            &self,
            _: &FleetDbKey,
            _: &Path,
            _: crate::lock::FencingToken,
        ) -> Result<(), FleetError> {
            Ok(())
        }

        pub(super) async fn purge_before_create(
            &self,
            _: &FleetDbKey,
            _: bool,
        ) -> Result<(), FleetError> {
            Ok(())
        }

        pub(super) async fn purge_after_delete(&self, _: &FleetDbKey) {}

        #[allow(clippy::needless_pass_by_value)]
        pub(super) fn spawn_replica_lifecycle(
            &self,
            _: tokio_util::sync::CancellationToken,
        ) -> Option<tokio::task::JoinHandle<()>> {
            None
        }

        pub(super) const fn begin_replica_shutdown(&self, _: std::time::Instant) {}

        pub(super) async fn finish_replica_shutdown(&self, _: std::time::Instant) {}

        pub(super) const fn rpo(&self) -> Option<std::time::Duration> {
            None
        }

        pub(super) async fn ship_replica(
            &self,
            key: &FleetDbKey,
        ) -> Result<ShipReport, FleetError> {
            Err(FleetError::NotReplicated { name: key.name() })
        }

        /// No replica: only a `rebuild` engine restores.
        pub(super) async fn replace_from_replica(
            &self,
            key: &FleetDbKey,
            _: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<Option<crate::replication::RestoreOutcome>, FleetError> {
            if !matches!(self.inner.engine.ship_mode(), ShipMode::Rebuild) {
                return Err(FleetError::NotReplicated { name: key.name() });
            }
            let path = self.path_of(key);
            super::check_contained(self.root(), &path)?;
            // A rebuild replaces a database: it never brings back a deleted
            // one. The path lock keeps a delete of another process out.
            let (root, name) = (self.root().to_path_buf(), key.name());
            let _lock = crate::time::spawn_blocking(move || super::PathLock::acquire(&root, &name))
                .await
                .map_err(|error| FleetError::Restore {
                    name: key.name(),
                    detail: error.to_string(),
                })??;
            if !self.inner.engine.exists(&path) {
                return Err(FleetError::NotFound { name: key.name() });
            }
            self.inner.engine.rebuild(key, &path).await.map(|()| None)
        }
    }
}
/// The `SQLite` engine and the `SQLite`-only fleet operations.
#[cfg(feature = "sqlite")]
mod sqlite;

/// Generation shipping: `ShipMode::Generations`.
#[cfg(feature = "sqlite")]
pub(crate) mod generations;

/// Restore one database from a local backup file.
#[cfg(feature = "sqlite")]
mod local_restore;

#[cfg(feature = "sqlite")]
pub use local_restore::{LocalRestore, LocalRestoreOutcome};
#[cfg(feature = "sqlite")]
pub use sqlite::{DatabaseFleetBuilder, FleetDbInfo, FleetMigrationReport, SqliteEngine};
#[cfg(feature = "sqlite")]
pub(crate) use sqlite::{build_for_app, read_identity};

/// Declares a type or a trait with the parameter `E: FleetEngine`. With
/// `sqlite`, `E` defaults to `SqliteEngine`, so the bare name is the
/// `SQLite` fleet.
macro_rules! default_engine {
    ($(#[$meta:meta])* $vis:vis struct $name:ident<E: FleetEngine> $($rest:tt)*) => {
        #[cfg(feature = "sqlite")]
        $(#[$meta])*
        $vis struct $name<E: FleetEngine = SqliteEngine> $($rest)*
        #[cfg(not(feature = "sqlite"))]
        $(#[$meta])*
        $vis struct $name<E: FleetEngine> $($rest)*
    };
    ($(#[$meta:meta])* $vis:vis trait $name:ident<E: FleetEngine> $($rest:tt)*) => {
        #[cfg(feature = "sqlite")]
        $(#[$meta])*
        $vis trait $name<E: FleetEngine = SqliteEngine> $($rest)*
        #[cfg(not(feature = "sqlite"))]
        $(#[$meta])*
        $vis trait $name<E: FleetEngine> $($rest)*
    };
}

/// How many databases [`DatabaseFleet::each`] and `migrate_all` work on at
/// once by default.
pub const DEFAULT_FLEET_CONCURRENCY: usize = 8;

/// How often the idle sweeper looks for databases to close, at most.
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How long a database stays open after its last use before LRU may close it,
/// when `database.fleet.min_residency` is not set.
///
/// A request resolves its database, then checks a connection out a moment
/// later (a generated repository even later, lazily); without this grace a
/// burst of opens could close a database between the two and fail the
/// request with a closed database.
pub const DEFAULT_MIN_RESIDENCY: Duration = Duration::from_secs(2);

/// How often a drain checks whether the last use of a database has ended.
const DRAIN_POLL: Duration = Duration::from_millis(20);

/// How long a close waits for the workers of one database to stop. Workers
/// of different databases stop together, so this also bounds a shutdown.
pub const WORKER_QUIESCE_TIMEOUT: Duration = Duration::from_secs(10);

// ── Errors ───────────────────────────────────────────────────────────────────

/// Why a fleet operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FleetError {
    /// The routing key cannot name a database (`400`).
    #[error(transparent)]
    InvalidKey(#[from] FleetLayoutError),
    /// The database does not exist and is not created on demand (`404`).
    #[error("fleet database {name} does not exist; provision it first")]
    NotFound {
        /// The database name.
        name: String,
    },
    /// `DatabaseFleet::provision` or `DatabaseFleet::import` was asked
    /// for a database that exists (`409`).
    #[error("fleet database {name} already exists")]
    AlreadyExists {
        /// The database name.
        name: String,
    },
    /// The database is being deleted (`410`).
    #[error("fleet database {name} is being deleted")]
    Deleting {
        /// The database name.
        name: String,
    },
    /// The database has migrations this boot will not apply (`503`,
    /// `SQLite`).
    #[error(
        "fleet database {name} has {count} pending migration(s) and this boot does not \
         auto-apply them; run the app once with AUTUMN_MIGRATE=1"
    )]
    PendingMigrations {
        /// The database name.
        name: String,
        /// Number of pending migrations.
        count: usize,
    },
    /// The file at this key's path belongs to another key (`500`): it was
    /// copied or moved by hand. Serving it would show one tenant another's data.
    #[error(
        "fleet database file for {name} identifies itself as {found}; refusing to serve it \
         (was the file copied or renamed?)"
    )]
    Misplaced {
        /// The key that was opened.
        name: String,
        /// The name recorded inside the file.
        found: String,
    },
    /// A migration failed (`503`, `SQLite`).
    #[error("fleet database {name}: {detail}")]
    Migration {
        /// The database name.
        name: String,
        /// Redacted detail.
        detail: String,
    },
    /// The engine could not open or use the database (`503`).
    #[error("fleet database {name}: {detail}")]
    Unavailable {
        /// The database name.
        name: String,
        /// Detail.
        detail: String,
    },
    /// Another process sharing the fleet root has the database open, so it
    /// cannot be deleted or replaced from here (`409`).
    #[error(
        "fleet database {name} is open in another process; close it there (or stop that \
         process) before deleting or restoring it"
    )]
    InUseElsewhere {
        /// The database name.
        name: String,
    },
    /// A restore from the replica failed, or a restore from a local file
    /// refused its file or the key (`503`).
    #[error("fleet database {name}: restore failed: {detail}")]
    Restore {
        /// The database name.
        name: String,
        /// Detail.
        detail: String,
    },
    /// The fleet does not replicate, so there is nothing to restore from (`503`).
    #[error("fleet database {name}: the fleet is not replicated ([replication] is off)")]
    NotReplicated {
        /// The database name.
        name: String,
    },
    /// This host does not own the key's slot at the current epoch (`421`).
    /// Another host serves it, or this host waits for the slot's lease.
    #[error("fleet database {name}: this host does not own slot {slot}")]
    NotOwner {
        /// The database name.
        name: String,
        /// The routing slot.
        slot: u16,
    },
    /// The database records an ownership epoch above the epoch of this open
    /// (`421`). A newer owner opened it, so this host is fenced.
    #[error(
        "fleet database {name} records epoch {found}, above this host's epoch {epoch}; \
         a newer owner serves it"
    )]
    NewerEpoch {
        /// The database name.
        name: String,
        /// The epoch of this open.
        epoch: u64,
        /// The epoch the database records.
        found: u64,
    },
    /// A newer epoch owns the replica of the database (`421`). The fence of
    /// core refused the ship before any write.
    #[error(
        "fleet database {name}: epoch {newest} owns the replica now, so this host's ship under \
         epoch {epoch} is refused"
    )]
    Fenced {
        /// The database name.
        name: String,
        /// The epoch of this host.
        epoch: u64,
        /// The newer epoch.
        newest: u64,
    },
    /// `DatabaseFleet::import` (`SQLite`) refused its source file (`422`): it is
    /// missing, is not a database, fails the integrity check, or does not
    /// record the key in its identity row. Nothing was changed.
    #[error("cannot import {path} as fleet database {name}: {detail}")]
    InvalidSource {
        /// The database name.
        name: String,
        /// The source path.
        path: String,
        /// Why the source was refused.
        detail: String,
    },
    /// The request names no routing key (`400`): no tenant, and no
    /// `ShardKeyOverride`.
    #[error("no fleet routing key: {detail}")]
    NoRoutingKey {
        /// Why no key was found.
        detail: String,
    },
    /// The app has no fleet of this engine (`503`).
    #[error("no fleet of engine {engine} is installed")]
    NoFleet {
        /// The engine type.
        engine: &'static str,
    },
    /// A file operation failed (`500`).
    #[error("fleet {op}: {detail}")]
    Io {
        /// What was being attempted.
        op: &'static str,
        /// I/O detail.
        detail: String,
    },
}

impl FleetError {
    /// The HTTP status a request that hit this error answers with. Applied
    /// by [`AutumnError`]'s `From` impl, so `?` on a fleet call in a handler
    /// produces it.
    #[must_use]
    pub const fn http_status(&self) -> http::StatusCode {
        use http::StatusCode;
        match self {
            Self::InvalidKey(_) | Self::NoRoutingKey { .. } => StatusCode::BAD_REQUEST,
            Self::InvalidSource { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::AlreadyExists { .. } | Self::InUseElsewhere { .. } => StatusCode::CONFLICT,
            Self::Deleting { .. } => StatusCode::GONE,
            Self::NotOwner { .. } | Self::NewerEpoch { .. } | Self::Fenced { .. } => {
                StatusCode::MISDIRECTED_REQUEST
            }
            Self::PendingMigrations { .. }
            | Self::Migration { .. }
            | Self::Unavailable { .. }
            | Self::Restore { .. }
            | Self::NotReplicated { .. }
            | Self::NoFleet { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Misplaced { .. } | Self::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Names in the replica of a database that a sidecar must not take.
pub const RESERVED_SIDECAR_NAMES: [&str; 3] = ["generations", "epochs", "tombstone"];

/// The key prefix of sidecar `engine` under the replica prefix `root`:
/// `<root>/<engine>`.
///
/// A sidecar name is 1 to 32 bytes of `a-z`, `0-9`, `_` and `-`. It starts
/// with a letter or a digit, and it is not in [`RESERVED_SIDECAR_NAMES`].
///
/// # Errors
///
/// [`DestinationError::Rejected`](crate::replication::DestinationError::Rejected)
/// for any other name.
pub fn sidecar_root(
    root: &str,
    engine: &str,
) -> Result<String, crate::replication::DestinationError> {
    check_sidecar_name(engine)?;
    Ok(format!("{root}/{engine}"))
}

/// See [`sidecar_root`].
fn check_sidecar_name(engine: &str) -> Result<(), crate::replication::DestinationError> {
    let starts_well = engine
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let valid = starts_well
        && engine.len() <= 32
        && engine
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && !RESERVED_SIDECAR_NAMES.contains(&engine);
    if !valid {
        return Err(crate::replication::DestinationError::Rejected {
            detail: format!(
                "sidecar name {engine:?} is not 1 to 32 bytes of a-z, 0-9, _ and -, or is \
                 reserved ({})",
                RESERVED_SIDECAR_NAMES.join(", ")
            ),
        });
    }
    Ok(())
}

fn io_error(op: &'static str) -> impl FnOnce(std::io::Error) -> FleetError {
    move |e| FleetError::Io {
        op,
        detail: e.to_string(),
    }
}

// ── Public handle types ──────────────────────────────────────────────────────

default_engine! {
/// One open database in the fleet. Cheap to clone.
///
/// Every live handle (and every clone) is a lease: while any exists, idle,
/// `max_open` and memory-budget eviction skip the database, so a database
/// in use is never closed under its holder. Hold a handle for a request or a task, never in
/// a long-lived cache: each retained handle pins one open database, and
/// enough of them defeat `max_open` and `idle_close_secs`. Call
/// [`DatabaseFleet::open`] again instead; it is cheap while the database is
/// open.
///
/// An explicit [`DatabaseFleet::close`], [`DatabaseFleet::delete`] or
/// `restore` does not wait for leases. A delete or a restore stops the
/// workers and closes the handle at once. [`DatabaseFleet::close`] first
/// lets the workers finish their current hook (for at most
/// [`WORKER_QUIESCE_TIMEOUT`]), then closes the handle. After that new use
/// fails, and the next [`DatabaseFleet::open`] opens the database fresh.
pub struct FleetDatabase<E: FleetEngine> {
    key: FleetDbKey,
    path: Arc<Path>,
    handle: E::Handle,
    closed: CancellationToken,
    /// Counts the holders of this database. The fleet's own copy is one; any
    /// more and the database is in use, so it is never closed.
    lease: Arc<()>,
    /// The ownership epoch this database opened under. `None` without
    /// ownership.
    epoch: Option<crate::lock::FencingToken>,
    /// Background work to stop before the database closes. `None` once the
    /// stop has begun: a later registration is stopped at once.
    workers: Arc<Mutex<Option<Vec<Worker>>>>,
    /// The replica of this database: the destination and the key prefix.
    /// `None` when the fleet does not replicate.
    replica: Option<Arc<ReplicaBase>>,
    /// Set when the database closes, or is fenced, deleted or restored. A
    /// sidecar scope then refuses writes.
    retired: Arc<AtomicBool>,
}
}

/// Where the replica of one database is: a destination and a key prefix.
pub(crate) struct ReplicaBase {
    destination: Arc<dyn crate::replication::ReplicaDestination>,
    root: String,
    /// The fleet's ownership, so a sidecar write checks the epoch.
    ownership: Option<Arc<super::fleet_ownership::FleetOwnership>>,
}

impl ReplicaBase {
    /// The fence of the database `key` opened under `epoch`: a newer epoch
    /// at the destination, or one this host saw, refuses each write.
    fn fence(
        &self,
        key: &FleetDbKey,
        epoch: Option<crate::lock::FencingToken>,
    ) -> crate::replication::ShipFence {
        let fence = crate::replication::ShipFence::new(
            Arc::clone(&self.destination),
            self.root.clone(),
            epoch.map(crate::lock::FencingToken::get),
        );
        let Some(ownership) = &self.ownership else {
            return fence;
        };
        let slot = key.routing_slot();
        let (known, lost) = (Arc::downgrade(ownership), Arc::downgrade(ownership));
        fence
            .known_epoch(Arc::new(move || {
                known.upgrade().map_or(0, |o| o.fence_floor(slot))
            }))
            .on_fenced(Arc::new(move |newest| {
                let newer = i64::try_from(newest)
                    .ok()
                    .and_then(|n| crate::lock::FencingToken::try_from(n).ok());
                if let (Some(ownership), Some(newer)) = (lost.upgrade(), newer) {
                    ownership.lose(slot, newer);
                }
            }))
    }
}

impl<E: FleetEngine> Clone for FleetDatabase<E> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            path: Arc::clone(&self.path),
            handle: self.handle.clone(),
            closed: self.closed.clone(),
            lease: Arc::clone(&self.lease),
            epoch: self.epoch,
            workers: Arc::clone(&self.workers),
            replica: self.replica.clone(),
            retired: Arc::clone(&self.retired),
        }
    }
}

/// A task registered with [`FleetDatabase::register_worker`].
struct Worker {
    stop: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl<E: FleetEngine> FleetDatabase<E> {
    /// Which database this is.
    #[must_use]
    pub const fn key(&self) -> &FleetDbKey {
        &self.key
    }

    /// The database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The ownership epoch this database opened under (issue #3233). `None`
    /// when the fleet has no ownership.
    #[must_use]
    pub const fn epoch(&self) -> Option<crate::lock::FencingToken> {
        self.epoch
    }

    /// The engine handle: what a handler uses.
    #[must_use]
    pub const fn handle(&self) -> &E::Handle {
        &self.handle
    }

    /// The replica destination of a sidecar engine: the keys under
    /// `<key prefix>/<engine>/`, next to this database's replica. A sidecar
    /// follows this database's key, so it ships its own files there.
    /// `Ok(None)` when the fleet does not replicate.
    ///
    /// Put each copy under `generations/<id>/` of the scope, with an id of
    /// [`generation_id`](crate::replication::segment::generation_id). A purge
    /// after a delete keeps the generations of a newer database of the same
    /// key. It removes every other file of the scope.
    ///
    /// The scope refuses writes after the database closes, or after a fence,
    /// a delete or a restore. It also refuses them as soon as this host does
    /// not hold the slot at the epoch the database opened under. Ship the
    /// last files in `on_close`. A fenced database calls `on_close` with the
    /// writes refused.
    ///
    /// # Errors
    ///
    /// [`DestinationError::Rejected`](crate::replication::DestinationError::Rejected)
    /// when `engine` is not a valid sidecar name. See [`sidecar_root`].
    pub fn sidecar_replica(
        &self,
        engine: &str,
    ) -> Result<Option<crate::replication::ScopedDestination>, crate::replication::DestinationError>
    {
        check_sidecar_name(engine)?;
        let Some(replica) = &self.replica else {
            return Ok(None);
        };
        // Each write checks the flag and the epoch: a fence that has not
        // reached this database yet still stops its sidecar.
        let retired = Arc::clone(&self.retired);
        let fence = replica.ownership.clone().zip(self.epoch);
        let slot = self.key.routing_slot();
        let stop = move || {
            retired.load(Ordering::Acquire)
                || fence
                    .as_ref()
                    .is_some_and(|(ownership, epoch)| ownership.epoch_for(slot) != Some(*epoch))
        };
        crate::replication::ScopedDestination::new(
            Arc::new(crate::replication::FencedDestination::new(
                Arc::clone(&replica.destination),
                replica.fence(&self.key, self.epoch),
            )),
            sidecar_root(&replica.root, engine)?,
        )
        .map(|scope| Some(scope.stopped_when(Arc::new(stop))))
    }

    /// A lease that keeps this database open while it is held. See
    /// [`ShardLease`](crate::sharding::ShardLease).
    #[must_use]
    pub fn lease(&self) -> crate::sharding::ShardLease {
        Arc::clone(&self.lease) as crate::sharding::ShardLease
    }

    /// Cancelled when the fleet begins to close this database. Background
    /// work tied to one database (its commit-hook worker) stops on it.
    #[must_use]
    pub const fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    /// Register background work that uses this database. Before the database
    /// closes, the fleet cancels `stop` and awaits `handle` (for at most
    /// [`WORKER_QUIESCE_TIMEOUT`]), so the work can finish its current step
    /// while the database still serves it.
    ///
    /// Keep `stop` apart from [`closed`](Self::closed): that token means the
    /// database is closing.
    pub fn register_worker(&self, stop: CancellationToken, handle: tokio::task::JoinHandle<()>) {
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(workers) = workers.as_mut() {
            workers.push(Worker { stop, handle });
        } else {
            // Too late to settle: the database is about to close.
            stop.cancel();
            handle.abort();
        }
    }

    /// Stop the registered workers. With `settle`, wait for them within the
    /// timeout; without it, abort them now.
    async fn quiesce_workers(&self, settle: bool) {
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        for worker in &workers {
            worker.stop.cancel();
        }
        if !settle {
            for worker in &workers {
                worker.handle.abort();
            }
            return;
        }
        let key = &self.key;
        let aborts: Vec<_> = workers.iter().map(|w| w.handle.abort_handle()).collect();
        let settle = futures::future::join_all(workers.into_iter().map(|w| w.handle));
        if tokio::time::timeout(WORKER_QUIESCE_TIMEOUT, settle)
            .await
            .is_err()
        {
            tracing::warn!(database = %key, "a fleet worker did not stop in time; aborting it");
            // A detached task would ack against a closed database.
            for abort in aborts {
                abort.abort();
            }
        }
    }
}

impl<E: FleetEngine> fmt::Debug for FleetDatabase<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FleetDatabase")
            .field("key", &self.key)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

default_engine! {
/// Work that follows a fleet database's lifecycle. Installed with
/// [`DatabaseFleet::add_lifecycle`].
pub trait FleetLifecycle<E: FleetEngine>: Send + Sync + 'static {
    /// Before opening a database whose file is missing. Return `Ok(true)`
    /// when the hook produced the file (a restore from a replica); the open
    /// then treats it as an existing database. Runs on a blocking thread. An
    /// error refuses the open.
    ///
    /// The fleet holds the path lock while the hook runs, and has cleared the
    /// stray sidecars of the absent database first. Other keys that share the
    /// lock file wait, so do the slow work (a download) only as needed.
    ///
    /// Each request-path open of a missing file asks this hook, whatever
    /// `[database.fleet] restore_missing` says (that setting controls only
    /// the restore from the replica). A background open does not ask. With
    /// replication, an open of a deleted database does not ask either. To
    /// restore a local backup, use `DatabaseFleet::restore_from_file`.
    ///
    /// # Errors
    ///
    /// A message that becomes [`FleetError::Restore`].
    fn restore_missing(&self, key: &FleetDbKey, path: &Path) -> Result<bool, String> {
        let _ = (key, path);
        Ok(false)
    }

    /// After the engine opened the database, before its first use. Runs on
    /// a blocking thread. An error refuses the open.
    ///
    /// # Errors
    ///
    /// A message that becomes [`FleetError::Unavailable`].
    fn on_open(&self, db: &FleetDatabase<E>) -> Result<(), String> {
        let _ = db;
        Ok(())
    }

    /// After the database is closed and its last use ended. Runs on a
    /// blocking thread. A replicator ships its last frames here.
    fn on_close(&self, db: &FleetDatabase<E>) {
        let _ = db;
    }

    /// Before a delete or a restore touches the closed database's files.
    /// Let go of anything that holds them open (a parked replicator does).
    fn release(&self, key: &FleetDbKey) {
        let _ = key;
    }

    /// After a delete has removed the database's files. Not called when the
    /// delete was refused or found nothing to delete.
    fn on_delete(&self, key: &FleetDbKey) {
        let _ = key;
    }
}
}

/// Counters for health and metrics. See [`DatabaseFleet::stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct FleetStats {
    /// Databases open now.
    pub open: usize,
    /// Databases being closed now.
    pub draining: usize,
    /// The configured cap.
    pub max_open: usize,
    /// The memory the open databases hold, by the engine's count.
    pub resident_bytes: u64,
    /// The configured memory budget. `0`: no budget.
    pub max_resident_bytes: u64,
    /// Databases opened since boot.
    pub opens_total: u64,
    /// Databases closed since boot (LRU, idle, delete).
    pub closes_total: u64,
    /// Opens that failed since boot.
    pub open_failures_total: u64,
    /// Migrations applied by this process since boot.
    pub migrations_applied_total: u64,
    /// Databases created since boot.
    pub created_total: u64,
    /// Databases deleted since boot.
    pub deleted_total: u64,
    /// How the databases get to object storage: `continuous`,
    /// `generations` or `rebuild`.
    pub ship_mode: &'static str,
    /// The RPO in seconds: the sync interval of `continuous`, the interval
    /// of `generations`. `None` for `rebuild`, or with no replica.
    pub rpo_secs: Option<u64>,
}

/// The result of [`DatabaseFleet::ship_now`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct ShipReport {
    /// The id of the new generation in `generations` mode. `None` when
    /// nothing changed, or in `continuous` mode.
    pub generation: Option<String>,
    /// Objects with new data: files of a generation, or WAL segments and a
    /// base snapshot.
    pub shipped: usize,
    /// Files whose hash the previous generation holds. They are not
    /// shipped again.
    pub reused: usize,
    /// Bytes shipped.
    pub bytes: u64,
}

// ── The fleet ────────────────────────────────────────────────────────────────

/// One entry of the open map. `cell` is filled once by the entry's single
/// owned opener task, which sets `finished` when it ends.
struct Entry<E: FleetEngine> {
    cell: OnceCell<FleetDatabase<E>>,
    finished: AtomicBool,
    done: Notify,
    last_use_seq: AtomicU64,
    last_use_at: Mutex<std::time::Instant>,
}

/// The use mark of an entry only background opens used. Foreground uses
/// count from 1, so such an entry is the first LRU victim.
const BACKGROUND_SEQ: u64 = 0;

default_engine! {
/// A database [`DatabaseFleet::open_background`] returned.
#[derive(Debug)]
pub struct BackgroundOpen<E: FleetEngine> {
    /// The database. Holding it is a lease.
    pub db: FleetDatabase<E>,
    /// Whether this call opened it (it was not open before).
    pub opened: bool,
}
}

/// A database leaving the open map: its key, its handle, its drain marker.
type Draining<E> = (FleetDbKey, Option<FleetDatabase<E>>, Arc<Drain>);

/// A key taken out of service: its open entry, if any, and its drain marker.
type Exclusive<E> = (FleetDbKey, Option<Arc<Entry<E>>>, Arc<Drain>);

/// What an opener does next: wait for a drain, or open through an entry.
enum Next<E: FleetEngine> {
    Wait(Arc<Drain>),
    /// A background open found the fleet full.
    NoRoom,
    /// The entry, and whether this caller made it and so must start its opener.
    Open(Arc<Entry<E>>, bool),
}

/// Counts an owned mutation until it ends, so `close_all` can wait for it.
/// Made before the task is spawned: no window exists where it is unseen.
struct MutationGuard<E: FleetEngine>(DatabaseFleet<E>);

impl<E: FleetEngine> MutationGuard<E> {
    fn new(fleet: &DatabaseFleet<E>) -> Self {
        fleet.inner.mutations.fetch_add(1, Ordering::AcqRel);
        Self(fleet.clone())
    }
}

impl<E: FleetEngine> Drop for MutationGuard<E> {
    fn drop(&mut self) {
        self.0.inner.mutations.fetch_sub(1, Ordering::AcqRel);
        self.0.inner.mutations_done.notify_waiters();
    }
}

/// Ends an open: forgets the entry if it holds no database, then wakes the
/// waiters. A dropped guard (a panic) does the same.
struct OpenGuard<E: FleetEngine> {
    fleet: DatabaseFleet<E>,
    key: FleetDbKey,
    entry: Arc<Entry<E>>,
}

impl<E: FleetEngine> Drop for OpenGuard<E> {
    fn drop(&mut self) {
        if self.entry.cell.get().is_none() {
            self.fleet.forget_entry(&self.key, &self.entry);
        }
        self.entry.finished.store(true, Ordering::Release);
        self.entry.done.notify_waiters();
    }
}

/// Finishes a drain when dropped, so a panic cannot strand the key.
struct DrainGuard<'a, E: FleetEngine> {
    fleet: &'a DatabaseFleet<E>,
    key: FleetDbKey,
    drain: Arc<Drain>,
}

impl<E: FleetEngine> Drop for DrainGuard<'_, E> {
    fn drop(&mut self) {
        self.fleet.finish_drain(&self.key, &self.drain);
    }
}

/// A key that is closing or being deleted. Openers wait on `done`.
struct Drain {
    deleting: bool,
    finished: AtomicBool,
    done: Notify,
    /// The retire flag of the database and the epoch it opened under, so a
    /// fence can retire a database that is still draining.
    retire: Option<(Arc<AtomicBool>, Option<crate::lock::FencingToken>)>,
}

impl Drain {
    async fn wait(&self) {
        loop {
            let notified = self.done.notified();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

struct State<E: FleetEngine> {
    open: HashMap<FleetDbKey, Arc<Entry<E>>>,
    draining: HashMap<FleetDbKey, Arc<Drain>>,
    /// Serializes the in-process provisions of one key.
    provisioning: HashMap<FleetDbKey, Arc<tokio::sync::Mutex<()>>>,
    /// Keys a provision created but no caller has seen succeed yet.
    unacknowledged: HashSet<FleetDbKey>,
    seq: u64,
    /// Set by `close_all`. No entry is claimed after it.
    shutting_down: bool,
}

impl<E: FleetEngine> Default for State<E> {
    fn default() -> Self {
        Self {
            open: HashMap::new(),
            draining: HashMap::new(),
            provisioning: HashMap::new(),
            unacknowledged: HashSet::new(),
            seq: 0,
            shutting_down: false,
        }
    }
}

#[derive(Default)]
struct Counters {
    opens: AtomicU64,
    closes: AtomicU64,
    open_failures: AtomicU64,
    created: AtomicU64,
    deleted: AtomicU64,
}

struct Inner<E: FleetEngine> {
    engine: E,
    root: PathBuf,
    template: FleetPathTemplate,
    max_open: usize,
    /// The memory budget in bytes. `0`: no budget.
    max_resident_bytes: u64,
    create_on_demand: bool,
    idle_close: Option<Duration>,
    min_residency: Duration,
    #[cfg(feature = "sqlite")]
    replication: std::sync::OnceLock<Arc<super::fleet_replication::FleetReplication>>,
    ownership: std::sync::OnceLock<Arc<super::fleet_ownership::FleetOwnership>>,
    /// The shared volume lock held while ownership is installed.
    volume_lock: Mutex<Option<VolumeLock>>,
    state: Mutex<State<E>>,
    lifecycle: std::sync::RwLock<Vec<Arc<dyn FleetLifecycle<E>>>>,
    counters: Counters,
    in_flight_drains: AtomicUsize,
    /// Owned `delete`, `restore` and `provision` tasks that have not ended.
    mutations: AtomicUsize,
    mutations_done: Notify,
}

default_engine! {
/// A fleet of databases of one engine. Cheap to clone. See the module docs.
pub struct DatabaseFleet<E: FleetEngine> {
    inner: Arc<Inner<E>>,
}
}

impl<E: FleetEngine> Clone for DatabaseFleet<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E: FleetEngine> fmt::Debug for DatabaseFleet<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseFleet")
            .field("mode", &self.mode())
            .field("root", &self.inner.root)
            .field("path", &self.inner.template.as_str())
            .finish_non_exhaustive()
    }
}

impl<E: FleetEngine> DatabaseFleet<E> {
    /// A fleet of `engine` from its configuration section. Creates the root.
    /// The `SQLite` fleet uses `DatabaseFleet::builder` instead, which also
    /// takes the migration sets.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] for a bad template, [`FleetError::Io`] when
    /// the root cannot be created.
    pub fn with_engine(config: &DatabaseFleetConfig, engine: E) -> Result<Self, FleetError> {
        Self::from_config(config, |_| engine)
    }

    /// Build the fleet. `engine` gets the canonical root.
    pub(crate) fn from_config(
        config: &DatabaseFleetConfig,
        engine: impl FnOnce(&Path) -> E,
    ) -> Result<Self, FleetError> {
        let template = FleetPathTemplate::parse(config.path_template(), config.mode)?;
        std::fs::create_dir_all(&config.root).map_err(io_error("create root directory"))?;
        // Canonical, so the containment check in `path_of` compares like with
        // like and a relative root does not move with the working directory.
        let root = std::fs::canonicalize(&config.root).map_err(io_error("resolve root"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                engine: engine(&root),
                root,
                template,
                max_open: config.max_open.max(1),
                max_resident_bytes: config.max_resident_bytes,
                create_on_demand: config.effective_create_on_demand(),
                idle_close: (config.idle_close_secs > 0)
                    .then(|| Duration::from_secs(config.idle_close_secs)),
                min_residency: config.min_residency,
                #[cfg(feature = "sqlite")]
                replication: std::sync::OnceLock::new(),
                ownership: std::sync::OnceLock::new(),
                volume_lock: Mutex::new(None),
                state: Mutex::new(State::default()),
                lifecycle: std::sync::RwLock::new(Vec::new()),
                counters: Counters::default(),
                in_flight_drains: AtomicUsize::new(0),
                mutations: AtomicUsize::new(0),
                mutations_done: Notify::new(),
            }),
        })
    }

    /// The engine of this fleet.
    #[must_use]
    pub fn engine(&self) -> &E {
        &self.inner.engine
    }

    /// What one database holds.
    #[must_use]
    pub fn mode(&self) -> FleetMode {
        self.inner.template.mode()
    }

    /// The canonical root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The configured soft cap on open databases. See
    /// [`DatabaseFleetConfig::max_open`].
    #[must_use]
    pub fn max_open(&self) -> usize {
        self.inner.max_open
    }

    /// How long a database stays open after its last use before LRU may
    /// close it.
    #[must_use]
    pub fn min_residency(&self) -> Duration {
        self.inner.min_residency
    }

    /// The memory budget in bytes. `0`: no budget. See
    /// [`DatabaseFleetConfig::max_resident_bytes`].
    #[must_use]
    pub fn max_resident_bytes(&self) -> u64 {
        self.inner.max_resident_bytes
    }

    /// The memory the open databases hold now, by the engine's count.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.resident_locked(&self.lock_state())
    }

    fn resident_locked(&self, state: &State<E>) -> u64 {
        state
            .open
            .values()
            .filter_map(|entry| entry.cell.get())
            .map(|db| self.inner.engine.resident_bytes(&db.handle))
            .fold(0, u64::saturating_add)
    }

    /// Install lifecycle hooks. Install before the first open: a database
    /// already open does not see `on_open` again.
    pub fn add_lifecycle(&self, hooks: Arc<dyn FleetLifecycle<E>>) {
        self.inner
            .lifecycle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(hooks);
    }

    fn hooks(&self) -> Vec<Arc<dyn FleetLifecycle<E>>> {
        self.inner
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Serve only the slots `ownership` holds (issue #3233). Install before
    /// the first open, once, and before the first
    /// [`refresh`](super::fleet_ownership::FleetOwnership::refresh):
    /// [`start_ownership`](Self::start_ownership) does both in that order.
    ///
    /// The process first takes a shared OS lock on the root, so that
    /// [`release_ownership`](Self::release_ownership) knows when it is the
    /// last process on the volume. Without it, the process it replaces could
    /// see itself as the last one and release the epoch this one serves.
    ///
    /// # Errors
    ///
    /// [`FleetError::Io`] when the volume lock cannot be taken, or when
    /// ownership is already installed.
    pub fn install_ownership(
        &self,
        ownership: Arc<super::fleet_ownership::FleetOwnership>,
    ) -> Result<(), FleetError> {
        let installed = || FleetError::Io {
            op: "install fleet ownership",
            detail: "the fleet already has ownership".to_owned(),
        };
        if self.inner.ownership.get().is_some() {
            return Err(installed());
        }
        let lock = VolumeLock::shared(self.root()).map_err(|error| FleetError::Io {
            op: "lock the fleet volume",
            detail: error.to_string(),
        })?;
        self.inner
            .ownership
            .set(ownership)
            .map_err(|_| installed())?;
        *self
            .inner
            .volume_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lock);
        self.wire_fence_hook();
        Ok(())
    }

    /// Install `ownership` and make its first claim. Run it before the first
    /// open.
    ///
    /// The install takes the volume lock before the claim. Otherwise an old
    /// process that stops meanwhile sees itself as the last one and releases
    /// the epoch this process just claimed.
    ///
    /// # Errors
    ///
    /// As [`install_ownership`](Self::install_ownership); nothing is claimed
    /// then.
    pub async fn start_ownership(
        &self,
        ownership: Arc<super::fleet_ownership::FleetOwnership>,
    ) -> Result<super::fleet_ownership::RefreshReport, FleetError> {
        self.install_ownership(Arc::clone(&ownership))?;
        Ok(ownership.refresh().await)
    }

    /// Release this host's ranges at shutdown, after the databases closed,
    /// so the next owner can claim them at once. Returns whether it released.
    ///
    /// It releases only when this is the last process on the volume (the
    /// processes share the owner id and the epoch) and nothing committed is
    /// left unshipped. Otherwise the leases lapse. They also lapse when the
    /// store does not answer within a renew interval, or when a store call
    /// outlived the loop's shutdown wait: it may still extend a lease.
    pub async fn release_ownership(&self) -> bool {
        let Some(ownership) = self.ownership() else {
            return false;
        };
        if self.unshipped() {
            tracing::warn!("fleet ownership: unshipped frames; the ranges lapse instead");
            return false;
        }
        if ownership.unsettled() {
            tracing::warn!(
                "fleet ownership: a store call is still running; the ranges lapse instead"
            );
            return false;
        }
        let shared = self
            .inner
            .volume_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if shared.is_none() {
            return false;
        }
        drop(shared);
        let root = self.root().to_path_buf();
        let last = crate::time::spawn_blocking(move || VolumeLock::exclusive(&root))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        let Some(_last) = last else {
            tracing::info!("fleet ownership: another process on this volume serves; not released");
            return false;
        };
        if tokio::time::timeout(ownership.renew_interval(), ownership.release_all())
            .await
            .is_err()
        {
            tracing::warn!("fleet ownership: the store did not answer; the ranges lapse instead");
            return false;
        }
        true
    }

    /// The epoch to open `key` under: `None` without ownership.
    fn owned_epoch(
        &self,
        key: &FleetDbKey,
    ) -> Result<Option<crate::lock::FencingToken>, FleetError> {
        let Some(ownership) = self.ownership() else {
            return Ok(None);
        };
        let slot = key.routing_slot();
        ownership
            .epoch_for(slot)
            .map(Some)
            .ok_or_else(|| FleetError::NotOwner {
                name: key.name(),
                slot,
            })
    }

    /// The installed ownership, if any.
    #[must_use]
    pub fn ownership(&self) -> Option<&Arc<super::fleet_ownership::FleetOwnership>> {
        self.inner.ownership.get()
    }

    /// Whether this host may serve `key` now. Always `true` without
    /// ownership.
    #[must_use]
    pub fn owns(&self, key: &FleetDbKey) -> bool {
        self.owned_epoch(key).is_ok()
    }

    /// Begin to close each open database whose slot this host no longer
    /// holds at the epoch it opened under. Returns how many began closing.
    #[must_use = "the count is how many databases began closing"]
    pub fn fence_unowned(&self) -> usize {
        let Some(ownership) = self.ownership() else {
            return 0;
        };
        let victims = self.begin_drains(|state| {
            // A database that already drains (a close waits for its users)
            // is not open, but its sidecar scopes must stop too.
            for (key, drain) in &state.draining {
                if let Some((retired, epoch)) = &drain.retire
                    && ownership.epoch_for(key.routing_slot()) != *epoch
                {
                    retired.store(true, Ordering::Release);
                }
            }
            state
                .open
                .iter()
                .filter(|(key, entry)| {
                    entry
                        .cell
                        .get()
                        .is_some_and(|db| ownership.epoch_for(key.routing_slot()) != db.epoch)
                })
                .map(|(key, _)| key.clone())
                .collect()
        });
        let fenced = victims.len();
        for (key, db, drain) in victims {
            tracing::warn!(database = %key, "fleet database closed: this host lost its slot");
            // No settle: the data is not this host's any more.
            self.spawn_drain(key, db, drain, false);
        }
        fenced
    }

    /// Renew this host's ranges and close what it lost, until `shutdown`
    /// (issue #3233). It wakes every renew interval, when a local lease runs
    /// out, and when a replicator sees a newer epoch. A no-op without
    /// ownership.
    #[must_use = "await the handle at shutdown"]
    pub fn spawn_ownership(&self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let fleet = self.clone();
        tokio::spawn(async move {
            let Some(ownership) = fleet.ownership().cloned() else {
                return;
            };
            loop {
                let changed = ownership.changed().notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let started = tokio::time::Instant::now();
                // Close a lapsed range first: the store may hang (a real
                // partition), and the local lease must still end on time.
                let _ = fleet.fence_unowned();
                let refresh = ownership.refresh();
                tokio::pin!(refresh);
                let mut stopping = false;
                loop {
                    let lapse = ownership.next_lapse();
                    tokio::select! {
                        _ = &mut refresh => break,
                        () = sleep_until_some(lapse) => {
                            let _ = fleet.fence_unowned();
                        }
                        () = shutdown.cancelled() => {
                            stopping = true;
                            break;
                        }
                    }
                }
                if stopping {
                    // A store may finish a call it was sent even when the
                    // call is dropped, and a renewal that lands after the
                    // release extends the lease again. So let it finish
                    // first, within a renew interval: a hung store must not
                    // hold shutdown.
                    if tokio::time::timeout(ownership.renew_interval(), &mut refresh)
                        .await
                        .is_err()
                    {
                        ownership.mark_unsettled();
                    }
                    return;
                }
                let _ = fleet.fence_unowned();
                let wake = ownership.next_wake_since(started);
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep_until(wake) => {}
                    () = &mut changed => {}
                }
            }
        })
    }

    // ── Keys ─────────────────────────────────────────────────────────────

    /// The database key for a routing key (a tenant id), per the fleet mode.
    ///
    /// A slot fleet takes any routing key, because the key never names a
    /// file. This function does not check the id. The app must check it.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] when a tenant fleet is given an id that
    /// cannot name a file.
    pub fn key_for(&self, routing_key: &str) -> Result<FleetDbKey, FleetError> {
        let slot = crate::sharding::slot_for_key(routing_key.into()).0;
        Ok(match self.mode() {
            FleetMode::Tenant => FleetDbKey::Tenant {
                id: TenantDbId::parse(routing_key)?,
                slot,
            },
            FleetMode::Slot => FleetDbKey::Slot(slot),
        })
    }

    /// The database key for a [`ShardKey`](crate::sharding::ShardKey).
    ///
    /// A slot fleet hashes the key exactly as configured shards do. A tenant
    /// fleet needs a textual id: an integer key is its decimal form and a
    /// byte key (a UUID) its lowercase hex.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] when the id cannot name a file.
    pub fn key_for_shard_key(
        &self,
        key: crate::sharding::ShardKey<'_>,
    ) -> Result<FleetDbKey, FleetError> {
        use crate::sharding::ShardKey;
        let slot = crate::sharding::slot_for_key(key).0;
        Ok(match self.mode() {
            FleetMode::Slot => FleetDbKey::Slot(slot),
            FleetMode::Tenant => {
                let id = match key {
                    ShardKey::Str(id) => TenantDbId::parse(id)?,
                    ShardKey::Int(id) => TenantDbId::parse(&id.to_string())?,
                    ShardKey::Bytes(bytes) => {
                        use std::fmt::Write as _;
                        let mut hex = String::with_capacity(bytes.len() * 2);
                        for byte in bytes {
                            let _ = write!(hex, "{byte:02x}");
                        }
                        TenantDbId::parse(&hex)?
                    }
                };
                // The tenant's slot is the hash of its id string, which is
                // what `key_for` and the on-disk layout use.
                let slot = crate::sharding::slot_for_key(id.as_str().into()).0;
                FleetDbKey::Tenant { id, slot }
            }
        })
    }

    /// The key for a name produced by [`FleetDbKey::name`].
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] for a malformed name or one of the other mode.
    pub fn key_for_name(&self, name: &str) -> Result<FleetDbKey, FleetError> {
        let key = match crate::fleet_layout::parse_db_name(name)? {
            crate::fleet_layout::ParsedDbName::Tenant(id) => {
                let slot = crate::sharding::slot_for_key(id.as_str().into()).0;
                FleetDbKey::Tenant { id, slot }
            }
            crate::fleet_layout::ParsedDbName::Slot(slot) => FleetDbKey::Slot(slot),
        };
        if key.mode() != self.mode() {
            return Err(FleetLayoutError::InvalidName(name.to_owned()).into());
        }
        Ok(key)
    }

    /// Refuse a key this fleet cannot hold: another mode's, a slot out of
    /// range, or a tenant key whose slot is not its id's (all constructible
    /// by hand, since `FleetDbKey`'s variants are public).
    fn check_key(&self, key: &FleetDbKey) -> Result<(), FleetError> {
        let consistent = match key {
            FleetDbKey::Slot(slot) => *slot < crate::config::SLOT_COUNT,
            FleetDbKey::Tenant { id, slot } => {
                *slot == crate::sharding::slot_for_key(id.as_str().into()).0
            }
        };
        if key.mode() != self.mode() || !consistent {
            return Err(FleetLayoutError::InvalidName(key.name()).into());
        }
        Ok(())
    }

    /// The file of `key`, under the root.
    #[must_use]
    pub fn path_of(&self, key: &FleetDbKey) -> PathBuf {
        self.inner.root.join(self.inner.template.render(key))
    }

    // ── Open ─────────────────────────────────────────────────────────────

    /// Open the database for a routing key (see [`key_for`](Self::key_for)).
    ///
    /// # Errors
    ///
    /// See [`open`](Self::open).
    pub async fn open_for(&self, routing_key: &str) -> Result<FleetDatabase<E>, FleetError> {
        let key = self.key_for(routing_key)?;
        self.open(&key).await
    }

    /// Open a database, creating it when the fleet creates on demand.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotFound`], [`FleetError::Deleting`],
    /// [`FleetError::PendingMigrations`], [`FleetError::Misplaced`],
    /// [`FleetError::NewerEpoch`], or an engine or I/O failure.
    pub async fn open(&self, key: &FleetDbKey) -> Result<FleetDatabase<E>, FleetError> {
        self.open_with(key, self.inner.create_on_demand).await
    }

    /// Run `work` while no other provision or delete of `key` runs its file
    /// step. A delete then cannot clear the marker of a creation that
    /// follows its removal.
    async fn with_provision_turn<T>(&self, key: &FleetDbKey, work: impl Future<Output = T>) -> T {
        let lock = Arc::clone(
            self.lock_state()
                .provisioning
                .entry(key.clone())
                .or_default(),
        );
        let result = {
            let _turn = lock.lock().await;
            work.await
        };
        drop(lock);
        let mut state = self.lock_state();
        if state
            .provisioning
            .get(key)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            state.provisioning.remove(key);
        }
        drop(state);
        result
    }

    /// Whether the database is on disk, by the engine's rule.
    #[must_use]
    pub fn exists(&self, key: &FleetDbKey) -> bool {
        self.inner.engine.exists(&self.path_of(key))
    }

    async fn open_with(
        &self,
        key: &FleetDbKey,
        create: bool,
    ) -> Result<FleetDatabase<E>, FleetError> {
        // Only a background claim reports no room.
        self.open_entry(key, create, false)
            .await?
            .map(|opened| opened.db)
            .ok_or_else(|| FleetError::Unavailable {
                name: key.name(),
                detail: "no room".to_owned(),
            })
    }

    /// Open `key` for background work (ADR 0019 §6). It never creates a
    /// database, never restores a missing one from its replica, and never
    /// makes the fleet hold more than `max_open`: with no room it returns
    /// `Ok(None)`. It does not count as a use, so the
    /// database stays the first LRU victim, and
    /// [`close_if_unused`](Self::close_if_unused) closes it again when no
    /// request used it meanwhile.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotFound`] when the file is missing (and no restore
    /// produced it), or another [`open`](Self::open) error.
    pub async fn open_background(
        &self,
        key: &FleetDbKey,
    ) -> Result<Option<BackgroundOpen<E>>, FleetError> {
        self.open_entry(key, false, true).await
    }

    /// Close `key` if a background open opened it, nothing else used it since,
    /// and nobody holds it (no lease, no checked-out connection). Returns
    /// whether the close began.
    #[must_use = "whether the close began"]
    pub fn close_if_unused(&self, key: &FleetDbKey) -> bool {
        let begun = self.begin_drains(|state| {
            state
                .open
                .get(key)
                .filter(|entry| {
                    entry.last_use_seq.load(Ordering::Relaxed) == BACKGROUND_SEQ
                        && entry.cell.get().is_some_and(|db| self.is_idle(db))
                })
                .map(|_| vec![key.clone()])
                .unwrap_or_default()
        });
        let closing = !begun.is_empty();
        for (key, db, drain) in begun {
            self.spawn_drain(key, db, drain, true);
        }
        closing
    }

    /// Open through the key's entry. A `background` open is refused for room
    /// (`Ok(None)`) and does not mark the entry used.
    async fn open_entry(
        &self,
        key: &FleetDbKey,
        create: bool,
        background: bool,
    ) -> Result<Option<BackgroundOpen<E>>, FleetError> {
        self.check_key(key)?;
        loop {
            // Before anything touches the disk: `restore_missing` runs only
            // for a key this host owns (issue #3233).
            let epoch = self.owned_epoch(key)?;
            let next = self.claim_entry(key, background)?;
            let (entry, elected) = match next {
                Next::Wait(drain) => {
                    drain.wait().await;
                    continue;
                }
                Next::NoRoom => return Ok(None),
                Next::Open(entry, elected) => (entry, elected),
            };
            // No await between the claim and the spawn, so a cancelled caller
            // cannot leave an entry without an opener.
            let opener =
                elected.then(|| self.spawn_opener(key, create, !background, epoch, &entry));
            let result = match (entry.cell.get(), opener) {
                (Some(db), _) => Ok(db.clone()),
                (None, Some(opener)) => opener.await.unwrap_or_else(|error| {
                    Err(FleetError::Unavailable {
                        name: key.name(),
                        detail: format!("open task failed: {error}"),
                    })
                }),
                (None, None) => {
                    entry.wait().await;
                    match entry.cell.get() {
                        Some(db) => Ok(db.clone()),
                        // The opener failed and its entry is gone: open afresh.
                        None => continue,
                    }
                }
            };
            match result {
                Ok(db) => {
                    // The database was drained between our lookup and now;
                    // go round and open it fresh.
                    if db.closed.is_cancelled() {
                        continue;
                    }
                    // Read the epoch again: the open can take long (a
                    // restore), and the range can be lost meanwhile. Never
                    // serve a database under an epoch this host does not hold
                    // now. The fence closes it; it is not awaited here, as
                    // the caller may hold one of its connections.
                    match self.owned_epoch(key) {
                        Ok(now) if now == db.epoch => {}
                        Ok(_) => {
                            let _ = self.fence_unowned();
                            return Err(FleetError::Unavailable {
                                name: key.name(),
                                detail: "the database reopens under a new ownership epoch; retry"
                                    .to_owned(),
                            });
                        }
                        Err(error) => {
                            let _ = self.fence_unowned();
                            return Err(error);
                        }
                    }
                    if !background {
                        // The claim stamped the entry before the file was
                        // created and migrated. A slow open must not count as
                        // idle time.
                        entry.mark_used_now();
                        self.enforce_capacity(Some(key));
                    }
                    return Ok(Some(BackgroundOpen {
                        db,
                        opened: elected,
                    }));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Start the entry's one opener as an owned task. A cancelled caller only
    /// stops waiting: the open (file, migrations, `on_open` hooks) still ends
    /// in the entry, where a later open, close or delete finds it.
    fn spawn_opener(
        &self,
        key: &FleetDbKey,
        create: bool,
        restore: bool,
        epoch: Option<crate::lock::FencingToken>,
        entry: &Arc<Entry<E>>,
    ) -> tokio::task::JoinHandle<Result<FleetDatabase<E>, FleetError>> {
        let guard = OpenGuard {
            fleet: self.clone(),
            key: key.clone(),
            entry: Arc::clone(entry),
        };
        tokio::spawn(async move {
            match guard
                .fleet
                .open_now(guard.key.clone(), create, restore, epoch)
                .await
            {
                Ok(db) => {
                    let _ = guard.entry.cell.set(db.clone());
                    // The caller may be gone; the cap is the opener's to keep.
                    guard.fleet.enforce_capacity(Some(&guard.key));
                    Ok(db)
                }
                Err(error) => {
                    let counters = &guard.fleet.inner.counters;
                    counters.open_failures.fetch_add(1, Ordering::Relaxed);
                    Err(error)
                }
            }
        })
    }

    /// Under the lock: wait for a drain of `key`, or take (or make) its entry
    /// and mark it used. A `background` claim marks nothing, and makes no
    /// entry when the fleet already holds `max_open`, or the memory budget.
    fn claim_entry(&self, key: &FleetDbKey, background: bool) -> Result<Next<E>, FleetError> {
        let mut state = self.lock_state();
        if state.shutting_down {
            return Err(FleetError::Unavailable {
                name: key.name(),
                detail: "the fleet is shutting down".to_owned(),
            });
        }
        if let Some(drain) = state.draining.get(key).cloned() {
            drop(state);
            if drain.deleting {
                return Err(FleetError::Deleting { name: key.name() });
            }
            return Ok(Next::Wait(drain));
        }
        if background {
            if let Some(entry) = state.open.get(key) {
                return Ok(Next::Open(Arc::clone(entry), false));
            }
            let budget = self.inner.max_resident_bytes;
            if state.open.len() >= self.inner.max_open
                || (budget > 0 && self.resident_locked(&state) >= budget)
            {
                return Ok(Next::NoRoom);
            }
            let entry = Arc::new(Entry::new(BACKGROUND_SEQ));
            state.open.insert(key.clone(), Arc::clone(&entry));
            return Ok(Next::Open(entry, true));
        }
        state.seq += 1;
        let seq = state.seq;
        let mut elected = false;
        let entry = Arc::clone(state.open.entry(key.clone()).or_insert_with(|| {
            elected = true;
            Arc::new(Entry::new(seq))
        }));
        // Under the lock: `close_if_unused` reads the mark under it too.
        entry.touch(seq);
        drop(state);
        Ok(Next::Open(entry, elected))
    }

    /// Remove `entry` from the open map, if it is still the one there.
    fn forget_entry(&self, key: &FleetDbKey, entry: &Arc<Entry<E>>) {
        let mut state = self.lock_state();
        if state
            .open
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            state.open.remove(key);
        }
    }

    /// `restore`: a missing database may come back from its replica. A
    /// background open passes `false`, so it never brings back a deleted
    /// database.
    async fn open_now(
        &self,
        key: FleetDbKey,
        create: bool,
        restore: bool,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<FleetDatabase<E>, FleetError> {
        let path = self.path_of(&key);
        let name = key.name();
        check_contained(self.root(), &path)?;
        let mut existed = self.inner.engine.exists(&path);
        if !existed && restore {
            existed = self.restore_missing(&key, &path).await?;
        }
        if existed && let Some(epoch) = epoch {
            self.check_epoch_stamp(&key, &path, epoch).await?;
        }
        if !existed {
            if !create {
                return Err(FleetError::NotFound { name });
            }
            self.purge_before_create(&key, false).await?;
        }
        // The engine makes a missing database, stamps the key and the epoch,
        // and refuses a database of another key or a newer epoch.
        let handle = self.inner.engine.open(&key, &path, epoch).await?;
        let replica = self.replica_base(&key).map(Arc::new);
        let db = FleetDatabase {
            key,
            path: Arc::from(path.as_path()),
            handle,
            closed: CancellationToken::new(),
            lease: Arc::new(()),
            epoch,
            workers: Arc::new(Mutex::new(Some(Vec::new()))),
            replica,
            retired: Arc::new(AtomicBool::new(false)),
        };
        // The database is complete here, so a failure below closes the
        // handle and never removes what another opener may be serving.
        if let Err(error) = self.run_on_open(&db).await {
            // The hooks that started have run `on_close`: a scope that one
            // of them kept writes nothing more.
            db.retired.store(true, Ordering::Release);
            db.quiesce_workers(true).await;
            db.closed.cancel();
            let (fleet, closing) = (self.clone(), db.clone());
            let _ = crate::time::spawn_blocking(move || fleet.inner.engine.close(&closing.handle))
                .await;
            return Err(error);
        }
        let counters = &self.inner.counters;
        counters.opens.fetch_add(1, Ordering::Relaxed);
        if !existed {
            counters.created.fetch_add(1, Ordering::Relaxed);
        }
        tracing::debug!(database = %name, created = !existed, "fleet database opened");
        Ok(db)
    }

    /// Run the `on_open` hooks. A hook that fails undoes the hooks that
    /// started before it.
    async fn run_on_open(&self, db: &FleetDatabase<E>) -> Result<(), FleetError> {
        let hooks = self.hooks();
        if hooks.is_empty() {
            return Ok(());
        }
        let name = db.key.name();
        let opened = db.clone();
        crate::time::spawn_blocking(move || {
            let mut started: Vec<&Arc<dyn FleetLifecycle<E>>> = Vec::new();
            for hook in &hooks {
                if let Err(detail) = hook.on_open(&opened) {
                    // Undo the hooks that did start (a replicator that
                    // registered) before refusing the open. The open is
                    // refused: a hook must ship nothing of it.
                    opened.retired.store(true, Ordering::Release);
                    // Stop the workers first, as `drain` does.
                    tokio::runtime::Handle::current().block_on(opened.quiesce_workers(true));
                    for hook in started {
                        hook.on_close(&opened);
                    }
                    return Err(detail);
                }
                started.push(hook);
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r)
        .map_err(|detail| FleetError::Unavailable { name, detail })
    }

    /// Ask the lifecycle hooks to produce a missing file (a restore).
    async fn restore_missing(&self, key: &FleetDbKey, path: &Path) -> Result<bool, FleetError> {
        let hooks = self.hooks();
        if hooks.is_empty() {
            return Ok(false);
        }
        // A deleted database is not restored by an open (issue #3235).
        if self.deleted_in_replica(key).await? {
            tracing::info!(database = %key, "fleet database was deleted: its replica is not restored");
            return Ok(false);
        }
        let (hook_key, hook_path) = (key.clone(), path.to_path_buf());
        let root = self.inner.root.clone();
        let name = key.name();
        let fleet = self.clone();
        crate::time::spawn_blocking(move || {
            // The hook publishes the restored file. Hold the path lock so no
            // other process publishes meanwhile, and clear the stale sidecars
            // of the absent database first: they would replay onto it.
            with_path_lock(&root, &name, || {
                if !fleet.inner.engine.exists(&hook_path) {
                    remove_files(
                        fleet.inner.engine.sidecars(&hook_path),
                        "clear stray sidecars",
                        &name,
                    )?;
                }
                for hook in hooks {
                    let restored =
                        hook.restore_missing(&hook_key, &hook_path)
                            .map_err(|detail| FleetError::Restore {
                                name: name.clone(),
                                detail,
                            })?;
                    if restored {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
        })
        .await
        .map_err(|error| FleetError::Restore {
            name: key.name(),
            detail: error.to_string(),
        })?
    }

    // ── Close ────────────────────────────────────────────────────────────

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State<E>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Close least recently used idle databases until at most `max_open`
    /// are open and they hold at most `max_resident_bytes`. A database in
    /// use is never chosen, so both caps are soft while every open database
    /// is busy.
    fn enforce_capacity(&self, keep: Option<&FleetDbKey>) {
        let max_open = self.inner.max_open;
        let budget = self.inner.max_resident_bytes;
        let min_residency = self.inner.min_residency;
        let now = crate::time::ambient_instant();
        let victims = self.begin_drains(|state| {
            let excess = state.open.len().saturating_sub(max_open);
            let over = if budget == 0 {
                0
            } else {
                self.resident_locked(state).saturating_sub(budget)
            };
            if excess == 0 && over == 0 {
                return Vec::new();
            }
            let mut idle: Vec<(u64, FleetDbKey, u64)> = state
                .open
                .iter()
                .filter(|(key, _)| Some(*key) != keep)
                // The residency grace covers a request between its resolve
                // and its checkout. A background entry has no request.
                .filter(|(_, entry)| {
                    entry.last_use_seq.load(Ordering::Relaxed) == BACKGROUND_SEQ
                        || now.saturating_duration_since(entry.last_used()) >= min_residency
                })
                .filter_map(|(key, entry)| {
                    let db = entry.cell.get()?;
                    self.is_idle(db).then(|| {
                        let bytes = self.inner.engine.resident_bytes(&db.handle);
                        (
                            entry.last_use_seq.load(Ordering::Relaxed),
                            key.clone(),
                            bytes,
                        )
                    })
                })
                .collect();
            idle.sort_unstable();
            pick_victims(idle, excess, over, state.open.len())
        });
        for (key, db, drain) in victims {
            self.spawn_drain(key, db, drain, true);
        }
    }

    /// Close every database idle for longer than `idle_close`, returning how
    /// many began closing.
    ///
    /// Called by the sweeper [`spawn_maintenance`](Self::spawn_maintenance)
    /// starts; public for tests and tools that drive time by hand.
    #[must_use = "the count is how many databases began closing"]
    pub fn close_idle(&self) -> usize {
        let Some(idle_close) = self.inner.idle_close else {
            return 0;
        };
        let now = crate::time::ambient_instant();
        let victims = self.begin_drains(|state| {
            state
                .open
                .iter()
                .filter(|(_, entry)| {
                    now.saturating_duration_since(entry.last_used()) >= idle_close
                        && entry.cell.get().is_some_and(|db| self.is_idle(db))
                })
                .map(|(key, _)| key.clone())
                .collect()
        });
        let closed = victims.len();
        for (key, db, drain) in victims {
            self.spawn_drain(key, db, drain, true);
        }
        closed
    }

    /// Under one lock: pick keys with `pick`, then move each from open to
    /// draining.
    fn begin_drains(&self, pick: impl FnOnce(&State<E>) -> Vec<FleetDbKey>) -> Vec<Draining<E>> {
        let mut state = self.lock_state();
        let keys = pick(&state);
        let draining = keys
            .iter()
            .filter_map(|key| self.begin_drain_locked(&mut state, key, false))
            .collect();
        drop(state);
        draining
    }

    /// Move `key` from open to draining. `None` when it is not open (or is
    /// still opening, which is never interrupted).
    fn begin_drain_locked(
        &self,
        state: &mut State<E>,
        key: &FleetDbKey,
        deleting: bool,
    ) -> Option<Draining<E>> {
        let db = state.open.get(key)?.cell.get()?.clone();
        state.open.remove(key);
        let drain = Arc::new(Drain {
            deleting,
            finished: AtomicBool::new(false),
            done: Notify::new(),
            retire: Some((Arc::clone(&db.retired), db.epoch)),
        });
        state.draining.insert(key.clone(), Arc::clone(&drain));
        self.inner.in_flight_drains.fetch_add(1, Ordering::Relaxed);
        Some((key.clone(), Some(db), drain))
    }

    fn spawn_drain(
        &self,
        key: FleetDbKey,
        db: Option<FleetDatabase<E>>,
        drain: Arc<Drain>,
        settle: bool,
    ) {
        let fleet = self.clone();
        tokio::spawn(async move {
            let _finish = DrainGuard {
                fleet: &fleet,
                key: key.clone(),
                drain,
            };
            fleet.drain(&key, db, settle).await;
        });
    }

    /// Close one database: stop new use, wait for the current use to end,
    /// then run the `on_close` hooks.
    ///
    /// `settle` lets the database's workers finish their step first, so a
    /// hook is acked. A delete or restore passes `false`: the data goes away,
    /// and the engine must stop stale holders at once.
    async fn drain(&self, key: &FleetDbKey, db: Option<FleetDatabase<E>>, settle: bool) {
        let Some(db) = db else {
            return;
        };
        // A fence, a delete or a restore: the data is not this host's to
        // ship. A sidecar scope refuses writes from now on, before any wait.
        let retired = Arc::clone(&db.retired);
        if !settle {
            retired.store(true, Ordering::Release);
        }
        // Workers settle first: their last ack needs a live database.
        db.quiesce_workers(settle).await;
        db.closed.cancel();
        // A close can do I/O (a `SQLite` connection checkpoints when it
        // drops): keep it off the async workers.
        let (fleet, closing) = (self.clone(), db.clone());
        let _ =
            crate::time::spawn_blocking(move || fleet.inner.engine.close(&closing.handle)).await;
        while self.inner.engine.in_use(&db.handle) {
            tokio::time::sleep(DRAIN_POLL).await;
        }
        let hooks = self.hooks();
        if !hooks.is_empty() {
            let _ = crate::time::spawn_blocking(move || {
                for hook in hooks {
                    hook.on_close(&db);
                }
            })
            .await;
        }
        // Closed: a clone that a sidecar keeps writes nothing more.
        retired.store(true, Ordering::Release);
        self.inner.counters.closes.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(database = %key, "fleet database closed");
    }

    fn finish_drain(&self, key: &FleetDbKey, drain: &Arc<Drain>) {
        {
            let mut state = self.lock_state();
            if state
                .draining
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, drain))
            {
                state.draining.remove(key);
            }
        }
        self.inner.in_flight_drains.fetch_sub(1, Ordering::Relaxed);
        drain.finished.store(true, Ordering::Release);
        drain.done.notify_waiters();
    }

    /// Close one database now (waiting for its in-flight connections). A
    /// no-op when it is not open.
    ///
    /// Cancellation-safe: the work runs in an owned task, so a dropped caller
    /// only stops waiting.
    pub async fn close(&self, key: &FleetDbKey) {
        let (fleet, key) = (self.clone(), key.clone());
        // A task panic has nothing to report to: `close` returns no result.
        let _ = tokio::spawn(async move { fleet.close_owned(&key).await }).await;
    }

    async fn close_owned(&self, key: &FleetDbKey) {
        // A cold open is never interrupted: let it end, then close it.
        self.settle_opening(key).await;
        let begun = self.begin_drains(|_| vec![key.clone()]).pop();
        if let Some((key, db, drain)) = begun {
            let _finish = DrainGuard {
                fleet: self,
                key: key.clone(),
                drain,
            };
            self.drain(&key, db, true).await;
        } else {
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(drain) = pending {
                drain.wait().await;
            }
        }
    }

    /// Set `shutting_down` if no counted work runs. Only then: a provision
    /// counted before this must finish its own open.
    fn stop_admitting_if_idle(&self) -> bool {
        let mut state = self.lock_state();
        let idle = self.inner.mutations.load(Ordering::Acquire) == 0;
        if idle {
            state.shutting_down = true;
        }
        drop(state);
        idle
    }

    /// Count a new `provision`, `delete` or `restore` unless shutdown has
    /// begun. The check and the
    /// count share one lock hold, and `close_all` sets the flag under the same
    /// lock. So a call is either counted before `close_all` looks, or refused.
    fn admit_mutation(&self, key: &FleetDbKey) -> Result<MutationGuard<E>, FleetError> {
        let state = self.lock_state();
        if state.shutting_down {
            return Err(FleetError::Unavailable {
                name: key.name(),
                detail: "the fleet is shutting down".to_owned(),
            });
        }
        let counted = MutationGuard::new(self);
        drop(state);
        Ok(counted)
    }

    /// Close every open database and wait for all drains, for shutdown.
    pub async fn close_all(&self) {
        // Mutations first: one that just started may open or drain a key.
        loop {
            let ended = self.inner.mutations_done.notified();
            // Under the lock `admit_mutation` takes: no call slips in between
            // the count and the flag.
            if self.stop_admitting_if_idle() {
                break;
            }
            ended.await;
        }
        let keys: Vec<FleetDbKey> = self.lock_state().open.keys().cloned().collect();
        // Together, so the worker timeout is paid once, not per database.
        futures::future::join_all(keys.iter().map(|key| self.close(key))).await;
        loop {
            let pending: Vec<Arc<Drain>> = self.lock_state().draining.values().cloned().collect();
            if pending.is_empty() {
                return;
            }
            for drain in pending {
                drain.wait().await;
            }
        }
    }

    /// How often the sweeper runs: often enough to close idle databases on
    /// time, and to retry the `max_open` cap once the residency grace that
    /// blocked it has passed (a burst of opens inside one grace window would
    /// otherwise stay over the cap until the next open).
    fn sweep_interval(&self) -> Duration {
        let idle = self
            .inner
            .idle_close
            .map_or(MAX_SWEEP_INTERVAL, |idle| idle / 4);
        idle.min(self.inner.min_residency.max(Duration::from_millis(100)))
            .clamp(Duration::from_millis(100), MAX_SWEEP_INTERVAL)
    }

    /// Start the sweeper: it closes idle databases and enforces `max_open`
    /// and `max_resident_bytes`.
    /// It stops when `shutdown` is cancelled, after closing every database so
    /// replicators ship their last frames.
    #[must_use = "await the handle at shutdown so every database closes (and ships its last frames)"]
    pub fn spawn_maintenance(&self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let fleet = self.clone();
        let interval = self.sweep_interval();
        // Ends on its own at shutdown; nothing waits for a purge in flight.
        drop(self.spawn_replica_lifecycle(shutdown.clone()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(interval) => {
                        let _ = fleet.close_idle();
                        fleet.enforce_capacity(None);
                    }
                }
            }
            // Real time on purpose: the flush budget covers blocking I/O.
            #[allow(
                clippy::disallowed_methods,
                reason = "real time: the shutdown bound covers blocking I/O"
            )]
            let shutdown_began = std::time::Instant::now();
            // Closing ships each database's last frames. Bound that too.
            fleet.begin_replica_shutdown(shutdown_began);
            fleet.close_all().await;
            fleet.finish_replica_shutdown(shutdown_began).await;
        })
    }

    // ── Delete, backup ──────────────────────────────────────────────────

    /// Delete a database and its sidecar files. Requests for it get `410`
    /// while this runs, and `404` (or a fresh database, when created on
    /// demand) after.
    ///
    /// The call holds the path lock, which every process that shares the
    /// root takes to publish or remove a database at that path. So no other
    /// process publishes a database there while this call runs. One that does
    /// so afterwards keeps its files: this call touches nothing after the
    /// unlink.
    ///
    /// With replication installed, the replica goes too (issue #3235). A
    /// tombstone row is written first: when it cannot be written, the delete
    /// is refused and no file is removed. After the files are gone, the
    /// replica is purged at once, or kept for
    /// `replica_retention_after_delete`. A failed purge does not fail the
    /// delete: the tombstone stays and
    /// `purge_deleted_replicas` retries it.
    ///
    /// Cancellation-safe: the work runs in an owned task, so a dropped caller
    /// only stops waiting.
    ///
    /// # Errors
    ///
    /// [`FleetError::Deleting`] when a delete is already running,
    /// [`FleetError::Unavailable`] once a shutdown has begun, or
    /// [`FleetError::Io`].
    pub async fn delete(&self, key: &FleetDbKey) -> Result<bool, FleetError> {
        self.check_key(key)?;
        let counted = self.admit_mutation(key)?;
        let (fleet, key) = (self.clone(), key.clone());
        tokio::spawn(async move {
            let _counted = counted;
            fleet.delete_owned(&key).await
        })
        .await
        .map_err(|error| FleetError::Io {
            op: "delete database",
            detail: error.to_string(),
        })?
    }

    async fn delete_owned(&self, key: &FleetDbKey) -> Result<bool, FleetError> {
        let begun = self.begin_exclusive(key, true)?;
        let Some((key, entry, drain)) = begun else {
            // A plain close is running; wait, then delete.
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(pending) = pending {
                pending.wait().await;
            }
            return Box::pin(self.delete_owned(key)).await;
        };
        let _finish = DrainGuard {
            fleet: self,
            key: key.clone(),
            drain,
        };
        let db = self.take_database(entry).await;
        self.drain(&key, db, false).await;
        // Hooks let go of the files first (a parked replicator holds one open).
        for hook in self.hooks() {
            hook.release(&key);
        }
        let path = self.path_of(&key);
        let root = self.inner.root.clone();
        let name = key.name();
        let record = self.delete_record(&name);
        let join_error = |e: tokio::task::JoinError| FleetError::Io {
            op: "delete database",
            detail: e.to_string(),
        };
        // `Err`: nothing is recorded or removed. `Ok`: the result of the
        // removal.
        let removed = self
            .with_provision_turn(&key, async {
                let lock = {
                    // Before the lock: the lock then waits for one write
                    // only. The connection stays out of the pool while the
                    // lock is busy (at most the lock timeout). It goes back
                    // at the end of this block.
                    let mut control = record.checkout().await?;
                    // The path lock first: the check, the row and the
                    // removal then see one state. A delete in another
                    // process that comes after the removal sees no
                    // database, and keeps the row of this delete (issue
                    // #3516).
                    let (lock_root, lock_path, lock_name) =
                        (root.clone(), path.clone(), name.clone());
                    let lock = crate::time::spawn_blocking(move || {
                        check_contained(&lock_root, &lock_path)?;
                        PathLock::acquire(&lock_root, &lock_name)
                    })
                    .await
                    .map_err(join_error)
                    .and_then(|r| r)?;
                    record
                        .record(&mut control, self.inner.engine.exists(&path))
                        .await?;
                    lock
                };
                let (fleet, root, path, task_key) =
                    (self.clone(), root.clone(), path.clone(), key.clone());
                let removed = crate::time::spawn_blocking(move || {
                    let _lock = lock;
                    // The engine refuses when another process still serves
                    // the database: removing it under that process would
                    // split the tenant's data.
                    let removed = fleet.inner.engine.remove(&task_key, &path)?;
                    prune_empty_dirs(&path, &root);
                    Ok(removed)
                })
                .await
                .map_err(join_error)
                .and_then(|r| r);
                if removed.is_ok() {
                    self.lock_state().unacknowledged.remove(&key);
                }
                Ok::<_, FleetError>(removed)
            })
            .await?;
        if matches!(removed, Ok(true)) {
            self.inner.counters.deleted.fetch_add(1, Ordering::Relaxed);
            self.purge_after_delete(&key).await;
            // Only now: a refused delete (another process has the file open)
            // or a missing file must not trigger, say, replica cleanup.
            for hook in self.hooks() {
                hook.on_delete(&key);
            }
        } else {
            record.drop_pending(&key).await;
        }
        removed
    }

    /// Wait until no open of `key` is in progress. An open is never
    /// interrupted: this waits for the entry's opener task to end.
    async fn settle_opening(&self, key: &FleetDbKey) {
        loop {
            let opening = self
                .lock_state()
                .open
                .get(key)
                .filter(|entry| entry.cell.get().is_none())
                .cloned();
            let Some(entry) = opening else {
                break;
            };
            entry.wait().await;
        }
    }

    /// Under the lock: take `key` out of service for a delete (openers get
    /// `410`) or a restore (openers wait). The drain goes in first, so no new
    /// entry can appear. The caller settles the returned entry with
    /// [`take_database`](Self::take_database). `Ok(None)` when a plain close
    /// is running: the caller waits for it, then retries.
    fn begin_exclusive(
        &self,
        key: &FleetDbKey,
        deleting: bool,
    ) -> Result<Option<Exclusive<E>>, FleetError> {
        let mut state = self.lock_state();
        if let Some(drain) = state.draining.get(key) {
            return if drain.deleting {
                Err(FleetError::Deleting { name: key.name() })
            } else {
                Ok(None)
            };
        }
        let entry = state.open.remove(key);
        let drain = Arc::new(Drain {
            deleting,
            finished: AtomicBool::new(false),
            done: Notify::new(),
            retire: entry
                .as_ref()
                .and_then(|entry| entry.cell.get())
                .map(|db| (Arc::clone(&db.retired), db.epoch)),
        });
        state.draining.insert(key.clone(), Arc::clone(&drain));
        drop(state);
        self.inner.in_flight_drains.fetch_add(1, Ordering::Relaxed);
        Ok(Some((key.clone(), entry, drain)))
    }

    /// The database behind an entry that `begin_exclusive` took out of the
    /// open map. An open that is still running ends first: it is never
    /// interrupted, and a late opener cannot start one, as the drain is in.
    async fn take_database(&self, entry: Option<Arc<Entry<E>>>) -> Option<FleetDatabase<E>> {
        let entry = entry?;
        entry.wait().await;
        entry.cell.get().cloned()
    }

    // ── Enumeration and fan-out ─────────────────────────────────────────

    /// Every database on disk, sorted by key. A database is a file or a
    /// directory at a template path that the engine takes as a database.
    /// Entries that do not match the template (sidecars, strays, a tenant
    /// file in the wrong bucket) and symbolic links are skipped.
    ///
    /// # Errors
    ///
    /// [`FleetError::Io`] when the root cannot be read.
    pub async fn list(&self) -> Result<Vec<FleetDbKey>, FleetError> {
        let fleet = self.clone();
        crate::time::spawn_blocking(move || fleet.list_blocking())
            .await
            .map_err(|e| FleetError::Io {
                op: "list",
                detail: e.to_string(),
            })?
    }

    fn list_blocking(&self) -> Result<Vec<FleetDbKey>, FleetError> {
        let template = &self.inner.template;
        let slot_of = |id: &str| crate::sharding::slot_for_key(id.into()).0;
        let mut found = Vec::new();
        let mut frontier = vec![(PathBuf::new(), 0_usize)];
        while let Some((relative, depth)) = frontier.pop() {
            let last = depth + 1 == template.depth();
            if let Some(literal) = template.literal_segment(depth) {
                let next = relative.join(literal);
                let meta = std::fs::symlink_metadata(self.inner.root.join(&next));
                match meta {
                    Ok(meta)
                        if last
                            && (meta.is_file() || meta.is_dir())
                            && self.inner.engine.exists(&self.inner.root.join(&next)) =>
                    {
                        if let Some(key) = template.key_for_path(&next, &slot_of) {
                            found.push(key);
                        }
                    }
                    Ok(meta) if !last && meta.is_dir() => frontier.push((next, depth + 1)),
                    _ => {}
                }
                continue;
            }
            let dir = self.inner.root.join(&relative);
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && depth > 0 => continue,
                Err(e) => return Err(io_error("list")(e)),
            };
            for entry in entries {
                let entry = entry.map_err(io_error("list"))?;
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !template.segment_matches(depth, &name) {
                    continue;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let next = relative.join(&name);
                if last
                    && (file_type.is_file() || file_type.is_dir())
                    && self.inner.engine.exists(&self.inner.root.join(&next))
                {
                    if let Some(key) = template.key_for_path(&next, &slot_of) {
                        found.push(key);
                    }
                } else if !last && file_type.is_dir() {
                    frontier.push((next, depth + 1));
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Run `f` on every database on disk, at most `concurrency` at once,
    /// opening each (never creating). Results come back in key order; a
    /// failure on one database does not stop the others.
    ///
    /// This opens every database, which churns the open set past `max_open`;
    /// use it for admin work, not on a hot path. With ownership, it skips the
    /// files of slots this host does not own.
    ///
    /// # Errors
    ///
    /// [`FleetError::Io`] when the fleet cannot be listed.
    pub async fn each<T, Fut, F>(
        &self,
        concurrency: usize,
        f: F,
    ) -> Result<Vec<(FleetDbKey, Result<T, AutumnError>)>, FleetError>
    where
        T: Send,
        Fut: std::future::Future<Output = Result<T, AutumnError>> + Send,
        F: Fn(FleetDatabase<E>) -> Fut + Send + Sync,
    {
        use futures::StreamExt as _;
        // With ownership, only the slots this host serves (issue #3233).
        let keys = self.list().await?.into_iter().filter(|key| self.owns(key));
        let f = &f;
        let results = futures::stream::iter(keys.map(|key| async move {
            let result = match self.open_with(&key, false).await {
                Ok(db) => f(db).await,
                Err(error) => Err(error.into()),
            };
            (key, result)
        }))
        .buffered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await;
        Ok(results)
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> FleetStats {
        let (open, resident_bytes) = {
            let state = self.lock_state();
            (state.open.len(), self.resident_locked(&state))
        };
        let c = &self.inner.counters;
        FleetStats {
            open,
            draining: self.inner.in_flight_drains.load(Ordering::Relaxed),
            max_open: self.inner.max_open,
            resident_bytes,
            max_resident_bytes: self.inner.max_resident_bytes,
            opens_total: c.opens.load(Ordering::Relaxed),
            closes_total: c.closes.load(Ordering::Relaxed),
            open_failures_total: c.open_failures.load(Ordering::Relaxed),
            migrations_applied_total: self.inner.engine.migrations_applied(),
            created_total: c.created.load(Ordering::Relaxed),
            deleted_total: c.deleted.load(Ordering::Relaxed),
            ship_mode: self.inner.engine.ship_mode().name(),
            rpo_secs: self.rpo().map(|rpo| rpo.as_secs()),
        }
    }

    /// Ship the open database of `key` now: one generation in
    /// `generations` mode, one WAL tick in `continuous` mode. Core checks
    /// the fence before each write.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotReplicated`] with no replica, or in `rebuild` mode.
    /// [`FleetError::Fenced`] when a newer epoch owns the replica.
    /// [`FleetError::Unavailable`] when the database is not open in this
    /// process, or the ship failed. The error of [`FleetEngine::snapshot`].
    pub async fn ship_now(&self, key: &FleetDbKey) -> Result<ShipReport, FleetError> {
        self.check_key(key)?;
        self.ship_replica(key).await
    }

    /// Bring the database of `key` back from its newest replica, as an
    /// operator does after a loss. `generations`: the newest generation.
    /// `continuous`: the latest WAL state. `rebuild`: the engine builds it
    /// again ([`FleetEngine::rebuild`]).
    ///
    /// It takes the tombstone row of a delete, so it undoes a delete within
    /// its retention. After the purge nothing is left: it is refused.
    ///
    /// Cancellation-safe: the work runs in an owned task.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotReplicated`] with no replica (and not `rebuild`),
    /// [`FleetError::NotFound`] when nothing was shipped for the key (in
    /// `rebuild` mode: when no database is on disk),
    /// [`FleetError::Deleting`] during a delete, [`FleetError::Unavailable`]
    /// once a shutdown has begun, [`FleetError::InUseElsewhere`] when
    /// another process has the database open, or [`FleetError::Restore`].
    /// In `rebuild` mode, the error of [`FleetEngine::rebuild`].
    pub async fn restore_latest(&self, key: &FleetDbKey) -> Result<(), FleetError> {
        self.restore_to_point(key, None).await.map(drop)
    }

    /// The restore of [`restore_latest`](Self::restore_latest), at or
    /// before `target`. `Some` outcome in `continuous` mode.
    pub(crate) async fn restore_to_point(
        &self,
        key: &FleetDbKey,
        target: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Option<crate::replication::RestoreOutcome>, FleetError> {
        self.check_key(key)?;
        let (fleet, key, name) = (self.clone(), key.clone(), key.name());
        let counted = self.admit_mutation(&key)?;
        tokio::spawn(async move {
            let _counted = counted;
            fleet.restore_owned(&key, target).await
        })
        .await
        .map_err(|error| FleetError::Restore {
            name,
            detail: error.to_string(),
        })?
    }

    async fn restore_owned(
        &self,
        key: &FleetDbKey,
        target: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Option<crate::replication::RestoreOutcome>, FleetError> {
        self.with_exclusive(key, self.replace_from_replica(key, target))
            .await
    }

    /// Run `work` while `key` is out of service for a restore. Openers wait.
    /// It closes the open database first. The hooks release its files. Then
    /// `work` starts.
    async fn with_exclusive<T>(
        &self,
        key: &FleetDbKey,
        work: impl Future<Output = Result<T, FleetError>>,
    ) -> Result<T, FleetError> {
        loop {
            if let Some((key, entry, drain)) = self.begin_exclusive(key, false)? {
                let _finish = DrainGuard {
                    fleet: self,
                    key: key.clone(),
                    drain,
                };
                let db = self.take_database(entry).await;
                self.drain(&key, db, false).await;
                for hook in self.hooks() {
                    hook.release(&key);
                }
                // The guard finishes the drain whatever fails: an error that
                // skipped it would strand every later opener of this key.
                return work.await;
            }
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(pending) = pending {
                pending.wait().await;
            }
        }
    }

    /// Keys of the databases open now, sorted.
    #[must_use]
    pub fn open_keys(&self) -> Vec<FleetDbKey> {
        let mut keys: Vec<FleetDbKey> = self.lock_state().open.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The database of `key` when it is open now. It never opens one, and it
    /// does not count as a use. Holding the result is a lease.
    #[must_use]
    pub fn get_open(&self, key: &FleetDbKey) -> Option<FleetDatabase<E>> {
        let state = self.lock_state();
        let db = state.open.get(key)?.cell.get()?.clone();
        drop(state);
        (!db.closed.is_cancelled()).then_some(db)
    }

    /// Write a consistent copy of the database of `key` into `dir`, as
    /// `dir/<file name of its path>`. Returns the files written. Opens the
    /// database (never creates it) and holds it for the copy.
    ///
    /// # Errors
    ///
    /// An [`open`](Self::open) error, or the engine's copy error.
    pub async fn snapshot(&self, key: &FleetDbKey, dir: &Path) -> Result<Vec<PathBuf>, FleetError> {
        let db = self.open_with(key, false).await?;
        self.inner.engine.snapshot(&db.handle, &db.path, dir).await
    }
}

impl<E: FleetEngine> Entry<E> {
    fn new(seq: u64) -> Self {
        Self {
            cell: OnceCell::new(),
            finished: AtomicBool::new(false),
            done: Notify::new(),
            last_use_seq: AtomicU64::new(seq),
            last_use_at: Mutex::new(crate::time::ambient_instant()),
        }
    }

    /// Wait until the opener task has ended.
    async fn wait(&self) {
        loop {
            let notified = self.done.notified();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn last_used(&self) -> std::time::Instant {
        *self
            .last_use_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mark_used_now(&self) {
        *self
            .last_use_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = crate::time::ambient_instant();
    }

    fn touch(&self, seq: u64) {
        self.last_use_seq.store(seq, Ordering::Relaxed);
        *self
            .last_use_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = crate::time::ambient_instant();
    }
}

impl<E: FleetEngine> DatabaseFleet<E> {
    /// Nobody holds a lease beyond the fleet's own, and the engine reports
    /// no use (no checked-out connection, no waiter).
    fn is_idle(&self, db: &FleetDatabase<E>) -> bool {
        Arc::strong_count(&db.lease) == 1 && !self.inner.engine.in_use(&db.handle)
    }
}

/// The keys to close, in LRU order: enough for `excess` databases and
/// `over` bytes. `idle` is `(last use, key, bytes)`, sorted. `open` is the
/// count of open databases.
///
/// A close for the budget alone must free memory, so it skips a database
/// that holds none. It also keeps one database open: a budget below one
/// database must not close the last one on each sweep.
fn pick_victims(
    idle: Vec<(u64, FleetDbKey, u64)>,
    excess: usize,
    over: u64,
    open: usize,
) -> Vec<FleetDbKey> {
    let (mut count, mut bytes) = (excess, over);
    let mut victims = Vec::new();
    for (_, key, held) in idle {
        if count == 0 && bytes == 0 {
            break;
        }
        if count == 0 {
            if held == 0 {
                continue;
            }
            if victims.len() + 1 >= open {
                break;
            }
        }
        victims.push(key);
        count = count.saturating_sub(1);
        bytes = bytes.saturating_sub(held);
    }
    victims
}

/// The next candidate staging name in this process; see [`staging_path`].
// Boot builds only a `SQLite` fleet so far: without `sqlite` the staging
// helpers and the health indicator have no caller yet.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// A private path beside `path` for building a database before publishing it:
/// `<dir>/<file>`, where `<dir>` is `.<file>.<tag>-<pid>-<n>`, a directory no
/// template matches (it starts with a dot). The directory is created
/// exclusively, so the name is this caller's alone even across processes that
/// share the volume but not a PID namespace (two containers can both be PID
/// 1): a name someone else holds is skipped. Release it with
/// [`discard_staging`].
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) fn staging_path(path: &Path, tag: &str) -> Result<PathBuf, FleetError> {
    let file_name = path.file_name().ok_or_else(|| FleetError::Io {
        op: "reserve staging name",
        detail: format!("{} has no file name", path.display()),
    })?;
    let file_label = file_name.to_string_lossy();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error("create database directory"))?;
    }
    loop {
        let dir = path.with_file_name(format!(
            ".{file_label}.{tag}-{}-{}",
            std::process::id(),
            STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir.join(file_name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error("reserve staging name")(e)),
        }
    }
}

/// Remove a staging file, any sidecars `SQLite` left beside it, and the
/// directory [`staging_path`] reserved for it.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) fn discard_staging(staging: &Path) {
    let _ = std::fs::remove_file(staging);
    for sidecar in crate::fleet_layout::sidecar_paths(staging) {
        let _ = std::fs::remove_file(sidecar);
    }
    if let Some(dir) = staging.parent() {
        // Only an empty directory goes: nothing of anyone else's is touched.
        let _ = std::fs::remove_dir(dir);
    }
}

/// Refuse a database path that leads out of the fleet's (canonical) root: the
/// file itself a symlink, or a directory on the way to it resolving
/// elsewhere (the nearest one that exists, so a directory not created yet is
/// judged by where it would be created). A stale or planted link would
/// otherwise let a key open, claim and migrate a file that is not a fleet
/// database (the control database, say). Blocking.
fn check_contained(root: &Path, path: &Path) -> Result<(), FleetError> {
    let refuse = |detail: String| FleetError::Io {
        op: "check database path",
        detail,
    };
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(refuse(format!(
            "{} is a symlink; a fleet database must be a regular file inside the root",
            path.display()
        )));
    }
    // The nearest directory that exists decides: a missing one below it would
    // be created through whatever it resolves to.
    let existing = path
        .ancestors()
        .skip(1)
        .find_map(|dir| std::fs::canonicalize(dir).ok());
    if let Some(resolved) = existing
        && !resolved.starts_with(root)
    {
        return Err(refuse(format!(
            "{} resolves to {}, outside the fleet root {}",
            path.display(),
            resolved.display(),
            root.display()
        )));
    }
    Ok(())
}

/// Remove each of `files`. A missing file is fine. Blocking.
fn remove_files(
    files: impl IntoIterator<Item = PathBuf>,
    op: &'static str,
    name: &str,
) -> Result<(), FleetError> {
    for file in files {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(removal_error(op, name)(e)),
        }
    }
    Ok(())
}

/// Remove the directories the template made for `path` when they are now
/// empty, up to (never) `root`. `remove_dir` refuses a non-empty
/// directory, which is the check. Blocking.
fn prune_empty_dirs(path: &Path, root: &Path) {
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

/// Map a failed file removal. Windows error 32 (sharing violation) means
/// another process has the file open. Return [`FleetError::InUseElsewhere`].
fn removal_error<'a>(
    op: &'static str,
    name: &'a str,
) -> impl FnOnce(std::io::Error) -> FleetError + 'a {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    move |e| {
        if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
            FleetError::InUseElsewhere {
                name: name.to_owned(),
            }
        } else {
            io_error(op)(e)
        }
    }
}

// ── The path lock ────────────────────────────────────────────────────────────

/// Directory under the fleet root that holds the path locks.
const LOCK_DIR: &str = crate::fleet_layout::RESERVED_DIR;

/// How many lock files the fleet uses. A key maps to one of them by a stable
/// hash, so a fleet of millions of tenants needs no file per tenant, and a
/// lock file is never deleted (deleting one would break the exclusion).
const LOCK_STRIPES: u64 = 256;

/// Pause between attempts to take a held path lock.
const LOCK_POLL: Duration = Duration::from_millis(5);

/// Attempts before a path lock times out: about 60 seconds.
const LOCK_POLLS: u32 = 12_000;

/// FNV-1a over the key name. Not `DefaultHasher`: processes built by
/// different compilers share one root and must agree on the lock file.
fn lock_stripe(name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash % LOCK_STRIPES
}

/// Run `work` while holding the cross-process lock for `name`'s path.
/// Blocking. Every process that shares the root takes this lock to publish a
/// database at a path (create, restore) or to remove one (delete). So a
/// process that finds the path empty under the lock knows that no other
/// process can publish there until it lets go. It can then clear the stray
/// sidecars of the absent database without risking a live `-wal`.
///
/// The OS drops the lock when a process dies, so a crash leaves no stale lock.
/// Keep `work` short: other keys on the same lock file wait for it.
pub(crate) fn with_path_lock<T>(
    root: &Path,
    name: &str,
    work: impl FnOnce() -> Result<T, FleetError>,
) -> Result<T, FleetError> {
    let mut lock = path_lock_file(root, name)?;
    poll_path_lock(&mut lock, name, |guard| {
        let done = work();
        drop(guard);
        done
    })?
}

/// The lock file of `name`'s stripe.
fn path_lock_file(root: &Path, name: &str) -> Result<fd_lock::RwLock<std::fs::File>, FleetError> {
    let dir = root.join(LOCK_DIR);
    std::fs::create_dir_all(&dir).map_err(io_error("create lock directory"))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{:02x}.lock", lock_stripe(name))))
        .map_err(io_error("open path lock"))?;
    Ok(fd_lock::RwLock::new(file))
}

/// Take `lock` and give its guard to `hold`. Blocking: polls for about 60
/// seconds, then times out.
fn poll_path_lock<T>(
    lock: &mut fd_lock::RwLock<std::fs::File>,
    name: &str,
    hold: impl FnOnce(fd_lock::RwLockWriteGuard<'_, std::fs::File>) -> T,
) -> Result<T, FleetError> {
    let mut polls = 0_u32;
    loop {
        match lock.try_write() {
            Ok(guard) => return Ok(hold(guard)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                polls += 1;
                if polls >= LOCK_POLLS {
                    return Err(FleetError::Unavailable {
                        name: name.to_owned(),
                        detail: "timed out waiting for the path lock".to_owned(),
                    });
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(e) => return Err(io_error("lock database path")(e)),
        }
    }
}

/// The path lock of [`with_path_lock`], held across an `await`. A delete
/// holds it while it writes its tombstone row and removes the files.
///
/// It owns the open lock file. Drop closes the file, and the OS then frees
/// the lock, as for [`VolumeLock`]. On Windows the OS can free it a short
/// time after the close. Another process then polls a little longer.
pub(crate) struct PathLock {
    _file: std::fs::File,
}

impl PathLock {
    /// Take the lock for `name`'s path. Blocking, like [`with_path_lock`].
    ///
    /// # Errors
    ///
    /// [`FleetError::Unavailable`] when the lock times out, or
    /// [`FleetError::Io`] when the lock file cannot be opened.
    pub(crate) fn acquire(root: &Path, name: &str) -> Result<Self, FleetError> {
        let mut lock = path_lock_file(root, name)?;
        // Keep it held: closing the file frees it.
        poll_path_lock(&mut lock, name, |guard| std::mem::forget(guard))?;
        Ok(Self {
            _file: lock.into_inner(),
        })
    }
}

/// An OS lock on `<root>/.fleet-locks/owner.lock`. Each process with
/// ownership holds it shared. A process that gets it exclusive is the last one
/// on the volume. The OS frees it when the process ends.
struct VolumeLock {
    _file: std::fs::File,
}

impl VolumeLock {
    fn open(root: &Path) -> std::io::Result<fd_lock::RwLock<std::fs::File>> {
        let dir = root.join(crate::fleet_layout::RESERVED_DIR);
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("owner.lock"))?;
        Ok(fd_lock::RwLock::new(file))
    }

    /// Take the lock shared. Blocks only while a process holds it exclusive
    /// (for a moment, at its shutdown).
    fn shared(root: &Path) -> std::io::Result<Self> {
        let lock = Self::open(root)?;
        // Keep it held: closing the file frees it.
        std::mem::forget(lock.read()?);
        Ok(Self {
            _file: lock.into_inner(),
        })
    }

    /// Take the lock exclusive, without waiting. `None` while another
    /// process holds it.
    fn exclusive(root: &Path) -> std::io::Result<Option<Self>> {
        let mut lock = Self::open(root)?;
        match lock.try_write() {
            Ok(guard) => std::mem::forget(guard),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
        Ok(Some(Self {
            _file: lock.into_inner(),
        }))
    }
}

/// Sleep until `at`, or forever when there is none.
async fn sleep_until_some(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Readiness indicator `db:fleet`: the root directory is usable, with the
/// fleet's counters as details. Individual databases are not pinged — a
/// tenant fleet can hold millions — a failed open answers its own request.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) struct FleetHealthIndicator<E: FleetEngine> {
    fleet: DatabaseFleet<E>,
}

#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
impl<E: FleetEngine> FleetHealthIndicator<E> {
    pub(crate) const fn new(fleet: DatabaseFleet<E>) -> Self {
        Self { fleet }
    }
}

impl<E: FleetEngine> crate::actuator::HealthIndicator for FleetHealthIndicator<E> {
    fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        Box::pin(async move {
            use crate::actuator::HealthCheckOutput;
            let root = self.fleet.root().to_path_buf();
            let writable = crate::time::spawn_blocking(move || {
                // Unique per check, created new and removed by this check only:
                // concurrent probes (or processes) never race on one name.
                let probe = staging_path(&root.join("fleet"), "probe")
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let written = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&probe)
                    .map(drop);
                discard_staging(&probe);
                written
            })
            .await;
            let stats = self.fleet.stats();
            let mut details: HashMap<String, serde_json::Value> = serde_json::to_value(stats)
                .ok()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            details.insert("mode".to_owned(), self.fleet.mode().to_string().into());
            details.insert(
                "replicating".to_owned(),
                serde_json::Value::Bool(self.fleet.replicates()),
            );
            match writable {
                Ok(Ok(())) => HealthCheckOutput::up().with_details(details),
                Ok(Err(e)) => {
                    details.insert(
                        "error".to_owned(),
                        format!("fleet root is not writable: {e}").into(),
                    );
                    HealthCheckOutput::down().with_details(details)
                }
                Err(e) => {
                    details.insert("error".to_owned(), e.to_string().into());
                    HealthCheckOutput::down().with_details(details)
                }
            }
        })
    }
}

// The tests below open `SQLite` fleets. `engine_tests` covers the core
// without `sqlite`.
#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::sqlite::*;
    use super::*;
    use crate::migrate::EmbeddedMigrations;
    use std::sync::atomic::AtomicUsize;

    fn config(root: &Path, mode: FleetMode) -> DatabaseFleetConfig {
        DatabaseFleetConfig {
            mode,
            root: root.display().to_string(),
            path: None,
            max_open: 8,
            max_resident_bytes: 0,
            pool_size: 2,
            create_on_demand: None,
            idle_close_secs: 0,
            restore_missing: false,
            ownership: None,
            replica_retention_after_delete: std::time::Duration::ZERO,
            restore_check_sample: 0,
            replication_workers: 4,
            min_residency: std::time::Duration::from_secs(2),
            mmap_size: 0,
            cache_size_kib: 0,
        }
    }

    fn fleet(
        root: &Path,
        mode: FleetMode,
        tweak: impl FnOnce(&mut DatabaseFleetConfig),
    ) -> DatabaseFleet {
        let mut config = config(root, mode);
        tweak(&mut config);
        DatabaseFleet::builder(config)
            .min_residency(Duration::ZERO)
            // A loaded CI runner can take more than the default 5 s to open a
            // database and run its migrations.
            .connect_timeout_secs(30)
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .build()
            .unwrap()
    }

    async fn table_exists(db: &FleetDatabase, table: &str) -> bool {
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        let mut conn = db.pool().get().await.unwrap();
        let count: Count = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind::<diesel::sql_types::Text, _>(table)
        .get_result(&mut *conn)
        .await
        .unwrap();
        count.n == 1
    }

    /// How long a test waits for a call that must not hang. A stranded drain
    /// hangs for ever, so a long bound still catches it. A short one fails on
    /// a slow runner: a database create takes seconds on Windows CI, and a
    /// build plus its migrations takes longer still.
    const NOT_STUCK: Duration = Duration::from_secs(60);

    /// Wait for every drain to end. A stranded drain never ends, so `NOT_STUCK`
    /// still catches it. A short bound failed on a loaded Windows runner.
    async fn settle(fleet: &DatabaseFleet) {
        let drained = async {
            while fleet.stats().draining != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(NOT_STUCK, drained)
            .await
            .expect("drains did not settle");
    }

    #[tokio::test]
    async fn a_slot_database_is_created_migrated_and_tagged_on_first_use() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let db = fleet.open_for("acme").await.unwrap();
        let slot = crate::sharding::slot_for_key("acme".into()).0;
        assert_eq!(db.key(), &FleetDbKey::Slot(slot));
        let expected = fleet
            .root()
            .join(format!("{:03}/slot-{slot:05}.db", slot / 64));
        assert_eq!(db.path(), expected);
        assert!(expected.is_file());
        assert!(table_exists(&db, "_autumn_version_history").await);
        assert!(table_exists(&db, IDENTITY_TABLE).await);
        let stats = fleet.stats();
        assert_eq!(
            (stats.open, stats.opens_total, stats.created_total),
            (1, 1, 1)
        );
        assert_eq!(stats.migrations_applied_total, 3);
        // Two tenants in the same slot share the database.
        let again = fleet.open(&FleetDbKey::Slot(slot)).await.unwrap();
        assert_eq!(again.path(), db.path());
        assert_eq!(fleet.stats().opens_total, 1);
    }

    #[tokio::test]
    async fn a_tenant_database_must_be_provisioned_unless_created_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        assert!(matches!(
            fleet.open(&key).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(!fleet.exists(&key));
        assert_eq!(fleet.stats().open, 0, "a failed open leaves nothing behind");

        let db = fleet.provision(&key).await.unwrap();
        assert!(db.path().ends_with(format!("{:03}/acme.db", key.bucket())));
        assert!(matches!(
            fleet.provision(&key).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        fleet.open(&key).await.unwrap();

        assert!(matches!(
            fleet.open_for("../etc/passwd").await,
            Err(FleetError::InvalidKey(_))
        ));
        assert!(matches!(
            fleet.open_for("Acme").await,
            Err(FleetError::InvalidKey(_))
        ));

        let on_demand = super::DatabaseFleet::builder(DatabaseFleetConfig {
            create_on_demand: Some(true),
            ..config(tmp.path(), FleetMode::Tenant)
        })
        .build()
        .unwrap();
        on_demand.open_for("globex").await.unwrap();
    }

    /// A slot key never names a file, so a slot fleet takes any routing key. A
    /// tenant fleet refuses the same keys. In slot mode the app checks its ids.
    #[test]
    fn a_slot_fleet_takes_keys_that_a_tenant_fleet_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let slot = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let tenant = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        for key in ["Acme", "../etc/passwd", "", "con", "a b"] {
            assert!(
                matches!(tenant.key_for(key), Err(FleetError::InvalidKey(_))),
                "tenant mode refuses {key:?}"
            );
            let expected = crate::sharding::slot_for_key(key.into()).0;
            assert_eq!(slot.key_for(key).unwrap(), FleetDbKey::Slot(expected));
        }
    }

    #[tokio::test]
    async fn concurrent_first_opens_share_one_open_and_one_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(7);
        let opens = futures::future::join_all((0..16).map(|_| fleet.open(&key))).await;
        assert!(opens.iter().all(Result::is_ok));
        let stats = fleet.stats();
        assert_eq!(stats.opens_total, 1);
        assert_eq!(stats.migrations_applied_total, 3);
    }

    #[tokio::test]
    async fn the_least_recently_used_idle_database_is_closed_past_max_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 2);
        let first = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(1)).await.unwrap(); // 1 is now newer than 2
        fleet.open(&FleetDbKey::Slot(3)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(
            fleet.open_keys(),
            vec![FleetDbKey::Slot(1), FleetDbKey::Slot(3)]
        );
        assert!(!first.pool().is_closed());
        assert_eq!(fleet.stats().closes_total, 1);
        // A closed database reopens on demand.
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(fleet.stats().open, 2);
    }

    /// LRU closes a mapped database. The pool drops, and the connections
    /// unmap the file. A reopen maps it again and reads every row.
    #[tokio::test]
    async fn an_evicted_mapped_database_reopens_with_its_rows() {
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct MmapSize {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            mmap_size: i64,
        }
        #[derive(diesel::QueryableByName)]
        struct CacheSize {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            cache_size: i64,
        }
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        async fn exec(db: &FleetDatabase, sql: &str) {
            let mut conn = db.pool().get().await.unwrap();
            diesel::sql_query(sql).execute(&mut *conn).await.unwrap();
        }
        async fn state(db: &FleetDatabase) -> (i64, i64, i64) {
            let mut conn = db.pool().get().await.unwrap();
            let mmap = diesel::sql_query("PRAGMA mmap_size")
                .get_result::<MmapSize>(&mut *conn)
                .await
                .unwrap()
                .mmap_size;
            let cache = diesel::sql_query("PRAGMA cache_size")
                .get_result::<CacheSize>(&mut *conn)
                .await
                .unwrap()
                .cache_size;
            let rows = diesel::sql_query("SELECT COUNT(*) AS n FROM notes")
                .get_result::<Count>(&mut *conn)
                .await
                .unwrap()
                .n;
            (mmap, cache, rows)
        }

        const MMAP: u64 = 16 * 1024 * 1024;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| {
            c.max_open = 1;
            c.mmap_size = MMAP;
            c.cache_size_kib = 4096;
        });
        let first = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        exec(&first, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        for n in 0..50 {
            exec(&first, &format!("INSERT INTO notes VALUES ('note {n}')")).await;
        }
        assert_eq!(
            state(&first).await,
            (i64::try_from(MMAP).unwrap(), -4096, 50)
        );
        drop(first);

        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(fleet.open_keys(), vec![FleetDbKey::Slot(2)]);
        assert_eq!(fleet.stats().closes_total, 1, "slot 1 was evicted");

        let reopened = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        exec(&reopened, "INSERT INTO notes VALUES ('after the reopen')").await;
        assert_eq!(
            state(&reopened).await,
            (i64::try_from(MMAP).unwrap(), -4096, 51)
        );
    }

    #[tokio::test]
    async fn the_configured_min_residency_sets_the_lru_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            max_open: 1,
            max_resident_bytes: 0,
            min_residency: Duration::ZERO,
            ..config(tmp.path(), FleetMode::Slot)
        })
        .build()
        .unwrap();
        assert_eq!(fleet.min_residency(), Duration::ZERO);
        fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(fleet.open_keys(), vec![FleetDbKey::Slot(2)]);

        let default = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        assert_eq!(default.min_residency(), DEFAULT_MIN_RESIDENCY);
        let tuned = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .min_residency(Duration::from_millis(5))
            .build()
            .unwrap();
        assert_eq!(tuned.min_residency(), Duration::from_millis(5));
    }

    #[tokio::test]
    async fn a_database_used_within_min_residency_is_not_closed_by_lru() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            max_open: 1,
            max_resident_bytes: 0,
            ..config(tmp.path(), FleetMode::Slot)
        })
        .min_residency(Duration::from_secs(60))
        .build()
        .unwrap();
        let resolved = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(
            fleet.stats().open,
            2,
            "the cap is soft while both are recent"
        );
        // The handle resolved first still checks out: nothing closed it.
        assert!(resolved.pool().get().await.is_ok());
    }

    #[tokio::test]
    async fn the_sweeper_closes_a_burst_once_its_residency_has_passed() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            max_open: 1,
            max_resident_bytes: 0,
            ..config(tmp.path(), FleetMode::Slot)
        })
        .min_residency(Duration::from_millis(150))
        .build()
        .unwrap();
        assert!(fleet.sweep_interval() <= Duration::from_millis(150));
        let started = std::time::Instant::now();
        for slot in 1..=3 {
            fleet.open(&FleetDbKey::Slot(slot)).await.unwrap();
        }
        // A slow runner can outlast the grace while it opens the burst. Then
        // the sweep may already have closed some, and this check does not apply.
        let open = fleet.stats().open;
        if started.elapsed() < Duration::from_millis(150) {
            assert_eq!(open, 3, "a burst inside the grace stays open");
        }
        settle(&fleet).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        // A sweep closes only an idle database. On a busy runner a pool can
        // still hold a connection after the sleep, so sweep as the sweeper
        // does, until the burst is closed or the deadline passes.
        for _ in 0..100 {
            fleet.enforce_capacity(None);
            settle(&fleet).await;
            if fleet.stats().open == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            fleet.stats().open,
            1,
            "the sweep enforces max_open afterwards"
        );
    }

    const BROKEN: EmbeddedMigrations =
        diesel_migrations::embed_migrations!("tests/fixtures/fleet_broken_migrations");

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = walk(dir);
        names.sort();
        names
    }

    fn walk(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == LOCK_DIR {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                out.extend(walk(&entry.path()));
            } else {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        out
    }

    #[tokio::test]
    async fn a_failed_first_migration_publishes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations("broken", BROKEN)
            .build()
            .unwrap();
        let err = fleet.open(&FleetDbKey::Slot(3)).await.unwrap_err();
        assert!(matches!(err, FleetError::Migration { .. }), "{err}");
        assert!(!fleet.exists(&FleetDbKey::Slot(3)));
        assert!(
            entries(fleet.root()).is_empty(),
            "no database and no staging file left: {:?}",
            entries(fleet.root())
        );
    }

    #[tokio::test]
    async fn of_concurrent_provisions_only_the_creator_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        let results = futures::future::join_all((0..8).map(|_| fleet.provision(&key))).await;
        let created = results.iter().filter(|r| r.is_ok()).count();
        let refused = results
            .iter()
            .filter(|r| matches!(r, Err(FleetError::AlreadyExists { .. })))
            .count();
        assert_eq!((created, refused), (1, 7), "{results:?}");
        assert_eq!(fleet.stats().created_total, 1);
    }

    #[tokio::test]
    async fn a_failing_later_set_rolls_back_the_earlier_sets() {
        let tmp = tempfile::tempdir().unwrap();
        // An existing database with no migrations at all.
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        bare.open(&FleetDbKey::Slot(8)).await.unwrap();
        bare.close_all().await;

        let fleet = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .migrations("broken", BROKEN)
            .build()
            .unwrap();
        let err = fleet.open(&FleetDbKey::Slot(8)).await.unwrap_err();
        assert!(matches!(err, FleetError::Migration { .. }), "{err}");

        let check = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        let db = check.open(&FleetDbKey::Slot(8)).await.unwrap();
        assert!(
            !table_exists(&db, "_autumn_version_history").await,
            "the earlier set's migrations were rolled back with the failing one"
        );
    }

    #[test]
    fn creating_a_database_another_process_published_keeps_theirs() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00001.db");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"their bytes").unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), true);
        let created = create_database_file(tmp.path(), &target, "slot:00001", &migrations).unwrap();
        assert!(!created, "the other process won the race");
        assert_eq!(std::fs::read(&target).unwrap(), b"their bytes");
        assert_eq!(
            entries(tmp.path()),
            vec!["slot-00001.db".to_owned()],
            "no staging left"
        );

        let fresh = tmp.path().join("000/slot-00002.db");
        assert!(create_database_file(tmp.path(), &fresh, "slot:00002", &migrations).unwrap());
        assert!(fresh.is_file());
    }

    #[tokio::test]
    async fn a_database_with_a_checked_out_connection_is_never_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 1);
        let busy = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        let held = busy.pool().get().await.unwrap();
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(3)).await.unwrap();
        settle(&fleet).await;
        let open = fleet.open_keys();
        assert!(
            open.contains(&FleetDbKey::Slot(1)),
            "busy database kept: {open:?}"
        );
        assert!(!busy.closed().is_cancelled());
        drop(held);
    }

    #[tokio::test]
    async fn idle_databases_close_after_idle_close() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.idle_close_secs = 1);
        let db = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        let (pool, closed) = (db.pool().clone(), db.closed().clone());
        drop(db);
        // The first open stamps its use before creating and migrating the
        // file, which can take over a second on a slow runner. Using the open
        // database again (a cached hit) stamps it now.
        drop(fleet.open(&FleetDbKey::Slot(1)).await.unwrap());
        assert_eq!(fleet.close_idle(), 0, "not idle long enough yet");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(fleet.close_idle(), 1);
        settle(&fleet).await;
        assert!(closed.is_cancelled());
        assert!(pool.is_closed());
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn a_held_lease_keeps_a_database_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| {
            c.idle_close_secs = 1;
            c.max_open = 1;
        });
        // A lazily acquiring repository holds only the lease and a pool.
        let db = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        let (lease, pool) = (db.lease(), db.pool().clone());
        drop(db);
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let _ = fleet.close_idle();
        fleet.enforce_capacity(None);
        settle(&fleet).await;
        assert!(fleet.open_keys().contains(&FleetDbKey::Slot(1)));
        assert!(pool.get().await.is_ok(), "the leased pool still checks out");
        drop(lease);
        let _ = fleet.close_idle();
        settle(&fleet).await;
        assert!(!fleet.open_keys().contains(&FleetDbKey::Slot(1)));
    }

    #[tokio::test]
    async fn a_background_open_never_grows_the_fleet_past_max_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 1);
        drop(fleet.open(&FleetDbKey::Slot(2)).await.unwrap());
        let hot = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        settle(&fleet).await;
        // Slot 2 exists on disk, but the one open slot is taken.
        let refused = fleet.open_background(&FleetDbKey::Slot(2)).await.unwrap();
        assert!(refused.is_none(), "no room: the background open waits");
        settle(&fleet).await;
        assert_eq!(fleet.open_keys(), vec![FleetDbKey::Slot(1)]);
        assert!(!hot.closed().is_cancelled(), "the hot database stays open");

        // An open database is shared, and the call says it did not open it.
        let shared = fleet
            .open_background(&FleetDbKey::Slot(1))
            .await
            .unwrap()
            .expect("already open");
        assert!(!shared.opened);
        assert_eq!(fleet.stats().open, 1);
    }

    #[tokio::test]
    async fn a_background_open_never_creates_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let missing = FleetDbKey::Slot(7);
        let error = fleet.open_background(&missing).await.unwrap_err();
        assert!(matches!(error, FleetError::NotFound { .. }), "{error}");
        assert!(!fleet.exists(&missing));
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn close_if_unused_spares_a_leased_or_used_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        for slot in [1, 2] {
            drop(fleet.open(&FleetDbKey::Slot(slot)).await.unwrap());
            fleet.close(&FleetDbKey::Slot(slot)).await;
        }

        // Opened in the background and still held: a lease.
        let held = fleet
            .open_background(&FleetDbKey::Slot(1))
            .await
            .unwrap()
            .expect("room");
        assert!(held.opened);
        assert!(!fleet.close_if_unused(&FleetDbKey::Slot(1)), "leased");
        drop(held);
        assert!(fleet.close_if_unused(&FleetDbKey::Slot(1)));
        settle(&fleet).await;
        assert!(!fleet.open_keys().contains(&FleetDbKey::Slot(1)));

        // A request used it after the background open: it is the request's now.
        let background = fleet
            .open_background(&FleetDbKey::Slot(2))
            .await
            .unwrap()
            .expect("room");
        drop(fleet.open(&FleetDbKey::Slot(2)).await.unwrap());
        drop(background);
        assert!(!fleet.close_if_unused(&FleetDbKey::Slot(2)), "used");
        settle(&fleet).await;
        assert!(fleet.open_keys().contains(&FleetDbKey::Slot(2)));
    }

    #[tokio::test]
    async fn a_background_database_is_the_first_lru_victim() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 2);
        drop(fleet.open(&FleetDbKey::Slot(3)).await.unwrap());
        fleet.close(&FleetDbKey::Slot(3)).await;
        drop(fleet.open(&FleetDbKey::Slot(1)).await.unwrap());
        drop(
            fleet
                .open_background(&FleetDbKey::Slot(3))
                .await
                .unwrap()
                .expect("room"),
        );
        // A request past the cap evicts the background database, not slot 1.
        drop(fleet.open(&FleetDbKey::Slot(2)).await.unwrap());
        settle(&fleet).await;
        let mut open = fleet.open_keys();
        open.sort();
        assert_eq!(open, vec![FleetDbKey::Slot(1), FleetDbKey::Slot(2)]);
    }

    #[tokio::test]
    async fn keys_of_another_mode_or_out_of_range_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let slots = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let tenant = FleetDbKey::Tenant {
            id: TenantDbId::parse("acme").unwrap(),
            slot: crate::sharding::slot_for_key("acme".into()).0,
        };
        for bad in [tenant.clone(), FleetDbKey::Slot(16384)] {
            assert!(matches!(
                slots.open(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
            assert!(matches!(
                slots.delete(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
        }
        let tenants = fleet(&tmp.path().join("t"), FleetMode::Tenant, |c| {
            c.create_on_demand = Some(true);
        });
        let wrong_slot = FleetDbKey::Tenant {
            id: TenantDbId::parse("acme").unwrap(),
            slot: crate::sharding::slot_for_key("acme".into()).0 ^ 1,
        };
        for bad in [FleetDbKey::Slot(1), wrong_slot] {
            assert!(matches!(
                tenants.open(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
            assert!(matches!(
                tenants.provision(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
        }
        assert!(
            entries(tenants.root()).is_empty(),
            "nothing touched the disk"
        );
    }

    fn plain_database(dir: &Path) -> PathBuf {
        use diesel::connection::SimpleConnection as _;
        let path = dir.join("plain.db");
        crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(&path))
            .unwrap()
            .batch_execute("PRAGMA journal_mode = WAL; CREATE TABLE t (id INTEGER)")
            .unwrap();
        path
    }

    #[test]
    fn a_sharing_violation_is_in_use_elsewhere() {
        let busy = removal_error("delete database", "acme")(std::io::Error::from_raw_os_error(32));
        assert!(matches!(busy, FleetError::InUseElsewhere { name } if name == "acme"));
        let other = removal_error("delete database", "acme")(std::io::Error::other("disk"));
        assert!(matches!(other, FleetError::Io { .. }), "{other}");
    }

    #[test]
    fn held_access_keeps_other_openers_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let inner = with_exclusive_access(&path, "plain", true, |_| {
            Ok(exclusive_guard(&path, "plain").map(drop))
        })
        .unwrap();
        assert!(matches!(inner, Err(FleetError::InUseElsewhere { .. })));
    }

    #[test]
    fn released_access_lets_the_work_open_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let inner = with_exclusive_access(&path, "plain", false, |_| {
            Ok(exclusive_guard(&path, "plain").map(drop))
        })
        .unwrap();
        assert!(inner.is_ok(), "the guard was released first");
    }

    #[test]
    fn access_is_refused_while_another_connection_has_the_file_open() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let mut other =
            crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(&path)).unwrap();
        other.batch_execute("SELECT 1 FROM t").unwrap();
        for hold in [true, false] {
            let ran = std::cell::Cell::new(false);
            let err = with_exclusive_access(&path, "plain", hold, |_| {
                ran.set(true);
                Ok(())
            })
            .unwrap_err();
            assert!(matches!(err, FleetError::InUseElsewhere { .. }), "{err}");
            assert!(!ran.get(), "no work without proof");
        }
    }

    #[test]
    fn publishing_over_a_file_that_appeared_is_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging.db");
        let path = tmp.path().join("live.db");
        std::fs::write(&staging, b"restored").unwrap();
        std::fs::write(&path, b"opened by someone else").unwrap();
        let err = publish_restored(&staging, &path, "acme").unwrap_err();
        assert!(matches!(err, FleetError::AlreadyExists { .. }), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"opened by someone else");
        std::fs::remove_file(&path).unwrap();
        publish_restored(&staging, &path, "acme").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"restored");
    }

    #[tokio::test]
    async fn delete_refuses_a_database_another_process_has_open() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let recorder = Arc::new(Recorder::default());
        fleet.add_lifecycle(recorder.clone());
        let key = FleetDbKey::Slot(4);
        let path = fleet.open(&key).await.unwrap().path().to_path_buf();
        fleet.close(&key).await;
        // Another process's connection (a separate handle to the same file).
        let mut other = crate::db::establish_sqlite_migration_connection(&format!(
            "sqlite://{}",
            path.display()
        ))
        .unwrap();
        other
            .batch_execute("SELECT 1 FROM _autumn_fleet_identity")
            .unwrap();
        let err = fleet.delete(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::InUseElsewhere { .. }), "{err}");
        assert_eq!(err.http_status(), http::StatusCode::CONFLICT);
        assert!(path.is_file(), "nothing was unlinked");
        assert_eq!(
            recorder.deleted.load(Ordering::SeqCst),
            0,
            "on_delete (say, replica cleanup) must not run for a refused delete"
        );
        fleet.open(&key).await.expect("the key is not stranded");
        fleet.close(&key).await;
        drop(other);
        assert!(fleet.delete(&key).await.unwrap());
        assert!(!path.exists());
        assert_eq!(recorder.deleted.load(Ordering::SeqCst), 1);

        // Nothing on disk: nothing deleted, so no hook either.
        assert!(!fleet.delete(&key).await.unwrap());
        assert_eq!(recorder.deleted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn delete_removes_the_files_and_the_emptied_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.provision(&key).await.unwrap();
        let path = db.path().to_path_buf();
        let bucket = path.parent().unwrap().to_path_buf();
        drop(db);
        assert!(fleet.delete(&key).await.unwrap());
        assert!(!path.exists());
        for sidecar in crate::fleet_layout::sidecar_paths(&path) {
            assert!(!sidecar.exists(), "{}", sidecar.display());
        }
        assert!(!bucket.exists(), "the emptied bucket directory is removed");
        assert!(fleet.root().is_dir(), "the root is kept");
        assert!(matches!(
            fleet.open(&key).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(
            !fleet.delete(&key).await.unwrap(),
            "deleting nothing reports false"
        );
        assert_eq!(fleet.stats().deleted_total, 1);
    }

    #[tokio::test]
    async fn a_file_copied_to_another_key_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let acme = fleet.key_for("acme").unwrap();
        let globex = fleet.key_for("globex").unwrap();
        fleet.provision(&acme).await.unwrap();
        fleet.close(&acme).await;
        let target = fleet.path_of(&globex);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(fleet.path_of(&acme), &target).unwrap();
        let err = fleet.open(&globex).await.unwrap_err();
        assert!(
            matches!(&err, FleetError::Misplaced { name, found } if name == "tenant:globex" && found == "tenant:acme"),
            "{err}"
        );
        assert_eq!(err.http_status(), http::StatusCode::INTERNAL_SERVER_ERROR);

        // `AUTUMN_MIGRATE=1` must not migrate it either: the copy is acme's
        // data, and its migrations are not globex's to run.
        let report = fleet.migrate_all(2).await.unwrap();
        assert_eq!(report.databases, 2);
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        let (failed_key, failure) = &report.failed[0];
        assert_eq!(failed_key, &globex);
        assert!(
            matches!(failure, FleetError::Misplaced { found, .. } if found == "tenant:acme"),
            "{failure}"
        );
    }

    /// A database path, or a bucket directory on the way to it, that is a
    /// symlink must not lead the fleet out of its root: tenant `control`
    /// linked to the control database would otherwise be claimed and
    /// migrated as a tenant.
    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_out_of_the_root_are_refused() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fleet");
        std::fs::create_dir(&root).unwrap();
        let fleet = fleet(&root, FleetMode::Tenant, |c| {
            c.create_on_demand = Some(true);
        });
        let outside = tmp.path().join("control.db");
        crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(&outside))
            .unwrap()
            .batch_execute("CREATE TABLE control_only (id INTEGER)")
            .unwrap();

        // The database file itself is a symlink.
        let control = fleet.key_for("control").unwrap();
        let path = fleet.path_of(&control);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let err = fleet.open(&control).await.unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");

        // A bucket directory is a symlink that leaves the root.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let acme = fleet.key_for("acme").unwrap();
        let acme_path = fleet.path_of(&acme);
        let bucket = acme_path.parent().unwrap();
        if bucket != path.parent().unwrap() {
            std::os::unix::fs::symlink(&elsewhere, bucket).unwrap();
            let err = fleet.open(&acme).await.unwrap_err();
            assert!(err.to_string().contains("outside"), "{err}");
            // Provisioning does its own file work first: it must check too.
            let err = fleet.provision(&acme).await.unwrap_err();
            assert!(err.to_string().contains("outside"), "{err}");
            assert!(
                std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
                "nothing was created outside the root"
            );
        }

        // `AUTUMN_MIGRATE=1` never migrates it: enumeration skips a symlink,
        // and the containment check refuses one that slips through.
        fleet.migrate_all(2).await.unwrap();
        let mut conn =
            crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(&outside))
                .unwrap();
        assert!(
            conn.batch_execute(&format!("SELECT 1 FROM {IDENTITY_TABLE}"))
                .is_err(),
            "the control database was never claimed"
        );
    }

    /// A missing directory under a symlinked one: the nearest directory that
    /// exists decides, before `create_dir_all` follows the link.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "fleet path templates use {placeholders}"
    )]
    async fn a_missing_directory_under_a_symlink_out_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fleet");
        std::fs::create_dir(&root).unwrap();
        let fleet = fleet(&root, FleetMode::Tenant, |c| {
            c.path = Some("{bucket}/new/{tenant}.db".to_owned());
            c.create_on_demand = Some(true);
        });
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let acme = fleet.key_for("acme").unwrap();
        let bucket = fleet
            .path_of(&acme)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink(&elsewhere, &bucket).unwrap();
        let err = fleet.provision(&acme).await.unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        let err = fleet.open(&acme).await.unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing was created outside the root"
        );
    }

    /// `AUTUMN_MIGRATE=1` lists the fleet, then migrates each file. A file
    /// deleted in between (by a live process sharing the root) must stay
    /// deleted: migrating it must not recreate an empty tenant database.
    #[cfg(unix)]
    #[tokio::test]
    async fn migrating_a_database_deleted_after_listing_does_not_recreate_it() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let acme = fleet.key_for("acme").unwrap();
        fleet.provision(&acme).await.unwrap();
        fleet.close(&acme).await;
        let path = fleet.path_of(&acme);
        std::fs::remove_file(&path).unwrap();
        let applied = migrate_existing_blocking(&fleet, &acme).expect("a vanished file is skipped");
        assert_eq!(applied, 0);
        assert!(!path.exists(), "the deleted database stayed deleted");
    }

    #[test]
    fn a_staging_name_another_process_took_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("acme.db");
        // Another container with the same PID and counter got there first.
        let next = STAGING_SEQ.load(Ordering::Relaxed);
        let taken: Vec<PathBuf> = (next..next + 8)
            .map(|n| {
                tmp.path()
                    .join(format!(".acme.db.creating-{}-{n}", std::process::id()))
            })
            .collect();
        for dir in &taken {
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("acme.db"), b"theirs").unwrap();
        }
        let staging = staging_path(&path, "creating").unwrap();
        assert!(
            !taken.iter().any(|dir| staging.starts_with(dir)),
            "{} reused a taken name",
            staging.display()
        );
        assert!(staging.parent().unwrap().is_dir(), "the name is reserved");
        discard_staging(&staging);
        assert!(
            !staging.parent().unwrap().exists(),
            "discard frees the name"
        );
        for dir in &taken {
            assert_eq!(std::fs::read(dir.join("acme.db")).unwrap(), b"theirs");
        }
    }

    #[tokio::test]
    async fn backup_writes_a_consistent_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(9);
        let db = fleet.open(&key).await.unwrap();
        {
            use diesel_async::RunQueryDsl as _;
            let mut conn = db.pool().get().await.unwrap();
            diesel::sql_query("CREATE TABLE notes (body TEXT NOT NULL)")
                .execute(&mut *conn)
                .await
                .unwrap();
            diesel::sql_query("INSERT INTO notes (body) VALUES ('kept')")
                .execute(&mut *conn)
                .await
                .unwrap();
        }
        let dest = tmp.path().join("backup.db");
        fleet.backup(&key, &dest).await.unwrap();
        assert!(matches!(
            fleet.backup(&key, &dest).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        let rows = read_bodies(&dest);
        assert_eq!(rows, vec!["kept".to_owned()]);
    }

    /// A tenant fleet with one note in `acme`, a backup of it, and the key.
    async fn backed_up_tenant(tmp: &Path) -> (DatabaseFleet, FleetDbKey, PathBuf) {
        use diesel_async::RunQueryDsl as _;
        let fleet = fleet(&tmp.join("fleet"), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.provision(&key).await.unwrap();
        {
            let mut conn = db.pool().get().await.unwrap();
            diesel::sql_query("CREATE TABLE notes (body TEXT NOT NULL)")
                .execute(&mut *conn)
                .await
                .unwrap();
            diesel::sql_query("INSERT INTO notes (body) VALUES ('kept')")
                .execute(&mut *conn)
                .await
                .unwrap();
        }
        drop(db);
        let backup = tmp.join("acme-backup.db");
        fleet.backup(&key, &backup).await.unwrap();
        (fleet, key, backup)
    }

    /// No staging file or directory is left under the root.
    fn no_staging_files(root: &Path) -> bool {
        walk_paths(root).iter().all(|path| {
            let text = path.to_string_lossy();
            !text.contains(".importing-") && !text.contains(".creating-")
        })
    }

    fn walk_paths(dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                paths.extend(walk_paths(&path));
            }
            paths.push(path);
        }
        paths
    }

    #[tokio::test]
    async fn import_puts_a_backup_back_under_its_key() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, key, backup) = backed_up_tenant(tmp.path()).await;
        assert!(fleet.delete(&key).await.unwrap());
        assert!(!fleet.exists(&key));

        let db = fleet.import(&key, &backup).await.unwrap();
        assert_eq!(db.key(), &key);
        assert_eq!(read_bodies(&fleet.path_of(&key)), vec!["kept".to_owned()]);
        assert!(backup.is_file(), "the source stays in place");
        assert!(no_staging_files(fleet.root()));
        fleet.close_all().await;
    }

    #[tokio::test]
    async fn import_refuses_a_key_whose_database_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, key, backup) = backed_up_tenant(tmp.path()).await;
        assert!(matches!(
            fleet.import(&key, &backup).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        assert_eq!(read_bodies(&fleet.path_of(&key)), vec!["kept".to_owned()]);
        fleet.close_all().await;
    }

    #[tokio::test]
    async fn import_refuses_a_backup_of_another_key() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, _, backup) = backed_up_tenant(tmp.path()).await;
        let other = fleet.key_for("globex").unwrap();
        let refused = fleet.import(&other, &backup).await.unwrap_err();
        assert!(
            matches!(&refused, FleetError::InvalidSource { detail, .. } if detail.contains("tenant:acme")),
            "{refused}"
        );
        assert_eq!(
            refused.http_status(),
            http::StatusCode::UNPROCESSABLE_ENTITY
        );
        assert!(!fleet.exists(&other));
        assert!(no_staging_files(fleet.root()));
        fleet.close_all().await;
    }

    #[tokio::test]
    async fn import_migrates_a_backup_before_it_publishes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = DatabaseFleet::builder(config(&tmp.path().join("bare"), FleetMode::Tenant))
            .build()
            .unwrap();
        let key = bare.key_for("acme").unwrap();
        drop(bare.provision(&key).await.unwrap());
        let backup = tmp.path().join("bare-backup.db");
        bare.backup(&key, &backup).await.unwrap();
        bare.close_all().await;

        let fleet = fleet(&tmp.path().join("fleet"), FleetMode::Tenant, |_| {});
        let db = fleet.import(&key, &backup).await.unwrap();
        assert!(table_exists(&db, "_autumn_version_history").await);
        drop(db);
        fleet.close_all().await;
    }

    /// The source's newest rows can still be in its `-wal`: the copy has them.
    #[tokio::test]
    async fn import_reads_the_rows_in_the_source_wal() {
        use diesel::Connection as _;
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(&tmp.path().join("fleet"), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        drop(fleet.provision(&key).await.unwrap());
        let source = tmp.path().join("live.db");
        fleet.backup(&key, &source).await.unwrap();
        assert!(fleet.delete(&key).await.unwrap());
        let mut writer =
            diesel::SqliteConnection::establish(&source.display().to_string()).unwrap();
        writer
            .batch_execute(
                "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; \
                 CREATE TABLE notes (body TEXT NOT NULL); \
                 INSERT INTO notes (body) VALUES ('in the wal');",
            )
            .unwrap();
        let mut wal = source.as_os_str().to_owned();
        wal.push("-wal");
        assert!(
            std::fs::metadata(&wal).unwrap().len() > 0,
            "the rows are in the -wal"
        );

        drop(fleet.import(&key, &source).await.unwrap());
        assert_eq!(
            read_bodies(&fleet.path_of(&key)),
            vec!["in the wal".to_owned()]
        );
        drop(writer);
        fleet.close_all().await;
    }

    /// A directory at a database path is no `SQLite` database: `exists`,
    /// `list` and `open` act as if nothing is there.
    #[tokio::test]
    async fn a_directory_at_a_database_path_is_no_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(&tmp.path().join("fleet"), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        std::fs::create_dir_all(fleet.path_of(&key)).unwrap();
        assert!(!fleet.exists(&key));
        assert!(fleet.list().await.unwrap().is_empty());
        assert!(matches!(
            fleet.open(&key).await,
            Err(FleetError::NotFound { .. })
        ));
    }

    /// The copy keeps no epoch of the source: a high stamp there would skip
    /// the check against a newer replica later.
    #[tokio::test]
    async fn import_drops_the_source_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, key, backup) = backed_up_tenant(tmp.path()).await;
        assert!(fleet.delete(&key).await.unwrap());
        write_epoch_stamp(&backup, &key.name(), 99).unwrap();

        drop(fleet.import(&key, &backup).await.unwrap());
        fleet.close_all().await;
        assert_eq!(
            read_epoch_stamp(&fleet.path_of(&key), &key.name()).unwrap(),
            0
        );
        assert_eq!(read_epoch_stamp(&backup, &key.name()).unwrap(), 99);
    }

    fn sqlite_exec(path: &Path, sql: &str) {
        use diesel::Connection as _;
        use diesel::connection::SimpleConnection as _;
        diesel::SqliteConnection::establish(&path.display().to_string())
            .unwrap()
            .batch_execute(sql)
            .unwrap();
    }

    #[tokio::test]
    async fn import_refuses_a_file_that_is_not_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(&tmp.path().join("fleet"), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        let junk = tmp.path().join("junk.db");
        std::fs::write(&junk, b"not a database, not at all, no").unwrap();
        // A plain SQLite file: no fleet made it, so it records no identity.
        let plain = tmp.path().join("plain.db");
        sqlite_exec(&plain, "CREATE TABLE notes (body TEXT NOT NULL);");
        // An identity "table" that is a view answers any name: refused.
        let forged = tmp.path().join("forged.db");
        sqlite_exec(
            &forged,
            &format!("CREATE VIEW {IDENTITY_TABLE} AS SELECT 1 AS id, 'tenant:acme' AS name;"),
        );
        for source in [junk, plain, forged, tmp.path().join("absent.db")] {
            let refused = fleet.import(&key, &source).await.unwrap_err();
            assert!(
                matches!(refused, FleetError::InvalidSource { .. }),
                "{}: {refused}",
                source.display()
            );
        }
        assert!(!fleet.exists(&key));
        assert!(no_staging_files(fleet.root()));
    }

    fn read_bodies(path: &Path) -> Vec<String> {
        use diesel::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Body {
            #[diesel(sql_type = diesel::sql_types::Text)]
            body: String,
        }
        let mut conn = crate::db::establish_sqlite_migration_connection(&format!(
            "sqlite://{}",
            path.display()
        ))
        .unwrap();
        diesel::sql_query("SELECT body FROM notes")
            .load::<Body>(&mut conn)
            .unwrap()
            .into_iter()
            .map(|b| b.body)
            .collect()
    }

    #[tokio::test]
    async fn list_finds_databases_and_ignores_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let mut keys = Vec::new();
        for id in ["acme", "globex", "initech"] {
            let key = fleet.key_for(id).unwrap();
            fleet.provision(&key).await.unwrap();
            keys.push(key);
        }
        keys.sort();
        // Strays: a sidecar-looking file, a foreign file, a tenant file in the
        // wrong bucket, and a symlink.
        let acme = fleet.path_of(&keys[0]);
        std::fs::write(acme.with_file_name("notes.txt"), b"x").unwrap();
        std::fs::write(fleet.root().join("README"), b"x").unwrap();
        let wrong_bucket = fleet
            .root()
            .join(format!("{:03}", (keys[0].bucket() + 1) % 256));
        std::fs::create_dir_all(&wrong_bucket).unwrap();
        std::fs::write(wrong_bucket.join("acme.db"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&acme, acme.with_file_name("zzz.db")).unwrap();
        assert_eq!(fleet.list().await.unwrap(), keys);

        let names = fleet
            .each(2, |db| async move { Ok(db.key().name()) })
            .await
            .unwrap();
        let names: Vec<String> = names.into_iter().map(|(_, r)| r.unwrap()).collect();
        assert_eq!(
            names,
            vec!["tenant:acme", "tenant:globex", "tenant:initech"]
        );
    }

    #[tokio::test]
    async fn migrate_applies_the_pending_migrations_of_one_database() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        bare.open(&FleetDbKey::Slot(5)).await.unwrap();
        bare.open(&FleetDbKey::Slot(6)).await.unwrap();
        bare.close_all().await;

        let fleet = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .apply_migrations_on_open(false)
            .build()
            .unwrap();
        assert_eq!(fleet.migrate(&FleetDbKey::Slot(5)).await.unwrap(), 3);
        assert_eq!(fleet.migrate(&FleetDbKey::Slot(5)).await.unwrap(), 0);
        // Only the key that was named.
        assert!(matches!(
            fleet.open(&FleetDbKey::Slot(6)).await,
            Err(FleetError::PendingMigrations { count: 3, .. })
        ));
        // A missing database is reported and not created.
        let missing = FleetDbKey::Slot(7);
        assert!(matches!(
            fleet.migrate(&missing).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(!fleet.exists(&missing));
    }

    #[tokio::test]
    async fn a_file_migration_source_and_the_framework_sets_migrate_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations/20260101000000_create_notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("up.sql"), "CREATE TABLE notes (id INTEGER);").unwrap();
        std::fs::write(dir.join("down.sql"), "DROP TABLE notes;").unwrap();
        let root = tmp.path().join("fleet");
        let source =
            diesel_migrations::FileBasedMigrations::from_path(tmp.path().join("migrations"))
                .unwrap();
        let fleet = DatabaseFleet::builder(config(&root, FleetMode::Slot))
            .migration_source("app", source)
            .framework_migrations()
            .build()
            .unwrap();
        let db = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        for table in [
            "notes",
            "_autumn_version_history",
            "autumn_repository_commit_hooks",
            "_autumn_derivations",
        ] {
            assert!(table_exists(&db, table).await, "{table}");
        }
        assert!(
            !table_exists(&db, "api_tokens").await,
            "the control-plane set stays on the control database"
        );
        fleet.close_all().await;

        assert!(fleet.remapped_migrations().is_empty());

        // Boot's embedded framework sets find nothing left to apply.
        let all = Arc::new(vec![
            (
                "repository-commit-hooks",
                crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
                crate::migrate::MigrationScope::Everywhere,
                None,
            ),
            (
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
                crate::migrate::MigrationScope::Everywhere,
                None,
            ),
            (
                "derivations",
                crate::derivation::DERIVATION_MIGRATIONS,
                crate::migrate::MigrationScope::Everywhere,
                None,
            ),
        ]);
        let boot = DatabaseFleet::from_parts(
            &DatabaseFleetConfig {
                min_residency: Duration::ZERO,
                ..config(&root, FleetMode::Slot)
            },
            5,
            FleetMigrations::new(&all, false),
        )
        .unwrap();
        boot.open(&FleetDbKey::Slot(1)).await.unwrap();
    }

    #[tokio::test]
    async fn migrate_keys_visits_only_the_given_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        for slot in [1, 2] {
            bare.open(&FleetDbKey::Slot(slot)).await.unwrap();
        }
        bare.close_all().await;
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let report = fleet
            .migrate_keys(vec![FleetDbKey::Slot(1), FleetDbKey::Slot(3)], 2)
            .await;
        assert_eq!((report.databases, report.applied), (2, 3));
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(
            !fleet.exists(&FleetDbKey::Slot(3)),
            "a missing key is not created"
        );
        // Slot 2 was not given: it still has pending migrations.
        let pending = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .apply_migrations_on_open(false)
            .build()
            .unwrap();
        assert!(matches!(
            pending.open(&FleetDbKey::Slot(2)).await,
            Err(FleetError::PendingMigrations { .. })
        ));
    }

    #[tokio::test]
    async fn refuse_unknown_migrations_checks_the_migrated_file() {
        let tmp = tempfile::tempdir().unwrap();
        // A set the next fleet does not have, as a plugin's.
        let full = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .migrations("derivations", crate::derivation::DERIVATION_MIGRATIONS)
            .build()
            .unwrap();
        full.open(&FleetDbKey::Slot(1)).await.unwrap();
        full.close_all().await;

        let partial = |refuse| {
            DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
                .migrations(
                    "version-history",
                    crate::version_history::VERSION_HISTORY_MIGRATIONS,
                )
                .apply_migrations_on_open(false)
                .refuse_unknown_migrations(refuse)
                .build()
                .unwrap()
        };
        let key = FleetDbKey::Slot(1);
        match partial(true).migrate(&key).await {
            Err(FleetError::Migration { detail, .. }) => {
                assert!(detail.contains("none of the sets"), "{detail}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let report = partial(true).migrate_keys(vec![key.clone()], 1).await;
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        // The default does not check.
        assert_eq!(partial(false).migrate(&key).await.unwrap(), 0);
    }

    /// While pinned, a database cannot be removed, so opening it cannot
    /// create a new file in its place; a missing file is `None`.
    #[cfg(windows)]
    #[test]
    fn a_pinned_database_cannot_be_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.db");
        assert!(pin_existing(&path).unwrap().is_none());
        std::fs::write(&path, b"").unwrap();
        let pin = pin_existing(&path).unwrap().expect("the file exists");
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, tmp.path().join("moved.db")).is_err());
        drop(pin);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn known_versions_are_the_recorded_versions_of_every_set() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let mut expected: Vec<String> =
            crate::migrate::migration_versions_and_names::<diesel::sqlite::Sqlite, _>(
                &crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .unwrap()
            .into_iter()
            .map(|(version, _)| version)
            .collect();
        expected.sort();
        assert_eq!(fleet.known_migration_versions(), expected);
    }

    #[test]
    fn a_control_database_inside_the_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(&tmp.path().join("fleet"), FleetMode::Slot, |_| {});
        let inside = format!("sqlite://{}", fleet.root().join("control.db").display());
        let outside = format!("sqlite://{}", tmp.path().join("control.db").display());
        let err = fleet.check_control_outside_root(&[&inside]).unwrap_err();
        assert!(err.contains("control database"), "{err}");
        fleet.check_control_outside_root(&[&outside]).unwrap();
    }

    /// A control database under a directory not created yet, reached
    /// through a symlinked root, is judged by where it would be created.
    #[cfg(unix)]
    #[test]
    fn a_control_database_under_a_missing_directory_of_the_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let fleet = fleet(&alias, FleetMode::Slot, |_| {});
        let control = format!("sqlite://{}", real.join("000/sub/control.db").display());
        let err = fleet.check_control_outside_root(&[&control]).unwrap_err();
        assert!(err.contains("control database"), "{err}");
        assert_eq!(
            canonical_target(Path::new("missing-dir/control.db")),
            Some(
                std::fs::canonicalize(".")
                    .unwrap()
                    .join("missing-dir/control.db")
            )
        );
    }

    #[test]
    fn a_shared_version_is_reported_as_remapped() {
        let tmp = tempfile::tempdir().unwrap();
        // The version of the framework's derivations migration.
        let dir = tmp.path().join("migrations/20260907101530_create_notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("up.sql"), "CREATE TABLE notes (id INTEGER);").unwrap();
        std::fs::write(dir.join("down.sql"), "DROP TABLE notes;").unwrap();
        let source =
            diesel_migrations::FileBasedMigrations::from_path(tmp.path().join("migrations"))
                .unwrap();
        let fleet = DatabaseFleet::builder(config(&tmp.path().join("fleet"), FleetMode::Slot))
            .migration_source("app", source)
            .framework_migrations()
            .build()
            .unwrap();
        assert_eq!(
            fleet.remapped_migrations(),
            vec!["20260907101530_create_notes".to_owned()]
        );
    }

    #[tokio::test]
    async fn inspect_reads_the_identity_and_version_and_writes_nothing() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let acme = fleet.key_for("acme").unwrap();
        fleet.provision(&acme).await.unwrap();
        fleet.close_all().await;
        let info = fleet.inspect(&acme).await.unwrap();
        assert_eq!(info.identity.as_deref(), Some("tenant:acme"));
        let mut expected: Vec<String> =
            crate::migrate::migration_versions_and_names::<diesel::sqlite::Sqlite, _>(
                &crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .unwrap()
            .into_iter()
            .map(|(version, _)| version)
            .collect();
        expected.sort();
        assert_eq!(info.applied_migrations, expected);

        // A file the fleet never opened: nothing to read, and nothing added.
        let globex = fleet.key_for("globex").unwrap();
        let path = fleet.path_of(&globex);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(&path))
            .unwrap()
            .batch_execute("CREATE TABLE notes (id INTEGER)")
            .unwrap();
        let info = fleet.inspect(&globex).await.unwrap();
        assert_eq!(info.identity, None);
        assert!(info.applied_migrations.is_empty(), "{info:?}");
        let mut conn =
            crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(&path)).unwrap();
        assert!(
            conn.batch_execute(&format!("SELECT 1 FROM {IDENTITY_TABLE}"))
                .is_err(),
            "inspect did not claim the file"
        );

        let missing = fleet.key_for("initech").unwrap();
        assert!(matches!(
            fleet.inspect(&missing).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(!fleet.exists(&missing));
    }

    #[tokio::test]
    async fn an_existing_database_with_pending_migrations_is_refused_until_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a database with no migrations at all.
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        bare.open(&FleetDbKey::Slot(5)).await.unwrap();
        bare.close_all().await;

        let report_only = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .apply_migrations_on_open(false)
            .build()
            .unwrap();
        let err = report_only.open(&FleetDbKey::Slot(5)).await.unwrap_err();
        assert!(
            matches!(err, FleetError::PendingMigrations { count: 3, .. }),
            "{err}"
        );
        assert_eq!(err.http_status(), http::StatusCode::SERVICE_UNAVAILABLE);
        // A new database is always migrated: there is nothing to break.
        report_only.open(&FleetDbKey::Slot(6)).await.unwrap();

        let report = report_only.migrate_all(4).await.unwrap();
        assert_eq!(report.databases, 2);
        assert_eq!(report.applied, 3);
        assert!(report.failed.is_empty());
        let db = report_only.open(&FleetDbKey::Slot(5)).await.unwrap();
        assert!(table_exists(&db, "_autumn_version_history").await);
    }

    /// A worker that, once told to stop, finishes one last step that needs
    /// the pool (an ack) after `release` fires.
    struct SettlingWorker {
        release: Arc<Notify>,
        acked: Arc<AtomicBool>,
        hang: bool,
    }

    impl FleetLifecycle for SettlingWorker {
        fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
            let stop = CancellationToken::new();
            let (pool, release, acked, hang) = (
                db.pool().clone(),
                Arc::clone(&self.release),
                Arc::clone(&self.acked),
                self.hang,
            );
            let task_stop = stop.clone();
            let handle = tokio::spawn(async move {
                task_stop.cancelled().await;
                if hang {
                    std::future::pending::<()>().await;
                }
                release.notified().await;
                acked.store(pool.get().await.is_ok(), Ordering::SeqCst);
            });
            db.register_worker(stop, handle);
            Ok(())
        }
    }

    fn settling(hang: bool) -> (Arc<SettlingWorker>, Arc<Notify>, Arc<AtomicBool>) {
        let worker = Arc::new(SettlingWorker {
            release: Arc::new(Notify::new()),
            acked: Arc::new(AtomicBool::new(false)),
            hang,
        });
        (
            worker.clone(),
            Arc::clone(&worker.release),
            Arc::clone(&worker.acked),
        )
    }

    #[tokio::test]
    async fn close_lets_a_worker_settle_before_the_pool_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let key = FleetDbKey::Slot(21);
        let db = fleet.open(&key).await.unwrap();
        let closing = {
            let (fleet, key) = (fleet.clone(), key.clone());
            tokio::spawn(async move { fleet.close(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !db.pool().is_closed(),
            "the pool stays open while the worker runs"
        );
        assert!(!db.closed().is_cancelled());
        release.notify_one();
        closing.await.unwrap();
        assert!(
            acked.load(Ordering::SeqCst),
            "the worker got a connection to settle"
        );
        assert!(db.pool().is_closed());
    }

    /// Issue 3231 holds for a sweeper's close: the worker settles first.
    #[tokio::test]
    async fn close_if_unused_lets_a_worker_settle_before_the_pool_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(22);
        drop(fleet.open(&key).await.unwrap());
        fleet.close(&key).await;
        let (worker, release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let opened = fleet.open_background(&key).await.unwrap().expect("room");
        let db = opened.db.clone();
        drop(opened);
        let pool = db.pool().clone();
        drop(db);
        assert!(fleet.close_if_unused(&key));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !pool.is_closed(),
            "the pool stays open while the worker runs"
        );
        release.notify_one();
        settle(&fleet).await;
        assert!(acked.load(Ordering::SeqCst), "the worker settled first");
        assert!(pool.is_closed());
    }

    /// Counts the restores the fleet asks for, and produces nothing.
    #[derive(Default)]
    struct CountingRestore(AtomicUsize);

    impl FleetLifecycle for CountingRestore {
        fn restore_missing(&self, _key: &FleetDbKey, _path: &Path) -> Result<bool, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        }
    }

    #[tokio::test]
    async fn a_background_open_never_restores_a_missing_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| {
            c.create_on_demand = Some(false);
        });
        let restores = Arc::new(CountingRestore::default());
        fleet.add_lifecycle(restores.clone());
        let key = FleetDbKey::Slot(23);
        let error = fleet.open_background(&key).await.unwrap_err();
        assert!(matches!(error, FleetError::NotFound { .. }), "{error}");
        assert_eq!(
            restores.0.load(Ordering::SeqCst),
            0,
            "a deleted tenant stays deleted"
        );
        let _ = fleet.open(&key).await.unwrap_err();
        assert_eq!(
            restores.0.load(Ordering::SeqCst),
            1,
            "a request may restore it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn close_gives_up_on_a_worker_that_never_settles() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, _release, acked) = settling(true);
        fleet.add_lifecycle(worker);
        let key = FleetDbKey::Slot(22);
        let db = fleet.open(&key).await.unwrap();
        let began = tokio::time::Instant::now();
        fleet.close(&key).await;
        assert!(
            db.pool().is_closed(),
            "a stuck worker cannot block the close"
        );
        assert!(began.elapsed() >= WORKER_QUIESCE_TIMEOUT);
        assert!(!acked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn delete_stops_workers_at_once_instead_of_letting_them_settle() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, _release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let key = FleetDbKey::Slot(29);
        let db = fleet.open(&key).await.unwrap();
        let deleted = tokio::time::timeout(Duration::from_secs(5), fleet.delete(&key))
            .await
            .expect("a delete does not wait for a worker");
        assert!(deleted.unwrap());
        assert!(db.pool().is_closed());
        assert!(!acked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_worker_registered_after_the_stop_began_is_stopped_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(30);
        let db = fleet.open(&key).await.unwrap();
        fleet.close(&key).await;
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let handle = tokio::spawn(async move { task_stop.cancelled().await });
        let probe = handle.abort_handle();
        db.register_worker(stop.clone(), handle);
        assert!(stop.is_cancelled());
        tokio::task::yield_now().await;
        assert!(probe.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn close_all_pays_the_worker_timeout_once() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, _release, _acked) = settling(true);
        fleet.add_lifecycle(worker);
        for slot in 25..28 {
            fleet.open(&FleetDbKey::Slot(slot)).await.unwrap();
        }
        let began = tokio::time::Instant::now();
        fleet.close_all().await;
        assert!(
            began.elapsed() < WORKER_QUIESCE_TIMEOUT * 2,
            "three stuck workers cost one timeout, not three"
        );
    }

    #[tokio::test]
    async fn close_all_waits_for_workers_of_every_database_together() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let first = fleet.open(&FleetDbKey::Slot(23)).await.unwrap();
        let second = fleet.open(&FleetDbKey::Slot(24)).await.unwrap();
        let closing = {
            let fleet = fleet.clone();
            tokio::spawn(async move { fleet.close_all().await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!first.pool().is_closed() && !second.pool().is_closed());
        // One permit per worker: `notify_waiters` would be lost to a late one.
        release.notify_one();
        release.notify_one();
        closing.await.unwrap();
        assert!(acked.load(Ordering::SeqCst));
        assert!(first.pool().is_closed() && second.pool().is_closed());
    }

    #[derive(Default)]
    struct Recorder {
        opened: AtomicUsize,
        closed: AtomicUsize,
        deleted: AtomicUsize,
    }

    impl FleetLifecycle for Recorder {
        fn on_open(&self, _db: &FleetDatabase) -> Result<(), String> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn on_close(&self, db: &FleetDatabase) {
            assert!(db.path().is_file());
            assert!(db.pool().is_closed());
            assert_eq!(db.pool().status().size, 0);
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
        fn on_delete(&self, _key: &FleetDbKey) {
            self.deleted.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Refuse;
    impl FleetLifecycle for Refuse {
        fn on_open(&self, _db: &FleetDatabase) -> Result<(), String> {
            Err("replicator unavailable".to_owned())
        }
    }

    #[tokio::test]
    async fn lifecycle_hooks_follow_open_close_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let recorder = Arc::new(Recorder::default());
        fleet.add_lifecycle(recorder.clone());
        let key = FleetDbKey::Slot(11);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        let closing = {
            let fleet = fleet.clone();
            let key = key.clone();
            tokio::spawn(async move { fleet.close(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            recorder.closed.load(Ordering::SeqCst),
            0,
            "on_close waits for the in-flight connection"
        );
        assert!(db.closed().is_cancelled());
        assert!(
            db.pool().get().await.is_err(),
            "a closed pool hands out nothing new"
        );
        drop(held);
        closing.await.unwrap();
        assert_eq!(recorder.closed.load(Ordering::SeqCst), 1);

        fleet.open(&key).await.unwrap();
        assert!(fleet.delete(&key).await.unwrap());
        assert_eq!(recorder.opened.load(Ordering::SeqCst), 2);
        assert_eq!(recorder.closed.load(Ordering::SeqCst), 2);
        assert_eq!(recorder.deleted.load(Ordering::SeqCst), 1);

        fleet.add_lifecycle(Arc::new(Refuse));
        let err = fleet.open(&FleetDbKey::Slot(12)).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert_eq!(fleet.stats().open_failures_total, 1);
    }

    #[tokio::test]
    async fn opening_during_a_delete_is_refused_and_after_it_starts_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(13);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        let deleting = {
            let fleet = fleet.clone();
            let key = key.clone();
            tokio::spawn(async move { fleet.delete(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(
            fleet.open(&key).await,
            Err(FleetError::Deleting { .. })
        ));
        drop(held);
        assert!(deleting.await.unwrap().unwrap());
        // A slot fleet creates on demand: the slot comes back empty.
        let fresh = fleet.open(&key).await.unwrap();
        assert!(!table_exists(&fresh, "notes").await);
    }

    // ── Cancellation safety (#3229) ──────────────────────────────────────

    /// Hook whose `on_open` blocks for a while, like a slow replicator start.
    struct SlowOpen {
        delay: Duration,
        started: AtomicUsize,
        inner: Recorder,
    }

    impl SlowOpen {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                delay,
                started: AtomicUsize::new(0),
                inner: Recorder::default(),
            })
        }
    }

    impl FleetLifecycle for SlowOpen {
        fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
            self.started.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.inner.on_open(db)
        }
        fn on_close(&self, db: &FleetDatabase) {
            self.inner.on_close(db);
        }
    }

    /// Cancel `call` after `after`, as a request timeout does.
    async fn cancelled<T>(after: Duration, call: impl Future<Output = T>) {
        assert!(
            tokio::time::timeout(after, call).await.is_err(),
            "the call finished before the timeout"
        );
    }

    /// Cancel `call` once a new run of the slow `on_open` hook has started.
    async fn cancelled_in_hook<T>(hook: &SlowOpen, call: impl Future<Output = T>) {
        let before = hook.started.load(Ordering::SeqCst);
        let started = async {
            while hook.started.load(Ordering::SeqCst) == before {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        };
        tokio::select! {
            _ = call => panic!("the call finished before the hook started"),
            () = started => {}
        }
    }

    /// Wait until `key` has a file, as a publish that outlived its caller does.
    async fn published(fleet: &DatabaseFleet, key: &FleetDbKey) {
        let found = async {
            while !fleet.exists(key) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(NOT_STUCK, found)
            .await
            .expect("the file was never published");
    }

    #[tokio::test]
    async fn a_cancelled_delete_still_finishes_and_frees_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(21);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        cancelled(Duration::from_millis(60), fleet.delete(&key)).await;
        drop(held);
        drop(db);
        settle(&fleet).await;
        assert!(!fleet.exists(&key), "the delete ran to the end");
        assert_eq!(fleet.stats().deleted_total, 1);
        // A slot fleet creates on demand: the key opens again, empty.
        let fresh = tokio::time::timeout(NOT_STUCK, fleet.open(&key))
            .await
            .expect("open is not stuck on a stranded drain")
            .unwrap();
        assert!(!table_exists(&fresh, "notes").await);
        drop(fresh);
        tokio::time::timeout(NOT_STUCK, fleet.close_all())
            .await
            .expect("close_all does not wait on a stranded drain");
    }

    #[tokio::test]
    async fn a_cancelled_close_still_finishes_and_frees_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let recorder = Arc::new(Recorder::default());
        fleet.add_lifecycle(recorder.clone());
        let key = FleetDbKey::Slot(22);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        cancelled(Duration::from_millis(60), fleet.close(&key)).await;
        drop(held);
        drop(db);
        settle(&fleet).await;
        assert_eq!(recorder.closed.load(Ordering::SeqCst), 1);
        tokio::time::timeout(NOT_STUCK, fleet.open(&key))
            .await
            .expect("open is not stuck on a stranded drain")
            .unwrap();
        tokio::time::timeout(NOT_STUCK, fleet.close_all())
            .await
            .expect("close_all returns");
    }

    #[tokio::test]
    async fn a_cancelled_first_open_leaves_nothing_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(400));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(23);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        // The delete waits for the open that is still running, then drains it.
        assert!(fleet.delete(&key).await.unwrap());
        settle(&fleet).await;
        let (opened, closed) = (
            hook.inner.opened.load(Ordering::SeqCst),
            hook.inner.closed.load(Ordering::SeqCst),
        );
        assert_eq!(opened, 1, "the abandoned open ran its hook");
        assert_eq!(opened, closed, "every on_open has its on_close");
        assert!(!fleet.exists(&key));
    }

    /// Fails every open after a delay, and counts the attempts.
    struct SlowFail(AtomicUsize);

    impl FleetLifecycle for SlowFail {
        fn on_open(&self, _db: &FleetDatabase) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            Err("hook failed".to_owned())
        }
    }

    #[tokio::test]
    async fn many_cancelled_callers_share_one_opener() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = Arc::new(SlowFail(AtomicUsize::new(0)));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(27);
        let callers = futures::future::join_all((0..5).map(|_| fleet.open(&key)));
        // Drop all five callers while the one opener runs its hook.
        let _ = tokio::time::timeout(Duration::from_millis(60), callers).await;
        // The opener fails on its own task; wait for its entry to go.
        let failed = async {
            while !(fleet.stats().open_failures_total > 0 && fleet.open_keys().is_empty()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(NOT_STUCK, failed)
            .await
            .expect("the failed entry never went");
        assert_eq!(hook.0.load(Ordering::SeqCst), 1, "one opener, not five");
        assert_eq!(fleet.stats().open_failures_total, 1);
        assert!(fleet.open_keys().is_empty(), "the failed entry is gone");
    }

    #[tokio::test]
    async fn a_cancelled_open_still_keeps_the_fleet_within_max_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 1);
        let hook = SlowOpen::new(Duration::from_millis(100));
        fleet.add_lifecycle(hook.clone());
        fleet.open(&FleetDbKey::Slot(28)).await.unwrap();
        let second = FleetDbKey::Slot(29);
        // The caller leaves; the opener task still fills the entry.
        cancelled_in_hook(&hook, fleet.open(&second)).await;
        let closed = async {
            while !(fleet.stats().open == 1 && fleet.stats().draining == 0) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        assert!(
            tokio::time::timeout(NOT_STUCK, closed).await.is_ok(),
            "the older database stayed open: {:?}",
            fleet.open_keys()
        );
    }

    #[tokio::test]
    async fn close_all_waits_for_a_delete_that_just_started() {
        use futures::FutureExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(30);
        fleet.open(&key).await.unwrap();
        // Closed, so `close_all` has no open key or drain to wait on.
        fleet.close(&key).await;
        // One poll starts the owned delete; the caller then leaves.
        assert!(fleet.delete(&key).now_or_never().is_none());
        fleet.close_all().await;
        assert_eq!(fleet.stats().deleted_total, 1, "the delete ended first");
        assert!(!fleet.exists(&key));
    }

    #[tokio::test]
    async fn close_waits_for_an_open_that_is_still_running() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(200));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(26);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        fleet.close(&key).await;
        settle(&fleet).await;
        assert_eq!(hook.inner.closed.load(Ordering::SeqCst), 1);
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn close_all_waits_for_an_open_that_is_still_running() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(300));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(25);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        fleet.close_all().await;
        assert_eq!(hook.inner.opened.load(Ordering::SeqCst), 1);
        assert_eq!(
            hook.inner.closed.load(Ordering::SeqCst),
            1,
            "the late open was closed, not left running"
        );
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_first_open_is_kept_for_the_next_opener() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(300));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(24);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        fleet.open(&key).await.unwrap();
        assert_eq!(
            hook.inner.opened.load(Ordering::SeqCst),
            1,
            "one open, not one per caller"
        );
        fleet.close_all().await;
        assert_eq!(hook.inner.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_cancelled_provision_is_reported_once_by_the_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        // A zero timeout drops the call at its first wait, mid-publish.
        cancelled(Duration::ZERO, fleet.provision(&key)).await;
        // Let the abandoned publish finish.
        published(&fleet, &key).await;
        let retry = fleet.provision(&key).await;
        assert!(retry.is_ok(), "the retry sees the creation: {retry:?}");
        let again = fleet.provision(&key).await;
        assert!(
            matches!(again, Err(FleetError::AlreadyExists { .. })),
            "the creation is reported once: {again:?}"
        );
        assert_eq!(fleet.stats().created_total, 1);
    }

    #[tokio::test]
    async fn a_provision_that_finishes_is_not_reported_again() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        fleet.provision(&key).await.unwrap();
        assert!(matches!(
            fleet.provision(&key).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        assert_eq!(fleet.stats().created_total, 1);
    }

    #[tokio::test]
    async fn a_provision_cancelled_while_opening_is_reported_by_the_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(200));
        fleet.add_lifecycle(hook.clone());
        let key = fleet.key_for("acme").unwrap();
        // The file is published; the caller leaves during the open.
        cancelled_in_hook(&hook, fleet.provision(&key)).await;
        let retry = fleet.provision(&key).await;
        assert!(retry.is_ok(), "the retry reports the creation: {retry:?}");
        assert!(matches!(
            fleet.provision(&key).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        assert_eq!(hook.inner.opened.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_delete_clears_an_unclaimed_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        cancelled(Duration::ZERO, fleet.provision(&key)).await;
        published(&fleet, &key).await;
        assert!(fleet.delete(&key).await.unwrap());
        // Another process creates the file. This fleet did not.
        let other = self::fleet(tmp.path(), FleetMode::Tenant, |_| {});
        other.provision(&key).await.unwrap();
        assert!(
            matches!(
                fleet.provision(&key).await,
                Err(FleetError::AlreadyExists { .. })
            ),
            "a stale marker must not claim another process's creation"
        );
    }

    #[test]
    fn names_resolve_back_to_keys_of_the_fleet_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let tenants = DatabaseFleet::builder(config(tmp.path(), FleetMode::Tenant))
            .build()
            .unwrap();
        let key = tenants.key_for_name("tenant:acme").unwrap();
        assert_eq!(key, tenants.key_for("acme").unwrap());
        assert!(tenants.key_for_name("slot:00001").is_err());
        let slots = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        assert_eq!(
            slots.key_for_name("slot:00001").unwrap(),
            FleetDbKey::Slot(1)
        );
        assert!(slots.key_for_name("tenant:acme").is_err());
    }

    #[test]
    fn the_main_file_is_unlinked_last() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let sidecars = crate::fleet_layout::sidecar_paths(&path);
        let mut seen: Vec<PathBuf> = Vec::new();
        unlink_database_set(&path, "delete database", "acme", true, |file| {
            seen.push(file.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.last(), Some(&path));
        assert_eq!(seen.len(), sidecars.len() + 2, "sidecars, lock file, main");
    }

    #[test]
    fn a_database_published_after_the_unlink_keeps_its_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let wal = tmp.path().join(format!(
            "{}-wal",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&wal, b"old").unwrap();
        // Another process publishes a new database the moment the main file
        // is gone, and commits to its `-wal`.
        unlink_database_set(&path, "delete database", "acme", true, |file| {
            let done = std::fs::remove_file(file);
            if file == path {
                std::fs::write(&path, b"new").unwrap();
                std::fs::write(&wal, b"new frames").unwrap();
            }
            done
        })
        .unwrap();
        assert_eq!(std::fs::read(&wal).unwrap(), b"new frames");
    }

    #[test]
    fn a_sharing_violation_on_a_sidecar_stops_before_the_main_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let err = unlink_database_set(&path, "delete database", "acme", true, |file| {
            if file == path {
                std::fs::remove_file(file)
            } else {
                Err(std::io::Error::from_raw_os_error(32))
            }
        });
        if cfg!(windows) {
            assert!(matches!(err, Err(FleetError::InUseElsewhere { .. })));
        } else {
            assert!(err.is_err());
        }
        assert!(path.is_file(), "the main file is still there");
    }

    #[test]
    fn remove_database_files_removes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let sidecars = crate::fleet_layout::sidecar_paths(&path);
        for sidecar in &sidecars {
            std::fs::write(sidecar, b"x").unwrap();
        }
        assert!(remove_database_files(&path, tmp.path(), "acme", true).unwrap());
        assert!(!path.exists());
        assert!(sidecars.iter().all(|sidecar| !sidecar.exists()));
    }

    #[test]
    fn the_strays_of_an_absent_path_go_and_a_main_file_is_never_unlinked() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.db");
        let mut seen = Vec::new();
        // The caller holds the path lock, so no other process can publish.
        unlink_database_set(&path, "delete database", "acme", false, |file| {
            seen.push(file.to_path_buf());
            Ok(())
        })
        .unwrap();
        let mut strays = crate::fleet_layout::sidecar_paths(&path).to_vec();
        strays.push(crate::fleet_layout::replicator_lock_path(&path));
        assert_eq!(seen, strays);
    }

    #[tokio::test]
    async fn provision_after_shutdown_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        fleet.close_all().await;
        let err = fleet.provision(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert!(!fleet.path_of(&key).exists());
    }

    #[test]
    fn the_guard_tells_the_work_whether_the_file_was_there() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let seen = with_exclusive_access(&path, "plain", true, Ok).unwrap();
        assert!(seen);
        let absent = tmp.path().join("missing.db");
        // The file appears after the guard looked. The work still sees what
        // the guard saw.
        let seen = with_exclusive_access(&absent, "missing", true, |present| {
            std::fs::write(&absent, b"new").unwrap();
            Ok(present)
        })
        .unwrap();
        assert!(!seen);
    }

    #[test]
    fn a_database_published_after_an_absent_guard_is_not_reported_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        // The guard saw no file. Another process publishes one before the work.
        let path = plain_database(tmp.path());
        assert!(!remove_database_files(&path, tmp.path(), "acme", false).unwrap());
        assert!(path.is_file(), "the new database stays");
    }

    #[tokio::test]
    async fn a_delete_waits_for_a_running_open_and_no_new_opener_gets_in() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(26);
        // An opener is running when the delete takes the key.
        let Next::Open(entry, true) = fleet.claim_entry(&key, false).unwrap() else {
            panic!("expected to make the entry");
        };
        let opener = fleet.spawn_opener(&key, true, true, None, &entry);
        let (taken, held, drain) = fleet.begin_exclusive(&key, true).unwrap().unwrap();
        // The drain is in first: no new opener gets an entry.
        assert!(matches!(
            fleet.claim_entry(&key, false),
            Err(FleetError::Deleting { .. })
        ));
        // The running open ends, and the delete gets its database.
        let db = fleet
            .take_database(held)
            .await
            .expect("the opened database");
        assert!(db.path().is_file());
        opener.await.unwrap().unwrap();
        fleet.drain(&taken, Some(db), true).await;
        fleet.finish_drain(&taken, &drain);
    }

    #[tokio::test]
    async fn an_open_after_close_all_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(27);
        fleet.open(&key).await.unwrap();
        fleet.close_all().await;
        assert_eq!(fleet.stats().open, 0);
        let err = fleet.open(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert_eq!(err.http_status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn close_all_refuses_opens_only_after_the_counted_work_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        // A counted operation (a provision, say) is still running.
        let pending = MutationGuard::new(&fleet);
        let closing = {
            let fleet = fleet.clone();
            tokio::spawn(async move { fleet.close_all().await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!closing.is_finished(), "close_all waits for the work");
        // The work may still open its database: the flag is not set yet.
        fleet.open(&FleetDbKey::Slot(28)).await.unwrap();
        drop(pending);
        closing.await.unwrap();
        let after = fleet.open(&FleetDbKey::Slot(29)).await;
        assert!(matches!(after, Err(FleetError::Unavailable { .. })));
    }

    #[tokio::test]
    async fn no_work_is_admitted_once_shutdown_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        fleet.close_all().await;
        assert!(fleet.admit_mutation(&FleetDbKey::Slot(30)).is_err());
    }

    #[tokio::test]
    async fn delete_and_restore_are_refused_once_shutdown_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(31);
        fleet.open(&key).await.unwrap();
        fleet.close_all().await;
        let path = fleet.path_of(&key);
        let err = fleet.delete(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert!(path.is_file(), "a refused delete removes nothing");
        // `restore` checks the shutdown flag before it needs replication.
        let err = fleet.restore(&key, None).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
    }

    #[test]
    fn path_locks_exclude_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let first = {
            let (root, log) = (tmp.path().to_path_buf(), Arc::clone(&log));
            std::thread::spawn(move || {
                with_path_lock(&root, "slot:00001", || {
                    entered_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(200));
                    log.lock().unwrap().push("first leaves");
                    Ok(())
                })
                .unwrap();
            })
        };
        entered.recv().unwrap();
        with_path_lock(tmp.path(), "slot:00001", || {
            log.lock().unwrap().push("second enters");
            Ok(())
        })
        .unwrap();
        first.join().unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["first leaves", "second enters"]);
    }

    #[test]
    fn a_create_waits_for_a_publisher_and_leaves_its_wal_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00005.db");
        let wal = tmp.path().join("000/slot-00005.db-wal");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Another process holds the path lock. It publishes a database and
        // commits to it while the lock is held.
        let (held_tx, held) = std::sync::mpsc::channel();
        let publisher = {
            let (root, target, wal) = (tmp.path().to_path_buf(), target.clone(), wal.clone());
            std::thread::spawn(move || {
                with_path_lock(&root, "slot:00005", || {
                    held_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    std::fs::write(&target, b"their database").unwrap();
                    std::fs::write(&wal, b"their live frames").unwrap();
                    Ok(())
                })
                .unwrap();
            })
        };
        held.recv().unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), true);
        let created = create_database_file(tmp.path(), &target, "slot:00005", &migrations).unwrap();
        publisher.join().unwrap();
        assert!(!created, "the publisher won: this create waited for it");
        assert_eq!(std::fs::read(&target).unwrap(), b"their database");
        assert_eq!(std::fs::read(&wal).unwrap(), b"their live frames");
    }

    #[test]
    fn a_new_database_does_not_inherit_a_stray_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00003.db");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let wal = tmp.path().join("000/slot-00003.db-wal");
        std::fs::write(&wal, b"left behind by a crash").unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), true);
        assert!(create_database_file(tmp.path(), &target, "slot:00003", &migrations).unwrap());
        assert!(!wal.exists(), "a stale -wal would replay onto the new file");
    }

    #[test]
    fn a_stray_wal_beside_a_published_database_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00004.db");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"their bytes").unwrap();
        let wal = tmp.path().join("000/slot-00004.db-wal");
        std::fs::write(&wal, b"their frames").unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), true);
        assert!(!create_database_file(tmp.path(), &target, "slot:00004", &migrations).unwrap());
        assert_eq!(std::fs::read(&wal).unwrap(), b"their frames");
    }

    #[tokio::test]
    async fn delete_waits_for_the_path_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(32);
        let path = fleet.open(&key).await.unwrap().path().to_path_buf();
        fleet.close(&key).await;
        let (held_tx, held) = std::sync::mpsc::channel();
        let holder = {
            let (root, name) = (tmp.path().to_path_buf(), key.name());
            std::thread::spawn(move || {
                with_path_lock(&root, &name, || {
                    held_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(400));
                    Ok(())
                })
                .unwrap();
            })
        };
        held.recv().unwrap();
        let began = std::time::Instant::now();
        let deleting = {
            let (fleet, key) = (fleet.clone(), key.clone());
            tokio::spawn(async move { fleet.delete(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(path.is_file(), "the delete waits for the lock holder");
        assert!(deleting.await.unwrap().unwrap());
        assert!(began.elapsed() >= Duration::from_millis(300));
        assert!(!path.exists());
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn delete_clears_the_strays_of_an_absent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(33);
        let path = fleet.path_of(&key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let wal = crate::fleet_layout::sidecar_paths(&path)[0].clone();
        std::fs::write(&wal, b"left behind by a crash").unwrap();
        assert!(!fleet.delete(&key).await.unwrap(), "there was no database");
        assert!(!wal.exists());
    }

    /// A restore hook that records whether the stray `-wal` of the absent
    /// database was still there when it ran. It restores nothing.
    struct SeesStrays {
        wal_present: AtomicUsize,
        locked: AtomicUsize,
        root: PathBuf,
        name: String,
    }

    impl FleetLifecycle for SeesStrays {
        fn restore_missing(&self, _key: &FleetDbKey, path: &Path) -> Result<bool, String> {
            if crate::fleet_layout::sidecar_paths(path)[0].exists() {
                self.wal_present.fetch_add(1, Ordering::SeqCst);
            }
            // The fleet must hold the path lock while the hook runs: taking
            // it here from this thread must time out, so only try it.
            let dir = self.root.join(LOCK_DIR);
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join(format!("{:02x}.lock", lock_stripe(&self.name))))
                .map_err(|e| e.to_string())?;
            let mut lock = fd_lock::RwLock::new(file);
            if lock.try_write().is_err() {
                self.locked.fetch_add(1, Ordering::SeqCst);
            }
            Ok(false)
        }
    }

    #[tokio::test]
    async fn a_restore_on_open_runs_under_the_lock_after_the_strays_go() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(34);
        let hook = Arc::new(SeesStrays {
            wal_present: AtomicUsize::new(0),
            locked: AtomicUsize::new(0),
            root: tmp.path().to_path_buf(),
            name: key.name(),
        });
        fleet.add_lifecycle(hook.clone());
        let path = fleet.path_of(&key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let wal = crate::fleet_layout::sidecar_paths(&path)[0].clone();
        std::fs::write(&wal, b"left behind by a crash").unwrap();
        fleet.open(&key).await.unwrap();
        assert_eq!(hook.wal_present.load(Ordering::SeqCst), 0, "strays cleared");
        assert_eq!(hook.locked.load(Ordering::SeqCst), 1, "the hook ran locked");
    }
}

#[cfg(test)]
pub(crate) mod test_engine;

#[cfg(test)]
mod engine_tests;

#[cfg(all(test, feature = "sqlite"))]
mod generations_tests;

#[cfg(all(test, feature = "sqlite"))]
mod local_restore_tests;
