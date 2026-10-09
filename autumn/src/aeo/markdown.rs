//! HTML to Markdown, for `Accept: text/markdown` negotiation.
//!
//! The converter is small and has no dependencies. It reads the HTML that
//! Autumn pages send (Maud output, mostly well formed) and writes `CommonMark`
//! with GFM tables. It keeps content and drops chrome:
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
    let doc = parse(html);
    let mut out = String::new();
    let title = doc.find("title").map(|t| doc.text_of(t));
    let description = doc.meta_description();
    let title = title.map(|t| collapse_ws(&t)).filter(|t| !t.is_empty());
    let description = description
        .map(|d| collapse_ws(&d))
        .filter(|d| !d.is_empty());
    if title.is_some() || description.is_some() {
        out.push_str("---\n");
        if let Some(t) = &title {
            out.push_str("title: ");
            out.push_str(&yaml_quote(t));
            out.push('\n');
        }
        if let Some(d) = &description {
            out.push_str("description: ");
            out.push_str(&yaml_quote(d));
            out.push('\n');
        }
        out.push_str("---\n\n");
    }
    let root = doc
        .find("main")
        .or_else(|| doc.find("body"))
        .unwrap_or(ROOT);
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
}

impl Doc {
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

    /// First element named `name`, in document order.
    fn find(&self, name: &str) -> Option<usize> {
        (1..self.nodes.len()).find(|&id| self.name(id) == Some(name))
    }

    fn meta_description(&self) -> Option<String> {
        (1..self.nodes.len())
            .find(|&id| {
                self.name(id) == Some("meta")
                    && self
                        .attr(id, "name")
                        .is_some_and(|n| n.eq_ignore_ascii_case("description"))
            })
            .and_then(|id| self.attr(id, "content").map(str::to_owned))
    }

    /// All text below `id`, without markup.
    fn text_of(&self, id: usize) -> String {
        let mut out = String::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            match &self.nodes[n].kind {
                Kind::Text(t) => out.push_str(t),
                Kind::Element { .. } => stack.extend(self.nodes[n].children.iter().rev()),
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
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "xmp", "noscript"];

/// Start tags that close an open element of the listed names first.
fn implied_close(tag: &str) -> &'static [&'static str] {
    match tag {
        "li" => &["li"],
        "dt" | "dd" => &["dt", "dd"],
        "tr" => &["tr", "td", "th"],
        "td" | "th" => &["td", "th"],
        "option" => &["option"],
        "thead" | "tbody" | "tfoot" => &["thead", "tbody", "tfoot", "tr", "td", "th"],
        "p" | "div" | "ul" | "ol" | "table" | "pre" | "blockquote" | "h1" | "h2" | "h3" | "h4"
        | "h5" | "h6" | "hr" | "section" | "article" | "header" | "footer" | "nav" | "main"
        | "form" | "dl" | "figure" | "aside" => &["p"],
        _ => &[],
    }
}

/// Elements that stop the search for an implied close (a `<li>` inside a
/// nested `<ul>` must not close the outer `<li>`).
const SCOPE: &[&str] = &["ul", "ol", "table", "dl", "select", "blockquote", "div"];

#[allow(clippy::too_many_lines)] // one tokenizer loop; splitting it hides the state
fn parse(html: &str) -> Doc {
    let mut doc = Doc {
        nodes: vec![Node {
            kind: Kind::Element {
                name: "#root".to_owned(),
                attrs: Vec::new(),
            },
            children: Vec::new(),
        }],
    };
    let mut stack: Vec<usize> = vec![ROOT];
    // Start tags past `MAX_DEPTH` are not kept; this counts them so their end
    // tags do not close kept elements.
    let mut dropped = 0usize;
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
            let text = decode_entities(&html[i..end]);
            if !text.is_empty() {
                push(&mut doc, &stack, Kind::Text(text));
            }
            i = end;
            continue;
        }
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(bytes.len(), |n| i + n + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(bytes.len(), |n| i + n + 1);
            continue;
        }
        let Some(tag) = read_tag(rest) else {
            // A lone `<` is text.
            push(&mut doc, &stack, Kind::Text("<".to_owned()));
            i += 1;
            continue;
        };
        i += tag.len;
        if tag.end {
            if dropped > 0 {
                dropped -= 1;
                continue;
            }
            if let Some(pos) = stack
                .iter()
                .rposition(|&id| doc.name(id) == Some(tag.name.as_str()))
                && pos > 0
            {
                stack.truncate(pos);
            }
            continue;
        }

        // Implied end tags.
        let closes = implied_close(&tag.name);
        if !closes.is_empty() {
            for pos in (1..stack.len()).rev() {
                let Some(open) = doc.name(stack[pos]) else {
                    break;
                };
                if closes.contains(&open) {
                    stack.truncate(pos);
                    break;
                }
                if SCOPE.contains(&open) {
                    break;
                }
            }
        }

        let is_void = VOID.contains(&tag.name.as_str()) || tag.self_closing;
        if !is_void && stack.len() > MAX_DEPTH {
            dropped += 1;
            continue;
        }
        let raw = RAW_TEXT.contains(&tag.name.as_str()) && !tag.self_closing;
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
            let close = format!("</{name}");
            let body_end = find_ascii_ci(&html[i..], &close).map_or(bytes.len(), |n| i + n);
            let content = &html[i..body_end];
            if !content.is_empty() {
                let text = if name == "title" || name == "textarea" {
                    decode_entities(content)
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
            i = html[body_end..]
                .find('>')
                .map_or(bytes.len(), |n| body_end + n + 1);
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

/// Read one tag at the start of `s` (which starts with `<`).
fn read_tag(s: &str) -> Option<Tag> {
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
        return None;
    }
    let name = s[name_start..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i) {
            None => return None,
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
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
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
                    let close = memchr(q, &b[i + 1..])?;
                    value = decode_entities(&s[i + 1..i + 1 + close]);
                    i += close + 2;
                }
                Some(_) => {
                    let start = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                        i += 1;
                    }
                    value = decode_entities(&s[start..i]);
                }
                None => return None,
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
    Some(Tag {
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
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let semi = rest[1..]
            .find(|c: char| c == ';' || c == '&' || c.is_whitespace() || c == '<')
            .map(|n| n + 1);
        let decoded = semi
            .filter(|&n| rest.as_bytes()[n] == b';')
            .and_then(|n| decode_reference(&rest[1..n]).map(|c| (c, n + 1)));
        if let Some((c, used)) = decoded {
            out.push(c);
            rest = &rest[used..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

fn decode_reference(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        return Some(
            char::from_u32(code)
                .filter(|&c| c != '\0')
                .unwrap_or('\u{fffd}'),
        );
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "times" => '×',
        "euro" => '€',
        "pound" => '£',
        "deg" => '°',
        _ => return None,
    })
}

fn collapse_ws(s: &str) -> String {
    s.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ── Writer ──────────────────────────────────────────────────────────────────

/// Elements dropped with their content.
const DROP: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "math", "iframe", "object", "canvas",
    "nav", "button", "input", "select", "textarea", "option", "dialog", "title",
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
    "html",
    "body",
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
            self.blocks.push(text);
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
                let text = collapse_ws(&doc.text_of(id));
                if !text.is_empty() {
                    let fence = if text.contains('`') { "``" } else { "`" };
                    let _ = write!(self.line, "{fence}{text}{fence}");
                }
            }
            "a" => {
                let text = Self::inline_of(doc, id, depth);
                match doc.attr(id, "href").map(str::trim) {
                    Some(href)
                        if !href.is_empty()
                            && !href.to_ascii_lowercase().starts_with("javascript:") =>
                    {
                        let label = if text.is_empty() {
                            href.to_owned()
                        } else {
                            text
                        };
                        let _ = write!(self.line, "[{label}]({})", escape_url(href));
                    }
                    _ => self.line.push_str(&text),
                }
            }
            "img" => {
                if let Some(src) = doc.attr(id, "src").filter(|s| !s.trim().is_empty()) {
                    let alt = escape_inline(&collapse_ws(doc.attr(id, "alt").unwrap_or("")));
                    let _ = write!(self.line, "![{alt}]({})", escape_url(src.trim()));
                }
            }
            "pre" => {
                self.flush();
                let code = doc.text_of(id);
                let code = code.strip_prefix('\n').unwrap_or(&code).trim_end();
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
                    .unwrap_or("");
                let fence = if code.contains("```") { "````" } else { "```" };
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
            "ul" | "ol" => {
                self.flush();
                let list = Self::list(doc, id, depth, name == "ol");
                if !list.is_empty() {
                    self.blocks.push(list);
                }
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
        let text = Self::inline_of(doc, id, depth);
        if !text.is_empty() {
            let _ = write!(self.line, "{mark}{text}{mark}");
        }
    }

    /// Render the children of `id` as one inline string.
    fn inline_of(doc: &Doc, id: usize, depth: usize) -> String {
        let mut sub = Self::default();
        sub.children(doc, id, depth);
        sub.flush();
        sub.blocks.join(" ")
    }

    /// Render the children of `id` as finished blocks.
    fn sub_block(doc: &Doc, id: usize, depth: usize) -> String {
        let mut sub = Self::default();
        sub.children(doc, id, depth);
        sub.finish().trim_end().to_owned()
    }

    fn list(doc: &Doc, id: usize, depth: usize, ordered: bool) -> String {
        let mut lines = Vec::new();
        let mut n = 0usize;
        for &item in &doc.nodes[id].children {
            if doc.name(item) != Some("li") || is_hidden(doc, item) {
                continue;
            }
            n += 1;
            let marker = if ordered {
                format!("{n}. ")
            } else {
                "- ".to_owned()
            };
            let pad = " ".repeat(marker.len());
            // A nested list sits under the item text with no blank line.
            let body = collapse_nested_list_gap(&Self::sub_block(doc, item, depth + 1));
            for (k, l) in body.lines().enumerate() {
                if k == 0 {
                    lines.push(format!("{marker}{l}"));
                } else if l.is_empty() {
                    lines.push(String::new());
                } else {
                    lines.push(format!("{pad}{l}"));
                }
            }
            if body.is_empty() {
                lines.push(marker.trim_end().to_owned());
            }
        }
        lines.join("\n")
    }

    fn table(doc: &Doc, id: usize, depth: usize) -> String {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut stack = vec![id];
        let mut tr_ids = Vec::new();
        while let Some(n) = stack.pop() {
            for &c in doc.nodes[n].children.iter().rev() {
                match doc.name(c) {
                    Some("tr") => tr_ids.push(c),
                    Some("thead" | "tbody" | "tfoot") => stack.push(c),
                    _ => {}
                }
            }
        }
        // `stack` pops in reverse; rebuild document order.
        tr_ids.sort_unstable();
        for tr in tr_ids {
            let cells: Vec<String> = doc.nodes[tr]
                .children
                .iter()
                .filter(|&&c| matches!(doc.name(c), Some("td" | "th")))
                .map(|&c| {
                    Self::inline_of(doc, c, depth + 1)
                        .replace('|', "\\|")
                        .replace('\n', " ")
                })
                .collect();
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        let Some(width) = rows.iter().map(Vec::len).max() else {
            return String::new();
        };
        let mut out = Vec::new();
        for (k, row) in rows.iter().enumerate() {
            let mut cells = row.clone();
            cells.resize(width, String::new());
            out.push(format!("| {} |", cells.join(" | ")));
            if k == 0 {
                out.push(format!("|{}", " --- |".repeat(width)));
            }
        }
        out.join("\n")
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

fn escape_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn escape_url(url: &str) -> String {
    url.replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
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
    fn line_breaks_and_rules() {
        assert_eq!(md("<p>a<br>b</p><hr><p>c</p>"), "a  \nb\n\n---\n\nc\n");
    }

    #[test]
    fn entities_decode() {
        assert_eq!(
            md("<p>&lt;&gt;&amp;&#39;&#x41;&nbsp;&copy;&unknown;</p>"),
            "<>&'A\u{a0}©&unknown;\n"
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
    fn deep_nesting_does_not_overflow() {
        let html = "<div>".repeat(100_000) + "deep" + &"</div>".repeat(100_000);
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
