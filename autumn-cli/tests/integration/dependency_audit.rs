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

/// Every RUSTSEC id waived by a cargo-deny config.
///
/// Only structured `{ id = "…" }` entries count: an id cited inside another
/// waiver's `reason` ("superseded by RUSTSEC-…") is prose, and counting it
/// would invent a waiver that does not exist.
fn waived_ids(config: &str) -> Vec<String> {
    config
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter(|line| line.contains("id ="))
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

/// The step in the generated workflow that runs the audit.
///
/// Since #1633 that step derives its check list from `deny.toml` — advisories
/// always, plus any optional section the policy declares — so the assertions
/// below look for the unconditional seed of that list rather than a fixed
/// command. The gate itself is unchanged: advisories can never be dropped.
const AUDIT_STEP: &str = "Audit dependencies (RustSec advisories and declared policy)";

/// The seed of the derived check list. Advisories are not optional.
const UNCONDITIONAL_ADVISORIES: &str = "checks=\"advisories\"";

/// The body of one workflow step: from its `- name: <step>` to the next step.
///
/// Whole-file `contains` checks on a workflow are how a retry loop gets deleted
/// from the step that needs it while some other step's `sleep` keeps the
/// assertion green.
fn workflow_step<'a>(yaml: &'a str, step: &str) -> &'a str {
    let start = yaml
        .find(&format!("- name: {step}"))
        .unwrap_or_else(|| panic!("no step named {step:?} in:\n{yaml}"));
    let rest = &yaml[start + 1..];
    let end = rest
        .find("- name: ")
        .map_or(yaml.len(), |at| start + 1 + at);
    &yaml[start..end]
}

// ---------------------------------------------------------------------------
// AC1 — the scaffolded CI audits dependencies by default, and blocks.
// ---------------------------------------------------------------------------

#[test]
fn scaffolded_ci_audits_dependencies_by_default() {
    let (_tmp, project) = scaffold("audit-app", &[]);
    let ci = code_only(&read_project_file(&project, ".github/workflows/ci.yml"));

    // Scoped to the audit step: a whole-file `contains` is satisfied by the
    // seed appearing anywhere, including in a step that no longer audits.
    let audit = workflow_step(&ci, AUDIT_STEP);
    assert!(
        audit.contains("cargo deny --offline check $checks")
            && audit.contains(UNCONDITIONAL_ADVISORIES),
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
    let header = template
        .split("\nname: CI")
        .next()
        .expect("the template must have a header comment before `name: CI`");
    let optional_section = header
        .split("Optional extensions")
        .nth(1)
        .expect("the header must still list its optional extensions");
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
    let entries = code_only(&deny);
    for id in waived_ids(&deny) {
        let entry = entries
            .lines()
            .find(|line| line.contains(&id) && line.contains("id ="))
            .unwrap_or_else(|| {
                panic!("waiver for {id} must be a structured ignore entry:\n{deny}")
            });
        assert!(
            entry.contains("reason ="),
            "waiver for {id} must record a rationale, got: {entry}"
        );
        // On the entry, not in the file's prose: a header that merely
        // *describes* review-by dates satisfies a whole-file match, and every
        // shipped waiver would pass while carrying none.
        assert!(
            entry.to_lowercase().contains("review-by"),
            "waivers are debt: {id} must carry a review-by date in its reason, got: {entry}"
        );
    }
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

/// cargo-deny has no knob that downgrades a vulnerability — the `ignore` list
/// is the only way past one — so the ways this policy could quietly stop
/// gating are narrowing a scope, or the workflow asking for a different check.
#[test]
fn the_scaffolded_policy_ships_its_widest_scopes() {
    let (_tmp, project) = scaffold("posture-app", &[]);
    let deny = code_only(&read_project_file(&project, "deny.toml"));

    for (key, expected) in [("unmaintained", "all"), ("unsound", "all")] {
        let line = deny
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{key} =")))
            .unwrap_or_else(|| panic!("the policy must state its {key} scope:\n{deny}"));
        assert!(
            line.contains(expected),
            "a generated app must ship the widest {key} scope — an author may narrow \
             it themselves, the framework must not narrow it for them; got: {line}"
        );
    }

    let ci = code_only(&read_project_file(&project, ".github/workflows/ci.yml"));
    assert!(
        workflow_step(&ci, AUDIT_STEP).contains(UNCONDITIONAL_ADVISORIES),
        "narrowing the policy is pointless if the workflow stops asking for the \
         advisories check:\n{ci}"
    );
}

/// The policy is a config file before it is documentation: if it does not
/// parse, every generated app fails its first CI run on a TOML error.
#[test]
fn the_scaffolded_policy_is_valid_toml() {
    for flags in [&[][..], &["--api"], &["--bundled-pg"]] {
        let name = format!("toml-{}-app", flags.join("-").replace("--", ""));
        let (_tmp, project) = scaffold(&name, flags);
        let raw = read_project_file(&project, "deny.toml");
        let parsed: toml::Value = toml::from_str(&raw)
            .unwrap_or_else(|e| panic!("generated deny.toml is not valid TOML: {e}\n{raw}"));
        let ignore = parsed
            .get("advisories")
            .and_then(|advisories| advisories.get("ignore"))
            .and_then(toml::Value::as_array)
            .expect("the policy must expose an [advisories] ignore list");
        for waiver in ignore {
            assert!(
                waiver.get("id").is_some() && waiver.get("reason").is_some(),
                "every shipped waiver must carry an id and a reason, got: {waiver}"
            );
        }
    }
}

/// A waiver for an advisory the app's tree cannot reach spends the one signal
/// cargo-deny has for "this waiver of yours has gone stale".
#[test]
fn the_scaffold_waives_only_what_its_own_flavor_can_hit() {
    let (_tmp, plain) = scaffold("no-extra-waiver-app", &[]);
    assert!(
        !read_project_file(&plain, "deny.toml").contains("RUSTSEC-2024-0384"),
        "the managed-pg-bundled waiver has no business in an app that never \
         enables that feature"
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

    let fetch = workflow_step(&ci, "Fetch the RustSec advisory database");
    assert!(
        fetch.contains("cargo deny fetch db"),
        "the advisory database fetch must be its own, retryable step:\n{fetch}"
    );
    assert!(
        fetch.contains("for attempt in"),
        "the fetch must retry rather than fail on the first network hiccup:\n{fetch}"
    );
    assert!(
        fetch.contains("sleep"),
        "retries must back off between attempts:\n{fetch}"
    );
    assert!(
        fetch.contains("exit 1"),
        "an unreachable advisory database must fail the job, not skip the audit:\n{fetch}"
    );
    // The last attempt has nothing left to wait for: sleeping there burns CI
    // minutes and logs a retry that never comes.
    assert!(
        fetch.contains("-lt 3"),
        "the backoff must not sleep after the final attempt:\n{fetch}"
    );
    // An app upgraded from a release before this gate existed has this workflow
    // and no policy file. cargo-deny would silently fall back to its built-in
    // default — no waivers — and fail on an advisory the scaffold already
    // triaged, so the gate says what is missing instead.
    assert!(
        fetch.contains("deny.toml"),
        "the gate must check its policy exists before auditing:\n{fetch}"
    );
    let audit = workflow_step(&ci, AUDIT_STEP);
    assert!(
        audit.contains("--offline check $checks") && audit.contains(UNCONDITIONAL_ADVISORIES),
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
        workflow_step(&ci, AUDIT_STEP).contains(UNCONDITIONAL_ADVISORIES),
        "the --api scaffold must audit its dependencies too:\n{ci}"
    );
    let deny = read_project_file(&project, "deny.toml");
    assert!(deny.contains("[advisories]"), "--api needs the policy too");
}

/// Pinning the auditor buys nothing if the job restores a previously cached
/// binary instead: `Swatinem/rust-cache` caches `$CARGO_HOME/bin` by default,
/// and `install-action` skips its verified download when the pinned version is
/// already on PATH — so from the second run onward the auditor would come from
/// a cache entry rather than the pin.
#[test]
fn the_auditor_comes_from_its_pin_and_never_from_a_cache() {
    let (_tmp, project) = scaffold("cache-app", &[]);
    for workflow in [
        code_only(&read_project_file(&project, ".github/workflows/ci.yml")),
        code_only(&read_repo_file(".github/workflows/ci.yml")),
        code_only(&read_repo_file(".github/workflows/publish-gate.yml")),
    ] {
        for (install, _) in workflow.match_indices("cargo-deny@") {
            // The cache step that could serve this install is the last one
            // before it; other jobs' caches are none of this test's business.
            let Some(cache) = workflow[..install].rfind("Swatinem/rust-cache") else {
                continue;
            };
            assert!(
                workflow[cache..install].contains("cache-bin: false"),
                "the cache step preceding a pinned cargo-deny install must not cache \
                 $CARGO_HOME/bin, or the auditor comes from the cache instead of the \
                 pin:\n{}",
                &workflow[cache..install]
            );
        }
    }
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
    for framework in [
        ".github/workflows/ci.yml",
        ".github/workflows/publish-gate.yml",
    ] {
        assert_eq!(
            scaffold_pin,
            pin(&code_only(&read_repo_file(framework))),
            "the scaffolded audit pin and {framework}'s must agree — an auditor that \
             drifts between the release path, PR CI and generated apps means three \
             different gates wearing one name"
        );
    }
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
        // One attempt: a host without network should reach the skip below in a
        // second, not after two rounds of backoff.
        .env("ADVISORY_DB_FETCH_RETRIES", "1")
        .output()
        .expect("failed to run check-advisories.sh --self-test");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // cargo-deny being installed does not mean the host can reach crates.io and
    // the RustSec database. Reporting an offline developer's machine as a
    // security regression trains people to ignore this suite, so the gate's own
    // fail-closed messages — the ones it prints when it could not fetch —
    // become a skip here. A real regression fails some other way, loudly.
    for offline in ["failing closed", "could not fetch the self-test fixture"] {
        if combined.contains(offline) {
            eprintln!("skipping: this host cannot reach the advisory database\n{combined}");
            return;
        }
    }
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

/// Every `autumn-web` feature any scaffold flavor can turn on has to be inside
/// the graph the release gate audits. Otherwise a flavor ships a waiver set
/// that does not cover its own dependency tree and its day-one CI is red —
/// which is exactly what `--bundled-pg` did, whose `managed-pg-bundled` feature
/// drags in `instant` (RUSTSEC-2024-0384).
#[test]
fn the_gate_audits_every_feature_a_scaffold_flavor_can_enable() {
    let audited = gate_audited_features();
    let defaults = autumn_web_default_features();

    for flags in scaffold_flavors() {
        let name = format!("features-{}-app", flavor_slug(flags));
        let (_tmp, project) = scaffold(&name, flags);
        for feature in scaffold_autumn_web_features(&read_project_file(&project, "Cargo.toml")) {
            assert!(
                audited.contains(&feature) || defaults.contains(&feature),
                "`autumn new {}` enables autumn-web/{feature}, which the release gate never \
                 audits — that flavor's shipped waivers are unverified. Add it to the \
                 --features list in scripts/check-advisories.sh.\naudited: {audited:?}",
                flags.join(" "),
            );
        }
    }
}

/// Every flavor `autumn new` can produce, so a gate that only holds for the
/// default scaffold cannot pass for the whole feature.
fn scaffold_flavors() -> Vec<&'static [&'static str]> {
    vec![
        &[],
        &["--api"],
        &["--daemon"],
        &["--bundled-pg"],
        &["--with-i18n", "--with-seed"],
        &["--api", "--with-i18n"],
    ]
}

fn flavor_slug(flags: &[&str]) -> String {
    if flags.is_empty() {
        "default".to_owned()
    } else {
        flags.join("-").replace("--", "")
    }
}

/// The `--features` list `scripts/check-advisories.sh` audits the scaffold with.
fn gate_audited_features() -> Vec<String> {
    let script = read_repo_file("scripts/check-advisories.sh");
    let line = script
        .lines()
        .find(|line| line.starts_with("SCAFFOLD_FEATURES="))
        .unwrap_or_else(|| panic!("the gate must audit an explicit feature set:\n{script}"));
    line.trim_start_matches("SCAFFOLD_FEATURES=")
        .trim_matches('"')
        .split(',')
        .map(|feature| feature.trim().to_owned())
        .filter(|feature| !feature.is_empty())
        .collect()
}

/// autumn-web's own default features — on unless a flavor opts out, and every
/// opt-out flavor names a subset of them.
fn autumn_web_default_features() -> Vec<String> {
    let manifest = read_repo_file("autumn/Cargo.toml");
    let line = manifest
        .lines()
        .find(|line| line.starts_with("default = ["))
        .expect("autumn/Cargo.toml must declare a default feature list");
    line.trim_start_matches("default = [")
        .trim_end_matches(']')
        .split(',')
        .map(|feature| feature.trim().trim_matches('"').to_owned())
        .filter(|feature| !feature.is_empty())
        .collect()
}

/// Every autumn-web feature a generated `Cargo.toml` can switch on: those named
/// on the dependency line, and those the app's own features forward.
fn scaffold_autumn_web_features(cargo_toml: &str) -> Vec<String> {
    let mut features: Vec<String> = cargo_toml
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| {
            line.match_indices("autumn-web/")
                .map(|(at, _)| {
                    line[at + "autumn-web/".len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        })
        .collect();

    if let Some(line) = cargo_toml
        .lines()
        .find(|line| line.starts_with("autumn-web = "))
        && let Some(at) = line.find("features = [")
    {
        let list = &line[at + "features = [".len()..];
        let list = &list[..list.find(']').unwrap_or(list.len())];
        features.extend(
            list.split(',')
                .map(|feature| feature.trim().trim_matches('"').to_owned())
                .filter(|feature| !feature.is_empty()),
        );
    }
    features.sort();
    features.dedup();
    features
}

/// The `--bundled-pg` flavor's tree carries an advisory the default flavor's
/// does not, so that flavor's shipped policy has to cover it.
#[test]
fn the_scaffold_policy_covers_the_bundled_postgres_flavor() {
    let (_tmp, project) = scaffold("bundled-pg-policy-app", &["--bundled-pg"]);
    let deny = read_project_file(&project, "deny.toml");
    assert!(
        deny.contains("RUSTSEC-2024-0384"),
        "`--bundled-pg` pulls `instant` (RUSTSEC-2024-0384) through \
         managed-pg-bundled; without a waiver that app's first CI run is red:\n{deny}"
    );
}

/// The gate is only worth something if it reaches every app `autumn new` can
/// produce — a flavor whose workflow renders without the audit step, or without
/// the policy it reads, is a flavor shipping an ungated app.
#[test]
fn every_flavor_ships_the_gate_and_its_policy() {
    for flags in scaffold_flavors() {
        let name = format!("gate-{}-app", flavor_slug(flags));
        let (_tmp, project) = scaffold(&name, flags);
        let ci = code_only(&read_project_file(&project, ".github/workflows/ci.yml"));
        let audit = workflow_step(&ci, AUDIT_STEP);
        assert!(
            audit.contains("--offline check $checks") && audit.contains(UNCONDITIONAL_ADVISORIES),
            "`autumn new {}` must still audit its dependencies:\n{ci}",
            flags.join(" ")
        );
        assert!(
            !ci.contains("continue-on-error"),
            "`autumn new {}` must not soft-fail any step:\n{ci}",
            flags.join(" ")
        );
        let deny = read_project_file(&project, "deny.toml");
        assert!(
            deny.contains("[advisories]") && deny.contains("RUSTSEC-2023-0071"),
            "`autumn new {}` must ship the policy its CI reads:\n{deny}",
            flags.join(" ")
        );
    }
}

// ---------------------------------------------------------------------------
// The framework's own gate: what the script does, not just that it is called.
// ---------------------------------------------------------------------------

/// AC5 for the framework's gate, executed rather than grepped: with an
/// advisory database that cannot be fetched, the gate must fail — not skip the
/// audit, not report a clean tree.
#[cfg(unix)]
#[test]
fn the_gate_fails_closed_when_the_advisory_database_cannot_be_fetched() {
    use std::os::unix::fs::PermissionsExt as _;

    let stub_dir = tempfile::tempdir().expect("tempdir");
    let stub = stub_dir.path().join("cargo");
    fs::write(
        &stub,
        "#!/bin/sh\n\
         # `--version` succeeds so the gate gets past its tool check; the fetch\n\
         # always fails, standing in for an unreachable advisory database.\n\
         case \"$*\" in\n\
         *--version*) echo 'cargo-deny 0.0.0-stub'; exit 0 ;;\n\
         *'fetch db'*) echo 'stub: network unreachable' >&2; exit 1 ;;\n\
         *) exit 0 ;;\n\
         esac\n",
    )
    .expect("write stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod stub");

    let path = format!(
        "{}:{}",
        stub_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(workspace_root().join("scripts/check-advisories.sh"))
        .current_dir(workspace_root())
        .env("PATH", path)
        .env("ADVISORY_DB_FETCH_RETRIES", "1")
        .output()
        .expect("failed to run check-advisories.sh");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "an unfetchable advisory database must fail the gate, got success:\n{combined}"
    );
    assert!(
        combined.contains("failing closed"),
        "the failure must say the gate failed closed rather than skipped:\n{combined}"
    );
}

/// The script has to keep the properties the docs promise: retries, an offline
/// check so a failure names an advisory, and a fail-closed exit.
#[test]
fn the_gate_script_retries_the_fetch_and_audits_offline() {
    let script = read_repo_file("scripts/check-advisories.sh");
    let code = code_only(&script);
    for expected in [
        "cargo deny fetch db",
        "--offline check advisories",
        "failing closed",
    ] {
        assert!(
            code.contains(expected),
            "the gate must keep {expected:?} in its executable body:\n{code}"
        );
    }
    assert!(
        code.contains("ADVISORY_DB_FETCH_RETRIES"),
        "the fetch must stay retryable:\n{code}"
    );
    assert!(
        code.contains("audit_scaffold_graph"),
        "the gate must audit the scaffold's day-one graph, not only the workspace:\n{code}"
    );
}

/// A gate the release does not depend on is a gate a release can ignore.
#[test]
fn prepare_release_depends_on_the_advisory_gate() {
    let gate = read_repo_file(".github/workflows/publish-gate.yml");
    let prepare = gate
        .split("\n  prepare-release:")
        .nth(1)
        .expect("publish-gate.yml must define a prepare-release job");
    let needs = prepare
        .lines()
        .find(|line| line.trim_start().starts_with("needs:"))
        .expect("prepare-release must declare its gate dependencies");
    assert!(
        needs.contains("advisories"),
        "the release must not be preparable while the advisory gate is red, got: {needs}"
    );
}

/// The framework's own advisory steps must block, exactly as the scaffold's do.
#[test]
fn the_frameworks_advisory_steps_cannot_be_soft_failed() {
    for workflow in [
        ".github/workflows/ci.yml",
        ".github/workflows/publish-gate.yml",
    ] {
        let yaml = code_only(&read_repo_file(workflow));
        for step in [
            "Dependency advisory gate",
            "Advisory gate self-test (injected vulnerable dependency)",
        ] {
            let body = workflow_step(&yaml, step);
            assert!(
                !body.contains("continue-on-error"),
                "{workflow}'s {step:?} step must fail the job:\n{body}"
            );
            assert!(
                !body.contains("|| true"),
                "{workflow}'s {step:?} step must not swallow its exit code:\n{body}"
            );
        }
    }
}

/// Waivers in the framework's own policy are held to what the scaffold's are:
/// an id alone records a decision nobody can review.
#[test]
fn the_frameworks_own_waivers_carry_a_reason() {
    for config in ["deny.toml", "deny-sqlite.toml"] {
        let policy = read_repo_file(config);
        let entries = code_only(&policy);
        for id in waived_ids(&policy) {
            let entry = entries
                .lines()
                .find(|line| line.contains(&id) && line.contains("id ="))
                .unwrap_or_else(|| panic!("{config}: {id} must be a structured ignore entry"));
            assert!(
                entry.contains("reason ="),
                "{config}: waiver for {id} must record a rationale, got: {entry}"
            );
        }
    }
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

/// The upgrade path's instruction is a command people run verbatim, and the
/// policy it tells them to copy is flavor-specific: a `--bundled-pg` app given
/// the default flavor's policy fails the very audit the instruction exists to
/// satisfy.
#[test]
fn the_migration_note_keeps_the_donor_policy_flavor_correct() {
    let guide = read_repo_file("docs/migrations/next.md");
    let note = guide
        .split("### CI: `autumn upgrade` adds a blocking dependency audit")
        .nth(1)
        .expect("the migration guide must carry the deny.toml step")
        .split("\n### ")
        .next()
        .unwrap_or_default();

    assert!(
        note.contains("--bundled-pg"),
        "the donor project must be generated with the reader's original flags:\n{note}"
    );
    // `autumn new` validates its argument as a Rust package name, so a path
    // argument is rejected before anything is generated.
    assert!(
        !note.contains("autumn new /"),
        "`autumn new` takes a package name, not a path — this command would fail \
         for every reader who ran it:\n{note}"
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
