//! Integration tests for `autumn routes posture` (issue #1624) — the
//! falsifiability suite the acceptance criteria ask for, driven end to end
//! through the real `autumn` binary over committed manifest fixtures.
//!
//! The fixtures under `tests/fixtures/posture/` are four manifests of the same
//! toy app, all in the schema `autumn routes audit` emits:
//!
//! - `base.json`     — the accepted posture: `/admin/users` is role-gated.
//! - `widened.json`  — the seeded PR: that route is now `public`.
//! - `cosmetic.json` — the same posture, handler renamed and moved to another
//!   file: a refactor, and nothing more.
//! - `narrowed.json` — the public landing page becomes role-gated.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("posture")
        .join(name)
}

fn run(args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("routes")
        .arg("posture")
        .args(args)
        .output()
        .expect("failed to run autumn routes posture")
}

fn diff(base: &str, head: &str, extra: &[&str]) -> Output {
    let base = fixture(base);
    let head = fixture(head);
    let mut args = vec![
        "diff",
        "--base",
        base.to_str().unwrap(),
        "--head",
        head.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    run(&args)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn json_report(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| {
        panic!(
            "expected a JSON report, got:\n{}\nstderr:\n{}\nerror: {e}",
            stdout(out),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// The acknowledgment marker the report tells the reviewer to paste.
fn ack_marker(out: &Output) -> String {
    json_report(out)["ack_phrase"]
        .as_str()
        .expect("the JSON report carries the exact phrase")
        .to_owned()
}

// ── AC-7: the seeded pull request ───────────────────────────────────────────

/// A PR that flips a role-gated route to public is blocked, and the report
/// names that route.
#[test]
fn a_widening_pull_request_is_blocked_and_the_diff_names_the_route() {
    let out = diff("base.json", "widened.json", &["--format", "json"]);
    assert_eq!(out.status.code(), Some(1), "must block: {:?}", out.status);

    let report = json_report(&out);
    assert_eq!(report["blocked"], true);
    assert_eq!(report["counts"]["widening"], 1);
    let finding = &report["findings"][0];
    assert_eq!(finding["path"], "/admin/users");
    assert_eq!(finding["method"], "GET");
    assert_eq!(finding["severity"], "widening");
    assert!(
        finding["before"].as_str().unwrap().contains("admin"),
        "the report must state what the gate used to be: {finding}"
    );
    assert_eq!(finding["after"], "public");
}

/// The markdown a workflow posts on the PR names the route and hands the
/// reviewer the exact line to paste back.
#[test]
fn the_markdown_report_names_the_route_and_the_acknowledgment_line() {
    let out = diff("base.json", "widened.json", &["--format", "markdown"]);
    let md = stdout(&out);
    assert!(md.contains("/admin/users"), "{md}");
    assert!(md.contains("/ack-posture "), "{md}");
    assert!(
        md.contains("<!-- autumn-posture-gate -->"),
        "the update marker keeps the workflow from spamming: {md}"
    );
}

/// Adding the acknowledgment marker unblocks the same PR.
#[test]
fn the_acknowledgment_marker_unblocks_the_widening() {
    let blocked = diff("base.json", "widened.json", &["--format", "json"]);
    let marker = ack_marker(&blocked);

    let dir = tempfile::tempdir().unwrap();
    let comments = dir.path().join("comments.txt");
    std::fs::write(
        &comments,
        format!("Looks right to me.\n{marker} deliberate, launch week\n"),
    )
    .unwrap();

    let out = diff(
        "base.json",
        "widened.json",
        &["--format", "json", "--ack-file", comments.to_str().unwrap()],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "an acknowledged widening must pass: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json_report(&out);
    assert_eq!(report["blocked"], false);
    assert_eq!(
        report["acknowledgment_reason"], "deliberate, launch week",
        "the reason is carried through to the report"
    );
}

/// A cosmetic refactor of the same handler produces no posture finding at all.
#[test]
fn a_cosmetic_refactor_produces_no_finding_and_no_output() {
    let out = diff("base.json", "cosmetic.json", &["--format", "markdown"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout(&out).trim(),
        "",
        "a refactor must post nothing at all on the PR"
    );
}

/// A surface-*narrowing* PR is reported but never blocked.
#[test]
fn a_narrowing_pull_request_is_annotated_but_not_blocked() {
    let out = diff("base.json", "narrowed.json", &["--format", "json"]);
    assert_eq!(out.status.code(), Some(0));
    let report = json_report(&out);
    assert_eq!(report["blocked"], false);
    assert_eq!(report["counts"]["widening"], 0);
    assert!(report["counts"]["narrowing"].as_u64().unwrap() >= 1);
}

/// Acknowledging one widening does not pre-acknowledge the next one.
#[test]
fn an_acknowledgment_does_not_survive_a_further_widening() {
    let first = diff("base.json", "widened.json", &["--format", "json"]);
    let marker = ack_marker(&first);

    let dir = tempfile::tempdir().unwrap();
    let comments = dir.path().join("comments.txt");
    std::fs::write(&comments, format!("{marker}\n")).unwrap();

    // A later commit widens something else as well.
    let mut wider: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("widened.json")).unwrap()).unwrap();
    wider["dimensions"]["routes"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "path": "/internal/metrics",
            "method": "GET",
            "name": "metrics",
            "classification": "public",
            "roles": [],
            "scopes": [],
            "policy": false,
            "source": "user",
            "provenance": "provable"
        }));
    let wider_path = dir.path().join("wider.json");
    std::fs::write(&wider_path, serde_json::to_string_pretty(&wider).unwrap()).unwrap();

    let out = Command::new(autumn_bin())
        .args([
            "routes",
            "posture",
            "diff",
            "--base",
            fixture("base.json").to_str().unwrap(),
            "--head",
            wider_path.to_str().unwrap(),
            "--format",
            "json",
            "--ack-file",
            comments.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "re-widening after an acknowledgment must block again"
    );
    assert_eq!(json_report(&out)["counts"]["widening"], 2);
}

// ── bootstrap, errors, and the escape hatch ─────────────────────────────────

/// Turning the gate on in a repository with no committed baseline blocks
/// nothing.
#[test]
fn a_missing_baseline_bootstraps_instead_of_failing() {
    let out = Command::new(autumn_bin())
        .args([
            "routes",
            "posture",
            "diff",
            "--base",
            "/nonexistent/base.json",
            "--head",
            fixture("widened.json").to_str().unwrap(),
            "--allow-missing-base",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let report = json_report(&out);
    assert_eq!(report["bootstrap"], true);
    assert_eq!(report["blocked"], false);
}

/// Without the flag, a missing baseline is a usage error (exit 2) — distinct
/// from "the gate blocked" (exit 1), so CI can tell the two apart.
#[test]
fn a_missing_baseline_without_the_flag_is_a_usage_error() {
    let out = Command::new(autumn_bin())
        .args([
            "routes",
            "posture",
            "diff",
            "--base",
            "/nonexistent/base.json",
            "--head",
            fixture("widened.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// An inline `--ack` is the same escape hatch as the comment marker, for local
/// runs and for workflows that already know the digest.
#[test]
fn an_inline_ack_flag_unblocks_the_same_widening() {
    let blocked = diff("base.json", "widened.json", &["--format", "json"]);
    let digest = json_report(&blocked)["ack_digest"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = diff(
        "base.json",
        "widened.json",
        &["--format", "json", "--ack", &digest],
    );
    assert_eq!(out.status.code(), Some(0));
}

// ── digest + verify ─────────────────────────────────────────────────────────

/// The posture digest is stable under cosmetic change and moves on a real one.
#[test]
fn the_posture_digest_ignores_refactors_and_notices_widening() {
    let digest_of = |name: &str| {
        let out = run(&["digest", "--manifest", fixture(name).to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(0), "digest must succeed");
        stdout(&out).trim().to_owned()
    };
    assert_eq!(digest_of("base.json"), digest_of("cosmetic.json"));
    assert_ne!(digest_of("base.json"), digest_of("widened.json"));
    assert_eq!(digest_of("base.json").len(), 64);
}

/// Deploy-time verification: the genuine manifest verifies against the digest
/// CI recorded, and a tampered one does not.
#[test]
fn verify_accepts_the_recorded_digest_and_rejects_a_tampered_manifest() {
    let out = run(&[
        "digest",
        "--manifest",
        fixture("base.json").to_str().unwrap(),
    ]);
    let digest = stdout(&out).trim().to_owned();

    let genuine = run(&[
        "verify",
        "--manifest",
        fixture("base.json").to_str().unwrap(),
        "--expect-digest",
        &digest,
        "--skip-signature",
    ]);
    assert_eq!(
        genuine.status.code(),
        Some(0),
        "a genuine manifest must verify: {}",
        String::from_utf8_lossy(&genuine.stderr)
    );

    let tampered = run(&[
        "verify",
        "--manifest",
        fixture("widened.json").to_str().unwrap(),
        "--expect-digest",
        &digest,
        "--skip-signature",
    ]);
    assert_eq!(
        tampered.status.code(),
        Some(1),
        "a manifest whose posture is not the acknowledged one must not verify"
    );
}

/// Skipping the signature check is visible in the output — it is an escape
/// hatch, not a default.
#[test]
fn skipping_the_signature_check_says_so_out_loud() {
    let out = run(&[
        "verify",
        "--manifest",
        fixture("base.json").to_str().unwrap(),
        "--skip-signature",
    ]);
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    assert!(
        text.to_lowercase().contains("signature"),
        "the waived check must still be reported: {text}"
    );
}
