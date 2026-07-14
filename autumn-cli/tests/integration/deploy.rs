//! Integration tests for `autumn deploy` (issue #1607).
//!
//! Exercise the locally-verifiable spine: `deploy plan` renders the systemd
//! unit and the ordered step list, `deploy check` fails fast with an actionable
//! message when `[deploy] host` is unset, and the group exposes `--help`.

use std::fs;

use crate::common::{run_autumn, run_autumn_fail};
use tempfile::TempDir;

/// A minimal project directory with a `Cargo.toml` (for the package-name
/// default) and an `autumn.toml` containing the given `[deploy]` body.
fn project(deploy_body: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("create temp project dir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(
        dir.path().join("autumn.toml"),
        format!("[deploy]\n{deploy_body}"),
    )
    .expect("write autumn.toml");
    dir
}

#[test]
fn deploy_plan_prints_unit_and_steps() {
    let dir = project("host = \"203.0.113.10\"\n");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["deploy", "plan"], &[]);
    assert_eq!(
        code,
        Some(0),
        "deploy plan should succeed\nstderr:\n{stderr}"
    );

    // Renders the systemd unit with the resolved paths and an EnvironmentFile
    // (secrets are never inlined into the unit). ExecStart runs the uploaded
    // standalone app binary at the `current` symlink directly — NOT
    // `autumn serve --release` (which would rebuild from source).
    assert!(
        stdout.contains("ExecStart=/srv/autumn/demoapp/current/demoapp"),
        "stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("autumn serve --release"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("WantedBy=multi-user.target"),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("EnvironmentFile=/srv/autumn/demoapp/shared/autumn.env"),
        "stdout:\n{stdout}"
    );

    // Renders the ordered deploy steps, with migrations before cutover.
    let migrate = stdout.find("[migrate]").expect("migrate step present");
    let cutover = stdout.find("[cutover]").expect("cutover step present");
    assert!(
        migrate < cutover,
        "migrations must precede cutover\nstdout:\n{stdout}"
    );
    assert!(stdout.contains("[readiness-gate]"), "stdout:\n{stdout}");
    assert!(stdout.contains("[prune]"), "stdout:\n{stdout}");
}

#[test]
fn deploy_check_fails_fast_without_host() {
    // A bare [deploy] table has no host; check must fail with an actionable
    // message naming the key to set, and exit non-zero.
    let dir = project("");
    let (stdout, stderr) = run_autumn_fail(dir.path(), &["deploy", "check"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("host"),
        "check should mention the missing host\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("[deploy] host"),
        "check should name the config key\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn doctor_fails_offline_on_deploy_without_host() {
    // A `[deploy]` table with no host makes `autumn deploy check` fail
    // immediately, so default/OFFLINE `autumn doctor` (no `--online`) must fail
    // on it too — the host-present validation runs offline, only the TCP probe
    // is gated behind `--online`.
    let dir = project("");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["doctor"], &[]);
    let combined = format!("{stdout}{stderr}");
    assert_ne!(
        code,
        Some(0),
        "offline doctor must fail on a hostless [deploy]\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("deploy_host"),
        "doctor must surface the offline deploy_host check\n{combined}"
    );
    assert!(
        combined.contains("[deploy] host"),
        "doctor must name the config key to set\n{combined}"
    );
}

#[test]
fn doctor_reads_deploy_host_from_dotenv() {
    // Regression: doctor must layer .env like `deploy check` (Codex round-10 P2)
    // — bare OsEnv would skip this. With NO `[deploy]` in autumn.toml and the
    // deploy host supplied ONLY via `AUTUMN_DEPLOY__HOST` in a `.env` file, the
    // profile-aware dotenv overlay must materialize the deploy config so the
    // `deploy_host` preflight RUNS (and passes) instead of being skipped — which
    // is exactly what happens if doctor resolves through a bare `OsEnv`.
    let dir = tempfile::tempdir().expect("create temp project dir");
    // Package name for the app-name default; deliberately NO `[deploy]` section
    // in autumn.toml so that, absent dotenv, the deploy preflight is skipped.
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"demoapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(dir.path().join("autumn.toml"), "").expect("write autumn.toml");
    // Host arrives ONLY through `.env` — never the process env — so the test
    // proves the dotenv overlay path, not an OS-env read.
    fs::write(
        dir.path().join(".env"),
        "AUTUMN_DEPLOY__HOST=deploy.example.test\n",
    )
    .expect("write .env");

    // `AUTUMN_DOTENV=1` force-loads `.env` regardless of the resolved profile so
    // the test is deterministic; it does NOT carry the deploy host itself.
    let (stdout, stderr, _code) = run_autumn(dir.path(), &["doctor"], &[("AUTUMN_DOTENV", "1")]);
    let combined = format!("{stdout}{stderr}");
    // Passing check renders as `✅ deploy_host — deploy target host is configured`
    // (see `format_check_line` / `grade_deploy_host_present`). Its presence means
    // the env-only host materialized a deploy config and the preflight ran.
    assert!(
        combined.contains("deploy_host — deploy target host is configured"),
        "doctor must resolve the .env-only deploy host and run the deploy_host \
         check (bare OsEnv would skip it)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn deploy_help_lists_subcommands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (stdout, stderr, code) = run_autumn(dir.path(), &["deploy", "--help"], &[]);
    assert_eq!(
        code,
        Some(0),
        "deploy --help should succeed\nstderr:\n{stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("check"),
        "help should list check\n{combined}"
    );
    assert!(
        combined.contains("plan"),
        "help should list plan\n{combined}"
    );
    assert!(
        combined.contains("rollback"),
        "help should list rollback\n{combined}"
    );
}
