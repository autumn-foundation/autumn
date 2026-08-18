//! Integration tests for `autumn upgrade`'s app-code migration step
//! (issue #1629).
//!
//! Each test scaffolds a throwaway app in a `TempDir` — a `Cargo.toml` pinning
//! an old `autumn-web` plus a few `src/*.rs` files — and runs the real `autumn`
//! binary against it. The fixture sources are only ever *parsed*, so they must
//! be syntactically valid Rust but need not compile or link.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

fn run_upgrade(root: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("upgrade")
        .args(args)
        .current_dir(root)
        .output()
        .expect("failed to run autumn upgrade")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn report(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

/// An app pinned to `version` of `autumn-web`.
fn app(version: &str) -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        &format!(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nautumn-web = \"{version}\"\n"
        ),
    );
    tmp
}

/// The 0.6.0 rename's call sites, as an app on 0.5.x would have written them.
const USES_WITH_POOL: &str = r"use autumn_web::prelude::*;

pub fn build(pool: Pool) -> PostRepository {
    // Historically this was `with_pool`.
    PostRepository::with_pool(pool.clone())
}

pub fn build_two(pool: Pool) -> CommentRepository {
    let repo = CommentRepository::with_pool(pool);
    repo
}
";

#[test]
fn preview_is_the_default_and_writes_nothing() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    let before = read(tmp.path(), "src/main.rs");

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));

    let out = stdout_of(&output);
    assert!(out.contains("src/main.rs"), "{out}");
    assert!(
        out.contains("with_pool_untracked"),
        "the diff shows the new name: {out}"
    );
    assert!(
        out.contains("2 site"),
        "the affected-site count is reported: {out}"
    );
    assert!(out.contains("--apply"), "{out}");
    assert_eq!(
        read(tmp.path(), "src/main.rs"),
        before,
        "a bare `autumn upgrade` must not write anything"
    );
}

#[test]
fn apply_rewrites_every_call_site() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    let after = read(tmp.path(), "src/main.rs");
    assert!(
        !after.contains("PostRepository::with_pool("),
        "no bare `with_pool` call site remains:\n{after}"
    );
    assert_eq!(
        after.matches("with_pool_untracked(").count(),
        2,
        "both call sites were rewritten:\n{after}"
    );
    assert!(
        after.contains("// Historically this was `with_pool`."),
        "comments are left exactly as they were:\n{after}"
    );
}

#[test]
fn applying_twice_is_a_no_op() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let first = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(first.status.success(), "{}", report(&first));
    let after_first = read(tmp.path(), "src/main.rs");

    let second = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(second.status.success(), "{}", report(&second));
    assert_eq!(
        read(tmp.path(), "src/main.rs"),
        after_first,
        "a second apply must change nothing"
    );
    assert!(
        stdout_of(&second)
            .to_lowercase()
            .contains("nothing to change"),
        "{}",
        report(&second)
    );
}

#[test]
fn an_app_that_never_used_the_api_reports_nothing_to_change() {
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/main.rs",
        "fn main() {\n    println!(\"hello\");\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        stdout_of(&output)
            .to_lowercase()
            .contains("nothing to change"),
        "{}",
        report(&output)
    );
}

#[test]
fn macro_call_sites_are_listed_as_manual_with_file_and_line() {
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/lib.rs",
        "pub fn f(pool: Pool) {\n    make_repo! { PostRepository::with_pool(pool) }\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    let out = stdout_of(&output);
    assert!(out.contains("manual"), "{out}");
    assert!(
        out.contains("src/lib.rs:2"),
        "the exact site is named: {out}"
    );
    assert!(out.contains("inside a macro invocation"), "{out}");
    assert!(
        out.contains("docs/migrations/0.6.0.md"),
        "the guide is linked: {out}"
    );
    assert!(
        read(tmp.path(), "src/lib.rs").contains("PostRepository::with_pool(pool)"),
        "a macro body is never rewritten"
    );
}

#[test]
fn guide_only_changes_in_range_are_reported_with_their_guide_section() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(
        out.contains("0.6.0-tenancy-jwt-secret-secretstring"),
        "a manual change in the range is still surfaced: {out}"
    );
    assert!(out.contains("docs/migrations/0.6.0.md#"), "{out}");
}

#[test]
fn the_version_range_is_read_from_the_apps_cargo_manifest() {
    // Already on 0.6.0: the 0.6.0 migrations are behind this app, not ahead.
    let tmp = app("0.6.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        stdout_of(&output)
            .to_lowercase()
            .contains("nothing to change"),
        "{}",
        report(&output)
    );
    assert!(
        read(tmp.path(), "src/main.rs").contains("PostRepository::with_pool(pool.clone())"),
        "nothing was rewritten"
    );
}

#[test]
fn an_undetectable_version_asks_for_from_instead_of_guessing() {
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--from"), "{stderr}");
    assert!(
        read(tmp.path(), "src/main.rs").contains("PostRepository::with_pool(pool.clone())"),
        "a run that cannot determine its range must not rewrite anything"
    );
}

#[test]
fn from_overrides_the_detected_version() {
    let tmp = app("0.6.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--from", "0.5.0", "--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "src/main.rs").contains("with_pool_untracked("));
}

#[test]
fn build_artifacts_and_vendored_sources_are_never_scanned() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    write(tmp.path(), "target/debug/build/gen.rs", USES_WITH_POOL);
    write(tmp.path(), "vendor/autumn-web/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "target/debug/build/gen.rs").contains("with_pool(pool.clone())"));
    assert!(read(tmp.path(), "vendor/autumn-web/src/lib.rs").contains("with_pool(pool.clone())"));
}

#[test]
fn tests_and_examples_are_app_code_and_are_rewritten() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    write(tmp.path(), "tests/repo.rs", USES_WITH_POOL);
    write(tmp.path(), "examples/demo.rs", USES_WITH_POOL);
    write(tmp.path(), "benches/bench.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    for file in ["tests/repo.rs", "examples/demo.rs", "benches/bench.rs"] {
        assert!(
            read(tmp.path(), file).contains("with_pool_untracked("),
            "{file} is the app's own source and must be migrated"
        );
    }
}

#[test]
fn a_workspace_member_is_migrated_too() {
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\"]\n\n[workspace.dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(tmp.path(), "api/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "api/src/lib.rs").contains("with_pool_untracked("));
}

#[test]
fn an_unparsable_file_is_skipped_and_reported_never_written() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    write(tmp.path(), "src/broken.rs", "fn f( { this is not rust\n");
    let broken_before = read(tmp.path(), "src/broken.rs");

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("src/broken.rs"), "the skip is reported: {out}");
    assert_eq!(read(tmp.path(), "src/broken.rs"), broken_before);
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "one unparsable file does not stop the rest of the migration"
    );
}

#[test]
fn json_output_is_machine_readable() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--json"]);
    assert!(output.status.success(), "{}", report(&output));
    let value: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("stdout is a single JSON document");
    assert_eq!(value["from"], "0.5.0");
    assert_eq!(value["to"], "0.6.0");
    assert_eq!(value["applied"], false);
    assert_eq!(value["rewritten_sites"], 2);
    let confidences: Vec<&str> = value["migrations"]
        .as_array()
        .expect("migrations array")
        .iter()
        .map(|m| m["confidence"].as_str().expect("confidence label"))
        .collect();
    assert!(confidences.contains(&"auto"), "{confidences:?}");
    assert!(confidences.contains(&"manual"), "{confidences:?}");
}

#[test]
fn list_migrations_prints_the_registry_without_scanning() {
    let tmp = TempDir::new().expect("tempdir");
    let output = run_upgrade(tmp.path(), &["--list-migrations"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(
        out.contains("0.6.0-repository-with-pool-untracked"),
        "{out}"
    );
    assert!(out.contains("auto"), "{out}");
    assert!(out.contains("docs/migrations/0.6.0.md#"), "{out}");
}

#[test]
fn every_registry_guide_link_resolves_to_a_real_guide_section() {
    // The registry, the guides, and the release gate have to agree; a dangling
    // anchor turns the `manual` list into a dead end.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let output = run_upgrade(&root, &["--list-migrations", "--json"]);
    assert!(output.status.success(), "{}", report(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout_of(&output)).expect("valid JSON");

    for migration in value["migrations"].as_array().expect("migrations") {
        let id = migration["id"].as_str().expect("id");
        let guide = migration["guide"].as_str().expect("guide");
        let (path, anchor) = guide.split_once('#').expect("guide link carries an anchor");
        let body = fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("{id}: cannot read {path}: {e}"));
        let anchors: Vec<String> = body
            .lines()
            .filter(|line| line.starts_with("### "))
            .map(|line| github_anchor(line.trim_start_matches("### ")))
            .collect();
        assert!(
            anchors.iter().any(|a| a == anchor),
            "{id}: {path} has no section anchored `#{anchor}`; found {anchors:?}"
        );
        assert!(
            body.contains(id),
            "{id}: {path} must name the codemod id in its `**Automation:**` label"
        );
    }
}

/// GitHub's heading-anchor slug: lowercase, drop everything but word
/// characters, spaces and hyphens, then spaces to hyphens.
fn github_anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect::<String>()
        .replace(' ', "-")
}

#[test]
fn every_breaking_change_in_every_guide_carries_a_confidence_label() {
    // Issue #1629 AC: "Every documented breaking change in a release is
    // classified in its migration guide with a confidence label." The shell
    // gate enforces this where it is load-bearing (rename-level changes); this
    // reads the guides as a set and holds the whole repository to it.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("docs/migrations");

    let mut guides: Vec<_> = fs::read_dir(&root)
        .expect("read docs/migrations")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "md")
                && !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("README.md" | "TEMPLATE.md")
                )
        })
        .collect();
    guides.sort();
    assert!(!guides.is_empty(), "no migration guides found in {root:?}");

    let mut classified = 0usize;
    for guide in guides {
        let entries = breaking_entries(&fs::read_to_string(&guide).expect("read guide"));
        assert!(
            !entries.is_empty(),
            "{}: no breaking-change entries found — either the guide has none, \
             or `breaking_entries` stopped recognising the section shape and \
             this test is now vacuous",
            guide.display(),
        );
        for (heading, label) in entries {
            classified += 1;
            let label = label.unwrap_or_else(|| {
                panic!(
                    "{}: breaking change `{heading}` carries no `**Automation:**` label \
                     (issue #1629)",
                    guide.display()
                )
            });
            assert!(
                matches!(label.as_str(), "auto" | "review" | "manual"),
                "{}: breaking change `{heading}` has an unknown automation label `{label}`",
                guide.display(),
            );
        }
    }
    assert!(
        classified >= 10,
        "only {classified} breaking changes were read across every guide; the \
         parser has almost certainly stopped seeing them",
    );
}

/// Every `### ` entry under `## Breaking changes`, paired with the value of its
/// `**Automation:**` label if it has one.
///
/// Fenced blocks are skipped: a guide that *shows* the convention in a sample
/// is not declaring it.
fn breaking_entries(body: &str) -> Vec<(String, Option<String>)> {
    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    let mut in_breaking = false;
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(title) = line.strip_prefix("## ") {
            in_breaking = title.trim().eq_ignore_ascii_case("Breaking changes");
            continue;
        }
        if let Some(title) = line.strip_prefix("### ") {
            if in_breaking {
                entries.push((title.trim().to_owned(), None));
            }
            continue;
        }
        // A label written as a bullet is the same declaration a reader sees.
        // The marker is only a marker when whitespace follows it — stripping
        // bare `-`/`*` would eat the `**` of the label itself.
        let trimmed = line.trim_start();
        let body = ["- ", "* ", "+ "]
            .iter()
            .find_map(|marker| trimmed.strip_prefix(marker))
            .unwrap_or(trimmed)
            .trim_start();
        if let Some(rest) = body.strip_prefix("**Automation:**")
            && let Some(entry) = entries.last_mut()
            && entry.1.is_none()
        {
            entry.1 = rest
                .trim()
                .strip_prefix('`')
                .and_then(|rest| rest.split('`').next())
                .map(str::to_owned);
        }
    }
    entries
}

#[test]
fn a_file_with_both_rewritable_and_macro_sites_gets_both_treatments() {
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/main.rs",
        "pub fn a(pool: Pool) -> Repo {\n    Repo::with_pool(pool)\n}\n\n\
         pub fn b(pool: Pool) {\n    make_repo! { Repo::with_pool(pool) }\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    let after = read(tmp.path(), "src/main.rs");
    assert!(
        after.contains("Repo::with_pool_untracked(pool)\n}"),
        "the reachable site is rewritten:\n{after}"
    );
    assert!(
        after.contains("make_repo! { Repo::with_pool(pool) }"),
        "the macro body is left alone:\n{after}"
    );
    let out = stdout_of(&output);
    assert!(
        out.contains("src/main.rs:6"),
        "the macro site is named: {out}"
    );
}

#[test]
fn a_function_reference_that_is_never_called_is_reported_not_skipped() {
    // It stops compiling after the rename just the same, so staying silent
    // about it would break the "nothing is silently skipped" promise.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/main.rs",
        "pub fn a(xs: Vec<Pool>) {\n    xs.into_iter().map(Repo::with_pool);\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("src/main.rs:2"), "{out}");
    assert!(out.contains("referenced without being called"), "{out}");
    assert!(
        read(tmp.path(), "src/main.rs").contains("map(Repo::with_pool)"),
        "a reference is reported, never rewritten"
    );
}

#[test]
fn a_symlinked_source_file_is_reported_rather_than_followed() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/real.rs", USES_WITH_POOL);
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        tmp.path().join("src/real.rs"),
        tmp.path().join("src/link.rs"),
    )
    .expect("symlink");
    #[cfg(not(unix))]
    return;

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("src/link.rs"), "the link is named: {out}");
    assert!(out.contains("symbolic link"), "{out}");
    // The real file, reached directly, is still migrated.
    assert!(read(tmp.path(), "src/real.rs").contains("with_pool_untracked("));
}

#[test]
fn a_positional_path_migrates_that_directory_not_the_working_one() {
    let outside = TempDir::new().expect("tempdir");
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = Command::new(autumn_bin())
        .args(["upgrade", tmp.path().to_str().expect("utf-8 path")])
        .args(["--to", "0.6.0", "--apply"])
        .current_dir(outside.path())
        .output()
        .expect("failed to run autumn upgrade");
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "src/main.rs").contains("with_pool_untracked("));
}

#[test]
fn an_unreadable_path_is_a_failure_not_an_empty_report() {
    // A path typo must not read as "your app is already migrated".
    let tmp = app("0.5.0");
    let output = run_upgrade(
        tmp.path(),
        &["no-such-directory", "--from", "0.5.0", "--to", "0.6.0"],
    );
    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a readable directory"),
        "{}",
        report(&output),
    );
}

#[test]
fn generated_and_dependency_trees_are_never_scanned() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    for excluded in [
        "target/debug/gen.rs",
        "vendor/dep/src/lib.rs",
        "node_modules/thing/x.rs",
        "dist/out.rs",
        "tmp/scratch.rs",
        ".cargo-cache/dep.rs",
    ] {
        write(tmp.path(), excluded, USES_WITH_POOL);
    }

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    for excluded in [
        "target/debug/gen.rs",
        "vendor/dep/src/lib.rs",
        "node_modules/thing/x.rs",
        "dist/out.rs",
        "tmp/scratch.rs",
        ".cargo-cache/dep.rs",
    ] {
        assert!(
            read(tmp.path(), excluded).contains("with_pool(pool.clone())"),
            "{excluded} is not the app's own source and must not be rewritten"
        );
    }
}

#[test]
fn an_unparsable_target_version_is_rejected_before_anything_is_scanned() {
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "banana", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--to"),
        "{}",
        report(&output),
    );
    assert!(read(tmp.path(), "src/main.rs").contains("with_pool(pool.clone())"));
}

#[test]
fn an_unparsable_recorded_version_asks_for_from() {
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"*\"\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--from"), "{stderr}");
    assert!(read(tmp.path(), "src/main.rs").contains("with_pool(pool.clone())"));
}

#[test]
fn an_already_bumped_manifest_is_told_how_to_recover() {
    // The commonest way to see an empty range: the dependency was bumped
    // before the codemods were run, so the app records the target release.
    let tmp = app("0.6.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("--from"), "the recovery is spelled out: {out}");
}

#[test]
fn a_preview_whose_every_site_is_manual_does_not_offer_apply() {
    // `--apply` would write nothing here, so offering it is a lie.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/lib.rs",
        "pub fn f(pool: Pool) {\n    make_repo! { Repo::with_pool(pool) }\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(
        !out.contains("Re-run with `--apply`"),
        "there is nothing for --apply to write: {out}"
    );
    assert!(out.contains("a human"), "{out}");
}

#[test]
fn a_same_named_framework_builder_method_is_never_rewritten() {
    // `AppState::with_pool` and `AuthzContext::with_pool` are *current*
    // framework builders that still carry the old name. The renamed repository
    // constructor takes no `self`, so the `.` call form is a different function
    // — rewriting it would break ordinary test setup and the app would not
    // compile, which is the one thing this command must never do.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/main.rs",
        "pub fn setup(state: AppState, pool: Pool) -> AppState {\n\
        \x20   let repo = PostRepository::with_pool(pool.clone());\n\
        \x20   drop(repo);\n\
        \x20   state.with_pool(pool)\n\
         }\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    let after = read(tmp.path(), "src/main.rs");
    assert!(
        after.contains("PostRepository::with_pool_untracked(pool.clone())"),
        "the repository constructor is migrated:\n{after}"
    );
    assert!(
        after.contains("state.with_pool(pool)"),
        "the AppState builder method is left exactly as it was:\n{after}"
    );
}

#[test]
fn every_pre_target_release_in_range_is_reported_not_just_the_newest() {
    // An app coming from 0.3.x has 0.4.0 and 0.5.0 breaks ahead of it too.
    // Reporting only the 0.6.0 ones would read as "those are the only breaks".
    let tmp = app("0.3.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--json"]);
    assert!(output.status.success(), "{}", report(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout_of(&output)).expect("valid JSON");
    let versions: Vec<&str> = value["migrations"]
        .as_array()
        .expect("migrations")
        .iter()
        .map(|m| m["version"].as_str().expect("version"))
        .collect();
    for release in ["0.4.0", "0.5.0", "0.6.0"] {
        assert!(
            versions.contains(&release),
            "{release} is in range and must be reported, got {versions:?}"
        );
    }
}
