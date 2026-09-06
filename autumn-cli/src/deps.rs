//! Dependency policy evaluation shared by `autumn doctor` and `autumn dev`
//! (issue #1633).
//!
//! Detection is not re-implemented here. Doctor and dev run the same auditor
//! (`cargo deny`), against the same policy file (`deny.toml`) and the same
//! waiver store (`[advisories] ignore`) that the CI gate from issue #1600 runs.
//! A local verdict and the CI verdict therefore cannot disagree.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The app-level dependency policy file. Also the CI gate's config.
pub const POLICY_FILE: &str = "deny.toml";

/// Policy sections that switch on an extra cargo-deny check when present.
///
/// The scaffolded CI workflow derives its check list from the same names, so
/// uncommenting a section widens the local check and the CI gate together.
pub const OPTIONAL_CHECKS: &[&str] = &["bans", "licenses", "sources"];

/// An advisory database older than this is reported as stale.
pub const STALE_AFTER_DAYS: u64 = 30;

/// Diagnostic codes that report on the policy file, not on the dependency
/// tree. They never change the CI verdict, so reporting them is noise.
const CONFIG_ONLY_CODES: &[&str] = &[
    "advisory-ignored",
    "advisory-not-detected",
    "license-not-encountered",
    "license-exception-not-encountered",
    "unmatched-skip",
    "unmatched-skip-root",
    "unmatched-organization",
    "unmatched-bypass",
    // Always paired with `unlicensed`, which names the same crate.
    "no-license-field",
    "deprecated",
];

/// Consequence of one finding.
///
/// Severity is consequence, not taxonomy: the policy decides what fails. A
/// denied finding is at least [`Severity::High`]; a warned one is at most
/// [`Severity::Medium`]. Critical/High therefore means "the CI gate fails".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// One advisory or policy violation against the app's lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// `RUSTSEC-YYYY-NNNN` for an advisory, else the cargo-deny code.
    pub id: String,
    /// The cargo-deny diagnostic code, e.g. `vulnerability` or `banned`.
    pub code: String,
    /// `name version`, empty when the auditor did not name a crate.
    pub package: String,
    pub title: String,
    pub severity: Severity,
    /// Waived by an `[advisories] ignore` entry in the policy file.
    pub waived: bool,
    /// The auditor graded this an error, so the CI gate fails on it.
    pub blocking: bool,
}

/// What an evaluation of the app's dependency policy produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluation {
    /// No policy file. The CI gate fails on this too.
    NoPolicy,
    /// `cargo deny` is not on PATH.
    AuditorMissing,
    /// The `RustSec` database was never fetched, so nothing can be verified.
    DatabaseMissing,
    /// The auditor produced a verdict.
    Audited {
        findings: Vec<Finding>,
        checks: Vec<String>,
        db_age_days: Option<u64>,
    },
    /// The auditor ran but produced no verdict.
    Unavailable(String),
}

/// The cargo-deny checks this policy file activates, in command order.
///
/// Advisories are always checked: they are the gate from issue #1600. The
/// optional sections are opt-in, so an app that leaves them commented out gets
/// the same command locally that its CI runs.
pub fn policy_checks(policy: &str) -> Vec<String> {
    let mut checks = vec!["advisories".to_owned()];
    checks.extend(
        OPTIONAL_CHECKS
            .iter()
            .filter(|section| declares_section(policy, section))
            .map(|section| (*section).to_owned()),
    );
    checks
}

/// True when `policy` declares `[section]` on an uncommented line.
fn declares_section(policy: &str, section: &str) -> bool {
    let header = format!("[{section}]");
    policy.lines().any(|line| code_of(line) == header)
}

/// The code part of one policy line: comment stripped, trimmed.
fn code_of(line: &str) -> &str {
    let line = line.trim();
    if line.starts_with('#') {
        return "";
    }
    line.split('#').next().unwrap_or("").trim()
}

/// A custom `db-path` declared by the policy, if any.
pub fn policy_db_path(policy: &str) -> Option<String> {
    policy.lines().find_map(|line| {
        let value = code_of(line).strip_prefix("db-path")?.trim_start();
        let value = value.strip_prefix('=')?.trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        (!value.is_empty()).then(|| value.to_owned())
    })
}

// ── CVSS v3 base score ───────────────────────────────────────────────────────

/// CVSS v3 base score for `vector`, or `None` when it is not a complete v3
/// vector. CVSS v3.1 specification, section 8.1.
///
/// The arithmetic is written as the specification writes it. A fused
/// multiply-add is more accurate, and that is the problem: the score is
/// rounded up to one decimal, so a more accurate intermediate can land on the
/// other side of a boundary from every reference implementation.
#[allow(
    clippy::suboptimal_flops,
    reason = "matches the CVSS reference implementations"
)]
pub fn cvss3_base_score(vector: &str) -> Option<f64> {
    let body = vector
        .strip_prefix("CVSS:3.1/")
        .or_else(|| vector.strip_prefix("CVSS:3.0/"))?;

    let (mut av, mut ac, mut pr, mut ui) = (None, None, None, None);
    let (mut scope, mut conf, mut integ, mut avail) = (None, None, None, None);
    for part in body.split('/') {
        let (key, value) = part.split_once(':')?;
        let slot = match key {
            "AV" => &mut av,
            "AC" => &mut ac,
            "PR" => &mut pr,
            "UI" => &mut ui,
            "S" => &mut scope,
            "C" => &mut conf,
            "I" => &mut integ,
            "A" => &mut avail,
            // Temporal and environmental metrics do not change the base score.
            _ => continue,
        };
        *slot = Some(value);
    }

    let changed = match scope? {
        "U" => false,
        "C" => true,
        _ => return None,
    };
    let av = match av? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let ac = match ac? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    // Privileges Required is scored higher when the scope changes.
    let pr = match (pr?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.50,
        _ => return None,
    };
    let ui = match ui? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let impact_metric = |value: &str| match value {
        "H" => Some(0.56),
        "L" => Some(0.22),
        "N" => Some(0.0),
        _ => None,
    };
    let conf = impact_metric(conf?)?;
    let integ = impact_metric(integ?)?;
    let avail = impact_metric(avail?)?;

    let iss: f64 = 1.0 - (1.0 - conf) * (1.0 - integ) * (1.0 - avail);
    let impact = if changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return Some(0.0);
    }
    let exploitability: f64 = 8.22 * av * ac * pr * ui;
    let raw = if changed {
        1.08 * (impact + exploitability)
    } else {
        impact + exploitability
    };
    Some(roundup(raw.min(10.0)))
}

/// Round up to one decimal place, per CVSS v3.1 appendix A.
fn roundup(value: f64) -> f64 {
    let scaled = (value * 100_000.0).round();
    if scaled % 10_000.0 == 0.0 {
        scaled / 100_000.0
    } else {
        ((scaled / 10_000.0).floor() + 1.0) / 10.0
    }
}

/// CVSS v3 severity band for `vector`. A zero score has no band.
pub fn severity_from_cvss(vector: Option<&str>) -> Option<Severity> {
    let score = cvss3_base_score(vector?)?;
    Some(match score {
        s if s >= 9.0 => Severity::Critical,
        s if s >= 7.0 => Severity::High,
        s if s >= 4.0 => Severity::Medium,
        s if s > 0.0 => Severity::Low,
        _ => return None,
    })
}

// ── Diagnostic stream ────────────────────────────────────────────────────────

/// Findings in one cargo-deny JSON diagnostic stream.
///
/// `None` when the stream carries no summary: the auditor did not finish, so
/// "no findings" would be a false all-clear.
pub fn parse_audit(ndjson: &str) -> Option<Vec<Finding>> {
    let mut finished = false;
    let mut waived: HashSet<String> = HashSet::new();
    let mut diagnostics: Vec<serde_json::Value> = Vec::new();

    for line in ndjson.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match entry.get("type").and_then(serde_json::Value::as_str) {
            Some("summary") => finished = true,
            Some("diagnostic") => {
                let Some(fields) = entry.get("fields") else {
                    continue;
                };
                if code(fields) == "advisory-ignored" {
                    if let Some(id) = waived_id(fields) {
                        waived.insert(id);
                    }
                } else {
                    diagnostics.push(fields.clone());
                }
            }
            _ => {}
        }
    }
    if !finished {
        return None;
    }
    Some(
        diagnostics
            .iter()
            .filter_map(|fields| finding_from(fields, &waived))
            .collect(),
    )
}

fn code(fields: &serde_json::Value) -> &str {
    fields
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

/// The advisory id an `advisory-ignored` marker names.
fn waived_id(fields: &serde_json::Value) -> Option<String> {
    let labels = fields.get("labels")?.as_array()?;
    let label = labels
        .iter()
        .find(|label| {
            label.get("message").and_then(serde_json::Value::as_str)
                == Some("advisory ignored here")
        })
        .or_else(|| labels.first())?;
    Some(label.get("span")?.as_str()?.to_owned())
}

/// One finding, or `None` when the diagnostic reports on the policy file
/// rather than on the dependency tree.
fn finding_from(fields: &serde_json::Value, waived: &HashSet<String>) -> Option<Finding> {
    let code = code(fields);
    if code.is_empty() || CONFIG_ONLY_CODES.contains(&code) {
        return None;
    }
    let level = fields
        .get("severity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("note");
    let advisory = fields.get("advisory").filter(|value| !value.is_null());
    // A finding is something the policy graded: an error or a warning. Notes
    // and helps are context — one per private workspace crate, for one — and
    // reporting them buries the findings that matter. A waived advisory is the
    // exception: it is graded down to a note but still carries its advisory.
    if !matches!(level, "error" | "warning") && advisory.is_none() {
        return None;
    }
    let blocking = level == "error";

    let id = advisory
        .and_then(|advisory| advisory.get("id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(code)
        .to_owned();
    let message = fields
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let title = one_line(
        &advisory
            .and_then(|advisory| advisory.get("title"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| labelled_title(fields, message), str::to_owned),
    );

    let waived = waived.contains(&id);
    Some(Finding {
        code: code.to_owned(),
        package: package_of(fields, advisory),
        title,
        severity: grade(advisory, blocking, waived),
        waived,
        blocking,
        id,
    })
}

/// Longest finding title doctor renders. A diagnostic label can carry one
/// lock entry per line; doctor renders one line per finding.
pub const TITLE_MAX_CHARS: usize = 160;

/// Collapse whitespace runs to single spaces and bound the length.
pub fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= TITLE_MAX_CHARS {
        return collapsed;
    }
    let kept: String = collapsed.chars().take(TITLE_MAX_CHARS - 1).collect();
    format!("{kept}\u{2026}")
}

/// The diagnostic message, plus the label that says what was violated.
///
/// A policy violation names the crate in `message` but the violated rule — a
/// rejected license, for one — only in a label.
fn labelled_title(fields: &serde_json::Value, message: &str) -> String {
    let span = fields
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|label| {
            !label
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .is_empty()
        })
        .and_then(|label| label.get("span"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if span.is_empty() || message.contains(span) {
        return message.to_owned();
    }
    format!("{message}: {span}")
}

/// `name version` for the crate a diagnostic is about.
fn package_of(fields: &serde_json::Value, advisory: Option<&serde_json::Value>) -> String {
    let krate = fields
        .get("graphs")
        .and_then(serde_json::Value::as_array)
        .and_then(|graphs| graphs.first())
        .and_then(|graph| graph.get("Krate"));
    if let Some(krate) = krate {
        let name = krate
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let version = krate
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !name.is_empty() {
            return if version.is_empty() {
                one_line(name)
            } else {
                one_line(&format!("{name} {version}"))
            };
        }
    }
    advisory
        .and_then(|advisory| advisory.get("package"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Grade one finding.
///
/// Severity is consequence, so the policy decides it: a denied finding is at
/// least high, a warned one at most medium. CVSS only separates critical from
/// high inside what the policy already denies.
///
/// A waiver is the exception. cargo-deny grades a waived advisory down to a
/// note, so the emitted severity says nothing about the advisory; a reader has
/// to see what the waiver accepted, so a waived finding keeps its own severity.
fn grade(advisory: Option<&serde_json::Value>, blocking: bool, waived: bool) -> Severity {
    let base = advisory.map_or(Severity::Low, |advisory| {
        match advisory
            .get("informational")
            .and_then(serde_json::Value::as_str)
        {
            // A vulnerability with no scorable CVSS is treated as high.
            None => severity_from_cvss(advisory.get("cvss").and_then(serde_json::Value::as_str))
                .unwrap_or(Severity::High),
            Some("unsound") => Severity::Medium,
            Some(_) => Severity::Low,
        }
    });
    if waived {
        base
    } else if blocking {
        base.max(Severity::High)
    } else {
        base.min(Severity::Medium)
    }
}

/// The worst severity among unwaived findings.
pub fn worst_severity(findings: &[Finding]) -> Option<Severity> {
    findings
        .iter()
        .filter(|finding| !finding.waived)
        .map(|finding| finding.severity)
        .max()
}

// ── Advisory database age ────────────────────────────────────────────────────

/// Whole days between `then` and `now`. A clock that moved backwards reads as
/// fresh rather than as a huge age.
pub fn age_days(then: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(then)
        .map_or(0, |elapsed| elapsed.as_secs() / 86_400)
}

/// Newest modification time among the database checkouts under `dbs_dir`.
pub fn newest_db_mtime(dbs_dir: &Path) -> Option<SystemTime> {
    std::fs::read_dir(dbs_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max()
}

/// Where cargo-deny keeps the `RustSec` database when the policy names no path.
fn default_db_dir() -> PathBuf {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"));
    cargo_home.join("advisory-dbs")
}

// ── Dev-loop output ──────────────────────────────────────────────────────────

/// How many critical ids the startup banner names before it truncates.
const BANNER_ID_LIMIT: usize = 5;

/// Lines `autumn dev` prints at startup. Empty means silent.
///
/// Silence is the default: a clean tree, a waived tree, and every state where
/// the policy cannot be evaluated all add nothing to the dev loop. Doctor is
/// where those are reported.
pub fn dev_lines(eval: &Evaluation) -> Vec<String> {
    let Evaluation::Audited { findings, .. } = eval else {
        return Vec::new();
    };
    let Some(worst) = worst_severity(findings) else {
        return Vec::new();
    };
    let active: Vec<&Finding> = findings.iter().filter(|finding| !finding.waived).collect();
    let count = active.len();
    let plural = if count == 1 { "" } else { "s" };

    if worst < Severity::Critical {
        return vec![format!(
            "  \u{26A0}\u{FE0F}  {count} dependency finding{plural} ({} worst) \u{2014} run `autumn doctor` for detail.",
            worst.label()
        )];
    }

    let critical: Vec<&str> = active
        .iter()
        .filter(|finding| finding.severity == Severity::Critical)
        .map(|finding| finding.id.as_str())
        .collect();
    let mut named = critical
        .iter()
        .take(BANNER_ID_LIMIT)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if critical.len() > BANNER_ID_LIMIT {
        let _ = write!(named, ", and {} more", critical.len() - BANNER_ID_LIMIT);
    }
    vec![
        "  \u{26A0}\u{FE0F}  CRITICAL DEPENDENCY ADVISORY".to_owned(),
        format!(
            "     {count} finding{plural}, {} critical: {named}",
            critical.len()
        ),
        "     Run `autumn doctor` for detail. `deny.toml` holds the policy and the waivers."
            .to_owned(),
    ]
}

// ── Evaluation ───────────────────────────────────────────────────────────────

/// Evaluate the dependency policy for the app rooted at `root`.
///
/// Always offline: the advisory database is never fetched here, so this cannot
/// hang and cannot depend on the network. `cargo deny fetch db` refreshes it.
pub fn evaluate(root: &Path) -> Evaluation {
    let Ok(policy) = std::fs::read_to_string(root.join(POLICY_FILE)) else {
        return Evaluation::NoPolicy;
    };
    if !auditor_present() {
        return Evaluation::AuditorMissing;
    }
    let dbs_dir = policy_db_path(&policy).map_or_else(default_db_dir, PathBuf::from);
    let Some(mtime) = newest_db_mtime(&dbs_dir) else {
        return Evaluation::DatabaseMissing;
    };
    let checks = policy_checks(&policy);
    let output = Command::new("cargo")
        .args([
            "deny",
            "--offline",
            "--format",
            "json",
            "--log-level",
            "info",
            "check",
        ])
        .args(&checks)
        .current_dir(root)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => return Evaluation::Unavailable(format!("could not run cargo-deny: {error}")),
    };
    // cargo-deny writes its diagnostic stream to stderr.
    let stream = String::from_utf8_lossy(&output.stderr);
    parse_audit(&stream).map_or_else(
        || Evaluation::Unavailable(first_error(&stream)),
        |findings| Evaluation::Audited {
            findings,
            checks,
            db_age_days: Some(age_days(mtime, SystemTime::now())),
        },
    )
}

/// True when `cargo deny` is on PATH.
fn auditor_present() -> bool {
    Command::new("cargo")
        .args(["deny", "--version"])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The first error the auditor logged, for an evaluation that produced no
/// verdict.
fn first_error(stream: &str) -> String {
    stream
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .find(|entry| {
            entry.get("fields").and_then(|fields| fields.get("level"))
                == Some(&serde_json::Value::String("ERROR".to_owned()))
        })
        .and_then(|entry| {
            Some(
                entry
                    .get("fields")?
                    .get("message")?
                    .as_str()?
                    .lines()
                    .next()?
                    .to_owned(),
            )
        })
        .unwrap_or_else(|| "cargo-deny produced no verdict".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `vulnerability` diagnostic plus the summary that proves the run
    /// finished. Shaped after real `cargo deny 0.20.2 --format json` output.
    const CRITICAL_VULN: &str = r#"
{"fields":{"level":"INFO","message":"checking advisories..."},"type":"log"}
{"fields":{"advisory":{"cvss":"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H","id":"RUSTSEC-2099-0001","informational":null,"package":"badcrate","title":"remote code execution"},"code":"vulnerability","graphs":[{"Krate":{"name":"badcrate","version":"1.2.3"}}],"message":"remote code execution","severity":"error"},"type":"diagnostic"}
{"fields":{"advisories":{"errors":1,"helps":0,"notes":0,"warnings":0}},"type":"summary"}
"#;

    /// A waived advisory: the `advisory-ignored` marker plus the advisory
    /// itself, downgraded to a note.
    const WAIVED_UNMAINTAINED: &str = r#"
{"fields":{"code":"advisory-ignored","labels":[{"column":13,"line":64,"message":"advisory ignored here","span":"RUSTSEC-2024-0384"},{"column":43,"line":64,"message":"ignore reason","span":"instant unmaintained"}],"message":"advisory ignored","severity":"note"},"type":"diagnostic"}
{"fields":{"advisory":{"cvss":null,"id":"RUSTSEC-2024-0384","informational":"unmaintained","package":"instant","title":"`instant` is unmaintained"},"code":"unmaintained","graphs":[{"Krate":{"name":"instant","version":"0.1.13"}}],"message":"`instant` is unmaintained","severity":"note"},"type":"diagnostic"}
{"fields":{"advisories":{"errors":0,"helps":0,"notes":2,"warnings":0}},"type":"summary"}
"#;

    /// A yanked crate the policy only warns about.
    const YANKED_WARNING: &str = r#"
{"fields":{"code":"yanked","graphs":[{"Krate":{"name":"oldcrate","version":"0.3.1"}}],"message":"detected yanked crate","severity":"warning"},"type":"diagnostic"}
{"fields":{"advisories":{"errors":0,"helps":0,"notes":0,"warnings":1}},"type":"summary"}
"#;

    /// Policy violations from the licenses, bans and sources checks.
    const POLICY_VIOLATIONS: &str = r#"
{"fields":{"code":"banned","graphs":[{"Krate":{"name":"serde","version":"1.0.229"}}],"labels":[{"column":20,"line":4,"message":"banned here","span":"serde"}],"message":"crate 'serde = 1.0.229' is explicitly banned","severity":"error"},"type":"diagnostic"}
{"fields":{"code":"rejected","graphs":[{"Krate":{"name":"brotli","version":"8.0.4"}}],"labels":[{"column":12,"line":28,"message":"rejected: license is not explicitly allowed","span":"BSD-3-Clause"}],"message":"failed to satisfy license requirements","severity":"error"},"type":"diagnostic"}
{"fields":{"code":"source-not-allowed","graphs":[{"Krate":{"name":"adler2","version":"2.0.1"}}],"message":"detected 'registry' source not explicitly allowed","severity":"error"},"type":"diagnostic"}
{"fields":{"code":"license-not-encountered","message":"license 'Zlib' was not encountered","severity":"warning"},"type":"diagnostic"}
{"fields":{"licenses":{"errors":2,"helps":0,"notes":0,"warnings":1}},"type":"summary"}
"#;

    /// Informational notes a real run emits in bulk. cargo-deny prints one per
    /// private workspace crate — 94 of them in this workspace.
    const INFORMATIONAL_NOTES: &str = r#"
{"fields":{"code":"skipped-private-workspace-crate","graphs":[{"Krate":{"name":"blog","version":"0.1.0"}}],"message":"skipping private workspace crate 'blog = 0.1.0'","severity":"note"},"type":"diagnostic"}
{"fields":{"code":"skipped-private-workspace-crate","graphs":[{"Krate":{"name":"hello","version":"0.1.0"}}],"message":"skipping private workspace crate 'hello = 0.1.0'","severity":"note"},"type":"diagnostic"}
{"fields":{"licenses":{"errors":0,"helps":0,"notes":2,"warnings":0}},"type":"summary"}
"#;

    /// A real `duplicate` diagnostic: its label span carries one lock entry per
    /// line, so the label text is multi-line.
    const MULTILINE_LABEL: &str = r#"
{"fields":{"code":"duplicate","graphs":[{"Krate":{"name":"base64","version":"0.21.7"}}],"labels":[{"column":1,"line":3,"message":"lock entries","span":"base64 0.21.7 registry+https://github.com/rust-lang/crates.io-index\nbase64 0.22.1 registry+https://github.com/rust-lang/crates.io-index\nbase64 0.23.1 registry+https://github.com/rust-lang/crates.io-index"}],"message":"found 3 duplicate entries for crate 'base64'","severity":"warning"},"type":"diagnostic"}
{"fields":{"bans":{"errors":0,"helps":0,"notes":0,"warnings":1}},"type":"summary"}
"#;

    /// A waived vulnerability. cargo-deny grades a waived advisory down to a
    /// note, so the emitted severity says nothing about the advisory itself.
    const WAIVED_VULNERABILITY: &str = r#"
{"fields":{"code":"advisory-ignored","labels":[{"column":13,"line":6,"message":"advisory ignored here","span":"RUSTSEC-2099-0001"}],"message":"advisory ignored","severity":"note"},"type":"diagnostic"}
{"fields":{"advisory":{"cvss":"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H","id":"RUSTSEC-2099-0001","informational":null,"package":"badcrate","title":"remote code execution"},"code":"vulnerability","graphs":[{"Krate":{"name":"badcrate","version":"1.2.3"}}],"message":"remote code execution","severity":"note"},"type":"diagnostic"}
{"fields":{"advisories":{"errors":0,"helps":0,"notes":2,"warnings":0}},"type":"summary"}
"#;

    /// A vulnerability scored with CVSS v4.0. 64 of the 432 scored advisories
    /// in the `RustSec` database use v4 vectors, which this code does not score.
    const V4_VULNERABILITY: &str = r#"
{"fields":{"advisory":{"cvss":"CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N","id":"RUSTSEC-2099-0002","informational":null,"package":"newcrate","title":"remote code execution"},"code":"vulnerability","graphs":[{"Krate":{"name":"newcrate","version":"2.0.0"}}],"message":"remote code execution","severity":"error"},"type":"diagnostic"}
{"fields":{"advisories":{"errors":1,"helps":0,"notes":0,"warnings":0}},"type":"summary"}
"#;

    fn finding(id: &str, severity: Severity, waived: bool, blocking: bool) -> Finding {
        Finding {
            id: id.to_owned(),
            code: "vulnerability".to_owned(),
            package: "badcrate 1.2.3".to_owned(),
            title: "boom".to_owned(),
            severity,
            waived,
            blocking,
        }
    }

    // ── policy_checks ────────────────────────────────────────────────────────

    #[test]
    fn policy_with_only_advisories_checks_advisories() {
        let policy = "[advisories]\nyanked = \"warn\"\n";
        assert_eq!(policy_checks(policy), vec!["advisories".to_owned()]);
    }

    #[test]
    fn commented_sections_do_not_widen_the_check_list() {
        // The scaffold ships these sections commented out. A commented section
        // must not widen the local check, or doctor would fail where CI passes.
        let policy = "[advisories]\n# [licenses]\n# allow = [\"MIT\"]\n#[bans]\n";
        assert_eq!(policy_checks(policy), vec!["advisories".to_owned()]);
    }

    #[test]
    fn declared_sections_widen_the_check_list_in_command_order() {
        let policy = "[sources]\n[advisories]\n[licenses]\nallow = []\n[bans]\n";
        assert_eq!(
            policy_checks(policy),
            vec![
                "advisories".to_owned(),
                "bans".to_owned(),
                "licenses".to_owned(),
                "sources".to_owned()
            ]
        );
    }

    #[test]
    fn advisories_are_checked_even_when_the_section_is_absent() {
        // Advisories are the gate from #1600. They are never optional.
        assert_eq!(policy_checks("[bans]\n"), vec!["advisories", "bans"]);
    }

    #[test]
    fn a_custom_db_path_is_read_from_the_policy() {
        let policy = "[advisories]\ndb-path = \"/srv/advisory-db\"\n";
        assert_eq!(policy_db_path(policy).as_deref(), Some("/srv/advisory-db"));
        assert_eq!(policy_db_path("[advisories]\n"), None);
        assert_eq!(policy_db_path("# db-path = \"/nope\"\n"), None);
    }

    // ── CVSS v3 base score ───────────────────────────────────────────────────

    #[test]
    fn marvin_attack_vector_scores_medium() {
        // RUSTSEC-2023-0071 / CVE-2023-49092. NVD publishes 5.9 MEDIUM.
        let v = "CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:H/I:N/A:N";
        let score = cvss3_base_score(v).expect("v3.1 vector must parse");
        assert!((score - 5.9).abs() < 0.05, "expected 5.9, got {score}");
        assert_eq!(severity_from_cvss(Some(v)), Some(Severity::Medium));
    }

    #[test]
    fn worst_case_unchanged_scope_vector_scores_critical() {
        let v = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";
        let score = cvss3_base_score(v).expect("v3.1 vector must parse");
        assert!((score - 9.8).abs() < 0.05, "expected 9.8, got {score}");
        assert_eq!(severity_from_cvss(Some(v)), Some(Severity::Critical));
    }

    #[test]
    fn changed_scope_raises_the_score() {
        let v = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H";
        let score = cvss3_base_score(v).expect("v3.1 vector must parse");
        assert!((score - 10.0).abs() < 0.05, "expected 10.0, got {score}");
    }

    #[test]
    fn a_vector_with_no_impact_scores_none() {
        let v = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N";
        assert_eq!(cvss3_base_score(v), Some(0.0));
        assert_eq!(severity_from_cvss(Some(v)), None);
    }

    #[test]
    fn v30_vectors_parse_too() {
        let v = "CVSS:3.0/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H";
        let score = cvss3_base_score(v).expect("v3.0 vector must parse");
        assert!((score - 7.8).abs() < 0.05, "expected 7.8, got {score}");
    }

    #[test]
    fn non_v3_and_malformed_vectors_are_not_scored() {
        // A v4 vector, a truncated vector and prose must all decline rather
        // than invent a band.
        assert_eq!(cvss3_base_score("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N"), None);
        assert_eq!(cvss3_base_score("CVSS:3.1/AV:N/AC:L"), None);
        assert_eq!(cvss3_base_score("high"), None);
        assert_eq!(severity_from_cvss(None), None);
    }

    // ── parse_audit ──────────────────────────────────────────────────────────

    #[test]
    fn a_stream_without_a_summary_is_not_a_verdict() {
        // No summary means the auditor did not finish. Reporting "no findings"
        // there would be a false all-clear.
        assert_eq!(parse_audit(""), None);
        assert_eq!(
            parse_audit(r#"{"fields":{"level":"ERROR","message":"boom"},"type":"log"}"#),
            None
        );
    }

    #[test]
    fn a_clean_run_yields_a_verdict_with_no_findings() {
        let clean = r#"{"fields":{"advisories":{"errors":0,"helps":0,"notes":0,"warnings":0}},"type":"summary"}"#;
        assert_eq!(parse_audit(clean), Some(Vec::new()));
    }

    #[test]
    fn a_vulnerability_carries_its_id_package_title_and_severity() {
        let findings = parse_audit(CRITICAL_VULN).expect("summary present");
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.id, "RUSTSEC-2099-0001");
        assert_eq!(f.code, "vulnerability");
        assert_eq!(f.package, "badcrate 1.2.3");
        assert_eq!(f.title, "remote code execution");
        assert_eq!(f.severity, Severity::Critical);
        assert!(f.blocking);
        assert!(!f.waived);
    }

    #[test]
    fn a_waived_advisory_is_reported_as_waived_not_as_a_failure() {
        let findings = parse_audit(WAIVED_UNMAINTAINED).expect("summary present");
        // The `advisory-ignored` marker is not itself a finding.
        assert_eq!(findings.len(), 1, "got {findings:?}");
        let f = &findings[0];
        assert_eq!(f.id, "RUSTSEC-2024-0384");
        assert!(f.waived);
        assert!(!f.blocking);
    }

    #[test]
    fn a_warned_finding_never_exceeds_medium() {
        let findings = parse_audit(YANKED_WARNING).expect("summary present");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "yanked");
        assert_eq!(findings[0].package, "oldcrate 0.3.1");
        assert!(!findings[0].blocking);
        assert!(findings[0].severity <= Severity::Medium);
    }

    #[test]
    fn a_denied_finding_is_at_least_high() {
        // Severity is consequence: whatever the taxonomy, a finding that fails
        // the CI gate is reported as high or critical.
        let findings = parse_audit(POLICY_VIOLATIONS).expect("summary present");
        let codes: Vec<&str> = findings.iter().map(|f| f.code.as_str()).collect();
        assert_eq!(codes, vec!["banned", "rejected", "source-not-allowed"]);
        for f in &findings {
            assert!(f.blocking, "{} must block", f.code);
            assert!(f.severity >= Severity::High, "{} severity too low", f.code);
        }
    }

    #[test]
    fn a_policy_violation_names_the_crate_and_the_violated_rule() {
        let findings = parse_audit(POLICY_VIOLATIONS).expect("summary present");
        let rejected = findings
            .iter()
            .find(|f| f.code == "rejected")
            .expect("rejected");
        assert_eq!(rejected.package, "brotli 8.0.4");
        assert_eq!(rejected.id, "rejected");
        assert!(
            rejected.title.contains("BSD-3-Clause"),
            "the rejected license must be named: {}",
            rejected.title
        );
    }

    #[test]
    fn config_only_diagnostics_are_not_findings() {
        // `license-not-encountered` reports on the policy file, not the tree.
        let findings = parse_audit(POLICY_VIOLATIONS).expect("summary present");
        assert!(
            findings.iter().all(|f| f.code != "license-not-encountered"),
            "config-only diagnostics must not be reported"
        );
    }

    #[test]
    fn informational_notes_are_not_findings() {
        // Regression: a real run against this workspace emitted 94
        // `skipped-private-workspace-crate` notes. A note the policy did not
        // grade is not a finding, and reporting it buries the ones that are.
        assert_eq!(parse_audit(INFORMATIONAL_NOTES), Some(Vec::new()));
    }

    #[test]
    fn a_waived_advisory_survives_the_note_filter() {
        // Waived advisories are also emitted at note level. They carry an
        // advisory, so they stay reportable — as waived.
        let findings = parse_audit(WAIVED_UNMAINTAINED).expect("summary present");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].waived);
    }

    #[test]
    fn a_finding_is_always_one_line() {
        // Regression: a real `duplicate` diagnostic carries one lock entry per
        // line in its label. Doctor renders one line per finding and caps the
        // list, so a finding that expands to four lines defeats the cap.
        let findings = parse_audit(MULTILINE_LABEL).expect("summary present");
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert!(!finding.title.contains('\n'), "title: {}", finding.title);
        assert!(
            !finding.package.contains('\n'),
            "package: {}",
            finding.package
        );
        assert!(
            finding.title.chars().count() <= TITLE_MAX_CHARS,
            "an unbounded title floods the line: {}",
            finding.title
        );
        assert!(
            finding.title.contains("base64"),
            "the crate must survive the trim: {}",
            finding.title
        );
    }

    #[test]
    fn a_waived_finding_keeps_the_severity_it_would_have_had() {
        // Regression: waiving is not downgrading. A reader has to see what the
        // waiver accepted, so a waived critical stays critical.
        let findings = parse_audit(WAIVED_VULNERABILITY).expect("summary present");
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert!(finding.waived);
        assert!(!finding.blocking);
        assert_eq!(finding.severity, Severity::Critical);
        // It is still not a failure, and still not dev-loop noise.
        assert_eq!(worst_severity(&findings), None);
    }

    #[test]
    fn an_unscorable_vulnerability_is_treated_as_high_not_as_low() {
        // A CVSS v4.0 vector is not scored here. The fallback must be
        // conservative: an unscorable vulnerability still fails doctor, it
        // just does not earn the dev-loop banner.
        let findings = parse_audit(V4_VULNERABILITY).expect("summary present");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::High);
        assert!(findings[0].blocking);
    }

    // ── worst_severity ───────────────────────────────────────────────────────

    #[test]
    fn worst_severity_ignores_waived_findings() {
        let findings = vec![
            finding("RUSTSEC-1", Severity::Critical, true, false),
            finding("RUSTSEC-2", Severity::Medium, false, false),
        ];
        assert_eq!(worst_severity(&findings), Some(Severity::Medium));
        assert_eq!(
            worst_severity(&[finding("RUSTSEC-1", Severity::Critical, true, false)]),
            None
        );
        assert_eq!(worst_severity(&[]), None);
    }

    // ── age_days ─────────────────────────────────────────────────────────────

    #[test]
    fn age_is_whole_days_and_never_negative() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(90 * 86_400);
        let then = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(88 * 86_400 + 100);
        assert_eq!(age_days(then, now), 1);
        // A clock that moved backwards reads as fresh, never as a huge age.
        assert_eq!(age_days(now, then), 0);
    }

    #[test]
    fn a_missing_database_directory_has_no_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(newest_db_mtime(&dir.path().join("absent")), None);
        // An empty directory holds no database either.
        assert_eq!(newest_db_mtime(dir.path()), None);
    }

    #[test]
    fn the_newest_database_checkout_sets_the_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("advisory-db-1")).expect("mkdir");
        assert!(newest_db_mtime(dir.path()).is_some());
    }

    // ── dev_lines ────────────────────────────────────────────────────────────

    #[test]
    fn a_clean_tree_adds_no_lines_to_dev_output() {
        let eval = Evaluation::Audited {
            findings: Vec::new(),
            checks: vec!["advisories".to_owned()],
            db_age_days: Some(1),
        };
        assert!(dev_lines(&eval).is_empty());
    }

    #[test]
    fn dev_stays_silent_when_the_policy_cannot_be_evaluated() {
        // Offline, no auditor, no policy: dev says nothing. Doctor is where
        // those are reported.
        for eval in [
            Evaluation::NoPolicy,
            Evaluation::AuditorMissing,
            Evaluation::DatabaseMissing,
            Evaluation::Unavailable("cargo metadata failed".to_owned()),
        ] {
            assert!(dev_lines(&eval).is_empty(), "{eval:?} must be silent");
        }
    }

    #[test]
    fn dev_stays_silent_when_every_finding_is_waived() {
        let eval = Evaluation::Audited {
            findings: vec![finding("RUSTSEC-1", Severity::Critical, true, false)],
            checks: vec!["advisories".to_owned()],
            db_age_days: Some(1),
        };
        assert!(dev_lines(&eval).is_empty());
    }

    #[test]
    fn a_non_critical_tree_gets_exactly_one_dev_line() {
        let eval = Evaluation::Audited {
            findings: vec![
                finding("RUSTSEC-1", Severity::High, false, true),
                finding("RUSTSEC-2", Severity::Low, false, false),
            ],
            checks: vec!["advisories".to_owned()],
            db_age_days: Some(1),
        };
        let lines = dev_lines(&eval);
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert!(
            lines[0].contains('2'),
            "the count must appear: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("high"),
            "the worst severity must appear: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("autumn doctor"),
            "the line must say where detail lives: {}",
            lines[0]
        );
    }

    #[test]
    fn a_critical_finding_gets_a_loud_banner() {
        let eval = Evaluation::Audited {
            findings: vec![finding(
                "RUSTSEC-2099-0001",
                Severity::Critical,
                false,
                true,
            )],
            checks: vec!["advisories".to_owned()],
            db_age_days: Some(1),
        };
        let lines = dev_lines(&eval);
        assert!(
            lines.len() > 1,
            "a critical finding is not a one-liner: {lines:?}"
        );
        let banner = lines.join("\n");
        assert!(banner.contains("CRITICAL"), "{banner}");
        assert!(banner.contains("RUSTSEC-2099-0001"), "{banner}");
        assert!(banner.contains("autumn doctor"), "{banner}");
    }
}
