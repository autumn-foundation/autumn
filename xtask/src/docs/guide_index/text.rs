//! A document indexed by character, the way the guide-index rules count
//! positions, with a one-byte-per-character view for the patterns.
//!
//! Every position in this gate is a character offset: a span blanked in one
//! copy of a document must line up with the same span in another, and a
//! column is a count of characters. Rust strings index by byte, so a [`Text`]
//! keeps the characters and, beside them, a pure-ASCII string with exactly
//! one byte per character. A pattern run over that string reports byte
//! offsets that are character offsets.
//!
//! The ASCII view keeps every ASCII character except `\x01`, and stands one
//! byte in for each other class of character the patterns can tell apart:
//!
//! | Character | Byte | Matched by |
//! | --- | --- | --- |
//! | whitespace (Python's `str.isspace`) | `\x1c` | [`ws!`] |
//! | decimal digit (category `Nd`) | `\x01` | [`digit!`] |
//! | anything else | `\x7f` | negated classes only |
//! | `\x01` itself | `\x02` | negated classes only |
//!
//! Patterns therefore never write `\s`, `\S` or `\d`; they use the macros
//! below, which match those bytes as Python's `re` matches the characters.

use regex::Regex;
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::docs::re::is_py_space;

/// The characters of Python's `\s`, for use inside a class.
macro_rules! ws_chars {
    () => {
        r"\t\n\x0B\x0C\r\x1C-\x1F "
    };
}

/// Python's `\s`.
macro_rules! ws {
    () => {
        concat!("[", ws_chars!(), "]")
    };
}

/// Python's `\S`.
macro_rules! not_ws {
    () => {
        concat!("[^", ws_chars!(), "]")
    };
}

/// Python's `\s` without `\n`: the whitespace a blank line may hold.
macro_rules! line_ws {
    () => {
        r"[\t\x0B\x0C\r\x1C-\x1F ]"
    };
}

/// Python's `\d`.
macro_rules! digit {
    () => {
        r"[0-9\x01]"
    };
}

pub(super) use {digit, line_ws, not_ws, ws, ws_chars};

/// The byte that stands for `c` in a [`Text`]'s ASCII view.
fn class_byte(c: char) -> u8 {
    if c.is_ascii() {
        if c == '\x01' { 0x02 } else { c as u8 }
    } else if is_py_space(c) {
        0x1c
    } else if get_general_category(c) == GeneralCategory::DecimalNumber {
        0x01
    } else {
        0x7f
    }
}

/// A span of a pattern match, in character offsets.
#[derive(Debug, Clone)]
pub struct Hit {
    spans: Vec<Option<(usize, usize)>>,
}

impl Hit {
    fn new(caps: &regex::Captures<'_>, base: usize) -> Self {
        Self {
            spans: caps
                .iter()
                .map(|m| m.map(|m| (base + m.start(), base + m.end())))
                .collect(),
        }
    }

    /// Where the whole match starts.
    pub fn start(&self) -> usize {
        self.spans[0].map_or(0, |s| s.0)
    }

    /// Where the whole match ends.
    pub fn end(&self) -> usize {
        self.spans[0].map_or(0, |s| s.1)
    }

    /// Group `i`'s span, if it took part.
    pub fn group(&self, i: usize) -> Option<(usize, usize)> {
        self.spans.get(i).copied().flatten()
    }
}

/// A document as characters, with its ASCII view.
#[derive(Debug, Clone, Default)]
pub struct Text {
    /// The characters.
    pub c: Vec<char>,
    a: String,
}

impl Text {
    pub fn new(s: &str) -> Self {
        Self::from_chars(s.chars().collect())
    }

    pub fn from_chars(c: Vec<char>) -> Self {
        let a = c.iter().map(|&ch| char::from(class_byte(ch))).collect();
        Self { c, a }
    }

    pub const fn len(&self) -> usize {
        self.c.len()
    }

    /// The ASCII view of `[a, b)`, clamped to the text.
    pub fn ascii(&self, a: usize, b: usize) -> &str {
        let n = self.a.len();
        let b = b.min(n);
        &self.a[a.min(b)..b]
    }

    /// The whole ASCII view.
    pub fn view(&self) -> &str {
        &self.a
    }

    /// The characters of `[a, b)` as a string, clamped like a Python slice.
    pub fn slice(&self, a: usize, b: usize) -> String {
        let n = self.c.len();
        let b = b.min(n);
        self.c[a.min(b)..b].iter().collect()
    }

    /// True when the character at `i` is `ch`.
    pub fn is(&self, i: usize, ch: char) -> bool {
        self.c.get(i) == Some(&ch)
    }

    pub fn starts_with(&self, pos: usize, s: &str) -> bool {
        pos <= self.a.len() && self.a[pos..].starts_with(s)
    }

    /// Python's `text.find(needle, start, end)` for an ASCII needle: the
    /// first occurrence lying wholly inside `[start, end)`.
    pub fn find(&self, needle: &str, start: usize, end: usize) -> Option<usize> {
        let end = end.min(self.a.len());
        if start > end {
            return None;
        }
        self.a[start..end].find(needle).map(|i| start + i)
    }

    /// The first `ch` at or after `from`.
    pub fn find_char(&self, ch: char, from: usize) -> Option<usize> {
        let from = from.min(self.c.len());
        self.c[from..]
            .iter()
            .position(|&c| c == ch)
            .map(|i| from + i)
    }

    /// Python's `text.rfind(ch, start, end)`.
    pub fn rfind_char(&self, ch: char, start: usize, end: usize) -> Option<usize> {
        let end = end.min(self.c.len());
        if start > end {
            return None;
        }
        self.c[start..end]
            .iter()
            .rposition(|&c| c == ch)
            .map(|i| start + i)
    }

    /// The offset of the newline ending the line holding `pos`, or the end.
    pub fn line_end(&self, pos: usize) -> usize {
        self.find_char('\n', pos).unwrap_or(self.c.len())
    }

    /// Python's `pattern.match(text, pos)`. The pattern must begin with
    /// `\A`, which anchors it at `pos`; nothing in this gate's patterns
    /// looks behind the match.
    pub fn match_at(&self, re: &Regex, pos: usize) -> Option<Hit> {
        let pos = pos.min(self.a.len());
        re.captures(&self.a[pos..]).map(|c| Hit::new(&c, pos))
    }

    /// Python's `pattern.search(text, pos)`.
    pub fn search(&self, re: &Regex, pos: usize) -> Option<Hit> {
        let pos = pos.min(self.a.len());
        re.captures_at(&self.a, pos).map(|c| Hit::new(&c, 0))
    }

    /// Python's `pattern.finditer(text)`.
    pub fn find_iter(&self, re: &Regex) -> Vec<Hit> {
        re.captures_iter(&self.a).map(|c| Hit::new(&c, 0)).collect()
    }
}

/// Python's `str.expandtabs(4)`: a tab advances to the next multiple of four,
/// and a line break restarts the count.
pub fn expand_tabs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0;
    for ch in s.chars() {
        match ch {
            '\t' => {
                let pad = 4 - col % 4;
                out.extend(std::iter::repeat_n(' ', pad));
                col += pad;
            }
            '\n' | '\r' => {
                out.push(ch);
                col = 0;
            }
            _ => {
                out.push(ch);
                col += 1;
            }
        }
    }
    out
}

/// The next tab stop after column `col`.
pub const fn next_tab_stop(col: usize) -> usize {
    (col / 4 + 1) * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ascii_view_has_one_byte_per_character() {
        let t = Text::new("a\u{a0}\u{661}é\x01\t");
        assert_eq!(t.len(), 6);
        assert_eq!(t.view(), "a\x1c\x01\x7f\x02\t");
    }

    #[test]
    fn whitespace_follows_python() {
        for c in ['\u{85}', '\u{a0}', '\u{2028}', '\u{3000}'] {
            assert_eq!(class_byte(c), 0x1c, "{c:?}");
        }
        assert_eq!(
            class_byte('\x1f'),
            0x1f,
            "ASCII separators keep their own byte"
        );
        assert_eq!(
            class_byte('\u{200b}'),
            0x7f,
            "a zero-width space is not whitespace"
        );
    }

    #[test]
    fn find_and_rfind_follow_python_slices() {
        let t = Text::new("ab\ncd\nef");
        assert_eq!(t.find("\n", 0, 8), Some(2));
        assert_eq!(t.find("cd", 0, 4), None, "the needle must end before `end`");
        assert_eq!(t.rfind_char('\n', 0, 5), Some(2));
        assert_eq!(t.rfind_char('\n', 0, 2), None);
        assert_eq!(t.line_end(3), 5);
        assert_eq!(t.line_end(6), 8);
        assert_eq!(t.slice(6, 99), "ef");
        assert_eq!(t.find("x", 9, 3), None);
    }

    #[test]
    fn expand_tabs_follows_python() {
        assert_eq!(expand_tabs("-\tx"), "-   x");
        assert_eq!(expand_tabs("10.\tx"), "10. x");
        assert_eq!(expand_tabs("ab\r\tc"), "ab\r    c");
        assert_eq!(next_tab_stop(0), 4);
        assert_eq!(next_tab_stop(4), 8);
    }

    #[test]
    fn matches_are_anchored_at_the_position() {
        let re = Regex::new(r"\A\[(a)\]").unwrap();
        let t = Text::new("é[a]");
        let hit = t.match_at(&re, 1).unwrap();
        assert_eq!((hit.start(), hit.end(), hit.group(1)), (1, 4, Some((2, 3))));
        assert!(t.match_at(&re, 0).is_none());
    }
}
