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
                // A new table section closes an open cell, row, or section like a
                // new `<tr>` does: `<thead><tr><th>H<tbody>...` omits `</th>`,
                // `</tr>` and `</thead>` together.
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

/// Locates the byte position right after an oversized tag's own `>` —
/// `after_name` is the position right after the tag name (e.g. right after
/// `<script`, before any attributes), and this searches the still-unparsed
/// attribute list for the real closing `>` via the same quote-aware
/// [`find_tag_end`] [`parse_open_tag`] itself uses, just unbounded this
/// time. Falls back to `len` (EOF) if no real `>` is ever found, matching
/// this parser's usual auto-close-at-EOF tolerance.
///
/// Unbounded is safe here (a bounded-window fallback used to exist and was
/// removed — see [`find_tag_end`]'s own docs for why an earlier version of
/// *it* wasn't safe unbounded, and how that was fixed at the root instead
/// of by adding a window here): scoping the search to a fixed-size window
/// and giving up beyond it doesn't just risk a *slower* search, it's
/// outright wrong — it can never distinguish "the real `>` is genuinely
/// far away" (e.g. a large but legitimate base64 attribute) from "there
/// is no real `>` at all," so it either has to give up early on valid
/// content or, if it instead treats the window boundary as a fake body
/// start, leaks whatever's left of the attribute value (and the literal
/// `">`) as visible text once normal parsing resumes mid-attribute. Now
/// that `find_tag_end` itself costs O(distance to the real `>`, or to EOF
/// if there is none) rather than O(window size), removing the window
/// entirely is both simpler and strictly more correct.
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
fn oversized_tag_body_start(input: &str, after_name: usize, len: usize) -> usize {
    find_tag_end(&input[after_name..]).map_or(len, |i| after_name + i + 1)
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
/// locating the real `>` the same unbounded quote-aware way
/// [`oversized_tag_body_start`] does for the seven tags that function
/// covers — see its docs for why unbounded is both simpler and more
/// correct than a bounded-window fallback. Returns `None` if `input[pos..]`
/// doesn't even look like the start of a tag name — no leading
/// ASCII-alphabetic character, or no tag-name boundary within
/// [`MAX_TAG_NAME_SCAN`] — so the caller can fall through to its plain
/// literal-text handling for a genuinely bogus `<`.
///
/// A trailing XHTML-style `/` (self-closing syntax) is not treated as
/// meaningful here, matching the normal (non-oversized) tag-push path —
/// see its docs for why: real browsers ignore that flag on every ordinary
/// (non-void, non-foreign) HTML element, so honoring it here would push an
/// empty element and let whatever follows become a sibling instead of the
/// element's real content.
fn push_oversized_generic_tag(
    stack: &mut Vec<(String, Vec<Node>, usize)>,
    input: &str,
    pos: usize,
    len: usize,
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
    let gt = find_tag_end(&input[after_name..]);
    let body_start = gt.map_or(len, |i| after_name + i + 1);

    close_implied_tags(stack, &tag);
    if is_void_element(&tag) {
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
        // The name must end at whitespace, '/' or '>', so "</scripty>" does not match.
        // '/' admits the browser-tolerated `</script/>`; without it the scan would run
        // to EOF and drop everything after.
        let is_boundary = after_tag
            .chars()
            .next()
            .is_none_or(|c| c == '>' || c == '/' || c.is_whitespace());
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
/// `</script`). A forward scan that tracks HTML5 attribute-value tokenizer
/// state via [`consume_attr_value`] (see its docs for why this needs to be
/// real forward state, not a backward-looking heuristic), stopping at the
/// first `>` or unquoted `<` — an unquoted `<` ends the scan with `None`
/// (this candidate never closes; some other tag starts here instead), a
/// genuinely quoted value's contents are skipped over regardless of what
/// they contain, and `>` outside any value is the real answer.
///
/// Unbounded by any fixed byte count — unlike a fixed window, this can
/// never reject a genuine closing tag just because its attribute list (or
/// a run of whitespace before `>`, legal per HTML5's end-tag grammar) is
/// long (mirroring why [`oversized_tag_body_start`] moved away from a
/// fixed window) — while still never reintroducing the O(n^2) risk a naive
/// fully-unbounded *quote-blind* search would have here specifically:
/// [`consume_raw_text`]'s outer `match_indices('<')` loop calls this once
/// per candidate and, on failure, moves on to try the *next* one rather
/// than stopping (unlike [`handle_closing_tag`], where a failed unbounded
/// search jumps straight to EOF and ends the whole parse) — so a raw-text
/// body containing many `"</script "` fragments with no `>` anywhere would
/// make a search that ignored `<` entirely pay a full remaining-input scan
/// on every single failed candidate, genuinely O(n^2). Stopping at the
/// first *unquoted* `<` instead makes each candidate's own share of the
/// work bounded by the gap up to that `<` (or, if a genuinely quoted value
/// is open, by the gap to its matching close quote) — every such gap is
/// disjoint from every other candidate's, so the total across every
/// candidate in one [`consume_raw_text`] call stays O(`rest.len()`), not
/// O(candidates × `rest.len()`).
///
/// Quote-awareness matters for two distinct reasons here. First, the same
/// one [`parse_open_tag`]'s own `>` search has: real browsers' end-tag
/// tokenizer *does* enter a quote-aware, attribute-value-like state once
/// whitespace follows the tag name (by which point [`consume_raw_text`]'s
/// `is_boundary` check has already confirmed we're here), so e.g.
/// `</script data-x=">secret">` must skip the quoted `>` and end at the
/// real one after it, not treat the quoted `>` as the close and leak
/// `secret">` as visible text. Second, and specific to this function: a
/// quoted attribute value can itself legally contain a literal `<` (e.g.
/// `</script data-x="<">Visible`) — without quote-awareness on the `<`
/// stop condition too, that embedded `<` would be mistaken for the start
/// of a new tag, ending the scan with `None` even though a real `>`
/// follows right after the value closes, and hiding `Visible` (or, for
/// `title`/`textarea`, being unable to recognize the close at all) as a
/// result.
fn raw_close_tag_end(after_tag: &str) -> Option<usize> {
    let bytes = after_tag.as_bytes();
    let mut i = 0;
    let mut in_unquoted_value = false;
    loop {
        let rel = bytes
            .get(i..)?
            .iter()
            .position(|&b| b == b'>' || b == b'<' || b == b'=' || b.is_ascii_whitespace())?;
        let idx = i + rel;
        match bytes[idx] {
            b'>' => return Some(idx),
            b'<' if !in_unquoted_value => return None,
            b'=' if !in_unquoted_value => {
                let (next_i, quoted) = consume_attr_value(bytes, idx)?;
                in_unquoted_value = !quoted;
                i = next_i;
            }
            b if b.is_ascii_whitespace() => {
                in_unquoted_value = false;
                i = idx + 1;
            }
            _ => {
                // '<' or '=' inside an unquoted value is literal content, not a stop.
                i = idx + 1;
            }
        }
    }
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
    // The bounded search handles every realistic closing tag. Only one whose attributes
    // push its `>` past `MAX_TAG_SCAN` reaches the unbounded search. If that finds no `>`
    // either, the tag runs to EOF.
    let tag_end = find_tag_end(bounded_prefix(rest, MAX_TAG_SCAN)).or_else(|| find_tag_end(rest));
    let new_pos = pos + 2 + tag_end.map_or(rest.len(), |i| i + 1);

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

/// Locates the position right after an HTML comment's closing delimiter,
/// given `pos` pointing at its opening `<!--`. Real browsers accept two
/// different closing delimiters — the standard `-->`, and the
/// parse-error-tolerant `--!>` (WHATWG's "comment end bang state", reached
/// after `--` is followed by `!` instead of `>`) — and close the comment
/// at whichever one is written, not just the standard one. Without
/// recognizing the second form, `<!-- hidden --!><p>Visible</p>` searched
/// only for a literal `-->` that never appears, so the comment (and the
/// bounded `find` failing) fell back to swallowing the *entire rest of
/// the document* as one unterminated comment — discarding `<p>Visible</p>`
/// along with it, not just the comment's own content. Falls back to `len`
/// (EOF) if *neither* delimiter is ever found, matching this parser's
/// usual auto-close-at-EOF tolerance.
fn comment_end(input: &str, pos: usize, len: usize) -> usize {
    let rest = &input[pos..];
    [
        rest.find("-->").map(|i| i + 3),
        rest.find("--!>").map(|i| i + 4),
    ]
    .into_iter()
    .flatten()
    .min()
    .map_or(len, |i| pos + i)
}

/// Parse `input` into a forest of top-level [`Node`]s.
pub(super) fn parse(input: &str) -> Vec<Node> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    // Frames are (tag, children so far, `nearest_structural_idx`). The implicit root is
    // index 0 with an empty tag; it is never popped and is its own structural index.
    let mut stack: Vec<(String, Vec<Node>, usize)> = vec![(String::new(), Vec::new(), 0)];

    while pos < len {
        if bytes[pos] == b'<' {
            if input[pos..].starts_with("<!--") {
                pos = comment_end(input, pos, len);
                continue;
            }
            if input[pos..].starts_with("<!") || input[pos..].starts_with("<?") {
                // Doctype or processing instruction.
                let end = input[pos..].find('>').map_or(len, |i| pos + i + 1);
                pos = end;
                continue;
            }
            if input[pos..].starts_with("</") {
                pos = handle_closing_tag(&mut stack, input, pos);
                continue;
            }
            if let Some((tag, tag_end)) = parse_open_tag(&input[pos..]) {
                pos += tag_end;
                close_implied_tags(&mut stack, &tag);

                if is_raw_text_element(&tag) {
                    // Entities are decoded like ordinary text: required for `title` and
                    // `textarea`, harmless for `script`/`style`, whose content is discarded.
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
                // A trailing self-closing `/` is ignored, as browsers do on non-void
                // elements: `<li/>One<li/>Two` opens two non-empty `<li>`s.
                if is_void_element(&tag) {
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
            // `parse_open_tag` found no `>` within `MAX_TAG_SCAN`. Falling back to literal
            // text would leak the content of these seven tags into the document, so skip
            // to the tag's real `>` first. `script`/`style`/`title` are then discarded
            // through their closing tag, `textarea` keeps its raw text, and
            // `head`/`noscript`/`template` get a frame and parse normally.
            if let Some(name) = oversized_raw_text_tag_name(&input[pos..]) {
                let after_name = pos + 1 + name.len();
                let body_start = oversized_tag_body_start(input, after_name, len);
                pos = if name == "textarea" {
                    push_oversized_raw_text_content_tag(&mut stack, input, body_start, name)
                } else if is_raw_text_element(name) {
                    consume_raw_text(input, body_start, name).1
                } else {
                    push_oversized_nested_tag(&mut stack, name, body_start)
                };
                continue;
            }
            // An oversized ordinary tag, e.g. `<div data-state="...4 KiB+...">`.
            if let Some(new_pos) = push_oversized_generic_tag(&mut stack, input, pos, len) {
                pos = new_pos;
                continue;
            }
            // A lone '<' that isn't a tag is literal text.
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
    // Non-whitespace text implicitly closes an open `<head>`, as an unexpected tag does
    // in `implicitly_closes`: `<head><title>X</title>Visible`.
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
/// Returns `(tag_name, total_consumed_len)`, or `None` if `s` doesn't start
/// with a well-formed tag (e.g. `< foo>` with a space, or an unterminated
/// `<foo`). A trailing XHTML-style `/` is consumed as part of the tag but
/// not reported separately — see [`parse`]'s call site for why it isn't
/// meaningful to this parser.
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
/// remaining input length — see [`MAX_TAG_SCAN`] for why that bound
/// matters.
fn bounded_prefix(s: &str, max_len: usize) -> &str {
    let mut end = s.len().min(max_len);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Given `bytes[eq_idx] == b'='`, consumes what follows per HTML5's "before
/// attribute value state": skips whitespace (permitted between `=` and the
/// value — `title = "x"` is valid, not just `title="x"`), then either
/// enters a quoted value if the next byte is `"`/`'` — skipping to its
/// matching close quote, wherever it is, regardless of what it contains —
/// or starts an unquoted value otherwise (any other byte, including EOF/`>`
/// immediately, i.e. an empty value). Returns the byte offset to resume
/// scanning from, and whether the value was quoted.
///
/// Determining "is this quote/`=` genuine" by looking only at what
/// *immediately precedes* it (a byte or two of backward lookback) cannot
/// work in general — a stray `=` or quote character inside an already-
/// active *unquoted* value (`title=x=">`, a parse-error condition real
/// browsers still tolerate as literal content) is byte-for-byte
/// indistinguishable, looking only backward from that point, from the
/// closing quote of a *properly completed* adjacent quoted attribute with
/// no separating whitespace (`a="x"b="y"` — also valid, unremarkable HTML,
/// since ending a quoted value returns the tokenizer straight to
/// "before attribute name state" with no whitespace required). Both are a
/// quote character immediately followed by more name-like bytes then `=`.
/// The only way to tell them apart is genuine forward state: did the
/// scanner reach this point by properly finishing a quoted value (real
/// name=value boundary, safe to start fresh), or by drifting through an
/// unquoted value that never terminated (not safe — everything in it,
/// `=`/quotes included, is just literal content)? [`find_tag_end`] and
/// [`raw_close_tag_end`] each track this themselves as `in_unquoted_value`
/// state while scanning forward, and only call this function when that
/// state is `false` — i.e. only at a position forward-scanning has already
/// established is a genuine value start.
///
/// Returns `None` only if a quoted value's matching close quote is never
/// found (the whole tag never closes — matches this parser's usual
/// auto-close-at-EOF tolerance for a genuinely unterminated quoted
/// attribute); an unquoted value can't fail to "close" here since the
/// caller keeps scanning through its content until real termination
/// (whitespace or `>`/`<`) on its own.
fn consume_attr_value(bytes: &[u8], eq_idx: usize) -> Option<(usize, bool)> {
    let mut j = eq_idx + 1;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    match bytes.get(j) {
        Some(&quote @ (b'"' | b'\'')) => {
            let after_quote = j + 1;
            let close_off = bytes.get(after_quote..)?.iter().position(|&b| b == quote)?;
            Some((after_quote + close_off + 1, true))
        }
        _ => Some((j, false)),
    }
}

/// Find the byte offset of the `>` that ends an opening tag within `window`,
/// skipping any `>` that appears inside a *genuinely quoted or unquoted*
/// attribute value (e.g. `<div title="Balance > 100">` or, per HTML5's
/// parse-error-tolerant unquoted-value handling, `<div title=x=">`) —
/// otherwise such a `>` is mistaken for the tag's real closing delimiter,
/// truncating the tag early and leaking the rest of the attribute value
/// into the document as literal text.
///
/// Two tiers. First, the fast, heavily-optimized [`str::find`] path: locate
/// the naive first `>`, and if no quote character appears anywhere before
/// it, that naive `>` genuinely is the answer — an unquoted value can't
/// hide a `>` from a naive scan the way a quoted one can, since (per
/// HTML5's own tokenizer) an unquoted value terminates *at* the first `>`
/// it meets, exactly where the naive scan would already stop. This covers
/// the overwhelmingly common case (no attributes, or attributes with no
/// quote character at all) in O(distance to the real `>`, or to EOF if
/// there is none) — essential since `window` is sometimes
/// [`MAX_TAG_SCAN`]-bounded (cheap either way) and sometimes unbounded,
/// straight from
/// [`oversized_tag_body_start`]/[`push_oversized_generic_tag`] on the
/// entire remaining document, where a slower per-byte scan would cost far
/// more (confirmed by timing: this tier alone, not the second, is what
/// keeps `"<a".repeat(100_000)` linear — a manual byte-by-byte scan here
/// instead measured over 3s for that input, an easy trap since it's still
/// O(n) asymptotically, just with a much larger constant).
///
/// Second, only once a quote is confirmed present before the naive `>`: a
/// forward scan tracking real HTML5 attribute-value tokenizer state via
/// [`consume_attr_value`] — see its docs for why this needs to be genuine
/// forward state, not a backward-looking heuristic, to correctly
/// distinguish a quote that's a genuine value opener from one that isn't.
/// Bounded the same way the pre-check above already established a quote
/// exists before some point — this tier existing at all is gated on that,
/// so it doesn't reopen the "prove a quote style is absent over a huge
/// span" O(n^2) shape a previous version of this pre-check had (confirmed
/// by timing: 16,000 single-quoted 5 KiB tags took over two minutes before
/// *that* fix) — the gate here checks presence within a bounded prefix
/// (`window[..naive_gt]`), not the whole window.
fn find_tag_end(window: &str) -> Option<usize> {
    let naive_gt = window.find('>')?;
    if !window[..naive_gt].contains('"') && !window[..naive_gt].contains('\'') {
        return Some(naive_gt);
    }
    let bytes = window.as_bytes();
    let mut i = 0;
    let mut in_unquoted_value = false;
    loop {
        let rel = bytes
            .get(i..)?
            .iter()
            .position(|&b| b == b'>' || b == b'=' || b.is_ascii_whitespace())?;
        let idx = i + rel;
        match bytes[idx] {
            b'>' => return Some(idx),
            b'=' if !in_unquoted_value => {
                let (next_i, quoted) = consume_attr_value(bytes, idx)?;
                in_unquoted_value = !quoted;
                i = next_i;
            }
            b if b.is_ascii_whitespace() => {
                in_unquoted_value = false;
                i = idx + 1;
            }
            _ => {
                // '=' inside an unquoted value is literal content.
                i = idx + 1;
            }
        }
    }
}

fn parse_open_tag(s: &str) -> Option<(String, usize)> {
    debug_assert!(s.starts_with('<'));
    let rest = &s[1..];
    let first = rest.chars().next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }

    let window = bounded_prefix(rest, MAX_TAG_SCAN);

    // Find `>` before allocating, so a malformed `<` costs only the bounded scan.
    let gt = find_tag_end(window)?;

    let name_end = window[..gt]
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(gt);
    let tag = window[..name_end].to_ascii_lowercase();
    Some((tag, 1 + gt + 1))
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
        // Bound the `;` search before scanning: an unbounded search makes a run of bare
        // `&` O(n^2). Entity names are at most 6 chars (numeric ones up to 8), so 11
        // chars (`&` plus 10) is ample.
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
                // Skip the consumed entity body.
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
        // Any tag outside `is_structural_tag`'s closed set is transparent, including
        // one nobody listed.
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
        // Block tags with no implied-close rule are not scope barriers, even though
        // `layout.rs` renders them as blocks: the `<li>` beneath stays reachable.
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
        // A transparent `<div>` must not let the search reach past a nested `<ul>`:
        // "Child" stays in its own nested list.
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
        // A new `<tr>` closes both the open `<td>` and the `<tr>` above it.
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
        // `<table>` must close the `<p>` beneath the open `<strong>`, not just check the
        // top frame.
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
        // `head` is not rendered, so a `<body>` nested inside it would drop the document.
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
        // HTML5 also permits omitting the `<body>` start tag: any non-head tag closes `<head>`.
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
        // The attribute pushes the tag's `>` past `MAX_TAG_SCAN` (4 KiB).
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
        // The raw-text scan must start after the tag's real `>`, not inside the quoted
        // attribute that contains `</script>`.
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
        // Literal-text fallback here would also close the open `<head>` (see `push_text`),
        // leaking both the attribute and the title text.
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
        // These parse normally and are hidden later by `layout.rs`'s `is_non_rendered`, so
        // the parser only has to keep "Secret" inside the element's subtree. `text()` does
        // not filter hidden tags, so the test checks structure instead.
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
        // `</head>` is optional, so a raw-text scan for it would discard the rest of the
        // document.
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
        // A raw-text scan would stop at the inner `</template>` and leave "leak" as a
        // rendered top-level text node.
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
        // A non-void `/>` still opens a frame (browsers ignore it), so "Visible" is
        // `my-widget`'s content. A void element pushes no frame, so its text is a sibling.
        let oversized_attr = "Q".repeat(5000);
        let html = format!(r#"<my-widget data-x="{oversized_attr}" />Visible"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            1,
            "expected a single <my-widget>, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "my-widget");
        assert_eq!(text(children), "Visible");

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
        // The closing tag's attributes exceed `MAX_TAG_SCAN`; "Visible" must not be lost
        // to the EOF fallback.
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
    fn oversized_ordinary_tag_far_beyond_any_fixed_bound_still_finds_its_real_close() {
        // A large data URI: the oversized-tag search has no window, so parsing resumes at
        // the real `>` however far away it is.
        let oversized_attr = "Q".repeat(300 * 1024);
        let html = format!(r#"<img src="{oversized_attr}">Visible"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected the void <img> and its sibling text, with no leaked attribute-soup \
             siblings in between, got a tree with {} nodes",
            nodes.len()
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "img");
        assert!(
            children.is_empty(),
            "img is a void element, got {children:?}"
        );
        assert_eq!(
            nodes[1],
            Node::Text("Visible".to_owned()),
            "the oversized attribute value must not leak into visible text"
        );
    }

    #[test]
    fn long_run_of_single_quoted_oversized_divs_is_linear_not_quadratic() {
        // `find_tag_end` runs unbounded here. Its quote check must cover only the text
        // before the first `>`; proving `"` absent from the whole remainder on every tag
        // is O(n²).
        let candidate = format!("<div data-x='{}'>", "A".repeat(5000));
        let make = |k: usize| candidate.repeat(k);
        let nodes = crate::pdf::linear::assert_linear("parse", 2_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1, "expected one nested chain of <div>s");
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
        // `</>`'s empty name must not match the root frame's empty-string tag.
        assert_eq!(text(&parse("hello</>world")), "helloworld");
        assert_eq!(text(&parse("<div></></div>")), "");
        assert_eq!(text(&parse("</>")), "");
    }

    #[test]
    fn html_comments_are_never_rendered() {
        let nodes = parse("Before<!-- hidden -->After");
        assert_eq!(text(&nodes), "BeforeAfter");
    }

    #[test]
    fn abrupt_empty_comment_closes_immediately_not_at_the_next_real_close() {
        // `<!-->` closes because `find("-->")` reuses the opener's own two dashes.
        let nodes = parse("<!--><p>Visible</p>");
        assert_eq!(nodes.len(), 1, "expected just the <p>, got {nodes:?}");
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "p"));
        assert_eq!(text(&nodes), "Visible");
    }

    #[test]
    fn comment_closed_with_the_browser_tolerated_bang_terminator_does_not_swallow_the_rest_of_the_document()
     {
        let nodes = parse("<!-- hidden --!><p>Visible</p>");
        assert_eq!(nodes.len(), 1, "got {nodes:?}");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the comment to be skipped and only the <p> to remain")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn comment_closes_at_whichever_terminator_appears_first() {
        let nodes = parse("<!-- a --!>b<!-- c -->d");
        assert_eq!(text(&nodes), "bd");
    }

    #[test]
    fn ordinary_closing_tag_with_a_browser_tolerated_attribute_still_matches_its_opener() {
        // The name ends at whitespace, and the `>` search skips the quoted `>`.
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
        // Unsupported `&pound;` passes through literally.
        assert_eq!(text(&nodes), "Fish & Chips — &pound;5 AB");
    }

    #[test]
    fn long_run_of_unterminated_ampersands_is_linear_not_quadratic() {
        let make = |k: usize| "&".repeat(k);
        let nodes =
            crate::pdf::linear::assert_linear("decode_entities", 200_000, make, |html| parse(html));
        let html = make(200_000);
        assert_eq!(
            text(&nodes),
            html,
            "unterminated `&` passes through unchanged"
        );
    }

    #[test]
    fn long_run_of_unterminated_open_tags_is_linear_not_quadratic() {
        // A failed `parse_open_tag` consumes nothing and is retried at the next byte, so
        // its `>` search must stay bounded.
        let make = |k: usize| "<a".repeat(k);
        let nodes =
            crate::pdf::linear::assert_linear("parse_open_tag", 100_000, make, |html| parse(html));
        let html = make(100_000);
        assert_eq!(
            text(&nodes),
            html,
            "unterminated `<a` fragments pass through unchanged as literal text"
        );
    }

    #[test]
    fn long_run_of_unmatched_closing_tags_is_linear_not_quadratic() {
        // No `</x>` matches, so each close would scan the whole stack without
        // `MAX_CLOSE_SCAN`.
        let make = |k: usize| format!("{}{}", "<a>".repeat(k), "</x>".repeat(k));
        let nodes = crate::pdf::linear::assert_linear("parse", 50_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn long_run_of_never_closing_wrapper_tags_is_linear_not_quadratic() {
        let make = |k: usize| "<span>".repeat(k);
        let nodes = crate::pdf::linear::assert_linear("parse", 100_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1, "expected one deeply nested <span> tree");
    }

    #[test]
    fn long_run_of_never_closing_phrasing_wrappers_is_linear_not_quadratic() {
        // `close_implied_tags` looks past transparent frames. The cached
        // `nearest_structural_idx` keeps that O(1) per tag instead of a walk down the stack.
        let make = |k: usize| "<strong>".repeat(k);
        let nodes = crate::pdf::linear::assert_linear("parse", 100_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1, "expected one deeply nested <strong> tree");
    }

    #[test]
    fn long_run_of_unterminated_raw_text_closes_is_linear_not_quadratic() {
        // Each failed `</script ` candidate's `>` search must stop at the next `<`, not
        // scan to EOF.
        let make = |k: usize| format!("<script>{}", "</script ".repeat(k));
        let nodes = crate::pdf::linear::assert_linear("parse", 50_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
    }

    #[test]
    fn raw_text_closing_tag_skips_a_quoted_bracket_look_alike() {
        // Browsers read attributes on an end tag quote-aware, so the quoted `>` is not
        // the close.
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
        // HTML5's end-tag grammar allows any amount of whitespace before `>`.
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
    fn raw_text_closing_tag_with_a_long_non_whitespace_attribute_is_still_recognized() {
        let value = "x".repeat(100);
        let html = format!(r#"<script>hidden</script data-x="{value}">Visible"#);
        let nodes = parse(&html);
        assert_eq!(
            nodes.len(),
            2,
            "expected script + trailing text, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "script");
        assert_eq!(text(children), "hidden");
        assert_eq!(nodes[1], Node::Text("Visible".to_owned()));
    }

    #[test]
    fn raw_text_closing_tag_recognizes_xhtml_style_self_closing_slash() {
        let html = "<script>hidden</script/><p>Visible</p>";
        let nodes = parse(html);
        assert_eq!(nodes.len(), 2, "expected script + <p>, got {nodes:?}");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "script");
        assert_eq!(text(children), "hidden");
        let Node::Element { tag, children } = &nodes[1] else {
            panic!("expected the second node to be an element")
        };
        assert_eq!(tag, "p");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn long_run_of_raw_text_closes_padded_with_whitespace_is_linear_not_quadratic() {
        let candidate = format!("</script{}", " ".repeat(200));
        let make = |k: usize| format!("<script>{}", candidate.repeat(k));
        let nodes = crate::pdf::linear::assert_linear("parse", 2_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
    }

    #[test]
    fn long_run_of_raw_text_closes_padded_with_long_attributes_is_linear_not_quadratic() {
        // `consume_raw_text` tries the next candidate when one fails, so each candidate's `>`
        // search must stop at the next `<` rather than scan to EOF.
        let candidate = format!("</script data-x=\"{}", "y".repeat(300));
        let make = |k: usize| format!("<script>{}", candidate.repeat(k));
        let nodes = crate::pdf::linear::assert_linear("parse", 2_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
    }

    #[test]
    fn self_closing_syntax_on_a_raw_text_element_is_ignored_like_a_real_browser() {
        // Browsers ignore `/>` on non-void elements, so the script body stays raw content.
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
    fn self_closing_syntax_on_an_ordinary_non_void_element_is_ignored_like_a_real_browser() {
        let nodes = parse("<ul><li/>One<li/>Two</ul>");
        assert_eq!(nodes.len(), 1, "expected a single <ul>, got {nodes:?}");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "ul");
        assert_eq!(
            children.len(),
            2,
            "expected two <li> children, got {children:?}"
        );
        for (child, expected) in children.iter().zip(["One", "Two"]) {
            let Node::Element { tag, children } = child else {
                panic!("expected an <li> element, got {child:?}")
            };
            assert_eq!(tag, "li");
            assert_eq!(text(children), expected);
        }
    }

    #[test]
    fn attribute_value_quote_separated_from_equals_by_whitespace_is_still_recognized() {
        // HTML5 allows whitespace around `=`.
        let nodes = parse(r#"<div title = "Balance > 100">Visible</div>"#);
        assert_eq!(nodes.len(), 1, "expected a single <div>, got {nodes:?}");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn stray_equals_and_quote_inside_an_already_started_unquoted_value_does_not_swallow_the_tag() {
        // `title=x="` starts an unquoted value at `x`; the `=` and `"` after it are literal
        // content, not a new quoted value.
        let nodes = parse(r#"<div title=x=">Visible</div>"#);
        assert_eq!(nodes.len(), 1, "expected a single <div>, got {nodes:?}");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn adjacent_quoted_attributes_with_no_separating_whitespace_are_both_recognized() {
        // A quoted value needs no whitespace before the next attribute, so `b`'s quote opens a
        // real value and its `>` is not the tag's end.
        let nodes = parse(r#"<div a="x"b=">secret">Visible</div>"#);
        assert_eq!(nodes.len(), 1, "expected a single <div>, got {nodes:?}");
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn closing_tag_deeper_than_the_scan_window_is_ignored_not_matched() {
        // A close whose opener is more than `MAX_CLOSE_SCAN` frames back is ignored, so "tail"
        // stays nested. Accepted: only pathological input nests this deep.
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
        // Tailwind-style class lists run to hundreds of bytes; `MAX_TAG_SCAN` must admit them.
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
        let nodes = parse("<div title='Balance > 100'>Visible</div>");
        assert_eq!(nodes.len(), 1);
        let Node::Element { children, .. } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn literal_quote_in_an_unquoted_attribute_value_does_not_swallow_the_tag() {
        // An unquoted value may contain a literal quote (`title=it's`); it opens nothing.
        let nodes = parse("<div title=it's>Visible</div>");
        assert_eq!(
            nodes.len(),
            1,
            "expected a single <div> node, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(tag, "div");
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn raw_text_closing_tag_with_a_literal_angle_bracket_in_a_quoted_attribute_still_closes() {
        let nodes = parse(r#"<script>hidden</script data-x="<">Visible"#);
        assert_eq!(
            nodes.len(),
            2,
            "expected script + trailing text, got {nodes:?}"
        );
        let Node::Element { tag, children } = &nodes[0] else {
            panic!("expected the first node to be an element")
        };
        assert_eq!(tag, "script");
        assert_eq!(text(children), "hidden");
        assert_eq!(nodes[1], Node::Text("Visible".to_owned()));
    }

    #[test]
    fn long_run_of_unterminated_quoted_attributes_is_linear_not_quadratic() {
        // Exercises `find_tag_end`'s quote-tracking path. The quote never closes and no `>`
        // exists, so the whole input is one `<a>` auto-closed at EOF, as a browser's tokenizer
        // stays in attribute-value state until EOF.
        let make = |k: usize| "<a title=\"x".repeat(k);
        let nodes = crate::pdf::linear::assert_linear(
            "find_tag_end's quoted-attribute path",
            50_000,
            make,
            |html| parse(html),
        );
        assert_eq!(
            nodes,
            vec![Node::Element {
                tag: "a".to_owned(),
                children: Vec::new(),
            }],
            "the whole unterminated attribute must be swallowed into one auto-closed <a>, \
             not leaked as visible text"
        );
    }

    #[test]
    fn long_run_of_stray_apostrophes_before_the_real_close_is_linear_not_quadratic() {
        // No apostrophe directly follows `=`, so none opens a quoted span.
        let make = |k: usize| format!("<div title={}>Visible</div>", "it's ".repeat(k));
        let nodes =
            crate::pdf::linear::assert_linear("find_tag_end", 50_000, make, |html| parse(html));
        assert_eq!(
            nodes.len(),
            1,
            "expected a single <div> node, got {nodes:?}"
        );
        let Node::Element { children, .. } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn long_run_of_whitespace_separated_stray_apostrophes_is_linear_not_quadratic() {
        // Each apostrophe follows whitespace rather than `=`, so none opens a quoted value.
        let make = |k: usize| format!("<div title={}>Visible</div>", "x     '".repeat(k));
        let nodes =
            crate::pdf::linear::assert_linear("find_tag_end", 50_000, make, |html| parse(html));
        assert_eq!(
            nodes.len(),
            1,
            "expected a single <div> node, got {nodes:?}"
        );
        let Node::Element { children, .. } = &nodes[0] else {
            panic!("expected an element")
        };
        assert_eq!(text(children), "Visible");
    }

    #[test]
    fn long_run_of_raw_text_closes_with_quoted_angle_brackets_is_linear_not_quadratic() {
        let candidate = "</script data-x=\"<\" ";
        let make = |k: usize| format!("<script>{}", candidate.repeat(k));
        let nodes = crate::pdf::linear::assert_linear("parse", 2_000, make, |html| parse(html));
        assert_eq!(nodes.len(), 1);
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "script"));
    }

    #[test]
    fn find_tag_end_skips_mixed_quote_styles_and_embedded_brackets() {
        let w = r#" data-x="a" data-y='b>c' data-z="Balance > 100">Visible"#;
        let expected = w.find("\">Visible").unwrap() + 1;
        assert_eq!(find_tag_end(w), Some(expected));
    }

    #[test]
    fn oversized_tag_with_many_short_quote_pairs_before_its_real_close_is_linear_not_quadratic() {
        // Many completed quote pairs before the real `>`: each step of the scan must cost
        // only its own gap, not a rescan toward the distant `>`.
        let pairs = 20_000;
        let make = |k: usize| {
            format!(
                r"<script data-x={}>Secret</script><p>Visible</p>",
                r#""a""#.repeat(k)
            )
        };
        let nodes = crate::pdf::linear::assert_linear("parse", pairs, make, |html| parse(html));
        // The oversized `<script>` yields no node; parsing resumes after its real `>`.
        assert_eq!(nodes.len(), 1, "got {nodes:?}");
        assert!(matches!(&nodes[0], Node::Element { tag, .. } if tag == "p"));
        assert_eq!(text(&nodes), "Visible");
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
        // No recursive `text` walk here: it would overflow in the test itself. Dropping `nodes`
        // exercises `Node`'s iterative `Drop`.
        let nodes = parse(&html);
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn malformed_lone_angle_bracket_is_literal_text() {
        let nodes = parse("a < b");
        assert_eq!(text(&nodes), "a < b");
    }
}
