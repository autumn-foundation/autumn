//! CSV import route with dry-run preview and row errors (issue #1393).
//!
//! `autumn generate scaffold --import` emits the other half of the data door
//! #1315 opened: a `GET /{plural}/import` upload form and a
//! `POST /{plural}/import` handler that parses the uploaded multipart CSV,
//! previews it through `autumn_web::data::csv::import_csv` in
//! `ImportMode::DryRun` unless the submit explicitly confirms a commit, and
//! renders a per-row error report drawn from `ImportReport`. A confirmed
//! commit writes the valid rows through the repository's
//! `save_many_skip_invalid` — a batch insert inside a transaction that falls
//! back to row-by-row on a constraint violation, so no row is dropped
//! silently.
//!
//! The import reuses the SAME `CsvSchema` impl the export emits (one column map
//! drives both directions), so it is gated behind exactly the export's gate.

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

fn run_autumn_ok(dir: &Path, args: &[&str]) -> String {
    let output = run_autumn(dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// `autumn new` + `generate scaffold Post <cols> [extra_flags]`, returning the
/// tempdir guard, the generated project root and the generator's own output.
fn scaffold_project(
    name: &str,
    cols: &[&str],
    extra_flags: &[&str],
) -> (tempfile::TempDir, PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec!["generate", "scaffold", "Post"];
    args.extend_from_slice(cols);
    args.extend_from_slice(extra_flags);
    let output = run_autumn_ok(&project, &args);
    (tmp, project, output)
}

/// A slug column token. Held in a const so the `{from:title}` braces never sit
/// inside a macro or array literal, where clippy reads them as a format
/// argument (same reason `scaffold_lock_version.rs` holds one).
const SLUG_COL: &str = "slug:slug{from:title}";

/// The default three-column scaffold used by most cases here.
fn default_cols() -> Vec<&'static str> {
    vec!["title:String", "body:Text", "published:bool"]
}

/// `scaffold_project` with `--import`, returning `src/routes/posts.rs`.
fn import_routes(name: &str, extra_flags: &[&str]) -> (tempfile::TempDir, String) {
    let mut flags = vec!["--import"];
    flags.extend_from_slice(extra_flags);
    let (tmp, project, _) = scaffold_project(name, &default_cols(), &flags);
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

/// Slice out a private `fn name(` helper: its signature through the line before
/// the next top-level `fn`/handler.
fn fn_slice<'a>(routes: &'a str, name: &str) -> &'a str {
    let needle = format!("\nfn {name}(");
    let start = routes
        .find(&needle)
        .unwrap_or_else(|| panic!("routes file must emit `fn {name}(`:\n{routes}"));
    let rest = &routes[start + needle.len()..];
    rest.split("\nfn ")
        .next()
        .unwrap_or(rest)
        .split("\npub async fn ")
        .next()
        .unwrap_or(rest)
}

// ── AC1: two routes, emitted and registered ───────────────────────────────────

#[test]
fn import_flag_emits_an_upload_form_route_and_a_handler_route() {
    let (_tmp, routes) = import_routes("import-routes", &[]);
    assert!(
        routes.contains(r#"#[get("/posts/import")]"#),
        "the upload form route must be emitted:\n{routes}"
    );
    assert!(
        routes.contains(r#"#[post("/posts/import")]"#),
        "the import handler route must be emitted:\n{routes}"
    );
    assert!(
        routes.contains("pub async fn import_form("),
        "the upload form handler must be emitted:\n{routes}"
    );
    assert!(
        routes.contains("pub async fn import("),
        "the import handler must be emitted:\n{routes}"
    );
}

#[test]
fn both_import_routes_are_mounted_in_main() {
    let mut flags = vec!["--import"];
    flags.extend_from_slice(&[] as &[&str]);
    let (_tmp, project, _) = scaffold_project("import-main", &default_cols(), &flags);
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    for entry in ["routes::posts::import_form", "routes::posts::import"] {
        assert!(
            main.contains(entry),
            "`{entry}` must be mounted in main.rs:\n{main}"
        );
    }
}

#[test]
fn paths_module_exposes_the_import_helper() {
    let (_tmp, routes) = import_routes("import-paths", &[]);
    let paths = routes
        .split_once("autumn_web::paths![")
        .expect("paths! block")
        .1
        .split_once("];")
        .expect("paths! block end")
        .0;
    for helper in ["import_form", "import"] {
        assert!(
            paths.contains(helper),
            "the paths! block must expose `{helper}`:\n{paths}"
        );
    }
}

#[test]
fn index_links_to_the_import_page() {
    let (_tmp, routes) = import_routes("import-link", &[]);
    let index = handler_slice(&routes, "index");
    assert!(
        index.contains("paths::import_form()"),
        "the index must link to the import page:\n{index}"
    );
}

// ── AC2: dry-run preview by default ───────────────────────────────────────────

#[test]
fn the_handler_previews_in_dry_run_mode_unless_the_submit_confirms() {
    let (_tmp, routes) = import_routes("import-dryrun", &[]);
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("autumn_web::data::csv::ImportMode::DryRun"),
        "an unconfirmed submit must run in DryRun:\n{import}"
    );
    assert!(
        import.contains("autumn_web::data::csv::ImportMode::Insert"),
        "a confirmed submit must run in Insert mode:\n{import}"
    );
    assert!(
        import.contains("autumn_web::data::csv::import_csv("),
        "the handler must drive the shipped import engine:\n{import}"
    );
    // The write call must be reachable only under the confirmation flag.
    let (before_write, _) = import
        .split_once("save_many_skip_invalid")
        .expect("the commit path must call save_many_skip_invalid");
    assert!(
        before_write.contains("if commit {"),
        "the write must sit behind the explicit commit confirmation:\n{import}"
    );
}

#[test]
fn the_preview_reports_totals_and_per_row_errors_with_line_numbers() {
    let (_tmp, routes) = import_routes("import-preview", &[]);
    // The report renderer is shared by the preview and the commit result.
    assert!(
        routes.contains("fn import_report_view("),
        "a shared report view must be emitted:\n{routes}"
    );
    let view = routes
        .split_once("fn import_report_view(")
        .expect("report view")
        .1;
    let view = view.split("\npub async fn ").next().unwrap_or(view);
    for needle in [
        "total_rows()",
        "report.inserted",
        "error.line",
        "error.message",
    ] {
        assert!(
            view.contains(needle),
            "the report view must render `{needle}`:\n{view}"
        );
    }
    assert!(
        view.contains("error.column"),
        "a field-level error must name its column:\n{view}"
    );
}

// ── AC3: a confirmed commit writes inside a transaction, no silent drops ──────

#[test]
fn a_confirmed_commit_writes_valid_rows_and_surfaces_row_level_failures() {
    let (_tmp, routes) = import_routes("import-commit", &[]);
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("repo.save_many_skip_invalid("),
        "the commit must go through the transactional batch insert:\n{import}"
    );
    // Every DB-level failure is folded back into the report against its own
    // CSV line, so inserted-vs-failed always adds up.
    assert!(
        import.contains("report.errors.push("),
        "row-level write failures must be surfaced in the report:\n{import}"
    );
    assert!(
        import.contains("report.inserted"),
        "the inserted count must be corrected for failed writes:\n{import}"
    );
}

// ── AC4: one column map drives both directions ────────────────────────────────

#[test]
fn the_import_reuses_the_exports_csv_schema_impl() {
    let (_tmp, routes) = import_routes("import-schema", &[]);
    assert_eq!(
        routes
            .matches("impl autumn_web::data::csv::CsvSchema for Post {")
            .count(),
        1,
        "the import must reuse the export's single CsvSchema impl:\n{routes}"
    );
    let form = fn_slice(&routes, "import_form_body");
    assert!(
        form.contains("csv_columns()"),
        "the upload form must name the expected columns from CsvSchema:\n{form}"
    );
}

/// A file this app EXPORTED must re-import unchanged. Two places the two
/// directions could otherwise disagree, both pinned here.
#[test]
fn a_file_this_app_exported_round_trips_back_in() {
    let (_tmp, routes) = import_routes("import-roundtrip", &[]);
    // 1. The export's spreadsheet-formula guard prefixes an apostrophe to a text
    //    value beginning `=`/`+`/`-`/`@`. Without the inverse, every round trip
    //    would store one more apostrophe.
    assert!(
        routes.contains("fn csv_unguard_cell("),
        "the import must undo the export's formula guard:\n{routes}"
    );
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("csv_unguard_cell(value)"),
        "the row decoder must route each value through the inverse guard:\n{import}"
    );
}

#[test]
fn the_unguard_helper_is_omitted_when_no_column_can_carry_a_formula() {
    // Every column here is typed (numeric/bool), so `render_csv_schema_impl`
    // emits no `csv_text_cell` — and an unused inverse would be a `dead_code`
    // warning in the generated app, whose contract is to compile clean.
    let (_tmp, project, _) = scaffold_project(
        "import-noguard",
        &["count:i32", "published:bool"],
        &["--import"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    // The `fn`, not the word: `render_csv_schema_impl`'s doc comment names the
    // guard even where no column routes through it.
    assert!(!routes.contains("fn csv_text_cell("), "{routes}");
    assert!(
        !routes.contains("csv_unguard_cell"),
        "no guarded column means no inverse to emit:\n{routes}"
    );
}

#[test]
fn the_datetime_parser_accepts_the_format_the_export_writes() {
    // The export writes a timestamp with chrono's `Display` (a SPACE between
    // date and time); the browser's `datetime-local` control sends a `T`. An
    // import scaffold has to accept both or an exported file fails to re-import
    // on exactly the column it just wrote.
    let (_tmp, project, _) = scaffold_project(
        "import-datetime",
        &["title:String", "published_at:NaiveDateTime"],
        &["--import"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains(r#"parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")"#),
        "the datetime parser must accept the exported format:\n{routes}"
    );

    // ...and without the flag the helper is the pre-#1393 one, byte for byte.
    let (_tmp2, plain, _) = scaffold_project(
        "import-datetime-plain",
        &["title:String", "published_at:NaiveDateTime"],
        &[],
    );
    let plain_routes = fs::read_to_string(plain.join("src/routes/posts.rs")).unwrap();
    assert!(
        plain_routes.contains("fn parse_local_datetime("),
        "the plain scaffold still emits the helper:\n{plain_routes}"
    );
    assert!(
        !plain_routes.contains(r#"parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")"#),
        "a scaffold without --import must keep the pre-#1393 helper:\n{plain_routes}"
    );
}

// ── AC5: CSRF, size limit, and a content-type/extension check ─────────────────

#[test]
fn the_post_is_csrf_protected_and_the_form_carries_the_token_first() {
    let (_tmp, routes) = import_routes("import-csrf", &[]);
    assert!(
        routes.contains("#[secured]\n#[post(\"/posts/import\")]"),
        "the import POST must be secured:\n{routes}"
    );
    let form = fn_slice(&routes, "import_form_body");
    assert!(
        form.contains("(csrf_input(csrf, csrf_field))"),
        "the upload form must render the shipped CSRF hidden input:\n{form}"
    );
    assert!(
        form.contains(r#"enctype="multipart/form-data""#),
        "the upload form must submit multipart:\n{form}"
    );
    // The CSRF/submit-token inputs must precede the file input so they land
    // inside `security.csrf.token_scan_bytes` for a multipart body.
    let csrf_at = form.find("csrf_input(").expect("csrf input");
    let file_at = form.find(r#"type="file""#).expect("file input");
    assert!(
        csrf_at < file_at,
        "the CSRF field must be rendered before the file part:\n{form}"
    );
}

#[test]
fn the_upload_is_size_capped_and_checked_by_extension_and_content_type() {
    let (_tmp, routes) = import_routes("import-guards", &[]);
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains(".with_max_bytes(MAX_IMPORT_BYTES)"),
        "the upload must be capped by the emitted per-route limit:\n{import}"
    );
    assert!(
        import.contains(".bytes_limited()"),
        "the upload must go through the framework's size-limited read:\n{import}"
    );
    assert!(
        routes.contains("const MAX_IMPORT_BYTES: usize"),
        "the per-route upload cap must be an editable constant:\n{routes}"
    );
    assert!(
        routes.contains("fn is_csv_upload("),
        "a content-type/extension check must be emitted:\n{routes}"
    );
    let check = routes.split_once("fn is_csv_upload(").expect("check").1;
    let check = check.split("\nfn ").next().unwrap_or(check);
    assert!(
        check.contains(r#"eq_ignore_ascii_case("csv")"#),
        "the check must accept the .csv extension, case-insensitively:\n{check}"
    );
    assert!(
        check.contains("text/csv"),
        "the check must accept the text/csv media type:\n{check}"
    );
}

// ── AC6: the generated smoke test covers preview AND commit ───────────────────

#[test]
fn the_generated_smoke_test_previews_then_commits() {
    let (_tmp, project, _) = scaffold_project("import-gen-test", &default_cols(), &["--import"]);
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(
        test.contains("posts_csv_import_previews_then_commits"),
        "a CSV import test must be generated:\n{test}"
    );
    for needle in [
        // a 2-row CSV: one valid, one invalid
        "multipart/form-data; boundary=BOUND",
        "use autumn_web::data::csv::{ImportMode, ImportOptions, ImportRowResult, import_csv};",
        "import_csv(&uploaded[..], &options,",
        "ImportMode::DryRun",
    ] {
        assert!(
            test.contains(needle),
            "the generated import test must exercise `{needle}`:\n{test}"
        );
    }
    // Dry run: 1 insertable + 1 row error, and NOTHING persisted.
    assert!(
        test.contains("would insert 1"),
        "the generated test must assert the dry-run insertable count:\n{test}"
    );
    assert!(
        test.contains("a dry run must not write"),
        "the generated test must assert the dry run persisted nothing:\n{test}"
    );
    // Commit: exactly one row survives.
    assert!(
        test.contains("exactly the valid row must persist"),
        "the generated test must assert the committed row count:\n{test}"
    );
}

// ── Cargo features ────────────────────────────────────────────────────────────

#[test]
fn the_import_enables_the_multipart_feature() {
    let (_tmp, project, _) = scaffold_project("import-cargo", &default_cols(), &["--import"]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"multipart\""),
        "the import handler needs autumn-web's multipart feature:\n{cargo}"
    );
    assert!(
        cargo.contains("\"csv\""),
        "the import handler needs autumn-web's csv feature:\n{cargo}"
    );
}

#[test]
fn destroy_takes_the_import_back_out() {
    let (_tmp, project, _) = scaffold_project("import-destroy", &default_cols(), &["--import"]);
    run_autumn_ok(&project, &["destroy", "scaffold", "Post", "--force"]);
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !main.contains("routes::posts::import"),
        "destroy must unmount the import routes:\n{main}"
    );
}

// ── A slug'd resource cannot be shadowed by the static segment ────────────────

#[test]
fn a_derived_slug_never_collides_with_the_import_segment() {
    let (_tmp, project, _) =
        scaffold_project("import-slug", &["title:String", SLUG_COL], &["--import"]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains(r#"candidate != "import""#),
        "a derived slug of \"import\" would be shadowed by the static route:\n{routes}"
    );
}

// ── AC7: additive — without the flag nothing changes ──────────────────────────

#[test]
fn omitting_the_flag_leaves_the_scaffold_untouched() {
    let (_tmp, project, _) = scaffold_project("import-absent", &default_cols(), &[]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(!routes.contains("/posts/import"), "{routes}");
    assert!(!routes.contains("import_csv"), "{routes}");
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(!main.contains("routes::posts::import"), "{main}");
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        !cargo.contains("\"multipart\""),
        "no import means no multipart feature:\n{cargo}"
    );
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(!test.contains("csv_import"), "{test}");
}

// ── Gated-off variants warn instead of emitting a broken module ───────────────

/// The import shares the export's `CsvSchema`, so it is emitted exactly where
/// the export is. A variant that cannot have one must emit no import surface at
/// all — routes module, `main.rs` mount and Cargo feature alike — and must say
/// why rather than leaving the author to notice the missing link.
fn assert_no_import_anywhere(project: &Path) {
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap_or_default();
    assert!(!routes.contains("/posts/import"), "{routes}");
    assert!(!routes.contains("import_csv"), "{routes}");
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !main.contains("routes::posts::import"),
        "main.rs must not mount an import the module never emitted:\n{main}"
    );
}

#[test]
fn live_scaffold_omits_the_import_and_says_why() {
    let (_tmp, project, output) =
        scaffold_project("import-live", &default_cols(), &["--import", "--live"]);
    assert_no_import_anywhere(&project);
    assert!(
        output.contains("--import"),
        "the generator must warn that --import was not honoured:\n{output}"
    );
}

#[test]
fn sharded_scaffold_omits_the_import() {
    let (_tmp, project, _) = scaffold_project(
        "import-sharded",
        &default_cols(),
        &["--import", "--sharded"],
    );
    assert_no_import_anywhere(&project);
}

#[test]
fn api_scaffold_omits_the_import() {
    let (_tmp, project, _) =
        scaffold_project("import-api", &default_cols(), &["--import", "--api"]);
    assert!(!project.join("src/routes/posts.rs").exists());
    assert_no_import_anywhere(&project);
}

// ── Owner-scoped scaffolds keep the index's posture ───────────────────────────

#[test]
fn an_owner_scoped_scaffold_authorizes_the_import_like_create() {
    let (_tmp, project, _) = scaffold_project(
        "import-owner",
        &["title:String", "user:references"],
        &["--import"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("authorize_create::<Post>"),
        "an owner-scoped import must run the same create authorization the \
         create handler runs:\n{import}"
    );
}
