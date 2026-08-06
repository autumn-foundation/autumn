//! Lays a parsed [`Node`](super::html::Node) tree out as PDF pages.
//!
//! Deliberately **not** a CSS box-model layout engine (see [`crate::pdf`]
//! module docs for why): block elements flow top-to-bottom in a single
//! column, tables use naive equal-width columns, and styling is limited to
//! bold/italic via the built-in Helvetica font family. This is enough for
//! scaffold-shaped documents (headings, paragraphs, tables, lists) — not for
//! arbitrary CSS layouts.

use printpdf::{
    BuiltinFont, Color, Line, LinePoint, Op, PdfFontHandle, PdfPage, Point, Pt, Rgb, TextItem,
};

use super::html::Node;
use super::metrics::{char_width_1000em, text_width_pt};

/// Recursion depth cap for walking the parsed node tree — defense in depth
/// against pathologically deep (adversarial or accidental) nesting; the
/// [`super::html`] parser itself is iterative and immune to this, but this
/// layout walker recurses per nesting level for the (normally shallow)
/// element tree it receives.
const MAX_DEPTH: u32 = 512;

/// A4 portrait, matching the default most other frameworks in this space
/// (Rails' `wicked_pdf`, `WeasyPrint`) ship.
const PAGE_WIDTH_PT: f32 = 595.28;
const PAGE_HEIGHT_PT: f32 = 841.89;
const MARGIN_PT: f32 = 50.0;

const BLACK: Color = Color::Rgb(Rgb {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    icc_profile: None,
});

/// One inline run of same-styled text, or an explicit line break.
#[derive(Debug, Clone, PartialEq)]
enum Span {
    Run {
        text: String,
        bold: bool,
        italic: bool,
    },
    Break,
}

/// A single word (already whitespace-split) carrying its own style, or an
/// explicit line break — the unit [`wrap`] packs into lines.
#[derive(Debug, Clone, PartialEq)]
enum Word {
    Text {
        text: String,
        bold: bool,
        italic: bool,
        /// No whitespace separated this word from the previous one in the
        /// source HTML (e.g. `$<strong>42.00</strong>`, where "$" and
        /// "42.00" are adjacent spans with nothing between them) — render
        /// with no space before it, and never break a line between it and
        /// the previous word.
        glue: bool,
    },
    Break,
}

#[derive(Debug, Clone, PartialEq)]
struct TableRow {
    /// `(cell spans, is_header)`.
    cells: Vec<(Vec<Span>, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading(u8, Vec<Span>),
    Paragraph(Vec<Span>),
    ListItem { marker: String, spans: Vec<Span> },
    Rule,
    Table(Vec<TableRow>),
}

/// Recognized block-level tags that flush any pending implicit paragraph and
/// start a new block. Everything else (span, a, unknown tags, ...) is either
/// a recognized inline style or a transparent passthrough.
fn heading_level(tag: &str) -> Option<u8> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

/// Tags whose content is never rendered as visible text, even though the
/// generic "unrecognized tag = transparent passthrough" rule would otherwise
/// walk into them. A full server-rendered page (the natural input for
/// `Pdf::from_html` when it isn't a purpose-built Maud fragment) commonly
/// carries a `<head>` (with `<title>`/`<meta>`/`<link>`) and inline
/// `<script>`/`<style>` blocks; without this, their raw source text would be
/// emitted into the PDF ahead of (or interleaved with) the actual content.
fn is_non_rendered(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "noscript" | "template" | "head" | "title"
    )
}

/// Tags that are block-level when a browser lays them out, but that can turn
/// up *inside* a context this renderer represents as flat [`Span`]s rather
/// than nested [`Block`]s — a list item's or table cell's content
/// (`<li><p>First</p><p>Second</p></li>`, a `<td>` with multiple
/// paragraphs). `inline_spans` can't give these their own [`Block`] the way
/// `flatten_blocks` does for a top-level `<div>`, but it can still keep
/// their text from gluing directly onto whatever comes before/after by
/// inserting a line break around them — the smallest change that stops
/// `<li><p>First</p><p>Second</p></li>` from rendering as "`FirstSecond`".
fn is_block_boundary_in_inline_context(tag: &str) -> bool {
    heading_level(tag).is_some()
        || matches!(tag, "p" | "div" | "blockquote" | "li" | "dl" | "dt" | "dd")
}

/// Push a line break unless `out` is empty or already ends with one —
/// avoids emitting consecutive/leading [`Span::Break`]s when several block
/// boundaries are adjacent.
fn push_block_break(out: &mut Vec<Span>) {
    if !matches!(out.last(), None | Some(Span::Break)) {
        out.push(Span::Break);
    }
}

/// Drop a trailing [`Span::Break`] left over from [`push_block_break`]
/// wrapping the *last* nested block in a finished span list (a list item, a
/// table cell, ...) — nothing follows it, so it would only render as a
/// stray blank line.
fn trim_trailing_break(spans: &mut Vec<Span>) {
    if matches!(spans.last(), Some(Span::Break)) {
        spans.pop();
    }
}

/// Walk `nodes` collecting inline [`Span`]s, tracking bold/italic state
/// through `strong`/`b` and `em`/`i`, translating `br` to [`Span::Break`],
/// and treating any other tag (including unrecognized ones) as a transparent
/// container — so a scaffold view's wrapper `<div>`/`<span>` markup degrades
/// to its text content instead of being dropped.
fn inline_spans(nodes: &[Node], bold: bool, italic: bool, depth: u32, out: &mut Vec<Span>) {
    if depth > MAX_DEPTH {
        return;
    }
    for node in nodes {
        match node {
            Node::Text(text) => {
                if !text.is_empty() {
                    out.push(Span::Run {
                        text: text.clone(),
                        bold,
                        italic,
                    });
                }
            }
            Node::Element { tag, children } => match tag.as_str() {
                "br" => out.push(Span::Break),
                "strong" | "b" => inline_spans(children, true, italic, depth + 1, out),
                "em" | "i" => inline_spans(children, bold, true, depth + 1, out),
                _ if is_non_rendered(tag) => {}
                "ul" => {
                    push_block_break(out);
                    inline_list_items(children, false, depth + 1, out);
                    push_block_break(out);
                }
                "ol" => {
                    push_block_break(out);
                    inline_list_items(children, true, depth + 1, out);
                    push_block_break(out);
                }
                _ if is_block_boundary_in_inline_context(tag) => {
                    push_block_break(out);
                    inline_spans(children, bold, italic, depth + 1, out);
                    push_block_break(out);
                }
                _ => inline_spans(children, bold, italic, depth + 1, out),
            },
        }
    }
}

/// Like [`extract_list_items`], but emits each item's marker + content as
/// flat [`Span`]s (with a line break between items) instead of
/// [`Block::ListItem`]s. `inline_spans` can't produce nested `Block`s — it's
/// the leaf-level representation already used for a list item's or table
/// cell's own content — so a `<ul>`/`<ol>` nested inside one (e.g.
/// `<li>Parent<ul><li>Child</li></ul></li>`) used to fall through to the
/// generic transparent-wrapper case, which recursed into the inner `<li>`
/// via the same `is_block_boundary_in_inline_context` handling as a stray
/// `<p>` — a line break plus bare text, no marker, no list semantics at
/// all. Not real nested-list layout (no indentation), but keeps each item's
/// bullet/number instead of losing it — same degrade philosophy as
/// `inline_spans`'s other block-boundary handling.
fn inline_list_items(nodes: &[Node], ordered: bool, depth: u32, out: &mut Vec<Span>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut index = 0u32;
    for node in nodes {
        let Node::Element { tag, children } = node else {
            continue;
        };
        if tag != "li" {
            continue;
        }
        index += 1;
        if index > 1 {
            push_block_break(out);
        }
        let marker = if ordered {
            format!("{index}. ")
        } else {
            "\u{2022} ".to_owned()
        };
        out.push(Span::Run {
            text: marker,
            bold: false,
            italic: false,
        });
        inline_spans(children, false, false, depth + 1, out);
    }
}

fn extract_table_rows(nodes: &[Node], depth: u32, out: &mut Vec<TableRow>) {
    if depth > MAX_DEPTH {
        return;
    }
    for node in nodes {
        let Node::Element { tag, children } = node else {
            continue;
        };
        match tag.as_str() {
            "tr" => {
                let mut cells = Vec::new();
                for cell in children {
                    let Node::Element {
                        tag: cell_tag,
                        children: cell_children,
                    } = cell
                    else {
                        continue;
                    };
                    let is_header = cell_tag == "th";
                    if is_header || cell_tag == "td" {
                        let mut spans = Vec::new();
                        // `cell_children` is two levels below `tr`'s `depth`
                        // (tr -> td/th -> cell_children).
                        inline_spans(cell_children, is_header, false, depth + 2, &mut spans);
                        trim_trailing_break(&mut spans);
                        cells.push((spans, is_header));
                    }
                }
                out.push(TableRow { cells });
            }
            // Structural wrappers (thead/tbody/tfoot) — descend without
            // emitting a row themselves.
            "thead" | "tbody" | "tfoot" => extract_table_rows(children, depth + 1, out),
            _ if is_non_rendered(tag) => {}
            // Anything else inside a <table> (most commonly <caption>, or a
            // stray text-bearing tag) isn't a row — but its text must still
            // render somewhere, matching this renderer's "unknown tags pass
            // their text through transparently" contract (see module docs).
            // A single-cell row is the simplest way to surface it without a
            // dedicated non-tabular-content block type.
            _ => {
                let mut spans = Vec::new();
                inline_spans(children, false, false, depth + 1, &mut spans);
                trim_trailing_break(&mut spans);
                if !spans.is_empty() {
                    out.push(TableRow {
                        cells: vec![(spans, false)],
                    });
                }
            }
        }
    }
}

fn extract_list_items(nodes: &[Node], ordered: bool, depth: u32, out: &mut Vec<Block>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut index = 0u32;
    for node in nodes {
        let Node::Element { tag, children } = node else {
            continue;
        };
        if tag != "li" {
            continue;
        }
        index += 1;
        let marker = if ordered {
            format!("{index}.")
        } else {
            "\u{2022}".to_owned()
        };
        let mut spans = Vec::new();
        inline_spans(children, false, false, depth + 1, &mut spans);
        trim_trailing_break(&mut spans);
        out.push(Block::ListItem { marker, spans });
    }
}

/// Flatten a parsed node tree into a flow of [`Block`]s. Consecutive inline
/// content not wrapped in a block tag (bare text, `<span>`, `<strong>`, ... at
/// the top level) is collected into an implicit paragraph, matching how a
/// browser would flow loose text.
fn flatten_blocks(nodes: &[Node], depth: u32, out: &mut Vec<Block>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pending: Vec<Span> = Vec::new();
    let flush = |pending: &mut Vec<Span>, out: &mut Vec<Block>| {
        if !pending.is_empty() {
            out.push(Block::Paragraph(std::mem::take(pending)));
        }
    };

    for node in nodes {
        match node {
            Node::Text(text) => {
                // Pushed even when whitespace-only: a text node between two
                // loose inline elements (`<span>Hello</span> <span>world</span>`)
                // carries the one significant space HTML collapses runs of
                // whitespace to — dropping it here would make `words_of`
                // glue the surrounding words together with no space at all.
                // A whitespace-only span still contributes zero *words* (see
                // `words_of`), so this never emits a visible extra blank
                // line — it only preserves the separator.
                if !text.is_empty() {
                    pending.push(Span::Run {
                        text: text.clone(),
                        bold: false,
                        italic: false,
                    });
                }
            }
            Node::Element { tag, children } => {
                if let Some(level) = heading_level(tag) {
                    flush(&mut pending, out);
                    let mut spans = Vec::new();
                    inline_spans(children, true, false, depth + 1, &mut spans);
                    trim_trailing_break(&mut spans);
                    out.push(Block::Heading(level, spans));
                    continue;
                }
                match tag.as_str() {
                    // `p`/`li` cannot legally nest another block element in
                    // HTML (a nested block inside them is already malformed
                    // input), so flattening their content to one implicit
                    // paragraph is a reasonable degrade — and `li`'s normal
                    // path is `extract_list_items` below, not here; this arm
                    // only sees a stray `<li>` outside a `<ul>`/`<ol>`.
                    // `dt`/`dd` (a description list's term/value pair) are
                    // the same shape: each is its own block-level unit whose
                    // content is normally inline, so it gets its own
                    // paragraph rather than gluing onto its sibling term or
                    // value — without this, `<dl><dt>Title</dt><dd>My
                    // Post</dd>...</dl>` (as emitted by scaffold detail
                    // views, e.g. a `property_list` widget) renders as one
                    // run of unbroken text with no row boundaries at all.
                    "p" | "li" | "dt" | "dd" => {
                        flush(&mut pending, out);
                        let mut spans = Vec::new();
                        inline_spans(children, false, false, depth + 1, &mut spans);
                        trim_trailing_break(&mut spans);
                        out.push(Block::Paragraph(spans));
                    }
                    // `div`/`blockquote`/`dl` commonly wrap *other block
                    // elements* (`<div><h1>...</h1><p>...</p></div>`,
                    // `<blockquote><p>...</p></blockquote>`, a `<dl>`'s
                    // `<dt>`/`<dd>` children) — recursing through
                    // `flatten_blocks` (rather than flattening every
                    // descendant through `inline_spans` into one paragraph,
                    // which would merge a heading and two paragraphs into a
                    // single run of unbroken text) lets nested block tags
                    // still produce their own blocks. When the children are
                    // purely inline (e.g. `<div><span>hi</span></div>`),
                    // `flatten_blocks`'s own pending/flush accumulator
                    // produces exactly the same single implicit paragraph
                    // this used to build directly.
                    "div" | "blockquote" | "dl" => {
                        flush(&mut pending, out);
                        flatten_blocks(children, depth + 1, out);
                    }
                    "hr" => {
                        flush(&mut pending, out);
                        out.push(Block::Rule);
                    }
                    "table" => {
                        flush(&mut pending, out);
                        let mut rows = Vec::new();
                        extract_table_rows(children, depth + 1, &mut rows);
                        out.push(Block::Table(rows));
                    }
                    "ul" => {
                        flush(&mut pending, out);
                        extract_list_items(children, false, depth + 1, out);
                    }
                    "ol" => {
                        flush(&mut pending, out);
                        extract_list_items(children, true, depth + 1, out);
                    }
                    "br" => pending.push(Span::Break),
                    "strong" | "b" => inline_spans(children, true, false, depth + 1, &mut pending),
                    "em" | "i" => inline_spans(children, false, true, depth + 1, &mut pending),
                    _ if is_non_rendered(tag) => {}
                    // Transparent passthrough: unknown/inline wrapper tags
                    // (span, a, ...) flow their children into the current
                    // implicit paragraph rather than being dropped.
                    _ => flatten_into_pending(children, depth + 1, &mut pending, out),
                }
            }
        }
    }
    flush(&mut pending, out);
}

/// Like [`flatten_blocks`], but for a transparent inline wrapper: nested
/// block tags still start real blocks (flushing `pending` first), while
/// inline content keeps accumulating into the caller's `pending` buffer.
fn flatten_into_pending(nodes: &[Node], depth: u32, pending: &mut Vec<Span>, out: &mut Vec<Block>) {
    if depth > MAX_DEPTH {
        return;
    }
    // Reuse `flatten_blocks` by giving it a scratch buffer, then splice: if
    // it only ever produced inline text (no nested block tags fired), that
    // text lives in blocks as trailing paragraphs — simplest correct
    // approach is to just recurse the same tag-matching logic directly.
    for node in nodes {
        match node {
            Node::Text(text) => {
                // See the matching comment in `flatten_blocks` — a
                // whitespace-only text node is a significant separator
                // between loose inline elements, not noise to discard.
                if !text.is_empty() {
                    pending.push(Span::Run {
                        text: text.clone(),
                        bold: false,
                        italic: false,
                    });
                }
            }
            Node::Element { tag, children } => {
                if heading_level(tag).is_some()
                    || matches!(
                        tag.as_str(),
                        "p" | "div"
                            | "li"
                            | "blockquote"
                            | "hr"
                            | "table"
                            | "ul"
                            | "ol"
                            | "dl"
                            | "dt"
                            | "dd"
                    )
                {
                    if !pending.is_empty() {
                        out.push(Block::Paragraph(std::mem::take(pending)));
                    }
                    flatten_blocks(std::slice::from_ref(node), depth, out);
                } else {
                    match tag.as_str() {
                        "br" => pending.push(Span::Break),
                        "strong" | "b" => inline_spans(children, true, false, depth + 1, pending),
                        "em" | "i" => inline_spans(children, false, true, depth + 1, pending),
                        _ if is_non_rendered(tag) => {}
                        _ => flatten_into_pending(children, depth + 1, pending, out),
                    }
                }
            }
        }
    }
}

/// Flatten `spans` into words, splitting each run's text on whitespace and
/// tracking, per word, whether it was directly adjacent (no whitespace) to
/// the previous span's text — see [`Word::Text::glue`]. A span whose text is
/// entirely whitespace (or empty) breaks any glue run without itself
/// emitting a word.
fn words_of(spans: &[Span]) -> Vec<Word> {
    let mut words = Vec::new();
    let mut glue_next = false;
    for span in spans {
        match span {
            Span::Break => {
                words.push(Word::Break);
                glue_next = false;
            }
            Span::Run { text, bold, italic } => {
                // Must agree with the split predicate below on what counts as
                // a "real" (breakable) whitespace boundary — NBSP doesn't,
                // since it's deliberately kept *inside* the resulting token
                // rather than split off. Using the blanket `char::is_whitespace`
                // here (which NBSP also satisfies) would say a span starting/
                // ending with NBSP has a "real" separator at that edge, gluing
                // it to nothing — so `wrap` inserts its own extra plain space
                // next to a token that already renders the NBSP as one, and
                // allows a line break at a boundary the NBSP was meant to make
                // unbreakable.
                let is_breakable_ws = |c: char| c.is_whitespace() && c != '\u{00A0}';
                let starts_with_ws = text.starts_with(is_breakable_ws);
                let ends_with_ws = text.ends_with(is_breakable_ws);
                let mut emitted_any = false;
                // Split on breakable whitespace only — NBSP (`&nbsp;`,
                // decoded to U+00A0) satisfies `char::is_whitespace()` so
                // `split_whitespace()` would treat it as an ordinary word
                // separator, discarding the entire point of a *non*-breaking
                // space: it stays inside the resulting token instead, so a
                // line can never break between the words it joins (it still
                // renders as a real space — `char_width_1000em` gives it the
                // same width as a plain space — the token is just atomic).
                for (i, w) in text
                    .split(|c: char| c.is_whitespace() && c != '\u{00A0}')
                    .filter(|w| !w.is_empty())
                    .enumerate()
                {
                    words.push(Word::Text {
                        text: w.to_owned(),
                        bold: *bold,
                        italic: *italic,
                        glue: i == 0 && glue_next && !starts_with_ws,
                    });
                    emitted_any = true;
                }
                glue_next = emitted_any && !ends_with_ws;
            }
        }
    }
    words
}

/// A word already positioned within a wrapped line: `(text, bold, italic,
/// glue)`, where `glue` means "no space before this word" — see
/// [`Word::Text::glue`].
type StyledWord = (String, bool, bool, bool);

/// Split `text` into the fewest possible chunks that each fit within
/// `max_width_pt`, breaking at character boundaries (not word boundaries —
/// this is only used for a single token that's already too wide to fit on a
/// line by itself, e.g. a long URL/hash/identifier with no internal
/// whitespace to break at).
///
/// Always makes progress: a chunk always gets at least one character even if
/// that character alone exceeds `max_width_pt`, so this can't loop forever
/// on a pathologically narrow `max_width_pt`.
fn split_into_fitting_chunks(
    text: &str,
    font_size_pt: f32,
    bold: bool,
    max_width_pt: f32,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0.0f32;
    for ch in text.chars() {
        let ch_width = f32::from(char_width_1000em(ch, bold)) / 1000.0 * font_size_pt;
        if !current.is_empty() && current_width + ch_width > max_width_pt {
            chunks.push(std::mem::take(&mut current));
            current_width = 0.0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Greedily word-wrap `words` to `max_width_pt`, honoring explicit
/// [`Word::Break`]s. Each returned line is a list of [`StyledWord`]s in
/// left-to-right order; the caller positions each word itself rather than
/// this function merging same-style runs, keeping the wrapping logic simple
/// and easy to verify. A glued word is kept on the same line as the word
/// before it whenever it fits — but if it wouldn't (e.g. two large,
/// differently-styled runs immediately adjacent in the source HTML with no
/// whitespace between them), the line still breaks before it, the same as
/// an ordinary word boundary would; the only difference glue makes is that
/// no rendered space is inserted, which a line break doesn't need anyway.
///
/// A single word wider than `max_width_pt` on its own (a long URL, hash, or
/// identifier with nowhere to break) is character-wrapped via
/// [`split_into_fitting_chunks`] instead of being left to overflow the page
/// or table-cell boundary.
fn wrap(words: &[Word], max_width_pt: f32, font_size_pt: f32) -> Vec<Vec<StyledWord>> {
    let space_w = text_width_pt(" ", font_size_pt, false);
    let mut lines = Vec::new();
    let mut current: Vec<StyledWord> = Vec::new();
    let mut current_width = 0.0f32;

    for word in words {
        match word {
            Word::Break => {
                lines.push(std::mem::take(&mut current));
                current_width = 0.0;
            }
            Word::Text {
                text,
                bold,
                italic,
                glue,
            } => {
                let w = text_width_pt(text, font_size_pt, *bold);
                if w > max_width_pt && !text.is_empty() {
                    if !current.is_empty() {
                        lines.push(std::mem::take(&mut current));
                        current_width = 0.0;
                    }
                    let chunks = split_into_fitting_chunks(text, font_size_pt, *bold, max_width_pt);
                    let last = chunks.len().saturating_sub(1);
                    for (i, chunk) in chunks.into_iter().enumerate() {
                        let chunk_w = text_width_pt(&chunk, font_size_pt, *bold);
                        if i == last {
                            current_width = chunk_w;
                            current = vec![(chunk, *bold, *italic, false)];
                        } else {
                            lines.push(vec![(chunk, *bold, *italic, false)]);
                        }
                    }
                    continue;
                }
                let mut glued = *glue && !current.is_empty();
                // Glued words skip the space width (nothing renders between
                // them and the previous word) but otherwise get the same
                // fit check as any other word — a glued run that doesn't
                // fit still breaks the line, it just doesn't gain a
                // rendered space by doing so.
                let needed = if current.is_empty() || glued {
                    w
                } else {
                    w + space_w
                };
                if !current.is_empty() && current_width + needed > max_width_pt {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0.0;
                    glued = false;
                }
                current_width += if current.is_empty() || glued {
                    w
                } else {
                    w + space_w
                };
                current.push((text.clone(), *bold, *italic, glued));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

const fn builtin_font(bold: bool, italic: bool) -> BuiltinFont {
    match (bold, italic) {
        (false, false) => BuiltinFont::Helvetica,
        (true, false) => BuiltinFont::HelveticaBold,
        (false, true) => BuiltinFont::HelveticaOblique,
        (true, true) => BuiltinFont::HelveticaBoldOblique,
    }
}

/// Accumulates [`Op`]s across pages, handling page breaks.
struct Writer {
    pages: Vec<PdfPage>,
    ops: Vec<Op>,
    /// Distance in points from the top margin down to the current baseline.
    y_from_top: f32,
    content_width: f32,
}

impl Writer {
    fn new() -> Self {
        Self {
            pages: Vec::new(),
            ops: Vec::new(),
            y_from_top: 0.0,
            content_width: (-2.0f32).mul_add(MARGIN_PT, PAGE_WIDTH_PT),
        }
    }

    /// PDF y (from the bottom-left origin) for the current cursor.
    fn cursor_y_pt(&self) -> f32 {
        PAGE_HEIGHT_PT - MARGIN_PT - self.y_from_top
    }

    fn ensure_space(&mut self, height_needed: f32) {
        let max_y = (-2.0f32).mul_add(MARGIN_PT, PAGE_HEIGHT_PT);
        if self.y_from_top + height_needed > max_y && self.y_from_top > 0.0 {
            self.new_page();
        }
    }

    fn new_page(&mut self) {
        let ops = std::mem::take(&mut self.ops);
        self.pages.push(PdfPage::new(
            Pt(PAGE_WIDTH_PT).into(),
            Pt(PAGE_HEIGHT_PT).into(),
            ops,
        ));
        self.y_from_top = 0.0;
    }

    /// Draw one word at an explicit x offset (from the left margin) on the
    /// current line.
    fn draw_word(&mut self, x_from_left: f32, text: &str, bold: bool, italic: bool, size: f32) {
        self.ops.push(Op::StartTextSection);
        self.ops.push(Op::SetFont {
            font: PdfFontHandle::Builtin(builtin_font(bold, italic)),
            size: Pt(size),
        });
        self.ops.push(Op::SetFillColor { col: BLACK });
        self.ops.push(Op::SetTextCursor {
            pos: Point {
                x: Pt(MARGIN_PT + x_from_left),
                y: Pt(self.cursor_y_pt()),
            },
        });
        self.ops.push(Op::ShowText {
            items: vec![TextItem::Text(text.to_owned())],
        });
        self.ops.push(Op::EndTextSection);
    }

    /// Render `lines` (as produced by [`wrap`]) starting at `x_offset` from
    /// the left margin, within `width`, advancing the cursor by one
    /// `line_height` per line.
    ///
    /// `break_pages` controls whether this may itself trigger a page break
    /// per line: pass `true` for ordinary top-level flow (paragraphs,
    /// headings, list items), and `false` when called once per *column*
    /// from [`draw_table`](Self::draw_table) — there, the row as a whole
    /// already had its space reserved up front (see that method), and a
    /// page break triggered by one column midway through would flush the
    /// page and reset the cursor to the top of the new one, but the caller's
    /// saved `y_from_top` for the *next* column would then be stale (from
    /// the old, already-flushed page), corrupting that column's vertical
    /// position. Not breaking here just lets a single row that's taller
    /// than a whole page overflow past the bottom margin instead — visually
    /// imperfect, but not a page-break/coordinate-corrupting bug.
    fn draw_lines(
        &mut self,
        lines: &[Vec<StyledWord>],
        x_offset: f32,
        font_size: f32,
        line_height: f32,
        break_pages: bool,
    ) {
        let space_w = text_width_pt(" ", font_size, false);
        for line in lines {
            if break_pages {
                self.ensure_space(line_height);
            }
            let mut x = x_offset;
            let mut first = true;
            for (text, bold, italic, glue) in line {
                if !first && !glue {
                    x += space_w;
                }
                self.draw_word(x, text, *bold, *italic, font_size);
                x += text_width_pt(text, font_size, *bold);
                first = false;
            }
            self.y_from_top += line_height;
        }
    }

    fn draw_spans(&mut self, spans: &[Span], font_size: f32, line_height: f32, space_after: f32) {
        let words = words_of(spans);
        if words.is_empty() {
            return;
        }
        let lines = wrap(&words, self.content_width, font_size);
        self.draw_lines(&lines, 0.0, font_size, line_height, true);
        self.y_from_top += space_after;
    }

    fn draw_rule(&mut self) {
        self.ensure_space(14.0);
        let y = self.cursor_y_pt() - 4.0;
        self.ops.push(Op::SetOutlineColor { col: BLACK });
        self.ops.push(Op::SetOutlineThickness { pt: Pt(0.75) });
        self.ops.push(Op::DrawLine {
            line: Line {
                points: vec![
                    LinePoint {
                        p: Point {
                            x: Pt(MARGIN_PT),
                            y: Pt(y),
                        },
                        bezier: false,
                    },
                    LinePoint {
                        p: Point {
                            x: Pt(MARGIN_PT + self.content_width),
                            y: Pt(y),
                        },
                        bezier: false,
                    },
                ],
                is_closed: false,
            },
        });
        self.y_from_top += 14.0;
    }

    /// Draw `rows` as a naive equal-width-column table.
    ///
    /// Known limitation: a single row is never split across a page
    /// boundary — `ensure_space(row_height)` below reserves room for the
    /// *whole* row up front, and if `row_height` alone exceeds a full page
    /// (e.g. one cell wraps to dozens of lines of a long description), that
    /// reservation is a no-op (see [`ensure_space`](Self::ensure_space)) and
    /// [`draw_lines`](Self::draw_lines) is deliberately told not to page-break
    /// mid-column (`break_pages: false`, see its docs) to avoid corrupting
    /// later columns' position. The row's content past the bottom margin is
    /// then clipped — present in the source and in `extract_text`'s output,
    /// but not visible in the rendered PDF. Splitting one oversized row
    /// across pages with all columns advancing in lockstep is a real
    /// layout-engine feature this deliberately-simple renderer doesn't
    /// attempt (see the module docs on scope); tables sized for realistic
    /// scaffold content (invoice line items, a handful of columns) never
    /// approach this limit.
    // Column/row counts are bounded by how many cells a template author
    // writes into one table (never remotely close to f32's 24-bit mantissa),
    // so the usize/f32 conversions below can't meaningfully lose precision.
    #[allow(clippy::cast_precision_loss)]
    fn draw_table(&mut self, rows: &[TableRow]) {
        const FONT_SIZE: f32 = 10.5;
        const LINE_HEIGHT: f32 = 14.0;
        const CELL_PADDING: f32 = 4.0;

        let n_cols = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
        if n_cols == 0 {
            return;
        }
        let col_width = self.content_width / n_cols as f32;

        for row in rows {
            let wrapped: Vec<Vec<Vec<StyledWord>>> = row
                .cells
                .iter()
                .map(|(spans, _)| wrap(&words_of(spans), col_width - CELL_PADDING, FONT_SIZE))
                .collect();
            let row_lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let row_height = row_lines as f32 * LINE_HEIGHT;
            self.ensure_space(row_height);
            for (col, lines) in wrapped.iter().enumerate() {
                let x_offset = col as f32 * col_width;
                let saved_y = self.y_from_top;
                self.draw_lines(lines, x_offset, FONT_SIZE, LINE_HEIGHT, false);
                self.y_from_top = saved_y;
            }
            self.y_from_top += row_height;
        }
        self.y_from_top += 6.0;
    }

    fn draw_block(&mut self, block: &Block) {
        match block {
            Block::Heading(level, spans) => {
                let size = match level {
                    1 => 22.0,
                    2 => 18.0,
                    3 => 16.0,
                    4 => 14.0,
                    5 => 12.5,
                    _ => 11.5,
                };
                self.draw_spans(spans, size, size * 1.3, size * 0.5);
            }
            Block::Paragraph(spans) => {
                self.draw_spans(spans, 11.0, 14.5, 10.0);
            }
            Block::ListItem { marker, spans } => {
                const INDENT: f32 = 16.0;
                self.ensure_space(14.5);
                self.draw_word(0.0, marker, false, false, 11.0);
                let words = words_of(spans);
                let lines = wrap(&words, self.content_width - INDENT, 11.0);
                self.draw_lines(&lines, INDENT, 11.0, 14.5, true);
                self.y_from_top += 4.0;
            }
            Block::Rule => self.draw_rule(),
            Block::Table(rows) => self.draw_table(rows),
        }
    }

    fn finish(mut self) -> Vec<PdfPage> {
        // Always emit at least one (possibly empty) page.
        if self.pages.is_empty() || self.y_from_top > 0.0 || !self.ops.is_empty() {
            self.new_page();
        }
        self.pages
    }
}

/// Render a parsed HTML-subset document as one or more [`PdfPage`]s.
pub(super) fn render_pages(html: &str) -> Vec<PdfPage> {
    let nodes = super::html::parse(html);
    let mut blocks = Vec::new();
    flatten_blocks(&nodes, 0, &mut blocks);

    let mut writer = Writer::new();
    for block in &blocks {
        writer.draw_block(block);
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_long_text_into_multiple_lines() {
        let words = words_of(&[Span::Run {
            text: "the quick brown fox jumps over the lazy dog".to_owned(),
            bold: false,
            italic: false,
        }]);
        let lines = wrap(&words, 80.0, 12.0);
        assert!(lines.len() > 1, "expected wrapping at a narrow width");
        for line in &lines {
            let width: f32 = line
                .iter()
                .map(|(t, b, _, _)| text_width_pt(t, 12.0, *b))
                .sum();
            assert!(width <= 80.0 + 1.0, "line exceeds max width: {width}");
        }
    }

    #[test]
    fn oversized_single_token_is_character_wrapped_not_overflowed() {
        // Regression: a single word wider than the whole column/page (a long
        // URL, hash, or identifier with no whitespace to break at) used to
        // be placed on its own line unsplit, overflowing past the boundary.
        let words = words_of(&[Span::Run {
            text: "https://example.com/a/very/long/path/that/has/no/spaces/anywhere/at/all"
                .to_owned(),
            bold: false,
            italic: false,
        }]);
        let lines = wrap(&words, 80.0, 12.0);
        assert!(
            lines.len() > 1,
            "expected the token to be split across lines"
        );
        for line in &lines {
            let width: f32 = line
                .iter()
                .map(|(t, b, _, _)| text_width_pt(t, 12.0, *b))
                .sum();
            assert!(width <= 80.0 + 1.0, "line exceeds max width: {width}");
        }
        let reassembled: String = lines
            .iter()
            .flat_map(|line| line.iter().map(|(t, ..)| t.as_str()))
            .collect();
        assert_eq!(
            reassembled, "https://example.com/a/very/long/path/that/has/no/spaces/anywhere/at/all",
            "splitting must not drop or reorder any characters"
        );
    }

    #[test]
    fn oversized_token_narrower_than_max_width_is_left_whole() {
        // A word that fits on its own line (even if it wouldn't fit
        // alongside other content already on the current line) must not be
        // needlessly split.
        let words = words_of(&[Span::Run {
            text: "short".to_owned(),
            bold: false,
            italic: false,
        }]);
        let lines = wrap(&words, 80.0, 12.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].0, "short");
    }

    #[test]
    fn wrap_honors_explicit_break() {
        let words = vec![
            Word::Text {
                text: "a".to_owned(),
                bold: false,
                italic: false,
                glue: false,
            },
            Word::Break,
            Word::Text {
                text: "b".to_owned(),
                bold: false,
                italic: false,
                glue: false,
            },
        ];
        let lines = wrap(&words, 1000.0, 12.0);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn non_breaking_space_keeps_its_words_on_one_line() {
        // Regression: `&nbsp;` decodes to U+00A0, which satisfies
        // `char::is_whitespace()` — `split_whitespace()` treated it as an
        // ordinary word separator, discarding its entire point (a line must
        // never break between the words it joins). `words_of` must keep an
        // NBSP-joined run as a single atomic token instead.
        let words = words_of(&[Span::Run {
            text: "Invoice\u{00A0}#42".to_owned(),
            bold: false,
            italic: false,
        }]);
        assert_eq!(
            words,
            vec![Word::Text {
                text: "Invoice\u{00A0}#42".to_owned(),
                bold: false,
                italic: false,
                glue: false,
            }],
            "NBSP must not split the run into two breakable words"
        );
        // Even at a width that fits neither word comfortably alongside the
        // other, the pair must stay on one line — same as any other single
        // token, just with a real (not zero-width) space rendered in it.
        let narrow_width = text_width_pt("Invoice\u{00A0}#42", 12.0, false) + 1.0;
        let lines = wrap(&words, narrow_width, 12.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].0, "Invoice\u{00A0}#42");
    }

    #[test]
    fn non_breaking_space_leading_a_styled_span_still_glues_to_the_previous_word() {
        // Regression: `Hello<strong>&nbsp;world</strong>` — the leading NBSP
        // stays inside the second span's token (`"\u{00A0}world"`, per the
        // fix above), but `starts_with_ws`/`ends_with_ws` used the blanket
        // `char::is_whitespace()` predicate, which NBSP also satisfies. That
        // treated the span boundary as a "real" separator and set `glue:
        // false` — so `wrap` would insert its own plain space next to a
        // token that already renders the NBSP as one (a visible double
        // space), and would allow a line break exactly where the NBSP was
        // meant to forbid one.
        let words = words_of(&[
            Span::Run {
                text: "Hello".to_owned(),
                bold: false,
                italic: false,
            },
            Span::Run {
                text: "\u{00A0}world".to_owned(),
                bold: true,
                italic: false,
            },
        ]);
        assert_eq!(
            words,
            vec![
                Word::Text {
                    text: "Hello".to_owned(),
                    bold: false,
                    italic: false,
                    glue: false,
                },
                Word::Text {
                    text: "\u{00A0}world".to_owned(),
                    bold: true,
                    italic: false,
                    glue: true,
                },
            ],
            "the NBSP-led word must glue to the previous word, not add a second separator"
        );
    }

    #[test]
    fn glued_run_that_cannot_fit_still_breaks_the_line() {
        // Regression: two adjacently-styled runs with no whitespace between
        // them (e.g. `<strong>...</strong><em>...</em>`) were always kept on
        // one line regardless of size, because the fit check was skipped
        // entirely for glued words — each individually fit under
        // `max_width_pt`, but their combined width could run to nearly
        // double it, clipping the second run past the column/page boundary.
        let words = vec![
            Word::Text {
                text: "WWWW".to_owned(),
                bold: false,
                italic: false,
                glue: false,
            },
            Word::Text {
                text: "WWWW".to_owned(),
                bold: false,
                italic: false,
                glue: true,
            },
        ];
        let max_width_pt = 50.0;
        let word_width = text_width_pt("WWWW", 12.0, false);
        assert!(
            word_width <= max_width_pt,
            "fixture word must fit alone on a line"
        );
        assert!(
            word_width * 2.0 > max_width_pt,
            "fixture pair must not fit together on one line"
        );
        let lines = wrap(&words, max_width_pt, 12.0);
        assert_eq!(
            lines.len(),
            2,
            "the glued word must move to its own line rather than overflow"
        );
        assert_eq!(lines[0], vec![("WWWW".to_owned(), false, false, false)]);
        assert_eq!(
            lines[1],
            vec![("WWWW".to_owned(), false, false, false)],
            "the word that moved to a new line is no longer glued to anything on it"
        );
    }

    #[test]
    fn adjacent_spans_with_no_whitespace_render_with_no_space_between() {
        // Regression: "$" and a separately-styled "42.00" right next to it
        // (e.g. `$<strong>42.00</strong>`, this feature's own flagship
        // money-formatting example) used to always get a space inserted
        // between them by word-based layout, rendering "$ 42.00".
        let words = words_of(&[
            Span::Run {
                text: "$".to_owned(),
                bold: false,
                italic: false,
            },
            Span::Run {
                text: "42.00".to_owned(),
                bold: true,
                italic: false,
            },
        ]);
        let lines = wrap(&words, 1000.0, 12.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            vec![
                ("$".to_owned(), false, false, false),
                ("42.00".to_owned(), true, false, true),
            ]
        );
    }

    #[test]
    fn spans_separated_by_whitespace_still_get_a_space() {
        let words = words_of(&[
            Span::Run {
                text: "Total:".to_owned(),
                bold: false,
                italic: false,
            },
            Span::Run {
                text: " ".to_owned(),
                bold: false,
                italic: false,
            },
            Span::Run {
                text: "$42.00".to_owned(),
                bold: true,
                italic: false,
            },
        ]);
        let lines = wrap(&words, 1000.0, 12.0);
        assert_eq!(
            lines[0],
            vec![
                ("Total:".to_owned(), false, false, false),
                ("$42.00".to_owned(), true, false, false),
            ]
        );
    }

    #[test]
    fn flatten_blocks_groups_bare_text_as_implicit_paragraph() {
        let nodes = super::super::html::parse("hello <strong>world</strong>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Paragraph(_)));
    }

    #[test]
    fn flatten_blocks_recognizes_headings_paragraphs_and_tables() {
        let nodes = super::super::html::parse(
            "<h1>Invoice</h1><p>Hello</p><table><tr><th>A</th></tr><tr><td>1</td></tr></table>",
        );
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], Block::Heading(1, _)));
        assert!(matches!(&blocks[1], Block::Paragraph(_)));
        assert!(matches!(&blocks[2], Block::Table(rows) if rows.len() == 2));
    }

    #[test]
    fn table_caption_text_is_not_silently_dropped() {
        // Regression: `<caption>` (or any non-row table child) matched the
        // `extract_table_rows` catch-all with no fallback, discarding its
        // text — contradicting this renderer's "unknown tags still render
        // their text" contract.
        let nodes = super::super::html::parse(
            "<table><caption>Grand Total</caption><tr><td>1</td></tr></table>",
        );
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        let Block::Table(rows) = &blocks[0] else {
            panic!("expected a table block")
        };
        assert_eq!(rows.len(), 2, "caption becomes an extra row, not lost");
        let caption_text: String = rows[0]
            .cells
            .iter()
            .flat_map(|(spans, _)| spans)
            .map(|s| match s {
                Span::Run { text, .. } => text.clone(),
                Span::Break => String::new(),
            })
            .collect();
        assert_eq!(caption_text, "Grand Total");
    }

    #[test]
    fn unknown_wrapper_tags_pass_through_transparently() {
        let nodes = super::super::html::parse(r#"<div class="card"><span>hi</span></div>"#);
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "hi".to_owned(), bold: false, italic: false,
            }])
        );
    }

    #[test]
    fn div_wrapper_preserves_nested_block_structure() {
        // Regression: `<div>` used to flatten every descendant through
        // `inline_spans` into one paragraph, merging a heading and two
        // paragraphs into a single unbroken run of text ("TitleFirstSecond")
        // — exactly the "one div wraps the whole page body" shape a typical
        // Maud layout function produces.
        let nodes = super::super::html::parse("<div><h1>Title</h1><p>First</p><p>Second</p></div>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(
            blocks.len(),
            3,
            "expected 3 separate blocks, got {blocks:?}"
        );
        assert!(
            matches!(&blocks[0], Block::Heading(1, spans) if spans == &[Span::Run {
                text: "Title".to_owned(), bold: true, italic: false,
            }])
        );
        assert!(
            matches!(&blocks[1], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "First".to_owned(), bold: false, italic: false,
            }])
        );
        assert!(
            matches!(&blocks[2], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Second".to_owned(), bold: false, italic: false,
            }])
        );
    }

    #[test]
    fn blockquote_wrapper_preserves_nested_paragraph() {
        let nodes = super::super::html::parse("<blockquote><p>Quote text</p></blockquote>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Quote text".to_owned(), bold: false, italic: false,
            }])
        );
    }

    #[test]
    fn list_item_with_nested_paragraphs_keeps_them_separate() {
        // Regression: `<li>`'s content goes through `inline_spans`, which
        // had no notion of a block boundary — `<li><p>First</p><p>Second</p></li>`
        // rendered "FirstSecond" with no separator at all (worse than plain
        // whitespace collapsing: there wasn't even a space).
        let nodes = super::super::html::parse("<ul><li><p>First</p><p>Second</p></li></ul>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        let Block::ListItem { spans, .. } = &blocks[0] else {
            panic!("expected a list item block");
        };
        assert_eq!(
            spans,
            &[
                Span::Run {
                    text: "First".to_owned(),
                    bold: false,
                    italic: false,
                },
                Span::Break,
                Span::Run {
                    text: "Second".to_owned(),
                    bold: false,
                    italic: false,
                },
            ],
            "nested paragraphs must be line-break separated, with no trailing break"
        );
    }

    #[test]
    fn nested_list_inside_a_list_item_keeps_its_markers() {
        // Regression: `<li>`'s content goes through `inline_spans`, which had
        // no explicit handling for a nested `<ul>`/`<ol>` — it fell through
        // to the generic transparent-wrapper case, so `<ul><li>Parent<ul><li>Child</li></ul></li></ul>`
        // reduced the inner `<li>` to a bare line break plus text, with no
        // bullet and no list semantics at all.
        let nodes = super::super::html::parse("<ul><li>Parent<ul><li>Child</li></ul></li></ul>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        let Block::ListItem { marker, spans } = &blocks[0] else {
            panic!("expected a list item block");
        };
        assert_eq!(marker, "\u{2022}");
        assert_eq!(
            spans,
            &[
                Span::Run {
                    text: "Parent".to_owned(),
                    bold: false,
                    italic: false,
                },
                Span::Break,
                Span::Run {
                    text: "\u{2022} ".to_owned(),
                    bold: false,
                    italic: false,
                },
                Span::Run {
                    text: "Child".to_owned(),
                    bold: false,
                    italic: false,
                },
            ],
            "the nested item must keep its own bullet marker instead of losing all list semantics"
        );
    }

    #[test]
    fn table_cell_with_nested_paragraphs_keeps_them_separate() {
        // Same bug as `list_item_with_nested_paragraphs_keeps_them_separate`,
        // reported for `<td>`/`<th>` cell content.
        let nodes = super::super::html::parse("<table><tr><td><p>A</p><p>B</p></td></tr></table>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        let Block::Table(rows) = &blocks[0] else {
            panic!("expected a table block");
        };
        assert_eq!(rows.len(), 1);
        let (spans, is_header) = &rows[0].cells[0];
        assert!(!is_header);
        assert_eq!(
            spans,
            &[
                Span::Run {
                    text: "A".to_owned(),
                    bold: false,
                    italic: false,
                },
                Span::Break,
                Span::Run {
                    text: "B".to_owned(),
                    bold: false,
                    italic: false,
                },
            ],
            "nested paragraphs inside a cell must be line-break separated, with no trailing break"
        );
    }

    #[test]
    fn description_list_terms_and_values_keep_their_own_blocks() {
        // Regression: `<dl>`/`<dt>`/`<dd>` (as emitted by scaffold detail
        // views — e.g. a `property_list` widget) fell through the generic
        // "unknown tag = transparent passthrough" rule with no block
        // separation at all, so `<dl><dt>Title</dt><dd>My Post</dd>
        // <dt>Published</dt><dd>true</dd></dl>` rendered as one glued run,
        // "TitleMy PostPublishedtrue", instead of four separate rows.
        let nodes = super::super::html::parse(
            "<dl><dt>Title</dt><dd>My Post</dd><dt>Published</dt><dd>true</dd></dl>",
        );
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        let texts: Vec<String> = blocks
            .iter()
            .map(|block| {
                let Block::Paragraph(spans) = block else {
                    panic!("expected a paragraph block, got {block:?}")
                };
                spans
                    .iter()
                    .map(|span| match span {
                        Span::Run { text, .. } => text.as_str(),
                        Span::Break => "",
                    })
                    .collect()
            })
            .collect();
        assert_eq!(texts, vec!["Title", "My Post", "Published", "true"]);
    }

    #[test]
    fn description_list_inside_a_transparent_wrapper_still_keeps_blocks_separate() {
        // Regression: `flatten_into_pending` (the path a `<dl>` takes when
        // nested inside an unrecognized transparent wrapper, e.g.
        // `<span><dl>...</dl></span>`) keeps its own separate block-tag
        // list rather than sharing `flatten_blocks`'s — it was missed when
        // `dl`/`dt`/`dd` were added there, so this path still glued terms
        // and values together despite the top-level fix.
        let nodes =
            super::super::html::parse("<span><dl><dt>Title</dt><dd>My Post</dd></dl></span>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        let texts: Vec<String> = blocks
            .iter()
            .map(|block| {
                let Block::Paragraph(spans) = block else {
                    panic!("expected a paragraph block, got {block:?}")
                };
                spans
                    .iter()
                    .map(|span| match span {
                        Span::Run { text, .. } => text.as_str(),
                        Span::Break => "",
                    })
                    .collect()
            })
            .collect();
        assert_eq!(texts, vec!["Title", "My Post"]);
    }

    #[test]
    fn whitespace_between_loose_inline_elements_is_not_dropped() {
        // Regression: a whitespace-only text node separating two loose
        // inline elements used to be filtered out entirely (treated the
        // same as insignificant whitespace between block tags), so
        // `words_of` never saw a boundary and glued the two words together
        // ("Helloworld" instead of "Hello world").
        let nodes = super::super::html::parse("<span>Hello</span> <span>world</span>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!("expected a paragraph block")
        };
        let words = words_of(spans);
        assert_eq!(
            words,
            vec![
                Word::Text {
                    text: "Hello".to_owned(),
                    bold: false,
                    italic: false,
                    glue: false,
                },
                Word::Text {
                    text: "world".to_owned(),
                    bold: false,
                    italic: false,
                    glue: false,
                },
            ],
            "the space between the two spans must survive as a real word boundary"
        );
    }

    #[test]
    fn script_and_style_content_is_never_rendered() {
        // Regression: `<script>`/`<style>` (and `<head>`/`<title>`) matched
        // the generic "unrecognized tag = transparent passthrough" rule,
        // so a full server-rendered page's inline CSS/JS source text was
        // emitted into the PDF as visible content.
        let nodes = super::super::html::parse(
            "<head><title>Ignored</title><style>body { color: red; }</style></head>\
             <script>alert('hi');</script><p>Visible</p>",
        );
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Visible".to_owned(), bold: false, italic: false,
            }])
        );
    }

    #[test]
    fn render_pages_produces_at_least_one_page_for_empty_input() {
        let pages = render_pages("");
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn deeply_nested_wrapper_tags_do_not_overflow_the_stack() {
        let mut html = String::new();
        for _ in 0..50_000 {
            html.push_str("<span>");
        }
        html.push_str("hi");
        for _ in 0..50_000 {
            html.push_str("</span>");
        }
        // Must not panic/overflow; content beyond MAX_DEPTH is allowed to be
        // dropped (defense-in-depth against adversarial input), so this only
        // asserts it completes and still produces at least one page.
        let pages = render_pages(&html);
        assert!(!pages.is_empty());
    }

    #[test]
    fn render_pages_paginates_long_content() {
        use std::fmt::Write as _;

        let mut html = String::new();
        for i in 0..200 {
            let _ = write!(html, "<p>Line number {i}</p>");
        }
        let pages = render_pages(&html);
        assert!(pages.len() > 1, "expected multiple pages for long content");
    }
}
