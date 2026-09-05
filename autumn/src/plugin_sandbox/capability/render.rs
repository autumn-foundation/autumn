//! Render hooks: a sandboxed plugin filling a host-declared slot (issue #1632).
//!
//! The host application declares named slots; a plugin granted a slot returns
//! something for it, and the host puts it on the page. That last clause is the
//! whole problem — the fragment is served from the *host's* origin, so anything
//! executable in it executes with the host page's authority.
//!
//! # The fragment is a tree, not a string of HTML
//!
//! The obvious design is "the guest returns HTML and the host sanitises it".
//! Sanitising is a filter in front of a parser, and every notable sanitiser bug
//! in the last decade has been a *parser differential*: the sanitiser's HTML
//! parser and the browser's disagree about one input — `<noscript>` contents,
//! foreign content in `<svg>`/`<math>`, a mutation the serialiser makes on the
//! way out — and the filter approves a string that means something else once
//! parsed. Getting that right requires a full HTML5 tokeniser and an ongoing
//! commitment to track browser behaviour.
//!
//! So the guest does not send HTML. It sends a [`FragmentNode`] tree, and the
//! host renders it:
//!
//! ```text
//! guest → {"op":"fragment","nodes":[{"node":"element","tag":"p",
//!            "children":[{"node":"text","text":"3 orders"}]}]}
//! host  → <p>3 orders</p>
//! ```
//!
//! There is no parser to disagree with, because there is no parsing. The output
//! is a function this module writes, from a closed tag list, a closed attribute
//! list, and text that is escaped on the way out. Script, styles, event
//! handlers, `javascript:` URLs, `<iframe>`, `<object>` and inline SVG are not
//! filtered out — there is no way to express them.
//!
//! That also makes the result **CSP-safe** in the strict sense: nothing rendered
//! here needs `unsafe-inline` for scripts or styles, so a host page with a
//! nonce-based policy keeps it.
//!
//! # A slow or trapping hook omits the fragment
//!
//! A render hook is decoration on somebody else's page. If the guest traps,
//! exhausts its fuel, answers with something this module refuses, or overruns
//! the `render_bytes` quota, the host omits the fragment and serves the page.
//! There is no path where a plugin's failure becomes the page's.

use serde::{Deserialize, Serialize};

/// Tags a fragment may use.
///
/// A closed list of *structural* elements. Everything absent is absent because
/// it carries authority the host page's own markup would not: `<script>` and
/// `<style>` execute, `<iframe>`/`<object>`/`<embed>` load an origin,
/// `<form>`/`<input>`/`<button>` submit somewhere, `<img>`/`<video>` fetch and
/// leak a viewer's IP to a third party, `<link>`/`<meta>`/`<base>` re-point the
/// document, and `<svg>`/`<math>` open the foreign-content parsing modes that
/// most sanitiser bypasses live in.
pub const ALLOWED_TAGS: &[&str] = &[
    "a", "abbr", "b", "br", "code", "dd", "div", "dl", "dt", "em", "h3", "h4", "h5", "h6", "hr",
    "i", "li", "ol", "p", "pre", "small", "span", "strong", "table", "tbody", "td", "tfoot", "th",
    "thead", "time", "tr", "ul",
];

/// Tags rendered as a single self-closing element, with no children.
pub const VOID_TAGS: &[&str] = &["br", "hr"];

/// Attributes a fragment may set, and on which tags.
///
/// `style` is absent: an inline style needs `unsafe-inline` in a style policy,
/// and CSS alone can exfiltrate — `background: url(https://…)` keyed off an
/// attribute selector is a well-known read of the surrounding page. `id` is
/// absent because a duplicate id changes what the *host's* own scripts and
/// labels resolve to. `target` and `rel` are absent because the renderer sets
/// them itself, below.
pub const ALLOWED_ATTRIBUTES: &[(&str, &str)] = &[
    ("a", "href"),
    ("a", "title"),
    ("time", "datetime"),
    ("td", "colspan"),
    ("th", "colspan"),
    ("td", "rowspan"),
    ("th", "rowspan"),
    ("*", "class"),
    ("*", "lang"),
    ("*", "dir"),
];

/// How deep a fragment may nest.
///
/// Rendering recurses, so the depth is stack the guest chooses. Eight is far
/// past what a panel needs and far short of anything that could matter.
pub const MAX_DEPTH: usize = 8;

/// How many nodes one fragment may carry, counted across the whole tree.
pub const MAX_NODES: usize = 512;

/// Longest accepted text run or attribute value, in bytes.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;

/// One node of a fragment.
///
/// Internally tagged by `node`, like every other frame in this protocol, so a
/// plugin author writes `{"node":"text","text":"…"}` rather than a nested
/// object per variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum FragmentNode {
    /// A run of text, escaped on the way out.
    Text {
        /// The text.
        text: String,
    },
    /// An element from [`ALLOWED_TAGS`].
    Element {
        /// The tag name.
        tag: String,
        /// Attributes, each of which must appear in [`ALLOWED_ATTRIBUTES`].
        #[serde(default)]
        attributes: Vec<(String, String)>,
        /// Children.
        #[serde(default)]
        children: Vec<FragmentNode>,
    },
}

/// Why a fragment was not rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RenderError {
    /// The plugin was not granted this slot.
    SlotNotGranted(String),
    /// A tag outside [`ALLOWED_TAGS`].
    ForbiddenTag(String),
    /// An attribute outside [`ALLOWED_ATTRIBUTES`] for its tag.
    ForbiddenAttribute {
        /// The tag it appeared on.
        tag: String,
        /// The attribute name.
        name: String,
    },
    /// An attribute value this build will not emit.
    InvalidAttributeValue {
        /// The attribute name.
        name: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// The tree nests deeper than [`MAX_DEPTH`].
    TooDeep,
    /// The tree carries more than [`MAX_NODES`] nodes.
    TooManyNodes,
    /// A text run or attribute value is over [`MAX_TEXT_BYTES`].
    TextTooLong(usize),
    /// The rendered fragment is over the `render_bytes` quota.
    TooLarge {
        /// What it came to.
        found: usize,
        /// The ceiling.
        max: usize,
    },
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlotNotGranted(slot) => {
                write!(f, "`[grants].slots` does not name {slot:?}")
            }
            Self::ForbiddenTag(tag) => write!(
                f,
                "<{tag}> is not one of the structural tags a render hook may use"
            ),
            Self::ForbiddenAttribute { tag, name } => {
                write!(f, "attribute `{name}` is not allowed on <{tag}>")
            }
            Self::InvalidAttributeValue { name, reason } => {
                write!(f, "attribute `{name}`: {reason}")
            }
            Self::TooDeep => write!(f, "a fragment may nest at most {MAX_DEPTH} deep"),
            Self::TooManyNodes => write!(f, "a fragment may carry at most {MAX_NODES} nodes"),
            Self::TextTooLong(found) => write!(
                f,
                "a text run or attribute value of {found} bytes is over the \
                 {MAX_TEXT_BYTES}-byte ceiling"
            ),
            Self::TooLarge { found, max } => write!(
                f,
                "the rendered fragment came to {found} bytes, over the {max}-byte `render_bytes` \
                 quota"
            ),
        }
    }
}

impl std::error::Error for RenderError {}

/// Render a fragment to HTML, or say why it will not be.
///
/// # Errors
///
/// See [`RenderError`]. Every one of them makes the host omit the fragment.
pub fn render(nodes: &[FragmentNode], max_bytes: usize) -> Result<String, RenderError> {
    let mut out = String::new();
    let mut budget = MAX_NODES;
    for node in nodes {
        write_node(&mut out, node, 0, &mut budget, max_bytes)?;
    }
    if out.len() > max_bytes {
        return Err(RenderError::TooLarge {
            found: out.len(),
            max: max_bytes,
        });
    }
    Ok(out)
}

fn write_node(
    out: &mut String,
    node: &FragmentNode,
    depth: usize,
    budget: &mut usize,
    max_bytes: usize,
) -> Result<(), RenderError> {
    if depth > MAX_DEPTH {
        return Err(RenderError::TooDeep);
    }
    *budget = budget.checked_sub(1).ok_or(RenderError::TooManyNodes)?;
    // Checked as the string grows rather than only at the end: a tree within
    // every structural bound can still render to megabytes (512 nodes each
    // carrying 8 KiB of text is 4 MiB), and the point of the ceiling is not to
    // *report* that but to stop building it.
    if out.len() > max_bytes {
        return Err(RenderError::TooLarge {
            found: out.len(),
            max: max_bytes,
        });
    }
    match node {
        FragmentNode::Text { text } => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(RenderError::TextTooLong(text.len()));
            }
            escape_into(out, text);
        }
        FragmentNode::Element {
            tag,
            attributes,
            children,
        } => {
            let tag = tag.to_ascii_lowercase();
            if !ALLOWED_TAGS.contains(&tag.as_str()) {
                return Err(RenderError::ForbiddenTag(super::super::manifest::rejected(
                    &tag,
                )));
            }
            out.push('<');
            out.push_str(&tag);
            for (name, value) in attributes {
                write_attribute(out, &tag, name, value)?;
            }
            if VOID_TAGS.contains(&tag.as_str()) {
                // Rendered `<br />` rather than `<br>`: the fragment is spliced
                // into a host page whose author may be serving XHTML, and the
                // self-closing form parses the same in HTML5.
                out.push_str(" />");
                return Ok(());
            }
            out.push('>');
            for child in children {
                write_node(out, child, depth.saturating_add(1), budget, max_bytes)?;
            }
            out.push_str("</");
            out.push_str(&tag);
            out.push('>');
        }
    }
    Ok(())
}

fn write_attribute(
    out: &mut String,
    tag: &str,
    name: &str,
    value: &str,
) -> Result<(), RenderError> {
    let name = name.to_ascii_lowercase();
    let allowed = ALLOWED_ATTRIBUTES
        .iter()
        .any(|(on, known)| *known == name && (*on == "*" || *on == tag));
    if !allowed {
        return Err(RenderError::ForbiddenAttribute {
            tag: tag.to_owned(),
            name: super::super::manifest::rejected(&name),
        });
    }
    if value.len() > MAX_TEXT_BYTES {
        return Err(RenderError::TextTooLong(value.len()));
    }
    match name.as_str() {
        "href" => check_href(value)?,
        "class" | "lang" | "dir" | "datetime" => {
            // A closed charset rather than escaping: these values end up in
            // selectors, in `lang` matching and in date parsing, and a value
            // that needs escaping to be safe here is a value that was never
            // going to work anyway.
            if !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b' ' | b'-' | b'_' | b':' | b'.' | b'+')
            }) {
                return Err(RenderError::InvalidAttributeValue {
                    name: name.clone(),
                    reason: "expected letters, digits, spaces and `-_:.+`",
                });
            }
        }
        "colspan" | "rowspan" => {
            if value.is_empty()
                || value.len() > 3
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(RenderError::InvalidAttributeValue {
                    name: name.clone(),
                    reason: "expected one to three digits",
                });
            }
        }
        // `title` is the only free-form value, and it is escaped like text.
        _ => {}
    }
    out.push(' ');
    out.push_str(&name);
    out.push_str("=\"");
    escape_into(out, value);
    out.push('"');
    Ok(())
}

/// Refuse any `href` that is not a same-document or same-origin path.
///
/// An allow-list of *shapes* rather than a deny-list of schemes. `javascript:`
/// is the famous one, but `data:text/html`, `vbscript:`, and every scheme a
/// browser or an OS handler adds next are the same hole, and a deny-list is
/// never finished. A protocol-relative `//host` is refused for the same reason
/// an absolute URL is: the fragment is decoration on the host's page, and a
/// plugin that wants to send a reader elsewhere can say so in text.
fn check_href(value: &str) -> Result<(), RenderError> {
    let shaped = (value.starts_with('/') && !value.starts_with("//"))
        || value.starts_with('#')
        || value.starts_with('?');
    if !shaped {
        return Err(RenderError::InvalidAttributeValue {
            name: "href".to_owned(),
            reason: "expected a same-origin link: `/path`, `?query` or `#fragment`",
        });
    }
    // Control characters are how `java\tscript:` gets past a scheme check in a
    // browser that strips them. Nothing here needs one.
    if value.chars().any(char::is_control) {
        return Err(RenderError::InvalidAttributeValue {
            name: "href".to_owned(),
            reason: "may not carry control characters",
        });
    }
    Ok(())
}

/// Escape text for both element content and a double-quoted attribute value.
///
/// One function for both contexts, escaping the union of what each needs: `&`
/// and `<` and `>` for content, plus `"` and `'` for attributes. Two functions
/// would be one function and a place to use the wrong one.
fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> FragmentNode {
        FragmentNode::Text {
            text: value.to_owned(),
        }
    }

    fn element(
        tag: &str,
        attributes: &[(&str, &str)],
        children: Vec<FragmentNode>,
    ) -> FragmentNode {
        FragmentNode::Element {
            tag: tag.to_owned(),
            attributes: attributes
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            children,
        }
    }

    #[test]
    fn a_fragment_renders_to_escaped_html() {
        let nodes = vec![element(
            "p",
            &[("class", "panel")],
            vec![text("3 orders <b>& counting</b>")],
        )];
        assert_eq!(
            render(&nodes, 4096),
            Ok(r#"<p class="panel">3 orders &lt;b&gt;&amp; counting&lt;/b&gt;</p>"#.to_owned())
        );
    }

    #[test]
    fn a_void_tag_never_carries_children_into_the_page() {
        let nodes = vec![element("br", &[], vec![text("smuggled")])];
        assert_eq!(render(&nodes, 4096), Ok("<br />".to_owned()));
    }

    #[test]
    fn nothing_executable_can_be_expressed() {
        for tag in [
            "script", "style", "iframe", "object", "embed", "form", "input", "button", "img",
            "svg", "math", "link", "meta", "base", "template", "noscript",
        ] {
            assert!(
                matches!(
                    render(&[element(tag, &[], Vec::new())], 4096),
                    Err(RenderError::ForbiddenTag(_))
                ),
                "<{tag}> was rendered"
            );
        }
    }

    #[test]
    fn no_event_handler_or_style_survives() {
        for name in [
            "onclick",
            "onerror",
            "onload",
            "style",
            "id",
            "srcdoc",
            "formaction",
            "xlink:href",
        ] {
            assert!(
                matches!(
                    render(&[element("div", &[(name, "x")], Vec::new())], 4096),
                    Err(RenderError::ForbiddenAttribute { .. })
                ),
                "{name} was rendered"
            );
        }
    }

    #[test]
    fn an_href_may_only_point_back_at_the_host() {
        for href in ["/orders/7", "?page=2", "#top"] {
            assert!(
                render(&[element("a", &[("href", href)], Vec::new())], 4096).is_ok(),
                "{href}"
            );
        }
        for href in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox",
            "https://attacker.test/",
            "//attacker.test/",
            "\\\\attacker.test\\share",
        ] {
            assert!(
                matches!(
                    render(&[element("a", &[("href", href)], Vec::new())], 4096),
                    Err(RenderError::InvalidAttributeValue { .. })
                ),
                "{href} was rendered"
            );
        }
    }

    #[test]
    fn an_attribute_value_cannot_break_out_of_its_quotes() {
        let rendered = render(
            &[element(
                "a",
                &[("title", "\" onmouseover=\"alert(1)")],
                Vec::new(),
            )],
            4096,
        )
        .expect("title is free-form and escaped");
        assert!(!rendered.contains("onmouseover=\"alert"), "{rendered}");
        assert!(rendered.contains("&quot;"), "{rendered}");
    }

    #[test]
    fn a_class_value_is_a_closed_charset_rather_than_an_escaped_one() {
        assert!(render(&[element("div", &[("class", "a b-c_d")], Vec::new())], 4096).is_ok());
        assert!(matches!(
            render(&[element("div", &[("class", "a\"b")], Vec::new())], 4096),
            Err(RenderError::InvalidAttributeValue { .. })
        ));
    }

    #[test]
    fn a_tree_deeper_or_wider_than_the_ceilings_is_refused() {
        let mut deep = text("leaf");
        for _ in 0..(MAX_DEPTH + 2) {
            deep = element("div", &[], vec![deep]);
        }
        assert_eq!(render(&[deep], 1 << 20), Err(RenderError::TooDeep));

        let wide: Vec<FragmentNode> = (0..=MAX_NODES).map(|_| text("x")).collect();
        assert_eq!(render(&wide, 1 << 20), Err(RenderError::TooManyNodes));
    }

    #[test]
    fn a_fragment_over_the_render_bytes_quota_is_refused_rather_than_truncated() {
        // A truncated fragment is a fragment with an unclosed tag, which
        // reflows the host page around it.
        let nodes: Vec<FragmentNode> = (0..64).map(|_| text(&"x".repeat(1024))).collect();
        assert!(matches!(
            render(&nodes, 4096),
            Err(RenderError::TooLarge { .. })
        ));
    }

    #[test]
    fn an_over_long_text_run_is_refused_before_it_is_escaped() {
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            render(&[text(&long)], 1 << 20),
            Err(RenderError::TextTooLong(MAX_TEXT_BYTES + 1))
        );
    }

    #[test]
    fn every_allowed_tag_actually_renders() {
        for tag in ALLOWED_TAGS {
            assert!(
                render(&[element(tag, &[], Vec::new())], 4096).is_ok(),
                "<{tag}> is allowed but does not render"
            );
        }
    }

    #[test]
    fn a_fragment_round_trips_through_the_wire_shape_a_guest_writes() {
        let json = r#"[{"node":"element","tag":"p","children":[{"node":"text","text":"hi"}]}]"#;
        let nodes: Vec<FragmentNode> = serde_json::from_str(json).expect("parses");
        assert_eq!(render(&nodes, 4096), Ok("<p>hi</p>".to_owned()));
        // Anything the shape does not define is refused, like every other frame.
        assert!(
            serde_json::from_str::<Vec<FragmentNode>>(
                r#"[{"node":"element","tag":"p","onclick":"x"}]"#
            )
            .is_err()
        );
    }
}
