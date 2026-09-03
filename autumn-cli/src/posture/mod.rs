//! `autumn routes posture` — the review-time gate and the deploy-time proof.
//!
//! [`crate::routes_audit`] (issue #1604) proves *what* an app's security surface
//! is. This module answers the next question: *what did this change do to it,
//! and did a human agree to that?* — plus, at deploy time, *is the manifest in
//! this artifact the one they agreed to?* (issue #1624).
//!
//! Three commands, all pure functions of files on disk:
//!
//! ```text
//! autumn routes posture diff   --base B.json --head H.json   # the PR gate
//! autumn routes posture digest --manifest M.json             # the recorded number
//! autumn routes posture verify --manifest M.json --expect-digest D --repo o/r
//! ```
//!
//! The gate blocks only on *widening*: a new open route, a guard removed or
//! loosened, a classification downgraded. Narrowing and neutral changes
//! annotate and never block, and a pull request that moves nothing renders
//! nothing at all — a gate that cries wolf is a gate that gets turned off.
//!
//! See `docs/guide/posture-gate.md` and `docs/plans/2026-09-03-posture-gate.md`.

use std::fmt::Write as _;

pub mod ack;
pub mod diff;
pub mod model;
pub mod render;
pub mod verify;

use ack::Acknowledgment;
use diff::Finding;
use model::{ManifestError, PostureManifest};
use render::Report;

/// Exit code when a widening posture change is unacknowledged.
pub const EXIT_BLOCKED: i32 = 1;
/// Exit code for a usage or I/O problem — distinct from "the gate blocked" so
/// CI can tell "your PR widens the surface" from "the tool could not run".
pub const EXIT_USAGE: i32 = 2;

/// Options for `autumn routes posture diff`.
#[derive(Debug, Clone)]
pub struct DiffOptions<'a> {
    /// Base (previously accepted) manifest.
    pub base: &'a str,
    /// Head (freshly built) manifest.
    pub head: &'a str,
    /// `markdown` (default), `text`, or `json`.
    pub format: &'a str,
    /// Also write the rendered report here.
    pub output: Option<&'a str>,
    /// Acknowledgment markers passed inline.
    pub acks: &'a [String],
    /// File of harvested pull-request text to scan for acknowledgment markers.
    pub ack_file: Option<&'a str>,
    /// Treat a missing base manifest as "no baseline yet" instead of an error.
    pub allow_missing_base: bool,
}

/// What a diff run concluded, before it is rendered or turned into an exit code.
#[derive(Debug)]
pub struct Evaluation {
    pub findings: Vec<Finding>,
    /// Digest over the widening subset — what an acknowledgment must carry.
    pub ack_digest: String,
    /// Posture digest of the head manifest.
    pub head_posture_digest: String,
    /// The acknowledgment that matched, when one did.
    pub acknowledged: Option<Acknowledgment>,
    /// No base manifest was available.
    pub bootstrap: bool,
}

impl Evaluation {
    /// Whether this run blocks the pull request.
    #[must_use]
    pub fn blocked(&self) -> bool {
        !diff::widening(&self.findings).is_empty() && self.acknowledged.is_none()
    }

    /// The process exit code this evaluation implies.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(self.blocked()) * EXIT_BLOCKED
    }

    fn report(&self) -> Report<'_> {
        Report {
            findings: &self.findings,
            ack_digest: self.ack_digest.clone(),
            head_posture_digest: self.head_posture_digest.clone(),
            acknowledged: self.acknowledged.clone(),
            bootstrap: self.bootstrap,
        }
    }
}

/// Compare two manifests and resolve any acknowledgment against the result.
///
/// `base` is `None` when the project has no accepted baseline yet: the first
/// run on a repository that just enabled the gate reports the posture it found
/// and blocks nothing, because there is no *change* to widen.
#[must_use]
pub fn evaluate(
    base: Option<&PostureManifest>,
    head: &PostureManifest,
    ack_text: &str,
) -> Evaluation {
    let findings = base.map(|b| diff::diff(b, head)).unwrap_or_default();
    let widening = diff::widening(&findings);
    let ack_digest = ack::ack_digest(&widening);
    // Only a widening can be acknowledged. Resolving a marker against a clean
    // run would let a stale comment sit on the pull request claiming to have
    // approved something nobody proposed.
    let acknowledged = if widening.is_empty() {
        None
    } else {
        ack::matching(&ack::parse_acks(ack_text), &ack_digest).cloned()
    };
    Evaluation {
        findings,
        ack_digest,
        head_posture_digest: head.posture_digest(),
        acknowledged,
        bootstrap: base.is_none(),
    }
}

/// Render an evaluation in the requested format.
///
/// Unknown formats are a usage error rather than a silent fallback: a CI job
/// that typos `--format markdwon` must not post a plain-text report and call it
/// a success.
pub fn render(evaluation: &Evaluation, format: &str) -> Result<String, String> {
    let report = evaluation.report();
    match format {
        "markdown" | "md" => Ok(render::markdown(&report)),
        "text" => Ok(render::text(&report)),
        "json" => Ok(render::json(&report)),
        other => Err(format!(
            "unknown --format `{other}` (expected `markdown`, `text`, or `json`)"
        )),
    }
}

/// Collect acknowledgment text from `--ack` values and `--ack-file`.
fn harvested_ack_text(opts: &DiffOptions<'_>) -> Result<String, String> {
    let mut text = String::new();
    for ack in opts.acks {
        // An inline `--ack <digest>` is the phrase, without the ceremony of
        // typing it: normalize it into a marker line so there is exactly one
        // parser for both entry points.
        let _ = writeln!(text, "{} {}", ack::ACK_PHRASE, ack.trim());
    }
    if let Some(path) = opts.ack_file {
        let harvested = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read acknowledgment file {path}: {e}"))?;
        text.push_str(&harvested);
        text.push('\n');
    }
    Ok(text)
}

/// `autumn routes posture diff`.
#[must_use]
pub fn run_diff(opts: &DiffOptions<'_>) -> i32 {
    let head = match PostureManifest::read(opts.head) {
        Ok(m) => m,
        Err(e) => return fail(&e.to_string()),
    };
    let base = match PostureManifest::read(opts.base) {
        Ok(m) => Some(m),
        Err(ManifestError::Io { .. }) if opts.allow_missing_base => None,
        Err(e) => return fail(&e.to_string()),
    };
    let ack_text = match harvested_ack_text(opts) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };

    let evaluation = evaluate(base.as_ref(), &head, &ack_text);
    let rendered = match render(&evaluation, opts.format) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };

    if let Some(path) = opts.output
        && let Err(e) = std::fs::write(path, &rendered)
    {
        return fail(&format!("cannot write report to {path}: {e}"));
    }
    if !rendered.is_empty() {
        println!("{rendered}");
    }

    if evaluation.blocked() {
        eprintln!(
            "\n\u{2717} security posture widened and is not acknowledged.\n  \
             Acknowledge it with this line \u{2014} as a pull-request comment, or \
             locally as `--ack <digest>`:\n\n      {} {}\n",
            ack::ACK_PHRASE,
            ack::short(&evaluation.ack_digest)
        );
    }
    evaluation.exit_code()
}

/// `autumn routes posture digest`.
#[must_use]
pub fn run_digest(manifest: &str, format: &str) -> i32 {
    let digest = match verify::manifest_digest(manifest) {
        Ok(d) => d,
        Err(e) => return fail(&e.to_string()),
    };
    match format {
        "text" => println!("{digest}"),
        "json" => println!(
            "{}",
            serde_json::json!({ "manifest": manifest, "posture_digest": digest })
        ),
        other => {
            return fail(&format!(
                "unknown --format `{other}` (expected `text` or `json`)"
            ));
        }
    }
    0
}

/// `autumn routes posture verify`.
#[must_use]
pub fn run_verify(opts: &verify::VerifyOptions<'_>) -> i32 {
    let report = match verify::verify_with(opts, &verify::GhAttestationVerifier) {
        Ok(r) => r,
        Err(e) => return fail(&e.to_string()),
    };
    println!("\u{1F342} autumn routes posture verify\n");
    println!("  manifest        {}", report.manifest);
    println!("  posture digest  {}", report.posture_digest);
    for check in &report.checks {
        let mark = if check.waived {
            "\u{26A0}"
        } else if check.passed {
            "\u{2713}"
        } else {
            "\u{2717}"
        };
        println!("  {mark} {:<22} {}", check.name, check.detail);
    }
    if report.passed() { 0 } else { EXIT_BLOCKED }
}

/// Print a usage/I/O failure and hand back the exit code for one.
fn fail(message: &str) -> i32 {
    eprintln!("\u{2717} {message}");
    EXIT_USAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> PostureManifest {
        PostureManifest::parse(json, "test.json").expect("fixture parses")
    }

    fn manifest(routes: &str) -> PostureManifest {
        parse(&format!(
            r#"{{"schema_version":3,"dimensions":{{
                 "routes":{{"provenance":"provable","source":"m","entries":[{routes}]}},
                 "csrf":{{"provenance":"declared","source":"c","exempt_paths":[],"entries":[]}},
                 "security_headers":{{"provenance":"declared","source":"c","entries":[]}},
                 "authorization_policies":{{"provenance":"provable","source":"m","runtime_caveat":"x","entries":[]}}
               }},"excluded":[]}}"#
        ))
    }

    fn route(path: &str, classification: &str, roles: &[&str]) -> String {
        let roles = roles
            .iter()
            .map(|r| format!("\"{r}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"path":"{path}","method":"GET","name":"h","classification":"{classification}",
                 "roles":[{roles}],"scopes":[],"policy":false,"source":"user","provenance":"provable"}}"#
        )
    }

    #[test]
    fn an_unchanged_posture_neither_blocks_nor_says_anything() {
        let base = manifest(&route("/admin", "gated", &["admin"]));
        let head = manifest(&route("/admin", "gated", &["admin"]));
        let e = evaluate(Some(&base), &head, "");
        assert!(!e.blocked());
        assert_eq!(e.exit_code(), 0);
        assert!(e.findings.is_empty());
        assert_eq!(render(&e, "markdown").unwrap(), "");
    }

    #[test]
    fn a_widening_blocks_until_the_matching_marker_is_present() {
        let base = manifest(&route("/admin", "gated", &["admin"]));
        let head = manifest(&route("/admin", "public", &[]));

        let blocked = evaluate(Some(&base), &head, "");
        assert!(blocked.blocked());
        assert_eq!(blocked.exit_code(), EXIT_BLOCKED);

        let comment = format!("{} {}", ack::ACK_PHRASE, ack::short(&blocked.ack_digest));
        let acknowledged = evaluate(Some(&base), &head, &comment);
        assert!(!acknowledged.blocked());
        assert_eq!(acknowledged.exit_code(), 0);
        assert!(acknowledged.acknowledged.is_some());
    }

    /// The acknowledgment is bound to the widening set, so a *later* widening
    /// re-blocks even though the marker is still sitting on the pull request.
    #[test]
    fn re_widening_after_an_acknowledgment_blocks_again() {
        let base = manifest(&route("/admin", "gated", &["admin"]));
        let first = manifest(&route("/admin", "public", &[]));
        let comment = format!(
            "{} {}",
            ack::ACK_PHRASE,
            ack::short(&evaluate(Some(&base), &first, "").ack_digest)
        );
        assert!(!evaluate(Some(&base), &first, &comment).blocked());

        let second = manifest(&format!(
            "{},{}",
            route("/admin", "public", &[]),
            route("/internal", "public", &[])
        ));
        assert!(
            evaluate(Some(&base), &second, &comment).blocked(),
            "a new widening must not inherit the old acknowledgment"
        );
    }

    /// Pushing commits that do not change the widening set keeps the
    /// acknowledgment valid — otherwise every push would re-ask.
    #[test]
    fn an_unrelated_push_keeps_the_acknowledgment_valid() {
        let base = manifest(&route("/admin", "gated", &["admin"]));
        let first = manifest(&route("/admin", "public", &[]));
        let comment = format!(
            "{} {}",
            ack::ACK_PHRASE,
            ack::short(&evaluate(Some(&base), &first, "").ack_digest)
        );

        // A later commit adds a *gated* route: neutral, so the widening set is
        // untouched.
        let later = manifest(&format!(
            "{},{}",
            route("/admin", "public", &[]),
            route("/reports", "gated", &["admin"])
        ));
        assert!(!evaluate(Some(&base), &later, &comment).blocked());
    }

    #[test]
    fn narrowing_only_changes_never_block() {
        let base = manifest(&route("/admin", "public", &[]));
        let head = manifest(&route("/admin", "gated", &["admin"]));
        let e = evaluate(Some(&base), &head, "");
        assert!(!e.blocked());
        assert!(!e.findings.is_empty(), "but they are still reported");
    }

    #[test]
    fn with_no_baseline_nothing_blocks() {
        let head = manifest(&route("/admin", "public", &[]));
        let e = evaluate(None, &head, "");
        assert!(e.bootstrap);
        assert!(!e.blocked());
        assert!(e.findings.is_empty());
        assert_eq!(e.exit_code(), 0);
    }

    #[test]
    fn the_head_posture_digest_is_the_manifests_own() {
        let head = manifest(&route("/admin", "gated", &["admin"]));
        let e = evaluate(None, &head, "");
        assert_eq!(e.head_posture_digest, head.posture_digest());
    }

    #[test]
    fn an_unknown_format_is_a_usage_error_not_a_silent_fallback() {
        let head = manifest(&route("/a", "gated", &["admin"]));
        let e = evaluate(None, &head, "");
        assert!(render(&e, "markdwon").is_err());
    }

    #[test]
    fn an_acknowledgment_for_a_different_digest_does_not_unblock() {
        let base = manifest(&route("/admin", "gated", &["admin"]));
        let head = manifest(&route("/admin", "public", &[]));
        let e = evaluate(Some(&base), &head, "/ack-posture 0000000000000000");
        assert!(e.blocked());
        assert!(e.acknowledged.is_none());
    }
}
