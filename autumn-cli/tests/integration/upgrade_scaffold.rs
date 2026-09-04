//! Integration tests for `autumn upgrade`'s scaffold-file reconciliation
//! (issue #1593).
//!
//! Each test scaffolds a real project with the real `autumn` binary, ages it —
//! deleting a file the old release did not have, editing one, rewinding the
//! recorded baseline — and then runs the real binary against it. Nothing is
//! mocked: the whole point of the feature is that what `autumn new` writes and
//! what `autumn upgrade` considers current are the same bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

const MANIFEST: &str = ".autumn/scaffold.toml";

fn report(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("upgrade")
        .args(args)
        .current_dir(root)
        .output()
        .expect("failed to run autumn upgrade")
}

/// A freshly scaffolded project, plus its root.
fn new_project(name: &str, extra: &[&str]) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let output = Command::new(autumn_bin())
        .arg("new")
        .arg(name)
        .args(extra)
        .current_dir(tmp.path())
        .output()
        .expect("failed to run autumn new");
    assert!(output.status.success(), "{}", report(&output));
    let root = tmp.path().join(name);
    (tmp, root)
}

/// Rewind the recorded baseline to `version` and forget `paths`, so the project
/// looks like one scaffolded by a release that never generated them.
fn age_to(root: &Path, version: &str, paths: &[&str]) {
    use std::fmt::Write as _;

    let text = fs::read_to_string(root.join(MANIFEST)).expect("manifest");
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with("version = ") {
            let _ = writeln!(out, "version = \"{version}\"");
            continue;
        }
        if paths
            .iter()
            .any(|path| line.starts_with(&format!("\"{path}\" =")))
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    fs::write(root.join(MANIFEST), out).unwrap();
    for path in paths {
        let _ = fs::remove_file(root.join(path));
    }
}

#[test]
fn a_fresh_project_reports_no_scaffold_drift() {
    let (_tmp, root) = new_project("fresh", &[]);
    let output = run(&root, &[]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("Scaffold files"), "{out}");
    assert!(out.contains("up to date"), "{out}");
}

#[test]
fn an_older_project_is_offered_the_files_this_release_added() {
    let (_tmp, root) = new_project("aged", &[]);
    age_to(&root, "0.5.0", &["rust-toolchain.toml", "clippy.toml"]);

    let output = run(&root, &[]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("rust-toolchain.toml"), "{out}");
    assert!(out.contains("clippy.toml"), "{out}");
    assert!(out.contains("add"), "{out}");
    // Preview only: nothing on disk changed.
    assert!(!root.join("rust-toolchain.toml").exists(), "{out}");
    assert!(!root.join("clippy.toml").exists(), "{out}");
    assert!(out.contains("--apply"), "{out}");
}

#[test]
fn apply_writes_the_offered_files() {
    let (_tmp, root) = new_project("applied", &[]);
    age_to(&root, "0.5.0", &["rust-toolchain.toml"]);

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        root.join("rust-toolchain.toml").is_file(),
        "{}",
        report(&output)
    );

    // ...and the project is clean afterwards.
    let after = run(&root, &["--check"]);
    assert!(after.status.success(), "{}", report(&after));
}

#[test]
fn an_edited_file_is_a_conflict_and_survives_apply_byte_for_byte() {
    let (_tmp, root) = new_project("edited", &[]);
    let mine = "FROM scratch\n# hand-tuned, do not touch\n";
    fs::write(root.join("Dockerfile"), mine).unwrap();
    age_to(&root, "0.5.0", &[]);

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("conflict"), "{out}");
    assert!(out.contains("Dockerfile"), "{out}");
    assert_eq!(
        fs::read_to_string(root.join("Dockerfile")).unwrap(),
        mine,
        "an edited file must never be overwritten"
    );
    // The report tells the reader how to undo whatever it *did* write.
    assert!(out.contains("git diff"), "{out}");
}

#[test]
fn application_source_is_never_touched() {
    let (_tmp, root) = new_project("untouched", &[]);
    age_to(&root, "0.5.0", &["rust-toolchain.toml"]);
    let main_rs = fs::read_to_string(root.join("src/main.rs")).unwrap();
    let tests_rs = fs::read_to_string(root.join("tests/integration_test.rs")).unwrap();
    let cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let readme = fs::read_to_string(root.join("README.md")).unwrap();

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        main_rs
    );
    assert_eq!(
        fs::read_to_string(root.join("tests/integration_test.rs")).unwrap(),
        tests_rs
    );
    assert_eq!(fs::read_to_string(root.join("Cargo.toml")).unwrap(), cargo);
    assert_eq!(fs::read_to_string(root.join("README.md")).unwrap(), readme);
    assert!(!stdout_of(&output).contains("src/main.rs"));
}

#[test]
fn check_exits_nonzero_on_drift_and_zero_when_clean() {
    let (_tmp, root) = new_project("gated", &[]);
    let clean = run(&root, &["--check"]);
    assert!(clean.status.success(), "{}", report(&clean));

    age_to(&root, "0.5.0", &["rustfmt.toml"]);
    let dirty = run(&root, &["--check"]);
    assert!(!dirty.status.success(), "{}", report(&dirty));
    assert_eq!(dirty.status.code(), Some(3), "{}", report(&dirty));
    assert!(
        stdout_of(&dirty).contains("rustfmt.toml"),
        "{}",
        report(&dirty)
    );
    // A check never writes.
    assert!(!root.join("rustfmt.toml").exists());
}

#[test]
fn check_and_apply_together_are_a_usage_error() {
    let (_tmp, root) = new_project("both", &[]);
    let output = run(&root, &["--check", "--apply"]);
    assert_eq!(output.status.code(), Some(2), "{}", report(&output));
}

#[test]
fn check_outside_an_autumn_project_says_so() {
    let tmp = TempDir::new().unwrap();
    let output = run(tmp.path(), &["--check"]);
    assert_eq!(output.status.code(), Some(2), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Autumn project"),
        "{}",
        report(&output)
    );
}

#[test]
fn a_project_predating_provenance_still_upgrades_best_effort() {
    let (_tmp, root) = new_project("legacy", &[]);
    // A pre-#1593 app: no manifest at all, missing a file a later release
    // added, and carrying its own edit to another.
    fs::remove_dir_all(root.join(".autumn")).unwrap();
    fs::remove_file(root.join("rust-toolchain.toml")).unwrap();
    fs::write(root.join("clippy.toml"), "# mine\n").unwrap();

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    // Best effort: the missing file is added...
    assert!(root.join("rust-toolchain.toml").is_file(), "{out}");
    // ...and the one that differs is a conflict, not an overwrite.
    assert_eq!(
        fs::read_to_string(root.join("clippy.toml")).unwrap(),
        "# mine\n"
    );
    assert!(out.contains("conflict"), "{out}");
    assert!(out.contains("no recorded baseline"), "{out}");
}

#[test]
fn the_summary_links_this_release_upgrade_guide() {
    let (_tmp, root) = new_project("guided", &[]);
    let out = stdout_of(&run(&root, &[]));
    assert!(out.contains("Upgrade guide:"), "{out}");
    assert!(
        out.contains("https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/migrations/"),
        "{out}"
    );
}

#[test]
fn the_json_report_carries_the_scaffold_section() {
    let (_tmp, root) = new_project("machine", &[]);
    age_to(&root, "0.5.0", &["rustfmt.toml"]);

    let output = run(&root, &["--json"]);
    assert!(output.status.success(), "{}", report(&output));
    let value: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("json report parses");
    let scaffold = &value["scaffold"];
    assert_eq!(scaffold["drift"], true, "{value}");
    assert_eq!(scaffold["baseline"], "0.5.0", "{value}");
    let files = scaffold["files"].as_array().expect("files array");
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "rustfmt.toml" && file["status"] == "add"),
        "{value}"
    );
    // The app-code report is still there and untouched by this addition.
    assert!(value["migrations"].is_array(), "{value}");
}

#[test]
fn a_directory_that_is_not_an_autumn_project_gets_no_scaffold_section() {
    // `autumn upgrade` also runs over plain Rust crates that merely depend on
    // autumn-web. Offering to seed a Dockerfile into one would be nonsense.
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"plain\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nautumn-web = \"0.6.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    let output = run(tmp.path(), &[]);
    assert!(output.status.success(), "{}", report(&output));
    assert!(
        !stdout_of(&output).contains("Scaffold files"),
        "{}",
        report(&output)
    );
}

#[test]
fn a_deliberately_deleted_file_is_reported_but_not_restored() {
    let (_tmp, root) = new_project("deleted", &[]);
    fs::remove_file(root.join(".env.example")).unwrap();

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains(".env.example"), "{out}");
    assert!(out.contains("removed"), "{out}");
    assert!(
        !root.join(".env.example").exists(),
        "a deliberate deletion must not be undone: {out}"
    );
    // ...and it does not hold a CI gate red forever.
    let check = run(&root, &["--check"]);
    assert!(check.status.success(), "{}", report(&check));
}

#[test]
fn an_api_project_is_never_offered_fullstack_files() {
    let (_tmp, root) = new_project("apiapp", &["--api"]);
    age_to(&root, "0.5.0", &[]);
    let out = stdout_of(&run(&root, &["--apply"]));
    assert!(!out.contains("tailwind.config.js"), "{out}");
    assert!(!root.join("tailwind.config.js").exists(), "{out}");
    assert!(!root.join("static/css/input.css").exists(), "{out}");
}

#[test]
fn a_file_that_is_not_utf8_is_never_treated_as_missing_and_overwritten() {
    // The population this protects is every project that exists today: without
    // a provenance manifest there is no baseline, so a file misread as absent
    // would be classified `add` and truncated. `read_to_string` fails on
    // non-UTF-8 exactly as it does on a missing file, and those two must not
    // reach the classifier as the same answer.
    let (_tmp, root) = new_project("binary", &[]);
    fs::remove_dir_all(root.join(".autumn")).unwrap();
    let mine: &[u8] = &[0xff, 0xfe, b'A', 0x00, b'B', 0x00];
    fs::write(root.join(".env.example"), mine).unwrap();

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains(".env.example"), "{out}");
    assert!(out.contains("conflict"), "{out}");
    assert_eq!(
        fs::read(root.join(".env.example")).unwrap(),
        mine,
        "an unreadable file must survive --apply byte for byte"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_scaffold_file_is_never_written_through() {
    // A monorepo hoists shared config and symlinks it back into each crate.
    // Writing through the link edits a file outside the project, which
    // `git diff` inside the project would not even show.
    let (tmp, root) = new_project("linked", &[]);
    let shared = tmp.path().join("shared-rustfmt.toml");
    fs::write(&shared, "edition = \"2015\"\n").unwrap();
    fs::remove_file(root.join("rustfmt.toml")).unwrap();
    std::os::unix::fs::symlink(&shared, root.join("rustfmt.toml")).unwrap();

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains("rustfmt.toml"), "{out}");
    assert!(out.contains("symlink"), "{out}");
    assert_eq!(
        fs::read_to_string(&shared).unwrap(),
        "edition = \"2015\"\n",
        "the link target lives outside the project and must never be written"
    );
    assert!(
        root.join("rustfmt.toml")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link itself must be left in place"
    );
}

#[test]
fn a_to_version_does_not_pretend_to_reconcile_a_historical_scaffold() {
    // This CLI ships one set of scaffold templates: its own. `--to` selects
    // which codemods run; downgrades and historical scaffolds are out of scope,
    // and a report claiming otherwise would be false.
    let (_tmp, root) = new_project("pinned", &[]);
    let output = run(&root, &["--to", "0.6.0"]);
    assert!(output.status.success(), "{}", report(&output));
    let out = stdout_of(&output);
    let scaffold_line = out
        .lines()
        .find(|line| line.starts_with("Scaffold files"))
        .unwrap_or_else(|| panic!("no scaffold header in:\n{out}"));
    assert!(
        scaffold_line.contains(env!("CARGO_PKG_VERSION")),
        "{scaffold_line}"
    );
    assert!(!scaffold_line.contains("0.6.0"), "{scaffold_line}");
}

#[test]
fn check_mode_does_not_print_file_contents_into_the_build_log() {
    // `--check` is documented as a CI gate, and `autumn.toml` / `.env.example`
    // are where people put connection strings. The gate needs the verdict and
    // the file names, not the working contents.
    let (_tmp, root) = new_project("quiet", &[]);
    fs::write(
        root.join(".env.example"),
        "DATABASE_URL=postgres://user:hunter2@db.internal/prod\n",
    )
    .unwrap();

    let output = run(&root, &["--check"]);
    assert_eq!(output.status.code(), Some(3), "{}", report(&output));
    let out = stdout_of(&output);
    assert!(out.contains(".env.example"), "{out}");
    assert!(out.contains("conflict"), "{out}");
    assert!(!out.contains("hunter2"), "{out}");
    // ...while the full report, which a human asked for, still shows the diff.
    let full = stdout_of(&run(&root, &[]));
    assert!(full.contains("hunter2"), "{full}");
}

#[test]
fn check_does_not_pass_a_project_whose_scaffold_it_could_not_render() {
    // A gate that goes green because the tool could not look is worse than no
    // gate. Without a usable `[package] name` the scaffold cannot be rendered,
    // so there is no verdict to report — and "no verdict" is not "clean".
    let (_tmp, root) = new_project("nameless", &[]);
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();

    let output = run(&root, &["--check"]);
    assert_eq!(output.status.code(), Some(2), "{}", report(&output));
    let combined = format!(
        "{}{}",
        stdout_of(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("package"), "{combined}");

    // ...and the manifest it could not use is still intact afterwards.
    assert!(root.join(".autumn/scaffold.toml").is_file());
}

#[test]
fn new_announces_the_scaffold_manifest_and_says_to_commit_it() {
    // `autumn new` lists every file it creates, and this is one of them. It
    // also only has value once it is committed — a manifest that never reaches
    // the next checkout is a manifest that never becomes a baseline — so the
    // summary is where a developer finds that out.
    let tmp = TempDir::new().expect("tempdir");
    let output = Command::new(autumn_bin())
        .arg("new")
        .arg("announced")
        .current_dir(tmp.path())
        .output()
        .expect("failed to run autumn new");
    assert!(output.status.success(), "{}", report(&output));

    let out = stdout_of(&output);
    assert!(out.contains(".autumn/scaffold.toml"), "{out}");
    assert!(out.contains("commit"), "{out}");
}

#[test]
fn accepting_a_conflict_lets_the_ci_gate_go_green_again() {
    // Without this, a team whose Dockerfile is deliberately theirs can never
    // make `--check` pass — and a permanently red gate is a deleted gate.
    let (_tmp, root) = new_project("accepted", &[]);
    let mine = "FROM scratch\n# ours, on purpose\n";
    fs::write(root.join("Dockerfile"), mine).unwrap();
    assert_eq!(run(&root, &["--check"]).status.code(), Some(3));

    let accept = run(&root, &["--accept", "Dockerfile"]);
    assert!(accept.status.success(), "{}", report(&accept));
    assert!(
        stdout_of(&accept).contains("Dockerfile"),
        "{}",
        report(&accept)
    );

    let check = run(&root, &["--check"]);
    assert!(check.status.success(), "{}", report(&check));
    assert!(stdout_of(&check).contains("pinned") || !stdout_of(&check).contains("conflict"));

    // ...and it is still never written.
    let applied = run(&root, &["--apply"]);
    assert!(applied.status.success(), "{}", report(&applied));
    assert_eq!(fs::read_to_string(root.join("Dockerfile")).unwrap(), mine);
    assert!(
        fs::read_to_string(root.join(MANIFEST))
            .unwrap()
            .contains("pinned"),
        "the pin must be recorded where a later checkout can read it"
    );
}

#[test]
fn accept_refuses_a_path_the_scaffold_does_not_own() {
    let (_tmp, root) = new_project("refused", &[]);
    let output = run(&root, &["--accept", "src/main.rs"]);
    assert_eq!(output.status.code(), Some(2), "{}", report(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("src/main.rs"),
        "{}",
        report(&output)
    );
}

#[test]
fn a_workspace_member_is_not_seeded_with_files_the_workspace_root_owns() {
    // A crate-local `clippy.toml` SHADOWS the workspace's rather than adding to
    // it, silently dropping its lints and MSRV pin; `.github/` only runs from
    // the repository root. Seeding those into a member is a regression that
    // looks like an upgrade.
    let (tmp, root) = new_project("member", &[]);
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    // `autumn new` writes a bare `[workspace]` table so a generated project is
    // its own root wherever it is dropped. Adopting one INTO a workspace means
    // deleting that table — which is what makes this crate a member at all.
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        manifest.replace("[workspace]\n", ""),
    )
    .unwrap();
    for path in ["clippy.toml", "rustfmt.toml", "rust-toolchain.toml"] {
        fs::remove_file(root.join(path)).unwrap();
    }
    fs::remove_dir_all(root.join(".github")).unwrap();
    fs::remove_dir_all(root.join(".autumn")).unwrap();

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));
    for path in [
        "clippy.toml",
        "rustfmt.toml",
        "rust-toolchain.toml",
        ".github/workflows/ci.yml",
    ] {
        assert!(
            !root.join(path).exists(),
            "{path} was seeded into a workspace member:\n{}",
            stdout_of(&output)
        );
    }
    assert!(
        stdout_of(&output).contains("workspace"),
        "{}",
        stdout_of(&output)
    );
    // ...and the per-crate files are still reconciled.
    assert!(root.join("Dockerfile").is_file());
}

/// Issue #2495: `posture-gate.yml` and `ci.yml` no longer pin the CLI they
/// install to this app's own `autumn-web` version — but `autumn upgrade
/// --apply` renders them through its own, independently constructed
/// `TemplateVars` (`upgrade/scaffold.rs::current_files`), a different call
/// site than `autumn new`'s (`new.rs::generate_inner`). Prove the fix reaches
/// the upgrade path too, not just the one `autumn new` exercises.
#[test]
fn apply_writes_posture_gate_and_ci_without_pinning_to_app_version() {
    let (_tmp, root) = new_project("upgrade-latest-cli", &[]);
    age_to(
        &root,
        "0.5.0",
        &[
            ".github/workflows/posture-gate.yml",
            ".github/workflows/ci.yml",
        ],
    );

    let output = run(&root, &["--apply"]);
    assert!(output.status.success(), "{}", report(&output));

    for path in [
        ".github/workflows/posture-gate.yml",
        ".github/workflows/ci.yml",
    ] {
        let content = fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("{path} must be written by --apply: {e}"));
        assert!(
            !content.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "{path} must not pin the installed CLI to this app's autumn \
             version: {content}"
        );
        assert!(
            !content.contains("-s -- --version"),
            "{path}'s install.sh invocation must not pass --version, so \
             install.sh's own default (latest) resolves the release to \
             install: {content}"
        );
    }
}

#[test]
fn check_and_list_migrations_together_are_a_usage_error() {
    // `--list-migrations` exits before anything is checked, so accepting the
    // pair would give CI a gate that silently gates nothing.
    let (_tmp, root) = new_project("bothflags", &[]);
    let output = run(&root, &["--check", "--list-migrations"]);
    assert_eq!(output.status.code(), Some(2), "{}", report(&output));
}

#[test]
fn the_check_json_report_has_the_same_shape_as_a_normal_run() {
    // One `jq '.scaffold.drift'` has to work against both, or a CI author
    // following the documented example reads null and passes.
    let (_tmp, root) = new_project("shape", &[]);
    age_to(&root, "0.5.0", &["rustfmt.toml"]);

    let checked: serde_json::Value =
        serde_json::from_str(&stdout_of(&run(&root, &["--check", "--json"]))).unwrap();
    let normal: serde_json::Value =
        serde_json::from_str(&stdout_of(&run(&root, &["--json"]))).unwrap();
    assert_eq!(checked["scaffold"]["drift"], true, "{checked}");
    assert_eq!(normal["scaffold"]["drift"], true, "{normal}");
    // A preview plans writes but performs none, and the two counts say so.
    assert_eq!(checked["scaffold"]["writable"], 1, "{checked}");
    assert_eq!(checked["scaffold"]["written"], 0, "{checked}");
}

#[test]
fn a_removed_only_report_does_not_talk_about_conflicts_that_do_not_exist() {
    let (_tmp, root) = new_project("removedonly", &[]);
    fs::remove_file(root.join(".env.example")).unwrap();

    let out = stdout_of(&run(&root, &["--check"]));
    assert!(out.contains("removed"), "{out}");
    assert!(!out.contains("conflict(s) need review"), "{out}");
    // ...and `--check` never points at diffs it deliberately suppressed.
    assert!(!out.contains("diffs above"), "{out}");
}

#[test]
fn accept_honours_json_mode() {
    // `--json` is documented as the machine-readable mode; a caller that
    // combines it with `--accept` gets prose on stdout and a parse error.
    let (_tmp, root) = new_project("acceptjson", &[]);
    fs::write(root.join("Dockerfile"), "FROM scratch\n").unwrap();

    let output = run(&root, &["--accept", "Dockerfile", "--json"]);
    assert!(output.status.success(), "{}", report(&output));
    let value: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("stdout must be JSON");
    assert_eq!(value["accepted"][0], "Dockerfile", "{value}");
}

#[cfg(unix)]
#[test]
fn an_apply_that_cannot_record_its_baseline_exits_nonzero() {
    // Exit 0 here would tell a CI script the upgrade succeeded, when in fact
    // the very next `--check` is guaranteed to exit 3 on the files this run
    // just wrote correctly.
    let (tmp, root) = new_project("baseline", &[]);
    let outside = tmp.path().join("outside.toml");
    fs::write(&outside, "not mine\n").unwrap();
    fs::remove_file(root.join(MANIFEST)).unwrap();
    std::os::unix::fs::symlink(&outside, root.join(MANIFEST)).unwrap();
    fs::remove_file(root.join("rustfmt.toml")).unwrap();

    let output = run(&root, &["--apply"]);
    assert_eq!(output.status.code(), Some(1), "{}", report(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline"), "{}", report(&output));

    // The file it did write is still correct, and the link target is untouched.
    assert!(root.join("rustfmt.toml").is_file());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "not mine\n");
}

#[test]
fn check_does_not_pass_a_project_scaffolded_by_a_newer_release() {
    // Refusing to look is not an all-clear — the same rule as an unreadable
    // package name. A gate that goes green because the CLI is too old to
    // reconcile the project is worse than no gate.
    let (_tmp, root) = new_project("fromfuture", &[]);
    let manifest = fs::read_to_string(root.join(MANIFEST)).unwrap();
    fs::write(
        root.join(MANIFEST),
        manifest
            .lines()
            .map(|line| {
                if line.starts_with("version = ") {
                    "version = \"99.0.0\"".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

    let output = run(&root, &["--check"]);
    assert_eq!(output.status.code(), Some(2), "{}", report(&output));
    let combined = format!(
        "{}{}",
        stdout_of(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("99.0.0"), "{combined}");
}
