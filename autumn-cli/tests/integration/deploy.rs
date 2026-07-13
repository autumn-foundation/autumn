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
