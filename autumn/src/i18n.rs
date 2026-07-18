//! Locale-aware text resolution (opt-in via the `i18n` feature flag).
//!
//! This module is the canonical Autumn answer to the i18n question that every
//! Spring Boot / Rails / Phoenix migrant asks within their first afternoon:
//! *"How do I localize my templates?"* It ships a thin layer over Project
//! Fluent's `.ftl` syntax with three opinionated conventions:
//!
//! 1. **Translations live at `i18n/<locale>.ftl`** (relative to the project
//!    root, discovered at startup via [`Bundle::load_from_dir`]).
//! 2. **A request-scoped [`Locale`] extractor** resolves the active locale
//!    from query string, cookie, `Accept-Language` header, and configured
//!    default — in that order.
//! 3. **A [`t!`](crate::t) macro** performs the actual key lookup with
//!    automatic fallback to the default locale, emitting a rate-limited
//!    `tracing::warn!` on misses.
//!
//! # Quick start
//!
//! Enable the feature flag in your `Cargo.toml`:
//!
//! ```toml
//! autumn-web = { version = "0.3", features = ["i18n"] }
//! ```
//!
//! Configure supported locales in `autumn.toml`:
//!
//! ```toml
//! [i18n]
//! default_locale = "en"
//! supported_locales = ["en", "es"]
//! ```
//!
//! Drop a translation file at `i18n/en.ftl`:
//!
//! ```text
//! welcome.title = Welcome to my blog
//! welcome.greeting = Hello, { $name }!
//! ```
//!
//! And use the macro from a handler:
//!
//! ```ignore
//! use autumn_web::prelude::*;
//! use autumn_web::i18n::Locale;
//!
//! #[get("/")]
//! async fn index(locale: Locale) -> Markup {
//!     html! { h1 { (t!(locale, "welcome.title")) } }
//! }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde::Deserialize;

/// Static configuration for the i18n subsystem.
///
/// Populated from the `[i18n]` block in `autumn.toml` (or programmatically
/// via [`AutumnConfig`](crate::config::AutumnConfig) if the application is
/// constructed in code).
///
/// Defaults are conservative: a single English locale and a one-link
/// fallback chain (default locale only). Apps that want a richer fallback
/// (e.g. `pt-BR -> pt -> en`) configure it explicitly.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct I18nConfig {
    /// Locale used when the request locale cannot be resolved. Must be
    /// listed in [`Self::supported_locales`].
    pub default_locale: String,

    /// Locales the application is willing to serve. The [`Locale`]
    /// extractor will negotiate against this list when reading the
    /// `Accept-Language` header. Order is significant only as
    /// documentation — the resolution algorithm picks the best match.
    pub supported_locales: Vec<String>,

    /// Optional explicit fallback chain. When a key is missing from the
    /// active locale, [`Bundle::translate`] walks this chain in order
    /// before giving up and returning the key itself.
    ///
    /// When empty, falls back to `[default_locale]`.
    pub fallback_chain: Vec<String>,

    /// Filesystem directory containing `<locale>.ftl` files, relative to
    /// the application's manifest directory. Defaults to `"i18n"`.
    pub dir: String,
}

impl Default for I18nConfig {
    fn default() -> Self {
        Self {
            default_locale: "en".to_owned(),
            supported_locales: vec!["en".to_owned()],
            fallback_chain: Vec::new(),
            dir: "i18n".to_owned(),
        }
    }
}

impl I18nConfig {
    /// Resolved fallback chain. Always ends with [`Self::default_locale`].
    #[must_use]
    pub fn resolved_fallback_chain(&self) -> Vec<String> {
        if self.fallback_chain.is_empty() {
            return vec![self.default_locale.clone()];
        }
        let mut chain = self.fallback_chain.clone();
        if !chain.iter().any(|l| l == &self.default_locale) {
            chain.push(self.default_locale.clone());
        }
        chain
    }
}

/// Errors produced by [`Bundle::load_from_dir`].
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The configured `i18n/` directory could not be read.
    #[error("i18n directory `{path}` could not be read: {source}")]
    DirectoryRead {
        /// Path that the framework attempted to read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The default locale's `.ftl` file was missing — fail-fast at startup
    /// rather than silently shipping a broken app.
    #[error("i18n default locale `{locale}` is missing — expected file at `{path}`")]
    MissingDefaultLocale {
        /// The configured default locale name.
        locale: String,
        /// Path the framework expected the file at.
        path: PathBuf,
    },

    /// One of the `.ftl` files contained a syntax error.
    #[error("failed to parse `{path}` at line {line}: {message}")]
    Parse {
        /// File that failed to parse.
        path: PathBuf,
        /// 1-based line number where parsing failed.
        line: usize,
        /// Human-readable description of what went wrong.
        message: String,
    },

    /// A `.ftl` file's name did not look like a recognizable locale tag.
    #[error("file `{path}` is not a recognizable locale tag")]
    InvalidLocaleFilename {
        /// Path of the offending file.
        path: PathBuf,
    },
}

/// Active request locale, resolved by the [`FromRequestParts`] impl below.
///
/// # Resolution order
///
/// 1. `?locale=xx` query parameter (explicit override, useful for testing)
/// 2. `autumn_locale` cookie (set by application code, e.g. on a switcher
///    form submit)
/// 3. `Accept-Language` header, negotiated against the configured
///    [`I18nConfig::supported_locales`]
/// 4. [`I18nConfig::default_locale`]
///
/// This is stable and documented: applications can rely on the order. If
/// step 1–3 produce a locale that is **not** in the supported list, the
/// extractor falls through to the next step rather than serving an
/// unsupported locale.
#[derive(Debug, Clone)]
pub struct Locale {
    tag: String,
    bundle: Option<Arc<Bundle>>,
}

impl Locale {
    /// Construct a [`Locale`] for testing without going through extraction.
    #[must_use]
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            bundle: None,
        }
    }

    /// The resolved locale tag (e.g. `"en"`, `"es-MX"`).
    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Attach a runtime [`Bundle`] for translation lookups.
    #[must_use]
    pub fn with_bundle(mut self, bundle: Arc<Bundle>) -> Self {
        self.bundle = Some(bundle);
        self
    }

    /// Returns the bundle attached during request extraction, if any.
    #[must_use]
    pub const fn bundle(&self) -> Option<&Arc<Bundle>> {
        self.bundle.as_ref()
    }

    /// Translate a key in this locale, returning the translated string or
    /// (if the key is missing in every locale) the key itself.
    #[must_use]
    pub fn t(&self, key: &str) -> String {
        self.t_with(key, &[])
    }

    /// Translate a key with named arguments.
    #[must_use]
    pub fn t_with(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.bundle
            .as_ref()
            .map_or_else(|| key.to_owned(), |b| b.translate(&self.tag, key, args))
    }
}

/// Resolve a locale string against a list of supported locales.
///
/// Negotiation strategy:
/// - Exact match wins (e.g. `"es-MX"` matches `"es-MX"`).
/// - Otherwise the primary subtag is tried (e.g. `"es-MX"` matches `"es"`).
/// - Returns `None` if neither matches.
#[must_use]
pub fn negotiate<'a>(requested: &str, supported: &'a [String]) -> Option<&'a str> {
    let normalized = requested.trim();
    for s in supported {
        if s.eq_ignore_ascii_case(normalized) {
            return Some(s.as_str());
        }
    }
    if let Some((primary, _)) = normalized.split_once('-') {
        for s in supported {
            if s.eq_ignore_ascii_case(primary) {
                return Some(s.as_str());
            }
        }
    }
    None
}

/// Pick the best locale from an `Accept-Language` header value.
///
/// Parses comma-separated `lang;q=0.9` pairs, sorts by descending `q`, and
/// negotiates each against `supported`. Returns `None` if no entry matches.
#[must_use]
pub fn parse_accept_language<'a>(header: &str, supported: &'a [String]) -> Option<&'a str> {
    let mut entries: Vec<(f32, &str)> = header
        .split(',')
        .filter_map(|raw| {
            let mut parts = raw.split(';');
            let tag = parts.next()?.trim();
            if tag.is_empty() || tag == "*" {
                return None;
            }
            let q = parts
                .find_map(|p| {
                    let p = p.trim();
                    p.strip_prefix("q=").and_then(|v| v.parse::<f32>().ok())
                })
                .unwrap_or(1.0);
            if q.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                return None;
            }
            Some((q, tag))
        })
        .collect();
    // Stable sort by q descending so equal-q entries preserve header order.
    entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (_, tag) in entries {
        if let Some(matched) = negotiate(tag, supported) {
            return Some(matched);
        }
    }
    None
}

/// Runtime translation store. Loaded once at startup and stored as an
/// [`AppState`](crate::state::AppState) extension.
pub struct Bundle {
    /// `locale_tag -> { key -> message }`.
    messages: HashMap<String, HashMap<String, String>>,
    /// Resolution chain to walk on missing keys, ending in default locale.
    fallback_chain: Vec<String>,
    /// Default locale (for warn deduplication and last-resort fallback).
    default_locale: String,
    /// Configured supported locales (for the [`Locale`] extractor).
    supported_locales: Vec<String>,
    /// Per-key warn de-duplication so a missing key on a hot path doesn't
    /// flood the log. Stored alongside last-warned timestamp.
    miss_warnings: std::sync::Mutex<HashMap<(String, String), Instant>>,
    /// How frequently the same `(locale, key)` miss is allowed to warn.
    warn_dedup_window: Duration,
    /// Total number of misses observed (test instrumentation).
    miss_count: AtomicU64,
}

impl fmt::Debug for Bundle {
    /// Eliminates heap allocation during debug formatting by passing the natively debuggable Keys iterator.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bundle")
            .field("locales", &self.messages.keys())
            .field("default_locale", &self.default_locale)
            .field("supported_locales", &self.supported_locales)
            .field("miss_count", &self.miss_count.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Bundle {
    /// Load all `<locale>.ftl` files from `dir` according to `config`.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::DirectoryRead`] if the directory cannot be
    /// listed, [`LoadError::MissingDefaultLocale`] if the default
    /// locale's file is absent (fail-fast), or [`LoadError::Parse`] if a
    /// file has a syntax error.
    pub fn load_from_dir(dir: &Path, config: &I18nConfig) -> Result<Self, LoadError> {
        let mut messages: HashMap<String, HashMap<String, String>> = HashMap::new();

        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                // Treat missing dir as "no translations yet" so the
                // missing-default-file branch produces a clear error.
                return Err(LoadError::MissingDefaultLocale {
                    locale: config.default_locale.clone(),
                    path: dir.join(format!("{}.ftl", config.default_locale)),
                });
            }
            Err(source) => {
                return Err(LoadError::DirectoryRead {
                    path: dir.to_path_buf(),
                    source,
                });
            }
        };

        for entry in entries {
            let entry = entry.map_err(|source| LoadError::DirectoryRead {
                path: dir.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ftl") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| LoadError::InvalidLocaleFilename { path: path.clone() })?
                .to_owned();
            let raw =
                std::fs::read_to_string(&path).map_err(|source| LoadError::DirectoryRead {
                    path: path.clone(),
                    source,
                })?;
            let parsed = parse_ftl(&raw, &path)?;
            messages.insert(stem, parsed);
        }

        Self::from_locale_messages(
            messages,
            config,
            dir.join(format!("{}.ftl", config.default_locale)),
        )
    }

    /// Validate that the default locale is present and assemble the bundle.
    ///
    /// Shared tail of [`load_from_dir`](Self::load_from_dir) and
    /// [`load_from_embedded`](Self::load_from_embedded): only their file-source
    /// loops differ. `missing_default_path` is the path reported when the
    /// default locale's file is absent.
    fn from_locale_messages(
        messages: HashMap<String, HashMap<String, String>>,
        config: &I18nConfig,
        missing_default_path: PathBuf,
    ) -> Result<Self, LoadError> {
        if !messages.contains_key(&config.default_locale) {
            return Err(LoadError::MissingDefaultLocale {
                locale: config.default_locale.clone(),
                path: missing_default_path,
            });
        }

        Ok(Self {
            messages,
            fallback_chain: config.resolved_fallback_chain(),
            default_locale: config.default_locale.clone(),
            supported_locales: config.supported_locales.clone(),
            miss_warnings: std::sync::Mutex::new(HashMap::new()),
            warn_dedup_window: Duration::from_secs(60),
            miss_count: AtomicU64::new(0),
        })
    }

    /// Load all `<locale>.ftl` files from a directory embedded into the binary
    /// at compile time (via [`embed_locales!`](crate::embed_locales)).
    ///
    /// The embedded counterpart to [`load_from_dir`](Self::load_from_dir): same
    /// `.ftl` discovery, same [`parse_ftl`] parsing, and the same fail-fast on a
    /// missing default locale — but every byte comes from the binary, so no
    /// `i18n/` sidecar directory is required at runtime.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::MissingDefaultLocale`] if the default locale's file
    /// is absent, [`LoadError::InvalidLocaleFilename`] if a file name or its
    /// contents are not valid UTF-8, or [`LoadError::Parse`] on a syntax error.
    #[cfg(feature = "embed-assets")]
    pub fn load_from_embedded(
        dir: &include_dir::Dir,
        config: &I18nConfig,
    ) -> Result<Self, LoadError> {
        let mut messages: HashMap<String, HashMap<String, String>> = HashMap::new();

        for file in dir.files() {
            let path = file.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ftl") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| LoadError::InvalidLocaleFilename {
                    path: path.to_path_buf(),
                })?
                .to_owned();
            let raw = file
                .contents_utf8()
                .ok_or_else(|| LoadError::InvalidLocaleFilename {
                    path: path.to_path_buf(),
                })?;
            let parsed = parse_ftl(raw, path)?;
            messages.insert(stem, parsed);
        }

        Self::from_locale_messages(
            messages,
            config,
            PathBuf::from(format!("{}.ftl", config.default_locale)),
        )
    }

    /// Construct a bundle directly from in-memory translations (test helper).
    #[must_use]
    pub fn from_messages(
        messages: HashMap<String, HashMap<String, String>>,
        config: &I18nConfig,
    ) -> Self {
        Self {
            messages,
            fallback_chain: config.resolved_fallback_chain(),
            default_locale: config.default_locale.clone(),
            supported_locales: config.supported_locales.clone(),
            miss_warnings: std::sync::Mutex::new(HashMap::new()),
            warn_dedup_window: Duration::from_secs(60),
            miss_count: AtomicU64::new(0),
        }
    }

    /// Available locales (those with a loaded `.ftl` file).
    #[must_use]
    pub fn locales(&self) -> Vec<&str> {
        self.messages.keys().map(String::as_str).collect()
    }

    /// Locales the application is configured to serve.
    #[must_use]
    pub fn supported_locales(&self) -> &[String] {
        &self.supported_locales
    }

    /// Default locale tag.
    #[must_use]
    pub fn default_locale(&self) -> &str {
        &self.default_locale
    }

    /// Resolved fallback chain (always ends in default locale).
    #[must_use]
    pub fn fallback_chain(&self) -> &[String] {
        &self.fallback_chain
    }

    /// Total recorded misses (test instrumentation).
    #[must_use]
    pub fn miss_count(&self) -> u64 {
        self.miss_count.load(Ordering::Relaxed)
    }

    /// Translate a key in the requested locale, falling back through the
    /// chain on miss. The returned string has any `{ $name }` placeables
    /// substituted with values from `args`; missing args are left as-is so
    /// the bug is visible in development.
    pub fn translate(&self, locale: &str, key: &str, args: &[(&str, &str)]) -> String {
        if let Some(template) = self.lookup_template(locale, key) {
            return interpolate(template, args);
        }
        self.record_miss(locale, key);
        // Final fallback: return the key itself so a missing translation is
        // visible but the page still renders.
        format!("{{${key}}}")
    }

    fn lookup_template(&self, locale: &str, key: &str) -> Option<&str> {
        if let Some(found) = self.messages.get(locale).and_then(|m| m.get(key)) {
            return Some(found.as_str());
        }
        for fallback in &self.fallback_chain {
            if fallback == locale {
                continue;
            }
            if let Some(found) = self.messages.get(fallback).and_then(|m| m.get(key)) {
                return Some(found.as_str());
            }
        }
        None
    }

    fn record_miss(&self, locale: &str, key: &str) {
        self.miss_count.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let should_warn = {
            // Default a missing entry far enough in the past that the first
            // miss warns immediately; saturating_sub guards against a
            // hypothetical very-early Instant on platforms where the boot
            // reference is small.
            let stale = now
                .checked_sub(self.warn_dedup_window + Duration::from_secs(1))
                .unwrap_or(now);
            let miss_key = (locale.to_owned(), key.to_owned());
            let mut guard = match self.miss_warnings.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let last_warned = guard.get(&miss_key).copied().unwrap_or(stale);
            if now.duration_since(last_warned) >= self.warn_dedup_window {
                guard.insert(miss_key, now);
                true
            } else {
                false
            }
        };
        if should_warn {
            tracing::warn!(
                target: "autumn::i18n",
                locale = %locale,
                key = %key,
                "i18n key missing in requested and fallback locales",
            );
        }
    }
}

/// Minimal Fluent-syntax parser. Supports the common subset used in real-world
/// `.ftl` files:
///
/// - Comments starting with `#` (line comments).
/// - `key = value` entries.
/// - Multi-line continuations: subsequent lines indented with at least one
///   space are appended to the previous value, joined with a single space.
/// - Blank lines between entries.
///
/// More elaborate Fluent features (terms with `-`, selectors, `NUMBER`) are
/// not parsed in this minimal pass; they would round-trip as a single string
/// via the `=` separator and produce a useful (if unprocessed) value. A
/// future enhancement can swap this for `fluent-bundle` without changing
/// the public API.
fn parse_ftl(src: &str, path: &Path) -> Result<HashMap<String, String>, LoadError> {
    let mut messages = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_value = String::new();

    let flush =
        |messages: &mut HashMap<String, String>, key: &mut Option<String>, value: &mut String| {
            if let Some(k) = key.take() {
                messages.insert(k, std::mem::take(value).trim().to_owned());
            }
        };

    for (idx, line) in src.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim_start();

        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut messages, &mut current_key, &mut current_value);
            continue;
        }

        let starts_indented = line.starts_with(' ') || line.starts_with('\t');
        if starts_indented {
            if current_key.is_some() {
                if !current_value.is_empty() {
                    current_value.push(' ');
                }
                current_value.push_str(trimmed);
                continue;
            }
            return Err(LoadError::Parse {
                path: path.to_path_buf(),
                line: line_no,
                message: format!("indented continuation has no preceding key: `{trimmed}`"),
            });
        }

        // New entry — flush any previous one.
        flush(&mut messages, &mut current_key, &mut current_value);

        let Some((raw_key, raw_value)) = trimmed.split_once('=') else {
            return Err(LoadError::Parse {
                path: path.to_path_buf(),
                line: line_no,
                message: format!("expected `key = value`, got: `{trimmed}`"),
            });
        };
        let key = raw_key.trim();
        if key.is_empty() {
            return Err(LoadError::Parse {
                path: path.to_path_buf(),
                line: line_no,
                message: "empty key before `=`".to_owned(),
            });
        }
        current_key = Some(key.to_owned());
        raw_value.trim().clone_into(&mut current_value);
    }

    flush(&mut messages, &mut current_key, &mut current_value);
    Ok(messages)
}

/// Substitute `{ $name }` placeables. Unknown variables are left untouched
/// so the bug is visible in dev rather than producing a silent empty string.
fn interpolate(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(open) = remaining.find('{') {
        out.push_str(&remaining[..open]);
        let after_open = &remaining[open + 1..];
        let Some(close_rel) = after_open.find('}') else {
            // Unterminated `{` — emit the rest verbatim and stop.
            out.push_str(&remaining[open..]);
            return out;
        };
        let inside = after_open[..close_rel].trim();
        let after_close = &after_open[close_rel + 1..];
        match inside.strip_prefix('$') {
            Some(var) if !var.is_empty() => {
                let var = var.trim();
                if let Some((_, val)) = args.iter().find(|(k, _)| *k == var) {
                    out.push_str(val);
                } else {
                    // Unknown var — preserve the original placeable literally.
                    out.push('{');
                    out.push_str(&after_open[..close_rel]);
                    out.push('}');
                }
            }
            _ => {
                // Not a `$name` placeable — pass through verbatim.
                out.push('{');
                out.push_str(&after_open[..close_rel]);
                out.push('}');
            }
        }
        remaining = after_close;
    }
    out.push_str(remaining);
    out
}

impl<S> FromRequestParts<S> for Locale
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Pull bundle from request extensions if a previous middleware
        // installed one. (Bundles are normally registered as an AppState
        // extension and threaded through via a layer; for tests we build
        // [`Locale`] directly via [`Locale::new`] / [`Locale::with_bundle`].)
        let bundle = parts.extensions.get::<Arc<Bundle>>().cloned();
        let supported: Vec<String> = bundle
            .as_ref()
            .map(|b| b.supported_locales.clone())
            .unwrap_or_default();
        let default = bundle
            .as_ref()
            .map_or_else(|| "en".to_owned(), |b| b.default_locale.clone());

        // Resolution order: query → session (signed cookie) → plain cookie
        // (legacy / sessions-off) → Accept-Language → default.
        let mut resolved = resolve_query_override(parts, &supported);
        if resolved.is_none() {
            resolved = resolve_from_session(parts, &supported).await;
        }
        if resolved.is_none() {
            resolved = resolve_from_plain_cookie(parts, &supported);
        }
        if resolved.is_none() {
            resolved = resolve_from_accept_language(parts, &supported);
        }
        let resolved = resolved.unwrap_or(default);

        let mut locale = Self::new(resolved);
        if let Some(bundle) = bundle {
            locale = locale.with_bundle(bundle);
        }
        Ok(locale)
    }
}

/// Session key used for the persisted locale.
///
/// Apps that want the locale switcher to persist via the framework's
/// signed session cookie write to this key (see
/// [`set_locale_in_session`]). The [`Locale`] extractor reads it
/// automatically when a [`Session`](crate::session::Session) is in
/// request extensions.
pub const LOCALE_SESSION_KEY: &str = "autumn_locale";

fn resolve_query_override(parts: &Parts, supported: &[String]) -> Option<String> {
    let query = parts.uri.query()?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("locale=")
            && let Some(matched) = negotiate(value, supported)
        {
            return Some(matched.to_owned());
        }
    }
    None
}

async fn resolve_from_session(parts: &Parts, supported: &[String]) -> Option<String> {
    // Session is published into request extensions by SessionLayer. When it
    // isn't (e.g. session feature disabled or layer not installed), this
    // simply falls through. Reading is async because the session uses an
    // RwLock under the hood — but no I/O happens here.
    let session = parts.extensions.get::<crate::session::Session>().cloned()?;
    let value = session.get(LOCALE_SESSION_KEY).await?;
    negotiate(&value, supported).map(str::to_owned)
}

fn resolve_from_plain_cookie(parts: &Parts, supported: &[String]) -> Option<String> {
    let cookie_header = parts
        .headers
        .get(axum::http::header::COOKIE)
        .and_then(|h| h.to_str().ok())?;
    for cookie in cookie_header.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix("autumn_locale=")
            && let Some(matched) = negotiate(value, supported)
        {
            return Some(matched.to_owned());
        }
    }
    None
}

fn resolve_from_accept_language(parts: &Parts, supported: &[String]) -> Option<String> {
    let header = parts
        .headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|h| h.to_str().ok())?;
    parse_accept_language(header, supported).map(str::to_owned)
}

/// Persist a locale choice into the signed session cookie.
///
/// This is the recommended way to remember a user's language switch —
/// the value rides on the framework's HMAC-signed session cookie so a
/// hostile client cannot forge it. Apps that don't use sessions can
/// fall back to the unsigned [`set_locale_cookie`] helper.
///
/// # Examples
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::i18n::set_locale_in_session;
///
/// #[post("/locale/{locale}")]
/// async fn switch(session: Session, Path(locale): Path<String>) -> impl IntoResponse {
///     set_locale_in_session(&session, &locale).await;
///     axum::response::Redirect::to("/")
/// }
/// ```
pub async fn set_locale_in_session(session: &crate::session::Session, locale: &str) {
    session.insert(LOCALE_SESSION_KEY, locale).await;
}

/// Produce a value for the `Set-Cookie` response header that persists the
/// chosen locale across requests. The cookie is `Path=/` and lives a year.
///
/// The locale is percent-encoded before formatting so cookie delimiters
/// cannot inject additional attributes.
///
/// This is unsigned; signing requires session integration. Apps that don't
/// trust their own UI should keep relying on `Accept-Language`.
#[must_use]
pub fn set_locale_cookie(locale: &str) -> String {
    let locale = encode_locale_cookie_value(locale);
    format!("autumn_locale={locale}; Path=/; Max-Age=31536000; SameSite=Lax")
}

fn encode_locale_cookie_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_locale_cookie_value_byte(byte) {
            encoded.push(char::from(byte));
        } else {
            push_percent_encoded(&mut encoded, byte);
        }
    }
    encoded
}

const fn is_locale_cookie_value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn push_percent_encoded(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(char::from(HEX[(byte >> 4) as usize]));
    output.push(char::from(HEX[(byte & 0x0f) as usize]));
}

/// Translate a key in the active locale, with **compile-time validation**
/// that the key exists in the default locale's `.ftl` file.
///
/// The macro itself lives in [`autumn_macros`] (the proc-macro crate). See
/// its [`t`] docs for the compile-time check semantics
/// and the env-var contract used to locate the default-locale bundle.
///
/// # Forms
///
/// ```ignore
/// t!(locale, "welcome.title")
/// t!(locale, "welcome.greeting", name = "Ada")
/// ```
///
/// # Runtime behaviour
///
/// Always returns a [`String`]. On a runtime miss (key not present in any
/// fallback locale), returns `{$key}` and emits a rate-limited
/// `tracing::warn!` so the omission is visible without flooding logs.
pub use autumn_macros::t;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};

    fn build_parts(uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut req = Request::builder().uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = req.body(Body::empty()).unwrap();
        let (parts, _) = req.into_parts();
        parts
    }

    fn cfg(default: &str, supported: &[&str]) -> I18nConfig {
        I18nConfig {
            default_locale: default.to_owned(),
            supported_locales: supported.iter().map(|s| (*s).to_owned()).collect(),
            fallback_chain: vec![],
            dir: "i18n".to_owned(),
        }
    }

    fn bundle_with(locales: &[(&str, &[(&str, &str)])], cfg: &I18nConfig) -> Bundle {
        let mut messages = HashMap::new();
        for (loc, kvs) in locales {
            let mut m = HashMap::new();
            for (k, v) in *kvs {
                m.insert((*k).to_owned(), (*v).to_owned());
            }
            messages.insert((*loc).to_owned(), m);
        }
        Bundle::from_messages(messages, cfg)
    }

    // ── Config ────────────────────────────────────────────────────

    #[test]
    fn i18n_config_default_is_english_only() {
        let cfg = I18nConfig::default();
        assert_eq!(cfg.default_locale, "en");
        assert_eq!(cfg.supported_locales, vec!["en".to_owned()]);
        assert_eq!(cfg.dir, "i18n");
    }

    #[test]
    fn fallback_chain_defaults_to_default_locale() {
        let cfg = cfg("en", &["en", "es"]);
        assert_eq!(cfg.resolved_fallback_chain(), vec!["en".to_owned()]);
    }

    #[test]
    fn fallback_chain_appends_default_when_missing() {
        let mut cfg = cfg("en", &["en", "es", "pt-BR"]);
        cfg.fallback_chain = vec!["pt".to_owned(), "es".to_owned()];
        let chain = cfg.resolved_fallback_chain();
        assert_eq!(
            chain,
            vec!["pt".to_owned(), "es".to_owned(), "en".to_owned()]
        );
    }

    #[test]
    fn fallback_chain_keeps_user_order_when_default_present() {
        let mut cfg = cfg("en", &["en", "es"]);
        cfg.fallback_chain = vec!["es".to_owned(), "en".to_owned()];
        assert_eq!(
            cfg.resolved_fallback_chain(),
            vec!["es".to_owned(), "en".to_owned()]
        );
    }

    // ── FTL parsing ───────────────────────────────────────────────

    #[test]
    fn parse_ftl_basic_keys() {
        let src = "welcome.title = Hi\ngreeting = Hello, { $name }!\n";
        let parsed = parse_ftl(src, Path::new("test.ftl")).unwrap();
        assert_eq!(parsed.get("welcome.title").map(String::as_str), Some("Hi"));
        assert_eq!(
            parsed.get("greeting").map(String::as_str),
            Some("Hello, { $name }!")
        );
    }

    #[test]
    fn parse_ftl_skips_comments_and_blank_lines() {
        let src = "# header comment\n\nwelcome = Hi\n\n# trailing\n";
        let parsed = parse_ftl(src, Path::new("test.ftl")).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get("welcome").map(String::as_str), Some("Hi"));
    }

    #[test]
    fn parse_ftl_supports_indented_continuation() {
        let src = "long = first line\n  second line\n  third\n";
        let parsed = parse_ftl(src, Path::new("test.ftl")).unwrap();
        assert_eq!(
            parsed.get("long").map(String::as_str),
            Some("first line second line third")
        );
    }

    #[test]
    fn parse_ftl_rejects_missing_equals() {
        let err = parse_ftl("oops no equals here\n", Path::new("test.ftl")).unwrap_err();
        match err {
            LoadError::Parse { line, .. } => assert_eq!(line, 1),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn parse_ftl_rejects_orphan_continuation() {
        let err = parse_ftl("  orphan continuation\n", Path::new("test.ftl")).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }));
    }

    // ── Bundle::load_from_dir ────────────────────────────────────

    #[test]
    fn load_from_dir_errors_when_default_locale_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("es.ftl"), "hi = Hola\n").unwrap();
        let cfg = cfg("en", &["en", "es"]);
        let err = Bundle::load_from_dir(tmp.path(), &cfg).unwrap_err();
        match err {
            LoadError::MissingDefaultLocale { locale, path } => {
                assert_eq!(locale, "en");
                assert!(path.ends_with("en.ftl"));
            }
            other => panic!("expected MissingDefaultLocale, got {other:?}"),
        }
    }

    #[test]
    fn load_from_dir_loads_all_ftl_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("en.ftl"), "hi = Hi\n").unwrap();
        std::fs::write(tmp.path().join("es.ftl"), "hi = Hola\n").unwrap();
        // README.md should be ignored.
        std::fs::write(tmp.path().join("README.md"), "ignore me").unwrap();
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Bundle::load_from_dir(tmp.path(), &cfg).unwrap();
        let mut locales = bundle.locales();
        locales.sort_unstable();
        assert_eq!(locales, vec!["en", "es"]);
    }

    #[test]
    fn load_from_dir_propagates_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("en.ftl"), "no equals here\n").unwrap();
        let cfg = cfg("en", &["en"]);
        let err = Bundle::load_from_dir(tmp.path(), &cfg).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }));
    }

    #[test]
    fn load_from_dir_missing_directory_is_treated_as_missing_default() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let cfg = cfg("en", &["en"]);
        let err = Bundle::load_from_dir(&missing, &cfg).unwrap_err();
        assert!(matches!(err, LoadError::MissingDefaultLocale { .. }));
    }

    // ── Bundle::translate ────────────────────────────────────────

    #[test]
    fn translate_returns_translated_string() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = bundle_with(&[("en", &[("hi", "Hi")]), ("es", &[("hi", "Hola")])], &cfg);
        assert_eq!(bundle.translate("es", "hi", &[]), "Hola");
        assert_eq!(bundle.translate("en", "hi", &[]), "Hi");
    }

    #[test]
    fn translate_falls_back_to_default_locale_on_miss() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = bundle_with(
            &[
                ("en", &[("only_in_en", "english only")]),
                ("es", &[("hi", "Hola")]),
            ],
            &cfg,
        );
        assert_eq!(bundle.translate("es", "only_in_en", &[]), "english only");
    }

    #[test]
    fn translate_uses_explicit_fallback_chain() {
        let mut cfg = cfg("en", &["en", "es", "pt-BR"]);
        cfg.fallback_chain = vec!["pt".to_owned(), "es".to_owned(), "en".to_owned()];
        let bundle = bundle_with(
            &[
                ("en", &[]),
                ("pt", &[("howdy", "Olá")]),
                ("es", &[("howdy", "Hola")]),
            ],
            &cfg,
        );
        // pt-BR falls back to pt before es.
        assert_eq!(bundle.translate("pt-BR", "howdy", &[]), "Olá");
    }

    #[test]
    fn translate_returns_marker_on_total_miss_and_records() {
        let cfg = cfg("en", &["en"]);
        let bundle = bundle_with(&[("en", &[])], &cfg);
        let result = bundle.translate("en", "definitely.missing", &[]);
        assert_eq!(result, "{$definitely.missing}");
        assert_eq!(bundle.miss_count(), 1);
    }

    #[test]
    fn translate_dedups_warnings_for_same_key() {
        let cfg = cfg("en", &["en"]);
        let bundle = bundle_with(&[("en", &[])], &cfg);
        for _ in 0..5 {
            let _ = bundle.translate("en", "missing", &[]);
        }
        assert_eq!(bundle.miss_count(), 5);
        // No assertion on warn output (handled by subscriber); the public
        // contract is that misses are counted.
    }

    #[test]
    fn translate_substitutes_named_args() {
        let cfg = cfg("en", &["en"]);
        let bundle = bundle_with(&[("en", &[("greeting", "Hello, { $name }!")])], &cfg);
        assert_eq!(
            bundle.translate("en", "greeting", &[("name", "Ada")]),
            "Hello, Ada!"
        );
    }

    #[test]
    fn translate_leaves_unknown_args_visible() {
        let cfg = cfg("en", &["en"]);
        let bundle = bundle_with(&[("en", &[("greeting", "Hello, { $name }!")])], &cfg);
        // No matching arg → keep template form, so the bug is loud.
        let out = bundle.translate("en", "greeting", &[]);
        assert!(out.contains("{ $name }"), "got: {out}");
    }

    // ── Locale resolution ────────────────────────────────────────

    #[test]
    fn negotiate_exact_match() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        assert_eq!(negotiate("es", &supported), Some("es"));
    }

    #[test]
    fn negotiate_falls_back_to_primary_subtag() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        assert_eq!(negotiate("es-MX", &supported), Some("es"));
    }

    #[test]
    fn negotiate_returns_none_when_unsupported() {
        let supported = vec!["en".to_owned()];
        assert_eq!(negotiate("ja", &supported), None);
    }

    #[test]
    fn parse_accept_language_picks_highest_q() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let header = "fr;q=0.9, es;q=0.8, en;q=0.7";
        // fr is unsupported; es wins over en because q=0.8 > 0.7.
        assert_eq!(parse_accept_language(header, &supported), Some("es"));
    }

    #[test]
    fn parse_accept_language_skips_wildcard() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        assert_eq!(parse_accept_language("*", &supported), None);
    }

    #[test]
    fn parse_accept_language_ignores_zero_quality_entries() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        assert_eq!(parse_accept_language("es;q=0, en;q=0", &supported), None);
    }

    #[tokio::test]
    async fn locale_extractor_uses_query_override() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[])], &cfg));
        let mut parts = build_parts("/?locale=es", &[(header::ACCEPT_LANGUAGE.as_str(), "en")]);
        parts.extensions.insert(bundle.clone());
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "es");
    }

    #[tokio::test]
    async fn locale_extractor_uses_cookie_when_no_query() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[])], &cfg));
        let mut parts = build_parts(
            "/",
            &[
                (header::COOKIE.as_str(), "autumn_locale=es; other=foo"),
                (header::ACCEPT_LANGUAGE.as_str(), "en"),
            ],
        );
        parts.extensions.insert(bundle.clone());
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "es");
    }

    #[tokio::test]
    async fn locale_extractor_negotiates_accept_language() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[])], &cfg));
        let mut parts = build_parts(
            "/",
            &[(header::ACCEPT_LANGUAGE.as_str(), "es-MX,es;q=0.9,en;q=0.8")],
        );
        parts.extensions.insert(bundle.clone());
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "es");
    }

    #[tokio::test]
    async fn locale_extractor_falls_through_to_default() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[])], &cfg));
        let mut parts = build_parts(
            "/?locale=ja",
            &[(header::ACCEPT_LANGUAGE.as_str(), "ja-JP")],
        );
        parts.extensions.insert(bundle.clone());
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "en");
    }

    #[tokio::test]
    async fn locale_extractor_resolution_order_is_query_then_cookie() {
        // Query wins over cookie.
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[])], &cfg));
        let mut parts = build_parts(
            "/?locale=es",
            &[(header::COOKIE.as_str(), "autumn_locale=en")],
        );
        parts.extensions.insert(bundle.clone());
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "es");
    }

    #[test]
    fn set_locale_cookie_format() {
        let cookie = set_locale_cookie("es");
        assert!(cookie.starts_with("autumn_locale=es"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("SameSite=Lax"));
    }

    #[test]
    fn set_locale_cookie_percent_encodes_cookie_delimiters() {
        let cookie = set_locale_cookie("es; Secure; SameSite=None");

        assert!(cookie.starts_with("autumn_locale=es%3B%20Secure%3B%20SameSite%3DNone;"));
        assert!(!cookie.starts_with("autumn_locale=es; Secure;"));
    }

    // ── Session integration ─────────────────────────────────────

    #[tokio::test]
    async fn locale_extractor_reads_signed_session_cookie() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[])], &cfg));
        let session = crate::session::Session::new_for_test(
            "test-id".to_owned(),
            std::collections::HashMap::new(),
        );
        crate::i18n::set_locale_in_session(&session, "es").await;

        let mut parts = build_parts("/", &[(axum::http::header::ACCEPT_LANGUAGE.as_str(), "en")]);
        parts.extensions.insert(bundle.clone());
        parts.extensions.insert(session);
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "es");
    }

    #[tokio::test]
    async fn signed_session_locale_overrides_plain_cookie() {
        let cfg = cfg("en", &["en", "es", "fr"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[]), ("fr", &[])], &cfg));
        let session = crate::session::Session::new_for_test(
            "test-id".to_owned(),
            std::collections::HashMap::new(),
        );
        crate::i18n::set_locale_in_session(&session, "fr").await;

        let mut parts = build_parts(
            "/",
            &[(axum::http::header::COOKIE.as_str(), "autumn_locale=es")],
        );
        parts.extensions.insert(bundle.clone());
        parts.extensions.insert(session);
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "fr");
    }

    #[tokio::test]
    async fn query_still_overrides_session() {
        let cfg = cfg("en", &["en", "es", "fr"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[]), ("fr", &[])], &cfg));
        let session = crate::session::Session::new_for_test(
            "test-id".to_owned(),
            std::collections::HashMap::new(),
        );
        crate::i18n::set_locale_in_session(&session, "fr").await;

        let mut parts = build_parts("/?locale=es", &[]);
        parts.extensions.insert(bundle.clone());
        parts.extensions.insert(session);
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "es");
    }

    #[tokio::test]
    async fn unsupported_session_locale_falls_through() {
        let cfg = cfg("en", &["en", "es"]);
        let bundle = Arc::new(bundle_with(&[("en", &[]), ("es", &[])], &cfg));
        let session = crate::session::Session::new_for_test(
            "test-id".to_owned(),
            std::collections::HashMap::new(),
        );
        crate::i18n::set_locale_in_session(&session, "ja").await;

        let mut parts = build_parts("/", &[(axum::http::header::ACCEPT_LANGUAGE.as_str(), "es")]);
        parts.extensions.insert(bundle.clone());
        parts.extensions.insert(session);
        let locale = Locale::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(locale.tag(), "es");
    }

    // ── t! macro ─────────────────────────────────────────────────

    #[test]
    fn t_macro_basic_lookup() {
        let cfg = cfg("en", &["en"]);
        let bundle = Arc::new(bundle_with(&[("en", &[("hi", "Hi")])], &cfg));
        let locale = Locale::new("en").with_bundle(bundle);
        assert_eq!(t!(locale, "hi"), "Hi");
    }

    #[test]
    fn t_macro_with_named_args() {
        let cfg = cfg("en", &["en"]);
        let bundle = Arc::new(bundle_with(&[("en", &[("g", "Hello, { $name }!")])], &cfg));
        let locale = Locale::new("en").with_bundle(bundle);
        assert_eq!(t!(locale, "g", name = "Ada"), "Hello, Ada!");
    }
}
