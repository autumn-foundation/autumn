//! `autumn generate scaffold … --searchable title,body` wires a full-text
//! search box into the generated list view (issue #1319): `#[searchable]` on
//! the model, `searchable` on the repository, a `search_vector` migration, and
//! an `active_search` widget targeting a `search_page`-backed results route.

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

fn run_autumn_expect_fail(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    assert!(
        !output.status.success(),
        "autumn {args:?} unexpectedly succeeded\nstdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `autumn new` + `generate scaffold Post title:String body:Text --searchable
/// title,body`, returning the project root of the fresh app.
fn searchable_project() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "search-app"]);
    let project = tmp.path().join("search-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--searchable",
            "title,body",
        ],
    );
    (tmp, project)
}

#[test]
fn model_carries_searchable_attributes() {
    let (_tmp, project) = searchable_project();
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("#[searchable(language = \"english\")]"),
        "model must opt into full-text search:\n{model}"
    );
    assert!(
        model.contains("#[searchable(weight = \"A\")]"),
        "first searchable field must carry the `A` weight:\n{model}"
    );
    assert!(
        model.contains("#[searchable(weight = \"B\")]"),
        "second searchable field must carry the `B` weight:\n{model}"
    );
}

#[test]
fn repository_carries_searchable() {
    let (_tmp, project) = searchable_project();
    let repo = fs::read_to_string(project.join("src/repositories/post.rs")).unwrap();
    assert!(
        repo.contains("api = \"/api/posts\", searchable)"),
        "repository attribute must carry `searchable`:\n{repo}"
    );
}

#[test]
fn migration_adds_search_vector_and_gin_index() {
    let (_tmp, project) = searchable_project();
    let migrations = project.join("migrations");
    let up = fs::read_dir(&migrations)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path().join("up.sql"))
        .find(|p| p.exists())
        .and_then(|p| fs::read_to_string(p).ok())
        .expect("an up.sql migration must exist");
    assert!(
        up.contains("ADD COLUMN search_vector tsvector GENERATED ALWAYS AS"),
        "migration must add the generated tsvector column:\n{up}"
    );
    assert!(
        up.contains("USING gin(search_vector)"),
        "migration must add the GIN index:\n{up}"
    );
    // Consistent with what the `#[repository(..., searchable)]` macro queries.
    assert!(
        up.contains("to_tsvector('english'::regconfig")
            && up.contains("coalesce(\"title\"::text, '')")
            && up.contains("coalesce(\"body\"::text, '')"),
        "migration weights must cover both searchable fields:\n{up}"
    );
}

#[test]
fn search_vector_is_not_in_schema_or_struct() {
    let (_tmp, project) = searchable_project();
    let schema = fs::read_to_string(project.join("src/schema.rs")).unwrap();
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        !schema.contains("search_vector"),
        "search_vector is a generated column and must not appear in schema.rs:\n{schema}"
    );
    assert!(
        !model.contains("search_vector"),
        "search_vector is a generated column and must not appear in the model struct:\n{model}"
    );
}

#[test]
fn index_renders_search_box_wired_to_results_route() {
    let (_tmp, project) = searchable_project();
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("active_search(\"post-search\""),
        "index must render the active_search widget:\n{routes}"
    );
    assert!(
        routes.contains("ActiveSearchConfig::new(\"/posts/search\", \"#posts-search-results\")"),
        "search input must target the generated results route/container:\n{routes}"
    );
    // htmx isn't loaded by the shared layout, so the index inlines the script
    // and keeps a <noscript> plain listing for graceful degradation.
    assert!(
        routes.contains("HTMX_JS_PATH") && routes.contains("noscript"),
        "search index must inline htmx and degrade without JS:\n{routes}"
    );
}

#[test]
fn search_handler_calls_search_page_with_empty_fallback() {
    let (_tmp, project) = searchable_project();
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("#[get(\"/posts/search\")]") && routes.contains("pub async fn search("),
        "a search results handler must be emitted:\n{routes}"
    );
    assert!(
        routes.contains("repo.search_page(q, &page_req)"),
        "search handler must call search_page:\n{routes}"
    );
    assert!(
        routes.contains("repo.page(&page_req).await?"),
        "an empty query must fall back to page():\n{routes}"
    );
}

#[test]
fn search_route_registered_in_main() {
    let (_tmp, project) = searchable_project();
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains("routes::posts::search"),
        "the search route must be registered in main.rs:\n{main_rs}"
    );
}

#[test]
fn omitting_searchable_is_purely_additive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "plain-app"]);
    let project = tmp.path().join("plain-app");
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "body:Text"],
    );
    for rel in [
        "src/models/post.rs",
        "src/repositories/post.rs",
        "src/routes/posts.rs",
    ] {
        let content = fs::read_to_string(project.join(rel)).unwrap();
        assert!(
            !content.contains("searchable") && !content.contains("active_search"),
            "a plain scaffold must carry no FTS artifacts in `{rel}`:\n{content}"
        );
    }
}

#[test]
fn searchable_non_text_field_fails_generation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "reject-app"]);
    let project = tmp.path().join("reject-app");
    let stderr = run_autumn_expect_fail(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "views:i64",
            "--searchable",
            "views",
        ],
    );
    assert!(
        stderr.contains("views"),
        "error must name the offending field: {stderr}"
    );
}

/// Slow end-to-end check: the `--searchable` scaffold type-checks against this
/// workspace's `autumn-web`.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn searchable_scaffold_cargo_checks() {
    use std::fmt::Write as _;

    let (_tmp, project) = searchable_project();

    let cargo_toml_path = project.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    let autumn_macros = workspace_root.join("autumn-macros");
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\nautumn-macros = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/"),
        autumn_macros.display().to_string().replace('\\', "/"),
    );
    fs::write(&cargo_toml_path, content).unwrap();

    let check = Command::new("cargo")
        .args(["check", "--bins"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "generated --searchable app failed to compile\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
