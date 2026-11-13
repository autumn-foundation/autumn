//! Text helpers that reproduce the Python the docs gates were written in.
//!
//! The ported gates must give the same answer the scripts gave, so where
//! Rust and Python differ (line splitting, whitespace, URL unquoting, a
//! regex lookbehind) the Python behaviour is spelled out here once.

use regex::{Captures, Regex};

/// Declare a lazily compiled pattern.
///
/// The `expect` is the documented exception to "no panics": the pattern is
/// a literal, and `every_pattern_compiles` in the markdown tests forces each
/// one, so a bad pattern fails the test suite rather than a gate run.
macro_rules! pattern {
    ($(#[$meta:meta])* $vis:vis static $name:ident = $src:expr;) => {
        $(#[$meta])*
        $vis static $name: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| {
                regex::Regex::new($src)
                    .expect(concat!("pattern ", stringify!($name), " compiles"))
            });
    };
}
pub(crate) use pattern;

/// Every match of `re` in `text` that is not preceded by a backslash, as
/// Python's `findall` returns them for a pattern that starts with `(?<!\\)`.
///
/// A failed lookbehind at a position means "no match here, try the next
/// position", so a rejected candidate resumes the search one character
/// later rather than after the candidate. Resuming after it would skip a
/// real link nested inside an escaped one.
#[must_use]
pub fn captures_unescaped<'t>(re: &Regex, text: &'t str) -> Vec<Captures<'t>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at <= text.len() {
        let Some(caps) = re.captures_at(text, at) else {
            break;
        };
        let Some(whole) = caps.get(0) else { break };
        if text[..whole.start()].ends_with('\\') {
            at = next_boundary(text, whole.start());
            continue;
        }
        at = if whole.end() > whole.start() {
            whole.end()
        } else {
            next_boundary(text, whole.end())
        };
        found.push(caps);
    }
    found
}

fn next_boundary(text: &str, i: usize) -> usize {
    text[i..]
        .chars()
        .next()
        .map_or(text.len() + 1, |c| i + c.len_utf8())
}

/// Python's `re.sub` with a function replacement.
#[must_use]
pub fn replace_all(re: &Regex, text: &str, with: impl FnMut(&Captures<'_>) -> String) -> String {
    re.replace_all(text, with).into_owned()
}

/// Group `i` of `caps` as a `&str`, or `None` when it did not participate.
#[must_use]
pub fn group<'t>(caps: &Captures<'t>, i: usize) -> Option<&'t str> {
    caps.get(i).map(|m| m.as_str())
}

/// Python's `str.isspace` for one character: Unicode `White_Space` plus the
/// four separators (`\x1c`-`\x1f`) Python also counts.
#[must_use]
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `text` as an ASCII pattern sees it under Python's `re` rules.
///
/// Rust's `\b` counts a combining mark as a word character and Python's does
/// not, and Python counts `²` (category `No`) where Rust does not. Mapping
/// every non-ASCII character to an ASCII stand-in, one for one, makes the two
/// engines agree for a pattern written in ASCII: Python whitespace becomes
/// `\x1c` (which a pattern's `\s` must then include), a Python word
/// character (category `L` or `N`) becomes `_`, and anything else becomes
/// `\x01`. With `fold`, the four characters Python's `re.IGNORECASE` folds
/// onto an ASCII letter (`İ`, `ı`, `ſ`, the Kelvin sign) become that letter.
#[must_use]
pub fn py_ascii_view(text: &str, fold: bool) -> String {
    text.chars()
        .map(|c| match c {
            _ if c.is_ascii() => c,
            '\u{130}' | '\u{131}' if fold => 'i',
            '\u{17f}' if fold => 's',
            '\u{212a}' if fold => 'k',
            _ if is_py_space(c) => '\x1c',
            _ if is_py_word(c) => '_',
            _ => '\x01',
        })
        .collect()
}

/// Python's `\w` for a non-ASCII character: a letter or a number.
fn is_py_word(c: char) -> bool {
    use unicode_general_category::GeneralCategory as G;
    matches!(
        unicode_general_category::get_general_category(c),
        G::UppercaseLetter
            | G::LowercaseLetter
            | G::TitlecaseLetter
            | G::ModifierLetter
            | G::OtherLetter
            | G::DecimalNumber
            | G::LetterNumber
            | G::OtherNumber
    )
}

/// Python's `str.strip()`.
#[must_use]
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python's `str.splitlines()`: every Unicode line boundary Python splits on,
/// `\r\n` as one, and no trailing empty line after a final newline.
#[must_use]
pub fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        lines.push(&text[start..i]);
        start = if c == '\r'
            && let Some(&(j, '\n')) = chars.peek()
        {
            chars.next();
            j + 1
        } else {
            i + c.len_utf8()
        };
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Python's `urllib.parse.unquote`: `%XX` escapes decoded as UTF-8, invalid
/// bytes replaced, malformed escapes left as written.
#[must_use]
pub fn unquote(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = (hex(h), hex(l))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pattern! {
        static BRACKETED = r"\[[^\[\]]*\](?:\([^()]*\))?";
    }

    pattern! {
        static NESTED = r"\[(?:[^\[\]]|\[[^\[\]]*\])*\](?:\([^()]*\))?";
    }

    fn found<'t>(re: &Regex, text: &'t str) -> Vec<&'t str> {
        captures_unescaped(re, text)
            .iter()
            .map(|c| group(c, 0).unwrap())
            .collect()
    }

    #[test]
    fn an_escaped_candidate_is_skipped() {
        assert_eq!(found(&BRACKETED, r"\[a](b) [c](d)"), ["[c](d)"]);
        assert_eq!(found(&BRACKETED, r"[a](b)"), ["[a](b)"]);
    }

    /// The case that makes "resume one character later" necessary: a
    /// rejected candidate must not swallow a real match that starts inside
    /// it.
    #[test]
    fn a_rejected_candidate_does_not_hide_a_later_start() {
        assert_eq!(found(&NESTED, r"\[x [y](z)](w)"), ["[y](z)"]);
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("\n"), [""]);
        assert_eq!(py_splitlines("a\nb"), ["a", "b"]);
        assert_eq!(py_splitlines("a\r\nb\r\n"), ["a", "b"]);
        assert_eq!(py_splitlines("a\rb\x0cc\u{2028}d"), ["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\n\nb"), ["a", "", "b"]);
    }

    #[test]
    fn the_ascii_view_keeps_python_word_classes() {
        assert_eq!(py_ascii_view("a\u{301}\u{b2}\u{a0}é", false), "a\x01_\x1c_");
        assert_eq!(py_ascii_view("\u{17f}\u{130}", true), "si");
        assert_eq!(py_ascii_view("\u{17f}\u{130}", false), "__");
    }

    #[test]
    fn strip_matches_python() {
        assert_eq!(py_strip("\x1f\t a \u{a0}"), "a");
        assert_eq!(py_strip("   "), "");
    }

    #[test]
    fn unquote_matches_python() {
        assert_eq!(unquote("a%20b.md"), "a b.md");
        assert_eq!(unquote("caf%C3%A9"), "café");
        assert_eq!(unquote("100%"), "100%");
        assert_eq!(unquote("%zz%4"), "%zz%4");
        assert_eq!(unquote("%FF"), "\u{fffd}");
    }
}
