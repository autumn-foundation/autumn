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
//!   code. A warning (error under `--strict`). Dynamic key sites are taken into
//!   account: a key any dynamic site's static prefix could build (e.g.
//!   `status.open` under a `format!("status.{state}")` site) is not flagged, and
//!   a *fully*-dynamic site (`t(&key)`) suppresses Unused reporting entirely
//!   (informational only) since it could reference any key.
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

use autumn_web::config::{AutumnConfig, Env, OsEnv};
use autumn_web::i18n::{Bundle, I18nConfig};
use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use serde::Serialize;

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
    /// The leading *static* portion of the key expression, up to the first
    /// dynamic interpolation — e.g. `format!("status.{state}")` → `"status."`.
    /// Empty when no leading literal is discoverable (a bare variable,
    /// `format!("{x}")`, …), meaning the site could reference *any* key. Used to
    /// suppress false `Unused` reports for keys such a site could cover.
    pub key_prefix: String,
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
    /// True when a *fully*-dynamic key site (one with an empty static prefix,
    /// e.g. `locale.t(&key)`) was found. Such a site could reference any key, so
    /// the **Unused** set cannot be trusted: `unused` entries are then reported
    /// for information only and never fail `--strict`.
    pub unused_suppressed_by_dynamic: bool,
    pub locales: Vec<LocaleReport>,
}

impl Report {
    /// Any locale with a **Missing** key (the correctness failure).
    #[must_use]
    pub fn has_missing(&self) -> bool {
        self.locales.iter().any(|l| !l.missing.is_empty())
    }

    /// Any **Untranslated**/**Unused** warnings across locales. When a
    /// fully-dynamic key site suppressed Unused checking, the (informational)
    /// `unused` lists do not count toward warnings and never fail `--strict`.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.locales.iter().any(|l| !l.untranslated.is_empty())
            || (!self.unused_suppressed_by_dynamic
                && self.locales.iter().any(|l| !l.unused.is_empty()))
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
        // Never follow symlinks: a symlinked directory cycle under the project
        // would otherwise recurse forever and hang this CI tool. `file_type()`
        // (unlike `Path::is_dir`) does not traverse the link.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            // Skip build artifacts, VCS metadata, and hidden dirs.
            if name == "target" || name.starts_with('.') {
                continue;
            }
            collect_rs_files(&path, out);
        } else if file_type.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
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

            // Macro form: `t ! ( <locale>, "key" [, args] )`. `t!` is a proc
            // macro, so every Rust macro delimiter — `t!(...)`, `t!{...}`,
            // `t![...]` — compiles and carries the same token stream; accept
            // all three (but not `Delimiter::None` implicit groups).
            if name == "t"
                && matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!')
                && let Some(TokenTree::Group(group)) = trees.get(i + 2)
                && matches!(
                    group.delimiter(),
                    Delimiter::Parenthesis | Delimiter::Brace | Delimiter::Bracket
                )
            {
                record_key_at(&group.stream(), 1, file, result);
                i += 3;
                continue;
            }

            // Call form: `(t|t_with) ( <args> )`, not a macro invocation.
            // Only a METHOD (`receiver.t(...)`) or ASSOCIATED (`Type::t(...)`)
            // call is an Autumn translation site — the sole `t`/`t_with` entry
            // points are the `Locale::t` / `Locale::t_with` methods. A bare
            // free-function `t("...")` (some project's own helper/fixture) is
            // NOT a translation reference and must be left alone; the
            // free-standing translation form is the `t!` macro handled above.
            if (name == "t" || name == "t_with")
                && let Some(TokenTree::Group(group)) = trees.get(i + 1)
                && group.delimiter() == Delimiter::Parenthesis
            {
                let prev_punct = i.checked_sub(1).and_then(|j| match &trees[j] {
                    TokenTree::Punct(p) => Some(p.as_char()),
                    _ => None,
                });
                match prev_punct {
                    // Method call `receiver.t("key")`: preceded by `.`, key is
                    // the first argument.
                    Some('.') => {
                        record_key_at(&group.stream(), 0, file, result);
                        i += 2;
                        continue;
                    }
                    // Associated call `Locale::t(&loc, "key")`: the `::` path
                    // separator tokenizes as two `:` puncts, so the token before
                    // `t` is `:`. The `&self` receiver occupies argument 0, so the
                    // key is argument 1 — the same slot as the `t!` macro form.
                    // Reading the fixed key slot (rather than the first literal
                    // anywhere) means a dynamic key like `&format!("status.{s}")`
                    // records the KEY argument's `status.` prefix instead of
                    // mistaking the receiver for a fully-dynamic (empty-prefix)
                    // key and suppressing every Unused warning project-wide.
                    Some(':') => {
                        record_key_at(&group.stream(), 1, file, result);
                        i += 2;
                        continue;
                    }
                    // Bare free-function `t(...)` — not a translation call.
                    _ => {}
                }
            }
        }

        // Descend into any nested group (block, macro body, tuple, etc.).
        if let TokenTree::Group(group) = &trees[i] {
            scan_stream(&group.stream(), file, result);
        }
        i += 1;
    }
}

/// Split a call/macro argument [`TokenStream`] on its **top-level** commas.
///
/// Nested commas (inside `(...)`, `[...]`, `{...}`) live within a single
/// [`TokenTree::Group`] and so are never seen here — each returned segment is
/// exactly one argument. Working on the raw token list rather than parsing the
/// whole list as [`syn::Expr`] means a `$metavariable` (or any other
/// non-`Expr` token) in *another* argument position cannot discard a literal
/// key — e.g. `t!($locale, "nav.home")` or `.t_with("nav.home", &[("n", $n)])`
/// written inside a `macro_rules!` transcriber still records `nav.home`.
fn split_top_level_args(args: &TokenStream) -> Vec<TokenStream> {
    let mut segments = Vec::new();
    let mut current: Vec<TokenTree> = Vec::new();
    for tt in args.clone() {
        if matches!(&tt, TokenTree::Punct(p) if p.as_char() == ',') {
            segments.push(std::mem::take(&mut current).into_iter().collect());
        } else {
            current.push(tt);
        }
    }
    if !current.is_empty() {
        segments.push(current.into_iter().collect());
    }
    segments
}

/// Extract the argument at `index` from a call/macro argument list and classify
/// just that key slot. Tokens in the remaining positions are never inspected,
/// so metavariables elsewhere do not discard a literal key.
fn record_key_at(args: &TokenStream, index: usize, file: &str, result: &mut ScanResult) {
    if let Some(segment) = split_top_level_args(args).get(index) {
        record_key_segment(segment, file, result);
    }
}

/// Return the value of `segment` if it is a single string-literal token.
fn segment_str_lit(segment: &TokenStream) -> Option<String> {
    syn::parse2::<syn::LitStr>(segment.clone())
        .ok()
        .map(|s| s.value())
}

/// Classify a key segment: a lone string literal is a referenced key; anything
/// else (a metavariable, a `format!(...)`, a borrow, …) is a dynamic
/// (unanalyzable) call site.
fn record_key_segment(segment: &TokenStream, file: &str, result: &mut ScanResult) {
    if let Some(key) = segment_str_lit(segment) {
        result.referenced.insert(key);
    } else {
        record_dynamic_segment(segment, file, result);
    }
}

fn record_dynamic_segment(segment: &TokenStream, file: &str, result: &mut ScanResult) {
    let line = segment
        .clone()
        .into_iter()
        .next()
        .map_or(0, |tt| tt.span().start().line);
    result.dynamic.push(DynamicSite {
        file: file.to_owned(),
        line,
        snippet: segment.to_string(),
        key_prefix: dynamic_key_prefix(segment),
    });
}

/// Derive the leading *static* key prefix a dynamic key site could reference.
///
/// - `format!("status.{state}")` → `"status."`
/// - `&format!("nav.{}", x)`     → `"nav."`
/// - `&(format!("status.{s}"))`  → `"status."`
/// - `"status.".to_owned() + s`  → `"status."`
/// - a bare variable / `&key` / `format!("{x}")` (no leading literal) → `""`
///
/// An empty prefix means the site could reference *any* key, so the caller must
/// treat the entire `Unused` set as untrustworthy.
fn dynamic_key_prefix(segment: &TokenStream) -> String {
    let trees: Vec<TokenTree> = segment.clone().into_iter().collect();
    // Peel leading borrows/derefs and unwrap any group that merely *wraps* the
    // whole key expression — e.g. `&(format!("status.{s}"))`, `&(&format!(...))`,
    // `((format!(...)))` — so the leading `format!`/literal is exposed. Without
    // this, a wrapped `format!` is invisible to the detection below and the site
    // is misread as fully dynamic (empty prefix), which suppresses every Unused
    // warning project-wide.
    let normalized = normalize_key_segment(&trees);

    // Case 1: a leading `format!(...)` invocation (e.g. the `format!` in
    // `&format!("nav.{}", x)`, now exposed at the front by normalization). Its
    // first argument's format string supplies the static prefix — the text
    // before the first `{` interpolation.
    if let Some(group) = leading_format_group(&normalized) {
        return split_top_level_args(&group.stream())
            .first()
            .and_then(segment_str_lit)
            .map_or_else(String::new, |fmt| format_static_prefix(&fmt));
    }

    // Case 2: a leading string literal (e.g. `"status." + s`). A plain literal
    // has no interpolation, so its whole value is static leading text. Only the
    // *leading* literal counts; one in a later position must not masquerade as
    // the key's prefix.
    leading_str_literal(&normalized)
}

/// Peel leading borrow/deref puncts (`&`, `*`) and unwrap a single leading
/// parenthesized or implicit (`Delimiter::None`) group that wraps the whole key
/// expression, returning the exposed top-level token list. Recurses so nested
/// wrappers such as `&(&format!(...))` and `((format!(...)))` fully unwrap.
///
/// Only *leading* wrappers around the entire expression are peeled — a group in
/// a later position (a `format!`'s own arg list, a method-call's args, the right
/// operand of a `+`) is left intact, so a `format!` buried after real tokens is
/// never misattributed as the key's prefix.
fn normalize_key_segment(trees: &[TokenTree]) -> Vec<TokenTree> {
    let Some(start) = trees
        .iter()
        .position(|tt| !matches!(tt, TokenTree::Punct(p) if matches!(p.as_char(), '&' | '*')))
    else {
        return Vec::new();
    };
    let rest = &trees[start..];
    if let TokenTree::Group(g) = &rest[0]
        && matches!(g.delimiter(), Delimiter::Parenthesis | Delimiter::None)
    {
        let inner: Vec<TokenTree> = g.stream().into_iter().collect();
        return normalize_key_segment(&inner);
    }
    rest.to_vec()
}

/// If `trees` begins with a `format!(...)` / `format!{...}` / `format![...]`
/// invocation, return its argument group. Expects a [`normalize_key_segment`]-
/// normalized stream, so the `format` ident is the leading meaningful token.
fn leading_format_group(trees: &[TokenTree]) -> Option<&Group> {
    if let Some(TokenTree::Ident(id)) = trees.first()
        && *id == "format"
        && matches!(trees.get(1), Some(TokenTree::Punct(p)) if p.as_char() == '!')
        && let Some(TokenTree::Group(group)) = trees.get(2)
    {
        return Some(group);
    }
    None
}

/// The value of the leading string literal of a
/// [`normalize_key_segment`]-normalized expression. `None` when the expression
/// does not begin with a string literal (a bare variable, a metavariable, a
/// `format!`, a method-call receiver…), yielding an empty prefix.
fn leading_str_literal(trees: &[TokenTree]) -> String {
    match trees.first() {
        Some(tt @ TokenTree::Literal(_)) => {
            segment_str_lit(&TokenStream::from(tt.clone())).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// The static leading text of a `format!` format string: everything before the
/// first `{` interpolation, with `{{`/`}}` escapes decoded to `{`/`}`.
fn format_static_prefix(fmt: &str) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '{' => break, // a real interpolation begins the dynamic tail
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            other => out.push(other),
        }
    }
    out
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
        // Mirror the runtime `parse_ftl`, which treats only ' ' and '\t' as
        // continuation indent.
        if line.starts_with(' ')
            || line.starts_with('\t')
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

    // Dynamic key sites make the **Unused** set imprecise: a key a dynamic site
    // could build must not be flagged Unused. A site with a non-empty static
    // prefix (`format!("status.{s}")` → `"status."`) suppresses only keys under
    // that prefix; a *fully*-dynamic site (empty prefix, e.g. `t(&key)`) could
    // reference any key, so Unused is suppressed wholesale (reported for info,
    // never failing `--strict`).
    let unused_suppressed_by_dynamic = scan.dynamic.iter().any(|d| d.key_prefix.is_empty());
    let dynamic_prefixes: Vec<&str> = scan
        .dynamic
        .iter()
        .map(|d| d.key_prefix.as_str())
        .filter(|p| !p.is_empty())
        .collect();

    // Seed the set of locales to report on from BOTH the `.ftl` files present on
    // disk AND the locales declared in `[i18n].supported_locales`. A configured
    // locale with no `.ftl` file must still be reported (its keys resolve to the
    // default via the fallback chain, so they surface as Untranslated warnings)
    // — otherwise "added a second locale but shipped untranslated UI" slips past
    // even `--strict`. The union deduplicates locales that are both configured
    // and have a file.
    let mut name_set: BTreeSet<&String> = per_locale_keys.keys().collect();
    name_set.extend(config.supported_locales.iter());

    // Default locale first, then the rest alphabetically.
    let mut names: Vec<&String> = name_set.into_iter().collect();
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

            // Fluent terms (`-brand = …`) are only referenced from within other
            // messages via `{ -brand }`, never through `t!`, so they would
            // always show up as Unused. Exclude them to avoid false-positive CI
            // noise. Also exclude any key a dynamic site's static prefix could
            // build (e.g. `status.open` under a `format!("status.{s}")` site).
            let unused: Vec<String> = keys
                .iter()
                .filter(|k| !k.starts_with('-') && !scan.referenced.contains(*k))
                .filter(|k| !dynamic_prefixes.iter().any(|p| k.starts_with(p)))
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
        unused_suppressed_by_dynamic,
        locales,
    }
}

// ── Config loading ────────────────────────────────────────────────────────

#[derive(Debug, Default, serde::Deserialize)]
struct ProjectToml {
    #[serde(default)]
    i18n: I18nConfig,
}

/// An [`Env`] that reports `root` as the manifest directory so the layered
/// config loader reads `autumn.toml`, inline `[profile.<env>.i18n]` overlays,
/// and `autumn-<env>.toml` from the project being checked, while every other
/// variable (notably `AUTUMN_ENV`) is read from the underlying environment.
struct RootedEnv<'a> {
    root: &'a Path,
    inner: &'a dyn Env,
}

impl Env for RootedEnv<'_> {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        if key == "AUTUMN_MANIFEST_DIR" {
            return Ok(self.root.to_string_lossy().into_owned());
        }
        self.inner.var(key)
    }
}

/// Resolve the effective `[i18n]` config for `root`, honoring `AUTUMN_ENV` and
/// the same profile-overlay layering the runtime applies at startup
/// ([`AutumnConfig::load_with_env`]): base `autumn.toml` ← `[profile.<env>.i18n]`
/// ← `autumn-<env>.toml`. This ensures `i18n check` inspects the *same* locale
/// directory and supported-locale set the app will serve under the active
/// profile — e.g. under `AUTUMN_ENV=prod` a production overlay's
/// `dir`/`supported_locales` are checked instead of the base defaults, so
/// missing production translations are no longer silently passed.
///
/// Falls back to a base-only parse of `autumn.toml` if the layered load fails
/// (e.g. an unrelated prod validation error), so the check never regresses to
/// "no config" when a base `[i18n]` block is present.
///
/// Also returns the active profile name (e.g. `"dev"`, `"prod"`, or a custom
/// profile) so callers can inspect the matching `[profile.<env>.i18n]` /
/// `autumn-<env>.toml` overlays; it is `None` only when the layered load failed
/// and we fell back to a base-only parse.
fn load_i18n_config(root: &Path, env: &dyn Env) -> (I18nConfig, Option<String>) {
    let rooted = RootedEnv { root, inner: env };
    if let Ok(config) = AutumnConfig::load_with_env(&rooted) {
        let profile = config.profile_name().map(str::to_owned);
        return (config.i18n, profile);
    }
    (load_i18n_config_base_only(root), None)
}

/// Returns `true` when the project *explicitly* configures i18n in any layer
/// the runtime config loader reads — a base `autumn.toml` `[i18n]` table, a
/// `[profile.<env>.i18n]` inline overlay for the active profile, or an
/// `autumn-<env>.toml` overlay's `[i18n]` table.
///
/// This distinguishes a project that genuinely has *no* i18n (where a missing
/// locale directory is a legitimate no-op) from one that configures i18n but
/// whose resolved directory is absent (a misconfiguration that the runtime
/// would hit via `.i18n_auto()` → [`Bundle::load_from_dir`] →
/// `MissingDefaultLocale` at startup). In the latter case the check must *not*
/// skip: it lets the bundle loader surface the same startup error.
fn i18n_is_configured(root: &Path, profile: Option<&str>, selected_input: &str) -> bool {
    // Base `autumn.toml`: a top-level `[i18n]` table, or a
    // `[profile.<env>.i18n]` inline overlay for the active profile.
    if let Some(base) = read_config_table(&root.join("autumn.toml")) {
        if base.contains_key("i18n") {
            return true;
        }
        if let Some(profile) = profile
            && let Some(profiles) = base.get("profile").and_then(toml::Value::as_table)
        {
            for name in profile_lookup_names(profile) {
                if profiles
                    .get(name)
                    .and_then(toml::Value::as_table)
                    .is_some_and(|t| t.contains_key("i18n"))
                {
                    return true;
                }
            }
        }
    }

    // `autumn-<env>.toml` overlay. The runtime loads only the FIRST existing
    // override file in `profile_override_file_lookup_names` order and `break`s
    // (see `AutumnConfig::load_with_env` in `autumn/src/config.rs`), so only
    // that file's `[i18n]` table participates. Consulting later aliases here
    // would let an *unloaded* file's `[i18n]` falsely report i18n as configured
    // (e.g. `autumn-production.toml` when `autumn-prod.toml` already won).
    if let Some(profile) = profile {
        for name in profile_override_file_lookup_names(profile, selected_input) {
            let path = root.join(format!("autumn-{name}.toml"));
            if path.exists() {
                return read_config_table(&path).is_some_and(|t| t.contains_key("i18n"));
            }
        }
    }

    false
}

/// Parse a TOML file into its top-level table, returning `None` when the file
/// is absent or unparseable.
fn read_config_table(path: &Path) -> Option<toml::Table> {
    let raw = std::fs::read_to_string(path).ok()?;
    toml::from_str::<toml::Table>(&raw).ok()
}

/// The active profile plus its legacy aliases, mirroring the runtime config
/// loader's `profile_lookup_names` so a `[profile.production.i18n]` overlay (or
/// `autumn-production.toml`) is detected under `AUTUMN_ENV=prod` and vice versa.
fn profile_lookup_names(profile: &str) -> Vec<&str> {
    match profile {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Ordered `autumn-<name>.toml` override-file lookup names, mirroring the
/// runtime `profile_override_file_lookup_names` in `autumn/src/config.rs`
/// exactly. The runtime loads only the FIRST existing file in this order (then
/// `break`s), and the order prefers the explicitly-selected spelling — so under
/// `AUTUMN_ENV=prod` an `autumn-prod.toml` wins over `autumn-production.toml`,
/// but `AUTUMN_ENV=production` reverses that preference. `selected_input` is the
/// raw profile spelling from the environment (see [`selected_profile_input`]).
fn profile_override_file_lookup_names(profile: &str, selected_input: &str) -> Vec<String> {
    match profile {
        "prod" if selected_input.eq_ignore_ascii_case("production") => {
            vec!["production".to_owned(), "prod".to_owned()]
        }
        "prod" => vec!["prod".to_owned(), "production".to_owned()],
        "dev" if selected_input.eq_ignore_ascii_case("development") => {
            vec!["development".to_owned(), "dev".to_owned()]
        }
        "dev" => vec!["dev".to_owned(), "development".to_owned()],
        other => vec![other.to_owned()],
    }
}

/// The raw profile spelling the runtime would resolve from the environment,
/// mirroring the `AUTUMN_ENV` → `AUTUMN_PROFILE` precedence of the runtime's
/// `resolve_profile_input`. Only these env-var sources are relevant to profile
/// override-file precedence, which is the sole use of this value: picking the
/// winning `autumn-<env>.toml` alias when both spellings exist on disk.
fn selected_profile_input(env: &dyn Env) -> String {
    for key in ["AUTUMN_ENV", "AUTUMN_PROFILE"] {
        if let Ok(val) = env.var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }
    String::new()
}

/// Read only the base `[i18n]` section from `autumn.toml` in `root`, falling
/// back to [`I18nConfig::default`] when the file or section is absent. Used as
/// a safety net when the full layered load fails.
fn load_i18n_config_base_only(root: &Path) -> I18nConfig {
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
    let code = run_in(Path::new("."), &OsEnv, opts);
    std::process::exit(code);
}

/// Testable core: run the check against `root`, print a report, and return the
/// exit code without terminating the process. `env` supplies `AUTUMN_ENV`
/// (and other variables) for profile-aware i18n config resolution.
#[must_use]
pub fn run_in(root: &Path, env: &dyn Env, opts: I18nCheckOptions) -> i32 {
    let (config, profile) = load_i18n_config(root, env);
    let dir = root.join(&config.dir);

    // Skip (exit 0) *only* when the project has no i18n configuration at all.
    // When i18n is configured but the resolved directory is absent, fall
    // through to `Bundle::load_from_dir` so the missing directory surfaces the
    // same `MissingDefaultLocale` error the app would fail with at startup —
    // rather than a false CI pass.
    if !dir.exists() && !i18n_is_configured(root, profile.as_deref(), &selected_profile_input(env))
    {
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

    if report.unused_suppressed_by_dynamic {
        println!(
            "  note: Unused checking suppressed — a fully-dynamic key site (no static \
             prefix) is present; Unused entries below are informational only and do not \
             fail --strict."
        );
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
        let unused_label = if report.unused_suppressed_by_dynamic {
            "Unused (informational)"
        } else {
            "Unused"
        };
        print_key_list(unused_label, &locale.unused, "·");
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
    fn free_function_t_is_not_a_translation_reference() {
        // A bare `t("...")` free function (e.g. a project's own helper or a test
        // fixture accessor) is NOT an Autumn translation call — only the `t!`
        // macro, the `.t(...)`/`.t_with(...)` methods, and the `Type::t(...)`
        // associated form are. A free `t(...)` must not pollute referenced keys.
        let src = r#"
            fn t(name: &str) -> Fixture {
                Fixture::load(name)
            }
            fn view(locale: &Locale) {
                let _ = t("fixture-name");
                let _ = locale.t("nav.home");
                let _ = Locale::t(&locale, "nav.title");
                let _ = t!(locale, "nav.macro");
            }
        "#;
        let mut result = ScanResult::default();
        scan_source(src, "view.rs", &mut result);
        assert_eq!(
            result.referenced,
            keys(&["nav.home", "nav.macro", "nav.title"]),
            "the bare free `t(\"fixture-name\")` call must be ignored"
        );
        assert!(result.dynamic.is_empty());
    }

    #[test]
    fn macro_accepts_all_delimiters() {
        // `t!` is a proc macro, so brace/bracket invocations compile just like
        // the parenthesized form and carry the same key; all three must record.
        let src = r#"
            fn view(locale: &Locale) {
                let _ = t!(locale, "nav.paren");
                let _ = t!{ locale, "nav.home" };
                let _ = t![locale, "nav.brace"];
            }
        "#;
        let mut result = ScanResult::default();
        scan_source(src, "view.rs", &mut result);
        assert_eq!(
            result.referenced,
            keys(&["nav.brace", "nav.home", "nav.paren"])
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
    fn macro_metavariables_do_not_discard_literal_key() {
        // A translation call written inside a `macro_rules!` transcriber carries
        // `$metavariable` tokens in non-key argument positions. Parsing the whole
        // argument list as `syn::Expr` would reject the `$...` tokens and drop the
        // call — but the key slot is still a plain string literal and must be
        // recorded as referenced.
        let src = r#"
            macro_rules! nav {
                ($locale:expr, $n:expr) => {{
                    let _ = t!($locale, "nav.home");
                    let _ = $locale.t_with("nav.count", &[("n", $n)]);
                }};
            }
        "#;
        let mut result = ScanResult::default();
        scan_source(src, "macros.rs", &mut result);
        assert_eq!(result.referenced, keys(&["nav.count", "nav.home"]));
        assert!(
            result.dynamic.is_empty(),
            "metavariables in non-key positions must not create dynamic sites: {:?}",
            result.dynamic
        );
    }

    #[test]
    fn metavariable_in_key_slot_is_dynamic_not_referenced() {
        // When the key slot ITSELF is a metavariable (genuinely non-literal), it
        // is correctly classified as dynamic — not recorded as a referenced key.
        let src = r"
            macro_rules! tr {
                ($locale:expr, $key:expr) => {
                    let _ = t!($locale, $key);
                };
            }
        ";
        let mut result = ScanResult::default();
        scan_source(src, "tr.rs", &mut result);
        assert!(
            result.referenced.is_empty(),
            "a metavariable key must not be a referenced key: {:?}",
            result.referenced
        );
        assert_eq!(result.dynamic.len(), 1);
    }

    #[test]
    fn dynamic_prefix_from_format_and_leading_literal() {
        // The static prefix is the text before the first `{` interpolation for a
        // `format!`, the whole leading literal for a concat, and empty when no
        // leading literal is discoverable.
        let src = r#"
            fn view(locale: &Locale, state: &str, x: &str, key: &str) {
                let _ = locale.t(&format!("status.{state}"));
                let _ = locale.t(&format!("nav.{}", x));
                let _ = locale.t(&format!("{x}"));
                let _ = locale.t(&("footer.".to_owned() + x));
                let _ = locale.t(key);
            }
        "#;
        let mut result = ScanResult::default();
        scan_source(src, "view.rs", &mut result);
        let prefixes: Vec<&str> = result
            .dynamic
            .iter()
            .map(|d| d.key_prefix.as_str())
            .collect();
        assert_eq!(prefixes, vec!["status.", "nav.", "", "footer.", ""]);
    }

    #[test]
    fn dynamic_prefix_descends_through_wrapping_groups() {
        // A `format!` wrapped in an extra group behind borrows — `&(format!(…))`,
        // `&(&format!(…))`, `(format!(…))` — must still expose the leading
        // `format!` so its static prefix is recovered, rather than being misread
        // as a fully-dynamic (empty-prefix) site that suppresses all Unused
        // checking. A bare variable still yields an empty prefix.
        let src = r#"
            fn view(locale: &Locale, state: &str, x: &str, y: &str, key: &str) {
                let _ = locale.t(&(format!("status.{state}")));
                let _ = locale.t(&(&format!("nav.{x}")));
                let _ = locale.t((format!("footer.{y}")));
                let _ = locale.t(&(key));
            }
        "#;
        let mut result = ScanResult::default();
        scan_source(src, "view.rs", &mut result);
        let prefixes: Vec<&str> = result
            .dynamic
            .iter()
            .map(|d| d.key_prefix.as_str())
            .collect();
        assert_eq!(prefixes, vec!["status.", "nav.", "footer.", ""]);
    }

    #[test]
    fn wrapped_dynamic_prefix_suppresses_only_matching_unused_keys() {
        // A `format!` wrapped in an extra group — `t(&(format!("status.{s}")))` —
        // records prefix `status.` (NOT empty), so `unused_suppressed_by_dynamic`
        // stays false: `status.*` is suppressed from Unused, but an unrelated
        // stale `footer.legacy` still fails `--strict`.
        let src = r#"
            fn view(locale: &Locale, state: &str) {
                let _ = locale.t("nav.home");
                let _ = locale.t(&(format!("status.{state}")));
            }
        "#;
        let mut scan = ScanResult::default();
        scan_source(src, "view.rs", &mut scan);
        assert_eq!(scan.dynamic.len(), 1);
        assert_eq!(scan.dynamic[0].key_prefix, "status.");

        let mut per_locale = BTreeMap::new();
        per_locale.insert(
            "en".to_owned(),
            keys(&["nav.home", "status.open", "status.closed", "footer.legacy"]),
        );
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        assert!(!report.unused_suppressed_by_dynamic);
        let en = &report.locales[0];
        assert_eq!(
            en.unused,
            vec!["footer.legacy".to_owned()],
            "wrapped prefix `status.` must suppress status.* but keep footer.legacy"
        );
        assert!(report.has_warnings());
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 1);
    }

    #[test]
    fn format_static_prefix_decodes_escaped_braces() {
        assert_eq!(format_static_prefix("status.{state}"), "status.");
        assert_eq!(format_static_prefix("nav.{}"), "nav.");
        assert_eq!(format_static_prefix("{x}"), "");
        assert_eq!(format_static_prefix("a{{b}}.{x}"), "a{b}.");
        assert_eq!(format_static_prefix("plain"), "plain");
    }

    #[test]
    fn dynamic_prefix_suppresses_only_matching_unused_keys() {
        // A dynamic site `t(&format!("status.{state}"))` records prefix
        // `status.`, so `status.open`/`status.closed` must NOT be flagged Unused
        // — but an unrelated `footer.legacy` (matching no prefix) still is.
        let src = r#"
            fn view(locale: &Locale, state: &str) {
                let _ = locale.t("nav.home");
                let _ = locale.t(&format!("status.{state}"));
            }
        "#;
        let mut scan = ScanResult::default();
        scan_source(src, "view.rs", &mut scan);

        let mut per_locale = BTreeMap::new();
        per_locale.insert(
            "en".to_owned(),
            keys(&["nav.home", "status.open", "status.closed", "footer.legacy"]),
        );
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        assert!(!report.unused_suppressed_by_dynamic);
        let en = &report.locales[0];
        assert_eq!(
            en.unused,
            vec!["footer.legacy".to_owned()],
            "prefix `status.` must suppress status.* keys but keep footer.legacy"
        );
        // Genuinely-unused key still fails under --strict.
        assert!(report.has_warnings());
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 1);
    }

    #[test]
    fn associated_dynamic_key_records_key_arg_not_receiver() {
        // `Locale::t(&locale, &format!("status.{state}"))` is an associated call:
        // the receiver `&locale` is argument 0 and the key is argument 1. The
        // scanner must classify the KEY argument, deriving prefix `status.` — not
        // fall back to the receiver (empty prefix), which would suppress every
        // Unused warning project-wide. So `status.*` is suppressed but an
        // unrelated `footer.legacy` still fails `--strict`.
        let src = r#"
            fn view(locale: &Locale, state: &str) {
                let _ = Locale::t(&locale, "nav.home");
                let _ = Locale::t(&locale, &format!("status.{state}"));
                let _ = Locale::t_with(&locale, &format!("status.{state}"), &[("n", "1")]);
            }
        "#;
        let mut scan = ScanResult::default();
        scan_source(src, "view.rs", &mut scan);

        // The literal associated key is recorded as referenced (no regression).
        assert!(scan.referenced.contains("nav.home"));
        // Both dynamic associated sites derive the `status.` prefix from their
        // KEY argument, never the receiver.
        assert_eq!(scan.dynamic.len(), 2);
        assert!(scan.dynamic.iter().all(|d| d.key_prefix == "status."));

        let mut per_locale = BTreeMap::new();
        per_locale.insert(
            "en".to_owned(),
            keys(&["nav.home", "status.open", "status.closed", "footer.legacy"]),
        );
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        assert!(!report.unused_suppressed_by_dynamic);
        let en = &report.locales[0];
        assert_eq!(
            en.unused,
            vec!["footer.legacy".to_owned()],
            "associated dynamic prefix `status.` must suppress status.* but keep footer.legacy"
        );
        // Genuinely-unused key still fails under --strict.
        assert!(report.has_warnings());
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 1);
    }

    #[test]
    fn associated_fully_dynamic_key_still_suppresses_entirely() {
        // A bare-variable key in the associated form `Locale::t(&locale, key)`
        // yields an empty prefix (could reference any key), so Unused reporting is
        // suppressed wholesale — confirming index-1 classification does not read
        // the receiver, which would also (wrongly) yield an empty prefix.
        let src = r#"
            fn view(locale: &Locale, key: &str) {
                let _ = Locale::t(&locale, "nav.home");
                let _ = Locale::t(&locale, key);
            }
        "#;
        let mut scan = ScanResult::default();
        scan_source(src, "view.rs", &mut scan);
        assert_eq!(scan.dynamic.len(), 1);
        assert_eq!(scan.dynamic[0].key_prefix, "");
        assert!(scan.dynamic[0].snippet.contains("key"));

        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home", "footer.legacy"]));
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);
        assert!(report.unused_suppressed_by_dynamic);
        assert_eq!(report.exit_code(true), 0);
    }

    #[test]
    fn fully_dynamic_site_suppresses_unused_entirely() {
        // A bare-variable key site `t(&key)` (empty prefix) could reference any
        // key, so Unused reporting is suppressed wholesale: even otherwise-unused
        // keys must not fail `--strict`.
        let src = r#"
            fn view(locale: &Locale, key: &str) {
                let _ = locale.t("nav.home");
                let _ = locale.t(&key);
            }
        "#;
        let mut scan = ScanResult::default();
        scan_source(src, "view.rs", &mut scan);
        assert_eq!(scan.dynamic.len(), 1);
        assert_eq!(scan.dynamic[0].key_prefix, "");

        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home", "footer.legacy"]));
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        assert!(report.unused_suppressed_by_dynamic);
        // The unused key is still listed (informational) …
        let en = &report.locales[0];
        assert_eq!(en.unused, vec!["footer.legacy".to_owned()]);
        // … but does NOT count as a warning or fail --strict.
        assert!(!report.has_warnings());
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 0);
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
    fn configured_locale_without_ftl_file_is_untranslated() {
        // `es` is declared in `[i18n].supported_locales` but has NO `es.ftl` on
        // disk. Runtime negotiation can still select `es` and fall back to the
        // default bundle, so the untranslated UI must surface: `es` appears in
        // the report with every default key Untranslated (resolvable via the
        // fallback chain → NOT Missing), a warning that fails only under
        // `--strict`.
        let scan = scan_with(&["nav.home", "nav.about"]);
        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home", "nav.about"]));
        // Only `en` has a file; `es` is configured but its file is absent.
        let config = I18nConfig {
            default_locale: "en".to_owned(),
            supported_locales: vec!["en".to_owned(), "es".to_owned()],
            fallback_chain: Vec::new(),
            dir: "i18n".to_owned(),
        };
        let report = build_report(&scan, &config, &per_locale);

        // `es` is present in the report even though it has no `.ftl` file.
        let es = report
            .locales
            .iter()
            .find(|l| l.locale == "es")
            .expect("configured `es` must appear in the report");
        assert!(
            es.missing.is_empty(),
            "fallback supplies keys: {:?}",
            es.missing
        );
        assert_eq!(
            es.untranslated,
            vec!["nav.about".to_owned(), "nav.home".to_owned()]
        );
        assert!(es.unused.is_empty());

        // Warning by default (exit 0), failure under --strict (exit 1).
        assert!(!report.has_missing());
        assert!(report.has_warnings());
        assert_eq!(report.exit_code(false), 0);
        assert_eq!(report.exit_code(true), 1);
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
    fn fluent_terms_are_not_reported_unused() {
        // A Fluent term (`-brand = …`) is referenced only from within other
        // messages via `{ -brand }`, never through `t!`, so it must not be
        // flagged Unused (that would be false-positive CI noise).
        let src = "nav.home = Home\n-brand = Autumn\n";
        assert_eq!(parse_ftl_keys(src), keys(&["-brand", "nav.home"]));

        let scan = scan_with(&["nav.home"]);
        let mut per_locale = BTreeMap::new();
        per_locale.insert("en".to_owned(), keys(&["nav.home", "-brand"]));
        let report = build_report(&scan, &cfg("en", &[]), &per_locale);

        let en = &report.locales[0];
        assert!(
            en.unused.is_empty(),
            "Fluent term should not be Unused: {:?}",
            en.unused
        );
        assert!(!report.has_warnings());
    }

    #[cfg(unix)]
    #[test]
    fn collect_rs_files_does_not_follow_symlink_cycles() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // One real source file, to be collected exactly once.
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        // A subdirectory holding a symlink back to the root, forming a cycle:
        // root/sub/loop -> root. Following it would recurse forever.
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        symlink(root, sub.join("loop")).unwrap();

        let mut files = Vec::new();
        collect_rs_files(root, &mut files);

        // Terminates (no infinite recursion / stack overflow) and does not
        // double-count `main.rs` by traversing the symlinked cycle.
        assert_eq!(files.len(), 1, "cycle must not be followed: {files:?}");
        assert!(files[0].ends_with("main.rs"));
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
