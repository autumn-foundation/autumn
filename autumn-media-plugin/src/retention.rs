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
    if let Ok(canonical_path) = std::fs::canonicalize(path) {
        return canonical_path.starts_with(&canonical_root);
    }
    if let Some(parent) = path.parent()
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
    {
        let reconstructed = canonical_parent.join(path.file_name().unwrap_or_default());
        return reconstructed.starts_with(&canonical_root);
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

/// Delete the given (already expired, non-deferred) files, re-checking
/// [`within_root`] for each. Returns `(deleted, errors)`.
///
/// Pure synchronous filesystem work (`canonicalize` + `remove_file`), intended
/// to run under [`tokio::task::spawn_blocking`].
fn delete_expired_files(root: &Path, paths: &[PathBuf]) -> (usize, usize) {
    let mut deleted = 0;
    let mut errors = 0;
    for path in paths {
        if !within_root(root, path) {
            // Should not happen for a path collected from within root, but the
            // guard keeps a symlinked entry from escaping.
            errors += 1;
            continue;
        }
        match delete_recording_file(path) {
            Ok(()) => deleted += 1,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "media retention: failed to delete recording file");
                errors += 1;
            }
        }
    }
    (deleted, errors)
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

    // Phase 2 — delete off the executor.
    let (deleted, delete_errors) = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || delete_expired_files(&root, &to_delete))
            .await
            .unwrap_or((0, 0))
    };
    report.deleted = deleted;
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
