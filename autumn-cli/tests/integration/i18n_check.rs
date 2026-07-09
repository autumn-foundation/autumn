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
