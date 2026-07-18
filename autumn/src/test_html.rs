//! Dependency-free HTML parser and CSS-selector matcher backing the
//! structural HTML assertions on [`crate::test::TestResponse`] (issue #1147).
//!
//! Autumn renders server-side HTML with Maud + htmx. Maud is a compile-time
//! macro that can only emit well-formed, balanced markup, so the parser here
//! targets *well-formed* fragments and documents rather than implementing the
//! full HTML5 error-recovery algorithm. Crucially, parsing fragments
//! *literally* means a bare `<tr>` htmx swap is preserved — a spec-compliant
//! HTML5 tree builder would foster-parent and drop table-section elements that
//! appear outside a `<table>`, breaking assertions on partial responses.
//!
//! The supported CSS-selector subset covers the structural selectors that
//! matter for verifying rendered pages:
//!
//! - type / tag selectors (`div`, `tr`, `a`) and the universal selector `*`
//! - class selectors (`.note-row`) and id selectors (`#note-7`)
//! - attribute selectors: `[attr]`, `[attr=value]`, `[attr^=value]`,
//!   `[attr$=value]`, `[attr*=value]` (values may be quoted or bare)
//! - compound selectors (`a.link[href]`) and selector lists (`a, button`)
//! - descendant (`table tr`) and child (`tbody > tr`) combinators
//!
//! Out of scope (mirrors the issue): pseudo-classes, XPath, sibling
//! combinators, and namespaces.

use std::fmt::Write as _;

// ── DOM ────────────────────────────────────────────────────────────────────

/// A parsed HTML node: either an element or a run of text.
#[derive(Debug, Clone)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// A parsed element with a lowercased tag name, attributes, and children.
#[derive(Debug, Clone)]
pub struct Element {
    /// Lowercased tag name.
    pub tag: String,
    /// Attributes as `(lowercased-name, decoded-value)` pairs, in source order.
    pub attrs: Vec<(String, String)>,
    /// Child nodes in document order.
    pub children: Vec<Node>,
}

impl Element {
    /// Look up an attribute value by (case-insensitive) name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.attrs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The element's class list (whitespace-separated `class` attribute).
    fn has_class(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|v| v.split_whitespace().any(|c| c == class))
    }

    /// All descendant text, concatenated in document order (entities decoded).
    pub fn text(&self) -> String {
        let mut out = String::new();
        collect_text(&self.children, &mut out);
        out
    }
}

fn collect_text(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Element(el) => collect_text(&el.children, out),
        }
    }
}

/// Collapse runs of ASCII whitespace into single spaces and trim the ends, so
/// text/`assert_text` comparisons survive indentation and line-wrapping
/// changes in templates.
pub fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for word in s.split_whitespace() {
        if !first {
            out.push(' ');
        }
        out.push_str(word);
        first = false;
    }
    out
}

// ── Parser ───────────────────────────────────────────────────────────────────

/// Void elements never have children or a closing tag.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is parsed as raw text (no nested markup). This
/// covers both true raw-text elements (`script`, `style`) and *escapable*
/// raw-text / RCDATA elements (`textarea`, `title`).
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style", "textarea", "title"];

/// Escapable raw-text (RCDATA) elements: their content is not parsed as markup
/// but character references *are* decoded, exactly like ordinary element text.
/// `script`/`style` are true raw text and kept verbatim.
const ESCAPABLE_RAW_TEXT_ELEMENTS: &[&str] = &["textarea", "title"];

/// Parse an HTML fragment or document into a forest of root nodes.
pub fn parse(input: &str) -> Vec<Node> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        input,
        pos: 0,
    };
    p.parse_forest()
}

struct Parser<'a> {
    bytes: &'a [u8],
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse_forest(&mut self) -> Vec<Node> {
        let mut roots: Vec<Node> = Vec::new();
        let mut stack: Vec<Element> = Vec::new();

        while self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'<' {
                if self.try_consume_comment() || self.try_consume_bogus_decl() {
                    continue;
                }
                if self.peek_at(1) == Some(b'/') {
                    self.handle_close_tag(&mut roots, &mut stack);
                    continue;
                }
                if self.peek_at(1).is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.handle_open_tag(&mut roots, &mut stack);
                    continue;
                }
                // A stray '<' that isn't markup falls through and is treated
                // as text by the shared consumer below.
            }
            self.consume_text_until_lt(&mut roots, &mut stack);
        }

        // Auto-close any still-open elements (well-formed input won't hit this,
        // but malformed input shouldn't lose nodes).
        while let Some(el) = stack.pop() {
            attach(&mut roots, &mut stack, Node::Element(el));
        }
        roots
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn try_consume_comment(&mut self) -> bool {
        if self.input[self.pos..].starts_with("<!--") {
            if let Some(end) = self.input[self.pos + 4..].find("-->") {
                self.pos += 4 + end + 3;
            } else {
                self.pos = self.bytes.len();
            }
            true
        } else {
            false
        }
    }

    /// Consume a doctype or other `<!...>` / `<?...>` declaration.
    fn try_consume_bogus_decl(&mut self) -> bool {
        let starts =
            self.input[self.pos..].starts_with("<!") || self.input[self.pos..].starts_with("<?");
        if !starts {
            return false;
        }
        if let Some(end) = self.input[self.pos..].find('>') {
            self.pos += end + 1;
        } else {
            self.pos = self.bytes.len();
        }
        true
    }

    fn consume_text_until_lt(&mut self, roots: &mut Vec<Node>, stack: &mut [Element]) {
        let start = self.pos;
        // Always consume at least one byte to guarantee forward progress.
        self.pos += 1;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'<' {
            self.pos += 1;
        }
        let raw = &self.input[start..self.pos];
        let text = decode_entities(raw);
        attach(roots, stack, Node::Text(text));
    }

    fn handle_open_tag(&mut self, roots: &mut Vec<Node>, stack: &mut Vec<Element>) {
        self.pos += 1; // consume '<'
        let tag = self.read_name().to_ascii_lowercase();
        let attrs = self.read_attributes();
        let self_closing = self.consume_tag_end();

        let element = Element {
            tag: tag.clone(),
            attrs,
            children: Vec::new(),
        };

        if self_closing || VOID_ELEMENTS.contains(&tag.as_str()) {
            attach(roots, stack, Node::Element(element));
            return;
        }

        if RAW_TEXT_ELEMENTS.contains(&tag.as_str()) {
            let mut el = element;
            let raw = self.read_raw_text(&tag);
            if !raw.is_empty() {
                // `<title>`/`<textarea>` are escapable raw text (RCDATA): their
                // character references are decoded just like ordinary element
                // text, so `assert_text("title", "Tom & Jerry")` works against
                // Maud's escaped `<title>Tom &amp; Jerry</title>`. `<script>`/
                // `<style>` are true raw text and kept verbatim.
                let text = if ESCAPABLE_RAW_TEXT_ELEMENTS.contains(&tag.as_str()) {
                    decode_entities(&raw)
                } else {
                    raw
                };
                el.children.push(Node::Text(text));
            }
            attach(roots, stack, Node::Element(el));
            return;
        }

        stack.push(element);
    }

    fn handle_close_tag(&mut self, roots: &mut Vec<Node>, stack: &mut Vec<Element>) {
        self.pos += 2; // consume '</'
        let tag = self.read_name().to_ascii_lowercase();
        // Skip to '>'.
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'>' {
            self.pos += 1;
        }
        if self.pos < self.bytes.len() {
            self.pos += 1; // consume '>'
        }

        // Find the matching open element, closing any intervening unclosed
        // elements (well-formed input closes the top of the stack).
        if let Some(idx) = stack.iter().rposition(|e| e.tag == tag) {
            while stack.len() > idx {
                let el = stack.pop().expect("stack non-empty above idx");
                attach(roots, stack, Node::Element(el));
            }
        }
        // No matching open tag: ignore the stray close tag.
    }

    /// Read an ASCII tag/attribute name (alnum plus `-`, `_`, `:`).
    fn read_name(&mut self) -> &'a str {
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':') {
                self.pos += 1;
            } else {
                break;
            }
        }
        &self.input[start..self.pos]
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn read_attributes(&mut self) -> Vec<(String, String)> {
        let mut attrs = Vec::new();
        loop {
            self.skip_whitespace();
            match self.bytes.get(self.pos) {
                None | Some(b'>') => break,
                Some(b'/') if self.peek_at(1) == Some(b'>') => break,
                Some(b'/') => {
                    // Stray slash; skip it.
                    self.pos += 1;
                    continue;
                }
                _ => {}
            }
            let name = self.read_name();
            if name.is_empty() {
                // Not a valid attribute start; bail to avoid an infinite loop.
                self.pos += 1;
                continue;
            }
            let name = name.to_ascii_lowercase();
            self.skip_whitespace();
            let value = if self.bytes.get(self.pos) == Some(&b'=') {
                self.pos += 1; // consume '='
                self.skip_whitespace();
                self.read_attr_value()
            } else {
                // Boolean attribute: value defaults to empty string.
                String::new()
            };
            attrs.push((name, decode_entities(&value)));
        }
        attrs
    }

    fn read_attr_value(&mut self) -> String {
        if let Some(&q @ (b'"' | b'\'')) = self.bytes.get(self.pos) {
            self.pos += 1; // opening quote
            let start = self.pos;
            while self.pos < self.bytes.len() && self.bytes[self.pos] != q {
                self.pos += 1;
            }
            let value = self.input[start..self.pos].to_string();
            if self.pos < self.bytes.len() {
                self.pos += 1; // closing quote
            }
            value
        } else {
            let start = self.pos;
            while self.pos < self.bytes.len() {
                let c = self.bytes[self.pos];
                if c.is_ascii_whitespace() || c == b'>' {
                    break;
                }
                self.pos += 1;
            }
            self.input[start..self.pos].to_string()
        }
    }

    /// Consume the end of an open tag, returning whether it was self-closing.
    fn consume_tag_end(&mut self) -> bool {
        self.skip_whitespace();
        if self.bytes.get(self.pos) == Some(&b'/') && self.peek_at(1) == Some(b'>') {
            self.pos += 2;
            true
        } else {
            if self.bytes.get(self.pos) == Some(&b'>') {
                self.pos += 1;
            }
            false
        }
    }

    /// Read raw text up to the matching `</tag>` (case-insensitive).
    fn read_raw_text(&mut self, tag: &str) -> String {
        let start = self.pos;
        let needle = format!("</{tag}");
        let mut search_from = self.pos;
        loop {
            let lowered = self.input[search_from..].to_ascii_lowercase();
            let Some(rel) = lowered.find(&needle) else {
                // No closing tag: the rest of the input is raw content.
                let text = self.input[start..].to_string();
                self.pos = self.bytes.len();
                return text;
            };
            let match_pos = search_from + rel;
            // `</script` is only a real close tag when the matched name ends
            // here — i.e. the next char is `>`, `/`, whitespace, or EOF. This
            // prevents `</scripture>` inside a JS string from terminating a
            // `<script>` block early.
            let after = self.input[match_pos + needle.len()..].chars().next();
            let is_close = match after {
                None | Some('>' | '/') => true,
                Some(c) => c.is_whitespace(),
            };
            if is_close {
                let text = self.input[start..match_pos].to_string();
                self.pos = match_pos;
                // Consume the closing tag through '>'.
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'>' {
                    self.pos += 1;
                }
                if self.pos < self.bytes.len() {
                    self.pos += 1;
                }
                return text;
            }
            // False match (e.g. `</scripture`): keep scanning past it.
            search_from = match_pos + needle.len();
        }
    }
}

fn attach(roots: &mut Vec<Node>, stack: &mut [Element], node: Node) {
    if let Some(top) = stack.last_mut() {
        top.children.push(node);
    } else {
        roots.push(node);
    }
}

/// Decode the handful of HTML entities Maud emits when escaping text and
/// attribute values, plus numeric character references.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(semi) = s[i + 1..].find(';')
        {
            let entity = &s[i + 1..i + 1 + semi];
            if let Some(ch) = decode_one_entity(entity) {
                out.push(ch);
                i += 1 + semi + 1;
                continue;
            }
        }
        // Not an entity we recognize; copy the byte's char.
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn decode_one_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some('\u{a0}'),
        _ => {
            let num = entity.strip_prefix('#')?;
            let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                num.parse::<u32>().ok()?
            };
            char::from_u32(code)
        }
    }
}

const fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

// ── Selectors ────────────────────────────────────────────────────────────────

/// A compiled list of complex selectors (a comma-separated selector group).
#[derive(Debug)]
pub struct SelectorList {
    selectors: Vec<Complex>,
}

#[derive(Debug)]
struct Complex {
    /// Compound selectors in source order. `combinator` describes the
    /// relationship to the *previous* compound; the first is `None`.
    parts: Vec<(Option<Combinator>, Compound)>,
}

#[derive(Debug, Clone, Copy)]
enum Combinator {
    Descendant,
    Child,
}

#[derive(Debug, Default)]
struct Compound {
    tag: Option<String>,
    /// All id selectors in the compound. A compound with two distinct ids
    /// (`#a#b`) matches nothing, mirroring CSS, rather than silently using the
    /// last one.
    ids: Vec<String>,
    classes: Vec<String>,
    attrs: Vec<AttrPred>,
}

#[derive(Debug)]
struct AttrPred {
    name: String,
    op: AttrOp,
}

#[derive(Debug)]
enum AttrOp {
    Exists,
    Equals(String),
    Prefix(String),
    Suffix(String),
    Substring(String),
}

impl SelectorList {
    /// Parse a CSS selector string into a matchable [`SelectorList`].
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut selectors = Vec::new();
        for group in split_top_level_commas(input) {
            let group = group.trim();
            if group.is_empty() {
                return Err(format!("empty selector in `{input}`"));
            }
            selectors.push(parse_complex(group)?);
        }
        if selectors.is_empty() {
            return Err(format!("empty selector `{input}`"));
        }
        Ok(Self { selectors })
    }

    /// Return every element matching this selector list, in document order.
    pub fn matches<'a>(&self, roots: &'a [Node]) -> Vec<&'a Element> {
        let mut out = Vec::new();
        let mut ancestors: Vec<&Element> = Vec::new();
        self.walk(roots, &mut ancestors, &mut out);
        out
    }

    fn walk<'a>(
        &self,
        nodes: &'a [Node],
        ancestors: &mut Vec<&'a Element>,
        out: &mut Vec<&'a Element>,
    ) {
        for node in nodes {
            if let Node::Element(el) = node {
                if self.selectors.iter().any(|c| c.matches(el, ancestors)) {
                    out.push(el);
                }
                ancestors.push(el);
                self.walk(&el.children, ancestors, out);
                ancestors.pop();
            }
        }
    }
}

impl Complex {
    /// Match this complex selector against `el`, given its `ancestors`
    /// (root-most first, immediate parent last).
    fn matches(&self, el: &Element, ancestors: &[&Element]) -> bool {
        let n = self.parts.len();
        // The rightmost compound must match the candidate element itself.
        if !self.parts[n - 1].1.matches(el) {
            return false;
        }
        // Match the remaining compounds against the ancestor chain.
        self.match_prefix(n - 1, ancestors)
    }

    /// Match `parts[0..idx]` against `ancestors`, where `parts[idx]` has already
    /// matched the element whose ancestor chain is `ancestors`. Descendant steps
    /// backtrack across every candidate ancestor rather than greedily committing
    /// to the nearest one, so selectors like `.foo > .bar span` still match when
    /// a `.bar` is nested inside another `.bar`.
    fn match_prefix(&self, idx: usize, ancestors: &[&Element]) -> bool {
        if idx == 0 {
            return true; // all compounds matched
        }
        // `parts[idx].0` is the combinator linking `parts[idx - 1]` to `parts[idx]`.
        let combinator = self.parts[idx].0.unwrap_or(Combinator::Descendant);
        let target = &self.parts[idx - 1].1;
        match combinator {
            Combinator::Child => {
                let Some((parent, rest)) = ancestors.split_last() else {
                    return false;
                };
                target.matches(parent) && self.match_prefix(idx - 1, rest)
            }
            Combinator::Descendant => {
                // Try each ancestor from nearest to farthest, backtracking.
                (0..ancestors.len()).rev().any(|k| {
                    target.matches(ancestors[k]) && self.match_prefix(idx - 1, &ancestors[..k])
                })
            }
        }
    }
}

impl Compound {
    fn matches(&self, el: &Element) -> bool {
        if let Some(tag) = &self.tag
            && el.tag != *tag
        {
            return false;
        }
        if let Some(el_id) = el.attr("id") {
            if !self.ids.iter().all(|id| id == el_id) {
                return false;
            }
        } else if !self.ids.is_empty() {
            return false;
        }
        if !self.classes.iter().all(|c| el.has_class(c)) {
            return false;
        }
        self.attrs.iter().all(|pred| pred.matches(el))
    }
}

impl AttrPred {
    fn matches(&self, el: &Element) -> bool {
        let Some(value) = el.attr(&self.name) else {
            return false;
        };
        match &self.op {
            AttrOp::Exists => true,
            AttrOp::Equals(v) => value == v,
            AttrOp::Prefix(v) => !v.is_empty() && value.starts_with(v.as_str()),
            AttrOp::Suffix(v) => !v.is_empty() && value.ends_with(v.as_str()),
            AttrOp::Substring(v) => !v.is_empty() && value.contains(v.as_str()),
        }
    }
}

/// Split a selector list on top-level commas (ignoring commas inside `[...]`
/// or quotes).
fn split_top_level_commas(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_brackets = false;
    let mut quote: Option<char> = None;
    for c in input.chars() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    current.push(c);
                }
                '[' => {
                    in_brackets = true;
                    current.push(c);
                }
                ']' => {
                    in_brackets = false;
                    current.push(c);
                }
                ',' if !in_brackets => {
                    parts.push(std::mem::take(&mut current));
                }
                _ => current.push(c),
            },
        }
    }
    parts.push(current);
    parts
}

/// Parse a single complex selector (compounds joined by combinators).
fn parse_complex(input: &str) -> Result<Complex, String> {
    let tokens = tokenize_complex(input);
    let mut parts: Vec<(Option<Combinator>, Compound)> = Vec::new();
    // Number of `>` combinators accumulated in the current gap between
    // compounds. More than one (e.g. `div >> span`) is malformed.
    let mut child_combinators = 0usize;
    let mut seen_compound = false;

    for token in tokens {
        match token {
            ComplexToken::Child => {
                if !seen_compound {
                    return Err(format!("selector `{input}` may not start with `>`"));
                }
                child_combinators += 1;
            }
            ComplexToken::Whitespace => {}
            ComplexToken::Compound(text) => {
                let compound = parse_compound(&text, input)?;
                let combinator = if seen_compound {
                    match child_combinators {
                        0 => Some(Combinator::Descendant),
                        1 => Some(Combinator::Child),
                        _ => {
                            return Err(format!(
                                "selector `{input}` has consecutive `>` combinators"
                            ));
                        }
                    }
                } else {
                    None
                };
                parts.push((combinator, compound));
                seen_compound = true;
                child_combinators = 0;
            }
        }
    }

    if child_combinators > 0 {
        return Err(format!(
            "selector `{input}` ends with a dangling combinator"
        ));
    }
    if parts.is_empty() {
        return Err(format!("selector `{input}` has no compound selectors"));
    }
    Ok(Complex { parts })
}

enum ComplexToken {
    Compound(String),
    Child,
    Whitespace,
}

fn tokenize_complex(input: &str) -> Vec<ComplexToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_brackets = false;
    let mut quote: Option<char> = None;

    let flush = |current: &mut String, tokens: &mut Vec<ComplexToken>| {
        if !current.is_empty() {
            tokens.push(ComplexToken::Compound(std::mem::take(current)));
        }
    };

    for c in input.chars() {
        if let Some(q) = quote {
            current.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                current.push(c);
            }
            '[' => {
                in_brackets = true;
                current.push(c);
            }
            ']' => {
                in_brackets = false;
                current.push(c);
            }
            '>' if !in_brackets => {
                flush(&mut current, &mut tokens);
                tokens.push(ComplexToken::Child);
            }
            c if c.is_whitespace() && !in_brackets => {
                flush(&mut current, &mut tokens);
                tokens.push(ComplexToken::Whitespace);
            }
            _ => current.push(c),
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

/// Parse a compound selector (`a.link#main[href^="/x"]`).
fn parse_compound(input: &str, full: &str) -> Result<Compound, String> {
    let mut compound = Compound::default();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                // Universal selector: imposes no constraint.
                i += 1;
            }
            '.' => {
                i += 1;
                let name = read_ident(&chars, &mut i);
                if name.is_empty() {
                    return Err(format!("empty class in selector `{full}`"));
                }
                compound.classes.push(name);
            }
            '#' => {
                i += 1;
                let name = read_ident(&chars, &mut i);
                if name.is_empty() {
                    return Err(format!("empty id in selector `{full}`"));
                }
                compound.ids.push(name);
            }
            '[' => {
                i += 1;
                let pred = parse_attr_pred(&chars, &mut i, full)?;
                compound.attrs.push(pred);
            }
            ':' => {
                // Pseudo-classes/elements are out of scope; reject rather than
                // folding `:` into a tag/class identifier (which would let a
                // typo like `.flash:first-child` silently match `.flash`).
                return Err(format!(
                    "pseudo-classes/elements are not supported in selector `{full}`"
                ));
            }
            c if is_ident_char(c) => {
                let name = read_ident(&chars, &mut i).to_ascii_lowercase();
                compound.tag = Some(name);
            }
            other => {
                return Err(format!(
                    "unexpected character `{other}` in selector `{full}`"
                ));
            }
        }
    }
    Ok(compound)
}

fn parse_attr_pred(chars: &[char], i: &mut usize, full: &str) -> Result<AttrPred, String> {
    skip_ws(chars, i);
    let name = read_ident(chars, i).to_ascii_lowercase();
    if name.is_empty() {
        return Err(format!("empty attribute name in selector `{full}`"));
    }
    skip_ws(chars, i);
    // Determine the operator.
    let op_kind = match chars.get(*i) {
        Some(']') => {
            *i += 1;
            return Ok(AttrPred {
                name,
                op: AttrOp::Exists,
            });
        }
        Some('=') => {
            *i += 1;
            Some('=')
        }
        Some(c @ ('^' | '$' | '*')) if chars.get(*i + 1) == Some(&'=') => {
            *i += 2;
            Some(*c)
        }
        other => {
            return Err(format!(
                "unsupported attribute operator near `{other:?}` in selector `{full}`"
            ));
        }
    };
    skip_ws(chars, i);
    let value = read_attr_value(chars, i, full)?;
    skip_ws(chars, i);
    if chars.get(*i) != Some(&']') {
        return Err(format!("missing `]` in selector `{full}`"));
    }
    *i += 1;
    let op = match op_kind {
        Some('=') => AttrOp::Equals(value),
        Some('^') => AttrOp::Prefix(value),
        Some('$') => AttrOp::Suffix(value),
        Some('*') => AttrOp::Substring(value),
        _ => unreachable!("operator kind validated above"),
    };
    Ok(AttrPred { name, op })
}

fn read_attr_value(chars: &[char], i: &mut usize, full: &str) -> Result<String, String> {
    if let Some(&q @ ('"' | '\'')) = chars.get(*i) {
        *i += 1;
        let start = *i;
        while *i < chars.len() && chars[*i] != q {
            *i += 1;
        }
        if *i >= chars.len() {
            return Err(format!("unterminated quoted value in selector `{full}`"));
        }
        let value: String = chars[start..*i].iter().collect();
        *i += 1; // closing quote
        Ok(value)
    } else {
        let start = *i;
        while *i < chars.len() && chars[*i] != ']' && !chars[*i].is_whitespace() {
            *i += 1;
        }
        Ok(chars[start..*i].iter().collect())
    }
}

fn read_ident(chars: &[char], i: &mut usize) -> String {
    let start = *i;
    while *i < chars.len() && is_ident_char(chars[*i]) {
        *i += 1;
    }
    chars[start..*i].iter().collect()
}

const fn is_ident_char(c: char) -> bool {
    // Note: `:` is intentionally excluded so pseudo-classes (`tr:first-child`)
    // are rejected rather than folded into a tag/class/id name.
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

fn skip_ws(chars: &[char], i: &mut usize) {
    while *i < chars.len() && chars[*i].is_whitespace() {
        *i += 1;
    }
}

// ── Snippet rendering for failure messages ──────────────────────────────────

/// Render a compact, indented outline of the parsed tree for failure messages,
/// truncated to `max_chars` characters.
pub fn outline(roots: &[Node], max_chars: usize) -> String {
    let mut out = String::new();
    render_outline(roots, 0, &mut out);
    truncate_chars(out.trim_end(), max_chars)
}

fn render_outline(nodes: &[Node], depth: usize, out: &mut String) {
    for node in nodes {
        match node {
            Node::Element(el) => {
                for _ in 0..depth {
                    out.push_str("  ");
                }
                out.push('<');
                out.push_str(&el.tag);
                for (k, v) in &el.attrs {
                    let _ = write!(out, " {k}=\"{v}\"");
                }
                out.push_str(">\n");
                render_outline(&el.children, depth + 1, out);
            }
            Node::Text(t) => {
                let trimmed = normalize_ws(t);
                if !trimmed.is_empty() {
                    for _ in 0..depth {
                        out.push_str("  ");
                    }
                    let _ = writeln!(out, "{:?}", truncate_chars(&trimmed, 80));
                }
            }
        }
    }
}

/// Truncate a string to at most `max` characters on a char boundary.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}… ({} chars total)", s.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(html: &str, css: &str) -> usize {
        let roots = parse(html);
        SelectorList::parse(css).unwrap().matches(&roots).len()
    }

    #[test]
    fn parses_nested_elements_and_text() {
        let roots = parse("<div class='a'><p>Hello <b>world</b></p></div>");
        assert_eq!(roots.len(), 1);
        let Node::Element(div) = &roots[0] else {
            panic!("expected element");
        };
        assert_eq!(div.tag, "div");
        assert_eq!(div.attr("class"), Some("a"));
        assert_eq!(div.text(), "Hello world");
    }

    #[test]
    fn handles_void_and_self_closing_elements() {
        let roots = parse("<ul><li>one<br>still one</li><li>two</li></ul>");
        assert_eq!(
            count("<ul><li>one<br>still one</li><li>two</li></ul>", "li"),
            2
        );
        assert_eq!(count("<ul><li>one<br>two</li></ul>", "br"), 1);
        let _ = roots;
    }

    #[test]
    fn preserves_bare_table_row_fragment() {
        // The key advantage over a spec HTML5 tree builder: a bare <tr>
        // fragment (htmx swap) is not foster-parented away.
        assert_eq!(count("<tr><td>x</td></tr>", "tr"), 1);
        assert_eq!(
            count("<tr id='r1'><td><a href='/n/1'>x</a></td></tr>", "tr#r1 a"),
            1
        );
    }

    #[test]
    fn tag_class_id_attribute_selectors() {
        let html = r#"<a class="link primary" id="go" href="/notes/1" data-x="y">Go</a>"#;
        assert_eq!(count(html, "a"), 1);
        assert_eq!(count(html, ".link"), 1);
        assert_eq!(count(html, ".primary.link"), 1);
        assert_eq!(count(html, "#go"), 1);
        assert_eq!(count(html, "a#go.link"), 1);
        assert_eq!(count(html, "[href]"), 1);
        assert_eq!(count(html, "[href=\"/notes/1\"]"), 1);
        assert_eq!(count(html, "[href^=\"/notes/\"]"), 1);
        assert_eq!(count(html, "[href$=\"/1\"]"), 1);
        assert_eq!(count(html, "[href*=\"notes\"]"), 1);
        assert_eq!(count(html, "[href=\"/notes/2\"]"), 0);
        assert_eq!(count(html, ".missing"), 0);
    }

    #[test]
    fn descendant_and_child_combinators() {
        let html = "<table><tbody><tr><td><a>x</a></td></tr></tbody></table>";
        assert_eq!(count(html, "table a"), 1);
        assert_eq!(count(html, "table tr"), 1);
        assert_eq!(count(html, "tbody > tr"), 1);
        // `table > tr` is false: tbody is between them.
        assert_eq!(count(html, "table > tr"), 0);
        assert_eq!(count(html, "table > tbody > tr > td"), 1);
    }

    #[test]
    fn selector_lists() {
        let html = "<div><a>1</a><button>2</button><span>3</span></div>";
        assert_eq!(count(html, "a, button"), 2);
        assert_eq!(count(html, "a, button, span"), 3);
    }

    #[test]
    fn decodes_entities_in_text_and_attributes() {
        let roots = parse(r#"<a title="Tom &amp; Jerry">Fish &amp; Chips &#39;n&#39; peas</a>"#);
        let Node::Element(a) = &roots[0] else {
            panic!("expected element");
        };
        assert_eq!(a.attr("title"), Some("Tom & Jerry"));
        assert_eq!(a.text(), "Fish & Chips 'n' peas");
    }

    #[test]
    fn ignores_comments_and_doctype() {
        let html = "<!DOCTYPE html><!-- hi --><p>ok</p>";
        assert_eq!(count(html, "p"), 1);
        let roots = parse(html);
        assert_eq!(
            SelectorList::parse("p").unwrap().matches(&roots)[0].text(),
            "ok"
        );
    }

    #[test]
    fn raw_text_in_script_is_not_parsed_as_markup() {
        let html = "<div><script>if (a < b) { x(); }</script><p>after</p></div>";
        assert_eq!(count(html, "p"), 1);
        assert_eq!(count(html, "div p"), 1);
    }

    #[test]
    fn escapable_raw_text_decodes_entities_but_script_stays_verbatim() {
        // `<title>`/`<textarea>` are RCDATA: their entities are decoded, like
        // ordinary element text — matching Maud's escaped output.
        let title = parse("<title>Tom &amp; Jerry</title>");
        let Node::Element(el) = &title[0] else {
            panic!("expected element");
        };
        assert_eq!(el.text(), "Tom & Jerry");

        let textarea = parse("<textarea>1 &lt; 2 &amp;&amp; 3 &gt; 2</textarea>");
        let Node::Element(el) = &textarea[0] else {
            panic!("expected element");
        };
        assert_eq!(el.text(), "1 < 2 && 3 > 2");

        // `<script>` is true raw text: entities are *not* decoded.
        let script = parse("<script>var s = \"a &amp; b\";</script>");
        let Node::Element(el) = &script[0] else {
            panic!("expected element");
        };
        assert_eq!(el.text(), "var s = \"a &amp; b\";");
    }

    #[test]
    fn document_order_is_preserved() {
        let html = "<ul><li>a</li><li>b</li><li>c</li></ul>";
        let roots = parse(html);
        let texts: Vec<String> = SelectorList::parse("li")
            .unwrap()
            .matches(&roots)
            .iter()
            .map(|el| el.text())
            .collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn invalid_selectors_error() {
        assert!(SelectorList::parse("").is_err());
        assert!(SelectorList::parse("> div").is_err());
        assert!(SelectorList::parse("div >").is_err());
        assert!(SelectorList::parse("a,").is_err());
        assert!(SelectorList::parse("[href").is_err());
        // Repeated child combinators are a typo, not `div > span`.
        assert!(SelectorList::parse("div >> span").is_err());
        assert!(SelectorList::parse("div > > span").is_err());
        // Unsupported pseudo-classes must be rejected, not folded into idents.
        assert!(SelectorList::parse("tr:first-child").is_err());
        assert!(SelectorList::parse(".flash:first-child").is_err());
        assert!(SelectorList::parse("a:hover").is_err());
    }

    #[test]
    fn repeated_ids_must_match_all() {
        // A compound with two distinct ids matches nothing (it must satisfy
        // both), rather than silently using the last id.
        let html = r#"<div id="go">x</div>"#;
        assert_eq!(count(html, "#go"), 1);
        assert_eq!(count(html, "#missing#go"), 0);
        assert_eq!(count(html, "#go#go"), 1); // same id twice is satisfiable
    }

    #[test]
    fn pseudo_class_does_not_false_pass_negative_assertions() {
        // `.flash:first-child` must not silently parse as a literal class and
        // make `assert_no_selector`-style checks pass when a `.flash` exists.
        let html = r#"<div class="flash">oops</div>"#;
        assert!(SelectorList::parse(".flash:first-child").is_err());
        // The plain class selector still matches as expected.
        assert_eq!(count(html, ".flash"), 1);
    }

    #[test]
    fn descendant_match_backtracks_over_ancestors() {
        // `.bar` nested inside another `.bar` that is a direct child of `.foo`.
        // Greedy nearest-ancestor matching would commit to the inner `.bar`,
        // fail the `.foo > .bar` child step, and wrongly report no match.
        let html = r#"
            <div class="foo">
              <div class="bar">
                <div class="bar">
                  <span>target</span>
                </div>
              </div>
            </div>
        "#;
        assert_eq!(count(html, ".foo > .bar span"), 1);
        // Sanity: the child combinator itself still rejects a too-deep parent.
        assert_eq!(count(html, ".foo > span"), 0);
    }

    #[test]
    fn raw_text_close_tag_requires_name_boundary() {
        // A `</scripture>` substring inside a JS string must not terminate the
        // `<script>` block early and resume markup parsing.
        let html = r#"<div><script>var s = "</scripture><b>x</b>";</script><p>after</p></div>"#;
        // The `<b>` lives inside the script string, so it is not an element.
        assert_eq!(count(html, "b"), 0);
        // Only the real `<p>` after the (correctly closed) script is seen.
        assert_eq!(count(html, "div > p"), 1);
        let roots = parse(html);
        let scripts = SelectorList::parse("script").unwrap();
        assert_eq!(
            scripts.matches(&roots)[0].text(),
            r#"var s = "</scripture><b>x</b>";"#
        );
    }
}
