//! Route-level SEO for reddit-clone: canonical URLs and a database-backed
//! sitemap.
//!
//! This module holds the two parts of SEO that are not route attributes:
//!
//! 1. [`RedditSitemapSource`] — the [`SitemapSource`] that lists every
//!    community and every post in `/sitemap.xml`.
//! 2. [`base_url`] and [`with_canonical`] — the helpers that make an absolute
//!    canonical URL from a request path.
//!
//! The per-page meta tags live on the route attributes themselves. See
//! `routes::posts::front_page`, `routes::posts::show`,
//! `routes::subreddits::show`, and `routes::about::about`.
//!
//! The guide for this module is `docs/guide/seo.md`.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use autumn_web::config::AutumnConfig;
use autumn_web::db::RuntimeConnection;
use autumn_web::seo::{SeoMeta, SitemapChangefreq, SitemapEntry, SitemapSource};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::Pool;

use crate::schema::{posts, subreddits};

/// The maximum number of posts in `/sitemap.xml`.
///
/// The sitemap protocol permits 50,000 URLs in one file. This example stops
/// well below that limit. A larger site must serve a sitemap index instead.
/// See `docs/guide/seo.md`.
const MAX_POST_ENTRIES: i64 = 5_000;

/// The maximum number of communities in `/sitemap.xml`.
const MAX_SUBREDDIT_ENTRIES: i64 = 1_000;

/// The site base URL, without a trailing slash.
///
/// `main` writes this value once at start-up. The handlers read it on each
/// request. A `OnceLock` keeps the read cheap: the alternative is the
/// `AutumnConfig` extractor, which clones the full configuration for every
/// request that wants one string.
static BASE_URL: OnceLock<Option<String>> = OnceLock::new();

/// Record the `[seo] base_url` value from `autumn.toml`.
///
/// Call this one time, before the application starts. Later calls do nothing.
pub fn init_base_url(config: &AutumnConfig) {
    let base = config
        .seo
        .base_url
        .as_deref()
        .map(|url| url.trim_end_matches('/').to_owned());
    let _ = BASE_URL.set(base);
}

/// Return the configured base URL, or `None` when `autumn.toml` sets none.
#[must_use]
pub fn base_url() -> Option<&'static str> {
    BASE_URL.get().and_then(Option::as_deref)
}

/// Make an absolute URL from an application path.
///
/// Returns `None` when no base URL is configured. A canonical tag must be an
/// absolute URL, so the caller omits the tag in that case.
#[must_use]
pub fn absolute_url(path: &str) -> Option<String> {
    base_url().map(|base| format!("{base}{path}"))
}

/// The pure half of [`with_canonical`], with the base URL passed in.
///
/// [`BASE_URL`] is process-wide, so the tests exercise this function instead.
#[must_use]
fn with_canonical_in(seo: SeoMeta, base: Option<&str>, path: &str) -> SeoMeta {
    match base {
        Some(base) => seo.canonical(format!("{base}{path}")),
        None => seo,
    }
}

/// Add a canonical URL to `seo` for the given application path.
///
/// The tag tells a crawler which URL is the true address of the page. This
/// application shows the same post at two paths — `/posts/{id}` redirects to
/// `/r/{sub}/posts/{slug}` — and a query string makes more variants. The
/// canonical tag makes all of them collapse to one URL.
///
/// If no base URL is configured, `seo` comes back unchanged.
#[must_use]
pub fn with_canonical(seo: SeoMeta, path: &str) -> SeoMeta {
    with_canonical_in(seo, base_url(), path)
}

/// Cut `text` down to a one-line page description.
///
/// A description tag is a short summary. Search engines show approximately 155
/// characters, so this function stops at `max_chars` and adds an ellipsis. It
/// also replaces each run of whitespace with one space, because the source
/// text is a multi-line post body.
///
/// Returns `None` for text that has no words, for example a link-only post.
#[must_use]
pub fn summarize(text: &str, max_chars: usize) -> Option<String> {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    if flat.chars().count() <= max_chars {
        return Some(flat);
    }
    // Cut on a character boundary, then back off to the last word break so the
    // description does not end in the middle of a word.
    let head: String = flat.chars().take(max_chars).collect();
    let trimmed = match head.rfind(' ') {
        Some(idx) if idx > 0 => &head[..idx],
        _ => head.as_str(),
    };
    Some(format!("{}\u{2026}", trimmed.trim_end()))
}

/// The application's sitemap source.
///
/// The framework calls [`SitemapSource::entries`] one time, while it builds
/// the router. It renders the result into a static `/sitemap.xml` body. The
/// sitemap is therefore a snapshot of the database at start-up. That is the
/// correct trade for this application: the body costs nothing to serve, and a
/// crawler reads it minutes or hours after a deploy. An application that must
/// list new content immediately serves its own `/sitemap.xml` route instead;
/// see `docs/guide/seo.md`.
pub struct RedditSitemapSource {
    /// A pool of this source's own. The application state does not exist yet
    /// when the framework collects sitemap entries, so the source cannot use
    /// the `Db` extractor.
    pool: Option<Pool<RuntimeConnection>>,
}

impl RedditSitemapSource {
    /// Build the source from the loaded configuration.
    ///
    /// The source stays quiet when the application runs without a database:
    /// it then contributes no entries, and `/sitemap.xml` still lists the
    /// static routes the framework derives on its own.
    #[must_use]
    pub fn from_config(config: &AutumnConfig) -> Self {
        let pool = match autumn_web::db::create_pool(&config.database) {
            Ok(pool) => pool,
            Err(err) => {
                tracing::warn!(error = %err, "sitemap: cannot build a pool; sitemap will list static routes only");
                None
            }
        };
        Self { pool }
    }

    /// Read the database and build one entry for each public page.
    ///
    /// A failure here is not fatal. The function logs the failure and returns
    /// the entries it has. A short sitemap is better than a failed boot.
    async fn collect(&self) -> Vec<SitemapEntry> {
        let Some(base) = base_url() else {
            tracing::warn!(
                "sitemap: [seo] base_url is not set; the sitemap needs absolute URLs, so it stays empty"
            );
            return Vec::new();
        };
        let Some(pool) = self.pool.as_ref() else {
            return Vec::new();
        };
        let mut conn = match pool.get().await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::warn!(error = %err, "sitemap: cannot get a connection; skipping dynamic entries");
                return Vec::new();
            }
        };

        // The two hub pages. `/` and `/r` are `#[get]` routes, so the
        // framework cannot derive them: it derives only `#[static_get]` paths.
        // `/about` is a `#[static_get]` route and arrives automatically.
        let mut entries = vec![
            SitemapEntry::new(format!("{base}/"))
                .changefreq(SitemapChangefreq::Hourly)
                .priority(1.0),
            SitemapEntry::new(format!("{base}/r"))
                .changefreq(SitemapChangefreq::Daily)
                .priority(0.8),
        ];

        let subs: Vec<String> = match subreddits::table
            .order(subreddits::name.asc())
            .limit(MAX_SUBREDDIT_ENTRIES)
            .select(subreddits::slug)
            .load(&mut conn)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "sitemap: community query failed");
                Vec::new()
            }
        };
        for slug in subs {
            entries.push(
                SitemapEntry::new(format!("{base}/r/{slug}"))
                    .changefreq(SitemapChangefreq::Daily)
                    .priority(0.7),
            );
        }

        // `updated_at` becomes each post's `<lastmod>`, which tells a crawler
        // whether it must fetch the page again.
        let rows: Vec<(String, String, chrono::NaiveDateTime)> = match posts::table
            .inner_join(subreddits::table)
            .order(posts::updated_at.desc())
            .limit(MAX_POST_ENTRIES)
            .select((subreddits::slug, posts::slug, posts::updated_at))
            .load(&mut conn)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "sitemap: post query failed");
                Vec::new()
            }
        };
        for (sub_slug, post_slug, updated_at) in rows {
            entries.push(
                SitemapEntry::new(format!("{base}/r/{sub_slug}/posts/{post_slug}"))
                    .lastmod(updated_at.format("%Y-%m-%d").to_string())
                    .changefreq(SitemapChangefreq::Weekly)
                    .priority(0.6),
            );
        }

        tracing::info!(count = entries.len(), "sitemap: collected entries");
        entries
    }
}

impl SitemapSource for RedditSitemapSource {
    fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>> {
        Box::pin(self.collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_keeps_short_text_unchanged() {
        assert_eq!(
            summarize("A short body.", 155).as_deref(),
            Some("A short body.")
        );
    }

    #[test]
    fn summarize_collapses_whitespace() {
        assert_eq!(
            summarize("first line\n\n  second line", 155).as_deref(),
            Some("first line second line")
        );
    }

    #[test]
    fn summarize_cuts_on_a_word_break() {
        let out = summarize("alpha bravo charlie delta", 14).expect("some text");
        assert_eq!(out, "alpha bravo\u{2026}");
    }

    #[test]
    fn summarize_returns_none_for_blank_text() {
        assert!(summarize("   \n\t ", 155).is_none());
        assert!(summarize("", 155).is_none());
    }

    #[test]
    fn summarize_handles_multibyte_text() {
        // The cut must land on a character boundary, not a byte boundary.
        let out = summarize("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 3).expect("some text");
        assert_eq!(out, "\u{e9}\u{e9}\u{e9}\u{2026}");
    }

    #[test]
    fn with_canonical_adds_an_absolute_url() {
        let seo = with_canonical_in(
            SeoMeta::new().title("t"),
            Some("https://example.com"),
            "/r/rust/posts/hello",
        );
        assert!(
            seo.render().into_string().contains(
                r#"<link rel="canonical" href="https://example.com/r/rust/posts/hello">"#
            ),
            "canonical tag missing: {}",
            seo.render().into_string()
        );
    }

    #[test]
    fn with_canonical_is_a_no_op_without_a_base_url() {
        let seo = with_canonical_in(SeoMeta::new().title("t"), None, "/r/rust");
        assert_eq!(seo, SeoMeta::new().title("t"));
    }
}
