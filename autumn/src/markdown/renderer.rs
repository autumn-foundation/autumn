//! Markdown-to-HTML renderer with heading ID injection and TOC extraction.

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::markdown::types::{RenderOptions, RenderedMarkdown, TocItem};

/// Render Markdown body text to HTML, injecting stable `id` attributes on
/// every heading and returning an ordered table of contents.
///
/// Anchors are unique within a document, so the HTML stays valid and every TOC
/// entry links to its own heading. Each heading keeps the slug its own text
/// produces; only *repeats* of an already-claimed slug are suffixed with `-1`,
/// `-2`, … A suffix never takes a slug that another heading owns by name, so
/// `## Example` / `## Example` / `## Example 1` renders `example`, `example-2`,
/// `example-1` — `#example-1` still points at "Example 1" either way round.
///
/// Fenced code blocks preserve their language hint as a `language-{lang}`
/// CSS class. Raw HTML in the source is escaped rather than emitted — this
/// function rewrites pulldown-cmark's `Html`/`InlineHtml` events to text before
/// the writer runs (pulldown's own writer would pass them through verbatim).
///
/// That is **not** the same as sanitizing, and this renderer is not a
/// sanitizer — see below.
///
/// # Not for user-submitted text
///
/// This helper targets *trusted, build-time* content — pages you authored and
/// committed. It applies **no** URL-scheme allowlist (a `[x](javascript:…)`
/// link renders as written) and injects heading `id` anchors from the document
/// text, which user-controlled headings could use for DOM clobbering.
///
/// For anything a request body carried in — posts, comments, wiki bodies —
/// use [`render_user_content_html`](crate::markdown::render_user_content_html)
/// (or `render_user_content` for `maud::Markup`) instead, which disables
/// raw-HTML passthrough and runs the output through an allowlist sanitizer.
///
/// # Example
///
/// ```
/// use autumn_web::markdown::{RenderOptions, render};
///
/// let out = render("# Hello\n\nWorld.", RenderOptions::default());
/// assert!(out.html.contains(r#"id="hello""#));
/// assert_eq!(out.toc[0].text, "Hello");
/// ```
#[must_use]
pub fn render(body: &str, options: RenderOptions) -> RenderedMarkdown {
    let mut pulldown_opts = Options::empty();
    if options.enable_tables {
        pulldown_opts.insert(Options::ENABLE_TABLES);
    }
    if options.enable_strikethrough {
        pulldown_opts.insert(Options::ENABLE_STRIKETHROUGH);
    }
    if options.enable_tasklists {
        pulldown_opts.insert(Options::ENABLE_TASKLISTS);
    }

    let parser = Parser::new_ext(body, pulldown_opts);
    let raw: Vec<Event<'_>> = parser.collect();

    let mut toc: Vec<TocItem> = Vec::new();
    let mut output: Vec<Event<'_>> = Vec::with_capacity(raw.len());
    let mut anchors = AnchorAllocator::with_reserved(&raw);

    let mut i = 0;
    while i < raw.len() {
        match &raw[i] {
            Event::Start(Tag::Heading { level, .. }) => {
                let level_u8 = heading_level_to_u8(*level);
                let text = heading_text(&raw, i);
                let id = anchors.allocate(&heading_id(&text));
                // Only inject an id and add a TOC entry when the heading has
                // at least one alphanumeric character.  An empty id (e.g. a
                // punctuation-only heading like `# !!!`) would produce invalid
                // markup `<h1 id="">` and a broken TOC link pointing to `#`.
                if id.is_empty() {
                    // No alphanumeric text — plain heading, no id, no TOC entry.
                    // An empty id would produce invalid `<h1 id="">` markup and
                    // a broken TOC link pointing to bare `#`.
                    output.push(Event::Html(CowStr::from(format!("<h{level_u8}>"))));
                } else {
                    toc.push(TocItem {
                        level: level_u8,
                        id: id.clone(),
                        text,
                    });
                    output.push(Event::Html(CowStr::from(format!(
                        "<h{level_u8} id=\"{id}\">"
                    ))));
                }
                i += 1;
            }
            Event::End(TagEnd::Heading(level)) => {
                let level_u8 = heading_level_to_u8(*level);
                output.push(Event::Html(CowStr::from(format!("</h{level_u8}>"))));
                i += 1;
            }
            _ => {
                match &raw[i] {
                    Event::Html(s) | Event::InlineHtml(s) => {
                        output.push(Event::Text(s.clone()));
                    }
                    other => {
                        output.push(other.clone());
                    }
                }
                i += 1;
            }
        }
    }

    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, output.into_iter());

    RenderedMarkdown { html, toc }
}

/// Collect a heading's plain-text content, given the index of its
/// `Event::Start(Tag::Heading { .. })` in `events`.
fn heading_text(events: &[Event<'_>], start: usize) -> String {
    let mut text = String::with_capacity(128);
    for event in &events[start + 1..] {
        match event {
            Event::Text(t) | Event::Code(t) => text.push_str(t),
            // Preserve word boundaries across soft/hard line breaks.
            Event::SoftBreak | Event::HardBreak => text.push(' '),
            Event::End(TagEnd::Heading(_)) => break,
            _ => {}
        }
    }
    text
}

/// Hands out document-unique heading anchors.
///
/// A document may legitimately repeat a heading ("Example", "Usage", "Notes"),
/// but [`heading_id`] is a pure function of the heading text, so every
/// repetition slugifies to the same string. Emitting that string twice
/// produces duplicate `id` attributes — invalid HTML — and makes every TOC
/// link for the repeated heading jump to the first occurrence.
///
/// Every heading keeps the slug its own text produces; only *repeats* of an
/// already-claimed slug are suffixed with `-1`, `-2`, … (the convention
/// GitHub, mdBook, and Hugo all use). To make that hold regardless of heading
/// order, the allocator is seeded with every heading's natural slug up front
/// and never hands one out as a suffix. A document containing `## Example`,
/// `## Example`, `## Example 1` therefore yields `example`, `example-2`,
/// `example-1` — the second `## Example` skips past `example-1` because
/// `## Example 1` owns it by name, whether it appears before or after.
///
/// Without that reservation the suffix search would be first-come: the second
/// `## Example` would take `example-1`, and a `#example-1` link already
/// published against `## Example 1` would silently resolve to a different
/// heading — worse for a docs site than a dead link.
struct AnchorAllocator {
    /// Slugs handed out so far.
    used: std::collections::HashSet<String>,
    /// Every slug some heading in the document produces from its own text.
    /// Never used as a collision suffix, so each such heading can claim it.
    reserved: std::collections::HashSet<String>,
    /// Highest suffix tried per base slug, so a heading repeated `n` times
    /// costs O(n) probes in total rather than O(n²).
    next_suffix: std::collections::HashMap<String, usize>,
}

impl AnchorAllocator {
    /// Seed the allocator with the natural slug of every heading in `events`.
    fn with_reserved(events: &[Event<'_>]) -> Self {
        let mut reserved = std::collections::HashSet::new();
        for (i, event) in events.iter().enumerate() {
            if matches!(event, Event::Start(Tag::Heading { .. })) {
                let slug = heading_id(&heading_text(events, i));
                if !slug.is_empty() {
                    reserved.insert(slug);
                }
            }
        }
        Self {
            used: std::collections::HashSet::new(),
            reserved,
            next_suffix: std::collections::HashMap::new(),
        }
    }

    /// Claim and return a unique anchor derived from `base`.
    ///
    /// An empty `base` (a heading with no alphanumeric characters) is returned
    /// as-is and never claimed — the caller emits no `id` at all for it, so
    /// such headings must not consume or collide in the anchor namespace.
    fn allocate(&mut self, base: &str) -> String {
        if base.is_empty() {
            return String::new();
        }
        if self.used.insert(base.to_owned()) {
            return base.to_owned();
        }
        // Resume from the highest suffix already tried for this base rather
        // than rescanning from 1, then write it back below. Taken by value so
        // the loop can borrow `self.used` mutably.
        let mut counter = self.next_suffix.get(base).copied().unwrap_or(0);
        let id = loop {
            counter += 1;
            let candidate = format!("{base}-{counter}");
            // Skip slugs some other heading owns by name, then skip anything
            // already handed out.
            if !self.reserved.contains(&candidate) && self.used.insert(candidate.clone()) {
                break candidate;
            }
        };
        self.next_suffix.insert(base.to_owned(), counter);
        id
    }
}

const fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Derive a stable, URL-safe anchor ID from heading text.
///
/// Splits on non-alphanumeric characters (Unicode-aware), lowercases the
/// remaining parts, filters empty parts, and joins with `-`.  Non-ASCII
/// scripts (e.g. German umlauts, CJK characters) are preserved so that
/// anchors remain meaningful for non-English content.
///
/// This is a pure function of the heading text, so two headings that read the
/// same produce the same ID. [`render`] deduplicates within a document by
/// suffixing later collisions; call this directly only when you need the raw
/// slug for one piece of text.
///
/// Text with no alphanumeric characters (e.g. `"!!!"`) yields an empty string.
/// [`render`] emits no `id` attribute at all in that case rather than an
/// invalid `id=""`.
///
/// # Examples
///
/// ```
/// # use autumn_web::markdown::heading_id;
/// assert_eq!(heading_id("Hello, World!"), "hello-world");
/// assert_eq!(heading_id("Getting Started"), "getting-started");
/// assert_eq!(heading_id("Über uns"), "über-uns");
/// ```
#[must_use]
pub fn heading_id(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_simple_paragraph() {
        let result = render("Hello **world**!", RenderOptions::default());
        assert!(result.html.contains("<strong>world</strong>"));
        assert!(result.toc.is_empty());
    }

    #[test]
    fn generates_stable_heading_ids() {
        let result = render("# Hello World\n\nSome text.", RenderOptions::default());
        assert!(result.html.contains(r#"id="hello-world""#));
        assert_eq!(result.toc.len(), 1);
        assert_eq!(result.toc[0].id, "hello-world");
        assert_eq!(result.toc[0].text, "Hello World");
        assert_eq!(result.toc[0].level, 1);
    }

    #[test]
    fn extracts_ordered_toc() {
        let md = "# Title\n\n## Section 1\n\nText.\n\n### Subsection\n\n## Section 2\n";
        let result = render(md, RenderOptions::default());
        assert_eq!(result.toc.len(), 4);
        assert_eq!(result.toc[0].level, 1);
        assert_eq!(result.toc[0].id, "title");
        assert_eq!(result.toc[1].level, 2);
        assert_eq!(result.toc[1].id, "section-1");
        assert_eq!(result.toc[2].level, 3);
        assert_eq!(result.toc[2].id, "subsection");
        assert_eq!(result.toc[3].level, 2);
        assert_eq!(result.toc[3].id, "section-2");
    }

    #[test]
    fn preserves_fenced_code_language() {
        let md = "```rust\nfn main() {}\n```";
        let result = render(md, RenderOptions::default());
        assert!(result.html.contains("language-rust"));
    }

    #[test]
    fn renders_tables_when_enabled() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let result = render(
            md,
            RenderOptions {
                enable_tables: true,
                ..Default::default()
            },
        );
        assert!(result.html.contains("<table>"));
    }

    #[test]
    fn suppresses_tables_when_disabled() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let result = render(
            md,
            RenderOptions {
                enable_tables: false,
                ..Default::default()
            },
        );
        assert!(!result.html.contains("<table>"));
    }

    #[test]
    fn renders_strikethrough_when_enabled() {
        let result = render(
            "~~strike~~",
            RenderOptions {
                enable_strikethrough: true,
                ..Default::default()
            },
        );
        assert!(result.html.contains("<del>"));
    }

    #[test]
    fn suppresses_strikethrough_when_disabled() {
        let result = render(
            "~~strike~~",
            RenderOptions {
                enable_strikethrough: false,
                ..Default::default()
            },
        );
        assert!(!result.html.contains("<del>"));
    }

    #[test]
    fn empty_body_renders_empty_html() {
        let result = render("", RenderOptions::default());
        assert_eq!(result.html.trim(), "");
        assert!(result.toc.is_empty());
    }

    #[test]
    fn heading_id_strips_special_chars() {
        assert_eq!(heading_id("Hello, World!"), "hello-world");
        assert_eq!(heading_id("Getting Started"), "getting-started");
        assert_eq!(heading_id("  Leading Spaces  "), "leading-spaces");
    }

    #[test]
    fn heading_id_unique_for_different_texts() {
        assert_ne!(heading_id("Section 1"), heading_id("Section 2"));
    }

    #[test]
    fn heading_id_level6() {
        let result = render("###### Deep\n", RenderOptions::default());
        assert!(result.html.contains(r#"<h6 id="deep">"#));
        assert_eq!(result.toc[0].level, 6);
    }

    #[test]
    fn multiple_headings_all_in_toc() {
        let md = "# One\n## Two\n### Three\n";
        let result = render(md, RenderOptions::default());
        assert_eq!(result.toc.len(), 3);
        assert!(result.html.contains(r#"id="one""#));
        assert!(result.html.contains(r#"id="two""#));
        assert!(result.html.contains(r#"id="three""#));
    }

    #[test]
    fn heading_id_apostrophe_handled() {
        // apostrophe becomes a separator, collapsed
        assert_eq!(heading_id("What's New"), "what-s-new");
    }

    #[test]
    fn heading_id_all_special_chars() {
        assert_eq!(heading_id("!!!"), "");
    }

    #[test]
    fn heading_id_unicode_preserved() {
        // Non-ASCII alphanumerics are kept and lowercased.
        assert_eq!(heading_id("Über uns"), "über-uns");
        assert_eq!(heading_id("日本語"), "日本語");
    }

    #[test]
    fn soft_break_in_heading_preserved_as_space() {
        // Setext headings may span multiple lines; the SoftBreak between lines
        // must produce a space so adjacent words are not merged in the TOC text
        // and the generated anchor ID.
        let md = "Hello\nWorld\n=====\n";
        let result = render(md, RenderOptions::default());
        assert_eq!(result.toc[0].text, "Hello World");
        assert!(result.html.contains(r#"id="hello-world""#));
    }

    #[test]
    fn hard_break_in_heading_preserved_as_space() {
        // A backslash hard-break inside a setext heading must not merge words.
        let md = "Hello\\\nWorld\n=====\n";
        let result = render(md, RenderOptions::default());
        assert_eq!(result.toc[0].text, "Hello World");
        assert!(result.html.contains(r#"id="hello-world""#));
    }

    #[test]
    fn punctuation_only_heading_emits_no_id_and_no_toc_entry() {
        // heading_id("!!!") returns ""; the renderer must not emit id=""
        // or add a broken TOC entry pointing to "#".
        let result = render("# !!!\n\nText.", RenderOptions::default());
        assert!(!result.html.contains("id="));
        assert!(result.toc.is_empty());
        // The heading tag itself must still be present.
        assert!(result.html.contains("<h1>"));
    }

    #[test]
    fn duplicate_headings_get_unique_ids() {
        // Real docs repeat headings ("Example", "Usage", "Notes"). Emitting the
        // same `id` twice is invalid HTML and makes every TOC link for the
        // repeated heading jump to the first occurrence.
        let md = "## Example\n\nFirst.\n\n## Example\n\nSecond.\n\n## Example\n\nThird.\n";
        let result = render(md, RenderOptions::default());
        let ids: Vec<&str> = result.toc.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["example", "example-1", "example-2"]);
        assert!(result.html.contains(r#"<h2 id="example">"#));
        assert!(result.html.contains(r#"<h2 id="example-1">"#));
        assert!(result.html.contains(r#"<h2 id="example-2">"#));
    }

    #[test]
    fn anchors_do_not_leak_between_documents() {
        // The allocator must be per-`render` call. If its state were ever
        // shared (a `static`/`thread_local`, or a future caching refactor),
        // the same document would render different anchors depending on what
        // was rendered before it — silently breaking every published deep link
        // on a multi-page docs build.
        let md = "## Example\n\n## Example\n";
        let first = render(md, RenderOptions::default());
        let second = render(md, RenderOptions::default());
        let ids_a: Vec<&str> = first.toc.iter().map(|t| t.id.as_str()).collect();
        let ids_b: Vec<&str> = second.toc.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids_a, ids_b, "anchor state leaked across render() calls");
        assert_eq!(ids_a, vec!["example", "example-1"]);
    }

    #[test]
    fn first_occurrence_keeps_unsuffixed_id() {
        // Heading IDs are URL-visible. Deduplication must only ever *add* a
        // suffix to later duplicates so existing deep links keep resolving.
        let md = "# Intro\n\n## Setup\n\n## Setup\n";
        let result = render(md, RenderOptions::default());
        assert_eq!(result.toc[0].id, "intro");
        assert_eq!(result.toc[1].id, "setup");
        assert_eq!(result.toc[2].id, "setup-1");
    }

    #[test]
    fn dedup_suffix_skips_ids_already_taken_by_another_heading() {
        // "Example 1" naturally slugifies to "example-1", which is also the
        // suffix a second "Example" would want. The dedup counter must skip
        // past ids that are already in use rather than collide again.
        let md = "## Example\n\n## Example 1\n\n## Example\n";
        let result = render(md, RenderOptions::default());
        let ids: Vec<&str> = result.toc.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["example", "example-1", "example-2"]);
    }

    #[test]
    fn duplicate_never_steals_another_headings_natural_slug() {
        // "Example 1" slugifies naturally to "example-1". A second "Example"
        // must not take that id just because it appears first — a published
        // `#example-1` link would then silently resolve to the wrong heading,
        // which is worse for a docs site than a dead link.
        let md = "## Example\n\n## Example\n\n## Example 1\n";
        let result = render(md, RenderOptions::default());
        let ids: Vec<&str> = result.toc.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["example", "example-2", "example-1"]);
        // The heading that owns "example-1" by its own text keeps it.
        assert_eq!(result.toc[2].text, "Example 1");
    }

    #[test]
    fn suffixes_never_nest() {
        // A base slug that is itself a suffixed form must not produce `x-1-1`.
        let md = "# x\n# x\n# x\n# x-1\n# x-2\n# x\n";
        let result = render(md, RenderOptions::default());
        let ids: Vec<&str> = result.toc.iter().map(|t| t.id.as_str()).collect();
        for id in &ids {
            assert!(
                !id.contains("-1-"),
                "nested suffix in {id:?} (all: {ids:?})"
            );
        }
        // Headings whose own text yields "x-1"/"x-2" keep those ids.
        assert_eq!(ids[3], "x-1");
        assert_eq!(ids[4], "x-2");
    }

    #[test]
    fn dedup_is_case_insensitive_via_slug() {
        // "Setup" and "SETUP" slugify to the same id, so the second must be
        // suffixed.
        let md = "## Setup\n\n## SETUP\n";
        let result = render(md, RenderOptions::default());
        assert_eq!(result.toc[0].id, "setup");
        assert_eq!(result.toc[1].id, "setup-1");
    }

    #[test]
    fn duplicate_punctuation_only_headings_stay_id_free() {
        // Empty slugs are not ids at all, so they must not participate in
        // dedup (no `id="-1"` nonsense) and must stay out of the TOC.
        let result = render("# !!!\n\n# ???\n", RenderOptions::default());
        assert!(!result.html.contains("id="));
        assert!(result.toc.is_empty());
    }

    #[test]
    fn toc_ids_match_emitted_heading_ids() {
        // The TOC is only useful if every entry's id resolves to exactly one
        // heading. A bare `contains` check would pass vacuously when several
        // headings share an id, so assert each id appears *once* and that the
        // document emits no ids beyond the ones the TOC lists.
        let md = "# Guide\n## Example\n### Example\n## Example\n";
        let result = render(md, RenderOptions::default());

        let ids: Vec<&str> = result.toc.iter().map(|t| t.id.as_str()).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "TOC ids must be distinct: {ids:?}");

        for id in &ids {
            let needle = format!(r#" id="{id}">"#);
            assert_eq!(
                result.html.matches(&needle).count(),
                1,
                "TOC id {id:?} must match exactly one heading in:\n{}",
                result.html
            );
        }
        assert_eq!(
            result.html.matches(r#" id=""#).count(),
            ids.len(),
            "every emitted id must be represented in the TOC:\n{}",
            result.html
        );
    }

    #[test]
    fn escapes_raw_html() {
        let md = "<script>alert('xss')</script>\n\nAn <img src=x onerror=alert(1)> image.";
        let result = render(md, RenderOptions::default());
        assert!(!result.html.contains("<script>"));
        assert!(!result.html.contains("<img"));
        assert!(result.html.contains("&lt;script&gt;"));
        assert!(result.html.contains("&lt;img"));
    }
}
