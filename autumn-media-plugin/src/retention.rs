//! Generic, harvest-free recording-retention sweep.
//!
//! The generalization of Arroyo's recording-retention service
//! (`src/services/recordings.rs`): an hourly loop
//! that deletes recording files older than a configured retention window and
//! leaves everything else alone. Arroyo keyed expiry on a broadcast row's
//! `ended_at` and, after deleting, cleared `recording_path`; the generic form
//! here has no application schema, so it keys expiry on each file's **mtime**
//! and only owns the filesystem side — deleting individual expired files inside
//! a configured recordings root.
//!
//! The one application-specific decision Arroyo made — *defer* a file that is
//! still referenced by an in-progress encode workflow — is exposed as an
//! app-overridable [`RetentionDefer`] predicate: when it returns `true` for a
//! path, the sweep skips that file this tick and retries on the next.
//!
//! Safety mirrors Arroyo's sweep: only regular **files** are removed
//! (directories are left to the ingest layer / `MediaMTX`'s own
//! `recordDeleteAfter`), a missing file is treated as success (idempotent), and
//! every candidate is checked to be [`within_root`] before deletion.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Seconds between retention sweeps (hourly, matching Arroyo).
const SWEEP_INTERVAL_SECONDS: u64 = 3600;

/// Seconds in a day.
const SECONDS_PER_DAY: u64 = 86_400;

/// An app-overridable predicate: given a candidate recording path, return
/// `true` to **defer** its deletion (e.g. a still-encoding workflow references
/// it). Deferred files are retried on the next sweep.
pub type RetentionDefer =
    Arc<dyn Fn(PathBuf) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// The outcome tallies of one sweep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Expired files deleted this sweep.
    pub deleted: usize,
    /// Expired files skipped because the defer predicate held them.
    pub deferred: usize,
    /// Files kept because they are not yet expired (or their mtime is
    /// unreadable).
    pub kept: usize,
    /// Files that could not be inspected or deleted.
    pub errors: usize,
}

/// The retention window as a [`Duration`].
fn retention_window(retention_days: u32) -> Duration {
    Duration::from_secs(u64::from(retention_days) * SECONDS_PER_DAY)
}

/// Compute when a recording expires given its `modified` time and the window.
#[must_use]
pub fn recording_expires_at(modified: SystemTime, retention_days: u32) -> SystemTime {
    modified
        .checked_add(retention_window(retention_days))
        .unwrap_or(modified)
}

/// Whether a recording modified at `modified` is expired as of `now`.
#[must_use]
pub fn is_expired(modified: SystemTime, now: SystemTime, retention_days: u32) -> bool {
    recording_expires_at(modified, retention_days) <= now
}

/// Whether `path` resolves inside `root`.
///
/// Prevents a symlink or a caller-supplied path from escaping the recordings
/// root. Resolution order: lexical prefix (fast path), then canonicalize both,
/// then canonicalize the parent and reconstruct (covers an already-deleted
/// file whose parent still exists).
#[must_use]
pub fn within_root(root: &Path, path: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    let Ok(canonical_root) = std::fs::canonicalize(root) else {
        return false;
    };
    within_canonical_root(&canonical_root, path)
}

/// Strict containment check with **no lexical fast path**, used on the delete
/// path (see [`delete_expired_files`]): fully resolve `path` — following
/// symlinks in every component, including a parent directory that may have been
/// swapped for a symlink after the scan — and confirm it is still inside the
/// already-canonicalized `canonical_root`. A file whose parent is gone is
/// reconstructed from its canonical parent so an in-flight concurrent delete is
/// still judged correctly.
fn within_canonical_root(canonical_root: &Path, path: &Path) -> bool {
    if let Ok(canonical_path) = std::fs::canonicalize(path) {
        return canonical_path.starts_with(canonical_root);
    }
    if let Some(parent) = path.parent()
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
    {
        let reconstructed = canonical_parent.join(path.file_name().unwrap_or_default());
        return reconstructed.starts_with(canonical_root);
    }
    false
}

/// Delete a single recording **file**.
///
/// Directories are left alone (the ingest layer owns segment-directory
/// cleanup), and a missing file is treated as success so the sweep is
/// idempotent against concurrent cleanup.
fn delete_recording_file(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Recursively collect regular files under `root`.
fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => collect_files(&path, out),
            Ok(file_type) if file_type.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// The result of the synchronous scan phase: expired candidates plus the tallies
/// for files decided without needing the (async) defer hook.
#[derive(Default)]
struct RecordingScan {
    /// Expired files awaiting the async defer decision + deletion.
    expired: Vec<PathBuf>,
    /// Files kept because not yet expired or with an unreadable mtime.
    kept: usize,
    /// Files whose metadata could not be read.
    errors: usize,
}

/// Traverse `root` and classify each regular file as expired / kept / error.
///
/// Pure synchronous filesystem work (`read_dir` + `metadata`), intended to run
/// under [`tokio::task::spawn_blocking`] so it never blocks the async executor.
/// The async defer decision and deletion are handled by the caller.
fn scan_recordings_root(root: &Path, retention_days: u32, now: SystemTime) -> RecordingScan {
    let mut scan = RecordingScan::default();
    let mut files = Vec::new();
    collect_files(root, &mut files);
    for path in files {
        let Ok(metadata) = std::fs::metadata(&path) else {
            scan.errors += 1;
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            scan.kept += 1;
            continue;
        };
        if is_expired(modified, now, retention_days) {
            scan.expired.push(path);
        } else {
            scan.kept += 1;
        }
    }
    scan
}

/// Delete the given (already expired, non-deferred) files. Returns
/// `(deleted, kept, errors)`.
///
/// Pure synchronous filesystem work (`canonicalize` + `metadata` + `remove_file`),
/// intended to run under [`tokio::task::spawn_blocking`]. Because the async
/// defer hook awaits between the scan and this phase, each candidate is
/// re-validated immediately before unlinking:
///
/// * **Containment (TOCTOU on the parent):** a strict, non-lexical canonical
///   check ([`within_canonical_root`]) — a parent directory swapped for a
///   symlink after the scan would still satisfy the lexical [`within_root`] fast
///   path, so re-resolve here and skip anything that now escapes `root`.
/// * **Freshness (TOCTOU on the file):** re-`stat` and re-check expiry against
///   the same `now`/`retention_days` used by the scan — a file touched or
///   replaced during the await may no longer be expired and must not be deleted
///   on the stale decision. A vanished/again-fresh file is a normal race
///   outcome, not an error.
fn delete_expired_files(
    root: &Path,
    paths: &[PathBuf],
    retention_days: u32,
    now: SystemTime,
) -> (usize, usize, usize) {
    let mut deleted = 0;
    let mut kept = 0;
    let mut errors = 0;
    let canonical_root = std::fs::canonicalize(root).ok();
    for path in paths {
        // Strict canonical containment when root canonicalizes; otherwise fall
        // back to the lexical guard rather than mass-deleting on an
        // unverifiable root.
        let contained = canonical_root.as_ref().map_or_else(
            || within_root(root, path),
            |canonical_root| within_canonical_root(canonical_root, path),
        );
        if !contained {
            // A parent was swapped for a symlink escaping root (or the entry is
            // otherwise no longer within it): refuse to follow it out.
            errors += 1;
            continue;
        }
        match std::fs::metadata(path) {
            Ok(metadata) => match metadata.modified() {
                Ok(modified) if is_expired(modified, now, retention_days) => {}
                // Freshened between scan and delete, or its mtime is no longer
                // readable — do not delete on an unconfirmed decision.
                Ok(_) | Err(_) => {
                    kept += 1;
                    continue;
                }
            },
            // Vanished (or now unreadable) since the scan — a benign race, skip.
            Err(_) => continue,
        }
        match delete_recording_file(path) {
            Ok(()) => deleted += 1,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "media retention: failed to delete recording file");
                errors += 1;
            }
        }
    }
    (deleted, kept, errors)
}

/// Sweep `root` once, deleting recording files older than the retention window.
///
/// `now` is taken as a parameter (never the wall clock) so the decision is
/// deterministic for tests. A `retention_days` of `0` disables the sweep and
/// returns an empty report. `defer`, when provided, can hold back an
/// otherwise-expired file for the next tick.
///
/// The blocking filesystem work (directory traversal + `metadata`, then
/// `canonicalize` + `remove_file`) runs on [`tokio::task::spawn_blocking`]
/// threads so it never stalls the async executor; the app-overridable async
/// [`RetentionDefer`] hook is consulted between the two phases, preserving its
/// per-file semantics.
pub async fn sweep_recordings_root(
    root: &Path,
    retention_days: u32,
    now: SystemTime,
    defer: Option<&RetentionDefer>,
) -> RetentionReport {
    let mut report = RetentionReport::default();
    if retention_days == 0 {
        return report;
    }

    // Phase 1 — scan off the executor.
    let scan = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || scan_recordings_root(&root, retention_days, now))
            .await
            .unwrap_or_default()
    };
    report.kept = scan.kept;
    report.errors = scan.errors;

    // Between phases — consult the async defer hook per expired file.
    let mut to_delete = Vec::with_capacity(scan.expired.len());
    for path in scan.expired {
        if let Some(defer) = defer
            && defer(path.clone()).await
        {
            report.deferred += 1;
            continue;
        }
        to_delete.push(path);
    }

    // Phase 2 — delete off the executor, re-validating each candidate against
    // the scan→delete race window (containment + freshness).
    let (deleted, kept, delete_errors) = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            delete_expired_files(&root, &to_delete, retention_days, now)
        })
        .await
        .unwrap_or((0, 0, 0))
    };
    report.deleted = deleted;
    report.kept += kept;
    report.errors += delete_errors;

    report
}

/// Spawn the hourly retention sweep loop over `root`.
///
/// A `retention_days` of `0` disables retention: no loop is spawned. The loop
/// tolerates transient failures (each tick is independent) and never panics.
pub fn spawn_retention_sweep_loop(
    root: PathBuf,
    retention_days: u32,
    defer: Option<RetentionDefer>,
) {
    if retention_days == 0 {
        tracing::info!("media retention: disabled (retention_days = 0); sweep not spawned");
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECONDS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let report =
                sweep_recordings_root(&root, retention_days, SystemTime::now(), defer.as_ref())
                    .await;
            if report.deleted > 0 || report.errors > 0 || report.deferred > 0 {
                tracing::info!(
                    deleted = report.deleted,
                    deferred = report.deferred,
                    errors = report.errors,
                    "media retention sweep completed"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        RetentionDefer, is_expired, recording_expires_at, sweep_recordings_root, within_root,
    };
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    const DAY: u64 = 86_400;

    fn write_with_mtime(path: &std::path::Path, contents: &[u8], age_secs: u64) {
        std::fs::write(path, contents).expect("write");
        let modified = SystemTime::now() - Duration::from_secs(age_secs);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(modified).expect("set_modified");
    }

    #[test]
    fn expiry_boundary_is_days_after_modified() {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let expires = recording_expires_at(modified, 14);
        assert_eq!(expires, modified + Duration::from_secs(14 * DAY));

        let now = SystemTime::now();
        let old = now - Duration::from_secs(100 * DAY);
        assert!(is_expired(old, now, 14));
        assert!(!is_expired(old, now, 200));
    }

    #[tokio::test]
    async fn sweep_deletes_expired_and_keeps_fresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = temp.path().join("old.mp4");
        let fresh = temp.path().join("fresh.mp4");
        write_with_mtime(&old, b"old", 100 * DAY);
        write_with_mtime(&fresh, b"fresh", 60);

        let report = sweep_recordings_root(temp.path(), 14, SystemTime::now(), None).await;

        assert_eq!(report.deleted, 1);
        assert_eq!(report.kept, 1);
        assert!(!old.exists(), "expired file should be deleted");
        assert!(fresh.exists(), "fresh file should be kept");
    }

    #[tokio::test]
    async fn sweep_honors_defer_hook() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = temp.path().join("held.mp4");
        write_with_mtime(&old, b"held", 100 * DAY);

        let defer: RetentionDefer = Arc::new(|_path| Box::pin(async { true }));
        let report = sweep_recordings_root(temp.path(), 14, SystemTime::now(), Some(&defer)).await;

        assert_eq!(report.deferred, 1);
        assert_eq!(report.deleted, 0);
        assert!(old.exists(), "deferred file must survive the sweep");
    }

    #[tokio::test]
    async fn sweep_skips_file_freshened_between_scan_and_delete() {
        let temp = tempfile::tempdir().expect("tempdir");
        let racy = temp.path().join("racy.mp4");
        let normal = temp.path().join("normal.mp4");
        write_with_mtime(&racy, b"racy", 100 * DAY);
        write_with_mtime(&normal, b"normal", 100 * DAY);

        // The defer hook runs between the scan (phase 1) and the delete
        // (phase 2). Use it to freshen `racy`'s mtime mid-sweep — standing in
        // for a concurrent touch/replace during the async await — while never
        // actually deferring.
        let racy_for_defer = racy.clone();
        let defer: RetentionDefer = Arc::new(move |path| {
            let racy = racy_for_defer.clone();
            Box::pin(async move {
                if path == racy {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&racy)
                        .expect("open for mtime")
                        .set_modified(SystemTime::now())
                        .expect("set_modified");
                }
                false
            })
        });

        let report = sweep_recordings_root(temp.path(), 14, SystemTime::now(), Some(&defer)).await;

        assert!(
            racy.exists(),
            "a file freshened between scan and delete must not be deleted"
        );
        assert!(!normal.exists(), "a still-expired file is still deleted");
        assert_eq!(report.deleted, 1);
        assert_eq!(
            report.kept, 1,
            "the freshened file counts as kept, not deleted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sweep_rejects_parent_symlink_swap_on_delete() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(root.join("subdir")).expect("mkdir subdir");
        std::fs::create_dir_all(&outside).expect("mkdir outside");

        // A fresh file outside root, sharing the candidate's file name. If the
        // delete path followed the swapped parent symlink it would be unlinked.
        let victim = outside.join("seg.mp4");
        write_with_mtime(&victim, b"victim", 60);

        let expired = root.join("subdir").join("seg.mp4");
        write_with_mtime(&expired, b"seg", 100 * DAY);

        // Between scan and delete, swap root/subdir (a real dir at scan time)
        // for a symlink to `outside`. root/subdir/seg.mp4 still satisfies the
        // lexical within_root fast path but now resolves outside root.
        let root_subdir = root.join("subdir");
        let outside_for_defer = outside.clone();
        let defer: RetentionDefer = Arc::new(move |_path| {
            let root_subdir = root_subdir.clone();
            let outside = outside_for_defer.clone();
            Box::pin(async move {
                std::fs::remove_dir_all(&root_subdir).expect("rm subdir");
                symlink(&outside, &root_subdir).expect("symlink");
                false
            })
        });

        let report = sweep_recordings_root(&root, 14, SystemTime::now(), Some(&defer)).await;

        assert!(
            victim.exists(),
            "a file outside root must never be unlinked via a swapped parent symlink"
        );
        assert_eq!(
            report.deleted, 0,
            "nothing inside root was actually deleted"
        );
    }

    #[tokio::test]
    async fn sweep_disabled_when_retention_zero() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = temp.path().join("old.mp4");
        write_with_mtime(&old, b"old", 100 * DAY);

        let report = sweep_recordings_root(temp.path(), 0, SystemTime::now(), None).await;

        assert_eq!(report, super::RetentionReport::default());
        assert!(old.exists(), "disabled retention deletes nothing");
    }

    #[test]
    fn within_root_rejects_outside_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(within_root(temp.path(), &temp.path().join("live/seg.mp4")));
        assert!(!within_root(
            temp.path(),
            std::path::Path::new("/etc/hostname")
        ));
    }
}
