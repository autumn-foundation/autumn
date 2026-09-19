//! The published platform-support policy must match the shipped one (#1616).
//!
//! `docs/guide/platform-support.md` is the artifact a Windows developer reads
//! before adopting autumn, and `autumn-cli/src/platform.rs` is what the CLI
//! actually enforces. A policy that says one thing and behaves another way is
//! worse than no policy — it is the Rails failure mode the issue cites. These
//! tests bind the two together, so adding a Tier 2 command without documenting
//! it (or documenting a tier the code does not enforce) fails the build.
//!
//! The policy table itself lives in the binary crate, so this suite reads the
//! source rather than linking it: the parity that matters is between two files a
//! human edits, and reading both is the check.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("autumn-cli lives under the workspace root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn policy_source() -> String {
    read("autumn-cli/src/platform.rs")
}

fn policy_doc() -> String {
    read("docs/guide/platform-support.md")
}

/// The policy table as `(command, tier)` pairs, in declaration order.
///
/// Parsed from the source rather than imported because `platform.rs` belongs to
/// a binary crate that integration tests cannot link against.
fn policy_rows() -> Vec<(String, String)> {
    let source = policy_source();
    let table = source
        .split_once("pub const POLICY: &[PolicyEntry] = &[")
        .expect("POLICY table should exist")
        .1
        .split_once("\n];")
        .expect("POLICY table should be terminated")
        .0
        .to_owned();

    let mut rows = Vec::new();
    let mut pending: Option<String> = None;
    for line in table.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("command: \"")
            && let Some(command) = rest.strip_suffix("\",")
        {
            pending = Some(command.to_owned());
        } else if let Some(rest) = line.strip_prefix("tier: SupportTier::")
            && let Some(tier) = rest.strip_suffix(",")
            && let Some(command) = pending.take()
        {
            rows.push((command, tier.to_owned()));
        }
    }
    rows
}

fn policy_commands() -> Vec<String> {
    policy_rows().into_iter().map(|(c, _)| c).collect()
}

/// The commands the code assigns to `tier` (`"Native"` or `"Wsl2"`).
fn code_tier(tier: &str) -> Vec<String> {
    policy_rows()
        .into_iter()
        .filter(|(_, t)| t == tier)
        .map(|(c, _)| c)
        .collect()
}

/// The first-column entries of the Markdown table under the heading that starts
/// with `heading`, with surrounding backticks stripped.
///
/// This is what makes the parity real rather than decorative: a substring search
/// would happily pass with `autumn deploy` moved from the Tier 2 table into the
/// Tier 1 one — the precise drift the policy exists to prevent.
fn guide_tier(heading: &str) -> Vec<String> {
    let doc = policy_doc();
    let after = doc
        .split_once(heading)
        .unwrap_or_else(|| panic!("the guide must have a section starting `{heading}`"))
        .1;
    // Stop at the next top-level heading so a later table cannot leak in.
    let section = after
        .split_once("\n## ")
        .map_or(after, |(before, _)| before);

    section
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .filter_map(|line| line.split('|').nth(1))
        .map(|cell| cell.trim().trim_matches('`').trim().to_owned())
        .filter(|cell| !cell.is_empty() && cell != "Command" && !cell.starts_with("---"))
        .collect()
}

#[test]
fn the_policy_table_is_not_empty() {
    // Guards the parser itself: a refactor that renames the table would
    // otherwise turn every parity test below into a vacuous pass.
    assert!(
        policy_commands().len() >= 10,
        "parsed too few policy rows — has the table been renamed? {:?}",
        policy_commands()
    );
}

#[test]
fn every_policy_command_appears_in_the_published_guide() {
    let doc = policy_doc();
    for command in policy_commands() {
        assert!(
            doc.contains(&command),
            "`{command}` is enforced by autumn-cli/src/platform.rs but is not \
             listed in docs/guide/platform-support.md"
        );
    }
}

#[test]
fn the_guide_and_the_code_agree_on_tier_1_exactly() {
    let code = code_tier("Native");
    let guide = guide_tier("## Tier 1 —");
    assert_eq!(
        code, guide,
        "the Tier 1 table in docs/guide/platform-support.md must list exactly \
         the SupportTier::Native rows of autumn-cli/src/platform.rs, in order"
    );
}

#[test]
fn the_guide_and_the_code_agree_on_tier_2_exactly() {
    let code = code_tier("Wsl2");
    let guide = guide_tier("## Tier 2 —");
    assert_eq!(
        code, guide,
        "the Tier 2 table in docs/guide/platform-support.md must list exactly \
         the SupportTier::Wsl2 rows of autumn-cli/src/platform.rs, in order"
    );
}

#[test]
fn the_tier_parser_actually_reads_tiers() {
    // Guards the parser: if `policy_rows` silently returned no tiers, the two
    // set-equality tests above would compare empty vectors and pass.
    let rows = policy_rows();
    assert!(rows.iter().any(|(_, t)| t == "Native"), "{rows:?}");
    assert!(rows.iter().any(|(_, t)| t == "Wsl2"), "{rows:?}");
    assert_eq!(
        rows.len(),
        policy_commands().len(),
        "every command row must carry a tier"
    );
}

#[test]
fn the_guide_publishes_both_tier_headings() {
    let doc = policy_doc();
    assert!(doc.contains("Tier 1"), "the guide must name Tier 1");
    assert!(doc.contains("Tier 2"), "the guide must name Tier 2");
    assert!(
        doc.contains("WSL2"),
        "the guide must name WSL2 as the Tier 2 answer"
    );
}

#[test]
fn the_guide_names_the_ac_mandated_tier_one_floor() {
    // The issue fixes this list as the minimum Tier 1 surface. Documenting less
    // than this is a policy regression regardless of what the table says.
    let doc = policy_doc();
    for command in [
        "autumn new",
        "autumn doctor",
        "autumn setup",
        "autumn dev",
        "autumn test",
        "managed Postgres",
    ] {
        assert!(doc.contains(command), "the guide must name `{command}`");
    }
}

#[test]
fn the_guide_documents_the_windows_prerequisites_doctor_reports() {
    let doc = policy_doc();
    assert!(
        doc.contains("vcpkg") && doc.contains("VCPKG_ROOT"),
        "the guide must document the OpenSSL/vcpkg prerequisite doctor flags"
    );
    assert!(
        doc.contains("--passkeys"),
        "the guide must say which command needs it"
    );
}

#[test]
fn the_guide_documents_the_service_elevation_prerequisite() {
    // #1639 added it to `WINDOWS_PREREQUISITES`, so `doctor` prints it. A
    // prerequisite doctor names and the guide does not is one an operator meets
    // as an access-denied error halfway through a registration.
    let source = policy_source();
    assert!(
        source.contains("install-service") && source.contains("Administrator"),
        "the policy must carry the service-registration prerequisite"
    );
    let doc = policy_doc();
    assert!(
        doc.contains("install-service") && doc.contains("Administrator"),
        "the guide must document the elevation the service journey needs"
    );
}

#[test]
fn the_guide_records_that_the_daemon_lifecycle_left_tier_2() {
    // The move is the point of #1639, and a guide that still routes a Windows
    // operator to WSL2 for it would undo the slice while every table stayed
    // consistent.
    let doc = policy_doc();
    let tier_two = guide_tier("## Tier 2");
    assert!(
        !tier_two
            .iter()
            .any(|command| command.contains("serve --daemon")),
        "the daemon lifecycle must not be listed under Tier 2: {tier_two:?}"
    );
    assert!(
        doc.contains("#1639"),
        "the guide must say where the promotion came from"
    );
}

#[test]
fn the_guide_assigns_the_1456_browser_probe_a_tier_with_a_workaround() {
    // AC 5: #1456 is either resolved under this target or explicitly assigned
    // to a tier with a documented workaround. It IS resolved (the Windows
    // probe now accepts an existing `.exe` without executing it), and the
    // guide has to say so — an undocumented fix is indistinguishable from an
    // open bug to the developer who hit it.
    let doc = policy_doc();
    assert!(doc.contains("#1456"), "the guide must reference #1456");
    assert!(
        doc.contains("SystemTest"),
        "the guide must name the affected surface"
    );
}

#[test]
fn the_guide_documents_the_ci_gate_that_enforces_tier_one() {
    // A tier promise with no gate is a wish. The guide must point at the job.
    let doc = policy_doc();
    assert!(
        doc.contains("windows-latest"),
        "the guide must name the CI runner enforcing Tier 1"
    );
}

#[test]
fn the_readme_links_the_policy_so_it_is_findable_before_adoption() {
    // AC 1 says "README + docs": a Windows developer evaluating autumn reads
    // the README first, and that is where the cliff has to be signposted.
    let readme = read("README.md");
    assert!(
        readme.contains("platform-support.md"),
        "README.md must link docs/guide/platform-support.md"
    );
    assert!(
        readme.contains("WSL2"),
        "README.md must mention WSL2 so the Tier 2 answer is visible up front"
    );
}

#[test]
fn the_policy_doc_url_in_the_code_points_at_the_published_guide() {
    let source = policy_source();
    assert!(
        source.contains("docs/guide/platform-support.md"),
        "POLICY_DOC_URL must point at the published guide"
    );
}

#[test]
fn the_windows_journey_job_still_gates_the_daemon_lifecycle() {
    // #1639 promoted the daemon lifecycle to Tier 1, and a tier promise with no
    // gate is a wish. The `windows-tier1` job's own meta-test below only checks
    // that the job exists — these steps could be deleted without a single test
    // going red, which is exactly how a Tier 1 claim rots back into a Tier 2
    // reality. Pinned by step name, the way this repo already pins the
    // cold-start tests it needs named in `generator-conformance.yml`.
    let ci = read(".github/workflows/ci.yml");
    for step in [
        "Journey: the daemon starts, is discoverable, and serves",
        "Journey: stop drains in-flight requests and stops Postgres cleanly",
        "Journey: restart brings the daemon back as a new process",
        "Journey: register a boot-start service that survives a crash",
    ] {
        assert!(
            ci.contains(step),
            "the Windows journey must still gate `{step}`"
        );
    }
}

#[test]
fn the_daemon_journey_asserts_the_things_the_acceptance_criteria_name() {
    // Guards against the steps surviving as names while their assertions are
    // hollowed out. Each string below is the one that makes its step mean what
    // its title says.
    let ci = read(".github/workflows/ci.yml");
    for (what, needle) in [
        ("a tcp discovery file", r#"transport\s*=\s*"tcp""#),
        ("a second start is rejected", "already running"),
        ("the stop is observably a drain", "prestop grace"),
        // The drain must be graded on an APPLICATION route. `/health` is an
        // alias for the readiness probe, which phase 2 of the shutdown sequence
        // flips to 503 on purpose while the listener is still accepting — so
        // grading the drain on it calls a correct drain a failure.
        ("the drain is graded on a real route", "hello=200"),
        // And the flip itself is graded, so the timing assertion cannot be
        // satisfied by a server that merely took the whole budget to die.
        ("readiness drains before the listener closes", "health=503"),
        ("the managed cluster is not orphaned", "postmaster.pid"),
        ("boot start", "AUTO_START"),
        ("crash restart", "RESTART"),
        (
            "a supervised restart is observed",
            "supervised restart observed",
        ),
        ("the service entry is gone", "1060"),
    ] {
        assert!(
            ci.contains(needle),
            "the Windows journey must still assert {what}"
        );
    }
}

#[test]
fn the_windows_tier_one_ci_job_exists_and_gates_trunk_dev() {
    // AC 3: the journey job must actually exist, run on windows-latest, and be
    // wired into a workflow that runs on pull requests targeting trunk-dev.
    let ci = read(".github/workflows/ci.yml");
    let job = ci
        .split_once("\n  windows-tier1:\n")
        .expect("ci.yml must define a `windows-tier1` job")
        .1;
    // Bound the assertion to this job's own block, so the `windows-latest` in
    // the `test` matrix elsewhere in the file cannot satisfy it. A job's keys
    // are indented four spaces; the next job's name is indented two.
    let block: String = job
        .lines()
        .take_while(|line| line.trim().is_empty() || line.starts_with("    "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        block.contains("runs-on: windows-latest"),
        "the Tier 1 journey job must run on windows-latest"
    );
    // The whole point is that it gates merges: it has to live in the workflow
    // that runs on pull requests into trunk-dev.
    let triggers = ci.split_once("\njobs:").expect("ci.yml has jobs").0;
    assert!(
        triggers.contains("trunk-dev"),
        "the workflow carrying it must run on trunk-dev pull requests"
    );
}
