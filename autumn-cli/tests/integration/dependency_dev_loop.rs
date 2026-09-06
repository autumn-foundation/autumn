//! Issue #1633 — dependency advisories and policy in the dev loop.
//!
//! The dev-loop surfaces themselves (`autumn doctor`'s grading, `autumn dev`'s
//! startup lines, the CVSS banding) are unit tested beside the code they grade.
//! What can only be pinned from out here is the contract between the three
//! artifacts a generated app ships:
//!
//! * the **policy file** (`deny.toml`) — one file for advisories, licenses,
//!   bans and sources, with the optional sections commented out;
//! * the **workflow** (`.github/workflows/ci.yml`) — which must widen its
//!   check list exactly when the policy widens, or a lockfile that passes
//!   locally could still fail CI;
//! * the **docs** — the policy, the severity defaults, the offline behavior,
//!   and how a local finding maps to the CI failure it predicts.
//!
//! A parity claim nobody tested is a parity claim that quietly stops holding.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("autumn-cli should live under the workspace root")
        .to_path_buf()
}

/// Read a repository file with line endings normalized to LF.
///
/// `.gitattributes` declares `* text=auto`, so a Windows checkout materializes
/// these files with CRLF and every line-structure assertion below would
/// silently stop matching.
fn read_repo_file(rel: &str) -> String {
    let path = workspace_root().join(rel);
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    content.replace("\r\n", "\n")
}

/// Strip `#` comment lines so a content assertion cannot be satisfied by prose.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn scaffold(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(autumn_bin())
        .args(["new", name])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run autumn new");
    assert!(
        output.status.success(),
        "autumn new {name} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let project = tmp.path().join(name);
    (tmp, project)
}

fn read_project_file(project: &Path, rel: &str) -> String {
    let path = project.join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("generated project is missing {rel}: {e}"))
        .replace("\r\n", "\n")
}

/// The optional policy sections the generated workflow derives its check list
/// from, read out of the workflow itself.
fn workflow_optional_sections(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .find_map(|line| {
            let list = line.trim().strip_prefix("for section in ")?;
            let list = list.strip_suffix("; do")?;
            Some(
                list.split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
        })
        .expect("the generated workflow must derive its check list from the policy file")
}

// ── The policy file ──────────────────────────────────────────────────────────

#[test]
fn the_scaffolded_policy_ships_every_section_the_gate_can_enforce() {
    let (_tmp, project) = scaffold("policyapp");
    let policy = read_project_file(&project, "deny.toml");
    for section in ["licenses", "bans", "sources"] {
        assert!(
            policy.contains(&format!("# [{section}]")),
            "deny.toml must ship a commented `[{section}]` default so a team can \
             adopt it without inventing the file: \n{policy}"
        );
    }
}

#[test]
fn the_scaffolded_policy_leaves_the_optional_sections_off() {
    // "Safe commented defaults": a fresh app enforces advisories only, so its
    // first push is not red over a transitive crate's license.
    let (_tmp, project) = scaffold("quietapp");
    let policy = read_project_file(&project, "deny.toml");
    for section in ["licenses", "bans", "sources"] {
        assert!(
            !code_only(&policy).contains(&format!("[{section}]")),
            "`[{section}]` must ship commented out, not active"
        );
    }
    assert!(
        code_only(&policy).contains("[advisories]"),
        "the advisory gate itself is never optional"
    );
}

#[test]
fn the_scaffolded_policy_defaults_are_quiet_when_uncommented() {
    // cargo-deny's own defaults are noisy: `multiple-versions` warns on every
    // duplicate (67 of them in this workspace) and a workspace crate with no
    // license field is graded unlicensed. A commented default that floods the
    // dev loop the moment it is enabled is not a safe default.
    let (_tmp, project) = scaffold("defaultsapp");
    let policy = read_project_file(&project, "deny.toml");
    assert!(
        policy.contains("multiple-versions = \"allow\""),
        "the commented [bans] default must not warn on every duplicate crate"
    );
    assert!(
        policy.contains("private = { ignore = true }"),
        "the commented [licenses] default must not grade the app's own crates"
    );
}

#[test]
fn the_scaffolded_policy_says_the_dev_loop_reads_it() {
    let (_tmp, project) = scaffold("readmeapp");
    let policy = read_project_file(&project, "deny.toml");
    assert!(
        policy.contains("autumn doctor"),
        "the policy file must say that `autumn doctor` evaluates it"
    );
    assert!(
        policy.contains("autumn dev"),
        "the policy file must say what `autumn dev` does with it"
    );
}

// ── The workflow ─────────────────────────────────────────────────────────────

#[test]
fn the_scaffolded_workflow_derives_its_check_list_from_the_policy() {
    let (_tmp, project) = scaffold("ciapp");
    let workflow = read_project_file(&project, ".github/workflows/ci.yml");
    let code = code_only(&workflow);
    assert!(
        code.contains("cargo deny --offline check $checks"),
        "the audit step must run the derived check list, not a fixed one:\n{code}"
    );
    assert!(
        code.contains("checks=\"advisories\""),
        "advisories are never optional"
    );
}

#[test]
fn the_workflow_and_the_policy_agree_on_the_optional_sections() {
    // The parity pin. `autumn doctor` widens its check list when a policy
    // section is uncommented; if the workflow's list drifts from that one, a
    // lockfile could pass locally and fail CI for a dependency reason.
    let (_tmp, project) = scaffold("parityapp");
    let workflow = read_project_file(&project, ".github/workflows/ci.yml");
    let mut from_workflow = workflow_optional_sections(&workflow);
    from_workflow.sort();
    // The same names `crate::deps::OPTIONAL_CHECKS` holds. Spelled out rather
    // than imported: an integration test links the binary, not the module, and
    // a constant that can only be checked against itself pins nothing.
    assert_eq!(
        from_workflow,
        vec![
            "bans".to_owned(),
            "licenses".to_owned(),
            "sources".to_owned()
        ],
        "the workflow must derive exactly the sections `autumn doctor` does"
    );
}

#[test]
fn the_workflow_still_fails_closed_without_a_policy_file() {
    // #1600's invariant, re-asserted: deriving the check list must not have
    // turned a missing policy into a silently narrower audit.
    let (_tmp, project) = scaffold("failclosedapp");
    let workflow = read_project_file(&project, ".github/workflows/ci.yml");
    assert!(
        code_only(&workflow).contains("if [ ! -f deny.toml ]"),
        "a missing policy file must still fail the job"
    );
}

// ── Doctor ───────────────────────────────────────────────────────────────────

#[test]
fn doctor_reports_a_dependency_check_on_a_scaffolded_app() {
    let (_tmp, project) = scaffold("doctorapp");
    let output = Command::new(autumn_bin())
        .args(["doctor", "--json"])
        .current_dir(&project)
        .output()
        .expect("failed to run autumn doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("doctor --json: {e}\n{stdout}"));
    let check = parsed["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|check| check["name"] == "dependencies")
        .unwrap_or_else(|| panic!("doctor must report a `dependencies` check\n{stdout}"));
    assert!(
        check["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "the dependency check must say what it found: {check}"
    );
    // Whatever the local toolbox holds, the check never silently passes over an
    // unevaluated policy: a missing auditor or database is a warning.
    assert_ne!(check["status"], "error", "unexpected status: {check}");
}

// ── Docs ─────────────────────────────────────────────────────────────────────

#[test]
fn the_docs_explain_the_dev_loop_contract() {
    let docs = read_repo_file("docs/guide/supply-chain.md");
    for needle in [
        // The policy file and the sections it can carry.
        "deny.toml",
        "[licenses]",
        "[bans]",
        "[sources]",
        // The dev-loop surfaces.
        "autumn doctor",
        "autumn dev",
        // The severity defaults.
        "critical",
        // Offline behavior.
        "cargo deny fetch db",
    ] {
        assert!(
            docs.contains(needle),
            "docs/guide/supply-chain.md must explain {needle:?}"
        );
    }
}

#[test]
fn the_docs_map_a_local_finding_onto_the_ci_failure_it_predicts() {
    let docs = read_repo_file("docs/guide/supply-chain.md");
    assert!(
        docs.contains("## The dev loop"),
        "docs/guide/supply-chain.md needs a dev-loop section"
    );
    assert!(
        docs.contains("STALE") || docs.contains("stale"),
        "the docs must define the stale-data behavior"
    );
    assert!(
        docs.contains("waiv"),
        "the docs must say waivers are shared with the CI gate"
    );
}
