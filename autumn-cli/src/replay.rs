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
    /// Explicit `--profile`, or `None` to default to the capsule's recorded
    /// profile (falling back to `dev`).
    pub profile: Option<&'a str>,
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

    let recorded_profile = recorded_profile(&capsule);
    let profile = effective_profile(opts.profile, recorded_profile.as_deref());

    let mut command = Command::new(&binary);
    command
        .env(REPLAY_CAPSULE_ENV, &capsule)
        .env("AUTUMN_ENV", &profile)
        .env("AUTUMN_PROFILE", &profile)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command.status().unwrap_or_else(|error| {
        eprintln!("Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });

    std::process::exit(exit_code(status));
}

/// The profile the capsule recorded (`app.profile`), when it recorded one.
///
/// Best-effort: an unreadable or malformed capsule returns `None` here and is
/// then refused properly by the app binary, which owns capsule validation.
fn recorded_profile(capsule: &Path) -> Option<String> {
    let json = std::fs::read_to_string(capsule).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    value
        .get("app")?
        .get("profile")?
        .as_str()
        .map(str::to_owned)
}

/// The profile the replayed app boots with.
///
/// An explicit `--profile` wins (with a warning when it differs from the
/// recording — profile-gated routes and configuration can then legitimately
/// diverge); otherwise the capsule's recorded profile, so the replay boots the
/// way the failing run did; `dev` only when the capsule recorded none.
fn effective_profile(explicit: Option<&str>, recorded: Option<&str>) -> String {
    match (explicit, recorded) {
        (Some(explicit), Some(recorded)) if explicit != recorded => {
            eprintln!(
                "warning: replaying with --profile {explicit}, but the capsule was recorded \
                 under the {recorded:?} profile — profile-gated routes and configuration may \
                 differ from the failing run"
            );
            explicit.to_owned()
        }
        (Some(explicit), _) => explicit.to_owned(),
        (None, Some(recorded)) => recorded.to_owned(),
        (None, None) => "dev".to_owned(),
    }
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
    fn replay_defaults_to_the_capsules_recorded_profile() {
        assert_eq!(effective_profile(None, Some("prod")), "prod");
        assert_eq!(effective_profile(None, None), "dev");
        assert_eq!(effective_profile(Some("staging"), Some("prod")), "staging");
        assert_eq!(effective_profile(Some("prod"), Some("prod")), "prod");
    }

    #[test]
    fn recorded_profile_reads_app_profile_from_the_capsule_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capsule.json");
        std::fs::write(&path, r#"{"app":{"name":"shop","profile":"prod"}}"#).expect("write");
        assert_eq!(recorded_profile(&path).as_deref(), Some("prod"));

        std::fs::write(&path, r#"{"app":{"name":"shop"}}"#).expect("write");
        assert_eq!(recorded_profile(&path), None);

        std::fs::write(&path, "not json").expect("write");
        assert_eq!(
            recorded_profile(&path),
            None,
            "a malformed capsule is the app binary's refusal to make, not ours"
        );
    }

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
