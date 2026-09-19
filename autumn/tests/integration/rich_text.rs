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

/// The complete per-tag attribute allowlist, mirroring `build_sanitizer`'s
/// `tag_attributes` map in `autumn/src/markdown/user_content.rs`.
///
/// This is an **allowlist**, not a denylist, and that distinction is the whole
/// point: a denylist of scary-looking names (`onerror`, `srcdoc`, …) silently
/// tolerates anything nobody thought to enumerate — `style` (a full-viewport
/// clickjacking overlay), `ping` and `target` (which together defeat the forced
/// `rel` hardening), `data-*` hooks into the host page's own scripts. Asserting
/// the exact surviving set means widening the sanitizer without widening this
/// table fails the corpus.
///
/// `rel` appears on `a` because ammonia *adds* it (`link_rel`); it is not
/// accepted from input.
const ALLOWED_TAG_ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href", "title", "rel"]),
    ("code", &["class"]),
    ("pre", &["class"]),
    ("th", &["style"]),
    ("td", &["style"]),
    ("ol", &["start"]),
];

/// The attributes permitted on `tag`, or an empty slice when the tag takes none.
fn allowed_attributes_for(tag: &str) -> &'static [&'static str] {
    ALLOWED_TAG_ATTRIBUTES
        .iter()
        .find(|(name, _)| *name == tag)
        .map_or(&[], |(_, attrs)| *attrs)
}

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
    // Non-scripting attributes that are just as dangerous as an `on*` handler:
    // a full-viewport `style` overlay is clickjacking/UI-redress, a remote
    // `background: url(…)` is the same reader-IP beacon the `<img>` exclusion
    // exists to prevent, and `ping`/`target` together defeat the forced
    // `rel="noopener noreferrer"` hardening.
    "<p style=\"position:fixed;top:0;left:0;width:100vw;height:100vh;z-index:9999\">x</p>",
    "<p style=\"background:url(https://evil.example/beacon.png)\">x</p>",
    "<a href=\"https://ok.example\" ping=\"https://evil.example/track\" target=\"_blank\">x</a>",
    "<a href=\"https://ok.example\" rel=\"\">x</a>",
    // A `data-*` hook into the host page's own scripts.
    "<p data-controller=\"admin\" data-action=\"delete\">x</p>",
    // Attribute smuggling through the two value-narrowed attributes.
    "<pre><code class=\"language-rust evil-hook\">x</code></pre>",
    "<table><tr><td style=\"text-align:left;position:fixed\">x</td></tr></table>",
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
            // The allowlist assertion subsumes every denylist we could write —
            // event handlers, `id`/`name`, `srcdoc`, `formaction`, `style`,
            // `ping`, `target`, `data-*` — because anything not named for this
            // tag fails here.
            assert!(
                allowed_attributes_for(&name).contains(&attr.as_str()),
                "payload {payload:?} kept non-allowlisted attribute {attr:?} on \
                 <{name}> (allowed: {:?}):\n{html}",
                allowed_attributes_for(&name)
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
fn ac_named_combined_payload_emits_no_script_and_no_javascript_href() {
    // The acceptance criterion names this exact input: the rendered output must
    // contain no `<script>` and no `javascript:` href.
    let payload = "<script>alert(1)</script>[x](javascript:alert(1))";
    let html = render_user_content_html(payload);
    let lowered = html.to_ascii_lowercase();

    // No script element.
    assert!(!lowered.contains("<script"), "{html}");
    assert!(!lowered.contains("</script"), "{html}");
    // No `javascript:` href — the criterion is about the *href*, and
    // `assert_inert` proves it structurally by parsing every surviving tag.
    assert!(
        !lowered.contains("href="),
        "no anchor survives here:\n{html}"
    );
    assert_inert(payload, &html);

    // Everything survives only as inert, escaped text. Note the link is not
    // even parsed as a link here: `<script>` opens a CommonMark *HTML block*,
    // which swallows the rest of the line — so `javascript:` appears in the
    // output as literal characters. That is why this asserts on `href` rather
    // than on the bare substring.
    assert!(html.contains("&lt;script&gt;"), "{html}");

    // Split across lines the link IS parsed as a link — and is still defused.
    let split = render_user_content_html("<script>alert(1)</script>\n\n[x](javascript:alert(1))");
    let split_lower = split.to_ascii_lowercase();
    assert!(!split_lower.contains("<script"), "{split}");
    assert!(
        !split_lower.contains("javascript:"),
        "a genuinely parsed javascript: link must leave no trace:\n{split}"
    );
    assert!(split.contains('x'), "the link text survives:\n{split}");
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
fn deeply_nested_blocks_render_in_linear_time() {
    // A rich-text column is rendered on every show-page view and on every
    // keystroke-debounced preview POST, so the renderer must not be
    // super-linear in any attacker-controlled dimension. `"> "` is the maximal
    // amplifier: two source bytes per nesting level.
    //
    // Before the nesting cap this was O(depth²) inside the HTML sanitizer's
    // open-elements scope walk — 80 KB of input took ~111s.
    //
    // This asserts the *complexity class*, not a wall-clock budget. An absolute
    // deadline cannot tell "the algorithm regressed" from "this runner is
    // slow", and the two differ by orders of magnitude across CI platforms:
    // 80 KB of this input renders in ~24ms on a Linux runner and ~13s on a
    // macOS one — a ~500x spread for identical work, which silently ate any
    // fixed ceiling placed between them. Doubling the depth must roughly
    // double the work (linear, ~2x); quadratic would quadruple it (~4x). A
    // ratio is invariant to machine speed, so it means the same thing
    // everywhere.
    use std::time::{Duration, Instant};

    fn render_nested(levels: usize) -> (Duration, usize) {
        let source = "> ".repeat(levels);
        let started = Instant::now();
        let html = render_user_content_html(&source);
        (started.elapsed(), html.len())
    }

    const BASE_LEVELS: usize = 20_000;
    const MAX_RATIO: f64 = 3.0;

    // Warm-up: keep one-off initialisation (allocator growth, the sanitizer's
    // lazily-built allowlist) out of the first measured sample, which would
    // otherwise inflate the baseline and depress the ratio.
    let _ = render_nested(1_000);

    // This test shares the consolidated `integration_tests` binary with ~1800
    // others running on a parallel thread pool, so contention can stretch one
    // sample and not the other. Preemption only ever makes a sample *slower*,
    // so the lowest ratio observed is the best estimate of the true one: a
    // genuine super-linear regression misses the bound on every attempt, while
    // a scheduling blip does not survive repetition. The two sizes are measured
    // adjacently within each attempt so a slow patch tends to hit both, and the
    // loop exits as soon as one attempt is under the bound — a healthy renderer
    // pays for exactly one attempt.
    let mut best_ratio = f64::INFINITY;
    let mut best_pair = None;
    let mut html_len = 0;
    let mut baseline_too_fast = false;

    for _ in 0..3 {
        let (base, _) = render_nested(BASE_LEVELS);
        let (doubled, len) = render_nested(BASE_LEVELS * 2);
        html_len = len;

        // Below ~1ms the clock's granularity, not the renderer, dominates the
        // sample; a ratio computed from noise would flake in both directions.
        // Any machine that fast is nowhere near a super-linear blow-up anyway,
        // so the absolute backstop below carries the check on its own.
        if base < Duration::from_millis(1) {
            baseline_too_fast = true;
            break;
        }

        let ratio = doubled.as_secs_f64() / base.as_secs_f64();
        if ratio < best_ratio {
            best_ratio = ratio;
            best_pair = Some((base, doubled));
        }
        if best_ratio < MAX_RATIO {
            break;
        }
    }

    if !baseline_too_fast {
        let (base, doubled) = best_pair.expect("at least one attempt was measured");
        assert!(
            best_ratio < MAX_RATIO,
            "doubling nesting depth multiplied render time by {best_ratio:.2}x \
             (best of 3 attempts: {base:?} at {BASE_LEVELS} levels -> {doubled:?} \
             at {}) — linear is ~2x and quadratic ~4x, so the renderer has \
             regressed to super-linear behaviour",
            BASE_LEVELS * 2
        );

        // Backstop against a catastrophic regression that somehow keeps its
        // shape, and against a pathologically slow platform. Deliberately far
        // above the slowest observed healthy run (~13s on macOS) and far below
        // the ~111s the pre-cap quadratic behaviour cost.
        assert!(
            doubled.as_secs() < 60,
            "rendering {} bytes of nested blockquotes took {doubled:?}",
            BASE_LEVELS * 4
        );
    }

    // Output stays bounded too: past the cap the nesting is flattened rather
    // than emitted, so a 80 KB input cannot inflate into megabytes of markup.
    assert!(
        html_len < 10_000,
        "output inflated to {html_len} bytes from a capped-depth input"
    );
}

#[test]
fn nesting_below_the_cap_is_preserved_exactly() {
    // The cap must not disturb documents anyone would actually write.
    let source = "> ".repeat(20) + "quoted";
    let html = render_user_content_html(&source);
    assert_eq!(
        html.matches("<blockquote>").count(),
        20,
        "ordinary nesting must survive untouched:\n{html}"
    );
    assert!(html.contains("quoted"), "{html}");
}

#[test]
fn rejected_autolink_keeps_its_text_exactly_once() {
    // Dropping the anchor must not duplicate the destination: the parser
    // already emits the autolink's text as its own event.
    let html = render_user_content_html("<javascript:alert(1)>");
    assert_eq!(html.trim(), "<p>javascript:alert(1)</p>", "{html}");
    let html = render_user_content_html("<ftp://example.com/f>");
    assert_eq!(html.trim(), "<p>ftp://example.com/f</p>", "{html}");
    // An allowed autolink still becomes a real link, exactly once.
    let html = render_user_content_html("<https://example.com/>");
    assert_eq!(html.matches("<a ").count(), 1, "{html}");
    assert_eq!(html.matches("https://example.com/").count(), 2, "{html}");
}

#[test]
fn very_long_input_does_not_panic() {
    let source = "a **b** [c](https://d.example)\n\n".repeat(5_000);
    let html = render_user_content_html(&source);
    assert!(html.contains("<strong>b</strong>"));
}
