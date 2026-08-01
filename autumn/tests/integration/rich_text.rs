//! Safe rich-text rendering for **user-submitted** Markdown (issue #1255).
//!
//! These tests are the regression lock on the sanitization guarantee documented
//! in `docs/guide/rich-text.md`: `markdown::render_user_content` must render
//! user Markdown with raw-HTML passthrough disabled and the result run through
//! the curated allowlist, so a content app built on autumn cannot ship stored
//! XSS by using the shipped helper.
//!
//! The payload corpus below is deliberately adversarial. Widening
//! `RICH_TEXT_ALLOWED_TAGS` / `RICH_TEXT_ALLOWED_URL_SCHEMES` without also
//! re-deriving these expectations should fail loudly here.

use autumn_web::markdown::{
    RICH_TEXT_ALLOWED_TAGS, RICH_TEXT_ALLOWED_URL_SCHEMES, render_user_content,
    render_user_content_html, sanitize_user_html,
};

/// Tag-open markers that must never appear *as live markup* in the rendered
/// output. A payload that survives as escaped text (`&lt;script&gt;`) is inert
/// and does not match these, which is exactly the distinction we want.
const FORBIDDEN_TAGS: &[&str] = &[
    "<script",
    "</script",
    "<iframe",
    "<object",
    "<embed",
    "<style",
    "<form",
    "<input",
    "<svg",
    "<math",
    "<base",
    "<link",
    "<meta",
    "<img",
    "<noscript",
    "<template",
    "<textarea",
];

/// Attribute names that must never survive on any element. Checked against
/// parsed attribute *names* (see [`parse_tag`]), never raw substrings, so an
/// escaped `&quot; onmouseover=&quot;` sitting inertly inside a `title` value
/// is correctly treated as harmless text.
const FORBIDDEN_ATTRS: &[&str] = &[
    "srcdoc",
    "formaction",
    // User-controlled ids/names enable DOM clobbering.
    "id",
    "name",
    "http-equiv",
];

/// Adversarial Markdown/HTML payloads a hostile user could submit.
const XSS_CORPUS: &[&str] = &[
    // The AC's named payload.
    "[x](javascript:alert(1))",
    // Case obfuscation.
    "[x](JaVaScRiPt:alert(1))",
    // Entity obfuscation — CommonMark decodes entity refs in link destinations.
    "[x](&#106;avascript:alert(1))",
    "[x](javascript&colon;alert(1))",
    // Control-character obfuscation (browsers strip TAB/LF/CR inside URLs).
    "[x](java\tscript:alert(1))",
    "[x](java\nscript:alert(1))",
    // Leading whitespace / NUL padding.
    "[x](   javascript:alert(1))",
    "[x](\u{0}javascript:alert(1))",
    // Other executable schemes.
    "[x](vbscript:msgbox(1))",
    "[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)",
    // Image destinations take the same path as links.
    "![x](javascript:alert(1))",
    "![x](data:text/html;base64,PHNjcmlwdD4=)",
    // Reference-style links resolve to the same destination.
    "[x][ref]\n\n[ref]: javascript:alert(1)",
    // Autolinks.
    "<javascript:alert(1)>",
    // Raw HTML blocks and inline raw HTML.
    "<script>alert(1)</script>",
    "<SCRIPT>alert(1)</SCRIPT>",
    "<img src=x onerror=alert(1)>",
    "<div onclick=\"alert(1)\">hi</div>",
    "<a href=\"#\" onmouseover=\"alert(1)\">x</a>",
    "<style>body{background:url(javascript:alert(1))}</style>",
    "<iframe src=\"https://evil.example\"></iframe>",
    "<iframe srcdoc=\"&lt;script&gt;alert(1)&lt;/script&gt;\"></iframe>",
    "<object data=\"evil.swf\"></object>",
    "<embed src=\"evil.swf\">",
    "<form action=\"https://evil.example\"><input type=\"submit\" formaction=\"javascript:alert(1)\"></form>",
    "<base href=\"https://evil.example/\">",
    "<link rel=\"stylesheet\" href=\"https://evil.example/x.css\">",
    "<meta http-equiv=\"refresh\" content=\"0;url=https://evil.example\">",
    "<svg onload=\"alert(1)\"></svg>",
    "<svg><animate onbegin=alert(1) attributeName=x dur=1s></svg>",
    // mXSS-shaped namespace confusion.
    "<math><mtext><table><mglyph><style><!--</style><img src=x onerror=alert(1)>",
    "<noscript><p title=\"</noscript><img src=x onerror=alert(1)>\">",
    // Comment-smuggled markup.
    "<!--<img src=x onerror=alert(1)>-->",
    // Backslash-escaped scheme.
    "[x](\\6a avascript:alert(1))",
    // Inside a fenced code block the payload must stay inert text.
    "```html\n<script>alert(1)</script>\n```",
    // Inside inline code.
    "`<img src=x onerror=alert(1)>`",
    // Inside a blockquote / list, where a naive filter might not descend.
    "> [x](javascript:alert(1))",
    "- [x](javascript:alert(1))",
    // Inside a table cell.
    "| a |\n|---|\n| [x](javascript:alert(1)) |",
    // Title attribute injection attempt.
    "[x](https://ok.example \"a\\\" onmouseover=\\\"alert(1)\")",
];

/// Split `html` into its live tag regions (`<…>`).
///
/// The rendered output never contains a raw `<` in text — the HTML writer
/// escapes it to `&lt;` — so every `<…>` span really is a tag. Restricting the
/// attribute assertions to these spans is what lets the corpus distinguish a
/// *neutralised* payload (`&lt;img src=x onerror=…&gt;`, inert text that happens
/// to spell `onerror`) from a live one.
fn tag_regions(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        rest = &rest[start..];
        let end = rest.find('>').map_or(rest.len(), |i| i + 1);
        out.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    out
}

/// Split one lowercased tag region into its element name and `(name, value)`
/// attribute pairs.
///
/// Deliberately a *strict* reader rather than a permissive one: ammonia always
/// emits `name="value"` with `"`/`&`/`<`/`>` escaped inside the value, so
/// respecting the quotes is enough to tell a real attribute from characters
/// that merely look like one inside a value.
fn parse_tag(region: &str) -> (String, Vec<(String, String)>) {
    let body = region
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end_matches('/');
    let body = body.trim_start_matches('/');
    let mut chars = body.chars().peekable();

    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
            chars.next();
        } else {
            break;
        }
    }

    let mut attrs = Vec::new();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut attr = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c.is_whitespace() {
                break;
            }
            attr.push(c);
            chars.next();
        }
        if attr.is_empty() {
            break;
        }
        let mut value = String::new();
        if chars.peek() == Some(&'=') {
            chars.next();
            match chars.peek().copied() {
                Some(quote @ ('"' | '\'')) => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == quote {
                            break;
                        }
                        value.push(c);
                    }
                }
                _ => {
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() {
                            break;
                        }
                        value.push(c);
                        chars.next();
                    }
                }
            }
        }
        attrs.push((attr, value));
    }
    (name, attrs)
}

/// Assert that `html` — the render of `payload` — contains no executable
/// markup: no live dangerous tag, no non-allowlisted element, no event-handler
/// or clobbering attribute, and no URL attribute outside
/// `RICH_TEXT_ALLOWED_URL_SCHEMES`.
fn assert_inert(payload: &str, html: &str) {
    let lowered = html.to_ascii_lowercase();
    for tag in FORBIDDEN_TAGS {
        assert!(
            !lowered.contains(tag),
            "payload {payload:?} emitted live {tag:?}:\n{html}"
        );
    }
    for region in tag_regions(&lowered) {
        let (name, attrs) = parse_tag(&region);
        // Nothing outside the curated tag set may survive as live markup.
        assert!(
            name.is_empty() || RICH_TEXT_ALLOWED_TAGS.contains(&name.as_str()),
            "payload {payload:?} emitted non-allowlisted tag {name:?}:\n{html}"
        );
        for (attr, value) in attrs {
            assert!(
                !attr.starts_with("on"),
                "payload {payload:?} kept event handler {attr:?} on <{name}>:\n{html}"
            );
            assert!(
                !FORBIDDEN_ATTRS.contains(&attr.as_str()),
                "payload {payload:?} kept attribute {attr:?} on <{name}>:\n{html}"
            );
            if !matches!(attr.as_str(), "href" | "src") {
                continue;
            }
            // Strip the characters a browser ignores inside a URL before
            // reading the scheme, so an obfuscated `java\tscript:` can't slip
            // past this check either.
            let squeezed: String = value
                .chars()
                .filter(|c| (*c as u32) > 0x20 && *c as u32 != 0x7F)
                .collect();
            let Some((scheme, _)) = squeezed.split_once(':') else {
                continue;
            };
            let looks_like_scheme = scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
            assert!(
                !looks_like_scheme || RICH_TEXT_ALLOWED_URL_SCHEMES.contains(&scheme),
                "payload {payload:?} kept a {scheme:?} URL ({value:?}):\n{html}"
            );
        }
    }
}

#[test]
fn xss_corpus_never_emits_executable_markup() {
    for payload in XSS_CORPUS {
        assert_inert(payload, &render_user_content_html(payload));
    }
}

#[test]
fn xss_corpus_is_inert_through_the_html_sanitizer_too() {
    // The second control on its own: feeding each payload straight to the
    // allowlist sanitizer (as `sanitize_user_html` callers with an HTML source
    // do) must also produce inert output.
    for payload in XSS_CORPUS {
        assert_inert(payload, &sanitize_user_html(payload));
    }
}

#[test]
fn ac_named_javascript_link_payload_emits_no_href() {
    // The acceptance criterion names this payload explicitly: the rendered
    // output must carry no `javascript:` URL at all.
    let html = render_user_content_html("[x](javascript:alert(1))");
    assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
    // The link text survives as plain text — we drop the anchor, not the words.
    assert!(html.contains('x'), "{html}");
}

#[test]
fn script_tag_is_escaped_not_executed() {
    let html = render_user_content_html("<script>alert('xss')</script>");
    assert!(!html.contains("<script"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
}

#[test]
fn safe_markdown_still_renders() {
    let html = render_user_content_html(
        "# Title\n\nSome **bold** and _italic_ and `code`.\n\n- one\n- two\n\n\
         1. first\n2. second\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n\
         [link](https://example.com) and [rel](/local/path) and [mail](mailto:a@b.example)\n\n\
         | a | b |\n|---|---|\n| 1 | 2 |\n",
    );
    assert!(html.contains("<h1>"), "{html}");
    assert!(html.contains("<strong>bold</strong>"), "{html}");
    assert!(html.contains("<em>italic</em>"), "{html}");
    assert!(html.contains("<code>code</code>"), "{html}");
    assert!(html.contains("<ul>"), "{html}");
    assert!(html.contains("<ol>"), "{html}");
    assert!(html.contains("<blockquote>"), "{html}");
    assert!(html.contains("<pre>"), "{html}");
    assert!(html.contains("<table>"), "{html}");
    assert!(html.contains("https://example.com"), "{html}");
    assert!(html.contains("/local/path"), "{html}");
    assert!(html.contains("mailto:a@b.example"), "{html}");
}

#[test]
fn headings_carry_no_id_attribute() {
    // User-controlled `id` attributes enable DOM clobbering (a user heading
    // named "Login" shadowing `document.getElementById("login")`), so the
    // user-content renderer must not inject the heading anchors the
    // trusted-content `render()` emits.
    let html = render_user_content_html("# Login\n\n## csrf-token\n");
    assert!(!html.contains("id="), "{html}");
}

#[test]
fn external_links_are_rel_hardened() {
    let html = render_user_content_html("[x](https://example.com)");
    let lowered = html.to_ascii_lowercase();
    assert!(lowered.contains("rel="), "{html}");
    assert!(lowered.contains("noopener"), "{html}");
    assert!(lowered.contains("noreferrer"), "{html}");
}

#[test]
fn images_are_not_embedded_but_alt_text_survives() {
    // Image embedding is out of scope for this field (issue #1255): a Markdown
    // image degrades to its alt text rather than emitting a remote `<img>`.
    let html = render_user_content_html("![a cat](https://example.com/cat.png)");
    assert!(!html.contains("<img"), "{html}");
    assert!(html.contains("a cat"), "{html}");
}

#[test]
fn fenced_code_language_class_survives() {
    let html = render_user_content_html("```rust\nfn main() {}\n```");
    assert!(html.contains("language-rust"), "{html}");
}

#[test]
fn code_class_allowlist_rejects_non_language_classes() {
    // `class` is only tolerated on code/pre and only in the `language-*` shape
    // the Markdown renderer itself emits — never an arbitrary attacker class
    // that could hook a page's own CSS/JS selectors.
    let html = sanitize_user_html("<pre><code class=\"evil-hook\">x</code></pre>");
    assert!(!html.contains("evil-hook"), "{html}");
}

#[test]
fn sanitize_user_html_strips_disallowed_tags_and_attributes() {
    let html = sanitize_user_html(
        "<p onclick=\"alert(1)\">ok</p><script>alert(1)</script><iframe></iframe>",
    );
    assert!(html.contains("ok"), "{html}");
    assert!(!html.contains("onclick"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("<iframe"), "{html}");
}

#[test]
fn sanitize_user_html_drops_javascript_href() {
    let html = sanitize_user_html("<a href=\"javascript:alert(1)\">x</a>");
    assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
}

#[test]
fn allowlists_are_published_and_curated() {
    // The guarantee is inspectable, not folklore: the tag and scheme allowlists
    // are public constants the guide documents.
    for tag in [
        "p", "a", "ul", "ol", "li", "code", "pre", "table", "strong", "em",
    ] {
        assert!(
            RICH_TEXT_ALLOWED_TAGS.contains(&tag),
            "expected {tag} in the allowlist"
        );
    }
    for tag in [
        "script", "style", "iframe", "img", "form", "input", "object",
    ] {
        assert!(
            !RICH_TEXT_ALLOWED_TAGS.contains(&tag),
            "{tag} must never be allowlisted"
        );
    }
    assert!(RICH_TEXT_ALLOWED_URL_SCHEMES.contains(&"https"));
    assert!(!RICH_TEXT_ALLOWED_URL_SCHEMES.contains(&"javascript"));
    assert!(!RICH_TEXT_ALLOWED_URL_SCHEMES.contains(&"data"));
}

#[test]
fn markup_helper_matches_string_helper() {
    let source = "Hello **world** [x](javascript:alert(1))";
    let markup: maud::Markup = render_user_content(source);
    assert_eq!(markup.into_string(), render_user_content_html(source));
}

#[test]
fn empty_input_renders_empty_output() {
    assert_eq!(render_user_content_html("").trim(), "");
}

#[test]
fn very_long_input_does_not_panic() {
    let source = "a **b** [c](https://d.example)\n\n".repeat(5_000);
    let html = render_user_content_html(&source);
    assert!(html.contains("<strong>b</strong>"));
}
