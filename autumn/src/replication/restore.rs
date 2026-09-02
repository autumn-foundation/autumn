//! Point-in-time restore from a replica (issue #1628, phase 2).
//!
//! Two halves, both pure of the transport:
//!
//! * [`plan`] reads only the destination's **key listing** — generation ids and
//!   segment names already carry their timestamps — and decides which generation
//!   and which prefix of its segments reconstruct the database as of a chosen
//!   instant. Nothing is downloaded to make that decision.
//! * [`apply`] downloads exactly that, reassembles `db` + `db-wal`, lets `SQLite`'s
//!   own recovery replay the WAL, and only then publishes the result.
//!
//! # Refusal, not best effort
//!
//! Handing `SQLite` a damaged WAL is the dangerous failure mode: recovery stops at
//! the first frame that does not validate and reports success, so a silently
//! truncated replica looks like a clean restore that is merely missing the last
//! few minutes. Every check here therefore **refuses** rather than trims:
//!
//! * a generation without its `snapshot.json` commit marker is ignored (it was
//!   interrupted mid-upload);
//! * a missing or out-of-order segment sequence is an error, never a silent stop;
//! * a segment whose `start_offset` does not continue the previous one, or whose
//!   salt is not the generation's, is an error;
//! * every payload's SHA-256 and length are verified before use;
//! * the reassembled database must pass `PRAGMA integrity_check` **before** it is
//!   moved into place.
//!
//! The same code path backs the periodic verifier, so "verified restorable" means
//! literally that — a restore ran.

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::destination::{DestinationError, ReplicaDestination};
use super::segment::{self, SegmentError, SegmentHeader, SnapshotMeta};
use super::sqlite::{self, SqliteError};
use super::wal;

/// Why a restore could not be planned or applied.
#[derive(Debug)]
pub enum RestoreError {
    /// The destination itself failed.
    Destination(DestinationError),
    /// No complete generation exists under the configured prefix.
    NoReplica {
        /// The key prefix that was searched.
        root: String,
    },
    /// The requested instant predates every replicated generation.
    TargetBeforeRetention {
        /// The instant that was asked for.
        target: DateTime<Utc>,
        /// The oldest instant the replica can reconstruct.
        oldest: DateTime<Utc>,
    },
    /// A segment is missing from the middle of a generation's sequence.
    SegmentGap {
        /// The generation.
        generation: String,
        /// The sequence number that should have come next.
        expected_seq: u64,
        /// What was found there instead.
        found_seq: u64,
    },
    /// A segment does not start where the previous one ended.
    SegmentDiscontinuity {
        /// The generation.
        generation: String,
        /// The offending segment.
        seq: u64,
        /// Byte offset the previous segment ended at.
        expected_offset: u64,
        /// Byte offset this segment claims to start at.
        found_offset: u64,
    },
    /// A segment carries a salt that is not its generation's.
    SaltMismatch {
        /// The generation.
        generation: String,
        /// The offending segment.
        seq: u64,
    },
    /// A payload failed its own framing/verification.
    Segment(SegmentError),
    /// The snapshot's bytes do not match the digest recorded when it was taken.
    SnapshotDigest {
        /// Bytes the metadata promised.
        expected_len: u64,
        /// Bytes actually recovered.
        actual_len: u64,
    },
    /// The replica was written by a newer layout version.
    UnsupportedVersion {
        /// The version found on the destination.
        version: u32,
    },
    /// Local I/O while staging or publishing the restore failed.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// I/O detail.
        detail: String,
    },
    /// `SQLite` refused the reassembled database.
    Sqlite(SqliteError),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Destination(e) => write!(f, "{e}"),
            Self::NoReplica { root } => write!(
                f,
                "no complete replica generation was found under {root:?}.\n  \
                 Check [replication] prefix/profile and that the app has been running \
                 with replication enabled."
            ),
            Self::TargetBeforeRetention { target, oldest } => write!(
                f,
                "cannot restore to {target}: the oldest replicated state is {oldest}.\n  \
                 Pick a later --timestamp, or raise [replication] retention_hours before \
                 the window you need scrolls past."
            ),
            Self::SegmentGap {
                generation,
                expected_seq,
                found_seq,
            } => write!(
                f,
                "replica generation {generation} is missing segment {expected_seq} \
                 (next present segment is {found_seq}) — refusing to restore a replica \
                 with a hole in it."
            ),
            Self::SegmentDiscontinuity {
                generation,
                seq,
                expected_offset,
                found_offset,
            } => write!(
                f,
                "replica generation {generation} segment {seq} starts at WAL offset \
                 {found_offset} but the previous segment ended at {expected_offset}."
            ),
            Self::SaltMismatch { generation, seq } => write!(
                f,
                "replica generation {generation} segment {seq} carries a salt from a \
                 different WAL generation."
            ),
            Self::Segment(e) => write!(f, "{e}"),
            Self::SnapshotDigest {
                expected_len,
                actual_len,
            } => write!(
                f,
                "the replica snapshot failed verification (metadata promised \
                 {expected_len} byte(s), recovered {actual_len})"
            ),
            Self::UnsupportedVersion { version } => write!(
                f,
                "the replica uses layout version {version}, newer than this build \
                 understands ({}) — upgrade autumn before restoring.",
                segment::LAYOUT_VERSION
            ),
            Self::Io { op, detail } => write!(f, "restore {op} failed: {detail}"),
            Self::Sqlite(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RestoreError {}

impl From<DestinationError> for RestoreError {
    fn from(e: DestinationError) -> Self {
        Self::Destination(e)
    }
}

impl From<SegmentError> for RestoreError {
    fn from(e: SegmentError) -> Self {
        Self::Segment(e)
    }
}

impl From<SqliteError> for RestoreError {
    fn from(e: SqliteError) -> Self {
        Self::Sqlite(e)
    }
}

impl RestoreError {
    fn io(op: &'static str) -> impl Fn(std::io::Error) -> Self {
        move |e| Self::Io {
            op,
            detail: e.to_string(),
        }
    }
}

/// One segment selected by [`plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSegment {
    /// Full destination key.
    pub key: String,
    /// Position inside the generation.
    pub seq: u64,
    /// When the segment was shipped.
    pub created_at: DateTime<Utc>,
}

/// What a restore will download and reconstruct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    /// The generation the restore is built from.
    pub generation: String,
    /// When that generation's base snapshot was taken.
    pub generation_started_at: DateTime<Utc>,
    /// Key of the base snapshot.
    pub snapshot_key: String,
    /// Key of the snapshot's metadata / commit marker.
    pub snapshot_meta_key: String,
    /// The segments to replay, in order.
    pub segments: Vec<PlannedSegment>,
    /// The instant that was asked for (`None` = "the latest available").
    pub requested: Option<DateTime<Utc>>,
    /// The instant the restore will actually land on: the ship time of the last
    /// replayed segment, or the snapshot time when none is replayed.
    pub effective: DateTime<Utc>,
}

/// What a completed restore produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// The plan that was applied.
    pub plan: RestorePlan,
    /// Where the restored database was written.
    pub output: PathBuf,
    /// Bytes of the restored database file.
    pub bytes: u64,
    /// WAL frames replayed onto the snapshot.
    pub frames_replayed: u64,
}

/// Convert epoch milliseconds to a UTC instant, clamping an unrepresentable
/// value rather than failing a restore over a clock artifact.
fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

/// Decide what to restore, reading only the destination's key listing.
///
/// `target` is the instant to restore to; `None` means "the latest replicated
/// state". A generation is a candidate only when its `snapshot.json` commit
/// marker is present.
///
/// # Errors
///
/// See [`RestoreError`].
pub fn plan(
    destination: &dyn ReplicaDestination,
    root: &str,
    target: Option<DateTime<Utc>>,
) -> Result<RestorePlan, RestoreError> {
    let keys = destination.list(&segment::generations_prefix(root))?;

    // A generation counts only once its snapshot metadata (written last) exists.
    let mut complete: Vec<(String, i64)> = Vec::new();
    for key in &keys {
        if !key.ends_with(segment::SNAPSHOT_META_OBJECT) {
            continue;
        }
        let Some(generation) = segment::generation_of_key(root, key) else {
            continue;
        };
        let Some(info) = segment::parse_generation_id(&generation) else {
            continue;
        };
        complete.push((generation, info.created_ms));
    }
    complete.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let Some((oldest_gen, oldest_ms)) = complete.first().cloned() else {
        return Err(RestoreError::NoReplica {
            root: root.to_owned(),
        });
    };
    let _ = oldest_gen;

    let target_ms = target.map_or(i64::MAX, |t| t.timestamp_millis());
    let Some((generation, generation_ms)) = complete
        .iter()
        .rev()
        .find(|(_, ms)| *ms <= target_ms)
        .cloned()
    else {
        return Err(RestoreError::TargetBeforeRetention {
            target: target.unwrap_or_else(Utc::now),
            oldest: ms_to_utc(oldest_ms),
        });
    };

    // Segment keys carry `seq` and ship time, so the selection needs no downloads.
    let segment_keys = destination.list(&segment::segments_prefix(root, &generation))?;
    let mut refs: Vec<(u64, i64, String)> = segment_keys
        .into_iter()
        .filter_map(|key| segment::parse_segment_key(&key).map(|r| (r.seq, r.created_ms, key)))
        .collect();
    refs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut segments = Vec::new();
    let mut expected_seq: u64 = 0;
    for (seq, created_ms, key) in refs {
        if created_ms > target_ms {
            break;
        }
        if seq != expected_seq {
            return Err(RestoreError::SegmentGap {
                generation,
                expected_seq,
                found_seq: seq,
            });
        }
        expected_seq = seq.saturating_add(1);
        segments.push(PlannedSegment {
            key,
            seq,
            created_at: ms_to_utc(created_ms),
        });
    }

    let generation_started_at = ms_to_utc(generation_ms);
    let effective = segments
        .last()
        .map_or(generation_started_at, |s| s.created_at);

    Ok(RestorePlan {
        snapshot_key: segment::snapshot_key(root, &generation),
        snapshot_meta_key: segment::snapshot_meta_key(root, &generation),
        generation,
        generation_started_at,
        segments,
        requested: target,
        effective,
    })
}

/// Download, verify and reconstruct the database described by `plan`, writing it
/// to `output`.
///
/// The work happens in a sibling staging directory; `output` is only written
/// once `SQLite` has replayed the WAL and passed `PRAGMA integrity_check`, so a
/// failed restore never leaves a half-built database behind (and never
/// overwrites a good one with a bad one).
///
/// # Errors
///
/// See [`RestoreError`].
pub fn apply(
    destination: &dyn ReplicaDestination,
    plan: &RestorePlan,
    output: &Path,
) -> Result<RestoreOutcome, RestoreError> {
    let staging = staging_dir(output);
    // A leftover staging directory from an interrupted run must not be reused.
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(RestoreError::io("create staging directory"))?;

    let result = build_in_staging(destination, plan, &staging);
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(RestoreError::io("create output directory"))?;
    }
    // The staged database is checkpointed, so its sidecars carry nothing; drop
    // any stale ones next to the output so SQLite cannot recover an old WAL over
    // the freshly restored file.
    let _ = std::fs::remove_file(wal::wal_path(output));
    let _ = std::fs::remove_file(wal::shm_path(output));
    let staged_db = staging.join("restored.db");
    move_file(&staged_db, output)?;
    let _ = std::fs::remove_dir_all(&staging);

    let bytes = std::fs::metadata(output).map_or(outcome.bytes, |m| m.len());
    Ok(RestoreOutcome {
        output: output.to_path_buf(),
        bytes,
        ..outcome
    })
}

/// Plan and apply in one step.
///
/// # Errors
///
/// See [`RestoreError`].
pub fn restore(
    destination: &dyn ReplicaDestination,
    root: &str,
    target: Option<DateTime<Utc>>,
    output: &Path,
) -> Result<RestoreOutcome, RestoreError> {
    let plan = plan(destination, root, target)?;
    apply(destination, &plan, output)
}

/// The staging directory a restore of `output` builds in.
fn staging_dir(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_os_string();
    name.push(".restore-staging");
    PathBuf::from(name)
}

/// Rename `from` to `to`, falling back to copy+remove across filesystems.
fn move_file(from: &Path, to: &Path) -> Result<(), RestoreError> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // A rename across filesystems fails with EXDEV; the staging directory and
    // the output can legitimately live on different mounts.
    std::fs::copy(from, to).map_err(RestoreError::io("publish restored database"))?;
    std::fs::remove_file(from).map_err(RestoreError::io("clean up staging"))
}

/// Reassemble and verify inside `staging`, returning everything but the final
/// output path (which [`apply`] fills in after publishing).
fn build_in_staging(
    destination: &dyn ReplicaDestination,
    plan: &RestorePlan,
    staging: &Path,
) -> Result<RestoreOutcome, RestoreError> {
    let meta_bytes = destination.get(&plan.snapshot_meta_key)?;
    let meta: SnapshotMeta = serde_json::from_slice(&meta_bytes).map_err(|e| RestoreError::Io {
        op: "parse snapshot metadata",
        detail: e.to_string(),
    })?;
    if meta.version > segment::LAYOUT_VERSION {
        return Err(RestoreError::UnsupportedVersion {
            version: meta.version,
        });
    }

    // Snapshot: download the gzip stream, inflate it, and verify the bytes match
    // the digest recorded when it was taken.
    let compressed = staging.join("snapshot.db.gz");
    destination.get_to_file(&plan.snapshot_key, &compressed)?;
    let db_path = staging.join("restored.db");
    let uncompressed_len = inflate_to(&compressed, &db_path)?;
    let _ = std::fs::remove_file(&compressed);
    if uncompressed_len != meta.uncompressed_len {
        return Err(RestoreError::SnapshotDigest {
            expected_len: meta.uncompressed_len,
            actual_len: uncompressed_len,
        });
    }
    let digest = sha256_file(&db_path)?;
    if digest != meta.sha256 {
        return Err(RestoreError::SnapshotDigest {
            expected_len: meta.uncompressed_len,
            actual_len: uncompressed_len,
        });
    }

    // WAL: concatenate the selected segments, checking continuity as we go.
    let wal_target = wal::wal_path(&db_path);
    let mut frames_replayed: u64 = 0;
    // The generation's salt is whatever its own segment 0 carries; every later
    // segment must agree, or frames from two WAL generations have been mixed.
    let mut expected_salt: Option<(u32, u32)> = None;
    if plan.segments.is_empty() {
        let _ = std::fs::remove_file(&wal_target);
    } else {
        let mut file =
            std::fs::File::create(&wal_target).map_err(RestoreError::io("stage the WAL"))?;
        let mut expected_offset: u64 = 0;
        for planned in &plan.segments {
            let payload = destination.get(&planned.key)?;
            let (header, raw) = segment::decode_segment(&payload)?;
            check_continuity(
                plan,
                &mut expected_salt,
                planned.seq,
                &header,
                expected_offset,
            )?;
            file.write_all(&raw)
                .map_err(RestoreError::io("stage the WAL"))?;
            expected_offset = header.end_offset;
            frames_replayed = frames_replayed.saturating_add(header.frame_count);
        }
        file.sync_all().map_err(RestoreError::io("stage the WAL"))?;
    }
    // A stale shared-memory index would make SQLite trust a WAL that is no
    // longer there.
    let _ = std::fs::remove_file(wal::shm_path(&db_path));

    // Let SQLite replay the WAL, then prove the result is a sound database
    // BEFORE it is allowed anywhere near the output path.
    let mut conn = sqlite::open(&db_path)?;
    sqlite::checkpoint_truncate(&mut conn, &db_path)?;
    sqlite::integrity_check(&mut conn)?;
    drop(conn);
    let _ = std::fs::remove_file(wal::wal_path(&db_path));
    let _ = std::fs::remove_file(wal::shm_path(&db_path));

    let bytes = std::fs::metadata(&db_path)
        .map_err(RestoreError::io("stat restored database"))?
        .len();
    Ok(RestoreOutcome {
        plan: plan.clone(),
        output: db_path,
        bytes,
        frames_replayed,
    })
}

/// Refuse a segment that does not continue the previous one or belongs to a
/// different WAL generation.
fn check_continuity(
    plan: &RestorePlan,
    expected_salt: &mut Option<(u32, u32)>,
    seq: u64,
    header: &SegmentHeader,
    expected_offset: u64,
) -> Result<(), RestoreError> {
    if header.start_offset != expected_offset {
        return Err(RestoreError::SegmentDiscontinuity {
            generation: plan.generation.clone(),
            seq,
            expected_offset,
            found_offset: header.start_offset,
        });
    }
    let salt = (header.salt1, header.salt2);
    match expected_salt {
        Some(expected) if *expected != salt => {
            return Err(RestoreError::SaltMismatch {
                generation: plan.generation.clone(),
                seq,
            });
        }
        Some(_) => {}
        None => *expected_salt = Some(salt),
    }
    Ok(())
}

/// Gunzip `source` into `target`, returning the number of bytes written.
fn inflate_to(source: &Path, target: &Path) -> Result<u64, RestoreError> {
    let input = std::fs::File::open(source).map_err(RestoreError::io("open snapshot"))?;
    let mut decoder = flate2::read::GzDecoder::new(std::io::BufReader::new(input));
    let mut output = std::fs::File::create(target).map_err(RestoreError::io("stage snapshot"))?;
    let written =
        std::io::copy(&mut decoder, &mut output).map_err(RestoreError::io("inflate snapshot"))?;
    output
        .sync_all()
        .map_err(RestoreError::io("stage snapshot"))?;
    Ok(written)
}

/// Streaming SHA-256 of a file, so a large snapshot is never buffered.
fn sha256_file(path: &Path) -> Result<String, RestoreError> {
    use sha2::{Digest as _, Sha256};
    let mut file = std::fs::File::open(path).map_err(RestoreError::io("hash snapshot"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read =
            std::io::Read::read(&mut file, &mut buf).map_err(RestoreError::io("hash snapshot"))?;
        if read == 0 {
            break;
        }
        let chunk = buf.get(..read).unwrap_or(&[]);
        hasher.update(chunk);
    }
    Ok(hex::encode(hasher.finalize()))
}
