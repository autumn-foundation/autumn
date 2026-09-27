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
//! These artifacts only ever execute in someone else's CI, so their invariants
//! have to be pinned here rather than discovered during an incident.

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
    // A fresh scaffold's policy is clean, and an unevaluated policy says so in
    // its detail rather than reporting a finding. Either way it never fails.
    assert_ne!(check["status"], "fail", "unexpected status: {check}");
}

/// The check list `autumn doctor` derives for the policy in `project`.
///
/// Read from the `checks: …` fragment doctor prints in every state, so this
/// works whether or not cargo-deny is installed on the machine running it.
fn doctor_checks(project: &Path) -> Vec<String> {
    let output = Command::new(autumn_bin())
        .args(["doctor", "--json"])
        .current_dir(project)
        .output()
        .expect("failed to run autumn doctor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("doctor --json: {e}\n{stdout}"));
    let detail = parsed["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|check| check["name"] == "dependencies")
        .and_then(|check| check["detail"].as_str())
        .unwrap_or_else(|| panic!("no dependencies detail\n{stdout}"))
        .to_owned();
    let listed = detail
        .split("checks: ")
        .nth(1)
        .unwrap_or_else(|| panic!("doctor must name the checks it derived: {detail}"));
    listed
        .split([';', '\n'])
        .next()
        .unwrap_or_default()
        .split(", ")
        .map(|check| check.trim().to_owned())
        .filter(|check| !check.is_empty())
        .collect()
}

/// The check list the generated workflow derives, by running its own shell.
#[cfg(unix)]
fn workflow_checks(workflow: &str, project: &Path) -> Vec<String> {
    let mut script = String::new();
    for line in workflow
        .lines()
        .skip_while(|line| !line.trim().starts_with("checks=\"advisories\""))
        .take_while(|line| !line.trim().starts_with("cargo deny"))
        // The step echoes the derived list for its own log; the value is what
        // this test reads.
        .filter(|line| !line.trim().starts_with("echo "))
    {
        script.push_str(line.trim_start());
        script.push('\n');
    }
    assert!(
        script.contains("for section in"),
        "could not extract the derivation from the workflow:\n{workflow}"
    );
    let output = Command::new("bash")
        .arg("-c")
        .arg(format!("set -e\n{script}\nprintf '%s' \"$checks\""))
        .current_dir(project)
        .output()
        .expect("failed to run the workflow derivation");
    assert!(
        output.status.success(),
        "the workflow derivation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Every TOML spelling that declares a cargo-deny section, and the policies
/// that only look like they do.
const SECTION_SPELLINGS: &[(&str, bool)] = &[
    ("[bans]\n", true),
    ("[ bans ]\n", true),
    ("[bans] # inline note\n", true),
    ("[bans.build]\n", true),
    ("[[bans.deny]]\n", true),
    ("bans.deny = []\n", true),
    ("bans = { multiple-versions = \"allow\" }\n", true),
    ("  [bans]\n", true),
    ("[\"bans\"]\n", true),
    ("['bans']\n", true),
    ("[bansible]\n", false),
    ("# [bans]\n", false),
    ("#[bans]\n", false),
    ("[advisories]\n", false),
];

/// `autumn doctor` and the generated workflow must derive the same check list
/// from the same policy file — the whole basis of AC4's parity claim.
///
/// This runs the workflow's own shell, so it cannot pass by agreeing with a
/// re-implementation of the rule.
#[cfg(unix)]
#[test]
fn doctor_and_the_generated_workflow_derive_the_same_checks() {
    let (_tmp, project) = scaffold("deriveapp");
    let workflow = read_project_file(&project, ".github/workflows/ci.yml");
    let policy_path = project.join("deny.toml");

    for (policy, declares_bans) in SECTION_SPELLINGS {
        fs::write(&policy_path, policy).expect("write policy");
        let expected = if *declares_bans {
            vec!["advisories".to_owned(), "bans".to_owned()]
        } else {
            vec!["advisories".to_owned()]
        };
        let from_workflow = workflow_checks(&workflow, &project);
        let from_doctor = doctor_checks(&project);
        assert_eq!(
            from_workflow, expected,
            "the workflow derived the wrong checks for {policy:?}"
        );
        assert_eq!(
            from_doctor, expected,
            "doctor derived the wrong checks for {policy:?}"
        );
    }
}

/// The scaffolded policy itself must derive advisories only — a fresh app is
/// not silently opted into a license or ban policy it never wrote.
#[cfg(unix)]
#[test]
fn the_shipped_policy_derives_advisories_only_on_both_sides() {
    let (_tmp, project) = scaffold("shippedapp");
    let workflow = read_project_file(&project, ".github/workflows/ci.yml");
    assert_eq!(workflow_checks(&workflow, &project), vec!["advisories"]);
    assert_eq!(doctor_checks(&project), vec!["advisories"]);
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
        docs.contains("## Part 3b — the dev loop"),
        "docs/guide/supply-chain.md needs a dev-loop section, in the file's own \
         `Part N` scheme"
    );
    for (claim, needle) in [
        ("the stale-data behaviour", "stale"),
        (
            "that the waiver store is shared with the CI gate",
            "`[advisories] ignore` entry",
        ),
        ("the auditor-version difference", "cargo-deny@0.20.2"),
        (
            "that CI fetches the database and doctor does not",
            "fetches the RustSec database",
        ),
        ("how a waived finding is graded", "its own CVSS band"),
        (
            "that an unevaluated policy passes rather than warns",
            "not evaluated",
        ),
    ] {
        assert!(
            docs.contains(needle),
            "docs/guide/supply-chain.md must explain {claim} (looked for {needle:?})"
        );
    }
}

/// The docs must not promise a warning where the check passes.
///
/// Regression: the offline section still said a never-fetched database "warns
/// once" after the implementation was changed to pass with a `not evaluated`
/// detail. Documentation that oversells a gate is worse than none — a reader
/// relies on `--strict` to catch an unevaluated tree, and it does not.
#[test]
fn the_docs_never_promise_a_warning_for_a_state_that_passes() {
    let docs = read_repo_file("docs/guide/supply-chain.md");
    for state in ["Database never fetched", "cargo-deny not installed"] {
        let bullet = docs
            .split("- **")
            .find(|b| b.starts_with(state))
            .unwrap_or_else(|| panic!("no bullet for {state:?}"));
        let bullet = bullet.split("\n- **").next().unwrap_or(bullet);
        // Both states are graded `pass` by `check_dependencies_impl`.
        assert!(
            bullet.contains("pass"),
            "the {state:?} bullet must say it passes: {bullet}"
        );
        assert!(
            !bullet.contains("warns once"),
            "the {state:?} bullet still promises a warning: {bullet}"
        );
    }
}

/// Every fenced block in the guide must close on a line of its own.
///
/// Regression: an edit left prose on the closing-fence line
/// (```` ``` The line follows… ````). Under `CommonMark` that is not a closing
/// fence, so the block stayed open and swallowed the dependency-check,
/// severity, dev-loop and offline sections into one code block. The link gate
/// does not see this — it checks links, not fences — and neither does anything
/// else in the suite, which is why the page rendered wrong with every test
/// green.
#[test]
fn the_guides_code_fences_close_on_their_own_line() {
    for guide in ["docs/guide/supply-chain.md"] {
        let text = read_repo_file(guide);
        let mut open: Option<usize> = None;
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if !trimmed.starts_with("```") {
                continue;
            }
            match open {
                // An opening fence may carry an info string (```text).
                None => open = Some(number + 1),
                // A closing fence may carry nothing at all.
                Some(start) => {
                    assert!(
                        trimmed.trim_end_matches('`').trim().is_empty(),
                        "{guide}:{} closes the block opened at line {start} but carries \
                         trailing text, so the fence does not close: {line}",
                        number + 1
                    );
                    open = None;
                }
            }
        }
        assert!(
            open.is_none(),
            "{guide} leaves a code fence open at line {}",
            open.unwrap_or_default()
        );
    }
}
