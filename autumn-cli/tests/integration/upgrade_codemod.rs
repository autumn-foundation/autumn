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
    run_upgrade_with_env(root, args, &[])
}

fn run_upgrade_with_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(autumn_bin());
    command.arg("upgrade").args(args).current_dir(root);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("failed to run autumn upgrade")
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
///
/// The `#[repository]` traits are part of the fixture, not decoration: they are
/// what makes `PgPostRepository` and `PgCommentRepository` generated types
/// rather than names that merely look like them, and the codemod refuses to
/// rewrite a receiver no scanned trait accounts for.
const USES_WITH_POOL: &str = r"use autumn_web::prelude::*;

#[repository]
pub trait PostRepository {
    fn noop(&self);
}

#[repository]
pub trait CommentRepository {
    fn noop(&self);
}

pub fn build(pool: Pool) -> PostRepository {
    // Historically this was `with_pool`.
    PgPostRepository::with_pool(pool.clone())
}

pub fn build_two(pool: Pool) -> CommentRepository {
    let repo = PgCommentRepository::with_pool(pool);
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
        !after.contains("PgPostRepository::with_pool("),
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
        "pub fn f(pool: Pool) {\n    make_repo! { PgPostRepository::with_pool(pool) }\n}\n",
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
        read(tmp.path(), "src/lib.rs").contains("PgPostRepository::with_pool(pool)"),
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
        read(tmp.path(), "src/main.rs").contains("PgPostRepository::with_pool(pool.clone())"),
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
        read(tmp.path(), "src/main.rs").contains("PgPostRepository::with_pool(pool.clone())"),
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
        "#[repository]\npub trait PostRepository {\n    fn noop(&self);\n}\n\n\
         pub fn a(pool: Pool) -> Repo {\n    PgPostRepository::with_pool(pool)\n}\n\n\
         pub fn b(pool: Pool) {\n    make_repo! { PgPostRepository::with_pool(pool) }\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    let after = read(tmp.path(), "src/main.rs");
    assert!(
        after.contains("PgPostRepository::with_pool_untracked(pool)\n}"),
        "the reachable site is rewritten:\n{after}"
    );
    assert!(
        after.contains("make_repo! { PgPostRepository::with_pool(pool) }"),
        "the macro body is left alone:\n{after}"
    );
    let out = stdout_of(&output);
    assert!(
        // The `#[repository]` trait the fixture declares occupies the first
        // five lines, so the macro site sits at 11.
        out.contains("src/main.rs:11"),
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
        "pub fn a(xs: Vec<Pool>) {\n    xs.into_iter().map(PgPostRepository::with_pool);\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("src/main.rs:2"), "{out}");
    assert!(out.contains("referenced without being called"), "{out}");
    assert!(
        read(tmp.path(), "src/main.rs").contains("map(PgPostRepository::with_pool)"),
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
    // Hidden directories are covered separately: only *known* metadata names
    // are skipped, because an unknown dot-directory can hold compiled source
    // (see `source_in_a_hidden_directory_is_migrated_not_dropped`).
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    for excluded in [
        "target/debug/gen.rs",
        "vendor/dep/src/lib.rs",
        "node_modules/thing/x.rs",
        "dist/out.rs",
        "tmp/scratch.rs",
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
        "pub fn f(pool: Pool) {\n    make_repo! { PgPostRepository::with_pool(pool) }\n}\n",
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
        "#[repository]\npub trait PostRepository {\n    fn noop(&self);\n}\n\n\
         pub fn setup(state: AppState, pool: Pool) -> AppState {\n\
        \x20   let repo = PgPostRepository::with_pool(pool.clone());\n\
        \x20   drop(repo);\n\
        \x20   state.with_pool(pool)\n\
         }\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    let after = read(tmp.path(), "src/main.rs");
    assert!(
        after.contains("PgPostRepository::with_pool_untracked(pool.clone())"),
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

#[test]
fn a_virtual_workspace_whose_members_declare_the_dependency_is_migrated() {
    // A virtual root has no `[dependencies]` and no `[workspace.dependencies]`;
    // each member carries its own `autumn-web` line. Reading only the root
    // would abort an ordinary layout with "cannot tell which version".
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\", \"worker\"]\n",
    );
    for member in ["api", "worker"] {
        write(
            tmp.path(),
            &format!("{member}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        );
        write(tmp.path(), &format!("{member}/src/lib.rs"), USES_WITH_POOL);
    }

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    for member in ["api", "worker"] {
        assert!(
            read(tmp.path(), &format!("{member}/src/lib.rs")).contains("with_pool_untracked("),
            "{member} declares autumn-web itself and must be migrated"
        );
    }
}

#[test]
fn members_on_different_versions_take_the_oldest_floor() {
    // The member that is furthest behind decides: a migration the other member
    // is already past finds nothing to do there, while taking the newest floor
    // would strand the one that is genuinely behind.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"old\", \"new\"]\n",
    );
    for (member, version) in [("old", "0.5.0"), ("new", "0.6.0")] {
        write(
            tmp.path(),
            &format!("{member}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\nautumn-web = \"{version}\"\n"
            ),
        );
    }
    write(tmp.path(), "old/src/lib.rs", USES_WITH_POOL);
    write(tmp.path(), "new/src/lib.rs", "pub fn f() {}\n");

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "old/src/lib.rs").contains("with_pool_untracked("),
        "the member still on 0.5.0 is migrated"
    );
}

#[test]
fn a_member_pinned_behind_the_workspace_root_still_decides_the_floor() {
    // The root records 0.6.0 but one member is still on 0.5.0. Taking the root
    // alone would select no migration while happily scanning that member's
    // source, leaving it unmigrated.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\", \"legacy\"]\n\n\
         [workspace.dependencies]\nautumn-web = \"0.6.0\"\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = { workspace = true }\n",
    );
    write(tmp.path(), "api/src/lib.rs", "pub fn f() {}\n");
    write(
        tmp.path(),
        "legacy/Cargo.toml",
        "[package]\nname = \"legacy\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(tmp.path(), "legacy/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "legacy/src/lib.rs").contains("with_pool_untracked("),
        "the member still on 0.5.0 must be migrated:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_unrelated_type_with_the_same_associated_function_is_reported_not_rewritten() {
    // `#[repository]` names its concrete type `Pg{trait}`, so an app's own
    // `Cache::with_pool(pool)` is not the renamed constructor — but it is
    // reported, because an aliased import would look identical.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/cache.rs",
        "pub fn build(pool: Pool) -> Cache {\n    Cache::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/cache.rs").contains("Cache::with_pool(pool)"),
        "an unrelated type must not be rewritten"
    );
    let out = stdout_of(&output);
    assert!(out.contains("src/cache.rs:2"), "the site is named: {out}");
    assert!(
        out.contains("receiver is not a generated repository"),
        "and the reason is given: {out}"
    );
}

#[test]
fn an_ambiguous_member_requirement_fails_detection_instead_of_being_ignored() {
    // Root says 0.6.0, one member declares `<0.6` — a requirement with no
    // usable floor. Dropping that member and trusting the root would migrate
    // nothing while scanning its 0.5.x source, which is what refusing
    // upper-bound-only requirements exists to prevent.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\", \"legacy\"]\n\n\
         [workspace.dependencies]\nautumn-web = \"0.6.0\"\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(tmp.path(), "api/src/lib.rs", "pub fn f() {}\n");
    write(
        tmp.path(),
        "legacy/Cargo.toml",
        "[package]\nname = \"legacy\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"<0.6\"\n",
    );
    write(tmp.path(), "legacy/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--from"),
        "{}",
        report(&output),
    );
    assert!(
        read(tmp.path(), "legacy/src/lib.rs").contains("with_pool(pool.clone())"),
        "nothing is rewritten while the range is a guess"
    );
}

#[test]
fn a_target_specific_dependency_declaration_is_a_declaration() {
    // Cargo honours `[target.'cfg(unix)'.dependencies]`, so reporting "cannot
    // determine the version" for one would be wrong.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [target.'cfg(unix)'.dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "src/main.rs").contains("with_pool_untracked("));
}

#[test]
fn a_path_dependency_without_a_version_fails_detection() {
    // `autumn-web = { path = "../autumn" }` is a declaration with no version
    // to read — and a vendored checkout is exactly the population the 0.6.0
    // rename affects. Letting the sibling's 0.6.0 decide the floor would
    // migrate this crate's source against a version nobody checked.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\", \"vendored\"]\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.6.0\"\n",
    );
    write(tmp.path(), "api/src/lib.rs", "pub fn f() {}\n");
    write(
        tmp.path(),
        "vendored/Cargo.toml",
        "[package]\nname = \"vendored\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = { path = \"../../autumn\" }\n",
    );
    write(tmp.path(), "vendored/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--from"),
        "{}",
        report(&output),
    );
    assert!(
        read(tmp.path(), "vendored/src/lib.rs").contains("with_pool(pool.clone())"),
        "nothing is rewritten while the range is a guess"
    );

    // With the range stated explicitly, the same tree migrates.
    let forced = run_upgrade(tmp.path(), &["--from", "0.5.0", "--to", "0.6.0", "--apply"]);
    assert!(forced.status.success(), "{}", report(&forced));
    assert!(read(tmp.path(), "vendored/src/lib.rs").contains("with_pool_untracked("));
}

#[test]
fn an_aliased_dependency_declaration_is_recognised() {
    // Cargo's renamed-dependency form. An exact-key lookup reads this as "no
    // autumn-web here", which in a workspace lets a sibling pick the floor for
    // a crate whose sources are rewritten anyway.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn_web = { package = \"autumn-web\", version = \"0.5.0\" }\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "an aliased declaration still records the version"
    );
}

#[test]
fn the_oldest_target_specific_requirement_decides_the_floor() {
    // Disjoint target tables at different versions. Taking the first match
    // could pick 0.6.0 and skip the codemod, while the source scan rewrites the
    // 0.5.x target's `#[cfg]` code regardless.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [target.'cfg(windows)'.dependencies]\nautumn-web = \"0.6.0\"\n\n\
         [target.'cfg(unix)'.dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "the older target requirement must decide the floor:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_aliased_path_dependency_is_still_ambiguous() {
    // The alias must not become a way to slip a version-less declaration past
    // the ambiguity check.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn_web = { package = \"autumn-web\", path = \"../autumn\" }\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--from"),
        "{}",
        report(&output),
    );
}

#[test]
fn a_dev_only_dependency_still_records_the_version() {
    // A test-support crate depends on autumn-web only for its tests — and its
    // `tests/` are scanned and rewritten, so it has to get a vote.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dev-dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(tmp.path(), "tests/repo.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "tests/repo.rs").contains("with_pool_untracked("));
}

#[test]
fn an_older_dev_dependency_decides_the_floor_against_a_newer_normal_one() {
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.6.0\"\n\n\
         [dev-dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(tmp.path(), "tests/repo.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "tests/repo.rs").contains("with_pool_untracked("),
        "the older dev-dependency must decide the floor:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_app_on_a_release_candidate_is_migrated_to_the_release() {
    // Autumn ships release candidates (the migration-guide gate handles
    // `## [0.7.0-rc.1]` sections), so this is a real upgrade path: the
    // candidate precedes the release and still needs its migrations.
    let tmp = app("0.5.0-rc.1");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "src/main.rs").contains("with_pool_untracked("));
}

#[test]
fn an_app_on_the_release_candidate_of_the_target_still_gets_that_release() {
    // 0.6.0-rc.1 -> 0.6.0: equal triples, but the candidate is behind.
    let tmp = app("0.6.0-rc.1");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "a candidate of the target release is still behind it:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_app_module_named_like_a_build_directory_is_still_migrated() {
    // `target`, `vendor`, `tmp` name build output or third-party code only at
    // a crate root. Under `src/` they are ordinary modules, and skipping them
    // left the app failing to compile with nothing in the report to say why.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    for module in [
        "src/vendor/mod.rs",
        "src/tmp/helper.rs",
        "src/target/mod.rs",
        "src/dist/mod.rs",
    ] {
        write(tmp.path(), module, USES_WITH_POOL);
    }

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    for module in [
        "src/vendor/mod.rs",
        "src/tmp/helper.rs",
        "src/target/mod.rs",
        "src/dist/mod.rs",
    ] {
        assert!(
            read(tmp.path(), module).contains("with_pool_untracked("),
            "{module} is an app module, not a build tree"
        );
    }
}

#[test]
fn build_directories_at_a_workspace_member_root_are_still_skipped() {
    // The rule is "at a crate root", so a member's own `target/` is skipped
    // just as the workspace root's is.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\"]\n\n\
         [workspace.dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(tmp.path(), "api/src/lib.rs", USES_WITH_POOL);
    write(tmp.path(), "api/target/debug/gen.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(read(tmp.path(), "api/src/lib.rs").contains("with_pool_untracked("));
    assert!(
        read(tmp.path(), "api/target/debug/gen.rs").contains("with_pool(pool.clone())"),
        "a member's own build output is not app source"
    );
}

#[test]
fn a_symlinked_source_directory_is_reported_rather_than_ignored() {
    // A linked `src/` has `is_dir() == false`, so an extension-gated check saw
    // nothing at all: no traversal, no report, and a run that says "nothing to
    // change" about an app it never looked at.
    #[cfg(not(unix))]
    return;
    #[cfg(unix)]
    {
        let tmp = app("0.5.0");
        write(tmp.path(), "real/lib.rs", USES_WITH_POOL);
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("src"))
            .expect("symlink");

        let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
        assert!(output.status.success(), "{}", report(&output));
        let out = stdout_of(&output);
        assert!(out.contains("src"), "the linked directory is named: {out}");
        assert!(out.contains("symbolic link"), "{out}");
        assert!(
            !out.to_lowercase().contains("nothing to change"),
            "a linked source tree must not read as a clean app: {out}"
        );
    }
}

#[test]
fn running_inside_a_member_resolves_the_inherited_workspace_version() {
    // `autumn upgrade` pointed at a member, not the workspace root. The
    // member's only declaration is `{ workspace = true }`, and the entry it
    // inherits lives one directory *up* — outside the scanned tree. Cargo walks
    // up to find it; refusing to do so aborted over a version Cargo resolves.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\"]\n\n\
         [workspace.dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = { workspace = true }\n",
    );
    write(tmp.path(), "api/src/lib.rs", USES_WITH_POOL);

    let member = tmp.path().join("api");
    let output = run_upgrade(&member, &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "api/src/lib.rs").contains("with_pool_untracked("),
        "the inherited 0.5.0 puts the member in range:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_unresolvable_inherited_dependency_asks_for_from() {
    // `{ workspace = true }` with no enclosing workspace to resolve it. The
    // manifest would not build, so guessing a range for it is not an option.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = { workspace = true }\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--from"),
        "{}",
        report(&output),
    );
}

#[test]
fn source_in_a_hidden_directory_is_migrated_not_dropped() {
    // `#[path = ".generated/repositories.rs"] mod repositories;` is legal, and
    // the file behind it is compiled app code. Skipping every dot-directory
    // left it on the old API with nothing in the report to say so.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/main.rs",
        "#[path = \"../.generated/repositories.rs\"]\nmod repositories;\n\nfn main() {}\n",
    );
    write(tmp.path(), ".generated/repositories.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), ".generated/repositories.rs").contains("with_pool_untracked("),
        "compiled source in a hidden directory must be migrated:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn tool_and_vcs_metadata_directories_are_still_skipped() {
    // The exclusion is by name now, so the dot-directories that hold metadata
    // rather than app code stay out — without reporting noise on every run.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    for metadata in [
        ".git/hooks/x.rs",
        ".github/scripts/x.rs",
        ".cargo/registry/x.rs",
        ".vscode/x.rs",
    ] {
        write(tmp.path(), metadata, USES_WITH_POOL);
    }

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    for metadata in [
        ".git/hooks/x.rs",
        ".github/scripts/x.rs",
        ".cargo/registry/x.rs",
        ".vscode/x.rs",
    ] {
        assert!(
            read(tmp.path(), metadata).contains("with_pool(pool.clone())"),
            "{metadata} is metadata, not app source"
        );
    }
    assert!(
        !stdout_of(&output).contains(".git"),
        "and metadata is skipped quietly, not reported on every run"
    );
}

#[test]
fn a_hidden_workspace_member_still_decides_the_floor() {
    // The source walk now scans non-metadata hidden directories, so a crate
    // under one gets its sources rewritten. The manifest walk has to agree:
    // reading only the root's 0.6.0 selects no migration at all, and the hidden
    // crate's call sites — scanned, planned, and left alone — stay on the old
    // name with the report claiming there was nothing to do.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.6.0\"\n",
    );
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    write(
        tmp.path(),
        ".generated-app/Cargo.toml",
        "[package]\nname = \"generated\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(tmp.path(), ".generated-app/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), ".generated-app/src/lib.rs").contains("with_pool_untracked("),
        "a hidden crate the walk scans must also vote on the floor:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_inherited_aliased_dependency_resolves_through_the_workspace_key() {
    // The member says `autumn = { workspace = true }` — no literal `autumn-web`
    // key and no `package` field of its own. Only the workspace entry names the
    // real crate. Matching the member's key against that entry is the only way
    // to see the dependency Cargo resolves without trouble.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\"]\n\n\
         [workspace.dependencies]\n\
         autumn = { package = \"autumn-web\", version = \"0.5.0\" }\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn = { workspace = true }\n",
    );
    write(tmp.path(), "api/src/lib.rs", USES_WITH_POOL);

    let member = tmp.path().join("api");
    let output = run_upgrade(&member, &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "api/src/lib.rs").contains("with_pool_untracked("),
        "the aliased workspace entry records 0.5.0:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_inherited_dependency_on_another_crate_is_not_mistaken_for_autumn_web() {
    // Resolving inherited entries by key must not turn every `{ workspace =
    // true }` line into an autumn-web declaration: this app inherits `serde`
    // and declares autumn-web outright, and the serde entry has no bearing on
    // the floor.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\"]\n\n\
         [workspace.dependencies]\nserde = \"1\"\n",
    );
    write(
        tmp.path(),
        "api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nserde = { workspace = true }\nautumn-web = \"0.5.0\"\n",
    );
    write(tmp.path(), "api/src/lib.rs", USES_WITH_POOL);

    let member = tmp.path().join("api");
    let output = run_upgrade(&member, &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "api/src/lib.rs").contains("with_pool_untracked("),
        "an inherited serde must not make the autumn-web version unreadable:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_path_typo_is_reported_as_a_bad_path_not_a_missing_dependency() {
    // `recorded_version` on a nonexistent directory finds nothing, so running
    // it first turned an ordinary typo into "add an `autumn-web` dependency to
    // Cargo.toml" — advice about a manifest that is not there either. The scan
    // root is checked before anything is read from it.
    let tmp = TempDir::new().expect("tempdir");

    let output = run_upgrade(tmp.path(), &["no-such-directory", "--to", "0.6.0"]);
    assert!(!output.status.success(), "{}", report(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not a readable directory"),
        "a path typo names the path:\n{}",
        report(&output)
    );
    assert!(
        !stderr.contains("Add an `autumn-web` dependency"),
        "and does not send the user to a manifest that does not exist:\n{}",
        report(&output)
    );
}

#[test]
#[cfg(unix)]
fn a_scan_root_that_cannot_be_read_is_a_failure_not_an_empty_report() {
    // `is_dir()` answers "yes" for a directory with no read permission, so the
    // root landed in `skipped` and the run exited 0 having examined nothing --
    // a `--apply` that migrated no file at all, reported as success.
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().expect("tempdir");
    let locked = tmp.path().join("locked");
    fs::create_dir(&locked).unwrap();
    write(&locked, "src/main.rs", USES_WITH_POOL);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    // Root ignores directory permissions, so there is nothing to assert when
    // the suite runs as root. Restore the mode either way so the tempdir can be
    // cleaned up.
    let enforced = fs::read_dir(&locked).is_err();
    let output = run_upgrade(
        tmp.path(),
        &["locked", "--from", "0.5.0", "--to", "0.6.0", "--apply"],
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if !enforced {
        return;
    }

    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a readable directory"),
        "an unreadable root is the bad-root exit, not an empty report:\n{}",
        report(&output)
    );
}

#[test]
fn a_redirected_cargo_target_directory_is_still_build_output() {
    // `CARGO_TARGET_DIR=build` puts build output somewhere the `target` name
    // check never looks, so the walk descended into generated code and `--apply`
    // rewrote artifacts that the next `cargo build` overwrites anyway.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    write(tmp.path(), "build/debug/build/generated.rs", USES_WITH_POOL);

    let output = run_upgrade_with_env(
        tmp.path(),
        &["--to", "0.6.0", "--apply"],
        &[("CARGO_TARGET_DIR", "build")],
    );
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "app source is still migrated:\n{}",
        stdout_of(&output)
    );
    assert!(
        read(tmp.path(), "build/debug/build/generated.rs").contains("with_pool(pool.clone())"),
        "the configured target directory is build output:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_target_directory_configured_in_cargo_config_is_still_build_output() {
    // The same redirection, written down in `.cargo/config.toml` instead of the
    // environment -- the form a project commits rather than exports.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    write(
        tmp.path(),
        ".cargo/config.toml",
        "[build]\ntarget-dir = \"out\"\n",
    );
    write(tmp.path(), "out/debug/build/generated.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "app source is still migrated:\n{}",
        stdout_of(&output)
    );
    assert!(
        read(tmp.path(), "out/debug/build/generated.rs").contains("with_pool(pool.clone())"),
        "a committed target-dir redirect is build output too:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_module_named_like_a_redirected_target_directory_is_still_migrated() {
    // The exclusion follows the *configured path*, not the name. With the target
    // directory redirected to `out/`, an unrelated `src/out/mod.rs` is ordinary
    // app code and must be migrated like any other module.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    write(tmp.path(), "src/out/mod.rs", USES_WITH_POOL);
    write(
        tmp.path(),
        ".cargo/config.toml",
        "[build]\ntarget-dir = \"out\"\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/out/mod.rs").contains("with_pool_untracked("),
        "only the configured path is build output, not the name:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_nested_crate_redirects_its_own_target_directory() {
    // Cargo reads `.cargo/config.toml` from the invocation directory upward, so
    // a nested crate redirects its *own* output. Resolving the redirect once at
    // the scan root left `tools/helper/build/` looking like app source.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    write(
        tmp.path(),
        "tools/helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        tmp.path(),
        "tools/helper/.cargo/config.toml",
        "[build]\ntarget-dir = \"build\"\n",
    );
    write(tmp.path(), "tools/helper/src/main.rs", USES_WITH_POOL);
    write(
        tmp.path(),
        "tools/helper/build/debug/generated.rs",
        USES_WITH_POOL,
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "tools/helper/src/main.rs").contains("with_pool_untracked("),
        "the nested crate's own source is still app code:\n{}",
        stdout_of(&output)
    );
    assert!(
        read(tmp.path(), "tools/helper/build/debug/generated.rs")
            .contains("with_pool(pool.clone())"),
        "the nested crate's configured output directory is build output:\n{}",
        stdout_of(&output)
    );
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "and the redirect does not leak upward:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn an_environment_target_redirect_wins_over_a_nested_config() {
    // `CARGO_TARGET_DIR` overrides every `.cargo/config.toml`, so a nested
    // crate's `target-dir` is not in effect at all when it is set — that
    // directory is ordinary source and must be migrated like any other.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    write(
        tmp.path(),
        "tools/helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        tmp.path(),
        "tools/helper/.cargo/config.toml",
        "[build]\ntarget-dir = \"build\"\n",
    );
    write(tmp.path(), "tools/helper/build/mod.rs", USES_WITH_POOL);
    write(tmp.path(), "out/debug/generated.rs", USES_WITH_POOL);

    let output = run_upgrade_with_env(
        tmp.path(),
        &["--to", "0.6.0", "--apply"],
        &[("CARGO_TARGET_DIR", "out")],
    );
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "tools/helper/build/mod.rs").contains("with_pool_untracked("),
        "the overridden config is not in effect:\n{}",
        stdout_of(&output)
    );
    assert!(
        read(tmp.path(), "out/debug/generated.rs").contains("with_pool(pool.clone())"),
        "the environment redirect is where output actually lands:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_nested_workspaces_member_resolves_against_its_own_workspace() {
    // A whole workspace sitting inside the scanned tree. Its member inherits
    // with the literal key, and the entry lives in `tools/ws/Cargo.toml` --
    // *between* the member and the scan root. Resolving inheritance from the
    // scan root's ancestors instead of the member's finds nothing, and an
    // unresolved literal `autumn-web` is treated as a declaration with no
    // readable version, which fails detection for the entire run.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.6.0\"\n",
    );
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    write(
        tmp.path(),
        "tools/ws/Cargo.toml",
        "[workspace]\nmembers = [\"api\"]\n\n\
         [workspace.dependencies]\nautumn-web = \"0.5.0\"\n",
    );
    write(
        tmp.path(),
        "tools/ws/api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = { workspace = true }\n",
    );
    write(tmp.path(), "tools/ws/api/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "tools/ws/api/src/lib.rs").contains("with_pool_untracked("),
        "the nested workspace's own entry records 0.5.0:\n{}",
        report(&output)
    );
}

#[test]
fn guide_locations_are_printed_as_urls_the_user_can_open() {
    // The registry stores repo-relative paths so the release gate can check
    // them against the tree. Printed verbatim by an installed CLI running in
    // someone's app, `docs/migrations/0.6.0.md#...` names a file in *their*
    // project -- a dead end at exactly the point the report is telling them to
    // go read something.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains(
            "https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/0.6.0.md#"
        ),
        "guide locations are openable URLs:\n{stdout}"
    );
    assert!(
        !stdout.contains(" docs/migrations/"),
        "and no bare repo-relative path is left in the report:\n{stdout}"
    );
}

#[test]
fn list_migrations_prints_guide_urls_too() {
    let tmp = app("0.5.0");
    let output = run_upgrade(tmp.path(), &["--list-migrations"]);
    assert!(output.status.success(), "{}", report(&output));
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/"),
        "{stdout}"
    );
    assert!(!stdout.contains(" docs/migrations/"), "{stdout}");
}

#[test]
fn json_keeps_the_repo_relative_guide_and_adds_the_url() {
    // Automation pinned to the registry's own paths keeps working; the URL is
    // additive.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--json"]);
    assert!(output.status.success(), "{}", report(&output));
    let parsed: serde_json::Value = serde_json::from_str(&stdout_of(&output)).expect("json");
    let migration = &parsed["migrations"][0];
    assert!(
        migration["guide"]
            .as_str()
            .expect("guide")
            .starts_with("docs/migrations/"),
        "{parsed:#}"
    );
    assert!(
        migration["guide_url"]
            .as_str()
            .expect("guide_url")
            .starts_with("https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/"),
        "{parsed:#}"
    );
}

#[test]
fn a_wildcard_version_requirement_has_a_floor() {
    // `autumn-web = "0.5.*"` is a documented Cargo requirement whose floor is
    // unambiguously 0.5.0 -- the same floor as `"0.5"`, which is already
    // accepted. Refusing it sent the user to `--from` for no reason.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.5.*\"\n",
    );
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "0.5.* floors at 0.5.0:\n{}",
        report(&output)
    );
}

#[test]
fn a_bare_wildcard_requirement_still_has_no_floor() {
    // `*` accepts every release, so it says nothing about which one the app is
    // on. Reading it as 0.0.0 would silently select every migration ever
    // written; asking for `--from` is the honest answer.
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
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--from"),
        "{}",
        report(&output)
    );
}

/// A `#[repository]` trait, which is what makes `Pg{Trait}` a generated type.
fn repository_trait(name: &str) -> String {
    format!(
        "use autumn_web::prelude::*;\n\n\
         #[repository]\npub trait {name} {{\n    fn noop(&self);\n}}\n"
    )
}

#[test]
fn a_receiver_no_repository_trait_generates_is_reported_not_rewritten() {
    // `PgAuditRepository` follows the generated naming convention exactly, but
    // nothing in this app declares `#[repository] trait AuditRepository`, so the
    // type is the app's own. Rewriting it produces a call to a method that does
    // not exist; the convention alone is not evidence.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/repositories.rs",
        &repository_trait("PostRepository"),
    );
    write(
        tmp.path(),
        "src/audit.rs",
        "pub struct PgAuditRepository;\n\n\
         impl PgAuditRepository {\n    \
         pub fn with_pool(_pool: Pool) -> Self {\n        Self\n    }\n}\n\n\
         pub fn build(pool: Pool) -> PgAuditRepository {\n    \
         PgAuditRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/audit.rs").contains("PgAuditRepository::with_pool(pool)"),
        "an app's own type must not be rewritten:\n{}",
        stdout_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("src/audit.rs:10"),
        "and it must be reported, not silently skipped:\n{stdout}"
    );
}

#[test]
fn a_receiver_backed_by_a_repository_trait_in_another_file_is_rewritten() {
    // The trait and the call site normally live in different modules, so the
    // evidence has to be collected across the whole scan rather than per file.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/repositories.rs",
        &repository_trait("PostRepository"),
    );
    write(
        tmp.path(),
        "src/routes.rs",
        "pub fn build(pool: Pool) -> PgPostRepository {\n    \
         PgPostRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/routes.rs").contains("with_pool_untracked(pool)"),
        "a trait declared elsewhere in the app still generates this type:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_qualified_repository_attribute_is_recognised() {
    // `#[autumn_web::repository(...)]` is what the scaffold itself emits, so
    // this is the *common* spelling, not an exotic one. Requiring the
    // attribute's first token to be `repository` missed it, and every call site
    // in a scaffolded app was then classified as an unverified receiver and
    // left on the old name.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/repositories.rs",
        "use autumn_web::prelude::*;\n\n\
         #[autumn_web::repository(Post, table = \"posts\")]\n\
         pub trait PostRepository {\n    fn noop(&self);\n}\n\n\
         pub fn build(pool: Pool) -> PgPostRepository {\n    \
         PgPostRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/repositories.rs").contains("with_pool_untracked(pool)"),
        "the qualified spelling generates the type just the same:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_same_named_type_in_another_module_is_not_verified_by_the_real_one() {
    // `#[repository] trait AuditRepository` in `repositories` makes
    // `PgAuditRepository` a generated type *there*. An app is free to have an
    // unrelated type of the same name elsewhere, and a call written against
    // that one names the module it came from.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/repositories.rs",
        "use autumn_web::prelude::*;\n\n\
         #[repository]\npub trait AuditRepository {\n    fn noop(&self);\n}\n",
    );
    write(
        tmp.path(),
        "src/custom.rs",
        "pub struct PgAuditRepository;\n\n\
         impl PgAuditRepository {\n    \
         pub fn with_pool(_pool: Pool) -> Self {\n        Self\n    }\n}\n",
    );
    write(
        tmp.path(),
        "src/main.rs",
        "mod custom;\nmod repositories;\n\n\
         pub fn build(pool: Pool) -> custom::PgAuditRepository {\n    \
         custom::PgAuditRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("custom::PgAuditRepository::with_pool(pool)"),
        "a receiver qualified with a module that generates nothing must not be rewritten:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_module_qualified_generated_receiver_is_still_rewritten() {
    // The common way to write it once the trait lives in its own module. This
    // must keep working: scoping the check must not cost the ordinary path.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/repositories.rs",
        "use autumn_web::prelude::*;\n\n\
         #[repository]\npub trait PostRepository {\n    fn noop(&self);\n}\n",
    );
    write(
        tmp.path(),
        "src/main.rs",
        "mod repositories;\n\n\
         pub fn a(pool: Pool) -> repositories::PgPostRepository {\n    \
         repositories::PgPostRepository::with_pool(pool)\n}\n\n\
         pub fn b(pool: Pool) -> crate::repositories::PgPostRepository {\n    \
         crate::repositories::PgPostRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let after = read(tmp.path(), "src/main.rs");
    assert!(
        after.contains("repositories::PgPostRepository::with_pool_untracked(pool)"),
        "a module-qualified generated receiver is still the generated type:\n{}",
        stdout_of(&output)
    );
    assert_eq!(
        after.matches("with_pool_untracked").count(),
        2,
        "both the relative and the `crate::` spelling:\n{after}"
    );
}

#[test]
fn an_inline_module_qualifier_matching_the_declaration_is_rewritten() {
    // The trait declared in an inline `mod` block, called with that module as
    // the qualifier from the file's top level.
    let tmp = app("0.5.0");
    write(
        tmp.path(),
        "src/main.rs",
        "use autumn_web::prelude::*;\n\n\
         mod repositories {\n    \
         #[repository]\n    pub trait PostRepository {\n        fn noop(&self);\n    }\n}\n\n\
         pub fn build(pool: Pool) -> repositories::PgPostRepository {\n    \
         repositories::PgPostRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked(pool)"),
        "{}",
        stdout_of(&output)
    );
}

#[test]
fn a_locally_defined_type_is_not_verified_by_another_crates_trait() {
    // Two crates in one scan. `api` declares `#[repository] trait
    // AuditRepository`, so `PgAuditRepository` is generated *there*. `tools`
    // defines its own struct of that name and calls it unqualified. A global
    // name set verified the wrong one.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\", \"tools\"]\n",
    );
    for member in ["api", "tools"] {
        write(
            tmp.path(),
            &format!("{member}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        );
    }
    write(
        tmp.path(),
        "api/src/lib.rs",
        "use autumn_web::prelude::*;\n\n\
         #[repository]\npub trait AuditRepository {\n    fn noop(&self);\n}\n",
    );
    write(
        tmp.path(),
        "tools/src/lib.rs",
        "pub struct PgAuditRepository;\n\n\
         impl PgAuditRepository {\n    \
         pub fn with_pool(_pool: Pool) -> Self {\n        Self\n    }\n}\n\n\
         pub fn build(pool: Pool) -> PgAuditRepository {\n    \
         PgAuditRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "tools/src/lib.rs").contains("PgAuditRepository::with_pool(pool)"),
        "a hand-written type must not be verified by a trait elsewhere:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_qualified_receiver_is_vetoed_by_a_same_pathed_handwritten_type() {
    // Crate `api` generates `repositories::PgAuditRepository`; crate `tools`
    // writes its own type at the same module path and calls it with that path.
    // Matching the qualifier against generated declarations alone accepted the
    // wrong crate's evidence.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"api\", \"tools\"]\n",
    );
    for member in ["api", "tools"] {
        write(
            tmp.path(),
            &format!("{member}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\nautumn-web = \"0.5.0\"\n"
            ),
        );
    }
    write(
        tmp.path(),
        "api/src/repositories.rs",
        "use autumn_web::prelude::*;\n\n\
         #[repository]\npub trait AuditRepository {\n    fn noop(&self);\n}\n",
    );
    write(
        tmp.path(),
        "tools/src/repositories.rs",
        "pub struct PgAuditRepository;\n\n\
         impl PgAuditRepository {\n    \
         pub fn with_pool(_pool: Pool) -> Self {\n        Self\n    }\n}\n",
    );
    write(
        tmp.path(),
        "tools/src/lib.rs",
        "pub mod repositories;\n\n\
         pub fn build(pool: Pool) -> repositories::PgAuditRepository {\n    \
         repositories::PgAuditRepository::with_pool(pool)\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "tools/src/lib.rs")
            .contains("repositories::PgAuditRepository::with_pool(pool)"),
        "a hand-written type at the same module path makes the qualifier ambiguous:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_target_directory_pointing_outside_its_crate_is_still_excluded() {
    // `target-dir = "../../build"` puts the output beside the workspace root,
    // not under the crate that configured it. Carrying the override only while
    // descending never reaches that sibling directory.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    write(
        tmp.path(),
        "tools/helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        tmp.path(),
        "tools/helper/.cargo/config.toml",
        "[build]\ntarget-dir = \"../../build\"\n",
    );
    write(tmp.path(), "build/debug/generated.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "app source is still migrated:\n{}",
        stdout_of(&output)
    );
    assert!(
        read(tmp.path(), "build/debug/generated.rs").contains("with_pool(pool.clone())"),
        "a target directory outside the declaring crate is still build output:\n{}",
        stdout_of(&output)
    );
}

#[test]
fn a_syntactically_invalid_file_is_skipped_not_rewritten() {
    // `let = …;` tokenises perfectly well — balanced delimiters, valid tokens —
    // but it is not valid Rust. Accepting the token stream as proof of validity
    // meant rewriting a file the compiler will reject anyway, and counting it
    // as migrated.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    write(
        tmp.path(),
        "src/broken.rs",
        "#[repository]\npub trait PostRepository { fn noop(&self); }\n\n\
         pub fn build(pool: Pool) {\n    let = PgPostRepository::with_pool(pool);\n}\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/broken.rs").contains("let = PgPostRepository::with_pool(pool);"),
        "a file that is not valid Rust must be left alone:\n{}",
        stdout_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("src/broken.rs"),
        "and reported under Skipped:\n{stdout}"
    );
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "one unparsable file does not stop the rest:\n{stdout}"
    );
}

#[test]
fn an_unparsable_member_manifest_fails_detection() {
    // A member whose `Cargo.toml` cannot be parsed may well be the oldest crate
    // in the workspace. Reading it as "declares nothing" let a newer sibling
    // pick the floor and skip the migration its sources still need.
    let tmp = TempDir::new().expect("tempdir");
    write(
        tmp.path(),
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.6.0\"\n",
    );
    write(tmp.path(), "src/main.rs", "fn main() {}\n");
    write(tmp.path(), "member/Cargo.toml", "[package\nname = broken\n");
    write(tmp.path(), "member/src/lib.rs", USES_WITH_POOL);

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(!output.status.success(), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--from"),
        "a manifest that cannot be read is not evidence of absence:\n{}",
        report(&output)
    );
}

#[test]
fn a_config_inside_build_output_does_not_exclude_app_source() {
    // Target-directory discovery walks the tree looking for `.cargo/config.toml`.
    // A populated target tree is huge, and anything inside it is build output --
    // including a config that got copied there. Descending into a directory
    // already known to be output let such a file exclude `src/` itself.
    let tmp = app("0.5.0");
    write(tmp.path(), "src/main.rs", USES_WITH_POOL);
    write(
        tmp.path(),
        ".cargo/config.toml",
        "[build]\ntarget-dir = \"out\"\n",
    );
    write(tmp.path(), "out/debug/generated.rs", USES_WITH_POOL);
    // A stray config inside the output tree, naming the app's own source.
    write(
        tmp.path(),
        "out/debug/vendored/.cargo/config.toml",
        // Three levels up from `out/debug/vendored` is the project root.
        "[build]\ntarget-dir = \"../../../src\"\n",
    );

    let output = run_upgrade(tmp.path(), &["--to", "0.6.0", "--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        read(tmp.path(), "src/main.rs").contains("with_pool_untracked("),
        "app source must not be excluded by a config found inside build output:\n{}",
        stdout_of(&output)
    );
    assert!(
        read(tmp.path(), "out/debug/generated.rs").contains("with_pool(pool.clone())"),
        "and the configured output directory is still excluded:\n{}",
        stdout_of(&output)
    );
}
