//! `autumn db retention` — report, dry-run, and on-demand purge of the
//! unified data-retention policy for framework-owned data (issue #1605).
//!
//! Every number this command prints comes from the application binary, not
//! from the CLI: it compiles and runs the app with
//! `AUTUMN_DB_RETENTION=report|purge` and reads the JSON report from the one
//! line of stdout starting with `AUTUMN_DB_RETENTION_REPORT=`. That is deliberate
//! rather than convenient — the policy depends on the app's own resolved
//! config, its GDPR legal-hold registrations, and its installed audit sinks,
//! none of which the standalone CLI can see. Running it in-app means the
//! report and the enforcement come from one code path and cannot drift.
//!
//! Mirrors the `autumn retention --dry-run` (issue #1342) plumbing, which
//! does the same thing for app-declared `#[repository(..., retention(...))]`
//! policies.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// Line prefix framing the app's machine-readable JSON report, matched
/// verbatim against `autumn_web`'s `app::FRAMEWORK_RETENTION_JSON_PREFIX`
/// (crate-private there, so duplicated here rather than shared — the same
/// pattern the `autumn retention` plumbing already uses).
const RETENTION_JSON_PREFIX: &str = "AUTUMN_DB_RETENTION_REPORT=";

/// The env var selecting the app's one-shot mode.
const RETENTION_MODE_ENV: &str = "AUTUMN_DB_RETENTION";

/// The env var carrying `--dataset`.
const RETENTION_DATASET_ENV: &str = "AUTUMN_DB_RETENTION_DATASET";

/// What the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionMode {
    /// Report the effective policy and how many rows are eligible right now.
    /// Deletes nothing. The default.
    Report,
    /// Same as [`Self::Report`], but phrased as "would remove".
    DryRun,
    /// Enforce the policy immediately.
    Purge,
}

impl RetentionMode {
    /// The `AUTUMN_DB_RETENTION` value this mode maps onto.
    ///
    /// `Report` and `DryRun` are the same operation in the app — counting
    /// without deleting — and differ only in how the CLI phrases the result.
    const fn env_value(self) -> &'static str {
        match self {
            Self::Report | Self::DryRun => "report",
            Self::Purge => "purge",
        }
    }
}

/// Options controlling `autumn db retention`.
pub struct RetentionOptions<'a> {
    /// Package to run (for workspaces).
    pub package: Option<&'a str>,
    /// Binary target to run (for packages with multiple bin targets).
    pub bin: Option<&'a str>,
    /// Profile forwarded to the app binary via `AUTUMN_ENV`.
    pub profile: &'a str,
    /// What to do.
    pub mode: RetentionMode,
    /// Restrict to one dataset key (`job_history`, `sessions`, …).
    pub dataset: Option<&'a str>,
    /// Allow `--purge` against a non-dev/test profile.
    pub force: bool,
    /// Print the app's JSON verbatim instead of a table.
    pub json: bool,
}

/// One dataset's row in the report — mirrors
/// `autumn_web::data_retention::RetentionDatasetReport`.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionDatasetReport {
    pub dataset: String,
    pub description: String,
    pub enforcement: String,
    pub window_secs: Option<u64>,
    pub source: String,
    pub cutoff: Option<String>,
    pub eligible_rows: Option<u64>,
    pub rows_removed: u64,
    /// `true` when the run stopped at its per-run batch cap with rows still
    /// stale — the policy was only partially enforced this tick.
    #[serde(default)]
    pub truncated: bool,
    pub dry_run: bool,
    pub skipped: Option<String>,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// Run `autumn db retention`.
pub fn run(opts: &RetentionOptions<'_>) {
    eprintln!("\u{1F342} autumn db retention\n");
    if let Some(refusal) = production_refusal(opts) {
        eprintln!("\u{2717} {refusal}");
        std::process::exit(1);
    }
    crate::routes::compile_binary(opts.package, opts.bin);
    let binary = crate::routes::find_binary(opts.package, opts.bin);

    let mut command = Command::new(&binary);
    clear_competing_one_shot_env(&mut command);
    command
        .env(RETENTION_MODE_ENV, opts.mode.env_value())
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    apply_dataset_env(&mut command, opts.dataset);
    crate::task::apply_managed_pg_env(&mut command, opts.package);

    let output = command.output().unwrap_or_else(|error| {
        eprintln!("\u{2717} Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(json) = extract_report_json(&stdout) else {
        eprintln!("Failed to find the retention report in the binary's output.");
        eprintln!(
            "Last output from the app:\n{}",
            tail_for_diagnostics(&stdout)
        );
        // Never inherit a zero exit here: a binary that exits 0 without
        // emitting the report line (one whose `main` short-circuits before
        // `AppBuilder::run`'s dispatch, or one built against an autumn-web
        // that predates this mode) would otherwise make a scripted purge look
        // successful when nothing ran.
        std::process::exit(output.status.code().filter(|code| *code != 0).unwrap_or(1));
    };

    if opts.json {
        println!("{json}");
    } else {
        let reports: Vec<RetentionDatasetReport> =
            serde_json::from_str(json).unwrap_or_else(|error| {
                eprintln!("Failed to parse the retention report JSON: {error}");
                eprintln!("Raw output: {stdout}");
                std::process::exit(1);
            });
        print!("{}", format_report(&reports, opts.mode));
    }

    // The app exits non-zero when any dataset failed; carry that through so a
    // scripted purge cannot look successful after a partial failure.
    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }
}

/// Why an on-demand `--purge` is being refused, if it is.
///
/// `--purge` deletes immediately and irreversibly. Against a non-dev/test
/// profile it requires `--force`, the same guard `autumn db drop` and
/// `autumn db scrub` apply — the scheduled in-process sweep is the intended
/// way to enforce a policy in production, and it needs no flag at all.
/// Read-only modes are never refused.
fn production_refusal(opts: &RetentionOptions<'_>) -> Option<String> {
    if opts.mode != RetentionMode::Purge || opts.force {
        return None;
    }
    let profile = crate::migrate::effective_profile(Some(opts.profile));
    if matches!(profile.as_str(), "dev" | "test") {
        return None;
    }
    Some(format!(
        "Refusing to purge framework data against the {profile:?} profile.\n  \
         The configured [retention] policy already runs automatically inside the app; \
         re-run with --force if you really mean to purge now."
    ))
}

/// Clear the other internal one-shot mode env vars `AppBuilder::run` checks
/// *before* `AUTUMN_DB_RETENTION` in its dispatch chain.
///
/// `Command` inherits the parent environment, so any of these left over in
/// the CLI's own environment (from a wrapping script, or a previous `autumn
/// migrate` / `autumn task run ...` in the same shell) would silently hijack
/// this invocation into a completely different — and potentially mutating —
/// mode. `AUTUMN_REPLAY_CAPSULE` is checked *after* this mode and so is
/// deliberately left alone.
fn clear_competing_one_shot_env(command: &mut Command) {
    for var in [
        "AUTUMN_BUILD_STATIC",
        "AUTUMN_DUMP_ROUTES",
        "AUTUMN_DUMP_CACHE_COHERENCE",
        "AUTUMN_DUMP_DATA_FLOW",
        "AUTUMN_DUMP_JOBS",
        "AUTUMN_LIST_TASKS",
        "AUTUMN_RUN_TASK",
        "AUTUMN_MIGRATE",
        "AUTUMN_RETENTION_DRY_RUN",
    ] {
        command.env_remove(var);
    }
}

/// Set or clear `AUTUMN_DB_RETENTION_DATASET` on `command`.
///
/// Explicitly removed when no `--dataset` was passed, so a value the CLI's
/// own environment happens to carry cannot narrow (or fail) a run the
/// operator asked to cover everything.
fn apply_dataset_env(command: &mut Command, dataset: Option<&str>) {
    if let Some(dataset) = dataset {
        command.env(RETENTION_DATASET_ENV, dataset);
    } else {
        command.env_remove(RETENTION_DATASET_ENV);
    }
}

/// Find the report line in the child's captured stdout and return its JSON
/// payload.
///
/// Not assumed to be the entirety of stdout: the `dev` profile initializes a
/// stdout-backed tracing formatter and Diesel writes migration progress to
/// stdout, both of which can print first. Takes the *last* matching line —
/// the report is printed immediately before the child exits.
fn extract_report_json(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(RETENTION_JSON_PREFIX))
}

/// The last few lines of the child's stdout, for a diagnostic message.
///
/// Not the whole stream: under the default `dev` profile the app initializes
/// a stdout tracing subscriber, so echoing everything would bury the actual
/// problem in startup logging (and dump it into a CI log).
fn tail_for_diagnostics(stdout: &str) -> String {
    const LINES: usize = 20;
    let lines: Vec<&str> = stdout.lines().collect();
    let start = lines.len().saturating_sub(LINES);
    lines[start..].join("\n")
}

/// Render a window in seconds as the most readable whole unit.
fn format_window(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return "forever".to_owned();
    };
    for (unit_secs, suffix) in [(86_400, 'd'), (3_600, 'h'), (60, 'm')] {
        if secs % unit_secs == 0 && secs >= unit_secs {
            return format!("{}{suffix}", secs / unit_secs);
        }
    }
    format!("{secs}s")
}

/// The "rows" cell for one dataset: what is eligible, why it is not
/// countable, or what went wrong.
fn format_rows(report: &RetentionDatasetReport, mode: RetentionMode) -> String {
    if let Some(error) = report.error.as_deref() {
        return format!("error: {error}");
    }
    if mode == RetentionMode::Purge && !report.dry_run {
        // A truncated run left rows behind; saying only "removed N" would read
        // as "the policy is now enforced", which it is not until a later tick
        // drains the rest.
        if report.truncated {
            return format!("removed {} (more remain)", report.rows_removed);
        }
        return format!("removed {}", report.rows_removed);
    }
    report
        .eligible_rows
        .map_or_else(|| "\u{2014}".to_owned(), |rows| rows.to_string())
}

/// Render the report as a fixed-width table.
///
/// Every dataset is listed, including the ones with no window: "which
/// framework tables exist and how long do you keep them" is the question the
/// command exists to answer, and omitting the unconfigured ones would hide
/// exactly the unbounded growth an operator is looking for.
#[must_use]
pub fn format_report(reports: &[RetentionDatasetReport], mode: RetentionMode) -> String {
    if reports.is_empty() {
        return "No framework-owned datasets are registered.\n".to_owned();
    }

    let rows: Vec<(String, String, String, String, String)> = reports
        .iter()
        .map(|report| {
            (
                report.dataset.clone(),
                format_window(report.window_secs),
                report.source.clone(),
                report.enforcement.clone(),
                format_rows(report, mode),
            )
        })
        .collect();

    let rows_header = match mode {
        RetentionMode::Report => "Eligible now",
        RetentionMode::DryRun => "Would remove",
        RetentionMode::Purge => "Result",
    };
    let headers = ("Dataset", "Retention", "Source", "Enforced by", rows_header);
    let width =
        |header: &str, extract: fn(&(String, String, String, String, String)) -> &String| {
            rows.iter()
                .map(|row| extract(row).chars().count())
                .max()
                .unwrap_or(0)
                .max(header.chars().count())
        };
    let w0 = width(headers.0, |r| &r.0);
    let w1 = width(headers.1, |r| &r.1);
    let w2 = width(headers.2, |r| &r.2);
    let w3 = width(headers.3, |r| &r.3);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
        headers.0, headers.1, headers.2, headers.3, headers.4
    );
    let _ = writeln!(
        out,
        "{:-<w0$}  {:-<w1$}  {:-<w2$}  {:-<w3$}  {:-<12}",
        "", "", "", "", ""
    );
    for row in &rows {
        let _ = writeln!(
            out,
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
            row.0, row.1, row.2, row.3, row.4
        );
    }

    // A single-dataset report (`--dataset <key>`) has room to answer the
    // follow-up questions the table cannot: what this dataset actually is,
    // and the exact instant the window resolves to.
    if let [only] = reports {
        let _ = writeln!(out, "\n  {}", only.description);
        let _ = writeln!(
            out,
            "  Cutoff: {}",
            only.cutoff.as_deref().unwrap_or("\u{2014} (kept forever)")
        );
        let _ = writeln!(out, "  Took:   {}ms", only.duration_ms);
    }

    // Notes go under the table rather than in a column: a legal-hold reason
    // is free text an operator wrote and must be shown verbatim, not
    // truncated to fit.
    let notes: Vec<&RetentionDatasetReport> = reports
        .iter()
        .filter(|report| report.skipped.is_some())
        .collect();
    if !notes.is_empty() {
        out.push('\n');
        for report in notes {
            if let Some(skipped) = report.skipped.as_deref() {
                let _ = writeln!(out, "  \u{2139} {}: {skipped}", report.dataset);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(dataset: &str) -> RetentionDatasetReport {
        RetentionDatasetReport {
            dataset: dataset.to_owned(),
            description: "a dataset".to_owned(),
            enforcement: "sweep".to_owned(),
            window_secs: Some(7_776_000),
            source: "[retention]".to_owned(),
            cutoff: Some("2026-06-01T00:00:00Z".to_owned()),
            eligible_rows: Some(42),
            rows_removed: 0,
            truncated: false,
            dry_run: true,
            skipped: None,
            duration_ms: 3,
            error: None,
        }
    }

    #[test]
    fn the_engines_report_deserializes_into_this_crates_mirror() {
        // `RetentionDatasetReport` here is a hand-written mirror of
        // `autumn_web::data_retention::RetentionDatasetReport`; the two are
        // joined only by a JSON line on the child's stdout, so a renamed or
        // dropped field in `autumn-web` would break `autumn db retention` at
        // runtime with no compile error anywhere. Round-trip a real engine
        // report through the wire format to catch that at build time.
        let engine = autumn_web::data_retention::RetentionDatasetReport {
            dataset: "job_history".to_owned(),
            description: "Finished job rows".to_owned(),
            enforcement: "sweep".to_owned(),
            window_secs: Some(7_776_000),
            source: "[retention]".to_owned(),
            cutoff: Some("2026-06-01T00:00:00Z".to_owned()),
            eligible_rows: Some(9),
            rows_removed: 9,
            truncated: false,
            dry_run: false,
            skipped: None,
            duration_ms: 4,
            error: None,
        };
        let json = serde_json::to_string(&[&engine]).expect("serialize");

        let decoded: Vec<RetentionDatasetReport> =
            serde_json::from_str(&json).expect("the CLI mirror must decode the engine's report");

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].dataset, "job_history");
        assert_eq!(decoded[0].window_secs, Some(7_776_000));
        assert_eq!(decoded[0].eligible_rows, Some(9));
        assert_eq!(decoded[0].rows_removed, 9);
        assert!(!decoded[0].truncated);
        assert_eq!(decoded[0].cutoff.as_deref(), Some("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn extract_report_json_finds_the_framed_line() {
        let stdout = "2026-08-31T12:00:00Z INFO booting\nRunning migration 0001\n\
                      AUTUMN_DB_RETENTION_REPORT=[{\"dataset\":\"job_history\"}]\n";
        assert_eq!(
            extract_report_json(stdout),
            Some("[{\"dataset\":\"job_history\"}]")
        );
    }

    #[test]
    fn extract_report_json_returns_none_without_a_framed_line() {
        assert_eq!(extract_report_json("just some logs\n"), None);
    }

    #[test]
    fn apply_dataset_env_removes_an_inherited_var_when_no_dataset_given() {
        // Without an explicit removal, an AUTUMN_DB_RETENTION_DATASET already
        // present in the CLI's environment would narrow a run the operator
        // asked to cover everything, via Command's default env inheritance.
        let mut command = Command::new("true");
        command.env(RETENTION_DATASET_ENV, "leftover");

        apply_dataset_env(&mut command, None);

        let value = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new(RETENTION_DATASET_ENV));
        assert_eq!(
            value,
            Some((std::ffi::OsStr::new(RETENTION_DATASET_ENV), None)),
            "the variable must be explicitly removed, not merely absent: {value:?}"
        );
    }

    #[test]
    fn apply_dataset_env_sets_the_requested_dataset() {
        let mut command = Command::new("true");
        apply_dataset_env(&mut command, Some("job_history"));
        let value = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new(RETENTION_DATASET_ENV));
        assert_eq!(
            value,
            Some((
                std::ffi::OsStr::new(RETENTION_DATASET_ENV),
                Some(std::ffi::OsStr::new("job_history"))
            ))
        );
    }

    #[test]
    fn clear_competing_one_shot_env_removes_every_earlier_mode_var() {
        // Each of these is checked before AUTUMN_DB_RETENTION in
        // AppBuilder::run's dispatch chain, so any one left in the
        // environment would hijack this invocation into another mode.
        let competing = [
            "AUTUMN_BUILD_STATIC",
            "AUTUMN_DUMP_ROUTES",
            "AUTUMN_DUMP_CACHE_COHERENCE",
            "AUTUMN_DUMP_DATA_FLOW",
            "AUTUMN_DUMP_JOBS",
            "AUTUMN_LIST_TASKS",
            "AUTUMN_RUN_TASK",
            "AUTUMN_MIGRATE",
            "AUTUMN_RETENTION_DRY_RUN",
        ];
        let mut command = Command::new("true");
        for var in competing {
            command.env(var, "1");
        }

        clear_competing_one_shot_env(&mut command);

        for var in competing {
            let value = command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(var));
            assert_eq!(
                value,
                Some((std::ffi::OsStr::new(var), None)),
                "{var} must be explicitly removed: {value:?}"
            );
        }
    }

    #[test]
    fn report_and_dry_run_both_ask_the_app_only_to_count() {
        assert_eq!(RetentionMode::Report.env_value(), "report");
        assert_eq!(RetentionMode::DryRun.env_value(), "report");
        assert_eq!(RetentionMode::Purge.env_value(), "purge");
    }

    #[test]
    fn format_window_prefers_the_largest_whole_unit() {
        assert_eq!(format_window(Some(7_776_000)), "90d");
        assert_eq!(format_window(Some(3_600)), "1h");
        assert_eq!(format_window(Some(1_800)), "30m");
        assert_eq!(format_window(Some(90)), "90s");
        assert_eq!(format_window(None), "forever");
    }

    #[test]
    fn format_report_lists_every_dataset_including_unconfigured_ones() {
        // "Which framework tables exist and how long do you keep them" is the
        // question; hiding the unbounded ones would hide the answer.
        let mut unconfigured = report("audit_archives");
        unconfigured.window_secs = None;
        unconfigured.source = "unset".to_owned();
        unconfigured.eligible_rows = None;
        unconfigured.skipped = Some("no retention window configured".to_owned());

        let table = format_report(
            &[report("job_history"), unconfigured],
            RetentionMode::Report,
        );

        assert!(table.contains("job_history"), "{table}");
        assert!(table.contains("audit_archives"), "{table}");
        assert!(table.contains("forever"), "{table}");
        assert!(table.contains("90d"), "{table}");
        assert!(table.contains("Eligible now"), "{table}");
    }

    #[test]
    fn format_report_shows_a_legal_hold_reason_verbatim() {
        let mut held = report("job_history");
        held.skipped = Some("legal hold: SOX \u{2014} 7 year retention".to_owned());
        held.eligible_rows = None;

        let table = format_report(&[held], RetentionMode::Report);

        assert!(
            table.contains("legal hold: SOX \u{2014} 7 year retention"),
            "the operator's own hold reason must be shown in full: {table}"
        );
    }

    #[test]
    fn format_report_reports_removals_after_a_purge() {
        let mut purged = report("job_history");
        purged.dry_run = false;
        purged.rows_removed = 17;

        let table = format_report(&[purged], RetentionMode::Purge);

        assert!(table.contains("removed 17"), "{table}");
        assert!(table.contains("Result"), "{table}");
    }

    #[test]
    fn format_report_flags_a_truncated_purge() {
        let mut partial = report("job_history");
        partial.dry_run = false;
        partial.rows_removed = 500_000;
        partial.truncated = true;

        let table = format_report(&[partial], RetentionMode::Purge);

        assert!(
            table.contains("more remain"),
            "a run that hit its batch cap must not read as fully enforced: {table}"
        );
    }

    #[test]
    fn format_report_surfaces_a_per_dataset_error() {
        let mut failed = report("job_history");
        failed.error = Some("connection refused".to_owned());

        let table = format_report(&[failed], RetentionMode::Report);

        assert!(table.contains("error: connection refused"), "{table}");
    }

    #[test]
    fn a_single_dataset_report_explains_what_it_is_and_the_exact_cutoff() {
        let table = format_report(&[report("job_history")], RetentionMode::Report);
        assert!(table.contains("a dataset"), "{table}");
        assert!(table.contains("2026-06-01T00:00:00Z"), "{table}");
        assert!(table.contains("Took:"), "{table}");
    }

    #[test]
    fn a_multi_dataset_report_omits_the_per_dataset_detail_block() {
        let table = format_report(
            &[report("job_history"), report("job_tracking")],
            RetentionMode::Report,
        );
        assert!(
            !table.contains("Took:"),
            "the detail block is only useful for a single-dataset report: {table}"
        );
    }

    #[test]
    fn format_report_handles_an_empty_report() {
        assert!(format_report(&[], RetentionMode::Report).contains("No framework-owned datasets"));
    }

    #[test]
    fn dry_run_header_says_would_remove() {
        let table = format_report(&[report("job_history")], RetentionMode::DryRun);
        assert!(table.contains("Would remove"), "{table}");
    }
}
