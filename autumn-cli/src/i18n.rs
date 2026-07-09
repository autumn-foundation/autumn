//! `autumn i18n check` — compare translation keys referenced in code against
//! the keys defined in each `i18n/<locale>.ftl` (Fluent) file.
//!
//! autumn ships first-class Fluent i18n (`Locale::t("key")`, `t_with(...)`,
//! and the [`t!`] macro), but the only signal that a key is missing from a
//! locale is a *runtime* miss ([`Bundle::miss_count`] increments and a warning
//! fires when a real user hits the page). This command surfaces those problems
//! at build/CI time instead:
//!
//! - **Missing** — a key referenced in code that is absent from a locale's
//!   resolved fallback chain (consistent with
//!   [`I18nConfig::resolved_fallback_chain`]). This is the correctness
//!   failure: it drives a non-zero exit.
//! - **Untranslated** — a key present in the default locale but absent from a
//!   non-default locale. A warning (error under `--strict`).
//! - **Unused** — a key defined in a `.ftl` with no matching call site in
//!   code. A warning (error under `--strict`).
//!
//! The scanner statically extracts string-literal keys from `t!(...)`,
//! `.t(...)`, and `.t_with(...)` call sites via [`syn`]. Keys built at runtime
//! (e.g. `t(&format!("nav.{section}"))`) are not analyzable; they are reported
//! separately as "dynamic — not checked" so they neither cause false
//! `Missing` results nor silently suppress real ones.
//!
//! The i18n bundle is loaded through the existing [`Bundle::load_from_dir`]
//! loader, so a `.ftl` syntax error or a missing default locale fails the
//! command exactly as it would fail the app at startup.
//!
//! [`Bundle::load_from_dir`]: autumn_web::i18n::Bundle::load_from_dir
//! [`Bundle::miss_count`]: autumn_web::i18n::Bundle::miss_count
//! [`I18nConfig::resolved_fallback_chain`]: autumn_web::i18n::I18nConfig::resolved_fallback_chain

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use autumn_web::i18n::{Bundle, I18nConfig};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens as _;
use serde::Serialize;
use syn::parse::Parser as _;
use syn::spanned::Spanned as _;

/// Output format for `autumn i18n check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable text (default).
    Text,
    /// Machine-readable JSON for `autumn check` / CI consumption.
    Json,
}

/// Options parsed from CLI flags.
#[derive(Debug, Clone, Copy)]
pub struct I18nCheckOptions {
    pub format: OutputFormat,
    /// Treat **Untranslated**/**Unused** warnings as failures (exit non-zero).
    pub strict: bool,
}

/// A `t(...)`/`t_with(...)`/`t!(...)` call site whose key is not a string
/// literal and therefore cannot be checked statically.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DynamicSite {
    /// Source file, relative to the project root.
    pub file: String,
    /// 1-based line number of the call site.
    pub line: usize,
    /// The offending key expression, as source text.
    pub snippet: String,
}

/// Per-locale findings.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocaleReport {
    pub locale: String,
    pub is_default: bool,
    /// Referenced-in-code keys absent from this locale's fallback chain.
    pub missing: Vec<String>,
    /// Keys present in the default locale but absent from this (non-default)
    /// locale. Always empty for the default locale.
    pub untranslated: Vec<String>,
    /// Keys defined in this locale's `.ftl` with no call site in code.
    pub unused: Vec<String>,
}

/// The full result of a check run.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    pub default_locale: String,
    pub fallback_chain: Vec<String>,
    /// Number of distinct string-literal keys referenced in code.
    pub referenced_keys: usize,
    /// Number of `.rs` files scanned.
    pub files_scanned: usize,
    /// Call sites with non-literal keys (not analyzable).
    pub dynamic: Vec<DynamicSite>,
    pub locales: Vec<LocaleReport>,
}

impl Report {
    /// Any locale with a **Missing** key (the correctness failure).
    #[must_use]
    pub fn has_missing(&self) -> bool {
        self.locales.iter().any(|l| !l.missing.is_empty())
    }

    /// Any **Untranslated**/**Unused** warnings across locales.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.locales
            .iter()
            .any(|l| !l.untranslated.is_empty() || !l.unused.is_empty())
    }

    /// Process exit code: non-zero when any locale has Missing keys, or (under
    /// `--strict`) any Untranslated/Unused warnings.
    #[must_use]
    pub fn exit_code(&self, strict: bool) -> i32 {
        i32::from(self.has_missing() || (strict && self.has_warnings()))
    }
}

// ── Referenced-key scanner ────────────────────────────────────────────────

/// Result of scanning a source tree for `t`/`t_with` call sites.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Distinct string-literal keys referenced in code.
    pub referenced: BTreeSet<String>,
    /// Call sites whose key is not a string literal.
    pub dynamic: Vec<DynamicSite>,
    /// Number of `.rs` files parsed.
    pub files_scanned: usize,
}

/// Walk `root` recursively and extract referenced translation keys from every
/// `.rs` file, skipping `target/`, `.git/`, and other hidden directories.
#[must_use]
pub fn scan_project(root: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    files.sort();
    for path in files {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        scan_source(&src, &rel, &mut result);
        result.files_scanned += 1;
    }
    result
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // Skip build artifacts, VCS metadata, and hidden dirs.
            if name == "target" || name.starts_with('.') {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Tokenize one Rust source string and record referenced/dynamic keys.
///
/// The scan runs over the raw token tree rather than the parsed AST so that
/// `t!(...)` / `.t(...)` call sites **nested inside other macros** — most
/// importantly `maud::html! { ... }`, where essentially all view-layer
/// translations live — are still found. A `syn`-`Visit` walk would stop at the
/// outer `html!` invocation and never see the `t!` calls inside its token
/// stream. A file that fails to tokenize is skipped silently.
fn scan_source(src: &str, file: &str, result: &mut ScanResult) {
    let Ok(stream) = TokenStream::from_str(src) else {
        return;
    };
    scan_stream(&stream, file, result);
}

/// Recursively walk a token stream, descending into every delimited group
/// (including macro bodies), and record any `t!` / `t` / `t_with` call site.
fn scan_stream(stream: &TokenStream, file: &str, result: &mut ScanResult) {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    let mut i = 0;
    while i < trees.len() {
        if let TokenTree::Ident(ident) = &trees[i] {
            let name = ident.to_string();

            // Macro form: `t ! ( <locale>, "key" [, args] )`.
            if name == "t"
                && matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!')
                && let Some(TokenTree::Group(group)) = trees.get(i + 2)
                && group.delimiter() == Delimiter::Parenthesis
            {
                record_key_at(&group.stream(), 1, file, result);
                i += 3;
                continue;
            }

            // Call form: `(t|t_with) ( <args> )`, not a macro invocation.
            if (name == "t" || name == "t_with")
                && let Some(TokenTree::Group(group)) = trees.get(i + 1)
                && group.delimiter() == Delimiter::Parenthesis
            {
                // A method call `receiver.t("key")` is preceded by `.` and puts
                // the key first. A free/associated call `Locale::t(&loc, "key")`
                // places the receiver first, so the key is the first *literal*
                // argument. Either way, taking the first string-literal argument
                // recovers the key (a `t_with` call's later `("name","val")`
                // args never precede the key).
                let preceded_by_dot =
                    i > 0 && matches!(&trees[i - 1], TokenTree::Punct(p) if p.as_char() == '.');
                if preceded_by_dot {
                    record_key_at(&group.stream(), 0, file, result);
                } else {
                    record_first_literal_key(&group.stream(), file, result);
                }
                i += 2;
                continue;
            }
        }

        // Descend into any nested group (block, macro body, tuple, etc.).
        if let TokenTree::Group(group) = &trees[i] {
            scan_stream(&group.stream(), file, result);
        }
        i += 1;
    }
}

/// Parse a call/macro argument list and classify the argument at `index` as the
/// translation key.
fn record_key_at(args: &TokenStream, index: usize, file: &str, result: &mut ScanResult) {
    let parser = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
    let Ok(parsed) = parser.parse2(args.clone()) else {
        return;
    };
    if let Some(expr) = parsed.iter().nth(index) {
        record_key(expr, file, result);
    }
}

/// Parse a call argument list and treat the first string-literal argument as
/// the key; if there is none, record the call as a dynamic site.
fn record_first_literal_key(args: &TokenStream, file: &str, result: &mut ScanResult) {
    let parser = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
    let Ok(parsed) = parser.parse2(args.clone()) else {
        return;
    };
    if let Some(expr) = parsed.iter().find(|e| as_str_lit(e).is_some()) {
        record_key(expr, file, result);
    } else if let Some(first) = parsed.iter().next() {
        record_dynamic(first, file, result);
    }
}

/// Return the value of `expr` if it is a string literal.
fn as_str_lit(expr: &syn::Expr) -> Option<String> {
    if let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(s),
        ..
    }) = expr
    {
        Some(s.value())
    } else {
        None
    }
}

/// Classify a key expression: a string literal is a referenced key; anything
/// else is a dynamic (unanalyzable) call site.
fn record_key(expr: &syn::Expr, file: &str, result: &mut ScanResult) {
    if let Some(key) = as_str_lit(expr) {
        result.referenced.insert(key);
    } else {
        record_dynamic(expr, file, result);
    }
}

fn record_dynamic(expr: &syn::Expr, file: &str, result: &mut ScanResult) {
    result.dynamic.push(DynamicSite {
        file: file.to_owned(),
        line: expr.span().start().line,
        snippet: expr.to_token_stream().to_string(),
    });
}

// ── `.ftl` key extraction ─────────────────────────────────────────────────

/// Extract the set of message keys defined in a `.ftl` source, mirroring the
/// runtime [`parse_ftl`](autumn_web) grammar: `key = value` entries, `#`
/// comments and blank lines ignored, indented lines treated as continuations
/// of the previous entry.
#[must_use]
pub fn parse_ftl_keys(src: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for line in src.lines() {
        // Continuation of a multi-line value, a comment, or a blank line.
        if line.starts_with(char::is_whitespace)
            || line.trim_start().starts_with('#')
            || line.trim().is_empty()
        {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                keys.insert(key.to_owned());
            }
        }
    }
    keys
}

/// Read every `<locale>.ftl` file in `dir` and return `locale -> {keys}`.
fn load_locale_keys(dir: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let mut per_locale = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return per_locale;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ftl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        per_locale.insert(stem.to_owned(), parse_ftl_keys(&src));
    }
    per_locale
}

// ── Report assembly ───────────────────────────────────────────────────────

/// Build the per-locale report from the referenced keys, the defined keys per
/// locale, and the resolved fallback chain. Pure — unit-tested directly.
#[must_use]
pub fn build_report(
    scan: &ScanResult,
    config: &I18nConfig,
    per_locale_keys: &BTreeMap<String, BTreeSet<String>>,
) -> Report {
    let default_locale = config.default_locale.clone();
    let chain = config.resolved_fallback_chain();

    // Keys resolvable purely through the shared fallback chain (used by every
    // locale). `Bundle::translate` walks this chain on a per-locale miss.
    let mut chain_keys: BTreeSet<String> = BTreeSet::new();
    for locale in &chain {
        if let Some(keys) = per_locale_keys.get(locale) {
            chain_keys.extend(keys.iter().cloned());
        }
    }

    let empty = BTreeSet::new();
    let default_keys = per_locale_keys.get(&default_locale).unwrap_or(&empty);

    // Default locale first, then the rest alphabetically.
    let mut names: Vec<&String> = per_locale_keys.keys().collect();
    names.sort_by_key(|n| (**n != default_locale, (*n).clone()));

    let locales = names
        .into_iter()
        .map(|locale| {
            let keys = per_locale_keys.get(locale).unwrap_or(&empty);
            let is_default = *locale == default_locale;

            // Resolvable in this locale = its own keys + the fallback chain's.
            let mut resolvable = keys.clone();
            resolvable.extend(chain_keys.iter().cloned());

            let missing: Vec<String> = scan
                .referenced
                .iter()
                .filter(|k| !resolvable.contains(*k))
                .cloned()
                .collect();

            let untranslated: Vec<String> = if is_default {
                Vec::new()
            } else {
                default_keys.difference(keys).cloned().collect()
            };

            let unused: Vec<String> = keys
                .iter()
                .filter(|k| !scan.referenced.contains(*k))
                .cloned()
                .collect();

            LocaleReport {
                locale: locale.clone(),
                is_default,
                missing,
                untranslated,
                unused,
            }
        })
        .collect();

    Report {
        default_locale,
        fallback_chain: chain,
        referenced_keys: scan.referenced.len(),
        files_scanned: scan.files_scanned,
        dynamic: scan.dynamic.clone(),
        locales,
    }
}

// ── Config loading ────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Deserialize)]
struct ProjectToml {
    #[serde(default)]
    i18n: I18nConfig,
}

/// Read the `[i18n]` section from `autumn.toml` in `root`, falling back to
/// [`I18nConfig::default`] when the file or section is absent.
fn load_i18n_config(root: &Path) -> I18nConfig {
    let path = root.join("autumn.toml");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return I18nConfig::default();
    };
    toml::from_str::<ProjectToml>(&raw)
        .map(|p| p.i18n)
        .unwrap_or_default()
}

// ── Entry points ──────────────────────────────────────────────────────────

/// Run `autumn i18n check` against the current working directory and exit with
/// the appropriate CI status code.
pub fn run(opts: I18nCheckOptions) -> ! {
    let code = run_in(Path::new("."), opts);
    std::process::exit(code);
}

/// Testable core: run the check against `root`, print a report, and return the
/// exit code without terminating the process.
#[must_use]
pub fn run_in(root: &Path, opts: I18nCheckOptions) -> i32 {
    let config = load_i18n_config(root);
    let dir = root.join(&config.dir);

    if !dir.exists() {
        if opts.format == OutputFormat::Json {
            println!(
                "{{\"status\":\"skipped\",\"reason\":\"no i18n directory\",\"dir\":{}}}",
                serde_json::to_string(&config.dir).unwrap_or_else(|_| "\"i18n\"".to_owned())
            );
        } else {
            println!(
                "autumn i18n check: no i18n directory (`{}`) found — nothing to check.",
                config.dir
            );
        }
        return 0;
    }

    // Load through the real bundle loader so a `.ftl` syntax error or a missing
    // default locale fails here exactly as it would at app startup.
    if let Err(err) = Bundle::load_from_dir(&dir, &config) {
        eprintln!("autumn i18n check: failed to load i18n bundle: {err}");
        return 1;
    }

    let per_locale_keys = load_locale_keys(&dir);
    let scan = scan_project(root);
    let report = build_report(&scan, &config, &per_locale_keys);

    match opts.format {
        OutputFormat::Json => print_json(&report),
        OutputFormat::Text => print_text(&report, opts.strict),
    }

    report.exit_code(opts.strict)
}

fn print_json(report: &Report) {
    match serde_json::to_string_pretty(report) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("autumn i18n check: failed to serialize report: {err}"),
    }
}

fn print_text(report: &Report, strict: bool) {
    println!("autumn i18n check");
    println!(
        "  default locale: {}   fallback chain: {}",
        report.default_locale,
        report.fallback_chain.join(" -> ")
    );
    println!(
        "  scanned {} referenced key(s) across {} .rs file(s)",
        report.referenced_keys, report.files_scanned
    );

    if report.dynamic.is_empty() {
        println!("  dynamic (not checked): none");
    } else {
        println!(
            "  dynamic (not checked): {} call site(s) — keys built at runtime:",
            report.dynamic.len()
        );
        for site in &report.dynamic {
            println!("      {}:{}: {}", site.file, site.line, site.snippet);
        }
    }

    for locale in &report.locales {
        let tag = if locale.is_default {
            format!("{} (default)", locale.locale)
        } else {
            locale.locale.clone()
        };
        println!("\n  locale `{tag}`:");
        print_key_list("Missing", &locale.missing, "✗");
        if !locale.is_default {
            print_key_list("Untranslated", &locale.untranslated, "⚠");
        }
        print_key_list("Unused", &locale.unused, "·");
    }

    println!();
    if report.has_missing() {
        println!("Result: FAIL — missing keys in one or more locales.");
    } else if strict && report.has_warnings() {
        println!("Result: FAIL (--strict) — untranslated/unused keys present.");
    } else if report.has_warnings() {
        println!("Result: PASS with warnings — untranslated/unused keys present.");
    } else {
        println!("Result: PASS — all referenced keys are translated.");
    }
}

fn print_key_list(label: &str, keys: &[String], marker: &str) {
    if keys.is_empty() {
        println!("      {label}: (none)");
    } else {
        println!("      {label}:");
        for key in keys {
            println!("        {marker} {key}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(default: &str, chain: &[&str]) -> I18nConfig {
        I18nConfig {
            default_locale: default.to_owned(),
            supported_locales: vec![default.to_owned()],
            fallback_chain: chain.iter().map(|s| (*s).to_owned()).collect(),
            dir: "i18n".to_owned(),
        }
    }

    /// A [`ScanResult`] with the given referenced keys and no dynamic sites.
    fn scan_with(referenced: &[&str]) -> ScanResult {
        ScanResult {
            referenced: keys(referenced),
            ..Default::default()
        }
    }

    fn keys(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn scans_macro_method_and_call_forms() {
        let src = r#"
            fn view(locale: &Locale) {
                let _ = t!(locale, "nav.home");
                let _ = t!(locale, "greet.hi", name = who);
                let _ = locale.t("nav.about");
                let _ = locale.t_with("greet.count", &[("n", "1")]);
                let _ = Locale::t(&locale, "nav.brand");
            }
        "#;
        let mut result = ScanResult::default();
        scan_source(src, "view.rs", &mut result);
        assert_eq!(
            result.referenced,
            keys(&[
                "greet.count",
                "greet.hi",
                "nav.about",
                "nav.brand",
                "nav.home",
            ])
        );
        assert!(result.dynamic.is_empty());
    }

    #[test]
    fn dynamic_keys_are_reported_not_referenced() {
        let src = r#"
            fn view(locale: &Locale, section: &str) {
                let _ = locale.t(&format!("nav.{section}"));
                let _ = locale.t("nav.home");
            }
        "#;
        let mut result = ScanResult::default();
        scan_source(src, "view.rs", &mut result);
        assert_eq!(result.referenced, keys(&["nav.home"]));
        assert_eq!(result.dynamic.len(), 1);
        assert_eq!(result.dynamic[0].file, "view.rs");
        assert!(result.dynamic[0].snippet.contains("format"));
    }

    #[test]
    fn parse_ftl_keys_extracts_top_level_keys_only() {
        let src =
            "# comment\nnav.home = Home\nnav.about = About\n  continued line\n\nfooter.tag = Hi\n";
        assert_eq!(
            parse_ftl_keys(src),
            keys(&["footer.tag", "nav.about", "nav.home"])
        );
    }

    #[test]
    fn missing_key_flags_every_locale() {
        // `nav.about` is referenced but defined in NO locale (not even the
        // default) — so it cannot be resolved through any fallback chain and is
        // Missing everywhere. This is the correctness failure.
        let scan = scan_with(&["nav.home", "nav.about"]);
        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home"]));
        per_locale.insert("es".to_owned(), keys(&["nav.home"]));
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        assert!(report.has_missing());
        assert_eq!(report.exit_code(false), 1);
        let es = report.locales.iter().find(|l| l.locale == "es").unwrap();
        assert_eq!(es.missing, vec!["nav.about".to_owned()]);
        let en = report.locales.iter().find(|l| l.locale == "en").unwrap();
        assert_eq!(en.missing, vec!["nav.about".to_owned()]);
    }

    #[test]
    fn untranslated_key_is_a_warning_not_missing() {
        // `nav.about` is in the default locale `en` but not `es`. The default
        // fallback chain supplies it, so `es` is NOT Missing it — but it IS
        // Untranslated (a warning, error only under `--strict`).
        let scan = scan_with(&["nav.home", "nav.about"]);
        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home", "nav.about"]));
        per_locale.insert("es".to_owned(), keys(&["nav.home"]));
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        assert!(!report.has_missing());
        assert!(report.has_warnings());
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 1);
        let es = report.locales.iter().find(|l| l.locale == "es").unwrap();
        assert!(es.missing.is_empty());
        assert_eq!(es.untranslated, vec!["nav.about".to_owned()]);
    }

    #[test]
    fn fallback_chain_suppresses_missing() {
        // `es` lacks `nav.about`, but the fallback chain `es -> en` supplies
        // it, so it is NOT reported missing.
        let scan = scan_with(&["nav.home", "nav.about"]);
        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home", "nav.about"]));
        per_locale.insert("es".to_owned(), keys(&["nav.home"]));
        let report = build_report(&scan, &cfg("en", &["es", "en"]), &per_locale);

        let es = report.locales.iter().find(|l| l.locale == "es").unwrap();
        assert!(es.missing.is_empty(), "fallback should supply nav.about");
        assert!(!report.has_missing());
        assert_eq!(report.exit_code(false), 0);
    }

    #[test]
    fn unused_key_is_a_warning_only() {
        let scan = scan_with(&["nav.home"]);
        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home", "footer.old"]));
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        assert!(!report.has_missing());
        assert!(report.has_warnings());
        assert_eq!(report.exit_code(false), 0, "unused is not a failure");
        assert_eq!(report.exit_code(true), 1, "unused fails under --strict");
        let en = &report.locales[0];
        assert_eq!(en.unused, vec!["footer.old".to_owned()]);
    }

    #[test]
    fn clean_project_passes() {
        let scan = scan_with(&["nav.home"]);
        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home"]));
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);
        assert!(!report.has_missing());
        assert!(!report.has_warnings());
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 0);
    }
}
