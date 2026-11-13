//! `autumn destroy` after the generator template changed (issue #1835).
//!
//! `destroy` recomputes the plan a matching `generate` would build now, so a
//! newer CLI whose template moved on used to report `Diverged` for every
//! untouched file that generator wrote. `generate` now records a digest of each
//! file it owns in `.autumn/generated.toml`, and `destroy` accepts a file
//! matching either that digest or the current render.
//!
//! A template change cannot be made from a test — one binary ships one
//! template. The same asymmetry is reproduced from the other side: put content
//! on disk that the current render does NOT produce, and record it in the
//! manifest as what `generate` wrote. That is byte-for-byte the state an
//! older-CLI project is in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

const MANIFEST: &str = ".autumn/generated.toml";
const OWNED: &str = "src/models/post.rs";

fn run_autumn(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn")
}

fn run_autumn_ok(dir: &Path, args: &[&str]) -> std::process::Output {
    let output = run_autumn(dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

/// `autumn new` + `generate model Post title:String`.
fn project_with_model(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    run_autumn_ok(&project, &["generate", "model", "Post", "title:String"]);
    (tmp, project)
}

/// Mirrors `generate::provenance::text_digest`. `autumn-cli` is a bin-only
/// crate, so an integration test cannot import it; a drift here fails these
/// tests rather than passing them.
fn digest(contents: &str) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(contents.replace("\r\n", "\n").as_bytes()))
}

/// Rewrite the recorded digest for `OWNED` to match what is now on disk.
fn rerecord(project: &Path, contents: &str) {
    let path = project.join(MANIFEST);
    let text = fs::read_to_string(&path).expect("manifest");
    let mut doc: toml::Table = text.parse().expect("manifest is TOML");
    let entry = doc
        .get_mut("files")
        .and_then(toml::Value::as_table_mut)
        .and_then(|files| files.get_mut(OWNED))
        .and_then(toml::Value::as_table_mut)
        .expect("an entry for the owned file");
    entry.insert("digest".to_owned(), toml::Value::String(digest(contents)));
    fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
}

#[test]
fn generate_records_a_digest_for_every_file_it_owns() {
    let (_tmp, project) = project_with_model("prov-record");

    let manifest = fs::read_to_string(project.join(MANIFEST)).expect("manifest written");
    assert!(manifest.contains(OWNED), "{manifest}");
    assert!(
        !manifest.contains("src/main.rs"),
        "a shared Modify target is never owned: {manifest}"
    );
    let doc: toml::Table = manifest.parse().unwrap();
    assert_eq!(
        digest(&fs::read_to_string(project.join(OWNED)).unwrap()),
        doc["files"][OWNED]["digest"].as_str().unwrap(),
        "the recorded digest is the digest of what was written"
    );
    // The recorded identity is the arguments plus a fingerprint of the
    // generator config they resolve from, so only the prefix is fixed here.
    assert!(
        doc["files"][OWNED]["invocation"]
            .as_str()
            .unwrap()
            .starts_with("model\u{1f}Post\u{1f}title:String\u{1e}"),
        "the command that wrote it is recorded with it: {:?}",
        doc["files"][OWNED]["invocation"]
    );
}

#[test]
fn destroy_with_different_arguments_is_refused() {
    // `destroy` mirrors `generate` argument-for-argument. Omitting the fields
    // renders a different model, and the recorded digest must not stand in for
    // arguments the user did not repeat — the `schema.rs` and `Cargo.toml`
    // reverts are derived from those fields and would silently no-op.
    let (_tmp, project) = project_with_model("prov-wrong-args");
    let older_output = "// what an older CLI's template rendered\n";
    fs::write(project.join(OWNED), older_output).unwrap();
    rerecord(&project, older_output);

    let output = run_autumn(&project, &["destroy", "model", "Post"]);

    assert!(
        !output.status.success(),
        "different arguments, no tolerance"
    );
    assert!(project.join(OWNED).exists());
}

#[test]
fn destroy_removes_an_untouched_file_whose_template_changed() {
    let (_tmp, project) = project_with_model("prov-older-cli");
    let older_output = "// what an older CLI's template rendered\n";
    fs::write(project.join(OWNED), older_output).unwrap();
    rerecord(&project, older_output);

    run_autumn_ok(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(!project.join(OWNED).exists(), "untouched file must go");
    assert!(
        !fs::read_to_string(project.join(MANIFEST))
            .unwrap_or_default()
            .contains(OWNED),
        "the manifest entry must go with it"
    );
}

#[test]
fn destroy_still_refuses_a_hand_edited_file() {
    let (_tmp, project) = project_with_model("prov-edited");
    fs::write(project.join(OWNED), "// my own code\n").unwrap();

    let output = run_autumn(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(!output.status.success(), "an edited file must be refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to destroy"), "{stderr}");
    assert!(
        stderr.contains(MANIFEST),
        "the message names the baseline: {stderr}"
    );
    assert!(project.join(OWNED).exists(), "a real edit must survive");
}

#[test]
fn force_still_deletes_a_hand_edited_file() {
    let (_tmp, project) = project_with_model("prov-forced");
    fs::write(project.join(OWNED), "// my own code\n").unwrap();

    run_autumn_ok(
        &project,
        &["destroy", "model", "Post", "title:String", "--force"],
    );

    assert!(!project.join(OWNED).exists());
}

#[test]
fn a_project_without_a_manifest_behaves_as_before() {
    let (_tmp, project) = project_with_model("prov-legacy");
    let older_output = "// what an older CLI's template rendered\n";
    fs::write(project.join(OWNED), older_output).unwrap();
    fs::remove_file(project.join(MANIFEST)).unwrap();

    let output = run_autumn(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(!output.status.success(), "no baseline, no tolerance");
    assert!(project.join(OWNED).exists());
}

#[test]
fn the_manifest_is_committed_not_ignored() {
    let (_tmp, project) = project_with_model("prov-committed");
    let gitignore = fs::read_to_string(project.join(".gitignore")).unwrap();
    assert!(
        !gitignore.lines().any(|line| {
            let pattern = line.trim().trim_end_matches('/');
            !pattern.is_empty()
                && !pattern.starts_with('#')
                && MANIFEST.starts_with(pattern.trim_start_matches('/'))
        }),
        "the baseline must be trackable: {gitignore}"
    );
}

// ---- issue #2557: files the templates no longer name ----------------------
//
// A newer template can rename or drop a file. The recomputed plan then names
// no action for the old path. The same trick as above reproduces it: put a
// file on disk that the current render never writes, and record it under the
// model's own invocation, as an older CLI would have.

const ORPHAN: &str = "src/models/post_v1.rs";

/// Write `contents` at [`ORPHAN`] and record it beside [`OWNED`], with the
/// same invocation and the given `scope` (`None` writes no scope key, as
/// 0.8.0 did).
fn plant_orphan(project: &Path, contents: &str, scope: Option<&str>) {
    fs::write(project.join(ORPHAN), contents).unwrap();
    let path = project.join(MANIFEST);
    let mut doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
    let files = doc
        .get_mut("files")
        .and_then(toml::Value::as_table_mut)
        .expect("files table");
    let mut entry = files[OWNED].as_table().expect("owned entry").clone();
    entry.insert("digest".to_owned(), toml::Value::String(digest(contents)));
    match scope {
        Some(scope) => entry.insert("scope".to_owned(), toml::Value::String(scope.to_owned())),
        None => entry.remove("scope"),
    };
    files.insert(ORPHAN.to_owned(), toml::Value::Table(entry));
    fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
}

#[test]
fn generate_records_the_owned_scope() {
    let (_tmp, project) = project_with_model("prov-scope");
    let doc: toml::Table = fs::read_to_string(project.join(MANIFEST))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(doc["files"][OWNED]["scope"].as_str(), Some("owned"));
}

#[test]
fn destroy_removes_an_untouched_file_the_templates_renamed() {
    let (_tmp, project) = project_with_model("prov-orphan");
    plant_orphan(&project, "// an older template's file\n", Some("owned"));

    let output = run_autumn_ok(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(!project.join(ORPHAN).exists(), "the stranded file must go");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(ORPHAN) && stdout.contains("no longer generated"),
        "{stdout}"
    );
    assert!(
        !fs::read_to_string(project.join(MANIFEST))
            .unwrap_or_default()
            .contains(ORPHAN),
        "its entry must go with it"
    );
}

#[test]
fn destroy_dry_run_lists_the_stranded_file() {
    let (_tmp, project) = project_with_model("prov-orphan-dry");
    plant_orphan(&project, "// an older template's file\n", Some("owned"));

    let output = run_autumn_ok(
        &project,
        &["destroy", "model", "Post", "title:String", "--dry-run"],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("Would remove {ORPHAN} (no longer generated)")),
        "{stdout}"
    );
    assert!(project.join(ORPHAN).exists());
    assert!(
        fs::read_to_string(project.join(MANIFEST))
            .unwrap()
            .contains(ORPHAN),
        "a dry run keeps the entry"
    );
}

#[test]
fn destroy_keeps_an_edited_stranded_file_and_names_it() {
    let (_tmp, project) = project_with_model("prov-orphan-edited");
    plant_orphan(&project, "// an older template's file\n", Some("owned"));
    fs::write(project.join(ORPHAN), "// my own code\n").unwrap();

    let output = run_autumn_ok(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(project.join(ORPHAN).exists(), "an edit is never swept");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(ORPHAN), "{stderr}");
}

#[test]
fn destroy_names_a_stranded_file_from_an_older_manifest() {
    // 0.8.0 wrote no scope, so owned and shared look the same: keep and warn.
    let (_tmp, project) = project_with_model("prov-orphan-legacy");
    plant_orphan(&project, "// an older template's file\n", None);

    let output = run_autumn_ok(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(project.join(ORPHAN).exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(ORPHAN), "{stderr}");
}

#[test]
fn destroy_never_sweeps_another_commands_files() {
    let (_tmp, project) = project_with_model("prov-orphan-sibling");
    run_autumn_ok(&project, &["generate", "model", "Comment", "body:String"]);
    plant_orphan(&project, "// an older template's file\n", Some("owned"));

    run_autumn_ok(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(!project.join(ORPHAN).exists());
    assert!(project.join("src/models/comment.rs").exists());
    assert!(
        fs::read_to_string(project.join(MANIFEST))
            .unwrap()
            .contains("src/models/comment.rs")
    );
}

#[test]
fn destroy_keeps_a_stranded_shared_file_and_names_it() {
    let (_tmp, project) = project_with_model("prov-orphan-shared");
    plant_orphan(&project, "// an older template's file\n", Some("shared"));

    let output = run_autumn_ok(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(
        project.join(ORPHAN).exists(),
        "a shared file is never swept"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(ORPHAN), "{stderr}");
}

#[test]
fn destroy_never_sweeps_a_forged_entry_for_a_project_file() {
    // The manifest is project content. A forged `owned` entry with the right
    // digest must still not reach a file outside the generator's directories.
    let (_tmp, project) = project_with_model("prov-orphan-forged");
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let path = project.join(MANIFEST);
    let mut doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
    let files = doc
        .get_mut("files")
        .and_then(toml::Value::as_table_mut)
        .expect("files table");
    let mut entry = files[OWNED].as_table().expect("owned entry").clone();
    entry.insert("digest".to_owned(), toml::Value::String(digest(&cargo)));
    files.insert("Cargo.toml".to_owned(), toml::Value::Table(entry));
    fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();

    run_autumn_ok(&project, &["destroy", "model", "Post", "title:String"]);

    assert!(project.join("Cargo.toml").exists());
}

// Issue #2560: a config input the arguments cannot see must not turn a destroy
// into a partial one.

const SCAFFOLD: [&str; 4] = ["scaffold", "Post", "title:String", "--i18n"];

fn with(verb: &str, extra: &[&str]) -> Vec<String> {
    std::iter::once(verb)
        .chain(SCAFFOLD)
        .chain(extra.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn run_verb(project: &Path, verb: &str, extra: &[&str]) -> std::process::Output {
    let args = with(verb, extra);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_autumn(project, &refs)
}

/// `autumn new` + `generate scaffold Post --i18n`, with the bundle in `dir`.
fn i18n_scaffold_in(name: &str, dir: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    if dir != "i18n" {
        let toml = project.join("autumn.toml");
        let body = fs::read_to_string(&toml).unwrap();
        fs::write(toml, format!("{body}\n[i18n]\ndir = \"{dir}\"\n")).unwrap();
    }
    let out = run_verb(&project, "generate", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        fs::read_to_string(project.join(dir).join("en.ftl"))
            .unwrap()
            .contains("Post"),
        "the scaffold wrote its keys to {dir}/en.ftl"
    );
    (tmp, project)
}

fn i18n_scaffold(name: &str) -> (tempfile::TempDir, PathBuf) {
    i18n_scaffold_in(name, "i18n")
}

/// Points `[i18n] dir` back at the default, the way a config edit would.
fn drop_i18n_dir_setting(project: &Path) {
    let path = project.join("autumn.toml");
    let body = fs::read_to_string(&path).unwrap();
    let kept: Vec<&str> = body.lines().filter(|l| !l.starts_with("dir = ")).collect();
    fs::write(path, kept.join("\n") + "\n").unwrap();
}

#[test]
fn destroy_refuses_when_the_i18n_dir_moved_since_generate() {
    let (_tmp, project) = i18n_scaffold_in("prov-i18n-dir", "translations");
    drop_i18n_dir_setting(&project);

    let output = run_verb(&project, "destroy", &[]);

    assert!(!output.status.success(), "a partial destroy must not run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to destroy"), "{stderr}");
    assert!(
        stderr.contains("translations/en.ftl"),
        "names the stranded edit: {stderr}"
    );
    assert!(
        project.join("src/models/post.rs").exists(),
        "nothing removed"
    );
    assert!(
        fs::read_to_string(project.join("translations/en.ftl"))
            .unwrap()
            .contains("Post"),
        "the translations survive"
    );
}

#[test]
fn force_destroys_despite_a_moved_i18n_dir() {
    let (_tmp, project) = i18n_scaffold_in("prov-i18n-force", "translations");
    drop_i18n_dir_setting(&project);

    let out = run_verb(&project, "destroy", &["--force"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!project.join("src/models/post.rs").exists());
}

#[test]
fn a_dry_run_names_the_moved_i18n_dir_and_removes_nothing() {
    let (_tmp, project) = i18n_scaffold_in("prov-i18n-dry", "translations");
    drop_i18n_dir_setting(&project);

    let out = run_verb(&project, "destroy", &["--dry-run"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("translations/en.ftl"));
    assert!(project.join("src/models/post.rs").exists());
}

#[test]
fn destroy_still_works_when_the_i18n_config_is_unchanged() {
    let (_tmp, project) = i18n_scaffold("prov-i18n-same");

    let output = run_verb(&project, "destroy", &[]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!project.join("src/models/post.rs").exists());
    assert!(
        !fs::read_to_string(project.join("i18n/en.ftl"))
            .unwrap()
            .contains("Post")
    );
}

#[test]
fn destroy_tolerates_a_locale_bundle_added_after_generate() {
    // A translator copies the default bundle. The plan reverts more than
    // generate did, never less, so the destroy proceeds and cleans both.
    let (_tmp, project) = i18n_scaffold("prov-i18n-fr");
    fs::copy(project.join("i18n/en.ftl"), project.join("i18n/fr.ftl")).unwrap();

    let output = run_verb(&project, "destroy", &[]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !fs::read_to_string(project.join("i18n/fr.ftl"))
            .unwrap()
            .contains("Post")
    );
}

#[test]
fn destroy_proceeds_when_the_moved_bundle_was_moved_with_it() {
    // The old target is gone, so the revert it recorded has nothing to leave.
    let (_tmp, project) = i18n_scaffold_in("prov-i18n-moved", "translations");
    fs::rename(project.join("translations"), project.join("i18n")).unwrap();
    drop_i18n_dir_setting(&project);

    let output = run_verb(&project, "destroy", &[]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !fs::read_to_string(project.join("i18n/en.ftl"))
            .unwrap()
            .contains("Post")
    );
}

// A revert the generator skips because of file state is not a changed input.

fn plain_scaffold(name: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf, Vec<String>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec!["scaffold", "Post", "title:String"];
    args.extend_from_slice(extra);
    let mut generate = vec!["generate"];
    generate.extend_from_slice(&args);
    run_autumn_ok(&project, &generate);
    let destroy: Vec<String> = std::iter::once("destroy")
        .chain(args)
        .map(str::to_owned)
        .collect();
    (tmp, project, destroy)
}

fn run_owned(project: &Path, args: &[String]) -> std::process::Output {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_autumn(project, &refs)
}

#[test]
fn destroy_of_a_plain_scaffold_still_succeeds() {
    let (_tmp, project, destroy) = plain_scaffold("prov-plain", &[]);

    let out = run_owned(&project, &destroy);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!project.join("src/models/post.rs").exists());
}

#[test]
fn destroy_of_a_live_validation_scaffold_still_succeeds() {
    // The submit-token exemption is appended once. At destroy time it exists,
    // so the plan has no revert for it. That is a skip, not a changed input.
    let (_tmp, project, destroy) = plain_scaffold("prov-live-validation", &["--live-validation"]);

    let out = run_owned(&project, &destroy);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!project.join("src/models/post.rs").exists());
}

#[test]
fn a_refusal_lists_each_edit_on_its_own_line_and_names_force() {
    let (_tmp, project) = i18n_scaffold_in("prov-i18n-text", "translations");
    drop_i18n_dir_setting(&project);

    let out = run_verb(&project, "destroy", &[]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("pass --force"), "{stderr}");
    assert!(
        !stderr.contains("  below"),
        "no runs of spaces in prose: {stderr}"
    );
    assert!(
        stderr
            .lines()
            .any(|l| l == "  translations/en.ftl: I18nFtlKeys(Post, post)"),
        "{stderr}"
    );
}

#[test]
fn a_dry_run_says_a_real_run_would_refuse() {
    let (_tmp, project) = i18n_scaffold_in("prov-i18n-dry-text", "translations");
    drop_i18n_dir_setting(&project);

    let out = run_verb(&project, "destroy", &["--dry-run"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("A real run refuses without --force"),
        "{stdout}"
    );
    assert!(
        stdout.contains("translations/en.ftl: I18nFtlKeys(Post, post)"),
        "{stdout}"
    );
}

#[test]
fn a_forced_dry_run_does_not_report_a_refusal() {
    let (_tmp, project) = i18n_scaffold_in("prov-i18n-dry-force", "translations");
    drop_i18n_dir_setting(&project);

    let out = run_verb(&project, "destroy", &["--dry-run", "--force"]);

    assert!(!String::from_utf8_lossy(&out.stdout).contains("A real run refuses without --force"));
}

#[test]
fn a_finished_destroy_forgets_its_run() {
    let (_tmp, project) = i18n_scaffold("prov-i18n-forget");
    fs::write(
        project.join(MANIFEST),
        format!(
            "{}\n[files.\"keep.rs\"]\ndigest = \"1\"\ninvocation = \"other\"\n",
            fs::read_to_string(project.join(MANIFEST)).unwrap()
        ),
    )
    .unwrap();

    let out = run_verb(&project, "destroy", &[]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = fs::read_to_string(project.join(MANIFEST)).unwrap();
    assert!(!manifest.contains("runs"), "{manifest}");
    assert!(manifest.contains("keep.rs"), "{manifest}");
}
