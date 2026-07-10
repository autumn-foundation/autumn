//! `belongs_to` `references` fields render a populated `<select>` labeled by a
//! display column, and index/show views render the parent's display value
//! instead of the raw foreign-key id (issue #1146).

// DSL tokens like `"post:references{label:slug}"` are literal scaffold inputs,
// not format strings — the `{label:col}` is the modifier syntax under test.
#![allow(clippy::literal_string_with_formatting_args)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn_ok(dir: &Path, args: &[&str]) {
    let output = Command::new(autumn_bin())
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

/// `autumn new` + `generate model Post <post_cols>` + `generate scaffold
/// Comment body:Text <comment_ref>`, returning the generated `comments.rs`.
fn belongs_to_routes(
    name: &str,
    post_cols: &[&str],
    comment_ref: &str,
) -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut post_args = vec!["generate", "model", "Post"];
    post_args.extend_from_slice(post_cols);
    run_autumn_ok(&project, &post_args);
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Comment", "body:Text", comment_ref],
    );
    let routes = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();
    (tmp, routes)
}

#[test]
fn select_loader_uses_title_display_column_heuristic() {
    let (_tmp, routes) = belongs_to_routes("bt-title-app", &["title:String"], "post:references");
    assert!(
        routes.contains(".select((posts::id, posts::title))"),
        "the loader must select the id + the `title` display column:\n{routes}"
    );
    assert!(
        routes.contains(".map(|(id, label)| (id.to_string(), label))"),
        "options must be labeled by the display value, not the id:\n{routes}"
    );
}

#[test]
fn select_loader_prefers_name_over_other_string_columns() {
    // `name` beats a later `headline` string column per the heuristic.
    let (_tmp, routes) = belongs_to_routes(
        "bt-name-app",
        &["headline:String", "name:String"],
        "post:references",
    );
    assert!(
        routes.contains(".select((posts::id, posts::name))"),
        "the loader must prefer the `name` column:\n{routes}"
    );
}

#[test]
fn label_override_selects_the_named_column() {
    let (_tmp, routes) = belongs_to_routes(
        "bt-override-app",
        &["title:String", "slug:String"],
        "post:references{label:slug}",
    );
    assert!(
        routes.contains(".select((posts::id, posts::slug))"),
        "an explicit {{label:slug}} must select the `slug` column:\n{routes}"
    );
}

#[test]
fn falls_back_to_id_when_target_has_no_string_column() {
    // A `Post` with only numeric columns has no display column, so the loader
    // keeps the id as both value and label.
    let (_tmp, routes) = belongs_to_routes("bt-noid-app", &["views:i64"], "post:references");
    assert!(
        routes.contains(".select(posts::id)"),
        "with no string column the loader must fall back to id-only:\n{routes}"
    );
    assert!(
        routes.contains("(id.to_string(), id.to_string())"),
        "the id-fallback loader labels each option by its id:\n{routes}"
    );
}

#[test]
fn show_view_renders_parent_label_not_raw_fk() {
    let (_tmp, routes) = belongs_to_routes("bt-show-app", &["title:String"], "post:references");
    // A per-view load of the parent's title, rendered in the property list.
    assert!(
        routes.contains("let post_id_label: String")
            && routes.contains("posts::table.find(fk).select(posts::title)"),
        "show must load the parent's display label:\n{routes}"
    );
    assert!(
        routes.contains("(post_id_label)"),
        "show must render the loaded label:\n{routes}"
    );
    // The show property list must NOT render the raw FK integer.
    assert!(
        !routes.contains("maud::html! { (row.post_id) }"),
        "show must not render the raw post_id integer:\n{routes}"
    );
}

#[test]
fn index_view_renders_parent_label_via_loaded_map() {
    let (_tmp, routes) = belongs_to_routes("bt-index-app", &["title:String"], "post:references");
    assert!(
        routes.contains("let post_id_labels: std::collections::HashMap<String, String>"),
        "index must load a parent-label map:\n{routes}"
    );
    assert!(
        routes.contains("post_id_labels.get(&row.post_id.to_string())"),
        "the index column must look the label up from the map:\n{routes}"
    );
}

#[test]
fn nullable_reference_renders_dash_and_blank_option() {
    let (_tmp, routes) =
        belongs_to_routes("bt-nullable-app", &["title:String"], "post:references?");
    // The nullable select gets a blank first option.
    assert!(
        routes.contains("\"— Unset —\".to_string()"),
        "a nullable reference must offer a blank first option:\n{routes}"
    );
    // The nullable FK renders a dash for the None case in show and index.
    assert!(
        routes.contains("None => \"—\".to_string()"),
        "show must render a dash for an unset nullable reference:\n{routes}"
    );
    assert!(
        routes.contains("match &row.post_id { Some(v) =>"),
        "index must handle the nullable FK's None case:\n{routes}"
    );
}

/// Slow end-to-end check: the `belongs_to` scaffold (display-column select
/// loader, index label map, show label load) type-checks against this
/// workspace's `autumn-web`.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn belongs_to_scaffold_cargo_checks() {
    use std::fmt::Write as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bt-check-app"]);
    let project: PathBuf = tmp.path().join("bt-check-app");
    run_autumn_ok(&project, &["generate", "model", "Post", "title:String"]);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
        ],
    );

    let cargo_toml_path = project.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/")
    );
    fs::write(&cargo_toml_path, content).unwrap();

    let check = Command::new("cargo")
        .args(["check", "--bins"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the belongs_to scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
