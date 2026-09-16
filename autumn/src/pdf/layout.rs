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

thread_local! {
    /// Set when a walker drops a subtree with a visible mark in it because
    /// it passed [`MAX_DEPTH`] — a capped subtree of transparent wrapper
    /// tags with nothing inside drops nothing, so that case leaves this
    /// unset (see [`subtree_has_visible_content`]). [`render_pages`] clears
    /// the flag at the start of every call and reads it at the end, so one
    /// deep document logs one warning, not one per node.
    static DEPTH_CAP_HIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True if `nodes`, or anything nested inside them, would visibly affect
/// the rendered page: real text, or a tag that acts as a structural break
/// point purely by being present, regardless of its own content — `<br>`,
/// `<ul>`, `<ol>`, and every [`is_block_boundary_in_inline_context`] tag
/// (`<hr>`, `<li>`, `<div>`, `<p>`, ...).
///
/// Each of those forces a split even when it is completely empty:
/// `flatten_into_pending` flushes whatever text came before it into its
/// own paragraph the instant it sees one (before handing the tag itself
/// to [`flatten_blocks`], which adds nothing further for a childless one),
/// and `inline_spans` pushes a `Span::Break` around it. So an empty
/// `<li>`/`<div>`/... sitting between two runs of real text keeps them on
/// separate lines instead of gluing them together — dropping it is a real
/// rendering difference even though it drops no content of its own.
///
/// This can warn even in the rarer case where a content-free tag like
/// this has no real siblings around it to separate, so dropping it truly
/// changes nothing — telling those two cases apart would need this check
/// to see outside `nodes` (the siblings around wherever the tag would
/// have gone), which it deliberately does not do. That trade-off is
/// intentional: an occasional extra warning on an edge case that turns
/// out to be harmless costs far less than silently missing a real one,
/// which is the whole reason this signal exists (issue #2801).
///
/// Does not look inside [`is_non_rendered`] tags (`<script>`, `<style>`,
/// ...), whose content the renderer never draws regardless of depth.
///
/// Walks with an explicit stack, not recursion: a capped subtree can be
/// arbitrarily deep (that is the whole reason it got capped), so this must
/// stay stack-safe the same way [`super::html`]'s parser and `Node`'s own
/// `Drop` do.
/// `glue_risk` is true when the buffer this subtree would have appended
/// to already ends in a real word with nothing separating it yet (see
/// [`ends_with_glueable_word`]) — in that case, even whitespace-only text
/// counts as visible, because dropping it is what would let `words_of`
/// glue that word to whatever comes after. Pass `false` when no such
/// buffer exists yet (a fresh, empty one has nothing to glue to).
fn subtree_has_visible_content(nodes: &[Node], glue_risk: bool) -> bool {
    let mut stack: Vec<&Node> = nodes.iter().collect();
    let mut has_whitespace_only_text = false;
    while let Some(node) = stack.pop() {
        match node {
            Node::Text(text) => {
                // words_of splits on breakable whitespace and drops the
                // pieces it produces, so a run of nothing but breakable
                // whitespace (plain spaces, newlines) never becomes a word
                // and draw_spans never draws or advances for it — only a
                // char that survives that split (an ordinary character, or
                // a non-breaking space, which is whitespace by Unicode's
                // definition but still renders as its own word) means this
                // text is really visible on its own.
                if text.chars().any(|c| !is_breakable_whitespace(c)) {
                    return true;
                }
                if !text.is_empty() {
                    has_whitespace_only_text = true;
                }
            }
            Node::Element { tag, children } => {
                if tag == "br"
                    || tag == "ul"
                    || tag == "ol"
                    || is_block_boundary_in_inline_context(tag)
                {
                    return true;
                }
                if !is_non_rendered(tag) {
                    stack.extend(children);
                }
            }
        }
    }
    glue_risk && has_whitespace_only_text
}

/// True if `nodes` has a direct `<li>` child.
///
/// For [`extract_list_items`]/[`inline_list_items`]'s own depth-cap guard,
/// not [`subtree_has_visible_content`]: both walkers skip every node that
/// isn't a direct `<li>` — including whitespace text between `<ul>`/`<ol>`
/// and its first item — so nothing else in `nodes` ever draws anything.
/// One flat scan, no recursion: a list's items are never nested inside a
/// wrapper tag (real HTML or not — [`extract_list_items`]'s own loop would
/// skip a wrapper, not look inside it), so this never needs to.
fn nodes_contain_an_li(nodes: &[Node]) -> bool {
    nodes
        .iter()
        .any(|node| matches!(node, Node::Element { tag, .. } if tag == "li"))
}

/// True if `nodes`, or anything nested inside them, would leave a mark in
/// a fresh, empty `Vec<Span>` run through `inline_spans` — skipping
/// [`is_non_rendered`] subtrees, same as [`subtree_has_visible_content`].
///
/// Unlike that function, a structural tag (`<div>`, `<p>`, ...) does *not*
/// count on its own here: `extract_table_rows`'s catch-all arm builds this
/// exact `children` list into a fresh, empty buffer, so `push_block_break`'s
/// "skip a leading/trailing break" rule empties out any break a
/// content-free structural tag would otherwise add (unlike `inline_spans`'s
/// *other* callers, which hand it a buffer that already has real siblings'
/// text in it). Only two things survive that: actual text, and a `<li>`'s
/// marker — [`inline_list_items`] pushes that as a real `Span::Run`, not a
/// `Span::Break`, for any `<li>` that is a direct child of a `<ul>`/`<ol>`,
/// even a completely empty one (see
/// `empty_li_past_the_depth_cap_still_warns`), so it is never trimmed away.
fn subtree_has_nonempty_text(nodes: &[Node]) -> bool {
    let mut stack: Vec<&Node> = nodes.iter().collect();
    let mut br_count = 0u32;
    while let Some(node) = stack.pop() {
        match node {
            Node::Text(text) => {
                if !text.is_empty() {
                    return true;
                }
            }
            Node::Element { tag, children } => {
                if tag == "ul" || tag == "ol" {
                    // inline_list_items scans only direct <li> children
                    // and ignores everything else without recursing into
                    // it, so a listless <ul>/<ol>'s descendants (stray
                    // text, nested elements) never reach the page —
                    // don't scan past this node either way.
                    if nodes_contain_an_li(children) {
                        return true;
                    }
                    continue;
                }
                // Two or more <br> tags survive trim_trailing_break (it
                // pops only the last one), leaving a real Span::Break that
                // draws a row. <br> pushes its break directly, bypassing
                // push_block_break's "no two in a row" suppression that
                // every other break-producing tag goes through, so a
                // plain occurrence count is enough here regardless of
                // order or nesting.
                if tag == "br" {
                    br_count += 1;
                    if br_count >= 2 {
                        return true;
                    }
                }
                if !is_non_rendered(tag) {
                    stack.extend(children);
                }
            }
        }
    }
    false
}

/// True if `nodes` has something [`extract_table_rows`] would act on: a
/// `<tr>` with at least one `<td>`/`<th>` cell, a `<thead>`/`<tbody>`/`<tfoot>`
/// with such a `<tr>` inside it, any other non-[`is_non_rendered`] tag with
/// real text in it (the catch-all arm turns that into a one-cell row) — or,
/// when `table_has_other_populated_row` is true, *any* `<tr>` at all, even
/// an empty one.
///
/// A cell-less `<tr>` pushes a zero-cell row, and on its own
/// [`Writer::draw_table`] draws nothing for it (every row's cell count is
/// 0, so the table is empty end to end) — but once some *other* row in the
/// same table has a real cell, `draw_table`'s `n_cols` is already nonzero,
/// and every row from then on, including a zero-cell one, still consumes a
/// line and shifts everything after it. `table_has_other_populated_row`
/// carries that fact in from [`extract_table_rows`]'s own depth-cap guard,
/// which is the only caller — this predicate has no way to see the rest of
/// the table itself.
///
/// For `extract_table_rows`'s own depth-cap guard, not
/// [`subtree_has_visible_content`]: that walker's loop skips every bare
/// text node outright (`let Node::Element { .. } = node else { continue
/// };`), so whitespace between `<table>` and `</table>` never produces a
/// row on its own — same shape of gap as [`nodes_contain_an_li`] fixes for
/// the list walkers.
fn nodes_contain_table_output(nodes: &[Node], table_has_other_populated_row: bool) -> bool {
    let mut stack: Vec<&Node> = nodes.iter().collect();
    while let Some(node) = stack.pop() {
        let Node::Element { tag, children } = node else {
            continue;
        };
        match tag.as_str() {
            "tr" => {
                if table_has_other_populated_row
                    || children.iter().any(
                        |cell| matches!(cell, Node::Element { tag, .. } if tag == "td" || tag == "th"),
                    )
                {
                    return true;
                }
            }
            // The HTML parser auto-closes a <thead>/<tbody>/<tfoot> when
            // another one of the three opens (see html::implicitly_closes),
            // so real markup can never nest them — pushed onto the same
            // stack as everything else purely for defense in depth, the
            // same reason the whole walk is iterative rather than
            // recursive.
            "thead" | "tbody" | "tfoot" => stack.extend(children),
            _ if is_non_rendered(tag) => {}
            _ => {
                if subtree_has_nonempty_text(children) {
                    return true;
                }
            }
        }
    }
    false
}

/// True if `node` — via the exact same dispatch [`extract_table_rows`]
/// itself uses — would ever push a [`TableRow`] with at least one cell:
/// a `<tr>` with a real `<td>`/`<th>`, recursively through
/// `<thead>`/`<tbody>`/`<tfoot>` (the only tags it descends into without
/// emitting a row of its own), or its catch-all arm turning any other
/// tag's real text (most commonly `<caption>`) into a one-cell row. Any of
/// these push [`Writer::draw_table`]'s `n_cols` above zero. Mirrors
/// [`nodes_contain_table_output`]'s own catch-all arm, which needs the
/// exact same [`subtree_has_nonempty_text`] check for the exact same
/// reason. Walks with an explicit stack for the same stack-safety reason
/// as [`subtree_has_visible_content`].
fn node_contains_a_populated_row(node: &Node) -> bool {
    let mut stack: Vec<&Node> = vec![node];
    while let Some(node) = stack.pop() {
        let Node::Element { tag, children } = node else {
            continue;
        };
        match tag.as_str() {
            "tr" => {
                if children.iter().any(
                    |cell| matches!(cell, Node::Element { tag, .. } if tag == "td" || tag == "th"),
                ) {
                    return true;
                }
            }
            "thead" | "tbody" | "tfoot" => stack.extend(children),
            _ if is_non_rendered(tag) => {}
            _ => {
                if subtree_has_nonempty_text(children) {
                    return true;
                }
            }
        }
    }
    false
}

/// For every position in `nodes`, whether a later `<tr>` — at this level,
/// or inherited from an ancestor's remaining siblings via `has_more_after`
/// — has a real cell. One backward pass builds the whole array, for the
/// same O(n)-not-O(n²) reason [`GlueContext`] exists:
/// [`extract_table_rows`] threads this through its own thead/tbody/tfoot
/// recursion once per node list
/// instead of rescanning the remaining siblings on every iteration.
fn populated_row_after_each(nodes: &[Node], has_more_after: bool) -> Vec<bool> {
    let mut after = vec![has_more_after; nodes.len() + 1];
    for i in (0..nodes.len()).rev() {
        after[i] = node_contains_a_populated_row(&nodes[i]) || after[i + 1];
    }
    after
}

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
        /// with no space before it. [`wrap`] still breaks a line here if the
        /// glued pair doesn't fit together (see `unbreakable` below for the
        /// one case where it must not).
        glue: bool,
        /// A literal NBSP (`&nbsp;`) sits at this glue boundary — trailing
        /// on the previous span's text, or leading on this one. Unlike
        /// ordinary `glue` (two adjacent spans with nothing between them,
        /// where a line break is an acceptable fallback if they don't fit),
        /// an NBSP is the source HTML explicitly asking for these two words
        /// to never separate across a line break — so [`wrap`] must move
        /// this word *and* the one it's glued to together, rather than
        /// breaking between them on overflow. Always implies `glue`.
        unbreakable: bool,
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
        || matches!(
            tag,
            "p" | "div"
                | "blockquote"
                | "li"
                | "dl"
                | "dt"
                | "dd"
                | "section"
                | "article"
                | "main"
                | "header"
                | "footer"
                | "nav"
                | "aside"
                // A nested `<table>` (e.g. `<td><table>...</table></td>`) has
                // no dedicated `Block::Table` path here — `extract_table_rows`
                // only runs on a *top-level* table — so without this, its
                // `table`/`tr`/`td`/`th` structure fell through to the
                // generic transparent-wrapper case and glued adjacent cells'
                // text directly together (`<td>A</td><td>B</td>` rendering
                // as "AB"). Not a real nested table (no grid/borders), but
                // keeps each cell's content from merging into its neighbor.
                | "table"
                | "thead"
                | "tbody"
                | "tfoot"
                | "tr"
                | "td"
                | "th"
                // `<hr>` (e.g. `<li>Before<hr>After</li>`) is a void
                // element (no children to recurse into), and this context
                // has no `Block::Rule` to give it the way `flatten_blocks`
                // does for a top-level `<hr>` — but it still needs to keep
                // "Before" and "After" from gluing into "BeforeAfter". A
                // line break is the closest flat-span equivalent of a rule.
                | "hr"
        )
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
///
/// `has_more_after` resolves to true if `out` will get more content, from
/// this call or an ancestor's remaining siblings, once this call returns —
/// see [`ends_with_glueable_word`]. A depth-cap guard needs both ends: a
/// real word already in `out` with nothing separating it yet (before), and
/// something still to come that could glue onto it (after) — checked
/// before-first, since resolving `has_more_after` can require a scan and
/// `ends_with_glueable_word` never does. Pass `&GlueContext::Resolved(false)`
/// for a call that starts a fresh buffer (a heading, table cell, or list
/// item's own `spans`) — nothing outside it can ever glue to its content.
fn inline_spans(
    nodes: &[Node],
    bold: bool,
    italic: bool,
    depth: u32,
    has_more_after: &GlueContext,
    out: &mut Vec<Span>,
) {
    if depth > MAX_DEPTH {
        if subtree_has_visible_content(
            nodes,
            ends_with_glueable_word(out) && has_more_after.resolve(),
        ) {
            DEPTH_CAP_HIT.with(|hit| hit.set(true));
        }
        return;
    }
    for (i, node) in nodes.iter().enumerate() {
        let more_after = GlueContext::LaterSiblings {
            siblings: &nodes[i + 1..],
            ancestor: has_more_after,
        };
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
                "strong" | "b" => {
                    inline_spans(children, true, italic, depth + 1, &more_after, out);
                }
                "em" | "i" => {
                    inline_spans(children, bold, true, depth + 1, &more_after, out);
                }
                _ if is_non_rendered(tag) => {}
                "ul" => {
                    push_block_break(out);
                    inline_list_items(children, false, bold, italic, depth + 1, out);
                    push_block_break(out);
                }
                "ol" => {
                    push_block_break(out);
                    inline_list_items(children, true, bold, italic, depth + 1, out);
                    push_block_break(out);
                }
                _ if is_block_boundary_in_inline_context(tag) => {
                    // Unlike the transparent cases above, this tag's own
                    // trailing push_block_break (right below) unconditionally
                    // separates its content from whatever follows it out
                    // here — so that later content can never glue to
                    // anything inside, regardless of what more_after says.
                    push_block_break(out);
                    inline_spans(
                        children,
                        bold,
                        italic,
                        depth + 1,
                        &GlueContext::Resolved(false),
                        out,
                    );
                    push_block_break(out);
                }
                _ => inline_spans(children, bold, italic, depth + 1, &more_after, out),
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
///
/// `bold`/`italic` are the ambient style `inline_spans` was already
/// carrying at the point it found this `<ul>`/`<ol>` (e.g. `true` for a
/// list nested inside a `<th>`, which `inline_spans` starts bold) — applied
/// to both the marker and, via a plain pass-through to the recursive
/// `inline_spans` call below, each item's own content, exactly like
/// `inline_spans` already threads it through every other nested tag.
fn inline_list_items(
    nodes: &[Node],
    ordered: bool,
    bold: bool,
    italic: bool,
    depth: u32,
    out: &mut Vec<Span>,
) {
    if depth > MAX_DEPTH {
        if nodes_contain_an_li(nodes) {
            DEPTH_CAP_HIT.with(|hit| hit.set(true));
        }
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
            bold,
            italic,
        });
        // If this item's content starts with a block boundary — `<li><p>Child</p></li>` —
        // `inline_spans` pushes a break before it. That is normally correct, separating
        // one block from the one before it, but here `out` already ends with the marker's
        // own `Run`, so the leading break lands between the marker and its first line of
        // content instead of before a preceding sibling, splitting them across two lines.
        // Strip exactly that one leading break, never more: anything after it is
        // legitimate inter-block spacing within the item's own content.
        let content_start = out.len();
        inline_spans(
            children,
            bold,
            italic,
            depth + 1,
            &GlueContext::Resolved(false),
            out,
        );
        if out.get(content_start) == Some(&Span::Break) {
            out.remove(content_start);
        }
    }
}

/// `has_more_after` is true if a later `<tr>` — an ancestor's remaining
/// siblings, once this call returns — has a real cell. Needed alongside
/// `out`'s already-collected rows so the depth-cap guard can tell whether
/// the *whole* table (not just this capped subtree) ever gets a nonzero
/// `Writer::draw_table` column count — see
/// [`nodes_contain_table_output`]'s doc comment. Pass `false` for the
/// initial call from a fresh `<table>`: `Writer::draw_table`'s `n_cols` is
/// scoped to one table, so nothing outside this one is relevant.
fn extract_table_rows(nodes: &[Node], depth: u32, has_more_after: bool, out: &mut Vec<TableRow>) {
    if depth > MAX_DEPTH {
        let table_has_other_populated_row =
            has_more_after || out.iter().any(|row| !row.cells.is_empty());
        if nodes_contain_table_output(nodes, table_has_other_populated_row) {
            DEPTH_CAP_HIT.with(|hit| hit.set(true));
        }
        return;
    }
    let more_after = populated_row_after_each(nodes, has_more_after);
    for (i, node) in nodes.iter().enumerate() {
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
                        inline_spans(
                            cell_children,
                            is_header,
                            false,
                            depth + 2,
                            &GlueContext::Resolved(false),
                            &mut spans,
                        );
                        trim_trailing_break(&mut spans);
                        cells.push((spans, is_header));
                    }
                }
                out.push(TableRow { cells });
            }
            // Structural wrappers (thead/tbody/tfoot) — descend without
            // emitting a row themselves.
            "thead" | "tbody" | "tfoot" => {
                extract_table_rows(children, depth + 1, more_after[i], out);
            }
            _ if is_non_rendered(tag) => {}
            // Anything else inside a <table> (most commonly <caption>, or a
            // stray text-bearing tag) isn't a row — but its text must still
            // render somewhere, matching this renderer's "unknown tags pass
            // their text through transparently" contract (see module docs).
            // A single-cell row is the simplest way to surface it without a
            // dedicated non-tabular-content block type.
            _ => {
                let mut spans = Vec::new();
                inline_spans(
                    children,
                    false,
                    false,
                    depth + 1,
                    &GlueContext::Resolved(false),
                    &mut spans,
                );
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
        if nodes_contain_an_li(nodes) {
            DEPTH_CAP_HIT.with(|hit| hit.set(true));
        }
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
        inline_spans(
            children,
            false,
            false,
            depth + 1,
            &GlueContext::Resolved(false),
            &mut spans,
        );
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
        // No glue risk here: `pending` (below) doesn't exist yet at this
        // point, so there's no accumulated word this call could ever glue
        // a dropped whitespace-only span onto.
        if subtree_has_visible_content(nodes, false) {
            DEPTH_CAP_HIT.with(|hit| hit.set(true));
        }
        return;
    }
    let mut pending: Vec<Span> = Vec::new();
    let flush = |pending: &mut Vec<Span>, out: &mut Vec<Block>| {
        if !pending.is_empty() {
            out.push(Block::Paragraph(std::mem::take(pending)));
        }
    };

    for (i, node) in nodes.iter().enumerate() {
        let more_after = GlueContext::LaterSiblings {
            siblings: &nodes[i + 1..],
            ancestor: &GlueContext::Resolved(false),
        };
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
                    inline_spans(
                        children,
                        true,
                        false,
                        depth + 1,
                        &GlueContext::Resolved(false),
                        &mut spans,
                    );
                    trim_trailing_break(&mut spans);
                    out.push(Block::Heading(level, spans));
                    continue;
                }
                match tag.as_str() {
                    // `p` and `li` cannot legally nest another block element in
                    // HTML — a nested block inside them is already malformed
                    // input — so flattening their content to one implicit
                    // paragraph is a reasonable degrade. `li`'s normal path is
                    // `extract_list_items` below, not here; this arm sees only a
                    // stray `<li>` outside a `<ul>`/`<ol>`. `dt` and `dd`, a
                    // description list's term and value, are the same shape: each
                    // is its own block-level unit whose content is normally
                    // inline, so it gets its own paragraph rather than gluing onto
                    // its sibling. Without this,
                    // `<dl><dt>Title</dt><dd>My Post</dd>...</dl>`, as scaffold
                    // detail views emit from a `property_list` widget, renders as
                    // one run of unbroken text with no row boundaries.
                    "p" | "li" | "dt" | "dd" => {
                        flush(&mut pending, out);
                        let mut spans = Vec::new();
                        inline_spans(
                            children,
                            false,
                            false,
                            depth + 1,
                            &GlueContext::Resolved(false),
                            &mut spans,
                        );
                        trim_trailing_break(&mut spans);
                        out.push(Block::Paragraph(spans));
                    }
                    // `div`, `blockquote`, and `dl` commonly wrap other block
                    // elements — `<div><h1>...</h1><p>...</p></div>`,
                    // `<blockquote><p>...</p></blockquote>`, a `<dl>`'s `<dt>`
                    // and `<dd>` children. Recursing through `flatten_blocks`,
                    // rather than flattening every descendant through
                    // `inline_spans` into one paragraph, lets nested block tags
                    // produce their own blocks; otherwise a heading and two
                    // paragraphs merge into a single run of unbroken text. When
                    // the children are purely inline
                    // (`<div><span>hi</span></div>`), `flatten_blocks`'s own
                    // pending/flush accumulator produces exactly the same single
                    // implicit paragraph this used to build directly.
                    //
                    // HTML5's sectioning and landmark elements — `section`,
                    // `article`, `main`, `header`, `footer`, `nav`, `aside` —
                    // commonly wrap block content the way a `<div>` does. Without
                    // them here, adjacent elements of loose text fell through to
                    // the generic transparent-passthrough arm and accumulated
                    // into one pending paragraph with no separator, rendering
                    // `<aside>Summary</aside><aside>Details</aside>` as
                    // `SummaryDetails`.
                    "div" | "blockquote" | "dl" | "section" | "article" | "main" | "header"
                    | "footer" | "nav" | "aside" => {
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
                        extract_table_rows(children, depth + 1, false, &mut rows);
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
                    "strong" | "b" => {
                        inline_spans(children, true, false, depth + 1, &more_after, &mut pending);
                    }
                    "em" | "i" => {
                        inline_spans(children, false, true, depth + 1, &more_after, &mut pending);
                    }
                    _ if is_non_rendered(tag) => {}
                    // Transparent passthrough: unknown/inline wrapper tags
                    // (span, a, ...) flow their children into the current
                    // implicit paragraph rather than being dropped.
                    _ => {
                        flatten_into_pending(children, depth + 1, &more_after, &mut pending, out);
                    }
                }
            }
        }
    }
    flush(&mut pending, out);
}

/// Like [`flatten_blocks`], but for a transparent inline wrapper: nested
/// block tags still start real blocks (flushing `pending` first), while
/// inline content keeps accumulating into the caller's `pending` buffer.
fn flatten_into_pending(
    nodes: &[Node],
    depth: u32,
    has_more_after: &GlueContext,
    pending: &mut Vec<Span>,
    out: &mut Vec<Block>,
) {
    if depth > MAX_DEPTH {
        if subtree_has_visible_content(
            nodes,
            ends_with_glueable_word(pending) && has_more_after.resolve(),
        ) {
            DEPTH_CAP_HIT.with(|hit| hit.set(true));
        }
        return;
    }
    // Reuse `flatten_blocks` by giving it a scratch buffer, then splice: if
    // it only ever produced inline text (no nested block tags fired), that
    // text lives in blocks as trailing paragraphs — simplest correct
    // approach is to just recurse the same tag-matching logic directly.
    for (i, node) in nodes.iter().enumerate() {
        let more_after = GlueContext::LaterSiblings {
            siblings: &nodes[i + 1..],
            ancestor: has_more_after,
        };
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
                            | "section"
                            | "article"
                            | "main"
                            | "header"
                            | "footer"
                            | "nav"
                            | "aside"
                    )
                {
                    if !pending.is_empty() {
                        out.push(Block::Paragraph(std::mem::take(pending)));
                    }
                    flatten_blocks(std::slice::from_ref(node), depth, out);
                } else {
                    match tag.as_str() {
                        "br" => pending.push(Span::Break),
                        "strong" | "b" => {
                            inline_spans(children, true, false, depth + 1, &more_after, pending);
                        }
                        "em" | "i" => {
                            inline_spans(children, false, true, depth + 1, &more_after, pending);
                        }
                        _ if is_non_rendered(tag) => {}
                        _ => {
                            flatten_into_pending(children, depth + 1, &more_after, pending, out);
                        }
                    }
                }
            }
        }
    }
}

/// Non-breaking space variants this renderer treats identically to a
/// literal `&nbsp;` (U+00A0) for line-breaking purposes: U+2007 FIGURE
/// SPACE and U+202F NARROW NO-BREAK SPACE, both common in localized
/// number formatting (aligned digit columns; French-style thousands
/// separators, e.g. `10 000`) — and, like U+00A0, whitespace per
/// Unicode's `White_Space` property, so a blanket `char::is_whitespace()`
/// check alone can't tell them apart from an ordinary breakable space.
/// Purely a line-breaking concern: whether the glyph itself renders
/// correctly is the same already-documented base-14/WinAnsi-encoding
/// limitation that applies to any character outside that set (CJK,
/// emoji, ...) — unaffected by this.
const fn is_non_breaking_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{2007}' | '\u{202F}')
}

/// A whitespace char [`words_of`] actually splits words on — plain spaces,
/// tabs, newlines, but not a non-breaking space variant (see
/// [`is_non_breaking_space`]), which stays glued to its word instead of
/// separating it.
const fn is_breakable_whitespace(c: char) -> bool {
    c.is_whitespace() && !is_non_breaking_space(c)
}

/// True if `out`'s last span is a real word with nothing yet separating
/// it from whatever comes next — the same condition [`words_of`] tracks
/// internally as `glue_next`. A depth-cap guard past this point in `out`
/// must treat even whitespace-only text in the capped subtree as visible,
/// because dropping it is what would let `words_of` glue that word to the
/// next one instead of keeping a space between them.
fn ends_with_glueable_word(out: &[Span]) -> bool {
    matches!(out.last(), Some(Span::Run { text, .. }) if !text.ends_with(is_breakable_whitespace))
}

/// The other half of [`ends_with_glueable_word`]: whether processing a
/// sequence of nodes in document order — the way
/// [`inline_spans`]/[`flatten_into_pending`] actually would — pushes a real
/// (non-whitespace) word into the buffer they share, stops that from ever
/// happening, or leaves it undecided.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GlueLookahead {
    /// Found real (non-whitespace) text: a later word really could glue.
    Confirmed,
    /// Hit a `<br>`, a `<ul>`/`<ol>` (always wrapped in a break by
    /// [`push_block_break`], regardless of whether it has a real `<li>`), or
    /// an [`is_block_boundary_in_inline_context`] tag — each one already
    /// separates whatever comes after it from whatever came before, so
    /// nothing beyond it, inside this list or outside it, can retroactively
    /// glue across that point.
    Stopped,
    /// Nothing in this list decided either way (empty, all whitespace, all
    /// skipped non-rendered tags): defer to whatever follows it.
    Exhausted,
}

/// [`GlueLookahead`] for one node's own subtree, treating its children the
/// way [`inline_spans`]/[`flatten_into_pending`] would recurse into them.
/// Walks with an explicit stack (children pushed in reverse, so popping
/// yields left-to-right) for the same stack-safety reason as
/// [`subtree_has_visible_content`]: a transparent wrapper's content can
/// itself be arbitrarily deep.
fn node_glue_lookahead(node: &Node) -> GlueLookahead {
    let mut stack: Vec<&Node> = vec![node];
    while let Some(node) = stack.pop() {
        match node {
            Node::Text(text) => {
                // Leading breakable whitespace already clears words_of's
                // glue boundary on its own — same conclusiveness as a
                // Stopped tag, regardless of what real text follows it in
                // this same run. Only text with a real char and no
                // whitespace ahead of it is a genuine glue risk; pure
                // whitespace (or an empty string) decides nothing yet.
                if text.starts_with(is_breakable_whitespace) {
                    return GlueLookahead::Stopped;
                }
                if text.chars().any(|c| !is_breakable_whitespace(c)) {
                    return GlueLookahead::Confirmed;
                }
            }
            Node::Element { tag, children } => {
                if tag == "br"
                    || tag == "ul"
                    || tag == "ol"
                    || is_block_boundary_in_inline_context(tag)
                {
                    return GlueLookahead::Stopped;
                }
                if !is_non_rendered(tag) {
                    stack.extend(children.iter().rev());
                }
            }
        }
    }
    GlueLookahead::Exhausted
}

/// A lazily-resolved "does something after this position glue" signal.
/// Building one never scans anything — it only borrows the later siblings
/// (if any) at this level, plus whatever an ancestor level already
/// deferred. The scan happens, at most once per value, inside
/// [`resolve`](Self::resolve), and only when a caller actually asks.
///
/// This laziness is load-bearing, not an optimization for its own sake:
/// every recursive call builds one of these for its *current* sibling, and
/// most of them are never resolved at all, because the depth-cap guard
/// that would consume it never fires, or [`ends_with_glueable_word`]
/// (checked first, since it is O(1)) is already false. An eager version —
/// scan `nodes[i + 1..]` up front, for every `i`, at every one of the
/// ~[`MAX_DEPTH`] levels the depth cap allows before native recursion
/// stops — is what made a wide sibling list (or, worse, a long chain where
/// each level has more than one child) cost O(depth × n) instead of O(n).
enum GlueContext<'a> {
    Resolved(bool),
    LaterSiblings {
        siblings: &'a [Node],
        ancestor: &'a Self,
    },
}

impl GlueContext<'_> {
    /// A caller builds this with `siblings` set to *only* the nodes after
    /// the one it is about to recurse into — never that node itself, since
    /// whatever is inside it gets discovered by that recursion's own
    /// eventual depth-cap guard, and pre-scanning it here too would be
    /// pure duplicated work. Every sibling actually in `siblings` has
    /// *not* been (and, from this call's perspective, may never be)
    /// visited any other way, so those are the only nodes this walk
    /// touches.
    fn resolve(&self) -> bool {
        match self {
            GlueContext::Resolved(b) => *b,
            GlueContext::LaterSiblings { siblings, ancestor } => {
                for node in *siblings {
                    match node_glue_lookahead(node) {
                        GlueLookahead::Confirmed => return true,
                        GlueLookahead::Stopped => return false,
                        GlueLookahead::Exhausted => {}
                    }
                }
                ancestor.resolve()
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
    // Whether the pending `glue_next` boundary is specifically an NBSP —
    // i.e. the previous span's text ended with a literal U+00A0 — as
    // opposed to two spans with plain nothing (no whitespace at all)
    // between them. See [`Word::Text::unbreakable`].
    let mut glue_next_unbreakable = false;
    for span in spans {
        match span {
            Span::Break => {
                words.push(Word::Break);
                glue_next = false;
                glue_next_unbreakable = false;
            }
            Span::Run { text, bold, italic } => {
                // Must agree with the split predicate below on what counts as a
                // real, breakable whitespace boundary. NBSP does not, since it is
                // deliberately kept inside the resulting token rather than split
                // off. The blanket `char::is_whitespace`, which NBSP also
                // satisfies, would say a span starting or ending with NBSP has a
                // real separator at that edge, gluing it to nothing — so `wrap`
                // inserts its own extra plain space next to a token that already
                // renders the NBSP as one, and allows a line break at a boundary
                // the NBSP was meant to make unbreakable.
                let is_breakable_ws = |c: char| c.is_whitespace() && !is_non_breaking_space(c);
                let starts_with_ws = text.starts_with(is_breakable_ws);
                let ends_with_ws = text.ends_with(is_breakable_ws);
                let starts_with_nbsp = text.starts_with(is_non_breaking_space);
                let ends_with_nbsp = text.ends_with(is_non_breaking_space);
                let mut emitted_any = false;
                // Split on breakable whitespace only. NBSP and its non-breaking
                // variants (`&nbsp;`/U+00A0, U+2007, U+202F — see
                // `is_non_breaking_space`) satisfy `char::is_whitespace()`, so
                // `split_whitespace()` would treat them as ordinary word
                // separators, discarding the whole point of a non-breaking space.
                // It stays inside the resulting token instead, so a line can never
                // break between the words it joins. It still renders as a real
                // space — `char_width_1000em` gives it a plain space's width — the
                // token is simply atomic.
                for (i, w) in text
                    .split(is_breakable_ws)
                    .filter(|w| !w.is_empty())
                    .enumerate()
                {
                    let glue = i == 0 && glue_next && !starts_with_ws;
                    words.push(Word::Text {
                        text: w.to_owned(),
                        bold: *bold,
                        italic: *italic,
                        glue,
                        unbreakable: glue && (glue_next_unbreakable || starts_with_nbsp),
                    });
                    emitted_any = true;
                }
                glue_next = emitted_any && !ends_with_ws;
                glue_next_unbreakable = emitted_any && ends_with_nbsp;
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
/// An embedded non-breaking space (`&nbsp;`/U+00A0, or one of the other
/// variants [`is_non_breaking_space`] recognizes, kept inside the token by
/// `words_of` — see [`Word::Text::unbreakable`] for the same rule at a
/// *span* boundary) must never sit at a chunk boundary on *either* side: as
/// the last character of one chunk, it isolates whatever follows onto the
/// next; as the first character of a chunk, it isolates whatever precedes
/// it onto the previous *and* leaves a rendered leading space at the start
/// of the new line — both indistinguishable from an ordinary space
/// wrapping there, exactly what a non-breaking space forbids. Consecutive
/// ones chain: `A&nbsp;B&nbsp;C`
/// has *no* legal split point anywhere between `A` and `C`, so when the
/// natural per-character boundary would land inside that chain, the whole
/// chain (back to the nearest ordinary, non-NBSP-adjacent character) moves
/// to the *next* chunk together — the same "relocate the whole unbreakable
/// run, not just its last word" rule [`wrap`] applies to a glued run of
/// *words*, applied here at the character level via an equivalent
/// incrementally-tracked `run_start`/`run_width` (not recomputed by
/// rescanning `current` on every overflow, for the same reason `wrap`'s
/// `run_width` isn't: a long chain of NBSP-glued characters must stay
/// linear, not quadratic, in the number of overflow events).
///
/// Always makes progress: a chunk always gets at least one character even if
/// that character alone exceeds `max_width_pt` — the sole exception being a
/// chunk that's entirely one NBSP-connected chain with no earlier split
/// point to relocate to, which is left overflowing rather than split
/// mid-chain, the same as an entire line that's one unbreakable run in
/// [`wrap`].
///
/// `first_chunk_max_width_pt` is the width budget for *only* the first
/// produced chunk; every later chunk uses `max_width_pt`. The plain
/// oversized-token case in [`wrap`] (and every direct caller below) passes
/// the same value for both — a fresh line has the full column width
/// available. [`split_oversized_glued_word`] passes a *narrower* value for
/// the first chunk specifically: that chunk is appended to a line that
/// already has other content on it, so sizing it against the full column
/// width the way the rest of this function already does would produce a
/// first chunk that, combined with what's already on the line, still
/// overflows well past the column — not the character-level wrapping this
/// function exists to provide.
fn split_into_fitting_chunks(
    text: &str,
    font_size_pt: f32,
    bold: bool,
    first_chunk_max_width_pt: f32,
    max_width_pt: f32,
) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_width = 0.0f32;
    // Byte index into `current` (always a char boundary) where the
    // NBSP-connected run ending at `current`'s last character begins, and
    // that run's width — see the function docs above.
    let mut run_start = 0usize;
    let mut run_width = 0.0f32;
    for ch in text.chars() {
        let ch_width = f32::from(char_width_1000em(ch, bold)) / 1000.0 * font_size_pt;
        let connected = is_non_breaking_space(ch) || current.ends_with(is_non_breaking_space);
        let limit = if chunks.is_empty() {
            first_chunk_max_width_pt
        } else {
            max_width_pt
        };
        if !current.is_empty() && current_width + ch_width > limit {
            if connected {
                if run_start > 0 {
                    let tail = current.split_off(run_start);
                    chunks.push(std::mem::take(&mut current));
                    current = tail;
                    current_width = run_width;
                    run_start = 0;
                }
                // Else the whole chunk built so far is one NBSP-connected
                // chain with nowhere earlier to split — accept the
                // overflow rather than break mid-chain.
            } else {
                chunks.push(std::mem::take(&mut current));
                current_width = 0.0;
                run_start = 0;
                run_width = 0.0;
            }
        }
        if connected {
            run_width += ch_width;
        } else {
            run_start = current.len();
            run_width = ch_width;
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
/// [`Word::Text::unbreakable`] words are held to a stricter rule: an NBSP
/// means the source HTML explicitly forbids a line break at that boundary,
/// so on overflow the *entire* unbreakable run built up so far (tracked via
/// `run_start`/`run_width`, not just the word that doesn't fit) moves to the
/// next line together, rather than splitting between the run and the new
/// word the way ordinary glue would.
///
/// A single word wider than `max_width_pt` on its own (a long URL, hash, or
/// identifier with nowhere to break) is character-wrapped via
/// [`split_into_fitting_chunks`] instead of being left to overflow the page
/// or table-cell boundary.
///
/// If an NBSP-glued (unbreakable) word is itself individually oversized,
/// [`wrap`] calls this to character-split it while keeping the first chunk
/// glued to whatever `current` already holds — `w` is that word's own
/// (already-known-oversized) width, counted into `*current_width` before
/// this runs, so `*current_width - w` is the width of whatever's already
/// on the line the first chunk must still fit alongside. That's the width
/// budget passed to [`split_into_fitting_chunks`] for its first chunk
/// specifically (see that function's `first_chunk_max_width_pt` docs) —
/// without it, the first chunk was sized against the *full* column width
/// the same as every later chunk, so appending it to an already-nonempty
/// line could still send the combined line well past `max_width_pt`,
/// defeating the character-wrapping this function exists to provide.
/// Returns `true` and leaves `current`/`current_width`/`lines` updated
/// (first chunk appended and flushed, remaining chunks distributed, last
/// one left as the new `current`) if splitting actually produced more
/// than one chunk; returns `false` with nothing touched if
/// [`split_into_fitting_chunks`] returned only one chunk (nothing left to
/// split — e.g. the whole word is one NBSP-connected chain with no earlier
/// split point, see that function's own accepted-overflow fallback), so the
/// caller can fall through to its plain glue-the-whole-word-as-is path.
/// Extracted out of [`wrap`] purely to keep that function's line count
/// down — this has no state of its own beyond its `&mut` parameters.
#[allow(clippy::too_many_arguments)]
fn split_oversized_glued_word(
    text: &str,
    bold: bool,
    italic: bool,
    w: f32,
    font_size_pt: f32,
    max_width_pt: f32,
    current: &mut Vec<StyledWord>,
    current_width: &mut f32,
    lines: &mut Vec<Vec<StyledWord>>,
) -> bool {
    let existing_width = (*current_width - w).max(0.0);
    let mut chunks = split_into_fitting_chunks(
        text,
        font_size_pt,
        bold,
        (max_width_pt - existing_width).max(0.0),
        max_width_pt,
    )
    .into_iter();
    let first = chunks
        .next()
        .expect("split_into_fitting_chunks never returns empty chunks for non-empty text");
    let rest: Vec<String> = chunks.collect();
    if rest.is_empty() {
        return false;
    }
    let first_w = text_width_pt(&first, font_size_pt, bold);
    *current_width = *current_width - w + first_w;
    current.push((first, bold, italic, true));
    lines.push(std::mem::take(current));
    let last = rest.len() - 1;
    for (i, chunk) in rest.into_iter().enumerate() {
        let chunk_w = text_width_pt(&chunk, font_size_pt, bold);
        if i == last {
            *current_width = chunk_w;
            *current = vec![(chunk, bold, italic, false)];
        } else {
            lines.push(vec![(chunk, bold, italic, false)]);
        }
    }
    true
}

/// Handles an NBSP-glued (`unbreakable`) word once the caller ([`wrap`])
/// has already confirmed `unbreakable` is set and `current` isn't empty —
/// the two preconditions for this word needing glued-run handling instead
/// of the plain word-wrap path below. An unbreakable word must never be
/// split away from whatever it's glued to just because it also happens to
/// be individually too wide for one line on its own, so on overflow the
/// *entire* unbreakable run built up so far (tracked via `run_start`/
/// `run_width`, not just this word) relocates to a fresh line together,
/// rather than splitting between the run and this word the way ordinary
/// glue would; if there's nowhere better to put it (`run_start == 0`, the
/// run already spans the whole line from its start), the overflow is
/// accepted instead of looping forever.
///
/// The NBSP boundary only forbids a break *right there*, though — it says
/// nothing about the rest of this word if it's *also* individually wider
/// than a whole line (e.g. a still-open `<strong>` run glued via `&nbsp;`
/// to 100 characters of unbroken text). Left whole, that's not just
/// suboptimal, it's the entire remaining run rendered as one unsplit,
/// unbounded-width token — overflowing and clipped, not merely spilling a
/// little past the margin. [`split_oversized_glued_word`] character-splits
/// it the same way the ordinary oversized-token branch in [`wrap`] does,
/// just keeping the first chunk glued right here; see its docs for the
/// `false` fallback (nothing left to split) this falls through from.
///
/// Extracted out of [`wrap`] purely to keep that function's line count
/// down — this has no state of its own beyond its `&mut` parameters.
#[allow(clippy::too_many_arguments)]
fn handle_unbreakable_word(
    text: &str,
    bold: bool,
    italic: bool,
    w: f32,
    font_size_pt: f32,
    max_width_pt: f32,
    current: &mut Vec<StyledWord>,
    current_width: &mut f32,
    run_start: &mut usize,
    run_width: &mut f32,
    lines: &mut Vec<Vec<StyledWord>>,
) {
    let new_run_width = *run_width + w;
    let prefix_width = *current_width - *run_width;
    if *run_start > 0 && prefix_width + new_run_width > max_width_pt {
        let tail = current.split_off(*run_start);
        lines.push(std::mem::take(current));
        *current = tail;
        *current_width = new_run_width;
        *run_start = 0;
    } else {
        *current_width = prefix_width + new_run_width;
    }
    if w > max_width_pt
        && !text.is_empty()
        && split_oversized_glued_word(
            text,
            bold,
            italic,
            w,
            font_size_pt,
            max_width_pt,
            current,
            current_width,
            lines,
        )
    {
        *run_start = 0;
        *run_width = *current_width;
        return;
    }
    current.push((text.to_owned(), bold, italic, true));
    *run_width = new_run_width;
}

fn wrap(words: &[Word], max_width_pt: f32, font_size_pt: f32) -> Vec<Vec<StyledWord>> {
    let space_w = text_width_pt(" ", font_size_pt, false);
    let mut lines = Vec::new();
    let mut current: Vec<StyledWord> = Vec::new();
    let mut current_width = 0.0f32;
    // Index into `current` where the active unbreakable (NBSP-glued) run
    // begins, and that run's total width — tracked incrementally (not
    // recomputed by summing `current[run_start..]` on each word) so a long
    // chain of NBSP-glued words stays linear, not quadratic, in the number
    // of words — see the `long_run_of_unterminated_*` lint on this module
    // for why that class of bug matters here.
    let mut run_start = 0usize;
    let mut run_width = 0.0f32;

    for word in words {
        match word {
            Word::Break => {
                lines.push(std::mem::take(&mut current));
                current_width = 0.0;
                run_start = 0;
                run_width = 0.0;
            }
            Word::Text {
                text,
                bold,
                italic,
                glue,
                unbreakable,
            } => {
                let w = text_width_pt(text, font_size_pt, *bold);
                // See `handle_unbreakable_word`'s docs for why this needs
                // its own path, checked *before* the oversized-token
                // branch below — an unbreakable (NBSP-glued) word must
                // never be split away from whatever it's glued to just
                // because it also happens to be individually too wide for
                // one line on its own.
                if *unbreakable && !current.is_empty() {
                    handle_unbreakable_word(
                        text,
                        *bold,
                        *italic,
                        w,
                        font_size_pt,
                        max_width_pt,
                        &mut current,
                        &mut current_width,
                        &mut run_start,
                        &mut run_width,
                        &mut lines,
                    );
                    continue;
                }
                if w > max_width_pt && !text.is_empty() {
                    if !current.is_empty() {
                        lines.push(std::mem::take(&mut current));
                        current_width = 0.0;
                    }
                    let chunks = split_into_fitting_chunks(
                        text,
                        font_size_pt,
                        *bold,
                        max_width_pt,
                        max_width_pt,
                    );
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
                    run_start = 0;
                    run_width = current_width;
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
                run_start = current.len();
                run_width = w;
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
                // A fixed 16pt indent fits every bullet/low-numbered marker
                // this renderer draws ("•", "1." .. "9.") comfortably, but
                // an ordered list's marker keeps growing with its index —
                // "100." alone is already ~21pt at 11pt Helvetica, wider
                // than the indent, so content wrapped at a fixed 16pt
                // overlapped the marker instead of starting after it. Grow
                // the indent (and thus the content's wrap width) to fit
                // whichever marker this specific item actually has.
                const MIN_INDENT: f32 = 16.0;
                const MARKER_GAP: f32 = 4.0;
                const LINE_HEIGHT: f32 = 14.5;
                self.ensure_space(LINE_HEIGHT);
                self.draw_word(0.0, marker, false, false, 11.0);
                let indent = (text_width_pt(marker, 11.0, false) + MARKER_GAP).max(MIN_INDENT);
                let words = words_of(spans);
                let lines = wrap(&words, self.content_width - indent, 11.0);
                if lines.is_empty() {
                    // An empty item (`<li></li>`, or one whose only content
                    // was skipped, e.g. `<li><script>...</script></li>`)
                    // has no lines for `draw_lines` to advance `y_from_top`
                    // by — it only adds `line_height` per *line drawn*, and
                    // there are none — so without this, only the fixed 4pt
                    // spacer below would separate this marker from the next
                    // item's, landing them almost on top of each other.
                    self.y_from_top += LINE_HEIGHT;
                } else {
                    self.draw_lines(&lines, indent, 11.0, LINE_HEIGHT, true);
                }
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
///
/// If the input nests past [`MAX_DEPTH`], the excess content is dropped and
/// this logs one `tracing::warn!` at target `autumn::pdf` — see the
/// [module docs](crate::pdf)'s "Nesting depth limit" section.
pub(super) fn render_pages(html: &str) -> Vec<PdfPage> {
    DEPTH_CAP_HIT.with(|hit| hit.set(false));

    let nodes = super::html::parse(html);
    let mut blocks = Vec::new();
    flatten_blocks(&nodes, 0, &mut blocks);

    let mut writer = Writer::new();
    for block in &blocks {
        writer.draw_block(block);
    }

    if DEPTH_CAP_HIT.with(std::cell::Cell::get) {
        tracing::warn!(
            target: "autumn::pdf",
            max_depth = MAX_DEPTH,
            "pdf layout: nesting depth cap reached; content past this depth was dropped",
        );
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
    fn oversized_token_does_not_split_immediately_adjacent_to_an_embedded_nbsp() {
        // Regression: an oversized token — already too wide for one line, so it
        // goes through `split_into_fitting_chunks`'s plain character splitter —
        // containing an embedded NBSP had no NBSP awareness. If the natural
        // per-character width boundary fell right after the NBSP, the NBSP ended
        // up as the last character of one chunk and whatever followed it started
        // the next, breaking exactly the boundary NBSP forbids. A first fix moved
        // the NBSP itself to the next chunk, which merely relocated the forbidden
        // break to before the NBSP, leaving a rendered leading space at the start
        // of the next line and still splitting the pair: the character before the
        // NBSP must move with it. Repro: 67 `A`s followed by `&nbsp;B` — the 67 As
        // plus the NBSP fit within the content width, but adding `B` does not, so
        // the naive split lands right after the NBSP.
        let text = format!("{}\u{00A0}B", "A".repeat(67));
        let font_size_pt = 11.0;
        let max_width_pt = text_width_pt(&"A".repeat(67), font_size_pt, false)
            + text_width_pt("\u{00A0}", font_size_pt, false)
            + 0.5;
        let chunks =
            split_into_fitting_chunks(&text, font_size_pt, false, max_width_pt, max_width_pt);
        assert!(
            chunks
                .iter()
                .all(|c| !c.starts_with('\u{00A0}') && !c.ends_with('\u{00A0}')),
            "no chunk boundary may sit immediately before or after an NBSP, got {chunks:?}"
        );
        let reassembled: String = chunks.concat();
        assert_eq!(
            reassembled, text,
            "splitting must not drop or reorder any characters"
        );
    }

    #[test]
    fn oversized_token_does_not_split_around_other_unicode_non_breaking_space_variants() {
        // Same regression as the U+00A0 case above, for U+2007/U+202F —
        // see `is_non_breaking_space`.
        for nbsp in ['\u{2007}', '\u{202F}'] {
            let text = format!("{}{nbsp}B", "A".repeat(67));
            let font_size_pt = 11.0;
            let max_width_pt = text_width_pt(&"A".repeat(67), font_size_pt, false)
                + text_width_pt(&nbsp.to_string(), font_size_pt, false)
                + 0.5;
            let chunks =
                split_into_fitting_chunks(&text, font_size_pt, false, max_width_pt, max_width_pt);
            assert!(
                chunks
                    .iter()
                    .all(|c| !c.starts_with(nbsp) && !c.ends_with(nbsp)),
                "U+{:04X}: no chunk boundary may sit immediately before or after it, got \
                 {chunks:?}",
                nbsp as u32
            );
            let reassembled: String = chunks.concat();
            assert_eq!(
                reassembled, text,
                "U+{:04X}: splitting must not drop or reorder any characters",
                nbsp as u32
            );
        }
    }

    #[test]
    fn oversized_token_does_not_split_when_the_incoming_character_is_the_nbsp() {
        // Regression: overflow can be triggered by the NBSP arriving as the current
        // character, not only by it already sitting at the end of the accumulated
        // chunk. `current` does not yet end with an NBSP at that point, so the
        // NBSP-adjacency guard keyed off `current.ends_with(NBSP)` never fired, and
        // the boundary landed right before the NBSP the way it used to land right
        // after one. Repro: 67 `A`s followed by `i&nbsp;B` — the As plus `i` fit
        // within the content width, but adding the NBSP does not, so the naive split
        // lands right before it.
        let text = format!("{}i\u{00A0}B", "A".repeat(67));
        let font_size_pt = 11.0;
        let max_width_pt = text_width_pt(&"A".repeat(67), font_size_pt, false)
            + text_width_pt("i", font_size_pt, false)
            + 0.5;
        let chunks =
            split_into_fitting_chunks(&text, font_size_pt, false, max_width_pt, max_width_pt);
        assert!(
            chunks
                .iter()
                .all(|c| !c.starts_with('\u{00A0}') && !c.ends_with('\u{00A0}')),
            "no chunk boundary may sit immediately before or after an NBSP, got {chunks:?}"
        );
        let reassembled: String = chunks.concat();
        assert_eq!(
            reassembled, text,
            "splitting must not drop or reorder any characters"
        );
    }

    #[test]
    fn oversized_token_moves_the_entire_nbsp_connected_chain_not_just_one_neighbor() {
        // Regression: when an oversized token contains several NBSPs, the previous
        // fix pulled only one preceding character back before emitting the chunk. If
        // that character was itself connected to an earlier NBSP, the emitted chunk
        // still ended in that earlier NBSP, relocating which boundary got broken
        // rather than fixing the bug. Repro: 66 `A`s followed by `&nbsp;B&nbsp;C` —
        // the As plus the first NBSP plus `B` fit within the content width, but
        // adding the second NBSP does not, so the naive split used to land the first
        // chunk right after the first NBSP.
        let text = format!("{}\u{00A0}B\u{00A0}C", "A".repeat(66));
        let font_size_pt = 11.0;
        let max_width_pt = text_width_pt(&"A".repeat(66), font_size_pt, false)
            + text_width_pt("\u{00A0}B", font_size_pt, false)
            + 0.5;
        let chunks =
            split_into_fitting_chunks(&text, font_size_pt, false, max_width_pt, max_width_pt);
        assert!(
            chunks
                .iter()
                .all(|c| !c.starts_with('\u{00A0}') && !c.ends_with('\u{00A0}')),
            "no chunk boundary may sit immediately before or after an NBSP, got {chunks:?}"
        );
        let reassembled: String = chunks.concat();
        assert_eq!(
            reassembled, text,
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
                unbreakable: false,
            },
            Word::Break,
            Word::Text {
                text: "b".to_owned(),
                bold: false,
                italic: false,
                glue: false,
                unbreakable: false,
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
                unbreakable: false,
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
    fn other_unicode_non_breaking_space_variants_also_keep_their_words_on_one_line() {
        // Regression: `is_breakable_ws` and `words_of` exempted only U+00A0, but
        // U+2007 FIGURE SPACE and U+202F NARROW NO-BREAK SPACE are both whitespace
        // per Unicode's `White_Space` property — so `char::is_whitespace()` alone
        // cannot tell them from an ordinary breakable space — and both are common in
        // localized number formatting: aligned digit columns, and French-style
        // thousands separators like `10 000`. Without special-casing them the way
        // U+00A0 is handled, a line could break inside such a number.
        for nbsp in ['\u{2007}', '\u{202F}'] {
            let text = format!("10{nbsp}000");
            let words = words_of(&[Span::Run {
                text: text.clone(),
                bold: false,
                italic: false,
            }]);
            assert_eq!(
                words,
                vec![Word::Text {
                    text: text.clone(),
                    bold: false,
                    italic: false,
                    glue: false,
                    unbreakable: false,
                }],
                "U+{:04X} must not split the run into two breakable words, got {words:?}",
                nbsp as u32
            );
            let narrow_width = text_width_pt(&text, 12.0, false) + 1.0;
            let lines = wrap(&words, narrow_width, 12.0);
            assert_eq!(
                lines.len(),
                1,
                "U+{:04X}: the number must stay on one line, got {lines:?}",
                nbsp as u32
            );
        }
    }

    #[test]
    fn non_breaking_space_leading_a_styled_span_still_glues_to_the_previous_word() {
        // Regression: in `Hello<strong>&nbsp;world</strong>` the leading NBSP stays
        // inside the second span's token (`"\u{00A0}world"`, per the fix above), but
        // `starts_with_ws`/`ends_with_ws` used the blanket `char::is_whitespace()`
        // predicate, which NBSP also satisfies. That treated the span boundary as a
        // real separator and set `glue: false`, so `wrap` would insert its own plain
        // space next to a token that already renders the NBSP as one — a visible
        // double space — and would allow a line break exactly where the NBSP forbids
        // one.
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
                    unbreakable: false,
                },
                Word::Text {
                    text: "\u{00A0}world".to_owned(),
                    bold: true,
                    italic: false,
                    glue: true,
                    unbreakable: true,
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
                unbreakable: false,
            },
            Word::Text {
                text: "WWWW".to_owned(),
                bold: false,
                italic: false,
                glue: true,
                unbreakable: false,
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
    fn unbreakable_nbsp_pair_moves_together_when_it_does_not_fit() {
        // Regression: `Hello<strong>&nbsp;world</strong>` after earlier text
        // that leaves room for "Hello" but not the NBSP-glued "world" — the
        // overflow branch used to treat this exactly like ordinary glue
        // (`glued_run_that_cannot_fit_still_breaks_the_line` above), pushing
        // "Prefix Hello" together as a finished line and placing the
        // NBSP-led word alone on the next line — splitting the exact
        // boundary NBSP forbids a break at. An NBSP pair that doesn't fit
        // must move to the new line *together*, not split.
        let words = vec![
            Word::Text {
                text: "WWWW".to_owned(), // stands in for "Prefix"
                bold: false,
                italic: false,
                glue: false,
                unbreakable: false,
            },
            Word::Text {
                text: "WWWW".to_owned(), // stands in for "Hello"
                bold: false,
                italic: false,
                glue: false,
                unbreakable: false,
            },
            Word::Text {
                text: "WWWW".to_owned(), // stands in for NBSP-led "world"
                bold: false,
                italic: false,
                glue: true,
                unbreakable: true,
            },
        ];
        let word_width = text_width_pt("WWWW", 12.0, false);
        let space_w = text_width_pt(" ", 12.0, false);
        // Fits "Prefix Hello" (two words + one space) but not a third glued
        // "WWWW" on top of that; a fresh line fits the NBSP pair alone
        // (two words, no space between them).
        let max_width_pt = 2.0f32.mul_add(word_width, space_w) + 0.5;
        let lines = wrap(&words, max_width_pt, 12.0);
        assert_eq!(
            lines.len(),
            2,
            "the NBSP pair must move to a new line rather than splitting across two"
        );
        assert_eq!(
            lines[0],
            vec![("WWWW".to_owned(), false, false, false)],
            "only the unrelated prefix word stays on the first line"
        );
        assert_eq!(
            lines[1],
            vec![
                ("WWWW".to_owned(), false, false, false),
                ("WWWW".to_owned(), false, false, true),
            ],
            "the NBSP-glued pair must move together onto the second line"
        );
    }

    #[test]
    fn unbreakable_word_that_is_individually_oversized_stays_glued_to_its_predecessor() {
        // Regression: `Hello<strong>&nbsp;` followed by a long unbroken run of
        // characters wider than a whole line. Fixed in two rounds:
        // 1. The oversized-token branch ran before the unbreakable check, so it
        //    unconditionally flushed `current` ("Hello") as its own finished line,
        //    losing the glue to the NBSP-led word, and then character-split the
        //    oversized word with no notion of the NBSP boundary. Fixed by checking
        //    `unbreakable` first.
        // 2. That fix went too far the other way: it glued the entire oversized word
        //    onto "Hello" with no splitting, so the whole run rendered as one
        //    unsplit, unbounded token — overflowing and clipped, not merely spilling
        //    past the margin. The NBSP forbids a break only at its own boundary, so
        //    the rest of the run is still character-split, with its first chunk kept
        //    glued to "Hello".
        let words = vec![
            Word::Text {
                text: "Hello".to_owned(),
                bold: false,
                italic: false,
                glue: false,
                unbreakable: false,
            },
            Word::Text {
                text: format!("\u{00A0}{}", "A".repeat(100)),
                bold: true,
                italic: false,
                glue: true,
                unbreakable: true,
            },
        ];
        let max_width_pt = 495.0; // a typical paragraph content width
        let word_width = text_width_pt(&format!("\u{00A0}{}", "A".repeat(100)), 11.0, true);
        assert!(
            word_width > max_width_pt,
            "fixture word must be individually oversized"
        );
        let lines = wrap(&words, max_width_pt, 11.0);
        assert!(
            lines.len() > 1,
            "the oversized NBSP-led word must still be character-split across multiple \
             lines instead of left whole, got {lines:?}"
        );
        assert_eq!(
            lines[0][0],
            ("Hello".to_owned(), false, false, false),
            "\"Hello\" must not be flushed onto its own line ahead of the glued word"
        );
        assert!(
            lines[0][1].0.starts_with('\u{00A0}'),
            "the first chunk of the NBSP-led word must stay glued (with its NBSP intact) \
             right after \"Hello\", got {:?}",
            lines[0][1]
        );
        assert!(
            lines[0][1].3,
            "the first chunk of the NBSP-led word must still render glued (no rendered \
             space before it)"
        );
        // Every line, including the first (whose first chunk is sized
        // against the space actually remaining after "Hello", not the
        // full column — see `oversized_glued_word_first_chunk_is_sized_to_the_remaining_line_width`
        // for the regression this guards), must actually fit.
        for (i, line) in lines.iter().enumerate() {
            let line_width: f32 = line
                .iter()
                .map(|(text, bold, _, _)| text_width_pt(text, 11.0, *bold))
                .sum();
            assert!(
                line_width <= max_width_pt,
                "line {i} exceeds max_width_pt ({line_width} > {max_width_pt}): {line:?}"
            );
        }
        let rejoined: String = lines
            .iter()
            .flat_map(|line| line.iter().map(|(text, ..)| text.as_str()))
            .collect();
        assert_eq!(
            rejoined,
            format!("Hello\u{00A0}{}", "A".repeat(100)),
            "splitting into chunks must not drop or duplicate any characters"
        );
    }

    #[test]
    fn oversized_glued_word_first_chunk_is_sized_to_the_remaining_line_width() {
        // Regression: the character-split fix above sized the *first*
        // chunk against the full `max_width_pt`, the same as every later
        // chunk — but the first chunk is appended to a line that already
        // has other content on it, so sizing it against the full column
        // still overflowed by roughly however wide that existing content
        // was. 40 "A"s (~294pt at this font size) followed by an
        // NBSP-glued run of "<strong>&nbsp;" + 100 more "A"s used to
        // produce a first chunk sized to ~489pt — combined with the
        // 40-"A" prefix, well past the 495pt column, with only later
        // chunks actually respecting `max_width_pt`.
        let words = vec![
            Word::Text {
                text: "A".repeat(40),
                bold: false,
                italic: false,
                glue: false,
                unbreakable: false,
            },
            Word::Text {
                text: format!("\u{00A0}{}", "A".repeat(100)),
                bold: true,
                italic: false,
                glue: true,
                unbreakable: true,
            },
        ];
        let max_width_pt = 495.0;
        let lines = wrap(&words, max_width_pt, 11.0);
        for (i, line) in lines.iter().enumerate() {
            let line_width: f32 = line
                .iter()
                .map(|(text, bold, _, _)| text_width_pt(text, 11.0, *bold))
                .sum();
            assert!(
                line_width <= max_width_pt,
                "line {i} exceeds max_width_pt ({line_width} > {max_width_pt}): {line:?}"
            );
        }
        let rejoined: String = lines
            .iter()
            .flat_map(|line| line.iter().map(|(text, ..)| text.as_str()))
            .collect();
        assert_eq!(
            rejoined,
            format!("{}\u{00A0}{}", "A".repeat(40), "A".repeat(100)),
            "splitting into chunks must not drop or duplicate any characters"
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
    fn semantic_sectioning_elements_keep_adjacent_blocks_separate() {
        // Regression: `section`/`article`/`main`/`header`/`footer` weren't
        // in the "wraps other block elements" arm alongside `div`, so they
        // fell through to the generic transparent-passthrough case — two
        // adjacent `<section>`s of loose text accumulated into the same
        // pending paragraph with no separator at all.
        let nodes = super::super::html::parse(
            "<main><section>Summary</section><section>Details</section></main>",
        );
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(
            blocks.len(),
            2,
            "expected 2 separate paragraphs, got {blocks:?}"
        );
        assert!(
            matches!(&blocks[0], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Summary".to_owned(), bold: false, italic: false,
            }])
        );
        assert!(
            matches!(&blocks[1], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Details".to_owned(), bold: false, italic: false,
            }])
        );
    }

    #[test]
    fn nav_and_aside_keep_adjacent_blocks_separate() {
        // Same bug as `semantic_sectioning_elements_keep_adjacent_blocks_separate`,
        // reported again for `nav`/`aside` after the first fix.
        let nodes = super::super::html::parse("<aside>Summary</aside><aside>Details</aside>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(
            blocks.len(),
            2,
            "expected 2 separate paragraphs, got {blocks:?}"
        );
        assert!(
            matches!(&blocks[0], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Summary".to_owned(), bold: false, italic: false,
            }])
        );
        assert!(
            matches!(&blocks[1], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Details".to_owned(), bold: false, italic: false,
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
    fn hr_inside_a_list_item_still_separates_adjacent_text() {
        // Regression: `<hr>` is a void element (no children), so it wasn't
        // in `is_block_boundary_in_inline_context` and fell through to the
        // generic transparent-wrapper case in `inline_spans` — recursing
        // into its (empty) children produced nothing, and no break was
        // inserted either, so `<li>Before<hr>After</li>` rendered
        // "BeforeAfter" with the rule silently vanishing.
        let nodes = super::super::html::parse("<ul><li>Before<hr>After</li></ul>");
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
                    text: "Before".to_owned(),
                    bold: false,
                    italic: false,
                },
                Span::Break,
                Span::Run {
                    text: "After".to_owned(),
                    bold: false,
                    italic: false,
                },
            ],
            "hr must still separate the text around it, not vanish and glue them together"
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
    fn list_nested_inside_a_table_header_cell_stays_bold() {
        // Regression: `inline_spans`'s `"ul"`/`"ol"` branch called
        // `inline_list_items` without passing through the ambient
        // `bold`/`italic` it had just been called with — so
        // `<th><ul><li>Header</li></ul></th>`, where `extract_table_rows`
        // starts `inline_spans` with `bold: true` for a `<th>` cell, lost
        // that bold styling for both the list's marker and its item text,
        // even though the same content would stay bold if it weren't
        // wrapped in a list.
        let nodes =
            super::super::html::parse("<table><tr><th><ul><li>Header</li></ul></th></tr></table>");
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(blocks.len(), 1);
        let Block::Table(rows) = &blocks[0] else {
            panic!("expected a table block");
        };
        assert_eq!(rows.len(), 1);
        let (spans, is_header) = &rows[0].cells[0];
        assert!(is_header);
        assert_eq!(
            spans,
            &[
                Span::Run {
                    text: "\u{2022} ".to_owned(),
                    bold: true,
                    italic: false,
                },
                Span::Run {
                    text: "Header".to_owned(),
                    bold: true,
                    italic: false,
                },
            ],
            "both the list marker and its item content must stay bold inside a <th>, got {spans:?}"
        );
    }

    #[test]
    fn nested_list_item_marker_stays_beside_paragraph_wrapped_content() {
        // Regression: `inline_list_items` pushes the marker `Run` directly
        // into `out`, then calls `inline_spans` for the item's content —
        // when that content starts with a block boundary (here `<p>`),
        // `inline_spans` pushes a break *before* it, which is correct when
        // something precedes it but here lands directly between the
        // marker and its own first line, splitting `<li><p>Child</p></li>`
        // into the marker alone on one line and "Child" on the next.
        let nodes =
            super::super::html::parse("<ul><li>Parent<ul><li><p>Child</p></li></ul></li></ul>");
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
            "the nested marker must stay on the same line as its paragraph-wrapped content"
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
    fn nested_table_inside_a_cell_keeps_its_rows_and_cells_separate() {
        // Regression: a `<table>` nested inside a `<td>` has no dedicated
        // `Block::Table` path (only a top-level table gets one) — its inner
        // `table`/`tr`/`td` nodes used to fall through `inline_spans`'s
        // generic transparent-wrapper case, so adjacent cells' text glued
        // directly together with no separator: `<td>A</td><td>B</td>`
        // rendered as "AB".
        let nodes = super::super::html::parse(
            "<table><tr><td><table><tr><td>A</td><td>B</td></tr></table></td></tr></table>",
        );
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
            "the nested table's cells must be line-break separated, not glued into \"AB\""
        );
    }

    #[test]
    fn omitted_p_close_before_a_table_still_produces_a_real_table_block() {
        // Regression: without an implied close, `<p>Intro<table>...</table>`
        // nested the table *inside* the still-open `<p>`, so `flatten_blocks`'s
        // `"p"` arm sent the whole thing through `inline_spans` — which has
        // no notion of a table — flattening its rows/cells into bare inline
        // text ("IntroAB") instead of a real `Block::Table`.
        let nodes = super::super::html::parse(
            "<p>Intro</p><table><tr><td>A</td><td>B</td></tr></table><p>After</p>",
        );
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(
            blocks.len(),
            3,
            "expected 3 separate blocks (p, table, p), got {blocks:?}"
        );
        assert!(
            matches!(&blocks[0], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Intro".to_owned(), bold: false, italic: false,
            }])
        );
        let Block::Table(rows) = &blocks[1] else {
            panic!("expected a real table block, got {:?}", blocks[1]);
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cells.len(), 2, "expected two separate cells");
        assert!(
            matches!(&blocks[2], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "After".to_owned(), bold: false, italic: false,
            }])
        );
    }

    #[test]
    fn omitted_head_close_before_body_does_not_discard_the_whole_document() {
        // Regression: without an implied close, `<body>` nested *inside* the
        // still-open `<head>` — and `head` is in `is_non_rendered`, so its
        // entire subtree (which would now include `<body>`) was discarded
        // wholesale, dropping the whole visible document, not just one
        // element's structure.
        let nodes = super::super::html::parse(
            "<html><head><title>X</title><body><p>Visible</p></body></html>",
        );
        let mut blocks = Vec::new();
        flatten_blocks(&nodes, 0, &mut blocks);
        assert_eq!(
            blocks.len(),
            1,
            "expected the <body>'s <p> to survive, got {blocks:?}"
        );
        assert!(
            matches!(&blocks[0], Block::Paragraph(spans) if spans == &[Span::Run {
                text: "Visible".to_owned(), bold: false, italic: false,
            }])
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
                    unbreakable: false,
                },
                Word::Text {
                    text: "world".to_owned(),
                    bold: false,
                    italic: false,
                    glue: false,
                    unbreakable: false,
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
    fn deeply_nested_table_sections_do_not_overflow_the_stack() {
        // nodes_contain_table_output (the depth-cap guard's own truncation
        // check) must stay stack-safe for an arbitrarily deep subtree, the
        // same way the renderer itself does — a naive recursive walk
        // through thead/tbody/tfoot would crash right where the cap was
        // supposed to protect against exactly this. The HTML parser
        // auto-closes a <thead>/<tbody>/<tfoot> when another one of the
        // three opens (see html::implicitly_closes), so real markup can
        // never actually nest them — this builds the Node tree directly
        // to test the function's own stack safety regardless of what the
        // current parser happens to allow. (Codex review on PR #2810.)
        //
        // 100,000 levels, not 2,000,000: a naive recursive walk overflows
        // well before this depth on any plausible native stack, and the
        // smaller tree avoids a ~190 MiB allocation spike that could make
        // this test OOM or stall on a memory-constrained CI worker running
        // tests in parallel. (Codex review on PR #2810.)
        let mut node = Node::Element {
            tag: "thead".to_owned(),
            children: Vec::new(),
        };
        for _ in 0..100_000 {
            node = Node::Element {
                tag: "thead".to_owned(),
                children: vec![node],
            };
        }
        // Must not panic/overflow.
        assert!(!nodes_contain_table_output(
            std::slice::from_ref(&node),
            false
        ));
    }

    // ── Depth-cap truncation must be visible, not silent (issue #2801) ──

    /// Counts `WARN` events at `target` seen while it is the default
    /// subscriber. Scoped to one thread by [`tracing::subscriber::set_default`],
    /// so parallel tests do not see each other's events.
    #[derive(Clone, Default)]
    struct WarnCounter {
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnCounter {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let meta = event.metadata();
            if meta.target() == "autumn::pdf" && *meta.level() == tracing::Level::WARN {
                self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Render `html` under a capture subscriber and return how many
    /// `autumn::pdf` warnings it emitted.
    fn count_pdf_depth_warnings(html: &str) -> usize {
        use tracing_subscriber::layer::SubscriberExt as _;

        let counter = WarnCounter::default();
        let subscriber = tracing_subscriber::registry().with(counter.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        render_pages(html);
        counter.count.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `n` levels of `<span>` wrapped around `MARKER` — the exact shape
    /// issue #2801 used to find the 512/513 cutover.
    fn nested_span_html(n: usize) -> String {
        let mut html = "<span>".repeat(n);
        html.push_str("MARKER");
        html.push_str(&"</span>".repeat(n));
        html
    }

    #[test]
    fn exactly_at_the_depth_cap_emits_no_warning() {
        assert_eq!(
            count_pdf_depth_warnings(&nested_span_html(512)),
            0,
            "512 levels is the documented cap, not past it — must not warn \
             (issue #2801's own n=512 case)"
        );
    }

    #[test]
    fn one_level_past_the_depth_cap_emits_one_warning() {
        assert_eq!(
            count_pdf_depth_warnings(&nested_span_html(513)),
            1,
            "513 levels is one past the cap — must log exactly one warning, \
             not zero (silent) and not one per truncated node \
             (issue #2801's own n=513 case)"
        );
    }

    #[test]
    fn whitespace_only_ul_past_the_depth_cap_does_not_warn() {
        // A <ul> with only whitespace between its tags (no <li> at all) has
        // a Text("\n") child. extract_list_items/inline_list_items skip
        // every node that isn't an <li> — including that whitespace text —
        // so it draws nothing. subtree_has_visible_content doesn't know
        // that: it treats non-empty text as content on its own, which is
        // right for the 4 general-purpose walkers but wrong for these two
        // list-only ones.
        let html = format!(
            "{}<ul>\n</ul>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "a <ul> with no real <li> draws nothing, whitespace or not, so this must not warn"
        );
    }

    #[test]
    fn whitespace_only_table_past_the_depth_cap_does_not_warn() {
        // Same shape as the <ul> case: extract_table_rows's loop skips any
        // node that isn't an Element (`let Node::Element { .. } = node else
        // { continue };`), so a <table> with only a newline between its
        // tags produces no row.
        let html = format!(
            "{}<table>\n</table>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "a <table> with no real row content draws nothing, so this must not warn"
        );
    }

    #[test]
    fn empty_tr_past_the_depth_cap_does_not_warn() {
        // extract_table_rows pushes a TableRow for any <tr>, even a
        // cell-less one — but Writer::draw_table returns immediately when
        // every row's cell count maxes out at 0 (n_cols == 0), so a table
        // that is nothing but empty <tr>s draws no mark at all.
        let html = format!(
            "{}<table><tr></tr></table>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "a <tr> with no <td>/<th> cells draws nothing, so this must not warn"
        );
    }

    #[test]
    fn empty_caption_past_the_depth_cap_does_not_warn() {
        // extract_table_rows's catch-all arm only pushes a row when the
        // tag's own inline content is non-empty; an empty <caption> (or any
        // other non-tr tag with nothing in it) produces none.
        let html = format!(
            "{}<table><caption></caption></table>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "an empty <caption> draws nothing, so this must not warn"
        );
    }

    #[test]
    fn table_with_real_cell_content_past_the_depth_cap_still_warns() {
        // Sanity check alongside the two tests above: a <tr> that DOES have
        // a real cell must still warn when dropped.
        let html = format!(
            "{}<table><tr><td>X</td></tr></table>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "a <tr> with a real <td> cell draws a row, so this must warn"
        );
    }

    #[test]
    fn empty_tr_past_the_depth_cap_still_warns_when_the_table_has_other_rows() {
        // Unlike the fully-empty-table case (empty_tr_past_the_depth_cap_does_not_warn),
        // a table with an earlier real-celled row already has a nonzero
        // Writer::draw_table n_cols — every row from then on, including a
        // later zero-cell one, still consumes a 14pt line and shifts
        // everything after it. Dropping a capped empty <tr> is therefore a
        // real layout change even though the row itself draws no visible
        // mark, once some other row in the table has a real cell.
        //
        // Calls extract_table_rows directly at an already-past-cap depth,
        // rather than building enough real HTML nesting to reach it: a
        // <tr>'s own cell content is checked 2 levels deeper than the row
        // itself (see the `depth + 2` in the "tr" arm below), so any HTML
        // deep enough to push a *sibling* row's own recursion past the cap
        // already drops this row's cell text too, at a shallower depth —
        // there's no depth where "the other row's content survives intact"
        // and "this row's recursion is capped" are both true at once.
        // (Codex review on PR #2810.)
        DEPTH_CAP_HIT.with(|hit| hit.set(false));
        let mut out = vec![TableRow {
            cells: vec![(
                vec![Span::Run {
                    text: "X".to_owned(),
                    bold: false,
                    italic: false,
                }],
                false,
            )],
        }];
        let empty_tr = Node::Element {
            tag: "tr".to_owned(),
            children: Vec::new(),
        };
        extract_table_rows(
            std::slice::from_ref(&empty_tr),
            MAX_DEPTH + 1,
            false,
            &mut out,
        );
        assert!(
            DEPTH_CAP_HIT.with(std::cell::Cell::get),
            "the table already has a real row, so the dropped empty <tr> still shifts layout"
        );
    }

    #[test]
    fn node_contains_a_populated_row_recognizes_catch_all_content() {
        // node_contains_a_populated_row (the "does a later sibling
        // populate this table" lookahead extract_table_rows' depth-cap
        // guard uses) only recognized a literal <tr> with a real cell —
        // but extract_table_rows's own catch-all arm (most commonly
        // <caption>) also turns real content into a one-cell row, which
        // is just as capable of making Writer::draw_table's n_cols
        // nonzero. A <caption> with real text must count too, the same
        // way nodes_contain_table_output's own catch-all arm already
        // does via subtree_has_nonempty_text. (Codex review on PR #2810.)
        //
        // Unit-tested directly on the helper rather than through
        // extract_table_rows/DEPTH_CAP_HIT end to end: a sibling <tr>'s
        // own recursion and a sibling <caption>'s own cell-content check
        // are both checked at the same depth + 1 relative to their shared
        // parent, so wrapping the whole thing deep enough to cap the
        // <tr>'s recursion caps the <caption>'s own content at the exact
        // same point too — there's no depth where only one of them is
        // capped, the same shallower-depth conflict
        // empty_tr_past_the_depth_cap_still_warns_when_the_table_has_other_rows's
        // doc comment already ran into for cell content specifically.
        let caption = Node::Element {
            tag: "caption".to_owned(),
            children: vec![Node::Text("X".to_owned())],
        };
        assert!(
            node_contains_a_populated_row(&caption),
            "a <caption> with real text draws a one-cell row via extract_table_rows's \
             catch-all arm, so this must count as a populated row"
        );
    }

    #[test]
    fn table_caption_with_an_empty_list_item_past_the_depth_cap_still_warns() {
        // A <li> inside a <ul> always draws a marker, even empty (see
        // empty_li_past_the_depth_cap_still_warns) — including one reached
        // through extract_table_rows's catch-all arm, which hands a
        // <caption>'s children to inline_spans same as anywhere else.
        // subtree_has_nonempty_text alone can't see this: the marker isn't
        // a literal Text node in the source HTML, it's synthesized by
        // inline_list_items.
        let html = format!(
            "{}<table><caption><ul><li></li></ul></caption></table>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "a real <li>'s marker draws even when the <li> itself is empty, so this must warn"
        );
    }

    #[test]
    fn table_caption_with_a_listless_ul_past_the_depth_cap_does_not_warn() {
        // A <ul> with no direct <li> child renders nothing: inline_spans
        // hands ul/ol to inline_list_items, which scans only direct <li>
        // children and ignores everything else without recursing into it
        // — so bare text inside the <ul> (not wrapped in an <li>) never
        // reaches the page. subtree_has_nonempty_text must not keep
        // scanning a listless <ul>'s descendants either. (Codex review on
        // PR #2810.)
        let html = format!(
            "{}<table><caption><ul>ignored</ul></caption></table>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "a <ul> with no direct <li> draws nothing, so this must not warn"
        );
    }

    #[test]
    fn table_caption_with_two_breaks_past_the_depth_cap_still_warns() {
        // Two literal <br> tags survive trim_trailing_break (it pops only
        // the last one), so extract_table_rows's catch-all arm pushes a
        // real one-cell row for the leftover break — but <br> pushes its
        // Span::Break directly, bypassing push_block_break's "no two
        // breaks in a row" suppression that every other break-producing
        // tag goes through. subtree_has_nonempty_text doesn't count <br>
        // at all, so it misses this case. (Codex review on PR #2810.)
        let html = format!(
            "{}<table><caption><br><br></caption></table>{}",
            "<span>".repeat(512),
            "</span>".repeat(512)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "two <br>s survive trimming and draw a row, so this must warn"
        );
    }

    #[test]
    fn empty_span_past_the_depth_cap_does_not_warn() {
        // N empty <span> wrappers around nothing, for several N past the
        // cap: every level is checked, not just the first one past it,
        // because a shallow "is this one slice empty" check only catches
        // the exact depth where the wrapper chain runs out — one level
        // deeper, that slice holds one more (still empty) wrapper element
        // and looks non-empty by slice length alone. (Codex review on PR
        // #2810, first at 513 levels, then again at 514.)
        for n in 513..=520 {
            let html = format!("{}{}", "<span>".repeat(n), "</span>".repeat(n));
            assert_eq!(
                count_pdf_depth_warnings(&html),
                0,
                "{n} empty nested wrappers drop no content, so this must not warn"
            );
        }
    }

    #[test]
    fn whitespace_only_text_past_the_depth_cap_does_not_warn() {
        // A lone space or newline is nonempty text, but words_of splits on
        // breakable whitespace and drops it, so draw_spans never draws or
        // advances for it — nothing is actually lost by truncating it.
        // (Codex review on PR #2810.)
        for content in ["\n", " ", "  \n  "] {
            let html = format!(
                "{}{}{}",
                "<span>".repeat(513),
                content,
                "</span>".repeat(513)
            );
            assert_eq!(
                count_pdf_depth_warnings(&html),
                0,
                "{content:?} draws nothing, so this must not warn"
            );
        }
    }

    #[test]
    fn whitespace_only_text_sandwiched_between_real_words_past_the_depth_cap_still_warns() {
        // Unlike the isolated case above, a dropped whitespace-only span
        // here sits right after real text ("A") that the surrounding
        // buffer already holds — so dropping it removes the one thing
        // stopping words_of from gluing "A" and "B" into "AB". (Codex
        // review on PR #2810.)
        let html = format!("A{}{}{}B", "<span>".repeat(513), " ", "</span>".repeat(513));
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "dropping this space would glue \"A\" and \"B\" together, so this must warn"
        );
    }

    #[test]
    fn whitespace_only_text_before_a_leading_space_past_the_depth_cap_does_not_warn() {
        // Unlike the case above, the later sibling here ("B", preceded by
        // its own literal space) already carries its own separator —
        // words_of clears the glue boundary on that leading space
        // regardless of whether the capped whitespace survives, so both
        // capped and uncapped output render "A B" identically. (Codex
        // review on PR #2810.)
        let html = format!(
            "A{}{}{} B",
            "<span>".repeat(513),
            " ",
            "</span>".repeat(513)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "\" B\"'s own leading space already separates it from \"A\", so this must not warn"
        );
    }

    #[test]
    fn whitespace_only_text_before_a_whitespace_sibling_past_the_depth_cap_does_not_warn() {
        // Same idea, but the separator is a distinct whitespace-only
        // sibling ahead of "B" rather than leading whitespace within the
        // same text node — still resolves the glue risk on its own.
        // (Codex review on PR #2810.)
        let html = format!(
            "A{}{}{}<span> </span>B",
            "<span>".repeat(513),
            " ",
            "</span>".repeat(513)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "the whitespace-only sibling before \"B\" already separates it from \"A\", so this must not warn"
        );
    }

    #[test]
    fn whitespace_only_text_at_end_of_document_past_the_depth_cap_does_not_warn() {
        // A dropped whitespace-only span right after real text ("A") looks
        // exactly like the sandwiched case above from `out`'s trailing
        // content alone — but with nothing after it, there is no following
        // word for the space to separate: the rendered text is "A" either
        // way. (Codex review on PR #2810.)
        let html = format!("A{}{}{}", "<span>".repeat(513), " ", "</span>".repeat(513));
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "nothing follows this space, so dropping it changes nothing"
        );
    }

    #[test]
    fn whitespace_only_text_followed_by_non_rendered_content_past_the_depth_cap_does_not_warn() {
        // A later sibling exists, but a <script> never renders anything —
        // so the capped whitespace still has no real word to separate.
        // (Codex review on PR #2810.)
        let html = format!(
            "A{}{}{}<script>x</script>",
            "<span>".repeat(513),
            " ",
            "</span>".repeat(513)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "a <script> sibling never renders, so this must not warn"
        );
    }

    #[test]
    fn glue_lookahead_does_not_cross_an_enclosing_block_boundary() {
        // inline_spans always wraps a <div> in push_block_break before AND
        // after recursing into its children — so whatever the div's own
        // deeply-capped content does, "B" outside it can never glue to
        // "A" inside it: the trailing break already separates them either
        // way. The lookahead passed into the div's own children must not
        // inherit "B follows the div" as a reason to warn. (Codex review
        // on PR #2810.)
        let html = format!(
            "<table><tr><td><div>A{}{}{}</div>B</td></tr></table>",
            "<span>".repeat(513),
            " ",
            "</span>".repeat(513)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "push_block_break already separates the div from \"B\" regardless, so this must not warn"
        );
    }

    #[test]
    fn glue_lookahead_over_many_siblings_is_linear_not_quadratic() {
        // Regression: computing more_after fresh for every sibling
        // (later_content_could_glue(&nodes[i+1..]) inside the loop) scans
        // the whole remaining suffix on every iteration — O(n) per node,
        // O(n^2) overall for n flat top-level siblings, even nowhere near
        // the depth cap. (Codex review on PR #2810.)
        let html = "<span>x</span>".repeat(20_000);
        let start = std::time::Instant::now();
        let pages = render_pages(&html);
        assert!(!pages.is_empty());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "render_pages took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn glue_lookahead_over_a_deep_chain_is_linear_not_quadratic() {
        // Regression: glue_after_each(nodes, ...) scans nodes[0]'s entire
        // subtree via node_glue_lookahead even though the caller only ever
        // reads more_after[i + 1..] — never more_after[i] — so that scan is
        // wasted. For a single-child wrapper chain, nodes[0]'s subtree IS
        // the rest of the (possibly huge) chain, and this call happens
        // fresh at every one of the ~512 levels the depth cap allows
        // before it stops native recursion: O(depth) calls, each O(chain
        // length), instead of O(chain length) total. (Codex review on PR
        // #2810.)
        let n = 200_000;
        let html = format!("{}{}", "<span>".repeat(n), "</span>".repeat(n));
        let start = std::time::Instant::now();
        let pages = render_pages(&html);
        assert!(!pages.is_empty());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "render_pages took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn glue_lookahead_over_a_deep_two_child_chain_is_linear_not_quadratic() {
        // Regression: the prior fix only stopped glue_after_each from
        // scanning nodes[0]'s own subtree — but with 2 siblings per level
        // (an empty tag plus the deep chain), nodes[1] is scanned in full
        // via node_glue_lookahead to build after[0], and that still
        // happens fresh at every one of the ~512 levels the depth cap
        // allows: O(depth) calls, each O(chain length), same blowup in a
        // shape the single-child test doesn't cover. (Codex review on PR
        // #2810.)
        let n = 200_000;
        let html = format!(
            "{}{}{}",
            "<span><i></i>".repeat(n),
            "x",
            "</span>".repeat(n)
        );
        let start = std::time::Instant::now();
        let pages = render_pages(&html);
        assert!(!pages.is_empty());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "render_pages took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn whitespace_only_text_followed_only_by_more_whitespace_past_the_depth_cap_does_not_warn() {
        // A later sibling exists and is even nonempty text, but it too is
        // whitespace-only — still nothing for the capped space to glue.
        // (Codex review on PR #2810.)
        let html = format!("A{}{}{} ", "<span>".repeat(513), " ", "</span>".repeat(513));
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "a whitespace-only sibling never renders a word, so this must not warn"
        );
    }

    #[test]
    fn hr_past_the_depth_cap_still_warns() {
        // A dropped <hr> has no text, but it still draws a visible rule
        // (or, in an inline context, a line break) — losing it is a real
        // content loss, so "no text" must not mean "nothing to warn about".
        let html = format!("{}<hr>{}", "<span>".repeat(513), "</span>".repeat(513));
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "a dropped <hr> is dropped visible content, so this must warn"
        );
    }

    #[test]
    fn empty_li_past_the_depth_cap_still_warns() {
        // An empty <li> still draws a marker and reserves a line (see
        // empty_list_item_still_reserves_a_full_line) even with no text of
        // its own, so dropping it is a real content loss too.
        let html = format!(
            "{}<ul><li></li></ul>{}",
            "<span>".repeat(513),
            "</span>".repeat(513)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "a dropped empty <li> still draws a marker, so this must warn"
        );
    }

    #[test]
    fn script_text_past_the_depth_cap_does_not_warn() {
        // <script>'s text is never rendered, capped or not (see
        // script_and_style_content_is_never_rendered), so losing it to the
        // depth cap is not a real content loss.
        let html = format!(
            "{}<script>alert('x')</script>{}",
            "<span>".repeat(513),
            "</span>".repeat(513)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            0,
            "script text was never going to render, so this must not warn"
        );
    }

    #[test]
    fn br_past_the_depth_cap_still_warns() {
        // <br>, like <hr>, draws no text but still becomes a Span::Break —
        // a real (if small) piece of output, so it must warn like <hr>
        // does, not stay silent because it has no text of its own.
        let html = format!("{}<br>{}", "<span>".repeat(513), "</span>".repeat(513));
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "a dropped <br> is dropped output (a line break), so this must warn"
        );
    }

    #[test]
    fn bare_li_outside_a_list_past_the_depth_cap_still_warns() {
        // A stray <li> with no enclosing <ul>/<ol> isn't a list item, but
        // it is still a "flush point": flatten_into_pending closes off
        // whatever text came before it into its own paragraph the moment
        // it sees a <li> (same as <div>, <p>, ...), so an <li> sitting
        // between two runs of real text keeps them on separate lines
        // instead of gluing them — even though the <li> itself is empty.
        // Dropping it changes the output, so this must warn.
        let html = format!("{}<li></li>{}", "<span>".repeat(513), "</span>".repeat(513));
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "a stray <li> is still a structural flush point, so this must warn"
        );
    }

    #[test]
    fn empty_div_sandwiched_between_text_past_the_depth_cap_still_warns() {
        // An empty <div> between "A" and "B", buried past the cap, is a
        // real (if easy to miss) rendering difference: uncapped, it forces
        // "A" and "B" into separate paragraphs (flatten_into_pending flushes
        // pending text into its own Block::Paragraph the instant it sees a
        // <div>, then hands the (empty) <div> to flatten_blocks, which adds
        // nothing further); dropped, "A" and "B" merge into one paragraph.
        // Same story inline: inline_spans always pushes a Span::Break
        // around a <div> (see is_block_boundary_in_inline_context's doc
        // comment), empty or not.
        let html = format!(
            "A{}<div></div>{}B",
            "<span>".repeat(513),
            "</span>".repeat(513)
        );
        assert_eq!(
            count_pdf_depth_warnings(&html),
            1,
            "a dropped empty <div> still separates its neighbors, so this must warn"
        );
    }

    #[test]
    fn shallow_nesting_emits_no_warning() {
        let html = "<p>Hello <strong>world</strong></p>";
        assert_eq!(
            count_pdf_depth_warnings(html),
            0,
            "ordinary shallow content must never log a depth-cap warning"
        );
    }

    #[test]
    fn ordered_list_marker_wide_enough_to_overlap_the_fixed_indent_gets_more_room() {
        // Regression: the indent between a list marker and its item's
        // content was a fixed 16pt, which fits every bullet/low-numbered
        // marker comfortably ("•", "1." .. "9.") but not an ordered list
        // marker whose digits keep growing — "100." alone is already
        // ~21pt at 11pt Helvetica, wider than the indent, so content wrapped
        // at a fixed 16pt started underneath the marker's own text instead
        // of after it.
        let mut writer = Writer::new();
        let marker = "100.".to_owned();
        writer.draw_block(&Block::ListItem {
            marker: marker.clone(),
            spans: vec![Span::Run {
                text: "Item".to_owned(),
                bold: false,
                italic: false,
            }],
        });
        let cursor_xs: Vec<f32> = writer
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::SetTextCursor { pos } => Some(pos.x.0),
                _ => None,
            })
            .collect();
        assert_eq!(
            cursor_xs.len(),
            2,
            "expected one cursor position for the marker and one for the item's text"
        );
        let (marker_x, content_x) = (cursor_xs[0], cursor_xs[1]);
        let marker_width = text_width_pt(&marker, 11.0, false);
        assert!(
            content_x - marker_x >= marker_width,
            "content (x={content_x}) must start at or past the end of the marker \
             (x={marker_x} + width={marker_width}), not overlap it"
        );
    }

    #[test]
    fn empty_list_item_still_reserves_a_full_line() {
        // Regression: `draw_lines` only advances `y_from_top` per *line it
        // draws* — an empty item (`<li></li>`, or one whose only content was
        // skipped) produces zero wrapped lines, so only the fixed 4pt
        // spacer after it separated its marker from the next item's,
        // placing the two markers almost on top of each other instead of on
        // their own lines.
        let mut writer = Writer::new();
        writer.draw_block(&Block::ListItem {
            marker: "\u{2022}".to_owned(),
            spans: vec![],
        });
        let advance = writer.y_from_top;
        assert!(
            advance >= 14.5,
            "an empty list item must still advance a full line's height, got {advance}"
        );
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
