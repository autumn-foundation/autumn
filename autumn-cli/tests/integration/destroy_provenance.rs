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

fn digest(contents: &str) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(contents.replace("\r\n", "\n").as_bytes()))
}

/// Rewrite the recorded digest for `OWNED` to match what is now on disk.
fn rerecord(project: &Path, contents: &str) {
    let path = project.join(MANIFEST);
    let text = fs::read_to_string(&path).expect("manifest");
    let mut doc: toml::Table = text.parse().expect("manifest is TOML");
    let files = doc
        .get_mut("files")
        .and_then(toml::Value::as_table_mut)
        .expect("[files] table");
    files.insert(OWNED.to_owned(), toml::Value::String(digest(contents)));
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
    assert_eq!(
        digest(&fs::read_to_string(project.join(OWNED)).unwrap()),
        {
            let doc: toml::Table = manifest.parse().unwrap();
            doc["files"][OWNED].as_str().unwrap().to_owned()
        },
        "the recorded digest is the digest of what was written"
    );
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
    assert!(!gitignore.contains(".autumn"), "{gitignore}");
}
