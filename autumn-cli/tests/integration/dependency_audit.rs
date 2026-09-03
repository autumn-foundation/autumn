//! Issue #1600 — known-vulnerable dependencies are blocked, by default, in the
//! CI `autumn new` scaffolds and on the framework's own release path.
//!
//! Three surfaces are covered here:
//!
//! * **Scaffold** — the generated `.github/workflows/ci.yml` runs a blocking
//!   dependency-advisory audit, and the generated project ships the advisory
//!   policy (`deny.toml`) that audit reads, waivers and all.
//! * **Framework release train** — the advisory gate that keeps an autumn-web
//!   release from shipping with an unwaived `RustSec` advisory, including the
//!   negative proof that the gate can actually go red.
//! * **Docs** — what the gate checks, how to read a failure, how to waive.
//!
//! Most of these assert on YAML and TOML that only ever executes in someone
//! else's CI. That is deliberate: a security gate that silently stops gating is
//! indistinguishable from a passing build, so the invariants have to be pinned
//! down here rather than discovered during an incident.

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
/// silently stop matching (see `supply_chain.rs` for the same guard).
fn read_repo_file(rel: &str) -> String {
    let path = workspace_root().join(rel);
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    content.replace("\r\n", "\n")
}

/// Strip `#` comment lines so a content assertion cannot be satisfied by prose.
///
/// The workflow YAML in this repo carries long explanatory comments that name
/// the very commands these tests look for; without this, deleting a step and
/// leaving its comment behind would keep the tests green.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `autumn new <name> [flags]` into a fresh tempdir, returning the tempdir and
/// the generated project directory.
fn scaffold(name: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut args = vec!["new", name];
    args.extend_from_slice(extra);
    let output = Command::new(autumn_bin())
        .args(&args)
        .current_dir(tmp.path())
        .output()
        .expect("failed to run autumn new");
    assert!(
        output.status.success(),
        "autumn {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let project = tmp.path().join(name);
    (tmp, project)
}

/// `autumn new <name>`, returning the tempdir, the project dir, and stdout.
fn scaffold_with_output(name: &str) -> (tempfile::TempDir, PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(autumn_bin())
        .args(["new", name])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run autumn new");
    assert!(output.status.success(), "autumn new {name} failed");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let project = tmp.path().join(name);
    (tmp, project, stdout)
}

fn read_project_file(project: &Path, rel: &str) -> String {
    let path = project.join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("generated project is missing {rel}: {e}"))
        .replace("\r\n", "\n")
}

/// Every RUSTSEC id mentioned in a cargo-deny config's `ignore` list.
fn waived_ids(config: &str) -> Vec<String> {
    config
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| {
            line.match_indices("RUSTSEC-")
                .map(|(at, _)| {
                    line[at..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// AC1 — the scaffolded CI audits dependencies by default, and blocks.
// ---------------------------------------------------------------------------

#[test]
fn scaffolded_ci_audits_dependencies_by_default() {
    let (_tmp, project) = scaffold("audit-app", &[]);
    let ci = code_only(&read_project_file(&project, ".github/workflows/ci.yml"));

    assert!(
        ci.contains("cargo deny") && ci.contains("check advisories"),
        "the generated CI must run a dependency-advisory audit as a real step:\n{ci}"
    );
    assert!(
        ci.contains("cargo-deny@"),
        "the auditor must be installed at a pinned version:\n{ci}"
    );
}

#[test]
fn scaffolded_audit_is_not_an_opt_in_comment() {
    let template = read_repo_file("autumn-cli/src/templates/.github/workflows/ci.yml.tmpl");
    assert!(
        !template.contains("cargo install cargo-audit"),
        "the opt-in `cargo install cargo-audit` note must be replaced by a real gate:\n{template}"
    );
    let optional_section = template
        .split("Optional extensions")
        .nth(1)
        .unwrap_or_default();
    assert!(
        !optional_section.to_lowercase().contains("audit:"),
        "dependency auditing must not be listed as an optional extension:\n{optional_section}"
    );
}

#[test]
fn scaffolded_audit_step_cannot_be_soft_failed() {
    let (_tmp, project) = scaffold("hard-fail-app", &[]);
    let ci = code_only(&read_project_file(&project, ".github/workflows/ci.yml"));

    assert!(
        !ci.contains("continue-on-error"),
        "no step in the generated CI may be advisory-only:\n{ci}"
    );
    for line in ci.lines().filter(|l| l.contains("cargo deny")) {
        assert!(
            !line.contains("|| true") && !line.contains("|| exit 0"),
            "the audit must fail the job, got: {line}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC2 + AC3 — day-one green, with an explicit, documented waiver mechanism.
// ---------------------------------------------------------------------------

#[test]
fn scaffolded_project_ships_an_advisory_policy() {
    let (_tmp, project) = scaffold("policy-app", &[]);
    let deny = read_project_file(&project, "deny.toml");

    assert!(
        deny.contains("[advisories]"),
        "the scaffolded deny.toml must configure the advisories check:\n{deny}"
    );
    assert!(
        deny.contains("ignore = ["),
        "the scaffolded deny.toml must show the waiver list, even when empty:\n{deny}"
    );
}

/// The one advisory a fresh scaffold cannot avoid: `jsonwebtoken` is a
/// non-optional autumn-web dependency, so `rsa` (RUSTSEC-2023-0071, no patched
/// release) is in every generated app's tree. AC2 allows exactly this — an
/// explicitly documented waiver — and nothing else.
#[test]
fn scaffold_waivers_are_documented_with_a_reason_and_a_review_date() {
    let (_tmp, project) = scaffold("waiver-app", &[]);
    let deny = read_project_file(&project, "deny.toml");

    assert!(
        deny.contains("RUSTSEC-2023-0071"),
        "the unavoidable rsa advisory must ship pre-waived so day-one CI is green:\n{deny}"
    );
    for id in waived_ids(&deny) {
        let entry = deny
            .lines()
            .find(|line| line.contains(&id) && line.contains("id ="))
            .unwrap_or_else(|| {
                panic!("waiver for {id} must be a structured ignore entry:\n{deny}")
            });
        assert!(
            entry.contains("reason ="),
            "waiver for {id} must record a rationale, got: {entry}"
        );
    }
    assert!(
        deny.to_lowercase().contains("review-by"),
        "waivers are debt: each must carry a review-by date:\n{deny}"
    );
}

/// A waiver the framework itself has not triaged has no business being
/// pre-installed in every generated app.
#[test]
fn scaffold_waivers_are_a_subset_of_the_frameworks_own() {
    let (_tmp, project) = scaffold("subset-app", &[]);
    let scaffold_ids = waived_ids(&read_project_file(&project, "deny.toml"));
    let framework_ids = waived_ids(&read_repo_file("deny.toml"));

    assert!(!scaffold_ids.is_empty(), "expected at least the rsa waiver");
    for id in scaffold_ids {
        assert!(
            framework_ids.contains(&id),
            "{id} is waived for every scaffolded app but not triaged in the workspace deny.toml"
        );
    }
}

#[test]
fn scaffold_policy_never_downgrades_a_vulnerability() {
    let (_tmp, project) = scaffold("posture-app", &[]);
    let deny = code_only(&read_project_file(&project, "deny.toml"));

    for downgrade in ["vulnerability = \"allow\"", "vulnerability = \"warn\""] {
        assert!(
            !deny.contains(downgrade),
            "the scaffolded policy must keep vulnerabilities blocking, got: {downgrade}"
        );
    }
    assert!(
        deny.contains("unmaintained =") && deny.contains("unsound ="),
        "the scaffolded policy must state its unmaintained/unsound scope explicitly:\n{deny}"
    );
}

#[test]
fn scaffolded_project_documents_the_waiver_workflow() {
    let (_tmp, project) = scaffold("docs-app", &[]);
    let deny = read_project_file(&project, "deny.toml");
    assert!(
        deny.contains("cargo deny check advisories"),
        "the policy file must name the command that reads it:\n{deny}"
    );
    let readme = read_project_file(&project, "README.md");
    assert!(
        readme.contains("deny.toml"),
        "the generated README's project layout must mention the advisory policy:\n{readme}"
    );
}

/// A developer who does not know `deny.toml` exists reaches for disabling the
/// CI step when the gate first fires, so `autumn new` names it and says what it
/// is for.
#[test]
fn autumn_new_announces_the_advisory_policy_it_wrote() {
    let (_tmp, _project, stdout) = scaffold_with_output("announce-app");
    let line = stdout
        .lines()
        .find(|line| line.contains("deny.toml"))
        .unwrap_or_else(|| panic!("`autumn new` must announce deny.toml:\n{stdout}"));
    assert!(
        line.to_lowercase().contains("advisor"),
        "the line must say what the file is for, got: {line}"
    );
}

// ---------------------------------------------------------------------------
// AC5 — defined behavior when the advisory database is unreachable.
// ---------------------------------------------------------------------------

#[test]
fn scaffolded_audit_retries_then_fails_closed_when_the_database_is_unreachable() {
    let (_tmp, project) = scaffold("offline-app", &[]);
    let raw = read_project_file(&project, ".github/workflows/ci.yml");
    let ci = code_only(&raw);

    assert!(
        ci.contains("cargo deny fetch db"),
        "the advisory database fetch must be its own, retryable step:\n{ci}"
    );
    assert!(
        ci.contains("for attempt in"),
        "the fetch must retry rather than fail on the first network hiccup:\n{ci}"
    );
    assert!(
        ci.contains("sleep"),
        "retries must back off between attempts:\n{ci}"
    );
    assert!(
        ci.contains("exit 1"),
        "an unreachable advisory database must fail the job, not skip the audit:\n{ci}"
    );
    assert!(
        ci.contains("--offline check advisories"),
        "the check itself must run against the fetched database, so a failure there \
         is a real advisory and not a network blip:\n{ci}"
    );
    assert!(
        raw.to_lowercase().contains("fail closed"),
        "the workflow must document the unreachable-database behavior:\n{raw}"
    );
}

// ---------------------------------------------------------------------------
// The audit reaches every app flavor, and the auditor pin does not drift.
// ---------------------------------------------------------------------------

#[test]
fn the_api_flavor_ships_the_same_gate() {
    let (_tmp, project) = scaffold("api-audit-app", &["--api"]);
    let ci = code_only(&read_project_file(&project, ".github/workflows/ci.yml"));
    assert!(
        ci.contains("check advisories"),
        "the --api scaffold must audit its dependencies too:\n{ci}"
    );
    let deny = read_project_file(&project, "deny.toml");
    assert!(deny.contains("[advisories]"), "--api needs the policy too");
}

/// The version pin in the scaffold and the one the framework audits itself with
/// must agree: a scaffold that lags is a scaffold whose users hit bugs this
/// repo already fixed, and a divergence is invisible until it bites.
#[test]
fn the_scaffold_audits_with_the_same_pinned_auditor_as_the_framework() {
    fn pin(yaml: &str) -> String {
        let at = yaml
            .find("cargo-deny@")
            .unwrap_or_else(|| panic!("no pinned cargo-deny in:\n{yaml}"));
        yaml[at..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .collect()
    }
    let scaffold_pin = pin(&code_only(&read_repo_file(
        "autumn-cli/src/templates/.github/workflows/ci.yml.tmpl",
    )));
    let framework_pin = pin(&code_only(&read_repo_file(".github/workflows/ci.yml")));
    assert_eq!(
        scaffold_pin, framework_pin,
        "the scaffolded audit pin and the framework's own must agree"
    );
}

// ---------------------------------------------------------------------------
// AC4 — the framework's own release path blocks an unwaived advisory.
// ---------------------------------------------------------------------------

#[test]
fn advisory_gate_script_exists_and_is_executable() {
    let path = workspace_root().join("scripts/check-advisories.sh");
    assert!(path.is_file(), "scripts/check-advisories.sh must exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "check-advisories.sh must be executable");
    }
}

#[test]
fn the_publish_gate_runs_the_advisory_gate() {
    let gate = code_only(&read_repo_file(".github/workflows/publish-gate.yml"));
    assert!(
        gate.contains("scripts/check-advisories.sh"),
        "a release must not be publishable with an unwaived advisory in its tree:\n{gate}"
    );
}

#[test]
fn pull_request_ci_runs_the_same_advisory_gate() {
    let ci = code_only(&read_repo_file(".github/workflows/ci.yml"));
    assert!(
        ci.contains("scripts/check-advisories.sh"),
        "PR CI and the publish gate must share one advisory gate:\n{ci}"
    );
}

/// The gate is only worth anything if it can go red. CI proves that on every
/// run by auditing a project with a deliberately injected known-vulnerable
/// dependency and requiring the gate to reject it.
#[test]
fn ci_proves_the_gate_can_still_reject_a_vulnerable_dependency() {
    let ci = code_only(&read_repo_file(".github/workflows/ci.yml"));
    assert!(
        ci.contains("check-advisories.sh --self-test"),
        "CI must run the injected-vulnerability self-test:\n{ci}"
    );
}

/// The same negative proof, run here when cargo-deny is installed locally.
///
/// Skipped (not failed) without cargo-deny: this suite compiles into a
/// consolidated binary that runs on hosts with no supply-chain tooling. CI's
/// `supply-chain` job installs cargo-deny and runs the very same `--self-test`,
/// which `ci_proves_the_gate_can_still_reject_a_vulnerable_dependency` pins.
#[cfg(unix)]
#[test]
fn the_advisory_gate_rejects_an_injected_known_vulnerable_dependency() {
    let has_cargo_deny = Command::new("cargo")
        .args(["deny", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !has_cargo_deny {
        eprintln!("skipping: cargo-deny is not installed on this host");
        return;
    }

    let out = Command::new(workspace_root().join("scripts/check-advisories.sh"))
        .arg("--self-test")
        .current_dir(workspace_root())
        .output()
        .expect("failed to run check-advisories.sh --self-test");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "the advisory gate self-test failed:\n{combined}"
    );
    assert!(
        combined.contains("RUSTSEC-2020-0071"),
        "the self-test must name the advisory it injected:\n{combined}"
    );
}

/// AC2, enforced rather than asserted once: the scaffold's own policy must be
/// clean against the autumn-web graph a generated app pins, so "day-one CI is
/// green" cannot quietly stop being true between releases.
#[test]
fn the_gate_checks_the_scaffolds_day_one_dependency_tree() {
    let script = read_repo_file("scripts/check-advisories.sh");
    assert!(
        script.contains("autumn-cli/src/templates/deny.toml.tmpl"),
        "the gate must audit the scaffold's shipped policy against autumn-web's graph:\n{script}"
    );
}

// ---------------------------------------------------------------------------
// AC6 — docs.
// ---------------------------------------------------------------------------

#[test]
fn the_guide_explains_the_audit_gate() {
    let guide = read_repo_file("docs/guide/supply-chain.md");
    let lower = guide.to_lowercase();
    assert!(
        lower.contains("advisory gate") || lower.contains("advisory audit"),
        "the supply-chain guide must cover the advisory gate"
    );
    assert!(
        guide.contains("cargo deny check advisories"),
        "the guide must show the command CI runs"
    );
    assert!(
        guide.contains("deny.toml") && lower.contains("waive"),
        "the guide must explain how to waive an advisory"
    );
    assert!(
        lower.contains("fail closed") || lower.contains("unreachable"),
        "the guide must document what happens when the advisory database is unreachable"
    );
}

#[test]
fn the_guide_no_longer_defers_vulnerability_scanning() {
    let guide = read_repo_file("docs/guide/supply-chain.md");
    assert!(
        !guide.contains("is a separate question"),
        "the guide's 'known-vulnerable is a separate question' caveat is now false"
    );
}

#[test]
fn the_release_checklist_documents_the_advisory_gate() {
    let checklist = read_repo_file("docs/release-checklist.md");
    assert!(
        checklist.contains("check-advisories.sh"),
        "the release checklist must name the advisory gate script"
    );
}
