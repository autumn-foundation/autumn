//! Atom and RSS 2.0 feed rendering for content apps.
//!
//! Build a [`Feed`] from channel metadata plus an iterator of [`FeedEntry`]
//! values and return it directly from a `#[get]` handler — it implements
//! [`IntoResponse`] and sets the correct `Content-Type`
//! (`application/atom+xml` for Atom, `application/rss+xml` for RSS), UTF-8,
//! with every text field XML-escaped.
//!
//! For conditional GET, [`Feed::conditional`] reuses the crate's [`crate::etag`]
//! machinery so feed pollers get a `304 Not Modified` on the unchanged-content
//! path instead of re-downloading an unchanged feed.
//!
//! ```
//! use autumn_web::feed::{Feed, FeedEntry};
//! use chrono::{TimeZone, Utc};
//!
//! let published = Utc.with_ymd_and_hms(2026, 5, 31, 12, 0, 0).unwrap();
//! let feed = Feed::atom(
//!     "My Blog",
//!     "https://example.com/",
//!     "https://example.com/feed.xml",
//! )
//! .author("Jane Doe")
//! .entries([FeedEntry::new(
//!     "https://example.com/posts/hello",
//!     "Hello, world",
//!     "https://example.com/posts/hello",
//! )
//! .summary("First post")
//! .published(published)
//! .updated(published)]);
//!
//! let xml = feed.render();
//! assert!(xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));
//! assert!(xml.contains("<title>My Blog</title>"));
//! ```

use std::fmt::Write as _;

use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use http::header::CONTENT_TYPE;

use crate::etag::{ETag, fresh_when, hash_etag};

/// Wire format for a rendered [`Feed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedFormat {
    /// Atom 1.0 (`application/atom+xml`).
    Atom,
    /// RSS 2.0 (`application/rss+xml`).
    Rss,
}

impl FeedFormat {
    /// The `Content-Type` header value (including `charset=utf-8`).
    #[must_use]
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Atom => "application/atom+xml; charset=utf-8",
            Self::Rss => "application/rss+xml; charset=utf-8",
        }
    }
}

/// A syndication feed: channel metadata plus a list of [`FeedEntry`] items.
#[derive(Debug, Clone)]
pub struct Feed {
    format: FeedFormat,
    title: String,
    site_link: String,
    self_link: String,
    author: Option<String>,
    description: Option<String>,
    updated: Option<DateTime<Utc>>,
    entries: Vec<FeedEntry>,
}

/// A single item in a [`Feed`].
#[derive(Debug, Clone)]
pub struct FeedEntry {
    id: String,
    title: String,
    link: String,
    summary: Option<String>,
    content: Option<String>,
    published: Option<DateTime<Utc>>,
    updated: Option<DateTime<Utc>>,
}

impl Feed {
    /// Start an Atom 1.0 feed from its channel metadata.
    ///
    /// `site_link` is the human-facing site URL (Atom `alternate` link);
    /// `self_link` is the canonical URL of the feed document itself (Atom
    /// `self` link), which should match the route the feed is served from.
    ///
    /// ```
    /// use autumn_web::feed::Feed;
    /// let feed = Feed::atom("My Blog", "https://example.com/", "https://example.com/feed.xml");
    /// assert!(feed.render().contains("http://www.w3.org/2005/Atom"));
    /// ```
    #[must_use]
    pub fn atom(
        title: impl Into<String>,
        site_link: impl Into<String>,
        self_link: impl Into<String>,
    ) -> Self {
        Self::new(FeedFormat::Atom, title, site_link, self_link)
    }

    /// Start an RSS 2.0 feed from its channel metadata.
    ///
    /// ```
    /// use autumn_web::feed::Feed;
    /// let feed = Feed::rss("My Blog", "https://example.com/", "https://example.com/feed.xml");
    /// assert!(feed.render().contains("<rss version=\"2.0\""));
    /// ```
    #[must_use]
    pub fn rss(
        title: impl Into<String>,
        site_link: impl Into<String>,
        self_link: impl Into<String>,
    ) -> Self {
        Self::new(FeedFormat::Rss, title, site_link, self_link)
    }

    fn new(
        format: FeedFormat,
        title: impl Into<String>,
        site_link: impl Into<String>,
        self_link: impl Into<String>,
    ) -> Self {
        Self {
            format,
            title: title.into(),
            site_link: site_link.into(),
            self_link: self_link.into(),
            author: None,
            description: None,
            updated: None,
            entries: Vec::new(),
        }
    }

    /// Set the feed author (Atom `<author><name>`; RSS `<dc:creator>`).
    ///
    /// For Atom the feed title is used as the feed-level author when none is
    /// set, since Atom 1.0 requires a feed-level author unless every entry
    /// carries its own.
    #[must_use]
    pub fn author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Set the channel description (required for RSS; defaults to the title).
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the channel `updated` timestamp explicitly. When unset it is derived
    /// from the newest entry.
    #[must_use]
    pub const fn updated(mut self, updated: DateTime<Utc>) -> Self {
        self.updated = Some(updated);
        self
    }

    /// Append a single entry.
    #[must_use]
    pub fn entry(mut self, entry: FeedEntry) -> Self {
        self.entries.push(entry);
        self
    }

    /// Append every entry produced by `entries`.
    #[must_use]
    pub fn entries(mut self, entries: impl IntoIterator<Item = FeedEntry>) -> Self {
        self.entries.extend(entries);
        self
    }

    /// The `Content-Type` header value for this feed's format.
    #[must_use]
    pub const fn content_type(&self) -> &'static str {
        self.format.content_type()
    }

    /// The newest timestamp across the feed: the explicit channel `updated` if
    /// set, otherwise the maximum `updated`/`published` across entries.
    #[must_use]
    pub fn last_updated(&self) -> Option<DateTime<Utc>> {
        if let Some(updated) = self.updated {
            return Some(updated);
        }
        self.entries
            .iter()
            .filter_map(|e| e.updated.or(e.published))
            .max()
    }

    /// A **weak** [`ETag`] over the exact rendered feed bytes, suitable for
    /// conditional GET: it changes whenever any rendered field changes —
    /// including two edits within the same whole second, or content edits that
    /// leave timestamps untouched — and is stable otherwise.
    ///
    /// The tag is weak (a weak body hash) so it remains a valid validator even
    /// when the framework applies a content-coding (gzip/br) to the response:
    /// weak validators denote semantic equivalence across representations that
    /// may differ only in content-coding, whereas a strong validator would be
    /// violated by serving byte-different identity and compressed bodies under
    /// one tag. This mirrors what [`crate::etag::EtagLayer`] does for
    /// body-derived tags.
    #[must_use]
    pub fn etag(&self) -> ETag {
        hash_etag(&self.render())
    }

    /// Wrap this feed in conditional-GET handling, reusing [`crate::etag`].
    ///
    /// The weak `ETag` — a weak hash of the exact rendered body — is the sole
    /// freshness validator: only a matching `If-None-Match` yields `304 Not
    /// Modified`. The tag is weak so it stays a valid validator even when the
    /// framework applies a content-coding (gzip/br) to the response; weak
    /// `If-None-Match` comparison still yields `304` on match. `Last-Modified`
    /// is emitted for caches/readers, but a coarse whole-second
    /// `If-Modified-Since` is deliberately NOT honored on its own, so a body
    /// that changes within the same second (or via a feed-level edit that
    /// leaves entry timestamps unchanged) can never be served stale.
    #[must_use]
    pub fn conditional(self, request_headers: &http::HeaderMap) -> Response {
        let content_type = self.content_type();
        let body = self.render();
        let etag = hash_etag(&body);
        let last_modified = self.last_updated();
        let mut headers = request_headers.clone();
        headers.remove(http::header::IF_MODIFIED_SINCE);
        fresh_when(&headers, etag)
            .last_modified(last_modified)
            .or_else(move || ([(CONTENT_TYPE, content_type)], body))
            .into_response()
    }

    /// Render the feed to an XML string in its format.
    #[must_use]
    pub fn render(&self) -> String {
        match self.format {
            FeedFormat::Atom => self.render_atom(),
            FeedFormat::Rss => self.render_rss(),
        }
    }

    fn render_atom(&self) -> String {
        let updated = self.last_updated().unwrap_or_else(fallback_timestamp);
        let mut s = String::new();
        s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
        s.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
        writeln!(s, "  <title>{}</title>", escape(&self.title)).unwrap();
        writeln!(s, "  <id>{}</id>", escape(&self.self_link)).unwrap();
        writeln!(s, "  <link href=\"{}\"/>", escape(&self.site_link)).unwrap();
        writeln!(
            s,
            "  <link href=\"{}\" rel=\"self\" type=\"application/atom+xml\"/>",
            escape(&self.self_link)
        )
        .unwrap();
        writeln!(s, "  <updated>{}</updated>", rfc3339(updated)).unwrap();
        // Atom requires a feed-level author unless every entry has one; default
        // to the feed title so the feed is always valid.
        let author = self.author.as_deref().unwrap_or(&self.title);
        s.push_str("  <author>\n");
        writeln!(s, "    <name>{}</name>", escape(author)).unwrap();
        s.push_str("  </author>\n");
        for e in &self.entries {
            s.push_str("  <entry>\n");
            writeln!(s, "    <title>{}</title>", escape(&e.title)).unwrap();
            writeln!(s, "    <id>{}</id>", escape(&e.id)).unwrap();
            writeln!(s, "    <link href=\"{}\"/>", escape(&e.link)).unwrap();
            let entry_updated = e.updated.or(e.published).unwrap_or(updated);
            writeln!(s, "    <updated>{}</updated>", rfc3339(entry_updated)).unwrap();
            if let Some(published) = e.published {
                writeln!(s, "    <published>{}</published>", rfc3339(published)).unwrap();
            }
            if let Some(summary) = &e.summary {
                writeln!(s, "    <summary>{}</summary>", escape(summary)).unwrap();
            }
            if let Some(content) = &e.content {
                writeln!(
                    s,
                    "    <content type=\"html\">{}</content>",
                    escape(content)
                )
                .unwrap();
            }
            s.push_str("  </entry>\n");
        }
        s.push_str("</feed>\n");
        s
    }

    fn render_rss(&self) -> String {
        let updated = self.last_updated().unwrap_or_else(fallback_timestamp);
        let description = self.description.as_deref().unwrap_or(&self.title);
        let mut s = String::new();
        s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
        s.push_str("<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n");
        s.push_str("  <channel>\n");
        writeln!(s, "    <title>{}</title>", escape(&self.title)).unwrap();
        writeln!(s, "    <link>{}</link>", escape(&self.site_link)).unwrap();
        writeln!(s, "    <description>{}</description>", escape(description)).unwrap();
        if let Some(author) = &self.author {
            writeln!(s, "    <dc:creator>{}</dc:creator>", escape(author)).unwrap();
        }
        writeln!(
            s,
            "    <atom:link href=\"{}\" rel=\"self\" type=\"application/rss+xml\"/>",
            escape(&self.self_link)
        )
        .unwrap();
        writeln!(s, "    <lastBuildDate>{}</lastBuildDate>", rfc2822(updated)).unwrap();
        for e in &self.entries {
            s.push_str("    <item>\n");
            writeln!(s, "      <title>{}</title>", escape(&e.title)).unwrap();
            writeln!(s, "      <link>{}</link>", escape(&e.link)).unwrap();
            writeln!(
                s,
                "      <guid isPermaLink=\"false\">{}</guid>",
                escape(&e.id)
            )
            .unwrap();
            let body = e.content.as_deref().or(e.summary.as_deref());
            if let Some(body) = body {
                writeln!(s, "      <description>{}</description>", escape(body)).unwrap();
            }
            if let Some(published) = e.published.or(e.updated) {
                writeln!(s, "      <pubDate>{}</pubDate>", rfc2822(published)).unwrap();
            }
            s.push_str("    </item>\n");
        }
        s.push_str("  </channel>\n");
        s.push_str("</rss>\n");
        s
    }
}

impl FeedEntry {
    /// Start an entry from its stable id, title, and link.
    ///
    /// `id` should be a stable, globally-unique identifier — a permalink URL is
    /// the usual choice.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>, link: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            link: link.into(),
            summary: None,
            content: None,
            published: None,
            updated: None,
        }
    }

    /// Set a short text summary (Atom `<summary>`, RSS `<description>`).
    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Set the full content (Atom `<content type="html">`, preferred RSS body).
    #[must_use]
    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// Set the publication timestamp.
    #[must_use]
    pub const fn published(mut self, published: DateTime<Utc>) -> Self {
        self.published = Some(published);
        self
    }

    /// Set the last-updated timestamp.
    #[must_use]
    pub const fn updated(mut self, updated: DateTime<Utc>) -> Self {
        self.updated = Some(updated);
        self
    }
}

impl IntoResponse for Feed {
    fn into_response(self) -> Response {
        let content_type = self.content_type();
        let body = self.render();
        ([(CONTENT_TYPE, content_type)], body).into_response()
    }
}

/// Deterministic fallback used for `<updated>` / `lastBuildDate` when a feed
/// has no channel timestamp and no dated entries, so the rendered body (and the
/// `ETag` derived from it) stays stable across renders.
const fn fallback_timestamp() -> DateTime<Utc> {
    DateTime::from_timestamp(0, 0).expect("unix epoch is a valid timestamp")
}

fn rfc3339(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn rfc2822(dt: DateTime<Utc>) -> String {
    dt.to_rfc2822()
}

/// XML-escape a text value: `&`, `<`, `>`, `"`, `'`. Characters outside the
/// XML 1.0 `Char` set (control characters below space other than tab, newline,
/// and carriage return) are dropped, since they cannot appear in a well-formed
/// XML document even when escaped. Escaping `>` also neutralises any `]]>`
/// sequence so untrusted titles/bodies cannot break out of the document.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !is_xml_char(c) {
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Whether `c` is a valid XML 1.0 character (XML 1.0 §2.2 `Char` production).
/// Rust `char` already excludes surrogates, so only the low control range and
/// the two non-characters `U+FFFE`/`U+FFFF` need filtering here.
const fn is_xml_char(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{A}' | '\u{D}'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}'
    )
}

#[cfg(test)]
mod proptests {
    //! Property-based invariants for XML escaping and feed rendering. `escape`
    //! is private, so it is exercised in-crate here.
    use super::*;
    use proptest::prelude::*;

    /// Reverse the five entities `escape` emits. `&amp;` is decoded LAST so a
    /// payload like `&lt;` is not double-decoded.
    fn unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// `escape` never emits a raw `<` or `>`: these are always turned into
        /// entities, so an escaped value can never open or close a tag. This is
        /// the core XML-injection-safety invariant.
        #[test]
        fn escape_has_no_raw_angle_brackets(s in ".*") {
            let out = escape(&s);
            prop_assert!(!out.contains('<'));
            prop_assert!(!out.contains('>'));
        }

        /// Escaping is a lossless, reversible transform over the XML-legal
        /// characters of the input: un-escaping the output recovers exactly the
        /// input with non-XML characters dropped.
        #[test]
        fn escape_round_trips_over_xml_chars(s in ".*") {
            let filtered: String = s.chars().filter(|&c| is_xml_char(c)).collect();
            prop_assert_eq!(unescape(&escape(&s)), filtered);
        }

        /// `Feed::render` (both formats) never panics on arbitrary field values,
        /// always produces the XML prolog, and never lets an injected `<` from a
        /// field body appear raw in the output.
        #[test]
        fn render_never_panics_and_escapes_fields(
            title in ".*",
            body in ".*",
            rss in any::<bool>(),
        ) {
            // Salt the fields with tag-like markers to prove they are escaped.
            let marked_title = format!("<x>{title}");
            let marked_body = format!("</feed>{body}");
            let feed = if rss {
                Feed::rss(marked_title, "https://example.com/", "https://example.com/feed.xml")
            } else {
                Feed::atom(marked_title, "https://example.com/", "https://example.com/feed.xml")
            }
            .description("d")
            .entry(FeedEntry::new("id-1", "entry title", "https://example.com/1").content(marked_body));

            let out = feed.render();
            prop_assert!(out.starts_with("<?xml version=\"1.0\""));
            // The injected markers must not survive as raw tags.
            prop_assert!(!out.contains("<x>"));
            // Structural `</feed>` appears exactly once (Atom) / zero times
            // (RSS) — the injected copy in the body must not add another.
            let expected_close = if rss { 0 } else { 1 };
            prop_assert_eq!(out.matches("</feed>").count(), expected_close);
        }
    }
}
