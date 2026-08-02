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
    // options are the referenced table's rows, loaded at render time by the
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
    // Issue #1146: `Post` has a `title:String` column, so the loader selects
    // the id + the display column (a `name`/`title` heuristic), labeling each
    // option by the parent's title rather than its raw id.
    assert!(
        routes.contains(".select((posts::id, posts::title))"),
        "the loader must query the referenced table's id + display column: {routes}"
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

// ── Issue #1236: zero-JS multipart file uploads ─────────────────────────

/// A scaffold with a file-attachment column emits progressive-enhancement
/// multipart upload handlers: a `multipart/form-data` form (via
/// `FieldControl::File`), a `Multipart` body streamed straight to the blob
/// store with `save_to_blob_store`, no hidden presign key, and no
/// JavaScript/presign dead-end in the default path.
#[test]
fn scaffold_attachment_emits_zero_js_multipart_handlers() {
    let (_tmp, project) = scaffold_project(
        "attach-app",
        "Photo",
        &["caption:String", "image:Attachment"],
    );
    let routes = fs::read_to_string(project.join("src/routes/photos.rs")).unwrap();

    // Attachment promoted to FieldControl::File (auto multipart enctype + plain
    // file input, no `.exclude` + hand-rolled append, no hidden key input).
    assert!(
        routes.contains(".override_field(\"image\", autumn_web::form::FieldControl::File)"),
        "{routes}"
    );
    assert!(!routes.contains(".exclude(\"image\")"), "{routes}");
    assert!(
        !routes.contains("input type=\"hidden\" name=\"image\""),
        "{routes}"
    );

    // Handlers take a Multipart body and stream to the blob store.
    assert!(
        routes.contains("mut multipart: autumn_web::extract::Multipart"),
        "{routes}"
    );
    // Issue #1872 (codex P2, centralized): the whole multipart-parse-and-decode
    // span runs inside one `async {…}.await` block whose `Err` is funneled through
    // a single cleanup path, so each save is a plain `?` (its failure — and every
    // other `?` in the span — is covered centrally, no per-line `match` wrapper).
    assert!(
        routes.contains(".save_to_blob_store(&*store, key).await?;"),
        "{routes}"
    );
    assert!(
        routes.contains("duplicate attachment field: image"),
        "duplicate file parts must be rejected before a staged blob is overwritten: {routes}"
    );
    assert!(routes.contains("multipart.next_field().await?"), "{routes}");
    // The parse span is wrapped in a single fallible block funneling every `?`
    // (next_field, save, bytes_limited, utf8, decode_form) to one cleanup path.
    assert!(
        routes.contains("let __parse_result: AutumnResult<PhotoForm> = async {"),
        "the multipart parse span must run inside a single cleanup-on-error block:\n{routes}"
    );
    // Default path does not CALL the presign helper (the header note may still
    // mention it as an advanced opt-in), so match the invocation.
    assert!(!routes.contains("complete_direct_upload(&"), "{routes}");

    // Preserve-on-update and create-binding.
    assert!(
        routes.contains("new.image = image_blob.or(current.image);"),
        "{routes}"
    );
    assert!(routes.contains("new.image = image_blob;"), "{routes}");

    // Reloaded index/show views render non-sensitive stored metadata rather
    // than reducing every persisted attachment to the same generic label or
    // exposing the backend object key (AC3).
    assert!(
        routes.contains("blob.content_type, blob.byte_size"),
        "{routes}"
    );
    assert!(
        routes.contains("CSRF scans only a bounded prefix")
            && routes.contains("oversized files retain their 413"),
        "the generated note must describe the streaming CSRF/upload-limit behavior: {routes}"
    );

    // storage + multipart auto-enabled on autumn-web (AC7).
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("storage"), "{cargo}");
    assert!(cargo.contains("multipart"), "{cargo}");
}

/// AC6: a scaffold with an attachment column emits a generated write-path test
/// that performs a zero-JS multipart create and asserts the blob key is bound
/// on the persisted record. Follows the #1127 in-process style (no database,
/// so it runs green without Docker): the emitted test drives the real
/// `Multipart` extractor + `save_to_blob_store` against a real `LocalBlobStore`
/// and asserts the bound blob key is non-empty. This is a string-shape check;
/// `generated_attachment_scaffold_multipart_test_passes` actually runs it.
#[test]
fn scaffold_attachment_emits_generated_multipart_write_path_test() {
    let (_tmp, project) = scaffold_project(
        "attach-wp-app",
        "Photo",
        &["caption:String", "image:Attachment"],
    );
    // The generated write-path test lives in the resource's `tests/<snake>.rs`.
    let test_src = fs::read_to_string(project.join("tests/photo.rs")).unwrap();

    // A dedicated, non-ignored multipart write-path test exists (the
    // `#[tokio::test]` sits directly on the fn, with no `#[ignore]` between
    // them, so it runs green without Docker).
    assert!(
        test_src.contains("#[tokio::test]\nasync fn photos_multipart_upload_binds_blob_key() {"),
        "multipart write-path test must be a runnable, non-ignored tokio test: {test_src}"
    );

    // It builds a real blob store, drives a real multipart create through the
    // real `save_to_blob_store` path, and asserts the blob key is bound.
    assert!(test_src.contains("LocalBlobStore::new("), "{test_src}");
    assert!(
        test_src.contains("BlobStoreState::new(blob_store.clone())"),
        "{test_src}"
    );
    assert!(
        test_src.contains("multipart/form-data; boundary=BOUND"),
        "{test_src}"
    );
    assert!(
        test_src.contains(".save_to_blob_store(&*store, key).await?"),
        "{test_src}"
    );
    assert!(
        test_src.contains("the attachment column must bind a non-empty blob key"),
        "{test_src}"
    );
    // The uploaded bytes are read back out of the store at the bound key.
    assert!(
        test_src.contains("store_handle") && test_src.contains(".get(&bound.key)"),
        "{test_src}"
    );

    // A plain (non-attachment) scaffold must NOT get the multipart write-path
    // test — the #1127 in-memory CRUD test is unchanged there.
    let (_tmp2, plain) = scaffold_project("plain-wp-app", "Article", &["title:String"]);
    let plain_src = fs::read_to_string(plain.join("tests/article.rs")).unwrap();
    assert!(
        !plain_src.contains("multipart_upload_binds_blob_key"),
        "{plain_src}"
    );
    assert!(
        plain_src.contains("async fn articles_write_path_crud()"),
        "plain scaffold keeps the #1127 write-path test: {plain_src}"
    );
}

/// Slow end-to-end proof (issue #1236, AC7): a freshly scaffolded resource
/// with an attachment column must `cargo check --tests` with only the
/// auto-enabled `storage` + `multipart` features — the multipart-streaming
/// codegen has to actually compile, not just look right as a string.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_attachment_scaffold_cargo_checks() {
    let (_tmp, project) = scaffold_project(
        "attach-check",
        "Photo",
        &["caption:String", "image:Attachment"],
    );
    assert_project_cargo_checks(&project);
}

/// Issue #1872 (codex P2, centralized): the entire fallible parse span —
/// `next_field()?`, each `save_to_blob_store()?`, `bytes_limited()?`, the UTF-8
/// `map_err(…)?`, and `decode_form(…)?` — runs inside one `async {…}.await`
/// block whose `Err` is funneled through a single cleanup path. So each save and
/// the decode are plain `?` (no per-line `match`), and a malformed boundary /
/// oversized text field / invalid UTF-8 arriving *after* a save is cleaned up
/// too, not just the save and decode failures.
fn assert_parse_span_centralized(routes: &str) {
    assert!(
        routes.contains("let __parse_result: AutumnResult<PhotoForm> = async {"),
        "the parse span must run inside a single cleanup-on-error block:\n{routes}"
    );
    assert!(
        routes.contains("let blob = field.save_to_blob_store(&*store, key).await?;"),
        "the multipart save is a plain `?` covered by the central cleanup block:\n{routes}"
    );
    assert!(
        routes.contains("let form = decode_form(Bytes::from(encoded))?;"),
        "decode_form is a plain `?` covered by the central cleanup block:\n{routes}"
    );
    assert!(
        routes.contains("while let Some(field) = multipart.next_field().await? {"),
        "next_field must be a `?` inside the covered parse block:\n{routes}"
    );
    assert!(
        routes.contains("let value = String::from_utf8(field.bytes_limited().await?)"),
        "bytes_limited must be a `?` inside the covered parse block:\n{routes}"
    );
    // The block's single `Err` arm runs the cleanup then re-returns the exact
    // error the `?` produced, preserving the original error->response mapping.
    assert!(
        routes.contains("let form = match __parse_result {") && routes.contains("Err(err) => {"),
        "the parse block funnels every `?` failure through one cleanup+return:\n{routes}"
    );
}

/// Issue #1872: the create/update handlers stream each uploaded file to the
/// blob store *during* multipart parsing — before changeset validation and the
/// DB write. Every early return after that save (invalid changeset,
/// unique-violation 422, `into_new` parse failure, generic DB `Err`, the
/// update's row-load and not-found guard) must best-effort delete the
/// just-saved blob so an ordinary invalid submission doesn't orphan its upload.
/// The success path must *not* delete, and a preserved existing blob
/// (`current.<field>`) is never touched — only the freshly-saved
/// `<field>_blob`, recorded via `saved_blob_keys`, is cleaned up.
#[test]
fn scaffold_attachment_cleans_up_saved_blob_on_early_return() {
    // `email:String:unique` gives us the unique-violation 422 branch too, so all
    // four create early returns and all of the update early returns are present.
    let (_tmp, project) = scaffold_project(
        "attach-cleanup-app",
        "Photo",
        &["caption:String", "image:Attachment", "email:String:unique"],
    );
    let routes = fs::read_to_string(project.join("src/routes/photos.rs")).unwrap();

    // Only the freshly-saved blob keys are recorded for cleanup; preserved
    // existing blobs are never enrolled.
    assert!(
        routes.contains("let mut saved_blob_keys: Vec<String> = Vec::new();"),
        "{routes}"
    );
    assert!(
        routes.contains("saved_blob_keys.push(blob.key.clone());"),
        "{routes}"
    );
    // The cleanup snippet best-effort deletes each saved blob, ignoring the
    // result so a cleanup failure never masks the original error/response.
    assert!(
        routes.contains("for key in &saved_blob_keys {")
            && routes.contains("let _ = store.delete(key).await;"),
        "{routes}"
    );

    assert_parse_span_centralized(&routes);

    let create = handler_body(&routes, "pub async fn create(", "pub async fn update(");

    // `saved_blob_keys` is declared *before* the parse span (outside the `async`
    // block, so the block borrows it mutably and the post-block cleanup can read
    // it), and the push happens at save-time — both before `decode_form(`.
    let decl_at = create
        .find("let mut saved_blob_keys: Vec<String> = Vec::new();")
        .unwrap_or_else(|| panic!("missing saved_blob_keys decl in create:\n{create}"));
    let parse_at = create
        .find("let __parse_result: AutumnResult<PhotoForm> = async {")
        .unwrap_or_else(|| panic!("missing parse block in create:\n{create}"));
    assert!(
        decl_at < parse_at,
        "saved_blob_keys must be declared before the parse span in create:\n{create}"
    );
    let push_at = create
        .find("saved_blob_keys.push(blob.key.clone());")
        .unwrap_or_else(|| panic!("missing saved_blob_keys.push in create:\n{create}"));
    let decode_at = create
        .find("decode_form(")
        .unwrap_or_else(|| panic!("missing decode_form( in create:\n{create}"));
    assert!(
        push_at < decode_at,
        "saved_blob_keys must be populated before decode_form in create:\n{create}"
    );
    // Split create at the success tail: everything up to `flash.success` is the
    // early-return region, everything after is the success path.
    let create_success_at = create
        .find("flash.success(\"Photo created\")")
        .unwrap_or_else(|| panic!("missing create success in:\n{create}"));
    let (create_early, create_success) = create.split_at(create_success_at);

    // Cleanup guards every create early return that can occur after the first
    // blob is saved. Codex P2 centralized the parse span: the mid-loop save
    // failure, `next_field`, `bytes_limited`, the UTF-8 conversion, and
    // `decode_form` on malformed scalar input all funnel through ONE cleanup
    // inside the parse block — so those five formerly-separate (and two
    // previously-uncovered) returns collapse to a single `store.delete` loop. The
    // four post-parse returns keep their own cleanup: validation 422, `into_new`
    // parse failure, the unique-violation 422, and the generic DB `Err`.
    assert_eq!(
        create_early
            .matches("let _ = store.delete(key).await;")
            .count(),
        5,
        "create must clean up the saved blob on all five early-return sites \
         (one central parse-span cleanup covering save/next_field/bytes_limited/utf8/decode_form, \
         plus validation, into_new, unique-violation, DB error):\n{create}"
    );
    // The success path must NOT delete the blob it just persisted.
    assert!(
        !create_success.contains("store.delete("),
        "create success path must not delete the persisted blob:\n{create_success}"
    );

    let update = handler_body(&routes, "pub async fn update(", "pub async fn destroy(");
    let authorize_at = update
        .find("autumn_web::authorization::authorize::<Photo>")
        .unwrap_or_else(|| panic!("missing update authorization:\n{update}"));
    let save_at = update
        .find("save_to_blob_store")
        .unwrap_or_else(|| panic!("missing multipart save:\n{update}"));
    assert!(
        authorize_at < save_at,
        "record authorization must run before an attachment update stores attacker-controlled bytes:\n{update}"
    );
    let update_success_at = update
        .find("flash.success(\"Photo updated\")")
        .unwrap_or_else(|| panic!("missing update success in:\n{update}"));
    let (update_early, update_success) = update.split_at(update_success_at);
    // Cleanup guards every update early return that can occur after the first
    // blob is saved. Codex P2 centralized the parse span: the mid-loop save
    // failure, `next_field`, `bytes_limited`, the UTF-8 conversion, and
    // `decode_form` collapse to ONE cleanup inside the parse block. The five
    // post-parse returns keep their own cleanup: validation 422,
    // `into_new`, the unique-violation 422, the generic DB `Err`, and the
    // not-found (`updated == 0`) guard. Current-row load and authorization now
    // happen before parsing, when there is no staged blob to clean up.
    assert_eq!(
        update_early
            .matches("let _ = store.delete(key).await;")
            .count(),
        6,
        "update must clean up the saved blob on all six early-return sites \
         (one central parse-span cleanup, plus validation, into_new, \
         unique-violation, DB error, not-found):\n{update}"
    );
    assert!(
        !update_success.contains("store.delete("),
        "update success path must not delete the persisted blob:\n{update_success}"
    );

    // The preserve-on-update bind is untouched: only `image_blob` (the freshly
    // saved upload) is ever deleted, never the preserved `current.image`.
    assert!(
        routes.contains("new.image = image_blob.or(current.image);"),
        "{routes}"
    );

    // A plain (non-attachment) scaffold gets none of the cleanup machinery.
    let (_tmp2, plain) = scaffold_project("plain-cleanup-app", "Article", &["title:String"]);
    let plain_routes = fs::read_to_string(plain.join("src/routes/articles.rs")).unwrap();
    assert!(!plain_routes.contains("saved_blob_keys"), "{plain_routes}");
    assert!(!plain_routes.contains("store.delete("), "{plain_routes}");
}
