//! Allowlisted sort/filter wiring in scaffolded index views (issue #1126).
//!
//! The generated non-live `index` handler extracts a `ListQuery`, applies it
//! via `repo.list(..)`, and renders a `data_table` whose sortable headers +
//! active-sort state are symmetric with what the server orders by. The `--live`
//! variant keeps the SSE `<ul>` and is deliberately gated off from sort/filter.

use std::fs;
use std::path::Path;
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

/// `autumn new` + `generate scaffold Post <cols> [extra_flags]`, returning the
/// generated `posts.rs` routes file.
fn scaffold_routes(name: &str, extra_flags: &[&str]) -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec![
        "generate",
        "scaffold",
        "Post",
        "title:String",
        "body:Text",
        "views:i64",
        "published:bool",
    ];
    args.extend_from_slice(extra_flags);
    run_autumn_ok(&project, &args);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    (tmp, routes)
}

#[test]
fn index_extracts_list_query_and_calls_list() {
    let (_tmp, routes) = scaffold_routes("sf-plain", &[]);
    assert!(
        routes.contains("use autumn_web::pagination::ListQuery;"),
        "index must import the ListQuery extractor:\n{routes}"
    );
    assert!(
        routes.contains("use autumn_web::reexports::axum::extract::RawQuery;"),
        "index must import RawQuery to preserve filters on sort links:\n{routes}"
    );
    assert!(
        routes.contains("list_query: ListQuery,"),
        "the index handler must extract ListQuery:\n{routes}"
    );
    assert!(
        routes.contains("repo.list(&list_query, &page_req)"),
        "the index must call the allowlisted list() method, not page():\n{routes}"
    );
    assert!(
        !routes.contains("repo.page(&page_req)"),
        "the non-live index must not fall back to the unsorted page():\n{routes}"
    );
}

#[test]
fn columns_are_sortable_and_config_is_symmetric() {
    let (_tmp, routes) = scaffold_routes("sf-cols", &[]);
    // Every scalar column opts into a sort header keyed by its column name.
    for key in ["id", "title", "body", "views", "published"] {
        assert!(
            routes.contains(&format!(".sortable(\"{key}\")")),
            "column `{key}` must render a sortable header:\n{routes}"
        );
    }
    // The data_table config is wired symmetrically with what the server applies.
    assert!(
        routes.contains(".query(raw_query.as_deref().unwrap_or_default())"),
        "data_table must preserve the raw query string on sort links:\n{routes}"
    );
    assert!(
        routes.contains(".active_sort(list_query.sort().unwrap_or_default())")
            && routes.contains(".active_dir(list_query.direction())"),
        "data_table active-sort state must mirror the ListQuery:\n{routes}"
    );
}

#[test]
fn live_variant_gates_sort_filter_off() {
    // The `--live` SSE `<ul>` has no column headers; sort/filter is gated off
    // and the index keeps the plain `page()` call.
    let (_tmp, routes) = scaffold_routes("sf-live", &["--live"]);
    assert!(
        routes.contains("repo.page(&page_req)"),
        "the --live index keeps the unsorted page() call:\n{routes}"
    );
    assert!(
        !routes.contains("repo.list("),
        "the --live index must not wire the data_table list():\n{routes}"
    );
    assert!(
        !routes.contains("ListQuery"),
        "the --live index must not import/extract ListQuery:\n{routes}"
    );
}

#[test]
fn live_validation_variant_gets_sort_filter() {
    // `--live-validation` (without `--live`) renders the normal data_table
    // index, so it DOES get sort/filter.
    let (_tmp, routes) = scaffold_routes("sf-liveval", &["--live-validation"]);
    assert!(
        routes.contains("repo.list(&list_query, &page_req)"),
        "the --live-validation index must wire the allowlisted list():\n{routes}"
    );
    assert!(
        routes.contains(".sortable(\"title\")"),
        "the --live-validation index must render sortable headers:\n{routes}"
    );
}

#[test]
fn sharded_variant_keeps_from_shard_and_lists() {
    let (_tmp, routes) = scaffold_routes("sf-sharded", &["--sharded", "--shard-key", "title"]);
    assert!(
        routes.contains("PgPostRepository::from_shard(&db)"),
        "the sharded index must pin a single shard via from_shard:\n{routes}"
    );
    assert!(
        routes.contains("repo.list(&list_query, &page_req)"),
        "the sharded index must apply the allowlisted list() on the shard:\n{routes}"
    );
}
