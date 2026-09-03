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
/// - a quoted line (`>` …) never acknowledges anything, so quoting somebody
///   else's comment — which GitHub's own reply UI does automatically — does not
///   silently re-acknowledge a digest;
/// - a fenced code block is inert, so the documentation example in a comment
///   does not acknowledge anything either;
/// - the digest must be plain lower/upper-case hex, 8..=64 characters.
#[must_use]
pub fn parse_acks(text: &str) -> Vec<Acknowledgment> {
    let mut acks = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || trimmed.starts_with('>') {
            continue;
        }
        if let Some(ack) = parse_marker(trimmed) {
            acks.push(ack);
        }
    }
    acks
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
        .find(|ack| super::verify::digest_matches(digest, &ack.digest))
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
