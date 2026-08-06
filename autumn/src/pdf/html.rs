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
                // A new table section (`<thead>`/`<tbody>`/`<tfoot>`)
                // closes an open cell/row/section the same way a new
                // `<tr>` does — `<table><thead><tr><th>H<tbody>...` omits
                // `</th>`, `</tr>`, *and* `</thead>` together, and without
                // this the whole `<tbody>` (and its row/cell) nested
                // *inside* the still-open header cell, so
                // `extract_table_rows` (which only reads a `<tr>`'s
                // *direct* `<td>`/`<th>` children) flattened the body
                // row's text into the header cell instead of emitting it
                // as a separate row.
                | ("tr", "tr" | "thead" | "tbody" | "tfoot")
                | ("td" | "th", "td" | "th" | "tr" | "thead" | "tbody" | "tfoot")
                | ("thead" | "tbody" | "tfoot", "thead" | "tbody" | "tfoot")
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

/// If `s` (starting with `<`) looks like the start of an opening tag for
/// one of the six tags `layout.rs`'s `is_non_rendered` hides wholesale
/// (`script`, `style`, `noscript`, `template`, `head`, `title`), or for
/// `textarea` — case-insensitively, and only when the tag name is
/// immediately followed by a real tag-name boundary (whitespace, `/`, or
/// `>`), not merely a shared prefix (`<scripted>` doesn't count) — returns
/// its canonical lowercase tag name.
///
/// Used only as a fallback once [`parse_open_tag`] has already failed (its
/// bounded search found no `>` within [`MAX_TAG_SCAN`]) — e.g. one of these
/// seven tags carrying an attribute long enough to push its own `>` past
/// that bound. Without this, [`parse`]'s normal "unrecognized tag"
/// fallback treats the lone `<` as literal text and re-parses everything
/// after it one byte at a time — meaning content that's supposed to be
/// hidden (`layout.rs` skips the six `is_non_rendered` tags' subtrees
/// entirely, and `script`/`style`/`title`/`textarea` content additionally
/// isn't even meant to be *tokenized* as markup, see
/// [`is_raw_text_element`]) leaks into the visible document instead — for
/// `<title>` inside a still-open `<head>`, the non-whitespace fallback
/// text also implicitly closes the head (see [`push_text`]), so both the
/// oversized attribute *and* the title text leak in; for `<textarea>`
/// specifically, the fallback also means its real content (meant to
/// render, unlike the other six) gets tokenized as markup instead of
/// staying raw text — a stray `<b>`-looking sequence inside it would
/// wrongly become a real element instead of literal text.
///
/// Mostly mirrors `is_non_rendered`'s tag set (plus `textarea`, which
/// isn't `is_non_rendered` — see below) rather than importing it: this
/// small parser has no dependency on `layout.rs`'s rendering decisions,
/// same as [`is_structural_tag`] mirrors (rather than calls into) the
/// tag sets it's related to elsewhere in this file.
///
/// The caller in [`parse`] gives three of the six `is_non_rendered` tags
/// — `head`, `noscript`, and `template` — different handling than
/// `script`/`style`/`title`: those three are [structural,
/// generally-parsed elements](is_structural_tag) even when normally
/// sized (their content is real markup, only hidden at render time by
/// `is_non_rendered`), so discarding straight through to a literal
/// closing tag the way the true [raw-text elements](is_raw_text_element)
/// do would be wrong two different ways. For `head`, whose closing tag is
/// optional (implicitly closed by later non-head content, see
/// [`implicitly_closes`]), it would swallow the rest of the document —
/// including `<body>` — whenever `</head>` is omitted, which is legal
/// HTML. For `noscript`/`template`, it would stop at the first *nested*
/// same-name closing tag rather than the correctly-matching outer one —
/// e.g. `<template>` containing another `<template>` — leaking whatever
/// sits between the inner and outer closing tags into the visible
/// document instead of keeping it inside the (fully hidden) outer
/// element's subtree. See the call site for how all three instead get a
/// real stack frame and fall through to normal parsing, exactly like a
/// normally-sized instance of the same tag already does.
///
/// `textarea` gets a fourth kind of handling, different again: like
/// `script`/`style`/`title` it's a genuine [raw-text
/// element](is_raw_text_element) (its content must not be tokenized as
/// markup), so it reuses the same [`consume_raw_text`] scan those three
/// do — but unlike them, its content is real, rendered text (a form
/// default value, say), not something `is_non_rendered` discards, so the
/// scanned text is emitted as a real child node instead of thrown away —
/// see the call site's `push_oversized_raw_text_content_tag`. Nested
/// same-tag content still closes at the first matching closing tag rather
/// than the correctly-nested one for all four genuinely raw-text tags
/// (`script`/`style`/`title`/`textarea`), the same simplification
/// [`consume_raw_text`] already accepts for a normally-sized `<script>`/
/// `<style>`/`<textarea>` — acceptable here too since real HTML doesn't
/// nest these tags either (a literal `<textarea>` inside a `<textarea>`
/// is just more raw text up to the first `</textarea>`, not a nested
/// element). `head`/`noscript`/`template` don't get that simplification
/// (see the call site in [`parse`]): their content is real,
/// generally-parsed markup that's only hidden at render time, so nested
/// same-name tags must close in the correct (innermost-first) order the
/// way normal parsing already guarantees.
fn oversized_raw_text_tag_name(s: &str) -> Option<&'static str> {
    debug_assert!(s.starts_with('<'));
    let rest = &s[1..];
    for name in [
        "script", "style", "title", "head", "noscript", "template", "textarea",
    ] {
        if rest.len() < name.len()
            || !rest.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes())
        {
            continue;
        }
        let is_boundary = rest[name.len()..]
            .chars()
            .next()
            .is_none_or(|c| c == '>' || c == '/' || c.is_whitespace());
        if is_boundary {
            return Some(name);
        }
    }
    None
}

/// The correct fallback position when a bounded oversized-tag `>` search
/// (in [`oversized_tag_body_start`], [`handle_closing_tag`], and
/// [`push_oversized_generic_tag`]) fails to find a real `>` *within its
/// scan window* — the absolute position right after that window, **not**
/// `len` (EOF). "Not found within the bound" must never be conflated with
/// "not found at all, so consume everything to the end of the document":
/// the former only means this parser gave up looking this far, not that
/// there's nothing real beyond it. A `<img src="...far more than 256 KiB
/// of base64...">` followed by genuine subsequent content (`Visible`, a
/// closing tag's own real content, ...) has that content sitting right
/// after the oversized attribute, not at EOF — treating "beyond the
/// window" the same as "EOF" silently swallowed it entirely instead of
/// just this one oversized tag's own unparseable attribute soup.
fn oversized_scan_window_end(input: &str, after: usize) -> usize {
    after + bounded_prefix(&input[after..], MAX_OVERSIZED_TAG_SCAN).len()
}

/// Locates the byte position right after an oversized tag's own `>` —
/// `after_name` is the position right after the tag name (e.g. right after
/// `<script`, before any attributes), and this searches the still-unparsed
/// attribute list for the real closing `>` via the same quote-aware
/// [`find_tag_end`] [`parse_open_tag`] itself uses, just bounded to the
/// more generous [`MAX_OVERSIZED_TAG_SCAN`] instead — see that constant's
/// docs for why a second, larger bound still matters here even though
/// `find_tag_end` itself is linear. Falls back to
/// [`oversized_scan_window_end`] (not `len`/EOF) if no real `>` is found
/// within that bound — see that function's docs for why.
///
/// Quote-awareness matters here specifically: a naive scan for `>` (or,
/// worse, skipping straight to a raw-text-style scan for `</name` from
/// `after_name` without finding the real `>` at all) can be fooled by a
/// quoted attribute value that itself contains a `</name`-looking
/// substring appearing *before* the tag's actual closing `>` — e.g.
/// `<script data-x="</script>` followed by more oversized attribute data
/// and then `">Secret</script>` — mistaking that quoted text for the real
/// closing tag and leaking everything after it (the rest of the attribute
/// value, and the element's real content) as visible text.
fn oversized_tag_body_start(input: &str, after_name: usize) -> usize {
    find_tag_end(bounded_prefix(&input[after_name..], MAX_OVERSIZED_TAG_SCAN)).map_or_else(
        || oversized_scan_window_end(input, after_name),
        |i| after_name + i + 1,
    )
}

/// Handles an oversized `<head ...>`/`<noscript ...>`/`<template ...>`
/// opening tag once [`oversized_raw_text_tag_name`] has already recognized
/// it (and confirmed it isn't one of the three genuinely
/// [raw-text](is_raw_text_element) tags) and [`oversized_tag_body_start`]
/// has located where its content begins — see [`oversized_raw_text_tag_name`]'s
/// docs for why these three can't reuse the same discard-to-literal-
/// closing-tag treatment as `script`/`style`/`title`. Pushes a real frame
/// for `name` (exactly as a normally-sized instance of the same tag would
/// get) and returns `body_start` unchanged (accepted as a parameter purely
/// so the caller doesn't have to juggle two different "new pos" values
/// across the `if`). Extracted out of [`parse`] purely to keep that
/// function's line count down — this has no state of its own beyond
/// `stack`.
fn push_oversized_nested_tag(
    stack: &mut Vec<(String, Vec<Node>, usize)>,
    name: &str,
    body_start: usize,
) -> usize {
    let structural_idx = nearest_structural_idx(stack, name);
    stack.push((name.to_owned(), Vec::new(), structural_idx));
    body_start
}

/// Handles an oversized `<textarea ...>` opening tag once
/// [`oversized_raw_text_tag_name`] has already recognized it and
/// [`oversized_tag_body_start`] has located where its content begins — see
/// that function's docs for why `textarea` can't reuse either of the
/// other two treatments: it's a genuine [raw-text element](is_raw_text_element)
/// (its content must not be tokenized as markup, so it needs the same
/// [`consume_raw_text`] scan `script`/`style`/`title` use), but unlike
/// those three its content is real, rendered text rather than something
/// `is_non_rendered` discards, so — mirroring the normal-sized `textarea`
/// handling in [`parse`] — the scanned text is decoded and emitted as a
/// real child node instead of thrown away. Extracted out of [`parse`]
/// purely to keep that function's line count down.
fn push_oversized_raw_text_content_tag(
    stack: &mut [(String, Vec<Node>, usize)],
    input: &str,
    body_start: usize,
    name: &str,
) -> usize {
    let (text, new_pos) = consume_raw_text(input, body_start, name);
    let mut children = Vec::new();
    if !text.is_empty() {
        children.push(Node::Text(decode_entities(text)));
    }
    stack
        .last_mut()
        .expect("root frame is never popped")
        .1
        .push(Node::Element {
            tag: name.to_owned(),
            children,
        });
    new_pos
}

/// Bound on how far [`push_oversized_generic_tag`] looks for its tag
/// name's own end (`>`, `/`, or whitespace) before giving up. Real tag
/// names — including verbose custom-element ones (`<my-custom-widget>`)
/// — are always short, so this only needs to be generous, not large; the
/// point is to *reject* a run of bare `<` characters with no real tag
/// structure at all (e.g. `"<a".repeat(n)`, already covered by
/// [`long_run_of_unterminated_open_tags_is_linear_not_quadratic`]) rather
/// than swallow the *entire remaining document* as one bogus tag name —
/// which, besides being wrong, would also make `close_implied_tags`
/// (called with that name below) and the eventual tag-name allocation
/// cost proportional to document size on every such `<`, reopening the
/// same O(n^2) shape `MAX_TAG_SCAN` exists to prevent for the same
/// pattern in `parse_open_tag`.
const MAX_TAG_NAME_SCAN: usize = 128;

/// Handles an oversized *ordinary* opening tag at `input[pos..]` — one
/// [`oversized_raw_text_tag_name`] doesn't recognize, e.g.
/// `<div data-state="...more than 4 KiB...">` — once
/// [`parse_open_tag`]'s own [`MAX_TAG_SCAN`]-bounded search has already
/// failed to find its `>`. Without this, `parse`'s final fallback (a lone
/// `<` treated as literal text, re-parsed one byte at a time) rendered the
/// entire oversized attribute list as visible text instead of it being
/// invisible the way any other tag's attributes already are.
///
/// Mirrors the normal (non-oversized) tag-push logic in [`parse`]
/// (`close_implied_tags`, then push an empty element for a self-closing
/// or [void](is_void_element) tag, or a real frame otherwise) — just
/// locating the real `>` (and whether it's self-closing) the same way
/// [`oversized_tag_body_start`] does for the seven tags that function
/// covers, bounded to [`MAX_OVERSIZED_TAG_SCAN`] for the same reason —
/// including falling back to [`oversized_scan_window_end`] (not EOF) if
/// no real `>` is found within that bound, so a void/self-closing tag
/// whose attribute list exceeds even that generous bound doesn't swallow
/// the rest of the document as if it were the tag's own content. Returns
/// `None` if `input[pos..]` doesn't even look like the start of a tag
/// name — no leading ASCII-alphabetic character, or no tag-name boundary
/// within [`MAX_TAG_NAME_SCAN`] — so the caller can fall through to its
/// plain literal-text handling for a genuinely bogus `<`.
fn push_oversized_generic_tag(
    stack: &mut Vec<(String, Vec<Node>, usize)>,
    input: &str,
    pos: usize,
) -> Option<usize> {
    let rest = &input[pos + 1..];
    let first = rest.chars().next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let name_end = bounded_prefix(rest, MAX_TAG_NAME_SCAN)
        .find(|c: char| c == '>' || c == '/' || c.is_whitespace())?;
    let tag = rest[..name_end].to_ascii_lowercase();
    let after_name = pos + 1 + name_end;
    let gt = find_tag_end(bounded_prefix(&input[after_name..], MAX_OVERSIZED_TAG_SCAN));
    let self_closing =
        gt.is_some_and(|i| input[after_name..after_name + i].trim_end().ends_with('/'));
    let body_start = gt.map_or_else(
        || oversized_scan_window_end(input, after_name),
        |i| after_name + i + 1,
    );

    close_implied_tags(stack, &tag);
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
        let structural_idx = nearest_structural_idx(stack, &tag);
        stack.push((tag, Vec::new(), structural_idx));
    }
    Some(body_start)
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
        let Some(gt) = raw_close_tag_end(after_tag) else {
            continue;
        };
        let consumed = i + 2 + tag.len() + gt + 1;
        return (&rest[..i], pos + consumed);
    }
    (rest, input.len())
}

/// Locates the `>` that ends a raw-text closing tag candidate, given
/// `after_tag` (the text right after the tag name, e.g. right after
/// `</script`). Two passes, both bounded overall — never reintroducing the
/// O(n^2) risk [`MAX_RAW_CLOSE_SCAN`] exists to prevent:
///
/// 1. A cheap bounded search within [`MAX_RAW_CLOSE_SCAN`] bytes, using the
///    same quote-aware [`find_tag_end`] an opening tag's own `>` search
///    uses — real browsers' end-tag tokenizer *does* enter a quote-aware,
///    attribute-value-like state once whitespace follows the tag name (by
///    which point [`consume_raw_text`]'s `is_boundary` check has already
///    confirmed we're here), so e.g. `</script data-x=">secret">` must
///    skip the quoted `>` and end at the real one after it, not treat the
///    quoted `>` as the close and leak `secret">` as visible text. Covers
///    the overwhelmingly common case, including stray non-whitespace
///    content before `>` as long as it's close by.
/// 2. If that fails, a fallback that walks through a *pure* run of ASCII
///    whitespace immediately following (unbounded in length, but legal per
///    the HTML5 end-tag grammar's "closing tag, arbitrary whitespace, then
///    `>`"), succeeding only if `>` immediately follows it. This can't
///    blow up quadratically even though it's unbounded: every byte it
///    walks is whitespace, which can never itself be a `<` — so
///    [`consume_raw_text`]'s outer `match_indices('<')` loop never
///    revisits any of it while looking for the *next* candidate. Total
///    work across every failed candidate in one call stays O(`rest.len()`),
///    same as the `match_indices` scan it's riding along with. A pure
///    whitespace run never contains a quote, so this pass needs no
///    quote-awareness of its own.
///
/// Without pass 1's quote-awareness, a quoted attribute value in the
/// closing tag containing a literal `>` was mistaken for the real one,
/// leaking the rest of the attribute value and the genuine subsequent
/// content as visible text. Without pass 2, a legitimate closing tag with
/// 64+ bytes of whitespace before `>` (rare, but valid HTML) was rejected
/// outright, making [`consume_raw_text`] scan through to EOF instead —
/// hiding all following visible content for `script`/`style`, or
/// rendering the literal (unclosed-looking) tag text for
/// `title`/`textarea`.
fn raw_close_tag_end(after_tag: &str) -> Option<usize> {
    find_tag_end(bounded_prefix(after_tag, MAX_RAW_CLOSE_SCAN)).or_else(|| {
        let ws_len = after_tag
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(after_tag.len());
        after_tag[ws_len..].starts_with('>').then_some(ws_len)
    })
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

/// Handles a `</tag>` closing sequence at `input[pos..]` (the caller has
/// already confirmed it starts with `"</"`): matches it against the
/// nearest open frame with that name within [`MAX_CLOSE_SCAN`] frames of
/// the stack, closing everything down to and including it. A stray/
/// mismatched close tag with no matching opener nearby is ignored rather
/// than corrupting the tree — this also covers an empty tag name (`</>`),
/// which must never match: the implicit root frame is *also* keyed by an
/// empty string (it has no tag), and popping it would leave `stack`
/// empty. Returns the new `pos`, past the closing tag. Extracted out of
/// [`parse`] purely to keep that function's line count down — this has no
/// state of its own beyond `stack`.
///
/// The tag name is the text up to the first real tag-name boundary
/// (whitespace, `/`, or `>`) — *not* everything up to the first `>`, and
/// the `>` search itself is the same quote-aware [`find_tag_end`] an
/// opening tag's own `>` uses, bounded to [`MAX_TAG_SCAN`] to stay cheap
/// on every single `</`, not just oversized ones. Real browsers *do*
/// enter a quote-aware, attribute-value-like state for a closing tag once
/// whitespace follows the tag name (see [`raw_close_tag_end`]'s docs for
/// the same rule applied to a raw-text closing tag) — without both fixes,
/// a browser-tolerated attribute on an ordinary end tag broke this two
/// ways: `</div data-x="...">` took the whole `div data-x="` blob as the
/// "name" (never matching the real open `div`), and if that attribute
/// value itself contained a literal `>` (e.g. `</div data-x=">secret">`),
/// the naive un-quote-aware search stopped there and leaked
/// `secret">` — plus everything after — as visible text instead of the
/// genuine content following the real closing `>`.
fn handle_closing_tag(
    stack: &mut Vec<(String, Vec<Node>, usize)>,
    input: &str,
    pos: usize,
) -> usize {
    let rest = &input[pos + 2..];
    let name_end = rest
        .find(|c: char| c == '>' || c == '/' || c.is_whitespace())
        .unwrap_or(rest.len());
    let name = rest[..name_end].to_ascii_lowercase();
    // Two tiers, same reasoning as `MAX_OVERSIZED_TAG_SCAN`'s docs: the
    // cheap `MAX_TAG_SCAN`-bounded search handles every realistic closing
    // tag (attributes are already rare there, let alone oversized ones),
    // so it's tried first on every single `</` in the document. Only a
    // closing tag whose own attribute list *itself* exceeds that falls
    // through to the larger (but still bounded, for the same total-cost
    // reason) second attempt. If even *that* fails to find the real `>`,
    // resume right after the scanned window (see
    // `oversized_scan_window_end`) rather than at EOF — otherwise
    // `</div data-x="...far more than 256 KiB...">Visible` would silently
    // drop `Visible` and everything else genuinely following it, not just
    // this one (extraordinarily verbose) closing tag's own attribute soup.
    let tag_end = find_tag_end(bounded_prefix(rest, MAX_TAG_SCAN))
        .or_else(|| find_tag_end(bounded_prefix(rest, MAX_OVERSIZED_TAG_SCAN)));
    let new_pos = tag_end.map_or_else(
        || oversized_scan_window_end(input, pos + 2),
        |i| pos + 2 + i + 1,
    );

    let matching_depth = if name.is_empty() {
        None
    } else {
        let window_start = stack.len().saturating_sub(MAX_CLOSE_SCAN);
        stack[window_start..]
            .iter()
            .rposition(|(tag, _, _)| *tag == name)
            .map(|i| window_start + i)
    };
    if let Some(depth) = matching_depth {
        while stack.len() > depth {
            let (tag, children, _) = stack.pop().expect("depth <= stack.len()");
            stack
                .last_mut()
                .expect("root frame is never popped")
                .1
                .push(Node::Element { tag, children });
        }
    }
    new_pos
}

/// Parse `input` into a forest of top-level [`Node`]s.
pub(super) fn parse(input: &str) -> Vec<Node> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    // Stack of (tag_name, children-so-far, nearest_structural_idx — see
    // that function's docs). The implicit root is index 0 with an empty tag
    // name; it is never popped, and its own nearest_structural_idx is
    // itself (0), same as any other structural frame.
    let mut stack: Vec<(String, Vec<Node>, usize)> = vec![(String::new(), Vec::new(), 0)];

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
            if input[pos..].starts_with("</") {
                pos = handle_closing_tag(&mut stack, input, pos);
                continue;
            }
            if let Some((tag, self_closing, tag_end)) = parse_open_tag(&input[pos..]) {
                pos += tag_end;
                close_implied_tags(&mut stack, &tag);

                if is_raw_text_element(&tag) {
                    // `<script>`/`<style>`/`<title>`/`<textarea>` content is
                    // never tokenized as markup — see `consume_raw_text` —
                    // so a `<` that merely *looks* like the start of a tag
                    // (a JS comparison, a CSS selector combinator, a `<`
                    // typed into a textarea's default value) can't swallow
                    // the real closing tag. Entities are still decoded
                    // (matching the normal text-node path below) — a no-op
                    // for the two tags whose content is discarded anyway,
                    // and required for `title`/`textarea`'s real content.
                    //
                    // `self_closing` is deliberately ignored here (unlike
                    // the generic `self_closing || is_void_element` case
                    // below): these four are non-void HTML elements, and
                    // real browsers ignore a stray XHTML-style `/>` on a
                    // non-void, non-foreign element entirely — `<script
                    // src="x" />alert(1)</script>` still parses exactly
                    // like `<script src="x">`, with `alert(1)` as its raw
                    // content, not as sibling markup. Treating
                    // `self_closing` as authoritative here would resume
                    // *normal* parsing right after the `/>`, letting the
                    // real script/style source (or title/textarea markup)
                    // be tokenized and rendered as visible document
                    // content instead of staying raw/hidden.
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
                    let structural_idx = nearest_structural_idx(&stack, &tag);
                    stack.push((tag, Vec::new(), structural_idx));
                }
                continue;
            }
            // `parse_open_tag` found no `>` within `MAX_TAG_SCAN` — normally
            // that just means an unterminated/malformed tag, handled below
            // by falling back to literal text one byte at a time. But for
            // the seven tags `oversized_raw_text_tag_name` recognizes
            // (their own `>` pushed past the bound by e.g. an oversized
            // attribute), that fallback would leak their content into the
            // visible document — see that function's docs for the full
            // three-way (four, counting `textarea`) split in how each
            // group is handled below. First skip past the oversized tag's
            // *own* attribute list (its content must never be scanned
            // before its real `>` is found — see `oversized_tag_body_start`),
            // then: for `script`/`style`/`title`, discard straight through
            // to the closing tag (or EOF) via the same raw-text scan
            // already used for a normally-parsed instance, without
            // emitting any node for the unparseable opening tag itself;
            // for `textarea`, the same scan but keeping (not discarding)
            // the real content, see `push_oversized_raw_text_content_tag`;
            // for `head`/`noscript`/`template` — whose content is real,
            // generally-parsed markup rather than raw text, see
            // `push_oversized_nested_tag` — push a real frame and fall
            // through to normal parsing instead.
            if let Some(name) = oversized_raw_text_tag_name(&input[pos..]) {
                let after_name = pos + 1 + name.len();
                let body_start = oversized_tag_body_start(input, after_name);
                pos = if name == "textarea" {
                    push_oversized_raw_text_content_tag(&mut stack, input, body_start, name)
                } else if is_raw_text_element(name) {
                    consume_raw_text(input, body_start, name).1
                } else {
                    push_oversized_nested_tag(&mut stack, name, body_start)
                };
                continue;
            }
            // Not one of those seven, but still a well-formed (if
            // oversized) *ordinary* tag — `<div data-state="...4KiB+...">`
            // — gets the same fallback treatment, just pushed as a normal
            // element instead of the seven's special handling. See
            // `push_oversized_generic_tag`'s docs.
            if let Some(new_pos) = push_oversized_generic_tag(&mut stack, input, pos) {
                pos = new_pos;
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
        let (tag, children, _) = stack.pop().expect("stack.len() > 1");
        stack
            .last_mut()
            .expect("root frame is never popped")
            .1
            .push(Node::Element { tag, children });
    }

    stack.pop().expect("root frame always present").1
}

/// Tags [`close_implied_tags`] must never search *past* when deciding
/// whether `new_tag`'s opening should reach down and close something
/// further below on `stack`. Two different reasons a tag ends up in this
/// set:
///
/// - It's itself a possible `open_tag` in [`implicitly_closes`] (`p`,
///   `head`, `li`, `dt`, `dd`, `tr`, `td`, `th`, `thead`, `tbody`,
///   `tfoot`) — the search has to be *able* to stop exactly there for a
///   match to ever fire.
/// - It's a genuine HTML5 scope boundary (`ul`, `ol`, `table`, and `html`/
///   `body` at the root) that must block the search even when it doesn't
///   match anything itself — `<ul><li>Parent<ul><li>Child` must leave the
///   inner `<li>` inside the inner `<ul>` rather than reaching past it to
///   close the outer `<li>` two levels up.
///
/// Everything else — including tags `layout.rs` gives real block-level
/// *rendering* treatment to, like `div`/headings/`section`/`blockquote`
/// (see `is_block_boundary_in_inline_context`) — is transparent here, the
/// same as a plain inline-formatting tag (`span`, `mark`, ...): a
/// still-open `<li>`/`<td>`/`<p>` beneath one of these doesn't stop being
/// reachable just because an ordinary block wrapper sits in between, the
/// same way real HTML5 closes enclosing elements along with whatever
/// they're inside when an ancestor implicitly closes. Rendering-level
/// block-boundary-ness and parsing-level scope-boundary-ness are different
/// questions with different answers for these tags — conflating them was
/// itself a bug (see below).
///
/// Regression: this used to be `closes_open_paragraph(tag) || ...`, reusing
/// that function's tag list as a shortcut on the (incorrect) assumption
/// that "closes an open `<p>`" and "is a scope boundary" were the same
/// set. They're not — `div` (among others in that list) closes an open
/// `<p>` but was never meant to block this search. Reported repro:
/// `<ul><li><div>One<li>Two</ul>` — the ordinary (not oversized) `<div>`
/// wrapper (wrongly treated as structural via the old
/// `closes_open_paragraph` shortcut) blocked `close_implied_tags` from
/// ever reaching the enclosing `<li>`, so the second `<li>` nested inside
/// the first instead of closing it and `extract_list_items` emitted only
/// one marker. An earlier fix already made purely inline-formatting tags
/// (`mark`, `time`, `cite`, ...) transparent here — this extends the same
/// treatment to `div`-shaped block tags that, like those, have no implied-
/// close semantics of their own and aren't a real HTML5 scope boundary.
fn is_structural_tag(tag: &str) -> bool {
    is_valid_in_head(tag)
        || matches!(
            tag,
            "thead"
                | "tbody"
                | "tfoot"
                | "tr"
                | "td"
                | "th"
                | "html"
                | "body"
                | "p"
                | "li"
                | "dt"
                | "dd"
                | "ul"
                | "ol"
                | "table"
        )
}

/// The index (within `stack`, *before* the new frame for `tag` is pushed)
/// that [`close_implied_tags`] should treat as this new frame's search
/// target once it becomes the top of the stack: `stack.len()` (i.e. its own
/// eventual index) if `tag` is a [structural tag](is_structural_tag) —
/// searching should stop here — otherwise whatever the *current* top
/// already resolves to, inherited unchanged since a transparent frame
/// doesn't change what's searchable beneath it.
///
/// Computed once here and cached in the pushed frame's third tuple field
/// for the rest of its life (see [`close_implied_tags`]) rather than
/// re-walked on every call: `is_structural_tag` inverting a short
/// "known-transparent" allowlist into its much longer closed complement
/// made a per-call *search* back through a run of transparent frames
/// (even one bounded via `MAX_CLOSE_SCAN` to stay linear) measurably
/// costlier — a stack of nothing-but-`<a>` tags pays that walk, and the
/// larger tag-classification check inside it, on every single tag open.
/// Caching removes the walk (and its bound) entirely: each frame already
/// knows its own answer the instant it's pushed, so `close_implied_tags`
/// never inspects more than the current top frame plus its cached target.
fn nearest_structural_idx(stack: &[(String, Vec<Node>, usize)], tag: &str) -> usize {
    if is_structural_tag(tag) {
        stack.len()
    } else {
        stack.last().map_or(0, |frame| frame.2)
    }
}

/// Cascades [`implicitly_closes`] up `stack`: repeatedly finds the closest
/// still-open frame that opening `new_tag` implicitly closes and pops down
/// to it, so e.g. a new `<tr>` closes both an open `<td>` *and* the `<tr>`
/// above it (found and closed one loop iteration apart), not just the
/// immediate stack top. Extracted out of [`parse`] purely to keep that
/// function's line count down — this has no state of its own beyond
/// `stack`.
///
/// Looks *past* a run of non-[structural](is_structural_tag) frames at the
/// top, not just at the top itself: `<p><strong>Intro<table>` has `strong`
/// sitting directly above the still-open `p` when `<table>` opens, and
/// `strong` has no implied-close rule of its own against `table` —
/// checking only `stack.last()` would stop right there and never see the
/// `p` beneath, leaving the table nested inside it (and flattened through
/// `inline_spans`, which has no notion of a table, instead of becoming a
/// real `Block::Table`). Only non-structural frames are skipped this way —
/// a genuine block container (`ul`, `table`, ...) sitting at the top always
/// stops the search at that frame, matching or not, so e.g.
/// `<ul><li>Parent<ul><li>Child` correctly leaves the inner `<li>` opening
/// *inside* the inner `<ul>` rather than reaching past it to close the
/// outer `<li>` two levels up.
///
/// This "look past" costs nothing at call time: the top frame's third
/// tuple field is the [`nearest_structural_idx`] computed (and cached) when
/// that frame was pushed, so finding the target is one index lookup rather
/// than a walk back through `stack` — see that function's docs for why a
/// per-call search (even one bounded to stay linear) stopped being cheap
/// enough once [`is_structural_tag`] grew from a short "known-transparent"
/// allowlist's negation into its own much larger closed set.
fn close_implied_tags(stack: &mut Vec<(String, Vec<Node>, usize)>, new_tag: &str) {
    loop {
        if stack.len() <= 1 {
            break;
        }
        let target = stack[stack.len() - 1].2;
        if !implicitly_closes(&stack[target].0, new_tag) {
            break;
        }
        while stack.len() > target {
            let (closed_tag, children, _) = stack.pop().expect("stack.len() > target >= 1");
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

fn push_text(stack: &mut Vec<(String, Vec<Node>, usize)>, text: &str) {
    if text.is_empty() {
        return;
    }
    // Non-whitespace text is just as invalid inside `<head>` as an
    // unexpected tag — same implied close as `implicitly_closes`'s `head`
    // case, just triggered by content instead of a new element (e.g.
    // `<head><title>X</title>Visible</head>`, or the same with no closing
    // tag at all). Whitespace-only text (formatting indentation between
    // tags) doesn't count — it's not real content.
    if stack.last().is_some_and(|(tag, _, _)| tag == "head")
        && text.contains(|c: char| !c.is_whitespace())
    {
        let (closed_tag, children, _) = stack.pop().expect("just checked stack.last() above");
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

/// Bound on the quote-aware `>` search used once a tag is already known to
/// be oversized (its own `>` didn't fit within [`MAX_TAG_SCAN`]) —
/// [`oversized_tag_body_start`], [`handle_closing_tag`]'s fallback tier,
/// and [`push_oversized_generic_tag`] all use it. [`find_tag_end`] itself
/// is linear in the size of the window it's given (an earlier version
/// wasn't — see its own docs), so this bound isn't there to protect a
/// single call; it protects against *many* calls each getting handed a
/// huge window. Every caller here is reached only after its own
/// `MAX_TAG_SCAN`-bounded attempt already failed, so triggering many of
/// them still costs the attacker at least `MAX_TAG_SCAN` bytes each —
/// without this second bound, though, each of those (already-paid-for)
/// triggers could scan up to the *entire remaining document*, so a
/// document densely packed with the minimum oversized-tag payload could
/// still cost O(triggers × remaining document length) overall. Bounding
/// the fallback window too keeps each trigger's *own* worst case
/// constant, so total cost stays linear in document size. Sized well
/// beyond `MAX_TAG_SCAN` (256 KiB) so it only matters for the genuinely
/// rare case of an oversized tag's attribute list *itself* exceeding 4
/// KiB — any realistic large attribute value (a base64 thumbnail, a JSON
/// blob) still parses correctly; only a deliberately pathological payload
/// falls back further to this parser's usual auto-close-at-EOF tolerance.
const MAX_OVERSIZED_TAG_SCAN: usize = 256 * 1024;

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
    // until reaching a `>` with no unclosed quote before it.
    //
    // Regression: an earlier version of this loop re-derived `gt` via a
    // fresh `rest.find('>')` on *every* iteration, scanning from the
    // (advancing) `pos` all the way back out to the same absolute `>`
    // position each time — for a long chain of *short* quote pairs all
    // closing well before the eventual real `>` (e.g.
    // `data-x="a""a""a"...">`), that `>` never moves, so every one of
    // those re-scans pays the same long distance again: O(pairs *
    // distance-to-`>`), the same O(n^2) shape `MAX_TAG_SCAN`/
    // `MAX_CLOSE_SCAN` were fixed for elsewhere in this module (confirmed
    // by timing it, not just complexity-class reasoning: doubling the
    // input roughly quadrupled the time). `gt` only genuinely needs
    // re-deriving when the quote just skipped turns out to have closed
    // *past* it — meaning that old `gt` was actually inside the quote and
    // was never real to begin with; otherwise it's unchanged from the
    // last time it was found, so reusing it is correct, not just faster.
    let mut pos = 0;
    let mut gt = window.find('>')?;
    loop {
        if gt < pos {
            gt = pos + window.get(pos..)?.find('>')?;
        }
        let rest = window.get(pos..gt)?;
        // A single forward byte-scan for *either* quote character, not
        // `rest.find('"')` and `rest.find('\'')` as two separate calls: if
        // only one quote style is ever used (overwhelmingly the common
        // case), the *other* character never appears anywhere in `rest`,
        // and confirming that absence costs `find` a full scan of `rest`
        // every time — with `rest` shrinking only a few bytes per
        // iteration for a long chain of short quote pairs, that's another
        // O(pairs * distance-to-`>`) cost, the same shape this loop's
        // `gt`-reuse fix above already eliminated for the `>` search.
        // `bytes()`/`position()` (not `char_indices()`) since both quote
        // characters are single ASCII bytes — no UTF-8 decoding needed to
        // compare against them.
        let quote_before_gt = rest.bytes().position(|b| b == b'"' || b == b'\'');
        let Some(quote_off) = quote_before_gt else {
            return Some(gt);
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
    fn an_omitted_li_closing_tag_is_implied_through_a_transparent_inline_wrapper() {
        // Regression: `close_implied_tags`'s look-through only recognized
        // `strong`/`b`/`em`/`i` as skippable phrasing wrappers — `span`/`a`
        // are just as transparent to `inline_spans` (its generic
        // passthrough case treats them identically), but weren't in the
        // skip set, so `<ul><li><span>One<li>Two</ul>` left the second
        // `<li>` nested under the first (stopping the search at `span`)
        // instead of closing it.
        let nodes = parse("<ul><li><span>One<li>Two</ul>");
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
        let Node::Element {
            tag,
            children: first_li,
        } = &children[0]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "li");
        assert_eq!(first_li.len(), 1, "expected one <span> child");
        assert!(matches!(&first_li[0], Node::Element { tag, .. } if tag == "span"));
        assert_eq!(text(first_li), "One");
        let Node::Element {
            tag,
            children: second_li,
        } = &children[1]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "li");
        assert_eq!(text(second_li), "Two");
    }

    #[test]
    fn an_omitted_li_closing_tag_is_implied_through_the_remaining_transparent_wrappers() {
        // Regression: after the `span`/`a` fix, other standard phrasing
        // wrappers `inline_spans` also treats transparently (`small`,
        // `code`, `abbr`, `label`, ...) still stopped the implied-close
        // search — same bug, different tag.
        for wrapper in ["small", "code", "abbr", "label"] {
            let html = format!("<ul><li><{wrapper}>One<li>Two</ul>");
            let nodes = parse(&html);
            assert_eq!(nodes.len(), 1, "wrapper {wrapper:?}");
            let Node::Element { tag, children } = &nodes[0] else {
                panic!("expected an element ({wrapper:?})")
            };
            assert_eq!(tag, "ul");
            assert_eq!(
                children.len(),
                2,
                "expected two sibling <li>s for wrapper {wrapper:?}, got {children:?}"
            );
            let Node::Element { tag, .. } = &children[1] else {
                panic!("expected an element ({wrapper:?})")
            };
            assert_eq!(tag, "li", "wrapper {wrapper:?}");
        }
    }

    #[test]
    fn an_omitted_li_closing_tag_is_implied_through_any_unrecognized_wrapper_tag() {
        // Regression: `is_phrasing_wrapper` used to be its own allowlist of
        // "known transparent tags", one step behind `inline_spans`'s
        // actually-exhaustive "anything unrecognized is transparent" rule —
        // `mark`/`time`/`cite` (and any other tag `layout.rs` doesn't
        // specially recognize) are just as transparent as `span`, but
        // weren't in the allowlist, so `<ul><li><mark>One<li>Two</ul>` left
        // the second `<li>` nested under the first instead of closing it.
        // Now that the check is inverted against the closed set of tags the
        // renderer *does* special-case, an arbitrary never-listed tag
        // (`made-up-tag`) is covered too, not just these three.
        for wrapper in ["mark", "time", "cite", "made-up-tag"] {
            let html = format!("<ul><li><{wrapper}>One<li>Two</ul>");
            let nodes = parse(&html);
            assert_eq!(nodes.len(), 1, "wrapper {wrapper:?}");
            let Node::Element { tag, children } = &nodes[0] else {
                panic!("expected an element ({wrapper:?})")
            };
            assert_eq!(tag, "ul");
            assert_eq!(
                children.len(),
                2,
                "expected two sibling <li>s for wrapper {wrapper:?}, got {children:?}"
            );
            let Node::Element { tag, .. } = &children[1] else {
                panic!("expected an element ({wrapper:?})")
            };
            assert_eq!(tag, "li", "wrapper {wrapper:?}");
        }
    }

    #[test]
    fn an_omitted_li_closing_tag_is_implied_through_an_ordinary_block_wrapper() {
        // Regression: `is_structural_tag` used to reuse `closes_open_paragraph`'s
        // tag list wholesale, wrongly treating `div` (and other ordinary
        // block tags with no implied-close semantics of their own, like
        // headings/`section`/`blockquote`) as a scope *barrier* the same
        // way genuine ones (`ul`/`table`) are — so `<ul><li><div>One<li>Two</ul>`
        // left the second `<li>` nested inside the `<div>` inside the
        // first `<li>` instead of closing it, and `extract_list_items`
        // emitted only one marker. `div` gets real block-level rendering
        // treatment from `layout.rs` (see `is_block_boundary_in_inline_context`),
        // but that's an orthogonal concern from parsing-level scope —
        // a still-open `<li>` beneath it must stay reachable, the same as
        // through a purely inline wrapper like `<mark>`.
        for wrapper in ["div", "h2", "section", "blockquote", "header"] {
            let html = format!("<ul><li><{wrapper}>One<li>Two</ul>");
            let nodes = parse(&html);
            assert_eq!(nodes.len(), 1, "wrapper {wrapper:?}");
            let Node::Element { tag, children } = &nodes[0] else {
                panic!("expected an element ({wrapper:?})")
            };
            assert_eq!(tag, "ul");
            assert_eq!(
                children.len(),
                2,
                "expected two sibling <li>s for wrapper {wrapper:?}, got {children:?}"
            );
            let Node::Element {
                tag,
                children: first_li,
            } = &children[0]
            else {
                panic!("expected an element ({wrapper:?})")
            };
            assert_eq!(tag, "li", "wrapper {wrapper:?}");
            assert_eq!(
                first_li.len(),
                1,
                "expected one wrapper child ({wrapper:?})"
            );
            assert!(
                matches!(&first_li[0], Node::Element { tag, .. } if tag == wrapper),
                "wrapper {wrapper:?}: expected the first <li> to still contain its wrapper, \
                 got {first_li:?}"
            );
            assert_eq!(text(first_li), "One", "wrapper {wrapper:?}");
            let Node::Element { tag, children } = &children[1] else {
                panic!("expected an element ({wrapper:?})")
            };
            assert_eq!(tag, "li", "wrapper {wrapper:?}");
            assert_eq!(text(children), "Two", "wrapper {wrapper:?}");
        }
    }

    #[test]
    fn nested_list_scope_barrier_survives_the_ordinary_block_wrapper_fix() {
        // Guard against overcorrecting the fix above: `ul`/`ol`/`table`
        // must remain genuine scope barriers — a `<div>` no longer
        // blocking `close_implied_tags`' search must not somehow let it
        // reach *past* a nested `<ul>` too. `<ul><li>Parent<div><ul><li>Child</ul></div></li></ul>`
        // must still leave "Child" as its own nested list, not merge it
        // into the outer list's second item.
        let nodes = parse("<ul><li>Parent<div><ul><li>Child</ul></div><li>Sibling</ul>");
        assert_eq!(nodes.len(), 1);
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "ul");
        assert_eq!(
            children.len(),
            2,
            "expected two sibling top-level <li>s, got {children:?}"
        );
        let Node::Element {
            tag,
            children: first_li,
        } = &children[0]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "li");
        assert_eq!(first_li.len(), 2, "expected text then <div>: {first_li:?}");
        assert!(matches!(&first_li[0], Node::Text(t) if t == "Parent"));
        let Node::Element {
            tag,
            children: div_children,
        } = &first_li[1]
        else {
            panic!("expected the <div>")
        };
        assert_eq!(tag, "div");
        assert_eq!(div_children.len(), 1, "expected the nested <ul>");
        let Node::Element {
            tag,
            children: inner_ul,
        } = &div_children[0]
        else {
            panic!("expected the nested <ul>")
        };
        assert_eq!(tag, "ul");
        assert_eq!(inner_ul.len(), 1, "expected the inner <li>");
        assert_eq!(text(inner_ul), "Child");
        let Node::Element {
            tag,
            children: second_li,
        } = &children[1]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "li");
        assert_eq!(text(second_li), "Sibling");
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
    fn omitted_th_tr_and_thead_closing_tags_are_implied_by_a_following_tbody() {
        // Regression: valid HTML can omit `</th>`, `</tr>`, *and* `</thead>`
        // together before a following `<tbody>` — none of `td`/`th`'s,
        // `tr`'s, or `thead`'s implied-close rules covered a new table
        // section starting, only a new cell/row within the *same* section,
        // so the whole `<tbody>` (and its row/cell) nested inside the
        // still-open header cell instead of becoming `<thead>`'s sibling.
        let nodes = parse("<table><thead><tr><th>H<tbody><tr><td>A</table>");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "table");
        assert_eq!(
            children.len(),
            2,
            "expected <thead> and <tbody> as siblings, got {children:?}"
        );
        let Node::Element {
            tag,
            children: thead_children,
        } = &children[0]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "thead");
        assert_eq!(thead_children.len(), 1, "expected one <tr>");
        assert_eq!(text(thead_children), "H");
        let Node::Element {
            tag,
            children: tbody_children,
        } = &children[1]
        else {
            panic!("expected an element")
        };
        assert_eq!(tag, "tbody");
        assert_eq!(tbody_children.len(), 1, "expected one <tr>");
        assert_eq!(
            text(tbody_children),
            "A",
            "the body row must be a sibling row, not text flattened into the header cell"
        );
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
    fn oversized_script_tag_does_not_leak_its_source_as_visible_text() {
        // Regression: `parse_open_tag`'s search for a tag's own `>` is
        // bounded by `MAX_TAG_SCAN` (4 KiB) — a `<script>` carrying an
        // attribute longer than that pushes its own `>` past the bound, so
        // `parse_open_tag` failed and the parser fell back to treating the
        // lone `<` as literal text, re-tokenizing everything after it
        // (the oversized attribute value *and* the real JS source) as
        // ordinary markup/text instead of staying non-rendered.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(
            r#"<script data-x="{oversized_attr}">var secret = "should never render";</script><p>Visible</p>"#
        );
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            1,
            "the oversized <script> must not leak any node into the tree, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "p");
        let rendered = text(children);
        assert_eq!(rendered, "Visible");
        assert!(
            !rendered.contains("secret") && !rendered.contains('Q'),
            "the script's source and its oversized attribute value must never appear as \
             visible text, got {rendered:?}"
        );
    }

    #[test]
    fn oversized_tag_with_a_closing_tag_look_alike_in_its_own_attribute_is_not_fooled() {
        // Regression: the oversized-tag fallback's raw-text scan started
        // right after the tag *name* — still inside the still-unparsed,
        // still-open attribute list — rather than after the tag's own
        // real `>`. A quoted attribute value containing a `</script`-
        // looking substring *before* that real `>` (e.g.
        // `<script data-x="</script>` followed by oversized attribute
        // data and then `">Secret</script>`) got mistaken for the actual
        // closing tag, after which the rest of the attribute value and
        // "Secret" were reparsed as ordinary visible markup/text.
        let oversized_attr = format!("</script>{}", "Q".repeat(5000));
        let html = format!(r#"<script data-x="{oversized_attr}">Secret</script><p>Visible</p>"#);
        let nodes = parse(&html);
        let rendered = text(&nodes);
        assert!(
            !rendered.contains("Secret") && !rendered.contains('Q'),
            "the script's source and its oversized attribute value (including the embedded \
             </script>-looking substring) must never appear as visible text, got {rendered:?}"
        );
        assert!(
            rendered.contains("Visible"),
            "the following sibling <p> must still render normally, got {rendered:?}"
        );
    }

    #[test]
    fn oversized_title_tag_does_not_leak_into_the_visible_document() {
        // Regression: `oversized_raw_text_tag_name` only recognized
        // `<script`/`<style`, not `<title` — so a `<title>` carrying an
        // oversized attribute fell through to the plain literal-text
        // fallback the same way an oversized `<script>` used to. Worse
        // than script/style: inside a still-open `<head>`, that fallback's
        // non-whitespace text also implicitly closes the head (see
        // `push_text`), so both the attribute value *and* the title text
        // leaked into the visible document despite `title` being
        // `is_non_rendered`.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(
            r#"<head><title data-x="{oversized_attr}">Secret</title></head><p>Visible</p>"#
        );
        let nodes = parse(&html);
        let rendered = text(&nodes);
        assert!(
            !rendered.contains("Secret") && !rendered.contains('Q'),
            "the title's text and its oversized attribute value must never appear as visible \
             text, got {rendered:?}"
        );
        assert!(
            rendered.contains("Visible"),
            "the following sibling <p> must still render normally, got {rendered:?}"
        );
    }

    #[test]
    fn oversized_noscript_and_template_tags_do_not_leak_into_the_visible_document() {
        // Regression: `oversized_raw_text_tag_name` recognized `<script`/
        // `<style`/`<title` but missed `<noscript`/`<template` (and
        // `<head`, covered separately below since it needs different
        // handling) — all six are `is_non_rendered` in `layout.rs`, but
        // only the first three got the oversized-tag suppression fix. A
        // repro matching the reported one: an oversized `<template>`
        // leaked both its attribute value and "Secret" into the visible
        // document. Both get a real stack frame and normal parsing (see
        // `push_oversized_nested_tag`), so what matters at this (parser)
        // layer is that "Secret" and the oversized attribute value stay
        // nested *inside* the element's own subtree — never escaping as a
        // top-level sibling — since `layout.rs`'s `is_non_rendered` (tested
        // separately) is what actually hides a whole subtree at render
        // time; `text()` here has no such filter, so it isn't the right
        // tool to assert final visibility for a tag with real children.
        for tag in ["noscript", "template"] {
            let oversized_attr = "Q".repeat(5000);
            let html = format!(r#"<{tag} data-x="{oversized_attr}">Secret</{tag}><p>Visible</p>"#);
            let nodes = parse(&html);
            assert_eq!(
                nodes.len(),
                2,
                "tag {tag:?}: expected exactly the oversized element and its sibling <p> at the \
                 top level (nothing escaped as an extra sibling), got {nodes:?}"
            );
            let Node::Element { tag: first_tag, .. } = &nodes[0] else {
                panic!("tag {tag:?}: expected the first top-level node to be an element")
            };
            assert_eq!(first_tag, tag);
            let Node::Element {
                tag: second_tag,
                children,
            } = &nodes[1]
            else {
                panic!("tag {tag:?}: expected the second top-level node to be an element")
            };
            assert_eq!(second_tag, "p");
            assert_eq!(
                text(children),
                "Visible",
                "tag {tag:?}: the following sibling <p> must still render normally"
            );
        }
    }

    #[test]
    fn oversized_head_tag_does_not_swallow_the_rest_of_the_document_at_an_implicit_close() {
        // Regression: an earlier fix added `head` to the same
        // raw-text-scan-to-literal-closing-tag suppression as the other
        // five non-rendered tags — but `<head>`'s closing tag is legally
        // optional (HTML5, and this parser's own `implicitly_closes`,
        // close it as soon as non-head content begins), so a document
        // that omits `</head>` entirely — an oversized `<head data-x="...">`
        // directly followed by `<body>Visible</body>`, with no `</head>`
        // anywhere — used to have that raw-text scan search for a literal
        // `</head>` all the way to EOF, discarding the *entire rest of the
        // document* including the visible body.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(r#"<head data-x="{oversized_attr}"><body>Visible</body>"#);
        let nodes = parse(&html);
        let rendered = text(&nodes);
        assert!(
            !rendered.contains('Q'),
            "the oversized attribute value must never appear as visible text, got {rendered:?}"
        );
        assert!(
            rendered.contains("Visible"),
            "the <body> content must still render even though </head> was never written, got \
             {rendered:?}"
        );
    }

    #[test]
    fn oversized_template_nested_inside_itself_hides_everything_up_to_the_outer_close() {
        // Regression: the oversized-tag fallback used to route `<template>`
        // through the same raw-text scan as `script`/`style`/`title`
        // (`consume_raw_text`), which stops at the *first* literal closing
        // tag and — for these three genuinely raw-text tags — never
        // pushes a node for the (unparseable) opening tag at all. For a
        // nested `<template><template>inner</template>leak</template>`
        // body, that first closing tag is the *inner* one, so the old code
        // discarded everything up through it, emitted no `template`
        // element whatsoever, and resumed *normal top-level* parsing
        // right on "leak" — turning it into a bare top-level text node
        // (rendered) instead of staying nested inside the outer
        // template's subtree (hidden by `layout.rs`'s `is_non_rendered`)
        // the way real HTML5 nesting requires.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(
            r#"<template data-x="{oversized_attr}"><template>inner</template>leak</template><p>Visible</p>"#
        );
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected exactly the outer template element and its sibling <p> at the top level \
             (nothing escaped as an extra sibling), got {nodes:?}"
        );
        let Node::Element { tag: first_tag, .. } = &nodes[0] else {
            panic!(
                "expected the first top-level node to be the outer template element, not a \
                 leaked text node, got {:?}",
                nodes[0]
            )
        };
        assert_eq!(first_tag, "template");
        let Node::Element {
            tag: second_tag,
            children,
        } = &nodes[1]
        else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(second_tag, "p");
        assert_eq!(
            text(children),
            "Visible",
            "the following sibling <p> must still render normally"
        );
    }

    #[test]
    fn oversized_textarea_tag_still_renders_its_content_as_raw_text() {
        // Regression: `textarea` was entirely missing from
        // `oversized_raw_text_tag_name`'s recognized set — an oversized
        // `<textarea data-x="...4KiB...">` (its own `>` pushed past
        // `MAX_TAG_SCAN`) fell all the way through to `parse`'s generic
        // "unrecognized tag" fallback, which (a) rendered the opening
        // tag's oversized attribute soup as literal visible text and (b)
        // tokenized the textarea's real content as ordinary markup
        // instead of raw text — a `<b>`-looking sequence inside it would
        // wrongly become a real `<b>` element instead of staying literal,
        // unlike a normal-sized `<textarea>`.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(r#"<textarea data-x="{oversized_attr}">a<b</textarea><p>Visible</p>"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the textarea element and its sibling <p>, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first top-level node to be an element")
        };
        assert_eq!(tag, "textarea");
        assert_eq!(
            text(children),
            "a<b",
            "the oversized attribute must not leak, and the real content must survive as \
             literal raw text (not tokenized as markup), got {children:?}"
        );
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn oversized_ordinary_tag_is_skipped_not_rendered_as_literal_text() {
        // Regression: `oversized_raw_text_tag_name` only recognizes seven
        // specific tags — an oversized *ordinary* tag like
        // `<div data-state="...4KiB+...">` (not one of the seven) fell
        // all the way through to `parse`'s final fallback, which treats a
        // lone `<` as literal text and re-parses everything after it one
        // byte at a time — rendering the entire oversized attribute list
        // as visible text, unlike every other tag's (invisible)
        // attributes.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(r#"<div data-state="{oversized_attr}">Visible</div>"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            1,
            "expected a single <div>, with the oversized attribute nowhere in sight, got \
             {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(
            text(children),
            "Visible",
            "the oversized attribute value must not leak into the div's own content"
        );
    }

    #[test]
    fn oversized_ordinary_tag_self_closing_and_void_variants_still_work() {
        // Same bug as above, covering the two branches
        // `push_oversized_generic_tag` has to replicate from the normal
        // (non-oversized) tag-push path: an oversized self-closing tag
        // (XHTML-style `/>`) and an oversized void element must not push
        // a real stack frame — either would otherwise swallow later
        // sibling content as their own children instead of leaving it as
        // a sibling.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(r#"<my-widget data-x="{oversized_attr}" />Visible"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the self-closing tag and its sibling text, got {nodes:?}"
        );
        assert!(matches!(
            &nodes[0],
            Node::Element { tag, children } if tag == "my-widget" && children.is_empty()
        ));
        assert_eq!(nodes[1], Node::Text("Visible".to_owned()));

        let html = format!(r#"<img data-x="{oversized_attr}">Visible"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the void <img> and its sibling text, got {nodes:?}"
        );
        assert!(matches!(
            &nodes[0],
            Node::Element { tag, children } if tag == "img" && children.is_empty()
        ));
        assert_eq!(nodes[1], Node::Text("Visible".to_owned()));
    }

    #[test]
    fn oversized_ordinary_tag_with_a_quoted_bracket_look_alike_is_not_fooled() {
        // Same quote-awareness concern as the oversized six/seven special
        // tags: a quoted attribute value containing a literal `>` must
        // not be mistaken for the tag's own real closing `>`.
        let oversized_attr = format!("{}>", "Q".repeat(5000));
        let html = format!(r#"<div data-x="{oversized_attr}">Visible</div>"#);
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1, "got {nodes:?}");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn oversized_ordinary_closing_tag_beyond_the_fast_bound_still_finds_its_real_close() {
        // Regression: `handle_closing_tag`'s search for its own `>` was
        // bounded to `MAX_TAG_SCAN` (4 KiB) with no fallback — a closing
        // tag whose attribute list *itself* exceeded that (already an
        // unusual case, but a browser-tolerated one) fell straight to the
        // `rest.len()` EOF fallback, silently dropping the real
        // subsequent content ("Visible" here) instead of just this one
        // closing tag's own oversized attribute soup.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(r#"<div>One</div data-x="{oversized_attr}">Visible"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the closed <div> and the genuine trailing text, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(text(children), "One");
        assert_eq!(nodes[1], Node::Text("Visible".to_owned()));
    }

    #[test]
    fn oversized_ordinary_tag_beyond_even_the_generous_bound_does_not_drop_the_rest_of_the_document()
     {
        // Regression: when even `MAX_OVERSIZED_TAG_SCAN` (256 KiB) isn't
        // enough to find the tag's real `>` — e.g. a base64 data URI
        // attribute genuinely larger than that — `push_oversized_generic_tag`
        // used to fall back straight to EOF, silently dropping all
        // subsequent content ("Visible" here) as if it never existed. It
        // now resumes parsing right after the scanned window instead (see
        // `oversized_scan_window_end`) — the reported repro:
        // `<img src="...over 256 KiB of base64...">Visible`.
        let oversized_attr = "Q".repeat(300 * 1024);
        let html = format!(r#"<img src="{oversized_attr}">Visible"#);
        let nodes = parse(&html);
        assert!(
            text(&nodes).contains("Visible"),
            "content genuinely following an extremely oversized tag must not be silently \
             dropped as if the rest of the document didn't exist"
        );
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
    fn ordinary_closing_tag_with_a_browser_tolerated_attribute_still_matches_its_opener() {
        // Regression: `handle_closing_tag` took *everything* before the
        // first `>` as the tag name (only trimmed, never split at
        // whitespace) — so `</div data-x="...">` took the whole
        // `"div data-x=\""`-shaped blob as the "name", which never matches
        // the real open `div`, leaving it (and everything after) nested
        // inside the still-open element instead of closing it. Worse, if
        // that stray attribute value itself contained a literal `>` (real
        // browsers *do* enter a quote-aware state here, same as for a
        // raw-text closing tag), the naive un-quote-aware `>` search
        // stopped at the quoted one and leaked the rest of the attribute
        // value plus genuine subsequent content as visible text — the
        // reported repro: `<div>One</div data-x=">secret">Two`.
        let nodes = parse(r#"<div>One</div data-x=">secret">Two"#);
        assert_eq!(
            nodes.len(),
            2,
            "expected the closed <div> and the genuine trailing text as separate top-level \
             nodes, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(
            text(children),
            "One",
            "the div's own content must not include anything from its closing tag's \
             attribute soup"
        );
        assert_eq!(
            nodes[1],
            Node::Text("Two".to_owned()),
            "only the genuine trailing text may appear as a sibling — the quoted attribute \
             value must not leak, got {nodes:?}"
        );
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
    fn raw_text_closing_tag_skips_a_quoted_bracket_look_alike() {
        // Regression: `raw_close_tag_end`'s bounded search used a plain
        // `.find('>')`, so a closing tag with a quoted attribute-like
        // value containing a literal `>` (real browsers *do* enter a
        // quote-aware, attribute-value-like state for a closing tag once
        // whitespace follows the tag name) was mistaken for the tag's own
        // end — `<script>hidden</script data-x=">secret">Visible` matched
        // the quoted `>` right after `data-x="` as the close, resuming
        // normal parsing at `secret">Visible` and leaking `secret">` as
        // visible text instead of staying inside the (still hidden)
        // script element up through the real closing `>`.
        let html = r#"<script>hidden</script data-x=">secret">Visible"#;
        let nodes = parse(html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the script element and the trailing text, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "script");
        assert_eq!(
            text(children),
            "hidden",
            "the script's own content must stay hidden inside it"
        );
        assert_eq!(
            nodes[1],
            Node::Text("Visible".to_owned()),
            "only the genuine trailing text must appear as a sibling — the quoted attribute \
             value must not leak, got {nodes:?}"
        );
    }

    #[test]
    fn raw_text_closing_tag_accepts_arbitrary_whitespace_before_the_bracket() {
        // Regression: `MAX_RAW_CLOSE_SCAN` (64 bytes) bounded the search
        // for `>` after a candidate `</script`/`</style` prefix — but
        // HTML5's end-tag grammar allows *arbitrary* whitespace between
        // the tag name and `>`. A real (if unusually padded) closing tag
        // with 64+ whitespace bytes before `>` used to fall outside that
        // bound entirely, so `consume_raw_text` never recognized it and
        // scanned through to EOF instead — hiding everything after it.
        let padding = " ".repeat(200);
        let html = format!("<script>alert(1)</script{padding}><p>Visible</p>");
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the script element and its sibling <p>, got {nodes:?}"
        );
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn long_run_of_raw_text_closes_padded_with_whitespace_is_linear_not_quadratic() {
        // Regression guard for `raw_close_tag_end`'s arbitrary-whitespace
        // fallback: unlike the bounded search it falls back from, its
        // whitespace-skip is not itself length-bounded — verify a
        // candidate-heavy adversarial input (many `</script` fragments,
        // each followed by a long run of whitespace but never a `>`)
        // still parses in linear, not quadratic, time.
        let candidate = format!("</script{}", " ".repeat(200));
        let html = format!("<script>{}", candidate.repeat(2_000));
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
    fn self_closing_syntax_on_a_raw_text_element_is_ignored_like_a_real_browser() {
        // Regression: `<script>`/`<style>`/`<title>`/`<textarea>` are
        // non-void HTML elements — real browsers ignore a stray
        // XHTML-style `/>` on them entirely, still treating everything up
        // to the next real closing tag as raw content. This parser used
        // to gate the raw-text branch on `!self_closing`, so
        // `<script src="x" />alert(1)</script>` skipped straight to the
        // generic self-closing/void-element case, emitting an empty
        // `script` element and then parsing `alert(1)` as ordinary
        // sibling markup — real script source rendered as visible text
        // instead of staying hidden.
        let html = r#"<script src="x" />alert(1)</script><p>Visible</p>"#;
        let nodes = parse(html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the script element and its sibling <p>, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "script");
        assert_eq!(
            text(children),
            "alert(1)",
            "the script source must stay inside the script element as raw content"
        );
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second top-level node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
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
        //
        // The whole string is one giant unterminated `<a title="x...`
        // attribute (its quote never closes, and no `>` appears anywhere)
        // — `push_oversized_generic_tag` now recognizes this as a real
        // (if malformed) `<a>` tag once its name is found (`"a"`, well
        // within `MAX_TAG_NAME_SCAN`). It does *not* swallow the whole
        // (550,000-byte) document as one single tag, though: once its own
        // `>` search comes up empty even within the generous
        // `MAX_OVERSIZED_TAG_SCAN` bound, it resumes parsing right after
        // that scan window (see `oversized_scan_window_end`) rather than
        // at EOF — "not found within the bound" must never be conflated
        // with "not found at all". For this specific pathological input,
        // that means normal parsing resumes mid-attribute, immediately
        // re-triggers the same oversized-tag handling, and so on in
        // roughly `len / MAX_OVERSIZED_TAG_SCAN` bounded steps — the exact
        // resulting tree shape (nested `<a>`s with stray attribute-soup
        // text fragments) is an accepted artifact of resuming mid-token
        // on a genuinely degenerate input, not something worth pinning
        // down exactly; what matters here is that parsing still completes
        // without panicking and in linear (not quadratic) time.
        let html = "<a title=\"x".repeat(50_000);
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        assert!(
            !nodes.is_empty(),
            "parsing must still produce something, not silently vanish"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "find_tag_end's quoted-attribute path took {:?} — looks quadratic",
            start.elapsed()
        );
    }

    #[test]
    fn find_tag_end_skips_mixed_quote_styles_and_embedded_brackets() {
        // Both quote styles in the same window, one of them (`'b>c'`)
        // containing a literal `>` that must not be mistaken for the
        // real closing delimiter.
        let w = r#" data-x="a" data-y='b>c' data-z="Balance > 100">Visible"#;
        let expected = w.find("\">Visible").unwrap() + 1;
        assert_eq!(find_tag_end(w), Some(expected));
    }

    #[test]
    fn oversized_tag_with_many_short_quote_pairs_before_its_real_close_is_linear_not_quadratic() {
        // Regression, found while investigating an unrelated report: two
        // compounding O(n^2) bugs in `find_tag_end`'s quote-tracking loop,
        // neither caught by `long_run_of_unterminated_quoted_attributes_is_linear_not_quadratic`
        // above (which never reaches a *real* closing `>` at all, so never
        // exercises this loop's steady-state behavior across many
        // completed quote pairs).
        //
        // 1. Each iteration re-derived its `>` candidate via a fresh
        //    `.find('>')`, rescanning from the (advancing) start position
        //    all the way back out to the *same* distant `>` every time —
        //    for a long chain of short quote pairs all closing well
        //    before the real `>`, that's O(pairs * distance-to-`>`) for a
        //    single call (confirmed by timing: doubling the input
        //    quadrupled the time before the fix).
        // 2. Even with that fixed, finding the *next* quote checked both
        //    quote styles via two separate `.find()` calls — since real
        //    HTML overwhelmingly uses only one style consistently, the
        //    *other* style's absence cost a full scan of the remaining
        //    span on every single iteration too, the same shape again.
        let pairs = 20_000;
        let html = format!(
            r"<script data-x={}>Secret</script><p>Visible</p>",
            r#""a""#.repeat(pairs)
        );
        let start = std::time::Instant::now();
        let nodes = parse(&html);
        // Matches the established oversized-script behavior: no node at
        // all for the (unparseable) opening tag, its source discarded —
        // just confirms parsing correctly landed right after the real
        // `>` and continued normally, not that it got lost somewhere in
        // the quote chain.
        assert_eq!(nodes.len(), 1, "got {nodes:?}");
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "p"));
        assert_eq!(text(&nodes), "Visible");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "parse took {:?} — looks quadratic again",
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
