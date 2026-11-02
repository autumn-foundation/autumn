//! Cross-tenant analytics over the replicas of a fleet (ADR 0032, issue
//! #3236).
//!
//! The lake reads object storage, not the volume. It never opens a file
//! under the fleet root, so it runs on a host with no volume.
//!
//! ```text
//! list    ─► one listing of <base>/fleet/ (tenants, markers, generations)
//! decide  ─► a tombstone row or marker covers the generation: skip it
//! restore ─► N workers restore N tenants into a private scratch directory
//! ingest  ─► in tenant order: ATTACH one file, copy each table with an
//!            _autumn_tenant column, DETACH, remove the file
//! query   ─► a read-only connection runs one SELECT over the lake
//! ```
//!
//! Each tenant is consistent at its own snapshot. The lake has no atomic
//! snapshot across tenants. The SQL is operator SQL: do not pass request
//! input to it.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use diesel::connection::SimpleConnection as _;
use diesel::{OptionalExtension as _, RunQueryDsl as _, SqliteConnection};

use crate::entropy::Entropy;
use crate::fleet_layout::{FleetDbKey, TenantDbId};
use crate::replication::destination::ReplicaDestination;
use crate::replication::restore::{self, RestoreError, RestorePlan};
use crate::replication::segment;

use super::fleet_replication::replica_root;
use super::fleet_tombstones::{FleetTombstones, MARKER_OBJECT, Marker, restore_allowed};

/// The column the lake adds to each table: the tenant id of the row.
pub const TENANT_COLUMN: &str = "_autumn_tenant";

/// The lake table with one row per loaded tenant: `tenant`, `generation`,
/// `effective` (RFC 3339) and `effective_ms`.
pub const MANIFEST_TABLE: &str = "_autumn_lake_tenants";

/// Tenants restored at the same time when the caller sets no count.
pub const DEFAULT_LAKE_WORKERS: usize = 4;

/// The temporary view a query runs through.
const RESULT_VIEW: &str = "_autumn_lake_result";

/// The identity table of a fleet database.
const IDENTITY_TABLE: &str = "_autumn_fleet_identity";

/// Tombstone rows read per page.
const TOMBSTONE_PAGE: usize = 500;

/// Columns per `json_array` call. `SQLite` caps function arguments.
const COLUMNS_PER_ARRAY: usize = 100;

/// A value in a lake result. It keeps the `SQLite` storage class.
#[derive(Debug, Clone, PartialEq)]
pub enum LakeValue {
    /// `NULL`.
    Null,
    /// An `INTEGER`.
    Integer(i64),
    /// A `REAL`.
    Real(f64),
    /// A `TEXT`.
    Text(String),
    /// A `BLOB`.
    Blob(Vec<u8>),
}

impl LakeValue {
    /// The value as an `i64`, when it is an integer.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// The value as an `f64`, when it is a number.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "an integer read as a real")]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Integer(value) => Some(*value as f64),
            Self::Real(value) => Some(*value),
            _ => None,
        }
    }

    /// The value as text, when it is text.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

/// The rows of one lake query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LakeRows {
    /// Column names, in result order.
    pub columns: Vec<String>,
    /// Rows, in result order.
    pub rows: Vec<Vec<LakeValue>>,
}

/// One tenant in the lake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LakeTenant {
    /// The tenant.
    pub tenant: TenantDbId,
    /// The replica generation the lake restored.
    pub generation: String,
    /// The instant the restored state is current as of.
    pub effective: DateTime<Utc>,
}

/// Why the lake did not load a database it found in object storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// A tombstone row or marker covers the generation (issue #3235).
    Deleted,
    /// No complete generation exists yet.
    Unreplicated,
    /// The tenant has no state at or before the `as_of` instant.
    AbsentAtTarget,
    /// The name is not a valid tenant id.
    InvalidName,
    /// A slot database. The lake loads tenant databases only.
    SlotDatabase,
    /// The tenant failed. Only with [`FleetLake::skip_failed_tenants`].
    Failed(String),
}

/// A database the lake did not load, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LakeSkip {
    /// The name in object storage: the tenant id, or the slot number.
    pub name: String,
    /// Why.
    pub reason: SkipReason,
}

/// Why a lake could not be made or queried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LakeError {
    /// The lake has no tombstone source and no `markers_only` opt-in.
    #[error(
        "the lake has no tombstone source: call with_tombstones(..), or markers_only() \
         to accept that a delete with no marker yet is not seen"
    )]
    NoTombstoneSource,
    /// The scratch directory or the kept copy is under the fleet root.
    #[error("{} is under the fleet root {}", path.display(), root.display())]
    UnderRoot {
        /// The path, canonical.
        path: PathBuf,
        /// The fleet root, canonical.
        root: PathBuf,
    },
    /// The scratch directory cannot be made or removed.
    #[error("lake scratch directory: {0}")]
    Scratch(String),
    /// The kept copy cannot be written.
    #[error("keep the lake: {0}")]
    Persist(String),
    /// The destination cannot be listed or read.
    #[error("replica destination: {0}")]
    Destination(String),
    /// The tombstone rows cannot be read.
    #[error("tombstone rows: {0}")]
    Tombstones(String),
    /// The tombstone marker of a tenant cannot be read.
    #[error("tenant {tenant}: unreadable tombstone marker: {detail}")]
    Marker {
        /// The tenant.
        tenant: String,
        /// Detail.
        detail: String,
    },
    /// The restore of a tenant failed.
    #[error("tenant {tenant}: restore failed: {detail}")]
    Restore {
        /// The tenant.
        tenant: String,
        /// Detail.
        detail: String,
    },
    /// The identity row of a restored file names another database.
    #[error("tenant {tenant}: the replica is misplaced: {detail}")]
    Misplaced {
        /// The tenant.
        tenant: String,
        /// Detail.
        detail: String,
    },
    /// A source table already has the tag column.
    #[error(
        "tenant {tenant}: table {table} has a column named {TENANT_COLUMN}, which the lake \
         adds; rename the column"
    )]
    ReservedColumn {
        /// The tenant.
        tenant: String,
        /// The table.
        table: String,
    },
    /// Copying a tenant into the lake failed.
    #[error("tenant {tenant}: load into the lake failed: {detail}")]
    Ingest {
        /// The tenant.
        tenant: String,
        /// Detail.
        detail: String,
    },
    /// The destination holds slot databases only.
    #[error("the fleet keeps slot databases; the lake loads tenant databases only")]
    SlotFleet,
    /// The query failed, or it is not one `SELECT`, `WITH` or `VALUES`.
    #[error("lake query: {0}")]
    Query(String),
    /// A blocking task did not finish.
    #[error("lake task: {0}")]
    Task(String),
}

impl LakeError {
    /// The tenant a per-tenant error names.
    fn tenant(&self) -> Option<&str> {
        match self {
            Self::Marker { tenant, .. }
            | Self::Restore { tenant, .. }
            | Self::Misplaced { tenant, .. }
            | Self::ReservedColumn { tenant, .. }
            | Self::Ingest { tenant, .. } => Some(tenant),
            _ => None,
        }
    }
}

/// A read-only view of a fleet's replicas, loaded on demand into a scratch
/// `SQLite` lake. See the module docs.
#[derive(Clone)]
pub struct FleetLake {
    destination: Arc<dyn ReplicaDestination>,
    base: String,
    tombstones: Option<Arc<FleetTombstones>>,
    markers_only: bool,
    skip_failed: bool,
    as_of: Option<DateTime<Utc>>,
    tables: Option<Vec<String>>,
    scratch: Option<PathBuf>,
    workers: usize,
    entropy: Arc<dyn Entropy>,
    /// A canonical fleet root the scratch directory must not be under.
    forbidden_root: Option<PathBuf>,
}

impl std::fmt::Debug for FleetLake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetLake")
            .field("destination", &self.destination.describe())
            .field("base", &self.base)
            .field("tombstones", &self.tombstones.is_some())
            .field("markers_only", &self.markers_only)
            .field("skip_failed", &self.skip_failed)
            .field("as_of", &self.as_of)
            .field("tables", &self.tables)
            .field("scratch", &self.scratch)
            .field("workers", &self.workers)
            .finish_non_exhaustive()
    }
}

impl FleetLake {
    /// A lake over the replicas under `base` (the replication root, for
    /// example `app/prod`) in `destination`.
    #[must_use]
    pub fn new(destination: Arc<dyn ReplicaDestination>, base: impl Into<String>) -> Self {
        Self {
            destination,
            base: base.into().trim_end_matches('/').to_owned(),
            tombstones: None,
            markers_only: false,
            skip_failed: false,
            as_of: None,
            tables: None,
            scratch: None,
            workers: DEFAULT_LAKE_WORKERS,
            entropy: Arc::new(crate::entropy::OsEntropy),
            forbidden_root: None,
        }
    }

    /// Skip each tenant that has a tombstone row on the control database.
    #[must_use]
    pub fn with_tombstones(mut self, tombstones: Arc<FleetTombstones>) -> Self {
        self.tombstones = Some(tombstones);
        self
    }

    /// Use the tombstone markers in object storage only. A delete whose
    /// marker is not written yet is then not seen.
    #[must_use]
    pub const fn markers_only(mut self) -> Self {
        self.markers_only = true;
        self
    }

    /// Report a tenant that fails as skipped, and load the rest. By default
    /// such a tenant fails the whole load.
    #[must_use]
    pub const fn skip_failed_tenants(mut self) -> Self {
        self.skip_failed = true;
        self
    }

    /// Restore each tenant at its newest state at or before `at`. The
    /// tenants are not restored at one common instant.
    #[must_use]
    pub const fn as_of(mut self, at: DateTime<Utc>) -> Self {
        self.as_of = Some(at);
        self
    }

    /// Load only these tables.
    #[must_use]
    pub fn with_tables<I, S>(mut self, tables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tables = Some(tables.into_iter().map(Into::into).collect());
        self
    }

    /// Make the scratch directory under `parent`, which must exist. The
    /// default is the system temporary directory. The lake needs about as
    /// much space as the loaded tables, plus one restored file per worker.
    #[must_use]
    pub fn with_scratch_dir(mut self, parent: impl Into<PathBuf>) -> Self {
        self.scratch = Some(parent.into());
        self
    }

    /// Restore `workers` tenants at the same time. `0` means `1`.
    #[must_use]
    pub const fn with_workers(mut self, workers: usize) -> Self {
        self.workers = if workers == 0 { 1 } else { workers };
        self
    }

    /// Name the scratch directory from `entropy`.
    #[must_use]
    pub fn with_entropy(mut self, entropy: Arc<dyn Entropy>) -> Self {
        self.entropy = entropy;
        self
    }

    /// Refuse a scratch directory or a kept copy under `root`, the fleet
    /// root. [`DatabaseFleet::lake`](super::fleet::DatabaseFleet::lake)
    /// sets it. A `root` that does not exist refuses nothing.
    #[must_use]
    pub fn outside_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.forbidden_root = Some(root.into());
        self
    }

    /// Restore every live tenant and load it into a new lake.
    ///
    /// # Errors
    ///
    /// See [`LakeError`]. A tenant that fails fails the load, unless
    /// [`skip_failed_tenants`](Self::skip_failed_tenants) is set.
    pub async fn materialize(&self) -> Result<LakeSnapshot, LakeError> {
        let rows = self.tombstone_rows().await?;
        let lake = self.clone();
        crate::time::spawn_blocking(move || lake.materialize_blocking(&rows))
            .await
            .map_err(|error| LakeError::Task(error.to_string()))?
    }

    /// Make a lake, run one query on it, and remove it. Each call restores
    /// every tenant: use [`materialize`](Self::materialize) to run more.
    ///
    /// # Errors
    ///
    /// See [`LakeError`].
    pub async fn query(&self, sql: &str) -> Result<LakeRows, LakeError> {
        let snapshot = self.materialize().await?;
        let sql = sql.to_owned();
        crate::time::spawn_blocking(move || {
            let rows = collect_rows(&snapshot.files.db, &sql);
            drop(snapshot);
            rows
        })
        .await
        .map_err(|error| LakeError::Task(error.to_string()))?
    }

    /// The names (`tenant:<id>`) of the tombstone rows. Empty with
    /// `markers_only`.
    async fn tombstone_rows(&self) -> Result<HashSet<String>, LakeError> {
        let Some(tombstones) = &self.tombstones else {
            return if self.markers_only {
                Ok(HashSet::new())
            } else {
                Err(LakeError::NoTombstoneSource)
            };
        };
        let mut names = HashSet::new();
        let mut after: Option<String> = None;
        loop {
            let page = tombstones
                .list_after(after.as_deref(), TOMBSTONE_PAGE)
                .await
                .map_err(|error| LakeError::Tombstones(error.to_string()))?;
            let full = page.len() == TOMBSTONE_PAGE;
            after = page.last().map(|row| row.key.clone());
            names.extend(page.into_iter().map(|row| row.key));
            if !full {
                return Ok(names);
            }
        }
    }

    fn materialize_blocking(&self, rows: &HashSet<String>) -> Result<LakeSnapshot, LakeError> {
        let root = self
            .forbidden_root
            .as_deref()
            .and_then(|root| root.canonicalize().ok());
        let scratch = Scratch::create(
            &self.scratch.clone().unwrap_or_else(std::env::temp_dir),
            root.as_deref(),
            self.entropy.as_ref(),
        )?;
        let found = discover(self.destination.as_ref(), &self.base)?;
        let mut skipped = found.skipped;
        let mut planned = Vec::new();
        for (tenant, keys) in &found.tenants {
            match self.plan_tenant(tenant, keys, rows) {
                Ok(Planned::Restore(plan)) => planned.push((tenant.clone(), *plan)),
                Ok(Planned::Skip(reason)) => skipped.push(LakeSkip {
                    name: tenant.to_string(),
                    reason,
                }),
                Err(error) => skipped.push(self.failed(error)?),
            }
        }

        let db = scratch.dir.join("lake.db");
        let mut conn = open_lake(&db)?;
        let mut tenants = Vec::new();
        for (chunk_index, chunk) in planned.chunks(self.workers).enumerate() {
            let restored = self.restore_chunk(&scratch.dir, chunk_index, chunk);
            for ((tenant, plan), (output, result)) in chunk.iter().zip(restored) {
                let loaded = result
                    .map_err(|error| LakeError::Restore {
                        tenant: tenant.to_string(),
                        detail: error.to_string(),
                    })
                    .and_then(|()| {
                        ingest(&mut conn, tenant, plan, &output, self.tables.as_deref())
                    });
                remove_database(&output);
                match loaded {
                    Ok(()) => tenants.push(LakeTenant {
                        tenant: tenant.clone(),
                        generation: plan.generation.clone(),
                        effective: plan.effective,
                    }),
                    Err(error) => skipped.push(self.failed(error)?),
                }
            }
        }
        drop(conn);
        skipped.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(LakeSnapshot {
            files: Arc::new(LakeFiles {
                dir: scratch.keep(),
                db,
            }),
            tenants,
            skipped,
            root,
        })
    }

    /// A per-tenant error: a skip with `skip_failed_tenants`, else the
    /// error.
    fn failed(&self, error: LakeError) -> Result<LakeSkip, LakeError> {
        match error.tenant() {
            Some(tenant) if self.skip_failed => Ok(LakeSkip {
                name: tenant.to_owned(),
                reason: SkipReason::Failed(error.to_string()),
            }),
            _ => Err(error),
        }
    }

    /// Decide from the listing, then plan the restore. The restore applies
    /// this plan, so the decision and the restore see the same generation.
    /// A listing older than the replica (a ship or a purge since) is read
    /// again for this tenant, once.
    fn plan_tenant(
        &self,
        tenant: &TenantDbId,
        keys: &[String],
        rows: &HashSet<String>,
    ) -> Result<Planned, LakeError> {
        let key = FleetDbKey::Tenant {
            id: tenant.clone(),
            slot: 0,
        };
        let root = replica_root(&self.base, &key);
        let row = rows.contains(&key.name());
        if let Attempt::Done(planned) = self.plan_from(tenant, &root, keys, row)? {
            return Ok(planned);
        }
        let fresh = self
            .destination
            .list(&format!("{root}/"))
            .map_err(|error| LakeError::Destination(error.to_string()))?;
        match self.plan_from(tenant, &root, &fresh, row)? {
            Attempt::Done(planned) => Ok(planned),
            Attempt::Stale(detail) => Err(LakeError::Restore {
                tenant: tenant.to_string(),
                detail,
            }),
        }
    }

    /// One decision and plan from `keys`.
    fn plan_from(
        &self,
        tenant: &TenantDbId,
        root: &str,
        keys: &[String],
        row: bool,
    ) -> Result<Attempt, LakeError> {
        let marker = match self.marker(tenant, root, keys)? {
            MarkerRead::Absent => None,
            MarkerRead::At(deleted_at) => Some(deleted_at),
            MarkerRead::Gone => {
                return Ok(Attempt::Stale(
                    "the tombstone marker is gone since the listing".to_owned(),
                ));
            }
        };
        let skip = |reason| Ok(Attempt::Done(Planned::Skip(reason)));
        match decide(row, marker, newest_generation(root, keys)) {
            Decision::Skip(Skip::Deleted) => return skip(SkipReason::Deleted),
            Decision::Skip(Skip::Unreplicated) => return skip(SkipReason::Unreplicated),
            Decision::Plan => {}
        }
        let plan =
            match restore::plan_from_listing(self.destination.as_ref(), root, keys, self.as_of) {
                Ok(plan) => plan,
                Err(RestoreError::NoReplica { .. }) => return skip(SkipReason::Unreplicated),
                Err(RestoreError::TargetBeforeRetention { .. }) => {
                    return skip(SkipReason::AbsentAtTarget);
                }
                // The head moved on after the listing.
                Err(error @ RestoreError::TruncatedTail { .. }) => {
                    return Ok(Attempt::Stale(error.to_string()));
                }
                Err(error) => {
                    return Err(LakeError::Restore {
                        tenant: tenant.to_string(),
                        detail: error.to_string(),
                    });
                }
            };
        // `as_of` can pick an older generation than the newest.
        if !restore_allowed(row, marker, plan.generation_started_at) {
            return skip(SkipReason::Deleted);
        }
        Ok(Attempt::Done(Planned::Restore(Box::new(plan))))
    }

    /// The tenant's marker, read only when the listing has one.
    fn marker(
        &self,
        tenant: &TenantDbId,
        root: &str,
        keys: &[String],
    ) -> Result<MarkerRead, LakeError> {
        let marker_key = format!("{root}/{MARKER_OBJECT}");
        if !keys.contains(&marker_key) {
            return Ok(MarkerRead::Absent);
        }
        let unreadable = |detail: String| LakeError::Marker {
            tenant: tenant.to_string(),
            detail,
        };
        let body = match self.destination.get(&marker_key) {
            Ok(body) => body,
            // A purge or a restore removed it after the listing.
            Err(error) if error.is_not_found() => return Ok(MarkerRead::Gone),
            Err(error) => return Err(unreadable(error.to_string())),
        };
        serde_json::from_slice::<Marker>(&body)
            .map(|marker| MarkerRead::At(marker.deleted_at()))
            .map_err(|error| unreadable(error.to_string()))
    }

    /// Restore one chunk of tenants, one thread each. Blocking.
    fn restore_chunk(
        &self,
        dir: &Path,
        chunk_index: usize,
        chunk: &[(TenantDbId, RestorePlan)],
    ) -> Vec<(PathBuf, Result<(), RestoreError>)> {
        let destination = self.destination.as_ref();
        let output = |index: usize| dir.join(format!("{chunk_index:08}-{index:04}.db"));
        std::thread::scope(|scope| {
            // Start every thread before the first join.
            #[allow(clippy::needless_collect, reason = "spawn all, then join")]
            let handles: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(index, (_, plan))| {
                    let path = output(index);
                    let handle = std::thread::Builder::new()
                        .name("autumn-fleet-lake".to_owned())
                        .spawn_scoped(scope, {
                            let path = path.clone();
                            move || restore::apply(destination, plan, &path).map(|_| ())
                        });
                    (path, plan, handle)
                })
                .collect();
            handles
                .into_iter()
                .map(|(path, plan, handle)| {
                    // No thread: restore on this one.
                    let result = handle.map_or_else(
                        |_| restore::apply(destination, plan, &path).map(|_| ()),
                        |handle| {
                            handle.join().unwrap_or_else(|_| {
                                Err(RestoreError::Io {
                                    op: "restore thread",
                                    detail: "the restore thread panicked".to_owned(),
                                })
                            })
                        },
                    );
                    (path, result)
                })
                .collect()
        })
    }
}

/// What the lake does with one tenant.
enum Planned {
    Restore(Box<RestorePlan>),
    Skip(SkipReason),
}

/// One try of [`FleetLake::plan_tenant`].
enum Attempt {
    Done(Planned),
    /// The listing is older than the replica. Why.
    Stale(String),
}

/// The tombstone marker of a tenant.
enum MarkerRead {
    /// The listing has none.
    Absent,
    /// The listing has one, and it is gone now.
    Gone,
    /// Its `deleted_at`.
    At(DateTime<Utc>),
}

/// The databases one listing of `<base>/fleet/` finds.
struct Discovery {
    /// Each tenant and every key under its root, sorted by tenant.
    tenants: BTreeMap<TenantDbId, Vec<String>>,
    /// Invalid names and slot databases.
    skipped: Vec<LakeSkip>,
}

/// List `<base>/fleet/` once and group the keys by database.
fn discover(destination: &dyn ReplicaDestination, base: &str) -> Result<Discovery, LakeError> {
    let prefix = format!("{base}/fleet/");
    let keys = destination
        .list(&prefix)
        .map_err(|error| LakeError::Destination(error.to_string()))?;
    let mut tenants: BTreeMap<TenantDbId, Vec<String>> = BTreeMap::new();
    let mut invalid = BTreeSet::new();
    let mut slots = BTreeSet::new();
    for key in keys {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        let Some((kind, rest)) = rest.split_once('/') else {
            continue;
        };
        let Some((name, _)) = rest.split_once('/') else {
            continue;
        };
        match kind {
            "tenant" => match TenantDbId::parse(name) {
                Ok(id) => tenants.entry(id).or_default().push(key),
                Err(_) => {
                    invalid.insert(name.to_owned());
                }
            },
            "slot" => {
                slots.insert(name.to_owned());
            }
            _ => {}
        }
    }
    if tenants.is_empty() && !slots.is_empty() {
        return Err(LakeError::SlotFleet);
    }
    let skip = |reason: SkipReason| {
        move |name: String| LakeSkip {
            name,
            reason: reason.clone(),
        }
    };
    let skipped = invalid
        .into_iter()
        .map(skip(SkipReason::InvalidName))
        .chain(slots.into_iter().map(skip(SkipReason::SlotDatabase)))
        .collect();
    Ok(Discovery { tenants, skipped })
}

/// When the newest complete generation under `root` was created.
fn newest_generation(root: &str, keys: &[String]) -> Option<DateTime<Utc>> {
    keys.iter()
        .filter(|key| key.ends_with(segment::SNAPSHOT_META_OBJECT))
        .filter_map(|key| segment::generation_of_key(root, key))
        .filter_map(|generation| segment::parse_generation_id(&generation))
        .filter_map(|info| DateTime::from_timestamp_millis(info.created_ms))
        .max()
}

/// The inclusion decision before a restore. With the `restore_allowed`
/// check after the plan, it is `included` in `verification/fleet_lake.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Skip(Skip),
    Plan,
}

/// Why [`decide`] skips a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    Deleted,
    Unreplicated,
}

/// Decide from the listing alone. `row`: a tombstone row exists.
/// `marker`: the marker's `deleted_at`. `newest`: when the newest complete
/// generation was created. A marker with no generation newer than it is a
/// delete, or a purge in progress.
pub(crate) fn decide(
    row: bool,
    marker: Option<DateTime<Utc>>,
    newest: Option<DateTime<Utc>>,
) -> Decision {
    match newest {
        _ if row => Decision::Skip(Skip::Deleted),
        Some(newest) if restore_allowed(false, marker, newest) => Decision::Plan,
        Some(_) => Decision::Skip(Skip::Deleted),
        None if marker.is_some() => Decision::Skip(Skip::Deleted),
        None => Decision::Skip(Skip::Unreplicated),
    }
}

// ── Scratch ─────────────────────────────────────────────────────────────────

/// A private scratch directory. Removed on drop unless kept.
struct Scratch {
    dir: PathBuf,
    keep: bool,
}

impl Scratch {
    /// Make a new owner-only directory under `parent`. Refuse a `parent`
    /// under `root`.
    fn create(
        parent: &Path,
        root: Option<&Path>,
        entropy: &dyn Entropy,
    ) -> Result<Self, LakeError> {
        let parent = parent
            .canonicalize()
            .map_err(|error| LakeError::Scratch(format!("{}: {error}", parent.display())))?;
        refuse_under(&parent, root)?;
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        for _ in 0..8 {
            let dir = parent.join(format!("autumn-lake-{:016x}", entropy.next_u64()));
            match builder.create(&dir) {
                Ok(()) => return Ok(Self { dir, keep: false }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(LakeError::Scratch(format!("{}: {error}", dir.display())));
                }
            }
        }
        Err(LakeError::Scratch(format!(
            "no unused directory name under {}",
            parent.display()
        )))
    }

    /// Give the directory to a snapshot.
    fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.dir.clone()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Refuse `path` (canonical) under `root` (canonical).
fn refuse_under(path: &Path, root: Option<&Path>) -> Result<(), LakeError> {
    match root {
        Some(root) if path.starts_with(root) => Err(LakeError::UnderRoot {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        }),
        _ => Ok(()),
    }
}

/// Remove a restored file and its sidecars.
fn remove_database(path: &Path) {
    let _ = std::fs::remove_file(path);
    for sidecar in crate::fleet_layout::sidecar_paths(path) {
        let _ = std::fs::remove_file(sidecar);
    }
}

// ── Ingest ──────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct Name {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

#[derive(diesel::QueryableByName)]
struct ListedTable {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    kind: String,
}

#[derive(diesel::QueryableByName)]
struct Column {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    hidden: i32,
}

/// A `file:` URI for `path` with `query`.
fn uri(path: &Path, query: &str) -> Result<String, String> {
    crate::replication::file_uri(path)
        .map(|uri| format!("{uri}?{query}"))
        .ok_or_else(|| format!("{} is not valid Unicode", path.display()))
}

/// Make the lake file and its manifest table.
fn open_lake(db: &Path) -> Result<SqliteConnection, LakeError> {
    let scratch = |detail: String| LakeError::Scratch(format!("{}: {detail}", db.display()));
    let mut conn =
        super::establish_sqlite_migration_connection(&uri(db, "mode=rwc").map_err(scratch)?)
            .map_err(|error| scratch(error.to_string()))?;
    // A scratch file: a crash loses nothing the next load does not make.
    // The journal stays (in memory): a skipped tenant rolls back.
    conn.batch_execute(&format!(
        "PRAGMA journal_mode = MEMORY; PRAGMA synchronous = OFF; \
         CREATE TABLE {MANIFEST_TABLE} (tenant TEXT PRIMARY KEY, generation TEXT NOT NULL, \
         effective TEXT NOT NULL, effective_ms INTEGER NOT NULL);"
    ))
    .map_err(|error| scratch(error.to_string()))?;
    Ok(conn)
}

/// Copy one restored tenant into the lake, in one transaction.
fn ingest(
    conn: &mut SqliteConnection,
    tenant: &TenantDbId,
    plan: &RestorePlan,
    path: &Path,
    tables: Option<&[String]>,
) -> Result<(), LakeError> {
    let failed = |detail: String| LakeError::Ingest {
        tenant: tenant.to_string(),
        detail,
    };
    // A private file `restore::apply` checkpointed: no locks, no sidecars.
    diesel::sql_query("ATTACH DATABASE ? AS src")
        .bind::<diesel::sql_types::Text, _>(uri(path, "mode=ro&immutable=1").map_err(failed)?)
        .execute(conn)
        .map_err(|error| failed(error.to_string()))?;
    let result = conn
        .batch_execute("BEGIN")
        .map_err(|error| failed(error.to_string()))
        .and_then(|()| copy_tenant(conn, tenant, plan, tables));
    let ended = match &result {
        Ok(()) => conn.batch_execute("COMMIT"),
        Err(_) => conn.batch_execute("ROLLBACK"),
    };
    let detached = conn.batch_execute("DETACH DATABASE src");
    result?;
    ended.map_err(|error| failed(error.to_string()))?;
    detached.map_err(|error| failed(error.to_string()))
}

/// The copy inside the transaction of [`ingest`].
fn copy_tenant(
    conn: &mut SqliteConnection,
    tenant: &TenantDbId,
    plan: &RestorePlan,
    tables: Option<&[String]>,
) -> Result<(), LakeError> {
    let failed = |error: diesel::result::Error| LakeError::Ingest {
        tenant: tenant.to_string(),
        detail: error.to_string(),
    };
    check_identity(conn, tenant)?;
    let mut names: Vec<String> =
        diesel::sql_query("SELECT name, type AS kind FROM pragma_table_list WHERE schema = 'src'")
            .load::<ListedTable>(conn)
            .map_err(failed)?
            .into_iter()
            .filter(|table| table.kind == "table" && is_app_table(&table.name))
            .map(|table| table.name)
            .filter(|name| {
                tables.is_none_or(|only| only.iter().any(|t| t.eq_ignore_ascii_case(name)))
            })
            .collect();
    names.sort();
    for table in &names {
        let columns: Vec<String> =
            diesel::sql_query("SELECT name, hidden FROM pragma_table_xinfo(?, 'src')")
                .bind::<diesel::sql_types::Text, _>(table)
                .load::<Column>(conn)
                .map_err(failed)?
                .into_iter()
                // 1: a hidden column of a virtual table. 2, 3: generated.
                .filter(|column| column.hidden != 1)
                .map(|column| column.name)
                .collect();
        if columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(TENANT_COLUMN))
        {
            return Err(LakeError::ReservedColumn {
                tenant: tenant.to_string(),
                table: table.clone(),
            });
        }
        add_lake_table(conn, table, &columns).map_err(failed)?;
        let list = std::iter::once(TENANT_COLUMN.to_owned())
            .chain(columns.iter().cloned())
            .map(|column| quote_ident(&column))
            .collect::<Vec<_>>()
            .join(", ");
        let select = columns
            .iter()
            .map(|column| quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        let table = quote_ident(table);
        diesel::sql_query(format!(
            "INSERT INTO main.{table} ({list}) SELECT ?, {select} FROM src.{table}"
        ))
        .bind::<diesel::sql_types::Text, _>(tenant.as_str())
        .execute(conn)
        .map_err(failed)?;
    }
    diesel::sql_query(format!("INSERT INTO {MANIFEST_TABLE} VALUES (?, ?, ?, ?)"))
        .bind::<diesel::sql_types::Text, _>(tenant.as_str())
        .bind::<diesel::sql_types::Text, _>(&plan.generation)
        .bind::<diesel::sql_types::Text, _>(plan.effective.to_rfc3339())
        .bind::<diesel::sql_types::BigInt, _>(plan.effective.timestamp_millis())
        .execute(conn)
        .map_err(failed)?;
    Ok(())
}

/// A table the app owns: not `SQLite`'s, the framework's or diesel's.
fn is_app_table(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !(lower.starts_with("sqlite_")
        || lower.starts_with("_autumn_")
        || lower.starts_with("__diesel"))
}

/// The identity row of the restored file must name the tenant.
fn check_identity(conn: &mut SqliteConnection, tenant: &TenantDbId) -> Result<(), LakeError> {
    let misplaced = |detail: String| LakeError::Misplaced {
        tenant: tenant.to_string(),
        detail,
    };
    let expected = FleetDbKey::Tenant {
        id: tenant.clone(),
        slot: 0,
    }
    .name();
    let exists =
        diesel::sql_query("SELECT name FROM src.sqlite_schema WHERE type = 'table' AND name = ?")
            .bind::<diesel::sql_types::Text, _>(IDENTITY_TABLE)
            .get_result::<Name>(conn)
            .optional()
            .map_err(|error| misplaced(error.to_string()))?
            .is_some();
    if !exists {
        return Err(misplaced("the identity row is missing".to_owned()));
    }
    let found = diesel::sql_query(format!(
        "SELECT name FROM src.{IDENTITY_TABLE} WHERE id = 1"
    ))
    .get_result::<Name>(conn)
    .optional()
    .map_err(|error| misplaced(error.to_string()))?;
    match found {
        Some(found) if found.name == expected => Ok(()),
        Some(found) => Err(misplaced(format!(
            "the identity row names {}, not {expected}",
            found.name
        ))),
        None => Err(misplaced("the identity row is missing".to_owned())),
    }
}

/// Make the lake table, or add the columns it does not have yet. The
/// columns have no type, so each value keeps its storage class.
fn add_lake_table(
    conn: &mut SqliteConnection,
    table: &str,
    columns: &[String],
) -> Result<(), diesel::result::Error> {
    let existing: Vec<String> = diesel::sql_query("SELECT name FROM pragma_table_info(?, 'main')")
        .bind::<diesel::sql_types::Text, _>(table)
        .load::<Name>(conn)?
        .into_iter()
        .map(|column| column.name.to_ascii_lowercase())
        .collect();
    let quoted = quote_ident(table);
    if existing.is_empty() {
        let list = std::iter::once(TENANT_COLUMN)
            .chain(columns.iter().map(String::as_str))
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(", ");
        diesel::sql_query(format!("CREATE TABLE main.{quoted} ({list})")).execute(conn)?;
        return Ok(());
    }
    for column in columns {
        if !existing.contains(&column.to_ascii_lowercase()) {
            diesel::sql_query(format!(
                "ALTER TABLE main.{quoted} ADD COLUMN {}",
                quote_ident(column)
            ))
            .execute(conn)?;
        }
    }
    Ok(())
}

/// Quote an identifier for `SQLite`.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ── Snapshot and query ──────────────────────────────────────────────────────

/// The files of a lake. The last owner removes them, so a query still
/// running keeps them.
#[derive(Debug)]
struct LakeFiles {
    dir: PathBuf,
    db: PathBuf,
}

impl Drop for LakeFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A loaded lake. Dropping it removes its scratch directory, after the
/// last running query.
#[derive(Debug)]
pub struct LakeSnapshot {
    files: Arc<LakeFiles>,
    tenants: Vec<LakeTenant>,
    skipped: Vec<LakeSkip>,
    /// The canonical fleet root, when known.
    root: Option<PathBuf>,
}

impl LakeSnapshot {
    /// The tenants in the lake, sorted by id.
    #[must_use]
    pub fn tenants(&self) -> &[LakeTenant] {
        &self.tenants
    }

    /// The databases the lake did not load, sorted by name.
    #[must_use]
    pub fn skipped(&self) -> &[LakeSkip] {
        &self.skipped
    }

    /// The lake database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.files.db
    }

    /// Run one `SELECT`, `WITH` or `VALUES` statement.
    ///
    /// # Errors
    ///
    /// [`LakeError::Query`].
    pub async fn query(&self, sql: &str) -> Result<LakeRows, LakeError> {
        let (files, sql) = (Arc::clone(&self.files), sql.to_owned());
        crate::time::spawn_blocking(move || collect_rows(&files.db, &sql))
            .await
            .map_err(|error| LakeError::Task(error.to_string()))?
    }

    /// The column names of `sql`, with no rows read. Blocking.
    ///
    /// # Errors
    ///
    /// [`LakeError::Query`].
    pub fn columns(&self, sql: &str) -> Result<Vec<String>, LakeError> {
        columns_of(&mut read_only(&self.files.db)?, sql)
    }

    /// Run one statement and give each row to `each`. Blocking. Returns
    /// the number of rows given.
    ///
    /// # Errors
    ///
    /// [`LakeError::Query`].
    pub fn for_each_row(
        &self,
        sql: &str,
        each: impl FnMut(&[String], Vec<LakeValue>) -> ControlFlow<()>,
    ) -> Result<usize, LakeError> {
        for_each_row(&self.files.db, sql, each)
    }

    /// Write a standalone copy of the lake to `path` (owner-only on unix).
    /// `path` must not exist. The copy holds the rows of every tenant.
    /// Blocking.
    ///
    /// # Errors
    ///
    /// [`LakeError::Persist`] when `path` exists or cannot be written.
    /// [`LakeError::UnderRoot`] when `path` is under the fleet root.
    pub fn persist(&self, path: &Path) -> Result<(), LakeError> {
        let failed = |detail: String| LakeError::Persist(format!("{}: {detail}", path.display()));
        let name = path
            .file_name()
            .ok_or_else(|| failed("no file name".to_owned()))?;
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let parent = parent
            .canonicalize()
            .map_err(|error| failed(error.to_string()))?;
        refuse_under(&parent, self.root.as_deref())?;
        let target = parent.join(name);
        // A URI, so SQLite writes this file: a name like `file:x` is a name.
        let target_uri = crate::replication::file_uri(&target)
            .ok_or_else(|| failed("the path is not valid Unicode".to_owned()))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        drop(
            options
                .open(&target)
                .map_err(|error| failed(error.to_string()))?,
        );
        let written = read_only(&self.files.db).and_then(|mut conn| {
            diesel::sql_query("VACUUM INTO ?")
                .bind::<diesel::sql_types::Text, _>(target_uri)
                .execute(&mut conn)
                .map(|_| ())
                .map_err(|error| LakeError::Query(error.to_string()))
        });
        if written.is_err() {
            let _ = std::fs::remove_file(&target);
        }
        written.map_err(|error| failed(error.to_string()))
    }
}

/// A read-only connection to the lake.
fn read_only(db: &Path) -> Result<SqliteConnection, LakeError> {
    super::establish_sqlite_migration_connection(&uri(db, "mode=ro").map_err(LakeError::Query)?)
        .map_err(|error| LakeError::Query(error.to_string()))
}

fn collect_rows(db: &Path, sql: &str) -> Result<LakeRows, LakeError> {
    let mut out = LakeRows::default();
    for_each_row(db, sql, |columns, row| {
        if out.columns.is_empty() {
            out.columns = columns.to_vec();
        }
        out.rows.push(row);
        ControlFlow::Continue(())
    })?;
    if out.columns.is_empty() {
        out.columns = columns_of(&mut read_only(db)?, sql)?;
    }
    Ok(out)
}

/// Make the result view for `sql` and return its columns. The view holds
/// one statement: a trailing statement does not run, and it is refused.
fn columns_of(conn: &mut SqliteConnection, sql: &str) -> Result<Vec<String>, LakeError> {
    let query = |error: diesel::result::Error| LakeError::Query(error.to_string());
    let body = format!("SELECT * FROM (\n{}\n)", statement(sql));
    diesel::sql_query(format!("CREATE TEMP VIEW {RESULT_VIEW} AS {body}"))
        .execute(conn)
        .map_err(query)?;
    let stored: Name =
        diesel::sql_query("SELECT sql AS name FROM temp.sqlite_schema WHERE name = ?")
            .bind::<diesel::sql_types::Text, _>(RESULT_VIEW)
            .get_result(conn)
            .map_err(query)?;
    if !stored.name.ends_with(&body) {
        return Err(LakeError::Query(
            "the lake runs one SELECT, WITH or VALUES statement".to_owned(),
        ));
    }
    Ok(diesel::sql_query(format!(
        "SELECT name FROM pragma_table_info('{RESULT_VIEW}', 'temp')"
    ))
    .load::<Name>(conn)
    .map_err(query)?
    .into_iter()
    .map(|column| column.name)
    .collect())
}

/// `sql` with no trailing `;`, white space or comments. It reads quotes
/// and comments, so a `;` or a `--` in a literal stays.
fn statement(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    // The end of the last byte that is code, not space, `;` or a comment.
    let mut end = 0;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            quote @ (b'\'' | b'"' | b'`' | b'[') => {
                let close = if quote == b'[' { b']' } else { quote };
                at += 1;
                while at < bytes.len() {
                    if bytes[at] == close {
                        // A doubled quote is one quote in the literal.
                        if close != b']' && bytes.get(at + 1) == Some(&close) {
                            at += 2;
                            continue;
                        }
                        break;
                    }
                    at += 1;
                }
                at = (at + 1).min(bytes.len());
                end = at;
            }
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
            }
            byte if byte == b';' || byte.is_ascii_whitespace() => at += 1,
            _ => {
                at += 1;
                end = at;
            }
        }
    }
    // `end` follows an ASCII byte or the end of a literal, so it is a char
    // boundary.
    sql.get(..end).unwrap_or(sql)
}

#[derive(diesel::QueryableByName)]
struct Encoded {
    #[diesel(sql_type = diesel::sql_types::Text)]
    row: String,
}

fn for_each_row(
    db: &Path,
    sql: &str,
    mut each: impl FnMut(&[String], Vec<LakeValue>) -> ControlFlow<()>,
) -> Result<usize, LakeError> {
    let query = |detail: String| LakeError::Query(detail);
    let mut conn = read_only(db)?;
    let columns = columns_of(&mut conn, sql)?;
    conn.batch_execute("PRAGMA query_only = ON")
        .map_err(|error| query(error.to_string()))?;
    // `quote()` keeps the storage class and round-trips a real. Text goes
    // as `T` and hex: `quote()` stops at a NUL. A tab joins the arrays:
    // JSON escapes a tab inside a string.
    let arrays = columns
        .chunks(COLUMNS_PER_ARRAY)
        .map(|chunk| {
            let quoted = chunk
                .iter()
                .map(|column| {
                    let column = quote_ident(column);
                    format!(
                        "CASE typeof({column}) WHEN 'text' THEN 'T' || hex({column}) \
                         ELSE quote({column}) END"
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("json_array({quoted})")
        })
        .collect::<Vec<_>>()
        .join(" || char(9) || ");
    // MATERIALIZED: each value is computed once, so its type and its bytes
    // agree even for `random()`.
    let statement = format!(
        "WITH _autumn_lake_rows AS MATERIALIZED (SELECT * FROM temp.{RESULT_VIEW}) \
         SELECT {arrays} AS row FROM _autumn_lake_rows"
    );
    let rows = diesel::sql_query(statement)
        .load_iter::<Encoded, diesel::connection::DefaultLoadingMode>(&mut conn)
        .map_err(|error| query(error.to_string()))?;
    let mut given = 0;
    for row in rows {
        let row = row.map_err(|error| query(error.to_string()))?;
        let mut values = Vec::with_capacity(columns.len());
        for part in row.row.split('\t') {
            let literals: Vec<String> =
                serde_json::from_str(part).map_err(|error| query(error.to_string()))?;
            for literal in literals {
                values.push(parse_literal(&literal).map_err(query)?);
            }
        }
        given += 1;
        if each(&columns, values).is_break() {
            break;
        }
    }
    Ok(given)
}

/// Parse one `quote()` literal, or `T` and the hex of a text, into a value.
pub(crate) fn parse_literal(literal: &str) -> Result<LakeValue, String> {
    let bad = || format!("unexpected value {literal:?}");
    if literal == "NULL" {
        return Ok(LakeValue::Null);
    }
    if let Some(hex) = literal.strip_prefix('T') {
        let bytes = decode_hex(hex).ok_or_else(bad)?;
        return Ok(LakeValue::Text(String::from_utf8(bytes).unwrap_or_else(
            |error| String::from_utf8_lossy(error.as_bytes()).into_owned(),
        )));
    }
    if let Some(text) = literal.strip_prefix('\'') {
        let text = text.strip_suffix('\'').ok_or_else(bad)?;
        return Ok(LakeValue::Text(text.replace("''", "'")));
    }
    if let Some(hex) = literal.strip_prefix("X'") {
        let hex = hex.strip_suffix('\'').ok_or_else(bad)?;
        return decode_hex(hex).map(LakeValue::Blob).ok_or_else(bad);
    }
    if literal.contains(['.', 'e', 'E']) {
        return literal
            .parse::<f64>()
            .map(LakeValue::Real)
            .map_err(|_| bad());
    }
    literal
        .parse::<i64>()
        .map(LakeValue::Integer)
        .map_err(|_| bad())
}

/// Decode upper- or lowercase hex. `None` for an odd length or a bad digit.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !hex.is_ascii() {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests;
