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
use super::metrics::text_width_pt;

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
                _ => inline_spans(children, bold, italic, depth + 1, out),
            },
        }
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
                        inline_spans(cell_children, is_header, false, depth + 1, &mut spans);
                        cells.push((spans, is_header));
                    }
                }
                out.push(TableRow { cells });
            }
            // Structural wrappers (thead/tbody/tfoot) — descend without
            // emitting a row themselves.
            "thead" | "tbody" | "tfoot" => extract_table_rows(children, depth + 1, out),
            _ => {}
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
                if !text.trim().is_empty() {
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
                    out.push(Block::Heading(level, spans));
                    continue;
                }
                match tag.as_str() {
                    "p" | "div" | "li" | "blockquote" => {
                        flush(&mut pending, out);
                        let mut spans = Vec::new();
                        inline_spans(children, false, false, depth + 1, &mut spans);
                        out.push(Block::Paragraph(spans));
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
                if !text.trim().is_empty() {
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
                        "p" | "div" | "li" | "blockquote" | "hr" | "table" | "ul" | "ol"
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
                        _ => flatten_into_pending(children, depth + 1, pending, out),
                    }
                }
            }
        }
    }
}

fn words_of(spans: &[Span]) -> Vec<Word> {
    let mut words = Vec::new();
    for span in spans {
        match span {
            Span::Break => words.push(Word::Break),
            Span::Run { text, bold, italic } => {
                for w in text.split_whitespace() {
                    words.push(Word::Text {
                        text: w.to_owned(),
                        bold: *bold,
                        italic: *italic,
                    });
                }
            }
        }
    }
    words
}

/// Greedily word-wrap `words` to `max_width_pt`, honoring explicit
/// [`Word::Break`]s. Each returned line is a list of `(text, bold, italic)`
/// words in left-to-right order; the caller positions each word itself
/// rather than this function merging same-style runs, keeping the wrapping
/// logic simple and easy to verify.
fn wrap(words: &[Word], max_width_pt: f32, font_size_pt: f32) -> Vec<Vec<(String, bool, bool)>> {
    let space_w = text_width_pt(" ", font_size_pt, false);
    let mut lines = Vec::new();
    let mut current: Vec<(String, bool, bool)> = Vec::new();
    let mut current_width = 0.0f32;

    for word in words {
        match word {
            Word::Break => {
                lines.push(std::mem::take(&mut current));
                current_width = 0.0;
            }
            Word::Text { text, bold, italic } => {
                let w = text_width_pt(text, font_size_pt, *bold);
                let needed = if current.is_empty() { w } else { w + space_w };
                if !current.is_empty() && current_width + needed > max_width_pt {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0.0;
                }
                current_width += if current.is_empty() { w } else { w + space_w };
                current.push((text.clone(), *bold, *italic));
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
    /// `line_height` per line. Returns the number of lines rendered.
    fn draw_lines(
        &mut self,
        lines: &[Vec<(String, bool, bool)>],
        x_offset: f32,
        font_size: f32,
        line_height: f32,
    ) -> usize {
        let space_w = text_width_pt(" ", font_size, false);
        for line in lines {
            self.ensure_space(line_height);
            let mut x = x_offset;
            for (text, bold, italic) in line {
                self.draw_word(x, text, *bold, *italic, font_size);
                x += text_width_pt(text, font_size, *bold) + space_w;
            }
            self.y_from_top += line_height;
        }
        lines.len()
    }

    fn draw_spans(&mut self, spans: &[Span], font_size: f32, line_height: f32, space_after: f32) {
        let words = words_of(spans);
        if words.is_empty() {
            return;
        }
        let lines = wrap(&words, self.content_width, font_size);
        self.draw_lines(&lines, 0.0, font_size, line_height);
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
            let wrapped: Vec<Vec<Vec<(String, bool, bool)>>> = row
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
                self.draw_lines(lines, x_offset, FONT_SIZE, LINE_HEIGHT);
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
                self.draw_lines(&lines, INDENT, 11.0, 14.5);
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
                .map(|(t, b, _)| text_width_pt(t, 12.0, *b))
                .sum();
            assert!(width <= 80.0 + 1.0, "line exceeds max width: {width}");
        }
    }

    #[test]
    fn wrap_honors_explicit_break() {
        let words = vec![
            Word::Text {
                text: "a".to_owned(),
                bold: false,
                italic: false,
            },
            Word::Break,
            Word::Text {
                text: "b".to_owned(),
                bold: false,
                italic: false,
            },
        ];
        let lines = wrap(&words, 1000.0, 12.0);
        assert_eq!(lines.len(), 2);
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
