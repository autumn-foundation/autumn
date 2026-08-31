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

    // Belt and braces: nothing else may curl a file into an executable path
    // without a following checksum verification.
    for line in dockerfile.lines() {
        let l = line.trim();
        assert!(
            !(l.contains("curl") && l.contains("chmod +x")),
            "unverified download piped into an executable: {l}"
        );
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

    assert!(
        dockerfile.contains("sha256sums.txt") && dockerfile.contains("sha256sum -c"),
        "the generated Dockerfile must verify the Tailwind download's SHA-256, \
         got:\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("tailwindcss-linux-arm64") && dockerfile.contains("uname -m"),
        "the generated Dockerfile must pick the asset for the build \
         architecture — hardcoding x86 silently installs an unrunnable binary \
         on arm64. Got:\n{dockerfile}"
    );
}

/// AC3: the shipped binary must be able to report its own crate versions with
/// no source tree. `cargo-auditable` embeds that list; `RUSTC_WORKSPACE_WRAPPER`
/// applies it to both build paths the template renders (`cargo build --release`
/// and `autumn build --embed`) without duplicating the build step.
#[test]
fn scaffold_dockerfile_builds_an_auditable_binary() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();

    assert!(
        dockerfile.contains("cargo install --locked cargo-auditable"),
        "the builder stage must install cargo-auditable, got:\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("RUSTC_WORKSPACE_WRAPPER=cargo-auditable"),
        "the builder stage must route the compile through cargo-auditable, \
         got:\n{dockerfile}"
    );
}

/// AC4: the image itself carries a machine-readable SBOM at a predictable path.
#[test]
fn scaffold_dockerfile_bakes_an_sbom_into_the_image() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();

    assert!(
        dockerfile.contains("autumn sbom --output"),
        "the builder stage must generate a CycloneDX SBOM, got:\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("/usr/share/autumn/sbom.cdx.json"),
        "the SBOM must land at the documented path /usr/share/autumn/sbom.cdx.json, \
         got:\n{dockerfile}"
    );
    // The runtime stage — not just the builder — has to carry it, or the
    // shipped image has no SBOM at all.
    let runtime = dockerfile
        .split_once("AS runtime")
        .expect("Dockerfile must have a runtime stage")
        .1;
    assert!(
        runtime.contains("sbom.cdx.json"),
        "the runtime stage must COPY the SBOM out of the builder, got:\n{runtime}"
    );
}

/// The SBOM is only discoverable if a scanner knows where to look. An OCI
/// label points at it without any autumn-specific knowledge.
#[test]
fn scaffold_dockerfile_labels_the_image_with_its_sbom_path() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();
    assert!(
        dockerfile.contains("org.opencontainers.image."),
        "the runtime image must carry OCI image labels, got:\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("io.autumn.sbom.path"),
        "the image must advertise its SBOM path as a label, got:\n{dockerfile}"
    );
}

/// The image build must not silently fall back to a non-auditable, SBOM-less
/// image when `autumn sbom` fails — that would defeat the whole default.
#[test]
fn scaffold_dockerfile_does_not_swallow_sbom_failures() {
    let temp = release_init(None);
    let dockerfile = fs::read_to_string(temp.path().join("Dockerfile")).unwrap();
    for line in dockerfile.lines() {
        let l = line.trim();
        if l.contains("autumn sbom") || l.contains("autumn setup") {
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

#[test]
fn scaffolded_deploy_workflows_attest_the_image_they_push() {
    for (target, workflow) in [
        ("aws-ecs", ".github/workflows/aws-deploy.yml"),
        ("gcp-cloud-run", ".github/workflows/gcp-deploy.yml"),
        ("azure-container-apps", ".github/workflows/azure-deploy.yml"),
    ] {
        let temp = release_init(Some(target));
        let yaml = fs::read_to_string(temp.path().join(workflow))
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

#[test]
fn scaffolded_deploy_workflows_attest_the_image_sbom() {
    for (target, workflow) in [
        ("aws-ecs", ".github/workflows/aws-deploy.yml"),
        ("gcp-cloud-run", ".github/workflows/gcp-deploy.yml"),
        ("azure-container-apps", ".github/workflows/azure-deploy.yml"),
    ] {
        let temp = release_init(Some(target));
        let yaml = fs::read_to_string(temp.path().join(workflow)).unwrap();
        assert!(
            yaml.contains("actions/attest-sbom"),
            "{target}: the SBOM baked into the image must itself be attested:\n{yaml}"
        );
    }
}

// ===========================================================================
// AC1 + AC2 — autumn's own release train
// ===========================================================================

#[test]
fn publish_gate_runs_the_sbom_gate() {
    let gate = read_repo_file(".github/workflows/publish-gate.yml");
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
        "prepare-release must produce the SBOM release artifact:\n{prepare}"
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
    let script = fs::read_to_string(&path).unwrap();
    // The gate is only meaningful if it regenerates and compares, and if it
    // ties the SBOM's identity to the tag being released.
    assert!(
        script.contains("--verify"),
        "the gate must re-verify the SBOM against the source tree:\n{script}"
    );
    assert!(
        script.contains("RELEASE_TAG"),
        "the gate must check the SBOM against the tagged version:\n{script}"
    );
}

#[test]
fn release_workflow_attaches_the_sbom_and_attests_every_asset() {
    let release = read_repo_file(".github/workflows/release.yml");
    assert!(
        release.contains("sbom.cdx.json"),
        "release.yml must attach the SBOM as a release asset:\n{release}"
    );
    assert!(
        release.contains("actions/attest-build-provenance"),
        "release.yml must attest the assets it uploads:\n{release}"
    );
    assert!(
        release.contains("attestations: write") && release.contains("id-token: write"),
        "release.yml needs `attestations: write` + `id-token: write`:\n{release}"
    );
}

#[test]
fn cli_release_workflow_attests_every_binary_archive() {
    let cli_release = read_repo_file(".github/workflows/cli-release.yml");
    assert!(
        cli_release.contains("actions/attest-build-provenance"),
        "cli-release.yml must attest each per-target archive:\n{cli_release}"
    );
    assert!(
        cli_release.contains("attestations: write") && cli_release.contains("id-token: write"),
        "cli-release.yml needs `attestations: write` + `id-token: write`:\n{cli_release}"
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

#[test]
fn sbom_is_reproducible_across_runs() {
    let temp = tiny_crate();
    let (first, _, _) = run_autumn(temp.path(), &["sbom"]);
    let (second, _, _) = run_autumn(temp.path(), &["sbom"]);
    assert_eq!(first, second, "autumn sbom output must be reproducible");
    assert!(!first.is_empty());
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
