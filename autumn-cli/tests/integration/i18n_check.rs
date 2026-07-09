//! Integration tests for `autumn i18n check` (issue #1252).
//!
//! Each test scaffolds a throwaway project (an `autumn.toml` `[i18n]` block,
//! `i18n/<locale>.ftl` files, and `src/*.rs` call sites) in a `TempDir`, runs
//! the real `autumn` binary against it, and asserts on the exit code and
//! output. The fixture `.rs` files are only ever *parsed* by the scanner, so
//! they need to be syntactically valid Rust but need not compile or link.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// Write a file, creating parent directories as needed.
fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Run `autumn i18n check [args...]` inside `root`.
fn run_check(root: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("i18n")
        .arg("check")
        .args(args)
        .current_dir(root)
        .output()
        .expect("failed to run autumn i18n check")
}

/// Run `autumn i18n check [args...]` inside `root` with `AUTUMN_ENV` set, so
/// profile overlays (`[profile.<env>.i18n]` / `autumn-<env>.toml`) are applied.
/// `AUTUMN_ENV` is scoped to the spawned child process, so no test-process
/// global env is mutated.
fn run_check_with_env(root: &Path, autumn_env: &str, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("i18n")
        .arg("check")
        .args(args)
        .current_dir(root)
        .env("AUTUMN_ENV", autumn_env)
        .output()
        .expect("failed to run autumn i18n check")
}

const AUTUMN_TOML: &str = "\
[i18n]
default_locale = \"en\"
supported_locales = [\"en\", \"es\"]
";

/// Two call sites: a `t!` macro key and a `.t(...)` method key.
const CALL_SITES: &str = r#"
fn view(locale: &Locale) -> String {
    let a = t!(locale, "nav.home");
    let b = locale.t("nav.about");
    format!("{a}{b}")
}
"#;

fn clean_project() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "autumn.toml", AUTUMN_TOML);
    write(root, "src/main.rs", CALL_SITES);
    write(root, "i18n/en.ftl", "nav.home = Home\nnav.about = About\n");
    write(
        root,
        "i18n/es.ftl",
        "nav.home = Inicio\nnav.about = Acerca\n",
    );
    dir
}

#[test]
fn clean_project_exits_zero() {
    let dir = clean_project();
    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "clean project should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("PASS"), "stdout:\n{stdout}");
}

#[test]
fn missing_key_exits_nonzero_and_names_key_and_locale() {
    let dir = clean_project();
    // Delete `nav.about` from the Spanish locale so it is referenced in code
    // but missing from `es` (and its fallback chain is just `en`... which has
    // it — so to force a real miss, drop it from BOTH is wrong. `es` alone
    // missing it, with default `en` supplying it, is a *fallback hit*, not a
    // miss). Delete it everywhere to make it genuinely missing.
    write(dir.path(), "i18n/en.ftl", "nav.home = Home\n");
    write(dir.path(), "i18n/es.ftl", "nav.home = Inicio\n");

    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "missing key should exit non-zero\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("nav.about"),
        "should name the key\n{stdout}"
    );
    assert!(
        stdout.contains("Missing"),
        "should label it Missing\n{stdout}"
    );
    assert!(stdout.contains("en"), "should name the locale\n{stdout}");
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn fallback_locale_supplies_key_so_not_missing() {
    // `es` lacks `nav.about`, but the default locale `en` supplies it via the
    // resolved fallback chain, so `es` must NOT be reported missing.
    let dir = clean_project();
    write(dir.path(), "i18n/es.ftl", "nav.home = Inicio\n");

    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "fallback should keep exit 0\nstdout:\n{stdout}"
    );
    // It IS untranslated (present in `en`, absent from `es`), a warning only.
    assert!(stdout.contains("Untranslated"), "stdout:\n{stdout}");
}

#[test]
fn unused_key_is_warning_by_default_and_error_under_strict() {
    let dir = clean_project();
    // Add a defined-but-never-referenced key to the default locale.
    write(
        dir.path(),
        "i18n/en.ftl",
        "nav.home = Home\nnav.about = About\nfooter.legacy = Old\n",
    );

    let default_run = run_check(dir.path(), &[]);
    let default_stdout = String::from_utf8_lossy(&default_run.stdout);
    assert!(
        default_run.status.success(),
        "unused key alone should exit 0 by default\nstdout:\n{default_stdout}"
    );
    assert!(
        default_stdout.contains("footer.legacy"),
        "should list the unused key\n{default_stdout}"
    );
    assert!(
        default_stdout.contains("Unused"),
        "stdout:\n{default_stdout}"
    );

    let strict_run = run_check(dir.path(), &["--strict"]);
    assert!(
        !strict_run.status.success(),
        "unused key should fail under --strict\nstdout:\n{}",
        String::from_utf8_lossy(&strict_run.stdout)
    );
}

#[test]
fn dynamic_keys_are_listed_not_flagged() {
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale, section: &str) -> String {
    let a = t!(locale, "nav.home");
    let b = locale.t("nav.about");
    let c = locale.t(&format!("nav.{section}"));
    format!("{a}{b}{c}")
}
"#,
    );

    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "dynamic key must not cause a failure\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("dynamic (not checked): 1"),
        "should report the dynamic call site\n{stdout}"
    );
    assert!(
        stdout.contains("format"),
        "should show the snippet\n{stdout}"
    );
}

#[test]
fn dynamic_prefix_suppresses_matching_unused_but_not_others() {
    // A dynamic site `locale.t(&format!("status.{state}"))` records the static
    // prefix `status.`, so the defined `status.open`/`status.closed` keys must
    // NOT be flagged Unused (and `--strict` still passes). An unrelated
    // `footer.legacy` (matching no prefix) IS still Unused and fails `--strict`.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale, state: &str) -> String {
    let a = t!(locale, "nav.home");
    let b = locale.t("nav.about");
    let c = locale.t(&format!("status.{state}"));
    format!("{a}{b}{c}")
}
"#,
    );
    write(
        dir.path(),
        "i18n/en.ftl",
        "nav.home = Home\nnav.about = About\nstatus.open = Open\nstatus.closed = Closed\n",
    );
    write(
        dir.path(),
        "i18n/es.ftl",
        "nav.home = Inicio\nnav.about = Acerca\nstatus.open = Abierto\nstatus.closed = Cerrado\n",
    );

    // status.* is covered by the dynamic prefix → no Unused → --strict passes.
    let strict = run_check(dir.path(), &["--strict"]);
    let strict_stdout = String::from_utf8_lossy(&strict.stdout);
    assert!(
        strict.status.success(),
        "status.* keys are covered by the `status.` dynamic prefix, so --strict must pass\nstdout:\n{strict_stdout}"
    );
    assert!(
        !strict_stdout.contains("status.open") && !strict_stdout.contains("status.closed"),
        "status.* keys must not be reported Unused\n{strict_stdout}"
    );

    // Add a genuinely-unused key matching no prefix: --strict must now fail.
    write(
        dir.path(),
        "i18n/en.ftl",
        "nav.home = Home\nnav.about = About\nstatus.open = Open\nstatus.closed = Closed\nfooter.legacy = Old\n",
    );
    let unrelated = run_check(dir.path(), &["--strict"]);
    let unrelated_out = String::from_utf8_lossy(&unrelated.stdout);
    assert!(
        !unrelated.status.success(),
        "a key matching no dynamic prefix must still fail --strict\nstdout:\n{unrelated_out}"
    );
    assert!(
        unrelated_out.contains("footer.legacy"),
        "should list the genuinely-unused key\n{unrelated_out}"
    );
}

#[test]
fn wrapped_literal_key_is_referenced_so_deleting_it_is_missing() {
    // A literal key passed through a borrow or a parenthesized group —
    // `locale.t(&"nav.about")` / `locale.t(("nav.about"))` — must be recorded as
    // a referenced key. Deleting it from every locale must then surface as
    // Missing (non-zero exit), not silently pass because the site was misread as
    // a dynamic (unanalyzable) key.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale) -> String {
    let a = t!(locale, "nav.home");
    let b = locale.t(&"nav.about");
    format!("{a}{b}")
}
"#,
    );
    // Drop `nav.about` from every locale so the referenced key is genuinely
    // missing across the fallback chain.
    write(dir.path(), "i18n/en.ftl", "nav.home = Home\n");
    write(dir.path(), "i18n/es.ftl", "nav.home = Inicio\n");

    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a wrapped literal key deleted from the .ftl must be Missing (non-zero exit)\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("nav.about"),
        "should name the wrapped literal key\n{stdout}"
    );
    assert!(
        stdout.contains("Missing"),
        "should label it Missing\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn nested_translation_call_in_args_is_referenced_so_deleting_it_is_missing() {
    // A translation call nested in another call's arguments —
    // `t_with("message", &[("status", &locale.t("status.open"))])` — must record
    // BOTH keys. Deleting the nested `status.open` from every locale must surface
    // as Missing (non-zero exit), not silently pass because the outer branch
    // skipped the argument group.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale) -> String {
    locale.t_with("message", &[("status", &locale.t("status.open"))])
}
"#,
    );
    // Define both keys, then drop the nested `status.open` from every locale so
    // it is genuinely missing across the fallback chain while `message` stays.
    write(
        dir.path(),
        "i18n/en.ftl",
        "message = Status is { $status }\n",
    );
    write(
        dir.path(),
        "i18n/es.ftl",
        "message = Estado es { $status }\n",
    );

    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a nested translation key deleted from the .ftl must be Missing (non-zero exit)\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("status.open"),
        "should name the nested key\n{stdout}"
    );
    assert!(
        stdout.contains("Missing"),
        "should label it Missing\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn generic_arg_before_key_keeps_key_in_slot_so_deleting_it_is_missing() {
    // When an earlier argument carries a turbofish with more than one type
    // parameter — `t!(locale_for::<A, B>(), "nav.about")` — the comma inside
    // `<A, B>` is not wrapped in a group. A naive comma splitter would shift the
    // literal key out of its slot and never record it, letting a genuinely
    // missing translation pass. The grammar-aware split keeps the key in slot,
    // so deleting it from every locale surfaces as Missing (non-zero exit).
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale) -> String {
    let a = t!(locale, "nav.home");
    let b = t!(locale_for::<A, B>(), "nav.about");
    format!("{a}{b}")
}
"#,
    );
    // Drop `nav.about` from every locale so the referenced key is genuinely
    // missing across the fallback chain.
    write(dir.path(), "i18n/en.ftl", "nav.home = Home\n");
    write(dir.path(), "i18n/es.ftl", "nav.home = Inicio\n");

    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a key behind a multi-param generic argument must still be checked \
         (Missing), not shifted out of its slot\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("nav.about"),
        "should name the key kept in its slot despite the generic\n{stdout}"
    );
    assert!(
        stdout.contains("Missing"),
        "should label it Missing\n{stdout}"
    );
    assert!(
        !stdout.contains("dynamic (not checked): 1"),
        "the `<A, B>` fragment must not create a bogus dynamic site\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn wrapped_literal_with_trailing_tokens_is_dynamic_not_a_false_missing() {
    // A parenthesized literal that CONTINUES after the group —
    // `locale.t((" nav.home ").trim())` — is a derived expression, not a literal
    // key. The runtime looks up the post-`trim` value (`nav.home`). The scanner
    // must NOT record the raw wrapped literal `" nav.home "` (spaces included) as
    // a referenced key: doing so would flag a false Missing (`" nav.home "` is
    // absent from the `.ftl`) paired with a false Unused (`nav.home` looks
    // unreferenced) even though the translations are correct. Instead the site is
    // treated as dynamic and the check passes.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale) -> String {
    locale.t((" nav.home ").trim()).into()
}
"#,
    );
    // `nav.home` IS defined in every locale — the correct key the runtime uses.
    write(dir.path(), "i18n/en.ftl", "nav.home = Home\n");
    write(dir.path(), "i18n/es.ftl", "nav.home = Inicio\n");

    // Even under --strict the check must pass: no false Missing, and the
    // fully-dynamic site suppresses Unused reporting.
    let output = run_check(dir.path(), &["--strict"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a `(...).trim()` site must be dynamic, not a false Missing/Unused pair\nstdout:\n{stdout}"
    );
    // No Missing key was recorded (the `✗` marker never appears): the raw
    // wrapped literal `" nav.home "` was NOT lifted into the referenced set. Its
    // only appearance in the output is the dynamic call-site snippet.
    assert!(
        !stdout.contains('✗'),
        "no Missing key must be reported for the `(...).trim()` site\n{stdout}"
    );
    assert!(
        stdout.contains("0 referenced key(s)"),
        "the `(...).trim()` site must not contribute a referenced key\n{stdout}"
    );
    assert!(
        stdout.contains("dynamic (not checked): 1"),
        "the `(...).trim()` site must be reported as dynamic\n{stdout}"
    );
    assert!(stdout.contains("PASS"), "stdout:\n{stdout}");
}

#[test]
fn path_qualified_format_prefix_is_recovered_not_treated_as_fully_dynamic() {
    // A path-qualified format macro — `locale.t(&std::format!("status.{state}"))`
    // — must be recognized as `format!` and record prefix `status.`, NOT leave
    // the stream starting with `std` and be misread as a fully-dynamic
    // (empty-prefix) site that suppresses every Unused warning project-wide. So
    // `status.*` is covered, but an unrelated stale `footer.legacy` still fails
    // `--strict`.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale, state: &str) -> String {
    let a = t!(locale, "nav.home");
    let b = locale.t("nav.about");
    let c = locale.t(&std::format!("status.{state}"));
    format!("{a}{b}{c}")
}
"#,
    );
    write(
        dir.path(),
        "i18n/en.ftl",
        "nav.home = Home\nnav.about = About\nstatus.open = Open\nstatus.closed = Closed\nfooter.legacy = Old\n",
    );
    write(
        dir.path(),
        "i18n/es.ftl",
        "nav.home = Inicio\nnav.about = Acerca\nstatus.open = Abierto\nstatus.closed = Cerrado\nfooter.legacy = Viejo\n",
    );

    let strict = run_check(dir.path(), &["--strict"]);
    let strict_stdout = String::from_utf8_lossy(&strict.stdout);
    assert!(
        !strict.status.success(),
        "footer.legacy matches no prefix, so --strict must fail (path-qualified format! must not suppress all Unused)\nstdout:\n{strict_stdout}"
    );
    assert!(
        !strict_stdout.contains("Unused checking suppressed"),
        "a path-qualified format! has a static prefix and must NOT suppress Unused wholesale\n{strict_stdout}"
    );
    assert!(
        strict_stdout.contains("footer.legacy"),
        "should list the genuinely-unused key\n{strict_stdout}"
    );
    assert!(
        !strict_stdout.contains("status.open") && !strict_stdout.contains("status.closed"),
        "status.* keys must be suppressed by the path-qualified `status.` dynamic prefix\n{strict_stdout}"
    );
}

#[test]
fn wrapped_dynamic_prefix_is_recovered_not_treated_as_fully_dynamic() {
    // A dynamic key wrapped in an extra group behind a borrow —
    // `locale.t(&(format!("status.{state}")))` — must still expose the leading
    // `format!` and record prefix `status.`, NOT be misread as a fully-dynamic
    // (empty-prefix) site that suppresses every Unused warning project-wide.
    // So `status.*` is covered, but an unrelated stale `footer.legacy` still
    // fails `--strict`.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale, state: &str) -> String {
    let a = t!(locale, "nav.home");
    let b = locale.t("nav.about");
    let c = locale.t(&(format!("status.{state}")));
    format!("{a}{b}{c}")
}
"#,
    );
    write(
        dir.path(),
        "i18n/en.ftl",
        "nav.home = Home\nnav.about = About\nstatus.open = Open\nstatus.closed = Closed\nfooter.legacy = Old\n",
    );
    write(
        dir.path(),
        "i18n/es.ftl",
        "nav.home = Inicio\nnav.about = Acerca\nstatus.open = Abierto\nstatus.closed = Cerrado\nfooter.legacy = Viejo\n",
    );

    let strict = run_check(dir.path(), &["--strict"]);
    let strict_stdout = String::from_utf8_lossy(&strict.stdout);
    assert!(
        !strict.status.success(),
        "footer.legacy matches no prefix, so --strict must fail (wrapped format! must not suppress all Unused)\nstdout:\n{strict_stdout}"
    );
    assert!(
        !strict_stdout.contains("Unused checking suppressed"),
        "a wrapped format! has a static prefix and must NOT suppress Unused wholesale\n{strict_stdout}"
    );
    assert!(
        strict_stdout.contains("footer.legacy"),
        "should list the genuinely-unused key\n{strict_stdout}"
    );
    assert!(
        !strict_stdout.contains("status.open") && !strict_stdout.contains("status.closed"),
        "status.* keys must be suppressed by the wrapped `status.` dynamic prefix\n{strict_stdout}"
    );
}

#[test]
fn associated_dynamic_prefix_records_key_arg_not_receiver() {
    // The associated call form `Locale::t(&locale, &format!("status.{state}"))`
    // puts the receiver at argument 0 and the KEY at argument 1. The scanner
    // must classify the key argument, deriving prefix `status.` — not the
    // receiver, which would yield an empty prefix and suppress every Unused
    // warning project-wide. So `status.*` is covered but an unrelated
    // `footer.legacy` still fails `--strict`.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale, state: &str) -> String {
    let a = Locale::t(&locale, "nav.home");
    let b = Locale::t(&locale, "nav.about");
    let c = Locale::t(&locale, &format!("status.{state}"));
    format!("{a}{b}{c}")
}
"#,
    );
    write(
        dir.path(),
        "i18n/en.ftl",
        "nav.home = Home\nnav.about = About\nstatus.open = Open\nstatus.closed = Closed\nfooter.legacy = Old\n",
    );
    write(
        dir.path(),
        "i18n/es.ftl",
        "nav.home = Inicio\nnav.about = Acerca\nstatus.open = Abierto\nstatus.closed = Cerrado\nfooter.legacy = Viejo\n",
    );

    // `status.*` is covered by the associated dynamic prefix, but `footer.legacy`
    // matches no prefix → still Unused → `--strict` must fail (the receiver was
    // NOT mistaken for a fully-dynamic empty-prefix key).
    let strict = run_check(dir.path(), &["--strict"]);
    let strict_stdout = String::from_utf8_lossy(&strict.stdout);
    assert!(
        !strict.status.success(),
        "footer.legacy matches no prefix, so --strict must fail (receiver must not suppress all Unused)\nstdout:\n{strict_stdout}"
    );
    assert!(
        strict_stdout.contains("footer.legacy"),
        "should list the genuinely-unused key\n{strict_stdout}"
    );
    assert!(
        !strict_stdout.contains("status.open") && !strict_stdout.contains("status.closed"),
        "status.* keys must be suppressed by the associated `status.` dynamic prefix\n{strict_stdout}"
    );
}

#[test]
fn fully_dynamic_site_suppresses_unused_reporting() {
    // A bare-variable key site `locale.t(&key)` has no static prefix and could
    // reference any key, so Unused reporting is suppressed entirely: even an
    // otherwise-unused key must not fail `--strict`, and the output notes it.
    let dir = clean_project();
    write(
        dir.path(),
        "src/main.rs",
        r#"
fn view(locale: &Locale, key: &str) -> String {
    let a = t!(locale, "nav.home");
    let b = locale.t("nav.about");
    let c = locale.t(&key);
    format!("{a}{b}{c}")
}
"#,
    );
    // `footer.legacy` is defined but never statically referenced.
    write(
        dir.path(),
        "i18n/en.ftl",
        "nav.home = Home\nnav.about = About\nfooter.legacy = Old\n",
    );
    write(
        dir.path(),
        "i18n/es.ftl",
        "nav.home = Inicio\nnav.about = Acerca\nfooter.legacy = Viejo\n",
    );

    let output = run_check(dir.path(), &["--strict"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a fully-dynamic key site suppresses Unused, so --strict must pass\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Unused checking suppressed"),
        "output should note the suppression\n{stdout}"
    );
}

#[test]
fn json_format_emits_machine_readable_report() {
    let dir = clean_project();
    write(dir.path(), "i18n/en.ftl", "nav.home = Home\n");
    write(dir.path(), "i18n/es.ftl", "nav.home = Inicio\n");

    let output = run_check(dir.path(), &["--format", "json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "missing key still exits non-zero in json mode\nstdout:\n{stdout}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"));
    assert_eq!(json["default_locale"], "en");
    let locales = json["locales"].as_array().expect("locales array");
    let missing_anywhere = locales
        .iter()
        .flat_map(|l| l["missing"].as_array().cloned().unwrap_or_default())
        .any(|k| k == "nav.about");
    assert!(missing_anywhere, "nav.about should be missing\n{stdout}");
}

#[test]
fn profile_overlay_selects_prod_locale_dir_under_autumn_env() {
    // The base `[i18n]` block points at `i18n/`, whose `en.ftl` has every
    // referenced key (so the default/dev check PASSes). A `[profile.prod.i18n]`
    // overlay redirects the check at `i18n-prod/`, whose `en.ftl` is *missing*
    // `nav.about` — proving that under `AUTUMN_ENV=prod` the command honors the
    // production overlay (locale dir + config) instead of the base defaults.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "autumn.toml",
        "\
[i18n]
default_locale = \"en\"
supported_locales = [\"en\"]
dir = \"i18n\"

[profile.prod.i18n]
default_locale = \"en\"
supported_locales = [\"en\"]
dir = \"i18n-prod\"
",
    );
    write(root, "src/main.rs", CALL_SITES);
    // Base locale dir: complete.
    write(root, "i18n/en.ftl", "nav.home = Home\nnav.about = About\n");
    // Prod locale dir: `nav.about` is genuinely missing.
    write(root, "i18n-prod/en.ftl", "nav.home = Home\n");

    // Without AUTUMN_ENV: base `i18n/` is inspected → all keys present → PASS.
    let base = run_check(root, &[]);
    let base_stdout = String::from_utf8_lossy(&base.stdout);
    assert!(
        base.status.success(),
        "base (dev) check should PASS using i18n/\nstdout:\n{base_stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&base.stderr)
    );
    assert!(base_stdout.contains("PASS"), "stdout:\n{base_stdout}");

    // With AUTUMN_ENV=prod: the `[profile.prod.i18n]` overlay redirects the
    // check at `i18n-prod/`, where `nav.about` is missing → FAIL.
    let prod = run_check_with_env(root, "prod", &[]);
    let prod_stdout = String::from_utf8_lossy(&prod.stdout);
    assert!(
        !prod.status.success(),
        "prod overlay should FAIL: i18n-prod/ is missing nav.about\nstdout:\n{prod_stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&prod.stderr)
    );
    assert!(
        prod_stdout.contains("nav.about"),
        "should name the key missing from the prod locale dir\n{prod_stdout}"
    );
    assert!(
        prod_stdout.contains("Missing"),
        "should label it Missing under the prod overlay\n{prod_stdout}"
    );
    assert!(prod_stdout.contains("FAIL"), "stdout:\n{prod_stdout}");
}

#[test]
fn no_i18n_directory_is_a_noop_pass() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "autumn.toml", "[server]\n");
    write(dir.path(), "src/main.rs", "fn main() {}\n");

    let output = run_check(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a project without i18n should pass\nstdout:\n{stdout}"
    );
    assert!(stdout.contains("nothing to check"), "stdout:\n{stdout}");
}

#[test]
fn configured_i18n_with_missing_directory_fails() {
    // The project explicitly configures `[i18n]` but the resolved directory is
    // absent. This is a misconfiguration: at startup `.i18n_auto()` would call
    // `Bundle::load_from_dir`, which reports `MissingDefaultLocale`. The check
    // must mirror that (non-zero exit), NOT skip to a false CI pass.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "autumn.toml",
        "\
[i18n]
default_locale = \"en\"
supported_locales = [\"en\"]
dir = \"i18n\"
",
    );
    write(root, "src/main.rs", "fn main() {}\n");
    // Note: no `i18n/` directory is created.

    let output = run_check(root, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a configured but missing i18n directory must fail, not skip\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("nothing to check"),
        "must not report the no-op skip when i18n is configured\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("default locale") && stderr.contains("missing"),
        "should surface the missing-default-locale bundle error\nstderr:\n{stderr}"
    );
}

#[test]
fn override_file_alias_precedence_matches_runtime_first_existing_wins() {
    // Runtime config loading merges only the FIRST existing profile override
    // file (`autumn-prod.toml` before `autumn-production.toml` under
    // `AUTUMN_ENV=prod`; see `profile_override_file_lookup_names` + the `break`
    // in `autumn/src/config.rs`). Here `autumn-prod.toml` — the one the runtime
    // would actually load — has NO `[i18n]`, while the never-loaded
    // `autumn-production.toml` DOES. With no `i18n/` directory, the project has
    // no active i18n config in scope, so the check must take the no-op skip
    // (exit 0) rather than consulting the unloaded alias and failing.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Base config declares no i18n.
    write(root, "autumn.toml", "[server]\n");
    write(root, "src/main.rs", "fn main() {}\n");
    // The override file the runtime actually loads (first existing) — no i18n.
    write(root, "autumn-prod.toml", "[log]\nlevel = \"info\"\n");
    // A later alias that is never loaded, but DOES configure i18n. The check
    // must not consult it.
    write(
        root,
        "autumn-production.toml",
        "\
[i18n]
default_locale = \"en\"
supported_locales = [\"en\"]
dir = \"i18n\"
",
    );
    // No `i18n/` directory exists.

    let output = run_check_with_env(root, "prod", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "only the first-existing override file (autumn-prod.toml, no i18n) is \
         loaded → no active i18n config → no-op skip (exit 0)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("nothing to check"),
        "should report the no-op skip, not consult the unloaded autumn-production.toml\nstdout:\n{stdout}"
    );
}

#[test]
fn profile_overlay_with_missing_directory_fails_under_autumn_env() {
    // No base `[i18n]`, so under dev the project is a genuine no-op (PASS). A
    // `[profile.prod.i18n]` overlay points at a directory that does not exist,
    // so under `AUTUMN_ENV=prod` the check must fail exactly as the prod app
    // would at startup — the missing-directory skip must not mask it.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "autumn.toml",
        "\
[server]

[profile.prod.i18n]
default_locale = \"en\"
supported_locales = [\"en\"]
dir = \"i18n-prod\"
",
    );
    write(root, "src/main.rs", "fn main() {}\n");
    // Neither `i18n/` nor `i18n-prod/` exists.

    // Dev: no i18n config in scope → genuine no-op PASS.
    let dev = run_check(root, &[]);
    let dev_stdout = String::from_utf8_lossy(&dev.stdout);
    assert!(
        dev.status.success(),
        "dev has no i18n config in scope and should skip-PASS\nstdout:\n{dev_stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&dev.stderr)
    );
    assert!(
        dev_stdout.contains("nothing to check"),
        "dev should report the no-op skip\nstdout:\n{dev_stdout}"
    );

    // Prod: the `[profile.prod.i18n]` overlay configures i18n but `i18n-prod/`
    // is absent → fail with the bundle loader's error.
    let prod = run_check_with_env(root, "prod", &[]);
    let prod_stderr = String::from_utf8_lossy(&prod.stderr);
    assert!(
        !prod.status.success(),
        "prod overlay configures i18n but the dir is missing → must fail\nstdout:\n{}\nstderr:\n{prod_stderr}",
        String::from_utf8_lossy(&prod.stdout)
    );
    assert!(
        prod_stderr.contains("default locale") && prod_stderr.contains("missing"),
        "should surface the missing-default-locale bundle error under prod\nstderr:\n{prod_stderr}"
    );
}
