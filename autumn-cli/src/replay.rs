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
    /// Explicit build kind: `Some(true)` for `--release`, `Some(false)` for
    /// `--debug`, or `None` to default to the build kind the capsule recorded
    /// (falling back to a debug build).
    pub build: Option<bool>,
    /// Cargo features forwarded to `cargo build --features`. The capsule does
    /// not record the recording binary's feature set, so feature-gated code
    /// the failure depends on needs these passed explicitly.
    pub features: Option<&'a str>,
    /// Forwarded to `cargo build --no-default-features`.
    pub no_default_features: bool,
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

    let recorded_profile = recorded_profile(&capsule);
    let profile = effective_profile(opts.profile, recorded_profile.as_deref());
    let release = effective_release(opts.build, recorded_debug_assertions(&capsule));

    eprintln!("autumn replay {}\n", capsule.display());
    compile_binary(opts, release);
    let mut binary = crate::routes::find_binary(opts.package, opts.bin);
    if release {
        binary = release_path(binary);
    }

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

/// Whether the recording binary was compiled with `debug_assertions`
/// (`app.debug_assertions`), when the capsule recorded it.
///
/// Best-effort, like [`recorded_profile`]: an unreadable or malformed capsule
/// returns `None` here and is then refused properly by the app binary.
fn recorded_debug_assertions(capsule: &Path) -> Option<bool> {
    let json = std::fs::read_to_string(capsule).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    value.get("app")?.get("debug_assertions")?.as_bool()
}

/// Whether the replay binary is compiled `--release`.
///
/// An explicit `--release`/`--debug` wins (with a warning when it differs
/// from the recording — `cfg(debug_assertions)`-gated code and release-only
/// behaviour can then legitimately diverge); otherwise the capsule's recorded
/// build kind, so a failure recorded by a release binary replays in one; a
/// debug build only when the capsule recorded none.
fn effective_release(explicit: Option<bool>, recorded_debug_assertions: Option<bool>) -> bool {
    let recorded = recorded_debug_assertions.map(|debug| !debug);
    match (explicit, recorded) {
        (Some(explicit), Some(recorded)) if explicit != recorded => {
            eprintln!(
                "warning: replaying with a {} build, but the capsule was recorded by a {} \
                 build — `cfg(debug_assertions)` code paths and release-only behaviour may \
                 differ from the failing run",
                build_kind(explicit),
                build_kind(recorded)
            );
            explicit
        }
        (Some(explicit), _) => explicit,
        (None, Some(recorded)) => recorded,
        (None, None) => false,
    }
}

/// Human name for a build kind, for the mismatch warning.
const fn build_kind(release: bool) -> &'static str {
    if release { "release" } else { "debug" }
}

/// Compile the app binary with the replay's build configuration.
///
/// A plain `cargo build` would produce a default-features debug binary no
/// matter how the failing binary was built, and `cfg(debug_assertions)`- or
/// feature-gated code would then diverge from the recording before the
/// request is even replayed.
fn compile_binary(opts: &ReplayOptions<'_>, release: bool) {
    let mut cargo = Command::new("cargo");
    cargo.arg("build");
    if release {
        cargo.arg("--release");
    }
    if let Some(features) = opts.features {
        cargo.args(["--features", features]);
    }
    if opts.no_default_features {
        cargo.arg("--no-default-features");
    }
    if let Some(pkg) = opts.package {
        cargo.args(["-p", pkg]);
    }
    if let Some(bin) = opts.bin {
        cargo.args(["--bin", bin]);
    }

    let status = cargo.status().expect("failed to run cargo build");
    if !status.success() {
        eprintln!("\u{2717} Compilation failed");
        std::process::exit(1);
    }
}

/// Swap a resolved `<target>/debug/<bin>` path to the release directory.
fn release_path(path: PathBuf) -> PathBuf {
    if let (Some(target_dir), Some(bin)) = (path.parent().and_then(Path::parent), path.file_name())
    {
        return target_dir.join("release").join(bin);
    }
    path
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
    fn replay_defaults_to_the_capsules_recorded_build_kind() {
        // Recorded debug_assertions = false means a release build was failing.
        assert!(effective_release(None, Some(false)));
        assert!(!effective_release(None, Some(true)));
        // Old capsules recorded nothing: a debug build, as before.
        assert!(!effective_release(None, None));
        // An explicit flag always wins.
        assert!(effective_release(Some(true), Some(true)));
        assert!(!effective_release(Some(false), Some(false)));
    }

    #[test]
    fn recorded_debug_assertions_reads_app_debug_assertions_from_the_capsule_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capsule.json");
        std::fs::write(
            &path,
            r#"{"app":{"name":"shop","profile":"prod","debug_assertions":false}}"#,
        )
        .expect("write");
        assert_eq!(recorded_debug_assertions(&path), Some(false));

        std::fs::write(&path, r#"{"app":{"name":"shop"}}"#).expect("write");
        assert_eq!(
            recorded_debug_assertions(&path),
            None,
            "capsules recorded before the field existed carry no build kind"
        );
    }

    #[test]
    fn release_path_swaps_only_the_profile_directory() {
        let debug = PathBuf::from("/work/target/debug/app");
        assert_eq!(
            release_path(debug),
            PathBuf::from("/work/target/release/app")
        );
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
