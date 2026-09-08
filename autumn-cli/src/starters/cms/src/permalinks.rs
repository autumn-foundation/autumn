//! Permalinks — URL generation and front-end request resolution.
//!
//! WordPress's permalink system is a settings-driven rewrite table: an operator
//! picks a structure, WordPress compiles it to a list of regular expressions,
//! and `WP_Query` reverses those captures back into a query. Getting the two
//! halves to agree is a perennial source of 404s after a settings change.
//!
//! Here generation ([`PermalinkStructure::permalink`]) and resolution
//! ([`resolve`]) are two functions in one module with a round-trip property
//! test between them, so a structure that renders a URL the resolver cannot
//! read is a failing test rather than a broken site.

use crate::content_types;

/// The URL shape for a post's permalink. The variants are WordPress's own
/// "Common Settings" choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PermalinkStructure {
    /// `/?p=123` — no rewriting at all.
    Plain,
    /// `/archives/123`
    Numeric,
    /// `/sample-post` — WordPress's recommended default, and ours.
    #[default]
    PostName,
    /// `/2026/09/sample-post`
    MonthAndName,
    /// `/2026/09/07/sample-post`
    DayAndName,
}

impl PermalinkStructure {
    /// The stored option value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Numeric => "numeric",
            Self::PostName => "post_name",
            Self::MonthAndName => "month_and_name",
            Self::DayAndName => "day_and_name",
        }
    }

    /// The label the settings screen shows, with WordPress's own example URL.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plain => "Plain — /?p=123",
            Self::Numeric => "Numeric — /archives/123",
            Self::PostName => "Post name — /sample-post",
            Self::MonthAndName => "Month and name — /2026/09/sample-post",
            Self::DayAndName => "Day and name — /2026/09/07/sample-post",
        }
    }

    /// Parse a stored option value, falling back to the default.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "plain" => Self::Plain,
            "numeric" => Self::Numeric,
            "month_and_name" => Self::MonthAndName,
            "day_and_name" => Self::DayAndName,
            _ => Self::PostName,
        }
    }

    /// Every structure, in the order the settings screen lists them.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Plain,
            Self::Numeric,
            Self::PostName,
            Self::MonthAndName,
            Self::DayAndName,
        ]
    }

    /// The permalink for a piece of content.
    ///
    /// Only the `post` type follows the configured structure. A `page` is
    /// always addressed by its path (`/about/team`) and a custom type is always
    /// prefixed by its slug (`/product/widget`) — exactly as WordPress does,
    /// because a dated URL for an undated content type reads as a bug.
    #[must_use]
    pub fn permalink(self, post: &crate::models::Post, ancestry: &[String]) -> String {
        match post.post_type.as_str() {
            "page" => {
                if ancestry.is_empty() {
                    format!("/{}", post.slug)
                } else {
                    format!("/{}/{}", ancestry.join("/"), post.slug)
                }
            }
            "post" => self.post_permalink(post),
            other => format!("/{other}/{}", post.slug),
        }
    }

    fn post_permalink(self, post: &crate::models::Post) -> String {
        // Dated structures need a date. A post that has never been published
        // has no `published_at`, so fall back to its creation date rather than
        // rendering `//sample-post`.
        let date = post.published_at.unwrap_or(post.created_at);
        match self {
            Self::Plain => format!("/?p={}", post.id),
            Self::Numeric => format!("/archives/{}", post.id),
            Self::PostName => format!("/{}", post.slug),
            Self::MonthAndName => {
                format!("/{}/{}", date.format("%Y/%m"), post.slug)
            }
            Self::DayAndName => {
                format!("/{}/{}", date.format("%Y/%m/%d"), post.slug)
            }
        }
    }
}

/// What a front-end request path resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// `/` — the blog index, or the configured static front page.
    FrontPage,
    /// A single post or custom-type item, addressed by slug.
    Single { post_type: String, slug: String },
    /// A single post addressed by id (the `numeric` structure).
    SingleById { id: i64 },
    /// A hierarchical page, addressed by its full ancestry path.
    Page { path: Vec<String> },
    /// A term archive: `/category/rust`, `/tag/async`.
    TermArchive { taxonomy: String, slug: String },
    /// A term's syndication feed: `/category/rust/feed`.
    ///
    /// Resolved here rather than mounted as its own route: a literal
    /// `/{taxonomy_base}/{slug}/feed` route and the `/{*path}` catch-all are a
    /// route-shape conflict the router rejects at build time, and WordPress's
    /// URL for this really is `/category/rust/feed` rather than something under
    /// a reserved prefix.
    TermFeed { taxonomy: String, slug: String },
    /// `/author/jane`
    AuthorArchive { username: String },
    /// The date-ordered archive of a custom post type.
    PostTypeArchive { post_type: String },
    /// `/2026`, `/2026/09`, `/2026/09/07`
    DateArchive {
        year: i32,
        month: Option<u32>,
        day: Option<u32>,
    },
    /// Nothing matched.
    NotFound,
}

/// Resolve a front-end request path.
///
/// The order is significant and mirrors WordPress's rewrite priority: the
/// reserved prefixes (taxonomy bases, `author`, `archives`, custom-type
/// archives) are matched before anything is treated as content, so a category
/// called "author" cannot shadow the author archive — and, more importantly, a
/// *post* slugged `category` cannot make every category URL unreachable.
///
/// Resolution is deliberately independent of the configured
/// [`PermalinkStructure`]: every dated structure ends in the post slug, so
/// taking the last segment reads all of them. Changing the setting therefore
/// does not 404 the URLs already in the wild, which is the single most common
/// WordPress permalink complaint.
#[must_use]
pub fn resolve(path: &str) -> Resolved {
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode(s).to_lowercase())
        .collect();

    if segments.is_empty() {
        return Resolved::FrontPage;
    }

    // 1. Term feeds: `/{rewrite_base}/{slug}/feed`. Checked before the archive
    //    so a term slugged `feed` cannot make every term feed unreachable.
    if segments.len() == 3 && segments[2] == "feed" {
        for taxonomy in content_types::all_taxonomies() {
            if segments[0] == taxonomy.rewrite_base {
                return Resolved::TermFeed {
                    taxonomy: taxonomy.slug.to_owned(),
                    slug: segments[1].clone(),
                };
            }
        }
    }

    // 2. Term archives: `/{rewrite_base}/{slug}`.
    if segments.len() == 2 {
        for taxonomy in content_types::all_taxonomies() {
            if segments[0] == taxonomy.rewrite_base {
                return Resolved::TermArchive {
                    taxonomy: taxonomy.slug.to_owned(),
                    slug: segments[1].clone(),
                };
            }
        }
        //    Author archive.
        if segments[0] == "author" {
            return Resolved::AuthorArchive {
                username: segments[1].clone(),
            };
        }
        // 3. The `numeric` permalink structure.
        if segments[0] == "archives"
            && let Ok(id) = segments[1].parse::<i64>()
        {
            return Resolved::SingleById { id };
        }
    }

    // 4. Date archives — an all-numeric path of one to three segments.
    if let Some(archive) = date_archive(&segments) {
        return archive;
    }

    // 5. Custom post types: `/{archive_base}` is the archive, and
    //    `/{slug}/{item}` is one item of it. `post` and `page` are excluded —
    //    they own the bare paths handled below.
    for post_type in content_types::all_post_types() {
        if !post_type.public || matches!(post_type.slug, "post" | "page") {
            continue;
        }
        if segments.len() == 1 && post_type.has_archive && segments[0] == post_type.archive_base {
            return Resolved::PostTypeArchive {
                post_type: post_type.slug.to_owned(),
            };
        }
        if segments.len() == 2 && segments[0] == post_type.slug {
            return Resolved::Single {
                post_type: post_type.slug.to_owned(),
                slug: segments[1].clone(),
            };
        }
    }

    // 6. A single bare segment is either a post or a top-level page; the
    //    handler tries the page first (a page is the more specific claim on a
    //    bare path) and falls back to the post.
    if segments.len() == 1 {
        return Resolved::Single {
            post_type: "post".to_owned(),
            slug: segments[0].clone(),
        };
    }

    // 7. Anything deeper is either a dated post permalink — whose last segment
    //    is the slug — or a nested page path. `Page` carries the whole path so
    //    the handler can walk the ancestry; if that finds nothing it retries
    //    the last segment as a post.
    Resolved::Page { path: segments }
}

/// Match `/2026`, `/2026/09` and `/2026/09/07`, and nothing else.
fn date_archive(segments: &[String]) -> Option<Resolved> {
    if segments.is_empty() || segments.len() > 3 {
        return None;
    }
    if !segments
        .iter()
        .all(|s| s.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    // A four-digit leading segment is a year; anything else numeric is not a
    // date (an `/archives/123`-style id is handled above, and a post slugged
    // `123` still resolves as a post because it is one segment of three digits,
    // not four).
    if segments[0].len() != 4 {
        return None;
    }
    let year: i32 = segments[0].parse().ok()?;
    let month = match segments.get(1) {
        Some(raw) => Some(raw.parse::<u32>().ok().filter(|m| (1..=12).contains(m))?),
        None => None,
    };
    let day = match segments.get(2) {
        Some(raw) => Some(raw.parse::<u32>().ok().filter(|d| (1..=31).contains(d))?),
        None => None,
    };
    Some(Resolved::DateArchive { year, month, day })
}

/// Decode `%XX` escapes in a path segment.
///
/// Slugs are ASCII by construction (`slugify` guarantees it), but a request can
/// carry anything, and a percent-encoded segment that reached the database
/// comparison undecoded would silently 404 a legitimate URL.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // A malformed sequence must not panic the router; lossy decoding turns it
    // into something that simply will not match any slug.
    String::from_utf8_lossy(&out).into_owned()
}

/// A representative `Post`, shared by the tests in this crate that need one.
#[cfg(test)]
pub(crate) mod tests_support {
    use crate::models::Post;

    /// A published post dated 2026-09-07, id 123, slug `sample-post`.
    pub(crate) fn sample_post() -> Post {
        let published = chrono::NaiveDate::from_ymd_opt(2026, 9, 7)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        Post {
            id: 123,
            post_type: "post".to_owned(),
            title: "Sample Post".to_owned(),
            slug: "sample-post".to_owned(),
            excerpt: String::new(),
            body: String::new(),
            status: "publish".to_owned(),
            author_id: 1,
            parent_id: None,
            featured_media_id: None,
            menu_order: 0,
            comment_status: "open".to_owned(),
            password: String::new(),
            sticky: false,
            comment_count: 0,
            published_at: Some(published),
            lock_version: 0,
            created_at: published,
            updated_at: published,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permalinks::tests_support::sample_post;

    #[test]
    fn every_structure_renders_wordpresss_example_url() {
        let post = sample_post();
        assert_eq!(PermalinkStructure::Plain.permalink(&post, &[]), "/?p=123");
        assert_eq!(
            PermalinkStructure::Numeric.permalink(&post, &[]),
            "/archives/123"
        );
        assert_eq!(
            PermalinkStructure::PostName.permalink(&post, &[]),
            "/sample-post"
        );
        assert_eq!(
            PermalinkStructure::MonthAndName.permalink(&post, &[]),
            "/2026/09/sample-post"
        );
        assert_eq!(
            PermalinkStructure::DayAndName.permalink(&post, &[]),
            "/2026/09/07/sample-post"
        );
    }

    /// The property that keeps a permalink-settings change from 404ing every
    /// URL already in the wild: whatever structure generated it, resolving the
    /// URL finds the same post.
    #[test]
    fn every_generated_permalink_resolves_back_to_its_post() {
        let post = sample_post();
        for structure in PermalinkStructure::all() {
            let url = structure.permalink(&post, &[]);
            // `Plain` is a query string, not a path — it is resolved by the
            // `?p=` handler, not the path router.
            if *structure == PermalinkStructure::Plain {
                assert!(url.starts_with("/?p="));
                continue;
            }
            match resolve(&url) {
                Resolved::Single { slug, .. } => assert_eq!(slug, post.slug, "{url}"),
                Resolved::SingleById { id } => assert_eq!(id, post.id, "{url}"),
                Resolved::Page { path } => {
                    assert_eq!(path.last().unwrap(), &post.slug, "{url}");
                }
                other => panic!("{url} resolved to {other:?}"),
            }
        }
    }

    #[test]
    fn pages_are_addressed_by_ancestry() {
        let mut page = sample_post();
        page.post_type = "page".to_owned();
        page.slug = "team".to_owned();
        let ancestry = vec!["about".to_owned()];
        assert_eq!(
            PermalinkStructure::DayAndName.permalink(&page, &ancestry),
            "/about/team",
            "a page must never take a dated URL"
        );
        assert_eq!(
            resolve("/about/team"),
            Resolved::Page {
                path: vec!["about".to_owned(), "team".to_owned()]
            }
        );
    }

    #[test]
    fn a_term_feed_resolves_before_its_archive() {
        assert_eq!(
            resolve("/category/rust/feed"),
            Resolved::TermFeed {
                taxonomy: "category".to_owned(),
                slug: "rust".to_owned()
            }
        );
        assert_eq!(
            resolve("/tag/async/feed"),
            Resolved::TermFeed {
                taxonomy: "post_tag".to_owned(),
                slug: "async".to_owned()
            }
        );
        // Three segments that are not a feed still fall through to content.
        assert!(matches!(
            resolve("/category/rust/other"),
            Resolved::Page { .. }
        ));
    }

    #[test]
    fn reserved_prefixes_win_over_content() {
        assert_eq!(
            resolve("/category/rust"),
            Resolved::TermArchive {
                taxonomy: "category".to_owned(),
                slug: "rust".to_owned()
            }
        );
        assert_eq!(
            resolve("/tag/async"),
            Resolved::TermArchive {
                taxonomy: "post_tag".to_owned(),
                slug: "async".to_owned()
            }
        );
        assert_eq!(
            resolve("/author/jane"),
            Resolved::AuthorArchive {
                username: "jane".to_owned()
            }
        );
    }

    #[test]
    fn date_archives_match_only_real_dates() {
        assert_eq!(
            resolve("/2026"),
            Resolved::DateArchive {
                year: 2026,
                month: None,
                day: None
            }
        );
        assert_eq!(
            resolve("/2026/09"),
            Resolved::DateArchive {
                year: 2026,
                month: Some(9),
                day: None
            }
        );
        // Month 13 is not a date; it falls through to content resolution.
        assert!(matches!(resolve("/2026/13"), Resolved::Page { .. }));
        // A three-digit segment is a slug, not a year.
        assert!(matches!(resolve("/123"), Resolved::Single { .. }));
    }

    #[test]
    fn root_is_the_front_page() {
        assert_eq!(resolve("/"), Resolved::FrontPage);
        assert_eq!(resolve(""), Resolved::FrontPage);
        assert_eq!(resolve("///"), Resolved::FrontPage);
    }

    #[test]
    fn percent_escapes_are_decoded_before_matching() {
        assert_eq!(
            resolve("/caf%C3%A9"),
            Resolved::Single {
                post_type: "post".to_owned(),
                slug: "café".to_owned()
            }
        );
        // A malformed escape must not panic — it just will not match a slug.
        assert!(matches!(resolve("/%zz"), Resolved::Single { .. }));
        assert!(matches!(resolve("/%"), Resolved::Single { .. }));
    }

    #[test]
    fn structure_values_round_trip() {
        for structure in PermalinkStructure::all() {
            assert_eq!(PermalinkStructure::parse(structure.as_str()), *structure);
        }
        assert_eq!(
            PermalinkStructure::parse("nonsense"),
            PermalinkStructure::PostName
        );
    }
}
