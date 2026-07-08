//! Scaffold view emission via `form_for` (issue #1135, phase 3).
//!
//! The generated create/edit views must render their whole form through a
//! single shared `form_for` call (one `<resource>_form_for` helper), with the
//! per-field controls derived from the `#[model]`-derived `FormModel`
//! descriptors — no hand-emitted per-field `*_input(...)` lines. Enum and
//! decimal columns get `.override_field(...)` escape hatches (the derive
//! can't know enum variants, and maps decimals to a free `step="any"` rather
//! than the column's declared scale).

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

/// `autumn new` + `autumn generate scaffold` in a fresh tempdir, returning
/// the tempdir guard and the project root.
fn scaffold_project(
    project_name: &str,
    resource: &str,
    columns: &[&str],
) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn(tmp.path(), &["new", project_name]);
    let project = tmp.path().join(project_name);
    let mut args = vec!["generate", "scaffold", resource];
    args.extend_from_slice(columns);
    run_autumn(&project, &args);
    (tmp, project)
}

/// A representative column mix: string, text, integer, decimal, boolean,
/// date-valued (datetime — the field DSL has no bare-date kind), tz datetime,
/// enum, a foreign-key reference, and an optional (nullable) column.
const WIDGET_COLUMNS: &[&str] = &[
    "name:String",
    "description:Text",
    "quantity:i32",
    "price:decimal{10,2}",
    "active:bool",
    "published_on:NaiveDateTime",
    "released_at:DateTime",
    "status:enum{draft,live,retired}",
    "post:references",
    "notes:Option<String>",
];

/// `autumn new` + `generate model Post` + `generate scaffold Widget`: the
/// `post:references` column's select promotion requires the referenced
/// model (and its `src/schema.rs` entry) to exist at scaffold time —
/// otherwise the column falls back to a plain numeric id input (see
/// `scaffold_references_column_with_missing_target_falls_back_to_number_input`).
fn scaffold_widget_project(project_name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn(tmp.path(), &["new", project_name]);
    let project = tmp.path().join(project_name);
    run_autumn(&project, &["generate", "model", "Post", "title:String"]);
    let mut args = vec!["generate", "scaffold", "Widget"];
    args.extend_from_slice(WIDGET_COLUMNS);
    run_autumn(&project, &args);
    (tmp, project)
}

fn scaffold_widget() -> (tempfile::TempDir, String) {
    let (tmp, project) = scaffold_widget_project("form-for-app");
    let routes = fs::read_to_string(project.join("src/routes/widgets.rs")).unwrap();
    (tmp, routes)
}

/// Slice `routes` from the start of one handler to the start of the next.
fn handler_body<'a>(routes: &'a str, from: &str, to: &str) -> &'a str {
    let start = routes
        .find(from)
        .unwrap_or_else(|| panic!("missing {from} in:\n{routes}"));
    let end = routes[start..]
        .find(to)
        .unwrap_or_else(|| panic!("missing {to} after {from} in:\n{routes}"));
    &routes[start..start + end]
}

/// The changeset-aware per-field helpers the pre-#1135 generator emitted one
/// call per column of. None of them may appear in the generated views any
/// more — `form_for` dispatches to them internally.
const PER_FIELD_HELPERS: &[&str] = &[
    "text_input",
    "required_text_input",
    "textarea_input",
    "number_input",
    "required_number_input",
    "checkbox_input",
    "date_input",
    "required_date_input",
    "datetime_input",
    "required_datetime_input",
    "select_input",
    "required_select_input",
];

#[test]
fn scaffold_create_and_edit_views_use_single_form_for_call() {
    let (_tmp, routes) = scaffold_widget();

    // Exactly one real `form_for` builder call in the whole file — inside the
    // shared `widget_form_for` helper both views (and the 422 re-render
    // branches) call.
    assert_eq!(
        routes.matches("autumn_web::form::form_for(").count(),
        1,
        "expected exactly one form_for builder call:\n{routes}"
    );

    // Each view body renders through exactly one call of the shared helper.
    let new_form = handler_body(&routes, "pub async fn new_form", "pub async fn create");
    assert_eq!(
        new_form.matches("widget_form_for(&changeset").count(),
        1,
        "new view must contain exactly one form_for call:\n{new_form}"
    );
    let edit_form = handler_body(&routes, "pub async fn edit_form", "pub async fn update");
    assert_eq!(
        edit_form.matches("widget_form_for(&changeset").count(),
        1,
        "edit view must contain exactly one form_for call:\n{edit_form}"
    );

    // No per-field input helper lines remain anywhere in the generated file.
    for helper in PER_FIELD_HELPERS {
        let line = format!("autumn_web::form::{helper}(&changeset");
        assert!(
            !routes.contains(&line),
            "generated views must not hand-emit {helper}:\n{routes}"
        );
    }

    // The controls come from the `#[model]`-derived FormModel descriptors,
    // delegated to from the scaffold's form struct.
    assert!(
        routes.contains("impl autumn_web::form::FormModel for WidgetForm"),
        "WidgetForm must implement FormModel:\n{routes}"
    );
    assert!(
        routes.contains("<Widget as autumn_web::form::FormModel>::form_fields()"),
        "WidgetForm must delegate its form fields to the derived Widget impl:\n{routes}"
    );
}

#[test]
fn scaffold_enum_column_overrides_to_select() {
    let (_tmp, routes) = scaffold_widget();

    // The derive maps unknown types (the generated `Status` enum) to a text
    // control; the scaffold knows the variants statically and must promote
    // the field to a select with a placeholder plus one option per variant.
    assert!(
        routes.contains(
            ".override_field(\"status\", autumn_web::form::FieldControl::Select { options: vec![(\"\".into(), \"— Select —\".into()), (\"draft\".into(), \"Draft\".into()), (\"live\".into(), \"Live\".into()), (\"retired\".into(), \"Retired\".into())] })"
        ),
        "enum column must get a Select override with its variants:\n{routes}"
    );
}

#[test]
fn scaffold_references_column_overrides_to_select_and_loads_options() {
    let (_tmp, routes) = scaffold_widget();

    // Issue #1135 AC 2: "references→select". The derive maps the `i64` FK
    // column to a number input; the scaffold promotes it to a select whose
    // options are the referenced table's ids, loaded at render time by the
    // controller and threaded through the shared helper.
    assert!(
        routes.contains(
            ".override_field(\"post_id\", autumn_web::form::FieldControl::Select { options: post_id_select })"
        ),
        "references column must be promoted to a Select: {routes}"
    );
    assert!(
        routes.contains("post_id_options: &[(String, String)],"),
        "the form helper must take the loaded options: {routes}"
    );
    assert!(
        routes.contains(
            "async fn post_id_select_options(db: &mut Db) -> AutumnResult<Vec<(String, String)>>"
        ),
        "the controller must have an options loader: {routes}"
    );
    assert!(
        routes.contains(".select(posts::id)"),
        "the loader must query the referenced table's ids: {routes}"
    );
    // Every form-rendering site loads the options: the new_form and edit_form
    // GET handlers plus the create/update 422 re-render branches.
    assert_eq!(
        routes
            .matches("let post_id_options = post_id_select_options(&mut db).await?;")
            .count(),
        4,
        "all four form-rendering sites must load the reference options: {routes}"
    );
}

#[test]
fn scaffold_decimal_column_overrides_number_step() {
    let (_tmp, routes) = scaffold_widget();

    // The derive maps `rust_decimal::Decimal` to `Number {{ step: "any" }}`;
    // the scaffold knows the declared scale and must pin the browser step to
    // the column's smallest representable increment (`decimal{10,2}` → 0.01).
    assert!(
        routes.contains(
            ".override_field(\"price\", autumn_web::form::FieldControl::Number { step: Some(\"0.01\".into()) })"
        ),
        "decimal column must get a scale-derived Number step override:\n{routes}"
    );
}

/// Slow end-to-end check: the `form_for`-rendered scaffold (delegated
/// `FormModel` impl, shared `widget_form_for` helper, enum select override,
/// decimal step override, references select with loaded options) actually
/// type-checks against the real framework.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_form_for_scaffold_cargo_checks() {
    // The `post:references` column's select options are loaded from the
    // referenced table, so its schema entry must exist at scaffold time —
    // `scaffold_widget_project` generates the `Post` model the reference
    // points at before scaffolding (same ordering the FOREIGN KEY imposes at
    // the database level). A missing target downgrades the column to a
    // numeric id input instead (covered by the sibling ignored test below).
    let (_tmp, project) = scaffold_widget_project("form-for-check");
    assert_project_cargo_checks(&project);
}

/// Patch the generated project to use this workspace's `autumn-web`, then
/// `cargo check --tests` it, asserting success.
fn assert_project_cargo_checks(project: &Path) {
    // Point the generated project at the local autumn-web crate so the check
    // exercises this workspace's `form_for`, not a published version.
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
        .args(["check", "--tests"])
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

#[test]
fn scaffold_references_column_with_missing_target_falls_back_to_number_input() {
    // Regression test (issue #1135 review): a `references` column whose
    // target model hasn't been generated has always been a warning, not an
    // error — the scaffold assumes the table exists out-of-band and its
    // output must still compile. Without the `Post` model there is no
    // `posts` entry in `src/schema.rs`, so the scaffold must skip the whole
    // select pipeline for `post_id` (schema import, options loader, Select
    // override) and keep the derived numeric id input.
    let (_tmp, project) = scaffold_project("missing-target-app", "Widget", WIDGET_COLUMNS);
    let routes = fs::read_to_string(project.join("src/routes/widgets.rs")).unwrap();

    assert!(
        routes.contains("use crate::schema::widgets;"),
        "only the resource's own schema module may be imported: {routes}"
    );
    assert!(
        !routes.contains("posts::") && !routes.contains("post_id_select_options"),
        "the missing target must not produce select machinery: {routes}"
    );
    assert!(
        !routes.contains(".override_field(\"post_id\""),
        "the column must keep the derived number input, not a Select: {routes}"
    );
    // The other schema-specific overrides are unaffected by the fallback.
    assert!(
        routes.contains(".override_field(\"status\""),
        "the enum Select override must survive the reference fallback: {routes}"
    );
}

/// Slow end-to-end sibling of `generated_form_for_scaffold_cargo_checks`:
/// the warning-only missing-reference-target path must still produce a
/// project that type-checks (the review regression this guards against was
/// an unconditional `use crate::schema::{widgets, posts};` import plus a
/// `posts::table` option loader with no `posts` schema entry to satisfy
/// them).
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_scaffold_with_missing_reference_target_cargo_checks() {
    let (_tmp, project) = scaffold_project("missing-target-check", "Widget", WIDGET_COLUMNS);
    assert_project_cargo_checks(&project);
}

#[test]
fn rescaffold_with_added_column_leaves_view_form_call_unchanged() {
    // Issue #1135 success metric: adding a column requires zero view edits.
    // Scaffold the same resource twice — once with an extra column — and the
    // view bodies plus the shared form helper must be byte-identical: every
    // schema-derived difference lives in the form struct / conversions, not
    // in the views.
    let base = &["name:String", "quantity:i32"];
    let extended = &["name:String", "quantity:i32", "notes:Option<String>"];
    let (_tmp_a, project_a) = scaffold_project("regen-a", "Gadget", base);
    let (_tmp_b, project_b) = scaffold_project("regen-b", "Gadget", extended);
    let routes_a = fs::read_to_string(project_a.join("src/routes/gadgets.rs")).unwrap();
    let routes_b = fs::read_to_string(project_b.join("src/routes/gadgets.rs")).unwrap();

    for (from, to) in [
        ("pub async fn new_form", "pub async fn create"),
        ("pub async fn edit_form", "pub async fn update"),
        ("fn gadget_form_for(", "form.render()"),
    ] {
        let body_a = handler_body(&routes_a, from, to);
        let body_b = handler_body(&routes_b, from, to);
        assert_eq!(
            body_a, body_b,
            "the {from} view section must not change when a plain column is added"
        );
    }
}
