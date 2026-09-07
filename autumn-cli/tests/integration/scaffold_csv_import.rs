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
    // No `(`: some helpers carry a lifetime parameter (`fn f<'a>(…)`).
    let needle = format!("\nfn {name}");
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
    // `routes::posts::import` is a SUBSTRING of `routes::posts::import_form`, so
    // each entry is matched with its own line terminator — otherwise mounting
    // only the form would satisfy both assertions.
    for entry in ["routes::posts::import_form,", "routes::posts::import,"] {
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
    // Terminated, for the same substring reason as the `main.rs` mount above.
    for helper in ["    import_form,", "    import,"] {
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
    // The write must be the FIRST statement of the commit block. A looser
    // "somewhere after an `if commit {`" check is vacuous: the handler already
    // contains `mode: if commit {` and a second `if commit {` inside the row
    // closure, both before the real gate, so deleting the gate entirely would
    // still satisfy it. This is the single safety property of the whole feature.
    let (before_write, _) = import
        .split_once("repo.save_many_skip_invalid(")
        .expect("the commit path must call save_many_skip_invalid");
    let gate = before_write
        .rfind("if commit {")
        .expect("the write must sit inside an `if commit` block");
    // Walk from just inside that `if commit {` to the write, tracking block
    // depth. If the block CLOSES before the write is reached, the `if commit`
    // found above is not the one guarding it — which is exactly what would
    // happen if the real gate were deleted and `rfind` landed on the closure's
    // own `if commit { pending_lines.push(..) }` instead.
    let between = &before_write[gate + "if commit {".len()..];
    let mut depth: i32 = 1;
    for byte in between.bytes() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        assert!(
            depth > 0,
            "the `if commit` block closes before the write, so the write is not \
             gated by it:\n{between}"
        );
    }
    assert_eq!(
        import.matches("repo.save_many_skip_invalid(").count(),
        1,
        "exactly one write call, so the gate above covers all of them:\n{import}"
    );
    // `import_csv` hands the mode to the closure and never acts on it itself, so
    // the mode alone does not make a dry run safe — say so here, where someone
    // tempted to relax the gate above will read it.
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
        routes.contains("fn csv_unguard_cell<'a>("),
        "the import must undo the export's formula guard:\n{routes}"
    );
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("csv_unguard_cell(key, value)"),
        "the row decoder must route each value through the inverse guard:\n{import}"
    );
    // ...and only on the columns the export actually guards. Stripping an
    // apostrophe from a typed column would make the import quietly more
    // permissive than the form it claims to mirror (`'-5` -> `-5`).
    assert!(
        routes.contains(r#"const CSV_TEXT_COLUMNS: &[&str] = &["title", "body"];"#),
        "the inverse must be scoped to the guarded columns:\n{routes}"
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

/// A spreadsheet does not write `true`/`false`. Excel writes `TRUE`/`FALSE`,
/// people write `1`/`0` or `yes`, and "no" is very often an empty cell — while
/// serde's `bool` accepts exactly two spellings. Without a normalizer every one
/// of those fails the WHOLE ROW on a column the generated form itself treats as
/// optional (`#[serde(default)] pub published: bool`).
#[test]
fn a_boolean_column_accepts_the_spellings_a_spreadsheet_writes() {
    let (_tmp, routes) = import_routes("import-bools", &[]);
    assert!(
        routes.contains(r#"const CSV_BOOL_COLUMNS: &[(&str, bool)] = &[("published", true)];"#),
        "the boolean columns must be listed with their blank-cell meaning:\n{routes}"
    );
    let cell = fn_slice(&routes, "csv_bool_cell");
    for needle in [
        r#""true" | "t" | "yes" | "y" | "1" | "on" => "true","#,
        r#"=> "false","#,
    ] {
        assert!(
            cell.contains(needle),
            "the normalizer must accept `{needle}`:\n{cell}"
        );
    }
    // A blank cell means `false` for a NON-nullable column (that is what an
    // unchecked checkbox means) and stays blank for a nullable one, where
    // `decode_form` strips it to `None`.
    assert!(
        cell.contains("return if *blank_is_false { \"false\" } else { value };"),
        "a blank cell must respect the column's nullability:\n{cell}"
    );
    // An unrecognised value is NOT coerced — it fails with the form's own
    // message, which names the column.
    assert!(
        cell.contains("_ => value,"),
        "an unrecognised value must pass through untouched:\n{cell}"
    );
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("csv_bool_cell(key, csv_unguard_cell(key, value))"),
        "the row decoder must apply both cell rules:\n{import}"
    );

    // A nullable bool keeps its blank.
    let (_tmp2, project, _) = scaffold_project(
        "import-bools-null",
        &["title:String", "published:Option<bool>"],
        &["--import"],
    );
    let nullable = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        nullable.contains(r#"&[("published", false)]"#),
        "a nullable bool's blank cell is NULL, not false:\n{nullable}"
    );
}

/// The export writes columns the form does not carry — `id` and `created_at`
/// always, plus anything dropped from the form. They are ignored on the way in,
/// so the upload page has to NAME them: otherwise an operator edits one in a
/// spreadsheet, re-uploads, and watches nothing happen.
#[test]
fn the_upload_page_names_the_columns_it_cannot_set() {
    let (_tmp, project, _) = scaffold_project(
        "import-ignored",
        &["title:String", "tag:String", "--default", "tag=general"],
        &["--import"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    let form = fn_slice(&routes, "import_form_body");
    assert!(
        routes.contains(r#"const CSV_IGNORED_COLUMNS: &[&str] = &["id", "tag", "created_at"];"#),
        "the columns the import cannot set must be listed:\n{routes}"
    );
    assert!(
        form.contains(r#"code { (CSV_IGNORED_COLUMNS.join(", ")) }"#),
        "the ignored columns must be named on the upload page:\n{form}"
    );
    // ...and the expected header row is copy-pasteable: a bare comma, because
    // the CSV parser does not trim and `", "` would make every column miss.
    assert!(
        form.contains(r#"csv_columns().join(",")"#),
        "the expected header row must be copy-pasteable:\n{form}"
    );
}

/// An at-rest `#[encrypted]` column is omitted from the export (#1340) but is a
/// REQUIRED field on the generated form, so a file whose header is
/// `csv_columns()` can never satisfy it: every row would fail with "missing
/// field". Refuse the surface rather than ship one that cannot work.
#[test]
fn an_encrypted_column_refuses_the_import_and_says_why() {
    let (_tmp, project, output) = scaffold_project(
        "import-encrypted",
        &["title:String", "api_token:String{encrypted}"],
        &["--import"],
    );
    assert_no_import_anywhere(&project);
    assert!(
        output.contains("api_token") && output.contains("#[encrypted]"),
        "the warning must name the column and the reason:\n{output}"
    );
}

/// A write that fails for a reason `save_many_skip_invalid` cannot isolate (a
/// timeout, a dropped connection) leaves earlier chunks COMMITTED. A bare 500
/// would tell the operator nothing about what landed, and their only move —
/// re-uploading — would duplicate it, because the import is insert-only.
#[test]
fn a_failed_write_reports_what_may_already_be_committed() {
    let (_tmp, routes) = import_routes("import-writefail", &[]);
    let import = handler_slice(&routes, "import");
    assert!(
        !import.contains("save_many_skip_invalid(&pending_rows).await?"),
        "the write must not `?` away a partial commit:\n{import}"
    );
    assert!(
        import.contains("write_failure = Some(err.to_string());"),
        "a failed write must be carried into the report:\n{import}"
    );
    let view = fn_slice(&routes, "import_report_view");
    assert!(
        view.contains("@if let Some(failure) = write_failure {"),
        "the report must lead with the write failure:\n{view}"
    );
    // The rows-read count must survive an aborted write. Zeroing `inserted`
    // there would collapse `total_rows()` — the file's own size — to the error
    // count, so a 10 000-row upload would report "Rows read: 0".
    let (_, after_err) = import
        .split_once("write_failure = Some(err.to_string());")
        .expect("the aborted-write branch");
    assert!(
        !after_err.contains("report.inserted = 0"),
        "an aborted write must not zero the parse-pass count:\n{import}"
    );
    assert_eq!(
        import
            .matches("report.inserted = saved.len() as u64;")
            .count(),
        1,
        "the inserted count is set only where the write actually returned:\n{import}"
    );
    // ...and the count's label stops claiming "inserted" once the write aborted.
    assert!(
        view.contains("@if committed && write_failure.is_none() {"),
        "an aborted write must not label its count as inserted:\n{view}"
    );
}

/// The row cap bounds what a 2 MiB upload can make the server DO with it — a
/// file of six-byte rows is ~350 000 of them, each costing a decode, a
/// validation pass and (when it fails) a rendered table row.
#[test]
fn the_import_is_capped_by_rows_as_well_as_by_bytes() {
    let (_tmp, routes) = import_routes("import-rowcap", &[]);
    assert!(
        routes.contains("const MAX_IMPORT_ROWS: u64 = 10_000;"),
        "the row count must be capped, not just the byte count:\n{routes}"
    );
    assert!(
        routes.contains("const MAX_REPORT_ERRORS: usize = 200;"),
        "the rendered error list must be bounded:\n{routes}"
    );
    let import = handler_slice(&routes, "import");
    // The cap must bind BEFORE `import_csv` runs. A malformed row never reaches
    // the row handler — `import_csv` records it and moves on — so a counter
    // inside the closure would miss a file of nothing but malformed rows, which
    // is exactly the file that costs the most to accumulate.
    let (before_import, _) = import
        .split_once("autumn_web::data::csv::import_csv(")
        .expect("the handler must drive the import engine");
    assert!(
        before_import.contains("count_data_rows(&uploaded[..]) > MAX_IMPORT_ROWS {"),
        "the row cap must be checked before the file is imported:\n{import}"
    );
    // ...and an over-cap file is REFUSED whole, never imported as a prefix.
    let (_, after_check) = before_import
        .split_once("count_data_rows(&uploaded[..]) > MAX_IMPORT_ROWS {")
        .expect("the cap check");
    assert!(
        after_check.contains("UNPROCESSABLE_ENTITY"),
        "an over-cap file must be refused, not partially imported:\n{import}"
    );
    let view = fn_slice(&routes, "import_report_view");
    assert!(
        view.contains("report.errors.iter().take(MAX_REPORT_ERRORS)"),
        "the error listing must be bounded:\n{view}"
    );
}

/// Two things the counts alone cannot say, both raised by review:
///
/// * a row reported as FAILED may still be in the database — `after_create`
///   hooks run once the insert has committed, and `save_many_skip_invalid`
///   returns such a row's index among the failures with no way to tell it from
///   a row that never landed;
/// * a column the form does not carry is silently dropped, so an operator can
///   edit it in a spreadsheet and watch nothing happen.
#[test]
fn the_report_says_what_the_counts_cannot() {
    let (_tmp, routes) = import_routes("import-caveats", &[]);
    let view = fn_slice(&routes, "import_report_view");
    assert!(
        view.contains("@if write_failures > 0 {") && view.contains("after-create hook"),
        "a database-stage failure must carry the after-commit-hook caveat:\n{view}"
    );

    // The discarded-column alert is emitted only where a column can actually be
    // discarded — `id`/`created_at` are in every exported file and warning about
    // them would fire on every ordinary round trip.
    assert!(
        !routes.contains("CSV_DISCARDED_COLUMNS"),
        "a model with no droppable column needs no discarded-column alert:\n{routes}"
    );

    let (_tmp2, project, _) = scaffold_project(
        "import-discarded",
        &["title:String", "tag:String", "--default", "tag=general"],
        &["--import"],
    );
    let defaulted = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        defaulted.contains(r#"const CSV_DISCARDED_COLUMNS: &[&str] = &["tag"];"#),
        "a --default'ed column is one the import silently cannot set:\n{defaulted}"
    );
    let import = handler_slice(&defaulted, "import");
    assert!(
        import.contains("discarded_seen = CSV_DISCARDED_COLUMNS.iter().any(|column| {"),
        "the parse pass must notice a value for a column it will drop:\n{import}"
    );
    let defaulted_view = fn_slice(&defaulted, "import_report_view");
    assert!(
        defaulted_view.contains("@if discarded_seen {"),
        "the report must say so when a supplied value was dropped:\n{defaulted_view}"
    );
}

/// "Unconfirmed means dry run" has to hold for EVERY upload, not just the
/// first. Every page this handler renders — the preview, the committed result,
/// and the 422 refusal — comes back with the confirmation box unchecked, because
/// the operator's next move after any of them may be to choose a DIFFERENT file,
/// and a carried-over tick would commit one nobody has previewed.
#[test]
fn no_page_arms_a_commit_for_the_next_file_chosen() {
    let (_tmp, routes) = import_routes("import-rearm", &[]);
    let import = handler_slice(&routes, "import");
    // Four renders: the file-type/empty refusal, the missing-columns refusal, the
    // over-cap refusal, and the report. The count is asserted, not just the
    // ratio — a fifth render added later should fail here and be looked at,
    // which is how the missing-columns one was caught.
    assert_eq!(
        import.matches("import_form_body(").count(),
        4,
        "the handler renders the form on each refusal and on the report:\n{import}"
    );
    assert_eq!(
        import.matches("submit_field.as_ref(), false,").count(),
        4,
        "every re-render must leave the confirmation unchecked:\n{import}"
    );
    // ...and the GET form, which has no submit to carry over anyway.
    let form = handler_slice(&routes, "import_form");
    assert!(
        form.contains("submit_field.as_ref(), false, None)"),
        "the upload form starts unconfirmed:\n{form}"
    );
}

/// A file that shares no column names with the model must be REFUSED, not
/// imported as a run of blank records.
///
/// `decode_form` ignores headers it does not know and defaults fields that are
/// absent, so for a model whose every form field can be defaulted — an unchecked
/// checkbox's `bool`, an optional column — an unrelated spreadsheet decodes
/// cleanly into defaults. Row-level validation cannot catch it: each row IS
/// valid. Only the header can.
#[test]
fn a_file_missing_the_expected_columns_is_refused_whole() {
    // Every field here is defaultable, which is what makes the check load-bearing
    // rather than belt-and-braces.
    let (_tmp, project, _) = scaffold_project(
        "import-headers",
        &["published:bool", "note:Option<String>"],
        &["--import"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains(r#"const CSV_REQUIRED_COLUMNS: &[&str] = &["published", "note"];"#),
        "the form-carried columns must be named:\n{routes}"
    );
    let import = handler_slice(&routes, "import");
    // The check precedes the import, because a missing column is a property of
    // the FILE, not of its rows.
    let (before_import, _) = import
        .split_once("autumn_web::data::csv::import_csv(")
        .expect("the handler must drive the import engine");
    assert!(
        before_import.contains("read_header(&uploaded[..])")
            && before_import.contains("CSV_REQUIRED_COLUMNS"),
        "the header must be checked before any row is decoded:\n{import}"
    );
    let (_, after_check) = before_import
        .split_once("if !missing.is_empty() {")
        .expect("the missing-columns branch");
    assert!(
        after_check.contains("UNPROCESSABLE_ENTITY"),
        "a file missing expected columns must be refused:\n{import}"
    );
    // ...and the operator is told WHICH columns, not just that something is off.
    assert!(
        after_check.contains(r#"missing.join(", ")"#),
        "the refusal must name the missing columns:\n{import}"
    );
}

/// The header check and the row decoder must agree about what a column is
/// CALLED. The check compares trimmed names, so a file headed `a, b, c` — RFC
/// 4180 keeps that space, and plenty of exporters write it — gets past it. If
/// the decoder then saw the raw `" b"`, it would match no field on the form,
/// serde would default the one that key was meant to fill, and the import would
/// report success while storing `false`/`None`: the same silent-default hole the
/// header check exists to close, reopened through a narrower door.
#[test]
fn the_decoder_sees_the_same_column_names_the_header_check_accepted() {
    let (_tmp, routes) = import_routes("import-padded", &[]);
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("let key = key.trim();"),
        "the decoder's keys must be normalized like the header check's:\n{import}"
    );
    // The trimmed name must reach the cell rules too — they look their column up
    // by name (`CSV_BOOL_COLUMNS`, `CSV_TEXT_COLUMNS`), so a raw padded key would
    // skip the blank-checkbox and formula-guard handling as well.
    assert!(
        import.contains("(key, csv_bool_cell(key, csv_unguard_cell(key, value)))"),
        "the cell rules must receive the normalized key:\n{import}"
    );
    // Both sides of the agreement, asserted together so neither can drift alone.
    assert!(
        import.contains("found.trim() == *column"),
        "the header check trims, which is what makes the above necessary:\n{import}"
    );
}

/// A `Bytea` column must not be importable, because the CSV cannot carry it
/// back. The export renders it with `String::from_utf8_lossy`, so a byte that
/// is not valid UTF-8 is ALREADY a U+FFFD replacement character in the file —
/// and `into_new`'s `into_bytes()` would store those bytes, so importing this
/// app's own export would silently replace a binary column with mojibake.
#[test]
fn a_bytea_column_is_not_importable() {
    let (_tmp, project, _) = scaffold_project(
        "import-bytea",
        &["title:String", "blob:Option<Bytea>"],
        &["--import"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    // Not settable...
    assert!(
        routes.contains(r#"const CSV_REQUIRED_COLUMNS: &[&str] = &["title"];"#),
        "a Bytea column must not be one the import requires or sets:
{routes}"
    );
    // ...named on the upload page as a column the import cannot set...
    assert!(
        routes.contains(r#"const CSV_IGNORED_COLUMNS: &[&str] = &["id", "blob", "created_at"];"#),
        "a Bytea column must be named as unsettable:
{routes}"
    );
    // ...and flagged in the report when a file actually supplies a value, since
    // an operator editing that column would otherwise see nothing happen.
    assert!(
        routes.contains(r#"const CSV_DISCARDED_COLUMNS: &[&str] = &["blob"];"#),
        "a supplied Bytea value must raise the discarded-column alert:
{routes}"
    );
    // The export still writes the column — this is an import-side exclusion, not
    // a change to #1315's surface.
    // (The nullable column exports through `map(|bytes| …)`, the non-nullable
    // one through `&self.blob` — assert the parts common to both rather than
    // one shape's exact expression.)
    assert!(
        routes.contains("self.blob") && routes.contains("String::from_utf8_lossy"),
        "the export must still carry the column:
{routes}"
    );
}

/// Excluding a column from the LISTS is not enough — the decoder must never see
/// it. `{Pascal}Form` still carries a `String` field for a `Bytea` column, so a
/// pair that reaches `decode_form` is written back by `into_new` regardless of
/// what the column lists say. This asserts the exclusion where it actually
/// bites, which the list-level assertions cannot.
#[test]
fn an_unsettable_column_never_reaches_the_form_decoder() {
    let (_tmp, project, _) = scaffold_project(
        "import-bytea-decode",
        &["title:String", "blob:Option<Bytea>"],
        &["--import"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    let import = handler_slice(&routes, "import");
    assert!(
        import.contains("if CSV_IGNORED_COLUMNS.contains(&key) {")
            && import.contains("return None;"),
        "an ignored column must be filtered out before decode_form sees it:\n{import}"
    );
    // The premise the filter exists for: the form DOES still carry the column,
    // so without the filter its exported mojibake would decode and be written.
    assert!(
        routes.contains("pub blob: Option<String>,"),
        "the form still carries the Bytea column, which is why filtering matters:\n{routes}"
    );
}

/// A NON-NULLABLE `Bytea` is refused outright. The import must filter the column
/// out (it cannot round-trip), but the form declares it as a bare `String` with
/// no default — so a filtered row fails "missing field" and the importer could
/// never import anything. A nullable one is fine: the form declares
/// `Option<String>`, so a filtered column decodes as `None`.
///
/// This is the general shape the `#[encrypted]` refusal is the other instance
/// of: a column the import cannot set that the form nonetheless requires.
#[test]
fn a_required_unsettable_column_refuses_the_import() {
    let (_tmp, project, output) = scaffold_project(
        "import-bytea-required",
        &["title:String", "blob:Bytea"],
        &["--import"],
    );
    assert_no_import_anywhere(&project);
    assert!(
        output.contains("non-nullable Bytea column"),
        "the warning must name the shape:\n{output}"
    );
    assert!(
        output.contains("blob:Option<Bytea>"),
        "the warning must name the way out:\n{output}"
    );

    // CONTROL: the nullable form of the same column keeps the surface, because
    // a filtered-out `Option<String>` decodes as `None` rather than failing.
    let (_tmp2, nullable, _) = scaffold_project(
        "import-bytea-nullable",
        &["title:String", "blob:Option<Bytea>"],
        &["--import"],
    );
    let routes = fs::read_to_string(nullable.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains(r#"#[post("/posts/import")]"#),
        "a nullable Bytea column must still get the import:\n{routes}"
    );
}

/// A model with nothing the form can set must not get an importer at all: a form
/// with no settable field decodes ANY row, so an unrelated upload would preview
/// and commit as a run of blank records — the wrong-file failure in the one
/// shape where there is no header to check.
#[test]
fn a_model_with_no_settable_column_refuses_the_import() {
    let (_tmp, project, output) =
        scaffold_project("import-nosettable", &["cover:Attachment"], &["--import"]);
    // `multipart` is expected HERE: the Attachment column's own upload handlers
    // need it regardless of the import.
    assert_no_import_surface(&project, false);
    assert!(
        output.contains("no column on this model can be set from a CSV"),
        "the warning must name the reason:\n{output}"
    );
    assert!(
        output.contains("drop --import"),
        "the warning must name a way out:\n{output}"
    );
}

/// The second shape of "no settable column": every column `--default`ed, so
/// `{Pascal}Form` is an empty struct. Same refusal as the `Attachment`-only
/// model above, and for the same reason — an empty form decodes any row, so an
/// importer here could only ever commit rows of defaults.
///
/// Carries the control the refusal needs: a model that DOES have a settable
/// column still gets the full import surface, so this is not passing merely
/// because the feature is absent everywhere.
#[test]
fn an_all_defaulted_model_refuses_the_import_too() {
    let (_tmp, project, output) = scaffold_project(
        "import-noheaders",
        &["tag:String", "--default", "tag=general"],
        &["--import"],
    );
    assert_no_import_anywhere(&project);
    assert!(
        output.contains("no column on this model can be set from a CSV"),
        "the warning must name the reason:\n{output}"
    );

    // CONTROL: one settable column is enough to get the surface, header check
    // and all.
    let (_tmp2, settable, _) = scaffold_project(
        "import-noheaders-control",
        &["title:String", "tag:String", "--default", "tag=general"],
        &["--import"],
    );
    let control = fs::read_to_string(settable.join("src/routes/posts.rs")).unwrap();
    assert!(
        control.contains(r#"const CSV_REQUIRED_COLUMNS: &[&str] = &["title"];"#),
        "a model with a settable column must still emit the check:\n{control}"
    );
    assert!(
        control.contains("read_header(&uploaded[..])"),
        "...and the header read that goes with it:\n{control}"
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
    // The sibling text part is capped too: without narrowing it, a four-byte
    // checkbox value could be sent as the GLOBAL 16 MiB upload limit.
    assert!(
        import.contains("field.with_max_bytes(64).bytes_limited()"),
        "the confirmation part must be capped to its own size:\n{import}"
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
    // Dry run: 1 insertable + 1 row error ON THE RIGHT LINE, and NOTHING
    // persisted. The line number is asserted here as well as in the emitted test
    // because it is the property the CRLF fix in this same change exists for —
    // without it the generated test could stop checking and nothing would say so.
    for needle in [
        "would insert 1",
        "errors 1",
        "first error line 3",
        "assert_eq!(\n        after_commit, \"Keep me\"",
    ] {
        assert!(
            test.contains(needle),
            "the generated test must assert `{needle}`:\n{test}"
        );
    }
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

/// `update_main_rs` only ADDS entries, so a re-run that stops emitting the
/// import leaves `main.rs` mounting two handlers the fresh routes module no
/// longer defines — and the project stops compiling. `--import` is exactly the
/// kind of flag someone forgets to repeat on a `--force` regeneration, and the
/// variant gates can turn the surface off without the flag changing at all.
#[test]
fn regenerating_without_the_flag_unmounts_the_import_routes() {
    let (_tmp, project, _) = scaffold_project("import-regen", &default_cols(), &["--import"]);
    let mounted = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        mounted.contains("routes::posts::import_form,")
            && mounted.contains("routes::posts::import,"),
        "the import routes start out mounted:\n{mounted}"
    );

    // The same scaffold again, WITHOUT `--import`.
    let mut args = vec!["generate", "scaffold", "Post"];
    args.extend_from_slice(&default_cols());
    args.push("--force");
    run_autumn_ok(&project, &args);

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        !routes.contains("pub async fn import"),
        "the re-render emits no import handlers:\n{routes}"
    );
    let pruned = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !pruned.contains("routes::posts::import"),
        "main.rs must not keep mounting handlers the module no longer defines:\n{pruned}"
    );
    // ...and the prune is surgical: every other entry survives, including the
    // export, whose name is not a prefix relationship away from the import's.
    for kept in [
        "routes::posts::index,",
        "routes::posts::show,",
        "routes::posts::create,",
        "routes::posts::destroy,",
        "routes::posts::export_csv,",
    ] {
        assert!(
            pruned.contains(kept),
            "`{kept}` must survive the import prune:\n{pruned}"
        );
    }
}

/// The same hazard reached through a VARIANT gate rather than a dropped flag:
/// `--import` is still on the command line, but adding `--sharded` turns the
/// surface off, so the stale entries must go just the same.
#[test]
fn a_variant_that_gates_the_import_off_also_unmounts_it() {
    let (_tmp, project, _) = scaffold_project("import-regen-gate", &default_cols(), &["--import"]);
    let mut args = vec!["generate", "scaffold", "Post"];
    args.extend_from_slice(&default_cols());
    args.extend_from_slice(&["--import", "--sharded", "--force"]);
    run_autumn_ok(&project, &args);
    let pruned = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !pruned.contains("routes::posts::import"),
        "a gated-off variant must unmount the import too:\n{pruned}"
    );
}

// ── Gated-off variants warn instead of emitting a broken module ───────────────

/// The import shares the export's `CsvSchema`, so it is emitted exactly where
/// the export is. A variant that cannot have one must emit no import surface at
/// all — routes module, `main.rs` mount and Cargo feature alike — and must say
/// why rather than leaving the author to notice the missing link.
fn assert_no_import_anywhere(project: &Path) {
    assert_no_import_surface(project, true);
}

/// `assert_no_import_anywhere`, but `expect_no_multipart` false for a scaffold
/// that needs `multipart` for a reason of its own — an `Attachment` column's
/// upload handlers take a `Multipart` extractor whether or not an import
/// exists, so asserting the feature's absence there would be asserting the
/// wrong thing.
fn assert_no_import_surface(project: &Path, expect_no_multipart: bool) {
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap_or_default();
    assert!(!routes.contains("/posts/import"), "{routes}");
    assert!(!routes.contains("import_csv"), "{routes}");
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        !main.contains("routes::posts::import"),
        "main.rs must not mount an import the module never emitted:\n{main}"
    );
    // The Cargo feature and the generated test are two more places the surface
    // could leak; greping only the routes module would miss both.
    if expect_no_multipart {
        let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
        assert!(
            !cargo.contains("\"multipart\""),
            "no import means no multipart feature:\n{cargo}"
        );
    }
    let test = fs::read_to_string(project.join("tests/post.rs")).unwrap_or_default();
    assert!(!test.contains("csv_import"), "{test}");
}

/// The strongest form of "additive": for a variant that cannot have the import,
/// passing `--import` must change NOTHING. A grep only sees the strings it was
/// told to look for; this compares the whole generated tree.
/// A path relative to the project root, with a migration directory's 14-digit
/// generation timestamp replaced by a fixed placeholder and `/` as the
/// separator on every platform.
///
/// Two things it must survive, both learned the hard way:
///
/// * the two scaffolds this compares are separate process runs, so they straddle
///   a second boundary often enough to matter — hence the placeholder;
/// * this runs on Windows too, where `Path::display()` writes `\` — hence
///   walking COMPONENTS rather than stripping a `"migrations/"` string prefix,
///   which silently never matched there and turned the flake into a hard
///   failure on the slowest runner.
///
/// Deliberately narrow: it rewrites only a 14-digit component directly under
/// `migrations/`, so a digit run anywhere else in a path — or in a file's
/// contents — is untouched.
fn normalized_relative_path(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).expect("under root");
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.first().map(String::as_str) == Some("migrations")
        && let Some(dir) = parts.get_mut(1)
        && let Some((stamp, tail)) = dir.clone().split_once('_')
        && stamp.len() == 14
        && stamp.bytes().all(|b| b.is_ascii_digit())
    {
        *dir = format!("<timestamp>_{tail}");
    }
    parts.join("/")
}

/// Apply the same timestamp rule to `.autumn/generated.toml`'s body.
///
/// The manifest keys every file by its real relative path, so a migration
/// directory's 14-digit stamp appears again *inside* this one file — where
/// normalizing the file's own path cannot reach it. Two scaffolds one second
/// apart then differ on the key alone, with every digest identical. Also drops
/// the recorded `invocation`, which differs by construction: the two runs type
/// different commands, and that is the flag under test.
fn normalized_manifest_body(body: &str) -> String {
    body.lines()
        .filter(|line| !line.starts_with("invocation = "))
        .map(normalized_manifest_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rewrite `migrations/<14 digits>_` to `migrations/<timestamp>_` in one line.
/// Anything else — a shorter run, a non-digit, a different parent — is left
/// alone, mirroring `normalized_relative_path`.
fn normalized_manifest_line(line: &str) -> String {
    let Some((head, rest)) = line.split_once("migrations/") else {
        return line.to_owned();
    };
    let Some((stamp, tail)) = rest.split_once('_') else {
        return line.to_owned();
    };
    if stamp.len() == 14 && stamp.bytes().all(|b| b.is_ascii_digit()) {
        format!("{head}migrations/<timestamp>_{tail}")
    } else {
        line.to_owned()
    }
}

/// The normalization above is what a Windows CI run caught: it used to strip a
/// literal `"migrations/"` prefix, which never matched a `\`-separated path, so
/// the timestamp survived and a second-boundary straddle became a hard failure
/// on the slowest runner. `PathBuf` joining is separator-correct per platform,
/// so building the fixture that way exercises the real shape on each one.
#[test]
fn normalized_relative_path_is_separator_agnostic() {
    let root = PathBuf::from("root");
    let migration = root
        .join("migrations")
        .join("20260827030722_create_posts")
        .join("up.sql");
    assert_eq!(
        normalized_relative_path(&migration, &root),
        "migrations/<timestamp>_create_posts/up.sql"
    );

    // Only a 14-digit component directly under `migrations/` is rewritten.
    let other = root.join("src").join("routes").join("posts.rs");
    assert_eq!(
        normalized_relative_path(&other, &root),
        "src/routes/posts.rs"
    );

    let not_a_stamp = root.join("migrations").join("readme_notes").join("up.sql");
    assert_eq!(
        normalized_relative_path(&not_a_stamp, &root),
        "migrations/readme_notes/up.sql"
    );

    // A digit run of the right length somewhere else stays put.
    let elsewhere = root.join("data").join("20260827030722_export.csv");
    assert_eq!(
        normalized_relative_path(&elsewhere, &root),
        "data/20260827030722_export.csv"
    );
}

/// The path rule above cannot reach the stamp recorded *inside* the manifest.
/// A Windows run caught that gap the same way it caught the first one: two
/// scaffolds a second apart, identical digests, keys one second off.
#[test]
fn normalized_manifest_body_rewrites_the_stamp_in_its_keys() {
    let body = "\
[files.\"migrations/20260907025351_create_posts/up.sql\"]
digest = \"abc\"
invocation = \"autumn generate scaffold post --import\"

[files.\"src/models/post.rs\"]
digest = \"def\"";
    assert_eq!(
        normalized_manifest_body(body),
        "\
[files.\"migrations/<timestamp>_create_posts/up.sql\"]
digest = \"abc\"

[files.\"src/models/post.rs\"]
digest = \"def\""
    );

    // Two runs a second apart must normalize to the same text.
    let earlier = "[files.\"migrations/20260907025351_create_posts/up.sql\"]";
    let later = "[files.\"migrations/20260907025352_create_posts/up.sql\"]";
    assert_eq!(
        normalized_manifest_body(earlier),
        normalized_manifest_body(later)
    );

    // A digest that merely looks stamp-like is untouched.
    let digest = "digest = \"20260907025351_not_a_path\"";
    assert_eq!(normalized_manifest_body(digest), digest);
}

fn assert_import_flag_changes_nothing(name: &str, extra: &[&str]) {
    let mut flags = vec!["--import"];
    flags.extend_from_slice(extra);
    // The SAME project name for both, in their own tempdirs: `autumn new` stamps
    // the name into `.env.example`, `Cargo.toml` and the README, so two names
    // would differ for reasons that have nothing to do with the flag.
    let with_flag = scaffold_project(name, &default_cols(), &flags);
    let without = scaffold_project(name, &default_cols(), extra);
    let files = |root: &Path| -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("readable dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(body) = fs::read_to_string(&path) {
                    // The migration directory embeds a generation timestamp to
                    // the SECOND (`migrations/20260827030722_create_posts/`), and
                    // the two scaffolds this compares are two separate process
                    // runs — so they straddle a second boundary often enough to
                    // matter, and the raw paths would then differ for a reason
                    // that has nothing to do with the flag under test. Normalize
                    // the timestamp away; the rest of the path (and every file
                    // body) is still compared exactly.
                    let rel = normalized_relative_path(&path, root);
                    // Freshly generated secrets differ by construction; every
                    // other file is the generator's own output.
                    if rel.contains("master.key") || rel.contains("credentials.enc") {
                        continue;
                    }
                    // `.autumn/generated.toml` records the digest of each file
                    // AND the command that wrote it (issue #1835). The two runs
                    // type different commands by construction — that is the
                    // flag under test — so the recorded command differs while
                    // the digests, which describe the output this gate is
                    // about, must not. Compare the digests, drop the command.
                    // Its keys carry the migration timestamp too, which the
                    // path normalization above cannot reach.
                    let body = if rel.ends_with(".autumn/generated.toml") {
                        normalized_manifest_body(&body)
                    } else {
                        body
                    };
                    out.push((rel, body));
                }
            }
        }
        out.sort();
        out
    };
    let a = files(&with_flag.1);
    let b = files(&without.1);
    assert_eq!(
        a.len(),
        b.len(),
        "--import must not add or remove files for a variant that cannot honour it"
    );
    for ((name_a, body_a), (name_b, body_b)) in a.iter().zip(b.iter()) {
        assert_eq!(name_a, name_b, "same tree shape");
        assert_eq!(
            body_a, body_b,
            "--import changed {name_a} for a variant that cannot honour it"
        );
    }
}

#[test]
fn live_scaffold_omits_the_import_and_says_why() {
    let (_tmp, project, output) =
        scaffold_project("import-live", &default_cols(), &["--import", "--live"]);
    assert_no_import_anywhere(&project);
    assert!(
        output.contains("--import: no CSV import route generated"),
        "the generator must warn that --import was not honoured:\n{output}"
    );
    assert!(
        output.contains("SSE island"),
        "the warning must name the --live reason, not a neighbouring one:\n{output}"
    );
}

#[test]
fn sharded_scaffold_omits_the_import() {
    let (_tmp, project, output) = scaffold_project(
        "import-sharded",
        &default_cols(),
        &["--import", "--sharded"],
    );
    assert_no_import_anywhere(&project);
    assert!(
        output.contains("sharded repository pins every write"),
        "the warning must name the sharding reason:\n{output}"
    );
}

#[test]
fn a_gated_off_variant_is_byte_identical_with_and_without_the_flag() {
    assert_import_flag_changes_nothing("import-id-live", &["--live"]);
    assert_import_flag_changes_nothing("import-id-sharded", &["--sharded"]);
    assert_import_flag_changes_nothing("import-id-api", &["--api"]);
}

#[test]
fn api_scaffold_omits_the_import() {
    let (_tmp, project, _) =
        scaffold_project("import-api", &default_cols(), &["--import", "--api"]);
    assert!(!project.join("src/routes/posts.rs").exists());
    assert_no_import_anywhere(&project);
}

/// `destroy scaffold` takes the `multipart` feature back out — unless something
/// else still needs it. The `csv` feature has the same pair of tests in
/// `scaffold_csv_export.rs`; this is the import's half.
#[test]
fn destroy_takes_the_multipart_feature_back_out() {
    let (_tmp, project, _) = scaffold_project("import-destroy-mp", &default_cols(), &["--import"]);
    run_autumn_ok(&project, &["destroy", "scaffold", "Post", "--force"]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        !cargo.contains("\"multipart\""),
        "the only multipart user is gone, so the feature must go too:\n{cargo}"
    );
}

#[test]
fn destroy_keeps_the_multipart_feature_while_hand_written_code_uses_it() {
    let (_tmp, project, _) = scaffold_project("import-destroy-mp2", &default_cols(), &["--import"]);
    fs::write(
        project.join("src/uploads.rs"),
        "use autumn_web::extract::Multipart;\npub async fn handle(_form: Multipart) {}\n",
    )
    .unwrap();
    run_autumn_ok(&project, &["destroy", "scaffold", "Post", "--force"]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"multipart\""),
        "a hand-written Multipart route still needs the feature:\n{cargo}"
    );
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
