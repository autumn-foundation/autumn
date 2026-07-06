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
    let current_version = workspace_package_value(&root_toml, "version");
    let current_series = current_version
        .rsplit_once('.')
        .map_or(current_version.as_str(), |(series, _)| series);
    let rust_version = workspace_package_value(&root_toml, "rust-version");
    let current_health_json = format!(r#"{{ "status": "ok", "version": "{current_version}" }}"#);
    let docs = read_docs_once(&root);

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
                    "cargo install autumn-cli --version {current_version}"
                )),
                "{doc} must show the published CLI install command for autumn-cli {current_version}"
            );
        }

        if content.contains("autumn-web =") {
            assert!(
                content.contains(&format!("autumn-web = \"{current_series}\""))
                    || content.contains(&format!("autumn-web = \"{current_version}\""))
                    || content.contains(&format!("version = \"{current_series}\""))
                    || content.contains(&format!("version = \"{current_version}\"")),
                "{doc} must show the current autumn-web release line ({current_series} or {current_version})",
            );
        }

        if content.contains(r#""status": "ok""#) {
            assert!(
                content.contains(&current_health_json),
                "{doc} must show the current JSON health version {current_version}"
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
        docs_smoke.contains(&current_health_json),
        "docs-smoke must show the exact health JSON with version {current_version}"
    );
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
        script.contains(r#"semver_toolchain="${AUTUMN_SEMVER_RUST_VERSION:-1.92.0}""#),
        "{} must pin the semver rustdoc toolchain to Rust 1.92.0 by default",
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
        semver_job.contains("dtolnay/rust-toolchain@1.92.0"),
        "{} semver job must install the pinned Rust 1.92.0 toolchain",
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
    std::fs::copy(
        root.join("scripts/check-release-notes.sh"),
        scripts_dir.join("check-release-notes.sh"),
    )
    .expect("copy release-notes script");

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
