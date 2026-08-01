//! Markdown-to-HTML renderer with heading ID injection and TOC extraction.

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::markdown::types::{RenderOptions, RenderedMarkdown, TocItem};

/// Render Markdown body text to HTML, injecting stable `id` attributes on
/// every heading and returning an ordered table of contents.
///
/// Fenced code blocks preserve their language hint as a `language-{lang}`
/// CSS class. The output HTML is safe: it is produced by pulldown-cmark's
/// built-in HTML writer, which escapes raw HTML by default.
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

    let mut i = 0;
    while i < raw.len() {
        match &raw[i] {
            Event::Start(Tag::Heading { level, .. }) => {
                let level_u8 = heading_level_to_u8(*level);
                // Look ahead to collect the heading's plain-text content.
                let mut text = String::with_capacity(128);
                let mut j = i + 1;
                while j < raw.len() {
                    match &raw[j] {
                        Event::Text(t) | Event::Code(t) => text.push_str(t),
                        // Preserve word boundaries across soft/hard line breaks.
                        Event::SoftBreak | Event::HardBreak => text.push(' '),
                        Event::End(TagEnd::Heading(_)) => break,
                        _ => {}
                    }
                    j += 1;
                }
                let id = heading_id(&text);
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
    fn escapes_raw_html() {
        let md = "<script>alert('xss')</script>\n\nAn <img src=x onerror=alert(1)> image.";
        let result = render(md, RenderOptions::default());
        assert!(!result.html.contains("<script>"));
        assert!(!result.html.contains("<img"));
        assert!(result.html.contains("&lt;script&gt;"));
        assert!(result.html.contains("&lt;img"));
    }
}
