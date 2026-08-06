//! A small, tolerant HTML-subset parser.
//!
//! This is **not** a general HTML5 parser: it is just enough tag/text/entity
//! handling to walk a Maud-rendered (or hand-written) HTML string into a tree
//! [`crate::pdf::layout`] can lay out as PDF text. It never panics on
//! malformed input — unmatched closing tags are ignored, unclosed tags are
//! auto-closed at end of input, and it parses iteratively (no recursion) so
//! adversarially deep nesting can't blow the stack.

/// A parsed node: either a run of text or an element with children.
#[derive(Debug, PartialEq)]
pub(super) enum Node {
    Text(String),
    Element { tag: String, children: Vec<Self> },
}

impl Drop for Node {
    /// Drop a (possibly very deep) tree iteratively.
    ///
    /// The compiler-generated `Drop` glue for a recursive type like this one
    /// would drop each level's children by recursing — for a pathologically
    /// deep tree (see the parser's own stack-safety test) that overflows the
    /// stack even though *parsing* never recurses. This flattens the tree
    /// into an explicit heap-allocated work list instead: each popped node's
    /// children are moved out onto the list before the (now childless, so
    /// trivially-dropped) node itself goes out of scope.
    fn drop(&mut self) {
        let mut stack: Vec<Self> = if let Self::Element { children, .. } = self {
            std::mem::take(children)
        } else {
            return;
        };
        while let Some(mut node) = stack.pop() {
            if let Self::Element { children, .. } = &mut node {
                stack.append(children);
            }
        }
    }
}

/// HTML elements with no content and no closing tag.
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "br" | "hr"
            | "img"
            | "input"
            | "meta"
            | "link"
            | "area"
            | "base"
            | "col"
            | "embed"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Whether opening `new_tag` implicitly closes an `open_tag` still open at
/// the top of the stack — HTML5's "optional end tag" rule, scoped to the
/// tags this renderer gives special (marker/cell/row/block) treatment to.
/// Real-world or hand-written HTML commonly omits these closing tags
/// (`<ul><li>One<li>Two</ul>`, `<tr><td>A<td>B</table>`,
/// `<p>Intro<table>...</table>`); without this, the new tag nests *inside*
/// the still-open previous one instead of becoming its sibling.
///
/// For `li`/`dt`/`dd`/`tr`/`td`/`th`, `extract_list_items`/
/// `extract_table_rows` (in `layout.rs`) only look at *direct* children, so
/// a nested-instead-of-sibling element's marker/cell/row boundary becomes
/// invisible to them (a second bullet silently disappears, a second cell
/// silently merges into the first). For `p`, the failure mode is
/// different but just as real: a still-open `<p>` swallows the next
/// supported block element as inline content instead of letting it become
/// its own top-level [`Block`](super::layout) — `<p>Intro<table>...</table>`
/// flattens the table's rows/cells into bare inline text (`IntroAB`
/// instead of a real table) since `inline_spans` has no notion of a table.
/// For `head`, the failure mode is the most severe of all: `head` is in
/// `is_non_rendered` (in `layout.rs`), so nesting anything under a
/// still-open `head` discards the *entire visible document* wholesale, not
/// just one element's structure. HTML5 permits omitting *both* `</head>`
/// (`<head><title>X</title><body>...`) *and* the `<body>` start tag itself
/// (`<head><title>X</title><p>...`, or even bare text with no wrapper tag
/// at all) — either way, the first tag or non-whitespace text that isn't
/// valid content for `<head>` (see [`is_valid_in_head`]) implicitly closes
/// it, matching the "in head" insertion mode's behavior for any
/// unexpected token, not only an explicit `<body>`.
///
/// Doesn't need to cover every HTML5 optional-end-tag rule, only the ones
/// for tags this renderer actually gives that special treatment to.
fn implicitly_closes(open_tag: &str, new_tag: &str) -> bool {
    (open_tag == "p" && closes_open_paragraph(new_tag))
        || (open_tag == "head" && !is_valid_in_head(new_tag))
        || matches!(
            (open_tag, new_tag),
            ("li", "li")
                | ("dt" | "dd", "dt" | "dd")
                | ("tr", "tr")
                | ("td" | "th", "td" | "th" | "tr")
        )
}

/// Tags HTML5 permits directly inside `<head>` — anything else implies
/// `</head>` before it opens; see [`implicitly_closes`].
fn is_valid_in_head(tag: &str) -> bool {
    matches!(
        tag,
        "head" | "title" | "base" | "link" | "meta" | "style" | "script" | "noscript" | "template"
    )
}

/// Tags that, per HTML5's `<p>` implied-end-tag rule, close an open `<p>`
/// when they start — restricted to headings and the block-level container
/// tags this renderer gives real block/list/table structure to, since only
/// those have the "swallowed as inline content" failure mode
/// [`implicitly_closes`] exists to prevent.
fn closes_open_paragraph(new_tag: &str) -> bool {
    matches!(
        new_tag,
        "p" | "div"
            | "blockquote"
            | "dl"
            | "dt"
            | "dd"
            | "hr"
            | "table"
            | "ul"
            | "ol"
            | "li"
            | "section"
            | "article"
            | "main"
            | "header"
            | "footer"
            | "nav"
            | "aside"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

/// "Raw text" elements per the HTML5 parsing spec: their content is never
/// tokenized as markup at all, even if it contains characters that look
/// like tags (e.g. a JS comparison `a<b` inside `<script>`, or a CSS `>`
/// combinator inside `<style>`) — only the literal closing tag ends them.
/// Without this, such a `<` can be parsed as a bogus opening tag that
/// swallows the real `</script>`, leaving the element unclosed and nesting
/// (and, since `is_non_rendered` in `layout.rs` skips its subtree, hiding)
/// everything that follows.
///
/// `title` and `textarea` are technically RCDATA elements (character
/// references still decode, unlike true raw text such as `script`/`style`)
/// rather than raw-text ones, but both are scanned the same way here: the
/// caller in [`parse`] runs [`decode_entities`] over whatever text this
/// returns, which is a no-op for `script`/`style` (their content is
/// discarded wholesale by `is_non_rendered` in `layout.rs` regardless) and
/// correct for `title`/`textarea` (real content that still needs its
/// entities decoded). What matters for all four is that their content must
/// not be tokenized as markup: a tag-looking sequence such as
/// `<title>a<b</title>` or `<textarea>a<b</textarea>` would otherwise let
/// `<b` consume the real closing tag the same way an unhandled `<script>`
/// body could, leaving everything after it nested (and, for `title`,
/// hidden) inside an unclosed element — or, for `textarea`, simply lost.
fn is_raw_text_element(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "title" | "textarea")
}

/// Bound on how far [`consume_raw_text`] scans past a candidate
/// `</script`/`</style` prefix looking for the closing `>` — the same
/// defense as [`MAX_TAG_SCAN`] for the same reason: a legitimate closing
/// tag has no attributes and essentially no whitespace before `>`, but an
/// unbounded `after_tag.find('>')` would cost O(remaining input) on every
/// failed candidate — a raw-text body containing many `"</script "`
/// fragments with no `>` anywhere would make this O(n^2) overall, the same
/// shape of bug [`MAX_TAG_SCAN`]/[`MAX_CLOSE_SCAN`] were fixed for.
const MAX_RAW_CLOSE_SCAN: usize = 64;

/// Scan `input[pos..]` for the literal, case-insensitive closing tag for a
/// [raw text element](is_raw_text_element) (e.g. `</script>`, allowing
/// whitespace before the `>`), returning the text before it and the
/// position just past the closing tag. If no closing tag is found, all of
/// `input[pos..]` is returned as text with the position at end of input —
/// matching this parser's usual "auto-close at EOF" tolerance for
/// unterminated tags.
fn consume_raw_text<'a>(input: &'a str, pos: usize, tag: &str) -> (&'a str, usize) {
    let rest = &input[pos..];
    for (i, _) in rest.match_indices('<') {
        let Some(after_slash) = rest[i + 1..].strip_prefix('/') else {
            continue;
        };
        if after_slash.len() < tag.len()
            || !after_slash.as_bytes()[..tag.len()].eq_ignore_ascii_case(tag.as_bytes())
        {
            continue;
        }
        let after_tag = &after_slash[tag.len()..];
        // Must be immediately followed by whitespace or '>' — not e.g.
        // "</scripty>" merely starting with "script".
        let is_boundary = after_tag
            .chars()
            .next()
            .is_none_or(|c| c == '>' || c.is_whitespace());
        if !is_boundary {
            continue;
        }
        let Some(gt) = bounded_prefix(after_tag, MAX_RAW_CLOSE_SCAN).find('>') else {
            continue;
        };
        let consumed = i + 2 + tag.len() + gt + 1;
        return (&rest[..i], pos + consumed);
    }
    (rest, input.len())
}

/// Bound on how many frames back [`parse`]'s closing-tag matcher scans
/// looking for the nearest open tag with a given name. Well-formed HTML
/// (even deeply nested) closes tags in the order they were opened, so a
/// match is almost always found in the last frame or two; scanning the
/// *entire* stack on every close only matters for malformed input that
/// closes an ancestor while many descendants are still open. Without a
/// bound, a long run of opens followed by a long run of non-matching closes
/// (e.g. `"<a>".repeat(n) + "</x>".repeat(n)`, where "x" never appears on
/// the stack so no close ever pops it) costs O(n) per close for O(n^2)
/// overall — the same shape of bug already fixed for `decode_entities` and
/// `parse_open_tag`. A close whose matching opener is further back than
/// this is treated the same as a close with no matching opener at all:
/// ignored, rather than auto-closing a deep run of intervening tags.
const MAX_CLOSE_SCAN: usize = 512;

/// Parse `input` into a forest of top-level [`Node`]s.
pub(super) fn parse(input: &str) -> Vec<Node> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    // Stack of (tag_name, children-so-far). The implicit root is index 0 with
    // an empty tag name; it is never popped.
    let mut stack: Vec<(String, Vec<Node>)> = vec![(String::new(), Vec::new())];

    while pos < len {
        if bytes[pos] == b'<' {
            if input[pos..].starts_with("<!--") {
                let end = input[pos..].find("-->").map_or(len, |i| pos + i + 3);
                pos = end;
                continue;
            }
            if input[pos..].starts_with("<!") || input[pos..].starts_with("<?") {
                // Doctype / processing-instruction-like: skip to the next '>'.
                let end = input[pos..].find('>').map_or(len, |i| pos + i + 1);
                pos = end;
                continue;
            }
            if let Some(rest) = input[pos..].strip_prefix("</") {
                let name_end = rest.find('>').unwrap_or(rest.len());
                let name = rest[..name_end].trim().to_ascii_lowercase();
                pos += 2 + name_end + usize::from(name_end < rest.len());

                // Close frames up to and including the matching open tag, if
                // any exists within the last MAX_CLOSE_SCAN frames of the
                // stack. A stray/mismatched close tag with no matching
                // opener nearby is ignored rather than corrupting the tree.
                // An empty tag name (`</>`) must never match: the implicit
                // root frame is *also* keyed by an empty string (it has no
                // tag), and popping it would leave `stack` empty.
                let matching_depth = if name.is_empty() {
                    None
                } else {
                    let window_start = stack.len().saturating_sub(MAX_CLOSE_SCAN);
                    stack[window_start..]
                        .iter()
                        .rposition(|(tag, _)| *tag == name)
                        .map(|i| window_start + i)
                };
                if let Some(depth) = matching_depth {
                    while stack.len() > depth {
                        let (tag, children) = stack.pop().expect("depth <= stack.len()");
                        stack
                            .last_mut()
                            .expect("root frame is never popped")
                            .1
                            .push(Node::Element { tag, children });
                    }
                }
                continue;
            }
            if let Some((tag, self_closing, tag_end)) = parse_open_tag(&input[pos..]) {
                pos += tag_end;
                close_implied_tags(&mut stack, &tag);

                if !self_closing && is_raw_text_element(&tag) {
                    // `<script>`/`<style>`/`<title>`/`<textarea>` content is
                    // never tokenized as markup — see `consume_raw_text` —
                    // so a `<` that merely *looks* like the start of a tag
                    // (a JS comparison, a CSS selector combinator, a `<`
                    // typed into a textarea's default value) can't swallow
                    // the real closing tag. Entities are still decoded
                    // (matching the normal text-node path below) — a no-op
                    // for the two tags whose content is discarded anyway,
                    // and required for `title`/`textarea`'s real content.
                    let (text, new_pos) = consume_raw_text(input, pos, &tag);
                    pos = new_pos;
                    let mut children = Vec::new();
                    if !text.is_empty() {
                        children.push(Node::Text(decode_entities(text)));
                    }
                    stack
                        .last_mut()
                        .expect("root frame is never popped")
                        .1
                        .push(Node::Element { tag, children });
                    continue;
                }
                if self_closing || is_void_element(&tag) {
                    stack
                        .last_mut()
                        .expect("root frame is never popped")
                        .1
                        .push(Node::Element {
                            tag,
                            children: Vec::new(),
                        });
                } else {
                    stack.push((tag, Vec::new()));
                }
                continue;
            }
            // A lone '<' that isn't a recognizable tag: treat as literal text.
            push_text(&mut stack, "<");
            pos += 1;
            continue;
        }

        let next_lt = input[pos..].find('<').map_or(len, |i| pos + i);
        let raw = &input[pos..next_lt];
        if !raw.is_empty() {
            push_text(&mut stack, &decode_entities(raw));
        }
        pos = next_lt;
    }

    // Auto-close any still-open tags at end of input.
    while stack.len() > 1 {
        let (tag, children) = stack.pop().expect("stack.len() > 1");
        stack
            .last_mut()
            .expect("root frame is never popped")
            .1
            .push(Node::Element { tag, children });
    }

    stack.pop().expect("root frame always present").1
}

/// Inline-formatting tags this parser treats as transparent wrappers around
/// their content (`layout.rs`'s `inline_spans` recurses straight through
/// them, tracking bold/italic). Unlike a genuine block container (`ul`,
/// `table`, `div`, ...), one of these sitting directly above a still-open
/// frame doesn't change *what* new content belongs to — so
/// [`close_implied_tags`] may look straight through a run of them when
/// deciding whether `new_tag`'s opening should close something further
/// down, the same way real HTML5 closes enclosing formatting elements
/// along with whatever they're inside when that container closes.
fn is_phrasing_wrapper(tag: &str) -> bool {
    matches!(tag, "strong" | "b" | "em" | "i")
}

/// Cascades [`implicitly_closes`] up `stack`: repeatedly finds the closest
/// still-open frame that opening `new_tag` implicitly closes and pops down
/// to it, so e.g. a new `<tr>` closes both an open `<td>` *and* the `<tr>`
/// above it (found and closed one loop iteration apart), not just the
/// immediate stack top. Extracted out of [`parse`] purely to keep that
/// function's line count down — this has no state of its own beyond
/// `stack`.
///
/// Looks *past* a run of [`is_phrasing_wrapper`] frames at the top, not
/// just at the top itself: `<p><strong>Intro<table>` has `strong` sitting
/// directly above the still-open `p` when `<table>` opens, and `strong`
/// has no implied-close rule of its own against `table` — checking only
/// `stack.last()` would stop right there and never see the `p` beneath,
/// leaving the table nested inside it (and flattened through
/// `inline_spans`, which has no notion of a table, instead of becoming a
/// real `Block::Table`). Only phrasing wrappers are skipped this way — a
/// genuine block container (`ul`, `table`, ...) sitting at the top always
/// stops the search at that frame, matching or not, so e.g.
/// `<ul><li>Parent<ul><li>Child` correctly leaves the inner `<li>` opening
/// *inside* the inner `<ul>` rather than reaching past it to close the
/// outer `<li>` two levels up.
fn close_implied_tags(stack: &mut Vec<(String, Vec<Node>)>, new_tag: &str) {
    loop {
        if stack.len() <= 1 {
            break;
        }
        // Bounded the same way the closing-tag matcher in `parse` bounds
        // its scan (`MAX_CLOSE_SCAN`) and for the same reason: this now
        // runs on *every* opening tag, so an unbounded walk through a
        // stack that's nothing but phrasing wrappers (e.g. `n` unclosed
        // `<strong>`s, which never stop the walk early) would be an O(n)
        // scan repeated n times — O(n²) overall, the same blowup already
        // fixed elsewhere in this parser for a pathologically deep
        // (adversarial or accidental) stack. A stack of anything *else*
        // that never closes (e.g. `n` unclosed `<span>`s) stops the walk
        // on its very first frame, so this bound only matters for the
        // phrasing-wrapper case.
        let floor = stack.len().saturating_sub(MAX_CLOSE_SCAN).max(1);
        let mut target = stack.len() - 1;
        while target > floor && is_phrasing_wrapper(&stack[target].0) {
            target -= 1;
        }
        if !implicitly_closes(&stack[target].0, new_tag) {
            break;
        }
        while stack.len() > target {
            let (closed_tag, children) = stack.pop().expect("stack.len() > target >= 1");
            stack
                .last_mut()
                .expect("root frame is never popped")
                .1
                .push(Node::Element {
                    tag: closed_tag,
                    children,
                });
        }
    }
}

fn push_text(stack: &mut Vec<(String, Vec<Node>)>, text: &str) {
    if text.is_empty() {
        return;
    }
    // Non-whitespace text is just as invalid inside `<head>` as an
    // unexpected tag — same implied close as `implicitly_closes`'s `head`
    // case, just triggered by content instead of a new element (e.g.
    // `<head><title>X</title>Visible</head>`, or the same with no closing
    // tag at all). Whitespace-only text (formatting indentation between
    // tags) doesn't count — it's not real content.
    if stack.last().is_some_and(|(tag, _)| tag == "head")
        && text.contains(|c: char| !c.is_whitespace())
    {
        let (closed_tag, children) = stack.pop().expect("just checked stack.last() above");
        stack
            .last_mut()
            .expect("root frame is never popped")
            .1
            .push(Node::Element {
                tag: closed_tag,
                children,
            });
    }
    let top = stack.last_mut().expect("root frame is never popped");
    if let Some(Node::Text(prev)) = top.1.last_mut() {
        prev.push_str(text);
    } else {
        top.1.push(Node::Text(text.to_owned()));
    }
}

/// Parse an opening tag starting at `s[0] == '<'`.
///
/// Returns `(tag_name, self_closing, total_consumed_len)`, or `None` if `s`
/// doesn't start with a well-formed tag (e.g. `< foo>` with a space, or an
/// unterminated `<foo`).
///
/// Attributes are ignored entirely (no CSS support), but a literal `>`
/// inside a quoted attribute value is still handled correctly — see
/// [`find_tag_end`] — rather than being mistaken for the tag's own closing
/// delimiter.
/// Bound on how far [`parse_open_tag`] scans looking for the closing `>`. A
/// long run of unterminated `<tag` fragments with no `>` anywhere
/// (adversarial or just malformed input, e.g. `"<a".repeat(n)`) would
/// otherwise make that scan cover the *entire remainder* of the document —
/// and because a failed parse doesn't consume any input (the caller falls
/// back to treating just the `<` as literal text and retries at the very
/// next byte), that unbounded cost gets paid again at every subsequent `<` —
/// O(n^2) overall, the same shape of bug `decode_entities` was fixed for.
/// This bound must stay in place (an earlier draft that dropped it entirely
/// reintroduced the O(n^2) scan cost) but doesn't need to be tight: the
/// tag-name allocation below only happens *after* `>` is found, so a large
/// window costs nothing extra on the (bounded-scan, no-allocation) failure
/// path — only real matches pay for the window size, and a real match is
/// one allocation per tag in the document, not per byte scanned. Sized
/// generously (4 KiB) so a long but genuine attribute list — a Tailwind
/// utility-class soup, several `data-*`/`aria-*` attributes — still parses
/// instead of being rejected and leaked into the PDF as literal text.
const MAX_TAG_SCAN: usize = 4096;

/// Take a byte-length-bounded, UTF-8-safe prefix of `s` — cheap regardless
/// of `max_len`'s size (no per-char iterator overhead, unlike walking
/// `s.char_indices()` up to the bound), nudged back to the nearest char
/// boundary so the result can't split a multi-byte character. Used to cap
/// a scan for a delimiter (`>`) at a fixed cost instead of the full
/// remaining input length — see [`MAX_TAG_SCAN`] and
/// [`MAX_RAW_CLOSE_SCAN`] for why that bound matters.
fn bounded_prefix(s: &str, max_len: usize) -> &str {
    let mut end = s.len().min(max_len);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Find the byte offset of the `>` that ends an opening tag within `window`,
/// skipping any `>` that appears inside a quoted attribute value (e.g.
/// `<div title="Balance > 100">`) — otherwise that quoted `>` is mistaken
/// for the tag's real closing delimiter, truncating the tag early and
/// leaking the rest of the attribute value into the document as literal
/// text.
///
/// Takes the fast, heavily-optimized [`str::find`] path whenever `window`
/// contains no quote character at all — the overwhelmingly common case, and
/// exactly what the existing quadratic-scan regression tests exercise (long
/// runs of unterminated tags with no attributes) — so this doesn't reopen
/// the O(n^2) cost [`MAX_TAG_SCAN`] exists to bound. Only pays for the
/// slower quote-tracking scan when a `"`/`'` is actually present, still
/// bounded by the same `window` (already capped to [`MAX_TAG_SCAN`] bytes
/// by the caller) either way.
fn find_tag_end(window: &str) -> Option<usize> {
    // Two single-char `contains` calls, not one `contains(['"', '\''])` —
    // `str`'s multi-char-pattern search isn't memchr-accelerated the way a
    // single literal `char` pattern is, and using it here regressed the
    // quadratic-scan timing tests by roughly 10x despite being only a
    // pre-check (verified by actually timing it, not just complexity-class
    // reasoning — see the module's other `MAX_TAG_SCAN`-adjacent history).
    if !window.contains('"') && !window.contains('\'') {
        return window.find('>');
    }
    // A quote is present *somewhere* in the window, but that doesn't mean
    // one hides the tag's actual closing `>` — check for a `>` at all
    // first (a single fast scan, identical cost to the no-quote fast path
    // above), and only fall into the quote-tracking walk below when a
    // quote appears *before* that naive position. This matters for the
    // "genuinely unterminated tag" case (no `>` anywhere in the window):
    // an earlier version of this function skipped straight to the
    // quote-tracking loop whenever any quote was present, which kept
    // re-scanning the *entire* remaining window for a `>` that was never
    // going to be found, once per quote encountered — quadratic-shaped
    // within the window and measurably ~6x slower than this early-exit,
    // caught by actually timing it rather than trusting complexity-class
    // reasoning alone.
    let naive_gt = window.find('>')?;
    if !window[..naive_gt].contains('"') && !window[..naive_gt].contains('\'') {
        return Some(naive_gt);
    }
    // A quote precedes the naive `>`, so it might be hiding the real one
    // (e.g. `title="Balance > 100"`). Walk forward, jumping over each
    // quoted span via a single fast `find` for its matching close quote,
    // until reaching a `>` with no unclosed quote before it. Already known
    // to terminate (a `>` exists somewhere past `pos` on every iteration,
    // found the same cheap way), so this can't degrade into the same
    // rescan-for-nothing shape the naive-`>` check above heads off.
    let mut pos = 0;
    loop {
        let rest = window.get(pos..)?;
        let gt = rest.find('>')?;
        let quote_before_gt = [rest.find('"'), rest.find('\'')]
            .into_iter()
            .flatten()
            .filter(|&q| q < gt)
            .min();
        let Some(quote_off) = quote_before_gt else {
            return Some(pos + gt);
        };
        let quote_idx = pos + quote_off;
        let quote = window.as_bytes()[quote_idx];
        let after_quote = quote_idx + 1;
        let close_off = window.get(after_quote..)?.find(quote as char)?;
        pos = after_quote + close_off + 1;
    }
}

fn parse_open_tag(s: &str) -> Option<(String, bool, usize)> {
    debug_assert!(s.starts_with('<'));
    let rest = &s[1..];
    let first = rest.chars().next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }

    let window = bounded_prefix(rest, MAX_TAG_SCAN);

    // Find `>` first and bail before doing any allocation if it's not in the
    // window — the common failure case (a malformed/unterminated `<`) then
    // costs only the bounded scan above, never a string allocation.
    let gt = find_tag_end(window)?;

    let name_end = window[..gt]
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(gt);
    let tag = window[..name_end].to_ascii_lowercase();
    let self_closing = window[..gt].trim_end().ends_with('/');
    Some((tag, self_closing, 1 + gt + 1))
}

/// Decode the small set of entities likely to appear in developer-authored
/// (Maud-escaped) HTML: the five predefined XML entities, `&nbsp;` and a
/// handful of common typographic entities, and numeric character references.
/// Anything unrecognized is passed through unchanged (including the leading
/// `&`) rather than dropped, so malformed input never loses data.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices();
    while let Some((i, ch)) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let rest = &raw[i..];
        // Bound the semicolon search to a small *character* window before
        // scanning, not after: searching the unbounded remainder for a `;`
        // and only checking the offset afterward means a long run of `&`
        // with no nearby `;` rescans the whole rest of `raw` for every `&`
        // — O(n^2) on adversarial input (e.g. thousands of bare `&`
        // characters). All supported entity names are ASCII and at most 6
        // characters, so an 11-character window (`&` + up to 10 name chars)
        // is ample headroom while keeping each `&` O(1) to resolve.
        let window_end = rest
            .char_indices()
            .nth(11)
            .map_or(rest.len(), |(off, _)| off);
        let Some(semi) = rest[..window_end].find(';') else {
            out.push('&');
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = decode_one_entity(entity);
        match decoded {
            Some(c) => {
                out.push(c);
                // Advance the outer iterator past the consumed entity body.
                for _ in 0..semi {
                    chars.next();
                }
            }
            None => out.push('&'),
        }
    }
    out
}

fn decode_one_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "copy" => Some('©'),
        _ => {
            let dec = entity.strip_prefix('#')?;
            let value = if let Some(hex) = dec.strip_prefix('x').or_else(|| dec.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                dec.parse::<u32>().ok()?
            };
            char::from_u32(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(children: &[Node]) -> String {
        children
            .iter()
            .map(|n| match n {
                Node::Text(t) => t.clone(),
                Node::Element { children, .. } => text(children),
            })
            .collect()
    }

    #[test]
    fn plain_text_round_trips() {
        let nodes = parse("hello world");
        assert_eq!(nodes, vec![Node::Text("hello world".to_owned())]);
    }

    #[test]
    fn nested_elements_build_a_tree() {
        let nodes = parse("<p>Hello <strong>bold</strong> world</p>");
        assert_eq!(nodes.len(), 1);
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Hello bold world");
        assert!(matches!(&children[1], Node::Element { tag, .. } if tag == "strong"));
    }

    #[test]
    fn an_omitted_li_closing_tag_is_implied_by_the_next_li() {
        // Regression: `<li>` is an HTML5 "optional end tag" element — real
        // or hand-written HTML commonly omits `</li>` before the next
        // `<li>` starts. Without implied-close handling, the second `<li>`
        // nested *inside* the first instead of becoming its sibling.
        let nodes = parse("<ul><li>One<li>Two</ul>");
        assert_eq!(nodes.len(), 1);
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "ul");
        assert_eq!(
            children.len(),
            2,
            "expected two sibling <li>s, got {children:?}"
        );
        for (child, expected_text) in children.iter().zip(["One", "Two"]) {
            let Node::Element { tag, children } = child else {
                panic!("expected an element")
            };
            assert_eq!(tag, "li");
            assert_eq!(text(children), expected_text);
        }
    }

    #[test]
    fn an_omitted_td_closing_tag_is_implied_by_the_next_td_or_tr() {
        // Same bug, table-cell/row variant: `<td>`/`<th>`/`<tr>` are also
        // optional-end-tag elements, and a new `<tr>` must close both an
        // open `<td>` *and* the `<tr>` above it, not just the cell.
        let nodes = parse("<table><tr><td>A<td>B<tr><td>C</table>");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "table");
        assert_eq!(
            children.len(),
            2,
            "expected two sibling <tr>s, got {children:?}"
        );
        let Node::Element {
            tag,
            children: row1,
        } = &children[0]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "tr");
        assert_eq!(row1.len(), 2, "expected two sibling <td>s, got {row1:?}");
        assert_eq!(text(std::slice::from_ref(&row1[0])), "A");
        assert_eq!(text(std::slice::from_ref(&row1[1])), "B");
        let Node::Element {
            tag,
            children: row2,
        } = &children[1]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "tr");
        assert_eq!(row2.len(), 1);
        assert_eq!(text(row2), "C");
    }

    #[test]
    fn an_omitted_p_closing_tag_is_implied_by_a_following_block_element() {
        // Regression: `<p>` is also an HTML5 "optional end tag" element —
        // real/hand-written HTML commonly omits `</p>` before the next
        // block element starts. Without implied-close handling, a
        // following `<table>` nested *inside* the still-open `<p>` instead
        // of becoming its sibling, and since `<p>`'s content goes through
        // `inline_spans` (which has no notion of a table), the table's
        // rows/cells flattened into bare inline text.
        let nodes = parse("<p>Intro<table><tr><td>A</td><td>B</td></tr></table><p>After");
        assert_eq!(
            nodes.len(),
            3,
            "expected <p>, <table>, <p> as three siblings, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Intro");
        assert!(matches!(&nodes[1], Node::Element { tag, .. } if tag == "table"));
        let Node::Element { tag, children } = &nodes[2] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "After");
    }

    #[test]
    fn an_omitted_p_closing_tag_is_implied_through_intervening_inline_formatting() {
        // Regression: `close_implied_tags` used to check only the stack's
        // *top* frame. `<p><strong>Intro<table>...` has `strong` sitting
        // directly above the still-open `<p>` when `<table>` opens, and
        // `strong` has no implied-close rule of its own against `table` —
        // checking only the top stopped right there and never saw the `<p>`
        // beneath it, so the table stayed nested inside the still-open `<p>`
        // (and `<strong>`) instead of becoming a sibling.
        let nodes = parse("<p><strong>Intro<table><tr><td>A</td><td>B</td></tr></table><p>After");
        assert_eq!(
            nodes.len(),
            3,
            "expected <p>, <table>, <p> as three siblings, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(
            children.len(),
            1,
            "expected a single <strong> child, got {children:?}"
        );
        assert!(matches!(&children[0], Node::Element { tag, .. } if tag == "strong"));
        assert_eq!(text(children), "Intro");
        assert!(matches!(&nodes[1], Node::Element { tag, .. } if tag == "table"));
        let Node::Element { tag, children } = &nodes[2] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "After");
    }

    #[test]
    fn an_omitted_head_closing_tag_is_implied_by_body() {
        // Regression: `</head>` is also an HTML5 "optional end tag" —
        // omitting it is common/valid (`<html><head><title>X</title><body>...`).
        // Without implied-close handling, `<body>` nested *inside* the
        // still-open `<head>` — and since `head` is in `is_non_rendered`
        // (`layout.rs`), its entire subtree is discarded, silently dropping
        // the whole visible document.
        let nodes = parse("<html><head><title>X</title><body><p>Visible</p></body></html>");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "html");
        assert_eq!(
            children.len(),
            2,
            "expected <head> and <body> as siblings, got {children:?}"
        );
        assert!(matches!(&children[0], Node::Element { tag, .. } if tag == "head"));
        let Node::Element {
            tag,
            children: body_children,
        } = &children[1]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "body");
        assert_eq!(text(body_children), "Visible");
    }

    #[test]
    fn an_omitted_body_start_tag_also_implies_a_head_close() {
        // Regression: HTML5 permits omitting the `<body>` *start* tag
        // itself, not just `</head>` — `<head><title>X</title><p>...`
        // (or even bare text with no wrapper tag at all) is equally valid.
        // The previous fix only matched an explicit `("head", "body")`
        // transition, so `<p>` opening while `<head>` was still open didn't
        // close it — `<p>` (and its "Visible" text) nested inside `<head>`
        // and, since `head` is in `is_non_rendered` (`layout.rs`), vanished
        // along with the rest of the document.
        let nodes = parse("<html><head><title>X</title><p>Visible</p></html>");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "html");
        assert_eq!(
            children.len(),
            2,
            "expected <head> and <p> as siblings, got {children:?}"
        );
        assert!(matches!(&children[0], Node::Element { tag, .. } if tag == "head"));
        let Node::Element {
            tag,
            children: p_children,
        } = &children[1]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(p_children), "Visible");
    }

    #[test]
    fn non_whitespace_text_also_implies_a_head_close() {
        // Regression: bare text with no wrapper tag at all is just as valid
        // body content as `<p>...` — `<head><title>X</title>Visible` (a
        // still-open `<head>`, no `<body>`/`<p>` element at all) used to
        // leave "Visible" nested inside (and, being non-rendered, hidden
        // by) `<head>` since `implicitly_closes` only fires on a new tag,
        // never on a text node.
        let nodes = parse("<html><head><title>X</title>Visible</html>");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "html");
        assert_eq!(
            children.len(),
            2,
            "expected <head> and the bare text as siblings, got {children:?}"
        );
        assert!(matches!(&children[0], Node::Element { tag, .. } if tag == "head"));
        assert_eq!(children[1], Node::Text("Visible".to_owned()));
    }

    #[test]
    fn whitespace_only_text_does_not_close_an_open_head() {
        // Formatting whitespace (indentation/newlines between tags) between
        // `<head>`'s children must not trigger the same-as-text implied
        // close — only genuine non-whitespace content should.
        let nodes =
            parse("<html><head>\n  <title>X</title>\n</head><body><p>Visible</p></body></html>");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "html");
        let Node::Element {
            tag: head_tag,
            children: head_children,
        } = &children[0]
        else {
            panic!("expected an element")
        };
        assert_eq!(head_tag, "head");
        assert!(
            head_children
                .iter()
                .any(|n| matches!(n, Node::Element { tag, .. } if tag == "title")),
            "the <title> must still be a child of <head>, not hoisted out by whitespace"
        );
    }

    #[test]
    fn script_content_with_a_stray_angle_bracket_does_not_swallow_later_siblings() {
        // Regression: `<script>`/`<style>` content used to be tokenized as
        // ordinary markup, so a `<` that merely *looks* like the start of a
        // tag (e.g. a JS comparison `a<b`) got parsed as a bogus opening
        // tag — which then consumed the *real* `</script>` as part of its
        // own (malformed) closing, leaving `script` unclosed and nesting
        // everything that followed (here, the `<p>`) inside it instead of
        // as a sibling.
        let nodes = parse("<script>if(a<b){}</script><p>Visible</p>");
        assert_eq!(
            nodes.len(),
            2,
            "the <p> must be a sibling of <script>, not swallowed into it"
        );
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn style_content_with_a_stray_angle_bracket_does_not_swallow_later_siblings() {
        let nodes = parse("<style>/* a<b */</style><p>Visible</p>");
        assert_eq!(nodes.len(), 2);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "style"));
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn title_content_with_a_stray_angle_bracket_does_not_swallow_later_siblings() {
        // Regression: `title` wasn't in `is_raw_text_element`'s tag list,
        // so `<title>a<b</title><p>Visible</p>` let the stray `<b` inside
        // the title get parsed as a bogus opening tag that consumed the
        // real `</title>` — same shape of bug as the `<script>`/`<style>`
        // cases above, just for the one other tag `is_non_rendered` in
        // `layout.rs` discards wholesale.
        let nodes = parse("<title>a<b</title><p>Visible</p>");
        assert_eq!(
            nodes.len(),
            2,
            "the <p> must be a sibling of <title>, not swallowed into it"
        );
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "title"));
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn textarea_content_with_a_stray_angle_bracket_is_not_dropped() {
        // Regression: same shape of bug as `title`/`script`/`style` above,
        // but for a tag whose content is real, rendered text (a form
        // default value) rather than something `is_non_rendered` discards —
        // so the failure mode is losing that text rather than hiding
        // unrelated siblings after it.
        let nodes = parse("<textarea>a<b</textarea><p>Visible</p>");
        assert_eq!(
            nodes.len(),
            2,
            "the <p> must be a sibling of <textarea>, not swallowed into it"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first top-level node to be an element")
        };
        assert_eq!(tag, "textarea");
        assert_eq!(
            text(children),
            "a<b",
            "the textarea's literal content must survive, not be dropped"
        );
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn textarea_content_still_decodes_entities() {
        // `textarea`'s content is meant to render, unlike `title`/`script`/
        // `style`'s — so unlike those, it must still decode entities rather
        // than passing them through as literal `&amp;` text.
        let nodes = parse("<textarea>Ben &amp; Jerry</textarea>");
        assert_eq!(nodes.len(), 1);
        let Node::Element { children, .. } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(text(children), "Ben & Jerry");
    }

    #[test]
    fn void_elements_have_no_children_and_need_no_close() {
        let nodes = parse("a<br>b<hr/>c");
        assert_eq!(nodes.len(), 5);
        assert!(
            matches!(&nodes[1], Node::Element { tag, children } if tag == "br" && children.is_empty())
        );
        assert!(
            matches!(&nodes[3], Node::Element { tag, children } if tag == "hr" && children.is_empty())
        );
    }

    #[test]
    fn unclosed_tags_are_auto_closed_at_eof() {
        let nodes = parse("<div><p>oops");
        assert_eq!(nodes.len(), 1);
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected div")
        };
        assert_eq!(tag, "div");
        assert!(matches!(&children[0], Node::Element { tag, .. } if tag == "p"));
    }

    #[test]
    fn stray_closing_tag_is_ignored() {
        let nodes = parse("hello</p>world");
        assert_eq!(text(&nodes), "helloworld");
    }

    #[test]
    fn empty_closing_tag_does_not_panic() {
        // Regression: `</>` has an empty tag name, which used to collide
        // with the implicit root frame's own empty-string sentinel and pop
        // it, panicking on the next `stack.last_mut()`.
        assert_eq!(text(&parse("hello</>world")), "helloworld");
        assert_eq!(text(&parse("<div></></div>")), "");
        assert_eq!(text(&parse("</>")), "");
    }

    #[test]
    fn entities_are_decoded() {
        let nodes = parse("Fish &amp; Chips &mdash; &pound;5 &#65;&#x42;");
        // `&pound;` is not in the supported set, so it (and its `&`) survives
        // literally rather than being dropped.
        assert_eq!(text(&nodes), "Fish & Chips — &pound;5 AB");
    }

    #[test]
    fn long_run_of_unterminated_ampersands_is_linear_not_quadratic() {
        // Regression: the semicolon search used to scan the *entire*
        // remainder of the string per `&` before checking how far away it
        // was, making a long run of unterminated `&` (no nearby `;`) O(n^2).
        // 200k chars comfortably reproduced multi-second blowups before the
        // fix; this should now complete near-instantly.
        let html = "&".repeat(200_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(
            text(&nodes),
            html,
            "unterminated `&` passes through unchanged"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "decode_entities took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_unterminated_open_tags_is_linear_not_quadratic() {
        // Regression: `parse_open_tag`'s search for the closing `>` (and the
        // tag-name search before it) scanned the entire remainder of the
        // string, and on failure returned `None` *without consuming any
        // input* — so the outer loop retried at the very next byte and paid
        // that same unbounded scan again, for every `<` in a long run of
        // unterminated fragments (e.g. `"<a".repeat(n)`, with no `>`
        // anywhere) — O(n^2) overall.
        let html = "<a".repeat(100_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(
            text(&nodes),
            html,
            "unterminated `<a` fragments pass through unchanged as literal text"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse_open_tag took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_unmatched_closing_tags_is_linear_not_quadratic() {
        // Regression: the closing-tag matcher scanned the *entire* open-tag
        // stack (`stack.iter().rposition(...)`) looking for a matching
        // opener, on every `</tag>` encountered. For a deep run of opens
        // followed by a long run of closes that never match anything on the
        // stack (e.g. `"<a>".repeat(n) + "</x>".repeat(n)` — "x" never
        // appears, so no close ever pops the stack), each of the n closes
        // pays the full O(n) scan for O(n^2) overall.
        let html = format!("{}{}", "<a>".repeat(50_000), "</x>".repeat(50_000));
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        // None of the "</x>" closes match anything, so they're all ignored
        // — the tree is just 50,000 nested (empty) <a> elements.
        assert_eq!(nodes.len(), 1);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_never_closing_wrapper_tags_is_linear_not_quadratic() {
        // A non-phrasing-wrapper tag (`span` isn't `strong`/`b`/`em`/`i`)
        // stops `close_implied_tags`'s walk on its very first frame, so a
        // long run of never-closing `<span>`s should cost O(1) per tag
        // regardless of stack depth — this is the cheap case; see
        // `long_run_of_never_closing_phrasing_wrappers_is_linear_not_quadratic`
        // just below for the bounded-but-not-free phrasing-wrapper case.
        let html = "<span>".repeat(100_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1, "expected one deeply nested <span> tree");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_never_closing_phrasing_wrappers_is_linear_not_quadratic() {
        // Regression: `close_implied_tags` now walks *past* a run of open
        // phrasing-wrapper frames (`strong`/`b`/`em`/`i` — see
        // `an_omitted_p_closing_tag_is_implied_through_intervening_inline_formatting`)
        // looking for a frame beneath them that `new_tag`'s opening
        // implicitly closes. Unlike a non-wrapper tag (which stops the walk
        // immediately), a long run of *never-closing* wrappers — e.g. `n`
        // unclosed `<strong>`s, which never themselves match any
        // implied-close rule and so never stop the walk early — would walk
        // the full stack depth on every one of the n tag opens without this
        // bounded the same way `MAX_CLOSE_SCAN` bounds the closing-tag
        // matcher, for the same O(n^2) reason.
        let html = "<strong>".repeat(100_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1, "expected one deeply nested <strong> tree");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn long_run_of_unterminated_raw_text_closes_is_linear_not_quadratic() {
        // Regression: `consume_raw_text`'s search for the closing `>` past
        // a candidate `</script`/`</style` prefix scanned the *entire*
        // remaining suffix on every failed candidate. A `<script>` body
        // containing many `"</script "` fragments with no `>` anywhere
        // (each one looks like it could be the real closing tag, right up
        // until the bound where a `>` should be) pays that full scan once
        // per candidate — O(n^2) overall, the same shape of bug
        // `MAX_TAG_SCAN`/`MAX_CLOSE_SCAN` were fixed for.
        let html = format!("<script>{}", "</script ".repeat(50_000));
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse took {:?} — looks quadratic again",
            start.elapsed()
        );
    }

    #[test]
    fn closing_tag_deeper_than_the_scan_window_is_ignored_not_matched() {
        // Documents the accepted precision loss from bounding the
        // closing-tag scan (MAX_CLOSE_SCAN): a close whose matching opener
        // sits further back than the window no longer auto-closes the many
        // intervening tags — it's treated the same as a close with no
        // matching opener at all (ignored), so trailing content ends up
        // nested inside the still-open tags instead of becoming a sibling
        // at the root. Real templates essentially never nest hundreds of
        // levels deep, let alone rely on this specific deep-ancestor-close
        // pattern, so this only affects pathological input.
        let html = format!("<outer>{}</outer>tail", "<a>".repeat(600));
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            1,
            "the out-of-window close must not split off a sibling at the root"
        );
        assert_eq!(text(&nodes), "tail");
    }

    #[test]
    fn valid_tag_with_long_attribute_list_still_parses() {
        // Regression: a real Tailwind-style `class` attribute easily runs
        // past a couple hundred bytes on its own — MAX_TAG_SCAN must stay
        // generous enough that a genuine (if verbose) opening tag doesn't
        // get rejected and leaked into the PDF as literal `<div ...>` text.
        let long_class = "flex items-center justify-between px-4 py-2 bg-white \
            dark:bg-gray-900 border border-gray-200 rounded-lg shadow-sm \
            hover:shadow-md transition-shadow duration-200 text-sm font-medium \
            text-gray-700 dark:text-gray-300 focus:outline-none focus:ring-2 \
            focus:ring-offset-2 data-controller=\"dropdown\" aria-label=\"menu\"";
        assert!(
            long_class.len() > 256,
            "test fixture must exceed the old, too-tight window"
        );
        let html = format!("<div class=\"{long_class}\">Hello</div>");
        let nodes = parse(&html);
        assert_eq!(
            text(&nodes),
            "Hello",
            "a long but well-formed opening tag must parse, not leak as literal text"
        );
    }

    #[test]
    fn quoted_greater_than_inside_an_attribute_does_not_end_the_tag_early() {
        // Regression: `parse_open_tag`'s search for the closing `>` used to
        // stop at the *first* `>` anywhere in the tag, including one inside
        // a quoted attribute value — `<div title="Balance > 100">Visible</div>`
        // truncated the tag right after "Balance ", leaking ` 100">` into
        // the document as literal text ahead of "Visible".
        let nodes = parse(r#"<div title="Balance > 100">Visible</div>"#);
        assert_eq!(
            nodes.len(),
            1,
            "expected a single <div> node, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(
            text(children),
            "Visible",
            "the quoted attribute value must not leak into the rendered text"
        );
    }

    #[test]
    fn quoted_apostrophe_greater_than_inside_an_attribute_does_not_end_the_tag_early() {
        // Same bug, single-quoted attribute variant.
        let nodes = parse("<div title='Balance > 100'>Visible</div>");
        assert_eq!(nodes.len(), 1);
        let Node::Element { children, .. } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn long_run_of_unterminated_quoted_attributes_is_linear_not_quadratic() {
        // Regression: `find_tag_end`'s quote-tracking slow path is only
        // reached when the scan window contains a quote character at all —
        // verify that path itself doesn't reopen the O(n^2) cost
        // `MAX_TAG_SCAN` exists to bound, the same way the plain
        // `long_run_of_unterminated_open_tags_is_linear_not_quadratic` test
        // verifies the quote-free fast path.
        let html = "<a title=\"x".repeat(50_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert_eq!(text(&nodes), html);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "find_tag_end's quoted-attribute path took {:?} — looks quadratic",
            start.elapsed()
        );
    }

    #[test]
    fn attributes_are_ignored_but_dont_break_parsing() {
        let nodes = parse(r#"<p class="total" data-x='1'>Total</p>"#);
        assert_eq!(text(&nodes), "Total");
    }

    #[test]
    fn deeply_nested_input_does_not_overflow_the_stack() {
        let mut html = String::new();
        for _ in 0..50_000 {
            html.push_str("<div>");
        }
        html.push('x');
        for _ in 0..50_000 {
            html.push_str("</div>");
        }
        // `parse` is iterative, so it must not overflow. Deliberately does
        // *not* walk the resulting tree with a recursive helper like `text`
        // here — that would just move the overflow risk into the test
        // itself. Dropping `nodes` at the end of this test exercises
        // `Node`'s custom iterative `Drop` the same way.
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn malformed_lone_angle_bracket_is_literal_text() {
        let nodes = parse("a < b");
        assert_eq!(text(&nodes), "a < b");
    }
}
