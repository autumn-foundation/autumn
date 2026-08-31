//! Supply-chain gates for issue #1615: SBOMs and signed provenance for
//! framework and app releases.
//!
//! Two halves:
//!
//! * **Scaffold** — what `autumn release init` writes into a user's project,
//!   and what `autumn sbom` does when run against a real source tree or a
//!   real compiled binary.
//! * **Framework release train** — the workflow/script/doc content that makes
//!   autumn's own releases carry an SBOM and a provenance attestation.
//!
//! The second half asserts on YAML that only ever executes at tag time. That
//! is deliberate: without these tests a regression in the release train is
//! discovered during a release, which is the most expensive moment to find it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("autumn-cli should live under the workspace root")
        .to_path_buf()
}

fn read_repo_file(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// Strip comment lines so a content assertion cannot be satisfied by prose.
///
/// Both the Dockerfiles and the workflow YAML in this repo carry long
/// explanatory comments that mention the very commands and paths these tests
/// look for — asserting on the raw text lets a deleted step keep passing
/// because its rationale comment survived.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// Run `autumn release init` in a throwaway project directory and return it.
fn release_init(target: Option<&str>) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("failed to create temp dir");
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();

    let mut args = vec!["release", "init"];
    if let Some(t) = target {
        args.push("--target");
        args.push(t);
    }
    let output = Command::new(autumn_bin())
        .args(&args)
        .current_dir(temp.path())
        .output()
        .expect("failed to run `autumn release init`");
    assert!(
        output.status.success(),
        "autumn release init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    temp
}

// ===========================================================================
// AC4 — the scaffolded production image
// ===========================================================================

/// The issue's headline defect: the release Dockerfile `curl`s the Tailwind
/// binary with no integrity check, while `autumn setup` SHA-256-verifies the
/// very same download. Route the image build through the verified path.
#[test]
fn scaffold_dockerfile_downloads_no_unverified_executable() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();

    assert!(
        !dockerfile.contains("tailwindcss/releases/download"),
        "the Dockerfile must not fetch the Tailwind binary itself — \
         `autumn setup` does it with SHA-256 verification. Got:\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("autumn setup"),
        "the Dockerfile must obtain Tailwind via `autumn setup`, whose \
         download is checksum-verified. Got:\n{dockerfile}"
    );

    // Belt and braces: nothing may curl a file into an executable path without
    // a checksum verification in the same command. Join `\`-continued lines
    // first — a RUN spanning ten physical lines is one logical command, and a
    // per-line scan would never see the two halves together.
    let logical = code_only(&dockerfile).replace("\\\n", " ");
    for line in logical.lines() {
        let l = line.trim();
        if l.contains("curl") && l.contains("chmod +x") {
            assert!(
                l.contains("sha256sum") || l.contains("autumn setup"),
                "unverified download piped into an executable: {l}"
            );
        }
    }
}

/// The same defect existed in the Dockerfile `autumn new` writes, which fetched
/// the Tailwind binary with no verification (and, worse, hardcoded `linux-x64`
/// so an arm64 build silently installed an x86 binary). Fixing only the release
/// Dockerfile would leave an identical hole one command away.
#[test]
fn new_app_dockerfile_verifies_its_tailwind_download() {
    let temp = tempfile::tempdir().expect("failed to create temp dir");
    let output = Command::new(autumn_bin())
        .args(["new", "verified-app"])
        .current_dir(temp.path())
        .output()
        .expect("failed to run `autumn new`");
    assert!(
        output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let dockerfile =
        fs::read_to_string(temp.path().join("verified-app").join("Dockerfile")).unwrap();

    let code = code_only(&dockerfile);
    assert!(
        code.contains("sha256sums.txt") && code.contains("sha256sum -c"),
        "the generated Dockerfile must verify the Tailwind download's SHA-256, \
         got:\n{dockerfile}"
    );
    // Verification is only a check if it happens BEFORE the binary is put in
    // place and made executable.
    let verify_at = code.find("sha256sum -c").unwrap();
    let install_at = code
        .find("chmod +x target/autumn/tailwindcss")
        .expect("the Dockerfile must make the binary executable somewhere");
    assert!(
        verify_at < install_at,
        "the checksum must be verified before the download is installed:\n{code}"
    );
    assert!(
        code.contains("test -s expected.sha256"),
        "an extraction that matched nothing must fail rather than verify an empty \
         list:\n{code}"
    );
    assert!(
        code.contains("tailwindcss-linux-arm64") && code.contains("uname -m"),
        "the generated Dockerfile must pick the asset for the build \
         architecture — hardcoding x86 silently installs an unrunnable binary \
         on arm64. Got:\n{dockerfile}"
    );
}

/// The production image gets Tailwind through `autumn setup` while the
/// `autumn new` image still pins the version itself. Nothing else stops the
/// two surfaces from silently compiling assets with different Tailwind builds.
#[test]
fn both_tailwind_pins_agree() {
    let setup = read_repo_file("autumn-cli/src/setup.rs");
    let setup_pin = setup
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("const TAILWIND_VERSION: &str = \"")
                .and_then(|r| r.split('"').next())
        })
        .expect("setup.rs must pin a Tailwind version");

    let dockerfile = read_repo_file("autumn-cli/src/templates/Dockerfile.tmpl");
    let template_pin = dockerfile
        .lines()
        .find_map(|l| l.trim().strip_prefix("ARG TAILWIND_VERSION="))
        .expect("the autumn new Dockerfile must pin a Tailwind version");

    assert_eq!(
        setup_pin, template_pin,
        "`autumn setup` (which the production image uses) and the `autumn new` \
         Dockerfile must pin the same Tailwind release, or the two images \
         compile CSS with different builds"
    );
}

/// The version the generated Dockerfile pins for `cargo install autumn-cli`.
/// The SBOM steps can only be emitted when that pin can actually run them.
fn pinned_cli_version(dockerfile: &str) -> String {
    dockerfile
        .lines()
        .find_map(|l| l.trim().strip_prefix("ARG AUTUMN_CLI_VERSION="))
        .expect("the Dockerfile must pin an autumn-cli version")
        .to_owned()
}

/// True when `version` is strictly after 0.7.0 — the last release published
/// without `autumn sbom`. Mirrors `release.rs`'s `SBOM_MIN_CLI_VERSION` gate.
fn pin_ships_autumn_sbom(version: &str) -> bool {
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<u64> = core.split('.').filter_map(|p| p.parse().ok()).collect();
    parts.len() == 3 && (parts[0], parts[1], parts[2]) >= (0, 7, 1)
}

/// AC3: the shipped binary must be able to report its own crate versions with
/// no source tree. `cargo-auditable` embeds that list — reached through the
/// `cargo auditable` SUBCOMMAND, never a bare `RUSTC_WORKSPACE_WRAPPER`, which
/// cargo-auditable refuses to run under and which would kill the build on
/// cargo's first `rustc -vV` probe.
#[test]
fn scaffold_dockerfile_builds_an_auditable_binary() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();

    assert!(
        dockerfile.contains("cargo install --locked cargo-auditable"),
        "the builder stage must install cargo-auditable, got:\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("RUN cargo auditable build --release"),
        "the compile must go through `cargo auditable build`, got:\n{dockerfile}"
    );
    assert!(
        !code_only(&dockerfile).contains("RUSTC_WORKSPACE_WRAPPER"),
        "cargo-auditable cannot run as a bare rustc wrapper — it exits 1 unless \
         invoked through `cargo auditable`. Got:\n{dockerfile}"
    );
}

/// The embedded single-binary variant compiles TWICE; both phases must carry
/// the dependency list or the shipped binary is the un-instrumented one.
#[test]
fn scaffold_dockerfile_keeps_the_embed_build_auditable_too() {
    let release_tmpl = read_repo_file("autumn-cli/src/release.rs");
    assert!(
        release_tmpl.contains("RUN autumn build --embed --auditable"),
        "the embed build step must pass --auditable"
    );
}

/// AC4: the image itself carries a machine-readable SBOM at a predictable path.
///
/// Stated as an invariant rather than a flat assertion, because the steps are
/// gated on the pinned CLI being able to run `autumn sbom`: between this
/// landing and the next release the pin is an already-published version that
/// predates the subcommand, and emitting the step anyway would make every
/// `docker build` fail. Once a CLI that ships it is what gets installed, the
/// first branch is the one that runs — with no test change.
#[test]
fn scaffold_dockerfile_bakes_an_sbom_into_the_image() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();
    let pin = pinned_cli_version(&dockerfile);

    if pin_ships_autumn_sbom(&pin) {
        assert!(
            dockerfile.contains("autumn sbom --output"),
            "the builder stage must generate a CycloneDX SBOM, got:\n{dockerfile}"
        );
        assert!(
            dockerfile.contains("/usr/share/autumn/sbom.cdx.json"),
            "the SBOM must land at the documented path, got:\n{dockerfile}"
        );
        assert!(
            dockerfile.contains("io.autumn.sbom.path"),
            "the image must advertise its SBOM path as a label, got:\n{dockerfile}"
        );
        let runtime = dockerfile
            .split_once("AS runtime")
            .expect("Dockerfile must have a runtime stage")
            .1;
        assert!(
            runtime.contains("sbom.cdx.json"),
            "the runtime stage must COPY the SBOM out of the builder, got:\n{runtime}"
        );
    } else {
        assert!(
            !dockerfile.contains("autumn sbom"),
            "the pinned CLI ({pin}) has no `sbom` subcommand, so calling it would \
             break every docker build:\n{dockerfile}"
        );
        assert!(
            !dockerfile.contains("sbom.cdx.json"),
            "…and nothing may COPY a file that was never generated:\n{dockerfile}"
        );
    }
}

/// OCI metadata is emitted regardless of the SBOM gate.
#[test]
fn scaffold_dockerfile_labels_the_image() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();
    assert!(
        dockerfile.contains("org.opencontainers.image."),
        "the runtime image must carry OCI image labels, got:\n{dockerfile}"
    );
}

/// The image build must not silently fall back to a non-auditable, SBOM-less
/// image when a supply-chain step fails — that would defeat the whole default.
#[test]
fn scaffold_dockerfile_does_not_swallow_supply_chain_failures() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();
    // Join continued lines first: a `\`-continued RUN is one logical command,
    // so a per-physical-line scan would miss `|| true` on its own line.
    let logical = dockerfile.replace("\\\n", " ");
    for line in logical.lines() {
        let l = line.trim();
        if l.contains("autumn sbom") || l.contains("autumn setup") || l.contains("cargo auditable")
        {
            assert!(
                !l.contains("|| true") && !l.contains("|| :"),
                "supply-chain steps must fail the build, not be swallowed: {l}"
            );
        }
    }
}

// ===========================================================================
// AC4 — provenance for the scaffolded app's image, by default
// ===========================================================================

/// Every scaffold target that emits a deploy workflow must attest what it
/// pushes. Derived from `release.rs`'s own target list rather than hardcoded,
/// so adding a fourth workflow-emitting target cannot quietly skip this.
fn workflow_emitting_targets() -> Vec<(String, String)> {
    let release_rs = read_repo_file("autumn-cli/src/release.rs");
    let mut out = Vec::new();
    for (flag, workflow) in [
        ("aws-ecs", ".github/workflows/aws-deploy.yml"),
        ("gcp-cloud-run", ".github/workflows/gcp-deploy.yml"),
        ("azure-container-apps", ".github/workflows/azure-deploy.yml"),
        ("aws-app-runner", ".github/workflows/aws-deploy.yml"),
        ("fly", ".github/workflows/fly-deploy.yml"),
        ("docker-compose", ".github/workflows/compose-deploy.yml"),
    ] {
        if release_rs.contains(&format!("\"{workflow}\""))
            && !out.iter().any(|(_, w)| w == workflow)
        {
            out.push((flag.to_owned(), workflow.to_owned()));
        }
    }
    assert!(
        !out.is_empty(),
        "release.rs should scaffold at least one deploy workflow"
    );
    out
}

#[test]
fn scaffolded_deploy_workflows_attest_the_image_they_push() {
    for (target, workflow) in workflow_emitting_targets() {
        let temp = release_init(Some(&target));
        let yaml = fs::read_to_string(temp.path().join(&workflow))
            .unwrap_or_else(|e| panic!("{target}: failed to read {workflow}: {e}"));

        assert!(
            yaml.contains("actions/attest-build-provenance"),
            "{target}: the deploy workflow must attest the image it pushes:\n{yaml}"
        );
        assert!(
            yaml.contains("attestations: write"),
            "{target}: attesting needs `attestations: write`:\n{yaml}"
        );
        assert!(
            yaml.contains("id-token: write"),
            "{target}: keyless signing needs `id-token: write`:\n{yaml}"
        );
        assert!(
            yaml.contains("subject-digest"),
            "{target}: the attestation must bind the pushed image DIGEST, not a \
             mutable tag:\n{yaml}"
        );
    }
}

/// The attestation steps must not be able to block a deploy: by the time they
/// run the image is already in the registry, and a repository whose plan has
/// no artifact attestations would otherwise never reach production.
#[test]
fn attestation_never_gates_the_deploy_itself() {
    for (target, workflow) in workflow_emitting_targets() {
        let temp = release_init(Some(&target));
        let yaml = fs::read_to_string(temp.path().join(&workflow)).unwrap();

        let attest_at = yaml
            .find("actions/attest-build-provenance")
            .unwrap_or_else(|| panic!("{target}: no attestation step"));
        let deploy_at = yaml
            .rfind("- name: Deploy")
            .or_else(|| yaml.rfind("- name: Update"))
            .or_else(|| yaml.rfind("- name: Roll"))
            .unwrap_or_else(|| panic!("{target}: could not locate the deploy step:\n{yaml}"));
        assert!(
            attest_at > deploy_at,
            "{target}: attestation must come AFTER the deploy — the image is \
             already pushed, so failing here would block a shipped release"
        );

        let block = &yaml[attest_at.saturating_sub(600)..];
        assert!(
            block.contains("continue-on-error: true"),
            "{target}: a Sigstore hiccup must not fail a successful deploy:\n{block}"
        );
    }
}

#[test]
fn scaffolded_deploy_workflows_resolve_the_digest_by_repository() {
    for (target, workflow) in workflow_emitting_targets() {
        let temp = release_init(Some(&target));
        let yaml = fs::read_to_string(temp.path().join(&workflow)).unwrap();
        assert!(
            !yaml.contains("index .RepoDigests 0"),
            "{target}: `RepoDigests` belongs to the image ID and can list several \
             repositories — taking element 0 by position can attest the wrong \
             one:\n{yaml}"
        );
        assert!(
            yaml.contains("range .RepoDigests"),
            "{target}: the digest must be matched against the repository actually \
             pushed to:\n{yaml}"
        );
    }
}

#[test]
fn scaffolded_deploy_workflows_attest_the_image_sbom() {
    for (target, workflow) in workflow_emitting_targets() {
        let temp = release_init(Some(&target));
        let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();
        let yaml = fs::read_to_string(temp.path().join(&workflow)).unwrap();

        // The SBOM attestation extracts a file the image only carries when the
        // pinned CLI could generate it, so the two must move together.
        if pin_ships_autumn_sbom(&pinned_cli_version(&dockerfile)) {
            assert!(
                yaml.contains("actions/attest-sbom"),
                "{target}: the SBOM baked into the image must itself be attested:\n{yaml}"
            );
        } else {
            assert!(
                !yaml.contains("actions/attest-sbom"),
                "{target}: nothing may attest an SBOM the image does not carry:\n{yaml}"
            );
        }
    }
}

// ===========================================================================
// AC1 + AC2 — autumn's own release train
// ===========================================================================

#[test]
fn publish_gate_runs_the_sbom_gate() {
    let gate = code_only(&read_repo_file(".github/workflows/publish-gate.yml"));
    assert!(
        gate.contains("check-sbom.sh"),
        "publish-gate.yml must run the SBOM gate script:\n{gate}"
    );
    let prepare = gate
        .split_once("prepare-release:")
        .expect("publish-gate.yml must still have a prepare-release job")
        .1;
    assert!(
        prepare.contains("sbom.cdx.json"),
        "prepare-release must carry the SBOM release artifact:\n{prepare}"
    );
}

/// Removing `sbom` from `prepare-release`'s `needs:` would silently un-gate
/// the release: the job would still find the artifact from a concurrent run,
/// or fail late, instead of the gate simply being required.
#[test]
fn prepare_release_depends_on_the_sbom_gate() {
    let gate = code_only(&read_repo_file(".github/workflows/publish-gate.yml"));
    let prepare = gate.split_once("prepare-release:").unwrap().1;
    let needs = prepare
        .lines()
        .find(|l| l.trim_start().starts_with("needs:"))
        .expect("prepare-release must declare needs:");
    assert!(
        needs.contains("sbom"),
        "prepare-release must depend on the sbom gate, got: {needs}"
    );
}

/// The SBOM handed to the release must be re-verified AFTER it travelled
/// through the artifact store — that is the only point where a substitution or
/// truncation can actually happen.
#[test]
fn prepare_release_reverifies_the_downloaded_sbom() {
    let gate = code_only(&read_repo_file(".github/workflows/publish-gate.yml"));
    let prepare = gate.split_once("prepare-release:").unwrap().1;
    assert!(
        prepare.contains("--verify sbom.cdx.json"),
        "prepare-release must re-verify the downloaded SBOM, not just shape-check \
         it:\n{prepare}"
    );
    assert!(
        prepare.contains("--expect-version"),
        "…and pin it to the version being released:\n{prepare}"
    );
}

#[test]
fn sbom_gate_script_exists_and_is_executable() {
    let path = workspace_root().join("scripts/check-sbom.sh");
    assert!(path.is_file(), "scripts/check-sbom.sh must exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "scripts/check-sbom.sh must be executable"
        );
    }
}

/// Actually RUN the gate, rather than grepping it. The tag/version
/// disagreement is the one part of AC1 that can genuinely fail at release
/// time, so it needs to be exercised, not asserted about.
#[cfg(unix)]
#[test]
fn sbom_gate_rejects_a_tag_that_disagrees_with_the_workspace_version() {
    let script = workspace_root().join("scripts/check-sbom.sh");
    let out = Command::new(&script)
        .current_dir(workspace_root())
        .env("RELEASE_TAG", "v99.99.99")
        .env("SBOM_OUT", "target/sbom-gate-test.cdx.json")
        .output()
        .expect("failed to run check-sbom.sh");

    assert!(
        !out.status.success(),
        "a tag that does not match the workspace version must fail the gate"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("v99.99.99"),
        "the failure must name the offending tag, got:\n{combined}"
    );
}

#[test]
fn release_workflow_attaches_the_sbom() {
    let release = code_only(&read_repo_file(".github/workflows/release.yml"));
    assert!(
        release.contains("mv sbom.cdx.json"),
        "release.yml must stage the SBOM as a named release asset:\n{release}"
    );
    assert!(
        release.contains("files: autumn-"),
        "release.yml must pass the SBOM to the release action's `files:`:\n{release}"
    );
}

/// The SBOM's provenance must be minted where the workflow actually runs on
/// the tag. `release.yml` is `workflow_run`-triggered, which GitHub always
/// runs in default-branch context, so an attestation created there would
/// record the default branch rather than the tagged commit. The Publish Gate
/// is triggered by the tag push itself.
#[test]
fn the_sbom_is_attested_in_tag_context() {
    let gate = code_only(&read_repo_file(".github/workflows/publish-gate.yml"));
    let sbom_job = gate
        .split_once("\n  sbom:")
        .expect("publish-gate.yml must have an sbom job")
        .1;
    let job = sbom_job.split("\n  smoke:").next().unwrap();
    assert!(
        job.contains("actions/attest-build-provenance"),
        "the sbom job must attest the document it produces:\n{job}"
    );
    assert!(
        job.contains("id-token: write") && job.contains("attestations: write"),
        "the sbom job needs its own permissions — publish-gate is `contents: \
         read` at workflow level:\n{job}"
    );
    assert!(
        job.contains("github.ref_type == 'tag'"),
        "a pull-request run has nothing to attest:\n{job}"
    );

    let release = code_only(&read_repo_file(".github/workflows/release.yml"));
    assert!(
        !release.contains("actions/attest-build-provenance"),
        "release.yml must not mint a second, default-branch-scoped attestation \
         for the same file:\n{release}"
    );
}

/// A reusable workflow can only DOWNGRADE the caller job's token, so the
/// `permissions:` block inside cli-release.yml is inert unless the calling job
/// grants the same scopes. Without this the attestation step fails and, running
/// before the upload, the release ends up with no CLI binaries at all.
#[test]
fn the_binaries_job_passes_down_the_scopes_attestation_needs() {
    let release = code_only(&read_repo_file(".github/workflows/release.yml"));
    let binaries = release
        .split_once("binaries:")
        .expect("release.yml must still have a binaries job")
        .1;
    let perms = binaries
        .split_once("permissions:")
        .expect("the binaries job must declare permissions")
        .1;
    // Only look at the permissions block itself, not the rest of the file.
    let block: String = perms
        .lines()
        .take_while(|l| l.trim().is_empty() || l.starts_with("      "))
        .collect::<Vec<_>>()
        .join("\n");
    for scope in ["id-token: write", "attestations: write", "contents: write"] {
        assert!(
            block.contains(scope),
            "the binaries job must grant `{scope}` — a called workflow cannot add \
             it back:\n{block}"
        );
    }
}

#[test]
fn cli_release_workflow_attests_every_binary_archive() {
    let cli_release = code_only(&read_repo_file(".github/workflows/cli-release.yml"));
    assert!(
        cli_release.contains("actions/attest-build-provenance"),
        "cli-release.yml must attest each per-target archive:\n{cli_release}"
    );
    assert!(
        cli_release.contains("attestations: write") && cli_release.contains("id-token: write"),
        "cli-release.yml needs `attestations: write` + `id-token: write`:\n{cli_release}"
    );

    let attest_at = cli_release.find("actions/attest-build-provenance").unwrap();
    let upload_at = cli_release.find("gh release upload").unwrap();
    assert!(
        attest_at < upload_at,
        "the archive must be attested before it is attached to the release"
    );
}

// ===========================================================================
// AC5 — "verify what you're running"
// ===========================================================================

#[test]
fn verification_doc_covers_both_surfaces_with_runnable_commands() {
    let doc = read_repo_file("docs/guide/supply-chain.md");

    // Framework release asset.
    assert!(
        doc.contains("gh attestation verify"),
        "the doc must give the one-command verification:\n{doc}"
    );
    assert!(
        doc.contains("--repo autumn-foundation/autumn"),
        "the verification command must pin the expected repository — without \
         it, any repo's attestation would satisfy the check:\n{doc}"
    );
    // Scaffolded app image.
    assert!(
        doc.contains("oci://"),
        "the doc must show verifying a container image, not just a file:\n{doc}"
    );
    // The negative case is what proves the check is real.
    assert!(
        doc.to_lowercase().contains("tamper"),
        "the doc must show verification FAILING for a tampered asset:\n{doc}"
    );
    // AC3's documented command.
    assert!(
        doc.contains("autumn sbom --binary"),
        "the doc must document reading the crate list out of a binary:\n{doc}"
    );
}

#[test]
fn verification_doc_is_reachable_from_the_docs_a_reader_already_has() {
    for entry in ["docs/guide/deployment.md", "docs/release-checklist.md"] {
        let content = read_repo_file(entry);
        assert!(
            content.contains("supply-chain.md"),
            "{entry} must link to the supply-chain verification guide"
        );
    }
}

// ===========================================================================
// End-to-end: the command itself, against real inputs
// ===========================================================================

/// A dependency-free crate so `cargo metadata` resolves with no network.
fn tiny_crate() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"sbom-demo\"\nversion = \"4.5.6\"\nedition = \"2021\"\nlicense = \"MIT\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    temp
}

fn run_autumn(dir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let out = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

#[test]
fn sbom_describes_a_real_source_tree() {
    let temp = tiny_crate();
    let (stdout, stderr, code) = run_autumn(temp.path(), &["sbom"]);
    assert_eq!(code, Some(0), "autumn sbom failed: {stderr}");

    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("autumn sbom must emit JSON ({e}): {stdout}"));
    assert_eq!(v["bomFormat"], "CycloneDX");
    assert_eq!(v["metadata"]["component"]["name"], "sbom-demo");
    assert_eq!(v["metadata"]["component"]["version"], "4.5.6");
}

/// Two runs a second apart would not catch a second-granularity timestamp, so
/// assert on the absence of the fields that make a `CycloneDX` document vary at
/// all, and on the serial number being content-derived rather than random.
#[test]
fn sbom_is_reproducible_across_runs() {
    let temp = tiny_crate();
    let (first, _, _) = run_autumn(temp.path(), &["sbom"]);
    let (second, _, _) = run_autumn(temp.path(), &["sbom"]);
    assert_eq!(first, second, "autumn sbom output must be reproducible");
    assert!(!first.is_empty());

    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert!(
        v["metadata"].get("timestamp").is_none(),
        "a wall-clock timestamp would make --verify impossible: {first}"
    );
    // A serialNumber IS required (actions/attest-sbom rejects a CycloneDX
    // document without one) — but it must be derived, not random.
    let serial = v["serialNumber"].as_str().expect("serialNumber");
    assert!(serial.starts_with("urn:uuid:"), "{serial}");
}

/// `--expect-version` is what ties a document to the release being cut; a
/// disagreement has to fail the process, not just a pure function.
#[test]
fn sbom_expect_version_rejects_the_wrong_version() {
    let temp = tiny_crate();
    let (_, _, code) = run_autumn(temp.path(), &["sbom", "--expect-version", "4.5.6"]);
    assert_eq!(code, Some(0), "the crate really is at 4.5.6");

    let (_, stderr, code) = run_autumn(temp.path(), &["sbom", "--expect-version", "9.9.9"]);
    assert_eq!(code, Some(1), "a version disagreement must fail");
    assert!(
        stderr.contains("9.9.9") && stderr.contains("4.5.6"),
        "the error must name both versions, got: {stderr}"
    );
}

/// `--output` into a directory that does not exist yet must work: the release
/// gate and the image build both write into paths they do not pre-create.
#[test]
fn sbom_output_creates_missing_parent_directories() {
    let temp = tiny_crate();
    let (_, stderr, code) = run_autumn(temp.path(), &["sbom", "--output", "nested/dir/sbom.json"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(temp.path().join("nested/dir/sbom.json").is_file());
}

/// `--manifest-path` lets the gate describe a project it is not standing in.
#[test]
fn sbom_honours_an_explicit_manifest_path() {
    let temp = tiny_crate();
    let elsewhere = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("Cargo.toml");
    let (stdout, stderr, code) = run_autumn(
        elsewhere.path(),
        &["sbom", "--manifest-path", manifest.to_str().unwrap()],
    );
    assert_eq!(code, Some(0), "{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["metadata"]["component"]["name"], "sbom-demo");
}

#[test]
fn sbom_verify_accepts_its_own_output_and_rejects_a_tampered_one() {
    let temp = tiny_crate();
    let (_, stderr, code) = run_autumn(temp.path(), &["sbom", "--output", "sbom.cdx.json"]);
    assert_eq!(code, Some(0), "autumn sbom --output failed: {stderr}");

    let (_, stderr, code) = run_autumn(temp.path(), &["sbom", "--verify", "sbom.cdx.json"]);
    assert_eq!(
        code,
        Some(0),
        "a freshly generated SBOM must verify: {stderr}"
    );

    // Smuggle an extra component in, exactly as a compromised release would.
    let path = temp.path().join("sbom.cdx.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    doc["components"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "type": "library",
            "bom-ref": "pkg:cargo/backdoor@6.6.6",
            "name": "backdoor",
            "version": "6.6.6",
            "purl": "pkg:cargo/backdoor@6.6.6"
        }));
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    let (_, stderr, code) = run_autumn(temp.path(), &["sbom", "--verify", "sbom.cdx.json"]);
    assert_eq!(code, Some(1), "a tampered SBOM must fail the gate");
    assert!(
        stderr.contains("backdoor@6.6.6"),
        "the failure must name the offending component, got: {stderr}"
    );
}

#[test]
fn sbom_verify_rejects_a_missing_file() {
    let temp = tiny_crate();
    let (_, _, code) = run_autumn(temp.path(), &["sbom", "--verify", "nope.cdx.json"]);
    assert_eq!(code, Some(1), "a missing SBOM must fail the gate");
}

/// AC3, end to end: the crate list comes out of the compiled artifact with no
/// source tree and no lockfile in sight.
/// Writing fixed-width ELF headers is all narrowing casts by nature; every
/// value here is a handful of bytes.
#[allow(clippy::cast_possible_truncation)]
#[test]
fn sbom_reads_the_crate_list_out_of_a_compiled_binary() {
    use std::io::Write as _;

    let audit_json = r#"{"packages":[
        {"name":"prod-app","version":"9.9.9","source":"local","root":true,"dependencies":[1]},
        {"name":"tokio","version":"1.44.0","source":"crates.io","dependencies":[]}
    ]}"#;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(audit_json.as_bytes()).unwrap();
    let payload = enc.finish().unwrap();

    // A minimal ELF64 whose only real content is the `.dep-v0` section
    // cargo-auditable writes.
    let mut strtab = vec![0u8];
    let shstrtab_off = strtab.len() as u32;
    strtab.extend_from_slice(b".shstrtab\0");
    let dep_off = strtab.len() as u32;
    strtab.extend_from_slice(b".dep-v0\0");

    let ehdr = 64usize;
    let strtab_at = ehdr;
    let payload_at = strtab_at + strtab.len();
    let shoff = payload_at + payload.len();

    let mut elf = Vec::new();
    elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
    elf.extend_from_slice(&[0u8; 9]);
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type
    elf.extend_from_slice(&62u16.to_le_bytes()); // e_machine
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    elf.extend_from_slice(&0u64.to_le_bytes()); // e_phoff
    elf.extend_from_slice(&(shoff as u64).to_le_bytes()); // e_shoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    elf.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&3u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_shstrndx
    elf.extend_from_slice(&strtab);
    elf.extend_from_slice(&payload);

    let mut shdr = |name: u32, off: u64, size: u64| {
        elf.extend_from_slice(&name.to_le_bytes());
        elf.extend_from_slice(&1u32.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&off.to_le_bytes());
        elf.extend_from_slice(&size.to_le_bytes());
        elf.extend_from_slice(&0u32.to_le_bytes());
        elf.extend_from_slice(&0u32.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
    };
    shdr(0, 0, 0);
    shdr(shstrtab_off, strtab_at as u64, strtab.len() as u64);
    shdr(dep_off, payload_at as u64, payload.len() as u64);

    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("prod-app");
    fs::write(&bin, &elf).unwrap();

    // Deliberately run from a directory with no Cargo.toml and no Cargo.lock.
    let (stdout, stderr, code) =
        run_autumn(temp.path(), &["sbom", "--binary", bin.to_str().unwrap()]);
    assert_eq!(code, Some(0), "autumn sbom --binary failed: {stderr}");

    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["metadata"]["component"]["name"], "prod-app");
    assert_eq!(v["metadata"]["component"]["version"], "9.9.9");
    assert_eq!(v["components"][0]["name"], "tokio");
    assert_eq!(v["components"][0]["version"], "1.44.0");
}

#[test]
fn sbom_binary_explains_a_binary_built_without_cargo_auditable() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("plain");
    // A valid but audit-data-free ELF header is enough to reach the check.
    let mut elf = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
    elf.extend_from_slice(&[0u8; 57]);
    fs::write(&bin, &elf).unwrap();

    let (_, stderr, code) = run_autumn(temp.path(), &["sbom", "--binary", bin.to_str().unwrap()]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("cargo-auditable"),
        "the error must name the fix, got: {stderr}"
    );
}
