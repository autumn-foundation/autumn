//! Cross-tenant analytics over the replicas of a fleet (ADR 0032, issue
//! #3236).
//!
//! The lake reads object storage, not the volume. It never opens a file
//! under the fleet root, so it runs on a host with no volume.
//!
//! ```text
//! list    ─► one listing of <base>/fleet/ (tenants, markers, generations)
//! decide  ─► a tombstone row or marker covers the generation: skip it
//! has     ─► the sink holds this state: keep it, restore nothing
//! restore ─► N workers restore N members into a private scratch directory
//! check   ─► the identity of each restored member names its key
//! add     ─► in key order: the sink takes each restored member
//! finish  ─► the sink gives its lake
//! ```
//!
//! A generation has one of two formats ([`LakeFormat`]). WAL shipping
//! commits a generation with `snapshot.json`. Core restores a `SQLite` file
//! and reads its identity row. Generation shipping commits with
//! `manifest.json`. Core downloads the files of the manifest and checks
//! each hash. The sink reads the identity ([`LakeSink::check_identity`]).
//! When a replica has generations of the two formats, core uses the newer
//! one. It compares the epoch first, and then the time.
//!
//! Core runs the inclusion rule (list, decide). Then it asks the sink
//! `has`, restores, and checks the identity of each restored member. A
//! [`LakeSink`] builds the lake (ADR 0040 §6). [`FleetLake::load`] runs any
//! sink. [`FleetLake::materialize`] runs the `SQLite` sink: it copies each
//! table with an `_autumn_tenant` column into one lake file, and
//! [`LakeSnapshot::query`] runs one `SELECT` on it.
//!
//! Each tenant is consistent at its own snapshot. The lake has no atomic
//! snapshot across tenants. The SQL is operator SQL: do not pass request
//! input to it.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Timestamp::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::time::Timestamp;

use crate::entropy::Entropy;
use crate::fleet_layout::{FleetDbKey, TenantDbId};
use crate::replication::destination::ReplicaDestination;
use crate::replication::restore::{self, RestoreError, RestorePlan};
use crate::replication::segment;

use super::fleet::generations::{self, Manifest};
use super::fleet_replication::replica_root;
use super::fleet_tombstones::{FleetTombstones, MARKER_OBJECT, Marker, restore_allowed};

mod sqlite;

pub use sqlite::{LakeRows, LakeSnapshot, LakeValue};

use sqlite::{LakeFiles, SqliteSink, check_identity, collect_rows};
#[cfg(test)]
use sqlite::{parse_literal, quote_ident, statement};
#[cfg(test)]
use std::ops::ControlFlow;

/// The column the `SQLite` lake adds to each table: the tenant id of the
/// row.
pub const TENANT_COLUMN: &str = "_autumn_tenant";

/// The `SQLite` lake table with one row per loaded tenant: `tenant`,
/// `generation`, `effective` (RFC 3339) and `effective_ms`.
pub const MANIFEST_TABLE: &str = "_autumn_lake_tenants";

/// Members restored at the same time when the caller sets no count.
pub const DEFAULT_LAKE_WORKERS: usize = 4;

/// Tombstone rows read per page.
const TOMBSTONE_PAGE: usize = 500;

// ── Seam ────────────────────────────────────────────────────────────────────

/// The replica state of one member: a generation and the WAL position in
/// it. Two loads restore the same data when their states are equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LakeGeneration {
    /// The generation id.
    pub id: String,
    /// The last WAL segment that the restore applies after the snapshot,
    /// as (WAL index, sequence). `None`: the snapshot only.
    pub segment: Option<(u32, u64)>,
    /// The ship time of the last segment, or of the snapshot when there is
    /// no segment.
    #[serde(with = "crate::time::compat::serde_autosi")]
    pub effective: Timestamp,
}

impl LakeGeneration {
    /// The state that `plan` restores.
    fn of(plan: &MemberPlan) -> Self {
        match plan {
            MemberPlan::Wal(plan) => Self {
                id: plan.generation.clone(),
                segment: plan
                    .segments
                    .last()
                    .map(|segment| (segment.index, segment.seq)),
                effective: plan.effective,
            },
            MemberPlan::Generations { manifest, .. } => Self {
                id: manifest.generation.clone(),
                segment: None,
                effective: plan.started_at(),
            },
        }
    }
}

/// How the replica of a member is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LakeFormat {
    /// `SQLite` WAL shipping: a snapshot and WAL segments. Core restores a
    /// checkpointed `SQLite` file and checks its identity row.
    Wal,
    /// Generation shipping (`ShipMode::Generations`): the files of one
    /// manifest. Core restores the engine's snapshot entry, one file or one
    /// directory. The sink checks its identity
    /// ([`LakeSink::check_identity`]).
    Generations,
}

/// One included member, given to [`LakeSink::add`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct LakeMember<'a> {
    /// The database. A tenant key carries its routing slot.
    pub key: &'a FleetDbKey,
    /// The replica state restored.
    pub generation: &'a LakeGeneration,
    /// The format of the replica.
    pub format: LakeFormat,
    /// The restored database. [`LakeFormat::Wal`]: a `SQLite` file,
    /// checkpointed, with no sidecars. [`LakeFormat::Generations`]: the
    /// snapshot entry of the engine, with the file name it had on the
    /// volume. Its identity names `key`. Core removes it after `add`
    /// returns.
    pub file: &'a Path,
}

/// Builds a lake from the members that core includes (ADR 0040 §6).
///
/// Core runs the inclusion rule first. Then it calls the sink in two
/// passes, each in key order:
///
/// 1. Core calls [`has`](Self::has) for each included member. When it is
///    `true`, core calls [`keep`](Self::keep) and restores nothing.
/// 2. Core restores each other member and calls [`add`](Self::add).
///
/// Then core calls [`finish`](Self::finish). Core never calls `has` for an
/// excluded member. When the load fails, core drops the sink and does not
/// call `finish`.
///
/// # Contract
///
/// - A sink that keeps its lake between loads drops, at `finish`, each
///   member it got from neither `add` nor `keep`. A deleted member is
///   gone then. So is a member that failed in this load.
/// - When `add` or `keep` fails, leave no part of that member.
/// - Return [`LakeError::Ingest`] for a member that fails. With
///   [`FleetLake::skip_failed_tenants`], core skips the member and loads
///   the rest. Other errors that do not name a member fail the load.
/// - Keep the catalog in files under the sink's own prefix, for example a
///   manifest per member and generation. Do not write under
///   `<base>/fleet/`: core lists it. Core adds no lake catalog table to the
///   control database.
pub trait LakeSink: Send + 'static {
    /// What [`finish`](Self::finish) gives.
    type Lake: Send + 'static;

    /// Set to `true` to get slot databases. A slot database holds many
    /// tenants, so its key names no tenant. When `false`, core skips slot
    /// databases, and refuses a fleet of slot databases only
    /// ([`LakeError::SlotFleet`]).
    const TAKES_SLOTS: bool = false;

    /// Take one restored member.
    ///
    /// # Errors
    ///
    /// [`LakeError::Ingest`] when the member fails.
    fn add(&mut self, member: LakeMember<'_>) -> Result<(), LakeError>;

    /// Check that the restored `member` is the database of its key. Core
    /// calls it before [`add`](Self::add) for each
    /// [`LakeFormat::Generations`] member. Core checks a
    /// [`LakeFormat::Wal`] member itself.
    ///
    /// The default refuses the member: only a sink for the engine can read
    /// the identity in its files.
    ///
    /// # Errors
    ///
    /// [`LakeError::Misplaced`] when the member names another database.
    /// [`LakeError::Ingest`] when the sink cannot read the member.
    fn check_identity(&self, member: LakeMember<'_>) -> Result<(), LakeError> {
        Err(LakeError::Ingest {
            tenant: member_name(member.key),
            detail: "the fleet ships generations, and this sink cannot read their identity. \
                     Load it with a sink for the fleet's engine"
                .to_owned(),
        })
    }

    /// Return `true` when the sink holds this state of the member. Core
    /// then calls [`keep`](Self::keep) and does not restore the member.
    fn has(&self, key: &FleetDbKey, generation: &LakeGeneration) -> bool {
        let _ = (key, generation);
        false
    }

    /// Keep the member that [`has`](Self::has) holds.
    ///
    /// # Errors
    ///
    /// [`LakeError::Ingest`] when the member fails.
    fn keep(&mut self, key: &FleetDbKey, generation: &LakeGeneration) -> Result<(), LakeError> {
        let _ = (key, generation);
        Ok(())
    }

    /// Give the lake after the last member.
    ///
    /// # Errors
    ///
    /// The sink cannot complete the lake.
    fn finish(self) -> Result<Self::Lake, LakeError>;
}

/// One member in a load.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LakeEntry {
    /// The database.
    pub key: FleetDbKey,
    /// Its replica state.
    pub generation: LakeGeneration,
    /// Core restored it in this load. `false`: the sink kept it.
    pub restored: bool,
}

/// The result of [`FleetLake::load`].
#[derive(Debug)]
#[non_exhaustive]
pub struct LakeLoad<L> {
    /// What the sink gave.
    pub lake: L,
    /// The members in the lake, sorted by key.
    pub members: Vec<LakeEntry>,
    /// The databases not in the lake, sorted by name.
    pub skipped: Vec<LakeSkip>,
}

/// One tenant in the lake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LakeTenant {
    /// The tenant.
    pub tenant: TenantDbId,
    /// The replica generation the lake restored.
    pub generation: String,
    /// The instant the restored state is current as of.
    pub effective: Timestamp,
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
    /// A slot database, and the sink takes no slots.
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
    /// The destination holds slot databases only, and the sink takes no
    /// slots.
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

/// A read-only view of a fleet's replicas, loaded on demand into a
/// [`LakeSink`]. See the module docs.
#[derive(Clone)]
pub struct FleetLake {
    destination: Arc<dyn ReplicaDestination>,
    base: String,
    tombstones: Option<Arc<FleetTombstones>>,
    markers_only: bool,
    skip_failed: bool,
    as_of: Option<Timestamp>,
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
    pub const fn as_of(mut self, at: Timestamp) -> Self {
        self.as_of = Some(at);
        self
    }

    /// Load only these tables into the `SQLite` lake. A sink of
    /// [`load`](Self::load) gets the full restored file.
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

    /// Restore every live tenant and load it into a new `SQLite` lake.
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

    /// Run the inclusion rule and give each included member to `sink`. The
    /// restored files go in a scratch directory that the load removes.
    ///
    /// # Errors
    ///
    /// See [`LakeError`]. A member that fails fails the load, unless
    /// [`skip_failed_tenants`](Self::skip_failed_tenants) is set.
    pub async fn load<S: LakeSink>(&self, sink: S) -> Result<LakeLoad<S::Lake>, LakeError> {
        let rows = self.tombstone_rows().await?;
        let lake = self.clone();
        crate::time::spawn_blocking(move || {
            let (scratch, _) = lake.new_scratch()?;
            lake.load_blocking(&scratch.dir, &rows, sink)
        })
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

    /// A new scratch directory, and the canonical fleet root.
    fn new_scratch(&self) -> Result<(Scratch, Option<PathBuf>), LakeError> {
        let root = self
            .forbidden_root
            .as_deref()
            .and_then(|root| root.canonicalize().ok());
        let scratch = Scratch::create(
            &self.scratch.clone().unwrap_or_else(std::env::temp_dir),
            root.as_deref(),
            self.entropy.as_ref(),
        )?;
        Ok((scratch, root))
    }

    fn materialize_blocking(&self, rows: &HashSet<String>) -> Result<LakeSnapshot, LakeError> {
        let (scratch, root) = self.new_scratch()?;
        let sink = SqliteSink::create(scratch.dir.join("lake.db"), self.tables.clone())?;
        let load = self.load_blocking(&scratch.dir, rows, sink)?;
        let tenants = load
            .members
            .into_iter()
            .filter_map(|member| {
                Some(LakeTenant {
                    tenant: member.key.tenant()?.clone(),
                    generation: member.generation.id,
                    effective: member.generation.effective,
                })
            })
            .collect();
        Ok(LakeSnapshot {
            files: Arc::new(LakeFiles {
                dir: scratch.keep(),
                db: load.lake,
            }),
            tenants,
            skipped: load.skipped,
            root,
        })
    }

    /// The inclusion rule. Restored files go in `dir`.
    fn load_blocking<S: LakeSink>(
        &self,
        dir: &Path,
        rows: &HashSet<String>,
        mut sink: S,
    ) -> Result<LakeLoad<S::Lake>, LakeError> {
        let found = discover(self.destination.as_ref(), &self.base, S::TAKES_SLOTS)?;
        let mut skipped = found.skipped;
        let mut members = Vec::new();
        let mut planned = Vec::new();
        for (key, keys) in &found.members {
            match self.plan_member(key, keys, rows) {
                Ok(Planned::Restore(plan)) => {
                    let generation = LakeGeneration::of(&plan);
                    if !sink.has(key, &generation) {
                        planned.push((key.clone(), *plan, generation));
                        continue;
                    }
                    match sink.keep(key, &generation) {
                        Ok(()) => members.push(LakeEntry {
                            key: key.clone(),
                            generation,
                            restored: false,
                        }),
                        Err(error) => skipped.push(self.member_failed(key, error)?),
                    }
                }
                Ok(Planned::Skip(reason)) => skipped.push(LakeSkip {
                    name: member_name(key),
                    reason,
                }),
                Err(error) => skipped.push(self.failed(error)?),
            }
        }

        for (chunk_index, chunk) in planned.chunks(self.workers).enumerate() {
            let restored = self.restore_chunk(dir, chunk_index, chunk);
            for ((key, plan, generation), (output, result)) in chunk.iter().zip(restored) {
                let name = member_name(key);
                let member = LakeMember {
                    key,
                    generation,
                    format: plan.format(),
                    file: &output.file,
                };
                let loaded = result
                    .map_err(|detail| LakeError::Restore {
                        tenant: name.clone(),
                        detail,
                    })
                    .and_then(|()| match member.format {
                        LakeFormat::Wal => check_identity(key, &name, member.file),
                        LakeFormat::Generations => sink.check_identity(member),
                    })
                    .and_then(|()| sink.add(member));
                output.remove();
                match loaded {
                    Ok(()) => members.push(LakeEntry {
                        key: key.clone(),
                        generation: generation.clone(),
                        restored: true,
                    }),
                    Err(error) => skipped.push(self.member_failed(key, error)?),
                }
            }
        }
        members.sort_by(|a, b| a.key.cmp(&b.key));
        skipped.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(LakeLoad {
            lake: sink.finish()?,
            members,
            skipped,
        })
    }

    /// An error of `key` after its plan: a skip named from the key with
    /// `skip_failed_tenants`, else the error.
    fn member_failed(&self, key: &FleetDbKey, error: LakeError) -> Result<LakeSkip, LakeError> {
        match error.tenant() {
            Some(_) if self.skip_failed => Ok(LakeSkip {
                name: member_name(key),
                reason: SkipReason::Failed(error.to_string()),
            }),
            _ => Err(error),
        }
    }

    /// A per-member error: a skip with `skip_failed_tenants`, else the
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
    /// again for this member, once.
    fn plan_member(
        &self,
        key: &FleetDbKey,
        keys: &[String],
        rows: &HashSet<String>,
    ) -> Result<Planned, LakeError> {
        let root = replica_root(&self.base, key);
        let row = rows.contains(&key.name());
        if let Attempt::Done(planned) = self.plan_from(key, &root, keys, row)? {
            return Ok(planned);
        }
        let fresh = self
            .destination
            .list(&format!("{root}/"))
            .map_err(|error| LakeError::Destination(error.to_string()))?;
        match self.plan_from(key, &root, &fresh, row)? {
            Attempt::Done(planned) => Ok(planned),
            Attempt::Stale(detail) => Err(LakeError::Restore {
                tenant: member_name(key),
                detail,
            }),
        }
    }

    /// One decision and plan from `keys`.
    fn plan_from(
        &self,
        key: &FleetDbKey,
        root: &str,
        keys: &[String],
        row: bool,
    ) -> Result<Attempt, LakeError> {
        let marker = match self.marker(key, root, keys)? {
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
        // The newest generation of each format at or before the target.
        let target_ms = self.as_of.map_or(i64::MAX, Timestamp::as_millisecond);
        let manifest = generations::complete_generations(root, keys)
            .into_iter()
            .rev()
            .find(|generation| generation.created_ms <= target_ms);
        let wal = restore::complete_snapshots(root, keys)
            .into_iter()
            .rev()
            .find(|(_, created_ms, _)| *created_ms <= target_ms);
        let format = newer_format(
            wal.map(|(epoch, created_ms, _)| (epoch, created_ms)),
            manifest.as_ref().map(|g| (g.epoch, g.created_ms)),
        );
        let plan = match (format, manifest) {
            // `decide` saw a generation, so each one is after the target.
            (None, _) => return skip(SkipReason::AbsentAtTarget),
            (Some(LakeFormat::Generations), Some(chosen)) => {
                match self.plan_generations(key, root, &chosen)? {
                    Ok(plan) => plan,
                    Err(stale) => return Ok(Attempt::Stale(stale)),
                }
            }
            // `newer_format` gives `Generations` only with a manifest.
            (Some(LakeFormat::Wal | LakeFormat::Generations), _) => {
                match restore::plan_from_listing(self.destination.as_ref(), root, keys, self.as_of)
                {
                    Ok(plan) => MemberPlan::Wal(plan),
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
                            tenant: member_name(key),
                            detail: error.to_string(),
                        });
                    }
                }
            }
        };
        // `as_of` can pick an older generation than the newest.
        if !restore_allowed(row, marker, plan.started_at()) {
            return skip(SkipReason::Deleted);
        }
        Ok(Attempt::Done(Planned::Restore(Box::new(plan))))
    }

    /// Read and check the manifest of `chosen`. The inner `Err` tells why
    /// the manifest is gone since the listing, for example after a prune.
    fn plan_generations(
        &self,
        key: &FleetDbKey,
        root: &str,
        chosen: &generations::Complete,
    ) -> Result<Result<MemberPlan, String>, LakeError> {
        let id = chosen.id.as_str();
        let failed = |detail: String| LakeError::Restore {
            tenant: member_name(key),
            detail,
        };
        let body = match self.destination.get(&generations::manifest_key(root, id)) {
            Ok(body) => body,
            Err(error) if error.is_not_found() => {
                return Ok(Err(format!(
                    "the manifest of generation {id} is gone since the listing"
                )));
            }
            Err(error) => return Err(failed(error.to_string())),
        };
        let manifest = generations::read_manifest(&body, id).map_err(failed)?;
        Ok(Ok(MemberPlan::Generations {
            root: root.to_owned(),
            manifest,
            created: segment::timestamp_from_ms(chosen.created_ms),
        }))
    }

    /// The member's marker, read only when the listing has one.
    fn marker(
        &self,
        key: &FleetDbKey,
        root: &str,
        keys: &[String],
    ) -> Result<MarkerRead, LakeError> {
        let marker_key = format!("{root}/{MARKER_OBJECT}");
        if !keys.contains(&marker_key) {
            return Ok(MarkerRead::Absent);
        }
        let unreadable = |detail: String| LakeError::Marker {
            tenant: member_name(key),
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

    /// Restore one chunk of members, one thread each. Blocking.
    fn restore_chunk(
        &self,
        dir: &Path,
        chunk_index: usize,
        chunk: &[(FleetDbKey, MemberPlan, LakeGeneration)],
    ) -> Vec<(Output, Result<(), String>)> {
        let destination = self.destination.as_ref();
        std::thread::scope(|scope| {
            // Start every thread before the first join.
            #[allow(clippy::needless_collect, reason = "spawn all, then join")]
            let handles: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(index, (_, plan, _))| {
                    let output = Output::of(plan, dir, &format!("{chunk_index:08}-{index:04}"));
                    let handle = std::thread::Builder::new()
                        .name("autumn-fleet-lake".to_owned())
                        .spawn_scoped(scope, {
                            let file = output.file.clone();
                            move || plan.restore(destination, &file)
                        });
                    (output, plan, handle)
                })
                .collect();
            handles
                .into_iter()
                .map(|(output, plan, handle)| {
                    // No thread: restore on this one.
                    let result = handle.map_or_else(
                        |_| plan.restore(destination, &output.file),
                        |handle| {
                            handle
                                .join()
                                .unwrap_or_else(|_| Err("the restore thread panicked".to_owned()))
                        },
                    );
                    (output, result)
                })
                .collect()
        })
    }
}

/// The name of a member in a [`LakeSkip`] and in errors: the tenant id, or
/// the slot number.
fn member_name(key: &FleetDbKey) -> String {
    match key {
        FleetDbKey::Tenant { id, .. } => id.to_string(),
        FleetDbKey::Slot(slot) => format!("{slot:05}"),
    }
}

/// What the lake does with one member.
enum Planned {
    Restore(Box<MemberPlan>),
    Skip(SkipReason),
}

/// The restore of one member, by the format of its replica.
enum MemberPlan {
    /// A WAL generation: the snapshot and its segments.
    Wal(RestorePlan),
    /// A generation of `ShipMode::Generations`: the files of `manifest`.
    Generations {
        /// The replica root of the member.
        root: String,
        /// The checked manifest.
        manifest: Manifest,
        /// When the generation was made, from its id.
        created: Timestamp,
    },
}

impl MemberPlan {
    const fn format(&self) -> LakeFormat {
        match self {
            Self::Wal(_) => LakeFormat::Wal,
            Self::Generations { .. } => LakeFormat::Generations,
        }
    }

    /// When the generation was made. The tombstone rule reads it.
    const fn started_at(&self) -> Timestamp {
        match self {
            Self::Wal(plan) => plan.generation_started_at,
            Self::Generations { created, .. } => *created,
        }
    }

    /// The entry name of the restored database: the name on the volume
    /// for generations. A WAL restore makes one `SQLite` file.
    fn entry_name(&self) -> Option<&str> {
        match self {
            Self::Wal(_) => None,
            Self::Generations { manifest, .. } => manifest
                .files
                .first()
                .and_then(|file| file.path.split('/').next()),
        }
    }

    /// Restore the member at `file`. Blocking.
    fn restore(&self, destination: &dyn ReplicaDestination, file: &Path) -> Result<(), String> {
        match self {
            Self::Wal(plan) => restore::apply(destination, plan, file)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Self::Generations { root, manifest, .. } => {
                generations::download(destination, root, manifest, file).map(|_| ())
            }
        }
    }
}

/// Where one member is restored.
struct Output {
    /// The restored database.
    file: PathBuf,
    /// The private directory of a generations member. `None`: a WAL file.
    dir: Option<PathBuf>,
}

impl Output {
    /// The output of `plan` in `dir`, named from `stem`.
    fn of(plan: &MemberPlan, dir: &Path, stem: &str) -> Self {
        plan.entry_name().map_or_else(
            || Self {
                file: dir.join(format!("{stem}.db")),
                dir: None,
            },
            // A directory of its own keeps the name the engine gave.
            |name| {
                let own = dir.join(stem);
                Self {
                    file: own.join(name),
                    dir: Some(own),
                }
            },
        )
    }

    /// Remove the restored database and its files.
    fn remove(&self) {
        match &self.dir {
            Some(dir) => {
                let _ = std::fs::remove_dir_all(dir);
            }
            None => remove_database(&self.file),
        }
    }
}

/// One try of [`FleetLake::plan_member`].
enum Attempt {
    Done(Planned),
    /// The listing is older than the replica. Why.
    Stale(String),
}

/// The tombstone marker of a member.
enum MarkerRead {
    /// The listing has none.
    Absent,
    /// The listing has one, and it is gone now.
    Gone,
    /// Its `deleted_at`.
    At(Timestamp),
}

/// The databases one listing of `<base>/fleet/` finds.
struct Discovery {
    /// Each member and every key under its root, sorted by key.
    members: BTreeMap<FleetDbKey, Vec<String>>,
    /// Invalid names, and slot databases when the sink takes no slots.
    skipped: Vec<LakeSkip>,
}

/// List `<base>/fleet/` once and group the keys by database. `slots`: the
/// sink takes slot databases.
fn discover(
    destination: &dyn ReplicaDestination,
    base: &str,
    slots: bool,
) -> Result<Discovery, LakeError> {
    let prefix = format!("{base}/fleet/");
    let keys = destination
        .list(&prefix)
        .map_err(|error| LakeError::Destination(error.to_string()))?;
    let mut members: BTreeMap<FleetDbKey, Vec<String>> = BTreeMap::new();
    let mut invalid = BTreeSet::new();
    let mut refused = BTreeSet::new();
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
        let member = match kind {
            "tenant" => TenantDbId::parse(name).ok().map(tenant_key),
            "slot" if slots => slot_key(name),
            "slot" => {
                refused.insert(name.to_owned());
                continue;
            }
            _ => continue,
        };
        match member {
            Some(member) => members.entry(member).or_default().push(key),
            None => {
                invalid.insert(name.to_owned());
            }
        }
    }
    if members.is_empty() && !refused.is_empty() {
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
        .chain(refused.into_iter().map(skip(SkipReason::SlotDatabase)))
        .collect();
    Ok(Discovery { members, skipped })
}

/// The key of a tenant, with its routing slot.
fn tenant_key(id: TenantDbId) -> FleetDbKey {
    let slot = crate::sharding::slot_for_key(id.as_str().into()).0;
    FleetDbKey::Tenant { id, slot }
}

/// The key of a slot directory name. Only the five-digit form that
/// [`replica_root`] writes is valid.
fn slot_key(name: &str) -> Option<FleetDbKey> {
    let slot = name.parse::<u16>().ok()?;
    let key = FleetDbKey::slot(slot).ok()?;
    (format!("{slot:05}") == name).then_some(key)
}

/// When the newest complete generation under `root` was created. A
/// generation is complete with its `snapshot.json` (WAL shipping) or its
/// `manifest.json` (generation shipping).
fn newest_generation(root: &str, keys: &[String]) -> Option<Timestamp> {
    keys.iter()
        .filter_map(|key| {
            let generation = segment::generation_of_key(root, key)?;
            let complete = key.ends_with(segment::SNAPSHOT_META_OBJECT)
                || *key == generations::manifest_key(root, &generation);
            complete.then_some(generation)
        })
        .filter_map(|generation| segment::parse_generation_id(&generation))
        .map(|info| segment::timestamp_from_ms(info.created_ms))
        .max()
}

/// The format of the newer of two generations, each as (epoch, created
/// ms). When the two are equal, core uses the manifest. `None` when there
/// is neither.
fn newer_format(wal: Option<(u64, i64)>, manifest: Option<(u64, i64)>) -> Option<LakeFormat> {
    match (wal, manifest) {
        (None, None) => None,
        (Some(_), None) => Some(LakeFormat::Wal),
        (Some(wal), Some(manifest)) if wal > manifest => Some(LakeFormat::Wal),
        (_, Some(_)) => Some(LakeFormat::Generations),
    }
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
pub(crate) fn decide(row: bool, marker: Option<Timestamp>, newest: Option<Timestamp>) -> Decision {
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod generations_tests;
