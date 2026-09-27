//! End-to-end scaffolding of a `richtext` column (issue #1255).
//!
//! `autumn generate scaffold Post title:String body:richtext` must produce an
//! app that is **safe by default**: the form edits Markdown source through the
//! shipped editor widget, the show view renders it through the sanitizing
//! `markdown::render_user_content`, and the project's `autumn-web` dependency
//! carries the `markdown` feature so all of that resolves.
//!
//! The string assertions run everywhere; the `#[ignore]`d compile check proves
//! the emitted code actually builds against this workspace's `autumn-web`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run the production `autumn` binary in `dir`, asserting success.
fn run_autumn(dir: &Path, args: &[&str]) {
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
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

/// `autumn new` + `autumn generate scaffold … body:richtext` in a fresh
/// tempdir, returning the tempdir guard and the project root.
fn rich_text_project(project_name: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn(tmp.path(), &["new", project_name]);
    let project = tmp.path().join(project_name);
    let mut args = vec![
        "generate",
        "scaffold",
        "Post",
        "title:String",
        "body:richtext",
    ];
    args.extend_from_slice(extra);
    run_autumn(&project, &args);
    (tmp, project)
}

#[test]
fn richtext_scaffold_emits_the_editor_preview_and_sanitized_render() {
    let (_tmp, project) = rich_text_project("richtext-app", &[]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // The form control is the Markdown editor, wired to the preview endpoint.
    assert!(
        routes.contains("rich_text_area_htmx_with_token_field"),
        "form must render the Markdown editor:\n{routes}"
    );
    assert!(
        routes.contains("paths::preview_body()"),
        "the editor must link to the typed preview path helper:\n{routes}"
    );

    // The preview endpoint exists and sanitizes.
    assert!(
        routes.contains("#[post(\"/posts/preview/body\")]"),
        "{routes}"
    );
    assert!(routes.contains("pub async fn preview_body("), "{routes}");

    // The show view renders through the sanitizer, never raw.
    assert!(
        routes.contains("autumn_web::markdown::render_user_content(&row.body)"),
        "show view must sanitize:\n{routes}"
    );
    assert!(
        !routes.contains("maud::PreEscaped(&row.body)"),
        "the raw source must never be emitted pre-escaped:\n{routes}"
    );

    // The preview route is mounted in main.rs.
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains("routes::posts::preview_body"),
        "the preview handler must be mounted or the editor 404s:\n{main_rs}"
    );

    // The `markdown` feature is enabled so the helper resolves.
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("markdown"), "{cargo}");

    // The preview POST hx-includes the whole form, so it must not spend the
    // one-time submit token.
    let autumn_toml = fs::read_to_string(project.join("autumn.toml")).unwrap();
    assert!(
        autumn_toml.contains("/posts/preview"),
        "the preview route must be submit-token exempt:\n{autumn_toml}"
    );
}

#[test]
fn richtext_scaffold_migration_uses_a_plain_text_column() {
    // The column stores Markdown SOURCE, never rendered HTML — so it is an
    // ordinary TEXT column with no special storage handling.
    let (_tmp, project) = rich_text_project("richtext-migration-app", &[]);
    let migrations = fs::read_dir(project.join("migrations")).unwrap();
    let up = migrations
        .filter_map(Result::ok)
        .map(|e| e.path().join("up.sql"))
        .find(|p| p.exists() && fs::read_to_string(p).is_ok_and(|s| s.contains("posts")))
        .expect("create_posts migration");
    let sql = fs::read_to_string(up).unwrap();
    assert!(sql.contains("body TEXT NOT NULL"), "{sql}");

    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    assert!(schema.contains("body -> Text,"), "{schema}");

    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(model.contains("pub body: String,"), "{model}");
}

/// Slow end-to-end check: the generated `richtext` project type-checks against
/// this workspace's `autumn-web`, proving the emitted editor call, preview
/// handler, and sanitized show render all resolve with the auto-enabled
/// `markdown` feature.
///
/// Ignored by default; run with
/// `cargo test -p autumn-cli --test cli_tests -- --ignored richtext`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn richtext_scaffold_cargo_checks() {
    let (_tmp, project) = rich_text_project("richtext-check-app", &[]);
    let cargo_toml_path = project.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\nautumn-macros = {{ path = \"{}\" }}\n",
        workspace_root
            .join("autumn")
            .display()
            .to_string()
            .replace('\\', "/"),
        workspace_root
            .join("autumn-macros")
            .display()
            .to_string()
            .replace('\\', "/"),
    );
    fs::write(&cargo_toml_path, content).unwrap();

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the richtext scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
