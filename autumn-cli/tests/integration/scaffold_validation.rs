//! Scaffold DSL constraint modifiers fan out to BOTH server-side `#[validate]`
//! rules and client-side HTML5 input constraints from one declaration
//! (issue #1388).

// DSL tokens like `"contact:String{email}"` are literal scaffold inputs, not
// format strings — the `{email}`/`{url}`/`{min=…}` is the modifier under test.
#![allow(clippy::literal_string_with_formatting_args)]

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

fn run_autumn_ok(dir: &Path, args: &[&str]) {
    let output = run_autumn(dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `autumn new` + `generate scaffold Post` with a representative constraint mix.
fn constrained_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String{min=3,max=120}",
            "contact:String{email}",
            "homepage:String{url}",
            "age:i32{min=0,max=130}",
            "bio:Option<String>{max=200}",
        ],
    );
    (tmp, project)
}

#[test]
fn model_carries_validate_attributes_from_dsl_constraints() {
    let (_tmp, project) = constrained_project("validate-model-app");
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();

    assert!(
        model.contains("#[validate(length(min = 3, max = 120))]"),
        "title must get a length rule:\n{model}"
    );
    assert!(
        model.contains("#[validate(email)]"),
        "contact must get an email rule:\n{model}"
    );
    assert!(
        model.contains("#[validate(url)]"),
        "homepage must get a url rule:\n{model}"
    );
    assert!(
        model.contains("#[validate(range(min = 0, max = 130))]"),
        "age must get a range rule:\n{model}"
    );
    assert!(
        model.contains("#[validate(length(max = 200))]"),
        "nullable bio must still get its max-length rule:\n{model}"
    );
}

/// Issue #1748: a required numeric with a `{min,max}` range that spans the
/// type's zero default (`age:i32{min=0,max=130}`) must NOT keep its native
/// `i32` on the form struct — a native field defaults to `0`, which pre-fills
/// the input and passes both `required` (the HTML attribute) and the
/// `range(0,130)` rule, so a blank submission silently becomes `0`. The fix
/// represents it as `Option<i32>` with `#[validate(required, range(...))]`:
/// the default is `None` (renders blank, forces deliberate input), `range`
/// validates the inner value only when `Some`, and `required` rejects `None`
/// server-side. The `into_new` path must unwrap the `Option`, never coerce a
/// missing value to `0`.
#[test]
fn constrained_required_numeric_spanning_zero_is_optional_not_native() {
    let (_tmp, project) = constrained_project("validate-optnum-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // The form-struct field is `Option<i32>` (blank -> None), carrying both the
    // `required` and `range` validators.
    assert!(
        routes.contains(
            "    #[validate(required)]\n    #[validate(range(min = 0, max = 130))]\n    pub age: Option<i32>,"
        ),
        "constrained required `age` must be `Option<i32>` with #[validate(required, range(...))]:\n{routes}"
    );

    // The native-typed representation (which pre-fills `0`) must be gone.
    assert!(
        !routes.contains("pub age: i32,"),
        "constrained required `age` must not keep its native `i32` type:\n{routes}"
    );

    // `into_new` must unwrap the Option and reject a missing value, never
    // silently coerce a blank submission to `0` via a native clone.
    assert!(
        routes.contains("age: form.age.ok_or_else("),
        "into_new must unwrap the Option and error on a missing value:\n{routes}"
    );
    assert!(
        !routes.contains("age: form.age.clone(),") && !routes.contains("age: form.age,"),
        "into_new must not pass the native field straight through:\n{routes}"
    );

    // The edit-form seed maps a persisted row's native value back into `Some`.
    assert!(
        routes.contains("age: Some(row.age),"),
        "from_row must wrap the persisted native value in Some:\n{routes}"
    );
}

/// Issue #1748 (Codex follow-up): the synthetic constrained-required-`Option<T>`
/// numeric (`age:i32{min=0,max=130}`) must be dropped from the request body when
/// its `field=` pair is present-but-empty, exactly like a genuinely nullable
/// field. The decoder strips empty pairs only for fields matching
/// `is_nullable_form_field`; that field is `Option<i32>` but has
/// `nullable == false`, so without including it a blank `age=` (from curl, a
/// non-browser client, or disabled JS) reaches `serde_urlencoded`, which parses
/// `""` into the inner numeric and returns a **400** *before*
/// `#[validate(required)]` can render the intended **422** inline error.
/// Dropping the empty pair makes `age` decode as `None`, so `required` fires and
/// produces the 422 (the #1748 intent). Asserts the generated
/// `is_nullable_form_field` set includes `age` alongside the nullable `bio`.
#[test]
fn constrained_required_numeric_empty_pair_is_dropped_like_nullable() {
    let (_tmp, project) = constrained_project("validate-blankdrop-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // The decoder gates its empty-pair drop on `is_nullable_form_field`.
    assert!(
        routes.contains("!(value.is_empty() && is_nullable_form_field(key))"),
        "decoder must drop empty pairs for the nullable/drop set:\n{routes}"
    );

    // The synthetic `Option<i32>` `age` must be part of that drop set so a blank
    // `age=` decodes as `None` (-> 422 via `required`) rather than being parsed
    // as `""` by serde_urlencoded (-> 400). It appears alongside nullable `bio`.
    let helper = routes
        .split_once("fn is_nullable_form_field(name: &str) -> bool {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(body, _)| body)
        .expect("generated routes must define is_nullable_form_field");
    assert!(
        helper.contains("\"age\""),
        "constrained required `age` must be in the empty-pair drop set (is_nullable_form_field):\n{helper}"
    );
    assert!(
        helper.contains("\"bio\""),
        "nullable `bio` must remain in the empty-pair drop set:\n{helper}"
    );
}

#[test]
fn cargo_toml_pulls_in_validator_dependency() {
    let (_tmp, project) = constrained_project("validate-cargo-app");
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("validator"),
        "the validator crate must be added when constraints are present:\n{cargo}"
    );
}

#[test]
fn form_inputs_carry_matching_html5_constraints() {
    let (_tmp, project) = constrained_project("validate-form-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // The constrained fields are excised from the derived render and
    // re-appended with HTML5 attributes.
    for field in ["title", "contact", "homepage", "age", "bio"] {
        assert!(
            routes.contains(&format!(".exclude(\"{field}\")")),
            "{field} must be excluded from the derived render:\n{routes}"
        );
    }

    // The constrained fields route through the typed `a11y::TextField`
    // primitive (#1706), so the HTML5 constraints are typed builder calls
    // (`.minlength(3u32)`) rather than raw `<input>` attributes; the rendered
    // element/attributes/values are unchanged, only their source spelling.
    assert!(
        routes.contains(".minlength(3u32).maxlength(120u32)"),
        "title must render minlength/maxlength:\n{routes}"
    );
    assert!(
        routes.contains(".input_type(\"email\")"),
        "contact must render type=email:\n{routes}"
    );
    assert!(
        routes.contains(".input_type(\"url\")"),
        "homepage must render type=url:\n{routes}"
    );
    assert!(
        routes.contains(".input_type(\"number\")") && routes.contains(".min(\"0\").max(\"130\")"),
        "age must render type=number with min/max:\n{routes}"
    );
    assert!(
        routes.contains(".maxlength(200u32)"),
        "bio must render maxlength:\n{routes}"
    );
}

/// A `String`/`Text` length bound above `u32::MAX` is meaningless for an HTML
/// length attribute (which fits a `u32`) and would otherwise reach the typed
/// `a11y::TextField::maxlength(u32)` codegen as an overflowing `u32` literal
/// that fails to compile the generated app. The generator rejects it up front
/// at DSL parse time with an actionable message naming the `u32::MAX` limit.
#[test]
fn length_bound_above_u32_max_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "validate-large-len-app"]);
    let project = tmp.path().join("validate-large-len-app");
    let output = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Doc",
            "title:String{max=5000000000}",
        ],
    );
    assert!(
        !output.status.success(),
        "a >u32::MAX length bound must fail the scaffold"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("4294967295"),
        "the error must name the u32::MAX limit:\n{stderr}"
    );
}

/// A length bound exactly at `u32::MAX` is the largest bound that still fits an
/// HTML length attribute, so it is accepted and emits a plain `u32` literal.
#[test]
fn length_bound_at_u32_max_is_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "validate-max-len-app"]);
    let project = tmp.path().join("validate-max-len-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Doc",
            "title:String{max=4294967295}",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/docs.rs")).unwrap();
    let title_input = slice_text_field(&routes, "title");
    assert!(
        title_input.contains(".maxlength(4294967295u32)"),
        "a bound at u32::MAX must emit a plain u32 literal:\n{title_input}"
    );
}

/// The explicit `--validate name=length:...` form renders solely to the
/// server-side `#[validate(length(...))]` derive, which the `validator` crate
/// accepts as a `u64`. Unlike the DSL inline `{max=…}` constraint, it never
/// populates `FieldConstraints`, so it never reaches the HTML5 length attribute
/// or the typed `a11y::TextField::maxlength(u32)` codegen. A bound above
/// `u32::MAX` is therefore legitimate here and must be emitted verbatim rather
/// than rejected (the `u32` guard belongs only to the DSL/HTML5 path).
#[test]
fn explicit_validate_length_bound_above_u32_max_is_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "validate-server-len-app"]);
    let project = tmp.path().join("validate-server-len-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Doc",
            "title:String",
            "--validate",
            "title=length:max=5000000000",
        ],
    );
    let model = fs::read_to_string(project.join("src/models/doc.rs")).unwrap();
    assert!(
        model.contains("#[validate(length(max = 5000000000))]"),
        "a server-side length bound above u32::MAX must be emitted verbatim:\n{model}"
    );
    // It must NOT reach the typed `u32` maxlength builder (that path is fed by
    // the DSL inline constraint, not the explicit `--validate` form).
    let routes = fs::read_to_string(project.join("src/routes/docs.rs")).unwrap();
    assert!(
        !routes.contains(".maxlength("),
        "explicit --validate length must not emit an HTML5 maxlength builder:\n{routes}"
    );
}

#[test]
fn constrained_float_fields_keep_step_any() {
    // A constrained float must keep `step="any"` so browsers don't default to
    // `step="1"` and reject fractional input like `0.5` that the server-side
    // range validator and the `f64` type accept (issue #1388 — the client and
    // server constraints must agree). Matches the unconstrained float control.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "validate-float-step-app"]);
    let project = tmp.path().join("validate-float-step-app");
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Reading", "ratio:f64{min=0,max=1}"],
    );
    let routes = fs::read_to_string(project.join("src/routes/readings.rs")).unwrap();
    // Routed through `a11y::TextField` (#1706): a `number` input built via
    // `TextField::new("ratio").input_type("number")` rather than a raw `<input>`.
    let ratio_input = slice_text_field(&routes, "ratio");
    assert!(
        ratio_input.contains(".input_type(\"number\")"),
        "the constrained float must render as a number input:\n{ratio_input}"
    );
    // Float bounds are canonicalized to valid float literals (`0` -> `0.0`), so
    // the typed builder calls carry the decimal form too.
    assert!(
        ratio_input.contains(".min(\"0.0\")")
            && ratio_input.contains(".max(\"1.0\")")
            && ratio_input.contains(".step(\"any\")"),
        "constrained float must carry min/max AND step=any:\n{ratio_input}"
    );
}

#[test]
fn required_only_on_non_nullable_constrained_fields() {
    let (_tmp, project) = constrained_project("validate-required-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // The non-nullable `title` input keeps the browser-native required signal;
    // the nullable `bio` input must NOT (leaving it blank is valid). Both route
    // through `a11y::TextField` (#1706), so `required`/`aria-required` are the
    // typed `.required().aria_required()` builder calls.
    let title_input = slice_text_field(&routes, "title");
    assert!(
        title_input.contains(".required().aria_required()"),
        "non-nullable title must be required:\n{title_input}"
    );
    let bio_input = slice_text_field(&routes, "bio");
    assert!(
        !bio_input.contains("required"),
        "nullable bio must not be required:\n{bio_input}"
    );
}

/// Slice a routed `a11y::TextField` call's field-specific builder calls: from
/// the `TextField::new("field")` marker up to the shared `.class(` skeleton, so
/// a field-scoped assertion doesn't accidentally match a sibling. The
/// input-type/value/constraint/`required` builders all precede `.class(`.
fn slice_text_field<'a>(routes: &'a str, field: &str) -> &'a str {
    let marker = format!("TextField::new(\"{field}\")");
    let start = routes
        .find(&marker)
        .unwrap_or_else(|| panic!("missing {marker} in:\n{routes}"));
    let rest = &routes[start..];
    let end = rest.find(".class(").unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn preserved_value_binding_survives_422_rerender() {
    let (_tmp, project) = constrained_project("validate-rerender-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    // The appended control re-fills its value from the changeset, so a 422
    // re-render keeps what the user typed.
    assert!(
        routes.contains(".value(changeset.field_value(\"title\").unwrap_or_default())"),
        "constrained inputs must re-fill from the changeset:\n{routes}"
    );
}

/// A `Text` DSL column is long-form (Postgres `TEXT`), so its constrained
/// control must be a multi-line `<textarea>` — not the single-line `<input>`
/// the `String` columns get — while still carrying the length/`required`
/// constraints and re-filling on a 422.
#[test]
fn text_columns_render_constrained_textarea_not_input() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "validate-textarea-app"]);
    let project = tmp.path().join("validate-textarea-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Article",
            "body:Text{min=10,max=5000}",
            "notes:Option<Text>{max=1000}",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/articles.rs")).unwrap();

    // Both text columns are excised from the derived render and re-appended.
    for field in ["body", "notes"] {
        assert!(
            routes.contains(&format!(".exclude(\"{field}\")")),
            "{field} must be excluded from the derived render:\n{routes}"
        );
        assert!(
            routes.contains(&format!("textarea id=\"{field}\" name=\"{field}\"")),
            "{field} must render a <textarea>:\n{routes}"
        );
    }

    // A `text` column must never fall back to a single-line text `<input>`,
    // and its length constraints must not be spelled with input-only `type`.
    assert!(
        !routes.contains("input type=\"text\" id=\"body\""),
        "body must not render a single-line text input:\n{routes}"
    );

    // The non-nullable `body` carries the length rules + required, and its
    // value is the element's text content, not a `value=` attribute.
    let body_attrs = slice_textarea_attrs(&routes, "textarea id=\"body\" name=\"body\"");
    assert!(
        body_attrs.contains("minlength=\"10\"") && body_attrs.contains("maxlength=\"5000\""),
        "body textarea must carry minlength/maxlength:\n{body_attrs}"
    );
    assert!(
        body_attrs.contains("required aria-required=\"true\""),
        "non-nullable body textarea must be required:\n{body_attrs}"
    );
    assert!(
        routes.contains("(changeset.field_value(\"body\").unwrap_or_default())"),
        "body textarea must re-fill from the changeset:\n{routes}"
    );
    assert!(
        !routes.contains("value=(changeset.field_value(\"body\")"),
        "body value must be textarea text content, not a value= attribute:\n{routes}"
    );

    // The nullable `notes` keeps only its maxlength — no required, no minlength.
    let notes_attrs = slice_textarea_attrs(&routes, "textarea id=\"notes\" name=\"notes\"");
    assert!(
        notes_attrs.contains("maxlength=\"1000\""),
        "notes textarea must carry maxlength:\n{notes_attrs}"
    );
    assert!(
        !notes_attrs.contains("required") && !notes_attrs.contains("minlength"),
        "nullable notes textarea must be neither required nor minlength-bound:\n{notes_attrs}"
    );
}

/// A `Text{email}` / `Text{url}` field is type-dependent and inherently
/// single-line: a `<textarea>` can't carry `type="email"`/`type="url"`, so the
/// standard `form_for` path must render an `<input type="email|url">` (not a
/// textarea) or the DSL/doc-promised client-side format validation is lost.
#[test]
fn text_email_and_url_render_typed_input_not_textarea() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "validate-text-email-app"]);
    let project = tmp.path().join("validate-text-email-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Contact",
            "email_addr:Text{email}",
            "site:Text{url}",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/contacts.rs")).unwrap();

    // A `Text{email}`/`Text{url}` field routes through `a11y::TextField`
    // (#1706): a typed single-line input via `TextField::new(...)` +
    // `.input_type("email"|"url")`, never a raw `<input>` or a `<textarea>`.
    let email_input = slice_text_field(&routes, "email_addr");
    assert!(
        email_input.contains(".input_type(\"email\")"),
        "a Text{{email}} field must render a typed email input:\n{email_input}"
    );
    let site_input = slice_text_field(&routes, "site");
    assert!(
        site_input.contains(".input_type(\"url\")"),
        "a Text{{url}} field must render a typed url input:\n{site_input}"
    );
    // Neither may be a <textarea> (which can't carry type=email/url).
    assert!(
        !routes.contains("textarea id=\"email_addr\"") && !routes.contains("textarea id=\"site\""),
        "type-dependent Text controls must not be textareas:\n{routes}"
    );
    // The 422 value is carried via the input's `.value(...)` (input, not
    // textarea content), routed through `a11y::TextField` (#1706).
    assert!(
        routes.contains(".value(changeset.field_value(\"email_addr\").unwrap_or_default())"),
        "the typed input must re-fill via a value builder call:\n{routes}"
    );
}

/// Slice a textarea's field-specific attributes: from the `id`/`name` marker
/// up to the shared `class=` skeleton, so an assertion can't match a sibling
/// control's attributes.
fn slice_textarea_attrs<'a>(routes: &'a str, marker: &str) -> &'a str {
    let start = routes
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker} in:\n{routes}"));
    let rest = &routes[start..];
    let end = rest.find("class=").unwrap_or(rest.len());
    &rest[..end]
}

/// Issue #1750: a `--live-validation` scaffold renders its form through the
/// per-field path (not `form_for`), which historically dropped the #1388
/// client-side HTML5 constraint attributes. The live path must now carry the
/// SAME attributes the standard path emits — `minlength`/`maxlength`,
/// `type="email"`/`type="url"`, numeric `min`/`max`, and a `<textarea>` for
/// constrained `Text` — threaded through the htmx inline-validation wiring, and
/// the inline-validate handler fragment must carry them too so the swapped-in
/// field doesn't shed its client-side guards on the first `change`.
#[test]
fn live_validation_forms_carry_html5_constraints() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "live-validate-html5-app"]);
    let project = tmp.path().join("live-validate-html5-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String{min=3,max=120}",
            "contact:String{email}",
            "homepage:String{url}",
            "age:i32{min=0,max=130}",
            "notes:Text{min=10,max=500}",
            "bio:Option<String>{max=200}",
            "--live-validation",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // The string length constraint on the htmx-validated `title` input.
    assert!(
        routes.contains("minlength=\"3\" maxlength=\"120\""),
        "title must render minlength/maxlength in live-validation mode:\n{routes}"
    );
    // Typed inputs (email/url) instead of a bare text input.
    assert!(
        routes.contains("type=\"email\" id=\"contact\""),
        "contact must render type=email in live-validation mode:\n{routes}"
    );
    assert!(
        routes.contains("type=\"url\" id=\"homepage\""),
        "homepage must render type=url in live-validation mode:\n{routes}"
    );
    // Numeric min/max on the number input.
    assert!(
        routes.contains("type=\"number\"") && routes.contains("min=\"0\" max=\"130\""),
        "age must render type=number with min/max in live-validation mode:\n{routes}"
    );
    // A constrained `Text` column becomes a <textarea> carrying its length rule.
    assert!(
        routes.contains("textarea id=\"notes\" name=\"notes\"")
            && routes.contains("maxlength=\"500\""),
        "constrained Text `notes` must render a <textarea> with maxlength:\n{routes}"
    );
    // The nullable `bio` keeps its maxlength.
    assert!(
        routes.contains("maxlength=\"200\""),
        "bio must render maxlength in live-validation mode:\n{routes}"
    );

    // The htmx inline-validation wiring survives on a constrained validated
    // input (real-time validation still works alongside the static constraints),
    // and its swap wrapper marker is present.
    assert!(
        routes.contains("hx-post=(paths::validate_title())")
            && routes.contains("data-autumn-field-wrapper=\"title\""),
        "the constrained title input must keep its htmx inline-validation wiring (via typed path helper):\n{routes}"
    );

    // Issue #1360 (finding F): the inline-validation POST `hx-include`s the whole
    // form, which carries the hidden one-time `_submit_token`. Without a filter
    // the first field validation would consume the token on the guarded
    // `/validate/...` route, and the real create submit would replay the
    // validation fragment instead of running the mutation. The htmx input must
    // drop the token via `hx-params`, while the real create form keeps it.
    assert!(
        routes.contains("hx-params=\"not _submit_token\""),
        "live-validation inputs must drop _submit_token from the inline POST:\n{routes}"
    );
    assert!(
        routes.contains("submit_token_input(submit_token.as_ref(), submit_field.as_ref())"),
        "the create/update form must still carry the submit token:\n{routes}"
    );

    // The inline-validate handler returns the SAME constrained fragment, so the
    // swapped-in field doesn't drop the client-side attributes on first change.
    let validate_title = slice_fn(&routes, "pub async fn validate_title(");
    assert!(
        validate_title.contains("minlength=\"3\" maxlength=\"120\""),
        "the validate_title fragment must also carry the HTML5 constraints:\n{validate_title}"
    );
    assert!(
        !validate_title.contains("type=\"email\""),
        "sanity: the validate_title slice must be bounded to its own handler:\n{validate_title}"
    );
}

/// Slice a generated handler body from its `fn` marker to the next handler's
/// `#[post(` attribute (or end of file), so an assertion scoped to one
/// inline-validate handler can't accidentally match a sibling's fragment.
fn slice_fn<'a>(routes: &'a str, marker: &str) -> &'a str {
    let start = routes
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker} in:\n{routes}"));
    let rest = &routes[start..];
    let end = rest[1..].find("#[post(").map_or(rest.len(), |rel| rel + 1);
    &rest[..end]
}

#[test]
fn unknown_constraint_modifier_fails_the_scaffold() {
    // AC5: a misspelled modifier fails loudly, naming the offending token,
    // rather than being silently dropped.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bad-modifier-app"]);
    let project = tmp.path().join("bad-modifier-app");
    let output = run_autumn(
        &project,
        &["generate", "scaffold", "Post", "title:String{maxx=5}"],
    );
    assert!(
        !output.status.success(),
        "an unknown modifier must fail the scaffold"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("maxx"),
        "the error must name the offending token:\n{stderr}"
    );
}

#[test]
fn out_of_range_i32_bound_fails_the_scaffold() {
    // `3000000000` > i32::MAX: emitting `#[validate(range(max = 3000000000))]`
    // on an `i32` field would fail to COMPILE the generated app, so the
    // generator must reject the bound up front with an actionable message.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "oob-bound-app"]);
    let project = tmp.path().join("oob-bound-app");
    let output = run_autumn(
        &project,
        &["generate", "scaffold", "Post", "count:i32{max=3000000000}"],
    );
    assert!(
        !output.status.success(),
        "an out-of-range i32 bound must fail the scaffold"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("3000000000") && stderr.contains("i32"),
        "the error must name the bad bound and the field type:\n{stderr}"
    );
}

#[test]
fn email_and_url_together_fails_the_scaffold() {
    // Emitting both `#[validate(email)]` and `#[validate(url)]` makes the field
    // unwritable, so the mutually-exclusive pair is rejected up front with an
    // actionable error naming the conflict and the field.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "email-url-app"]);
    let project = tmp.path().join("email-url-app");
    let output = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Contact",
            "contact:String{email,url}",
        ],
    );
    assert!(
        !output.status.success(),
        "email + url on one field must fail the scaffold"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mutually exclusive") && stderr.contains("contact"),
        "the error must name the conflict and the field:\n{stderr}"
    );
}

#[test]
fn in_range_i32_bound_generates_and_type_checks_shape() {
    // The happy path: a valid in-range i32 bound still emits the range rule.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "in-range-bound-app"]);
    let project = tmp.path().join("in-range-bound-app");
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "count:i32{max=1000000}"],
    );
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("#[validate(range(max = 1000000))]"),
        "a valid in-range bound must still emit its range rule:\n{model}"
    );
}

#[test]
fn non_canonical_numeric_bounds_generate_valid_rust_literals() {
    // Bounds that parse but aren't valid Rust literals (`.5`, `+1`) must be
    // canonicalized so the generated `#[validate(range(...))]` compiles:
    // `.5` -> `0.5`, whole-number float `1` -> `1.0`, `+1` -> `1`.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "canon-bound-app"]);
    let project = tmp.path().join("canon-bound-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "ratio:f64{min=.5,max=1}",
            "count:i32{min=+1,max=10}",
        ],
    );
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("#[validate(range(min = 0.5, max = 1.0))]"),
        "the float bounds must be canonical valid float literals:\n{model}"
    );
    assert!(
        model.contains("#[validate(range(min = 1, max = 10))]"),
        "the `+1` integer bound must canonicalize to `1`:\n{model}"
    );
    // No raw non-canonical tokens should survive anywhere in the generated app.
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    for bad in ["=.5", "\".5\"", "min = .5", "= +1", "\"+1\""] {
        assert!(
            !model.contains(bad) && !routes.contains(bad),
            "no raw non-canonical bound token ({bad}) may survive:\nmodel:\n{model}\nroutes:\n{routes}"
        );
    }
}

/// Slow end-to-end check: the constrained scaffold (model `#[validate]` rules,
/// validator dependency, HTML5-attributed form controls) type-checks against
/// this workspace's `autumn-web`.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn constrained_scaffold_cargo_checks() {
    use std::fmt::Write as _;

    let (_tmp, project) = constrained_project("validate-check-app");
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
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the constrained scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
