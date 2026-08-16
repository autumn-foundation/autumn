//! `autumn generate scaffold … --i18n` emits translatable views (issue #1349):
//! every page title, heading, button, link, and field label in the generated
//! HTML becomes a `t!(locale, "key")` lookup, the view handlers take the
//! `Locale` extractor, and the referenced keys are back-filled into
//! `i18n/en.ftl` with their English values.
//!
//! These exercise the real CLI binary end to end — the flag parsing, the
//! project-level wiring (`autumn-web`'s `i18n` feature, `[i18n]` in
//! `autumn.toml`, `.i18n_auto()` in `main.rs`), and the round trip through
//! `autumn i18n check`, which is the check the framework itself would run
//! against the generated app.

use std::collections::BTreeSet;
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

fn run_autumn_expect_fail(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    assert!(
        !output.status.success(),
        "autumn {args:?} unexpectedly succeeded\nstdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `autumn new` + a `--i18n` scaffold of a five-field resource — the shape the
/// issue's success metric measures (~12 hand edits to localize, down to 0).
fn i18n_project(extra: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "i18n-app"]);
    let project = tmp.path().join("i18n-app");
    let mut args = vec![
        "generate",
        "scaffold",
        "Post",
        "title:String",
        "body:Text",
        "published:bool",
        "views:i64",
        "author_name:String",
        "--i18n",
    ];
    args.extend_from_slice(extra);
    run_autumn_ok(&project, &args);
    (tmp, project)
}

/// Every `t!(locale, "key")` key referenced by `src`.
fn t_macro_keys(src: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut rest = src;
    while let Some(pos) = rest.find("t!(locale, \"") {
        let after = &rest[pos + "t!(locale, \"".len()..];
        let Some(end) = after.find('"') else { break };
        keys.insert(after[..end].to_owned());
        rest = &after[end..];
    }
    keys
}

/// Every top-level key defined in an `.ftl` source.
fn ftl_keys(src: &str) -> BTreeSet<String> {
    src.lines()
        .filter(|l| {
            !l.trim_start().starts_with('#')
                && !l.trim().is_empty()
                && !l.starts_with(' ')
                && !l.starts_with('\t')
        })
        .filter_map(|l| l.split_once('='))
        .map(|(k, _)| k.trim().to_owned())
        .collect()
}

#[test]
fn views_look_up_every_user_facing_string_through_the_t_macro() {
    let (_tmp, project) = i18n_project(&[]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    let keys = t_macro_keys(&routes);
    for expected in [
        // Shared chrome, reused across resources.
        "common.create",
        "common.save",
        "common.back",
        "common.edit",
        "common.new",
        "common.delete",
        // This resource's nouns.
        "post.name",
        "post.name.plural",
        "post.index.title",
        "post.field.title",
        "post.field.author_name",
    ] {
        assert!(
            keys.contains(expected),
            "missing key `{expected}`: {keys:?}"
        );
    }
    // …and none of the English survives in the markup.
    for literal in [
        "\"New Post\"",
        "\"Post index\"",
        "\"Back to list\"",
        "\"Author Name\"",
        "Button::new(\"Create\")",
        "Button::new(\"Save\")",
        "Column::new(\"Title\"",
    ] {
        assert!(
            !routes.contains(literal),
            "hardcoded label {literal} survived:\n{routes}"
        );
    }
}

#[test]
fn every_referenced_key_is_defined_in_the_generated_en_ftl() {
    let (_tmp, project) = i18n_project(&[]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    let ftl = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();

    let referenced = t_macro_keys(&routes);
    let defined = ftl_keys(&ftl);
    let missing: Vec<_> = referenced.difference(&defined).collect();
    assert!(missing.is_empty(), "undefined keys {missing:?}:\n{ftl}");
    // The English values are the exact strings the plain scaffold renders, so an
    // `en` app is visually identical to a non-`--i18n` one.
    assert!(ftl.contains("common.create = Create"), "{ftl}");
    assert!(ftl.contains("common.new = New { $resource }"), "{ftl}");
    assert!(ftl.contains("post.name = Post"), "{ftl}");
    assert!(
        ftl.contains("post.field.author_name = Author Name"),
        "{ftl}"
    );
}

#[test]
fn the_generated_app_passes_autumn_i18n_check() {
    let (_tmp, project) = i18n_project(&[]);
    // The framework's own linter (#1252) is the strongest statement of the two
    // properties this feature has to hold: no key referenced without a
    // definition, and no definition nobody references. `--strict` promotes the
    // "unused" warnings to failures so both directions are enforced.
    run_autumn_ok(&project, &["i18n", "check", "--strict"]);
}

#[test]
fn the_project_is_wired_so_the_keys_resolve_with_no_further_config() {
    let (_tmp, project) = i18n_project(&[]);

    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("\"i18n\""),
        "autumn-web's `i18n` feature must be enabled:\n{cargo}"
    );
    let autumn_toml = fs::read_to_string(project.join("autumn.toml")).unwrap();
    assert!(autumn_toml.contains("[i18n]"), "{autumn_toml}");
    assert!(
        autumn_toml.contains("default_locale = \"en\""),
        "{autumn_toml}"
    );
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains(".i18n_auto()"),
        "the builder must load the bundle:\n{main_rs}"
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(routes.contains("use autumn_web::i18n::Locale;"), "{routes}");
    assert!(routes.contains("locale: Locale"), "{routes}");
}

#[test]
fn a_second_resource_reuses_the_shared_chrome_instead_of_duplicating_it() {
    let (_tmp, project) = i18n_project(&[]);
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Comment", "body:Text", "--i18n"],
    );

    let ftl = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    assert_eq!(
        ftl.matches("common.create =").count(),
        1,
        "shared chrome must be written once:\n{ftl}"
    );
    let keys = ftl_keys(&ftl);
    assert!(keys.contains("post.field.title"), "{ftl}");
    assert!(keys.contains("comment.field.body"), "{ftl}");
    assert!(
        !keys.iter().any(|k| k.starts_with("comment.create")),
        "chrome must not be duplicated per model:\n{ftl}"
    );
    // Both resources still pass the linter together.
    run_autumn_ok(&project, &["i18n", "check", "--strict"]);
}

#[test]
fn translating_the_ftl_survives_regeneration() {
    let (_tmp, project) = i18n_project(&[]);
    let ftl_path = project.join("i18n/en.ftl");
    let translated = fs::read_to_string(&ftl_path)
        .unwrap()
        .replace("common.create = Create", "common.create = Publish");
    fs::write(&ftl_path, &translated).unwrap();

    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
            "views:i64",
            "author_name:String",
            "--i18n",
            "--force",
        ],
    );
    let after = fs::read_to_string(&ftl_path).unwrap();
    assert!(
        after.contains("common.create = Publish"),
        "an edited value must not be rewritten:\n{after}"
    );
}

#[test]
fn the_flag_composes_with_searchable_and_soft_delete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "combo-app"]);
    let project = tmp.path().join("combo-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--i18n",
            "--searchable",
            "title,body",
            "--soft-delete",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    // The strings those two flags add are translated too, not just the base CRUD.
    for literal in [
        "\"Search Posts\"",
        "\"Trash is empty.\"",
        "Button::new(\"Restore\")",
        "\"Deleted Posts\"",
    ] {
        assert!(
            !routes.contains(literal),
            "flag-specific label {literal} survived untranslated:\n{routes}"
        );
    }
    run_autumn_ok(&project, &["i18n", "check", "--strict"]);
}

#[test]
fn an_api_scaffold_renders_no_labels_so_the_flag_is_a_no_op() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "api-app"]);
    let project = tmp.path().join("api-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--api",
            "--i18n",
        ],
    );
    assert!(
        !project.join("i18n/en.ftl").exists(),
        "`--api --i18n` references no keys, so it writes no `.ftl` stub"
    );
    assert!(
        !project.join("src/routes").exists(),
        "an `--api` scaffold emits no HTML routes module"
    );
}

#[test]
fn the_view_variants_that_cannot_be_translated_are_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "live-app"]);
    let project = tmp.path().join("live-app");
    let stderr = run_autumn_expect_fail(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--i18n",
            "--live",
        ],
    );
    assert!(
        stderr.contains("--i18n") && stderr.contains("--live"),
        "the error must name both flags: {stderr}"
    );
    assert!(
        !project.join("i18n").exists() && !project.join("src/routes").exists(),
        "a refused scaffold must write nothing"
    );
}

#[test]
fn omitting_the_flag_leaves_the_scaffold_untouched() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "plain-app"]);
    let project = tmp.path().join("plain-app");
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "body:Text"],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(!routes.contains("t!(locale"), "{routes}");
    assert!(!routes.contains("Locale"), "{routes}");
    assert!(routes.contains("\"New Post\""), "{routes}");
    assert!(
        !project.join("i18n/en.ftl").exists(),
        "the default scaffold stays zero-i18n-config"
    );
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(!cargo.contains("\"i18n\""), "{cargo}");
}

#[test]
fn destroy_takes_back_the_resource_keys_and_leaves_the_shared_chrome() {
    let (_tmp, project) = i18n_project(&[]);
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Comment", "body:Text", "--i18n"],
    );
    run_autumn_ok(
        &project,
        &["destroy", "scaffold", "Comment", "body:Text", "--i18n"],
    );

    let ftl = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    assert!(
        !ftl.contains("comment."),
        "the destroyed resource's keys must be gone:\n{ftl}"
    );
    assert!(
        ftl.contains("post.field.title"),
        "the surviving resource's keys must remain:\n{ftl}"
    );
    assert!(
        ftl.contains("common.create = Create"),
        "shared chrome is reused by the surviving resource:\n{ftl}"
    );
    // `.i18n_auto()` panics at startup if the default bundle is missing, so the
    // file itself must always survive a destroy.
    assert!(project.join("i18n/en.ftl").exists());
    run_autumn_ok(&project, &["i18n", "check", "--strict"]);
}
