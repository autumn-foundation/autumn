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
use std::fmt::Write as _;
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

/// String literals in Maud **body** position — `{ "text" }` — which is where
/// user-facing prose ends up, as opposed to attribute values and class lists.
///
/// Deliberately generic. The older sweep below names the literals it expects to
/// be gone, which only ever covers the surfaces someone remembered to list; the
/// strings that survived longest into review were all behind a flag that sweep
/// never turned on (the lock banner, the duplicate-value error, the attachment
/// meta). This finds prose the flag has not claimed, without being told about
/// it in advance.
fn maud_body_prose(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b'"' {
            j += usize::from(bytes[j] == b'\\') + 1;
        }
        if j >= bytes.len() {
            break;
        }
        let literal = &src[start..j];
        // Body position: the literal follows `{` (with only whitespace between)
        // or another body literal / splice. Attribute values follow `=`.
        let before = src[..i].trim_end();
        let is_body = before.ends_with('{') || before.ends_with(')');
        // Prose, not markup: more than one word, and no CSS/class/path shape.
        let is_prose = literal.contains(' ')
            && literal.chars().any(|c| c.is_ascii_alphabetic())
            && !literal.contains("autumn-")
            && !literal.contains('<')
            && !literal.starts_with('/');
        if is_body && is_prose {
            out.insert(literal.to_owned());
        }
        i = j + 1;
    }
    out
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

/// The sweep the named-literal test below cannot do: turn on every flag
/// `--i18n` composes with and assert no PROSE survives in view body position.
///
/// Each of the last several English strings to reach review was behind a flag
/// the basic fixture never set — the optimistic-lock conflict banner, the
/// duplicate-value error, the attachment meta line. An allowlist finds those
/// only once someone has already found them.
#[test]
fn no_prose_survives_in_a_maximally_flagged_scaffold() {
    // Two scaffolds, because `lock_version` and `Attachment` are a refused
    // combination for reasons that predate this flag (the multipart handler
    // saves the blob before the row write). Between them every surface `--i18n`
    // composes with is turned on.
    let sweep = |name: &str, columns: &[&str]| -> BTreeSet<String> {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_autumn_ok(tmp.path(), &["new", name]);
        let project = tmp.path().join(name);
        let mut args = vec!["generate", "scaffold", "Post"];
        args.extend_from_slice(columns);
        args.extend_from_slice(&[
            "--i18n",
            "--soft-delete",
            "--searchable",
            "title,body",
            // Issue #1393: the CSV import surface renders a whole page of its
            // own (upload form, preview totals, per-row error table), so it has
            // to be inside this sweep or its prose would be exactly the kind of
            // flag-gated English the sweep exists to catch.
            "--import",
        ]);
        run_autumn_ok(&project, &args);
        maud_body_prose(&fs::read_to_string(project.join("src/routes/posts.rs")).unwrap())
    };

    let mut leaked = sweep(
        "i18n-sweep-attach",
        &[
            "title:String:unique",
            "body:Text",
            "author_name:String",
            "cover:Attachment",
            "status:String:states(draft -> published, published -> archived)",
        ],
    );
    leaked.extend(sweep(
        "i18n-sweep-lock",
        &[
            "title:String:unique",
            "body:Text",
            "author_name:String",
            "lock_version:i32",
        ],
    ));

    // Anything left is either prose the flag forgot, or a widget's own chrome
    // this scaffold cannot reach — and the latter is warned about by name, not
    // emitted here.
    assert!(
        leaked.is_empty(),
        "user-facing prose survived `--i18n` in view body position: {leaked:#?}\n\
         Either route it through `ViewLabels`, or — if it comes from an \
         autumn-web widget this scaffold only calls — warn about it and add it \
         to the documented gaps."
    );

    // CONTROL. An empty result would otherwise prove only that the detector
    // found nothing anywhere — so run it over the same scaffold generated
    // WITHOUT the flag, where the prose is known to be present. This is what
    // keeps the assertion above from silently decaying into a no-op.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "plain-sweep"]);
    let plain = tmp.path().join("plain-sweep");
    run_autumn_ok(
        &plain,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String:unique",
            "body:Text",
            "author_name:String",
            "lock_version:i32",
            "--soft-delete",
            "--searchable",
            "title,body",
        ],
    );
    let control = maud_body_prose(&fs::read_to_string(plain.join("src/routes/posts.rs")).unwrap());
    assert!(
        !control.is_empty(),
        "the prose detector found nothing even in a scaffold with no `--i18n` \
         at all, so the assertion above proves nothing — fix the detector"
    );
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
        "common.delete",
        "common.show",
        // Widget-supplied defaults the generator now overrides.
        "common.pagination",
        "common.previous",
        "common.next",
        // This resource's own strings.
        "post.new",
        "post.name.plural",
        "post.index.title",
        "post.index.empty",
        "post.show.title",
        "post.edit.title",
        "post.delete.confirm",
        "post.field.title",
        "post.field.author_name",
        // One-shot notices render into these same views.
        "post.flash.created",
        "post.flash.updated",
        "post.flash.deleted",
    ] {
        assert!(
            keys.contains(expected),
            "missing key `{expected}`: {keys:?}"
        );
    }
    // …and none of the English survives.
    //
    // Every literal here is one the PLAIN scaffold really does contain — the
    // control below re-generates without the flag and asserts exactly that, so
    // these can never quietly decay into assertions about a string that was
    // never emitted in the first place.
    let hardcoded = [
        "\"New Post\"",
        "\"Post index\"",
        "h1 { \"Posts\" }",
        "Column::new(\"Title\"",
        "(\"Created at\", maud::html!",
        "DataTableConfig::new(\"No posts yet.\")",
        "flash.success(\"Post created\")",
        "confirm('Delete this Post?')",
        "Link::new(paths::index(), \"Back to list\")",
        "Link::new(paths::edit(row.id), \"Edit\")",
    ];
    for literal in hardcoded {
        assert!(
            !routes.contains(literal),
            "hardcoded label {literal} survived:\n{routes}"
        );
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "control-app"]);
    let control = tmp.path().join("control-app");
    run_autumn_ok(
        &control,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "published:bool",
            "views:i64",
            "author_name:String",
        ],
    );
    let plain = fs::read_to_string(control.join("src/routes/posts.rs")).unwrap();
    for literal in hardcoded {
        assert!(
            plain.contains(literal),
            "the non-i18n control must contain {literal}, or the assertion above \
             proves nothing:\n{plain}"
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
    // Nothing is defined that nothing references, either — that is what
    // `autumn i18n check --strict` fails on, and a translator should never be
    // handed a key the app can never show them.
    let unused: Vec<_> = defined.difference(&referenced).collect();
    assert!(unused.is_empty(), "unused keys {unused:?}:\n{routes}");
    // The English values are the exact strings the plain scaffold renders, so an
    // `en` app is visually identical to a non-`--i18n` one.
    assert!(ftl.contains("common.create = Create"), "{ftl}");
    assert!(ftl.contains("common.back = Back to list"), "{ftl}");
    assert!(ftl.contains("post.new = New Post"), "{ftl}");
    assert!(ftl.contains("post.name.plural = Posts"), "{ftl}");
    assert!(
        ftl.contains("post.field.author_name = Author Name"),
        "{ftl}"
    );
    // Row keys and counts interpolate as Fluent arguments so a translation can
    // position them; the model's NAME never does — a noun dropped into a
    // sentence pattern cannot be made to agree in gender or case from the
    // bundle alone, which is why "New Post" is a per-resource key.
    assert!(ftl.contains("post.show.title = Post #{ $id }"), "{ftl}");
    assert!(!ftl.contains("$resource"), "no noun interpolation:\n{ftl}");
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
            // Without this the index is owner-scoped and the Trash view is gated
            // off entirely — the assertions below would then pass against a file
            // that simply has no trash markup in it.
            "--no-policy",
        ],
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("pub async fn trash(") && routes.contains("pub async fn search("),
        "both surfaces must actually be emitted for this test to mean anything"
    );
    // The strings those two flags add are translated too, not just the base CRUD.
    for literal in [
        "\"Search Posts\"",
        "\"Trash is empty.\"",
        "Button::new(\"Restore\")",
        "\"Deleted Posts\"",
        "Link::new(export_href, \"Export CSV\")",
        "Column::new(\"Actions\"",
        "Column::new(\"Deleted At\"",
        "\"Post restored",
        "\"Delete selected\"",
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

#[test]
fn keys_land_in_the_locale_the_app_actually_treats_as_default() {
    // A project configured on `fr` resolves every lookup through `fr.ftl` and
    // its fallback chain, which need not include `en`. Keys parked in `en.ftl`
    // would render as visible `{$key}` placeholders at runtime while `t!`'s
    // compile-time check (which defaults to `en`) passed — a miss that only
    // shows up in production.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "fr-app"]);
    let project = tmp.path().join("fr-app");
    let autumn_toml = project.join("autumn.toml");
    let mut config = fs::read_to_string(&autumn_toml).unwrap();
    // A trailing comment and a custom bundle directory are both legal, and both
    // have to be honoured: the app resolves lookups through `<dir>/<tag>.ftl`.
    config.push_str(
        "\n[i18n] # locale settings\ndefault_locale = \"fr\" # ours\n         supported_locales = [\"fr\"]\ndir = \"translations\"\n",
    );
    fs::write(&autumn_toml, config).unwrap();

    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--i18n"],
    );
    assert!(
        project.join("translations/fr.ftl").exists(),
        "keys must be written to the configured directory and default locale"
    );
    assert!(!project.join("i18n/en.ftl").exists());
    // …and the image ships the directory the app actually reads.
    let dockerfile = fs::read_to_string(project.join("Dockerfile")).unwrap();
    assert!(
        dockerfile.contains("COPY translations ./translations"),
        "{dockerfile}"
    );
    // The existing configuration is left exactly as the project set it.
    let after = fs::read_to_string(&autumn_toml).unwrap();
    assert!(after.contains("default_locale = \"fr\""), "{after}");
    assert_eq!(
        after.matches("[i18n]").count(),
        1,
        "a second `[i18n]` table makes the whole config unparseable:\n{after}"
    );
    run_autumn_ok(&project, &["i18n", "check", "--strict"]);
}

#[test]
fn the_image_carries_the_bundle_it_panics_without() {
    // `.i18n_auto()` reads the default locale's `.ftl` at startup and PANICS
    // when it is missing, and the plain `autumn new` Dockerfile copies no
    // `i18n/` directory — so without this the container builds clean and then
    // crash-loops on deploy.
    let (_tmp, project) = i18n_project(&[]);
    let dockerfile = fs::read_to_string(project.join("Dockerfile")).unwrap();
    assert!(
        dockerfile.contains("COPY i18n ./i18n"),
        "the builder stage must see the bundle:\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("COPY --from=builder /app/i18n /app/i18n"),
        "the runtime stage must ship the bundle:\n{dockerfile}"
    );
}

#[test]
fn regenerating_after_dropping_a_field_prunes_its_keys() {
    let (_tmp, project) = i18n_project(&[]);
    let ftl_path = project.join("i18n/en.ftl");
    fs::write(
        &ftl_path,
        fs::read_to_string(&ftl_path)
            .unwrap()
            .replace("post.new = New Post", "post.new = Nouvel article"),
    )
    .unwrap();

    // Same resource, two fewer fields.
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--i18n",
            "--force",
        ],
    );
    let ftl = fs::read_to_string(&ftl_path).unwrap();
    for gone in [
        "post.field.views",
        "post.field.author_name",
        "post.field.published",
    ] {
        assert!(
            !ftl.contains(gone),
            "`{gone}` has no call site left, so `i18n check --strict` fails on it:\n{ftl}"
        );
    }
    assert!(
        ftl.contains("post.new = Nouvel article"),
        "a translated value must survive regeneration:\n{ftl}"
    );
    run_autumn_ok(&project, &["i18n", "check", "--strict"]);
}

#[test]
fn a_non_default_locale_project_still_compiles_against_the_t_validator() {
    // `t!`'s compile-time check does not read `autumn.toml`: it looks in
    // `i18n/en.ftl` and degrades to a runtime lookup only when that file is
    // ABSENT. A project on `default_locale = "fr"` that still keeps an
    // `i18n/en.ftl` would otherwise get a `compile_error!` per lookup.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "fr-en-app"]);
    let project = tmp.path().join("fr-en-app");
    fs::create_dir_all(project.join("i18n")).unwrap();
    fs::write(project.join("i18n/en.ftl"), "welcome.title = Welcome\n").unwrap();
    let autumn_toml = project.join("autumn.toml");
    let mut config = fs::read_to_string(&autumn_toml).unwrap();
    config.push_str("\n[i18n]\ndefault_locale = \"fr\"\nsupported_locales = [\"fr\"]\n");
    fs::write(&autumn_toml, config).unwrap();

    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--i18n"],
    );
    let fr = fs::read_to_string(project.join("i18n/fr.ftl")).unwrap();
    let en = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    assert!(fr.contains("post.new = New Post"), "{fr}");
    assert!(
        en.contains("post.new = New Post"),
        "the bundle the validator reads must be kept in step:\n{en}"
    );
    assert!(
        en.contains("welcome.title = Welcome"),
        "and its existing keys left alone:\n{en}"
    );
}

#[test]
fn destroy_prunes_the_block_from_every_locale_bundle() {
    // A translator starts a locale by copying the default bundle and
    // translating in place, so `fr.ftl` carries this resource's marked block
    // too. Reverting only the default leaves those copies behind with nothing
    // referencing them, which `autumn i18n check --strict` fails on as unused.
    let (_tmp, project) = i18n_project(&[]);
    let en = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    // The translated copy, with one hand-authored key of its own outside the
    // generated blocks — that one must survive.
    fs::write(
        project.join("i18n/fr.ftl"),
        format!(
            "{}\nmarketing.tagline = Notre slogan\n",
            en.replace("New Post", "Nouvel article")
        ),
    )
    .unwrap();

    // Destroy recomputes the generate plan, so it needs the same flags — and
    // the same field list — to reach the i18n reverts at all.
    run_autumn_ok(
        &project,
        &[
            "destroy",
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

    let fr = fs::read_to_string(project.join("i18n/fr.ftl")).unwrap();
    assert!(
        !fr.contains("post.new"),
        "the copied block must go with the resource:\n{fr}"
    );
    assert!(
        fr.contains("marketing.tagline = Notre slogan"),
        "a hand-authored key outside the block stays:\n{fr}"
    );
}

#[test]
fn scaffolding_into_a_multilingual_project_says_which_locales_need_work() {
    // New keys land in the default bundle only — filling `es.ftl` with English
    // would report as translated and ship English. `autumn i18n check
    // --strict` exits non-zero on the untranslated keys until someone writes
    // them, so the generator says so by name rather than letting CI be the
    // messenger.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "multi-locale-app"]);
    let project = tmp.path().join("multi-locale-app");
    fs::create_dir_all(project.join("i18n")).unwrap();
    fs::write(project.join("i18n/es.ftl"), "welcome.title = Bienvenido\n").unwrap();

    let output = Command::new(autumn_bin())
        .args(["generate", "scaffold", "Post", "title:String", "--i18n"])
        .current_dir(&project)
        .output()
        .expect("failed to run autumn");
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("es") && stderr.contains("translated"),
        "the untranslated locale must be named:\n{stderr}"
    );
    // And nothing English was written into it.
    let es = fs::read_to_string(project.join("i18n/es.ftl")).unwrap();
    assert!(!es.contains("post.new"), "{es}");
}

#[test]
fn regenerating_keeps_a_dropped_field_key_hand_written_code_still_calls() {
    // The other half of `regenerating_after_dropping_a_field_prunes_its_keys`:
    // "this render no longer emits it" is not "nothing uses it". A surviving
    // call site outside the generated module keeps its key, because `t!`
    // rejects a missing key at COMPILE time — the same protection `destroy`
    // already gets from a project-wide scan.
    let (_tmp, project) = i18n_project(&[]);
    let ftl_path = project.join("i18n/en.ftl");
    fs::create_dir_all(project.join("src/components")).unwrap();
    fs::write(
        project.join("src/components/nav.rs"),
        "fn card(locale: &Locale) -> Markup { html! { (t!(locale, \"post.field.views\")) } }\n",
    )
    .unwrap();

    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--i18n",
            "--force",
        ],
    );

    let ftl = fs::read_to_string(&ftl_path).unwrap();
    assert!(
        ftl.contains("post.field.views"),
        "a live call site keeps its key through regeneration:\n{ftl}"
    );
    assert!(
        !ftl.contains("post.field.author_name") && !ftl.contains("post.field.published"),
        "keys nothing calls are still pruned:\n{ftl}"
    );
}

#[test]
fn a_workspace_parents_cargo_config_selects_the_validator_bundle() {
    // Cargo reads `.cargo/config[.toml]` from the directory and every ancestor.
    // A generated app nested in a workspace usually carries no `.cargo` of its
    // own and inherits the root's, so reading only the app's directory takes a
    // locale cargo never applies and keeps the wrong bundle in step.
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path();
    fs::create_dir_all(workspace.join(".cargo")).unwrap();
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[env]\nAUTUMN_I18N_DEFAULT_LOCALE = \"de\"\n",
    )
    .unwrap();

    run_autumn_ok(workspace, &["new", "nested-app"]);
    let project = workspace.join("nested-app");
    fs::create_dir_all(project.join("i18n")).unwrap();
    fs::write(project.join("i18n/de.ftl"), "welcome.title = Willkommen\n").unwrap();
    fs::write(project.join("i18n/en.ftl"), "welcome.title = Welcome\n").unwrap();
    let autumn_toml = project.join("autumn.toml");
    let config = fs::read_to_string(&autumn_toml).unwrap();
    fs::write(
        &autumn_toml,
        format!("{config}\n[i18n]\ndefault_locale = \"fr\"\nsupported_locales = [\"fr\"]\n"),
    )
    .unwrap();

    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--i18n"],
    );

    let de = fs::read_to_string(project.join("i18n/de.ftl")).unwrap();
    let en = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    assert!(
        de.contains("post.new = New Post"),
        "the workspace config selects the validator bundle:\n{de}"
    );
    assert!(
        !en.contains("post.new"),
        "`en.ftl` is not what this build validates against:\n{en}"
    );
}

#[test]
fn the_extensionless_cargo_config_wins_as_cargo_says_it_does() {
    // With both present cargo warns "both `…/config` and `…/config.toml`
    // exist. Using `…/config`" and applies the extensionless one. Reading the
    // other takes a locale cargo never applies, and the scaffold then keeps the
    // wrong bundle in step.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "dual-config-app"]);
    let project = tmp.path().join("dual-config-app");
    fs::create_dir_all(project.join(".cargo")).unwrap();
    fs::write(
        project.join(".cargo/config"),
        "[env]\nAUTUMN_I18N_DEFAULT_LOCALE = \"de\"\n",
    )
    .unwrap();
    fs::write(
        project.join(".cargo/config.toml"),
        "[env]\nAUTUMN_I18N_DEFAULT_LOCALE = \"es\"\n",
    )
    .unwrap();
    fs::create_dir_all(project.join("i18n")).unwrap();
    fs::write(project.join("i18n/de.ftl"), "welcome.title = Willkommen\n").unwrap();
    fs::write(project.join("i18n/es.ftl"), "welcome.title = Bienvenido\n").unwrap();
    let autumn_toml = project.join("autumn.toml");
    let config = fs::read_to_string(&autumn_toml).unwrap();
    fs::write(
        &autumn_toml,
        format!("{config}\n[i18n]\ndefault_locale = \"fr\"\nsupported_locales = [\"fr\"]\n"),
    )
    .unwrap();

    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--i18n"],
    );

    let de = fs::read_to_string(project.join("i18n/de.ftl")).unwrap();
    let es = fs::read_to_string(project.join("i18n/es.ftl")).unwrap();
    assert!(
        de.contains("post.new = New Post"),
        "the extensionless config selects the validator bundle:\n{de}"
    );
    assert!(
        !es.contains("post.new"),
        "`config.toml` is the one cargo ignores here:\n{es}"
    );
}

#[test]
fn regeneration_keeps_a_live_key_in_the_validator_bundle_too() {
    // Holding a key back from the runtime prune is no use if the bundle `t!`
    // validates against loses it: the surviving call fails to compile anyway,
    // just from the other file. Both merges get the same protection.
    let (_tmp, project) = i18n_project(&[]);
    // `default_locale = "fr"` moves the runtime bundle to `i18n/fr.ftl` while
    // `i18n/en.ftl` stays the validator's — two different files.
    let autumn_toml = project.join("autumn.toml");
    let config = fs::read_to_string(&autumn_toml).unwrap();
    fs::write(
        &autumn_toml,
        config.replace("default_locale = \"en\"", "default_locale = \"fr\""),
    )
    .unwrap();
    fs::create_dir_all(project.join("src/components")).unwrap();
    fs::write(
        project.join("src/components/nav.rs"),
        "fn card(locale: &Locale) -> Markup { html! { (t!(locale, \"post.field.views\")) } }\n",
    )
    .unwrap();

    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "body:Text",
            "--i18n",
            "--force",
        ],
    );

    let en = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    let fr = fs::read_to_string(project.join("i18n/fr.ftl")).unwrap();
    // `en.ftl` is the one the original scaffold wrote and the one `t!` now
    // validates against, so it is where the dropped-but-still-called key lives.
    assert!(
        en.contains("post.field.views"),
        "the bundle `t!` validates against keeps the live key:\n{en}"
    );
    assert!(
        !en.contains("post.field.author_name"),
        "keys nothing calls are still pruned from it:\n{en}"
    );
    assert!(
        fr.contains("post.new"),
        "and the runtime bundle still gets this render's keys:\n{fr}"
    );
}

#[test]
fn the_validator_bundle_a_cargo_config_selects_is_kept_in_step() {
    // `t!` reads `AUTUMN_I18N_DEFAULT_LOCALE` to pick the bundle it validates
    // against, and a setting the macro needs on every build lives in
    // `.cargo/config.toml`. Syncing a hardcoded `i18n/en.ftl` leaves the file
    // the validator actually opens without the generated keys, so `cargo check`
    // fails with a `compile_error!` per lookup. Driven through the config file
    // rather than the process environment so the test stays hermetic.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "cfg-validator-app"]);
    let project = tmp.path().join("cfg-validator-app");
    fs::create_dir_all(project.join(".cargo")).unwrap();
    fs::write(
        project.join(".cargo/config.toml"),
        "[env]\nAUTUMN_I18N_DEFAULT_LOCALE = \"de\"\n",
    )
    .unwrap();
    fs::create_dir_all(project.join("i18n")).unwrap();
    // Both exist, so only the one the macro would open must be updated.
    fs::write(project.join("i18n/de.ftl"), "welcome.title = Willkommen\n").unwrap();
    fs::write(project.join("i18n/en.ftl"), "welcome.title = Welcome\n").unwrap();
    let autumn_toml = project.join("autumn.toml");
    let mut config = fs::read_to_string(&autumn_toml).unwrap();
    config.push_str("\n[i18n]\ndefault_locale = \"fr\"\nsupported_locales = [\"fr\"]\n");
    fs::write(&autumn_toml, config).unwrap();

    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "--i18n"],
    );

    let fr = fs::read_to_string(project.join("i18n/fr.ftl")).unwrap();
    let de = fs::read_to_string(project.join("i18n/de.ftl")).unwrap();
    let en = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    assert!(
        fr.contains("post.new = New Post"),
        "the runtime bundle still follows `autumn.toml`:\n{fr}"
    );
    assert!(
        de.contains("post.new = New Post"),
        "the bundle the validator opens must be kept in step:\n{de}"
    );
    assert!(
        de.contains("welcome.title = Willkommen"),
        "and its existing translations left alone:\n{de}"
    );
    assert!(
        !en.contains("post.new"),
        "`en.ftl` is not what this build validates against:\n{en}"
    );
}

#[test]
fn destroying_a_featureful_resource_prunes_only_the_chrome_it_alone_used() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "chrome-app"]);
    let project = tmp.path().join("chrome-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "--i18n",
            "--soft-delete",
            "--no-policy",
        ],
    );
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Comment", "body:Text", "--i18n"],
    );
    run_autumn_ok(
        &project,
        &[
            "destroy",
            "scaffold",
            "Post",
            "title:String",
            "--i18n",
            "--soft-delete",
            "--no-policy",
        ],
    );

    let ftl = fs::read_to_string(project.join("i18n/en.ftl")).unwrap();
    // Only Post had a Trash view, so its chrome goes…
    for gone in ["common.trash", "common.restore", "common.purge"] {
        assert!(
            !ftl.contains(gone),
            "`{gone}` has no call site left, so `--strict` fails on it:\n{ftl}"
        );
    }
    // …while the chrome the surviving Comment still uses stays.
    for kept in ["common.create", "common.save", "common.back"] {
        assert!(ftl.contains(kept), "`{kept}` is still in use:\n{ftl}");
    }
    assert!(ftl.contains("comment.new = New Comment"), "{ftl}");
    run_autumn_ok(&project, &["i18n", "check", "--strict"]);
}

#[test]
fn the_flag_changes_nothing_at_all_about_an_api_scaffold() {
    // Documented as a pure no-op for `--api`, which has to include the
    // compatibility refusals: otherwise a flag that changes nothing about the
    // output still changes whether it may exist.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "api-gate-app"]);
    let project = tmp.path().join("api-gate-app");
    for flags in [vec!["--api", "--i18n"], vec!["--api", "--i18n", "--live"]] {
        let mut args = vec!["generate", "scaffold", "Common", "title:String", "--force"];
        args.extend_from_slice(&flags);
        run_autumn_ok(&project, &args);
    }
    assert!(
        !project.join("i18n").exists(),
        "no bundle for an api scaffold"
    );
}

#[test]
fn an_embed_build_carries_the_bundle_too() {
    // `.i18n_auto()` loads from DISK. An `autumn build --embed` binary is meant
    // to be self-contained, and the release image ships no sidecar for it — so
    // converting an app to `--i18n` has to embed the bundle the same way
    // `autumn new --with-i18n` does, or the "self-contained" binary panics on a
    // missing default locale when run from an empty directory.
    let (_tmp, project) = i18n_project(&[]);
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(main_rs.contains("static EMBEDDED_LOCALES"), "{main_rs}");
    assert!(
        main_rs.contains("app.embedded_locales(&EMBEDDED_LOCALES)"),
        "{main_rs}"
    );
    assert_eq!(
        main_rs.matches("EMBEDDED_LOCALES").count(),
        2,
        "declared once, installed once — no duplicate wiring:\n{main_rs}"
    );

    // Byte-for-byte the same wiring `autumn new --with-i18n` produces.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "native", "--with-i18n"]);
    let native = fs::read_to_string(tmp.path().join("native/src/main.rs")).unwrap();
    for line in [
        "static EMBEDDED_LOCALES: autumn_web::include_dir::Dir = autumn_web::embed_locales!();",
        "    let app = app.embedded_locales(&EMBEDDED_LOCALES);",
        "        .i18n_auto()",
    ] {
        assert!(native.contains(line), "control lacks {line}:\n{native}");
        assert!(
            main_rs.contains(line),
            "converted app lacks {line}:\n{main_rs}"
        );
    }
}

#[test]
fn a_resource_that_would_collide_with_the_shared_chrome_is_refused() {
    // `Common`'s keys would land under `common.*` — the chrome namespace every
    // other resource references — so destroying it later would break them all.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "chrome-app"]);
    let project = tmp.path().join("chrome-app");
    let stderr = run_autumn_expect_fail(
        &project,
        &["generate", "scaffold", "Common", "title:String", "--i18n"],
    );
    assert!(
        stderr.contains("Common") && stderr.contains("common."),
        "the error must explain the namespace clash: {stderr}"
    );
    // Without `--i18n` the name is perfectly fine.
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Common", "title:String"],
    );
}

#[test]
fn the_delete_prompt_survives_a_translation_containing_a_quote_or_backslash() {
    // The prompt travels through a JavaScript string literal, and a translation
    // is not trusted input — it can come from a translator, a TMS, or a
    // crowdsourced `.ftl`. Escaping only apostrophes would leave a backslash to
    // close the literal early and turn the rest of the value into code.
    let (_tmp, project) = i18n_project(&[]);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("serde_json::to_string(&t!(locale, \"post.delete.confirm\"))"),
        "the prompt must be encoded as a JS string literal, not hand-escaped:\n{routes}"
    );
    assert!(
        !routes.contains(".replace('\\'',"),
        "a single-character escape is not a JS string encoder:\n{routes}"
    );
}

// ── Compile conformance ──────────────────────────────────────────────────
//
// The string assertions above prove the generator EMITTED a `t!` lookup; only a
// real `cargo check` proves the result is Rust that compiles. Both of the bugs
// this feature shipped with — a `&t!(…)` temporary borrowed into a `columns`
// vec that outlives it (E0716, `--soft-delete`), and a `transition_<field>`
// handler calling `show_view(locale.clone(), …)` without a `locale` parameter
// (E0425, `:states(...)`) — were invisible to grep and instant under `cargo
// check`. Adding the locale extractor also moves handlers toward axum's
// 16-argument `Handler` ceiling, which no amount of string matching can see.
//
// `#[ignore]`d (each compiles a fresh project) and therefore named explicitly
// in `.github/workflows/generator-conformance.yml` — an ignored test in
// `cli_tests` that the workflow does not name never runs anywhere.

/// Point the generated project's `autumn-web`/`autumn-macros` at this checkout,
/// so the compile proves the emitted code against the code in this PR rather
/// than the last published release.
fn patch_autumn_web_path(project: &Path) {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let mut cargo_toml = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    let web = workspace_root.join("autumn").display().to_string();
    let macros = workspace_root.join("autumn-macros").display().to_string();
    write!(
        cargo_toml,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\nautumn-macros = {{ path = \"{}\" }}\n",
        web.replace('\\', "/"),
        macros.replace('\\', "/"),
    )
    .expect("write to String is infallible");
    fs::write(project.join("Cargo.toml"), cargo_toml).unwrap();
}

fn assert_project_cargo_checks(project: &Path, what: &str) {
    patch_autumn_web_path(project);
    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(project)
        .output()
        .expect("failed to run cargo check");
    assert!(
        check.status.success(),
        "cargo check on the {what} --i18n scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// Generate one `--i18n` scaffold in a fresh app and return its project root.
fn i18n_scaffold(name: &str, fields: &[&str], flags: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec!["generate", "scaffold", "Post"];
    args.extend_from_slice(fields);
    args.push("--i18n");
    args.extend_from_slice(flags);
    run_autumn_ok(&project, &args);
    (tmp, project)
}

#[test]
#[ignore = "slow: compiles a generated project — run with `cargo test -p autumn-cli -- --ignored`"]
fn i18n_scaffold_cargo_checks() {
    // `title:String:unique` earns its place here: a unique column routes the
    // duplicate-value message through the bundle instead of `UNIQUE_CONSTRAINTS`
    // (issue #1349), which rebinds the `unique_violation_field` destructuring in
    // both the create and update handlers. Nothing but a real compile proves
    // those two still typecheck.
    // `archived:bool?` earns its place for the same reason: a nullable bool is
    // the one field whose CONTROL the scaffold overrides under `--i18n`, to
    // replace the derive's hardcoded `— Unset —`/`Yes`/`No`. The emitted
    // `vec![(String, String)]` has to typecheck against `FieldControl::Select`
    // with `t!` calls in it, which only a real compile proves.
    let (_tmp, project) = i18n_scaffold(
        "i18n-check",
        &[
            "title:String:unique",
            "body:Text",
            "published:bool",
            "archived:Option<bool>",
            "views:i64",
            "author_name:String",
        ],
        &[],
    );
    assert_project_cargo_checks(&project, "flat");
}

#[test]
#[ignore = "slow: compiles a generated project — run with `cargo test -p autumn-cli -- --ignored`"]
fn i18n_scaffold_with_trash_and_search_cargo_checks() {
    // `--soft-delete` with `--no-policy` is the combination that actually emits
    // the Trash view: an owner-scoped index gates it off, which is precisely how
    // the E0716 in its two extra `Column`s escaped the string-only tests.
    let (_tmp, project) = i18n_scaffold(
        "i18n-check-trash",
        &["title:String", "body:Text"],
        &["--soft-delete", "--searchable", "title,body", "--no-policy"],
    );
    assert_project_cargo_checks(&project, "soft-delete + searchable");
}

#[test]
#[ignore = "slow: compiles a generated project — run with `cargo test -p autumn-cli -- --ignored`"]
fn i18n_scaffold_with_owner_scoped_search_cargo_checks() {
    // An owner column puts the `search` handler within one argument of axum's
    // 16-extractor `Handler` ceiling; adding `locale: Locale` went over it and
    // failed with an unreadable elided-tuple trait error.
    let (_tmp, project) = i18n_scaffold(
        "i18n-check-owner",
        &["title:String", "body:Text", "user_id:i64"],
        &["--searchable", "title,body"],
    );
    assert_project_cargo_checks(&project, "owner-scoped searchable");
}

#[test]
#[ignore = "slow: compiles a generated project — run with `cargo test -p autumn-cli -- --ignored`"]
fn i18n_scaffold_with_state_machine_and_attachment_cargo_checks() {
    // A `:states(...)` column emits `transition_<field>` handlers that render
    // the shared `show_view`, and an `Attachment` column adds the "Current
    // <Field>:" label plus the multipart create/update path — two more places
    // the locale has to reach.
    let (_tmp, project) = i18n_scaffold(
        "i18n-check-states",
        &[
            "title:String",
            "status:String:states(draft -> published, published -> archived)",
            "cover:Attachment",
        ],
        &[],
    );
    assert_project_cargo_checks(&project, "state machine + attachment");

    // A lock column adds the transition handler's 409 branch, which re-renders
    // the shared `show_view` a second time and flashes a stale-write notice.
    // (Kept separate: `lock_version` alongside an `Attachment` column is refused
    // for reasons that predate this flag.)
    let (_tmp_lock, project_lock) = i18n_scaffold(
        "i18n-check-lock",
        &[
            "title:String",
            "status:String:states(draft -> published, published -> archived)",
            "lock_version:i32",
        ],
        &[],
    );
    assert_project_cargo_checks(&project_lock, "state machine + optimistic lock");
}
