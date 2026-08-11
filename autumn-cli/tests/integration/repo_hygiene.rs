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

    // The Postgres-dependent gate must also be present (AC-2).
    assert!(
        workflow.contains("generated_scaffold_serves_posts_index_and_json_api"),
        "generator-conformance.yml must run the Postgres e2e gate \
         `generated_scaffold_serves_posts_index_and_json_api`",
    );

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
    // literal text, not a fence. Treating them as one swallowed the entries
    // between two such displayed markers.
    let tmp = migration_gate_fixture("0.7.0");
    std::fs::write(
        tmp.path().join("CHANGELOG.md"),
        "# Changelog\n\n\
         ## [0.7.0] - 2026-09-01\n\n\
         ### Changed\n\n\
         - **docs:** a fence is written as:\n\n    ```\n\n\
         - **db:** **Breaking:** `with_pool` renamed, with no guide anywhere.\n\n\
         - **docs:** and closed with:\n\n    ```\n",
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
