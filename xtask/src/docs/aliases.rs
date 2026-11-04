//! Reader-vocabulary gate: the page that documents a capability must carry
//! the word a reader searches for it by.
//!
//! The other docs gates check that what a page says is true, and each
//! presupposes a reader who already reached the page. Nobody reads their way
//! down a 150-page index; they type the word they already have. A page that
//! never uses that word produces no 404 and no error: the reader concludes the
//! framework lacks the feature.
//!
//! The baseline defect: `autumn generate auth User --totp` ships two-factor
//! authentication, documented accurately in `docs/guide/authentication.md`,
//! which called it "TOTP" and "multi-factor" and never "2FA" or "two-factor".
//! A corpus-wide grep for the reader's words found four hits and no answer:
//! two were URL-encoded slashes (`%2F` followed by `A`), one an example URL on
//! the rate-limiting page, one an agent skill file.
//!
//! # What it checks
//!
//! For each row of [`READER_VOCABULARY`], the named page must carry every one
//! of the row's terms in its rendered text. Presence elsewhere in the corpus
//! does not count. The table is declared, not discovered: no mechanism knows
//! that "2FA" is the reader's word for `--totp`, so a human decides once and
//! the gate holds it.
//!
//! A term counts only if it survives into rendered text ([`prose`]): HTML
//! comments, `<script>`/`<style>` bodies, reference definitions, link
//! destinations, full-reference labels, HTML tags and their attributes, and
//! fence info strings are dropped, and `%XX` escapes become `/`. Link text,
//! inner text, code spans, fence bodies and autolink URLs are kept, because
//! they render.
//!
//! A row passing here means the word is on the page, not that a search ranks
//! the page for it: `check-docs-retrieval.sh` searches only the slug, H1 and
//! headings, and `docs/guide/authentication.md` has no heading for 2FA.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;

use crate::docs::re::pattern;

/// One capability: its label, the page that documents it, and the reader's
/// words for it, every one of which must appear.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub label: &'static str,
    pub page: &'static str,
    pub terms: &'static [&'static str],
}

/// The reader's vocabulary, not the codebase's.
///
/// A row earns its place when the two differ (`--totp` and "2FA"); rows where
/// they agree are kept as tripwires against a retitle into internal
/// vocabulary. Every term in a row is required, because a reader searches one
/// word, not an alternation, so a label must name exactly what its terms
/// enforce. A single term may still spell one idea several ways
/// (`password reset` or `reset password`). Terms are case-insensitive regexes.
pub const READER_VOCABULARY: &[Row] = &[
    Row {
        label: "two-factor authentication (2FA)",
        page: "docs/guide/authentication.md",
        terms: &[r"\b2fa\b", r"\btwo[- ]factor\b"],
    },
    Row {
        label: "sign-up",
        page: "docs/guide/authentication.md",
        terms: &[r"\bsign[- ]?up\b"],
    },
    Row {
        label: "password reset",
        page: "docs/guide/authentication.md",
        terms: &[
            r"\bforgot[- ]password\b",
            r"\bpassword reset\b|\breset password\b",
        ],
    },
    Row {
        label: "passwordless sign-in (magic link)",
        page: "docs/guide/authentication.md",
        terms: &[r"\bpasswordless\b", r"\bmagic[- ]link\b", r"\bmagic link\b"],
    },
    Row {
        label: "passkeys / WebAuthn",
        page: "docs/guide/authentication.md",
        terms: &[r"\bpasskey", r"\bwebauthn\b"],
    },
    Row {
        label: "OAuth sign-in",
        page: "docs/guide/oauth.md",
        terms: &[r"\boauth\b"],
    },
    Row {
        label: "CSRF protection",
        page: "docs/guide/middleware.md",
        terms: &[r"\bcsrf\b"],
    },
    Row {
        label: "CORS",
        page: "docs/guide/middleware.md",
        terms: &[r"\bcors\b"],
    },
    Row {
        label: "rate limiting",
        page: "docs/guide/rate-limiting.md",
        terms: &[r"\brate[- ]limit"],
    },
    Row {
        label: "background jobs",
        page: "docs/guide/jobs.md",
        terms: &[r"\bbackground job"],
    },
    Row {
        label: "cron",
        page: "docs/guide/jobs.md",
        terms: &[r"\bcron\b"],
    },
    Row {
        label: "websockets",
        page: "docs/guide/websockets.md",
        terms: &[r"\bwebsocket"],
    },
    Row {
        label: "full-text search",
        page: "docs/guide/full-text-search.md",
        terms: &[r"\bfull[- ]text search\b"],
    },
    Row {
        label: "file upload",
        page: "docs/guide/forms.md",
        terms: &[r"\bfile upload\b"],
    },
    Row {
        label: "S3 storage",
        page: "docs/guide/storage.md",
        terms: &[r"\bs3\b"],
    },
    Row {
        label: "database migrations",
        page: "docs/guide/migrations.md",
        terms: &[r"\bmigration"],
    },
    Row {
        label: "testing",
        page: "docs/guide/testing.md",
        terms: &[r"\btest"],
    },
    Row {
        label: "deployment",
        page: "docs/guide/deployment.md",
        terms: &[r"\bdeploy"],
    },
    Row {
        label: "liveness / readiness probes",
        page: "docs/guide/cloud-native.md",
        terms: &[
            r"\bliveness probe\b",
            r"\breadiness probe\b",
            r"\bstartup probe\b",
        ],
    },
];

pattern! { static PERCENT_ESCAPE = r"%[0-9A-Fa-f]{2}"; }

pattern! {
    /// The constructs that decide what a region is. One scan, in document
    /// order: whichever opens first wins, as in a renderer. Running the rules
    /// in sequence makes the answer depend on their order, and neither order
    /// is right (a comment a fence displays, a backtick inside a comment).
    static NEXT_CONSTRUCT = concat!(
        r"(?mi)(?P<comment><!--)",
        r"|(?P<raw><(?P<rawname>script|style)\b)",
        r"|(?P<fence>^[ \t]{0,3}(?P<frun>`{3,}|~{3,})[^\n]*$)",
        r"|(?P<crun>`+)",
    );
}

pattern! {
    /// A reference definition, with a title that may sit on the next line.
    /// Python's `\S` also excludes `\x1c`-`\x1f`.
    static REF_DEFINITION = concat!(
        r"(?m)^[ \t]*\[[^\]]+\]:[ \t]*[^\s\x1c-\x1f]+[^\n]*",
        r#"(?:\n[ \t]+(?:"[^"\n]*"|'[^'\n]*'|\([^)\n]*\))[ \t]*)?"#,
    );
}

pattern! {
    /// An autolink, whose URL is its label: the brackets go and the content
    /// stays. Both forms: an absolute URI with any scheme, and a bare email.
    static AUTOLINK = concat!(
        r"<(",
        r"[a-zA-Z][a-zA-Z0-9+.-]{1,31}:[^<>\s\x1c-\x1f]*",
        r"|[^\s\x1c-\x1f<>@]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
        r")>",
    );
}

pattern! {
    /// An HTML tag, ending at the first `>` outside a quoted attribute value.
    static HTML_TAG = r#"</?[A-Za-z][A-Za-z0-9:-]*(?:[^>"']|"[^"]*"|'[^']*')*>"#;
}

/// How far a link destination is scanned: a destination is not a paragraph.
const DEST_SCAN_LIMIT: usize = 500;

/// Remove `](…)` destinations, honouring nested and escaped parentheses.
///
/// An unbalanced `](` is left alone: malformed markdown renders literally, so
/// its text is on the page, and a stray `](` must not swallow the file.
fn strip_link_destinations(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    loop {
        let Some(close) =
            (at..len.saturating_sub(1)).find(|&j| chars[j] == ']' && chars[j + 1] == '(')
        else {
            out.extend(&chars[at.min(len)..]);
            return out;
        };
        out.extend(&chars[at..=close]);
        let mut scan = close + 2;
        let mut depth = 1usize;
        let limit = len.min(scan + DEST_SCAN_LIMIT);
        while scan < limit && depth > 0 {
            match chars[scan] {
                '\\' => {
                    scan += 2;
                    continue;
                }
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            scan += 1;
        }
        if depth > 0 {
            out.push('(');
            at = close + 2;
        } else {
            out.push(' ');
            at = scan;
        }
    }
}

/// Remove the label of a full reference, `[text][label]`: only the first
/// bracket renders. The label must sit immediately after a `]`; `[a] [b]` is
/// two shortcut references, both visible.
fn strip_ref_labels(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut last, mut p) = (0, 0);
    while p < b.len() {
        if b[p] == b'['
            && p > 0
            && b[p - 1] == b']'
            && let Some(q) = b[p + 1..].iter().position(|&x| x == b'[' || x == b']')
            && b[p + 1 + q] == b']'
        {
            let end = p + 1 + q + 1;
            out.push_str(&text[last..p]);
            out.push(' ');
            last = end;
            p = end;
            continue;
        }
        p += 1;
    }
    out.push_str(&text[last..]);
    out
}

/// A prose region with the markup that carries invisible text removed.
fn strip_markup(chunk: &str) -> String {
    let chunk = REF_DEFINITION.replace_all(chunk, " ");
    let chunk = AUTOLINK.replace_all(&chunk, " ${1} ");
    // Destinations before tags, since `](…)` may hold `<…>`; labels after,
    // since `][label]` needs its `]`.
    let chunk = strip_link_destinations(&chunk);
    let chunk = strip_ref_labels(&chunk);
    HTML_TAG.replace_all(&chunk, " ").into_owned()
}

/// A region of a document, by how it renders.
#[derive(Debug, PartialEq, Eq)]
enum Segment<'t> {
    /// Rendered as code: kept whole.
    Code(std::borrow::Cow<'t, str>),
    /// Ordinary markdown: its markup is stripped.
    Prose(&'t str),
    /// Never rendered.
    Drop,
}

/// The first backtick run at or after `from` that is exactly `len` long.
fn closing_run(text: &str, from: usize, len: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut p = from;
    while p < b.len() {
        if b[p] != b'`' {
            p += 1;
            continue;
        }
        let start = p;
        while p < b.len() && b[p] == b'`' {
            p += 1;
        }
        if p - start == len && (start == 0 || b[start - 1] != b'`') {
            return Some(start);
        }
    }
    None
}

/// Walk the document once, splitting it into code, prose and dropped
/// regions; at each position the construct that starts first wins.
fn segments(text: &str) -> Vec<Segment<'_>> {
    let n = text.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let Some(c) = NEXT_CONSTRUCT.captures_at(text, i) else {
            out.push(Segment::Prose(&text[i..]));
            break;
        };
        let (start, end) = (
            c.get(0).map_or(i, |m| m.start()),
            c.get(0).map_or(i, |m| m.end()),
        );
        if start > i {
            out.push(Segment::Prose(&text[i..start]));
        }
        if c.name("comment").is_some() {
            out.push(Segment::Drop);
            i = text[end..].find("-->").map_or(n, |e| end + e + 3);
            continue;
        }
        if let Some(name) = c.name("rawname") {
            // A raw-text body's backticks are not code spans and its `<!--`
            // is not a comment, so the scanner consumes it whole.
            let closed = Regex::new(&format!(r"(?i)</{}\b", name.as_str()))
                .ok()
                .and_then(|re| re.find_at(text, end));
            out.push(Segment::Drop);
            i = closed
                .and_then(|cm| text[cm.end()..].find('>').map(|e| cm.end() + e + 1))
                .unwrap_or(n);
            continue;
        }
        if let Some(run) = c.name("frun") {
            // A fence closes on a run of the same character at least as long.
            // Its info string and closing delimiter are metadata, never text.
            let run = run.as_str();
            let ch = &run[..1];
            let close = Regex::new(&format!(
                r"(?m)^[ \t]{{0,3}}{}{{{},}}[ \t]*$",
                regex::escape(ch),
                run.len()
            ))
            .ok()
            .and_then(|re| re.find_at(text, end));
            let stop = close.map_or(n, |cm| cm.end());
            let mut body: Vec<&str> = text[start..stop].split('\n').skip(1).collect();
            if close.is_some() && !body.is_empty() {
                body.pop();
            }
            out.push(Segment::Code(body.join("\n").into()));
            i = stop;
            continue;
        }
        // An inline code span: it closes on a run of the same length, not
        // across a blank line; unclosed, its backticks are literal.
        let run_len = end - start;
        match closing_run(text, end, run_len) {
            Some(cs) if !text[end..cs].contains("\n\n") => {
                out.push(Segment::Code(text[start..cs + run_len].into()));
                i = cs + run_len;
            }
            _ => {
                out.push(Segment::Prose(&text[start..end]));
                i = end;
            }
        }
    }
    out
}

/// What a reader sees of `text`: code kept, prose stripped of invisible
/// markup, unrendered regions dropped, and `%XX` escapes turned into `/` so
/// `%2FA` is not "2FA".
#[must_use]
pub fn prose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for seg in segments(text) {
        match seg {
            Segment::Code(chunk) => out.push_str(&chunk),
            Segment::Prose(chunk) => out.push_str(&strip_markup(chunk)),
            Segment::Drop => out.push(' '),
        }
    }
    PERCENT_ESCAPE.replace_all(&out, "/").into_owned()
}

/// `text` as an ASCII term sees it under Python's `re.IGNORECASE` and `\b`.
///
/// Rust's `\b` counts a combining mark as a word character and Python's does
/// not, and Python counts `²` (category `No`) where Rust does not. Mapping
/// every non-ASCII character to an ASCII stand-in makes both engines agree:
/// a Python word character (category `L` or `N`) becomes `_`, which no term
/// contains, anything else becomes `\x01`, and the four characters Python
/// folds onto an ASCII letter (`İ`, `ı`, `ſ`, the Kelvin sign) become that
/// letter. Terms are ASCII, which a test holds.
fn term_view(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            _ if c.is_ascii() => c,
            '\u{130}' | '\u{131}' => 'i',
            '\u{17f}' => 's',
            '\u{212a}' => 'k',
            _ if is_python_word(c) => '_',
            _ => '\x01',
        })
        .collect()
}

/// Python's `\w` for a non-ASCII character: a letter or a number.
fn is_python_word(c: char) -> bool {
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

/// One missing word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defect {
    pub label: &'static str,
    pub page: &'static str,
    pub term: &'static str,
    pub why: &'static str,
}

const ABSENT: &str = "reader word absent";
const MISSING_PAGE: &str = "page does not exist";

/// Check `rows`, reading pages through `read` and `exists`. Returns the
/// defects and how many pages were read.
///
/// # Errors
///
/// Returns an error when a page cannot be read or a term is not a valid
/// pattern.
pub fn check_rows(
    rows: &[Row],
    read: &dyn Fn(&str) -> Result<String>,
    exists: &dyn Fn(&str) -> bool,
) -> Result<(Vec<Defect>, usize)> {
    let mut defects = Vec::new();
    let mut checked = 0;
    for row in rows {
        if !exists(row.page) {
            defects.push(Defect {
                label: row.label,
                page: row.page,
                term: row.terms[0],
                why: MISSING_PAGE,
            });
            continue;
        }
        checked += 1;
        let visible = term_view(&prose(&read(row.page)?));
        for &term in row.terms {
            let re = Regex::new(&format!("(?i){term}"))
                .with_context(|| format!("term `{term}` for {}", row.label))?;
            if !re.is_match(&visible) {
                defects.push(Defect {
                    label: row.label,
                    page: row.page,
                    term,
                    why: ABSENT,
                });
            }
        }
    }
    Ok((defects, checked))
}

/// Read a page as Python's `open(path, errors="replace")` does: invalid UTF-8
/// replaced, and `\r\n` and `\r` read as `\n`.
fn read_page(root: &Path, page: &str) -> Result<String> {
    let path = root.join(page);
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(String::from_utf8_lossy(&raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n"))
}

/// Run the gate over the checkout at `root` and print its report.
///
/// # Errors
///
/// Returns an error when a page cannot be read, or on a failed write.
pub fn run(root: &Path, out: &mut impl Write) -> Result<bool> {
    writeln!(
        out,
        "Checking that capability pages carry the words readers search by..."
    )?;
    let (defects, checked) =
        check_rows(READER_VOCABULARY, &|page| read_page(root, page), &|page| {
            root.join(page).exists()
        })?;
    let terms: usize = READER_VOCABULARY.iter().map(|r| r.terms.len()).sum();
    writeln!(
        out,
        "vocabulary rows: {} ({terms} required terms, {checked} pages read)",
        READER_VOCABULARY.len()
    )?;
    writeln!(out, "defects: {}", defects.len())?;
    if defects.is_empty() {
        writeln!(out, "Reader-vocabulary gate OK.")?;
        return Ok(true);
    }
    writeln!(out)?;
    writeln!(
        out,
        "A reader searching for this capability by its common name does not"
    )?;
    writeln!(out, "land on the page that answers them:")?;
    for d in &defects {
        writeln!(out, "  {}: {} -- {}", d.page, d.why, d.label)?;
        writeln!(out, "    expected to match: {}", d.term)?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "Fix by putting the reader's word on that page (a retitle, a sentence"
    )?;
    writeln!(
        out,
        "in the intro, or the capability named in its own table row) -- NOT by"
    )?;
    writeln!(
        out,
        "writing a new page: that splits the search rank and the two copies"
    )?;
    writeln!(
        out,
        "drift apart. If the capability genuinely moved, update the row."
    )?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    //! The script's self-test, one test per property.

    use super::*;

    const ROW: &[Row] = &[Row {
        label: "2FA",
        page: "p.md",
        terms: &[r"\b2fa\b|\btwo[- ]factor\b"],
    }];

    const TWO: &[Row] = &[Row {
        label: "2FA",
        page: "p.md",
        terms: &[r"\b2fa\b", r"\btwo[- ]factor\b"],
    }];

    fn check_text(rows: &[Row], text: &str) -> Vec<Defect> {
        let text = text.to_owned();
        check_rows(rows, &|_| Ok(text.clone()), &|_| true)
            .unwrap()
            .0
    }

    fn one(text: &str) -> Vec<Defect> {
        check_text(ROW, text)
    }

    fn absent(text: &str) -> bool {
        let d = one(text);
        d.len() == 1 && d[0].why == ABSENT
    }

    /// The row's pattern matches the raw text, so only [`prose`] stands
    /// between the input and a false pass.
    fn raw_matches(text: &str) -> bool {
        Regex::new(&format!("(?i){}", ROW[0].terms[0]))
            .unwrap()
            .is_match(text)
    }

    #[test]
    fn a_page_missing_the_reader_word_is_a_defect() {
        assert!(absent("TOTP enrollment and multi-factor login."));
    }

    #[test]
    fn a_percent_escape_does_not_satisfy_a_row() {
        assert!(absent("see path=%2FA then stop"));
        assert!(
            Regex::new(r"(?i)\b2fa\b")
                .unwrap()
                .is_match("see path=%2FA then stop")
        );
    }

    #[test]
    fn a_page_carrying_the_reader_word_passes() {
        assert!(one("Two-factor authentication (2FA) via TOTP.").is_empty());
    }

    #[test]
    fn a_missing_page_is_a_defect_not_a_crash() {
        let rows = [Row {
            label: "x",
            page: "does/not/exist.md",
            terms: &["x"],
        }];
        let (d, checked) = check_rows(&rows, &|_| unreachable!(), &|_| false).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].why, MISSING_PAGE);
        assert_eq!(checked, 0);
    }

    #[test]
    fn every_term_in_a_row_is_required() {
        let d = check_text(TWO, "Enable 2FA with TOTP.");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].term, r"\btwo[- ]factor\b");
        assert!(check_text(TWO, "Two-factor auth (2FA) via TOTP.").is_empty());
    }

    #[test]
    fn a_term_that_never_renders_does_not_count() {
        for (what, text) in [
            (
                "an HTML comment",
                "<!-- drift-allow: 2FA is covered -->\nTOTP setup.",
            ),
            ("a multi-line HTML comment", "<!--\n2FA\n-->\nTOTP setup."),
            (
                "a link destination",
                "See [the flow](./two-factor-setup.md) for TOTP.",
            ),
            (
                "a reference definition",
                "See [flow][f].\n\n[f]: ./2fa-guide.md\n",
            ),
        ] {
            assert!(absent(text), "{what} must not satisfy a reader-word row");
            assert!(
                raw_matches(text),
                "precondition: raw {what} should match the row"
            );
        }
    }

    #[test]
    fn what_markup_wraps_is_visible() {
        for (what, text) in [
            ("link text", "See [two-factor setup](./totp.md)."),
            (
                "HTML inner text",
                "<span class=\"x\">two-factor</span> setup.",
            ),
            (
                "a fenced code block",
                "Setup:\n\n```sh\n# enable two-factor\n```\n",
            ),
            (
                "an inline code span",
                "Run `--totp` for `two-factor` login.",
            ),
        ] {
            assert!(
                one(text).is_empty(),
                "{what} is reader-visible and should satisfy a row"
            );
        }
    }

    #[test]
    fn an_html_attribute_is_not_rendered_text() {
        assert!(absent("<span id=\"2fa\">TOTP</span> enrollment."));
        assert!(raw_matches("<span id=\"2fa\">TOTP</span>"));
    }

    #[test]
    fn a_balanced_paren_destination_is_still_a_destination() {
        let nested = "See [flow](./guide_(v1)/two-factor) and TOTP.";
        assert!(absent(nested));
        let naive = Regex::new(r"\]\([^)]*\)").unwrap().replace_all(nested, " ");
        assert!(
            raw_matches(&naive),
            "precondition: the naive regex leaks the tail"
        );
    }

    #[test]
    fn an_escaped_paren_does_not_close_a_destination() {
        assert!(absent(r"See [flow](./a\)two-factor) and TOTP."));
    }

    #[test]
    fn an_unbalanced_destination_renders_literally_and_counts() {
        assert!(one("Stray ](./two-factor and more prose.").is_empty());
    }

    #[test]
    fn a_comment_containing_backticks_is_still_a_comment() {
        assert!(absent(
            "<!-- `2FA` and `two-factor` wording -->\nTOTP enrollment."
        ));
    }

    #[test]
    fn a_fence_closes_on_a_longer_run() {
        assert!(one("Text.\n\n```\n[flow](./two-factor)\n````\n").is_empty());
        assert!(
            one("````\n```\ntwo-factor\n````\n").is_empty(),
            "a shorter run inside a longer fence is content"
        );
    }

    #[test]
    fn a_reference_definition_title_on_the_next_line_does_not_count() {
        assert!(absent("See [f].\n\n[f]: /totp\n    \"two-factor setup\"\n"));
    }

    #[test]
    fn a_quoted_angle_bracket_does_not_end_a_tag() {
        assert!(absent("<span id=\">two-factor\">TOTP</span> enrollment."));
    }

    #[test]
    fn script_and_style_bodies_never_render() {
        for tag in ["style", "script"] {
            assert!(
                absent(&format!(
                    "<{tag}>.two-factor {{ color: red }}</{tag}>\nTOTP setup."
                )),
                "a <{tag}> body must not satisfy a reader-word row"
            );
            assert!(
                absent(&format!(
                    "<{tag}>\nx = `two-factor`;\n</{tag}>\nTOTP setup."
                )),
                "a backticked <{tag}> body must not satisfy a row"
            );
        }
    }

    #[test]
    fn a_raw_text_element_inside_a_fence_is_visible_code() {
        assert!(one("Example:\n\n```html\n<style>.two-factor{}</style>\n```\n").is_empty());
    }

    #[test]
    fn an_autolink_url_is_its_label() {
        assert!(one("See <https://example.com/two-factor> for setup.").is_empty());
        assert!(absent(
            "See [the guide](https://example.com/two-factor) for setup."
        ));
    }

    #[test]
    fn both_autolink_forms_render() {
        for form in [
            "<two-factor@example.com>",
            "<mailto:help@example.com?subject=two-factor>",
        ] {
            assert!(one(&format!("Mail {form} to enrol.")).is_empty(), "{form}");
        }
        assert!(absent("<span data-x=\"two-factor\">TOTP</span>"));
    }

    #[test]
    fn a_full_reference_label_is_an_identifier() {
        assert!(absent("See [setup][two-factor].\n\n[two-factor]: ./totp\n"));
        assert!(one("See [two-factor setup][x].\n\n[x]: ./totp\n").is_empty());
        assert!(
            one("See [totp] [two-factor] both.\n\n[totp]: ./a\n[two-factor]: ./b\n").is_empty(),
            "a shortcut reference renders its label"
        );
    }

    #[test]
    fn a_fence_info_string_is_metadata() {
        assert!(absent("Example:\n\n```two-factor\nlet x = 1;\n```\n"));
        assert!(one("Example:\n\n```rust\n// two-factor setup\n```\n").is_empty());
    }

    #[test]
    fn segments_follow_document_order() {
        assert_eq!(
            segments("a <!-- x --> `c` d"),
            [
                Segment::Prose("a "),
                Segment::Drop,
                Segment::Prose(" "),
                Segment::Code("`c`".into()),
                Segment::Prose(" d"),
            ]
        );
        assert_eq!(prose("``a`` b ``unclosed"), "``a`` b ``unclosed");
        assert_eq!(
            prose("`a\n\nb`"),
            "`a\n\nb`",
            "a code span cannot cross a blank line"
        );
    }

    #[test]
    fn term_matching_follows_python_word_boundaries() {
        let hit = |term: &str, text: &str| {
            Regex::new(&format!("(?i){term}"))
                .unwrap()
                .is_match(&term_view(text))
        };
        assert!(
            hit(r"\b2fa\b", "\u{301}2fa "),
            "a combining mark is not a word character"
        );
        assert!(!hit(r"\bcron\b", "\u{b2}cron "), "`²` is a word character");
        assert!(hit(r"\btest", "te\u{17f}t"), "`ſ` folds to `s`");
        assert!(hit(r"\bs3\b", "S3"));
        assert!(!hit(r"\bs3\b", "éS3"));
    }

    #[test]
    fn every_term_is_ascii() {
        for row in READER_VOCABULARY {
            for term in row.terms {
                assert!(term.is_ascii(), "{}: `{term}` is not ASCII", row.label);
                assert!(
                    !term.contains('_'),
                    "{}: `_` stands in for non-ASCII letters",
                    row.label
                );
            }
        }
    }

    #[test]
    fn this_repository_passes() {
        let mut out = Vec::new();
        let passed = run(crate::repo_root(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(passed, "{out}");
        assert!(out.contains("vocabulary rows: 19"), "{out}");
    }
}
