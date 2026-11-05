//! What a reader can see and click: [`readable`] blanks every region of a
//! document where a row-shaped line or a link renders as something else.

use std::borrow::Cow;

use super::scan::{
    ATX, FENCE, LIST_COL, LIST_ITEM, Labels, SETEXT, THEMATIC, blank, defn_entries, label_key,
    opens_fence, quote_depth, tail_at, unquote,
};
use super::text::{Text, expand_tabs, next_tab_stop};
use crate::docs::re::{is_py_space, pattern, py_strip};

/// Stands in for a rendered image or code span: visible content, matched by
/// no markdown construct, and not a format character.
const MARK: char = '\u{fffc}';

/// The characters a live backslash escape stops opening their construct.
const ESCAPABLE: &[char] = &['[', ']', '!', '<', '`'];

/// Raw HTML blocks that run to their closing tag.
const LITERAL: &[&str] = &["pre", "script", "style", "textarea"];

/// `CommonMark`'s type-6 block tags, which open a raw block even with text
/// after them.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

pattern! { static HTML_OPEN = r"\A {0,3}<(/?)([a-zA-Z][a-zA-Z0-9-]*)"; }
pattern! { static COMMENT_BLOCK = r"\A {0,3}<!--"; }
pattern! { static DECL_PI = r"\A {0,3}<\?"; }
pattern! { static DECL_CDATA = r"\A {0,3}<!\[CDATA\["; }
pattern! { static DECL_BANG = r"\A {0,3}<![A-Z]"; }
pattern! {
    /// A URI or email autolink, by `CommonMark`'s grammar: a scheme is 2 to 32
    /// characters, and an email local part has no brackets.
    static AUTOLINK = concat!(
        r"\A(?:<[A-Za-z][A-Za-z0-9+.-]{1,31}:[^<>\x00\x02-\x20]*>",
        r"|<[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+",
        r"@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?",
        r"(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*>)",
    );
}
pattern! { static INLINE_DECL = r"\A<![A-Za-z][^>]*>"; }
pattern! { static BLANK_LINE = r"\n[ \t]*\n"; }

/// A raw HTML block opener on `line`: `(closing, tag)`. `CommonMark` ends a
/// tag name at whitespace, `>`, `/>` or the end of the line, so `<div.class`
/// opens nothing.
fn html_open(line: &str) -> Option<(bool, String)> {
    let c = HTML_OPEN.captures(line)?;
    let rest = &line[c[0].len()..];
    let ends = rest.is_empty() || rest.starts_with([' ', '\t', '>']) || rest.starts_with("/>");
    ends.then(|| (!c[1].is_empty(), c[2].to_ascii_lowercase()))
}

/// True when the whitespace after a newline at `pos` does not end in another
/// newline: the newline does not start a blank line.
fn not_blank_after(s: &[u8], pos: usize) -> bool {
    let mut k = pos;
    while k < s.len() && s[k] != b'\n' && is_py_space(char::from(s[k])) {
        k += 1;
    }
    k >= s.len() || s[k] != b'\n'
}

/// Whitespace inside a tag from `i`: spaces and tabs, and at most one line
/// ending, which may not start a blank line.
fn tag_ws(s: &[u8], mut i: usize) -> usize {
    let mut newline = false;
    while i < s.len() {
        match s[i] {
            b' ' | b'\t' => i += 1,
            b'\n' if !newline && not_blank_after(s, i + 1) => {
                newline = true;
                i += 1;
            }
            _ => break,
        }
    }
    i
}

fn name_end(
    s: &[u8],
    i: usize,
    first: impl Fn(u8) -> bool,
    rest: impl Fn(u8) -> bool,
) -> Option<usize> {
    if i < s.len() && first(s[i]) {
        let mut j = i + 1;
        while j < s.len() && rest(s[j]) {
            j += 1;
        }
        Some(j)
    } else {
        None
    }
}

/// A quoted attribute value at `i`, which may cross a line ending but not a
/// blank line.
fn quoted_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = *bytes.get(start).filter(|&&b| b == b'"' || b == b'\'')?;
    let mut at = start + 1;
    while at < bytes.len() {
        match bytes[at] {
            b if b == quote => return Some(at + 1),
            b'\n' if !not_blank_after(bytes, at + 1) => return None,
            _ => at += 1,
        }
    }
    None
}

/// The end of an inline HTML tag at `pos`, by `CommonMark`'s grammar: an open
/// tag with attributes, or a closing tag.
fn inline_tag_at(s: &[u8], pos: usize) -> Option<usize> {
    let tag_name = |b: u8| b.is_ascii_alphanumeric() || b == b'-';
    if s.get(pos) != Some(&b'<') {
        return None;
    }
    if s.get(pos + 1) == Some(&b'/') {
        let i = name_end(s, pos + 2, |b| b.is_ascii_alphabetic(), tag_name)?;
        let i = tag_ws(s, i);
        return (s.get(i) == Some(&b'>')).then_some(i + 1);
    }
    let mut i = name_end(s, pos + 1, |b| b.is_ascii_alphabetic(), tag_name)?;
    loop {
        let after_ws = tag_ws(s, i);
        if after_ws == i {
            break;
        }
        let Some(attr_end) = name_end(
            s,
            after_ws,
            |b| b.is_ascii_alphabetic() || b == b'_' || b == b':',
            |b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'),
        ) else {
            break;
        };
        i = attr_end;
        let eq = tag_ws(s, attr_end);
        if s.get(eq) == Some(&b'=') {
            let v = tag_ws(s, eq + 1);
            let bare = |b: u8| {
                !matches!(
                    b,
                    b' ' | b'\t' | b'\n' | b'"' | b'\'' | b'=' | b'<' | b'>' | b'`'
                )
            };
            if let Some(end) = quoted_end(s, v).or_else(|| name_end(s, v, bare, bare)) {
                i = end;
            }
        }
    }
    let mut i = tag_ws(s, i);
    if s.get(i) == Some(&b'/') {
        i += 1;
    }
    (s.get(i) == Some(&b'>')).then_some(i + 1)
}

/// True when the five characters at `i` are `<img` and a whitespace, `/` or
/// `>`, compared as Python's `re.IGNORECASE` compares them.
fn is_img_tag(t: &Text, i: usize) -> bool {
    let at = |k: usize| t.c.get(i + k).copied();
    at(0) == Some('<')
        && matches!(at(1), Some('i' | 'I' | '\u{130}' | '\u{131}'))
        && matches!(at(2), Some('m' | 'M'))
        && matches!(at(3), Some('g' | 'G'))
        && at(4).is_some_and(|c| is_py_space(c) || c == '/' || c == '>')
}

/// The column at which `col` begins in `s`, as an offset; `None` when a tab
/// straddles it.
fn byte_at_col(s: &str, col: usize) -> Option<usize> {
    let mut c = 0;
    for (k, ch) in s.char_indices() {
        if c == col {
            return Some(k);
        }
        if c > col {
            return None;
        }
        c = if ch == '\t' { next_tab_stop(c) } else { c + 1 };
    }
    (c == col).then_some(s.len())
}

/// The column an item's content starts at on a tab-expanded, unquoted line.
/// A thematic break is not a list item.
fn list_content_col(expanded: &str) -> Option<usize> {
    if THEMATIC.is_match(expanded) {
        return None;
    }
    let m = LIST_COL.captures(expanded)?;
    let marker_end = m[1].len() + m[2].len();
    let pad = m[3].len();
    Some(marker_end + if pad <= 4 { pad } else { 1 })
}

/// `(closer_start, end)` for the code span whose opening run starts at
/// `pos`. It closes on a run of the same length and cannot cross a blank
/// line.
fn code_span_at(t: &Text, pos: usize) -> Option<(usize, usize)> {
    let n = t.len();
    let mut j = pos;
    while j < n && t.c[j] == '`' {
        j += 1;
    }
    let run = j - pos;
    while j < n {
        if t.c[j] == '\n' {
            let mut k = j + 1;
            while k < n && matches!(t.c[k], ' ' | '\t') {
                k += 1;
            }
            if k >= n || t.c[k] == '\n' {
                return None;
            }
        }
        if t.c[j] != '`' {
            j += 1;
            continue;
        }
        let closer = j;
        while j < n && t.c[j] == '`' {
            j += 1;
        }
        if j - closer == run {
            return Some((closer, j));
        }
    }
    None
}

/// True when the character at `pos` follows an odd run of backslashes.
fn escaped(t: &Text, pos: usize) -> bool {
    t.c[..pos].iter().rev().take_while(|&&c| c == '\\').count() % 2 == 1
}

/// The end of a non-tag inline raw HTML construct at `pos` (a CDATA
/// section, a processing instruction or a declaration) closing before
/// `limit`, the end of the paragraph.
fn inline_raw_at(t: &Text, pos: usize, limit: usize) -> Option<usize> {
    for (opener, closer) in [("<![CDATA[", "]]>"), ("<?", "?>")] {
        if t.starts_with(pos, opener) {
            return t
                .find(closer, pos + opener.len(), limit)
                .map(|end| end + closer.len());
        }
    }
    t.match_at(&INLINE_DECL, pos)
        .map(|m| m.end())
        .filter(|&end| end <= limit)
}

/// The end of the image label or reference label opening at `pos`: balanced
/// brackets, not across a blank line, with code spans skipped.
fn balanced(t: &Text, pos: usize) -> Option<usize> {
    let n = t.len();
    let (mut depth, mut k) = (1usize, pos + 1);
    while k < n && depth > 0 {
        let ch = t.c[k];
        if ch == '\\' {
            k += if t.is(k + 1, '\n') { 1 } else { 2 };
            continue;
        }
        if ch == '\n' {
            let mut j = k + 1;
            while j < n && matches!(t.c[j], ' ' | '\t') {
                j += 1;
            }
            if j >= n || t.c[j] == '\n' {
                return None;
            }
        }
        if ch == '`'
            && let Some((_, end)) = code_span_at(t, k)
        {
            k = end;
            continue;
        }
        if ch == '[' {
            depth += 1;
        } else if ch == ']' {
            depth -= 1;
        }
        k += 1;
    }
    (depth == 0).then_some(k)
}

/// The labels `t` appears to define, read from a first pass of
/// [`readable`] that resolves nothing. That pass masks no reference image,
/// which is the conservative direction, and fences and comments do not
/// depend on labels.
pub fn candidate_labels(t: &Text) -> Labels {
    defn_entries(&readable(t, Some(&Labels::new())), None)
        .into_iter()
        .map(|d| d.key)
        .collect()
}

/// The open list items: `(content column, quote depth it opened at)`.
type ListCols = Vec<(usize, usize)>;

/// The part of `t` a reader can see and click, blanked space for space so
/// line numbers stay put.
///
/// It removes fenced code, HTML comments, raw HTML blocks and declarations,
/// indented code, code spans, images (label and all) and inline HTML tags.
/// One scan: whichever construct opens first consumes its own extent, and an
/// unterminated one is left literal rather than run to the end of the file.
/// `resolved` defaults to [`candidate_labels`].
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
pub fn readable(t: &Text, resolved: Option<&Labels>) -> Text {
    let resolved: Cow<'_, Labels> =
        resolved.map_or_else(|| Cow::Owned(candidate_labels(t)), Cow::Borrowed);
    let mut out = t.c.clone();
    let n = t.len();
    let mut lowered: Option<Vec<char>> = None;
    let mut i = 0;
    let mut in_paragraph = false;
    let mut list_cols: ListCols = Vec::new();

    // Where a block opened inside a quote of `depth` has to stop: a quote
    // is a container, so nothing opened in one outlives it.
    let quote_limit = |after: usize, depth: usize| -> usize {
        if depth == 0 {
            return n;
        }
        let mut k = after;
        while k <= n {
            let e = t.line_end(k);
            if quote_depth(t.ascii(k, e)) < depth {
                return k;
            }
            if e >= n {
                break;
            }
            k = e + 1;
        }
        n
    };

    'scan: while i < n {
        let at_line_start = i == 0 || t.c[i - 1] == '\n';
        let eol = t.line_end(i);

        if at_line_start {
            let line = t.ascii(i, eol);
            // Block-quote markers come off before anything measures the
            // line: inside a quote, indentation counts from the marker.
            let depth = quote_depth(line);
            let content = if depth > 0 { unquote(line) } else { line };
            let expanded = expand_tabs(content);
            let stripped = expanded.trim_start_matches(' ');
            let indent = expanded.len() - stripped.len();
            if stripped.is_empty() {
                in_paragraph = false;
                i = if eol < n { eol + 1 } else { n };
                continue;
            }
            // A line below an item's content column leaves that item for
            // the enclosing one, and leaving a quote closes the items
            // opened inside it.
            while list_cols.last().is_some_and(|&(_, d)| d > depth) {
                list_cols.pop();
            }
            if !LIST_ITEM.is_match(content) || THEMATIC.is_match(content) {
                while list_cols.last().is_some_and(|&(c, _)| indent < c) {
                    list_cols.pop();
                }
            }
            let mut list_col = list_cols.last().map_or(0, |&(c, _)| c);
            // Blocks inside a list item are measured from its content
            // column, not the margin.
            let rel = if list_col > 0 && indent >= list_col && content == expanded {
                &content[list_col..]
            } else {
                content
            };
            // Indented code opens four columns past the content column, and
            // cannot interrupt a paragraph.
            if indent >= 4 + list_col && !in_paragraph {
                let mut j = i;
                while j < n {
                    let stop = t.line_end(j);
                    let raw = t.ascii(j, stop);
                    if depth > 0 && quote_depth(raw) < depth {
                        break;
                    }
                    let seg = expand_tabs(if depth > 0 { unquote(raw) } else { raw });
                    let body = seg.trim_start_matches(' ');
                    if !body.is_empty() && seg.len() - body.len() < 4 + list_col {
                        break;
                    }
                    blank(&mut out, j, stop);
                    j = stop + 1;
                }
                i = j.min(n);
                in_paragraph = false;
                continue;
            }
            if let Some(col) = list_content_col(&expanded) {
                while list_cols.last().is_some_and(|&(c, _)| indent < c) {
                    list_cols.pop();
                }
                list_cols.push((col, depth));
                list_col = col;
                // An overpadded marker puts indented code on its own line.
                if LIST_COL
                    .captures(&expanded)
                    .is_some_and(|pad| pad[3].len() > 4)
                    && let Some(off) = byte_at_col(content, col)
                {
                    let start = i + (line.len() - content.len()) + off;
                    blank(&mut out, start, eol);
                    in_paragraph = false;
                    i = if eol < n { eol + 1 } else { n };
                    continue;
                }
            }
            let was_paragraph = in_paragraph;
            // Whether this line opens a raw HTML block is decided once, here.
            // An autolink is a link, and a type-7 tag opens a block only when
            // alone on its line and not interrupting a paragraph.
            let mut html = if AUTOLINK.is_match(rel) {
                None
            } else {
                html_open(rel)
            };
            if let Some((_, tag)) = &html {
                let tag = tag.as_str();
                let alone = {
                    let s = py_strip(rel).as_bytes();
                    inline_tag_at(s, 0) == Some(s.len())
                };
                let type7 = !LITERAL.contains(&tag) && !BLOCK_TAGS.contains(&tag);
                if type7 && (!alone || was_paragraph) {
                    html = None;
                }
            }
            // A Setext underline ends the paragraph only under a paragraph.
            let setext = SETEXT.is_match(rel) && was_paragraph;
            in_paragraph = !(ATX.is_match(rel)
                || opens_fence(rel)
                || html.is_some()
                || THEMATIC.is_match(rel)
                || setext);

            if let Some(m) = FENCE.captures(rel).filter(|_| opens_fence(rel)) {
                let fence_char = m[1].as_bytes()[0];
                let length = m[1].len();
                let mut j = eol + 1;
                if j > n {
                    blank(&mut out, i, n);
                    i = n;
                    continue;
                }
                loop {
                    let stop = t.line_end(j);
                    let seg = t.ascii(j, stop);
                    // A fence opened inside a quote ends when the quote does.
                    if depth > 0 && quote_depth(seg) < depth {
                        blank(&mut out, i, j);
                        i = j;
                        break;
                    }
                    // The closer is measured from the opener's column.
                    let mut inner = if depth > 0 { unquote(seg) } else { seg };
                    if list_col > 0 && !inner.contains('\t') {
                        let pre = inner.len() - inner.trim_start_matches(' ').len();
                        if pre >= list_col {
                            inner = &inner[list_col..];
                        }
                    }
                    if let Some(c) = FENCE.captures(inner)
                        && c[1].as_bytes()[0] == fence_char
                        && c[1].len() >= length
                        && py_strip(&c[2]).is_empty()
                    {
                        blank(&mut out, i, stop);
                        i = stop;
                        break;
                    }
                    if stop >= n {
                        blank(&mut out, i, n);
                        i = n;
                        break;
                    }
                    j = stop + 1;
                }
                continue;
            }

            // A raw HTML block that starts a line owns that line to its end,
            // whatever follows its terminator.
            if COMMENT_BLOCK.is_match(rel) {
                let lead = rel.trim_start_matches([' ', '\t']);
                let short = lead.starts_with("<!--->") || lead.starts_with("<!-->");
                let close = t.find("-->", i + if short { 2 } else { 4 }, n);
                let stop = close.map_or(n, |c| t.line_end(c + 3));
                let stop = stop.min(quote_limit(eol + 1, depth));
                blank(&mut out, i, stop);
                i = stop;
                continue;
            }

            for (pat, end) in [(&DECL_PI, "?>"), (&DECL_CDATA, "]]>"), (&DECL_BANG, ">")] {
                if pat.is_match(rel) {
                    let close = t.find(end, i + 2, n);
                    let stop = close.map_or(n, |c| t.line_end(c + end.len()));
                    let stop = stop.min(quote_limit(eol + 1, depth));
                    blank(&mut out, i, stop);
                    i = stop;
                    continue 'scan;
                }
            }

            if let Some((closing, tag)) = html {
                let quote_stop = quote_limit(eol + 1, depth);
                let stop = if LITERAL.contains(&tag.as_str()) && !closing {
                    let closer: Vec<char> = format!("</{tag}>").chars().collect();
                    let lower = lowered
                        .get_or_insert_with(|| t.c.iter().flat_map(|c| c.to_lowercase()).collect());
                    let idx = (i..lower.len().saturating_sub(closer.len() - 1))
                        .find(|&k| lower[k..k + closer.len()] == closer[..]);
                    idx.map_or(n, |x| t.line_end(x + closer.len()))
                } else {
                    // A raw block ends at a blank line, spaces and tabs
                    // included.
                    t.search(&BLANK_LINE, i).map_or(n, |m| m.start())
                };
                let stop = stop.min(quote_stop);
                blank(&mut out, i, stop);
                i = stop;
                continue;
            }
        }

        if t.starts_with(i, "<!--") {
            // `<!-->` and `<!--->` are complete comments. Any other inline
            // comment needs its closer before the paragraph ends.
            if let Some(short) = ["<!--->", "<!-->"]
                .into_iter()
                .find(|s| t.starts_with(i, s))
            {
                blank(&mut out, i, i + short.len());
                i += short.len();
                continue;
            }
            let limit = t.search(&BLANK_LINE, i).map_or(n, |m| m.start());
            let Some(close) = t.find("-->", i + 4, limit) else {
                i += 1;
                continue;
            };
            blank(&mut out, i, close + 3);
            i = close + 3;
            continue;
        }

        if t.c[i] == '\\' {
            // A backslash run, resolved by parity: only an odd run leaves a
            // live escape for the character after it.
            let mut j = i;
            while j < n && t.c[j] == '\\' {
                j += 1;
            }
            if (j - i) % 2 == 1 && j < n && ESCAPABLE.contains(&t.c[j]) {
                blank(&mut out, j, j + 1);
                j += 1;
            }
            i = j;
            continue;
        }

        if t.c[i] == '`' {
            if escaped(t, i) {
                i += 1;
                continue;
            }
            let start = i;
            while i < n && t.c[i] == '`' {
                i += 1;
            }
            let run = i - start;
            let Some((closer, end)) = code_span_at(t, start) else {
                continue;
            };
            // A code span renders, so one with visible content leaves a mark
            // that keeps a link around it from reading as empty.
            blank(&mut out, start, end);
            if !py_strip(&t.slice(start + run, closer)).is_empty() {
                out[start] = MARK;
            }
            i = end;
            continue;
        }

        if t.c[i] == '!' && t.is(i + 1, '[') && !escaped(t, i) {
            // An image is `![alt](target)`, `![alt][ref]` or `![alt][]`; a
            // link inside its label is alt text. The bare shortcut `![alt]`
            // is left alone, and a reference image counts only when its label
            // resolves.
            if let Some(label) = balanced(t, i + 1)
                && label < n
                && matches!(t.c[label], '(' | '[')
            {
                let mut end = if t.c[label] == '(' {
                    tail_at(t, label).map(|(e, _)| e)
                } else {
                    balanced(t, label)
                };
                if let Some(e) = end
                    && t.c[label] == '['
                {
                    let reference = t.slice(label + 1, e - 1);
                    let key = if py_strip(&reference).is_empty() {
                        label_key(&t.slice(i + 2, label - 1))
                    } else {
                        label_key(&reference)
                    };
                    if key.is_none_or(|k| !resolved.contains(&k)) {
                        end = None;
                    }
                }
                if let Some(e) = end {
                    blank(&mut out, i, e);
                    if out[i] != '\n' {
                        out[i] = MARK;
                    }
                    i = e;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        if t.c[i] == '<' {
            // `<…>` after `](` or `]:` is a link destination, not a tag.
            let back = t.c[..i].iter().rposition(|&c| c != ' ' && c != '\t');
            if let Some(back) = back
                && back >= 1
                && matches!(t.c[back], '(' | ':')
                && t.c[back - 1] == ']'
                && let Some(close) = t.find_char('>', i)
                && close < t.line_end(i)
            {
                i = close + 1;
                continue;
            }
            if let Some(auto) = t.match_at(&AUTOLINK, i) {
                // Blanked: nothing inside an autolink's URI is a reference.
                blank(&mut out, i, auto.end());
                i = auto.end();
                continue;
            }
            if let Some(end) = inline_tag_at(t.view().as_bytes(), i) {
                blank(&mut out, i, end);
                // A raw `<img>` renders, so a link around one has content.
                if is_img_tag(t, i) {
                    out[i] = MARK;
                }
                i = end;
                continue;
            }
            let limit = t.search(&BLANK_LINE, i).map_or(n, |m| m.start());
            if let Some(end) = inline_raw_at(t, i, limit) {
                blank(&mut out, i, end);
                i = end;
                continue;
            }
        }

        i += 1;
    }

    Text::from_chars(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(s: &str) -> String {
        readable(&Text::new(s), None).c.iter().collect()
    }

    #[test]
    fn inline_tags_follow_the_grammar() {
        let tag = |s: &str| inline_tag_at(s.as_bytes(), 0);
        assert_eq!(tag("<span>x"), Some(6));
        assert_eq!(tag("<span title=\"a > b\">"), Some(20));
        assert_eq!(tag("<span\ntitle='x'>"), Some(16));
        assert_eq!(tag("<span\n\ntitle='x'>"), None, "not across a blank line");
        assert_eq!(tag("<span title=\"\n\nx\">"), None);
        assert_eq!(tag("<span = [x]>"), None);
        assert_eq!(tag("</div >"), Some(7));
        assert_eq!(tag("<a b=c/>"), Some(8));
        assert_eq!(tag("<br/>"), Some(5));
    }

    #[test]
    fn html_openers_end_their_name_properly() {
        assert_eq!(html_open("<div>"), Some((false, "div".into())));
        assert_eq!(html_open("   </PRE>"), Some((true, "pre".into())));
        assert_eq!(html_open("<div.class>"), None);
        assert_eq!(html_open("<br/>"), Some((false, "br".into())));
    }

    #[test]
    fn blocks_are_blanked_space_for_space() {
        assert_eq!(read("a\n```\n[x](y)\n```\nb"), "a\n   \n      \n   \nb");
        assert_eq!(read("<!-- [x](y) -->\nz"), "               \nz");
        assert_eq!(read("x `[a](b)` y"), "x \u{fffc}        y");
        assert_eq!(
            read("![alt [x](a.md)](p.png) z"),
            format!("\u{fffc}{} z", " ".repeat(22))
        );
        assert_eq!(read("\\[a](b)"), "\\ a](b)");
    }

    #[test]
    fn indented_code_cannot_interrupt_a_paragraph() {
        assert_eq!(read("para\n    [x](y)"), "para\n    [x](y)");
        assert_eq!(read("# h\n    [x](y)"), "# h\n          ");
    }

    #[test]
    fn img_matches_as_python_ignorecase_does() {
        assert!(is_img_tag(&Text::new("<IMG src>"), 0));
        assert!(is_img_tag(&Text::new("<\u{130}mg/>"), 0));
        assert!(!is_img_tag(&Text::new("<imgx>"), 0));
    }

    #[test]
    fn columns_and_offsets_agree() {
        assert_eq!(byte_at_col("-   \tx", 5), None);
        assert_eq!(byte_at_col("-     x", 2), Some(2));
        assert_eq!(list_content_col("-     x"), Some(2));
        assert_eq!(list_content_col("10.  x"), Some(5));
        assert_eq!(list_content_col("* * *"), None);
    }
}
