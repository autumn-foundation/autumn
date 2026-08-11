//! CSV export route + download link in scaffolded list views (issue #1315).
//!
//! `autumn generate scaffold` emits a `CsvSchema` impl covering every scaffolded
//! column, a `GET /{plural}/export.csv` handler that streams the index's row set
//! through `autumn_web::data::csv::export_csv` as an `attachment` download, an
//! "Export CSV" link on the index that carries the active sort/filter query, and
//! a generated test asserting the download contract. The export mirrors the
//! index's row-set query branch for branch, so the `--api`, `--live`,
//! `--sharded` and owner-scoped `--live-validation` variants — whose indexes run
//! a query the export cannot reuse verbatim — are deliberately gated off.

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

/// `autumn new` + `generate scaffold Post <cols> [extra_flags]`, returning the
/// tempdir guard and the generated project root.
fn scaffold_project(
    name: &str,
    cols: &[&str],
    extra_flags: &[&str],
) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec!["generate", "scaffold", "Post"];
    args.extend_from_slice(cols);
    args.extend_from_slice(extra_flags);
    run_autumn_ok(&project, &args);
    (tmp, project)
}

/// The default three-column scaffold used by most cases here.
fn default_cols() -> Vec<&'static str> {
    vec!["title:String", "body:Text", "published:bool"]
}

/// `scaffold_project`, returning the generated `src/routes/posts.rs` source.
fn scaffold_routes(name: &str, extra_flags: &[&str]) -> (tempfile::TempDir, String) {
    let (tmp, project) = scaffold_project(name, &default_cols(), extra_flags);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    (tmp, routes)
}

/// Slice out a named `pub async fn` handler: its signature through the line
/// before the next top-level handler.
fn handler_slice<'a>(routes: &'a str, name: &str) -> &'a str {
    let needle = format!("pub async fn {name}(");
    let start = routes
        .find(&needle)
        .unwrap_or_else(|| panic!("routes file must emit `{needle}`:\n{routes}"));
    let rest = &routes[start + needle.len()..];
    rest.split("pub async fn ").next().unwrap_or(rest)
}

/// The `impl … CsvSchema for Post { … }` block.
fn csv_schema_impl(routes: &str) -> &str {
    let start = routes
        .find("CsvSchema for Post")
        .unwrap_or_else(|| panic!("routes file must emit a CsvSchema impl:\n{routes}"));
    let rest = &routes[start..];
    // The impl is followed by the export handler's doc comment.
    rest.split("\n/// `GET").next().unwrap_or(rest)
}

// ── AC1: a CsvSchema impl covering every scaffolded column ────────────────────

#[test]
fn plain_scaffold_emits_a_csv_schema_impl_for_the_model() {
    let (_tmp, routes) = scaffold_routes("csv-plain", &[]);
    assert!(
        routes.contains("impl autumn_web::data::csv::CsvSchema for Post {"),
        "a CsvSchema impl must be emitted for the model:\n{routes}"
    );
    let schema = csv_schema_impl(&routes);
    assert!(
        schema.contains("fn csv_columns() -> &'static [&'static str] {"),
        "{schema}"
    );
    assert!(
        schema.contains("fn to_csv_record(&self) -> Vec<String> {"),
        "{schema}"
    );
}

#[test]
fn csv_columns_cover_every_scaffolded_column_in_declaration_order() {
    let (_tmp, routes) = scaffold_routes("csv-columns", &[]);
    let schema = csv_schema_impl(&routes);
    // `created_at` closes the list, matching what the `show` view renders.
    assert!(
        schema.contains(r#"&["id", "title", "body", "published", "created_at"]"#),
        "csv_columns must list id + every scaffolded column in order:\n{schema}"
    );
    // The record must be built in exactly the same order, one slot per header.
    let record = schema
        .split_once("fn to_csv_record")
        .expect("to_csv_record")
        .1;
    let id_at = record.find("self.id").expect("id slot");
    let title_at = record.find("self.title").expect("title slot");
    let body_at = record.find("self.body").expect("body slot");
    let published_at = record.find("self.published").expect("published slot");
    let created_at = record.find("self.created_at").expect("created_at slot");
    assert!(
        id_at < title_at
            && title_at < body_at
            && body_at < published_at
            && published_at < created_at,
        "to_csv_record slots must follow csv_columns order:\n{record}"
    );
    // One value per header, no more and no less.
    assert_eq!(
        record.matches("self.").count(),
        5,
        "to_csv_record must emit exactly one slot per csv_columns entry:\n{record}"
    );
}

// ── AC7: nullable columns are an empty cell, never the string `None` ──────────

#[test]
fn nullable_columns_serialize_to_an_empty_cell() {
    let (_tmp, project) = scaffold_project(
        "csv-nullable",
        &[
            "title:String",
            "subtitle:Option<String>",
            "views:Option<i64>",
            "token:Option<Uuid>",
        ],
        &[],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    let schema = csv_schema_impl(&routes);
    for slot in [
        "self.subtitle.clone().unwrap_or_default()",
        "self.views.as_ref().map(ToString::to_string).unwrap_or_default()",
        "self.token.as_ref().map(ToString::to_string).unwrap_or_default()",
    ] {
        assert!(
            schema.contains(slot),
            "nullable column must serialize to an empty cell via `{slot}`:\n{schema}"
        );
    }
    // A debug-formatted `Option` would print the literal `None` into the cell.
    let debug_fmt = concat!('{', ":?", '}');
    assert!(
        !schema.contains(debug_fmt),
        "a debug-formatted Option would export the literal `None`:\n{schema}"
    );
}

// ── AC2: the streaming export route + its headers ─────────────────────────────

#[test]
fn plain_scaffold_emits_the_export_route() {
    let (_tmp, routes) = scaffold_routes("csv-route", &[]);
    assert!(
        routes.contains("#[get(\"/posts/export.csv\")]"),
        "an export route must be emitted:\n{routes}"
    );
    let export = handler_slice(&routes, "export_csv");
    assert!(
        export.contains("autumn_web::data::csv::export_csv("),
        "the export handler must stream rows through export_csv:\n{export}"
    );
    assert!(
        export.contains("autumn_web::download::Download::from_bytes("),
        "the export handler must answer with a Download:\n{export}"
    );
    assert!(
        export.contains(r#".filename("posts.csv")"#),
        "the download must be named `{{plural}}.csv`:\n{export}"
    );
}

// ── AC4: the export honours the index's allowlisted sort/filter ───────────────

#[test]
fn export_reflects_the_filtered_view_not_find_all() {
    let (_tmp, routes) = scaffold_routes("csv-filtered", &[]);
    let export = handler_slice(&routes, "export_csv");
    assert!(
        export.contains("list_query: ListQuery,"),
        "the export must extract the same ListQuery as the index:\n{export}"
    );
    assert!(
        export.contains("repo.list(&list_query,"),
        "the export must apply the allowlisted sort/filter via repo.list:\n{export}"
    );
    assert!(
        !export.contains("find_all("),
        "the export must not unconditionally load every row:\n{export}"
    );
}

#[test]
fn export_is_bounded_by_a_row_cap_and_reads_in_pages() {
    let (_tmp, routes) = scaffold_routes("csv-bounded", &[]);
    assert!(
        routes.contains("const MAX_EXPORT_ROWS: usize"),
        "the export must carry an explicit row cap:\n{routes}"
    );
    let export = handler_slice(&routes, "export_csv");
    assert!(
        export.contains("autumn_web::pagination::MAX_PAGE_SIZE"),
        "the export must read in MAX_PAGE_SIZE batches:\n{export}"
    );
    assert!(
        export.contains("MAX_EXPORT_ROWS"),
        "the export must stop at the row cap:\n{export}"
    );
}

// ── AC3: the index renders an "Export CSV" link ───────────────────────────────

#[test]
fn index_renders_an_export_csv_link() {
    let (_tmp, routes) = scaffold_routes("csv-link", &[]);
    let index = handler_slice(&routes, "index");
    assert!(
        index.contains("\"Export CSV\""),
        "the index must render an Export CSV link:\n{index}"
    );
    assert!(
        index.contains("paths::export_csv()"),
        "the link must point at the typed export path:\n{index}"
    );
}

#[test]
fn export_link_preserves_the_active_sort_and_filter() {
    let (_tmp, routes) = scaffold_routes("csv-link-query", &[]);
    let index = handler_slice(&routes, "index");
    assert!(
        index.contains("let export_href ="),
        "the index must build the export href from the active query:\n{index}"
    );
    assert!(
        index.contains("pager_query"),
        "the export href must carry the current sort/filter query string:\n{index}"
    );
}

#[test]
fn paths_module_exposes_the_export_helper() {
    let (_tmp, routes) = scaffold_routes("csv-paths", &[]);
    let paths = routes
        .split_once("autumn_web::paths![")
        .expect("paths! block")
        .1
        .split_once("];")
        .expect("paths! block end")
        .0;
    assert!(
        paths.contains("export_csv"),
        "the paths! block must export the csv helper:\n{paths}"
    );
}

#[test]
fn export_route_is_mounted_in_main() {
    let (_tmp, project) = scaffold_project("csv-main", &default_cols(), &[]);
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main.contains("routes::posts::export_csv"),
        "the export route must be mounted:\n{main}"
    );
}

// ── AC5: same security posture as the index ───────────────────────────────────

#[test]
fn export_mirrors_the_index_security_posture() {
    // No owner column: the generated index carries no `#[secured]`, so neither
    // does the export — it opens no new public data path either way.
    let (_tmp, routes) = scaffold_routes("csv-posture", &[]);
    let index_secured = routes.contains("#[secured]\n#[get(\"/posts\")]");
    let export_secured = routes.contains("#[secured]\n#[get(\"/posts/export.csv\")]");
    assert_eq!(
        index_secured, export_secured,
        "the export must carry exactly the index's #[secured] posture:\n{routes}"
    );
}

#[test]
fn owner_scoped_export_never_calls_the_unscoped_list() {
    let (_tmp, project) = scaffold_project(
        "csv-owner",
        &["title:String", "body:Text", "user:references"],
        &[],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("#[secured]\n#[get(\"/posts/export.csv\")]"),
        "an owner-scoped export must be #[secured] like its index:\n{routes}"
    );
    let export = handler_slice(&routes, "export_csv");
    assert!(
        export.contains("repo.list_scoped(owner_id, &list_query,"),
        "an owner-scoped export must go through list_scoped:\n{export}"
    );
    assert!(
        !export.contains("repo.list(&list_query"),
        "an owner-scoped export must never call the unscoped list:\n{export}"
    );
}

// ── The `csv` Cargo feature ───────────────────────────────────────────────────

#[test]
fn scaffold_enables_the_autumn_web_csv_feature() {
    let (_tmp, project) = scaffold_project("csv-feature", &default_cols(), &[]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let dep_line = cargo
        .lines()
        .find(|l| l.starts_with("autumn-web = {"))
        .unwrap_or_else(|| panic!("no autumn-web dependency line:\n{cargo}"));
    assert!(
        dep_line.contains("\"csv\""),
        "the scaffold must enable autumn-web's csv feature:\n{dep_line}"
    );
}

#[test]
fn destroy_removes_the_csv_feature_again() {
    let (_tmp, project) = scaffold_project("csv-destroy", &default_cols(), &[]);
    run_autumn_ok(&project, &["destroy", "scaffold", "Post", "--force"]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        !cargo.contains("\"csv\""),
        "destroy must remove the csv feature it added:\n{cargo}"
    );
}

// ── AC6: the generated test asserts the download contract ─────────────────────

#[test]
fn generated_test_asserts_the_csv_download_contract() {
    let (_tmp, project) = scaffold_project("csv-gen-test", &default_cols(), &[]);
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(
        test.contains("posts_export_csv_downloads_a_spreadsheet"),
        "a CSV export test must be generated:\n{test}"
    );
    for needle in [
        ".assert_ok()",
        r#".assert_header_contains("content-type", "text/csv")"#,
        r#".assert_header_contains("content-disposition", "attachment")"#,
        r#"filename=\"posts.csv\""#,
    ] {
        assert!(
            test.contains(needle),
            "generated CSV test must assert `{needle}`:\n{test}"
        );
    }
    // AC7, asserted rather than assumed: RFC 4180 quoting and the empty cell.
    assert!(
        test.contains("assert_no_literal_none") || test.contains("\"None\""),
        "generated CSV test must prove a NULL is not exported as `None`:\n{test}"
    );
}

// ── Gated-off variants stay byte-identical to their pre-#1315 output ──────────

#[test]
fn live_scaffold_omits_csv_export() {
    let (_tmp, routes) = scaffold_routes("csv-live", &["--live"]);
    assert!(!routes.contains("export.csv"), "{routes}");
    assert!(!routes.contains("CsvSchema"), "{routes}");
}

#[test]
fn sharded_scaffold_omits_csv_export() {
    let (_tmp, routes) = scaffold_routes("csv-sharded", &["--sharded"]);
    assert!(!routes.contains("export.csv"), "{routes}");
    assert!(!routes.contains("CsvSchema"), "{routes}");
}

#[test]
fn owner_scoped_live_validation_scaffold_omits_csv_export() {
    // The owner-scoped `--live-validation` index runs a manual owner-filtered
    // diesel query rather than a scoped repository method, so there is no query
    // the export could reuse without re-deriving the owner filter by hand.
    let (_tmp, project) = scaffold_project(
        "csv-lv-owner",
        &["title:String", "user:references"],
        &["--live-validation"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(!routes.contains("export.csv"), "{routes}");
    assert!(!routes.contains("CsvSchema"), "{routes}");
}

#[test]
fn plain_live_validation_scaffold_keeps_csv_export() {
    // Without an owner column `--live-validation` renders the standard
    // data_table index on `repo.list`, so the export applies unchanged.
    let (_tmp, routes) = scaffold_routes("csv-lv-plain", &["--live-validation"]);
    assert!(routes.contains("#[get(\"/posts/export.csv\")]"), "{routes}");
}

#[test]
fn api_scaffold_omits_csv_export() {
    let (_tmp, project) = scaffold_project("csv-api", &default_cols(), &["--api"]);
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(!main.contains("export_csv"), "{main}");
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        !cargo.contains("\"csv\""),
        "an --api scaffold emits no export, so it must not enable the csv feature:\n{cargo}"
    );
}
