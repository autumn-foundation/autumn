//! The safety allowlist every Constela document is checked against.
//!
//! # Threat model
//!
//! A Constela document reaching this module was written by a language model,
//! usually from a prompt a user typed. That makes it exactly as trustworthy as
//! the request body in [`markdown::render_user_content`][uc] — the author is an
//! attacker until proven otherwise — and this module is the counterpart of that
//! path's allowlist, applied to a UI tree instead of prose.
//!
//! [uc]: crate::markdown::render_user_content
//!
//! # The guarantee
//!
//! Four independent controls, all applied:
//!
//! 1. **Tags are allowlisted.** [`is_allowed_tag`] admits a curated set of
//!    structural, text, media and form elements. Everything else — every
//!    script-bearing, style-bearing, navigation-hijacking and
//!    document-structure element — is rejected at validation time, so it never
//!    reaches the renderer.
//! 2. **Attributes are allowlisted.** [`is_allowed_attr`] admits presentation,
//!    form, table and `aria-*`/`data-*` attributes. Every `on*` handler
//!    attribute is rejected, as is `style`, so a document cannot smuggle script
//!    through an attribute even on an allowed tag.
//! 3. **URL schemes are allowlisted.** [`is_allowed_url`] admits relative URLs
//!    and the `http`, `https`, `mailto` and `tel` schemes, after stripping the
//!    ASCII whitespace and control characters browsers ignore when resolving a
//!    scheme. `javascript:`, `data:`, `vbscript:` and `file:` are rejected —
//!    including obfuscated spellings such as `java\tscript:`.
//! 4. **Text is escaped, never interpolated.** Every byte of document-derived
//!    output goes through one of the renderer's two escape functions, and tag
//!    and attribute *names* are written only after clearing the allowlists
//!    above — so they are fixed strings from a fixed set rather than document
//!    text at all. There is no other path from a document to raw HTML: the one
//!    node that produces markup from text,
//!    [`Node::Markdown`](super::ast::Node::Markdown), routes through
//!    [`markdown::render_user_content`][uc]'s sanitizer. The enumeration of
//!    every write is in the header of `src/constela/render.rs`.
//!
//! Controls 1–3 are checked during validation and are therefore visible to the
//! author as diagnostics; control 4 is structural and cannot be switched off.
//!
//! # Why the lists are fixed
//!
//! There is no per-document or per-app configuration here, for the same reason
//! [`RICH_TEXT_ALLOWED_TAGS`](crate::markdown::RICH_TEXT_ALLOWED_TAGS) has
//! none: a guarantee that varies per call site is a guarantee nobody can state.
//! One list, one behaviour, reasoned about once.

// autumn-panic-gate: request-path module — it parses, validates and renders a
// document written by a language model, so every panic in it is reachable by
// hostile input and would be a 500 rather than a diagnostic. Production code
// path must be panic-free. See CONTRIBUTING.md "Request-path panic gate".
// Justify exceptions with #[allow(clippy::<lint>, reason = "…")] at the
// narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

/// HTML tags a document may use.
///
/// Curated, and deliberately not "everything that is not dangerous": a tag
/// earns its place by being something a generated UI actually needs.
#[rustfmt::skip]
pub const ALLOWED_TAGS: &[&str] = &[
    // Sectioning and structure
    "div", "span", "section", "article", "aside", "header", "footer", "main",
    "nav", "figure", "figcaption", "details", "summary", "dialog",
    // Text
    "p", "br", "hr", "blockquote", "pre", "code", "kbd", "samp", "var",
    "h1", "h2", "h3", "h4", "h5", "h6",
    "em", "strong", "b", "i", "u", "s", "del", "ins", "mark", "small",
    "sub", "sup", "abbr", "cite", "q", "time", "address", "wbr",
    // Lists
    "ul", "ol", "li", "dl", "dt", "dd",
    // Links and media. `img` is allowed (unlike the rich-text path, which
    // drops it): a generated UI is chrome the app is rendering on purpose, and
    // its image sources are scheme-checked below.
    "a", "img", "picture", "source", "audio", "video", "track",
    // Tables
    "table", "thead", "tbody", "tfoot", "tr", "th", "td", "caption",
    "colgroup", "col",
    // Forms
    "form", "label", "input", "textarea", "select", "option", "optgroup",
    "button", "fieldset", "legend", "datalist", "output", "progress", "meter",
];

/// Attributes allowed on any tag, beyond the `aria-*`/`data-*` families and
/// the URL-bearing set.
#[rustfmt::skip]
pub const ALLOWED_ATTRS: &[&str] = &[
    // Presentation and identity. `style` is absent on purpose: it is an
    // expression language of its own and the reason `class` exists here.
    "class", "id", "title", "lang", "dir", "hidden", "role", "tabindex",
    "translate", "slot", "part",
    // Links
    "target", "rel", "download", "hreflang", "type", "referrerpolicy",
    // Media
    // `srcset` is absent on purpose: it holds a comma-separated *list* of URLs
    // with descriptors, so a single-value scheme check does not cover it and a
    // list parser here would be a second URL grammar to get wrong. Use `src`.
    "alt", "width", "height", "loading", "decoding",
    "controls", "autoplay", "loop", "muted", "playsinline", "preload",
    "kind", "srclang", "label", "default",
    // Tables
    "colspan", "rowspan", "headers", "scope", "span", "abbr",
    // Forms
    "name", "value", "placeholder", "required", "disabled", "readonly",
    "checked", "selected", "multiple", "min", "max", "step", "minlength",
    "maxlength", "pattern", "size", "rows", "cols", "wrap", "for", "form",
    "list", "accept", "autocomplete", "autofocus", "inputmode", "enterkeyhint",
    "spellcheck", "novalidate", "method", "low", "high", "optimum",
    // Details/dialog
    "open",
];

/// Attributes whose value is a URL and must pass [`is_allowed_url`].
pub const URL_ATTRS: &[&str] = &["href", "src", "action", "formaction", "poster", "cite"];

/// URL schemes a document may link to or load from.
pub const ALLOWED_URL_SCHEMES: &[&str] = &["http", "https", "mailto", "tel"];

/// Attributes whose value is an element id (or a space-separated list of them)
/// and is therefore rewritten with the render's id prefix.
///
/// Rewriting these — rather than banning `id` outright, as the rich-text path
/// does — is what lets a generated form keep `<label for>`/`aria-labelledby`
/// working while still being unable to collide with, or clobber, an id the
/// host page's own scripts rely on. See
/// [`RenderContext::id_prefix`](super::RenderContext::id_prefix).
#[rustfmt::skip]
pub const ID_REF_ATTRS: &[&str] = &[
    "id", "for", "form", "list", "headers",
    "aria-labelledby", "aria-describedby", "aria-controls", "aria-owns",
    "aria-activedescendant", "aria-details", "aria-errormessage", "aria-flowto",
];

/// Tags that must not be given children, because HTML gives them none.
#[rustfmt::skip]
pub const VOID_TAGS: &[&str] = &[
    "br", "hr", "img", "input", "source", "track", "wbr", "col",
];

/// Whether `tag` may appear in a document.
///
/// Comparison is on the lowercased tag, since HTML tag names are
/// case-insensitive and `SCRIPT` must not slip past a check for `script`.
#[must_use]
pub fn is_allowed_tag(tag: &str) -> bool {
    let lower = tag.to_ascii_lowercase();
    ALLOWED_TAGS.contains(&lower.as_str())
}

/// Whether `tag` is a void element and must be rendered without children.
#[must_use]
pub fn is_void_tag(tag: &str) -> bool {
    let lower = tag.to_ascii_lowercase();
    VOID_TAGS.contains(&lower.as_str())
}

/// The attribute-name prefix the renderer owns.
///
/// Event bindings, element refs and island wrappers are emitted under this
/// prefix, so a document writing one directly would produce a duplicate
/// attribute next to the renderer's own — or a binding the renderer never
/// recorded. It is reserved rather than merely deduplicated: a document has a
/// first-class way to express every one of these (an `EventHandler`, a `ref`,
/// an `island` node), so writing the raw attribute is always a mistake and
/// never the only way to say something.
pub const RESERVED_ATTR_PREFIX: &str = "data-constela-";

/// Whether `attr` is in the renderer's reserved namespace.
#[must_use]
pub fn is_reserved_attr(attr: &str) -> bool {
    attr.to_ascii_lowercase().starts_with(RESERVED_ATTR_PREFIX)
}

/// Canonicalize a prop name to the HTML attribute it means.
///
/// A model trained on JSX writes `className` and `htmlFor` far more readily
/// than `class` and `for`, and upstream Constela accepts both spellings.
/// Rejecting them would produce a diagnostic about spelling rather than about
/// safety, so they are mapped here — in the policy, so that
/// [`is_allowed_attr`] and the renderer can never disagree about which name
/// they are judging.
#[must_use]
pub fn canonical_attr(name: &str) -> String {
    match name {
        "className" => "class".to_string(),
        "htmlFor" => "for".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Whether `attr` may appear on an element.
///
/// Rejects every `on*` name explicitly and first, so an event-handler
/// attribute can never be admitted by a later rule — including one added to
/// [`ALLOWED_ATTRS`] by mistake.
#[must_use]
pub fn is_allowed_attr(attr: &str) -> bool {
    let lower = attr.to_ascii_lowercase();
    if lower.starts_with("on") {
        return false;
    }
    if let Some(suffix) = lower
        .strip_prefix("data-")
        .or_else(|| lower.strip_prefix("aria-"))
    {
        // Reject the empty suffix (`data-`, `aria-`) and anything carrying a
        // character that would need escaping in an attribute name.
        return !suffix.is_empty()
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    }
    ALLOWED_ATTRS.contains(&lower.as_str()) || URL_ATTRS.contains(&lower.as_str())
}

/// Whether `attr` carries a URL.
#[must_use]
pub fn is_url_attr(attr: &str) -> bool {
    let lower = attr.to_ascii_lowercase();
    URL_ATTRS.contains(&lower.as_str())
}

/// Whether `attr` carries an element id reference that must be prefixed.
#[must_use]
pub fn is_id_ref_attr(attr: &str) -> bool {
    let lower = attr.to_ascii_lowercase();
    ID_REF_ATTRS.contains(&lower.as_str())
}

/// Whether `url` is safe to place in a URL-bearing attribute.
///
/// Relative URLs — anything with no scheme, including `/a`, `./a`, `?q=1` and
/// `#frag` — are allowed. An absolute URL is allowed only when its scheme is in
/// [`ALLOWED_URL_SCHEMES`].
///
/// The scheme is read from a copy with ASCII whitespace and C0 control
/// characters removed, because that is what a browser does before resolving
/// one: `java\tscript:alert(1)` and `\u{1}javascript:alert(1)` both navigate,
/// so both must be rejected.
///
/// ```
/// # use autumn_web::constela::policy::is_allowed_url;
/// assert!(is_allowed_url("/dashboard"));
/// assert!(is_allowed_url("https://example.com"));
/// assert!(!is_allowed_url("javascript:alert(1)"));
/// assert!(!is_allowed_url("java\tscript:alert(1)"));
/// assert!(!is_allowed_url("data:text/html;base64,PHNjcmlwdD4="));
/// ```
#[must_use]
pub fn is_allowed_url(url: &str) -> bool {
    let stripped: String = url
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
        .collect();

    // Read the characters up to the scheme delimiter. A `/`, `?` or `#` before
    // any `:` means the URL is relative and there is no scheme to check —
    // `/a:b` is a path, not a scheme. Accumulated char by char rather than
    // sliced at the delimiter so no byte index is ever taken on a string whose
    // contents came from a document.
    let mut scheme = String::new();
    let mut delimited = false;
    for c in stripped.chars() {
        match c {
            ':' => {
                delimited = true;
                break;
            }
            '/' | '?' | '#' => return true,
            other => scheme.push(other),
        }
    }
    if !delimited {
        return true;
    }

    // Per RFC 3986 a scheme is ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ).
    // Anything else is not a scheme, so the string is a relative reference.
    let mut bytes = scheme.bytes();
    let scheme_shaped = bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.');
    if !scheme_shaped {
        return true;
    }

    ALLOWED_URL_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_script_bearing_tags() {
        for tag in [
            "script", "style", "iframe", "object", "embed", "base", "link", "meta", "svg", "math",
            "template", "noscript", "frame", "frameset", "applet", "html", "head", "body", "title",
        ] {
            assert!(!is_allowed_tag(tag), "{tag} must not be allowed");
        }
    }

    #[test]
    fn tag_check_is_case_insensitive() {
        assert!(!is_allowed_tag("SCRIPT"));
        assert!(!is_allowed_tag("ScRiPt"));
        assert!(is_allowed_tag("DIV"));
    }

    #[test]
    fn rejects_every_event_handler_attribute() {
        for attr in [
            "onclick",
            "onerror",
            "onload",
            "onfocus",
            "onmouseover",
            "onanimationstart",
            "ONCLICK",
            "onwebkitanimationend",
        ] {
            assert!(!is_allowed_attr(attr), "{attr} must not be allowed");
        }
    }

    #[test]
    fn jsx_spellings_canonicalize_to_the_html_attribute_they_mean() {
        assert_eq!(canonical_attr("className"), "class");
        assert_eq!(canonical_attr("htmlFor"), "for");
        assert_eq!(canonical_attr("ARIA-Label"), "aria-label");
        assert!(is_allowed_attr(&canonical_attr("className")));
    }

    #[test]
    fn rejects_style_and_unknown_attributes() {
        assert!(!is_allowed_attr("style"));
        assert!(!is_allowed_attr("xmlns"));
        assert!(!is_allowed_attr("http-equiv"));
        assert!(!is_allowed_attr("srcdoc"));
        assert!(!is_allowed_attr("formmethod"));
    }

    #[test]
    fn allows_data_and_aria_families_but_not_bare_prefixes() {
        assert!(is_allowed_attr("data-testid"));
        assert!(is_allowed_attr("aria-label"));
        assert!(!is_allowed_attr("data-"));
        assert!(!is_allowed_attr("aria-"));
        assert!(!is_allowed_attr("data-a b"));
        assert!(!is_allowed_attr("data-a\"b"));
    }

    #[test]
    fn the_renderer_namespace_is_reserved() {
        assert!(is_reserved_attr("data-constela-on-click"));
        assert!(is_reserved_attr("DATA-CONSTELA-REF"));
        assert!(!is_reserved_attr("data-constellation"));
        assert!(!is_reserved_attr("data-testid"));
    }

    #[test]
    fn relative_urls_are_allowed() {
        for url in [
            "",
            "/",
            "/dashboard",
            "./a",
            "../a",
            "#anchor",
            "?q=1",
            "posts/1",
            "/a:b",
            "//example.com/x",
        ] {
            assert!(is_allowed_url(url), "{url:?} must be allowed");
        }
    }

    #[test]
    fn dangerous_schemes_are_rejected_including_obfuscated_spellings() {
        for url in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "  javascript:alert(1)",
            "\u{1}javascript:alert(1)",
            "vbscript:msgbox(1)",
            "data:text/html;base64,PHNjcmlwdD4=",
            "file:///etc/passwd",
            "blob:https://example.com/x",
        ] {
            assert!(!is_allowed_url(url), "{url:?} must be rejected");
        }
    }

    #[test]
    fn allowed_schemes_pass() {
        for url in [
            "http://example.com",
            "https://example.com/a?b=c#d",
            "HTTPS://EXAMPLE.COM",
            "mailto:someone@example.com",
            "tel:+15550100",
        ] {
            assert!(is_allowed_url(url), "{url:?} must be allowed");
        }
    }

    #[test]
    fn allowlists_have_no_duplicates() {
        // A duplicate is harmless at runtime but means two edits disagreed
        // about whether an entry was already there, which is worth catching.
        for (label, list) in [
            ("ALLOWED_TAGS", ALLOWED_TAGS),
            ("ALLOWED_ATTRS", ALLOWED_ATTRS),
            ("URL_ATTRS", URL_ATTRS),
            ("ID_REF_ATTRS", ID_REF_ATTRS),
            ("VOID_TAGS", VOID_TAGS),
        ] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(before, sorted.len(), "{label} has a duplicate entry");
        }
    }

    #[test]
    fn every_void_tag_is_an_allowed_tag() {
        for tag in VOID_TAGS {
            assert!(is_allowed_tag(tag), "{tag} is void but not allowed");
        }
    }
}
