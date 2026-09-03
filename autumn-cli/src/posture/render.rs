//! Turning a set of findings into something a reviewer reads on the pull
//! request — and into JSON for anything downstream.
//!
//! Two rules the shape of this module exists to keep:
//!
//! 1. **Silence is a feature.** A PR with no posture change renders to nothing,
//!    so the workflow has nothing to post.
//! 2. **The reviewer must never have to compute anything.** The report carries
//!    the exact acknowledgment line to paste, already filled in.

use std::fmt::Write as _;

use serde::Serialize;

use super::ack::{ACK_PHRASE, Acknowledgment, short};
use super::diff::{Finding, Severity};

/// HTML marker the workflow greps for so it can *update* its comment instead of
/// appending a new one on every push.
pub const COMMENT_MARKER: &str = "<!-- autumn-posture-gate -->";

/// Longest table this renders per severity section before collapsing the tail
/// into a count. A 200-row table is a table nobody reads.
pub const MAX_ROWS: usize = 25;

/// Everything a report needs to know, assembled by [`super::run_diff`].
#[derive(Debug)]
pub struct Report<'a> {
    pub findings: &'a [Finding],
    /// Digest of the widening subset; the number the acknowledgment carries.
    pub ack_digest: String,
    /// Posture digest of the head manifest (what a release would record).
    pub head_posture_digest: String,
    /// The acknowledgment that unblocked this run, when one did.
    pub acknowledged: Option<Acknowledgment>,
    /// No base manifest existed: this is the first run, nothing to compare.
    pub bootstrap: bool,
}

impl Report<'_> {
    /// The findings that block.
    #[must_use]
    pub fn widening(&self) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Widening)
            .collect()
    }

    /// Whether this run blocks the pull request.
    #[must_use]
    pub fn blocked(&self) -> bool {
        !self.widening().is_empty() && self.acknowledged.is_none()
    }
}

/// The machine-readable report.
#[derive(Debug, Serialize)]
struct JsonReport<'a> {
    schema: &'static str,
    blocked: bool,
    bootstrap: bool,
    ack_digest: String,
    ack_phrase: String,
    head_posture_digest: &'a str,
    acknowledged_by_digest: Option<&'a str>,
    acknowledgment_reason: Option<&'a str>,
    counts: Counts,
    findings: Vec<JsonFinding<'a>>,
}

#[derive(Debug, Serialize)]
struct Counts {
    widening: usize,
    neutral: usize,
    narrowing: usize,
}

#[derive(Debug, Serialize)]
struct JsonFinding<'a> {
    kind: &'a str,
    severity: &'a str,
    method: &'a str,
    path: &'a str,
    before: &'a str,
    after: &'a str,
    detail: &'a str,
}

/// The markdown a reviewer reads on the pull request.
///
/// Empty when there is nothing to say — the caller posts nothing rather than
/// posting "no changes".
#[must_use]
pub fn markdown(report: &Report<'_>) -> String {
    if report.findings.is_empty() && !report.bootstrap {
        return String::new();
    }
    let mut out = String::from("### \u{1F6E1}\u{FE0F} Security posture diff\n\n");

    if report.bootstrap {
        out.push_str(
            "No posture baseline was found for the base branch, so there is nothing to \
             compare against yet. Commit the manifest this run produced and the next pull \
             request will be diffed against it.\n\n",
        );
    }

    let widening = report.widening();
    if !widening.is_empty() {
        match &report.acknowledged {
            Some(ack) => {
                let _ = write!(
                    out,
                    "**{} widening change{} \u{2014} acknowledged**",
                    widening.len(),
                    plural(widening.len())
                );
                if let Some(reason) = &ack.reason {
                    let _ = write!(out, " \u{2014} \u{201C}{}\u{201D}", escape(reason));
                }
                out.push_str("\n\n");
            }
            None => {
                let _ = write!(
                    out,
                    "**{} widening change{} \u{2014} acknowledgment required.** This pull \
                     request makes part of the app reachable by more callers than before.\n\n",
                    widening.len(),
                    plural(widening.len())
                );
            }
        }
        table(&mut out, &widening);
    }

    section(
        &mut out,
        "Other changes",
        &by_severity(report.findings, Severity::Neutral),
    );
    section(
        &mut out,
        "Narrowing",
        &by_severity(report.findings, Severity::Narrowing),
    );

    if !widening.is_empty() && report.acknowledged.is_none() {
        let _ = write!(
            out,
            "To acknowledge these exact changes, comment on this pull request with:\n\n\
             ```\n{ACK_PHRASE} {}\n```\n\n\
             The digest names this set of widenings: unrelated pushes keep it valid, and a \
             *new* widening needs a new acknowledgment.\n\n",
            short(&report.ack_digest)
        );
    }

    let _ = write!(
        out,
        "<sub>posture digest `{}` \u{2022} generated by `autumn routes posture diff`</sub>\
         \n\n{COMMENT_MARKER}\n",
        report.head_posture_digest
    );
    out
}

/// One severity section, omitted entirely when empty.
fn section(out: &mut String, title: &str, findings: &[&Finding]) {
    if findings.is_empty() {
        return;
    }
    let _ = write!(out, "**{title} ({})**\n\n", findings.len());
    table(out, findings);
}

fn by_severity(findings: &[Finding], severity: Severity) -> Vec<&Finding> {
    findings.iter().filter(|f| f.severity == severity).collect()
}

/// A markdown table, capped at [`MAX_ROWS`] with the tail collapsed to a count.
fn table(out: &mut String, findings: &[&Finding]) {
    out.push_str("| Change | Method | Path | Before | After |\n|---|---|---|---|---|\n");
    for f in findings.iter().take(MAX_ROWS) {
        let _ = writeln!(
            out,
            "| {} | `{}` | `{}` | {} | {} |",
            escape(&f.detail),
            escape(&f.method),
            escape(&f.path),
            code(&f.before),
            code(&f.after)
        );
    }
    if findings.len() > MAX_ROWS {
        let _ = write!(out, "\n\u{2026} and {} more.\n", findings.len() - MAX_ROWS);
    }
    out.push('\n');
}

/// Wrap a posture label in code ticks, leaving the "absent"/"none" sentinels
/// as plain words so the table reads as prose where there is no value.
fn code(value: &str) -> String {
    if matches!(value, "absent" | "none" | "not emitted") {
        format!("_{value}_")
    } else {
        format!("`{}`", escape(value))
    }
}

/// Neutralize what would break out of a markdown table cell.
///
/// Manifest values are machine-generated, but a route path, a role name or a
/// CSP value is still app-controlled text landing in a rendered PR comment:
/// a `|` would forge a column and a newline would forge a row. Backticks are
/// stripped rather than escaped, since a value carrying one would otherwise
/// close the code span it is rendered inside.
fn escape(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
        .replace('`', "'")
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The plain-text rendering for a terminal.
#[must_use]
pub fn text(report: &Report<'_>) -> String {
    if report.findings.is_empty() && !report.bootstrap {
        return String::new();
    }
    let mut out = String::new();
    if report.bootstrap {
        out.push_str("no posture baseline to compare against yet\n");
    }
    for f in report.findings {
        let _ = writeln!(
            out,
            "{:<10} {:<7} {}  {} \u{2192} {}  ({})",
            f.severity.as_str(),
            f.method,
            f.path,
            f.before,
            f.after,
            f.detail
        );
    }
    if !report.widening().is_empty() {
        out.push('\n');
        match &report.acknowledged {
            Some(ack) => {
                let _ = writeln!(out, "acknowledged: {ACK_PHRASE} {}", ack.digest);
            }
            None => {
                let _ = writeln!(
                    out,
                    "blocked: acknowledge with `{ACK_PHRASE} {}`",
                    short(&report.ack_digest)
                );
            }
        }
    }
    out
}

/// The JSON rendering, for anything that wants to consume findings directly.
#[must_use]
pub fn json(report: &Report<'_>) -> String {
    let counts = Counts {
        widening: by_severity(report.findings, Severity::Widening).len(),
        neutral: by_severity(report.findings, Severity::Neutral).len(),
        narrowing: by_severity(report.findings, Severity::Narrowing).len(),
    };
    let document = JsonReport {
        schema: "autumn.posture-diff.v1",
        blocked: report.blocked(),
        bootstrap: report.bootstrap,
        ack_digest: report.ack_digest.clone(),
        ack_phrase: format!("{ACK_PHRASE} {}", short(&report.ack_digest)),
        head_posture_digest: &report.head_posture_digest,
        acknowledged_by_digest: report.acknowledged.as_ref().map(|a| a.digest.as_str()),
        acknowledgment_reason: report
            .acknowledged
            .as_ref()
            .and_then(|a| a.reason.as_deref()),
        counts,
        findings: report
            .findings
            .iter()
            .map(|f| JsonFinding {
                kind: f.kind,
                severity: f.severity.as_str(),
                method: &f.method,
                path: &f.path,
                before: &f.before,
                after: &f.after,
                detail: &f.detail,
            })
            .collect(),
    };
    serde_json::to_string_pretty(&document)
        .unwrap_or_else(|e| format!("{{\"error\":\"cannot serialize report: {e}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(kind: &'static str, severity: Severity, path: &str) -> Finding {
        Finding {
            kind,
            severity,
            method: "GET".to_owned(),
            path: path.to_owned(),
            before: "gated (roles: admin)".to_owned(),
            after: "public".to_owned(),
            detail: "guard removed: gated \u{2192} public".to_owned(),
        }
    }

    fn report(findings: &[Finding], acknowledged: Option<Acknowledgment>) -> Report<'_> {
        Report {
            findings,
            ack_digest: "0123456789abcdef0123".to_owned(),
            head_posture_digest: "deadbeef".to_owned(),
            acknowledged,
            bootstrap: false,
        }
    }

    #[test]
    fn no_findings_renders_no_markdown_at_all() {
        assert_eq!(markdown(&report(&[], None)), "");
    }

    #[test]
    fn a_widening_finding_names_the_route_and_the_transition() {
        let findings = vec![finding(
            "classification_downgraded",
            Severity::Widening,
            "/admin/users",
        )];
        let md = markdown(&report(&findings, None));
        assert!(md.contains("/admin/users"), "{md}");
        assert!(md.contains("GET"), "{md}");
        assert!(md.contains("gated (roles: admin)"), "{md}");
        assert!(md.contains("public"), "{md}");
        assert!(
            md.contains(COMMENT_MARKER),
            "carries the update marker: {md}"
        );
    }

    #[test]
    fn a_widening_report_prints_the_exact_acknowledgment_line() {
        let findings = vec![finding(
            "classification_downgraded",
            Severity::Widening,
            "/admin/users",
        )];
        let md = markdown(&report(&findings, None));
        assert!(
            md.contains(&format!("{ACK_PHRASE} {}", short("0123456789abcdef0123"))),
            "the reviewer must be able to paste it verbatim: {md}"
        );
    }

    #[test]
    fn an_acknowledged_report_says_so_and_does_not_ask_again() {
        let findings = vec![finding(
            "classification_downgraded",
            Severity::Widening,
            "/admin/users",
        )];
        let ack = Acknowledgment {
            digest: short("0123456789abcdef0123"),
            reason: Some("launch week".to_owned()),
        };
        let md = markdown(&report(&findings, Some(ack)));
        assert!(md.to_lowercase().contains("acknowledged"), "{md}");
        assert!(md.contains("launch week"), "{md}");
        assert!(
            !md.contains("comment on this pull request"),
            "no second ask once acknowledged: {md}"
        );
    }

    #[test]
    fn narrowing_only_changes_render_without_asking_for_acknowledgment() {
        let findings = vec![finding("route_removed", Severity::Narrowing, "/old")];
        let md = markdown(&report(&findings, None));
        assert!(md.contains("/old"), "{md}");
        assert!(
            !md.contains(ACK_PHRASE),
            "nothing to acknowledge, so nothing is asked: {md}"
        );
    }

    #[test]
    fn a_long_finding_list_is_capped_with_a_tail_count() {
        let findings: Vec<Finding> = (0..MAX_ROWS + 7)
            .map(|i| finding("route_added_open", Severity::Widening, &format!("/r{i:03}")))
            .collect();
        let md = markdown(&report(&findings, None));
        assert!(md.contains("7 more"), "{md}");
        assert!(md.contains("/r000"), "{md}");
        assert!(!md.contains("/r031"), "beyond the cap: {md}");
    }

    #[test]
    fn bootstrap_says_there_is_no_baseline_and_asks_for_nothing() {
        let mut r = report(&[], None);
        r.bootstrap = true;
        let md = markdown(&r);
        assert!(md.to_lowercase().contains("baseline"), "{md}");
        assert!(!md.contains(ACK_PHRASE), "{md}");
    }

    #[test]
    fn json_report_carries_findings_counts_and_the_ack_digest() {
        let findings = vec![
            finding("classification_downgraded", Severity::Widening, "/a"),
            finding("route_removed", Severity::Narrowing, "/b"),
        ];
        let out = json(&report(&findings, None));
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["blocked"], true);
        assert_eq!(v["counts"]["widening"], 1);
        assert_eq!(v["counts"]["narrowing"], 1);
        assert_eq!(v["findings"][0]["path"], "/a");
        assert_eq!(v["findings"][0]["severity"], "widening");
        assert_eq!(v["ack_digest"], "0123456789abcdef0123");
        assert!(
            v["ack_phrase"]
                .as_str()
                .unwrap()
                .starts_with("/ack-posture ")
        );
    }

    #[test]
    fn json_report_of_a_clean_run_is_still_valid_json() {
        let out = json(&report(&[], None));
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["blocked"], false);
        assert_eq!(v["counts"]["widening"], 0);
        assert!(v["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn text_rendering_states_the_verdict_in_one_line() {
        let findings = vec![finding(
            "classification_downgraded",
            Severity::Widening,
            "/admin",
        )];
        let out = text(&report(&findings, None));
        assert!(out.contains("/admin"), "{out}");
        assert!(out.to_lowercase().contains("widening"), "{out}");
        assert_eq!(text(&report(&[], None)), "", "silence when nothing changed");
    }
}
