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
