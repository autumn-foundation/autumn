//! `autumn replay` — the CLI half of deterministic replay capsules (#1598).
//!
//! Fast tests only: they exercise argument handling and the refusal path, which
//! must happen *before* the app binary is compiled, so nothing here runs cargo.
//! The end-to-end capture/replay behaviour lives in the `autumn-web` suite
//! (`failure_capsule_end_to_end`).

use std::path::Path;
use std::process::{Command, Output};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn")
}

#[test]
fn replay_appears_in_top_level_help() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_autumn(tmp.path(), &["--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "`autumn --help` must succeed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        help.contains("replay"),
        "`autumn --help` must list the replay subcommand:\n{help}"
    );
}

#[test]
fn replay_help_documents_the_capsule_argument_and_exit_codes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_autumn(tmp.path(), &["replay", "--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "`autumn replay --help` must succeed");
    assert!(
        help.contains("capsule"),
        "`autumn replay --help` must document the capsule argument:\n{help}"
    );
    assert!(
        help.contains("Exit codes"),
        "`autumn replay --help` must document the verdict exit codes:\n{help}"
    );
    for flag in ["--package", "--bin", "--profile"] {
        assert!(
            help.contains(flag),
            "`autumn replay --help` must document {flag}:\n{help}"
        );
    }
}

#[test]
fn replay_without_capsule_arg_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_autumn(tmp.path(), &["replay"]);
    assert!(
        !out.status.success(),
        "`autumn replay` with no capsule must fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("capsule"),
        "the error must name the missing argument:\n{stderr}"
    );
}

#[test]
fn replay_refuses_a_missing_capsule_with_exit_two() {
    // Exit 2 is "refused, nothing was replayed" — and it must be reached
    // without compiling the app, so a typo'd path costs a second, not a build.
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_autumn(tmp.path(), &["replay", "no-such-capsule.json"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a capsule that does not exist must be refused with exit 2\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no-such-capsule.json"),
        "the refusal must name the path it looked for:\n{stderr}"
    );
}
