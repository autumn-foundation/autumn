//! Acknowledgment: the human half of the gate.
//!
//! A widening posture blocks until someone says, on the pull request, "yes, I
//! meant that". The marker is a comment line:
//!
//! ```text
//! /ack-posture 3f2a91c0d47b5e6a  widening /admin/users on purpose, launch week
//! ```
//!
//! The digest binds the acknowledgment to **the exact set of widening
//! findings** it was written for. That is what makes the marker survive
//! unrelated pushes (the set is unchanged, so the digest is unchanged) while
//! re-blocking the moment a later commit widens something new (a new set is a
//! new digest, and no comment carries it yet).
//!
//! Everything parsed here came from a pull-request comment, which is to say
//! from anyone who can type. Two lines of defense: this parser is strict about
//! *shape*, and the workflow that harvests the comments is strict about *who* —
//! it passes on only comments whose author association is `OWNER`, `MEMBER` or
//! `COLLABORATOR`. This module trusts its input to have been filtered already
//! and says so out loud rather than pretending to an authorization model it has
//! no identity to enforce.

use super::diff::Finding;
use super::model::hex_digest;

/// The comment phrase that acknowledges a posture widening.
pub const ACK_PHRASE: &str = "/ack-posture";

/// Line the harvester writes between two comment bodies.
///
/// Fenced-code state must not leak from one comment into the next: a colleague
/// who pastes a log and forgets the closing fence would otherwise silently
/// swallow every later acknowledgment — or, worse, flip the parity so that a
/// marker *inside* a fence (the gate's own posted report contains one) becomes
/// live. Each body is parsed with its own fence state.
pub const SOURCE_SEPARATOR: &str = "<!-- autumn:ack-source -->";

/// How much of the digest the phrase carries. 64 bits is far more than enough
/// to bind an acknowledgment to a finding set nobody is trying to collide, and
/// short enough to read out loud.
pub const SHORT_DIGEST_LEN: usize = 16;

/// One acknowledgment marker found in the harvested text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acknowledgment {
    /// The digest exactly as written (lower-cased), 8..=64 hex characters.
    pub digest: String,
    /// Whatever the author wrote after the digest, when they wrote anything.
    pub reason: Option<String>,
}

/// The digest of a set of widening findings.
///
/// Order-independent: the findings are canonicalized, sorted and de-duplicated
/// first, so the digest describes the *set*, not the order the differ happened
/// to emit it in.
#[must_use]
pub fn ack_digest(widening: &[&Finding]) -> String {
    let mut lines: Vec<String> = widening.iter().map(|f| f.canonical()).collect();
    lines.sort();
    lines.dedup();
    hex_digest(lines.join("\n").as_bytes())
}

/// The first [`SHORT_DIGEST_LEN`] characters of a digest — what the phrase says.
#[must_use]
pub fn short(digest: &str) -> String {
    digest.chars().take(SHORT_DIGEST_LEN).collect()
}

/// Extract every acknowledgment marker from harvested pull-request text.
///
/// Strict on purpose:
/// - the phrase must start the line (leading whitespace allowed), so it cannot
///   be smuggled into the middle of a sentence;
/// - a quoted line (`>` …), and any line the quoted paragraph lazily runs on
///   into, never acknowledges anything, so quoting somebody else's comment —
///   which GitHub's own reply UI does automatically — does not silently
///   re-acknowledge a digest;
/// - a fenced code block is inert, so the documentation example in a comment
///   does not acknowledge anything either;
/// - the digest must be plain lower/upper-case hex, 8..=64 characters.
#[must_use]
pub fn parse_acks(text: &str) -> Vec<Acknowledgment> {
    let mut acks = Vec::new();
    // The character and length of the fence currently open, if any. Toggling a
    // bool instead would let the inner ``` of a report quoted inside a ````
    // block *close* the outer fence, making the quoted marker live — so
    // quoting an acknowledgment would acknowledge it.
    let mut fence: Option<(char, usize)> = None;
    // Inside a multi-line `<!-- … -->`, which GitHub renders as nothing.
    let mut html_comment = false;
    // Inside a block-quote paragraph. `CommonMark` continues one across a line
    // that carries no `>` of its own — a *lazy continuation* — and GitHub
    // renders that line as quoted too, so a marker there is discussion, not a
    // decision. Checking only the current line let it count as a grant.
    let mut quote_paragraph = false;
    for line in text.lines() {
        if line.trim() == SOURCE_SEPARATOR {
            // A new comment body starts here, with a clean slate. Checked
            // first, because the separator is itself an HTML comment.
            fence = None;
            html_comment = false;
            quote_paragraph = false;
            continue;
        }
        // A blank line ends the quoted paragraph, so the lazy continuation
        // stops here and the rest of the comment is live text again. Read from
        // the raw line: a line that renders empty because it is all HTML
        // comment is not a paragraph break.
        if line.trim().is_empty() {
            quote_paragraph = false;
            continue;
        }
        // An acknowledgment has to be *visible* on the pull request — that is
        // the whole value of putting it there. Text inside an HTML comment
        // renders as nothing, so it acknowledges nothing.
        //
        // Inside a fenced block, though, the text is literal: a line that puts
        // an HTML comment before a fence run is content, because a closing
        // fence carries no prefix. Stripping the comment there would uncover
        // a bare run that closed the block early,
        // making everything after it live while GitHub still drew it as code.
        // So strip only outside a fence — and an `<!--` inside one opens
        // nothing, for the same reason.
        let visible = if fence.is_some() {
            line.to_owned()
        } else {
            strip_html_comments(line, &mut html_comment)
        };
        if visible.trim().is_empty() {
            continue;
        }
        let line = visible.as_str();
        // Markdown's other code block: four spaces (or a tab) of indentation
        // renders as code, so a marker written that way is a sample, not a
        // grant. Skipping is the safe direction — the worst case is a reviewer
        // re-posting an unindented line, never an acknowledgment nobody meant.
        if fence.is_none() && is_indented_code(line) {
            continue;
        }
        let indent = leading_indent(line);
        let trimmed = line.trim_start();
        match (fence, fence_run(trimmed)) {
            // A fence closes only with the same character, at least as long,
            // nothing after it, and at most three spaces of indentation —
            // `CommonMark`'s rule. The indent matters: four spaces makes the
            // line *content* of the block, so GitHub keeps drawing the code
            // block, and closing there would make every following line live
            // while the reviewer still sees it as a sample.
            (Some((open_char, open_len)), Some((c, len, bare)))
                if c == open_char && len >= open_len && bare && indent <= 3 =>
            {
                fence = None;
                continue;
            }
            (None, Some((c, len, _))) => {
                fence = Some((c, len));
                // A fence can interrupt a paragraph, quoted or not.
                quote_paragraph = false;
                continue;
            }
            _ => {}
        }
        if trimmed.starts_with('>') {
            quote_paragraph = true;
            continue;
        }
        if fence.is_some() || quote_paragraph {
            continue;
        }
        if let Some(ack) = parse_marker(trimmed) {
            acks.push(ack);
        }
    }
    acks
}

/// The part of `line` GitHub actually renders, with any `<!-- … -->` removed.
///
/// `open` carries the state across lines, since a comment may span several.
fn strip_html_comments(line: &str, open: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        if *open {
            match rest.find("-->") {
                Some(end) => {
                    *open = false;
                    rest = &rest[end + 3..];
                }
                None => return out,
            }
        } else if let Some(start) = rest.find("<!--") {
            out.push_str(&rest[..start]);
            *open = true;
            rest = &rest[start + 4..];
        } else {
            out.push_str(rest);
            return out;
        }
    }
}

/// How many columns of indentation `line` starts with, tabs counted as four.
///
/// Only ever compared against `CommonMark`'s three-space allowance for a fence,
/// and only ever to *refuse* to close one, so counting a tab as a full stop
/// rather than to the next one errs toward leaving the block open — which
/// withholds an acknowledgment instead of granting one.
fn leading_indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Whether `line` is indented enough to render as a Markdown code block.
///
/// Four spaces, or one tab. Deliberately not a full `CommonMark` block parser:
/// this only ever *withholds* an acknowledgment, so over-matching costs a
/// reviewer one re-post while under-matching would grant something nobody
/// asked for.
fn is_indented_code(line: &str) -> bool {
    line.starts_with("    ") || line.starts_with('\t')
}

/// The leading fence run of `line`, as `(character, length, nothing follows)`.
///
/// `None` when the line does not open or close a fence at all. The third
/// element separates a closing fence (bare) from an opening one that carries
/// an info string, such as a backtick run followed by `rust`.
fn fence_run(line: &str) -> Option<(char, usize, bool)> {
    let c = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = line.chars().take_while(|ch| *ch == c).count();
    if len < 3 {
        return None;
    }
    let rest = line.chars().skip(len).collect::<String>();
    Some((c, len, rest.trim().is_empty()))
}

/// Parse one already-trimmed, already-vetted line.
///
/// `str::get` rather than slicing: the line came from a comment box, so it may
/// well start with an emoji, and a byte-index slice through one panics.
fn parse_marker(line: &str) -> Option<Acknowledgment> {
    let head = line.get(..ACK_PHRASE.len())?;
    if !head.eq_ignore_ascii_case(ACK_PHRASE) {
        return None;
    }
    let rest = line.get(ACK_PHRASE.len()..)?;
    // The phrase must be a whole word: `/ack-posture-later …` is not one.
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let (token, reason) = rest
        .find(char::is_whitespace)
        .map_or((rest, ""), |i| (&rest[..i], rest[i..].trim()));
    let digest = token.to_ascii_lowercase();
    if !is_digest(&digest) {
        return None;
    }
    Some(Acknowledgment {
        digest,
        reason: (!reason.is_empty()).then(|| reason.to_owned()),
    })
}

/// Whether `candidate` is a plausible digest: hex, and long enough to be worth
/// parsing at all. Whether it is long enough to *match* is
/// [`crate::posture::verify::digest_matches`]'s call.
fn is_digest(candidate: &str) -> bool {
    (8..=64).contains(&candidate.len()) && candidate.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether any harvested acknowledgment matches `digest`.
///
/// A marker may carry the short form or the full digest; both are compared
/// case-insensitively against the expected digest's corresponding prefix.
#[must_use]
pub fn matching<'a>(acks: &'a [Acknowledgment], digest: &str) -> Option<&'a Acknowledgment> {
    acks.iter()
        .find(|ack| super::verify::marker_matches(digest, &ack.digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::posture::diff::Severity;

    fn finding(kind: &'static str, path: &str) -> Finding {
        Finding {
            kind,
            severity: Severity::Widening,
            method: "GET".to_owned(),
            path: path.to_owned(),
            before: "gated (roles: admin)".to_owned(),
            after: "public".to_owned(),
            fingerprint: "class:gated->public".to_owned(),
            detail: "d".to_owned(),
        }
    }

    // ── digest ──────────────────────────────────────────────────────────────

    #[test]
    fn digest_is_stable_across_finding_order() {
        let a = finding("classification_downgraded", "/a");
        let b = finding("route_added_open", "/b");
        assert_eq!(ack_digest(&[&a, &b]), ack_digest(&[&b, &a]));
    }

    #[test]
    fn a_new_widening_changes_the_digest() {
        let a = finding("classification_downgraded", "/a");
        let b = finding("route_added_open", "/b");
        assert_ne!(ack_digest(&[&a]), ack_digest(&[&a, &b]));
    }

    #[test]
    fn short_digest_is_sixteen_hex_characters() {
        let a = finding("classification_downgraded", "/a");
        let d = ack_digest(&[&a]);
        assert_eq!(short(&d).len(), SHORT_DIGEST_LEN);
        assert!(d.starts_with(&short(&d)));
        assert!(short(&d).chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── parsing ─────────────────────────────────────────────────────────────

    #[test]
    fn parses_a_bare_marker() {
        let acks = parse_acks("/ack-posture 0123456789abcdef");
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].digest, "0123456789abcdef");
        assert_eq!(acks[0].reason, None);
    }

    #[test]
    fn parses_a_marker_with_a_reason() {
        let acks = parse_acks("  /ack-posture 0123456789ABCDEF  launch week, intentional\n");
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].digest, "0123456789abcdef", "digest is normalized");
        assert_eq!(acks[0].reason.as_deref(), Some("launch week, intentional"));
    }

    #[test]
    fn parses_several_markers_across_harvested_comments() {
        let text = "first comment\n/ack-posture aaaaaaaaaaaaaaaa\n---\n/ack-posture bbbbbbbbbbbbbbbb why not\n";
        let acks = parse_acks(text);
        assert_eq!(acks.len(), 2);
        assert_eq!(acks[1].digest, "bbbbbbbbbbbbbbbb");
    }

    #[test]
    fn a_quoted_marker_acknowledges_nothing() {
        assert!(parse_acks("> /ack-posture 0123456789abcdef").is_empty());
        assert!(parse_acks(">> /ack-posture 0123456789abcdef").is_empty());
    }

    /// Inside a fenced block the text is literal, so a line that puts an HTML
    /// comment before a fence run is content: a closing fence carries no
    /// prefix. Stripping the comment first left a bare run that did close it,
    /// making everything after live while GitHub still drew the code block.
    #[test]
    fn an_html_comment_does_not_uncover_a_closing_fence() {
        let text = "```\n<!-- note --> ```\n/ack-posture 0123456789abcdef\n";
        assert!(parse_acks(text).is_empty(), "{:?}", parse_acks(text));
    }

    /// Outside a fence the stripping still applies, and a marker after a
    /// properly closed block still counts.
    #[test]
    fn a_marker_after_a_block_that_really_closed_still_acknowledges() {
        let text = "```\n<!-- note --> ```\n```\n/ack-posture 0123456789abcdef  yes\n";
        let acks = parse_acks(text);
        assert_eq!(acks.len(), 1, "{acks:?}");
    }

    /// A closing fence may be indented at most three spaces. Four or more
    /// makes the line ordinary *content* of the block, so GitHub keeps drawing
    /// the code block — and a parser that closed there would treat everything
    /// after it as live text while the reviewer sees it inside the block.
    #[test]
    fn an_over_indented_fence_does_not_close_the_block() {
        let text = "```\n    ```\n/ack-posture 0123456789abcdef\n";
        assert!(parse_acks(text).is_empty(), "{:?}", parse_acks(text));
        let tab = "```\n\t```\n/ack-posture 0123456789abcdef\n";
        assert!(parse_acks(tab).is_empty(), "{:?}", parse_acks(tab));
    }

    /// Three spaces is still a fence, though: over-matching here would swallow
    /// a real acknowledgment written under a slightly indented code sample.
    #[test]
    fn a_fence_indented_three_spaces_still_closes_the_block() {
        let text = "```\n   ```\n/ack-posture 0123456789abcdef  yes\n";
        let acks = parse_acks(text);
        assert_eq!(acks.len(), 1, "{acks:?}");
    }

    /// `CommonMark` continues a block-quote paragraph across a line that
    /// carries no `>` of its own, so GitHub renders this whole thing as quoted
    /// discussion. Checking only the current line let the second line count as
    /// a live grant — an acknowledgment nobody can see was given.
    #[test]
    fn a_lazy_continuation_of_a_quote_acknowledges_nothing() {
        let text = "> Do not use this marker:\n/ack-posture 0123456789abcdef\n";
        assert!(parse_acks(text).is_empty(), "{:?}", parse_acks(text));
    }

    /// The continuation ends where the quoted paragraph does. A blank line
    /// closes it, and the marker after it is an ordinary comment line again —
    /// otherwise quoting anything would poison the rest of the comment.
    #[test]
    fn a_marker_after_the_quote_ends_still_acknowledges() {
        let text = "> some quoted context\n\n/ack-posture 0123456789abcdef  yes\n";
        let acks = parse_acks(text);
        assert_eq!(acks.len(), 1, "{acks:?}");
        assert_eq!(acks[0].digest, "0123456789abcdef");
    }

    /// A new comment body starts with a clean slate here too: an unterminated
    /// quote in one comment must not swallow the next comment's marker.
    #[test]
    fn a_quote_does_not_leak_across_harvested_comments() {
        let text =
            format!("> quoted tail\n{SOURCE_SEPARATOR}\n/ack-posture 0123456789abcdef  yes\n");
        assert_eq!(parse_acks(&text).len(), 1, "{:?}", parse_acks(&text));
    }

    #[test]
    fn a_marker_inside_a_fenced_code_block_acknowledges_nothing() {
        let text = "Here is how you do it:\n```\n/ack-posture 0123456789abcdef\n```\nthanks";
        assert!(parse_acks(text).is_empty());
    }

    #[test]
    fn a_marker_mid_sentence_acknowledges_nothing() {
        assert!(parse_acks("I think /ack-posture 0123456789abcdef would work").is_empty());
    }

    #[test]
    fn a_marker_without_a_digest_acknowledges_nothing() {
        assert!(parse_acks("/ack-posture").is_empty());
        assert!(parse_acks("/ack-posture please").is_empty());
        assert!(parse_acks("/ack-posture 0123").is_empty(), "too short");
        assert!(
            parse_acks("/ack-posture 0123456789abcdefzz").is_empty(),
            "not hex"
        );
    }

    #[test]
    fn the_phrase_is_case_insensitive() {
        assert_eq!(parse_acks("/ACK-POSTURE 0123456789abcdef").len(), 1);
    }

    /// An HTML comment renders as nothing at all. A marker hidden in one would
    /// acknowledge a widening while the pull request shows no such decision —
    /// which defeats the whole point of putting the record in public.
    #[test]
    fn a_marker_hidden_in_an_html_comment_does_not_acknowledge() {
        let body = "\
Looks fine to me.

<!--
/ack-posture 0123456789abcdef
-->
";
        assert!(parse_acks(body).is_empty());
    }

    /// The harvester's own separator is an HTML comment, so comment tracking
    /// must not swallow it — otherwise one body's state leaks into the next.
    #[test]
    fn html_comment_tracking_does_not_swallow_the_source_separator() {
        let harvested = format!(
            "{SOURCE_SEPARATOR}\n<!-- an unterminated comment\n\
             {SOURCE_SEPARATOR}\n/ack-posture 0123456789abcdef\n"
        );
        assert_eq!(
            parse_acks(&harvested).len(),
            1,
            "a new comment body starts with a clean slate"
        );
    }

    /// A marker after a closed comment on the same line still counts.
    #[test]
    fn a_marker_after_a_closed_html_comment_still_acknowledges() {
        let acks = parse_acks("<!-- nit --> /ack-posture 0123456789abcdef  agreed\n");
        assert_eq!(acks.len(), 1);
    }

    /// Markdown's *other* code block: four leading spaces. `trim_start` erased
    /// the indentation, so a reviewer writing the marker as an indented sample
    /// — while arguing against it — acknowledged it.
    #[test]
    fn an_indented_code_sample_does_not_acknowledge() {
        let body = "\
Before anyone runs off and does this:

    /ack-posture 0123456789abcdef

…let us agree it is actually wanted.
";
        assert!(parse_acks(body).is_empty());
    }

    /// A tab indent is an indented code block too.
    #[test]
    fn a_tab_indented_sample_does_not_acknowledge() {
        assert!(parse_acks("here it is:\n\n\t/ack-posture 0123456789abcdef\n").is_empty());
    }

    /// …but ordinary light indentation is not a code block, and a reviewer who
    /// indents a line by a space or two still means it.
    #[test]
    fn a_slightly_indented_marker_still_acknowledges() {
        let acks = parse_acks("  /ack-posture 0123456789abcdef  yes, intended\n");
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].reason.as_deref(), Some("yes, intended"));
    }

    /// A reviewer quoting the gate's own report wraps it in a *longer* fence,
    /// because the report already contains a three-backtick block. Toggling on
    /// any fence line lets the inner three-backtick line close the outer
    /// four-backtick one, so the quoted marker becomes live and the reviewer
    /// acknowledges a widening they were only discussing. Per `CommonMark`, a
    /// fence closes only with the same character, at least as long.
    #[test]
    fn a_nested_fence_does_not_reopen_the_block_it_sits_in() {
        let quoting_the_report = "\
Not convinced by this one:

````
### 🛡️ Security posture diff

To acknowledge these exact changes, comment with:

```
/ack-posture 0123456789abcdef
```
````

Let us talk about it first.
";
        assert!(
            parse_acks(quoting_the_report).is_empty(),
            "quoting the report is not acknowledging it"
        );
    }

    /// The mirror of the above: a genuine marker after a correctly closed
    /// longer fence still counts, so the stricter rule does not make the
    /// escape hatch harder to use.
    #[test]
    fn a_marker_after_a_closed_longer_fence_still_acknowledges() {
        let body = "\
````
context
```
not a marker: /ack-posture ffffffffffffffff
```
````

/ack-posture 0123456789abcdef  intentional, launch week
";
        let acks = parse_acks(body);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].digest, "0123456789abcdef");
    }

    /// Tildes and backticks do not close each other.
    #[test]
    fn a_tilde_fence_is_not_closed_by_backticks() {
        let body = "~~~\n```\n/ack-posture 0123456789abcdef\n";
        assert!(parse_acks(body).is_empty());
    }

    #[test]
    fn an_unbalanced_fence_does_not_leak_into_the_next_comment() {
        // Comment 1 pastes a log and forgets the closing fence; comment 2 is a
        // genuine acknowledgment. Without isolation, comment 2 is swallowed.
        let harvested = format!(
            "{SOURCE_SEPARATOR}\nHere is the failing log:\n```\nthread panicked\n\
             {SOURCE_SEPARATOR}\n/ack-posture 0123456789abcdef\n"
        );
        let acks = parse_acks(&harvested);
        assert_eq!(acks.len(), 1, "the second comment still acknowledges");
    }

    #[test]
    fn an_unbalanced_fence_cannot_make_a_fenced_marker_live() {
        // The gate's own report carries the marker inside a fence. A previous
        // comment with an unbalanced fence must not invert the parity and turn
        // that report into an acknowledgment of itself.
        let harvested = format!(
            "{SOURCE_SEPARATOR}\nlog:\n```\noops\n\
             {SOURCE_SEPARATOR}\nquoting the bot:\n```\n/ack-posture 0123456789abcdef\n```\n"
        );
        assert!(parse_acks(&harvested).is_empty());
    }

    // ── matching ────────────────────────────────────────────────────────────

    #[test]
    fn a_short_marker_matches_the_full_digest() {
        let digest = ack_digest(&[&finding("route_added_open", "/a")]);
        let acks = parse_acks(&format!("/ack-posture {}", short(&digest)));
        assert!(matching(&acks, &digest).is_some());
    }

    #[test]
    fn a_full_marker_matches_the_full_digest() {
        let digest = ack_digest(&[&finding("route_added_open", "/a")]);
        let acks = parse_acks(&format!("/ack-posture {digest}"));
        assert!(matching(&acks, &digest).is_some());
    }

    #[test]
    fn a_marker_for_another_finding_set_does_not_match() {
        let acknowledged = ack_digest(&[&finding("route_added_open", "/a")]);
        let now = ack_digest(&[
            &finding("route_added_open", "/a"),
            &finding("route_added_open", "/b"),
        ]);
        let acks = parse_acks(&format!("/ack-posture {}", short(&acknowledged)));
        assert!(
            matching(&acks, &now).is_none(),
            "re-widening after an acknowledgment must re-block"
        );
    }

    #[test]
    fn a_prefix_shorter_than_the_published_marker_never_matches() {
        let digest = ack_digest(&[&finding("route_added_open", "/a")]);
        // A genuine 8-character prefix of the right digest still does not
        // acknowledge it: the published marker is 16, and accepting less would
        // let a shorter, weaker binding through the gate. Parsing it (rather
        // than dropping it) is what makes the "no acknowledgment matched"
        // diagnostic able to say so.
        let truncated: String = digest.chars().take(8).collect();
        let acks = parse_acks(&format!("/ack-posture {truncated}"));
        assert_eq!(acks.len(), 1, "it parses");
        assert!(matching(&acks, &digest).is_none(), "but it does not match");
    }

    #[test]
    fn a_marker_that_is_not_a_prefix_of_the_expected_digest_does_not_match() {
        let digest = ack_digest(&[&finding("route_added_open", "/a")]);
        let acks = parse_acks("/ack-posture 00000000000000000000");
        assert!(matching(&acks, &digest).is_none());
    }
}
