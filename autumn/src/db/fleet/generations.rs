//! Generation shipping of a fleet (`ShipMode::Generations`, ADR 0040 §5).
//!
//! Core ships the database. The engine only gives files (`snapshot`).
//!
//! ```text
//! <root>/epochs/<epoch:020>                      one per epoch that wrote
//! <root>/generations/<id>/epoch-<epoch:020>      the epoch of the generation
//! <root>/generations/<id>/files/<sha256>         files new in this generation
//! <root>/generations/<id>/manifest.json          written last: the commit
//! <root>/tombstone                               the delete marker
//! ```
//!
//! ```text
//! every `every`, and at close
//!   snapshot into scratch ─► hash each file
//!   same files as the previous manifest? ─► ship nothing
//!   fence ─► epochs/<n> ─► epoch-<n> ─► new files ─► manifest.json
//!   prune: keep the 2 newest, and each holder they name
//! restore
//!   newest complete generation by (epoch, time), the tombstone rules
//!   download, check each sha256 and length, publish
//! ```
//!
//! The model and its proofs are in `verification/fleet_generations.rs`.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate".
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::{
    DatabaseFleet, Drain, FleetDatabase, FleetDbKey, FleetEngine, FleetError, FleetLifecycle,
    Inner, Notify, Ordering, ShipReport, check_contained, discard_staging, staging_path,
    with_path_lock,
};
use crate::db::fleet_replication::{
    ClosedBound, FleetReplication, GenerationRetry, UnshippedGeneration,
};
use crate::db::fleet_tombstones::restore_allowed;
use crate::replication::destination::{DestinationError, ReplicaDestination};
use crate::replication::fence::FencedDestination;
use crate::replication::{segment, status};

/// The commit marker of a generation.
pub const MANIFEST_OBJECT: &str = "manifest.json";

/// The directory of a generation's files, by content hash.
pub const FILES_DIR: &str = "files";

/// The layout version of a [`Manifest`].
pub const MANIFEST_VERSION: u32 = 1;

/// Complete generations a prune keeps, with every holder they name.
const KEEP_GENERATIONS: usize = 2;

/// Where a ship writes its snapshot, under the fleet's reserved directory.
const SHIP_DIR: &str = "ship";

/// The commit marker of one generation: every file of the database.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// The generation id, from
    /// [`generation_id`](crate::replication::segment::generation_id).
    pub generation: String,
    /// When the generation was made, epoch milliseconds (app clock).
    pub created_ms: i64,
    /// The epoch of the writer. `None` without ownership.
    pub epoch: Option<u64>,
    /// Every file, in snapshot order.
    pub files: Vec<ManifestFile>,
}

/// One file of a [`Manifest`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManifestFile {
    /// The path in the snapshot directory, `/` separated. The first part is
    /// the file name of the database.
    pub path: String,
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    /// The length in bytes.
    pub len: u64,
    /// The generation that holds the content, at `files/<sha256>`.
    pub holder: String,
}

/// A file of a new snapshot.
#[derive(Debug, Clone)]
pub struct LocalFile {
    /// The path in the snapshot directory, `/` separated.
    pub path: String,
    /// Where the file is now.
    pub file: PathBuf,
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    /// The length in bytes.
    pub len: u64,
}

/// What a generation puts, and its manifest files.
#[derive(Debug)]
pub struct Planned<'a> {
    /// The files to put: one per new hash.
    pub put: Vec<&'a LocalFile>,
    /// Every file of the new manifest.
    pub files: Vec<ManifestFile>,
}

/// The plan of generation `id` over `prev`. `None` when the files are the
/// files of `prev`: nothing ships.
pub fn plan_ship<'a>(
    prev: Option<&Manifest>,
    files: &'a [LocalFile],
    id: &str,
) -> Option<Planned<'a>> {
    if let Some(prev) = prev {
        let old: HashSet<(&str, &str)> = prev
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.sha256.as_str()))
            .collect();
        let new: HashSet<(&str, &str)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.sha256.as_str()))
            .collect();
        if old == new && prev.files.len() == files.len() {
            return None;
        }
    }
    let mut held: HashMap<&str, &str> = prev
        .map(|prev| {
            prev.files
                .iter()
                .map(|f| (f.sha256.as_str(), f.holder.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let mut put = Vec::new();
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let known = held
            .get(file.sha256.as_str())
            .map(|holder| (*holder).to_owned());
        let holder = known.unwrap_or_else(|| {
            // A new hash: this generation holds it.
            held.insert(&file.sha256, id);
            put.push(file);
            id.to_owned()
        });
        out.push(ManifestFile {
            path: file.path.clone(),
            sha256: file.sha256.clone(),
            len: file.len,
            holder,
        });
    }
    Some(Planned { put, files: out })
}

/// A complete generation: its manifest is at the destination.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Complete {
    /// The epoch of the generation, 0 for none.
    pub epoch: u64,
    /// When the generation was made, epoch milliseconds.
    pub created_ms: i64,
    /// The generation id.
    pub id: String,
}

/// The complete generations in `keys`, oldest first: by epoch, then by
/// time, as `restore::plan` orders them.
pub fn complete_generations(root: &str, keys: &[String]) -> Vec<Complete> {
    let epochs = crate::replication::restore::generation_epochs(root, keys);
    let mut complete: Vec<Complete> = keys
        .iter()
        .filter(|key| key.ends_with(MANIFEST_OBJECT))
        .filter_map(|key| segment::generation_of_key(root, key))
        .filter(|id| keys.iter().any(|key| *key == manifest_key(root, id)))
        .filter_map(|id| {
            let info = segment::parse_generation_id(&id)?;
            Some(Complete {
                epoch: epochs.get(&id).copied().unwrap_or(0),
                created_ms: info.created_ms,
                id,
            })
        })
        .collect();
    complete.sort();
    complete.dedup();
    complete
}

/// The generations a prune keeps.
#[derive(Debug, Default)]
pub struct Keep {
    /// The ids of the generations to keep.
    pub ids: BTreeSet<String>,
    /// The (epoch, time) of the oldest kept generation of the newest. A
    /// prune removes only what is older.
    pub floor: Option<(u64, i64)>,
}

/// The newest `keep` complete generations, and every holder their
/// manifests name. `manifests` holds at least the manifests of the newest.
pub fn keep_set(complete: &[Complete], manifests: &HashMap<String, Manifest>, keep: usize) -> Keep {
    let newest = complete
        .get(complete.len().saturating_sub(keep)..)
        .unwrap_or_default();
    let mut ids: BTreeSet<String> = newest.iter().map(|g| g.id.clone()).collect();
    for generation in newest {
        if let Some(manifest) = manifests.get(&generation.id) {
            ids.extend(manifest.files.iter().map(|f| f.holder.clone()));
        }
    }
    Keep {
        ids,
        floor: newest.first().map(|g| (g.epoch, g.created_ms)),
    }
}

/// The keys a prune deletes: each key of a generation that is not kept and
/// is older than the floor. Manifests come first: a prune that stops part
/// way leaves no complete generation with missing files.
pub fn prunable(root: &str, keys: &[String], keep: &Keep) -> Vec<String> {
    let Some(floor) = keep.floor else {
        return Vec::new();
    };
    let epochs = crate::replication::restore::generation_epochs(root, keys);
    let mut doomed: Vec<String> = keys
        .iter()
        .filter(|key| {
            segment::generation_of_key(root, key).is_some_and(|id| {
                !keep.ids.contains(&id)
                    && segment::parse_generation_id(&id).is_some_and(|info| {
                        (epochs.get(&id).copied().unwrap_or(0), info.created_ms) < floor
                    })
            })
        })
        .cloned()
        .collect();
    doomed.sort_by_key(|key| !key.ends_with(MANIFEST_OBJECT));
    doomed
}

/// The key of the manifest of generation `id`.
#[must_use]
pub fn manifest_key(root: &str, id: &str) -> String {
    format!("{}{MANIFEST_OBJECT}", segment::generation_prefix(root, id))
}

/// The key of the file with hash `sha256` in generation `holder`.
#[must_use]
pub fn file_key(root: &str, holder: &str, sha256: &str) -> String {
    format!(
        "{}{FILES_DIR}/{sha256}",
        segment::generation_prefix(root, holder)
    )
}

/// Refuse a manifest path that can leave the snapshot: empty, absolute, a
/// `.`, `..` or empty part, or a backslash.
pub fn check_relative(path: &str) -> Result<(), String> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        Err(format!("the manifest path {path:?} is not a relative path"))
    } else {
        Ok(())
    }
}

/// Refuse a hash that is not 64 lowercase hex digits.
pub fn check_hash(sha256: &str) -> Result<(), String> {
    if sha256.len() == 64
        && sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(format!("the manifest hash {sha256:?} is not a SHA-256"))
    }
}

/// Refuse a set of snapshot paths that is not one database: two parts of
/// it must not share a path, every path starts with the same name, and a
/// one-part path (a database file) stands alone.
pub fn check_shape<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    let paths: Vec<&str> = paths.into_iter().collect();
    let mut seen = HashSet::new();
    let mut first = None;
    for path in &paths {
        if !seen.insert(*path) {
            return Err(format!("the snapshot has the path {path:?} twice"));
        }
        let name = path.split('/').next().unwrap_or_default();
        if *first.get_or_insert(name) != name {
            return Err(format!(
                "the snapshot holds more than one entry ({:?} and {name:?})",
                first.unwrap_or_default()
            ));
        }
    }
    if paths.len() > 1 && paths.iter().any(|path| !path.contains('/')) {
        return Err("the snapshot holds a database file and other files".to_owned());
    }
    Ok(())
}

/// Refuse a manifest of another layout, generation or shape.
fn check_manifest(manifest: &Manifest, id: &str) -> Result<(), String> {
    if manifest.version != MANIFEST_VERSION {
        return Err(format!(
            "manifest version {} is not the one this build reads ({MANIFEST_VERSION})",
            manifest.version
        ));
    }
    if manifest.generation != id {
        return Err(format!(
            "the manifest of generation {id} names generation {}",
            manifest.generation
        ));
    }
    if manifest.files.is_empty() {
        return Err(format!("the manifest of generation {id} has no file"));
    }
    for file in &manifest.files {
        check_relative(&file.path)?;
        check_hash(&file.sha256)?;
        if segment::parse_generation_id(&file.holder).is_none() {
            return Err(format!("the holder {:?} is not a generation", file.holder));
        }
    }
    check_shape(manifest.files.iter().map(|f| f.path.as_str()))
}

/// The SHA-256 and the length of `path`. Blocking.
fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    use sha2::{Digest as _, Sha256};

    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or(&[]));
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok((hex::encode(hasher.finalize()), total))
}

/// The files of a snapshot in `dir`, hashed. Blocking.
fn local_files(dir: &Path, files: &[PathBuf]) -> Result<Vec<LocalFile>, String> {
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let relative = file.strip_prefix(dir).map_err(|_| {
            format!(
                "the snapshot wrote {} outside its directory",
                file.display()
            )
        })?;
        let parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        let path = parts.join("/");
        check_relative(&path)?;
        let (sha256, len) =
            hash_file(file).map_err(|e| format!("could not read {}: {e}", file.display()))?;
        out.push(LocalFile {
            path,
            file: file.clone(),
            sha256,
            len,
        });
    }
    if out.is_empty() {
        return Err("the snapshot wrote no file".to_owned());
    }
    check_shape(out.iter().map(|f| f.path.as_str()))?;
    Ok(out)
}

/// The ship state of one open database. Its lock is held only for a short
/// read or write, so health never waits for an upload.
#[derive(Debug)]
pub struct GenerationTrack {
    /// When the database opened.
    pub registered_at: DateTime<Utc>,
    /// The last ship that worked, also one that found no change.
    pub last_ok: Option<DateTime<Utc>>,
    /// Every commit at or before it is in the replica: the instant before the
    /// snapshot of the last ship that worked. `None` after a fence.
    pub replicated_through: Option<DateTime<Utc>>,
    /// A ship found a newer epoch at the replica.
    pub fenced: bool,
    /// Failed ships in a row.
    pub failures: u32,
    /// The error of the last failed ship.
    pub last_error: Option<String>,
}

impl GenerationTrack {
    /// The track of a database that opened at `now`.
    pub const fn new(now: DateTime<Utc>) -> Self {
        Self {
            registered_at: now,
            last_ok: None,
            replicated_through: None,
            fenced: false,
            failures: 0,
            last_error: None,
        }
    }
}

/// The ship locks and the track of one open database.
#[derive(Debug)]
pub struct GenerationSlot {
    /// Held for a whole ship, the snapshot too: one ship at a time.
    turn: tokio::sync::Mutex<()>,
    /// Held for the blocking part of a ship. A delete waits on it.
    upload: Mutex<()>,
    track: Mutex<GenerationTrack>,
    /// The epoch of the open, 0 without ownership.
    epoch: u64,
}

impl GenerationSlot {
    /// The slot of a database that opened at `now` under `epoch` (0 without
    /// ownership).
    pub fn new(now: DateTime<Utc>, epoch: u64) -> Self {
        Self {
            turn: tokio::sync::Mutex::new(()),
            upload: Mutex::new(()),
            track: Mutex::new(GenerationTrack::new(now)),
            epoch,
        }
    }

    /// The epoch of the open, 0 without ownership.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The track. Hold it only for a short read or write.
    pub fn track(&self) -> std::sync::MutexGuard<'_, GenerationTrack> {
        self.track.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until no upload runs. Blocking.
    pub fn wait_upload(&self) {
        drop(self.upload.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// The newest complete generation of `key` with its manifest. Blocking.
///
/// `target`: the newest generation at or before it. `honor_marker`: a
/// tombstone marker that is not older than the generation refuses it
/// (`restore_allowed`). An explicit restore takes the row, and does not.
///
/// # Errors
///
/// The destination failed, or the manifest is unreadable.
fn newest(
    replication: &FleetReplication,
    key: &FleetDbKey,
    target: Option<DateTime<Utc>>,
    honor_marker: bool,
) -> Result<Option<(Complete, Manifest)>, String> {
    let root = replication.root_for(key);
    let keys = replication
        .destination()
        .list(&segment::generations_prefix(&root))
        .map_err(|e| e.to_string())?;
    let target_ms = target.map_or(i64::MAX, |t| t.timestamp_millis());
    let Some(chosen) = complete_generations(&root, &keys)
        .into_iter()
        .rev()
        .find(|g| g.created_ms <= target_ms)
    else {
        return Ok(None);
    };
    if honor_marker {
        let marker = replication.marker(key)?;
        let created = DateTime::from_timestamp_millis(chosen.created_ms).unwrap_or_default();
        if !restore_allowed(false, marker.map(|m| m.deleted_at()), created) {
            tracing::info!(database = %key, "fleet database was deleted: its generations are not restored");
            return Ok(None);
        }
    }
    let body = replication
        .destination()
        .get(&manifest_key(&root, &chosen.id))
        .map_err(|e| e.to_string())?;
    let manifest: Manifest =
        serde_json::from_slice(&body).map_err(|e| format!("unreadable manifest: {e}"))?;
    check_manifest(&manifest, &chosen.id)?;
    Ok(Some((chosen, manifest)))
}

/// Write the files of `manifest` at `target` (the first part of each path
/// becomes the file name of `target`). Each file is checked against its
/// hash and length. Blocking.
fn download(
    destination: &dyn ReplicaDestination,
    root: &str,
    manifest: &Manifest,
    target: &Path,
) -> Result<u64, String> {
    let parent = target.parent().ok_or("the target has no parent")?;
    let name = target.file_name().ok_or("the target has no file name")?;
    let mut bytes = 0_u64;
    for file in &manifest.files {
        let rest: Vec<&str> = file.path.split('/').skip(1).collect();
        let mut out = parent.join(name);
        for part in &rest {
            out.push(part);
        }
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        destination
            .get_to_file(&file_key(root, &file.holder, &file.sha256), &out)
            .map_err(|e| e.to_string())?;
        let (sha256, len) = hash_file(&out).map_err(|e| e.to_string())?;
        if sha256 != file.sha256 || len != file.len {
            return Err(format!(
                "{} does not match its manifest (sha256 {sha256}, {len} byte(s))",
                file.path
            ));
        }
        bytes = bytes.saturating_add(len);
    }
    Ok(bytes)
}

/// Remove a staged database: a file or a directory.
fn discard(staging: &Path) {
    if staging.is_dir() {
        let _ = std::fs::remove_dir_all(staging);
    }
    discard_staging(staging);
}

/// Put the staged database at `path`. `false` when a database appeared at
/// `path` first: that one stays. Blocking, under the path lock.
fn publish(staging: &Path, path: &Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    let published = if staging.is_dir() {
        std::fs::rename(staging, path)
    } else {
        std::fs::hard_link(staging, path)
    };
    match published {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || path.exists() => Ok(false),
        Err(e) => Err(format!("could not publish the restored database: {e}")),
    }
}

/// The result of a restore check of one database.
pub fn check_restore(
    replication: &FleetReplication,
    key: &FleetDbKey,
    scratch: &Path,
) -> crate::db::fleet_replication::RestoreCheck {
    use crate::db::fleet_replication::RestoreCheck;
    let once = || {
        std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        let target = scratch.join(format!("{:016x}", replication.entropy().next_u64()));
        let outcome = newest(replication, key, None, false).and_then(|found| match found {
            Some((_, manifest)) => download(
                replication.destination().as_ref(),
                &replication.root_for(key),
                &manifest,
                &target,
            )
            .map(|_| true),
            None => Ok(false),
        });
        discard(&target);
        let _ = std::fs::remove_file(&target);
        outcome
    };
    match once().or_else(|_| once()) {
        Ok(true) => RestoreCheck::Passed,
        Ok(false) => RestoreCheck::NoReplica,
        Err(error) => RestoreCheck::Failed(error),
    }
}

/// The fence error of `key` from a destination error.
fn ship_error(key: &FleetDbKey, error: DestinationError) -> FleetError {
    match error {
        DestinationError::Fenced { epoch, newest } => FleetError::Fenced {
            name: key.name(),
            epoch,
            newest,
        },
        error => FleetError::Unavailable {
            name: key.name(),
            detail: format!("could not ship a generation: {error}"),
        },
    }
}

/// What a ship needs of an open database. No lease: the worker of a
/// database must not keep it open.
pub struct ShipTarget<E: FleetEngine> {
    key: FleetDbKey,
    path: PathBuf,
    handle: E::Handle,
    epoch: Option<crate::lock::FencingToken>,
    retired: Arc<std::sync::atomic::AtomicBool>,
    /// The slot of this open. A ship never makes one.
    slot: Arc<GenerationSlot>,
    /// The hold of a retry. The ship ends it after the snapshot.
    hold: Arc<Mutex<Option<KeyHold<E>>>>,
}

impl<E: FleetEngine> Clone for ShipTarget<E> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            path: self.path.clone(),
            handle: self.handle.clone(),
            epoch: self.epoch,
            retired: Arc::clone(&self.retired),
            slot: Arc::clone(&self.slot),
            hold: Arc::clone(&self.hold),
        }
    }
}

/// Keeps a closed key out of service. The drop ends it.
pub struct KeyHold<E: FleetEngine> {
    fleet: DatabaseFleet<E>,
    key: FleetDbKey,
    drain: Arc<Drain>,
}

impl<E: FleetEngine> Drop for KeyHold<E> {
    fn drop(&mut self) {
        self.fleet.finish_drain(&self.key, &self.drain);
    }
}

impl<E: FleetEngine> ShipTarget<E> {
    /// The target of `db`, with the slot of its open. It holds no lease.
    pub fn of(db: &FleetDatabase<E>, slot: Arc<GenerationSlot>) -> Self {
        Self {
            key: db.key.clone(),
            path: db.path.to_path_buf(),
            handle: db.handle.clone(),
            epoch: db.epoch,
            retired: Arc::clone(&db.retired),
            slot,
            hold: Arc::new(Mutex::new(None)),
        }
    }

    /// Set or end the hold of a retry.
    fn set_hold(&self, hold: Option<KeyHold<E>>) {
        let old = std::mem::replace(
            &mut *self.hold.lock().unwrap_or_else(PoisonError::into_inner),
            hold,
        );
        drop(old);
    }
}

impl<E: FleetEngine> DatabaseFleet<E> {
    /// Ship one generation of the open database `db` now. The ship holds no
    /// lease.
    ///
    /// # Errors
    ///
    /// [`FleetError::Unavailable`] when `db` ships no generations in this
    /// process. Else see [`ship_target`](Self::ship_target).
    pub(super) async fn ship_generation(
        &self,
        replication: &Arc<FleetReplication>,
        db: FleetDatabase<E>,
    ) -> Result<ShipReport, FleetError> {
        let slot = replication
            .current_generation_slot(db.key())
            .ok_or_else(|| FleetError::Unavailable {
                name: db.key().name(),
                detail: "the database ships no generations in this process".to_owned(),
            })?;
        let target = ShipTarget::of(&db, slot);
        drop(db);
        self.ship_target(replication, &target).await
    }

    /// Ship one generation of `target`: snapshot, hash, put, prune. The
    /// ship runs in an owned task: a dropped caller does not stop it half
    /// way, and the turn stays held until it ends.
    ///
    /// # Errors
    ///
    /// [`FleetError::Fenced`] when a newer epoch owns the replica.
    /// [`FleetError::Unavailable`] when the database is retired or a put
    /// failed. The error of [`FleetEngine::snapshot`].
    async fn ship_target(
        &self,
        replication: &Arc<FleetReplication>,
        target: &ShipTarget<E>,
    ) -> Result<ShipReport, FleetError> {
        let (fleet, replication, target) = (self.clone(), Arc::clone(replication), target.clone());
        let name = target.key.name();
        tokio::spawn(async move { fleet.ship_owned(&replication, &target).await })
            .await
            .map_err(|e| FleetError::Unavailable {
                name,
                detail: format!("the ship task failed: {e}"),
            })?
    }

    async fn ship_owned(
        &self,
        replication: &Arc<FleetReplication>,
        target: &ShipTarget<E>,
    ) -> Result<ShipReport, FleetError> {
        // One ship at a time per database: a later snapshot never lands
        // before an earlier one.
        let _turn = target.slot.turn.lock().await;
        // Before the snapshot: a close after it keeps a record this ship
        // does not cover.
        let mark = replication.generation_mark();
        // Read before the snapshot: a commit at or before it is in the copy.
        let through = replication.now();
        let shipped = self.ship_locked(replication, target).await;
        let now = replication.now();
        if shipped.is_ok() {
            // The files at the path hold the writes of an earlier record.
            replication.drop_unshipped_generation(&target.key, mark);
        }
        {
            let mut track = target.slot.track();
            match &shipped {
                Ok(_) => {
                    track.last_ok = Some(now);
                    // Ships run one at a time, so the bound follows the clock.
                    if let Some(bound) = status::bound_below(through) {
                        track.replicated_through = Some(bound);
                    }
                    track.failures = 0;
                    track.last_error = None;
                }
                Err(error) => {
                    if matches!(error, FleetError::Fenced { .. }) {
                        track.replicated_through = None;
                        track.fenced = true;
                    }
                    track.failures = track.failures.saturating_add(1);
                    track.last_error = Some(error.to_string());
                }
            }
        }
        replication.notify_progress();
        shipped
    }

    /// Write the epoch marker of `target`, through the fence. A newer owner
    /// writes it at open, so an old owner is fenced before the first
    /// generation of the new owner.
    async fn mark_epoch(&self, replication: &Arc<FleetReplication>, target: &ShipTarget<E>) {
        let Some(epoch) = target.epoch.map(crate::lock::FencingToken::get) else {
            return;
        };
        let (fleet, owned, replication) = (self.clone(), target.clone(), Arc::clone(replication));
        let marked = crate::time::spawn_blocking(move || {
            let _upload = owned
                .slot
                .upload
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let root = replication.root_for(&owned.key);
            fleet
                .ship_destination(&replication, &owned, Some(epoch))
                .put(&segment::epoch_marker_key(&root, epoch), b"{}")
        })
        .await;
        match marked {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(database = %target.key, %error, "fleet generation: could not write the epoch marker; the first ship writes it");
            }
            Err(error) => {
                tracing::warn!(database = %target.key, %error, "fleet generation: the epoch marker task failed");
            }
        }
    }

    /// A ship, under the turn.
    async fn ship_locked(
        &self,
        replication: &Arc<FleetReplication>,
        target: &ShipTarget<E>,
    ) -> Result<ShipReport, FleetError> {
        if target.retired.load(Ordering::Acquire) {
            return Err(ship_error(
                &target.key,
                DestinationError::Rejected {
                    detail: "the database is retired".to_owned(),
                },
            ));
        }
        // One ship at a time across the processes on this volume: each reads
        // the newest manifest, and no older snapshot lands after a newer one.
        let _lock = ShipLock::acquire(self.root(), &target.key).await?;
        let scratch =
            ship_scratch(self.root()).join(format!("{:016x}", replication.entropy().next_u64()));
        std::fs::create_dir_all(&scratch).map_err(super::io_error("create ship scratch"))?;
        let files = self
            .inner
            .engine
            .snapshot(&target.handle, &target.path, &scratch)
            .await;
        // The copy is in scratch: opens of a held key may go on.
        target.set_hold(None);
        let shipped = match files {
            Ok(files) => {
                let (fleet, replication, owned, scratch) = (
                    self.clone(),
                    Arc::clone(replication),
                    target.clone(),
                    scratch.clone(),
                );
                crate::time::spawn_blocking(move || {
                    fleet.ship_blocking(&replication, &owned, &scratch, &files)
                })
                .await
                .map_err(|e| FleetError::Unavailable {
                    name: target.key.name(),
                    detail: format!("the ship task failed: {e}"),
                })
                .and_then(|r| r)
            }
            Err(error) => Err(error),
        };
        let _ = std::fs::remove_dir_all(&scratch);
        shipped
    }

    /// The blocking part of a ship, under the upload lock.
    fn ship_blocking(
        &self,
        replication: &Arc<FleetReplication>,
        target: &ShipTarget<E>,
        scratch: &Path,
        files: &[PathBuf],
    ) -> Result<ShipReport, FleetError> {
        let key = &target.key;
        let slot = &target.slot;
        let _upload = slot.upload.lock().unwrap_or_else(PoisonError::into_inner);
        let unavailable = |detail: String| FleetError::Unavailable {
            name: key.name(),
            detail,
        };
        let files = local_files(scratch, files).map_err(unavailable)?;
        // Read under the ship lock, at each ship: another process can have
        // shipped and pruned since this process shipped last.
        let prev = newest(replication, key, None, true)
            .map_err(unavailable)?
            .map(|(_, manifest)| manifest);
        let root = replication.root_for(key);
        let raw = replication.destination();
        let floor_ms = replication
            .newest_generation(key)
            .map_err(unavailable)?
            .map(|at| at.timestamp_millis());
        let created_ms = replication
            .now()
            .timestamp_millis()
            .max(floor_ms.map_or(i64::MIN, |ms| ms.saturating_add(1)));
        let id = segment::generation_id(created_ms, replication.entropy().next_u64());
        let Some(planned) = plan_ship(prev.as_ref(), &files, &id) else {
            return Ok(ShipReport {
                generation: None,
                shipped: 0,
                reused: files.len(),
                bytes: 0,
            });
        };
        let epoch = target.epoch.map(crate::lock::FencingToken::get);
        let destination = self.ship_destination(replication, target, epoch);
        // With no epoch of its own, a generation takes the newest epoch at
        // the destination: it must order after the fenced generations.
        let generation_epoch = match epoch {
            Some(epoch) => {
                destination
                    .put(&segment::epoch_marker_key(&root, epoch), b"{}")
                    .map_err(|e| ship_error(key, e))?;
                Some(epoch)
            }
            None => replication.newest_epoch(key).map_err(unavailable)?,
        };
        if let Some(epoch) = generation_epoch {
            destination
                .put(&segment::generation_epoch_key(&root, &id, epoch), b"{}")
                .map_err(|e| ship_error(key, e))?;
        }
        let mut bytes = 0_u64;
        for file in &planned.put {
            destination
                .put_file(&file_key(&root, &id, &file.sha256), &file.file)
                .map_err(|e| ship_error(key, e))?;
            bytes = bytes.saturating_add(file.len);
        }
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            generation: id.clone(),
            created_ms,
            epoch: generation_epoch,
            files: planned.files,
        };
        let body = serde_json::to_vec(&manifest).map_err(|e| unavailable(e.to_string()))?;
        // Last: the manifest makes the generation complete.
        destination
            .put(&manifest_key(&root, &id), &body)
            .map_err(|e| ship_error(key, e))?;
        let report = ShipReport {
            generation: Some(id),
            shipped: planned.put.len(),
            reused: manifest.files.len().saturating_sub(planned.put.len()),
            bytes,
        };
        tracing::debug!(database = %key, generation = ?report.generation, shipped = report.shipped, reused = report.reused, "fleet generation shipped");
        // A retired or fenced host prunes nothing: the new owner can name
        // these files.
        let pruned = destination
            .fence()
            .check()
            .map_err(|e| e.to_string())
            .and_then(|()| prune(raw.as_ref(), &root, &manifest));
        if let Err(error) = pruned {
            tracing::warn!(database = %key, %error, "fleet generation prune skipped or failed; the next ship tries again");
        }
        Ok(report)
    }

    /// The fenced destination of a ship of `target`. Its writes stop when
    /// the database is retired, or when this host does not hold the slot
    /// at the epoch the database opened under.
    fn ship_destination(
        &self,
        replication: &Arc<FleetReplication>,
        target: &ShipTarget<E>,
        epoch: Option<u64>,
    ) -> FencedDestination {
        let retired = Arc::clone(&target.retired);
        let fence = self.ownership().cloned().zip(target.epoch);
        let slot = target.key.routing_slot();
        // A passed shutdown cutoff stops the writes too: a large generation
        // does not hold the shutdown past its flush budget.
        let shutdown = Arc::downgrade(replication);
        let stop = move || {
            retired.load(Ordering::Acquire)
                || fence
                    .as_ref()
                    .is_some_and(|(ownership, epoch)| ownership.epoch_for(slot) != Some(*epoch))
                || shutdown.upgrade().is_some_and(|replication| {
                    replication
                        .shutdown_cutoff()
                        .is_some_and(crate::db::fleet_replication::cutoff_passed)
                })
        };
        FencedDestination::new(
            Arc::clone(replication.destination()),
            replication
                .ship_fence(&target.key, epoch)
                .stopped_when(Arc::new(stop)),
        )
    }

    /// Why the closed `target` must not ship now. `Retired`: it is retired,
    /// or a newer epoch owns its slot. `Failed`: this host does not hold the
    /// slot at the epoch of the record, and no newer epoch is known. The
    /// record stays: the next open ships the file. `None`: it may ship.
    fn kept_stop(&self, target: &ShipTarget<E>) -> Option<GenerationRetry> {
        if target.retired.load(Ordering::Acquire) {
            return Some(GenerationRetry::Retired);
        }
        let (ownership, epoch) = self.ownership().zip(target.epoch)?;
        let slot = target.key.routing_slot();
        if ownership.fence_floor(slot) > epoch.get() {
            return Some(GenerationRetry::Retired);
        }
        (ownership.epoch_for(slot) != Some(epoch)).then(|| {
            GenerationRetry::Failed(
                "this host does not hold the slot at the epoch of the close".to_owned(),
            )
        })
    }

    /// Whether `db` was retired by a lapse of this host's lease, and not by
    /// a newer epoch. Its writes are still this host's to ship.
    fn lapsed(&self, db: &FleetDatabase<E>) -> bool {
        let Some((ownership, epoch)) = self.ownership().zip(db.epoch) else {
            return false;
        };
        let slot = db.key.routing_slot();
        ownership.fence_floor(slot) <= epoch.get() && ownership.epoch_for(slot) != Some(epoch)
    }

    /// Take the closed `key` out of service for the snapshot of a retry:
    /// opens, deletes and restores wait. `None` when the key is open,
    /// opening or draining.
    fn hold_closed(&self, key: &FleetDbKey) -> Option<KeyHold<E>> {
        let mut state = self.lock_state();
        if state.open.contains_key(key) || state.draining.contains_key(key) {
            return None;
        }
        // No retire flag: a lapse is not a fence, and the ship checks the
        // epoch itself.
        let drain = Arc::new(Drain {
            deleting: false,
            finished: AtomicBool::new(false),
            done: Notify::new(),
            retire: None,
            hold: true,
        });
        state.draining.insert(key.clone(), Arc::clone(&drain));
        drop(state);
        self.inner.in_flight_drains.fetch_add(1, Ordering::Relaxed);
        Some(KeyHold {
            fleet: self.clone(),
            key: key.clone(),
            drain,
        })
    }

    /// Ship the kept generation of the closed `target` once. Blocking.
    fn retry_kept(
        &self,
        replication: &Arc<FleetReplication>,
        target: &ShipTarget<E>,
        runtime: &tokio::runtime::Handle,
    ) -> GenerationRetry {
        if let Some(stop) = self.kept_stop(target) {
            return stop;
        }
        // The hold ends after the snapshot: no open writes the files while
        // it reads them, and no delete or restore removes them.
        let Some(hold) = self.hold_closed(&target.key) else {
            return GenerationRetry::Busy;
        };
        if !self.inner.engine.exists(&target.path) {
            // Another process deleted it: nothing is left to ship.
            return GenerationRetry::Retired;
        }
        target.set_hold(Some(hold));
        let shipped = runtime.block_on(self.ship_target(replication, target));
        target.set_hold(None);
        match shipped {
            Ok(_) => GenerationRetry::Shipped,
            Err(FleetError::Fenced { .. }) => GenerationRetry::Retired,
            Err(error) => match self.kept_stop(target) {
                Some(GenerationRetry::Retired) => GenerationRetry::Retired,
                _ => GenerationRetry::Failed(error.to_string()),
            },
        }
    }

    /// Restore the missing database of `key` at `path` from its newest
    /// generation. Blocking: core holds the path lock.
    fn restore_missing_generation(
        replication: &FleetReplication,
        key: &FleetDbKey,
        path: &Path,
    ) -> Result<bool, String> {
        let Some((_, manifest)) = newest(replication, key, None, true)? else {
            return Ok(false);
        };
        let staging = staging_path(path, "restoring").map_err(|e| e.to_string())?;
        let result = download(
            replication.destination().as_ref(),
            &replication.root_for(key),
            &manifest,
            &staging,
        )
        .and_then(|bytes| {
            // `false`: another process published first. Its database opens.
            if publish(&staging, path)? {
                tracing::info!(database = %key, bytes, generation = %manifest.generation, "fleet database restored from its generations");
            }
            Ok(true)
        });
        discard(&staging);
        result
    }

    /// Replace the database at `path` with the newest generation at or
    /// before `target`. The tombstone marker does not apply: the caller
    /// took the row. Blocking.
    ///
    /// With `above`, only a generation of an epoch above it replaces the
    /// database: an older generation can lack writes of the file on disk.
    /// Returns `false` when it kept the database.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotFound`] when no generation is there.
    /// [`FleetError::InUseElsewhere`] when another process has the
    /// database open. [`FleetError::Restore`] for any other failure.
    pub(super) fn restore_generation_in_place(
        &self,
        replication: &FleetReplication,
        key: &FleetDbKey,
        target: Option<DateTime<Utc>>,
        path: &Path,
        above: Option<u64>,
    ) -> Result<bool, FleetError> {
        let name = key.name();
        let failed = |detail: String| FleetError::Restore {
            name: name.clone(),
            detail,
        };
        check_contained(self.root(), path)?;
        let (chosen, manifest) = newest(replication, key, target, false)
            .map_err(failed)?
            .ok_or_else(|| FleetError::NotFound { name: name.clone() })?;
        if above.is_some_and(|stamp| chosen.epoch <= stamp) {
            return Ok(false);
        }
        let staging = staging_path(path, "restoring")?;
        let downloaded = download(
            replication.destination().as_ref(),
            &replication.root_for(key),
            &manifest,
            &staging,
        );
        let result = downloaded.map_err(failed).and_then(|_| {
            with_path_lock(self.root(), &name, || {
                self.inner.engine.remove(key, path)?;
                if publish(&staging, path).map_err(failed)? {
                    Ok(true)
                } else {
                    Err(FleetError::AlreadyExists { name: name.clone() })
                }
            })
        });
        discard(&staging);
        result
    }
}

/// Remove the generations a prune does not keep. Blocking, after a ship.
/// `last` is the manifest the ship just wrote.
fn prune(destination: &dyn ReplicaDestination, root: &str, last: &Manifest) -> Result<(), String> {
    let keys = destination
        .list(&segment::generations_prefix(root))
        .map_err(|e| e.to_string())?;
    let complete = complete_generations(root, &keys);
    if complete.len() <= KEEP_GENERATIONS {
        return Ok(());
    }
    let mut manifests = HashMap::new();
    for generation in complete.iter().rev().take(KEEP_GENERATIONS) {
        let manifest = if last.generation == generation.id {
            last.clone()
        } else {
            let body = destination
                .get(&manifest_key(root, &generation.id))
                .map_err(|e| e.to_string())?;
            serde_json::from_slice(&body).map_err(|e| format!("unreadable manifest: {e}"))?
        };
        manifests.insert(generation.id.clone(), manifest);
    }
    let keep = keep_set(&complete, &manifests, KEEP_GENERATIONS);
    for key in prunable(root, &keys, &keep) {
        destination
            .delete(&key)
            .map_err(|e| format!("could not delete {key}: {e}"))?;
    }
    Ok(())
}

/// The lifecycle hook of `ShipMode::Generations`: a ship worker for each
/// open database, and the restore of a missing one.
pub struct GenerationShipper<E: FleetEngine> {
    fleet: Weak<Inner<E>>,
    replication: Arc<FleetReplication>,
    every: Duration,
}

impl<E: FleetEngine> GenerationShipper<E> {
    pub fn new(
        fleet: &DatabaseFleet<E>,
        replication: Arc<FleetReplication>,
        every: Duration,
    ) -> Self {
        Self {
            fleet: Arc::downgrade(&fleet.inner),
            replication,
            every,
        }
    }
}

impl<E: FleetEngine> GenerationShipper<E> {
    /// Keep the closed `db` as unshipped work. The record holds its key,
    /// path and epoch, and its closed handle: `snapshot` then copies the
    /// files at the path.
    fn keep(
        &self,
        db: &FleetDatabase<E>,
        slot: Arc<GenerationSlot>,
        runtime: Option<tokio::runtime::Handle>,
    ) {
        // Its own flag: the close sets the flag of `db` when it ends.
        let retired = Arc::new(AtomicBool::new(false));
        let target = ShipTarget {
            retired: Arc::clone(&retired),
            ..ShipTarget::of(db, Arc::clone(&slot))
        };
        let (fleet, replication) = (self.fleet.clone(), Arc::downgrade(&self.replication));
        let retry = move || {
            let (Some(inner), Some(replication)) = (fleet.upgrade(), replication.upgrade()) else {
                return GenerationRetry::Failed("the fleet is gone".to_owned());
            };
            // The current runtime first: the one of the close can be gone.
            let Some(runtime) = tokio::runtime::Handle::try_current()
                .ok()
                .or_else(|| runtime.clone())
            else {
                return GenerationRetry::Failed("no Tokio runtime to ship on".to_owned());
            };
            DatabaseFleet { inner }.retry_kept(&replication, &target, &runtime)
        };
        self.replication.keep_unshipped_generation(
            db.key(),
            UnshippedGeneration::new(
                db.path.to_path_buf(),
                db.epoch.map(crate::lock::FencingToken::get),
                retired,
                Some(slot),
                Box::new(retry),
            ),
        );
    }
}

impl<E: FleetEngine> FleetLifecycle<E> for GenerationShipper<E> {
    fn restore_missing(&self, key: &FleetDbKey, path: &Path) -> Result<bool, String> {
        if !self.replication.restores_missing() {
            return Ok(false);
        }
        DatabaseFleet::<E>::restore_missing_generation(&self.replication, key, path)
    }

    fn on_open(&self, db: &FleetDatabase<E>) -> Result<(), String> {
        // The slot lives until `on_close`: the last ship needs it.
        let epoch = db.epoch().map_or(0, crate::lock::FencingToken::get);
        let slot = self.replication.open_generation_slot(db.key(), epoch);
        let target = ShipTarget::of(db, slot);
        let (fleet, replication, every) = (
            self.fleet.clone(),
            Arc::clone(&self.replication),
            self.every,
        );
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
        let handle = runtime.spawn(async move {
            // The epoch marker first: an old owner that still ships is
            // fenced from now on, also before the first generation.
            if let Some(fleet) = fleet.upgrade().map(|inner| DatabaseFleet { inner }) {
                fleet.mark_epoch(&replication, &target).await;
            }
            let start = tokio::time::Instant::now() + every;
            let mut tick = tokio::time::interval_at(start, every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = stopped.cancelled() => break,
                    _ = tick.tick() => {
                        let Some(fleet) = fleet.upgrade().map(|inner| DatabaseFleet { inner }) else {
                            return;
                        };
                        if let Err(error) = fleet.ship_target(&replication, &target).await {
                            tracing::warn!(database = %target.key, %error, "fleet generation ship failed; the next interval tries again");
                        }
                    }
                }
            }
        });
        db.register_worker(stop, handle);
        Ok(())
    }

    /// The last generation of a close. It runs after every use ended, so it
    /// holds every write. A delete, a restore, a fence or a refused open
    /// retires the database first: then it ships nothing.
    ///
    /// Core keeps a failed or skipped ship as unshipped work. The loop and
    /// the shutdown flush try it again.
    fn on_close(&self, db: &FleetDatabase<E>) {
        let Some(slot) = self.replication.current_generation_slot(db.key()) else {
            return;
        };
        let late = self
            .replication
            .shutdown_cutoff()
            .is_some_and(crate::db::fleet_replication::cutoff_passed);
        let fleet = self.fleet.upgrade().map(|inner| DatabaseFleet { inner });
        let runtime = tokio::runtime::Handle::try_current().ok();
        let outcome = if db.retired.load(Ordering::Acquire) {
            // A lapse of the lease is not a fence: the writes stay this
            // host's. A delete or a restore releases the record at once.
            fleet
                .as_ref()
                .is_some_and(|fleet| fleet.lapsed(db))
                .then(|| FleetError::Unavailable {
                    name: db.key().name(),
                    detail: "the lease of this host lapsed".to_owned(),
                })
                .map_or(Ok(()), Err)
        } else {
            let not_tried = |detail: &str| FleetError::Unavailable {
                name: db.key().name(),
                detail: detail.to_owned(),
            };
            match (late, fleet, &runtime) {
                (false, Some(fleet), Some(runtime)) => {
                    let target = ShipTarget::of(db, Arc::clone(&slot));
                    runtime
                        .block_on(fleet.ship_target(&self.replication, &target))
                        .map(drop)
                }
                (true, ..) => Err(not_tried("the shutdown cutoff passed")),
                (false, ..) => Err(not_tried("no fleet or Tokio runtime to ship on")),
            }
        };
        match outcome {
            Ok(()) => {}
            // A newer epoch owns the replica: the data is not this host's.
            Err(error @ FleetError::Fenced { .. }) => {
                tracing::warn!(database = %db.key(), %error, "fleet generation ship at close refused");
            }
            Err(error) => {
                tracing::warn!(database = %db.key(), %error, "fleet generation ship at close did not ship");
                // After a ship in flight: its success must not drop the
                // record of later writes.
                let _turn = slot.turn.blocking_lock();
                self.keep(db, Arc::clone(&slot), runtime);
            }
        }
        // A ship of a retired database may still upload: wait, so a purge
        // after a delete sees every object it wrote.
        slot.wait_upload();
        // Kept before the slot goes: a waiter never sees the key in neither.
        let closed = {
            let track = slot.track();
            if track.fenced {
                ClosedBound::Fenced
            } else {
                ClosedBound::At {
                    at: track.replicated_through,
                    epoch: slot.epoch(),
                }
            }
        };
        self.replication.remember_closed(db.key(), closed);
        self.replication.forget_generation_slot(db.key(), &slot);
    }

    /// A delete or a restore starts: the kept generation of `key` stops
    /// shipping. Core settles it when the delete or the restore ends.
    fn release(&self, key: &FleetDbKey) {
        // A delete or a restore replaces the files: the old bound is void.
        self.replication.forget_closed(key);
        self.replication.release_unshipped_generation(key);
    }
}

/// The directory of the ship locks, under the fleet's reserved directory.
/// A sweep never touches it.
const SHIP_LOCK_DIR: &str = "ship-locks";

/// How long a ship waits for the ship lock of another process.
const SHIP_LOCK_WAIT: Duration = Duration::from_mins(1);

/// How often a ship tries the lock again.
const SHIP_LOCK_POLL: Duration = Duration::from_millis(25);

/// The OS lock that makes one ship of a database run at a time, across the
/// processes on a volume. Databases share 256 lock files by a hash of the
/// name. The OS frees the lock when the file closes or the process ends.
struct ShipLock {
    _file: std::fs::File,
}

impl ShipLock {
    /// Try once. `None` when another ship holds it. Blocking, but fast.
    fn try_acquire(root: &Path, key: &FleetDbKey) -> std::io::Result<Option<Self>> {
        let dir = root
            .join(crate::fleet_layout::RESERVED_DIR)
            .join(SHIP_LOCK_DIR);
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{:03}.lock", super::lock_stripe(&key.name()))))?;
        let mut lock = fd_lock::RwLock::new(file);
        match lock.try_write() {
            // Keep it held: closing the file frees it.
            Ok(guard) => std::mem::forget(guard),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
        Ok(Some(Self {
            _file: lock.into_inner(),
        }))
    }

    /// Wait for the lock, for at most [`SHIP_LOCK_WAIT`].
    ///
    /// # Errors
    ///
    /// [`FleetError::Unavailable`] when the wait ends, or the lock file
    /// cannot be opened.
    async fn acquire(root: &Path, key: &FleetDbKey) -> Result<Self, FleetError> {
        let unavailable = |detail: String| FleetError::Unavailable {
            name: key.name(),
            detail,
        };
        let deadline = tokio::time::Instant::now() + SHIP_LOCK_WAIT;
        loop {
            match Self::try_acquire(root, key) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(SHIP_LOCK_POLL).await;
                }
                Ok(None) => {
                    return Err(unavailable(
                        "another process ships this database; the next ship tries again".to_owned(),
                    ));
                }
                Err(error) => return Err(unavailable(format!("the ship lock failed: {error}"))),
            }
        }
    }
}

/// The scratch directory of the ships of the fleet at `root`.
pub fn ship_scratch(root: &Path) -> PathBuf {
    root.join(crate::fleet_layout::RESERVED_DIR).join(SHIP_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, body: &[u8]) -> LocalFile {
        LocalFile {
            path: path.to_owned(),
            file: std::path::PathBuf::from(path),
            sha256: crate::replication::segment::sha256_hex(body),
            len: u64::try_from(body.len()).unwrap(),
        }
    }

    fn manifest(id: &str, files: Vec<ManifestFile>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            generation: id.to_owned(),
            created_ms: 0,
            epoch: None,
            files,
        }
    }

    #[test]
    fn the_first_generation_ships_every_file() {
        let files = [local("db/a", b"a"), local("db/b", b"b")];
        let planned = plan_ship(None, &files, "g1").expect("a first generation ships");
        assert_eq!(planned.put.len(), 2);
        assert!(planned.files.iter().all(|f| f.holder == "g1"));
    }

    #[test]
    fn an_unchanged_set_ships_no_generation() {
        let files = [local("db/a", b"a"), local("db/b", b"b")];
        let first = plan_ship(None, &files, "g1").unwrap();
        let prev = manifest("g1", first.files);
        assert!(plan_ship(Some(&prev), &files, "g2").is_none());
    }

    #[test]
    fn a_changed_file_ships_alone_and_the_rest_keep_their_holder() {
        let old = [local("db/a", b"a"), local("db/b", b"b")];
        let first = plan_ship(None, &old, "g1").unwrap();
        let prev = manifest("g1", first.files);
        let files = [
            local("db/a", b"a"),
            local("db/b", b"b2"),
            local("db/c", b"a"),
        ];
        let planned = plan_ship(Some(&prev), &files, "g2").unwrap();
        let put: Vec<&str> = planned.put.iter().map(|f| f.path.as_str()).collect();
        // `db/c` has the content of `db/a`: the hash is held, so it is not put.
        assert_eq!(put, ["db/b"]);
        let holder = |path: &str| {
            planned
                .files
                .iter()
                .find(|f| f.path == path)
                .unwrap()
                .holder
                .clone()
        };
        assert_eq!(holder("db/a"), "g1");
        assert_eq!(holder("db/b"), "g2");
        assert_eq!(holder("db/c"), "g1");
    }

    #[test]
    fn a_removed_file_makes_a_new_generation() {
        let old = [local("db/a", b"a"), local("db/b", b"b")];
        let first = plan_ship(None, &old, "g1").unwrap();
        let prev = manifest("g1", first.files);
        let new = [local("db/a", b"a")];
        let planned = plan_ship(Some(&prev), &new, "g2").unwrap();
        assert!(planned.put.is_empty());
        assert_eq!(planned.files.len(), 1);
    }

    #[test]
    fn one_hash_twice_is_put_once() {
        let files = [local("db/a", b"x"), local("db/b", b"x")];
        let planned = plan_ship(None, &files, "g1").unwrap();
        assert_eq!(planned.put.len(), 1);
        assert_eq!(planned.files.len(), 2);
    }

    const ROOT: &str = "app/fleet/tenant/acme";

    fn keys_of(id: &str, epoch: Option<u64>, complete: bool) -> Vec<String> {
        let mut keys = vec![file_key(ROOT, id, &"0".repeat(64))];
        if let Some(epoch) = epoch {
            keys.push(crate::replication::segment::generation_epoch_key(
                ROOT, id, epoch,
            ));
        }
        if complete {
            keys.push(manifest_key(ROOT, id));
        }
        keys
    }

    fn id(ms: i64) -> String {
        crate::replication::segment::generation_id(ms, 1)
    }

    #[test]
    fn complete_generations_order_by_epoch_then_time() {
        let mut keys = Vec::new();
        keys.extend(keys_of(&id(3_000), Some(1), true));
        keys.extend(keys_of(&id(2_000), Some(2), true));
        keys.extend(keys_of(&id(1_000), Some(2), true));
        keys.extend(keys_of(&id(4_000), Some(3), false));
        keys.push(format!("{ROOT}/tombstone"));
        let complete = complete_generations(ROOT, &keys);
        let ids: Vec<&str> = complete.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, [id(3_000), id(1_000), id(2_000)]);
        assert_eq!(complete.last().unwrap().epoch, 2);
    }

    #[test]
    fn the_keep_set_holds_the_newest_and_every_named_holder() {
        let (g1, g2, g3, g4) = (id(1_000), id(2_000), id(3_000), id(4_000));
        let file = |holder: &str| ManifestFile {
            path: "db".to_owned(),
            sha256: "0".repeat(64),
            len: 1,
            holder: holder.to_owned(),
        };
        let mut keys = Vec::new();
        for g in [&g1, &g2, &g3, &g4] {
            keys.extend(keys_of(g, None, true));
        }
        let complete = complete_generations(ROOT, &keys);
        let manifests: std::collections::HashMap<String, Manifest> = [
            (g3.clone(), manifest(&g3, vec![file(&g1)])),
            (g4.clone(), manifest(&g4, vec![file(&g4)])),
        ]
        .into_iter()
        .collect();
        let keep = keep_set(&complete, &manifests, 2);
        assert!(keep.ids.contains(&g3) && keep.ids.contains(&g4));
        assert!(keep.ids.contains(&g1), "g3 names a file of g1");
        assert!(!keep.ids.contains(&g2));
        let doomed = prunable(ROOT, &keys, &keep);
        assert!(doomed.iter().all(|key| key.contains(&g2)), "{doomed:?}");
        assert!(!doomed.is_empty());
    }

    #[test]
    fn a_prune_never_removes_a_generation_newer_than_the_kept_ones() {
        let (g1, g2, g3) = (id(1_000), id(2_000), id(3_000));
        let mut keys = Vec::new();
        keys.extend(keys_of(&g1, None, true));
        keys.extend(keys_of(&g2, None, true));
        // An upload in progress: no manifest yet.
        keys.extend(keys_of(&g3, None, false));
        let complete = complete_generations(ROOT, &keys);
        let manifests = std::collections::HashMap::new();
        let keep = keep_set(&complete, &manifests, 1);
        let doomed = prunable(ROOT, &keys, &keep);
        assert!(doomed.iter().all(|key| key.contains(&g1)), "{doomed:?}");
    }

    #[test]
    fn manifest_paths_stay_inside_the_snapshot() {
        for good in ["acme.txt", "acme.rocks/000012.sst"] {
            assert!(check_relative(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a\\b",
            "./a",
            "a/",
        ] {
            assert!(check_relative(bad).is_err(), "{bad:?}");
        }
        assert!(check_hash(&"a".repeat(64)).is_ok());
        assert!(check_hash("../x").is_err());
        assert!(check_hash(&"A".repeat(64)).is_err());
    }

    #[test]
    fn a_snapshot_must_be_one_database() {
        assert!(check_shape(["acme.db"]).is_ok());
        assert!(check_shape(["acme.rocks/1.sst", "acme.rocks/CURRENT"]).is_ok());
        assert!(check_shape(["acme.db", "acme.db"]).is_err(), "a path twice");
        assert!(check_shape(["a/x", "b/x"]).is_err(), "two entries");
        assert!(
            check_shape(["acme.db", "acme.db/x"]).is_err(),
            "a file and a directory"
        );
    }

    #[test]
    fn one_ship_lock_holder_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();
        let key = crate::fleet_layout::FleetDbKey::Slot(7);
        let held = ShipLock::try_acquire(tmp.path(), &key).unwrap();
        assert!(held.is_some());
        assert!(ShipLock::try_acquire(tmp.path(), &key).unwrap().is_none());
        drop(held);
        assert!(ShipLock::try_acquire(tmp.path(), &key).unwrap().is_some());
    }

    #[test]
    fn a_manifest_round_trips_through_json() {
        let files = [local("db/a", b"a")];
        let planned = plan_ship(None, &files, "g1").unwrap();
        let written = manifest("g1", planned.files);
        let body = serde_json::to_vec(&written).unwrap();
        let read: Manifest = serde_json::from_slice(&body).unwrap();
        assert_eq!(read, written);
    }
}
