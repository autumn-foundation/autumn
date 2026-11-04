//! Links, reference links and reference definitions, read from a
//! [`Text`] at character offsets.
//!
//! This is not a `CommonMark` parser. It reads the shapes the guide index and
//! the README use, and where a shape could be read two ways it takes the one
//! that fails the gate rather than the one that passes it: a row the reader
//! cannot use is reported as a page listed nowhere.

use std::collections::{HashMap, HashSet};

use unicase::UniCase;
use unicode_general_category::{GeneralCategory, get_general_category};

use super::entities;
use super::text::{Hit, Text, digit, line_ws, next_tab_stop, not_ws, ws, ws_chars};
use crate::docs::re::{is_py_space, pattern, py_strip};

/// Folded reference labels.
pub type Labels = HashSet<String>;

/// Every `[` offset mapped to the offset of its `]`.
pub type Pairs = Vec<Option<usize>>;

/// Whitespace inside a link: optional, and never across a blank line.
macro_rules! link_ws {
    () => {
        r"[ \t]*(?:\n[ \t]*)?"
    };
}

/// Required whitespace inside a link, never across a blank line.
macro_rules! link_ws1 {
    () => {
        r"(?:[ \t]+|[ \t]*\n[ \t]*)"
    };
}

/// A link title: `"…"`, `'…'` or `(…)`. Its delimiter may be escaped, and it
/// may cross a line ending but not a blank line.
///
/// A body newline is allowed only when the next line is not blank. That is a
/// lookahead in Python (`\n(?!\s*\n)`); here it is spelled as "a newline,
/// the next line's leading whitespace, then a body character", with an
/// optional final newline before the closer, which accepts exactly the same
/// titles.
macro_rules! title {
    () => {
        concat!(
            r#""(?:\\[^\n]|[^"\\\n]|\n"#,
            line_ws!(),
            r#"*(?:\\[^\n]|[^"\\"#,
            ws_chars!(),
            r#"]))*(?:\n"#,
            line_ws!(),
            r#"*)?""#,
            r"|'(?:\\[^\n]|[^'\\\n]|\n",
            line_ws!(),
            r"*(?:\\[^\n]|[^'\\",
            ws_chars!(),
            r"]))*(?:\n",
            line_ws!(),
            r"*)?'",
            r"|\((?:\\[^\n]|[^()\\\n]|\n",
            line_ws!(),
            r"*(?:\\[^\n]|[^()\\",
            ws_chars!(),
            r"]))*(?:\n",
            line_ws!(),
            r"*)?\)",
        )
    };
}

/// An angle-bracketed destination.
macro_rules! angle {
    () => {
        r"<[^<>\n]*>"
    };
}

/// A list marker.
macro_rules! marker {
    () => {
        concat!(r"(?:[-*+]|", digit!(), r"{1,9}[.)])")
    };
}

/// The label, destination and optional title of a reference definition.
macro_rules! defn_body {
    () => {
        concat!(
            r"\[((?:\\.|[^\[\]\n])+)\]:[ \t]*(?:\n[ \t]*)?",
            "(",
            angle!(),
            "|",
            not_ws!(),
            "+)",
            "(?:",
            link_ws1!(),
            "(?:",
            title!(),
            "))?[ \t]*$",
        )
    };
}

pattern! { static OPEN = concat!(r"\A\(", link_ws!()); }
pattern! { static ANGLE_DEST = concat!(r"\A", angle!()); }
pattern! { static CLOSE = concat!(r"\A(?:", link_ws1!(), "(?:", title!(), "))?", link_ws!(), r"\)"); }

pattern! {
    /// A reference label after link text. Like a title, it may cross a line
    /// ending but not a blank line.
    static LABEL = concat!(
        r"\A\[((?:\\.|[^\[\]\n]|\n", line_ws!(), r"*(?:\\.|[^\[\]", ws_chars!(), r"]))*(?:\n", line_ws!(), r"*)?)\]",
    );
}

pattern! {
    /// A reference definition at the start of a line, behind any block-quote
    /// and list-item prefixes, which compose in any order and depth.
    ///
    /// A definition written as a list item's continuation content (a marker
    /// on an earlier line, then the definition indented under it) is not
    /// recognised. Telling it apart from indented code needs the open list's
    /// content column, which this does not track.
    pub(super) static DEFN = concat!(
        r"(?m)^(?:[ \t]{0,3}(?:>[ \t]?)+|[ \t]{0,3}", marker!(), r"[ \t]{1,4})*",
        r"[ \t]{0,3}",
        defn_body!(),
    );
}

pattern! {
    /// The same definition, unanchored, for asking whether one starts at a
    /// given offset (inside a list item, say).
    static DEFN_AT = concat!(r"(?m)\A", defn_body!());
}

pattern! {
    /// A list item interrupts a paragraph only when it is non-empty and,
    /// when ordered, starts at 1.
    pub(super) static INTERRUPT_LIST = concat!(r"\A {0,3}(?:[-*+]|1[.)])[ \t]+", not_ws!());
}

pattern! { pub(super) static ATX = r"\A {0,3}#{1,6}(?:[ \t]|$)"; }
pattern! { pub(super) static ATX_H1 = r"\A {0,3}#(?:[ \t].*)?$"; }
pattern! { pub(super) static ATX_H2 = r"\A {0,3}##(?:[ \t](.*))?$"; }
pattern! { pub(super) static SETEXT_H1 = r"\A {0,3}=+[ \t]*$"; }
pattern! { pub(super) static FENCE = r"\A {0,3}(`{3,}|~{3,})(.*)$"; }
pattern! { static BLOCKQUOTE = r"\A {0,3}(?:>[ \t]?)+"; }
pattern! { pub(super) static THEMATIC = r"\A {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$"; }
pattern! { pub(super) static SETEXT = r"\A {0,3}(?:=+|-+)[ \t]*$"; }
pattern! { pub(super) static LIST_ITEM = concat!(r"\A(", ws!(), "*)", marker!(), ws!(), "+"); }
pattern! { pub(super) static LIST_COL = concat!(r"\A( *)(", marker!(), ")( +)"); }
pattern! { static MARKER = concat!(r"\A", marker!()); }
pattern! { static PFX_QUOTE = r"\A[ \t]{0,3}(?:>[ \t]?)+"; }
pattern! { static PFX_LIST = concat!(r"\A[ \t]{0,3}(", marker!(), r")([ \t]{1,4})"); }
pattern! { static URI_SCHEME = r"\A[A-Za-z][A-Za-z0-9+.-]*:"; }
pattern! {
    /// A backslash escape, which exists only before ASCII punctuation.
    static ESCAPED_PUNCT = r"\\([!-/:-@\[-`{-~])";
}
pattern! { static ESCAPED_PUNCT_AT = r"\A\\[!-/:-@\[-`{-~]"; }
pattern! {
    /// A character reference. `CommonMark` decodes only the
    /// semicolon-terminated spellings.
    static CHAR_REF = r"&(?:#[0-9]{1,7}|#[xX][0-9A-Fa-f]{1,6}|[A-Za-z][A-Za-z0-9]*);";
}

/// `CommonMark` caps a reference label at 999 characters.
const LABEL_LIMIT: usize = 999;

/// True when `line` opens a fenced code block. A backtick fence's info
/// string may not contain a backtick.
pub fn opens_fence(line: &str) -> bool {
    FENCE
        .captures(line)
        .is_some_and(|m| !(m[1].starts_with('`') && m[2].contains('`')))
}

/// True when a Setext underline under `above` forms a heading: it needs a
/// paragraph line there.
pub fn setext_context(above: &str) -> bool {
    !py_strip(above).is_empty()
        && !ATX.is_match(above)
        && !THEMATIC.is_match(above)
        && !LIST_ITEM.is_match(above)
}

/// How many block-quote markers open `line`.
pub fn quote_depth(line: &str) -> usize {
    BLOCKQUOTE
        .find(line)
        .map_or(0, |m| m.as_str().matches('>').count())
}

/// `line` with its block-quote prefix removed.
pub fn unquote(line: &str) -> &str {
    BLOCKQUOTE.find(line).map_or(line, |m| &line[m.end()..])
}

/// True when `line` starts a new block instead of continuing `above`.
fn interrupts(line: &str, above: &str) -> bool {
    if py_strip(line).is_empty()
        || ATX.is_match(line)
        || THEMATIC.is_match(line)
        || opens_fence(line)
        || BLOCKQUOTE.is_match(line)
        || INTERRUPT_LIST.is_match(line)
    {
        return true;
    }
    if SETEXT.is_match(line) {
        return setext_context(above);
    }
    false
}

/// Every `[` mapped to its matching `]`, in one pass.
///
/// A backslash never swallows a line ending, and a block boundary (a blank
/// line, a heading, a fence, a quote, a new list item) empties the stack:
/// link text cannot span one.
pub fn bracket_pairs(t: &Text) -> Pairs {
    let n = t.len();
    let mut pairs = vec![None; n];
    let mut stack = Vec::new();
    let (mut j, mut line_start) = (0, 0);
    while j < n {
        let ch = t.c[j];
        if ch == '\\' {
            j += if t.is(j + 1, '\n') { 1 } else { 2 };
            continue;
        }
        if ch == '\n' {
            let next_end = t.find_char('\n', j + 1).unwrap_or(n);
            if interrupts(t.ascii(j + 1, next_end), t.ascii(line_start, j)) {
                stack.clear();
            }
            line_start = j + 1;
        }
        if ch == '[' {
            stack.push(j);
        } else if ch == ']'
            && let Some(open) = stack.pop()
        {
            pairs[open] = Some(j);
        }
        j += 1;
    }
    pairs
}

/// `(end, destination)` for a bare destination at `pos`: it stops at
/// whitespace or at a `)` outside a balanced pair.
pub fn dest_at(t: &Text, pos: usize) -> Option<(usize, String)> {
    let n = t.len();
    let (mut depth, mut j) = (0usize, pos);
    while j < n {
        let ch = t.c[j];
        if ch == '\\' && j + 1 < n {
            j += if t.match_at(&ESCAPED_PUNCT_AT, j).is_some() {
                2
            } else {
                1
            };
            continue;
        }
        if matches!(ch, ' ' | '\t' | '\n') {
            break;
        }
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            if depth == 0 {
                break;
            }
            depth -= 1;
        }
        j += 1;
    }
    (depth == 0).then(|| (j, t.slice(pos, j)))
}

/// `(end, destination)` for the `(dest "title")` after a link's text.
pub fn tail_at(t: &Text, pos: usize) -> Option<(usize, String)> {
    let open = t.match_at(&OPEN, pos)?;
    let (after, dest) = match t.match_at(&ANGLE_DEST, open.end()) {
        Some(angle) => (angle.end(), t.slice(angle.start(), angle.end())),
        None => dest_at(t, open.end())?,
    };
    let close = t.match_at(&CLOSE, after)?;
    Some((close.end(), dest))
}

/// True when a link's text renders something and holds no link itself.
///
/// Character references decode after matched emphasis comes off, because
/// flanking is decided on the source. A link inside the text deactivates
/// the outer one; an inline link always does, and a reference does when its
/// label is in `resolved`.
fn text_renders(t: &Text, pos: usize, close: usize, pairs: &Pairs, resolved: &Labels) -> bool {
    let inner = t.slice(pos + 1, close - 1);
    if py_strip(&visible(&decode_char_refs(&strip_emphasis(&inner)))).is_empty() {
        return false;
    }
    for q in pos + 1..close - 1 {
        if t.c[q] != '[' || (q > 0 && t.c[q - 1] == '!') {
            continue;
        }
        let Some(inner_close) = pairs[q] else {
            continue;
        };
        if inner_close >= close - 1 {
            continue;
        }
        if tail_at(t, inner_close + 1).is_some() {
            return false;
        }
        if !resolved.is_empty()
            && let Some((end, label)) = ref_at(t, q, pairs, &Labels::new())
            && end < close
            && label_key(&label).is_some_and(|key| resolved.contains(&key))
        {
            return false;
        }
    }
    true
}

/// `(end, destination)` for an inline link at `pos`.
pub fn link_at(t: &Text, pos: usize, pairs: &Pairs, resolved: &Labels) -> Option<(usize, String)> {
    if pos > 0 && t.c[pos - 1] == '!' {
        return None;
    }
    let close = pairs[pos]? + 1;
    let tail = tail_at(t, close)?;
    text_renders(t, pos, close, pairs, resolved).then_some(tail)
}

/// `(end, label)` for a full, collapsed or shortcut reference link at `pos`.
pub fn ref_at(t: &Text, pos: usize, pairs: &Pairs, resolved: &Labels) -> Option<(usize, String)> {
    if pos > 0 && t.c[pos - 1] == '!' {
        return None;
    }
    let close = pairs[pos]? + 1;
    if !text_renders(t, pos, close, pairs, resolved) {
        return None;
    }
    let inner = t.slice(pos + 1, close - 1);
    if let Some(m) = t.match_at(&LABEL, close) {
        // `[a][b](c.md)` is an inline link at `[b]`, which deactivates this
        // one. A colon after a full or collapsed reference is punctuation.
        if t.is(m.end(), '(') {
            return None;
        }
        let label = m
            .group(1)
            .filter(|(a, b)| a < b)
            .map_or(inner, |(a, b)| t.slice(a, b));
        return Some((m.end(), label));
    }
    if t.is(close, '[') || t.is(close, '(') {
        return None;
    }
    // A colon after a shortcut reference disqualifies it only when a real
    // definition follows: `- [Alpha]: the page` is a link and punctuation.
    if t.is(close, ':') && t.match_at(&DEFN_AT, pos).is_some() {
        return None;
    }
    Some((close, inner))
}

type At = fn(&Text, usize, &Pairs, &Labels) -> Option<(usize, String)>;

/// Every non-overlapping `at` hit in `t`, left to right, as
/// `(start, end, destination or label)`.
fn scan(t: &Text, at: At, resolved: &Labels) -> Vec<(usize, usize, String)> {
    let pairs = bracket_pairs(t);
    let mut out = Vec::new();
    let (mut i, n) = (0, t.len());
    while i < n {
        if t.c[i] == '['
            && let Some((end, found)) = at(t, i, &pairs, resolved)
        {
            out.push((i, end, found));
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// `(start, end, destination)` for every inline link in `t`.
pub fn link_spans(t: &Text, resolved: &Labels) -> Vec<(usize, usize, String)> {
    scan(t, link_at, resolved)
}

/// Every reference label used in `t`. `resolved` is what tells an outer
/// reference that its text already holds a link.
pub fn ref_labels(t: &Text, resolved: &Labels) -> Vec<String> {
    scan(t, ref_at, resolved)
        .into_iter()
        .map(|(_, _, label)| label)
        .collect()
}

/// Where a top-level list item's content starts on `line`, or `None`.
///
/// Padding is measured in columns from the end of the marker and must be one
/// to four of them; a fifth starts indented code inside the item.
pub fn row_at(line: &str) -> Option<usize> {
    let m = MARKER.find(line)?;
    let bytes = line.as_bytes();
    let (base, mut col, mut i) = (m.end(), m.end(), m.end());
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        col = if bytes[i] == b' ' {
            col + 1
        } else {
            next_tab_stop(col)
        };
        i += 1;
    }
    ((1..=4).contains(&(col - base)) && i < bytes.len()).then_some(i)
}

/// Python's `html.unescape` applied to one [`CHAR_REF`] match.
fn unescape_ref(s: &str) -> String {
    let body = &s[1..];
    if let Some(num) = body.strip_prefix('#') {
        let digits = num.trim_end_matches(';');
        let code = digits
            .strip_prefix(['x', 'X'])
            .map_or_else(|| digits.parse(), |hex| u32::from_str_radix(hex, 16));
        let Ok(code) = code else {
            return s.to_owned();
        };
        if let Ok(i) = entities::REMAPPED.binary_search_by_key(&code, |&(k, _)| k) {
            return entities::REMAPPED[i].1.to_string();
        }
        if (0xd800..=0xdfff).contains(&code) || code > 0x10_ffff {
            return '\u{fffd}'.to_string();
        }
        if entities::DROPPED
            .iter()
            .any(|&(a, b)| (a..=b).contains(&code))
        {
            return String::new();
        }
        return char::from_u32(code).map_or_else(String::new, String::from);
    }
    // `html.unescape` reads at most 32 name characters, then an optional `;`.
    let name = body.trim_end_matches(';');
    let (head, rest) = if name.len() <= 32 {
        (body, "")
    } else {
        (&body[..32], &body[32..])
    };
    named(head) + rest
}

/// Python's named-reference fallback: the whole name, else its longest
/// prefix of two or more characters that is a legacy name, else literal.
fn named(s: &str) -> String {
    let lookup = |k: &str| {
        entities::NAMED
            .binary_search_by(|&(name, _)| name.cmp(k))
            .ok()
            .map(|i| entities::NAMED[i].1)
    };
    if let Some(v) = lookup(s) {
        return v.to_owned();
    }
    for x in (2..s.len()).rev() {
        if let Some(v) = lookup(&s[..x]) {
            return format!("{v}{}", &s[x..]);
        }
    }
    format!("&{s}")
}

/// Decode the semicolon-terminated character references in `s`.
pub fn decode_char_refs(s: &str) -> String {
    CHAR_REF
        .replace_all(s, |c: &regex::Captures<'_>| unescape_ref(&c[0]))
        .into_owned()
}

/// `s` without its Unicode format characters (category `Cf`), which render
/// nothing a reader can see or aim at.
pub fn visible(s: &str) -> String {
    s.chars()
        .filter(|&c| get_general_category(c) != GeneralCategory::Format)
        .collect()
}

/// `s` with matched outer emphasis delimiters (`*`, `_`, `~`) removed.
///
/// Only outer pairs come off, only when the runs are distinct, and only
/// when neither sits against whitespace on its inner side. Unpaired
/// delimiters are literal text.
pub fn strip_emphasis(s: &str) -> String {
    const WS: &[char] = &[' ', '\t', '\n', '\r', '\x0c', '\x0b'];
    let mut s: Vec<char> = s.chars().collect();
    while s.len() >= 2 {
        let ch = s[0];
        if !matches!(ch, '*' | '_' | '~') || s[s.len() - 1] != ch {
            break;
        }
        let lead = s.iter().take_while(|&&c| c == ch).count();
        let tail = s.iter().rev().take_while(|&&c| c == ch).count();
        if lead + tail >= s.len() {
            break;
        }
        if WS.contains(&s[lead]) || WS.contains(&s[s.len() - tail - 1]) {
            break;
        }
        s = s[1..s.len() - 1].to_vec();
    }
    s.into_iter().collect()
}

/// A reference label's key: whitespace collapsed, full Unicode case fold.
/// `None` for an overlong label or one with nothing but whitespace.
pub fn label_key(raw: &str) -> Option<String> {
    if raw.chars().count() > LABEL_LIMIT {
        return None;
    }
    let collapsed = raw
        .split(is_py_space)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let key = UniCase::new(collapsed).to_folded_case();
    (!key.is_empty()).then_some(key)
}

/// `t` with every character of `[a, b)` in `spans` blanked, newlines kept.
fn blank_spans(t: &Text, spans: impl IntoIterator<Item = (usize, usize)>) -> Text {
    let mut out = t.c.clone();
    for (a, b) in spans {
        blank(&mut out, a, b);
    }
    Text::from_chars(out)
}

/// Blank `[a, b)` of `out`, keeping newlines.
pub fn blank(out: &mut [char], a: usize, b: usize) {
    let b = b.min(out.len());
    for c in out.iter_mut().take(b).skip(a) {
        if *c != '\n' {
            *c = ' ';
        }
    }
}

/// `t` with every inline link blanked: a later pass must not read a link's
/// title as markdown.
pub fn blank_links(t: &Text) -> Text {
    blank_spans(
        t,
        link_spans(t, &Labels::new())
            .into_iter()
            .map(|(a, b, _)| (a, b)),
    )
}

/// True when a definition's line prefix leaves it a definition: every list
/// marker in it must be followed by one to four columns of padding.
fn prefix_ok(prefix: &str) -> bool {
    let step = |col: usize, ch: char| {
        if ch == '\t' {
            next_tab_stop(col)
        } else {
            col + 1
        }
    };
    let (mut col, mut i) = (0, 0);
    while i < prefix.len() {
        let rest = &prefix[i..];
        if let Some(m) = PFX_QUOTE.find(rest) {
            col = rest[..m.end()].chars().fold(col, step);
            i += m.end();
            continue;
        }
        if let Some(c) = PFX_LIST.captures(rest) {
            let marker_end = c.get(1).map_or(0, |g| g.end());
            col = rest[..marker_end].chars().fold(col, step);
            let base = col;
            col = rest[marker_end..c[0].len()].chars().fold(col, step);
            if !(1..=4).contains(&(col - base)) {
                return false;
            }
            i += c[0].len();
            continue;
        }
        break;
    }
    true
}

/// A real reference definition: where it is, its key and its destination.
pub struct Defn {
    pub hit: Hit,
    pub key: String,
    pub dest: String,
}

/// True when `s` is a whole angle-bracketed destination.
pub fn is_angle(s: &str) -> bool {
    s.len() >= 2
        && s.starts_with('<')
        && s.ends_with('>')
        && !s[1..s.len() - 1].contains(['<', '>', '\n'])
}

/// Every real reference definition in `body`.
///
/// `origin` is the text before links were masked, which block starts are
/// read from: masking turns a line holding only a link into spaces, which
/// would otherwise read as a blank line. A definition must start a block,
/// have a foldable label within the cap, and have a destination that parses.
///
/// A bare destination with an unbalanced parenthesis is rejected, as
/// markdown-it does. cmark-gfm accepts it; the docs gates agree with each
/// other on the stricter reading.
pub fn defn_entries(body: &Text, origin: Option<&Text>) -> Vec<Defn> {
    let blocks = origin.unwrap_or(body);
    let mut out = Vec::new();
    for hit in body.find_iter(&DEFN) {
        let (Some(label), Some(dest)) = (hit.group(1), hit.group(2)) else {
            continue;
        };
        if !starts_block(blocks, hit.start()) || !prefix_ok(body.ascii(hit.start(), label.0 - 1)) {
            continue;
        }
        let Some(key) = label_key(&body.slice(label.0, label.1)) else {
            continue;
        };
        let dest = body.slice(dest.0, dest.1);
        if dest.starts_with('<') {
            if !is_angle(&dest) {
                continue;
            }
        } else {
            let d = Text::new(&dest);
            if dest_at(&d, 0).is_none_or(|(end, _)| end != d.len()) {
                continue;
            }
        }
        out.push(Defn { hit, key, dest });
    }
    out
}

/// True when `pos` begins a block rather than continuing a paragraph.
///
/// A new list item or a deeper block quote on the definition's own line
/// always starts one. Otherwise the line above decides, and a run of
/// definition lines is walked back to its head.
fn starts_block(t: &Text, pos: usize) -> bool {
    let own = t.ascii(pos, t.line_end(pos));
    if INTERRUPT_LIST.is_match(own) {
        return true;
    }
    if BLOCKQUOTE.is_match(own) {
        let prev_line = t.rfind_char('\n', 0, pos).map_or("", |above| {
            let start = t.rfind_char('\n', 0, above).map_or(0, |x| x + 1);
            t.ascii(start, above)
        });
        if quote_depth(own) > quote_depth(prev_line) {
            return true;
        }
    }
    let mut pos = pos;
    loop {
        if pos == 0 {
            return true;
        }
        let Some(start) = t.rfind_char('\n', 0, pos) else {
            return false;
        };
        let prev_start = t.rfind_char('\n', 0, start).map_or(0, |x| x + 1);
        let prev = t.ascii(prev_start, start);
        if py_strip(prev).is_empty()
            || ATX.is_match(prev)
            || opens_fence(prev)
            || THEMATIC.is_match(prev)
        {
            return true;
        }
        if SETEXT.is_match(prev) {
            if prev_start == 0 {
                return false;
            }
            let above_start = t.rfind_char('\n', 0, prev_start - 1).map_or(0, |x| x + 1);
            return setext_context(t.ascii(above_start, prev_start - 1));
        }
        if !DEFN.is_match(prev) {
            return false;
        }
        pos = prev_start;
    }
}

/// `t` with every real definition blanked: definitions vanish from the
/// rendered page, so nothing inside one is visible or clickable.
pub fn blank_defns(t: &Text) -> Text {
    blank_spans(
        t,
        defn_entries(t, None)
            .into_iter()
            .map(|d| (d.hit.start(), d.hit.end())),
    )
}

/// Every definition in `t`, folded label to destination; the first
/// definition of a label wins. Inline links are blanked first, so a
/// definition-shaped line inside a link title does not count.
pub fn definitions(t: &Text) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for d in defn_entries(&blank_links(t), Some(t)) {
        out.entry(d.key).or_insert(d.dest);
    }
    out
}

/// Python's `re.sub(ESCAPED_PUNCT, r"\1", s)`.
pub fn unescape_punct(s: &str) -> String {
    ESCAPED_PUNCT.replace_all(s, "$1").into_owned()
}

/// True when `s` begins with a URI scheme.
pub fn has_scheme(s: &str) -> bool {
    URI_SCHEME.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(s: &str) -> Vec<String> {
        link_spans(&Text::new(s), &Labels::new())
            .into_iter()
            .map(|(_, _, d)| d)
            .collect()
    }

    #[test]
    fn character_references_decode_as_python_does() {
        assert_eq!(decode_char_refs("alpha&#46;md"), "alpha.md");
        assert_eq!(
            decode_char_refs("&#x2e;&amp;&ampx;&notit;"),
            ".&&x;\u{ac}it;"
        );
        assert_eq!(
            decode_char_refs("&#46md"),
            "&#46md",
            "no semicolon, no decode"
        );
        assert_eq!(
            decode_char_refs("&#0;&#128;&#1;&#xD800;"),
            "\u{fffd}\u{20ac}\u{fffd}"
        );
        assert_eq!(decode_char_refs("&bogus;"), "&bogus;");
        let long = format!("&{};", "a".repeat(40));
        assert_eq!(
            decode_char_refs(&long),
            long,
            "only 32 name characters are read"
        );
    }

    #[test]
    fn format_characters_are_invisible() {
        assert_eq!(visible("\u{200b}a\u{feff}\u{ad}"), "a");
        assert_eq!(visible("\u{fffc}"), "\u{fffc}");
    }

    #[test]
    fn emphasis_comes_off_only_when_it_pairs() {
        assert_eq!(strip_emphasis("*&#32;*"), "&#32;");
        assert_eq!(strip_emphasis("* *"), "* *");
        assert_eq!(strip_emphasis("***"), "***");
        assert_eq!(strip_emphasis("**a**"), "a");
        assert_eq!(strip_emphasis("~~a~~"), "a");
    }

    #[test]
    fn labels_fold_and_collapse() {
        assert_eq!(
            label_key("Guide   Catalog").as_deref(),
            Some("guide catalog")
        );
        assert_eq!(label_key("\u{1e9e}").as_deref(), Some("ss"));
        assert_eq!(label_key(" \t"), None);
        assert_eq!(label_key(&"a".repeat(1000)), None);
    }

    #[test]
    fn inline_links_are_read_with_their_tails() {
        assert_eq!(links("[A](alpha.md)"), ["alpha.md"]);
        assert_eq!(links("[A](<my file.md> \"t\")"), ["<my file.md>"]);
        assert_eq!(links("[A](a((b)).md)"), ["a((b)).md"]);
        assert_eq!(
            links("[A](a.md \"t\n\n\")"),
            Vec::<String>::new(),
            "a title cannot cross a blank line"
        );
        assert_eq!(links("[A](a.md \"t\nu\")"), ["a.md"]);
        assert_eq!(links("[A](a.md \"t\n  \")"), ["a.md"]);
        assert_eq!(links("![A](a.png)"), Vec::<String>::new());
        assert_eq!(links("[](a.md)"), Vec::<String>::new());
        assert_eq!(links("[o [B](b.md)](a.md)"), ["b.md"]);
    }

    #[test]
    fn rows_are_measured_in_columns() {
        assert_eq!(row_at("-\t[A](a.md)"), Some(2));
        assert_eq!(row_at("10.\t[A](a.md)"), Some(4));
        assert_eq!(row_at("-   \t[A](a.md)"), None);
        assert_eq!(row_at("10. \t[A](a.md)"), None);
        assert_eq!(row_at("-     [A]"), None);
        assert_eq!(row_at("-"), None);
    }

    #[test]
    fn definitions_need_a_block_start() {
        let t = Text::new("Some prose\n[a]: x.md\n\n[b]: y.md\n> [c]: z.md\n- [d]: w.md\n");
        let defs = definitions(&t);
        assert!(!defs.contains_key("a"));
        assert_eq!(defs.get("b").map(String::as_str), Some("y.md"));
        assert_eq!(defs.get("c").map(String::as_str), Some("z.md"));
        assert_eq!(defs.get("d").map(String::as_str), Some("w.md"));
    }

    #[test]
    fn a_list_marker_with_too_much_padding_defines_nothing() {
        assert!(prefix_ok("- "));
        assert!(prefix_ok("> - "));
        assert!(!prefix_ok("-\t\t- "));
    }
}
