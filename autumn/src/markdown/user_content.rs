//! Safe rendering of **user-submitted** Markdown (issue #1255).
//!
//! The rest of this module ([`render`](super::render), [`MarkdownRegistry`](super::MarkdownRegistry))
//! is built for *trusted, build-time, file-based* content: pages you authored
//! and committed to the repository. This file is the other half — the path for
//! text a request body carried in, where the author is an attacker until proven
//! otherwise.
//!
//! # The guarantee
//!
//! [`render_user_content`] renders Markdown under two independent controls:
//!
//! 1. **Raw-HTML passthrough is disabled.** Every `Html`/`InlineHtml` event
//!    pulldown-cmark produces is rewritten to a `Text` event, so raw markup in
//!    the source is HTML-escaped rather than emitted. Link destinations are
//!    additionally checked against [`RICH_TEXT_ALLOWED_URL_SCHEMES`] *before*
//!    the HTML writer sees them, and a link with a rejected scheme is degraded
//!    to its own text. Image destinations are never rendered at all (see
//!    below), so they are dropped rather than scheme-checked.
//! 2. **The result is run through an allowlist sanitizer.** The HTML string is
//!    then passed through [`ammonia`], configured with the curated
//!    [`RICH_TEXT_ALLOWED_TAGS`] tag set, a per-tag attribute allowlist, and the
//!    same URL-scheme allowlist.
//!
//! Either control alone blocks the canonical payloads; both are applied so a
//! bypass of one is not a bypass of the feature. The regression corpus that
//! locks this down lives in `tests/integration/rich_text.rs`.
//!
//! # What the allowlist deliberately excludes
//!
//! - **`<img>`** — image embedding is out of scope for this field. A Markdown
//!   image degrades to its alt text, so a post cannot beacon a reader's IP to a
//!   third-party host.
//! - **`id`/`name` attributes** — user-controlled ids enable DOM clobbering
//!   (a heading called `Login` shadowing `document.getElementById("login")`), so
//!   unlike [`render`](super::render) this path injects no heading anchors.
//! - **`style` attributes**, except the `text-align` rule pulldown-cmark itself
//!   emits for aligned table cells.
//! - **`class` attributes**, except the `language-*` hint on fenced code blocks.
//! - **`<script>`, `<style>`, `<iframe>`, `<object>`, `<embed>`, `<form>`,
//!   `<input>`, `<svg>`, `<math>`, `<base>`, `<link>`, `<meta>`** and every
//!   event-handler (`on*`) attribute.

use std::sync::LazyLock;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// The HTML tags [`render_user_content`] may emit — formatting, links, lists,
/// code, and tables, per issue #1255. Anything else in the rendered output is
/// removed by the sanitizer.
///
/// This is a fixed, curated set: per-field allowlist configuration is
/// deliberately out of scope, so the guarantee is the same everywhere in every
/// app and can be reasoned about once.
#[rustfmt::skip]
pub const RICH_TEXT_ALLOWED_TAGS: &[&str] = &[
    // Block structure
    "p", "br", "hr", "blockquote",
    // Headings
    "h1", "h2", "h3", "h4", "h5", "h6",
    // Inline formatting. `sub`/`sup` have no CommonMark syntax and so are
    // unreachable from `render_user_content_html`; they are allowlisted for
    // `sanitize_user_html` callers whose source is already HTML.
    "em", "strong", "del", "sub", "sup",
    // Links
    "a",
    // Lists
    "ul", "ol", "li",
    // Code
    "code", "pre",
    // Tables
    "table", "thead", "tbody", "tr", "th", "td",
];

/// The URL schemes a link in user-submitted Markdown may use.
///
/// A destination whose scheme is absent from this list is rejected: the anchor
/// is dropped and its text kept. Relative (`/about`), fragment (`#anchor`), and
/// protocol-relative (`//host/path`) destinations carry no scheme and are
/// allowed.
pub const RICH_TEXT_ALLOWED_URL_SCHEMES: &[&str] = &["http", "https", "mailto", "tel"];

/// `rel` value forced onto every surviving link. `noopener`/`noreferrer` close
/// reverse-tabnabbing and referrer leakage; `nofollow` stops user-submitted
/// content from conferring search-engine reputation (comment-spam hardening).
const LINK_REL: &str = "noopener noreferrer nofollow";

/// The `text-align` values pulldown-cmark emits on aligned table cells. Any
/// other `style` declaration is dropped — this is the whole of the CSS surface
/// user content can reach.
const ALLOWED_TEXT_ALIGN: &[&str] = &["left", "right", "center"];

/// Render user-submitted Markdown to sanitized HTML.
///
/// See the [`markdown` module documentation](crate::markdown) for the exact
/// guarantee and allowlist.
/// Prefer `render_user_content` (the `maud` feature) when you are emitting into
/// a Maud template.
///
/// # Example
///
/// ```
/// use autumn_web::markdown::render_user_content_html;
///
/// let html = render_user_content_html("Hello **world** [x](javascript:alert(1))");
/// assert!(html.contains("<strong>world</strong>"));
/// assert!(!html.contains("javascript:"));
/// ```
#[must_use]
pub fn render_user_content_html(source: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    let events = Parser::new_ext(source, opts);
    let safe = SafeEvents::new(events);

    let mut html = String::with_capacity(source.len() + source.len() / 2);
    pulldown_cmark::html::push_html(&mut html, safe);

    sanitize_user_html(&html)
}

/// Render user-submitted Markdown to sanitized [`maud::Markup`].
///
/// This is the helper a handler or template calls; the returned `Markup` is
/// already escaped-safe, so it can be interpolated directly:
///
/// ```rust,ignore
/// html! {
///     article class="post-body" { (render_user_content(&post.body)) }
/// }
/// ```
///
/// # Example
///
/// ```
/// use autumn_web::markdown::render_user_content;
///
/// let markup = render_user_content("[x](javascript:alert(1))");
/// assert!(!markup.into_string().contains("javascript:"));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn render_user_content(source: &str) -> maud::Markup {
    maud::PreEscaped(render_user_content_html(source))
}

/// Run an HTML string through the same allowlist [`render_user_content_html`]
/// uses.
///
/// Use this when the untrusted rich text reaches you as HTML rather than
/// Markdown (a legacy column, an imported feed). Markdown input should go
/// through [`render_user_content_html`] instead, which additionally disables
/// raw-HTML passthrough at the parser.
///
/// # Example
///
/// ```
/// use autumn_web::markdown::sanitize_user_html;
///
/// let clean = sanitize_user_html("<p onclick=\"alert(1)\">hi</p><script>alert(1)</script>");
/// assert_eq!(clean, "<p>hi</p>");
/// ```
#[must_use]
pub fn sanitize_user_html(html: &str) -> String {
    SANITIZER.clean(html).to_string()
}

/// The configured [`ammonia`] cleaner. Built once — `ammonia::Builder` compiles
/// its allowlists into hash sets, so rebuilding per render would dominate the
/// cost of rendering a short comment.
static SANITIZER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(build_sanitizer);

fn build_sanitizer() -> ammonia::Builder<'static> {
    use std::collections::{HashMap, HashSet};

    let mut builder = ammonia::Builder::empty();

    builder
        .tags(RICH_TEXT_ALLOWED_TAGS.iter().copied().collect())
        .url_schemes(RICH_TEXT_ALLOWED_URL_SCHEMES.iter().copied().collect())
        // `link_rel` applies to every `<a>` ammonia keeps, including ones whose
        // `rel` the input tried to set itself.
        .link_rel(Some(LINK_REL));

    // Per-tag attribute allowlist. Everything not named here — every `on*`
    // handler, `id`, `name`, `srcdoc`, `formaction`, … — is dropped.
    let attrs: HashMap<&str, HashSet<&str>> = HashMap::from([
        // `rel` is deliberately absent from `a`: `link_rel` above makes ammonia
        // *set* it on every surviving `<a>`, and ammonia asserts the two are
        // not both configured (an input-supplied `rel` would otherwise win over
        // the forced hardening).
        ("a", HashSet::from(["href", "title"])),
        ("code", HashSet::from(["class"])),
        ("pre", HashSet::from(["class"])),
        ("th", HashSet::from(["style"])),
        ("td", HashSet::from(["style"])),
        ("ol", HashSet::from(["start"])),
    ]);
    builder.tag_attributes(attrs);
    builder.generic_attributes(HashSet::new());

    // The three attributes above that carry a *value* space wide enough to be
    // abused (`class` selectors, `style` declarations) are narrowed to exactly
    // the shapes the Markdown renderer itself produces.
    builder.attribute_filter(|element, attribute, value| match (element, attribute) {
        ("code" | "pre", "class") => {
            // Only the `language-{lang}` hint from a fenced code block, and only
            // when `{lang}` is a plain identifier — never an arbitrary class an
            // attacker could use to hook the host page's CSS or JS selectors.
            let is_language_hint = value.strip_prefix("language-").is_some_and(|lang| {
                !lang.is_empty()
                    && lang
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'))
            });
            is_language_hint.then(|| value.to_owned().into())
        }
        ("th" | "td", "style") => {
            // Only the `text-align` rule pulldown-cmark emits for an aligned
            // table column.
            let normalized = value.trim().trim_end_matches(';');
            let aligned = normalized.split_once(':').is_some_and(|(prop, val)| {
                prop.trim().eq_ignore_ascii_case("text-align")
                    && ALLOWED_TEXT_ALIGN.contains(&val.trim().to_ascii_lowercase().as_str())
            });
            aligned.then(|| value.to_owned().into())
        }
        _ => Some(value.into()),
    });

    builder
}

/// The deepest block nesting (blockquotes, lists, tables) [`render_user_content`]
/// will emit. Anything past this is flattened: the container tags are dropped
/// and their content kept.
///
/// This is a **denial-of-service bound**, not a style rule. The HTML sanitizer
/// walks its open-elements stack once per block start tag, so emitting `n`
/// nested containers costs O(n²) — and `"> "` is two source bytes per level, so
/// a single request body can ask for millions of levels. Capping the depth makes
/// the whole render linear in input size. Real prose never approaches 100 levels
/// of nesting; documents below the cap render byte-identically.
const MAX_BLOCK_NESTING_DEPTH: usize = 100;

/// Streaming adapter over pulldown-cmark events that removes every avenue for
/// attacker-controlled markup *before* the HTML writer runs:
///
/// - `Html` / `InlineHtml` events become `Text`, so raw markup is escaped.
/// - A link whose destination scheme is not in [`RICH_TEXT_ALLOWED_URL_SCHEMES`]
///   has its anchor dropped; the link text survives as plain text.
/// - Images are always dropped in favour of their alt text (image embedding is
///   out of scope for this field).
/// - Block nesting past [`MAX_BLOCK_NESTING_DEPTH`] is flattened, bounding the
///   sanitizer's per-start-tag stack walk (see that constant).
struct SafeEvents<'a, I: Iterator<Item = Event<'a>>> {
    inner: I,
    /// Depth of image tags currently open. Non-zero means we are inside an
    /// image's alt-text events, which we keep as plain text.
    image_depth: usize,
    /// Stack of open links, `true` when that link's anchor was dropped and its
    /// matching `End` must be dropped too.
    dropped_links: Vec<bool>,
    /// Stack of open block containers, `true` when that container was dropped
    /// for exceeding the depth cap and its matching `End` must be dropped too.
    dropped_blocks: Vec<bool>,
}

impl<'a, I: Iterator<Item = Event<'a>>> SafeEvents<'a, I> {
    const fn new(inner: I) -> Self {
        Self {
            inner,
            image_depth: 0,
            dropped_links: Vec::new(),
            dropped_blocks: Vec::new(),
        }
    }
}

/// Whether `tag` opens a block container that nests — i.e. one that grows the
/// HTML sanitizer's open-elements stack and so counts against
/// [`MAX_BLOCK_NESTING_DEPTH`].
const fn is_nesting_block(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::BlockQuote(_)
            | Tag::List(_)
            | Tag::Item
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::FootnoteDefinition(_)
    )
}

/// The [`TagEnd`] counterpart of [`is_nesting_block`]. Kept adjacent so the two
/// can never drift — a mismatch would desynchronize the `dropped_blocks` stack.
const fn is_nesting_block_end(tag: TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::BlockQuote(_)
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::FootnoteDefinition
    )
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for SafeEvents<'a, I> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        loop {
            let event = self.inner.next()?;
            match event {
                // Raw HTML never reaches the writer as markup.
                Event::Html(s) | Event::InlineHtml(s) => return Some(Event::Text(s)),
                Event::Start(Tag::Image { .. }) => {
                    self.image_depth += 1;
                }
                Event::End(TagEnd::Image) => {
                    self.image_depth = self.image_depth.saturating_sub(1);
                }
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => {
                    if url_scheme_allowed(&dest_url) {
                        self.dropped_links.push(false);
                        return Some(Event::Start(Tag::Link {
                            link_type,
                            dest_url,
                            title,
                            id,
                        }));
                    }
                    // Rejected scheme: drop the anchor and keep the text. The
                    // parser emits the link's visible text as its own `Text`
                    // event — including for an autolink, whose text is the
                    // destination — so suppressing just the `Start`/`End` pair
                    // preserves it exactly once.
                    self.dropped_links.push(true);
                }
                Event::End(TagEnd::Link) => {
                    if !self.dropped_links.pop().unwrap_or(false) {
                        return Some(Event::End(TagEnd::Link));
                    }
                }
                // Inside an image, code spans are alt text — flatten to text so
                // the alt survives without markup.
                Event::Code(s) if self.image_depth > 0 => return Some(Event::Text(s)),
                Event::Start(tag) if is_nesting_block(&tag) => {
                    // Past the cap, drop the container but keep descending —
                    // the content inside it still renders, just unwrapped.
                    let over_cap = self.dropped_blocks.len() >= MAX_BLOCK_NESTING_DEPTH;
                    self.dropped_blocks.push(over_cap);
                    if !over_cap {
                        return Some(Event::Start(tag));
                    }
                }
                Event::End(tag) if is_nesting_block_end(tag) => {
                    if !self.dropped_blocks.pop().unwrap_or(false) {
                        return Some(Event::End(tag));
                    }
                }
                other => return Some(other),
            }
        }
    }
}

/// Whether a link destination's scheme is in [`RICH_TEXT_ALLOWED_URL_SCHEMES`].
///
/// A destination with no scheme (relative, fragment, or protocol-relative) is
/// allowed. The scheme is read with the same tolerances a browser applies:
/// leading whitespace and embedded control characters (TAB/LF/CR/NUL) are
/// ignored, and the comparison is ASCII-case-insensitive — so `java\tscript:`
/// and `JaVaScRiPt:` are recognised as `javascript:` and rejected.
fn url_scheme_allowed(dest: &str) -> bool {
    url_scheme(dest).is_none_or(|scheme| RICH_TEXT_ALLOWED_URL_SCHEMES.contains(&scheme.as_str()))
}

/// Extract a URL's lowercased scheme, or `None` when it has none.
///
/// Mirrors the two normalisations the WHATWG URL parser applies before it reads
/// a scheme, and no more:
///
/// - leading (and trailing) C0 control characters and spaces are removed —
///   so `"\0javascript:…"` and `"  javascript:…"` are `javascript`;
/// - TAB, LF, and CR are removed from *anywhere* in the URL — so
///   `"java\tscript:…"` is `javascript`.
///
/// An interior *space* is deliberately **not** removed: a browser
/// percent-encodes it rather than closing up the string, so `"foo bar:baz"` is
/// a relative URL, not a `foo bar` scheme.
fn url_scheme(dest: &str) -> Option<String> {
    let trimmed = dest.trim_matches(|c: char| (c as u32) <= 0x20);
    let mut scheme = String::new();
    for c in trimmed.chars() {
        match c {
            '\t' | '\n' | '\r' => {}
            ':' => {
                return (!scheme.is_empty() && is_valid_scheme(&scheme)).then_some(scheme);
            }
            // A path/query/fragment separator ends any possible scheme: the
            // colon in `/a:b` or `#a:b` is data, not a scheme delimiter.
            '/' | '?' | '#' => return None,
            c => scheme.push(c.to_ascii_lowercase()),
        }
    }
    None
}

/// Whether `candidate` is syntactically a URL scheme (RFC 3986: an ASCII letter
/// followed by letters, digits, `+`, `-`, or `.`). A colon in `foo bar:baz`
/// does not introduce a scheme, so such a destination stays relative.
fn is_valid_scheme(candidate: &str) -> bool {
    let mut chars = candidate.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_extraction_handles_obfuscation() {
        assert_eq!(url_scheme("https://a"), Some("https".to_owned()));
        assert_eq!(url_scheme("JaVaScRiPt:x"), Some("javascript".to_owned()));
        assert_eq!(url_scheme("java\tscript:x"), Some("javascript".to_owned()));
        assert_eq!(url_scheme("  javascript:x"), Some("javascript".to_owned()));
        assert_eq!(
            url_scheme("\u{0}javascript:x"),
            Some("javascript".to_owned())
        );
        // Relative, fragment, and protocol-relative destinations have no scheme.
        assert_eq!(url_scheme("/a/b"), None);
        assert_eq!(url_scheme("#anchor"), None);
        assert_eq!(url_scheme("//host/path"), None);
        assert_eq!(url_scheme("a/b:c"), None);
        // A colon after a non-scheme-shaped prefix is data, not a delimiter.
        assert_eq!(url_scheme("foo bar:baz"), None);
        assert_eq!(url_scheme("1abc:x"), None);
    }

    #[test]
    fn scheme_allowlist_accepts_only_curated_schemes() {
        assert!(url_scheme_allowed("https://example.com"));
        assert!(url_scheme_allowed("http://example.com"));
        assert!(url_scheme_allowed("mailto:a@b.example"));
        assert!(url_scheme_allowed("tel:+15551234"));
        assert!(url_scheme_allowed("/relative"));
        assert!(!url_scheme_allowed("javascript:alert(1)"));
        assert!(!url_scheme_allowed("vbscript:x"));
        assert!(!url_scheme_allowed("data:text/html,x"));
        assert!(!url_scheme_allowed("file:///etc/passwd"));
    }

    #[test]
    fn table_alignment_style_survives_but_other_css_does_not() {
        let aligned =
            sanitize_user_html("<table><tr><td style=\"text-align: right\">1</td></tr></table>");
        assert!(aligned.contains("style=\"text-align: right\""), "{aligned}");
        let injected =
            sanitize_user_html("<table><tr><td style=\"position:fixed;top:0\">1</td></tr></table>");
        assert!(!injected.contains("position"), "{injected}");
        // A `text-align` declaration smuggling a second rule alongside it is
        // rejected wholesale rather than partially kept.
        let smuggled = sanitize_user_html(
            "<table><tr><td style=\"text-align:left;position:fixed\">1</td></tr></table>",
        );
        assert!(!smuggled.contains("position"), "{smuggled}");
    }

    #[test]
    fn allowed_tag_and_scheme_lists_have_no_duplicates() {
        let mut tags = RICH_TEXT_ALLOWED_TAGS.to_vec();
        tags.sort_unstable();
        let len = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), len, "duplicate entry in RICH_TEXT_ALLOWED_TAGS");
    }
}
