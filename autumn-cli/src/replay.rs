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
///
/// Compiles the app binary and runs it with `AUTUMN_REPLAY_CAPSULE` set, the
/// same delegation `autumn task` uses: the application — not the CLI — knows
/// its routes, state and configuration, so it is the only thing that can
/// rebuild itself around a recorded request. The child prints the verdict
/// (JSON on stdout, a human summary on stderr) and this process exits with the
/// child's code.
///
/// Unlike `autumn task`, no managed-Postgres environment is applied: a replay
/// serves its database from the capsule and must not attach to a live cluster.
pub fn run(opts: &ReplayOptions<'_>) {
    // Resolved before anything is compiled, so a typo'd path costs a second
    // rather than a build.
    let capsule = resolve_capsule_path(Path::new(opts.capsule)).unwrap_or_else(|error| {
        eprintln!("autumn replay: {error}");
        std::process::exit(EXIT_REFUSED);
    });

    eprintln!("autumn replay {}\n", capsule.display());
    crate::routes::compile_binary(opts.package, opts.bin);
    let binary = crate::routes::find_binary(opts.package, opts.bin);

    let mut command = Command::new(&binary);
    command
        .env(REPLAY_CAPSULE_ENV, &capsule)
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command.status().unwrap_or_else(|error| {
        eprintln!("Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });

    std::process::exit(exit_code(status));
}

/// Resolve the capsule argument to an absolute path.
///
/// Absolute because the child is free to resolve relative paths against its own
/// working directory, and because the path is echoed back in the verdict.
fn resolve_capsule_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_dir() {
        return Err(format!(
            "{} is a directory, not a capsule — pass the capsule JSON file inside it",
            path.display()
        ));
    }
    if !path.is_file() {
        return Err(format!(
            "no capsule at {} — pass the path to a capsule written by `[failure_capture]` \
             (default directory: tmp/autumn-capsules)",
            path.display()
        ));
    }
    std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve {}: {error}", path.display()))
}

/// The exit code to forward for a finished replay child.
///
/// The child's code *is* the verdict — 0 reproduced, 1 diverged or mismatched,
/// 2 refused — so it is passed through untouched. A child killed by a signal
/// has no code to forward and reports plain failure.
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
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
        assert_eq!(exit_code(ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(ExitStatus::from_raw(1 << 8)), 1);
        assert_eq!(exit_code(ExitStatus::from_raw(2 << 8)), 2);
        // Killed by a signal: no code to forward, report failure.
        assert_eq!(exit_code(ExitStatus::from_raw(9)), 1);
    }
}
