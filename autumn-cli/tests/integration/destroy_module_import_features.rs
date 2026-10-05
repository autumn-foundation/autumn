//! `autumn destroy scaffold` keeps a feature that a module import uses (issue #2186).
//!
//! `destroy` keeps an `autumn-web` feature when the remaining source contains
//! its marker. A module import (`use autumn_web::storage;`) does not have a
//! trailing `::`. A marker that has `::` does not find it, and `destroy`
//! removes a feature that the code needs.
//!
//! Each hand-written file is in `src/`, not in the `owner_dir` of the feature
//! (`src/models` for `storage`, `src/routes` for `markdown`). In `owner_dir`,
//! the sibling check keeps the feature, and the test cannot fail.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_autumn_ok(dir: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The scaffold that enables each feature.
fn fields(feature: &str) -> &'static [&'static str] {
    match feature {
        "storage" => &["Document", "file:Attachment"],
        "markdown" => &["Post", "title:String", "body:richtext"],
        other => panic!("no scaffold for feature {other}"),
    }
}

/// Make a project, scaffold the resource for `feature`, write `handwritten`
/// (if given) to `src/<file>`, destroy the scaffold, and return `Cargo.toml`.
fn destroy_with(feature: &str, name: &str, handwritten: Option<(&str, &str)>) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project: PathBuf = tmp.path().join(name);

    let mut generate = vec!["generate", "scaffold"];
    generate.extend_from_slice(fields(feature));
    run_autumn_ok(&project, &generate);
    let before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        before.contains(&format!("\"{feature}\"")),
        "premise: the scaffold enables `{feature}`:\n{before}"
    );

    if let Some((file, source)) = handwritten {
        fs::write(project.join("src").join(file), source).unwrap();
    }

    let mut destroy = vec!["destroy", "scaffold"];
    destroy.extend_from_slice(fields(feature));
    destroy.push("--force");
    run_autumn_ok(&project, &destroy);
    fs::read_to_string(project.join("Cargo.toml")).unwrap()
}

fn assert_kept(feature: &str, cargo: &str) {
    assert!(
        cargo.contains(&format!("\"{feature}\"")),
        "hand-written code still uses `{feature}`; destroy must keep it:\n{cargo}"
    );
}

// ── storage ──────────────────────────────────────────────────────────────────

#[test]
fn destroy_keeps_storage_for_a_module_import() {
    let cargo = destroy_with(
        "storage",
        "storage-mod",
        Some((
            "thumbs.rs",
            "use autumn_web::storage;\n\
             pub fn thumb(b: &storage::Blob) -> &storage::Blob { b }\n",
        )),
    );
    assert_kept("storage", &cargo);
}

#[test]
fn destroy_keeps_storage_for_a_renamed_module_import() {
    let cargo = destroy_with(
        "storage",
        "storage-alias",
        Some((
            "thumbs.rs",
            "use autumn_web::storage as blobs;\n\
             pub fn thumb(b: &blobs::Blob) -> &blobs::Blob { b }\n",
        )),
    );
    assert_kept("storage", &cargo);
}

#[test]
fn destroy_removes_storage_when_nothing_uses_it() {
    let cargo = destroy_with("storage", "storage-unused", None);
    assert!(
        !cargo.contains("\"storage\""),
        "no code uses `storage`; destroy must remove it:\n{cargo}"
    );
}

// ── markdown ─────────────────────────────────────────────────────────────────

#[test]
fn destroy_keeps_markdown_for_a_module_import() {
    let cargo = destroy_with(
        "markdown",
        "markdown-mod",
        Some((
            "blog.rs",
            "use autumn_web::markdown;\n\
             pub fn body(src: &str) -> String { markdown::render_user_content(src).into_string() }\n",
        )),
    );
    assert_kept("markdown", &cargo);
}

#[test]
fn destroy_keeps_markdown_for_a_renamed_module_import() {
    let cargo = destroy_with(
        "markdown",
        "markdown-alias",
        Some((
            "blog.rs",
            "use autumn_web::markdown as md;\n\
             pub fn body(src: &str) -> String { md::render_user_content(src).into_string() }\n",
        )),
    );
    assert_kept("markdown", &cargo);
}

#[test]
fn destroy_removes_markdown_when_nothing_uses_it() {
    let cargo = destroy_with("markdown", "markdown-unused", None);
    assert!(
        !cargo.contains("\"markdown\""),
        "no code uses `markdown`; destroy must remove it:\n{cargo}"
    );
}
