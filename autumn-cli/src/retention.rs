//! `autumn retention --dry-run` -- report what every declared
//! `#[repository(..., retention(...))]` policy would sweep, without deleting
//! anything (issue #1342).
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_RETENTION_DRY_RUN=1`, and reads the JSON report from the one line
//! of its stdout starting with [`RETENTION_DRY_RUN_JSON_PREFIX`] — not the
//! entirety of stdout, which can also carry tracing/migration output the
//! child prints before the report line. Running from inside the app is the
//! only sound source: policies are compiled into the app's own
//! model/repository types, so the standalone CLI (which links `autumn-web`
//! but never the user's models) cannot see them — mirrors `autumn jobs
//! manifest` / `autumn task --list`.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// Line prefix framing the app's machine-readable JSON report, matched
/// verbatim against `autumn_web`'s `app::RETENTION_DRY_RUN_JSON_PREFIX`
/// (private to that crate, so duplicated here rather than shared — the same
/// pattern already used for the `AUTUMN_RETENTION_DRY_RUN`/
/// `AUTUMN_RETENTION_MODEL` env var names below).
const RETENTION_DRY_RUN_JSON_PREFIX: &str = "AUTUMN_RETENTION_DRY_RUN_REPORT=";

/// Options controlling `autumn retention`.
pub struct RetentionOptions<'a> {
    /// Package to run (for workspaces).
    pub package: Option<&'a str>,
    /// Binary target to run (for packages with multiple bin targets).
    pub bin: Option<&'a str>,
    /// Profile forwarded to the app binary via `AUTUMN_ENV`.
    pub profile: &'a str,
    /// Report what would be swept without deleting anything. Currently the
    /// only supported mode — see [`run`].
    pub dry_run: bool,
    /// Narrow the report to a single model's policy.
    pub model: Option<&'a str>,
}

/// One model's dry-run report, deserialized from the app's
/// `AUTUMN_RETENTION_DRY_RUN=1` stdout — mirrors
/// `autumn_web::retention::RetentionSweepReport`. Every report this command
/// prints is a dry run by construction, so `dry_run` (present in the app's
/// JSON) is intentionally not modeled here — `Deserialize` ignores unknown
/// fields by default.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionSweepReport {
    pub model: String,
    /// Schema-unique, unlike `model` — two modules can declare same-named
    /// models, each with its own policy, so this is what tells their rows
    /// apart in an unfiltered report.
    pub table: String,
    pub rows_swept: u64,
    pub duration_ms: u64,
}

/// Run `autumn retention`.
pub fn run(opts: &RetentionOptions<'_>) {
    if !opts.dry_run {
        eprintln!("autumn retention: only --dry-run is supported today.");
        eprintln!(
            "Declared policies already run on their own fleet-coordinated schedule inside \
             the app the moment it boots; there is no separate command to trigger a real \
             sweep by hand."
        );
        std::process::exit(1);
    }

    eprintln!("\u{1F342} autumn retention --dry-run\n");
    crate::routes::compile_binary(opts.package, opts.bin);
    let binary = crate::routes::find_binary(opts.package, opts.bin);

    let mut command = Command::new(&binary);
    clear_competing_one_shot_env(&mut command);
    command
        .env("AUTUMN_RETENTION_DRY_RUN", "1")
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    apply_model_env(&mut command, opts.model);
    crate::task::apply_managed_pg_env(&mut command, opts.package);

    let output = command.output().unwrap_or_else(|error| {
        eprintln!("\u{2717} Failed to run {}: {error}", binary.display());
        std::process::exit(1);
    });

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while running the retention dry-run",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reports: Vec<RetentionSweepReport> = extract_dry_run_json(&stdout).map_or_else(
        || {
            eprintln!("Failed to find the retention dry-run report in the binary's output.");
            eprintln!("Raw output: {stdout}");
            std::process::exit(1);
        },
        |json| {
            serde_json::from_str(json).unwrap_or_else(|error| {
                eprintln!("Failed to parse retention dry-run JSON: {error}");
                eprintln!("Raw output: {stdout}");
                std::process::exit(1);
            })
        },
    );

    print!("{}", format_retention_report(&reports));
}

/// Clear the other internal one-shot mode env vars `AppBuilder::run` checks
/// *before* `AUTUMN_RETENTION_DRY_RUN` in its dispatch chain.
///
/// `Command` inherits the parent process's environment by default, so any of
/// these left over in the CLI's own environment (e.g. from a wrapping script,
/// or a previous `autumn migrate`/`autumn task run ...` invocation in the
/// same shell) would silently hijack a `--dry-run` invocation into a
/// completely different — and potentially mutating — mode, since every one of
/// them is checked earlier in `AppBuilder::run`'s dispatch chain than
/// `AUTUMN_RETENTION_DRY_RUN` (#1342 review round 23). `AUTUMN_REPLAY_CAPSULE`
/// is checked *after* retention dry-run and so is deliberately left alone;
/// `AUTUMN_DUMP_SECURITY` only matters inside the `AUTUMN_DUMP_ROUTES` branch,
/// which is already cleared here.
fn clear_competing_one_shot_env(command: &mut Command) {
    for var in [
        "AUTUMN_BUILD_STATIC",
        "AUTUMN_DUMP_ROUTES",
        "AUTUMN_DUMP_CACHE_COHERENCE",
        "AUTUMN_DUMP_DATA_FLOW",
        "AUTUMN_DUMP_AGENT_AUTHORITY",
        "AUTUMN_DUMP_JOBS",
        "AUTUMN_LIST_TASKS",
        "AUTUMN_RUN_TASK",
        "AUTUMN_MIGRATE",
    ] {
        command.env_remove(var);
    }
}

/// Set or clear `AUTUMN_RETENTION_MODEL` on `command` from `model`.
///
/// `Command` inherits the parent process's environment by default, so
/// without an explicit removal when `model` is `None`, a
/// `AUTUMN_RETENTION_MODEL` the CLI's own environment already happens to
/// carry (e.g. exported by a wrapping script, or left over from a previous
/// invocation) would silently leak into a filter-less `--dry-run` —
/// reporting just that one model, or failing as not-found, despite
/// `--model` never being passed on this invocation (#1342 review round 20).
///
/// A free function (rather than inlined into [`run`]) so this is
/// unit-testable via [`Command::get_envs`] without actually spawning a
/// process.
fn apply_model_env(command: &mut Command, model: Option<&str>) {
    if let Some(model) = model {
        command.env("AUTUMN_RETENTION_MODEL", model);
    } else {
        command.env_remove("AUTUMN_RETENTION_MODEL");
    }
}

/// Find the dry-run report line in the child's captured stdout and return
/// its JSON payload (the text after [`RETENTION_DRY_RUN_JSON_PREFIX`]).
///
/// Not assumed to be the entirety of stdout (#1342 review round 14): the
/// default `dev` profile initializes a stdout-backed tracing formatter, and
/// Diesel writes pending-migration progress directly to stdout, both of
/// which can print before the report line whenever anything is pending.
/// Takes the *last* matching line — the report is always printed
/// immediately before the child exits.
fn extract_dry_run_json(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(RETENTION_DRY_RUN_JSON_PREFIX))
}

/// Render a dry-run report as a fixed-width table, sorted by model name then
/// table name (the app already sorts its JSON, but formatting is defensive
/// against a future caller feeding in unsorted data). The table-name column
/// is what keeps two same-named models in different modules distinguishable
/// in an unfiltered report — see `--model`'s help for disambiguating a
/// filtered one.
pub fn format_retention_report(reports: &[RetentionSweepReport]) -> String {
    if reports.is_empty() {
        return "No retention(...) policies are registered.\n".to_string();
    }

    let mut sorted: Vec<&RetentionSweepReport> = reports.iter().collect();
    sorted.sort_by(|a, b| a.model.cmp(&b.model).then_with(|| a.table.cmp(&b.table)));

    let model_width = sorted
        .iter()
        .map(|r| r.model.len())
        .max()
        .unwrap_or("Model".len())
        .max("Model".len());
    let table_width = sorted
        .iter()
        .map(|r| r.table.len())
        .max()
        .unwrap_or("Table".len())
        .max("Table".len());
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<model_width$}  {:<table_width$}  Rows that would be swept  Duration (ms)",
        "Model", "Table"
    );
    let _ = writeln!(
        out,
        "{:-<model_width$}  {:-<table_width$}  ------------------------  -------------",
        "", ""
    );
    for report in &sorted {
        let _ = writeln!(
            out,
            "{:<model_width$}  {:<table_width$}  {:<24}  {}",
            report.model, report.table, report.rows_swept, report.duration_ms
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_dry_run_json_finds_the_framed_report_line() {
        let stdout = "2026-08-17T12:00:00Z INFO booting\nRunning migration 0001\n\
                       AUTUMN_RETENTION_DRY_RUN_REPORT=[{\"model\":\"Widget\"}]\n";
        assert_eq!(
            extract_dry_run_json(stdout),
            Some("[{\"model\":\"Widget\"}]")
        );
    }

    #[test]
    fn extract_dry_run_json_ignores_incidental_stdout_noise() {
        // Regression (#1342 review round 14): the default `dev` profile's
        // stdout-backed tracing formatter and Diesel's migration-progress
        // output can both print before the report line whenever anything
        // is pending — the extractor must not choke on (or accidentally
        // match) any of that.
        let stdout = "Running migration 20260101000000_create_widgets\n\
                       {\"level\":\"INFO\",\"message\":\"listening\"}\n\
                       AUTUMN_RETENTION_DRY_RUN_REPORT=[]\n";
        assert_eq!(extract_dry_run_json(stdout), Some("[]"));
    }

    #[test]
    fn extract_dry_run_json_returns_none_without_a_framed_line() {
        let stdout = "some unrelated log output\nno report line here\n";
        assert_eq!(extract_dry_run_json(stdout), None);
    }

    #[test]
    fn clear_competing_one_shot_env_removes_every_var_checked_before_retention_dry_run() {
        // Regression (#1342 review round 23): AppBuilder::run checks
        // AUTUMN_BUILD_STATIC, AUTUMN_DUMP_ROUTES,
        // AUTUMN_DUMP_CACHE_COHERENCE, AUTUMN_DUMP_DATA_FLOW,
        // AUTUMN_DUMP_AGENT_AUTHORITY, AUTUMN_DUMP_JOBS, AUTUMN_LIST_TASKS,
        // AUTUMN_RUN_TASK, and AUTUMN_MIGRATE *before*
        // AUTUMN_RETENTION_DRY_RUN in its dispatch chain — any of them left
        // over in the CLI's own environment would hijack a --dry-run
        // invocation into a completely different (potentially mutating)
        // mode via Command's default environment inheritance.
        let competing_vars = [
            "AUTUMN_BUILD_STATIC",
            "AUTUMN_DUMP_ROUTES",
            "AUTUMN_DUMP_CACHE_COHERENCE",
            "AUTUMN_DUMP_DATA_FLOW",
            "AUTUMN_DUMP_AGENT_AUTHORITY",
            "AUTUMN_DUMP_JOBS",
            "AUTUMN_LIST_TASKS",
            "AUTUMN_RUN_TASK",
            "AUTUMN_MIGRATE",
        ];
        let mut command = std::process::Command::new("true");
        for var in competing_vars {
            command.env(var, "1");
        }

        clear_competing_one_shot_env(&mut command);

        for var in competing_vars {
            let value = command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(var));
            assert_eq!(
                value,
                Some((std::ffi::OsStr::new(var), None)),
                "{var} must be explicitly removed (None value), not merely absent from \
                 overrides, or it would still be inherited from the parent environment: \
                 {value:?}"
            );
        }
    }

    #[test]
    fn apply_model_env_removes_an_inherited_var_when_no_model_given() {
        // Regression (#1342 review round 20): without an explicit removal,
        // an AUTUMN_RETENTION_MODEL already present in the CLI's own
        // environment (e.g. exported by a wrapping script) would otherwise
        // leak into a filter-less --dry-run via Command's default
        // environment inheritance.
        let mut command = std::process::Command::new("true");
        // Simulate the var already being set, as it would be if inherited
        // from the parent process's environment.
        command.env("AUTUMN_RETENTION_MODEL", "LeftoverFromParentEnv");

        apply_model_env(&mut command, None);

        let value = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("AUTUMN_RETENTION_MODEL"));
        assert_eq!(
            value,
            Some((std::ffi::OsStr::new("AUTUMN_RETENTION_MODEL"), None)),
            "the variable must be explicitly removed (None value), not merely absent from \
             overrides, or it would still be inherited from the parent environment: {value:?}"
        );
    }

    #[test]
    fn apply_model_env_sets_the_requested_model() {
        let mut command = std::process::Command::new("true");

        apply_model_env(&mut command, Some("Widget"));

        let value = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("AUTUMN_RETENTION_MODEL"));
        assert_eq!(
            value,
            Some((
                std::ffi::OsStr::new("AUTUMN_RETENTION_MODEL"),
                Some(std::ffi::OsStr::new("Widget"))
            ))
        );
    }

    #[test]
    fn format_retention_report_includes_model_and_row_count() {
        let table = format_retention_report(&[RetentionSweepReport {
            model: "Widget".to_string(),
            table: "widgets".to_string(),
            rows_swept: 42,
            duration_ms: 7,
        }]);

        assert!(table.contains("Widget"));
        assert!(table.contains("widgets"));
        assert!(table.contains("42"));
    }

    #[test]
    fn format_retention_report_sorts_by_model_name() {
        let table = format_retention_report(&[
            RetentionSweepReport {
                model: "Zeta".to_string(),
                table: "zetas".to_string(),
                rows_swept: 1,
                duration_ms: 1,
            },
            RetentionSweepReport {
                model: "Alpha".to_string(),
                table: "alphas".to_string(),
                rows_swept: 2,
                duration_ms: 2,
            },
        ]);

        let alpha_pos = table.find("Alpha").expect("Alpha present");
        let zeta_pos = table.find("Zeta").expect("Zeta present");
        assert!(alpha_pos < zeta_pos, "Alpha must sort before Zeta: {table}");
    }

    #[test]
    fn format_retention_report_disambiguates_same_model_name_by_table() {
        // Regression (#1342 review round 5): two policies can share a model
        // name (same-named model in different modules); the table column is
        // what keeps their rows distinguishable in an unfiltered report.
        let table = format_retention_report(&[
            RetentionSweepReport {
                model: "Session".to_string(),
                table: "admin_sessions".to_string(),
                rows_swept: 1,
                duration_ms: 1,
            },
            RetentionSweepReport {
                model: "Session".to_string(),
                table: "auth_sessions".to_string(),
                rows_swept: 2,
                duration_ms: 2,
            },
        ]);

        assert!(table.contains("admin_sessions"));
        assert!(table.contains("auth_sessions"));
    }

    #[test]
    fn format_retention_report_reports_no_policies_when_empty() {
        let table = format_retention_report(&[]);
        assert!(table.contains("No retention"));
    }
}
