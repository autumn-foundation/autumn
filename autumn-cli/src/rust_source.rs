//! Comment- and string-aware scanning of user-owned Rust source.
//!
//! Commands that edit an application's `src/main.rs` — `autumn generate pwa`,
//! `autumn plugin add` — must never splice code into a comment, and must never
//! mistake text inside a string literal for code. Autumn's own documentation
//! ships quick-start snippets containing the exact lines those commands anchor
//! on (`autumn_web::app()`, `autumn_web::push::router()`,
//! `AdminPlugin::new()`), so a `main.rs` that pasted one into a doc comment or
//! into embedded help text would otherwise be edited *inside* it — or, worse,
//! be read as "already wired" and skipped, leaving the app without the mount
//! the command reported installing.
//!
//! [`mask_non_code`] answers both by blanking comment bodies and string-literal
//! contents to spaces **in place**, preserving every byte offset and line
//! break. Callers scan the mask and splice into the original at the offsets it
//! reports, so one scanner serves both the anchor search and its "is it already
//! there?" companion — they can never disagree about what counts as code.

/// Lexer state while walking Rust source.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// Ordinary code.
    Code,
    /// Inside a `//` line comment, until the newline.
    LineComment,
    /// Inside `/* … */`, which nests in Rust; carries the open depth.
    BlockComment(usize),
    /// Inside a `"…"` string literal.
    Str,
    /// Inside a `r"…"` / `r#"…"#` raw string; carries the hash count.
    RawStr(usize),
    /// Inside a `'…'` character literal.
    CharLit,
}

/// Return `source` with every comment body and string-literal content replaced
/// by spaces.
///
/// The result has **exactly** the same length as the input and the same
/// newlines, so a byte offset into the mask is a byte offset into the original.
/// Delimiters are blanked along with their contents, so a trailing `// note`
/// no longer hides the code before it from a `trim_end()` match.
///
/// Handles the literal forms that can hide a probe or an anchor: line and
/// (nesting) block comments, escaped strings, raw strings of any hash count,
/// and character literals — with lifetimes (`'a`) correctly *not* treated as
/// the start of one.
#[must_use]
pub fn mask_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut state = Scan::Code;
    let mut i = 0usize;

    while i < bytes.len() {
        let (next, advance) = match state {
            Scan::Code => step_code(bytes, i, &mut out),
            Scan::LineComment => step_line_comment(bytes, i, &mut out),
            Scan::BlockComment(depth) => step_block_comment(bytes, i, depth, &mut out),
            Scan::Str | Scan::CharLit => {
                let terminator = if state == Scan::Str { b'"' } else { b'\'' };
                step_quoted(bytes, i, terminator, state, &mut out)
            }
            Scan::RawStr(hashes) => step_raw_string(bytes, i, hashes, &mut out),
        };
        state = next;
        i += advance;
    }

    // Only whole ASCII bytes were replaced with ASCII spaces, so the result is
    // still the valid UTF-8 it started as.
    String::from_utf8(out).unwrap_or_else(|_| source.to_owned())
}

/// Blank one byte unless it is the newline that keeps lines aligned.
const fn blank(out: &mut [u8], at: usize) {
    if out[at] != b'\n' {
        out[at] = b' ';
    }
}

/// Blank `count` bytes from `at`.
fn blank_run(out: &mut [u8], at: usize, count: usize) {
    for offset in 0..count {
        blank(out, at + offset);
    }
}

/// One step in ordinary code: enter a comment or literal, or move on.
fn step_code(bytes: &[u8], i: usize, out: &mut [u8]) -> (Scan, usize) {
    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
        blank_run(out, i, 2);
        (Scan::LineComment, 2)
    } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
        blank_run(out, i, 2);
        (Scan::BlockComment(1), 2)
    } else if bytes[i] == b'"' {
        blank(out, i);
        (Scan::Str, 1)
    } else if let Some(after) = raw_string_start(bytes, i) {
        // `after` is just past the opening quote; the hashes sit between the
        // `r` and it.
        let hashes = after - i - 2;
        blank_run(out, i, after - i);
        (Scan::RawStr(hashes), after - i)
    } else if bytes[i] == b'\'' && is_char_literal(bytes, i) {
        blank(out, i);
        (Scan::CharLit, 1)
    } else {
        (Scan::Code, 1)
    }
}

/// One step inside a `//` comment: it ends at the newline.
const fn step_line_comment(bytes: &[u8], i: usize, out: &mut [u8]) -> (Scan, usize) {
    if bytes[i] == b'\n' {
        (Scan::Code, 1)
    } else {
        blank(out, i);
        (Scan::LineComment, 1)
    }
}

/// One step inside a `/* … */` comment, which nests in Rust.
fn step_block_comment(bytes: &[u8], i: usize, depth: usize, out: &mut [u8]) -> (Scan, usize) {
    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
        blank_run(out, i, 2);
        (Scan::BlockComment(depth + 1), 2)
    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
        blank_run(out, i, 2);
        let next = if depth == 1 {
            Scan::Code
        } else {
            Scan::BlockComment(depth - 1)
        };
        (next, 2)
    } else {
        blank(out, i);
        (Scan::BlockComment(depth), 1)
    }
}

/// One step inside a `"…"` string or `'…'` character literal, honouring
/// backslash escapes so `"\""` does not terminate early.
fn step_quoted(
    bytes: &[u8],
    i: usize,
    terminator: u8,
    state: Scan,
    out: &mut [u8],
) -> (Scan, usize) {
    if bytes[i] == b'\\' {
        let span = if i + 1 < bytes.len() { 2 } else { 1 };
        blank_run(out, i, span);
        (state, span)
    } else if bytes[i] == terminator {
        blank(out, i);
        (Scan::Code, 1)
    } else {
        blank(out, i);
        (state, 1)
    }
}

/// One step inside a raw string, which ends only at a quote followed by the
/// same number of hashes it opened with.
fn step_raw_string(bytes: &[u8], i: usize, hashes: usize, out: &mut [u8]) -> (Scan, usize) {
    if bytes[i] == b'"' && closes_raw_string(bytes, i, hashes) {
        blank_run(out, i, hashes + 1);
        (Scan::Code, hashes + 1)
    } else {
        blank(out, i);
        (Scan::RawStr(hashes), 1)
    }
}

/// If a raw-string literal opens at `i` (`r"`, `r#"`, `r##"`, …), the offset
/// just past its opening quote.
fn raw_string_start(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes[i] != b'r' {
        return None;
    }
    // A `r` that continues an identifier (`for`, `var`) is not a literal.
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        return None;
    }
    let mut at = i + 1;
    while bytes.get(at) == Some(&b'#') {
        at += 1;
    }
    (bytes.get(at) == Some(&b'"')).then_some(at + 1)
}

/// Whether the `"` at `i` closes a raw string opened with `hashes` hashes.
fn closes_raw_string(bytes: &[u8], i: usize, hashes: usize) -> bool {
    (1..=hashes).all(|offset| bytes.get(i + offset) == Some(&b'#'))
}

/// Whether the `'` at `i` opens a character literal rather than a lifetime.
///
/// `'a` in `&'a str` or `Cow<'static, str>` is a lifetime; `'a'` and `'\n'`
/// are literals. Treating a lifetime as an opening quote would blank the rest
/// of the file.
fn is_char_literal(bytes: &[u8], i: usize) -> bool {
    match bytes.get(i + 1) {
        // `'\n'`, `'\''`, `'\\'` — an escape is always a literal.
        Some(b'\\') => true,
        Some(_) => {
            // A literal closes within a few bytes; a lifetime never does.
            // Scan past one (possibly multi-byte) character.
            let mut at = i + 2;
            while at < bytes.len() && at <= i + 5 {
                if bytes[at] == b'\'' {
                    return true;
                }
                // Only a multi-byte UTF-8 continuation can legitimately extend
                // the character; anything else means this was a lifetime.
                if bytes[at] & 0b1100_0000 != 0b1000_0000 {
                    return false;
                }
                at += 1;
            }
            false
        }
        None => false,
    }
}

/// The index of the `)` that closes the `(` at `open`, or `None` if the
/// source ends first.
///
/// Only meaningful on a masked source: a paren inside a string or comment
/// would otherwise unbalance the count.
#[must_use]
pub fn balanced_close_paren(masked: &str, open: usize) -> Option<usize> {
    let bytes = masked.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every line of `source` with comments and string contents blanked, paired
/// with its byte offset in the **original** source.
///
/// The offset is tracked as the walk proceeds rather than recovered afterwards
/// with `source.find(line)`, which would re-locate an identical earlier line
/// and hand back the wrong splice point.
#[must_use]
pub fn code_lines(source: &str) -> Vec<(String, usize)> {
    let masked = mask_non_code(source);
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in masked.split_inclusive('\n') {
        out.push((line.to_owned(), offset));
        offset += line.len();
    }
    out
}

/// Walk `source`'s code lines and return the first non-`None` result of `f`.
///
/// Every caller that edits a user-owned `main.rs` shares this one scanner so
/// they can never disagree about what counts as code. Callers today:
/// `autumn generate pwa`'s push-router injection and `autumn plugin add`'s
/// builder-chain scan and mount probe.
pub fn for_each_code_line<T>(
    source: &str,
    mut f: impl FnMut(&str, usize) -> Option<T>,
) -> Option<T> {
    code_lines(source)
        .into_iter()
        .find_map(|(line, offset)| f(&line, offset))
}

/// Whether `line` declares `async fn main` — the entry point, not a helper
/// whose name merely starts with it (`async fn main_loop`).
#[must_use]
pub fn declares_async_main(line: &str) -> bool {
    const NEEDLE: &str = "async fn main";
    let mut rest = line;
    while let Some(at) = rest.find(NEEDLE) {
        let after = &rest[at + NEEDLE.len()..];
        if after
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        {
            return true;
        }
        rest = after;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{
        balanced_close_paren, code_lines, declares_async_main, for_each_code_line, mask_non_code,
    };

    /// The mask must be a byte-for-byte overlay: offsets computed on it index
    /// the original exactly.
    #[test]
    fn masking_preserves_length_and_newlines() {
        for source in [
            "let x = \"hello\";\n",
            "// comment\nlet y = 2;\n",
            "/* a\n   b */\nlet z = 3;\n",
            "let r = r#\"raw \" string\"#;\n",
            "let c = 'x'; let s: &'a str;\n",
        ] {
            let masked = mask_non_code(source);
            assert_eq!(masked.len(), source.len(), "{source:?}");
            assert_eq!(
                masked.matches('\n').count(),
                source.matches('\n').count(),
                "{source:?}"
            );
        }
    }

    /// The finding this scanner exists for: a probe hidden in a string literal
    /// must not read as code.
    #[test]
    fn string_contents_are_not_code() {
        let source = "fn main() {\n    let help = \"run AdminPlugin::new() yourself\";\n}\n";
        assert!(
            for_each_code_line(source, |line, _| line
                .contains("AdminPlugin::new(")
                .then_some(()))
            .is_none()
        );
    }

    #[test]
    fn raw_string_contents_are_not_code() {
        let source = "fn main() {\n    let doc = r#\"\n    autumn_web::app()\n\"#;\n}\n";
        assert!(
            for_each_code_line(source, |line, _| line
                .contains("autumn_web::app()")
                .then_some(()))
            .is_none()
        );
    }

    /// A raw string with more hashes must not be closed by a shorter run.
    #[test]
    fn raw_strings_respect_their_hash_count() {
        let source = "let d = r##\"a \"# b autumn_web::app()\"##;\nlet real = autumn_web::app();\n";
        let found = for_each_code_line(source, |line, offset| {
            line.contains("autumn_web::app()").then_some(offset)
        })
        .expect("the real call");
        assert!(source[found..].starts_with("let real"), "{found}");
    }

    #[test]
    fn line_and_block_comments_are_not_code() {
        for source in [
            "// let x = 1;\n",
            "/// let x = 1;\n",
            "//! let x = 1;\n",
            "   // let x = 1;\n",
            "/*\nlet x = 1;\n*/\n",
            "/* /* nested */ let x = 1; */\n",
        ] {
            assert!(
                for_each_code_line(source, |line, _| line.contains("let x").then_some(()))
                    .is_none(),
                "{source:?}"
            );
        }
    }

    /// A `//` inside a string is not a comment, and a quote inside a comment
    /// does not open a string — the two states have to be tracked together.
    #[test]
    fn comment_and_string_states_do_not_leak_into_each_other() {
        let source = "let url = \"https://example.com\";\nlet real = autumn_web::app();\n";
        assert!(
            for_each_code_line(source, |line, _| line
                .contains("autumn_web::app()")
                .then_some(()))
            .is_some()
        );

        let source = "// he said \"hi\n let real = autumn_web::app();\n";
        assert!(
            for_each_code_line(source, |line, _| line
                .contains("autumn_web::app()")
                .then_some(()))
            .is_some(),
            "an unbalanced quote in a comment must not swallow the rest of the file"
        );
    }

    /// A lifetime is not a character literal; reading `'a` as an opening quote
    /// would blank everything after it.
    #[test]
    fn lifetimes_do_not_open_a_character_literal() {
        let source = "fn f<'a>(s: &'a str) {}\nlet real = autumn_web::app();\n";
        assert!(
            for_each_code_line(source, |line, _| line
                .contains("autumn_web::app()")
                .then_some(()))
            .is_some()
        );
    }

    #[test]
    fn character_literals_are_still_masked() {
        let source = "let q = '\"'; let real = autumn_web::app();\n";
        assert!(
            for_each_code_line(source, |line, _| line
                .contains("autumn_web::app()")
                .then_some(()))
            .is_some(),
            "a quote inside a char literal must not open a string"
        );
    }

    /// The offset handed to the callback must point at the line the callback
    /// actually saw — recovering it afterwards with `find` would land on an
    /// identical earlier line and splice at the wrong place.
    #[test]
    fn the_offset_locates_the_matching_line() {
        let source = "let x = 1;\nlet x = 1;\nlet y = 2;\n";
        let offset = for_each_code_line(source, |line, offset| {
            line.contains("y = 2").then_some(offset)
        })
        .expect("found");
        assert_eq!(&source[offset..offset + 9], "let y = 2");
    }

    #[test]
    fn every_line_is_reported_with_a_stable_offset() {
        let source = "a\nbb\nccc\n";
        let lines = code_lines(source);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].1, 0);
        assert_eq!(lines[1].1, 2);
        assert_eq!(lines[2].1, 5);
    }

    #[test]
    fn a_helper_whose_name_starts_with_main_is_not_the_entry_point() {
        assert!(declares_async_main("async fn main() {"));
        assert!(declares_async_main("pub async fn main() {"));
        assert!(!declares_async_main("async fn main_loop() {"));
        assert!(!declares_async_main("async fn mainly() {"));
        assert!(!declares_async_main("fn main() {"));
    }

    #[test]
    fn balanced_close_paren_spans_nested_calls() {
        let src = "f(a, g(b, c), d) tail";
        assert_eq!(balanced_close_paren(src, 1), Some(15));
        assert_eq!(balanced_close_paren(src, 0), None, "index 0 is not a paren");
        assert_eq!(balanced_close_paren("f(a", 1), None, "unterminated");
    }

    #[test]
    fn the_first_match_wins() {
        let source = "let a = 1;\nlet a = 2;\n";
        let offset = for_each_code_line(source, |line, offset| {
            line.contains("let a").then_some(offset)
        })
        .expect("found");
        assert_eq!(offset, 0);
    }
}
