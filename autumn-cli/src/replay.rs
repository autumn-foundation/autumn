//! `autumn replay` -- replay a failure capsule against the application.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// Environment variable the app binary reads to select replay mode.
pub const REPLAY_CAPSULE_ENV: &str = "AUTUMN_REPLAY_CAPSULE";

/// Exit code used for a capsule that is never replayed at all.
const EXIT_REFUSED: i32 = 2;

/// Options controlling `autumn replay`.
pub struct ReplayOptions<'a> {
    pub capsule: &'a str,
    pub package: Option<&'a str>,
    pub bin: Option<&'a str>,
    pub profile: &'a str,
}

/// Run `autumn replay`.
pub fn run(opts: &ReplayOptions<'_>) {
    let _ = opts;
    eprintln!("autumn replay is not implemented");
    std::process::exit(1);
}

/// Resolve the capsule argument to an absolute path.
fn resolve_capsule_path(path: &Path) -> Result<PathBuf, String> {
    let _ = path;
    Err("capsule resolution is not implemented".to_string())
}

/// The exit code to forward for a finished replay child.
const fn exit_code(status: &ExitStatus) -> i32 {
    let _ = status;
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_capsule_path_rejects_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.json");
        let error = resolve_capsule_path(&missing).expect_err("a missing capsule must be rejected");
        assert!(
            error.contains("nope.json"),
            "the error must name the path: {error}"
        );
        assert!(
            error.contains("tmp/autumn-capsules"),
            "the error must point at the capsule directory: {error}"
        );
    }

    #[test]
    fn resolve_capsule_path_returns_an_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let capsule = dir.path().join("capsule.json");
        std::fs::write(&capsule, "{}").expect("write capsule");
        let resolved = resolve_capsule_path(&capsule).expect("an existing capsule resolves");
        assert!(
            resolved.is_absolute(),
            "the child may run elsewhere: {}",
            resolved.display()
        );
    }

    #[test]
    fn resolve_capsule_path_rejects_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = resolve_capsule_path(dir.path()).expect_err("a directory is not a capsule");
        assert!(
            error.contains("directory"),
            "the error must say what is wrong: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_child_verdict_exit_code_is_forwarded_verbatim() {
        use std::os::unix::process::ExitStatusExt as _;

        // 0 reproduced, 1 diverged/mismatched, 2 refused — a collapsed 2 would
        // make "this capsule was never replayed" indistinguishable from "the
        // bug is gone".
        assert_eq!(exit_code(&ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(&ExitStatus::from_raw(1 << 8)), 1);
        assert_eq!(exit_code(&ExitStatus::from_raw(2 << 8)), 2);
        // Killed by a signal: no code to forward, report failure.
        assert_eq!(exit_code(&ExitStatus::from_raw(9)), 1);
    }
}
