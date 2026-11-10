//! Compile gate for `autumn generate teams` (#3595).
//!
//! The teams templates are string constants. No other test compiles them.
//! This test scaffolds a fresh app, runs `generate teams`, and runs the
//! generated module's own unit tests against this workspace's `autumn-web`.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Run the production `autumn` binary in `dir`, asserting success.
fn run_autumn(dir: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Point the generated app at this workspace's `autumn-web`.
fn patch_to_workspace(project: &Path) {
    let cargo_toml = project.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml).unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let path = |crate_dir: &str| {
        workspace
            .join(crate_dir)
            .display()
            .to_string()
            .replace('\\', "/")
    };
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\nautumn-macros = {{ path = \"{}\" }}\n",
        path("autumn"),
        path("autumn-macros"),
    );
    fs::write(&cargo_toml, content).unwrap();
}

/// The generated app compiles, and the generated members page redisplays
/// the invite form with field errors (its own unit tests pass).
#[test]
#[ignore = "slow: cargo-tests a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn teams_scaffold_cargo_tests() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn(tmp.path(), &["new", "teams-check-app"]);
    let project = tmp.path().join("teams-check-app");
    run_autumn(&project, &["generate", "teams"]);
    patch_to_workspace(&project);

    let output = Command::new("cargo")
        .args(["test", "teams::"])
        .current_dir(&project)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "cargo test on the teams scaffold failed:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    for test in [
        "members_content_invite_form_clean_when_no_error",
        "members_content_invite_form_shows_email_error_and_preserves_input",
        "members_content_invite_form_shows_role_error_on_role_field",
        "members_content_hides_invite_form_when_caller_cannot_manage",
    ] {
        assert!(
            stdout.contains(&format!("{test} ... ok")),
            "generated test `{test}` did not pass:\n{stdout}"
        );
    }
}
