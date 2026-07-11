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

    assert!(
        routes.contains("minlength=\"3\" maxlength=\"120\""),
        "title must render minlength/maxlength:\n{routes}"
    );
    assert!(
        routes.contains("type=\"email\""),
        "contact must render type=email:\n{routes}"
    );
    assert!(
        routes.contains("type=\"url\""),
        "homepage must render type=url:\n{routes}"
    );
    assert!(
        routes.contains("type=\"number\"") && routes.contains("min=\"0\" max=\"130\""),
        "age must render type=number with min/max:\n{routes}"
    );
    assert!(
        routes.contains("maxlength=\"200\""),
        "bio must render maxlength:\n{routes}"
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
    assert!(
        routes.contains("type=\"number\" id=\"ratio\""),
        "the constrained float must render as a number input:\n{routes}"
    );
    let ratio_input = slice_input(&routes, "id=\"ratio\" name=\"ratio\"");
    // Float bounds are canonicalized to valid float literals (`0` -> `0.0`), so
    // the HTML5 attributes carry the decimal form too.
    assert!(
        ratio_input.contains("min=\"0.0\"")
            && ratio_input.contains("max=\"1.0\"")
            && ratio_input.contains("step=\"any\""),
        "constrained float must carry min/max AND step=any:\n{ratio_input}"
    );
}

#[test]
fn required_only_on_non_nullable_constrained_fields() {
    let (_tmp, project) = constrained_project("validate-required-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // The non-nullable `title` input keeps the browser-native required signal;
    // the nullable `bio` input must NOT (leaving it blank is valid).
    let title_input = slice_input(&routes, "id=\"title\" name=\"title\"");
    assert!(
        title_input.contains("required aria-required=\"true\""),
        "non-nullable title must be required:\n{title_input}"
    );
    let bio_input = slice_input(&routes, "id=\"bio\" name=\"bio\"");
    assert!(
        !bio_input.contains("required"),
        "nullable bio must not be required:\n{bio_input}"
    );
}

/// Slice from an input's id/name marker to the terminating `;` so a
/// field-scoped attribute assertion doesn't accidentally match a sibling.
fn slice_input<'a>(routes: &'a str, marker: &str) -> &'a str {
    let start = routes
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker} in:\n{routes}"));
    let end = routes[start..]
        .find(';')
        .map_or(routes.len(), |rel| start + rel);
    &routes[start..end]
}

#[test]
fn preserved_value_binding_survives_422_rerender() {
    let (_tmp, project) = constrained_project("validate-rerender-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    // The appended control re-fills its value from the changeset, so a 422
    // re-render keeps what the user typed.
    assert!(
        routes.contains("value=(changeset.field_value(\"title\").unwrap_or_default())"),
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

    assert!(
        routes.contains("type=\"email\" id=\"email_addr\""),
        "a Text{{email}} field must render a typed email input:\n{routes}"
    );
    assert!(
        routes.contains("type=\"url\" id=\"site\""),
        "a Text{{url}} field must render a typed url input:\n{routes}"
    );
    // Neither may be a <textarea> (which can't carry type=email/url).
    assert!(
        !routes.contains("textarea id=\"email_addr\"") && !routes.contains("textarea id=\"site\""),
        "type-dependent Text controls must not be textareas:\n{routes}"
    );
    // The 422 value is carried via the `value=` attribute (input, not content).
    assert!(
        routes.contains("value=(changeset.field_value(\"email_addr\").unwrap_or_default())"),
        "the typed input must re-fill via a value= attribute:\n{routes}"
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
