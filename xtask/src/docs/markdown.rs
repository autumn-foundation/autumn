//! The markdown model the docs gates share: which text is live prose, which
//! headings carry which anchors, and which link destinations a page holds.
//!
//! This is a port of the Python that lived in `scripts/check-docs-links.sh`,
//! kept close to it on purpose: each rule below exists because a real page,
//! or a self-test written for one, needed it. It is not a full `CommonMark`
//! parser. It is the subset that decides what a reader can click.
//!
//! The per-line shapes (block quote, fence, list item, heading) are matched
//! by hand rather than by regex: they run on every line of every page, twice,
//! and a regex there made the gate ten times slower in a debug build. Each
//! matcher's doc comment gives the Python pattern it replaces, and the tests
//! check the two agree on every line of this repository's corpus.

use std::collections::{HashMap, HashSet};

use super::re::{
    captures_unescaped, group, is_py_space, pattern, py_splitlines, py_strip, replace_all,
};

// Link destinations. CommonMark allows a bare destination with one level of
// balanced parentheses (`guide(v2).md`) or an angle-bracket one
// (`<missing file.md>`, the only way to write a space). Exactly one of the
// two capture groups matches per link.
macro_rules! bare {
    () => {
        r"(?:[^()\s]|\([^()\s]*\))+"
    };
}
// A title may be delimited by `"`, `'` or `()`, with escapes inside. A link
// whose title the pattern cannot parse is skipped rather than checked, so
// all three are accepted.
macro_rules! title {
    () => {
        r#"(?:\s+(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^()\\]|\\.)*\)))?"#
    };
}
macro_rules! dest {
    () => {
        concat!(r"\(\s*(?:<([^<>]*)>|(", bare!(), r"))", title!(), r"\s*\)")
    };
}
// A label character: anything but a bracket or backslash, or an escape.
// `[closing \]](page.md)` keeps its destination.
macro_rules! flat {
    () => {
        r"(?:[^\[\]\\]|\\.)"
    };
}

pattern! {
    /// `[label](dest)`. Matched through [`captures_unescaped`]: the opening
    /// bracket must be unescaped, as `\[x](y)` renders literal text.
    static INLINE = concat!(r"\[", flat!(), r"*\]", dest!());
}
pattern! {
    /// A label holding one nested label, for a linked image:
    /// `[![alt](img.png)](page.md)`. [`INLINE`] finds the image, this finds
    /// the page a reader clicks through to.
    static NESTED = concat!(r"\[(?:", flat!(), r"|\[", flat!(), r"*\])*\]", dest!());
}
pattern! {
    /// A reference definition: `[label]: dest`.
    static REFDEF = concat!(r"(?m)^ {0,3}\[", flat!(), r"+\]:\s*(?:<([^<>]*)>|(\S+))");
}
pattern! {
    /// Markdown drops the backslash from escaped ASCII punctuation in a
    /// destination, so `guide\(v2\).md` addresses `guide(v2).md`.
    static ESCAPED_PUNCT = r"\\([!-/:-@\[-`{-~])";
}
pattern! {
    /// A rustdoc intra-doc path: two or more `::`-joined identifiers.
    pub static RUSTDOC = r"^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+$";
}
pattern! {
    /// An HTML comment; markdown inside one renders nowhere.
    static HTML_COMMENT = r"(?s)<!--.*?-->";
}
pattern! {
    /// An explicit anchor: `<a id="x">`, `<a name='x'>`, `<a id=x>`.
    static HTML_ANCHOR =
        r#"<a\s[^>]*?\b(?:id|name)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#;
}
pattern! {
    static HEADING_MARKS = r"^#+\s*";
}
pattern! {
    static HEADING_LINK = r"\[([^\]]*)\]\([^)]*\)";
}

/// Width of an indent, counting tabs to `CommonMark`'s four-column stops.
#[must_use]
pub fn column(s: &str) -> usize {
    s.chars()
        .fold(0, |w, c| if c == '\t' { w + 4 - (w % 4) } else { w + 1 })
}

fn leading(s: &str, pred: impl Fn(u8) -> bool) -> usize {
    s.bytes().take_while(|&b| pred(b)).count()
}

const fn blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// `^ {0,3}(?:>[ \t]?)+`: the byte length of a line's block-quote prefix and
/// how many markers it holds. At most three spaces may precede it; more
/// would let `    > ```` ` (indented code that starts with `>`) lose both
/// its marker and its indent and become a column-zero fence.
#[must_use]
pub fn quote_prefix(line: &str) -> (usize, usize) {
    let b = line.as_bytes();
    let mut i = leading(line, |c| c == b' ').min(3);
    if b.get(i) != Some(&b'>') {
        return (0, 0);
    }
    let mut depth = 0;
    while b.get(i) == Some(&b'>') {
        depth += 1;
        i += 1;
        if b.get(i).is_some_and(|&c| blank(c)) {
            i += 1;
        }
    }
    (i, depth)
}

/// How many block-quote markers open `line` (0 when it is not quoted).
#[must_use]
pub fn quote_depth(line: &str) -> usize {
    quote_prefix(line).1
}

/// `line` with its leading block-quote markers removed.
#[must_use]
pub fn unquote_line(line: &str) -> &str {
    &line[quote_prefix(line).0..]
}

/// `^[ \t]*`, measured in columns.
fn indent_width(s: &str) -> usize {
    column(&s[..leading(s, blank)])
}

/// ``^([ \t]*)(`{3,}|~{3,})(.*)$``: a fence line's indent, marker run and
/// info string.
fn fence(line: &str) -> Option<(&str, &str, &str)> {
    let indent = leading(line, blank);
    let rest = &line[indent..];
    let mark = *rest.as_bytes().first()?;
    if mark != b'`' && mark != b'~' {
        return None;
    }
    let run = leading(rest, |c| c == mark);
    if run < 3 || rest[run..].contains('\n') {
        return None;
    }
    Some((&line[..indent], &rest[..run], &rest[run..]))
}

/// `^([ \t]*)(?:[-*+]|\d{1,9}[.)])([ \t]+)(?=\S)`: a list item's marker
/// indent and its whole prefix (indent, marker, spacing).
fn list_item(line: &str) -> Option<(&str, &str)> {
    let indent = leading(line, blank);
    let rest = &line[indent..];
    let marker = match rest.as_bytes().first()? {
        b'-' | b'*' | b'+' => 1,
        _ => {
            let digits = leading(rest, |c| c.is_ascii_digit());
            if !(1..=9).contains(&digits)
                || !matches!(rest.as_bytes().get(digits), Some(b'.' | b')'))
            {
                return None;
            }
            digits + 1
        }
    };
    let spacing = leading(&rest[marker..], blank);
    let after = rest[marker + spacing..].chars().next();
    if spacing == 0 || after.is_none_or(is_py_space) {
        return None;
    }
    Some((&line[..indent], &line[..indent + marker + spacing]))
}

/// `^ {0,3}(=+|-+)[ \t]*$`: a Setext underline.
fn is_setext(line: &str) -> bool {
    let spaces = leading(line, |c| c == b' ');
    if spaces > 3 {
        return false;
    }
    let rest = &line[spaces..];
    let Some(&mark) = rest.as_bytes().first() else {
        return false;
    };
    if mark != b'=' && mark != b'-' {
        return false;
    }
    let run = leading(rest, |c| c == mark);
    rest[run..].bytes().all(blank)
}

/// `^(?: {0,3}[#>|]| {0,3}(?:[-*+=]|\d{1,9}[.)])[ \t]|\s*$|\s{4,})`: a line
/// that cannot continue a paragraph a Setext underline promotes.
fn is_not_paragraph(line: &str) -> bool {
    if line.chars().all(is_py_space)
        || line.chars().take(4).filter(|&c| is_py_space(c)).count() == 4
    {
        return true;
    }
    let spaces = leading(line, |c| c == b' ');
    if spaces > 3 {
        return false;
    }
    let rest = &line.as_bytes()[spaces..];
    match rest.first() {
        Some(b'#' | b'>' | b'|') => true,
        Some(b'-' | b'*' | b'+' | b'=') => rest.get(1).is_some_and(|&c| blank(c)),
        _ => {
            let digits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
            (1..=9).contains(&digits)
                && matches!(rest.get(digits), Some(b'.' | b')'))
                && rest.get(digits + 1).is_some_and(|&c| blank(c))
        }
    }
}

/// `^ {0,3}(#{1,6})\s+(.*)$`: an ATX heading's text.
fn atx(line: &str) -> Option<&str> {
    let spaces = leading(line, |c| c == b' ');
    if spaces > 3 {
        return None;
    }
    let rest = &line[spaces..];
    let hashes = leading(rest, |c| c == b'#');
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &rest[hashes..];
    if !after.chars().next().is_some_and(is_py_space) {
        return None;
    }
    Some(after.trim_start_matches(is_py_space))
}

/// Blank fenced code blocks, keeping one (empty) line per line.
///
/// Scanned line by line because a fence may sit up to three columns past the
/// start of its container: nested in a list item it is routinely at four or
/// more columns. The innermost open list item's content column is tracked,
/// and a fence is recognised only within three columns of it, so at top level
/// a four-column line stays indented code and cannot swallow the paragraph
/// between two of them.
///
/// A fence closes on the same character, at least as long, with nothing but
/// whitespace after it. It also closes when its container ends: the block
/// quote it opened in (by depth, so `> > ~~~` ends at `> `), or the list item
/// that held it. An unclosed fence otherwise runs to end of file, as
/// `CommonMark` and GitHub render it.
///
/// Lines are blanked rather than dropped, so Setext detection still sees
/// what really preceded an underline.
#[must_use]
pub fn strip_fences(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut opener: Option<&str> = None;
    let mut stack: Vec<usize> = Vec::new();
    let mut quoted = 0usize;
    let mut held = 0usize;
    for line in py_splitlines(text) {
        let mut base = stack.last().copied().unwrap_or(0);
        let depth = quote_depth(line);
        if opener.is_some()
            && !py_strip(line).is_empty()
            && ((quoted > 0 && depth < quoted)
                || (held > 0 && depth == quoted && indent_width(unquote_line(line)) < held))
        {
            opener = None;
            quoted = 0;
            held = 0;
        }
        let content = unquote_line(line);
        let parts = fence(content).map(|(indent, run, info)| (column(indent), run, info));
        if let Some(open) = opener {
            out.push("");
            let closes = parts.is_some_and(|(indent, run, info)| {
                indent <= base + 3
                    && run.as_bytes()[0] == open.as_bytes()[0]
                    && run.len() >= open.len()
                    && py_strip(info).is_empty()
            });
            if closes {
                opener = None;
                quoted = 0;
                held = 0;
            }
            continue;
        }
        if let Some((indent, prefix)) = list_item(content) {
            let marker = column(indent);
            while stack.last().is_some_and(|&top| top > marker) {
                stack.pop();
            }
            stack.push(column(prefix));
        } else if !py_strip(content).is_empty() {
            // Dedenting leaves the innermost items but stays inside an outer
            // one, so a fence indented for the parent list is still a fence.
            let dedent = indent_width(content);
            while stack.last().is_some_and(|&top| dedent < top) {
                stack.pop();
            }
        }
        base = stack.last().copied().unwrap_or(0);
        // A backtick fence's info string may not contain a backtick.
        match parts {
            Some((indent, run, info))
                if indent <= base + 3 && !(run.starts_with('`') && info.contains('`')) =>
            {
                opener = Some(run);
                quoted = depth;
                held = base;
                out.push("");
            }
            _ => out.push(line),
        }
    }
    out.join("\n")
}

/// Replace each HTML comment with as many newlines as it spanned, so line
/// positions survive.
#[must_use]
pub fn blank_comments(text: &str) -> String {
    replace_all(&HTML_COMMENT, text, |c| {
        "\n".repeat(group(c, 0).unwrap_or("").matches('\n').count())
    })
}

/// Remove code spans: ``(?<!`)(`+)(?!`)(.*?)(?<!`)\1(?!`)``.
///
/// A span opens on a whole backtick run and closes on the next whole run of
/// the same length on the same line, so a double-backtick span can hold a
/// single backtick, and a one-tick opener is not closed by the first tick of
/// a two-tick run. A run with no closer is literal text.
#[must_use]
pub fn strip_code_spans(text: &str) -> String {
    let b = text.as_bytes();
    let run_end = |from: usize| from + b[from..].iter().take_while(|&&c| c == b'`').count();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let open_end = run_end(i);
        let width = open_end - i;
        let mut k = open_end;
        let mut close = None;
        while k < b.len() && b[k] != b'\n' {
            if b[k] == b'`' {
                let end = run_end(k);
                if end - k == width {
                    close = Some(end);
                    break;
                }
                k = end;
            } else {
                k += 1;
            }
        }
        if let Some(end) = close {
            out.push_str(&text[copied..i]);
            copied = end;
            i = end;
        } else {
            i = open_end;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The live prose of a page: fences blanked, comments blanked, code spans
/// removed. What is left is what renders as clickable text.
#[must_use]
pub fn strip_code(text: &str) -> String {
    strip_code_spans(&blank_comments(&strip_fences(text)))
}

/// GitHub's heading slug.
///
/// Lowercase, keep word characters, whitespace and hyphens, then turn each
/// space into its own hyphen. Runs are not collapsed: `## a — b` is `a--b`.
#[must_use]
pub fn slugify(heading: &str) -> String {
    let h = replace_all(&HEADING_MARKS, py_strip(heading), |_| String::new()).replace('`', "");
    let h = replace_all(&HEADING_LINK, &h, |c| group(c, 1).unwrap_or("").to_owned());
    let kept: String = h
        .to_lowercase()
        .chars()
        .filter(|&c| c.is_alphanumeric() || c == '_' || is_py_space(c) || c == '-')
        .collect();
    py_strip(&kept).replace(' ', "-")
}

/// Every anchor a page defines: heading slugs (ATX and Setext, with GitHub's
/// `-1`, `-2` suffixes for repeats) and explicit `<a id>`/`<a name>` ids.
///
/// Headings inside fences or HTML comments render no anchor and are not
/// indexed. YAML front matter is skipped: its closing `---` sits under a
/// content line exactly as a Setext underline does, and a phantom anchor
/// makes a broken link look resolved.
#[must_use]
pub fn anchors(text: &str) -> HashSet<String> {
    let body = blank_comments(&strip_fences(text));
    let lines = py_splitlines(&body);

    let start = if lines.first().is_some_and(|l| py_strip(l) == "---")
        && let Some(close) = lines.iter().skip(1).position(|l| py_strip(l) == "---")
    {
        close + 2
    } else {
        0
    };

    let mut found = HashSet::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut add = |title: &str| {
        let s = slugify(title);
        if s.is_empty() {
            return;
        }
        if found.insert(s.clone()) {
            return;
        }
        // The suffix must land on an id nothing else claimed: `Foo`, `Foo`,
        // `Foo-1` become `foo`, `foo-1`, `foo-1-1`.
        let mut n = seen.get(&s).copied().unwrap_or(0).max(1);
        while found.contains(&format!("{s}-{n}")) {
            n += 1;
        }
        seen.insert(s.clone(), n + 1);
        found.insert(format!("{s}-{n}"));
    };

    // A heading nested in a block quote still carries an anchor, so quote
    // markers come off the underline, the ATX line and the promoted
    // paragraph alike.
    let unquoted: Vec<&str> = lines.iter().map(|l| unquote_line(l)).collect();
    let depths: Vec<usize> = lines.iter().map(|l| quote_depth(l)).collect();

    for i in start..lines.len() {
        let line = unquoted[i];
        if let Some(title) = atx(line) {
            add(title);
        } else if is_setext(line)
            && i > start
            && depths[i - 1] == depths[i]
            && !is_not_paragraph(unquoted[i - 1])
        {
            // The underline promotes the whole paragraph above it, within
            // the same container.
            let mut j = i - 1;
            while j > start && depths[j - 1] == depths[i] && !is_not_paragraph(unquoted[j - 1]) {
                j -= 1;
            }
            let title: Vec<&str> = unquoted[j..i].iter().map(|l| py_strip(l)).collect();
            add(&title.join(" "));
        }
    }

    // A tag shown as inline code creates no anchor.
    for caps in HTML_ANCHOR.captures_iter(&strip_code_spans(&body)) {
        if let Some(id) = group(&caps, 1)
            .or_else(|| group(&caps, 2))
            .or_else(|| group(&caps, 3))
        {
            found.insert(id.to_owned());
        }
    }
    found
}

/// Every link destination in `text`, once each, in first-seen order, with
/// escapes resolved and angle brackets removed.
///
/// Pass live prose ([`strip_code`]): a sample inside code is not a link.
///
/// Escaped backslashes are folded to a placeholder first: in `\\[x](y.md)`
/// the first backslash consumes the second, so the bracket is unescaped.
#[must_use]
pub fn link_targets(text: &str) -> Vec<String> {
    let text = text.replace("\\\\", "\0");
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    let found = captures_unescaped(&INLINE, &text)
        .into_iter()
        .chain(captures_unescaped(&NESTED, &text))
        .chain(REFDEF.captures_iter(&text));
    for caps in found {
        let bracketed = group(&caps, 1).filter(|s| !s.is_empty());
        let raw =
            py_strip(bracketed.or_else(|| group(&caps, 2)).unwrap_or("")).replace('\0', "\\\\");
        let target = replace_all(&ESCAPED_PUNCT, &raw, |c| {
            group(c, 1).unwrap_or("").to_owned()
        });
        if !target.is_empty() && seen.insert(target.clone()) {
            targets.push(target);
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_compiles() {
        for re in [
            &*INLINE,
            &*NESTED,
            &*REFDEF,
            &*ESCAPED_PUNCT,
            &*RUSTDOC,
            &*HTML_COMMENT,
            &*HTML_ANCHOR,
            &*HEADING_MARKS,
            &*HEADING_LINK,
        ] {
            assert!(!re.as_str().is_empty());
        }
    }

    // The Python patterns the hand matchers replace. `LIST_ITEM`'s
    // `(?=\S)` lookahead becomes a trailing `\S` here, and the matcher's
    // prefix is compared without that character.
    pattern! {
        static REF_BLOCKQUOTE = r"^ {0,3}(?:>[ \t]?)+";
    }
    pattern! {
        static REF_FENCE = r"^([ \t]*)(`{3,}|~{3,})(.*)$";
    }
    pattern! {
        static REF_LIST_ITEM = r"^([ \t]*)(?:[-*+]|[0-9]{1,9}[.)])([ \t]+)\S";
    }
    pattern! {
        static REF_SETEXT = r"^ {0,3}(=+|-+)[ \t]*$";
    }
    pattern! {
        static REF_NOT_PARAGRAPH =
            r"^(?: {0,3}[#>|]| {0,3}(?:[-*+=]|[0-9]{1,9}[.)])[ \t]|\s*$|\s{4,})";
    }
    pattern! {
        static REF_ATX = r"^ {0,3}(#{1,6})\s+(.*)$";
    }

    fn agree(line: &str) {
        let q = REF_BLOCKQUOTE
            .find(line)
            .map_or((0, 0), |m| (m.end(), m.as_str().matches('>').count()));
        assert_eq!(quote_prefix(line), q, "block quote: {line:?}");

        let f = REF_FENCE.captures(line).map(|c| {
            (
                group(&c, 1).unwrap(),
                group(&c, 2).unwrap(),
                group(&c, 3).unwrap(),
            )
        });
        assert_eq!(fence(line), f, "fence: {line:?}");

        let l = REF_LIST_ITEM.captures(line).map(|c| {
            let whole = group(&c, 0).unwrap();
            let last = whole.chars().next_back().unwrap();
            (
                group(&c, 1).unwrap(),
                &whole[..whole.len() - last.len_utf8()],
            )
        });
        assert_eq!(list_item(line), l, "list item: {line:?}");

        assert_eq!(
            is_setext(line),
            REF_SETEXT.is_match(line),
            "setext: {line:?}"
        );
        assert_eq!(
            is_not_paragraph(line),
            REF_NOT_PARAGRAPH.is_match(line),
            "not-paragraph: {line:?}"
        );
        let a = REF_ATX.captures(line).map(|c| group(&c, 2).unwrap());
        assert_eq!(atx(line), a, "atx: {line:?}");
    }

    /// Every line of every page in this repository, before and after its
    /// quote markers come off.
    #[test]
    fn hand_matchers_agree_with_the_patterns_on_the_corpus() {
        let root = crate::repo_root();
        let files = super::super::corpus::tracked_markdown(root).unwrap();
        let mut lines = 0;
        for f in files {
            let Some(text) = super::super::corpus::read(&root.join(&f)) else {
                continue;
            };
            for line in py_splitlines(&text) {
                agree(line);
                agree(unquote_line(line));
                lines += 1;
            }
        }
        assert!(lines > 10_000, "only {lines} lines");
    }

    /// Every line up to four characters over the characters these shapes
    /// turn on.
    #[test]
    fn hand_matchers_agree_with_the_patterns_exhaustively() {
        const ALPHABET: [char; 15] = [
            ' ', '\t', '>', '`', '~', '-', '=', '#', '1', '.', ')', '*', '|', 'a', '+',
        ];
        fn walk(line: &mut String, depth: usize) {
            agree(line);
            if depth == 0 {
                return;
            }
            for c in ALPHABET {
                line.push(c);
                walk(line, depth - 1);
                line.pop();
            }
        }
        walk(&mut String::new(), 4);
        agree("1234567890. x");
        agree("123456789. x");
        agree("####### x");
        agree("###### x");
        agree("   > > > x");
        agree("    > x");
        agree("> \t> x");
    }

    #[test]
    fn column_counts_tabs_to_four_column_stops() {
        assert_eq!(column(""), 0);
        assert_eq!(column("  "), 2);
        assert_eq!(column("\t"), 4);
        assert_eq!(column(" \t"), 4);
        assert_eq!(column("    \t"), 8);
    }

    #[test]
    fn quote_depth_counts_markers() {
        assert_eq!(quote_depth("plain"), 0);
        assert_eq!(quote_depth("> a"), 1);
        assert_eq!(quote_depth("> > a"), 2);
        assert_eq!(quote_depth("   > a"), 1);
        assert_eq!(quote_depth("    > a"), 0, "four spaces is code");
    }

    #[test]
    fn code_spans_follow_run_lengths() {
        assert_eq!(strip_code_spans("a `b` c"), "a  c");
        assert_eq!(strip_code_spans("a ``b ` c`` d"), "a  d");
        assert_eq!(
            strip_code_spans("a `b`` c"),
            "a `b`` c",
            "unmatched run is literal"
        );
        assert_eq!(
            strip_code_spans("a `b\nc` d"),
            "a `b\nc` d",
            "spans stay on one line"
        );
        assert_eq!(strip_code_spans("`a` `b`"), " ");
        assert_eq!(strip_code_spans("``"), "``");
        assert_eq!(strip_code_spans("x ``` `y` ```"), "x ");
    }

    #[test]
    fn slugs_follow_github() {
        assert_eq!(
            slugify("`after_commit` — post-commit callbacks"),
            "after_commit--post-commit-callbacks"
        );
        assert_eq!(slugify("Setext title"), "setext-title");
        assert_eq!(slugify("Café"), "café");
        assert_eq!(slugify("See [the guide](x.md) now"), "see-the-guide-now");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn fences_are_blanked_line_for_line() {
        let text = "a\n```md\n[x](y.md)\n```\nb";
        assert_eq!(strip_fences(text), "a\n\n\n\nb");
    }

    #[test]
    fn link_targets_cover_inline_nested_and_reference_forms() {
        let text = "[a](one.md) [![i](img.png)](page.md) [r][ref]\n\n[ref]: <three.md>\n";
        assert_eq!(
            link_targets(text),
            ["one.md", "img.png", "page.md", "three.md"]
        );
    }

    #[test]
    fn anchors_index_headings_and_html_ids() {
        let a = anchors("# Top\n\n## Top\n\nPara\n---\n\n<a id='Mixed'></a>\n");
        for id in ["top", "top-1", "para", "Mixed"] {
            assert!(a.contains(id), "{id} missing from {a:?}");
        }
    }
}
