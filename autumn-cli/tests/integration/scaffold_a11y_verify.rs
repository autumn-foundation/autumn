//! Issue #3395: `autumn a11y verify` passes on the forms the scaffold
//! generator writes, for the standard `form_for` path and the
//! `--live-validation` raw-markup path.

use std::path::Path;
use std::process::Command;

fn run_autumn(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn")
}

fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed (exit={:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Scaffold `Widget` with every field kind, then run `a11y verify` on `src/`.
fn assert_scaffold_verifies(extra: &[&str]) {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_ok(&run_autumn(tmp.path(), &["new", "a11yapp"]), "autumn new");
    let project = tmp.path().join("a11yapp");
    assert_ok(
        &run_autumn(&project, &["generate", "model", "Post", "title:String"]),
        "generate model",
    );
    let mut args = vec![
        "generate",
        "scaffold",
        "Widget",
        "name:String",
        "email:String{email}",
        "description:Text",
        "summary:Text{max=200}",
        "quantity:i32",
        "price:decimal{10,2}",
        "active:bool",
        "flag:Option<bool>",
        "published_on:NaiveDateTime",
        "status:enum{draft,live}",
        "post:references",
        "notes:Option<String>",
        "body:richtext",
    ];
    args.extend_from_slice(extra);
    assert_ok(&run_autumn(&project, &args), "generate scaffold");

    let out = run_autumn(&project, &["a11y", "verify", "src", "--format", "json"]);
    assert_ok(&out, "a11y verify");
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("a11y verify emits JSON");
    let findings = report["findings"].as_array().expect("findings array");
    assert!(findings.is_empty(), "scaffold findings: {findings:#?}");
}

#[test]
fn a11y_verify_passes_on_scaffolded_forms() {
    // A slug field is not supported with `--live-validation`.
    assert_scaffold_verifies(&["handle:slug{from:name}"]);
}

#[test]
fn a11y_verify_passes_on_live_validation_scaffolded_forms() {
    assert_scaffold_verifies(&["--live-validation"]);
}
