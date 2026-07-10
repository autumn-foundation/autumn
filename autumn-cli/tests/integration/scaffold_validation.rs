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
