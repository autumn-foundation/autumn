use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GENERATED_EXAMPLE_CSS: &[&str] = &[
    "examples/blog/static/css/autumn.css",
    "examples/bookmarks/static/css/autumn.css",
    "examples/bookmarks-distributed/static/css/autumn.css",
    "examples/reddit-clone/static/css/autumn.css",
    "examples/todo-app/static/css/autumn.css",
    "examples/wiki/static/css/autumn.css",
];

const FIRST_RUN_DOCS: &[&str] = &[
    "README.md",
    "docs/guide/getting-started.md",
    "docs/guide/docs-smoke.md",
    "docs/guide/deployment.md",
    "docs/guide/websockets.md",
    "docs/guide/tutorial/01-project-setup.md",
    "docs/guide/tutorial/12-whats-next.md",
    "docs/guide/macro-transparency.md",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("autumn-cli should live under the workspace root")
        .to_path_buf()
}

fn normalize_hygiene_doc(content: &str) -> String {
    content.replace("\r\n", "\n").replace("//! ", "")
}

fn workspace_package_value(root_toml: &toml::Value, key: &str) -> String {
    root_toml
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get(key))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("workspace.package.{key} should be set"))
        .to_owned()
}

fn read_workspace_manifest(root: &Path) -> toml::Value {
    let root_manifest_path = root.join("Cargo.toml");
    let root_manifest = std::fs::read_to_string(&root_manifest_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", root_manifest_path.display()));
    toml::from_str(&root_manifest).expect("workspace Cargo.toml should parse as TOML")
}

fn parse_semver_triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The version the first-run docs pin is the latest *published* autumn-cli
/// release, which can lag the (unreleased) workspace version between releases
/// (PR #1622). The README quickstart is the source of truth for that pin —
/// the same convention `scripts/check-quickstart.sh` uses — so parse it from
/// there and hold every other first-run doc to it.
fn published_cli_version(readme: &str) -> String {
    readme
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("cargo install autumn-cli --version ")
        })
        .and_then(|rest| rest.split_whitespace().next())
        .map_or_else(
            || {
                panic!(
                    "README.md must pin the published CLI install command \
                     `cargo install autumn-cli --version <x.y.z>`"
                )
            },
            str::to_owned,
        )
}

fn read_docs_once(root: &Path) -> Vec<(&'static str, String)> {
    FIRST_RUN_DOCS
        .iter()
        .map(|doc| {
            let path = root.join(doc);
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            (*doc, content)
        })
        .collect()
}

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("failed to run git")
}

/// Every `*.sh` under `dir`, recursively.
fn shell_scripts(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
    {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            shell_scripts(&path, found);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("sh"))
        {
            found.push(path);
        }
    }
}

fn bash_command() -> Command {
    #[cfg(windows)]
    {
        for candidate in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
        ] {
            if Path::new(candidate).is_file() {
                return Command::new(candidate);
            }
        }
    }
    Command::new("bash")
}

fn member_manifest_forbids_unsafe_code(manifest_toml: &toml::Value) -> bool {
    let inherits_workspace_lints = manifest_toml
        .get("lints")
        .and_then(|lints| lints.get("workspace"))
        .and_then(toml::Value::as_bool)
        == Some(true);
    let local_unsafe_code_lint = manifest_toml
        .get("lints")
        .and_then(|lints| lints.get("rust"))
        .and_then(|rust| rust.get("unsafe_code"))
        .and_then(toml::Value::as_str);

    local_unsafe_code_lint.map_or(inherits_workspace_lints, |level| level == "forbid")
}

#[test]
fn workspace_test_profile_keeps_ci_artifacts_bounded() {
    let root = workspace_root();
    let root_toml = read_workspace_manifest(&root);
    let test_profile = root_toml
        .get("profile")
        .and_then(|profile| profile.get("test"))
        .unwrap_or_else(|| {
            panic!(
                "workspace Cargo.toml must set [profile.test] so `cargo test --workspace` \
                 does not fill CI disks with full debug artifacts"
            )
        });

    assert_eq!(
        test_profile.get("debug").and_then(toml::Value::as_str),
        Some("line-tables-only"),
        "[profile.test] should keep only line-table debug info for useful panic locations \
         without full debug artifact bloat",
    );
    assert_eq!(
        test_profile
            .get("incremental")
            .and_then(toml::Value::as_bool),
        Some(false),
        "[profile.test] should disable incremental caches in CI-sized test builds",
    );

    let build_override = test_profile
        .get("build-override")
        .unwrap_or_else(|| panic!("[profile.test.build-override] should be set"));
    assert_eq!(
        build_override
            .get("debug")
            .and_then(toml::Value::as_integer),
        Some(0),
        "[profile.test.build-override] should avoid debug info for build scripts and proc macros",
    );
}

#[test]
fn first_run_docs_match_current_release_line() {
    let root = workspace_root();
    let root_toml = read_workspace_manifest(&root);
    let workspace_version = workspace_package_value(&root_toml, "version");
    let rust_version = workspace_package_value(&root_toml, "rust-version");
    let docs = read_docs_once(&root);

    // The first-run docs pin the latest *published* release, parsed from the
    // README quickstart. Validate the pin so this test still catches a
    // malformed or future-dated pin: it must be a plain x.y.z semver and must
    // not be newer than the (possibly unreleased) workspace version.
    let readme = docs
        .iter()
        .find(|(doc, _)| *doc == "README.md")
        .map(|(_, content)| content)
        .expect("README.md should be included in FIRST_RUN_DOCS");
    let published_version = published_cli_version(readme);
    let published_triple = parse_semver_triple(&published_version).unwrap_or_else(|| {
        panic!(
            "README.md pins a malformed autumn-cli version `{published_version}`; expected x.y.z"
        )
    });
    let workspace_triple = parse_semver_triple(&workspace_version).unwrap_or_else(|| {
        panic!("workspace.package.version `{workspace_version}` should be x.y.z")
    });
    assert!(
        published_triple <= workspace_triple,
        "README.md pins autumn-cli {published_version}, which is newer than the workspace \
         version {workspace_version}; the quickstart must pin the latest published release",
    );

    let published_series = published_version
        .rsplit_once('.')
        .map_or(published_version.as_str(), |(series, _)| series);
    let published_health_json =
        format!(r#"{{ "status": "ok", "version": "{published_version}" }}"#);

    for (doc, content) in &docs {
        for stale in [
            "Rust 1.85",
            "Rust 1.86",
            "rustc 1.85",
            "rustc 1.86",
            "rust:1.86",
            "autumn-web = \"0.1.0\"",
            "version=\"0.1.0\"",
            "\"version\": \"0.1.0\"",
            "v0.1.0",
            "crates.io publication is not yet available",
        ] {
            assert!(
                !content.contains(stale),
                "{doc} still references stale first-run release/MSRV text: {stale}"
            );
        }

        if content.contains("cargo install --path autumn-cli") {
            assert!(
                content
                    .to_ascii_lowercase()
                    .contains("local development only"),
                "{doc} uses `cargo install --path autumn-cli` without clearly marking it as local development only",
            );
        }

        if content.contains("Rust ") {
            assert!(
                content.contains(&format!("Rust {rust_version}+")),
                "{doc} must state the workspace MSRV Rust {rust_version}+"
            );
        }

        if content.contains("cargo install autumn-cli") {
            assert!(
                content.contains(&format!(
                    "cargo install autumn-cli --version {published_version}"
                )),
                "{doc} must show the published CLI install command for autumn-cli {published_version}"
            );
        }

        if content.contains("autumn-web =") {
            assert!(
                content.contains(&format!("autumn-web = \"{published_series}\""))
                    || content.contains(&format!("autumn-web = \"{published_version}\""))
                    || content.contains(&format!("version = \"{published_series}\""))
                    || content.contains(&format!("version = \"{published_version}\"")),
                "{doc} must show the published autumn-web release line ({published_series} or {published_version})",
            );
        }

        if content.contains(r#""status": "ok""#) {
            assert!(
                content.contains(&published_health_json),
                "{doc} must show the published JSON health version {published_version}"
            );
        }
    }

    let docs_smoke = docs
        .iter()
        .find(|(doc, _)| *doc == "docs/guide/docs-smoke.md")
        .map(|(_, content)| content)
        .expect("docs smoke guide should be included in FIRST_RUN_DOCS");
    assert!(
        docs_smoke.contains("`/` returns `Welcome to smoke-app!`"),
        "docs-smoke must expect the root page generated by `autumn new smoke-app`"
    );
    assert!(
        docs_smoke.contains(&published_health_json),
        "docs-smoke must show the exact health JSON with version {published_version}"
    );
}

#[test]
fn published_cli_version_parses_readme_quickstart_pin() {
    let readme = "## Quickstart\n\n```bash\n# Install the published CLI\ncargo install autumn-cli --version 0.5.0\n```\n";
    assert_eq!(published_cli_version(readme), "0.5.0");
}

#[test]
fn semver_triple_parser_rejects_malformed_pins() {
    assert_eq!(parse_semver_triple("0.5.0"), Some((0, 5, 0)));
    assert_eq!(parse_semver_triple("0.5"), None);
    assert_eq!(parse_semver_triple("0.5.0.1"), None);
    assert_eq!(parse_semver_triple("0.5.x"), None);
}

#[test]
fn release_checklist_includes_docs_smoke_gate() {
    let root = workspace_root();
    let checklist_path = root.join("docs/release-checklist.md");
    let checklist = std::fs::read_to_string(&checklist_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", checklist_path.display()));

    for required in [
        "docs-smoke",
        "docs/guide/docs-smoke.md",
        "autumn-web",
        "autumn-cli",
        "release blocker",
    ] {
        assert!(
            checklist.contains(required),
            "release checklist must include `{required}` as part of the first-run docs smoke gate",
        );
    }
}

#[test]
fn workspace_forbids_unsafe_code_for_all_members() {
    let root = workspace_root();
    let root_toml = read_workspace_manifest(&root);

    let unsafe_code_lint = root_toml
        .get("workspace")
        .and_then(|workspace| workspace.get("lints"))
        .and_then(|lints| lints.get("rust"))
        .and_then(|rust| rust.get("unsafe_code"))
        .and_then(toml::Value::as_str);
    assert_eq!(
        unsafe_code_lint,
        Some("forbid"),
        "workspace root must set [workspace.lints.rust] unsafe_code = \"forbid\"",
    );

    let members = root_toml
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .expect("workspace.members should be an array");

    for member in members {
        let member = member
            .as_str()
            .expect("workspace member entries should be strings");
        let manifest_path = root.join(member).join("Cargo.toml");
        let manifest = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));
        let manifest_toml: toml::Value = toml::from_str(&manifest).unwrap_or_else(|err| {
            panic!("{} should parse as TOML: {err}", manifest_path.display())
        });

        assert!(
            member_manifest_forbids_unsafe_code(&manifest_toml),
            "{member}/Cargo.toml must either inherit workspace lints or set \
             [lints.rust] unsafe_code = \"forbid\"",
        );
    }
}

#[test]
fn member_manifest_forbid_check_rejects_local_override_of_workspace_lints() {
    let manifest_toml: toml::Value = toml::from_str(
        r#"
        [lints]
        workspace = true

        [lints.rust]
        unsafe_code = "warn"
        "#,
    )
    .expect("manifest snippet should parse");

    assert!(!member_manifest_forbids_unsafe_code(&manifest_toml));
}

#[test]
fn member_manifest_forbid_check_allows_workspace_lints_without_override() {
    let manifest_toml: toml::Value = toml::from_str(
        r"
        [lints]
        workspace = true
        ",
    )
    .expect("manifest snippet should parse");

    assert!(member_manifest_forbids_unsafe_code(&manifest_toml));
}

#[test]
fn generated_example_css_is_ignored_and_untracked() {
    let root = workspace_root();

    let tracked = git(
        &root,
        &["ls-files", "--", "examples/*/static/css/autumn.css"],
    );
    assert!(
        tracked.status.success(),
        "git ls-files failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&tracked.stdout),
        String::from_utf8_lossy(&tracked.stderr),
    );
    let tracked_stdout = String::from_utf8_lossy(&tracked.stdout);
    assert!(
        tracked_stdout.trim().is_empty(),
        "generated example CSS must not be tracked:\n{tracked_stdout}",
    );

    let mut ignore_args = vec!["check-ignore", "--no-index", "--"];
    ignore_args.extend_from_slice(GENERATED_EXAMPLE_CSS);
    let ignored = git(&root, &ignore_args);
    assert!(
        ignored.status.success(),
        "generated example CSS must be ignored:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ignored.stdout),
        String::from_utf8_lossy(&ignored.stderr),
    );

    let ignored_stdout = String::from_utf8_lossy(&ignored.stdout);
    for generated_path in GENERATED_EXAMPLE_CSS {
        assert!(
            ignored_stdout.lines().any(|line| line == *generated_path),
            "ignore rules did not match {generated_path}; matched:\n{ignored_stdout}",
        );
    }
}

#[test]
fn publish_dry_run_script_uses_list_not_no_verify() {
    let root = workspace_root();
    let script_path = root.join("scripts/check-publish-dry-run.sh");
    let script = std::fs::read_to_string(&script_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", script_path.display()));

    // --list enumerates the files that would be in the archive and validates
    // the manifest without touching the registry.  --no-verify is intentionally
    // avoided because it rewrites workspace path deps to their pinned registry
    // versions and resolves them against crates.io, which causes false failures
    // for plugin crates that depend on autumn-web features not yet published.
    assert!(
        script.contains(r#"cargo package -p "$crate" --list --allow-dirty"#),
        "{} must use `cargo package --list` for manifest/file verification",
        script_path.display(),
    );
    assert!(
        !script.contains(r#"cargo package -p "$crate" --no-verify --allow-dirty"#),
        "{} must not use `--no-verify`; that triggers registry resolution and causes false failures for plugin crates",
        script_path.display(),
    );
}

#[test]
fn shell_release_scripts_are_lf_normalized() {
    let root = workspace_root();
    let attributes_path = root.join(".gitattributes");
    let attributes = std::fs::read_to_string(&attributes_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", attributes_path.display()));
    assert!(
        attributes.contains("*.sh text eol=lf"),
        ".gitattributes must force LF checkout for shell scripts so release gates run under bash"
    );

    let scripts_dir = root.join("scripts");
    for entry in std::fs::read_dir(&scripts_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", scripts_dir.display()))
    {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read script entry: {err}"));
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sh") {
            continue;
        }

        let bytes = std::fs::read(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        assert!(
            !bytes.windows(2).any(|window| window == b"\r\n"),
            "{} must use LF line endings; CRLF makes bash treat options and blank lines as containing carriage returns",
            path.display()
        );
    }
}

#[test]
fn semver_script_installs_tool_without_lto_hotspot() {
    let root = workspace_root();
    let script_path = root.join("scripts/check-semver.sh");
    let script = std::fs::read_to_string(&script_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", script_path.display()));
    let release_lto_assignment = [
        "CARGO_PROFILE_RELEASE_LTO=\"",
        "$",
        "{",
        "CARGO_PROFILE_RELEASE_LTO:-false",
        "}\"",
    ]
    .concat();
    let release_codegen_units_assignment = [
        "CARGO_PROFILE_RELEASE_CODEGEN_UNITS=\"",
        "$",
        "{",
        "CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16",
        "}\"",
    ]
    .concat();

    assert!(
        script.contains(&release_lto_assignment),
        "{} must disable release LTO only for auto-installing cargo-semver-checks; Windows rustc has crashed in that installer profile",
        script_path.display(),
    );
    assert!(
        script.contains(&release_codegen_units_assignment),
        "{} must avoid codegen-units=1 only for auto-installing cargo-semver-checks",
        script_path.display(),
    );
    assert!(
        script.contains("cargo install cargo-semver-checks --locked"),
        "{} must still install cargo-semver-checks when the tool is missing",
        script_path.display(),
    );
}

#[test]
fn semver_script_checks_optional_features_with_pinned_rustdoc_toolchain() {
    let root = workspace_root();
    let script_path = root.join("scripts/check-semver.sh");
    let script = std::fs::read_to_string(&script_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", script_path.display()));

    assert!(
        script.contains(r#"semver_toolchain="${AUTUMN_SEMVER_RUST_VERSION:-1.94.1}""#),
        "{} must pin the semver rustdoc toolchain to Rust 1.94.1 by default",
        script_path.display(),
    );
    assert!(
        script.contains(r#"SEMVER_CARGO=(rustup run "$semver_toolchain" cargo)"#),
        "{} must run cargo-semver-checks through the pinned semver toolchain",
        script_path.display(),
    );
    assert!(
        !script.contains("--default-features"),
        "{} must not narrow SemVer coverage to default features only",
        script_path.display(),
    );
    assert!(
        !script.contains("--all-features"),
        "{} should use cargo-semver-checks' default feature heuristic, not force every internal/test feature",
        script_path.display(),
    );

    let workflow_path = root.join(".github/workflows/publish-gate.yml");
    let workflow = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", workflow_path.display()));
    let semver_job = workflow
        .split("  semver:")
        .nth(1)
        .and_then(|job| job.split("\n  # ---").next())
        .unwrap_or_else(|| panic!("{} must define a semver job", workflow_path.display()));
    assert!(
        semver_job.contains("dtolnay/rust-toolchain@1.94.1"),
        "{} semver job must install the pinned Rust 1.94.1 toolchain",
        workflow_path.display(),
    );
    assert!(
        !semver_job.contains("dtolnay/rust-toolchain@stable"),
        "{} semver job must not follow latest stable rustdoc JSON",
        workflow_path.display(),
    );
}

#[test]
fn webauthn_docs_explain_native_openssl_vcpkg_prerequisite() {
    let root = workspace_root();
    let guide_path = root.join("docs/guide/generators.md");
    let guide = std::fs::read_to_string(&guide_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", guide_path.display()));

    for required in ["WebAuthn", "OpenSSL", "vcpkg", "VCPKG_ROOT"] {
        assert!(
            guide.contains(required),
            "{} must document the WebAuthn/OpenSSL native dependency prerequisite `{required}`",
            guide_path.display(),
        );
    }
}

#[test]
fn publish_gate_prepare_release_does_not_mutate_changelog() {
    let root = workspace_root();
    let workflow_path = root.join(".github/workflows/publish-gate.yml");
    let workflow = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", workflow_path.display()));

    for forbidden in [
        "git-cliff --config cliff.toml --output CHANGELOG.md",
        "Commit CHANGELOG.md to trunk",
        "git commit -m \"docs: update CHANGELOG.md",
        "git push origin HEAD:trunk",
        "contents: write",
    ] {
        assert!(
            !workflow.contains(forbidden),
            "publish-gate must not mutate CHANGELOG.md from a detached tag checkout; found `{forbidden}`",
        );
    }
}

#[test]
fn release_notes_script_detects_breaking_section_with_long_changelog_entry() {
    let root = workspace_root();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let scripts_dir = tmp.path().join("scripts");
    let migrations_dir = tmp.path().join("docs/migrations");
    std::fs::create_dir_all(&scripts_dir).expect("scripts dir");
    std::fs::create_dir_all(&migrations_dir).expect("migrations dir");
    for script in ["check-release-notes.sh", "check-migration-guides.sh"] {
        // check-release-notes.sh delegates breaking-change detection to the
        // migration-guide gate, so the fixture needs both scripts.
        std::fs::copy(root.join("scripts").join(script), scripts_dir.join(script))
            .unwrap_or_else(|err| panic!("copy {script}: {err}"));
    }

    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.4.0\"\n",
    )
    .expect("workspace manifest");
    std::fs::write(migrations_dir.join("0.4.0.md"), "# Migrating to 0.4.0\n")
        .expect("migration guide");

    let mut changelog = String::from(
        "# Changelog\n\n\
         ## [0.4.0] - 2026-05-11\n\n\
         ### Breaking Changes\n\n\
         - A deliberate break acknowledged by the migration guide.\n\n\
         ### Added\n",
    );
    for i in 0..200_000 {
        writeln!(changelog, "- filler line {i}").expect("write filler line");
    }
    changelog.push_str("\n## [0.3.0] - 2026-04-01\n\n- Previous release.\n");
    std::fs::write(tmp.path().join("CHANGELOG.md"), changelog).expect("changelog");

    let output = bash_command()
        .arg("scripts/check-release-notes.sh")
        .current_dir(tmp.path())
        .output()
        .expect("run release-notes check");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "release-notes check failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("migration guide exists: docs/migrations/0.4.0.md"),
        "breaking release should be acknowledged by the migration guide:\nstdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("non-breaking release"),
        "breaking release was misclassified as non-breaking:\nstdout:\n{stdout}"
    );
}

#[test]
fn bookmarks_example_tracks_regenerated_scaffold_layout() {
    let root = workspace_root();
    let bookmarks = root.join("examples/bookmarks");

    for generated_path in [
        "src/models/bookmark.rs",
        "src/models/mod.rs",
        "src/repositories/bookmark.rs",
        "src/repositories/mod.rs",
        "src/routes/bookmarks.rs",
        "src/routes/mod.rs",
        "tests/bookmark.rs",
    ] {
        assert!(
            bookmarks.join(generated_path).is_file(),
            "issue #534 expects examples/bookmarks to keep the generated scaffold file: {generated_path}",
        );
    }

    for replaced_path in ["src/models.rs", "src/repositories.rs"] {
        assert!(
            !bookmarks.join(replaced_path).exists(),
            "issue #534 expects the old flat bookmarks source file to be replaced: {replaced_path}",
        );
    }
}

#[test]
fn after_commit_docs_do_not_promise_crash_safe_delivery() {
    let root = workspace_root();
    let transactions_path = root.join("docs/guide/transactions.md");
    let transactions = std::fs::read_to_string(&transactions_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", transactions_path.display()));

    assert!(
        transactions.contains("not a crash-safe delivery mechanism"),
        "{} must explicitly warn that process-local after_commit callbacks can be lost after commit",
        transactions_path.display(),
    );
    assert!(
        transactions.contains("durable outbox"),
        "{} must point crash-safe side effects at an in-transaction outbox or queue",
        transactions_path.display(),
    );
    assert!(
        !transactions.contains("Autumn eliminates this race with `after_commit` callbacks"),
        "{} must not claim after_commit eliminates the DB-commit/process-crash race",
        transactions_path.display(),
    );
}

#[test]
fn version_history_docs_put_sensitive_attribute_on_repository_trait() {
    let root = workspace_root();
    for rel_path in [
        "docs/guide/version-history.md",
        "autumn/src/version_history.rs",
    ] {
        let path = root.join(rel_path);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let content = normalize_hygiene_doc(&content);
        let attr = "#[version_history(sensitive = [\"password_digest\", \"reset_token\"])]";
        let repo = "#[repository(Post, versioned = true)]";

        assert!(
            content.contains(&format!("{attr}\n{repo}")),
            "{rel_path} must put #[version_history(...)] on the repository trait, not inside its empty body",
        );
        assert!(
            !content.contains(&format!("{repo}\npub trait PostRepository {{\n    {attr}")),
            "{rel_path} must not show #[version_history(...)] as an item inside the trait body",
        );
    }
}

#[test]
fn hygiene_doc_normalization_accepts_windows_line_endings() {
    let attr = "#[version_history(sensitive = [\"password_digest\", \"reset_token\"])]";
    let repo = "#[repository(Post, versioned = true)]";
    let content = format!("{attr}\r\n{repo}\r\npub trait PostRepository {{}}\r\n");
    let content = normalize_hygiene_doc(&content);

    assert!(content.contains(&format!("{attr}\n{repo}")));
}

#[test]
fn version_history_migration_has_tenant_scope_column_and_index() {
    let root = workspace_root();
    let migration_path =
        root.join("autumn/migrations/20260526000000_create_version_history/up.sql");
    let migration = std::fs::read_to_string(&migration_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", migration_path.display()));

    assert!(
        migration.contains("tenant_id   TEXT"),
        "{} must store tenant_id so tenant-scoped history reads can fail closed",
        migration_path.display(),
    );
    assert!(
        migration.contains("(table_name, tenant_id, record_id, recorded_at ASC)"),
        "{} must index tenant-scoped history lookups",
        migration_path.display(),
    );
}

// ── Generator conformance CI gate (issue #1017) ───────────────────────────────

#[test]
fn generator_conformance_ci_gate_is_configured() {
    let root = workspace_root();

    // AC-1 / AC-2: A dedicated workflow file must exist that runs the ignored
    // generator conformance tests (compiled compile/serve gates) — NOT just
    // `cargo test --workspace` which skips all `#[ignore]`d tests.
    let workflow_path = root.join(".github/workflows/generator-conformance.yml");
    let workflow = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", workflow_path.display()));

    // The three non-Postgres gates must be invoked explicitly via --ignored.
    for test_name in [
        "generated_project_compiles_runs_and_serves",
        "generated_scaffold_cargo_checks",
        "generated_scaffold_config_cargo_checks",
    ] {
        assert!(
            workflow.contains(test_name),
            "generator-conformance.yml must invoke `{test_name}` via --ignored; \
             these tests are CI-gated, not abandoned — see CONTRIBUTING.md",
        );
    }

    // The Postgres-dependent gate must also be present (AC-2), alongside the two
    // issue #1388 gates: the live-HTTP one is the only RUNTIME proof of that
    // feature's headline acceptance criterion, and `constrained_scaffold_cargo_checks`
    // the only proof that the full constraint mix (including `{url}` and a
    // nullable bound) COMPILES. Every other test for the scaffold DSL's `{…}`
    // modifiers string-matches generated source, so if these are dropped from
    // the workflow the feature stops being verified anywhere.
    //
    // Matched as the full `<name> -- --ignored --exact` INVOCATION, not the bare
    // name: every gate is also mentioned in the job's header comment, so a bare
    // substring check would stay green after the `run:` step itself was deleted
    // — exactly the regression this pin exists to catch.
    //
    // Backslashes are dropped and all whitespace collapsed first, because the
    // longer invocations wrap across shell line continuations. Folding on the
    // literal "\\\n" would be WRONG: on a Windows checkout the workflow arrives
    // with CRLF endings, so the continuation is a backslash followed by "\\r\\n"
    // and the fold silently no-ops — which is exactly how this assertion first
    // failed on `Test (windows-latest)` while passing everywhere else.
    let invocations = workflow
        .replace('\\', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for test_name in [
        "generated_scaffold_serves_posts_index_and_json_api",
        "generated_constrained_scaffold_enforces_validation_end_to_end",
        "integration::scaffold_validation::constrained_scaffold_cargo_checks",
    ] {
        assert!(
            invocations.contains(&format!("{test_name} -- --ignored --exact")),
            "generator-conformance.yml must INVOKE `{test_name}` (not merely name it \
             in a comment); these gates are the only place issue #1388's scaffold \
             DSL constraints are compiled and exercised at runtime",
        );
    }

    // The auth/TOTP generator gate must also be included so that changes to
    // autumn-cli/src/generate/auth.rs are caught alongside scaffold changes.
    assert!(
        workflow.contains("generated_auth_totp_cargo_checks"),
        "generator-conformance.yml must run `generated_auth_totp_cargo_checks` so \
         auth generator changes are compile-verified alongside scaffold changes",
    );

    // AC-4: path filters must cover the generator template surface, the
    // entire autumn-web public API (autumn/src/**), and crate manifests so
    // that manifest-only dependency/feature changes also trigger the gate.
    for path_fragment in [
        "autumn-cli/src/generate",
        "autumn-cli/src/templates",
        "autumn-cli/src/new.rs",
        "autumn-cli/src/migrate.rs",
        "autumn-cli/Cargo.toml",
        "autumn/src/",
        "autumn/Cargo.toml",
        "autumn-macros",
    ] {
        assert!(
            workflow.contains(path_fragment),
            "generator-conformance.yml must declare a path filter covering `{path_fragment}`",
        );
    }

    // AC-4: a scheduled run catches regressions on branches where the path
    // filter would not trigger.
    assert!(
        workflow.contains("schedule"),
        "generator-conformance.yml must include a cron schedule so generator rot \
         is caught even when no template or prelude file was touched directly",
    );
}

// ── cli_tests per-test triage (issue #1945) ───────────────────────────────

#[test]
fn cli_tests_docker_ignored_tests_are_ci_swept() {
    let root = workspace_root();
    let ci_path = root.join(".github/workflows/ci.yml");
    let ci_yml = std::fs::read_to_string(&ci_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", ci_path.display()));

    // A newly-added `#[ignore = "requires Docker (testcontainers)"]` test in
    // ANY `autumn-cli/tests/integration/*.rs` module must run in CI with no
    // workflow edit, the same guarantee the autumn-web sweep already gives
    // (#1923). That only holds if ci.yml's cli_tests invocation is a BARE
    // `--ignored` sweep (discovers every ignored test in the binary) rather
    // than a specific-test filter — matched as the literal invocation tail,
    // not a substring of the surrounding comment, so deleting the sweep
    // itself would still fail this even though "cli_tests" stays mentioned
    // in prose above it.
    let invocation = ci_yml
        .replace('\\', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        invocation
            .contains("cargo test -p autumn-cli --test cli_tests -- --ignored --test-threads=1"),
        "ci.yml must run a BARE `--ignored` sweep over `cli_tests` so a newly-added \
         Docker `#[ignore]`d test runs automatically; see issue #1945",
    );
}

#[test]
fn cli_tests_cold_start_ignored_tests_are_ci_named() {
    let root = workspace_root();
    let generator_conformance_path = root.join(".github/workflows/generator-conformance.yml");
    let generator_conformance = std::fs::read_to_string(&generator_conformance_path)
        .unwrap_or_else(|err| {
            panic!(
                "failed to read {}: {err}",
                generator_conformance_path.display()
            )
        });

    // Unlike the Docker-gated tests above, these `#[ignore]`d tests each
    // scaffold and cargo-check/build/run a fresh project — too slow for the
    // fast Docker sweep, and ci.yml's cli_tests invocation explicitly
    // `--skip`s each of them by exact name for that reason. So each one must
    // be named explicitly in generator-conformance.yml (matching every other
    // generator-shaped gate above) or it never runs anywhere — the gap issue
    // #1945 flagged as the `cli_tests` binary's remaining per-test triage.
    //
    // Matched as the full `<name> -- --ignored --exact` INVOCATION, not the
    // bare name (same rationale and normalization as
    // `generator_conformance_ci_gate_is_configured` above): a bare substring
    // check would stay green after cargo lost its `--ignored --exact` flags,
    // gained a typo that selects zero tests, or the whole `run:` step was
    // deleted while the test name lingered in a comment.
    let invocations = generator_conformance
        .replace('\\', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for test_name in [
        "integration::api_scaffold::api_scaffold_cargo_checks",
        "integration::cloud_native_scaffold::scaffolded_app_passes_routes_audit_gate",
        "integration::cloud_native_scaffold::scaffolded_api_app_passes_routes_audit_gate",
        "integration::generate_position_scaffold::unscoped_position_generated_project_cargo_checks",
        "integration::generate_position_scaffold::scoped_position_generated_project_cargo_checks",
        "integration::generate_position_scaffold::soft_delete_position_generated_project_cargo_checks",
        "integration::scaffold_belongs_to::belongs_to_scaffold_cargo_checks",
        "integration::scaffold_bulk_delete::bulk_delete_generated_project_cargo_checks",
        "integration::scaffold_rich_text::richtext_scaffold_cargo_checks",
        "integration::scaffold_search::searchable_scaffold_cargo_checks",
        "integration::scaffold_trash::trash_generated_project_cargo_checks",
        "integration::seed_model_linking::linked_seed_binary_cargo_checks",
        "integration::serve::serve_daemon_start_status_stop_over_unix_socket",
        "integration::scaffold_form_for::generated_form_for_scaffold_cargo_checks",
        "integration::scaffold_form_for::generated_scaffold_with_missing_reference_target_cargo_checks",
    ] {
        assert!(
            invocations.contains(&format!("{test_name} -- --ignored --exact")),
            "generator-conformance.yml must INVOKE `{test_name}` (not merely name it \
             in a comment) via --ignored --exact; this cli_tests test is CI-gated, \
             not abandoned — see issue #1945",
        );
    }
}

/// Drop YAML comment text from a workflow file.
///
/// The coverage guard below matches `--test <name>` against workflow source, so
/// without this a target whose invocation was commented out — or merely
/// mentioned in prose — would satisfy it while no job runs the target, which is
/// the very hole the guard exists to close.
///
/// A `#` opens a comment when it starts the line or follows whitespace and is
/// not inside a quoted string; a `#` inside a shell word (`$#`) is left alone.
fn strip_yaml_comments(yaml: &str) -> String {
    let mut out = String::with_capacity(yaml.len());
    for line in yaml.lines() {
        let (mut in_single, mut in_double, mut after_ws) = (false, false, true);
        let mut end = line.len();
        for (idx, ch) in line.char_indices() {
            match ch {
                '\'' if !in_double => in_single = !in_single,
                '"' if !in_single => in_double = !in_double,
                '#' if !in_single && !in_double && after_ws => {
                    end = idx;
                    break;
                }
                _ => {}
            }
            after_ws = ch.is_whitespace();
        }
        out.push_str(line.get(..end).unwrap_or(line));
        out.push('\n');
    }
    out
}

/// Values a cargo command gives to `flag`, in both the `--flag value` and
/// `--flag=value` spellings. Values are whole tokens, so `--test sim_chaos_crash`
/// yields `sim_chaos_crash` and never satisfies a lookup for `sim_chaos`.
fn flag_values<'a>(tokens: &[&'a str], flag: &str) -> Vec<&'a str> {
    let eq = format!("{flag}=");
    let mut values: Vec<&str> = tokens
        .windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1])
        .collect();
    values.extend(tokens.iter().filter_map(|t| t.strip_prefix(eq.as_str())));
    values
}

/// Every `sqlite`-gated `[[test]]` target in `autumn/Cargo.toml` must be named
/// in a CI workflow (issue #1908).
///
/// These targets are `#![cfg(feature = "sqlite")]`, so the default
/// `cargo test --workspace` compiles each to an empty, passing binary. The
/// backend flip makes a bare `cargo test --features sqlite` unsafe, so the
/// sqlite job enumerates its targets BY NAME. A target added to `Cargo.toml` but
/// not to that list therefore never runs anywhere and fails silently forever —
/// which is how `sqlite_tracked_sessions` shipped dark. This closes the gap for
/// every future target.
///
/// Membership is read from each target's own `#![cfg(...)]` gate rather than a
/// name prefix, so a sqlite target named otherwise is still covered and a
/// backend-independent `sim_*` target is not wrongly demanded. Coverage means a
/// live cargo command that enables the `sqlite` feature AND names the target:
/// a commented-out line, a prose mention, a prefix of another target's name, or
/// a `--test` without the feature all leave the target dark and must fail here.
#[test]
fn sqlite_test_targets_are_ci_named() {
    let root = workspace_root();
    let manifest_path = root.join("autumn/Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));

    // Collect the cargo commands every workflow actually runs. `strip_yaml_comments`
    // drops commented-out invocations and prose mentions; joining `\`-continued
    // lines keeps one wrapped command as one command, so a target is credited only
    // to the invocation that names it.
    let workflows_dir = root.join(".github/workflows");
    let mut commands: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&workflows_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", workflows_dir.display()))
        .flatten()
    {
        let Ok(body) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let mut pending = String::new();
        for line in strip_yaml_comments(&body).lines() {
            let trimmed = line.trim_end();
            if let Some(head) = trimmed.strip_suffix('\\') {
                pending.push_str(head);
                pending.push(' ');
            } else {
                pending.push_str(trimmed);
                commands.push(std::mem::take(&mut pending));
            }
        }
        if !pending.is_empty() {
            commands.push(pending);
        }
    }

    // A command covers a target only when it BOTH enables the `sqlite` feature and
    // names the target. Without the feature the target's crate-level
    // `#![cfg(feature = "sqlite")]` compiles it to an empty binary that exits 0, so
    // a feature-less `--test <target>` is not coverage.
    let sqlite_commands: Vec<Vec<&str>> = commands
        .iter()
        .map(|command| command.split_whitespace().collect::<Vec<_>>())
        .filter(|tokens| {
            tokens.contains(&"cargo")
                && tokens.contains(&"test")
                && (tokens.contains(&"--all-features")
                    || flag_values(tokens, "--features").iter().any(|value| {
                        value
                            .trim_matches(['"', '\''])
                            .split(',')
                            .any(|feature| feature.trim() == "sqlite")
                    }))
        })
        .collect();

    let is_invoked = |target: &str| {
        sqlite_commands
            .iter()
            .any(|tokens| flag_values(tokens, "--test").contains(&target))
    };

    // Pair each `[[test]]` name with its path, then keep only the sqlite-gated
    // ones.
    let mut name: Option<&str> = None;
    let mut gated = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if let Some(value) = line
            .strip_prefix("name = \"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            name = Some(value);
        } else if let Some(path) = line
            .strip_prefix("path = \"")
            .and_then(|rest| rest.strip_suffix('"'))
            && let Some(target) = name.take()
        {
            let source = std::fs::read_to_string(root.join("autumn").join(path))
                .unwrap_or_else(|err| panic!("failed to read {path}: {err}"));
            if source
                .lines()
                .take_while(|l| !l.starts_with("use ") && !l.starts_with("mod "))
                .any(|l| l.starts_with("#![cfg(") && l.contains(r#"feature = "sqlite""#))
            {
                gated.push(target.to_owned());
            }
        }
    }
    assert!(
        gated.len() > 20,
        "expected the sqlite-gated [[test]] targets to be discovered, found {gated:?}"
    );

    for target in gated {
        assert!(
            is_invoked(&target),
            "a CI workflow must run `--test {target}`; a sqlite-gated target missing from \
             the sqlite job's named list compiles to an empty binary and never runs — \
             see issue #1908",
        );
    }
}

#[test]
fn contributing_documents_ignored_generator_tests() {
    let root = workspace_root();

    // AC-6: CONTRIBUTING.md must explain that the generator conformance tests
    // carry #[ignore] annotations but are still machine-verified by CI, so
    // contributors do not assume these tests are abandoned.
    let contributing_path = root.join("CONTRIBUTING.md");
    let contributing = std::fs::read_to_string(&contributing_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", contributing_path.display()));

    assert!(
        contributing.contains("generator-conformance"),
        "CONTRIBUTING.md must mention the `generator-conformance` CI gate",
    );
    assert!(
        contributing.contains("#[ignore]"),
        "CONTRIBUTING.md must explain that `#[ignore]` on generator tests means \
         CI-gated, not abandoned",
    );
    assert!(
        contributing.contains("autumn-cli/src/generate")
            && contributing.contains("autumn-cli/src/templates"),
        "CONTRIBUTING.md must name the generator template paths that trigger the gate",
    );
}

#[test]
fn runtime_config_migration_sorts_after_existing_framework_versions() {
    let root = workspace_root();
    let migrations_dir = root.join("autumn/migrations");
    let mut runtime_config_version = None;
    let mut version_history_version = None;

    for entry in std::fs::read_dir(&migrations_dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", migrations_dir.display()))
    {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read migration entry: {err}"));
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some((version, name)) = file_name.split_once('_') else {
            continue;
        };

        match name {
            "create_runtime_config" => runtime_config_version = Some(version.to_owned()),
            "create_version_history" => version_history_version = Some(version.to_owned()),
            _ => {}
        }
    }

    let runtime_config_version =
        runtime_config_version.expect("runtime config framework migration must exist");
    let version_history_version =
        version_history_version.expect("version history framework migration must exist");

    assert!(
        runtime_config_version > version_history_version,
        "runtime config migration must sort after version history so new deployments roll back in release order"
    );
}

#[test]
fn benchmark_runtime_startup_applies_packaged_migrations() {
    let root = workspace_root();

    let spring_properties_path =
        root.join("benchmarks/runtime/spring-boot/src/main/resources/application.properties");
    let spring_properties = std::fs::read_to_string(&spring_properties_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", spring_properties_path.display()));
    assert!(
        spring_properties.contains("spring.flyway.enabled=true"),
        "{} must keep Flyway enabled so standalone fresh benchmark databases get the packaged schema",
        spring_properties_path.display(),
    );
    assert!(
        !spring_properties.contains("spring.flyway.enabled=false"),
        "{} must not disable Flyway for standalone benchmark runs",
        spring_properties_path.display(),
    );

    let django_dockerfile_path = root.join("benchmarks/runtime/django/Dockerfile");
    let django_dockerfile = std::fs::read_to_string(&django_dockerfile_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", django_dockerfile_path.display()));
    assert!(
        django_dockerfile.contains("python manage.py migrate --noinput &&")
            && django_dockerfile.contains("gunicorn benchapp.asgi:application"),
        "{} must run Django migrations before serving the benchmark",
        django_dockerfile_path.display(),
    );

    let autumn_main_path = root.join("benchmarks/runtime/autumn/src/main.rs");
    let autumn_main = std::fs::read_to_string(&autumn_main_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", autumn_main_path.display()));
    assert!(
        autumn_main.contains("embed_migrations!()")
            && autumn_main.contains(".migrations(MIGRATIONS)"),
        "{} must register benchmark migrations before running Autumn",
        autumn_main_path.display(),
    );

    let rails_dockerfile_path = root.join("benchmarks/runtime/rails/Dockerfile");
    let rails_dockerfile = std::fs::read_to_string(&rails_dockerfile_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", rails_dockerfile_path.display()));
    assert!(
        rails_dockerfile.contains("bundle exec rails db:migrate && bundle exec puma"),
        "{} must run Rails migrations before starting Puma",
        rails_dockerfile_path.display(),
    );

    let loco_production_path = root.join("benchmarks/runtime/loco/config/production.yaml");
    let loco_production = std::fs::read_to_string(&loco_production_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", loco_production_path.display()));
    assert!(
        loco_production.contains("auto_migrate: true"),
        "{} must keep Loco auto-migration enabled for fresh benchmark databases",
        loco_production_path.display(),
    );
    assert!(
        !loco_production.contains("auto_migrate: false"),
        "{} must not disable Loco auto-migration for standalone benchmark runs",
        loco_production_path.display(),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Issue #978 — build-and-boot the generated release image in CI so the deploy
// story can't rot. The generated Dockerfile and the "10-minute deploy" promise
// in docs/guide/deployment.md must be enforced by a gate that actually builds
// and boots the image, not just by file-shape string assertions.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn release_image_boot_gate_is_configured() {
    let root = workspace_root();

    // AC: a dedicated workflow file must exist that builds and boots the
    // generated release image — not just `cargo test` shape assertions.
    let workflow_path = root.join(".github/workflows/release-image-boot.yml");
    let workflow = std::fs::read_to_string(&workflow_path).unwrap_or_else(|err| {
        panic!(
            "failed to read {}: {err}\n\
             issue #978 requires a build-and-boot gate for the generated release image",
            workflow_path.display()
        )
    });

    // The heavy lifting (docker build, boot, probe) lives in a reusable shell
    // harness so the workflow stays thin and the gate is runnable locally.
    let harness_path = root.join("scripts/check-release-image-boot.sh");
    let harness = std::fs::read_to_string(&harness_path).unwrap_or_else(|err| {
        panic!(
            "failed to read {}: {err}\n\
             issue #978 requires a build-and-boot harness script invoked by the gate",
            harness_path.display()
        )
    });

    // AC: the gate is path-scoped to the deployment scaffold surface so it
    // protects the artifacts without taxing every unrelated PR.
    for path_fragment in [
        "autumn-cli/src/release.rs",
        "autumn-cli/src/new.rs",
        "autumn-cli/src/templates",
        "scripts/check-release-image-boot.sh",
        ".github/workflows/release-image-boot.yml",
    ] {
        assert!(
            workflow.contains(path_fragment),
            "release-image-boot.yml must declare a path filter covering `{path_fragment}` \
             so changes to the deploy scaffold trigger the gate",
        );
    }

    // AC: a scheduled run protects branches where the path filter would not
    // fire (e.g. a base-image bump reaching crates.io transitively).
    assert!(
        workflow.contains("schedule"),
        "release-image-boot.yml must include a cron schedule so the deploy path \
         is verified even when no scaffold file was touched directly",
    );

    // The workflow must run the harness for the bare target and the
    // docker-compose target (AC: both covered).
    assert!(
        workflow.contains("check-release-image-boot.sh"),
        "release-image-boot.yml must invoke the build-and-boot harness script",
    );
    assert!(
        workflow.contains("docker-compose"),
        "release-image-boot.yml must exercise the --target docker-compose variant",
    );

    // AC: a throwaway Postgres (service container) backs the bare target.
    assert!(
        workflow.contains("postgres"),
        "release-image-boot.yml must provision a throwaway Postgres for the boot test",
    );

    // ── Harness behaviour (the actual build-and-boot contract) ──────────────

    // AC: scaffolds a fresh project then runs `autumn release init --force`.
    // Assert against the actual command invocation, not just a log/comment string:
    // the harness runs `"${AUTUMN}" new "${PROJECT_NAME}"` which contains the
    // literal substring `"${AUTUMN}" new "` in the script source.
    assert!(
        harness.contains("\"${AUTUMN}\" new \""),
        "harness must scaffold a fresh project via `autumn new`",
    );
    assert!(
        harness.contains("release") && harness.contains("init") && harness.contains("--force"),
        "harness must run `autumn release init --force`",
    );

    // AC: docker build the generated image.
    assert!(
        harness.contains("docker build"),
        "harness must `docker build` the generated image",
    );

    // AC: exercise the one-shot migrate path before the web container is ready.
    assert!(
        harness.contains("autumn migrate"),
        "harness must run the one-shot `autumn migrate` before booting the web tier",
    );

    // AC: assert both /health and /actuator/health reach 200.
    assert!(
        harness.contains("/health"),
        "harness must probe GET /health",
    );
    assert!(
        harness.contains("/actuator/health"),
        "harness must probe GET /actuator/health",
    );

    // AC: bounded startup window (≤ 30s) — the budget must be encoded by name.
    assert!(
        harness.contains("STARTUP_BUDGET_SECS"),
        "harness must encode a bounded (≤ 30s) startup window for the health probe",
    );

    // AC: docker-compose path is brought up and torn down cleanly.
    assert!(
        harness.contains("docker compose") || harness.contains("docker-compose"),
        "harness must drive the docker-compose target",
    );
    assert!(
        harness.contains("down -v"),
        "harness must tear the compose stack down cleanly (`docker compose down -v`)",
    );

    // AC: on failure, surface build/boot logs and the failing probe response.
    assert!(
        harness.contains("docker logs") || harness.contains("compose logs"),
        "harness must dump container logs on failure for diagnosability",
    );

    // Secondary guard: final runtime image size budget (< 150 MB).
    assert!(
        harness.contains("150"),
        "harness must guard the runtime image size budget (< 150 MB)",
    );
}

#[test]
fn deployment_guide_references_build_and_boot_gate() {
    let root = workspace_root();

    // AC: docs/guide/deployment.md references this gate as the proof behind its
    // "10-minute" claim — the numeric promise must point at the machine check.
    let doc_path = root.join("docs/guide/deployment.md");
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", doc_path.display()));

    assert!(
        doc.contains("release-image-boot"),
        "deployment.md must reference the `release-image-boot` CI gate as the proof \
         behind the documented 10-minute deploy promise",
    );
}

// ---------------------------------------------------------------------------
// Migration guide coverage gate (issue #1588)
//
// Autumn ships every 2–4 weeks and, pre-1.0, most releases can break existing
// apps. `docs/migrations/` is the documented upgrade path, but nothing forced
// a guide to exist: the only automated check keyed off a `### Breaking`
// CHANGELOG heading this repo has never used, so it never fired. These tests
// pin the replacement gate — `scripts/check-migration-guides.sh` — and the
// backfilled guides it protects.
// ---------------------------------------------------------------------------

/// Guides that must exist for already-published releases (issue #1588 AC2).
/// Floored at 0.4.0: earlier releases are explicitly out of scope.
const BACKFILLED_MIGRATION_GUIDES: &[&str] = &["0.4.0", "0.5.0", "0.6.0"];

/// Build a self-contained repo skeleton the migration-guide gate can run
/// against, with `scripts/check-migration-guides.sh` copied in from the real
/// workspace. Returns the tempdir; the caller writes `CHANGELOG.md` and any
/// guides it needs.
fn migration_gate_fixture(workspace_version: &str) -> tempfile::TempDir {
    let root = workspace_root();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let scripts_dir = tmp.path().join("scripts");
    let migrations_dir = tmp.path().join("docs/migrations");
    std::fs::create_dir_all(&scripts_dir).expect("scripts dir");
    std::fs::create_dir_all(&migrations_dir).expect("migrations dir");
    std::fs::copy(
        root.join("scripts/check-migration-guides.sh"),
        scripts_dir.join("check-migration-guides.sh"),
    )
    .expect("copy migration-guide script");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        format!("[workspace.package]\nversion = \"{workspace_version}\"\n"),
    )
    .expect("workspace manifest");
    std::fs::write(
        migrations_dir.join("README.md"),
        "# Migration Guides\n\n## Index\n",
    )
    .expect("migrations index");
    // The gate reads its placeholder vocabulary from the real TEMPLATE.md, so
    // the fixture needs the actual file rather than a stand-in.
    std::fs::copy(
        root.join("docs/migrations/TEMPLATE.md"),
        migrations_dir.join("TEMPLATE.md"),
    )
    .expect("copy migration guide template");
    // Every real checkout has a rolling draft: the release checklist recreates
    // it after each release so the links to it keep resolving.
    write_fixture_guide(&tmp, "next", &template_draft());
    tmp
}

/// `TEMPLATE.md` with its banner removed — what the release checklist tells
/// the operator to recreate `next.md` from.
fn template_draft() -> String {
    let template = std::fs::read_to_string(workspace_root().join("docs/migrations/TEMPLATE.md"))
        .expect("read TEMPLATE.md");
    template
        .lines()
        .skip_while(|line| !line.starts_with("> ") && !line.starts_with('#'))
        .filter(|line| !line.starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A minimal guide that satisfies the gate's shape requirements, so shape
/// failures never masquerade as coverage failures in these tests.
fn valid_migration_guide(version: &str) -> String {
    format!(
        "# Migrating from Autumn `0.x` to `{version}`\n\n\
         ## At a glance\n\n\
         - **Old version:** `autumn-web 0.x`\n\
         - **New version:** `autumn-web {version}`\n\n\
         ## Summary\n\n\
         Why this release breaks.\n\n\
         ## Before you start\n\n\
         Pin the old version and get green.\n\n\
         ## Breaking changes\n\n\
         ### Area: the thing that broke\n\n\
         Before / after.\n\n\
         ## How to verify\n\n\
         Run `cargo check`.\n\n\
         ### Guide-only upgrade walkthrough\n\n\
         - **Status:** performed 2026-01-01\n"
    )
}

/// Register a guide in the fixture and index it, mirroring what the real
/// `docs/migrations/README.md` does.
fn write_fixture_guide(tmp: &tempfile::TempDir, version: &str, body: &str) {
    let migrations = tmp.path().join("docs/migrations");
    std::fs::write(migrations.join(format!("{version}.md")), body).expect("guide");
    let index_path = migrations.join("README.md");
    let mut index = std::fs::read_to_string(&index_path).expect("index");
    writeln!(index, "- [`{version}.md`]({version}.md)").expect("index entry");
    std::fs::write(&index_path, index).expect("write index");
}

fn run_migration_gate(dir: &Path) -> Output {
    bash_command()
        .arg("scripts/check-migration-guides.sh")
        .current_dir(dir)
        .output()
        .expect("run migration-guide gate")
}

fn gate_report(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn migration_guide_gate_passes_for_this_repository() {
    let root = workspace_root();
    let output = run_migration_gate(&root);
    assert!(
        output.status.success(),
        "scripts/check-migration-guides.sh must pass on this repository — every \
         breaking CHANGELOG entry needs a linked migration guide (issue #1588).\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guides_are_backfilled_for_published_releases() {
    let root = workspace_root();
    for version in BACKFILLED_MIGRATION_GUIDES {
        let guide = root.join(format!("docs/migrations/{version}.md"));
        assert!(
            guide.is_file(),
            "issue #1588 AC2 requires a backfilled migration guide at {}",
            guide.display(),
        );
    }

    // The 0.6.0 cycle renamed the generated repository constructor `with_pool`
    // to `with_pool_untracked` (#1273). The guide is the only place a user
    // upgrading onto that release can learn the new name.
    let guide_060 = std::fs::read_to_string(root.join("docs/migrations/0.6.0.md"))
        .expect("read docs/migrations/0.6.0.md");
    assert!(
        guide_060.contains("with_pool_untracked"),
        "docs/migrations/0.6.0.md must document the `with_pool` -> \
         `with_pool_untracked` rename (issue #1588 AC2)",
    );
}

#[test]
fn migration_guides_are_indexed_and_record_a_walkthrough() {
    let root = workspace_root();
    let migrations = root.join("docs/migrations");
    let index = std::fs::read_to_string(migrations.join("README.md")).expect("read index");

    for entry in std::fs::read_dir(&migrations).expect("read docs/migrations") {
        let path = entry.expect("dir entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let is_markdown = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if !is_markdown || name == "README.md" || name == "TEMPLATE.md" {
            continue;
        }

        assert!(
            index.contains(&name),
            "docs/migrations/README.md must index {name} (issue #1588 AC1)",
        );

        let guide = std::fs::read_to_string(&path).expect("read guide");
        assert!(
            guide.contains("## How to verify"),
            "{name} must tell the reader how to verify the upgrade (issue #1588 AC1)",
        );
        assert!(
            guide.contains("### Guide-only upgrade walkthrough"),
            "{name} must record the guide-only upgrade walkthrough (issue #1588 AC5)",
        );
    }
}

#[test]
fn migration_guide_gate_requires_a_guide_for_a_breaking_release() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Changed\n\n\
         - **api:** **Breaking:** `App::run` takes a config argument. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a breaking release with no migration guide must fail the gate\n{}",
        gate_report(&output),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("docs/migrations/0.7.0.md"),
        "the failure must name the missing guide path\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_requires_breaking_entries_to_link_their_guide() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Changed\n\n\
         - **api:** **Breaking:** `App::run` takes a config argument.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a breaking entry that does not link its guide must fail the gate \
         (issue #1588 AC3)\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_flags_unmarked_breaking_prose() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Changed\n\n\
         - **api:** the response shape changed. Breaking for code that matches\n  \
           on the old variant.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "breaking prose without the `**Breaking:**` marker must fail the gate — \
         an unmarked break is invisible to the coverage check\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_non_breaking_prose() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Added\n\n\
         - **notify:** a new store. All additions are additive — no\n  \
           breaking change to existing surfaces.\n\
         - **search:** a new backend is a new `impl` rather than a breaking change.\n\
         - **jobs:** Additive and non-breaking: jobs without the attribute are unchanged.\n\
         - **mcp:** `Origin` validation defends against DNS-rebinding without\n  \
           breaking agent clients.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "explicitly non-breaking prose must not trip the gate — false positives \
         would push contributors to spam the marker\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_rejects_stub_guides() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        "# Migrating from Autumn `0.6` to `0.7`\n\nTODO.\n",
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Changed\n\n\
         - **api:** **Breaking:** `App::run` takes a config argument. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a guide that only exists to satisfy the file check must fail the shape \
         check — an empty stub strands the reader just as hard as no guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_ignores_releases_before_the_backfill_floor() {
    let tmp = migration_gate_fixture("0.3.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.3.0] - 2026-04-27\n\n\
         ### Changed\n\n\
         - **api:** **Breaking:** an ancient break with no guide.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "guides for releases before 0.4.0 are explicitly out of scope (issue \
         #1588) — the gate must not demand them\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_survives_a_long_changelog() {
    // Regression guard mirroring `release_notes_script_detects_breaking_section_
    // with_long_changelog_entry`: `awk | grep -q` under `pipefail` drops the
    // rest of a long entry on SIGPIPE and reports a false negative.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));

    let mut changelog = String::from(
        "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Changed\n\n\
         - **api:** **Breaking:** `App::run` takes a config argument. See the \
           [migration guide](docs/migrations/0.7.0.md).\n\n\
         ### Added\n\n",
    );
    for i in 0..200_000 {
        writeln!(changelog, "- filler line {i}").expect("write filler line");
    }
    changelog.push_str("\n## [0.3.0] - 2026-04-27\n\n- Previous release.\n");
    std::fs::write(tmp.path().join("CHANGELOG.md"), changelog).expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "the gate must classify a long changelog entry correctly\n{}",
        gate_report(&output),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("docs/migrations/0.7.0.md"),
        "the gate must report the guide it matched\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_covers_unreleased_breaking_entries() {
    let tmp = migration_gate_fixture("0.6.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [Unreleased]\n\n\
         ### Changed\n\n\
         - **routing:** **Breaking:** `Route` gained a field.\n\n\
         ## [0.6.0] - 2026-07-18\n\n- Released.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an unreleased breaking entry must require the rolling \
         docs/migrations/next.md draft (issue #1588 AC3)\n{}",
        gate_report(&output),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("next.md"),
        "the failure must point at the rolling next.md draft\n{}",
        gate_report(&output),
    );
}

#[test]
fn release_checklist_gates_publication_on_the_migration_guide() {
    let root = workspace_root();
    let checklist_path = root.join("docs/release-checklist.md");
    let checklist = std::fs::read_to_string(&checklist_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", checklist_path.display()));

    // Anchor on the section itself, not on incidental strings that survive
    // deleting the very steps this test exists to protect.
    let gate_section = checklist
        .split("## Migration Guide Gate")
        .nth(1)
        .unwrap_or_else(|| {
            panic!(
                "{} must have a `## Migration Guide Gate` section (issue #1588 AC4)",
                checklist_path.display()
            )
        })
        .split("\n## ")
        .next()
        .expect("section body");

    for required in [
        // The gate itself, run before publishing.
        "scripts/check-migration-guides.sh",
        // The rolling draft is renamed and its changelog links repointed.
        "git mv docs/migrations/next.md",
        // AC5: the walk-through, against an app scaffolded on the previous
        // release, following only the guide, recorded in the guide.
        "autumn new",
        "cargo install autumn-cli --version",
        "### Guide-only upgrade walkthrough",
    ] {
        assert!(
            gate_section.contains(required),
            "the `## Migration Guide Gate` section of {} must require `{required}` \
             (issue #1588 AC4/AC5)",
            checklist_path.display(),
        );
    }
}

#[test]
fn ci_runs_the_migration_guide_gate_on_every_pull_request() {
    let root = workspace_root();
    let workflow_path = root.join(".github/workflows/ci.yml");
    // `.gitattributes` forces LF for `*.sh` but not for workflows, so on a
    // Windows checkout this file arrives with CRLF and every `\n`-anchored
    // split below would miss.
    let workflow = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", workflow_path.display()))
        .replace("\r\n", "\n");

    // `contains("pull_request")` over the whole file is not evidence: the
    // string also appears in the `concurrency:` expression. Check the `on:`
    // block, which is everything above `jobs:`.
    let (triggers, jobs) = workflow
        .split_once("\njobs:\n")
        .unwrap_or_else(|| panic!("{} must define jobs", workflow_path.display()));
    assert!(
        triggers.contains("  pull_request:"),
        "{} must trigger on pull requests so the gate runs at review time",
        workflow_path.display(),
    );

    // Find the job that runs the gate and check it is unguarded — a step
    // hidden behind `if: github.event_name == 'push'` would never see a PR.
    // A job starts at a two-space-indented `name:` key at the top level.
    let is_job_header = |line: &str| {
        line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim().contains(' ')
    };
    let mut job_blocks: Vec<String> = Vec::new();
    for line in jobs.lines() {
        if is_job_header(line) || job_blocks.is_empty() {
            job_blocks.push(String::new());
        }
        let block = job_blocks.last_mut().expect("a block is open");
        block.push_str(line);
        block.push('\n');
    }
    let job = job_blocks
        .iter()
        .find(|job| job.contains("check-migration-guides.sh"))
        .unwrap_or_else(|| {
            panic!(
                "{} must run the migration-guide gate so a breaking change without \
                 a guide fails CI on the PR, not at tag time (issue #1588 AC4)",
                workflow_path.display()
            )
        });
    assert!(
        !job.contains("if:"),
        "{}: the migration-guide job must not be conditional — it is the gate \
         that makes the guide mandatory",
        workflow_path.display(),
    );
    assert!(
        job.contains("actions/checkout"),
        "{}: the migration-guide job must check the repository out",
        workflow_path.display(),
    );
}

#[test]
fn migration_guide_gate_ignores_markers_inside_code_spans() {
    // An entry that *documents* the convention (``**Breaking:**`` in a code
    // span) is a mention, not a declaration. Without this, the changelog entry
    // that introduces the gate declares itself breaking.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Added\n\n\
         - **release:** a gate that reads the `**Breaking:**` marker. \
           Non-breaking. <!-- migration-guide-gate: documents the marker -->\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a `**Breaking:**` token inside a code span is a mention, not a \
         declaration\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_honours_an_explicit_suppression() {
    // Prose *about* breaking changes (release tooling, policy docs) trips the
    // unmarked-break lint by construction. The escape hatch is explicit and
    // greppable rather than a cleverer regex.
    let tmp = migration_gate_fixture("0.7.0");
    let changelog = "# Changelog\n\n\
         ## [0.7.0] - 2026-08-01\n\n\
         ### Added\n\n\
         - **release:** the gate fails when a section declares a breaking \
           change with no guide, or when a breaking entry does not link one.\n";
    std::fs::write(tmp.path().join("CHANGELOG.md"), changelog).expect("changelog");
    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "prose about breaking changes must trip the lint without a suppression",
    );

    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        format!(
            "{}  <!-- migration-guide-gate: describes the gate -->\n",
            changelog.trim_end()
        ),
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an explicit `<!-- migration-guide-gate: ... -->` suppression must \
         clear the entry\n{}",
        gate_report(&output),
    );
}

#[test]
fn every_repository_script_is_tracked_by_git() {
    // `.gitignore` carries a blanket `*.sh`, so a newly added gate script is
    // untracked by default: it runs locally, passes review, and then fails on
    // a clean CI clone with "No such file or directory". Walking the directory
    // rather than scraping workflow text covers scripts nested under
    // `scripts/lib/`, scripts sourced by other scripts, and scripts a
    // composite action or `.yaml` workflow invokes.
    let root = workspace_root();

    let tracked = Command::new("git")
        .args(["ls-files", "scripts/"])
        .current_dir(&root)
        .output()
        .expect("run git ls-files");
    assert!(tracked.status.success(), "git ls-files scripts/ failed");
    let tracked = String::from_utf8_lossy(&tracked.stdout).into_owned();

    let mut scripts = Vec::new();
    shell_scripts(&root.join("scripts"), &mut scripts);
    assert!(!scripts.is_empty(), "expected shell scripts under scripts/");

    for script in scripts {
        let relative = script
            .strip_prefix(&root)
            .expect("script lives under the workspace root")
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            tracked.lines().any(|line| line == relative),
            "{relative} is not tracked by git — the blanket `*.sh` entry in \
             .gitignore swallowed it, so anything invoking it fails on a clean \
             clone. Add it with `git add -f {relative}`.",
        );
    }
}

// ---------------------------------------------------------------------------
// Migration guide gate — bypass regressions found in review (issue #1588).
//
// Each of these was a fixture that made the gate report OK while a user would
// have been stranded, or blocked work that was fine.
// ---------------------------------------------------------------------------

/// A section body with two undeniable breaking entries and no guide anywhere:
/// any heading spelling that swallows this must fail the gate.
const TWO_BREAKS: &str = "\n### Breaking Changes\n\n\
     - **db:** **Breaking:** `with_pool` is renamed to `with_pool_untracked`.\n\
     - **config:** **Breaking:** `[server] port` must be an integer.\n";

#[test]
fn migration_guide_gate_rejects_section_headings_it_cannot_parse() {
    // A `## ` heading the version regex misses used to emit no record at all:
    // the section was not merely un-gated, it was invisible, and `--list`
    // reported nothing to do. `## [0.7.0-rc.1]` is the normal shape for the
    // release-candidate tags docs/release-checklist.md tells operators to cut.
    for heading in [
        "## [0.7.0-rc.1] - 2026-09-01",
        "## [v0.7.0] - 2026-09-01",
        "## [0.7] - 2026-09-01",
        "## Unreleased",
        "## [unreleased]",
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            format!("# Changelog\n\n{heading}\n{TWO_BREAKS}"),
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert!(
            !output.status.success(),
            "`{heading}` must not silently drop its section from the gate\n{}",
            gate_report(&output),
        );
    }
}

#[test]
fn migration_guide_gate_maps_a_release_candidate_to_the_release_guide() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0-rc.1] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an rc section must be gated against the release's own guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_rejects_an_unclosed_code_fence() {
    // One odd fence toggle used to swallow every later section, headings and
    // all — the gate reported OK for a changelog it had stopped reading.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        format!(
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Added\n\n\
             - **docs:** an example that forgets to close its fence:\n\n  \
               ```toml\n  [server]\n\n\
             ## [0.6.0] - 2026-08-01\n{TWO_BREAKS}"
        ),
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an unclosed fence hides every section below it — the gate must say so \
         rather than report OK on a changelog it stopped reading\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_reads_nested_fences() {
    // A ````markdown fence wrapping a ``` sample is balanced under CommonMark
    // (a closing fence must be at least as long as its opener) but was an odd
    // number of toggles under a naive parser.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Added\n\n\
         - **docs:** how to nest a fence:\n\n  \
           ````markdown\n  ```\n  ````\n\n\
         - **api:** a non-breaking addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a balanced nested fence must not confuse the parser\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_suppression_cannot_silence_a_declared_break() {
    // The suppression exists for entries that talk *about* breaking changes.
    // It must never override an explicit `**Breaking:**` declaration —
    // otherwise one comment on a parent bullet kills every nested marker under
    // it, which is exactly this repo's house style for multi-part entries.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** a multi-part entry. <!-- migration-guide-gate: n/a -->\n  \
           - **Breaking:** `with_pool` is renamed to `with_pool_untracked`.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a suppression comment must not silence an explicit `**Breaking:**` \
         marker\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_lose_a_marker_to_a_stray_backtick() {
    // Pairing backticks positionally made an odd backtick swallow the marker
    // *and* the word "breaking" — the author did everything right and the gate
    // required no guide at all.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** the ` character is now rejected in identifiers.\n  \
           **Breaking:** `with_pool` is renamed to `with_pool_untracked`.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an unbalanced inline code span must not make a declared break \
         invisible\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_negation_allowlist_does_not_swallow_real_breaks() {
    // Each of these uses the word "breaking" and is a genuine break; the
    // negation stripper must not decide they were negated.
    for entry in [
        "- **db:** no direct breaking change to the API, but `with_pool` is renamed.",
        "- **net:** prevents breaking the listener; `port` must now be an integer.",
        "- **auth:** avoids breaking tenants by removing `Session::from_request`.",
        "- **ws:** no longer breaking out early; `WsError::Closed` is removed.",
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            format!("# Changelog\n\n## [0.7.0] - 2026-09-01\n\n### Changed\n\n{entry}\n"),
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert!(
            !output.status.success(),
            "the negation allowlist swallowed a real break: {entry}\n{}",
            gate_report(&output),
        );
    }
}

#[test]
fn migration_guide_gate_reads_asterisk_bullets() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         * **db:** **Breaking:** `with_pool` is renamed.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "`* ` is a legal markdown bullet — entries written with it must not be \
         invisible to the gate\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_requires_a_real_link_not_a_mention() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. Someday we will write \
           `docs/migrations/0.7.0.md`.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a bare path mention is not a link — the reader must be able to click \
         through to the fix path\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_marker_case_variants() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **BREAKING:** `with_pool` is renamed.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "`**BREAKING:**` is the same declaration — it must demand a guide, not \
         fall through to the prose lint with misleading advice\n{}",
        gate_report(&output),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("docs/migrations/0.7.0.md"),
        "the failure must be the missing guide, not 'unmarked breaking change'\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_rejects_an_empty_sectioned_stub() {
    // Every required heading present, no content under any of them.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        "# Migrating to 0.7.0\n\n\
         ## At a glance\n\n\
         ## Summary\n\n\
         ## Before you start\n\n\
         ## Breaking changes\n\n\
         ## How to verify\n\n\
         ### Guide-only upgrade walkthrough\n\n\
         - **Status:** performed 2026-09-01\n",
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "headings with nothing under them are a stub — it strands the reader \
         exactly as hard as no guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_requires_real_headings_not_substrings() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        &valid_migration_guide("0.7.0").replace("## At a glance", "#### At a glance"),
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "`#### At a glance` must not satisfy a required `## At a glance` \
         heading by substring match",
    );
}

#[test]
fn migration_guide_gate_rejects_a_pending_walkthrough_on_a_released_guide() {
    // AC5 is "performed and recorded". `pending` is legitimate only on the
    // rolling next.md draft.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        &valid_migration_guide("0.7.0").replace("performed 2026-01-01", "pending"),
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a versioned guide must not ship with a pending walk-through — that is \
         the whole of issue #1588 AC5\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_list_mode_reports_the_inventory() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = bash_command()
        .args(["scripts/check-migration-guides.sh", "--list"])
        .current_dir(tmp.path())
        .output()
        .expect("run --list");
    assert!(
        output.status.success(),
        "--list must succeed\n{}",
        gate_report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.7.0") && stdout.contains("docs/migrations/0.7.0.md"),
        "--list is the release operator's inventory; it must name the section \
         and its guide\n{}",
        gate_report(&output),
    );

    let usage = bash_command()
        .args(["scripts/check-migration-guides.sh", "--bogus"])
        .current_dir(tmp.path())
        .output()
        .expect("run bogus flag");
    assert!(
        !usage.status.success(),
        "an unknown flag must not be silently treated as the gate",
    );
}

#[test]
fn release_notes_and_migration_gates_agree_on_what_is_breaking() {
    // Two scripts with two regexes drift. `check-release-notes.sh` must reach
    // the same verdict as the gate on the same changelog, or a release can
    // pass one and fail the other with no way to satisfy both.
    let root = workspace_root();
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::copy(
        root.join("scripts/check-release-notes.sh"),
        tmp.path().join("scripts/check-release-notes.sh"),
    )
    .expect("copy release-notes script");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Added\n\n\
         - **release:** a gate that reads the `**Breaking:**` marker. This entry \
           is non-breaking. <!-- migration-guide-gate: documents the marker -->\n",
    )
    .expect("changelog");

    let gate = run_migration_gate(tmp.path());
    let notes = bash_command()
        .arg("scripts/check-release-notes.sh")
        .current_dir(tmp.path())
        .output()
        .expect("run release-notes check");

    assert_eq!(
        gate.status.success(),
        notes.status.success(),
        "the two release gates disagree on the same changelog.\n\
         check-migration-guides.sh:\n{}\ncheck-release-notes.sh:\n{}",
        gate_report(&gate),
        gate_report(&notes),
    );
}

#[test]
fn migration_guide_gate_requires_a_dated_or_backfilled_walkthrough() {
    // "not yet performed" is a free pass a new release could write. A versioned
    // guide's status must *begin* with a dated run or an explicit `backfilled`
    // claim, which is visible in the diff — a negated form that merely contains
    // one of those words says the opposite of what it would be read as.
    for (status, should_pass) in [
        ("performed 2026-09-01, 18 minutes", true),
        (
            "backfilled — record written after the release shipped",
            true,
        ),
        ("not performed — record backfilled by #1588", false),
        ("not yet performed", false),
        ("pending", false),
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(
            &tmp,
            "0.7.0",
            &valid_migration_guide("0.7.0").replace("performed 2026-01-01", status),
        );
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** `with_pool` renamed. See the \
               [migration guide](docs/migrations/0.7.0.md).\n",
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert_eq!(
            output.status.success(),
            should_pass,
            "walk-through status {status:?} should {} the gate\n{}",
            if should_pass { "pass" } else { "fail" },
            gate_report(&output),
        );
    }
}

// --- Review round: fenced content and the rolling draft's lifecycle ---------

#[test]
fn migration_guide_gate_rejects_a_guide_whose_structure_is_only_a_code_sample() {
    // The guide parser did not track fences, so a guide could satisfy every
    // required heading — and the walk-through record — from inside a markdown
    // code sample. Same bug class as the changelog parser's, one file over.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        &format!(
            "# Migrating to 0.7.0\n\n\
             Here is what a guide looks like:\n\n\
             ````markdown\n{}````\n",
            valid_migration_guide("0.7.0")
        ),
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "headings inside a code sample are an illustration, not the guide's \
         own structure",
    );
}

#[test]
fn migration_guide_gate_reads_tilde_fenced_changelog_examples() {
    // `~~~` is a legal markdown fence. Recognising only backticks meant a
    // tilde-fenced config sample leaked into the entry, so a `breaking` key or
    // comment inside it tripped the unmarked-break lint on valid docs.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Added\n\n\
         - **config:** a new sample:\n\n  \
           ~~~toml\n  \
           # set this before breaking out the load balancer\n  \
           - not a bullet\n  \
           ~~~\n\n\
         - **api:** an unrelated addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a tilde-fenced example must be part of its entry, not parsed as \
         prose and bullets\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_a_freshly_recreated_rolling_draft() {
    // docs/release-checklist.md requires recreating `next.md` from TEMPLATE.md
    // after every release so the links to it keep resolving. The template
    // carries `{X.Y.Z}`-style placeholders by design, so the placeholder and
    // empty-section checks must not apply to the draft — otherwise following
    // the documented procedure leaves the gate red until someone invents
    // details for a release that has no changes yet.
    let recreated = template_draft();
    assert!(
        recreated.contains("{X.Y.Z}"),
        "this test is only meaningful while TEMPLATE.md still has placeholders",
    );

    let tmp = migration_gate_fixture("0.6.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n- Nothing breaking yet.\n",
    )
    .expect("changelog");
    write_fixture_guide(&tmp, "next", &recreated);

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a rolling draft recreated from the template must pass — this is the \
         documented post-release step\n{}",
        gate_report(&output),
    );

    // The same content under a released version must still be rejected: the
    // exemption is for the draft, not a licence to ship placeholders.
    write_fixture_guide(&tmp, "0.6.0", &recreated);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.6.0] - 2026-07-18\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.6.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a released guide full of template placeholders must fail\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("0.6.0.md"),
        "the finding must be about the released guide, not the draft\n{}",
        gate_report(&output),
    );
}

// --- Review round 2: rc paths, index entries, negated statuses -------------

#[test]
fn release_notes_gate_normalizes_release_candidate_guide_paths() {
    // The migration gate maps `## [0.7.0-rc.1]` to `docs/migrations/0.7.0.md`,
    // but check-release-notes.sh built the path straight from the workspace
    // version — so the rc guide the policy prescribes satisfied one gate and
    // blocked the other. Two gates, one answer.
    let root = workspace_root();
    let tmp = migration_gate_fixture("0.7.0-rc.1");
    std::fs::copy(
        root.join("scripts/check-release-notes.sh"),
        tmp.path().join("scripts/check-release-notes.sh"),
    )
    .expect("copy release-notes script");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0-rc.1] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let gate = run_migration_gate(tmp.path());
    assert!(
        gate.status.success(),
        "the rc section must be satisfied by its release's guide\n{}",
        gate_report(&gate),
    );

    let notes = bash_command()
        .arg("scripts/check-release-notes.sh")
        .current_dir(tmp.path())
        .output()
        .expect("run release-notes check");
    assert!(
        notes.status.success(),
        "check-release-notes.sh must look for the same guide the migration \
         gate prescribes for a release candidate\n{}",
        gate_report(&notes),
    );
}

#[test]
fn migration_guide_gate_requires_an_index_entry_not_a_mention() {
    // The index check matched the filename anywhere in README.md, and the
    // process text names `next.md` repeatedly — so deleting its Index bullet
    // during the documented release rename went unnoticed.
    let tmp = migration_gate_fixture("0.7.0");
    let migrations = tmp.path().join("docs/migrations");
    std::fs::write(migrations.join("0.7.0.md"), valid_migration_guide("0.7.0")).expect("guide");
    std::fs::write(
        migrations.join("README.md"),
        "# Migration Guides\n\n\
         ## Index\n\n\
         - [`0.6.0.md`](0.6.0.md)\n\n\
         ## Process\n\n\
         Rename the draft to `0.7.0.md` when the release ships.\n",
    )
    .expect("index");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a filename mentioned in the process prose is not an index entry — the \
         reader has to be able to find the guide from the Index",
    );
}

#[test]
fn migration_guide_gate_rejects_a_negated_walkthrough_status() {
    // The status was a substring search, so a status that explicitly says the
    // walk-through was NOT done satisfied the requirement that it was.
    for (status, should_pass) in [
        ("performed 2026-09-01, 18 minutes", true),
        ("backfilled — written after 0.7.0 shipped", true),
        ("not performed 2026-09-01", false),
        ("pending — performed 2026-09-01", false),
        ("we intend this to be performed 2026-09-01", false),
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(
            &tmp,
            "0.7.0",
            &valid_migration_guide("0.7.0").replace("performed 2026-01-01", status),
        );
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** `with_pool` renamed. See the \
               [migration guide](docs/migrations/0.7.0.md).\n",
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert_eq!(
            output.status.success(),
            should_pass,
            "walk-through status {status:?} should {} the gate\n{}",
            if should_pass { "pass" } else { "fail" },
            gate_report(&output),
        );
    }
}

// --- Review round 3: placeholders, code-span runs, status placement --------

#[test]
fn migration_guide_gate_rejects_any_unresolved_template_placeholder() {
    // The placeholder check listed a handful of tokens by name, so a guide
    // copied from TEMPLATE.md with only the version fields filled in — and
    // `{Area}`, `{old MSRV}`, `{Short description}` still in place — certified
    // as finished.
    for placeholder in ["{Area}", "{old MSRV}", "{Short description}"] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(
            &tmp,
            "0.7.0",
            &valid_migration_guide("0.7.0").replace(
                "### Area: the thing that broke",
                &format!("### {placeholder}: broke"),
            ),
        );
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** `with_pool` renamed. See the \
               [migration guide](docs/migrations/0.7.0.md).\n",
        )
        .expect("changelog");

        assert!(
            !run_migration_gate(tmp.path()).status.success(),
            "an unresolved {placeholder} must not certify as a finished guide",
        );
    }
}

#[test]
fn migration_guide_gate_disambiguates_a_marker_only_inside_a_code_span() {
    // A multi-backtick span is valid CommonMark, so ``**Breaking:**`` is a
    // mention. But a stray backtick can also swallow a real marker into what
    // *looks* like a span. The two are textually indistinguishable, and the
    // costs are not symmetric — a missed break strands users — so the gate
    // asks for one explicit token instead of guessing.
    let entry_with_span = "- **release:** the gate reads the ``**Breaking:**`` marker.";

    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        format!("# Changelog\n\n## [0.7.0] - 2026-09-01\n\n### Added\n\n{entry_with_span}\n"),
    )
    .expect("changelog");
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a marker that survives only inside a code span is ambiguous and must \
         be called out\n{}",
        gate_report(&output),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("code span"),
        "the failure must name the ambiguity, not just demand a guide\n{}",
        gate_report(&output),
    );

    // The documented resolution: say it is a mention.
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        format!(
            "# Changelog\n\n## [0.7.0] - 2026-09-01\n\n### Added\n\n{entry_with_span} \
             <!-- migration-guide-gate: documents the marker -->\n"
        ),
    )
    .expect("changelog");
    assert!(
        run_migration_gate(tmp.path()).status.success(),
        "an acknowledged mention must pass",
    );
}

#[test]
fn migration_guide_gate_requires_the_status_under_the_walkthrough_heading() {
    // A status recorded anywhere in the file satisfied the walk-through
    // requirement, so the section the release process actually points at could
    // hold nothing but prose.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        &valid_migration_guide("0.7.0")
            .replace(
                "Why this release breaks.",
                "Why this release breaks.\n\n- **Status:** performed 2026-09-01",
            )
            .replace(
                "- **Status:** performed 2026-01-01",
                "We will get to this soon.",
            ),
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "the walk-through result must be recorded under its own heading, not \
         anywhere in the guide",
    );
}

// --- Review round 4: placeholder vocabulary, heading anchoring -------------

#[test]
fn migration_guide_gate_rejects_placeholders_inside_code_spans_and_fences() {
    // Stripping code spans before looking for `{...}` — done to stop `{:?}`
    // from reading as a placeholder — hid the placeholders that matter most:
    // TEMPLATE.md puts its version fields in code spans and fenced snippets.
    for guide_body in [
        // Inline code span.
        valid_migration_guide("0.7.0").replace(
            "- **New version:** `autumn-web 0.7.0`",
            "- **New version:** `autumn-web {X.Y.Z}`",
        ),
        // Fenced snippet.
        valid_migration_guide("0.7.0").replace(
            "Pin the old version and get green.",
            "```toml\nautumn-web = \"{X.Y.Z}\"\n```",
        ),
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(&tmp, "0.7.0", &guide_body);
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** `with_pool` renamed. See the \
               [migration guide](docs/migrations/0.7.0.md).\n",
        )
        .expect("changelog");

        assert!(
            !run_migration_gate(tmp.path()).status.success(),
            "an unresolved template placeholder must be caught wherever it \
             hides:\n{guide_body}",
        );
    }
}

#[test]
fn migration_guide_gate_does_not_treat_rust_braces_as_placeholders() {
    // The flip side: guides are full of `{:?}`, `Route { .. }` and
    // `format!("{addr}")`. Only the tokens TEMPLATE.md actually emits count.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        // Braces come in as arguments: a literal `{:?}` in the source trips
        // clippy::literal_string_with_formatting_args, and escaping it instead
        // would leave `format!` with nothing to substitute.
        &valid_migration_guide("0.7.0").replace(
            "Before / after.",
            &format!(
                "Logs that formatted the config with `{open}:?{close}` now show \
                 it redacted, and `Route {open} .. {close}` literals need the \
                 new field:\n\n```rust,ignore\nlet route = Route {open} seo: \
                 SeoRouteDefaults::EMPTY {close};\nprintln!(\"{open}addr{close}\");\n```",
                open = '{',
                close = '}',
            ),
        ),
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "ordinary Rust and format braces are not template placeholders\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_only_reads_the_documented_breaking_heading() {
    // `### Breaking Changes` declares a break. A heading that merely starts
    // with the word — `### Breaking down request latency` — marked every
    // bullet under it, failing an additive section for missing guides.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking down request latency\n\n\
         - **perf:** the access log now records a p99 bucket.\n\
         - **perf:** connection reuse is reported separately.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "only `### Breaking Changes` declares a break — a heading that starts \
         with the word does not\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_ignores_a_guide_link_inside_a_code_span() {
    // Marker detection reads the code-span-stripped text; the link check read
    // the raw entry. A path in backticks renders as code, not as a clickable
    // link, so the reader is left without the promised path to the guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. Write it as \
           `[migration guide](docs/migrations/0.7.0.md)`.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link that only exists inside a code span renders as code, not as a \
         link the reader can follow",
    );
}

// --- Review round 6: the draft must exist; comments are not content -------

#[test]
fn migration_guide_gate_requires_the_rolling_draft_to_exist() {
    // docs/release-checklist.md requires recreating next.md after every
    // release so the links to it from README.md and STABILITY.md keep
    // resolving — but with no breaking entries under Unreleased there was
    // nothing to validate, so forgetting it passed silently and 404'd.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::remove_file(tmp.path().join("docs/migrations/next.md")).expect("remove draft");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n- **api:** a non-breaking addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "the rolling draft must exist between releases\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("next.md"),
        "the failure must name the missing draft\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_count_html_comments_as_content() {
    // `<!-- TODO -->` is the canonical stub marker and renders as nothing, so
    // a guide using it under every heading has no reader-visible instructions.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        // The whole required section, not a subsection under it: a nested
        // heading is itself content for its parent.
        &valid_migration_guide("0.7.0").replace(
            "### Area: the thing that broke\n\nBefore / after.",
            "<!-- TODO: write this before release -->",
        ),
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an HTML comment renders as nothing — it is not migration instructions",
    );
}

// --- Review round 7: HTML comments in both parsers ------------------------

#[test]
fn migration_guide_gate_ignores_headings_inside_html_comments() {
    // Round 6 stopped comments counting as *content* but left the heading and
    // status rules reading the raw line, so a guide could satisfy its whole
    // required structure from inside a comment block — reader-invisible.
    // Two shapes do *not* reproduce this, and it is worth knowing why: a guide
    // commented out wholesale is caught by the content check, and a one-line
    // `<!-- ## At a glance -->` never matches the heading rule because the line
    // starts with `<`. The shape that gets through is a multi-line comment,
    // where the heading does start at column 0, with visible prose under it —
    // the gate records structure the reader cannot see.
    let mut guide = String::from("# Migrating to 0.7.0\n\n");
    for heading in [
        "## At a glance",
        "## Summary",
        "## Before you start",
        "## Breaking changes",
        "## How to verify",
        "### Guide-only upgrade walkthrough",
    ] {
        writeln!(guide, "<!--\n{heading}\n-->\n\nSome prose.\n").expect("write guide");
    }
    guide.push_str("<!--\n- **Status:** performed 2026-09-01\n-->\n");

    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a guide commented out wholesale renders as nothing — its headings are \
         not the guide's structure",
    );
}

#[test]
fn migration_guide_gate_skips_commented_out_changelog_bullets() {
    // A bullet parked inside `<!-- ... -->` renders nowhere, so it is not a
    // declaration and must not demand a guide.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **api:** a non-breaking addition.\n\n\
         <!--\n\
         - **db:** **Breaking:** parked until we decide. No guide yet.\n\
         -->\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a commented-out bullet is not a changelog entry\n{}",
        gate_report(&output),
    );

    // Uncommented, the same bullet is a real declaration again.
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** parked until we decide. No guide yet.\n",
    )
    .expect("changelog");
    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "the same bullet outside a comment must still demand a guide",
    );
}

// --- Review round 8: only rendered markup counts --------------------------

#[test]
fn migration_guide_gate_ignores_a_suppression_shown_as_code() {
    // The suppression is read from the raw entry because it *is* an HTML
    // comment. That let an entry which merely *displays* the token in
    // backticks — documentation of the escape hatch — silence a real break.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** this is breaking for anyone calling it. Silence a mention \
           with `<!-- migration-guide-gate: reason -->`.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a suppression rendered as code is documentation, not a suppression\n{}",
        gate_report(&output),
    );

    // The real thing still works.
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** this is breaking for anyone calling it. \
           <!-- migration-guide-gate: explains the gate -->\n",
    )
    .expect("changelog");
    assert!(
        run_migration_gate(tmp.path()).status.success(),
        "a real suppression comment must still clear the entry",
    );
}

#[test]
fn migration_guide_gate_ignores_index_entries_that_do_not_render() {
    // An index entry the reader cannot click is not an index entry, however
    // it came to be invisible.
    for hidden in [
        "<!-- - [`0.7.0.md`](0.7.0.md) -->",
        "Write it as `- [`0.7.0.md`](0.7.0.md)`",
        "```markdown\n- [`0.7.0.md`](0.7.0.md)\n```",
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        let migrations = tmp.path().join("docs/migrations");
        std::fs::write(migrations.join("0.7.0.md"), valid_migration_guide("0.7.0")).expect("guide");
        std::fs::write(
            migrations.join("README.md"),
            format!("# Migration Guides\n\n## Index\n\n- [`next.md`](next.md)\n{hidden}\n"),
        )
        .expect("index");
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** `with_pool` renamed. See the \
               [migration guide](docs/migrations/0.7.0.md).\n",
        )
        .expect("changelog");

        assert!(
            !run_migration_gate(tmp.path()).status.success(),
            "an index entry that does not render is not discoverable: {hidden}",
        );
    }
}

// --- Review round 9: unclosed comments, empty fences ----------------------

#[test]
fn migration_guide_gate_rejects_an_unclosed_html_comment() {
    // The same fail-closed rule the unclosed *fence* already had: a stuck
    // comment state silently discards every heading and entry after it, so a
    // breaking release with no guide disappears from the gate entirely.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Added\n\n\
         - **api:** an addition.\n\n\
         <!-- parked, forgot to close\n\n\
         ## [0.6.0] - 2026-08-01\n\n\
         ### Breaking Changes\n\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an unclosed comment hides every section below it — the gate must say \
         so rather than report OK on a changelog it stopped reading\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("comment"),
        "the failure must name the unclosed comment\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_count_empty_fences_as_content() {
    // Fence delimiters are not migration instructions. An empty block under
    // every required heading renders as nothing at all.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(
        &tmp,
        "0.7.0",
        &valid_migration_guide("0.7.0").replace(
            "### Area: the thing that broke\n\nBefore / after.",
            "```rust\n```",
        ),
    );
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an empty fenced block is not content",
    );

    // A fence with something in it still is.
    write_fixture_guide(
        &tmp,
        "0.7.0",
        &valid_migration_guide("0.7.0").replace(
            "### Area: the thing that broke\n\nBefore / after.",
            "```rust\nlet repo = PostRepository::with_pool_untracked(pool);\n```",
        ),
    );
    assert!(
        run_migration_gate(tmp.path()).status.success(),
        "a fenced example with a line in it is perfectly good content",
    );
}

// --- Review round 10: fenced examples of markup ---------------------------

#[test]
fn migration_guide_gate_does_not_read_comment_delimiters_inside_fences() {
    // A fenced example that *shows* `<!--` and `-->` on separate lines is a
    // code sample, not a comment. Scanning its body for comment delimiters
    // marked every entry between the two lines as commented out — including a
    // real break with no guide.
    // A single fence that opens *and* closes the comment does not reproduce
    // it, and one that only opens it is caught by the unclosed-comment rule.
    // The shape that hides a break is two samples: one showing the opener, one
    // showing the closer, with a real entry between them.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Added\n\n\
         - **docs:** a suppression opens like this:\n\n  \
           ```markdown\n  <!--\n  ```\n\n\
         - **db:** **Breaking:** `with_pool` renamed, with no guide anywhere.\n\n\
         - **docs:** and closes like this:\n\n  \
           ```markdown\n  -->\n  ```\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a fenced example of comment syntax must not comment out the entries \
         after it",
    );
}

#[test]
fn migration_guide_gate_index_scan_handles_nested_fences() {
    // The index scan used a plain toggle rather than the CommonMark run-length
    // rule the other parsers use, so an inner ``` inside a ````markdown sample
    // "closed" the fence and the rendered code sample counted as an entry.
    let tmp = migration_gate_fixture("0.7.0");
    let migrations = tmp.path().join("docs/migrations");
    std::fs::write(migrations.join("0.7.0.md"), valid_migration_guide("0.7.0")).expect("guide");
    std::fs::write(
        migrations.join("README.md"),
        "# Migration Guides\n\n\
         ## Index\n\n\
         - [`next.md`](next.md)\n\n\
         Add a bullet shaped like this:\n\n\
         ````markdown\n```\n- [`0.7.0.md`](0.7.0.md)\n```\n````\n",
    )
    .expect("index");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link inside a nested fenced sample is not an index entry",
    );
}

#[test]
fn migration_guide_gate_does_not_read_comment_delimiters_inside_code_spans() {
    // Round 10 stopped fenced bodies from opening comments. The same shape one
    // level down — `<!--` and `-->` shown in separate *inline* spans — still
    // did, and because the later span cleared the state the unclosed-comment
    // guard never fired either.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Added\n\n\
         - **docs:** a suppression opens with `<!--` on its own line.\n\
         - **db:** **Breaking:** `with_pool` renamed, with no guide anywhere.\n\
         - **docs:** and closes with `-->`.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "delimiters displayed as code are not comment state\n{}",
        gate_report(&output),
    );

    // A real comment spanning those same entries still hides them.
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Added\n\n\
         - **api:** an addition.\n\n\
         <!--\n\
         - **db:** **Breaking:** parked, no guide.\n\
         -->\n",
    )
    .expect("changelog");
    assert!(
        run_migration_gate(tmp.path()).status.success(),
        "a genuine comment must still hide the entries inside it",
    );
}

// --- Review round 12: complete links, rendered heading --------------------

#[test]
fn migration_guide_gate_requires_a_complete_markdown_link() {
    // `](path)` on its own renders as literal text, not a link, so a typo that
    // drops the `[label]` leaves the reader with no way to reach the guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See \
           ](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a bracket typo is not a link",
    );

    // The complete form still passes.
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");
    assert!(
        run_migration_gate(tmp.path()).status.success(),
        "a complete inline link must still satisfy the check",
    );
}

#[test]
fn migration_guide_gate_reads_the_breaking_heading_as_rendered() {
    // The heading rule matches the rendered text, but the breaking-heading
    // test still read the raw line, so an invisible trailing comment took a
    // whole section out of the breaking inventory.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes <!-- release note -->\n\n\
         - **db:** `with_pool` is renamed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "the heading renders as `### Breaking Changes`, so the bullets under it \
         are breaking entries",
    );
}

// --- Review round 13: markdown's permitted leniency -----------------------

#[test]
fn migration_guide_gate_accepts_indented_release_headings() {
    // Up to three leading spaces is a valid ATX heading. An unrecognised
    // heading emitted no UNPARSED finding either, so the section vanished.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        // The indentation has to sit after an explicit `\n` on the same source
        // line: a trailing `\` continuation would swallow it.
        "# Changelog\n\n   ## [0.7.0] - 2026-09-01\n\n  ### Changed\n\n\
         - **db:** **Breaking:** `with_pool` renamed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an indented heading is still a heading",
    );
}

#[test]
fn migration_guide_gate_reads_plus_bullets() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         + **api:** **Breaking:** removed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "`+` is a legal markdown bullet",
    );
}

#[test]
fn migration_guide_gate_ignores_over_indented_fence_markers() {
    // Four spaces makes a line an indented code block, so those backticks are
    // literal text, not a fence. Treating them as one swallowed the entry
    // between two such displayed markers.
    //
    // The markers sit at top level on purpose: indentation is measured from
    // the enclosing list item content column (see the round-18 test), so the
    // same four spaces *inside* a `- ` item would only be two relative — and
    // that genuinely is a fence.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n    ```\n\n\
         - **db:** **Breaking:** `with_pool` renamed, with no guide anywhere.\n\n    ```\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "four-space-indented backticks are literal text, not a fence",
    );
}

// --- Review round 14: escapes, closing hashes, tabs, fence info strings ----

#[test]
fn migration_guide_gate_rejects_an_escaped_link_bracket() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See \\[migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an escaped bracket renders as literal text, so there is no link",
    );
}

#[test]
fn migration_guide_gate_reads_a_closed_atx_breaking_heading() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes ###\n\n\
         - **db:** `with_pool` is renamed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "trailing hashes are an ATX closing sequence, not heading text",
    );
}

#[test]
fn migration_guide_gate_reads_tab_separated_headings() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n##\t[0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a tab after `##` still opens an ATX heading",
    );
}

#[test]
fn migration_guide_gate_rejects_an_invalid_backtick_info_string() {
    // A backtick fence's info string cannot contain a backtick, so this line
    // opens nothing. Treating it as a fence hid the entries after it, and the
    // later literal run "closed" the synthetic fence so the unclosed guard
    // never fired.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **docs:** an example follows.\n\n```foo`bar\n\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n\n```\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an invalid info string does not open a fence",
    );
}

// --- Review round 15: bullet whitespace, image openers --------------------

#[test]
fn migration_guide_gate_reads_indented_and_tabbed_bullets() {
    // A list item may carry up to three leading spaces, and a tab after the
    // marker. Both were ignored outright, so the entry's marker, missing guide
    // and missing link all went unseen.
    for changelog in [
        "# Changelog\n\n## [0.7.0] - 2026-09-01\n\n### Changed\n\n  - **db:** **Breaking:** renamed, no guide.\n",
        "# Changelog\n\n## [0.7.0] - 2026-09-01\n\n### Changed\n\n-\t**db:** **Breaking:** renamed, no guide.\n",
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        std::fs::write(tmp.path().join("CHANGELOG.md"), changelog).expect("changelog");
        assert!(
            !run_migration_gate(tmp.path()).status.success(),
            "markdown renders this as a list entry:\n{changelog}",
        );
    }
}

#[test]
fn migration_guide_gate_keeps_nested_bullets_inside_their_entry() {
    // The counterpart, and the reason round 13 did not simply dedent bullets:
    // an indented bullet *under an open entry* is a nested list item belonging
    // to it. Splitting those apart would reinterpret this repo's real
    // changelog, where multi-part entries carry their marker on a sub-bullet.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **sharding:** a multi-part entry. See the \
           [migration guide](docs/migrations/0.7.0.md).\n  \
           - **Breaking:** `with_pool` is renamed.\n  \
           - the signature is unchanged.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "the parent entry carries the link for its nested bullets\n{}",
        gate_report(&output),
    );
    let list = bash_command()
        .args(["scripts/check-migration-guides.sh", "--list"])
        .current_dir(tmp.path())
        .output()
        .expect("run --list");
    assert!(
        String::from_utf8_lossy(&list.stdout)
            .lines()
            .any(|line| line.starts_with("0.7.0") && line.trim_end().ends_with(" 1")),
        "the nested bullets are one entry, not three\n{}",
        String::from_utf8_lossy(&list.stdout),
    );
}

#[test]
fn migration_guide_gate_rejects_an_image_as_a_guide_link() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See \
           ![migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "`![...](path)` renders an image, not a link the reader can follow",
    );
}

// --- Review round 16: escaped openers, fences inside comments -------------

#[test]
fn migration_guide_gate_rejects_an_escaped_suppression_opener() {
    // `\<!-- ... -->` renders as literal text, so it is a displayed example of
    // the escape hatch rather than a use of it.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** this is breaking for every caller. Silence a mention with \
           \\<!-- migration-guide-gate: rendered example -->\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an escaped opener is not a suppression",
    );
}

#[test]
fn migration_guide_gate_treats_fences_inside_comments_as_comment_content() {
    // Round 10 put fences before comments so a fenced example of `<!--` could
    // not comment out the file. The mirror case then broke: a fence displayed
    // *inside* a comment opened a real fence, swallowed the entries after the
    // comment closed, and a later rendered fence closed the synthetic one — so
    // both states ended clear and no unclosed guard fired.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <!--\n```\n-->\n\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n\n```\n-->\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a fence delimiter inside a comment is comment content, not a fence",
    );
}

// --- Review round 17: sibling bullets, literal comment bodies -------------

#[test]
fn migration_guide_gate_splits_indented_sibling_bullets() {
    // Round 15's `entry == ""` guard let the *first* indented bullet start an
    // entry and then swallowed its siblings as if they were nested, so a
    // sibling's link covered a breaking entry that had none.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## [0.7.0] - 2026-09-01\n\n### Changed\n\n  \
         - **db:** **Breaking:** renamed, with no link of its own.\n  \
         - **docs:** see the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "sibling bullets are separate entries; one cannot carry another's link",
    );
}

#[test]
fn migration_guide_gate_closes_comments_on_a_backticked_terminator() {
    // Code spans are not parsed inside an HTML comment, so a backtick-wrapped
    // `-->` still closes it. Masking it away kept the comment open and hid a
    // visible breaking entry until a later stray terminator tidied up.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <!-- parked, terminator shown as `-->` `\n\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n\n-->\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "the comment ends at the first `-->`, so the entry after it is visible",
    );
}

#[test]
fn migration_guide_gate_rejects_an_escaped_closing_bracket() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide\\](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an escaped `]` does not close a link label",
    );
}

#[test]
fn migration_guide_gate_validates_the_drafts_status_value() {
    // The draft exemption widened the accepted set to "anything", so a typo or
    // an explicitly unsuccessful record certified as a walk-through.
    for (status, should_pass) in [
        ("pending — recorded at release", true),
        ("performed 2026-09-01", true),
        ("failed", false),
        ("pendign", false),
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        let draft = std::fs::read_to_string(tmp.path().join("docs/migrations/next.md"))
            .expect("read draft");
        let updated = draft
            .lines()
            .map(|line| {
                if line.starts_with("- **Status:**") {
                    format!("- **Status:** {status}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("docs/migrations/next.md"), updated).expect("write draft");
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n## [Unreleased]\n\n- **api:** an addition.\n",
        )
        .expect("changelog");

        assert_eq!(
            run_migration_gate(tmp.path()).status.success(),
            should_pass,
            "draft status {status:?} should {} the gate",
            if should_pass { "pass" } else { "fail" },
        );
    }
}

// --- Review round 18: list-item containers --------------------------------

#[test]
fn migration_guide_gate_ends_an_entry_when_its_list_item_ends() {
    // A blank line then an unindented paragraph is outside the list item, so a
    // release-wide link in that paragraph is not the bullet's link.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n\
         Everything in this release is covered by the \
         [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a paragraph outside the list item does not supply the bullet's link",
    );
}

#[test]
fn migration_guide_gate_reads_fences_relative_to_their_list_item() {
    // Markdown strips the list item's indentation before interpreting blocks
    // inside it, so a four-space-indented fence within a `- ` item really is a
    // fence — and a link displayed in it is a code sample, not a link.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. Link entries like this:\n\n    \
           ~~~markdown\n    See the [migration guide](docs/migrations/0.7.0.md).\n    ~~~\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link displayed inside a fenced example is not the entry's link",
    );
}

// --- Review round 19: sibling reprocessing, nested columns, indented code --

#[test]
fn migration_guide_gate_does_not_drop_a_sibling_at_a_different_indent() {
    // Round 18 regression: ` - x` after `- y` is deeper than the previous
    // entry's marker but left of its content column, so the bullet rule
    // rejected it and the continuation path flushed without reprocessing it.
    // The entry vanished entirely — the worst possible outcome for a gate.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **api:** an addition.\n \
         - **db:** **Breaking:** renamed, with no link.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "the sibling is an entry and must be checked, not discarded\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_reads_fences_inside_nested_items() {
    // A fence is measured from the *innermost* open item's content column.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n  \
           - link entries like this:\n\n       \
           ~~~markdown\n       See the [migration guide](docs/migrations/0.7.0.md).\n       ~~~\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link shown inside a fenced sample in a nested item is not a link",
    );
}

#[test]
fn migration_guide_gate_ignores_links_in_indented_code_blocks() {
    // Four spaces past the item's content column is an indented code block, so
    // the link renders as literal text.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n      \
           See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an indented code block renders as literal text, not a link",
    );
}

// --- Review round 20: empty child headings, ordered lists ------------------

#[test]
fn migration_guide_gate_requires_content_under_child_headings() {
    // A deeper heading counts as content for its parent only once the child
    // itself has some: otherwise a tree of empty headings reads as a complete
    // guide while containing no instructions at all.
    let tmp = migration_gate_fixture("0.7.0");
    let mut guide = String::from("# Migrating to 0.7.0\n\n");
    for heading in [
        "## At a glance",
        "## Summary",
        "## Before you start",
        "## Breaking changes",
        "## How to verify",
    ] {
        writeln!(guide, "{heading}\n\n#### Placeholder\n").expect("write guide");
    }
    guide.push_str("### Guide-only upgrade walkthrough\n\n- **Status:** performed 2026-09-01\n");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an empty child heading is not migration guidance",
    );
}

#[test]
fn migration_guide_gate_reads_ordered_list_entries() {
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         1. **api:** **Breaking:** removed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "`1.` is a legal list marker, so this is a declared break",
    );
}

#[test]
fn migration_guide_gate_nests_under_ordered_items() {
    // The counterpart: content indented under an ordered marker belongs to it.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         1. **api:** **Breaking:** removed. See the\n   \
            [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "the continuation line belongs to the ordered item\n{}",
        gate_report(&output),
    );
}

// --- Review round 21: tab stops, lazy continuation ------------------------

#[test]
fn migration_guide_gate_expands_tabs_when_measuring_columns() {
    // A tab advances to the next 4-column stop, so `-\t` puts content at
    // column 4 and a two-space bullet after it is a *sibling*, not nested.
    // Counting the tab as one column merged them and let the sibling's link
    // cover the breaking entry.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         -\t**db:** **Breaking:** renamed, with no link of its own.\n  \
         - **docs:** see the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "the two items are siblings; one cannot supply the other's link",
    );
}

#[test]
fn migration_guide_gate_keeps_lazy_continuation_lines() {
    // CommonMark lets an unindented line immediately after a list item's first
    // line continue its paragraph. Flushing there dropped the marker entirely.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** API removal details follow\n\
         **Breaking:** `with_pool` is removed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a lazy continuation line is part of the entry, marker and all",
    );

    // A blank line ends the paragraph, so the same line is then outside it.
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n\
         Everything here is in the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");
    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "after a blank line the paragraph is outside the item again",
    );
}

#[test]
fn migration_guide_gate_ignores_links_inside_html_attributes() {
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. \
           <span title=\"[migration guide](docs/migrations/0.7.0.md)\">details</span>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "markdown does not render a link inside an HTML attribute",
    );
}

#[test]
fn migration_guide_gate_does_not_mistake_generics_for_html() {
    // The counterpart, and the reason the mask is narrow: this changelog is
    // full of `Option<String>` and `Vec<Route>`. Masking every `<...>` would
    // silently delete prose the marker and the lint both read.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** `Option<String>` becomes \
           `Option<SecretString>` for every `Vec<Route>` builder. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "generic type parameters are not HTML tags\n{}",
        gate_report(&output),
    );
}

// --- Review round 23: raw HTML blocks, template presence ------------------

#[test]
fn migration_guide_gate_ignores_links_inside_raw_html_blocks() {
    // CommonMark leaves the contents of a raw HTML block literal, so a link
    // written inside one is not clickable.
    //
    // The block has to sit *inside* the list item to reproduce: an unindented
    // one after a blank line is already outside the entry, by the round-18
    // item-end rule, so its link was never the entry link in the first place.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <div>\n  See the [migration guide](docs/migrations/0.7.0.md).\n  </div>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "text inside a raw HTML block is literal, not a link",
    );
}

#[test]
fn migration_guide_gate_requires_the_guide_template() {
    // The placeholder vocabulary is read from TEMPLATE.md. If the file goes
    // missing the loop reads nothing, placeholder validation silently switches
    // off, and the release workflow loses the file it recreates next.md from.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::remove_file(tmp.path().join("docs/migrations/TEMPLATE.md")).expect("remove template");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n- **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a missing template must fail loudly, not disable a check\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TEMPLATE.md"),
        "the failure must name the missing template\n{}",
        gate_report(&output),
    );
}

// --- Review round 24: HTML block end conditions ---------------------------

#[test]
fn migration_guide_gate_ends_a_script_block_at_its_closing_tag() {
    // CommonMark closes a `<script>`/`<style>`/`<pre>`/`<textarea>` block (type
    // 1) on the line carrying the end tag, not at the next blank line like a
    // `<div>` (type 6). Holding the block open past `</script>` swallowed a
    // link that renders and is clickable, failing an entry that is correct.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         <script>\n  var demo = 1;\n  </script>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "the line after `</script>` is a paragraph again, so its link counts\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_keeps_text_after_a_closing_script_tag_literal() {
    // The end-condition line is *part of* the block, so a link sharing that
    // line with `</script>` is raw HTML and renders as text. Closing the block
    // mid-line instead would count a link the reader cannot click.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         <script>\n  var demo = 1;\n  \
         </script>See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "text sharing the closing-tag line is still inside the HTML block",
    );
}

#[test]
fn migration_guide_gate_closes_a_single_line_pre_block() {
    // `<pre>x</pre>` opens and closes on one line: the block must not stay
    // open and eat the entry's link on the line below.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         <pre>old_name</pre>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a one-line `<pre>` block ends on its own line\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_still_holds_a_div_block_open_to_the_blank_line() {
    // The counterpart guard: type 6 keeps its blank-line end condition, so the
    // type-1 fix must not be applied to every tag. A `**Breaking:**` marker
    // parked inside a `<div>` renders as literal text, declaring nothing, so
    // the section has no breaking entry and needs no guide.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <div>\n\
         - **db:** **Breaking:** renamed, with no guide.\n\
         </div>\n\
         - **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a `<div>` block runs to the blank line, so nothing inside it declares\n{}",
        gate_report(&output),
    );
}

// --- Review round 25: inline link destinations ----------------------------

#[test]
fn migration_guide_gate_accepts_a_guide_link_carrying_a_title() {
    // CommonMark allows an optional title after the destination. Matching the
    // literal `](path)` rejected `](path "title")`, so a link the reader can
    // click failed the gate.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md \"upgrade instructions\").\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a link title is metadata, not a reason to fail the build\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_an_angle_bracketed_guide_link() {
    // The other destination form: `<...>` around the path, with optional
    // whitespace inside the parentheses.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide]( <docs/migrations/0.7.0.md> ).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "`<path>` is a destination, and surrounding whitespace is allowed\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_read_the_guide_path_out_of_a_link_title() {
    // The counterpart, and the reason the destination is parsed rather than
    // searched for: this link goes somewhere else and only *mentions* the
    // guide in its tooltip. Clicking it does not reach the upgrade path.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [release notes](https://example.invalid/notes \
           \"docs/migrations/0.7.0.md\").\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a path named in a title is not where the link goes",
    );
}

#[test]
fn migration_guide_gate_rejects_a_guide_link_with_an_unclosed_title() {
    // An unterminated title is not a link at all — CommonMark falls back to
    // literal text — so the entry still has no clickable upgrade path.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md \"upgrade instructions).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an unclosed title leaves the whole construct as literal text",
    );
}

// --- Review round 26: link labels, titles, type-6 openers -----------------

#[test]
fn migration_guide_gate_requires_whitespace_before_a_link_title() {
    // CommonMark separates an optional title from the destination with
    // whitespace. `(<path>"title")` therefore closes nothing and renders as
    // literal text, so an entry written that way has no clickable guide —
    // skipping optional whitespace without recording that any was there
    // accepted it.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](<docs/migrations/0.7.0.md>\"upgrade\").\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a title jammed against an angle destination does not make a link",
    );
}

#[test]
fn migration_guide_gate_accepts_balanced_brackets_in_a_link_label() {
    // A link label may contain balanced brackets. Stopping the backward scan
    // at the first `]` mistook the inner label's bracket for the end of a
    // previous link and rejected a link that is clickable.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See \
           [the [migration guide]](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "balanced brackets are allowed inside a link label\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_opens_a_type_six_block_with_trailing_content() {
    // CommonMark type 6 starts on a known block tag followed by whitespace,
    // `>`, `/>` or end of line — the rest of the line may be anything. Requiring
    // the tag to stand alone let `<div>example` slip through, leaving the link
    // on the next line inside the block scanned as if it were clickable.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <div>example\n  See the [migration guide](docs/migrations/0.7.0.md).\n  </div>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "text inside a raw HTML block is literal however the block opened",
    );
}

#[test]
fn migration_guide_gate_does_not_treat_an_unknown_tag_as_a_block_opener() {
    // The counterpart guard: type 6 is a fixed list of block tags. Loosening it
    // to any tag name would swallow prose — `<MyWidget>` in a sentence, or a
    // generic like `<Route>` — and delete entries the marker and lint read.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         <MyWidget>renders differently now.\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an unknown tag name does not open a raw HTML block\n{}",
        gate_report(&output),
    );
}

// --- Review round 27: attribute values, reference definitions -------------

#[test]
fn migration_guide_gate_masks_through_a_quoted_attribute_containing_a_bracket() {
    // A `>` inside a quoted attribute value does not end the tag, so the
    // whole construct is raw HTML and the link text inside it is literal.
    // Stopping the mask at the first `>` left it looking clickable.
    //
    // Confirmed against the CommonMark reference implementation: this renders
    // as `<span title="> [guide](…)">x</span>` — no anchor element.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. \
           <span title=\"> [migration guide](docs/migrations/0.7.0.md)\">details</span>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a `>` inside a quoted attribute value does not close the tag",
    );
}

#[test]
fn migration_guide_gate_does_not_count_a_link_definition_as_section_content() {
    // A link reference definition renders nothing on its own, so a required
    // section holding only one is empty to the reader — the gate exists to
    // stop exactly that kind of hollow guide.
    // Targets `## Summary`, which has no child heading: a required section is
    // allowed to take its content from a subsection, so `## How to verify`
    // would be populated by its walkthrough child either way.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "[stub]: https://example.invalid",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an invisible reference definition is not migration guidance\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Summary"),
        "the failure must name the empty section\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_keeps_a_link_definition_from_hiding_real_content() {
    // The counterpart: a section that carries a definition *and* prose is not
    // empty. Dropping the whole line, rather than only definitions, would
    // start failing guides that use reference-style links.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "[stub]: https://example.invalid\n\nWhy this breaks, in full, with [stub].",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "reference-style links are normal markdown, not an empty section\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_closes_a_type_one_block_on_any_type_one_end_tag() {
    // CommonMark ends a type-1 block on `</script>`, `</style>`, `</pre>` or
    // `</textarea>` — spec: "it need not match the start tag". Verified
    // against the reference implementation: a `<script>` block closed by
    // `</style>` renders the following line as a paragraph, and its link as a
    // real anchor, exactly as `</script>` would.
    //
    // Recorded as a test because "close only on the matching tag" looks like
    // the more careful rule and is not: it would hold the block open past
    // where readers see it end, swallowing links that are clickable.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         <script>\n  var demo = 1;\n  </style>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "any type-1 end tag closes the block, per the spec\n{}",
        gate_report(&output),
    );
}

// --- Review round 28: list-relative openers, non-tag HTML blocks ----------

#[test]
fn migration_guide_gate_opens_an_html_block_at_the_list_content_column() {
    // Markdown strips a list item's content indentation before interpreting
    // the blocks inside it, so four spaces under a `- ` item is a two-space
    // indent — a valid HTML block opener. Measuring from column 0 left it
    // looking like over-indented text and the link inside it looking
    // clickable. The fence rule already measures this way.
    //
    // Reference implementation: the `<div>` body, link included, renders
    // literally with no anchor element.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n    \
         <div>\n    See the [migration guide](docs/migrations/0.7.0.md).\n    </div>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "four spaces inside a `- ` item is a two-space indent",
    );
}

#[test]
fn migration_guide_gate_treats_a_processing_instruction_block_as_literal() {
    // CommonMark type 3: `<?` opens a raw block that runs to `?>`.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <?demo\n  See the [migration guide](docs/migrations/0.7.0.md).\n  ?>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a processing instruction leaves its body literal",
    );
}

#[test]
fn migration_guide_gate_treats_a_cdata_block_as_literal() {
    // CommonMark type 5: `<![CDATA[` runs to `]]>`.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <![CDATA[\n  See the [migration guide](docs/migrations/0.7.0.md).\n  ]]>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a CDATA section leaves its body literal",
    );
}

#[test]
fn migration_guide_gate_treats_a_declaration_block_as_literal() {
    // CommonMark type 4: `<!` plus a letter runs to the next `>`.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <!DOCTYPE demo\n  See the [migration guide](docs/migrations/0.7.0.md).\n  >\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a declaration leaves its body literal",
    );
}

#[test]
fn migration_guide_gate_closes_a_non_tag_html_block_on_its_own_line() {
    // The counterpart guard: these blocks end on the line carrying their
    // terminator, including the opening line. Holding one open would swallow
    // the entry's real link on a later line and fail a correct changelog.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         <?demo ?>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a one-line `<? ?>` block ends on its own line\n{}",
        gate_report(&output),
    );
}

// --- Review round 29: tab columns, unclosed blocks, guide list indent -----

#[test]
fn migration_guide_gate_strips_tab_indentation_by_column_not_character() {
    // `leading_spaces` counts visual columns, so a tab reads as four. Feeding
    // that count to `substr` on the *unexpanded* line started the slice in the
    // middle of the fence marker, and the fence was missed — leaving a link
    // that renders as code text looking like a clickable guide.
    //
    // Reference implementation: the fence body renders inside `<pre><code>`,
    // link included, with no anchor element.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\
         \t~~~\n\tSee the [migration guide](docs/migrations/0.7.0.md).\n\t~~~\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a tab-indented fence inside a list item is a fence",
    );
}

#[test]
fn migration_guide_gate_rejects_an_unclosed_raw_html_block() {
    // An unclosed block swallows the rest of the file, exactly as an unclosed
    // fence or comment does — every section and entry after it disappears from
    // all four checks. Those two already fail closed; this one has to as well.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         <script>\n\n\
         ## [Unreleased]\n\n\
         - **db:** **Breaking:** renamed, with no guide.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an unclosed HTML block hides every entry after it\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("never closed"),
        "the failure must name the unclosed opener\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_reads_guide_fences_at_the_list_content_column() {
    // The guide validator never tracked list indentation, so a fence indented
    // four spaces under a `- ` item was missed and the headings *displayed*
    // inside that sample were counted as the guide's own sections. A guide can
    // then satisfy every shape check while containing no instructions at all.
    //
    // The uneven indentation is the point, and is what a column-blind parser
    // gets wrong: the fence marker sits at four spaces, which `dedent3` alone
    // cannot see past, while the sample body sits at two, which it can. Both
    // are inside the item, so CommonMark reads the whole sample as code — the
    // reference implementation renders `## How to verify` within
    // `<pre><code>`, not as a heading.
    // This guide has no `## How to verify` section of its own — only one
    // *displayed* inside the sample — and every other required section for
    // real, so the sample is the only thing that can satisfy the check.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = "# Migrating from Autumn `0.x` to `0.7.0`\n\n\
         ## At a glance\n\n\
         - **New version:** `autumn-web 0.7.0`\n\n\
         ## Summary\n\n\
         Why this release breaks.\n\n\
         ## Before you start\n\n\
         Pin the old version and get green.\n\n\
         ## Breaking changes\n\n\
         ### Area: the thing that broke\n\n\
         Before / after.\n\n\
         - The verification section is expected to look like this:\n\n    \
         ```markdown\n  ## How to verify\n\n  Run `cargo check`.\n    ```\n\n\
         ### Guide-only upgrade walkthrough\n\n\
         - **Status:** performed 2026-01-01\n";
    write_fixture_guide(&tmp, "0.7.0", guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a displayed sample is not the guide's own structure\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_a_guide_showing_a_fenced_sample_in_a_list() {
    // The counterpart guard: tracking list indentation must not start failing
    // guides that legitimately show a fenced example under a bullet while
    // carrying every required section for real.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = format!(
        "{}\n## Appendix\n\n\
         - A config sample, shown under a bullet:\n\n    \
         ```toml\n    [server]\n    port = 3000\n    ```\n",
        valid_migration_guide("0.7.0")
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a fenced sample under a bullet is normal guide writing\n{}",
        gate_report(&output),
    );
}

// --- Review round 30: reference-style guide links -------------------------

/// A changelog whose breaking entry links its guide in `link_markup`, plus
/// whatever `tail` adds after it (usually the reference definition).
fn reference_link_changelog(link_markup: &str, tail: &str) -> String {
    format!(
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See {link_markup}.\n\n{tail}"
    )
}

#[test]
fn migration_guide_gate_accepts_a_full_reference_style_guide_link() {
    // `[text][label]` with the definition elsewhere is a normal markdown link
    // and renders as a real anchor; rejecting it blocks CI on valid writing.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        reference_link_changelog(
            "the [migration guide][upgrade]",
            "[upgrade]: docs/migrations/0.7.0.md\n",
        ),
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a reference-style link is a link\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_collapsed_and_shortcut_reference_links() {
    // `[label][]` and a bare `[label]` both resolve to the same definition.
    for markup in ["[upgrade][]", "[upgrade]"] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            reference_link_changelog(markup, "[upgrade]: docs/migrations/0.7.0.md\n"),
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert!(
            output.status.success(),
            "`{markup}` resolves to the guide\n{}",
            gate_report(&output),
        );
    }
}

#[test]
fn migration_guide_gate_matches_reference_labels_case_insensitively() {
    // CommonMark folds case when matching a label to its definition.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        reference_link_changelog(
            "the [migration guide][UpGrade]",
            "[upgrade]: docs/migrations/0.7.0.md\n",
        ),
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "labels match case-insensitively\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_ignores_a_reference_definition_inside_a_fence() {
    // The fail-open counterpart: a definition *displayed* in a code sample
    // defines nothing, so the entry above it still has no clickable guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        reference_link_changelog(
            "the [migration guide][upgrade]",
            "```\n[upgrade]: docs/migrations/0.7.0.md\n```\n",
        ),
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a definition shown inside a fence defines nothing",
    );
}

#[test]
fn migration_guide_gate_rejects_a_reference_link_with_no_definition() {
    // An unresolved label renders as literal text, so there is no link at all.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        reference_link_changelog(
            "the [migration guide][nope]",
            "[upgrade]: docs/migrations/0.7.0.md\n",
        ),
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an undefined label is not a link",
    );
}

// --- Review round 31: type-7 vs open paragraphs, definition indentation ---

#[test]
fn migration_guide_gate_keeps_a_type_seven_tag_inline_inside_a_paragraph() {
    // CommonMark types 1-6 may interrupt a paragraph; type 7 may not. A custom
    // tag on the line after an entry's first line is therefore inline HTML,
    // the paragraph continues, and the link below it is a real anchor —
    // confirmed against the reference implementation. Opening a block here
    // discarded a link the reader can click and failed a correct entry.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         <x-widget>\n  See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a type-7 tag cannot interrupt a paragraph\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_opens_a_type_seven_block_after_a_blank_line() {
    // The counterpart: with no paragraph open, the same tag *does* start a
    // block, and the link inside it is literal. Both halves have to hold, or
    // "type 7 never opens" becomes a hole rather than a fix.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <x-widget>\n  See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "with no paragraph open the tag starts a raw block",
    );
}

#[test]
fn migration_guide_gate_lets_a_type_six_tag_interrupt_a_paragraph() {
    // The other counterpart, and the reason the rule is scoped to type 7:
    // `<div>` is a block tag and *may* interrupt a paragraph, so the link
    // below it stays literal.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n  \
         <div>\n  See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a type-6 block tag does interrupt a paragraph",
    );
}

#[test]
fn migration_guide_gate_collects_definitions_indented_under_a_wide_marker() {
    // A `100. ` marker puts its content column at five, so a definition
    // indented five spaces sits inside the item and is a real definition. The
    // collection pass only stripped three columns and missed it, rejecting a
    // reference link that resolves and renders.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         100. **Breaking:** renamed. See the [migration guide][upgrade].\n\n     \
         [upgrade]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a definition inside a wide list item still defines\n{}",
        gate_report(&output),
    );
}

// --- Review round 32: quoted attrs in type-7, index list indentation ------

#[test]
fn migration_guide_gate_opens_a_type_seven_tag_with_a_quoted_bracket() {
    // A `>` inside a quoted attribute value does not end the tag, so
    // `<x-widget title=">">` is still a complete tag standing alone on its
    // line and still opens a raw block. Rejecting it left the link below
    // looking clickable when the reference implementation renders it as text.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <x-widget title=\">\">\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a quoted `>` does not close the tag, so the block still opens",
    );
}

#[test]
fn migration_guide_gate_reads_index_fences_at_the_list_content_column() {
    // The index scan was the last parser measuring indentation from column 0.
    // A fence inside an Index list item went unrecognised, so an entry merely
    // *displayed* in a sample counted as the guide being indexed — and the
    // index is the only way a reader finds a guide at all.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("docs/migrations/0.7.0.md"),
        valid_migration_guide("0.7.0"),
    )
    .expect("guide");
    std::fs::write(
        tmp.path().join("docs/migrations/README.md"),
        "# Migration Guides\n\n\
         ## Index\n\n\
         - [`next.md`](next.md)\n\
         - An entry is written like this:\n\n    \
         ```markdown\n  - [`0.7.0.md`](0.7.0.md)\n    ```\n",
    )
    .expect("index");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an entry shown inside a code sample does not index the guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guides_never_show_plugin_check_without_its_required_flag() {
    // `--plugin-name` is a required argument, so a bare `autumn plugin-check`
    // exits during argument parsing. The guide-only walk-through is performed
    // by following a guide literally from its first step, which makes an
    // unrunnable command a defect wherever it appears — not only under
    // `## How to verify`.
    let root = workspace_root();
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(root.join("docs/migrations")).expect("migrations dir") {
        let path = entry.expect("dir entry").path();
        if path
            .extension()
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("read guide");
        for (idx, line) in body.lines().enumerate() {
            if line.contains("autumn plugin-check") && !line.contains("--plugin-name") {
                offenders.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "`autumn plugin-check` needs --plugin-name to run at all:\n{}",
        offenders.join("\n"),
    );
}

// --- Review round 33: item boundaries, empty tags, marked paragraphs ------

#[test]
fn migration_guide_gate_ends_an_entry_at_an_outdented_fence() {
    // A column-zero fence ends the list item, so the link after it renders in
    // its own paragraph, outside the entry. Skipping fence lines without
    // closing the item let that link be absorbed as a lazy continuation.
    //
    // No blank lines anywhere: a blank line before the fence would already
    // have closed the item paragraph and the entry would flush correctly. It
    // is the fence *itself* that has to end the item here, which is what the
    // reference implementation does — it renders the link in its own `<p>`,
    // outside the `<ul>`.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** removed API.\n\
         ```\nsample\n```\n\
         [migration guide](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link outside the item cannot supply that item's guide link",
    );
}

#[test]
fn migration_guide_gate_does_not_count_empty_inline_tags_as_content() {
    // `<span></span>` has source text but renders nothing, so a required
    // section holding only it is empty to a reader — the same hollow-guide
    // failure as an HTML comment or a bare reference definition.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace("Why this release breaks.", "<span></span>");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "tag-only inline HTML renders nothing\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_keeps_prose_wrapped_in_inline_tags() {
    // The counterpart: stripping tags must not strip the words between them,
    // or a guide that marks up its prose starts failing as empty.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "Why <em>this</em> release breaks, in <code>detail</code>.",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "marked-up prose is still prose\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_counts_a_breaking_marker_in_a_plain_paragraph() {
    // The convention is the inline marker, not the bullet. A section that
    // declares a break in an ordinary paragraph produced zero entries, so it
    // could ship with no guide at all while following the documented rule.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         **Breaking:** the `page` trait method is gone.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a marked paragraph declares a break and needs a guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_a_marked_paragraph_that_links_its_guide() {
    // And it must be satisfiable the same way a bullet is, or the fix above
    // would just be a new way to fail with no route to green.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         **Breaking:** the `page` trait method is gone. See the\n\
         [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a marked paragraph that links its guide is covered\n{}",
        gate_report(&output),
    );
}

// --- Review round 34: paragraph state, prose under a breaking heading -----

#[test]
fn migration_guide_gate_closes_a_paragraph_at_an_interrupting_html_block() {
    // A complete raw block such as `<pre></pre>` ends the paragraph it
    // interrupts, so the standalone tag on the next line starts a block of its
    // own. Leaving the paragraph flag set kept that tag inline and let the
    // link below it count, though the reference implementation renders the
    // whole run literally.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n  \
         <pre></pre>\n  <x-widget>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an interrupting block ends the paragraph it interrupts",
    );
}

#[test]
fn migration_guide_gate_counts_prose_under_a_breaking_changes_heading() {
    // The heading is a documented alternative to the inline marker, so prose
    // beneath it declares a break without repeating the token. Requiring the
    // token there left a section able to ship with no guide at all.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         Removed the `page` trait method.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "prose under the heading declares a break and needs a guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_treat_a_breaking_section_note_as_an_entry() {
    // The counterpart, and the shape this repository's own changelog uses: a
    // block quote introducing the section is an aside, not a declaration, so
    // it must not demand a guide link of its own. Without this the fix above
    // would fail the real CHANGELOG.md.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         > Backfilled from the guide, which shipped with the release.\n\n\
         - **db:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a block quote note is not a breaking entry\n{}",
        gate_report(&output),
    );
}

// --- Review round 35: image labels, definitions in a paragraph ------------

#[test]
fn migration_guide_gate_ignores_a_link_nested_in_an_image_label() {
    // CommonMark flattens a link inside an image label into alt text — the
    // reference implementation renders `alt="diagram showing migration
    // guide"` and no anchor. The image guard only checked the candidate's own
    // bracket, so a nested one slipped through.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. \
           ![diagram showing [migration guide](docs/migrations/0.7.0.md)](diagram.png)\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "alt text is not a link the reader can follow",
    );
}

#[test]
fn migration_guide_gate_ignores_a_definition_continuing_a_paragraph() {
    // A link reference definition cannot interrupt a paragraph: following
    // ordinary text it is literal, so the reference above it never resolves
    // and the entry has no clickable guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n  \
         more prose here.\n  [upgrade]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a definition cannot interrupt a paragraph",
    );
}

#[test]
fn migration_guide_gate_collects_consecutive_link_definitions() {
    // The counterpart, and the regression the paragraph rule invites: a run of
    // definitions is normal markdown. Only the *first* follows a blank line,
    // so treating a definition as opening a paragraph would swallow the rest.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [other]: https://example.invalid\n\
         [upgrade]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a definition does not open a paragraph\n{}",
        gate_report(&output),
    );
}

// --- Review round 36: definition parsing, unmarked prose ------------------

#[test]
fn migration_guide_gate_rejects_a_definition_with_an_unterminated_title() {
    // CommonMark creates no definition when the title never closes — the whole
    // line renders as a paragraph. Truncating at the first whitespace recorded
    // a destination that does not exist, so the reference above resolved to a
    // link the reader never gets.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [upgrade]: docs/migrations/0.7.0.md \"unterminated\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a malformed title means there is no definition at all",
    );
}

#[test]
fn migration_guide_gate_accepts_a_definition_split_across_lines() {
    // The destination may sit on the line below the label, and the title on
    // the line below that. Both render as real links, so rejecting either
    // blocks CI on valid markdown.
    for tail in [
        "[upgrade]:\n    docs/migrations/0.7.0.md\n",
        "[upgrade]: docs/migrations/0.7.0.md\n\"why it broke\"\n",
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            format!(
                "# Changelog\n\n\
                 ## [0.7.0] - 2026-09-01\n\n\
                 ### Changed\n\n\
                 - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n{tail}"
            ),
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert!(
            output.status.success(),
            "a definition may span lines\n{}",
            gate_report(&output),
        );
    }
}

#[test]
fn migration_guide_gate_lints_an_unmarked_break_in_a_plain_paragraph() {
    // The unmarked-break lint only ever saw list items, so prose describing a
    // break without the marker was never examined — the same shape of hole as
    // marked paragraphs never becoming entries.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         This is a breaking API rename.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "prose describing a break without the marker must be caught\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_leaves_ordinary_prose_paragraphs_alone() {
    // The counterpart: admitting paragraphs to the lint must not turn every
    // sentence in the changelog into an entry demanding a guide.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         This release focuses on observability and developer experience.\n\n\
         ### Changed\n\n\
         - **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "prose that describes no break declares nothing\n{}",
        gate_report(&output),
    );
}

// --- Review round 37: comments interrupt paragraphs, deep headings --------

#[test]
fn migration_guide_gate_closes_a_paragraph_at_an_interrupting_comment() {
    // A complete HTML comment is a raw block that interrupts a paragraph, so
    // the standalone tag after it opens a block of its own and everything
    // inside is literal — the reference implementation renders the link as
    // text. Paragraph state was read from the line *before* the comment was
    // removed, so the comment looked like ordinary prose.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n  \
         <!-- note -->\n  <x-widget>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a comment interrupts the paragraph it follows",
    );
}

#[test]
fn migration_guide_gate_ends_an_entry_at_a_deeper_outdented_heading() {
    // `##` and `###` flushed the open item and the deeper levels did not, so a
    // link under an outdented `####` was appended to the entry above it even
    // though it renders outside that list entirely.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\
         #### Notes\n\
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link under a later heading is not that entry's link",
    );
}

#[test]
fn migration_guide_gate_does_not_read_a_deep_heading_as_an_entry() {
    // The counterpart to admitting paragraphs to the lint: a `####` heading is
    // a heading, not prose, so wording like "Breaking down latency" in one
    // must not be linted as an unmarked break.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         #### Breaking down request latency\n\n\
         - **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a heading is not an entry\n{}",
        gate_report(&output),
    );
}

// --- Review round 38: escaped bangs, block-quoted fences ------------------

#[test]
fn migration_guide_gate_accepts_a_link_after_an_escaped_bang() {
    // `\!` renders as a literal `!` and leaves the link clickable — the
    // reference implementation emits `!<a href=…>`. Treating any preceding
    // `!` as an image rejected a link that works.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. \\![migration guide](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an escaped bang is a literal `!`, not an image\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_opens_a_fence_inside_a_block_quote() {
    // A block-quote marker sits in front of the fence, so fence detection has
    // to look past it. It did not, and the link displayed inside the quoted
    // sample counted although CommonMark renders it as code.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n  \
         > ~~~\n  > [migration guide](docs/migrations/0.7.0.md)\n  > ~~~\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link inside a quoted fence renders as code",
    );
}

#[test]
fn migration_guide_gate_still_treats_a_block_quote_note_as_an_aside() {
    // The counterpart, and the reason quote markers are stripped only for
    // block detection: an aside under `### Breaking Changes` must stay an
    // aside. Stripping the marker everywhere would make it an entry and fail
    // this repository's own changelog.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         > Backfilled from the guide, which shipped with the release.\n\n\
         - **db:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a quoted note is not a breaking entry\n{}",
        gate_report(&output),
    );
}

// --- Review round 39: link syntax inside a link title ---------------------

#[test]
fn migration_guide_gate_ignores_link_syntax_inside_a_link_title() {
    // A parsed link's title is tooltip text, so link syntax written inside it
    // renders as characters — the reference implementation emits
    // `title="[migration guide](…)"` and no second anchor. Scanning every `]`
    // independently re-read the title as a candidate of its own.
    //
    // Stronger than the round-25 case, which only put a bare *path* in the
    // title: this one puts a complete, well-formed link there.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. \
           [release note](https://example.invalid \
           \"[migration guide](docs/migrations/0.7.0.md)\")\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link written inside a title is tooltip text, not a link",
    );
}

#[test]
fn migration_guide_gate_still_finds_a_guide_link_after_another_link() {
    // The counterpart: skipping past a parsed link must resume scanning after
    // it, not abandon the rest of the line. A guide link following an
    // unrelated one still has to be found.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, see the \
           [release note](https://example.invalid \"context\") and the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "scanning resumes after a parsed link\n{}",
        gate_report(&output),
    );
}

// --- Review round 40: indented code, introductory prose -------------------

#[test]
fn migration_guide_gate_does_not_read_indented_code_as_a_breaking_entry() {
    // Four spaces makes an indented code block: literal text, so neither the
    // words nor the link inside it render. `block_body` strips only the three
    // markdown allows on a block construct, so the leftover space is exactly
    // the signal that this is code.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n    \
         Removed API, in a sample with no link of its own.\n\n\
         - **db:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a code sample is not an entry, and its link is not clickable\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_allows_introductory_prose_before_a_breaking_list() {
    // The conventional shape of the section: a sentence introducing the list
    // beneath it. The introduction is not itself an entry, so demanding its
    // own guide link rejects a correct changelog.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         The following changes require action:\n\n\
         - **db:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an introduction to a list is not an entry of its own\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_still_counts_a_prose_only_breaking_section() {
    // And the counterpart that must survive it: with no list at all, the prose
    // *is* the entry. This is the round-34 hole, and relaxing introductions
    // must not reopen it.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         Removed the `page` trait method.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "prose is the entry when the section has no list\n{}",
        gate_report(&output),
    );
}

// --- Review round 41: indented code in the index --------------------------

#[test]
fn migration_guide_gate_ignores_an_index_entry_in_indented_code() {
    // Four spaces at top level is an indented code block, so the bullet
    // renders as code and the guide is not linked from the Index at all —
    // which is the only route a reader has to it.
    //
    // The indented entry comes *first* on purpose: four spaces after an open
    // bullet is a nested list item, which does index the guide. Only with no
    // list open is it code. The next test pins the other half.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("docs/migrations/0.7.0.md"),
        valid_migration_guide("0.7.0"),
    )
    .expect("guide");
    std::fs::write(
        tmp.path().join("docs/migrations/README.md"),
        "# Migration Guides\n\n\
         ## Index\n\n    \
         - [`0.7.0.md`](0.7.0.md)\n\n\
         - [`next.md`](next.md)\n",
    )
    .expect("index");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an index bullet rendered as code does not index the guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_a_nested_index_entry() {
    // The counterpart: the same four spaces *under a bullet* is a nested list
    // item, not code, and indexes the guide perfectly well. The difference is
    // the enclosing item content column, which is why the check measures from
    // it rather than from column 0.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("docs/migrations/0.7.0.md"),
        valid_migration_guide("0.7.0"),
    )
    .expect("guide");
    std::fs::write(
        tmp.path().join("docs/migrations/README.md"),
        "# Migration Guides\n\n\
         ## Index\n\n\
         - [`next.md`](next.md)\n    \
         - [`0.7.0.md`](0.7.0.md)\n",
    )
    .expect("index");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a nested index bullet is a real link\n{}",
        gate_report(&output),
    );
}

// --- Review round 42: prose after a list is still an entry ----------------

#[test]
fn migration_guide_gate_counts_breaking_prose_that_follows_a_list() {
    // Holding prose until the section ends was meant to spare an *introduction*
    // to a list. Prose that comes after the list introduces nothing — it is a
    // second declaration, and discarding it let a break ship with no link.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         - **db:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n\n\
         The `page` trait method is also gone, with no link of its own.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "prose after the list is a declaration, not an introduction\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_counts_a_linked_declaration_after_a_list() {
    // And it has to be satisfiable: the same paragraph carrying its own link
    // passes, so the rule adds a check rather than an unavoidable failure.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Breaking Changes\n\n\
         - **db:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n\n\
         The `page` trait method is also gone. See the\n\
         [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a linked declaration after the list is covered\n{}",
        gate_report(&output),
    );
}

// --- Review round 43: boolean attributes in a type-7 opener ---------------

#[test]
fn migration_guide_gate_opens_a_type_seven_tag_with_a_boolean_attribute() {
    // `<x-widget disabled>` is a complete tag, so it opens a raw block and the
    // link below it renders literally. The opener accepted attribute-free tags
    // and tags carrying `name=value`, and a valueless attribute is neither.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <x-widget disabled>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a valueless attribute still makes a complete tag",
    );
}

#[test]
fn migration_guide_gate_does_not_open_a_block_on_nested_angle_brackets() {
    // The counterpart that keeps the opener from swallowing prose: a generic
    // standing alone on its line is not a tag, and treating it as one would
    // hide the entries the unmarked-break lint has to read.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n\n  \
         <Vec<Route>>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "nested angle brackets are not a complete tag\n{}",
        gate_report(&output),
    );
}

// --- Portability: awk regex literals across implementations ---------------

#[test]
fn shell_scripts_keep_slashes_out_of_awk_bracket_expressions() {
    // The one-true-awk that ships as macOS `/usr/bin/awk` ends a regex literal
    // at a `/` inside `[...]` and rejects the whole program:
    //
    //   awk: nonterminated character class ^</?(address|articl
    //
    // gawk and mawk accept it, so `[[:space:]/>]` ran clean on Linux and took
    // out every migration-gate test on macOS at once. Write the slash as an
    // alternative — `([[:space:]>]|\/|$)` — and it parses everywhere.
    let root = workspace_root();
    let mut scripts = Vec::new();
    shell_scripts(&root.join("scripts"), &mut scripts);
    assert!(!scripts.is_empty(), "expected scripts/ to contain scripts");

    let mut offenders = Vec::new();
    for path in scripts {
        let body = std::fs::read_to_string(&path).expect("read script");
        for (idx, line) in body.lines().enumerate() {
            // A POSIX class opens a bracket expression; a `/` before the `]`
            // that closes it is the construct macOS cannot parse.
            let Some(open) = line.find("[[:") else {
                continue;
            };
            let rest = &line[open + 3..];
            let Some(class_end) = rest.find(":]") else {
                continue;
            };
            let after_class = &rest[class_end + 2..];
            let bracket_end = after_class.find(']').unwrap_or(after_class.len());
            if after_class[..bracket_end].contains('/') {
                offenders.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "macOS awk cannot parse a `/` inside a bracket expression:\n{}",
        offenders.join("\n"),
    );
}

// --- Review rounds 44-45: container blanks, fence state, fragments --------

#[test]
fn migration_guide_gate_ends_a_quoted_html_block_at_a_quoted_blank_line() {
    // Inside a block quote a blank line is written `>`, not whitespace, so
    // testing the raw line for blankness never ends the block and the link
    // after it was discarded — the reference implementation renders it as a
    // real anchor inside the quote.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         > <div>\n  > x\n  >\n  \
         > See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a quoted blank line ends the quoted block\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_closes_a_paragraph_at_an_empty_fence() {
    // An empty fenced block still ends the paragraph before it. Neither
    // delimiter reached the branch that clears the flag, so a standalone tag
    // afterwards stayed inline and the headings inside the raw block it should
    // have opened were read as the guide's own structure.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = "# Migrating from Autumn `0.x` to `0.7.0`\n\n\
         Intro prose.\n\n\
         ```\n```\n\
         <x-widget>\n\
         ## At a glance\n\n\
         - **New version:** `autumn-web 0.7.0`\n\n\
         ## Summary\n\n\
         Why.\n\n\
         ## Before you start\n\n\
         Pin.\n\n\
         ## Breaking changes\n\n\
         Before / after.\n\n\
         ## How to verify\n\n\
         Run `cargo check`.\n\n\
         ### Guide-only upgrade walkthrough\n\n\
         - **Status:** performed 2026-01-01\n";
    write_fixture_guide(&tmp, "0.7.0", guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "headings inside a raw block are not the guide's structure\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_keeps_an_entry_open_across_an_indented_fence() {
    // A fence indented to the item content column belongs to the item, so the
    // link after it is still the entry's link. Fence delimiters render as
    // nothing, so measuring their indentation from the *visible* text always
    // read zero and flushed the entry early.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed.\n  \
         ```\n  sample\n  ```\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an indented fence stays inside its list item\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_a_fragment_link_to_the_guide() {
    // A deep link into the relevant section is a better link, not a worse one:
    // the destination still resolves to the required guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md#api-changes).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a fragment still points at the guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_count_a_multiline_definition_as_content() {
    // A definition whose destination sits on the next line renders nothing at
    // all, so a section holding only one is empty. The opener alone is not a
    // definition, which is exactly why the emptiness check missed it.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "[only]:\n    https://example.invalid",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a definition split across lines still renders nothing\n{}",
        gate_report(&output),
    );
}

// --- Review round 46: marker padding, suppression in link metadata --------

#[test]
fn migration_guide_gate_caps_list_marker_padding_at_four_spaces() {
    // CommonMark takes only one space as marker padding once five or more
    // follow, leaving four to make the content an indented code block. The
    // reference implementation renders the whole entry inside `<pre><code>`,
    // so neither the marker nor the link is real — consuming every space made
    // a code sample satisfy coverage.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         -     **Breaking:** example [guide](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    let listed = bash_command()
        .args(["scripts/check-migration-guides.sh", "--list"])
        .current_dir(tmp.path())
        .output()
        .expect("run --list");
    let inventory = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(
        output.status.success(),
        "a code sample declares nothing, so nothing is required\n{}",
        gate_report(&output),
    );
    assert!(
        inventory
            .lines()
            .any(|line| line.starts_with("0.7.0") && line.ends_with('0')),
        "the section must report zero breaking entries:\n{inventory}",
    );
}

#[test]
fn migration_guide_gate_keeps_four_space_padding_a_real_entry() {
    // The boundary on the other side: four spaces is still marker padding, so
    // this is an ordinary entry and its missing guide link must be caught.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         -    **Breaking:** renamed, with no link of its own.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "four spaces is padding, so this is a real breaking entry",
    );
}

#[test]
fn migration_guide_gate_ignores_a_suppression_inside_a_link_title() {
    // The suppression has to be a real HTML comment. Inside a link title it is
    // tooltip metadata — the reference implementation renders it as a `title`
    // attribute — so it must not silence the unmarked-break lint.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - This is a breaking API rename \
           [details](https://example.invalid \
           \"<!-- migration-guide-gate: tooltip -->\")\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a tooltip is not a suppression comment",
    );
}

#[test]
fn migration_guide_gate_still_honours_a_real_suppression_beside_a_link() {
    // The counterpart: masking link metadata must not blind the gate to a
    // genuine suppression written next to a link, which is how the entry
    // describing this gate is written in the real changelog.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **release:** the gate fails when a section declares a breaking \
           change with no guide, see [the docs](https://example.invalid). \
           <!-- migration-guide-gate: describes the gate itself -->\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a real suppression beside a link still applies\n{}",
        gate_report(&output),
    );
}

// --- Review round 47: link targets, ordered-marker interruption -----------

#[test]
fn migration_guide_gate_ignores_the_word_breaking_in_a_link_target() {
    // A destination is metadata, not rendered prose, so a URL that happens to
    // contain "breaking" does not describe a break. The lint read it and
    // rejected an ordinary documentation entry.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **api:** an addition, see \
           [compatibility note](https://example.invalid/breaking-changes).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a word in a URL is not prose describing a break\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_lets_a_non_one_ordered_marker_continue_a_paragraph() {
    // Only a `1.` ordered marker may interrupt a paragraph. After a marked
    // *paragraph*, `2.` is a lazy continuation — the reference implementation
    // renders both lines inside one `<p>` — so its link is the entry's link.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         **Breaking:** renamed.\n\
         2. [migration guide](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "`2.` cannot interrupt a paragraph, so it continues it\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_keeps_a_non_one_marker_a_sibling_of_a_list_item() {
    // The counterpart that stops the rule spreading: after a *list item*,
    // `2.` is a sibling item rather than a continuation, so its link belongs
    // to that new item and not to the breaking entry above it. Verified
    // against the reference implementation, which emits a separate `<ol>`.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **Breaking:** renamed, with no link of its own.\n\
         2. [migration guide](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "after a list item a numbered marker opens a sibling",
    );
}

#[test]
fn migration_guide_gate_lets_a_one_marker_interrupt_a_paragraph() {
    // And the other boundary: `1.` *may* interrupt a paragraph, so it starts
    // its own list and takes its link with it.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         **Breaking:** renamed, with no link of its own.\n\
         1. [migration guide](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "`1.` interrupts the paragraph and takes its link with it",
    );
}

// --- Review round 48: definitions, thematic breaks, tag validity ----------

#[test]
fn migration_guide_gate_does_not_lint_a_reference_definition_as_prose() {
    // A definition renders nothing, so it declares nothing — but it was
    // becoming a paragraph entry, and a label containing the word "breaking"
    // was then reported as an unmarked break, blocking a valid changelog.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [Unreleased]\n\n\
         [breaking-reference]: https://example.invalid/note\n\n\
         - **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a link definition is not prose describing a break\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_ends_an_entry_at_a_thematic_break() {
    // `---` ends the list item, so the link after it renders in its own
    // paragraph outside the list and cannot be that entry's guide link.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\
         ---\n\
         [migration guide](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link past a thematic break is outside the entry",
    );
}

#[test]
fn migration_guide_gate_does_not_open_a_block_on_a_malformed_tag() {
    // `<x =>` is not a well-formed tag, so CommonMark renders it as literal
    // text and the bullet below stays a real list item. Accepting anything
    // tag-shaped swallowed that entry and reported the section as empty.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <x =>\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a malformed tag must not hide the entry beneath it",
    );
}

#[test]
fn migration_guide_gate_still_opens_a_block_on_a_well_formed_tag() {
    // The counterpart: tightening tag validation must not stop recognising
    // the real thing, in any of its shapes.
    for tag in [
        "<x-widget>",
        "<x-widget disabled>",
        "<x-widget a=\"1\" b>",
        "</x-widget>",
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            format!(
                "# Changelog\n\n\
                 ## [0.7.0] - 2026-09-01\n\n\
                 ### Changed\n\n\
                 - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
                 {tag}\n  \
                 See the [migration guide](docs/migrations/0.7.0.md).\n"
            ),
        )
        .expect("changelog");

        assert!(
            !run_migration_gate(tmp.path()).status.success(),
            "`{tag}` is a complete tag and opens a raw block",
        );
    }
}

// --- Review round 49: only punctuation is backslash-escapable -------------

#[test]
fn migration_guide_gate_keeps_a_backslash_before_a_letter_in_a_destination() {
    // Only ASCII punctuation can be backslash-escaped, so a backslash before a
    // letter stays in the destination — the reference implementation renders
    // `href="docs/migrations/0.7.0.m%5Cd"`. Dropping every backslash made a
    // link to a nonexistent path satisfy the guide requirement.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.m\\d).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a preserved backslash means this points somewhere else",
    );
}

#[test]
fn migration_guide_gate_resolves_an_escaped_punctuation_destination() {
    // The counterpart: punctuation *is* escapable, so `0.7.0\.md` renders as
    // `0.7.0.md` and does point at the guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0\\.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an escaped dot still resolves to the guide\n{}",
        gate_report(&output),
    );
}

// --- Review round 50: heading levels, separators, label validity ----------

#[test]
fn migration_guide_gate_reads_seven_hashes_as_prose() {
    // ATX headings stop at six markers, so a seven-hash line is an ordinary
    // paragraph. Skipping it as a heading took it out of the unmarked-break
    // lint entirely.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         ####### This is a breaking API rename\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "seven hashes render as prose and must reach the lint",
    );
}

#[test]
fn migration_guide_gate_still_reads_six_hashes_as_a_heading() {
    // The boundary on the other side: six is still a heading, so wording in
    // one is not prose and must not be linted.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         ###### Breaking down request latency\n\n\
         - **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "six hashes is a heading, not prose\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_count_a_thematic_break_as_content() {
    // A separator renders as a rule and carries no migration instructions, so
    // a required section holding only one is a stub.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace("Why this release breaks.", "---");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a rule is not migration guidance\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_rejects_a_definition_label_with_a_stray_bracket() {
    // An unescaped bracket inside a label means CommonMark creates no
    // definition at all — both lines render as plain paragraphs — so the
    // reference above it resolves to nothing.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][up[grade].\n\n\
         [up[grade]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an invalid label defines nothing",
    );
}

// --- Review round 51: comment openers inside link metadata ----------------

#[test]
fn migration_guide_gate_ignores_a_comment_opener_inside_a_link_title() {
    // A `<!--` in a link title is tooltip text, not an HTML comment: the
    // reference implementation renders it as a `title` attribute and the entry
    // below it as a normal bullet. Reading it as a comment both swallowed the
    // rest of the file and reported an unclosed comment.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md \"<!-- tooltip\").\n\n\
         - **api:** a later entry.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a tooltip is not a comment, and the link is real\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_still_reads_a_comment_beside_a_link() {
    // The counterpart: masking link metadata must leave a genuine comment on
    // the same line readable, which is how commented-out entries are written.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **api:** an addition, see [notes](https://example.invalid). \
           <!-- **Breaking:** parked, not shipped -->\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a marker inside a real comment declares nothing\n{}",
        gate_report(&output),
    );
}

// --- Review round 52: `/>` as a unit, empty list items --------------------

#[test]
fn migration_guide_gate_requires_a_closing_slash_bracket_to_open_type_six() {
    // A type-6 opener ends in whitespace, `>`, `/>` or end of line. A bare
    // slash is none of those, so `<div/not-a-tag` renders as a paragraph and
    // the bullet below it is a real entry — accepting `/` alone swallowed it.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <div/not-a-tag\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a bare slash does not open a block, so the entry is visible",
    );
}

#[test]
fn migration_guide_gate_still_opens_type_six_on_a_self_closing_tag() {
    // The counterpart: `<div/>` *is* an opener, so the entry beneath it is
    // literal and supplies nothing.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <div/>\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "`<div/>` opens a block and the link inside it is literal",
    );
}

#[test]
fn migration_guide_gate_does_not_count_an_empty_list_item_as_content() {
    // A bare `-` renders as an empty `<li>` and carries no instructions, so a
    // required section holding only one is a stub.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace("Why this release breaks.", "-");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an empty bullet is not migration guidance\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_counts_a_bullet_that_carries_words() {
    // The counterpart: a bullet with content is content. Excluding empty ones
    // must not exclude the ordinary case, which is how guides are written.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "- The repository trait changed.",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a bullet with words in it is content\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_gate_scripts_pin_a_byte_oriented_locale() {
    // macOS awk decodes multibyte characters according to the ambient locale
    // and aborts the program when a byte does not decode:
    //
    //   awk: towc: multibyte conversion failure on: '<byte>'
    //
    // CHANGELOG.md contains an en dash, so the gate exited 2 on the real
    // repository without reading a line. Neither awk available here reproduces
    // it, and the Linux CI job cannot either, so the requirement is pinned as
    // a lint rather than as behaviour.
    let root = workspace_root();
    let script = std::fs::read_to_string(root.join("scripts/check-migration-guides.sh"))
        .expect("read migration gate");
    assert!(
        script.contains("export LC_ALL=C"),
        "the gate must pin a byte-oriented locale for awk",
    );
}

#[test]
fn migration_guide_gate_reads_a_changelog_containing_non_ascii() {
    // The behaviour that lint protects: an en dash in an entry must not stop
    // the gate reading the file, whatever locale the caller has set.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** ships every 2\u{2013}4 weeks \u{2014} renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = bash_command()
        .arg("scripts/check-migration-guides.sh")
        .current_dir(tmp.path())
        .env("LC_ALL", "C")
        .output()
        .expect("run migration-guide gate");
    assert!(
        output.status.success(),
        "non-ASCII prose must not stop the gate\n{}",
        gate_report(&output),
    );
}

// --- Review round 53: attributed inline tags, definitions as links --------

#[test]
fn migration_guide_gate_strips_attributed_inline_tags_from_content() {
    // `<span disabled></span>` renders nothing a reader can use, but tag
    // stripping only removed bare tags, so the attributed opener survived and
    // marked the section populated.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0")
        .replace("Why this release breaks.", "<span disabled></span>");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an attributed empty tag renders no guidance\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_read_a_definition_as_its_own_link() {
    // A definition inside the item renders no anchor at all, so the entry has
    // no clickable guide. Scanning the definition line for links resolved its
    // own label as a shortcut reference and satisfied the entry with it.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         [upgrade]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a definition is not a link the reader can follow",
    );
}

#[test]
fn migration_guide_gate_still_resolves_a_reference_the_entry_actually_uses() {
    // The counterpart: the definition still *defines*. An entry that uses the
    // reference is linked, which is the round-30 behaviour and must survive.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [upgrade]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a reference the entry uses still resolves\n{}",
        gate_report(&output),
    );
}

// --- Review round 54: definition escapes, line-initial comments -----------

#[test]
fn migration_guide_gate_unescapes_punctuation_in_a_definition_destination() {
    // Punctuation is escapable, so `docs/migrations/0.7.0\.md` resolves to the
    // guide. Inline destinations learned this in round 49; the definitions
    // collector kept the backslash and rejected a link that renders.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [upgrade]: docs/migrations/0.7.0\\.md\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an escaped dot in a definition still resolves\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_treats_a_line_initial_comment_as_a_raw_block() {
    // A line beginning with `<!--` is a type-2 HTML block, so the whole line
    // is raw — the reference implementation emits it verbatim, link syntax and
    // all. Stripping just the comment handed the rest back as markdown and
    // counted a link that renders as text.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <!-- note --> **Breaking:** renamed. \
         [guide](docs/migrations/0.7.0.md)\n\n\
         - **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a raw line declares nothing, so nothing is required\n{}",
        gate_report(&output),
    );
    let listed = bash_command()
        .args(["scripts/check-migration-guides.sh", "--list"])
        .current_dir(tmp.path())
        .output()
        .expect("run --list");
    let inventory = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(
        inventory
            .lines()
            .any(|line| line.starts_with("0.7.0") && line.ends_with('0')),
        "the raw line must not count as a breaking entry:\n{inventory}",
    );
}

#[test]
fn migration_guide_gate_keeps_an_inline_comment_inline() {
    // The counterpart: a comment that does not start the line is inline HTML,
    // the paragraph is ordinary markdown, and its guide link is real.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. <!-- note --> See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an inline comment leaves the rest of the line markdown\n{}",
        gate_report(&output),
    );
}

// --- Review round 55: block body computation, comment re-masking ----------

#[test]
fn migration_guide_gate_keeps_a_multi_line_html_block_open() {
    // A type-6 block runs to the next blank line, however many lines it holds.
    // Testing blankness against a body that had not been computed yet closed
    // the block after one line and handed the rest back as markdown, so a
    // literal bullet inside it became a real entry.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <div>\n\
         literal first line\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n\
         - **api:** an addition.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "everything up to the blank line is literal, so nothing declares\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_remasks_link_metadata_after_a_comment() {
    // Once a completed comment is removed, the rest of the line is rescanned —
    // and a `<!--` in a link title further along is still tooltip text. Masking
    // only code spans on the rescan reported an unclosed comment and rejected
    // a valid changelog.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **api:** an addition. <!-- note --> \
           [details](https://example.invalid \"<!-- tooltip\")\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a tooltip after a comment is still a tooltip\n{}",
        gate_report(&output),
    );
}

// --- Review round 56: multi-line code spans, `=` in attribute values ------

#[test]
fn migration_guide_gate_carries_code_span_state_across_lines() {
    // A code span may cross a newline. When it does, a `<!--` inside it is
    // literal code — the reference implementation renders
    // `<code>a span and &lt;!-- still code</code>` — and the entry below is a
    // real declaration. Masking spans one line at a time opened a comment and
    // swallowed it.
    //
    // The `<!--` must not start its line: one that does opens a type-2 block
    // legitimately, ends the paragraph, and leaves the backtick literal. That
    // shape is not this bug, and the gate already handles it.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **api:** an addition with `a span\n  \
         and <!-- still code` here.\n\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n",
    )
    .expect("changelog");

    // Asserted on the inventory, not the exit status: the unclosed-comment
    // guard fires on this input too, so the gate exits non-zero either way and
    // a status-only assertion passes while the entry is still being swallowed.
    let listed = bash_command()
        .args(["scripts/check-migration-guides.sh", "--list"])
        .current_dir(tmp.path())
        .output()
        .expect("run --list");
    let inventory = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(
        inventory
            .lines()
            .any(|line| line.starts_with("0.7.0") && line.ends_with('1')),
        "the entry below a multi-line code span is a real declaration:\n{inventory}",
    );
}

#[test]
fn migration_guide_gate_rejects_an_equals_in_an_unquoted_attribute() {
    // CommonMark excludes `=` from unquoted attribute values, so `<x foo=a=b>`
    // is not a tag: it renders as a paragraph and the bullet below it is a
    // real entry. Consuming the second `=` opened a block and hid it.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <x foo=a=b>\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a malformed attribute value means this is not a tag",
    );
}

#[test]
fn migration_guide_gate_still_opens_a_block_on_a_quoted_equals() {
    // The counterpart: `=` inside a *quoted* value is ordinary, so this is a
    // tag and does open a block.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <x foo=\"a=b\">\n  \
         See the [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a quoted `=` is fine, so the block opens and the link is literal",
    );
}

// --- Review round 57: definition destinations, index definitions ----------

#[test]
fn migration_guide_gate_rejects_an_unbalanced_paren_in_a_definition() {
    // An unbalanced `(` makes the destination invalid, so CommonMark creates
    // no definition — both lines render as paragraphs — and the reference
    // above resolves to nothing.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [upgrade]: docs/migrations/0.7.0.md#(unterminated\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an invalid destination defines nothing",
    );
}

#[test]
fn migration_guide_gate_still_accepts_balanced_parens_in_a_definition() {
    // The counterpart: parentheses are legal in a destination when balanced,
    // so this one still resolves.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [upgrade]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a well-formed definition still resolves\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_index_a_guide_by_definition_alone() {
    // A definition renders nothing, so an Index holding only one has no
    // clickable entry — but its own label was resolving as a shortcut
    // reference and reporting the guide as indexed. The same fix the entry
    // scan got in round 53, in the parser that was still missing it.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("docs/migrations/0.7.0.md"),
        valid_migration_guide("0.7.0"),
    )
    .expect("guide");
    std::fs::write(
        tmp.path().join("docs/migrations/README.md"),
        "# Migration Guides\n\n\
         ## Index\n\n\
         - [`next.md`](next.md)\n\n\
         [0.7.0.md]: 0.7.0.md\n",
    )
    .expect("index");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a definition is not an index entry a reader can click\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_treats_a_lowercase_declaration_as_a_raw_block() {
    // Declared behaviour, not a fix. CommonMark since 0.30 opens a type-4
    // block on `<!` plus any ASCII letter, and the reference implementation
    // renders `<!foo` and `<!FOO` identically — both raw. A review round
    // asked for uppercase-only; this pins the spec-current behaviour so the
    // question does not get reopened from memory.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         <!foo\n\
         - **db:** **Breaking:** renamed, with no guide anywhere.\n\
         >\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a lowercase declaration opens a block, so the bullet is literal\n{}",
        gate_report(&output),
    );
}

// --- Review round 58: escaped title delimiters, whitespace entities -------

#[test]
fn migration_guide_gate_rejects_a_definition_with_an_escaped_title_delimiter() {
    // `"unterminated\"` leaves the title unclosed, so CommonMark creates no
    // definition — both lines render as paragraphs. Finding the closer with a
    // plain search accepted the escaped quote and recorded a definition that
    // does not exist.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [upgrade]: docs/migrations/0.7.0.md \"unterminated\\\"\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an escaped quote does not close the title, so nothing is defined",
    );
}

#[test]
fn migration_guide_gate_still_accepts_a_closed_definition_title() {
    // The counterpart: a title that does close leaves the definition intact.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
         [upgrade]: docs/migrations/0.7.0.md \"why it broke\"\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a closed title still leaves a working definition\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_count_whitespace_entities_as_content() {
    // `&nbsp;` renders as a blank paragraph — the reference implementation
    // emits `<p> </p>` — so a required section holding only one carries no
    // instructions, whatever the source text looks like.
    for entity in ["&nbsp;", "&#160;", "&#xa0;"] {
        let tmp = migration_gate_fixture("0.7.0");
        let guide = valid_migration_guide("0.7.0").replace("Why this release breaks.", entity);
        write_fixture_guide(&tmp, "0.7.0", &guide);
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** renamed. See the \
               [migration guide](docs/migrations/0.7.0.md).\n",
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert!(
            !output.status.success(),
            "`{entity}` renders blank and is not guidance\n{}",
            gate_report(&output),
        );
    }
}

#[test]
fn migration_guide_gate_counts_prose_containing_an_entity() {
    // The counterpart: an entity beside real words is ordinary writing, and
    // the section is populated by the words.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0")
        .replace("Why this release breaks.", "Why&nbsp;this release breaks.");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an entity inside prose does not make the prose vanish\n{}",
        gate_report(&output),
    );
}

// --- Review round 59: markers in metadata, char refs, empty links ---------

#[test]
fn migration_guide_gate_ignores_a_marker_inside_a_link_destination() {
    // The marker renders as part of a URL, not as changelog prose — the
    // reference implementation emits it inside `href`. Reading it there turned
    // an ordinary entry into a declaration and demanded a guide for it.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **api:** an addition, see \
           [details](https://example.invalid/**Breaking:**/notes).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a marker in a URL declares nothing\n{}",
        gate_report(&output),
    );
    let listed = bash_command()
        .args(["scripts/check-migration-guides.sh", "--list"])
        .current_dir(tmp.path())
        .output()
        .expect("run --list");
    let inventory = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(
        inventory
            .lines()
            .any(|line| line.starts_with("0.7.0") && line.ends_with('0')),
        "the section must report zero breaking entries:\n{inventory}",
    );
}

#[test]
fn migration_guide_gate_resolves_a_character_reference_in_a_destination() {
    // `next&#x2e;md` renders as `next.md`, so the link points at the guide.
    // Comparing the encoded source text rejected a link that works.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0&#x2e;7&#46;0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a character reference resolves to the guide path\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_count_an_empty_link_as_content() {
    // `[](url)` renders as an anchor with no text, so a required section
    // holding only one shows the reader nothing.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0")
        .replace("Why this release breaks.", "[](https://example.invalid)");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an anchor with no text is not guidance\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_counts_a_link_that_has_text() {
    // The counterpart: a link with a label is content, which is how guides
    // point at upstream notes.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "See [the upstream notes](https://example.invalid).",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a labelled link is visible content\n{}",
        gate_report(&output),
    );
}

// --- Review round 60: markers in HTML attributes --------------------------

#[test]
fn migration_guide_gate_ignores_a_marker_inside_an_html_attribute() {
    // The marker renders inside a `title` attribute, not as prose — the
    // reference implementation emits it as an attribute value. Reading it
    // there turned a documentation entry into a declaration demanding a guide.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **docs:** a documentation change. \
           <span title=\"**Breaking:** internal term\">tooltip</span>\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a marker in an attribute declares nothing\n{}",
        gate_report(&output),
    );
    let listed = bash_command()
        .args(["scripts/check-migration-guides.sh", "--list"])
        .current_dir(tmp.path())
        .output()
        .expect("run --list");
    let inventory = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(
        inventory
            .lines()
            .any(|line| line.starts_with("0.7.0") && line.ends_with('0')),
        "the section must report zero breaking entries:\n{inventory}",
    );
}

#[test]
fn migration_guide_gate_does_not_lint_breaking_prose_in_an_attribute() {
    // The same rule for the unmarked-break lint: wording inside an attribute
    // is metadata, so it must not be read as prose describing a break.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **docs:** a documentation change. \
           <span title=\"describes a breaking change elsewhere\">tooltip</span>\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "attribute text is not prose describing a break\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_an_indented_definition_continuation() {
    // Declared behaviour, not a fix. A review round asked for a definition
    // continuation indented four spaces to be rejected as indented code. It is
    // not: indented code cannot interrupt a paragraph, and the reference
    // implementation resolves the reference at three *and* four spaces. Pinned
    // so the question is not reopened from memory.
    for indent in ["   ", "    "] {
        let tmp = migration_gate_fixture("0.7.0");
        write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            format!(
                "# Changelog\n\n\
                 ## [0.7.0] - 2026-09-01\n\n\
                 ### Changed\n\n\
                 - **db:** **Breaking:** renamed. See the [migration guide][upgrade].\n\n\
                 [upgrade]:\n{indent}docs/migrations/0.7.0.md\n"
            ),
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert!(
            output.status.success(),
            "a continuation indented {} spaces still defines\n{}",
            indent.len(),
            gate_report(&output),
        );
    }
}

// --- Review round 61: empty block quotes, invisible links -----------------

#[test]
fn migration_guide_gate_does_not_count_an_empty_block_quote_as_content() {
    // A bare `>` renders as an empty blockquote, so a required section holding
    // only one shows the reader nothing. Quote markers were stripped for block
    // detection but not for the emptiness test.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace("Why this release breaks.", ">");
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an empty quote is not migration guidance\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_counts_a_quote_that_carries_words() {
    // The counterpart: a quote with prose in it is content, which is how the
    // real guides carry their backfill notes.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "> Backfilled from the release notes.",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a quoted note is visible content\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_rejects_an_invisible_guide_link() {
    // `[](path)` renders an anchor with no text: the destination is right and
    // there is nothing for the reader to click, so it is not the upgrade path
    // the entry has to carry.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed API. [](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an anchor with no text is not a link the reader can follow",
    );
}

#[test]
fn migration_guide_gate_rejects_an_invisible_index_entry() {
    // The same rule in the index scan: an entry nobody can see does not make
    // the guide findable.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("docs/migrations/0.7.0.md"),
        valid_migration_guide("0.7.0"),
    )
    .expect("guide");
    std::fs::write(
        tmp.path().join("docs/migrations/README.md"),
        "# Migration Guides\n\n\
         ## Index\n\n\
         - [`next.md`](next.md)\n\
         - [](0.7.0.md)\n",
    )
    .expect("index");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an invisible index entry does not index the guide\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_accepts_an_image_labelled_guide_link() {
    // The boundary the label rule must not cross: an image *is* something the
    // reader sees, so a link labelled with one is a real link.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed API. \
           [![upgrade](icon.png)](docs/migrations/0.7.0.md)\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an image label is visible, so the link counts\n{}",
        gate_report(&output),
    );
}

// --- Review round 62: continuations only follow an incomplete definition --

#[test]
fn migration_guide_gate_counts_prose_after_a_complete_definition() {
    // A definition that carries its destination is finished, so the line below
    // it is ordinary prose — the reference implementation renders `Details.`
    // as a paragraph. Treating every line after a definition as a possible
    // destination suppressed it and reported the section empty.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "[ref]: https://example.invalid\nDetails.",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "prose after a finished definition is content\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_still_suppresses_a_destination_continuation() {
    // The counterpart that must survive: an opener with no destination is
    // continued by the line below it, and the whole definition renders
    // nothing — so a section holding only that is still a stub.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "[ref]:\n    https://example.invalid",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a split definition still renders nothing\n{}",
        gate_report(&output),
    );
}

// --- Review round 63: anchored version headings, label length -------------

#[test]
fn migration_guide_gate_fails_closed_on_a_heading_that_merely_mentions_a_version() {
    // `## notes [0.3.0]` is not a release heading. Matching the version
    // anywhere in the line classified it as the out-of-scope 0.3.0 section, so
    // a breaking entry beneath it needed no guide and the gate passed — the
    // exact silent-swallow the unparseable-heading guard exists to prevent.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## notes [0.3.0]\n\n\
         - **db:** **Breaking:** renamed, with no guide.\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a heading it cannot parse is a hard error\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unreadable release heading"),
        "the failure must name the unparseable heading\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_still_reads_the_documented_heading_shapes() {
    // The counterpart: anchoring must not stop the real shapes parsing, both
    // the dated release heading and the bare unreleased one.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [Unreleased]\n\n\
         - **api:** an addition.\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "the documented heading shapes still parse\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_rejects_an_overlong_reference_label() {
    // CommonMark caps a reference label at 999 characters, so a longer one
    // creates no definition and the reference renders as literal text.
    let label = "a".repeat(1000);
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        format!(
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** renamed. See the [migration guide][{label}].\n\n\
             [{label}]: docs/migrations/0.7.0.md\n"
        ),
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an overlong label defines nothing",
    );
}

#[test]
fn migration_guide_gate_accepts_a_label_at_the_length_limit() {
    // The boundary: 999 characters is still a valid label and still resolves.
    let label = "b".repeat(999);
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        format!(
            "# Changelog\n\n\
             ## [0.7.0] - 2026-09-01\n\n\
             ### Changed\n\n\
             - **db:** **Breaking:** renamed. See the [migration guide][{label}].\n\n\
             [{label}]: docs/migrations/0.7.0.md\n"
        ),
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a label at the limit still resolves\n{}",
        gate_report(&output),
    );
}

// --- Review round 64: `--` inside a comment ------------------------------

#[test]
fn migration_guide_gate_treats_a_comment_containing_dashes_as_a_comment() {
    // Declared behaviour, not a fix. CommonMark 0.30 dropped the rule that a
    // comment may not contain `--`; a comment now runs from `<!--` to the
    // first `-->`. The reference implementation renders both of these as
    // comments, so the marker inside declares nothing and the section needs no
    // guide. A review round asked for the pre-0.30 reading; this pins the
    // spec-current one so it is not reopened from memory.
    for entry in [
        "- <!-- bad-- **Breaking:** renamed without guide. -->",
        "- an entry <!-- bad-- **Breaking:** renamed without guide. --> tail",
    ] {
        let tmp = migration_gate_fixture("0.7.0");
        std::fs::write(
            tmp.path().join("CHANGELOG.md"),
            format!(
                "# Changelog\n\n\
                 ## [0.7.0] - 2026-09-01\n\n\
                 ### Changed\n\n{entry}\n"
            ),
        )
        .expect("changelog");

        let output = run_migration_gate(tmp.path());
        assert!(
            output.status.success(),
            "a marker inside a comment declares nothing\n{}",
            gate_report(&output),
        );
        let listed = bash_command()
            .args(["scripts/check-migration-guides.sh", "--list"])
            .current_dir(tmp.path())
            .output()
            .expect("run --list");
        let inventory = String::from_utf8_lossy(&listed.stdout).to_string();
        assert!(
            inventory
                .lines()
                .any(|line| line.starts_with("0.7.0") && line.ends_with('0')),
            "the section must report zero breaking entries:\n{inventory}",
        );
    }
}

// --- Review round 65: links do not span a paragraph boundary --------------

#[test]
fn migration_guide_gate_rejects_a_link_split_across_a_blank_line() {
    // An inline link cannot span a paragraph break: the reference
    // implementation renders two paragraphs and no anchor. Joining the item's
    // lines with a space erased the boundary and made the halves look like one
    // working link.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. [guide](docs/migrations/0.7.0.md\n\n  )\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "a link cannot be assembled across a paragraph break",
    );
}

#[test]
fn migration_guide_gate_accepts_a_link_wrapped_within_a_paragraph() {
    // The counterpart, and the reason the fix is scoped to blank lines: a
    // label wrapped onto the next line of the *same* paragraph is one link,
    // and changelogs wrap like this constantly.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration\n  \
         guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "a wrapped label is still one link\n{}",
        gate_report(&output),
    );
}

// --- Review round 66: text inside a visible raw HTML block ---------------

#[test]
fn migration_guide_gate_counts_text_inside_a_raw_html_container() {
    // A `<div>` renders its contents, so instructions written inside one are
    // guidance a reader can read. Treating every line of a raw block as
    // invisible rejected a guide that says what to do.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "<div>\nUpgrade by changing the API call.\n</div>",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "words inside a container are still words\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_count_text_inside_a_comment_block() {
    // The counterpart: a comment shows nothing, so the same words inside one
    // leave the section a stub.
    let tmp = migration_gate_fixture("0.7.0");
    let guide = valid_migration_guide("0.7.0").replace(
        "Why this release breaks.",
        "<!--\nUpgrade by changing the API call.\n-->",
    );
    write_fixture_guide(&tmp, "0.7.0", &guide);
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the \
           [migration guide](docs/migrations/0.7.0.md).\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a commented-out instruction is not guidance\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_still_ignores_links_inside_a_raw_html_container() {
    // Counting the *text* must not start counting the *markdown*: a guide link
    // written inside a container still renders as literal text, so it cannot
    // satisfy an entry.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed, with no link of its own.\n\n  \
         <div>\n  See the [migration guide](docs/migrations/0.7.0.md).\n  </div>\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "markdown inside a container is still literal",
    );
}

// --- Review round 67: escapes in reference labels are literal -------------

#[test]
fn migration_guide_gate_matches_reference_labels_without_unescaping() {
    // Declared behaviour, not a fix. Label matching folds case and collapses
    // whitespace; it does not unescape punctuation. The reference
    // implementation links `[up\!]` to `[up\!]:` and does *not* link `[up!]`
    // to it, so the two labels are distinct. A review round asked for
    // unescaping, which would make an unrelated label resolve to the guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][up\\!].\n\n\
         [up\\!]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    let output = run_migration_gate(tmp.path());
    assert!(
        output.status.success(),
        "an escaped label matches its own escaped definition\n{}",
        gate_report(&output),
    );
}

#[test]
fn migration_guide_gate_does_not_match_an_unescaped_label_to_an_escaped_one() {
    // The other half: `[up!]` is a different label from `[up\!]`, so it
    // resolves to nothing and the entry has no clickable guide.
    let tmp = migration_gate_fixture("0.7.0");
    write_fixture_guide(&tmp, "0.7.0", &valid_migration_guide("0.7.0"));
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** renamed. See the [migration guide][up!].\n\n\
         [up\\!]: docs/migrations/0.7.0.md\n",
    )
    .expect("changelog");

    assert!(
        !run_migration_gate(tmp.path()).status.success(),
        "an unescaped label does not resolve to an escaped definition",
    );
}

// ---------------------------------------------------------------------------
// Codemod coverage (issue #1629).
//
// #1588 made the migration guide a release gate; these pin the follow-on gate:
// a rename-level break -- the class `autumn upgrade` can rewrite -- does not
// ship without either a codemod that actually exists in the registry, or a
// stated reason it stays manual.
// ---------------------------------------------------------------------------

/// A codemod registry the gate can read, in the shape it greps for: entries of
/// the production `APP_MIGRATIONS` table that actually carry a rewrite.
fn write_fixture_registry(tmp: &tempfile::TempDir, ids: &[&str]) {
    write_fixture_registry_with(tmp, ids, &[]);
}

/// As [`write_fixture_registry`], plus `guide_only` ids that rewrite nothing —
/// the gate must not accept those as backing an `auto`/`review` label.
fn write_fixture_registry_with(tmp: &tempfile::TempDir, rewriting: &[&str], guide_only: &[&str]) {
    let dir = tmp.path().join("autumn-cli/src/upgrade");
    std::fs::create_dir_all(&dir).expect("registry dir");
    let mut body = String::from("pub static APP_MIGRATIONS: &[AppMigration] = &[\n");
    for id in rewriting {
        let _ = writeln!(
            body,
            "    AppMigration {{\n        id: \"{id}\",\n                     rewrite: Rewrite::CallRename {{ from: \"a\", to: \"b\" }},\n    }},"
        );
    }
    for id in guide_only {
        let _ = writeln!(
            body,
            "    AppMigration {{\n        id: \"{id}\",\n                     rewrite: Rewrite::GuideOnly,\n    }},"
        );
    }
    body.push_str("];\n");
    // A `#[cfg(test)]` fixture table lives in the real file below the
    // production one; ids from it must never count as shipped.
    body.push_str(
        "\n#[cfg(test)]\nmod tests {\n    static FIXTURE_REGISTRY: &[AppMigration] = &[\n\
         \x20       AppMigration {\n            id: \"9.9.9-test-only\",\n                     rewrite: Rewrite::CallRename { from: \"x\", to: \"y\" },\n        },\n    ];\n}\n",
    );
    std::fs::write(dir.join("migrations.rs"), body).expect("registry");
}

/// A guide whose single breaking change is `body`, appended under the heading.
fn guide_with_breaking_change(version: &str, heading: &str, body: &str) -> String {
    format!(
        "# Migrating to `{version}`\n\n\
         ## At a glance\n\n\
         - **New version:** `autumn-web {version}`\n\n\
         ## Summary\n\n\
         Why this release breaks.\n\n\
         ## Before you start\n\n\
         Pin the old version and get green.\n\n\
         ## Breaking changes\n\n\
         ### {heading}\n\n\
         {body}\n\n\
         ## How to verify\n\n\
         Run `cargo check`.\n\n\
         ### Guide-only upgrade walkthrough\n\n\
         - **Status:** performed 2026-01-01\n"
    )
}

/// The changelog every fixture below shares: one breaking entry, guide linked.
fn breaking_changelog(version: &str) -> String {
    format!(
        "# Changelog\n\n\
         ## [{version}] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **db:** **Breaking:** the constructor changed. See the \
         [migration guide](docs/migrations/{version}.md).\n"
    )
}

fn gate_fixture_with_guide(version: &str, guide: &str) -> tempfile::TempDir {
    let tmp = migration_gate_fixture(version);
    write_fixture_guide(&tmp, version, guide);
    std::fs::write(tmp.path().join("CHANGELOG.md"), breaking_changelog(version))
        .expect("changelog");
    tmp
}

#[test]
fn codemod_gate_fails_a_rename_with_no_automation_label() {
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "Only the name changes.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a rename-level break must be classified (issue #1629)\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("automation label"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_ignores_a_semantic_change_with_no_automation_label() {
    // "Semantic/behavioral changes remain guide-only with no justification
    // needed" — the gate must not turn every behaviour change into paperwork.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Security: the signing secret is required in production",
            "Set `AUTUMN_TENANCY__JWT_SECRET` before booting.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_fails_an_auto_label_naming_no_shipped_codemod() {
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `auto` — `autumn upgrade` rewrites every call site; \
             codemod `0.7.0-not-shipped`.",
        ),
    );
    write_fixture_registry(&tmp, &["0.6.0-something-else"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a guide may not promise a codemod nobody wrote\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names no shipped codemod"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_accepts_an_auto_label_backed_by_the_registry() {
    let mut guide = guide_with_breaking_change(
        "0.7.0",
        "Repository: `with_pool` is renamed to `with_pool_untracked`",
        "**Automation:** `auto` — `autumn upgrade` rewrites every call site; \
         codemod `0.7.0-with-pool`.",
    );
    // A shipped codemod also has to have been used by the walk-through.
    guide = guide.replace(
        "- **Status:** performed 2026-01-01",
        "- **Codemod:** `autumn upgrade --apply` covered the rename.\n\
         - **Status:** performed 2026-01-01",
    );
    let tmp = gate_fixture_with_guide("0.7.0", &guide);
    write_fixture_registry(&tmp, &["0.7.0-with-pool"]);
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_fails_a_shipped_codemod_with_no_codemod_first_walkthrough() {
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `auto` — codemod `0.7.0-with-pool` rewrites every site.",
        ),
    );
    write_fixture_registry(&tmp, &["0.7.0-with-pool"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "the walk-through is performed codemod-first (issue #1629)\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("codemod-first walk-through"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_fails_a_rename_left_manual_without_a_reason() {
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `manual`",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a rename left manual has to say why\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no reason given"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_accepts_a_rename_left_manual_with_a_reason() {
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `manual` — the new name is only reachable from \
             inside the `repository!` macro, which no codemod may rewrite.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_rejects_an_unknown_automation_label() {
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `probably` — we think a codemod could do this.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "only auto/review/manual are labels\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown automation"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_reads_a_label_from_the_code_span_not_the_prose() {
    // "…we chose `manual` because auto was unsafe" must not read as `auto`.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `manual` — an auto rewrite would need type \
             inference this tool does not have.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_does_not_read_labels_out_of_fenced_samples() {
    // A guide that *documents* the convention shows the label in a fence; that
    // is a sample, not a declaration, and must not satisfy a real entry.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "```markdown\n**Automation:** `auto` — codemod `0.7.0-with-pool`.\n```",
        ),
    );
    write_fixture_registry(&tmp, &["0.7.0-with-pool"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a label inside a fence is a sample, not a classification\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("automation label"),
        "the entry must fail as unclassified, not for some unrelated reason\n{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_reads_the_registry_this_repository_actually_ships() {
    // The real registry and the real guides have to agree with each other, and
    // the gate is the thing that keeps them agreeing.
    let root = workspace_root();
    let registry = root.join("autumn-cli/src/upgrade/migrations.rs");
    assert!(
        registry.is_file(),
        "the codemod registry is the path the gate greps: {}",
        registry.display(),
    );
    let body = std::fs::read_to_string(&registry).expect("read registry");
    assert!(
        body.contains("id: \"0.6.0-repository-with-pool-untracked\""),
        "the first shipped codemod is the 0.6.0 with_pool rename (issue #1629)",
    );
    // The whole-repo gate run itself is asserted by
    // `migration_guide_gate_passes_for_this_repository`; this test pins the
    // path and the id the gate greps for.
    assert!(
        std::fs::read_to_string(root.join("docs/migrations/0.6.0.md"))
            .expect("read the 0.6.0 guide")
            .contains("0.6.0-repository-with-pool-untracked"),
        "the guide and the registry name the same codemod",
    );
}

#[test]
fn codemod_gate_reads_a_label_written_as_a_list_item() {
    // A bulleted label renders as the same declaration a reader sees, so it has
    // to be held to the same rule — otherwise a bullet is a way to promise a
    // codemod nobody wrote.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Config: the `session.ttl` default changed",
            "- **Automation:** `auto` — `autumn upgrade` fixes every call site; \
             codemod `0.7.0-vapourware`.",
        ),
    );
    write_fixture_registry(&tmp, &["0.7.0-something-else"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a bulleted label is still a label\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names no shipped codemod"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_reads_entries_outside_the_breaking_changes_section() {
    // A rename documented under `## Behavior changes` breaks apps exactly as
    // hard as one under `## Breaking changes`.
    let guide = valid_migration_guide("0.7.0").replace(
        "## How to verify",
        "## Behavior changes\n\n\
         ### Routing: `Router::mount` is renamed to `Router::attach`\n\n\
         Only the name changes.\n\n\
         ## How to verify",
    );
    let tmp = gate_fixture_with_guide("0.7.0", &guide);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a rename outside `## Breaking changes` must still be classified\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("automation label"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_does_not_accept_another_releases_codemod() {
    // Citing the previous release's codemod says nothing about this release.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `foo` is renamed to `bar`",
            "**Automation:** `auto` — codemod \
             `0.6.0-repository-with-pool-untracked` rewrites every site.",
        ),
    );
    write_fixture_registry(&tmp, &["0.6.0-repository-with-pool-untracked"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a 0.6.0 codemod does not cover a 0.7.0 rename\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names no shipped codemod"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_scopes_the_codemod_record_to_the_walkthrough_section() {
    // A `- **Codemod:**` bullet parked under a later sibling heading is not a
    // record of a codemod-first walk-through.
    let guide = guide_with_breaking_change(
        "0.7.0",
        "Repository: `with_pool` is renamed to `with_pool_untracked`",
        "**Automation:** `auto` — codemod `0.7.0-with-pool` rewrites every site.",
    )
    .replace(
        "- **Status:** performed 2026-01-01",
        "- **Status:** performed 2026-01-01\n\n\
         ### An unrelated later subsection\n\n\
         - **Codemod:** `autumn upgrade --apply`\n",
    );
    let tmp = gate_fixture_with_guide("0.7.0", &guide);
    write_fixture_registry(&tmp, &["0.7.0-with-pool"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "the codemod record has to sit under the walk-through heading\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("codemod-first walk-through"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_rejects_a_label_that_is_not_in_a_code_span() {
    // `**Automation:** auto` is prose, not a declaration the parser can read;
    // failing it beats silently reading the first word of the sentence.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** auto — we think a codemod covers this.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "an unbackticked label is not a label\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown automation"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_accepts_a_reason_wrapped_across_lines() {
    // Real guides wrap. A justification whose *first* line is short is still a
    // justification, and rejecting it would push authors to write one long line.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `manual` — no\nmechanical rewrite is possible here.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_accepts_the_reason_the_error_message_suggests() {
    // The `NOJUSTIFY` message offers "needs new arguments" as an acceptable
    // reason; a threshold that rejects it would be telling authors a lie.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `with_pool` is renamed to `with_pool_untracked`",
            "**Automation:** `manual` — needs new arguments.",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_honours_the_documented_suppression_token() {
    // An entry that *documents* the convention rather than declaring a break
    // uses the same escape hatch every other check in this gate offers.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Process: how a rename is classified",
            "Explains the labels. \
             <!-- migration-guide-gate: describes the convention itself -->",
        ),
    );
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_survives_a_registry_it_cannot_read() {
    // Fails closed and says so, rather than reading an unreadable registry as
    // "no codemods shipped" and blaming the guide.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Security: the signing secret is required",
            "Set it before booting.",
        ),
    );
    let registry = tmp.path().join("unreadable-registry.rs");
    std::fs::write(&registry, "id: \"0.7.0-x\",\n").expect("registry");
    let mut permissions = std::fs::metadata(&registry)
        .expect("metadata")
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(0o000);
    }
    std::fs::set_permissions(&registry, permissions).expect("chmod");

    let output = bash_command()
        .arg("scripts/check-migration-guides.sh")
        .env("CODEMOD_REGISTRY", "unreadable-registry.rs")
        .current_dir(tmp.path())
        .output()
        .expect("run migration-guide gate");

    // Running as root defeats the permission bits; only assert when it took.
    if std::fs::read_to_string(&registry).is_ok() {
        return;
    }
    assert!(
        !output.status.success(),
        "an unreadable registry is a broken checkout, not an empty one\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be read"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_does_not_accept_a_test_only_registry_id() {
    // The registry file carries a `#[cfg(test)]` fixture table below the
    // production one. A guide citing an id from it names no shipped codemod.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `foo` is renamed to `bar`",
            "**Automation:** `auto` — codemod `9.9.9-test-only` rewrites every site.",
        ),
    );
    write_fixture_registry(&tmp, &["0.7.0-real"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a test-fixture id is not a shipped codemod\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names no shipped codemod"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_does_not_accept_a_guide_only_id_as_a_codemod() {
    // A `GuideOnly` entry exists so the upgrade summary can link the guide; it
    // rewrites nothing, so it cannot back an `auto` label.
    let tmp = gate_fixture_with_guide(
        "0.7.0",
        &guide_with_breaking_change(
            "0.7.0",
            "Repository: `foo` is renamed to `bar`",
            "**Automation:** `auto` — codemod `0.7.0-guide-only` rewrites every site.",
        ),
    );
    write_fixture_registry_with(&tmp, &[], &["0.7.0-guide-only"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a migration that rewrites nothing is not a codemod\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names no shipped codemod"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_rejects_an_id_that_merely_starts_with_a_shipped_one() {
    // Codex review on #2231: the known-id test was `index(visible, id)`, a bare
    // substring, so a guide could name `0.7.0-with-pool-extra` — a codemod
    // nobody wrote — and be vouched for by the shipped `0.7.0-with-pool`.
    let mut guide = guide_with_breaking_change(
        "0.7.0",
        "Repository: `with_pool` is renamed to `with_pool_untracked`",
        "**Automation:** `auto` — `autumn upgrade` rewrites every call site; \
         codemod `0.7.0-with-pool-extra`.",
    );
    guide = guide.replace(
        "- **Status:** performed 2026-01-01",
        "- **Codemod:** `autumn upgrade --apply` covered the rename.\n\
         - **Status:** performed 2026-01-01",
    );
    let tmp = gate_fixture_with_guide("0.7.0", &guide);
    write_fixture_registry(&tmp, &["0.7.0-with-pool"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "a longer id is a different codemod, not the shipped one\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names no shipped codemod"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_accepts_a_shipped_id_that_is_a_prefix_of_a_longer_one() {
    // The boundary fix must not swing the other way: naming the shipped
    // `0.7.0-with-pool` is still valid when a longer id also ships.
    let mut guide = guide_with_breaking_change(
        "0.7.0",
        "Repository: `with_pool` is renamed to `with_pool_untracked`",
        "**Automation:** `auto` — `autumn upgrade` rewrites every call site; \
         codemod `0.7.0-with-pool`.",
    );
    guide = guide.replace(
        "- **Status:** performed 2026-01-01",
        "- **Codemod:** `autumn upgrade --apply` covered the rename.\n\
         - **Status:** performed 2026-01-01",
    );
    let tmp = gate_fixture_with_guide("0.7.0", &guide);
    write_fixture_registry(&tmp, &["0.7.0-with-pool", "0.7.0-with-pool-extra"]);
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}

#[test]
fn codemod_gate_rejects_a_walkthrough_codemod_bullet_that_ran_nothing() {
    // Codex review on #2231: the walk-through requirement matched the
    // `- **Codemod:**` label alone, so `none` recorded a codemod-first
    // walk-through that never ran the codemod.
    let mut guide = guide_with_breaking_change(
        "0.7.0",
        "Repository: `with_pool` is renamed to `with_pool_untracked`",
        "**Automation:** `auto` — `autumn upgrade` rewrites every call site; \
         codemod `0.7.0-with-pool`.",
    );
    guide = guide.replace(
        "- **Status:** performed 2026-01-01",
        "- **Codemod:** none\n\
         - **Status:** performed 2026-01-01",
    );
    let tmp = gate_fixture_with_guide("0.7.0", &guide);
    write_fixture_registry(&tmp, &["0.7.0-with-pool"]);
    let output = run_migration_gate(tmp.path());
    assert!(
        !output.status.success(),
        "`none` is not a codemod-first walk-through\n{}",
        gate_report(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("codemod-first walk-through"),
        "{}",
        gate_report(&output),
    );
}

#[test]
fn codemod_gate_accepts_a_walkthrough_codemod_bullet_wrapped_onto_a_continuation() {
    // The value check reads the whole bullet, so a wrapped invocation — the
    // shape `TEMPLATE.md` itself uses — still counts.
    let mut guide = guide_with_breaking_change(
        "0.7.0",
        "Repository: `with_pool` is renamed to `with_pool_untracked`",
        "**Automation:** `auto` — `autumn upgrade` rewrites every call site; \
         codemod `0.7.0-with-pool`.",
    );
    guide = guide.replace(
        "- **Status:** performed 2026-01-01",
        "- **Codemod:** the preview first, then\n    \
         `autumn upgrade --apply` for the rename.\n\
         - **Status:** performed 2026-01-01",
    );
    let tmp = gate_fixture_with_guide("0.7.0", &guide);
    write_fixture_registry(&tmp, &["0.7.0-with-pool"]);
    let output = run_migration_gate(tmp.path());
    assert!(output.status.success(), "{}", gate_report(&output));
}
