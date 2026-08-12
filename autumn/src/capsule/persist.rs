//! Writing capsules to disk, and reading them back.
//!
//! Capsules land in a plain directory of JSON files (`tmp/autumn-capsules` by
//! default, project-relative like the maintenance flag file). Because the
//! contents are real production request data, the writer is deliberately
//! conservative: owner-only permissions on unix, a temp-then-rename so a
//! reader never sees a half-written file, and an oldest-first prune so an
//! error storm cannot fill a disk.
//!
//! Persistence is best-effort by construction. Every failure path logs and
//! returns `None`: a capsule that cannot be written must never turn a 500 into
//! a worse 500.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
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
    )
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;

use crate::capsule::capture::CaptureScope;
use crate::capsule::schema::{
    AppInfo, CAPSULE_FORMAT_VERSION, Capsule, CapsuleError, CapsuleOutcome,
};

/// Where a persisted capsule ended up.
///
/// Carried on [`ErrorEvent::capsule`](crate::reporting::ErrorEvent::capsule) so
/// a reporter can attach the path (or the id) to whatever it ships upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRef {
    /// Capsule id — the request id when one was available.
    pub id: String,
    /// Absolute or project-relative path to the written capsule.
    pub path: PathBuf,
}

/// Resolve the capsule directory from the configured path.
#[must_use]
pub fn capsule_dir(dir: &str) -> PathBuf {
    PathBuf::from(dir)
}

/// Write the capsule for a finished request, returning where it landed.
///
/// Returns `None` when the capsule could not be written; the reason is logged
/// at `error` level and never propagated to the request.
#[must_use]
pub fn persist(scope: &CaptureScope, outcome: CapsuleOutcome) -> Option<CapsuleRef> {
    let capsule = assemble(scope, outcome)?;
    let settings = scope.settings();
    let dir = capsule_dir(&settings.dir);

    let json = match serde_json::to_vec_pretty(&capsule) {
        Ok(json) => json,
        Err(error) => {
            tracing::error!(%error, "failure capsule could not be serialized; dropping it");
            return None;
        }
    };

    let path = dir.join(file_name(&capsule));
    if let Err(error) = write_atomically(&dir, &path, &json) {
        tracing::error!(
            %error,
            path = %path.display(),
            "failure capsule could not be written; the failure itself is still reported"
        );
        return None;
    }
    prune(&dir, settings.max_capsules);

    Some(CapsuleRef {
        id: capsule.id,
        path,
    })
}

/// Turn a finished scope into the capsule document.
///
/// Returns `None` when the capture layer never recorded a request for this
/// scope — there is nothing replayable to write.
fn assemble(scope: &CaptureScope, outcome: CapsuleOutcome) -> Option<Capsule> {
    let raw = scope.raw_request()?;
    let (request, redacted) = crate::capsule::redact::redact_request(raw, scope.filter());

    let mut db = scope.db_snapshot();
    if let Some(db) = db.as_mut() {
        for tape in &mut db.connections {
            for exchange in tape
                .prologue
                .iter_mut()
                .chain(tape.statements.iter_mut())
                .chain(tape.catalog.iter_mut())
                .chain(tape.exchanges.iter_mut())
            {
                crate::capsule::redact::mask_binds(&mut exchange.binds, &redacted);
            }
        }
    }

    let settings = scope.settings();
    Some(Capsule {
        format_version: CAPSULE_FORMAT_VERSION,
        id: scope.id().to_owned(),
        captured_at: Utc::now(),
        autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
        app: AppInfo {
            name: settings.app_name.clone(),
            profile: settings.profile.clone(),
        },
        request,
        outcome,
        clock: scope.clock_readings(),
        db,
        truncated: scope.is_truncated(),
        notes: scope.notes(),
    })
}

/// Capsule file name: sortable timestamp, a process-local sequence number to
/// break ties within the same microsecond, then the capsule id.
fn file_name(capsule: &Capsule) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = capsule.captured_at.format("%Y%m%dT%H%M%S%.6f");
    let id = sanitize_id(&capsule.id);
    format!("{stamp}-{sequence:06}-{id}.json")
}

/// Reduce an id to characters that are safe in a file name.
fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if sanitized.is_empty() {
        "capsule".to_owned()
    } else {
        sanitized
    }
}

/// Write owner-only, through a temp file, so a reader never sees a partial
/// capsule and the contents are never group- or world-readable.
fn write_atomically(dir: &Path, path: &Path, json: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let temp = path.with_extension("json.tmp");

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    {
        let mut file = options.open(&temp)?;
        file.write_all(json)?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)
}

/// Delete the oldest capsules beyond the retention cap.
///
/// File names begin with a sortable timestamp, so lexical order is
/// chronological order.
fn prune(dir: &Path, max_capsules: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    if names.len() <= max_capsules {
        return;
    }
    names.sort();
    let excess = names.len().saturating_sub(max_capsules);
    for path in names.into_iter().take(excess) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(
                %error,
                path = %path.display(),
                "failure capsule could not be pruned"
            );
        }
    }
}

/// Read a capsule back from disk.
///
/// # Errors
///
/// Returns [`CapsuleError`] when the file cannot be read, is not a capsule, or
/// was written by an incompatible format version.
pub fn load_capsule(path: &Path) -> Result<Capsule, CapsuleError> {
    let json = std::fs::read_to_string(path).map_err(CapsuleError::Io)?;
    Capsule::from_json(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_dir_is_project_relative_by_default() {
        assert_eq!(
            capsule_dir("tmp/autumn-capsules"),
            PathBuf::from("tmp/autumn-capsules")
        );
    }

    #[test]
    fn load_capsule_rejects_a_missing_file() {
        let error = load_capsule(Path::new("does/not/exist.json"))
            .expect_err("a missing capsule must be an error");
        assert!(matches!(error, CapsuleError::Io(_)));
    }

    #[test]
    fn load_capsule_round_trips_a_written_capsule() {
        use crate::capsule::schema::test_support;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capsule.json");
        let capsule = test_support::capsule(
            test_support::request("GET", "/boom"),
            CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: None,
            },
        );
        std::fs::write(
            &path,
            serde_json::to_string(&capsule).expect("capsule serializes"),
        )
        .expect("fixture writes");

        let loaded = load_capsule(&path).expect("a freshly written capsule must load back");
        assert_eq!(loaded.request.uri, "/boom");
    }
}
