//! First-class SEO toolkit: sitemap.xml, robots.txt, and meta tag helpers.
//!
//! Autumn content apps need three artifacts for full crawl coverage:
//! a `sitemap.xml`, a `robots.txt`, and per-page meta tags. This module
//! provides builders and helpers for all three with sensible defaults.
//!
//! # Quick start
//!
//! ## Meta tags
//!
//! ```rust,ignore
//! use autumn_web::seo::SeoMeta;
//! use autumn_web::prelude::*;
//!
//! #[get("/posts/{slug}")]
//! async fn show(slug: Path<String>) -> Markup {
//!     let meta = SeoMeta::new()
//!         .title("My Blog Post")
//!         .description("A fascinating exploration of things")
//!         .canonical(format!("https://example.com/posts/{}", *slug))
//!         .og_image("https://example.com/og.jpg");
//!     html! {
//!         head { (meta.render()) }
//!     }
//! }
//! ```
//!
//! ## Route-level meta tag defaults
//!
//! Static values can be declared once on the route attribute instead of being
//! rebuilt in every handler. Add a `seo(...)` argument, then take a [`SeoMeta`]
//! parameter — it arrives pre-populated with the declared defaults:
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::seo::SeoMeta;
//!
//! #[get("/about", seo(title = "About • My Blog", description = "Learn about us"))]
//! async fn about(seo: SeoMeta) -> Markup {
//!     html! { head { (seo.render()) } }
//! }
//!
//! // The handler refines the attribute defaults with per-request values:
//! #[get("/posts/{slug}", seo(og_type = "article"))]
//! async fn show(slug: Path<String>, seo: SeoMeta) -> Markup {
//!     let seo = seo.title(format!("{} • Blog", *slug));
//!     html! { head { (seo.render()) } }
//! }
//! ```
//!
//! Every [`SeoMeta`] builder method has a matching `seo(...)` key: `title`,
//! `description`, `canonical`, `og_title`, `og_description`, `og_image`,
//! `og_type`, `og_url`, `twitter_card`, `twitter_title`,
//! `twitter_description`, `twitter_image`, and `robots`. Values must be string
//! literals; unknown or repeated keys are compile errors.
//!
//! The argument works on every HTTP route macro — [`get`](crate::get),
//! [`post`](crate::post), [`put`](crate::put), [`patch`](crate::patch),
//! [`delete`](crate::delete) — and on [`static_get`](crate::static_get), so
//! pre-rendered pages carry the same tags. ([`ws`](macro@crate::ws) takes a
//! path only: a WebSocket upgrade serves no crawlable document.)
//!
//! It supplies *values*, not markup — the handler still chooses where to emit
//! them, normally by embedding [`SeoMeta::render`] in a layout, so declaring
//! `seo(...)` on a handler that never takes a `SeoMeta` parameter renders
//! nothing. A handler that takes `SeoMeta` on a route without `seo(...)`
//! simply receives an empty builder; the extractor never fails.
//!
//! ## hreflang alternates (issue #1251)
//!
//! When locale-prefixed routing (`[i18n] locale_prefix_enabled = true`,
//! see [`crate::i18n`]) is on, [`locale_alternates`] builds the
//! `(hreflang, absolute URL)` pairs for a page's localized variants —
//! including an `x-default` entry — and [`SeoMeta::hreflang_alternates`]
//! renders them as `<link rel="alternate" hreflang="…">` tags:
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::seo::{SeoMeta, locale_alternates};
//!
//! #[get("/posts", seo(title = "Posts"))]
//! async fn index(locale: Locale, seo: SeoMeta) -> Markup {
//!     let seo = seo.hreflang_alternates(locale_alternates(
//!         "https://example.com",
//!         "/posts",
//!         "en",
//!         &["en".to_owned(), "es".to_owned()],
//!     ));
//!     html! { head { (seo.render()) } }
//! }
//! ```
//!
//! `sitemap.xml` also lists one entry per supported locale for each eligible
//! static route automatically when the flag is on.
//!
//! ## Sitemap
//!
//! Register a [`SitemapSource`] on the app builder for dynamic routes:
//!
//! ```rust,ignore
//! use autumn_web::seo::{SitemapEntry, SitemapSource};
//! use std::future::Future;
//! use std::pin::Pin;
//!
//! struct BlogSitemapSource;
//!
//! impl SitemapSource for BlogSitemapSource {
//!     fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>> {
//!         Box::pin(async {
//!             vec![SitemapEntry::new("https://example.com/posts/hello")]
//!         })
//!     }
//! }
//!
//! // In main():
//! // autumn_web::app()
//! //     .routes(routes![...])
//! //     .seo_source(BlogSitemapSource)
//! //     .run()
//! //     .await;
//! ```
//!
//! ## Robots.txt
//!
//! Configure in `autumn.toml`:
//!
//! ```toml
//! [seo]
//! base_url = "https://example.com"
//!
//! [seo.robots]
//! additional_rules = ["Disallow: /admin"]
//! ```
//!
//! The framework defaults: `dev`/`test` → disallow all; `prod` → allow all.

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Response;
use axum::routing::get;

#[cfg(feature = "maud")]
use maud::{Markup, html};

// ── SitemapEntry ─────────────────────────────────────────────────────────────

/// A single URL entry in a sitemap.
#[derive(Debug, Clone)]
pub struct SitemapEntry {
    /// The fully-qualified URL of the page.
    pub loc: String,
    /// Last modified date in `YYYY-MM-DD` format.
    pub lastmod: Option<String>,
    /// Suggested crawl frequency.
    pub changefreq: Option<SitemapChangefreq>,
    /// Relative priority (0.0–1.0). Clamped on construction.
    pub priority: Option<f32>,
}

impl SitemapEntry {
    /// Create a new entry with the given URL.
    pub fn new(loc: impl Into<String>) -> Self {
        Self {
            loc: loc.into(),
            lastmod: None,
            changefreq: None,
            priority: None,
        }
    }

    /// Set the last modified date (ISO 8601, e.g. `"2026-01-15"`).
    #[must_use]
    pub fn lastmod(mut self, lastmod: impl Into<String>) -> Self {
        self.lastmod = Some(lastmod.into());
        self
    }

    /// Set the suggested change frequency.
    #[must_use]
    pub const fn changefreq(mut self, changefreq: SitemapChangefreq) -> Self {
        self.changefreq = Some(changefreq);
        self
    }

    /// Set the priority (clamped to 0.0–1.0).
    #[must_use]
    pub const fn priority(mut self, priority: f32) -> Self {
        self.priority = Some(priority.clamp(0.0, 1.0));
        self
    }
}

// ── SitemapChangefreq ─────────────────────────────────────────────────────────

/// Suggested update frequency for a sitemap entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SitemapChangefreq {
    Always,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Never,
}

impl SitemapChangefreq {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
            Self::Never => "never",
        }
    }
}

// ── SitemapSource ─────────────────────────────────────────────────────────────

/// Trait for providing dynamic sitemap entries (e.g. blog posts from a database).
///
/// Implement this trait and register the source with
/// [`AppBuilder::seo_source`](crate::app::AppBuilder::seo_source).
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::seo::{SitemapEntry, SitemapSource};
/// use std::pin::Pin;
/// use std::future::Future;
///
/// struct PostSitemapSource;
///
/// impl SitemapSource for PostSitemapSource {
///     fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>> {
///         Box::pin(async {
///             vec![
///                 SitemapEntry::new("https://example.com/posts/hello-world")
///                     .lastmod("2026-01-15")
///                     .changefreq(autumn_web::seo::SitemapChangefreq::Weekly),
///             ]
///         })
///     }
/// }
/// ```
pub trait SitemapSource: Send + Sync {
    /// Return the sitemap entries for this source.
    fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>>;
}

// ── Internal AppState extension newtypes ──────────────────────────────────────

/// Registered sitemap sources stored in AppState extensions.
#[doc(hidden)]
pub struct RegisteredSitemapSources(pub Vec<Arc<dyn SitemapSource>>);

/// Registered SEO config stored in AppState extensions.
#[doc(hidden)]
pub struct RegisteredSeoConfig(pub crate::config::SeoConfig);

// ── robots_txt() ──────────────────────────────────────────────────────────────

/// Generate an environment-aware `robots.txt` string.
///
/// - `dev`/`test` profiles → `Disallow: /` (blocks all crawlers)
/// - `prod`/`production` profiles → `Allow: /` (permits all crawlers)
///
/// # Arguments
///
/// * `profile` — The active profile (`"dev"`, `"test"`, or `"prod"`).
/// * `sitemap_url` — Optional sitemap URL to inject as a `Sitemap:` directive.
/// * `additional_rules` — Extra lines to append (e.g. `"Disallow: /admin"`).
#[must_use]
pub fn robots_txt(profile: &str, sitemap_url: Option<&str>, additional_rules: &[String]) -> String {
    let mut txt = String::new();

    let is_prod = matches!(profile, "prod" | "production");
    if is_prod {
        txt.push_str("User-agent: *\nAllow: /\n");
    } else {
        txt.push_str("User-agent: *\nDisallow: /\n");
    }

    for rule in additional_rules {
        txt.push_str(rule);
        txt.push('\n');
    }

    if let Some(url) = sitemap_url {
        txt.push('\n');
        txt.push_str("Sitemap: ");
        txt.push_str(url);
        txt.push('\n');
    }

    txt
}

// ── sitemap_xml() ─────────────────────────────────────────────────────────────

/// Generate a valid `sitemap.xml` string.
///
/// Produces a `<urlset>` document with up to 50,000 entries (the Sitemap
/// protocol limit per file).  When more entries are supplied the first 50,000
/// are included and a `tracing::warn!` is emitted.  For sites that genuinely
/// exceed this limit, register a custom `/sitemap.xml` handler that builds and
/// serves a sitemap index alongside the numbered shard files.
///
/// # Arguments
///
/// * `entries` — The sitemap entries to include.
/// * `_base_url` — Reserved for future sitemap-index support; currently unused.
#[must_use]
pub fn sitemap_xml(entries: &[SitemapEntry], _base_url: Option<&str>) -> String {
    const CHUNK_SIZE: usize = 50_000;

    if entries.len() > CHUNK_SIZE {
        tracing::warn!(
            count = entries.len(),
            limit = CHUNK_SIZE,
            "sitemap: entry count exceeds the {CHUNK_SIZE}-URL per-file limit; \
             only the first {CHUNK_SIZE} entries will be served. \
             Register a custom /sitemap.xml handler to serve a sitemap index for larger sites.",
        );
        return sitemap_urlset_xml(&entries[..CHUNK_SIZE]);
    }
    sitemap_urlset_xml(entries)
}

/// Build a `<urlset>` sitemap.
#[must_use]
pub(crate) fn sitemap_urlset_xml(entries: &[SitemapEntry]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">",
    );
    for entry in entries {
        xml.push_str("\n  <url>");
        xml.push_str("\n    <loc>");
        xml.push_str(&xml_escape(&entry.loc));
        xml.push_str("</loc>");
        if let Some(lastmod) = &entry.lastmod {
            xml.push_str("\n    <lastmod>");
            xml.push_str(lastmod);
            xml.push_str("</lastmod>");
        }
        if let Some(freq) = entry.changefreq {
            xml.push_str("\n    <changefreq>");
            xml.push_str(freq.as_str());
            xml.push_str("</changefreq>");
        }
        if let Some(prio) = entry.priority {
            xml.push_str("\n    <priority>");
            write!(xml, "{prio:.1}").ok();
            xml.push_str("</priority>");
        }
        xml.push_str("\n  </url>");
    }
    xml.push_str("\n</urlset>");
    xml
}

/// Escape XML special characters in a single pass over the input.
fn xml_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

// ── SeoMeta builder ───────────────────────────────────────────────────────────

/// Builder for per-page SEO meta tags.
///
/// Generates `<title>`, `<meta>`, `<link rel="canonical">`, Open Graph,
/// and Twitter card tags from a fluent builder API.
///
/// # Example
///
/// ```rust,ignore
/// # #[cfg(feature = "maud")]
/// # {
/// use autumn_web::seo::SeoMeta;
///
/// let meta = SeoMeta::new()
///     .title("My Post")
///     .description("A great post")
///     .canonical("https://example.com/posts/my-post")
///     .og_image("https://example.com/og.jpg")
///     .twitter_card("summary_large_image");
///
/// // Embed in a Maud template:
/// // html! { head { (meta.render()) } }
/// # }
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeoMeta {
    title: Option<String>,
    description: Option<String>,
    canonical: Option<String>,
    og_title: Option<String>,
    og_description: Option<String>,
    og_image: Option<String>,
    og_type: Option<String>,
    og_url: Option<String>,
    twitter_card: Option<String>,
    twitter_title: Option<String>,
    twitter_description: Option<String>,
    twitter_image: Option<String>,
    robots_directive: Option<String>,
    hreflang_alternates: Vec<(String, String)>,
}

impl SeoMeta {
    /// Create a new, empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the page `<title>` (also used as the default OG/Twitter title).
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the `<meta name="description">` content.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the `<link rel="canonical">` URL.
    ///
    /// Also used as the `og:url` fallback.
    #[must_use]
    pub fn canonical(mut self, url: impl Into<String>) -> Self {
        self.canonical = Some(url.into());
        self
    }

    /// Set the `og:image` URL.
    #[must_use]
    pub fn og_image(mut self, url: impl Into<String>) -> Self {
        self.og_image = Some(url.into());
        self
    }

    /// Set the `og:type` value (default: omitted; common: `"website"`, `"article"`).
    #[must_use]
    pub fn og_type(mut self, og_type: impl Into<String>) -> Self {
        self.og_type = Some(og_type.into());
        self
    }

    /// Override the `og:title` (defaults to `title()`).
    #[must_use]
    pub fn og_title(mut self, title: impl Into<String>) -> Self {
        self.og_title = Some(title.into());
        self
    }

    /// Override the `og:description` (defaults to `description()`).
    #[must_use]
    pub fn og_description(mut self, desc: impl Into<String>) -> Self {
        self.og_description = Some(desc.into());
        self
    }

    /// Set the `og:url` (defaults to `canonical()` if not set).
    #[must_use]
    pub fn og_url(mut self, url: impl Into<String>) -> Self {
        self.og_url = Some(url.into());
        self
    }

    /// Set the `twitter:card` type (e.g. `"summary_large_image"`).
    ///
    /// When set, `twitter:title` and `twitter:description` are also emitted.
    #[must_use]
    pub fn twitter_card(mut self, card_type: impl Into<String>) -> Self {
        self.twitter_card = Some(card_type.into());
        self
    }

    /// Override the `twitter:title` (defaults to `title()`).
    #[must_use]
    pub fn twitter_title(mut self, title: impl Into<String>) -> Self {
        self.twitter_title = Some(title.into());
        self
    }

    /// Override the `twitter:description` (defaults to `description()`).
    #[must_use]
    pub fn twitter_description(mut self, desc: impl Into<String>) -> Self {
        self.twitter_description = Some(desc.into());
        self
    }

    /// Set the `twitter:image` URL.
    #[must_use]
    pub fn twitter_image(mut self, url: impl Into<String>) -> Self {
        self.twitter_image = Some(url.into());
        self
    }

    /// Set the `<meta name="robots">` directive (e.g. `"noindex"`, `"nofollow"`).
    #[must_use]
    pub fn robots(mut self, directive: impl Into<String>) -> Self {
        self.robots_directive = Some(directive.into());
        self
    }

    /// Add `<link rel="alternate" hreflang="…">` tags for the current page's
    /// localized variants (issue #1251).
    ///
    /// Pairs are `(hreflang value, absolute URL)` — use [`locale_alternates`]
    /// to build the list (including the `x-default` entry) from the
    /// current path, base URL, default locale, and supported locales.
    #[must_use]
    pub fn hreflang_alternates(mut self, alternates: Vec<(String, String)>) -> Self {
        self.hreflang_alternates = alternates;
        self
    }

    /// Render all configured meta tags as Maud [`Markup`].
    ///
    /// Emits only the tags that have been configured. Empty builders produce
    /// no output.
    #[cfg(feature = "maud")]
    #[must_use]
    pub fn render(&self) -> Markup {
        let og_title = self.og_title.as_ref().or(self.title.as_ref());
        let og_desc = self.og_description.as_ref().or(self.description.as_ref());
        let twitter_title = self.twitter_title.as_ref().or(self.title.as_ref());
        let twitter_desc = self
            .twitter_description
            .as_ref()
            .or(self.description.as_ref());
        let og_url = self.og_url.as_ref().or(self.canonical.as_ref());
        let has_twitter = self.twitter_card.is_some();

        html! {
            @if let Some(title) = &self.title {
                title { (title) }
            }
            @if let Some(desc) = &self.description {
                meta name="description" content=(desc);
            }
            @if let Some(dir) = &self.robots_directive {
                meta name="robots" content=(dir);
            }
            @if let Some(url) = &self.canonical {
                link rel="canonical" href=(url);
            }
            @if let Some(t) = og_title {
                meta property="og:title" content=(t);
            }
            @if let Some(d) = og_desc {
                meta property="og:description" content=(d);
            }
            @if let Some(img) = &self.og_image {
                meta property="og:image" content=(img);
            }
            @if let Some(ot) = &self.og_type {
                meta property="og:type" content=(ot);
            }
            @if let Some(url) = og_url {
                meta property="og:url" content=(url);
            }
            @if let Some(card) = &self.twitter_card {
                meta name="twitter:card" content=(card);
            }
            @if has_twitter {
                @if let Some(t) = twitter_title {
                    meta name="twitter:title" content=(t);
                }
                @if let Some(d) = twitter_desc {
                    meta name="twitter:description" content=(d);
                }
            }
            @if let Some(img) = &self.twitter_image {
                meta name="twitter:image" content=(img);
            }
            @for (lang, href) in &self.hreflang_alternates {
                link rel="alternate" hreflang=(lang) href=(href);
            }
        }
    }
}

/// Build `(hreflang, absolute URL)` pairs for [`SeoMeta::hreflang_alternates`]:
/// one entry per supported locale plus an `x-default` entry pointing at the
/// default locale (issue #1251).
///
/// `path` is the current page's locale-stripped path (e.g. `"/posts"`, as
/// returned by axum's `Uri` extractor inside a locale-prefixed nest — nesting
/// strips the matched prefix for downstream extraction). `base_url` is
/// trimmed of any trailing slash.
///
/// The root path is a special case: axum's `nest("/{locale}", router)` makes
/// the *bare* `/{locale}` (no trailing slash) match the inner router's own
/// `"/"` route — `/{locale}/` 404s — so `path = "/"` produces
/// `{base_url}/{locale}`, not `{base_url}/{locale}/`.
///
/// # Example
///
/// ```
/// use autumn_web::seo::locale_alternates;
///
/// let alternates = locale_alternates(
///     "https://example.com",
///     "/posts",
///     "en",
///     &["en".to_owned(), "es".to_owned()],
/// );
/// assert_eq!(
///     alternates,
///     vec![
///         ("en".to_owned(), "https://example.com/en/posts".to_owned()),
///         ("es".to_owned(), "https://example.com/es/posts".to_owned()),
///         ("x-default".to_owned(), "https://example.com/en/posts".to_owned()),
///     ]
/// );
///
/// let root_alternates =
///     locale_alternates("https://example.com", "/", "en", &["en".to_owned()]);
/// assert_eq!(
///     root_alternates,
///     vec![
///         ("en".to_owned(), "https://example.com/en".to_owned()),
///         ("x-default".to_owned(), "https://example.com/en".to_owned()),
///     ]
/// );
/// ```
#[must_use]
pub fn locale_alternates(
    base_url: &str,
    path: &str,
    default_locale: &str,
    supported_locales: &[String],
) -> Vec<(String, String)> {
    let base_url = base_url.trim_end_matches('/');
    let join = |locale: &str| -> String {
        if path == "/" {
            format!("{base_url}/{locale}")
        } else {
            format!("{base_url}/{locale}{path}")
        }
    };
    let mut alternates: Vec<(String, String)> = supported_locales
        .iter()
        .map(|locale| (locale.clone(), join(locale)))
        .collect();
    alternates.push(("x-default".to_owned(), join(default_locale)));
    alternates
}

// ── SeoRouteDefaults (route-level defaults) ───────────────────────────────────

/// Route-level SEO defaults declared on a route attribute.
///
/// Emitted by the route macros for
/// `#[get("/about", seo(title = "About", description = "…"))]` (and the same
/// `seo(...)` argument on [`post`](crate::post), [`static_get`](crate::static_get),
/// and friends), stored on [`Route::seo`](crate::Route::seo), and attached to
/// each matching request as an extension so the [`SeoMeta`] extractor can hand
/// the handler a pre-populated builder.
///
/// Every field borrows a `&'static str` straight out of the attribute literal,
/// which keeps the type [`Copy`] and allocation-free until a handler asks for
/// it. Applications rarely name this type: declare the values on the attribute
/// and take a [`SeoMeta`] parameter in the handler.
///
/// # Example
///
/// ```rust,no_run
/// # #[cfg(feature = "maud")]
/// # {
/// use autumn_web::prelude::*;
///
/// #[get("/about", seo(title = "About • My Blog", description = "Learn about us"))]
/// async fn about(seo: SeoMeta) -> Markup {
///     // `seo` already carries the attribute's title and description.
///     html! { head { (seo.render()) } }
/// }
/// # }
/// ```
///
/// # Constructing one by hand
///
/// The type is `#[non_exhaustive]`, because the set of useful SEO keys is
/// open-ended and new ones must stay additive. Start from
/// [`EMPTY`](Self::EMPTY) and chain the `with_*` setters, all of which are
/// `const`:
///
/// ```rust
/// use autumn_web::seo::SeoRouteDefaults;
///
/// const DEFAULTS: SeoRouteDefaults = SeoRouteDefaults::EMPTY
///     .with_title("About • My Blog")
///     .with_og_type("website");
///
/// assert_eq!(DEFAULTS.title, Some("About • My Blog"));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SeoRouteDefaults {
    /// Default page `<title>`.
    pub title: Option<&'static str>,
    /// Default `<meta name="description">`.
    pub description: Option<&'static str>,
    /// Default `<link rel="canonical">`.
    pub canonical: Option<&'static str>,
    /// Default `og:title`.
    pub og_title: Option<&'static str>,
    /// Default `og:description`.
    pub og_description: Option<&'static str>,
    /// Default `og:image`.
    pub og_image: Option<&'static str>,
    /// Default `og:type` (e.g. `"article"`).
    pub og_type: Option<&'static str>,
    /// Default `og:url`.
    pub og_url: Option<&'static str>,
    /// Default `twitter:card` type.
    pub twitter_card: Option<&'static str>,
    /// Default `twitter:title`.
    pub twitter_title: Option<&'static str>,
    /// Default `twitter:description`.
    pub twitter_description: Option<&'static str>,
    /// Default `twitter:image`.
    pub twitter_image: Option<&'static str>,
    /// Default `<meta name="robots">` directive.
    pub robots: Option<&'static str>,
}

impl SeoRouteDefaults {
    /// Defaults with every key unset — what a route without a `seo(...)`
    /// argument records.
    ///
    /// Route macros use this as the base of a `with_*` setter chain, so only
    /// the keys named on the attribute appear in generated code — and no
    /// struct literal for this type is ever emitted into a user's crate, which
    /// is what lets it stay `#[non_exhaustive]`.
    pub const EMPTY: Self = Self {
        title: None,
        description: None,
        canonical: None,
        og_title: None,
        og_description: None,
        og_image: None,
        og_type: None,
        og_url: None,
        twitter_card: None,
        twitter_title: None,
        twitter_description: None,
        twitter_image: None,
        robots: None,
    };

    /// Set the default page `<title>`.
    #[must_use]
    pub const fn with_title(mut self, value: &'static str) -> Self {
        self.title = Some(value);
        self
    }

    /// Set the default `<meta name="description">`.
    #[must_use]
    pub const fn with_description(mut self, value: &'static str) -> Self {
        self.description = Some(value);
        self
    }

    /// Set the default `<link rel="canonical">`.
    #[must_use]
    pub const fn with_canonical(mut self, value: &'static str) -> Self {
        self.canonical = Some(value);
        self
    }

    /// Set the default `og:title`.
    #[must_use]
    pub const fn with_og_title(mut self, value: &'static str) -> Self {
        self.og_title = Some(value);
        self
    }

    /// Set the default `og:description`.
    #[must_use]
    pub const fn with_og_description(mut self, value: &'static str) -> Self {
        self.og_description = Some(value);
        self
    }

    /// Set the default `og:image`.
    #[must_use]
    pub const fn with_og_image(mut self, value: &'static str) -> Self {
        self.og_image = Some(value);
        self
    }

    /// Set the default `og:type`.
    #[must_use]
    pub const fn with_og_type(mut self, value: &'static str) -> Self {
        self.og_type = Some(value);
        self
    }

    /// Set the default `og:url`.
    #[must_use]
    pub const fn with_og_url(mut self, value: &'static str) -> Self {
        self.og_url = Some(value);
        self
    }

    /// Set the default `twitter:card` type.
    #[must_use]
    pub const fn with_twitter_card(mut self, value: &'static str) -> Self {
        self.twitter_card = Some(value);
        self
    }

    /// Set the default `twitter:title`.
    #[must_use]
    pub const fn with_twitter_title(mut self, value: &'static str) -> Self {
        self.twitter_title = Some(value);
        self
    }

    /// Set the default `twitter:description`.
    #[must_use]
    pub const fn with_twitter_description(mut self, value: &'static str) -> Self {
        self.twitter_description = Some(value);
        self
    }

    /// Set the default `twitter:image`.
    #[must_use]
    pub const fn with_twitter_image(mut self, value: &'static str) -> Self {
        self.twitter_image = Some(value);
        self
    }

    /// Set the default `<meta name="robots">` directive.
    #[must_use]
    pub const fn with_robots(mut self, value: &'static str) -> Self {
        self.robots = Some(value);
        self
    }

    /// Whether no key was declared.
    ///
    /// The router skips installing the request extension for empty defaults, so
    /// routes that never mention `seo(...)` pay nothing at request time.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.canonical.is_none()
            && self.og_title.is_none()
            && self.og_description.is_none()
            && self.og_image.is_none()
            && self.og_type.is_none()
            && self.og_url.is_none()
            && self.twitter_card.is_none()
            && self.twitter_title.is_none()
            && self.twitter_description.is_none()
            && self.twitter_image.is_none()
            && self.robots.is_none()
    }

    /// Expand these defaults into an owned [`SeoMeta`] builder that a handler
    /// can refine further.
    #[must_use]
    pub fn to_meta(&self) -> SeoMeta {
        let mut meta = SeoMeta::new();
        if let Some(v) = self.title {
            meta = meta.title(v);
        }
        if let Some(v) = self.description {
            meta = meta.description(v);
        }
        if let Some(v) = self.canonical {
            meta = meta.canonical(v);
        }
        if let Some(v) = self.og_title {
            meta = meta.og_title(v);
        }
        if let Some(v) = self.og_description {
            meta = meta.og_description(v);
        }
        if let Some(v) = self.og_image {
            meta = meta.og_image(v);
        }
        if let Some(v) = self.og_type {
            meta = meta.og_type(v);
        }
        if let Some(v) = self.og_url {
            meta = meta.og_url(v);
        }
        if let Some(v) = self.twitter_card {
            meta = meta.twitter_card(v);
        }
        if let Some(v) = self.twitter_title {
            meta = meta.twitter_title(v);
        }
        if let Some(v) = self.twitter_description {
            meta = meta.twitter_description(v);
        }
        if let Some(v) = self.twitter_image {
            meta = meta.twitter_image(v);
        }
        if let Some(v) = self.robots {
            meta = meta.robots(v);
        }
        meta
    }
}

impl From<SeoRouteDefaults> for SeoMeta {
    fn from(defaults: SeoRouteDefaults) -> Self {
        defaults.to_meta()
    }
}

/// Extract the route's declared SEO defaults as a refinable [`SeoMeta`].
///
/// This extractor never fails. A route with no `seo(...)` argument yields an
/// empty builder, exactly as if the handler had called [`SeoMeta::new`], so
/// adding the parameter is always safe.
///
/// Note that the attribute supplies *values*, not markup: the handler (or its
/// layout) still decides where to emit them, normally via
/// [`SeoMeta::render`].
impl<S> axum::extract::FromRequestParts<S> for SeoMeta
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<SeoRouteDefaults>()
            .map_or_else(Self::new, SeoRouteDefaults::to_meta))
    }
}

// ── HTTP route builders (used by AppBuilder::run) ─────────────────────────────

/// Build an axum [`Router`] serving `/robots.txt` and `/sitemap.xml`.
///
/// The router is generic over the application state `S`, making it compatible
/// with both bare test routers and full `AppState`-powered production routers.
///
/// Used by [`AppBuilder::seo_source`](crate::app::AppBuilder::seo_source) when
/// assembling the server. The `entries` parameter provides the initial set of
/// URLs to include in the sitemap; dynamic sources registered via `seo_source()`
/// can supply additional entries at request time.
pub fn build_seo_router<S>(
    profile: &str,
    base_url: Option<&str>,
    additional_rules: &[String],
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    build_seo_router_with_entries(profile, base_url, additional_rules, &[])
}

/// Build a SEO router with a pre-populated list of sitemap entries.
///
/// # Panics
///
/// This function will not panic in practice. The `Response::builder()` calls
/// inside the route handlers use hard-coded, well-formed `Content-Type` header
/// values that can never produce an error.
pub fn build_seo_router_with_entries<S>(
    profile: &str,
    base_url: Option<&str>,
    additional_rules: &[String],
    entries: &[SitemapEntry],
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let base_url = base_url.map(|u| u.trim_end_matches('/'));
    let sitemap_url = base_url.map(|b| format!("{b}/sitemap.xml"));
    let robots_body = robots_txt(profile, sitemap_url.as_deref(), additional_rules);
    let sitemap_body = sitemap_xml(entries, base_url);
    build_seo_router_from_bodies(robots_body, sitemap_body)
}

/// Build a SEO router from pre-rendered `robots.txt` and `sitemap.xml` bodies.
///
/// Use this when you need full control over how the bodies are generated
/// (e.g. to honour `[seo.robots] sitemap_url` or `allow_all` overrides).
///
/// # Panics
///
/// In practice this function cannot panic. The hard-coded `Content-Type`
/// header values are always valid.
pub fn build_seo_router_from_bodies<S>(robots_body: String, sitemap_body: String) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::<S>::new()
        .route(
            "/robots.txt",
            get(move || {
                let body = robots_body.clone();
                async move {
                    Response::builder()
                        .header("Content-Type", "text/plain; charset=utf-8")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        )
        .route(
            "/sitemap.xml",
            get(move || {
                let body = sitemap_body.clone();
                async move {
                    Response::builder()
                        .header("Content-Type", "application/xml; charset=utf-8")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        )
}

// ── App-level SEO helpers (shared by run() and run_build_mode()) ──────────────

/// Return `true` when the `[seo]` config section contains any non-default value.
pub(crate) const fn has_seo_config(seo_cfg: &crate::config::SeoConfig) -> bool {
    seo_cfg.base_url.is_some()
        || !seo_cfg.robots.additional_rules.is_empty()
        || seo_cfg.robots.allow_all.is_some()
        || seo_cfg.robots.sitemap_url.is_some()
}

/// Resolve the effective robots.txt profile from `raw_profile` and the
/// optional `allow_all` override in `[seo.robots]`.
pub(crate) const fn effective_seo_profile(raw_profile: &str, allow_all: Option<bool>) -> &str {
    match allow_all {
        Some(true) => "prod",
        Some(false) => "dev",
        None => raw_profile,
    }
}

/// Whether a route's declared `robots` directive asks crawlers not to index the
/// page, in which case it must not be advertised in `sitemap.xml`.
///
/// Matches `noindex` as a comma-separated directive anywhere in the value, so
/// `"noindex"`, `"noindex, nofollow"`, and `"nofollow, noindex"` all count
/// while an unrelated value like `"noarchive"` does not.
pub(crate) fn robots_directive_is_noindex(directive: &str) -> bool {
    directive
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case("noindex"))
}

/// Whether these route-level defaults exclude the page from the sitemap.
///
/// This governs only the paths Autumn *derives on its own* — the concrete
/// `#[static_get]` route paths it would otherwise add automatically. Entries
/// coming from a [`SitemapSource`] the application registered via
/// [`AppBuilder::seo_source`](crate::app::AppBuilder::seo_source) are passed
/// through untouched; see [`assemble_seo_bodies`] for why.
#[must_use]
pub(crate) fn defaults_exclude_from_sitemap(defaults: SeoRouteDefaults) -> bool {
    defaults.robots.is_some_and(robots_directive_is_noindex)
}

/// Collect sitemap entries from dynamic sources and static path hints, then
/// build the `robots.txt` and `sitemap.xml` bodies.
///
/// # Which entries route-level `robots = "noindex"` filters
///
/// `static_paths` arrives already filtered by
/// [`defaults_exclude_from_sitemap`]: a `#[static_get]` route declaring
/// `noindex` is dropped before it gets here, so Autumn never advertises a URL
/// it derived itself while also asking crawlers not to index it.
///
/// `sources` are **not** filtered, deliberately. A [`SitemapSource`] is an
/// explicit, application-authored list of URLs, and its
/// [`SitemapEntry`] values carry only a `loc` string — nothing ties an entry
/// back to the route that serves it. Silently dropping entries an application
/// asked for would be its own surprise ("I registered this source, why is my
/// URL missing?"), and matching concrete URLs back to route templates would
/// mean guessing which of two contradictory instructions the author meant.
///
/// The practical consequence: a parameterized route such as
/// `#[static_get("/posts/{slug}", params = …, seo(robots = "noindex"))]`
/// contributes nothing to the sitemap on its own — its template is skipped for
/// containing `{`, and `params_fn` output is used for pre-rendering only, never
/// for the sitemap. Its concrete URLs appear only if the application also
/// registers a `SitemapSource` emitting them, which is a contradiction in the
/// application's own configuration rather than something Autumn introduces.
/// Omit those URLs from the source to resolve it.
///
/// Called by both `AppBuilder::run` (server mode) and
/// `AppBuilder::run_build_mode` (static build mode).
///
/// `locale` carries the locale-prefix routing config (issue #1251) when
/// `[i18n] locale_prefix_enabled = true`: each eligible static path expands to
/// one sitemap entry per supported locale (`{base_url}/{locale}{path}`)
/// instead of a single unprefixed entry, since only the prefixed URLs are
/// actually reachable. Paths matching `locale.exclude_prefixes` are listed
/// unprefixed, same as when `locale` is `None`.
pub(crate) async fn assemble_seo_bodies(
    profile: &str,
    base_url: Option<&str>,
    sitemap_url_override: Option<&str>,
    additional_rules: &[String],
    sources: &[Arc<dyn SitemapSource>],
    static_paths: &[&str],
    locale: Option<SitemapLocaleConfig<'_>>,
) -> (String, String) {
    let base_url = base_url.map(|u| u.trim_end_matches('/'));

    let mut sitemap_entries = Vec::new();
    for source in sources {
        let mut entries = source.entries().await;
        sitemap_entries.append(&mut entries);
    }

    if let Some(bu) = base_url {
        for path in static_paths {
            if path.contains('{') {
                continue;
            }
            match &locale {
                Some(loc)
                    if !loc.supported_locales.is_empty()
                        && !matches_locale_exclude_prefix(path, loc.exclude_prefixes) =>
                {
                    // Root-path special case: axum's `nest("/{locale}",
                    // router)` matches bare `/{locale}` (no trailing slash)
                    // against the inner router's own "/" route — `/{locale}/`
                    // 404s — so `path == "/"` must not get a doubled slash.
                    for locale_code in loc.supported_locales {
                        let entry = if *path == "/" {
                            format!("{bu}/{locale_code}")
                        } else {
                            format!("{bu}/{locale_code}{path}")
                        };
                        sitemap_entries.push(SitemapEntry::new(entry));
                    }
                }
                _ => sitemap_entries.push(SitemapEntry::new(format!("{bu}{path}"))),
            }
        }
    }

    let derived_sitemap_url = base_url.map(|b| format!("{b}/sitemap.xml"));
    let sitemap_url = sitemap_url_override.or(derived_sitemap_url.as_deref());
    let robots_body = robots_txt(profile, sitemap_url, additional_rules);
    let sitemap_body = sitemap_xml(&sitemap_entries, base_url);
    (robots_body, sitemap_body)
}

/// Locale-prefix routing config passed to [`assemble_seo_bodies`] so the
/// sitemap lists each localized URL instead of a single unprefixed one
/// (issue #1251's sitemap acceptance criterion).
pub(crate) struct SitemapLocaleConfig<'a> {
    pub supported_locales: &'a [String],
    pub exclude_prefixes: &'a [String],
}

/// `true` when `path` equals one of `prefixes` or starts with `{prefix}/`.
/// A trailing `/*` (or `/`) on a configured prefix is stripped before
/// comparing, so `"/api"` and `"/api/*"` are equivalent — except a bare `"/"`
/// (e.g. a `#[static_get("/")]` route), which is kept as-is and matched
/// exactly: stripping its trailing slash would normalize it to an empty
/// prefix, which the empty-prefix guard below then silently rejects, so
/// `"/"` would never actually get excluded (Codex review).
///
/// Mirrors `router::matches_locale_exclude_prefix` — kept as a separate copy
/// so this module doesn't need a hard dependency on the `i18n`-feature-gated
/// router internals for what is a few lines of string matching.
fn matches_locale_exclude_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|raw| {
        let prefix = raw.strip_suffix("/*").unwrap_or(raw.as_str());
        let prefix = if prefix == "/" {
            prefix
        } else {
            prefix.strip_suffix('/').unwrap_or(prefix)
        };
        !prefix.is_empty() && (path == prefix || path.starts_with(&format!("{prefix}/")))
    })
}

// ── Static build helpers ──────────────────────────────────────────────────────

/// Write `robots.txt` and `sitemap.xml` to `dist_dir` as part of `autumn build`.
///
/// Called by `AppBuilder::run_build_mode` after static routes are rendered.
///
/// # Arguments
///
/// * `dist_dir` — The output directory (e.g. `dist/`).
/// * `profile` — The active profile.
/// * `base_url` — The site base URL (auto-injects the `Sitemap:` directive).
/// * `additional_rules` — Extra robots.txt rules.
/// * `entries` — Sitemap entries to include (from registered sources + static metas).
///
/// # Errors
///
/// Returns `std::io::Error` if writing fails.
pub async fn write_seo_files(
    dist_dir: &Path,
    profile: &str,
    base_url: Option<&str>,
    sitemap_url_override: Option<&str>,
    additional_rules: &[String],
    entries: &[SitemapEntry],
) -> Result<(), std::io::Error> {
    let base_url = base_url.map(|u| u.trim_end_matches('/'));
    let derived_sitemap_url = base_url.map(|b| format!("{b}/sitemap.xml"));
    let sitemap_url = sitemap_url_override.or(derived_sitemap_url.as_deref());
    let robots = robots_txt(profile, sitemap_url, additional_rules);
    let sitemap = sitemap_xml(entries, base_url);

    tokio::fs::write(dist_dir.join("robots.txt"), robots).await?;
    tokio::fs::write(dist_dir.join("sitemap.xml"), sitemap).await?;

    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sitemap_entry_builder() {
        let e = SitemapEntry::new("https://example.com/")
            .lastmod("2026-01-01")
            .changefreq(SitemapChangefreq::Weekly)
            .priority(0.9);
        assert_eq!(e.loc, "https://example.com/");
        assert_eq!(e.lastmod.as_deref(), Some("2026-01-01"));
        assert_eq!(e.changefreq, Some(SitemapChangefreq::Weekly));
        assert!((e.priority.unwrap() - 0.9).abs() < 0.001);
    }

    #[test]
    fn sitemap_entry_priority_clamped() {
        let hi = SitemapEntry::new("https://example.com/").priority(1.5);
        let lo = SitemapEntry::new("https://example.com/").priority(-0.5);
        assert!((hi.priority.unwrap() - 1.0).abs() < 0.001);
        assert!((lo.priority.unwrap() - 0.0).abs() < 0.001);
    }

    #[test]
    fn xml_escape_replaces_special_chars() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn robots_txt_staging_profile_disallows() {
        let txt = robots_txt("staging", None, &[]);
        assert!(txt.contains("Disallow: /"));
        assert!(!txt.contains("Allow: /"));
    }

    #[test]
    fn has_seo_config_false_when_empty() {
        let cfg = crate::config::SeoConfig::default();
        assert!(!has_seo_config(&cfg));
    }

    #[test]
    fn has_seo_config_true_when_base_url_set() {
        let cfg = crate::config::SeoConfig {
            base_url: Some("https://example.com".to_string()),
            ..Default::default()
        };
        assert!(has_seo_config(&cfg));
    }

    #[test]
    fn has_seo_config_true_when_allow_all_set() {
        let cfg = crate::config::SeoConfig {
            robots: crate::config::RobotsConfig {
                allow_all: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(has_seo_config(&cfg));
    }

    #[test]
    fn has_seo_config_true_when_sitemap_url_set() {
        let cfg = crate::config::SeoConfig {
            robots: crate::config::RobotsConfig {
                sitemap_url: Some("https://example.com/sitemap.xml".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(has_seo_config(&cfg));
    }

    #[test]
    fn has_seo_config_true_when_additional_rules_set() {
        let cfg = crate::config::SeoConfig {
            robots: crate::config::RobotsConfig {
                additional_rules: vec!["Disallow: /admin".to_string()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(has_seo_config(&cfg));
    }

    #[test]
    fn effective_seo_profile_respects_allow_all_true() {
        assert_eq!(effective_seo_profile("dev", Some(true)), "prod");
    }

    #[test]
    fn effective_seo_profile_respects_allow_all_false() {
        assert_eq!(effective_seo_profile("prod", Some(false)), "dev");
    }

    #[test]
    fn effective_seo_profile_falls_back_to_raw_when_none() {
        assert_eq!(effective_seo_profile("staging", None), "staging");
    }

    struct SimpleSitemapSource {
        entries: Vec<SitemapEntry>,
    }

    impl SitemapSource for SimpleSitemapSource {
        fn entries(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<SitemapEntry>> + Send + '_>>
        {
            let entries = self.entries.clone();
            Box::pin(async move { entries })
        }
    }

    #[tokio::test]
    async fn assemble_seo_bodies_empty() {
        let (robots, sitemap) = assemble_seo_bodies("prod", None, None, &[], &[], &[], None).await;
        assert!(robots.contains("Allow: /"));
        assert!(sitemap.contains("<urlset"));
    }

    #[tokio::test]
    async fn assemble_seo_bodies_collects_source_entries() {
        let source = Arc::new(SimpleSitemapSource {
            entries: vec![SitemapEntry::new("https://example.com/post/1")],
        }) as Arc<dyn SitemapSource>;
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[source],
            &[],
            None,
        )
        .await;
        assert!(
            sitemap.contains("https://example.com/post/1"),
            "should include source entry; got:\n{sitemap}"
        );
    }

    #[tokio::test]
    async fn assemble_seo_bodies_includes_static_paths() {
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/about", "/contact"],
            None,
        )
        .await;
        assert!(sitemap.contains("https://example.com/about"));
        assert!(sitemap.contains("https://example.com/contact"));
    }

    #[tokio::test]
    async fn assemble_seo_bodies_skips_dynamic_paths() {
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/posts/{slug}"],
            None,
        )
        .await;
        assert!(
            !sitemap.contains("/posts/"),
            "should skip paths with params; got:\n{sitemap}"
        );
    }

    #[tokio::test]
    async fn assemble_seo_bodies_uses_sitemap_url_override() {
        let (robots, _) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            Some("https://cdn.example.com/sitemap.xml"),
            &[],
            &[],
            &[],
            None,
        )
        .await;
        assert!(
            robots.contains("Sitemap: https://cdn.example.com/sitemap.xml"),
            "should use override url; got:\n{robots}"
        );
    }

    // ── SeoRouteDefaults / SeoMeta extractor (#1182) ─────────────────────────

    #[test]
    fn robots_directive_noindex_detection() {
        assert!(robots_directive_is_noindex("noindex"));
        assert!(robots_directive_is_noindex("noindex, nofollow"));
        assert!(robots_directive_is_noindex("nofollow, noindex"));
        assert!(robots_directive_is_noindex("NoIndex"));
        assert!(robots_directive_is_noindex(" noindex "));
        // Substring matches must not count.
        assert!(!robots_directive_is_noindex("noarchive"));
        assert!(!robots_directive_is_noindex("index, follow"));
        assert!(!robots_directive_is_noindex("max-snippet:-1"));
    }

    #[test]
    fn defaults_exclude_from_sitemap_only_for_noindex() {
        assert!(!defaults_exclude_from_sitemap(SeoRouteDefaults::EMPTY));
        assert!(!defaults_exclude_from_sitemap(
            SeoRouteDefaults::EMPTY.with_title("About")
        ));
        assert!(!defaults_exclude_from_sitemap(
            SeoRouteDefaults::EMPTY.with_robots("nofollow")
        ));
        assert!(defaults_exclude_from_sitemap(
            SeoRouteDefaults::EMPTY.with_robots("noindex, nofollow")
        ));
    }

    #[tokio::test]
    async fn assemble_seo_bodies_does_not_filter_registered_source_entries() {
        // A registered `SitemapSource` is the application's explicit URL list.
        // Route-level `robots = "noindex"` filters the paths Autumn derives on
        // its own (done before this call), never a source's entries — dropping
        // those silently would make `seo_source` lossy. Pinned so the boundary
        // stays a decision rather than an accident.
        let source = Arc::new(SimpleSitemapSource {
            entries: vec![SitemapEntry::new("https://example.com/posts/hello")],
        }) as Arc<dyn SitemapSource>;

        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[source],
            // The caller already dropped any noindex static route, and a
            // parameterized template would be skipped for containing `{`.
            &[],
            None,
        )
        .await;

        assert!(
            sitemap.contains("https://example.com/posts/hello"),
            "explicitly registered source entries must survive; got:\n{sitemap}"
        );
    }

    #[tokio::test]
    async fn assemble_seo_bodies_skips_parameterized_templates() {
        // The other half of the same story: a parameterized static route
        // contributes nothing on its own, so a noindex one cannot leak in
        // through the template path either.
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/posts/{slug}"],
            None,
        )
        .await;

        assert!(
            !sitemap.contains("/posts/"),
            "parameterized templates must never be advertised; got:\n{sitemap}"
        );
    }

    #[test]
    fn route_defaults_setters_are_const_and_chainable() {
        const DEFAULTS: SeoRouteDefaults = SeoRouteDefaults::EMPTY
            .with_title("About")
            .with_og_type("website");
        assert_eq!(DEFAULTS.title, Some("About"));
        assert_eq!(DEFAULTS.og_type, Some("website"));
        assert_eq!(DEFAULTS.description, None);
    }

    #[test]
    fn route_defaults_empty_is_default() {
        assert_eq!(SeoRouteDefaults::default(), SeoRouteDefaults::EMPTY);
        assert!(SeoRouteDefaults::EMPTY.is_empty());
    }

    #[test]
    fn route_defaults_is_empty_false_when_any_key_set() {
        let defaults = SeoRouteDefaults {
            og_type: Some("article"),
            ..SeoRouteDefaults::EMPTY
        };
        assert!(!defaults.is_empty());
    }

    #[test]
    fn route_defaults_to_meta_populates_every_key() {
        let defaults = SeoRouteDefaults {
            title: Some("T"),
            description: Some("D"),
            canonical: Some("C"),
            og_title: Some("OT"),
            og_description: Some("OD"),
            og_image: Some("OI"),
            og_type: Some("OTY"),
            og_url: Some("OU"),
            twitter_card: Some("TC"),
            twitter_title: Some("TT"),
            twitter_description: Some("TD"),
            twitter_image: Some("TI"),
            robots: Some("noindex"),
        };
        let expected = SeoMeta::new()
            .title("T")
            .description("D")
            .canonical("C")
            .og_title("OT")
            .og_description("OD")
            .og_image("OI")
            .og_type("OTY")
            .og_url("OU")
            .twitter_card("TC")
            .twitter_title("TT")
            .twitter_description("TD")
            .twitter_image("TI")
            .robots("noindex");
        assert_eq!(defaults.to_meta(), expected);
    }

    #[test]
    fn route_defaults_empty_to_meta_is_empty_builder() {
        assert_eq!(SeoRouteDefaults::EMPTY.to_meta(), SeoMeta::new());
    }

    #[tokio::test]
    async fn extractor_resolves_route_defaults_from_extension() {
        use axum::extract::FromRequestParts;

        let mut parts = axum::http::Request::builder()
            .uri("/about")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        parts.extensions.insert(SeoRouteDefaults {
            title: Some("About"),
            ..SeoRouteDefaults::EMPTY
        });

        let meta = SeoMeta::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(meta, SeoMeta::new().title("About"));
    }

    #[tokio::test]
    async fn extractor_yields_empty_builder_without_extension() {
        use axum::extract::FromRequestParts;

        let mut parts = axum::http::Request::builder()
            .uri("/bare")
            .body(())
            .unwrap()
            .into_parts()
            .0;

        let meta = SeoMeta::from_request_parts(&mut parts, &()).await.unwrap();
        assert_eq!(meta, SeoMeta::new());
    }

    #[tokio::test]
    async fn extractor_result_is_refinable_by_the_handler() {
        use axum::extract::FromRequestParts;

        let mut parts = axum::http::Request::builder()
            .uri("/posts/hello")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        parts.extensions.insert(SeoRouteDefaults {
            og_type: Some("article"),
            title: Some("Attribute Title"),
            ..SeoRouteDefaults::EMPTY
        });

        let meta = SeoMeta::from_request_parts(&mut parts, &())
            .await
            .unwrap()
            .title("Handler Title");
        assert_eq!(
            meta,
            SeoMeta::new().og_type("article").title("Handler Title")
        );
    }

    #[tokio::test]
    async fn assemble_seo_bodies_trims_trailing_slash() {
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com/"),
            None,
            &[],
            &[],
            &["/about"],
            None,
        )
        .await;
        assert!(
            sitemap.contains("https://example.com/about"),
            "base_url trailing slash should be trimmed; got:\n{sitemap}"
        );
    }

    #[tokio::test]
    async fn assemble_seo_bodies_lists_each_localized_url_when_locale_prefix_enabled() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/about"],
            Some(SitemapLocaleConfig {
                supported_locales: &supported,
                exclude_prefixes: &[],
            }),
        )
        .await;
        assert!(
            sitemap.contains("https://example.com/en/about"),
            "should list the en-prefixed URL; got:\n{sitemap}"
        );
        assert!(
            sitemap.contains("https://example.com/es/about"),
            "should list the es-prefixed URL; got:\n{sitemap}"
        );
        assert!(
            !sitemap.contains(">https://example.com/about<"),
            "unprefixed URL should not also be listed; got:\n{sitemap}"
        );
    }

    #[tokio::test]
    async fn assemble_seo_bodies_root_static_path_has_no_trailing_slash_per_locale() {
        // axum's `nest("/en", router)` matches bare "/en" against the inner
        // router's "/" route — "/en/" 404s — so a static "/" route must list
        // "https://example.com/en", not "https://example.com/en/".
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/"],
            Some(SitemapLocaleConfig {
                supported_locales: &supported,
                exclude_prefixes: &[],
            }),
        )
        .await;
        assert!(
            sitemap.contains(">https://example.com/en<"),
            "got:\n{sitemap}"
        );
        assert!(
            !sitemap.contains(">https://example.com/en/<"),
            "got:\n{sitemap}"
        );
    }

    #[tokio::test]
    async fn assemble_seo_bodies_leaves_excluded_prefixes_unlocalized_in_sitemap() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let exclude = vec!["/api".to_owned()];
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/about", "/api/status"],
            Some(SitemapLocaleConfig {
                supported_locales: &supported,
                exclude_prefixes: &exclude,
            }),
        )
        .await;
        assert!(sitemap.contains("https://example.com/en/about"));
        assert!(
            sitemap.contains("https://example.com/api/status"),
            "excluded prefix should list its unprefixed URL; got:\n{sitemap}"
        );
        assert!(!sitemap.contains("https://example.com/en/api/status"));
    }

    #[tokio::test]
    async fn assemble_seo_bodies_root_exclude_prefix_excludes_exactly_the_root() {
        // Codex review (P1): a bare "/" exclude entry (as
        // exclude_static_routes_from_locale_prefix adds for a
        // #[static_get("/")] route) must exclude exactly the root path, not
        // be normalized away to an empty, always-non-matching prefix.
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let exclude = vec!["/".to_owned()];
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/", "/about"],
            Some(SitemapLocaleConfig {
                supported_locales: &supported,
                exclude_prefixes: &exclude,
            }),
        )
        .await;
        assert!(
            sitemap.contains(">https://example.com/<"),
            "excluded root should list its unprefixed URL; got:\n{sitemap}"
        );
        assert!(!sitemap.contains(">https://example.com/en<"));
        // "/" in the exclude list must not exclude unrelated paths.
        assert!(sitemap.contains("https://example.com/en/about"));
    }

    #[tokio::test]
    async fn assemble_seo_bodies_does_not_exclude_path_sharing_a_string_prefix() {
        // "/apikeys" merely starts with the same characters as the excluded
        // "/api" prefix — it is not a sub-path of it and must still be
        // localized like any other eligible static path.
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let exclude = vec!["/api".to_owned()];
        let (_, sitemap) = assemble_seo_bodies(
            "prod",
            Some("https://example.com"),
            None,
            &[],
            &[],
            &["/apikeys"],
            Some(SitemapLocaleConfig {
                supported_locales: &supported,
                exclude_prefixes: &exclude,
            }),
        )
        .await;
        assert!(
            sitemap.contains("https://example.com/en/apikeys"),
            "/apikeys must be localized, not swept in with /api; got:\n{sitemap}"
        );
        assert!(sitemap.contains("https://example.com/es/apikeys"));
    }

    // ── hreflang alternates (issue #1251) ────────────────────────────────────

    #[test]
    fn locale_alternates_includes_every_supported_locale_and_x_default() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let alternates = locale_alternates("https://example.com", "/posts", "en", &supported);
        assert_eq!(
            alternates,
            vec![
                ("en".to_owned(), "https://example.com/en/posts".to_owned()),
                ("es".to_owned(), "https://example.com/es/posts".to_owned()),
                (
                    "x-default".to_owned(),
                    "https://example.com/en/posts".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn locale_alternates_trims_base_url_trailing_slash() {
        let supported = vec!["en".to_owned()];
        let alternates = locale_alternates("https://example.com/", "/about", "en", &supported);
        assert_eq!(
            alternates,
            vec![
                ("en".to_owned(), "https://example.com/en/about".to_owned()),
                (
                    "x-default".to_owned(),
                    "https://example.com/en/about".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn locale_alternates_root_path_has_no_trailing_slash() {
        // axum's `nest("/en", router)` makes bare "/en" match the inner
        // router's "/" route — "/en/" 404s.
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let alternates = locale_alternates("https://example.com", "/", "en", &supported);
        assert_eq!(
            alternates,
            vec![
                ("en".to_owned(), "https://example.com/en".to_owned()),
                ("es".to_owned(), "https://example.com/es".to_owned()),
                ("x-default".to_owned(), "https://example.com/en".to_owned()),
            ]
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn seo_meta_renders_hreflang_alternate_links() {
        let meta = SeoMeta::new().hreflang_alternates(locale_alternates(
            "https://example.com",
            "/posts",
            "en",
            &["en".to_owned(), "es".to_owned()],
        ));
        let rendered = meta.render().into_string();
        assert!(rendered.contains(
            r#"<link rel="alternate" hreflang="en" href="https://example.com/en/posts">"#
        ));
        assert!(rendered.contains(
            r#"<link rel="alternate" hreflang="es" href="https://example.com/es/posts">"#
        ));
        assert!(rendered.contains(
            r#"<link rel="alternate" hreflang="x-default" href="https://example.com/en/posts">"#
        ));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn seo_meta_without_alternates_renders_no_hreflang_links() {
        let meta = SeoMeta::new().title("Home");
        assert!(!meta.render().into_string().contains("hreflang"));
    }
}
