//! `autumn retention --dry-run` -- report what every declared
//! `#[repository(..., retention(...))]` policy would sweep, without deleting
//! anything (issue #1342).
//!
//! Compiles the target binary (debug profile), runs it with
//! `AUTUMN_RETENTION_DRY_RUN=1`, and parses the JSON report from its stdout.
//! Running from inside the app is the only sound source: policies are
//! compiled into the app's own model/repository types, so the standalone CLI
//! (which links `autumn-web` but never the user's models) cannot see them —
//! mirrors `autumn jobs manifest` / `autumn task --list`.

use std::fmt::Write as _;
use std::process::{Command, Stdio};

use serde::Deserialize;

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
    command
        .env("AUTUMN_RETENTION_DRY_RUN", "1")
        .env("AUTUMN_ENV", opts.profile)
        .env("AUTUMN_PROFILE", opts.profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(model) = opts.model {
        command.env("AUTUMN_RETENTION_MODEL", model);
    }
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
    let reports: Vec<RetentionSweepReport> =
        serde_json::from_str(&stdout).unwrap_or_else(|error| {
            eprintln!("Failed to parse retention dry-run JSON: {error}");
            eprintln!("Raw output: {stdout}");
            std::process::exit(1);
        });

    print!("{}", format_retention_report(&reports));
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
