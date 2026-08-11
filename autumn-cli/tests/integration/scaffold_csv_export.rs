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

#[test]
fn export_reads_past_the_cap_so_truncation_is_detectable() {
    // Breaking on `rows.len() >= MAX_EXPORT_ROWS` fills the cap EXACTLY and so
    // never observes a row beyond it: `truncated` stays false, no warning is
    // logged and no `x-export-truncated` header is set, while the rows past the
    // cap are dropped anyway. Every over-cap export would then be silently
    // short — the precise failure the truncation signal exists to prevent. The
    // loop must read past the cap and decide truncation strictly.
    let (_tmp, routes) = scaffold_routes("csv-truncation", &[]);
    let export = handler_slice(&routes, "export_csv");
    assert!(
        !export.contains(">= MAX_EXPORT_ROWS"),
        "stopping once the cap is filled cannot distinguish a complete export \
         from a truncated one:\n{export}"
    );
    assert!(
        export.contains("if exhausted || rows.len() > MAX_EXPORT_ROWS"),
        "the export loop must read past the cap before stopping:\n{export}"
    );
    assert!(
        export.contains("let truncated = rows.len() > MAX_EXPORT_ROWS;"),
        "truncation must be decided by a strict over-cap comparison:\n{export}"
    );
    assert!(
        export.contains("rows.truncate(MAX_EXPORT_ROWS);"),
        "the surplus read past the cap must be trimmed before the CSV is \
         written:\n{export}"
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
    // The href is built FROM the raw query, not merely near a `pager_query`
    // binding (which every generated index has carried since the pager landed).
    assert!(
        index.contains(r#"format!("{}?{}", paths::export_csv(), pager_query)"#),
        "the export href must append the current sort/filter query string:\n{index}"
    );
    // ...and stays clean when there is no query to carry.
    assert!(
        index.contains("if pager_query.is_empty() {"),
        "an unfiltered index must link at the bare export path:\n{index}"
    );
    // Maud drops template whitespace, so the two links need an explicit
    // separator or they render as one glued run ("New PostExport CSV").
    assert!(
        index.contains(
            "\"New Post\"))\n        \" \"\n        (autumn_web::a11y::Link::new(export_href, \"Export CSV\"))"
        ),
        "the Export CSV link must be separated from the New Post link:\n{index}"
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
    // Pin the export's existence FIRST: without this the two `contains` bools
    // below are both `false` when the feature is deleted, and the equality
    // passes vacuously.
    assert!(
        routes.contains("#[get(\"/posts/export.csv\")]"),
        "the export route must exist for its posture to mean anything:\n{routes}"
    );
    let index_secured = routes.contains("#[secured]\n#[get(\"/posts\")]");
    let export_secured = routes.contains("#[secured]\n#[get(\"/posts/export.csv\")]");
    assert!(
        !index_secured,
        "a no-owner scaffold's index is expected to be unsecured; if that changed, \
         this test's premise needs revisiting:\n{routes}"
    );
    assert_eq!(
        index_secured, export_secured,
        "the export must carry exactly the index's #[secured] posture:\n{routes}"
    );
}

#[test]
fn export_carries_a_per_ip_throttle_the_index_does_not_need() {
    // One export reads up to MAX_EXPORT_ROWS rows over ~100 queries where one
    // index page reads 100 rows over two, so on an unsecured scaffold the route
    // is a large cost amplifier on traffic the index already accepts.
    let (_tmp, routes) = scaffold_routes("csv-throttle", &[]);
    assert!(
        routes.contains("#[autumn_web::throttle(limit = 6, per = \"1m\", key = \"ip\")]"),
        "the export must carry a per-IP throttle:\n{routes}"
    );
    // Fully qualified so no import is needed — a bare `#[throttle]` would not
    // resolve in the generated module.
    assert!(
        !routes.contains("\n#[throttle("),
        "the throttle attribute must be fully qualified:\n{routes}"
    );
}

#[test]
fn text_columns_are_guarded_against_spreadsheet_formula_injection() {
    // RFC 4180 (which `export_csv` implements) governs commas/quotes/newlines
    // and says nothing about formulas; Excel evaluates a leading `=` even
    // inside quotes.
    let (_tmp, project) = scaffold_project(
        "csv-formula",
        &["title:String", "views:i64", "at:DateTime"],
        &[],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("fn csv_text_cell(value: String) -> String {"),
        "a formula guard helper must be emitted:\n{routes}"
    );
    assert!(
        routes.contains("value.starts_with(['=', '+', '-', '@', '\\t', '\\r'])"),
        "{routes}"
    );
    let schema = csv_schema_impl(&routes);
    assert!(
        schema.contains("csv_text_cell(self.title.clone())"),
        "a text column must go through the guard:\n{schema}"
    );
    // Typed columns cannot carry a formula, and guarding them would prefix a
    // legitimate negative number.
    assert!(
        schema.contains("            self.views.to_string(),"),
        "a numeric column must NOT be guarded:\n{schema}"
    );
    assert!(
        schema.contains("            self.at.to_string(),"),
        "a timestamp column must NOT be guarded:\n{schema}"
    );
}

#[test]
fn scaffold_without_text_columns_omits_the_unused_guard_helper() {
    // An unused `fn` is a `dead_code` warning, and generated code must compile
    // warning-free.
    let (_tmp, project) = scaffold_project("csv-no-text", &["views:i64", "ok:bool"], &[]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("#[get(\"/posts/export.csv\")]"),
        "the export is still emitted:\n{routes}"
    );
    assert!(
        !routes.contains("fn csv_text_cell"),
        "no text column means no guard helper:\n{routes}"
    );
}

#[test]
fn soft_delete_scaffold_keeps_deleted_at_out_of_the_export() {
    // `list`/`list_scoped` filter `deleted_at IS NULL`, so the column is NULL
    // for every row that can reach the export — an always-blank spreadsheet
    // column is noise.
    let (_tmp, project) = scaffold_project("csv-soft", &default_cols(), &["--soft-delete"]);
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("deleted_at"),
        "premise: --soft-delete puts deleted_at on the model:\n{model}"
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    let schema = csv_schema_impl(&routes);
    assert!(
        !schema.contains("deleted_at"),
        "deleted_at must not be an exported column:\n{schema}"
    );
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(
        !test.contains("\"deleted_at\""),
        "the generated CSV test's header must match the impl:\n{test}"
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

#[test]
fn destroy_keeps_the_csv_feature_when_hand_written_code_still_uses_it() {
    // `import_csv` has no generator at all, so an author wiring an upload form
    // is doing it by hand by definition. Destroying the last scaffolded
    // resource must not strip the feature out from under them — the same rule
    // `storage`/`multipart` got in #1867.
    let (_tmp, project) = scaffold_project("csv-destroy-keep", &default_cols(), &[]);
    fs::write(
        project.join("src/reports.rs"),
        "use autumn_web::data::csv::export_csv;\n\
         pub fn write_report(out: &mut Vec<u8>) {\n    \
         let _ = export_csv(Vec::<crate::models::post::Post>::new(), out);\n\
         }\n",
    )
    .unwrap();
    run_autumn_ok(&project, &["destroy", "scaffold", "Post", "--force"]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"csv\""),
        "destroy must keep a feature hand-written code still references:\n{cargo}"
    );
}

#[test]
fn destroy_removes_the_csv_feature_when_the_surviving_resource_has_no_export() {
    // "some routes file still exists" is the wrong question for `csv`: only an
    // EXPORT-ENABLED resource needs it, so a surviving `--live` module must not
    // pin the feature (and the `csv` crate) in the graph forever.
    let (_tmp, project) = scaffold_project("csv-destroy-live", &default_cols(), &[]);
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Item", "name:String", "--live"],
    );
    run_autumn_ok(&project, &["destroy", "scaffold", "Post", "--force"]);
    assert!(
        project.join("src/routes/items.rs").exists(),
        "premise: the --live resource survives"
    );
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        !cargo.contains("\"csv\""),
        "a surviving export-less resource must not pin the csv feature:\n{cargo}"
    );
}

#[test]
fn destroy_keeps_the_csv_feature_while_another_export_survives() {
    let (_tmp, project) = scaffold_project("csv-destroy-two", &default_cols(), &[]);
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Article", "headline:String"],
    );
    run_autumn_ok(&project, &["destroy", "scaffold", "Post", "--force"]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"csv\""),
        "the surviving Article export still needs the csv feature:\n{cargo}"
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
    // AC7, asserted rather than assumed. The stand-in row's cells are
    // `Option<String>` with real `None`s, so the "not `None`" assertion has
    // failure power over the cell expression rather than being a tautology.
    assert!(
        test.contains("cells: Vec<Option<String>>,"),
        "the generated test's rows must model NULL columns as None:\n{test}"
    );
    assert!(
        test.contains("cell.clone().unwrap_or_default()"),
        "the generated test must use the same empty-cell expression as the impl:\n{test}"
    );
    assert!(
        test.contains("!body.contains(\"None\")"),
        "generated CSV test must prove a NULL is not exported as `None`:\n{test}"
    );
    // RFC 4180 escaping and the formula guard, both asserted against the real
    // response body.
    assert!(
        test.contains(r#"body.contains("\"\"q\"\"")"#),
        "generated CSV test must prove RFC 4180 quote escaping:\n{test}"
    );
    assert!(
        test.contains(r#"body.contains("'=HYPERLINK")"#),
        "generated CSV test must prove the formula guard reaches the body:\n{test}"
    );
}

// ── Gated-off variants stay byte-identical to their pre-#1315 output ──────────

/// A gated-off variant must omit the export from the routes module, the
/// `main.rs` mount AND the Cargo feature — all three or none. The failure the
/// two "must agree exactly" gate comments exist to prevent is precisely a
/// `main.rs` entry for a handler the module never emitted, which is a compile
/// error in the user's project, so checking only the routes file would miss it.
fn assert_no_export_anywhere(project: &Path, routes_file: &str) {
    let routes = fs::read_to_string(project.join(routes_file)).unwrap_or_default();
    assert!(!routes.contains("export.csv"), "{routes}");
    assert!(!routes.contains("CsvSchema"), "{routes}");
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !main.contains("routes::posts::export_csv"),
        "main.rs must not mount an export the module never emitted:\n{main}"
    );
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        !cargo.contains("\"csv\""),
        "no export means no csv feature:\n{cargo}"
    );
}

#[test]
fn live_scaffold_omits_csv_export() {
    let (_tmp, project) = scaffold_project("csv-live", &default_cols(), &["--live"]);
    assert_no_export_anywhere(&project, "src/routes/posts.rs");
}

#[test]
fn sharded_scaffold_omits_csv_export() {
    let (_tmp, project) = scaffold_project("csv-sharded", &default_cols(), &["--sharded"]);
    assert_no_export_anywhere(&project, "src/routes/posts.rs");
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
    assert_no_export_anywhere(&project, "src/routes/posts.rs");
}

#[test]
fn plain_live_validation_scaffold_keeps_csv_export() {
    // Without an owner column `--live-validation` renders the standard
    // data_table index on `repo.list`, so the export applies unchanged.
    let (_tmp, project) = scaffold_project("csv-lv-plain", &default_cols(), &["--live-validation"]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(routes.contains("#[get(\"/posts/export.csv\")]"), "{routes}");
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main.contains("routes::posts::export_csv"), "{main}");
}

#[test]
fn api_scaffold_omits_csv_export() {
    // `--api` emits no routes module at all, so there is no file to read.
    let (_tmp, project) = scaffold_project("csv-api", &default_cols(), &["--api"]);
    assert!(!project.join("src/routes/posts.rs").exists());
    assert_no_export_anywhere(&project, "src/routes/posts.rs");
}
