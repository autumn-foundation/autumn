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

use std::path::{Path, PathBuf};

use crate::capsule::capture::CaptureScope;
use crate::capsule::schema::{Capsule, CapsuleError, CapsuleOutcome};

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
    // stub: assembly + write land in the GREEN step.
    let _ = (scope, outcome);
    None
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
            .err()
            .expect("a missing capsule must be an error");
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
