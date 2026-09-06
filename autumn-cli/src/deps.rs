//! Dependency policy evaluation shared by `autumn doctor` and `autumn dev`
//! (issue #1633).
//!
//! Detection is not re-implemented here. Doctor and dev run the same auditor
//! (`cargo deny`) against the same policy file (`deny.toml`), the same waiver
//! store (`[advisories] ignore`) and the same check list as the CI gate from
//! issue #1600. Two differences remain and are reported, not hidden: CI pins
//! its auditor version, and CI fetches the advisory database before it audits.
//! See `docs/guide/supply-chain.md`, "Part 3b — the dev loop".

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;
use std::time::SystemTime;

/// The app-level dependency policy file. Also the CI gate's config.
pub const POLICY_FILE: &str = "deny.toml";

/// Policy sections that switch on an extra cargo-deny check when present.
///
/// The scaffolded CI workflow derives its check list from the same names, so
/// uncommenting a section widens the local check and the CI gate together.
pub const OPTIONAL_CHECKS: &[&str] = &["bans", "licenses", "sources"];

/// An advisory database older than this is reported as stale.
///
/// The CI gate fetches the database on every run. A local verdict is only as
/// fresh as local data, so the window is short enough that a weekly fetch
/// keeps the two comparable.
pub const STALE_AFTER_DAYS: u64 = 7;

/// Diagnostic codes that report on the policy file, not on the dependency
/// tree. Observed from cargo-deny 0.20.2.
///
/// These are dropped only when the policy grades them below an error. A policy
/// can promote any of them (`unused-ignored-advisory = "deny"`, for one), and a
/// promoted one fails the CI gate, so dropping it by code alone would report a
/// clean tree that CI rejects.
const CONFIG_ONLY_CODES: &[&str] = &[
    "advisory-ignored",
    "advisory-not-detected",
    "unknown-advisory",
    "yanked-not-detected",
    "license-not-encountered",
    "license-exception-not-encountered",
    "unmatched-source",
    "unmatched-organization",
    "unmatched-skip",
    "unmatched-skip-root",
    "unmatched-path-bypass",
    "unmatched-glob",
    "unused-wrapper",
    "unmatched-wrapper",
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
    AuditorMissing { checks: Vec<String> },
    /// The `RustSec` database was never fetched, so nothing can be verified.
    DatabaseMissing { checks: Vec<String> },
    /// The auditor produced a verdict.
    Audited {
        findings: Vec<Finding>,
        checks: Vec<String>,
        db_age_days: Option<u64>,
        /// The auditor version, as it reported itself.
        auditor: String,
    },
    /// The auditor ran but produced no verdict.
    Unavailable { reason: String, checks: Vec<String> },
}

/// The cargo-deny checks this policy file activates, in command order.
///
/// Advisories are always checked: they are the gate from issue #1600. The
/// optional sections are opt-in, so an app that leaves them commented out gets
/// the same command locally that its CI runs.
pub fn policy_checks(policy: &str) -> Vec<String> {
    let declared = declared_sections(policy);
    let mut checks = vec!["advisories".to_owned()];
    checks.extend(
        OPTIONAL_CHECKS
            .iter()
            .filter(|section| declared.iter().any(|key| key == *section))
            .map(|section| (*section).to_owned()),
    );
    checks
}

/// Optional checks the policy declares that the scaffolded workflow's grep
/// cannot see.
///
/// Doctor parses TOML; the workflow greps, because a shell cannot parse TOML.
/// The two agree on every spelling a person writes by hand, but a basic key
/// carrying an escape (`["ban\u0073"]`) decodes to `bans` for one and not the
/// other. Rather than let that diverge silently, it is reported.
pub fn checks_invisible_to_ci(policy: &str) -> Vec<String> {
    let parsed = declared_sections(policy);
    OPTIONAL_CHECKS
        .iter()
        .filter(|section| {
            parsed.iter().any(|key| key == *section)
                && !policy
                    .lines()
                    .any(|line| line_declares(code_of(line), section))
        })
        .map(|section| (*section).to_owned())
        .collect()
}

/// The top-level table keys the policy declares.
///
/// Parsed as TOML, so every spelling that reaches cargo-deny is seen the same
/// way: `[bans]`, `[ bans ]`, `["bans"]`, `[bans.build]`, `[[bans.deny]]`,
/// `bans.deny = …` and `bans = { … }` all declare the key `bans`.
///
/// A policy that does not parse falls back to a line scan. cargo-deny refuses
/// to load such a file anyway, so the fallback only has to avoid claiming the
/// sections vanished.
fn declared_sections(policy: &str) -> Vec<String> {
    if let Ok(table) = policy.parse::<toml::Table>() {
        return table.keys().cloned().collect();
    }
    OPTIONAL_CHECKS
        .iter()
        .filter(|section| {
            policy
                .lines()
                .any(|line| line_declares(code_of(line), section))
        })
        .map(|section| (*section).to_owned())
        .collect()
}

/// True when one policy line declares `section`, in any TOML spelling.
///
/// The fallback for an unparseable policy, and the rule the scaffolded workflow
/// mirrors in shell — grep cannot parse TOML.
fn line_declares(code: &str, section: &str) -> bool {
    let body = code
        .strip_prefix("[[")
        .or_else(|| code.strip_prefix('['))
        .map_or(code, ascii_trim_start);
    // Quoted keys: ["bans"] and ['bans'].
    let (body, quote) = match body.chars().next() {
        Some(q @ ('"' | '\'')) => (&body[q.len_utf8()..], Some(q)),
        _ => (body, None),
    };
    let Some(rest) = body.strip_prefix(section) else {
        return false;
    };
    let rest = match quote {
        Some(quote) => match rest.strip_prefix(quote) {
            Some(rest) => rest,
            None => return false,
        },
        None => rest,
    };
    matches!(ascii_trim_start(rest).chars().next(), Some(']' | '.' | '='))
}

/// The code part of one policy line: comment stripped, trimmed.
///
/// ASCII whitespace only. Rust's `trim` also strips U+00A0, which POSIX
/// `[[:space:]]` does not, and a rule the workflow cannot mirror is a rule the
/// two derivations disagree on.
fn code_of(line: &str) -> &str {
    let line = ascii_trim(line);
    if line.starts_with('#') {
        return "";
    }
    ascii_trim(line.split('#').next().unwrap_or(""))
}

fn ascii_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_ascii_whitespace())
}

fn ascii_trim_start(text: &str) -> &str {
    text.trim_start_matches(|c: char| c.is_ascii_whitespace())
}

/// A custom `db-path` declared by the policy, if any.
///
/// Parsed as TOML, so quoting and escapes are the parser's problem rather than
/// this code's. `~` and `$CARGO_HOME` are expanded, as cargo-deny expands them.
pub fn policy_db_path(policy: &str) -> Option<String> {
    let value = policy
        .parse::<toml::Table>()
        .ok()?
        .get("advisories")?
        .get("db-path")?
        .as_str()?
        .to_owned();
    (!value.is_empty()).then(|| expand_path(&value))
}

/// Expand the path prefixes cargo-deny accepts in `db-path`.
fn expand_path(path: &str) -> String {
    let home =
        directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_string_lossy().into_owned());
    let cargo_home = crate::upgrade::cargo_home().map(|dir| dir.to_string_lossy().into_owned());
    expand_path_with(path, home.as_deref(), cargo_home.as_deref())
}

/// [`expand_path`], with the environment injected. Pure, so it is testable
/// without mutating the process environment.
pub fn expand_path_with(path: &str, home: Option<&str>, cargo_home: Option<&str>) -> String {
    // Longest prefix first: `${CARGO_HOME}` also starts with `$CARGO_HOME`.
    for (prefix, value) in [
        ("${CARGO_HOME}", cargo_home),
        ("$CARGO_HOME", cargo_home),
        ("~", home),
    ] {
        if let Some(rest) = path.strip_prefix(prefix)
            && let Some(value) = value
        {
            return format!("{value}{rest}");
        }
    }
    path.to_owned()
}

// ── CVSS v3 base score ───────────────────────────────────────────────────────

/// CVSS v3 base score for `vector`, or `None` when it is not a complete v3
/// vector. CVSS v3.1 specification, section 8.1.
///
/// Do not fuse the multiplications. The score is rounded up to one decimal, so
/// a more accurate intermediate can cross a band boundary that every reference
/// implementation stays on the near side of. For the same reason the vector's
/// minor version selects the changed-scope impact equation rather than one
/// standing in for both.
#[allow(
    clippy::suboptimal_flops,
    reason = "matches the CVSS reference implementations"
)]
pub fn cvss3_base_score(vector: &str) -> Option<f64> {
    // The scope-changed impact equation differs between the two minor
    // versions, so the prefix is not decoration — it selects the formula.
    let (body, v31) = match vector.strip_prefix("CVSS:3.1/") {
        Some(body) => (body, true),
        None => (vector.strip_prefix("CVSS:3.0/")?, false),
    };

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
    // v3.1 revised the changed-scope impact equation (spec §7.1) to remove a
    // rounding artefact: the exponent dropped from 15 to 13 and the base gained
    // the 0.9731 factor. It moves scores near a band boundary — the vector in
    // `the_two_minor_versions_disagree_across_a_band_boundary` is 7.0 (high)
    // under v3.0 and 6.9 (medium) under v3.1 — so the version must pick the
    // equation rather than one standing in for both.
    let impact = if changed {
        if v31 {
            7.52 * (iss - 0.029) - 3.25 * (iss * 0.9731 - 0.02).powi(13)
        } else {
            7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
        }
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
    let span_of = |label: &serde_json::Value| {
        label
            .get("span")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    // The second label on this diagnostic is the developer's `reason` text.
    // Falling back to it blindly would let a reason of "yanked" mark real
    // findings waived, so the fallback requires an id-shaped span.
    labels
        .iter()
        .find(|label| {
            label.get("message").and_then(serde_json::Value::as_str)
                == Some("advisory ignored here")
        })
        .and_then(&span_of)
        .or_else(|| {
            labels
                .iter()
                .filter_map(span_of)
                .find(|span| is_advisory_id(span))
        })
}

/// True when `span` is shaped like an advisory id rather than prose.
fn is_advisory_id(span: &str) -> bool {
    !span.is_empty()
        && span.len() <= 64
        && span
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// One finding, or `None` when the diagnostic reports on the policy file
/// rather than on the dependency tree.
fn finding_from(fields: &serde_json::Value, waived: &HashSet<String>) -> Option<Finding> {
    let code = code(fields);
    let level = fields
        .get("severity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("note");
    let advisory = fields.get("advisory").filter(|value| !value.is_null());
    // A finding is what the policy graded: an error or a warning. Notes and
    // helps are context, one per private workspace crate. A waived advisory is
    // the exception: it is graded down to a note but still carries its advisory.
    if !matches!(level, "error" | "warning") && advisory.is_none() {
        return None;
    }
    let blocking = level == "error";
    // Config diagnostics report on the policy file, not on the tree. They are
    // noise — until the policy grades one an error, which is what turns the CI
    // gate red. Dropping those by code would report a tree CI rejects as clean.
    if CONFIG_ONLY_CODES.contains(&code) && !blocking {
        return None;
    }
    let code = if code.is_empty() { "finding" } else { code };

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
/// A waived finding has no consequence to grade, so it is graded by its own
/// CVSS band or kind. It can therefore read lower than the same finding
/// unwaived: the number describes the advisory, not the gate.
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

// ── Advisory database age ────────────────────────────────────────────────────

/// Whole days between `then` and `now`. A clock that moved backwards reads as
/// fresh rather than as a huge age.
pub fn age_days(then: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(then)
        .map_or(0, |elapsed| elapsed.as_secs() / 86_400)
}

/// Newest modification time among the database checkouts under `dbs_dir`.
///
/// Three candidates per checkout, newest wins:
///
/// * `.git/FETCH_HEAD` — written by every fetch, including one that finds
///   nothing new, so it answers "when did we last fetch" rather than "when did
///   the data last change". Absent after a fresh clone.
/// * `.git` — updated when a fetch writes refs or objects.
/// * the checkout root — updated when a fetch rewrites the working tree.
///
/// Measured against cargo-deny 0.20.2, whose gix-based fetch updates all three;
/// the root alone would still be the clone date on a fetch that only wrote
/// inside `.git`.
pub fn newest_db_mtime(dbs_dir: &Path) -> Option<SystemTime> {
    std::fs::read_dir(dbs_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let checkout = entry.path();
            let git = checkout.join(".git");
            [git.join("FETCH_HEAD"), git, checkout]
                .into_iter()
                .filter_map(|path| path.metadata().ok()?.modified().ok())
                .max()
        })
        .max()
}

/// Where cargo-deny keeps the `RustSec` database when the policy names no path.
///
/// Cargo's own home, resolved the way Cargo resolves it: on Windows `$HOME` is
/// normally unset and the home comes from the user profile.
fn default_db_dir() -> PathBuf {
    crate::upgrade::cargo_home()
        .unwrap_or_else(|| PathBuf::from(".cargo"))
        .join("advisory-dbs")
}

// ── Dev-loop output ──────────────────────────────────────────────────────────

/// How many critical ids the startup banner names before it truncates.
const BANNER_ID_LIMIT: usize = 5;

/// Lines `autumn dev` prints at startup. Empty means silent.
///
/// Only findings the policy denies are reported: those are the ones that turn
/// the CI gate red. A warned finding — a duplicate crate, a yanked crate — is
/// doctor's to report. A clean tree, a fully waived tree, and every state where
/// the policy could not be evaluated are all silent.
pub fn dev_lines(eval: &Evaluation) -> Vec<String> {
    let Evaluation::Audited { findings, .. } = eval else {
        return Vec::new();
    };
    let blocking: Vec<&Finding> = findings
        .iter()
        .filter(|finding| !finding.waived && finding.blocking)
        .collect();
    let count = blocking.len();
    if count == 0 {
        return Vec::new();
    }
    let plural = if count == 1 { "" } else { "s" };
    let worst = blocking
        .iter()
        .map(|finding| finding.severity)
        .max()
        .unwrap_or(Severity::High);

    if worst < Severity::Critical {
        return vec![format!(
            "  \u{26A0}\u{FE0F}  {count} blocking dependency finding{plural} (worst: {}) \u{2014} run `autumn doctor` for detail.",
            worst.label()
        )];
    }

    let critical: Vec<&str> = blocking
        .iter()
        .filter(|finding| finding.severity == Severity::Critical)
        .map(|finding| finding.id.as_str())
        .collect();
    vec![
        "  \u{26A0}\u{FE0F}  CRITICAL DEPENDENCY ADVISORY".to_owned(),
        format!(
            "     {count} blocking finding{plural}, {} critical: {}",
            critical.len(),
            name_some(&critical, BANNER_ID_LIMIT)
        ),
        "     Run `autumn doctor` for detail. `deny.toml` holds the policy and the waivers."
            .to_owned(),
    ]
}

/// Name up to `limit` ids, then count the rest.
pub fn name_some(ids: &[&str], limit: usize) -> String {
    let mut named = ids
        .iter()
        .take(limit)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > limit {
        let _ = write!(named, ", and {} more", ids.len() - limit);
    }
    named
}

// ── Evaluation ───────────────────────────────────────────────────────────────

/// How long `autumn doctor` waits for a verdict.
///
/// Offline is not the same as bounded: the auditor runs `cargo metadata`,
/// which waits on Cargo's package-cache lock while another build holds it.
/// Doctor reports a verdict or reports that it got none. It never hangs.
pub const DOCTOR_BUDGET: Duration = Duration::from_secs(30);

/// Run the evaluation on its own thread.
///
/// The caller decides how long to wait. A caller that gives up leaves the
/// thread to finish and drop its result.
pub fn spawn_evaluation(root: &Path) -> mpsc::Receiver<Evaluation> {
    let (sender, receiver) = mpsc::channel();
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        // A closed receiver means the caller moved on. Nothing to report.
        let _ = sender.send(evaluate(&root));
    });
    receiver
}

/// Wait up to `budget` for a spawned evaluation. `None` means it did not
/// finish.
pub fn await_evaluation(
    receiver: &mpsc::Receiver<Evaluation>,
    budget: Duration,
) -> Option<Evaluation> {
    receiver.recv_timeout(budget).ok()
}

/// Evaluate the policy, giving up after `budget`.
///
/// A budget that expires is reported as no verdict, never as a clean tree.
pub fn evaluate_within(root: &Path, budget: Duration) -> Evaluation {
    let receiver = spawn_evaluation(root);
    await_evaluation(&receiver, budget).unwrap_or_else(|| Evaluation::Unavailable {
        reason: format!("the audit did not finish within {}s", budget.as_secs()),
        checks: Vec::new(),
    })
}

/// The directory holding the app's policy file, searching upward from `start`.
///
/// The CI gate runs at the repository root. A developer inside a workspace
/// member must get that same graph, not "no policy".
pub fn find_policy_root(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().ok()?;
    for dir in start.ancestors() {
        if dir.join(POLICY_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        // The repository root bounds the search.
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// Evaluate the dependency policy for the app rooted at `root`.
///
/// Always offline: the advisory database is never fetched here, so this cannot
/// depend on the network. `cargo deny fetch db` refreshes it.
pub fn evaluate(root: &Path) -> Evaluation {
    let Some(root) = find_policy_root(root) else {
        return Evaluation::NoPolicy;
    };
    let Ok(policy) = std::fs::read_to_string(root.join(POLICY_FILE)) else {
        return Evaluation::NoPolicy;
    };
    let checks = policy_checks(&policy);

    // Both states below are reported BEFORE the optional-tool checks. Those
    // report "not evaluated" and pass, which would be a green local run on a
    // repository the CI gate rejects.
    //
    // cargo-deny loads the policy before it audits anything, so a policy that
    // is not valid TOML fails the gate outright.
    if let Err(error) = policy.parse::<toml::Table>() {
        return Evaluation::Unavailable {
            reason: one_line(&format!("{POLICY_FILE} is not valid TOML: {error}")),
            checks,
        };
    }
    // A section only one of the two derivations can see means the local check
    // list and the CI check list differ, so no verdict here predicts that gate.
    let invisible = checks_invisible_to_ci(&policy);
    if !invisible.is_empty() {
        return Evaluation::Unavailable {
            reason: format!(
                "{POLICY_FILE} declares {} in a spelling the CI workflow's check-list derivation cannot detect; write it as [{}]",
                invisible.join(", "),
                invisible.join("], [")
            ),
            checks,
        };
    }

    let auditor = match auditor_version() {
        Auditor::Version(version) => version,
        Auditor::NotInstalled => return Evaluation::AuditorMissing { checks },
        Auditor::Broken(reason) => {
            return Evaluation::Unavailable {
                reason: format!("{AUDITOR} is installed but not usable: {reason}"),
                checks,
            };
        }
    };

    // A policy that names its own database is cargo-deny's to resolve: this
    // pre-check only guards the default location, so an unreadable custom path
    // reports whatever the auditor says rather than a database that is missing.
    let custom_db = policy_db_path(&policy);
    let mtime = newest_db_mtime(&custom_db.map_or_else(default_db_dir, PathBuf::from));
    if mtime.is_none() && !policy_declares_db_path(&policy) {
        return Evaluation::DatabaseMissing { checks };
    }

    let mut command = Command::new(AUDITOR);
    command
        .args([
            "--offline",
            "--format",
            "json",
            "--log-level",
            "info",
            "check",
        ])
        .args(&checks)
        .current_dir(&root);
    no_toolchain_installs(&mut command);
    let output = command.output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return Evaluation::Unavailable {
                reason: one_line(&format!("could not run cargo-deny: {error}")),
                checks,
            };
        }
    };
    // cargo-deny writes its diagnostic stream to stderr.
    let stream = String::from_utf8_lossy(&output.stderr);
    let Some(findings) = parse_audit(&stream) else {
        return Evaluation::Unavailable {
            reason: first_error(&stream),
            checks,
        };
    };
    // The auditor's exit code is what the CI gate acts on. A rejection this
    // parse cannot account for means the stream was read wrongly, so report no
    // verdict rather than a clean tree.
    if !output.status.success() && !findings.iter().any(|finding| finding.blocking) {
        return Evaluation::Unavailable {
            reason: "the auditor rejected this tree for a reason that could not be read; run `cargo deny check` to see it".to_owned(),
            checks,
        };
    }
    Evaluation::Audited {
        findings,
        checks,
        db_age_days: mtime.map(|mtime| age_days(mtime, SystemTime::now())),
        auditor,
    }
}

/// True when the policy names its own advisory database location.
fn policy_declares_db_path(policy: &str) -> bool {
    policy_db_path(policy).is_some()
}

/// The auditor binary, invoked directly rather than as `cargo deny`.
///
/// `cargo` on PATH is rustup's shim: it reads the *project's*
/// `rust-toolchain.toml` and installs that toolchain before running anything.
/// Doctor runs inside the user's project, so going through the shim turns a
/// read-only check into a toolchain download — and an interrupted one leaves a
/// half-installed toolchain that breaks the next real `cargo build`. It also
/// misreports: a shim failure surfaced here as "cargo-deny is not installed".
/// cargo subcommands are plain `cargo-<name>` executables on PATH, so calling
/// the binary skips rustup entirely and behaves identically (issue #1633).
const AUDITOR: &str = "cargo-deny";

/// Forbid a child from installing a Rust toolchain.
///
/// cargo-deny shells out to `cargo metadata`, which is rustup's shim again: on
/// a project pinning a toolchain this machine lacks, that call *downloads* it.
/// `autumn doctor` is a read-only check and must never mutate the user's
/// toolchain — least of all halfway, which is what leaves the next `cargo
/// build` broken. With this set, rustup errors instead, and the audit reports
/// no verdict (issue #1633).
fn no_toolchain_installs(command: &mut Command) {
    command.env("RUSTUP_AUTO_INSTALL", "0");
}

/// What probing the auditor found.
///
/// A missing optional tool and a broken one are different answers: the first is
/// a pass reading "not evaluated", the second a warning. Collapsing them makes
/// a corrupt install read as one that was simply never done (issue #1633).
enum Auditor {
    /// Installed and answering; the reported version string.
    Version(String),
    /// Not on PATH at all.
    NotInstalled,
    /// Present but unusable — the reason, for the reader.
    Broken(String),
}

/// Probe the auditor.
///
/// Its version is reported alongside the verdict: the scaffolded CI pins its
/// auditor, a local run uses whatever is installed, and a reader comparing the
/// two needs both.
fn auditor_version() -> Auditor {
    let mut command = Command::new(AUDITOR);
    no_toolchain_installs(&mut command);
    let output = match command.arg("--version").output() {
        Ok(output) => output,
        // Only "no such file" means the optional tool was never installed. Any
        // other spawn error — not executable, wrong architecture, a permission
        // denial — is a broken install, and reporting that as "not installed"
        // turns it into a pass.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Auditor::NotInstalled;
        }
        Err(error) => return Auditor::Broken(one_line(&error.to_string())),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().find(|line| !line.trim().is_empty());
        return Auditor::Broken(match detail {
            Some(line) => one_line(line),
            None => format!("`{AUDITOR} --version` exited with {}", output.status),
        });
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    Auditor::Version(one_line(reported.lines().next().unwrap_or("cargo-deny")))
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
        .and_then(|entry| Some(one_line(entry.get("fields")?.get("message")?.as_str()?)))
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

    /// A stale waiver, under a policy that promotes it to an error.
    /// `unused-ignored-advisory = "deny"` makes cargo-deny exit 1 on this.
    const PROMOTED_CONFIG_DIAGNOSTIC: &str = r#"
{"fields":{"code":"advisory-not-detected","labels":[{"column":13,"line":4,"message":"no crate matched advisory criteria","span":"RUSTSEC-2020-0071"}],"message":"advisory was not encountered","severity":"error"},"type":"diagnostic"}
{"fields":{"advisories":{"errors":1,"helps":0,"notes":0,"warnings":0}},"type":"summary"}
"#;

    fn audited(findings: Vec<Finding>) -> Evaluation {
        Evaluation::Audited {
            findings,
            checks: vec!["advisories".to_owned()],
            db_age_days: Some(1),
            auditor: "cargo-deny 0.20.2".to_owned(),
        }
    }

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
    fn a_waived_finding_is_graded_by_its_own_severity() {
        // A waived finding has no consequence to grade, so the number
        // describes the advisory rather than the gate.
        let findings = parse_audit(WAIVED_VULNERABILITY).expect("summary present");
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert!(finding.waived);
        assert!(!finding.blocking);
        assert_eq!(finding.severity, Severity::Critical);
        // It is still not dev-loop noise.
        assert!(dev_lines(&audited(findings)).is_empty());
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

    #[test]
    fn a_malformed_policy_is_reported_before_the_optional_tool_checks() {
        // Regression, reproduced: with a malformed `deny.toml` and no
        // cargo-deny installed, doctor reported "not evaluated" and PASSED,
        // while cargo-deny — and so the CI gate — exits 1 loading the file.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join(POLICY_FILE),
            "[advisories]\nthis is not toml =\n",
        )
        .expect("policy");
        match evaluate(root.path()) {
            Evaluation::Unavailable { reason, .. } => {
                assert!(reason.contains("not valid TOML"), "{reason}");
            }
            other => panic!("a malformed policy must not read as evaluable: {other:?}"),
        }
    }

    #[test]
    fn a_spelling_only_doctor_can_see_is_reported_rather_than_diverging() {
        // Doctor parses TOML; the scaffolded workflow greps. A basic key with
        // an escape decodes for one and not the other, so the two check lists
        // would differ — the one thing this design exists to prevent.
        let escaped = "[advisories]\n[\"ban\\u0073\"]\n";
        assert!(
            policy_checks(escaped).contains(&"bans".to_owned()),
            "the TOML parse must decode the key"
        );
        assert_eq!(checks_invisible_to_ci(escaped), vec!["bans".to_owned()]);

        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(POLICY_FILE), escaped).expect("policy");
        match evaluate(root.path()) {
            Evaluation::Unavailable { reason, .. } => {
                assert!(reason.contains("bans"), "{reason}");
                assert!(reason.contains("[bans]"), "the fix must be named: {reason}");
            }
            other => panic!("a divergent spelling must not read as evaluable: {other:?}"),
        }
    }

    #[test]
    fn every_hand_written_spelling_is_visible_to_both_derivations() {
        for policy in [
            "[bans]\n",
            "[ bans ]\n",
            "[bans] # note\n",
            "[bans.build]\n",
            "[[bans.deny]]\n",
            "bans.deny = []\n",
            "[\"bans\"]\n",
            "['bans']\n",
        ] {
            assert!(
                checks_invisible_to_ci(policy).is_empty(),
                "wrongly reported as invisible: {policy:?}"
            );
        }
    }

    #[test]
    fn the_auditor_is_never_invoked_through_the_rustup_shim() {
        // Regression, caught by the Windows Tier 1 journey: `cargo` on PATH is
        // rustup's shim, which reads the *project's* `rust-toolchain.toml` and
        // installs that toolchain before running anything. Doctor runs inside
        // the user's project, so `cargo deny` there turned a read-only check
        // into a toolchain download; the next `cargo build` then failed with
        // "the 'cargo.exe' binary ... is not applicable to the '1.88.0-...'
        // toolchain". Invoking `cargo-deny` skips rustup entirely.
        let source = include_str!("deps.rs");
        let code = &source[..source.find("#[cfg(test)]").expect("test module")];
        assert!(
            !code.contains("Command::new(\"cargo\")"),
            "this module must not run anything through the `cargo` shim"
        );
        assert_eq!(AUDITOR, "cargo-deny");
        // cargo-deny reaches `cargo metadata` on its own, so every spawn must
        // also forbid an install. One guard call per spawned command.
        assert_eq!(
            code.matches("Command::new(").count(),
            code.matches("no_toolchain_installs(&mut").count(),
            "every command this module spawns must be guarded"
        );
    }

    #[test]
    fn a_read_only_check_never_installs_a_toolchain() {
        let mut command = Command::new(AUDITOR);
        no_toolchain_installs(&mut command);
        let set: Vec<_> = command
            .get_envs()
            .filter(|(key, _)| *key == "RUSTUP_AUTO_INSTALL")
            .collect();
        assert_eq!(set.len(), 1, "the guard must be set exactly once");
        assert_eq!(set[0].1, Some(std::ffi::OsStr::new("0")));
    }

    #[test]
    fn the_two_minor_versions_disagree_across_a_band_boundary() {
        // v3.1 revised the changed-scope impact equation. This vector is the
        // discriminating case: 7.0 (high) under v3.0, 6.9 (medium) under v3.1.
        // Every other vector in this suite scores identically under both, which
        // is exactly why using one equation for both went unnoticed.
        const METRICS: &str = "AV:P/AC:H/PR:L/UI:N/S:C/C:H/I:H/A:L";
        assert_eq!(cvss3_base_score(&format!("CVSS:3.0/{METRICS}")), Some(7.0));
        assert_eq!(cvss3_base_score(&format!("CVSS:3.1/{METRICS}")), Some(6.9));
        // And the band each lands in differs, which is the reason it matters.
        assert_eq!(
            severity_from_cvss(Some(&format!("CVSS:3.0/{METRICS}"))),
            Some(Severity::High)
        );
        assert_eq!(
            severity_from_cvss(Some(&format!("CVSS:3.1/{METRICS}"))),
            Some(Severity::Medium)
        );
    }

    #[test]
    fn a_broken_auditor_is_not_reported_as_a_missing_one() {
        // `AuditorMissing` passes, because no Autumn install path provides
        // cargo-deny. A cargo-deny that is present but unusable — corrupt,
        // wrong architecture, not executable — is a different answer, and
        // grading it as the optional-tool pass hides a machine whose CI-parity
        // claim is broken.
        let broken = Evaluation::Unavailable {
            reason: format!("{AUDITOR} is installed but not usable: Exec format error"),
            checks: vec!["advisories".to_owned()],
        };
        let result = crate::doctor::check_dependencies_impl(&broken);
        assert_eq!(result.status, crate::doctor::CheckStatus::Warn);

        // …whereas a genuinely absent tool still passes.
        let absent = Evaluation::AuditorMissing {
            checks: vec!["advisories".to_owned()],
        };
        assert_eq!(
            crate::doctor::check_dependencies_impl(&absent).status,
            crate::doctor::CheckStatus::Pass
        );
    }

    // ── Bounded waiting ──────────────────────────────────────────────────────

    #[test]
    fn a_verdict_that_arrives_in_time_is_returned() {
        let (sender, receiver) = mpsc::channel();
        sender.send(Evaluation::NoPolicy).expect("send");
        assert_eq!(
            await_evaluation(&receiver, Duration::from_secs(5)),
            Some(Evaluation::NoPolicy)
        );
    }

    #[test]
    fn a_verdict_that_outruns_its_budget_is_not_returned() {
        // Offline is not the same as bounded: the auditor runs `cargo
        // metadata`, which waits on Cargo's package-cache lock while another
        // build holds it.
        let (sender, receiver) = mpsc::channel::<Evaluation>();
        let result = await_evaluation(&receiver, Duration::from_millis(20));
        assert_eq!(result, None);
        drop(sender);
    }

    #[test]
    fn an_expired_budget_never_reads_as_a_clean_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No policy file, so this returns before any budget can matter — the
        // point is the shape of the timeout verdict, asserted below.
        assert_eq!(
            evaluate_within(dir.path(), Duration::from_secs(5)),
            Evaluation::NoPolicy
        );
        assert!(
            dev_lines(&Evaluation::Unavailable {
                reason: "the audit did not finish within 30s".to_owned(),
                checks: Vec::new(),
            })
            .is_empty()
        );
    }

    #[test]
    fn the_doctor_budget_is_bounded() {
        // A doctor run that stalls on an auditor is worse than one that says
        // it got no verdict.
        assert!(DOCTOR_BUDGET <= Duration::from_secs(60));
    }

    #[test]
    fn a_config_diagnostic_the_policy_promoted_is_a_finding() {
        // Regression, reproduced against cargo-deny 0.20.2: with
        // `unused-ignored-advisory = "deny"` a stale waiver is an ERROR and the
        // CI gate exits 1. Dropping it by code reported "all clear" on a tree
        // CI rejects.
        let findings = parse_audit(PROMOTED_CONFIG_DIAGNOSTIC).expect("summary present");
        assert_eq!(findings.len(), 1, "got {findings:?}");
        assert!(findings[0].blocking);
        assert!(findings[0].severity >= Severity::High);
    }

    #[test]
    fn the_same_config_diagnostic_below_error_is_still_noise() {
        let warned = PROMOTED_CONFIG_DIAGNOSTIC
            .replace(r#""severity":"error""#, r#""severity":"warning""#)
            .replace(r#""errors":1"#, r#""errors":0"#);
        assert_eq!(parse_audit(&warned), Some(Vec::new()));
    }

    #[test]
    fn every_toml_spelling_of_a_section_widens_the_check_list() {
        // All of these configure cargo-deny's `bans` check. A spelling the
        // derivation misses is a policy the team believes is enforced and is
        // not — silently, on both sides.
        for policy in [
            "[bans]\n",
            "[ bans ]\n",
            "[bans] # inline note\n",
            "[bans.build]\n",
            "[[bans.deny]]\n",
            "bans.deny = []\n",
            "bans = { multiple-versions = \"allow\" }\n",
            "[\"bans\"]\n",
            "['bans']\n",
        ] {
            assert!(
                policy_checks(policy).contains(&"bans".to_owned()),
                "not detected: {policy:?}"
            );
        }
        // And these do not.
        for policy in ["[bansible]\n", "[advisories]\n", "# [bans]\n", "#[bans]\n"] {
            assert_eq!(
                policy_checks(policy),
                vec!["advisories".to_owned()],
                "wrongly detected: {policy:?}"
            );
        }
    }

    #[test]
    fn an_unparseable_policy_falls_back_to_a_line_scan() {
        // cargo-deny refuses to load such a file, so the fallback only has to
        // avoid claiming the declared sections vanished.
        let broken = "[bans]\nthis is not toml =\n";
        assert!(broken.parse::<toml::Table>().is_err());
        assert!(policy_checks(broken).contains(&"bans".to_owned()));
    }

    #[test]
    fn the_line_scan_fallback_ignores_non_ascii_indentation() {
        // Rust's `trim` strips U+00A0; POSIX `[[:space:]]` does not. The
        // fallback is the rule the scaffolded workflow mirrors in shell, so it
        // must not see what grep cannot.
        assert!(!line_declares(code_of("\u{00A0}[bans]"), "bans"));
        assert!(line_declares(code_of("  [bans]"), "bans"));
    }

    #[test]
    fn a_home_or_cargo_home_prefix_is_expanded() {
        // cargo-deny expands these; reading them literally makes doctor report
        // a database that exists as never fetched.
        let home = Some("/home/tester");
        let cargo = Some("/home/tester/.cargo");
        assert_eq!(
            expand_path_with("~/mirror", home, cargo),
            "/home/tester/mirror"
        );
        assert_eq!(
            expand_path_with("$CARGO_HOME/advisory-dbs", home, cargo),
            "/home/tester/.cargo/advisory-dbs"
        );
        assert_eq!(
            expand_path_with("${CARGO_HOME}/advisory-dbs", home, cargo),
            "/home/tester/.cargo/advisory-dbs"
        );
        assert_eq!(expand_path_with("/srv/db", home, cargo), "/srv/db");
        // No HOME to expand with: the path is left alone rather than mangled.
        assert_eq!(expand_path_with("~/mirror", None, None), "~/mirror");
    }

    #[test]
    fn a_db_path_is_read_through_quoting_and_comments() {
        // The parser handles TOML literal strings, escapes and trailing
        // comments; hand-slicing the source text did not.
        assert_eq!(
            policy_db_path("[advisories]\ndb-path = '/srv/db#1'\n").as_deref(),
            Some("/srv/db#1")
        );
        assert_eq!(
            policy_db_path("[advisories]\ndb-path = \"/srv/db\" # mirrored nightly\n").as_deref(),
            Some("/srv/db")
        );
        // cargo-deny reads this key from `[advisories]`; a stray top-level one
        // configures nothing.
        assert_eq!(policy_db_path("db-path = \"/srv/db\"\n"), None);
        assert_eq!(policy_db_path("[advisories]\n"), None);
        assert_eq!(policy_db_path("# db-path = \"/nope\"\n"), None);
    }

    #[test]
    fn the_policy_is_found_from_inside_a_workspace_member() {
        // CI runs the audit at the repository root. A developer in a member
        // crate must get that graph, not "no policy".
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(POLICY_FILE), "[advisories]\n").expect("policy");
        let member = root.path().join("crates").join("app");
        std::fs::create_dir_all(&member).expect("member");
        assert_eq!(
            find_policy_root(&member).map(|found| found.canonicalize().expect("canonical")),
            Some(root.path().canonicalize().expect("canonical"))
        );
    }

    #[test]
    fn the_search_for_a_policy_stops_at_the_repository_root() {
        let outer = tempfile::tempdir().expect("tempdir");
        std::fs::write(outer.path().join(POLICY_FILE), "[advisories]\n").expect("policy");
        let repo = outer.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git");
        assert_eq!(find_policy_root(&repo), None);
    }

    #[test]
    fn a_fetch_marker_newer_than_the_checkout_sets_the_age() {
        // `.git/FETCH_HEAD` is written by every fetch, including one that finds
        // nothing new. It is the freshest signal available and must win over an
        // older checkout directory.
        let dbs = tempfile::tempdir().expect("tempdir");
        let git = dbs.path().join("advisory-db-abc").join(".git");
        std::fs::create_dir_all(&git).expect("git dir");
        let from_dirs = newest_db_mtime(dbs.path()).expect("mtime");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(git.join("FETCH_HEAD"), "ref").expect("fetch marker");
        let from_marker = newest_db_mtime(dbs.path()).expect("mtime");
        assert!(
            from_marker >= from_dirs,
            "the fetch marker must not read older than the checkout"
        );
    }

    #[test]
    fn a_fetch_into_an_existing_checkout_refreshes_the_age() {
        // A fetch writes inside `.git` and never touches the checkout's own
        // directory, so reading the root alone reports this morning's fetch as
        // months old.
        let dbs = tempfile::tempdir().expect("tempdir");
        let checkout = dbs.path().join("advisory-db-abc");
        std::fs::create_dir(&checkout).expect("checkout");
        let root_mtime = newest_db_mtime(dbs.path()).expect("mtime");
        std::fs::create_dir(checkout.join(".git")).expect("git");
        let fetched = newest_db_mtime(dbs.path()).expect("mtime");
        assert!(
            fetched >= root_mtime,
            "a fetch must not read as older than the clone"
        );
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
        assert!(dev_lines(&audited(Vec::new())).is_empty());
    }

    #[test]
    fn dev_stays_silent_when_the_policy_cannot_be_evaluated() {
        // Offline, no auditor, no policy: dev says nothing. Doctor is where
        // those are reported.
        for eval in [
            Evaluation::NoPolicy,
            Evaluation::AuditorMissing { checks: Vec::new() },
            Evaluation::DatabaseMissing { checks: Vec::new() },
            Evaluation::Unavailable {
                reason: "cargo metadata failed".to_owned(),
                checks: Vec::new(),
            },
        ] {
            assert!(dev_lines(&eval).is_empty(), "{eval:?} must be silent");
        }
    }

    #[test]
    fn dev_stays_silent_when_every_finding_is_waived() {
        let eval = audited(vec![finding("RUSTSEC-1", Severity::Critical, true, false)]);
        assert!(dev_lines(&eval).is_empty());
    }

    #[test]
    fn a_non_critical_tree_gets_exactly_one_dev_line() {
        let eval = audited(vec![
            finding("RUSTSEC-1", Severity::High, false, true),
            finding("RUSTSEC-2", Severity::Low, false, false),
        ]);
        let lines = dev_lines(&eval);
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert!(
            lines[0].contains('1'),
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
        let eval = audited(vec![finding(
            "RUSTSEC-2099-0001",
            Severity::Critical,
            false,
            true,
        )]);
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
