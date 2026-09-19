//! `autumn generate scaffold <Child> … --belongs-to <Parent>` wires a child
//! resource to its parent: a nested read route, a nested create route whose FK
//! comes from the path, a children list + inline create form on the parent's
//! own show view, back-links in both directions, and a generated write-path
//! test that pins cross-parent isolation (issue #1323).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn")
}

fn run_autumn_ok(dir: &Path, args: &[&str]) {
    let output = run_autumn(dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `autumn new` + `generate scaffold Post title:String` + `generate scaffold
/// Comment body:Text post:references --belongs-to Post`.
fn nested_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    run_autumn_ok(&project, &["generate", "scaffold", "Post", "title:String"]);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--belongs-to",
            "Post",
        ],
    );
    (tmp, project)
}

#[test]
fn nested_routes_and_parent_section_are_generated_end_to_end() {
    let (_tmp, project) = nested_project("nested-app");

    let child = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();
    assert!(
        child.contains("#[get(\"/posts/{post_id}/comments\", name = \"nested_index\")]"),
        "{child}"
    );
    assert!(
        child.contains("#[post(\"/posts/{post_id}/comments\", name = \"nested_create\")]"),
        "{child}"
    );
    assert!(
        child.contains(".filter(comments::post_id.eq(post_id))"),
        "{child}"
    );
    assert!(
        child.contains("crate::routes::posts::paths::show(row.post_id)"),
        "the child show view must link back to its parent:\n{child}"
    );

    let parent = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        parent.contains("crate::routes::comments::children_section("),
        "the parent show must render its children:\n{parent}"
    );
    assert!(parent.contains("(__autumn_children_comments)"), "{parent}");

    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("routes::comments::nested_index"), "{main}");
    assert!(main.contains("routes::comments::nested_create"), "{main}");

    let test = fs::read_to_string(project.join("tests/comment.rs")).unwrap();
    assert!(
        test.contains("async fn comments_nested_under_parent()"),
        "{test}"
    );
}

#[test]
fn rerunning_the_nested_generator_does_not_double_inject_the_parent() {
    let (_tmp, project) = nested_project("nested-idem-app");
    let before = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--belongs-to",
            "Post",
            "--force",
        ],
    );
    let after = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert_eq!(before, after, "parent injection must be idempotent");
}

#[test]
fn destroy_reverses_the_nested_edits_on_the_parent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "nested-destroy-app"]);
    let project = tmp.path().join("nested-destroy-app");
    run_autumn_ok(&project, &["generate", "scaffold", "Post", "title:String"]);
    let parent_before = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--belongs-to",
            "Post",
        ],
    );
    run_autumn_ok(
        &project,
        &[
            "destroy",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--belongs-to",
            "Post",
        ],
    );

    assert_eq!(
        fs::read_to_string(project.join("src/routes/posts.rs")).unwrap(),
        parent_before,
        "destroy must restore the parent routes file"
    );
    // The nested route entries are unmounted. Asserted by absence rather than
    // by byte-equality with a pre-destroy snapshot: a plain flat `generate` +
    // `destroy` round trip already reformats `main.rs`'s `routes![…]` list onto
    // one entry per line, so a whole-file comparison would be pinning that
    // pre-existing cosmetic churn rather than anything about nesting.
    let main_after = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !main_after.contains("routes::comments::nested_index"),
        "destroy must unmount the nested read route:\n{main_after}"
    );
    assert!(
        !main_after.contains("routes::comments::nested_create"),
        "destroy must unmount the nested create route:\n{main_after}"
    );
    assert!(
        main_after.contains("routes::posts::index"),
        "the parent's own routes must survive destroying the child:\n{main_after}"
    );
    assert!(!project.join("src/routes/comments.rs").exists());
}

#[test]
fn belongs_to_without_a_scaffolded_parent_fails_with_an_actionable_message() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "nested-noparent-app"]);
    let project = tmp.path().join("nested-noparent-app");
    let output = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--belongs-to",
            "Post",
        ],
    );
    assert!(!output.status.success(), "expected a failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("src/routes/posts.rs"),
        "message must name the missing parent module; got: {stderr}"
    );
}
