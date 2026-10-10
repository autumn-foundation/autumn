//! HTML to Markdown, for `Accept: text/markdown` negotiation.
//!
//! The converter is small and has no dependencies. It reads Autumn page HTML.
//! It writes `CommonMark` with GFM tables. It keeps the content and removes
//! navigation and controls:
//!
//! - It converts `<main>` when the page has one, else `<body>`.
//! - It drops `script`, `style`, `nav`, `form` controls, `svg`, `template`,
//!   and elements marked `hidden` or `aria-hidden="true"`.
//! - It puts `<title>` and `<meta name="description">` in YAML front matter.
//!
//! It never panics and never recurses deeper than [`MAX_DEPTH`].

use std::fmt::Write as _;

/// Maximum element depth the converter descends into. Deeper content is
/// written as plain text.
pub const MAX_DEPTH: usize = 256;

/// Estimate the token count of `markdown` (about four bytes per token).
///
/// Sent as `x-markdown-tokens`.
#[must_use]
pub const fn estimate_tokens(markdown: &str) -> usize {
    markdown.len().div_ceil(4)
}

/// Convert an HTML document to Markdown.
#[must_use]
pub fn html_to_markdown(html: &str) -> String {
    // As the HTML parser does first: CRLF and a lone CR are LF.
    let normalized;
    let html = if html.contains('\r') {
        normalized = html.replace("\r\n", "\n").replace('\r', "\n");
        normalized.as_str()
    } else {
        html
    };
    let mut doc = parse(html);
    doc.base = doc.document_base();
    let mut out = String::new();
    let title = doc
        .find_metadata(|id| doc.name(id) == Some("title"))
        .map(|t| doc.text_of(t));
    let description = doc.meta_description();
    let title = title.map(|t| collapse_ws(&t)).filter(|t| !t.is_empty());
    let description = description
        .map(|d| collapse_ws(&d))
        .filter(|d| !d.is_empty());
    if title.is_some() || description.is_some() {
        out.push_str("---\n");
        if let Some(t) = &title {
            out.push_str("title: ");
            out.push_str(&super::documents::yaml_quote(t));
            out.push('\n');
        }
        if let Some(d) = &description {
            out.push_str("description: ");
            out.push_str(&super::documents::yaml_quote(d));
            out.push('\n');
        }
        out.push_str("---\n\n");
    }
    // Without a `<main>`, the whole document: a browser moves content
    // before `<body>` or after `</body>` into the body.
    let root = doc.find_visible("main").unwrap_or(ROOT);
    let mut w = Writer::default();
    w.children(&doc, root, 0);
    let body = w.finish();
    if body.is_empty() {
        // Front matter alone is still a document; drop the separator blank.
        if out.ends_with("---\n\n") {
            out.pop();
        }
    } else {
        out.push_str(&body);
    }
    out
}

// ── Tree ────────────────────────────────────────────────────────────────────

const ROOT: usize = 0;

enum Kind {
    Element {
        name: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
}

struct Node {
    kind: Kind,
    children: Vec<usize>,
}

/// An arena tree. Nodes refer to children by index, so dropping a deep tree
/// never recurses.
struct Doc {
    nodes: Vec<Node>,
    /// The document's `<base href>`, when it is an absolute `http` or
    /// `https` URL.
    base: Option<url::Url>,
}

impl Doc {
    /// The first `<base>` with an `href`, as a browser picks it. A relative
    /// one resolves against the page URL, which is not known here, so it is
    /// not used; nor is a scheme other than `http` or `https`.
    fn document_base(&self) -> Option<url::Url> {
        let mut stack = vec![ROOT];
        while let Some(id) = stack.pop() {
            match self.name(id) {
                // Template content is not part of the document.
                Some("template") => continue,
                Some("base") => {
                    if let Some(href) = self.attr(id, "href") {
                        return url::Url::parse(href.trim())
                            .ok()
                            .filter(|u| matches!(u.scheme(), "http" | "https"));
                    }
                }
                _ => {}
            }
            stack.extend(self.nodes[id].children.iter().rev());
        }
        None
    }

    /// `raw` resolved against the document base, as a browser follows it.
    /// An absolute URL, or any URL without a base, is unchanged.
    fn resolve<'a>(&self, raw: &'a str) -> std::borrow::Cow<'a, str> {
        let Some(base) = &self.base else {
            return raw.into();
        };
        let cleaned: String = raw
            .trim_matches(|c: char| c <= ' ')
            .chars()
            .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
            .collect();
        match url::Url::parse(&cleaned) {
            Err(url::ParseError::RelativeUrlWithoutBase) => base
                .join(&cleaned)
                .map_or_else(|_| raw.into(), |u| String::from(u).into()),
            _ => raw.into(),
        }
    }

    fn name(&self, id: usize) -> Option<&str> {
        match &self.nodes[id].kind {
            Kind::Element { name, .. } => Some(name),
            Kind::Text(_) => None,
        }
    }

    fn attr(&self, id: usize, key: &str) -> Option<&str> {
        match &self.nodes[id].kind {
            Kind::Element { attrs, .. } => attrs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str()),
            Kind::Text(_) => None,
        }
    }

    /// First element that `want` accepts, in document order, outside hidden
    /// elements and the dropped ones other than `<head>` and `<title>`
    /// (`<template>`, `<svg>`, `<iframe>`, ...): a title in those is not the
    /// page's.
    fn find_metadata(&self, want: impl Fn(usize) -> bool) -> Option<usize> {
        let mut stack = vec![ROOT];
        while let Some(id) = stack.pop() {
            if let Some(n) = self.name(id) {
                let dropped = DROP.contains(&n) && !matches!(n, "head" | "title");
                if id != ROOT && (is_hidden(self, id) || dropped) {
                    continue;
                }
                if want(id) {
                    return Some(id);
                }
            }
            stack.extend(self.nodes[id].children.iter().rev());
        }
        None
    }

    /// First element named `name` that is not hidden and not inside a
    /// hidden or dropped element (such as `<template>`).
    fn find_visible(&self, name: &str) -> Option<usize> {
        let mut stack = vec![ROOT];
        while let Some(id) = stack.pop() {
            if let Some(n) = self.name(id) {
                if id != ROOT && (is_hidden(self, id) || DROP.contains(&n) || n == "template") {
                    continue;
                }
                if n == name {
                    return Some(id);
                }
            }
            stack.extend(self.nodes[id].children.iter().rev());
        }
        None
    }

    fn meta_description(&self) -> Option<String> {
        self.find_metadata(|id| {
            self.name(id) == Some("meta")
                && self
                    .attr(id, "name")
                    .is_some_and(|n| n.eq_ignore_ascii_case("description"))
        })
        .and_then(|id| self.attr(id, "content").map(str::to_owned))
    }

    /// The text below `id`, without markup, and without the descendants the
    /// writer skips: hidden ones and dropped ones (`<script>`, ...).
    fn text_of(&self, id: usize) -> String {
        let mut out = String::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            match &self.nodes[n].kind {
                Kind::Text(t) => out.push_str(t),
                Kind::Element { name, .. } => {
                    if n != id && (is_hidden(self, n) || DROP.contains(&name.as_str())) {
                        continue;
                    }
                    stack.extend(self.nodes[n].children.iter().rev());
                }
            }
        }
        out
    }
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is raw text up to the matching end tag.
const RAW_TEXT: &[&str] = &[
    "script",
    "style",
    "textarea",
    "title",
    "xmp",
    "noscript",
    "iframe",
    "noembed",
    "noframes",
    "plaintext",
];

/// Start tags that close an open element of the listed names first.
fn implied_close(tag: &str) -> &'static [&'static str] {
    match tag {
        "li" => &["li"],
        "dt" | "dd" => &["dt", "dd"],
        "tr" => &["tr", "td", "th"],
        "td" | "th" => &["td", "th"],
        "option" => &["option"],
        "thead" | "tbody" | "tfoot" => &["thead", "tbody", "tfoot", "tr", "td", "th"],
        _ => &[],
    }
}

/// The start tags that close an open `<p>`, as in the HTML parser. A `<li>`,
/// `<dt>` or `<dd>` closes its own kind first, then any `<p>` left open.
fn closes_paragraph(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "center"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "listing"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "plaintext"
            | "pre"
            | "search"
            | "section"
            | "summary"
            | "table"
            | "ul"
            | "xmp"
            | "li"
            | "dd"
            | "dt"
    )
}

/// Elements a document head can hold.
const HEAD_CONTENT: &[&str] = &[
    "base", "link", "meta", "noscript", "script", "style", "template", "title",
];

/// Elements that stop the search for an implied close (a `<li>` inside a
/// nested `<ul>` must not close the outer `<li>`).
const SCOPE: &[&str] = &[
    "ul",
    "ol",
    "table",
    "dl",
    "select",
    "blockquote",
    "template",
];

fn is_heading(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

/// Elements whose content an end tag for an outer element cannot leave: a
/// browser ignores a stray `</div>` inside a `<select>` or a `<template>`.
const WALL: &[&str] = &["select", "template"];

#[allow(clippy::too_many_lines)] // one tokenizer loop; splitting it hides the state
fn parse(html: &str) -> Doc {
    let mut doc = Doc {
        base: None,
        nodes: vec![Node {
            kind: Kind::Element {
                name: "#root".to_owned(),
                attrs: Vec::new(),
            },
            children: Vec::new(),
        }],
    };
    let mut stack: Vec<usize> = vec![ROOT];
    // The start tags past `MAX_DEPTH`. The parser does not keep them. An end
    // tag closes the last dropped tag it names; one naming only a kept
    // element closes that element and every dropped tag inside it.
    let mut dropped = Dropped::default();
    let bytes = html.as_bytes();
    let mut i = 0;

    let push = |doc: &mut Doc, stack: &[usize], kind: Kind| -> usize {
        let id = doc.nodes.len();
        doc.nodes.push(Node {
            kind,
            children: Vec::new(),
        });
        let parent = *stack.last().unwrap_or(&ROOT);
        doc.nodes[parent].children.push(id);
        id
    };

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let end = memchr(b'<', &bytes[i..]).map_or(bytes.len(), |n| i + n);
            let text = decode_entities(&html[i..end], false);
            // Past the depth limit, text joins the last kept element, unless
            // a dropped tag around it hides its content.
            if !text.is_empty() && !dropped.hides() {
                push(&mut doc, &stack, Kind::Text(text));
            }
            i = end;
            continue;
        }
        let rest = &html[i..];
        if let Some(body) = rest.strip_prefix("<!--") {
            i = comment_end(body).map_or(bytes.len(), |n| i + 4 + n);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(bytes.len(), |n| i + n + 1);
            continue;
        }
        let mut tag = match read_tag(rest) {
            TagRead::Tag(tag) => tag,
            TagRead::Text => {
                // A lone `<` is text.
                push(&mut doc, &stack, Kind::Text("<".to_owned()));
                i += 1;
                continue;
            }
            // An unterminated tag runs to the end, as in a browser.
            TagRead::Eof => break,
        };
        i += tag.len;
        // A browser reads `</br>` as `<br>`.
        if tag.end && tag.name == "br" {
            tag.end = false;
            tag.attrs.clear();
        }
        if tag.end {
            // `</select>` and `</template>` close their own wall; any other
            // end tag stops at the innermost one.
            let walled = !WALL.contains(&tag.name.as_str());
            if !dropped.is_empty() {
                let wall = dropped.wall().filter(|_| walled);
                match (dropped.last(&tag.name), wall) {
                    (Some(pos), Some(wall)) if pos < wall => {}
                    (Some(pos), _) => dropped.truncate(pos),
                    (None, Some(_)) => {}
                    (None, None) => close_kept(&doc, &mut stack, &mut dropped, &tag.name, walled),
                }
                continue;
            }
            close_kept(&doc, &mut stack, &mut dropped, &tag.name, walled);
            continue;
        }

        // In a `<select>`, an `<input>`, `<keygen>`, `<textarea>` or another
        // `<select>` closes it, as in a browser; a nested `<select>` is then
        // dropped.
        if matches!(
            tag.name.as_str(),
            "input" | "keygen" | "textarea" | "select"
        ) {
            let in_dropped = dropped
                .wall()
                .filter(|&w| dropped.last("select") == Some(w));
            let closed = if let Some(wall) = in_dropped {
                dropped.truncate(wall);
                true
            } else if dropped.wall().is_none()
                && let Some(pos) = stack
                    .iter()
                    .rposition(|&id| matches!(doc.name(id), Some("select" | "template")))
                && doc.name(stack[pos]) == Some("select")
            {
                stack.truncate(pos);
                dropped.clear();
                true
            } else {
                false
            };
            if closed && tag.name == "select" {
                continue;
            }
        }

        // An omitted `</head>`: a tag that cannot be in the head (`<body>`,
        // `<main>`, ...) closes it, as in a browser. Inside a `<template>`
        // the head is not the insertion point, so the template keeps it.
        if !HEAD_CONTENT.contains(&tag.name.as_str())
            && dropped.wall().is_none()
            && let Some(pos) = stack
                .iter()
                .rposition(|&id| matches!(doc.name(id), Some("head" | "template")))
            && doc.name(stack[pos]) == Some("head")
        {
            stack.truncate(pos);
            dropped.clear();
        }

        // Implied end tags. The search runs from the innermost open tag
        // out, through the dropped tags first, so closing a kept element
        // also closes every dropped tag inside it.
        let paragraph: &[&str] = if closes_paragraph(&tag.name) {
            &["p"]
        } else {
            &[]
        };
        for closes in [implied_close(&tag.name), paragraph] {
            if closes.is_empty() || dropped.implied_close(closes) {
                continue;
            }
            for pos in (1..stack.len()).rev() {
                let Some(open) = doc.name(stack[pos]) else {
                    break;
                };
                if closes.contains(&open) {
                    stack.truncate(pos);
                    dropped.clear();
                    break;
                }
                if SCOPE.contains(&open) {
                    break;
                }
            }
        }

        // A `<button>` or an `<a>` closes one that is still open, as in a
        // browser: they never nest. The search stops at a table cell or a
        // template, which a browser's scope does too.
        if matches!(tag.name.as_str(), "button" | "a") {
            if let Some(pos) = dropped.last(&tag.name) {
                dropped.truncate(pos);
            } else if let Some(pos) = stack
                .iter()
                .rposition(|&id| {
                    doc.name(id).is_some_and(|n| {
                        n == tag.name || matches!(n, "td" | "th" | "caption" | "table" | "template")
                    })
                })
                .filter(|&pos| pos > 0 && doc.name(stack[pos]) == Some(tag.name.as_str()))
            {
                stack.truncate(pos);
                dropped.clear();
            }
        }

        // A table part closes what the table holds that is not a table part
        // (a `<p>` a browser moved out of the table), as the browser's
        // "clear the stack back to a table context" does: the row stays in
        // the table, not in the paragraph.
        if matches!(
            tag.name.as_str(),
            "tr" | "tbody" | "thead" | "tfoot" | "caption" | "colgroup"
        ) && let Some(pos) = stack.iter().rposition(|&id| {
            doc.name(id).is_some_and(|n| {
                matches!(
                    n,
                    "table" | "tbody" | "thead" | "tfoot" | "tr" | "td" | "th" | "template"
                )
            })
        }) && matches!(
            doc.name(stack[pos]),
            Some("table" | "tbody" | "thead" | "tfoot")
        ) && pos + 1 < stack.len()
        {
            stack.truncate(pos + 1);
            dropped.clear();
        }

        // A heading closes a heading left open right before it, as in a
        // browser: `<h1>One<h2>Two` is two headings.
        if is_heading(&tag.name) {
            if let Some(top) = dropped.names.last() {
                if is_heading(top) {
                    dropped.truncate(dropped.names.len() - 1);
                }
            } else if stack.len() > 1
                && let Some(&top) = stack.last()
                && doc.name(top).is_some_and(is_heading)
            {
                stack.pop();
            }
        }

        // Browsers ignore `/>` on an HTML element: `<template/>` stays open
        // to `</template>`, and `<script/>` runs to `</script>`. Only a foreign
        // root (`<svg/>`, `<math/>`) closes itself.
        let raw = RAW_TEXT.contains(&tag.name.as_str());
        let is_void = VOID.contains(&tag.name.as_str())
            || (tag.self_closing && matches!(tag.name.as_str(), "svg" | "math"));
        if !is_void && (stack.len() > MAX_DEPTH || !dropped.is_empty()) {
            if raw {
                // Skip the raw text: its content is never markup.
                let body_end = raw_text_end(&html[i..], &tag.name).map_or(bytes.len(), |n| i + n);
                i = body_end + end_tag_len(&html[body_end..]);
            } else {
                let hides = DROP.contains(&tag.name.as_str())
                    || (tag.name == "dialog" && !tag.attrs.iter().any(|(k, _)| k == "open"))
                    || tag.attrs.iter().any(|(k, v)| {
                        k == "hidden" || (k == "aria-hidden" && v.eq_ignore_ascii_case("true"))
                    });
                dropped.push(tag.name, hides);
            }
            continue;
        }
        if dropped.hides() {
            // A void element (an `<img>`) inside a hidden dropped tag.
            continue;
        }
        let name = tag.name.clone();
        let id = push(
            &mut doc,
            &stack,
            Kind::Element {
                name: tag.name,
                attrs: tag.attrs,
            },
        );
        if raw {
            let body_end = raw_text_end(&html[i..], &name).map_or(bytes.len(), |n| i + n);
            let content = &html[i..body_end];
            if !content.is_empty() {
                let text = if name == "title" || name == "textarea" {
                    decode_entities(content, false)
                } else {
                    content.to_owned()
                };
                let tid = doc.nodes.len();
                doc.nodes.push(Node {
                    kind: Kind::Text(text),
                    children: Vec::new(),
                });
                doc.nodes[id].children.push(tid);
            }
            i = body_end + end_tag_len(&html[body_end..]);
        } else if !is_void {
            stack.push(id);
        }
    }
    doc
}

struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
    end: bool,
    self_closing: bool,
    /// Bytes consumed, including `<` and `>`.
    len: usize,
}

/// Result of [`read_tag`].
enum TagRead {
    Tag(Tag),
    /// Not a tag: the `<` is text. The scan stopped at or before the next
    /// `<`, so the parse stays linear.
    Text,
    /// The tag does not end before the input ends.
    Eof,
}

/// Read one tag at the start of `s` (which starts with `<`).
fn read_tag(s: &str) -> TagRead {
    let b = s.as_bytes();
    let mut i = 1;
    let end = b.get(i) == Some(&b'/');
    if end {
        i += 1;
    }
    let name_start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-' || b[i] == b':') {
        i += 1;
    }
    if i == name_start || !b[name_start].is_ascii_alphabetic() {
        return TagRead::Text;
    }
    let name = s[name_start..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i) {
            None => return TagRead::Eof,
            Some(b'<') => return TagRead::Text,
            Some(b'>') => {
                i += 1;
                break;
            }
            Some(b'/') => {
                self_closing = b.get(i + 1) == Some(&b'>');
                i += 1;
                continue;
            }
            Some(_) => {}
        }
        let key_start = i;
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && !matches!(b[i], b'=' | b'>' | b'/' | b'<')
        {
            i += 1;
        }
        let key = s[key_start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if b.get(i) == Some(&b'=') {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            match b.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    let Some(close) = memchr(q, &b[i + 1..]) else {
                        return TagRead::Eof;
                    };
                    value = decode_entities(&s[i + 1..i + 1 + close], true);
                    i += close + 2;
                }
                Some(_) => {
                    let start = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'>' | b'<')
                    {
                        i += 1;
                    }
                    value = decode_entities(&s[start..i], true);
                }
                None => return TagRead::Eof,
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
    TagRead::Tag(Tag {
        name,
        attrs,
        end,
        self_closing,
        len: i,
    })
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&c| c == needle)
}

/// Bytes from the comment body `body` (after `<!--`) through the comment's
/// end, as a browser reads it: `<!-->` and `<!--->` are empty comments, and
/// `--!>` ends one as `-->` does. `None` runs the comment to the end.
fn comment_end(body: &str) -> Option<usize> {
    if body.starts_with('>') {
        return Some(1);
    }
    if body.starts_with("->") {
        return Some(2);
    }
    let mut from = 0;
    while let Some(n) = body[from..].find("--") {
        let at = from + n;
        let after = &body[at + 2..];
        if after.starts_with('>') {
            return Some(at + 3);
        }
        if after.starts_with("!>") {
            return Some(at + 4);
        }
        from = at + 1;
    }
    None
}

/// Length of the end tag at the start of `s`. A quoted attribute value can
/// hold a `>` (`</script x=">">`), so the tag is read, not searched for.
fn end_tag_len(s: &str) -> usize {
    match read_tag(s) {
        TagRead::Tag(tag) => tag.len,
        TagRead::Eof => s.len(),
        TagRead::Text => s.find('>').map_or(s.len(), |n| n + 1),
    }
}

/// Offset of the end tag that closes the raw-text element `name`. As in the
/// HTML tokenizer, `</name` counts only when the tag name ends there, so
/// `</scripture>` inside a script is still script text. Nothing closes
/// `<plaintext>`: its text runs to the end of the document.
fn raw_text_end(hay: &str, name: &str) -> Option<usize> {
    if name == "plaintext" {
        return None;
    }
    let close = format!("</{name}");
    let mut from = 0;
    while let Some(n) = find_ascii_ci(&hay[from..], &close) {
        let at = from + n;
        match hay.as_bytes().get(at + close.len()) {
            None | Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') => return Some(at),
            Some(_) => from = at + close.len(),
        }
    }
    None
}

/// Find `needle` (ASCII) in `hay`, ignoring ASCII case.
fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Decode HTML character references. Unknown names stay as written.
fn decode_entities(s: &str, in_attribute: bool) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        let used = decode_reference(rest, in_attribute, &mut out);
        if used == 0 {
            out.push('&');
        }
        rest = &rest[used..];
    }
    out.push_str(rest);
    out
}

/// Decode the character reference at the start of `s` (just after the `&`)
/// onto `out`, as the HTML tokenizer does, and return how many bytes of `s`
/// it used; `0` when there is none, and the `&` stays text.
fn decode_reference(s: &str, in_attribute: bool, out: &mut String) -> usize {
    let b = s.as_bytes();
    if b.first() == Some(&b'#') {
        let hex = matches!(b.get(1), Some(b'x' | b'X'));
        let start = if hex { 2 } else { 1 };
        let digits = b[start..]
            .iter()
            .take_while(|c| {
                if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            })
            .count();
        if digits == 0 {
            return 0;
        }
        let radix = if hex { 16 } else { 10 };
        let code = u32::from_str_radix(&s[start..start + digits], radix).unwrap_or(u32::MAX);
        out.push(numeric_reference(code));
        let end = start + digits;
        return end + usize::from(b.get(end) == Some(&b';'));
    }
    let name = b.iter().take_while(|c| c.is_ascii_alphanumeric()).count();
    if name == 0 {
        return 0;
    }
    let named = super::entities::NAMED;
    if b.get(name) == Some(&b';')
        && let Ok(i) = named.binary_search_by(|(key, _)| (*key).cmp(&s[..name]))
    {
        out.push_str(named[i].1);
        return name + 1;
    }
    // The longest legacy name, read without its `;`. In an attribute, one
    // that runs on into a letter, a digit or `=` is left alone, so a query
    // string (`?a=1&copy=2`) keeps its text.
    let legacy = super::entities::LEGACY;
    for len in (1..=name.min(super::entities::LEGACY_MAX)).rev() {
        if let Ok(i) = legacy.binary_search_by(|(key, _)| (*key).cmp(&s[..len])) {
            let next = b.get(len);
            if in_attribute && next.is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'=') {
                return 0;
            }
            out.push_str(legacy[i].1);
            return len;
        }
    }
    0
}

/// The character a numeric reference stands for. As in a browser, the C1
/// range reads as Windows-1252, and NUL, a surrogate or a value past
/// U+10FFFF is U+FFFD.
fn numeric_reference(code: u32) -> char {
    const WINDOWS_1252: [u32; 32] = [
        0x20AC, 0x81, 0x201A, 0x192, 0x201E, 0x2026, 0x2020, 0x2021, 0x2C6, 0x2030, 0x160, 0x2039,
        0x152, 0x8D, 0x17D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
        0x2DC, 0x2122, 0x161, 0x203A, 0x153, 0x9D, 0x17E, 0x178,
    ];
    let code = match code {
        0x80..=0x9F => WINDOWS_1252[(code - 0x80) as usize],
        other => other,
    };
    char::from_u32(code)
        .filter(|&c| c != '\0')
        .unwrap_or('\u{fffd}')
}

fn collapse_ws(s: &str) -> String {
    s.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

// ── Writer ──────────────────────────────────────────────────────────────────

/// An end tag no dropped tag answers: close the last kept element it
/// names, and every dropped tag inside it. Unless the tag is `walled`
/// (`</select>`, `</template>`), it cannot close past the innermost open
/// `<select>` or `<template>`, as in a browser.
fn close_kept(doc: &Doc, stack: &mut Vec<usize>, dropped: &mut Dropped, name: &str, walled: bool) {
    let floor = if walled {
        stack
            .iter()
            .rposition(|&id| doc.name(id).is_some_and(|n| WALL.contains(&n)))
            .unwrap_or(0)
    } else {
        0
    };
    if let Some(pos) = stack.iter().rposition(|&id| doc.name(id) == Some(name))
        && pos > floor
    {
        dropped.clear();
        stack.truncate(pos);
    }
}

/// Open start tags past `MAX_DEPTH`. An index by name finds the match of
/// an end tag in constant time, and each tag leaves the stack once, so a
/// page of unmatched end tags still parses in linear time.
#[derive(Default)]
struct Dropped {
    names: Vec<String>,
    /// Per open tag: whether it hides its content (hidden, or a `DROP`
    /// element), as the writer would at a shallower depth.
    hides: Vec<bool>,
    /// How many open tags hide their content.
    hiding: usize,
    at: std::collections::HashMap<String, Vec<usize>>,
    /// Where the open `WALL` tags sit, innermost last.
    walls: Vec<usize>,
    /// Where the open `SCOPE` tags sit, innermost last.
    scopes: Vec<usize>,
}

impl Dropped {
    const fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    fn push(&mut self, name: String, hides: bool) {
        let pos = self.names.len();
        if WALL.contains(&name.as_str()) {
            self.walls.push(pos);
        }
        if SCOPE.contains(&name.as_str()) {
            self.scopes.push(pos);
        }
        self.at.entry(name.clone()).or_default().push(pos);
        self.names.push(name);
        self.hides.push(hides);
        self.hiding += usize::from(hides);
    }

    /// `true` inside a dropped tag that hides its content.
    const fn hides(&self) -> bool {
        self.hiding > 0
    }

    /// Where the last open dropped `name` sits, if one is open.
    fn last(&self, name: &str) -> Option<usize> {
        self.at.get(name).and_then(|v| v.last()).copied()
    }

    /// Where the last open dropped `select` or `template` sits: end tags
    /// for elements opened before it cannot reach past it.
    fn wall(&self) -> Option<usize> {
        self.walls.last().copied()
    }

    /// Apply a start tag's implied close (`closes`) to the dropped tags.
    /// `true` when the search ended here, on a closed tag or a `SCOPE`;
    /// `false` when it runs on into the kept elements.
    fn implied_close(&mut self, closes: &[&str]) -> bool {
        let found = closes.iter().filter_map(|name| self.last(name)).max();
        let scope = self.scopes.last().copied();
        match (found, scope) {
            (Some(pos), scope) if scope.is_none_or(|scope| pos >= scope) => {
                self.truncate(pos);
                true
            }
            (_, scope) => scope.is_some(),
        }
    }

    /// Close every dropped tag: a kept ancestor's end tag closed them all.
    fn clear(&mut self) {
        self.names.clear();
        self.hides.clear();
        self.hiding = 0;
        self.at.clear();
        self.walls.clear();
        self.scopes.clear();
    }

    /// Close the dropped tag at `pos` and every tag opened after it.
    fn truncate(&mut self, pos: usize) {
        while self.names.len() > pos {
            let top = self.names.len() - 1;
            if self.walls.last() == Some(&top) {
                self.walls.pop();
            }
            if self.scopes.last() == Some(&top) {
                self.scopes.pop();
            }
            if self.hides.pop() == Some(true) {
                self.hiding -= 1;
            }
            if let Some(n) = self.names.pop()
                && let Some(v) = self.at.get_mut(&n)
            {
                v.pop();
            }
        }
    }
}

/// Elements dropped with their content.
const DROP: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "math", "iframe", "object", "canvas",
    "nav", "button", "input", "select", "textarea", "option", "title", "noembed", "noframes",
];

const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "header",
    "footer",
    "main",
    "aside",
    "figure",
    "figcaption",
    "form",
    "fieldset",
    "address",
    "details",
    "summary",
    "dl",
    "dt",
    "dd",
    "#root",
    "center",
    "hgroup",
    "search",
];

/// Builds Markdown as a list of blocks. Each block is finished text; blocks
/// join with one blank line.
#[derive(Default)]
struct Writer {
    blocks: Vec<String>,
    /// Inline text of the block being built.
    line: String,
}

impl Writer {
    fn finish(mut self) -> String {
        self.flush();
        if self.blocks.is_empty() {
            return String::new();
        }
        let mut out = self.blocks.join("\n\n");
        out.push('\n');
        out
    }

    /// End the current paragraph.
    fn flush(&mut self) {
        let text = tidy_inline(&self.line);
        self.line.clear();
        if !text.is_empty() {
            // A `<br>` starts a new Markdown line, which can start a block
            // too, so each line is escaped, not only the first. Its leading
            // spaces go: a browser collapses them, and an indented `#` is
            // still a heading.
            let lines: Vec<String> = text
                .split('\n')
                .map(|line| escape_line_start(line.trim_start_matches(' ').to_owned()))
                .collect();
            self.blocks.push(lines.join("\n"));
        }
    }

    fn children(&mut self, doc: &Doc, id: usize, depth: usize) {
        for &child in &doc.nodes[id].children {
            self.node(doc, child, depth + 1);
        }
    }

    #[allow(clippy::too_many_lines)] // one match arm per element kind
    fn node(&mut self, doc: &Doc, id: usize, depth: usize) {
        let (name, _) = match &doc.nodes[id].kind {
            Kind::Text(t) => {
                self.line.push_str(&escape_inline(t));
                return;
            }
            Kind::Element { name, attrs } => (name.as_str(), attrs),
        };
        if is_hidden(doc, id) || DROP.contains(&name) {
            return;
        }
        if depth > MAX_DEPTH {
            self.line.push_str(&escape_inline(&doc.text_of(id)));
            return;
        }
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                let level = usize::from(name.as_bytes()[1] - b'0');
                let text = Self::inline_of(doc, id, depth);
                if !text.is_empty() {
                    self.blocks.push(format!("{} {text}", "#".repeat(level)));
                }
            }
            "br" => self.line.push_str("  \n"),
            "hr" => {
                self.flush();
                self.blocks.push("---".to_owned());
            }
            "strong" | "b" => self.wrap(doc, id, depth, "**"),
            "em" | "i" => self.wrap(doc, id, depth, "*"),
            "del" | "s" | "strike" => self.wrap(doc, id, depth, "~~"),
            "code" | "kbd" | "samp" => {
                let raw = doc.text_of(id);
                let text = collapse_ws(&raw);
                if !text.is_empty() {
                    // Edge spaces go outside the span, as with other markers.
                    let lead = if raw.starts_with(char::is_whitespace) {
                        " "
                    } else {
                        ""
                    };
                    let trail = if raw.ends_with(char::is_whitespace) {
                        " "
                    } else {
                        ""
                    };
                    let fence = "`".repeat(longest_backtick_run(&text) + 1);
                    let pad = if text.starts_with('`') || text.ends_with('`') {
                        " "
                    } else {
                        ""
                    };
                    let _ = write!(self.line, "{lead}{fence}{pad}{text}{pad}{fence}{trail}");
                }
            }
            "a" => {
                let (lead, text, trail) = Self::inline_parts(doc, id, depth);
                self.line.push_str(lead);
                match doc
                    .attr(id, "href")
                    .and_then(|h| safe_url(&doc.resolve(h), true))
                {
                    // A link with no text shows nothing, so it gets no
                    // label made up from its URL.
                    Some(href) if !text.is_empty() => {
                        // A `!` right before the label would make it an image,
                        // unless an odd run of `\` escapes it.
                        if let Some(before) = self.line.strip_suffix('!')
                            && before.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 0
                        {
                            self.line.pop();
                            self.line.push_str("\\!");
                        }
                        // A destination decodes character references too,
                        // so an `&copy;` the URL holds as text is escaped.
                        let _ = write!(self.line, "[{text}]({})", escape_entity_like(&href));
                    }
                    _ => self.line.push_str(&text),
                }
                self.line.push_str(trail);
            }
            "img" => {
                if let Some(src) = doc
                    .attr(id, "src")
                    .and_then(|s| safe_url(&doc.resolve(s), false))
                {
                    let alt = escape_inline(&collapse_ws(doc.attr(id, "alt").unwrap_or("")));
                    let _ = write!(self.line, "![{alt}]({})", escape_entity_like(&src));
                }
            }
            "pre" => {
                self.flush();
                let code = doc.text_of(id);
                // As in a browser, only a newline right after `<pre>` and one
                // right before `</pre>` are not content; spaces are.
                let code = code.strip_prefix('\n').unwrap_or(&code);
                let code = code.strip_suffix('\n').unwrap_or(code);
                let lang = doc.nodes[id]
                    .children
                    .iter()
                    .find(|&&c| doc.name(c) == Some("code"))
                    .and_then(|&c| doc.attr(c, "class"))
                    .and_then(|class| {
                        class
                            .split_ascii_whitespace()
                            .find_map(|c| c.strip_prefix("language-"))
                    })
                    // A language name is a word: a backtick in it would
                    // break the fence.
                    .filter(|lang| {
                        lang.chars().all(|c| {
                            c.is_alphanumeric() || matches!(c, '-' | '_' | '+' | '.' | '#')
                        })
                    })
                    .unwrap_or("");
                let fence = "`".repeat((longest_backtick_run(code) + 1).max(3));
                self.blocks.push(format!("{fence}{lang}\n{code}\n{fence}"));
            }
            "blockquote" => {
                self.flush();
                let inner = Self::sub_block(doc, id, depth);
                if !inner.is_empty() {
                    let quoted: Vec<String> = inner
                        .lines()
                        .map(|l| {
                            if l.is_empty() {
                                ">".to_owned()
                            } else {
                                format!("> {l}")
                            }
                        })
                        .collect();
                    self.blocks.push(quoted.join("\n"));
                }
            }
            "ul" | "ol" | "menu" => {
                self.flush();
                self.blocks.extend(Self::list(doc, id, depth, name == "ol"));
            }
            "table" => {
                self.flush();
                let table = Self::table(doc, id, depth);
                if !table.is_empty() {
                    self.blocks.push(table);
                }
            }
            "li" => {
                // A list item outside a list: treat as a paragraph.
                self.flush();
                self.children(doc, id, depth);
                self.flush();
            }
            "label" | "legend" | "caption" => {
                self.flush();
                self.children(doc, id, depth);
                self.flush();
            }
            _ if BLOCK.contains(&name) => {
                self.flush();
                self.children(doc, id, depth);
                self.flush();
            }
            _ => self.children(doc, id, depth),
        }
    }

    fn wrap(&mut self, doc: &Doc, id: usize, depth: usize, mark: &str) {
        let (lead, text, trail) = Self::inline_parts(doc, id, depth);
        if !text.is_empty() {
            let _ = write!(self.line, "{lead}{mark}{text}{mark}{trail}");
        }
    }

    /// Render the children of `id` as one inline string, on one line.
    fn inline_of(doc: &Doc, id: usize, depth: usize) -> String {
        Self::inline_parts(doc, id, depth).1
    }

    /// [`Self::inline_of`], plus the space that sat at each edge, so a
    /// marker never glues two words together.
    fn inline_parts(doc: &Doc, id: usize, depth: usize) -> (&'static str, String, &'static str) {
        let mut sub = Self::default();
        sub.children(doc, id, depth);
        let edge = |c: Option<char>| {
            if c.is_some_and(char::is_whitespace) {
                " "
            } else {
                ""
            }
        };
        let lead = edge(sub.line.chars().next());
        let trail = edge(sub.line.chars().last());
        sub.flush();
        let text = sub.blocks.join(" ").replace("  \n", " ").replace('\n', " ");
        (lead, text, trail)
    }

    /// Render the children of `id` as finished blocks.
    fn sub_block(doc: &Doc, id: usize, depth: usize) -> String {
        let mut sub = Self::default();
        sub.children(doc, id, depth);
        sub.finish().trim_end().to_owned()
    }

    /// A list as blocks: its items, and what a browser shows between them.
    /// Text or an element that is no `<li>` stays in place, so it splits the
    /// list there; numbering carries on past it.
    fn list(doc: &Doc, id: usize, depth: usize, ordered: bool) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut other = Self::default();
        let end_other = |other: &mut Self, lines: &mut Vec<String>, blocks: &mut Vec<String>| {
            let text = std::mem::take(other).finish();
            let text = text.trim_end();
            if !text.is_empty() {
                if !lines.is_empty() {
                    blocks.push(std::mem::take(lines).join("\n"));
                }
                blocks.push(text.to_owned());
            }
        };
        let numbers = if ordered {
            Self::item_numbers(doc, id)
        } else {
            Vec::new()
        };
        // `<ol type>` picks letters or Roman numerals; the attribute is case
        // sensitive.
        let style = doc
            .attr(id, "type")
            .filter(|t| matches!(*t, "a" | "A" | "i" | "I"));
        // A Markdown list number is 0 to 999999999, in decimal. For any
        // other label, the items are bullets that spell out their labels.
        let as_bullets = style.is_some() || numbers.iter().any(|n| !(0..=999_999_999).contains(n));
        let mut numbers = numbers.into_iter();
        for &item in &doc.nodes[id].children {
            if doc.name(item) != Some("li") {
                other.node(doc, item, depth + 1);
                continue;
            }
            if is_hidden(doc, item) {
                continue;
            }
            end_other(&mut other, &mut lines, &mut blocks);
            let (marker, label) = match numbers.next() {
                Some(n) if as_bullets => ("- ".to_owned(), format!("{}\\. ", list_label(n, style))),
                Some(n) => (format!("{n}. "), String::new()),
                None => ("- ".to_owned(), String::new()),
            };
            let pad = " ".repeat(marker.len());
            // A nested list sits under the item text with no blank line.
            let body = collapse_nested_list_gap(&Self::sub_block(doc, item, depth + 1));
            for (k, l) in body.lines().enumerate() {
                if k == 0 {
                    lines.push(format!("{marker}{label}{l}"));
                } else if l.is_empty() {
                    lines.push(String::new());
                } else {
                    lines.push(format!("{pad}{l}"));
                }
            }
            if body.is_empty() {
                lines.push(format!("{marker}{label}").trim_end().to_owned());
            }
        }
        end_other(&mut other, &mut lines, &mut blocks);
        if !lines.is_empty() {
            blocks.push(lines.join("\n"));
        }
        blocks
    }

    /// The number a browser shows on each visible item of the `<ol>` `id`.
    /// Counters are signed. `<ol reversed>` counts down, from the item count
    /// unless `start` says otherwise; `<li value>` renumbers from that item
    /// on.
    fn item_numbers(doc: &Doc, id: usize) -> Vec<i64> {
        let items: Vec<usize> = doc.nodes[id]
            .children
            .iter()
            .copied()
            .filter(|&c| doc.name(c) == Some("li") && !is_hidden(doc, c))
            .collect();
        let reversed = doc.attr(id, "reversed").is_some();
        let count = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let mut n: i64 = doc
            .attr(id, "start")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(if reversed { count } else { 1 });
        items
            .iter()
            .map(|&item| {
                if let Some(v) = doc.attr(item, "value").and_then(|v| v.trim().parse().ok()) {
                    n = v;
                }
                let number = n;
                n = if reversed {
                    n.saturating_sub(1)
                } else {
                    n.saturating_add(1)
                };
                number
            })
            .collect()
    }

    /// The text of each row's cells, in the columns a browser gives them.
    fn place_cells(doc: &Doc, rows_at: &[(usize, Vec<usize>)], depth: usize) -> Vec<Vec<String>> {
        // GFM has no spans: a place a `rowspan` or `colspan` covers is an
        // empty cell, so later cells keep the columns a browser gives them.
        // Placeholders are bounded by the table's own cells, so a hostile
        // span cannot blow up the output.
        let own: usize = rows_at.iter().map(|(_, row)| row.len()).sum();
        let mut budget = own.saturating_mul(8).saturating_add(64);
        // The row index each column stays covered until (exclusive).
        let mut covered_until: Vec<usize> = Vec::new();
        let mut rows = Vec::new();
        for (r, (_, row)) in rows_at.iter().enumerate() {
            let mut cells = Vec::new();
            let mut col = 0;
            for &c in row {
                while budget > 0 && covered_until.get(col).is_some_and(|&u| u > r) {
                    cells.push(String::new());
                    budget -= 1;
                    col += 1;
                }
                cells.push(
                    Self::inline_of(doc, c, depth + 1)
                        .replace('|', "\\|")
                        .replace('\n', " "),
                );
                let span = |name: &str| {
                    doc.attr(c, name)
                        .and_then(|v| v.trim().parse::<usize>().ok())
                };
                // `rowspan="0"` runs to the end of the table.
                let rowspan = match span("rowspan") {
                    Some(0) => usize::MAX,
                    Some(n) => n.min(65_534),
                    None => 1,
                };
                let colspan = span("colspan").unwrap_or(1).clamp(1, 1000);
                for k in 0..colspan {
                    if k > 0 {
                        if budget == 0 {
                            break;
                        }
                        cells.push(String::new());
                        budget -= 1;
                    }
                    if covered_until.len() <= col {
                        covered_until.resize(col + 1, 0);
                    }
                    covered_until[col] = r.saturating_add(rowspan);
                    col += 1;
                }
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        rows
    }

    fn table(doc: &Doc, id: usize, depth: usize) -> String {
        let mut stack = vec![id];
        // Each row, keyed by its first node for document order, with its
        // cells. A cell outside any `<tr>` opens a row in a browser, which
        // takes the cells after it up to the next `<tr>` or section.
        let mut rows_at: Vec<(usize, Vec<usize>)> = Vec::new();
        // Text or an element that is no table part: a browser moves it out,
        // before the table, and shows it.
        let mut foster_ids = Vec::new();
        let end_row = |implicit: &mut Vec<usize>, rows_at: &mut Vec<(usize, Vec<usize>)>| {
            if let Some(&first) = implicit.first() {
                rows_at.push((first, std::mem::take(implicit)));
            }
        };
        while let Some(n) = stack.pop() {
            let mut implicit = Vec::new();
            for &c in &doc.nodes[n].children {
                if is_hidden(doc, c) {
                    continue;
                }
                match doc.name(c) {
                    Some("td" | "th") => implicit.push(c),
                    Some("tr") => {
                        end_row(&mut implicit, &mut rows_at);
                        // In a row, anything but a cell is moved out too.
                        let (cells, other): (Vec<usize>, Vec<usize>) = doc.nodes[c]
                            .children
                            .iter()
                            .copied()
                            .filter(|&k| !is_hidden(doc, k))
                            .partition(|&k| matches!(doc.name(k), Some("td" | "th")));
                        foster_ids.extend(other);
                        rows_at.push((c, cells));
                    }
                    Some("thead" | "tbody" | "tfoot") => {
                        end_row(&mut implicit, &mut rows_at);
                        stack.push(c);
                    }
                    Some("caption" | "colgroup" | "col") => {}
                    _ => foster_ids.push(c),
                }
            }
            end_row(&mut implicit, &mut rows_at);
        }
        // Sections are read last-first; rebuild document order.
        rows_at.sort_unstable_by_key(|r| r.0);
        foster_ids.sort_unstable();
        let mut foster = Self::default();
        for c in foster_ids {
            foster.node(doc, c, depth + 1);
        }
        let foster = foster.finish().trim_end().to_owned();
        let rows = Self::place_cells(doc, &rows_at, depth);
        // A visible `<caption>` goes above the table, as its own paragraph.
        let caption = doc.nodes[id]
            .children
            .iter()
            .find(|&&c| doc.name(c) == Some("caption") && !is_hidden(doc, c))
            .map(|&c| escape_line_start(Self::inline_of(doc, c, depth + 1)))
            .filter(|c| !c.is_empty());
        let lead: Vec<String> = [Some(foster), caption]
            .into_iter()
            .flatten()
            .filter(|b| !b.is_empty())
            .collect();
        let Some(width) = rows.iter().map(Vec::len).max() else {
            return lead.join("\n\n");
        };
        let mut out: Vec<String> = lead.into_iter().flat_map(|b| [b, String::new()]).collect();
        for (k, row) in rows.iter().enumerate() {
            // Only the header row sets the column count; GFM fills a short
            // row with empty cells, so the others are not padded.
            if k == 0 {
                let mut cells = row.clone();
                cells.resize(width, String::new());
                out.push(format!("| {} |", cells.join(" | ")));
                out.push(format!("|{}", " --- |".repeat(width)));
            } else {
                out.push(format!("| {} |", row.join(" | ")));
            }
        }
        out.join("\n")
    }
}

/// The label a browser shows for item number `n` of an `<ol type>`:
/// letters (`a`, `A`) from 1 up, Roman numerals (`i`, `I`) from 1 to 3999,
/// and decimal otherwise.
fn list_label(n: i64, style: Option<&str>) -> String {
    let upper = style.is_some_and(|s| s == "A" || s == "I");
    let label = match style {
        Some("a" | "A") if n >= 1 => {
            // Bijective base 26: z, aa, ab, ...
            let mut n = n;
            let mut letters = Vec::new();
            while n > 0 {
                n -= 1;
                letters.push(b'a' + u8::try_from(n % 26).unwrap_or(0));
                n /= 26;
            }
            letters.iter().rev().map(|&b| char::from(b)).collect()
        }
        Some("i" | "I") if (1..=3999).contains(&n) => {
            const ROMAN: [(i64, &str); 13] = [
                (1000, "m"),
                (900, "cm"),
                (500, "d"),
                (400, "cd"),
                (100, "c"),
                (90, "xc"),
                (50, "l"),
                (40, "xl"),
                (10, "x"),
                (9, "ix"),
                (5, "v"),
                (4, "iv"),
                (1, "i"),
            ];
            let mut n = n;
            let mut out = String::new();
            for (value, digits) in ROMAN {
                while n >= value {
                    out.push_str(digits);
                    n -= value;
                }
            }
            out
        }
        _ => return n.to_string(),
    };
    if upper {
        label.to_ascii_uppercase()
    } else {
        label
    }
}

/// Join a list item's text and its nested list with one newline.
fn collapse_nested_list_gap(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let lines: Vec<&str> = body.split('\n').collect();
    let mut k = 0;
    while k < lines.len() {
        let l = lines[k];
        if l.is_empty() && lines.get(k + 1).is_some_and(|n| is_list_line(n)) && k > 0 {
            k += 1;
            continue;
        }
        if k > 0 {
            out.push('\n');
        }
        out.push_str(l);
        k += 1;
    }
    out
}

fn is_list_line(l: &str) -> bool {
    l.starts_with("- ")
        || l.split_once(". ")
            .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn is_hidden(doc: &Doc, id: usize) -> bool {
    doc.attr(id, "hidden").is_some()
        || doc
            .attr(id, "aria-hidden")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        // A `<dialog>` shows only while `open`.
        || (doc.name(id) == Some("dialog") && doc.attr(id, "open").is_none())
}

/// Collapse inline whitespace; keep the hard breaks that `<br>` wrote.
fn tidy_inline(s: &str) -> String {
    s.split("  \n")
        .map(collapse_ws)
        .collect::<Vec<_>>()
        .join("  \n")
        .trim_matches(|c: char| c == ' ' || c == '\n')
        .to_owned()
}

/// Escape Markdown syntax in text. `<` is escaped too, so decoded text
/// never becomes raw HTML in a Markdown renderer.
pub(super) fn escape_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '~') {
            out.push('\\');
        }
        out.push(c);
    }
    escape_entity_like(&out)
}

/// A link or image URL that is safe to write, percent-encoded for a
/// Markdown link. Relative URLs and `http`/`https` pass; `mailto` passes for
/// links. Every other scheme (`javascript:`, `data:`, ...) gives `None`.
fn safe_url(raw: &str, link: bool) -> Option<String> {
    // The URL parser trims C0 controls and spaces, then drops every tab and
    // newline, so `https://exa&#10;mple.com` is `https://example.com`.
    let url: String = raw
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    // An empty `href` links to the page itself; an empty `src` shows nothing.
    if url.is_empty() {
        return link.then(String::new);
    }
    // A character reference the decoder left (`javascript&colon;`) still
    // spells a scheme once a Markdown renderer decodes it, so an `&` before
    // the first `/`, `?` or `#` is refused.
    let head: String = url
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
        .take_while(|c| !matches!(c, '/' | '?' | '#'))
        .collect();
    if head.contains('&') {
        return None;
    }
    // Whatever comes before a `:` there is a scheme, however long.
    if let Some((scheme, _)) = head.split_once(':') {
        let scheme = scheme.to_ascii_lowercase();
        let allowed = matches!(scheme.as_str(), "http" | "https") || (link && scheme == "mailto");
        if !allowed {
            return None;
        }
    }
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            // A `\` would escape the closing `)` of the Markdown link.
            '\\' => out.push_str("%5C"),
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    let _ = write!(out, "%{b:02X}");
                }
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// Escape `&` where it starts text a renderer would read as an entity
/// (`&lt;`, `&#60;`).
fn escape_entity_like(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.char_indices() {
        if c == '&' {
            let rest = &s[i + 1..];
            let name_len = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'#')
                .count();
            if name_len > 0 && rest.as_bytes().get(name_len) == Some(&b';') {
                out.push('\\');
            }
        }
        out.push(c);
    }
    out
}

/// Escape a marker at the start of a paragraph line that Markdown would
/// read as a heading, quote, list, or rule.
fn escape_line_start(text: String) -> String {
    let first = text.chars().next();
    if matches!(first, Some('#' | '>' | '-' | '+' | '=')) {
        return format!("\\{text}");
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && matches!(text.as_bytes().get(digits), Some(b'.' | b')'))
        && text
            .as_bytes()
            .get(digits + 1)
            .is_none_or(u8::is_ascii_whitespace)
    {
        return format!("{}\\{}", &text[..digits], &text[digits..]);
    }
    text
}

/// Length of the longest run of backticks in `s`.
fn longest_backtick_run(s: &str) -> usize {
    s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        html_to_markdown(html)
    }

    #[test]
    fn headings_and_paragraphs() {
        assert_eq!(
            md("<h1>Title</h1><p>One   two\n three.</p><h2>Sub</h2><p>Four</p>"),
            "# Title\n\nOne two three.\n\n## Sub\n\nFour\n"
        );
    }

    #[test]
    fn front_matter_from_head() {
        let out = md("<!DOCTYPE html><html><head><title>Hi &amp; bye</title>\
             <meta name=\"description\" content=\"A &quot;page&quot;\"></head>\
             <body><p>Body</p></body></html>");
        assert_eq!(
            out,
            "---\ntitle: \"Hi & bye\"\ndescription: \"A \\\"page\\\"\"\n---\n\nBody\n"
        );
    }

    #[test]
    fn main_wins_over_body_chrome() {
        let out = md("<body><header><nav><a href=\"/\">Home</a></nav></header>\
             <main><h1>Post</h1><p>Text</p></main><footer>Foot</footer></body>");
        assert_eq!(out, "# Post\n\nText\n");
    }

    #[test]
    fn scripts_styles_and_hidden_content_are_dropped() {
        let out = md(
            "<p>a</p><script>var x = '<p>no</p>';</script><style>p{}</style>\
             <div hidden>gone</div><span aria-hidden=\"true\">icon</span><p>b</p>",
        );
        assert_eq!(out, "a\n\nb\n");
    }

    #[test]
    fn inline_formatting() {
        assert_eq!(
            md("<p><strong>bold</strong> <em>it</em> <code>a*b</code> \
                <a href=\"/x?a=1&amp;b=2\">link</a> <img src=\"/i.png\" alt=\"pic\"></p>"),
            "**bold** *it* `a*b` [link](/x?a=1&b=2) ![pic](/i.png)\n"
        );
    }

    #[test]
    fn text_with_markdown_syntax_is_escaped() {
        assert_eq!(md("<p>2 * 3 = [six]_</p>"), "2 \\* 3 = \\[six\\]\\_\n");
    }

    #[test]
    fn an_encoded_scheme_separator_is_not_a_safe_link() {
        assert_eq!(
            md("<p><a href=\"javascript&colon;alert(1)\">x</a></p>"),
            "x\n"
        );
        let out = md("<p>a <img src=\"data&colon;x\" alt=\"i\"> b</p>");
        assert!(!out.contains("data"), "{out}");
        // An `&` in the query is fine.
        assert_eq!(
            md("<p><a href=\"/s?a=1&amp;b=2\">s</a></p>"),
            "[s](/s?a=1&b=2)\n"
        );
    }

    #[test]
    fn javascript_links_keep_only_their_text() {
        assert_eq!(md("<p><a href=\"javascript:alert(1)\">x</a></p>"), "x\n");
    }

    #[test]
    fn lists_nest() {
        assert_eq!(
            md("<ul><li>a</li><li>b<ol><li>c</li><li>d</li></ol></li></ul>"),
            "- a\n- b\n  1. c\n  2. d\n"
        );
    }

    #[test]
    fn implied_end_tags_close_list_items_and_paragraphs() {
        assert_eq!(md("<ul><li>a<li>b</ul><p>x<p>y"), "- a\n- b\n\nx\n\ny\n");
    }

    #[test]
    fn preformatted_code_keeps_whitespace_and_language() {
        assert_eq!(
            md("<pre><code class=\"language-rust\">fn main() {\n    1 &lt; 2\n}</code></pre>"),
            "```rust\nfn main() {\n    1 < 2\n}\n```\n"
        );
    }

    #[test]
    fn blockquote_prefixes_every_line() {
        assert_eq!(
            md("<blockquote><p>a</p><p>b</p></blockquote>"),
            "> a\n>\n> b\n"
        );
    }

    #[test]
    fn tables_become_gfm() {
        assert_eq!(
            md("<table><thead><tr><th>A</th><th>B|C</th></tr></thead>\
                <tbody><tr><td>1</td><td>2</td></tr></tbody></table>"),
            "| A | B\\|C |\n| --- | --- |\n| 1 | 2 |\n"
        );
    }

    #[test]
    fn hidden_table_parts_stay_hidden() {
        assert_eq!(
            md("<table><tr><th>A</th><th hidden>secret</th></tr>\
                <tbody aria-hidden=\"true\"><tr><td>gone</td></tr></tbody>\
                <tbody><tr hidden><td>gone</td></tr><tr><td>1</td></tr></tbody></table>"),
            "| A |\n| --- |\n| 1 |\n"
        );
    }

    #[test]
    fn line_breaks_and_rules() {
        assert_eq!(md("<p>a<br>b</p><hr><p>c</p>"), "a  \nb\n\n---\n\nc\n");
    }

    #[test]
    fn entities_decode() {
        assert_eq!(
            md("<p>&lt;&gt;&amp;&#39;&#x41;&nbsp;&copy;&unknown;</p>"),
            "\\<>&'A\u{a0}©\\&unknown;\n"
        );
    }

    #[test]
    fn forms_keep_labels_and_drop_controls() {
        assert_eq!(
            md(
                "<form><label>Name</label><input name=\"n\"><button>Go</button>\
                <select><option>x</option></select><textarea>t</textarea></form>"
            ),
            "Name\n"
        );
    }

    #[test]
    fn unsafe_urls_are_dropped_and_text_cannot_become_html() {
        assert_eq!(
            md("<p><a href=\"java&#9;script:alert(1)\">a</a> \
                <a href=\"data:text/html,x\">b</a> <img src=\"data:x\" alt=\"c\"> \
                <a href=\"mailto:me@example.com\">d</a> \
                <a href=\"/x y(1)\">e</a> &lt;img src=x onerror=1&gt;</p>"),
            "a b [d](mailto:me@example.com) [e](/x%20y%281%29) \\<img src=x onerror=1>\n"
        );
    }

    #[test]
    fn paragraph_markers_and_entity_text_are_escaped() {
        assert_eq!(md("<p># of items</p>"), "\\# of items\n");
        assert_eq!(md("<p>1. Intro</p>"), "1\\. Intro\n");
        assert_eq!(md("<p>&amp;lt;</p>"), "\\&lt;\n");
        assert_eq!(md("<p>AT&amp;T</p>"), "AT&T\n");
    }

    #[test]
    fn inline_edges_keep_their_spaces() {
        assert_eq!(
            md("<p>Hello<strong> world</strong>!</p>"),
            "Hello **world**!\n"
        );
        assert_eq!(
            md("<p>See<a href=\"/x\"> this </a>now</p>"),
            "See [this](/x) now\n"
        );
        assert_eq!(md("<h2>a<br>b</h2>"), "## a b\n");
    }

    #[test]
    fn hidden_and_template_mains_are_skipped() {
        assert_eq!(md("<main hidden>secret</main><main>real</main>"), "real\n");
        assert_eq!(
            md("<template><main>tpl</main></template><main>real</main>"),
            "real\n"
        );
    }

    #[test]
    fn iframe_fallback_is_raw_text() {
        assert_eq!(
            md("<div><iframe></div><main>hidden</main></iframe><main>real</main></div>"),
            "real\n"
        );
    }

    #[test]
    fn an_omitted_head_end_tag_does_not_swallow_the_body() {
        assert_eq!(
            md("<head><title>T</title><body><main>Visible</main>"),
            "---\ntitle: \"T\"\n---\n\nVisible\n"
        );
        assert_eq!(
            md("<html><head><meta name=description content=d><p>Body text"),
            "---\ndescription: \"d\"\n---\n\nBody text\n"
        );
    }

    #[test]
    fn a_template_in_the_head_keeps_its_content() {
        assert_eq!(
            md(
                "<head><template><main>secret</main></template></head><body><main>visible</main></body>"
            ),
            "visible\n"
        );
    }

    #[test]
    fn a_kept_end_tag_closes_the_dropped_tags_inside_it() {
        let deep = "<div>".repeat(250);
        let spans = "<span>".repeat(100);
        let out = md(&format!(
            "{deep}<section hidden>{spans}secret</section><main>visible</main>"
        ));
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("visible"), "{out}");
    }

    #[test]
    fn an_input_like_tag_closes_a_select() {
        assert_eq!(md("<select><input><main>Visible</main>"), "Visible\n");
        assert_eq!(md("<select><option>a<select><p>Shown</p>"), "Shown\n");
        let deep = "<div>".repeat(300) + "<select><textarea>t</textarea>Visible";
        assert!(md(&deep).contains("Visible"));
    }

    #[test]
    fn a_stray_end_tag_cannot_leave_a_select() {
        assert_eq!(
            md("<div><select></div><main>hidden</main></select><main>real</main></div>"),
            "real\n"
        );
        assert_eq!(
            md("<div><template></div><main>hidden</main></template><main>real</main></div>"),
            "real\n"
        );
    }

    #[test]
    fn an_implied_close_ends_the_dropped_tags_inside_it() {
        let deep = "<div>".repeat(250);
        let spans = "<span>".repeat(100);
        let out = md(&format!(
            "{deep}<p>first{spans}<span hidden>secret<p>visible</p>after"
        ));
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("visible") && out.contains("after"), "{out}");
    }

    #[test]
    fn every_html_block_closes_an_open_paragraph() {
        assert_eq!(
            md("<p hidden>secret<address>Visible</address><p>After"),
            "Visible\n\nAfter\n"
        );
        for block in [
            "details",
            "fieldset",
            "figcaption",
            "hgroup",
            "menu",
            "search",
            "summary",
        ] {
            let out = md(&format!("<p hidden>secret<{block}>Visible</{block}>"));
            assert!(
                !out.contains("secret") && out.contains("Visible"),
                "{block}: {out}"
            );
        }
        // A list item closes the paragraph inside the item before it.
        let out = md("<ul><li><p>one<li>two</ul>");
        assert!(out.contains("- one") && out.contains("- two"), "{out}");
        let out = md("<p hidden>secret<li>shown");
        assert!(!out.contains("secret") && out.contains("shown"), "{out}");
    }

    #[test]
    fn plaintext_runs_to_the_end_of_the_document() {
        let out = md("<p>before</p><nav><plaintext></nav><main>secret</main>");
        assert!(out.contains("before") && !out.contains("secret"), "{out}");
        let deep = "<div>".repeat(300) + "<nav><plaintext></nav><p>secret</p>";
        assert!(!md(&deep).contains("secret"));
    }

    #[test]
    fn a_raw_text_end_tag_ends_at_its_own_close() {
        assert_eq!(
            md("<body><script>x</script x=\"<b>secret</b>\">visible</body>"),
            "visible\n"
        );
        let deep = "<div>".repeat(300) + "<script>x</script x='<b>secret</b>'>ok";
        let out = md(&deep);
        assert!(!out.contains("secret") && out.contains("ok"), "{out}");
    }

    #[test]
    fn a_marker_after_a_line_break_is_escaped() {
        assert_eq!(
            md("<p>Hello<br># not a heading<br>- not a list<br>1. not a list</p>"),
            "Hello  \n\\# not a heading  \n\\- not a list  \n1\\. not a list\n"
        );
        assert_eq!(md("<p>Hello<br> # indented</p>"), "Hello  \n\\# indented\n");
    }

    #[test]
    fn only_an_open_dialog_is_content() {
        let out = md("<main><p>Page</p><dialog open><p>Confirm?</p></dialog>\
             <dialog><p>secret</p></dialog></main>");
        assert!(out.contains("Confirm?") && !out.contains("secret"), "{out}");
        let deep = "<div>".repeat(300) + "<dialog>secret</dialog><dialog open>shown</dialog>";
        let out = md(&deep);
        assert!(out.contains("shown") && !out.contains("secret"), "{out}");
    }

    #[test]
    fn fallback_content_is_not_published() {
        let out = md("<p>shown</p><noembed>secret</noembed><noframes>secret</noframes>");
        assert_eq!(out, "shown\n");
    }

    #[test]
    fn literal_tildes_are_escaped() {
        assert_eq!(md("<p>~~not deleted~~</p>"), "\\~\\~not deleted\\~\\~\n");
        assert_eq!(md("<p>a</p><p>~~~</p><p>b</p>"), "a\n\n\\~\\~\\~\n\nb\n");
        assert_eq!(md("<p><del>gone</del></p>"), "~~gone~~\n");
    }

    #[test]
    fn an_ordered_list_keeps_its_start_number() {
        assert_eq!(
            md("<ol start=\"5\"><li>Step five</li><li>Step six</li></ol>"),
            "5. Step five\n6. Step six\n"
        );
        // A browser shows -2, which no Markdown list number can.
        assert_eq!(md("<ol start=\"-2\"><li>a</li></ol>"), "- -2\\. a\n");
        assert_eq!(md("<ol><li>a</li></ol>"), "1. a\n");
    }

    #[test]
    fn a_reversed_list_counts_down() {
        assert_eq!(
            md("<ol reversed><li>Third</li><li>Second</li><li>First</li></ol>"),
            "3. Third\n2. Second\n1. First\n"
        );
        assert_eq!(
            md("<ol reversed start=\"10\"><li>a</li><li>b</li></ol>"),
            "10. a\n9. b\n"
        );
    }

    #[test]
    fn a_code_language_with_a_backtick_is_dropped() {
        assert_eq!(
            md("<pre><code class=\"language-```\">x</code></pre><p>after</p>"),
            "```\nx\n```\n\nafter\n"
        );
        assert_eq!(
            md("<pre><code class=\"language-c++\">x</code></pre>"),
            "```c++\nx\n```\n"
        );
    }

    #[test]
    fn a_list_item_value_renumbers() {
        assert_eq!(
            md("<ol start=\"5\"><li>Five</li><li value=\"10\">Ten</li><li>Eleven</li></ol>"),
            "5. Five\n10. Ten\n11. Eleven\n"
        );
    }

    #[test]
    fn content_inside_a_table_but_outside_its_cells_is_kept() {
        assert_eq!(md("<table>Visible</table>"), "Visible\n");
        assert_eq!(
            md("<table><tr>Visible<td>42</td></tr></table>"),
            "Visible\n\n| 42 |\n| --- |\n"
        );
        assert_eq!(
            md("<table>\n  <p>Note</p>\n  <tr><td>1</td></tr>\n</table>"),
            "Note\n\n| 1 |\n| --- |\n"
        );
    }

    #[test]
    fn a_table_keeps_its_caption() {
        assert_eq!(
            md("<table><caption>Quarterly totals</caption><tr><td>42</td></tr></table>"),
            "Quarterly totals\n\n| 42 |\n| --- |\n"
        );
        assert_eq!(
            md("<table><caption hidden>gone</caption><tr><td>1</td></tr></table>"),
            "| 1 |\n| --- |\n"
        );
    }

    #[test]
    fn a_heading_closes_an_open_heading() {
        assert_eq!(md("<main><h1>One<h2>Two</h2></main>"), "# One\n\n## Two\n");
    }

    #[test]
    fn every_named_character_reference_decodes() {
        assert_eq!(md("<p>Save &frac12; today</p>"), "Save \u{bd} today\n");
        assert_eq!(
            md("<p>&NotEqualTilde;&Aacute;&zwnj;x</p>"),
            "\u{2242}\u{338}\u{c1}\u{200c}x\n"
        );
        // Not a reference: the source text stays, escaped.
        assert_eq!(md("<p>&zzz;</p>"), "\\&zzz;\n");
    }

    #[test]
    fn inline_code_keeps_the_spaces_around_it() {
        assert_eq!(md("<p>A<code> B </code>C</p>"), "A `B` C\n");
    }

    #[test]
    fn references_decode_as_in_a_browser() {
        // A legacy name needs no `;` in text.
        assert_eq!(md("<p>&copy 2026 &notit;</p>"), "\u{a9} 2026 \u{ac}it;\n");
        // C1 numbers read as Windows-1252; a `;` is optional.
        assert_eq!(
            md("<p>&#128; &#x93;q&#x94; &#169 x</p>"),
            "\u{20ac} \u{201c}q\u{201d} \u{a9} x\n"
        );
        // In an attribute, a legacy name running into `=` stays text.
        assert_eq!(
            md("<p><a href=\"/s?a=1&copy=2\">s</a></p>"),
            "[s](/s?a=1&copy=2)\n"
        );
    }

    #[test]
    fn a_list_item_closes_through_a_div() {
        assert_eq!(md("<ul><li>one<div><li>two</ul>"), "- one\n- two\n");
    }

    #[test]
    fn a_backslash_in_a_link_is_encoded() {
        assert_eq!(
            md("<p><a href=\"/docs\\\">Docs</a></p>"),
            "[Docs](/docs%5C)\n"
        );
    }

    #[test]
    fn a_menu_is_a_bullet_list() {
        assert_eq!(
            md("<menu><li>One</li><li>Two</li></menu>"),
            "- One\n- Two\n"
        );
    }

    #[test]
    fn preformatted_text_keeps_its_trailing_spaces() {
        assert_eq!(md("<pre>x </pre>"), "```\nx \n```\n");
        assert_eq!(md("<pre>\nx\n</pre>"), "```\nx\n```\n");
    }

    #[test]
    fn a_button_or_link_closes_an_open_one() {
        assert_eq!(
            md("<button>one<button>two</button><main>Visible</main>"),
            "Visible\n"
        );
        assert_eq!(
            md("<p><a href=\"/one\">one<a href=\"/two\">two</a>end</p>"),
            "[one](/one)[two](/two)end\n"
        );
        // A link in a table cell is not closed by one in the next cell.
        assert!(
            md("<table><tr><td><a href=\"/a\">a</a><td><a href=\"/b\">b</a></table>")
                .contains("[a](/a) | [b](/b)")
        );
    }

    #[test]
    fn comments_close_as_in_a_browser() {
        assert_eq!(md("<main><p>One</p><!--><p>Two</p></main>"), "One\n\nTwo\n");
        assert_eq!(
            md("<main><p>One</p><!---><p>Two</p></main>"),
            "One\n\nTwo\n"
        );
        assert_eq!(
            md("<main><p>One</p><!-- x --!><p>Two</p></main>"),
            "One\n\nTwo\n"
        );
        assert_eq!(
            md("<main><p>One</p><!-- x - -> y --><p>Two</p></main>"),
            "One\n\nTwo\n"
        );
    }

    #[test]
    fn a_bang_before_a_link_stays_text() {
        assert_eq!(md("<p>!<a href=\"/x\">x</a></p>"), "\\![x](/x)\n");
        assert_eq!(md("<p>Wow! <a href=\"/x\">x</a></p>"), "Wow! [x](/x)\n");
    }

    #[test]
    fn crlf_reads_as_lf() {
        assert_eq!(md("<pre>\r\nx\r\ny\r\n</pre>"), "```\nx\ny\n```\n");
        assert_eq!(md("<p>a\rb</p>"), "a b\n");
    }

    #[test]
    fn a_bang_after_an_escaped_backslash_is_escaped() {
        // The text `\` writes as `\\`, which leaves the `!` bare.
        assert_eq!(md("<p>\\!<a href=\"/x\">x</a></p>"), "\\\\\\![x](/x)\n");
    }

    #[test]
    fn front_matter_keeps_control_characters() {
        assert_eq!(
            md("<head><title>A&#8;B</title></head><body>x</body>"),
            "---\ntitle: \"A\\u0008B\"\n---\n\nx\n"
        );
    }

    #[test]
    fn a_long_unknown_scheme_is_refused() {
        assert_eq!(
            md("<a href=\"abcdefghijklmnopq:payload\">Open</a>"),
            "Open\n"
        );
        assert_eq!(md("<a href=\"/a/b:c\">Path</a>"), "[Path](/a/b:c)\n");
    }

    #[test]
    fn text_between_list_items_stays_in_place() {
        assert_eq!(md("<ul>Intro<li>One</li></ul>"), "Intro\n\n- One\n");
        assert_eq!(
            md("<ol>\n<li>a</li>\n<p>note</p>\n<li>b</li>\n</ol>"),
            "1. a\n\nnote\n\n2. b\n"
        );
        // Whitespace between items does not split the list.
        assert_eq!(md("<ul>\n<li>a</li>\n<li>b</li>\n</ul>"), "- a\n- b\n");
    }

    #[test]
    fn cells_outside_a_row_form_one() {
        assert_eq!(
            md("<table><td>A</td><td>B</td></table>"),
            "| A | B |\n| --- | --- |\n"
        );
        assert_eq!(
            md("<table><tbody><td>A<td>B<tr><td>C</td></tr></tbody></table>"),
            "| A | B |\n| --- | --- |\n| C |\n"
        );
    }

    #[test]
    fn content_outside_the_body_joins_it() {
        assert_eq!(md("<body>One</body>Two"), "OneTwo\n");
        assert_eq!(
            md("<html><head><title>T</title></head>Before<body><p>In</p></body></html>After"),
            "---\ntitle: \"T\"\n---\n\nBefore\n\nIn\n\nAfter\n"
        );
    }

    #[test]
    fn a_reference_in_a_url_stays_literal() {
        assert_eq!(
            md("<a href=\"/go?x=&amp;copy;&amp;y=1\">Go</a>"),
            "[Go](/go?x=\\&copy;&y=1)\n"
        );
        assert_eq!(
            md("<img src=\"/i?a=&amp;lt;\" alt=\"i\">"),
            "![i](/i?a=\\&lt;)\n"
        );
    }

    #[test]
    fn list_numbers_are_signed() {
        assert_eq!(
            md("<ol reversed start=\"1\"><li>A</li><li>B</li><li>C</li></ol>"),
            "- 1\\. A\n- 0\\. B\n- -1\\. C\n"
        );
        assert_eq!(
            md("<ol start=\"-2\"><li>A</li><li value=\"7\">B</li></ol>"),
            "- -2\\. A\n- 7\\. B\n"
        );
        assert_eq!(
            md("<ol reversed start=\"2\"><li>A</li><li>B</li></ol>"),
            "2. A\n1. B\n"
        );
        // No overflow at the top of the range.
        assert_eq!(
            md("<ol start=\"9223372036854775807\"><li>A</li><li>B</li></ol>"),
            "- 9223372036854775807\\. A\n- 9223372036854775807\\. B\n"
        );
    }

    #[test]
    fn an_empty_href_is_still_a_link() {
        assert_eq!(md("<a href=\"\">Reload</a>"), "[Reload]()\n");
        assert_eq!(md("<a href=\" \">Reload</a>"), "[Reload]()\n");
        assert_eq!(md("<p>a<a href=\"\"></a>b</p>"), "ab\n");
        assert_eq!(md("<a href=\"/tracking\"> </a><p>Body</p>"), "Body\n");
        assert_eq!(md("<img src=\"\" alt=\"x\">"), "");
    }

    #[test]
    fn links_resolve_against_the_base() {
        let page = |body: &str| {
            md(&format!(
                "<head><base href=\"https://cdn.example/assets/\"></head><body>{body}</body>"
            ))
        };
        assert_eq!(
            page("<a href=\"guide\">Guide</a>"),
            "[Guide](https://cdn.example/assets/guide)\n"
        );
        assert_eq!(
            page("<a href=\"/top\">Top</a> <img src=\"i.png\" alt=\"i\">"),
            "[Top](https://cdn.example/top) ![i](https://cdn.example/assets/i.png)\n"
        );
        assert_eq!(
            page("<a href=\"https://other.example/x\">X</a>"),
            "[X](https://other.example/x)\n"
        );
        assert_eq!(page("<a href=\"javascript:alert(1)\">J</a>"), "J\n");
        // A relative base, or one in a template, is not used.
        assert_eq!(
            md("<base href=\"/sub/\"><a href=\"guide\">Guide</a>"),
            "[Guide](guide)\n"
        );
        assert_eq!(
            md("<template><base href=\"https://x.example/\"></template><a href=\"g\">G</a>"),
            "[G](g)\n"
        );
    }

    #[test]
    fn ordered_list_types_keep_their_labels() {
        assert_eq!(
            md("<ol type=\"A\"><li>First</li><li>Second</li></ol>"),
            "- A\\. First\n- B\\. Second\n"
        );
        assert_eq!(
            md("<ol type=\"a\" start=\"26\"><li>z</li><li>aa</li></ol>"),
            "- z\\. z\n- aa\\. aa\n"
        );
        assert_eq!(
            md("<ol type=\"I\" start=\"3\"><li>c</li><li>d</li></ol>"),
            "- III\\. c\n- IV\\. d\n"
        );
        assert_eq!(
            md("<ol type=\"i\" start=\"0\"><li>z</li></ol>"),
            "- 0\\. z\n"
        );
        assert_eq!(md("<ol type=\"1\"><li>a</li></ol>"), "1. a\n");
        assert_eq!(list_label(1994, Some("i")), "mcmxciv");
    }

    #[test]
    fn spanned_cells_keep_their_columns() {
        assert_eq!(
            md("<table><tr><td rowspan=\"2\">X</td><td>Y</td></tr><tr><td>Z</td></tr></table>"),
            "| X | Y |\n| --- | --- |\n|  | Z |\n"
        );
        assert_eq!(
            md(
                "<table><tr><th colspan=\"2\">H</th><th>I</th></tr><tr><td>a</td><td>b</td><td>c</td></tr></table>"
            ),
            "| H |  | I |\n| --- | --- | --- |\n| a | b | c |\n"
        );
        // Placeholders are bounded by the table's own cells.
        let out = md(
            "<table><tr><td colspan=\"1000\" rowspan=\"0\">A</td></tr><tr><td>B</td></tr></table>",
        );
        // Two cells: 8 placeholders each and 64 spare, not 1000 columns.
        assert!(out.matches(" |").count() < 2 * (2 * 8 + 64 + 2), "{out}");
    }

    #[test]
    fn an_end_br_is_a_line_break() {
        assert_eq!(md("<p>One</br>Two</p>"), "One  \nTwo\n");
    }

    #[test]
    fn a_url_loses_its_tabs_and_newlines() {
        assert_eq!(
            md("<a href=\" https://exa&#10;mple.com/x&#9;y \">Link</a>"),
            "[Link](https://example.com/xy)\n"
        );
    }

    #[test]
    fn a_row_after_moved_out_content_stays_in_the_table() {
        assert_eq!(
            md("<table><p>Note<tr><td>1</td></tr></table>"),
            "Note\n\n| 1 |\n| --- |\n"
        );
    }

    #[test]
    fn code_blocks_skip_hidden_descendants() {
        assert_eq!(
            md("<pre><code><span hidden>secret</span>visible<script>x()</script></code></pre>"),
            "```\nvisible\n```\n"
        );
        assert_eq!(
            md("<p><code>a<span aria-hidden=\"true\">b</span>c</code></p>"),
            "`ac`\n"
        );
    }

    #[test]
    fn hidden_content_past_the_depth_limit_stays_hidden() {
        let deep = "<div>".repeat(300);
        for hidden in [
            "<div hidden>secret<img src=x alt=pic></div>",
            "<div aria-hidden=\"true\">secret</div>",
            "<nav>secret</nav>",
        ] {
            let out = md(&format!("{deep}{hidden}<p>ok</p>"));
            assert!(
                !out.contains("secret") && !out.contains("pic"),
                "{hidden}: {out}"
            );
            assert!(out.contains("ok"), "{hidden}: {out}");
        }
        // Plain content past the limit is still kept.
        assert!(md(&format!("{deep}<span>kept</span>")).contains("kept"));
    }

    #[test]
    fn raw_text_past_the_depth_limit_stays_dropped() {
        let html = "<div>".repeat(300) + "<script>var s='</div>'; secret()</script>ok";
        let out = md(&html);
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("ok"), "{out}");
    }

    #[test]
    fn front_matter_skips_hidden_metadata() {
        let out = md("<template><title>secret</title>\
             <meta name=\"description\" content=\"hidden\"></template>\
             <svg><title>icon</title></svg><iframe><title>framed</title></iframe>\
             <object><title>embedded</title></object><title>real</title>\
             <meta name=\"description\" content=\"shown\"><p>x</p>");
        assert_eq!(
            out,
            "---\ntitle: \"real\"\ndescription: \"shown\"\n---\n\nx\n"
        );
    }

    #[test]
    fn a_self_closing_html_tag_stays_open() {
        let out = md("<template/><main>hidden</main></template><main>real</main>");
        assert_eq!(out, "real\n");
        // A foreign root does close itself.
        assert_eq!(md("<p>a<svg/>b</p>"), "ab\n");
    }

    #[test]
    fn a_self_closing_raw_text_tag_still_runs_to_its_end_tag() {
        let out = md("<script/>\"<main>injected</main>\"</script><main>real</main>");
        assert_eq!(out, "real\n");
    }

    #[test]
    fn raw_text_ends_only_at_its_own_tag_name() {
        let out = md("<script>\"</scripture><main>injected</main>\"</script>\
             <main>real</main>");
        assert_eq!(out, "real\n");
        let deep = "<div>".repeat(300) + "<script>'</scripts><p>secret</p>'</SCRIPT >ok";
        let out = md(&deep);
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("ok"), "{out}");
    }

    #[test]
    fn fences_outgrow_backticks_in_code() {
        assert_eq!(md("<p><code>a``b</code></p>"), "```a``b```\n");
        assert_eq!(md("<pre>````x</pre>"), "`````\n````x\n`````\n");
    }

    #[test]
    fn unterminated_tags_parse_in_linear_time() {
        let started = std::time::Instant::now();
        let _ = md(&"<a b".repeat(200_000));
        let _ = md(&"<a b=\"".repeat(200_000));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn unmatched_end_tags_past_the_depth_limit_parse_in_linear_time() {
        // The growth rate, not a wall-clock budget: a debug build on a busy
        // runner is slow, but four times the input must take about four
        // times as long (a quadratic parse takes sixteen). Best of three
        // runs keeps a scheduler hiccup out of it.
        let time = |html: &str| {
            (0..3)
                .map(|_| {
                    let started = std::time::Instant::now();
                    let _ = md(html);
                    started.elapsed()
                })
                .min()
                .unwrap()
        };
        let cases: [fn(usize) -> String; 2] = [
            // Many dropped start tags, then many end tags that match none.
            |n| {
                format!(
                    "{}{}{}",
                    "<div>".repeat(300),
                    "<span>".repeat(n),
                    "</b>".repeat(n)
                )
            },
            // Each `<p>` asks for an implied close across every dropped span.
            |n| {
                format!(
                    "{}{}{}",
                    "<div>".repeat(300),
                    "<span>".repeat(n),
                    "<p></p>".repeat(n)
                )
            },
        ];
        for case in cases {
            let small = time(&case(5_000));
            let large = time(&case(20_000));
            assert!(large < small * 10, "{small:?} -> {large:?}");
        }
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let html = format!(
            "{}deep{}",
            "<div>".repeat(100_000),
            "</div>".repeat(100_000)
        );
        assert_eq!(md(&html), "deep\n");
    }

    #[test]
    fn malformed_input_never_panics() {
        for html in [
            "<",
            "</",
            "<p",
            "<a href=",
            "<a href=\"x",
            "&#",
            "&#x;",
            "&#99999999999;",
            "</p></div>",
            "<!--",
            "<!DOCTYPE",
            "<pre>",
            "<table><td>x",
            "<li>orphan",
            "<<<>>>",
            "<p>\u{0}</p>",
            "<p>é</p><b>ü",
        ] {
            let _ = md(html);
        }
    }

    #[test]
    fn empty_document_is_empty() {
        assert_eq!(md(""), "");
        assert_eq!(md("<html><body></body></html>"), "");
    }

    #[test]
    fn token_estimate_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
